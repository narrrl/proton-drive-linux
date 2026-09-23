//! Unified presentation model for every local Proton Drive location.
//!
//! Device sync state remains owned by `sync_folder`; these types are the stable
//! shape exposed to front-ends after that state is joined with the `mount`
//! presentation table.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::control::SyncProgress;

/// How a location makes remote content available locally.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    /// A full local copy reconciled in both directions.
    Mirror,
    /// A FUSE session that fetches content as it is accessed.
    OnDemand,
    /// A mode written by a newer client.
    #[default]
    #[serde(other)]
    Unknown,
}

impl MountMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::OnDemand => "ondemand",
            Self::Unknown => "unknown",
        }
    }
}

impl From<&str> for MountMode {
    fn from(value: &str) -> Self {
        match value {
            "mirror" => Self::Mirror,
            "ondemand" => Self::OnDemand,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for MountMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether the location itself was declared writable.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MountAccess {
    Rw,
    Ro,
}

impl MountAccess {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rw => "rw",
            Self::Ro => "ro",
        }
    }
}

impl fmt::Display for MountAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What remote root a local location presents.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MountKind {
    /// The account's primary My Files mount.
    MyFiles,
    /// A folder owned by this machine's Proton Drive device.
    Device { sync_folder_id: i64 },
    /// A future standalone mount rooted at a node shared with this account.
    Shared { share_root_uid: String },
}

/// One local Proton Drive location as exposed over the control protocol.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MountSpec {
    /// Presentation-row id. Device actions use the `sync_folder_id` carried by
    /// [`MountKind::Device`], not this id.
    pub id: i64,
    pub kind: MountKind,
    pub local_path: String,
    pub root_uid: String,
    pub root_share_id: String,
    pub mode: MountMode,
    pub access: MountAccess,
    /// `idle` | `syncing` | `error` | `conflict` for device locations.
    pub state: String,
    /// Last successful device sync, epoch seconds; `0` when not applicable.
    pub last_sync: i64,
    /// A device mode switch accepted but not yet applied.
    #[serde(default)]
    pub pending_mode: Option<MountMode>,
    /// Whether a FUSE session currently owns `local_path`.
    #[serde(default)]
    pub mounted: bool,
    /// Live device sync progress, absent when no pass is running.
    #[serde(default)]
    pub progress: Option<SyncProgress>,
    /// A synced folder paused on its own. Live daemon state, not a stored column.
    #[serde(default)]
    pub paused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_mode_keeps_legacy_wire_strings() {
        assert_eq!(
            serde_json::to_string(&MountMode::Mirror).unwrap(),
            r#""mirror""#
        );
        assert_eq!(
            serde_json::to_string(&MountMode::OnDemand).unwrap(),
            r#""ondemand""#
        );
        assert_eq!(
            serde_json::from_str::<MountMode>(r#""mirror""#).unwrap(),
            MountMode::Mirror
        );
        assert_eq!(
            serde_json::from_str::<MountMode>(r#""ondemand""#).unwrap(),
            MountMode::OnDemand
        );
    }

    #[test]
    fn mount_mode_tolerates_unknown_values() {
        assert_eq!(
            serde_json::from_str::<MountMode>(r#""from-the-future""#).unwrap(),
            MountMode::Unknown
        );
    }
}
