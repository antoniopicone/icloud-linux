//! The session flow against a scripted stand-in for Apple's servers.
//!
//! These tests pin the *shape* of the conversation (which endpoint, with
//! which cookies). They cannot vouch for what the real servers do.

use httpmock::prelude::*;
use icloud_api::{BrowserCookie, Client, ClientConfig, Endpoints, Error};
use serde_json::json;

fn endpoints(server: &MockServer) -> Endpoints {
    Endpoints { home: server.base_url(), setup: server.url("/setup/ws/1"), detect_partition: false }
}

fn client(server: &MockServer, dir: Option<&std::path::Path>) -> Client {
    Client::new(ClientConfig {
        account: "user@example.com".into(),
        session_dir: dir.map(Into::into),
        endpoints: endpoints(server),
    })
    .unwrap()
}

fn account_reply(trusted: bool) -> serde_json::Value {
    json!({
        "hsaTrustedBrowser": trusted,
        "dsInfo": {"dsid": "12345", "appleId": "user@example.com", "fullName": "U. Ser"},
        "webservices": {
            "drivews": {"url": "https://drive.example/drivews"},
            "docws": {"url": "https://docs.example/docws"},
        },
    })
}

/// A session saved by a browser sign-in, in `dir`.
fn signed_in(server: &MockServer, dir: &std::path::Path) {
    let mut validate = server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });
    client(server, Some(dir)).adopt_browser_session(&[browser_cookie("X-APPLE-WEBAUTH-TOKEN", "t")]).unwrap();
    validate.delete();
}

#[test]
fn a_saved_session_is_resumed_from_the_files_on_disk() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    signed_in(&server, dir.path());

    // A new process only has the files on disk.
    let validate = server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate").header_includes("cookie", "X-APPLE-WEBAUTH-TOKEN=t");
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });
    let again = client(&server, Some(dir.path()));
    assert!(again.has_saved_session());
    again.resume().unwrap();
    validate.assert();
}

#[test]
fn resume_without_a_session_asks_to_sign_in_and_touches_nothing() {
    let server = MockServer::start();
    let anything = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });
    let err = client(&server, None).resume().unwrap_err();
    assert!(matches!(err, Error::AuthRequired(_)));
    assert_eq!(anything.calls(), 0);
}

#[test]
fn a_lapsed_session_cannot_be_renewed_and_says_so() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    signed_in(&server, dir.path());
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(421)
            .header("content-type", "application/json")
            .json_body(json!({"error": "Invalid global session"}));
    });
    let err = client(&server, Some(dir.path())).renew_session().unwrap_err();
    assert!(err.is_auth(), "{err:?}");
}

#[test]
fn renewing_a_live_session_asks_validate_again() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    signed_in(&server, dir.path());
    let validate = server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });
    let client = client(&server, Some(dir.path()));
    client.resume().unwrap();
    client.renew_session().unwrap();
    validate.assert_calls(2);
}

#[test]
fn drive_needs_the_webservice_urls_from_the_account_record() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(200).header("content-type", "application/json").json_body(json!({
            "hsaTrustedBrowser": true, "dsInfo": {"dsid": "1"}, "webservices": {},
        }));
    });
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/requestWebAccessState");
        then.status(200).header("content-type", "application/json").json_body(json!({"isICDRSDisabled": false}));
    });
    let client = client(&server, None);
    client.adopt_browser_session(&[browser_cookie("X-APPLE-WEBAUTH-TOKEN", "t")]).unwrap();
    assert!(matches!(client.drive(), Err(Error::NotActivated(_))));
}

#[test]
fn partition_detection_redirects_the_setup_endpoint() {
    let server = MockServer::start();
    let probe = server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(421).header("x-apple-user-partition", "42");
    });
    let mut config =
        ClientConfig { account: "user@example.com".into(), session_dir: None, endpoints: endpoints(&server) };
    config.endpoints.detect_partition = true;
    // The mock server has no `p42-` host, so the follow-up call fails at the
    // network level; the point is that the probe happened and did not abort.
    let client = Client::new(config).unwrap();
    let _ = client.adopt_browser_session(&[browser_cookie("X-APPLE-WEBAUTH-TOKEN", "t")]);
    assert!(probe.calls() >= 1);
}

fn browser_cookie(name: &str, value: &str) -> BrowserCookie {
    BrowserCookie {
        name: name.into(),
        value: value.into(),
        domain: "127.0.0.1".into(),
        path: "/".into(),
        secure: false,
        http_only: true,
        expires: None,
    }
}

#[test]
fn a_browser_session_is_adopted_by_asking_icloud_com_only() {
    let server = MockServer::start();
    let identity = server.mock(|when, then| {
        when.path_includes("/appleauth/");
        then.status(500);
    });
    let validate = server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate").header_includes("cookie", "X-APPLE-WEBAUTH-TOKEN=web-token");
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });
    let dir = tempfile::tempdir().unwrap();
    let client = client(&server, Some(dir.path()));
    client
        .adopt_browser_session(&[
            browser_cookie("X-APPLE-WEBAUTH-TOKEN", "web-token"),
            browser_cookie("X-APPLE-WEBAUTH-HSA-TRUST", "trust"),
        ])
        .unwrap();
    validate.assert();
    assert_eq!(identity.calls(), 0, "nothing is sent to Apple's sign-in service");
    assert_eq!(client.account_info().apple_id.as_deref(), Some("user@example.com"));

    // It is saved like any other session.
    assert!(self::client(&server, Some(dir.path())).has_saved_session());
}

#[test]
fn a_browser_session_apple_does_not_trust_is_refused() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(200).header("content-type", "application/json").json_body(account_reply(false));
    });
    let err = client(&server, None).adopt_browser_session(&[browser_cookie("X-APPLE-WEBAUTH-TOKEN", "t")]).unwrap_err();
    assert!(matches!(err, Error::LoginFailed(_)), "{err:?}");
}

fn verified_but_not_trusted() -> serde_json::Value {
    let mut reply = account_reply(false);
    reply["hsaChallengeRequired"] = json!(false);
    reply
}

#[test]
fn a_browser_session_past_its_challenge_is_adopted_even_if_the_browser_is_not_remembered() {
    // What icloud.com gives today when the person signs in without choosing Trust.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(200).header("content-type", "application/json").json_body(verified_but_not_trusted());
    });
    let dir = tempfile::tempdir().unwrap();
    let client = client(&server, Some(dir.path()));
    client.adopt_browser_session(&[browser_cookie("X-APPLE-WEBAUTH-TOKEN", "t")]).unwrap();
    // And the daemon can resume it later.
    self::client(&server, Some(dir.path())).resume().unwrap();
}

#[test]
fn a_session_that_still_needs_a_code_is_never_resumed() {
    let server = MockServer::start();
    let mut reply = account_reply(false);
    reply["hsaChallengeRequired"] = json!(true);
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(200).header("content-type", "application/json").json_body(reply.clone());
    });
    let err = client(&server, None).adopt_browser_session(&[browser_cookie("X-APPLE-WEBAUTH-TOKEN", "t")]).unwrap_err();
    assert!(matches!(err, Error::LoginFailed(_)), "{err:?}");
}
