//! Pushing local changes to iCloud: new files and folders, edited content,
//! renames and moves, and deletions.
//!
//! One pass ([`Engine::sync_dirty`]) handles everything currently marked
//! dirty. Failures are logged and left for the next pass; nothing is ever
//! dropped because an upload failed.

use std::{fs::File, os::unix::fs::MetadataExt};

use super::*;

/// What identifies a file's content at a given moment, cheaply.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    mtime: (i64, i64),
}

impl Fingerprint {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self { size: meta.len(), mtime: (meta.mtime(), meta.mtime_nsec()) }
    }
}

impl Engine {
    /// Push every locally changed entry that lies within the sync boundary.
    pub fn sync_dirty(&self) -> Result<()> {
        let inner = &self.inner;
        let _pass = lock(&inner.upload_lock);

        let dirty: Vec<Entry> =
            inner.state.list_dirty_entries()?.into_iter().filter(|entry| self.allowed_to_sync(entry)).collect();
        if dirty.is_empty() {
            return Ok(());
        }
        sync_event!(debug, "dirty-scan", dirty_count = dirty.len());

        // Deletions first, deepest first, so children go before their folder.
        let mut tombstones: Vec<&Entry> = dirty.iter().filter(|e| e.tombstone).collect();
        tombstones.sort_by(|a, b| (b.path.depth(), &b.path).cmp(&(a.path.depth(), &a.path)));
        // Then everything else, folders first and shallow before deep, so a
        // parent exists on iCloud before what goes in it.
        let mut regular: Vec<&Entry> = dirty.iter().filter(|e| !e.tombstone).collect();
        regular.sort_by(|a, b| {
            (!a.is_directory(), a.path.depth(), &a.path).cmp(&(!b.is_directory(), b.path.depth(), &b.path))
        });

        for entry in tombstones {
            if inner.stop.is_raised() || self.auth_lost() {
                return Ok(());
            }
            self.sync_tombstone(entry);
        }
        for entry in regular {
            if inner.stop.is_raised() || self.auth_lost() {
                return Ok(());
            }
            // Earlier steps (a parent syncing its children's paths) may have
            // changed this entry since the scan.
            let Some(fresh) = inner.state.get_entry(&entry.path)? else { continue };
            if fresh.tombstone || !fresh.dirty {
                continue;
            }
            let outcome = if fresh.is_directory() { self.sync_directory(&fresh) } else { self.sync_file(&fresh) };
            if let Err(err) = outcome {
                self.note_failure(&err);
                tracing::error!("failed to sync {}: {err}", fresh.path);
            }
        }
        Ok(())
    }

    /// May this entry's change be sent? A move needs both ends inside the
    /// boundary; a delete is judged by the path iCloud knows the item under.
    fn allowed_to_sync(&self, entry: &Entry) -> bool {
        let policy = &self.inner.policy;
        let allowed = if entry.tombstone {
            policy.allows(entry.synced_path.as_ref().unwrap_or(&entry.path))
        } else {
            policy.allows(&entry.path) && entry.synced_path.as_ref().is_none_or(|synced| policy.allows(synced))
        };
        if !allowed {
            sync_event!(warn, "dirty-skip-disallowed", path = entry.path, synced_path = entry.synced_path);
        }
        allowed
    }

    fn sync_tombstone(&self, entry: &Entry) {
        let inner = &self.inner;
        sync_event!(info, "delete-start", path = entry.path, remote = entry.remote_drivewsid.is_some());
        if let Some(node) = entry.node()
            && let Err(err) = inner.drive.delete(&node, inner.config.delete_mode)
        {
            let err = crate::Error::from(err);
            self.note_failure(&err);
            tracing::error!("could not delete {} on iCloud: {err}", entry.path);
            return;
        }
        if let Err(err) = inner.state.remove_subtree(&entry.path) {
            tracing::error!("could not forget {}: {err}", entry.path);
            return;
        }
        sync_event!(info, "delete-complete", path = entry.path);
    }

