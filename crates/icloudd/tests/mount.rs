//! The whole stack against a real kernel FUSE mount, driven with ordinary
//! `std::fs` calls, with an in-memory iCloud behind it.
//!
//! These tests need `/dev/fuse` and `fusermount3` and are skipped (with a
//! message) where they are not available, such as a CI runner without FUSE.
//! Run them in the provided container with `tools/ci/run.sh cargo test --workspace`.

use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fuser::{BackgroundSession, Config as MountConfig, MountOption, Session};
use icloud_api::memory::MemoryDrive;
use icloud_core::{Engine, EngineConfig, FsCore, FsOptions, Mirror, SyncPolicy, SyncState};
use icloudd::IcloudFs;

struct Mounted {
    _session: BackgroundSession,
    _dir: tempfile::TempDir,
    mount: PathBuf,
    drive: Arc<MemoryDrive>,
    engine: Engine,
    state: Arc<SyncState>,
}

impl Mounted {
    fn path(&self, relative: &str) -> PathBuf {
        self.mount.join(relative)
    }
}

fn fuse_available() -> bool {
    let usable = Path::new("/dev/fuse").exists()
        && OpenOptions::new().read(true).write(true).open("/dev/fuse").is_ok()
        && ["/usr/bin/fusermount3", "/bin/fusermount3", "/usr/bin/fusermount"].iter().any(|p| Path::new(p).exists());
    if !usable {
        eprintln!("skipping: FUSE is not available here");
    }
    usable
}

fn mount(policy: SyncPolicy, read_only: bool) -> Option<Mounted> {
    if !fuse_available() {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let mount = dir.path().join("mnt");
    fs::create_dir_all(&mount).unwrap();

    let drive = Arc::new(MemoryDrive::new());
    let mirror = Arc::new(Mirror::open(&cache).unwrap());
    let state = Arc::new(SyncState::open(&cache.join("state.sqlite3")).unwrap());
    let engine = Engine::new(
        drive.clone(),
        mirror.clone(),
        state.clone(),
        policy.clone(),
        EngineConfig { auto_sync: false, ..EngineConfig::default() },
    );
    let core = Arc::new(FsCore::new(mirror, state.clone(), policy, Some(engine.clone()), FsOptions { read_only }));

    let mut config = MountConfig::default();
    config.mount_options = vec![MountOption::FSName("icloud-test".into())];
    config.n_threads = Some(8);
    config.clone_fd = true;
    let session = Session::new(IcloudFs::new(core), &mount, &config).expect("mount").spawn().expect("spawn");
    Some(Mounted { _session: session, _dir: dir, mount, drive, engine, state })
}

fn mounted() -> Option<Mounted> {
    mount(SyncPolicy::unrestricted(), false)
}

fn names(path: &Path) -> Vec<String> {
    let mut out: Vec<String> =
        fs::read_dir(path).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    out.sort();
    out
}

#[test]
fn browsing_shows_the_remote_tree_with_real_sizes_without_downloading() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "hello.txt", b"hello world", 1_700_000_000);
    m.drive.add_folder("/", "Docs");
    m.drive.add_file("/Docs", "a.md", b"# a", 1_700_000_100);

    assert_eq!(names(&m.mount), ["Docs", "hello.txt"]);
    let meta = fs::metadata(m.path("hello.txt")).unwrap();
    assert_eq!(meta.len(), 11);
    assert!(meta.is_file());
    assert!(fs::metadata(m.path("Docs")).unwrap().is_dir());
    assert_eq!(names(&m.path("Docs")), ["a.md"]);
    assert_eq!(m.drive.calls_matching("open:"), 0, "browsing must not download anything");
    let modified = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert_eq!(modified, 1_700_000_000);
}

#[test]
fn reading_a_file_downloads_it_once() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "hello.txt", b"hello world", 1);
    assert_eq!(fs::read_to_string(m.path("hello.txt")).unwrap(), "hello world");
    assert_eq!(fs::read_to_string(m.path("hello.txt")).unwrap(), "hello world");
    assert_eq!(m.drive.calls_matching("open:"), 1);
}

