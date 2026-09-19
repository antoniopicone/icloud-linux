//! The sync state database.
//!
//! One SQLite file records, for every path the mirror knows, what iCloud said
//! about it and what has happened to it locally since. The schema is the one
//! the Python implementation used, so an existing database is read as it is.
//!
//! Compared with that implementation:
//!
//! * multi-row changes (`rename_tree`, `remove_subtree`, …) run in one
//!   transaction, so a crash cannot leave half a subtree renamed;
//! * subtree queries use exact range scans instead of `LIKE`, for which the
//!   `_` and `%` that are common in file names are wildcards: removing
//!   `/my_docs` used to remove `/myXdocs` as well;
//! * overwriting a synced path by renaming another over it keeps a tombstone
//!   for the displaced remote file instead of violating the primary key.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use icloud_api::{Node, NodeKind};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::Value;

use crate::{error::Result, path::IcPath};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS entries (
        path TEXT PRIMARY KEY,
        type TEXT NOT NULL,
        parent_path TEXT NOT NULL,
        remote_drivewsid TEXT,
        remote_docwsid TEXT,
        remote_etag TEXT,
        remote_zone TEXT,
        remote_shareid TEXT,
        size INTEGER NOT NULL DEFAULT 0,
        mtime INTEGER NOT NULL DEFAULT 0,
        hydrated INTEGER NOT NULL DEFAULT 0,
        dirty INTEGER NOT NULL DEFAULT 0,
        tombstone INTEGER NOT NULL DEFAULT 0,
        local_sha256 TEXT,
        last_synced_at INTEGER,
        synced_path TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_entries_remote_drivewsid ON entries(remote_drivewsid);
    CREATE INDEX IF NOT EXISTS idx_entries_dirty ON entries(dirty, tombstone);
    CREATE TABLE IF NOT EXISTS pending_ops (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        op TEXT NOT NULL,
        path TEXT NOT NULL,
        target_path TEXT,
        queued_at INTEGER NOT NULL,
        retry_count INTEGER NOT NULL DEFAULT 0,
        last_error TEXT
    );
    CREATE TABLE IF NOT EXISTS folder_listings (
        path TEXT PRIMARY KEY,
        remote_drivewsid TEXT,
        listed_at INTEGER NOT NULL
    );
";

const ENTRY_COLUMNS: &str = "path, type, parent_path, remote_drivewsid, remote_docwsid, remote_etag, remote_zone, \
    remote_shareid, size, mtime, hydrated, dirty, tombstone, local_sha256, last_synced_at, synced_path";

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// One row of `entries`.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: IcPath,
    pub kind: NodeKind,
    pub parent_path: IcPath,
    pub remote_drivewsid: Option<String>,
    pub remote_docwsid: Option<String>,
    pub remote_etag: Option<String>,
    pub remote_zone: Option<String>,
    pub remote_shareid: Option<Value>,
    pub size: u64,
    pub mtime: i64,
    /// The contents are present in the mirror (always true for folders).
    pub hydrated: bool,
    /// Changed locally and not yet pushed to iCloud.
    pub dirty: bool,
    /// Deleted locally, waiting for the delete to reach iCloud.
    pub tombstone: bool,
    pub local_sha256: Option<String>,
    pub last_synced_at: Option<i64>,
    /// The path iCloud knows it under; differs from `path` after a local rename.
    pub synced_path: Option<IcPath>,
}

impl Entry {
    /// An item created on this machine: dirty, no remote identity yet.
    pub fn local(path: IcPath, kind: NodeKind, mtime: i64) -> Self {
        Self {
            parent_path: path.parent(),
            path,
            kind,
            remote_drivewsid: None,
            remote_docwsid: None,
            remote_etag: None,
            remote_zone: None,
            remote_shareid: None,
            size: 0,
            mtime,
            hydrated: true,
            dirty: true,
            tombstone: false,
            local_sha256: None,
            last_synced_at: None,
            synced_path: None,
        }
    }

    /// A clean entry describing what iCloud reports at `path`. Files start
    /// out not hydrated, except empty ones, which have nothing to fetch.
    pub fn from_remote(path: IcPath, node: &Node) -> Self {
        let directory = node.kind.is_directory();
        Self {
            parent_path: path.parent(),
            synced_path: Some(path.clone()),
            path,
            kind: node.kind,
            remote_drivewsid: Some(node.drivewsid.clone()),
            remote_docwsid: node.docwsid.clone(),
            remote_etag: node.etag.clone(),
            remote_zone: node.zone.clone(),
            remote_shareid: node.share_id.clone(),
            size: node.size,
            mtime: node.modified,
            hydrated: directory || node.size == 0,
            dirty: false,
            tombstone: false,
            local_sha256: None,
            last_synced_at: None,
        }
    }

    pub fn is_directory(&self) -> bool {
        self.kind.is_directory()
    }

