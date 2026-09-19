//! Fetching file contents: on demand when a file is opened, and in the
//! background with retries.

use super::*;

/// Give up waiting for a stable answer after this many attempts to hydrate a
/// file that keeps changing under us.
const MAX_HYDRATE_ATTEMPTS: usize = 3;

impl Engine {
    /// Make sure the contents of `path` are in the mirror, downloading them if
    /// needed. Does nothing for paths outside the sync boundary, for folders,
    /// and for files that are already local; the caller must not assume the
    /// file is hydrated afterwards (use [`hydrate`](Self::hydrate) for that).
    pub fn ensure_local_file(&self, path: &IcPath) -> Result<()> {
        let inner = &self.inner;
        if !inner.policy.allows(path) {
            return Ok(());
        }
        let needs_work = |entry: &Option<Entry>| {
            entry
                .as_ref()
                .is_some_and(|e| e.kind == NodeKind::File && !e.tombstone && !(e.hydrated && inner.mirror.exists(path)))
        };
        if !needs_work(&inner.state.get_entry(path)?) {
            return Ok(());
        }

        let _guard = inner.path_locks.acquire(path);
        // Somebody else may have done it while we waited for the lock.
        let Some(entry) = inner.state.get_entry(path)? else { return Ok(()) };
        if !needs_work(&Some(entry.clone())) {
            return Ok(());
        }

        let Some(node) = entry.node() else {
            // Created here, never on iCloud: whatever is in the mirror is the file.
            inner.mirror.create_file(path)?;
            let checksum = inner.mirror.sha256(path)?;
            let stats = inner.mirror.stat(path)?;
            inner.state.mark_hydrated(path, Some(&checksum), Some(stats.len()), Some(mtime_of(&stats)))?;
            sync_event!(info, "hydrate-complete", path = path, source = "local", size = stats.len());
            return Ok(());
        };

        sync_event!(info, "hydrate-start", path = path, drivewsid = node.drivewsid, size = node.size);
        let staged = {
            let _gate = lock(&inner.download_gate);
            let mut reader = inner.drive.open(&node).map_err(|err| {
                let err = crate::Error::from(err);
                self.note_failure(&err);
                err
            })?;
            inner.mirror.stage_stream(&mut reader, Some(entry.mtime))?
        };
        if node.size != 0 && staged.len() != node.size {
            tracing::warn!("{path}: iCloud announced {} bytes but sent {}", node.size, staged.len());
        }

        // The download may have taken minutes. Only keep it if the file is
        // still the one we asked for and nobody wrote to it meanwhile;
        // otherwise the caller (or the next refresh) asks again for the
        // current version. A file that was merely renamed is dirty but still
        // wants its content.
        let untouched = !inner.state.has_pending_content_change(path)?;
        let still_wanted = untouched
            && inner.state.get_entry(path)?.is_some_and(|now| {
                now.remote_drivewsid == entry.remote_drivewsid && now.remote_etag == entry.remote_etag && !now.tombstone
            });
        if !still_wanted {
            tracing::debug!("{path} changed while it was downloading; discarding the download");
            return Ok(());
        }
        let checksum = staged.sha256().to_owned();
        inner.mirror.commit(staged, path)?;
        let stats = inner.mirror.stat(path)?;
        inner.state.mark_hydrated(path, Some(&checksum), Some(stats.len()), Some(mtime_of(&stats)))?;
        sync_event!(info, "hydrate-complete", path = path, source = "remote", size = stats.len());
        Ok(())
    }

    /// Like [`ensure_local_file`](Self::ensure_local_file), but guarantees the
    /// file really is local afterwards, retrying when it changed on iCloud
    /// mid-download. Errors for paths outside the sync boundary.
    pub fn hydrate(&self, path: &IcPath) -> Result<()> {
        if !self.inner.policy.allows(path) {
            return Err(crate::Error::Forbidden(path.to_string()));
        }
        for _ in 0..MAX_HYDRATE_ATTEMPTS {
            self.ensure_local_file(path)?;
            match self.inner.state.get_entry(path)? {
                Some(e) if e.kind == NodeKind::File && !e.tombstone && !e.hydrated => {}
                _ => return Ok(()),
            }
        }
        Err(crate::Error::Setup(format!("{path} kept changing on iCloud while it was downloading")))
    }

