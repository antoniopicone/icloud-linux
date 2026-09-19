//! Sign-in on a background thread, for a UI that must not block.
//!
//! The UI sends [`AuthRequest`]s and polls [`AuthEvent`]s; nothing here knows
//! about a toolkit. Wrong verification codes are limited to three, because
//! each one counts towards Apple locking the account.

use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

use icloud_api::{AccountInfo, Client, Error as ApiError, LoginStatus, TrustedPhone, TwoFactorOptions};
use secrecy::SecretString;

pub const MAX_CODE_ATTEMPTS: u8 = 3;

/// The operations sign-in needs. [`Client`] implements it; tests and demos
/// supply their own.
pub trait Authenticator: Send + 'static {
    fn login(&mut self, password: &SecretString) -> icloud_api::Result<LoginStatus>;
    fn options(&self) -> TwoFactorOptions;
    fn send_sms(&mut self, phone: &TrustedPhone) -> icloud_api::Result<()>;
    fn verify_sms(&mut self, phone: &TrustedPhone, code: &str) -> icloud_api::Result<()>;
    fn verify_trusted_device(&mut self, code: &str) -> icloud_api::Result<()>;
    fn account(&self) -> AccountInfo;
}

impl Authenticator for Client {
    fn login(&mut self, password: &SecretString) -> icloud_api::Result<LoginStatus> {
        Client::login(self, password)
    }
    fn options(&self) -> TwoFactorOptions {
        self.two_factor_options()
    }
    fn send_sms(&mut self, phone: &TrustedPhone) -> icloud_api::Result<()> {
        Client::send_sms(self, phone)
    }
    fn verify_sms(&mut self, phone: &TrustedPhone, code: &str) -> icloud_api::Result<()> {
        Client::verify_sms(self, phone, code)
    }
    fn verify_trusted_device(&mut self, code: &str) -> icloud_api::Result<()> {
        Client::verify_trusted_device(self, code)
    }
    fn account(&self) -> AccountInfo {
        self.account_info()
    }
}

/// Where a verification code came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    TrustedDevice,
    /// SMS to the phone at this index of [`TwoFactorOptions::phones`].
    Sms(usize),
}

