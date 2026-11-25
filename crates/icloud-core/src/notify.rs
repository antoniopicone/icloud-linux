//! Telling a person why the file they opened did not open.
//!
//! A file that is not local yet is downloaded while the application that
//! opened it waits. When the download fails, all the application learns is
//! "input/output error", and many viewers show that as a spinner that never
//! stops. A desktop notification says what actually happened and what to do.
//! When the iCloud session has expired, clicking the notification opens the
//! sign-in on icloud.com (`icloudctl auth`), which restarts the service with
//! the new session once it is done.

use std::{
    ffi::OsString,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, PoisonError},
    thread,
    time::{Duration, Instant},
};

use crate::{
    i18n::{tr, trf},
    path::IcPath,
};

/// At most one notification in this long: an application that retries its
/// read, or a copy of a whole folder, must not bury the desktop in them.
const QUIET_PERIOD: Duration = Duration::from_secs(30);

/// The systemd unit the sign-in runs as. A second click while it runs finds
/// the name taken and starts nothing.
const SIGN_IN_UNIT: &str = "icloud-sign-in";

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub title: String,
    pub body: String,
    /// What clicking the notification (or its button) does.
    pub action: Option<NoteAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteAction {
    /// The button's label.
    pub label: String,
    /// The program to run, then its arguments.
    pub command: Vec<OsString>,
}

/// Where notifications go.
pub trait Notifier: Send + Sync {
    fn notify(&self, note: &Note);
}

/// `notify-send`, run in the background so a filesystem request never waits
/// for it. Nothing happens where it is not installed.
#[derive(Debug)]
pub struct DesktopNotifier {
    program: PathBuf,
}

impl Default for DesktopNotifier {
    fn default() -> Self {
        Self { program: PathBuf::from("notify-send") }
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, note: &Note) {
        let mut send = Command::new(&self.program);
        send.args(["--app-name=iCloud", "--icon=dialog-warning-symbolic"]);
        if let Some(action) = &note.action {
            // `default` is a click on the notification itself; the named
            // action is the button. notify-send prints the one chosen.
            send.arg("--action=default=".to_owned() + &action.label);
            send.arg("--action=run=".to_owned() + &action.label);
        }
        let child = send
            .args(["--", &note.title, &note.body])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => return tracing::debug!("could not run notify-send: {err}"),
        };
        let action = note.action.clone();
        let _ = thread::Builder::new().name("icloud-notify".into()).spawn(move || {
            let mut chosen = String::new();
            if let Some(stdout) = child.stdout.as_mut() {
                let _ = stdout.read_to_string(&mut chosen);
            }
            let _ = child.wait();
            if let Some(action) = action
                && matches!(chosen.trim(), "default" | "run")
            {
                run_detached(&action.command);
            }
        });
    }
}

fn run_detached(command: &[OsString]) {
    let Some((program, args)) = command.split_first() else { return };
    tracing::info!("starting {}", command.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
    match Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            let _ = thread::Builder::new().name("icloud-notify-action".into()).spawn(move || child.wait());
        }
        Err(err) => tracing::warn!("could not start {}: {err}", program.to_string_lossy()),
    }
}

/// `icloudctl auth --notify`: sign in on icloud.com, restart the service,
/// and report the outcome as a notification.
///
/// It runs as a transient systemd unit when `systemd-run` is there, so that
/// restarting this service at the end does not stop the sign-in with it.
pub fn sign_in_command() -> Vec<OsString> {
    sign_in_command_with(&program_next_to_us("icloudctl"), find_on_path("systemd-run").is_some())
}

fn sign_in_command_with(icloudctl: &Path, systemd_run: bool) -> Vec<OsString> {
    let mut command: Vec<OsString> = Vec::new();
    if systemd_run {
        command.extend(
            ["systemd-run", "--user", "--quiet", "--collect", &format!("--unit={SIGN_IN_UNIT}")].map(OsString::from),
        );
    }
    command.push(icloudctl.into());
    command.extend(["auth", "--notify"].map(OsString::from));
    command
}

/// `name` from the directory this program was started from, where the
/// installer puts all of them; otherwise whatever `PATH` finds.
fn program_next_to_us(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

/// Why a file could not be downloaded for the person who opened it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFailure {
    /// Only a new sign-in helps.
    SignInNeeded,
    /// iCloud did not answer or answered with an error; trying again may work.
    Unreachable,
}

impl OpenFailure {
    fn note(self, name: &str) -> Note {
        let title = trf("Could not open “{}”", &[&name]);
        match self {
            Self::SignInNeeded => Note {
                title,
                body: tr("The iCloud session has expired, so the file cannot be downloaded. Click to sign in again on icloud.com.")
                    .to_owned(),
                action: Some(NoteAction { label: tr("Sign in on icloud.com").to_owned(), command: sign_in_command() }),
            },
            Self::Unreachable => Note {
                title,
                body: tr("iCloud could not be reached. Check the connection, then open the file again.").to_owned(),
                action: None,
            },
        }
    }
}

