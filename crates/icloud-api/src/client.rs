//! Signing in to iCloud and reaching its services.
//!
//! [`Client`] is the port of the authentication half of `pyicloud`'s
//! `PyiCloudService`: SRP against `idmsa.apple.com`, the two-factor
//! challenge, session trust and `accountLogin`, which returns the list of web
//! service hosts.
//!
//! The intended use is:
//!
//! 1. The long-running daemon calls [`Client::resume`], which needs no
//!    password. It fails cleanly when the session has lapsed.
//! 2. An interactive tool calls [`Client::login`] with the password, and when
//!    that reports [`LoginStatus::TwoFactorRequired`] walks the user through
//!    [`Client::send_sms`] / [`Client::verify_sms`] or
//!    [`Client::verify_trusted_device`].
//!
//! A login attempt sends the password proof exactly once and never retries on
//! its own: repeated rejections are what makes Apple lock an account.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::{
    drive::HttpDrive,
    error::{Error, Result},
    session::{CT_JSON, Session, USER_AGENT},
    srp::{Protocol, SrpClient, derive_password},
    twofactor::{TrustedPhone, TwoFactorOptions, parse_boot_args},
};

const OAUTH_CLIENT_ID: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
const CLIENT_BUILD_NUMBER: &str = "2534Project66";
const CLIENT_MASTERING_NUMBER: &str = "2534B22";
const PCS_SLEEP: Duration = Duration::from_secs(5);
const PCS_MAX_RETRIES: usize = 10;

/// Where the pieces of iCloud live.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// Apple's identity provider, home of the SRP and 2FA endpoints.
    pub idmsa: String,
    /// The `icloud.com` origin sent as `Origin` and `Referer`.
    pub home: String,
    /// `…/setup/ws/1`, the account setup service.
    pub setup: String,
    /// Ask Apple which shard hosts the account before the first request.
    /// Without it non-default shards answer 421.
    pub detect_partition: bool,
}

impl Endpoints {
    pub fn production(china_mainland: bool) -> Self {
        let tld = if china_mainland { ".cn" } else { "" };
        Self {
            idmsa: "https://idmsa.apple.com".into(),
            home: format!("https://www.icloud.com{tld}"),
            setup: format!("https://setup.icloud.com{tld}/setup/ws/1"),
            detect_partition: !china_mainland,
        }
    }

    fn auth(&self) -> String {
        format!("{}/appleauth/auth", self.idmsa)
    }
}

/// Everything needed to open a [`Client`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub account: String,
    /// Where cookies and session tokens are kept. `None` keeps them in memory.
    pub session_dir: Option<PathBuf>,
    pub endpoints: Endpoints,
}

impl ClientConfig {
    pub fn new(account: impl Into<String>, session_dir: Option<PathBuf>) -> Self {
        Self { account: account.into(), session_dir, endpoints: Endpoints::production(false) }
    }
}

/// Result of [`Client::login`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginStatus {
    Authenticated,
    /// The password was accepted but a verification code is still needed.
    TwoFactorRequired,
}

/// Who the session belongs to, as far as Apple says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountInfo {
    pub apple_id: Option<String>,
    pub full_name: Option<String>,
}

#[derive(Default)]
struct State {
    /// Latest reply from `accountLogin` or `validate`.
    data: Value,
    options: TwoFactorOptions,
    requires_mfa: bool,
    partition_checked: bool,
    setup: String,
    dsid: Option<String>,
}

