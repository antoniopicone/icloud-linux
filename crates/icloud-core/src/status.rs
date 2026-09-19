//! What the daemon is doing right now, shown next to "iCloud" in the sidebar
//! of the file manager.
//!
//! With `crawl_mode: lazy` the mount is up in seconds and the work happens
//! while you browse, so it helps to *see* which folder is being listed and
//! which file is being downloaded. The label of the `iCloud` bookmark carries
//! that: `iCloud (listing: Documents)`, `iCloud (downloading: invoice.pdf)`,
//! plain `iCloud` when nothing has happened for a while.
//!
//! This is the Rust port of the old `icloud_status.py` Nautilus extension.
//! The label is just a line of `~/.config/gtk-3.0/bookmarks`, which Nautilus
//! and every GTK file dialog watch, so this does not have to run inside the
//! file manager. It runs as a small user service (`icloud-status`) that
//! follows the daemon's log:
//!
//! ```text
//! sync list-directory-start path='/Docs'          → listing: Docs
//! sync list-directory-complete path='/Docs' entries=12  → Docs: 12 items
//! sync hydrate-start path='/Docs/a.pdf' size=…    → downloading: a.pdf
//! sync hydrate-complete path='/Docs/a.pdf' …      → a.pdf downloaded
//! sync file-sync-start path='/Docs/b.txt' …       → syncing: b.txt
//! sync file-sync-complete path='/Docs/b.txt' …    → b.txt synced
//! ```
//!
//! What is *not* ported is the old extension's context-menu entry "iCloud sync
//! status…": that needs code loaded into the Nautilus process itself, which
//! means a native plugin library with C-ABI FFI, and nothing in this workspace
//! uses `unsafe`. The sidebar label carries the same information.

use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::{dirs::Layout, setup::recorded_mount};

/// How long a silence before the label goes back to plain `iCloud`.
pub const ACTIVITY_WINDOW: Duration = Duration::from_secs(20);
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// One thing the daemon reported doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Listing(String),
    Listed { name: String, items: u64 },
    Downloading(String),
    Downloaded(String),
    Syncing(String),
    Synced(String),
}

impl Activity {
    /// The words that go in the label.
    pub fn describe(&self) -> String {
        match self {
            Self::Listing(name) => format!("listing: {name}"),
            Self::Listed { name, items } => format!("{name}: {items} item{}", if *items == 1 { "" } else { "s" }),
            Self::Downloading(name) => format!("downloading: {name}"),
            Self::Downloaded(name) => format!("{name} downloaded"),
            Self::Syncing(name) => format!("syncing: {name}"),
            Self::Synced(name) => format!("{name} synced"),
        }
    }
}

/// A path shortened to its last component, `iCloud` for the root.
fn display_name(path: &str) -> String {
    match path.trim_end_matches('/').rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => "iCloud".to_owned(),
    }
}

/// The `key=value` fields after `sync <event>`. Values are either bare
/// (`entries=12`) or single-quoted with `\'` and `\\` escapes (`path='/a b'`).
fn parse_fields(rest: &str) -> Vec<(&str, String)> {
    let mut fields = Vec::new();
    let mut chars = rest.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let Some(eq) = rest[start..].find('=') else { break };
        let key = &rest[start..start + eq];
        if key.contains(char::is_whitespace) {
            break; // free text, not a field
        }
        // Advance past `key=`.
        while chars.peek().is_some_and(|&(i, _)| i < start + eq + 1) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek().is_some_and(|&(_, c)| c == '\'') {
            chars.next();
            while let Some((_, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, escaped)) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    '\'' => break,
                    other => value.push(other),
                }
            }
        } else {
            while let Some(&(_, c)) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }
        fields.push((key, value));
    }
    fields
}

