//! Two-way folder sync engine.
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
//!
//! ## Open-for-write deferral
//!
//! A file still held open for writing by another process is not uploaded.
//! Uploading mid-write produces a torn snapshot — a revision that was never a
//! real state of the file — wasting the encrypt-and-upload cost and creating a
//! revision the user never asked for. The FUSE mount path already defers until
//! `close(fd)` by design; the mirror path now does the same by scanning
//! `/proc/*/fd` once per reconcile pass and skipping any file whose fd flags
//! include `O_WRONLY` or `O_RDWR`. Deferred files are picked up on the next
//! pass, after the writer has closed them.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use notify::{RecursiveMode, Watcher};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use sha1::{Digest as _, Sha1};
use tracing::{debug, info, warn};

use crate::transfers::{CountingWriter, OwnedCountingReader};
use crate::{Core, now_secs, parse_uid};
use pdfs_core::batch;
use pdfs_core::control::{ActivityKind, TransferDirection};
use pdfs_core::db::{StoredSyncEntry, StoredSyncFolder};
use pdfs_core::syncignore::IgnoreRules;

mod engine;
mod local_scan;
mod model;
mod planner;
pub(crate) use engine::SYNC_CONCURRENCY;
pub(crate) use engine::{SyncMsg, spawn};
#[cfg(test)]
use engine::{classify, event_target, settle_with};
#[cfg(test)]
use local_scan::is_write_mode;
use local_scan::open_for_write_set;
use model::{LocalItem, LocalSig, Outcome, PassAbort, Pending, RemoteItem, RemoteSeen};
pub(crate) use planner::base_name;
#[cfg(test)]
use planner::conflict_path;
#[cfg(test)]
use planner::conflict_path_as;
use planner::{
    FilePlan, RemoteGaps, Unscanned, classification_order, conflict_path_with_suffix,
    filter_baseline, guard_local_wipe, guard_remote_wipe, join_rel, local_deletions, parent_rel,
    plan_file, rel_to_path, remote_sig, remote_subtree_changed, revision_moved, tracked_baseline,
    unchanged_remote_size,
};
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
    /// A path already the same on both sides got its first baseline. Nothing
    /// moved, so it is neither counted nor logged.
    Adopted,
    /// The local file to upload was deleted after the scan planned it. There is
    /// nothing left to send, and the next pass sees the deletion (B108).
    Vanished,
}

impl Core {
    /// Reconcile one synced folder, then apply any mode switch queued while it was
    /// busy. Both halves live here because the switch has to happen with the pass
    /// finished and its lock released, and the engine thread is the only place that
    /// is guaranteed — a switch requested over the control socket cannot wait for a
    /// pass that may run for minutes.
    pub(crate) fn reconcile_folder(&self, folder: &StoredSyncFolder) {
        // A paused folder skips the settle too: a queued switch to on-demand
        // evicts the local tree, which is only safe after a pass uploaded it.
        if self.sync_folder_paused(folder.id) {
            debug!(id = folder.id, "sync: folder paused, skipping reconcile");
            return;
        }
        // `ondemand` folders are live FUSE mounts, not mirrored, so there is nothing
        // to reconcile — but one may still carry a queued switch back to `mirror`,
        // so the settle below is not skipped with it.
        if folder.mode == "mirror" {
            self.reconcile_pass(folder);
        }
        self.settle_pending_mode(folder.id);
    }