#[test]
fn reading_in_the_middle_of_a_file_works() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "digits", b"0123456789", 1);
    let mut file = File::open(m.path("digits")).unwrap();
    file.seek(SeekFrom::Start(4)).unwrap();
    let mut buf = [0u8; 3];
    file.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"456");
}

#[test]
fn a_new_file_written_through_the_mount_reaches_icloud() {
    let Some(m) = mounted() else { return };
    m.drive.add_folder("/", "Docs");
    assert_eq!(names(&m.mount), ["Docs"]);
    fs::write(m.path("Docs/new.txt"), b"from linux").unwrap();

    m.engine.sync_dirty().unwrap();

    assert_eq!(m.drive.contents("/Docs/new.txt").unwrap(), b"from linux");
    assert!(m.state.list_dirty_entries().unwrap().is_empty());
}

#[test]
fn appending_and_overwriting_keep_the_untouched_bytes() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "log.txt", b"line one\n", 1);
    assert_eq!(names(&m.mount), ["log.txt"]);
    let mut file = OpenOptions::new().append(true).open(m.path("log.txt")).unwrap();
    file.write_all(b"line two\n").unwrap();
    drop(file);
    let mut file = OpenOptions::new().write(true).open(m.path("log.txt")).unwrap();
    file.write_all(b"LINE").unwrap();
    drop(file);

    assert_eq!(fs::read_to_string(m.path("log.txt")).unwrap(), "LINE one\nline two\n");
    m.engine.sync_dirty().unwrap();
    assert_eq!(m.drive.contents("/log.txt").unwrap(), b"LINE one\nline two\n");
}

#[test]
fn a_large_file_written_in_many_pieces_is_intact_and_uploads_once() {
    let Some(m) = mounted() else { return };
    let chunk: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
    let mut file = File::create(m.path("big.bin")).unwrap();
    for _ in 0..64 {
        file.write_all(&chunk).unwrap(); // 4 MiB in 64 KiB pieces
    }
    drop(file);

    assert_eq!(fs::metadata(m.path("big.bin")).unwrap().len(), 4 * 1024 * 1024);
    assert_eq!(m.state.pending_op_count().unwrap(), 1, "one queued operation, not one per write");
    m.engine.sync_dirty().unwrap();
    let uploaded = m.drive.contents("/big.bin").unwrap();
    assert_eq!(uploaded.len(), 4 * 1024 * 1024);
    assert!(uploaded.chunks(chunk.len()).all(|c| c == chunk.as_slice()));
    assert_eq!(m.drive.calls_matching("upload:"), 1);
}

#[test]
fn truncate_and_set_times_go_through() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "f.txt", b"abcdef", 1);
    assert_eq!(names(&m.mount), ["f.txt"]);
    let file = OpenOptions::new().write(true).open(m.path("f.txt")).unwrap();
    file.set_len(3).unwrap();
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    file.set_modified(when).unwrap();
    drop(file);

    assert_eq!(fs::read(m.path("f.txt")).unwrap(), b"abc");
    let modified = fs::metadata(m.path("f.txt")).unwrap().modified().unwrap();
    assert_eq!(modified, when);
}

