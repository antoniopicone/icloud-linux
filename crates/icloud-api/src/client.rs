//! A session with iCloud and the services it reaches.
//!
//! Signing in happens on icloud.com, in a browser: the Apple ID, the password
//! and the verification code are typed on Apple's own page, never here.
//! [`Client::adopt_browser_session`] takes the cookies that sign-in left and
//! has Apple confirm them; from then on [`Client::resume`] and
//! [`Client::renew_session`] keep using them, and [`Client::drive`] opens
//! iCloud Drive. Nothing in this crate handles a password.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::Duration,
};

use serde_json::{Value, json};

use crate::{
    drive::HttpDrive,
    error::{Error, Result},
    session::{BrowserCookie, Session},
};

const CLIENT_BUILD_NUMBER: &str = "2534Project66";
const CLIENT_MASTERING_NUMBER: &str = "2534B22";
const PCS_SLEEP: Duration = Duration::from_secs(5);
const PCS_MAX_RETRIES: usize = 10;

/// Where the pieces of iCloud live.
#[derive(Debug, Clone)]
pub struct Endpoints {
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
            home: format!("https://www.icloud.com{tld}"),
            setup: format!("https://setup.icloud.com{tld}/setup/ws/1"),
            detect_partition: !china_mainland,
        }
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

    /// True when the session cookie is on disk. Says nothing about whether
    /// Apple still accepts it.
    pub fn has_saved_session(&self) -> bool {
        self.session.has_credentials()
    }

    pub fn account_info(&self) -> AccountInfo {
        let state = self.state();
        let ds = state.data.get("dsInfo");
        let text = |key: &str| ds.and_then(|d| d.get(key)).and_then(Value::as_str).map(str::to_owned);
        AccountInfo { apple_id: text("appleId"), full_name: text("fullName") }
    }

    /// Continue from a sign-in made in a browser: take its cookies, then
    /// check with Apple that they make a trusted session. No password and no
    /// code are involved, so nothing here can count towards a lockout.
    ///
    /// The session is then the browser's own, saved like any other.
    pub fn adopt_browser_session(&self, cookies: &[BrowserCookie]) -> Result<()> {
        self.session.import_cookies(cookies);
        self.resume().map_err(|err| match err {
            Error::Http(_) => err,
            other => Error::LoginFailed(format!("Apple did not accept the browser's session ({other})")),
        })?;
        self.flush();
        Ok(())
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

    // ---- the session -----------------------------------------------------

    /// Reuse the saved session. Needs no password.
    ///
    /// Returns [`Error::AuthRequired`] when there is nothing to reuse, Apple
    /// no longer accepts it, or it still needs a verification code.
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
        if !self.is_verified() {
            return Err(Error::AuthRequired("the saved session still needs a verification code".into()));
        }
        Ok(())
    }

    /// Renew the web session while it is in use, the way icloud.com does from
    /// an open tab: `validate` hands out fresh cookies for a session Apple
    /// still accepts. Once it no longer does, only a new sign-in on
    /// icloud.com helps. The renewed session is saved.
    pub fn renew_session(&self) -> Result<()> {
        self.resume()?;
        self.flush();
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

    /// The session needs no further code. A trusted session qualifies; so does
    /// one Apple explicitly reports as past its challenge
    /// (`hsaChallengeRequired: false`) without remembering the browser, which
    /// is what a sign-in on icloud.com gives unless the person chose Trust.
    fn is_verified(&self) -> bool {
        self.is_trusted() || self.state().data.get("hsaChallengeRequired").and_then(Value::as_bool) == Some(false)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_endpoints_follow_the_region() {
        let global = Endpoints::production(false);
        assert_eq!(global.setup, "https://setup.icloud.com/setup/ws/1");
        assert!(global.detect_partition);
        let china = Endpoints::production(true);
        assert_eq!(china.home, "https://www.icloud.com.cn");
        assert_eq!(china.setup, "https://setup.icloud.com.cn/setup/ws/1");
    }
}
