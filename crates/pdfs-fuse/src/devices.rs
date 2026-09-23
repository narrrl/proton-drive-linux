//! This machine as a Proton Drive Device, and the local folders it keeps in
//! step with the remote (devices.md).
//!
//! A sync folder runs in one of two modes. `mirror` keeps a real local tree that
//! the engine in [`super::sync`] reconciles both ways; `ondemand` evicts that
//! tree and mounts a second FUSE session over the same path, rooted at the
//! folder`s remote node. Flipping between them is the delicate part — see
//! [`Core::apply_sync_folder_mode`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use pdfs_core::batch;
use pdfs_core::control::{
    ActivityKind, DeviceInfo, JobItem, MountKind, MountSpec, SyncFolderInfo, SyncPhase,
    SyncProgress,
};
use pdfs_core::db::{StoredDevice, StoredSyncFolder};
use pdfs_core::mounts::MountMode;
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::ids::{DeviceUid, NodeUid};
use proton_drive_rs::{DeviceType, Node};
use std::sync::OnceLock;
use tracing::{info, warn};

use super::sync::{self, base_name};
use super::{
    Core, SecondaryInsertRejection, SecondaryMount, State, SwitchBlocked, clear_stale_mount,
    device_type_str, dir_is_empty, evict_dir_contents, fuse_connection_id, is_stale_mount,
    now_secs, parse_uid, spawn_session, sync_folder_info, this_hostname,
};

/// A mode that this daemon is allowed to apply.
///
/// Persisted and control-protocol values first pass through [`MountMode`] so
/// forward-compatible unknown values never reach either destructive transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApplicableMountMode {
    Mirror,
    OnDemand,
}

impl ApplicableMountMode {
    fn parse(value: &str) -> Result<Self, String> {
        match MountMode::from(value) {
            MountMode::Mirror => Ok(Self::Mirror),
            MountMode::OnDemand => Ok(Self::OnDemand),
            MountMode::Unknown => Err(format!("unknown mode {value:?} (want mirror|ondemand)")),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::OnDemand => "ondemand",
        }
    }
}

fn select_mode_transition(
    current: &str,
    target: &str,
) -> Result<Option<ApplicableMountMode>, String> {
    let current = ApplicableMountMode::parse(current)
        .map_err(|error| format!("unsupported current mode: {error}"))?;
    let target = ApplicableMountMode::parse(target)
        .map_err(|error| format!("unsupported target mode: {error}"))?;
    Ok((current != target).then_some(target))
}

fn restore_snapshot_matches(snapshot: &StoredSyncFolder, current: &StoredSyncFolder) -> bool {
    current.id == snapshot.id
        && current.local_path == snapshot.local_path
        && current.remote_uid == snapshot.remote_uid
        && current.remote_share_id == snapshot.remote_share_id
        && current.mode == "ondemand"
}

fn pending_intent_matches(pending: Option<&str>, expected: Option<&str>) -> bool {
    expected.is_none_or(|expected| pending == Some(expected))
}

fn commit_after_teardown<T, E>(
    teardown: Result<(), E>,
    commit: impl FnOnce() -> T,
) -> Result<T, E> {
    teardown?;
    Ok(commit())
}

/// The device uid this machine has been adopted into, from `config.json`.
///
/// Read fresh each time rather than cached on [`Core`]: adoption rewrites the
/// config from the control socket, and the next `ensure_device` must see it
/// without a daemon restart.
pub(crate) fn adopted_device_uid() -> Option<String> {
    let uid = pdfs_core::config::AppDirs::new()
        .ok()?
        .load_config()
        .device_uid?;
    let uid = uid.trim().to_string();
    (!uid.is_empty()).then_some(uid)
}

/// Pin (or, with `None`, unpin) the adopted device uid in `config.json`.
pub(crate) fn set_adopted_device_uid(uid: Option<&str>) -> CoreResult<()> {
    let dirs = pdfs_core::config::AppDirs::new()
        .map_err(|e| CoreError::internal(format!("config dirs: {e}")))?;
    let mut cfg = dirs.load_config();
    cfg.device_uid = uid.map(|u| u.to_string());
    dirs.save_config(&cfg)
        .map_err(|e| CoreError::internal(format!("write config: {e}")))
}

impl Core {
    // ---- devices ----------------------------------------------------------

    /// Unified local-location listing for the control protocol.
    pub(crate) fn list_locations(&self) -> CoreResult<Vec<MountSpec>> {
        let progress = self.sync_progress.lock();
        let mut locations = self
            .db
            .mount_list()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
        for location in &mut locations {
            location.mounted = self.states.is_mounted_at(Path::new(&location.local_path));
            if let MountKind::Device { sync_folder_id } = &location.kind {
                location.progress = progress.get(sync_folder_id).cloned();
                location.paused = self.sync_folder_paused(*sync_folder_id);
            }
        }
        Ok(locations)
    }

    /// List the account's registered devices, flagging the one *this* machine
    /// syncs to so a front-end can treat it as more than another computer in the
    /// list — deleting it takes this machine's synced folders down with it.
    pub(crate) fn list_devices(&self) -> CoreResult<Vec<DeviceInfo>> {
        let devices = self
            .rt
            .block_on(self.client.enumerate_devices())
            .map_err(|e| CoreError::from_api(&e, "list devices"))?;
        // No cached device row yet means this machine syncs nothing, so none of
        // the listed devices is ours.
        let this_uid = self.db.device_get().ok().flatten().map(|d| d.uid);
        let pinned = adopted_device_uid();
        Ok(devices
            .into_iter()
            .map(|d| {
                let uid = d.uid.to_string();
                DeviceInfo {
                    this_device: this_uid.as_deref() == Some(uid.as_str()),
                    adopted: pinned.as_deref() == Some(uid.as_str()),
                    uid,
                    name: d.name.unwrap_or_else(|_| "(unnamed device)".to_string()),
                    device_type: device_type_str(d.device_type).to_string(),
                    last_sync: d.last_sync_time,
                }
            })
            .collect())
    }

