//! Keeping the web session alive while Drive is in use.
//!
//! iCloud's web session lapses unless it is renewed now and then, which is
//! what icloud.com does from an open tab. Once it has lapsed, every Drive
//! request fails with "Invalid global session" until the session is renewed.
//! [`RenewingDrive`] does that once, then repeats the request, so a file that
//! is opened after a quiet hour still downloads instead of failing.
//!
//! Renewal never sends a password (see [`Client::renew_session`]). When Apple
//! refuses it, the refusal is remembered for a minute so that a burst of
//! requests does not turn into a burst of renewals.
//!
//! [`Client::renew_session`]: crate::Client::renew_session

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    drive::{DeleteMode, Drive, Node},
    error::{Error, Result},
};

/// How long a refused renewal is believed before it is tried again.
const REFUSAL_MEMORY: Duration = Duration::from_secs(60);

type Renew = dyn Fn() -> Result<()> + Send + Sync;
type DiskCheck = dyn Fn() -> bool + Send + Sync;

/// A [`Drive`] that renews the session when iCloud stops accepting it.
pub struct RenewingDrive<D> {
    inner: D,
    renew: Box<Renew>,
    /// Optional probe that returns `true` when a fresher session appeared on
    /// disk (bypassing the refusal cooldown).
    disk_changed: Option<Box<DiskCheck>>,
    /// Counts successful renewals. Stored atomically so ordinary Drive requests
    /// never block behind an in-flight keep-alive renewal.
    generation: AtomicU64,
    state: Mutex<RenewState>,
}

#[derive(Debug, Default)]
struct RenewState {
    /// The last refusal, while it is fresh.
    refused: Option<(Instant, String)>,
}

impl<D> std::fmt::Debug for RenewingDrive<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenewingDrive").finish_non_exhaustive()
    }
}

impl<D: Drive> RenewingDrive<D> {
    /// `renew` renews the session `inner` sends its requests with.
    pub fn new(inner: D, renew: impl Fn() -> Result<()> + Send + Sync + 'static) -> Self {
        Self {
            inner,
            renew: Box::new(renew),
            disk_changed: None,
            generation: AtomicU64::new(0),
            state: Mutex::new(RenewState::default()),
        }
    }

    /// Supply a probe that returns `true` when a newer session was saved on
    /// disk, so a fresh `icloudctl auth` bypasses the refusal cooldown at once.
    #[must_use]
    pub fn with_disk_probe(mut self, probe: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.disk_changed = Some(Box::new(probe));
        self
    }

    fn state(&self) -> MutexGuard<'_, RenewState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn disk_has_newer_session(&self) -> bool {
        self.disk_changed.as_deref().is_some_and(|probe| probe())
    }

    /// Renew the session now, for a keep-alive timer.
    pub fn renew_now(&self) -> Result<()> {
        let generation = self.generation.load(Ordering::Acquire);
        self.renew_unless_done_since(generation)
    }

    /// Renew the session unless that already happened since `generation`.
    ///
    /// The lock is held throughout, so requests that failed together wait for
    /// one renewal and then all go again.
    fn renew_unless_done_since(&self, generation: u64) -> Result<()> {
        let mut state = self.state();
        if self.generation.load(Ordering::Acquire) != generation {
            return Ok(());
        }
        if let Some((at, why)) = &state.refused
            && at.elapsed() < REFUSAL_MEMORY
            && !self.disk_has_newer_session()
        {
            return Err(Error::AuthRequired(why.clone()));
        }
        match (self.renew)() {
            Ok(()) => {
                self.generation.fetch_add(1, Ordering::Release);
                state.refused = None;
                tracing::debug!("renewed the iCloud session");
                Ok(())
            }
            Err(err) => {
                if err.is_auth() {
                    state.refused = Some((Instant::now(), err.to_string()));
                }
                Err(err)
            }
        }
    }

    /// Run `op`; if iCloud returns an expired-session error (or an HTTP
    /// 401/421/450 that may mean the session lapsed), renew the session and
    /// run `op` once more.
    fn with_renewal<T>(&self, op: impl Fn(&D) -> Result<T>) -> Result<T> {
        let generation = self.generation.load(Ordering::Acquire);
        match op(&self.inner) {
            Err(err) if err.may_be_session_expired() => {
                tracing::info!("iCloud rejected a request ({err}); renewing the session");
                self.renew_unless_done_since(generation)?;
                tracing::info!("the iCloud session is valid again");
                op(&self.inner)
            }
            other => other,
        }
    }
}

