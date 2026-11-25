//! Blocking client for the parts of iCloud that `icloud-linux` needs.
//!
//! * [`Session`]: cookies, persistence, error normalisation.
//! * [`Client`]: take over a sign-in made on icloud.com, keep it alive, and
//!   hand out services. No password ever passes through this crate.
//! * [`drive`]: the [`Drive`] trait and its HTTP implementation.
//! * [`RenewingDrive`]: renews a lapsed web session and repeats the request.
//!
//! Notes, Photos and the like would arrive as further service modules next to
//! `drive`, obtained from the same [`Client`].

pub mod client;
pub mod drive;
pub mod error;
pub mod renew;
pub mod session;

#[cfg(any(test, feature = "testing"))]
pub mod memory;

pub use client::{AccountInfo, Client, ClientConfig, Endpoints};
pub use drive::{DeleteMode, Drive, HttpDrive, Node, NodeKind};
pub use error::{Error, Result};
pub use renew::RenewingDrive;
pub use session::{BrowserCookie, Session};

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
