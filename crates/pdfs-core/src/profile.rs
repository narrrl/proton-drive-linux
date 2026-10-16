//! The machine profile that travels with a device.
//!
//! Sync folder mappings, pins, and the settings that shape them live in
//! `cache.db` and `config.json` — both on the local disk. When that disk dies,
//! the remote data survives but the *arrangement* of it does not: which folders
//! were synced, where they lived locally, which were on-demand, what was pinned.
//!
//! So the daemon mirrors that arrangement into a `profile.json` in its device's
//! root folder on Drive. It goes up the normal upload path, which means it is
//! end-to-end encrypted with the account keys like any other file — 5.3's
//! "encrypt with the user's Proton keys" is satisfied by *not* special-casing
//! it. It lives under the device rather than the Drive root because it describes
//! that device; adopting the device is what makes it relevant.
//!
//! This module is pure data. The upload/download and the restore flow live in
//! `pdfs-fuse`, next to the client that performs them.

use serde::{Deserialize, Serialize};

use crate::mounts::MountMode;

/// File name of the profile clients before 3.5.0 wrote, one per device.
///
/// Read, never written: two computers sharing a device each overwrote the
/// other's. See [`profile_file_name`].
pub const PROFILE_FILE_NAME: &str = "profile.json";

/// File name of one machine's profile: `profile-<machine id>.json`.
///
/// One per machine, because several computers may sync into one device, and
/// each describes its own arrangement. A single shared file went to whichever
/// computer saved last.
pub fn profile_file_name(machine_id: &str) -> String {
    format!("profile-{machine_id}.json")
}

/// Whether `name` is a profile: [`PROFILE_FILE_NAME`] or one machine's
/// [`profile_file_name`].
pub fn is_profile_file_name(name: &str) -> bool {
    name == PROFILE_FILE_NAME
        || name
            .strip_prefix("profile-")
            .and_then(|rest| rest.strip_suffix(".json"))
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Folder, directly under the device root, that holds the profile.
///
/// The API refuses `Cannot create file at the root of a device` (HTTP 422), so
/// the profile cannot live at the device root itself even though that is where
/// it logically belongs (docs/BUGS.md B68/B78). A dotted name keeps it out of
/// the way in the web UI, and the restore ignores it when listing the device's
/// restorable folders.
pub const PROFILE_DIR_NAME: &str = ".proton-drive-linux";

/// Format version of the profile document.
///
/// Bumped only for changes a *reader* cannot survive. Adding fields does not
/// qualify — unknown fields are ignored on the way in, so an older client reads
/// a newer profile's common subset. A newer `version` than this is refused
/// outright rather than half-applied: a restore that silently drops folders it
/// did not understand is worse than one that tells you to upgrade.
pub const PROFILE_VERSION: u32 = 1;

/// One synced folder, as it was arranged on the machine that wrote the profile.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProfileFolder {
    /// Uid of the folder's remote root, under the device root. The stable
    /// identity — the local path may not exist on the restoring machine, but
    /// this does.
    pub remote_uid: String,
    /// Absolute local path it was synced to. A *suggestion* on restore: another
    /// machine may have a different username, home, or mount layout.
    pub local_path: String,
    /// `mirror` or `ondemand`. Unknown values written by a newer client do not
    /// invalidate the rest of the profile.
    pub mode: MountMode,
}

/// One pinned node (P5 pins), as `pins` stores it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProfilePin {
    pub uid: String,
    /// Mountpoint-relative path, kept for display: a pin whose node has been
    /// deleted should still be nameable in a restore summary.
    pub path: String,
    pub recursive: bool,
}

