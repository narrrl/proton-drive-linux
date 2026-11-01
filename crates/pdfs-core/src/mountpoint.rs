//! Whether a folder can take the My files mount without hiding anything.
//!
//! FUSE mounts over a folder that already has files in it without a word. The
//! files then sit under the mount, out of sight: not uploaded, and back only
//! once the service stops, so they look deleted (`docs/BUGS.md` B277, #67).
//! The daemon refuses such a folder, and the app refuses it when it is picked.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file system type the kernel reports for this client's own mounts.
pub const OWN_FS_TYPE: &str = "fuse.protondrive";

/// How many of a folder's entries an error names.
const NAMED_ENTRIES: usize = 3;

/// A folder that already has something in it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NotEmpty {
    /// The folder.
    pub path: PathBuf,
    /// The first few names in it, sorted.
    pub entries: Vec<String>,
    /// How many entries it has in all.
    pub count: usize,
}

impl std::fmt::Display for NotEmpty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not empty (holds ", self.path.display())?;
        write!(f, "{}", self.entries.join(", "))?;
        let rest = self.count.saturating_sub(self.entries.len());
        if rest > 0 {
            write!(f, " and {rest} more")?;
        }
        write!(
            f,
            "); My files would hide those files. Move them out, or choose another folder for My files"
        )
    }
}

impl std::error::Error for NotEmpty {}

/// What is in `dir`, if anything. A folder that does not exist yet is empty.
/// Hidden entries count: a `.directory` or a `.Trash-1000` hidden under the
/// mount is as lost as any other file.
pub fn not_empty(dir: &Path) -> std::io::Result<Option<NotEmpty>> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut names = read
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    if names.is_empty() {
        return Ok(None);
    }
    names.sort();
    let count = names.len();
    names.truncate(NAMED_ENTRIES);
    Ok(Some(NotEmpty {
        path: dir.to_path_buf(),
        entries: names,
        count,
    }))
}

/// The file system type mounted at `path`, as `/proc/self/mountinfo` names it
/// (`ext4`, `fuse.protondrive`, …), or `None` when nothing is mounted there.
pub fn mounted_fs_type(path: &Path) -> Option<String> {
    let wanted = path.canonicalize().ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mounts
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let separator = fields.iter().position(|field| *field == "-")?;
            let encoded = *fields.get(4)?;
            let decoded = encoded.replace("\\040", " ").replace("\\011", "\t");
            let mountpoint = PathBuf::from(decoded);
            if mountpoint.canonicalize().ok()? == wanted {
                Some(fields.get(separator + 1)?.to_string())
            } else {
                None
            }
        })
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pdfs-mountpoint-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_or_missing_folder_takes_the_mount() {
        let dir = temp_dir("empty");
        assert_eq!(not_empty(&dir).unwrap(), None);
        assert_eq!(not_empty(&dir.join("missing")).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_with_files_is_refused_and_named() {
        let dir = temp_dir("files");
        for name in ["report.pdf", "Notes", ".directory", "zeta.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let found = not_empty(&dir).unwrap().expect("must refuse");
        assert_eq!(found.count, 4);
        assert_eq!(found.entries, [".directory", "Notes", "report.pdf"]);
        let message = found.to_string();
        assert!(message.contains("is not empty"), "{message}");
        assert!(
            message.contains(".directory, Notes, report.pdf and 1 more"),
            "{message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hidden_file_alone_is_enough_to_refuse() {
        let dir = temp_dir("hidden");
        std::fs::write(dir.join(".Trash-1000"), b"").unwrap();
        let found = not_empty(&dir).unwrap().expect("must refuse");
        assert_eq!(found.count, 1);
        assert!(!found.to_string().contains("more"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_folder_is_not_a_mount() {
        let dir = temp_dir("plain");
        assert_eq!(mounted_fs_type(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
