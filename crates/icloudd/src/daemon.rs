//! Starting the daemon: connect, sync, mount, and shut down cleanly.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime},
};

use fuser::{Config as MountConfig, MountOption, Session, SessionACL};
use icloud_core::{
    Config, Engine, EngineConfig, FsCore, FsOptions, Layout, Mirror, SyncPolicy, SyncState,
    connect::{Connection, connect},
    notify::{DesktopNotifier, FailureReporter},
    setup::cleanup_mountpoint,
    thumbnails,
};
use signal_hook::{
    consts::{SIGINT, SIGTERM, SIGUSR1},
    iterator::Signals,
};

use crate::adapter::IcloudFs;

/// Threads serving kernel requests. Several are needed so that one request
/// blocked on a large download does not freeze every other one.
const FUSE_THREADS: usize = 16;
/// How often the web session is renewed. icloud.com does it every few minutes
/// from an open tab; a session left alone lapses within about half an hour,
/// and the next file opened would wait for it to be renewed first.
const SESSION_RENEW_INTERVAL: Duration = Duration::from_secs(300);
/// Poll slice for the session keep-alive thread, so shutdown is prompt and
/// wake-from-sleep or a fresh `icloudctl auth` on disk is noticed quickly.
const SESSION_POLL_SLICE: Duration = Duration::from_secs(5);
/// Initial retry delay when a renewal fails due to a transient network outage
/// (e.g. Wi-Fi reconnecting after boot or laptop wake).
const SESSION_RETRY_INITIAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Options {
    pub config: PathBuf,
    pub mountpoint: PathBuf,
    pub debug: bool,
}

/// What the daemon writes to the sync marker, and `icloudctl sync` reads.
pub const SYNC_OK: &str = "ok";
pub const SYNC_ERROR_PREFIX: &str = "error: ";

pub fn run(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
    let layout = Layout::from_env()?;
    let config = Config::load(&options.config, &layout)?;

    // Signal handlers first: a `SIGUSR1` arriving while the cache is being
    // reconciled must not kill the process.
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGUSR1])?;

    let mirror = Arc::new(Mirror::open(&config.cache_dir)?);
    let state = Arc::new(SyncState::open(&config.state_db())?);
    let policy = SyncPolicy::new(&config.sync_paths, &config.exclude_paths);

    let (engine, client, icloud_session, started_offline) = match connect(&config)? {
        Connection::Ready { drive, session: icloud_session, client, started_offline } => {
            let engine =
                Engine::new(drive, mirror.clone(), state.clone(), policy.clone(), EngineConfig::from_config(&config));
            // Whenever a file is hydrated (opened directly or via `icloudctl download`),
            // clear any stale thumbnail-failure note so Files draws its preview.
            if let Some(cache_home) = layout.cache_dir.parent().map(Path::to_path_buf) {
                let mount = options.mountpoint.clone();
                let real_mount = fs::canonicalize(&mount).unwrap_or_else(|_| mount.clone());
                engine.set_on_hydrated(move |path| {
                    let relative = path.as_str().trim_start_matches('/');
                    let mut files = vec![mount.join(relative)];
                    if real_mount != mount {
                        files.push(real_mount.join(relative));
                    }
                    let cleared = thumbnails::forget_failures(&cache_home, &files);
                    if cleared > 0 {
                        tracing::debug!("cleared {cleared} thumbnail failure note(s) for {path}");
                    }
                });
            }
            engine.start()?;
            (Some(engine), Some(client), Some(icloud_session), started_offline)
        }
        Connection::Unauthenticated { reason } => {
            tracing::error!(
                "starting UNAUTHENTICATED: {reason}. The cache is served read-only. \
                 Run `icloudctl auth` to sign in again."
            );
            (None, None, None, false)
        }
    };

    let core = Arc::new(
        FsCore::new(
            mirror,
            state,
            policy,
            engine.clone(),
            FsOptions { read_only: config.fuse_options.ro, preview_max_bytes: config.preview_max_bytes },
        )
        .with_failure_reporter(FailureReporter::new(DesktopNotifier::default())),
    );
    let filesystem = IcloudFs::new(core);

    cleanup_mountpoint(&options.mountpoint)?;
    let mut session = Session::new(filesystem, &options.mountpoint, &mount_config(&config))?;
    let mut unmounter = session.unmount_callable();
    tracing::info!("mounted iCloud Drive at {}", options.mountpoint.display());

    // Signals: stop on TERM/INT, refresh on USR1.
    let marker = layout.sync_marker();
    let stop_engine = engine.clone();
    thread::Builder::new().name("icloud-signals".into()).spawn(move || {
        for signal in signals.forever() {
            if signal == SIGUSR1 {
                spawn_on_demand_sync(stop_engine.clone(), marker.clone());
                continue;
            }
            tracing::info!("received signal {signal}, shutting down");
            if let Some(engine) = &stop_engine {
                engine.shutdown();
            }
            if let Err(err) = unmounter.unmount() {
                tracing::error!("could not unmount: {err}");
            }
            break;
        }
    })?;

    // Keep the web session alive, react to laptop wake-from-sleep, reconnect
    // after offline startup, and pick up a fresh `icloudctl auth` written to disk.
    let session_thread = if let (Some(icloud_session), Some(session_engine), Some(session_client)) =
        (icloud_session, engine.clone(), client.clone())
    {
        Some(thread::Builder::new().name("icloud-session".into()).spawn(move || {
            run_session_keepalive(&session_engine, &icloud_session, &session_client, started_offline);
        })?)
    } else {
        None
    };

    let outcome = session.run();

    if let Some(engine) = &engine {
        engine.shutdown_and_wait(Duration::from_secs(2));
    }
    if let Some(handle) = session_thread {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
    if let Some(Err(err)) = client.as_ref().map(|c| c.session().flush()) {
        tracing::warn!("could not save the iCloud session: {err}");
    }
    outcome.map_err(Into::into)
}

