//! Where icloud-linux keeps its files.
//!
//! The layout is the one the earlier Python implementation used, so an
//! existing installation is picked up as it is.

use std::{
    env,
    path::{Path, PathBuf},
};

use crate::error::{Error, Result};

const APP: &str = "icloud-linux";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub home: PathBuf,
    /// `~/.config/icloud-linux`: configuration, environment file, cookies.
    pub config_dir: PathBuf,
    /// `~/.local/state/icloud-linux`: log file and the on-demand-sync marker.
    pub state_dir: PathBuf,
    /// `~/.cache/icloud-linux`: default cache root (mirror and state database).
    pub cache_dir: PathBuf,
    /// `~/.config/systemd/user`.
    pub systemd_user_dir: PathBuf,
    /// `~/.config`: home of the GTK bookmarks the file manager reads.
    pub config_home: PathBuf,
    /// `~/.local/share`: where the file manager looks for scripts.
    pub data_home: PathBuf,
}

impl Layout {
    /// Resolve from `$HOME` and the XDG variables.
    pub fn from_env() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| Error::Config("$HOME is not set to an absolute path".into()))?;
        let xdg = |var: &str, fallback: &str| {
            env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(fallback))
        };
        Ok(Self {
            config_dir: xdg("XDG_CONFIG_HOME", ".config").join(APP),
            state_dir: xdg("XDG_STATE_HOME", ".local/state").join(APP),
            cache_dir: xdg("XDG_CACHE_HOME", ".cache").join(APP),
            systemd_user_dir: xdg("XDG_CONFIG_HOME", ".config").join("systemd/user"),
            config_home: xdg("XDG_CONFIG_HOME", ".config"),
            data_home: xdg("XDG_DATA_HOME", ".local/share"),
            home,
        })
    }

    /// A layout rooted under `root`, for tests.
    pub fn under(root: &Path) -> Self {
        Self {
            home: root.to_owned(),
            config_dir: root.join(".config").join(APP),
            state_dir: root.join(".local/state").join(APP),
            cache_dir: root.join(".cache").join(APP),
            systemd_user_dir: root.join(".config/systemd/user"),
            config_home: root.join(".config"),
            data_home: root.join(".local/share"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.yaml")
    }

    pub fn env_file(&self) -> PathBuf {
        self.config_dir.join("icloud.env")
    }

    pub fn cookie_dir(&self) -> PathBuf {
        self.config_dir.join("cookies")
    }

    pub fn service_file(&self) -> PathBuf {
        self.systemd_user_dir.join("icloud.service")
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("icloud.log")
    }

    /// Written by the daemon when an on-demand sync finishes.
    pub fn sync_marker(&self) -> PathBuf {
        self.state_dir.join("sync_done")
    }

    /// The bookmarks Nautilus and GTK file dialogs show in their sidebar.
    pub fn bookmarks_file(&self) -> PathBuf {
        self.config_home.join("gtk-3.0/bookmarks")
    }

    pub fn default_mount(&self) -> PathBuf {
        self.home.join("iCloud")
    }

    /// Expand a leading `~` or `~/` using this layout's home directory.
    pub fn expand(&self, path: &str) -> PathBuf {
        match path.strip_prefix('~') {
            Some("") => self.home.clone(),
            Some(rest) if rest.starts_with('/') => self.home.join(rest.trim_start_matches('/')),
            _ => PathBuf::from(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion_only_touches_a_leading_tilde() {
        let layout = Layout::under(Path::new("/home/u"));
        assert_eq!(layout.expand("~"), PathBuf::from("/home/u"));
        assert_eq!(layout.expand("~/x/y"), PathBuf::from("/home/u/x/y"));
        assert_eq!(layout.expand("/abs/~/x"), PathBuf::from("/abs/~/x"));
        assert_eq!(layout.expand("~other/x"), PathBuf::from("~other/x"));
        assert_eq!(layout.expand("rel"), PathBuf::from("rel"));
    }

    #[test]
    fn files_live_where_the_python_version_kept_them() {
        let layout = Layout::under(Path::new("/h"));
        assert_eq!(layout.config_file(), PathBuf::from("/h/.config/icloud-linux/config.yaml"));
        assert_eq!(layout.env_file(), PathBuf::from("/h/.config/icloud-linux/icloud.env"));
        assert_eq!(layout.cookie_dir(), PathBuf::from("/h/.config/icloud-linux/cookies"));
        assert_eq!(layout.log_file(), PathBuf::from("/h/.local/state/icloud-linux/icloud.log"));
        assert_eq!(layout.sync_marker(), PathBuf::from("/h/.local/state/icloud-linux/sync_done"));
        assert_eq!(layout.service_file(), PathBuf::from("/h/.config/systemd/user/icloud.service"));
        assert_eq!(layout.cache_dir, PathBuf::from("/h/.cache/icloud-linux"));
    }
}
