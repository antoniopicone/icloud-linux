//! Sign-in through a browser (`icloudctl auth --from-browser`, the installer's button): sign in on icloud.com in a real browser
//! and take the trust cookie from it.
//!
//! Apple keeps its cookies `HttpOnly`, so they cannot be read from a page. A
//! Chromium-family browser is therefore started with a throw-away profile and
//! a debugging port on `127.0.0.1` that it picks itself; the cookie is read
//! from that profile only and the profile is deleted afterwards. Nothing from
//! the person's own browser profile is touched.
//!
//! The cookie only replaces the verification code. The password is still
//! needed for the SRP exchange that produces the session token.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{Error, Result};
use serde_json::{Value, json};

const TRUST_COOKIE: &str = "X-APPLE-WEBAUTH-HSA-TRUST";
const START_URL: &str = "https://www.icloud.com/";
const BROWSERS: [&str; 7] = [
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "brave-browser",
    "microsoft-edge",
    "vivaldi",
];
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_secs(2);

/// The browser to start: `$ICLOUD_BROWSER` if set, else the first known one on `PATH`.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("ICLOUD_BROWSER").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(custom));
    }
    let path = std::env::var_os("PATH")?;
    BROWSERS.iter().find_map(|name| std::env::split_paths(&path).map(|dir| dir.join(name)).find(|c| c.is_file()))
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

    /// Wait for the person to sign in and trust the browser; returns the value
    /// of the trust cookie.
    pub fn wait_for_trust_token(&self, timeout: Duration) -> Result<String> {
        wait_for_cookie(&self.ws_url, timeout)
    }
}

/// [`find_browser`] with the error to show when there is none.
pub fn require_browser() -> Result<PathBuf> {
    find_browser().ok_or_else(|| {
        Error::Setup(
            "no Chromium-based browser found (chromium, google-chrome, brave-browser, microsoft-edge). \
             Install one, set ICLOUD_BROWSER to its path, or copy the trust cookie by hand: \
             `icloudctl auth --trust-token <value>`"
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
        .arg(START_URL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| Error::Setup(format!("could not start {}: {err}", browser.display())))
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
            return Err(Error::Setup("the browser exited before it was ready".into()));
        }
        if Instant::now() >= deadline {
            return Err(Error::Setup("the browser did not open its debugging port in time".into()));
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

fn wait_for_cookie(ws_url: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let (mut socket, _) =
        tungstenite::connect(ws_url).map_err(|err| Error::Setup(format!("could not talk to the browser: {err}")))?;
    let closed = || Error::Setup("the browser was closed before the sign-in finished".into());
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
            if let Some(token) = trust_cookie(&reply) {
                return Ok(token);
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err(Error::Setup("timed out waiting for the sign-in in the browser".into()));
        }
        thread::sleep(POLL_EVERY);
    }
    unreachable!("the loop only ends by returning")
}

/// The trust cookie in a `Storage.getCookies` reply, if Apple has set one.
fn trust_cookie(reply: &Value) -> Option<String> {
    reply.get("result")?.get("cookies")?.as_array()?.iter().find_map(|cookie| {
        let domain = cookie.get("domain")?.as_str()?.trim_start_matches('.');
        let value = cookie.get("value")?.as_str()?;
        let ours = cookie.get("name")?.as_str()? == TRUST_COOKIE
            && (domain.ends_with("icloud.com") || domain.ends_with("apple.com"))
            && !value.is_empty();
        ours.then(|| value.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread};

    use super::*;

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

    #[test]
    fn only_the_trust_cookie_on_an_apple_domain_counts() {
        let cookies = |list: Value| json!({"id": 1, "result": {"cookies": list}});
        let ok = cookies(json!([
            {"name": "X-APPLE-WEBAUTH-USER", "domain": ".icloud.com", "value": "u"},
            {"name": TRUST_COOKIE, "domain": ".icloud.com", "value": "tok"},
        ]));
        assert_eq!(trust_cookie(&ok).as_deref(), Some("tok"));
        assert!(trust_cookie(&cookies(json!([]))).is_none());
        assert!(
            trust_cookie(&cookies(json!([{"name": TRUST_COOKIE, "domain": ".icloud.com", "value": ""}]))).is_none()
        );
        assert!(
            trust_cookie(&cookies(json!([{"name": TRUST_COOKIE, "domain": ".evil.example", "value": "x"}]))).is_none()
        );
        assert!(
            trust_cookie(&cookies(json!([{"name": TRUST_COOKIE, "domain": "evilicloud.com.example", "value": "x"}])))
                .is_none()
        );
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
                    json!([{"name": TRUST_COOKIE, "domain": ".apple.com", "value": "trusted"}])
                };
                let reply = json!({"id": request["id"], "result": {"cookies": cookies}});
                ws.send(tungstenite::Message::text(reply.to_string())).unwrap();
            }
        });
        let token = wait_for_cookie(&format!("ws://127.0.0.1:{port}/devtools/browser/x"), Duration::from_secs(30));
        server.join().unwrap();
        assert_eq!(token.unwrap(), "trusted");
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
