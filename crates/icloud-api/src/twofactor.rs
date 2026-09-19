//! What Apple tells us about how a two-factor challenge can be answered.
//!
//! When the password is right but the browser is not trusted, Apple answers
//! `409` and serves an HTML shell whose `<script class="boot_args">` element
//! carries the challenge as JSON. This module extracts the parts the client
//! can act on: whether a trusted device is around and which phone numbers can
//! receive an SMS.
//!
//! # Not supported
//!
//! Apple's newer "bridge" flow, which pushes the code to a trusted device over
//! a websocket and runs a SPAKE2+ proof, and FIDO2 security keys. Accounts
//! that can only use those cannot sign in here; SMS works for everyone with a
//! trusted phone number, and a browser trust token can be imported instead.

use serde_json::Value;

use crate::error::{Error, Result};

/// How a verification code reaches the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeMethod {
    /// A code displayed on one of the user's trusted Apple devices.
    TrustedDevice,
    /// A code sent by SMS to a trusted phone number.
    Sms,
}

/// A phone number Apple can send a code to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPhone {
    /// Apple's identifier for the number; needed in every SMS request.
    pub id: Value,
    pub non_fteu: Option<bool>,
    /// Delivery mode Apple prefers for this number, usually `sms` or `push`.
    pub push_mode: Option<String>,
    /// Masked number for display, when Apple provides one.
    pub display: Option<String>,
}

impl TrustedPhone {
    fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let id = obj.get("id").filter(|v| v.is_i64() || v.is_u64() || v.is_string())?.clone();
        let text = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            id,
            non_fteu: obj.get("nonFTEU").and_then(Value::as_bool),
            push_mode: text("pushMode"),
            display: text("numberWithDialCode").or_else(|| text("obfuscatedNumber")),
        })
    }

    /// The nested `phoneNumber` object Apple's SMS endpoints expect.
    pub(crate) fn payload(&self) -> Value {
        let mut payload = serde_json::json!({ "id": self.id });
        if let Some(non_fteu) = self.non_fteu {
            payload["nonFTEU"] = Value::Bool(non_fteu);
        }
        payload
    }
}

/// The ways a pending challenge can be answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TwoFactorOptions {
    /// A trusted Apple device is available to show a code.
    pub has_trusted_devices: bool,
    pub phones: Vec<TrustedPhone>,
    /// Apple demands a hardware security key, which this client cannot use.
    pub security_key_required: bool,
}

impl TwoFactorOptions {
    /// The method to offer first.
    pub fn preferred_method(&self) -> CodeMethod {
        if self.has_trusted_devices || self.phones.is_empty() { CodeMethod::TrustedDevice } else { CodeMethod::Sms }
    }

    pub fn can_use_sms(&self) -> bool {
        !self.phones.is_empty()
    }

    /// Read the options out of the JSON found in Apple's boot data (or in a
    /// JSON auth-options reply, which has the same content at a shallower
    /// nesting).
    pub fn from_boot_json(root: &Value) -> Self {
        let direct = root.get("direct").unwrap_or(root);
        let two_sv = direct.get("twoSV").unwrap_or(direct);
        let bridge = two_sv.get("bridgeInitiateData");

        // The phone list lives in different places depending on which shape of
        // the reply we got. Look everywhere, keep first occurrence of each id.
        let candidates = [
            two_sv.get("phoneNumberVerification"),
            bridge.and_then(|b| b.get("phoneNumberVerification")),
            root.get("phoneNumberVerification"),
        ];
        let mut phones: Vec<TrustedPhone> = Vec::new();
        let mut add = |phone: TrustedPhone| {
            if !phones.iter().any(|p| p.id == phone.id) {
                phones.push(phone);
            }
        };
        for verification in candidates.into_iter().flatten() {
            if let Some(phone) = verification.get("trustedPhoneNumber").and_then(TrustedPhone::from_value) {
                add(phone);
            }
            if let Some(list) = verification.get("trustedPhoneNumbers").and_then(Value::as_array) {
                list.iter().filter_map(TrustedPhone::from_value).for_each(&mut add);
            }
        }
        if let Some(phone) = root.get("trustedPhoneNumber").and_then(TrustedPhone::from_value) {
            add(phone);
        }

        Self {
            has_trusted_devices: direct.get("hasTrustedDevices").and_then(Value::as_bool).unwrap_or(false)
                || root.get("hasTrustedDevices").and_then(Value::as_bool).unwrap_or(false),
            phones,
            security_key_required: root.get("fsaChallenge").is_some_and(|v| !v.is_null())
                || root.get("keyNames").is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty())),
        }
    }
}

