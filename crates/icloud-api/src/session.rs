//! HTTP session: cookie jar, persistence and the normalisation of Apple's
//! many error shapes into [`Error`].
//!
//! One [`Session`] is shared by the client and by every Drive request. It is `Send + Sync`; concurrent requests are fine, the parts that
//! mutate (cookies, session data, files on disk) are behind locks.

use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use reqwest::{
    StatusCode,
    blocking::{Client, RequestBuilder, Response},
    header::{HeaderMap, HeaderValue},
};
use reqwest_cookie_store::CookieStoreMutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.3.1 Safari/605.1.15";

/// Total time allowed for one metadata request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Drop idle sockets promptly so a network switch or laptop sleep does not
/// leave dead keep-alive connections sitting in the pool.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const TCP_KEEPALIVE_RETRIES: u32 = 3;
/// Kernel-level TCP timeout for unacknowledged / stalled transfers.
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(45);
/// Upper bound for a buffered response body. Real folder listings are a few
/// megabytes; this only protects against a misbehaving server.
const MAX_BODY_BYTES: u64 = 256 * 1024 * 1024;
/// Uploads get a total timeout that grows with the expected size.
const STREAM_BASE_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_MIN_BYTES_PER_SEC: u64 = 32 * 1024;

pub(crate) const CT_JSON: &str = "application/json";
pub(crate) const CT_PLAIN: &str = "plain/text";

/// What is kept next to the cookies. The id iCloud's web services know this
/// client by stays the same from one run to the next, as a browser's would,
/// and the last known service URLs let the daemon mount offline and recover
/// once the network returns.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drivews_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docws_url: Option<String>,
}

/// A cookie taken from a browser the person signed in with.
#[derive(Clone, PartialEq)]
pub struct BrowserCookie {
    pub name: String,
    pub value: String,
    /// As the browser reports it: a leading dot means "and subdomains".
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Seconds since the Unix epoch; `None` for a session cookie.
    pub expires: Option<f64>,
}

impl std::fmt::Debug for BrowserCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserCookie").field("name", &self.name).field("domain", &self.domain).finish_non_exhaustive()
    }
}

impl BrowserCookie {
    /// The `Set-Cookie` line that recreates this cookie, and the address it
    /// came from.
    fn set_cookie(&self, now: f64) -> Option<(String, reqwest::Url)> {
        let host = self.domain.trim_start_matches('.');
        let url = reqwest::Url::parse(&format!("https://{host}/")).ok()?;
        let mut line =
            format!("{}={}; Path={}", self.name, self.value, if self.path.is_empty() { "/" } else { &self.path });
        if self.domain.starts_with('.') {
            line.push_str("; Domain=");
            line.push_str(host);
        }
        if let Some(expires) = self.expires {
            let left = expires - now;
            if left <= 0.0 {
                return None; // already expired
            }
            line.push_str("; Max-Age=");
            line.push_str(&(left.ceil() as u64).max(1).to_string());
        }
        if self.secure {
            line.push_str("; Secure");
        }
        if self.http_only {
            line.push_str("; HttpOnly");
        }
        Some((line, url))
    }
}