fn run_session_keepalive(
    engine: &Engine,
    icloud_session: &icloud_api::RenewingDrive<icloud_api::HttpDrive>,
    client: &icloud_api::Client,
    started_offline: bool,
) {
    let mut transient_failures: u32 = 0;
    let mut next_wait = if started_offline { SESSION_RETRY_INITIAL } else { SESSION_RENEW_INTERVAL };
    let mut last_attempt = Instant::now();
    let mut last_wall = SystemTime::now();

    while !engine.is_stopped() {
        thread::sleep(SESSION_POLL_SLICE);
        if engine.is_stopped() {
            break;
        }
        let now_wall = SystemTime::now();
        let wall_elapsed = now_wall.duration_since(last_wall).unwrap_or(Duration::ZERO);
        let woke_from_sleep = wall_elapsed >= SESSION_RENEW_INTERVAL;
        let newer_on_disk = client.session().has_newer_on_disk();

        // Once auth is known to be lost, stop polling Apple every few minutes;
        // wait until a fresh sign-in writes newer cookies to disk.
        if engine.auth_lost() && !newer_on_disk {
            last_wall = now_wall;
            continue;
        }

        let due = newer_on_disk || woke_from_sleep || last_attempt.elapsed() >= next_wait;
        if !due {
            continue;
        }
        if woke_from_sleep && !newer_on_disk {
            tracing::info!(
                "system clock jumped by {}s; proactively renewing the iCloud session",
                wall_elapsed.as_secs()
            );
        }

        last_attempt = Instant::now();
        last_wall = now_wall;
        match icloud_session.renew_now() {
            Ok(()) => {
                engine.clear_auth_lost();
                transient_failures = 0;
                next_wait = SESSION_RENEW_INTERVAL;
            }
            Err(err) if err.is_auth() => {
                tracing::warn!("could not renew the iCloud session: {err}");
                engine.mark_auth_lost();
                transient_failures = 0;
                next_wait = SESSION_RENEW_INTERVAL;
            }
            Err(err) => {
                // Transient network/server issue: exponential backoff from 15s up to 5m.
                transient_failures = transient_failures.saturating_add(1);
                let mult = 1u32 << transient_failures.saturating_sub(1).min(5);
                next_wait = (SESSION_RETRY_INITIAL * mult).min(SESSION_RENEW_INTERVAL);
                tracing::warn!("could not renew the iCloud session ({err}); retrying in {}s", next_wait.as_secs());
            }
        }
    }
}

