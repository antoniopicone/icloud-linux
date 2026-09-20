//! Who is asking for a file's bytes.
//!
//! Downloading a file is only wanted when a person asked for it: by opening it
//! (double click) or by choosing "Download" in the file manager. Programs that
//! walk the drive on their own are a different matter: thumbnail generators
//! open every picture and document they are shown, and desktop search
//! indexers open everything. Left alone they would pull down a whole folder,
//! one file after another, and the download the person actually asked for
//! would wait in that queue.
//!
//! So a read of a file that is not local yet is classified by the program that
//! makes it:
//!
//! | Reader | Downloads a placeholder? |
//! |---|---|
//! | [`Reader::Person`], anything not recognised below | yes |
//! | [`Reader::Preview`], a thumbnailer | only up to the preview size limit |
//! | [`Reader::Indexer`], a search indexer | never |
//!
//! The program is found from the process id the kernel attaches to every FUSE
//! request: the name of that process, and of its first few ancestors, is
//! compared with the names of the well-known background readers. Only the
//! *program* names are compared, never arguments, so opening `thumbnails.pdf`
//! in a viewer is not mistaken for a thumbnailer.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Largest file a thumbnailer may trigger the download of, in bytes (200 kB).
pub const DEFAULT_PREVIEW_MAX_BYTES: u64 = 200_000;

/// How many parents of the requesting process are examined as well.
const ANCESTOR_DEPTH: usize = 3;

/// The kind of program that is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// A person, directly or through the program they opened: any program not
    /// known to be a background reader.
    Person,
    /// A thumbnail generator.
    Preview,
    /// A desktop search indexer.
    Indexer,
}

impl Reader {
    pub fn label(self) -> &'static str {
        match self {
            Self::Person => "an application",
            Self::Preview => "a thumbnailer",
            Self::Indexer => "a search indexer",
        }
    }
}

/// Something that can say who is reading, when (and only when) that matters.
///
/// Working it out means reading `/proc`, and almost every read is of a file
/// that is already local, where the answer is not needed. Hence the lazy
/// question instead of an eager value.
pub trait Requester {
    fn reader(&self) -> Reader;
}

impl Requester for Reader {
    fn reader(&self) -> Reader {
        *self
    }
}

/// Classify a program by its name. `None` when it is not a known background
/// reader.
fn classify_name(name: &str) -> Option<Reader> {
    let name = name.to_ascii_lowercase();
    // `comm` is cut at 15 characters (`gdk-pixbuf-thum`, `totem-video-thu`),
    // hence the short stems.
    if name.contains("thum") || name.contains("-thu") || name.contains("tumbler") {
        return Some(Reader::Preview);
    }
    if name.starts_with("tracker") || name.starts_with("localsearch") || name.starts_with("baloo") {
        return Some(Reader::Indexer);
    }
    None
}

/// The reader for a process known by these names. The strictest kind wins:
/// an indexer that spawns a helper is still an indexer.
pub fn classify_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Reader {
    let mut found = Reader::Person;
    for kind in names.into_iter().filter_map(classify_name) {
        if kind == Reader::Indexer {
            return Reader::Indexer;
        }
        found = kind;
    }
    found
}

/// The requester behind a process id, read from `/proc`.
#[derive(Debug, Clone)]
pub struct ProcessReader {
    proc_root: PathBuf,
    pid: u32,
}

impl ProcessReader {
    pub fn new(pid: u32) -> Self {
        Self::with_root("/proc", pid)
    }

    /// Like [`new`](Self::new) with a stand-in for `/proc`, for tests.
    pub fn with_root(proc_root: impl Into<PathBuf>, pid: u32) -> Self {
        Self { proc_root: proc_root.into(), pid }
    }

    /// The process a kernel-reported id belongs to. FUSE reports the id of
    /// the *thread* that made the request, and threads carry names of their
    /// own (`pool-papers`, or a test called `a_thumbnailer_…`); only the
    /// process's name says what program this is.
    fn process_of(&self, id: u32) -> u32 {
        fs::read_to_string(self.proc_root.join(id.to_string()).join("status"))
            .ok()
            .and_then(|status| status.lines().find_map(|line| line.strip_prefix("Tgid:")?.trim().parse().ok()))
            .unwrap_or(id)
    }

    /// The names a process goes by: `comm`, the executable and `argv[0]`.
    /// Each may be unavailable (the process may already be gone).
    fn names_of(&self, id: u32) -> Vec<String> {
        let dir = self.proc_root.join(self.process_of(id).to_string());
        let mut names = Vec::new();
        if let Ok(comm) = fs::read_to_string(dir.join("comm")) {
            names.push(comm.trim().to_owned());
        }
        if let Some(exe) = fs::read_link(dir.join("exe")).ok().as_deref().and_then(base_name) {
            names.push(exe);
        }
        if let Ok(cmdline) = fs::read(dir.join("cmdline")) {
            let first = cmdline.split(|b| *b == 0).next().unwrap_or_default();
            if let Some(name) = base_name(Path::new(&*String::from_utf8_lossy(first))) {
                names.push(name);
            }
        }
        names
    }

    fn parent_of(&self, pid: u32) -> Option<u32> {
        let stat = fs::read_to_string(self.proc_root.join(pid.to_string()).join("stat")).ok()?;
        // `pid (comm) S ppid …`: comm may itself contain spaces and brackets.
        let after = &stat[stat.rfind(')')? + 1..];
        after.split_whitespace().nth(1)?.parse().ok().filter(|ppid| *ppid > 1)
    }
}

