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

/// The last failed mount attempt.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MountFailure {
    /// The error, in English, as the journal has it.
    pub error: String,
    /// Unix seconds of the first failure in this run of failures.
    pub since: i64,
    /// Failed attempts in a row.
    pub attempts: u32,
    /// Unix seconds of the next attempt.
    pub next_attempt: i64,
}

/// Where the failure is kept for the state directory `state_dir`.
pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

/// Record `failure`, replacing the one before it. Written to a temporary file
/// and renamed, so a reader never sees half of it.
pub fn record(state_dir: &Path, failure: &MountFailure) -> std::io::Result<()> {
    let json = serde_json::to_vec(failure).map_err(std::io::Error::other)?;
    let tmp = state_dir.join(format!("{FILE_NAME}.tmp"));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path(state_dir))
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
        };
        record(&dir, &failure).unwrap();
        assert_eq!(read(&dir), Some(failure));
        clear(&dir);
        assert_eq!(read(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