impl<D: Drive> Drive for RenewingDrive<D> {
    fn root(&self) -> Result<Node> {
        self.with_renewal(Drive::root)
    }

    fn node(&self, drivewsid: &str, share_id: Option<&Value>) -> Result<Node> {
        self.with_renewal(|d| d.node(drivewsid, share_id))
    }

    fn children(&self, folder: &Node) -> Result<Vec<Node>> {
        self.with_renewal(|d| d.children(folder))
    }

    fn open(&self, file: &Node) -> Result<Box<dyn Read + Send>> {
        self.with_renewal(|d| d.open(file))
    }

    fn upload(&self, parent: &Node, name: &str, source: File, mtime: i64) -> Result<()> {
        // Each attempt gets the file from its start: a failed one may have read some of it.
        self.with_renewal(|d| {
            let mut file = source.try_clone()?;
            file.seek(SeekFrom::Start(0))?;
            d.upload(parent, name, file, mtime)
        })
    }

    fn create_folder(&self, parent: &Node, name: &str) -> Result<()> {
        self.with_renewal(|d| d.create_folder(parent, name))
    }

    fn delete(&self, node: &Node, mode: DeleteMode) -> Result<()> {
        self.with_renewal(|d| d.delete(node, mode))
    }

    fn rename(&self, node: &Node, new_name: &str) -> Result<()> {
        self.with_renewal(|d| d.rename(node, new_name))
    }

