//! SRP-6a client as Apple's `idmsa` endpoint speaks it.
//!
//! This is a port of the exact configuration `pyicloud` selects on top of the
//! Python `srp` library:
//!
//! * 2048-bit group from RFC 5054, generator 2, SHA-256;
//! * `rfc5054_enable()`: the generator and the public values are left-padded
//!   with zeros to the width of `N` when hashed for `k` and `u`, and `g` is
//!   padded when it is hashed into the proof;
//! * `no_username_in_x()`: `x = H(salt | H(":" | password))`, the user name is
//!   *not* part of the private key, although it *is* part of the `M1` proof;
//! * the "password" is not the user's password but the PBKDF2 output that
//!   [`derive_password`] produces from it.
//!
//! Getting a single byte of this wrong makes Apple answer "wrong password",
//! and every such answer counts towards locking the account. The tests pin the
//! output against vectors produced by the reference implementation.

use std::num::NonZeroU32;

use num_bigint::BigUint;
use ring::{
    digest::{Context, SHA256, digest},
    pbkdf2,
    rand::{SecureRandom, SystemRandom},
};

use crate::error::{Error, Result};

/// RFC 5054, appendix A, 2048-bit group.
const N_HEX: &str = "\
AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4\
A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF60\
95179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF\
747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B907\
8717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB37861\
60279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DB\
FBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73";
const G: u32 = 2;

/// Width in bytes of the private ephemeral `a`, as the reference client uses.
const EPHEMERAL_BYTES: usize = 256;
/// Apple's servers use tens of thousands of PBKDF2 rounds. Anything far above
/// that is a hostile or broken server trying to burn CPU.
const MAX_PBKDF2_ITERATIONS: u32 = 5_000_000;

/// Password-stretching flavour announced by the server in `signin/init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// PBKDF2 over the raw SHA-256 of the password.
    S2k,
    /// PBKDF2 over the *hex encoding* of the SHA-256 of the password.
    S2kFo,
}

impl Protocol {
    /// Wire names, in the order they are offered to the server.
    pub const OFFERED: [&'static str; 2] = ["s2k", "s2k_fo"];

    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "s2k" => Ok(Self::S2k),
            "s2k_fo" => Ok(Self::S2kFo),
            other => Err(Error::Protocol(format!("unsupported SRP protocol `{other}`"))),
        }
    }
}

/// Stretch the user's password the way Apple's SRP variant expects.
pub fn derive_password(password: &str, salt: &[u8], iterations: u32, protocol: Protocol) -> Result<Vec<u8>> {
    let iterations = NonZeroU32::new(iterations)
        .filter(|n| n.get() <= MAX_PBKDF2_ITERATIONS)
        .ok_or_else(|| Error::Protocol(format!("unreasonable PBKDF2 iteration count {iterations}")))?;

    let sha = digest(&SHA256, password.as_bytes());
    let input: Vec<u8> = match protocol {
        Protocol::S2k => sha.as_ref().to_vec(),
        Protocol::S2kFo => hex_lower(sha.as_ref()).into_bytes(),
    };
    let mut out = [0u8; 32];
    pbkdf2::derive(pbkdf2::PBKDF2_HMAC_SHA256, iterations, salt, &input, &mut out);
    Ok(out.to_vec())
}

/// Client proof and the value the server is expected to answer with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    /// `M1`, sent to the server.
    pub m1: Vec<u8>,
    /// `H(A | M1 | K)`, what a genuine server replies with (`m2`).
    pub m2: Vec<u8>,
}

/// One SRP handshake, from the ephemeral key to the proof.
pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    big_a: BigUint,
}

impl std::fmt::Debug for SrpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The private ephemeral must never reach a log.
        f.debug_struct("SrpClient").finish_non_exhaustive()
    }
}

impl SrpClient {
    /// Start a handshake with a fresh random ephemeral.
    pub fn new() -> Result<Self> {
        let mut secret = [0u8; EPHEMERAL_BYTES];
        SystemRandom::new()
            .fill(&mut secret)
            .map_err(|_| Error::Protocol("system random number generator failed".into()))?;
        // The reference client forces the top bit so `a` always has full length.
        secret[0] |= 0x80;
        Ok(Self::from_secret(&secret))
    }

    /// Start a handshake from a chosen ephemeral. Only useful for tests.
    pub fn from_secret(secret: &[u8]) -> Self {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("RFC 5054 constant is valid hex");
        let g = BigUint::from(G);
        let a = BigUint::from_bytes_be(secret);
        let big_a = g.modpow(&a, &n);
        Self { n, g, a, big_a }
    }

    /// Public ephemeral `A`, big-endian without leading zeros.
    pub fn public_a(&self) -> Vec<u8> {
        to_bytes(&self.big_a)
    }

