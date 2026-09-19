//! Mapping between the kernel's inode numbers and iCloud paths.
//!
//! The kernel talks in inodes; everything below this layer talks in paths.
//! An inode is handed out the first time a path is seen and stays valid until
//! the path is deleted. Renames move the mapping so that open files keep
//! working.

use std::collections::HashMap;

use icloud_core::IcPath;

pub(crate) const ROOT_INO: u64 = 1;

#[derive(Debug)]
pub(crate) struct InodeTable {
    by_ino: HashMap<u64, IcPath>,
    by_path: HashMap<IcPath, u64>,
    next: u64,
}

impl InodeTable {
    pub(crate) fn new() -> Self {
        let mut table = Self { by_ino: HashMap::new(), by_path: HashMap::new(), next: ROOT_INO + 1 };
        table.by_ino.insert(ROOT_INO, IcPath::root());
        table.by_path.insert(IcPath::root(), ROOT_INO);
        table
    }

    pub(crate) fn path(&self, ino: u64) -> Option<&IcPath> {
        self.by_ino.get(&ino)
    }

    /// The inode for `path`, assigning one on first sight.
    pub(crate) fn ino_for(&mut self, path: &IcPath) -> u64 {
        if let Some(ino) = self.by_path.get(path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_ino.insert(ino, path.clone());
        self.by_path.insert(path.clone(), ino);
        ino
    }

    /// Forget `path` and everything below it (after `unlink` or `rmdir`).
    pub(crate) fn remove_subtree(&mut self, path: &IcPath) {
        if path.is_root() {
            return;
        }
        let doomed: Vec<IcPath> = self.by_path.keys().filter(|p| p.is_within(path)).cloned().collect();
        for p in doomed {
            if let Some(ino) = self.by_path.remove(&p) {
                self.by_ino.remove(&ino);
            }
        }
    }

    /// Follow a rename: the inodes under `from` now live under `to`. Whatever
    /// occupied `to` is gone.
    pub(crate) fn rename(&mut self, from: &IcPath, to: &IcPath) {
        if from == to || from.is_root() || to.is_root() {
            return;
        }
        self.remove_subtree(to);
        let moving: Vec<(IcPath, IcPath)> =
            self.by_path.keys().filter_map(|p| p.rebase(from, to).map(|new| (p.clone(), new))).collect();
        for (old, new) in moving {
            if let Some(ino) = self.by_path.remove(&old) {
                self.by_ino.insert(ino, new.clone());
                self.by_path.insert(new, ino);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_ino.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> IcPath {
        IcPath::new(s)
    }

    #[test]
    fn the_root_is_inode_one_and_numbers_are_stable() {
        let mut table = InodeTable::new();
        assert_eq!(table.path(ROOT_INO), Some(&IcPath::root()));
        let a = table.ino_for(&p("/a"));
        assert_eq!(table.ino_for(&p("/a")), a, "asking twice gives the same inode");
        assert_ne!(table.ino_for(&p("/b")), a);
        assert_eq!(table.ino_for(&IcPath::root()), ROOT_INO);
        assert_eq!(table.path(a), Some(&p("/a")));
        assert_eq!(table.path(999), None);
    }

    #[test]
    fn removing_a_subtree_spares_lookalikes() {
        let mut table = InodeTable::new();
        let dir = table.ino_for(&p("/d"));
        let child = table.ino_for(&p("/d/f"));
        let sibling = table.ino_for(&p("/d2"));
        table.remove_subtree(&p("/d"));
        assert!(table.path(dir).is_none() && table.path(child).is_none());
        assert!(table.path(sibling).is_some(), "/d2 is not inside /d");
        table.remove_subtree(&IcPath::root());
        assert!(table.path(sibling).is_some(), "the root and its contents are never dropped wholesale");
    }

    #[test]
    fn a_rename_keeps_inodes_alive_under_their_new_paths() {
        let mut table = InodeTable::new();
        let dir = table.ino_for(&p("/old"));
        let child = table.ino_for(&p("/old/f"));
        table.rename(&p("/old"), &p("/new"));
        assert_eq!(table.path(dir), Some(&p("/new")));
        assert_eq!(table.path(child), Some(&p("/new/f")));
        assert_eq!(table.ino_for(&p("/new/f")), child);
        assert_ne!(table.ino_for(&p("/old")), dir, "the old name is free again and gets a fresh inode");
    }

    #[test]
    fn a_rename_over_an_existing_path_evicts_the_victim() {
        let mut table = InodeTable::new();
        let source = table.ino_for(&p("/a"));
        let victim = table.ino_for(&p("/b"));
        table.rename(&p("/a"), &p("/b"));
        assert_eq!(table.path(source), Some(&p("/b")));
        assert!(table.path(victim).is_none());
        assert_eq!(table.len(), 2, "root plus the moved file");
    }

    #[test]
    fn degenerate_renames_change_nothing() {
        let mut table = InodeTable::new();
        let a = table.ino_for(&p("/a"));
        table.rename(&p("/a"), &p("/a"));
        table.rename(&IcPath::root(), &p("/x"));
        table.rename(&p("/a"), &IcPath::root());
        assert_eq!(table.path(a), Some(&p("/a")));
    }
}