pub struct Client {
    session: Arc<Session>,
    endpoints: Endpoints,
    account: String,
    client_id: String,
    state: Mutex<State>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("account", &self.account).finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(config: ClientConfig) -> Result<Self> {
        let fresh_id = crate::uuid_v4()?;
        let session = Session::open(&config.account, config.session_dir.as_deref(), &config.endpoints.home, &fresh_id)?;
        let client_id = session.data().client_id.unwrap_or(fresh_id);
        let state = State { setup: config.endpoints.setup.clone(), ..State::default() };
        Ok(Self { session, endpoints: config.endpoints, account: config.account, client_id, state: Mutex::new(state) })
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    /// The underlying session, for callers that persist it on shutdown.
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// True when a session token and its cookie are on disk. Says nothing
    /// about whether Apple still accepts them.
    pub fn has_saved_session(&self) -> bool {
        self.session.has_credentials()
    }

    pub fn account_info(&self) -> AccountInfo {
        let state = self.state();
        let ds = state.data.get("dsInfo");
        let text = |key: &str| ds.and_then(|d| d.get(key)).and_then(Value::as_str).map(str::to_owned);
        AccountInfo { apple_id: text("appleId"), full_name: text("fullName") }
    }

    pub fn two_factor_options(&self) -> TwoFactorOptions {
        self.state().options.clone()
    }

    /// Seed the session with a trust token taken from a browser
    /// (`X-APPLE-WEBAUTH-HSA-TRUST`). The next [`login`](Self::login) then
    /// skips the verification code.
    pub fn set_trust_token(&self, token: &str) {
        self.session.update_data(|d| d.trust_token = Some(token.trim().to_owned()));
    }

    /// Forget the stored session, locally only.
    pub fn forget_session(&self) {
        self.session.clear();
        *self.state() = State { setup: self.endpoints.setup.clone(), partition_checked: true, ..State::default() };
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn setup_url(&self, endpoint: &str) -> String {
        format!("{}/{endpoint}", self.state().setup)
    }

    /// Query parameters iCloud's web services expect on every call.
    fn params(&self) -> BTreeMap<String, String> {
        let mut params = BTreeMap::from([
            ("clientBuildNumber".to_owned(), CLIENT_BUILD_NUMBER.to_owned()),
            ("clientMasteringNumber".to_owned(), CLIENT_MASTERING_NUMBER.to_owned()),
            ("clientId".to_owned(), self.client_id.clone()),
        ]);
        if let Some(dsid) = &self.state().dsid {
            params.insert("dsid".into(), dsid.clone());
        }
        params
    }

    // ---- sign in -------------------------------------------------------

    /// Reuse the saved session. Needs no password.
    ///
    /// Returns [`Error::AuthRequired`] when there is nothing to reuse, Apple
    /// no longer accepts it, or it belongs to an untrusted browser.
    pub fn resume(&self) -> Result<()> {
        if !self.session.has_credentials() {
            return Err(Error::AuthRequired("no saved iCloud session".into()));
        }
        self.ensure_partition();
        let reply = self
            .session
            .send(self.session.post(&self.setup_url("validate")).body("null"))
            .map_err(|err| match err {
                Error::Http(_) => err,
                other => Error::AuthRequired(other.to_string()),
            })?
            .value()?;
        self.adopt_account_data(reply);
        if !self.is_trusted() {
            return Err(Error::AuthRequired("the saved session is not trusted".into()));
        }
        Ok(())
    }

    /// Sign in with the password.
    ///
    /// A saved session token is tried first; the password proof is only sent
    /// when that does not work. Sends at most one proof.
    pub fn login(&self, password: &SecretString) -> Result<LoginStatus> {
        self.ensure_partition();

        match self.account_login(true) {
            Ok(()) => return Ok(LoginStatus::Authenticated),
            Err(Error::Http(err)) => return Err(Error::Http(err)),
            Err(other) => tracing::debug!("saved session not usable ({other}); signing in with the password"),
        }

        match self.srp_sign_in(password)? {
            LoginStatus::TwoFactorRequired => Ok(LoginStatus::TwoFactorRequired),
            LoginStatus::Authenticated => match self.account_login(true) {
                Ok(()) => {
                    self.flush();
                    Ok(LoginStatus::Authenticated)
                }
                Err(Error::TwoFactorRequired) => {
                    self.begin_challenge();
                    Ok(LoginStatus::TwoFactorRequired)
                }
                Err(other) => Err(other),
            },
        }
    }

    /// The SRP exchange with `idmsa.apple.com`.
    fn srp_sign_in(&self, password: &SecretString) -> Result<LoginStatus> {
        let auth = self.endpoints.auth();
        let oauth_state = &self.client_id;

        // Establishes the session id and `scnt` the later calls depend on.
        self.session.send(self.session.get(&format!("{auth}/authorize/signin")).query(&[
            ("frame_id", oauth_state.as_str()),
            ("skVersion", "7"),
            ("iframeid", oauth_state.as_str()),
            ("client_id", OAUTH_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", self.endpoints.home.as_str()),
            ("response_mode", "web_message"),
            ("state", oauth_state.as_str()),
            ("authVersion", "latest"),
        ]))?;

        let srp = SrpClient::new()?;
        let init = json!({
            "a": B64.encode(srp.public_a()),
            "accountName": self.account,
            "protocols": Protocol::OFFERED,
        });
        let reply = self
            .session
            .send(
                self.session
                    .post(&format!("{auth}/signin/init"))
                    .headers(self.auth_headers(&[])?)
                    .body(init.to_string()),
            )
            .map_err(as_login_failure("Failed to initiate SRP authentication."))?
            .value()?;

        let field =
            |key: &str| reply.get(key).ok_or_else(|| Error::Protocol(format!("signin/init reply lacks `{key}`")));
        let salt = decode_b64(field("salt")?)?;
        let server_public = decode_b64(field("b")?)?;
        let challenge = field("c")?.clone();
        let iterations = field("iteration")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| Error::Protocol("invalid PBKDF2 iteration count".into()))?;
        let protocol = Protocol::parse(field("protocol")?.as_str().unwrap_or_default())?;

        let derived = derive_password(password.expose_secret(), &salt, iterations, protocol)?;
        let proof = srp
            .process_challenge(&self.account, &derived, &salt, &server_public)
            .ok_or_else(|| Error::LoginFailed("the server sent invalid SRP parameters".into()))?;

        let mut complete = json!({
            "accountName": self.account,
            "c": challenge,
            "m1": B64.encode(&proof.m1),
            "m2": B64.encode(&proof.m2),
            "rememberMe": true,
            "trustTokens": [],
        });
        if let Some(token) = self.session.data().trust_token {
            complete["trustTokens"] = json!([token]);
        }

        let request = self
            .session
            .post(&format!("{auth}/signin/complete"))
            .query(&[("isRememberMeEnabled", "true")])
            .headers(self.auth_headers(&[])?)
            .body(complete.to_string());
        match self.session.send(request) {
            Ok(_) => Ok(LoginStatus::Authenticated),
            Err(Error::TwoFactorRequired) => {
                self.begin_challenge();
                Ok(LoginStatus::TwoFactorRequired)
            }
            Err(err @ Error::Http(_)) => Err(err),
            Err(_) => Err(Error::LoginFailed("Invalid email/password combination.".into())),
        }
    }

    /// `accountLogin` with the session token: exchanges it for the account
    /// record and the list of web services.
    fn account_login(&self, require_trust: bool) -> Result<()> {
        let data = self.session.data();
        let token =
            data.session_token.as_deref().ok_or_else(|| Error::LoginFailed("no session token available".into()))?;
        let body = json!({
            "accountCountryCode": data.account_country,
            "dsWebAuthToken": token,
            "extended_login": true,
            "trustToken": data.trust_token.clone().unwrap_or_default(),
        });
        let reply = self
            .session
            .send(self.session.post(&self.setup_url("accountLogin")).body(body.to_string()))
            .map_err(as_login_failure("Invalid authentication token."))?
            .value()?;

        if reply.get("termsUpdateNeeded").and_then(Value::as_bool).unwrap_or(false) {
            return Err(Error::LoginFailed(
                "Apple requires you to accept updated iCloud terms: sign in at https://www.icloud.com once".into(),
            ));
        }
        self.adopt_account_data(reply);
        if require_trust && !self.is_trusted() {
            return Err(Error::TwoFactorRequired);
        }
        let mut state = self.state();
        state.requires_mfa = false;
        Ok(())
    }

    fn adopt_account_data(&self, reply: Value) {
        let mut state = self.state();
        state.dsid = reply
            .get("dsInfo")
            .and_then(|d| d.get("dsid"))
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
        state.data = reply;
    }

    fn is_trusted(&self) -> bool {
        self.state().data.get("hsaTrustedBrowser").and_then(Value::as_bool).unwrap_or(false)
    }

    /// Record that a code is needed and learn how it can be delivered.
    fn begin_challenge(&self) {
        self.state().requires_mfa = true;
        let options = self.fetch_two_factor_options().unwrap_or_else(|err| {
            tracing::warn!("could not read the two-factor options: {err}");
            TwoFactorOptions::default()
        });
        self.state().options = options;
    }

    fn fetch_two_factor_options(&self) -> Result<TwoFactorOptions> {
        let request = self.session.get(&self.endpoints.auth()).headers(self.auth_headers(&[("Accept", "text/html")])?);
        let response = self.session.send(request)?;
        let root = match response.value() {
            Ok(json) => json,
            Err(_) => parse_boot_args(&response.text())?,
        };
        Ok(TwoFactorOptions::from_boot_json(&root))
    }

    // ---- two-factor ----------------------------------------------------

    /// Ask Apple to text a code to `phone`.
    pub fn send_sms(&self, phone: &TrustedPhone) -> Result<()> {
        let body = json!({ "phoneNumber": phone.payload(), "mode": "sms" });
        let request = self
            .session
            .put(&format!("{}/verify/phone", self.endpoints.auth()))
            .headers(self.auth_headers(&[("Accept", CT_JSON)])?)
            .body(body.to_string());
        self.session.send(request)?;
        Ok(())
    }

    /// Check a code that arrived by SMS, then trust the session.
    pub fn verify_sms(&self, phone: &TrustedPhone, code: &str) -> Result<()> {
        let code = clean_code(code)?;
        let body = json!({
            "phoneNumber": phone.payload(),
            "securityCode": { "code": code },
            "mode": phone.push_mode.as_deref().unwrap_or("sms"),
        });
        let request = self
            .session
            .post(&format!("{}/verify/phone/securitycode", self.endpoints.auth()))
            .headers(self.auth_headers(&[("Accept", "application/json, plain/text")])?)
            .body(body.to_string());
        self.check_code_reply(self.session.send(request))?;
        self.trust_session()
    }

    /// Check a code displayed on a trusted Apple device, then trust the session.
    pub fn verify_trusted_device(&self, code: &str) -> Result<()> {
        let code = clean_code(code)?;
        let request = self
            .session
            .post(&format!("{}/verify/trusteddevice/securitycode", self.endpoints.auth()))
            .headers(self.auth_headers(&[("Accept", CT_JSON)])?)
            .body(json!({ "securityCode": { "code": code } }).to_string());
        self.check_code_reply(self.session.send(request))?;
        self.trust_session()
    }

    /// Apple rejects a wrong code with an ordinary error status; a transport
    /// failure is something else and must not be reported as a wrong code.
    fn check_code_reply(&self, reply: Result<crate::session::ApiResponse>) -> Result<()> {
        match reply {
            Ok(_) => Ok(()),
            Err(err @ Error::Http(_)) => Err(err),
            Err(err) => {
                tracing::debug!("code rejected: {err}");
                Err(Error::WrongCode)
            }
        }
    }

    /// Ask Apple to remember this browser so no code is needed next time.
    fn trust_session(&self) -> Result<()> {
        self.state().requires_mfa = false;
        self.session
            .send(self.session.get(&format!("{}/2sv/trust", self.endpoints.auth())).headers(self.auth_headers(&[])?))?;
        self.account_login(true)?;
        self.flush();
        Ok(())
    }

    // ---- services --------------------------------------------------------

    /// Open the Drive service.
    pub fn drive(&self) -> Result<HttpDrive> {
        self.request_pcs("iclouddrive")?;
        let url = |key: &str| {
            self.state()
                .data
                .get("webservices")
                .and_then(|w| w.get(key))
                .and_then(|s| s.get("url"))
                .and_then(Value::as_str)
                .filter(|u| !u.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| Error::NotActivated(format!("webservice `{key}` is not available for this account")))
        };
        Ok(HttpDrive::new(Arc::clone(&self.session), url("drivews")?, url("docws")?, self.params()))
    }

    /// Ask Apple for permission to touch a service's data ("PCS"). On most
    /// accounts this is a single round trip that answers "not needed".
    fn request_pcs(&self, app: &str) -> Result<()> {
        let params = self.params();
        let check = |session: &Session, url: String| -> Result<Value> {
            session.send(session.post(&url).query(&params))?.value()
        };
        let mut state = check(&self.session, self.setup_url("requestWebAccessState"))?;

        if !state.get("isICDRSDisabled").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(());
        }
        let consented = |s: &Value| s.get("isDeviceConsentedForPCS").and_then(Value::as_bool).unwrap_or(true);

        if !consented(&state) {
            let reply = check(&self.session, self.setup_url("enableDeviceConsentForPCS"))?;
            if !reply.get("isDeviceConsentNotificationSent").and_then(Value::as_bool).unwrap_or(false) {
                return Err(Error::Api { code: None, reason: "Unable to request PCS access".into() });
            }
        }
        for _ in 0..PCS_MAX_RETRIES {
            if consented(&state) {
                break;
            }
            thread::sleep(PCS_SLEEP);
            state = check(&self.session, self.setup_url("requestWebAccessState"))?;
        }

        let mut last = String::new();
        for attempt in 0..PCS_MAX_RETRIES {
            let request = self
                .session
                .post(&self.setup_url("requestPCS"))
                .query(&params)
                .body(json!({ "appName": app, "derivedFromUserAction": attempt == 0 }).to_string());
            let reply = self.session.send(request)?.value()?;
            if reply.get("status").and_then(Value::as_str) == Some("success") {
                return Ok(());
            }
            last = reply.get("message").and_then(Value::as_str).unwrap_or_default().to_owned();
            if matches!(
                last.as_str(),
                "Requested the device to upload cookies." | "Cookies not available yet on server."
            ) {
                thread::sleep(PCS_SLEEP);
            } else {
                break;
            }
        }
        Err(Error::Api { code: None, reason: format!("Unable to request PCS access: {last}") })
    }

    // ---- plumbing ----------------------------------------------------------

    /// Find out which shard hosts the account and point the setup endpoint at
    /// it. Best effort: on failure the default endpoint is kept.
    fn ensure_partition(&self) {
        {
            let mut state = self.state();
            if state.partition_checked || !self.endpoints.detect_partition {
                state.partition_checked = true;
                return;
            }
            state.partition_checked = true;
        }
        let probe = self.session.post(&self.setup_url("validate")).body("{}");
        let partition = match self.session.send_unchecked(probe) {
            Ok(response) => response.header("x-apple-user-partition").map(str::to_owned),
            Err(err) => {
                tracing::debug!("could not detect the account partition: {err}");
                None
            }
        };
        if let Some(partition) = partition.filter(|p| p.chars().all(|c| c.is_ascii_alphanumeric())) {
            let setup = self.endpoints.setup.replacen("://", &format!("://p{partition}-"), 1);
            tracing::debug!("account lives on partition {partition}");
            self.state().setup = setup;
        }
    }

    fn flush(&self) {
        if let Err(err) = self.session.flush() {
            tracing::warn!("could not save the iCloud session: {err}");
        }
    }

    /// Headers Apple's identity endpoints require, with per-call overrides.
    fn auth_headers(&self, overrides: &[(&str, &str)]) -> Result<HeaderMap> {
        let fd_info = json!({ "U": USER_AGENT, "L": "en-US", "Z": "GMT+00:00", "V": "1.1", "F": "" }).to_string();
        let mut pairs: Vec<(&str, String)> = vec![
            ("Accept", format!("{CT_JSON}, text/javascript")),
            ("Content-Type", CT_JSON.to_owned()),
            ("X-Apple-OAuth-Client-Id", OAUTH_CLIENT_ID.to_owned()),
            ("X-Apple-OAuth-Client-Type", "firstPartyAuth".to_owned()),
            ("X-Apple-OAuth-Redirect-URI", self.endpoints.home.clone()),
            ("X-Apple-OAuth-Require-Grant-Code", "true".to_owned()),
            ("X-Apple-OAuth-Response-Mode", "web_message".to_owned()),
            ("X-Apple-OAuth-Response-Type", "code".to_owned()),
            ("X-Apple-OAuth-State", self.client_id.clone()),
            ("X-Apple-Widget-Key", OAUTH_CLIENT_ID.to_owned()),
            ("X-Apple-FD-Client-Info", fd_info),
            ("Referer", self.endpoints.idmsa.clone()),
            ("X-Apple-Frame-Id", self.client_id.clone()),
        ];
        let data = self.session.data();
        for (name, value) in [
            ("scnt", data.scnt),
            ("X-Apple-ID-Session-Id", data.session_id),
            ("X-Apple-Auth-Attributes", data.auth_attributes),
        ] {
            if let Some(value) = value {
                pairs.push((name, value));
            }
        }
        pairs.extend(overrides.iter().map(|(k, v)| (*k, (*v).to_owned())));

        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Protocol("bad header name".into()))?;
            let value = HeaderValue::from_str(&value).map_err(|_| Error::Protocol("bad header value".into()))?;
            headers.insert(name, value);
        }
        Ok(headers)
    }
}

