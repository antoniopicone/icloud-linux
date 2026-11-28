//! iCloud Drive: node model, the [`Drive`] trait and its HTTP implementation.
//!
//! The trait is the seam the sync engine is written against, so the engine can
//! be tested without a network and other back ends could be slotted in.
//! [`HttpDrive`] is a port of `pyicloud.services.drive` restricted to what the
//! engine needs.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    sync::{Arc, PoisonError, RwLock, RwLockReadGuard},
    thread,
    time::Duration,
};

use reqwest::{
    StatusCode,
    blocking::multipart::{Form, Part},
    header::{CONTENT_RANGE, CONTENT_TYPE, RANGE},
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
const LISTING_TIMEOUT: Duration = Duration::from_secs(300);
/// Retries for transient errors when looking up a file's download URL.
const MAX_LOOKUP_RETRIES: usize = 2;
/// Retries (with HTTP `Range` resumption) when a download stream drops mid-way.
const MAX_STREAM_RETRIES: usize = 3;

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

/// A shared drive is a drive, so wrappers can hold an `Arc` to one.
impl<D: Drive + ?Sized> Drive for std::sync::Arc<D> {
    fn root(&self) -> Result<Node> {
        (**self).root()
    }
    fn node(&self, drivewsid: &str, share_id: Option<&Value>) -> Result<Node> {
        (**self).node(drivewsid, share_id)
    }
    fn children(&self, folder: &Node) -> Result<Vec<Node>> {
        (**self).children(folder)
    }
    fn open(&self, file: &Node) -> Result<Box<dyn Read + Send>> {
        (**self).open(file)
    }
    fn upload(&self, parent: &Node, name: &str, source: File, mtime: i64) -> Result<()> {
        (**self).upload(parent, name, source, mtime)
    }
    fn create_folder(&self, parent: &Node, name: &str) -> Result<()> {
        (**self).create_folder(parent, name)
    }
    fn delete(&self, node: &Node, mode: DeleteMode) -> Result<()> {
        (**self).delete(node, mode)
    }
    fn rename(&self, node: &Node, new_name: &str) -> Result<()> {
        (**self).rename(node, new_name)
    }
    fn move_to(&self, node: &Node, destination: &Node) -> Result<()> {
        (**self).move_to(node, destination)
    }
}

/// Shared, dynamically-updatable endpoints and query parameters for Drive.
#[derive(Debug, Clone, Default)]
pub(crate) struct DriveEndpoints {
    pub(crate) service_root: String,
    pub(crate) document_root: String,
    pub(crate) params: BTreeMap<String, String>,
}

impl DriveEndpoints {
    pub(crate) fn new(service_root: String, document_root: String, params: BTreeMap<String, String>) -> Self {
        Self {
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
}

/// [`Drive`] over Apple's web API.
pub struct HttpDrive {
    session: Arc<Session>,
    endpoints: Arc<RwLock<DriveEndpoints>>,
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
        Self::with_shared_endpoints(
            session,
            Arc::new(RwLock::new(DriveEndpoints::new(service_root, document_root, params))),
        )
    }

    pub(crate) fn with_shared_endpoints(session: Arc<Session>, endpoints: Arc<RwLock<DriveEndpoints>>) -> Self {
        Self { session, endpoints }
    }

    fn endpoints(&self) -> RwLockReadGuard<'_, DriveEndpoints> {
        self.endpoints.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn service_url(&self, endpoint: &str) -> String {
        self.endpoints().service_url(endpoint)
    }

    fn document_url(&self, zone: &str, endpoint: &str) -> String {
        self.endpoints().document_url(zone, endpoint)
    }

    fn params(&self) -> BTreeMap<String, String> {
        self.endpoints().params.clone()
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
    fn post_body_within(&self, url: &str, content_type: &str, body: &Value, limit: Option<Duration>) -> Result<Value> {
        let params = self.params();
        let mut request =
            self.session.post(url).query(&params).header(CONTENT_TYPE, content_type).body(serde_json::to_vec(body)?);
        if let Some(limit) = limit {
            request = request.timeout(limit);
        }
        let response = self.session.send(request)?;
        if response.is_empty() { Ok(Value::Null) } else { response.value() }
    }

    fn item_details(&self, drivewsid: &str, share_id: Option<&Value>, limit: Option<Duration>) -> Result<Value> {
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

    fn client_id(&self) -> String {
        self.endpoints().params.get("clientId").cloned().unwrap_or_default()
    }
}

/// Ask `docws` for the signed content URL of `docwsid`, retrying briefly on
/// transient errors.
fn resolve_download_url(
    session: &Session,
    endpoints: &RwLock<DriveEndpoints>,
    zone: &str,
    docwsid: &str,
) -> Result<String> {
    let mut attempt = 0;
    loop {
        let (lookup_url, mut params) = {
            let ep = endpoints.read().unwrap_or_else(PoisonError::into_inner);
            (ep.document_url(zone, "download/by_id"), ep.params.clone())
        };
        params.insert("document_id".into(), docwsid.to_owned());
        let lookup = session.get(&lookup_url).query(&params);
        match session.send(lookup).and_then(|resp| resp.value()) {
            Ok(reply) => {
                return ["data_token", "package_token"]
                    .iter()
                    .find_map(|key| reply.get(*key)?.get("url")?.as_str())
                    .map(str::to_owned)
                    .ok_or_else(|| Error::Protocol("download reply has neither data_token nor package_token".into()));
            }
            Err(err) if err.is_transient() && attempt < MAX_LOOKUP_RETRIES => {
                attempt += 1;
                let wait = Duration::from_millis(400 * (1u64 << (attempt - 1)));
                tracing::debug!("download lookup failed ({err}); retrying in {}ms", wait.as_millis());
                thread::sleep(wait);
            }
            Err(err) => return Err(err),
        }
    }
}

fn open_content_stream(
    session: &Session,
    endpoints: &RwLock<DriveEndpoints>,
    url: &str,
    offset: u64,
    expected_len: u64,
) -> Result<reqwest::blocking::Response> {
    let params = endpoints.read().unwrap_or_else(PoisonError::into_inner).params.clone();
    let mut request = session.stream_get(url).query(&params);
    if offset > 0 {
        request = request.header(RANGE, format!("bytes={offset}-"));
    }
    session.send_stream(request, expected_len)
}

/// Derive a `docwsid` from a canonical `FILE::<zone>::<id>` `drivewsid` when
/// Apple omitted the separate `docwsid` field.
fn docwsid_from_drivewsid(drivewsid: &str) -> Option<&str> {
    let mut parts = drivewsid.split("::");
    let kind = parts.next()?;
    let _zone = parts.next()?;
    let id = parts.next()?;
    (kind == "FILE" && parts.next().is_none() && !id.is_empty()).then_some(id)
}

/// True when a `Content-Range` header (`bytes START-END/TOTAL`) begins at `offset`.
fn content_range_starts_at(headers: &reqwest::header::HeaderMap, offset: u64) -> bool {
    let Some(raw) = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(rest) = raw.trim().strip_prefix("bytes ") else { return false };
    let Some((start, _)) = rest.split_once('-') else { return false };
    start.trim().parse::<u64>().ok() == Some(offset)
}

/// Streams a downloaded file while verifying `Content-Length` and transparently
/// resuming via HTTP `Range` (and refreshing expired signed URLs) if the
/// connection stalls or drops mid-transfer.
struct ResumingDownloadReader {
    session: Arc<Session>,
    endpoints: Arc<RwLock<DriveEndpoints>>,
    zone: String,
    docwsid: String,
    url: String,
    reader: reqwest::blocking::Response,
    bytes_read: u64,
    expected_len: Option<u64>,
    announced_len: u64,
    retries: usize,
}

impl ResumingDownloadReader {
    fn new(
        session: Arc<Session>,
        endpoints: Arc<RwLock<DriveEndpoints>>,
        zone: String,
        docwsid: String,
        url: String,
        reader: reqwest::blocking::Response,
        announced_len: u64,
    ) -> Self {
        let expected_len = reader.content_length();
        Self { session, endpoints, zone, docwsid, url, reader, bytes_read: 0, expected_len, announced_len, retries: 0 }
    }

    fn resume_after(&mut self, cause: &str) -> io::Result<()> {
        if self.retries >= MAX_STREAM_RETRIES {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("download failed after {} retries at byte {}: {cause}", self.retries, self.bytes_read),
            ));
        }
        let backoff = Duration::from_millis(400 * (1u64 << self.retries));
        self.retries += 1;
        tracing::warn!(
            "download stream interrupted at byte {} ({cause}); resuming (attempt {}/{MAX_STREAM_RETRIES}) in {}ms",
            self.bytes_read,
            self.retries,
            backoff.as_millis()
        );
        thread::sleep(backoff);

        let response =
            match open_content_stream(&self.session, &self.endpoints, &self.url, self.bytes_read, self.announced_len) {
                Ok(resp) => resp,
                Err(err) if matches!(err, Error::Status { status: 401 | 403 | 410, .. }) || err.is_transient() => {
                    // Signed CDN URL may have expired or that edge node dropped;
                    // ask `docws` for a fresh URL and resume from `bytes_read`.
                    let fresh = resolve_download_url(&self.session, &self.endpoints, &self.zone, &self.docwsid)
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    self.url = fresh;
                    open_content_stream(&self.session, &self.endpoints, &self.url, self.bytes_read, self.announced_len)
                        .map_err(|e| io::Error::other(e.to_string()))?
                }
                Err(err) => return Err(io::Error::other(err.to_string())),
            };

        if self.bytes_read == 0 {
            if self.expected_len.is_none() {
                self.expected_len = response.content_length();
            }
            self.reader = response;
            return Ok(());
        }
        if response.status() == StatusCode::PARTIAL_CONTENT
            && content_range_starts_at(response.headers(), self.bytes_read)
        {
            self.reader = response;
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            format!("server did not honour Range resume at byte {} (HTTP {})", self.bytes_read, response.status()),
        ))
    }
}

impl Read for ResumingDownloadReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.reader.read(buf) {
                Ok(0) => {
                    if let Some(expected) = self.expected_len
                        && self.bytes_read < expected
                    {
                        self.resume_after(&format!("premature EOF after {} of {expected} bytes", self.bytes_read))?;
                        continue;
                    }
                    return Ok(0);
                }
                Ok(n) => {
                    self.bytes_read += n as u64;
                    self.retries = 0;
                    return Ok(n);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    if self.resume_after(&err.to_string()).is_err() {
                        return Err(err);
                    }
                }
            }
        }
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
        let docwsid = file
            .docwsid
            .as_deref()
            .or_else(|| docwsid_from_drivewsid(&file.drivewsid))
            .ok_or_else(|| Error::Protocol("file without docwsid".into()))?;
        let zone = file.zone.as_deref().unwrap_or(CLOUD_DOCS_ZONE);

        let url = resolve_download_url(&self.session, &self.endpoints, zone, docwsid)?;
        let response = open_content_stream(&self.session, &self.endpoints, &url, 0, file.size)?;
        Ok(Box::new(ResumingDownloadReader::new(
            Arc::clone(&self.session),
            Arc::clone(&self.endpoints),
            zone.to_owned(),
            docwsid.to_owned(),
            url,
            response,
            file.size,
        )))
    }

    fn upload(&self, parent: &Node, name: &str, source: File, mtime: i64) -> Result<()> {
        let zone = parent.zone.as_deref().unwrap_or(CLOUD_DOCS_ZONE);
        let folder_id =
            parent.docwsid.as_deref().ok_or_else(|| Error::Protocol("parent folder without docwsid".into()))?;
        let size = source.metadata()?.len();

        // 1. Ask where to put the bytes.
        let mut params = self.params();
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
