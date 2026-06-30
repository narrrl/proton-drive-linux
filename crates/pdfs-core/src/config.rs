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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app_version: APP_VERSION.to_string(),
            user_agent: USER_AGENT.to_string(),
        }
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

    /// Default mountpoint when the user does not pass one explicitly.
    pub fn default_mountpoint(&self) -> PathBuf {
        directories::UserDirs::new()
            .map(|u| u.home_dir().join("ProtonDrive"))
            .unwrap_or_else(|| PathBuf::from("/tmp/ProtonDrive"))
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