/// Recognise one log line. Anything that is not a `sync …` event is `None`.
pub fn parse_line(line: &str) -> Option<Activity> {
    let after = &line[line.find("sync ")? + "sync ".len()..];
    let (event, rest) = after.split_once(' ').unwrap_or((after, ""));
    let fields = parse_fields(rest);
    let get = |key: &str| fields.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
    let name = || display_name(get("path").unwrap_or("/"));
    match event {
        "list-directory-start" => Some(Activity::Listing(name())),
        "list-directory-complete" => {
            Some(Activity::Listed { name: name(), items: get("entries").and_then(|n| n.parse().ok()).unwrap_or(0) })
        }
        "hydrate-start" => Some(Activity::Downloading(name())),
        "hydrate-complete" => Some(Activity::Downloaded(name())),
        "file-sync-start" => Some(Activity::Syncing(name())),
        "file-sync-complete" => Some(Activity::Synced(name())),
        _ => None,
    }
}

/// Follows a log file and remembers the latest activity.
#[derive(Debug)]
pub struct StatusMonitor {
    position: u64,
    /// The unfinished last line of the previous read.
    partial: String,
    last: Option<(Activity, Instant)>,
}

impl StatusMonitor {
    /// Start at the end of the log, so restarting does not replay the whole
    /// history as if it had just happened.
    pub fn following(log: &Path) -> Self {
        Self { position: fs::metadata(log).map_or(0, |m| m.len()), partial: String::new(), last: None }
    }

    /// Read what was appended since the last call. Returns whether the label
    /// should change.
    pub fn poll(&mut self, log: &Path, now: Instant) -> bool {
        let mut changed = false;
        if let Ok(len) = fs::metadata(log).map(|m| m.len()) {
            if len < self.position {
                // Rotated or truncated: start again from the top.
                self.position = 0;
                self.partial.clear();
            }
            if len > self.position
                && let Ok(text) = self.read_new(log)
            {
                changed |= self.absorb(&text, now);
            }
        }
        if self.last.as_ref().is_some_and(|(_, at)| now.saturating_duration_since(*at) > ACTIVITY_WINDOW) {
            self.last = None;
            changed = true;
        }
        changed
    }

