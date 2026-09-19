//! Paths inside iCloud Drive.
//!
//! An [`IcPath`] is always absolute, `/`-separated, free of `.` and `..`
//! components and of empty components. It cannot be constructed any other way,
//! which is what stops a hostile file name or a crafted request from escaping
//! the mirror directory or a configured sync boundary.

use std::fmt;

/// A normalised absolute path within the Drive, `/` being the root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IcPath(String);

impl IcPath {
    pub fn root() -> Self {
        Self("/".to_owned())
    }

    /// Normalise `raw` the way `os.path.normpath("/" + raw.lstrip("/"))` does:
    /// duplicate slashes and `.` disappear and `..` cannot climb above the root.
    pub fn new(raw: &str) -> Self {
        let mut parts: Vec<&str> = Vec::new();
        for part in raw.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        if parts.is_empty() { Self::root() } else { Self(format!("/{}", parts.join("/"))) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// The containing folder; the root is its own parent.
    pub fn parent(&self) -> Self {
        match self.0.rfind('/') {
            Some(0) | None => Self::root(),
            Some(i) => Self(self.0[..i].to_owned()),
        }
    }

    /// Last component, or `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() { None } else { self.0.rsplit('/').next() }
    }

    /// Append a single component. Returns `None` when `name` is not a valid
    /// file name (empty, `.`, `..`, or containing `/` or NUL).
    pub fn join(&self, name: &str) -> Option<Self> {
        if !is_valid_name(name) {
            return None;
        }
        Some(if self.is_root() { Self(format!("/{name}")) } else { Self(format!("{}/{name}", self.0)) })
    }

    /// Number of components; the root has none.
    pub fn depth(&self) -> usize {
        if self.is_root() { 0 } else { self.0.matches('/').count() }
    }

    /// True when `self` is `prefix` or lies underneath it.
    pub fn is_within(&self, prefix: &Self) -> bool {
        prefix.is_root()
            || self.0 == prefix.0
            || self.0.strip_prefix(&prefix.0).is_some_and(|rest| rest.starts_with('/'))
    }

    /// The part of `self` below `prefix`, without a leading slash. Empty when
    /// the two are equal.
    pub fn relative_to(&self, prefix: &Self) -> Option<&str> {
        if !self.is_within(prefix) {
            return None;
        }
        if prefix.is_root() {
            Some(self.0.trim_start_matches('/'))
        } else {
            Some(self.0[prefix.0.len()..].trim_start_matches('/'))
        }
    }

    /// Replace the leading `from` with `to`; used to move whole subtrees.
    pub fn rebase(&self, from: &Self, to: &Self) -> Option<Self> {
        let rest = self.relative_to(from)?;
        Some(if rest.is_empty() {
            to.clone()
        } else {
            Self(if to.is_root() { format!("/{rest}") } else { format!("{}/{rest}", to.0) })
        })
    }

    /// Bounds for a range scan over everything strictly below this path:
    /// `path >= low AND path < high`. Exact, unlike `LIKE`, for names that
    /// contain `%` or `_`.
    pub(crate) fn subtree_bounds(&self) -> (String, String) {
        let low = if self.is_root() { "/".to_owned() } else { format!("{}/", self.0) };
        // '0' is the character right after '/'.
        let high = format!("{}0", &low[..low.len() - 1]);
        (low, high)
    }
}

/// A single file name Drive could hold.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0']) && name.len() <= 255
}

impl fmt::Display for IcPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for IcPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> IcPath {
        IcPath::new(s)
    }

    #[test]
    fn normalisation_matches_normpath_of_an_absolute_path() {
        assert_eq!(p("").as_str(), "/");
        assert_eq!(p("///").as_str(), "/");
        assert_eq!(p("a/b").as_str(), "/a/b");
        assert_eq!(p("/a//b/./c/").as_str(), "/a/b/c");
        assert_eq!(p("/a/b/../c").as_str(), "/a/c");
    }

    #[test]
    fn dot_dot_can_never_climb_above_the_root() {
        assert_eq!(p("/..").as_str(), "/");
        assert_eq!(p("/../../etc/passwd").as_str(), "/etc/passwd");
        assert_eq!(p("/allowed/../outside.txt").as_str(), "/outside.txt");
    }

    #[test]
    fn parents_and_names() {
        assert_eq!(p("/a/b").parent(), p("/a"));
        assert_eq!(p("/a").parent(), IcPath::root());
        assert_eq!(IcPath::root().parent(), IcPath::root());
        assert_eq!(p("/a/b.txt").file_name(), Some("b.txt"));
        assert_eq!(IcPath::root().file_name(), None);
    }

    #[test]
    fn join_refuses_anything_that_is_not_a_plain_name() {
        let base = p("/docs");
        assert_eq!(base.join("a.txt"), Some(p("/docs/a.txt")));
        assert_eq!(IcPath::root().join("a"), Some(p("/a")));
        for bad in ["", ".", "..", "a/b", "a\0b", &"x".repeat(256)] {
            assert_eq!(base.join(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn containment_respects_component_boundaries() {
        assert!(p("/a/b").is_within(&p("/a")));
        assert!(p("/a").is_within(&p("/a")));
        assert!(!p("/ab").is_within(&p("/a")), "a sibling that merely shares a prefix is not inside");
        assert!(p("/anything").is_within(&IcPath::root()));
        assert!(!p("/a").is_within(&p("/a/b")));
    }

    #[test]
    fn relative_paths_and_rebasing_move_subtrees() {
        assert_eq!(p("/a/b/c").relative_to(&p("/a")), Some("b/c"));
        assert_eq!(p("/a").relative_to(&p("/a")), Some(""));
        assert_eq!(p("/x").relative_to(&p("/a")), None);
        assert_eq!(p("/a/b/c").rebase(&p("/a"), &p("/z")), Some(p("/z/b/c")));
        assert_eq!(p("/a").rebase(&p("/a"), &p("/z/y")), Some(p("/z/y")));
        assert_eq!(p("/a/b").rebase(&p("/a"), &IcPath::root()), Some(p("/b")));
        assert_eq!(p("/q").rebase(&p("/a"), &p("/z")), None);
    }

    #[test]
    fn depth_counts_components() {
        assert_eq!(IcPath::root().depth(), 0);
        assert_eq!(p("/a").depth(), 1);
        assert_eq!(p("/a/b/c").depth(), 3);
    }

    #[test]
    fn subtree_bounds_are_exact_for_names_with_sql_wildcards() {
        let (low, high) = p("/my_docs").subtree_bounds();
        assert_eq!((low.as_str(), high.as_str()), ("/my_docs/", "/my_docs0"));
        assert!("/my_docs/file" >= low.as_str() && "/my_docs/file" < high.as_str());
        assert!(!("/myXdocs/file" >= low.as_str() && "/myXdocs/file" < high.as_str()));
        assert!(!("/my_docs2/file" >= low.as_str() && "/my_docs2/file" < high.as_str()));
        let (low, high) = IcPath::root().subtree_bounds();
        assert_eq!((low.as_str(), high.as_str()), ("/", "0"));
    }
}
