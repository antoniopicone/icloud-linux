//! HTTP session: cookie jar, Apple's session headers, persistence and the
//! normalisation of Apple's many error shapes into [`Error`].
//!
//! One [`Session`] is shared by the authentication code and by every Drive
//! request. It is `Send + Sync`; concurrent requests are fine, the parts that
//! mutate (cookies, session data, files on disk) are behind locks.

use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
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
/// Upper bound for a buffered response body. Real folder listings are a few
/// megabytes; this only protects against a misbehaving server.
const MAX_BODY_BYTES: u64 = 256 * 1024 * 1024;
/// Streams get a total timeout that grows with the expected size.
const STREAM_BASE_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_MIN_BYTES_PER_SEC: u64 = 32 * 1024;

pub(crate) const CT_JSON: &str = "application/json";
pub(crate) const CT_PLAIN: &str = "plain/text";

/// Mapping from Apple's response headers to the fields kept in the session.
const HEADER_TO_FIELD: [(&str, Field); 10] = [
    ("X-Apple-ID-Account-Country", Field::AccountCountry),
    ("X-Apple-ID-Session-Id", Field::SessionId),
    ("X-Apple-Auth-Attributes", Field::AuthAttributes),
    ("X-Apple-Session-Token", Field::SessionToken),
    ("X-Apple-TwoSV-Trust-Token", Field::TrustToken),
    ("X-Apple-TwoSV-Trust-Eligible", Field::TrustEligible),
    ("X-Apple-OAuth-Grant-Code", Field::GrantCode),
    ("X-Apple-I-Rscd", Field::AppleRscd),
    ("X-Apple-I-Ercd", Field::AppleErcd),
    ("scnt", Field::Scnt),
];

#[derive(Clone, Copy)]
enum Field {
    AccountCountry,
    SessionId,
    AuthAttributes,
    SessionToken,
    TrustToken,
    TrustEligible,
    GrantCode,
    AppleRscd,
    AppleErcd,
    Scnt,
}

/// Values Apple hands out in headers and expects back on later requests.
/// Everything here is sensitive; `Debug` prints only which fields are set.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_attributes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_eligible: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apple_rscd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apple_ercd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scnt: Option<String>,
}

impl std::fmt::Debug for SessionData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionData")
            .field("has_session_token", &self.session_token.is_some())
            .field("has_trust_token", &self.trust_token.is_some())
            .finish_non_exhaustive()
    }
}