fn decode_b64(value: &Value) -> Result<Vec<u8>> {
    let text = value.as_str().ok_or_else(|| Error::Protocol("expected a base64 string".into()))?;
    B64.decode(text).map_err(|_| Error::Protocol("invalid base64 in server reply".into()))
}

/// Turn any non-transport failure of an authentication step into
/// [`Error::LoginFailed`], keeping network errors as they are.
fn as_login_failure(message: &'static str) -> impl Fn(Error) -> Error {
    move |err| match err {
        Error::Http(_) | Error::TwoFactorRequired => err,
        _ => Error::LoginFailed(message.into()),
    }
}

/// Accept `123456`, `123 456` and `123-456`; refuse anything else before it
/// is sent to Apple.
fn clean_code(raw: &str) -> Result<String> {
    let code: String = raw.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if (4..=10).contains(&code.len()) && code.chars().all(|c| c.is_ascii_digit()) {
        Ok(code)
    } else {
        Err(Error::WrongCode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_normalised_or_refused() {
        assert_eq!(clean_code("123456").unwrap(), "123456");
        assert_eq!(clean_code(" 123 456\n").unwrap(), "123456");
        assert_eq!(clean_code("123-456").unwrap(), "123456");
        assert!(matches!(clean_code("12ab56"), Err(Error::WrongCode)));
        assert!(matches!(clean_code(""), Err(Error::WrongCode)));
        assert!(matches!(clean_code("1234567890123"), Err(Error::WrongCode)));
    }

    #[test]
    fn production_endpoints_follow_the_region() {
        let global = Endpoints::production(false);
        assert_eq!(global.setup, "https://setup.icloud.com/setup/ws/1");
        assert!(global.detect_partition);
        let china = Endpoints::production(true);
        assert_eq!(china.home, "https://www.icloud.com.cn");
        assert_eq!(china.setup, "https://setup.icloud.com.cn/setup/ws/1");
    }

    #[test]
    fn b64_decoding_rejects_garbage() {
        assert_eq!(decode_b64(&json!("AAEC")).unwrap(), vec![0, 1, 2]);
        assert!(decode_b64(&json!("!!!")).is_err());
        assert!(decode_b64(&json!(5)).is_err());
    }
}
