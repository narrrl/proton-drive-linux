//! Two-way folder sync engine (devices.md Phase 2).
//!
//! Each `mirror`-mode synced folder is kept mirrored between a local directory
//! and a folder under this machine's Proton Drive device. A local filesystem
//! watcher ([`notify`]) and a periodic remote poll both feed a debounced
//! reconcile pass that diffs three states — the current local tree, the current
//! remote tree, and the last-synced baseline in `sync_entry` — to decide, per
//! path, which side changed and how to propagate it.
//!
//! Change detection is `(mtime, size)` on each side (no cheap content hash is
//! exposed). The baseline is refreshed from ground truth after every applied
//! action, so a file just uploaded or downloaded reads as "unchanged" on the
//! next pass and never ping-pongs.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime};

use notify::{RecursiveMode, Watcher};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use tracing::{info, warn};

use crate::transfers::{CountingWriter, OwnedCountingReader};
use crate::{Core, now_secs, parse_uid};
use pdfs_core::control::{ActivityKind, TransferDirection};
use pdfs_core::db::{StoredSyncEntry, StoredSyncFolder};

/// Coalesce a burst of filesystem events into a single reconcile per folder.
const DEBOUNCE: Duration = Duration::from_secs(2);
/// How often to re-walk remotes so changes made on other clients are pulled in.
const POLL_INTERVAL: Duration = Duration::from_secs(120);
/// How many uploads/downloads/folder-creations a reconcile runs at once. The
/// per-item work is a network round-trip, so a folder with thousands of files is
/// unusable done one-at-a-time; this bounds concurrency like the bulk-upload
/// engine ([`crate::run_uploads`]) does.
const SYNC_CONCURRENCY: usize = 8;

/// A message to the sync engine's control thread.
pub(crate) enum SyncMsg {
    /// Reconcile one folder by id (a local change, or a targeted `SyncNow`).
    Reconcile(i64),
    /// Reconcile every mirror folder (periodic poll, or startup).
    ReconcileAll,
    /// Re-read the folder set and adjust the filesystem watches (after add/remove).
    Rewatch,
}

/// Start the sync engine: a control thread that owns the filesystem watcher and
/// serialises reconcile passes, plus a periodic poll thread. The engine receives
/// on `rx`; senders live in [`Core::sync_tx`].
pub(crate) fn spawn(core: Core, rx: Receiver<SyncMsg>) {
    if let Err(e) = std::thread::Builder::new()
        .name("pdfs-sync".into())
        .spawn(move || engine_loop(core, rx))
    {
        warn!(error = %e, "failed to start sync engine");
    }
}

/// The engine control loop. Single-threaded so reconcile passes never overlap.
fn engine_loop(core: Core, rx: Receiver<SyncMsg>) {
    // Paths the watcher currently covers, mapped to their folder id, so an event
    // path can be resolved back to the folder it belongs to.
    let watched: Mutex<Vec<(PathBuf, i64)>> = Mutex::new(Vec::new());

    let tx_events = core.sync_tx.clone();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // Ignore pure metadata/access noise; content changes are what matter.
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let _ = tx_events.send(SyncMsg::ReconcileAll);
        }) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "filesystem watcher unavailable; sync is poll-only");
                // Still run the loop so polling and explicit SyncNow work.
                poll_only_loop(&core, rx);
                return;
            }
        };

    // Periodic remote poll.
    {
        let tx = core.sync_tx.clone();
        std::thread::Builder::new()
            .name("pdfs-sync-poll".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(POLL_INTERVAL);
                    if tx.send(SyncMsg::ReconcileAll).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }

    rewatch(&core, &mut watcher, &watched);
    reconcile_all(&core);

    while let Ok(msg) = rx.recv() {
        let mut ids: HashSet<i64> = HashSet::new();
        let mut all = false;
        let mut do_rewatch = false;
        classify(msg, &mut ids, &mut all, &mut do_rewatch);
        // Debounce: let a burst settle, then drain everything queued behind it.
        std::thread::sleep(DEBOUNCE);
        while let Ok(m) = rx.try_recv() {
            classify(m, &mut ids, &mut all, &mut do_rewatch);
        }

        if do_rewatch {
            rewatch(&core, &mut watcher, &watched);
        }
        if all {
            reconcile_all(&core);
        } else {
            for id in ids {
                if let Ok(Some(folder)) = core.db.sync_folder_get(id) {
                    core.reconcile_folder(&folder);
                }
            }
        }
    }
}

/// Fallback loop when no filesystem watcher could be created: poll + SyncNow only.
fn poll_only_loop(core: &Core, rx: Receiver<SyncMsg>) {
    {
        let tx = core.sync_tx.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL_INTERVAL);
                if tx.send(SyncMsg::ReconcileAll).is_err() {
                    break;
                }
            }
        });
    }
    reconcile_all(core);
    while let Ok(msg) = rx.recv() {
        let mut ids: HashSet<i64> = HashSet::new();
        let mut all = false;
        let mut do_rewatch = false;
        classify(msg, &mut ids, &mut all, &mut do_rewatch);
        std::thread::sleep(DEBOUNCE);
        while let Ok(m) = rx.try_recv() {
            classify(m, &mut ids, &mut all, &mut do_rewatch);
        }
        if all {
            reconcile_all(core);
        } else {
            for id in ids {
                if let Ok(Some(folder)) = core.db.sync_folder_get(id) {
                    core.reconcile_folder(&folder);
                }
            }
        }
    }
}

fn classify(msg: SyncMsg, ids: &mut HashSet<i64>, all: &mut bool, rewatch: &mut bool) {
    match msg {
        SyncMsg::Reconcile(id) => {
            ids.insert(id);
        }
        SyncMsg::ReconcileAll => *all = true,
        SyncMsg::Rewatch => *rewatch = true,
    }
}

