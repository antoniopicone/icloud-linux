//! Sign-in through a browser (`icloudctl auth --from-browser`, the installer):
//! the person signs in on icloud.com in a real browser window.
//!
//! The Apple ID, the password and the verification code are typed into
//! Apple's own page; this program never sees them. What it takes is the
//! browser's Apple cookies, which Apple then checks
//! (`Client::adopt_browser_session`). The Apple ID comes from Apple's answer.
//!
//! Apple keeps its cookies `HttpOnly`, so they cannot be read from a page. A
//! Chromium-family browser is therefore started with a throw-away profile and
//! a debugging port on `127.0.0.1` that it picks itself; the cookies are read
//! from that profile only and the profile is deleted afterwards. Nothing from
//! the person's own browser profile is touched.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use icloud_api::{AccountInfo, BrowserCookie, Client, ClientConfig};
use serde_json::{Value, json};

use crate::{
    Config, Error, Layout, Result, connect,
    i18n::{tr, trf},
    setup,
};

const TRUST_COOKIE: &str = "X-APPLE-WEBAUTH-HSA-TRUST";
const SESSION_COOKIE: &str = "X-APPLE-WEBAUTH-TOKEN";
const START_URL: &str = "https://www.icloud.com/";
/// Chromium-based browsers, by the names their packages put on `PATH`.
const BROWSERS: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "brave-browser",
    "brave-browser-stable",
    "brave",
    "brave-origin",
    "brave-origin-stable",
    "microsoft-edge",
    "microsoft-edge-stable",
    "vivaldi",
    "vivaldi-stable",
    "opera",
];
/// The system's default-browser links, used when none of the above is found.
const DEFAULT_BROWSER_LINKS: &[&str] = &["x-www-browser", "gnome-www-browser"];
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_secs(2);

/// The browser to start: `$ICLOUD_BROWSER` if set, else the first known one on
/// `PATH`, else the system default browser if it is Chromium-based.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("ICLOUD_BROWSER").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(custom));
    }
    let path = std::env::var_os("PATH")?;
    let on_path = |name: &str| std::env::split_paths(&path).map(|dir| dir.join(name)).find(|c| c.is_file());
    BROWSERS.iter().find_map(|name| on_path(name)).or_else(|| {
        DEFAULT_BROWSER_LINKS
            .iter()
            .filter_map(|name| on_path(name))
            .find(|link| fs::canonicalize(link).is_ok_and(|target| is_chromium_based(&target)))
    })
}

fn is_chromium_based(binary: &Path) -> bool {
    let name = binary.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    ["chrom", "brave", "edge", "vivaldi", "opera"].iter().any(|family| name.contains(family))
}

/// A browser window open on icloud.com, with a throw-away profile. Dropping it
/// closes the browser and deletes the profile.
#[derive(Debug)]
pub struct BrowserSignIn {
    // Field order matters: the browser must stop before its profile goes.
    _process: BrowserProcess,
    _profile: tempfile::TempDir,
    ws_url: String,
}

impl BrowserSignIn {
    /// Open icloud.com in `browser`.
    pub fn start(browser: &Path) -> Result<Self> {
        let profile = tempfile::Builder::new().prefix("icloud-linux-browser-").tempdir()?;
        let mut process = BrowserProcess(spawn(browser, profile.path())?);
        let ws_url = debugging_endpoint(profile.path(), &mut process.0)?;
        Ok(Self { _process: process, _profile: profile, ws_url })
    }

    /// Wait for the person to sign in and trust the browser; returns the
    /// browser's Apple cookies.
    pub fn wait_for_session(&self, timeout: Duration) -> Result<Vec<BrowserCookie>> {
        wait_for_cookie(&self.ws_url, timeout)
    }
}

/// The whole browser sign-in: open icloud.com, wait for the person to sign in,
/// have Apple confirm the session, then save it and the Apple ID it belongs
/// to in the configuration under `layout` (which must exist: `init` first).
///
/// `opened` is called once the window is up, before the long wait. Any stored
/// password is dropped: it would belong to a sign-in this one replaces.
pub fn sign_in_and_save(layout: &Layout, opened: &mut dyn FnMut()) -> Result<AccountInfo> {
    let config = Config::load(&layout.config_file(), layout)?;
    let path = require_browser()?;
    let window = BrowserSignIn::start(&path)?;
    opened();
    let cookies = window.wait_for_session(SIGN_IN_TIMEOUT)?;
    drop(window); // close the browser before talking to Apple

    // The Apple ID is not known yet, so the session starts in memory and is
    // written under the right name once Apple has said whose it is.
    let mut client_config = ClientConfig::new("browser-sign-in", None);
    client_config.endpoints = connect::endpoints();
    let client = Client::new(client_config)?;
    client.adopt_browser_session(&cookies).map_err(|err| match err {
        icloud_api::Error::LoginFailed(detail) => {
            tracing::info!("{detail}");
            Error::Setup(
                tr("Apple did not accept the session from the browser. Try again, and choose Trust when Apple asks.")
                    .into(),
            )
        }
        other => other.into(),
    })?;
    let info = client.account_info();
    let apple_id =
        info.apple_id.clone().filter(|id| !id.trim().is_empty()).ok_or_else(|| {
            Error::Setup(tr("Apple accepted the sign-in but did not say which Apple ID it is").into())
        })?;
    client.session().save_as(&apple_id, &config.cookie_dir)?;
    setup::configure(layout, &apple_id, None)?;
    Ok(info)
}

