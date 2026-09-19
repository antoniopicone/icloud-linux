//! Learning what iCloud has: listing folders and reconciling the answers with
//! the local mirror and database.

use std::collections::{HashSet, VecDeque};

use time::macros::format_description;

use super::*;

/// What a crawl found, and which folders it managed to read.
struct Snapshot {
    /// Discovered items, parents before children.
    items: Vec<(IcPath, Node)>,
    ids: HashSet<String>,
    /// Folders whose children were listed successfully. Only entries directly
    /// inside these may be judged "deleted remotely": a folder that failed to
    /// list, or was deliberately not entered, tells us nothing.
    visited: HashSet<IcPath>,
}

impl Engine {
    /// List the direct children of `path` from iCloud, at most once per
    /// refresh interval, unless `force`. Never recursive.
    ///
    /// Called when a folder is actually opened (crawl mode `lazy`). Listing
    /// creates placeholders for files; it never downloads their contents.
    pub fn list_directory(&self, path: &IcPath, force: bool) -> Result<()> {
        let inner = &self.inner;
        let _guard = inner.path_locks.acquire(path);

        if !force && let Some(at) = inner.state.folder_listed_at(path)? {
            let age = self.now().saturating_sub(at);
            if age < i64::try_from(inner.config.refresh_interval.as_secs()).unwrap_or(i64::MAX) {
                return Ok(());
            }
        }

        let (folder, drivewsid) = if path.is_root() {
            (Node::root(), None)
        } else {
            let Some(entry) = inner.state.get_entry(path)? else {
                // Not a folder we know of; nothing to ask about, and no marker,
                // since it may become listable once its parent has been read.
                return Ok(());
            };
            if entry.tombstone || !entry.is_directory() {
                return Ok(());
            }
            let Some(node) = entry.node() else {
                // Created here and not yet on iCloud: there is nothing remote
                // to list and the folder is, legitimately, complete.
                inner.state.mark_folder_listed(path, None)?;
                return Ok(());
            };
            let id = node.drivewsid.clone();
            (node, Some(id))
        };

        sync_event!(info, "list-directory-start", path = path);
        let children = match inner.drive.children(&folder) {
            Ok(children) => children,
            Err(err) => {
                let err = crate::Error::from(err);
                self.note_failure(&err);
                tracing::error!("could not list {path} from iCloud: {err}");
                return Err(err);
            }
        };

        let mut seen: HashSet<&str> = HashSet::new();
        for child in &children {
            let Some(child_path) = path.join(&child.name) else {
                tracing::warn!("ignoring {:?} in {path}: not a usable file name", child.name);
                continue;
            };
            seen.insert(&child.drivewsid);
            self.apply_remote_item(&child_path, child, false)?;
        }

        // Sweep, limited to the direct children of this folder. A full crawl's
        // sweep spans the whole tree and is only safe once that crawl finished;
        // this one compares against this folder alone, so it cannot mistake a
        // branch nobody has visited for something deleted remotely.
        for entry in inner.state.list_children(path)? {
            let Some(id) = entry.remote_drivewsid.as_deref() else { continue };
            if seen.contains(id) || entry.dirty {
                continue;
            }
            tracing::info!("{} no longer exists on iCloud; removing it locally", entry.path);
            inner.mirror.remove_tree(&entry.path)?;
            inner.state.remove_subtree(&entry.path)?;
        }

        inner.state.mark_folder_listed(path, drivewsid.as_deref())?;
        sync_event!(info, "list-directory-complete", path = path, entries = children.len());
        Ok(())
    }

    /// Reconcile one item iCloud reported with what is stored locally.
    fn apply_remote_item(&self, path: &IcPath, node: &Node, schedule_download: bool) -> Result<()> {
        let state = &self.inner.state;
        let mut existing = state.get_entry_by_remote_id(&node.drivewsid)?;

        if let Some(entry) = &existing
            && entry.dirty
            && entry_conflicts(entry, path, node)
        {
            self.resolve_conflict(entry)?;
            existing = None;
        }
        match existing {
            None => {
                if let Some(occupant) = state.get_entry(path)?
                    && occupant.dirty
                {
                    self.resolve_conflict(&occupant)?;
                }
                self.materialize_remote_entry(path, node, schedule_download)
            }
            // Local changes (including a pending delete) win until uploaded.
            Some(entry) if entry.dirty => Ok(()),
            Some(entry) => self.refresh_clean_entry(entry, path, node, schedule_download),
        }
    }