/// Reconcile every mirror folder in turn.
fn reconcile_all(core: &Core) {
    let folders = match core.db.sync_folder_list() {
        Ok(f) => f,
        Err(e) => {
            warn!(error = ?e, "sync: cannot list folders");
            return;
        }
    };
    for folder in folders {
        core.reconcile_folder(&folder);
    }
}

/// Bring the filesystem watches in line with the current mirror-folder set:
/// watch newly-added folders, drop removed ones.
fn rewatch(
    core: &Core,
    watcher: &mut notify::RecommendedWatcher,
    watched: &Mutex<Vec<(PathBuf, i64)>>,
) {
    let want: Vec<(PathBuf, i64)> = match core.db.sync_folder_list() {
        Ok(f) => f
            .into_iter()
            .filter(|f| f.mode == "mirror")
            .map(|f| (PathBuf::from(f.local_path), f.id))
            .collect(),
        Err(e) => {
            warn!(error = ?e, "sync: cannot list folders to watch");
            return;
        }
    };
    let mut have = watched.lock().unwrap();
    // Drop watches no longer wanted.
    have.retain(|(path, _)| {
        if want.iter().any(|(p, _)| p == path) {
            true
        } else {
            let _ = watcher.unwatch(path);
            false
        }
    });
    // Add newly wanted watches.
    for (path, id) in &want {
        if have.iter().any(|(p, _)| p == path) {
            continue;
        }
        match watcher.watch(path, RecursiveMode::Recursive) {
            Ok(()) => have.push((path.clone(), *id)),
            Err(e) => warn!(path = %path.display(), error = %e, "sync: watch failed"),
        }
    }
}

// ---- reconcile ------------------------------------------------------------

/// One item found while walking a local tree.
struct LocalItem {
    is_dir: bool,
    mtime: i64,
    size: i64,
}

/// One item found while walking a remote tree.
struct RemoteItem {
    uid: NodeUid,
    is_dir: bool,
    mtime: i64,
    size: i64,
}

/// The result of a reconcile pass: what it moved, how many paths were kept as
/// conflict copies, and how many failed to apply (and so still need another
/// pass). The counts drive both the folder's state and its activity summary.
#[derive(Default)]
struct Outcome {
    uploaded: usize,
    downloaded: usize,
    created: usize,
    deleted: usize,
    conflicts: usize,
    errors: usize,
}

impl Outcome {
    /// Fold in one applied op.
    fn record(&mut self, applied: &Applied) {
        match applied {
            Applied::Dir(..) => self.created += 1,
            Applied::Uploaded => self.uploaded += 1,
            Applied::Downloaded => self.downloaded += 1,
            Applied::Conflict => self.conflicts += 1,
        }
    }

    /// Whether the pass moved nothing at all — the common case on a poll of an
    /// unchanged folder, which should not add a line to the activity feed.
    fn is_empty(&self) -> bool {
        self.uploaded == 0
            && self.downloaded == 0
            && self.created == 0
            && self.deleted == 0
            && self.conflicts == 0
            && self.errors == 0
    }

    /// A human summary of the pass: "3 uploaded, 1 downloaded, 2 failed".
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (n, label) in [
            (self.uploaded, "uploaded"),
            (self.downloaded, "downloaded"),
            (self.created, "folder(s) created"),
            (self.deleted, "deleted"),
            (self.conflicts, "conflicted"),
            (self.errors, "failed"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {label}"));
            }
        }
        parts.join(", ")
    }
}

/// A network operation queued during classification and run concurrently in a
/// per-depth batch. Parent uids are resolved up front (the parent folder is one
/// depth shallower and already created), so tasks share nothing mutable.
enum Pending {
    /// Create a new remote folder under `parent`.
    CreateDir { rel: String, parent: NodeUid },
    /// Upload a brand-new local file into `parent`.
    UploadNew { rel: String, parent: NodeUid },
    /// Upload a changed local file as a new revision of `uid`.
    UploadRevision { rel: String, uid: NodeUid },
    /// Download remote `uid` to the local path, stamping `mtime`. `size` is the
    /// remote's reported size, used as the transfer's expected total.
    Download {
        rel: String,
        uid: NodeUid,
        mtime: i64,
        size: i64,
    },
    /// Both sides changed: set the local copy aside, then download remote `uid`.
    Conflict {
        rel: String,
        uid: NodeUid,
        mtime: i64,
        size: i64,
    },
}

impl Pending {
    /// The path this op acts on, relative to the folder root.
    fn rel(&self) -> &str {
        match self {
            Pending::CreateDir { rel, .. }
            | Pending::UploadNew { rel, .. }
            | Pending::UploadRevision { rel, .. }
            | Pending::Download { rel, .. }
            | Pending::Conflict { rel, .. } => rel,
        }
    }

    /// How this op reads in the activity feed.
    fn kind(&self) -> ActivityKind {
        match self {
            Pending::CreateDir { .. } => ActivityKind::CreateFolder,
            Pending::UploadNew { .. } | Pending::UploadRevision { .. } => ActivityKind::Upload,
            Pending::Download { .. } | Pending::Conflict { .. } => ActivityKind::Download,
        }
    }

    /// The activity line's detail, which distinguishes ops that share a kind.
    fn detail(&self) -> &'static str {
        match self {
            Pending::CreateDir { .. } => "on Drive",
            Pending::UploadNew { .. } => "new file",
            Pending::UploadRevision { .. } => "new version",
            Pending::Download { .. } => "from Drive",
            Pending::Conflict { .. } => "local changes kept as a conflict copy",
        }
    }
}

/// What a [`Pending`] op did, so the reconcile can update shared state on the
/// engine thread (never inside a task).
enum Applied {
    /// A folder was created; register `uid` in `remote_dirs` for its children.
    Dir(String, NodeUid),
    /// A local file was uploaded (new file or new revision).
    Uploaded,
    /// A remote file was downloaded.
    Downloaded,
    /// A conflict copy was kept.
    Conflict,
}

