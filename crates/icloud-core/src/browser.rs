//! Sign-in through a browser (`icloudctl auth`, the installer, the
//! notification of an expired session):
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
    os::unix::process::CommandExt,
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

/// Set by older icloud.com sign-ins when the browser is trusted; imported when
/// present, never waited for.
#[cfg(test)]
const TRUST_COOKIE: &str = "X-APPLE-WEBAUTH-HSA-TRUST";
/// Set together when the sign-in is complete (`accountLogin`).
const SESSION_COOKIE: &str = "X-APPLE-WEBAUTH-TOKEN";
const USER_COOKIE: &str = "X-APPLE-WEBAUTH-USER";
const WS_READ_TIMEOUT: Duration = Duration::from_secs(5);
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

    /// Wait until the browser has Apple cookies that `accept` confirms as a
    /// verified session. Keeping the browser open during validation means a
    /// sign-in that is still on the 2FA or "Trust this browser?" screen is
    /// never interrupted prematurely.
    fn wait_for_accepted_session<T>(
        &self,
        timeout: Duration,
        accept: impl FnMut(&[BrowserCookie]) -> std::result::Result<T, Option<Error>>,
    ) -> Result<T> {
        wait_for_cookie_matching(&self.ws_url, timeout, accept)
    }
}

/// The whole browser sign-in: open icloud.com, wait for the person to sign in,
/// have Apple confirm the session, then save it and the Apple ID it belongs
/// to in the configuration under `layout` (which must exist: `init` first).
///
/// `opened` is called once the window is up, before the long wait.
pub fn sign_in_and_save(layout: &Layout, opened: &mut dyn FnMut()) -> Result<AccountInfo> {
    let config = Config::load(&layout.config_file(), layout)?;
    let path = require_browser()?;
    let window = BrowserSignIn::start(&path)?;
    opened();

    // Keep the browser open while verifying the session with Apple so a user
    // who is still finishing 2FA or clicking "Trust" does not have the window
    // closed mid-flow.
    let mut last_rejected: Option<Vec<BrowserCookie>> = None;
    let mut saw_unverified = false;
    let client = window
        .wait_for_accepted_session(SIGN_IN_TIMEOUT, |cookies| {
            if last_rejected.as_deref() == Some(cookies) {
                return Err(None);
            }
            let mut client_config = ClientConfig::new("browser-sign-in", None);
            client_config.endpoints = connect::endpoints();
            let client = match Client::new(client_config) {
                Ok(c) => c,
                Err(err) => return Err(Some(err.into())),
            };
            match client.adopt_browser_session(cookies) {
                Ok(()) => Ok(client),
                Err(icloud_api::Error::LoginFailed(detail)) => {
                    tracing::debug!("browser cookies not yet accepted by Apple ({detail}); keeping browser open");
                    last_rejected = Some(cookies.to_vec());
                    saw_unverified = true;
                    Err(None)
                }
                Err(err) if err.is_transient() => {
                    tracing::debug!("transient error verifying browser session ({err}); retrying while browser is open");
                    Err(None)
                }
                Err(other) => Err(Some(other.into())),
            }
        })
        .map_err(|err| {
            if saw_unverified && err.to_string().contains("timed out") {
                Error::Setup(
                    tr("Apple did not accept the session from the browser. Try again, and choose Trust when Apple asks.")
                        .into(),
                )
            } else {
                err
            }
        })?;
    drop(window);

    let info = client.account_info();
    let apple_id =
        info.apple_id.clone().filter(|id| !id.trim().is_empty()).ok_or_else(|| {
            Error::Setup(tr("Apple accepted the sign-in but did not say which Apple ID it is").into())
        })?;
    client.session().save_as(&apple_id, &config.cookie_dir)?;
    setup::configure(layout, &apple_id)?;
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

/// Stops the browser when dropped, so no path leaves it running.
///
/// The browser command is often a wrapper script (`/usr/bin/brave-origin`,
/// `google-chrome`) that starts the real browser as its child, so killing the
/// child alone would leave the window open. The browser is started in a
/// process group of its own, and the whole group is stopped.
#[derive(Debug)]
struct BrowserProcess(Child);

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};

        let group = Pid::from_child(&self.0);
        let _ = kill_process_group(group, Signal::TERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let _ = self.0.try_wait(); // reap the wrapper, or the group never looks empty
            if test_kill_process_group(group).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = kill_process_group(group, Signal::KILL);
        let _ = self.0.wait();
    }
}

fn spawn(browser: &Path, profile: &Path) -> Result<Child> {
    Command::new(browser)
        .process_group(0)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-debugging-port=0")
        .args(["--remote-debugging-address=127.0.0.1", "--no-first-run", "--no-default-browser-check"])
        .arg("--password-store=basic")
        // A sign-in page needs no GPU rasterisation, and on some young GPU
        // drivers it is what hangs the GPU.
        .arg("--disable-gpu-rasterization")
        .args(extra_args())
        .arg(format!("{}/", connect::endpoints().home.trim_end_matches('/')))
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
    wait_for_cookie_matching(ws_url, timeout, |cookies| Ok(cookies.to_vec()))
}

