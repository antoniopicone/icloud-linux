//! `icloudctl`: set up, sign in to, and control icloud-linux.

use std::process::ExitCode;

use clap::Parser;
use icloud_core::{Layout, setup::RealSystemctl};
use icloudctl::{
    cli::Cli,
    commands::{Context, run},
    prompt::Terminal,
};

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).with_writer(std::io::stderr).init();

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