/// A fully buffered response whose HTTP and API-level status were checked.
#[derive(Debug)]
pub struct ApiResponse {
    pub status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl ApiResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }

    pub fn value(&self) -> Result<Value> {
        self.json()
    }

    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    fn is_json(&self) -> bool {
        self.header("content-type")
            .and_then(|ct| ct.split(';').next())
            .is_some_and(|mime| matches!(mime.trim(), CT_JSON | "text/json"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: SystemTime,
    len: u64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    Some(FileStamp { modified: meta.modified().ok()?, len: meta.len() })
}

struct Persistence {
    session_file: PathBuf,
    cookie_file: PathBuf,
    /// Last serialised form written, to skip redundant disk writes.
    last_session: Mutex<String>,
    /// Stamp of `cookie_file` as last loaded or flushed by this process.
    last_cookie_stamp: Mutex<Option<FileStamp>>,
}

pub struct Session {
    client: Client,
    /// Dedicated streaming client with an inactivity read timeout and no total
    /// transfer cap, sharing the same cookie jar and default headers.
    stream_client: Client,
    jar: Arc<CookieStoreMutex>,
    data: Mutex<SessionData>,
    persistence: Option<Persistence>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding one of these locks cannot leave the data in a
    // state that is unsafe to read, so poisoning is ignored.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Session {
    /// Open a session, loading previously saved state from `dir` if present.
    ///
    /// With `dir == None` nothing is read from or written to disk.
    pub fn open(account: &str, dir: Option<&Path>, home_origin: &str, new_client_id: &str) -> Result<Arc<Self>> {
        let persistence = dir.map(|dir| {
            let (session_file, cookie_file) = session_files(account, dir);
            let stamp = file_stamp(&cookie_file);
            Persistence {
                session_file,
                cookie_file,
                last_session: Mutex::new(String::new()),
                last_cookie_stamp: Mutex::new(stamp),
            }
        });

        let mut data = SessionData::default();
        let mut store = cookie_store::CookieStore::default();
        if let Some(p) = &persistence {
            data = load_session_data(&p.session_file);
            store = load_cookies(&p.cookie_file);
            *lock(&p.last_session) = serde_json::to_string(&data).unwrap_or_default();
        }
        if data.client_id.is_none() {
            data.client_id = Some(new_client_id.to_owned());
        }

        let jar = Arc::new(CookieStoreMutex::new(store));
        let mut headers = HeaderMap::new();
        headers.insert("Origin", header_value(home_origin)?);
        headers.insert("Referer", header_value(&format!("{home_origin}/"))?);

        let client = Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers.clone())
            .cookie_provider(Arc::clone(&jar))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .tcp_keepalive(TCP_KEEPALIVE)
            .tcp_keepalive_interval(TCP_KEEPALIVE_INTERVAL)
            .tcp_keepalive_retries(TCP_KEEPALIVE_RETRIES)
            .tcp_user_timeout(TCP_USER_TIMEOUT)
            .build()?;

        let stream_client = Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .cookie_provider(Arc::clone(&jar))
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .tcp_keepalive(TCP_KEEPALIVE)
            .tcp_keepalive_interval(TCP_KEEPALIVE_INTERVAL)
            .tcp_keepalive_retries(TCP_KEEPALIVE_RETRIES)
            .tcp_user_timeout(TCP_USER_TIMEOUT)
            .build()?;

        Ok(Arc::new(Self { client, stream_client, jar, data: Mutex::new(data), persistence }))
    }

    pub fn get(&self, url: &str) -> RequestBuilder {
        self.client.get(url)
    }

    /// GET request for a streamed download: uses a per-read inactivity timeout
    /// instead of a fixed total timeout so large downloads are not cut off
    /// while making progress, yet stalled sockets fail promptly.
    pub fn stream_get(&self, url: &str) -> RequestBuilder {
        self.stream_client.get(url)
    }

    pub fn post(&self, url: &str) -> RequestBuilder {
        self.client.post(url)
    }

    pub fn data(&self) -> SessionData {
        lock(&self.data).clone()
    }

    /// Update the stored session metadata and persist it if it changed.
    pub fn update_data(&self, update: impl FnOnce(&mut SessionData)) {
        update(&mut lock(&self.data));
        self.persist_data();
    }

    /// Change the session data and persist it if it changed.
    pub fn cookie(&self, name: &str) -> Option<String> {
        let store = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
        store.iter_unexpired().find(|c| c.name() == name && !c.value().is_empty()).map(|c| c.value().to_owned())
    }

    /// Add cookies taken from a browser. Returns how many were accepted.
    pub fn import_cookies(&self, cookies: &[BrowserCookie]) -> usize {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let mut store = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
        cookies.iter().filter_map(|c| c.set_cookie(now)).filter(|(line, url)| store.parse(line, url).is_ok()).count()
    }

    /// Send a request and normalise the outcome. The body is buffered.
    pub fn send(&self, request: RequestBuilder) -> Result<ApiResponse> {
        let response = self.send_unchecked(request)?;
        self.check(&response)?;
        Ok(response)
    }

    /// Like [`send`](Self::send) but leaves the status for the caller to
    /// interpret.
    pub fn send_unchecked(&self, request: RequestBuilder) -> Result<ApiResponse> {
        let response = request.send()?;
        let status = response.status().as_u16();
        trace_reply(&response);
        let headers = response.headers().clone();
        let mut body = Vec::new();
        response.take(MAX_BODY_BYTES).read_to_end(&mut body)?;
        Ok(ApiResponse { status, headers, body })
    }

    /// Send a request whose body is streamed to the caller. Failures are
    /// turned into [`Error`] using at most 64 KiB of the body.
    pub fn send_stream(&self, request: RequestBuilder, _expected_len: u64) -> Result<Response> {
        let response = request.send()?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let mut body = Vec::new();
        response.take(64 * 1024).read_to_end(&mut body)?;
        let api = ApiResponse { status, headers, body };
        Err(match self.check(&api) {
            Err(err) => err,
            Ok(()) => Error::Status { status, reason: reason_phrase(status) },
        })
    }

    /// Total time allowed to move `len` bytes: a fixed allowance plus the time
    /// the transfer would take at a very slow but still useful rate.
    pub fn transfer_timeout(len: u64) -> Duration {
        STREAM_BASE_TIMEOUT + Duration::from_secs(len / STREAM_MIN_BYTES_PER_SEC)
    }

    /// Turn Apple's status and body conventions into a `Result`.
    pub(crate) fn check(&self, response: &ApiResponse) -> Result<()> {
        if !(200..300).contains(&response.status) {
            let json_or_auth = response.is_json() || matches!(response.status, 401 | 409 | 421 | 450 | 500);
            return Err(if json_or_auth {
                self.classify_error(response)
            } else {
                Error::Status { status: response.status, reason: reason_phrase(response.status) }
            });
        }
        if response.is_json() && !response.is_empty() {
            // A 200 can still carry an error in the body.
            if let Ok(Value::Object(map)) = response.json::<Value>()
                && let Some(err) = api_error_from_body(&map)
            {
                return Err(err);
            }
        }
        Ok(())
    }

    fn classify_error(&self, response: &ApiResponse) -> Error {
        let body: Option<Value> = response.json().ok();
        if response.status == 409 && response.is_json() && is_hsa2_challenge(body.as_ref()) {
            return Error::AuthRequired("the session needs a verification code: sign in again on icloud.com".into());
        }
        if response.status == 450 {
            return Error::AuthRequired("re-authentication required".into());
        }
        if let Some(Value::Object(map)) = &body
            && let Some(err) = api_error_from_body(map)
        {
            return err;
        }
        if response.status == 401 {
            return Error::Status { status: 401, reason: reason_phrase(401) };
        }
        make_api_error(Some(response.status.to_string()), reason_phrase(response.status))
    }

    fn persist_data(&self) {
        let Some(p) = &self.persistence else { return };
        let snapshot = lock(&self.data).clone();
        let Ok(json) = serde_json::to_string(&snapshot) else { return };
        let mut last = lock(&p.last_session);
        if *last == json {
            return;
        }
        match write_private(&p.session_file, json.as_bytes()) {
            Ok(()) => *last = json,
            Err(err) => tracing::warn!("could not save iCloud session data: {err}"),
        }
    }

    /// Write session data and cookies to disk.
    pub fn flush(&self) -> Result<()> {
        let Some(p) = &self.persistence else { return Ok(()) };
        self.persist_data();
        write_private(&p.cookie_file, &self.cookie_bytes()?)?;
        *lock(&p.last_cookie_stamp) = file_stamp(&p.cookie_file);
        Ok(())
    }

    /// True when another process (such as `icloudctl auth`) has written a newer
    /// cookie file on disk since this session last loaded or saved it.
    pub fn has_newer_on_disk(&self) -> bool {
        let Some(p) = &self.persistence else { return false };
        let Some(current) = file_stamp(&p.cookie_file) else { return false };
        *lock(&p.last_cookie_stamp) != Some(current)
    }

    /// Reload cookies and session data from disk if another process wrote a
    /// newer cookie file containing valid credentials. Returns whether the
    /// in-memory session was replaced.
    pub fn reload_from_disk_if_newer(&self) -> bool {
        let Some(p) = &self.persistence else { return false };
        let Some(current) = file_stamp(&p.cookie_file) else { return false };
        if *lock(&p.last_cookie_stamp) == Some(current) {
            return false;
        }
        let Some(store) = fs::File::open(&p.cookie_file)
            .ok()
            .and_then(|file| cookie_store::serde::json::load_all(io::BufReader::new(file)).ok())
        else {
            return false;
        };
        let has_token = store.iter_unexpired().any(|c| c.name() == "X-APPLE-WEBAUTH-TOKEN" && !c.value().is_empty());
        if !has_token {
            return false;
        }
        let disk_data = load_session_data(&p.session_file);
        {
            let mut jar = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
            *jar = store;
        }
        {
            let mut data = lock(&self.data);
            let fallback_client_id = data.client_id.clone();
            *data = disk_data;
            if data.client_id.is_none() {
                data.client_id = fallback_client_id;
            }
            *lock(&p.last_session) = serde_json::to_string(&*data).unwrap_or_default();
        }
        *lock(&p.last_cookie_stamp) = Some(current);
        tracing::info!("reloaded updated iCloud session from disk");
        true
    }

    /// Write this session to `dir` as the saved session of `account`, where
    /// [`Session::open`] for that account will find it. For a sign-in whose
    /// Apple ID is only known once Apple has answered.
    pub fn save_as(&self, account: &str, dir: &Path) -> Result<()> {
        let (session_file, cookie_file) = session_files(account, dir);
        let json = serde_json::to_string(&*lock(&self.data))?;
        write_private(&session_file, json.as_bytes())?;
        write_private(&cookie_file, &self.cookie_bytes()?)?;
        Ok(())
    }

    fn cookie_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let store = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut buf)
            .map_err(|e| Error::Protocol(format!("could not serialise cookies: {e}")))?;
        Ok(buf)
    }

    /// Forget everything, in memory and on disk.
    pub fn clear(&self) {
        *lock(&self.data) = SessionData::default();
        self.jar.lock().unwrap_or_else(PoisonError::into_inner).clear();
        if let Some(p) = &self.persistence {
            for file in [&p.session_file, &p.cookie_file] {
                if let Err(err) = fs::remove_file(file)
                    && err.kind() != io::ErrorKind::NotFound
                {
                    tracing::warn!("could not remove {}: {err}", file.display());
                }
            }
            lock(&p.last_session).clear();
            *lock(&p.last_cookie_stamp) = None;
        }
    }

    /// True when the cookie Apple's web services recognise a session by is
    /// stored. A browser sign-in provides that cookie but no session token.
    pub fn has_credentials(&self) -> bool {
        self.cookie("X-APPLE-WEBAUTH-TOKEN").is_some()
    }
}

