//! Static client identity and per-user filesystem paths.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{Error, Result};
use crate::menu::PromptConfig;
use crate::opener::OpenWith;
use crate::syncignore::DEFAULT_IGNORE_PATTERNS;

/// Proton requires `external-drive-{name}@{semver}-{channel}` (channel ∈
/// stable/beta/alpha); a malformed value trips the 422 anti-abuse path.
/// Follows the release so Proton can tell releases apart and retire old ones
/// without retiring every one.
pub const APP_VERSION: &str = concat!(
    "external-drive-linux@",
    env!("CARGO_PKG_VERSION"),
    "-stable"
);
pub const USER_AGENT: &str = concat!("proton-drive-linux/", env!("CARGO_PKG_VERSION"));

/// The fixed identity every release up to 3.1.0 wrote into `config.json`. Read
/// back as "unset" so those configs follow the release instead of pinning it.
const LEGACY_APP_VERSION: &str = "external-drive-linux@0.1.0-alpha";
const LEGACY_USER_AGENT: &str = "proton-drive-linux/0.1.0";

/// Keyring service name of the default instance; see [`keyring_service`].
pub const KEYRING_SERVICE: &str = "proton-drive-linux";

/// Environment variable that names a separate instance of the client.
///
/// Unset, everything is as it always was. Set to a name, every place the client
/// keeps an account apart is suffixed with it: the XDG config, state and cache
/// directories (and with them the database, the control socket and the
/// machine id), the Secret Service entry, the kernel keys and the default
/// mountpoint. A second daemon can then run on another account next to the
/// user's own, and neither sees the other's session. The test tooling uses it
/// to run against a test account without signing the user out.
///
/// The systemd unit belongs to the default instance only: with an instance
/// set, the `service` calls do nothing, so a `pdfs login` for a test account
/// never restarts the user's daemon. Start that instance's `pdfs daemon` by
/// hand.
pub const INSTANCE_ENV: &str = "PDFS_INSTANCE";

/// Base name of the client's directories and Secret Service entry.
const APP_NAME: &str = "proton-drive-linux";

/// The instance named by [`INSTANCE_ENV`], read once per process. `None` for
/// the default instance.
///
/// # Panics
///
/// When the name is not 1 to 32 lowercase ASCII letters, digits or `-`. A name
/// that was ignored instead would fall back to the default instance and touch
/// the user's own account. Front ends call [`check_instance`] first to report
/// it as an error.
pub fn instance() -> Option<&'static str> {
    match parsed_instance() {
        Ok(instance) => *instance,
        Err(message) => panic!("{message}"),
    }
}

/// Fails when [`INSTANCE_ENV`] is set to a name [`instance`] would refuse.
pub fn check_instance() -> Result<()> {
    match parsed_instance() {
        Ok(_) => Ok(()),
        Err(message) => Err(Error::Other(message.clone())),
    }
}

fn parsed_instance() -> &'static std::result::Result<Option<&'static str>, String> {
    static INSTANCE: std::sync::OnceLock<std::result::Result<Option<&'static str>, String>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| match std::env::var(INSTANCE_ENV) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Ok(name) if name.is_empty() => Ok(None),
        Ok(name) if is_instance_name(&name) => Ok(Some(Box::leak(name.into_boxed_str()))),
        _ => Err(format!(
            "{INSTANCE_ENV} must be 1 to 32 lowercase letters, digits or '-'"
        )),
    })
}

fn is_instance_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `base`, or `base` and the instance name joined by `separator`.
pub fn instance_suffixed(base: &str, separator: char) -> String {
    match instance() {
        Some(name) => format!("{base}{separator}{name}"),
        None => base.to_string(),
    }
}

/// The Secret Service name the session is stored under: [`KEYRING_SERVICE`],
/// suffixed with the [`instance`] when one is set.
pub fn keyring_service() -> &'static str {
    static SERVICE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SERVICE.get_or_init(|| instance_suffixed(KEYRING_SERVICE, '-'))
}

/// The directory name under each XDG base directory: `proton-drive-linux`,
/// suffixed with the [`instance`] when one is set.
pub fn app_dir_name() -> String {
    instance_suffixed(APP_NAME, '-')
}

/// Default soft cap on the on-disk content cache (5 GiB). LRU-evicts unpinned
/// blobs back under this; pinned files are exempt. See `pdfs_store::cache`.
pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Environment override for [`AppConfig::resolved_conflict_sweep`]. Exists so a
/// support case can disable the sweep without editing the config file or
/// reinstalling — it wins over the stored setting.
pub const CONFLICT_SWEEP_ENV: &str = "PDFS_CONFLICT_SWEEP";