    fn read_new(&mut self, log: &Path) -> io::Result<String> {
        let mut file = File::open(log)?;
        file.seek(SeekFrom::Start(self.position))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.position += bytes.len() as u64;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Feed text that may end in the middle of a line.
    fn absorb(&mut self, text: &str, now: Instant) -> bool {
        self.partial.push_str(text);
        let Some(end) = self.partial.rfind('\n') else { return false };
        let complete: String = self.partial.drain(..=end).collect();
        let mut changed = false;
        for line in complete.lines() {
            if let Some(activity) = parse_line(line) {
                self.last = Some((activity, now));
                changed = true;
            }
        }
        changed
    }

    /// The text after `iCloud` in the label; empty when idle.
    pub fn label_suffix(&self) -> String {
        self.last.as_ref().map_or_else(String::new, |(activity, _)| format!(" ({})", activity.describe()))
    }

    pub fn current(&self) -> Option<&Activity> {
        self.last.as_ref().map(|(activity, _)| activity)
    }
}

// ---- the bookmark ---------------------------------------------------------------

/// Percent-encode a path for a `file://` URI the way `GLib` does for the
/// bookmarks file: everything except letters, digits, `-._~` and `/`.
pub fn escape_uri_path(path: &str) -> String {
    use std::fmt::Write as _;
    path.bytes().fold(String::with_capacity(path.len()), |mut out, b| {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

/// `content` with the line for `uri` labelled `iCloud{suffix}`, added if it
/// was not there. Every other line, including other custom labels, is kept.
pub fn relabel(content: &str, uri: &str, suffix: &str) -> String {
    let wanted = format!("{uri} iCloud{suffix}");
    let mut found = false;
    let mut out = String::with_capacity(content.len() + wanted.len() + 1);
    for line in content.lines() {
        // The URI itself, with or without a label. A longer URI that merely
        // starts the same (`…/iCloud2`) is a different bookmark.
        let is_ours = line == uri || line.strip_prefix(uri).is_some_and(|rest| rest.starts_with(' '));
        if is_ours && !found {
            out.push_str(&wanted);
            found = true;
        } else if !is_ours {
            out.push_str(line);
        } else {
            continue; // a duplicate of our bookmark
        }
        out.push('\n');
    }
    if !found {
        out.push_str(&wanted);
        out.push('\n');
    }
    out
}

/// Set the label of the bookmark for `mount` in `bookmarks`. Writes nothing
/// when the file already says the right thing, so an idle watcher does not make
/// the sidebar redraw. Returns whether the file changed.
pub fn write_label(bookmarks: &Path, mount: &Path, suffix: &str) -> io::Result<bool> {
    let uri = format!("file://{}", escape_uri_path(&mount.to_string_lossy()));
    let current = match fs::read_to_string(bookmarks) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let updated = relabel(&current, &uri, suffix);
    if updated == current {
        return Ok(false);
    }
    if let Some(dir) = bookmarks.parent() {
        fs::create_dir_all(dir)?;
    }
    // Replace atomically so a file manager never reads half a file.
    let tmp = bookmarks.with_extension("icloud-tmp");
    fs::write(&tmp, updated)?;
    fs::rename(&tmp, bookmarks)?;
    Ok(true)
}

// ---- the watcher ------------------------------------------------------------------

/// Ties a [`StatusMonitor`] to the log and the bookmarks file.
#[derive(Debug)]
pub struct Watcher {
    layout: Layout,
    log: PathBuf,
    monitor: StatusMonitor,
}

impl Watcher {
    pub fn new(layout: Layout, log: PathBuf) -> Self {
        let monitor = StatusMonitor::following(&log);
        Self { layout, log, monitor }
    }

    /// The mount point, read every time so a re-`init` to another folder is
    /// followed without restarting.
    fn mount(&self) -> PathBuf {
        recorded_mount(&self.layout).unwrap_or_else(|| self.layout.default_mount())
    }

    /// One look at the log; updates the label if something changed.
    pub fn tick(&mut self, now: Instant) -> io::Result<bool> {
        if !self.monitor.poll(&self.log, now) {
            return Ok(false);
        }
        write_label(&self.layout.bookmarks_file(), &self.mount(), &self.monitor.label_suffix())
    }

    /// Put the plain label back.
    pub fn reset(&self) -> io::Result<bool> {
        write_label(&self.layout.bookmarks_file(), &self.mount(), "")
    }

    /// Watch until `stop` is raised, then leave the plain label behind.
    pub fn run(&mut self, stop: &AtomicBool, interval: Duration) {
        while !stop.load(Ordering::Relaxed) {
            if let Err(err) = self.tick(Instant::now()) {
                // Never let a transient error end the watch.
                tracing::warn!("could not update the sidebar label: {err}");
            }
            // Sleep in short slices so a stop request is honoured promptly.
            let deadline = Instant::now() + interval;
            while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100).min(interval));
            }
        }
        if let Err(err) = self.reset() {
            tracing::warn!("could not reset the sidebar label: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, sync::Arc};

    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    // ---- parsing ---------------------------------------------------------------

    #[test]
    fn the_events_the_engine_emits_are_recognised() {
        let cases = [
            ("sync list-directory-start path='/Docs'", Activity::Listing("Docs".into())),
            ("sync list-directory-start path='/'", Activity::Listing("iCloud".into())),
            (
                "sync list-directory-complete path='/Docs' entries=12",
                Activity::Listed { name: "Docs".into(), items: 12 },
            ),
            (
                "sync hydrate-start path='/Docs/a.pdf' drivewsid='FILE::1' size=10",
                Activity::Downloading("a.pdf".into()),
            ),
            ("sync hydrate-complete path='/Docs/a.pdf' source='remote' size=10", Activity::Downloaded("a.pdf".into())),
            ("sync file-sync-start path='/b.txt' remote_exists=False", Activity::Syncing("b.txt".into())),
            ("sync file-sync-complete path='/b.txt' size=3", Activity::Synced("b.txt".into())),
        ];
        for (line, expected) in cases {
            assert_eq!(parse_line(line), Some(expected), "{line}");
        }
    }

    #[test]
    fn a_full_tracing_line_with_a_prefix_parses() {
        let line = "2026-09-19T17:14:49.822Z  INFO icloud_core::engine::remote: sync list-directory-start path='/Docs'";
        assert_eq!(parse_line(line), Some(Activity::Listing("Docs".into())));
    }

    #[test]
    fn paths_with_spaces_quotes_and_backslashes_survive() {
        assert_eq!(
            parse_line(r"sync hydrate-start path='/My Docs/it\'s here.pdf' size=1"),
            Some(Activity::Downloading("it's here.pdf".into()))
        );
        assert_eq!(parse_line(r"sync hydrate-start path='/a\\b' size=1"), Some(Activity::Downloading(r"a\b".into())));
        assert_eq!(parse_line("sync list-directory-start path='/日本語/ünï'"), Some(Activity::Listing("ünï".into())));
    }

    #[test]
    fn other_lines_are_ignored() {
        for line in [
            "",
            "just some text",
            "INFO mounted iCloud Drive at /home/u/iCloud",
            "sync refresh-start reason='scheduled'",
            "sync delete-start path='/x' remote=True",
            "a message that mentions sync but is not an event",
        ] {
            assert_eq!(parse_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn descriptions_read_naturally() {
        assert_eq!(Activity::Listing("Docs".into()).describe(), "listing: Docs");
        assert_eq!(Activity::Listed { name: "Docs".into(), items: 1 }.describe(), "Docs: 1 item");
        assert_eq!(Activity::Listed { name: "Docs".into(), items: 0 }.describe(), "Docs: 0 items");
        assert_eq!(Activity::Downloaded("a".into()).describe(), "a downloaded");
    }

    // ---- following a log ----------------------------------------------------------

    fn append(path: &Path, text: &str) {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn history_from_before_the_watch_started_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("icloud.log");
        append(&log, "sync hydrate-start path='/old.txt' size=1\n");
        let mut monitor = StatusMonitor::following(&log);
        assert!(!monitor.poll(&log, t0()));
        assert_eq!(monitor.label_suffix(), "");
    }

    #[test]
    fn new_lines_change_the_label_and_the_latest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("icloud.log");
        let mut monitor = StatusMonitor::following(&log);
        let now = t0();
        append(&log, "noise\nsync list-directory-start path='/Docs'\nsync hydrate-start path='/Docs/a.pdf' size=1\n");
        assert!(monitor.poll(&log, now));
        assert_eq!(monitor.label_suffix(), " (downloading: a.pdf)");
        append(&log, "sync hydrate-complete path='/Docs/a.pdf' size=1\n");
        assert!(monitor.poll(&log, now));
        assert_eq!(monitor.label_suffix(), " (a.pdf downloaded)");
    }

    #[test]
    fn a_line_split_across_two_reads_is_not_lost_or_misread() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("icloud.log");
        let mut monitor = StatusMonitor::following(&log);
        append(&log, "sync list-directory-start pa");
        assert!(!monitor.poll(&log, t0()), "half a line says nothing yet");
        append(&log, "th='/Docs'\n");
        assert!(monitor.poll(&log, t0()));
        assert_eq!(monitor.current(), Some(&Activity::Listing("Docs".into())));
    }

    #[test]
    fn the_label_expires_after_the_quiet_period() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("icloud.log");
        let mut monitor = StatusMonitor::following(&log);
        let start = t0();
        append(&log, "sync hydrate-start path='/a' size=1\n");
        assert!(monitor.poll(&log, start));
        assert!(!monitor.poll(&log, start + ACTIVITY_WINDOW), "still within the window");
        assert!(monitor.poll(&log, start + ACTIVITY_WINDOW + Duration::from_secs(1)));
        assert_eq!(monitor.label_suffix(), "");
        assert!(!monitor.poll(&log, start + ACTIVITY_WINDOW * 3), "and it only expires once");
    }

    #[test]
    fn a_rotated_log_is_followed_from_its_new_start() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("icloud.log");
        append(&log, &"padding line\n".repeat(50));
        let mut monitor = StatusMonitor::following(&log);
        fs::write(&log, "sync list-directory-start path='/New'\n").unwrap();
        assert!(monitor.poll(&log, t0()));
        assert_eq!(monitor.current(), Some(&Activity::Listing("New".into())));
    }

    #[test]
    fn a_missing_log_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("never-created.log");
        let mut monitor = StatusMonitor::following(&log);
        assert!(!monitor.poll(&log, t0()));
    }

    #[test]
    fn invalid_utf8_in_the_log_does_not_stop_the_watch() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("icloud.log");
        let mut monitor = StatusMonitor::following(&log);
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        file.write_all(b"\xff\xfe garbage\nsync list-directory-start path='/ok'\n").unwrap();
        assert!(monitor.poll(&log, t0()));
        assert_eq!(monitor.current(), Some(&Activity::Listing("ok".into())));
    }

    // ---- the bookmarks file ----------------------------------------------------------

    #[test]
    fn uris_are_escaped_like_glib_does() {
        assert_eq!(escape_uri_path("/home/u/iCloud"), "/home/u/iCloud");
        assert_eq!(escape_uri_path("/home/u/My iCloud"), "/home/u/My%20iCloud");
        assert_eq!(escape_uri_path("/home/ü/x"), "/home/%C3%BC/x");
        assert_eq!(escape_uri_path("/a+b&c=d"), "/a%2Bb%26c%3Dd");
        assert_eq!(escape_uri_path("/keep-._~"), "/keep-._~");
    }

    #[test]
    fn relabelling_replaces_only_our_line_and_keeps_the_others_untouched() {
        let uri = "file:///home/u/iCloud";
        let before = "file:///home/u/Documents Docs\nfile:///home/u/iCloud iCloud (old)\nfile:///mnt/nas NAS\n";
        let after = relabel(before, uri, " (listing: X)");
        assert_eq!(
            after,
            "file:///home/u/Documents Docs\nfile:///home/u/iCloud iCloud (listing: X)\nfile:///mnt/nas NAS\n"
        );
    }

    #[test]
    fn a_missing_bookmark_is_added_which_also_puts_icloud_in_the_sidebar() {
        assert_eq!(relabel("", "file:///m", ""), "file:///m iCloud\n");
        assert_eq!(relabel("file:///other Other", "file:///m", " (x)"), "file:///other Other\nfile:///m iCloud (x)\n");
    }

    #[test]
    fn an_unlabelled_bookmark_is_recognised() {
        assert_eq!(relabel("file:///m\n", "file:///m", " (x)"), "file:///m iCloud (x)\n");
    }

    #[test]
    fn a_bookmark_that_merely_starts_the_same_is_a_different_one() {
        let before = "file:///home/u/iCloud2 Other drive\n";
        let after = relabel(before, "file:///home/u/iCloud", "");
        assert_eq!(after, "file:///home/u/iCloud2 Other drive\nfile:///home/u/iCloud iCloud\n");
    }

    #[test]
    fn duplicate_lines_for_our_bookmark_collapse_to_one() {
        let after = relabel("file:///m a\nfile:///m b\n", "file:///m", "");
        assert_eq!(after, "file:///m iCloud\n");
    }

    #[test]
    fn writing_the_label_is_idempotent_and_touches_the_file_only_when_needed() {
        let dir = tempfile::tempdir().unwrap();
        let bookmarks = dir.path().join("gtk-3.0/bookmarks");
        let mount = Path::new("/home/u/My iCloud");
        assert!(write_label(&bookmarks, mount, " (listing: A)").unwrap());
        assert_eq!(fs::read_to_string(&bookmarks).unwrap(), "file:///home/u/My%20iCloud iCloud (listing: A)\n");
        assert!(!write_label(&bookmarks, mount, " (listing: A)").unwrap(), "same text: no write");
        assert!(write_label(&bookmarks, mount, "").unwrap());
        assert_eq!(fs::read_to_string(&bookmarks).unwrap(), "file:///home/u/My%20iCloud iCloud\n");
        assert!(!bookmarks.with_extension("icloud-tmp").exists(), "no temporary file is left");
    }

    #[test]
    fn other_bookmarks_survive_the_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let bookmarks = dir.path().join("bookmarks");
        fs::write(&bookmarks, "file:///a A\nfile:///b\n").unwrap();
        write_label(&bookmarks, Path::new("/m"), " (x)").unwrap();
        assert_eq!(fs::read_to_string(&bookmarks).unwrap(), "file:///a A\nfile:///b\nfile:///m iCloud (x)\n");
    }

