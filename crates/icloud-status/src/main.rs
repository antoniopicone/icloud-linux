//! `icloud-status`: keeps the label of the `iCloud` sidebar entry up to date
//! with what the daemon is doing (`iCloud (downloading: invoice.pdf)`).
//!
//! It follows the daemon's log and rewrites one line of the GTK bookmarks file,
//! which Nautilus and every GTK file dialog watch. Run as a user service:
//! `icloudctl status-install`. On `SIGTERM` it puts the plain label back.

use std::{
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use icloud_core::{
    Layout,
    status::{POLL_INTERVAL, Watcher},
};
use signal_hook::consts::{SIGINT, SIGTERM};

fn main() -> ExitCode {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).with_ansi(false).init();

    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(err) => {
            tracing::error!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let log = std::env::var_os("ICLOUD_LOG_PATH").map_or_else(|| layout.log_file(), PathBuf::from);

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        if let Err(err) = signal_hook::flag::register(signal, Arc::clone(&stop)) {
            tracing::error!("cannot install the signal handler: {err}");
            return ExitCode::FAILURE;
        }
    }

    tracing::info!("following {}", log.display());
    Watcher::new(layout, log).run(&stop, POLL_INTERVAL.min(Duration::from_secs(5)));
    ExitCode::SUCCESS
}