    /// One reconcile pass over a mirror folder, updating its `state` column.
    fn reconcile_pass(&self, folder: &StoredSyncFolder) {
        if self.shutdown.is_stopping() {
            return;
        }
        // Hold the folder's lock for the whole pass so a mode switch can't evict the
        // local tree (and mount FUSE over it) while we walk and upload it.
        let lock = self.sync_lock(folder.id);
        let _guard = lock.lock();
        // `folder` was read before the lock; a switch may have landed in between, so
        // re-read and re-check the mode rather than trusting the snapshot.
        let current = match self.db.sync_folder_get(folder.id) {
            Ok(Some(current)) if current.mode == "mirror" => current,
            Ok(Some(_)) => return,
            Ok(None) => return,
            Err(e) => {
                warn!(id = folder.id, error = ?e, "sync: cannot re-read folder; skipping");
                return;
            }
        };
        // A folder waiting to go on-demand is about to have its local copy deleted,
        // so pulling the remote side down is work whose only result is more to evict.
        // All that pass owes the user is getting local changes up.
        let mut push_only = current.pending_mode.as_deref() == Some("ondemand");
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
        // Rebuilt per pass rather than cached on `Core`, so editing `.pdfsignore`
        // takes effect on the next reconcile instead of at the next daemon start.
        // The cost is one small file read against a pass that is about to walk the
        // whole tree.
        let rules = self.ignore_rules(&local_root);
        self.progress_begin(folder.id);
        let result = if push_only {
            self.push_pass(folder.id, &local_root, &remote_root, &rules)
        } else {
            match self.do_reconcile(folder.id, &local_root, &remote_root, &rules) {
                // The user asked for on-demand while this pass was running. Rather
                // than make them wait out a walk-and-download whose results are
                // about to be deleted, drop it here and do the only part that still
                // matters — getting local changes up — so the switch lands now
                // instead of after the pass and another poll.
                Err(PassAbort::Interrupted) => {
                    info!(
                        id = folder.id,
                        "sync: on-demand queued mid-pass; pushing instead"
                    );
                    push_only = true;
                    self.progress_begin(folder.id);
                    self.push_pass(folder.id, &local_root, &remote_root, &rules)
                }
                result => result,
            }
        };
        self.progress_end(folder.id);
        match result {
            Ok(outcome) => {
                // A folder only reaches `idle` when every path applied cleanly —
                // an un-uploaded file must keep it out of `idle` so it can't be
                // switched to on-demand (which evicts the local copy).
                //
                // A push pass is the exception on conflicts: it resolves them by
                // uploading the local copy under a conflict name rather than leaving
                // one on disk, so nothing local is left needing attention. Parking it
                // in `conflict` would block the very switch the pass was run for.
                let state = if outcome.errors > 0 {
                    "error"
                } else if outcome.conflicts > 0 && !push_only {
                    "conflict"
                } else {
                    "idle"
                };
                let _ = self.db.sync_folder_set_state(folder.id, state, now_secs());
                self.report_unscanned(folder.id, name, &outcome.unscanned);
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
            // A push pass is never interrupted, so only the stop gets here: the
            // folder keeps the state it had, which the next start's pass updates.
            Err(PassAbort::Stopped | PassAbort::Interrupted) => {
                info!(id = folder.id, "sync: pass cut short by the stop");
                let _ = self
                    .db
                    .sync_folder_set_state(folder.id, &current.state, current.last_sync);
            }
            Err(PassAbort::Failed(e)) => {
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

    /// Tell the activity feed what a pass over this folder had to leave out
    /// because it cannot be read, once each time that changes. Every other path
    /// in the folder still synced, so this does not make the folder's state an
    /// error; but a folder the user has no permission for is never going to
    /// reach Drive, and they should hear that once rather than not at all
    /// (B196).
    fn report_unscanned(&self, folder_id: i64, name: &str, unscanned: &[String]) {
        let previous = {
            let mut last = self.sync_unscanned.lock();
            if unscanned.is_empty() {
                last.remove(&folder_id)
            } else {
                last.insert(folder_id, unscanned.to_vec())
            }
        };
        if unscanned.is_empty() || previous.as_deref() == Some(unscanned) {
            return;
        }
        const SHOWN: usize = 3;
        let mut detail = unscanned[..unscanned.len().min(SHOWN)].join(", ");
        if unscanned.len() > SHOWN {
            detail.push_str(&format!(" and {} more", unscanned.len() - SHOWN));
        }
        self.log_activity(
            ActivityKind::Sync,
            format!("{} item(s) in {name} cannot be synced", unscanned.len()),
            detail,
            false,
        );
    }

    /// Compile the ignore rules for a synced folder: the global patterns from
    /// the config, plus any `.pdfsignore` at `local_root`.
    ///
    /// A config that cannot be read falls back to the built-in defaults rather
    /// than to "ignore nothing" — the defaults exclude build and VCS trees, and
    /// silently uploading those because a config read failed is the outcome this
    /// feature exists to prevent.
    pub(crate) fn ignore_rules(&self, local_root: &Path) -> IgnoreRules {
        let globals = match pdfs_core::config::AppDirs::new() {
            Ok(dirs) => dirs.load_config().resolved_ignore_patterns(),
            Err(e) => {
                warn!(error = %e, "sync: cannot resolve config dir; using default ignore patterns");
                pdfs_core::syncignore::DEFAULT_IGNORE_PATTERNS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            }
        };
        IgnoreRules::load(local_root, &globals)
    }

    /// Whether a switch to on-demand is waiting on this folder — i.e. whether a
    /// pass in flight is still doing work worth doing. Read from the db rather than
    /// held in memory because the request arrives on the control-socket thread while
    /// the pass runs on the engine thread, and the db row is already the one place
    /// both agree on.
    fn ondemand_queued(&self, folder_id: i64) -> bool {
        matches!(
            self.db.sync_folder_get(folder_id),
            Ok(Some(f)) if f.pending_mode.as_deref() == Some("ondemand")
        )
    }

    /// Get every local change up to Drive, and nothing else — the pass run for a
    /// folder waiting to go on-demand, whose local copy is about to be evicted.
    ///
    /// This exists because a full [`do_reconcile`](Self::do_reconcile) is both too
    /// slow and too wasteful to stand between the user and the switch they asked
    /// for. It walks the whole remote tree (the minutes-long part of a pass over a
    /// large folder) to answer a question the switch makes irrelevant — what the
    /// remote side has that we don't — and then downloads those files onto a disk
    /// we are about to clear. A push pass answers only "is anything here not on
    /// Drive yet?", which the local tree and the baseline already know, so it needs
    /// no remote walk at all and finishes in the time the local walk takes.
    ///
    /// The remote is consulted for exactly one thing: the files whose local copy
    /// changed. Those, and only those, could be a two-sided edit, so their nodes are
    /// fetched (cheaply, in one batch) to check whether the remote moved too. If it
    /// did, the local copy goes up under a conflict name rather than overwriting the
    /// other side. Everything else — remote edits, remote deletions, remote-only
    /// files — is left for the FUSE mount to show live once the switch lands.
    ///
    /// Per-path failures are counted, never fatal, exactly as in `do_reconcile`; a
    /// non-zero [`Outcome::errors`] leaves the folder out of `idle`, which keeps the
    /// queued switch waiting rather than evicting a file that never made it up.
    fn push_pass(
        &self,
        folder_id: i64,
        local_root: &Path,
        remote_root: &NodeUid,
        rules: &IgnoreRules,
    ) -> Result<Outcome, PassAbort> {
        let baseline = self
            .db
            .sync_entries(folder_id)
            .map_err(|e| format!("load baseline: {e:?}"))?;
        // Only the local side is walked here, so each baseline path is checked once.
        self.progress_scan_total(folder_id, baseline.len());

        let mut local: HashMap<String, LocalItem> = HashMap::new();
        let mut unscanned = Unscanned::default();
        let writing = open_for_write_set(local_root);
        self.walk_local(
            folder_id,
            local_root,
            rules,
            &writing,
            &mut local,
            &mut unscanned,
        )?;
        // The guard compares against the paths this pass could still see: an
        // ignored or unreadable path is absent from `local` by rule or by
        // failure, not by loss, and counting those as missing would trip the
        // guard on every pass — wedging the folder's sync permanently — the
        // moment a rule covers the whole tree.
        let tracked = tracked_baseline(&baseline, rules, &unscanned);
        // A local copy that is gone in its entirety is not trashed on Drive,
        // for the reason `guard_local_wipe` gives. A full pass stops there, but
        // this one exists for a switch to on-demand, which needs nothing local
        // and leaves Drive as it is: refusing the pass would only keep the
        // folder out of `idle`, wedging the switch for good (#46). So the pass
        // forgets those paths instead of trashing them, and the mount then
        // shows everything Drive still has.
        let local_gone = match guard_local_wipe(&tracked, &local) {
            Ok(()) => false,
            Err(reason) => {
                warn!(id = folder_id, %reason, "sync: going on-demand without trashing anything on Drive");
                true
            }
        };

        // The remote folder uids come from the baseline instead of a walk: every
        // directory this device has synced recorded its uid there when it was
        // created or first matched up.
        let mut remote_dirs: HashMap<String, NodeUid> = HashMap::new();
        remote_dirs.insert(String::new(), remote_root.clone());
        for (rel, item) in &local {
            if !item.is_dir {
                continue;
            }
            if let Some(uid) = baseline
                .get(rel)
                .and_then(|e| e.remote_uid.as_deref())
                .and_then(parse_uid)
            {
                remote_dirs.insert(rel.clone(), uid);
            }
        }
        // A local directory the baseline has no uid for is either brand new or one
        // whose first pass never finished — and those look identical from here.
        // Creating it blindly would duplicate a remote folder we simply haven't
        // looked at, so when any exist, the remote folder tree is walked for real.
        // It costs one light listing per folder and no file keys, and a folder whose
        // baseline is complete — the case this pass is written for — skips it
        // entirely and touches the network only for the files it uploads.
        if local
            .iter()
            .any(|(rel, item)| item.is_dir && !remote_dirs.contains_key(rel))
        {
            self.walk_remote_dirs(folder_id, remote_root, "", &mut remote_dirs)?;
        }

        // Files whose local copy has moved since the baseline: the only ones with
        // anything to push, and the only ones that could be a two-sided edit.
        let changed: Vec<&String> = local
            .iter()
            .filter(|(_, item)| !item.is_dir && !item.open_for_write)
            .filter(|(rel, item)| {
                baseline
                    .get(*rel)
                    .is_none_or(|b| b.local_mtime != item.mtime || b.local_size != item.size)
            })
            .map(|(rel, _)| rel)
            .collect();

        // One light batch tells us which of those also changed remotely. A light
        // node carries the modification time and the revision id without
        // unlocking the node key, and those are what the baseline compares
        // against.
        let check: Vec<NodeUid> = changed
            .iter()
            .filter_map(|rel| baseline.get(*rel))
            .filter_map(|e| e.remote_uid.as_deref())
            .filter_map(parse_uid)
            .collect();
        let remote_now: HashMap<String, (i64, Option<String>)> = if check.is_empty() {
            HashMap::new()
        } else {
            self.rt
                .block_on(self.drive.enumerate_nodes_light(&check))
                .map_err(|e| format!("resolve changed nodes: {e}"))?
                .into_iter()
                .filter(|n| !n.trashed)
                .map(|n| {
                    let revision = crate::node_revision_id(&n);
                    (n.uid.to_string(), (n.modification_time, revision))
                })
                .collect()
        };
        let changed: HashSet<&String> = changed.into_iter().collect();

        let mut outcome = Outcome {
            deferred: local.values().filter(|item| item.open_for_write).count(),
            unscanned: unscanned.report(),
            ..Default::default()
        };
        let mut order: Vec<&String> = local.keys().collect();
        order.sort_by_key(|p| p.matches('/').count());

        // Same depth-ascending batching as `do_reconcile`: a folder is created
        // remotely before the paths inside it are classified, so their parent uid is
        // in `remote_dirs` by the time they need it.
        let mut batch: Vec<Pending> = Vec::new();
        let mut batch_depth = 0usize;
        for rel in order {
            let depth = rel.matches('/').count();
            if depth > batch_depth {
                self.flush_batch(
                    folder_id,
                    local_root,
                    &mut remote_dirs,
                    std::mem::take(&mut batch),
                    &mut outcome,
                )?;
                batch_depth = depth;
            }
            let item = &local[rel];
            if item.is_dir {
                // The folder is already on Drive, so only its children carry work —
                // but if the walk above is what found it, record the uid now so the
                // next pass over this folder needs no walk at all.
                if let Some(uid) = remote_dirs.get(rel) {
                    if baseline
                        .get(rel)
                        .and_then(|e| e.remote_uid.as_deref())
                        .is_none()
                        && let Err(e) = self.baseline_dir(folder_id, rel, uid)
                    {
                        warn!(rel, error = %e, "sync: folder step failed; continuing");
                        outcome.errors += 1;
                    }
                    continue;
                }
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
                continue;
            }
            if !changed.contains(rel) {
                // Local copy matches the baseline, so it is already on Drive.
                continue;
            }

            let base = baseline.get(rel);
            let uid = base
                .and_then(|e| e.remote_uid.as_deref())
                .and_then(parse_uid);
            match uid {
                // Never uploaded (or the remote node is gone): it goes up as a new file.
                None => match remote_dirs.get(parent_rel(rel)) {
                    Some(parent) => batch.push(Pending::UploadNew {
                        rel: rel.clone(),
                        parent: parent.clone(),
                    }),
                    None => {
                        warn!(rel, "sync: no remote parent for file; continuing");
                        outcome.errors += 1;
                    }
                },
                Some(uid) => {
                    let (remote_moved, seen) = match remote_now.get(&uid.to_string()) {
                        Some((mtime, revision)) => {
                            let recorded = base.and_then(remote_sig);
                            let moved = recorded.is_none_or(|(m, _)| m != *mtime)
                                || revision_moved(base, revision.as_deref());
                            let seen = RemoteSeen {
                                mtime: *mtime,
                                size: recorded.map_or(0, |(_, size)| size),
                                revision: revision.clone(),
                            };
                            (moved, seen)
                        }
                        // Trashed or unreadable remotely — treat as gone and re-upload
                        // the local copy as a new file rather than losing it.
                        None => {
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
                            continue;
                        }
                    };
                    if remote_moved {
                        match remote_dirs.get(parent_rel(rel)) {
                            Some(parent) => batch.push(Pending::PushConflict {
                                rel: rel.clone(),
                                parent: parent.clone(),
                            }),
                            None => {
                                warn!(rel, "sync: no remote parent for file; continuing");
                                outcome.errors += 1;
                            }
                        }
                    } else {
                        batch.push(Pending::UploadRevision {
                            rel: rel.clone(),
                            uid,
                            seen,
                        });
                    }
                }
            }
        }
        self.flush_batch(
            folder_id,
            local_root,
            &mut remote_dirs,
            std::mem::take(&mut batch),
            &mut outcome,
        )?;

        // Paths the baseline knows that are no longer on disk: the user deleted them
        // locally, and the deletion has to reach Drive before the mount starts
        // showing the folder's remote contents — otherwise everything they deleted
        // comes back the moment the switch lands. Shallowest first, skipping anything
        // under a folder already trashed (its children went with it).
        let deletions = local_deletions(&tracked, &local);
        if local_gone {
            // Forgotten instead of trashed. A row left behind would read to
            // the switch as a deletion not on Drive yet and withdraw it, and
            // to a later full pass as a reason to stop again. Forgotten, a full
            // pass downloads the paths again.
            let mut forgotten: HashSet<&str> = HashSet::new();
            for rel in &deletions {
                let mut parent = parent_rel(rel);
                while !parent.is_empty() && !forgotten.contains(parent) {
                    parent = parent_rel(parent);
                }
                if parent.is_empty() {
                    let _ = self.db.sync_entries_remove_subtree(folder_id, rel);
                    forgotten.insert(rel);
                }
            }
            return Ok(outcome);
        }
        let mut trashed: Vec<String> = Vec::new();
        for rel in &deletions {
            if trashed
                .iter()
                .any(|dir| rel.starts_with(&format!("{dir}/")))
            {
                let _ = self.db.sync_entry_remove(folder_id, rel);
                continue;
            }
            let Some(uid) = baseline[rel].remote_uid.as_deref().and_then(parse_uid) else {
                let _ = self.db.sync_entry_remove(folder_id, rel);
                continue;
            };
            if let Err(e) = self
                .rt
                .block_on(self.drive.trash_nodes(&[uid]))
                .and_then(batch::into_unit)
            {
                warn!(rel, error = %e, "sync: trash remote failed");
                self.log_activity(ActivityKind::Trash, base_name(rel), e.to_string(), false);
                outcome.errors += 1;
                continue;
            }
            trashed.push(rel.clone());
            if let Some(parent) = parent_uid_of(rel, &remote_dirs, &baseline) {
                self.invalidate_children_of(&parent);
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

        Ok(outcome)
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
        rules: &IgnoreRules,
    ) -> Result<Outcome, PassAbort> {
        // Loaded before the walks: the remote one uses it to tell which files are
        // unchanged and so can skip decrypting their claimed size, and its size is
        // the only estimate available of how much this pass has to check — the walks
        // discover the real figure only by finishing.
        let baseline = self
            .db
            .sync_entries(folder_id)
            .map_err(|e| format!("load baseline: {e:?}"))?;
        // Both sides are walked, so each baseline path is checked about twice.
        self.progress_scan_total(folder_id, baseline.len() * 2);

        let mut local: HashMap<String, LocalItem> = HashMap::new();
        let mut unscanned = Unscanned::default();
        let writing = open_for_write_set(local_root);
        self.walk_local(
            folder_id,
            local_root,
            rules,
            &writing,
            &mut local,
            &mut unscanned,
        )?;
        // See `push_pass`: the guard must not count excluded paths as lost.
        guard_local_wipe(&tracked_baseline(&baseline, rules, &unscanned), &local)?;

        let mut remote: HashMap<String, RemoteItem> = HashMap::new();
        let mut remote_dirs: HashMap<String, NodeUid> = HashMap::new();
        let mut gaps = RemoteGaps::default();
        remote_dirs.insert(String::new(), remote_root.clone());
        self.walk_remote(
            folder_id,
            remote_root,
            "",
            &mut RemoteWalk {
                out: &mut remote,
                dirs: &mut remote_dirs,
                gaps: &mut gaps,
            },
            &baseline,
        )?;
        guard_remote_wipe(&tracked_baseline(&baseline, rules, &unscanned), &remote)?;

        let mut order = classification_order(&local, &remote, &baseline, rules);
        // A path the scan could not read is no more a local deletion than an
        // ignored one, and a remote path under it has nowhere to be written.
        // The same holds for a path the remote walk could not read: missing
        // from it, it would read as deleted on Drive (B212).
        order.retain(|rel| !unscanned.covers(rel) && !gaps.hides(rel, &remote, &remote_dirs));

        let mut unscanned_report = unscanned.report();
        unscanned_report.extend(gaps.report());
        let mut outcome = Outcome {
            unscanned: unscanned_report,
            ..Default::default()
        };
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
                )?;
                batch_depth = depth;
                // A depth boundary is the pass's natural checkpoint — everything
                // queued has been applied, so stopping here leaves nothing half done.
                if self.ondemand_queued(folder_id) {
                    return Err(PassAbort::Interrupted);
                }
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

            match plan_file(l, r, baseline.get(rel)) {
                // The size was read for real for a row without a revision id
                // (B254), and still matches: give the row the id, so later
                // walks trust the recorded size again.
                FilePlan::Unchanged => {
                    if let (Some(base), Some(revision)) =
                        (baseline.get(rel), r.and_then(|r| r.revision.as_ref()))
                        && base.remote_revision_id.is_none()
                    {
                        let row = StoredSyncEntry {
                            remote_revision_id: Some(revision.clone()),
                            ..base.clone()
                        };
                        if let Err(e) = self.db.sync_entry_upsert(folder_id, &row) {
                            debug!(rel, error = ?e, "sync: cannot record the revision id");
                        }
                    }
                }
                FilePlan::AdoptBaseline => {
                    // First sync of a file the same size on both sides: the apply
                    // step hashes the local copy and records the baseline only if
                    // it matches Drive's digest (B248).
                    let local = l.expect("adopt baseline requires a local file");
                    let remote = r.expect("adopt baseline requires a remote file");
                    let sha1 = remote
                        .sha1
                        .clone()
                        .expect("adopt baseline requires a remote digest");
                    batch.push(Pending::Adopt {
                        rel: rel.clone(),
                        uid: remote.uid.clone(),
                        seen: RemoteSeen::from(remote),
                        sha1,
                        local: LocalSig::from(local),
                    });
                }
                FilePlan::Deferred => {
                    debug!(
                        rel,
                        "sync: file open for write by another process; deferring upload"
                    );
                    outcome.deferred += 1;
                }
                FilePlan::UploadRevision => {
                    let remote = r.expect("upload revision requires a remote file");
                    batch.push(Pending::UploadRevision {
                        rel: rel.clone(),
                        uid: remote.uid.clone(),
                        seen: RemoteSeen::from(remote),
                    });
                }
                FilePlan::Download => {
                    let remote = r.expect("download requires a remote file");
                    batch.push(Pending::Download {
                        rel: rel.clone(),
                        uid: remote.uid.clone(),
                        seen: RemoteSeen::from(remote),
                        // What the walk saw here, so the apply step can tell a
                        // stale plan from a live one before it overwrites.
                        local: l.map(LocalSig::from),
                    });
                }
                FilePlan::Conflict => {
                    let remote = r.expect("conflict requires a remote file");
                    batch.push(Pending::Conflict {
                        rel: rel.clone(),
                        uid: remote.uid.clone(),
                        seen: RemoteSeen::from(remote),
                    });
                }
                FilePlan::UploadNew => match remote_dirs.get(parent_rel(rel)) {
                    Some(parent) => batch.push(Pending::UploadNew {
                        rel: rel.clone(),
                        parent: parent.clone(),
                    }),
                    None => {
                        warn!(rel, "sync: no remote parent for file; continuing");
                        outcome.errors += 1;
                    }
                },
                FilePlan::DeleteLocal => {
                    let path = local_root.join(rel_to_path(rel));
                    // The plan comes from a scan taken before the remote walk,
                    // which can take minutes. An edit saved since then is not
                    // something Drive deleted: keep it, and the next pass
                    // uploads it as a new file (B213).
                    if l.is_some_and(|planned| changed_since_scan(&path, planned)) {
                        info!(rel, "sync: changed since the scan; not deleting it");
                        outcome.deferred += 1;
                        continue;
                    }
                    if let Err(e) = removed_locally(std::fs::remove_file(&path)) {
                        warn!(rel, error = %e, "sync: remove local file failed");
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
                    self.log_activity(ActivityKind::Trash, base_name(rel), "removed locally", true);
                    outcome.deleted += 1;
                }
                FilePlan::DeleteRemote => {
                    let remote = r.expect("remote deletion requires a remote file");
                    if let Err(e) = self
                        .rt
                        .block_on(self.drive.trash_nodes(std::slice::from_ref(&remote.uid)))
                        .and_then(batch::into_unit)
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
                    if let Some(parent) = parent_uid_of(rel, &remote_dirs, &baseline) {
                        self.invalidate_children_of(&parent);
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
                FilePlan::ForgetBaseline => {
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
        )?;

        // Deferred folder deletions, deepest first. A folder that holds what
        // Drive never had stays, and its baseline row goes, so the next pass
        // creates it on Drive again and uploads what it holds (B211).
        if !delete_local_dirs.is_empty() {
            let mut current = self
                .db
                .sync_entries(folder_id)
                .map_err(|e| format!("reload baseline: {e:?}"))?;
            for removal in remove_local_dirs(local_root, delete_local_dirs, &mut current) {
                match removal {
                    DirRemoval::Removed(rel) => {
                        let _ = self.db.sync_entry_remove(folder_id, &rel);
                        self.log_activity(
                            ActivityKind::Trash,
                            base_name(&rel),
                            "removed locally",
                            true,
                        );
                        outcome.deleted += 1;
                    }
                    DirRemoval::Kept(rel, reason) => {
                        warn!(
                            rel,
                            reason, "sync: deleted on Drive, but holds unsynced data; keeping it"
                        );
                        let _ = self.db.sync_entry_remove(folder_id, &rel);
                        self.log_activity(
                            ActivityKind::Trash,
                            base_name(&rel),
                            format!("deleted on Proton Drive, but kept here: {reason}"),
                            false,
                        );
                    }
                    DirRemoval::Failed(rel, e) => {
                        warn!(rel, error = %e, "sync: remove local folder failed");
                        self.log_activity(
                            ActivityKind::Trash,
                            base_name(&rel),
                            e.to_string(),
                            false,
                        );
                        outcome.errors += 1;
                    }
                }
            }
        }
        delete_remote_dirs.sort_by_key(|(p, _)| std::cmp::Reverse(p.matches('/').count()));
        for (rel, uid) in delete_remote_dirs {
            // Deleted here, but another device has put something new in it on
            // Drive. This pass downloaded that, so the folder is back here
            // with it; trashing the folder now would take it away again (B214).
            if let Some(reason) = remote_subtree_changed(&rel, &remote, &baseline) {
                warn!(
                    rel,
                    reason, "sync: deleted here, but changed on Drive; not trashing it"
                );
                self.log_activity(
                    ActivityKind::Trash,
                    base_name(&rel),
                    format!("deleted here, but kept on Proton Drive: {reason}"),
                    false,
                );
                continue;
            }
            if let Err(e) = self
                .rt
                .block_on(self.drive.trash_nodes(&[uid]))
                .and_then(batch::into_unit)
            {
                warn!(rel, error = %e, "sync: trash remote folder failed");
                self.log_activity(ActivityKind::Trash, base_name(&rel), e.to_string(), false);
                outcome.errors += 1;
                continue;
            }
            if let Some(parent) = parent_uid_of(&rel, &remote_dirs, &baseline) {
                self.invalidate_children_of(&parent);
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

    /// Recursively walk a local tree into `out`, keyed by `/`-joined relative
    /// path. Symlinks and other special files are skipped, and paths that cannot
    /// be read go to `unscanned`. Reports each entry to the pass's scan progress.
    fn walk_local(
        &self,
        folder_id: i64,
        root: &Path,
        rules: &IgnoreRules,
        writing: &HashSet<PathBuf>,
        out: &mut HashMap<String, LocalItem>,
        unscanned: &mut Unscanned,
    ) -> Result<(), String> {
        walk_local_tree(root, rules, writing, out, unscanned, &mut |rel| {
            self.progress_scanned(folder_id, base_name(rel))
        })
    }

    /// Map every remote folder under `folder` to its uid by rel path, ignoring
    /// files. The cheap half of [`walk_remote`](Self::walk_remote): folders carry no
    /// content signature, so a light listing answers this without unlocking a single
    /// node key. Used by [`push_pass`](Self::push_pass) to tell a folder it must
    /// create from one it just hasn't recorded yet.
    fn walk_remote_dirs(
        &self,
        folder_id: i64,
        folder: &NodeUid,
        prefix: &str,
        dirs: &mut HashMap<String, NodeUid>,
    ) -> Result<(), String> {
        let uids = self
            .rt
            .block_on(self.drive.enumerate_folder_children_node_uids(folder))
            .map_err(|e| format!("enumerate {folder}: {e}"))?;
        if uids.is_empty() {
            return Ok(());
        }
        let nodes = self
            .rt
            .block_on(self.drive.enumerate_nodes_light(&uids))
            .map_err(|e| format!("resolve nodes: {e}"))?;
        for node in nodes {
            if node.trashed || !node.is_folder() || !crate::remote_name_is_safe(&node.name) {
                continue;
            }
            let rel = join_rel(prefix, &node.name);
            self.progress_scanned(folder_id, base_name(&rel));
            dirs.insert(rel.clone(), node.uid.clone());
            self.walk_remote_dirs(folder_id, &node.uid, &rel, dirs)?;
        }
        Ok(())
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
    ///
    /// What the walk could not read goes to `walk.gaps`, never silently out of
    /// the map: a missing path reads as deleted on Drive.
    fn walk_remote(
        &self,
        folder_id: i64,
        folder: &NodeUid,
        prefix: &str,
        walk: &mut RemoteWalk<'_>,
        baseline: &HashMap<String, StoredSyncEntry>,
    ) -> Result<(), PassAbort> {
        // Checked per folder, not per pass: this walk is the long pole on a large
        // tree, and a user who asks for on-demand three minutes into it should not
        // wait out the rest of a survey of files they want deleted. Bailing out here
        // is safe precisely because the walk has applied nothing — a *partial*
        // remote map, on the other hand, would be read as "the remote deleted
        // everything we haven't reached yet", so it must never reach the diff.
        if self.shutdown.is_stopping() {
            return Err(PassAbort::Stopped);
        }
        if self.ondemand_queued(folder_id) {
            return Err(PassAbort::Interrupted);
        }
        let uids = self
            .rt
            .block_on(self.drive.enumerate_folder_children_node_uids(folder))
            .map_err(|e| format!("enumerate {folder}: {e}"))?;
        if uids.is_empty() {
            return Ok(());
        }
        let nodes = self
            .rt
            .block_on(self.drive.enumerate_nodes_light(&uids))
            .map_err(|e| format!("resolve nodes: {e}"))?;
        // The SDK leaves out a node it cannot decrypt, and says so only in its
        // log. Its name went with it, so all this can record is the folder.
        if nodes.len() < uids.len() {
            warn!(
                folder = prefix,
                listed = uids.len(),
                read = nodes.len(),
                "sync: some nodes on Drive cannot be read; leaving them out"
            );
            walk.gaps.short_listing(prefix);
        }
        let nodes: Vec<Node> = nodes
            .into_iter()
            .filter(|node| {
                let safe = crate::remote_name_is_safe(&node.name);
                if !safe {
                    warn!(folder = prefix, uid = %node.uid, "sync: a name on Drive cannot be a local file name; leaving it out");
                }
                safe
            })
            .collect();

        // Files whose recorded mtime no longer matches: their size has to be
        // read for real, so these are the only node keys worth unlocking.
        let stale: Vec<NodeUid> = nodes
            .iter()
            .filter(|n| !n.trashed && !n.is_folder())
            .filter(|n| {
                unchanged_remote_size(
                    baseline,
                    &join_rel(prefix, &n.name),
                    n.modification_time,
                    crate::node_revision_id(n).as_deref(),
                )
                .is_none()
            })
            .map(|n| n.uid.clone())
            .collect();
        // The full enumeration also decrypts the content digest, which the
        // first sync of a path compares against the local file (B248).
        let sized: HashMap<String, (i64, Option<String>)> = if stale.is_empty() {
            HashMap::new()
        } else {
            self.rt
                .block_on(self.drive.enumerate_nodes(&stale))
                .map_err(|e| format!("resolve nodes: {e}"))?
                .iter()
                .map(|n| {
                    (
                        n.uid.to_string(),
                        (remote_file_sig(n).1, crate::node_content_sha1(n)),
                    )
                })
                .collect()
        };
        let stale: HashSet<String> = stale.iter().map(|u| u.to_string()).collect();

        for node in nodes {
            if node.trashed {
                continue;
            }
            let rel = join_rel(prefix, &node.name);
            self.progress_scanned(folder_id, base_name(&rel));
            if node.is_folder() {
                walk.dirs.insert(rel.clone(), node.uid.clone());
                walk.out.insert(
                    rel.clone(),
                    RemoteItem {
                        uid: node.uid.clone(),
                        is_dir: true,
                        mtime: 0,
                        size: 0,
                        revision: None,
                        sha1: None,
                    },
                );
                self.walk_remote(folder_id, &node.uid, &rel, walk, baseline)?;
            } else {
                let (size, sha1) = if stale.contains(&node.uid.to_string()) {
                    match sized.get(&node.uid.to_string()) {
                        Some(sized) => sized.clone(),
                        // Its size was needed but could not be read (the full
                        // enumeration skipped it as undecryptable). Leave it out
                        // rather than record a stale size, and say so: missing,
                        // it would read as deleted on Drive.
                        None => {
                            walk.gaps.unread(rel);
                            continue;
                        }
                    }
                } else {
                    match unchanged_remote_size(
                        baseline,
                        &rel,
                        node.modification_time,
                        crate::node_revision_id(&node).as_deref(),
                    ) {
                        Some(size) => (size, None),
                        None => continue,
                    }
                };
                walk.out.insert(
                    rel,
                    RemoteItem {
                        revision: crate::node_revision_id(&node),
                        uid: node.uid,
                        is_dir: false,
                        mtime: node.modification_time,
                        size,
                        sha1,
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
                    local_mtime_ns: None,
                    local_size: 0,
                    remote_rev: None,
                    remote_hash: None,
                    remote_revision_id: None,
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
    ///
    /// Fails with [`PassAbort::Stopped`] once the daemon is stopping, after the
    /// ops already sending have finished.
    fn flush_batch(
        &self,
        folder_id: i64,
        local_root: &Path,
        remote_dirs: &mut HashMap<String, NodeUid>,
        batch: Vec<Pending>,
        outcome: &mut Outcome,
    ) -> Result<(), PassAbort> {
        if batch.is_empty() {
            return Ok(());
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
                    // An op still waiting for its turn when the stop comes is
                    // not started; the ones already sending finish.
                    if core.shutdown.is_stopping() {
                        return None;
                    }
                    Some(core.apply_pending(folder_id, &root, op).await)
                });
            }
            let mut out = Vec::new();
            let mut join_errors = 0usize;
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Some(result)) => out.push(result),
                    Ok(None) => {}
                    // The runtime ends with the daemon and cancels what is left.
                    Err(e) if e.is_cancelled() => {}
                    Err(e) => {
                        join_errors += 1;
                        warn!(error = %e, "sync: task panicked");
                    }
                }
            }
            (out, join_errors)
        });
        let (results, join_errors) = results;
        outcome.errors += join_errors;
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
        // The pass goes no further: the next depth's paths may need a folder
        // this batch did not get to make, and went on to fail one by one
        // (`docs/BUGS.md` B169).
        if self.shutdown.is_stopping() {
            return Err(PassAbort::Stopped);
        }
        Ok(())
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
            Ok(Applied::Adopted) => {}
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
                    .drive
                    .create_folder(parent, base_name(rel), Some(now_secs()))
                    .await
                    .map_err(|e| format!("create remote folder {rel}: {e}"))?;
                // The parent has gained a child that no cached listing of it
                // knows about (B86).
                self.invalidate_children_of(parent);
                self.baseline_dir(folder_id, rel, &uid)?;
                Ok(Applied::Dir(rel.clone(), uid))
            }
            Pending::UploadNew { rel, parent } => {
                self.upload_new(folder_id, local_root, rel, parent).await
            }
            Pending::UploadRevision { rel, uid, seen } => {
                self.upload_revision(folder_id, local_root, rel, uid, seen)
                    .await
            }
            Pending::Download {
                rel,
                uid,
                seen,
                local,
            } => {
                self.download_file(folder_id, local_root, rel, uid, seen, *local)
                    .await?;
                Ok(Applied::Downloaded)
            }
            Pending::Adopt {
                rel,
                uid,
                seen: remote,
                sha1,
                local,
            } => {
                let path = local_root.join(rel_to_path(rel));
                let hashed = tokio::task::spawn_blocking(move || local_sha1(&path))
                    .await
                    .map_err(|e| format!("hash {rel}: {e}"))?
                    .map_err(|e| format!("hash {rel}: {e}"))?;
                let Some((seen, digest)) = hashed else {
                    return Ok(Applied::Vanished);
                };
                // Edited since the scan: the next pass plans it again.
                if !seen.same_content(local) {
                    return Ok(Applied::Vanished);
                }
                if !digest.eq_ignore_ascii_case(sha1) {
                    info!(
                        rel,
                        "sync: same size as on Drive but different content; keeping a conflict copy"
                    );
                    return self
                        .take_remote_keeping_local(folder_id, local_root, rel, uid, remote)
                        .await;
                }
                self.db
                    .sync_entry_upsert(
                        folder_id,
                        &StoredSyncEntry {
                            rel_path: rel.clone(),
                            remote_uid: Some(uid.to_string()),
                            local_mtime: seen.mtime,
                            local_mtime_ns: seen.mtime_ns,
                            local_size: seen.size,
                            remote_rev: Some(remote.mtime.to_string()),
                            remote_hash: Some(remote.size.to_string()),
                            remote_revision_id: remote.revision.clone(),
                        },
                    )
                    .map_err(|e| format!("adopt baseline {rel}: {e:?}"))?;
                Ok(Applied::Adopted)
            }
            Pending::Conflict { rel, uid, seen } => {
                self.take_remote_keeping_local(folder_id, local_root, rel, uid, seen)
                    .await
            }
            Pending::PushConflict { rel, parent } => {
                // The ordinary conflict resolution — rename the local copy aside and
                // let the next pass upload it — loses the file here: the folder is
                // going on-demand, so "next pass" comes after the local tree has been
                // evicted. Push the conflict copy now, while it still exists, and
                // leave the remote file holding the original path.
                let path = local_root.join(rel_to_path(rel));
                let conflict = preserve_conflict_copy(&path, now_secs())
                    .map_err(|e| format!("set aside conflict copy for {rel}: {e}"))?;
                let name = conflict
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| format!("conflict copy for {rel} has no name"))?;
                let conflict_rel = join_rel(parent_rel(rel), &name);
                self.upload_new(folder_id, local_root, &conflict_rel, parent)
                    .await?;
                info!(rel, "sync: uploaded local changes as a conflict copy");
                Ok(Applied::Conflict)
            }
        }
    }

    /// Set the local copy aside (it re-uploads as a new file next pass), then
    /// take the remote version as the shared truth.
    async fn take_remote_keeping_local(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        uid: &NodeUid,
        seen: &RemoteSeen,
    ) -> Result<Applied, String> {
        let path = local_root.join(rel_to_path(rel));
        if path.exists() {
            preserve_conflict_copy(&path, now_secs())
                .map_err(|e| format!("set aside conflict copy for {rel}: {e}"))?;
            info!(rel, "sync: kept local changes as a conflict copy");
        }
        // The local copy has just been moved aside, so the state this
        // download expects to find at the path is "nothing" — anything
        // that appears while it runs is a *new* racing write, and gets
        // its own conflict copy rather than being overwritten (B39).
        self.download_file(folder_id, local_root, rel, uid, seen, None)
            .await?;
        Ok(Applied::Conflict)
    }

    /// Upload a brand-new local file to its remote parent, then record baseline.
    async fn upload_new(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        parent: &NodeUid,
    ) -> Result<Applied, String> {
        let path = local_root.join(rel_to_path(rel));
        let name = base_name(rel);
        let Some(file) = open_to_upload(&path, rel)? else {
            return Ok(Applied::Vanished);
        };
        let meta = file.metadata().map_err(|e| format!("stat {rel}: {e}"))?;
        let mtime = system_mtime(&meta);
        // Taken from the descriptor being streamed, so it names the content that
        // actually goes up rather than whatever is at the path when we finish.
        let streamed = LocalSig::from(&meta);
        let thumbnails = self
            .upload_thumbnails_async(path.clone(), name.to_string())
            .await;
        // Count the bytes as they are read, so this upload shows up in
        // `GetQueueStatus` next to a manual one. The uid isn't known until the
        // draft is sealed, so the transfer registers without one.
        let (reader, sent) = Sha1Tap::new(OwnedCountingReader::new(
            file,
            self.transfers
                .begin(name, "", TransferDirection::Upload, meta.len()),
        ));
        let uid = self
            .drive
            // Mirror push: the local file is authoritative. If a prior attempt was
            // interrupted mid-upload it left an unsealed draft of this name; recover
            // it even across a daemon restart (which rotates our client uid), so the
            // folder can reach idle instead of `AlreadyExists`-looping forever.
            .upload_file_replacing_draft_from(
                parent,
                name,
                crate::media_type_for(name),
                reader,
                meta.len() as i64,
                thumbnails,
                Some(mtime),
                false,
            )
            .await
            .map_err(|e| format!("upload {rel}: {e}"))?;
        // The parent has gained a child that no cached listing of it knows
        // about. Without this, a folder later switched to on-demand serves a
        // snapshot that predates this upload (B86).
        self.invalidate_children_of(parent);
        warn_if_torn(rel, &path, streamed);
        let remote = self
            .landed_as_sent(rel, &uid, &sent.digest(), streamed)
            .await?;
        self.record_file_baseline(folder_id, rel, Some(streamed), &uid, &remote)?;
        Ok(Applied::Uploaded)
    }

    /// Upload a changed local file as a new revision of an existing remote node.
    ///
    /// The revision supersedes whatever is current on Drive, and the plan comes
    /// from a walk that may be minutes old. Another computer syncing the same
    /// folder may have committed a revision since, and uploading over it would
    /// leave that edit only in the revision history. So Drive is asked first,
    /// and a file that moved on is left for the next pass, which keeps the
    /// local edit as a conflict copy (B249). A revision committed between this
    /// check and the upload's own draft is not caught here; the read-back after
    /// the upload catches one committed after it.
    async fn upload_revision(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        uid: &NodeUid,
        seen: &RemoteSeen,
    ) -> Result<Applied, String> {
        let path = local_root.join(rel_to_path(rel));
        let Some(file) = open_to_upload(&path, rel)? else {
            return Ok(Applied::Vanished);
        };
        let current = self
            .drive
            .enumerate_nodes(std::slice::from_ref(uid))
            .await
            .map_err(|e| format!("check {rel} on Drive: {e}"))?;
        match current.first() {
            Some(node) if !node.trashed && seen.still_current(node) => {}
            Some(node) if !node.trashed => {
                return Err(format!(
                    "{rel} changed on Drive since this pass looked; it is compared again on the next pass"
                ));
            }
            _ => {
                return Err(format!(
                    "{rel} is no longer on Drive where this pass saw it; it is compared again on the next pass"
                ));
            }
        }
        let meta = file.metadata().map_err(|e| format!("stat {rel}: {e}"))?;
        let mtime = system_mtime(&meta);
        let streamed = LocalSig::from(&meta);
        let thumbnails = self
            .upload_thumbnails_async(path.clone(), base_name(rel).to_string())
            .await;
        let (reader, sent) = Sha1Tap::new(OwnedCountingReader::new(
            file,
            self.transfers.begin(
                base_name(rel),
                uid.to_string(),
                TransferDirection::Upload,
                meta.len(),
            ),
        ));
        self.drive
            .upload_new_revision_from(uid, reader, meta.len() as i64, thumbnails, Some(mtime))
            .await
            .map_err(|e| format!("upload revision {rel}: {e}"))?;
        warn_if_torn(rel, &path, streamed);
        let remote = self
            .landed_as_sent(rel, uid, &sent.digest(), streamed)
            .await?;
        self.record_file_baseline(folder_id, rel, Some(streamed), uid, &remote)?;
        Ok(Applied::Uploaded)
    }

    /// Download a remote file to its local path (atomically via a temp file),
    /// stamp the local mtime to match the remote, then record baseline.
    ///
    /// `expected_local` is the identity planning saw at the destination. The
    /// rename below is the destructive step, and the download in between can run
    /// for minutes, so the destination is re-examined right before it: a local
    /// file that no longer matches the plan is set aside as a conflict copy
    /// instead of being replaced (`docs/BUGS.md` B39).
    ///
    /// The baseline records `seen`, the revision the pass planned to download,
    /// and not a read of the node afterwards. Another computer may commit a
    /// revision while the download runs, and a read-back would record that
    /// revision as synced while the disk holds the older one, so its edit would
    /// never arrive (B250). If the download did fetch the newer revision, the
    /// next pass fetches it once more, which costs a transfer and loses nothing.
    async fn download_file(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        uid: &NodeUid,
        seen: &RemoteSeen,
        expected_local: Option<LocalSig>,
    ) -> Result<(), String> {
        let (mtime, size) = (seen.mtime, seen.size);
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
        // A file an on-demand mount already read, say one just moved here from
        // it, is in the content cache under the same revision key.
        let seeded = size > 0 && self.cache.copy_cached_to(uid, mtime, size as u64, &tmp);
        if seeded {
            std::fs::File::open(&tmp)
                .and_then(|f| f.sync_all())
                .map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    format!("sync tmp {rel}: {e}")
                })?;
            debug!(
                rel,
                "sync: placed from the content cache instead of downloading"
            );
        } else {
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
            self.drive
                .download_file_to(uid, &mut out)
                .await
                .map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    format!("download {rel}: {e}")
                })?;
            let mut out = out.into_inner();
            out.flush().map_err(|e| format!("flush tmp {rel}: {e}"))?;
            out.sync_all().map_err(|e| format!("sync tmp {rel}: {e}"))?;
        }
        // The last moment at which the destination can still be saved. Anything
        // other than the state planning classified means a writer got there
        // first, and its bytes are the ones nothing else has a copy of.
        if let Err(e) = keep_racing_local_edit(&path, expected_local) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("set aside racing local edit for {rel}: {e}"));
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("place {rel}: {e}"))?;
        // Match local mtime to the remote's so neither side looks "changed" next pass.
        let f = std::fs::File::options()
            .write(true)
            .open(&path)
            .map_err(|e| format!("open placed file {rel}: {e}"))?;
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64))
            .map_err(|e| format!("set mtime for {rel}: {e}"))?;
        f.sync_all()
            .map_err(|e| format!("sync placed file {rel}: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| format!("sync directory for {rel}: {e}"))?;
        }
        // Here the file on disk *is* the transferred content: it was published
        // by a rename and stamped just above, so its stat is the identity.
        self.record_file_baseline(folder_id, rel, local_sig(&path), uid, seen)
    }

    /// What Drive holds for `uid` after this daemon uploaded content with SHA-1
    /// `sent` and identity `streamed` to it, as the baseline should record it.
    ///
    /// Read back so the stored signature is exactly what a walk reports next
    /// time. A digest on Drive that is not `sent` means another computer
    /// committed a revision right after this upload. Recording that revision
    /// as synced would hide it, and the edit here would be lost to it on the
    /// next download. So the baseline is left as it was, and the next pass sees
    /// both sides changed and keeps a conflict copy (B250).
    async fn landed_as_sent(
        &self,
        rel: &str,
        uid: &NodeUid,
        sent: &str,
        streamed: LocalSig,
    ) -> Result<RemoteSeen, String> {
        let node = match self.drive.enumerate_nodes(std::slice::from_ref(uid)).await {
            Ok(nodes) => nodes.into_iter().next(),
            Err(e) => {
                warn!(rel, error = %e, "sync: baseline refetch failed; using local mtime");
                None
            }
        };
        let Some(node) = node else {
            // Drive does not list a node it has just made at once. The upload
            // stamped it with the local modification time, so that and the
            // size sent are what a walk reports.
            return Ok(RemoteSeen {
                mtime: streamed.mtime,
                size: streamed.size,
                revision: None,
            });
        };
        if let Some(digest) = crate::node_content_sha1(&node)
            && !digest.eq_ignore_ascii_case(sent)
        {
            return Err(format!(
                "{rel} was changed on Drive by another client right after this upload; it is compared again on the next pass"
            ));
        }
        let (mtime, size) = remote_file_sig(&node);
        Ok(RemoteSeen {
            mtime,
            size,
            revision: crate::node_revision_id(&node),
        })
    }

    /// Record a file's baseline: the local identity these bytes came from and
    /// the remote revision they were synced with. Called after every
    /// upload/download so the very next reconcile sees the path as unchanged.
    ///
    /// `local` is the identity the *transferred content* corresponds to, which
    /// is not always the file's current stat. An upload streams the live file,
    /// so it passes the identity it read the first byte at: if a writer changed
    /// the file mid-stream, the remote now holds a torn revision, and recording
    /// the pre-upload identity is what makes the next pass see the path as
    /// locally changed and upload a whole, clean revision over it. Recording the
    /// post-upload stat instead declared the torn revision synchronized and left
    /// it as the file's content on Drive (`docs/BUGS.md` B40).
    fn record_file_baseline(
        &self,
        folder_id: i64,
        rel: &str,
        local: Option<LocalSig>,
        uid: &NodeUid,
        remote: &RemoteSeen,
    ) -> Result<(), String> {
        let Some(local) = local else {
            return Err(format!("stat {rel}: no local identity to baseline"));
        };
        self.db
            .sync_entry_upsert(
                folder_id,
                &StoredSyncEntry {
                    rel_path: rel.to_string(),
                    remote_uid: Some(uid.to_string()),
                    local_mtime: local.mtime,
                    local_mtime_ns: local.mtime_ns,
                    local_size: local.size,
                    remote_rev: Some(remote.mtime.to_string()),
                    remote_hash: Some(remote.size.to_string()),
                    remote_revision_id: remote.revision.clone(),
                },
            )
            .map_err(|e| format!("baseline {rel}: {e:?}"))
    }
}

/// A [`Read`] that hashes what an upload reads through it, so the digest Drive
/// reports afterwards can be compared with what was sent.
struct Sha1Tap<R> {
    inner: R,
    hasher: Arc<Mutex<Sha1>>,
}

/// The digest of what went through a [`Sha1Tap`], read once the upload is in.
struct SentDigest(Arc<Mutex<Sha1>>);

impl<R: std::io::Read> Sha1Tap<R> {
    fn new(inner: R) -> (Self, SentDigest) {
        let hasher = Arc::new(Mutex::new(Sha1::new()));
        let digest = SentDigest(hasher.clone());
        (Self { inner, hasher }, digest)
    }
}

impl<R: std::io::Read> std::io::Read for Sha1Tap<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.lock().update(&buf[..n]);
        Ok(n)
    }
}

