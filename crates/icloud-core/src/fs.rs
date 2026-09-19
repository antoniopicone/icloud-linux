//! Filesystem semantics, independent of FUSE.
//!
//! [`FsCore`] answers the questions a kernel asks of a filesystem (`getattr`,
//! `readdir`, `write`, `rename`, …) in terms of paths and `errno` values. The
//! FUSE adapter in `icloudd` is a thin translation layer on top, so every rule
//! below is testable without a mount.
//!
//! Behaviour that differs from the earlier Python implementation, on purpose:
//!
//! * `rename` follows POSIX: a file cannot replace a directory, a directory
//!   can only replace an *empty* directory, and nothing can move into itself.
//!   The old code deleted the destination tree unconditionally.
//! * `rmdir` lists a never-opened folder first, so a folder whose contents are
//!   simply not known yet is not mistaken for an empty one and deleted with
//!   everything in it.
//! * A file that cannot be downloaded because it lies outside the sync
//!   boundary fails with `EACCES` instead of reading back as zeros.
//! * A renamed but never-downloaded file is downloaded on read; it used to read
//!   back as zeros.

use std::{io, os::unix::fs::MetadataExt, sync::Arc};

pub use rustix::io::Errno;

use icloud_api::NodeKind;

use crate::{
    config::CrawlMode,
    engine::Engine,
    mirror::{Capacity, Mirror},
    path::{IcPath, is_valid_name},
    policy::SyncPolicy,
    state::{Entry, SyncState, now},
};

pub type FsResult<T> = Result<T, Errno>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
}

/// What `stat` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub kind: FileKind,
    pub size: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    /// Permission bits only (no file-type bits).
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirItem {
    pub name: String,
    pub kind: FileKind,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FsOptions {
    /// Refuse every change with `EROFS`.
    pub read_only: bool,
}

pub struct FsCore {
    mirror: Arc<Mirror>,
    state: Arc<SyncState>,
    policy: SyncPolicy,
    /// `None` while there is no iCloud session: the cache is served read-only.
    engine: Option<Engine>,
    options: FsOptions,
}

impl std::fmt::Debug for FsCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsCore").field("authenticated", &self.engine.is_some()).finish_non_exhaustive()
    }
}

fn errno_of(err: &io::Error) -> Errno {
    Errno::from_io_error(err).unwrap_or(Errno::IO)
}

fn log_io(op: &str, path: &IcPath, err: &io::Error) -> Errno {
    let errno = errno_of(err);
    if !matches!(errno, Errno::NOENT | Errno::EXIST | Errno::NOTEMPTY | Errno::ISDIR | Errno::NOTDIR) {
        tracing::error!("{op} {path}: {err}");
    }
    errno
}

fn log_err(op: &str, path: &IcPath, err: &dyn std::fmt::Display) -> Errno {
    tracing::error!("{op} {path}: {err}");
    Errno::IO
}

impl FsCore {
    pub fn new(
        mirror: Arc<Mirror>,
        state: Arc<SyncState>,
        policy: SyncPolicy,
        engine: Option<Engine>,
        options: FsOptions,
    ) -> Self {
        Self { mirror, state, policy, engine, options }
    }

    pub fn is_authenticated(&self) -> bool {
        self.engine.is_some()
    }

    fn lazy_listing(&self) -> Option<&Engine> {
        self.engine.as_ref().filter(|e| e.crawl_mode() == CrawlMode::Lazy)
    }

    /// May a change be made to all of `paths`?
    fn check_mutation(&self, op: &str, paths: &[&IcPath]) -> FsResult<()> {
        if self.options.read_only {
            return Err(Errno::ROFS);
        }
        if self.engine.is_none() {
            return Err(Errno::ACCESS);
        }
        for path in paths {
            if Mirror::is_reserved(path) {
                return Err(Errno::ACCESS);
            }
            if !self.policy.allows(path) {
                tracing::warn!("{op} {path}: outside the synchronisation boundary");
                return Err(Errno::ACCESS);
            }
        }
        Ok(())
    }

    fn check_name(path: &IcPath) -> FsResult<()> {
        match path.file_name() {
            Some(name) if is_valid_name(name) => Ok(()),
            Some(_) => Err(Errno::NAMETOOLONG),
            None => Err(Errno::INVAL),
        }
    }

    fn entry(&self, op: &str, path: &IcPath) -> FsResult<Option<Entry>> {
        self.state.get_entry(path).map_err(|e| log_err(op, path, &e))
    }

    /// The entry at `path`, if there is a live (not deleted) one.
    fn live_entry(&self, op: &str, path: &IcPath) -> FsResult<Option<Entry>> {
        Ok(self.entry(op, path)?.filter(|e| !e.tombstone))
    }

    fn list_if_lazy(&self, path: &IcPath) {
        if let Some(engine) = self.lazy_listing()
            && let Err(err) = engine.list_directory(path, false)
        {
            // Never fail a lookup because of the network: what is already
            // in the mirror is still shown.
            tracing::debug!("could not list {path}: {err}");
        }
    }

    // ---- reading -------------------------------------------------------------

    pub fn getattr(&self, path: &IcPath) -> FsResult<Attr> {
        if Mirror::is_reserved(path) {
            return Err(Errno::NOENT);
        }
        if let Some(attr) = self.stat_known(path)? {
            return Ok(attr);
        }
        // Unknown here. Under lazy listing that is expected for direct access
        // whose parent was never listed (xdg-open, a typed path, a recent-files
        // entry): list the parent once before giving up.
        if self.lazy_listing().is_some() {
            self.list_if_lazy(&path.parent());
            if let Some(attr) = self.stat_known(path)? {
                return Ok(attr);
            }
        }
        Err(Errno::NOENT)
    }