impl Core {
    /// Reconcile one synced folder, updating its `state` column. `ondemand`
    /// folders are live FUSE mounts, not mirrored, so they are skipped here.
    pub(crate) fn reconcile_folder(&self, folder: &StoredSyncFolder) {
        if folder.mode != "mirror" {
            return;
        }
        // Hold the folder's lock for the whole pass so a mode switch can't evict the
        // local tree (and mount FUSE over it) while we walk and upload it.
        let lock = self.sync_lock(folder.id);
        let _guard = lock.lock().unwrap();
        // `folder` was read before the lock; a switch may have landed in between, so
        // re-read and re-check the mode rather than trusting the snapshot.
        match self.db.sync_folder_get(folder.id) {
            Ok(Some(current)) if current.mode == "mirror" => {}
            Ok(Some(_)) => return,
            Ok(None) => return,
            Err(e) => {
                warn!(id = folder.id, error = ?e, "sync: cannot re-read folder; skipping");
                return;
            }
        }
        let Some(remote_root) = parse_uid(&folder.remote_uid) else {
            warn!(id = folder.id, "sync: bad remote uid; skipping");
            return;
        };
        let local_root = PathBuf::from(&folder.local_path);
        if !local_root.is_dir() {
            warn!(id = folder.id, path = %local_root.display(), "sync: local root missing");
            let _ = self
                .db
                .sync_folder_set_state(folder.id, "error", now_secs());
            return;
        }

        let _ = self
            .db
            .sync_folder_set_state(folder.id, "syncing", folder.last_sync);
        let name = base_name(&folder.local_path);
        self.progress_begin(folder.id);
        let result = self.do_reconcile(folder.id, &local_root, &remote_root);
        self.progress_end(folder.id);
        match result {
            Ok(outcome) => {
                // A folder only reaches `idle` when every path applied cleanly —
                // an un-uploaded file must keep it out of `idle` so it can't be
                // switched to on-demand (which evicts the local copy).
                let state = if outcome.errors > 0 {
                    "error"
                } else if outcome.conflicts > 0 {
                    "conflict"
                } else {
                    "idle"
                };
                let _ = self.db.sync_folder_set_state(folder.id, state, now_secs());
                // Summarise a pass that actually moved something. A poll over an
                // unchanged folder does nothing and says nothing — otherwise the
                // feed would fill with "synced, 0 changes" every two minutes.
                if !outcome.is_empty() {
                    self.log_activity(
                        ActivityKind::Sync,
                        name,
                        outcome.summary(),
                        outcome.errors == 0,
                    );
                }
                // Surface a new conflict once (not on every poll while it persists).
                if state == "conflict" && folder.state != "conflict" {
                    self.log_activity(
                        ActivityKind::Sync,
                        format!("sync conflict in {name}"),
                        format!("{} file(s) kept as conflict copies", outcome.conflicts),
                        false,
                    );
                }
                // Surface partial failures once, too — the pass still uploaded
                // everything it could, but some paths need another attempt.
                if state == "error" && folder.state != "error" {
                    self.log_activity(
                        ActivityKind::Sync,
                        format!("sync incomplete for {name}"),
                        format!("{} item(s) failed; will retry", outcome.errors),
                        false,
                    );
                }
            }
            Err(e) => {
                warn!(id = folder.id, error = %e, "sync: reconcile failed");
                let _ = self
                    .db
                    .sync_folder_set_state(folder.id, "error", now_secs());
                if folder.state != "error" {
                    self.log_activity(
                        ActivityKind::Sync,
                        format!("sync failed for {name}"),
                        e,
                        false,
                    );
                }
            }
        }
    }