    fn materialize_remote_entry(&self, path: &IcPath, node: &Node, schedule_download: bool) -> Result<()> {
        sync_event!(
            info,
            "remote-materialize",
            path = path,
            entry_type = node.kind.as_str(),
            drivewsid = node.drivewsid,
            size = node.size
        );
        let entry = Entry::from_remote(path.clone(), node);
        if node.kind.is_directory() {
            self.inner.mirror.ensure_dir(path)?;
        } else {
            self.inner.mirror.materialize_placeholder(path, node.size, node.modified)?;
        }
        self.inner.state.upsert_entry(&entry)?;
        // Listing a folder means the user asked for its names, not its
        // contents: with `schedule_download == false` each file stays a
        // placeholder (right icon, right size) until it is really opened.
        if node.kind == NodeKind::File && !entry.hydrated && schedule_download {
            self.schedule_download(path);
        }
        Ok(())
    }

    fn refresh_clean_entry(&self, entry: Entry, path: &IcPath, node: &Node, schedule_download: bool) -> Result<()> {
        let (state, mirror) = (&self.inner.state, &self.inner.mirror);
        let mut entry = entry;
        let moved = entry.path != *path;

        if moved {
            sync_event!(info, "remote-rename", path = entry.path, target_path = path, entry_type = node.kind.as_str());
            if mirror.exists(&entry.path) {
                mirror.rename(&entry.path, path)?;
            }
            state.rename_tree(&entry.path, path, false, true)?;
            if let Some(moved) = state.get_entry(path)? {
                entry = moved;
            }
        }

        if node.kind.is_directory() {
            mirror.ensure_dir(path)?;
            state.upsert_entry(&Entry {
                hydrated: true,
                local_sha256: entry.local_sha256.clone(),
                last_synced_at: entry.last_synced_at,
                ..Entry::from_remote(path.clone(), node)
            })?;
            return Ok(());
        }

        // Every change to an item bumps its etag, a rename included, so after a
        // rename or move only size and modification time say whether the
        // *content* changed. Throwing away a large downloaded file because it
        // was renamed elsewhere would be needless.
        let changed =
            entry.size != node.size || entry.mtime != node.modified || (!moved && entry.remote_etag != node.etag);
        let mut hydrated = entry.hydrated && !changed;
        if changed {
            sync_event!(
                info,
                "remote-update",
                path = path,
                old_etag = entry.remote_etag,
                new_etag = node.etag,
                size = node.size
            );
            mirror.materialize_placeholder(path, node.size, node.modified)?;
            hydrated = node.size == 0;
        }
        state.upsert_entry(&Entry {
            hydrated,
            local_sha256: if hydrated { entry.local_sha256.clone() } else { None },
            last_synced_at: entry.last_synced_at,
            ..Entry::from_remote(path.clone(), node)
        })?;
        if !hydrated && schedule_download {
            self.schedule_download(path);
        }
        Ok(())
    }

    /// Both sides changed. Keep the local version next to the remote one as
    /// `<name>.local-conflict-<timestamp>` and let it upload as a new file.
    fn resolve_conflict(&self, entry: &Entry) -> Result<()> {
        let (state, mirror) = (&self.inner.state, &self.inner.mirror);
        let conflict = self.unique_conflict_path(&entry.path)?;
        tracing::warn!("conflict on {}; keeping the local version as {conflict}", entry.path);
        if mirror.exists(&entry.path) {
            mirror.rename(&entry.path, &conflict)?;
        }
        state.detach_subtree_as_conflict(&entry.path, &conflict)?;
        for child in state.fetch_subtree(&conflict)? {
            state.queue_op("conflict-copy", &child.path, None)?;
        }
        Ok(())
    }