impl SessionData {
    fn slot(&mut self, field: Field) -> &mut Option<String> {
        match field {
            Field::AccountCountry => &mut self.account_country,
            Field::SessionId => &mut self.session_id,
            Field::AuthAttributes => &mut self.auth_attributes,
            Field::SessionToken => &mut self.session_token,
            Field::TrustToken => &mut self.trust_token,
            Field::TrustEligible => &mut self.trust_eligible,
            Field::GrantCode => &mut self.grant_code,
            Field::AppleRscd => &mut self.apple_rscd,
            Field::AppleErcd => &mut self.apple_ercd,
            Field::Scnt => &mut self.scnt,
        }
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

struct Persistence {
    session_file: PathBuf,
    cookie_file: PathBuf,
    /// Last serialised form written, to skip redundant disk writes.
    last_session: Mutex<String>,
}

pub struct Session {
    client: Client,
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
            let stem: String = account.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
            Persistence {
                session_file: dir.join(format!("{stem}.session")),
                cookie_file: dir.join(format!("{stem}.cookiejar")),
                last_session: Mutex::new(String::new()),
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
            .default_headers(headers)
            .cookie_provider(Arc::clone(&jar))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;

        Ok(Arc::new(Self { client, jar, data: Mutex::new(data), persistence }))
    }

    pub fn get(&self, url: &str) -> RequestBuilder {
        self.client.get(url)
    }

    pub fn post(&self, url: &str) -> RequestBuilder {
        self.client.post(url)
    }

    pub fn put(&self, url: &str) -> RequestBuilder {
        self.client.put(url)
    }

    /// Snapshot of the session data.
    pub fn data(&self) -> SessionData {
        lock(&self.data).clone()
    }

    /// Change the session data and persist it if it changed.
    pub fn update_data(&self, change: impl FnOnce(&mut SessionData)) {
        change(&mut lock(&self.data));
        self.persist_data();
    }

    /// Value of the first unexpired cookie called `name`, on any domain.
    pub fn cookie(&self, name: &str) -> Option<String> {
        let store = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
        store.iter_unexpired().find(|c| c.name() == name && !c.value().is_empty()).map(|c| c.value().to_owned())
    }

    /// Send a request and normalise the outcome. The body is buffered.
    pub fn send(&self, request: RequestBuilder) -> Result<ApiResponse> {
        let response = self.send_unchecked(request)?;
        self.check(&response)?;
        Ok(response)
    }

    /// Like [`send`](Self::send) but leaves the status for the caller to
    /// interpret. Session headers are still absorbed.
    pub fn send_unchecked(&self, request: RequestBuilder) -> Result<ApiResponse> {
        let response = request.send()?;
        let status = response.status().as_u16();
        trace_reply(&response);
        let headers = response.headers().clone();
        let mut body = Vec::new();
        response.take(MAX_BODY_BYTES).read_to_end(&mut body)?;
        self.absorb_headers(&headers);
        Ok(ApiResponse { status, headers, body })
    }

    /// Send a request whose body is streamed to the caller. Failures are
    /// turned into [`Error`] using at most 64 KiB of the body.
    pub fn send_stream(&self, request: RequestBuilder, expected_len: u64) -> Result<Response> {
        let response = request.timeout(Self::transfer_timeout(expected_len)).send()?;
        self.absorb_headers(response.headers());
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

    fn absorb_headers(&self, headers: &HeaderMap) {
        let mut changed = false;
        {
            let mut data = lock(&self.data);
            for (name, field) in HEADER_TO_FIELD {
                let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()) else {
                    continue;
                };
                let slot = data.slot(field);
                if slot.as_deref() != Some(value) {
                    *slot = Some(value.to_owned());
                    changed = true;
                }
            }
        }
        if changed {
            self.persist_data();
        }
    }

    /// Turn Apple's status and body conventions into a `Result`.
    fn check(&self, response: &ApiResponse) -> Result<()> {
        if !(200..300).contains(&response.status) {
            let json_or_auth = response.is_json() || matches!(response.status, 409 | 421 | 450 | 500);
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
            return Error::TwoFactorRequired;
        }
        if response.status == 450 {
            return Error::AuthRequired("re-authentication required".into());
        }
        if let Some(Value::Object(map)) = &body
            && let Some(err) = api_error_from_body(map).or_else(|| service_error(map))
        {
            return err;
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
        let mut buf = Vec::new();
        {
            let store = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
            cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut buf)
                .map_err(|e| Error::Protocol(format!("could not serialise cookies: {e}")))?;
        }
        write_private(&p.cookie_file, &buf)?;
        Ok(())
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
        }
    }

    /// True when a session token and the cookie that goes with it are stored.
    pub fn has_credentials(&self) -> bool {
        lock(&self.data).session_token.is_some() && self.cookie("X-APPLE-WEBAUTH-TOKEN").is_some()
    }
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

/// The `serviceErrors` list Apple's identity endpoints use, e.g.
/// `{"code": "-20101", "message": "Your Apple Account or password was incorrect."}`.
fn service_error(map: &serde_json::Map<String, Value>) -> Option<Error> {
    let first = map.get("serviceErrors")?.as_array()?.first()?;
    let reason = first.get("message").and_then(Value::as_str)?.to_owned();
    let code = first.get("code").map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
    Some(Error::Api { code, reason })
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
    if reason == "Missing X-APPLE-WEBAUTH-TOKEN cookie" {
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

/// Write `contents` to `path` atomically with mode 0600, creating the parent
/// directory with mode 0700.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
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
    fn hsa2_challenge_detection_accepts_both_field_names() {
        assert!(is_hsa2_challenge(Some(&serde_json::json!({"authType": "hsa2"}))));
        assert!(is_hsa2_challenge(Some(&serde_json::json!({"authenticationType": "hsa2"}))));
        assert!(!is_hsa2_challenge(Some(&serde_json::json!({"authType": "sa"}))));
        assert!(!is_hsa2_challenge(None));
    }

    #[test]
    fn debug_output_hides_tokens() {
        let data = SessionData { session_token: Some("secret-token".into()), ..SessionData::default() };
        let shown = format!("{data:?}");
        assert!(!shown.contains("secret-token"));
        assert!(shown.contains("has_session_token: true"));
    }

    #[test]
    fn session_data_and_cookies_survive_a_restart_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = dir.path().join("state");
        let session = Session::open("me@example.com", Some(&cookies), "https://www.icloud.com", "cid-1").unwrap();
        session.update_data(|d| d.session_token = Some("tok".into()));
        session.flush().unwrap();

        let session_file = cookies.join("meexamplecom.session");
        let mode = fs::metadata(&session_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&cookies).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);

        let again = Session::open("me@example.com", Some(&cookies), "https://www.icloud.com", "cid-2").unwrap();
        assert_eq!(again.data().session_token.as_deref(), Some("tok"));
        assert_eq!(again.data().client_id.as_deref(), Some("cid-1"), "client id is sticky");

        again.clear();
        assert!(!session_file.exists());
    }

    #[test]
    fn corrupt_session_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("mecom.session"), b"{not json").unwrap();
        fs::write(dir.path().join("mecom.cookiejar"), b"garbage").unwrap();
        let session = Session::open("me.com", Some(dir.path()), "https://www.icloud.com", "cid").unwrap();
        assert!(session.data().session_token.is_none());
        assert!(!session.has_credentials());
    }
}