/// What the background conflict sweep (`pdfs-fuse`'s `pdfs-conflict-sweep`
/// thread) is allowed to do about a `(sync-conflict …)` copy it believes is a
/// redundant duplicate.
///
/// Defaults to [`SweepMode::Report`]: the sweep's whole reason to exist is that
/// the *previous* revision-identity check was wrong (B69), so the replacement is
/// not trusted with deletion until it has a release cycle of field evidence
/// behind it. See `docs/BUGS.md` B71.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SweepMode {
    /// Do not run the sweep at all — the thread is never spawned.
    Off,
    /// Run, and log what it *would* remove, but never trash anything. Divergent
    /// and orphaned copies are still surfaced to the activity feed as usual.
    #[default]
    Report,
    /// Trash copies proven identical to their live sibling.
    Enforce,
}

impl SweepMode {
    /// Parse an operator-supplied value ([`CONFLICT_SWEEP_ENV`] or a CLI flag).
    /// `None` for anything unrecognised, so a typo falls back to the stored
    /// setting rather than silently picking a mode the operator did not ask for.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "no" | "false" | "0" | "disabled" => Some(Self::Off),
            "report" | "dry-run" | "dryrun" | "observe" => Some(Self::Report),
            "enforce" | "on" | "yes" | "true" | "1" => Some(Self::Enforce),
            _ => None,
        }
    }

    /// Whether this mode is allowed to trash a duplicate.
    pub fn is_enforcing(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// Precedence for [`AppConfig::resolved_conflict_sweep`], split out from the
/// environment read so it can be unit-tested without mutating process state.
fn resolve_sweep_mode(env: Option<&str>, stored: Option<SweepMode>) -> SweepMode {
    env.and_then(SweepMode::parse)
        .or(stored)
        .unwrap_or_default()
}

/// Configuration structure allowing the user to customize client identification headers.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AppConfig {
    /// Override for [`APP_VERSION`]. `None` means "use the release's own";
    /// the old fixed default reads as `None`, so it leaves the file on the
    /// next save.
    #[serde(
        default,
        deserialize_with = "identity_override",
        skip_serializing_if = "Option::is_none"
    )]
    pub app_version: Option<String>,
    /// Override for [`USER_AGENT`], handled like `app_version`.
    #[serde(
        default,
        deserialize_with = "identity_override",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_agent: Option<String>,
    /// Soft cap on the on-disk content cache, in bytes (`0` = unlimited).
    /// `None` means "use [`DEFAULT_CACHE_BUDGET_BYTES`]"; the Settings page
    /// writes an explicit value here. Defaulted for configs predating the field.
    #[serde(default)]
    pub cache_budget: Option<u64>,
    /// Upload cap in bytes per second shared by every upload. `None` or `0`
    /// means no cap. Defaulted for configs predating the field.
    #[serde(default)]
    pub upload_limit: Option<u64>,
    /// Download cap in bytes per second shared by every download, FUSE reads
    /// included. `None` or `0` means no cap. Defaulted for configs predating
    /// the field.
    #[serde(default)]
    pub download_limit: Option<u64>,
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
    /// What the background conflict sweep may do. `None` means "use
    /// [`SweepMode::default`]" (report-only). Defaulted for configs predating
    /// the field, and overridable at runtime via [`CONFLICT_SWEEP_ENV`].
    #[serde(default)]
    pub conflict_sweep: Option<SweepMode>,
    /// How files opened from the prompt/browser are launched. `None` means
    /// "hand everything to `xdg-open`", which is what the client did before
    /// this existed. See [`crate::opener`]. Defaulted for configs predating the
    /// field.
    #[serde(default)]
    pub open_with: Option<OpenWith>,
    /// `pdfs-prompt` behaviour, including the external launcher used by
    /// `--dmenu`. See [`crate::menu`]. Defaulted for configs predating the
    /// field.
    #[serde(default)]
    pub prompt: Option<PromptConfig>,
    /// Paint the desktop app in Proton's colours instead of the system theme
    /// and accent colour. `None` means "follow the system". Defaulted for
    /// configs predating the field; also read from `proton_accent`, its name
    /// while it only changed the accent.
    #[serde(default, alias = "proton_accent")]
    pub proton_theme: Option<bool>,
    /// How the desktop app's My Files page lays out and orders a folder.
    /// Defaulted for configs predating the field.
    #[serde(default)]
    pub files_view: FilesView,
    /// How the desktop app's Photos page lays out its grid. Defaulted for
    /// configs predating the field.
    #[serde(default)]
    pub photos_view: PhotosView,
    /// The desktop app's main window size, remembered between runs. Defaulted
    /// for configs predating the field.
    #[serde(default)]
    pub window: WindowState,
    /// The user hid the tray icon from its own menu. The tray exits at start
    /// while this is set, and the app does not spawn it; Preferences turns it
    /// back on. Defaulted for configs predating the field.
    #[serde(default)]
    pub tray_hidden: bool,
    /// Language for the desktop app, tray and search prompt, as a gettext code
    /// such as `de` or `pt_BR`. `None` means "follow the system locale".
    /// Defaulted for configs predating the field.
    #[serde(default)]
    pub language: Option<String>,
    /// Draw the Places map from OpenFreeMap's street map instead of the
    /// built-in outline. Off by default: the tiles come from a third party,
    /// which then learns which parts of the world are looked at. Defaulted for
    /// configs predating the field.
    #[serde(default)]
    pub online_map: bool,
    /// Record a new folder, a new file, a rename or a delete through the mount
    /// locally and send it from the queue, instead of waiting for Drive in the
    /// syscall. `None` means on. `false` brings back the old path for one
    /// release (docs/ROADMAP.md). Defaulted for configs predating
    /// the field.
    #[serde(default)]
    pub local_first: Option<bool>,
    /// Send a file small enough, with its thumbnails, to fit 128 KiB in one
    /// request instead of three or more. `None` means on. `false` uploads
    /// every file in blocks, the way all files went before. Defaulted for
    /// configs predating the field.
    #[serde(default)]
    pub small_uploads: Option<bool>,
    /// Ask GitHub once a day whether a newer release is out, and say so in the
    /// desktop app. Off by default: GitHub then sees the address and version
    /// asking. Never used where a package manager installs updates (see
    /// [`crate::update::updates_managed`]). Defaulted for configs predating the
    /// field.
    #[serde(default)]
    pub update_check: bool,
    /// When the update check last asked GitHub, in Unix seconds, so a restart
    /// does not ask again within [`crate::update::CHECK_INTERVAL`]. Defaulted
    /// for configs predating the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_last_checked: Option<i64>,
    /// The latest release that check found, such as `3.6.0`, so the app can
    /// say so again after a restart without asking. Older than or equal to the
    /// running version once the update is installed. Defaulted for configs
    /// predating the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_latest: Option<String>,
}

