//! What each command does.

use std::{
    path::PathBuf,
    process::Command as Process,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use icloud_core::{Config, Error, Layout, Result, connect::client_for, hydrate, setup, setup::Systemctl, sync_request};
use secrecy::SecretString;

use crate::{
    auth::{self, AuthOptions},
    cli::Command,
    prompt::{Prompter, confirm},
};

#[allow(missing_debug_implementations)]
pub struct Context<'a> {
    pub layout: Layout,
    pub sys: &'a dyn Systemctl,
    pub ui: &'a mut dyn Prompter,
}

fn load_config(ctx: &Context<'_>) -> Result<Config> {
    let path = ctx.layout.config_file();
    if !path.exists() {
        return Err(Error::Config(format!(
            "{} not found: run `icloudctl init` and `icloudctl configure`",
            path.display()
        )));
    }
    Config::load(&path, &ctx.layout)
}

pub fn run(command: Command, ctx: &mut Context<'_>) -> Result<()> {
    match command {
        Command::Quickstart { mount_dir } => quickstart(ctx, mount_dir),
        Command::Init { mount_dir } => init(ctx, mount_dir),
        Command::Configure { email, store_password } => configure(ctx, email, store_password),
        Command::Auth { force_sms, trust_token, debug } => {
            authenticate(ctx, &AuthOptions { force_sms, trust_token, debug })
        }
        Command::Start => {
            setup::start(&ctx.layout, ctx.sys)?;
            ctx.ui.say(&setup::status_text(ctx.sys));
            Ok(())
        }
        Command::Stop => {
            setup::stop(&ctx.layout, ctx.sys)?;
            ctx.ui.say("Stopped.");
            Ok(())
        }
        Command::Restart => {
            setup::restart(&ctx.layout, ctx.sys)?;
            ctx.ui.say(&setup::status_text(ctx.sys));
            Ok(())
        }
        Command::Refresh => {
            sync_request::signal_daemon(ctx.sys)?;
            ctx.ui.say("Asked the service to refresh. Watch progress with `icloudctl logs`.");
            Ok(())
        }
        Command::Sync { timeout, quiet } => sync(ctx, Duration::from_secs(timeout), quiet),
        Command::Hydrate { dry_run, verbose } => hydrate_command(ctx, dry_run, verbose),
        Command::Download { paths, notify } => {
            let from_file_manager = std::env::var("NAUTILUS_SCRIPT_SELECTED_FILE_PATHS").ok();
            download(ctx, paths, from_file_manager.as_deref(), notify)
        }
        Command::MenuInstall => {
            let script = setup::install_menu_here(&ctx.layout)?;
            ctx.ui.say(&format!(
                "Added \"{}\" to the right-click menu of Files: right-click a file or folder in iCloud Drive, \
                 then Scripts.\nInstalled {}",
                setup::MENU_ENTRY,
                script.display()
            ));
            Ok(())
        }
        Command::MenuUninstall => {
            setup::remove_menu(&ctx.layout)?;
            ctx.ui.say("Removed the right-click entry.");
            Ok(())
        }
        Command::Status => {
            ctx.ui.say(&setup::status_text(ctx.sys));
            Ok(())
        }
        Command::Logs => logs(),
        Command::Doctor => doctor(ctx),
        Command::ClearCache { yes } => clear_cache(ctx, yes),
        Command::StatusInstall => {
            setup::install_status_service(&ctx.layout, ctx.sys)?;
            ctx.ui.say("The Files sidebar now shows what iCloud is doing next to its name.");
            Ok(())
        }
        Command::StatusUninstall => {
            setup::remove_status_service(&ctx.layout, ctx.sys)?;
            ctx.ui.say("Removed the sidebar status service.");
            Ok(())
        }
        Command::Trackerignore => {
            let config = load_config(ctx)?;
            if setup::apply_trackerignore(&config)? {
                ctx.ui.say("Excluded the mount from desktop search indexing.");
            } else {
                ctx.ui.say("Already excluded from desktop search indexing.");
            }
            Ok(())
        }
        Command::Uninstall { purge, yes } => uninstall(ctx, purge, yes),
    }
}

