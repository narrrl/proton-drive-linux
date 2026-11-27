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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use proton_drive_rs::Node;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use sha1::{Digest as _, Sha1};
use tracing::{debug, info, warn};

use crate::evidence::{Evidence, RemoteEvidence, Side};
use crate::planner::HasKind;
use pdfs_core::batch;
use pdfs_core::control::{ActivityKind, TransferDirection};
use pdfs_core::syncignore::IgnoreRules;
use pdfs_core::timestamp::now_secs;
use pdfs_core::transfers::{CountingWriter, OwnedCountingReader};
use pdfs_drive::parse_uid;
use pdfs_store::db::{SavedWalk, SavedWalkItem, StoredSyncEntry, StoredSyncFolder};

mod apply;
mod engine;
mod host;
mod local;
mod local_scan;
mod push;
mod walk;
use crate::model;
use crate::planner;
use crate::trace;
pub use engine::SYNC_CONCURRENCY;
pub use engine::{EngineHealth, SyncMsg, run};
#[cfg(test)]
use engine::{Running, burst_order, keep_wanted, smallest_first};
#[cfg(test)]
use engine::{classify, event_target, settle_with};
pub use host::{SyncHost, SyncParts, Syncer};
use local::*;
pub use local::{mirror_subtree_unsynced, mirror_tree_gone, mirror_tree_unsynced};
#[cfg(test)]
use local_scan::is_write_mode;
use local_scan::open_for_write_set;
use model::{Applied, remote_file_sig, system_mtime};
use model::{LocalItem, LocalSig, Outcome, PassAbort, Pending, RemoteItem, RemoteSeen};
pub use planner::base_name;
#[cfg(test)]
use planner::conflict_path;
#[cfg(test)]
use planner::conflict_path_as;
#[cfg(test)]
use planner::filter_baseline;
use planner::{
    FilePlan, RemoteGaps, Unscanned, classification_order, guard_local_wipe, guard_remote_wipe,
    join_rel, local_deletions, parent_rel, plan_file, rel_to_path, remote_sig,
    remote_subtree_changed, revision_confirms_baseline, revision_moved, tracked_baseline,
    unchanged_remote_size,
};
use trace::{Call, Count, Phase};

/// A pass that took at least this long is logged at info even when it changed
/// nothing, so the journal says where a slow engine spends its time.
const SLOW_PASS: Duration = Duration::from_secs(30);

