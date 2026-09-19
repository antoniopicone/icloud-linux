//! An in-memory [`Drive`], for tests of code that sits on top of this crate.
//!
//! It behaves like the real service where the sync engine can tell the
//! difference: every change to an item bumps its etag, uploading over an
//! existing name creates a numbered duplicate, and failures can be injected.
//! Every call is recorded so tests can assert what was (not) requested.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Cursor, Read},
    sync::{Mutex, MutexGuard, PoisonError},
};

use serde_json::Value;

use crate::{
    drive::{CLOUD_DOCS_ZONE, DeleteMode, Drive, Node, NodeKind, ROOT_DRIVEWSID},
    error::{Error, Result},
};

#[derive(Debug, Clone)]
struct Item {
    parent: Option<String>,
    name: String,
    kind: NodeKind,
    data: Vec<u8>,
    etag: u64,
    modified: i64,
}

/// What to make every call do instead of working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outage {
    /// A transport-style failure that a retry may cure.
    Offline,
    /// The session has expired; only signing in again helps.
    SessionExpired,
}

struct Inner {
    items: BTreeMap<String, Item>,
    trash: Vec<String>,
    next_id: u64,
    calls: Vec<String>,
    outage: Option<Outage>,
    /// Paths whose listing fails, whatever else works.
    failing_listings: Vec<String>,
    /// Runs while an upload is in flight, after its bytes were read.
    upload_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner").field("items", &self.items.len()).finish_non_exhaustive()
    }
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct MemoryDrive {
    inner: Mutex<Inner>,
}

impl Default for MemoryDrive {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryDrive {
    pub fn new() -> Self {
        let root =
            Item { parent: None, name: "root".into(), kind: NodeKind::Folder, data: vec![], etag: 1, modified: 0 };
        let items = BTreeMap::from([(ROOT_DRIVEWSID.to_owned(), root)]);
        Self {
            inner: Mutex::new(Inner {
                items,
                trash: vec![],
                next_id: 1,
                calls: vec![],
                outage: None,
                failing_listings: vec![],
                upload_hook: None,
            }),
        }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ---- test helpers -------------------------------------------------

    /// Create a folder (and nothing else) at `parent_path`/`name`.
    pub fn add_folder(&self, parent_path: &str, name: &str) -> Node {
        self.insert(parent_path, name, NodeKind::Folder, vec![], 0)
    }

    pub fn add_file(&self, parent_path: &str, name: &str, contents: &[u8], modified: i64) -> Node {
        self.insert(parent_path, name, NodeKind::File, contents.to_vec(), modified)
    }

    fn insert(&self, parent_path: &str, name: &str, kind: NodeKind, data: Vec<u8>, modified: i64) -> Node {
        let mut inner = self.inner();
        let parent = inner.resolve(parent_path).unwrap_or_else(|| panic!("no such folder {parent_path}"));
        inner.create(&parent, name, kind, data, modified)
    }

    /// Replace a file's contents as if edited on another device.
    pub fn modify(&self, path: &str, contents: &[u8], modified: i64) {
        let mut inner = self.inner();
        let id = inner.resolve(path).unwrap_or_else(|| panic!("no such item {path}"));
        let item = inner.items.get_mut(&id).expect("resolved id exists");
        item.data = contents.to_vec();
        item.modified = modified;
        item.etag += 1;
    }

    /// Delete an item and everything below it, as if done on another device.
    pub fn remove(&self, path: &str) {
        let mut inner = self.inner();
        if let Some(id) = inner.resolve(path) {
            inner.remove_tree(&id);
        }
    }

    /// Move or rename an item as if done on another device.
    pub fn relocate(&self, from: &str, to_parent: &str, new_name: &str) {
        let mut inner = self.inner();
        let id = inner.resolve(from).unwrap_or_else(|| panic!("no such item {from}"));
        let parent = inner.resolve(to_parent).unwrap_or_else(|| panic!("no such folder {to_parent}"));
        let item = inner.items.get_mut(&id).expect("resolved id exists");
        item.parent = Some(parent);
        item.name = new_name.to_owned();
        item.etag += 1;
    }

    pub fn contents(&self, path: &str) -> Option<Vec<u8>> {
        let inner = self.inner();
        inner.resolve(path).and_then(|id| inner.items.get(&id)).map(|item| item.data.clone())
    }

    pub fn exists(&self, path: &str) -> bool {
        self.inner().resolve(path).is_some()
    }

    /// Names that were moved to the trash, in order.
    pub fn trashed(&self) -> Vec<String> {
        let inner = self.inner();
        inner.trash.iter().filter_map(|id| inner.items.get(id)).map(|i| i.name.clone()).collect()
    }

    /// Every call made so far, as `verb:detail`.
    pub fn calls(&self) -> Vec<String> {
        self.inner().calls.clone()
    }

    pub fn calls_matching(&self, prefix: &str) -> usize {
        self.inner().calls.iter().filter(|c| c.starts_with(prefix)).count()
    }

    pub fn set_outage(&self, outage: Option<Outage>) {
        self.inner().outage = outage;
    }

    /// Make listing the folder at `path` fail while everything else works.
    pub fn fail_listing_of(&self, path: &str) {
        self.inner().failing_listings.push(path.to_owned());
    }

    /// Run `hook` in the middle of every upload, as another process writing to
    /// the file would.
    pub fn on_upload(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.inner().upload_hook = Some(std::sync::Arc::new(hook));
    }

    /// Path of an item, for recording calls readably. Takes and releases the
    /// lock, so it must not be called while holding the guard from `enter`.
    fn path(&self, id: &str) -> String {
        self.inner().path_of(id)
    }

    fn enter(&self, call: String) -> Result<MutexGuard<'_, Inner>> {
        let mut inner = self.inner();
        inner.calls.push(call);
        match inner.outage {
            None => Ok(inner),
            Some(Outage::Offline) => Err(Error::Protocol("simulated outage".into())),
            Some(Outage::SessionExpired) => Err(Error::AuthRequired("simulated expiry".into())),
        }
    }
}

impl Inner {
    fn resolve(&self, path: &str) -> Option<String> {
        let mut current = ROOT_DRIVEWSID.to_owned();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            current = self
                .items
                .iter()
                .find(|(_, item)| item.parent.as_deref() == Some(&current) && item.name == part)
                .map(|(id, _)| id.clone())?;
        }
        Some(current)
    }