    // ---- background warm-up ------------------------------------------------------

    pub fn schedule_download(&self, path: &IcPath) {
        self.schedule_download_after(path, Duration::ZERO);
    }

    fn schedule_download_after(&self, path: &IcPath, delay: Duration) {
        let inner = &self.inner;
        if !inner.policy.allows(path) || inner.stop.is_raised() {
            return;
        }
        if !lock(&inner.scheduled).insert(path.clone()) {
            return; // already queued or running
        }
        let secs = delay.as_secs();
        if delay.is_zero() {
            sync_event!(debug, "download-scheduled", path = path);
        } else {
            sync_event!(info, "download-scheduled", path = path, delay_seconds = secs);
        }
        inner.downloads.push(path.clone(), delay);
    }

    pub(super) fn spawn_download_workers(&self) {
        for n in 0..self.inner.config.warmup_workers {
            self.spawn(&format!("icloud-warmup-{n}"), Self::download_worker);
        }
    }

    fn download_worker(&self) {
        while let Some(path) = self.inner.downloads.pop() {
            self.download_job(&path);
        }
    }

    /// Delay before retry number `attempt` (1-based): 5 s, 10 s, 20 s … capped at 5 min.
    pub(super) fn retry_delay(attempt: u32) -> Duration {
        Duration::from_secs(300.min(5u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(16))))
    }

    /// Fetch one queued file, scheduling a retry with backoff on failure.
    pub(super) fn download_job(&self, path: &IcPath) {
        let inner = &self.inner;
        let mut retry: Option<Duration> = None;

        match self.ensure_local_file(path) {
            Ok(()) => {
                lock(&inner.attempts).remove(path);
                sync_event!(info, "download-complete", path = path);
                let done = inner.completed.fetch_add(1, Ordering::Relaxed) + 1;
                let total = inner.planned.load(Ordering::Relaxed);
                if total > 0 && (done == 1 || done == total || done.is_multiple_of(25)) {
                    tracing::info!("background warm-up: {done}/{total} files downloaded");
                }
            }
            Err(err) if err.is_auth() => {
                self.note_failure(&err);
                tracing::error!("warm-up of {path} blocked by an expired iCloud session: {err}");
                lock(&inner.attempts).remove(path);
            }
            Err(err) => {
                let attempt = {
                    let mut attempts = lock(&inner.attempts);
                    let n = attempts.entry(path.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                let delay = Self::retry_delay(attempt);
                tracing::error!(
                    "warm-up download of {path} failed (attempt {attempt}): {err}; retrying in {}s",
                    delay.as_secs()
                );
                retry = Some(delay);
            }
        }

        lock(&inner.scheduled).remove(path);
        if let Some(delay) = retry {
            self.schedule_download_after(path, delay);
        }
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.mtime()
}

#[cfg(test)]
mod tests {
    use icloud_api::memory::Outage;

    use super::{super::testing::Rig, *};

    fn rig_with_file() -> Rig {
        let rig = Rig::new();
        rig.drive.add_file("/", "f.txt", b"remote bytes", 1_700_000_000);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig
    }

    #[test]
    fn opening_a_placeholder_downloads_it_with_checksum_and_mtime() {
        let rig = rig_with_file();
        rig.engine.ensure_local_file(&Rig::path("/f.txt")).unwrap();

        assert_eq!(rig.local("/f.txt"), b"remote bytes");
        let entry = rig.entry("/f.txt").unwrap();
        assert!(entry.hydrated);
        assert_eq!(entry.size, 12);
        assert_eq!(entry.local_sha256.as_deref().map(str::len), Some(64));
        assert_eq!(
            entry.local_sha256,
            Some(rig.engine.inner.mirror.sha256(&Rig::path("/f.txt")).unwrap()),
            "the checksum computed while streaming must equal a fresh one"
        );
        use std::os::unix::fs::MetadataExt;
        assert_eq!(rig.engine.inner.mirror.stat(&Rig::path("/f.txt")).unwrap().mtime(), 1_700_000_000);
    }

    #[test]
    fn a_second_call_does_not_download_again() {
        let rig = rig_with_file();
        rig.engine.ensure_local_file(&Rig::path("/f.txt")).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/f.txt")).unwrap();
        assert_eq!(rig.drive.calls_matching("open:"), 1);
    }

    #[test]
    fn nothing_is_downloaded_outside_the_boundary() {
        let rig = Rig::with(
            SyncPolicy::new(&["/Allowed"], &[] as &[&str]),
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_file("/", "f.txt", b"x", 1);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/f.txt")).unwrap();
        assert_eq!(rig.drive.calls_matching("open:"), 0);
        assert!(!rig.entry("/f.txt").unwrap().hydrated);
        assert!(
            rig.engine.hydrate(&Rig::path("/f.txt")).is_err(),
            "explicit hydration must say no, not silently do nothing"
        );
        rig.engine.schedule_download(&Rig::path("/f.txt"));
        assert_eq!(rig.engine.stats().downloads_queued, 0);
    }

    #[test]
    fn folders_and_missing_paths_are_ignored() {
        let rig = rig_with_file();
        rig.drive.add_folder("/", "dir");
        rig.engine.list_directory(&IcPath::root(), true).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/dir")).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/nope")).unwrap();
        assert_eq!(rig.drive.calls_matching("open:"), 0);
    }

    #[test]
    fn a_local_only_file_is_adopted_without_the_network() {
        let rig = Rig::new();
        rig.write_local("/mine.txt", b"local");
        rig.engine.inner.state.mark_dirty(&Rig::path("/mine.txt"), None, None, Some(false), false).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/mine.txt")).unwrap();
        assert!(rig.entry("/mine.txt").unwrap().hydrated);
        assert_eq!(rig.drive.calls().len(), 0);
    }

    #[test]
    fn a_failed_download_leaves_the_placeholder_intact() {
        let rig = rig_with_file();
        rig.drive.set_outage(Some(Outage::Offline));
        assert!(rig.engine.ensure_local_file(&Rig::path("/f.txt")).is_err());
        assert!(!rig.entry("/f.txt").unwrap().hydrated);
        assert_eq!(rig.engine.inner.mirror.stat(&Rig::path("/f.txt")).unwrap().len(), 12);
        rig.drive.set_outage(None);
        rig.engine.ensure_local_file(&Rig::path("/f.txt")).unwrap();
        assert_eq!(rig.local("/f.txt"), b"remote bytes");
    }

    /// A drive that deletes the file locally at the moment its bytes are on
    /// the wire, to reproduce a race the real service makes possible.
    struct DeletesWhileDownloading {
        inner: Arc<icloud_api::memory::MemoryDrive>,
        state: Arc<SyncState>,
        victim: IcPath,
    }

    impl Drive for DeletesWhileDownloading {
        fn root(&self) -> icloud_api::Result<Node> {
            self.inner.root()
        }
        fn node(&self, id: &str, share: Option<&serde_json::Value>) -> icloud_api::Result<Node> {
            self.inner.node(id, share)
        }
        fn children(&self, folder: &Node) -> icloud_api::Result<Vec<Node>> {
            self.inner.children(folder)
        }
        fn open(&self, file: &Node) -> icloud_api::Result<Box<dyn std::io::Read + Send>> {
            let reader = self.inner.open(file)?;
            self.state.mark_tombstone(&self.victim).unwrap();
            Ok(reader)
        }
        fn upload(&self, parent: &Node, name: &str, source: std::fs::File, mtime: i64) -> icloud_api::Result<()> {
            self.inner.upload(parent, name, source, mtime)
        }
        fn create_folder(&self, parent: &Node, name: &str) -> icloud_api::Result<()> {
            self.inner.create_folder(parent, name)
        }
        fn delete(&self, node: &Node, mode: icloud_api::DeleteMode) -> icloud_api::Result<()> {
            self.inner.delete(node, mode)
        }
        fn rename(&self, node: &Node, name: &str) -> icloud_api::Result<()> {
            self.inner.rename(node, name)
        }
        fn move_to(&self, node: &Node, destination: &Node) -> icloud_api::Result<()> {
            self.inner.move_to(node, destination)
        }
    }

    #[test]
    fn a_download_that_lost_the_race_with_a_local_delete_is_discarded() {
        let rig = rig_with_file();
        let victim = Rig::path("/f.txt");
        let racy = Arc::new(DeletesWhileDownloading {
            inner: rig.drive.clone(),
            state: rig.engine.inner.state.clone(),
            victim: victim.clone(),
        });
        let engine = Engine::new(
            racy,
            rig.engine.inner.mirror.clone(),
            rig.engine.inner.state.clone(),
            SyncPolicy::unrestricted(),
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );

        engine.ensure_local_file(&victim).unwrap();

        let entry = rig.entry("/f.txt").unwrap();
        assert!(entry.tombstone && !entry.hydrated, "the downloaded bytes must not resurrect a deleted file");
        assert_eq!(rig.engine.inner.mirror.stat(&victim).unwrap().len(), 12, "the placeholder is untouched");
    }

    #[test]
    fn hydrate_confirms_the_file_is_really_local() {
        let rig = rig_with_file();
        rig.engine.hydrate(&Rig::path("/f.txt")).unwrap();
        assert!(rig.entry("/f.txt").unwrap().hydrated);
        rig.engine.hydrate(&Rig::path("/nothing")).unwrap();
    }

    #[test]
    fn retry_delays_double_and_are_capped() {
        let secs: Vec<_> = (1..=9).map(|n| Engine::retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
        assert_eq!(Engine::retry_delay(u32::MAX).as_secs(), 300, "no overflow for absurd attempt counts");
    }

    #[test]
    fn a_failing_background_job_retries_with_backoff_and_a_working_one_finishes() {
        let rig = rig_with_file();
        rig.drive.set_outage(Some(Outage::Offline));
        let path = Rig::path("/f.txt");
        rig.engine.schedule_download(&path);
        assert_eq!(rig.engine.stats().downloads_queued, 1);

        // Run the queued job by hand rather than through a worker thread.
        let job = rig.engine.inner.downloads.pop().unwrap();
        rig.engine.download_job(&job);

        assert_eq!(lock(&rig.engine.inner.attempts).get(&path), Some(&1));
        assert_eq!(rig.engine.stats().downloads_queued, 1, "a retry is queued");
        assert!(lock(&rig.engine.inner.scheduled).contains(&path), "and it is not scheduled twice meanwhile");
        rig.engine.schedule_download(&path);
        assert_eq!(rig.engine.stats().downloads_queued, 1);

        rig.drive.set_outage(None);
        rig.engine.download_job(&path);
        assert!(rig.entry("/f.txt").unwrap().hydrated);
        assert!(lock(&rig.engine.inner.attempts).get(&path).is_none());
        assert_eq!(rig.engine.stats().downloads_completed, 1);
    }

    #[test]
    fn an_expired_session_is_not_retried() {
        let rig = rig_with_file();
        rig.drive.set_outage(Some(Outage::SessionExpired));
        let path = Rig::path("/f.txt");
        rig.engine.schedule_download(&path);
        let job = rig.engine.inner.downloads.pop().unwrap();
        rig.engine.download_job(&job);
        assert_eq!(rig.engine.stats().downloads_queued, 0, "retrying cannot fix an expired session");
        assert!(rig.engine.auth_lost());
    }

    #[test]
    fn scheduling_after_shutdown_does_nothing() {
        let rig = rig_with_file();
        rig.engine.shutdown();
        rig.engine.schedule_download(&Rig::path("/f.txt"));
        assert_eq!(rig.engine.stats().downloads_queued, 0);
    }

    #[test]
    fn warm_up_of_everything_queues_every_unhydrated_file() {
        let rig = Rig::new();
        for name in ["a", "b", "c"] {
            rig.drive.add_file("/", name, b"x", 1);
        }
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/a")).unwrap();
        rig.engine.schedule_all_unhydrated().unwrap();
        assert_eq!(rig.engine.stats().downloads_queued, 2);
        assert_eq!(rig.engine.stats().downloads_planned, 2);
    }
}