    fn unique_conflict_path(&self, path: &IcPath) -> Result<IcPath> {
        let stamp = time::OffsetDateTime::now_utc()
            .format(format_description!("[year][month][day][hour][minute][second]"))
            .unwrap_or_else(|_| self.now().to_string());
        let name = path.file_name().unwrap_or("root");
        let parent = path.parent();
        for attempt in 1.. {
            let suffix = if attempt == 1 { String::new() } else { format!("-{attempt}") };
            let candidate = parent.join(&format!("{name}.local-conflict-{stamp}{suffix}"));
            let Some(candidate) = candidate else { break };
            if !self.inner.mirror.exists(&candidate) && self.inner.state.get_entry(&candidate)?.is_none() {
                return Ok(candidate);
            }
        }
        // A name too long to extend: fall back to a short one.
        Ok(parent.join(&format!("conflict-{stamp}")).unwrap_or_else(|| path.clone()))
    }

    // ---- full crawl -----------------------------------------------------------

    /// Crawl the whole drive and reconcile. Used at startup and on every
    /// refresh in [`CrawlMode::Full`].
    pub fn initial_scan(&self) -> Result<()> {
        let started = self.now();
        let snapshot = self.crawl_remote()?;
        self.apply_snapshot(&snapshot, Some(started))
    }

    fn crawl_remote(&self) -> Result<Snapshot> {
        tracing::info!("starting remote metadata crawl");
        let inner = &self.inner;
        let mut snapshot = Snapshot { items: Vec::new(), ids: HashSet::new(), visited: HashSet::new() };
        let mut queue = VecDeque::from([(Node::root(), IcPath::root())]);
        let started = Instant::now();
        let mut last_report = started;
        let mut folders = 0usize;

        while let Some((folder, path)) = queue.pop_front() {
            if inner.stop.is_raised() {
                return Err(crate::Error::Setup("interrupted".into()));
            }
            folders += 1;
            let children = match inner.drive.children(&folder) {
                Ok(children) => children,
                Err(err) => {
                    let err = crate::Error::from(err);
                    self.note_failure(&err);
                    tracing::error!("could not enumerate {path}: {err}");
                    continue;
                }
            };
            snapshot.visited.insert(path.clone());

            for child in children {
                let Some(child_path) = path.join(&child.name) else {
                    tracing::warn!("ignoring {:?} in {path}: not a usable file name", child.name);
                    continue;
                };
                if child.kind.is_directory() && inner.policy.should_descend(&child_path) {
                    queue.push_back((child.clone(), child_path.clone()));
                }
                snapshot.ids.insert(child.drivewsid.clone());
                snapshot.items.push((child_path, child));
            }

            if folders == 1 || folders.is_multiple_of(25) || last_report.elapsed() >= Duration::from_secs(5) {
                tracing::info!(
                    "crawl progress: {folders} folders scanned, {} entries found, {} folders queued",
                    snapshot.items.len(),
                    queue.len()
                );
                last_report = Instant::now();
            }
        }
        tracing::info!(
            "crawl complete: {} entries in {folders} folders, {:.1}s",
            snapshot.items.len(),
            started.elapsed().as_secs_f32()
        );
        Ok(snapshot)
    }

    fn apply_snapshot(&self, snapshot: &Snapshot, crawl_started_at: Option<i64>) -> Result<()> {
        let (state, mirror) = (&self.inner.state, &self.inner.mirror);
        for (path, node) in &snapshot.items {
            self.apply_remote_item(path, node, true)?;
        }

        for entry in state.list_entries()? {
            let Some(id) = entry.remote_drivewsid.as_deref() else { continue };
            if snapshot.ids.contains(id) || !snapshot.visited.contains(&entry.parent_path) {
                continue;
            }
            if entry.tombstone {
                // Already gone from iCloud; the pending delete has nothing left to do.
                state.remove_subtree(&entry.path)?;
                continue;
            }
            if entry.dirty {
                tracing::warn!("{} was deleted on iCloud but changed here; keeping it to upload", entry.path);
                state.clear_remote_identity(&entry.path)?;
                continue;
            }
            if let (Some(started), Some(synced)) = (crawl_started_at, entry.last_synced_at)
                && synced >= started
            {
                tracing::info!("keeping {} because it synced while the crawl was running", entry.path);
                continue;
            }
            tracing::info!("{} was deleted on iCloud; removing it locally", entry.path);
            mirror.remove_tree(&entry.path)?;
            state.remove_subtree(&entry.path)?;
        }
        Ok(())
    }