    // ---- the watcher ------------------------------------------------------------------

    fn watcher() -> (tempfile::TempDir, Watcher, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::under(dir.path());
        let log = layout.log_file();
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "").unwrap();
        let watcher = Watcher::new(layout, log.clone());
        (dir, watcher, log)
    }

    #[test]
    fn the_watcher_labels_the_recorded_mount_as_activity_appears() {
        let (dir, mut watcher, log) = watcher();
        let layout = Layout::under(dir.path());
        fs::create_dir_all(&layout.config_dir).unwrap();
        fs::write(layout.env_file(), "ICLOUD_MOUNT='/mnt/my drive'\n").unwrap();

        assert!(!watcher.tick(t0()).unwrap(), "nothing yet");
        append(&log, "sync hydrate-start path='/Docs/a.pdf' size=1\n");
        assert!(watcher.tick(t0()).unwrap());
        assert_eq!(
            fs::read_to_string(layout.bookmarks_file()).unwrap(),
            "file:///mnt/my%20drive iCloud (downloading: a.pdf)\n"
        );
    }

    #[test]
    fn without_a_recorded_mount_the_default_folder_is_used() {
        let (dir, mut watcher, log) = watcher();
        let layout = Layout::under(dir.path());
        append(&log, "sync list-directory-start path='/'\n");
        watcher.tick(t0()).unwrap();
        let text = fs::read_to_string(layout.bookmarks_file()).unwrap();
        assert!(text.starts_with(&format!("file://{}", layout.default_mount().display())), "{text}");
    }

    #[test]
    fn stopping_leaves_the_plain_label_behind() {
        let (dir, mut watcher, log) = watcher();
        let layout = Layout::under(dir.path());
        append(&log, "sync hydrate-start path='/x' size=1\n");
        watcher.tick(t0()).unwrap();
        assert!(fs::read_to_string(layout.bookmarks_file()).unwrap().contains("downloading: x"));

        let stop = Arc::new(AtomicBool::new(true)); // already asked to stop
        watcher.run(&stop, Duration::from_millis(10));
        let text = fs::read_to_string(layout.bookmarks_file()).unwrap();
        assert!(text.trim_end().ends_with("iCloud"), "{text}");
        assert!(!text.contains('('));
    }

    #[test]
    fn the_run_loop_notices_activity_and_stops_promptly() {
        let (dir, mut watcher, log) = watcher();
        let layout = Layout::under(dir.path());
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || watcher.run(&stop, Duration::from_millis(20)))
        };
        thread::sleep(Duration::from_millis(60));
        append(&log, "sync list-directory-start path='/Live'\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_to_string(layout.bookmarks_file()).is_ok_and(|t| t.contains("listing: Live")) {
            assert!(Instant::now() < deadline, "the label never appeared");
            thread::sleep(Duration::from_millis(20));
        }
        let asked = Instant::now();
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        assert!(asked.elapsed() < Duration::from_secs(2));
        assert!(!fs::read_to_string(layout.bookmarks_file()).unwrap().contains("Live"));
    }
}
