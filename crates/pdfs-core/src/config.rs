//! Static client identity and per-user filesystem paths.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::syncignore::DEFAULT_IGNORE_PATTERNS;

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
    /// Ignore patterns (gitignore syntax) applied to every mirror sync folder,
    /// on top of each folder's own `.pdfsignore`. `None` means "use
    /// [`DEFAULT_IGNORE_PATTERNS`]"; an explicit empty list disables the
    /// defaults. Defaulted for configs predating the field.
    #[serde(default)]
    pub ignore_patterns: Option<Vec<String>>,
    /// Device uid this machine has been *adopted* into, overriding the hostname
    /// heuristic in `ensure_device`. Lives here rather than in `cache.db` so it
    /// survives a fresh install — on a new machine the DB is empty by
    /// definition, which is exactly when the pin matters. `None` means "resolve
    /// by hostname". Defaulted for configs predating the field.
    #[serde(default)]
    pub device_uid: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app_version: APP_VERSION.to_string(),
            user_agent: USER_AGENT.to_string(),
            cache_budget: None,
            mountpoint: None,
            ignore_patterns: None,
            device_uid: None,
        }
    }
}

impl AppConfig {
    /// The effective cache budget in bytes: the user's explicit choice, or
    /// [`DEFAULT_CACHE_BUDGET_BYTES`] when unset.
    pub fn resolved_cache_budget(&self) -> u64 {
        self.cache_budget.unwrap_or(DEFAULT_CACHE_BUDGET_BYTES)
    }