    fn stat_known(&self, path: &IcPath) -> FsResult<Option<Attr>> {
        let entry = self.entry("getattr", path)?;

        if path.is_root() {
            return Ok(Some(match self.mirror.stat(path) {
                Ok(meta) => attr_from_meta(&meta, None),
                Err(_) => synthetic_dir(now()),
            }));
        }
        if entry.as_ref().is_some_and(|e| e.tombstone) {
            return Ok(None);
        }
        if let Ok(meta) = self.mirror.stat(path) {
            // A placeholder's real size lives in the database.
            let placeholder = entry.as_ref().filter(|e| e.kind == NodeKind::File && !e.hydrated);
            return Ok(Some(attr_from_meta(&meta, placeholder)));
        }
        // Known to the database but absent from the mirror (metadata only).
        Ok(entry.map(|e| {
            let dir = e.is_directory();
            Attr {
                kind: if dir { FileKind::Directory } else { FileKind::File },
                size: e.size,
                atime: now(),
                mtime: nonzero(e.mtime),
                ctime: nonzero(e.mtime),
                perm: if dir { 0o755 } else { 0o644 },
                nlink: if dir { 2 } else { 1 },
                uid: rustix::process::getuid().as_raw(),
                gid: rustix::process::getgid().as_raw(),
            }
        }))
    }

    pub fn readdir(&self, path: &IcPath) -> FsResult<Vec<DirItem>> {
        if Mirror::is_reserved(path) {
            return Err(Errno::NOENT);
        }
        self.list_if_lazy(path);
        if !self.mirror.exists(path) {
            return Err(Errno::NOENT);
        }
        if !self.mirror.is_dir(path) {
            return Err(Errno::NOTDIR);
        }
        let mut items: Vec<DirItem> = self
            .mirror
            .list_dir(path)
            .map_err(|e| log_io("readdir", path, &e))?
            .into_iter()
            .map(|(name, dir)| DirItem { name, kind: if dir { FileKind::Directory } else { FileKind::File } })
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(items)
    }

    /// Check that `path` can be opened; downloads a file that is not local.
    pub fn open(&self, path: &IcPath, for_write: bool) -> FsResult<()> {
        if Mirror::is_reserved(path) {
            return Err(Errno::NOENT);
        }
        if for_write {
            self.check_mutation("open", &[path])?;
        }
        let Some(entry) = self.live_entry("open", path)? else {
            return Err(Errno::NOENT);
        };
        if entry.is_directory() {
            return Err(Errno::ISDIR);
        }
        self.ensure_content(path, &entry)
    }

    /// Make sure a file's bytes are in the mirror before they are read or
    /// modified.
    fn ensure_content(&self, path: &IcPath, entry: &Entry) -> FsResult<()> {
        if entry.hydrated || entry.remote_drivewsid.is_none() {
            return Ok(());
        }
        let Some(engine) = &self.engine else {
            tracing::warn!("{path} is not downloaded and there is no iCloud session; run `icloudctl auth`");
            return Err(Errno::IO);
        };
        if !self.policy.allows(path) {
            return Err(Errno::ACCESS);
        }
        engine.hydrate(path).map_err(|e| {
            if e.is_auth() {
                tracing::error!("cannot download {path}: {e}. Run `icloudctl auth`, then `icloudctl restart`.");
            } else {
                tracing::error!("cannot download {path}: {e}");
            }
            Errno::IO
        })
    }

    pub fn read(&self, path: &IcPath, offset: u64, size: usize) -> FsResult<Vec<u8>> {
        if Mirror::is_reserved(path) {
            return Err(Errno::NOENT);
        }
        let Some(entry) = self.live_entry("read", path)? else { return Err(Errno::NOENT) };
        if entry.is_directory() {
            return Err(Errno::ISDIR);
        }
        self.ensure_content(path, &entry)?;
        self.mirror.read_at(path, offset, size).map_err(|e| log_io("read", path, &e))
    }

    pub fn statfs(&self) -> FsResult<Capacity> {
        self.mirror.capacity().map_err(|e| errno_of(&e))
    }

    // ---- creating ---------------------------------------------------------------

    pub fn create(&self, path: &IcPath, _mode: u32) -> FsResult<Attr> {
        self.check_mutation("create", &[path])?;
        Self::check_name(path)?;
        self.require_parent_dir("create", path)?;
        if self.live_entry("create", path)?.is_some() {
            return Err(Errno::EXIST);
        }
        // A file deleted a moment ago whose delete has not been sent yet must
        // still be deleted on iCloud once its replacement takes the name.
        self.state.bury_tombstone(path).map_err(|e| log_err("create", path, &e))?;

        self.mirror.create_file(path).map_err(|e| log_io("create", path, &e))?;
        let meta = self.mirror.stat(path).map_err(|e| log_io("create", path, &e))?;
        self.state
            .upsert_entry(&Entry::local(path.clone(), NodeKind::File, meta.mtime()))
            .and_then(|()| self.state.queue_op("create", path, None))
            .map_err(|e| log_err("create", path, &e))?;
        Ok(attr_from_meta(&meta, None))
    }

    pub fn mkdir(&self, path: &IcPath, _mode: u32) -> FsResult<()> {
        self.check_mutation("mkdir", &[path])?;
        Self::check_name(path)?;
        self.require_parent_dir("mkdir", path)?;
        if self.live_entry("mkdir", path)?.is_some() || self.mirror.exists(path) {
            return Err(Errno::EXIST);
        }
        self.state.bury_tombstone(path).map_err(|e| log_err("mkdir", path, &e))?;
        self.mirror.ensure_dir(path).map_err(|e| log_io("mkdir", path, &e))?;
        let meta = self.mirror.stat(path).map_err(|e| log_io("mkdir", path, &e))?;
        self.state
            .upsert_entry(&Entry::local(path.clone(), NodeKind::Folder, meta.mtime()))
            .and_then(|()| self.state.queue_op("mkdir", path, None))
            .map_err(|e| log_err("mkdir", path, &e))
    }

    fn require_parent_dir(&self, op: &str, path: &IcPath) -> FsResult<()> {
        let parent = path.parent();
        self.list_if_lazy(&parent);
        if parent.is_root() || self.mirror.is_dir(&parent) {
            Ok(())
        } else if self.mirror.exists(&parent) {
            Err(Errno::NOTDIR)
        } else {
            tracing::debug!("{op} {path}: parent does not exist");
            Err(Errno::NOENT)
        }
    }