impl SentDigest {
    fn digest(&self) -> String {
        crate::takeout::hex_lower(&self.0.lock().clone().finalize())
    }
}

/// A local file's identity and content SHA-1, both read from one descriptor,
/// or `None` when it is gone. The identity is taken after the hash, so a file
/// written to while it was hashed no longer matches the scan's.
fn local_sha1(path: &Path) -> std::io::Result<Option<(LocalSig, String)>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let digest =
        crate::takeout::sha1_of(&mut file).map_err(|e| std::io::Error::other(e.message))?;
    Ok(Some((LocalSig::from(&file.metadata()?), digest)))
}

/// Open a local file for upload, or `None` when it is gone.
///
/// A scan plans uploads from a listing, and the file can be deleted before its
/// upload starts — a build or a test tree cleaned up mid-pass. That is not a
/// failure: the next pass sees the deletion. Counting it as one put the folder
/// in `error` for a file nobody wanted any more (B108).
fn open_to_upload(path: &Path, rel: &str) -> Result<Option<std::fs::File>, String> {
    match std::fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(rel, "sync: file deleted before its upload; skipping");
            Ok(None)
        }
        Err(e) => Err(format!("open {rel}: {e}")),
    }
}

/// Say so when a file changed under an upload that was streaming it.
///
/// Purely diagnostic — the correction is already in place, because the baseline
/// records the identity the upload *read* rather than the one on disk now, so
/// the next pass classifies the path as locally changed and replaces the torn
/// revision. Without the log there is nothing in the journal to explain the
/// extra revision (`docs/BUGS.md` B40).
fn warn_if_torn(rel: &str, path: &Path, streamed: LocalSig) {
    let now = local_sig(path);
    if !now.is_some_and(|now| now.same_content(&streamed)) {
        warn!(
            rel,
            ?streamed,
            ?now,
            "sync: file changed while uploading; the remote revision may be torn \
             and will be replaced on the next pass"
        );
    }
}

