//! Everything the guided installer does, without a toolkit.
//!
//! The installer walks the same steps as `icloudctl` does: `init`,
//! `configure`, `auth`, `start`. Here each is a method of a [`Backend`], so
//! the GTK window is only a view over this logic and the whole flow is tested
//! (and demoed) without a display or a real iCloud account.

mod auth_worker;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

pub use auth_worker::{AuthEvent, AuthRequest, AuthWorker, Authenticator, Channel, MAX_CODE_ATTEMPTS};
use secrecy::SecretString;

use crate::{
    config::{Config, CrawlMode},
    connect::client_for,
    dirs::Layout,
    error::{Error, Result},
    setup::{self, Check, Severity, Systemctl},
};

/// What the user chose in the wizard: one checkbox each, hence the flags.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct InstallPlan {
    pub mount_dir: PathBuf,
    pub crawl_mode: CrawlMode,
    /// Keep GNOME's indexer from downloading the whole drive. Recommended.
    pub keep_indexer_out: bool,
    /// Label the sidebar entry with what the daemon is doing (`icloud-status`).
    pub show_sidebar_status: bool,
    /// Add "Download from iCloud" to the right-click menu of the file manager.
    pub add_context_menu: bool,
    /// Save the password so an expired session can be renewed unattended.
    pub remember_password: bool,
}

impl InstallPlan {
    pub fn recommended(layout: &Layout) -> Self {
        Self {
            mount_dir: layout.default_mount(),
            crawl_mode: CrawlMode::Lazy,
            keep_indexer_out: true,
            show_sidebar_status: true,
            add_context_menu: true,
            remember_password: false,
        }
    }
}

/// One line of the final progress list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub title: String,
    pub result: std::result::Result<String, String>,
}

impl StepOutcome {
    fn ok(title: &str, detail: impl Into<String>) -> Self {
        Self { title: title.to_owned(), result: Ok(detail.into()) }
    }

    fn failed(title: &str, err: impl std::fmt::Display) -> Self {
        Self { title: title.to_owned(), result: Err(err.to_string()) }
    }

    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }
}

/// What `prepare` settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The folder that will be mounted (may differ from the request).
    pub mount_dir: PathBuf,
    pub used_fallback: bool,
}

/// The side effects of the installer. [`SystemBackend`] does them for real.
pub trait Backend: Send + Sync {
    /// Can this machine run icloud-linux at all? Problems come with a fix.
    fn preflight(&self) -> Vec<Check>;
    /// `icloudctl init`: directories, default configuration, systemd unit.
    fn prepare(&self, plan: &InstallPlan) -> Result<Prepared>;
    /// `icloudctl configure`: remember the Apple ID.
    fn save_account(&self, username: &str, password: Option<SecretString>) -> Result<()>;
    /// The object that signs in (`icloudctl auth`).
    fn authenticator(&self) -> Result<Box<dyn Authenticator>>;
    /// `icloudctl start`, plus the optional extras.
    fn finish(&self, plan: &InstallPlan, mount_dir: &Path) -> Vec<StepOutcome>;
}

/// The real thing: writes files and drives systemd.
pub struct SystemBackend {
    layout: Layout,
    sys: Arc<dyn Systemctl>,
}

impl std::fmt::Debug for SystemBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemBackend").finish_non_exhaustive()
    }
}

impl SystemBackend {
    pub fn new(layout: Layout, sys: Arc<dyn Systemctl>) -> Self {
        Self { layout, sys }
    }

    fn config(&self) -> Result<Config> {
        Config::load(&self.layout.config_file(), &self.layout)
    }
}

/// Only what the wizard should stop the user for.
fn blocking(check: &Check) -> bool {
    matches!(check.severity, Severity::Missing)
}