/// [`find_browser`] with the error to show when there is none.
pub fn require_browser() -> Result<PathBuf> {
    find_browser().ok_or_else(|| {
        Error::Setup(
            tr("No Chromium-based browser was found (Chromium, Chrome, Brave, Edge, Vivaldi). \
                Install one, or set ICLOUD_BROWSER to its path.")
            .into(),
        )
    })
}

/// How long to wait for the person to finish signing in on icloud.com.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Kills the browser when dropped, so no path leaves it running.
#[derive(Debug)]
struct BrowserProcess(Child);

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(browser: &Path, profile: &Path) -> Result<Child> {
    Command::new(browser)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-debugging-port=0")
        .args(["--remote-debugging-address=127.0.0.1", "--no-first-run", "--no-default-browser-check"])
        .arg("--password-store=basic")
        // A sign-in page needs no GPU rasterisation, and on some young GPU
        // drivers it is what hangs the GPU.
        .arg("--disable-gpu-rasterization")
        .args(extra_args())
        .arg(START_URL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| Error::Setup(trf("could not start {}: {}", &[&browser.display(), &err])))
}

/// `$ICLOUD_BROWSER_ARGS`: extra flags for the browser, separated by spaces.
fn extra_args() -> Vec<String> {
    std::env::var("ICLOUD_BROWSER_ARGS").unwrap_or_default().split_whitespace().map(str::to_owned).collect()
}