    fn sync_directory(&self, entry: &Entry) -> Result<()> {
        let state = &self.inner.state;
        let Some(parent) = self.ensure_remote_parent(&entry.path)? else { return Ok(()) };
        let name = entry.path.file_name().unwrap_or_default();
        sync_event!(
            info,
            "directory-sync-start",
            path = entry.path,
            remote_exists = entry.remote_drivewsid.is_some(),
            synced_path = entry.synced_path
        );

        if entry.remote_drivewsid.is_none() {
            self.inner.drive.create_folder(&parent, name)?;
            let meta = self.refresh_child_meta(&entry.path.parent(), name)?;
            state.mark_clean(&entry.path, Some(&meta), None)?;
            sync_event!(info, "directory-create-complete", path = entry.path);
            return Ok(());
        }
        if entry.synced_path.as_ref().is_some_and(|synced| *synced != entry.path) {
            self.sync_move_or_rename(entry)?;
            // The move changed the folder's etag; keep the fresh one, or the
            // next rename or delete would be refused as stale.
            let meta = self.refresh_child_meta(&entry.path.parent(), name)?;
            state.mark_synced_subtree(&entry.path)?;
            state.mark_clean(&entry.path, Some(&meta), None)?;
        } else {
            state.mark_synced_subtree(&entry.path)?;
        }
        sync_event!(info, "directory-sync-complete", path = entry.path);
        Ok(())
    }

    fn sync_file(&self, entry: &Entry) -> Result<()> {
        let (state, mirror, drive) = (&self.inner.state, &self.inner.mirror, &self.inner.drive);
        let path = &entry.path;
        let Some(parent) = self.ensure_remote_parent(path)? else { return Ok(()) };
        let name = path.file_name().unwrap_or_default();
        sync_event!(
            info,
            "file-sync-start",
            path = path,
            remote_exists = entry.remote_drivewsid.is_some(),
            synced_path = entry.synced_path
        );

        if !mirror.exists(path) {
            // Gone from disk without going through the mount: treat as deleted.
            state.mark_tombstone(path)?;
            sync_event!(warn, "file-missing-marked-tombstone", path = path);
            return Ok(());
        }

        // A renamed placeholder has never had its bytes; iCloud has them, so
        // only the name needs to change and nothing must be uploaded.
        let content_changed = entry.remote_drivewsid.is_none() || state.has_pending_content_change(path)?;
        if content_changed {
            self.ensure_local_file(path)?;
            let hydrated = state.get_entry(path)?.is_some_and(|e| e.hydrated);
            if !hydrated {
                return Err(crate::Error::Setup(format!(
                    "{path} has local changes but no local content; not uploading"
                )));
            }
        }

        let mut entry = entry.clone();
        if entry.remote_drivewsid.is_some() && entry.synced_path.as_ref().is_some_and(|synced| synced != path) {
            self.sync_move_or_rename(&entry)?;
            entry = state.get_entry(path)?.unwrap_or(entry);
        }

        if !content_changed {
            let meta = self.refresh_child_meta(&path.parent(), name)?;
            state.mark_clean(path, Some(&meta), None)?;
            sync_event!(info, "file-sync-complete", path = path, size = meta.size);
            return Ok(());
        }

        // iCloud gives a re-uploaded name a numbered copy unless the old one
        // is out of the way. If the upload then fails the local file is still
        // dirty, so the next pass tries again; a failed delete is harmless.
        let before = Fingerprint::of(&mirror.stat(path)?);
        if let Some(node) = entry.node() {
            let _ = drive.delete(&node, DeleteMode::Permanent);
        }
        let file = File::open(mirror.local_path(path))?;
        drive.upload(&parent, name, file, mirror.stat(path)?.mtime())?;
        let meta = self.refresh_child_meta(&path.parent(), name)?;

        if Fingerprint::of(&mirror.stat(path)?) == before {
            let checksum = mirror.sha256(path)?;
            // Line the local mtime up with what iCloud recorded, so a restart
            // does not read the difference as a remote change.
            let _ = mirror.set_mtime(path, meta.modified);
            state.mark_clean(path, Some(&meta), Some(&checksum))?;
            sync_event!(info, "file-sync-complete", path = path, size = meta.size);
        } else {
            state.mark_uploaded_but_dirty(path, &meta)?;
            sync_event!(info, "file-changed-during-upload", path = path);
        }
        Ok(())
    }

