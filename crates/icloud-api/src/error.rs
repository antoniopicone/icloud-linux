//! Error type shared by every part of the client.
//!
//! Messages never contain credentials, tokens or cookies. Underlying
//! `reqwest` errors are stripped of their URL because Apple's query strings
//! carry the account's `dsid` and client identifiers.

use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport-level failure: DNS, TLS, timeout, connection reset.
    #[error("network error: {0}")]
    Http(reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unexpected response from iCloud: {0}")]
    Json(#[from] serde_json::Error),

    /// A non-success HTTP status without an API-level explanation.
    #[error("iCloud answered HTTP {status}: {reason}")]
    Status { status: u16, reason: String },

    /// An API-level error carried in a JSON body.
    #[error("{}", ApiDisplay(.code, .reason))]
    Api { code: Option<String>, reason: String },

    /// The account has two-factor authentication and the session is not
    /// trusted yet. Not a failure: the caller is expected to ask for a code.
    #[error("two-factor authentication is required")]
    TwoFactorRequired,

    /// The stored session is no longer accepted. Interactive re-login needed.
    #[error("the iCloud session is no longer valid: {0}")]
    AuthRequired(String),

    /// Apple rejected the credentials, or the login handshake failed.
    #[error("login failed: {0}")]
    LoginFailed(String),

    /// The requested service is not enabled for the account.
    #[error("iCloud service not available: {0}")]
    NotActivated(String),

    /// The server did something the protocol does not allow for.
    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("no trusted phone number is available to send a code to")]
    NoTrustedPhone,

    #[error("the verification code was not accepted")]
    WrongCode,
}

struct ApiDisplay<'a>(&'a Option<String>, &'a str);

impl fmt::Display for ApiDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(code) => write!(f, "iCloud error {code}: {}", self.1),
            None => write!(f, "iCloud error: {}", self.1),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err.without_url())
    }
}

impl Error {
    /// True when retrying cannot help until the user authenticates again.
    ///
    /// Deliberately narrow: a generic API error whose text mentions
    /// authentication is *not* one of these, because Apple also uses it for
    /// transient server-side failures that succeed on retry.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::TwoFactorRequired | Self::AuthRequired(_) | Self::LoginFailed(_))
    }
}
