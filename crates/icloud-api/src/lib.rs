//! Blocking client for the parts of iCloud that `icloud-linux` needs.
//!
//! * [`srp`]: Apple's flavour of SRP-6a and the password stretching it needs.
//! * [`Session`]: cookies, Apple's session headers, error normalisation.
//! * [`Client`]: sign in, two-factor, trust, and hand out services.
//! * [`drive`]: the [`Drive`] trait and its HTTP implementation.
//! * [`cloudkit`]: the `CloudKit` database client behind Reminders (and, later,
//!   Notes and Photos).
//! * [`reminders`]: iCloud Reminders: lists, reminders, incremental changes
//!   and writes.
//!
//! Every service is obtained from the same [`Client`], so one sign-in serves
//! them all.

pub mod client;
pub mod cloudkit;
pub mod drive;
pub mod error;
pub mod reminders;
pub mod session;
pub mod srp;
pub mod twofactor;

#[cfg(any(test, feature = "testing"))]
pub mod memory;

pub use client::{AccountInfo, Client, ClientConfig, Endpoints, LoginStatus};
pub use drive::{DeleteMode, Drive, HttpDrive, Node, NodeKind};
pub use error::{Error, Result};
pub use reminders::RemindersService;
pub use session::Session;
pub use twofactor::{CodeMethod, TrustedPhone, TwoFactorOptions};

/// A random (version 4) UUID in its canonical lower-case form.
pub(crate) fn uuid_v4() -> Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};

    let mut b = [0u8; 16];
    SystemRandom::new().fill(&mut b).map_err(|_| Error::Protocol("system random number generator failed".into()))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex = b.iter().fold(String::with_capacity(32), |mut acc, x| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{x:02x}");
        acc
    });
    Ok(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]))
}

#[cfg(test)]
mod tests {
    use super::uuid_v4;

    #[test]
    fn uuids_have_the_canonical_shape_and_differ() {
        let a = uuid_v4().unwrap();
        let b = uuid_v4().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4', "version nibble");
        assert!(matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "variant bits");
    }
}