    /// Rename a device by its uid.
    pub(crate) fn rename_device(&self, uid: &str, name: &str) -> CoreResult<()> {
        if name.is_empty() {
            return Err(CoreError::invalid("device name must not be empty"));
        }
        let device_uid = DeviceUid::from(uid);
        self.rt
            .block_on(self.client.rename_device(&device_uid, name))
            .map_err(|e| CoreError::from_api(&e, "rename device"))?;
        Ok(())
    }

    /// Delete (deregister) a device by its uid.
    pub(crate) fn delete_device(&self, uid: &str) -> CoreResult<()> {
        let device_uid = DeviceUid::from(uid);
        self.rt
            .block_on(self.client.delete_device(&device_uid))
            .map_err(|e| CoreError::from_api(&e, "delete device"))?;
        Ok(())
    }

    /// Adopt an existing device as *this* machine's, pinning it in `config.json`
    /// so hostname changes and reinstalls stop registering duplicates
    /// (features.md 5.1). `None` clears the pin and returns to hostname matching.
    ///
    /// Refuses a uid that is not in the account: the whole point of the pin is
    /// that it is authoritative, so accepting a bad one would defer the failure
    /// to the next `ensure_device` with no obvious cause.
    pub(crate) fn adopt_device(&self, uid: Option<&str>) -> CoreResult<String> {
        let Some(uid) = uid else {
            set_adopted_device_uid(None)?;
            return Ok("adoption cleared; device now resolved by hostname".to_string());
        };
        let remote = self
            .rt
            .block_on(self.client.enumerate_devices())
            .map_err(|e| CoreError::from_api(&e, "enumerate devices"))?;
        let dev = remote
            .into_iter()
            .find(|d| d.uid.to_string() == uid)
            .ok_or_else(|| CoreError::not_found(format!("no device {uid} in this account")))?;

        let name = dev
            .name
            .as_deref()
            .ok()
            .map(|n| n.to_string())
            .unwrap_or_else(this_hostname);
        set_adopted_device_uid(Some(uid))?;
        // Write the row through too, so callers that read the cached device
        // (and `ListSyncFolders`' "this device" flag) agree immediately rather
        // than after the next `ensure_device`.
        self.db
            .device_set(&StoredDevice {
                uid: uid.to_string(),
                share_id: dev.share_id.to_string(),
                root_uid: dev.root_folder_uid.to_string(),
                name: name.clone(),
                created: dev.creation_time,
            })
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
        info!(uid, name, "adopted device");
        Ok(format!("adopted device {name}"))
    }

    /// The device a restore reads from: this machine's own for `None`, else
    /// the registered device with that uid.
    ///
    /// Another device is looked up without touching the cached device row —
    /// restoring its folders here must not make this machine back up as it.
    /// Its folders are then attached with that device's share id, so they sync
    /// against the other computer's tree.
    pub(crate) fn restore_source(&self, device: Option<&str>) -> CoreResult<StoredDevice> {
        let Some(uid) = device else {
            return self.ensure_device();
        };
        let remote = self
            .rt
            .block_on(self.client.enumerate_devices())
            .map_err(|e| CoreError::from_api(&e, "enumerate devices"))?;
        let d = remote
            .into_iter()
            .find(|d| d.uid.to_string() == uid)
            .ok_or_else(|| CoreError::not_found(format!("no device {uid} in this account")))?;
        Ok(StoredDevice {
            uid: d.uid.to_string(),
            share_id: d.share_id.to_string(),
            root_uid: d.root_folder_uid.to_string(),
            name: d
                .name
                .as_deref()
                .ok()
                .unwrap_or("(unnamed device)")
                .to_string(),
            created: d.creation_time,
        })
    }

    // ---- device folder sync (devices.md, Phase 1) -------------------------