impl Syncer {
    /// Reconcile one synced folder, then apply any mode switch queued while it was
    /// busy. Both halves live here because the switch has to happen with the pass
    /// finished and its lock released, and the engine thread is the only place that
    /// is guaranteed — a switch requested over the control socket cannot wait for a
    /// pass that may run for minutes.
    pub fn reconcile_folder(&self, folder: &StoredSyncFolder) {
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
        let lock = self.lock(folder.id);
        let waited = Instant::now();
        let _guard = lock.lock();
        self.trace_phase(Phase::LockWait, waited.elapsed());
        // `folder` was read before the lock; a switch may have landed in between, so
        // re-read and re-check the mode rather than trusting the snapshot.
        let current = match self.store.sync_folder_get(folder.id) {
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
                .store
                .sync_folder_set_state(folder.id, "error", now_secs());
            return;
        }

        let _ = self
            .store
            .sync_folder_set_state(folder.id, "syncing", folder.last_sync);
        let name = base_name(&folder.local_path);
        // Rebuilt per pass rather than cached on `Core`, so editing `.pdfsignore`
        // takes effect on the next reconcile instead of at the next daemon start.
        // The cost is one small file read against a pass that is about to walk the
        // whole tree.
        let rules = self.ignore_rules(&local_root);
        // A first pass is the one a user waits on after adding or restoring a
        // folder, so it is said at info; the poll's passes stay at debug.
        if folder.last_sync == 0 {
            info!(id = folder.id, path = %folder.local_path, "sync: first pass starting");
        } else {
            debug!(id = folder.id, "sync: pass starting");
        }
        let started = Instant::now();
        self.progress_begin(folder.id);
        // An attachment's root can be trashed or moved by the web app. A
        // missing root must never turn into a scan of an empty remote tree.
        let attached_root = if current.kind == "attached" {
            self.attach_ancestors(&remote_root).and_then(|ancestors| {
                if ancestors.contains(&self.primary_root_uid) {
                    Ok(())
                } else {
                    Err(pdfs_core::CoreError::invalid(
                        "attached folder is no longer below My files",
                    ))
                }
            })
        } else {
            Ok(())
        };
        let result = if let Err(error) = attached_root {
            Err(PassAbort::Failed(error.to_string()))
        } else if push_only {
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
        let progress = self.progress.lock().get(&folder.id).cloned();
        self.progress_end(folder.id);
        self.log_trace(folder, &result, started.elapsed());
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
                let _ = self
                    .store
                    .sync_folder_set_state(folder.id, state, now_secs());
                let elapsed = started.elapsed();
                if let Some(first_error) = &outcome.first_error {
                    warn!(
                        id = folder.id,
                        state,
                        elapsed_ms = elapsed.as_millis() as u64,
                        summary = %outcome.summary(),
                        first_error,
                        "sync: pass finished"
                    );
                } else if folder.last_sync == 0 || !outcome.is_empty() || elapsed >= SLOW_PASS {
                    info!(
                        id = folder.id,
                        state,
                        elapsed_ms = elapsed.as_millis() as u64,
                        summary = %outcome.summary(),
                        "sync: pass finished"
                    );
                } else {
                    debug!(id = folder.id, state, "sync: pass finished");
                }
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
                info!(
                    id = folder.id,
                    elapsed_s = started.elapsed().as_secs(),
                    phase = ?progress.as_ref().map(|p| p.phase),
                    done = progress.as_ref().map_or(0, |p| p.done),
                    total = progress.as_ref().map_or(0, |p| p.total),
                    "sync: pass cut short by the stop"
                );
                let _ =
                    self.store
                        .sync_folder_set_state(folder.id, &current.state, current.last_sync);
            }
            Err(PassAbort::Abandoned) => {
                info!(
                    id = folder.id,
                    elapsed_s = started.elapsed().as_secs(),
                    "sync: pass stopped for the folder's removal"
                );
                let _ =
                    self.store
                        .sync_folder_set_state(folder.id, &current.state, current.last_sync);
            }
            Err(PassAbort::Failed(e)) => {
                warn!(id = folder.id, error = %e, "sync: reconcile failed");
                let _ = self
                    .store
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
        // A removal that gave up waiting leaves its request behind; the next
        // pass runs whole unless the removal is asked for again.
        self.abandon.lock().remove(&folder.id);
    }

    /// End the running pass's trace and put it in the journal: at info for
    /// a pass worth hearing about (a first, slow or unfinished one), else at
    /// debug, so the poll over a quiet folder adds nothing every two minutes.
    fn log_trace(
        &self,
        folder: &StoredSyncFolder,
        result: &Result<Outcome, PassAbort>,
        elapsed: Duration,
    ) {
        let how = match result {
            Ok(_) => "finished",
            Err(PassAbort::Stopped | PassAbort::Interrupted) => "cut short",
            Err(PassAbort::Abandoned) => "abandoned",
            Err(PassAbort::Failed(_)) => "failed",
        };
        let Some(trace) = self.engine.end_trace(how) else {
            return;
        };
        if how != "finished" || folder.last_sync == 0 || elapsed >= SLOW_PASS {
            info!(id = folder.id, how, trace = %trace, "sync: pass trace");
        } else {
            debug!(id = folder.id, how, trace = %trace, "sync: pass trace");
        }
    }

    /// Shadow check of a deletion the pass is about to make: count and log it
    /// when the evidence does not prove the path gone on the side the
    /// deletion is based on. Acts on nothing; the filters before the plan are
    /// what keep such a path out of it, and this says when they did not.
    fn check_deletion<L: HasKind, R: HasKind>(
        &self,
        evidence: &Evidence<'_, L, R>,
        rel: &str,
        side: Side,
    ) {
        if let Some(basis) = evidence.unproven_deletion(rel, side) {
            warn!(
                rel,
                ?side,
                ?basis,
                "sync: a planned deletion is not backed by proof of absence"
            );
            self.trace_count(Count::UnprovenDeletions, 1);
        }
    }

    /// Add `took` to a stage of the running pass's trace.
    fn trace_phase(&self, phase: Phase, took: Duration) {
        self.engine.trace(|t| t.phase(phase, took));
    }

    /// Add one call that took `took` to the running pass's trace.
    fn trace_call(&self, call: Call, took: Duration) {
        self.engine.trace(|t| t.call(call, took));
    }

    /// Add `n` to a counter of the running pass's trace.
    fn trace_count(&self, count: Count, n: usize) {
        self.engine.trace(|t| t.count(count, n));
    }

    /// Tell the activity feed what a pass over this folder had to leave out
    /// because it cannot be read, once each time that changes. Every other path
    /// in the folder still synced, so this does not make the folder's state an
    /// error; but a folder the user has no permission for is never going to
    /// reach Drive, and they should hear that once rather than not at all
    /// (B196).
    fn report_unscanned(&self, folder_id: i64, name: &str, unscanned: &[String]) {
        let previous = {
            let mut last = self.unscanned.lock();
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
    pub fn ignore_rules(&self, local_root: &Path) -> IgnoreRules {
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
            self.store.sync_folder_get(folder_id),
            Ok(Some(f)) if f.pending_mode.as_deref() == Some("ondemand")
        )
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
        let step = Instant::now();
        let baseline = self
            .store
            .sync_entries(folder_id)
            .map_err(|e| format!("load baseline: {e:?}"))?;
        self.trace_phase(Phase::Baseline, step.elapsed());
        // Both sides are walked, so each baseline path is checked about twice.
        self.progress_scan_total(folder_id, baseline.len() * 2);

        let mut local: HashMap<String, LocalItem> = HashMap::new();
        let mut unscanned = Unscanned::default();
        let step = Instant::now();
        let writing = open_for_write_set(local_root);
        self.walk_local(
            folder_id,
            local_root,
            rules,
            &writing,
            &mut local,
            &mut unscanned,
        )?;
        self.trace_phase(Phase::LocalWalk, step.elapsed());
        self.trace_count(Count::LocalEntries, local.len());
        // See `push_pass`: the guard must not count excluded paths as lost.
        guard_local_wipe(&tracked_baseline(&baseline, rules, &unscanned), &local)?;

        let mut remote: HashMap<String, RemoteItem> = HashMap::new();
        let mut remote_dirs: HashMap<String, NodeUid> = HashMap::new();
        let mut gaps = RemoteGaps::default();
        remote_dirs.insert(String::new(), remote_root.clone());
        let step = Instant::now();
        let walked = self.walk_remote(
            folder_id,
            remote_root,
            &mut RemoteWalk {
                out: &mut remote,
                dirs: &mut remote_dirs,
                gaps: &mut gaps,
            },
            &baseline,
        );
        // Timed even when cut short: a walk that never ends is the case the
        // trace exists for.
        self.trace_phase(Phase::RemoteWalk, step.elapsed());
        walked?;
        guard_remote_wipe(&tracked_baseline(&baseline, rules, &unscanned), &remote)?;

        let mut order = classification_order(&local, &remote, &baseline, rules);
        // A path the scan could not read is no more a local deletion than an
        // ignored one, and a remote path under it has nowhere to be written.
        // The same holds for a path the remote walk could not read: missing
        // from it, it would read as deleted on Drive (B212).
        // Only a path both walks know about is classified (`evidence`).
        order.retain(|rel| {
            pass_evidence(rules, &local, &unscanned, &remote, &remote_dirs, &gaps).classifiable(rel)
        });

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
        // The plan stage is the loop's time without the batches it flushes,
        // which `flush_batch` times as the apply stage.
        let planning = Instant::now();
        let mut applying = Duration::ZERO;

        for rel in &order {
            let depth = rel.matches('/').count();
            if depth > batch_depth {
                let step = Instant::now();
                let flushed = self.flush_batch(
                    folder_id,
                    local_root,
                    &mut remote_dirs,
                    std::mem::take(&mut batch),
                    &mut outcome,
                );
                applying += step.elapsed();
                if flushed.is_err() {
                    self.trace_phase(Phase::Plan, planning.elapsed().saturating_sub(applying));
                }
                flushed?;
                batch_depth = depth;
                // A depth boundary is the pass's natural checkpoint — everything
                // queued has been applied, so stopping here leaves nothing half done.
                if self.ondemand_queued(folder_id) {
                    self.trace_phase(Phase::Plan, planning.elapsed().saturating_sub(applying));
                    return Err(PassAbort::Interrupted);
                }
            }

            let l = local.get(rel);
            let r = remote.get(rel);
            let is_dir = l.map(|x| x.is_dir).or(r.map(|x| x.is_dir)).unwrap_or(false);
            let in_base = baseline.contains_key(rel);

            if is_dir {
                match (l.is_some(), r.is_some()) {
                    // Written only when it changed: every baseline write
                    // marks the folder it is in to be listed on Drive again.
                    (true, true) if baseline.get(rel) == Some(&dir_row(rel, &remote[rel].uid)) => {}
                    (true, true) => {
                        if let Err(e) = self.baseline_dir(folder_id, rel, &remote[rel].uid) {
                            warn!(rel, error = %e, "sync: folder step failed; continuing");
                            outcome.failed(&format!("{rel}: {e}"));
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
                                outcome.failed(&format!("{rel}: no remote parent for folder"));
                            }
                        }
                    }
                    (true, false) => {
                        // In baseline, gone remotely → the remote side deleted it.
                        self.check_deletion(
                            &pass_evidence(rules, &local, &unscanned, &remote, &remote_dirs, &gaps),
                            rel,
                            Side::Local,
                        );
                        delete_local_dirs.push(rel.clone());
                    }
                    (false, true) if !in_base => {
                        // New remote folder → create locally (cheap, inline).
                        if let Err(e) = std::fs::create_dir_all(local_root.join(rel_to_path(rel)))
                            .map_err(|e| format!("mkdir {rel}: {e}"))
                            .and_then(|()| self.baseline_dir(folder_id, rel, &remote[rel].uid))
                        {
                            warn!(rel, error = %e, "sync: folder step failed; continuing");
                            outcome.failed(&format!("{rel}: {e}"));
                        }
                    }
                    (false, true) => {
                        // In baseline, gone locally → the local side deleted it.
                        self.check_deletion(
                            &pass_evidence(rules, &local, &unscanned, &remote, &remote_dirs, &gaps),
                            rel,
                            Side::Remote,
                        );
                        delete_remote_dirs.push((rel.clone(), remote[rel].uid.clone()));
                    }
                    (false, false) => {
                        let _ = self.store.sync_entry_remove(folder_id, rel);
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
                        if let Err(e) = self.store.sync_entry_upsert(folder_id, &row) {
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
                        outcome.failed(&format!("{rel}: no remote parent for file"));
                    }
                },
                FilePlan::DeleteLocal => {
                    self.check_deletion(
                        &pass_evidence(rules, &local, &unscanned, &remote, &remote_dirs, &gaps),
                        rel,
                        Side::Local,
                    );
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
                        outcome.failed(&format!("{rel}: {e}"));
                        continue;
                    }
                    let _ = self.store.sync_entry_remove(folder_id, rel);
                    self.log_activity(ActivityKind::Trash, base_name(rel), "removed locally", true);
                    outcome.deleted += 1;
                }
                FilePlan::DeleteRemote => {
                    self.check_deletion(
                        &pass_evidence(rules, &local, &unscanned, &remote, &remote_dirs, &gaps),
                        rel,
                        Side::Remote,
                    );
                    let remote = r.expect("remote deletion requires a remote file");
                    let step = Instant::now();
                    let trashed = self
                        .rt
                        .block_on(self.drive.trash_nodes(std::slice::from_ref(&remote.uid)))
                        .and_then(batch::into_unit);
                    self.trace_call(Call::Trash, step.elapsed());
                    if let Err(e) = trashed {
                        warn!(rel, error = %e, "sync: trash remote failed");
                        self.log_activity(
                            ActivityKind::Trash,
                            base_name(rel),
                            e.to_string(),
                            false,
                        );
                        outcome.failed(&format!("{rel}: {e}"));
                        continue;
                    }
                    if let Some(parent) = parent_uid_of(rel, &remote_dirs, &baseline) {
                        self.invalidate_children_of(&parent);
                    }
                    let _ = self.store.sync_entry_remove(folder_id, rel);
                    self.log_activity(
                        ActivityKind::Trash,
                        base_name(rel),
                        "removed on Drive",
                        true,
                    );
                    outcome.deleted += 1;
                }
                FilePlan::ForgetBaseline => {
                    let _ = self.store.sync_entry_remove(folder_id, rel);
                }
            }
        }
        self.trace_phase(Phase::Plan, planning.elapsed().saturating_sub(applying));
        // Flush the deepest level's batch.
        self.flush_batch(
            folder_id,
            local_root,
            &mut remote_dirs,
            std::mem::take(&mut batch),
            &mut outcome,
        )?;
        let cleanup = Instant::now();

        // Deferred folder deletions, deepest first. A folder that holds what
        // Drive never had stays, and its baseline row goes, so the next pass
        // creates it on Drive again and uploads what it holds (B211).
        if !delete_local_dirs.is_empty() {
            let mut current = self
                .store
                .sync_entries(folder_id)
                .map_err(|e| format!("reload baseline: {e:?}"))?;
            for removal in remove_local_dirs(local_root, delete_local_dirs, &mut current) {
                match removal {
                    DirRemoval::Removed(rel) => {
                        let _ = self.store.sync_entry_remove(folder_id, &rel);
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
                        let _ = self.store.sync_entry_remove(folder_id, &rel);
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
                        outcome.failed(&format!("{rel}: {e}"));
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
            let step = Instant::now();
            let trashed = self
                .rt
                .block_on(self.drive.trash_nodes(&[uid]))
                .and_then(batch::into_unit);
            self.trace_call(Call::Trash, step.elapsed());
            if let Err(e) = trashed {
                warn!(rel, error = %e, "sync: trash remote folder failed");
                self.log_activity(ActivityKind::Trash, base_name(&rel), e.to_string(), false);
                outcome.failed(&format!("{rel}: {e}"));
                continue;
            }
            if let Some(parent) = parent_uid_of(&rel, &remote_dirs, &baseline) {
                self.invalidate_children_of(&parent);
            }
            let _ = self.store.sync_entry_remove(folder_id, &rel);
            self.log_activity(
                ActivityKind::Trash,
                base_name(&rel),
                "removed on Drive",
                true,
            );
            outcome.deleted += 1;
        }
        self.trace_phase(Phase::Cleanup, cleanup.elapsed());

        Ok(outcome)
    }
}

/// A [`Read`] that hashes what an upload reads through it, so the digest Drive
/// reports afterwards can be compared with what was sent.
/// The evidence of a mirror pass, built where it is asked so it borrows the
/// walks only for the question: `remote_dirs` gains the folders the pass
/// creates between questions.
fn pass_evidence<'a>(
    rules: &'a IgnoreRules,
    local: &'a HashMap<String, LocalItem>,
    unscanned: &'a Unscanned,
    remote: &'a HashMap<String, RemoteItem>,
    remote_dirs: &'a HashMap<String, NodeUid>,
    gaps: &'a RemoteGaps,
) -> Evidence<'a, LocalItem, RemoteItem> {
    Evidence {
        rules,
        local,
        unscanned,
        remote: Some(RemoteEvidence {
            entries: remote,
            dirs: remote_dirs,
            gaps,
        }),
    }
}

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
        pdfs_drive::digest::hex_lower(&self.0.lock().clone().finalize())
    }
}