    // ---- changing contents ------------------------------------------------------------

    pub fn write(&self, path: &IcPath, offset: u64, data: &[u8]) -> FsResult<usize> {
        self.check_mutation("write", &[path])?;
        let entry = self.live_entry("write", path)?;
        if let Some(entry) = &entry {
            if entry.is_directory() {
                return Err(Errno::ISDIR);
            }
            self.ensure_content(path, entry)?;
        }
        let written = self.mirror.write_at(path, offset, data).map_err(|e| log_io("write", path, &e))?;
        self.record_content_change("write", path, entry.is_some())?;
        Ok(written)
    }

    pub fn truncate(&self, path: &IcPath, len: u64) -> FsResult<()> {
        self.check_mutation("truncate", &[path])?;
        let entry = self.live_entry("truncate", path)?;
        if let Some(entry) = &entry {
            if entry.is_directory() {
                return Err(Errno::ISDIR);
            }
            self.ensure_content(path, entry)?;
        }
        self.mirror.truncate(path, len).map_err(|e| log_io("truncate", path, &e))?;
        self.record_content_change("truncate", path, entry.is_some())
    }

    /// The bytes of `path` changed: note it for the uploader. The checksum is
    /// computed at upload time; hashing the whole file on every `write` call
    /// made large copies quadratic.
    fn record_content_change(&self, op: &str, path: &IcPath, known: bool) -> FsResult<()> {
        let meta = self.mirror.stat(path).map_err(|e| log_io(op, path, &e))?;
        let outcome = if known {
            self.state.mark_dirty(path, Some(meta.len()), Some(meta.mtime()), Some(true), true)
        } else {
            let mut entry = Entry::local(path.clone(), NodeKind::File, meta.mtime());
            entry.size = meta.len();
            self.state.upsert_entry(&entry)
        };
        outcome.and_then(|()| self.state.queue_op("update", path, None)).map_err(|e| log_err(op, path, &e))
    }

    pub fn set_mtime(&self, path: &IcPath, mtime: i64) -> FsResult<()> {
        self.check_mutation("utimens", &[path])?;
        if !self.mirror.exists(path) {
            return Err(Errno::NOENT);
        }
        self.mirror.set_mtime(path, mtime).map_err(|e| log_io("utimens", path, &e))?;
        if self.live_entry("utimens", path)?.is_some() {
            // Only the time changed, so the checksum stays valid.
            self.state.mark_dirty(path, None, Some(mtime), None, false).map_err(|e| log_err("utimens", path, &e))?;
        }
        Ok(())
    }

    /// `chmod` and `chown` cannot be stored on iCloud; they are accepted and
    /// ignored so tools like `cp -a` and `rsync -a` do not fail.
    pub fn accept_permission_change(&self, path: &IcPath) -> FsResult<()> {
        if path.is_root() || self.live_entry("chmod", path)?.is_some() { Ok(()) } else { Err(Errno::NOENT) }
    }

    // ---- removing and renaming ------------------------------------------------------------

    pub fn unlink(&self, path: &IcPath) -> FsResult<()> {
        self.check_mutation("unlink", &[path])?;
        let entry = self.live_entry("unlink", path)?.ok_or(Errno::NOENT)?;
        if entry.is_directory() {
            return Err(Errno::ISDIR);
        }
        if self.mirror.exists(path) {
            self.mirror.remove_file(path).map_err(|e| log_io("unlink", path, &e))?;
        }
        self.forget("unlink", path, &entry)
    }

    pub fn rmdir(&self, path: &IcPath) -> FsResult<()> {
        self.check_mutation("rmdir", &[path])?;
        if path.is_root() {
            return Err(Errno::BUSY);
        }
        // A folder nobody opened has no children here yet, which would make it
        // look empty. Find out what is really in it before removing anything.
        self.list_if_lazy(path);
        let entry = self.live_entry("rmdir", path)?.ok_or(Errno::NOENT)?;
        if !entry.is_directory() {
            return Err(Errno::NOTDIR);
        }
        self.mirror.remove_dir(path).map_err(|e| log_io("rmdir", path, &e))?;
        self.forget("rmdir", path, &entry)
    }

    /// The item is gone locally: delete it on iCloud too, or just forget it if
    /// iCloud never had it.
    fn forget(&self, op: &str, path: &IcPath, entry: &Entry) -> FsResult<()> {
        let outcome = if entry.remote_drivewsid.is_some() {
            self.state.mark_tombstone(path).and_then(|()| self.state.queue_op("delete", path, None))
        } else if entry.is_directory() {
            self.state.remove_subtree(path)
        } else {
            self.state.remove_entry(path)
        };
        outcome.map_err(|e| log_err(op, path, &e))
    }

    pub fn rename(&self, from: &IcPath, to: &IcPath) -> FsResult<()> {
        self.check_mutation("rename", &[from, to])?;
        Self::check_name(to)?;
        if from == to {
            return Ok(());
        }
        if from.is_root() || to.is_root() {
            return Err(Errno::BUSY);
        }
        let source = self.live_entry("rename", from)?.ok_or(Errno::NOENT)?;
        if source.is_directory() && to.is_within(from) {
            return Err(Errno::INVAL);
        }
        self.require_parent_dir("rename", to)?;

        // Every descendant must also stay inside the boundary at its new place.
        if source.is_directory() {
            for item in self.state.fetch_subtree(from).map_err(|e| log_err("rename", from, &e))? {
                let target = item.path.rebase(from, to).ok_or(Errno::INVAL)?;
                self.check_mutation("rename", &[&item.path, &target])?;
            }
        }

        // POSIX rules for an existing destination.
        if let Some(existing) = self.live_entry("rename", to)? {
            match (source.is_directory(), existing.is_directory()) {
                (false, true) => return Err(Errno::ISDIR),
                (true, false) => return Err(Errno::NOTDIR),
                (true, true) => {
                    self.list_if_lazy(to);
                    let occupied = self.mirror.list_dir(to).map_err(|e| log_io("rename", to, &e))?;
                    if !occupied.is_empty() {
                        return Err(Errno::NOTEMPTY);
                    }
                }
                (false, false) => {}
            }
            self.mirror.remove_tree(to).map_err(|e| log_io("rename", to, &e))?;
        }

        self.mirror.rename(from, to).map_err(|e| log_io("rename", from, &e))?;
        self.state
            .rename_tree(from, to, true, false)
            .and_then(|()| self.state.queue_op("rename", from, Some(to)))
            .map_err(|e| log_err("rename", from, &e))
    }
}