/// Where the session of `account` is kept under `dir`.
fn session_files(account: &str, dir: &Path) -> (PathBuf, PathBuf) {
    let stem: String = account.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
    (dir.join(format!("{stem}.session")), dir.join(format!("{stem}.cookiejar")))
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::Protocol("invalid header value".into()))
}

fn reason_phrase(status: u16) -> String {
    StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("unknown status").to_owned()
}

fn is_hsa2_challenge(body: Option<&Value>) -> bool {
    let Some(Value::Object(map)) = body else { return false };
    ["authType", "authenticationType"].iter().any(|key| map.get(*key).and_then(Value::as_str) == Some("hsa2"))
}

/// Extract an API-level error from a JSON object, if it holds one.
fn api_error_from_body(map: &serde_json::Map<String, Value>) -> Option<Error> {
    let reason = ["errorMessage", "reason", "errorReason", "error"]
        .iter()
        .filter_map(|key| map.get(*key))
        .find(|v| !is_falsy(v))?;
    let reason = reason.as_str().map_or_else(|| "Unknown reason".to_owned(), str::to_owned);
    let code = ["errorCode", "serverErrorCode"]
        .iter()
        .filter_map(|key| map.get(*key))
        .find(|v| !is_falsy(v))
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
    Some(make_api_error(code, reason))
}

