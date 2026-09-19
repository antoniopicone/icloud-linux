//! A pretend system, for `--demo`: try the whole wizard without an Apple
//! account and without changing anything on the machine.

use std::{collections::VecDeque, path::Path, thread, time::Duration};

use icloud_api::{AccountInfo, Error as ApiError, LoginStatus, TrustedPhone, TwoFactorOptions};
use icloud_core::{
    Result,
    installer::{Authenticator, Backend, InstallPlan, Prepared, StepOutcome},
    setup::{Check, Severity},
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;

/// The code the demo accepts.
pub(crate) const DEMO_CODE: &str = "123456";

#[derive(Debug, Default)]
pub(crate) struct DemoBackend;

struct DemoAuth {
    options: TwoFactorOptions,
    pending: VecDeque<()>,
}

impl Authenticator for DemoAuth {
    fn login(&mut self, password: &SecretString) -> icloud_api::Result<LoginStatus> {
        thread::sleep(Duration::from_millis(900));
        match password.expose_secret() {
            "wrong" => Err(ApiError::LoginFailed("Invalid email/password combination.".into())),
            "trusted" => Ok(LoginStatus::Authenticated),
            _ => Ok(LoginStatus::TwoFactorRequired),
        }
    }

    fn options(&self) -> TwoFactorOptions {
        self.options.clone()
    }

    fn send_sms(&mut self, _: &TrustedPhone) -> icloud_api::Result<()> {
        thread::sleep(Duration::from_millis(500));
        self.pending.push_back(());
        Ok(())
    }

    fn verify_sms(&mut self, _: &TrustedPhone, code: &str) -> icloud_api::Result<()> {
        self.verify_trusted_device(code)
    }

    fn verify_trusted_device(&mut self, code: &str) -> icloud_api::Result<()> {
        thread::sleep(Duration::from_millis(600));
        if code.chars().filter(char::is_ascii_digit).collect::<String>() == DEMO_CODE {
            Ok(())
        } else {
            Err(ApiError::WrongCode)
        }
    }

    fn account(&self) -> AccountInfo {
        AccountInfo { apple_id: Some("demo@icloud.com".into()), full_name: Some("Demo User".into()) }
    }
}

impl Backend for DemoBackend {
    fn preflight(&self) -> Vec<Check> {
        vec![
            Check { severity: Severity::Ok, message: "/dev/fuse is present".into(), fix: None },
            Check { severity: Severity::Ok, message: "/usr/bin/fusermount3 found".into(), fix: None },
            Check { severity: Severity::Ok, message: "daemon: /usr/local/bin/icloudd".into(), fix: None },
        ]
    }

    fn prepare(&self, plan: &InstallPlan) -> Result<Prepared> {
        thread::sleep(Duration::from_millis(400));
        Ok(Prepared { mount_dir: plan.mount_dir.clone(), used_fallback: false })
    }

    fn save_account(&self, _: &str, _: Option<SecretString>) -> Result<()> {
        Ok(())
    }

    fn authenticator(&self) -> Result<Box<dyn Authenticator>> {
        let phones = vec![
            TrustedPhone {
                id: json!(1),
                non_fteu: Some(true),
                push_mode: Some("sms".into()),
                display: Some("+39 ••• ••• ••12".into()),
            },
            TrustedPhone {
                id: json!(2),
                non_fteu: None,
                push_mode: Some("sms".into()),
                display: Some("+1 ••• ••• ••99".into()),
            },
        ];
        Ok(Box::new(DemoAuth {
            options: TwoFactorOptions { has_trusted_devices: true, phones, security_key_required: false },
            pending: VecDeque::new(),
        }))
    }

    fn finish(&self, plan: &InstallPlan, mount_dir: &Path) -> Vec<StepOutcome> {
        let step = |title: &str, detail: String| {
            thread::sleep(Duration::from_millis(700));
            StepOutcome { title: title.to_owned(), result: Ok(detail) }
        };
        let mut steps = vec![
            step("Start the iCloud service", "running".into()),
            step("Mount iCloud Drive", mount_dir.display().to_string()),
        ];
        if plan.keep_indexer_out {
            steps.push(step("Keep the search indexer out", "done".into()));
        }
        if plan.show_sidebar_status {
            steps.push(step("Show activity in the Files sidebar", "appears next to iCloud".into()));
        }
        steps
    }
}