    /// The remote node this entry stands for, if it has ever been on iCloud.
    pub fn node(&self) -> Option<Node> {
        Some(Node {
            drivewsid: self.remote_drivewsid.clone()?,
            docwsid: self.remote_docwsid.clone(),
            etag: self.remote_etag.clone(),
            zone: self.remote_zone.clone(),
            share_id: self.remote_shareid.clone(),
            name: self.path.file_name().unwrap_or("root").to_owned(),
            kind: self.kind,
            size: self.size,
            modified: self.mtime,
        })
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let text = |i: usize| row.get::<_, Option<String>>(i);
        let path = IcPath::new(&row.get::<_, String>(0)?);
        Ok(Self {
            path,
            kind: NodeKind::parse(&row.get::<_, String>(1)?),
            parent_path: IcPath::new(&row.get::<_, String>(2)?),
            remote_drivewsid: text(3)?,
            remote_docwsid: text(4)?,
            remote_etag: text(5)?,
            remote_zone: text(6)?,
            remote_shareid: text(7)?.and_then(|s| serde_json::from_str(&s).ok()),
            size: u64::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
            mtime: row.get(9)?,
            hydrated: row.get(10)?,
            dirty: row.get(11)?,
            tombstone: row.get(12)?,
            local_sha256: text(13)?,
            last_synced_at: row.get(14)?,
            synced_path: text(15)?.map(|s| IcPath::new(&s)),
        })
    }
}

pub struct SyncState {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for SyncState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncState").finish_non_exhaustive()
    }
}