    /// The diff-and-apply core. A single path failing to apply (a transient
    /// upload error, an unreadable file, a name collision) must not abort the
    /// whole pass and strand every other path — otherwise one bad file in a
    /// large folder leaves the sync permanently incomplete and blocks the
    /// on-demand switch. So per-path failures are logged and counted in
    /// [`Outcome::errors`], and the walk continues. Only a failure to establish
    /// the diff itself (the walks / baseline load) aborts with `Err`.
    fn do_reconcile(
        &self,
        folder_id: i64,
        local_root: &Path,
        remote_root: &NodeUid,
    ) -> Result<Outcome, String> {
        let mut local: HashMap<String, LocalItem> = HashMap::new();
        walk_local(local_root, local_root, &mut local)?;

        // Loaded before the walk, which uses it to tell which remote files are
        // unchanged and so can skip decrypting their claimed size.
        let baseline = self
            .db
            .sync_entries(folder_id)
            .map_err(|e| format!("load baseline: {e:?}"))?;

        let mut remote: HashMap<String, RemoteItem> = HashMap::new();
        let mut remote_dirs: HashMap<String, NodeUid> = HashMap::new();
        remote_dirs.insert(String::new(), remote_root.clone());
        self.walk_remote(remote_root, "", &mut remote, &mut remote_dirs, &baseline)?;

        // Union of every path across the three states, shallow paths first so a
        // parent folder is created before its children are placed inside it.
        let mut paths: HashSet<String> = HashSet::new();
        paths.extend(local.keys().cloned());
        paths.extend(remote.keys().cloned());
        paths.extend(baseline.keys().cloned());
        let mut order: Vec<String> = paths.into_iter().collect();
        order.sort_by_key(|p| p.matches('/').count());

        let mut outcome = Outcome::default();
        // Folders to delete, collected here and removed deepest-first at the end
        // so a parent is never removed before its children.
        let mut delete_local_dirs: Vec<String> = Vec::new();
        let mut delete_remote_dirs: Vec<(String, NodeUid)> = Vec::new();

        // Single depth-ascending pass. Network work (folder creation, file
        // upload/download) is queued into `batch` and flushed concurrently at
        // each depth boundary — so a parent folder is created before the deeper
        // paths inside it are classified (their parent uid is then in
        // `remote_dirs`), while everything within a depth runs in parallel.
        // Interleaving by depth also means files start uploading as soon as
        // their folder exists, instead of after every folder in the tree.
        // Cheap, dependency-free work (local mkdir, baseline rows, entry
        // removals, deletions) is done inline; a per-item failure is logged and
        // counted, never fatal.
        let mut batch: Vec<Pending> = Vec::new();
        let mut batch_depth = 0usize;

        for rel in &order {
            let depth = rel.matches('/').count();
            if depth > batch_depth {
                self.flush_batch(
                    folder_id,
                    local_root,
                    &mut remote_dirs,
                    std::mem::take(&mut batch),
                    &mut outcome,
                );
                batch_depth = depth;
            }

            let l = local.get(rel);
            let r = remote.get(rel);
            let is_dir = l.map(|x| x.is_dir).or(r.map(|x| x.is_dir)).unwrap_or(false);
            let in_base = baseline.contains_key(rel);

            if is_dir {
                match (l.is_some(), r.is_some()) {
                    (true, true) => {
                        if let Err(e) = self.baseline_dir(folder_id, rel, &remote[rel].uid) {
                            warn!(rel, error = %e, "sync: folder step failed; continuing");
                            outcome.errors += 1;
                        }
                    }
                    (true, false) if !in_base => {
                        // New local folder → create remotely (batched).
                        match remote_dirs.get(parent_rel(rel)) {
                            Some(parent) => batch.push(Pending::CreateDir {
                                rel: rel.clone(),
                                parent: parent.clone(),
                            }),
                            None => {
                                warn!(rel, "sync: no remote parent for folder; continuing");
                                outcome.errors += 1;
                            }
                        }
                    }
                    (true, false) => {
                        // In baseline, gone remotely → the remote side deleted it.
                        delete_local_dirs.push(rel.clone());
                    }
                    (false, true) if !in_base => {
                        // New remote folder → create locally (cheap, inline).
                        if let Err(e) = std::fs::create_dir_all(local_root.join(rel_to_path(rel)))
                            .map_err(|e| format!("mkdir {rel}: {e}"))
                            .and_then(|()| self.baseline_dir(folder_id, rel, &remote[rel].uid))
                        {
                            warn!(rel, error = %e, "sync: folder step failed; continuing");
                            outcome.errors += 1;
                        }
                    }
                    (false, true) => {
                        // In baseline, gone locally → the local side deleted it.
                        delete_remote_dirs.push((rel.clone(), remote[rel].uid.clone()));
                    }
                    (false, false) => {
                        let _ = self.db.sync_entry_remove(folder_id, rel);
                    }
                }
                continue;
            }

            // File.
            let base = baseline.get(rel);
            let local_changed = l.is_some_and(|l| {
                base.is_none_or(|b| b.local_mtime != l.mtime || b.local_size != l.size)
            });
            let remote_changed =
                r.is_some_and(|r| base.is_none_or(|b| remote_sig(b) != Some((r.mtime, r.size))));

            match (l, r) {
                (Some(_), Some(r)) => {
                    // With no baseline, both sides read as "changed" and fall to the
                    // conflict arm — the safe default for a folder re-added over
                    // existing remote content.
                    if !local_changed && !remote_changed {
                        // Both untouched; baseline already matches.
                    } else if local_changed && !remote_changed {
                        batch.push(Pending::UploadRevision {
                            rel: rel.clone(),
                            uid: r.uid.clone(),
                        });
                    } else if remote_changed && !local_changed {
                        batch.push(Pending::Download {
                            rel: rel.clone(),
                            uid: r.uid.clone(),
                            mtime: r.mtime,
                            size: r.size,
                        });
                    } else {
                        batch.push(Pending::Conflict {
                            rel: rel.clone(),
                            uid: r.uid.clone(),
                            mtime: r.mtime,
                            size: r.size,
                        });
                    }
                }
                (Some(_), None) => {
                    if base.is_none() || local_changed {
                        // New local file, or remote deleted while local changed →
                        // (re)upload it as a new remote file.
                        match remote_dirs.get(parent_rel(rel)) {
                            Some(parent) => batch.push(Pending::UploadNew {
                                rel: rel.clone(),
                                parent: parent.clone(),
                            }),
                            None => {
                                warn!(rel, "sync: no remote parent for file; continuing");
                                outcome.errors += 1;
                            }
                        }
                    } else {
                        // Remote deleted, local untouched → delete local.
                        let _ = std::fs::remove_file(local_root.join(rel_to_path(rel)));
                        let _ = self.db.sync_entry_remove(folder_id, rel);
                        self.log_activity(
                            ActivityKind::Trash,
                            base_name(rel),
                            "removed locally",
                            true,
                        );
                        outcome.deleted += 1;
                    }
                }
                (None, Some(r)) => {
                    if base.is_none() || remote_changed {
                        // New remote file, or local deleted while remote changed →
                        // (re)download it.
                        batch.push(Pending::Download {
                            rel: rel.clone(),
                            uid: r.uid.clone(),
                            mtime: r.mtime,
                            size: r.size,
                        });
                    } else {
                        // Local deleted, remote untouched → delete remote.
                        if let Err(e) = self
                            .rt
                            .block_on(self.client.trash_nodes(std::slice::from_ref(&r.uid)))
                        {
                            warn!(rel, error = %e, "sync: trash remote failed");
                            self.log_activity(
                                ActivityKind::Trash,
                                base_name(rel),
                                e.to_string(),
                                false,
                            );
                            outcome.errors += 1;
                            continue;
                        }
                        let _ = self.db.sync_entry_remove(folder_id, rel);
                        self.log_activity(
                            ActivityKind::Trash,
                            base_name(rel),
                            "removed on Drive",
                            true,
                        );
                        outcome.deleted += 1;
                    }
                }
                (None, None) => {
                    let _ = self.db.sync_entry_remove(folder_id, rel);
                }
            }
        }
        // Flush the deepest level's batch.
        self.flush_batch(
            folder_id,
            local_root,
            &mut remote_dirs,
            std::mem::take(&mut batch),
            &mut outcome,
        );

        // Deferred folder deletions, deepest first.
        delete_local_dirs.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
        for rel in delete_local_dirs {
            let _ = std::fs::remove_dir_all(local_root.join(rel_to_path(&rel)));
            let _ = self.db.sync_entry_remove(folder_id, &rel);
            self.log_activity(
                ActivityKind::Trash,
                base_name(&rel),
                "removed locally",
                true,
            );
            outcome.deleted += 1;
        }
        delete_remote_dirs.sort_by_key(|(p, _)| std::cmp::Reverse(p.matches('/').count()));
        for (rel, uid) in delete_remote_dirs {
            if let Err(e) = self.rt.block_on(self.client.trash_nodes(&[uid])) {
                warn!(rel, error = %e, "sync: trash remote folder failed");
                self.log_activity(ActivityKind::Trash, base_name(&rel), e.to_string(), false);
                outcome.errors += 1;
                continue;
            }
            let _ = self.db.sync_entry_remove(folder_id, &rel);
            self.log_activity(
                ActivityKind::Trash,
                base_name(&rel),
                "removed on Drive",
                true,
            );
            outcome.deleted += 1;
        }

        Ok(outcome)
    }

