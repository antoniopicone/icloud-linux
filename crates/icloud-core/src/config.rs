//! `config.yaml`.
//!
//! The file format is the one the Python implementation used, so existing
//! configurations keep working. Two things are stricter on purpose: unknown
//! values for the mode options are rejected instead of silently replaced by a
//! default, and the password is optional and never printed.

use std::{
    fs,
    io::Write as _,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use icloud_api::DeleteMode;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    dirs::Layout,
    error::{Error, Result},
};

/// How the remote tree is discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrawlMode {
    /// List each folder the first time it is opened, like Finder does.
    #[default]
    Lazy,
    /// Crawl the whole drive at startup and on every refresh.
    Full,
}

/// When file contents are downloaded (only relevant to [`CrawlMode::Full`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WarmupMode {
    /// Fetch everything eligible in the background once it is known.
    #[default]
    Background,
    /// Fetch a file only when it is opened.
    Lazy,
}

/// What happens when a path changed both here and on iCloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictMode {
    /// Keep the local version as `<name>.local-conflict-<timestamp>`.
    #[default]
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FuseOptions {
    /// Let other users see the mount. Needs `user_allow_other` in `/etc/fuse.conf`.
    pub allow_other: bool,
    /// Mount read-only.
    pub ro: bool,
    /// Accepted for compatibility; libfuse 3 always allows non-empty mount points.
    pub nonempty: bool,
}

impl Default for FuseOptions {
    fn default() -> Self {
        Self { allow_other: false, ro: false, nonempty: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Apple ID.
    pub username: String,
    /// Optional. With it the daemon can renew an expired session by itself
    /// (a trusted session needs no code); without it, an expired session means
    /// running `icloudctl auth` again. Prefer leaving it out.
    #[serde(with = "secret_option", skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretString>,
    pub cache_dir: PathBuf,
    pub cookie_dir: PathBuf,
    /// Where to mount, if not given on the command line.
    pub mount_dir: Option<PathBuf>,
    pub crawl_mode: CrawlMode,
    pub warmup_mode: WarmupMode,
    pub conflict_mode: ConflictMode,
    /// What a delete on the mount does on iCloud: `trash` (recoverable in
    /// "Recently Deleted") or `permanent`.
    pub delete_mode: DeleteMode,
    pub upload_interval_seconds: u64,
    pub remote_refresh_interval_seconds: u64,
    pub warmup_workers: usize,
    /// Allow-list of iCloud paths; empty means all.
    pub sync_paths: Vec<String>,
    /// Deny-list of iCloud paths; wins over `sync_paths`.
    pub exclude_paths: Vec<String>,
    /// `false`: no background polling; use `icloudctl sync` to pull changes.
    pub auto_sync: bool,
    pub fuse_options: FuseOptions,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: None,
            cache_dir: PathBuf::from("~/.cache/icloud-linux"),
            cookie_dir: PathBuf::from("~/.config/icloud-linux/cookies"),
            mount_dir: None,
            crawl_mode: CrawlMode::default(),
            warmup_mode: WarmupMode::default(),
            conflict_mode: ConflictMode::default(),
            delete_mode: DeleteMode::Trash,
            upload_interval_seconds: 30,
            remote_refresh_interval_seconds: 300,
            warmup_workers: 1,
            sync_paths: Vec::new(),
            exclude_paths: Vec::new(),
            auto_sync: true,
            fuse_options: FuseOptions::default(),
        }
    }
}

impl Config {
    /// A configuration whose paths point into `layout`.
    pub fn for_layout(layout: &Layout) -> Self {
        Self { cache_dir: layout.cache_dir.clone(), cookie_dir: layout.cookie_dir(), ..Self::default() }
    }

    pub fn load(path: &Path, layout: &Layout) -> Result<Self> {
        let text =
            fs::read_to_string(path).map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Self::parse(&text, layout).map_err(|e| match e {
            Error::Config(msg) => Error::Config(format!("{}: {msg}", path.display())),
            other => other,
        })
    }

    pub fn parse(yaml: &str, layout: &Layout) -> Result<Self> {
        // An empty file (or one with only comments) is a valid, all-defaults config.
        let mut config: Self = if yaml.trim().is_empty() {
            Self::for_layout(layout)
        } else {
            serde_yaml_ng::from_str(yaml).map_err(|e| Error::Config(e.to_string()))?
        };
        config.cache_dir = layout.expand(&config.cache_dir.to_string_lossy());
        config.cookie_dir = layout.expand(&config.cookie_dir.to_string_lossy());
        config.mount_dir = config.mount_dir.map(|p| layout.expand(&p.to_string_lossy()));
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.upload_interval_seconds == 0 || self.remote_refresh_interval_seconds == 0 {
            return Err(Error::Config("the sync intervals must be at least one second".into()));
        }
        if self.warmup_workers == 0 || self.warmup_workers > 16 {
            return Err(Error::Config("warmup_workers must be between 1 and 16".into()));
        }
        Ok(())
    }

    /// The user name, or an error saying how to set it.
    pub fn require_username(&self) -> Result<&str> {
        if self.username.trim().is_empty() {
            Err(Error::Config("no Apple ID configured: run `icloudctl configure`".into()))
        } else {
            Ok(self.username.trim())
        }
    }

    pub fn upload_interval(&self) -> Duration {
        Duration::from_secs(self.upload_interval_seconds)
    }

    pub fn refresh_interval(&self) -> Duration {
        Duration::from_secs(self.remote_refresh_interval_seconds)
    }

    /// The sync state database.
    pub fn state_db(&self) -> PathBuf {
        self.cache_dir.join("state.sqlite3")
    }