    /// Auto-register (or recover) this machine as a Proton Drive Device, caching
    /// it so restarts reuse the same device. Recovery matches an existing remote
    /// Linux device by name before creating a new one, so a lost local record
    /// doesn't orphan the device's root folder.
    ///
    /// Resolution order: the *adopted* uid pinned in `config.json`, then the
    /// cached DB row (validated remotely), then a hostname match, then create.
    /// The pin exists because the hostname heuristic silently registers a second
    /// device after a rename or reinstall, orphaning the first one's folders
    /// (features.md 5.1).
    pub(crate) fn ensure_device(&self) -> CoreResult<StoredDevice> {
        let name = this_hostname();
        // Enumerate the remote devices once: used to validate the pin, validate
        // any cached record, and recover an existing device by name.
        let remote = self
            .rt
            .block_on(self.client.enumerate_devices())
            .map_err(|e| CoreError::from_api(&e, "enumerate devices"))?;

        // An adopted uid is an explicit instruction, so it outranks everything
        // and it fails loudly: falling back to the heuristic here would create
        // the duplicate device the user adopted specifically to avoid.
        if let Some(pin) = adopted_device_uid() {
            let d = remote
                .iter()
                .find(|d| d.uid.to_string() == pin)
                .ok_or_else(|| {
                    CoreError::not_found(format!(
                        "adopted device {pin} not found in this account; \
                         run `pdfs devices adopt --clear` to return to hostname matching"
                    ))
                })?;
            let dev = StoredDevice {
                uid: d.uid.to_string(),
                share_id: d.share_id.to_string(),
                root_uid: d.root_folder_uid.to_string(),
                name: d.name.as_deref().ok().unwrap_or(&name).to_string(),
                created: d.creation_time,
            };
            self.db
                .device_set(&dev)
                .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
            return Ok(dev);
        }

        // A cached device is only trustworthy if it still exists remotely. A
        // device deleted from another client (or the web UI) leaves a stale row
        // whose root folder is gone, so creating folders under it fails with
        // "parent node is not a folder". Re-register in that case.
        if let Some(dev) = self
            .db
            .device_get()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
        {
            if remote.iter().any(|d| d.uid.to_string() == dev.uid) {
                return Ok(dev);
            }
            warn!(uid = %dev.uid, "cached device is gone remotely; re-registering");
        }

        // Recover: an existing remote Linux device with the same name is ours.
        let existing = remote.into_iter().find(|d| {
            d.device_type == DeviceType::Linux && d.name.as_deref().ok() == Some(name.as_str())
        });
        let dev = match existing {
            Some(d) => StoredDevice {
                uid: d.uid.to_string(),
                share_id: d.share_id.to_string(),
                root_uid: d.root_folder_uid.to_string(),
                name,
                created: d.creation_time,
            },
            None => {
                let d = self
                    .rt
                    .block_on(self.client.create_device(&name, DeviceType::Linux))
                    .map_err(|e| CoreError::from_api(&e, "create device"))?;
                StoredDevice {
                    uid: d.uid.to_string(),
                    share_id: d.share_id.to_string(),
                    root_uid: d.root_folder_uid.to_string(),
                    name,
                    created: d.creation_time,
                }
            }
        };
        self.db
            .device_set(&dev)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
        Ok(dev)
    }