fn init(ctx: &mut Context<'_>, mount_dir: Option<PathBuf>) -> Result<()> {
    let mount = mount_dir.unwrap_or_else(|| ctx.layout.default_mount());
    let report = setup::init(&ctx.layout, &mount, &setup::locate_daemon()?, ctx.sys)?;
    if report.used_fallback {
        ctx.ui.say(&format!("{} is not writable; using {} instead.", mount.display(), report.mount_dir.display()));
    }
    ctx.ui.say("Initialized.");
    ctx.ui.say(&format!("- Config:  {}", ctx.layout.config_file().display()));
    ctx.ui.say(&format!("- Mount:   {}", report.mount_dir.display()));
    ctx.ui.say(&format!("- Service: {}", report.service_file.display()));
    ctx.ui.say("Next: icloudctl configure, then icloudctl auth");
    Ok(())
}

fn configure(ctx: &mut Context<'_>, email: Option<String>, store_password: bool) -> Result<()> {
    let email = match email {
        Some(email) => email,
        None => ctx.ui.line("Apple ID: ")?,
    };
    let password = if store_password {
        ctx.ui.say("The password will be saved in config.yaml (mode 0600).");
        Some(SecretString::from(ctx.ui.secret("Apple ID password (input hidden): ")?))
    } else {
        None
    };
    setup::configure(&ctx.layout, &email, password)?;
    ctx.ui.say(&format!("Saved {}", ctx.layout.config_file().display()));
    if !store_password {
        ctx.ui.say("The password is not stored; you will be asked for it when you sign in.");
    }
    Ok(())
}

fn authenticate(ctx: &mut Context<'_>, options: &AuthOptions) -> Result<()> {
    let config = load_config(ctx)?;
    let client = client_for(&config)?;
    auth::run(&client, &config, options, ctx.ui)?;
    ctx.ui.say(&format!("Session saved under {}", config.cookie_dir.display()));
    Ok(())
}

fn quickstart(ctx: &mut Context<'_>, mount_dir: Option<PathBuf>) -> Result<()> {
    init(ctx, mount_dir)?;
    ctx.ui.say("\nNow your Apple ID:");
    configure(ctx, None, false)?;
    ctx.ui.say("\nSigning in:");
    authenticate(ctx, &AuthOptions::default())?;
    ctx.ui.say("\nStarting the service…");
    setup::start(&ctx.layout, ctx.sys)?;
    ctx.ui.say(&setup::status_text(ctx.sys));
    match setup::install_menu_here(&ctx.layout) {
        Ok(_) => ctx.ui.say(&format!(
            "\nRight-click a file in iCloud Drive, then Scripts > {}, to keep it on this computer.",
            setup::MENU_ENTRY
        )),
        Err(err) => ctx.ui.say(&format!("\n(The right-click entry was not added: {err})")),
    }
    Ok(())
}

fn sync(ctx: &mut Context<'_>, timeout: Duration, quiet: bool) -> Result<()> {
    if !quiet {
        ctx.ui.say(&format!("Refreshing from iCloud (waiting up to {}s)…", timeout.as_secs()));
    }
    let took = sync_request::request_sync(&ctx.layout, ctx.sys, timeout)?;
    if !quiet {
        ctx.ui.say(&format!("Done in {}s.", took.as_secs()));
    }
    Ok(())
}

fn hydrate_command(ctx: &mut Context<'_>, dry_run: bool, verbose: bool) -> Result<()> {
    let config = load_config(ctx)?;
    let plan = hydrate::plan(&config)?;
    ctx.ui.say(&format!("Eligible files: {}", plan.eligible()));
    ctx.ui.say(&format!("Already local:  {}", plan.already_local));
    ctx.ui.say(&format!("To download:    {} ({})", plan.pending.len(), format_size(plan.pending_bytes())));

    if plan.pending.is_empty() {
        ctx.ui.say("Nothing to do.");
        return Ok(());
    }
    if dry_run {
        ctx.ui.say("\nDry run; would download:");
        for (path, size) in &plan.pending {
            ctx.ui.say(&format!("  {:>10}  {path}", format_size(*size)));
        }
        return Ok(());
    }

    let mount = mounted_dir(ctx, &config)?;

    let cancel = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&cancel))?;
    ctx.ui.say("\nDownloading; press Ctrl-C to pause (safe to re-run).");
    let mut last_report = std::time::Instant::now();
    let ui = &mut *ctx.ui;
    let report = hydrate::run(&mount, &plan, &cancel, |path, progress| {
        if verbose {
            ui.say(&format!("  [{}/{}] {path}", progress.done, progress.total));
        }
        if progress.done == progress.total
            || progress.done % 25 == 0
            || last_report.elapsed() >= Duration::from_secs(30)
        {
            let eta = progress.eta().map_or_else(String::new, |eta| format!("  ETA ~{}", format_duration(eta)));
            ui.say(&format!("  {}/{} files ({} failed){eta}", progress.done, progress.total, progress.failed));
            last_report = std::time::Instant::now();
        }
    });

    ctx.ui.say(&format!("\nDownloaded {} file(s).", report.succeeded));
    if report.interrupted {
        ctx.ui.say("Interrupted; run again to continue.");
    }
    if report.failed.is_empty() {
        return Ok(());
    }
    for (path, why) in report.failed.iter().take(10) {
        ctx.ui.say(&format!("  failed: {path}: {why}"));
    }
    Err(Error::Setup(format!(
        "{} file(s) could not be downloaded; re-run to retry, or see `icloudctl logs`",
        report.failed.len()
    )))
}

