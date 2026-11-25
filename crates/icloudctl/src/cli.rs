//! Command line grammar.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "icloudctl", version, about = "Control the icloud-linux service", propagate_version = true)]
pub struct Cli {
    /// Log what happens on the wire (addresses and status codes only; never
    /// passwords, tokens or cookies). Same as `ICLOUD_LOG=debug`.
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// One-command guided setup: init, sign in on icloud.com, start.
    Quickstart {
        /// Where to mount iCloud Drive (default: ~/iCloud).
        mount_dir: Option<PathBuf>,
    },
    /// Create directories, a default configuration and the systemd user service.
    Init {
        /// Where to mount iCloud Drive (default: ~/iCloud).
        mount_dir: Option<PathBuf>,
    },
    /// Sign in on icloud.com in a browser window, and restart the service
    /// with the new session if it is running.
    ///
    /// The Apple ID, the password and the verification code are typed on
    /// Apple's page and never reach this program. Needs a Chromium-based
    /// browser.
    Auth {
        /// Report the outcome as a desktop notification. This is how the
        /// "session expired" notification runs it.
        #[arg(long)]
        notify: bool,
    },
    /// Start the service.
    Start,
    /// Stop the service and unmount.
    Stop,
    /// Restart the service.
    Restart,
    /// Ask the running service to refresh from iCloud, without waiting.
    Refresh,
    /// Refresh from iCloud now and wait until it is done.
    Sync {
        /// Seconds to wait.
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        /// No progress output.
        #[arg(long)]
        quiet: bool,
    },
    /// Download every eligible file that is not local yet.
    Hydrate {
        /// Show what would be downloaded, and download nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print each file.
        #[arg(long)]
        verbose: bool,
    },
    /// Download files or folders from iCloud Drive to this computer now.
    ///
    /// This is what "Download from iCloud" in the right-click menu runs.
    /// Nothing else downloads a file's contents except opening it.
    Download {
        /// Files or folders inside iCloud Drive. Without any, the selection
        /// the file manager passes in the environment is used.
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
        /// Report progress and the result as desktop notifications.
        #[arg(long)]
        notify: bool,
    },
    /// Show whether the service is running.
    Status,
    /// Follow the service's log.
    Logs,
    /// Check the installation and say what is wrong.
    Doctor,
    /// Delete the local cache; it is rebuilt from iCloud on the next start.
    ClearCache {
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Show what iCloud is doing next to it in the Files sidebar.
    #[command(alias = "nautilus-install")]
    StatusInstall,
    /// Stop showing activity in the Files sidebar.
    #[command(alias = "nautilus-uninstall")]
    StatusUninstall,
    /// Add "Download from iCloud" to the right-click menu of Files (Nautilus).
    MenuInstall,
    /// Remove "Download from iCloud" from the right-click menu.
    MenuUninstall,
    /// Keep GNOME's search indexer out of the mount, where it would otherwise
    /// download the whole drive.
    Trackerignore,
    /// Remove the service.
    Uninstall {
        /// Also delete configuration, saved session and cache.
        #[arg(long)]
        purge: bool,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("icloudctl").chain(args.iter().copied())).unwrap().command
    }

    #[test]
    fn download_takes_paths_and_the_file_managers_double_dash() {
        assert!(matches!(
            parse(&["download", "--notify", "--", "a b.pdf", "Docs"]),
            Command::Download { paths, notify: true } if paths == [PathBuf::from("a b.pdf"), PathBuf::from("Docs")]
        ));
        assert!(matches!(parse(&["download"]), Command::Download { paths, notify: false } if paths.is_empty()));
    }

    #[test]
    fn the_grammar_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn commands_from_the_python_era_keep_their_names_and_flags() {
        assert!(matches!(parse(&["quickstart"]), Command::Quickstart { mount_dir: None }));
        assert!(matches!(parse(&["init", "/mnt/x"]), Command::Init { mount_dir: Some(_) }));
        assert!(matches!(parse(&["auth"]), Command::Auth { notify: false }));
        assert!(matches!(parse(&["auth", "--notify"]), Command::Auth { notify: true }));
        assert!(matches!(
            parse(&["hydrate", "--dry-run", "--verbose"]),
            Command::Hydrate { dry_run: true, verbose: true }
        ));
        assert!(matches!(parse(&["sync", "--timeout", "30", "--quiet"]), Command::Sync { timeout: 30, quiet: true }));
        assert!(matches!(parse(&["uninstall", "--purge"]), Command::Uninstall { purge: true, yes: false }));
        for name in [
            "start",
            "stop",
            "restart",
            "refresh",
            "status",
            "logs",
            "doctor",
            "clear-cache",
            "trackerignore",
            "status-install",
            "status-uninstall",
            "nautilus-install",
            "nautilus-uninstall",
        ] {
            Cli::try_parse_from(["icloudctl", name]).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn unknown_commands_and_flags_are_rejected() {
        assert!(Cli::try_parse_from(["icloudctl", "frobnicate"]).is_err());
        assert!(Cli::try_parse_from(["icloudctl", "start", "--now"]).is_err());
        // The Apple ID and the password are only ever typed on icloud.com.
        assert!(Cli::try_parse_from(["icloudctl", "configure", "me@x.com"]).is_err());
        assert!(Cli::try_parse_from(["icloudctl", "auth", "--force-sms"]).is_err());
        assert!(Cli::try_parse_from(["icloudctl"]).is_err());
    }
}