fn nonzero(mtime: i64) -> i64 {
    if mtime > 0 { mtime } else { now() }
}

fn synthetic_dir(at: i64) -> Attr {
    Attr {
        kind: FileKind::Directory,
        size: 0,
        atime: at,
        mtime: at,
        ctime: at,
        perm: 0o755,
        nlink: 2,
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
    }
}

/// `stat` of a mirror file. For a file not yet downloaded, the size and time
/// come from the database entry, which records what iCloud says.
fn attr_from_meta(meta: &std::fs::Metadata, placeholder: Option<&Entry>) -> Attr {
    let dir = meta.is_dir();
    let mtime = placeholder.map_or_else(|| meta.mtime(), |e| e.mtime);
    Attr {
        kind: if dir { FileKind::Directory } else { FileKind::File },
        size: placeholder.map_or_else(|| meta.len(), |e| e.size),
        atime: meta.atime(),
        mtime,
        ctime: placeholder.map_or_else(|| meta.ctime(), |e| e.mtime),
        perm: u16::try_from(meta.mode() & 0o7777).unwrap_or(0o644),
        nlink: u32::try_from(meta.nlink()).unwrap_or(1),
        uid: meta.uid(),
        gid: meta.gid(),
    }
}

#[cfg(test)]
mod tests {
    use icloud_api::memory::MemoryDrive;

    use super::*;
    use crate::engine::EngineConfig;

    struct Rig {
        _dir: tempfile::TempDir,
        drive: Arc<MemoryDrive>,
        engine: Engine,
        fs: FsCore,
    }

    fn p(s: &str) -> IcPath {
        IcPath::new(s)
    }

    impl Rig {
        fn with(policy: SyncPolicy, options: FsOptions, config: EngineConfig) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let drive = Arc::new(MemoryDrive::new());
            let mirror = Arc::new(Mirror::open(dir.path()).unwrap());
            let state = Arc::new(SyncState::open(&dir.path().join("state.sqlite3")).unwrap());
            let engine = Engine::new(drive.clone(), mirror.clone(), state.clone(), policy.clone(), config);
            let fs = FsCore::new(mirror, state, policy, Some(engine.clone()), options);
            Self { _dir: dir, drive, engine, fs }
        }

        fn new() -> Self {
            Self::with(
                SyncPolicy::unrestricted(),
                FsOptions::default(),
                EngineConfig { auto_sync: false, ..EngineConfig::default() },
            )
        }

        fn seeded() -> Self {
            let rig = Self::new();
            rig.drive.add_file("/", "top.txt", b"top content", 1_700_000_000);
            rig.drive.add_folder("/", "Docs");
            rig.drive.add_file("/Docs", "a.txt", b"alpha", 1_700_000_100);
            rig.drive.add_folder("/", "Empty");
            rig
        }

        fn names(&self, dir: &str) -> Vec<String> {
            self.fs.readdir(&p(dir)).unwrap().into_iter().map(|i| i.name).collect()
        }