/// Where iCloud Drive is mounted, provided it really is.
fn mounted_dir(ctx: &Context<'_>, config: &Config) -> Result<PathBuf> {
    let mount = config
        .mount_dir
        .clone()
        .or_else(|| setup::recorded_mount(&ctx.layout))
        .unwrap_or_else(|| ctx.layout.default_mount());
    if setup::is_mounted(&mount) {
        Ok(mount)
    } else {
        Err(Error::Setup(format!(
            "nothing is mounted at {}: start the service with `icloudctl start`",
            mount.display()
        )))
    }
}

/// A desktop notification, when asked for and possible. The right-click menu
/// runs without a terminal, so this is the only way it can report anything.
fn notify(enabled: bool, title: &str, body: &str) {
    if enabled {
        let _ = Process::new("notify-send")
            .args(["--app-name=iCloud", "--icon=folder-download-symbolic", "--", title, body])
            .status();
    }
}

/// `icloudctl download`: fetch what the user picked, and only that.
fn download(
    ctx: &mut Context<'_>,
    paths: Vec<PathBuf>,
    from_file_manager: Option<&str>,
    notify_user: bool,
) -> Result<()> {
    let outcome = download_selection(ctx, paths, from_file_manager, notify_user);
    if let Err(err) = &outcome {
        notify(notify_user, "iCloud download failed", &err.to_string());
    }
    outcome
}

fn download_selection(
    ctx: &mut Context<'_>,
    paths: Vec<PathBuf>,
    from_file_manager: Option<&str>,
    notify_user: bool,
) -> Result<()> {
    let picked: Vec<PathBuf> = if paths.is_empty() {
        from_file_manager.unwrap_or_default().lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect()
    } else {
        paths
    };
    if picked.is_empty() {
        return Err(Error::Setup("nothing selected: give files or folders inside iCloud Drive".into()));
    }

    let config = load_config(ctx)?;
    let mount = mounted_dir(ctx, &config)?;
    let cwd = std::env::current_dir()?;
    let selection = hydrate::resolve_selection(&mount, &picked, &cwd)?;
    if selection.files.is_empty() {
        return Err(Error::Setup(if selection.outside.is_empty() {
            "there is no file to download in the selection".into()
        } else {
            format!("not inside iCloud Drive ({}): nothing to download", mount.display())
        }));
    }
    for item in &selection.outside {
        ctx.ui.say(&format!("Skipped {}: not inside iCloud Drive.", item.display()));
    }

    let plan = hydrate::plan_files(&config, &selection.files)?;
    if plan.excluded > 0 {
        ctx.ui.say(&format!("Skipped {} file(s) outside the synchronisation boundary (sync_paths).", plan.excluded));
    }
    if plan.pending.is_empty() {
        ctx.ui.say("Already on this computer.");
        notify(notify_user, "Already on this computer", &format!("{} file(s)", plan.already_local));
        return Ok(());
    }

    let summary = format!("{} file(s), {}", plan.pending.len(), format_size(plan.pending_bytes()));
    ctx.ui.say(&format!("Downloading {summary}…"));
    notify(notify_user, "Downloading from iCloud", &summary);

    let cancel = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&cancel))?;
    }
    let ui = &mut *ctx.ui;
    let report = hydrate::run(&mount, &plan, &cancel, |path, progress| {
        ui.say(&format!("  [{}/{}] {path}", progress.done, progress.total));
    });

    forget_failed_thumbnails(ctx, &mount, &plan, &report);

    if report.failed.is_empty() && !report.interrupted {
        ctx.ui.say(&format!("Downloaded {} file(s).", report.succeeded));
        notify(notify_user, "Downloaded from iCloud", &summary);
        return Ok(());
    }
    for (path, why) in report.failed.iter().take(10) {
        ctx.ui.say(&format!("  failed: {path}: {why}"));
    }
    let message = if report.interrupted {
        format!("interrupted after {} file(s); run it again to continue", report.succeeded)
    } else {
        format!(
            "{} of {} file(s) could not be downloaded; see `icloudctl logs`",
            report.failed.len(),
            plan.pending.len()
        )
    };
    Err(Error::Setup(message))
}