    // ---- refreshing ---------------------------------------------------------------

    /// Refresh remote metadata the way the crawl mode dictates, logging rather
    /// than failing.
    pub(super) fn run_refresh(&self, reason: &str, force: bool) {
        sync_event!(info, "refresh-start", reason = reason);
        match self.refresh_blocking_with(force) {
            Ok(()) => sync_event!(info, "refresh-complete", reason = reason),
            Err(err) => {
                self.note_failure(&err);
                tracing::error!("remote refresh failed ({reason}): {err}");
            }
        }
    }

    /// Refresh now, on the calling thread: a full crawl in full mode, or a
    /// re-listing of every folder browsed so far in lazy mode.
    pub fn refresh_blocking(&self) -> Result<()> {
        self.refresh_blocking_with(true)
    }

    fn refresh_blocking_with(&self, force: bool) -> Result<()> {
        match self.inner.config.crawl_mode {
            CrawlMode::Full => self.initial_scan(),
            CrawlMode::Lazy => self.refresh_listed_folders(force),
        }
    }

    /// In lazy mode there is no "whole tree" to rescan: only folders somebody
    /// has opened have ever been shown, so only those are worth refreshing.
    /// With `force` all of them are re-listed, otherwise only stale ones.
    fn refresh_listed_folders(&self, force: bool) -> Result<()> {
        let state = &self.inner.state;
        let folders = if force {
            state.list_listed_folders()?
        } else {
            state.list_stale_folder_listings(self.inner.config.refresh_interval.as_secs())?
        };
        if folders.is_empty() {
            sync_event!(debug, "lazy-refresh-nothing-to-do");
            return Ok(());
        }
        sync_event!(info, "lazy-refresh-start", folders = folders.len());
        for folder in folders {
            if self.inner.stop.is_raised() {
                break;
            }
            if let Err(err) = self.list_directory(&folder, true) {
                tracing::error!("could not refresh {folder}: {err}");
            }
        }
        sync_event!(info, "lazy-refresh-complete");
        Ok(())
    }

    /// Give every hydrated-or-not file in the database to the download queue.
    pub(super) fn schedule_all_unhydrated(&self) -> Result<()> {
        let paths = self.inner.state.list_unhydrated_paths()?;
        self.inner.planned.store(paths.len() as u64, Ordering::Relaxed);
        self.inner.completed.store(0, Ordering::Relaxed);
        if paths.is_empty() {
            tracing::info!("background warm-up skipped: everything is already local");
        } else {
            tracing::info!("background warm-up scheduled for {} files", paths.len());
        }
        for path in &paths {
            self.schedule_download(path);
        }
        Ok(())
    }
}

/// Did the remote item move or change under a locally modified entry?
fn entry_conflicts(entry: &Entry, path: &IcPath, node: &Node) -> bool {
    entry.synced_path.as_ref().is_some_and(|synced| synced != path)
        || entry.remote_etag.as_ref().is_some_and(|etag| Some(etag) != node.etag.as_ref())
}