/// The My Files page's view choices, remembered between runs.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(default)]
pub struct FilesView {
    /// Columns instead of the icon grid.
    pub list: bool,
    pub sort: FileSort,
    pub descending: bool,
    /// Folders ahead of files, whatever the sort.
    pub folders_first: bool,
    /// The icon grid's thumbnail edge in logical pixels. `None` means the
    /// app's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_size: Option<i32>,
}

impl Default for FilesView {
    fn default() -> Self {
        Self {
            list: false,
            sort: FileSort::Name,
            descending: false,
            folders_first: true,
            icon_size: None,
        }
    }
}

/// The Photos page's grid choices, remembered between runs.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(default)]
pub struct PhotosView {
    /// The zoom level: the row height the grid aims for, in logical pixels.
    /// `None` means the app's default.
    pub row_height: Option<i32>,
    /// Crop every photo to a square tile instead of showing it at its own
    /// shape.
    pub square: bool,
}

/// The desktop app's main window, as it was when it last closed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(default)]
pub struct WindowState {
    /// The unmaximised size, in logical pixels.
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl WindowState {
    /// Below this the saved size is taken for damage and the default is used.
    pub const MIN_WIDTH: i32 = 480;
    pub const MIN_HEIGHT: i32 = 400;

    /// This state, with a size too small to use replaced by the default.
    pub fn usable(self) -> Self {
        if self.width < Self::MIN_WIDTH || self.height < Self::MIN_HEIGHT {
            Self {
                maximized: self.maximized,
                ..Self::default()
            }
        } else {
            self
        }
    }
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 980,
            height: 680,
            maximized: false,
        }
    }
}

/// The key a My Files listing is ordered by.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FileSort {
    #[default]
    Name,
    Size,
    Modified,
}

impl FileSort {
    /// The name used in the desktop app's actions and the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            FileSort::Name => "name",
            FileSort::Size => "size",
            FileSort::Modified => "modified",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "name" => Some(FileSort::Name),
            "size" => Some(FileSort::Size),
            "modified" => Some(FileSort::Modified),
            _ => None,
        }
    }
}

/// Reads an `app_version` or `user_agent` override, dropping the fixed
/// identity older releases stored as the default.
fn identity_override<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.filter(|v| v != LEGACY_APP_VERSION && v != LEGACY_USER_AGENT))
}

