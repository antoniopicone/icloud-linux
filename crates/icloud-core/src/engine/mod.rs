//! The synchronisation engine.
//!
//! It keeps three things consistent: iCloud Drive (through the [`Drive`]
//! trait), the local [`Mirror`] and the [`SyncState`] database.
//!
//! * `remote.rs` learns what iCloud has, either one folder at a time
//!   ([`CrawlMode::Lazy`]) or by crawling everything ([`CrawlMode::Full`]).
//! * `download.rs` fetches file contents on demand and in the background.
//! * `upload.rs` pushes local changes back.
//!
//! All of it is written against [`Drive`], so it runs unchanged against the
//! in-memory drive in the tests.

/// Emit a log line in the format the sidebar status watcher and the `sync` command
/// parse: `sync <event> key='value' key=42`. Fields that are `None` are left
/// out.
macro_rules! sync_event {
    ($level:ident, $event:literal $(, $key:ident = $value:expr)* $(,)?) => {{
        #[allow(unused_mut)]
        let mut line = String::from(concat!("sync ", $event));
        $(
            if let Some(rendered) = $crate::engine::Field::field(&$value) {
                line.push(' ');
                line.push_str(stringify!($key));
                line.push('=');
                line.push_str(&rendered);
            }
        )*
        tracing::$level!("{line}");
    }};
}

mod download;
mod remote;
mod sync_util;
mod upload;

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use icloud_api::{DeleteMode, Drive, Node, NodeKind};

use crate::{
    config::{Config, ConflictMode, CrawlMode, WarmupMode},
    error::Result,
    mirror::Mirror,
    path::IcPath,
    policy::SyncPolicy,
    state::{Entry, SyncState, now},
};
use sync_util::{DelayQueue, KeyedLocks, Signal, lock};

/// How a value appears in a `sync …` log line.
pub(crate) trait Field {
    fn field(&self) -> Option<String>;
}

impl Field for str {
    fn field(&self) -> Option<String> {
        Some(format!("'{}'", self.replace('\\', "\\\\").replace('\'', "\\'")))
    }
}
impl Field for String {
    fn field(&self) -> Option<String> {
        self.as_str().field()
    }
}
impl<T: Field + ?Sized> Field for &T {
    fn field(&self) -> Option<String> {
        (**self).field()
    }
}
impl Field for IcPath {
    fn field(&self) -> Option<String> {
        self.as_str().field()
    }
}
impl Field for bool {
    fn field(&self) -> Option<String> {
        Some(if *self { "True" } else { "False" }.to_owned())
    }
}
macro_rules! numeric_field {
    ($($t:ty),*) => {$(
        impl Field for $t {
            fn field(&self) -> Option<String> { Some(self.to_string()) }
        }
    )*};
}
numeric_field!(u32, u64, usize, i64);
impl<T: Field> Field for Option<T> {
    fn field(&self) -> Option<String> {
        self.as_ref().and_then(Field::field)
    }
}

/// The parts of [`Config`] the engine cares about.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub crawl_mode: CrawlMode,
    pub warmup_mode: WarmupMode,
    pub conflict_mode: ConflictMode,
    pub delete_mode: DeleteMode,
    pub upload_interval: Duration,
    pub refresh_interval: Duration,
    pub warmup_workers: usize,
    pub auto_sync: bool,
}

impl EngineConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            crawl_mode: config.crawl_mode,
            warmup_mode: config.warmup_mode,
            conflict_mode: config.conflict_mode,
            delete_mode: config.delete_mode,
            upload_interval: config.upload_interval(),
            refresh_interval: config.refresh_interval(),
            warmup_workers: config.warmup_workers,
            auto_sync: config.auto_sync,
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

/// A snapshot of the background work, for status displays and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EngineStats {
    pub downloads_queued: usize,
    pub downloads_completed: u64,
    pub downloads_planned: u64,
}

struct Inner {
    drive: Arc<dyn Drive>,
    mirror: Arc<Mirror>,
    state: Arc<SyncState>,
    policy: SyncPolicy,
    config: EngineConfig,