impl Requester for ProcessReader {
    fn reader(&self) -> Reader {
        let mut names = self.names_of(self.pid);
        let mut current = self.pid;
        for _ in 0..ANCESTOR_DEPTH {
            let Some(parent) = self.parent_of(current) else { break };
            names.extend(self.names_of(parent));
            current = parent;
        }
        classify_names(names.iter().map(String::as_str))
    }
}

fn base_name(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_process(root: &Path, pid: u32, ppid: u32, comm: &str, exe: &str, argv0: &str) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("status"), format!("Name:\t{comm}\nTgid:\t{pid}\nPid:\t{pid}\n")).unwrap();
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(dir.join("stat"), format!("{pid} ({comm}) S {ppid} 1 1 0 -1 4194560")).unwrap();
        fs::write(dir.join("cmdline"), format!("{argv0}\0--flag\0thumbnails.pdf\0")).unwrap();
        let _ = std::os::unix::fs::symlink(exe, dir.join("exe"));
    }

    #[test]
    fn well_known_thumbnailers_are_previews_even_when_the_name_is_cut_short() {
        for name in [
            "gnome-desktop-thumbnailer",
            "gdk-pixbuf-thum",
            "evince-thumbnailer",
            "totem-video-thu",
            "ffmpegthumbnailer",
            "tumblerd",
            "kio_thumbnail",
        ] {
            assert_eq!(classify_names([name]), Reader::Preview, "{name}");
        }
    }

    #[test]
    fn search_indexers_are_recognised() {
        for name in
            ["tracker-miner-fs-3", "tracker-extract-3", "localsearch-3", "localsearch-extractor-3", "baloo_file"]
        {
            assert_eq!(classify_names([name]), Reader::Indexer, "{name}");
        }
    }

    #[test]
    fn everything_else_is_a_person_at_work() {
        for name in
            ["evince", "papers", "nautilus", "thunar", "vlc", "cat", "icloudctl", "firefox", "code", "cp", "sushi"]
        {
            assert_eq!(classify_names([name]), Reader::Person, "{name}");
        }
        assert_eq!(classify_names([]), Reader::Person);
    }

    #[test]
    fn an_indexer_outranks_a_thumbnailer_among_the_ancestors() {
        assert_eq!(classify_names(["sh", "gdk-pixbuf-thum", "tracker-extract-3"]), Reader::Indexer);
        assert_eq!(classify_names(["sh", "gdk-pixbuf-thum", "bwrap"]), Reader::Preview);
    }

    #[test]
    fn the_process_is_identified_from_proc() {
        let proc = tempfile::tempdir().unwrap();
        fake_process(
            proc.path(),
            500,
            42,
            "gdk-pixbuf-thum",
            "/usr/bin/gdk-pixbuf-thumbnailer",
            "gdk-pixbuf-thumbnailer",
        );
        assert_eq!(ProcessReader::with_root(proc.path(), 500).reader(), Reader::Preview);
    }

    #[test]
    fn a_viewer_opening_a_file_called_thumbnails_is_not_a_thumbnailer() {
        let proc = tempfile::tempdir().unwrap();
        // The argument mentions "thumbnails"; only the program name counts.
        fake_process(proc.path(), 600, 42, "evince", "/usr/bin/evince", "evince");
        assert_eq!(ProcessReader::with_root(proc.path(), 600).reader(), Reader::Person);
    }

    #[test]
    fn a_helper_started_by_an_indexer_is_treated_as_the_indexer() {
        let proc = tempfile::tempdir().unwrap();
        fake_process(proc.path(), 700, 650, "sh", "/usr/bin/dash", "sh");
        fake_process(proc.path(), 650, 640, "tracker-extract-", "/usr/libexec/tracker-extract-3", "tracker-extract-3");
        fake_process(proc.path(), 640, 1, "systemd", "/usr/lib/systemd/systemd", "systemd");
        assert_eq!(ProcessReader::with_root(proc.path(), 700).reader(), Reader::Indexer);
    }

    #[test]
    fn a_thread_is_judged_by_its_process_not_by_its_own_name() {
        let proc = tempfile::tempdir().unwrap();
        fake_process(proc.path(), 900, 42, "papers", "/usr/bin/papers", "papers");
        // Thread 901 of process 900, named like a thumbnailer.
        let thread = proc.path().join("901");
        fs::create_dir_all(&thread).unwrap();
        fs::write(thread.join("status"), "Name:\tthumbnail-loader\nTgid:\t900\nPid:\t901\n").unwrap();
        fs::write(thread.join("comm"), "thumbnail-loader\n").unwrap();
        fs::write(thread.join("stat"), "901 (thumbnail-loader) S 42 1 1 0 -1 0").unwrap();
        assert_eq!(ProcessReader::with_root(proc.path(), 901).reader(), Reader::Person);
    }

    #[test]
    fn a_process_that_is_gone_is_a_person() {
        // Better to download once too often than to refuse a person.
        let proc = tempfile::tempdir().unwrap();
        assert_eq!(ProcessReader::with_root(proc.path(), 12345).reader(), Reader::Person);
    }

    #[test]
    fn command_names_with_brackets_do_not_confuse_the_parent_lookup() {
        let proc = tempfile::tempdir().unwrap();
        fake_process(proc.path(), 800, 790, "we ird) (name", "/usr/bin/x", "x");
        fake_process(proc.path(), 790, 1, "tracker-miner-fs", "/usr/libexec/tracker-miner-fs-3", "tracker-miner-fs-3");
        assert_eq!(ProcessReader::with_root(proc.path(), 800).reader(), Reader::Indexer);
    }
}