/// The `(rel_path, revision_id)` pairs [`Core::record_revisions`] stores for
/// one listing: files whose size was read for real and matches the baseline.
fn revisions_to_record(
    prefix: &str,
    nodes: &[Node],
    sized: &HashMap<String, (i64, Option<String>)>,
    baseline: &HashMap<String, StoredSyncEntry>,
) -> Vec<(String, String)> {
    nodes
        .iter()
        .filter(|node| !node.trashed && !node.is_folder())
        .filter_map(|node| {
            let revision = pdfs_drive::node_revision_id(node)?;
            let (size, _) = sized.get(&node.uid.to_string())?;
            let rel = join_rel(prefix, &node.name);
            revision_confirms_baseline(baseline.get(&rel)?, node.modification_time, *size)
                .then_some((rel, revision))
        })
        .collect()
}

/// What one folder's listing on Drive held.
#[derive(Default)]
struct Listing {
    items: Vec<(String, RemoteItem)>,
    /// Files seen whose size could not be read.
    unread: Vec<String>,
    /// The listing named nodes that could not be read.
    short: bool,
}

/// How old a saved walk of Drive may be and still be resumed. Its listings
/// are as old as that, as the first ones of a walk that long would be.
const WALK_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// The baseline row of a synced folder: only its uid on Drive.
fn dir_row(rel: &str, uid: &NodeUid) -> StoredSyncEntry {
    StoredSyncEntry {
        rel_path: rel.to_string(),
        remote_uid: Some(uid.to_string()),
        local_mtime: 0,
        local_mtime_ns: None,
        local_size: 0,
        remote_rev: None,
        remote_hash: None,
        remote_revision_id: None,
    }
}