/// Files that were turned away from a thumbnailer while they were not
/// downloaded are remembered as "failed"; now that they are here, let the file
/// manager try again.
fn forget_failed_thumbnails(
    ctx: &Context<'_>,
    mount: &std::path::Path,
    plan: &hydrate::Plan,
    report: &hydrate::Report,
) {
    let Some(cache_home) = ctx.layout.cache_dir.parent() else { return };
    let real_mount = std::fs::canonicalize(mount).unwrap_or_else(|_| mount.to_owned());
    let mut files = Vec::new();
    for (path, _) in plan.pending.iter().filter(|(p, _)| !report.failed.iter().any(|(f, _)| f == p)) {
        let relative = path.as_str().trim_start_matches('/');
        files.push(mount.join(relative));
        if real_mount != mount {
            files.push(real_mount.join(relative));
        }
    }
    let cleared = icloud_core::thumbnails::forget_failures(cache_home, &files);
    if cleared > 0 {
        tracing::debug!("cleared {cleared} thumbnail failure note(s)");
    }
}

fn logs() -> Result<()> {
    let status = Process::new("journalctl").args(["--user", "-u", setup::SERVICE_NAME, "-f"]).status()?;
    if status.success() { Ok(()) } else { Err(Error::Setup("journalctl failed".into())) }
}

fn doctor(ctx: &mut Context<'_>) -> Result<()> {
    ctx.ui.say("== icloud-linux doctor ==");
    let mut problems = 0;
    for check in setup::doctor(&ctx.layout, ctx.sys) {
        let label = match check.severity {
            setup::Severity::Ok => "OK",
            setup::Severity::Info => "INFO",
            setup::Severity::Warn => {
                problems += 1;
                "WARN"
            }
            setup::Severity::Missing => {
                problems += 1;
                "MISSING"
            }
        };
        ctx.ui.say(&format!("{label}: {}", check.message));
        if let Some(fix) = &check.fix {
            ctx.ui.say(&format!("      fix: {fix}"));
        }
    }
    if problems == 0 { Ok(()) } else { Err(Error::Setup(format!("{problems} problem(s) found"))) }
}

fn clear_cache(ctx: &mut Context<'_>, yes: bool) -> Result<()> {
    let config = load_config(ctx)?;
    if !yes
        && !confirm(ctx.ui, &format!("Delete {} ? Files are downloaded again on demand.", config.cache_dir.display()))
    {
        ctx.ui.say("Cancelled.");
        return Ok(());
    }
    // Local changes that were never uploaded live only in the cache.
    if setup::is_active(ctx.sys) {
        ctx.ui.say("Note: changes not yet uploaded to iCloud are lost with the cache.");
    }
    let restarted = setup::clear_cache(&ctx.layout, &config, ctx.sys)?;
    ctx.ui.say(&format!("Cleared {}.", config.cache_dir.display()));
    ctx.ui.say(if restarted {
        "The service was restarted and rebuilds the cache."
    } else {
        "Start the service to rebuild the cache."
    });
    Ok(())
}

fn uninstall(ctx: &mut Context<'_>, purge: bool, yes: bool) -> Result<()> {
    if purge
        && !yes
        && !confirm(ctx.ui, "This deletes your configuration, saved session and the whole local cache. Continue?")
    {
        ctx.ui.say("Cancelled.");
        return Ok(());
    }
    setup::uninstall(&ctx.layout, ctx.sys, purge)?;
    if purge {
        ctx.ui.say("Uninstalled and purged local configuration, session and cache.");
    } else {
        ctx.ui.say("Uninstalled the service. Configuration, session and cache are kept.");
        ctx.ui.say("Use `icloudctl uninstall --purge` to remove them too.");
    }
    Ok(())
}