impl Backend for SystemBackend {
    fn preflight(&self) -> Vec<Check> {
        // The full doctor also reports things that cannot exist before
        // installation (no config yet, service not initialised).
        setup::doctor(&self.layout, &*self.sys)
            .into_iter()
            .filter(|c| c.message.contains("fuse") || c.message.contains("daemon") || c.message.contains("FUSE"))
            .filter(|c| blocking(c) || c.severity == Severity::Ok)
            .collect()
    }

    fn prepare(&self, plan: &InstallPlan) -> Result<Prepared> {
        let report = setup::init(&self.layout, &plan.mount_dir, &setup::locate_daemon()?, &*self.sys)?;
        let mut config = self.config()?;
        config.crawl_mode = plan.crawl_mode;
        config.save(&self.layout.config_file())?;
        Ok(Prepared { mount_dir: report.mount_dir, used_fallback: report.used_fallback })
    }

    fn save_account(&self, username: &str, password: Option<SecretString>) -> Result<()> {
        setup::configure(&self.layout, username, password).map(drop)
    }

    fn authenticator(&self) -> Result<Box<dyn Authenticator>> {
        Ok(Box::new(client_for(&self.config()?)?))
    }

    fn finish(&self, plan: &InstallPlan, mount_dir: &Path) -> Vec<StepOutcome> {
        let mut steps = Vec::new();

        let started = setup::start(&self.layout, &*self.sys);
        let started_ok = started.is_ok();
        steps.push(match started {
            Ok(()) => StepOutcome::ok("Start the iCloud service", "running"),
            Err(err) => StepOutcome::failed("Start the iCloud service", err),
        });
        if !started_ok {
            return steps;
        }

        steps.push(if wait_until_mounted(mount_dir, Duration::from_secs(30)) {
            StepOutcome::ok("Mount iCloud Drive", mount_dir.display().to_string())
        } else {
            StepOutcome::failed(
                "Mount iCloud Drive",
                format!("{} did not appear; see `icloudctl logs`", mount_dir.display()),
            )
        });

        if plan.keep_indexer_out {
            steps.push(match self.config().and_then(|c| setup::apply_trackerignore(&c)) {
                Ok(_) => StepOutcome::ok("Keep the search indexer out", "done"),
                Err(err) => StepOutcome::failed("Keep the search indexer out", err),
            });
        }
        if plan.show_sidebar_status {
            steps.push(match setup::install_status_service(&self.layout, &*self.sys) {
                Ok(_) => StepOutcome::ok("Show activity in the Files sidebar", "appears next to iCloud"),
                Err(err) => StepOutcome::failed("Show activity in the Files sidebar", err),
            });
        }
        if plan.add_context_menu {
            steps.push(match setup::install_menu_here(&self.layout) {
                Ok(_) => StepOutcome::ok(
                    "Add \"Download from iCloud\" to the right-click menu",
                    "right-click a file, then Scripts",
                ),
                Err(err) => StepOutcome::failed("Add \"Download from iCloud\" to the right-click menu", err),
            });
        }
        steps
    }
}

/// Poll until something is mounted at `mount`.
pub fn wait_until_mounted(mount: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if setup::is_mounted(mount) {
            return true;
        }
        thread::sleep(Duration::from_millis(250));
    }
    setup::is_mounted(mount)
}

/// Check what the user typed before anything is done with it.
pub fn validate_account(username: &str, password: &str) -> std::result::Result<(), &'static str> {
    let username = username.trim();
    if username.is_empty() {
        return Err("Enter your Apple ID.");
    }
    if username.contains(char::is_whitespace) {
        return Err("An Apple ID is an email address or phone number, without spaces.");
    }
    if password.is_empty() {
        return Err("Enter your Apple ID password.");
    }
    Ok(())
}

/// Turn "how many digits are typed" into whether the Verify button is usable.
pub fn code_is_complete(code: &str) -> bool {
    let digits = code.chars().filter(char::is_ascii_digit).count();
    let others = code.chars().filter(|c| !c.is_ascii_digit() && !c.is_whitespace() && *c != '-').count();
    others == 0 && (4..=10).contains(&digits)
}