/// Names of the children of `folder` in the database, for tests and tools.
#[cfg(test)]
fn child_names(engine: &Engine, folder: &str) -> Vec<String> {
    let mut names: Vec<_> = engine
        .inner
        .state
        .list_children(&IcPath::new(folder))
        .unwrap()
        .into_iter()
        .filter(|e| !e.tombstone)
        .map(|e| e.path.file_name().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use icloud_api::memory::Outage;

    use super::{testing::Rig, *};

    fn seeded() -> Rig {
        let rig = Rig::new();
        rig.drive.add_file("/", "top.txt", b"top", 100);
        rig.drive.add_folder("/", "Docs");
        rig.drive.add_file("/Docs", "a.txt", b"alpha", 200);
        rig.drive.add_folder("/Docs", "Deep");
        rig.drive.add_file("/Docs/Deep", "z.txt", b"zeta", 300);
        rig
    }

    // ---- lazy listing -----------------------------------------------------------

    #[test]
    fn listing_the_root_shows_only_its_children_as_placeholders() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();

        assert_eq!(child_names(&rig.engine, "/"), ["Docs", "top.txt"]);
        assert!(rig.entry("/Docs/a.txt").is_none(), "listing must not recurse");

        let top = rig.entry("/top.txt").unwrap();
        assert_eq!((top.size, top.mtime, top.hydrated, top.dirty), (3, 100, false, false));
        let meta = rig.engine.inner.mirror.stat(&Rig::path("/top.txt")).unwrap();
        assert_eq!((meta.len(), meta.mtime()), (3, 100), "the placeholder has the remote size and time");
        assert!(rig.engine.inner.mirror.is_dir(&Rig::path("/Docs")));
    }

    #[test]
    fn listing_never_downloads_contents() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        assert_eq!(rig.drive.calls_matching("open:"), 0);
        assert_eq!(rig.engine.stats().downloads_queued, 0);
    }

    #[test]
    fn a_fresh_listing_is_not_repeated_but_force_repeats_it() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        assert_eq!(rig.drive.calls_matching("children:/"), 1);
        rig.engine.list_directory(&IcPath::root(), true).unwrap();
        assert_eq!(rig.drive.calls_matching("children:/"), 2);
    }

    #[test]
    fn an_expired_listing_is_repeated() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.inner.state.mark_folder_listed_at(&IcPath::root(), None, now() - 10_000).unwrap();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        assert_eq!(rig.drive.calls_matching("children:/"), 2);
    }

    #[test]
    fn subfolders_are_listed_from_their_own_id() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&Rig::path("/Docs"), false).unwrap();
        assert_eq!(child_names(&rig.engine, "/Docs"), ["Deep", "a.txt"]);
        assert!(rig.entry("/Docs/Deep/z.txt").is_none());
        assert_eq!(rig.drive.calls_matching("children:/Docs"), 1);
    }

    #[test]
    fn a_failed_listing_reports_the_error_and_leaves_no_marker() {
        let rig = seeded();
        rig.drive.set_outage(Some(Outage::Offline));
        assert!(rig.engine.list_directory(&IcPath::root(), false).is_err());
        assert!(rig.engine.inner.state.folder_listed_at(&IcPath::root()).unwrap().is_none());
        rig.drive.set_outage(None);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        assert!(rig.entry("/top.txt").is_some());
    }

    #[test]
    fn an_expired_session_is_remembered() {
        let rig = seeded();
        rig.drive.set_outage(Some(Outage::SessionExpired));
        let _ = rig.engine.list_directory(&IcPath::root(), false);
        assert!(rig.engine.auth_lost());
    }

    #[test]
    fn unknown_and_local_only_folders_are_handled_without_the_network() {
        let rig = seeded();
        rig.engine.list_directory(&Rig::path("/never-seen"), false).unwrap();
        assert!(rig.engine.inner.state.folder_listed_at(&Rig::path("/never-seen")).unwrap().is_none());

        rig.make_local_dir("/mine");
        rig.engine.list_directory(&Rig::path("/mine"), false).unwrap();
        assert!(rig.engine.inner.state.folder_listed_at(&Rig::path("/mine")).unwrap().is_some());
        assert_eq!(rig.drive.calls_matching("children:/mine"), 0);
    }

    #[test]
    fn a_file_is_not_a_listable_folder() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&Rig::path("/top.txt"), false).unwrap();
        assert!(rig.engine.inner.state.folder_listed_at(&Rig::path("/top.txt")).unwrap().is_none());
    }

    #[test]
    fn remote_deletions_remove_only_direct_children_of_the_listed_folder() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&Rig::path("/Docs"), false).unwrap();
        rig.engine.list_directory(&Rig::path("/Docs/Deep"), false).unwrap();

        rig.drive.remove("/top.txt");
        rig.drive.remove("/Docs/a.txt");
        rig.engine.list_directory(&IcPath::root(), true).unwrap();

        assert!(rig.entry("/top.txt").is_none());
        assert!(!rig.engine.inner.mirror.exists(&Rig::path("/top.txt")));
        assert!(rig.entry("/Docs/a.txt").is_some(), "/Docs was not re-listed, so it must be left alone");
        assert!(rig.entry("/Docs/Deep/z.txt").is_some());
    }

    #[test]
    fn remote_edits_turn_a_hydrated_file_back_into_a_placeholder() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/top.txt")).unwrap();
        assert_eq!(rig.local("/top.txt"), b"top");

        rig.drive.modify("/top.txt", b"changed elsewhere", 900);
        rig.engine.list_directory(&IcPath::root(), true).unwrap();

        let entry = rig.entry("/top.txt").unwrap();
        assert!(!entry.hydrated);
        assert_eq!((entry.size, entry.mtime), (17, 900));
        assert_eq!(rig.engine.inner.mirror.stat(&Rig::path("/top.txt")).unwrap().len(), 17);
        rig.engine.ensure_local_file(&Rig::path("/top.txt")).unwrap();
        assert_eq!(rig.local("/top.txt"), b"changed elsewhere");
    }

    #[test]
    fn an_unchanged_hydrated_file_stays_hydrated_across_a_relist() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/top.txt")).unwrap();
        rig.engine.list_directory(&IcPath::root(), true).unwrap();
        assert!(rig.entry("/top.txt").unwrap().hydrated);
        assert_eq!(rig.drive.calls_matching("open:"), 1);
    }

    #[test]
    fn remote_renames_and_moves_follow_the_id() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/top.txt")).unwrap();

        rig.drive.relocate("/top.txt", "/", "renamed.txt");
        rig.engine.list_directory(&IcPath::root(), true).unwrap();

        assert!(rig.entry("/top.txt").is_none());
        let moved = rig.entry("/renamed.txt").unwrap();
        assert!(moved.hydrated, "a rename must not throw the cached content away");
        assert_eq!(rig.local("/renamed.txt"), b"top");
        assert!(!rig.engine.inner.mirror.exists(&Rig::path("/top.txt")));
    }

    #[test]
    fn hostile_remote_names_are_ignored() {
        let rig = seeded();
        rig.drive.add_file("/", "..", b"x", 1);
        rig.drive.add_file("/", "a/b", b"x", 1);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        assert_eq!(child_names(&rig.engine, "/"), ["Docs", "top.txt"]);
    }

    // ---- conflicts --------------------------------------------------------------

    #[test]
    fn a_local_edit_and_a_remote_edit_keep_both_versions() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/top.txt")).unwrap();
        rig.write_local("/top.txt", b"my local edit");
        rig.drive.modify("/top.txt", b"their remote edit", 900);

        rig.engine.list_directory(&IcPath::root(), true).unwrap();

        // The remote version is back under the original name, as a placeholder.
        let theirs = rig.entry("/top.txt").unwrap();
        assert!(!theirs.dirty && theirs.remote_drivewsid.is_some());
        // The local version survives as a conflict copy, waiting to upload.
        let copies: Vec<_> =
            child_names(&rig.engine, "/").into_iter().filter(|n| n.contains(".local-conflict-")).collect();
        assert_eq!(copies.len(), 1, "{copies:?}");
        let copy = rig.entry(&format!("/{}", copies[0])).unwrap();
        assert!(copy.dirty && copy.remote_drivewsid.is_none());
        assert_eq!(rig.local(&format!("/{}", copies[0])), b"my local edit");
    }

    #[test]
    fn a_locally_created_file_colliding_with_a_new_remote_one_is_kept() {
        let rig = seeded();
        rig.write_local("/new.txt", b"local");
        rig.drive.add_file("/", "new.txt", b"remote", 5);
        rig.engine.list_directory(&IcPath::root(), false).unwrap();

        assert!(!rig.entry("/new.txt").unwrap().dirty, "the remote file owns the name");
        assert!(child_names(&rig.engine, "/").iter().any(|n| n.starts_with("new.txt.local-conflict-")));
    }

    #[test]
    fn a_pending_local_delete_survives_a_relist() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.inner.mirror.remove_file(&Rig::path("/top.txt")).unwrap();
        rig.engine.inner.state.mark_tombstone(&Rig::path("/top.txt")).unwrap();

        rig.engine.list_directory(&IcPath::root(), true).unwrap();

        let entry = rig.entry("/top.txt").unwrap();
        assert!(entry.tombstone, "the delete must not be undone by a refresh");
        assert!(!rig.engine.inner.mirror.exists(&Rig::path("/top.txt")));
    }

    #[test]
    fn conflict_copies_never_overwrite_each_other() {
        let rig = seeded();
        rig.write_local("/a.txt", b"one");
        let first = rig.engine.unique_conflict_path(&Rig::path("/a.txt")).unwrap();
        rig.engine.inner.mirror.create_file(&first).unwrap();
        let second = rig.engine.unique_conflict_path(&Rig::path("/a.txt")).unwrap();
        assert_ne!(first, second);
        assert!(first.as_str().starts_with("/a.txt.local-conflict-"));
    }

    // ---- lazy refresh -------------------------------------------------------------

    #[test]
    fn a_forced_lazy_refresh_relists_every_browsed_folder_and_only_those() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&Rig::path("/Docs"), false).unwrap();
        rig.drive.add_file("/Docs", "b.txt", b"beta", 1);
        rig.drive.add_file("/Docs/Deep", "never-browsed.txt", b"x", 1);

        rig.engine.refresh_blocking().unwrap();

        assert_eq!(child_names(&rig.engine, "/Docs"), ["Deep", "a.txt", "b.txt"]);
        assert_eq!(rig.drive.calls_matching("children:/Docs/Deep"), 0);
    }

    #[test]
    fn an_unforced_lazy_refresh_only_touches_stale_folders() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.list_directory(&Rig::path("/Docs"), false).unwrap();
        rig.engine.inner.state.mark_folder_listed_at(&Rig::path("/Docs"), None, now() - 10_000).unwrap();
        let before = rig.drive.calls_matching("children:");

        rig.engine.refresh_listed_folders(false).unwrap();

        assert_eq!(rig.drive.calls_matching("children:") - before, 1);
        assert_eq!(rig.drive.calls_matching("children:/Docs"), 2);
    }

    // ---- full crawl ---------------------------------------------------------------

    #[test]
    fn a_full_crawl_discovers_the_whole_tree() {
        let rig = seeded_full();
        rig.engine.initial_scan().unwrap();
        for path in ["/top.txt", "/Docs", "/Docs/a.txt", "/Docs/Deep", "/Docs/Deep/z.txt"] {
            assert!(rig.entry(path).is_some(), "{path}");
        }
        assert_eq!(rig.drive.calls_matching("open:"), 0, "crawling reads metadata only");
    }

    fn seeded_full() -> Rig {
        let rig = Rig::full();
        rig.drive.add_file("/", "top.txt", b"top", 100);
        rig.drive.add_folder("/", "Docs");
        rig.drive.add_file("/Docs", "a.txt", b"alpha", 200);
        rig.drive.add_folder("/Docs", "Deep");
        rig.drive.add_file("/Docs/Deep", "z.txt", b"zeta", 300);
        rig
    }

    #[test]
    fn a_full_crawl_removes_what_was_deleted_remotely() {
        let rig = seeded_full();
        rig.engine.initial_scan().unwrap();
        rig.drive.remove("/Docs/Deep");
        rig.drive.remove("/top.txt");
        rig.engine.initial_scan().unwrap();
        assert!(rig.entry("/Docs/Deep").is_none() && rig.entry("/Docs/Deep/z.txt").is_none());
        assert!(rig.entry("/top.txt").is_none());
        assert!(rig.entry("/Docs/a.txt").is_some());
    }

    #[test]
    fn a_folder_that_fails_to_list_is_not_mistaken_for_deleted() {
        let rig = seeded_full();
        rig.engine.initial_scan().unwrap();
        rig.drive.fail_listing_of("/Docs");
        rig.engine.initial_scan().unwrap();
        assert!(rig.entry("/Docs/a.txt").is_some(), "an unreadable folder tells us nothing");
        assert!(rig.entry("/Docs/Deep/z.txt").is_some());
    }

    #[test]
    fn a_full_crawl_keeps_dirty_files_that_icloud_no_longer_has() {
        let rig = seeded_full();
        rig.engine.initial_scan().unwrap();
        rig.engine.ensure_local_file(&Rig::path("/top.txt")).unwrap();
        rig.write_local("/top.txt", b"precious edit");
        rig.drive.remove("/top.txt");

        rig.engine.initial_scan().unwrap();

        let entry = rig.entry("/top.txt").unwrap();
        assert!(entry.dirty && entry.remote_drivewsid.is_none(), "it must upload again as a new file");
        assert_eq!(rig.local("/top.txt"), b"precious edit");
    }

    #[test]
    fn a_restricted_crawl_does_not_walk_outside_the_boundary() {
        let rig = Rig::with(
            SyncPolicy::new(&["/Docs/Deep"], &[] as &[&str]),
            EngineConfig {
                crawl_mode: CrawlMode::Full,
                warmup_mode: WarmupMode::Lazy,
                auto_sync: false,
                ..EngineConfig::default()
            },
        );
        rig.drive.add_folder("/", "Docs");
        rig.drive.add_folder("/Docs", "Deep");
        rig.drive.add_folder("/", "Photos");
        rig.drive.add_file("/Photos", "p.jpg", b"x", 1);
        rig.drive.add_file("/Docs/Deep", "z.txt", b"z", 1);

        rig.engine.initial_scan().unwrap();

        assert!(rig.entry("/Photos").is_some(), "top-level folders are still shown");
        assert!(rig.entry("/Photos/p.jpg").is_none());
        assert!(rig.entry("/Docs/Deep/z.txt").is_some());
        assert_eq!(rig.drive.calls_matching("children:/Photos"), 0);
    }

    #[test]
    fn a_crawl_stops_promptly_when_asked() {
        let rig = seeded_full();
        rig.engine.shutdown();
        assert!(rig.engine.initial_scan().is_err());
        assert!(rig.entry("/top.txt").is_none(), "an interrupted crawl must not be applied");
    }

    #[test]
    fn entries_synced_during_the_crawl_are_not_swept() {
        let rig = seeded_full();
        rig.engine.initial_scan().unwrap();
        // Simulate a file uploaded while the crawl ran: known locally, synced
        // "after" the crawl started, but absent from the crawl's snapshot.
        let mut entry = rig.entry("/top.txt").unwrap();
        entry.remote_drivewsid = Some("FILE::late".into());
        entry.last_synced_at = Some(now() + 5);
        rig.engine.inner.state.upsert_entry(&entry).unwrap();
        rig.engine.initial_scan().unwrap();
        assert!(rig.entry("/top.txt").is_some());
    }

    #[test]
    fn conflict_detection_compares_paths_and_etags() {
        let node = |etag: &str| Node { etag: Some(etag.into()), ..Node::root() };
        let mut entry = Entry::local(IcPath::new("/a"), NodeKind::File, 0);
        entry.synced_path = Some(IcPath::new("/a"));
        entry.remote_etag = Some("e1".into());
        assert!(!entry_conflicts(&entry, &IcPath::new("/a"), &node("e1")));
        assert!(entry_conflicts(&entry, &IcPath::new("/a"), &node("e2")), "etag changed remotely");
        assert!(entry_conflicts(&entry, &IcPath::new("/b"), &node("e1")), "moved remotely");
        entry.remote_etag = None;
        entry.synced_path = None;
        assert!(!entry_conflicts(&entry, &IcPath::new("/b"), &node("e2")), "never synced: nothing to conflict with");
    }

    #[test]
    fn interrupted_state_can_be_reconciled_after_the_mirror_is_damaged() {
        let rig = seeded();
        rig.engine.list_directory(&IcPath::root(), false).unwrap();
        rig.engine.ensure_local_file(&Rig::path("/top.txt")).unwrap();
        rig.write_local("/local-only.txt", b"mine");

        // While the daemon was down, the cache lost a file and a directory.
        rig.engine.inner.mirror.remove_file(&Rig::path("/top.txt")).unwrap();
        rig.engine.inner.mirror.remove_tree(&Rig::path("/Docs")).unwrap();
        rig.engine.inner.mirror.remove_file(&Rig::path("/local-only.txt")).unwrap();

        rig.engine.reconcile_persistent_cache().unwrap();

        assert!(rig.engine.inner.mirror.is_dir(&Rig::path("/Docs")));
        assert!(!rig.entry("/top.txt").unwrap().hydrated, "content is gone, so it must be fetched again");
        assert_eq!(rig.engine.inner.mirror.stat(&Rig::path("/top.txt")).unwrap().len(), 3);
        assert_eq!(
            rig.local("/local-only.txt"),
            b"",
            "a local-only file is recreated empty rather than lost from the index"
        );
    }
}