/// The full arrangement of one machine.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub version: u32,
    /// The device this profile describes.
    pub device_uid: String,
    /// Hostname of the machine that wrote it, so a restore prompt can say
    /// *"restore settings from laptop-xyz?"* rather than quoting a uid.
    pub hostname: String,
    /// The writing machine's id, as in its [`profile_file_name`]. `None` in a
    /// profile written before 3.5.0.
    #[serde(default)]
    pub machine_id: Option<String>,
    /// When it was written, epoch seconds.
    pub saved_at: i64,
    #[serde(default)]
    pub folders: Vec<ProfileFolder>,
    #[serde(default)]
    pub pins: Vec<ProfilePin>,
    /// Global sync ignore patterns. Safe to restore as-is.
    #[serde(default)]
    pub ignore_patterns: Option<Vec<String>>,
    /// Machine-shaped settings. Carried, but never applied without asking: a
    /// laptop's 5 GB cache budget and `~/ProtonDrive` mountpoint are not
    /// obviously right on the next machine.
    #[serde(default)]
    pub cache_budget: Option<u64>,
    #[serde(default)]
    pub mountpoint: Option<String>,
}

/// The saved arrangement of the folder with uid `remote_uid`, from the
/// profiles of every computer that syncs the device.
///
/// The profile of the machine named `hostname` wins, since it describes this
/// computer; otherwise the most recently saved one that has the folder.
pub fn saved_folder<'a>(
    profiles: &'a [Profile],
    remote_uid: &str,
    hostname: &str,
) -> Option<&'a ProfileFolder> {
    let holding = |p: &&'a Profile| p.folders.iter().any(|f| f.remote_uid == remote_uid);
    let profile = profiles
        .iter()
        .filter(holding)
        .find(|p| p.hostname == hostname)
        .or_else(|| profiles.iter().filter(holding).max_by_key(|p| p.saved_at))?;
    profile.folders.iter().find(|f| f.remote_uid == remote_uid)
}

