//! Log to stderr (journald picks that up) and to `icloud.log`, which the
//! sidebar status watcher follows.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tracing::Level;

/// Above this size the log is moved aside at startup.
const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Clone)]
struct Tee {
    file: Option<Arc<Mutex<File>>>,
}

struct TeeWriter<'a> {
    file: Option<MutexGuard<'a, File>>,
}

impl Write for TeeWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::stderr().write_all(buf)?;
        if let Some(file) = &mut self.file {
            // A full disk must not take the daemon down with it.
            let _ = file.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            let _ = file.flush();
        }
        io::stderr().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Tee {
    type Writer = TeeWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter { file: self.file.as_ref().map(|f| f.lock().unwrap_or_else(PoisonError::into_inner)) }
    }
}

/// Open the log for appending, moving an oversized one aside first.
fn open_log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    OpenOptions::new().create(true).append(true).mode(0o600).open(path)
}

/// Install the global logger. Logging to the file is best effort.
pub fn init(debug: bool, log_path: &Path) {
    let file = match open_log(log_path) {
        Ok(file) => Some(Arc::new(Mutex::new(file))),
        Err(err) => {
            eprintln!("cannot open the log file {}: {err}", log_path.display());
            None
        }
    };
    let level = if debug { Level::DEBUG } else { Level::INFO };
    tracing_subscriber::fmt().with_max_level(level).with_ansi(false).with_writer(Tee { file }).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_log_is_moved_aside_and_a_fresh_one_started() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("icloud.log");
        let big = File::create(&log).unwrap();
        big.set_len(MAX_LOG_BYTES + 1).unwrap();
        drop(big);
        let fresh = open_log(&log).unwrap();
        assert_eq!(fresh.metadata().unwrap().len(), 0);
        assert!(dir.path().join("icloud.log.1").exists());
    }

    #[test]
    fn a_small_log_is_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sub/icloud.log");
        writeln!(open_log(&log).unwrap(), "one").unwrap();
        writeln!(open_log(&log).unwrap(), "two").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn the_log_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("icloud.log");
        open_log(&log).unwrap();
        assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