        fn entry(&self, path: &str) -> Option<Entry> {
            self.engine.state().get_entry(&p(path)).unwrap()
        }
    }

    // ---- lookup and listing -----------------------------------------------------

    #[test]
    fn the_root_always_exists_and_is_a_directory() {
        let rig = Rig::new();
        let attr = rig.fs.getattr(&IcPath::root()).unwrap();
        assert_eq!(attr.kind, FileKind::Directory);
    }

    #[test]
    fn readdir_lists_a_folder_on_first_access() {
        let rig = Rig::seeded();
        assert_eq!(rig.names("/"), ["Docs", "Empty", "top.txt"]);
        assert_eq!(rig.drive.calls_matching("children:/"), 1);
        assert_eq!(rig.names("/Docs"), ["a.txt"]);
    }

    #[test]
    fn getattr_of_a_placeholder_reports_the_remote_size_and_time() {
        let rig = Rig::seeded();
        let attr = rig.fs.getattr(&p("/top.txt")).unwrap();
        assert_eq!(attr.kind, FileKind::File);
        assert_eq!((attr.size, attr.mtime), (11, 1_700_000_000));
    }

    #[test]
    fn getattr_lists_the_parent_when_a_path_is_reached_directly() {
        let rig = Rig::seeded();
        rig.fs.getattr(&p("/Docs")).unwrap(); // lists "/" implicitly
        let attr = rig.fs.getattr(&p("/Docs/a.txt")).unwrap(); // lists "/Docs" implicitly
        assert_eq!(attr.size, 5);
    }

    #[test]
    fn a_missing_path_is_enoent_and_costs_at_most_one_listing_of_its_parent() {
        let rig = Rig::seeded();
        assert_eq!(rig.fs.getattr(&p("/nope.txt")).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.fs.getattr(&p("/nope.txt")).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.drive.calls_matching("children:/"), 1, "the second miss must not go back to the network");
    }

    #[test]
    fn full_crawl_mode_never_lists_on_access() {
        let rig = Rig::with(
            SyncPolicy::unrestricted(),
            FsOptions::default(),
            EngineConfig { crawl_mode: CrawlMode::Full, auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_file("/", "f", b"x", 1);
        assert!(rig.fs.readdir(&IcPath::root()).unwrap().is_empty());
        assert_eq!(rig.fs.getattr(&p("/f")).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.drive.calls().len(), 0);
    }

    #[test]
    fn network_failures_do_not_break_lookups_of_cached_data() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.drive.set_outage(Some(icloud_api::memory::Outage::Offline));
        rig.engine.state().mark_folder_listed_at(&IcPath::root(), None, 0).unwrap();
        assert_eq!(rig.names("/"), ["Docs", "Empty", "top.txt"], "stale but still shown");
    }

    #[test]
    fn the_indexer_marker_is_invisible() {
        let rig = Rig::new();
        assert_eq!(rig.fs.getattr(&p("/.trackerignore")).unwrap_err(), Errno::NOENT);
        assert!(rig.names("/").is_empty());
        assert_eq!(rig.fs.create(&p("/.trackerignore"), 0o644).unwrap_err(), Errno::ACCESS);
        assert_eq!(rig.fs.open(&p("/.trackerignore"), false).unwrap_err(), Errno::NOENT);
    }

    #[test]
    fn readdir_errors() {
        let rig = Rig::seeded();
        rig.names("/");
        assert_eq!(rig.fs.readdir(&p("/top.txt")).unwrap_err(), Errno::NOTDIR);
        assert_eq!(rig.fs.readdir(&p("/missing")).unwrap_err(), Errno::NOENT);
    }

    // ---- reading -------------------------------------------------------------------------

    #[test]
    fn reading_downloads_the_file_on_demand() {
        let rig = Rig::seeded();
        rig.names("/");
        assert_eq!(rig.drive.calls_matching("open:"), 0);
        assert_eq!(rig.fs.read(&p("/top.txt"), 0, 4).unwrap(), b"top ");
        assert_eq!(rig.fs.read(&p("/top.txt"), 4, 100).unwrap(), b"content");
        assert_eq!(rig.drive.calls_matching("open:"), 1);
    }

    #[test]
    fn open_downloads_up_front_so_the_first_read_is_local() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.open(&p("/top.txt"), false).unwrap();
        assert_eq!(rig.drive.calls_matching("open:"), 1);
        assert!(rig.entry("/top.txt").unwrap().hydrated);
    }

    #[test]
    fn a_failed_download_is_eio_and_the_next_read_retries() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.drive.set_outage(Some(icloud_api::memory::Outage::Offline));
        assert_eq!(rig.fs.read(&p("/top.txt"), 0, 10).unwrap_err(), Errno::IO);
        rig.drive.set_outage(None);
        assert_eq!(rig.fs.read(&p("/top.txt"), 0, 3).unwrap(), b"top");
    }

    #[test]
    fn outside_the_boundary_a_file_is_not_readable_rather_than_reading_as_zeros() {
        let rig = Rig::with(
            SyncPolicy::new(&["/Docs"], &[] as &[&str]),
            FsOptions::default(),
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_file("/", "top.txt", b"secret", 1);
        rig.drive.add_folder("/", "Docs");
        rig.drive.add_file("/Docs", "ok.txt", b"fine", 1);
        rig.names("/");
        assert_eq!(rig.fs.read(&p("/top.txt"), 0, 10).unwrap_err(), Errno::ACCESS);
        assert_eq!(rig.fs.open(&p("/top.txt"), false).unwrap_err(), Errno::ACCESS);
        assert_eq!(rig.drive.calls_matching("open:"), 0);
        rig.names("/Docs");
        assert_eq!(rig.fs.read(&p("/Docs/ok.txt"), 0, 10).unwrap(), b"fine");
        // The listing (names, sizes) is still available outside the boundary.
        assert_eq!(rig.fs.getattr(&p("/top.txt")).unwrap().size, 6);
    }

    #[test]
    fn a_renamed_never_downloaded_file_reads_its_real_content() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.rename(&p("/top.txt"), &p("/moved.txt")).unwrap();
        assert_eq!(rig.fs.read(&p("/moved.txt"), 0, 100).unwrap(), b"top content", "must not read back as zeros");
    }

    #[test]
    fn reading_a_directory_or_a_missing_file_fails_sensibly() {
        let rig = Rig::seeded();
        rig.names("/");
        assert_eq!(rig.fs.read(&p("/Docs"), 0, 1).unwrap_err(), Errno::ISDIR);
        assert_eq!(rig.fs.read(&p("/nope"), 0, 1).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.fs.open(&p("/Docs"), false).unwrap_err(), Errno::ISDIR);
    }

    #[test]
    fn without_a_session_the_cache_is_served_and_the_rest_is_eio() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.read(&p("/top.txt"), 0, 1).unwrap();
        let offline = FsCore::new(
            rig.engine.mirror().clone(),
            rig.engine.state().clone(),
            SyncPolicy::unrestricted(),
            None,
            FsOptions::default(),
        );
        assert_eq!(offline.read(&p("/top.txt"), 0, 3).unwrap(), b"top", "already downloaded");
        rig.engine.state().mark_folder_listed_at(&IcPath::root(), None, 0).unwrap();
        assert!(offline.getattr(&p("/Docs")).is_ok());
        let entry_only = rig.engine.state().get_entry(&p("/Docs")).unwrap();
        assert!(entry_only.is_some());
        assert_eq!(offline.write(&p("/top.txt"), 0, b"x").unwrap_err(), Errno::ACCESS);
        assert_eq!(offline.create(&p("/new"), 0o644).unwrap_err(), Errno::ACCESS);
        assert!(!offline.is_authenticated());
    }

    #[test]
    fn a_not_downloaded_file_without_a_session_is_eio() {
        let rig = Rig::seeded();
        rig.names("/");
        let offline = FsCore::new(
            rig.engine.mirror().clone(),
            rig.engine.state().clone(),
            SyncPolicy::unrestricted(),
            None,
            FsOptions::default(),
        );
        assert_eq!(offline.read(&p("/top.txt"), 0, 3).unwrap_err(), Errno::IO);
    }

    // ---- creating and writing -------------------------------------------------------------------

    #[test]
    fn created_files_are_local_dirty_and_queued() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.names("/Docs");
        rig.fs.create(&p("/Docs/new.txt"), 0o644).unwrap();
        let entry = rig.entry("/Docs/new.txt").unwrap();
        assert!(entry.dirty && entry.hydrated && entry.remote_drivewsid.is_none());
        assert!(rig.engine.state().has_pending_content_change(&p("/Docs/new.txt")).unwrap());
        assert!(rig.names("/Docs").contains(&"new.txt".to_owned()));
    }

    #[test]
    fn creating_over_an_existing_file_or_in_a_missing_folder_fails() {
        let rig = Rig::seeded();
        rig.names("/");
        assert_eq!(rig.fs.create(&p("/top.txt"), 0o644).unwrap_err(), Errno::EXIST);
        assert_eq!(rig.fs.create(&p("/nowhere/f"), 0o644).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.fs.create(&p("/top.txt/f"), 0o644).unwrap_err(), Errno::NOTDIR);
        assert_eq!(rig.fs.create(&p(&format!("/{}", "x".repeat(300))), 0o644).unwrap_err(), Errno::NAMETOOLONG);
    }

    #[test]
    fn writes_go_to_the_mirror_and_mark_the_file_dirty() {
        let rig = Rig::seeded();
        rig.fs.create(&p("/w.txt"), 0o644).unwrap();
        assert_eq!(rig.fs.write(&p("/w.txt"), 0, b"hello").unwrap(), 5);
        assert_eq!(rig.fs.write(&p("/w.txt"), 5, b" world").unwrap(), 6);
        assert_eq!(rig.fs.read(&p("/w.txt"), 0, 100).unwrap(), b"hello world");
        let entry = rig.entry("/w.txt").unwrap();
        assert_eq!(entry.size, 11);
        assert!(entry.dirty && entry.local_sha256.is_none(), "the checksum is computed at upload time");
        assert_eq!(rig.engine.state().pending_op_count().unwrap(), 1, "one queued op, not one per write");
    }

    #[test]
    fn writing_to_a_synced_file_downloads_it_first_so_the_rest_is_kept() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.write(&p("/top.txt"), 0, b"TOP").unwrap();
        assert_eq!(rig.fs.read(&p("/top.txt"), 0, 100).unwrap(), b"TOP content");
        assert!(rig.entry("/top.txt").unwrap().dirty);
    }

    #[test]
    fn truncating_a_synced_file_keeps_the_prefix() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.truncate(&p("/top.txt"), 3).unwrap();
        assert_eq!(rig.fs.read(&p("/top.txt"), 0, 100).unwrap(), b"top");
        assert_eq!(rig.entry("/top.txt").unwrap().size, 3);
    }

    #[test]
    fn writing_to_a_directory_is_eisdir() {
        let rig = Rig::seeded();
        rig.names("/");
        assert_eq!(rig.fs.write(&p("/Docs"), 0, b"x").unwrap_err(), Errno::ISDIR);
        assert_eq!(rig.fs.truncate(&p("/Docs"), 0).unwrap_err(), Errno::ISDIR);
    }

    #[test]
    fn recreating_a_just_deleted_file_still_deletes_the_old_remote_one() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.unlink(&p("/top.txt")).unwrap();
        rig.fs.create(&p("/top.txt"), 0o644).unwrap();
        rig.fs.write(&p("/top.txt"), 0, b"new").unwrap();

        rig.engine.sync_dirty().unwrap();

        assert_eq!(rig.drive.contents("/top.txt").unwrap(), b"new");
        assert_eq!(rig.drive.trashed(), ["top.txt"], "the original must have been deleted, not orphaned");
        assert!(!rig.drive.exists("/top 2.txt"));
    }

    // ---- directories -----------------------------------------------------------------------------

    #[test]
    fn mkdir_creates_a_local_dirty_folder() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.mkdir(&p("/New"), 0o755).unwrap();
        assert!(rig.entry("/New").unwrap().dirty);
        assert_eq!(rig.fs.mkdir(&p("/New"), 0o755).unwrap_err(), Errno::EXIST);
        assert_eq!(rig.fs.mkdir(&p("/a/b/c"), 0o755).unwrap_err(), Errno::NOENT);
    }

    #[test]
    fn rmdir_removes_only_empty_folders() {
        let rig = Rig::seeded();
        rig.names("/");
        assert_eq!(rig.fs.rmdir(&p("/Docs")).unwrap_err(), Errno::NOTEMPTY);
        rig.fs.rmdir(&p("/Empty")).unwrap();
        assert!(rig.entry("/Empty").unwrap().tombstone);
        assert_eq!(rig.fs.getattr(&p("/Empty")).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.fs.rmdir(&p("/top.txt")).unwrap_err(), Errno::NOTDIR);
        assert_eq!(rig.fs.rmdir(&p("/ghost")).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.fs.rmdir(&IcPath::root()).unwrap_err(), Errno::BUSY);
    }

    #[test]
    fn rmdir_of_a_never_opened_folder_does_not_mistake_it_for_empty() {
        let rig = Rig::seeded();
        rig.names("/"); // "/Docs" is known but its contents were never listed
        assert!(rig.entry("/Docs/a.txt").is_none());
        assert_eq!(rig.fs.rmdir(&p("/Docs")).unwrap_err(), Errno::NOTEMPTY);
        assert!(rig.drive.exists("/Docs/a.txt"));
        rig.engine.sync_dirty().unwrap();
        assert!(rig.drive.exists("/Docs"), "nothing may have been deleted remotely");
    }

    #[test]
    fn rmdir_of_a_local_only_folder_leaves_no_trace() {
        let rig = Rig::seeded();
        rig.fs.mkdir(&p("/Scratch"), 0o755).unwrap();
        rig.fs.rmdir(&p("/Scratch")).unwrap();
        assert!(rig.entry("/Scratch").is_none());
        assert_eq!(rig.engine.state().pending_op_count().unwrap(), 0);
    }

    // ---- unlink ------------------------------------------------------------------------------------

    #[test]
    fn unlink_of_a_synced_file_leaves_a_tombstone_for_the_uploader() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.unlink(&p("/top.txt")).unwrap();
        assert!(rig.entry("/top.txt").unwrap().tombstone);
        assert_eq!(rig.fs.getattr(&p("/top.txt")).unwrap_err(), Errno::NOENT);
        assert!(!rig.names("/").contains(&"top.txt".to_owned()));
        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.trashed(), ["top.txt"]);
    }

    #[test]
    fn unlink_of_a_local_only_file_just_forgets_it() {
        let rig = Rig::seeded();
        rig.fs.create(&p("/tmp.txt"), 0o644).unwrap();
        rig.fs.unlink(&p("/tmp.txt")).unwrap();
        assert!(rig.entry("/tmp.txt").is_none());
        assert_eq!(rig.engine.state().pending_op_count().unwrap(), 0);
    }

    #[test]
    fn unlink_errors() {
        let rig = Rig::seeded();
        rig.names("/");
        assert_eq!(rig.fs.unlink(&p("/Docs")).unwrap_err(), Errno::ISDIR);
        assert_eq!(rig.fs.unlink(&p("/ghost")).unwrap_err(), Errno::NOENT);
    }

    // ---- rename (POSIX) -------------------------------------------------------------------------------

    #[test]
    fn a_plain_rename_moves_the_entry_and_the_bytes() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.read(&p("/top.txt"), 0, 1).unwrap();
        rig.fs.rename(&p("/top.txt"), &p("/Docs/moved.txt")).unwrap();
        assert_eq!(rig.fs.getattr(&p("/top.txt")).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.fs.read(&p("/Docs/moved.txt"), 0, 100).unwrap(), b"top content");
        let entry = rig.entry("/Docs/moved.txt").unwrap();
        assert!(entry.dirty);
        assert_eq!(entry.synced_path, Some(p("/top.txt")), "iCloud still knows the old name until the move is sent");
    }

    #[test]
    fn renaming_a_file_over_a_directory_is_refused_and_destroys_nothing() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.names("/Docs");
        assert_eq!(rig.fs.rename(&p("/top.txt"), &p("/Docs")).unwrap_err(), Errno::ISDIR);
        assert!(rig.entry("/Docs/a.txt").is_some(), "the directory and its contents must survive");
        assert!(rig.entry("/top.txt").is_some());
    }

    #[test]
    fn renaming_a_directory_over_a_file_is_refused() {
        let rig = Rig::seeded();
        rig.names("/");
        assert_eq!(rig.fs.rename(&p("/Docs"), &p("/top.txt")).unwrap_err(), Errno::NOTDIR);
    }

    #[test]
    fn renaming_a_directory_over_a_non_empty_directory_is_refused() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.mkdir(&p("/Other"), 0o755).unwrap();
        assert_eq!(rig.fs.rename(&p("/Other"), &p("/Docs")).unwrap_err(), Errno::NOTEMPTY);
        assert!(rig.entry("/Docs/a.txt").is_some() || rig.drive.exists("/Docs/a.txt"));
    }

    #[test]
    fn renaming_a_directory_over_an_empty_directory_replaces_it() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.names("/Empty");
        rig.fs.mkdir(&p("/Other"), 0o755).unwrap();
        rig.fs.rename(&p("/Other"), &p("/Empty")).unwrap();
        assert!(rig.fs.getattr(&p("/Empty")).is_ok());
        assert_eq!(rig.fs.getattr(&p("/Other")).unwrap_err(), Errno::NOENT);
        rig.engine.sync_dirty().unwrap();
        assert!(rig.drive.exists("/Empty"));
        assert_eq!(rig.drive.trashed(), ["Empty"], "the replaced folder is deleted remotely");
    }

    #[test]
    fn a_file_replaces_a_file_and_the_old_remote_one_is_deleted() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.names("/Docs");
        rig.fs.rename(&p("/top.txt"), &p("/Docs/a.txt")).unwrap();
        assert_eq!(rig.fs.read(&p("/Docs/a.txt"), 0, 100).unwrap(), b"top content");
        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.contents("/Docs/a.txt").unwrap(), b"top content");
        assert_eq!(rig.drive.trashed(), ["a.txt"]);
    }

    #[test]
    fn a_directory_cannot_be_moved_into_itself() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.names("/Docs");
        assert_eq!(rig.fs.rename(&p("/Docs"), &p("/Docs/inner")).unwrap_err(), Errno::INVAL);
    }

    #[test]
    fn renaming_a_directory_carries_its_descendants() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.names("/Docs");
        rig.fs.rename(&p("/Docs"), &p("/Papers")).unwrap();
        assert_eq!(rig.names("/Papers"), ["a.txt"]);
        assert_eq!(rig.fs.read(&p("/Papers/a.txt"), 0, 10).unwrap(), b"alpha");
    }

    #[test]
    fn rename_to_itself_and_of_missing_paths() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.rename(&p("/top.txt"), &p("/top.txt")).unwrap();
        assert_eq!(rig.fs.rename(&p("/ghost"), &p("/x")).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.fs.rename(&p("/top.txt"), &p("/nowhere/x")).unwrap_err(), Errno::NOENT);
        assert_eq!(rig.fs.rename(&IcPath::root(), &p("/x")).unwrap_err(), Errno::BUSY);
    }

    // ---- boundary and read-only -----------------------------------------------------------------------

    #[test]
    fn every_kind_of_change_outside_the_boundary_is_refused_without_side_effects() {
        let rig = Rig::with(
            SyncPolicy::new(&["/Allowed"], &["/Allowed/Skip"]),
            FsOptions::default(),
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_folder("/", "Allowed");
        rig.drive.add_folder("/Allowed", "Skip");
        rig.drive.add_file("/", "outside.txt", b"x", 1);
        rig.drive.add_file("/Allowed/Skip", "skipped.txt", b"x", 1);
        rig.names("/");
        rig.names("/Allowed");
        rig.names("/Allowed/Skip");

        let ops_before = rig.engine.state().pending_op_count().unwrap();
        for result in [
            rig.fs.create(&p("/new.txt"), 0o644).map(drop),
            rig.fs.mkdir(&p("/newdir"), 0o755),
            rig.fs.write(&p("/outside.txt"), 0, b"x").map(drop),
            rig.fs.truncate(&p("/outside.txt"), 0),
            rig.fs.unlink(&p("/outside.txt")),
            rig.fs.rename(&p("/outside.txt"), &p("/Allowed/in.txt")),
            rig.fs.rename(&p("/Allowed"), &p("/Moved")),
            rig.fs.set_mtime(&p("/outside.txt"), 5),
            rig.fs.create(&p("/Allowed/Skip/x"), 0o644).map(drop),
            rig.fs.unlink(&p("/Allowed/Skip/skipped.txt")),
        ] {
            assert_eq!(result.unwrap_err(), Errno::ACCESS);
        }
        assert_eq!(rig.engine.state().pending_op_count().unwrap(), ops_before);
        assert!(rig.entry("/outside.txt").is_some_and(|e| !e.dirty && !e.tombstone));
        rig.fs.create(&p("/Allowed/fine.txt"), 0o644).unwrap();
    }

    #[test]
    fn renaming_a_folder_is_refused_when_an_excluded_descendant_would_move() {
        let rig = Rig::with(
            SyncPolicy::new(&["/allowed"], &["/allowed/parent/excluded"]),
            FsOptions::default(),
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_folder("/", "allowed");
        rig.drive.add_folder("/allowed", "parent");
        rig.drive.add_folder("/allowed/parent", "excluded");
        rig.drive.add_file("/allowed/parent/excluded", "f.txt", b"x", 1);
        rig.names("/");
        rig.names("/allowed");
        rig.names("/allowed/parent");
        rig.names("/allowed/parent/excluded");

        assert_eq!(rig.fs.rename(&p("/allowed/parent"), &p("/allowed/newparent")).unwrap_err(), Errno::ACCESS);
        assert!(rig.entry("/allowed/parent/excluded/f.txt").is_some());
        assert!(rig.entry("/allowed/newparent").is_none());
    }

    #[test]
    fn read_only_mode_refuses_everything_that_changes_something() {
        let rig = Rig::with(
            SyncPolicy::unrestricted(),
            FsOptions { read_only: true },
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_file("/", "f.txt", b"data", 1);
        rig.names("/");
        assert_eq!(rig.fs.create(&p("/n"), 0o644).unwrap_err(), Errno::ROFS);
        assert_eq!(rig.fs.write(&p("/f.txt"), 0, b"x").unwrap_err(), Errno::ROFS);
        assert_eq!(rig.fs.unlink(&p("/f.txt")).unwrap_err(), Errno::ROFS);
        assert_eq!(rig.fs.open(&p("/f.txt"), true).unwrap_err(), Errno::ROFS);
        assert_eq!(rig.fs.read(&p("/f.txt"), 0, 4).unwrap(), b"data", "reading still works");
    }

    // ---- metadata --------------------------------------------------------------------------------------------

    #[test]
    fn utimens_sets_the_time_and_queues_only_a_metadata_change() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.read(&p("/top.txt"), 0, 1).unwrap();
        rig.fs.set_mtime(&p("/top.txt"), 1_500_000_000).unwrap();
        assert_eq!(rig.fs.getattr(&p("/top.txt")).unwrap().mtime, 1_500_000_000);
        assert!(rig.entry("/top.txt").unwrap().dirty);
        assert!(!rig.engine.state().has_pending_content_change(&p("/top.txt")).unwrap());
        assert_eq!(rig.fs.set_mtime(&p("/ghost"), 1).unwrap_err(), Errno::NOENT);
    }

    #[test]
    fn touching_a_file_does_not_upload_its_bytes_again() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.read(&p("/top.txt"), 0, 1).unwrap();
        rig.fs.set_mtime(&p("/top.txt"), 1_500_000_000).unwrap();
        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.calls_matching("upload:"), 0);
        assert!(!rig.entry("/top.txt").unwrap().dirty);
    }

    #[test]
    fn chmod_and_chown_are_accepted_for_existing_paths_only() {
        let rig = Rig::seeded();
        rig.names("/");
        rig.fs.accept_permission_change(&p("/top.txt")).unwrap();
        rig.fs.accept_permission_change(&IcPath::root()).unwrap();
        assert_eq!(rig.fs.accept_permission_change(&p("/ghost")).unwrap_err(), Errno::NOENT);
        assert!(!rig.entry("/top.txt").unwrap().dirty, "ignored means ignored");
    }

    #[test]
    fn statfs_reports_the_cache_filesystem() {
        assert!(Rig::new().fs.statfs().unwrap().blocks > 0);
    }

    // ---- an end-to-end round trip ---------------------------------------------------------------------------------

    #[test]
    fn a_full_session_of_ordinary_use_ends_with_icloud_matching_the_local_tree() {
        let rig = Rig::seeded();
        // Browse, edit, add, move, delete — like a person would.
        rig.names("/");
        rig.names("/Docs");
        rig.fs.write(&p("/Docs/a.txt"), 0, b"ALPHA").unwrap();
        rig.fs.mkdir(&p("/Projects"), 0o755).unwrap();
        rig.fs.create(&p("/Projects/plan.md"), 0o644).unwrap();
        rig.fs.write(&p("/Projects/plan.md"), 0, b"# plan").unwrap();
        rig.fs.rename(&p("/top.txt"), &p("/Projects/top.txt")).unwrap();
        rig.fs.rmdir(&p("/Empty")).unwrap();

        rig.engine.sync_dirty().unwrap();

        assert_eq!(rig.drive.contents("/Docs/a.txt").unwrap(), b"ALPHA");
        assert_eq!(rig.drive.contents("/Projects/plan.md").unwrap(), b"# plan");
        assert_eq!(rig.drive.contents("/Projects/top.txt").unwrap(), b"top content");
        assert!(!rig.drive.exists("/top.txt") && !rig.drive.exists("/Empty"));
        assert!(rig.engine.state().list_dirty_entries().unwrap().is_empty(), "everything is in sync");
    }
}
