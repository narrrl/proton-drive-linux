//! Reading and writing this machine's `profile.json` in its device root, and
//! the restore that rebuilds a machine from one (features.md 5.2 and 5.3).
//!
//! The document itself is defined in [`pdfs_core::profile`]; this is the half
//! that talks to Drive. Saving is best-effort and never blocks a user action:
//! the profile is a convenience for a *future* machine, so failing to write it
//! must not fail the folder add that triggered it.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use pdfs_core::control::RestorableFolder;
use pdfs_core::profile::{PROFILE_FILE_NAME, PROFILE_VERSION, Profile, ProfileFolder, ProfilePin};
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{info, warn};

use super::sync;
use super::{Core, now_secs, parse_uid, this_hostname};

/// Coalescing flags for background profile saves.
///
/// A single burst of user actions (adding three folders, pinning a tree) should
/// cost one upload, not one per action. `SAVING` admits one worker at a time and
/// `DIRTY` records that something changed while it was busy, so the worker takes
/// exactly one more lap afterwards.
///
/// Process-global rather than a [`Core`] field because on-demand mounts fork
/// their own `Core` (`Core::fork_state`) and all of them describe the same
/// machine — there is one profile per daemon, so there is one flag pair.
static SAVE_STATE: OnceLock<(AtomicBool, AtomicBool)> = OnceLock::new();

fn save_state() -> &'static (AtomicBool, AtomicBool) {
    SAVE_STATE.get_or_init(|| (AtomicBool::new(false), AtomicBool::new(false)))
}

impl Core {
    /// Note that the arrangement this machine's profile describes has changed,
    /// and upload a fresh profile in the background.
    ///
    /// Fire-and-forget by design: callers are mutation paths (add/remove a sync
    /// folder, switch a mode, pin a node, save settings) whose success does not
    /// depend on the backup succeeding.
    pub(crate) fn touch_profile(&self) {
        let (saving, dirty) = save_state();
        dirty.store(true, Ordering::SeqCst);
        // Someone is already uploading; they will see DIRTY and take another lap.
        if saving.swap(true, Ordering::SeqCst) {
            return;
        }
        let core = self.clone();
        std::thread::spawn(move || {
            let (saving, dirty) = save_state();
            while dirty.swap(false, Ordering::SeqCst) {
                if let Err(e) = core.save_profile() {
                    warn!(error = %e.message, "profile backup failed");
                    break;
                }
            }
            saving.store(false, Ordering::SeqCst);
        });
    }

    /// Serialize this machine's arrangement and upload it to `profile.json` in
    /// the device root, replacing any existing one.
    ///
    /// Does nothing when no device is registered: a machine that has never
    /// synced a folder has nothing to describe, and registering a device just to
    /// hold an empty profile would put a stray computer in the user's Drive.
    pub(crate) fn save_profile(&self) -> CoreResult<()> {
        let Some(device) = self
            .db
            .device_get()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
        else {
            return Ok(());
        };
        let root_uid = parse_uid(&device.root_uid).ok_or_else(|| {
            CoreError::internal(format!("bad device root uid: {}", device.root_uid))
        })?;

        let profile = self.build_profile(&device.uid)?;
        let bytes = profile.to_bytes().map_err(CoreError::internal)?;
        let len = bytes.len() as i64;

        // Replace the existing document rather than adding a second file of the
        // same name: the profile is a single mutable record, and a device root
        // with `profile.json` and `profile (1).json` in it teaches the restore
        // nothing about which to trust.
        let existing = self.find_device_child_file(&root_uid, PROFILE_FILE_NAME)?;
        match existing {
            Some(uid) => self
                .rt
                .block_on(self.client.upload_new_revision_from(
                    &uid,
                    Cursor::new(bytes),
                    len,
                    Vec::new(),
                    Some(now_secs()),
                ))
                .map(|_| ())
                .map_err(|e| CoreError::from_api(&e, "upload profile revision"))?,
            None => {
                self.rt
                    .block_on(self.client.upload_file_replacing_draft_from(
                        &root_uid,
                        PROFILE_FILE_NAME,
                        "application/json",
                        Cursor::new(bytes),
                        len,
                        Vec::new(),
                        Some(now_secs()),
                        false,
                    ))
                    .map_err(|e| CoreError::from_api(&e, "upload profile"))?;
            }
        }
        info!(folders = profile.folders.len(), "profile backed up");
        Ok(())
    }

