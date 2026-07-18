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
use pdfs_core::control::{ActivityKind, DeviceInfo, JobItem, SyncFolderInfo, SyncPhase, SyncProgress};
use pdfs_core::db::{StoredDevice, StoredSyncFolder};
use fuser::{Config, MountOption, Session};
use proton_drive_rs::{DeviceType, Node};
use proton_drive_rs::proton_sdk::ids::{DeviceUid, NodeUid};
use tracing::{info, warn};

use super::{
    BackgroundSession, Core, ProtonFs, State, SwitchBlocked, clear_stale_mount, device_type_str, dir_is_empty,
    evict_dir_contents, now_secs, parse_uid, sync_folder_info, this_hostname,
};
use super::sync::{self, base_name};

impl Core {
    // ---- devices ----------------------------------------------------------

    /// List the account's registered devices, flagging the one *this* machine
    /// syncs to so a front-end can treat it as more than another computer in the
    /// list — deleting it takes this machine's synced folders down with it.
    pub(crate) fn list_devices(&self) -> Result<Vec<DeviceInfo>, String> {
        let devices = self
            .rt
            .block_on(self.client.enumerate_devices())
            .map_err(|e| format!("list devices: {e}"))?;
        // No cached device row yet means this machine syncs nothing, so none of
        // the listed devices is ours.
        let this_uid = self.db.device_get().ok().flatten().map(|d| d.uid);
        Ok(devices
            .into_iter()
            .map(|d| {
                let uid = d.uid.to_string();
                DeviceInfo {
                    this_device: this_uid.as_deref() == Some(uid.as_str()),
                    uid,
                    name: d.name.unwrap_or_else(|_| "(unnamed device)".to_string()),
                    device_type: device_type_str(d.device_type).to_string(),
                    last_sync: d.last_sync_time,
                }
            })
            .collect())
    }