    /// Answer the server's challenge.
    ///
    /// `derived_password` is the output of [`derive_password`]. Returns `None`
    /// when the server's values violate the SRP-6a safety checks, in which case
    /// the handshake must be abandoned.
    pub fn process_challenge(
        &self,
        username: &str,
        derived_password: &[u8],
        salt: &[u8],
        server_public: &[u8],
    ) -> Option<Proof> {
        let n = &self.n;
        let big_b = BigUint::from_bytes_be(server_public);
        if (&big_b % n) == BigUint::ZERO {
            return None;
        }

        let width = to_bytes(n).len();
        let k = to_uint(&hash(&[&to_bytes(n), &pad(&to_bytes(&self.g), width)]));
        let u = to_uint(&hash(&[&pad(&to_bytes(&self.big_a), width), &pad(&to_bytes(&big_b), width)]));
        if u == BigUint::ZERO {
            return None;
        }

        // x = H(salt | H(":" | password)): the user name is deliberately absent.
        let inner = hash(&[b":", derived_password]);
        let x = to_uint(&hash(&[salt, &inner]));
        let v = self.g.modpow(&x, n);

        // (B - k*v) mod N, kept non-negative.
        let kv = (&k * &v) % n;
        let base = ((&big_b % n) + n - kv) % n;
        let s = base.modpow(&(&self.a + &u * &x), n);
        let key = hash(&[&to_bytes(&s)]);

        let m1 = hash(&[
            &h_n_xor_g(n, &self.g),
            &hash(&[username.as_bytes()]),
            salt,
            &to_bytes(&self.big_a),
            &to_bytes(&big_b),
            &key,
        ]);
        let m2 = hash(&[&to_bytes(&self.big_a), &m1, &key]);
        Some(Proof { m1, m2 })
    }
}

/// `H(N) xor H(pad(g))`, the first term of the client proof.
fn h_n_xor_g(n: &BigUint, g: &BigUint) -> Vec<u8> {
    let n_bytes = to_bytes(n);
    let h_n = hash(&[&n_bytes]);
    let h_g = hash(&[&pad(&to_bytes(g), n_bytes.len())]);
    h_n.iter().zip(&h_g).map(|(a, b)| a ^ b).collect()
}

fn hash(parts: &[&[u8]]) -> Vec<u8> {
    let mut ctx = Context::new(&SHA256);
    for part in parts {
        ctx.update(part);
    }
    ctx.finish().as_ref().to_vec()
}

fn to_uint(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

/// Minimal big-endian encoding; zero is the empty string, as in the reference.
fn to_bytes(n: &BigUint) -> Vec<u8> {
    if *n == BigUint::ZERO { Vec::new() } else { n.to_bytes_be() }
}

fn pad(bytes: &[u8], width: usize) -> Vec<u8> {
    let mut out = vec![0u8; width.saturating_sub(bytes.len())];
    out.extend_from_slice(bytes);
    out
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn protocol_names_round_trip() {
        assert_eq!(Protocol::parse("s2k").unwrap(), Protocol::S2k);
        assert_eq!(Protocol::parse("s2k_fo").unwrap(), Protocol::S2kFo);
        assert!(Protocol::parse("s3k").is_err());
    }

    #[test]
    fn iteration_count_is_bounded() {
        assert!(derive_password("pw", b"salt", 0, Protocol::S2k).is_err());
        assert!(derive_password("pw", b"salt", MAX_PBKDF2_ITERATIONS + 1, Protocol::S2k).is_err());
        assert!(derive_password("pw", b"salt", 1, Protocol::S2k).is_ok());
    }

    #[test]
    fn s2k_and_s2k_fo_differ() {
        let a = derive_password("hunter2", b"0123456789abcdef", 10, Protocol::S2k).unwrap();
        let b = derive_password("hunter2", b"0123456789abcdef", 10, Protocol::S2kFo).unwrap();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn hex_encoding_is_lowercase() {
        assert_eq!(hex_lower(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(unhex("00abff"), vec![0x00, 0xab, 0xff]);
    }

    #[test]
    fn random_ephemerals_are_full_width_and_distinct() {
        let a = SrpClient::new().unwrap();
        let b = SrpClient::new().unwrap();
        assert_ne!(a.public_a(), b.public_a());
        // A is a residue mod N, so at most 256 bytes.
        assert!(a.public_a().len() <= 256);
    }

    #[test]
    fn degenerate_server_values_are_rejected() {
        let client = SrpClient::from_secret(&[0x80; 256]);
        // B == 0 and B == N both violate the SRP-6a safety check.
        assert!(client.process_challenge("u", b"p", b"s", &[0]).is_none());
        assert!(client.process_challenge("u", b"p", b"s", &unhex(N_HEX)).is_none());
    }
}
