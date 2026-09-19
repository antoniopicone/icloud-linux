//! `icloudctl sync`: ask the running daemon to refresh from iCloud now.
//!
//! The daemon does the work on `SIGUSR1` and touches a marker file when it is
//! done; this side sends the signal and waits for the marker.

use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant, SystemTime},
};

use rustix::process::{Pid, Signal, kill_process};

use crate::{
    dirs::Layout,
    error::{Error, Result},
    setup::{Systemctl, daemon_pid},
};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(900);
const POLL: Duration = Duration::from_millis(500);

/// Signal the daemon and wait until it reports completion. Returns how long it took.
pub fn request_sync(layout: &Layout, sys: &dyn Systemctl, timeout: Duration) -> Result<Duration> {
    let pid = daemon_pid(sys)
        .and_then(Pid::from_raw)
        .ok_or_else(|| Error::Setup("icloud.service is not running; start it with `icloudctl start`".into()))?;
    let marker = layout.sync_marker();
    match fs::remove_file(&marker) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let started = Instant::now();
    let since = SystemTime::now();
    kill_process(pid, Signal::USR1).map_err(|e| Error::Setup(format!("cannot signal the daemon: {e}")))?;
    if wait_for_marker(&marker, since, timeout) {
        outcome_of(&fs::read_to_string(&marker).unwrap_or_default())?;
        Ok(started.elapsed())
    } else {
        Err(Error::Setup(format!("the sync did not finish within {}s; see `icloudctl logs`", timeout.as_secs())))
    }
}

/// Signal the daemon without waiting for it to finish.
pub fn signal_daemon(sys: &dyn Systemctl) -> Result<()> {
    let pid = daemon_pid(sys)
        .and_then(Pid::from_raw)
        .ok_or_else(|| Error::Setup("icloud.service is not running; start it with `icloudctl start`".into()))?;
    kill_process(pid, Signal::USR1).map_err(|e| Error::Setup(format!("cannot signal the daemon: {e}")))
}

/// What the daemon reported in the marker: `ok`, or `error: <why>`. Older
/// daemons wrote a bare timestamp, which counts as success.
pub fn outcome_of(marker_text: &str) -> Result<()> {
    match marker_text.trim().strip_prefix("error: ") {
        Some(reason) => Err(Error::Setup(format!("the daemon could not sync: {reason}"))),
        None => Ok(()),
    }
}

/// Wait for `marker` to exist with a modification time at or after `since`.
pub fn wait_for_marker(marker: &Path, since: SystemTime, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if fs::metadata(marker).and_then(|m| m.modified()).is_ok_and(|modified| modified >= since) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::testing::FakeSystemctl;

    #[test]
    fn a_marker_written_after_the_request_counts() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("sync_done");
        let since = SystemTime::now();
        let writer = {
            let marker = marker.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                fs::write(marker, b"1").unwrap();
            })
        };
        assert!(wait_for_marker(&marker, since, Duration::from_secs(5)));
        writer.join().unwrap();
    }

    #[test]
    fn a_stale_marker_from_before_the_request_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("sync_done");
        fs::write(&marker, b"old").unwrap();
        let since = SystemTime::now() + Duration::from_secs(60);
        assert!(!wait_for_marker(&marker, since, Duration::from_millis(80)));
    }

    #[test]
    fn the_daemons_verdict_is_read_from_the_marker() {
        assert!(outcome_of("ok").is_ok());
        assert!(outcome_of("1699999999.5").is_ok(), "the timestamp the Python daemon wrote");
        assert!(outcome_of("").is_ok());
        let err = outcome_of("error: not signed in to iCloud\n").unwrap_err();
        assert!(err.to_string().contains("not signed in"));
    }

    #[test]
    fn a_missing_marker_times_out() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!wait_for_marker(&dir.path().join("nope"), SystemTime::now(), Duration::from_millis(50)));
    }

    #[test]
    fn requesting_a_sync_from_a_stopped_service_says_so_and_signals_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::under(dir.path());
        let sys = FakeSystemctl::default();
        *sys.main_pid.lock().unwrap() = "0".into();
        let err = request_sync(&layout, &sys, Duration::from_millis(10)).unwrap_err();
        assert!(err.to_string().contains("not running"));
    }
}