    /// The local mirror of the drive.
    pub fn mirror_dir(&self) -> PathBuf {
        self.cache_dir.join("mirror")
    }

    /// Serialise with mode 0600, atomically. Creates the parent directory
    /// with mode 0700.
    pub fn save(&self, path: &Path) -> Result<()> {
        let body = format!(
            "# icloud-linux configuration. See config.example.yaml in the repository for every option.\n{}",
            serde_yaml_ng::to_string(self).map_err(|e| Error::Config(e.to_string()))?
        );
        write_private_file(path, body.as_bytes())?;
        Ok(())
    }
}

/// Atomically write `bytes` to `path` with mode 0600.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

mod secret_option {
    use super::{Deserialize, Deserializer, ExposeSecret, SecretString, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &Option<SecretString>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(secret) => s.serialize_some(secret.expose_secret()),
            None => s.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SecretString>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.filter(|s| !s.is_empty()).map(SecretString::from))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn layout() -> Layout {
        Layout::under(Path::new("/home/u"))
    }

    #[test]
    fn the_python_era_example_config_parses_unchanged() {
        let yaml = r#"
username: "me@example.com"
password: "secret"
cache_dir: "~/.cache/icloud-linux"
warmup_mode: "background"
conflict_mode: "copy"
upload_interval_seconds: 30
remote_refresh_interval_seconds: 300
warmup_workers: 1
cookie_dir: "~/.config/icloud-linux/cookies"
auto_sync: true
sync_paths:
  - /Downloads
exclude_paths:
  - /Downloads/Large Archive
fuse_options:
  allow_other: false
  ro: false
  nonempty: true
"#;
        let config = Config::parse(yaml, &layout()).unwrap();
        assert_eq!(config.username, "me@example.com");
        assert_eq!(config.password.as_ref().map(ExposeSecret::expose_secret), Some("secret"));
        assert_eq!(config.cache_dir, PathBuf::from("/home/u/.cache/icloud-linux"));
        assert_eq!(config.crawl_mode, CrawlMode::Lazy, "new option defaults to lazy");
        assert_eq!(config.delete_mode, DeleteMode::Trash);
        assert_eq!(config.sync_paths, ["/Downloads"]);
        assert_eq!(config.mirror_dir(), PathBuf::from("/home/u/.cache/icloud-linux/mirror"));
    }

    #[test]
    fn an_empty_file_means_all_defaults() {
        let config = Config::parse("# nothing\n", &layout()).unwrap();
        assert_eq!(config.upload_interval_seconds, 30);
        assert!(config.auto_sync);
        assert!(config.password.is_none());
        assert_eq!(config.cache_dir, PathBuf::from("/home/u/.cache/icloud-linux"));
    }

    #[test]
    fn unknown_mode_values_are_errors_not_silent_defaults() {
        for bad in ["crawl_mode: turbo", "warmup_mode: never", "conflict_mode: overwrite", "delete_mode: shred"] {
            assert!(Config::parse(bad, &layout()).is_err(), "{bad}");
        }
    }

    #[test]
    fn nonsense_numbers_are_rejected() {
        assert!(Config::parse("upload_interval_seconds: 0", &layout()).is_err());
        assert!(Config::parse("remote_refresh_interval_seconds: 0", &layout()).is_err());
        assert!(Config::parse("warmup_workers: 0", &layout()).is_err());
        assert!(Config::parse("warmup_workers: 500", &layout()).is_err());
        assert!(Config::parse("upload_interval_seconds: -3", &layout()).is_err());
    }

    #[test]
    fn unknown_keys_are_tolerated_for_forward_compatibility() {
        assert!(Config::parse("some_future_option: 1\nusername: a", &layout()).is_ok());
    }

    #[test]
    fn debug_output_never_contains_the_password() {
        let config = Config::parse("username: a\npassword: hunter2", &layout()).unwrap();
        assert!(!format!("{config:?}").contains("hunter2"));
    }

    #[test]
    fn an_empty_password_counts_as_none() {
        let config = Config::parse("password: ''", &layout()).unwrap();
        assert!(config.password.is_none());
    }

    #[test]
    fn saving_is_private_atomic_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::under(dir.path());
        let mut config = Config::for_layout(&layout);
        config.username = "me@example.com".into();
        config.password = Some(SecretString::from("pw".to_owned()));
        config.sync_paths = vec!["/A".into()];
        config.crawl_mode = CrawlMode::Full;

        let path = layout.config_file();
        config.save(&path).unwrap();

        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(!path.with_extension("tmp").exists());

        let loaded = Config::load(&path, &layout).unwrap();
        assert_eq!(loaded.username, "me@example.com");
        assert_eq!(loaded.password.as_ref().map(ExposeSecret::expose_secret), Some("pw"));
        assert_eq!(loaded.crawl_mode, CrawlMode::Full);
        assert_eq!(loaded.sync_paths, ["/A"]);
    }

    #[test]
    fn a_config_without_a_password_does_not_write_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::under(dir.path());
        let path = layout.config_file();
        Config::for_layout(&layout).save(&path).unwrap();
        assert!(!fs::read_to_string(path).unwrap().contains("password"));
    }

    #[test]
    fn the_username_is_required_where_it_matters() {
        assert!(Config::default().require_username().is_err());
        let config = Config { username: "  me@x.com ".into(), ..Config::default() };
        assert_eq!(config.require_username().unwrap(), "me@x.com");
    }

    #[test]
    fn load_errors_name_the_file() {
        let err = Config::load(Path::new("/nonexistent/config.yaml"), &layout()).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/config.yaml"));
    }
}