    /// Snapshot the local arrangement into a [`Profile`].
    fn build_profile(&self, device_uid: &str) -> CoreResult<Profile> {
        let folders = self
            .db
            .sync_folder_list()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .into_iter()
            .map(|f| ProfileFolder {
                remote_uid: f.remote_uid,
                local_path: f.local_path,
                // A queued switch is where the user asked the folder to be, so
                // that is what the next machine should set up.
                mode: f.pending_mode.unwrap_or(f.mode),
            })
            .collect();
        let pins = self
            .db
            .pin_list()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .into_iter()
            .map(|(uid, path, recursive)| ProfilePin {
                uid,
                path,
                recursive,
            })
            .collect();
        let cfg = pdfs_core::config::AppDirs::new()
            .map(|d| d.load_config())
            .unwrap_or_default();
        Ok(Profile {
            version: PROFILE_VERSION,
            device_uid: device_uid.to_string(),
            hostname: this_hostname(),
            saved_at: now_secs(),
            folders,
            pins,
            ignore_patterns: cfg.ignore_patterns,
            cache_budget: cfg.cache_budget,
            mountpoint: cfg.mountpoint,
        })
    }

    /// An untrashed *file* named `name` directly under the device root.
    ///
    /// The sibling of `find_device_child_folder`; kept separate because a folder
    /// named `profile.json` must not be mistaken for the profile.
    fn find_device_child_file(
        &self,
        root_uid: &NodeUid,
        name: &str,
    ) -> CoreResult<Option<NodeUid>> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_folder_children_node_uids(root_uid))
            .map_err(|e| CoreError::from_api(&e, "list device root"))?;
        if uids.is_empty() {
            return Ok(None);
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| CoreError::from_api(&e, "resolve device root children"))?;
        Ok(nodes
            .into_iter()
            .find(|n| !n.is_folder() && !n.trashed && n.name == name)
            .map(|n| n.uid))
    }

    /// Download and parse the profile in the device root, if there is one.
    fn load_profile(&self, root_uid: &NodeUid) -> CoreResult<Option<Profile>> {
        let Some(uid) = self.find_device_child_file(root_uid, PROFILE_FILE_NAME)? else {
            return Ok(None);
        };
        let mut buf: Vec<u8> = Vec::new();
        self.rt
            .block_on(self.client.download_file_to(&uid, &mut buf))
            .map_err(|e| CoreError::from_api(&e, "download profile"))?;
        Profile::parse(&buf).map(Some).map_err(CoreError::invalid)
    }

    // ---- restore (features.md 5.2) ----------------------------------------

    /// The folders under this machine's device that could be synced here, each
    /// carrying a proposed local path.
    ///
    /// The proposal comes from the profile when one exists and its path is
    /// usable on this machine, and from `~/<name>` otherwise — a restored
    /// `/home/other-user/Documents` would be wrong on a machine where that user
    /// does not exist, and silently creating it would be worse.
    pub(crate) fn list_restorable_folders(&self) -> CoreResult<Vec<RestorableFolder>> {
        let device = self.ensure_device()?;
        let root_uid = parse_uid(&device.root_uid).ok_or_else(|| {
            CoreError::internal(format!("bad device root uid: {}", device.root_uid))
        })?;

        let profile = match self.load_profile(&root_uid) {
            Ok(p) => p,
            // A missing or unreadable profile is not fatal: the remote tree
            // alone is enough to offer a restore, just with plainer defaults.
            Err(e) => {
                warn!(error = %e.message, "could not read profile; proposing defaults");
                None
            }
        };

        let uids = self
            .rt
            .block_on(self.client.enumerate_folder_children_node_uids(&root_uid))
            .map_err(|e| CoreError::from_api(&e, "list device root"))?;
        let nodes = if uids.is_empty() {
            Vec::new()
        } else {
            self.rt
                .block_on(self.client.enumerate_nodes(&uids))
                .map_err(|e| CoreError::from_api(&e, "resolve device root children"))?
        };

        // Folders already synced here are not restorable — they are already
        // restored — but they are still listed, flagged, so the picker can show
        // the whole device rather than a confusing subset.
        let existing: Vec<String> = self
            .db
            .sync_folder_list()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .into_iter()
            .map(|f| f.remote_uid)
            .collect();

        Ok(nodes
            .into_iter()
            .filter(|n| n.is_folder() && !n.trashed)
            .map(|n| {
                let uid = n.uid.to_string();
                let saved = profile
                    .as_ref()
                    .and_then(|p| p.folders.iter().find(|f| f.remote_uid == uid));
                let proposed = saved
                    .map(|f| PathBuf::from(&f.local_path))
                    .filter(|p| path_is_usable_here(p))
                    .unwrap_or_else(|| default_local_path(&n.name));
                RestorableFolder {
                    already_synced: existing.iter().any(|e| e == &uid),
                    remote_uid: uid,
                    name: n.name,
                    local_path: proposed.to_string_lossy().to_string(),
                    mode: saved
                        .map(|f| f.mode.clone())
                        .unwrap_or_else(|| "mirror".into()),
                }
            })
            .collect())
    }

    /// Attach remote device folders to local paths and let the sync engine fill
    /// them in.
    ///
    /// The download is not written here: a new sync-folder row starts with an
    /// empty baseline, and an empty baseline against an empty local directory
    /// and a populated remote already classifies as "download everything". The
    /// restore is therefore just bookkeeping plus a `Reconcile`, and it reuses
    /// the engine's conflict handling for the case where the local directory
    /// turns out not to be empty after all.
    pub(crate) fn restore_sync_folders(
        &self,
        items: &[pdfs_core::control::RestoreItem],
    ) -> CoreResult<String> {
        if items.is_empty() {
            return Err(CoreError::invalid("nothing to restore"));
        }
        let device = self.ensure_device()?;
        let existing = self
            .db
            .sync_folder_list()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;

        let mut restored = 0usize;
        let mut skipped: Vec<String> = Vec::new();
        for item in items {
            let local = PathBuf::from(&item.local_path);
            if !local.is_absolute() {
                skipped.push(format!("{}: local path must be absolute", item.local_path));
                continue;
            }
            if existing.iter().any(|f| f.remote_uid == item.remote_uid) {
                skipped.push(format!("{}: already synced", item.local_path));
                continue;
            }
            if existing
                .iter()
                .any(|f| Path::new(&f.local_path) == local.as_path())
            {
                skipped.push(format!("{}: path already in use", item.local_path));
                continue;
            }
            if let Err(e) = std::fs::create_dir_all(&local) {
                skipped.push(format!("{}: {e}", item.local_path));
                continue;
            }
            let mode = match item.mode.as_str() {
                "mirror" | "ondemand" => item.mode.as_str(),
                other => {
                    skipped.push(format!("{}: unknown mode {other}", item.local_path));
                    continue;
                }
            };
            let id = self
                .db
                .sync_folder_add(&local.to_string_lossy(), &item.remote_uid, &device.share_id)
                .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
            // `ondemand` is applied through the normal request path, which
            // mounts the FUSE session and handles the folder being busy — the
            // restore does not get its own copy of that logic.
            if mode == "ondemand" {
                let _ = self.request_sync_folder_mode(id, "ondemand");
            }
            let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));
            restored += 1;
        }
        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);

        let mut message = format!("restored {restored} folder(s)");
        if !skipped.is_empty() {
            message.push_str(&format!("; skipped: {}", skipped.join(", ")));
        }
        info!(restored, skipped = skipped.len(), "restore complete");
        Ok(message)
    }
}

/// Whether a path recorded on another machine can be reused verbatim here.
///
/// Requires the *parent* to exist: `~/Documents` on a machine with the same
/// username is a fine proposal even though the directory itself is gone (the
/// restore creates it), but `/home/someone-else/Documents` is not.
fn path_is_usable_here(path: &Path) -> bool {
    path.is_absolute() && path.parent().is_some_and(|p| p.is_dir())
}

/// `~/<name>`, or `/<name>` if the home directory cannot be determined.
fn default_local_path(name: &str) -> PathBuf {
    match pdfs_core::config::AppDirs::new()
        .ok()
        .and_then(|d| d.home_dir())
    {
        Some(home) => home.join(name),
        None => PathBuf::from("/").join(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_home_is_not_reused() {
        assert!(!path_is_usable_here(Path::new(
            "/home/definitely-not-a-user-here/Documents"
        )));
        assert!(!path_is_usable_here(Path::new("relative/path")));
    }

    /// A path whose parent exists but which does not itself is exactly the
    /// restore case — the directory is about to be created.
    #[test]
    fn missing_leaf_under_existing_parent_is_reused() {
        let dir = std::env::temp_dir().join("pdfs-restore-probe");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(path_is_usable_here(&dir.join("Documents")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
