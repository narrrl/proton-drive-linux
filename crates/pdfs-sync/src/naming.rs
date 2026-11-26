//! How conflict copies are named, here and on Drive.

use std::path::Path;
use std::sync::OnceLock;

/// A variant of `name` to fall back on when the remote already has that name.
///
/// Deliberately the same shape the sync engine uses for its conflict copies
/// (`sync.rs`, `conflict_path`), so the two halves of the product name the same
/// situation the same way and a user only has to learn it once.
pub fn conflict_name(name: &str, stamp: i64) -> String {
    conflict_name_as(name, conflict_label(), stamp)
}

/// [`conflict_name`] with the machine's label given.
pub fn conflict_name_as(name: &str, label: &str, stamp: i64) -> String {
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let tag = conflict_tag(label, stamp, 0);
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => format!("{stem} (sync-conflict {tag}).{ext}"),
        None => format!("{stem} (sync-conflict {tag})"),
    }
}

/// This machine's name as its conflict copies carry it (`docs/BUGS.md` B251).
///
/// Two computers syncing one folder that each keep a copy of the same file in
/// the same second would otherwise make the same name on Drive, and nobody
/// could tell which computer a copy came from. Read once, from the hostname.
pub fn conflict_label() -> &'static str {
    static LABEL: OnceLock<String> = OnceLock::new();
    LABEL.get_or_init(|| conflict_label_of(&this_hostname()))
}

/// `host` as one token: anything but ASCII letters, digits, `-`, `_` and `.`
/// becomes `-`, so the label holds no space for `conflict_base_name` to trip
/// on and no `/` or `)` to break the name.
pub fn conflict_label_of(host: &str) -> String {
    let label: String = host
        .chars()
        .take(32)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    label.trim_matches('-').to_string()
}

/// The inside of a conflict copy's marker: `<label> <stamp>`, with `-<n>`
/// when an earlier copy already holds that name. Without a label it is the
/// bare stamp that copies had before B251.
pub fn conflict_tag(label: &str, stamp: i64, suffix: u32) -> String {
    let suffix = if suffix == 0 {
        String::new()
    } else {
        format!("-{suffix}")
    };
    if label.is_empty() {
        format!("{stamp}{suffix}")
    } else {
        format!("{label} {stamp}{suffix}")
    }
}

/// This machine's hostname, used to name (and later recover) its Proton Drive
/// Device. Reads the live kernel hostname, falling back to a generic label.
pub fn this_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Linux device".to_string())
}
