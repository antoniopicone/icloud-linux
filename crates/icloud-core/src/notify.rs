//! Telling a person why the file they opened did not open.
//!
//! A file that is not local yet is downloaded while the application that
//! opened it waits. When the download fails, all the application learns is
//! "input/output error", and many viewers show that as a spinner that never
//! stops. A desktop notification says what actually happened and what to do.
//! When the iCloud session has expired, clicking the notification opens the
//! sign-in on icloud.com, which restarts the service with the new session.
//!
//! Notifications go straight to `org.freedesktop.Notifications` on the session
//! bus, not through `notify-send`: Ubuntu confines `notify-send` with an
//! `AppArmor` profile that lets it show a notification but never hear that it
//! was clicked. The click is the `default` action, which every notification
//! server invokes on a click on the notification itself (mako, quickshell,
//! GNOME Shell, dunst…); servers that draw buttons get one as well.
//!
//! The offer to sign in again waits for that click in a process of its own
//! (`icloudctl sign-in-offer`), run as a transient systemd unit: restarting the
//! service, which the sign-in ends with, must not take the offer with it.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

use zbus::{
    blocking::{Connection, MessageIterator, Proxy},
    message::Type as MessageType,
    zvariant::Value,
};

use crate::{
    i18n::{tr, trf},
    path::IcPath,
};

/// At most one notification in this long: an application that retries its
/// read, or a copy of a whole folder, must not bury the desktop in them.
const QUIET_PERIOD: Duration = Duration::from_secs(30);

/// The systemd unit the offer to sign in runs as. While one waits for a click,
/// another finds the name taken and does not start.
const SIGN_IN_UNIT: &str = "icloud-sign-in";

/// How long an offer to sign in waits for a click before it goes away.
pub const SIGN_IN_OFFER_WAIT: Duration = Duration::from_secs(60 * 60);

const DESTINATION: &str = "org.freedesktop.Notifications";
const OBJECT: &str = "/org/freedesktop/Notifications";
const INTERFACE: &str = "org.freedesktop.Notifications";
/// The action a click on the notification itself invokes.
pub const DEFAULT_ACTION: &str = "default";

// ---- the session bus ----------------------------------------------------------------------

/// How a notification that asked for a click ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The person clicked it (or one of its buttons): the action's key.
    Clicked(String),
    /// It was dismissed, or the server closed it.
    Closed,
    /// Nobody did anything in time; it was taken away.
    TimedOut,
}

/// The desktop's notification server.
#[derive(Debug, Clone)]
pub struct Desktop {
    conn: Connection,
}

impl Desktop {
    /// The server on the session bus.
    pub fn session() -> zbus::Result<Self> {
        Ok(Self { conn: Connection::session()? })
    }

    /// The server at the other end of `conn`, for tests.
    pub fn on(conn: Connection) -> Self {
        Self { conn }
    }

    fn proxy(&self) -> zbus::Result<Proxy<'_>> {
        Proxy::new(&self.conn, DESTINATION, OBJECT, INTERFACE)
    }

    fn send(&self, title: &str, body: &str, actions: &[(&str, &str)], expire_ms: i32) -> zbus::Result<u32> {
        let actions: Vec<&str> = actions.iter().flat_map(|(key, label)| [*key, *label]).collect();
        let hints: HashMap<&str, Value<'_>> = HashMap::new();
        let icon = notification_icon();
        self.proxy()?.call("Notify", &("iCloud", 0u32, icon.as_str(), title, body, actions, hints, expire_ms))
    }

    /// Show a notification and leave it to the server.
    pub fn show(&self, title: &str, body: &str) -> zbus::Result<()> {
        self.send(title, body, &[], -1).map(drop)
    }

    /// Show a notification with `actions` (key, label) and wait until it is
    /// clicked or closed, for at most `wait`; then it is taken away.
    pub fn ask(&self, title: &str, body: &str, actions: &[(&str, &str)], wait: Duration) -> zbus::Result<Outcome> {
        // Listen before showing it, so that not even a quick click is missed.
        let rule = zbus::MatchRule::builder().msg_type(MessageType::Signal).interface(INTERFACE)?.path(OBJECT)?.build();
        let signals = MessageIterator::for_match_rule(rule, &self.conn, None)?;
        // Expire never: the offer stays until it is answered or `wait` is over.
        let id = self.send(title, body, actions, 0)?;

        let (tx, rx) = mpsc::channel();
        thread::Builder::new().name("icloud-notification".into()).spawn(move || {
            for message in signals.flatten() {
                let header = message.header();
                let outcome = match header.member().map(zbus::names::MemberName::as_str) {
                    Some("ActionInvoked") => message
                        .body()
                        .deserialize::<(u32, String)>()
                        .ok()
                        .filter(|(of, _)| *of == id)
                        .map(|(_, key)| Outcome::Clicked(key)),
                    Some("NotificationClosed") => message
                        .body()
                        .deserialize::<(u32, u32)>()
                        .ok()
                        .filter(|(of, _)| *of == id)
                        .map(|_| Outcome::Closed),
                    _ => None,
                };
                if let Some(outcome) = outcome {
                    // A click comes just before the close; the first answer is the one.
                    let _ = tx.send(outcome);
                }
            }
        })?;

        let outcome = rx.recv_timeout(wait).unwrap_or(Outcome::TimedOut);
        if outcome != Outcome::Closed {
            // Clicked: some servers leave it on screen. Timed out: take it away.
            let _ = self.proxy()?.call_method("CloseNotification", &(id,));
        }
        Ok(outcome)
    }
}

