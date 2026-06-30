//! Static client identity and per-user filesystem paths.

use std::path::PathBuf;

use directories::ProjectDirs;

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

    /// Unix domain socket the mount daemon listens on for CLI control commands.
    pub fn control_socket(&self) -> PathBuf {
        self.state_dir().join("control.sock")
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
        Ok(())
    }
}