#[derive(Debug)]
pub enum AuthRequest {
    Login(SecretString),
    SendSms { phone: usize },
    Verify { channel: Channel, code: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthEvent {
    Authenticated(AccountInfo),
    /// The password was accepted; a verification code is still needed.
    CodeNeeded(TwoFactorOptions),
    SmsSent {
        to: String,
    },
    CodeRejected {
        attempts_left: u8,
    },
    /// Too many wrong codes: stop, or the account would be locked.
    OutOfAttempts,
    /// Something went wrong that the user should read.
    Failed(String),
}

#[derive(Debug)]
pub struct AuthWorker {
    requests: Sender<AuthRequest>,
    events: Receiver<AuthEvent>,
    thread: Option<JoinHandle<()>>,
}

impl AuthWorker {
    pub fn spawn(mut auth: Box<dyn Authenticator>) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<AuthRequest>();
        let (event_tx, event_rx) = mpsc::channel::<AuthEvent>();
        let thread = thread::Builder::new()
            .name("icloud-auth".into())
            .spawn(move || {
                let mut attempts_left = MAX_CODE_ATTEMPTS;
                for request in request_rx {
                    let events = handle(&mut *auth, request, &mut attempts_left);
                    for event in events {
                        if event_tx.send(event).is_err() {
                            return; // the UI is gone
                        }
                    }
                }
            })
            .ok();
        Self { requests: request_tx, events: event_rx, thread }
    }

    pub fn request(&self, request: AuthRequest) {
        // A closed channel means the worker ended; the UI then sees no events.
        let _ = self.requests.send(request);
    }

    /// The next event if one is ready.
    pub fn poll(&self) -> Option<AuthEvent> {
        self.events.try_recv().ok()
    }

    /// Block for the next event. For tests.
    pub fn wait(&self, timeout: std::time::Duration) -> Option<AuthEvent> {
        self.events.recv_timeout(timeout).ok()
    }
}

impl Drop for AuthWorker {
    fn drop(&mut self) {
        // Closing the request channel ends the thread; wait for it so a
        // request in flight does not outlive the window.
        let (dead, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.requests, dead));
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn describe(err: &ApiError) -> String {
    match err {
        ApiError::LoginFailed(msg) => msg.clone(),
        ApiError::Http(_) => "Could not reach Apple. Check your internet connection and try again.".into(),
        other => other.to_string(),
    }
}

fn handle(auth: &mut dyn Authenticator, request: AuthRequest, attempts_left: &mut u8) -> Vec<AuthEvent> {
    match request {
        AuthRequest::Login(password) => match auth.login(&password) {
            Ok(LoginStatus::Authenticated) => vec![AuthEvent::Authenticated(auth.account())],
            Ok(LoginStatus::TwoFactorRequired) => {
                let options = auth.options();
                if options.security_key_required && !options.can_use_sms() && !options.has_trusted_devices {
                    vec![AuthEvent::Failed(
                        "This account can only be verified with a hardware security key, which is not supported yet. \
                         Sign in once at icloud.com and import the browser trust token with `icloudctl auth --trust-token`."
                            .into(),
                    )]
                } else {
                    vec![AuthEvent::CodeNeeded(options)]
                }
            }
            Err(err) => vec![AuthEvent::Failed(describe(&err))],
        },
        AuthRequest::SendSms { phone } => {
            let options = auth.options();
            let Some(phone) = options.phones.get(phone) else {
                return vec![AuthEvent::Failed("No trusted phone number is available.".into())];
            };
            match auth.send_sms(phone) {
                Ok(()) => vec![AuthEvent::SmsSent {
                    to: phone.display.clone().unwrap_or_else(|| "your trusted number".into()),
                }],
                Err(err) => vec![AuthEvent::Failed(describe(&err))],
            }
        }
        AuthRequest::Verify { channel, code } => {
            if *attempts_left == 0 {
                return vec![AuthEvent::OutOfAttempts];
            }
            let outcome = match channel {
                Channel::TrustedDevice => auth.verify_trusted_device(&code),
                Channel::Sms(index) => match auth.options().phones.get(index) {
                    Some(phone) => auth.verify_sms(phone, &code),
                    None => Err(ApiError::NoTrustedPhone),
                },
            };
            match outcome {
                Ok(()) => vec![AuthEvent::Authenticated(auth.account())],
                Err(ApiError::WrongCode) => {
                    *attempts_left -= 1;
                    if *attempts_left == 0 {
                        vec![AuthEvent::OutOfAttempts]
                    } else {
                        vec![AuthEvent::CodeRejected { attempts_left: *attempts_left }]
                    }
                }
                // Not a verdict on the code: it does not use up an attempt.
                Err(err) => vec![AuthEvent::Failed(describe(&err))],
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use serde_json::json;

    use super::*;

    /// A scripted authenticator that records what it was asked to do.
    pub(crate) struct Fake {
        pub(crate) login: VecDeque<icloud_api::Result<LoginStatus>>,
        pub(crate) verify: VecDeque<icloud_api::Result<()>>,
        pub(crate) options: TwoFactorOptions,
        pub(crate) log: Arc<Mutex<Vec<String>>>,
    }

    pub(crate) fn phones() -> Vec<TrustedPhone> {
        vec![
            TrustedPhone {
                id: json!(1),
                non_fteu: Some(true),
                push_mode: Some("sms".into()),
                display: Some("+39 ••• 12".into()),
            },
            TrustedPhone { id: json!(2), non_fteu: None, push_mode: None, display: None },
        ]
    }

    impl Fake {
        pub(crate) fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            let fake = Self {
                login: VecDeque::new(),
                verify: VecDeque::new(),
                options: TwoFactorOptions { has_trusted_devices: true, phones: phones(), security_key_required: false },
                log: log.clone(),
            };
            (fake, log)
        }
    }

    impl Authenticator for Fake {
        fn login(&mut self, _: &SecretString) -> icloud_api::Result<LoginStatus> {
            self.log.lock().unwrap().push("login".into());
            self.login.pop_front().unwrap_or(Ok(LoginStatus::Authenticated))
        }
        fn options(&self) -> TwoFactorOptions {
            self.options.clone()
        }
        fn send_sms(&mut self, phone: &TrustedPhone) -> icloud_api::Result<()> {
            self.log.lock().unwrap().push(format!("send_sms {}", phone.id));
            Ok(())
        }
        fn verify_sms(&mut self, phone: &TrustedPhone, code: &str) -> icloud_api::Result<()> {
            self.log.lock().unwrap().push(format!("verify_sms {} {code}", phone.id));
            self.verify.pop_front().unwrap_or(Ok(()))
        }
        fn verify_trusted_device(&mut self, code: &str) -> icloud_api::Result<()> {
            self.log.lock().unwrap().push(format!("verify_device {code}"));
            self.verify.pop_front().unwrap_or(Ok(()))
        }
        fn account(&self) -> AccountInfo {
            AccountInfo { apple_id: Some("me@example.com".into()), full_name: None }
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    fn pw() -> SecretString {
        SecretString::from("pw".to_owned())
    }

    fn worker(fake: Fake) -> AuthWorker {
        AuthWorker::spawn(Box::new(fake))
    }

    #[test]
    fn a_trusted_account_authenticates_straight_away() {
        let (fake, _) = Fake::new();
        let w = worker(fake);
        w.request(AuthRequest::Login(pw()));
        assert_eq!(
            w.wait(WAIT),
            Some(AuthEvent::Authenticated(AccountInfo { apple_id: Some("me@example.com".into()), full_name: None }))
        );
    }

    #[test]
    fn two_factor_reports_the_available_options() {
        let (mut fake, _) = Fake::new();
        fake.login.push_back(Ok(LoginStatus::TwoFactorRequired));
        let w = worker(fake);
        w.request(AuthRequest::Login(pw()));
        match w.wait(WAIT) {
            Some(AuthEvent::CodeNeeded(options)) => {
                assert!(options.has_trusted_devices);
                assert_eq!(options.phones.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_rejected_password_is_explained_in_plain_words() {
        let (mut fake, _) = Fake::new();
        fake.login.push_back(Err(ApiError::LoginFailed("Invalid email/password combination.".into())));
        let w = worker(fake);
        w.request(AuthRequest::Login(pw()));
        assert_eq!(w.wait(WAIT), Some(AuthEvent::Failed("Invalid email/password combination.".into())));
    }

    #[test]
    fn a_security_key_only_account_is_told_what_to_do_instead() {
        let (mut fake, _) = Fake::new();
        fake.login.push_back(Ok(LoginStatus::TwoFactorRequired));
        fake.options = TwoFactorOptions { has_trusted_devices: false, phones: vec![], security_key_required: true };
        let w = worker(fake);
        w.request(AuthRequest::Login(pw()));
        match w.wait(WAIT) {
            Some(AuthEvent::Failed(msg)) => assert!(msg.contains("security key") && msg.contains("--trust-token")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sms_goes_to_the_chosen_number_and_the_code_is_checked_against_it() {
        let (fake, log) = Fake::new();
        let w = worker(fake);
        w.request(AuthRequest::SendSms { phone: 1 });
        assert_eq!(w.wait(WAIT), Some(AuthEvent::SmsSent { to: "your trusted number".into() }));
        w.request(AuthRequest::SendSms { phone: 0 });
        assert_eq!(w.wait(WAIT), Some(AuthEvent::SmsSent { to: "+39 ••• 12".into() }));
        w.request(AuthRequest::Verify { channel: Channel::Sms(0), code: "123456".into() });
        assert!(matches!(w.wait(WAIT), Some(AuthEvent::Authenticated(_))));
        assert_eq!(*log.lock().unwrap(), ["send_sms 2", "send_sms 1", "verify_sms 1 123456"]);
    }

    #[test]
    fn asking_for_a_missing_phone_fails_politely() {
        let (fake, log) = Fake::new();
        let w = worker(fake);
        w.request(AuthRequest::SendSms { phone: 9 });
        assert!(matches!(w.wait(WAIT), Some(AuthEvent::Failed(_))));
        w.request(AuthRequest::Verify { channel: Channel::Sms(9), code: "123456".into() });
        assert!(matches!(w.wait(WAIT), Some(AuthEvent::Failed(_))));
        assert!(log.lock().unwrap().is_empty(), "nothing must be sent to Apple");
    }

    #[test]
    fn three_wrong_codes_lock_out_further_attempts_without_contacting_apple() {
        let (mut fake, log) = Fake::new();
        for _ in 0..3 {
            fake.verify.push_back(Err(ApiError::WrongCode));
        }
        let w = worker(fake);
        let mut seen = Vec::new();
        for code in ["111111", "222222", "333333", "444444"] {
            w.request(AuthRequest::Verify { channel: Channel::TrustedDevice, code: code.into() });
            seen.push(w.wait(WAIT).unwrap());
        }
        assert_eq!(
            seen,
            [
                AuthEvent::CodeRejected { attempts_left: 2 },
                AuthEvent::CodeRejected { attempts_left: 1 },
                AuthEvent::OutOfAttempts,
                AuthEvent::OutOfAttempts,
            ]
        );
        assert_eq!(log.lock().unwrap().len(), 3, "the fourth code was never sent");
    }

    #[test]
    fn a_network_failure_does_not_use_up_an_attempt() {
        let (mut fake, _) = Fake::new();
        fake.verify.push_back(Err(ApiError::Protocol("boom".into())));
        fake.verify.push_back(Err(ApiError::WrongCode));
        let w = worker(fake);
        w.request(AuthRequest::Verify { channel: Channel::TrustedDevice, code: "111111".into() });
        assert!(matches!(w.wait(WAIT), Some(AuthEvent::Failed(_))));
        w.request(AuthRequest::Verify { channel: Channel::TrustedDevice, code: "222222".into() });
        assert_eq!(w.wait(WAIT), Some(AuthEvent::CodeRejected { attempts_left: 2 }));
    }

    #[test]
    fn dropping_the_worker_joins_its_thread() {
        let (fake, _) = Fake::new();
        let w = worker(fake);
        w.request(AuthRequest::Login(pw()));
        drop(w); // must not hang or leak
    }

    #[test]
    fn events_are_polled_without_blocking() {
        let (fake, _) = Fake::new();
        let w = worker(fake);
        assert_eq!(w.poll(), None);
    }
}