impl Profile {
    /// Parse a profile document, refusing one written by a newer client.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let profile: Profile =
            serde_json::from_slice(bytes).map_err(|e| format!("malformed profile.json: {e}"))?;
        if profile.version > PROFILE_VERSION {
            return Err(format!(
                "profile.json was written by a newer client (version {}, this client understands {PROFILE_VERSION}); upgrade to restore it",
                profile.version
            ));
        }
        Ok(profile)
    }

    /// Serialize for upload. Pretty-printed: it is a small file a user may well
    /// open in the web UI to see what their machine had.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|e| format!("serialize profile: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        Profile {
            version: PROFILE_VERSION,
            device_uid: "dev-1".to_string(),
            hostname: "laptop-xyz".to_string(),
            machine_id: Some("0123456789abcdef0123456789abcdef".to_string()),
            saved_at: 1_700_000_000,
            folders: vec![ProfileFolder {
                remote_uid: "vol~link".to_string(),
                local_path: "/home/nils/Documents".to_string(),
                mode: MountMode::Mirror,
            }],
            pins: vec![ProfilePin {
                uid: "vol~pin".to_string(),
                path: "Photos/2024".to_string(),
                recursive: true,
            }],
            ignore_patterns: Some(vec!["target/".to_string()]),
            cache_budget: Some(5_000_000_000),
            mountpoint: Some("/home/nils/ProtonDrive".to_string()),
        }
    }

    #[test]
    fn round_trips() {
        let p = sample();
        assert_eq!(Profile::parse(&p.to_bytes().unwrap()).unwrap(), p);
    }

    #[test]
    fn profile_keeps_legacy_mode_bytes_and_parses_old_documents() {
        let bytes = sample().to_bytes().unwrap();
        let json = String::from_utf8(bytes).unwrap();
        assert!(json.contains(r#""mode": "mirror""#));

        let old = br#"{
            "version": 1,
            "device_uid": "dev-1",
            "hostname": "old-client",
            "saved_at": 1,
            "folders": [
                {
                    "remote_uid": "vol~mirror",
                    "local_path": "/home/me/Mirror",
                    "mode": "mirror"
                },
                {
                    "remote_uid": "vol~ondemand",
                    "local_path": "/home/me/Cloud",
                    "mode": "ondemand"
                }
            ]
        }"#;
        let profile = Profile::parse(old).unwrap();
        assert_eq!(profile.folders[0].mode, MountMode::Mirror);
        assert_eq!(profile.folders[1].mode, MountMode::OnDemand);
    }

    #[test]
    fn unknown_folder_mode_does_not_reject_profile() {
        let json = br#"{
            "version": 1,
            "device_uid": "dev-1",
            "hostname": "future-client",
            "saved_at": 1,
            "folders": [{
                "remote_uid": "vol~future",
                "local_path": "/home/me/Future",
                "mode": "streaming"
            }]
        }"#;
        let profile = Profile::parse(json).unwrap();
        assert_eq!(profile.folders[0].mode, MountMode::Unknown);
    }

    /// A field added by a later client must not break this one's restore: the
    /// common subset is still usable, which is the whole reason `version` is
    /// reserved for breaking changes only.
    #[test]
    fn unknown_fields_are_ignored() {
        let json = br#"{
            "version": 1,
            "device_uid": "dev-1",
            "hostname": "laptop-xyz",
            "saved_at": 1,
            "folders": [],
            "something_from_the_future": {"a": 1}
        }"#;
        let p = Profile::parse(json).unwrap();
        assert_eq!(p.device_uid, "dev-1");
        assert!(p.folders.is_empty());
    }

    /// Missing optional sections read as empty rather than failing: a profile
    /// written before pins existed is still a valid folder mapping.
    #[test]
    fn absent_sections_default() {
        let json = br#"{"version":1,"device_uid":"d","hostname":"h","saved_at":0}"#;
        let p = Profile::parse(json).unwrap();
        assert!(p.pins.is_empty());
        assert!(p.ignore_patterns.is_none());
    }

    #[test]
    fn a_profile_name_is_the_legacy_one_or_one_per_machine() {
        assert!(is_profile_file_name(PROFILE_FILE_NAME));
        let name = profile_file_name("0123456789abcdef0123456789abcdef");
        assert_eq!(name, "profile-0123456789abcdef0123456789abcdef.json");
        assert!(is_profile_file_name(&name));
        assert!(!is_profile_file_name("profile-.json"));
        assert!(!is_profile_file_name("profile-a/b.json"));
        assert!(!is_profile_file_name("profile (1).json"));
        assert!(!is_profile_file_name("notes.json"));
    }

    /// A profile written before 3.5.0 has no machine id and still parses.
    #[test]
    fn a_profile_without_a_machine_id_parses() {
        let json = br#"{"version":1,"device_uid":"d","hostname":"h","saved_at":0}"#;
        assert_eq!(Profile::parse(json).unwrap().machine_id, None);
    }

    /// Several computers sync one device, each with its own profile. This
    /// computer's own wins, then the newest that has the folder.
    #[test]
    fn the_saved_folder_comes_from_this_computer_then_the_newest_profile() {
        let profile = |hostname: &str, saved_at: i64, path: &str| Profile {
            hostname: hostname.to_string(),
            saved_at,
            folders: vec![ProfileFolder {
                remote_uid: "vol~docs".to_string(),
                local_path: path.to_string(),
                mode: MountMode::Mirror,
            }],
            ..sample()
        };
        let profiles = vec![
            profile("desktop", 10, "/home/a/Docs"),
            profile("laptop", 5, "/home/b/Docs"),
            profile("old", 1, "/home/c/Docs"),
        ];
        let path =
            |host: &str| saved_folder(&profiles, "vol~docs", host).map(|f| f.local_path.as_str());
        assert_eq!(path("laptop"), Some("/home/b/Docs"));
        assert_eq!(path("new-machine"), Some("/home/a/Docs"));
        assert_eq!(saved_folder(&profiles, "vol~other", "laptop"), None);
        assert_eq!(saved_folder(&[], "vol~docs", "laptop"), None);
    }

    #[test]
    fn future_version_is_refused() {
        let mut p = sample();
        p.version = PROFILE_VERSION + 1;
        let err = Profile::parse(&p.to_bytes().unwrap()).unwrap_err();
        assert!(err.contains("newer client"), "{err}");
    }
}