impl AppConfig {
    /// The `x-pm-appversion` sent to Proton: the user's override, or
    /// [`APP_VERSION`].
    pub fn resolved_app_version(&self) -> &str {
        self.app_version.as_deref().unwrap_or(APP_VERSION)
    }

    /// The `User-Agent` sent to Proton: the user's override, or
    /// [`USER_AGENT`].
    pub fn resolved_user_agent(&self) -> &str {
        self.user_agent.as_deref().unwrap_or(USER_AGENT)
    }

    /// The effective cache budget in bytes: the user's explicit choice, or
    /// [`DEFAULT_CACHE_BUDGET_BYTES`] when unset.
    pub fn resolved_cache_budget(&self) -> u64 {
        self.cache_budget.unwrap_or(DEFAULT_CACHE_BUDGET_BYTES)
    }

    /// The effective opener policy: the user's rules, or the all-`xdg-open`
    /// default. Cloned rather than borrowed because callers hold it for the
    /// life of a window, long after the config value is gone.
    pub fn resolved_open_with(&self) -> OpenWith {
        self.open_with.clone().unwrap_or_default()
    }

    /// The effective prompt settings, defaulted when the section is absent.
    pub fn resolved_prompt(&self) -> PromptConfig {
        self.prompt.clone().unwrap_or_default()
    }

