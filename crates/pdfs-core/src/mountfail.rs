//! Why the daemon's mount keeps failing, for the front ends to read.
//!
//! The control socket opens only once a mount has succeeded, so a daemon whose
//! mount fails on every try had no way to say why. The app and the tray showed
//! "Connecting…" for days while the reason sat in the journal (`docs/BUGS.md`
//! B263). The daemon now writes its last failure to a small file in the state
//! directory, next to the control socket, and removes it once a mount is up.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file's name inside the state directory.
const FILE_NAME: &str = "mount-failure.json";

/// Failure to fetch My Files on a cold start with no cached root.
#[derive(Debug, thiserror::Error)]
#[error("fetch My Files root: {0}")]
pub struct RootLookup(pub proton_sdk::ProtonError);

impl RootLookup {
    pub fn needs_account_keys(&self) -> bool {
        use proton_sdk::{ProtonError, crypto::CryptoError};

        // SDK 0.12 carries rpgp's `MissingKey` as the text of Decrypt.
        // `a_real_missing_key_failure_waits_for_keys` pins that wording, so
        // an SDK or rpgp update that changes it fails the test instead of
        // silently going back to the retry loop. Never park for an unrelated
        // decryption error.
        matches!(self.0, ProtonError::KeysLocked)
            || matches!(&self.0, ProtonError::Crypto(CryptoError::Decrypt(reason)) if reason == "missing key")
    }
}

/// The last failed mount attempt.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MountFailure {
    /// The error, in English, as the journal has it.
    pub error: String,
    /// Unix seconds of the first failure in this run of failures.
    pub since: i64,
    /// Failed attempts in a row.
    pub attempts: u32,
    /// Unix seconds of the next attempt. Meaningless while `awaiting_keys`.
    pub next_attempt: i64,
    /// The daemon stopped retrying because the account keys cannot open My
    /// Files, and waits for a restart after key recovery. A separate field
    /// rather than an optional `next_attempt`, so an older version reading
    /// the file after a downgrade still understands it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_keys: bool,
    /// Set when the mount was refused because the folder already has files in
    /// it, so the app can explain that in the user's language. Absent in a
    /// file an older daemon wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_empty: Option<crate::mountpoint::NotEmpty>,
}

/// Where the failure is kept for the state directory `state_dir`.
pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

/// Record `failure`, replacing the one before it. Written to a temporary file
/// and renamed, so a reader never sees half of it.
pub fn record(state_dir: &Path, failure: &MountFailure) -> std::io::Result<()> {
    let json = serde_json::to_vec(failure).map_err(std::io::Error::other)?;
    proton_base_rs::atomic::write_with_mode(&path(state_dir), &json, 0o644)
}

/// Forget the failure: a mount is up, or a new daemon has started.
pub fn clear(state_dir: &Path) {
    let _ = std::fs::remove_file(path(state_dir));
}

/// The recorded failure, if there is one and it can be read.
pub fn read(state_dir: &Path) -> Option<MountFailure> {
    let bytes = std::fs::read(path(state_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_failure_reads_back_until_cleared() {
        let dir = std::env::temp_dir().join(format!("pdfs-mountfail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read(&dir), None);
        let failure = MountFailure {
            error: "fetch My Files root: missing key".into(),
            since: 1_700_000_000,
            attempts: 3,
            next_attempt: 1_700_000_040,
            awaiting_keys: false,
            not_empty: None,
        };
        record(&dir, &failure).unwrap();
        assert_eq!(read(&dir), Some(failure));
        clear(&dir);
        assert_eq!(read(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_an_older_daemon_wrote_still_reads() {
        let old = r#"{"error":"missing key","since":1,"attempts":2,"next_attempt":3}"#;
        let failure: MountFailure = serde_json::from_str(old).unwrap();
        assert_eq!(failure.next_attempt, 3);
        assert!(!failure.awaiting_keys);
        assert_eq!(failure.not_empty, None);
        assert_eq!(failure.attempts, 2);
    }

    /// An older daemon, after a downgrade, must still read what this one
    /// writes while it waits for key recovery.
    #[test]
    fn a_key_wait_reads_as_a_plain_failure_without_the_new_field() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OlderFailure {
            error: String,
            since: i64,
            attempts: u32,
            next_attempt: i64,
        }
        let failure = MountFailure {
            error: "fetch My Files root: missing key".into(),
            since: 1,
            attempts: 1,
            next_attempt: 1,
            awaiting_keys: true,
            not_empty: None,
        };
        let json = serde_json::to_string(&failure).unwrap();
        let older: OlderFailure = serde_json::from_str(&json).unwrap();
        assert_eq!(older.next_attempt, 1);
        let back: MountFailure = serde_json::from_str(&json).unwrap();
        assert!(back.awaiting_keys);
    }

    /// The error a key that is not the message's recipient really produces,
    /// not a hand-written payload.
    #[test]
    fn a_real_missing_key_failure_waits_for_keys() {
        use proton_sdk::crypto::{PrivateKey, generate_node_key};

        let unlock = |generated: proton_sdk::crypto::GeneratedNodeKey| {
            PrivateKey::from_armored(&generated.locked_armored, &generated.passphrase).unwrap()
        };
        let recipient = unlock(generate_node_key().unwrap());
        let stranger = unlock(generate_node_key().unwrap());
        let message = recipient.encrypt(b"root share key").unwrap();

        let single = stranger.decrypt_armored_message(&message).unwrap_err();
        assert!(RootLookup(single.into()).needs_account_keys());
        let any = proton_sdk::crypto::decrypt_armored_with_keys(&message, &[stranger]).unwrap_err();
        assert!(RootLookup(any.into()).needs_account_keys());
        assert!(
            !RootLookup(proton_sdk::ProtonError::invalid_operation("missing key"))
                .needs_account_keys()
        );
    }
}