/// The identity a reconcile pass uses to recognise a local file it has already
/// looked at: `None` when nothing is at the path.
fn local_sig(path: &Path) -> Option<LocalSig> {
    std::fs::metadata(path).ok().map(|m| LocalSig {
        mtime: system_mtime(&m),
        mtime_ns: system_mtime_ns(&m),
        size: m.len() as i64,
    })
}

/// Guard the destination of a download: if what is at `path` is no longer what
/// planning classified, move it aside as a conflict copy before the caller
/// renames over it.
///
/// A reconcile pass decides what to do from a walk that finished before the
/// transfer started, and a download of a large file can run for minutes. Without
/// this, a local edit completed inside that window was replaced by the remote
/// version with no copy of it kept anywhere — the one shape of loss a sync
/// engine must not have (`docs/BUGS.md` B39). Keeping the copy makes it re-upload as a
/// new file on the next pass, which is the same resolution a both-sides-changed
/// conflict already gets.
///
/// A path that is empty when it was expected to be is not a conflict: the
/// download is about to recreate it, and there is nothing to preserve.
fn keep_racing_local_edit(path: &Path, expected: Option<LocalSig>) -> std::io::Result<()> {
    let actual = local_sig(path);
    let unchanged = match (actual, expected) {
        (Some(a), Some(e)) => a.same_content(&e),
        // Nothing there: the download is about to create it, and there is
        // nothing to preserve either way.
        (None, _) => true,
        (Some(_), None) => false,
    };
    if unchanged {
        return Ok(());
    }
    warn!(
        path = %path.display(),
        ?expected,
        ?actual,
        "sync: local file changed while downloading; keeping it as a conflict copy"
    );
    preserve_conflict_copy(path, now_secs()).map(|_| ())
}

/// What became of one folder [`remove_local_dirs`] was asked to remove.
enum DirRemoval {
    Removed(String),
    /// Left in place, with why: it holds something Drive does not have.
    Kept(String, String),
    Failed(String, std::io::Error),
}

/// Remove the mirror folders in `dirs`, which Drive deleted, deepest first,
/// except those that hold something Drive never had.
///
/// The uploads of a new file or an edit in such a folder failed earlier in the
/// pass for want of a parent, and an ignored `.git` is never uploaded at all,
/// so removing the folder would lose them for good (B211). `current` is the
/// baseline as the pass has left it: a row the pass already removed with its
/// file must not read as a deletion not carried up yet, and a subfolder removed
/// here takes its rows with it for the same reason. A kept folder loses its own
/// row, so its parent sees it as new and is kept too.
fn remove_local_dirs(
    root: &Path,
    mut dirs: Vec<String>,
    current: &mut HashMap<String, StoredSyncEntry>,
) -> Vec<DirRemoval> {
    dirs.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    dirs.into_iter()
        .map(|rel| {
            if let Some(reason) = mirror_subtree_unsynced(root, &rel, current) {
                current.remove(&rel);
                return DirRemoval::Kept(rel, reason);
            }
            let prefix = format!("{rel}/");
            current.retain(|path, _| *path != rel && !path.starts_with(&prefix));
            match removed_locally(std::fs::remove_dir_all(root.join(rel_to_path(&rel)))) {
                Ok(()) => DirRemoval::Removed(rel),
                Err(e) => DirRemoval::Failed(rel, e),
            }
        })
        .collect()
}