/// The browser writes its chosen port and socket path to `DevToolsActivePort`.
fn debugging_endpoint(profile: &Path, child: &mut Child) -> Result<String> {
    let file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(text) = fs::read_to_string(&file)
            && let Some(url) = parse_active_port(&text)
        {
            return Ok(url);
        }
        if child.try_wait()?.is_some() {
            return Err(Error::Setup(tr("the browser exited before it was ready").into()));
        }
        if Instant::now() >= deadline {
            return Err(Error::Setup(tr("the browser did not open its debugging port in time").into()));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn parse_active_port(text: &str) -> Option<String> {
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    path.starts_with('/').then(|| format!("ws://127.0.0.1:{port}{path}"))
}

fn wait_for_cookie(ws_url: &str, timeout: Duration) -> Result<Vec<BrowserCookie>> {
    let deadline = Instant::now() + timeout;
    let (mut socket, _) =
        tungstenite::connect(ws_url).map_err(|err| Error::Setup(trf("could not talk to the browser: {}", &[&err])))?;
    let closed = || Error::Setup(tr("the browser was closed before the sign-in finished").into());
    for id in 1_u64.. {
        let request = json!({"id": id, "method": "Storage.getCookies"});
        socket.send(tungstenite::Message::text(request.to_string())).map_err(|_| closed())?;
        loop {
            let message = socket.read().map_err(|_| closed())?;
            let tungstenite::Message::Text(text) = message else { continue };
            let Ok(reply) = serde_json::from_str::<Value>(&text) else { continue };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // an event, or the answer to something else
            }
            if let Some(cookies) = signed_in_cookies(&reply) {
                return Ok(cookies);
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err(Error::Setup(tr("timed out waiting for the sign-in in the browser").into()));
        }
        thread::sleep(POLL_EVERY);
    }
    unreachable!("the loop only ends by returning")
}

fn is_apple_domain(domain: &str) -> bool {
    let host = domain.trim_start_matches('.');
    ["icloud.com", "apple.com", "icloud-content.com"].iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// The Apple cookies in a `Storage.getCookies` reply, once the sign-in is
/// complete: the session cookie and the trust cookie are both there.
fn signed_in_cookies(reply: &Value) -> Option<Vec<BrowserCookie>> {
    let cookies: Vec<BrowserCookie> = reply
        .get("result")?
        .get("cookies")?
        .as_array()?
        .iter()
        .filter_map(|c| {
            let domain = c.get("domain")?.as_str()?;
            is_apple_domain(domain).then_some(())?;
            Some(BrowserCookie {
                name: c.get("name")?.as_str()?.to_owned(),
                value: c.get("value")?.as_str()?.to_owned(),
                domain: domain.to_owned(),
                path: c.get("path").and_then(Value::as_str).unwrap_or("/").to_owned(),
                secure: c.get("secure").and_then(Value::as_bool).unwrap_or(false),
                http_only: c.get("httpOnly").and_then(Value::as_bool).unwrap_or(false),
                expires: c.get("expires").and_then(Value::as_f64).filter(|e| *e > 0.0),
            })
        })
        .collect();
    let has = |name: &str| cookies.iter().any(|c| c.name == name && !c.value.is_empty());
    (has(TRUST_COOKIE) && has(SESSION_COOKIE)).then_some(cookies)
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread};

    use super::*;

    #[test]
    fn chromium_family_binaries_are_recognised_by_name() {
        for yes in ["/opt/brave.com/brave-origin/brave-origin", "/usr/bin/chromium", "/opt/google/chrome/chrome"] {
            assert!(is_chromium_based(Path::new(yes)), "{yes}");
        }
        for no in ["/usr/lib/firefox/firefox", "/usr/bin/epiphany", "/usr/bin/lynx"] {
            assert!(!is_chromium_based(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn the_active_port_file_gives_the_socket_address() {
        assert_eq!(
            parse_active_port("45123\n/devtools/browser/abc-1\n").as_deref(),
            Some("ws://127.0.0.1:45123/devtools/browser/abc-1")
        );
        assert!(parse_active_port("45123\n").is_none());
        assert!(parse_active_port("x\n/devtools/browser/a\n").is_none());
        assert!(parse_active_port("45123\nnot-a-path\n").is_none());
    }

    fn cookie(name: &str, domain: &str, value: &str) -> Value {
        json!({"name": name, "domain": domain, "value": value, "path": "/", "secure": true, "httpOnly": true, "expires": 4_102_444_800.0})
    }

    fn reply(list: Vec<Value>) -> Value {
        json!({"id": 1, "result": {"cookies": list}})
    }

    #[test]
    fn the_sign_in_is_complete_only_with_both_the_session_and_the_trust_cookie() {
        let trust = cookie(TRUST_COOKIE, ".icloud.com", "t");
        let session = cookie(SESSION_COOKIE, ".icloud.com", "s");
        assert!(signed_in_cookies(&reply(vec![])).is_none());
        assert!(signed_in_cookies(&reply(vec![trust.clone()])).is_none());
        assert!(signed_in_cookies(&reply(vec![session.clone()])).is_none());
        assert!(signed_in_cookies(&reply(vec![cookie(TRUST_COOKIE, ".icloud.com", ""), session.clone()])).is_none());
        let done = signed_in_cookies(&reply(vec![trust, session, cookie("other", ".apple.com", "o")])).unwrap();
        assert_eq!(done.len(), 3);
        assert!(done.iter().all(|c| c.secure && c.http_only && c.expires.is_some()));
    }

    #[test]
    fn cookies_of_other_sites_are_never_taken() {
        let list = vec![
            cookie(TRUST_COOKIE, ".icloud.com", "t"),
            cookie(SESSION_COOKIE, ".icloud.com", "s"),
            cookie("tracker", ".example.com", "x"),
            cookie("sneaky", "evilicloud.com", "x"),
            cookie("sneaky2", "icloud.com.example", "x"),
        ];
        let done = signed_in_cookies(&reply(list)).unwrap();
        assert_eq!(done.len(), 2);
    }

    #[test]
    fn polling_waits_until_the_cookie_appears() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            for round in 0..2 {
                let request: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(request["method"], "Storage.getCookies");
                let cookies = if round == 0 {
                    json!([])
                } else {
                    json!([
                        cookie(TRUST_COOKIE, ".icloud.com", "trusted"),
                        cookie(SESSION_COOKIE, ".icloud.com", "session"),
                    ])
                };
                let reply = json!({"id": request["id"], "result": {"cookies": cookies}});
                ws.send(tungstenite::Message::text(reply.to_string())).unwrap();
            }
        });
        let cookies = wait_for_cookie(&format!("ws://127.0.0.1:{port}/devtools/browser/x"), Duration::from_secs(30));
        server.join().unwrap();
        assert!(cookies.unwrap().iter().any(|c| c.value == "trusted"));
    }

    #[test]
    fn a_closed_browser_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(tungstenite::accept(stream).unwrap());
        });
        let err = wait_for_cookie(&format!("ws://127.0.0.1:{port}/x"), Duration::from_secs(5)).unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("closed"));
    }
}