    fn path_of(&self, id: &str) -> String {
        let mut parts = Vec::new();
        let mut current = id;
        while let Some(item) = self.items.get(current) {
            let Some(parent) = &item.parent else { break };
            parts.push(item.name.as_str());
            current = parent;
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    fn node(&self, id: &str) -> Node {
        let item = &self.items[id];
        Node {
            drivewsid: id.to_owned(),
            docwsid: id.rsplit("::").next().map(str::to_owned),
            etag: Some(format!("e{}", item.etag)),
            zone: Some(CLOUD_DOCS_ZONE.to_owned()),
            share_id: None,
            name: item.name.clone(),
            kind: item.kind,
            size: item.data.len() as u64,
            modified: item.modified,
        }
    }

    fn create(&mut self, parent: &str, name: &str, kind: NodeKind, data: Vec<u8>, modified: i64) -> Node {
        // Like Apple with `allow_conflict`: never overwrite, number the copy.
        let mut unique = name.to_owned();
        let mut n = 1;
        while self.items.values().any(|i| i.parent.as_deref() == Some(parent) && i.name == unique) {
            n += 1;
            let (stem, ext) = split_extension(name);
            unique = match ext {
                Some(ext) => format!("{stem} {n}.{ext}"),
                None => format!("{stem} {n}"),
            };
        }
        self.next_id += 1;
        let label = if kind == NodeKind::File { "FILE" } else { "FOLDER" };
        let id = format!("{label}::{CLOUD_DOCS_ZONE}::{}", self.next_id);
        self.items
            .insert(id.clone(), Item { parent: Some(parent.to_owned()), name: unique, kind, data, etag: 1, modified });
        self.node(&id)
    }

    fn remove_tree(&mut self, id: &str) {
        let children: Vec<String> =
            self.items.iter().filter(|(_, i)| i.parent.as_deref() == Some(id)).map(|(k, _)| k.clone()).collect();
        for child in children {
            self.remove_tree(&child);
        }
        self.items.remove(id);
    }
}

fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    }
}