/// Sends the notifications, no more often than [`QUIET_PERIOD`].
pub struct FailureReporter {
    notifier: Box<dyn Notifier>,
    last: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for FailureReporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailureReporter").finish_non_exhaustive()
    }
}

impl FailureReporter {
    pub fn new(notifier: impl Notifier + 'static) -> Self {
        Self { notifier: Box::new(notifier), last: Mutex::new(None) }
    }

    /// Tell the person that `path` could not be downloaded, unless they were
    /// told about a failure a moment ago.
    pub fn report(&self, path: &IcPath, failure: OpenFailure) {
        self.report_at(path, failure, Instant::now());
    }

    fn report_at(&self, path: &IcPath, failure: OpenFailure, now: Instant) {
        {
            let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
            if last.is_some_and(|at| now.saturating_duration_since(at) < QUIET_PERIOD) {
                return;
            }
            *last = Some(now);
        }
        self.notifier.notify(&failure.note(path.file_name().unwrap_or("iCloud")));
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, sync::Arc};

    use super::*;

    #[derive(Default, Clone)]
    struct Recorder(Arc<Mutex<Vec<Note>>>);

    impl Notifier for Recorder {
        fn notify(&self, note: &Note) {
            self.0.lock().unwrap().push(note.clone());
        }
    }

    fn path(text: &str) -> IcPath {
        IcPath::new(text)
    }

    #[test]
    fn an_expired_session_is_reported_with_a_way_to_sign_in_again() {
        let sent = Recorder::default();
        FailureReporter::new(sent.clone()).report(&path("/Docs/LLM.pdf"), OpenFailure::SignInNeeded);
        let sent = sent.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].title, "Could not open “LLM.pdf”");
        assert!(sent[0].body.contains("sign in again"), "{}", sent[0].body);
        let action = sent[0].action.as_ref().expect("a click signs in again");
        assert_eq!(action.label, "Sign in on icloud.com");
        assert!(action.command.ends_with(&["auth".into(), "--notify".into()]), "{:?}", action.command);
    }

    #[test]
    fn a_network_failure_offers_nothing_to_click() {
        let sent = Recorder::default();
        FailureReporter::new(sent.clone()).report(&path("/a.pdf"), OpenFailure::Unreachable);
        assert!(sent.0.lock().unwrap()[0].action.is_none());
    }

    #[test]
    fn the_sign_in_runs_outside_the_service_when_systemd_run_is_there() {
        let with = sign_in_command_with(Path::new("/opt/bin/icloudctl"), true);
        assert_eq!(
            with,
            [
                "systemd-run",
                "--user",
                "--quiet",
                "--collect",
                "--unit=icloud-sign-in",
                "/opt/bin/icloudctl",
                "auth",
                "--notify"
            ]
            .map(OsString::from)
        );
        let without = sign_in_command_with(Path::new("icloudctl"), false);
        assert_eq!(without, ["icloudctl", "auth", "--notify"].map(OsString::from));
    }

    #[test]
    fn clicking_the_notification_runs_its_action() {
        let dir = tempfile::tempdir().unwrap();
        // A notify-send whose user clicks at once.
        let fake = dir.path().join("notify-send");
        fs_write_executable(&fake, "#!/bin/sh\necho default\n");
        let clicked = dir.path().join("clicked");
        let note = Note {
            title: "t".into(),
            body: "b".into(),
            action: Some(NoteAction {
                label: "Sign in".into(),
                command: ["touch".into(), clicked.clone().into_os_string()].to_vec(),
            }),
        };
        DesktopNotifier { program: fake }.notify(&note);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !clicked.exists() {
            assert!(Instant::now() < deadline, "the action never ran");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_notification_that_is_dismissed_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("notify-send");
        fs_write_executable(&fake, "#!/bin/sh\nexit 0\n");
        let clicked = dir.path().join("clicked");
        let note = Note {
            title: "t".into(),
            body: "b".into(),
            action: Some(NoteAction { label: "x".into(), command: vec!["touch".into(), clicked.clone().into()] }),
        };
        DesktopNotifier { program: fake }.notify(&note);
        thread::sleep(Duration::from_millis(300));
        assert!(!clicked.exists());
    }

    fn fs_write_executable(path: &Path, script: &str) {
        std::fs::write(path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn repeated_failures_are_reported_once_per_quiet_period() {
        let sent = Recorder::default();
        let reporter = FailureReporter::new(sent.clone());
        let t0 = Instant::now();
        reporter.report_at(&path("/a.pdf"), OpenFailure::Unreachable, t0);
        reporter.report_at(&path("/a.pdf"), OpenFailure::Unreachable, t0 + Duration::from_secs(1));
        reporter.report_at(&path("/b.pdf"), OpenFailure::Unreachable, t0 + Duration::from_secs(5));
        assert_eq!(sent.0.lock().unwrap().len(), 1);
        reporter.report_at(&path("/a.pdf"), OpenFailure::Unreachable, t0 + QUIET_PERIOD);
        assert_eq!(sent.0.lock().unwrap().len(), 2);
    }
}