/// The icon of iCloud's notifications: the cloud folder of the icon theme
/// Files uses, as a file, so every notification server shows the same one.
fn notification_icon() -> String {
    crate::icons::lookup(ICON, ICON_SIZE).map_or_else(|| ICON.to_owned(), |path| path.to_string_lossy().into_owned())
}

const ICON: &str = "folder-remote";
/// Notification icons are drawn at about 32 px on a scale-2 screen.
const ICON_SIZE: u32 = 64;

/// Show `title` and `body` on the desktop, if there is one. For the
/// notifications nobody needs to click.
pub fn show(title: &str, body: &str) {
    if let Err(err) = Desktop::session().and_then(|desktop| desktop.show(title, body)) {
        tracing::debug!("could not show a notification: {err}");
    }
}

// ---- what the daemon reports ---------------------------------------------------------------

/// Where the daemon's notifications go.
pub trait Notifier: Send + Sync {
    /// A notification with nothing to click.
    fn notify(&self, title: &str, body: &str);
    /// The session has expired while `file_name` was being opened: offer to
    /// sign in again.
    fn offer_sign_in(&self, file_name: &str);
}

/// The real desktop. Nothing here blocks a filesystem request.
#[derive(Debug)]
pub struct DesktopNotifier {
    icloudctl: PathBuf,
    systemd_run: bool,
}

impl Default for DesktopNotifier {
    fn default() -> Self {
        Self { icloudctl: program_next_to_us("icloudctl"), systemd_run: find_on_path("systemd-run").is_some() }
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, title: &str, body: &str) {
        let (title, body) = (title.to_owned(), body.to_owned());
        let _ = thread::Builder::new().name("icloud-notify".into()).spawn(move || show(&title, &body));
    }

    fn offer_sign_in(&self, file_name: &str) {
        run_detached(&sign_in_offer_command(&self.icloudctl, self.systemd_run, file_name));
    }
}

/// `icloudctl sign-in-offer --file NAME`, as a transient systemd unit when
/// `systemd-run` is there.
fn sign_in_offer_command(icloudctl: &Path, systemd_run: bool, file_name: &str) -> Vec<OsString> {
    let mut command: Vec<OsString> = Vec::new();
    if systemd_run {
        command.extend(
            ["systemd-run", "--user", "--quiet", "--collect", &format!("--unit={SIGN_IN_UNIT}")].map(OsString::from),
        );
    }
    command.push(icloudctl.into());
    command.extend(["sign-in-offer", "--file", file_name].map(OsString::from));
    command
}