    /// The effective conflict-sweep mode: the [`CONFLICT_SWEEP_ENV`] override if
    /// it parses, then the user's stored choice, then [`SweepMode::default`].
    ///
    /// The environment wins so a support case can stop a destructive background
    /// loop without editing config or reinstalling (`docs/BUGS.md` B71). An
    /// unparseable value is ignored rather than treated as "off", so a typo
    /// cannot silently change behaviour in either direction.
    pub fn resolved_conflict_sweep(&self) -> SweepMode {
        resolve_sweep_mode(
            std::env::var(CONFLICT_SWEEP_ENV).ok().as_deref(),
            self.conflict_sweep,
        )
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

/// Apply `edit` to the ignore patterns in effect in the config at `path`,
/// under the config lock, and return the saved config. A list equal to the
/// built-in one is stored as unset, so it follows the built-in list in later
/// releases.
pub fn change_ignore_patterns_at(
    path: &Path,
    edit: impl FnOnce(&mut Vec<String>),
) -> Result<AppConfig> {
    update_config_at(path, |config| {
        let mut list = config.resolved_ignore_patterns();
        edit(&mut list);
        let builtin = list.len() == DEFAULT_IGNORE_PATTERNS.len()
            && list
                .iter()
                .zip(DEFAULT_IGNORE_PATTERNS)
                .all(|(a, b)| a == b);
        config.ignore_patterns = (!builtin).then_some(list);
        config.clone()
    })
}

/// Fail when the config file at `path` exists but does not parse, so that
/// [`AppDirs::save_config`] never writes over settings it could not read.
fn refuse_to_replace_unreadable(path: &Path) -> Result<()> {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<AppConfig>(&content) {
            Ok(_) => Ok(()),
            Err(e) => Err(Error::Other(format!(
                "{} cannot be read ({e}); fix it or move it aside before changing settings",
                path.display()
            ))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// [`AppDirs::load_config`] for the file at `path`.
fn load_config_at(path: &Path) -> AppConfig {
    if path.exists() {
        match std::fs::read_to_string(path)
            .map_err(Error::from)
            .and_then(|content| serde_json::from_str::<AppConfig>(&content).map_err(Error::from))
        {
            Ok(config) => return config,
            Err(e) => {
                // Preserve the original bytes for diagnosis/recovery.  In
                // particular, never replace a truncated config with defaults
                // and silently lose mount, device, or ignore settings.
                // `save_config` refuses to write over it for the same reason.
                tracing::error!(path = %path.display(), error = %e, "config is unreadable; using in-memory defaults without overwriting it");
                return AppConfig::default();
            }
        }
    }

    let default_config = AppConfig::default();
    if let Err(e) = save_config_at(path, &default_config) {
        tracing::warn!(path = %path.display(), error = %e, "could not write the default config");
    }
    default_config
}

/// [`AppDirs::save_config`] for the file at `path`.
fn save_config_at(path: &Path, config: &AppConfig) -> Result<()> {
    refuse_to_replace_unreadable(path)?;
    proton_base_rs::atomic::write(path, &serde_json::to_vec_pretty(config)?)?;
    Ok(())
}

/// [`AppDirs::update_config`] for the file at `path`.
fn update_config_at<R>(path: &Path, change: impl FnOnce(&mut AppConfig) -> R) -> Result<R> {
    let _lock = lock_config(path)?;
    // Refuse before reading: an unreadable file loads as defaults, and the
    // change must not be applied to those.
    refuse_to_replace_unreadable(path)?;
    let mut config = load_config_at(path);
    let out = change(&mut config);
    save_config_at(path, &config)?;
    Ok(out)
}

/// How long [`lock_config`] waits for another writer. A change holds the lock
/// for one small read and write; anything longer is a stuck process, and the
/// setting is better reported as not saved than waited on forever.
const CONFIG_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Take the lock that orders changes to the config file at `path`, held until
/// the returned file is dropped.
///
/// A separate `config.json.lock`, never the config file itself: that file is
/// replaced by a rename on every save, so a lock on it would guard a file
/// nobody reads any more.
fn lock_config(path: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(PathBuf::from(name))?;
    let deadline = std::time::Instant::now() + CONFIG_LOCK_WAIT;
    loop {
        // SAFETY: `file` owns the descriptor and outlives the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(Error::Io(e));
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Other(format!(
                "{} is being changed by another process; try again",
                path.display()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Resolved XDG locations for state, cache, and the default mountpoint.
pub struct AppDirs {
    dirs: ProjectDirs,
}

impl AppDirs {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("io", "narl", &app_dir_name())
            .ok_or_else(|| Error::Other("cannot resolve home directory".into()))?;
        Ok(Self { dirs })
    }

    /// Configuration file path (e.g. ~/.config/proton-drive-linux/config.json).
    pub fn config_path(&self) -> PathBuf {
        self.dirs.config_dir().join("config.json")
    }

    /// Load config from disk, creating default if missing.
    pub fn load_config(&self) -> AppConfig {
        load_config_at(&self.config_path())
    }

    /// Persist `config` to [`config_path`](Self::config_path), creating the
    /// config dir if missing.
    ///
    /// Refuses while the file on disk does not parse: [`load_config`](Self::load_config)
    /// gives defaults for a file it cannot read, so the first change after the
    /// file was damaged would write defaults over the mountpoint, the device
    /// and the ignore rules the user had (`docs/BUGS.md` B223).
    ///
    /// Writes the whole file. To change a setting, use
    /// [`update_config`](Self::update_config), which does not lose another
    /// process's change.
    pub fn save_config(&self, config: &AppConfig) -> Result<()> {
        save_config_at(&self.config_path(), config)
    }

    /// Change settings in place: read the file, apply `change`, write it back,
    /// all under a lock, and return what `change` returned.
    ///
    /// The app, the tray and the daemon each write the file. Without the lock,
    /// two of them changing different settings at once both read the old file,
    /// and the one that wrote last undid the other's change (`docs/BUGS.md`
    /// B292).
    pub fn update_config<R>(&self, change: impl FnOnce(&mut AppConfig) -> R) -> Result<R> {
        update_config_at(&self.config_path(), change)
    }

    /// Apply `edit` to the ignore patterns in effect for every synced folder
    /// and save them, returning the saved config. See
    /// [`change_ignore_patterns_at`].
    pub fn change_ignore_patterns(&self, edit: impl FnOnce(&mut Vec<String>)) -> Result<AppConfig> {
        change_ignore_patterns_at(&self.config_path(), edit)
    }

    /// This machine's id (e.g. ~/.config/proton-drive-linux/machine-id). See
    /// [`machine_id`](Self::machine_id).
    pub fn machine_id_path(&self) -> PathBuf {
        self.dirs.config_dir().join("machine-id")
    }

    /// A random id for this machine, made on first use and kept in
    /// [`machine_id_path`](Self::machine_id_path).
    ///
    /// It names this machine's profile in a device it may share with other
    /// computers. Random rather than `/etc/machine-id`, so nothing that
    /// identifies the host goes to Drive. In a file of its own rather than
    /// `config.json`, because the app saves the config from a copy it loaded
    /// earlier, and that would drop an id the daemon had just added.
    pub fn machine_id(&self) -> Result<String> {
        let path = self.machine_id_path();
        match std::fs::read_to_string(&path) {
            Ok(id) if is_machine_id(id.trim()) => return Ok(id.trim().to_string()),
            Ok(_) => {
                tracing::warn!(path = %path.display(), "machine id is unusable; making a new one");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut bytes = [0u8; 16];
        use std::io::Read as _;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        // Not secret, and other tools may read it; keep it world-readable.
        proton_base_rs::atomic::write_with_mode(&path, format!("{id}\n").as_bytes(), 0o644)?;
        Ok(id)
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

    /// The Google Photos import's last run: its archives and how it ended, so
    /// an import cut short by a restart can be resumed.
    pub fn takeout_import_path(&self) -> PathBuf {
        self.state_dir().join("takeout-import.json")
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

    /// Default mountpoint when the user does not pass one explicitly:
    /// `~/ProtonDrive`, or `~/ProtonDrive-<instance>` for a named [`instance`].
    pub fn default_mountpoint(&self) -> PathBuf {
        let name = instance_suffixed("ProtonDrive", '-');
        directories::UserDirs::new()
            .map(|u| u.home_dir().join(&name))
            .unwrap_or_else(|| PathBuf::from("/tmp").join(&name))
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
    /// not something this client established (`docs/BUGS.md` B6).
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

/// Whether `id` is what [`AppDirs::machine_id`] writes: 32 lowercase hex
/// digits.
fn is_machine_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
/// other file (`docs/BUGS.md` B6).
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

    #[test]
    fn an_instance_name_is_short_lowercase_ascii() {
        for good in ["test", "bench-1", "a", &"x".repeat(32)] {
            assert!(is_instance_name(good), "{good:?}");
        }
        for bad in ["", "Test", "a/b", "../x", "a b", "ü", &"x".repeat(33)] {
            assert!(!is_instance_name(bad), "{bad:?}");
        }
    }

    /// The test process has no `PDFS_INSTANCE`, so it is the default instance
    /// and every name is the one releases before 4.0 used.
    #[test]
    fn the_default_instance_keeps_the_old_names() {
        assert_eq!(instance(), None);
        assert_eq!(keyring_service(), "proton-drive-linux");
        assert_eq!(app_dir_name(), "proton-drive-linux");
        assert!(
            AppDirs::new()
                .unwrap()
                .default_mountpoint()
                .ends_with("ProtonDrive")
        );
    }

    /// Two writers changing different settings at once both keep their change
    /// (`docs/BUGS.md` B292).
    #[test]
    fn concurrent_changes_to_different_settings_are_all_kept() {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-cfg-race-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let a = {
            let path = path.clone();
            std::thread::spawn(move || {
                for i in 1..=100u64 {
                    update_config_at(&path, |c| c.upload_limit = Some(i)).unwrap();
                }
            })
        };
        let b = {
            let path = path.clone();
            std::thread::spawn(move || {
                for i in 1..=100u64 {
                    update_config_at(&path, |c| c.download_limit = Some(i)).unwrap();
                }
            })
        };
        a.join().unwrap();
        b.join().unwrap();

        let config = load_config_at(&path);
        assert_eq!(config.upload_limit, Some(100));
        assert_eq!(config.download_limit, Some(100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignore_patterns_back_at_the_built_in_list_are_stored_as_unset() {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-cfg-ignore-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let config = change_ignore_patterns_at(&path, |list| list.push("*.db".into())).unwrap();
        assert_eq!(
            config.ignore_patterns.as_ref().unwrap().last().unwrap(),
            "*.db"
        );
        let config = change_ignore_patterns_at(&path, |list| list.retain(|p| p != "*.db")).unwrap();
        assert_eq!(config.ignore_patterns, None);
        assert_eq!(load_config_at(&path).ignore_patterns, None);
        // An empty list is a choice, not the built-in one.
        let config = change_ignore_patterns_at(&path, Vec::clear).unwrap();
        assert_eq!(config.ignore_patterns, Some(Vec::new()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_change_is_never_applied_over_an_unreadable_config() {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-cfg-update-guard-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, br#"{"mountpoint": "/home/me/Pro"#).unwrap();

        assert!(update_config_at(&path, |c| c.tray_hidden = true).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            br#"{"mountpoint": "/home/me/Pro"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A damaged config must not be replaced by defaults the next time a
    /// setting changes (`docs/BUGS.md` B223).
    #[test]
    fn an_unreadable_config_is_never_written_over() {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-cfg-guard-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        assert!(refuse_to_replace_unreadable(&path).is_ok(), "no file yet");
        std::fs::write(&path, serde_json::to_vec(&AppConfig::default()).unwrap()).unwrap();
        assert!(refuse_to_replace_unreadable(&path).is_ok());
        std::fs::write(&path, br#"{"mountpoint": "/home/me/Pro"#).unwrap();
        assert!(refuse_to_replace_unreadable(&path).is_err(), "truncated");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `docs/BUGS.md` B6. A permissive mode here is not a cosmetic problem: these
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
            app_version: Some("external-drive-test-client@1.0.0".to_string()),
            user_agent: Some("test-agent/1.0".to_string()),
            cache_budget: Some(1234),
            upload_limit: Some(500_000),
            download_limit: None,
            mountpoint: Some("/tmp/x".to_string()),
            ignore_patterns: Some(vec!["build/".to_string()]),
            device_uid: Some("dev-uid".to_string()),
            conflict_sweep: Some(SweepMode::Enforce),
            open_with: None,
            prompt: None,
            proton_theme: Some(true),
            files_view: FilesView {
                list: true,
                sort: FileSort::Modified,
                descending: true,
                folders_first: false,
                icon_size: Some(192),
            },
            photos_view: PhotosView {
                row_height: Some(240),
                square: true,
            },
            window: WindowState {
                width: 1200,
                height: 800,
                maximized: true,
            },
            tray_hidden: true,
            language: Some("de".to_string()),
            online_map: true,
            local_first: Some(false),
            small_uploads: Some(false),
            update_check: true,
            update_last_checked: Some(1_760_000_000),
            update_latest: Some("3.6.0".to_string()),
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.resolved_app_version(),
            "external-drive-test-client@1.0.0"
        );
        assert_eq!(decoded.resolved_user_agent(), "test-agent/1.0");
        assert_eq!(decoded.proton_theme, Some(true));
        assert!(decoded.tray_hidden);
        assert!(decoded.online_map);
        assert!(decoded.update_check);
        assert_eq!(decoded.update_last_checked, Some(1_760_000_000));
        assert_eq!(decoded.update_latest.as_deref(), Some("3.6.0"));
        assert_eq!(decoded.language.as_deref(), Some("de"));
        assert_eq!(decoded.upload_limit, Some(500_000));
        assert_eq!(decoded.small_uploads, Some(false));
        assert_eq!(decoded.download_limit, None);
        assert_eq!(decoded.files_view, config.files_view);
        assert_eq!(decoded.photos_view, config.photos_view);
        assert_eq!(decoded.window, config.window);
    }

    #[test]
    fn a_config_without_a_window_opens_at_the_default_size() {
        let decoded: AppConfig =
            serde_json::from_str(r#"{"app_version":"a","user_agent":"b"}"#).unwrap();
        assert_eq!(decoded.window, WindowState::default());
        let partial: WindowState = serde_json::from_str(r#"{"maximized":true}"#).unwrap();
        assert_eq!((partial.width, partial.height), (980, 680));
        assert!(partial.maximized);
    }

    #[test]
    fn a_window_saved_too_small_opens_at_the_default_size() {
        let tiny = WindowState {
            width: 0,
            height: 500,
            maximized: true,
        };
        assert_eq!(
            tiny.usable(),
            WindowState {
                maximized: true,
                ..WindowState::default()
            }
        );
        let fine = WindowState {
            width: 640,
            height: 480,
            maximized: false,
        };
        assert_eq!(fine.usable(), fine);
    }

    #[test]
    fn a_config_without_a_files_view_gets_the_default() {
        let decoded: AppConfig =
            serde_json::from_str(r#"{"app_version":"a","user_agent":"b"}"#).unwrap();
        assert_eq!(decoded.files_view, FilesView::default());
        let partial: FilesView = serde_json::from_str(r#"{"sort":"size"}"#).unwrap();
        assert_eq!(partial.sort, FileSort::Size);
        assert!(partial.folders_first);
        assert_eq!(partial.icon_size, None);
    }

    #[test]
    fn a_config_without_a_photos_view_gets_the_default() {
        let decoded: AppConfig =
            serde_json::from_str(r#"{"app_version":"a","user_agent":"b"}"#).unwrap();
        assert_eq!(decoded.photos_view, PhotosView::default());
        let partial: PhotosView = serde_json::from_str(r#"{"square":true}"#).unwrap();
        assert!(partial.square);
        assert_eq!(partial.row_height, None);
    }

    #[test]
    fn the_old_proton_accent_setting_turns_on_the_proton_theme() {
        let decoded: AppConfig =
            serde_json::from_str(r#"{"app_version":"a","user_agent":"b","proton_accent":true}"#)
                .unwrap();
        assert_eq!(decoded.proton_theme, Some(true));
    }

    #[test]
    fn test_default_app_config() {
        let default_config = AppConfig::default();
        assert_eq!(default_config.resolved_app_version(), APP_VERSION);
        assert_eq!(default_config.resolved_user_agent(), USER_AGENT);
    }

    #[test]
    fn the_client_identity_follows_the_release() {
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            APP_VERSION,
            format!("external-drive-linux@{version}-stable")
        );
        assert_eq!(USER_AGENT, format!("proton-drive-linux/{version}"));
    }

    #[test]
    fn an_old_config_with_the_fixed_identity_follows_the_release() {
        // Every config written up to 3.1.0 holds these two values, so they must
        // not pin the identity once Proton retires 0.1.0.
        let json = format!(
            r#"{{"app_version":"{LEGACY_APP_VERSION}","user_agent":"{LEGACY_USER_AGENT}","tray_hidden":true}}"#
        );
        let decoded: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.resolved_app_version(), APP_VERSION);
        assert_eq!(decoded.resolved_user_agent(), USER_AGENT);
        assert!(decoded.tray_hidden, "the rest of the config survives");

        let saved = serde_json::to_string(&decoded).unwrap();
        assert!(!saved.contains("app_version"), "{saved}");
        assert!(!saved.contains("user_agent"), "{saved}");
    }

    #[test]
    fn a_hand_set_client_identity_is_kept() {
        let json = r#"{"app_version":"external-drive-linux@9.9.9-beta","user_agent":"custom/1"}"#;
        let decoded: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            decoded.resolved_app_version(),
            "external-drive-linux@9.9.9-beta"
        );
        assert_eq!(decoded.resolved_user_agent(), "custom/1");
        let saved = serde_json::to_string(&decoded).unwrap();
        assert!(saved.contains("external-drive-linux@9.9.9-beta"), "{saved}");
    }

    #[test]
    fn a_config_without_a_client_identity_loads() {
        let decoded: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(decoded.resolved_app_version(), APP_VERSION);
        assert_eq!(decoded.resolved_user_agent(), USER_AGENT);
    }

    #[test]
    fn sweep_mode_defaults_to_report_only() {
        // The sweep trashes user files, so an installed config that has never
        // heard of the setting must not get the enforcing behaviour.
        assert_eq!(SweepMode::default(), SweepMode::Report);
        assert!(!SweepMode::default().is_enforcing());
        assert_eq!(resolve_sweep_mode(None, None), SweepMode::Report);
    }

    #[test]
    fn sweep_mode_parses_operator_spellings() {
        for s in ["off", "OFF", " no ", "false", "0", "disabled"] {
            assert_eq!(SweepMode::parse(s), Some(SweepMode::Off), "{s}");
        }
        for s in ["report", "dry-run", "dryrun", "observe"] {
            assert_eq!(SweepMode::parse(s), Some(SweepMode::Report), "{s}");
        }
        for s in ["enforce", "on", "yes", "true", "1"] {
            assert_eq!(SweepMode::parse(s), Some(SweepMode::Enforce), "{s}");
        }
        // A typo must not resolve to anything, so it falls through to the config.
        assert_eq!(SweepMode::parse("offf"), None);
        assert_eq!(SweepMode::parse(""), None);
    }

    #[test]
    fn sweep_mode_env_overrides_config_but_a_typo_does_not() {
        let stored = Some(SweepMode::Enforce);
        // The point of the override: kill a destructive loop without editing config.
        assert_eq!(resolve_sweep_mode(Some("off"), stored), SweepMode::Off);
        // Unrecognised value falls back to the stored choice rather than guessing.
        assert_eq!(resolve_sweep_mode(Some("nope"), stored), SweepMode::Enforce);
        assert_eq!(resolve_sweep_mode(None, stored), SweepMode::Enforce);
        // No env, no stored value: report-only.
        assert_eq!(resolve_sweep_mode(Some("nope"), None), SweepMode::Report);
    }

    #[test]
    fn a_config_predating_conflict_sweep_still_loads() {
        let json = r#"{"app_version":"v","user_agent":"u"}"#;
        let decoded: AppConfig = serde_json::from_str(json).unwrap();
        assert!(decoded.conflict_sweep.is_none());
    }

    #[test]
    fn sweep_mode_round_trips_through_the_config_file() {
        let config = AppConfig {
            conflict_sweep: Some(SweepMode::Enforce),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""conflict_sweep":"enforce""#), "{json}");
        let decoded: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.conflict_sweep, Some(SweepMode::Enforce));
    }

    #[test]
    fn a_machine_id_is_32_lowercase_hex_digits() {
        assert!(is_machine_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_machine_id("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_machine_id("0123456789abcdef"));
        assert!(!is_machine_id("../../../../etc/passwd/0123456789"));
        assert!(!is_machine_id(""));
    }

    #[test]
    fn a_config_predating_ignore_patterns_still_loads() {
        // Every installed config on disk looks like this; the field must not be
        // required or the daemon would fall back to a default config on upgrade.
        let json = r#"{"app_version":"v","user_agent":"u"}"#;
        let decoded: AppConfig = serde_json::from_str(json).unwrap();
        assert!(decoded.ignore_patterns.is_none());
        assert!(!decoded.tray_hidden, "an old config keeps the tray icon");
        assert!(
            decoded.language.is_none(),
            "an old config follows the system"
        );
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