    /// Carry a local rename or move out on iCloud.
    fn sync_move_or_rename(&self, entry: &Entry) -> Result<()> {
        let Some(synced) = &entry.synced_path else { return Ok(()) };
        let Some(mut node) = entry.node() else { return Ok(()) };
        let drive = &self.inner.drive;
        let (old_parent, new_parent) = (synced.parent(), entry.path.parent());
        let (old_name, new_name) = (synced.file_name().unwrap_or_default(), entry.path.file_name().unwrap_or_default());

        sync_event!(info, "move-start", path = synced, target_path = entry.path);
        if old_parent != new_parent {
            let destination = self
                .remote_node_for_path(&new_parent)?
                .ok_or_else(|| crate::Error::Setup(format!("{new_parent} is not on iCloud yet")))?;
            drive.move_to(&node, &destination)?;
            // The move changed the etag the rename must present.
            node = drive.node(&node.drivewsid, node.share_id.as_ref())?;
        }
        if old_name != new_name {
            drive.rename(&node, new_name)?;
        }
        sync_event!(info, "move-complete", path = synced, target_path = entry.path);
        Ok(())
    }

    /// The remote folder that will hold `path`, syncing missing ancestors first.
    fn ensure_remote_parent(&self, path: &IcPath) -> Result<Option<Node>> {
        let parent = path.parent();
        if parent.is_root() {
            return self.root_node().map(Some);
        }
        let state = &self.inner.state;
        let Some(mut entry) = state.get_entry(&parent)? else { return Ok(None) };
        if entry.dirty {
            self.sync_directory(&entry)?;
            match state.get_entry(&parent)? {
                Some(fresh) => entry = fresh,
                None => return Ok(None),
            }
        }
        Ok(entry.node())
    }

    fn root_node(&self) -> Result<Node> {
        let mut cached = lock(&self.inner.root);
        if let Some(root) = cached.as_ref() {
            return Ok(root.clone());
        }
        let root = self.inner.drive.root()?;
        *cached = Some(root.clone());
        Ok(root)
    }

    fn remote_node_for_path(&self, path: &IcPath) -> Result<Option<Node>> {
        if path.is_root() {
            return self.root_node().map(Some);
        }
        Ok(self.inner.state.get_entry(path)?.and_then(|entry| entry.node()))
    }

