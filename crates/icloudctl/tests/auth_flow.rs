//! The interactive sign-in, scripted, against a stand-in for Apple's servers.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use httpmock::prelude::*;
use icloud_api::{Client, ClientConfig, Endpoints};
use icloud_core::{Config, Layout};
use icloudctl::{
    auth::{AuthOptions, run},
    prompt::Prompter,
};
use secrecy::SecretString;
use serde_json::json;

#[derive(Default)]
struct Script {
    answers: std::collections::VecDeque<String>,
    said: Vec<String>,
    asked: Vec<String>,
}

impl Script {
    fn new(answers: &[&str]) -> Self {
        Self { answers: answers.iter().map(|s| (*s).to_owned()).collect(), ..Self::default() }
    }
    fn said(&self, needle: &str) -> bool {
        self.said.iter().any(|s| s.contains(needle))
    }
}

impl Prompter for Script {
    fn line(&mut self, text: &str) -> std::io::Result<String> {
        self.asked.push(text.to_owned());
        self.answers.pop_front().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "script ran out"))
    }
    fn secret(&mut self, text: &str) -> std::io::Result<String> {
        self.line(text)
    }
    fn say(&mut self, text: &str) {
        self.said.push(text.to_owned());
    }
}

const PAGE_WITH_PHONES: &str = r#"<html><script class="boot_args">
{"direct":{"hasTrustedDevices":true,"twoSV":{"phoneNumberVerification":{"trustedPhoneNumbers":[
{"id":1,"pushMode":"sms","numberWithDialCode":"+39 ••• 12"},{"id":2,"pushMode":"sms","numberWithDialCode":"+1 ••• 99"}]}}}}
</script></html>"#;

fn client(server: &MockServer) -> Client {
    Client::new(ClientConfig {
        account: "user@example.com".into(),
        session_dir: None,
        endpoints: Endpoints {
            idmsa: server.base_url(),
            home: server.base_url(),
            setup: server.url("/setup/ws/1"),
            detect_partition: false,
        },
    })
    .unwrap()
}

fn config() -> Config {
    Config::for_layout(&Layout::under(std::path::Path::new("/tmp/x")))
}

fn script_password_step(server: &MockServer, complete_status: u16) {
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth/authorize/signin");
        then.status(200).header("scnt", "s").header("X-Apple-ID-Session-Id", "i");
    });
    server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/signin/init");
        then.status(200).header("content-type", "application/json").json_body(json!({
            "salt": B64.encode([1u8; 16]), "b": B64.encode([0x42u8; 256]), "c": "c",
            "iteration": 100, "protocol": "s2k",
        }));
    });
    server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/signin/complete");
        then.status(complete_status)
            .header("content-type", "application/json")
            .header("X-Apple-Session-Token", "tok")
            .header("Set-Cookie", "X-APPLE-WEBAUTH-TOKEN=v; Path=/")
            .json_body(json!({"authType": "hsa2"}));
    });
}

fn script_trust_and_account(server: &MockServer) {
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth/2sv/trust");
        then.status(200);
    });
    server.mock(|when, then| {
        when.method(POST).path("/setup/ws/1/accountLogin");
        then.status(200).header("content-type", "application/json").json_body(json!({
            "hsaTrustedBrowser": true, "dsInfo": {"dsid": "1", "appleId": "user@example.com"}, "webservices": {},
        }));
    });
}

#[test]
fn a_trusted_account_signs_in_after_asking_for_the_password_only() {
    let server = MockServer::start();
    script_password_step(&server, 200);
    script_trust_and_account(&server);
    let mut ui = Script::new(&["hunter2"]);
    let info = run(&client(&server), &config(), &AuthOptions::default(), &mut ui).unwrap();
    assert_eq!(info.apple_id.as_deref(), Some("user@example.com"));
    assert!(ui.said("AUTH_OK"));
    assert_eq!(ui.asked, ["Apple ID password (input hidden): "]);
}

#[test]
fn a_stored_password_is_used_without_asking() {
    let server = MockServer::start();
    script_password_step(&server, 200);
    script_trust_and_account(&server);
    let mut config = config();
    config.password = Some(SecretString::from("stored".to_owned()));
    let mut ui = Script::new(&[]);
    run(&client(&server), &config, &AuthOptions::default(), &mut ui).unwrap();
    assert!(ui.asked.is_empty());
}

#[test]
fn two_factor_with_a_code_from_a_trusted_device() {
    let server = MockServer::start();
    script_password_step(&server, 409);
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth");
        then.status(200).header("content-type", "text/html").body(PAGE_WITH_PHONES);
    });
    let verify = server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/verify/trusteddevice/securitycode").body_includes(r#""code":"654321""#);
        then.status(204);
    });
    script_trust_and_account(&server);

    let mut ui = Script::new(&["hunter2", "654321"]);
    run(&client(&server), &config(), &AuthOptions::default(), &mut ui).unwrap();
    verify.assert();
    assert!(ui.said("trusted Apple devices"));
    assert!(ui.said("Type `sms`"), "the SMS escape hatch is offered");
}