/// What a remote walk fills in as it goes down the tree.
struct RemoteWalk<'a> {
    /// Every path it read, by rel path.
    out: &'a mut HashMap<String, RemoteItem>,
    /// Every folder it read, by rel path, to its uid.
    dirs: &'a mut HashMap<String, NodeUid>,
    /// What it saw but could not read.
    gaps: &'a mut RemoteGaps,
}

/// Whether the file at `path` is no longer what the scan saw as `planned`.
/// A file that is gone has nothing left to lose, so it has not changed.
fn changed_since_scan(path: &Path, planned: &LocalItem) -> bool {
    local_sig(path).is_some_and(|now| !now.same_content(&LocalSig::from(planned)))
}

/// Read a local removal as the sync engine has to read it: the *state* it was
/// asking for is "this path is gone", so a path that was already gone is a
/// success, not a failure.
///
/// It matters because the baseline is only dropped on success. Treating
/// `NotFound` as an error left the baseline in place for a file that no longer
/// exists, so the same removal was re-attempted and re-reported as an error on
/// every pass, forever. Every other error is still an error, and keeping the
/// baseline is exactly right there: the surviving file must not be mistaken for
/// a new local creation on the next pass and uploaded back (`docs/BUGS.md` B42).
fn removed_locally(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The body of [`Core::walk_local`], without a `Core`: recursively collect the
/// local tree under `root` into `out`, reporting each entry to `scanned`.
///
/// **A path the scan cannot read is left out and recorded in `unscanned`, never
/// silently dropped.** Every pass drops what `unscanned` covers from
/// classification, so a baseline row under it is neither a local deletion nor a
/// download target (`docs/BUGS.md` B55). That covers a subfolder the pass has no
/// permission for, an entry that vanished between the listing and its `stat`,
/// and a name that is not UTF-8. Failing the whole pass on any of them, as this
/// did before, left one root-owned folder stopping everything else in the tree
/// from syncing, on every pass, for good (B196).
///
/// The root is the exception, and so is any other I/O error: an absent or
/// unreadable root fails the pass, since leaving the root out would leave
/// nothing in, and an error this cannot name a cause for is not one to guess
/// about.
///
/// Split out from the method so exactly that can be tested against a real tree
/// with a real unreadable directory in it, which needs no `Core` at all.
fn walk_local_tree(
    root: &Path,
    rules: &IgnoreRules,
    writing: &HashSet<PathBuf>,
    out: &mut HashMap<String, LocalItem>,
    unscanned: &mut Unscanned,
    scanned: &mut dyn FnMut(&str),
) -> Result<(), String> {
    walk_local_dir(root, root, rules, writing, out, unscanned, scanned).map_err(|e| e.to_string())
}

/// Why [`walk_local_dir`] could not finish a folder.
enum ScanError {
    /// An I/O error on `what`, which the caller may leave out of the pass.
    Io { what: String, error: std::io::Error },
    /// Anything else, which fails the pass.
    Failed(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Io { what, error } => write!(f, "{what}: {error}"),
            ScanError::Failed(message) => f.write_str(message),
        }
    }
}

/// Leave `rel` out of the pass for `failure`, or hand it back when it is one
/// that fails the pass. Permission denied is reported to the user; an entry
/// that vanished mid-scan is not, since the next pass sees it settled.
fn leave_out(rel: &str, failure: ScanError, unscanned: &mut Unscanned) -> Result<(), ScanError> {
    let ScanError::Io { what, error } = &failure else {
        return Err(failure);
    };
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => {
            warn!(%what, %error, "sync: cannot read; leaving it out of the pass");
            unscanned.unreadable(rel.to_string(), &error.to_string());
            Ok(())
        }
        std::io::ErrorKind::NotFound => {
            debug!(%what, "sync: gone mid-scan; leaving it out of the pass");
            unscanned.vanished(rel.to_string());
            Ok(())
        }
        _ => Err(failure),
    }
}

fn walk_local_dir(
    root: &Path,
    dir: &Path,
    rules: &IgnoreRules,
    writing: &HashSet<PathBuf>,
    out: &mut HashMap<String, LocalItem>,
    unscanned: &mut Unscanned,
    scanned: &mut dyn FnMut(&str),
) -> Result<(), ScanError> {
    let io = |what: String| move |error| ScanError::Io { what, error };
    let entries = std::fs::read_dir(dir).map_err(io(format!("read {}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(io(format!("read entry in {}", dir.display())))?;
        let path = entry.path();
        let stripped = path.strip_prefix(root).map_err(|e| {
            ScanError::Failed(format!(
                "local path {} escaped sync root {}: {e}",
                path.display(),
                root.display()
            ))
        })?;
        let Some(rel) = stripped.to_str() else {
            // Nothing can be keyed by this name: not the baseline, which only
            // ever held paths this scan produced, and not Drive, whose names are
            // UTF-8. Leaving it out cannot read as a deletion of anything.
            warn!(path = %path.display(), "sync: name is not valid UTF-8; leaving it out of the pass");
            unscanned.unreadable(
                stripped.to_string_lossy().into_owned(),
                "name is not valid UTF-8",
            );
            continue;
        };
        // Ignore our own in-flight download temp files.
        if rel.contains(".pdfs-tmp-") {
            continue;
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // Gone since the listing, or in a folder that lists but does not
            // allow `stat`. Either way this pass cannot say what it is.
            Err(error) => {
                let what = format!("stat {}", path.display());
                leave_out(rel, ScanError::Io { what, error }, unscanned)?;
                continue;
            }
        };
        // Ignored paths are dropped here as well as from the classification
        // union — not for correctness (the union filter is what makes this
        // safe) but so an ignored `node_modules/` is never descended into or
        // counted against scan progress.
        if rules.is_ignored(rel, meta.is_dir()) {
            continue;
        }
        if meta.is_dir() {
            out.insert(
                rel.to_string(),
                LocalItem {
                    is_dir: true,
                    mtime: 0,
                    mtime_ns: None,
                    size: 0,
                    open_for_write: false,
                },
            );
            scanned(rel);
            if let Err(failure) =
                walk_local_dir(root, &path, rules, writing, out, unscanned, scanned)
            {
                leave_out(rel, failure, unscanned)?;
                // What the folder did list before failing goes with it: a
                // half-read folder is as unknown as an unread one.
                let prefix = format!("{rel}/");
                out.retain(|k, _| k != rel && !k.starts_with(&prefix));
            }
        } else if meta.is_file() {
            // Check whether any process holds this file open for writing.
            // The canonical path is used because /proc/*/fd links resolve
            // to canonical targets.
            let ofw = std::fs::canonicalize(&path)
                .map(|canon| writing.contains(&canon))
                .unwrap_or(false);
            out.insert(
                rel.to_string(),
                LocalItem {
                    is_dir: false,
                    mtime: system_mtime(&meta),
                    mtime_ns: system_mtime_ns(&meta),
                    size: meta.len() as i64,
                    open_for_write: ofw,
                },
            );
            scanned(rel);
        }
    }
    Ok(())
}

/// Why the mirror copy at `rel` holds something Proton Drive does not, or
/// `None` when everything under it is on Drive exactly as it is on disk.
///
/// A move out of a mirror folder removes the local copy once Drive has moved
/// the node, so anything only this disk has would be lost with it: a file not
/// uploaded yet, one edited since, a deletion not carried up yet, or anything
/// the engine never uploads. That is why ignored names count here, and why the
/// walk is its own rather than [`walk_local_tree`], which skips symlinks.
pub(crate) fn mirror_subtree_unsynced(
    root: &Path,
    rel: &str,
    baseline: &HashMap<String, StoredSyncEntry>,
) -> Option<String> {
    let mut seen = HashSet::new();
    if let Some(reason) = unsynced_local(root, rel, baseline, None, &mut seen) {
        return Some(reason);
    }
    let prefix = format!("{rel}/");
    baseline
        .keys()
        .filter(|r| *r == rel || r.starts_with(&prefix))
        .find(|r| !seen.contains(r.as_str()))
        .map(|r| format!("{r} was deleted here but not on Proton Drive yet"))
}

/// [`mirror_subtree_unsynced`] for a whole mirror folder: why its local copy
/// holds something Proton Drive does not, or `None` when evicting it loses
/// nothing. The switch to on-demand asks this before it empties the folder.
///
/// A push pass that ends `idle` is not enough on its own. It never uploads an
/// ignored path, a symlink or a folder it cannot read, it defers a file open
/// for writing, and a file can change after it finishes; the eviction deleted
/// all of those for good (B198).
///
/// A tracked path deleted here but still in the baseline counts, since the
/// mount would show it again. An ignored one does not: ignoring never carries
/// a deletion to Drive, so its row stays in the baseline for good, and holding
/// the switch on it would hold it forever.
pub(crate) fn mirror_tree_unsynced(
    root: &Path,
    baseline: &HashMap<String, StoredSyncEntry>,
    rules: &IgnoreRules,
) -> Option<String> {
    let mut seen = HashSet::new();
    if let Some(reason) = unsynced_local(root, "", baseline, Some(rules), &mut seen) {
        return Some(reason);
    }
    let mut missing: Vec<String> = filter_baseline(baseline, rules)
        .into_keys()
        .filter(|r| !seen.contains(r.as_str()))
        .collect();
    missing.sort_unstable();
    missing
        .first()
        .map(|r| format!("{r} was deleted here but not on Proton Drive yet"))
}

/// The walk behind both checks. `rules`, when given, only names an ignored
/// path as such in the reason; it is still something only this disk has.
fn unsynced_local(
    root: &Path,
    rel: &str,
    baseline: &HashMap<String, StoredSyncEntry>,
    rules: Option<&IgnoreRules>,
    seen: &mut HashSet<String>,
) -> Option<String> {
    let path = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_to_path(rel))
    };
    // The folder itself has no baseline row; only what is in it does.
    if !rel.is_empty() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => return Some(format!("cannot read {rel}: {e}")),
        };
        let Some(base) = baseline.get(rel) else {
            if rules.is_some_and(|rules| rules.is_ignored(rel, meta.is_dir())) {
                return Some(format!(
                    "{rel} is excluded from sync, so it is only on this computer"
                ));
            }
            return Some(format!("{rel} is not on Proton Drive yet"));
        };
        seen.insert(rel.to_string());
        if meta.is_file() {
            return (!LocalSig::from(&meta).same_content(&LocalSig::from(base)))
                .then(|| format!("{rel} has changes that are not on Proton Drive yet"));
        }
        if !meta.is_dir() {
            return Some(format!("{rel} is not a regular file or folder"));
        }
    }
    let shown = if rel.is_empty() { "the folder" } else { rel };
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(e) => return Some(format!("cannot read {shown}: {e}")),
    };
    for entry in entries {
        let name = match entry {
            Ok(entry) => entry.file_name(),
            Err(e) => return Some(format!("cannot read {shown}: {e}")),
        };
        let Some(name) = name.to_str() else {
            return Some(format!("{shown} holds a name that is not valid UTF-8"));
        };
        // A download the engine left half done; it is the engine's to clean up.
        if name.contains(".pdfs-tmp-") {
            continue;
        }
        if let Some(reason) = unsynced_local(root, &join_rel(rel, name), baseline, rules, seen) {
            return Some(reason);
        }
    }
    None
}

/// Publish a conflict copy without overwriting an earlier conflict bearing the
/// same timestamp. The original is removed only after the new copy is durable.
fn preserve_conflict_copy(path: &Path, stamp: i64) -> std::io::Result<PathBuf> {
    let metadata = std::fs::metadata(path)?;
    for suffix in 0..=u32::MAX {
        let conflict = conflict_path_with_suffix(path, stamp, suffix);
        let mut source = std::fs::File::open(path)?;
        let mut target = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&conflict)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Err(e) = std::io::copy(&mut source, &mut target).and_then(|_| target.sync_all()) {
            let _ = std::fs::remove_file(&conflict);
            return Err(e);
        }
        target.set_permissions(metadata.permissions())?;
        if let Ok(modified) = metadata.modified() {
            target.set_modified(modified)?;
        }
        target.sync_all()?;
        sync_parent_dir(&conflict)?;
        std::fs::remove_file(path)?;
        sync_parent_dir(&conflict)?;
        return Ok(conflict);
    }
    unreachable!("u32 conflict suffix space exhausted")
}

/// The remote uid of `rel`'s parent folder: from the pass's folder-uid map
/// first, and from the baseline second for a parent the map never covered.
///
/// Used to name the folder whose cached listing a remote mutation has just
/// invalidated (B86). `None` is not an error — it only means this pass cannot
/// say which folder to invalidate, and the caller skips it.
fn parent_uid_of(
    rel: &str,
    remote_dirs: &HashMap<String, NodeUid>,
    baseline: &HashMap<String, StoredSyncEntry>,
) -> Option<NodeUid> {
    let parent = parent_rel(rel);
    remote_dirs.get(parent).cloned().or_else(|| {
        baseline
            .get(parent)
            .and_then(|e| e.remote_uid.as_deref())
            .and_then(parse_uid)
    })
}

fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) => std::fs::File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

// ---- helpers --------------------------------------------------------------

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

