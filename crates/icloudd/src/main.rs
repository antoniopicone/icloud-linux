//! `icloudd`: mounts iCloud Drive as a FUSE filesystem.

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use icloud_core::Layout;
use icloudd::{daemon, logging};

#[derive(Debug, Parser)]
#[command(version, about = "Mount iCloud Drive as a local-first FUSE filesystem")]
struct Args {
    /// Path to config.yaml.
    #[arg(short, long, default_value_os_t = default_config())]
    config: PathBuf,

    /// Log debug messages too.
    #[arg(short = 'v', long)]
    debug: bool,

    /// Accepted for compatibility with the Python daemon's command line; the
    /// daemon always runs in the foreground.
    #[arg(short = 'f', hide = true)]
    _foreground: bool,

    /// Where to mount iCloud Drive.
    mountpoint: PathBuf,
}

fn default_config() -> PathBuf {
    Layout::from_env().map_or_else(|_| PathBuf::from("config.yaml"), |layout| layout.config_file())
}

fn main() -> ExitCode {
    let args = Args::parse();
    let log_path = std::env::var_os("ICLOUD_LOG_PATH").map_or_else(
        || Layout::from_env().map_or_else(|_| PathBuf::from("icloud.log"), |layout| layout.log_file()),
        PathBuf::from,
    );
    logging::init(args.debug, &log_path);

    let options = daemon::Options { config: args.config, mountpoint: args.mountpoint, debug: args.debug };
    match daemon::run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            ExitCode::FAILURE
        }
    }
}
