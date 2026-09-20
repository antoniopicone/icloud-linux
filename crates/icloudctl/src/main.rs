//! `icloudctl`: set up, sign in to, and control icloud-linux.

use std::process::ExitCode;

use clap::Parser;
use icloud_core::{Layout, setup::RealSystemctl};
use icloudctl::{
    cli::{Cli, Command},
    commands::{Context, run},
    prompt::Terminal,
};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let debug = cli.verbose
        || matches!(cli.command, Command::Auth { debug: true, .. })
        || std::env::var("ICLOUD_LOG").is_ok_and(|v| v.eq_ignore_ascii_case("debug"));
    let level = if debug { tracing::Level::DEBUG } else { tracing::Level::WARN };
    tracing_subscriber::fmt().with_max_level(level).with_writer(std::io::stderr).init();

    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut terminal = Terminal;
    let mut ctx = Context { layout, sys: &RealSystemctl, ui: &mut terminal };
    match run(cli.command, &mut ctx) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