#[test]
fn mkdir_rename_and_remove_follow_posix_rules_and_reach_icloud() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "keep.txt", b"keep", 1);
    m.drive.add_folder("/", "Old");
    m.drive.add_file("/Old", "inside.txt", b"inside", 1);
    assert_eq!(names(&m.mount), ["Old", "keep.txt"]);
    assert_eq!(names(&m.path("Old")), ["inside.txt"]);

    fs::create_dir(m.path("New")).unwrap();
    fs::rename(m.path("keep.txt"), m.path("New/kept.txt")).unwrap();
    fs::rename(m.path("Old"), m.path("Renamed")).unwrap();

    // Not empty: refused, and nothing is lost.
    let err = fs::remove_dir(m.path("Renamed")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::DirectoryNotEmpty);
    // A file cannot replace a directory.
    fs::write(m.path("plain.txt"), b"x").unwrap();
    assert!(fs::rename(m.path("plain.txt"), m.path("New")).is_err());
    assert!(m.path("New/kept.txt").exists(), "the directory the rename tried to replace is untouched");

    fs::remove_file(m.path("plain.txt")).unwrap();
    m.engine.sync_dirty().unwrap();

    assert_eq!(m.drive.contents("/New/kept.txt").unwrap(), b"keep");
    assert_eq!(m.drive.contents("/Renamed/inside.txt").unwrap(), b"inside");
    assert!(!m.drive.exists("/keep.txt") && !m.drive.exists("/Old"));
    assert!(!m.drive.exists("/plain.txt"));
}

#[test]
fn deleting_moves_things_to_the_trash_on_icloud() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "doomed.txt", b"x", 1);
    m.drive.add_folder("/", "EmptyDir");
    assert_eq!(names(&m.mount), ["EmptyDir", "doomed.txt"]);
    fs::remove_file(m.path("doomed.txt")).unwrap();
    fs::remove_dir(m.path("EmptyDir")).unwrap();
    assert!(!m.path("doomed.txt").exists());

    m.engine.sync_dirty().unwrap();
    let mut trashed = m.drive.trashed();
    trashed.sort();
    assert_eq!(trashed, ["EmptyDir", "doomed.txt"]);
}

#[test]
fn removing_a_never_opened_folder_does_not_delete_what_is_inside_it() {
    let Some(m) = mounted() else { return };
    m.drive.add_folder("/", "Unopened");
    m.drive.add_file("/Unopened", "precious.txt", b"data", 1);
    assert_eq!(names(&m.mount), ["Unopened"]);

    let err = fs::remove_dir(m.path("Unopened")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::DirectoryNotEmpty);
    m.engine.sync_dirty().unwrap();
    assert!(m.drive.exists("/Unopened/precious.txt"));
}

#[test]
fn a_directory_with_thousands_of_entries_lists_completely() {
    let Some(m) = mounted() else { return };
    m.drive.add_folder("/", "Big");
    for i in 0..3000 {
        m.drive.add_file("/Big", &format!("file-{i:05}.txt"), b"x", 1);
    }
    let listed = names(&m.path("Big"));
    assert_eq!(listed.len(), 3000, "paging through readdir must neither drop nor repeat entries");
    assert_eq!(listed.first().map(String::as_str), Some("file-00000.txt"));
    assert_eq!(listed.last().map(String::as_str), Some("file-02999.txt"));
    assert_eq!(m.drive.calls_matching("children:/Big"), 1, "listed from iCloud once");
}

#[test]
fn parallel_readers_of_different_files_do_not_block_each_other_forever() {
    let Some(m) = mounted() else { return };
    for i in 0..12 {
        m.drive.add_file("/", &format!("f{i}.txt"), format!("content {i}").as_bytes(), 1);
    }
    assert_eq!(names(&m.mount).len(), 12);
    let root = m.mount.clone();
    let handles: Vec<_> = (0..12)
        .map(|i| {
            let root = root.clone();
            std::thread::spawn(move || fs::read_to_string(root.join(format!("f{i}.txt"))).unwrap())
        })
        .collect();
    for (i, handle) in handles.into_iter().enumerate() {
        assert_eq!(handle.join().unwrap(), format!("content {i}"));
    }
}

#[test]
fn the_indexer_marker_is_not_visible_and_cannot_be_created() {
    let Some(m) = mounted() else { return };
    assert!(!names(&m.mount).contains(&".trackerignore".to_owned()));
    assert!(fs::metadata(m.path(".trackerignore")).is_err());
    assert!(File::create(m.path(".trackerignore")).is_err());
}