    /// Recursively walk a remote folder into `out`, recording every descendant's
    /// relative path, and mapping each subfolder's rel path to its uid in `dirs`.
    ///
    /// A file's size comes from its extended attributes, which only its own node
    /// key can decrypt — and unlocking that key costs an S2K derivation (tens of
    /// milliseconds) *per file*, which is the bulk of a pass over a large folder.
    /// So the walk enumerates cheaply ([`enumerate_nodes_light`]) and only pays
    /// for the files whose modification time has moved away from `baseline`: an
    /// unchanged mtime means the recorded size still stands.
    ///
    /// [`enumerate_nodes_light`]: proton_drive_rs::ProtonDriveClient::enumerate_nodes_light
    fn walk_remote(
        &self,
        folder: &NodeUid,
        prefix: &str,
        out: &mut HashMap<String, RemoteItem>,
        dirs: &mut HashMap<String, NodeUid>,
        baseline: &HashMap<String, StoredSyncEntry>,
    ) -> Result<(), String> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_folder_children_node_uids(folder))
            .map_err(|e| format!("enumerate {folder}: {e}"))?;
        if uids.is_empty() {
            return Ok(());
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes_light(&uids))
            .map_err(|e| format!("resolve nodes: {e}"))?;

        // Files whose recorded mtime no longer matches: their size has to be
        // read for real, so these are the only node keys worth unlocking.
        let stale: Vec<NodeUid> = nodes
            .iter()
            .filter(|n| !n.trashed && !n.is_folder())
            .filter(|n| {
                unchanged_remote_size(baseline, &join_rel(prefix, &n.name), n.modification_time)
                    .is_none()
            })
            .map(|n| n.uid.clone())
            .collect();
        let sized: HashMap<String, i64> = if stale.is_empty() {
            HashMap::new()
        } else {
            self.rt
                .block_on(self.client.enumerate_nodes(&stale))
                .map_err(|e| format!("resolve nodes: {e}"))?
                .iter()
                .map(|n| (n.uid.to_string(), remote_file_sig(n).1))
                .collect()
        };
        let stale: HashSet<String> = stale.iter().map(|u| u.to_string()).collect();

        for node in nodes {
            if node.trashed {
                continue;
            }
            let rel = join_rel(prefix, &node.name);
            if node.is_folder() {
                dirs.insert(rel.clone(), node.uid.clone());
                out.insert(
                    rel.clone(),
                    RemoteItem {
                        uid: node.uid.clone(),
                        is_dir: true,
                        mtime: 0,
                        size: 0,
                    },
                );
                self.walk_remote(&node.uid, &rel, out, dirs, baseline)?;
            } else {
                let size = if stale.contains(&node.uid.to_string()) {
                    match sized.get(&node.uid.to_string()) {
                        Some(size) => *size,
                        // Its size was needed but could not be read (the full
                        // enumeration skipped it as undecryptable). Leave it out
                        // rather than record a stale size, exactly as a full
                        // walk would have.
                        None => continue,
                    }
                } else {
                    match unchanged_remote_size(baseline, &rel, node.modification_time) {
                        Some(size) => size,
                        None => continue,
                    }
                };
                out.insert(
                    rel,
                    RemoteItem {
                        uid: node.uid,
                        is_dir: false,
                        mtime: node.modification_time,
                        size,
                    },
                );
            }
        }
        Ok(())
    }

    /// Record a folder's baseline row (folders carry no content signature).
    fn baseline_dir(&self, folder_id: i64, rel: &str, uid: &NodeUid) -> Result<(), String> {
        self.db
            .sync_entry_upsert(
                folder_id,
                &StoredSyncEntry {
                    rel_path: rel.to_string(),
                    remote_uid: Some(uid.to_string()),
                    local_mtime: 0,
                    local_size: 0,
                    remote_rev: None,
                    remote_hash: None,
                },
            )
            .map_err(|e| format!("baseline dir {rel}: {e:?}"))
    }

    /// Run a batch of queued [`Pending`] ops concurrently (bounded by
    /// [`SYNC_CONCURRENCY`]) and fold their results back into the reconcile's
    /// shared state on the engine thread: created folders are registered in
    /// `remote_dirs` (so deeper paths resolve their parent), and what each op did
    /// is tallied into `outcome`. Driven by `block_on` from the (non-runtime) sync
    /// engine thread, spawning the tasks onto the shared runtime.
    fn flush_batch(
        &self,
        folder_id: i64,
        local_root: &Path,
        remote_dirs: &mut HashMap<String, NodeUid>,
        batch: Vec<Pending>,
        outcome: &mut Outcome,
    ) {
        if batch.is_empty() {
            return;
        }
        self.progress_queued(folder_id, batch.len());
        let core = self.clone();
        let root = local_root.to_path_buf();
        let results = self.rt.block_on(async move {
            let sem = Arc::new(tokio::sync::Semaphore::new(SYNC_CONCURRENCY));
            let mut set = tokio::task::JoinSet::new();
            for op in batch {
                let core = core.clone();
                let sem = sem.clone();
                let root = root.clone();
                set.spawn(async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore not closed");
                    core.apply_pending(folder_id, &root, op).await
                });
            }
            let mut out = Vec::new();
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(result) => out.push(result),
                    Err(e) => warn!(error = %e, "sync: task panicked"),
                }
            }
            out
        });
        for result in results {
            match result {
                Ok(applied) => {
                    outcome.record(&applied);
                    if let Applied::Dir(rel, uid) = applied {
                        remote_dirs.insert(rel, uid);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "sync: step failed; continuing");
                    outcome.errors += 1;
                }
            }
        }
    }

    /// Apply one [`Pending`] op (async, so it can run concurrently in a batch),
    /// reporting it to the folder's live progress and to the activity feed. Each
    /// item logs its own line — the feed is meant to answer "what is it doing",
    /// which a pass-level summary alone cannot.
    async fn apply_pending(
        &self,
        folder_id: i64,
        local_root: &Path,
        op: Pending,
    ) -> Result<Applied, String> {
        self.progress_started(folder_id, base_name(op.rel()));
        let result = self.apply_one(folder_id, local_root, &op).await;
        self.progress_finished(folder_id);
        let name = base_name(op.rel());
        match &result {
            Ok(_) => self.log_activity(op.kind(), name, op.detail(), true),
            Err(e) => self.log_activity(op.kind(), name, e.clone(), false),
        }
        result
    }

    /// The body of one [`Pending`] op, without the progress/activity bookkeeping.
    async fn apply_one(
        &self,
        folder_id: i64,
        local_root: &Path,
        op: &Pending,
    ) -> Result<Applied, String> {
        match op {
            Pending::CreateDir { rel, parent } => {
                let uid = self
                    .client
                    .create_folder(parent, base_name(rel), Some(now_secs()))
                    .await
                    .map_err(|e| format!("create remote folder {rel}: {e}"))?;
                self.baseline_dir(folder_id, rel, &uid)?;
                Ok(Applied::Dir(rel.clone(), uid))
            }
            Pending::UploadNew { rel, parent } => {
                self.upload_new(folder_id, local_root, rel, parent).await?;
                Ok(Applied::Uploaded)
            }
            Pending::UploadRevision { rel, uid } => {
                self.upload_revision(folder_id, local_root, rel, uid)
                    .await?;
                Ok(Applied::Uploaded)
            }
            Pending::Download {
                rel,
                uid,
                mtime,
                size,
            } => {
                self.download_file(folder_id, local_root, rel, uid, *mtime, *size)
                    .await?;
                Ok(Applied::Downloaded)
            }
            Pending::Conflict {
                rel,
                uid,
                mtime,
                size,
            } => {
                // Set the local copy aside (it re-uploads as a new file next
                // pass), then take the remote version as the shared truth.
                let path = local_root.join(rel_to_path(rel));
                if path.exists() {
                    let conflict = conflict_path(&path, now_secs());
                    if let Err(e) = std::fs::rename(&path, &conflict) {
                        warn!(rel, error = %e, "sync: could not set aside conflict copy");
                    } else {
                        info!(rel, "sync: kept local changes as a conflict copy");
                    }
                }
                self.download_file(folder_id, local_root, rel, uid, *mtime, *size)
                    .await?;
                Ok(Applied::Conflict)
            }
        }
    }

    /// Upload a brand-new local file to its remote parent, then record baseline.
    async fn upload_new(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        parent: &NodeUid,
    ) -> Result<(), String> {
        let path = local_root.join(rel_to_path(rel));
        let name = base_name(rel);
        let file = std::fs::File::open(&path).map_err(|e| format!("open {rel}: {e}"))?;
        let meta = file.metadata().map_err(|e| format!("stat {rel}: {e}"))?;
        let mtime = system_mtime(&meta);
        // Count the bytes as they are read, so this upload shows up in
        // `GetQueueStatus` next to a manual one. The uid isn't known until the
        // draft is sealed, so the transfer registers without one.
        let reader = OwnedCountingReader::new(
            file,
            self.transfers
                .begin(name, "", TransferDirection::Upload, meta.len()),
        );
        let uid = self
            .client
            .upload_file_from(
                parent,
                name,
                crate::media_type_for(name),
                reader,
                meta.len() as i64,
                Vec::new(),
                Some(mtime),
                false,
            )
            .await
            .map_err(|e| format!("upload {rel}: {e}"))?;
        self.record_file_baseline(folder_id, rel, &path, &uid).await
    }

    /// Upload a changed local file as a new revision of an existing remote node.
    async fn upload_revision(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        uid: &NodeUid,
    ) -> Result<(), String> {
        let path = local_root.join(rel_to_path(rel));
        let file = std::fs::File::open(&path).map_err(|e| format!("open {rel}: {e}"))?;
        let meta = file.metadata().map_err(|e| format!("stat {rel}: {e}"))?;
        let mtime = system_mtime(&meta);
        let reader = OwnedCountingReader::new(
            file,
            self.transfers.begin(
                base_name(rel),
                uid.to_string(),
                TransferDirection::Upload,
                meta.len(),
            ),
        );
        self.client
            .upload_new_revision_from(uid, reader, meta.len() as i64, Vec::new(), Some(mtime))
            .await
            .map_err(|e| format!("upload revision {rel}: {e}"))?;
        self.record_file_baseline(folder_id, rel, &path, uid).await
    }

    /// Download a remote file to its local path (atomically via a temp file),
    /// stamp the local mtime to match the remote, then record baseline.
    async fn download_file(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        uid: &NodeUid,
        mtime: i64,
        size: i64,
    ) -> Result<(), String> {
        let path = local_root.join(rel_to_path(rel));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir for {rel}: {e}"))?;
        }
        let tmp = path.with_extension(format!(
            "pdfs-tmp-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        {
            let out = std::fs::File::create(&tmp).map_err(|e| format!("create tmp {rel}: {e}"))?;
            // Count the bytes as they land, so the download reports progress like
            // an on-demand hydration does.
            let guard = self.transfers.begin(
                base_name(rel),
                uid.to_string(),
                TransferDirection::Download,
                size.max(0) as u64,
            );
            let mut out = CountingWriter::new(out, &guard);
            self.client
                .download_file_to(uid, &mut out)
                .await
                .map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    format!("download {rel}: {e}")
                })?;
            out.into_inner().flush().ok();
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("place {rel}: {e}"))?;
        // Match local mtime to the remote's so neither side looks "changed" next pass.
        if let Ok(f) = std::fs::File::options().write(true).open(&path) {
            let _ =
                f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64));
        }
        self.record_file_baseline(folder_id, rel, &path, uid).await
    }

    /// Refresh a file's baseline from ground truth: the local stat and the
    /// remote node's reported signature. Called after every upload/download so
    /// the very next reconcile sees the path as unchanged.
    async fn record_file_baseline(
        &self,
        folder_id: i64,
        rel: &str,
        local_path: &Path,
        uid: &NodeUid,
    ) -> Result<(), String> {
        let (lmtime, lsize) = match std::fs::metadata(local_path) {
            Ok(m) => (system_mtime(&m), m.len() as i64),
            Err(e) => return Err(format!("stat {rel}: {e}")),
        };
        // Re-fetch the node so the stored remote signature is exactly what a walk
        // will report next time.
        let (rmtime, rsize) = match self.client.enumerate_nodes(std::slice::from_ref(uid)).await {
            Ok(nodes) => nodes
                .first()
                .map(remote_file_sig)
                .unwrap_or((lmtime, lsize)),
            Err(e) => {
                warn!(rel, error = %e, "sync: baseline refetch failed; using local mtime");
                (lmtime, lsize)
            }
        };
        self.db
            .sync_entry_upsert(
                folder_id,
                &StoredSyncEntry {
                    rel_path: rel.to_string(),
                    remote_uid: Some(uid.to_string()),
                    local_mtime: lmtime,
                    local_size: lsize,
                    remote_rev: Some(rmtime.to_string()),
                    remote_hash: Some(rsize.to_string()),
                },
            )
            .map_err(|e| format!("baseline {rel}: {e:?}"))
    }
}