/// Log the outcome of a request: status and address, never the query string,
/// headers or body.
fn trace_reply(response: &Response) {
    let url = response.url();
    tracing::debug!(status = response.status().as_u16(), "{}{}", url.host_str().unwrap_or("?"), url.path());
}

fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::String(s) => s.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn make_api_error(code: Option<String>, reason: String) -> Error {
    // What iCloud's web services say once the session's cookies have lapsed.
    // Unlike the generic 421 below, these name the session explicitly.
    if is_expired_session_reason(&reason) {
        return Error::AuthRequired(reason);
    }
    match code.as_deref() {
        Some("ZONE_NOT_FOUND" | "AUTHENTICATION_FAILED") => {
            Error::NotActivated("log into https://icloud.com/ once to finish setting up the service".into())
        }
        Some("ACCESS_DENIED") => Error::Api {
            reason: format!("{reason}. Wait a few minutes and try again: the servers may be throttling requests"),
            code,
        },
        Some("409" | "450" | "421" | "500") => {
            Error::Api { code, reason: "Authentication required for Account.".into() }
        }
        _ => Error::Api { code, reason },
    }
}

fn is_expired_session_reason(reason: &str) -> bool {
    let trimmed = reason.trim();
    [
        "Missing X-APPLE-WEBAUTH-TOKEN cookie",
        "Missing X-APPLE-WEBAUTH-USER cookie",
        "Invalid global session",
        "Invalid session",
        "Session expired",
    ]
    .iter()
    .any(|known| trimmed.eq_ignore_ascii_case(known))
}