/// A file's modification time as epoch seconds (0 if unavailable).
fn system_mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The same modification time in nanoseconds, when the filesystem reports one.
///
/// Whole seconds cannot distinguish an edit made in the same second as the last
/// sync from no edit at all, and if the size did not change either, that file
/// was never uploaded (`docs/BUGS.md` B25). `None` for a clock or filesystem that
/// cannot answer, which falls the comparison back to seconds rather than
/// asserting an mtime of zero.
fn system_mtime_ns(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A removal of something already gone has produced the state the sync
    /// engine wanted, so its baseline is dropped rather than retried forever.
    /// Anything else stays an error and keeps the baseline (`docs/BUGS.md` B42).
    #[test]
    fn an_already_absent_path_counts_as_removed() {
        let root = std::env::temp_dir().join(format!("pdfs-b42-{}", std::process::id()));
        let missing = root.join("never-existed");
        assert!(removed_locally(std::fs::remove_file(&missing)).is_ok());
        assert!(removed_locally(std::fs::remove_dir_all(&missing)).is_ok());

        // A directory in the way of `remove_file` is a real failure, and must
        // stay one: the file survives, and its baseline has to survive with it.
        let occupied = root.join("a-directory");
        std::fs::create_dir_all(&occupied).unwrap();
        assert!(removed_locally(std::fs::remove_file(&occupied)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The download guard: a destination that still matches the plan is
    /// replaced silently, one that has moved on since is kept (`docs/BUGS.md` B39).
    #[test]
    fn a_download_keeps_a_local_edit_it_did_not_plan_for() {
        let root = std::env::temp_dir().join(format!("pdfs-b39-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("report.txt");

        // Nothing there, nothing expected: the download simply creates it.
        assert!(keep_racing_local_edit(&path, None).is_ok());
        assert!(!path.exists());

        // Unchanged since planning: no copy, the caller overwrites it.
        std::fs::write(&path, b"planned").unwrap();
        let planned = local_sig(&path).unwrap();
        keep_racing_local_edit(&path, Some(planned)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"planned");

        // Changed since planning: the racing bytes are the only copy of
        // themselves, so they must survive the overwrite as a conflict copy.
        std::fs::write(&path, b"a much longer local edit").unwrap();
        keep_racing_local_edit(&path, Some(planned)).unwrap();
        let kept: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| std::fs::read(e.path()).unwrap_or_default())
            .collect();
        assert!(
            kept.iter()
                .any(|bytes| bytes == b"a much longer local edit"),
            "the racing edit must still exist somewhere: {kept:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Feed `n` `Reconcile` events `gap` apart on another thread, starting
    /// immediately. Returns the sender's join handle so the test can keep it
    /// alive (dropping the sender early would look like a disconnect).
    fn drip(n: usize, gap: Duration) -> (Receiver<SyncMsg>, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            for i in 0..n {
                if tx.send(SyncMsg::Reconcile(i as i64)).is_err() {
                    return;
                }
                std::thread::sleep(gap);
            }
        });
        (rx, h)
    }

    fn collect(rx: &Receiver<SyncMsg>, quiet: Duration, cap: Duration) -> (HashSet<i64>, Duration) {
        let mut ids = HashSet::new();
        let (mut all, mut rewatch) = (false, false);
        // The caller of `settle` has always classified the opening event first.
        let first = rx.recv().unwrap();
        classify(first, &mut ids, &mut all, &mut rewatch);
        let started = Instant::now();
        settle_with(rx, &mut ids, &mut all, &mut rewatch, quiet, cap);
        (ids, started.elapsed())
    }

    #[test]
    fn a_change_in_one_mirror_folder_reconciles_only_that_folder() {
        let watched = vec![
            (PathBuf::from("/home/me/Docs"), 1),
            (PathBuf::from("/home/me/Photos"), 2),
            (PathBuf::from("/home/me/Photos/Raw"), 3),
        ];
        let target = |paths: &[&str]| {
            let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            match event_target(&watched, &paths) {
                SyncMsg::Reconcile(id) => Some(id),
                SyncMsg::ReconcileAll => None,
                _ => panic!("unexpected message"),
            }
        };
        assert_eq!(target(&["/home/me/Docs/a.txt"]), Some(1));
        // A rename within one folder.
        assert_eq!(target(&["/home/me/Docs/a", "/home/me/Docs/b"]), Some(1));
        // The deepest root wins.
        assert_eq!(target(&["/home/me/Photos/Raw/x.cr2"]), Some(3));
        // Across folders, outside them all, or no path at all: walk everything.
        assert_eq!(target(&["/home/me/Docs/a", "/home/me/Photos/a"]), None);
        assert_eq!(target(&["/home/me/Docsx/a"]), None);
        assert_eq!(target(&[]), None);
    }

    /// The bug this replaced a fixed sleep for: a save that keeps writing past
    /// the debounce used to be walked mid-write, uploading a torn snapshot as a
    /// real revision. The window has to restart on every event, so settling
    /// waits for the writer to actually stop.
    #[test]
    fn settling_waits_for_the_last_event_not_the_first() {
        // Eight events 20ms apart — a burst lasting ~160ms, far longer than the
        // 60ms quiet window a fixed sleep would have used.
        let (rx, h) = drip(8, Duration::from_millis(20));
        let (ids, elapsed) = collect(&rx, Duration::from_millis(60), Duration::from_secs(30));
        h.join().unwrap();

        assert_eq!(ids.len(), 8, "every event in the burst is absorbed");
        assert!(
            elapsed >= Duration::from_millis(120),
            "settling returned after {elapsed:?}: it stopped waiting while events \
             were still arriving, which is the torn-revision bug"
        );
    }

    /// A burst that ends is not waited on any longer than the quiet window.
    #[test]
    fn settling_returns_once_the_burst_stops() {
        let (rx, h) = drip(3, Duration::from_millis(5));
        let (ids, elapsed) = collect(&rx, Duration::from_millis(50), Duration::from_secs(30));
        h.join().unwrap();

        assert_eq!(ids.len(), 3);
        assert!(
            elapsed < Duration::from_secs(5),
            "a finished burst must settle on the quiet window, not the cap"
        );
    }

    /// Sustained change — copying a large tree in — must not postpone the
    /// reconcile for as long as the copy runs. The cap ends the wait.
    #[test]
    fn settling_is_capped_under_continuous_change() {
        // Events every 10ms for far longer than the 150ms cap allows.
        let (rx, h) = drip(200, Duration::from_millis(10));
        let (_ids, elapsed) = collect(&rx, Duration::from_millis(100), Duration::from_millis(150));
        assert!(
            elapsed < Duration::from_secs(2),
            "settled after {elapsed:?}; the cap should have ended the wait near 150ms"
        );
        drop(rx);
        h.join().unwrap();
    }

    fn rules_for(patterns: &[&str]) -> IgnoreRules {
        // No folder root is needed: these patterns are all root-relative globs,
        // and `load` only touches the filesystem to look for an ignore file.
        let globals: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
        IgnoreRules::load(Path::new("/nonexistent-sync-root"), &globals)
    }

    fn local_item(is_dir: bool) -> LocalItem {
        LocalItem {
            mtime_ns: None,
            is_dir,
            mtime: 1,
            size: 1,
            open_for_write: false,
        }
    }

    fn remote_item(is_dir: bool) -> RemoteItem {
        RemoteItem {
            uid: uid(),
            is_dir,
            mtime: 1,
            size: 1,
            revision: None,
            sha1: None,
        }
    }

    /// The regression this whole feature has to not cause: a file that was
    /// synced *before* a rule started matching it is absent locally but present
    /// remotely and in the baseline — the exact shape `do_reconcile` answers by
    /// trashing the remote copy. It must never reach classification.
    #[test]
    fn a_newly_ignored_synced_file_is_never_classified_for_deletion() {
        let local: HashMap<String, LocalItem> = HashMap::new();
        let remote: HashMap<String, RemoteItem> =
            [("secrets/key.pem".to_string(), remote_item(false))]
                .into_iter()
                .collect();
        let baseline = baseline_with("secrets/key.pem", 1, 1);

        let order = classification_order(&local, &remote, &baseline, &rules_for(&["secrets/"]));

        assert!(
            order.is_empty(),
            "an ignored path reached classification as {order:?}; with no local \
             entry and a baseline row it would be trashed on Drive"
        );
    }

    /// Filtering only the local walk is not enough, and this is why: the remote
    /// and baseline sides carry the path too.
    #[test]
    fn the_filter_covers_the_remote_and_baseline_sides_not_just_local() {
        let local: HashMap<String, LocalItem> = [("target".to_string(), local_item(true))]
            .into_iter()
            .collect();
        let remote: HashMap<String, RemoteItem> = [("target".to_string(), remote_item(true))]
            .into_iter()
            .collect();
        let baseline = baseline_with("target", 1, 1);

        let order = classification_order(&local, &remote, &baseline, &rules_for(&["target/"]));

        assert!(order.is_empty(), "got {order:?}");
    }

    #[test]
    fn unignored_paths_still_classify_shallowest_first() {
        let local: HashMap<String, LocalItem> = [
            ("a/b/c.txt".to_string(), local_item(false)),
            ("a".to_string(), local_item(true)),
            ("a/b".to_string(), local_item(true)),
            ("node_modules/x.js".to_string(), local_item(false)),
        ]
        .into_iter()
        .collect();
        let remote: HashMap<String, RemoteItem> = HashMap::new();
        let baseline: HashMap<String, StoredSyncEntry> = HashMap::new();

        let order =
            classification_order(&local, &remote, &baseline, &rules_for(&["node_modules/"]));

        assert_eq!(
            order,
            vec!["a", "a/b", "a/b/c.txt"],
            "a parent must be created before the paths inside it"
        );
    }

    #[test]
    fn without_rules_every_path_is_classified() {
        let local: HashMap<String, LocalItem> = [("node_modules".to_string(), local_item(true))]
            .into_iter()
            .collect();
        let remote: HashMap<String, RemoteItem> = HashMap::new();
        let baseline: HashMap<String, StoredSyncEntry> = HashMap::new();

        let order = classification_order(&local, &remote, &baseline, &IgnoreRules::empty());

        assert_eq!(order, vec!["node_modules"]);
    }

    /// A rule covering the whole tree empties the local walk. Without the
    /// baseline being filtered to match, the wipe guard reads that as "every
    /// synced path vanished" and aborts the pass — every pass, forever.
    #[test]
    fn the_wipe_guard_ignores_paths_excluded_by_rule() {
        let mut baseline = baseline_with("build/a.o", 1, 1);
        baseline.extend(baseline_with("build/b.o", 1, 1));
        let local: HashMap<String, LocalItem> = HashMap::new();
        let rules = rules_for(&["build/"]);

        assert!(
            guard_local_wipe(&baseline, &local).is_err(),
            "unfiltered, this is the wedge: the guard fires on a rule, not a loss"
        );
        assert!(
            guard_local_wipe(&filter_baseline(&baseline, &rules), &local).is_ok(),
            "filtered, an all-ignored baseline leaves the guard nothing to check"
        );
    }

    /// The guard must still catch a real disappearance when rules are active.
    #[test]
    fn the_wipe_guard_still_fires_on_a_real_loss_with_rules_active() {
        let mut baseline = baseline_with("docs/a.md", 1, 1);
        baseline.extend(baseline_with("docs/b.md", 1, 1));
        let local: HashMap<String, LocalItem> = HashMap::new();
        let rules = rules_for(&["build/"]);

        assert!(
            guard_local_wipe(&filter_baseline(&baseline, &rules), &local).is_err(),
            "these paths are not ignored; their absence is a genuine wipe"
        );
    }

    /// The guard that keeps a vanished local tree (dead mount, unplugged disk)
    /// from being read as "the user deleted everything" and trashing the folder
    /// on Drive.
    #[test]
    fn guard_local_wipe_blocks_only_a_total_disappearance() {
        let base: HashMap<String, ()> = ["a.txt", "b.txt", "d/c.txt"]
            .iter()
            .map(|r| ((*r).to_string(), ()))
            .collect();
        let none: HashMap<String, ()> = HashMap::new();
        let one: HashMap<String, ()> = [("b.txt".to_string(), ())].into_iter().collect();

        assert!(
            guard_local_wipe(&base, &none).is_err(),
            "wholesale loss aborts"
        );
        assert!(
            guard_local_wipe(&base, &one).is_ok(),
            "one survivor means the tree is readable; the rest are real deletions"
        );
        // A one-entry folder can disappear because its mount or parent became
        // unavailable just as easily as a larger tree can.
        let single: HashMap<String, ()> = [("a.txt".to_string(), ())].into_iter().collect();
        assert!(guard_local_wipe(&single, &none).is_err());
        // Nothing synced yet: a first pass over an empty folder is not a wipe.
        assert!(guard_local_wipe(&none, &none).is_ok());
        // Local paths that are not in the baseline are new uploads, and do not
        // count as survivors.
        let fresh: HashMap<String, ()> = [("new.txt".to_string(), ())].into_iter().collect();
        assert!(guard_local_wipe(&base, &fresh).is_err());
    }

    /// B54's remaining case, end to end: a folder whose local root is still
    /// *readable* but is no longer the tree it was — an empty directory left
    /// where a mount used to be, or a different device mounted over it.
    ///
    /// Nothing fails here. The scan succeeds and honestly reports an empty
    /// directory, so the only thing standing between the user and a trashed
    /// remote folder is the guard, and it has to fire for a one-file baseline
    /// exactly as it does for a large one.
    #[test]
    fn a_replaced_mountpoint_trips_the_guard_at_every_baseline_size() {
        let root = sync_test_dir("wipe-replaced-mount");
        let rules = rules_for(&[]);
        let writing = HashSet::new();
        let mut local: HashMap<String, LocalItem> = HashMap::new();
        let mut progress = no_progress();
        walk_local_tree(
            &root,
            &rules,
            &writing,
            &mut local,
            &mut Unscanned::default(),
            &mut progress,
        )
        .unwrap();
        assert!(local.is_empty(), "the replacement root is genuinely empty");

        for size in 1..=3 {
            let baseline: HashMap<String, ()> =
                (0..size).map(|i| (format!("file{i}.txt"), ())).collect();
            assert!(
                guard_local_wipe(&filter_baseline(&baseline, &rules), &local).is_err(),
                "a {size}-path baseline against an empty root is not a deletion"
            );
        }

        // And the guard steps aside the moment the real tree is back, so a
        // remounted folder resumes rather than staying wedged.
        std::fs::write(root.join("file0.txt"), b"back").unwrap();
        let mut local: HashMap<String, LocalItem> = HashMap::new();
        walk_local_tree(
            &root,
            &rules,
            &writing,
            &mut local,
            &mut Unscanned::default(),
            &mut progress,
        )
        .unwrap();
        let baseline: HashMap<String, ()> = [("file0.txt".to_string(), ())].into_iter().collect();
        assert!(guard_local_wipe(&filter_baseline(&baseline, &rules), &local).is_ok());

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A restored folder starts with an empty baseline against
    /// an empty local directory and a full remote. That must reconcile as
    /// "download everything" rather than tripping the wipe guard — otherwise
    /// every restore would wedge its folder on the first pass.
    #[test]
    fn guard_local_wipe_is_inert_for_a_restored_folder() {
        let empty: HashMap<String, ()> = HashMap::new();
        assert!(guard_local_wipe(&empty, &empty).is_ok());
    }

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
                local_mtime_ns: None,
                remote_rev: Some(mtime.to_string()),
                remote_hash: Some(size.to_string()),
                remote_revision_id: None,
            },
        )])
    }

    #[test]
    fn an_unchanged_mtime_reuses_the_recorded_size() {
        // The whole point: this file's node key never gets unlocked.
        let base = baseline_with("a.txt", 1700, 4096);
        assert_eq!(
            unchanged_remote_size(&base, "a.txt", 1700, None),
            Some(4096)
        );
    }

    #[test]
    fn a_moved_mtime_forces_the_size_to_be_read() {
        let base = baseline_with("a.txt", 1700, 4096);
        assert_eq!(unchanged_remote_size(&base, "a.txt", 1701, None), None);
    }

    #[test]
    fn a_file_missing_from_the_baseline_forces_the_size_to_be_read() {
        // Nothing recorded to trust — a file the sync has never seen.
        let base = baseline_with("a.txt", 1700, 4096);
        assert_eq!(unchanged_remote_size(&base, "new.txt", 1700, None), None);
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
                local_mtime_ns: None,
                remote_rev: None,
                remote_hash: None,
                remote_revision_id: None,
            },
        )]);
        assert_eq!(unchanged_remote_size(&base, "d", 0, None), None);
    }

    #[test]
    fn remote_sig_round_trips_through_baseline_strings() {
        let e = StoredSyncEntry {
            rel_path: "x".into(),
            remote_uid: Some("v~l".into()),
            local_mtime: 10,
            local_size: 20,
            local_mtime_ns: None,
            remote_rev: Some("1700".into()),
            remote_hash: Some("4096".into()),
            remote_revision_id: None,
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
            local_mtime_ns: None,
            remote_rev: None,
            remote_hash: None,
            remote_revision_id: None,
        };
        assert_eq!(remote_sig(&e), None);
    }

    #[test]
    fn a_file_deleted_before_its_upload_is_skipped_not_failed() {
        let root = std::env::temp_dir().join(format!("pdfs-b108-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let gone = root.join("gone.txt");
        assert!(open_to_upload(&gone, "gone.txt").unwrap().is_none());

        std::fs::write(&gone, b"x").unwrap();
        assert!(open_to_upload(&gone, "gone.txt").unwrap().is_some());

        // Other open failures still count as errors.
        let err = open_to_upload(&gone.join("child"), "gone.txt/child");
        assert!(err.unwrap_err().starts_with("open gone.txt/child:"));
        let _ = std::fs::remove_dir_all(&root);

        let mut o = Outcome::default();
        o.record(&Applied::Vanished);
        assert!(o.is_empty());
        assert_eq!(o.errors, 0);
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
            seen: RemoteSeen {
                mtime: 0,
                size: 10,
                revision: None,
            },
            local: None,
        };
        assert_eq!(op.kind(), ActivityKind::Download);
    }

    fn uid() -> NodeUid {
        use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};
        NodeUid::new(VolumeId::from("vol"), LinkId::from("link"))
    }

    #[test]
    fn conflict_path_keeps_extension_and_directory() {
        let p = conflict_path_as(Path::new("/home/me/docs/notes.txt"), "laptop", 1700, 0);
        assert_eq!(
            p,
            PathBuf::from("/home/me/docs/notes (sync-conflict laptop 1700).txt")
        );
        let no_ext = conflict_path_as(Path::new("/home/me/README"), "laptop", 42, 2);
        assert_eq!(
            no_ext,
            PathBuf::from("/home/me/README (sync-conflict laptop 42-2)")
        );
        // The engine names its copies after this machine.
        assert_eq!(
            conflict_path(Path::new("/home/me/README"), 42),
            conflict_path_as(Path::new("/home/me/README"), crate::conflict_label(), 42, 0)
        );
    }

    #[test]
    fn conflict_preservation_never_replaces_an_existing_copy() {
        let dir = sync_test_dir("conflict-collision");
        let source = dir.join("notes.txt");
        let existing = conflict_path(&source, 1700);
        std::fs::write(&source, b"new local edit").unwrap();
        std::fs::write(&existing, b"older conflict").unwrap();

        let preserved = preserve_conflict_copy(&source, 1700).unwrap();

        assert_eq!(std::fs::read(&existing).unwrap(), b"older conflict");
        assert_eq!(std::fs::read(&preserved).unwrap(), b"new local edit");
        assert_eq!(
            preserved,
            conflict_path_as(&source, crate::conflict_label(), 1700, 1)
        );
        assert!(!source.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conflict_preservation_failure_keeps_the_source() {
        let dir = sync_test_dir("conflict-failure");
        // A directory cannot be copied through the regular-file preservation
        // path, deterministically forcing publication to fail after the source
        // has been discovered.
        let source = dir.join("not-a-file.txt");
        std::fs::create_dir(&source).unwrap();

        assert!(preserve_conflict_copy(&source, 1700).is_err());
        assert!(source.is_dir(), "failure must not remove the source");
        assert!(!conflict_path(&source, 1700).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// B49's remaining cases. Preservation is the step that makes a download
    /// safe to publish, so every way it can fail has to leave the local file
    /// where it was — the download is aborted, not the file replaced.
    #[test]
    fn conflict_preservation_failures_all_keep_the_local_file() {
        use std::os::unix::fs::PermissionsExt as _;

        // An unwritable directory: the conflict copy cannot be created at all.
        let dir = sync_test_dir("conflict-eacces");
        let source = dir.join("notes.txt");
        std::fs::write(&source, b"the only copy").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let denied = preserve_conflict_copy(&source, 1700);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(denied.is_err(), "a read-only directory must fail the copy");
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"the only copy",
            "the local file survives byte-identical"
        );
        std::fs::remove_dir_all(&dir).unwrap();

        // A source that vanished between planning and preservation. The pass
        // must report it rather than publish the download over the gap.
        let dir = sync_test_dir("conflict-enoent");
        assert!(preserve_conflict_copy(&dir.join("gone.txt"), 1700).is_err());
        std::fs::remove_dir_all(&dir).unwrap();

        // A write error partway through the copy — induced by making the
        // destination name a directory, so `create_new` fails as something
        // other than `AlreadyExists`.
        let dir = sync_test_dir("conflict-eisdir");
        let source = dir.join("notes.txt");
        std::fs::write(&source, b"still the only copy").unwrap();
        std::fs::create_dir(conflict_path(&source, 1700)).unwrap();
        // Suffix 0 is taken by a directory; `create_new` on it reports
        // `AlreadyExists`, so preservation moves on to suffix 1 and succeeds.
        // What must not happen is the source being removed without a copy.
        let preserved = preserve_conflict_copy(&source, 1700).unwrap();
        assert!(preserved.is_file());
        assert_eq!(std::fs::read(&preserved).unwrap(), b"still the only copy");
        assert!(!source.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A conflict copy keeps the bytes *and* the metadata that identifies them,
    /// so the next pass uploads the user's version rather than a file that looks
    /// freshly written.
    #[test]
    fn a_conflict_copy_is_byte_and_mtime_identical() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = sync_test_dir("conflict-identity");
        let source = dir.join("notes.txt");
        std::fs::write(&source, b"local edit that must survive").unwrap();
        let before = std::fs::metadata(&source).unwrap();

        let preserved = preserve_conflict_copy(&source, 1700).unwrap();
        let after = std::fs::metadata(&preserved).unwrap();

        assert_eq!(
            std::fs::read(&preserved).unwrap(),
            b"local edit that must survive"
        );
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        assert_eq!(after.permissions().mode(), before.permissions().mode());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- B55: an incomplete local scan is never a set of deletions ---------

    fn no_progress() -> impl FnMut(&str) {
        |_: &str| {}
    }

    /// Scan `root` with no rules, returning what it found and left out.
    fn scan(root: &Path) -> Result<(HashMap<String, LocalItem>, Unscanned), String> {
        let mut out = HashMap::new();
        let mut unscanned = Unscanned::default();
        let mut progress = no_progress();
        walk_local_tree(
            root,
            &rules_for(&[]),
            &HashSet::new(),
            &mut out,
            &mut unscanned,
            &mut progress,
        )?;
        Ok((out, unscanned))
    }

    fn sorted_keys<V>(map: &HashMap<String, V>) -> Vec<&str> {
        let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    /// B196, the report in #33: a folder the user may not read, such as a
    /// container's root-owned cache, is left out of the pass and named, and
    /// everything around it still scans. It used to fail the whole pass, so
    /// nothing else in the tree ever synced again.
    #[test]
    fn an_unreadable_subdirectory_is_left_out_and_the_rest_scans() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = sync_test_dir("scan-eacces");
        std::fs::write(root.join("keep.txt"), b"a").unwrap();
        std::fs::create_dir_all(root.join("docker/cache")).unwrap();
        std::fs::write(root.join("docker/compose.yml"), b"c").unwrap();
        let locked = root.join("docker/cache/locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("inner.txt"), b"b").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = scan(&root);

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        let (out, unscanned) = result.expect("one unreadable folder must not fail the pass");
        assert_eq!(
            sorted_keys(&out),
            vec!["docker", "docker/cache", "docker/compose.yml", "keep.txt"]
        );
        assert!(unscanned.covers("docker/cache/locked"));
        assert!(unscanned.covers("docker/cache/locked/inner.txt"));
        assert!(!unscanned.covers("docker/cache"));
        let report = unscanned.report();
        assert_eq!(report.len(), 1);
        assert!(report[0].starts_with("docker/cache/locked ("), "{report:?}");
    }

    /// A folder with read but no search permission lists its names but cannot
    /// `stat` them. Whatever it did list must go with it: a half-read folder
    /// is as unknown as an unread one.
    #[test]
    fn a_listable_but_unsearchable_folder_is_left_out_whole() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = sync_test_dir("scan-no-search");
        std::fs::write(root.join("keep.txt"), b"a").unwrap();
        let half = root.join("half");
        std::fs::create_dir(&half).unwrap();
        std::fs::write(half.join("one.txt"), b"1").unwrap();
        std::fs::write(half.join("two.txt"), b"2").unwrap();
        std::fs::set_permissions(&half, std::fs::Permissions::from_mode(0o400)).unwrap();

        let result = scan(&root);

        std::fs::set_permissions(&half, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        let (out, unscanned) = result.unwrap();
        // Each entry fails its own `stat` and is left out by name.
        assert_eq!(sorted_keys(&out), vec!["half", "keep.txt"]);
        assert!(unscanned.covers("half/one.txt"));
        assert!(unscanned.covers("half/two.txt"));
    }

    /// The other half of B196: what was left out is not a deletion, and not a
    /// download target either. Its baseline rows leave the wipe guard's set
    /// and a push pass's deletions, and its paths leave classification.
    #[test]
    fn a_path_the_scan_left_out_is_never_a_local_deletion() {
        let mut baseline = baseline_with("keep.txt", 1, 1);
        baseline.extend(baseline_with("locked", 0, 0));
        baseline.extend(baseline_with("locked/inner.txt", 1, 1));
        baseline.extend(baseline_with("gone.txt", 1, 1));
        let local: HashMap<String, LocalItem> = [("keep.txt".to_string(), local_item(false))]
            .into_iter()
            .collect();
        let mut unscanned = Unscanned::default();
        unscanned.unreadable("locked".to_string(), "Permission denied");
        let rules = rules_for(&[]);

        let tracked = tracked_baseline(&baseline, &rules, &unscanned);
        assert_eq!(sorted_keys(&tracked), vec!["gone.txt", "keep.txt"]);
        assert_eq!(local_deletions(&tracked, &local), vec!["gone.txt"]);

        // Every tracked path but one is gone, and the guard still lets the
        // pass run: one survivor proves the tree is there.
        assert!(guard_local_wipe(&tracked, &local).is_ok());
        // With everything readable left out, nothing is tracked, so nothing
        // can be lost and the guard has nothing to object to.
        let mut all = Unscanned::default();
        all.unreadable("keep.txt".to_string(), "Permission denied");
        all.unreadable("locked".to_string(), "Permission denied");
        all.vanished("gone.txt".to_string());
        assert!(tracked_baseline(&baseline, &rules, &all).is_empty());

        let remote: HashMap<String, RemoteItem> = HashMap::new();
        let mut order = classification_order(&local, &remote, &baseline, &rules);
        order.retain(|rel| !unscanned.covers(rel));
        order.sort_unstable();
        assert_eq!(order, vec!["gone.txt", "keep.txt"]);
    }

    #[test]
    fn unscanned_covers_a_path_and_what_is_under_it_but_not_its_siblings() {
        let mut unscanned = Unscanned::default();
        unscanned.unreadable("a/locked".to_string(), "Permission denied");
        unscanned.vanished("b.txt".to_string());

        assert!(unscanned.covers("a/locked"));
        assert!(unscanned.covers("a/locked/x/y.txt"));
        assert!(unscanned.covers("b.txt"));
        assert!(!unscanned.covers("a"));
        assert!(!unscanned.covers("a/locked2"));
        assert!(!unscanned.covers("b.txt.bak"));
        // A file that only vanished mid-scan is not worth telling anyone about.
        assert_eq!(unscanned.report(), vec!["a/locked (Permission denied)"]);
    }

    /// B197: a push pass read its deletions from the raw baseline, so a path
    /// synced before it became ignored, which the walk skips, was trashed on
    /// Drive, and the switch the push pass ran for then evicted the local copy.
    #[test]
    fn a_push_pass_never_reads_an_ignored_path_as_deleted() {
        let mut baseline = baseline_with("notes.md", 1, 1);
        baseline.extend(baseline_with("node_modules", 0, 0));
        baseline.extend(baseline_with("node_modules/x/index.js", 1, 1));
        baseline.extend(baseline_with("old.md", 1, 1));
        let local: HashMap<String, LocalItem> = [("notes.md".to_string(), local_item(false))]
            .into_iter()
            .collect();
        let rules = rules_for(&["node_modules/"]);

        let tracked = tracked_baseline(&baseline, &rules, &Unscanned::default());

        assert_eq!(local_deletions(&tracked, &local), vec!["old.md"]);
    }

    /// Deletions come out shallowest first, so a trashed folder can take its
    /// children with it.
    #[test]
    fn local_deletions_come_out_shallowest_first() {
        let mut baseline = baseline_with("d/e/f.txt", 1, 1);
        baseline.extend(baseline_with("d", 0, 0));
        baseline.extend(baseline_with("d/e", 0, 0));
        let local: HashMap<String, LocalItem> = HashMap::new();
        let tracked = tracked_baseline(&baseline, &rules_for(&[]), &Unscanned::default());

        assert_eq!(
            local_deletions(&tracked, &local),
            vec!["d", "d/e", "d/e/f.txt"]
        );
    }

    /// A baseline row recording `path` as it is on disk right now.
    fn synced_row(root: &Path, rel: &str) -> (String, StoredSyncEntry) {
        let meta = std::fs::symlink_metadata(root.join(rel_to_path(rel))).unwrap();
        let sig = LocalSig::from(&meta);
        let entry = StoredSyncEntry {
            rel_path: rel.to_string(),
            remote_uid: Some(format!("vol~{rel}")),
            local_mtime: sig.mtime,
            local_mtime_ns: sig.mtime_ns,
            local_size: sig.size,
            remote_rev: None,
            remote_hash: None,
            remote_revision_id: None,
        };
        (rel.to_string(), entry)
    }

    fn synced_subtree(name: &str) -> (PathBuf, HashMap<String, StoredSyncEntry>) {
        let root = sync_test_dir(name);
        std::fs::create_dir_all(root.join("dir/sub")).unwrap();
        std::fs::write(root.join("dir/a.txt"), b"a").unwrap();
        std::fs::write(root.join("dir/sub/b.txt"), b"b").unwrap();
        let baseline = ["dir", "dir/sub", "dir/a.txt", "dir/sub/b.txt"]
            .into_iter()
            .map(|rel| synced_row(&root, rel))
            .collect();
        (root, baseline)
    }

    /// Drive deleted `dir`. Everything it held that was synced goes with it;
    /// a file Drive never had keeps the folder, and the parent with it
    /// (`docs/BUGS.md` B211).
    #[test]
    fn a_folder_deleted_on_drive_keeps_what_drive_never_had() {
        let (root, mut baseline) = synced_subtree("b211-keep");
        // The pass already removed the synced files with their rows.
        std::fs::remove_file(root.join("dir/a.txt")).unwrap();
        baseline.remove("dir/a.txt");
        std::fs::remove_file(root.join("dir/sub/b.txt")).unwrap();
        baseline.remove("dir/sub/b.txt");
        std::fs::write(root.join("dir/new.txt"), b"never uploaded").unwrap();

        let removals = remove_local_dirs(
            &root,
            vec!["dir".to_string(), "dir/sub".to_string()],
            &mut baseline,
        );
        let shape: Vec<(&str, &str)> = removals
            .iter()
            .map(|r| match r {
                DirRemoval::Removed(rel) => ("removed", rel.as_str()),
                DirRemoval::Kept(rel, _) => ("kept", rel.as_str()),
                DirRemoval::Failed(rel, _) => ("failed", rel.as_str()),
            })
            .collect();
        assert_eq!(shape, [("removed", "dir/sub"), ("kept", "dir")]);
        assert_eq!(
            std::fs::read(root.join("dir/new.txt")).unwrap(),
            b"never uploaded"
        );
        assert!(!root.join("dir/sub").exists());
        // Its row is gone, so the next pass sees a new local folder.
        assert!(!baseline.contains_key("dir"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_deleted_on_drive_with_only_synced_content_is_removed() {
        let (root, mut baseline) = synced_subtree("b211-remove");
        for file in ["dir/a.txt", "dir/sub/b.txt"] {
            std::fs::remove_file(root.join(file)).unwrap();
            baseline.remove(file);
        }
        let removals = remove_local_dirs(
            &root,
            vec!["dir/sub".to_string(), "dir".to_string()],
            &mut baseline,
        );
        assert!(removals.iter().all(|r| matches!(r, DirRemoval::Removed(_))));
        assert!(!root.join("dir").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An edit to a file saved after the scan is not a deletion on Drive
    /// (`docs/BUGS.md` B213).
    #[test]
    fn a_file_changed_since_the_scan_is_not_deleted() {
        let root = sync_test_dir("b213");
        let path = root.join("notes.txt");
        std::fs::write(&path, b"as scanned").unwrap();
        let sig = local_sig(&path).unwrap();
        let planned = LocalItem {
            is_dir: false,
            mtime: sig.mtime,
            mtime_ns: sig.mtime_ns,
            size: sig.size,
            open_for_write: false,
        };
        assert!(!changed_since_scan(&path, &planned));
        std::fs::write(&path, b"edited after the scan").unwrap();
        assert!(changed_since_scan(&path, &planned));
        std::fs::remove_file(&path).unwrap();
        assert!(!changed_since_scan(&path, &planned));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fully_synced_mirror_subtree_may_leave() {
        let (root, baseline) = synced_subtree("move-synced");
        let reason = mirror_subtree_unsynced(&root, "dir", &baseline);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(reason, None);
    }

    #[test]
    fn a_mirror_subtree_with_a_file_not_uploaded_may_not_leave() {
        let (root, baseline) = synced_subtree("move-new-file");
        std::fs::write(root.join("dir/sub/new.txt"), b"n").unwrap();
        let reason = mirror_subtree_unsynced(&root, "dir", &baseline);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reason.unwrap().contains("dir/sub/new.txt"));
    }

    #[test]
    fn a_mirror_subtree_with_an_edit_not_uploaded_may_not_leave() {
        let (root, baseline) = synced_subtree("move-edit");
        std::fs::write(root.join("dir/a.txt"), b"longer now").unwrap();
        let reason = mirror_subtree_unsynced(&root, "dir", &baseline);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reason.unwrap().contains("dir/a.txt"));
    }

    #[test]
    fn a_mirror_subtree_with_a_deletion_not_carried_up_may_not_leave() {
        let (root, baseline) = synced_subtree("move-deleted");
        std::fs::remove_file(root.join("dir/sub/b.txt")).unwrap();
        let reason = mirror_subtree_unsynced(&root, "dir", &baseline);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reason.unwrap().contains("dir/sub/b.txt"));
    }

    #[test]
    fn a_mirror_subtree_holding_a_symlink_may_not_leave() {
        let (root, baseline) = synced_subtree("move-symlink");
        std::os::unix::fs::symlink("a.txt", root.join("dir/link")).unwrap();
        let reason = mirror_subtree_unsynced(&root, "dir", &baseline);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reason.unwrap().contains("dir/link"));
    }

    #[test]
    fn a_name_prefixed_sibling_does_not_hold_a_mirror_subtree_back() {
        let (root, mut baseline) = synced_subtree("move-sibling");
        // Deleted here but not on Drive, yet outside the subtree being moved.
        baseline.insert(
            "dirt".to_string(),
            StoredSyncEntry {
                rel_path: "dirt".to_string(),
                ..baseline["dir/a.txt"].clone()
            },
        );
        let reason = mirror_subtree_unsynced(&root, "dir", &baseline);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(reason, None);
    }

    // ---- B198: going online-only never evicts what Drive does not have -----

    #[test]
    fn a_fully_synced_mirror_folder_may_go_online_only() {
        let (root, baseline) = synced_subtree("evict-synced");
        let reason = mirror_tree_unsynced(&root, &baseline, &rules_for(&[]));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(reason, None);
    }

    /// The case that lost data: `.git/` is ignored by default, so no pass ever
    /// uploads it, and the eviction deleted it with the rest.
    #[test]
    fn an_ignored_folder_only_this_disk_has_holds_the_switch_back() {
        let (root, baseline) = synced_subtree("evict-ignored");
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref").unwrap();
        let reason = mirror_tree_unsynced(&root, &baseline, &rules_for(&[".git/"]));
        std::fs::remove_dir_all(&root).unwrap();
        let reason = reason.expect("an ignored folder is only on this disk");
        assert!(
            reason.contains(".git") && reason.contains("excluded"),
            "{reason}"
        );
    }

    /// An ignored row stays in the baseline for good once its file is gone, so
    /// counting it as a deletion not carried up would hold the switch forever.
    #[test]
    fn a_deleted_ignored_path_does_not_hold_the_switch_back() {
        let (root, mut baseline) = synced_subtree("evict-ignored-gone");
        baseline.insert(
            "build/out.o".to_string(),
            StoredSyncEntry {
                rel_path: "build/out.o".to_string(),
                ..baseline["dir/a.txt"].clone()
            },
        );
        let rules = rules_for(&["build/"]);
        let ignored = mirror_tree_unsynced(&root, &baseline, &rules);
        let tracked = mirror_tree_unsynced(&root, &baseline, &rules_for(&[]));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(ignored, None);
        assert!(tracked.unwrap().contains("build/out.o"));
    }

    #[test]
    fn an_edit_after_the_last_pass_holds_the_switch_back() {
        let (root, baseline) = synced_subtree("evict-edit");
        std::fs::write(root.join("dir/sub/b.txt"), b"edited since").unwrap();
        let reason = mirror_tree_unsynced(&root, &baseline, &rules_for(&[]));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reason.unwrap().contains("dir/sub/b.txt"));
    }

    #[test]
    fn a_new_file_at_the_root_holds_the_switch_back() {
        let (root, baseline) = synced_subtree("evict-new");
        std::fs::write(root.join("new.txt"), b"n").unwrap();
        let reason = mirror_tree_unsynced(&root, &baseline, &rules_for(&[]));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reason.unwrap().contains("new.txt"));
    }

    /// The folder B196 now leaves out of the pass instead of failing it is,
    /// by the same token, something the eviction must not delete.
    #[test]
    fn an_unreadable_folder_holds_the_switch_back() {
        use std::os::unix::fs::PermissionsExt as _;

        let (root, baseline) = synced_subtree("evict-eacces");
        let locked = root.join("dir/sub");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let reason = mirror_tree_unsynced(&root, &baseline, &rules_for(&[]));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reason.unwrap().contains("dir/sub"));
    }

    #[test]
    fn a_symlink_holds_the_switch_back() {
        let (root, baseline) = synced_subtree("evict-symlink");
        std::os::unix::fs::symlink("dir/a.txt", root.join("link")).unwrap();
        let reason = mirror_tree_unsynced(&root, &baseline, &rules_for(&[]));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reason.unwrap().contains("link"));
    }

    /// The same rule one level up: if the sync root itself cannot be read, the
    /// pass sees nothing, and "nothing" must never mean "delete everything".
    #[test]
    fn an_unreadable_root_fails_the_scan() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = sync_test_dir("scan-root-eacces");
        std::fs::write(root.join("keep.txt"), b"a").unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();

        let rules = rules_for(&[]);
        let writing = HashSet::new();
        let mut out = HashMap::new();
        let mut progress = no_progress();
        let result = walk_local_tree(
            &root,
            &rules,
            &writing,
            &mut out,
            &mut Unscanned::default(),
            &mut progress,
        );

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        assert!(result.is_err());
        assert!(out.is_empty());
    }

    /// A root that is gone entirely — an unmounted device folder, a removed
    /// directory — is the sharpest version of the same case: every baseline path
    /// is "missing", and the scan must refuse rather than report an empty tree.
    #[test]
    fn an_absent_root_fails_the_scan() {
        let root = sync_test_dir("scan-absent");
        std::fs::remove_dir_all(&root).unwrap();

        let rules = rules_for(&[]);
        let writing = HashSet::new();
        let mut out = HashMap::new();
        let mut progress = no_progress();
        assert!(
            walk_local_tree(
                &root,
                &rules,
                &writing,
                &mut out,
                &mut Unscanned::default(),
                &mut progress
            )
            .is_err()
        );
        assert!(out.is_empty());
    }

    /// A name that is not valid UTF-8 cannot be keyed into the scan, but it
    /// cannot be in the baseline or on Drive either, so leaving it out reads as
    /// the deletion of nothing. It used to fail the pass (B196).
    #[test]
    fn a_non_utf8_name_is_left_out_and_reported() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let root = sync_test_dir("scan-non-utf8");
        std::fs::write(root.join(OsStr::from_bytes(b"bad\xffname")), b"x").unwrap();
        std::fs::write(root.join("good.txt"), b"y").unwrap();

        let result = scan(&root);
        std::fs::remove_dir_all(&root).unwrap();

        let (out, unscanned) = result.unwrap();
        assert_eq!(sorted_keys(&out), vec!["good.txt"]);
        let report = unscanned.report();
        assert_eq!(report.len(), 1);
        assert!(report[0].contains("UTF-8"), "{report:?}");
    }

    /// The complement: a scan that *can* read everything reports everything, so
    /// the strictness above does not come at the cost of a working pass. A
    /// symlink is skipped by design rather than by failure, and a dangling one
    /// stats fine through `symlink_metadata`.
    #[test]
    fn a_readable_tree_scans_completely() {
        let root = sync_test_dir("scan-complete");
        std::fs::write(root.join("a.txt"), b"aa").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.txt"), b"bbb").unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("dangling")).unwrap();

        let rules = rules_for(&[]);
        let writing = HashSet::new();
        let mut out = HashMap::new();
        let mut seen: Vec<String> = Vec::new();
        let result = walk_local_tree(
            &root,
            &rules,
            &writing,
            &mut out,
            &mut Unscanned::default(),
            &mut |rel| seen.push(rel.to_string()),
        );
        std::fs::remove_dir_all(&root).unwrap();

        result.unwrap();
        let mut keys: Vec<&str> = out.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["a.txt", "sub", "sub/b.txt"]);
        assert_eq!(out["a.txt"].size, 2);
        assert_eq!(out["sub/b.txt"].size, 3);
        assert!(out["sub"].is_dir);
        assert_eq!(seen.len(), 3, "every reported entry ticks scan progress");
    }

    fn sync_test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("pdfs-sync-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    // ---- open-for-write detection tests -----------------------------------

    #[test]
    fn is_write_mode_detects_wronly() {
        // O_WRONLY = 0o1 → flags octal "0100001" (O_WRONLY | O_LARGEFILE on x86_64)
        let fdinfo = "pos:\t0\nflags:\t0100001\nmnt_id:\t29\n";
        assert!(is_write_mode(fdinfo), "O_WRONLY must be detected as write");
    }

    #[test]
    fn is_write_mode_detects_rdwr() {
        // O_RDWR = 0o2 → flags octal "0100002"
        let fdinfo = "pos:\t0\nflags:\t0100002\nmnt_id:\t29\n";
        assert!(is_write_mode(fdinfo), "O_RDWR must be detected as write");
    }

    #[test]
    fn is_write_mode_ignores_rdonly() {
        // O_RDONLY = 0o0 → flags octal "0100000" (just O_LARGEFILE)
        let fdinfo = "pos:\t0\nflags:\t0100000\nmnt_id:\t29\n";
        assert!(
            !is_write_mode(fdinfo),
            "O_RDONLY must not be detected as write"
        );
    }

    #[test]
    fn is_write_mode_handles_bare_zero() {
        // A plain "0" (no O_LARGEFILE, no access flags).
        let fdinfo = "pos:\t0\nflags:\t0\nmnt_id:\t29\n";
        assert!(!is_write_mode(fdinfo), "flags 0 is O_RDONLY");
    }

    #[test]
    fn is_write_mode_returns_false_on_missing_flags_line() {
        let fdinfo = "pos:\t42\nmnt_id:\t29\n";
        assert!(!is_write_mode(fdinfo));
    }

    #[test]
    fn is_write_mode_handles_append() {
        // O_WRONLY | O_APPEND = 0o1 | 0o2000 = 0o2001 → with O_LARGEFILE: 0o102001
        let fdinfo = "pos:\t0\nflags:\t0102001\nmnt_id:\t29\n";
        assert!(is_write_mode(fdinfo), "O_WRONLY|O_APPEND is still a write");
    }

    #[test]
    fn outcome_includes_deferred_in_summary() {
        let o = Outcome {
            uploaded: 3,
            deferred: 2,
            ..Default::default()
        };
        assert!(!o.is_empty());
        assert_eq!(o.summary(), "3 uploaded, 2 deferred (open for write)");
    }

    #[test]
    fn outcome_deferred_alone_is_not_empty() {
        let mut o = Outcome::default();
        assert!(o.is_empty());
        o.deferred = 1;
        assert!(!o.is_empty());
    }
}
