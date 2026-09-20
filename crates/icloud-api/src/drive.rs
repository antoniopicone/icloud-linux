//! iCloud Drive: node model, the [`Drive`] trait and its HTTP implementation.
//!
//! The trait is the seam the sync engine is written against, so the engine can
//! be tested without a network and other back ends could be slotted in.
//! [`HttpDrive`] is a port of `pyicloud.services.drive` restricted to what the
//! engine needs.

use std::{collections::BTreeMap, fs::File, io::Read, sync::Arc};

use reqwest::{
    blocking::multipart::{Form, Part},
    header::CONTENT_TYPE,
};
use serde_json::{Value, json};

use crate::{
    error::{Error, Result},
    session::{CT_JSON, CT_PLAIN, Session},
};

pub const CLOUD_DOCS_ZONE: &str = "com.apple.CloudDocs";
pub const ROOT_DRIVEWSID: &str = "FOLDER::com.apple.CloudDocs::root";
const VALIDATE_COOKIE: &str = "X-APPLE-WEBAUTH-VALIDATE";

/// Apple answers a folder listing in one piece, however many items it holds,
/// and for a folder of thousands of items that can take much longer than an
/// ordinary request. Giving up early would leave the folder unlisted.
const LISTING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// What a Drive item is. Anything that behaves like a directory is
/// [`is_directory`](Self::is_directory).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NodeKind {
    #[default]
    File,
    Folder,
    /// An application's container folder ("app library").
    AppLibrary,
    /// Trash and anything else Apple invents.
    Other,
}

impl NodeKind {
    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "file" => Self::File,
            "folder" => Self::Folder,
            "app_library" => Self::AppLibrary,
            _ => Self::Other,
        }
    }

    /// Lower-case name as stored in the state database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Folder => "folder",
            Self::AppLibrary => "app_library",
            Self::Other => "unknown",
        }
    }

    pub fn is_directory(self) -> bool {
        matches!(self, Self::Folder | Self::AppLibrary)
    }
}

/// One file or folder as the Drive web service describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub drivewsid: String,
    pub docwsid: Option<String>,
    pub etag: Option<String>,
    pub zone: Option<String>,
    pub share_id: Option<Value>,
    /// Display name, extension included.
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
    /// Modification time, Unix seconds; 0 when Apple did not say.
    pub modified: i64,
}

impl Node {
    /// The root folder, without asking the server. Enough to list it; use
    /// [`Drive::root`] when the server's own metadata is needed.
    pub fn root() -> Self {
        Self {
            drivewsid: ROOT_DRIVEWSID.to_owned(),
            docwsid: None,
            etag: None,
            zone: Some(CLOUD_DOCS_ZONE.to_owned()),
            share_id: None,
            name: "root".to_owned(),
            kind: NodeKind::Folder,
            size: 0,
            modified: 0,
        }
    }

    /// Parse one item of a `retrieveItemDetailsInFolders` reply.
    pub fn from_json(value: &Value) -> Result<Self> {
        let obj = value.as_object().ok_or_else(|| Error::Protocol("drive item is not an object".into()))?;
        let text = |key: &str| obj.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);

        let drivewsid = text("drivewsid").ok_or_else(|| Error::Protocol("drive item without drivewsid".into()))?;
        let kind = NodeKind::parse(obj.get("type").and_then(Value::as_str).unwrap_or("FILE"));

        let base = text("name")
            .unwrap_or_else(|| if drivewsid == ROOT_DRIVEWSID { "root".to_owned() } else { drivewsid.clone() });
        let name = match text("extension") {
            Some(ext) => format!("{base}.{ext}"),
            None => base,
        };

        let size = if kind.is_directory() { 0 } else { number(obj.get("size")) };
        let modified = text("dateModified").as_deref().and_then(parse_timestamp).unwrap_or(0);

        Ok(Self {
            drivewsid,
            docwsid: text("docwsid"),
            etag: text("etag"),
            zone: text("zone"),
            share_id: obj.get("shareID").filter(|v| !v.is_null()).cloned(),
            name,
            kind,
            size,
            modified,
        })
    }
}

/// Apple sends sizes as numbers or as decimal strings.
fn number(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Parse `2024-05-01T10:20:30Z`, optionally with fractional seconds or a
/// numeric offset, into Unix seconds.
pub fn parse_timestamp(text: &str) -> Option<i64> {
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};
    OffsetDateTime::parse(text, &Rfc3339).ok().map(OffsetDateTime::unix_timestamp)
}