/// Extract the JSON inside `<script class="boot_args">…</script>`.
pub fn parse_boot_args(html: &str) -> Result<Value> {
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after = &rest[start..];
        let Some(tag_end) = after.find('>') else { break };
        let tag = &after[..tag_end];
        let body = &after[tag_end + 1..];
        let Some(close) = body.find("</script") else { break };
        if has_class(tag, "boot_args") {
            return serde_json::from_str(body[..close].trim())
                .map_err(|e| Error::Protocol(format!("malformed HSA2 boot data: {e}")));
        }
        rest = &body[close..];
    }
    Err(Error::Protocol("the sign-in page carries no boot data".into()))
}

/// Does the opening tag `tag` have `class` containing the word `wanted`?
fn has_class(tag: &str, wanted: &str) -> bool {
    let Some(pos) = tag.find("class=") else { return false };
    let value = &tag[pos + "class=".len()..];
    let Some(quote) = value.chars().next().filter(|c| *c == '"' || *c == '\'') else { return false };
    let value = &value[1..];
    value.find(quote).is_some_and(|end| value[..end].split_whitespace().any(|c| c == wanted))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html><html><head>
<script type="text/javascript" class="other">var x = 1;</script>
<script type="application/json" class="boot_args">
{"direct":{"authInitialRoute":"auth/bridge/step","hasTrustedDevices":true,
"twoSV":{"authFactors":["sms"],"bridgeInitiateData":{"phoneNumberVerification":{
"trustedPhoneNumber":{"id":1,"nonFTEU":true,"pushMode":"sms","numberWithDialCode":"+39 ••• ••• ••12"},
"trustedPhoneNumbers":[{"id":1,"pushMode":"sms"},{"id":2,"obfuscatedNumber":"•• 99"}]}}}}}
</script></head><body></body></html>"#;

    #[test]
    fn boot_args_are_found_among_other_scripts() {
        let json = parse_boot_args(PAGE).unwrap();
        assert_eq!(json["direct"]["authInitialRoute"], "auth/bridge/step");
    }

    #[test]
    fn missing_or_broken_boot_args_are_reported() {
        assert!(parse_boot_args("<html></html>").is_err());
        assert!(parse_boot_args(r#"<script class="boot_args">{oops</script>"#).is_err());
        assert!(parse_boot_args(r#"<script class="boot_args_not">{}</script>"#).is_err());
    }

    #[test]
    fn options_list_each_phone_once_with_its_details() {
        let options = TwoFactorOptions::from_boot_json(&parse_boot_args(PAGE).unwrap());
        assert!(options.has_trusted_devices);
        assert_eq!(options.phones.len(), 2, "id 1 appears twice and must be deduplicated");
        assert_eq!(options.phones[0].display.as_deref(), Some("+39 ••• ••• ••12"));
        assert_eq!(options.phones[0].non_fteu, Some(true));
        assert_eq!(options.phones[1].display.as_deref(), Some("•• 99"));
        assert_eq!(options.preferred_method(), CodeMethod::TrustedDevice);
        assert!(options.can_use_sms());
    }

    #[test]
    fn sms_is_preferred_when_no_device_is_available() {
        let options = TwoFactorOptions::from_boot_json(&json!({
            "trustedPhoneNumber": {"id": "abc", "pushMode": "sms"},
            "mode": "sms",
        }));
        assert!(!options.has_trusted_devices);
        assert_eq!(options.preferred_method(), CodeMethod::Sms);
        assert_eq!(options.phones[0].id, json!("abc"));
    }

    #[test]
    fn security_keys_are_flagged() {
        let options = TwoFactorOptions::from_boot_json(&json!({"fsaChallenge": {"challenge": "x"}}));
        assert!(options.security_key_required);
        let options = TwoFactorOptions::from_boot_json(&json!({"keyNames": ["YubiKey"]}));
        assert!(options.security_key_required);
        assert!(!TwoFactorOptions::from_boot_json(&json!({})).security_key_required);
    }

    #[test]
    fn phone_payload_omits_unknown_fields() {
        let phone = TrustedPhone { id: json!(7), non_fteu: None, push_mode: None, display: None };
        assert_eq!(phone.payload(), json!({"id": 7}));
        let phone = TrustedPhone { non_fteu: Some(true), ..phone };
        assert_eq!(phone.payload(), json!({"id": 7, "nonFTEU": true}));
    }

    #[test]
    fn phones_without_a_usable_id_are_skipped() {
        let options = TwoFactorOptions::from_boot_json(&json!({
            "phoneNumberVerification": {"trustedPhoneNumbers": [{"id": null}, {"pushMode": "sms"}]},
        }));
        assert!(options.phones.is_empty());
    }
}
