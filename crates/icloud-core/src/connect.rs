//! Connecting the daemon to iCloud.

use std::sync::Arc;

use icloud_api::{Client, ClientConfig, Drive, Endpoints, LoginStatus};

use crate::{
    config::Config,
    error::{Error, Result},
};

/// Outcome of trying to reach iCloud at startup.
pub enum Connection {
    Ready {
        drive: Arc<dyn Drive>,
        /// Kept so its session can be saved on shutdown.
        client: Arc<Client>,
    },
    /// No usable session. The daemon still runs and serves its cache, so it
    /// does not crash-loop, which is what would trigger Apple's lockout.
    Unauthenticated { reason: String },
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready { .. } => f.write_str("Connection::Ready"),
            Self::Unauthenticated { reason } => write!(f, "Connection::Unauthenticated({reason})"),
        }
    }
}

/// Is iCloud served from mainland China (`icloud.com.cn`)? Set `ICLOUD_CHINA=1`.
fn china_mainland() -> bool {
    std::env::var("ICLOUD_CHINA").is_ok_and(|v| v == "1")
}

/// A client for the account in `config`, with its session stored under
/// `cookie_dir`.
pub fn client_for(config: &Config) -> Result<Client> {
    let mut client_config = ClientConfig::new(config.require_username()?, Some(config.cookie_dir.clone()));
    client_config.endpoints = Endpoints::production(china_mainland());
    Ok(Client::new(client_config)?)
}

/// Reuse the saved session, and if that fails try the stored password once.
///
/// At most one password proof is sent. A network failure is returned as an
/// error (the caller may retry later, which costs nothing); a session that
/// needs the user comes back as [`Connection::Unauthenticated`].
pub fn connect(config: &Config) -> Result<Connection> {
    let client = client_for(config)?;

    if let Err(err) = client.resume() {
        if !err.is_auth() {
            return Err(err.into());
        }
        tracing::info!("the saved session cannot be reused ({err})");
        let Some(password) = &config.password else {
            return Ok(Connection::Unauthenticated { reason: err.to_string() });
        };
        tracing::info!("signing in with the stored password");
        match client.login(password) {
            Ok(LoginStatus::Authenticated) => {}
            Ok(LoginStatus::TwoFactorRequired) => {
                return Ok(Connection::Unauthenticated {
                    reason: "a verification code is needed; run `icloudctl auth`".into(),
                });
            }
            Err(err) if err.is_auth() => return Ok(Connection::Unauthenticated { reason: err.to_string() }),
            Err(err) => return Err(err.into()),
        }
    }

    let drive: Arc<dyn Drive> = Arc::new(client.drive().map_err(Error::from)?);
    Ok(Connection::Ready { drive, client: Arc::new(client) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::Layout;

    #[test]
    fn a_config_without_an_apple_id_cannot_connect() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::for_layout(&Layout::under(dir.path()));
        assert!(matches!(connect(&config), Err(Error::Config(_))));
    }

    #[test]
    fn without_a_saved_session_or_password_the_daemon_parks_unauthenticated_and_never_touches_the_network() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::for_layout(&Layout::under(dir.path()));
        config.username = "me@example.com".into();
        match connect(&config).unwrap() {
            Connection::Unauthenticated { reason } => assert!(reason.contains("no saved iCloud session"), "{reason}"),
            Connection::Ready { .. } => panic!("cannot be ready without a session"),
        }
    }
}