/// How a remote delete is carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeleteMode {
    /// Move to "Recently Deleted", where it can still be recovered.
    #[default]
    Trash,
    /// Remove immediately and for good.
    Permanent,
}

/// The operations the sync engine needs from iCloud Drive.
pub trait Drive: Send + Sync {
    /// The root folder.
    fn root(&self) -> Result<Node>;
    /// A single node by id, with fresh metadata (etag in particular).
    fn node(&self, drivewsid: &str, share_id: Option<&Value>) -> Result<Node>;
    /// Direct children of a folder. Never recursive.
    fn children(&self, folder: &Node) -> Result<Vec<Node>>;
    /// Stream a file's contents.
    fn open(&self, file: &Node) -> Result<Box<dyn Read + Send>>;
    /// Create `name` under `parent` from the contents of `source`.
    fn upload(&self, parent: &Node, name: &str, source: File, mtime: i64) -> Result<()>;
    fn create_folder(&self, parent: &Node, name: &str) -> Result<()>;
    fn delete(&self, node: &Node, mode: DeleteMode) -> Result<()>;
    fn rename(&self, node: &Node, new_name: &str) -> Result<()>;
    fn move_to(&self, node: &Node, destination: &Node) -> Result<()>;
}

/// [`Drive`] over Apple's web API.
pub struct HttpDrive {
    session: Arc<Session>,
    service_root: String,
    document_root: String,
    params: BTreeMap<String, String>,
}

impl std::fmt::Debug for HttpDrive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpDrive").finish_non_exhaustive()
    }
}

impl HttpDrive {
    pub fn new(
        session: Arc<Session>,
        service_root: String,
        document_root: String,
        params: BTreeMap<String, String>,
    ) -> Self {
        Self {
            session,
            service_root: service_root.trim_end_matches('/').to_owned(),
            document_root: document_root.trim_end_matches('/').to_owned(),
            params,
        }
    }

    fn service_url(&self, endpoint: &str) -> String {
        format!("{}/{endpoint}", self.service_root)
    }

    fn document_url(&self, zone: &str, endpoint: &str) -> String {
        format!("{}/ws/{zone}/{endpoint}", self.document_root)
    }

    /// POST a JSON body with Apple's default `application/json` content type.
    fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        self.post_body(url, CT_JSON, body)
    }

    /// POST a JSON body under an explicit content type. Several Drive
    /// endpoints insist on `plain/text` for what is really JSON.
    fn post_body(&self, url: &str, content_type: &str, body: &Value) -> Result<Value> {
        self.post_body_within(url, content_type, body, None)
    }

    /// [`post_body`](Self::post_body) with a time limit other than the default.
    fn post_body_within(
        &self,
        url: &str,
        content_type: &str,
        body: &Value,
        limit: Option<std::time::Duration>,
    ) -> Result<Value> {
        let mut request = self
            .session
            .post(url)
            .query(&self.params)
            .header(CONTENT_TYPE, content_type)
            .body(serde_json::to_vec(body)?);
        if let Some(limit) = limit {
            request = request.timeout(limit);
        }
        let response = self.session.send(request)?;
        if response.is_empty() { Ok(Value::Null) } else { response.value() }
    }

    fn item_details(
        &self,
        drivewsid: &str,
        share_id: Option<&Value>,
        limit: Option<std::time::Duration>,
    ) -> Result<Value> {
        let mut item = json!({ "drivewsid": drivewsid, "partialData": false });
        if let Some(share) = share_id {
            item["shareID"] = share.clone();
        }
        let reply =
            self.post_body_within(&self.service_url("retrieveItemDetailsInFolders"), CT_JSON, &json!([item]), limit)?;
        reply
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .ok_or_else(|| Error::Protocol("empty reply to retrieveItemDetailsInFolders".into()))
    }

    /// The `t=` token from the validate cookie, required to start an upload.
    fn upload_token(&self) -> Result<String> {
        let cookie = self
            .session
            .cookie(VALIDATE_COOKIE)
            .ok_or_else(|| Error::AuthRequired("upload token cookie not found".into()))?;
        cookie
            .split(':')
            .find_map(|part| part.strip_prefix("t="))
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| Error::Protocol("cannot extract the upload token from its cookie".into()))
    }

    fn client_id(&self) -> &str {
        self.params.get("clientId").map_or("", String::as_str)
    }
}

impl Drive for HttpDrive {
    fn root(&self) -> Result<Node> {
        self.node(ROOT_DRIVEWSID, None)
    }

