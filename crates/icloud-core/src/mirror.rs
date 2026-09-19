//! The local mirror: a plain directory tree holding the files iCloud has.
//!
//! Everything the mount serves comes from here. Paths reach the disk only
//! through [`IcPath`], so no operation can leave the mirror root. Downloads
//! are written to a temporary file and renamed into place, so a reader never
//! sees a half-written file.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use ring::digest::{Context, SHA256};

use crate::path::IcPath;

/// Marker that GNOME's file indexer honours (`ignored-directories-with-content`).
///
/// The indexer walks `$HOME` recursively and opens every file to extract its
/// contents. On a FUSE mount that is indistinguishable from a user opening
/// every file, so left alone it downloads the whole drive within seconds of
/// mounting and defeats on-demand listing. The marker is written straight into
/// the mirror, not through the mount, so it is never taken for a user file and
/// never uploaded.
pub const TRACKER_IGNORE: &str = ".trackerignore";

const COPY_CHUNK: usize = 1 << 20;

/// Free-space figures, as `statfs` reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub block_size: u32,
    pub fragment_size: u32,
    pub blocks: u64,
    pub blocks_free: u64,
    pub blocks_available: u64,
    pub files: u64,
    pub files_free: u64,
    pub name_max: u32,
}

/// A fully downloaded file waiting to be moved into the mirror.
#[derive(Debug)]
pub struct Staged {
    tmp: tempfile::NamedTempFile,
    bytes: u64,
    sha256: String,
}

impl Staged {
    pub fn len(&self) -> u64 {
        self.bytes
    }

    /// SHA-256 of the staged bytes, hex encoded, computed while copying.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }
}

#[derive(Debug, Clone)]
pub struct Mirror {
    root: PathBuf,
    tmp: PathBuf,
}

impl Mirror {
    /// Open (creating if needed) the mirror under `cache_dir`.
    pub fn open(cache_dir: &Path) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;

        let mirror = Self { root: cache_dir.join("mirror"), tmp: cache_dir.join("tmp") };
        // The cache root is private to the user; that is what protects the
        // downloaded files, whatever their own modes are.
        fs::DirBuilder::new().recursive(true).mode(0o700).create(cache_dir)?;
        fs::create_dir_all(&mirror.root)?;
        fs::create_dir_all(&mirror.tmp)?;
        mirror.ensure_tracker_ignore();
        Ok(mirror)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ensure_tracker_ignore(&self) {
        let marker = self.root.join(TRACKER_IGNORE);
        // Best effort: a missing marker only costs extra indexing, never correctness.
        if !marker.exists()
            && let Err(err) = File::create(&marker)
        {
            tracing::warn!("could not create {}: {err}", marker.display());
        }
    }

    /// Is this path one of the mirror's own bookkeeping files? Such paths must
    /// never be visible through the mount.
    pub fn is_reserved(path: &IcPath) -> bool {
        path.parent().is_root() && path.file_name() == Some(TRACKER_IGNORE)
    }

    pub fn local_path(&self, path: &IcPath) -> PathBuf {
        match path.relative_to(&IcPath::root()) {
            Some("") | None => self.root.clone(),
            Some(relative) => self.root.join(relative),
        }
    }

    // ---- queries -----------------------------------------------------------

    pub fn exists(&self, path: &IcPath) -> bool {
        fs::symlink_metadata(self.local_path(path)).is_ok()
    }

    pub fn is_dir(&self, path: &IcPath) -> bool {
        fs::symlink_metadata(self.local_path(path)).is_ok_and(|m| m.is_dir())
    }