    /// Rename a device by its uid.
    pub(crate) fn rename_device(&self, uid: &str, name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("device name must not be empty".to_string());
        }
        let device_uid = DeviceUid::from(uid);
        self.rt
            .block_on(self.client.rename_device(&device_uid, name))
            .map_err(|e| format!("rename device: {e}"))?;
        Ok(())
    }

    /// Delete (deregister) a device by its uid.
    pub(crate) fn delete_device(&self, uid: &str) -> Result<(), String> {
        let device_uid = DeviceUid::from(uid);
        self.rt
            .block_on(self.client.delete_device(&device_uid))
            .map_err(|e| format!("delete device: {e}"))?;
        Ok(())
    }

    // ---- device folder sync (devices.md, Phase 1) -------------------------

    /// Auto-register (or recover) this machine as a Proton Drive Device, caching
    /// it so restarts reuse the same device. Recovery matches an existing remote
    /// Linux device by name before creating a new one, so a lost local record
    /// doesn't orphan the device's root folder.
    pub(crate) fn ensure_device(&self) -> Result<StoredDevice, String> {
        let name = this_hostname();
        // Enumerate the remote devices once: used both to validate any cached
        // record and to recover an existing device by name.
        let remote = self
            .rt
            .block_on(self.client.enumerate_devices())
            .map_err(|e| format!("enumerate devices: {e}"))?;

        // A cached device is only trustworthy if it still exists remotely. A
        // device deleted from another client (or the web UI) leaves a stale row
        // whose root folder is gone, so creating folders under it fails with
        // "parent node is not a folder". Re-register in that case.
        if let Some(dev) = self.db.device_get().map_err(|e| format!("db: {e:?}"))? {
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
                    .map_err(|e| format!("create device: {e}"))?;
                StoredDevice {
                    uid: d.uid.to_string(),
                    share_id: d.share_id.to_string(),
                    root_uid: d.root_folder_uid.to_string(),
                    name,
                    created: d.creation_time,
                }
            }
        };
        self.db.device_set(&dev).map_err(|e| format!("db: {e:?}"))?;
        Ok(dev)
    }

    /// An untrashed folder named `name` directly under the device root, if one
    /// already exists.
    pub(crate) fn find_device_child_folder(
        &self,
        root_uid: &NodeUid,
        name: &str,
    ) -> Result<Option<NodeUid>, String> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_folder_children_node_uids(root_uid))
            .map_err(|e| format!("list device root: {e}"))?;
        if uids.is_empty() {
            return Ok(None);
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| format!("resolve device root children: {e}"))?;
        Ok(nodes
            .into_iter()
            .find(|n| n.is_folder() && !n.trashed && n.name == name)
            .map(|n| n.uid))
    }

    /// Add a local folder to this device's sync set: register the device if
    /// needed, create a matching folder under the device root, upload the local
    /// tree into it once, and record the mapping. Phase 1 is a one-shot upload —
    /// the two-way engine (Phase 2) reconciles later changes.
    pub(crate) fn add_sync_folder(&self, local: &Path) -> Result<StoredSyncFolder, String> {
        let meta =
            std::fs::metadata(local).map_err(|e| format!("stat {}: {e}", local.display()))?;
        if !meta.is_dir() {
            return Err(format!("{} is not a directory", local.display()));
        }
        let local = local
            .canonicalize()
            .map_err(|e| format!("canonicalize {}: {e}", local.display()))?;
        let local_str = local.to_string_lossy().to_string();

        // Reject duplicates up front for a clear error (UNIQUE would also catch it).
        if self
            .db
            .sync_folder_list()
            .map_err(|e| format!("db: {e:?}"))?
            .iter()
            .any(|f| f.local_path == local_str)
        {
            return Err(format!("{} is already synced", local.display()));
        }

        let name = local
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("unusable folder name: {}", local.display()))?
            .to_string();

        let device = self.ensure_device()?;
        let root_uid = parse_uid(&device.root_uid)
            .ok_or_else(|| format!("bad device root uid: {}", device.root_uid))?;

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
                .map_err(|e| format!("create device folder {name}: {e}"))?,
        };

        let id = self
            .db
            .sync_folder_add(&local_str, &remote_root.to_string(), &device.share_id)
            .map_err(|e| format!("db: {e:?}"))?;

        // Hand the initial upload to the sync engine: an empty baseline against a
        // full local tree reconciles as "upload everything", and the folder is
        // added to the filesystem watch set in the same pass.
        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
        let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));

        info!(local = %local.display(), id, "added sync folder");
        self.db
            .sync_folder_get(id)
            .map_err(|e| format!("db: {e:?}"))?
            .ok_or_else(|| "sync folder vanished after insert".to_string())
    }

    /// List this device's synced folders for the front-ends, each carrying the
    /// live progress of its pass when one is running.
    pub(crate) fn list_sync_folders(&self) -> Result<Vec<SyncFolderInfo>, String> {
        let progress = self.sync_progress.lock();
        Ok(self
            .db
            .sync_folder_list()
            .map_err(|e| format!("db: {e:?}"))?
            .into_iter()
            .map(|f| {
                let live = progress.get(&f.id).cloned();
                sync_folder_info(f, live)
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
        self.sync_locks
            .lock()
            .entry(id)
            .or_default()
            .clone()
    }

    /// Remove a synced folder from the sync set. `delete_remote` also deletes its
    /// folder under the device root; otherwise the cloud copy is left in place.
    pub(crate) fn remove_sync_folder(&self, id: i64, delete_remote: bool) -> Result<(), String> {
        let folder = self
            .db
            .sync_folder_get(id)
            .map_err(|e| format!("db: {e:?}"))?
            .ok_or_else(|| format!("no synced folder with id {id}"))?;
        // An `ondemand` folder *is* a FUSE mount over its local path, so dropping
        // only the row would strand the mount: the path would keep serving a
        // folder the daemon no longer tracks, and nothing would ever unmount it.
        // Tear it down first — including before trashing the remote tree it
        // serves, which would otherwise leave it answering for deleted nodes.
        if let Some(session) = self.mounts.lock().remove(&id) {
            if let Err(e) = session.umount_and_join() {
                warn!(id, error = %e, "unmount on-demand folder failed");
            } else {
                info!(id, path = %folder.local_path, "unmounted on-demand folder");
            }
        }
        if delete_remote
            && let Some(uid) = parse_uid(&folder.remote_uid)
            && let Err(e) = self.rt.block_on(self.client.trash_nodes(&[uid]))
        {
            warn!(id, error = %e, "delete remote device folder failed");
        }
        if !self
            .db
            .sync_folder_remove(id)
            .map_err(|e| format!("db: {e:?}"))?
        {
            return Err(format!("no synced folder with id {id}"));
        }
        // Stop watching the folder we just dropped.
        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
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
        fork.state = Arc::new(Mutex::new(State {
            entries: HashMap::new(),
            by_uid: HashMap::new(),
            children: HashMap::new(),
            next_ino: 2,
            handles: HashMap::new(),
            next_fh: 1,
            db: self.db.clone(),
        }));
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
    pub(crate) fn request_sync_folder_mode(&self, id: i64, mode: &str) -> Result<String, String> {
        if mode != "mirror" && mode != "ondemand" {
            return Err(format!("unknown mode {mode:?} (want mirror|ondemand)"));
        }
        let folder = self
            .db
            .sync_folder_get(id)
            .map_err(|e| format!("db: {e:?}"))?
            .ok_or_else(|| format!("no synced folder with id {id}"))?;
        if folder.mode == mode {
            // Already there. A queued switch the other way is the user changing
            // their mind back before it landed, so withdraw it.
            if folder.pending_mode.is_some() {
                self.db
                    .sync_folder_set_pending_mode(id, None)
                    .map_err(|e| format!("db: {e:?}"))?;
                let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
                return Ok(format!("staying {mode}"));
            }
            return Ok(format!("already {mode}"));
        }

        match self.apply_sync_folder_mode(id, mode) {
            Ok(message) => Ok(message),
            // Not switchable this instant — remember the intent instead of making
            // the user retry, and kick a pass to clear whatever is in the way.
            Err(SwitchBlocked::NotNow) => {
                self.db
                    .sync_folder_set_pending_mode(id, Some(mode))
                    .map_err(|e| format!("db: {e:?}"))?;
                let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));
                Ok(match mode {
                    "ondemand" => format!(
                        "{} will go on-demand once its local changes are uploaded",
                        base_name(&folder.local_path)
                    ),
                    _ => format!(
                        "{} will start mirroring once the current sync finishes",
                        base_name(&folder.local_path)
                    ),
                })
            }
            Err(SwitchBlocked::Failed(e)) => Err(e),
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
        let Some(mode) = folder.pending_mode.as_deref() else {
            return;
        };
        if folder.mode == mode {
            let _ = self.db.sync_folder_set_pending_mode(id, None);
            return;
        }
        match self.apply_sync_folder_mode(id, mode) {
            Ok(message) => {
                info!(id, mode, "applied queued mode switch");
                self.log_activity(ActivityKind::Sync, &message, "", true);
            }
            // Still blocked: the pass could not get everything up, so the local copy
            // is not safe to evict yet. Leave the request standing — the next pass
            // (poll, or the retry the engine schedules) tries again.
            Err(SwitchBlocked::NotNow) => {
                info!(id, mode, "queued mode switch still waiting");
            }
            Err(SwitchBlocked::Failed(e)) => {
                warn!(id, mode, error = %e, "queued mode switch failed; withdrawing");
                let _ = self.db.sync_folder_set_pending_mode(id, None);
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
    pub(crate) fn apply_sync_folder_mode(&self, id: i64, mode: &str) -> Result<String, SwitchBlocked> {
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
        if folder.mode == mode {
            return Ok(format!("already {mode}"));
        }
        let local = PathBuf::from(&folder.local_path);

        match mode {
            "ondemand" => {
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

                // Reclaim the disk: empty the local dir (keep it as the mountpoint).
                evict_dir_contents(&local).map_err(|e| {
                    SwitchBlocked::Failed(format!("evict {}: {e}", local.display()))
                })?;

                let session = self
                    .spawn_ondemand_mount(&local, root)
                    .map_err(SwitchBlocked::Failed)?;
                self.mounts.lock().insert(id, session);
                // Persist the mode only now that the mount is actually up. Writing it
                // first would strand the folder on any failure below: the engine skips
                // non-mirror folders, so an `ondemand` row with no mount is a folder
                // that is neither mirrored nor browsable, and nothing retries it.
                // Failing before this point leaves it `mirror`, and the next pass
                // re-downloads whatever eviction removed.
                self.db
                    .sync_folder_set_mode(id, "ondemand")
                    .map_err(|e| SwitchBlocked::Failed(format!("db: {e:?}")))?;
                let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
                self.db.sync_folder_set_state(id, "idle", now_secs()).ok();
                info!(id, path = %local.display(), "mounted sync folder on-demand");
                Ok(format!("{} is now on-demand", local.display()))
            }
            _ => {
                // ondemand→mirror: tear down the secondary mount first.
                if let Some(session) = self.mounts.lock().remove(&id)
                    && let Err(e) = session.umount_and_join()
                {
                    warn!(id, error = %e, "unmount on-demand folder failed");
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
    pub(crate) fn spawn_ondemand_mount(&self, local: &Path, root: Node) -> Result<BackgroundSession, String> {
        clear_stale_mount(local);
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName("protondrive".to_string()),
            MountOption::Subtype("protondrive".to_string()),
            MountOption::DefaultPermissions,
        ];
        let fs = ProtonFs::new(self.fork_state(), root);
        Session::new(fs, local, &config)
            .and_then(|s| s.spawn())
            .map_err(|e| format!("mount {}: {e}", local.display()))
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
            let local = PathBuf::from(&folder.local_path);
            if !local.is_dir() {
                warn!(id = folder.id, path = %local.display(), "restore on-demand: local path missing");
                let _ = self
                    .db
                    .sync_folder_set_state(folder.id, "error", now_secs());
                continue;
            }
            // An `ondemand` folder's local dir is empty by construction — the switch
            // evicts it. Finding files there means the row is lying (a switch that
            // died between persisting the mode and evicting), and mounting over them
            // would hide real local data behind the remote tree. Leave the files
            // alone and let the user resolve it.
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
            let Some(root_uid) = parse_uid(&folder.remote_uid) else {
                warn!(id = folder.id, "restore on-demand: bad remote uid");
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
                        let _ = self
                            .db
                            .sync_folder_set_state(folder.id, "error", now_secs());
                        continue;
                    }
                },
                Err(e) => {
                    warn!(id = folder.id, error = %e, "restore on-demand: fetch remote failed");
                    let _ = self
                        .db
                        .sync_folder_set_state(folder.id, "error", now_secs());
                    continue;
                }
            };
            match self.spawn_ondemand_mount(&local, root) {
                Ok(session) => {
                    self.mounts.lock().insert(folder.id, session);
                    let _ = self.db.sync_folder_set_state(folder.id, "idle", now_secs());
                    info!(id = folder.id, path = %local.display(), "remounted on-demand folder");
                }
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
