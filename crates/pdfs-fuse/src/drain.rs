//! The write-back drain: turning queued mutations into remote calls.
//!
//! Every write the kernel hands us is answered the moment its bytes and its
//! `pending_op` row are on disk (offline.md Phase 3), which is what lets a `cp`
//! into the mount run at disk speed and lets an offline write succeed at all.
//! This module is the other half: the worker that walks that queue and performs
//! the uploads, creates, renames and trashes it recorded.
//!
//! Two invariants matter more than anything else here, because between them
//! they are the only thing standing between a queued write and lost data:
//!
//! 1. A staged blob is the *only* copy of the user`s bytes. It is dropped only
//!    after the op it belongs to has provably landed — never before, and never
//!    on a failure path that might be retried.
//! 2. A failure never pauses the queue. Recording it pushes that op`s
//!    `next_attempt_at` past now, so one file wedged against a vanished parent
//!    cannot hold up an unrelated upload behind it.
//!
//! When the remote has moved on underneath a queued revision, the losing bytes
//! are kept as a conflict copy rather than discarded — see
//! [`Core::revision_conflict`].

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use pdfs_core::batch;
use pdfs_core::cache::{Baseline, StagedWrite};
use pdfs_core::control::{ActivityKind, SyncIssue, TransferDirection};
use pdfs_core::db::{
    CreateLanding, CreateRetired, OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH,
    PARK_EXPIRY_MS, PendingOp, RenameMeta,
};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use tracing::{debug, error, info, warn};

use super::link::{self, is_network_error};
use super::state::{Intervals, PendingRevision};
use super::takeout::sha1_of;
use super::transfers::CountingReader;
use super::{
    Core, DRAIN_BACKOFF_MAX, DRAIN_BACKOFF_MIN, DRAIN_IDLE_POLL, DRAIN_REVISION_DEBOUNCE,
    DRAIN_REVISION_DEBOUNCE_MAX, UPLOAD_TIME_MEMORY, WriteAuthority, conflict_name,
    is_already_exists, is_gone, is_local_uid_str, media_type_for, node_revision_id, node_size,
    now_millis, now_secs, parse_node_uid,
};
use proton_drive_rs::proton_sdk::api::ResponseCode;
use proton_drive_rs::proton_sdk::error::ProtonError;

const DRAIN_ACCESS_RECHECK: Duration = Duration::from_secs(5);

/// How long an op may stay access-deferred before the deferral is reported as a
/// failure instead of retried in silence.
///
/// An access deferral costs no attempt and records no error, which is right for
/// the case it was written for: a share downgraded while the queue was moving,
/// undone a moment later. It is wrong for anything that does not clear. Until
/// this window existed such an op was re-deferred every
/// [`DRAIN_ACCESS_RECHECK`] indefinitely with `attempts` at zero, `last_error`
/// null and only a `debug!` line to show for it, so `pdfs status` counted it
/// among ordinary queued uploads and nothing ever said otherwise.
///
/// The condition that produced that in practice — a node absent from the local
/// tree, which [`Db::effective_node_access`] used to report indistinguishably
/// from a revoked permission — is now answered directly by
/// [`Core::resolve_unknown_authority`]. This window remains the backstop for
/// every deferral that does not resolve, including a refetch that keeps not
/// helping.
///
/// Five minutes is far longer than a permission change takes to settle and far
/// shorter than a user's patience with bytes that are not moving. Past it each
/// recheck records a failure, so the op enters the ordinary backoff and, at
/// [`FAILING_ATTEMPTS`], is counted and named by `Response::Status`.
///
/// [`Db::effective_node_access`]: pdfs_core::db::Db::effective_node_access
/// [`FAILING_ATTEMPTS`]: pdfs_core::db::FAILING_ATTEMPTS
const DRAIN_ACCESS_DEFER_LIMIT: Duration = Duration::from_secs(300);

/// A registered cancellation flag for one in-flight upload, deregistered when
/// dropped. See [`Core::begin_cancellable_upload`].
struct UploadCancel {
    registry: Arc<Mutex<HashMap<NodeUid, Arc<AtomicBool>>>>,
    uid: NodeUid,
    flag: Arc<AtomicBool>,
}

impl UploadCancel {
    /// Whether this upload was told to stop. Distinguishes an upload we
    /// abandoned on purpose from one the network broke.
    fn cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
}

impl Drop for UploadCancel {
    fn drop(&mut self) {
        let mut registry = self.registry.lock();
        // By identity: a supersede racing this upload's last bytes may already
        // have registered the next one under the same uid.
        if registry
            .get(&self.uid)
            .is_some_and(|current| Arc::ptr_eq(current, &self.flag))
        {
            registry.remove(&self.uid);
        }
    }
}

/// One upload counted in [`Core::landing_uploads`] from its op's retirement
/// until its bytes are cached, and uncounted when dropped, early return or not.
struct LandingUpload(Arc<AtomicU64>);

