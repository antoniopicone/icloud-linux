//! Errors of the sync engine and its supporting modules.

use std::io;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Api(#[from] icloud_api::Error),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("state database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("{0}")]
    Setup(String),

    /// The operation is outside the configured synchronisation boundary.
    #[error("{0} is outside the synchronisation boundary")]
    Forbidden(String),

    #[error("no iCloud session: run `icloudctl auth`")]
    Unauthenticated,
}

impl Error {
    /// True when only a fresh sign-in can help, so retrying is pointless.
    pub fn is_auth(&self) -> bool {
        match self {
            Self::Api(err) => err.is_auth(),
            Self::Unauthenticated => true,
            _ => false,
        }
    }
}
