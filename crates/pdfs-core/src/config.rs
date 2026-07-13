//! Static client identity and per-user filesystem paths.

use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Proton requires `external-drive-{name}@{semver}-{channel}` (channel ∈
/// stable/beta/alpha); a malformed value trips the 422 anti-abuse path.
pub const APP_VERSION: &str = "external-drive-linux@0.1.0-alpha";
pub const USER_AGENT: &str = "proton-drive-linux/0.1.0";

/// Keyring service name; one entry per credential kind keyed by username.
pub const KEYRING_SERVICE: &str = "proton-drive-linux";

/// Default soft cap on the on-disk content cache (5 GiB). LRU-evicts unpinned
/// blobs back under this; pinned files are exempt. See [`crate::cache`].
pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Configuration structure allowing the user to customize client identification headers.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AppConfig {
    pub app_version: String,
    pub user_agent: String,
    /// Soft cap on the on-disk content cache, in bytes (`0` = unlimited).
    /// `None` means "use [`DEFAULT_CACHE_BUDGET_BYTES`]"; the Settings page
    /// writes an explicit value here. Defaulted for configs predating the field.
    #[serde(default)]
    pub cache_budget: Option<u64>,
    /// Mountpoint the daemon mounts at. `None` means
    /// [`AppDirs::default_mountpoint`]; the Settings page writes an explicit
    /// path here. Defaulted for configs predating the field.
    #[serde(default)]
    pub mountpoint: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app_version: APP_VERSION.to_string(),
            user_agent: USER_AGENT.to_string(),
            cache_budget: None,
            mountpoint: None,
        }
    }
}

impl AppConfig {
    /// The effective cache budget in bytes: the user's explicit choice, or
    /// [`DEFAULT_CACHE_BUDGET_BYTES`] when unset.
    pub fn resolved_cache_budget(&self) -> u64 {
        self.cache_budget.unwrap_or(DEFAULT_CACHE_BUDGET_BYTES)
    }
}

/// Resolved XDG locations for state, cache, and the default mountpoint.
pub struct AppDirs {
    dirs: ProjectDirs,
}

impl AppDirs {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("io", "narl", "proton-drive-linux")
            .ok_or_else(|| Error::Other("cannot resolve home directory".into()))?;
        Ok(Self { dirs })
    }

    /// Configuration file path (e.g. ~/.config/proton-drive-linux/config.json).
    pub fn config_path(&self) -> PathBuf {
        self.dirs.config_dir().join("config.json")
    }

    /// Load config from disk, creating default if missing.
    pub fn load_config(&self) -> AppConfig {
        let path = self.config_path();
        if path.exists()
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(config) = serde_json::from_str::<AppConfig>(&content)
        {
            return config;
        }

        let default_config = AppConfig::default();
        if let Ok(content) = serde_json::to_string_pretty(&default_config) {
            let _ = std::fs::create_dir_all(self.dirs.config_dir());
            let _ = std::fs::write(&path, content);
        }
        default_config
    }

    /// Persist `config` to [`config_path`](Self::config_path), creating the
    /// config dir if missing. The Settings page calls this after editing the
    /// cache budget or mountpoint; the daemon re-reads the file on its next mount.
    pub fn save_config(&self, config: &AppConfig) -> Result<()> {
        let path = self.config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(config)?)?;
        Ok(())
    }

    /// Persistent state: inode map / cache index DB lives here.
    pub fn state_dir(&self) -> PathBuf {
        // `state_dir` is Linux-only in `directories`; fall back to data dir.
        self.dirs
            .state_dir()
            .unwrap_or_else(|| self.dirs.data_dir())
            .to_path_buf()
    }

    /// Hydrated file-content cache.
    pub fn cache_dir(&self) -> PathBuf {
        self.dirs.cache_dir().to_path_buf()
    }

    /// Directory holding cached file-content blobs (pinned files).
    pub fn content_cache_dir(&self) -> PathBuf {
        self.cache_dir().join("content")
    }

    /// JSON pin registry, in persistent state (not the evictable cache).
    pub fn pins_path(&self) -> PathBuf {
        self.state_dir().join("pins.json")
    }

    /// Unified SQLite metadata cache (inodes, FTS, cache LRU, pins). Lives in
    /// persistent state next to `control.sock`; only the daemon writes it.
    pub fn db_path(&self) -> PathBuf {
        self.state_dir().join("cache.db")
    }

    /// Unix domain socket the mount daemon listens on for CLI control commands.
    pub fn control_socket(&self) -> PathBuf {
        self.state_dir().join("control.sock")
    }

    /// Unix domain socket the tray uses to ensure single instance.
    pub fn tray_socket(&self) -> PathBuf {
        self.state_dir().join("tray.sock")
    }

    /// The user's home directory — the root the daemon indexes for local
    /// (non-Drive) file search. `None` if it cannot be resolved.
    pub fn home_dir(&self) -> Option<PathBuf> {
        directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
    }

    /// Default mountpoint when the user does not pass one explicitly.
    pub fn default_mountpoint(&self) -> PathBuf {
        directories::UserDirs::new()
            .map(|u| u.home_dir().join("ProtonDrive"))
            .unwrap_or_else(|| PathBuf::from("/tmp/ProtonDrive"))
    }

    /// The effective mountpoint: the config's explicit path, or
    /// [`default_mountpoint`](Self::default_mountpoint) when unset.
    pub fn resolved_mountpoint(&self, config: &AppConfig) -> PathBuf {
        config
            .mountpoint
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_mountpoint())
    }

    /// Create state + cache dirs if missing.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.state_dir())?;
        std::fs::create_dir_all(self.cache_dir())?;
        let _ = std::fs::create_dir_all(self.dirs.config_dir());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_config_serialization() {
        let config = AppConfig {
            app_version: "external-drive-test-client@1.0.0".to_string(),
            user_agent: "test-agent/1.0".to_string(),
            cache_budget: Some(1234),
            mountpoint: Some("/tmp/x".to_string()),
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.app_version, "external-drive-test-client@1.0.0");
        assert_eq!(decoded.user_agent, "test-agent/1.0");
    }

    #[test]
    fn test_default_app_config() {
        let default_config = AppConfig::default();
        assert_eq!(default_config.app_version, APP_VERSION);
        assert_eq!(default_config.user_agent, USER_AGENT);
    }
}