// ---- helpers --------------------------------------------------------------

/// Recursively walk a local directory into `out`, keyed by `/`-joined relative
/// path. Symlinks and other special files are skipped.
fn walk_local(root: &Path, dir: &Path, out: &mut HashMap<String, LocalItem>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let Ok(stripped) = path.strip_prefix(root) else {
            continue;
        };
        let Some(rel) = stripped.to_str() else {
            continue;
        };
        // Ignore our own in-flight download temp files.
        if rel.contains(".pdfs-tmp-") {
            continue;
        }
        if meta.is_dir() {
            out.insert(
                rel.to_string(),
                LocalItem {
                    is_dir: true,
                    mtime: 0,
                    size: 0,
                },
            );
            walk_local(root, &path, out)?;
        } else if meta.is_file() {
            out.insert(
                rel.to_string(),
                LocalItem {
                    is_dir: false,
                    mtime: system_mtime(&meta),
                    size: meta.len() as i64,
                },
            );
        }
    }
    Ok(())
}

/// A remote node's `(mtime, size)` change signature. Size prefers the plaintext
/// `claimed_size`; otherwise the encrypted storage size (stable either way, and
/// only ever compared against the same node's own baseline).
fn remote_file_sig(node: &Node) -> (i64, i64) {
    let size = match &node.kind {
        NodeKind::File {
            claimed_size,
            total_size_on_storage,
            ..
        } => claimed_size.unwrap_or(*total_size_on_storage),
        NodeKind::Folder => 0,
    };
    (node.modification_time, size)
}