/// How old the picture of Drive a walk gave may grow before a pass walks all
/// of Drive again. Between full walks, a pass lists only the folders the event
/// stream marked dirty; this full walk catches what the stream may have missed.
const AUDIT_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// How lately Drive's event stream must have been read for a pass to rely on
/// it. A few polls' worth.
const EVENTS_FRESH: Duration = Duration::from_secs(3 * crate::EVENT_POLL_INTERVAL.as_secs());

/// A folder for a walk of Drive to list.
struct WalkNext {
    prefix: String,
    uid: NodeUid,
}

/// Where [`Core::walk_remote`] starts.
struct WalkStart {
    todo: VecDeque<WalkNext>,
    /// What each folder held, for a walk that lists folders again.
    children: Option<Children>,
    /// For a walk of the dirty folders only: the last dirty mark it read.
    dirty_upto: Option<i64>,
}

/// The paths directly in each folder of a walk, by folder.
#[derive(Default)]
struct Children(HashMap<String, Vec<String>>);

impl Children {
    fn of(&self, dir: &str) -> &[String] {
        self.0.get(dir).map_or(&[], Vec::as_slice)
    }

    fn take(&mut self, dir: &str) -> Vec<String> {
        self.0.remove(dir).unwrap_or_default()
    }

    fn add<'a>(&mut self, dir: &str, rels: impl IntoIterator<Item = &'a String>) {
        self.0
            .entry(dir.to_string())
            .or_default()
            .extend(rels.into_iter().cloned());
    }

    /// Drop `rel` and everything under it from `walk`, noting each path in
    /// `removed`.
    fn remove_subtree(&mut self, rel: &str, walk: &mut RemoteWalk<'_>, removed: &mut Vec<String>) {
        let mut stack = vec![rel.to_string()];
        while let Some(rel) = stack.pop() {
            walk.out.remove(&rel);
            walk.dirs.remove(&rel);
            walk.gaps.forget(&rel);
            stack.extend(self.take(&rel));
            removed.push(rel);
        }
    }
}

