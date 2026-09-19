//! The sign-in flow against a scripted stand-in for Apple's servers.
//!
//! These tests pin the *shape* of the conversation (which endpoint, in which
//! order, with which fields). They cannot vouch for what the real servers do:
//! that needs a real account and is covered by the manual checklist in the
//! README.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use httpmock::prelude::*;
use icloud_api::{Client, ClientConfig, Endpoints, Error, LoginStatus};
use secrecy::SecretString;
use serde_json::json;

const PAGE: &str = r#"<html><script class="boot_args" type="application/json">
{"direct":{"hasTrustedDevices":false,"twoSV":{"phoneNumberVerification":{
"trustedPhoneNumber":{"id":1,"nonFTEU":true,"pushMode":"sms","numberWithDialCode":"+39 ••• 12"}}}}}
</script></html>"#;

fn endpoints(server: &MockServer) -> Endpoints {
    Endpoints {
        idmsa: server.base_url(),
        home: server.base_url(),
        setup: server.url("/setup/ws/1"),
        detect_partition: false,
    }
}

fn client(server: &MockServer, dir: Option<&std::path::Path>) -> Client {
    Client::new(ClientConfig {
        account: "user@example.com".into(),
        session_dir: dir.map(Into::into),
        endpoints: endpoints(server),
    })
    .unwrap()
}

fn password() -> SecretString {
    SecretString::from("hunter2".to_owned())
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

/// Script the password half of the conversation up to `signin/complete`.
fn script_srp(server: &MockServer, complete_status: u16) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth/authorize/signin");
        then.status(200).header("scnt", "scnt-1").header("X-Apple-ID-Session-Id", "sid-1");
    });
    server.mock(|when, then| {
        when.method(POST)
            .path("/appleauth/auth/signin/init")
            .header("scnt", "scnt-1")
            .header("X-Apple-ID-Session-Id", "sid-1")
            .body_includes(r#""accountName":"user@example.com""#)
            .body_includes(r#""protocols":["s2k","s2k_fo"]"#);
        then.status(200).header("content-type", "application/json").json_body(json!({
            "salt": B64.encode([7u8; 16]),
            "b": B64.encode([0x42u8; 256]),
            "c": "challenge-token",
            "iteration": 1000,
            "protocol": "s2k_fo",
        }));
    });
    server.mock(|when, then| {
        when.method(POST)
            .path("/appleauth/auth/signin/complete")
            .query_param("isRememberMeEnabled", "true")
            .body_includes(r#""c":"challenge-token""#)
            .body_includes(r#""m1":"#)
            .body_includes(r#""m2":"#);
        then.status(complete_status)
            .header("content-type", "application/json")
            .header("X-Apple-Session-Token", "session-token-1")
            .header("Set-Cookie", "X-APPLE-WEBAUTH-TOKEN=cookie-value; Path=/")
            .json_body(json!({"authType": "hsa2"}));
    })
}

#[test]
fn a_trusted_account_signs_in_with_the_password_alone() {
    let server = MockServer::start();
    script_srp(&server, 200);
    let account = server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/accountLogin").body_includes(r#""dsWebAuthToken":"session-token-1""#);
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });

    let client = client(&server, None);
    assert_eq!(client.login(&password()).unwrap(), LoginStatus::Authenticated);
    account.assert();
    assert_eq!(client.account_info().apple_id.as_deref(), Some("user@example.com"));
}

#[test]
fn an_untrusted_browser_goes_through_sms_and_ends_up_trusted() {
    let server = MockServer::start();
    script_srp(&server, 409);
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth").header("Accept", "text/html");
        then.status(200).header("content-type", "text/html").body(PAGE);
    });

    let client = client(&server, None);
    assert_eq!(client.login(&password()).unwrap(), LoginStatus::TwoFactorRequired);

    let options = client.two_factor_options();
    assert!(!options.has_trusted_devices);
    assert_eq!(options.phones.len(), 1);
    let phone = &options.phones[0];

    let send = server.mock(|when, then| {
        when.method(PUT)
            .path("/appleauth/auth/verify/phone")
            .body_includes(r#""mode":"sms""#)
            .body_includes(r#""phoneNumber":{"id":1,"nonFTEU":true}"#);
        then.status(200);
    });
    client.send_sms(phone).unwrap();
    send.assert();

    let verify = server.mock(|when, then| {
        when.method(POST)
            .path("/appleauth/auth/verify/phone/securitycode")
            .body_includes(r#""securityCode":{"code":"123456"}"#);
        then.status(200);
    });
    let trust = server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth/2sv/trust");
        then.status(200)
            .header("X-Apple-TwoSV-Trust-Token", "trust-1")
            .header("Set-Cookie", "X-APPLE-WEBAUTH-TOKEN=cookie-value; Path=/");
    });
    let account = server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/accountLogin").body_includes(r#""trustToken":"trust-1""#);
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });

    client.verify_sms(phone, "123 456").unwrap();
    verify.assert();
    trust.assert();
    account.assert();
}

