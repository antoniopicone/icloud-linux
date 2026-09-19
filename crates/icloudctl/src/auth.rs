//! `icloudctl auth`: the one-time interactive sign-in, including two-factor.

use icloud_api::{AccountInfo, Client, CodeMethod, Error as ApiError, LoginStatus, TrustedPhone, TwoFactorOptions};
use icloud_core::{Config, Error, Result};
use secrecy::SecretString;

use crate::prompt::Prompter;

/// A wrong code counts towards Apple's lockout as much as a wrong password, so
/// only a few attempts are made before stopping.
const MAX_CODE_ATTEMPTS: usize = 3;

#[derive(Debug, Default, Clone)]
pub struct AuthOptions {
    /// Ask for the code by SMS instead of waiting for a trusted device.
    pub force_sms: bool,
    /// A `X-APPLE-WEBAUTH-HSA-TRUST` value taken from a signed-in browser.
    pub trust_token: Option<String>,
    /// Print what Apple reported about the challenge.
    pub debug: bool,
}

pub fn run(client: &Client, config: &Config, options: &AuthOptions, ui: &mut dyn Prompter) -> Result<AccountInfo> {
    let password = match &config.password {
        Some(stored) => stored.clone(),
        None => SecretString::from(ui.secret("Apple ID password (input hidden): ")?),
    };
    if let Some(token) = &options.trust_token {
        ui.say("Using the trust token from your browser.");
        client.set_trust_token(token);
    }

    ui.say(&format!("Signing in as {}…", client.account()));
    match client.login(&password)? {
        LoginStatus::Authenticated => {}
        LoginStatus::TwoFactorRequired => two_factor(client, options, ui)?,
    }

    let info = client.account_info();
    ui.say("");
    ui.say("AUTH_OK");
    if let Some(id) = &info.apple_id {
        ui.say(&format!("Authenticated as: {id}"));
    }
    Ok(info)
}

fn two_factor(client: &Client, options: &AuthOptions, ui: &mut dyn Prompter) -> Result<()> {
    let choices = client.two_factor_options();
    if options.debug {
        ui.say(&format!("two-factor options: {choices:?}"));
    }
    ui.say("\nTwo-factor authentication is required.");

    if choices.security_key_required && !choices.can_use_sms() && !choices.has_trusted_devices {
        return Err(Error::Setup(
            "this account can only be verified with a hardware security key, which is not supported. \
             Import a browser trust token instead: `icloudctl auth --trust-token <value>`"
                .into(),
        ));
    }

    let mut method = if options.force_sms { CodeMethod::Sms } else { choices.preferred_method() };
    if method == CodeMethod::Sms && !choices.can_use_sms() {
        ui.say("No trusted phone number is available for SMS; falling back to the code on a trusted device.");
        method = CodeMethod::TrustedDevice;
    }

    let mut phone = None;
    if method == CodeMethod::Sms {
        phone = Some(send_sms(client, &choices, ui)?);
    } else {
        ui.say("Look at your trusted Apple devices for a verification code.");
        if choices.can_use_sms() {
            ui.say("No code arrived? Type `sms` at the prompt to have one texted to you.");
        }
    }

    for attempt in 1..=MAX_CODE_ATTEMPTS {
        let answer = ui.line("Verification code: ")?;
        if answer.eq_ignore_ascii_case("sms") && choices.can_use_sms() {
            phone = Some(send_sms(client, &choices, ui)?);
            continue;
        }
        let outcome = match &phone {
            Some(phone) => client.verify_sms(phone, &answer),
            None => client.verify_trusted_device(&answer),
        };
        match outcome {
            Ok(()) => {
                ui.say("Code accepted; this session is now trusted.");
                return Ok(());
            }
            Err(ApiError::WrongCode) if attempt < MAX_CODE_ATTEMPTS => {
                ui.say(&format!("That code was not accepted ({} attempt(s) left).", MAX_CODE_ATTEMPTS - attempt));
            }
            Err(ApiError::WrongCode) => {
                return Err(Error::Setup("the code was not accepted; stopping so the account is not locked".into()));
            }
            Err(other) => return Err(other.into()),
        }
    }
    Err(Error::Setup("authentication incomplete".into()))
}

/// Pick a phone (asking when there are several) and have a code texted to it.
fn send_sms(client: &Client, choices: &TwoFactorOptions, ui: &mut dyn Prompter) -> Result<TrustedPhone> {
    let phone = match choices.phones.as_slice() {
        [] => return Err(ApiError::NoTrustedPhone.into()),
        [only] => only.clone(),
        many => {
            ui.say("Send the code to which number?");
            for (index, phone) in many.iter().enumerate() {
                ui.say(&format!("  {}: {}", index + 1, phone.display.as_deref().unwrap_or("(number hidden)")));
            }
            let pick = ui.line("Number: ")?;
            let index = pick.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1));
            index.and_then(|i| many.get(i)).cloned().ok_or_else(|| Error::Setup("no such number".into()))?
        }
    };
    client.send_sms(&phone)?;
    ui.say(&format!("A code was sent by SMS to {}.", phone.display.as_deref().unwrap_or("your trusted number")));
    Ok(phone)
}