/// A saved walk loaded back into a [`RemoteWalk`].
struct Restored {
    /// The folders found and not listed yet.
    todo: VecDeque<WalkNext>,
    /// How many paths came back.
    items: usize,
    children: Children,
}

/// Load a saved walk into `walk`. `None` when it cannot be read back, so the
/// walk starts over.
fn restore_walk(saved: &SavedWalk, walk: &mut RemoteWalk<'_>) -> Option<Restored> {
    let mut todo = VecDeque::new();
    for dir in &saved.dirs {
        if dir.short {
            walk.gaps.short_listing(&dir.rel_path);
        }
        if !dir.listed {
            todo.push_back(WalkNext {
                prefix: dir.rel_path.clone(),
                uid: parse_uid(&dir.uid)?,
            });
        }
    }
    let mut children = Children::default();
    for item in &saved.items {
        children
            .0
            .entry(parent_rel(&item.rel_path).to_string())
            .or_default()
            .push(item.rel_path.clone());
        let Some(uid) = &item.uid else {
            walk.gaps.unread(item.rel_path.clone());
            continue;
        };
        let uid = parse_uid(uid)?;
        if item.is_dir {
            walk.dirs.insert(item.rel_path.clone(), uid.clone());
        }
        walk.out.insert(
            item.rel_path.clone(),
            RemoteItem {
                uid,
                is_dir: item.is_dir,
                mtime: item.mtime,
                size: item.size,
                revision: item.revision.clone(),
                sha1: item.sha1.clone(),
            },
        );
    }
    Some(Restored {
        todo,
        items: saved.items.len(),
        children,
    })
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

/// A listing call's result in [`Core::walk_remote`]. A failure below the root
/// records `prefix` as unlisted and returns `None`, so the walk goes on
/// without that folder. A failure at the root still fails the pass, since
/// nothing would be left to compare.
///
/// One listing error, after the SDK's own retries, used to fail the whole
/// walk. Nothing is applied before the walk ends, so on a link that drops a
/// request now and then a large first sync started over from nothing on
/// every pass and never got past it (`docs/BUGS.md` B271).
fn unlisted_gap<T>(
    walk: &mut RemoteWalk<'_>,
    prefix: &str,
    result: Result<T, String>,
) -> Result<Option<T>, PassAbort> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if prefix.is_empty() => Err(error.into()),
        Err(error) => {
            warn!(
                folder = prefix,
                error, "sync: a folder on Drive could not be listed; leaving it out of this pass"
            );
            walk.gaps.unlisted(prefix);
            Ok(None)
        }
    }
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

