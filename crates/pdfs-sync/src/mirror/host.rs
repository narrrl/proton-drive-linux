//! What the sync engine owns, and the narrow port through which it reaches the
//! rest of the daemon.
//!
//! The engine used to be a set of methods on the daemon's `Core`, the struct
//! every subsystem shares, so any part of a pass could read or lock anything in the
//! daemon. Its own state now lives in [`SyncParts`], next to the handles it
//! borrows (its tables through [`SyncStore`], Drive, the content cache), and everything else it
//! needs from the daemon goes through [`SyncHost`]. A [`Syncer`] is the two
//! together: the value the engine's methods run on.
//!
//! The daemon implements [`SyncHost`] in `pdfs-fuse`.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use parking_lot::Mutex;
use pdfs_core::CoreResult;
use pdfs_core::control::{ActivityKind, SyncPhase, SyncProgress};
use pdfs_core::lockrank::{Rank, Ranked};
use pdfs_core::shutdown::Shutdown;
use pdfs_core::transfers::TransferRegistry;
use pdfs_drive::DriveApi;
use pdfs_store::cache::ContentCache;
use proton_drive_rs::Thumbnail;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::info;

use super::{EngineHealth, SyncMsg};
use crate::store::SyncStore;

/// What the sync engine needs from the daemon that it does not own.
pub trait SyncHost: Send + Sync {
    /// Append an entry to the activity feed.
    fn log_activity(&self, kind: ActivityKind, target: String, detail: String, ok: bool);
    /// Forget the cached listing of `parent`, after a pass changed it on Drive.
    fn invalidate_children_of(&self, parent: &NodeUid);
    /// The folders above `start` on Drive, nearest first, read from the server.
    fn attach_ancestors(&self, start: &NodeUid) -> CoreResult<Vec<NodeUid>>;
    /// Apply a mode switch queued for synced folder `id`, if it can take it now.
    fn settle_pending_mode(&self, id: i64);
    /// Whether the user has paused all syncing.
    fn sync_paused(&self) -> bool;
    /// Whether the user has paused synced folder `id`.
    fn sync_folder_paused(&self, id: i64) -> bool;
    /// The thumbnails to upload with the file at `path`, named `name`; none
    /// when it is not an image or video this machine can render. Blocking.
    fn upload_thumbnails(&self, path: &Path, name: &str) -> Vec<Thumbnail>;
}

/// The sync engine's state, and the daemon handles it uses.
#[derive(Clone)]
pub struct SyncParts {
    /// The engine's tables: synced folders, baselines and saved walks.
    pub store: Arc<dyn SyncStore>,
    pub drive: Arc<dyn DriveApi>,
    pub rt: tokio::runtime::Handle,
    pub cache: Arc<ContentCache>,
    pub transfers: Arc<TransferRegistry>,
    pub shutdown: Arc<Shutdown>,
    /// My files' root: an attached folder must stay below it.
    pub primary_root_uid: NodeUid,

    /// Live per-folder progress, keyed by sync-folder id, so `ListSyncFolders`
    /// can say what a pass is doing rather than just "syncing". An entry exists
    /// only while that folder's pass is running.
    pub progress: Arc<Mutex<HashMap<i64, SyncProgress>>>,
    /// Whether the engine is alive, and which pass it is on.
    pub engine: Arc<EngineHealth>,
    /// Channel to the engine: nudges it to reconcile a folder, reconcile
    /// everything, or re-scan its watch set.
    pub tx: std::sync::mpsc::Sender<SyncMsg>,
    /// Per-folder locks, held for a whole pass and for a whole mode switch. A
    /// `mirror→ondemand` flip evicts the local tree and mounts FUSE over it, so
    /// it must never overlap a pass that is walking and uploading that tree.
    pub locks: Arc<Mutex<HashMap<i64, Arc<Ranked<()>>>>>,
    /// Per-folder paths the last pass could not read, so the activity feed
    /// says so when the set changes rather than on every poll (B196).
    pub unscanned: Arc<Mutex<HashMap<i64, Vec<String>>>>,
    /// Folders whose running pass should stop at its next checkpoint, because
    /// a removal is waiting for the folder's lock (B294).
    pub abandon: Arc<Mutex<HashSet<i64>>>,
    /// When the Drive event feed was last read, epoch seconds; 0 before the
    /// first read. A pass trusts the feed's dirty folders only while it is
    /// being read.
    pub events_read_at: Arc<AtomicI64>,
    /// Where each mirror folder changed on disk since a pass last looked,
    /// as the filesystem watcher reports it.
    pub local_dirty: Arc<crate::local_dirty::LocalDirty>,
    /// What each mirror folder's sync knows about its coverage (`root`).
    pub roots: Arc<crate::root::Roots>,
    /// When each step of a pass may start: its uploads, downloads and folder
    /// creations.
    pub admission: Arc<dyn pdfs_sched::Admission>,
}

impl SyncParts {
    /// The lock guarding synced folder `id` against a pass and a mode switch
    /// running at once.
    pub fn lock(&self, id: i64) -> Arc<Ranked<()>> {
        self.locks
            .lock()
            .entry(id)
            .or_insert_with(|| Arc::new(Ranked::new(Rank::SyncFolder, ())))
            .clone()
    }