/// The plan's mount folder must be somewhere the daemon can mount.
pub fn validate_mount_dir(dir: &Path, layout: &Layout) -> std::result::Result<(), String> {
    if !dir.is_absolute() {
        return Err("Choose a full folder path.".into());
    }
    if dir == layout.home || dir == Path::new("/") {
        return Err("Choose a folder inside your home directory, not the home directory itself.".into());
    }
    if dir.exists() && !dir.is_dir() {
        return Err("That path is a file, not a folder.".into());
    }
    if dir.is_dir()
        && std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
        && !setup::is_mounted(dir)
    {
        return Err("That folder is not empty. Pick an empty folder or a new name.".into());
    }
    Ok(())
}

pub fn ensure_error(message: impl Into<String>) -> Error {
    Error::Setup(message.into())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::{auth_worker::tests::Fake, *};
    use crate::setup::CommandOutput;

    #[derive(Default)]
    struct Sys {
        calls: Mutex<Vec<String>>,
        fail: Mutex<Option<String>>,
    }

    impl Systemctl for Sys {
        fn run(&self, args: &[&str]) -> std::io::Result<CommandOutput> {
            let line = args.join(" ");
            self.calls.lock().unwrap().push(line.clone());
            let failing = self.fail.lock().unwrap().as_deref().is_some_and(|f| line.starts_with(f));
            Ok(CommandOutput { success: !failing, stdout: String::new(), stderr: "denied".into() })
        }
    }

    fn backend() -> (tempfile::TempDir, Layout, Arc<Sys>, SystemBackend) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::under(dir.path());
        let sys = Arc::new(Sys::default());
        let backend = SystemBackend::new(layout.clone(), sys.clone());
        (dir, layout, sys, backend)
    }

    #[test]
    fn the_recommended_plan_is_lazy_private_and_conservative() {
        let plan = InstallPlan::recommended(&Layout::under(Path::new("/home/u")));
        assert_eq!(plan.mount_dir, Path::new("/home/u/iCloud"));
        assert_eq!(plan.crawl_mode, CrawlMode::Lazy);
        assert!(plan.keep_indexer_out);
        assert!(!plan.remember_password, "storing the password must be an explicit choice");
        assert!(plan.show_sidebar_status, "seeing what iCloud is doing is part of the experience");
        assert!(plan.add_context_menu);
    }

    #[test]
    fn account_input_is_validated() {
        assert!(validate_account("me@example.com", "pw").is_ok());
        assert!(validate_account("  me@example.com  ", "pw").is_ok());
        assert!(validate_account("", "pw").is_err());
        assert!(validate_account("me @example.com", "pw").is_err());
        assert!(validate_account("me@example.com", "").is_err());
    }

    #[test]
    fn a_code_is_complete_at_four_to_ten_digits() {
        assert!(code_is_complete("123456"));
        assert!(code_is_complete("123 456"));
        assert!(code_is_complete("123-456"));
        assert!(!code_is_complete("123"));
        assert!(!code_is_complete("12345a"));
        assert!(!code_is_complete(""));
        assert!(!code_is_complete("12345678901"));
    }

    #[test]
    fn mount_folders_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::under(dir.path());
        assert!(validate_mount_dir(&layout.default_mount(), &layout).is_ok(), "a new folder is fine");
        assert!(validate_mount_dir(Path::new("relative"), &layout).is_err());
        assert!(validate_mount_dir(&layout.home, &layout).is_err());
        assert!(validate_mount_dir(Path::new("/"), &layout).is_err());

        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_mount_dir(&file, &layout).unwrap_err().contains("file"));

        let full = dir.path().join("full");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("stuff"), b"x").unwrap();
        assert!(validate_mount_dir(&full, &layout).unwrap_err().contains("not empty"));

        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(validate_mount_dir(&empty, &layout).is_ok());
    }

    #[test]
    fn preparing_initialises_and_applies_the_chosen_crawl_mode_without_losing_settings() {
        let (_d, layout, sys, backend) = backend();
        // A daemon binary must be findable.
        let plan = InstallPlan { crawl_mode: CrawlMode::Full, ..InstallPlan::recommended(&layout) };
        let result = backend.prepare(&plan);
        // `locate_daemon` needs `icloudd` on disk; where it is absent the step
        // must fail with an explanation rather than write a broken service.
        match result {
            Ok(prepared) => {
                assert_eq!(prepared.mount_dir, plan.mount_dir);
                assert_eq!(Config::load(&layout.config_file(), &layout).unwrap().crawl_mode, CrawlMode::Full);
                assert_eq!(sys.calls.lock().unwrap().as_slice(), ["daemon-reload", "enable icloud.service"]);
            }
            Err(err) => {
                assert!(err.to_string().contains("icloudd"), "{err}");
                assert!(!layout.service_file().exists(), "no service may be registered without a daemon");
            }
        }
    }

    #[test]
    fn saving_the_account_honours_the_password_choice() {
        let (_d, layout, _sys, backend) = backend();
        backend.save_account("me@example.com", None).unwrap();
        let text = std::fs::read_to_string(layout.config_file()).unwrap();
        assert!(text.contains("me@example.com") && !text.contains("password"));
        backend.save_account("me@example.com", Some(SecretString::from("pw".to_owned()))).unwrap();
        assert!(std::fs::read_to_string(layout.config_file()).unwrap().contains("password"));
        assert!(backend.save_account("bad id", None).is_err());
    }

    #[test]
    fn finishing_stops_at_the_first_failure_and_says_why() {
        let (_d, layout, sys, backend) = backend();
        *sys.fail.lock().unwrap() = Some("start".into());
        let steps = backend.finish(&InstallPlan::recommended(&layout), &layout.default_mount());
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].is_ok());
        assert!(steps[0].result.as_ref().unwrap_err().contains("denied"));
    }

    #[test]
    fn a_mount_that_never_appears_is_detected() {
        let (_d, layout, _sys, _backend) = backend();
        let dir = layout.default_mount();
        // The fake systemctl "starts" the service but nothing mounts.
        assert!(!wait_until_mounted(&dir, Duration::from_millis(50)));
    }

    #[test]
    fn the_authenticator_needs_a_configured_apple_id() {
        let (_d, layout, _sys, backend) = backend();
        assert!(backend.authenticator().is_err());
        backend.save_account("me@example.com", None).unwrap();
        assert!(backend.authenticator().is_ok());
        drop(layout);
    }

    #[test]
    fn preflight_only_lists_what_matters_before_installing() {
        let (_d, _layout, _sys, backend) = backend();
        let checks = backend.preflight();
        assert!(!checks.is_empty());
        for check in &checks {
            assert!(matches!(check.severity, Severity::Ok | Severity::Missing), "{check:?}");
            assert!(!check.message.contains("config"), "config problems are expected before install");
        }
    }

    #[test]
    fn a_full_scripted_wizard_run_ends_authenticated() {
        // The path a GUI takes: account, sign in with 2FA, verify.
        let (mut fake, log) = Fake::new();
        fake.login.push_back(Ok(icloud_api::LoginStatus::TwoFactorRequired));
        let worker = AuthWorker::spawn(Box::new(fake));
        worker.request(AuthRequest::Login(SecretString::from("pw".to_owned())));
        let AuthEvent::CodeNeeded(options) = worker.wait(Duration::from_secs(5)).unwrap() else { panic!() };
        assert!(options.can_use_sms());
        worker.request(AuthRequest::SendSms { phone: 0 });
        assert!(matches!(worker.wait(Duration::from_secs(5)), Some(AuthEvent::SmsSent { .. })));
        worker.request(AuthRequest::Verify { channel: Channel::Sms(0), code: "123456".into() });
        assert!(matches!(worker.wait(Duration::from_secs(5)), Some(AuthEvent::Authenticated(_))));
        assert_eq!(log.lock().unwrap().len(), 3);
    }
}
