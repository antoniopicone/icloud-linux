//! Installing, configuring and controlling the service.
//!
//! Everything the `icloudctl` commands `init`, `configure`, `start`, `stop`,
//! `doctor`… do lives here, so the command line tool and the GTK installer
//! share one implementation. Nothing here spawns a shell: external programs
//! (`systemctl`, `fusermount3`) are run with explicit argument lists.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use secrecy::SecretString;

use crate::{
    config::{Config, write_private_file},
    dirs::Layout,
    error::{Error, Result},
    mirror::TRACKER_IGNORE,
};

pub const SERVICE_NAME: &str = "icloud.service";
/// Shows what the daemon is doing next to "iCloud" in the file manager's sidebar.
pub const STATUS_SERVICE: &str = "icloud-status.service";

// ---- running external programs ------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `systemctl --user …`. A trait so tests need no systemd.
pub trait Systemctl: Send + Sync {
    fn run(&self, args: &[&str]) -> io::Result<CommandOutput>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RealSystemctl;

impl Systemctl for RealSystemctl {
    fn run(&self, args: &[&str]) -> io::Result<CommandOutput> {
        let out = Command::new("systemctl").arg("--user").args(args).output()?;
        Ok(CommandOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Run and require success, turning a failure into a readable error.
fn systemctl_ok(sys: &dyn Systemctl, args: &[&str]) -> Result<CommandOutput> {
    let out = sys.run(args).map_err(|e| Error::Setup(format!("cannot run systemctl: {e}")))?;
    if out.success {
        Ok(out)
    } else {
        Err(Error::Setup(format!("`systemctl --user {}` failed: {}", args.join(" "), out.stderr.trim())))
    }
}

// ---- generated files ----------------------------------------------------------------

/// Quote one argument for a systemd unit's `ExecStart=` and friends.
pub fn systemd_quote(arg: &str) -> Result<String> {
    if arg.chars().any(char::is_control) {
        return Err(Error::Setup("a systemd argument must not contain control characters".into()));
    }
    let escaped = arg.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
    Ok(format!("\"{escaped}\""))
}

/// The `systemd --user` unit that runs the daemon.
pub fn service_unit(daemon: &Path, config: &Path, mount: &Path, fusermount: &Path) -> Result<String> {
    let q = |p: &Path| systemd_quote(&p.to_string_lossy());
    let (daemon, config, mount, fusermount) = (q(daemon)?, q(config)?, q(mount)?, q(fusermount)?);
    Ok(format!(
        "[Unit]
Description=iCloud Linux FUSE filesystem (user)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStartPre=-{fusermount} -uz {mount}
ExecStart={daemon} --config {config} {mount}
ExecStop=-{fusermount} -uz {mount}
ExecStopPost=-{fusermount} -uz {mount}
Restart=on-failure
RestartSec=15
TimeoutStopSec=10
KillMode=control-group
Environment=ICLOUD_LOG_PATH=%h/.local/state/icloud-linux/icloud.log

[Install]
WantedBy=default.target
"
    ))
}

/// Quote for a POSIX shell, since `icloud.env` is `source`d by other tools.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn env_file(config: &Path, mount: &Path) -> Result<String> {
    for p in [config, mount] {
        if p.to_string_lossy().contains(['\n', '\r']) {
            return Err(Error::Setup("paths must not contain newlines".into()));
        }
    }
    Ok(format!(
        "ICLOUD_CONFIG={}\nICLOUD_MOUNT={}\n",
        shell_quote(&config.to_string_lossy()),
        shell_quote(&mount.to_string_lossy())
    ))
}

/// The mount point recorded by `init`, if any.
pub fn recorded_mount(layout: &Layout) -> Option<PathBuf> {
    let text = fs::read_to_string(layout.env_file()).ok()?;
    let value = text.lines().find_map(|line| line.trim().strip_prefix("ICLOUD_MOUNT="))?;
    let value = unquote(value.trim());
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// Undo `shell_quote` and the `%q` style escaping of the Python-era files.
fn unquote(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    let mut single = false;
    let mut double = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '\\' if !single => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            other => out.push(other),
        }
    }
    out
}

// ---- helpers about the machine -----------------------------------------------------------

/// The FUSE unmount helper: `fusermount3` on current systems, `fusermount` on old ones.
pub fn find_fusermount() -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .flat_map(|dir| ["fusermount3", "fusermount"].map(|name| Path::new(dir).join(name)))
        .find(|candidate| candidate.is_file())
}

/// Where one of our binaries is: next to this executable, else on `PATH`.
pub fn locate_binary(name: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(sibling) = exe.parent().map(|dir| dir.join(name)).filter(|p| p.is_file())
    {
        return Some(sibling);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Where the daemon binary is.
pub fn locate_daemon() -> Result<PathBuf> {
    locate_binary("icloudd")
        .ok_or_else(|| Error::Setup("cannot find the `icloudd` daemon next to icloudctl or on PATH".into()))
}

/// Is something mounted exactly at `path`?
pub fn is_mounted(path: &Path) -> bool {
    let Ok(canonical) = fs::canonicalize(path) else { return false };
    fs::read_to_string("/proc/self/mountinfo").is_ok_and(|info| mounted_in(&info, &canonical))
}

fn mounted_in(mountinfo: &str, path: &Path) -> bool {
    mountinfo
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .any(|point| Path::new(&unescape_octal(point)) == path)
}

/// `/proc` escapes space, tab, newline and backslash as `\040`-style octal.
fn unescape_octal(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let value = bytes[i + 1..i + 4].iter().fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            out.push(u8::try_from(value).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Unmount `mount` if it is mounted or left as a dead FUSE endpoint
/// ("Transport endpoint is not connected") by a crashed daemon.
pub fn cleanup_mountpoint(mount: &Path) -> Result<()> {
    let stale = matches!(fs::metadata(mount), Err(e) if e.raw_os_error() == Some(107));
    if !(is_mounted(mount) || stale) {
        fs::create_dir_all(mount)?;
        return Ok(());
    }
    if let Some(tool) = find_fusermount() {
        // Lazy unmount first, then a plain one; either failing is fine if the
        // other worked, and the next check settles it.
        for flag in ["-uz", "-u"] {
            let _ = Command::new(&tool).arg(flag).arg(mount).output();
            if !is_mounted(mount) {
                break;
            }
        }
    }
    fs::create_dir_all(mount)?;
    Ok(())
}

// ---- init -------------------------------------------------------------------------------------

/// Create a directory and everything above it, private to the user.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    pub mount_dir: PathBuf,
    /// The requested mount point was not writable, so another was chosen.
    pub used_fallback: bool,
    pub config_created: bool,
    pub service_file: PathBuf,
}

/// Prepare directories, a default configuration and the systemd unit. This
/// does not need credentials, so it can run before the user has typed any.
pub fn init(layout: &Layout, requested_mount: &Path, daemon: &Path, sys: &dyn Systemctl) -> Result<InitReport> {
    for dir in [&layout.config_dir, &layout.state_dir, &layout.cache_dir] {
        ensure_private_dir(dir)?;
    }
    fs::create_dir_all(&layout.systemd_user_dir)?;

    let config_created = !layout.config_file().exists();
    if config_created {
        Config::for_layout(layout).save(&layout.config_file())?;
    }

    let (mount_dir, used_fallback) = usable_mount(layout, requested_mount)?;

    write_private_file(&layout.env_file(), env_file(&layout.config_file(), &mount_dir)?.as_bytes())?;
    let fusermount = find_fusermount().unwrap_or_else(|| PathBuf::from("/usr/bin/fusermount3"));
    let unit = service_unit(daemon, &layout.config_file(), &mount_dir, &fusermount)?;
    fs::write(layout.service_file(), unit)?;

    systemctl_ok(sys, &["daemon-reload"])?;
    systemctl_ok(sys, &["enable", SERVICE_NAME])?;
    Ok(InitReport { mount_dir, used_fallback, config_created, service_file: layout.service_file() })
}

/// The requested mount point if it can be created and written to, else `~/iCloudDrive`.
fn usable_mount(layout: &Layout, requested: &Path) -> Result<(PathBuf, bool)> {
    let writable = |dir: &Path| fs::create_dir_all(dir).is_ok() && is_writable(dir);
    if writable(requested) {
        return Ok((requested.to_owned(), false));
    }
    let fallback = layout.home.join("iCloudDrive");
    if writable(&fallback) {
        return Ok((fallback, true));
    }
    Err(Error::Setup(format!(
        "{} is not writable. Fix it with: sudo chown -R $USER: '{}'",
        requested.display(),
        requested.display()
    )))
}

fn is_writable(dir: &Path) -> bool {
    rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).is_ok()
}

// ---- configure ---------------------------------------------------------------------------------

/// Record the Apple ID (and optionally the password) in `config.yaml`,
/// keeping every other setting already there.
///
/// Storing the password is optional and off by default: with it the daemon
/// can renew an expired session unattended, without it an expired session
/// needs `icloudctl auth` again. It is written with mode 0600.
pub fn configure(layout: &Layout, username: &str, password: Option<SecretString>) -> Result<Config> {
    let username = username.trim();
    if username.is_empty() || username.contains(char::is_whitespace) {
        return Err(Error::Config("the Apple ID must be an email address or phone number".into()));
    }
    ensure_private_dir(&layout.config_dir)?;
    let path = layout.config_file();
    let mut config = if path.exists() { Config::load(&path, layout)? } else { Config::for_layout(layout) };
    config.username = username.to_owned();
    config.password = password;
    config.save(&path)?;
    Ok(config)
}

// ---- service control ------------------------------------------------------------------------

pub fn is_active(sys: &dyn Systemctl) -> bool {
    sys.run(&["is-active", "--quiet", SERVICE_NAME]).is_ok_and(|o| o.success)
}

pub fn start(layout: &Layout, sys: &dyn Systemctl) -> Result<()> {
    if let Some(mount) = recorded_mount(layout) {
        cleanup_mountpoint(&mount)?;
    }
    systemctl_ok(sys, &["start", SERVICE_NAME]).map(drop)
}

pub fn stop(layout: &Layout, sys: &dyn Systemctl) -> Result<()> {
    // Stopping something that is not running is not an error.
    let _ = sys.run(&["stop", SERVICE_NAME]);
    if let Some(mount) = recorded_mount(layout) {
        cleanup_mountpoint(&mount)?;
    }
    Ok(())
}

pub fn restart(layout: &Layout, sys: &dyn Systemctl) -> Result<()> {
    stop(layout, sys)?;
    start(layout, sys)
}

pub fn status_text(sys: &dyn Systemctl) -> String {
    match sys.run(&["--no-pager", "status", SERVICE_NAME]) {
        Ok(out) => out.stdout,
        Err(err) => format!("cannot run systemctl: {err}"),
    }
}

/// PID of the running daemon, from systemd.
pub fn daemon_pid(sys: &dyn Systemctl) -> Option<i32> {
    let out = sys.run(&["show", SERVICE_NAME, "--property=MainPID", "--value"]).ok()?;
    out.stdout.trim().parse::<i32>().ok().filter(|pid| *pid > 0)
}

/// Remove the service. With `purge`, configuration, session and cache too.
pub fn uninstall(layout: &Layout, sys: &dyn Systemctl, purge: bool) -> Result<()> {
    remove_status_service(layout, sys)?;
    let _ = sys.run(&["stop", SERVICE_NAME]);
    let _ = sys.run(&["disable", SERVICE_NAME]);
    remove_file_if_exists(&layout.service_file())?;
    let _ = sys.run(&["daemon-reload"]);
    let _ = sys.run(&["reset-failed"]);
    if let Some(mount) = recorded_mount(layout) {
        cleanup_mountpoint(&mount)?;
    }
    if purge {
        let cache =
            Config::load(&layout.config_file(), layout).map_or_else(|_| layout.cache_dir.clone(), |c| c.cache_dir);
        for path in [layout.config_file(), layout.env_file()] {
            remove_file_if_exists(&path)?;
        }
        for dir in [layout.cookie_dir(), cache, layout.state_dir.clone()] {
            match fs::remove_dir_all(&dir) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Delete the cache (mirror and state) and restart the service if it ran.
pub fn clear_cache(layout: &Layout, config: &Config, sys: &dyn Systemctl) -> Result<bool> {
    let was_active = is_active(sys);
    let _ = sys.run(&["stop", SERVICE_NAME]);
    if let Some(mount) = recorded_mount(layout) {
        cleanup_mountpoint(&mount)?;
    }
    match fs::remove_dir_all(&config.cache_dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    ensure_private_dir(&config.cache_dir)?;
    if was_active {
        start(layout, sys)?;
    }
    Ok(was_active)
}

// ---- desktop integration ------------------------------------------------------------------------------

/// The systemd user unit for the sidebar status watcher.
pub fn status_unit(binary: &Path) -> Result<String> {
    let binary = systemd_quote(&binary.to_string_lossy())?;
    Ok(format!(
        "[Unit]
Description=iCloud Drive status in the Files sidebar
After={SERVICE_NAME}
PartOf={SERVICE_NAME}

[Service]
Type=simple
ExecStart={binary}
Restart=on-failure
RestartSec=5

[Install]
WantedBy={SERVICE_NAME}
"
    ))
}

pub fn status_unit_file(layout: &Layout) -> PathBuf {
    layout.systemd_user_dir.join(STATUS_SERVICE)
}

pub fn status_service_installed(layout: &Layout) -> bool {
    status_unit_file(layout).exists()
}

/// Install and start the sidebar status watcher, looking for its binary next
/// to this one. It starts and stops with `icloud.service` and puts the plain
/// label back when it stops.
pub fn install_status_service(layout: &Layout, sys: &dyn Systemctl) -> Result<PathBuf> {
    let binary = locate_binary("icloud-status")
        .ok_or_else(|| Error::Setup("cannot find `icloud-status` next to icloudctl or on PATH".into()))?;
    install_status_unit(layout, sys, &binary)
}

/// [`install_status_service`] for a known binary.
pub fn install_status_unit(layout: &Layout, sys: &dyn Systemctl, binary: &Path) -> Result<PathBuf> {
    let unit = status_unit(binary)?;
    fs::create_dir_all(&layout.systemd_user_dir)?;
    let target = status_unit_file(layout);
    fs::write(&target, unit)?;
    systemctl_ok(sys, &["daemon-reload"])?;
    systemctl_ok(sys, &["enable", "--now", STATUS_SERVICE])?;
    Ok(target)
}

/// Stop and remove the watcher. Removing what is not there is fine.
pub fn remove_status_service(layout: &Layout, sys: &dyn Systemctl) -> Result<()> {
    if !status_service_installed(layout) {
        return Ok(());
    }
    let _ = sys.run(&["disable", "--now", STATUS_SERVICE]);
    remove_file_if_exists(&status_unit_file(layout))?;
    let _ = sys.run(&["daemon-reload"]);
    Ok(())
}

/// Create the marker that keeps GNOME's indexer out of the mirror, and nudge
/// the indexer so it notices. Returns whether anything changed.
pub fn apply_trackerignore(config: &Config) -> Result<bool> {
    let mirror = config.mirror_dir();
    if !mirror.is_dir() {
        return Err(Error::Setup(format!(
            "{} does not exist yet: start the service once, then run this again",
            mirror.display()
        )));
    }
    let marker = mirror.join(TRACKER_IGNORE);
    if marker.exists() {
        return Ok(false);
    }
    fs::write(&marker, b"")?;
    // The indexer may already be walking the tree with the old rules. It is
    // D-Bus activated, so killing it is enough: it comes back on demand.
    let _ = Command::new("pkill")
        .args(["-u", &rustix::process::getuid().as_raw().to_string(), "-f", "localsearch-3|tracker-miner-fs"])
        .output();
    Ok(true)
}

// ---- diagnostics ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Info,
    Warn,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub severity: Severity,
    pub message: String,
    /// What to do about it, when there is something to do.
    pub fix: Option<String>,
}

impl Check {
    fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self { severity, message: message.into(), fix: None }
    }

    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

/// Look at the installation and say what is wrong with it.
pub fn doctor(layout: &Layout, sys: &dyn Systemctl) -> Vec<Check> {
    let mut checks = Vec::new();

    checks.push(if Path::new("/dev/fuse").exists() {
        Check::new(Severity::Ok, "/dev/fuse is present")
    } else {
        Check::new(Severity::Missing, "/dev/fuse is missing")
            .with_fix("install FUSE (fuse3) and load the fuse kernel module")
    });
    checks.push(match find_fusermount() {
        Some(path) => Check::new(Severity::Ok, format!("{} found", path.display())),
        None => Check::new(Severity::Missing, "fusermount3 not found").with_fix("install the fuse3 package"),
    });
    checks.push(match locate_daemon() {
        Ok(path) => Check::new(Severity::Ok, format!("daemon: {}", path.display())),
        Err(err) => Check::new(Severity::Missing, err.to_string()),
    });

    let config_path = layout.config_file();
    let config = if config_path.exists() {
        match Config::load(&config_path, layout) {
            Ok(config) => {
                checks.push(Check::new(Severity::Ok, format!("config: {}", config_path.display())));
                if let Ok(meta) = fs::metadata(&config_path)
                    && meta.permissions().mode() & 0o077 != 0
                {
                    checks.push(
                        Check::new(Severity::Warn, "config.yaml is readable by other users")
                            .with_fix(format!("chmod 600 {}", config_path.display())),
                    );
                }
                if config.password.is_some() {
                    checks.push(Check::new(
                        Severity::Info,
                        "the password is stored in config.yaml (0600); remove the `password` line to stop storing it",
                    ));
                }
                Some(config)
            }
            Err(err) => {
                checks.push(Check::new(Severity::Warn, err.to_string()));
                None
            }
        }
    } else {
        checks.push(
            Check::new(Severity::Missing, format!("{} not found", config_path.display()))
                .with_fix("run `icloudctl init`"),
        );
        None
    };

    let mount = recorded_mount(layout);
    match &mount {
        Some(mount) if is_writable(mount) => {
            checks.push(Check::new(Severity::Ok, format!("mount point: {}", mount.display())));
        }
        Some(mount) => {
            checks.push(Check::new(Severity::Warn, format!("mount point not writable: {}", mount.display())));
        }
        None => checks.push(Check::new(Severity::Missing, "no mount point recorded").with_fix("run `icloudctl init`")),
    }
    checks.push(if layout.service_file().exists() {
        Check::new(Severity::Ok, "service file installed")
    } else {
        Check::new(Severity::Warn, "service not initialised").with_fix("run `icloudctl init`")
    });
    checks.push(if is_active(sys) {
        Check::new(Severity::Ok, "service is running")
    } else {
        Check::new(Severity::Info, "service is not running")
    });

    if let Some(config) = &config {
        checks.push(session_check(config));
        let mirror = config.mirror_dir();
        if mirror.is_dir() {
            checks.push(if mirror.join(TRACKER_IGNORE).exists() {
                Check::new(Severity::Ok, "excluded from desktop search indexing")
            } else {
                Check::new(Severity::Warn, "GNOME's indexer may download the whole drive")
                    .with_fix("run `icloudctl trackerignore`")
            });
        }
    }
    checks.push(if status_service_installed(layout) {
        Check::new(Severity::Ok, "sidebar status service installed")
    } else {
        Check::new(Severity::Info, "sidebar status service not installed").with_fix("run `icloudctl status-install`")
    });
    checks
}

fn session_check(config: &Config) -> Check {
    let has_session = fs::read_dir(&config.cookie_dir)
        .is_ok_and(|entries| entries.flatten().any(|e| e.path().extension().is_some_and(|x| x == "session")));
    if has_session {
        Check::new(Severity::Ok, "an iCloud session is saved")
    } else {
        Check::new(Severity::Warn, "no iCloud session saved").with_fix("run `icloudctl auth`")
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use super::*;

    /// A `systemctl` that records what it was asked and answers from a script.
    #[derive(Default)]
    pub(crate) struct FakeSystemctl {
        pub(crate) calls: Mutex<Vec<String>>,
        pub(crate) fail_on: Mutex<Option<String>>,
        pub(crate) active: Mutex<bool>,
        pub(crate) main_pid: Mutex<String>,
    }

    impl FakeSystemctl {
        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Systemctl for FakeSystemctl {
        fn run(&self, args: &[&str]) -> io::Result<CommandOutput> {
            let line = args.join(" ");
            self.calls.lock().unwrap().push(line.clone());
            let failing = self.fail_on.lock().unwrap().as_deref().is_some_and(|f| line.starts_with(f));
            let active = *self.active.lock().unwrap();
            let stdout = if line.starts_with("show") { self.main_pid.lock().unwrap().clone() } else { String::new() };
            Ok(CommandOutput {
                success: !failing && (!line.starts_with("is-active") || active),
                stdout,
                stderr: if failing { "boom".into() } else { String::new() },
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::{testing::FakeSystemctl, *};

    fn setup() -> (tempfile::TempDir, Layout, FakeSystemctl) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::under(dir.path());
        (dir, layout, FakeSystemctl::default())
    }

    // ---- generated text ---------------------------------------------------------

    #[test]
    fn systemd_arguments_are_quoted_and_control_characters_refused() {
        assert_eq!(systemd_quote("/plain/path").unwrap(), "\"/plain/path\"");
        assert_eq!(systemd_quote("/with space/and\"quote").unwrap(), "\"/with space/and\\\"quote\"");
        assert_eq!(systemd_quote("100%").unwrap(), "\"100%%\"", "% is special to systemd");
        assert_eq!(systemd_quote("back\\slash").unwrap(), "\"back\\\\slash\"");
        assert!(systemd_quote("a\nb").is_err());
        assert!(systemd_quote("a\rb").is_err());
        assert!(systemd_quote("a\0b").is_err());
    }

    #[test]
    fn the_service_unit_starts_the_daemon_and_cleans_up_the_mount() {
        let unit = service_unit(
            Path::new("/opt/icloud/icloudd"),
            Path::new("/home/u/.config/icloud-linux/config.yaml"),
            Path::new("/home/u/My iCloud"),
            Path::new("/usr/bin/fusermount3"),
        )
        .unwrap();
        assert!(unit.contains("ExecStart=\"/opt/icloud/icloudd\" --config \"/home/u/.config/icloud-linux/config.yaml\" \"/home/u/My iCloud\""));
        assert!(unit.contains("ExecStartPre=-\"/usr/bin/fusermount3\" -uz \"/home/u/My iCloud\""));
        assert!(unit.contains("ExecStopPost=-\"/usr/bin/fusermount3\" -uz"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("ICLOUD_LOG_PATH=%h/.local/state/icloud-linux/icloud.log"));
    }

    #[test]
    fn a_hostile_mount_path_cannot_inject_unit_directives() {
        let evil = Path::new("/tmp/x\nExecStart=/bin/sh");
        assert!(service_unit(Path::new("/d"), Path::new("/c"), evil, Path::new("/f")).is_err());
    }

    #[test]
    fn the_env_file_round_trips_awkward_paths() {
        for mount in ["/home/u/iCloud", "/home/u/My iCloud", "/home/u/it's here", "/home/u/$HOME `x`"] {
            let text = env_file(Path::new("/c/config.yaml"), Path::new(mount)).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let layout = Layout::under(dir.path());
            fs::create_dir_all(&layout.config_dir).unwrap();
            fs::write(layout.env_file(), text).unwrap();
            assert_eq!(recorded_mount(&layout), Some(PathBuf::from(mount)), "{mount}");
        }
        assert!(env_file(Path::new("/c"), Path::new("/a\nb")).is_err());
    }

    #[test]
    fn the_env_file_written_by_the_old_shell_script_is_still_understood() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::under(dir.path());
        fs::create_dir_all(&layout.config_dir).unwrap();
        // `printf '%q'` output for a path with a space.
        fs::write(layout.env_file(), "ICLOUD_CONFIG=/c/config.yaml\nICLOUD_MOUNT=/home/u/My\\ iCloud\n").unwrap();
        assert_eq!(recorded_mount(&layout), Some(PathBuf::from("/home/u/My iCloud")));
    }

    #[test]
    fn a_missing_env_file_means_no_recorded_mount() {
        let (_d, layout, _) = setup();
        assert_eq!(recorded_mount(&layout), None);
    }

    #[test]
    fn mount_detection_reads_mountinfo_including_escaped_names() {
        let info = "\
36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
99 35 0:50 / /home/u/My\\040iCloud rw,nosuid,nodev,relatime shared:5 - fuse icloudd rw,user_id=1000
";
        assert!(mounted_in(info, Path::new("/home/u/My iCloud")));
        assert!(mounted_in(info, Path::new("/mnt2")));
        assert!(!mounted_in(info, Path::new("/home/u")));
        assert!(!mounted_in("", Path::new("/x")));
    }

    #[test]
    fn octal_escapes_decode() {
        assert_eq!(unescape_octal("a\\040b"), "a b");
        assert_eq!(unescape_octal("a\\134b"), "a\\b");
        assert_eq!(unescape_octal("plain"), "plain");
        assert_eq!(unescape_octal("trailing\\04"), "trailing\\04");
    }

    // ---- init --------------------------------------------------------------------

    #[test]
    fn init_prepares_everything_and_registers_the_service() {
        let (_d, layout, sys) = setup();
        let mount = layout.home.join("iCloud");
        let report = init(&layout, &mount, Path::new("/opt/icloudd"), &sys).unwrap();

        assert_eq!(report.mount_dir, mount);
        assert!(!report.used_fallback && report.config_created);
        assert!(layout.config_file().is_file());
        assert!(mount.is_dir());
        assert_eq!(recorded_mount(&layout), Some(mount.clone()));
        let unit = fs::read_to_string(layout.service_file()).unwrap();
        assert!(unit.contains("/opt/icloudd"));
        assert_eq!(sys.calls(), ["daemon-reload", "enable icloud.service"]);
    }

    #[test]
    fn init_creates_private_directories() {
        let (_d, layout, sys) = setup();
        init(&layout, &layout.home.join("iCloud"), Path::new("/d"), &sys).unwrap();
        for dir in [&layout.config_dir, &layout.state_dir, &layout.cache_dir] {
            assert_eq!(fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700, "{}", dir.display());
        }
        assert_eq!(fs::metadata(layout.config_file()).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(layout.env_file()).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn init_keeps_an_existing_configuration() {
        let (_d, layout, sys) = setup();
        configure(&layout, "me@example.com", None).unwrap();
        let report = init(&layout, &layout.home.join("iCloud"), Path::new("/d"), &sys).unwrap();
        assert!(!report.config_created);
        assert_eq!(Config::load(&layout.config_file(), &layout).unwrap().username, "me@example.com");
    }

    #[test]
    fn init_falls_back_when_the_mount_point_is_not_usable() {
        let (_d, layout, sys) = setup();
        let blocker = layout.home.join("file");
        fs::write(&blocker, b"").unwrap();
        let report = init(&layout, &blocker.join("iCloud"), Path::new("/d"), &sys).unwrap();
        assert!(report.used_fallback);
        assert_eq!(report.mount_dir, layout.home.join("iCloudDrive"));
    }

    #[test]
    fn init_reports_systemd_failures() {
        let (_d, layout, sys) = setup();
        *sys.fail_on.lock().unwrap() = Some("daemon-reload".into());
        let err = init(&layout, &layout.home.join("iCloud"), Path::new("/d"), &sys).unwrap_err();
        assert!(err.to_string().contains("daemon-reload") && err.to_string().contains("boom"));
    }

    // ---- configure ---------------------------------------------------------------------

    #[test]
    fn configure_stores_the_apple_id_without_a_password_by_default() {
        let (_d, layout, _) = setup();
        configure(&layout, "  me@example.com ", None).unwrap();
        let text = fs::read_to_string(layout.config_file()).unwrap();
        assert!(text.contains("me@example.com") && !text.contains("password"));
        assert_eq!(fs::metadata(layout.config_file()).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn configure_can_store_a_password_when_asked_and_replaces_it_later() {
        let (_d, layout, _) = setup();
        configure(&layout, "me@example.com", Some(SecretString::from("pw1".to_owned()))).unwrap();
        let loaded = Config::load(&layout.config_file(), &layout).unwrap();
        assert_eq!(loaded.password.as_ref().map(ExposeSecret::expose_secret), Some("pw1"));
        configure(&layout, "me@example.com", None).unwrap();
        assert!(
            Config::load(&layout.config_file(), &layout).unwrap().password.is_none(),
            "declining removes a stored password"
        );
    }

    #[test]
    fn configure_preserves_other_settings() {
        let (_d, layout, _) = setup();
        fs::create_dir_all(&layout.config_dir).unwrap();
        fs::write(layout.config_file(), "sync_paths: [/Downloads]\ncrawl_mode: full\nusername: old@x.com\n").unwrap();
        configure(&layout, "new@x.com", None).unwrap();
        let loaded = Config::load(&layout.config_file(), &layout).unwrap();
        assert_eq!(loaded.username, "new@x.com");
        assert_eq!(loaded.sync_paths, ["/Downloads"]);
        assert_eq!(loaded.crawl_mode, crate::config::CrawlMode::Full);
    }

    #[test]
    fn configure_rejects_an_unusable_apple_id() {
        let (_d, layout, _) = setup();
        for bad in ["", "   ", "two words@x.com"] {
            assert!(configure(&layout, bad, None).is_err(), "{bad:?}");
        }
    }

    // ---- control -------------------------------------------------------------------------------

    #[test]
    fn start_stop_restart_drive_systemctl_in_order() {
        let (_d, layout, sys) = setup();
        start(&layout, &sys).unwrap();
        assert_eq!(sys.calls(), ["start icloud.service"]);
        sys.calls.lock().unwrap().clear();
        restart(&layout, &sys).unwrap();
        assert_eq!(sys.calls(), ["stop icloud.service", "start icloud.service"]);
    }

    #[test]
    fn stopping_a_service_that_is_not_running_is_fine() {
        let (_d, layout, sys) = setup();
        *sys.fail_on.lock().unwrap() = Some("stop".into());
        stop(&layout, &sys).unwrap();
    }

    #[test]
    fn a_start_failure_is_an_error() {
        let (_d, layout, sys) = setup();
        *sys.fail_on.lock().unwrap() = Some("start".into());
        assert!(start(&layout, &sys).is_err());
    }

    #[test]
    fn the_daemon_pid_comes_from_systemd_and_zero_means_not_running() {
        let (_d, _l, sys) = setup();
        *sys.main_pid.lock().unwrap() = "4242\n".into();
        assert_eq!(daemon_pid(&sys), Some(4242));
        *sys.main_pid.lock().unwrap() = "0\n".into();
        assert_eq!(daemon_pid(&sys), None);
        *sys.main_pid.lock().unwrap() = "garbage".into();
        assert_eq!(daemon_pid(&sys), None);
    }

    #[test]
    fn activity_is_read_from_is_active() {
        let (_d, _l, sys) = setup();
        assert!(!is_active(&sys));
        *sys.active.lock().unwrap() = true;
        assert!(is_active(&sys));
    }

    // ---- uninstall and cache ----------------------------------------------------------------------

    #[test]
    fn uninstall_keeps_data_unless_purging() {
        let (_d, layout, sys) = setup();
        init(&layout, &layout.home.join("iCloud"), Path::new("/d"), &sys).unwrap();
        fs::create_dir_all(layout.cookie_dir()).unwrap();
        fs::write(layout.cookie_dir().join("a.session"), b"{}").unwrap();

        uninstall(&layout, &sys, false).unwrap();
        assert!(!layout.service_file().exists());
        assert!(layout.config_file().exists() && layout.cookie_dir().exists());

        uninstall(&layout, &sys, true).unwrap();
        assert!(!layout.config_file().exists());
        assert!(!layout.cookie_dir().exists());
        assert!(!layout.cache_dir.exists());
        assert!(!layout.state_dir.exists());
    }

    #[test]
    fn uninstall_is_idempotent() {
        let (_d, layout, sys) = setup();
        uninstall(&layout, &sys, true).unwrap();
        uninstall(&layout, &sys, true).unwrap();
    }

    #[test]
    fn clearing_the_cache_restarts_only_a_service_that_was_running() {
        let (_d, layout, sys) = setup();
        let config = Config::for_layout(&layout);
        fs::create_dir_all(config.cache_dir.join("mirror")).unwrap();
        fs::write(config.cache_dir.join("mirror/file"), b"x").unwrap();

        assert!(!clear_cache(&layout, &config, &sys).unwrap());
        assert!(config.cache_dir.is_dir() && !config.cache_dir.join("mirror").exists());
        assert!(!sys.calls().iter().any(|c| c.starts_with("start")));

        *sys.active.lock().unwrap() = true;
        assert!(clear_cache(&layout, &config, &sys).unwrap());
        assert!(sys.calls().iter().any(|c| c.starts_with("start")));
    }

    // ---- desktop integration --------------------------------------------------------------------------

    #[test]
    fn the_status_unit_follows_the_main_service() {
        let unit = status_unit(Path::new("/opt/icloud/icloud-status")).unwrap();
        assert!(unit.contains("ExecStart=\"/opt/icloud/icloud-status\""));
        assert!(unit.contains("PartOf=icloud.service"), "stops and restarts with the main service");
        assert!(unit.contains("WantedBy=icloud.service"), "starts when the main service does");
        assert!(unit.contains("Restart=on-failure"));
        assert!(status_unit(Path::new("/x\ny")).is_err(), "no directive injection through a path");
    }

    #[test]
    fn the_status_service_is_written_enabled_and_removable() {
        let (_d, layout, sys) = setup();
        assert!(!status_service_installed(&layout));
        let unit = install_status_unit(&layout, &sys, Path::new("/opt/icloud-status")).unwrap();
        assert_eq!(unit, status_unit_file(&layout));
        assert!(status_service_installed(&layout));
        assert!(fs::read_to_string(&unit).unwrap().contains("/opt/icloud-status"));
        assert_eq!(sys.calls(), ["daemon-reload", "enable --now icloud-status.service"]);

        sys.calls.lock().unwrap().clear();
        remove_status_service(&layout, &sys).unwrap();
        assert!(!status_service_installed(&layout));
        assert_eq!(sys.calls(), ["disable --now icloud-status.service", "daemon-reload"]);

        sys.calls.lock().unwrap().clear();
        remove_status_service(&layout, &sys).unwrap();
        assert!(sys.calls().is_empty(), "nothing to remove means nothing to run");
    }

    #[test]
    fn a_failing_enable_is_reported() {
        let (_d, layout, sys) = setup();
        *sys.fail_on.lock().unwrap() = Some("enable".into());
        assert!(install_status_unit(&layout, &sys, Path::new("/x")).is_err());
    }

    #[test]
    fn uninstalling_removes_the_status_service_too() {
        let (_d, layout, sys) = setup();
        install_status_unit(&layout, &sys, Path::new("/x")).unwrap();
        uninstall(&layout, &sys, false).unwrap();
        assert!(!status_service_installed(&layout));
    }

    #[test]
    fn trackerignore_needs_a_mirror_and_is_idempotent() {
        let (_d, layout, _) = setup();
        let config = Config::for_layout(&layout);
        assert!(apply_trackerignore(&config).is_err());
        fs::create_dir_all(config.mirror_dir()).unwrap();
        assert!(apply_trackerignore(&config).unwrap());
        assert!(config.mirror_dir().join(TRACKER_IGNORE).exists());
        assert!(!apply_trackerignore(&config).unwrap(), "already in place");
    }

    // ---- doctor -----------------------------------------------------------------------------------------

    fn has(checks: &[Check], severity: Severity, needle: &str) -> bool {
        checks.iter().any(|c| c.severity == severity && c.message.contains(needle))
    }

    #[test]
    fn doctor_on_a_blank_machine_says_what_is_missing_and_how_to_fix_it() {
        let (_d, layout, sys) = setup();
        let checks = doctor(&layout, &sys);
        assert!(has(&checks, Severity::Missing, "not found"));
        assert!(checks.iter().any(|c| c.fix.as_deref().is_some_and(|f| f.contains("icloudctl init"))));
    }

    #[test]
    fn doctor_warns_about_a_world_readable_config_and_a_stored_password() {
        let (_d, layout, sys) = setup();
        init(&layout, &layout.home.join("iCloud"), Path::new("/d"), &sys).unwrap();
        configure(&layout, "me@example.com", Some(SecretString::from("pw".to_owned()))).unwrap();
        fs::set_permissions(layout.config_file(), fs::Permissions::from_mode(0o644)).unwrap();
        let checks = doctor(&layout, &sys);
        assert!(has(&checks, Severity::Warn, "readable by other users"));
        assert!(has(&checks, Severity::Info, "password is stored"));
    }

    #[test]
    fn doctor_reports_session_and_indexer_state() {
        let (_d, layout, sys) = setup();
        init(&layout, &layout.home.join("iCloud"), Path::new("/d"), &sys).unwrap();
        let config = Config::load(&layout.config_file(), &layout).unwrap();
        assert!(has(&doctor(&layout, &sys), Severity::Warn, "no iCloud session"));

        fs::create_dir_all(&config.cookie_dir).unwrap();
        fs::write(config.cookie_dir.join("me.session"), b"{}").unwrap();
        fs::create_dir_all(config.mirror_dir()).unwrap();
        let checks = doctor(&layout, &sys);
        assert!(has(&checks, Severity::Ok, "session is saved"));
        assert!(has(&checks, Severity::Warn, "indexer"));
        apply_trackerignore(&config).unwrap();
        assert!(has(&doctor(&layout, &sys), Severity::Ok, "excluded from desktop search"));
    }
}
