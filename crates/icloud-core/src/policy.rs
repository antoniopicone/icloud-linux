//! The synchronisation boundary.
//!
//! `sync_paths` is an allow-list and `exclude_paths` a deny-list. Both are
//! evaluated for every hydration, upload, rename and delete, and the deny-list
//! always wins. Paths outside the boundary are still *listed* (as stubs) but
//! their contents are never fetched and they can never be changed.

use crate::path::IcPath;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPolicy {
    /// `None` means "everything"; an empty configured list is the same thing.
    allow: Option<Vec<IcPath>>,
    deny: Vec<IcPath>,
}

impl SyncPolicy {
    pub fn new<A: AsRef<str>, D: AsRef<str>>(sync_paths: &[A], exclude_paths: &[D]) -> Self {
        let allow: Vec<IcPath> = sync_paths.iter().map(|p| IcPath::new(p.as_ref())).collect();
        Self {
            allow: (!allow.is_empty()).then_some(allow),
            deny: exclude_paths.iter().map(|p| IcPath::new(p.as_ref())).collect(),
        }
    }

    pub fn unrestricted() -> Self {
        Self::default()
    }

    /// May the contents of `path` be downloaded, changed, or removed?
    pub fn allows(&self, path: &IcPath) -> bool {
        if self.deny.iter().any(|prefix| path.is_within(prefix)) {
            return false;
        }
        self.allow.as_ref().is_none_or(|allow| allow.iter().any(|prefix| path.is_within(prefix)))
    }

    /// Must a crawl descend into this folder? True for anything on the way to,
    /// or inside, an allowed path, so a crawl restricted to `/Downloads` does
    /// not walk the whole drive.
    pub fn should_descend(&self, folder: &IcPath) -> bool {
        self.allow
            .as_ref()
            .is_none_or(|allow| allow.iter().any(|prefix| folder.is_within(prefix) || prefix.is_within(folder)))
    }

    pub fn is_restricted(&self) -> bool {
        self.allow.is_some() || !self.deny.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allow: &[&str], deny: &[&str]) -> SyncPolicy {
        SyncPolicy::new(allow, deny)
    }

    fn p(s: &str) -> IcPath {
        IcPath::new(s)
    }

    #[test]
    fn no_configuration_allows_everything() {
        let policy = SyncPolicy::unrestricted();
        assert!(policy.allows(&p("/any/thing")));
        assert!(!policy.is_restricted());
        assert!(policy.should_descend(&p("/any")));
    }

    #[test]
    fn an_empty_allow_list_is_unrestricted_not_deny_all() {
        let policy = policy(&[], &[]);
        assert!(policy.allows(&p("/x")));
    }

    #[test]
    fn allow_list_is_a_boundary_at_component_granularity() {
        let policy = policy(&["/Downloads"], &[]);
        assert!(policy.allows(&p("/Downloads")));
        assert!(policy.allows(&p("/Downloads/a/b.txt")));
        assert!(!policy.allows(&p("/Downloads2/a.txt")), "shared prefix is not containment");
        assert!(!policy.allows(&p("/Other")));
    }

    #[test]
    fn deny_list_wins_over_allow_list() {
        let policy = policy(&["/Downloads"], &["/Downloads/Large Archive"]);
        assert!(policy.allows(&p("/Downloads/small.txt")));
        assert!(!policy.allows(&p("/Downloads/Large Archive")));
        assert!(!policy.allows(&p("/Downloads/Large Archive/file")));
    }

    #[test]
    fn trailing_slashes_in_configuration_are_normalised() {
        let policy = policy(&["/allowed/"], &["/allowed/excluded/"]);
        assert!(policy.allows(&p("/allowed/file.txt")));
        assert!(!policy.allows(&p("/allowed/excluded/file.txt")));
    }

    #[test]
    fn dot_segments_cannot_escape_an_allowed_path() {
        let policy = policy(&["/allowed"], &[]);
        assert!(!policy.allows(&p("/allowed/../outside.txt")));
    }

    #[test]
    fn excluding_the_root_blocks_everything() {
        assert!(!policy(&[], &["/"]).allows(&p("/a")));
        assert!(policy(&["/"], &[]).allows(&p("/a")));
    }

    #[test]
    fn crawls_descend_only_towards_or_inside_allowed_paths() {
        let policy = policy(&["/Documents/Work"], &[]);
        assert!(policy.should_descend(&p("/Documents")), "ancestor of an allowed path");
        assert!(policy.should_descend(&p("/Documents/Work")));
        assert!(policy.should_descend(&p("/Documents/Work/2024")), "inside an allowed path");
        assert!(!policy.should_descend(&p("/Photos")));
        assert!(!policy.should_descend(&p("/Documents/Personal")));
    }
}