    pub fn stat(&self, path: &IcPath) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(self.local_path(path))
    }

    /// Names in a directory with whether each is a directory. Names that are
    /// not valid UTF-8 cannot be iCloud names and are skipped; the reserved
    /// marker file is hidden.
    pub fn list_dir(&self, path: &IcPath) -> io::Result<Vec<(String, bool)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.local_path(path))? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else { continue };
            if path.is_root() && name == TRACKER_IGNORE {
                continue;
            }
            out.push((name, entry.file_type()?.is_dir()));
        }
        Ok(out)
    }

    pub fn capacity(&self) -> io::Result<Capacity> {
        let s = rustix::fs::statvfs(&self.root)?;
        Ok(Capacity {
            block_size: u32::try_from(s.f_bsize).unwrap_or(4096),
            fragment_size: u32::try_from(s.f_frsize).unwrap_or(4096),
            blocks: s.f_blocks,
            blocks_free: s.f_bfree,
            blocks_available: s.f_bavail,
            files: s.f_files,
            files_free: s.f_ffree,
            name_max: u32::try_from(s.f_namemax).unwrap_or(255),
        })
    }

    // ---- creating ----------------------------------------------------------

    pub fn ensure_dir(&self, path: &IcPath) -> io::Result<()> {
        let local = self.local_path(path);
        if fs::symlink_metadata(&local).is_ok_and(|m| !m.is_dir()) {
            fs::remove_file(&local)?;
        }
        fs::create_dir_all(local)
    }

    fn ensure_parent(&self, path: &IcPath) -> io::Result<()> {
        fs::create_dir_all(self.local_path(&path.parent()))
    }

    /// Create an empty file if none exists; leave an existing one alone.
    pub fn create_file(&self, path: &IcPath) -> io::Result<()> {
        self.ensure_parent(path)?;
        OpenOptions::new().create(true).append(true).open(self.local_path(path)).map(drop)
    }

    /// A sparse file of the right size and time standing in for content that
    /// has not been downloaded. It takes no disk space.
    pub fn materialize_placeholder(&self, path: &IcPath, size: u64, mtime: i64) -> io::Result<()> {
        self.ensure_parent(path)?;
        let local = self.local_path(path);
        if fs::symlink_metadata(&local).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(&local)?;
        }
        let file = File::create(&local)?;
        file.set_len(size)?;
        set_mtime_of(&file, mtime)
    }

    /// Write everything `source` yields to `path`, atomically. Returns the
    /// number of bytes written.
    pub fn write_atomic_stream(&self, path: &IcPath, source: &mut dyn Read, mtime: Option<i64>) -> io::Result<u64> {
        let staged = self.stage_stream(source, mtime)?;
        let written = staged.len();
        self.commit(staged, path)?;
        Ok(written)
    }

    /// First half of [`write_atomic_stream`](Self::write_atomic_stream): read
    /// the whole stream into a temporary file. Lets a caller re-check that the
    /// destination is still wanted before [`commit`](Self::commit)ting, which
    /// matters when the read takes minutes.
    pub fn stage_stream(&self, source: &mut dyn Read, mtime: Option<i64>) -> io::Result<Staged> {
        let mut tmp = tempfile::NamedTempFile::new_in(&self.tmp)?;
        let (bytes, sha256) = copy_chunked(source, tmp.as_file_mut())?;
        if let Some(mtime) = mtime {
            set_mtime_of(tmp.as_file(), mtime)?;
        }
        Ok(Staged { tmp, bytes, sha256 })
    }

    /// Move a staged file into place. Dropping it instead discards it.
    pub fn commit(&self, staged: Staged, path: &IcPath) -> io::Result<()> {
        self.ensure_parent(path)?;
        staged.tmp.persist(self.local_path(path)).map(drop).map_err(|e| e.error)
    }

    // ---- reading and writing contents -------------------------------------

    pub fn read_at(&self, path: &IcPath, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let file = File::open(self.local_path(path))?;
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let n = file.read_at(&mut buf[filled..], offset + filled as u64)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        Ok(buf)
    }

    pub fn write_at(&self, path: &IcPath, offset: u64, data: &[u8]) -> io::Result<usize> {
        self.ensure_parent(path)?;
        let file = OpenOptions::new().write(true).create(true).truncate(false).open(self.local_path(path))?;
        file.write_all_at(data, offset)?;
        Ok(data.len())
    }

    pub fn truncate(&self, path: &IcPath, len: u64) -> io::Result<()> {
        self.ensure_parent(path)?;
        OpenOptions::new().write(true).create(true).truncate(false).open(self.local_path(path))?.set_len(len)
    }

    pub fn set_mtime(&self, path: &IcPath, mtime: i64) -> io::Result<()> {
        set_mtime_of(&File::open(self.local_path(path))?, mtime)
    }

    pub fn sha256(&self, path: &IcPath) -> io::Result<String> {
        let mut file = File::open(self.local_path(path))?;
        let mut ctx = Context::new(&SHA256);
        let mut buf = vec![0u8; COPY_CHUNK];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            ctx.update(&buf[..n]);
        }
        Ok(hex(ctx.finish().as_ref()))
    }

    // ---- removing and moving -----------------------------------------------

    pub fn remove_file(&self, path: &IcPath) -> io::Result<()> {
        fs::remove_file(self.local_path(path))
    }

    /// Remove an empty directory.
    pub fn remove_dir(&self, path: &IcPath) -> io::Result<()> {
        fs::remove_dir(self.local_path(path))
    }

    /// Remove a file or a whole directory tree; missing is fine.
    pub fn remove_tree(&self, path: &IcPath) -> io::Result<()> {
        let local = self.local_path(path);
        match fs::symlink_metadata(&local) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(local),
            Ok(_) => fs::remove_file(local),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Rename, replacing the destination if it is a file (or an empty
    /// directory being replaced by a directory), like `rename(2)`.
    pub fn rename(&self, from: &IcPath, to: &IcPath) -> io::Result<()> {
        self.ensure_parent(to)?;
        fs::rename(self.local_path(from), self.local_path(to))
    }
}

/// Copy `source` to `dest`, returning the byte count and the SHA-256.
fn copy_chunked(source: &mut dyn Read, dest: &mut File) -> io::Result<(u64, String)> {
    use std::io::Write as _;

    let mut buf = vec![0u8; COPY_CHUNK];
    let mut total = 0u64;
    let mut digest = Context::new(&SHA256);
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dest.write_all(&buf[..n])?;
        digest.update(&buf[..n]);
        total += n as u64;
    }
    dest.flush()?;
    Ok((total, hex(digest.finish().as_ref())))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

fn set_mtime_of(file: &File, mtime: i64) -> io::Result<()> {
    let seconds = u64::try_from(mtime).unwrap_or(0);
    file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn mirror() -> (tempfile::TempDir, Mirror) {
        let dir = tempfile::tempdir().unwrap();
        let mirror = Mirror::open(dir.path()).unwrap();
        (dir, mirror)
    }

    fn p(s: &str) -> IcPath {
        IcPath::new(s)
    }

    #[test]
    fn opening_creates_the_layout_and_the_indexer_marker() {
        let (dir, mirror) = mirror();
        assert!(dir.path().join("mirror").is_dir());
        assert!(dir.path().join("tmp").is_dir());
        assert!(mirror.root().join(TRACKER_IGNORE).is_file());
    }

    #[test]
    fn the_marker_is_hidden_and_reserved() {
        let (_dir, mirror) = mirror();
        mirror.create_file(&p("/real.txt")).unwrap();
        let names: Vec<_> = mirror.list_dir(&IcPath::root()).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["real.txt"]);
        assert!(Mirror::is_reserved(&p("/.trackerignore")));
        assert!(!Mirror::is_reserved(&p("/sub/.trackerignore")));
        assert!(!Mirror::is_reserved(&p("/other")));
    }

    #[test]
    fn placeholders_have_the_size_and_time_but_no_blocks() {
        let (_dir, mirror) = mirror();
        mirror.materialize_placeholder(&p("/a/b.bin"), 10_000_000, 1_700_000_000).unwrap();
        let meta = mirror.stat(&p("/a/b.bin")).unwrap();
        assert_eq!(meta.len(), 10_000_000);
        assert_eq!(meta.mtime(), 1_700_000_000);
        assert!(meta.blocks() < 100, "a placeholder must be sparse, used {} blocks", meta.blocks());
    }

    #[test]
    fn a_placeholder_replaces_a_directory_of_the_same_name() {
        let (_dir, mirror) = mirror();
        mirror.ensure_dir(&p("/x")).unwrap();
        mirror.create_file(&p("/x/inner")).unwrap();
        mirror.materialize_placeholder(&p("/x"), 3, 0).unwrap();
        assert!(!mirror.is_dir(&p("/x")));
    }

    #[test]
    fn ensure_dir_replaces_a_file() {
        let (_dir, mirror) = mirror();
        mirror.create_file(&p("/x")).unwrap();
        mirror.ensure_dir(&p("/x")).unwrap();
        assert!(mirror.is_dir(&p("/x")));
    }

    #[test]
    fn atomic_writes_land_complete_with_the_requested_mtime() {
        let (dir, mirror) = mirror();
        let n = mirror.write_atomic_stream(&p("/d/f.txt"), &mut &b"hello world"[..], Some(1_600_000_000)).unwrap();
        assert_eq!(n, 11);
        assert_eq!(mirror.read_at(&p("/d/f.txt"), 0, 100).unwrap(), b"hello world");
        assert_eq!(mirror.stat(&p("/d/f.txt")).unwrap().mtime(), 1_600_000_000);
        assert_eq!(fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0, "no temporary files are left behind");
    }

    #[test]
    fn a_failing_source_leaves_the_destination_untouched() {
        struct Broken(usize);
        impl Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::Error::other("connection reset"));
                }
                self.0 -= 1;
                buf[0] = b'x';
                Ok(1)
            }
        }
        let (dir, mirror) = mirror();
        mirror.write_atomic_stream(&p("/f"), &mut &b"original"[..], None).unwrap();
        assert!(mirror.write_atomic_stream(&p("/f"), &mut Broken(3), None).is_err());
        assert_eq!(mirror.read_at(&p("/f"), 0, 100).unwrap(), b"original");
        assert_eq!(fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn staged_downloads_carry_their_checksum() {
        let (dir, mirror) = mirror();
        let staged = mirror.stage_stream(&mut &b"abc"[..], Some(5)).unwrap();
        assert_eq!(staged.len(), 3);
        assert_eq!(staged.sha256(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(!mirror.exists(&p("/f")), "nothing is visible until it is committed");
        mirror.commit(staged, &p("/f")).unwrap();
        assert_eq!(mirror.read_at(&p("/f"), 0, 9).unwrap(), b"abc");
        // Dropping a staged file instead of committing discards it.
        drop(mirror.stage_stream(&mut &b"zzz"[..], None).unwrap());
        assert_eq!(fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn positional_reads_and_writes() {
        let (_dir, mirror) = mirror();
        mirror.write_at(&p("/f"), 0, b"0123456789").unwrap();
        mirror.write_at(&p("/f"), 3, b"abc").unwrap();
        assert_eq!(mirror.read_at(&p("/f"), 0, 100).unwrap(), b"012abc6789");
        assert_eq!(mirror.read_at(&p("/f"), 8, 100).unwrap(), b"89");
        assert_eq!(mirror.read_at(&p("/f"), 50, 10).unwrap(), b"");
        mirror.write_at(&p("/f"), 12, b"Z").unwrap();
        assert_eq!(mirror.stat(&p("/f")).unwrap().len(), 13, "writing past the end extends the file");
    }

    #[test]
    fn truncation_shrinks_and_extends() {
        let (_dir, mirror) = mirror();
        mirror.write_at(&p("/f"), 0, b"hello").unwrap();
        mirror.truncate(&p("/f"), 2).unwrap();
        assert_eq!(mirror.read_at(&p("/f"), 0, 10).unwrap(), b"he");
        mirror.truncate(&p("/f"), 4).unwrap();
        assert_eq!(mirror.read_at(&p("/f"), 0, 10).unwrap(), b"he\0\0");
    }

    #[test]
    fn checksums_match_the_known_sha256() {
        let (_dir, mirror) = mirror();
        mirror.write_at(&p("/abc"), 0, b"abc").unwrap();
        assert_eq!(
            mirror.sha256(&p("/abc")).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn rename_replaces_files_and_creates_missing_parents() {
        let (_dir, mirror) = mirror();
        mirror.write_at(&p("/a"), 0, b"A").unwrap();
        mirror.write_at(&p("/b"), 0, b"B").unwrap();
        mirror.rename(&p("/a"), &p("/b")).unwrap();
        assert_eq!(mirror.read_at(&p("/b"), 0, 10).unwrap(), b"A");
        mirror.rename(&p("/b"), &p("/deep/er/c")).unwrap();
        assert!(mirror.exists(&p("/deep/er/c")));
        assert!(!mirror.exists(&p("/b")));
    }

    #[test]
    fn removing_a_tree_tolerates_missing_paths() {
        let (_dir, mirror) = mirror();
        mirror.write_at(&p("/d/x/y"), 0, b"1").unwrap();
        mirror.remove_tree(&p("/d")).unwrap();
        mirror.remove_tree(&p("/d")).unwrap();
        assert!(!mirror.exists(&p("/d")));
        assert!(mirror.remove_dir(&p("/nope")).is_err());
    }

    #[test]
    fn remove_dir_refuses_a_non_empty_directory() {
        let (_dir, mirror) = mirror();
        mirror.write_at(&p("/d/f"), 0, b"1").unwrap();
        assert!(mirror.remove_dir(&p("/d")).is_err());
    }

    #[test]
    fn hostile_paths_cannot_leave_the_mirror() {
        let (dir, mirror) = mirror();
        let escaped = p("/../../etc/passwd");
        assert!(mirror.local_path(&escaped).starts_with(mirror.root()));
        mirror.write_at(&p("/../outside.txt"), 0, b"x").unwrap();
        assert!(mirror.root().join("outside.txt").exists());
        assert!(!dir.path().join("outside.txt").exists());
    }

    #[test]
    fn capacity_reports_the_backing_filesystem() {
        let (_dir, mirror) = mirror();
        let cap = mirror.capacity().unwrap();
        assert!(cap.blocks > 0 && cap.block_size > 0 && cap.name_max > 0);
    }

    #[test]
    fn mtimes_can_be_set_on_files_and_directories() {
        let (_dir, mirror) = mirror();
        mirror.write_at(&p("/f"), 0, b"x").unwrap();
        mirror.ensure_dir(&p("/d")).unwrap();
        mirror.set_mtime(&p("/f"), 1_000_000).unwrap();
        mirror.set_mtime(&p("/d"), 2_000_000).unwrap();
        assert_eq!(mirror.stat(&p("/f")).unwrap().mtime(), 1_000_000);
        assert_eq!(mirror.stat(&p("/d")).unwrap().mtime(), 2_000_000);
    }
}