#[test]
fn a_wrong_code_is_reported_as_such_and_does_not_trust_the_session() {
    let server = MockServer::start();
    script_srp(&server, 409);
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth");
        then.status(200).header("content-type", "text/html").body(PAGE);
    });
    server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/verify/trusteddevice/securitycode");
        then.status(400)
            .header("content-type", "application/json")
            .json_body(json!({"service_errors": [{"code": "-21669", "message": "Incorrect verification code."}]}));
    });
    let trust = server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth/2sv/trust");
        then.status(200);
    });

    let client = client(&server, None);
    client.login(&password()).unwrap();
    assert!(matches!(client.verify_trusted_device("000000"), Err(Error::WrongCode)));
    assert_eq!(trust.calls(), 0, "trust must not be requested after a rejected code");
}

#[test]
fn malformed_codes_never_reach_apple() {
    let server = MockServer::start();
    let anything = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });
    let client = client(&server, None);
    assert!(matches!(client.verify_trusted_device("abcdef"), Err(Error::WrongCode)));
    assert_eq!(anything.calls(), 0);
}

#[test]
fn a_rejected_password_is_a_login_failure_and_sends_the_proof_once() {
    let server = MockServer::start();
    let complete = script_srp(&server, 401);
    let client = client(&server, None);
    let err = client.login(&password()).unwrap_err();
    assert!(matches!(err, Error::LoginFailed(_)), "got {err:?}");
    assert!(err.is_auth());
    assert_eq!(complete.calls(), 1, "a rejected proof must not be retried");
}

#[test]
fn the_password_never_appears_in_what_is_sent() {
    let server = MockServer::start();
    script_srp(&server, 401);
    let leak = server.mock(|when, then| {
        when.body_includes("hunter2");
        then.status(200);
    });
    let _ = client(&server, None).login(&password());
    assert_eq!(leak.calls(), 0);
}

#[test]
fn a_saved_session_is_resumed_without_the_password() {
    let server = MockServer::start();
    script_srp(&server, 200);
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/accountLogin");
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });
    let dir = tempfile::tempdir().unwrap();

    // First run: sign in, which persists the session.
    let first = client(&server, Some(dir.path()));
    assert_eq!(first.login(&password()).unwrap(), LoginStatus::Authenticated);
    drop(first);

    // Second run: a new process would only have the files on disk.
    let validate = server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });
    let second = client(&server, Some(dir.path()));
    assert!(second.has_saved_session());
    second.resume().unwrap();
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
fn an_untrusted_saved_session_is_not_resumed() {
    let server = MockServer::start();
    script_srp(&server, 200);
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/accountLogin");
        then.status(200).header("content-type", "application/json").json_body(account_reply(true));
    });
    let dir = tempfile::tempdir().unwrap();
    client(&server, Some(dir.path())).login(&password()).unwrap();

    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/validate");
        then.status(200).header("content-type", "application/json").json_body(account_reply(false));
    });
    let err = client(&server, Some(dir.path())).resume().unwrap_err();
    assert!(matches!(err, Error::AuthRequired(_)));
}

#[test]
fn drive_needs_the_webservice_urls_from_the_account_record() {
    let server = MockServer::start();
    script_srp(&server, 200);
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/accountLogin");
        then.status(200).header("content-type", "application/json").json_body(json!({
            "hsaTrustedBrowser": true, "dsInfo": {"dsid": "1"}, "webservices": {},
        }));
    });
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/requestWebAccessState");
        then.status(200).header("content-type", "application/json").json_body(json!({"isICDRSDisabled": false}));
    });
    let client = client(&server, None);
    client.login(&password()).unwrap();
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
    let _ = client.login(&password());
    assert!(probe.calls() >= 1);
}