    fn node(&self, drivewsid: &str, share_id: Option<&Value>) -> Result<Node> {
        Node::from_json(&self.item_details(drivewsid, share_id, None)?)
    }

    fn children(&self, folder: &Node) -> Result<Vec<Node>> {
        let details = self.item_details(&folder.drivewsid, folder.share_id.as_ref(), Some(LISTING_TIMEOUT))?;
        let items = details.get("items").and_then(Value::as_array).ok_or_else(|| {
            let status = details.get("status").and_then(Value::as_str).unwrap_or("unknown");
            Error::Protocol(format!("no items in folder (status: {status})"))
        })?;
        items.iter().map(Node::from_json).collect()
    }

    fn open(&self, file: &Node) -> Result<Box<dyn Read + Send>> {
        // iCloud answers 400 for zero-byte files, and there is nothing to fetch.
        if file.size == 0 {
            return Ok(Box::new(std::io::empty()));
        }
        let docwsid = file.docwsid.as_deref().ok_or_else(|| Error::Protocol("file without docwsid".into()))?;
        let zone = file.zone.as_deref().unwrap_or(CLOUD_DOCS_ZONE);

        let mut params = self.params.clone();
        params.insert("document_id".into(), docwsid.to_owned());
        let lookup = self.session.get(&self.document_url(zone, "download/by_id")).query(&params);
        let reply = self.session.send(lookup)?.value()?;

        let url = ["data_token", "package_token"]
            .iter()
            .find_map(|key| reply.get(*key)?.get("url")?.as_str())
            .ok_or_else(|| Error::Protocol("download reply has neither data_token nor package_token".into()))?;

        let response = self.session.send_stream(self.session.get(url).query(&self.params), file.size)?;
        Ok(Box::new(response))
    }

    fn upload(&self, parent: &Node, name: &str, source: File, mtime: i64) -> Result<()> {
        let zone = parent.zone.as_deref().unwrap_or(CLOUD_DOCS_ZONE);
        let folder_id =
            parent.docwsid.as_deref().ok_or_else(|| Error::Protocol("parent folder without docwsid".into()))?;
        let size = source.metadata()?.len();

        // 1. Ask where to put the bytes.
        let mut params = self.params.clone();
        params.insert("token".into(), self.upload_token()?);
        let content_type = mime_guess::from_path(name).first_raw().unwrap_or("");
        let request = self
            .session
            .post(&self.document_url(zone, "upload/web"))
            .query(&params)
            .header(CONTENT_TYPE, CT_PLAIN)
            .body(serde_json::to_vec(&json!({
                "filename": name,
                "type": "FILE",
                "content_type": content_type,
                "size": size,
            }))?);
        let slot = self.session.send(request)?.value()?;
        let slot = slot.get(0).ok_or_else(|| Error::Protocol("empty upload slot".into()))?;
        let document_id = slot.get("document_id").and_then(Value::as_str);
        let content_url = slot.get("url").and_then(Value::as_str);
        let (Some(document_id), Some(content_url)) = (document_id, content_url) else {
            return Err(Error::Protocol("upload slot without document_id or url".into()));
        };

        // 2. Send the bytes. The multipart field is named after the file.
        let part = Part::reader_with_length(source, size).file_name(name.to_owned());
        let form = Form::new().part(name.to_owned(), part);
        let upload = self.session.post(content_url).timeout(Session::transfer_timeout(size)).multipart(form);
        let stored = self.session.send(upload)?.value()?;
        let info = stored.get("singleFile").ok_or_else(|| Error::Protocol("upload reply without singleFile".into()))?;
        let field = |key: &str| info.get(key).cloned().unwrap_or(Value::Null);

        // 3. Attach the stored bytes to a document in the folder.
        let mut data = json!({
            "signature": field("fileChecksum"),
            "wrapping_key": field("wrappingKey"),
            "reference_signature": field("referenceChecksum"),
            "size": field("size"),
        });
        // A receipt is absent for zero-byte files.
        if let Some(receipt) = info.get("receipt").filter(|r| !r.is_null()) {
            data["receipt"] = receipt.clone();
        }
        let millis = mtime.max(0).saturating_mul(1000);
        let update = json!({
            "data": data,
            "command": "add_file",
            "create_short_guid": true,
            "document_id": document_id,
            "path": { "starting_document_id": folder_id, "path": name },
            "allow_conflict": true,
            "file_flags": { "is_writable": true, "is_executable": false, "is_hidden": false },
            "mtime": millis,
            "btime": millis,
        });
        self.post_body(&self.document_url(zone, "update/documents"), CT_PLAIN, &update)?;
        Ok(())
    }