/// `SIGUSR1`: refresh from iCloud now, then tell `icloudctl sync` how it went.
fn spawn_on_demand_sync(engine: Option<Engine>, marker: PathBuf) {
    let spawned = thread::Builder::new().name("icloud-on-demand-sync".into()).spawn(move || {
        let outcome = match &engine {
            None => Err("not signed in to iCloud".to_owned()),
            Some(engine) => {
                tracing::info!("SIGUSR1: refreshing from iCloud");
                engine.refresh_blocking().map_err(|e| e.to_string())
            }
        };
        let text = match &outcome {
            Ok(()) => {
                tracing::info!("SIGUSR1: refresh complete");
                SYNC_OK.to_owned()
            }
            Err(err) => {
                tracing::error!("SIGUSR1: refresh failed: {err}");
                format!("{SYNC_ERROR_PREFIX}{err}")
            }
        };
        write_marker(&marker, &text);
    });
    if let Err(err) = spawned {
        tracing::error!("could not start the on-demand sync: {err}");
    }
}

fn write_marker(marker: &Path, text: &str) {
    if let Some(dir) = marker.parent() {
        let _ = icloud_core::setup::ensure_private_dir(dir);
    }
    if let Err(err) = fs::write(marker, text) {
        tracing::error!("could not write {}: {err}", marker.display());
    }
}

/// The "device" the mount is listed under. `GLib` (hence Files, GNOME's file
/// manager) hides mounts whose device is `none` from its list of drives, which
/// is what we want: otherwise the sidebar shows iCloud a second time as a
/// removable disk, with a disk icon and an eject button, next to the plain
/// folder entry that carries the activity label.
const MOUNT_SOURCE: &str = "none";

fn mount_config(config: &Config) -> MountConfig {
    let mut mount = MountConfig::default();
    mount.mount_options = vec![MountOption::FSName(MOUNT_SOURCE.into()), MountOption::Subtype("icloud".into())];
    if config.fuse_options.ro {
        mount.mount_options.push(MountOption::RO);
    }
    if config.fuse_options.allow_other {
        mount.acl = SessionACL::All;
    }
    mount.n_threads = Some(FUSE_THREADS);
    mount.clone_fd = true;
    mount
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mount_is_private_and_read_write_by_default() {
        let mount = mount_config(&Config::default());
        assert_eq!(mount.acl, SessionACL::Owner);
        assert!(!mount.mount_options.contains(&MountOption::RO));
        assert!(
            mount.mount_options.contains(&MountOption::FSName("none".into())),
            "GLib does not list a mount whose device is `none` as a disk"
        );
        assert!(mount.mount_options.contains(&MountOption::Subtype("icloud".into())));
        assert_eq!(mount.n_threads, Some(FUSE_THREADS));
    }

    #[test]
    fn options_from_the_config_reach_the_mount() {
        let mut config = Config::default();
        config.fuse_options.ro = true;
        config.fuse_options.allow_other = true;
        let mount = mount_config(&config);
        assert!(mount.mount_options.contains(&MountOption::RO));
        assert_eq!(mount.acl, SessionACL::All);
    }

    #[test]
    fn the_sync_marker_carries_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("state/sync_done");
        write_marker(&marker, SYNC_OK);
        assert_eq!(fs::read_to_string(&marker).unwrap(), "ok");
        write_marker(&marker, &format!("{SYNC_ERROR_PREFIX}boom"));
        assert_eq!(fs::read_to_string(&marker).unwrap(), "error: boom");
    }
}