impl SyncState {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // WAL lets `icloudctl hydrate` read the database while the daemon writes.
        // The pragma answers with a row, so it is queried rather than executed.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        // Databases from before shared folders were supported lack this column.
        let has_shareid = conn
            .prepare("PRAGMA table_info(entries)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(std::result::Result::ok)
            .any(|name| name == "remote_shareid");
        if !has_shareid {
            conn.execute("ALTER TABLE entries ADD COLUMN remote_shareid TEXT", [])?;
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ---- reading ---------------------------------------------------------

    pub fn get_entry(&self, path: &IcPath) -> Result<Option<Entry>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE path = ?1"),
                [path.as_str()],
                Entry::from_row,
            )
            .optional()?)
    }

    pub fn get_entry_by_remote_id(&self, drivewsid: &str) -> Result<Option<Entry>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE remote_drivewsid = ?1"),
                [drivewsid],
                Entry::from_row,
            )
            .optional()?)
    }

    fn query_entries(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Entry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, Entry::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_entries(&self) -> Result<Vec<Entry>> {
        self.query_entries(&format!("SELECT {ENTRY_COLUMNS} FROM entries ORDER BY path"), [])
    }

    pub fn count_entries(&self) -> Result<u64> {
        let n: i64 = self.conn().query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Direct children of `parent`, tombstones included.
    pub fn list_children(&self, parent: &IcPath) -> Result<Vec<Entry>> {
        self.query_entries(
            &format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE parent_path = ?1 ORDER BY path"),
            [parent.as_str()],
        )
    }

    pub fn list_unhydrated_paths(&self) -> Result<Vec<IcPath>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT path FROM entries WHERE type = 'file' AND tombstone = 0 AND hydrated = 0 ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.map(|p| p.map(|p| IcPath::new(&p))).collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_dirty_entries(&self) -> Result<Vec<Entry>> {
        self.query_entries(
            &format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE dirty = 1 OR tombstone = 1 ORDER BY path"),
            [],
        )
    }

    /// `path` and everything below it, shallowest first.
    pub fn fetch_subtree(&self, path: &IcPath) -> Result<Vec<Entry>> {
        fetch_subtree(&self.conn(), path)
    }

    // ---- writing entries -------------------------------------------------

    pub fn upsert_entry(&self, entry: &Entry) -> Result<()> {
        self.conn().execute(
            &format!(
                "INSERT INTO entries ({ENTRY_COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
                 ON CONFLICT(path) DO UPDATE SET
                    type = excluded.type, parent_path = excluded.parent_path,
                    remote_drivewsid = excluded.remote_drivewsid, remote_docwsid = excluded.remote_docwsid,
                    remote_etag = excluded.remote_etag, remote_zone = excluded.remote_zone,
                    remote_shareid = excluded.remote_shareid, size = excluded.size, mtime = excluded.mtime,
                    hydrated = excluded.hydrated, dirty = excluded.dirty, tombstone = excluded.tombstone,
                    local_sha256 = excluded.local_sha256, last_synced_at = excluded.last_synced_at,
                    synced_path = excluded.synced_path"
            ),
            params![
                entry.path.as_str(),
                entry.kind.as_str(),
                entry.parent_path.as_str(),
                entry.remote_drivewsid,
                entry.remote_docwsid,
                entry.remote_etag,
                entry.remote_zone,
                entry.remote_shareid.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default()),
                i64::try_from(entry.size).unwrap_or(i64::MAX),
                entry.mtime,
                entry.hydrated,
                entry.dirty,
                entry.tombstone,
                entry.local_sha256,
                entry.last_synced_at,
                entry.synced_path.as_ref().map(IcPath::as_str),
            ],
        )?;
        Ok(())
    }

    pub fn mark_hydrated(
        &self,
        path: &IcPath,
        sha256: Option<&str>,
        size: Option<u64>,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE entries SET hydrated = 1, local_sha256 = COALESCE(?1, local_sha256),
                    size = COALESCE(?2, size), mtime = COALESCE(?3, mtime) WHERE path = ?4",
            params![sha256, size.map(|s| i64::try_from(s).unwrap_or(i64::MAX)), mtime, path.as_str()],
        )?;
        Ok(())
    }

    /// Record a local change. `content_changed` invalidates the stored checksum.
    pub fn mark_dirty(
        &self,
        path: &IcPath,
        size: Option<u64>,
        mtime: Option<i64>,
        hydrated: Option<bool>,
        content_changed: bool,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE entries SET dirty = 1, tombstone = 0,
                    size = COALESCE(?1, size), mtime = COALESCE(?2, mtime), hydrated = COALESCE(?3, hydrated),
                    local_sha256 = CASE WHEN ?4 THEN NULL ELSE local_sha256 END
             WHERE path = ?5",
            params![
                size.map(|s| i64::try_from(s).unwrap_or(i64::MAX)),
                mtime,
                hydrated,
                content_changed,
                path.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn mark_tombstone(&self, path: &IcPath) -> Result<()> {
        self.conn().execute("UPDATE entries SET tombstone = 1, dirty = 1 WHERE path = ?1", [path.as_str()])?;
        Ok(())
    }

    /// The change reached iCloud: clear the dirty flag, adopt what iCloud now
    /// says, and forget the queued operations for the path.
    pub fn mark_clean(&self, path: &IcPath, remote: Option<&Node>, sha256: Option<&str>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE entries SET dirty = 0, tombstone = 0,
                    hydrated = CASE WHEN type = 'file' THEN hydrated ELSE 1 END,
                    remote_drivewsid = COALESCE(?1, remote_drivewsid), remote_docwsid = COALESCE(?2, remote_docwsid),
                    remote_etag = COALESCE(?3, remote_etag), remote_zone = COALESCE(?4, remote_zone),
                    size = COALESCE(?5, size), mtime = COALESCE(?6, mtime),
                    local_sha256 = COALESCE(?7, local_sha256), last_synced_at = ?8, synced_path = path
             WHERE path = ?9",
            params![
                remote.map(|n| n.drivewsid.as_str()),
                remote.and_then(|n| n.docwsid.as_deref()),
                remote.and_then(|n| n.etag.as_deref()),
                remote.and_then(|n| n.zone.as_deref()),
                remote.map(|n| i64::try_from(n.size).unwrap_or(i64::MAX)),
                remote.map(|n| n.modified),
                sha256,
                now(),
                path.as_str(),
            ],
        )?;
        tx.execute("DELETE FROM pending_ops WHERE path = ?1 OR target_path = ?1", [path.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    /// Like [`mark_clean`](Self::mark_clean) but for a file that changed again
    /// while it was being uploaded: adopt the new remote identity, keep the
    /// entry dirty and keep its queued operations, so the newer content goes
    /// up on the next pass.
    pub fn mark_uploaded_but_dirty(&self, path: &IcPath, remote: &Node) -> Result<()> {
        self.conn().execute(
            "UPDATE entries SET remote_drivewsid = ?1, remote_docwsid = ?2, remote_etag = ?3, remote_zone = ?4,
                    last_synced_at = ?5, synced_path = path
             WHERE path = ?6",
            params![remote.drivewsid, remote.docwsid, remote.etag, remote.zone, now(), path.as_str()],
        )?;
        Ok(())
    }

    pub fn remove_entry(&self, path: &IcPath) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM entries WHERE path = ?1", [path.as_str()])?;
        tx.execute("DELETE FROM pending_ops WHERE path = ?1 OR target_path = ?1", [path.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    /// Remove `path` and everything below it, along with their queued
    /// operations and listing markers.
    pub fn remove_subtree(&self, path: &IcPath) -> Result<()> {
        let (low, high) = path.subtree_bounds();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM entries WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
            params![path.as_str(), low, high],
        )?;
        tx.execute(
            "DELETE FROM pending_ops WHERE path = ?1 OR (path >= ?2 AND path < ?3)
                OR target_path = ?1 OR (target_path >= ?2 AND target_path < ?3)",
            params![path.as_str(), low, high],
        )?;
        forget_listings(&tx, path)?;
        tx.commit()?;
        Ok(())
    }

    /// Move the subtree at `old` to `new`.
    ///
    /// If a row already sits at `new` (an overwriting rename) and stands for a
    /// file on iCloud, it is kept as a tombstone under a private key so the
    /// delete still reaches iCloud; otherwise it is dropped.
    pub fn rename_tree(&self, old: &IcPath, new: &IcPath, root_dirty: bool, update_synced: bool) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let subtree = fetch_subtree(&tx, old)?;
        if subtree.is_empty() {
            return Ok(());
        }

        // Clear the way at the destination.
        for displaced in fetch_subtree(&tx, new)? {
            if displaced.remote_drivewsid.is_some() {
                if displaced.tombstone {
                    bury(&tx, &displaced, false)?;
                } else {
                    bury(&tx, &displaced, true)?;
                }
            } else {
                tx.execute("DELETE FROM entries WHERE path = ?1", [displaced.path.as_str()])?;
            }
        }

        for entry in &subtree {
            let Some(updated) = entry.path.rebase(old, new) else { continue };
            let dirty = if root_dirty && entry.path == *old { true } else { entry.dirty };
            let synced = match (&entry.synced_path, update_synced) {
                (Some(synced), true) => synced.rebase(old, new).or_else(|| Some(synced.clone())),
                (synced, _) => synced.clone(),
            };
            tx.execute(
                "UPDATE entries SET path = ?1, parent_path = ?2, dirty = ?3, synced_path = ?4 WHERE path = ?5",
                params![
                    updated.as_str(),
                    updated.parent().as_str(),
                    dirty,
                    synced.as_ref().map(IcPath::as_str),
                    entry.path.as_str()
                ],
            )?;
        }

        // Queued operations follow their paths.
        let ops: Vec<(i64, String, Option<String>)> = {
            let mut stmt = tx.prepare("SELECT id, path, target_path FROM pending_ops")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (id, path, target) in ops {
            let path = IcPath::new(&path);
            let target = target.map(|t| IcPath::new(&t));
            let new_path = path.rebase(old, new);
            let new_target = target.as_ref().and_then(|t| t.rebase(old, new));
            if new_path.is_some() || new_target.is_some() {
                tx.execute(
                    "UPDATE pending_ops SET path = ?1, target_path = ?2 WHERE id = ?3",
                    params![new_path.unwrap_or(path).as_str(), new_target.or(target).as_ref().map(IcPath::as_str), id],
                )?;
            }
        }
        // The moved folders' listing markers are keyed by their old paths.
        forget_listings(&tx, old)?;
        tx.commit()?;
        Ok(())
    }

    /// Something is about to take the place of a path whose deletion has not
    /// reached iCloud yet (a file deleted and immediately recreated). Move the
    /// pending delete out of the way, so the new entry does not overwrite it
    /// and the old remote file is still removed.
    pub fn bury_tombstone(&self, path: &IcPath) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let tombstone = tx
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE path = ?1"),
                [path.as_str()],
                Entry::from_row,
            )
            .optional()?;
        if let Some(entry) = tombstone.filter(|e| e.tombstone) {
            bury(&tx, &entry, false)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `path` and its descendants are known to iCloud under their current
    /// paths; only `path` itself stops being dirty.
    pub fn mark_synced_subtree(&self, path: &IcPath) -> Result<()> {
        let (low, high) = path.subtree_bounds();
        self.conn().execute(
            "UPDATE entries SET synced_path = path,
                    dirty = CASE WHEN path = ?1 THEN 0 ELSE dirty END,
                    tombstone = CASE WHEN path = ?1 THEN 0 ELSE tombstone END,
                    last_synced_at = ?2
             WHERE path = ?1 OR (path >= ?3 AND path < ?4)",
            params![path.as_str(), now(), low, high],
        )?;
        Ok(())
    }

    /// Move a locally changed subtree to `new` and detach it from iCloud, so
    /// it is uploaded as new content there. Used to keep the local side of a
    /// conflict.
    pub fn detach_subtree_as_conflict(&self, old: &IcPath, new: &IcPath) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for entry in fetch_subtree(&tx, old)? {
            let Some(updated) = entry.path.rebase(old, new) else { continue };
            tx.execute(
                "UPDATE entries SET path = ?1, parent_path = ?2, remote_drivewsid = NULL, remote_docwsid = NULL,
                        remote_etag = NULL, remote_zone = NULL, remote_shareid = NULL, synced_path = NULL,
                        dirty = 1, tombstone = 0
                 WHERE path = ?3",
                params![updated.as_str(), updated.parent().as_str(), entry.path.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// iCloud no longer has this item, but the local copy has changes worth
    /// keeping: treat it as brand new.
    pub fn clear_remote_identity(&self, path: &IcPath) -> Result<()> {
        self.conn().execute(
            "UPDATE entries SET remote_drivewsid = NULL, remote_docwsid = NULL, remote_etag = NULL,
                    remote_zone = NULL, remote_shareid = NULL, synced_path = NULL, dirty = 1, tombstone = 0
             WHERE path = ?1",
            [path.as_str()],
        )?;
        Ok(())
    }

    // ---- queued operations ---------------------------------------------------

    /// Note an operation. Deleting something that was only ever created
    /// locally cancels its queued operations instead of adding one.
    pub fn queue_op(&self, op: &str, path: &IcPath, target: Option<&IcPath>) -> Result<()> {
        let conn = self.conn();
        if op == "delete" {
            let created: Option<i64> = conn
                .query_row(
                    "SELECT id FROM pending_ops WHERE path = ?1 AND op IN ('create', 'mkdir')",
                    [path.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            if created.is_some() {
                conn.execute("DELETE FROM pending_ops WHERE path = ?1", [path.as_str()])?;
                return Ok(());
            }
        }
        if op == "update" {
            // Every `write` call reports one; a large file is written in
            // thousands of pieces and one note is all the uploader needs.
            let already: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_ops WHERE path = ?1 AND op IN ('create', 'update'))",
                [path.as_str()],
                |r| r.get(0),
            )?;
            if already {
                return Ok(());
            }
        }
        conn.execute(
            "INSERT INTO pending_ops (op, path, target_path, queued_at) VALUES (?1, ?2, ?3, ?4)",
            params![op, path.as_str(), target.map(IcPath::as_str), now()],
        )?;
        Ok(())
    }

    /// Has the *content* of `path` been written since it was last synced?
    /// A file that was only renamed or touched does not need re-uploading.
    pub fn has_pending_content_change(&self, path: &IcPath) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM pending_ops WHERE path = ?1 AND op IN ('create', 'update', 'conflict-copy'))",
            [path.as_str()],
            |r| r.get(0),
        )?)
    }

    pub fn pending_op_count(&self) -> Result<u64> {
        let n: i64 = self.conn().query_row("SELECT COUNT(*) FROM pending_ops", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    // ---- on-demand listing markers (crawl_mode: lazy) ------------------------

    /// When `path` was last listed from iCloud, if ever. The root has a row of
    /// its own here although it never has one in `entries`.
    pub fn folder_listed_at(&self, path: &IcPath) -> Result<Option<i64>> {
        Ok(self
            .conn()
            .query_row("SELECT listed_at FROM folder_listings WHERE path = ?1", [path.as_str()], |r| r.get(0))
            .optional()?)
    }

    pub fn mark_folder_listed(&self, path: &IcPath, drivewsid: Option<&str>) -> Result<()> {
        self.mark_folder_listed_at(path, drivewsid, now())
    }

    pub(crate) fn mark_folder_listed_at(&self, path: &IcPath, drivewsid: Option<&str>, at: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO folder_listings (path, remote_drivewsid, listed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET remote_drivewsid = excluded.remote_drivewsid, listed_at = excluded.listed_at",
            params![path.as_str(), drivewsid, at],
        )?;
        Ok(())
    }

    pub fn list_listed_folders(&self) -> Result<Vec<IcPath>> {
        self.folder_paths("SELECT path FROM folder_listings ORDER BY listed_at ASC", [])
    }

    pub fn list_stale_folder_listings(&self, ttl_seconds: u64) -> Result<Vec<IcPath>> {
        let cutoff = now().saturating_sub(i64::try_from(ttl_seconds).unwrap_or(i64::MAX));
        self.folder_paths("SELECT path FROM folder_listings WHERE listed_at < ?1 ORDER BY listed_at ASC", [cutoff])
    }

    fn folder_paths(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<IcPath>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
        Ok(rows.map(|p| p.map(|p| IcPath::new(&p))).collect::<rusqlite::Result<_>>()?)
    }

    /// Forget that `path` (and everything below) was listed, so the next
    /// access lists it again.
    pub fn forget_folder_listings(&self, path: &IcPath) -> Result<()> {
        forget_listings(&self.conn(), path)?;
        Ok(())
    }
}

fn fetch_subtree(conn: &Connection, path: &IcPath) -> Result<Vec<Entry>> {
    let (low, high) = path.subtree_bounds();
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries WHERE path = ?1 OR (path >= ?2 AND path < ?3)
         ORDER BY LENGTH(path) ASC, path ASC"
    ))?;
    let rows = stmt.query_map(params![path.as_str(), low, high], Entry::from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn forget_listings(conn: &Connection, path: &IcPath) -> rusqlite::Result<()> {
    let (low, high) = path.subtree_bounds();
    conn.execute(
        "DELETE FROM folder_listings WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
        params![path.as_str(), low, high],
    )?;
    Ok(())
}

/// Re-key `entry` under a private path and make it a pending delete. Its
/// `synced_path` keeps naming the remote identity, which is what the sync
/// boundary is checked against for a delete.
fn bury(tx: &Transaction<'_>, entry: &Entry, make_tombstone: bool) -> rusqlite::Result<()> {
    let graveyard = IcPath::new(&format!("{}\u{0}displaced-{}", entry.path, unique_marker(tx)?));
    let identity = entry.synced_path.as_ref().unwrap_or(&entry.path);
    tx.execute(
        "UPDATE entries SET path = ?1, tombstone = 1, dirty = 1, synced_path = ?2 WHERE path = ?3",
        params![graveyard.as_str(), identity.as_str(), entry.path.as_str()],
    )?;
    let _ = make_tombstone;
    Ok(())
}

/// A random token, for naming displaced tombstones so they cannot collide.
fn unique_marker(tx: &Transaction<'_>) -> rusqlite::Result<String> {
    tx.query_row("SELECT lower(hex(randomblob(8)))", [], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> IcPath {
        IcPath::new(s)
    }

    fn state() -> SyncState {
        SyncState::open_in_memory().unwrap()
    }

    fn remote(path: &str, id: &str, kind: NodeKind, size: u64) -> Entry {
        Entry {
            remote_drivewsid: Some(id.to_owned()),
            remote_etag: Some("e1".into()),
            size,
            hydrated: kind.is_directory() || size == 0,
            synced_path: Some(p(path)),
            ..Entry::local(p(path), kind, 100)
        }
        .clean()
    }

    impl Entry {
        fn clean(mut self) -> Self {
            self.dirty = false;
            self
        }
    }

    #[test]
    fn entries_round_trip_including_shared_folder_ids() {
        let state = state();
        let mut entry = remote("/a.txt", "FILE::1", NodeKind::File, 7);
        entry.remote_shareid = Some(serde_json::json!({"zoneName": "z", "shareName": "s"}));
        entry.local_sha256 = Some("abc".into());
        state.upsert_entry(&entry).unwrap();

        let loaded = state.get_entry(&p("/a.txt")).unwrap().unwrap();
        assert_eq!(loaded, entry);
        assert_eq!(state.get_entry_by_remote_id("FILE::1").unwrap().unwrap().path, p("/a.txt"));
        assert!(state.get_entry(&p("/missing")).unwrap().is_none());
    }

    #[test]
    fn upsert_replaces_an_existing_row() {
        let state = state();
        state.upsert_entry(&remote("/a", "F1", NodeKind::File, 1)).unwrap();
        state.upsert_entry(&remote("/a", "F1", NodeKind::File, 2)).unwrap();
        assert_eq!(state.count_entries().unwrap(), 1);
        assert_eq!(state.get_entry(&p("/a")).unwrap().unwrap().size, 2);
    }

    #[test]
    fn listing_children_uses_the_parent_column() {
        let state = state();
        state.upsert_entry(&remote("/d", "D", NodeKind::Folder, 0)).unwrap();
        state.upsert_entry(&remote("/d/a", "A", NodeKind::File, 1)).unwrap();
        state.upsert_entry(&remote("/d/b", "B", NodeKind::File, 1)).unwrap();
        state.upsert_entry(&remote("/d/sub/c", "C", NodeKind::File, 1)).unwrap();
        let names: Vec<_> = state.list_children(&p("/d")).unwrap().iter().map(|e| e.path.to_string()).collect();
        assert_eq!(names, ["/d/a", "/d/b"]);
    }

    #[test]
    fn dirty_and_unhydrated_queries() {
        let state = state();
        state.upsert_entry(&remote("/h", "H", NodeKind::File, 5)).unwrap();
        state.mark_hydrated(&p("/h"), Some("sum"), None, None).unwrap();
        state.upsert_entry(&remote("/u", "U", NodeKind::File, 5)).unwrap();
        state.upsert_entry(&Entry::local(p("/new"), NodeKind::File, 1)).unwrap();

        assert_eq!(state.list_unhydrated_paths().unwrap(), [p("/u")]);
        let dirty: Vec<_> = state.list_dirty_entries().unwrap().iter().map(|e| e.path.to_string()).collect();
        assert_eq!(dirty, ["/new"]);

        state.mark_tombstone(&p("/h")).unwrap();
        assert_eq!(state.list_dirty_entries().unwrap().len(), 2);
    }

    #[test]
    fn writing_invalidates_the_checksum_but_touching_does_not() {
        let state = state();
        let mut entry = remote("/f", "F", NodeKind::File, 3);
        entry.hydrated = true;
        entry.local_sha256 = Some("old".into());
        state.upsert_entry(&entry).unwrap();

        state.mark_dirty(&p("/f"), None, Some(200), None, false).unwrap();
        let touched = state.get_entry(&p("/f")).unwrap().unwrap();
        assert!(touched.dirty);
        assert_eq!(touched.local_sha256.as_deref(), Some("old"));
        assert_eq!(touched.mtime, 200);

        state.mark_dirty(&p("/f"), Some(9), None, Some(true), true).unwrap();
        let written = state.get_entry(&p("/f")).unwrap().unwrap();
        assert_eq!((written.size, written.local_sha256), (9, None));
    }

    #[test]
    fn mark_clean_adopts_the_remote_identity_and_drops_queued_ops() {
        let state = state();
        state.upsert_entry(&Entry::local(p("/n"), NodeKind::File, 1)).unwrap();
        state.queue_op("create", &p("/n"), None).unwrap();
        let node = Node {
            drivewsid: "FILE::9".into(),
            docwsid: Some("9".into()),
            etag: Some("e9".into()),
            zone: Some("z".into()),
            share_id: None,
            name: "n".into(),
            kind: NodeKind::File,
            size: 4,
            modified: 500,
        };
        state.mark_clean(&p("/n"), Some(&node), Some("sum")).unwrap();

        let entry = state.get_entry(&p("/n")).unwrap().unwrap();
        assert!(!entry.dirty);
        assert_eq!(entry.remote_drivewsid.as_deref(), Some("FILE::9"));
        assert_eq!(entry.synced_path, Some(p("/n")));
        assert_eq!((entry.size, entry.mtime), (4, 500));
        assert!(entry.last_synced_at.is_some());
        assert_eq!(state.pending_op_count().unwrap(), 0);
    }

    #[test]
    fn a_file_changed_during_upload_stays_dirty_and_keeps_its_ops() {
        let state = state();
        state.upsert_entry(&Entry::local(p("/n"), NodeKind::File, 1)).unwrap();
        state.queue_op("update", &p("/n"), None).unwrap();
        let node = Node {
            drivewsid: "FILE::9".into(),
            docwsid: None,
            etag: Some("e".into()),
            zone: None,
            share_id: None,
            name: "n".into(),
            kind: NodeKind::File,
            size: 1,
            modified: 1,
        };
        state.mark_uploaded_but_dirty(&p("/n"), &node).unwrap();
        let entry = state.get_entry(&p("/n")).unwrap().unwrap();
        assert!(entry.dirty);
        assert_eq!(entry.remote_drivewsid.as_deref(), Some("FILE::9"));
        assert!(state.has_pending_content_change(&p("/n")).unwrap());
    }

    #[test]
    fn deleting_something_only_ever_created_locally_cancels_its_ops() {
        let state = state();
        state.queue_op("create", &p("/tmp.txt"), None).unwrap();
        state.queue_op("update", &p("/tmp.txt"), None).unwrap();
        state.queue_op("delete", &p("/tmp.txt"), None).unwrap();
        assert_eq!(state.pending_op_count().unwrap(), 0);

        state.queue_op("delete", &p("/synced.txt"), None).unwrap();
        assert_eq!(state.pending_op_count().unwrap(), 1, "a synced file's delete must be remembered");
    }

    #[test]
    fn repeated_writes_queue_a_single_update() {
        let state = state();
        for _ in 0..1000 {
            state.queue_op("update", &p("/big.bin"), None).unwrap();
        }
        assert_eq!(state.pending_op_count().unwrap(), 1);
        state.queue_op("create", &p("/new"), None).unwrap();
        state.queue_op("update", &p("/new"), None).unwrap();
        assert_eq!(state.pending_op_count().unwrap(), 2, "a create already covers later writes");
    }

    #[test]
    fn a_recreated_path_does_not_swallow_the_pending_delete_of_its_predecessor() {
        let state = state();
        state.upsert_entry(&remote("/a.txt", "OLD", NodeKind::File, 1)).unwrap();
        state.mark_tombstone(&p("/a.txt")).unwrap();

        state.bury_tombstone(&p("/a.txt")).unwrap();
        state.upsert_entry(&Entry::local(p("/a.txt"), NodeKind::File, 5)).unwrap();

        let live = state.get_entry(&p("/a.txt")).unwrap().unwrap();
        assert!(live.remote_drivewsid.is_none() && !live.tombstone);
        let pending: Vec<_> = state.list_dirty_entries().unwrap().into_iter().filter(|e| e.tombstone).collect();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].remote_drivewsid.as_deref(), Some("OLD"));

        // Burying something that is not a pending delete does nothing.
        state.bury_tombstone(&p("/a.txt")).unwrap();
        assert!(state.get_entry(&p("/a.txt")).unwrap().is_some());
    }

    #[test]
    fn content_changes_are_distinguished_from_renames() {
        let state = state();
        state.queue_op("rename", &p("/a"), Some(&p("/b"))).unwrap();
        assert!(!state.has_pending_content_change(&p("/a")).unwrap());
        state.queue_op("update", &p("/a"), None).unwrap();
        assert!(state.has_pending_content_change(&p("/a")).unwrap());
    }

    #[test]
    fn removing_a_subtree_spares_lookalike_siblings() {
        let state = state();
        for (path, id) in [
            ("/my_docs", "1"),
            ("/my_docs/f", "2"),
            ("/myXdocs", "3"),
            ("/myXdocs/f", "4"),
            ("/my_docs2", "5"),
            ("/100%", "6"),
            ("/100x", "7"),
        ] {
            state.upsert_entry(&remote(path, id, NodeKind::File, 1)).unwrap();
        }
        state.remove_subtree(&p("/my_docs")).unwrap();
        let left: Vec<_> = state.list_entries().unwrap().iter().map(|e| e.path.to_string()).collect();
        assert_eq!(left, ["/100%", "/100x", "/myXdocs", "/myXdocs/f", "/my_docs2"]);

        state.remove_subtree(&p("/100%")).unwrap();
        assert!(state.get_entry(&p("/100x")).unwrap().is_some(), "% must not act as a wildcard");
    }

    #[test]
    fn removing_a_subtree_also_forgets_listings_and_queued_ops_below_it() {
        let state = state();
        state.upsert_entry(&remote("/d", "D", NodeKind::Folder, 0)).unwrap();
        state.mark_folder_listed(&p("/d"), Some("D")).unwrap();
        state.mark_folder_listed(&p("/d/sub"), Some("S")).unwrap();
        state.mark_folder_listed(&p("/dx"), Some("X")).unwrap();
        state.queue_op("update", &p("/d/f"), None).unwrap();
        state.remove_subtree(&p("/d")).unwrap();
        assert!(state.folder_listed_at(&p("/d")).unwrap().is_none());
        assert!(state.folder_listed_at(&p("/d/sub")).unwrap().is_none());
        assert!(state.folder_listed_at(&p("/dx")).unwrap().is_some());
        assert_eq!(state.pending_op_count().unwrap(), 0);
    }

    #[test]
    fn rename_tree_moves_descendants_and_marks_only_the_root_dirty() {
        let state = state();
        state.upsert_entry(&remote("/old", "D", NodeKind::Folder, 0)).unwrap();
        state.upsert_entry(&remote("/old/a", "A", NodeKind::File, 1)).unwrap();
        state.upsert_entry(&remote("/old/sub/b", "B", NodeKind::File, 1)).unwrap();
        state.queue_op("update", &p("/old/a"), None).unwrap();

        state.rename_tree(&p("/old"), &p("/new"), true, false).unwrap();

        let paths: Vec<_> = state.list_entries().unwrap().iter().map(|e| e.path.to_string()).collect();
        assert_eq!(paths, ["/new", "/new/a", "/new/sub/b"]);
        let root = state.get_entry(&p("/new")).unwrap().unwrap();
        assert!(root.dirty);
        assert_eq!(root.synced_path, Some(p("/old")), "iCloud still knows the old name");
        assert!(!state.get_entry(&p("/new/a")).unwrap().unwrap().dirty);
        assert_eq!(state.get_entry(&p("/new/a")).unwrap().unwrap().parent_path, p("/new"));
        assert!(state.has_pending_content_change(&p("/new/a")).unwrap(), "queued ops follow the rename");
    }

    #[test]
    fn a_remote_rename_can_update_the_synced_path_too() {
        let state = state();
        state.upsert_entry(&remote("/old", "D", NodeKind::Folder, 0)).unwrap();
        state.upsert_entry(&remote("/old/a", "A", NodeKind::File, 1)).unwrap();
        state.rename_tree(&p("/old"), &p("/new"), false, true).unwrap();
        let child = state.get_entry(&p("/new/a")).unwrap().unwrap();
        assert_eq!(child.synced_path, Some(p("/new/a")));
        assert!(!state.get_entry(&p("/new")).unwrap().unwrap().dirty);
    }

    #[test]
    fn renaming_over_a_synced_file_keeps_a_tombstone_for_the_displaced_one() {
        let state = state();
        state.upsert_entry(&remote("/a", "A", NodeKind::File, 1)).unwrap();
        state.upsert_entry(&remote("/b", "B", NodeKind::File, 1)).unwrap();

        state.rename_tree(&p("/a"), &p("/b"), true, false).unwrap();

        let at_b = state.get_entry(&p("/b")).unwrap().unwrap();
        assert_eq!(at_b.remote_drivewsid.as_deref(), Some("A"), "the moved file now lives at /b");
        let tombstones: Vec<_> = state.list_entries().unwrap().into_iter().filter(|e| e.tombstone).collect();
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].remote_drivewsid.as_deref(), Some("B"), "B must still be deleted on iCloud");
        assert!(tombstones[0].dirty);
        assert!(tombstones[0].path.as_str().starts_with("/b\0"));
        assert!(state.list_dirty_entries().unwrap().iter().any(|e| e.tombstone));
    }

    #[test]
    fn renaming_over_a_local_only_file_just_replaces_it() {
        let state = state();
        state.upsert_entry(&remote("/a", "A", NodeKind::File, 1)).unwrap();
        state.upsert_entry(&Entry::local(p("/b"), NodeKind::File, 1)).unwrap();
        state.rename_tree(&p("/a"), &p("/b"), true, false).unwrap();
        assert_eq!(state.count_entries().unwrap(), 1);
        assert_eq!(state.get_entry(&p("/b")).unwrap().unwrap().remote_drivewsid.as_deref(), Some("A"));
    }

    #[test]
    fn detaching_as_a_conflict_keeps_the_data_but_drops_the_remote_identity() {
        let state = state();
        let mut entry = remote("/doc.txt", "F", NodeKind::File, 3);
        entry.dirty = true;
        state.upsert_entry(&entry).unwrap();
        state.detach_subtree_as_conflict(&p("/doc.txt"), &p("/doc.txt.local-conflict-1")).unwrap();
        assert!(state.get_entry(&p("/doc.txt")).unwrap().is_none());
        let kept = state.get_entry(&p("/doc.txt.local-conflict-1")).unwrap().unwrap();
        assert!(kept.dirty && kept.remote_drivewsid.is_none() && kept.synced_path.is_none());
    }

    #[test]
    fn folder_listing_markers_track_freshness() {
        let state = state();
        assert!(state.folder_listed_at(&IcPath::root()).unwrap().is_none());
        state.mark_folder_listed_at(&IcPath::root(), None, 1_000).unwrap();
        state.mark_folder_listed(&p("/fresh"), Some("F")).unwrap();
        let stale = state.list_stale_folder_listings(300).unwrap();
        assert_eq!(stale, [IcPath::root()]);
        assert_eq!(state.list_listed_folders().unwrap().len(), 2);
        state.forget_folder_listings(&IcPath::root()).unwrap();
        assert!(state.folder_listed_at(&p("/fresh")).unwrap().is_none(), "forgetting the root forgets everything");
    }

    #[test]
    fn mark_synced_subtree_cleans_only_the_root_of_the_subtree() {
        let state = state();
        let mut a = remote("/d", "D", NodeKind::Folder, 0);
        a.dirty = true;
        a.synced_path = Some(p("/old"));
        let mut b = remote("/d/x", "X", NodeKind::File, 1);
        b.dirty = true;
        b.synced_path = Some(p("/old/x"));
        state.upsert_entry(&a).unwrap();
        state.upsert_entry(&b).unwrap();
        state.mark_synced_subtree(&p("/d")).unwrap();
        let root = state.get_entry(&p("/d")).unwrap().unwrap();
        let child = state.get_entry(&p("/d/x")).unwrap().unwrap();
        assert!(!root.dirty);
        assert!(child.dirty, "the child's own changes are not made clean by its parent's move");
        assert_eq!(child.synced_path, Some(p("/d/x")));
    }

    #[test]
    fn the_database_survives_a_reopen_and_migrates_missing_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.sqlite3");
        {
            // A database from before `remote_shareid` existed.
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE entries (path TEXT PRIMARY KEY, type TEXT NOT NULL, parent_path TEXT NOT NULL,
                    remote_drivewsid TEXT, remote_docwsid TEXT, remote_etag TEXT, remote_zone TEXT,
                    size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
                    hydrated INTEGER NOT NULL DEFAULT 0, dirty INTEGER NOT NULL DEFAULT 0,
                    tombstone INTEGER NOT NULL DEFAULT 0, local_sha256 TEXT, last_synced_at INTEGER, synced_path TEXT);
                 INSERT INTO entries (path, type, parent_path, remote_drivewsid, size) VALUES ('/old.txt','file','/','F',9);",
            )
            .unwrap();
        }
        let state = SyncState::open(&db).unwrap();
        let entry = state.get_entry(&p("/old.txt")).unwrap().unwrap();
        assert_eq!(entry.size, 9);
        assert!(entry.remote_shareid.is_none());
        state.upsert_entry(&remote("/new.txt", "N", NodeKind::File, 1)).unwrap();
        drop(state);
        assert_eq!(SyncState::open(&db).unwrap().count_entries().unwrap(), 2);
    }

    #[test]
    fn entry_to_node_needs_a_remote_id() {
        assert!(Entry::local(p("/x"), NodeKind::File, 0).node().is_none());
        let node = remote("/d/x.txt", "F", NodeKind::File, 7).node().unwrap();
        assert_eq!((node.name.as_str(), node.size, node.drivewsid.as_str()), ("x.txt", 7, "F"));
    }

    #[test]
    fn entries_from_remote_start_clean_and_unhydrated_unless_empty() {
        let node = |size, kind| Node {
            drivewsid: "X".into(),
            docwsid: None,
            etag: None,
            zone: None,
            share_id: None,
            name: "x".into(),
            kind,
            size,
            modified: 5,
        };
        let file = Entry::from_remote(p("/x"), &node(10, NodeKind::File));
        assert!(!file.hydrated && !file.dirty);
        assert_eq!(file.synced_path, Some(p("/x")));
        assert!(Entry::from_remote(p("/x"), &node(0, NodeKind::File)).hydrated);
        assert!(Entry::from_remote(p("/x"), &node(0, NodeKind::Folder)).hydrated);
    }
}
