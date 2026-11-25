//! `icloud-installer`: a guided setup for icloud-linux.
//!
//! Walks through the same steps as `icloudctl init`, `auth` and `start`,
//! in a libadwaita window.
//!
//! `--demo` runs the whole wizard against a pretend system (the sign-in is
//! pretended too) so it can be tried without an Apple account and without
//! changing anything.

mod demo;
mod wizard;

use std::sync::Arc;

use adw::prelude::*;
use gtk4::{gio, glib};
use icloud_core::{
    Layout,
    i18n::{tr, trf},
    installer::Backend,
    setup::RealSystemctl,
};
use libadwaita as adw;

const APP_ID: &str = "org.icloud_linux.Installer";

fn main() -> glib::ExitCode {
    let debug = std::env::var("ICLOUD_LOG").is_ok_and(|v| v.eq_ignore_ascii_case("debug"));
    let level = if debug { tracing::Level::DEBUG } else { tracing::Level::INFO };
    tracing_subscriber::fmt().with_max_level(level).init();

    let mut demo = false;
    let mut start_page = std::env::var("ICLOUD_INSTALLER_PAGE").ok();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--demo" => demo = true,
            "--page" => start_page = args.next(),
            "-h" | "--help" => {
                println!(
                    "{}",
                    tr(
                        "usage: icloud-installer [--demo]\n\n  --demo  try the wizard without an Apple account or any change to this machine"
                    )
                );
                return glib::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("{}", trf("unknown argument `{}`; see --help", &[&other]));
                return glib::ExitCode::FAILURE;
            }
        }
    }

    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(err) => {
            eprintln!("error: {err}");
            return glib::ExitCode::FAILURE;
        }
    };
    let backend: Arc<dyn Backend> = if demo {
        Arc::new(demo::DemoBackend)
    } else {
        Arc::new(icloud_core::installer::SystemBackend::new(layout.clone(), Arc::new(RealSystemctl)))
    };

    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_activate(move |app| {
        wizard::present(app, Arc::clone(&backend), layout.clone(), demo, start_page.clone());
    });
    app.run_with_args::<&str>(&["icloud-installer"])
}