    fn move_to(&self, node: &Node, destination: &Node) -> Result<()> {
        self.with_renewal(|d| d.move_to(node, destination))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write as _,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::*;
    use crate::memory::{MemoryDrive, Outage};

    /// A drive whose session has lapsed, and a renewal that fixes it (or not).
    fn lapsed(renewal_works: bool) -> (Arc<MemoryDrive>, Arc<AtomicUsize>, RenewingDrive<Arc<MemoryDrive>>) {
        let drive = Arc::new(MemoryDrive::new());
        drive.add_file("/", "a.txt", b"hello", 1);
        drive.set_outage(Some(Outage::SessionExpired));
        let renewals = Arc::new(AtomicUsize::new(0));
        let (d, n) = (drive.clone(), renewals.clone());
        let renewing = RenewingDrive::new(drive.clone(), move || {
            n.fetch_add(1, Ordering::SeqCst);
            if renewal_works {
                d.set_outage(None);
                Ok(())
            } else {
                Err(Error::AuthRequired("Missing X-APPLE-WEBAUTH-TOKEN cookie".into()))
            }
        });
        (drive, renewals, renewing)
    }

    #[test]
    fn a_lapsed_session_is_renewed_once_and_the_request_repeated() {
        let (_, renewals, drive) = lapsed(true);
        let root = drive.root().unwrap();
        assert_eq!(drive.children(&root).unwrap().len(), 1);
        assert_eq!(renewals.load(Ordering::SeqCst), 1, "a working session is not renewed again");
    }

    #[test]
    fn a_refused_renewal_is_an_auth_error_and_is_not_retried_at_once() {
        let (_, renewals, drive) = lapsed(false);
        assert!(drive.root().unwrap_err().is_auth());
        assert!(drive.root().unwrap_err().is_auth());
        assert!(drive.renew_now().unwrap_err().is_auth());
        assert_eq!(renewals.load(Ordering::SeqCst), 1, "Apple is asked once a minute at most");
    }

    #[test]
    fn other_failures_pass_through_without_renewing() {
        let (memory, renewals, drive) = lapsed(true);
        memory.set_outage(Some(Outage::Offline));
        assert!(!drive.root().unwrap_err().is_auth());
        assert_eq!(renewals.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn requests_that_fail_together_share_one_renewal() {
        let (_, renewals, drive) = lapsed(true);
        let drive = Arc::new(drive);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let drive = drive.clone();
                thread::spawn(move || drive.root().map(drop))
            })
            .collect();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_upload_is_repeated_from_the_start_of_the_file() {
        let (memory, _, drive) = lapsed(true);
        let mut source = tempfile::tempfile().unwrap();
        source.write_all(b"new contents").unwrap();
        let root = Node::root();
        drive.upload(&root, "b.txt", source, 5).unwrap();
        assert_eq!(memory.contents("/b.txt").as_deref(), Some(&b"new contents"[..]));
    }

    #[test]
    fn a_401_or_421_from_drive_triggers_renewal_and_retries() {
        struct FlakyAuthDrive {
            attempts: AtomicUsize,
        }
        impl Drive for FlakyAuthDrive {
            fn root(&self) -> Result<Node> {
                if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::Status { status: 401, reason: "Unauthorized".into() })
                } else {
                    Ok(Node::root())
                }
            }
            fn node(&self, _: &str, _: Option<&Value>) -> Result<Node> {
                unimplemented!()
            }
            fn children(&self, _: &Node) -> Result<Vec<Node>> {
                unimplemented!()
            }
            fn open(&self, _: &Node) -> Result<Box<dyn Read + Send>> {
                unimplemented!()
            }
            fn upload(&self, _: &Node, _: &str, _: File, _: i64) -> Result<()> {
                unimplemented!()
            }
            fn create_folder(&self, _: &Node, _: &str) -> Result<()> {
                unimplemented!()
            }
            fn delete(&self, _: &Node, _: DeleteMode) -> Result<()> {
                unimplemented!()
            }
            fn rename(&self, _: &Node, _: &str) -> Result<()> {
                unimplemented!()
            }
            fn move_to(&self, _: &Node, _: &Node) -> Result<()> {
                unimplemented!()
            }
        }
        let renewals = Arc::new(AtomicUsize::new(0));
        let n = renewals.clone();
        let drive = RenewingDrive::new(FlakyAuthDrive { attempts: AtomicUsize::new(0) }, move || {
            n.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(drive.root().is_ok());
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_newer_session_on_disk_bypasses_the_refusal_cooldown() {
        use std::sync::atomic::AtomicBool;

        let drive = Arc::new(MemoryDrive::new());
        drive.set_outage(Some(Outage::SessionExpired));
        let disk_updated = Arc::new(AtomicBool::new(false));
        let renewals = Arc::new(AtomicUsize::new(0));
        let (d, flag, n) = (drive.clone(), disk_updated.clone(), renewals.clone());
        let probe = disk_updated.clone();
        let renewing = RenewingDrive::new(drive.clone(), move || {
            n.fetch_add(1, Ordering::SeqCst);
            if flag.load(Ordering::SeqCst) {
                d.set_outage(None);
                Ok(())
            } else {
                Err(Error::AuthRequired("expired".into()))
            }
        })
        .with_disk_probe(move || probe.load(Ordering::SeqCst));

        assert!(renewing.root().unwrap_err().is_auth());
        assert!(renewing.renew_now().unwrap_err().is_auth());
        assert_eq!(renewals.load(Ordering::SeqCst), 1);

        // Fresh sign-in written to disk: cooldown is bypassed immediately.
        disk_updated.store(true, Ordering::SeqCst);
        renewing.renew_now().unwrap();
        assert_eq!(renewals.load(Ordering::SeqCst), 2);
        assert!(renewing.root().is_ok());
    }
}
