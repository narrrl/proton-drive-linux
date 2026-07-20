//! The machine profile that travels with a device (features.md 5.3).
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

/// File name of the profile inside the device root folder.
pub const PROFILE_FILE_NAME: &str = "profile.json";

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
    /// `mirror` or `ondemand`.
    pub mode: String,
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
            saved_at: 1_700_000_000,
            folders: vec![ProfileFolder {
                remote_uid: "vol~link".to_string(),
                local_path: "/home/nils/Documents".to_string(),
                mode: "mirror".to_string(),
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
    fn future_version_is_refused() {
        let mut p = sample();
        p.version = PROFILE_VERSION + 1;
        let err = Profile::parse(&p.to_bytes().unwrap()).unwrap_err();
        assert!(err.contains("newer client"), "{err}");
    }
}