impl Drive for MemoryDrive {
    fn root(&self) -> Result<Node> {
        let inner = self.enter("root".into())?;
        Ok(inner.node(ROOT_DRIVEWSID))
    }

    fn node(&self, drivewsid: &str, _share_id: Option<&Value>) -> Result<Node> {
        let inner = self.enter(format!("node:{drivewsid}"))?;
        if inner.items.contains_key(drivewsid) {
            Ok(inner.node(drivewsid))
        } else {
            Err(Error::Protocol(format!("no such node {drivewsid}")))
        }
    }

    fn children(&self, folder: &Node) -> Result<Vec<Node>> {
        let inner = self.enter(format!("children:{}", self.path(&folder.drivewsid)))?;
        if inner.failing_listings.contains(&inner.path_of(&folder.drivewsid)) {
            return Err(Error::Protocol("simulated listing failure".into()));
        }
        if !inner.items.contains_key(&folder.drivewsid) {
            return Err(Error::Protocol("no items in folder (status: NOT_FOUND)".into()));
        }
        Ok(inner
            .items
            .iter()
            .filter(|(_, item)| item.parent.as_deref() == Some(folder.drivewsid.as_str()))
            .map(|(id, _)| inner.node(id))
            .collect())
    }

    fn open(&self, file: &Node) -> Result<Box<dyn Read + Send>> {
        let inner = self.enter(format!("open:{}", self.path(&file.drivewsid)))?;
        let item = inner.items.get(&file.drivewsid).ok_or_else(|| Error::Protocol("no such file".into()))?;
        Ok(Box::new(Cursor::new(item.data.clone())))
    }

    fn upload(&self, parent: &Node, name: &str, mut source: File, mtime: i64) -> Result<()> {
        let mut data = Vec::new();
        source.read_to_end(&mut data)?;
        let hook = self.inner().upload_hook.clone();
        if let Some(hook) = hook {
            hook();
        }
        let mut inner = self.enter(format!("upload:{}/{name}", self.path(&parent.drivewsid)))?;
        if !inner.items.contains_key(&parent.drivewsid) {
            return Err(Error::Protocol("no such parent".into()));
        }
        inner.create(&parent.drivewsid, name, NodeKind::File, data, mtime);
        Ok(())
    }

    fn create_folder(&self, parent: &Node, name: &str) -> Result<()> {
        let mut inner = self.enter(format!("mkdir:{}/{name}", self.path(&parent.drivewsid)))?;
        if !inner.items.contains_key(&parent.drivewsid) {
            return Err(Error::Protocol("no such parent".into()));
        }
        inner.create(&parent.drivewsid, name, NodeKind::Folder, vec![], 0);
        Ok(())
    }

    fn delete(&self, node: &Node, mode: DeleteMode) -> Result<()> {
        let mut inner = self.enter(format!("delete:{}:{mode:?}", self.path(&node.drivewsid)))?;
        if !inner.items.contains_key(&node.drivewsid) {
            return Err(Error::Protocol("no such item".into()));
        }
        match mode {
            DeleteMode::Permanent => inner.remove_tree(&node.drivewsid),
            DeleteMode::Trash => {
                // Keep the item, detached from the tree, so tests can inspect it.
                let item = inner.items.get_mut(&node.drivewsid).expect("checked above");
                item.parent = Some("TRASH".into());
                inner.trash.push(node.drivewsid.clone());
            }
        }
        Ok(())
    }

    fn rename(&self, node: &Node, new_name: &str) -> Result<()> {
        let mut inner = self.enter(format!("rename:{}:{new_name}", self.path(&node.drivewsid)))?;
        let item = inner.items.get_mut(&node.drivewsid).ok_or_else(|| Error::Protocol("no such item".into()))?;
        item.name = new_name.to_owned();
        item.etag += 1;
        Ok(())
    }

    fn move_to(&self, node: &Node, destination: &Node) -> Result<()> {
        let mut inner =
            self.enter(format!("move:{}:{}", self.path(&node.drivewsid), self.path(&destination.drivewsid)))?;
        let item = inner.items.get_mut(&node.drivewsid).ok_or_else(|| Error::Protocol("no such item".into()))?;
        item.parent = Some(destination.drivewsid.clone());
        item.etag += 1;
        Ok(())
    }
}