    /// An untrashed folder named `name` directly under the device root, if one
    /// already exists.
    pub(crate) fn find_device_child_folder(
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
            .find(|n| n.is_folder() && !n.trashed && n.name == name)
            .map(|n| n.uid))
    }

    /// Add a local folder to this device's sync set: register the device if
    /// needed, create a matching folder under the device root, upload the local
    /// tree into it once, and record the mapping. Phase 1 is a one-shot upload —
    /// the two-way engine (Phase 2) reconciles later changes.
    pub(crate) fn add_sync_folder(&self, local: &Path) -> CoreResult<StoredSyncFolder> {
        let meta = std::fs::metadata(local)
            .map_err(|e| CoreError::internal(format!("stat {}: {e}", local.display())))?;
        if !meta.is_dir() {
            return Err(CoreError::invalid(format!(
                "{} is not a directory",
                local.display()
            )));
        }
        let local = local
            .canonicalize()
            .map_err(|e| CoreError::internal(format!("canonicalize {}: {e}", local.display())))?;
        let local_str = local.to_string_lossy().to_string();

        // Reject duplicates up front for a clear error (UNIQUE would also catch it).
        if self
            .db
            .sync_folder_list()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .iter()
            .any(|f| f.local_path == local_str)
        {
            return Err(CoreError::invalid(format!(
                "{} is already synced",
                local.display()
            )));
        }

        let name = local
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                CoreError::invalid(format!("unusable folder name: {}", local.display()))
            })?
            .to_string();

        // Syncing a local `.proton-drive-linux` would land on the folder holding
        // this device's own profile (`find_device_child_folder` reuses by name),
        // and the sync engine would then delete or overwrite it.
        if name == pdfs_core::profile::PROFILE_DIR_NAME {
            return Err(CoreError::invalid(format!(
                "{name} is reserved for this device's profile backup"
            )));
        }

        let device = self.ensure_device()?;
        let root_uid = parse_uid(&device.root_uid).ok_or_else(|| {
            CoreError::internal(format!("bad device root uid: {}", device.root_uid))
        })?;

        // The synced folder's remote root: the folder under the device root named
        // after the local basename. Reuse an existing one rather than creating a
        // second folder with the same name — re-adding a folder (after a removal, or
        // after a failed add that had already created it) must land back on the
        // original, not leave the user with two "Downloads" in their Drive. The
        // reconcile treats an existing remote tree correctly: unmatched paths read as
        // a conflict, not as data loss.
        let remote_root = match self.find_device_child_folder(&root_uid, &name)? {
            Some(uid) => {
                info!(name, "reusing existing device folder");
                uid
            }
            None => self
                .rt
                .block_on(
                    self.client
                        .create_folder(&root_uid, &name, Some(now_secs())),
                )
                .map_err(|e| CoreError::from_api(&e, &format!("create device folder {name}")))?,
        };

        let id = self
            .db
            .sync_folder_add(&local_str, &remote_root.to_string(), &device.share_id)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;

        // Hand the initial upload to the sync engine: an empty baseline against a
        // full local tree reconciles as "upload everything", and the folder is
        // added to the filesystem watch set in the same pass.
        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
        let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));

        info!(local = %local.display(), id, "added sync folder");
        self.db
            .sync_folder_get(id)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .ok_or_else(|| CoreError::not_found("sync folder vanished after insert"))
    }

    /// List this device's synced folders for the front-ends, each carrying the
    /// live progress of its pass when one is running.
    pub(crate) fn list_sync_folders(&self) -> CoreResult<Vec<SyncFolderInfo>> {
        let progress = self.sync_progress.lock();
        Ok(self
            .db
            .sync_folder_list()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .into_iter()
            .map(|f| {
                let live = progress.get(&f.id).cloned();
                let paused = self.sync_folder_paused(f.id);
                sync_folder_info(f, live, paused)
            })
            .collect())
    }

    /// Everything the daemon is chewing on that isn't moving bytes, for
    /// `GetQueueStatus`: the registered jobs (bulk-upload scans, the local index)
    /// plus a synthesized job per running sync pass, so one Activity view answers
    /// "is anything still happening?" without also polling `ListSyncFolders`.
    ///
    /// The sync passes are folded in here rather than tracked as registry jobs
    /// because the Devices page needs them per folder anyway
    /// ([`SyncFolderInfo::progress`]) — this keeps one source of truth and hits
    /// the db only while a pass is actually running.
    pub(crate) fn jobs_snapshot(&self) -> Vec<JobItem> {
        let mut jobs = self.transfers.jobs_snapshot();
        let mut passes: Vec<(i64, SyncProgress)> = self
            .sync_progress
            .lock()
            .iter()
            .map(|(id, p)| (*id, p.clone()))
            .collect();
        if passes.is_empty() {
            return jobs;
        }
        passes.sort_by_key(|(id, _)| *id);

        let names: HashMap<i64, String> = self
            .db
            .sync_folder_list()
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.id, f.local_path))
            .collect();
        for (id, p) in passes {
            // The row is titled with the folder's own name; the full local path
            // is what the Devices page shows, and is far too long for this line.
            let folder = names
                .get(&id)
                .and_then(|path| Path::new(path).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "folder".to_string());
            jobs.push(match p.phase {
                // The scan's total is the last pass's baseline, so a folder that
                // has never synced still reports indeterminate (`total: 0`) — but
                // every later pass has a real bar. A grown folder can push `done`
                // past the estimate; clamp so the row never reads "600 of 500".
                SyncPhase::Scanning => JobItem {
                    title: format!("Checking {folder}"),
                    detail: "Looking for changes".to_string(),
                    done: p.done as u64,
                    total: if p.total == 0 {
                        0
                    } else {
                        p.total.max(p.done) as u64
                    },
                },
                SyncPhase::Applying => JobItem {
                    title: format!("Syncing {folder}"),
                    detail: p.current.clone(),
                    done: p.done as u64,
                    total: p.total.max(p.done) as u64,
                },
            });
        }
        jobs
    }

    /// The lock guarding sync-folder `id` against concurrent reconcile/mode-switch.
    pub(crate) fn sync_lock(&self, id: i64) -> Arc<Mutex<()>> {
        self.sync_locks.lock().entry(id).or_default().clone()
    }

    /// Remove a synced folder from the sync set. `delete_remote` also deletes its
    /// folder under the device root; otherwise the cloud copy is left in place.
    pub(crate) fn remove_sync_folder(&self, id: i64, delete_remote: bool) -> CoreResult<()> {
        let lock = self.sync_lock(id);
        let _guard = lock.lock();
        let folder = self
            .db
            .sync_folder_get(id)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .ok_or_else(|| CoreError::not_found(format!("no synced folder with id {id}")))?;
        // An `ondemand` folder *is* a FUSE mount over its local path, so dropping
        // only the row would strand the mount: the path would keep serving a
        // folder the daemon no longer tracks, and nothing would ever unmount it.
        // Tear it down first — including before trashing the remote tree it
        // serves, which would otherwise leave it answering for deleted nodes.
        // Taken in its own statement so the registry lock is released before the
        // teardown: an edition-2024 `if let` chain holds the guard for the whole
        // body, and `teardown` unmounts and joins the session thread — which can
        // be inside a multi-second transfer. The shutdown path takes the same
        // lock, so holding it here risks systemd's stop timeout landing mid-
        // unmount and stranding the endpoint.
        let taken = self.mounts.lock().remove(&id);
        if let Some(mount) = taken
            && let Err(error) = commit_after_teardown(mount.teardown(), || ())
        {
            let _ = self.db.sync_folder_set_state(id, "error", now_secs());
            return Err(CoreError::internal(format!(
                "cannot remove synced folder: unmount {} failed: {error}",
                folder.local_path
            )));
        }
        info!(id, path = %folder.local_path, "unmounted on-demand folder");
        if !self
            .db
            .sync_folder_remove(id)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
        {
            return Err(CoreError::invalid(format!("no synced folder with id {id}")));
        }
        drop(_guard);
        self.clear_sync_folder_paused(id);
        // Stop watching the folder we just dropped.
        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
        if delete_remote
            && let Some(uid) = parse_uid(&folder.remote_uid)
            && let Err(e) = self
                .rt
                .block_on(self.client.trash_nodes(&[uid]))
                .and_then(batch::into_unit)
        {
            warn!(id, error = %e, "delete remote device folder failed");
        }
        Ok(())
    }

    /// Trigger a reconcile: one folder by id, or every folder when `id` is `None`.
    pub(crate) fn sync_now(&self, id: Option<i64>) {
        let _ = match id {
            Some(id) => self.sync_tx.send(sync::SyncMsg::Reconcile(id)),
            None => self.sync_tx.send(sync::SyncMsg::ReconcileAll),
        };
    }

    /// A sibling Core that shares this one's client/rt/cache/db (and transfer,
    /// activity, mount registries) but gets a **fresh, empty `State`** — its own
    /// inode space starting at [`ROOT_INO`]. Used to root a secondary FUSE session
    /// at an `ondemand` sync folder without colliding with the main mount's inodes
    /// (devices.md Phase 3).
    pub(crate) fn fork_state(&self) -> Core {
        let mut fork = self.clone();
        fork.primary = false;
        let share_access = self.state().share_access.clone();
        fork.state = Arc::new(Mutex::new(State::new(self.db.clone(), share_access, 2)));
        // A fresh inode space needs a fresh notification channel: this fork's
        // session is the only one that knows these inodes, so it must be the one
        // notified about them. Filled in by `spawn_ondemand_mount`.
        fork.notifier = Arc::new(OnceLock::new());
        fork.session_live = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Size upgrades are keyed by inode, which is per-fork too.
        fork.size_upgrades = Arc::new(Mutex::new(HashMap::new()));
        fork
    }

    /// Ask for a synced folder to move to `mode`, applying it now if the folder is
    /// free and safe to switch, and **queueing** it otherwise. Returns the human
    /// message for the reply.
    ///
    /// Queueing rather than rejecting is what makes the toggle usable: a folder
    /// that syncs continuously (a busy Downloads folder) is almost never caught in
    /// the narrow window where it is both unlocked and `idle`, so a `try_lock`
    /// rejection asks the user to keep retrying until they get lucky. Instead the
    /// intent is recorded and the engine applies it at the end of the pass already
    /// running — which, seeing a queued `ondemand`, also stops pulling down files
    /// it is about to evict ([`Core::push_pass`]).
    pub(crate) fn request_sync_folder_mode(&self, id: i64, mode: &str) -> CoreResult<String> {
        // Preserve the existing control behavior: reject an unsupported request
        // before looking up the folder, then validate the persisted source too.
        let target_mode = ApplicableMountMode::parse(mode).map_err(CoreError::invalid)?;
        let folder = self
            .db
            .sync_folder_get(id)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .ok_or_else(|| CoreError::not_found(format!("no synced folder with id {id}")))?;
        let current_mode = ApplicableMountMode::parse(&folder.mode)
            .map_err(|error| CoreError::invalid(format!("unsupported current mode: {error}")))?;
        // A switch waits on the folder's reconcile, which a pause skips; refuse
        // it up front rather than queue it indefinitely.
        if current_mode != target_mode && self.sync_folder_paused(id) {
            return Err(CoreError::invalid(
                "this folder is paused; resume it before changing its mode",
            ));
        }
        if current_mode == target_mode {
            let mode_str = target_mode.as_str();
            let lock = self.sync_lock(id);
            match lock.try_lock() {
                Some(_guard) => {
                    let current = self
                        .db
                        .sync_folder_get(id)
                        .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
                        .ok_or_else(|| {
                            CoreError::not_found(format!("no synced folder with id {id}"))
                        })?;
                    let locked_mode =
                        ApplicableMountMode::parse(&current.mode).map_err(|error| {
                            CoreError::invalid(format!("unsupported current mode: {error}"))
                        })?;
                    if locked_mode == target_mode {
                        self.db
                            .sync_folder_set_pending_mode(id, None)
                            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
                        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
                        return Ok(if current.pending_mode.is_some() {
                            format!("staying {mode_str}")
                        } else {
                            format!("already {mode_str}")
                        });
                    }
                }
                None => {
                    // A transition or reconcile owns the folder lock. Record
                    // this request instead of clearing the old target: if a
                    // queued switch already passed its recheck, its mode write
                    // preserves this opposite marker and the engine switches
                    // back next.
                    self.db
                        .sync_folder_set_pending_mode(id, Some(mode_str))
                        .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
                    let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));
                    return Ok(format!("staying {mode_str}"));
                }
            }
        }
        let mode = target_mode;
        let mode_str = target_mode.as_str();

        match self.apply_sync_folder_mode(id, mode, None) {
            Ok(message) => Ok(message),
            // Not switchable this instant — remember the intent instead of making
            // the user retry, and kick a pass to clear whatever is in the way.
            Err(SwitchBlocked::NotNow) => {
                self.db
                    .sync_folder_set_pending_mode(id, Some(mode_str))
                    .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
                let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));
                Ok(match mode {
                    ApplicableMountMode::OnDemand => format!(
                        "{} will go on-demand once its local changes are uploaded",
                        base_name(&folder.local_path)
                    ),
                    ApplicableMountMode::Mirror => format!(
                        "{} will start mirroring once the current sync finishes",
                        base_name(&folder.local_path)
                    ),
                })
            }
            Err(SwitchBlocked::Superseded) => Err(CoreError::internal(
                "direct mode request was unexpectedly superseded",
            )),
            Err(SwitchBlocked::Failed(e)) => Err(CoreError::internal(e)),
        }
    }

    /// Apply a queued mode switch if the folder has one and is now able to take it.
    /// Called by the sync engine after every pass, so a switch the user asked for
    /// mid-sync lands as soon as the pass that blocked it is done. A folder that is
    /// still not ready (its push failed, so the local copy is not safe to evict)
    /// keeps its `pending_mode` and is retried after the next pass.
    pub(crate) fn settle_pending_mode(&self, id: i64) {
        let Ok(Some(folder)) = self.db.sync_folder_get(id) else {
            return;
        };
        let Some(expected_mode) = folder.pending_mode.as_deref() else {
            return;
        };
        let mode = match select_mode_transition(&folder.mode, expected_mode) {
            Ok(Some(mode)) => mode,
            Ok(None) => {
                let _ = self.db.sync_folder_clear_pending_mode_if(id, expected_mode);
                return;
            }
            Err(error) => {
                warn!(
                    id,
                    current_mode = %folder.mode,
                    pending_mode = %expected_mode,
                    %error,
                    "withdrawing unsafe queued mode transition"
                );
                let _ = self.db.sync_folder_clear_pending_mode_if(id, expected_mode);
                return;
            }
        };
        let mode_str = mode.as_str();
        match self.apply_sync_folder_mode(id, mode, Some(expected_mode)) {
            Ok(message) => {
                info!(id, mode = mode_str, "applied queued mode switch");
                self.log_activity(ActivityKind::Sync, &message, "", true);
            }
            // Still blocked: the pass could not get everything up, so the local copy
            // is not safe to evict yet. Leave the request standing — the next pass
            // (poll, or the retry the engine schedules) tries again.
            Err(SwitchBlocked::NotNow) => {
                info!(id, mode = mode_str, "queued mode switch still waiting");
            }
            Err(SwitchBlocked::Superseded) => {
                info!(id, mode = mode_str, "queued mode switch was replaced");
            }
            Err(SwitchBlocked::Failed(e)) => {
                warn!(id, mode = mode_str, error = %e, "queued mode switch failed; withdrawing");
                let _ = self.db.sync_folder_clear_pending_mode_if(id, expected_mode);
                self.log_activity(
                    ActivityKind::Sync,
                    format!("couldn't switch {}", base_name(&folder.local_path)),
                    e,
                    false,
                );
            }
        }
    }

    /// Flip a synced folder between `mirror` (full local copy + two-way sync) and
    /// `ondemand` (a FUSE mount over the local path; no local storage). Returns a
    /// human message on success.
    ///
    /// - **mirror→ondemand**: require the folder in-sync (`idle`), stop watching it,
    ///   evict the local files to reclaim disk, then mount a secondary `ProtonFs`
    ///   rooted at the folder's remote node over its local path.
    /// - **ondemand→mirror**: unmount, clear the stale baseline (the local tree was
    ///   evicted), then hand the folder back to the engine, which re-downloads it.
    ///
    /// [`SwitchBlocked::NotNow`] means "not yet, try after a pass" and is never an
    /// error the user needs to see — callers queue on it.
    pub(crate) fn apply_sync_folder_mode(
        &self,
        id: i64,
        mode: ApplicableMountMode,
        expected_pending: Option<&str>,
    ) -> Result<String, SwitchBlocked> {
        let mode_str = mode.as_str();
        // Hold the folder's lock for the whole switch so no reconcile pass can be
        // running over the tree we are about to evict (or start while we mount over
        // it). A pass in flight holds the lock for its full duration, so `try_lock`
        // failing is exactly "still syncing" — and it is the only reliable signal:
        // the `state` column is still `idle` in the window between `add_sync_folder`
        // inserting the row and the engine picking it up.
        let lock = self.sync_lock(id);
        let Some(_guard) = lock.try_lock() else {
            return Err(SwitchBlocked::NotNow);
        };
        // Re-read under the lock: a pass that finished while we waited may have
        // changed `state`.
        let folder = self
            .db
            .sync_folder_get(id)
            .map_err(|e| SwitchBlocked::Failed(format!("db: {e:?}")))?
            .ok_or_else(|| SwitchBlocked::Failed(format!("no synced folder with id {id}")))?;
        if !pending_intent_matches(folder.pending_mode.as_deref(), expected_pending) {
            return Err(SwitchBlocked::Superseded);
        }
        let current_mode = ApplicableMountMode::parse(&folder.mode)
            .map_err(|e| SwitchBlocked::Failed(format!("unsupported current mode: {e}")))?;
        if current_mode == mode {
            return Ok(format!("already {mode_str}"));
        }
        let local = PathBuf::from(&folder.local_path);

        match mode {
            ApplicableMountMode::OnDemand => {
                // Only flip a folder that is fully in sync — a failed reconcile means
                // local edits could still be un-uploaded, and we are about to delete
                // the local copy. Not an error: a pass makes this true, and the queued
                // request is applied once one does.
                if folder.state != "idle" {
                    return Err(SwitchBlocked::NotNow);
                }
                let root_uid = parse_uid(&folder.remote_uid).ok_or_else(|| {
                    SwitchBlocked::Failed(format!("bad remote uid: {}", folder.remote_uid))
                })?;
                let root = self
                    .rt
                    .block_on(self.client.enumerate_nodes(std::slice::from_ref(&root_uid)))
                    .map_err(|e| SwitchBlocked::Failed(format!("fetch remote root: {e}")))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| SwitchBlocked::Failed("remote folder not found".to_string()))?;

                // Persist the mode *before* touching the local tree, and never roll it
                // back on failure.
                //
                // The eviction below deletes every local file in the folder. While the
                // row still says `mirror`, that empty directory is a valid input to the
                // sync engine, which reads it as "the user deleted all of this" and
                // trashes the whole folder on Drive. So the window between the evict and
                // the mode write is a window in which any failure — a busy mountpoint, a
                // missing fuse module, a dead DB — destroys the folder. Writing the mode
                // first closes it: the engine skips non-mirror folders, so the worst a
                // failure can now leave behind is an `ondemand` row with no mount, which
                // is inert. Inert and marked `error` is recoverable; trashed is not.
                //
                // The same reasoning forbids reverting to `mirror` when a step below
                // fails, since eviction may already have deleted part of the tree.
                // Recovery is the user's explicit switch back to `mirror`, whose arm
                // clears the baseline and re-downloads.
                if let Err(e) = self.db.sync_folder_set_mode(id, "ondemand") {
                    return Err(SwitchBlocked::Failed(format!("db: {e:?}")));
                }

                // Reclaim the disk before mounting. Once FUSE covers `local`, a
                // directory walk addresses the remote namespace, so evicting at
                // that point sends unlink/rmdir requests to Drive and leaves the
                // underlying mirror hidden and untouched.
                if let Err(e) = evict_dir_contents(&local) {
                    warn!(id, path = %local.display(), error = %e, "evict local dir contents failed");
                }
                let mount = self.spawn_ondemand_mount(&local, root).map_err(|e| {
                    SwitchBlocked::Failed(format!(
                        "mount ondemand failed after local cache eviction; switch back to mirror to restore it: {e}"
                    ))
                })?;
                self.publish_secondary_mount(id, mount)
                    .map_err(SwitchBlocked::Failed)?;
                let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
                self.db.sync_folder_set_state(id, "idle", now_secs()).ok();
                info!(id, path = %local.display(), "mounted sync folder on-demand");
                Ok(format!("{} is now on-demand", local.display()))
            }
            ApplicableMountMode::Mirror => {
                // ondemand→mirror: tear down the secondary mount first. Taken
                // before the teardown so the registry lock is not held across
                // the unmount and thread join (see `remove_sync_folder`).
                let taken = self.mounts.lock().remove(&id);
                if let Some(mount) = taken
                    && let Err(error) = commit_after_teardown(mount.teardown(), || ())
                {
                    let _ = self.db.sync_folder_set_state(id, "error", now_secs());
                    return Err(SwitchBlocked::Failed(format!(
                        "unmount {} failed; folder remains on-demand: {error}",
                        local.display()
                    )));
                }
                // The evicted local tree makes the old baseline lie ("everything
                // deleted locally"); clear it so the reconcile is a pure download.
                self.db
                    .sync_entries_clear(id)
                    .map_err(|e| SwitchBlocked::Failed(format!("db: {e:?}")))?;
                self.db
                    .sync_folder_set_mode(id, "mirror")
                    .map_err(|e| SwitchBlocked::Failed(format!("db: {e:?}")))?;
                let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
                let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));
                info!(id, path = %local.display(), "restored sync folder to mirror");
                Ok(format!(
                    "{} is mirroring again; downloading",
                    local.display()
                ))
            }
        }
    }

    /// Spawn a secondary FUSE session rooted at `root` over `local` on a forked
    /// inode space. Clears any stale kernel mount first (a crashed run can leave
    /// one, which would fail the fresh mount with EBUSY).
    pub(crate) fn spawn_ondemand_mount(
        &self,
        local: &Path,
        root: Node,
    ) -> CoreResult<SecondaryMount> {
        let core = self.fork_state();
        let session_live = core.session_live.clone();
        // The fork owns this state/path registration. `spawn_session` only
        // marks it live after FUSE has spawned successfully.
        core.register_state(local);
        let session = spawn_session(&core, local, root)
            .map_err(|e| CoreError::internal(format!("mount {}: {e}", local.display())))?;
        Ok(SecondaryMount::new(
            session,
            fuse_connection_id(local),
            session_live,
        ))
    }

    /// Publish one newly spawned secondary or explicitly tear it down when
    /// shutdown has closed publication or another session already owns the id.
    fn publish_secondary_mount(&self, id: i64, mount: SecondaryMount) -> Result<(), String> {
        let rejected = self.mounts.lock().insert(id, mount);
        let Err((reason, mount)) = rejected else {
            return Ok(());
        };
        let reason = match reason {
            SecondaryInsertRejection::Closed => "daemon is shutting down",
            SecondaryInsertRejection::Duplicate => "folder already has a live mount",
        };
        match mount.teardown() {
            Ok(()) => Err(reason.to_string()),
            Err(error) => Err(format!("{reason}; rejected mount teardown failed: {error}")),
        }
    }

    fn mark_restore_error_if_current(&self, snapshot: &StoredSyncFolder) {
        let lock = self.sync_lock(snapshot.id);
        let _guard = lock.lock();
        if self
            .db
            .sync_folder_get(snapshot.id)
            .ok()
            .flatten()
            .is_some_and(|current| restore_snapshot_matches(snapshot, &current))
        {
            let _ = self
                .db
                .sync_folder_set_state(snapshot.id, "error", now_secs());
        }
    }

    /// Re-establish FUSE mounts for folders left in `ondemand` mode across a daemon
    /// restart (their local dirs are empty on disk — the files live in the cloud).
    /// Best-effort per folder: a missing local path or a failed remote fetch marks
    /// the folder `error` and moves on rather than aborting the rest. Runs on its
    /// own thread from `mount` so the network fetches never block startup
    /// (devices.md Phase 4).
    pub(crate) fn restore_ondemand_mounts(&self) {
        let folders = match self.db.sync_folder_list() {
            Ok(f) => f,
            Err(e) => {
                warn!(error = ?e, "restore on-demand: cannot list folders");
                return;
            }
        };
        for folder in folders {
            if folder.mode != "ondemand" {
                continue;
            }
            let Some(root_uid) = parse_uid(&folder.remote_uid) else {
                warn!(id = folder.id, "restore on-demand: bad remote uid");
                self.mark_restore_error_if_current(&folder);
                continue;
            };
            let root = match self
                .rt
                .block_on(self.client.enumerate_nodes(std::slice::from_ref(&root_uid)))
            {
                Ok(v) => match v.into_iter().next() {
                    Some(n) => n,
                    None => {
                        warn!(id = folder.id, "restore on-demand: remote folder gone");
                        self.mark_restore_error_if_current(&folder);
                        continue;
                    }
                },
                Err(e) => {
                    warn!(id = folder.id, error = %e, "restore on-demand: fetch remote failed");
                    self.mark_restore_error_if_current(&folder);
                    continue;
                }
            };
            let lock = self.sync_lock(folder.id);
            let _guard = lock.lock();
            let current = match self.db.sync_folder_get(folder.id) {
                Ok(Some(current)) if restore_snapshot_matches(&folder, &current) => current,
                Ok(_) => {
                    info!(
                        id = folder.id,
                        "restore on-demand: folder changed while fetching"
                    );
                    continue;
                }
                Err(error) => {
                    warn!(
                        id = folder.id,
                        ?error,
                        "restore on-demand: cannot re-read folder"
                    );
                    continue;
                }
            };
            if !self.mounts.lock().is_accepting() {
                info!(id = folder.id, "restore on-demand: shutdown in progress");
                continue;
            }
            let local = PathBuf::from(&current.local_path);
            // A daemon killed before it could unmount can leave a dead FUSE
            // connection over the path. Clear it only after the row identity
            // and mode have been revalidated under the folder lock.
            if is_stale_mount(&local) {
                warn!(id = folder.id, path = %local.display(), "restore on-demand: clearing stale mount");
                clear_stale_mount(&local);
            }
            if !local.is_dir() {
                warn!(id = folder.id, path = %local.display(), "restore on-demand: local path missing");
                let _ = self
                    .db
                    .sync_folder_set_state(folder.id, "error", now_secs());
                continue;
            }
            match dir_is_empty(&local) {
                Ok(true) => {}
                Ok(false) => {
                    warn!(
                        id = folder.id,
                        path = %local.display(),
                        "restore on-demand: local dir is not empty; refusing to mount over it"
                    );
                    let _ = self
                        .db
                        .sync_folder_set_state(folder.id, "error", now_secs());
                    continue;
                }
                Err(e) => {
                    warn!(id = folder.id, path = %local.display(), error = %e, "restore on-demand: cannot read local dir");
                    let _ = self
                        .db
                        .sync_folder_set_state(folder.id, "error", now_secs());
                    continue;
                }
            }
            match self.spawn_ondemand_mount(&local, root) {
                Ok(mount) => match self.publish_secondary_mount(folder.id, mount) {
                    Ok(()) => {
                        let _ = self.db.sync_folder_set_state(folder.id, "idle", now_secs());
                        info!(id = folder.id, path = %local.display(), "remounted on-demand folder");
                    }
                    Err(error) => {
                        warn!(id = folder.id, %error, "restore on-demand: mount rejected");
                    }
                },
                Err(e) => {
                    warn!(id = folder.id, error = %e, "restore on-demand: mount failed");
                    let _ = self
                        .db
                        .sync_folder_set_state(folder.id, "error", now_secs());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pdfs_core::db::StoredSyncFolder;

    use super::{
        ApplicableMountMode, commit_after_teardown, pending_intent_matches,
        restore_snapshot_matches, select_mode_transition,
    };

    #[test]
    fn unknown_mode_cannot_select_a_destructive_transition() {
        let mut unmounts = 0;
        let mut baseline_clears = 0;
        let mut mode_writes = 0;
        if let Ok(mode) = ApplicableMountMode::parse("streaming") {
            match mode {
                ApplicableMountMode::Mirror => {
                    unmounts += 1;
                    baseline_clears += 1;
                    mode_writes += 1;
                }
                ApplicableMountMode::OnDemand => {
                    mode_writes += 1;
                }
            }
        }
        assert_eq!((unmounts, baseline_clears, mode_writes), (0, 0, 0));
        assert_eq!(
            ApplicableMountMode::parse("streaming"),
            Err("unknown mode \"streaming\" (want mirror|ondemand)".to_string())
        );
        assert_eq!(
            ApplicableMountMode::parse("mirror"),
            Ok(ApplicableMountMode::Mirror)
        );
        assert_eq!(
            ApplicableMountMode::parse("ondemand"),
            Ok(ApplicableMountMode::OnDemand)
        );
    }

    #[test]
    fn unknown_source_mode_with_pending_mirror_cannot_select_a_transition() {
        let mut destructive_helper_reached = false;
        let selected = select_mode_transition("streaming", "mirror");
        if let Ok(Some(_)) = selected {
            destructive_helper_reached = true;
        }

        assert!(!destructive_helper_reached);
        assert_eq!(
            selected,
            Err(
                "unsupported current mode: unknown mode \"streaming\" (want mirror|ondemand)"
                    .to_string()
            )
        );
    }

    fn folder(local_path: &str, remote_uid: &str, mode: &str) -> StoredSyncFolder {
        StoredSyncFolder {
            id: 7,
            local_path: local_path.to_string(),
            remote_uid: remote_uid.to_string(),
            remote_share_id: "share".to_string(),
            mode: mode.to_string(),
            pending_mode: None,
            state: "idle".to_string(),
            last_sync: 0,
        }
    }

    #[test]
    fn stale_restore_identity_or_mode_is_rejected() {
        let snapshot = folder("/mnt/folder", "volume~node", "ondemand");
        assert!(restore_snapshot_matches(
            &snapshot,
            &folder("/mnt/folder", "volume~node", "ondemand")
        ));
        assert!(!restore_snapshot_matches(
            &snapshot,
            &folder("/mnt/replaced", "volume~node", "ondemand")
        ));
        assert!(!restore_snapshot_matches(
            &snapshot,
            &folder("/mnt/folder", "volume~other", "ondemand")
        ));
        assert!(!restore_snapshot_matches(
            &snapshot,
            &folder("/mnt/folder", "volume~node", "mirror")
        ));
    }

    #[test]
    fn teardown_failure_prevents_follow_up_state_changes() {
        let mut commits = 0;
        let result = commit_after_teardown::<_, &str>(Err("busy"), || {
            commits += 1;
        });
        assert_eq!(result, Err("busy"));
        assert_eq!(commits, 0);

        commit_after_teardown::<_, &str>(Ok(()), || commits += 1).unwrap();
        assert_eq!(commits, 1);
    }

    #[test]
    fn canceled_or_replaced_queued_intent_cannot_transition() {
        assert!(pending_intent_matches(Some("ondemand"), Some("ondemand")));
        assert!(!pending_intent_matches(None, Some("ondemand")));
        assert!(!pending_intent_matches(Some("mirror"), Some("ondemand")));
        assert!(pending_intent_matches(Some("mirror"), None));
    }
}