    // ---- live progress ----------------------------------------------------

    /// Start tracking a pass over `folder_id`, in [`SyncPhase::Scanning`].
    pub fn progress_begin(&self, folder_id: i64) {
        self.progress.lock().insert(
            folder_id,
            SyncProgress {
                phase: SyncPhase::Scanning,
                done: 0,
                total: 0,
                current: String::new(),
            },
        );
    }

    /// Apply `f` to a folder's live progress, if a pass is running for it.
    fn progress_update(&self, folder_id: i64, f: impl FnOnce(&mut SyncProgress)) {
        let now = self.progress.lock().get_mut(&folder_id).map(|p| {
            f(p);
            (p.phase, p.done, p.total)
        });
        if let Some((folder, age)) = self.engine.moved()
            && let Some((phase, done, total)) = now
            && folder == folder_id
        {
            info!(
                id = folder,
                ?phase,
                done,
                total,
                elapsed_s = age.as_secs(),
                "sync: pass still running"
            );
        }
    }

    /// Set the number of items the scan expects to check, from the size of the
    /// last pass's baseline. Only an estimate — the folder may have grown since —
    /// but it turns the scan from an indeterminate pulse into a bar that moves,
    /// which is the difference between "it's stuck" and "it's working" on a folder
    /// whose walk takes minutes.
    pub fn progress_scan_total(&self, folder_id: i64, n: usize) {
        self.progress_update(folder_id, |p| p.total = n);
    }

    /// Note that the scan has checked one more item, named `name`.
    pub fn progress_scanned(&self, folder_id: i64, name: &str) {
        self.progress_update(folder_id, |p| {
            p.done += 1;
            p.current = name.to_string();
        });
    }

    /// Note that the scan has checked `n` more items at once, read back from
    /// a walk an earlier pass saved.
    pub fn progress_scanned_many(&self, folder_id: i64, n: usize) {
        self.progress_update(folder_id, |p| p.done += n);
    }

    /// Note that `n` more items have been queued for this pass, and that it has
    /// moved on from scanning to applying the diff. The scan's counts are dropped:
    /// they measured a different quantity (items checked, not items to apply), so
    /// carrying them over would start the applying bar at a meaningless fraction.
    pub fn progress_queued(&self, folder_id: i64, n: usize) {
        self.progress_update(folder_id, |p| {
            if p.phase == SyncPhase::Scanning {
                p.phase = SyncPhase::Applying;
                p.done = 0;
                p.total = 0;
                p.current.clear();
            }
            p.total += n;
        });
    }

    /// Note that work has started on `name` (shown as the pass's current item).
    pub fn progress_started(&self, folder_id: i64, name: &str) {
        self.progress_update(folder_id, |p| p.current = name.to_string());
    }

    /// Note that one queued item finished, whether it succeeded or not.
    pub fn progress_finished(&self, folder_id: i64) {
        self.progress_update(folder_id, |p| {
            p.done += 1;
            p.current.clear();
        });
    }

    /// Stop tracking a pass — no progress is reported for the folder until the
    /// next [`progress_begin`](Self::progress_begin).
    pub fn progress_end(&self, folder_id: i64) {
        self.progress.lock().remove(&folder_id);
    }
}

/// The sync engine's state and its port to the daemon: what a pass runs on.
#[derive(Clone)]
pub struct Syncer {
    parts: SyncParts,
    host: Arc<dyn SyncHost>,
}

impl Deref for Syncer {
    type Target = SyncParts;

    fn deref(&self) -> &SyncParts {
        &self.parts
    }
}

impl Syncer {
    pub fn new(parts: SyncParts, host: Arc<dyn SyncHost>) -> Self {
        Self { parts, host }
    }

    pub fn log_activity(
        &self,
        kind: ActivityKind,
        target: impl Into<String>,
        detail: impl std::fmt::Display,
        ok: bool,
    ) {
        self.host
            .log_activity(kind, target.into(), detail.to_string(), ok);
    }

    pub fn invalidate_children_of(&self, parent: &NodeUid) {
        self.host.invalidate_children_of(parent);
    }

    pub fn attach_ancestors(&self, start: &NodeUid) -> CoreResult<Vec<NodeUid>> {
        self.host.attach_ancestors(start)
    }

    pub fn settle_pending_mode(&self, id: i64) {
        self.host.settle_pending_mode(id);
    }

    pub fn sync_paused(&self) -> bool {
        self.host.sync_paused()
    }

    pub fn sync_folder_paused(&self, id: i64) -> bool {
        self.host.sync_folder_paused(id)
    }

    /// [`SyncHost::upload_thumbnails`], off the async runtime's threads.
    pub async fn thumbnails_for_upload(&self, path: PathBuf, name: String) -> Vec<Thumbnail> {
        let host = self.host.clone();
        tokio::task::spawn_blocking(move || host.upload_thumbnails(&path, &name))
            .await
            .unwrap_or_default()
    }
}