// ---- helpers --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic in a pass ends that pass and is counted; the engine is not
    /// left believing the pass still runs (`docs/BUGS.md` B259).
    #[test]
    fn a_panicking_pass_is_contained_and_counted() {
        let health = EngineHealth::default();
        assert!(!health.contain(7, || panic!("a pass that panics")));
        let report = health.report();
        assert_eq!(report.panics, 1);
        assert_eq!(report.pass_folder, None);
        assert!(health.contain(7, || {}));
        assert_eq!(health.report().panics, 1);
    }

    /// Each folder's last traced pass stays in the report, the running one
    /// comes first, and a pass that never got going leaves none (B294).
    #[test]
    fn the_last_pass_of_each_folder_is_traced() {
        let health = EngineHealth::default();
        health.contain(2, || {
            health.trace(|t| t.call(Call::ReadNodesFull, Duration::from_secs(2)));
            health.end_trace("cut short");
        });
        health.contain(3, || {});
        assert!(!health.contain(4, || panic!("a pass that panics")));
        let mut running = None;
        health.contain(3, || {
            health.trace(|t| t.count(Count::Nodes, 5));
            running = Some(health.report().traces);
        });

        let running = running.unwrap();
        assert_eq!(running[0].folder_id, 3);
        assert_eq!(running[0].ended, "");
        let ended: Vec<(i64, String)> = health
            .report()
            .traces
            .into_iter()
            .map(|t| (t.folder_id, t.ended))
            .collect();
        assert_eq!(
            ended,
            [(2, "cut short".into()), (4, "panicked".into())],
            "folder 3 never ended its trace"
        );
        let cut = &health.report().traces[0];
        assert_eq!(cut.calls[0].name, "read nodes full");
        assert_eq!(cut.calls[0].total_ms, 2000);
    }

    /// Folders named by id run before the rest of a `ReconcileAll` in the
    /// same burst, so a folder just added does not wait behind a pass over
    /// every other folder (`docs/BUGS.md` B260).
    #[test]
    fn named_folders_run_before_the_rest_of_a_burst() {
        let ids: HashSet<i64> = [9, 4].into();
        assert_eq!(burst_order(&ids, &[1, 4, 6, 9]), [4, 9, 1, 6]);
        assert_eq!(burst_order(&HashSet::new(), &[1, 4]), [1, 4]);
        // A folder the list no longer has still gets the pass it asked for.
        assert_eq!(burst_order(&[12].into(), &[1]), [12, 1]);
    }

    /// A folder too large to finish before the daemon stops no longer takes
    /// the first turn of every start from the others (`docs/BUGS.md` B294).
    #[test]
    fn the_smallest_folders_run_first_in_a_burst() {
        let counts: HashMap<i64, usize> = [(2, 493_000), (3, 60_000), (4, 45_000), (7, 40)].into();
        assert_eq!(smallest_first(&[2, 3, 4, 7], &counts), [7, 4, 3, 2]);
        // A folder with no baseline yet is the cheapest of all; ties keep their order.
        assert_eq!(smallest_first(&[9, 2, 8], &counts), [9, 8, 2]);
        // Named folders still go before the rest.
        let order = burst_order(&[2].into(), &smallest_first(&[2, 3, 7], &counts));
        assert_eq!(order, [2, 7, 3]);
    }

    /// A folder removed and added again keeps its watch under the new id;
    /// a folder that is gone loses its watch (`docs/BUGS.md` B272).
    #[test]
    fn a_re_added_folder_keeps_its_watch_under_the_new_id() {
        let mut have = vec![(PathBuf::from("/a"), 1), (PathBuf::from("/b"), 2)];
        let dropped = keep_wanted(&mut have, &[(PathBuf::from("/a"), 7)]);
        assert_eq!(have, [(PathBuf::from("/a"), 7)]);
        assert_eq!(dropped, [PathBuf::from("/b")]);
    }

    /// While a pass runs, the report names its folder, which is what tells a
    /// long pass apart from an idle engine.
    #[test]
    fn the_running_pass_is_reported() {
        let health = EngineHealth::default();
        let mut seen = None;
        health.contain(7, || seen = health.report().pass_folder);
        assert_eq!(seen, Some(7));
    }

    /// The engine reads as running only while its thread lives, including
    /// when the thread ends in a panic.
    #[test]
    fn the_engine_reads_as_stopped_once_its_thread_ends() {
        let health = Arc::new(EngineHealth::default());
        let thread_health = health.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (end_tx, end_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _running = Running::start(&thread_health);
            started_tx.send(()).unwrap();
            let _ = end_rx.recv();
            panic!("the engine thread panics");
        });
        started_rx.recv().unwrap();
        assert!(health.report().running);
        end_tx.send(()).unwrap();
        assert!(thread.join().is_err());
        assert!(!health.report().running);
    }

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
    fn guard_local_wipe_blocks_a_total_disappearance() {
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
            "in a small folder, one survivor means the tree is readable; the rest are real deletions"
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
            conflict_path_as(
                Path::new("/home/me/README"),
                crate::naming::conflict_label(),
                42,
                0
            )
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
            conflict_path_as(&source, crate::naming::conflict_label(), 1700, 1)
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

    /// Emptied before any pass saw it, the folder waits for the push pass that
    /// forgets those paths, instead of being refused for good (B255).
    #[test]
    fn only_a_local_copy_gone_in_full_counts_as_gone() {
        let (root, baseline) = synced_subtree("evict-gone");
        let rules = rules_for(&[]);
        let whole = mirror_tree_gone(&root, &baseline, &rules);
        std::fs::remove_dir_all(root.join("dir/sub")).unwrap();
        let partly = mirror_tree_gone(&root, &baseline, &rules);
        std::fs::remove_dir_all(root.join("dir")).unwrap();
        let emptied = mirror_tree_gone(&root, &baseline, &rules);
        let none_synced = mirror_tree_gone(&root, &HashMap::new(), &rules);
        std::fs::remove_dir_all(&root).unwrap();
        let no_root = mirror_tree_gone(&root, &baseline, &rules);
        assert!(!whole && !partly, "a deletion with survivors is a real one");
        assert!(emptied);
        assert!(!none_synced, "an empty baseline has nothing to lose");
        assert!(!no_root, "a missing folder is the pass's error to report");
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
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
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