impl LandingUpload {
    fn begin(count: &Arc<AtomicU64>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for LandingUpload {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The debounce arithmetic of [`Core::revision_debounce`], without the map:
/// the last measured upload time for this node, bounded on both sides.
///
/// A node nobody has uploaded yet gets the fixed grace period, which is the
/// same answer the fixed debounce always gave.
fn adaptive_debounce(measured: Option<Duration>) -> Duration {
    measured
        .unwrap_or(DRAIN_REVISION_DEBOUNCE)
        .clamp(DRAIN_REVISION_DEBOUNCE, DRAIN_REVISION_DEBOUNCE_MAX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DrainDisposition {
    Applied,
    AccessDeferred,
    /// An authority this op needs is missing from the local tree, so no access
    /// answer exists for it yet. Carries the uid to ask the remote about.
    AuthorityUnknown(NodeUid),
}

/// The node an earlier attempt at a create made before a rename moved the op
/// (`docs/BUGS.md` B184).
struct EarlierTwin {
    /// The folder the attempt was sent to.
    parent: NodeUid,
    /// The name it was sent under.
    name: String,
    /// The node it made.
    twin: NodeUid,
    /// Whether the node holds the op's blob already.
    holds_blob: bool,
}

/// Why an op went back on the clock without consuming an attempt. Only affects
/// what the user is told — every variant is deferred and reported identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessDeferral {
    /// An authority the tree knows about refuses the write.
    NotWritable,
    /// An authority missing from the tree was just re-read from the remote and
    /// re-interned; the access check gets another look on the next pass.
    AuthorityRefetched,
    /// An authority missing from the tree could not be asked about.
    RemoteUnreachable,
}

impl AccessDeferral {
    fn log_reason(self) -> &'static str {
        match self {
            Self::NotWritable => "the node is not writable",
            Self::AuthorityRefetched => "the node was missing locally and has been re-read",
            Self::RemoteUnreachable => "the node is missing locally and the remote is unreachable",
        }
    }

    /// Phrased to complete "<reason> for 300s." in a recorded `last_error`, so
    /// the text the user sees names the actual condition rather than the
    /// permission question it used to be flattened into.
    fn failure_reason(self) -> &'static str {
        match self {
            Self::NotWritable => "the share has not been writable",
            Self::AuthorityRefetched => {
                "the node has been missing from the local tree, and re-reading it from the remote \
                 has not restored it"
            }
            Self::RemoteUnreachable => {
                "the node has been missing from the local tree and the remote has been unreachable"
            }
        }
    }
}

fn pending_op_authorities(op: &PendingOp) -> Result<Vec<NodeUid>, Box<dyn std::error::Error>> {
    let uid = || -> Result<NodeUid, Box<dyn std::error::Error>> {
        parse_node_uid(&op.uid)
            .ok_or_else(|| Box::<dyn std::error::Error>::from("pending op has an unparseable uid"))
    };
    let parent = || -> Result<NodeUid, Box<dyn std::error::Error>> {
        let parent = op
            .parent_uid
            .as_deref()
            .ok_or("pending op has no parent authority")?;
        parse_node_uid(parent).ok_or_else(|| {
            Box::<dyn std::error::Error>::from("pending op has an unparseable parent")
        })
    };
    match op.kind.as_str() {
        // What a withdrawn create leaves: nothing on Drive to ask about but
        // the folder it was sent to, where the node it may have made is
        // trashed (`Core::drain_trash`).
        OP_TRASH if is_local_uid_str(&op.uid) => match op.parent_uid.as_deref() {
            Some(folder) if !is_local_uid_str(folder) => Ok(vec![parent()?]),
            _ => Ok(Vec::new()),
        },
        OP_REVISION | OP_TRASH => Ok(vec![uid()?]),
        OP_CREATE | OP_MKDIR => Ok(vec![parent()?]),
        OP_RENAME => {
            let mut authorities = vec![uid()?];
            if let Some(json) = op.meta_json.as_deref() {
                let meta: RenameMeta = serde_json::from_str(json)?;
                authorities.push(
                    parse_node_uid(&meta.original_parent_uid)
                        .ok_or("rename op has an unparseable original parent")?,
                );
            } else {
                // Rows written by 1.1.1 predate foreign shared mounts. Their
                // source tree was necessarily owned, so the node plus desired
                // destination are the complete available authority set.
            }
            authorities.push(parent()?);
            authorities.dedup();
            Ok(authorities)
        }
        other => Err(format!("unknown pending op kind {other:?}").into()),
    }
}

fn run_authorized_drain(
    op: &PendingOp,
    mut authority_of: impl FnMut(&NodeUid) -> WriteAuthority,
    apply: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<DrainDisposition, Box<dyn std::error::Error>> {
    // This check is the admission/linearization point. A downgrade that lands
    // first defers the op; one observed afterwards does not revoke this admitted
    // attempt. Every later queued operation rechecks. No permission lock is
    // held across remote latency.
    for authority in pending_op_authorities(op)? {
        match authority_of(&authority) {
            WriteAuthority::Writable => {}
            WriteAuthority::Denied => return Ok(DrainDisposition::AccessDeferred),
            // Nothing in the tree can answer for this node. Deferring on it
            // waits for a permission change that has no reason to come, so the
            // caller asks the remote instead (B83).
            WriteAuthority::Unknown => {
                return Ok(DrainDisposition::AuthorityUnknown(authority));
            }
        }
    }
    apply()?;
    Ok(DrainDisposition::Applied)
}

impl Core {
    /// Drain the pending-op queue: the background half of every write
    /// (offline.md Phase 3).
    ///
    /// Runs for the life of the mount. Ops are replayed oldest-first, each
    /// retried with doubling backoff and *never* dropped on failure — the staged
    /// blob is the only copy of the user's bytes, so a failed op stays queued
    /// until it lands or the user deletes the file.
    ///
    /// A failure does not pause the queue. Recording it pushes that op's
    /// `next_attempt_at` past `now`, so the next pass simply picks the next op
    /// that is due — one file wedged against a folder that no longer exists must
    /// not hold up an unrelated upload behind it. The worker only blocks once
    /// nothing is due at all.
    ///
    /// Several of these run at once ([`DRAIN_WORKERS`]), sharing the queue
    /// through [`Db::claim_next_due_op`]: the alternative was that a 10 GiB
    /// upload held every queued rename, trash and small write behind it for as
    /// long as it took. Ordering only has to hold *per node*, and the claim
    /// query is what guarantees it — no two workers are ever on one uid.
    ///
    /// `primary` marks the one worker that also runs the queue's idle chores.
    /// They are cheap, but they are not per-op work and doing them in every
    /// worker would just multiply them.
    ///
    /// [`DRAIN_WORKERS`]: super::DRAIN_WORKERS
    /// [`Db::claim_next_due_op`]: pdfs_core::db::Db::claim_next_due_op
    pub(crate) fn run_pending_drain(&self, primary: bool) {
        loop {
            // Between ops, never inside one: an op that has started is either
            // retired or released, so stopping here cannot leave the queue with
            // a row claimed by a worker that no longer exists (bugs.md B44).
            if self.shutdown.is_stopping() {
                debug!(primary, "drain worker stopping");
                return;
            }
            let now = now_millis();
            // Offline or paused, nothing is claimed. A claimed create may be on
            // the wire, so trashing its folder kept it as a trash; claimed
            // only to be handed back, it was kept although nothing had sent
            // it (`docs/BUGS.md` B178).
            let online = self.online.load(Ordering::Relaxed);
            let paused = self.sync_paused();
            // One row, chosen and *claimed* by the database. Reading the whole
            // queue to pick one op made a long queue quadratic to drain, and
            // held the shared connection — and so every FUSE metadata call —
            // for the duration.
            let due = match (online && !paused)
                .then(|| self.db.claim_next_due_op(now))
                .transpose()
            {
                Ok(due) => due.flatten(),
                Err(error) => {
                    error!(%error, "claiming the next pending operation failed");
                    self.wait_for_drain_work();
                    continue;
                }
            };
            let op = match due {
                Some(op) => op,
                None => {
                    if primary {
                        // Nothing to upload is exactly when the connection is
                        // free, so this is where the read path's buffered LRU
                        // touches get written (`ContentCache::flush_touches`).
                        // Cheap and a no-op when empty.
                        self.cache.flush_touches();
                        self.recover_fsynced_writes();
                        // Parked rows are invisible to `claim_next_due_op`, so
                        // an idle queue is exactly when a park that has outlived
                        // its writer has to be noticed.
                        self.sweep_parked_creates();
                    }
                    // Paused, nothing due changes that: sleep until a resume
                    // wakes the worker (or the idle poll notices a timed pause
                    // ran out), not until the next op falls due — that is
                    // "now" for everything already queued.
                    if paused {
                        self.wait_for_drain_work();
                        continue;
                    }
                    // A debounced or backed-off op may be waiting: sleep only
                    // until it becomes due rather than the full idle-poll.
                    self.wait_for_drain_work_or_due();
                    continue;
                }
            };
            let outcome = run_authorized_drain(
                &op,
                |uid| self.uid_write_authority(uid),
                || self.drain_op(&op),
            );
            if matches!(outcome, Ok(DrainDisposition::AccessDeferred)) {
                self.defer_for_access(&op, AccessDeferral::NotWritable);
                continue;
            }
            if let Ok(DrainDisposition::AuthorityUnknown(ref authority)) = outcome {
                self.resolve_unknown_authority(&op, authority);
                continue;
            }
            // The access check let this attempt through, so any earlier run of
            // deferrals is over and must not be counted against the next one.
            if matches!(outcome, Ok(DrainDisposition::Applied)) {
                if let Err(error) = self.db.clear_op_access_deferral(op.id) {
                    debug!(uid = %op.uid, %error, "clearing an access-deferral window failed");
                }
                // What landed may have made other ops claimable: the files
                // made in a folder that has its uid now, the next change to the
                // same node. This worker only takes one of them; the others
                // slept until the next idle poll.
                self.wake_drain();
            }
            if let Err(e) = outcome {
                let attempts = op.attempts + 1;
                let err_str = e.to_string();
                // Never infer that user data is disposable from an error string
                // or an arbitrary retry count. The staged blob and this row are
                // the only durable description of an accepted write. Even a
                // permission/quota error can become recoverable after the user
                // changes account state, and deleting the row here made the blob
                // unreachable after five ordinary network failures.
                //
                // A name a change of ours is about to free is a wait, not a
                // failure: the trash or rename holding it lands in moments.
                let held = waits_for_name(e.as_ref());
                let backoff = if held {
                    DRAIN_BACKOFF_MIN
                } else {
                    DRAIN_BACKOFF_MIN
                        .saturating_mul(1u32 << attempts.min(6))
                        .min(DRAIN_BACKOFF_MAX)
                };
                warn!(uid = %op.uid, attempts, error = %err_str, "pending upload failed; will retry");
                if let Err(e) = self.db.record_attempt_failure(
                    op.id,
                    &op.kind,
                    &err_str,
                    now_millis() + backoff.as_millis() as i64,
                ) {
                    error!(uid = %op.uid, error = %e, "recording a drain failure failed");
                    self.release_claim(&op);
                    self.wait_for_drain_work();
                    continue;
                }
                // What held the name wakes the ops waiting for it as it lets go
                // (`Db::wake_ops_waiting_for`). Letting go between our check and
                // the record above woke nothing, and the op sat out its backoff.
                // A create on the wire holds it where the database cannot see:
                // woken here, the op spent its waits in milliseconds and landed
                // under a conflict name (`docs/BUGS.md` B154).
                if held
                    && let Some(name) = op.name.as_deref()
                    && !self.creating_named(&op, name)
                    && let Err(e) = self.db.wake_ops_waiting_for(name)
                {
                    debug!(uid = %op.uid, error = %e, "waking the ops waiting for a name failed");
                }
                // A drain that cannot reach Drive has lost the link as surely as
                // a syscall that cannot. Unmarked, a short outage only the drain
                // met had no probe to see the link return, and the op sat out
                // its backoff long after (`docs/BUGS.md` B189).
                if let Some(proton) = proton_error(e.as_ref()) {
                    self.lost_link(proton, "drain");
                }
                // A failure to reach Drive says nothing about an earlier refusal,
                // so it keeps the issue that refusal recorded.
                if let Some(issue) = refusal(e.as_ref()) {
                    self.record_issue(&op, issue);
                }
                match self.db.retry_if_retargeted(
                    op.id,
                    op.parent_uid.as_deref(),
                    op.name.as_deref(),
                    op.blob_path.as_deref(),
                ) {
                    Ok(true) => {
                        debug!(uid = %op.uid, "create was renamed or rewritten while it failed; retrying")
                    }
                    Ok(false) => {}
                    Err(e) => {
                        debug!(uid = %op.uid, error = %e, "checking a failed create's target failed")
                    }
                }
            }
            // Unconditionally, on every path: a handler that retired its own row
            // leaves nothing to release, and one that returned without retiring
            // must not leave the row claimed by a worker that has moved on.
            self.release_claim(&op);
        }
    }

    /// Put an op the access check refused back on the clock, and decide whether
    /// its refusal is still news.
    ///
    /// Inside [`DRAIN_ACCESS_DEFER_LIMIT`] a deferral is what it always was: no
    /// attempt consumed, no error recorded, retried in five seconds. Past it the
    /// deferral is reported through [`Db::record_op_failure`] like any other
    /// stuck op, which is what puts it in `attempts`, in `last_error`, and — at
    /// [`FAILING_ATTEMPTS`] — in the `failing_ops` count and error text that
    /// `pdfs status` and the GUI show. The row and its staged blob stay exactly
    /// where they are either way; this changes only whether the user can see
    /// them.
    ///
    /// [`Db::record_op_failure`]: pdfs_core::db::Db::record_op_failure
    /// [`FAILING_ATTEMPTS`]: pdfs_core::db::FAILING_ATTEMPTS
    fn defer_for_access(&self, op: &PendingOp, reason: AccessDeferral) {
        let now = now_millis();
        let next_attempt_at = now + DRAIN_ACCESS_RECHECK.as_millis() as i64;
        let since = match self.db.defer_op_for_access(op.id, now, next_attempt_at) {
            Ok(since) => since,
            Err(error) => {
                error!(
                    uid = %op.uid,
                    %error,
                    "deferring access-blocked pending operation failed"
                );
                self.release_claim(op);
                self.wait_for_drain_work();
                return;
            }
        };
        let blocked = Duration::from_millis(now.saturating_sub(since).max(0) as u64);
        if since == now {
            // The first deferral of a run, at `warn!`: a write the mount accepted
            // is not going anywhere for now, and that is worth a line even when
            // the next recheck clears it.
            warn!(
                uid = %op.uid,
                kind = %op.kind,
                reason = reason.log_reason(),
                "pending operation deferred"
            );
        }
        if blocked >= DRAIN_ACCESS_DEFER_LIMIT {
            let attempts = op.attempts + 1;
            let backoff = DRAIN_BACKOFF_MIN
                .saturating_mul(1u32 << attempts.min(6))
                .min(DRAIN_BACKOFF_MAX);
            let error = format!(
                "{} for {}s. The staged bytes are kept.",
                reason.failure_reason(),
                blocked.as_secs()
            );
            warn!(uid = %op.uid, attempts, blocked_secs = blocked.as_secs(),
                  "pending operation has been access-blocked past the limit; reporting it");
            if let Err(error) =
                self.db
                    .record_op_failure(op.id, &error, now_millis() + backoff.as_millis() as i64)
            {
                error!(uid = %op.uid, %error, "recording an access-deferral failure failed");
            }
            if reason != AccessDeferral::RemoteUnreachable {
                self.record_issue(op, Some(SyncIssue::Access));
            }
        } else {
            debug!(
                uid = %op.uid,
                next_attempt_at,
                blocked_secs = blocked.as_secs(),
                reason = reason.log_reason(),
                "pending operation deferred until access is writable"
            );
        }
        self.release_claim(op);
    }

    /// Answer an authority the local tree has no row for by asking the remote,
    /// rather than deferring on a permission question nobody asked (B83).
    ///
    /// Three outcomes, and the point of the whole exercise is that they are
    /// three:
    ///
    /// * the node is still there — the tree simply lost it (a pruned subtree, a
    ///   mount whose registration went away). Re-intern it and the next pass has
    ///   a real access answer to work with.
    /// * the node is gone remotely. That is a permanent, *known* condition, so
    ///   it is recorded as a failure immediately instead of waiting out
    ///   [`DRAIN_ACCESS_DEFER_LIMIT`] to say so. The row and its staged blob are
    ///   kept, as on every other failure path — the bytes are still the user's,
    ///   and `pdfs recover` is how they get them back.
    /// * we could not ask. Ordinary deferral; the network is the network.
    fn resolve_unknown_authority(&self, op: &PendingOp, authority: &NodeUid) {
        let fetched = self.fetch_node_remote(authority);
        // The trash a withdrawn create left looks for the node it may have made
        // in the folder it was sent to. A folder trashed or gone took that node
        // with it, so there is nothing left to trash. Most often it is gone
        // because the user removed it with the file, and its trash landed first
        // (`docs/BUGS.md` B168).
        let folder_gone = match &fetched {
            Ok(None) => true,
            Ok(Some(node)) => node.trashed,
            Err(_) => false,
        };
        if op.kind == OP_TRASH
            && is_local_uid_str(&op.uid)
            && folder_gone
            && let Some(uid) = parse_node_uid(&op.uid)
        {
            debug!(uid = %op.uid, %authority, "a withdrawn create's folder is gone; trash op satisfied");
            if let Err(error) = self.retire_trash_op(op, &uid) {
                error!(uid = %op.uid, %error, "retiring a withdrawn create's trash failed");
                self.release_claim(op);
                return;
            }
            return;
        }
        match fetched {
            Ok(Some(node)) => {
                if let Err(error) = self.db.upsert_node(&node) {
                    error!(uid = %op.uid, %authority, %error,
                           "re-interning a queued operation's missing authority failed");
                } else {
                    info!(uid = %op.uid, %authority,
                          "re-interned an authority missing from the local tree; retrying");
                }
                // Deferred, not applied: the re-read has to go through the same
                // access check as everything else, on the next pass. The window
                // keeps running, so a refetch that never actually helps still
                // gets reported instead of looping in silence.
                self.defer_for_access(op, AccessDeferral::AuthorityRefetched);
            }
            Ok(None) => {
                let attempts = op.attempts + 1;
                let backoff = DRAIN_BACKOFF_MIN
                    .saturating_mul(1u32 << attempts.min(6))
                    .min(DRAIN_BACKOFF_MAX);
                let error = format!(
                    "{authority} no longer exists remotely, so this operation has nothing to \
                     apply to. The staged bytes are kept."
                );
                warn!(uid = %op.uid, %authority, attempts,
                      "a queued operation's authority is gone remotely; reporting it");
                if let Err(error) = self.db.record_op_failure(
                    op.id,
                    &error,
                    now_millis() + backoff.as_millis() as i64,
                ) {
                    error!(uid = %op.uid, %error, "recording a missing-authority failure failed");
                }
                self.record_issue(op, Some(SyncIssue::Missing));
                self.release_claim(op);
            }
            Err(error) => {
                debug!(uid = %op.uid, %authority, %error,
                       "could not ask the remote about a missing authority");
                self.defer_for_access(op, AccessDeferral::RemoteUnreachable);
            }
        }
    }

    /// Store why Drive refused `op`, or that its latest failure was no refusal.
    fn record_issue(&self, op: &PendingOp, issue: Option<SyncIssue>) {
        if let Err(error) = self.db.set_op_issue(op.id, issue.map(SyncIssue::as_str)) {
            error!(uid = %op.uid, %error, "recording a sync issue failed");
        }
    }

    /// Hand a claimed op back to the queue, logging rather than propagating —
    /// there is no caller left to propagate to, and the drain must not stop.
    fn release_claim(&self, op: &PendingOp) {
        if let Err(error) = self.db.release_op_claim(op.id) {
            error!(uid = %op.uid, %error, "releasing a drain claim failed");
        }
    }

    /// How long a freshly queued revision of `uid` should wait before a worker
    /// may pick it up.
    ///
    /// [`DRAIN_REVISION_DEBOUNCE`] exists for a tool that preallocates a file
    /// and then writes it, where two seconds is the right scale. It is the wrong
    /// scale for a large file on a slow link: an editor saving a 200 MB project
    /// every ten seconds queues a write, the drain starts sending it, the next
    /// save supersedes it mid-flight, and the file never finishes uploading
    /// while the user keeps working. Waiting roughly as long as the last upload
    /// of *this* node took moves that supersede into the queue, where replacing
    /// a row costs nothing.
    ///
    /// Never shorter than the fixed debounce and never longer than
    /// [`DRAIN_REVISION_DEBOUNCE_MAX`]: a node whose upload is genuinely slow
    /// still has to reach Drive, and a single pathological stall must not park
    /// it.
    ///
    /// [`DRAIN_REVISION_DEBOUNCE`]: super::DRAIN_REVISION_DEBOUNCE
    /// [`DRAIN_REVISION_DEBOUNCE_MAX`]: super::DRAIN_REVISION_DEBOUNCE_MAX
    pub(crate) fn revision_debounce(&self, uid: &NodeUid) -> Duration {
        let measured = self
            .upload_times
            .lock()
            .get(&uid.to_string())
            .map(|&(ms, _)| Duration::from_millis(ms));
        adaptive_debounce(measured)
    }

    /// Remember how long an upload of `uid` took, for the next write's
    /// debounce.
    ///
    /// Only a *completed* upload is a measurement. A cancelled or failed one
    /// says how long it ran before it stopped, which is not how long the file
    /// takes to send, and feeding that back would let one network fault widen
    /// the debounce for good.
    fn record_upload_time(&self, uid: &NodeUid, took: Duration) {
        let mut times = self.upload_times.lock();
        let now = now_millis();
        if times.len() >= UPLOAD_TIME_MEMORY
            && !times.contains_key(&uid.to_string())
            && let Some(oldest) = times
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
        {
            times.remove(&oldest);
        }
        times.insert(uid.to_string(), (took.as_millis() as u64, now));
    }

    /// Register a cancellation flag for an upload of `uid` that is about to
    /// start, returning a guard that deregisters it.
    ///
    /// Deregistration is by identity rather than by key: a supersede that
    /// arrives just as this upload finishes may already have registered the
    /// *next* upload's flag under the same uid, and removing that one would
    /// leave the newer upload uncancellable.
    fn begin_cancellable_upload(&self, uid: &NodeUid) -> UploadCancel {
        let flag = Arc::new(AtomicBool::new(false));
        self.upload_cancel.lock().insert(uid.clone(), flag.clone());
        UploadCancel {
            registry: self.upload_cancel.clone(),
            uid: uid.clone(),
            flag,
        }
    }

    /// Tell an upload of `uid` that is on the wire to stop, if there is one.
    ///
    /// Called when a newer revision of the file is queued. The upload does not
    /// stop instantly — it stops when the SDK next asks its reader for bytes,
    /// which for a 4 MiB block is soon enough to matter and late enough to be
    /// safe: the staged blob it is reading stays open until it lets go.
    pub(crate) fn cancel_upload(&self, uid: &NodeUid) {
        if let Some(flag) = self.upload_cancel.lock().get(uid) {
            flag.store(true, Ordering::Relaxed);
            debug!(%uid, "cancelling a superseded upload");
        }
    }

    /// Block until there is plausibly something to do: a new op, a reconnect, or
    /// the shortest outstanding backoff elapsing.
    pub(crate) fn wait_for_drain_work(&self) {
        sleep_for_drain_work(
            &self.drain_wake,
            || self.shutdown.is_stopping(),
            DRAIN_IDLE_POLL,
        );
    }

    /// Like [`wait_for_drain_work`](Self::wait_for_drain_work), but first
    /// checks whether a debounced or backed-off op is due before the idle-poll
    /// ceiling. A 2-second debounce must not sleep 30 seconds.
    fn wait_for_drain_work_or_due(&self) {
        let timeout = match self.db.earliest_due_at() {
            Ok(Some(ts)) => {
                let remaining = (ts - now_millis()).max(0) as u64;
                Duration::from_millis(remaining).min(DRAIN_IDLE_POLL)
            }
            _ => DRAIN_IDLE_POLL,
        };
        sleep_for_drain_work(&self.drain_wake, || self.shutdown.is_stopping(), timeout);
    }

    /// Perform one queued op and retire it.
    pub(crate) fn drain_op(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        self.finish_adoption(op)?;
        match op.kind.as_str() {
            OP_REVISION => self.drain_revision(op),
            OP_CREATE | OP_MKDIR => self.drain_local_node(op),
            OP_RENAME => self.drain_rename(op),
            OP_TRASH => self.drain_trash(op),
            other => Err(format!("unknown pending op kind {other:?}").into()),
        }
    }

    /// Finish adopting the node `op` is for, if its create landed but could
    /// not be read back.
    ///
    /// Until it is, the tree has the node as it was made locally, and a write
    /// queued over it is based on that rather than on what Drive holds. Sent
    /// so, the write found our own create's revision in its way and landed as
    /// a conflict copy (`docs/BUGS.md` B143).
    fn finish_adoption(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let Some(real) = parse_node_uid(&op.uid) else {
            return Ok(());
        };
        let local = match self.unadopted.lock().get(&real).cloned() {
            Some(local) => local,
            // The map does not outlive a restart, which also forgets the
            // stand-in: the row is under the real uid by then. A file as it was
            // made here has no revision of Drive's (`docs/BUGS.md` B182).
            None if op.kind == OP_REVISION && self.never_read_back(&real) => real.clone(),
            None => return Ok(()),
        };
        // A node gone since is the op's to deal with.
        if self.fetch_node_remote(&real)?.is_some() {
            self.adopt_real_uid(&local, &real, |_| {})?;
        }
        self.unadopted.lock().remove(&real);
        debug!(%local, %real, "adopted a landed create");
        Ok(())
    }

    /// Whether the tree has the file `real` as it was made here: its create
    /// landed, but Drive's answer was never read back.
    fn never_read_back(&self, real: &NodeUid) -> bool {
        let st = self.state();
        st.by_uid
            .get(real)
            .and_then(|ino| st.entries.get(ino))
            .is_some_and(|entry| !entry.node.is_folder() && node_revision_id(&entry.node).is_none())
    }

    /// Apply a queued rename/move to the remote.
    ///
    /// The op is the desired end state, so the remote's current state decides
    /// what actually has to be called: either half may already match (the event
    /// sync saw someone else do it, or an earlier attempt got half way through
    /// before failing). That also makes the whole thing idempotent, which a
    /// retrying queue needs.
    pub(crate) fn drain_rename(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let uid = parse_node_uid(&op.uid).ok_or("rename op has an unparseable uid")?;
        let parent_str = op.parent_uid.as_deref().ok_or("rename op has no parent")?;
        if is_local_uid_str(parent_str) {
            return Err(format!("destination {parent_str} has not been created yet").into());
        }
        let parent = parse_node_uid(parent_str).ok_or("rename op has an unparseable parent")?;
        let name = op.name.clone().ok_or("rename op has no name")?;

        // The node we were asked to rename may be gone or trashed by now. Either
        // way there is nothing to rename and nothing to lose — a rename holds no
        // bytes — so the op is satisfied rather than retried forever.
        let node = match self.fetch_node_remote(&uid)? {
            Some(n) if !n.trashed => n,
            _ => {
                warn!(%uid, name, "renamed node is gone or trashed remotely; dropping the rename");
                self.db.delete_op(op.id)?;
                return Ok(());
            }
        };
        let meta: Option<RenameMeta> = op
            .meta_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        if let Some(meta) = &meta
            && moved_elsewhere(meta, &node, &parent, &name)
        {
            // Someone moved or renamed it while ours was queued. Ours still
            // wins, as the last change does on Drive.
            info!(%uid, from = %node.name, to = %name,
                  "node moved remotely while its rename was queued");
        }
        let landed = if node.parent_uid.as_ref() == Some(&parent) {
            self.drain_rename_in_place(op, &uid, Some(&parent), &node.name, &name)?
        } else {
            // Move and rename land as one request (`move-multiple` takes a
            // target name), so there is no half-applied state between them and
            // no window for the requirements to go stale between two calls —
            // the race B46 queued this op to get away from.
            let target = (node.name != name).then_some(name.as_str());
            match self.move_rename_remote(&uid, &parent, target) {
                Ok(()) => name.clone(),
                // The destination holds that name already. Landing under a
                // *different* name is the non-destructive resolution: it neither
                // clobbers their file nor drops ours, and it is visible.
                Err(e) if is_already_exists(&e) => {
                    if self.name_is_held(op, Some(&parent), &name)? {
                        return Err(held_by_queued_change(&name));
                    }
                    // What held it may have let go since Drive answered
                    // (docs/BUGS.md B144).
                    match self.move_rename_remote(&uid, &parent, target) {
                        Ok(()) => name.clone(),
                        Err(e) if is_already_exists(&e) => {
                            let alt = conflict_name(&name, now_secs());
                            warn!(%uid, name, alt, "destination already holds that name; using a conflict name");
                            self.move_rename_remote(&uid, &parent, Some(&alt))?;
                            self.adopt_drained_name(&uid, &alt);
                            self.log_activity(
                                ActivityKind::Rename,
                                &name,
                                format!("destination already had that name; moved as {alt}"),
                                false,
                            );
                            alt
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                // The destination folder is gone. Leaving the node in its current
                // parent is the honest outcome: it is not where the user asked for
                // it, but it exists and it is where it has always been. The name
                // the user chose still applies there. Retrying the move could
                // only fail again — the folder is not coming back — and would
                // wedge the queue.
                Err(e) if is_gone(&e) => {
                    warn!(%uid, name, %parent, "move destination is gone; leaving the node where it is");
                    let landed = match self.drain_rename_in_place(
                        op,
                        &uid,
                        node.parent_uid.as_ref(),
                        &node.name,
                        &name,
                    ) {
                        Ok(landed) => landed,
                        // The node went with it; nothing is left to rename.
                        Err(e) if is_gone(e.as_ref()) => node.name.clone(),
                        Err(e) => return Err(e),
                    };
                    self.log_activity(
                        ActivityKind::Rename,
                        &landed,
                        "destination folder no longer exists; the file was left in place"
                            .to_string(),
                        false,
                    );
                    landed
                }
                Err(e) => return Err(e.into()),
            }
        };
        self.db.delete_op(op.id)?;
        // An op that wanted the name this node let go of waits no longer.
        if let Some(from) = meta.and_then(|m| m.original_name)
            && from != landed
        {
            self.db.wake_ops_waiting_for(&from)?;
            self.wake_drain();
        }
        info!(%uid, name = %landed, "pending rename landed");
        Ok(())
    }

    /// Rename `uid` without moving it, from `current` to `name`, and return the
    /// name it landed under. Someone may have taken the name while we were
    /// offline; the node then lands under a conflict name instead.
    fn drain_rename_in_place(
        &self,
        op: &PendingOp,
        uid: &NodeUid,
        parent: Option<&NodeUid>,
        current: &str,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        if current == name {
            return Ok(current.to_string());
        }
        match self.rename_remote(uid, name) {
            Ok(()) => Ok(name.to_string()),
            Err(e) if is_already_exists(&e) => {
                if self.name_is_held(op, parent, name)? {
                    return Err(held_by_queued_change(name));
                }
                // What held it may have let go since Drive answered
                // (docs/BUGS.md B144).
                match self.rename_remote(uid, name) {
                    Ok(()) => return Ok(name.to_string()),
                    Err(e) if is_already_exists(&e) => {}
                    Err(e) => return Err(e.into()),
                }
                let alt = conflict_name(name, now_secs());
                warn!(%uid, name, alt, "rename target name is taken; using a conflict name");
                self.rename_remote(uid, &alt)?;
                self.adopt_drained_name(uid, &alt);
                self.log_activity(
                    ActivityKind::Rename,
                    name,
                    format!("name was taken remotely; renamed to {alt}"),
                    false,
                );
                Ok(alt)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Apply a queued trash to the remote.
    ///
    /// A node that is already gone is a success, not a failure: the outcome the
    /// op asked for holds either way, and retrying forever against a node the
    /// server has forgotten would wedge the queue.
    pub(crate) fn drain_trash(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let uid = parse_node_uid(&op.uid).ok_or("trash op has an unparseable uid")?;
        let name = op.name.clone().unwrap_or_else(|| op.uid.clone());
        // The trash a delete left of a create that was sent, on the wire or
        // before, and failed (`Db::delete_ops_for_uid`). A create that failed
        // may still have made the node and lost only its answer, and the node
        // would keep the deleted file's name and bytes (docs/BUGS.md B151).
        if is_local_uid_str(&op.uid) {
            match self.withdrawn_twin(op)? {
                Some(twin) => {
                    self.hidden.lock().insert(twin.clone());
                    match self
                        .block_on_bounded(self.drive.trash_nodes(std::slice::from_ref(&twin)))
                        .and_then(batch::into_unit)
                    {
                        Ok(()) => {}
                        Err(e) if is_gone(&e) => {}
                        Err(e) => return Err(e.into()),
                    }
                    self.invalidate_trash();
                    info!(%uid, %twin, name, "a withdrawn create had landed unanswered; trashed it");
                }
                None => debug!(%uid, name, "withdrawn create never landed; trash op satisfied"),
            }
            self.retire_trash_op(op, &uid)?;
            return Ok(());
        }
        match self
            .block_on_bounded(self.drive.trash_nodes(std::slice::from_ref(&uid)))
            .and_then(batch::into_unit)
        {
            Ok(()) => {}
            Err(e) if is_gone(&e) => {
                debug!(%uid, name, "node was already gone remotely; trash op satisfied");
            }
            Err(e) => return Err(e.into()),
        }
        self.retire_trash_op(op, &uid)?;
        // A file trashed while open kept the bytes its handle reads, and a
        // final close while the trash was queued left them to this
        // (`docs/BUGS.md` B186).
        if !self.is_open_anywhere(&uid) {
            if let Some(blob) = self.pending_blob(&uid) {
                self.release_pending(&uid, &blob);
                self.cache.discard_staged(&blob);
            }
            self.cache.evict(&uid);
            self.evict_reader(&uid);
        }
        if let Err(e) = self.db.clear_own_sealed_rev(&uid.to_string()) {
            debug!(%uid, error = %e, "clearing the sealed-revision record failed");
        }
        self.invalidate_trash();
        self.log_activity(ActivityKind::Trash, &name, "trashed", true);
        info!(%uid, name, "pending trash landed");
        Ok(())
    }

    /// Remove a trash op that landed, and the blob of a create it withdrew.
    fn retire_trash_op(
        &self,
        op: &PendingOp,
        uid: &NodeUid,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(blob) = self.db.complete_trash_op(op.id, uid)? {
            self.cache.discard_staged(Path::new(&blob));
        }
        Ok(())
    }

    /// The node a create that a trash withdrew made on Drive although its
    /// answer was lost: in the create's folder under its name, or under one
    /// an attempt was sent with before a rename (`docs/BUGS.md` B184).
    fn withdrawn_twin(
        &self,
        op: &PendingOp,
    ) -> Result<Option<NodeUid>, Box<dyn std::error::Error>> {
        let mut targets: Vec<(String, String)> = op
            .parent_uid
            .clone()
            .zip(op.name.clone())
            .into_iter()
            .collect();
        for target in self.db.create_targets(op.id)? {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        for (parent, name) in targets {
            if is_local_uid_str(&parent) {
                continue;
            }
            let Some(parent) = parse_node_uid(&parent) else {
                continue;
            };
            if let Some(twin) = self.withdrawn_twin_in(op, &parent, &name)? {
                return Ok(Some(twin));
            }
        }
        Ok(None)
    }

    /// The node in `parent` under `name` a withdrawn create made, if it is one
    /// the create would have adopted had it been retried ([`adoptable`]). A
    /// node with a change of ours queued is one the user is still working on.
    fn withdrawn_twin_in(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<Option<NodeUid>, Box<dyn std::error::Error>> {
        let uids =
            match self.block_on_bounded(self.drive.enumerate_folder_children_node_uids(parent)) {
                Ok(uids) => uids,
                Err(e) if is_gone(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
        let children = self.block_on_bounded(self.drive.enumerate_nodes_light(&uids))?;
        let Some(twin) = children
            .into_iter()
            .find(|node| node.name == name && !node.trashed)
        else {
            return Ok(None);
        };
        let queued = twin.uid.to_string();
        for kind in [OP_RENAME, OP_REVISION, OP_TRASH] {
            if self.db.has_pending_op(&queued, kind)? {
                return Ok(None);
            }
        }
        let is_dir = match twin.kind {
            // A folder made by a create that never answered has nothing in
            // it: what was queued inside waited for its uid.
            NodeKind::Folder => {
                let inside = self
                    .block_on_bounded(self.drive.enumerate_folder_children_node_uids(&twin.uid))?;
                if !inside.is_empty() {
                    return Ok(None);
                }
                true
            }
            NodeKind::File { .. } => false,
        };
        let twin = match is_dir {
            true => twin,
            false => match self.fetch_node_remote(&twin.uid)? {
                Some(node) => node,
                None => return Ok(None),
            },
        };
        // A blob gone from staging matches nothing, and leaves the node be.
        let current = match op.blob_path.as_deref() {
            Some(blob) if node_size(&twin) > 0 => staged_sha1(Path::new(blob)).ok(),
            _ => None,
        };
        let sent = self.create_sent_digests(op.id, current);
        Ok(adoptable(is_dir, &twin, op.created_at, &sent).then_some(twin.uid))
    }

    /// Record the name the remote actually gave a node, after a conflict forced
    /// it away from the one the user asked for. Best effort: the event sync
    /// would correct the tree anyway, but not before the user has looked at it.
    pub(crate) fn adopt_drained_name(&self, uid: &NodeUid, name: &str) {
        self.for_each_state(|st| {
            let Some(&ino) = st.by_uid.get(uid) else {
                return;
            };
            let Some(entry) = st.entries.get_mut(&ino) else {
                return;
            };
            entry.node.name = name.to_string();
            let node = entry.node.clone();
            if let Err(e) = st.db.upsert_node(&node) {
                warn!(%uid, error = %e, "db upsert_node failed after a conflict rename");
            }
        });
    }

    /// Make a node that so far exists only on this machine real, and adopt the
    /// uid the server gives it (offline.md Phase 3b).
    pub(crate) fn drain_local_node(
        &self,
        op: &PendingOp,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Drive holds the name the create is sent with once it lands, whatever
        // became of the file meanwhile, until the rename or trash queued
        // behind it frees it (`Core::name_is_held`).
        if let (Some(parent), Some(name)) = (op.parent_uid.as_deref(), op.name.as_deref())
            && let Some(parent) = parse_node_uid(parent)
        {
            self.creating
                .lock()
                .insert(op.id, (parent, name.to_string()));
        }
        let created = self.create_local_node(op);
        self.creating.lock().remove(&op.id);
        created
    }

    fn create_local_node(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let local = parse_node_uid(&op.uid).ok_or("pending op has an unparseable uid")?;
        let parent_str = op.parent_uid.as_deref().ok_or("create op has no parent")?;
        // `run_pending_drain` will not offer an op whose parent is still a
        // placeholder, so reaching here with one is a bug, not a wait.
        if is_local_uid_str(parent_str) {
            return Err(format!("parent {parent_str} has not been created yet").into());
        }
        let parent = parse_node_uid(parent_str).ok_or("create op has an unparseable parent")?;
        let wanted = op.name.clone().ok_or("create op has no name")?;
        if self.db.has_withdrawn_create(parent_str, &wanted)? {
            return Err(held_by_queued_change(&wanted));
        }

        // Someone else may have taken the name while this sat in the queue —
        // reachable only because the op waited, which is the whole point of the
        // queue. Never overwrite theirs and never drop ours: land under a
        // conflict name, exactly as the sync engine does.
        //
        // Two things can hold the name that are not someone else's file. The
        // node a create made before the network dropped its answer is ours: the
        // create was queued because nobody heard back, so adopt it. And a node
        // whose trash is still queued is about to let go of it.
        let mut name = wanted.clone();
        let mut home = parent.clone();
        let mut sent = (parent_str.to_string(), wanted.clone());
        let mut blob_landed = true;
        // An earlier attempt, sent before the file was renamed or moved, may
        // have made it and lost the answer. That node is ours where it was
        // sent, and the rename follows it (`docs/BUGS.md` B184).
        let mut real = match self.earlier_twin(op, parent_str, &wanted)? {
            Some(EarlierTwin {
                parent: was_in,
                name: was_named,
                twin,
                holds_blob,
            }) => {
                info!(%local, %twin, was_named, wanted, "an earlier attempt landed unanswered under another name; adopting it");
                sent = (was_in.to_string(), was_named.clone());
                home = was_in;
                name = was_named;
                blob_landed = holds_blob;
                Ok(twin)
            }
            None => self.create_drained_node(op, &parent, &name),
        };
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref()))
            && let Some((twin, holds_blob)) = self.adoptable_twin(op, &parent, &wanted)?
        {
            info!(%local, %twin, wanted, "the name is held by our own unanswered create; adopting it");
            real = Ok(twin);
            blob_landed = holds_blob;
        }
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref()))
            && self.name_is_held(op, Some(&parent), &wanted)?
        {
            return Err(held_by_queued_change(&wanted));
        }
        // What held it may have let go since Drive answered (docs/BUGS.md
        // B144).
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
            real = self.create_drained_node(op, &parent, &wanted);
        }
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
            name = conflict_name(&wanted, now_secs());
            warn!(%local, wanted, name, "name is taken remotely; creating under a conflict name");
            real = self.create_drained_node(op, &parent, &name);
            if real.is_ok() {
                self.log_activity(
                    ActivityKind::Upload,
                    &wanted,
                    format!("name was taken remotely; created as {name}"),
                    false,
                );
            }
        }
        // The folder this node was created in may have been trashed remotely
        // while the op waited, in which case nothing will ever make the op
        // succeed as written and retrying it forever wedges the queue behind a
        // file that has bytes to save. Re-home it to the root: not where the
        // user put it, but it exists, it is visible, and the bytes are intact.
        //
        // Not for a create a delete withdrew while it was on the wire: the
        // folder is most often gone because the user removed it, with this
        // file, and the file would come back in the root (`docs/BUGS.md` B168).
        if let Some(root) = self.root_uid()
            && real.is_err()
            && !self.db.has_pending_op(&op.uid, OP_TRASH)?
            && self.parent_is_gone(&parent)
        {
            warn!(%local, name, "parent folder is gone remotely; creating in the root instead");
            real = self.create_drained_node(op, &root, &name);
            if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
                name = conflict_name(&wanted, now_secs());
                real = self.create_drained_node(op, &root, &name);
            }
            home = root;
            if real.is_ok() {
                self.log_activity(
                    ActivityKind::Upload,
                    &wanted,
                    format!("its folder was trashed remotely; created in the root as {name}"),
                    false,
                );
            }
        }
        let real = real?;
        // Retiring the op makes the ops behind it due: a child created in a new
        // folder, a rename made while the create was on the wire. Drive answers
        // a create before it lists the node, and those ops sent before then
        // found no folder or dropped the rename as gone. Best effort:
        // `adopt_real_uid` reads it back again and reports the failure.
        let _ = self.read_back(&real);

        // Retire the op before touching anything else: if we crash here the node
        // exists remotely and the local placeholder is reconciled by the event
        // sync, whereas a surviving op would create the file a second time.
        // An adopted node made empty has not had a blob this op carries
        // uploaded: it goes on as a revision of the node instead.
        let uploaded = blob_landed.then_some(op.blob_path.as_deref()).flatten();
        let landing = CreateLanding {
            local: &local.to_string(),
            real: &real.to_string(),
            sent: (&sent.0, &sent.1),
            landed: (&home.to_string(), &name),
        };
        let newer = match self.retire_create(op, uploaded, &landing, &local, &real)? {
            CreateRetired::Withdrawn { trash } => {
                return self.trash_withdrawn_create(op, trash, &local, &real, &name);
            }
            retired => matches!(retired, CreateRetired::Newer { .. }),
        };
        // The uploaded blob is the new file's content, so it becomes the cached
        // content, as for a revision. A partial write fills its gaps from it;
        // without it the next write open was refused (`docs/BUGS.md` B123).
        let landed: Option<StagedWrite> = uploaded
            .filter(|_| !newer)
            .and(op.meta_json.as_deref())
            .and_then(|json| serde_json::from_str(json).ok());
        // The op is gone, so the create has landed whatever the read-back
        // says. One that fails is finished before anything else is sent for
        // the node (`docs/BUGS.md` B143).
        if let Err(e) = self.adopt_real_uid(&local, &real, |node| {
            if let (Some(blob), Some(meta)) = (uploaded, &landed)
                && node_size(node) == meta.len
                && keeps_landed_upload(self.cache.is_pinned(&real), meta.len, self.cache.budget())
            {
                let _ =
                    self.cache
                        .store_file(&real, node.modification_time, meta.len, Path::new(blob));
            }
        }) {
            warn!(%local, %real, error = %e, "reading back a landed create failed; adopting it later");
            self.unadopted.lock().insert(real.clone(), local.clone());
        }
        // The feed will report this create back to us; the tree already has it
        // under its real uid, so that event is ours to ignore (`Core::self_changes`).
        self.note_self_change(&real);
        // The uploaded blob is done with either way: a write that replaced it
        // while it was on the wire already discarded it when it attached.
        if let Some(blob) = uploaded {
            self.cache.discard_staged(Path::new(blob));
        }
        if newer {
            info!(%local, %real, name, "a write landed during the create; queued it as a revision");
            self.wake_drain();
        }
        self.log_activity(ActivityKind::Upload, &name, "created", true);
        info!(%local, %real, name, kind = %op.kind, "pending create landed");
        Ok(())
    }

    /// Whether `name` is held on Drive by a change of ours that is to free it:
    /// a queued trash, a queued rename away from it, or another create on the
    /// wire that was sent with it in `parent`, which a rename or trash follows
    /// (`Db::finish_create`). The op that wants the name waits for it rather
    /// than land under a conflict name.
    ///
    /// A trash is waited for as long as it takes. The rest only for
    /// [`NAME_HOLD_ATTEMPTS`]: two files swapping names each hold the name the
    /// other wants.
    fn name_is_held(
        &self,
        op: &PendingOp,
        parent: Option<&NodeUid>,
        name: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if self.db.has_pending_trash_named(name)? {
            return Ok(true);
        }
        if op.attempts >= NAME_HOLD_ATTEMPTS {
            return Ok(false);
        }
        let creating = self
            .creating
            .lock()
            .iter()
            .any(|(id, (p, n))| *id != op.id && Some(p) == parent && n == name);
        Ok(creating || self.db.has_pending_move_from(name)?)
    }

    /// Whether a create other than `op` is on the wire under `name`, in any
    /// folder.
    fn creating_named(&self, op: &PendingOp, name: &str) -> bool {
        self.creating
            .lock()
            .iter()
            .any(|(id, (_, n))| *id != op.id && n == name)
    }

    /// Trash the node a create made after the file was deleted, or replaced by
    /// a rename, while its upload was on the wire (`docs/BUGS.md` B129).
    ///
    /// `Db::finish_create` queued the trash, claimed, in the transaction that
    /// found the create gone. It goes out now rather than behind the queue:
    /// until it lands the node holds its name on Drive, and a file created
    /// there again forks a conflict copy. A failure leaves it queued.
    fn trash_withdrawn_create(
        &self,
        op: &PendingOp,
        trash: i64,
        local: &NodeUid,
        real: &NodeUid,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.hidden.lock().insert(real.clone());
        self.note_self_change(real);
        let trash = PendingOp {
            id: trash,
            kind: OP_TRASH.to_string(),
            uid: real.to_string(),
            parent_uid: None,
            name: Some(name.to_string()),
            blob_path: None,
            meta_json: None,
            created_at: op.created_at,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };
        info!(%local, %real, name, "a create landed after its file was deleted; trashing it");
        if let Err(e) = self.drain_trash(&trash) {
            warn!(%real, name, error = %e, "trashing a withdrawn create failed; queued");
            if let Err(e) = self.db.record_op_failure(
                trash.id,
                &e.to_string(),
                now_millis() + DRAIN_BACKOFF_MIN.as_millis() as i64,
            ) {
                error!(%real, error = %e, "recording a drain failure failed");
            }
            self.release_claim(&trash);
            self.wake_drain();
        }
        Ok(())
    }

    /// The node an earlier attempt at create `op` made under a parent or name
    /// the op has been moved from since ([`Core::adoptable_twin`]).
    fn earlier_twin(
        &self,
        op: &PendingOp,
        parent: &str,
        name: &str,
    ) -> Result<Option<EarlierTwin>, Box<dyn std::error::Error>> {
        for (was_in, was_named) in self.db.create_targets(op.id)? {
            if (was_in.as_str(), was_named.as_str()) == (parent, name) {
                continue;
            }
            let Some(was_in) = parse_node_uid(&was_in) else {
                continue;
            };
            match self.adoptable_twin(op, &was_in, &was_named) {
                Ok(Some((twin, holds_blob))) => {
                    return Ok(Some(EarlierTwin {
                        parent: was_in,
                        name: was_named,
                        twin,
                        holds_blob,
                    }));
                }
                Ok(None) => {}
                Err(e) if is_gone(e.as_ref()) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// The node already holding `name` under `parent`, if it is the one an
    /// earlier attempt at this op made before its answer was lost, and whether
    /// it holds the op's blob already.
    ///
    /// A create that timed out may still have landed; the op was queued because
    /// the caller never heard back. Without this, its replay forked a
    /// `(sync-conflict)` copy of a file nobody else had touched.
    fn adoptable_twin(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<Option<(NodeUid, bool)>, Box<dyn std::error::Error>> {
        // A second file queued under the same name, after the first was
        // renamed away, finds the first one's node fresh and empty while its
        // create is on the wire. Taking it put both files on one node, and the
        // first one's rename took the second one's bytes along
        // (docs/BUGS.md B147).
        let racing = self
            .creating
            .lock()
            .iter()
            .any(|(id, (p, n))| *id != op.id && p == parent && n == name);
        if racing {
            return Ok(None);
        }
        let uids = self.block_on_bounded(self.drive.enumerate_folder_children_node_uids(parent))?;
        let children = self.block_on_bounded(self.drive.enumerate_nodes_light(&uids))?;
        // A node this mount trashed is not one an unanswered create made, even
        // while its trash is on the wire or a listing still shows it. Taking a
        // folder removed and made again put the new one's contents into the
        // old one, and its trash took them along (docs/BUGS.md B162).
        let hidden = self.hidden.lock().clone();
        let Some(twin) = children
            .into_iter()
            .find(|node| node.name == name && !node.trashed && !hidden.contains(&node.uid))
        else {
            return Ok(None);
        };
        // A node in the tree was answered for, by this mount or by a listing:
        // the same race after the first create landed. Without the adoption,
        // an unanswered twin forks a conflict copy, which loses nothing.
        if self.state().by_uid.contains_key(&twin.uid) {
            return Ok(None);
        }
        // The light listing carries no file size or digest.
        let twin = match twin.kind {
            NodeKind::Folder => twin,
            NodeKind::File { .. } => match self.fetch_node_remote(&twin.uid)? {
                Some(node) => node,
                None => return Ok(None),
            },
        };
        // A create uploads its bytes in the same call, so the node a lost
        // answer leaves holds them (docs/BUGS.md B127): the ones the op holds
        // now, or ones a write has replaced since (B156), which still go up.
        let current = match op.blob_path.as_deref() {
            Some(blob) if node_size(&twin) > 0 => Some(staged_sha1(Path::new(blob))?),
            _ => None,
        };
        let holds_blob = current
            .as_deref()
            .is_some_and(|sha| holds_bytes(&twin, sha));
        let sent = self.create_sent_digests(op.id, current);
        Ok(adoptable(op.kind == OP_MKDIR, &twin, op.created_at, &sent)
            .then_some((twin.uid, holds_blob)))
    }

    /// The SHA-1s of the bytes queued create `id` may have made its file with:
    /// `current`, those of the blob it holds now, and those each earlier
    /// attempt uploaded.
    fn create_sent_digests(&self, id: i64, current: Option<String>) -> Vec<String> {
        let mut sent = self.db.create_sent(id).unwrap_or_else(|e| {
            warn!(id, error = %e, "reading what a queued create sent failed");
            Vec::new()
        });
        sent.extend(current);
        sent
    }

    /// Make one queued `create`/`mkdir` real under a given name, and hand back
    /// the API's own error so the caller can tell a name clash from a failure.
    ///
    /// Split out because the conflict path has to run it twice: the second time
    /// under a different name.
    pub(crate) fn create_drained_node(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<NodeUid, Box<dyn std::error::Error>> {
        self.db
            .note_create_target(op.id, &parent.to_string(), name)?;
        match op.kind == OP_MKDIR {
            true => Ok(self.block_on_bounded(self.drive.create_folder(
                parent,
                name,
                Some(now_secs()),
            ))?),
            false => self.upload_created_file(op, parent, name),
        }
    }

    /// Upload the bytes a queued create accumulated, if any. A file that was
    /// created but never written (`touch`) has no blob and uploads as empty.
    pub(crate) fn upload_created_file(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<NodeUid, Box<dyn std::error::Error>> {
        let Some(blob) = op.blob_path.as_deref() else {
            return Ok(self.block_on_bounded(self.drive.upload_file(
                parent,
                name,
                media_type_for(name),
                b"",
            ))?);
        };
        let meta: StagedWrite = serde_json::from_str(op.meta_json.as_deref().unwrap_or(""))?;
        // An incomplete blob would be authored bytes over zeros, and there is no
        // base to repair it from — the file has never existed remotely. Refusing
        // to queue that is `queue_revision`'s job, so reaching here means the blob
        // is whole.
        if !meta.complete {
            return Err("queued create holds an incomplete blob".into());
        }
        // The call may make the file and lose its answer, and the op may hold
        // a newer blob by the time it is retried (`Core::adoptable_twin`).
        let sha = staged_sha1(Path::new(blob))?;
        self.db.note_create_sent(op.id, &sha)?;
        let thumbnails = self.upload_thumbnails(Path::new(blob), name);
        let guard = self
            .transfers
            .begin(name, op.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(blob)?, &guard);
        let uid = self.block_on_within(
            link::upload_deadline(meta.len),
            self.drive.upload_file_from(
                parent,
                name,
                media_type_for(name),
                reader,
                meta.len as i64,
                thumbnails,
                None,
                false,
            ),
        )?;
        Ok(uid)
    }

    /// Retire a landed create's op and move its queued state to the real uid,
    /// unless the create was withdrawn while it was on the wire.
    ///
    /// Runs under the `pending` lock, which `Core::enqueue_staged_write` also
    /// holds from attaching a blob to a create until it records the pending
    /// entry. So a write either attached before this ran and is carried over,
    /// or finds the create gone and queues against the real uid itself. The
    /// open write handles are repointed right after, for the same reason:
    /// a handle released later must not queue against the placeholder.
    fn retire_create(
        &self,
        op: &PendingOp,
        uploaded: Option<&str>,
        landing: &CreateLanding<'_>,
        local: &NodeUid,
        real: &NodeUid,
    ) -> Result<CreateRetired, Box<dyn std::error::Error>> {
        let retired = {
            let mut pending = self.pending.lock();
            let retired = self.db.finish_create(op.id, uploaded, landing, |json| {
                let mut meta: StagedWrite = serde_json::from_str(json).ok()?;
                meta.uid = landing.real.to_string();
                serde_json::to_string(&meta).ok()
            })?;
            // A withdrawn create's placeholder is the delete's to clean up, and
            // an open handle may still read its pending entry.
            if let CreateRetired::Withdrawn { .. } = retired {
                return Ok(retired);
            }
            pending.remove(local);
            if let CreateRetired::Newer { blob, meta } = &retired {
                let meta: StagedWrite = serde_json::from_str(meta)?;
                pending.insert(
                    real.clone(),
                    PendingRevision {
                        path: PathBuf::from(blob),
                        meta,
                    },
                );
            }
            retired
        };
        #[cfg(test)]
        self.drive.landing(real);
        if retired == CreateRetired::Landed
            && let Some(blob) = uploaded
        {
            self.cache_created(op, local, real, blob);
        }
        self.for_each_state(|st| {
            if let Some(ino) = st.by_uid.remove(local) {
                st.by_uid.insert(real.clone(), ino);
                // The node too: the next write of it upserts the row by the
                // node's uid, and would bring the placeholder row back.
                if let Some(e) = st.entries.get_mut(&ino) {
                    e.uid = real.clone();
                    e.node.uid = real.clone();
                }
                // Its children still name the placeholder as their parent, and
                // the next write of one would put it back on the row.
                for kid in st.children.get(&ino).cloned().unwrap_or_default() {
                    if let Some(e) = st.entries.get_mut(&kid)
                        && e.node.parent_uid.as_ref() == Some(local)
                    {
                        e.node.parent_uid = Some(real.clone());
                    }
                }
            }
            for aw in st.active_writes.values_mut() {
                if aw.uid == *local {
                    aw.uid = real.clone();
                }
            }
        });
        Ok(retired)
    }

    /// Cache a landed create's uploaded `blob` under `real`, keyed as the tree
    /// has the file.
    ///
    /// A write handle open on the file is repointed at `real` right after this,
    /// and a release fills its gaps from the cache by the size and mtime it
    /// opened over. Drive lists the new node a moment after the create, so
    /// `adopt_real_uid` caches the bytes only once it can read the node back.
    /// A release in between found nothing to fill from, and the next write open
    /// was refused (docs/BUGS.md B131).
    fn cache_created(&self, op: &PendingOp, local: &NodeUid, real: &NodeUid, blob: &str) {
        let Some(meta) = op
            .meta_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<StagedWrite>(json).ok())
        else {
            return;
        };
        let mut mtime = None;
        self.for_each_state(|st| {
            if let Some(entry) = st.by_uid.get(local).and_then(|ino| st.entries.get(ino)) {
                mtime = Some(entry.node.modification_time);
            }
        });
        if let Some(mtime) = mtime
            && keeps_landed_upload(self.cache.is_pinned(real), meta.len, self.cache.budget())
        {
            let _ = self
                .cache
                .store_file(real, mtime, meta.len, Path::new(blob));
        }
    }

    /// Swap a placeholder uid for the real one across everything that keyed off
    /// it: queued children, the DB, the in-memory tree, and the caches.
    ///
    /// The inode is deliberately kept, so anything already holding the file open
    /// keeps working across the drain.
    ///
    /// Applied to the tree every mount shares. A node created inside an
    /// on-demand sync folder once lived in that fork's own state, where the
    /// primary mount's drain never looked, and stayed on its `local~` uid —
    /// an empty file for as long as the daemon ran (`docs/BUGS.md` B74).
    ///
    /// `adopt` sees the node as Drive made it, before anything is rebased onto
    /// it.
    pub(crate) fn adopt_real_uid(
        &self,
        local: &NodeUid,
        real: &NodeUid,
        adopt: impl FnOnce(&Node),
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A rename made while the create was on the wire is queued against the
        // real uid (`Db::finish_create`). Until it lands, the tree keeps the
        // name and folder the user gave it. Asked before the read-back: a
        // rename that lands while Drive answers leaves no op behind, and the
        // answer may still have the old name (`docs/BUGS.md` B161).
        let real_key = real.to_string();
        let renaming = self.db.has_pending_op(&real_key, OP_RENAME)?;
        // Drive answers a create before it lists the new node.
        let mut node = self
            .read_back(real)
            .map_err(|e| self.errno_error(e, "fetch node"))?;
        // Before open handles are rebased onto the node, so one released in
        // between finds what `adopt` keeps (`docs/BUGS.md` B123).
        adopt(&node);
        // What Drive holds is the base open handles and queued writes are
        // rebased onto below. The tree shows a write queued over it, as a
        // listing does: a write open sized from the landed node over a shorter
        // queued blob left a gap that refused the next open (`docs/BUGS.md`
        // B123).
        let remote = node.clone();
        self.stamp_pending_sizes(std::slice::from_mut(&mut node));
        // The rows and queued ops already follow the node: `retire_create`
        // moved its row to the real uid, and ops find it by local id.

        self.for_each_state(|st| {
            if let Some(ino) = st.by_uid.remove(local) {
                st.by_uid.insert(real.clone(), ino);
            }
            let Some(&ino) = st.by_uid.get(real) else {
                st.write_through(node.clone());
                return;
            };
            // Asked again under the lock: `Core::queue_rename` queues its op
            // before it renames the entry under this lock. Asked only before,
            // a rename made since was overwritten with the name the create was
            // sent with, here and in the node row (`docs/BUGS.md` B152).
            let renaming =
                renaming || matches!(self.db.has_pending_op(&real_key, OP_RENAME), Ok(true));
            if renaming && let Some(e) = st.entries.get(&ino) {
                node.name = e.node.name.clone();
                node.parent_uid = e.node.parent_uid.clone();
            }
            st.write_through(node.clone());
            let mut landed = node.clone();
            st.keep_open_write_size(ino, &mut landed);
            if let Some(e) = st.entries.get_mut(&ino) {
                e.uid = real.clone();
                e.node = landed;
            }
            // Where the op said the node goes and where it actually landed can
            // differ — a conflict re-homes it — so the tree follows the parent
            // the server reports rather than the one we asked for.
            if let Some(parent) = node.parent_uid.clone()
                && let Some(&pino) = st.by_uid.get(&parent)
                && st.entries.get(&ino).is_some_and(|e| e.parent != pino)
            {
                let old = st.entries.get(&ino).map(|e| e.parent);
                if let Some(old) = old
                    && let Some(kids) = st.children.get_mut(&old)
                {
                    kids.retain(|&k| k != ino);
                }
                if let Some(e) = st.entries.get_mut(&ino) {
                    e.parent = pino;
                }
                if let Some(kids) = st.children.get_mut(&pino)
                    && !kids.contains(&ino)
                {
                    kids.push(ino);
                }
            }
        });
        // A handle still open on the placeholder was based on no revision at
        // all: a local-clock mtime and no revision id. Released as it is, its
        // write would land as a conflict copy of the empty file this create just
        // made (`docs/BUGS.md` B113). Rebased before the restamp below, so a
        // release that races this is caught there.
        //
        // A release that took its base before the rebase and queues after the
        // restamp is caught by neither. The revision this create sealed is
        // ours, as one a revision upload sealed is, so the drain chains that
        // write onto it instead of forking a conflict copy (B70 layer B).
        if let Some(rev) = node_revision_id(&remote)
            && let Err(e) = self
                .db
                .set_own_sealed_rev(&real.to_string(), &rev, now_millis())
        {
            warn!(%real, error = %e, "persisting our sealed revision failed");
        }
        self.rebase_open_writes(real, &remote);
        // A write carried over from the create was made against no revision at
        // all; the one just created is what it now replaces.
        self.rebaseline_pending(real, &remote);
        if node.is_folder() {
            // It was recorded as listed while local (it was empty and had nothing
            // to enumerate). That still holds: its queued children re-intern under
            // the real uid as they drain.
            if let Err(e) = self.db.set_listed(real, true) {
                warn!(%real, error = %e, "db set_listed(true) failed after remap");
            }
        }
        Ok(())
    }

    /// Why a queued write must not be applied to its node, or `None` when it
    /// still can be.
    ///
    /// A queued write is an edit of a specific revision. Time passes before it
    /// drains — indefinitely, if that is how long the network is gone — and in
    /// that window the node can be rewritten by another device, trashed, or
    /// deleted outright. Sending the blob anyway would silently drop whatever
    /// happened in between, which is exactly the thing the sync engine refuses
    /// to do (offline.md Phase 3b).
    ///
    /// Only checkable against a recorded baseline: a write staged before
    /// [`StagedWrite::based_on`] existed, or one against a node that has never
    /// existed remotely, has nothing to compare and is applied as before.
    pub(crate) fn revision_conflict(
        &self,
        uid: &NodeUid,
        meta: &StagedWrite,
        blob: &Path,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let Some(ref base) = meta.based_on else {
            return Ok(None);
        };
        let Some(node) = self.fetch_node_remote(uid)? else {
            return Ok(Some("the file no longer exists remotely".into()));
        };
        if node.trashed {
            return Ok(Some("the file was trashed remotely".into()));
        }
        let Some(reason) = revision_changed(base, &node) else {
            return Ok(None);
        };
        // The remote moved on from the baseline — normally a conflict. But if it
        // sits at a revision *this daemon* sealed, no other device changed the
        // file: it is a single-writer stall→resume (a browser that closed and
        // reopened its download fd; B70 layer B). Chain onto it — supersede
        // instead of forking — so the finished bytes win the name rather than
        // being exiled to a `(sync-conflict)` copy.
        let remote_rev = node_revision_id(&node);
        // The table, so the answer survives a restart (`own_sealed_rev`).
        let own_rev = self
            .db
            .own_sealed_rev(&uid.to_string(), now_millis())
            .unwrap_or_default();
        if is_own_self_supersede(meta.complete, remote_rev.as_deref(), own_rev.as_deref()) {
            debug!(%uid, ?remote_rev,
                   "queued write chains onto our own sealed revision; not a conflict");
            return Ok(None);
        }
        // An earlier attempt at this write may have landed with its answer
        // lost: the remote then holds these very bytes in a revision we never
        // heard of. Forked, the write was a conflict copy of itself
        // (docs/BUGS.md B155); sent again, it is a revision of the same bytes.
        if meta.complete
            && node_size(&node) == meta.len
            && let NodeKind::File {
                content_sha1: Some(remote),
                ..
            } = &node.kind
            && remote.eq_ignore_ascii_case(&staged_sha1(blob)?)
        {
            debug!(%uid, ?remote_rev, "the remote already holds the queued write; not a conflict");
            return Ok(None);
        }
        // An upload of ours landed and its read-back went unanswered, so its
        // revision id was never recorded as ours. The bytes it sent say the
        // same: the remote holds them, so no other device wrote since
        // (docs/BUGS.md B157).
        let sealed = self.sealed_unread.lock().get(uid).cloned();
        if meta.complete
            && let Some(sealed) = sealed
            && let NodeKind::File {
                content_sha1: Some(remote),
                ..
            } = &node.kind
            && remote.eq_ignore_ascii_case(&sealed)
        {
            debug!(%uid, ?remote_rev, "queued write chains onto our own unread revision; not a conflict");
            return Ok(None);
        }
        Ok(Some(reason))
    }

    /// Land a queued write that can no longer be applied to its own node as a
    /// *new* file beside it, and retire the op.
    ///
    /// The non-destructive resolution, and the same one the sync engine reaches
    /// for: the remote keeps whatever it has, the user keeps their bytes, and
    /// the name says which is which.
    ///
    /// An incomplete blob is gap-filled from whatever the node holds *now* —
    /// mixing revisions, which is only defensible because the result is
    /// explicitly a conflict copy rather than anyone's file. When even that is
    /// impossible the op is dropped but the staged bytes are deliberately left
    /// on disk: unreachable through the mount, but not destroyed, and the
    /// activity log says where they are.
    pub(crate) fn keep_as_conflict_copy(
        &self,
        op: &PendingOp,
        blob: &Path,
        meta: &StagedWrite,
        uid: &NodeUid,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let place = self.node_place(uid);
        let name = place
            .as_ref()
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| self.node_name(uid));
        // A node the tree has forgotten still has bytes worth keeping, so the
        // copy falls back to the root rather than being abandoned. So does one
        // whose folder went with it: an upload into a folder Drive no longer
        // has fails on every retry (B150).
        let folder = match place.map(|(p, _)| p) {
            Some(p) if self.fetch_node_remote(&p)?.is_some_and(|n| !n.trashed) => Some(p),
            _ => None,
        };
        let Some(parent) = folder.or_else(|| self.root_uid()) else {
            return self.abandon_to_staging(op, blob, uid, &name, reason);
        };
        warn!(%uid, name, reason, "queued write conflicts; keeping a conflict copy");

        if !meta.complete {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(blob)?;
            let mut written = Intervals::default();
            for &(s, e) in &meta.authored {
                written.add(s, e);
            }
            if let Err(e) = self.fill_gaps(
                uid,
                &file,
                meta.len,
                meta.base_mtime,
                meta.base_size,
                &written,
            ) {
                error!(%uid, name, error = ?e, "cannot complete a conflicted partial write");
                return self.abandon_to_staging(op, blob, uid, &name, reason);
            }
        }

        let alt = conflict_name(&name, now_secs());
        let thumbnails = self.upload_thumbnails(blob, &alt);
        let guard =
            self.transfers
                .begin(&alt, meta.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(blob)?, &guard);
        self.block_on_within(
            link::upload_deadline(meta.len),
            self.drive.upload_file_from(
                &parent,
                &alt,
                media_type_for(&alt),
                reader,
                meta.len as i64,
                thumbnails,
                None,
                false,
            ),
        )?;
        drop(guard);

        self.db.delete_op(op.id)?;
        // Dropping the pending entry hands the node back to the remote's truth:
        // reads stop coming from the staged blob, and the event sync stops
        // skipping it as "ahead of the server" (offline.md Phase 3a). Only if it
        // is still *this* write's entry — see [`Core::release_pending`].
        self.release_pending(uid, blob);
        self.cache.discard_staged(blob);
        self.cache.evict(uid);
        self.evict_reader(uid);
        // The conflict copy is a node the tree has never seen.
        self.for_each_state(|st| {
            if let Some(&ino) = st.by_uid.get(&parent) {
                st.invalidate_listing(ino);
            }
        });
        self.log_activity(
            ActivityKind::Upload,
            &name,
            format!("{reason}; local changes uploaded as {alt}"),
            false,
        );
        self.events.publish(&[
            pdfs_core::control::Topic::Files,
            pdfs_core::control::Topic::Conflicts,
        ]);
        info!(%uid, name, alt, "queued write landed as a conflict copy");
        Ok(())
    }

    /// Give up on placing a queued write anywhere the mount can see, without
    /// destroying it: the op goes (it could only fail forever) but the staged
    /// blob deliberately stays on disk, and the activity log says where.
    ///
    /// The last resort of the conflict path, and the same bargain
    /// [`Core::stage_orphaned_write`] strikes at the other end: bytes we cannot
    /// place are still bytes we do not get to delete.
    pub(crate) fn abandon_to_staging(
        &self,
        op: &PendingOp,
        blob: &Path,
        uid: &NodeUid,
        name: &str,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        error!(%uid, name, reason, staged = %blob.display(),
               "cannot place a conflicted write; bytes kept in staging");
        self.db.delete_op(op.id)?;
        self.release_pending(uid, blob);
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.log_activity(
            ActivityKind::Upload,
            name,
            format!("{reason}; local changes kept at {}", blob.display()),
            false,
        );
        Ok(())
    }

    /// Whether any mount still holds an open handle on `uid`.
    ///
    /// A transient file that is still open is still being written, however long
    /// it has been parked, and the rename that finishes it has not happened yet.
    fn uid_is_open(&self, uid: &NodeUid) -> bool {
        let mut open = false;
        self.for_each_state(|st| {
            if open {
                return;
            }
            open = st
                .by_uid
                .get(uid)
                .and_then(|ino| st.entries.get(ino))
                .is_some_and(|entry| entry.open_count > 0);
        });
        open
    }

    /// Retire parks that are never going to end (docs/BUGS.md B70).
    ///
    /// A parked create waits for one event: the rename from the transient name
    /// to the finished one. Nothing else ever un-parks it, and plenty of writers
    /// never perform it — an editor unlinks its swap file, a download is
    /// abandoned, or the file was always going to be called `index.tmp`. Such a
    /// row stayed at [`PARK_UNTIL`] for the life of the database, pinning the
    /// only copy of its bytes in `staging/` and counting against the queue.
    ///
    /// Two outcomes, and neither deletes bytes a user can still see:
    ///
    /// * The node is gone from the tree — the file was deleted while its create
    ///   was parked, so the create is no longer wanted. The op and its blob go,
    ///   which is what [`Core::discard_queued_ops`] would have done had the
    ///   delete path reached it.
    /// * The node is still there and has been parked past
    ///   [`PARK_EXPIRY_MS`] with nothing holding it open — the park has outlived
    ///   the writer it was protecting. Un-park it, and the ordinary drain
    ///   uploads it like any other create.
    ///
    /// An open node is left alone whatever its age: it is still being written.
    fn sweep_parked_creates(&self) {
        let parked = match self.db.parked_create_ops() {
            Ok(parked) if !parked.is_empty() => parked,
            Ok(_) => return,
            Err(error) => {
                warn!(%error, "reading parked creates failed; skipping the park sweep");
                return;
            }
        };
        let now = now_millis();
        let (mut released, mut dropped) = (0usize, 0usize);
        for op in &parked {
            let Some(uid) = parse_node_uid(&op.uid) else {
                continue;
            };
            let name = op.name.as_deref().unwrap_or("?");
            let listed = match self.db.node_by_uid(&op.uid) {
                Ok(known) => known.is_some(),
                // Never infer "deleted" from a failed read: that would discard
                // the bytes over a transient database error.
                Err(error) => {
                    debug!(%uid, %error, "cannot tell whether a parked node still exists");
                    continue;
                }
            };
            match park_verdict(op.created_at, now, self.uid_is_open(&uid), listed) {
                ParkVerdict::Keep => continue,
                ParkVerdict::Drop => {
                    if let Err(error) = self.discard_queued_ops(&uid) {
                        warn!(%uid, name, ?error, "dropping a deleted transient's parked create failed");
                        continue;
                    }
                    dropped += 1;
                    debug!(%uid, name, "dropped a parked create whose file was deleted");
                    continue;
                }
                ParkVerdict::Release => {}
            }
            match self.db.set_create_hold(&op.uid, false) {
                Ok(true) => {
                    released += 1;
                    info!(%uid, name, parked_ms = now - op.created_at,
                          "un-parked a transient create that was never renamed; uploading it");
                }
                Ok(false) => {}
                Err(error) => warn!(%uid, name, %error, "un-parking an expired park failed"),
            }
        }
        if released > 0 || dropped > 0 {
            info!(released, dropped, "swept parked creates");
        }
        if released > 0 {
            self.wake_drain();
        }
    }

    /// Drop `uid`'s pending entry, but only while it still names `blob`.
    ///
    /// A supersede can replace the entry while the op that owned it is in
    /// flight, so an unconditional `remove` retires a *newer* write's staged
    /// blob: reads fall back to a stale remote, the next `remote_baseline` forks
    /// a spurious `(sync-conflict)` copy of bytes that were never in conflict,
    /// and the incomplete-write interlock — which keys off the pending entry —
    /// is disarmed, letting a gap-fill read from a revision that no longer
    /// describes the file.
    pub(crate) fn release_pending(&self, uid: &NodeUid, blob: &Path) {
        let mut pending = self.pending.lock();
        if let Some(current) = pending.get(uid)
            && current.path == blob
        {
            pending.remove(uid);
        }
    }

    /// Whether a folder a queued op targets has stopped being a place a node can
    /// go. A failure to ask is not an answer: only a definite "trashed" or "not
    /// there" counts, so a network fault leaves the op to retry normally.
    pub(crate) fn parent_is_gone(&self, parent: &NodeUid) -> bool {
        match self.fetch_node_remote(parent) {
            Ok(Some(node)) => node.trashed,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// A node's parent uid and name, as the tree currently has them.
    pub(crate) fn node_place(&self, uid: &NodeUid) -> Option<(NodeUid, String)> {
        {
            let st = self.state();
            if let Some(entry) = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino))
                && let Some(parent) = entry.node.parent_uid.clone()
            {
                return Some((parent, entry.node.name.clone()));
            }
        }
        // The in-memory tree only holds what has been walked to since the daemon
        // started, and the drain routinely runs before anything has walked to
        // this file — after a restart, or for a write that arrived by path. The
        // DB knows it anyway. Without this fallback a conflict copy was named
        // after the node's *uid* and dumped in the root, which is how a file
        // called `G88km…==~c2do…== (sync-conflict 1784429627)` appears at the
        // top of someone's Drive.
        let node = self.db.node_by_uid(&uid.to_string()).ok().flatten()?;
        Some((node.parent_uid.clone()?, node.name.clone()))
    }

    /// The name to show a user for `uid`, for a transfer entry or an activity
    /// log line.
    ///
    /// Falls back to something short and human rather than to the uid: a uid is
    /// 130 characters of base64 that identifies the file to us and to nobody
    /// else, and it has ended up in both the activity log and on real files in
    /// the Drive root. The link prefix keeps it traceable without pretending to
    /// be a name.
    pub(crate) fn node_name(&self, uid: &NodeUid) -> String {
        self.node_place(uid).map(|(_, n)| n).unwrap_or_else(|| {
            let short: String = uid.link_id.to_string().chars().take(8).collect();
            format!("recovered-{short}")
        })
    }

    /// The uid of the My Files root, which every node in the mount descends
    /// from — the last resort for placing a file whose own parent is unknown.
    ///
    /// `None` only for a [`Core::fork_state`] sibling that has not interned its
    /// root yet, which is not where the drain runs; the drain must not panic
    /// over it regardless, since that would stop the queue for good.
    pub(crate) fn root_uid(&self) -> Option<NodeUid> {
        let root = self.root_ino();
        self.state().entries.get(&root).map(|e| e.uid.clone())
    }

    /// Upload a staged revision of a file the server already knows about.
    pub(crate) fn drain_revision(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let op_started = Instant::now();
        let blob = op
            .blob_path
            .clone()
            .ok_or("pending op has no staged blob")?;
        let blob = PathBuf::from(blob);
        let meta: StagedWrite = serde_json::from_str(op.meta_json.as_deref().unwrap_or(""))?;
        let uid = parse_node_uid(&meta.uid).ok_or("staged write has an unparseable uid")?;

        if let Some(reason) = self.revision_conflict(&uid, &meta, &blob)? {
            // Trashing the file from the mount drops its queued write, but not
            // from under a worker already holding it. That worker then finds the
            // node trashed — by us — and would upload the withdrawn bytes as a
            // conflict copy (a `recovered-…` file in the root when the tree has
            // already forgotten the node). A write nobody queues any more is
            // not a conflict.
            if !self.db.op_exists(op.id)? {
                debug!(%uid, reason, "queued write withdrawn while it drained; dropping it");
                return Ok(());
            }
            return self.keep_as_conflict_copy(op, &blob, &meta, &uid, &reason);
        }

        // An incomplete blob is authored bytes over zeros; the untouched ranges
        // have to be filled from the base before it can be sent. This is the case
        // the write could not resolve at release time (it was offline), and the
        // reason it is safe to do now is that we are not.
        if !meta.complete {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&blob)?;
            let mut written = Intervals::default();
            for &(s, e) in &meta.authored {
                written.add(s, e);
            }
            self.fill_gaps(
                &uid,
                &file,
                meta.len,
                meta.base_mtime,
                meta.base_size,
                &written,
            )
            .map_err(|e| self.errno_error(e, "gap-fill from base failed"))?;
        }

        let name = self.node_name(&uid);
        let thumbnails = self.upload_thumbnails(&blob, &name);
        let guard =
            self.transfers
                .begin(&name, meta.uid.clone(), TransferDirection::Upload, meta.len);
        // Registered before the first byte moves, so a write that lands while
        // this is on the wire can stop it rather than paying for a revision the
        // op it queued will immediately replace.
        let cancel = self.begin_cancellable_upload(&uid);
        // An unlink that withdrew this op before the flag above existed had no
        // upload to cancel; the row is the record that it happened.
        if !self.db.op_exists(op.id)? {
            debug!(%uid, "queued write withdrawn before it went out; dropping it");
            return Ok(());
        }
        let reader = CountingReader::new(File::open(&blob)?, &guard).with_cancel(cancel.flag());
        let started = Instant::now();
        let sent = self.block_on_within(
            link::upload_deadline(meta.len),
            self.drive
                .upload_new_revision_from(&uid, reader, meta.len as i64, thumbnails, None),
        );
        let took = started.elapsed();
        if let Err(e) = sent {
            if cancel.cancelled() {
                // Not a failure, and not this op's row to retire: the write that
                // cancelled us deleted it and took ownership of the staged blob
                // when it superseded this revision. Leaving the blob alone here
                // is the whole point — it is either already unlinked or it
                // belongs to the newer op.
                info!(%uid, "upload abandoned; a newer write superseded it");
                return Ok(());
            }
            return Err(e.into());
        }
        drop(cancel);
        drop(guard);
        // Only a completed upload times how long this file takes to send, which
        // is what the next write's debounce is scaled from.
        self.record_upload_time(&uid, took);

        // Still pending to whoever asks until the bytes are cached below, and
        // counted before the op goes so the status never reads empty in between.
        // An empty queue is how a script or the app knows a written file reads
        // without the network; retiring the op alone left the read-back's round
        // trip in which it did not (B119). The op itself has to go first: one
        // restored after a crash would re-send a revision this daemon has not
        // yet recorded as its own, and come back as a conflict copy.
        let _landing = LandingUpload::begin(&self.landing_uploads);
        // Retire the op before dropping the blob: a crash between the two leaves
        // an orphaned file (harmless), whereas the reverse would leave a queued
        // op pointing at nothing.
        self.db.delete_op(op.id)?;
        self.evict_reader(&uid);
        // The staged blob now matches the sealed revision, so it becomes the
        // file's cached content, stamped like the revision just read back.
        // Reading the file again then needs no network, which an editor or a
        // build does at once and a flaky link cannot always give (`docs/BUGS.md`
        // B119). Adopted by link, not by value: the blob is the whole file, and
        // a video read into a `Vec` here is an OOM. `discard_staged` below drops
        // the staging name only — the cache keeps the inode.
        self.refresh_after_upload(&uid, &blob, |mtime, size| {
            if size == meta.len
                && keeps_landed_upload(self.cache.is_pinned(&uid), meta.len, self.cache.budget())
            {
                let _ = self.cache.store_file(&uid, mtime, meta.len, &blob);
            }
        });
        self.cache.discard_staged(&blob);
        self.log_activity(ActivityKind::Upload, &name, "uploaded", true);
        // Both times, so a slow drain shows whether the upload itself or the
        // checks and gap-fill around it are where the time goes.
        info!(
            %uid,
            len = meta.len,
            upload_ms = took.as_millis() as u64,
            total_ms = op_started.elapsed().as_millis() as u64,
            "pending upload landed"
        );
        Ok(())
    }

    /// Adopt the server's metadata for a node we have just uploaded a revision
    /// for.
    ///
    /// [`State::record_pending_write`] deliberately stamps the node with the
    /// moment we *accepted* the write, so `ls` reflects it before the upload
    /// lands. The server stamps the sealed revision with its own time, and the
    /// two differ by however long the upload took. That difference is not
    /// cosmetic: [`Core::remote_baseline`] reads this node to build
    /// [`StagedWrite::based_on`], and [`Core::revision_conflict`] compares that
    /// baseline against the remote — so leaving our optimistic time in place
    /// makes the *next* write to this file look like another device changed it
    /// underneath us, and diverts it into a conflict copy over nothing.
    ///
    /// A write queued while this upload was on the wire is handled instead by
    /// [`Core::rebaseline_pending`]: its optimistic size/mtime must stay on the
    /// node (the same reason `apply_event` refuses to overwrite a node with a
    /// queued write), but its *baseline* still has to learn about the revision
    /// we just sealed. Skipping both, as this used to, is what produced a file
    /// that conflicted with itself and left a full-size duplicate behind.
    ///
    /// `blob` is the revision that landed, which stays the pending one until
    /// open write handles are rebased off it, and `adopt` gets the mtime and
    /// size of the node read back before that, so the uploaded bytes can be
    /// cached under its revision first. A handle released in between fills its
    /// gaps from one or the other; with the blob let go first and the cache
    /// filled last, it found neither, and the next write open was refused
    /// (docs/BUGS.md B123). When the node cannot be read back, `adopt` gets
    /// the stamp the tree still has, which is the base a write opened next
    /// takes: without it the blob went and nothing replaced it (B157). The
    /// blob's digest then stands in for the revision id the read-back did
    /// not bring, so the next write knows the revision as its own.
    ///
    /// Best effort: a failure here costs a spurious conflict copy on the next
    /// write, not this upload, which has already landed. Returns the node as
    /// the server now has it, or `None` when it could not be read back.
    pub(crate) fn refresh_after_upload(
        &self,
        uid: &NodeUid,
        blob: &Path,
        adopt: impl FnOnce(i64, u64),
    ) -> Option<Node> {
        // Asked before the read-back: the op went before it, so a rename queued
        // behind the upload can land while Drive answers, and the answer may
        // still have the old name (`docs/BUGS.md` B172).
        let placed = self.placement(uid);
        let renaming = self
            .db
            .has_pending_op(&uid.to_string(), OP_RENAME)
            .unwrap_or(false);
        let fetched = self.fetch_node_remote(uid);
        if fetched.is_ok() {
            self.sealed_unread.lock().remove(uid);
        }
        let node = match fetched {
            Ok(Some(node)) => node,
            // Trashed or deleted under us: the tree will hear it from the event
            // sync, which is better placed to unhook the inode than we are.
            Ok(None) => {
                self.release_pending(uid, blob);
                return None;
            }
            Err(e) => {
                warn!(%uid, error = %e,
                      "refreshing metadata after an upload failed; \
                       the next write to this file may conflict with itself");
                match staged_sha1(blob) {
                    Ok(sha) => {
                        self.sealed_unread.lock().insert(uid.clone(), sha);
                    }
                    Err(_) => {
                        self.sealed_unread.lock().remove(uid);
                    }
                }
                // No newer write took over, so the tree's stamp is this upload's.
                if self.pending_blob(uid).as_deref() == Some(blob) {
                    let mut stamp = None;
                    self.for_each_state(|st| {
                        if let Some(entry) = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino))
                        {
                            stamp = Some((entry.node.modification_time, node_size(&entry.node)));
                        }
                    });
                    if let Some((mtime, size)) = stamp {
                        adopt(mtime, size);
                    }
                }
                self.release_pending(uid, blob);
                return None;
            }
        };
        adopt(node.modification_time, node_size(&node));
        // This function's whole job is to bring the tree level with the revision
        // we just sealed, so the feed's report of that revision has nothing left
        // to tell us. Claimed here rather than at the upload call so it is only
        // recorded when the node was actually re-read (`Core::self_changes`).
        self.note_self_change(uid);
        // Rebase any *open* write handles targeting this node. A handle opened
        // before this upload carries a stale base — its `base_mtime`/`base_size`
        // name the revision we just replaced. Without this update,
        // `remote_baseline` will build a `based_on` from those stale values, the
        // drain will compare them against the revision we sealed, and the write
        // will be diverted into a conflict copy of itself.
        //
        // This is the open-handle counterpart of `rebaseline_pending`, which
        // covers the same gap for *queued* ops.
        {
            // Remember it as ours, so a queued write that opened over an earlier
            // revision recognises this as a self-supersede rather than a foreign
            // change and does not fork (B70 layer B; consulted in
            // `revision_conflict`).
            if let Some(rev) = node_revision_id(&node) {
                // In the table, not in memory: losing it at shutdown reopened
                // the fork window B70 layer B closed, for the first drain
                // after a restart.
                if let Err(e) = self
                    .db
                    .set_own_sealed_rev(&uid.to_string(), &rev, now_millis())
                {
                    warn!(%uid, error = %e, "persisting our sealed revision failed");
                }
            }
            // A write open from here until the tree has the node takes it as
            // its base: the blob it would have copied is let go of below, and
            // the tree's stamp names the revision this one replaced
            // (docs/BUGS.md B170).
            self.landed.lock().insert(uid.clone(), node.clone());
            self.rebase_open_writes(uid, &node);
        }
        self.release_pending(uid, blob);
        #[cfg(test)]
        self.drive.landing(uid);
        self.place_landed(uid, &node, placed, renaming);
        self.landed.lock().remove(uid);
        Some(node)
    }

    /// Put a node whose upload just landed into the tree, unless a newer write
    /// is queued for it. `before` is where the tree had the node and
    /// `renaming` whether a rename of it was queued, both from before the
    /// read-back.
    fn place_landed(
        &self,
        uid: &NodeUid,
        node: &Node,
        before: Option<(String, Option<NodeUid>)>,
        renaming: bool,
    ) {
        // Ordered so that a write queued *during* the fetch above is still
        // caught: it took its baseline from the node's optimistic stamp, and
        // this overwrites it with the revision the server actually holds.
        if self.rebaseline_pending(uid, node) {
            return;
        }
        // A rename queued behind the upload has not reached Drive yet, or
        // landed after Drive read the node back, and the tree keeps the name
        // and folder the user gave the file, as `adopt_real_uid` does. Taking
        // Drive's put the file back under its old name. A rename queued since
        // the read-back began has already moved the entry when the tree is
        // updated, so a name that changed in between is the user's too
        // (docs/BUGS.md B146, B172).
        let renaming = renaming
            || self
                .db
                .has_pending_op(&uid.to_string(), OP_RENAME)
                .unwrap_or(false);
        self.for_each_state(|st| {
            let moved = placed_in(st, uid) != before;
            let Some(entry) = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino)) else {
                return;
            };
            let parent = entry.parent;
            let mut node = node.clone();
            if renaming || moved {
                node.name = entry.node.name.clone();
                node.parent_uid = entry.node.parent_uid.clone();
            }
            st.intern(parent, node);
        });
    }

    /// Where the tree has `uid`: its name and parent.
    fn placement(&self, uid: &NodeUid) -> Option<(String, Option<NodeUid>)> {
        placed_in(&self.state(), uid)
    }

    /// Rebase every write handle open on `uid` onto `node`, a revision this
    /// daemon has just put there itself (`WriteHandle::rebase_onto`).
    fn rebase_open_writes(&self, uid: &NodeUid, node: &Node) {
        self.for_each_state(|st| {
            for aw in st.active_writes.values_mut() {
                if aw.uid == *uid {
                    aw.rebase_onto(node);
                    debug!(%uid, mtime = aw.base_mtime, size = aw.base_size,
                           "rebased open write handle onto the revision just landed");
                }
            }
        });
    }

    /// Point a still-queued write at the revision this upload just sealed, and
    /// report whether there was one.
    ///
    /// The revision we sent *is* the base the queued write will be applied over,
    /// but nothing else says so. [`Core::remote_baseline`] carries a baseline
    /// across a supersede because the op it inherits from "is the last one that
    /// actually observed the remote" — true only until that op drains. Once it
    /// has, the inherited baseline names a revision we replaced ourselves, and
    /// [`Core::revision_conflict`] reads that as another device having moved the
    /// file. It is a self-conflict, and it is expensive: the whole staged blob
    /// is re-uploaded as a second file.
    ///
    /// Both copies of the sidecar are updated — the in-memory one the next
    /// `release` inherits from, and the persisted one the drain reloads after a
    /// restart. Best effort on the DB half; the in-memory half is what the
    /// immediate next write reads.
    fn rebaseline_pending(&self, uid: &NodeUid, sealed: &Node) -> bool {
        let base = Baseline {
            mtime: sealed.modification_time,
            size: node_size(sealed),
            hash: None,
            revision_id: node_revision_id(sealed),
        };
        let mut pending = self.pending.lock();
        let Some(p) = pending.get_mut(uid) else {
            return false;
        };
        p.meta.based_on = Some(base.clone());
        match serde_json::to_string(&p.meta) {
            Ok(json) => {
                if let Err(e) = self.db.update_op_meta(&uid.to_string(), OP_REVISION, &json) {
                    warn!(%uid, error = %e,
                          "restamping a queued write's baseline failed; \
                           it may land as a conflict copy of itself");
                }
            }
            Err(e) => warn!(%uid, error = %e, "serializing a restamped sidecar failed"),
        }
        debug!(%uid, mtime = base.mtime, size = base.size,
               "queued write rebaselined onto the revision just uploaded");
        true
    }
}

/// Whether the remote `node` moved on from the revision a queued write was
/// `base`d on — the reason string when it did, `None` when the write can still be
/// applied. Pure so it is testable without a live remote (the trashed/gone checks
/// stay in [`Core::revision_conflict`], which does the fetch).
///
/// The revision id is the authoritative identity: it advances iff a *new* revision
/// was sealed. When both sides carry one, it alone decides — the server re-stamps
/// the *same* revision's mtime, and reading that drift as a change is exactly what
/// diverted a write into a conflict copy of its own base (B16/B25). Only when
/// there is no id to compare (an old sidecar, or a surface that omits it) does it
/// fall back to the observable `(mtime, size)` tuple.
fn revision_changed(base: &Baseline, node: &Node) -> Option<String> {
    if let (Some(base_rev), Some(remote_rev)) = (&base.revision_id, node_revision_id(node)) {
        return (*base_rev != remote_rev).then(|| {
            format!(
                "the remote revision changed under the queued write \
                 (based on revision {base_rev}, remote now at {remote_rev})"
            )
        });
    }
    let (mtime, size) = (node.modification_time, node_size(node));
    (mtime != base.mtime || size != base.size).then(|| {
        format!(
            "the remote revision changed under the queued write \
             (expected {} bytes at mtime {}, found {size} at {mtime})",
            base.size, base.mtime
        )
    })
}

/// Whether a queued write that found the remote moved on ([`revision_changed`]
/// fired) is nonetheless a single-writer self-supersede rather than a foreign
/// conflict — the remote sits at a revision this daemon sealed itself, so
/// chaining onto it is safe and forking would be wrong (B70 layer B).
///
/// `remote_rev` is the id the remote currently holds, `own_rev` this node's
/// latest own-sealed id. Requires a `complete` blob: an incomplete one's gaps
/// still refer to the stale base, so it must take the non-destructive
/// conflict-copy path instead of overwriting. A `None` remote id never matches
/// (there is nothing to prove it was ours), so absence falls through to a fork.
fn is_own_self_supersede(complete: bool, remote_rev: Option<&str>, own_rev: Option<&str>) -> bool {
    complete && remote_rev.is_some() && remote_rev == own_rev
}

/// Whether Drive has `node` neither where its queued rename found it nor where
/// the rename takes it (`parent`, `name`), so someone else moved or renamed it
/// in between. Unknown on a row without the original name.
fn moved_elsewhere(meta: &RenameMeta, node: &Node, parent: &NodeUid, name: &str) -> bool {
    let Some(original_name) = meta.original_name.as_deref() else {
        return false;
    };
    let at = |parent: &str, name: &str| {
        node.parent_uid.as_ref().map(ToString::to_string).as_deref() == Some(parent)
            && node.name == name
    };
    !at(&meta.original_parent_uid, original_name) && !at(&parent.to_string(), name)
}

/// The answer from Drive somewhere in `e`'s chain, if it got that far.
fn proton_error<'e>(e: &'e (dyn std::error::Error + 'static)) -> Option<&'e ProtonError> {
    std::iter::successors(Some(e), |e| e.source()).find_map(|e| e.downcast_ref::<ProtonError>())
}

/// What a failed attempt says about its op's sync issue: `None` when Drive
/// could not be reached, which says nothing; otherwise the issue Drive's answer
/// is, or `Some(None)` for a failure that is not a refusal.
fn refusal(e: &(dyn std::error::Error + 'static)) -> Option<Option<SyncIssue>> {
    let proton = proton_error(e)?;
    if is_network_error(proton) {
        return None;
    }
    let ProtonError::Api(api) = proton else {
        return Some(None);
    };
    Some(match api.code {
        ResponseCode::InsufficientQuota
        | ResponseCode::InsufficientSpace
        | ResponseCode::InsufficientVolumeQuota => Some(SyncIssue::Quota),
        ResponseCode::Forbidden | ResponseCode::NotEnoughPermissions => Some(SyncIssue::Access),
        ResponseCode::DoesNotExist => Some(SyncIssue::Missing),
        ResponseCode::TooManyChildren | ResponseCode::NestingTooDeep => Some(SyncIssue::Limit),
        ResponseCode::InvalidValue | ResponseCode::IncompatibleState => Some(SyncIssue::Rejected),
        _ => None,
    })
}

/// What the park sweep does with one parked create.
#[derive(Debug, PartialEq, Eq)]
enum ParkVerdict {
    /// Leave it parked: the rename it waits for can still come.
    Keep,
    /// Un-park it: nothing will rename it now, so the bytes go up as they are.
    Release,
    /// Drop it: the file was deleted, so the create is no longer wanted.
    Drop,
}

/// How long a parked create whose node is not `listed` is kept anyway. The
/// create is queued before its node is written, and a sweep in between took the
/// file for deleted (docs/BUGS.md B145).
const PARK_UNLISTED_GRACE_MS: i64 = 60 * 1000;

/// Judge one parked create by its age and whether its node is `listed`. An open
/// node is kept whatever its age — it is still being written, and the rename is
/// the writer's last step.
fn park_verdict(created_at: i64, now: i64, open: bool, listed: bool) -> ParkVerdict {
    let age = now - created_at;
    if open {
        ParkVerdict::Keep
    } else if !listed {
        if age < PARK_UNLISTED_GRACE_MS {
            ParkVerdict::Keep
        } else {
            ParkVerdict::Drop
        }
    } else if age < PARK_EXPIRY_MS {
        ParkVerdict::Keep
    } else {
        ParkVerdict::Release
    }
}

/// The retryable error for an op whose name a change of ours has yet to free
/// ([`Core::name_is_held`]).
fn held_by_queued_change(name: &str) -> Box<dyn std::error::Error> {
    Box::new(NameHeld(name.to_string()))
}

/// An op waits for a queued change of ours to free the name it wants.
#[derive(Debug)]
struct NameHeld(String);

impl std::fmt::Display for NameHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is still held by a node whose trash or rename is queued",
            self.0
        )
    }
}

impl std::error::Error for NameHeld {}

/// Whether a failed attempt only waited for a name ([`NameHeld`]).
fn waits_for_name(e: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(e), |e| e.source()).any(|e| e.is::<NameHeld>())
}

/// How many attempts an op waits for a rename or a create on the wire to free
/// the name it wants. Two files swapping names each wait for the other, so the
/// wait has to end; a conflict name ends it.
const NAME_HOLD_ATTEMPTS: i64 = 4;

/// How much older than its op a remote node may be and still be the one an
/// unanswered call made. The op is queued the moment the call gives up, so the
/// node is at most one call's deadline older; the rest is clock skew between
/// this machine and Proton.
const ADOPT_WINDOW_MS: i64 = 2 * 60 * 1000;

/// Whether `twin`, found holding the name a queued create wants, is the node an
/// earlier, unanswered attempt at that create made: the same kind, created
/// around when the op was queued, and for a file still empty, as the mount's
/// create leaves it, or holding exactly bytes the op uploaded, whose SHA-1 is
/// one of `sent`.
fn adoptable(is_dir: bool, twin: &Node, op_created_ms: i64, sent: &[String]) -> bool {
    if twin.creation_time.saturating_mul(1000) < op_created_ms - ADOPT_WINDOW_MS {
        return false;
    }
    match &twin.kind {
        NodeKind::Folder => is_dir,
        NodeKind::File { claimed_size, .. } => {
            !is_dir && (*claimed_size == Some(0) || sent.iter().any(|sha| holds_bytes(twin, sha)))
        }
    }
}

/// Whether `node` is a file whose content has SHA-1 `sha`.
fn holds_bytes(node: &Node, sha: &str) -> bool {
    matches!(&node.kind, NodeKind::File { content_sha1: Some(have), .. }
        if have.eq_ignore_ascii_case(sha))
}

/// The lowercase-hex SHA-1 of a staged blob, as Drive reports a file's.
fn staged_sha1(blob: &Path) -> Result<String, Box<dyn std::error::Error>> {
    Ok(sha1_of(&mut File::open(blob)?)?)
}

/// What share of the cache budget an unpinned upload may take and still be kept
/// as cached content once it lands: a sixteenth, 320 MiB of the default 5 GiB.
/// A larger file would push out much of what the user read lately, for bytes
/// they may never read back.
const LANDED_KEEP_SHARE: u64 = 16;

/// Whether an upload of `len` bytes that just landed stays as the file's cached
/// content. A pinned file always does, and so does any file when the budget is
/// unlimited (`0`).
fn keeps_landed_upload(pinned: bool, len: u64, budget: u64) -> bool {
    pinned || budget == 0 || len <= budget / LANDED_KEEP_SHARE
}

/// Where `st` has `uid`: its name and parent.
/// Sleep on the drain's `wake` for up to `timeout`, unless a wake-up came
/// since a worker last took one, or the mount is `stopping`.
///
/// One wake-up wakes every worker, and the first to wake takes it. A worker
/// that came to sleep after that slept out its timeout, the stop's included,
/// and the stop gave up on it after 10 s (`docs/BUGS.md` B180). The stop sets
/// its flag before it takes the lock, so a worker that finds it unset under
/// the lock is asleep when the wake-up comes.
fn sleep_for_drain_work(
    wake: &(Mutex<bool>, parking_lot::Condvar),
    stopping: impl Fn() -> bool,
    timeout: Duration,
) {
    let (lock, cv) = wake;
    let mut woken = lock.lock();
    if !*woken && !stopping() {
        cv.wait_for(&mut woken, timeout);
    }
    *woken = false;
}

fn placed_in(st: &crate::state::State, uid: &NodeUid) -> Option<(String, Option<NodeUid>)> {
    let entry = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino))?;
    Some((entry.node.name.clone(), entry.node.parent_uid.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WriteHandle;
    use pdfs_core::Access;
    use pdfs_core::db::Db;
    use proton_drive_rs::NodeKind;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};
    use std::cell::Cell;

    #[test]
    fn a_drain_worker_that_missed_the_stop_wake_up_does_not_sleep() {
        // Another worker took the stop's wake-up, and this one slept out its
        // timeout, past the stop's deadline (B180).
        let wake = (Mutex::new(false), parking_lot::Condvar::new());
        let started = Instant::now();
        sleep_for_drain_work(&wake, || true, Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn pending(kind: &str) -> PendingOp {
        PendingOp {
            id: 0,
            kind: kind.to_string(),
            uid: NodeUid::new(VolumeId::from("v"), LinkId::from("node")).to_string(),
            parent_uid: Some(NodeUid::new(VolumeId::from("v"), LinkId::from("parent")).to_string()),
            name: Some("name".to_string()),
            blob_path: (kind == OP_REVISION).then(|| "/staging/blob".to_string()),
            meta_json: None,
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        }
    }

    /// The park's only ordinary exit is the transient-to-final rename. The
    /// sweep is what happens when that rename never comes.
    #[test]
    fn a_park_expires_only_once_it_is_old_and_nobody_holds_the_file() {
        let now = PARK_EXPIRY_MS * 10;
        // Fresh: the writer may still rename it.
        assert_eq!(
            park_verdict(now - PARK_EXPIRY_MS + 1, now, false, true),
            ParkVerdict::Keep
        );
        // Old, but still open: a slow write is not an abandoned one.
        assert_eq!(
            park_verdict(now - PARK_EXPIRY_MS, now, true, true),
            ParkVerdict::Keep
        );
        // Old and closed: no rename is coming.
        assert_eq!(
            park_verdict(now - PARK_EXPIRY_MS, now, false, true),
            ParkVerdict::Release
        );
    }

    /// A transient create is queued a moment before its node is written. A
    /// sweep in between dropped it, and the rename to the finished name then
    /// failed with `EBUSY` (B145).
    #[test]
    fn a_refusal_from_drive_is_sorted_into_an_issue_and_the_network_is_not() {
        use proton_drive_rs::proton_sdk::error::ProtonApiError;
        let api = |code, http_status| {
            ProtonError::Api(ProtonApiError {
                code,
                http_status,
                message: String::new(),
                details: None,
            })
        };
        let issue = |e: ProtonError| refusal(&e);
        assert_eq!(
            issue(api(ResponseCode::InsufficientQuota, 422)),
            Some(Some(SyncIssue::Quota))
        );
        assert_eq!(
            issue(api(ResponseCode::NotEnoughPermissions, 403)),
            Some(Some(SyncIssue::Access))
        );
        assert_eq!(
            issue(api(ResponseCode::DoesNotExist, 404)),
            Some(Some(SyncIssue::Missing))
        );
        assert_eq!(
            issue(api(ResponseCode::TooManyChildren, 422)),
            Some(Some(SyncIssue::Limit))
        );
        // A refusal nobody sorted clears an earlier issue; the network keeps it.
        assert_eq!(issue(api(ResponseCode::AlreadyExists, 422)), Some(None));
        assert_eq!(issue(api(ResponseCode::RequestTimeout, 408)), None);
        // An error that is no refusal from Drive at all is no issue either.
        assert_eq!(refusal(&std::io::Error::other("disk")), None);

        // The drain sees Drive's error wrapped by whatever called it.
        #[derive(Debug)]
        struct Wrapped(ProtonError);
        impl std::fmt::Display for Wrapped {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("uploading")
            }
        }
        impl std::error::Error for Wrapped {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let wrapped = Wrapped(api(ResponseCode::InsufficientSpace, 422));
        assert_eq!(refusal(&wrapped), Some(Some(SyncIssue::Quota)));
    }

    /// An op that waited for a queued trash to free its name retried only
    /// after the doubled backoff, past the moment the trash landed.
    #[test]
    fn a_held_name_is_a_wait_and_other_failures_are_not() {
        assert!(waits_for_name(held_by_queued_change("c.bin").as_ref()));
        assert_eq!(
            held_by_queued_change("c.bin").to_string(),
            "c.bin is still held by a node whose trash or rename is queued"
        );
        assert!(!waits_for_name(&std::io::Error::other("disk")));
    }

    #[test]
    fn a_parked_create_is_dropped_only_once_its_file_is_gone_and_closed() {
        let now = PARK_EXPIRY_MS * 10;
        // Just queued, its node not written yet.
        assert_eq!(park_verdict(now - 3, now, false, false), ParkVerdict::Keep);
        // Still open: the writer has not let go of it.
        assert_eq!(
            park_verdict(now - PARK_UNLISTED_GRACE_MS, now, true, false),
            ParkVerdict::Keep
        );
        // Gone for a while and closed: the file was deleted.
        assert_eq!(
            park_verdict(now - PARK_UNLISTED_GRACE_MS, now, false, false),
            ParkVerdict::Drop
        );
    }

    #[test]
    fn drain_authorities_match_each_operation_kind() {
        for kind in [OP_REVISION, OP_TRASH] {
            assert_eq!(pending_op_authorities(&pending(kind)).unwrap().len(), 1);
        }
        for kind in [OP_CREATE, OP_MKDIR] {
            let authorities = pending_op_authorities(&pending(kind)).unwrap();
            assert_eq!(authorities.len(), 1);
            assert_eq!(authorities[0].to_string(), "v~parent");
        }
        let authorities = pending_op_authorities(&pending(OP_RENAME)).unwrap();
        assert_eq!(authorities.len(), 2);
        assert_eq!(authorities[0].to_string(), "v~node");
        assert_eq!(authorities[1].to_string(), "v~parent");
        let withdrawn = PendingOp {
            uid: "local~never-landed".to_string(),
            ..pending(OP_TRASH)
        };
        let authorities = pending_op_authorities(&withdrawn).unwrap();
        assert_eq!(authorities.len(), 1);
        assert_eq!(authorities[0].to_string(), "v~parent");
        let withdrawn = PendingOp {
            parent_uid: None,
            ..withdrawn
        };
        assert!(pending_op_authorities(&withdrawn).unwrap().is_empty());
    }

    fn folder_node(link: &str, parent: Option<&str>) -> Node {
        Node {
            uid: NodeUid::new(VolumeId::from("v"), LinkId::from(link)),
            parent_uid: parent
                .map(|parent| NodeUid::new(VolumeId::from("v"), LinkId::from(parent))),
            kind: NodeKind::Folder,
            name: link.to_string(),
            creation_time: 0,
            modification_time: 0,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            membership: None,
            photo: None,
            album: None,
            verification: Default::default(),
            direct_role: None,
            share_id: None,
        }
    }

    #[test]
    fn queued_cross_parent_rename_retains_viewer_source_authority_after_local_move() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pdfs-drain-rename-access-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("cache.db")).unwrap();
        let source = folder_node("source", None);
        let destination = folder_node("destination", None);
        let mut moved = file_node(1, 0, None);
        moved.parent_uid = Some(destination.uid.clone());
        for node in [&source, &destination, &moved] {
            db.upsert_node(node).unwrap();
        }
        db.set_share_access(&source.uid, Access::Viewer).unwrap();
        db.set_share_access(&destination.uid, Access::Editor)
            .unwrap();

        let mut op = pending(OP_RENAME);
        op.uid = moved.uid.to_string();
        op.parent_uid = Some(destination.uid.to_string());
        op.meta_json = Some(
            serde_json::to_string(&RenameMeta {
                original_parent_uid: source.uid.to_string(),
                original_name: Some(moved.name.clone()),
            })
            .unwrap(),
        );
        let authorities = pending_op_authorities(&op).unwrap();
        assert_eq!(authorities.len(), 3);
        assert_eq!(authorities[1], source.uid);

        let applied = Cell::new(false);
        let disposition = run_authorized_drain(
            &op,
            |uid| match db.effective_node_access(uid).unwrap() {
                Some(access) if access.writable() => WriteAuthority::Writable,
                Some(_) => WriteAuthority::Denied,
                None => WriteAuthority::Unknown,
            },
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(disposition, DrainDisposition::AccessDeferred);
        assert!(!applied.get());
        assert_eq!(
            db.effective_node_access(&moved.uid).unwrap(),
            Some(Access::Editor),
            "the node's optimistic destination ancestry is writable"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn viewer_deferred_drain_never_applies_or_consumes_an_attempt() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pdfs-drain-access-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("cache.db")).unwrap();
        let mut queued = pending(OP_REVISION);
        queued.attempts = 2;
        db.enqueue_op(&queued).unwrap();
        let op = db.pending_ops().unwrap().remove(0);
        let before_count = db.pending_op_counts().unwrap();
        let applied = Cell::new(false);

        let disposition = run_authorized_drain(
            &op,
            |_| WriteAuthority::Denied,
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(disposition, DrainDisposition::AccessDeferred);
        assert!(!applied.get(), "no API surrogate may run while read-only");
        db.defer_op_without_attempt(op.id, 5_000).unwrap();

        let retained = db.pending_ops().unwrap();
        let after_count = db.pending_op_counts().unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].attempts, op.attempts);
        assert_eq!(after_count.uploads, before_count.uploads);
        assert_eq!(after_count.changes, before_count.changes);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A deferral that never clears has to stop being silent. Found in the field
    /// as revision ops whose node had left the local tree: `require_uid_access`
    /// reads an unknown node as `EACCES`, so they were re-deferred every five
    /// seconds for thirty days with `attempts` at zero, `last_error` null and
    /// `failing_ops` at zero — 18 GiB of accepted writes that `pdfs status`
    /// reported as ordinary queued uploads.
    #[test]
    fn an_access_deferral_that_outlives_the_limit_is_reported_as_failing() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pdfs-drain-defer-limit-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("cache.db")).unwrap();
        db.enqueue_op(&pending(OP_REVISION)).unwrap();
        let op = db.pending_ops().unwrap().remove(0);

        // Two deferrals a recheck apart: the window opens on the first and the
        // second is measured against it, not against itself.
        let now = 1_000_000i64;
        let since = db.defer_op_for_access(op.id, now, now + 5_000).unwrap();
        assert_eq!(since, now, "the first deferral opens the window");
        let later = now + DRAIN_ACCESS_RECHECK.as_millis() as i64;
        assert_eq!(
            db.defer_op_for_access(op.id, later, later + 5_000).unwrap(),
            now,
            "a later deferral keeps the original stamp"
        );
        let queued = db.pending_ops().unwrap();
        assert_eq!(queued[0].attempts, 0, "deferrals consume no attempt");
        assert!(queued[0].last_error.is_none());

        // Past the limit the same deferral is reported like any other stuck op.
        let past = now + DRAIN_ACCESS_DEFER_LIMIT.as_millis() as i64;
        assert_eq!(
            db.defer_op_for_access(op.id, past, past + 5_000).unwrap(),
            now
        );
        db.record_op_failure(op.id, "not writable locally", past + 10_000)
            .unwrap();
        let failed = db.pending_ops().unwrap();
        assert_eq!(failed.len(), 1, "the row and its staged bytes stay queued");
        assert_eq!(failed[0].attempts, 1);
        assert_eq!(
            failed[0].last_error.as_deref(),
            Some("not writable locally")
        );

        // Reporting closes the window, so the next deferral starts a fresh one
        // rather than escalating on every recheck from here on.
        let after = past + 20_000;
        assert_eq!(
            db.defer_op_for_access(op.id, after, after + 5_000).unwrap(),
            after,
            "recording the failure re-armed the window"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An op that gets through the access check is no longer blocked, so a
    /// deferral after it must not inherit the earlier run's age and escalate
    /// immediately.
    #[test]
    fn clearing_the_window_makes_the_next_deferral_a_fresh_run() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pdfs-drain-defer-clear-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("cache.db")).unwrap();
        db.enqueue_op(&pending(OP_REVISION)).unwrap();
        let op = db.pending_ops().unwrap().remove(0);

        let now = 2_000_000i64;
        assert_eq!(
            db.defer_op_for_access(op.id, now, now + 5_000).unwrap(),
            now
        );
        db.clear_op_access_deferral(op.id).unwrap();
        let later = now + DRAIN_ACCESS_DEFER_LIMIT.as_millis() as i64;
        assert_eq!(
            db.defer_op_for_access(op.id, later, later + 5_000).unwrap(),
            later,
            "the window reopens at the new deferral, not the old one"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_authority_the_tree_does_not_have_is_reported_as_unknown_not_deferred() {
        // The B83 distinction, at the point it is made: a uid with no row must
        // not come back as `AccessDeferred`, because deferring waits on a
        // permission change that nothing is going to make. The caller answers
        // an `AuthorityUnknown` by asking the remote instead.
        let op = pending(OP_REVISION);
        let applied = Cell::new(false);
        let disposition = run_authorized_drain(
            &op,
            |_| WriteAuthority::Unknown,
            || {
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            disposition,
            DrainDisposition::AuthorityUnknown(parse_node_uid(&op.uid).unwrap())
        );
        assert!(!applied.get(), "an unresolved authority never applies");
    }

    #[test]
    fn a_missing_parent_authority_names_the_parent_not_the_node() {
        // A create's authority is its parent, so that is the uid the remote has
        // to be asked about — reporting the node instead would refetch the
        // thing that does not exist yet.
        let mut op = pending(OP_CREATE);
        let parent = folder_node("vanished-parent", None);
        op.parent_uid = Some(parent.uid.to_string());
        let disposition =
            run_authorized_drain(&op, |_| WriteAuthority::Unknown, || Ok(())).unwrap();
        assert_eq!(
            disposition,
            DrainDisposition::AuthorityUnknown(parent.uid.clone())
        );
    }

    #[test]
    fn owner_and_editor_drain_authorities_apply_normally() {
        for access in [Access::Owner, Access::Editor] {
            let applied = Cell::new(false);
            let disposition = run_authorized_drain(
                &pending(OP_REVISION),
                |_| {
                    if access.writable() {
                        WriteAuthority::Writable
                    } else {
                        WriteAuthority::Denied
                    }
                },
                || {
                    applied.set(true);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(disposition, DrainDisposition::Applied);
            assert!(applied.get());
        }
    }

    fn file_node(mtime: i64, size: i64, rev: Option<&str>) -> Node {
        Node {
            uid: NodeUid::new(VolumeId::from("v"), LinkId::from("l")),
            parent_uid: None,
            kind: NodeKind::File {
                media_type: "text/plain".into(),
                total_size_on_storage: size,
                active_revision_state: None,
                active_revision_id: rev.map(String::from),
                claimed_size: Some(size),
                claimed_modification_time: None,
                content_sha1: None,
            },
            name: "f.txt".into(),
            creation_time: 0,
            modification_time: mtime,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            membership: None,
            photo: None,
            album: None,
            verification: Default::default(),
            direct_role: None,
            share_id: None,
        }
    }

    #[test]
    fn only_a_fresh_empty_twin_of_the_same_kind_is_adopted() {
        // The op was queued at t=1000 s, after a create that got no answer.
        let queued = 1_000_000;
        let made = |mut node: Node, secs: i64| {
            node.creation_time = secs;
            node
        };
        assert!(adoptable(
            false,
            &made(file_node(0, 0, None), 990),
            queued,
            &[]
        ));
        assert!(adoptable(
            true,
            &made(folder_node("d", None), 990),
            queued,
            &[]
        ));
        // Made long before the op: someone else's file, not our lost create.
        assert!(!adoptable(
            false,
            &made(file_node(0, 0, None), 800),
            queued,
            &[]
        ));
        // Our create leaves a file empty; one with other content is someone
        // else's.
        assert!(!adoptable(
            false,
            &made(file_node(0, 5, None), 990),
            queued,
            &[]
        ));
        // The kind has to match what the op makes.
        assert!(!adoptable(
            true,
            &made(file_node(0, 0, None), 990),
            queued,
            &[]
        ));
        assert!(!adoptable(
            false,
            &made(folder_node("d", None), 990),
            queued,
            &[]
        ));
    }

    #[test]
    fn a_twin_holding_the_bytes_the_create_sent_is_adopted() {
        // The create uploaded its bytes and lost the answer (B127).
        let queued = 1_000_000;
        let mut twin = file_node(0, 5, None);
        twin.creation_time = 990;
        if let NodeKind::File { content_sha1, .. } = &mut twin.kind {
            *content_sha1 = Some("AA55".into());
        }
        let sent = |shas: &[&str]| shas.iter().map(|sha| sha.to_string()).collect::<Vec<_>>();
        assert!(adoptable(false, &twin, queued, &sent(&["aa55"])));
        assert!(!adoptable(false, &twin, queued, &sent(&["bb66"])));
        assert!(!adoptable(false, &twin, queued, &[]));
        assert!(!adoptable(true, &twin, queued, &sent(&["aa55"])));
        // An attempt sent these bytes before a write replaced them (B156).
        assert!(adoptable(false, &twin, queued, &sent(&["aa55", "bb66"])));
        assert!(holds_bytes(&twin, "aa55"));
        assert!(!holds_bytes(&twin, "bb66"));
    }

    #[test]
    fn a_landed_upload_stays_cached_unless_it_would_crowd_the_cache_out() {
        const GIB: u64 = 1 << 30;
        let budget = 5 * GIB;
        // An edited document or a source file: read back from disk, not Drive.
        assert!(keeps_landed_upload(false, 40_000, budget));
        assert!(keeps_landed_upload(false, budget / 16, budget));
        // A video the user copied in would push out what they read lately.
        assert!(!keeps_landed_upload(false, budget / 16 + 1, budget));
        // Pinned content is kept whatever its size, and so is everything when
        // the cache has no cap.
        assert!(keeps_landed_upload(true, 4 * GIB, budget));
        assert!(keeps_landed_upload(false, 4 * GIB, 0));
    }

    #[test]
    fn a_landing_upload_counts_until_its_guard_goes() {
        let count = Arc::new(AtomicU64::new(0));
        let first = LandingUpload::begin(&count);
        let landed = || -> Result<(), ()> {
            let _second = LandingUpload::begin(&count);
            assert_eq!(count.load(Ordering::SeqCst), 2);
            Err(())
        };
        // Uncounted on an early return too, or the queue never reads empty.
        assert!(landed().is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(first);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    fn baseline(mtime: i64, size: u64, rev: Option<&str>) -> Baseline {
        Baseline {
            mtime,
            size,
            hash: None,
            revision_id: rev.map(String::from),
        }
    }

    #[test]
    fn same_revision_id_is_not_a_conflict_despite_mtime_drift() {
        // The exact shape of the reported bug: same revision, server re-stamped
        // its mtime (1784897777 -> 1784897780), same size. Must NOT conflict.
        let base = baseline(1_784_897_777, 87_438, Some("rev-A"));
        let remote = file_node(1_784_897_780, 87_438, Some("rev-A"));
        assert_eq!(revision_changed(&base, &remote), None);
    }

    #[test]
    fn a_new_revision_id_is_a_conflict_even_at_the_same_size() {
        // A same-size edit by another device advances the revision id: this is a
        // real conflict the id catches where (mtime, size) alone could miss it.
        let base = baseline(100, 40, Some("rev-A"));
        let remote = file_node(100, 40, Some("rev-B"));
        assert!(revision_changed(&base, &remote).is_some());
    }

    /// The reported shape (docs/BUGS.md B113): a downloader creates a file
    /// through the mount, and the file is moved before its write drains. The
    /// move re-stamps the node's mtime on the server (1790955809 -> 1790956593),
    /// but the revision is still the empty one the create minted.
    #[test]
    fn a_move_before_the_first_write_drains_is_not_a_conflict() {
        let minted = file_node(1_790_955_809, 0, Some("empty"));
        let path = std::env::temp_dir().join(format!("pdfs-drain-test-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let h = WriteHandle::created(2, &minted, file, path);
        let base = baseline(h.base_mtime, h.base_size, h.base_revision_id.as_deref());

        let moved = file_node(1_790_956_593, 0, Some("empty"));
        assert_eq!(revision_changed(&base, &moved), None);
        // Another device writing it is still a conflict.
        let edited = file_node(1_790_956_593, 0, Some("theirs"));
        assert!(revision_changed(&base, &edited).is_some());
    }

    #[test]
    fn without_ids_it_falls_back_to_mtime_and_size() {
        // Old sidecar (no revision id): the (mtime, size) tuple still governs.
        let base = baseline(100, 40, None);
        assert_eq!(revision_changed(&base, &file_node(100, 40, None)), None);
        assert!(revision_changed(&base, &file_node(101, 40, None)).is_some());
        assert!(revision_changed(&base, &file_node(100, 41, None)).is_some());
    }

    #[test]
    fn a_complete_write_over_our_own_sealed_revision_chains_not_forks() {
        // B70 layer B: the partial we sealed ourselves (rev-mine) is what the
        // remote holds; the resume rewrote the whole file. Not a foreign change.
        assert!(is_own_self_supersede(
            true,
            Some("rev-mine"),
            Some("rev-mine")
        ));
    }

    #[test]
    fn a_move_to_a_revision_we_did_not_seal_still_forks() {
        // Another device wrote rev-theirs; we only ever sealed rev-mine. Fork.
        assert!(!is_own_self_supersede(
            true,
            Some("rev-theirs"),
            Some("rev-mine")
        ));
    }

    #[test]
    fn an_incomplete_blob_never_self_supersedes() {
        // Its gaps still refer to the stale base; overwriting would mix
        // revisions, so it must take the conflict-copy path even over our seal.
        assert!(!is_own_self_supersede(
            false,
            Some("rev-mine"),
            Some("rev-mine")
        ));
    }

    #[test]
    fn a_missing_remote_id_never_self_supersedes() {
        // No id to prove the revision was ours: fall through to a fork rather
        // than matching None==None.
        assert!(!is_own_self_supersede(true, None, None));
    }
}

#[cfg(test)]
mod debounce_tests {
    use super::*;

    #[test]
    fn a_node_never_uploaded_keeps_the_fixed_grace_period() {
        assert_eq!(adaptive_debounce(None), DRAIN_REVISION_DEBOUNCE);
    }

    #[test]
    fn a_fast_upload_does_not_shorten_the_grace_period() {
        // The 2 seconds are not about upload time at all — they are the window
        // in which a preallocate-then-write tool supersedes its own first
        // close. A file that sends in 50 ms must still get them.
        assert_eq!(
            adaptive_debounce(Some(Duration::from_millis(50))),
            DRAIN_REVISION_DEBOUNCE
        );
    }

    #[test]
    fn a_slow_upload_widens_the_grace_period_to_match() {
        // The case the fixed debounce got wrong: saves arriving faster than the
        // upload completes, each superseding one already on the wire.
        assert_eq!(
            adaptive_debounce(Some(Duration::from_secs(25))),
            Duration::from_secs(25)
        );
    }

    #[test]
    fn a_pathological_upload_cannot_park_a_node() {
        assert_eq!(
            adaptive_debounce(Some(Duration::from_secs(6000))),
            DRAIN_REVISION_DEBOUNCE_MAX
        );
    }
}