/// `1234567` → `1.2 MiB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 120 { format!("{secs}s") } else { format!("{}m {}s", secs / 60, secs % 60) }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Mutex};

    use icloud_core::setup::CommandOutput;

    use super::*;
    use crate::prompt::testing::Script;

    #[derive(Default)]
    struct FakeSystemctl {
        calls: Mutex<Vec<String>>,
        active: bool,
    }

    impl Systemctl for FakeSystemctl {
        fn run(&self, args: &[&str]) -> std::io::Result<CommandOutput> {
            let line = args.join(" ");
            self.calls.lock().unwrap().push(line.clone());
            let ok = !line.starts_with("is-active") || self.active;
            Ok(CommandOutput { success: ok, stdout: "status text".into(), stderr: String::new() })
        }
    }

    fn ctx_parts() -> (tempfile::TempDir, FakeSystemctl) {
        (tempfile::tempdir().unwrap(), FakeSystemctl::default())
    }

    fn run_command(dir: &tempfile::TempDir, sys: &FakeSystemctl, ui: &mut Script, command: Command) -> Result<()> {
        let mut ctx = Context { layout: Layout::under(dir.path()), sys, ui };
        run(command, &mut ctx)
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1_572_864), "1.5 MiB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert!(format_size(u64::MAX).ends_with("TiB"));
    }

    #[test]
    fn durations_switch_to_minutes_after_two() {
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn configure_without_the_flag_never_asks_for_a_password_and_never_stores_one() {
        let (dir, sys) = ctx_parts();
        let mut ui = Script::new(&["me@example.com"]);
        run_command(&dir, &sys, &mut ui, Command::Configure { email: None, store_password: false }).unwrap();
        assert_eq!(ui.asked, ["Apple ID: "], "the password must not even be requested");
        let saved = fs::read_to_string(Layout::under(dir.path()).config_file()).unwrap();
        assert!(saved.contains("me@example.com") && !saved.contains("password"));
        assert!(ui.said_contains("not stored"));
    }

    #[test]
    fn configure_with_the_flag_stores_the_password() {
        let (dir, sys) = ctx_parts();
        let mut ui = Script::new(&["hunter2"]);
        run_command(
            &dir,
            &sys,
            &mut ui,
            Command::Configure { email: Some("me@example.com".into()), store_password: true },
        )
        .unwrap();
        let saved = fs::read_to_string(Layout::under(dir.path()).config_file()).unwrap();
        assert!(saved.contains("hunter2"));
        assert!(ui.said_contains("0600"));
    }

    #[test]
    fn commands_that_need_a_config_say_how_to_get_one() {
        let (dir, sys) = ctx_parts();
        let err = run_command(&dir, &sys, &mut Script::new(&[]), Command::Hydrate { dry_run: true, verbose: false })
            .unwrap_err();
        assert!(err.to_string().contains("icloudctl init"), "{err}");
    }

    #[test]
    fn clearing_the_cache_asks_first_and_respects_no() {
        let (dir, sys) = ctx_parts();
        let layout = Layout::under(dir.path());
        Config::for_layout(&layout).save(&layout.config_file()).unwrap();
        fs::create_dir_all(layout.cache_dir.join("mirror")).unwrap();
        let mut ui = Script::new(&["n"]);
        run_command(&dir, &sys, &mut ui, Command::ClearCache { yes: false }).unwrap();
        assert!(layout.cache_dir.join("mirror").exists());
        assert!(ui.said_contains("Cancelled"));

        let mut ui = Script::new(&[]);
        run_command(&dir, &sys, &mut ui, Command::ClearCache { yes: true }).unwrap();
        assert!(!layout.cache_dir.join("mirror").exists());
    }

    #[test]
    fn a_purging_uninstall_asks_first() {
        let (dir, sys) = ctx_parts();
        let layout = Layout::under(dir.path());
        Config::for_layout(&layout).save(&layout.config_file()).unwrap();
        run_command(&dir, &sys, &mut Script::new(&["no"]), Command::Uninstall { purge: true, yes: false }).unwrap();
        assert!(layout.config_file().exists());
        run_command(&dir, &sys, &mut Script::new(&["yes"]), Command::Uninstall { purge: true, yes: false }).unwrap();
        assert!(!layout.config_file().exists());
    }

    #[test]
    fn a_plain_uninstall_does_not_ask_and_keeps_data() {
        let (dir, sys) = ctx_parts();
        let layout = Layout::under(dir.path());
        Config::for_layout(&layout).save(&layout.config_file()).unwrap();
        run_command(&dir, &sys, &mut Script::new(&[]), Command::Uninstall { purge: false, yes: false }).unwrap();
        assert!(layout.config_file().exists());
    }

    #[test]
    fn hydrate_dry_run_lists_files_and_downloads_nothing() {
        let (dir, sys) = ctx_parts();
        let layout = Layout::under(dir.path());
        Config::for_layout(&layout).save(&layout.config_file()).unwrap();
        let config = Config::load(&layout.config_file(), &layout).unwrap();
        let state = icloud_core::SyncState::open(&config.state_db()).unwrap();
        let mut entry = icloud_core::Entry::local(icloud_core::IcPath::new("/big.bin"), icloud_api::NodeKind::File, 1);
        entry.size = 3 * 1024 * 1024;
        entry.hydrated = false;
        entry.dirty = false;
        state.upsert_entry(&entry).unwrap();

        let mut ui = Script::new(&[]);
        run_command(&dir, &sys, &mut ui, Command::Hydrate { dry_run: true, verbose: false }).unwrap();
        assert!(ui.said_contains("To download:    1 (3.0 MiB)"));
        assert!(ui.said_contains("/big.bin"));
    }

    #[test]
    fn hydrate_refuses_to_run_without_a_mount() {
        let (dir, sys) = ctx_parts();
        let layout = Layout::under(dir.path());
        Config::for_layout(&layout).save(&layout.config_file()).unwrap();
        let config = Config::load(&layout.config_file(), &layout).unwrap();
        let state = icloud_core::SyncState::open(&config.state_db()).unwrap();
        let mut entry = icloud_core::Entry::local(icloud_core::IcPath::new("/f"), icloud_api::NodeKind::File, 1);
        entry.hydrated = false;
        entry.dirty = false;
        state.upsert_entry(&entry).unwrap();
        let err = run_command(&dir, &sys, &mut Script::new(&[]), Command::Hydrate { dry_run: false, verbose: false })
            .unwrap_err();
        assert!(err.to_string().contains("nothing is mounted"), "{err}");
    }

    #[test]
    fn download_needs_a_selection_and_a_mount() {
        let (dir, sys) = ctx_parts();
        let layout = Layout::under(dir.path());
        Config::for_layout(&layout).save(&layout.config_file()).unwrap();

        let none = Command::Download { paths: vec![], notify: false };
        let err = run_command(&dir, &sys, &mut Script::new(&[]), none).unwrap_err();
        assert!(err.to_string().contains("nothing selected"), "{err}");

        let some = Command::Download { paths: vec!["/somewhere/file.pdf".into()], notify: false };
        let err = run_command(&dir, &sys, &mut Script::new(&[]), some).unwrap_err();
        assert!(err.to_string().contains("nothing is mounted"), "{err}");
    }

    #[test]
    fn download_without_a_config_says_how_to_get_one() {
        let (dir, sys) = ctx_parts();
        let some = Command::Download { paths: vec!["f".into()], notify: false };
        let err = run_command(&dir, &sys, &mut Script::new(&[]), some).unwrap_err();
        assert!(err.to_string().contains("icloudctl init"), "{err}");
    }

    #[test]
    fn refresh_and_sync_need_a_running_service() {
        let (dir, sys) = ctx_parts();
        let err = run_command(&dir, &sys, &mut Script::new(&[]), Command::Refresh).unwrap_err();
        assert!(err.to_string().contains("not running"));
        let err =
            run_command(&dir, &sys, &mut Script::new(&[]), Command::Sync { timeout: 1, quiet: true }).unwrap_err();
        assert!(err.to_string().contains("not running"));
    }

    #[test]
    fn stop_and_status_talk_to_systemd() {
        let (dir, sys) = ctx_parts();
        let mut ui = Script::new(&[]);
        run_command(&dir, &sys, &mut ui, Command::Stop).unwrap();
        run_command(&dir, &sys, &mut ui, Command::Status).unwrap();
        let calls = sys.calls.lock().unwrap().clone();
        assert!(calls.iter().any(|c| c == "stop icloud.service"));
        assert!(ui.said_contains("status text"));
    }
}
