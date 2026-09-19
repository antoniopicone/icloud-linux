//! `icloudctl hydrate`: download every eligible file that is not local yet.
//!
//! Files are read through the mount, so the daemon's own download machinery
//! does the work. With `crawl_mode: lazy` only folders that have been opened
//! are known, so only their files can be hydrated; walk a tree first (for
//! example `find DIR -type d`) to make it known.

use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crate::{config::Config, error::Result, path::IcPath, policy::SyncPolicy, state::SyncState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Eligible files still to download, with their sizes.
    pub pending: Vec<(IcPath, u64)>,
    /// Eligible files that are already local.
    pub already_local: u64,
}

impl Plan {
    pub fn eligible(&self) -> u64 {
        self.pending.len() as u64 + self.already_local
    }

    pub fn pending_bytes(&self) -> u64 {
        self.pending.iter().map(|(_, size)| size).sum()
    }
}

/// Work out what there is to download, honouring `sync_paths` and `exclude_paths`.
pub fn plan(config: &Config) -> Result<Plan> {
    let policy = SyncPolicy::new(&config.sync_paths, &config.exclude_paths);
    let state = SyncState::open(&config.state_db())?;
    let (mut pending, mut already_local) = (Vec::new(), 0u64);
    for entry in state.list_entries()? {
        if entry.kind != icloud_api::NodeKind::File || entry.tombstone || !policy.allows(&entry.path) {
            continue;
        }
        if entry.hydrated {
            already_local += 1;
        } else {
            pending.push((entry.path, entry.size));
        }
    }
    Ok(Plan { pending, already_local })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub failed: usize,
    pub total: usize,
    pub elapsed: Duration,
}

impl Progress {
    /// Estimated time left, from the rate so far.
    pub fn eta(&self) -> Option<Duration> {
        let finished = self.done.checked_sub(self.failed).filter(|n| *n > 0)?;
        let rate = finished as f64 / self.elapsed.as_secs_f64().max(0.001);
        Some(Duration::from_secs_f64((self.total.saturating_sub(self.done)) as f64 / rate))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub succeeded: usize,
    pub failed: Vec<(IcPath, String)>,
    pub interrupted: bool,
}

/// Download `plan.pending` by reading each file's first byte through `mount`.
/// `progress` is called after every file; raising `cancel` stops cleanly.
pub fn run(mount: &Path, plan: &Plan, cancel: &AtomicBool, mut progress: impl FnMut(&IcPath, Progress)) -> Report {
    let started = Instant::now();
    let mut report = Report { succeeded: 0, failed: Vec::new(), interrupted: false };
    for (index, (path, _)) in plan.pending.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            report.interrupted = true;
            break;
        }
        match touch(mount, path) {
            Ok(()) => report.succeeded += 1,
            Err(err) => report.failed.push((path.clone(), err.to_string())),
        }
        progress(
            path,
            Progress {
                done: index + 1,
                failed: report.failed.len(),
                total: plan.pending.len(),
                elapsed: started.elapsed(),
            },
        );
    }
    report
}

/// Opening and reading a byte makes the daemon fetch the whole file.
fn touch(mount: &Path, path: &IcPath) -> std::io::Result<()> {
    let mut file = File::open(mount.join(path.as_str().trim_start_matches('/')))?;
    let mut one = [0u8; 1];
    file.read(&mut one).map(drop)
}

#[cfg(test)]
mod tests {
    use icloud_api::NodeKind;

    use super::*;
    use crate::{dirs::Layout, state::Entry};

    fn config_with_entries() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::for_layout(&Layout::under(dir.path()));
        config.sync_paths = vec!["/Wanted".into()];
        config.exclude_paths = vec!["/Wanted/Skip".into()];
        let state = SyncState::open(&config.state_db()).unwrap();
        let add = |path: &str, kind: NodeKind, size: u64, hydrated: bool, tombstone: bool| {
            let mut e = Entry::local(IcPath::new(path), kind, 1);
            e.size = size;
            e.hydrated = hydrated;
            e.tombstone = tombstone;
            e.dirty = false;
            state.upsert_entry(&e).unwrap();
        };
        add("/Wanted", NodeKind::Folder, 0, true, false);
        add("/Wanted/a.bin", NodeKind::File, 100, false, false);
        add("/Wanted/b.bin", NodeKind::File, 50, true, false);
        add("/Wanted/gone.bin", NodeKind::File, 9, false, true);
        add("/Wanted/Skip/c.bin", NodeKind::File, 7, false, false);
        add("/Other/d.bin", NodeKind::File, 7, false, false);
        (dir, config)
    }

    #[test]
    fn the_plan_honours_the_boundary_and_skips_folders_and_deleted_files() {
        let (_d, config) = config_with_entries();
        let plan = plan(&config).unwrap();
        assert_eq!(plan.pending, [(IcPath::new("/Wanted/a.bin"), 100)]);
        assert_eq!(plan.already_local, 1);
        assert_eq!((plan.eligible(), plan.pending_bytes()), (2, 100));
    }

    #[test]
    fn running_reads_each_file_through_the_mount_and_reports_failures() {
        let mount = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(mount.path().join("d")).unwrap();
        std::fs::write(mount.path().join("d/ok.txt"), b"content").unwrap();
        let plan =
            Plan { pending: vec![(IcPath::new("/d/ok.txt"), 7), (IcPath::new("/d/missing.txt"), 1)], already_local: 0 };
        let mut seen = Vec::new();
        let report =
            run(mount.path(), &plan, &AtomicBool::new(false), |path, p| seen.push((path.clone(), p.done, p.failed)));
        assert_eq!(report.succeeded, 1);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, IcPath::new("/d/missing.txt"));
        assert_eq!(seen.last().map(|s| (s.1, s.2)), Some((2, 1)));
        assert!(!report.interrupted);
    }

    #[test]
    fn cancelling_stops_before_the_next_file() {
        let mount = tempfile::tempdir().unwrap();
        let plan = Plan { pending: vec![(IcPath::new("/a"), 1), (IcPath::new("/b"), 1)], already_local: 0 };
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let report = run(mount.path(), &plan, &cancel, |_, _| {
            calls += 1;
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(calls, 1);
        assert!(report.interrupted);
    }

    #[test]
    fn the_eta_follows_the_rate_and_is_unknown_before_any_success() {
        let p = Progress { done: 10, failed: 0, total: 30, elapsed: Duration::from_secs(20) };
        assert_eq!(p.eta().map(|d| d.as_secs()), Some(40));
        assert_eq!(Progress { done: 3, failed: 3, total: 9, elapsed: Duration::from_secs(1) }.eta(), None);
    }
}
