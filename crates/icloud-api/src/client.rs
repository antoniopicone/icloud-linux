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
    sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock},
    thread,
    time::Duration,
};

use serde_json::{Value, json};

use crate::{
    drive::{DriveEndpoints, HttpDrive},
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
    /// Set when the daemon started offline using cached endpoints and still
    /// needs to run `request_pcs` once iCloud becomes reachable.
    pcs_pending: bool,
}

pub struct Client {
    session: Arc<Session>,
    endpoints: Endpoints,
    account: String,
    client_id: String,
    state: Mutex<State>,
    drive_endpoints: Arc<RwLock<DriveEndpoints>>,
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
        let saved = session.data();
        let client_id = saved.client_id.unwrap_or(fresh_id);
        let (setup, partition_checked) = if config.endpoints.detect_partition
            && let Some(saved_setup) = saved.setup_url.filter(|u| !u.trim().is_empty())
        {
            (saved_setup, true)
        } else {
            (config.endpoints.setup.clone(), false)
        };
        let state = State { setup, partition_checked, dsid: saved.dsid, ..State::default() };
        Ok(Self {
            session,
            endpoints: config.endpoints,
            account: config.account,
            client_id,
            state: Mutex::new(state),
            drive_endpoints: Arc::new(RwLock::new(DriveEndpoints::default())),
        })
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
        self.resume().map_err(|err| {
            if err.is_transient() {
                err
            } else {
                Error::LoginFailed(format!("Apple did not accept the browser's session ({err})"))
            }
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
    /// no longer accepts it, or it still needs a verification code. Transient
    /// network or server errors are returned as-is so callers can retry instead
    /// of mistakenly discarding a healthy session.
    pub fn resume(&self) -> Result<()> {
        if !self.session.has_credentials() {
            return Err(Error::AuthRequired("no saved iCloud session".into()));
        }
        self.ensure_partition();
        let params = self.params();
        let mut response =
            self.session.send_unchecked(self.session.post(&self.setup_url("validate")).query(&params).body("null"))?;
        // If Apple tells us the account lives on another shard, follow it once.
        if response.status == 421
            && self.endpoints.detect_partition
            && let Some(partition) = response
                .header("x-apple-user-partition")
                .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            let redirected = self.endpoints.setup.replacen("://", &format!("://p{partition}-"), 1);
            if self.state().setup != redirected {
                tracing::debug!("redirecting account setup to partition {partition}");
                self.state().setup = redirected.clone();
                self.session.update_data(|d| d.setup_url = Some(redirected));
                response = self
                    .session
                    .send_unchecked(self.session.post(&self.setup_url("validate")).query(&params).body("null"))?;
            }
        }
        if let Err(err) = self.session.check(&response) {
            return Err(if err.is_transient() { err } else { Error::AuthRequired(err.to_string()) });
        }
        let reply = response.value()?;
        self.adopt_account_data(reply);
        if !self.is_verified() {
            return Err(Error::AuthRequired("the saved session still needs a verification code".into()));
        }
        Ok(())
    }

    /// Renew the web session while it is in use, the way icloud.com does from
    /// an open tab: `validate` hands out fresh cookies for a session Apple
    /// still accepts. Reloads a newer session written to disk by `icloudctl auth`
    /// if one appeared, retries once on a transient socket hiccup (such as
    /// after laptop sleep), and saves the renewed session.
    pub fn renew_session(&self) -> Result<()> {
        if self.session.reload_from_disk_if_newer() {
            let saved = self.session.data();
            let mut state = self.state();
            state.dsid = saved.dsid;
            if self.endpoints.detect_partition {
                if let Some(saved_setup) = saved.setup_url.filter(|u| !u.trim().is_empty()) {
                    state.setup = saved_setup;
                    state.partition_checked = true;
                } else {
                    state.setup = self.endpoints.setup.clone();
                    state.partition_checked = false;
                }
            }
        }
        if let Err(err) = self.resume() {
            if err.is_transient() {
                tracing::debug!("session validation hit a transient error ({err}); retrying once");
                thread::sleep(Duration::from_millis(500));
                self.resume()?;
            } else {
                return Err(err);
            }
        }
        let needs_pcs = self.state().pcs_pending;
        if needs_pcs {
            self.request_pcs("iclouddrive")?;
            self.state().pcs_pending = false;
        }
        self.flush();
        Ok(())
    }

    fn adopt_account_data(&self, reply: Value) {
        let dsid = reply
            .get("dsInfo")
            .and_then(|d| d.get("dsid"))
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
        let webservice = |key: &str| {
            reply
                .get("webservices")
                .and_then(|w| w.get(key))
                .and_then(|s| s.get("url"))
                .and_then(Value::as_str)
                .filter(|u| !u.trim().is_empty())
                .map(str::to_owned)
        };
        let drivews = webservice("drivews");
        let docws = webservice("docws");
        let setup_url = {
            let mut state = self.state();
            state.dsid = dsid.clone();
            state.data = reply;
            state.setup.clone()
        };
        self.session.update_data(|d| {
            if let Some(id) = &dsid {
                d.dsid = Some(id.clone());
            }
            if self.endpoints.detect_partition && setup_url != self.endpoints.setup {
                d.setup_url = Some(setup_url.clone());
            }
            if let Some(url) = &drivews {
                d.drivews_url = Some(url.clone());
            }
            if let Some(url) = &docws {
                d.docws_url = Some(url.clone());
            }
        });
        let params = self.params();
        let mut ep = self.drive_endpoints.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(url) = drivews {
            ep.service_root = url.trim_end_matches('/').to_owned();
        }
        if let Some(url) = docws {
            ep.document_root = url.trim_end_matches('/').to_owned();
        }
        ep.params = params;
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
        let drivews = url("drivews")?;
        let docws = url("docws")?;
        *self.drive_endpoints.write().unwrap_or_else(PoisonError::into_inner) =
            DriveEndpoints::new(drivews, docws, self.params());
        Ok(HttpDrive::with_shared_endpoints(Arc::clone(&self.session), Arc::clone(&self.drive_endpoints)))
    }

    /// Open the Drive service from cached endpoints saved by a previous online
    /// session, so the daemon can mount immediately when booting offline and
    /// validate as soon as the network returns.
    pub fn drive_from_cache(&self) -> Option<HttpDrive> {
        if !self.has_saved_session() {
            return None;
        }
        let saved = self.session.data();
        let drivews = saved.drivews_url.filter(|u| !u.trim().is_empty())?;
        let docws = saved.docws_url.filter(|u| !u.trim().is_empty())?;
        self.state().pcs_pending = true;
        *self.drive_endpoints.write().unwrap_or_else(PoisonError::into_inner) =
            DriveEndpoints::new(drivews, docws, self.params());
        Some(HttpDrive::with_shared_endpoints(Arc::clone(&self.session), Arc::clone(&self.drive_endpoints)))
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
    /// it. Only remembers success once Apple actually answered, so a transient
    /// offline boot does not skip partition detection forever.
    fn ensure_partition(&self) {
        {
            let mut state = self.state();
            if state.partition_checked || !self.endpoints.detect_partition {
                state.partition_checked = true;
                return;
            }
        }
        let probe = self.session.post(&self.setup_url("validate")).body("{}");
        let response = match self.session.send_unchecked(probe) {
            Ok(response) => response,
            Err(err) => {
                tracing::debug!("could not detect the account partition: {err}");
                return;
            }
        };
        let partition = response
            .header("x-apple-user-partition")
            .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(str::to_owned);
        let mut state = self.state();
        state.partition_checked = true;
        if let Some(partition) = partition {
            let setup = self.endpoints.setup.replacen("://", &format!("://p{partition}-"), 1);
            tracing::debug!("account lives on partition {partition}");
            state.setup = setup.clone();
            drop(state);
            self.session.update_data(|d| d.setup_url = Some(setup));
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