/// The size the baseline recorded for `rel`, if its remote signature's mtime
/// still matches `mtime` — meaning the file has not changed on Drive, so the
/// recorded size stands and there is no need to decrypt it again.
///
/// `None` means the size has to be read for real: either the mtime moved, or
/// there is no baseline signature to trust (a file the sync has not seen).
fn unchanged_remote_size(
    baseline: &HashMap<String, StoredSyncEntry>,
    rel: &str,
    mtime: i64,
) -> Option<i64> {
    match baseline.get(rel).and_then(remote_sig) {
        Some((recorded, size)) if recorded == mtime => Some(size),
        _ => None,
    }
}

/// Join a child `name` onto a walk's `prefix`, giving a rel path.
fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// The stored remote signature of a baseline row, if it has one.
fn remote_sig(e: &StoredSyncEntry) -> Option<(i64, i64)> {
    match (&e.remote_rev, &e.remote_hash) {
        (Some(m), Some(s)) => Some((m.parse().ok()?, s.parse().ok()?)),
        _ => None,
    }
}

/// A file's modification time as epoch seconds (0 if unavailable).
fn system_mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The parent of a `/`-joined relative path (`""` for a top-level entry).
fn parent_rel(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

/// The final component of a `/`-joined relative path.
fn base_name(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    }
}

