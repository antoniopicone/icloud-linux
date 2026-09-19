//! The real `icloud-status` binary against real files.

use std::{
    fs,
    io::Write,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use icloud_core::Layout;

fn spawn(root: &std::path::Path, log: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_icloud-status"))
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("XDG_STATE_HOME", root.join(".local/state"))
        .env("ICLOUD_LOG_PATH", log)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start icloud-status")
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_binary_labels_the_bookmark_while_the_daemon_works_and_cleans_up_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::under(dir.path());
    let log = layout.log_file();
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, "old history that must be ignored\nsync hydrate-start path='/stale.txt' size=1\n").unwrap();
    fs::create_dir_all(&layout.config_dir).unwrap();
    fs::write(layout.env_file(), format!("ICLOUD_MOUNT='{}'\n", dir.path().join("My iCloud").display())).unwrap();

    let mut child = spawn(dir.path(), &log);
    std::thread::sleep(Duration::from_millis(400));
    assert!(!layout.bookmarks_file().exists(), "history from before the start is not replayed");

    let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
    writeln!(file, "2026-09-19T10:00:00Z  INFO sync hydrate-start path='/Docs/invoice.pdf' size=42").unwrap();
    file.flush().unwrap();

    wait_for("the label to appear", || {
        fs::read_to_string(layout.bookmarks_file()).is_ok_and(|t| t.contains("iCloud (downloading: invoice.pdf)"))
    });
    let text = fs::read_to_string(layout.bookmarks_file()).unwrap();
    assert!(text.contains("My%20iCloud"), "the mount comes from icloud.env and is URI-escaped: {text}");

    // A polite stop, as systemd sends it.
    Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "a signalled stop is a clean exit");
    let after = fs::read_to_string(layout.bookmarks_file()).unwrap();
    assert!(after.trim_end().ends_with("iCloud"), "the plain label is put back: {after}");
}