    fn create_folder(&self, parent: &Node, name: &str) -> Result<()> {
        let body = json!({
            "destinationDrivewsId": parent.drivewsid,
            "folders": [{
                "clientId": format!("FOLDER::UNKNOWN_ZONE::TempId-{}", crate::uuid_v4()?),
                "name": name,
            }],
        });
        self.post_body(&self.service_url("createFolders"), CT_PLAIN, &body)?;
        Ok(())
    }

    fn delete(&self, node: &Node, mode: DeleteMode) -> Result<()> {
        let etag = node.etag.as_deref().unwrap_or_default();
        match mode {
            DeleteMode::Permanent => {
                let body = json!({
                    "items": [{ "drivewsid": node.drivewsid, "etag": etag, "clientId": self.client_id() }],
                });
                self.post_json(&self.service_url("deleteItems"), &body)?;
            }
            DeleteMode::Trash => {
                let body = json!({
                    "items": [{ "drivewsid": node.drivewsid, "etag": etag, "clientId": node.drivewsid }],
                });
                self.post_json(&self.service_url("moveItemsToTrash"), &body)?;
            }
        }
        Ok(())
    }

    fn rename(&self, node: &Node, new_name: &str) -> Result<()> {
        // Sent exactly as pyicloud sends it: the full name, extension included.
        let body = json!({
            "items": [{
                "drivewsid": node.drivewsid,
                "etag": node.etag.as_deref().unwrap_or_default(),
                "name": new_name,
            }],
        });
        self.post_json(&self.service_url("renameItems"), &body)?;
        Ok(())
    }

    fn move_to(&self, node: &Node, destination: &Node) -> Result<()> {
        let body = json!({
            "destinationDrivewsId": destination.drivewsid,
            "items": [{
                "drivewsid": node.drivewsid,
                "etag": node.etag.as_deref().unwrap_or_default(),
                "clientId": node.drivewsid,
            }],
        });
        self.post_json(&self.service_url("moveItems"), &body)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_parsing_appends_the_extension_and_reads_string_sizes() {
        let node = Node::from_json(&json!({
            "drivewsid": "FILE::com.apple.CloudDocs::ABC",
            "docwsid": "ABC",
            "etag": "e1",
            "zone": "com.apple.CloudDocs",
            "name": "report",
            "extension": "pdf",
            "type": "FILE",
            "size": "1234",
            "dateModified": "2024-05-01T10:20:30Z",
        }))
        .unwrap();
        assert_eq!(node.name, "report.pdf");
        assert_eq!(node.size, 1234);
        assert_eq!(node.modified, 1_714_558_830);
        assert_eq!(node.kind, NodeKind::File);
    }

    #[test]
    fn folders_have_no_size_and_the_root_is_named() {
        let node = Node::from_json(&json!({
            "drivewsid": ROOT_DRIVEWSID, "type": "FOLDER", "size": 99,
        }))
        .unwrap();
        assert_eq!(node.name, "root");
        assert_eq!(node.size, 0);
        assert!(node.kind.is_directory());
    }

    #[test]
    fn unparsable_dates_become_zero_not_now() {
        let node = Node::from_json(&json!({"drivewsid": "X", "type": "FILE", "dateModified": "yesterday"})).unwrap();
        assert_eq!(node.modified, 0);
    }

    #[test]
    fn timestamps_with_offsets_and_fractions_parse() {
        assert_eq!(parse_timestamp("2024-05-01T10:20:30Z"), Some(1_714_558_830));
        assert_eq!(parse_timestamp("2024-05-01T12:20:30+02:00"), Some(1_714_558_830));
        assert_eq!(parse_timestamp("2024-05-01T10:20:30.750Z"), Some(1_714_558_830));
        assert_eq!(parse_timestamp("nope"), None);
    }

    #[test]
    fn items_without_an_id_are_rejected() {
        assert!(Node::from_json(&json!({"name": "x"})).is_err());
        assert!(Node::from_json(&json!("not an object")).is_err());
    }

    #[test]
    fn node_kind_round_trips_through_its_database_name() {
        for kind in [NodeKind::File, NodeKind::Folder, NodeKind::AppLibrary] {
            assert_eq!(NodeKind::parse(kind.as_str()), kind);
        }
        assert_eq!(NodeKind::parse("TRASH"), NodeKind::Other);
        assert!(!NodeKind::Other.is_directory());
    }
}