#[test]
fn typing_sms_switches_to_a_text_message_and_asks_which_number() {
    let server = MockServer::start();
    script_password_step(&server, 409);
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth");
        then.status(200).header("content-type", "text/html").body(PAGE_WITH_PHONES);
    });
    let send = server.mock(|when, then| {
        when.method(PUT)
            .path("/appleauth/auth/verify/phone")
            .body_includes(r#""id":2"#)
            .body_includes(r#""mode":"sms""#);
        then.status(200);
    });
    let verify = server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/verify/phone/securitycode").body_includes(r#""code":"111222""#);
        then.status(200);
    });
    script_trust_and_account(&server);

    let mut ui = Script::new(&["hunter2", "sms", "2", "111222"]);
    run(&client(&server), &config(), &AuthOptions::default(), &mut ui).unwrap();
    send.assert();
    verify.assert();
    assert!(ui.said("+1 ••• 99"), "the chosen number is confirmed to the user");
}

#[test]
fn force_sms_goes_straight_to_the_text_message() {
    let server = MockServer::start();
    script_password_step(&server, 409);
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth");
        then.status(200).header("content-type", "text/html").body(
            r#"<script class="boot_args">{"direct":{"twoSV":{"phoneNumberVerification":{"trustedPhoneNumber":{"id":5,"pushMode":"sms"}}}}}</script>"#,
        );
    });
    let send = server.mock(|when, then| {
        when.method(PUT).path("/appleauth/auth/verify/phone");
        then.status(200);
    });
    server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/verify/phone/securitycode");
        then.status(200);
    });
    script_trust_and_account(&server);
    let mut ui = Script::new(&["hunter2", "123456"]);
    run(&client(&server), &config(), &AuthOptions { force_sms: true, ..AuthOptions::default() }, &mut ui).unwrap();
    send.assert();
}

#[test]
fn three_wrong_codes_stop_the_flow_so_the_account_is_not_locked() {
    let server = MockServer::start();
    script_password_step(&server, 409);
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth");
        then.status(200).header("content-type", "text/html").body(PAGE_WITH_PHONES);
    });
    let verify = server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/verify/trusteddevice/securitycode");
        then.status(400).header("content-type", "application/json").json_body(json!({"service_errors": []}));
    });
    let mut ui = Script::new(&["hunter2", "111111", "222222", "333333", "444444"]);
    let err = run(&client(&server), &config(), &AuthOptions::default(), &mut ui).unwrap_err();
    assert!(err.to_string().contains("not accepted"));
    assert_eq!(verify.calls(), 3, "no fourth attempt");
    assert_eq!(ui.answers.len(), 1, "the fourth code was never even asked for");
}

#[test]
fn a_wrong_password_is_reported_and_asks_nothing_more() {
    let server = MockServer::start();
    script_password_step(&server, 401);
    let mut ui = Script::new(&["wrong"]);
    let err = run(&client(&server), &config(), &AuthOptions::default(), &mut ui).unwrap_err();
    assert!(err.to_string().contains("Invalid email/password"), "{err}");
    assert!(!ui.said("AUTH_OK"));
}

#[test]
fn an_account_needing_a_security_key_gets_a_clear_explanation() {
    let server = MockServer::start();
    script_password_step(&server, 409);
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({"fsaChallenge": {"challenge": "x"}}));
    });
    let err = run(&client(&server), &config(), &AuthOptions::default(), &mut Script::new(&["pw"])).unwrap_err();
    assert!(err.to_string().contains("security key") && err.to_string().contains("--trust-token"), "{err}");
}

#[test]
fn a_browser_trust_token_is_sent_with_the_password_proof() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/appleauth/auth/authorize/signin");
        then.status(200);
    });
    server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/signin/init");
        then.status(200).header("content-type", "application/json").json_body(json!({
            "salt": B64.encode([1u8; 16]), "b": B64.encode([0x42u8; 256]), "c": "c", "iteration": 100, "protocol": "s2k_fo",
        }));
    });
    let complete = server.mock(|when, then| {
        when.method(POST).path("/appleauth/auth/signin/complete").body_includes(r#""trustTokens":["browser-token"]"#);
        then.status(200).header("X-Apple-Session-Token", "tok").header("Set-Cookie", "X-APPLE-WEBAUTH-TOKEN=v; Path=/");
    });
    script_trust_and_account(&server);
    let opts = AuthOptions { trust_token: Some("browser-token".into()), ..AuthOptions::default() };
    run(&client(&server), &config(), &opts, &mut Script::new(&["pw"])).unwrap();
    complete.assert();
}