    /// The effective global sync ignore patterns: the user's explicit list, or
    /// [`DEFAULT_IGNORE_PATTERNS`] when unset. An explicit empty list is
    /// honoured as "no global patterns", which is why this is not a
    /// `unwrap_or_default`.
    pub fn resolved_ignore_patterns(&self) -> Vec<String> {
        match &self.ignore_patterns {
            Some(patterns) => patterns.clone(),
            None => DEFAULT_IGNORE_PATTERNS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
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
        if path.exists() {
            match std::fs::read_to_string(&path)
                .map_err(Error::from)
                .and_then(|content| {
                    serde_json::from_str::<AppConfig>(&content).map_err(Error::from)
                }) {
                Ok(config) => return config,
                Err(e) => {
                    // Preserve the original bytes for diagnosis/recovery.  In
                    // particular, never replace a truncated config with defaults
                    // and silently lose mount, device, or ignore settings.
                    tracing::error!(path = %path.display(), error = %e, "config is unreadable; using in-memory defaults without overwriting it");
                    return AppConfig::default();
                }
            }
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
        let parent = path
            .parent()
            .ok_or_else(|| Error::Other("configuration path has no parent".into()))?;
        std::fs::create_dir_all(parent)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let tmp = path.with_extension(format!("json.tmp-{}-{nonce}", std::process::id()));
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        use std::io::Write as _;
        file.write_all(&serde_json::to_vec_pretty(config)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(parent)?.sync_all()?;
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

    /// Create state + cache dirs if missing, owner-only.
    ///
    /// The mode is set explicitly rather than left to the umask. These
    /// directories hold decrypted file content, a plaintext index of every node
    /// name in the Drive, and the control socket — and `create_dir_all` under a
    /// typical umask makes them `0755`. Until this was fixed, the only thing
    /// keeping another local user out was `~/.cache` and `~/.local/state`
    /// happening to be `0700`, which is a convention of the user's system and
    /// not something this client established (bugs.md B6).
    ///
    /// Applied on every start, not just at creation: a directory that already
    /// exists with a permissive mode — restored from a backup that flattened
    /// modes, or created by an older build — is tightened here.
    pub fn ensure(&self) -> Result<()> {
        for dir in [self.state_dir(), self.cache_dir()] {
            std::fs::create_dir_all(&dir)?;
            restrict_dir(&dir)?;
        }
        let config_dir = self.dirs.config_dir().to_path_buf();
        std::fs::create_dir_all(&config_dir)?;
        restrict_dir(&config_dir)?;
        Ok(())
    }
}

/// Make `dir` owner-only (`0700`). These directories contain decrypted bytes,
/// plaintext names, and an authenticated control socket, so failure is fatal.
fn restrict_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt};
    let before = std::fs::symlink_metadata(dir)?;
    if before.file_type().is_symlink() || !before.is_dir() {
        return Err(Error::Other(format!(
            "sensitive path is not a real directory: {}",
            dir.display()
        )));
    }
    if before.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Other(format!(
            "sensitive directory is not owned by the current user: {}",
            dir.display()
        )));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let after = std::fs::symlink_metadata(dir)?;
    if after.permissions().mode() & 0o777 != 0o700 {
        return Err(Error::Other(format!(
            "sensitive directory did not become owner-only: {}",
            dir.display()
        )));
    }
    Ok(())
}

/// Make a just-bound Unix socket owner-only (`0600`).
///
/// Connecting to the control socket drives the daemon with its authenticated
/// session — enumerate, read, upload, trash, share — without presenting any
/// credential, since the daemon already holds one. It is an authority boundary,
/// not merely private data, and `UnixListener::bind` applies the umask like any
/// other file (bugs.md B6).
///
/// There is a window between `bind` and this call during which the socket
/// carries the umask's mode. It is closed in practice by the containing
/// directory: [`Dirs::ensure`] has already made the state directory `0700`, so
/// nothing else can reach the socket to exploit the window.
pub fn restrict_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// bugs.md B6. A permissive mode here is not a cosmetic problem: these
    /// directories hold decrypted content, a plaintext index of every node name,
    /// and the control socket that commands the daemon's session.
    #[test]
    fn restrict_dir_makes_a_directory_owner_only() {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-cfg-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Start deliberately world-readable, as `create_dir_all` under a typical
        // umask would leave it.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        restrict_dir(&dir).unwrap();

        assert_eq!(mode_of(&dir), 0o700, "group and other must have no access");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restrict_dir_refuses_a_symlink() {
        let root = std::env::temp_dir().join(format!(
            "pdfs-cfg-link-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let target = root.join("target");
        let link = root.join("state");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(restrict_dir(&link).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Connecting to the control socket confers the daemon's authenticated
    /// session with no credential, so it is an authority boundary rather than
    /// merely private data.
    #[test]
    fn restrict_socket_makes_a_socket_owner_only() {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-sock-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("control.sock");
        // chmod semantics are identical for a socket path and a regular file;
        // use a file so this regression also runs in sandboxes that prohibit
        // creating Unix sockets.
        let listener = std::fs::File::create(&sock).unwrap();

        restrict_socket(&sock).unwrap();

        assert_eq!(mode_of(&sock), 0o600, "no other local user may connect");
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_app_config_serialization() {
        let config = AppConfig {
            app_version: "external-drive-test-client@1.0.0".to_string(),
            user_agent: "test-agent/1.0".to_string(),
            cache_budget: Some(1234),
            mountpoint: Some("/tmp/x".to_string()),
            ignore_patterns: Some(vec!["build/".to_string()]),
            device_uid: Some("dev-uid".to_string()),
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

    #[test]
    fn a_config_predating_ignore_patterns_still_loads() {
        // Every installed config on disk looks like this; the field must not be
        // required or the daemon would fall back to a default config on upgrade.
        let json = r#"{"app_version":"v","user_agent":"u"}"#;
        let decoded: AppConfig = serde_json::from_str(json).unwrap();
        assert!(decoded.ignore_patterns.is_none());
        assert_eq!(
            decoded.resolved_ignore_patterns().len(),
            DEFAULT_IGNORE_PATTERNS.len(),
            "an absent list means the defaults apply"
        );
    }

    #[test]
    fn an_explicit_empty_ignore_list_disables_the_defaults() {
        let config = AppConfig {
            ignore_patterns: Some(Vec::new()),
            ..AppConfig::default()
        };
        assert!(
            config.resolved_ignore_patterns().is_empty(),
            "opting out must be expressible, so this cannot be unwrap_or_default"
        );
    }
}