    /// Fetch iCloud's current record of `name` inside `parent_path`.
    fn refresh_child_meta(&self, parent_path: &IcPath, name: &str) -> Result<Node> {
        let parent = self
            .remote_node_for_path(parent_path)?
            .ok_or_else(|| crate::Error::Setup(format!("{parent_path} is not on iCloud")))?;
        self.inner
            .drive
            .children(&parent)?
            .into_iter()
            .find(|child| child.name == name)
            .ok_or_else(|| crate::Error::Setup(format!("{name} not found under {parent_path} after syncing it")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use icloud_api::memory::Outage;

    use super::{super::testing::Rig, *};

    fn listed_rig() -> Rig {
        let rig = Rig::new();
        rig.drive.add_folder("/", "Docs");
        rig.drive.add_file("/Docs", "old.txt", b"old content", 100);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&Rig::path("/Docs"), false).unwrap();
        rig
    }

    fn uploads(rig: &Rig) -> usize {
        rig.drive.calls_matching("upload:")
    }

    // ---- new content -----------------------------------------------------------

    #[test]
    fn a_new_local_file_is_uploaded_and_becomes_clean() {
        let rig = listed_rig();
        rig.write_local("/Docs/new.txt", b"hello from linux");

        rig.engine.sync_dirty().unwrap();

        assert_eq!(rig.drive.contents("/Docs/new.txt").unwrap(), b"hello from linux");
        let entry = rig.entry("/Docs/new.txt").unwrap();
        assert!(!entry.dirty && entry.remote_drivewsid.is_some());
        assert_eq!(entry.synced_path, Some(Rig::path("/Docs/new.txt")));
        assert_eq!(rig.engine.inner.state.pending_op_count().unwrap(), 0);
        assert!(entry.local_sha256.is_some());
    }

    #[test]
    fn a_new_file_at_the_root_is_uploaded() {
        let rig = listed_rig();
        rig.write_local("/root.txt", b"r");
        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.contents("/root.txt").unwrap(), b"r");
    }

    #[test]
    fn new_folders_are_created_before_the_files_inside_them() {
        let rig = listed_rig();
        rig.make_local_dir("/Docs/new");
        rig.make_local_dir("/Docs/new/deeper");
        rig.write_local("/Docs/new/deeper/f.txt", b"deep");

        rig.engine.sync_dirty().unwrap();

        assert_eq!(rig.drive.contents("/Docs/new/deeper/f.txt").unwrap(), b"deep");
        assert!(!rig.entry("/Docs/new").unwrap().dirty);
        assert!(rig.entry("/Docs/new/deeper").unwrap().remote_drivewsid.is_some());
    }

    #[test]
    fn an_upload_failure_leaves_the_file_dirty_for_the_next_pass() {
        let rig = listed_rig();
        rig.write_local("/Docs/new.txt", b"data");
        rig.drive.set_outage(Some(Outage::Offline));
        rig.engine.sync_dirty().unwrap();
        assert!(rig.entry("/Docs/new.txt").unwrap().dirty);

        rig.drive.set_outage(None);
        rig.engine.sync_dirty().unwrap();
        assert!(!rig.entry("/Docs/new.txt").unwrap().dirty);
        assert_eq!(rig.drive.contents("/Docs/new.txt").unwrap(), b"data");
    }

    #[test]
    fn an_empty_new_file_uploads_too() {
        let rig = listed_rig();
        rig.write_local("/Docs/empty", b"");
        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.contents("/Docs/empty").unwrap(), b"");
    }

    // ---- edits ---------------------------------------------------------------------

    #[test]
    fn an_edited_file_replaces_the_remote_one_without_a_numbered_duplicate() {
        let rig = listed_rig();
        rig.engine.ensure_local_file(&Rig::path("/Docs/old.txt")).unwrap();
        rig.write_local("/Docs/old.txt", b"edited content");

        rig.engine.sync_dirty().unwrap();

        assert_eq!(rig.drive.contents("/Docs/old.txt").unwrap(), b"edited content");
        assert!(!rig.drive.exists("/Docs/old 2.txt"), "the old version must be removed first");
        let entry = rig.entry("/Docs/old.txt").unwrap();
        assert!(!entry.dirty);
        assert_eq!(entry.size, 14);
    }

    #[test]
    fn after_an_upload_the_local_mtime_matches_what_icloud_recorded() {
        let rig = listed_rig();
        rig.write_local("/Docs/new.txt", b"x");
        rig.engine.sync_dirty().unwrap();
        let entry = rig.entry("/Docs/new.txt").unwrap();
        assert_eq!(rig.engine.inner.mirror.stat(&Rig::path("/Docs/new.txt")).unwrap().mtime(), entry.mtime);
    }

