//! Which local folders may become a sync root.
//!
//! A mirror uploads everything under its root and deletes what Drive deleted,
//! so a root that holds more than the user meant is dangerous. `$HOME` takes
//! the decrypted content cache, `cache.db` and `~/.ssh` up to Drive, and walks
//! into the mount, which downloads the whole Drive only to upload it again.
//! Every place a root is added or restored goes through [`problem`].

use std::path::{Component, Path, PathBuf};

use crate::config::{AppConfig, AppDirs};

/// The paths a sync root must stay clear of.
#[derive(Debug, Clone, Default)]
pub struct Reserved {
    /// The user's home folder. A root may sit inside it, not be it or above it.
    pub home: Option<PathBuf>,
    /// The Proton Drive mount.
    pub mountpoint: Option<PathBuf>,
    /// This app's own configuration, state and cache folders.
    pub app_dirs: Vec<PathBuf>,
}

impl Reserved {
    /// The reserved paths of this user and `config`, resolved like a root.
    pub fn current(config: &AppConfig) -> Self {
        let Ok(dirs) = AppDirs::new() else {
            return Self::default();
        };
        let resolved = |path: PathBuf| resolve(&path).unwrap_or(path);
        let mut app_dirs = vec![dirs.state_dir(), dirs.cache_dir()];
        if let Some(config_dir) = dirs.config_path().parent() {
            app_dirs.push(config_dir.to_path_buf());
        }
        Self {
            home: dirs.home_dir().map(resolved),
            mountpoint: Some(resolved(dirs.resolved_mountpoint(config))),
            app_dirs: app_dirs.into_iter().map(resolved).collect(),
        }
    }
}

/// `path` as a root is compared: absolute, with symlinks resolved as far as the
/// path exists. A folder a restore is about to create does not exist yet, so
/// the part past the deepest existing folder is kept as written.
///
/// A `..` is refused rather than resolved: before a symlink it means something
/// else than after it, and a path recorded on another machine has no business
/// holding one.
pub fn resolve(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(format!("{} goes up with `..`", path.display()));
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(canonical) => {
                return Ok(rest
                    .into_iter()
                    .rev()
                    .fold(canonical, |path: PathBuf, part| path.join(part)));
            }
            Err(_) => {
                let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
                    return Ok(path.to_path_buf());
                };
                rest.push(name);
                if !existing.pop() {
                    return Ok(path.to_path_buf());
                }
            }
        }
    }
}

/// Why `root` cannot be synced, or `None` when it can. `root` is resolved
/// ([`resolve`]); `others` are the roots already synced.
pub fn problem(root: &Path, others: &[PathBuf], reserved: &Reserved) -> Option<String> {
    let shown = root.display();
    if root.parent().is_none() {
        return Some(format!("{shown} is the root of the file system"));
    }
    if let Some(home) = &reserved.home {
        if root == home {
            return Some(format!(
                "{shown} is your home folder; pick a folder inside it"
            ));
        }
        if home.starts_with(root) {
            return Some(format!("{shown} holds your home folder"));
        }
    }
    if let Some(mountpoint) = &reserved.mountpoint
        && overlaps(root, mountpoint)
    {
        return Some(format!(
            "{shown} overlaps the Proton Drive folder {}",
            mountpoint.display()
        ));
    }
    if let Some(dir) = reserved.app_dirs.iter().find(|dir| overlaps(root, dir)) {
        return Some(format!(
            "{shown} overlaps this app's own data in {}",
            dir.display()
        ));
    }
    if others.iter().any(|other| other == root) {
        return Some(format!("{shown} is already synced"));
    }
    if let Some(other) = others.iter().find(|other| overlaps(root, other)) {
        return Some(format!(
            "{shown} overlaps the synced folder {}",
            other.display()
        ));
    }
    None
}

/// Whether one of `a` and `b` is the other or inside it.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reserved() -> Reserved {
        Reserved {
            home: Some("/home/u".into()),
            mountpoint: Some("/home/u/ProtonDrive".into()),
            app_dirs: vec![
                "/home/u/.local/state/proton-drive-linux".into(),
                "/home/u/.cache/proton-drive-linux".into(),
                "/home/u/.config/proton-drive-linux".into(),
            ],
        }
    }

    fn refused(root: &str, others: &[&str]) -> bool {
        let others: Vec<PathBuf> = others.iter().map(PathBuf::from).collect();
        problem(Path::new(root), &others, &reserved()).is_some()
    }

    #[test]
    fn a_folder_inside_home_can_be_synced() {
        assert!(!refused("/home/u/Documents", &[]));
        assert!(!refused("/home/u/Pictures", &["/home/u/Documents"]));
        assert!(!refused("/mnt/data/photos", &[]));
    }

    #[test]
    fn the_file_system_root_home_and_its_parents_are_refused() {
        assert!(refused("/", &[]));
        assert!(refused("/home/u", &[]));
        assert!(refused("/home", &[]));
    }

    #[test]
    fn the_mount_and_the_apps_own_data_are_refused_either_way_round() {
        assert!(refused("/home/u/ProtonDrive", &[]));
        assert!(refused("/home/u/ProtonDrive/Documents", &[]));
        assert!(refused("/home/u/.cache", &[]));
        assert!(refused(
            "/home/u/.local/state/proton-drive-linux/staging",
            &[]
        ));
        assert!(refused("/home/u/.config", &[]));
    }

    #[test]
    fn a_root_inside_or_around_another_root_is_refused() {
        assert!(refused("/home/u/Documents", &["/home/u/Documents"]));
        assert!(refused("/home/u/Documents/Work", &["/home/u/Documents"]));
        assert!(refused("/home/u/stuff", &["/home/u/stuff/Documents"]));
        // A shared prefix of characters is not a shared folder.
        assert!(!refused("/home/u/Documents2", &["/home/u/Documents"]));
    }

    #[test]
    fn a_path_that_goes_up_or_is_relative_does_not_resolve() {
        assert!(resolve(Path::new("/home/u/../../etc")).is_err());
        assert!(resolve(Path::new("Documents")).is_err());
    }

    #[test]
    fn a_folder_still_to_be_created_resolves_under_its_existing_parent() {
        let base = std::env::temp_dir().join(format!("pdfs-syncroot-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let canonical = base.canonicalize().unwrap();
        assert_eq!(
            resolve(&base.join("new/inner")).unwrap(),
            canonical.join("new/inner")
        );
        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&canonical, &link).unwrap();
        assert_eq!(
            resolve(&link.join("later")).unwrap(),
            canonical.join("later"),
            "a symlink is followed to where it points"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
