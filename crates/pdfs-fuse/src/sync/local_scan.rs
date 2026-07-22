//! Local open-for-write detection used to defer torn uploads.

use super::*;

// ---- open-for-write detection ---------------------------------------------

/// Scan `/proc/*/fd` once and return the set of **canonical** paths under
/// `root` that any process currently holds open for writing (`O_WRONLY` or
/// `O_RDWR`). The scan is best-effort: unreadable `/proc/*/fd` directories
/// (other users' processes, or kernel threads) are silently skipped.
///
/// Cost: one `readdir` of `/proc`, then for every process one `readdir` of its
/// `/proc/<pid>/fd/`, and one `readlink` + one `open` of `/proc/<pid>/fdinfo/<n>`
/// per open fd that resolves under `root`. For a typical desktop with a few
/// hundred processes and a handful of files inside the sync root, this
/// completes in low single-digit milliseconds.
pub(super) fn open_for_write_set(root: &Path) -> HashSet<PathBuf> {
    let mut result = HashSet::new();
    let Ok(root_canonical) = std::fs::canonicalize(root) else {
        return result;
    };
    let Ok(proc_entries) = std::fs::read_dir("/proc") else {
        return result;
    };
    for proc_entry in proc_entries.filter_map(|e| e.ok()) {
        let pid_name = proc_entry.file_name();
        // Skip non-numeric entries (kernel threads, /proc/self, etc.)
        if !pid_name
            .to_str()
            .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let fd_dir = PathBuf::from("/proc").join(&pid_name).join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd_entry in fds.filter_map(|e| e.ok()) {
            // readlink on /proc/<pid>/fd/<n> gives the target path.
            let link = fd_dir.join(fd_entry.file_name());
            let Ok(target) = std::fs::read_link(&link) else {
                continue;
            };
            // Quick prefix check before the more expensive fdinfo read.
            if !target.starts_with(&root_canonical) {
                continue;
            }
            // Read the fd's flags from /proc/<pid>/fdinfo/<n> to determine
            // the access mode. The first line is "pos:\t<offset>", the second
            // is "flags:\t<octal>". We only need the low two bits of flags:
            //   0 = O_RDONLY, 1 = O_WRONLY, 2 = O_RDWR
            let fdinfo_path = PathBuf::from("/proc")
                .join(&pid_name)
                .join("fdinfo")
                .join(fd_entry.file_name());
            if let Ok(contents) = std::fs::read_to_string(&fdinfo_path)
                && is_write_mode(&contents)
            {
                result.insert(target);
            }
        }
    }
    result
}

/// Parse the `flags:` line out of a `/proc/<pid>/fdinfo/<n>` file and return
/// true if the low two bits indicate write access (`O_WRONLY = 1` or
/// `O_RDWR = 2`).
pub(super) fn is_write_mode(fdinfo: &str) -> bool {
    for line in fdinfo.lines() {
        if let Some(rest) = line.strip_prefix("flags:\t") {
            if let Ok(flags) = u32::from_str_radix(rest.trim_start_matches('0'), 8) {
                let access = flags & 0o3; // O_ACCMODE
                return access == 1 || access == 2; // O_WRONLY or O_RDWR
            }
            // An empty octal string ("0" stripped to "") means flags == 0 == O_RDONLY.
            return false;
        }
    }
    false
}