/// Turn a `/`-joined relative path into an OS path (`/` is already the separator
/// on Linux, this keeps the intent explicit).
fn rel_to_path(rel: &str) -> PathBuf {
    rel.split('/').collect()
}

/// The name for a conflict copy of `path`, e.g. `notes (sync-conflict 1700000000).txt`.
fn conflict_path(path: &Path, stamp: i64) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    let name = match ext {
        Some(ext) => format!("{stem} (sync-conflict {stamp}).{ext}"),
        None => format!("{stem} (sync-conflict {stamp})"),
    };
    match path.parent() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_and_base_split_relative_paths() {
        assert_eq!(parent_rel("a"), "");
        assert_eq!(base_name("a"), "a");
        assert_eq!(parent_rel("a/b"), "a");
        assert_eq!(base_name("a/b"), "b");
        assert_eq!(parent_rel("a/b/c.txt"), "a/b");
        assert_eq!(base_name("a/b/c.txt"), "c.txt");
    }

    #[test]
    fn rel_to_path_builds_nested_os_path() {
        assert_eq!(rel_to_path("a/b/c.txt"), PathBuf::from("a/b/c.txt"));
        assert_eq!(rel_to_path("file"), PathBuf::from("file"));
    }

    #[test]
    fn join_rel_joins_onto_a_prefix_and_leaves_the_root_bare() {
        assert_eq!(join_rel("", "a.txt"), "a.txt");
        assert_eq!(join_rel("d", "a.txt"), "d/a.txt");
        assert_eq!(join_rel("d/e", "a.txt"), "d/e/a.txt");
    }

    /// One baseline holding `rel` with remote signature `(mtime, size)`.
    fn baseline_with(rel: &str, mtime: i64, size: i64) -> HashMap<String, StoredSyncEntry> {
        HashMap::from([(
            rel.to_string(),
            StoredSyncEntry {
                rel_path: rel.to_string(),
                remote_uid: Some("v~l".into()),
                local_mtime: 0,
                local_size: 0,
                remote_rev: Some(mtime.to_string()),
                remote_hash: Some(size.to_string()),
            },
        )])
    }

    #[test]
    fn an_unchanged_mtime_reuses_the_recorded_size() {
        // The whole point: this file's node key never gets unlocked.
        let base = baseline_with("a.txt", 1700, 4096);
        assert_eq!(unchanged_remote_size(&base, "a.txt", 1700), Some(4096));
    }

    #[test]
    fn a_moved_mtime_forces_the_size_to_be_read() {
        let base = baseline_with("a.txt", 1700, 4096);
        assert_eq!(unchanged_remote_size(&base, "a.txt", 1701), None);
    }

    #[test]
    fn a_file_missing_from_the_baseline_forces_the_size_to_be_read() {
        // Nothing recorded to trust — a file the sync has never seen.
        let base = baseline_with("a.txt", 1700, 4096);
        assert_eq!(unchanged_remote_size(&base, "new.txt", 1700), None);
    }

    #[test]
    fn a_baseline_row_without_a_signature_forces_the_size_to_be_read() {
        // A folder row (no remote signature) must never be mistaken for an
        // unchanged file.
        let base = HashMap::from([(
            "d".to_string(),
            StoredSyncEntry {
                rel_path: "d".into(),
                remote_uid: Some("v~l".into()),
                local_mtime: 0,
                local_size: 0,
                remote_rev: None,
                remote_hash: None,
            },
        )]);
        assert_eq!(unchanged_remote_size(&base, "d", 0), None);
    }

    #[test]
    fn remote_sig_round_trips_through_baseline_strings() {
        let e = StoredSyncEntry {
            rel_path: "x".into(),
            remote_uid: Some("v~l".into()),
            local_mtime: 10,
            local_size: 20,
            remote_rev: Some("1700".into()),
            remote_hash: Some("4096".into()),
        };
        assert_eq!(remote_sig(&e), Some((1700, 4096)));
    }

    #[test]
    fn remote_sig_is_none_without_a_stored_signature() {
        let e = StoredSyncEntry {
            rel_path: "d".into(),
            remote_uid: Some("v~l".into()),
            local_mtime: 0,
            local_size: 0,
            remote_rev: None,
            remote_hash: None,
        };
        assert_eq!(remote_sig(&e), None);
    }

    #[test]
    fn outcome_summarises_only_what_it_moved() {
        let mut o = Outcome::default();
        assert!(o.is_empty());
        assert_eq!(o.summary(), "");

        o.record(&Applied::Uploaded);
        o.record(&Applied::Uploaded);
        o.record(&Applied::Downloaded);
        o.errors += 1;
        assert!(!o.is_empty());
        assert_eq!(o.summary(), "2 uploaded, 1 downloaded, 1 failed");
    }

    #[test]
    fn pending_ops_describe_themselves_for_the_feed() {
        let op = Pending::UploadNew {
            rel: "docs/report.pdf".into(),
            parent: uid(),
        };
        assert_eq!(op.rel(), "docs/report.pdf");
        assert_eq!(base_name(op.rel()), "report.pdf");
        assert_eq!(op.kind(), ActivityKind::Upload);

        let op = Pending::Download {
            rel: "photo.jpg".into(),
            uid: uid(),
            mtime: 0,
            size: 10,
        };
        assert_eq!(op.kind(), ActivityKind::Download);
    }

    fn uid() -> NodeUid {
        use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};
        NodeUid::new(VolumeId::from("vol"), LinkId::from("link"))
    }

    #[test]
    fn conflict_path_keeps_extension_and_directory() {
        let p = conflict_path(Path::new("/home/me/docs/notes.txt"), 1700);
        assert_eq!(
            p,
            PathBuf::from("/home/me/docs/notes (sync-conflict 1700).txt")
        );
        let no_ext = conflict_path(Path::new("/home/me/README"), 42);
        assert_eq!(no_ext, PathBuf::from("/home/me/README (sync-conflict 42)"));
    }
}