fn run_detached(command: &[OsString]) {
    let Some((program, args)) = command.split_first() else { return };
    let spawned =
        Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    match spawned {
        Ok(mut child) => {
            let _ = thread::Builder::new().name("icloud-notify-offer".into()).spawn(move || child.wait());
        }
        Err(err) => tracing::warn!("could not start {}: {err}", program.to_string_lossy()),
    }
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

/// The offer to sign in again: title, text and the label of its button.
pub fn sign_in_offer_text(file_name: &str) -> (String, String, String) {
    (
        trf("Could not open “{}”", &[&file_name]),
        tr("The iCloud session has expired, so the file cannot be downloaded. Click to sign in again on icloud.com.")
            .to_owned(),
        tr("Sign in on icloud.com").to_owned(),
    )
}

/// Why a file could not be downloaded for the person who opened it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFailure {
    /// Only a new sign-in helps.
    SignInNeeded,
    /// iCloud did not answer or answered with an error; trying again may work.
    Unreachable,
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
        let name = path.file_name().unwrap_or("iCloud");
        match failure {
            OpenFailure::SignInNeeded => self.notifier.offer_sign_in(name),
            OpenFailure::Unreachable => self.notifier.notify(
                &trf("Could not open “{}”", &[&name]),
                tr("iCloud could not be reached. Check the connection, then open the file again."),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zbus::{blocking::connection::Builder, interface};

    use super::*;

    #[derive(Default, Clone)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Notifier for Recorder {
        fn notify(&self, title: &str, body: &str) {
            self.0.lock().unwrap().push(format!("notify: {title}: {body}"));
        }
        fn offer_sign_in(&self, file_name: &str) {
            self.0.lock().unwrap().push(format!("offer: {file_name}"));
        }
    }

    fn path(text: &str) -> IcPath {
        IcPath::new(text)
    }

    #[test]
    fn an_expired_session_is_an_offer_to_sign_in_and_a_network_failure_is_not() {
        let sent = Recorder::default();
        let reporter = FailureReporter::new(sent.clone());
        let t0 = Instant::now();
        reporter.report_at(&path("/Docs/LLM.pdf"), OpenFailure::SignInNeeded, t0);
        reporter.report_at(&path("/a.pdf"), OpenFailure::Unreachable, t0 + QUIET_PERIOD);
        let sent = sent.0.lock().unwrap();
        assert_eq!(sent[0], "offer: LLM.pdf");
        assert!(sent[1].starts_with("notify: Could not open “a.pdf”: iCloud could not be reached"), "{}", sent[1]);
    }

    #[test]
    fn repeated_failures_are_reported_once_per_quiet_period() {
        let sent = Recorder::default();
        let reporter = FailureReporter::new(sent.clone());
        let t0 = Instant::now();
        reporter.report_at(&path("/a.pdf"), OpenFailure::Unreachable, t0);
        reporter.report_at(&path("/a.pdf"), OpenFailure::Unreachable, t0 + Duration::from_secs(1));
        reporter.report_at(&path("/b.pdf"), OpenFailure::SignInNeeded, t0 + Duration::from_secs(5));
        assert_eq!(sent.0.lock().unwrap().len(), 1);
        reporter.report_at(&path("/a.pdf"), OpenFailure::Unreachable, t0 + QUIET_PERIOD);
        assert_eq!(sent.0.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_offer_waits_outside_the_service_when_systemd_run_is_there() {
        let with = sign_in_offer_command(Path::new("/opt/bin/icloudctl"), true, "a b.pdf");
        assert_eq!(
            with,
            [
                "systemd-run",
                "--user",
                "--quiet",
                "--collect",
                "--unit=icloud-sign-in",
                "/opt/bin/icloudctl",
                "sign-in-offer",
                "--file",
                "a b.pdf"
            ]
            .map(OsString::from)
        );
        let without = sign_in_offer_command(Path::new("icloudctl"), false, "x");
        assert_eq!(without, ["icloudctl", "sign-in-offer", "--file", "x"].map(OsString::from));
    }

    #[test]
    fn the_offer_text_says_what_to_do() {
        let (title, body, button) = sign_in_offer_text("LLM.pdf");
        assert_eq!(title, "Could not open “LLM.pdf”");
        assert!(body.contains("Click to sign in again"), "{body}");
        assert_eq!(button, "Sign in on icloud.com");
    }

    // ---- against a stand-in notification server ---------------------------------------------

    /// What the stand-in server does once a notification arrives.
    #[derive(Clone, Copy)]
    enum Person {
        ClicksIt,
        ClicksTheButton,
        DismissesIt,
        IgnoresIt,
    }

    struct Server {
        shown: mpsc::Sender<(String, Vec<String>, i32)>,
        closed: Arc<Mutex<Vec<u32>>>,
    }

    #[interface(name = "org.freedesktop.Notifications")]
    impl Server {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            app: &str,
            replaces: u32,
            icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            expire: i32,
        ) -> u32 {
            let _ = (app, replaces, icon, body, hints);
            let _ = self.shown.send((summary.to_owned(), actions, expire));
            7
        }

        fn close_notification(&self, id: u32) {
            self.closed.lock().unwrap().push(id);
        }
    }

    /// Run `ask` against a server where `person` reacts to the notification.
    fn ask_with(person: Person, wait: Duration) -> (Outcome, Vec<String>, i32, Vec<u32>) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let (shown_tx, shown_rx) = mpsc::channel();
        let closed = Arc::new(Mutex::new(Vec::new()));
        let server_closed = closed.clone();
        let server = thread::spawn(move || {
            let guid = zbus::Guid::generate();
            Builder::async_io_unix_stream(theirs)
                .server(guid)
                .unwrap()
                .p2p()
                .serve_at(OBJECT, Server { shown: shown_tx, closed: server_closed })
                .unwrap()
                .build()
                .unwrap()
        });
        let client = Builder::async_io_unix_stream(ours).p2p().build().unwrap();
        let server = server.join().unwrap();

        let reactor = thread::spawn(move || {
            let (_, actions, expire) = shown_rx.recv().unwrap();
            let emit = |member: &str, body: &dyn erased::Body| body.emit(&server, member);
            match person {
                Person::ClicksIt => {
                    emit("ActionInvoked", &(7u32, DEFAULT_ACTION.to_owned()));
                    emit("NotificationClosed", &(7u32, 2u32));
                }
                Person::ClicksTheButton => emit("ActionInvoked", &(7u32, "sign-in".to_owned())),
                Person::DismissesIt => {
                    emit("NotificationClosed", &(99u32, 2u32)); // someone else's
                    emit("NotificationClosed", &(7u32, 2u32));
                }
                Person::IgnoresIt => {}
            }
            (actions, expire, server)
        });

        let outcome = Desktop::on(client)
            .ask("Could not open “a.pdf”", "body", &[(DEFAULT_ACTION, "Sign in"), ("sign-in", "Sign in")], wait)
            .unwrap();
        let (actions, expire, _server) = reactor.join().unwrap();
        thread::sleep(Duration::from_millis(50));
        let closed = closed.lock().unwrap().clone();
        (outcome, actions, expire, closed)
    }

    mod erased {
        use zbus::blocking::Connection;

        /// A signal body of either shape the server sends.
        pub(super) trait Body {
            fn emit(&self, conn: &Connection, member: &str);
        }

        impl<T: serde::Serialize + zbus::zvariant::DynamicType> Body for T {
            fn emit(&self, conn: &Connection, member: &str) {
                conn.emit_signal(None::<&str>, super::OBJECT, super::INTERFACE, member, self).unwrap();
            }
        }
    }

    #[test]
    fn a_click_on_the_notification_is_heard_and_it_stays_up_until_answered() {
        let (outcome, actions, expire, closed) = ask_with(Person::ClicksIt, Duration::from_secs(5));
        assert_eq!(outcome, Outcome::Clicked(DEFAULT_ACTION.into()));
        assert_eq!(actions, ["default", "Sign in", "sign-in", "Sign in"], "a default action and a button");
        assert_eq!(expire, 0, "it does not expire by itself");
        assert_eq!(closed, [7], "a clicked notification is taken away");
    }

    #[test]
    fn a_click_on_the_button_counts_too() {
        let (outcome, ..) = ask_with(Person::ClicksTheButton, Duration::from_secs(5));
        assert_eq!(outcome, Outcome::Clicked("sign-in".into()));
    }

    #[test]
    fn a_dismissed_notification_is_not_a_click() {
        let (outcome, _, _, closed) = ask_with(Person::DismissesIt, Duration::from_secs(5));
        assert_eq!(outcome, Outcome::Closed);
        assert!(closed.is_empty(), "nothing left to take away");
    }

    #[test]
    fn an_unanswered_offer_goes_away_after_the_wait() {
        let (outcome, _, _, closed) = ask_with(Person::IgnoresIt, Duration::from_millis(200));
        assert_eq!(outcome, Outcome::TimedOut);
        assert_eq!(closed, [7]);
    }
}