    #[test]
    fn a_write_that_lands_during_the_upload_is_not_lost() {
        let rig = listed_rig();
        rig.write_local("/Docs/busy.txt", b"first version");
        let engine = rig.engine.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        rig.drive.on_upload(move || {
            if !flag.swap(true, Ordering::SeqCst) {
                // Another process appends while the bytes are in flight.
                let path = Rig::path("/Docs/busy.txt");
                engine.inner.mirror.write_at(&path, 13, b" + more").unwrap();
                engine.inner.state.mark_dirty(&path, Some(20), Some(now()), Some(true), true).unwrap();
                engine.inner.state.queue_op("update", &path, None).unwrap();
            }
        });

        rig.engine.sync_dirty().unwrap();
        let after_first = rig.entry("/Docs/busy.txt").unwrap();
        assert!(after_first.dirty, "the newer content still has to go up");
        assert_eq!(rig.drive.contents("/Docs/busy.txt").unwrap(), b"first version");

        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.contents("/Docs/busy.txt").unwrap(), b"first version + more");
        assert!(!rig.entry("/Docs/busy.txt").unwrap().dirty);
    }

    // ---- renames and moves ----------------------------------------------------------

    fn rename_locally(rig: &Rig, from: &str, to: &str) {
        let (from, to) = (Rig::path(from), Rig::path(to));
        rig.engine.inner.mirror.rename(&from, &to).unwrap();
        rig.engine.inner.state.rename_tree(&from, &to, true, false).unwrap();
        rig.engine.inner.state.queue_op("rename", &from, Some(&to)).unwrap();
    }

    #[test]
    fn renaming_a_synced_file_renames_it_remotely_without_re_uploading() {
        let rig = listed_rig();
        rig.engine.ensure_local_file(&Rig::path("/Docs/old.txt")).unwrap();
        rename_locally(&rig, "/Docs/old.txt", "/Docs/renamed.txt");

        rig.engine.sync_dirty().unwrap();

        assert_eq!(rig.drive.contents("/Docs/renamed.txt").unwrap(), b"old content");
        assert!(!rig.drive.exists("/Docs/old.txt"));
        assert_eq!(uploads(&rig), 0, "a rename must not re-upload the bytes");
        assert_eq!(rig.drive.calls_matching("delete:"), 0, "and must not delete the remote copy");
        let entry = rig.entry("/Docs/renamed.txt").unwrap();
        assert!(!entry.dirty);
        assert_eq!(entry.synced_path, Some(Rig::path("/Docs/renamed.txt")));
    }

    #[test]
    fn renaming_a_file_that_was_never_downloaded_needs_no_download_either() {
        let rig = listed_rig();
        rename_locally(&rig, "/Docs/old.txt", "/Docs/renamed.txt");
        rig.engine.sync_dirty().unwrap();
        assert!(rig.drive.exists("/Docs/renamed.txt"));
        assert_eq!(rig.drive.calls_matching("open:"), 0);
        assert_eq!(uploads(&rig), 0);
    }

    #[test]
    fn moving_a_file_to_another_folder_moves_it_remotely() {
        let rig = listed_rig();
        rig.drive.add_folder("/", "Other");
        rig.engine.list_directory(&IcPath::root(), true).unwrap();
        rename_locally(&rig, "/Docs/old.txt", "/Other/old.txt");

        rig.engine.sync_dirty().unwrap();

        assert!(rig.drive.exists("/Other/old.txt") && !rig.drive.exists("/Docs/old.txt"));
        assert_eq!(uploads(&rig), 0);
        assert!(!rig.entry("/Other/old.txt").unwrap().dirty);
    }

    #[test]
    fn move_and_rename_together_do_both() {
        let rig = listed_rig();
        rig.drive.add_folder("/", "Other");
        rig.engine.list_directory(&IcPath::root(), true).unwrap();
        rename_locally(&rig, "/Docs/old.txt", "/Other/new-name.txt");
        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.contents("/Other/new-name.txt").unwrap(), b"old content");
    }

    #[test]
    fn renaming_a_folder_renames_it_once_and_keeps_its_contents() {
        let rig = listed_rig();
        rename_locally(&rig, "/Docs", "/Papers");
        rig.engine.sync_dirty().unwrap();

        assert_eq!(rig.drive.contents("/Papers/old.txt").unwrap(), b"old content");
        assert!(!rig.drive.exists("/Docs"));
        assert_eq!(rig.drive.calls_matching("rename:"), 1);
        assert_eq!(uploads(&rig), 0);
        let folder = rig.entry("/Papers").unwrap();
        assert!(!folder.dirty);
        assert_eq!(folder.synced_path, Some(Rig::path("/Papers")));
        let child = rig.entry("/Papers/old.txt").unwrap();
        assert_eq!(child.synced_path, Some(Rig::path("/Papers/old.txt")), "children follow the folder");
    }

    #[test]
    fn a_renamed_folder_keeps_a_fresh_etag_for_later_operations() {
        let rig = listed_rig();
        rename_locally(&rig, "/Docs", "/Papers");
        rig.engine.sync_dirty().unwrap();
        let stored = rig.entry("/Papers").unwrap().remote_etag.unwrap();
        let remote = rig.drive.node(&rig.entry("/Papers").unwrap().remote_drivewsid.unwrap(), None).unwrap();
        assert_eq!(remote.etag.as_deref(), Some(stored.as_str()));
    }

    #[test]
    fn renaming_over_a_synced_file_deletes_the_displaced_one() {
        let rig = listed_rig();
        rig.drive.add_file("/Docs", "target.txt", b"displaced", 5);
        rig.engine.list_directory(&Rig::path("/Docs"), true).unwrap();
        rename_locally(&rig, "/Docs/old.txt", "/Docs/target.txt");

        rig.engine.sync_dirty().unwrap();

        assert_eq!(rig.drive.contents("/Docs/target.txt").unwrap(), b"old content");
        assert_eq!(rig.drive.trashed(), ["target.txt"], "the displaced file is recoverable from Recently Deleted");
        assert!(!rig.drive.exists("/Docs/target 2.txt"));
        assert!(rig.engine.inner.state.list_dirty_entries().unwrap().is_empty());
    }

    // ---- deletes ---------------------------------------------------------------------

    fn delete_locally(rig: &Rig, path: &str) {
        let p = Rig::path(path);
        rig.engine.inner.mirror.remove_tree(&p).unwrap();
        rig.engine.inner.state.mark_tombstone(&p).unwrap();
        rig.engine.inner.state.queue_op("delete", &p, None).unwrap();
    }

    #[test]
    fn deleting_a_file_moves_it_to_the_trash_by_default() {
        let rig = listed_rig();
        delete_locally(&rig, "/Docs/old.txt");
        rig.engine.sync_dirty().unwrap();
        assert!(!rig.drive.exists("/Docs/old.txt"));
        assert_eq!(rig.drive.trashed(), ["old.txt"]);
        assert!(rig.entry("/Docs/old.txt").is_none(), "the tombstone is cleared once iCloud has it");
    }

    #[test]
    fn permanent_delete_mode_really_deletes() {
        let rig = Rig::with(
            SyncPolicy::unrestricted(),
            EngineConfig { auto_sync: false, delete_mode: DeleteMode::Permanent, ..EngineConfig::default() },
        );
        rig.drive.add_file("/", "gone.txt", b"x", 1);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        delete_locally(&rig, "/gone.txt");
        rig.engine.sync_dirty().unwrap();
        assert!(!rig.drive.exists("/gone.txt"));
        assert!(rig.drive.trashed().is_empty());
    }

    #[test]
    fn a_failed_remote_delete_keeps_the_tombstone() {
        let rig = listed_rig();
        delete_locally(&rig, "/Docs/old.txt");
        rig.drive.set_outage(Some(Outage::Offline));
        rig.engine.sync_dirty().unwrap();
        assert!(rig.entry("/Docs/old.txt").unwrap().tombstone);
        rig.drive.set_outage(None);
        rig.engine.sync_dirty().unwrap();
        assert!(rig.entry("/Docs/old.txt").is_none());
    }

    #[test]
    fn deleting_a_folder_tree_removes_children_before_the_folder() {
        let rig = listed_rig();
        delete_locally(&rig, "/Docs/old.txt");
        delete_locally(&rig, "/Docs");
        rig.engine.sync_dirty().unwrap();
        let calls = rig.drive.calls();
        let child = calls.iter().position(|c| c.starts_with("delete:/Docs/old.txt")).unwrap();
        let folder = calls.iter().position(|c| c.starts_with("delete:/Docs:")).unwrap();
        assert!(child < folder, "{calls:?}");
    }

    #[test]
    fn deleting_something_never_uploaded_touches_nothing_remote() {
        let rig = listed_rig();
        rig.write_local("/Docs/scratch.txt", b"temp");
        let p = Rig::path("/Docs/scratch.txt");
        rig.engine.inner.mirror.remove_file(&p).unwrap();
        rig.engine.inner.state.remove_entry(&p).unwrap();
        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.calls_matching("delete:"), 0);
        assert_eq!(uploads(&rig), 0);
    }

    #[test]
    fn a_file_that_vanished_from_disk_is_treated_as_deleted_not_uploaded_empty() {
        let rig = listed_rig();
        rig.engine.ensure_local_file(&Rig::path("/Docs/old.txt")).unwrap();
        rig.write_local("/Docs/old.txt", b"edit");
        rig.engine.inner.mirror.remove_file(&Rig::path("/Docs/old.txt")).unwrap();

        rig.engine.sync_dirty().unwrap(); // marks the tombstone
        rig.engine.sync_dirty().unwrap(); // and this one deletes it

        assert_eq!(uploads(&rig), 0);
        assert_eq!(rig.drive.trashed(), ["old.txt"]);
    }

    // ---- the sync boundary --------------------------------------------------------------

    #[test]
    fn changes_outside_the_boundary_are_never_sent() {
        let rig = Rig::with(
            SyncPolicy::new(&["/Allowed"], &["/Allowed/Skip"]),
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_folder("/", "Allowed");
        rig.drive.add_folder("/Allowed", "Skip");
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&Rig::path("/Allowed"), false).unwrap();

        rig.write_local("/outside.txt", b"x");
        rig.write_local("/Allowed/Skip/file.txt", b"x");
        rig.write_local("/Allowed/ok.txt", b"x");
        rig.engine.sync_dirty().unwrap();

        assert!(!rig.drive.exists("/outside.txt"));
        assert!(!rig.drive.exists("/Allowed/Skip/file.txt"));
        assert_eq!(rig.drive.contents("/Allowed/ok.txt").unwrap(), b"x");
        assert!(rig.entry("/outside.txt").unwrap().dirty, "still queued, in case the boundary changes");
    }

    #[test]
    fn a_move_out_of_the_boundary_is_not_sent() {
        let rig = Rig::with(
            SyncPolicy::new(&["/Allowed"], &[] as &[&str]),
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_folder("/", "Allowed");
        rig.drive.add_file("/Allowed", "f.txt", b"x", 1);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&Rig::path("/Allowed"), false).unwrap();
        rename_locally(&rig, "/Allowed/f.txt", "/Outside.txt");
        rig.engine.sync_dirty().unwrap();
        assert!(rig.drive.exists("/Allowed/f.txt"));
        assert_eq!(rig.drive.calls_matching("move:") + rig.drive.calls_matching("rename:"), 0);
    }

    #[test]
    fn deletes_outside_the_boundary_are_not_sent_either() {
        let rig = Rig::with(
            SyncPolicy::new(&["/Allowed"], &[] as &[&str]),
            EngineConfig { auto_sync: false, ..EngineConfig::default() },
        );
        rig.drive.add_file("/", "protected.txt", b"x", 1);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        delete_locally(&rig, "/protected.txt");
        rig.engine.sync_dirty().unwrap();
        assert!(rig.drive.exists("/protected.txt"));
    }

    // ---- passes ----------------------------------------------------------------------------

    #[test]
    fn a_pass_with_nothing_to_do_makes_no_calls() {
        let rig = listed_rig();
        let before = rig.drive.calls().len();
        rig.engine.sync_dirty().unwrap();
        assert_eq!(rig.drive.calls().len(), before);
    }

    #[test]
    fn a_pass_stops_when_the_session_is_lost_instead_of_hammering_icloud() {
        let rig = listed_rig();
        for name in ["a", "b", "c", "d"] {
            rig.write_local(&format!("/Docs/{name}.txt"), b"x");
        }
        rig.drive.set_outage(Some(Outage::SessionExpired));
        let before = rig.drive.calls().len();
        rig.engine.sync_dirty().unwrap();
        assert!(rig.engine.auth_lost());
        let attempts = rig.drive.calls().len() - before;
        assert!(attempts <= 2, "expected the pass to give up early, saw {attempts} calls");
    }

    #[test]
    fn concurrent_passes_do_not_overlap() {
        let rig = listed_rig();
        for i in 0..5 {
            rig.write_local(&format!("/Docs/f{i}.txt"), b"x");
        }
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let engine = rig.engine.clone();
                std::thread::spawn(move || engine.sync_dirty().unwrap())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(uploads(&rig), 5, "each file must be uploaded exactly once");
    }
}
