//! Forgetting that a thumbnail once failed.
//!
//! A thumbnailer that is turned away from a file that is not downloaded makes
//! the file manager write a "failed" note for it
//! (`~/.cache/thumbnails/fail/<program>/<md5 of the URI>.png`), and it does
//! not try again until the file changes. Downloading the file on purpose
//! changes nothing about it (it keeps its modification time), so without
//! clearing the note the preview would never appear. The note's name is the
//! MD5 of the file's URI, per the freedesktop thumbnail specification.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use md5::{Digest, Md5};

/// `file://` URI of `path` the way `GLib` writes it: everything but unreserved
/// characters, sub-delimiters, `:`, `@` and `/` is percent-encoded.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@'
            | b'/' => uri.push(char::from(byte)),
            other => {
                let _ = write!(uri, "%{other:02X}");
            }
        }
    }
    uri
}

/// The file name of the thumbnail (or failure note) for a URI.
pub fn thumbnail_name(uri: &str) -> String {
    let digest = Md5::digest(uri.as_bytes());
    let mut name = String::with_capacity(36);
    for byte in digest {
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(".png");
    name
}

/// Remove the failure notes for `files`. Returns how many were removed.
///
/// `cache_home` is `~/.cache`; the notes live in `thumbnails/fail/*/`.
pub fn forget_failures(cache_home: &Path, files: &[PathBuf]) -> usize {
    let programs: Vec<PathBuf> = fs::read_dir(cache_home.join("thumbnails/fail"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    if programs.is_empty() {
        return 0;
    }
    let mut removed = 0;
    for file in files {
        let name = thumbnail_name(&file_uri(file));
        for program in &programs {
            if fs::remove_file(program.join(&name)).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_are_escaped_like_glib_does() {
        assert_eq!(file_uri(Path::new("/home/u/iCloud/a b.pdf")), "file:///home/u/iCloud/a%20b.pdf");
        assert_eq!(file_uri(Path::new("/h/Résumé #1 (final).pdf")), "file:///h/R%C3%A9sum%C3%A9%20%231%20(final).pdf");
        assert_eq!(file_uri(Path::new("/h/a+b,c;d=e@f:g!h$i&j'k")), "file:///h/a+b,c;d=e@f:g!h$i&j'k");
        assert_eq!(file_uri(Path::new("/h/100%/x?y")), "file:///h/100%25/x%3Fy");
    }

    #[test]
    fn the_name_is_the_md5_of_the_uri_as_the_specification_says() {
        // The example in the freedesktop thumbnail specification.
        assert_eq!(thumbnail_name("file:///home/jens/photos/me.png"), "c6ee772d9e49320e97ec29a7eb5b1697.png");
    }

    #[test]
    fn only_the_notes_of_the_given_files_are_removed() {
        let cache = tempfile::tempdir().unwrap();
        let fail = cache.path().join("thumbnails/fail");
        let (a, b, other) = (
            PathBuf::from("/home/u/iCloud/a b.pdf"),
            PathBuf::from("/home/u/iCloud/big.png"),
            PathBuf::from("/home/u/iCloud/other.png"),
        );
        for program in ["gnome-thumbnail-factory", "other-app"] {
            fs::create_dir_all(fail.join(program)).unwrap();
            for file in [&a, &other] {
                fs::write(fail.join(program).join(thumbnail_name(&file_uri(file))), b"png").unwrap();
            }
        }
        assert_eq!(forget_failures(cache.path(), &[a.clone(), b]), 2, "a.pdf in both programs, big.png in none");
        assert!(!fail.join("other-app").join(thumbnail_name(&file_uri(&a))).exists());
        assert!(fail.join("other-app").join(thumbnail_name(&file_uri(&other))).exists());
    }

    #[test]
    fn no_thumbnail_cache_is_nothing_to_do() {
        let cache = tempfile::tempdir().unwrap();
        assert_eq!(forget_failures(cache.path(), &[PathBuf::from("/x")]), 0);
    }
}