#[test]
fn missing_paths_are_enoent_and_names_are_validated() {
    let Some(m) = mounted() else { return };
    assert_eq!(fs::metadata(m.path("nope")).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(fs::read(m.path("nope/deeper")).unwrap_err().kind(), ErrorKind::NotFound);
    let long = "x".repeat(300);
    assert!(File::create(m.path(&long)).is_err());
}

#[test]
fn changes_outside_the_sync_boundary_are_refused_by_the_kernel_interface() {
    let Some(m) = mount(SyncPolicy::new(&["/Allowed"], &[] as &[&str]), false) else { return };
    m.drive.add_folder("/", "Allowed");
    m.drive.add_file("/", "protected.txt", b"secret", 1);
    assert_eq!(names(&m.mount), ["Allowed", "protected.txt"]);

    assert_eq!(fs::metadata(m.path("protected.txt")).unwrap().len(), 6, "still listed");
    assert_eq!(fs::read(m.path("protected.txt")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(File::create(m.path("outside.txt")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(fs::remove_file(m.path("protected.txt")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    fs::write(m.path("Allowed/ok.txt"), b"fine").unwrap();
    m.engine.sync_dirty().unwrap();
    assert_eq!(m.drive.contents("/Allowed/ok.txt").unwrap(), b"fine");
    assert!(m.drive.exists("/protected.txt"));
}

#[test]
fn a_read_only_mount_refuses_changes_but_reads_fine() {
    let Some(m) = mount(SyncPolicy::unrestricted(), true) else { return };
    m.drive.add_file("/", "f.txt", b"readable", 1);
    assert_eq!(names(&m.mount), ["f.txt"]);
    assert_eq!(fs::read_to_string(m.path("f.txt")).unwrap(), "readable");
    assert!(File::create(m.path("new")).is_err());
    assert!(fs::remove_file(m.path("f.txt")).is_err());
    assert!(fs::create_dir(m.path("d")).is_err());
}

#[test]
fn unusual_names_survive_the_round_trip() {
    let Some(m) = mounted() else { return };
    let odd = ["spaces in name.txt", "ünïcödé-日本語.txt", "100%_done [final] (1).txt", "it's a \"quoted\" name.txt"];
    for name in odd {
        fs::write(m.path(name), name.as_bytes()).unwrap();
    }
    m.engine.sync_dirty().unwrap();
    for name in odd {
        assert_eq!(m.drive.contents(&format!("/{name}")).unwrap(), name.as_bytes(), "{name}");
        assert_eq!(fs::read_to_string(m.path(name)).unwrap(), name);
    }
    let mut expected = odd.map(str::to_owned).to_vec();
    expected.sort();
    assert_eq!(names(&m.mount), expected);
}

#[test]
fn a_remote_change_shows_up_after_a_refresh() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "shared.txt", b"version 1", 100);
    assert_eq!(fs::read_to_string(m.path("shared.txt")).unwrap(), "version 1");

    m.drive.modify("/shared.txt", b"version two!", 200);
    m.drive.add_file("/", "new-from-phone.txt", b"hi", 300);
    m.engine.refresh_blocking().unwrap();

    // The kernel may cache attributes for a second.
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(fs::read_to_string(m.path("shared.txt")).unwrap(), "version two!");
    assert!(names(&m.mount).contains(&"new-from-phone.txt".to_owned()));
}

#[test]
fn extended_attributes_are_simply_absent() {
    let Some(m) = mounted() else { return };
    m.drive.add_file("/", "f.txt", b"x", 1);
    assert_eq!(names(&m.mount), ["f.txt"]);
    // `getfattr`-style access through the standard library is not available, so
    // check what tools rely on: stat and read keep working, and copying with
    // xattr preservation (`cp -a`) does not fail on a file without any.
    let copy = m.mount.parent().unwrap().join("copy.txt");
    fs::copy(m.path("f.txt"), &copy).unwrap();
    assert_eq!(fs::read(copy).unwrap(), b"x");
}