fn wait_for_cookie_matching<T>(
    ws_url: &str,
    timeout: Duration,
    mut accept: impl FnMut(&[BrowserCookie]) -> std::result::Result<T, Option<Error>>,
) -> Result<T> {
    let deadline = Instant::now() + timeout;
    let (mut socket, _) =
        tungstenite::connect(ws_url).map_err(|err| Error::Setup(trf("could not talk to the browser: {}", &[&err])))?;
    if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = socket.get_mut() {
        let _ = tcp.set_read_timeout(Some(WS_READ_TIMEOUT));
    }
    let closed = || Error::Setup(tr("the browser was closed before the sign-in finished").into());
    for id in 1_u64.. {
        let request = json!({"id": id, "method": "Storage.getCookies"});
        socket.send(tungstenite::Message::text(request.to_string())).map_err(|_| closed())?;
        loop {
            let message = match socket.read() {
                Ok(msg) => msg,
                Err(tungstenite::Error::Io(ref err))
                    if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    if Instant::now() >= deadline {
                        return Err(Error::Setup(tr("timed out waiting for the sign-in in the browser").into()));
                    }
                    continue;
                }
                Err(_) => return Err(closed()),
            };
            let tungstenite::Message::Text(text) = message else { continue };
            let Ok(reply) = serde_json::from_str::<Value>(&text) else { continue };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // an event, or the answer to something else
            }
            if let Some(cookies) = signed_in_cookies(&reply) {
                match accept(&cookies) {
                    Ok(done) => return Ok(done),
                    Err(Some(err)) => return Err(err),
                    Err(None) => {}
                }
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
    ["icloud.com", "icloud.com.cn", "apple.com", "icloud-content.com"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// The Apple cookies in a `Storage.getCookies` reply, once the sign-in is
/// complete: icloud.com sets the session and user cookies together, after
/// the code and the "trust this browser" question.
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
    (has(SESSION_COOKIE) && has(USER_COOKIE)).then_some(cookies)
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
    fn the_sign_in_is_complete_once_the_session_and_user_cookies_are_both_set() {
        let session = cookie(SESSION_COOKIE, ".icloud.com", "s");
        let user = cookie(USER_COOKIE, ".icloud.com", "u");
        assert!(signed_in_cookies(&reply(vec![])).is_none());
        assert!(signed_in_cookies(&reply(vec![session.clone()])).is_none());
        assert!(signed_in_cookies(&reply(vec![user.clone()])).is_none());
        assert!(signed_in_cookies(&reply(vec![cookie(SESSION_COOKIE, ".icloud.com", ""), user.clone()])).is_none());
        // Today's icloud.com sets no trust cookie; it is imported when an older flow sets one.
        let done = signed_in_cookies(&reply(vec![session.clone(), user.clone()])).unwrap();
        assert_eq!(done.len(), 2);
        let with_trust = signed_in_cookies(&reply(vec![
            session,
            user,
            cookie(TRUST_COOKIE, ".icloud.com", "t"),
            cookie("other", ".apple.com", "o"),
        ]))
        .unwrap();
        assert_eq!(with_trust.len(), 4);
        assert!(with_trust.iter().all(|c| c.secure && c.http_only && c.expires.is_some()));
    }

    #[test]
    fn cookies_of_other_sites_are_never_taken() {
        let list = vec![
            cookie(USER_COOKIE, ".icloud.com", "u"),
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
                        cookie(USER_COOKIE, ".icloud.com", "user"),
                        cookie(SESSION_COOKIE, ".icloud.com", "session"),
                    ])
                };
                let reply = json!({"id": request["id"], "result": {"cookies": cookies}});
                ws.send(tungstenite::Message::text(reply.to_string())).unwrap();
            }
        });
        let cookies = wait_for_cookie(&format!("ws://127.0.0.1:{port}/devtools/browser/x"), Duration::from_secs(30));
        server.join().unwrap();
        assert!(cookies.unwrap().iter().any(|c| c.value == "session"));
    }

    #[test]
    fn stopping_the_browser_also_stops_what_its_wrapper_script_started() {
        // Like /usr/bin/brave-origin: a shell script whose child is the real browser.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let wrapper = Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 60 & echo $! > {}; wait", pid_file.display()))
            .process_group(0)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let child_pid = loop {
            if let Ok(text) = fs::read_to_string(&pid_file)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the wrapper never started its child");
            thread::sleep(Duration::from_millis(20));
        };
        drop(BrowserProcess(wrapper));
        let alive = Path::new(&format!("/proc/{child_pid}")).exists()
            && !fs::read_to_string(format!("/proc/{child_pid}/stat")).unwrap_or_default().contains(") Z ");
        assert!(!alive, "the browser started by the wrapper is still running");
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