fn load_session_data(path: &Path) -> SessionData {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            tracing::warn!("ignoring unreadable session file {}: {err}", path.display());
            SessionData::default()
        }),
        Err(_) => SessionData::default(),
    }
}

fn load_cookies(path: &Path) -> cookie_store::CookieStore {
    match fs::File::open(path) {
        Ok(file) => cookie_store::serde::json::load_all(io::BufReader::new(file)).unwrap_or_else(|err| {
            tracing::warn!("ignoring unreadable cookie file {}: {err}", path.display());
            cookie_store::CookieStore::default()
        }),
        Err(_) => cookie_store::CookieStore::default(),
    }
}

static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `path` atomically with mode 0600, creating the parent
/// directory with mode 0700. Uses a unique temporary name so concurrent
/// flushes of `.session` and `.cookiejar` can never collide.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    let base = path.file_name().and_then(|n| n.to_str()).unwrap_or("session");
    let pid = std::process::id();
    let tmp = loop {
        let seq = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{base}.{pid}.{seq}.tmp"));
        match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&candidate) {
            Ok(mut file) => {
                let write_res = file.write_all(contents).and_then(|()| file.sync_all());
                if let Err(err) = write_res {
                    let _ = fs::remove_file(&candidate);
                    return Err(err);
                }
                break candidate;
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    };
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn obj(v: &Value) -> &serde_json::Map<String, Value> {
        v.as_object().unwrap()
    }

    #[test]
    fn json_error_bodies_are_recognised() {
        let body = serde_json::json!({"errorMessage": "nope", "errorCode": "ACCESS_DENIED"});
        let err = api_error_from_body(obj(&body)).unwrap();
        assert!(err.to_string().contains("throttling"));

        let body = serde_json::json!({"reason": "", "error": false, "status": "ok"});
        assert!(api_error_from_body(obj(&body)).is_none());

        let body = serde_json::json!({"error": {"nested": true}});
        assert!(api_error_from_body(obj(&body)).unwrap().to_string().contains("Unknown reason"));
    }

    #[test]
    fn zone_not_found_means_service_not_activated() {
        let body = serde_json::json!({"reason": "x", "errorCode": "ZONE_NOT_FOUND"});
        assert!(matches!(api_error_from_body(obj(&body)), Some(Error::NotActivated(_))));
    }

    #[test]
    fn auth_status_codes_get_a_uniform_message() {
        let err = make_api_error(Some("421".into()), "Misdirected".into());
        assert!(err.to_string().contains("Authentication required"));
        assert!(!err.is_auth(), "421 is also used for wrong-shard requests");
    }

    #[test]
    fn missing_webauth_cookie_is_an_auth_error() {
        let err = make_api_error(None, "Missing X-APPLE-WEBAUTH-TOKEN cookie".into());
        assert!(err.is_auth());
    }

    #[test]
    fn a_lapsed_global_session_is_an_auth_error_whatever_the_status() {
        let body = serde_json::json!({"error": "Invalid global session"});
        assert!(matches!(api_error_from_body(obj(&body)), Some(Error::AuthRequired(_))));
        assert!(make_api_error(Some("421".into()), "Invalid global session".into()).is_auth());
    }

    #[test]
    fn hsa2_challenge_detection_accepts_both_field_names() {
        assert!(is_hsa2_challenge(Some(&serde_json::json!({"authType": "hsa2"}))));
        assert!(is_hsa2_challenge(Some(&serde_json::json!({"authenticationType": "hsa2"}))));
        assert!(!is_hsa2_challenge(Some(&serde_json::json!({"authType": "sa"}))));
        assert!(!is_hsa2_challenge(None));
    }

    #[test]
    fn session_data_and_cookies_survive_a_restart_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = dir.path().join("state");
        let session = Session::open("me@example.com", Some(&cookies), "https://www.icloud.com", "cid-1").unwrap();
        session.flush().unwrap();

        let session_file = cookies.join("meexamplecom.session");
        let mode = fs::metadata(&session_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&cookies).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);

        let again = Session::open("me@example.com", Some(&cookies), "https://www.icloud.com", "cid-2").unwrap();
        assert_eq!(again.data().client_id.as_deref(), Some("cid-1"), "client id is sticky");

        again.clear();
        assert!(!session_file.exists());
    }

    #[test]
    fn cookies_from_a_browser_make_a_session_and_expired_ones_are_dropped() {
        let session = Session::open("me.com", None, "https://www.icloud.com", "cid").unwrap();
        assert!(!session.has_credentials());
        let cookie = |name: &str, expires: Option<f64>| BrowserCookie {
            name: name.into(),
            value: "v".into(),
            domain: ".icloud.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expires,
        };
        let far = 4_102_444_800.0;
        let taken = session.import_cookies(&[
            cookie("X-APPLE-WEBAUTH-TOKEN", Some(far)),
            cookie("old", Some(1.0)),
            cookie("session-only", None),
        ]);
        assert_eq!(taken, 2);
        assert!(session.has_credentials());
        assert!(session.cookie("old").is_none());
        assert!(session.cookie("session-only").is_some());
    }

    #[test]
    fn an_unsaved_session_can_be_saved_under_an_account_found_later() {
        let dir = tempfile::tempdir().unwrap();
        let temporary = Session::open("unknown", None, "https://www.icloud.com", "cid-1").unwrap();
        let cookie = BrowserCookie {
            name: "X-APPLE-WEBAUTH-TOKEN".into(),
            value: "v".into(),
            domain: ".icloud.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expires: Some(4_102_444_800.0),
        };
        temporary.import_cookies(&[cookie]);
        temporary.save_as("me@example.com", dir.path()).unwrap();

        let reopened = Session::open("me@example.com", Some(dir.path()), "https://www.icloud.com", "cid-2").unwrap();
        assert!(reopened.has_credentials());
        assert_eq!(reopened.data().client_id.as_deref(), Some("cid-1"));
        let mode = fs::metadata(dir.path().join("meexamplecom.cookiejar")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn corrupt_session_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("mecom.session"), b"{not json").unwrap();
        fs::write(dir.path().join("mecom.cookiejar"), b"garbage").unwrap();
        let session = Session::open("me.com", Some(dir.path()), "https://www.icloud.com", "cid").unwrap();
        assert_eq!(session.data().client_id.as_deref(), Some("cid"));
        assert!(!session.has_credentials());
    }

    #[test]
    fn reloading_from_disk_picks_up_a_newer_session_written_by_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let make_cookie = |val: &str| BrowserCookie {
            name: "X-APPLE-WEBAUTH-TOKEN".into(),
            value: val.into(),
            domain: ".icloud.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expires: Some(4_102_444_800.0),
        };
        let first = Session::open("me@example.com", Some(dir.path()), "https://www.icloud.com", "cid-1").unwrap();
        first.import_cookies(&[make_cookie("old")]);
        first.flush().unwrap();
        assert!(!first.has_newer_on_disk());
        assert!(!first.reload_from_disk_if_newer());

        // Another process signs in and saves a newer, longer cookie value.
        let writer = Session::open("unknown", None, "https://www.icloud.com", "cid-2").unwrap();
        writer.import_cookies(&[make_cookie("fresh-longer-token")]);
        writer.save_as("me@example.com", dir.path()).unwrap();

        assert!(first.has_newer_on_disk());
        assert!(first.reload_from_disk_if_newer());
        assert_eq!(first.cookie("X-APPLE-WEBAUTH-TOKEN").as_deref(), Some("fresh-longer-token"));
        assert_eq!(first.data().client_id.as_deref(), Some("cid-2"));
        assert!(!first.has_newer_on_disk());
    }

    #[test]
    fn expired_session_reasons_and_transient_errors_are_classified_accurately() {
        for reason in ["Missing X-APPLE-WEBAUTH-USER cookie", "Invalid session", "Session expired"] {
            assert!(make_api_error(None, reason.into()).is_auth(), "{reason}");
        }
        let unauthorized = Error::Status { status: 401, reason: "Unauthorized".into() };
        assert!(unauthorized.may_be_session_expired());
        assert!(!unauthorized.is_auth());

        let unavailable = Error::Status { status: 503, reason: "Service Unavailable".into() };
        assert!(unavailable.is_transient());
        assert!(!unavailable.is_auth());
    }
}