    /// One holder per path at a time, for anything that changes a path's
    /// mirror content or state.
    path_locks: KeyedLocks<IcPath>,
    /// iCloud downloads misbehave when one session downloads in parallel, so
    /// only one file streams at a time, whoever asked for it.
    download_gate: Mutex<()>,
    downloads: DelayQueue<IcPath>,
    /// Only one upload pass at a time, whoever triggers it.
    upload_lock: Mutex<()>,
    scheduled: Mutex<HashSet<IcPath>>,
    attempts: Mutex<HashMap<IcPath, u32>>,
    planned: AtomicU64,
    completed: AtomicU64,

    stop: Signal,
    refresh_now: Signal,
    threads: Mutex<Vec<JoinHandle<()>>>,
    root: Mutex<Option<Node>>,
    /// Set when iCloud refused a request for want of a valid session.
    auth_lost: AtomicBool,
}

/// Cheap to clone; all clones drive the same engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("crawl_mode", &self.inner.config.crawl_mode).finish_non_exhaustive()
    }
}

impl Engine {
    pub fn new(
        drive: Arc<dyn Drive>,
        mirror: Arc<Mirror>,
        state: Arc<SyncState>,
        policy: SyncPolicy,
        config: EngineConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                drive,
                mirror,
                state,
                policy,
                config,
                path_locks: KeyedLocks::default(),
                download_gate: Mutex::new(()),
                downloads: DelayQueue::default(),
                upload_lock: Mutex::new(()),
                scheduled: Mutex::new(HashSet::new()),
                attempts: Mutex::new(HashMap::new()),
                planned: AtomicU64::new(0),
                completed: AtomicU64::new(0),
                stop: Signal::default(),
                refresh_now: Signal::default(),
                threads: Mutex::new(Vec::new()),
                root: Mutex::new(None),
                auth_lost: AtomicBool::new(false),
            }),
        }
    }

    pub fn crawl_mode(&self) -> CrawlMode {
        self.inner.config.crawl_mode
    }

    pub fn policy(&self) -> &SyncPolicy {
        &self.inner.policy
    }

    pub fn state(&self) -> &Arc<SyncState> {
        &self.inner.state
    }

    pub fn mirror(&self) -> &Arc<Mirror> {
        &self.inner.mirror
    }

    /// True once a request failed because the iCloud session is gone.
    pub fn auth_lost(&self) -> bool {
        self.inner.auth_lost.load(Ordering::Relaxed)
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.stop.is_raised()
    }

    pub fn stats(&self) -> EngineStats {
        EngineStats {
            downloads_queued: self.inner.downloads.len(),
            downloads_completed: self.inner.completed.load(Ordering::Relaxed),
            downloads_planned: self.inner.planned.load(Ordering::Relaxed),
        }
    }

    /// Remember that a failure needs a new sign-in, if it does.
    fn note_failure(&self, err: &crate::Error) {
        if err.is_auth() && !self.inner.auth_lost.swap(true, Ordering::Relaxed) {
            tracing::error!("the iCloud session is no longer valid. Run `icloudctl auth`, then `icloudctl restart`.");
        }
    }

    // ---- lifecycle ---------------------------------------------------------

    /// Bring the local state up to date and start the background threads.
    pub fn start(&self) -> Result<()> {
        let config = &self.inner.config;
        if self.has_persistent_cache()? {
            tracing::info!("using the persistent local cache in {}", self.inner.mirror.root().display());
            self.reconcile_persistent_cache()?;
            if config.crawl_mode == CrawlMode::Full && config.warmup_mode == WarmupMode::Background {
                self.schedule_all_unhydrated()?;
            }
        } else if config.crawl_mode == CrawlMode::Full {
            tracing::info!("no local cache yet; crawling iCloud Drive");
            self.initial_scan()?;
            if config.warmup_mode == WarmupMode::Background {
                self.schedule_all_unhydrated()?;
            }
        } else {
            // Nothing is fetched here. The root only has to exist as a real
            // directory for the mount to have something to sit on; its
            // contents are listed by the first read of "/".
            self.inner.mirror.ensure_dir(&IcPath::root())?;
            tracing::info!("crawl_mode=lazy: no startup crawl; folders are listed the first time they are opened");
        }
        if config.crawl_mode == CrawlMode::Lazy && config.warmup_mode == WarmupMode::Background {
            tracing::info!(
                "crawl_mode=lazy makes warmup_mode=background moot: files are downloaded when opened. \
                 Use crawl_mode: full to warm the whole drive up ahead of time."
            );
        }

        self.spawn_download_workers();
        if config.auto_sync {
            self.spawn("icloud-upload", Self::upload_loop);
            self.spawn("icloud-refresh", Self::refresh_loop);
        } else {
            tracing::info!("auto_sync is off: no background upload or refresh. Use `icloudctl sync` to pull changes.");
        }
        Ok(())
    }

    fn spawn(&self, name: &str, body: fn(&Self)) {
        let engine = self.clone();
        match thread::Builder::new().name(name.to_owned()).spawn(move || body(&engine)) {
            Ok(handle) => lock(&self.inner.threads).push(handle),
            Err(err) => tracing::error!("could not start the {name} thread: {err}"),
        }
    }

    /// Ask every background thread to stop, without waiting for them.
    pub fn shutdown(&self) {
        self.inner.stop.raise();
        self.inner.refresh_now.raise();
        self.inner.downloads.close();
    }

    /// [`shutdown`](Self::shutdown), then wait up to `patience` for the
    /// threads. A thread blocked in a long transfer is abandoned, not killed.
    pub fn shutdown_and_wait(&self, patience: Duration) {
        self.shutdown();
        let deadline = Instant::now() + patience;
        let handles: Vec<_> = std::mem::take(&mut *lock(&self.inner.threads));
        for handle in handles {
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }

    fn upload_loop(&self) {
        while !self.inner.stop.wait(self.inner.config.upload_interval) {
            if let Err(err) = self.sync_dirty() {
                self.note_failure(&err);
                tracing::error!("upload pass failed: {err}");
            }
        }
    }

    fn refresh_loop(&self) {
        if self.inner.config.crawl_mode == CrawlMode::Full && self.has_persistent_cache().unwrap_or(false) {
            tracing::info!("refreshing from iCloud in the background");
            self.run_refresh("startup", false);
        }
        loop {
            let manual = self.inner.refresh_now.wait(self.inner.config.refresh_interval);
            if self.inner.stop.is_raised() {
                break;
            }
            self.inner.refresh_now.clear();
            self.run_refresh(if manual { "manual" } else { "scheduled" }, manual);
        }
    }

    /// Ask the refresh loop to run now.
    pub fn request_refresh(&self) {
        sync_event!(info, "refresh-requested");
        self.inner.refresh_now.raise();
    }

    // ---- startup helpers -------------------------------------------------------

    fn has_persistent_cache(&self) -> Result<bool> {
        Ok(self.inner.state.count_entries()? > 0 && self.inner.mirror.root().is_dir())
    }

    /// Make the mirror agree with the database after a restart, or after the
    /// mirror was tampered with while the daemon was down.
    pub fn reconcile_persistent_cache(&self) -> Result<()> {
        let entries = self.inner.state.list_entries()?;
        let (mut recreated_dirs, mut missing_files) = (0usize, 0usize);
        let mirror = &self.inner.mirror;
        let state = &self.inner.state;

        for entry in &entries {
            if entry.tombstone {
                continue;
            }
            if entry.is_directory() {
                if !mirror.is_dir(&entry.path) {
                    mirror.ensure_dir(&entry.path)?;
                    recreated_dirs += 1;
                }
                continue;
            }

            if let Ok(stats) = mirror.stat(&entry.path) {
                use std::os::unix::fs::MetadataExt as _;
                let mut hydrated = entry.hydrated;
                let mut checksum = entry.local_sha256.clone();
                if entry.kind == NodeKind::File && (hydrated || entry.remote_drivewsid.is_none()) {
                    hydrated = true;
                    // Hashing every file at startup is what once made boot take
                    // minutes; only hash what visibly changed.
                    let changed = stats.len() != entry.size || stats.mtime() != entry.mtime;
                    if changed || checksum.is_none() {
                        checksum = Some(mirror.sha256(&entry.path)?);
                    }
                }
                state.upsert_entry(&Entry {
                    size: stats.len(),
                    mtime: stats.mtime(),
                    hydrated,
                    local_sha256: checksum,
                    ..entry.clone()
                })?;
                continue;
            }

            missing_files += 1;
            if entry.remote_drivewsid.is_some() {
                mirror.materialize_placeholder(&entry.path, entry.size, entry.mtime)?;
                state.upsert_entry(&Entry { hydrated: entry.size == 0, ..entry.clone() })?;
            } else {
                mirror.create_file(&entry.path)?;
                let stats = mirror.stat(&entry.path)?;
                use std::os::unix::fs::MetadataExt as _;
                state.upsert_entry(&Entry {
                    size: stats.len(),
                    mtime: stats.mtime(),
                    hydrated: true,
                    local_sha256: Some(mirror.sha256(&entry.path)?),
                    ..entry.clone()
                })?;
            }
        }
        tracing::info!(
            "persistent cache ready: {} entries, {recreated_dirs} directories recreated, {missing_files} files to fetch again",
            entries.len()
        );
        Ok(())
    }

    fn now(&self) -> i64 {
        now()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Shared scaffolding for the engine tests.

    use std::sync::Arc;

    use icloud_api::memory::MemoryDrive;

    use super::*;

    pub(crate) struct Rig {
        /// Held so the directory outlives the test.
        pub(crate) _dir: tempfile::TempDir,
        pub(crate) drive: Arc<MemoryDrive>,
        pub(crate) engine: Engine,
    }

    impl Rig {
        pub(crate) fn new() -> Self {
            Self::with(SyncPolicy::unrestricted(), EngineConfig { auto_sync: false, ..EngineConfig::default() })
        }

        pub(crate) fn with(policy: SyncPolicy, config: EngineConfig) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let drive = Arc::new(MemoryDrive::new());
            let mirror = Arc::new(Mirror::open(dir.path()).unwrap());
            let state = Arc::new(SyncState::open(&dir.path().join("state.sqlite3")).unwrap());
            let engine = Engine::new(drive.clone(), mirror, state, policy, config);
            Self { _dir: dir, drive, engine }
        }

        pub(crate) fn full() -> Self {
            Self::with(
                SyncPolicy::unrestricted(),
                EngineConfig {
                    crawl_mode: CrawlMode::Full,
                    warmup_mode: WarmupMode::Lazy,
                    auto_sync: false,
                    ..EngineConfig::default()
                },
            )
        }

        pub(crate) fn path(s: &str) -> IcPath {
            IcPath::new(s)
        }

        pub(crate) fn entry(&self, path: &str) -> Option<Entry> {
            self.engine.inner.state.get_entry(&IcPath::new(path)).unwrap()
        }

        pub(crate) fn local(&self, path: &str) -> Vec<u8> {
            self.engine.inner.mirror.read_at(&IcPath::new(path), 0, 1 << 24).unwrap()
        }

        /// Write a file the way the mount does, marking it dirty.
        pub(crate) fn write_local(&self, path: &str, contents: &[u8]) {
            let p = IcPath::new(path);
            let (mirror, state) = (&self.engine.inner.mirror, &self.engine.inner.state);
            mirror.truncate(&p, 0).unwrap();
            mirror.write_at(&p, 0, contents).unwrap();
            let existed = state.get_entry(&p).unwrap().is_some();
            if existed {
                state.mark_dirty(&p, Some(contents.len() as u64), Some(now()), Some(true), true).unwrap();
                state.queue_op("update", &p, None).unwrap();
            } else {
                let mut entry = Entry::local(p.clone(), NodeKind::File, now());
                entry.size = contents.len() as u64;
                state.upsert_entry(&entry).unwrap();
                state.queue_op("create", &p, None).unwrap();
            }
        }

        pub(crate) fn make_local_dir(&self, path: &str) {
            let p = IcPath::new(path);
            self.engine.inner.mirror.ensure_dir(&p).unwrap();
            self.engine.inner.state.upsert_entry(&Entry::local(p.clone(), NodeKind::Folder, now())).unwrap();
            self.engine.inner.state.queue_op("mkdir", &p, None).unwrap();
        }
    }
}
