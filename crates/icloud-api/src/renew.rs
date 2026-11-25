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
    sync::{Mutex, MutexGuard, PoisonError},
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

/// A [`Drive`] that renews the session when iCloud stops accepting it.
pub struct RenewingDrive<D> {
    inner: D,
    renew: Box<Renew>,
    state: Mutex<RenewState>,
}

#[derive(Debug, Default)]
struct RenewState {
    /// Counts successful renewals, so that requests which failed on the old
    /// session do not each renew it again.
    generation: u64,
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
        Self { inner, renew: Box::new(renew), state: Mutex::new(RenewState::default()) }
    }

    fn state(&self) -> MutexGuard<'_, RenewState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Renew the session now, for a keep-alive timer.
    pub fn renew_now(&self) -> Result<()> {
        let generation = self.state().generation;
        self.renew_unless_done_since(generation)
    }

    /// Renew the session unless that already happened since `generation`.
    ///
    /// The lock is held throughout, so requests that failed together wait for
    /// one renewal and then all go again.
    fn renew_unless_done_since(&self, generation: u64) -> Result<()> {
        let mut state = self.state();
        if state.generation != generation {
            return Ok(());
        }
        if let Some((at, why)) = &state.refused
            && at.elapsed() < REFUSAL_MEMORY
        {
            return Err(Error::AuthRequired(why.clone()));
        }
        match (self.renew)() {
            Ok(()) => {
                state.generation += 1;
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

    /// Run `op`; if iCloud no longer accepts the session, renew it and run
    /// `op` once more.
    fn with_renewal<T>(&self, op: impl Fn(&D) -> Result<T>) -> Result<T> {
        let generation = self.state().generation;
        match op(&self.inner) {
            Err(err @ Error::AuthRequired(_)) => {
                tracing::info!("iCloud no longer accepts the session ({err}); renewing it");
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
}
