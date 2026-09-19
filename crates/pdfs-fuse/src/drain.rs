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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use futures::StreamExt;
use pdfs_core::batch;
use pdfs_core::cache::{Baseline, StagedWrite};
use pdfs_core::control::{ActivityKind, TransferDirection};
use pdfs_core::db::{
    OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH, PARK_EXPIRY_MS, PendingOp, RenameMeta,
};
use proton_drive_rs::proton_sdk::error::ProtonError;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeMoveItem};
use tracing::{debug, error, info, warn};

use super::state::Intervals;
use super::transfers::CountingReader;
use super::{
    Core, DRAIN_BACKOFF_MAX, DRAIN_BACKOFF_MIN, DRAIN_IDLE_POLL, DRAIN_REVISION_DEBOUNCE,
    DRAIN_REVISION_DEBOUNCE_MAX, ROOT_INO, UPLOAD_TIME_MEMORY, WriteAuthority, conflict_name,
    is_already_exists, is_gone, is_local_uid_str, media_type_for, node_revision_id, node_size,
    now_millis, now_secs, parse_node_uid,
};

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
            // One row, chosen and *claimed* by the database. Reading the whole
            // queue to pick one op made a long queue quadratic to drain, and
            // held the shared connection — and so every FUSE metadata call —
            // for the duration.
            let due = match self.db.claim_next_due_op(now) {
                Ok(due) => due,
                Err(error) => {
                    error!(%error, "claiming the next pending operation failed");
                    self.wait_for_drain_work();
                    continue;
                }
            };
            let online = self.online.load(Ordering::Relaxed);
            let op = match due {
                Some(op) if online => op,
                idle => {
                    // Claimed but offline: hand it straight back, or it stays
                    // invisible to every worker until this one happens to be
                    // the next to notice the reconnect.
                    if let Some(op) = idle {
                        self.release_claim(&op);
                    }
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
            if matches!(outcome, Ok(DrainDisposition::Applied))
                && let Err(error) = self.db.clear_op_access_deferral(op.id)
            {
                debug!(uid = %op.uid, %error, "clearing an access-deferral window failed");
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
                let backoff = DRAIN_BACKOFF_MIN
                    .saturating_mul(1u32 << attempts.min(6))
                    .min(DRAIN_BACKOFF_MAX);
                warn!(uid = %op.uid, attempts, error = %err_str, "pending upload failed; will retry");
                if let Err(e) = self.db.record_op_failure(
                    op.id,
                    &err_str,
                    now_millis() + backoff.as_millis() as i64,
                ) {
                    error!(uid = %op.uid, error = %e, "recording a drain failure failed");
                    self.release_claim(&op);
                    self.wait_for_drain_work();
                    continue;
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
        match self.fetch_node_remote(authority) {
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
                self.release_claim(op);
            }
            Err(error) => {
                debug!(uid = %op.uid, %authority, %error,
                       "could not ask the remote about a missing authority");
                self.defer_for_access(op, AccessDeferral::RemoteUnreachable);
            }
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
        let (lock, cv) = &*self.drain_wake;
        let mut woken = lock.lock();
        if !*woken {
            cv.wait_for(&mut woken, DRAIN_IDLE_POLL);
        }
        *woken = false;
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
        let (lock, cv) = &*self.drain_wake;
        let mut woken = lock.lock();
        if !*woken {
            cv.wait_for(&mut woken, timeout);
        }
        *woken = false;
    }

    /// Perform one queued op and retire it.
    pub(crate) fn drain_op(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        match op.kind.as_str() {
            OP_REVISION => self.drain_revision(op),
            OP_CREATE | OP_MKDIR => self.drain_local_node(op),
            OP_RENAME => self.drain_rename(op),
            OP_TRASH => self.drain_trash(op),
            other => Err(format!("unknown pending op kind {other:?}").into()),
        }
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
        let landed = if node.parent_uid.as_ref() == Some(&parent) {
            self.drain_rename_in_place(&uid, &node.name, &name)?
        } else {
            // Move and rename land as one request (`move-multiple` takes a
            // target name), so there is no half-applied state between them and
            // no window for the requirements to go stale between two calls —
            // the race B46 queued this op to get away from.
            let target = (node.name != name).then_some(name.as_str());
            match self.move_renaming(&uid, &parent, target) {
                Ok(()) => name.clone(),
                // The destination holds that name already. Landing under a
                // *different* name is the non-destructive resolution: it neither
                // clobbers their file nor drops ours, and it is visible.
                Err(e) if is_already_exists(&e) => {
                    let alt = conflict_name(&name, now_secs());
                    warn!(%uid, name, alt, "destination already holds that name; using a conflict name");
                    self.move_renaming(&uid, &parent, Some(&alt))?;
                    self.adopt_drained_name(&uid, &alt);
                    self.log_activity(
                        ActivityKind::Rename,
                        &name,
                        format!("destination already had that name; moved as {alt}"),
                        false,
                    );
                    alt
                }
                // The destination folder is gone. Leaving the node in its current
                // parent is the honest outcome: it is not where the user asked for
                // it, but it exists and it is where it has always been. The name
                // the user chose still applies there. Retrying the move could
                // only fail again — the folder is not coming back — and would
                // wedge the queue.
                Err(e) if is_gone(&e) => {
                    warn!(%uid, name, %parent, "move destination is gone; leaving the node where it is");
                    let landed = match self.drain_rename_in_place(&uid, &node.name, &name) {
                        Ok(landed) => landed,
                        // The node went with it; nothing is left to rename.
                        Err(e) if is_gone(&e) => node.name.clone(),
                        Err(e) => return Err(e.into()),
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
        self.note_self_change(&uid);
        info!(%uid, name = %landed, "pending rename landed");
        Ok(())
    }

    /// Rename `uid` without moving it, from `current` to `name`, and return the
    /// name it landed under. Someone may have taken the name while we were
    /// offline; the node then lands under a conflict name instead.
    fn drain_rename_in_place(
        &self,
        uid: &NodeUid,
        current: &str,
        name: &str,
    ) -> Result<String, ProtonError> {
        if current == name {
            return Ok(current.to_string());
        }
        match self.rt.block_on(self.client.rename_node(uid, name, None)) {
            Ok(()) => Ok(name.to_string()),
            Err(e) if is_already_exists(&e) => {
                let alt = conflict_name(name, now_secs());
                warn!(%uid, name, alt, "rename target name is taken; using a conflict name");
                self.rt.block_on(self.client.rename_node(uid, &alt, None))?;
                self.adopt_drained_name(uid, &alt);
                self.log_activity(
                    ActivityKind::Rename,
                    name,
                    format!("name was taken remotely; renamed to {alt}"),
                    false,
                );
                Ok(alt)
            }
            Err(e) => Err(e),
        }
    }

    /// Move `uid` under `parent` in one request, renaming it to `target_name` on
    /// the way when one is given.
    fn move_renaming(
        &self,
        uid: &NodeUid,
        parent: &NodeUid,
        target_name: Option<&str>,
    ) -> Result<(), ProtonError> {
        let item = NodeMoveItem {
            uid: uid.clone(),
            target_name: target_name.map(str::to_string),
        };
        self.rt.block_on(async {
            let mut outcomes =
                std::pin::pin!(self.client.move_nodes_streaming(vec![item], parent.clone()));
            match outcomes.next().await {
                Some(Ok((_, outcome))) => outcome,
                Some(Err(e)) => Err(e),
                None => Err(ProtonError::invalid_operation(format!(
                    "move of {uid} reported no outcome"
                ))),
            }
        })
    }

    /// Apply a queued trash to the remote.
    ///
    /// A node that is already gone is a success, not a failure: the outcome the
    /// op asked for holds either way, and retrying forever against a node the
    /// server has forgotten would wedge the queue.
    pub(crate) fn drain_trash(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let uid = parse_node_uid(&op.uid).ok_or("trash op has an unparseable uid")?;
        let name = op.name.clone().unwrap_or_else(|| op.uid.clone());
        match self
            .rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(&uid)))
            .and_then(batch::into_unit)
        {
            Ok(()) => {}
            Err(e) if is_gone(&e) => {
                debug!(%uid, name, "node was already gone remotely; trash op satisfied");
            }
            Err(e) => return Err(e.into()),
        }
        self.db.complete_trash_op(op.id, &uid)?;
        self.own_sealed_revs.lock().remove(&uid);
        if let Err(e) = self.db.clear_own_sealed_rev(&uid.to_string()) {
            debug!(%uid, error = %e, "clearing the sealed-revision record failed");
        }
        self.invalidate_trash();
        self.log_activity(ActivityKind::Trash, &name, "trashed", true);
        info!(%uid, name, "pending trash landed");
        Ok(())
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
        let local = parse_node_uid(&op.uid).ok_or("pending op has an unparseable uid")?;
        let parent_str = op.parent_uid.as_deref().ok_or("create op has no parent")?;
        // `run_pending_drain` will not offer an op whose parent is still a
        // placeholder, so reaching here with one is a bug, not a wait.
        if is_local_uid_str(parent_str) {
            return Err(format!("parent {parent_str} has not been created yet").into());
        }
        let parent = parse_node_uid(parent_str).ok_or("create op has an unparseable parent")?;
        let wanted = op.name.clone().ok_or("create op has no name")?;

        // Someone else may have taken the name while this sat in the queue —
        // reachable only because the op waited, which is the whole point of the
        // queue. Never overwrite theirs and never drop ours: land under a
        // conflict name, exactly as the sync engine does.
        let mut name = wanted.clone();
        let mut real = self.create_drained_node(op, &parent, &name);
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
        if let Some(root) = self.root_uid()
            && real.is_err()
            && self.parent_is_gone(&parent)
        {
            warn!(%local, name, "parent folder is gone remotely; creating in the root instead");
            real = self.create_drained_node(op, &root, &name);
            if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
                name = conflict_name(&wanted, now_secs());
                real = self.create_drained_node(op, &root, &name);
            }
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

        // Retire the op before touching anything else: if we crash here the node
        // exists remotely and the local placeholder is reconciled by the event
        // sync, whereas a surviving op would create the file a second time.
        self.db.delete_op(op.id)?;
        self.adopt_real_uid(&local, &real)?;
        // The feed will report this create back to us; the tree already has it
        // under its real uid, so that event is ours to ignore (`Core::self_changes`).
        self.note_self_change(&real);
        if let Some(blob) = op.blob_path.as_deref() {
            self.cache.discard_staged(Path::new(blob));
        }
        self.pending.lock().remove(&local);
        self.log_activity(ActivityKind::Upload, &name, "created", true);
        info!(%local, %real, name, kind = %op.kind, "pending create landed");
        Ok(())
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
        match op.kind == OP_MKDIR {
            true => {
                Ok(self
                    .rt
                    .block_on(self.client.create_folder(parent, name, Some(now_secs())))?)
            }
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
            return Ok(self.rt.block_on(self.client.upload_file(
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
        let guard = self
            .transfers
            .begin(name, op.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(blob)?, &guard);
        let uid = self.rt.block_on(self.client.upload_file_from(
            parent,
            name,
            media_type_for(name),
            reader,
            meta.len as i64,
            Vec::new(),
            None,
            false,
        ))?;
        Ok(uid)
    }

    /// Swap a placeholder uid for the real one across everything that keyed off
    /// it: queued children, the DB, the in-memory tree, and the caches.
    ///
    /// The inode is deliberately kept, so anything already holding the file open
    /// keeps working across the drain.
    ///
    /// Applied to **every** live inode space, not just this Core's. A node
    /// created inside an on-demand sync folder lives in that fork's `state`,
    /// while the drain that lands its create belongs to the primary mount — so
    /// reaching for `self.state` here found nothing and left the fork's entry on
    /// its `local~` uid, which reads as an empty file for as long as the daemon
    /// runs (`docs/BUGS.md` B74). A uid is unique across mounts, so at most one
    /// of them matches and the others are no-ops.
    pub(crate) fn adopt_real_uid(
        &self,
        local: &NodeUid,
        real: &NodeUid,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let node = self
            .fetch_node(real)
            .map_err(|e| self.errno_error(e, "fetch node"))?;
        // Repoints queued children and node rows, and drops the placeholder row.
        self.db
            .remap_local_uid(&local.to_string(), &real.to_string())?;

        self.for_each_state(|st| {
            let Some(ino) = st.by_uid.remove(local) else {
                return;
            };
            st.by_uid.insert(real.clone(), ino);
            if let Some(e) = st.entries.get_mut(&ino) {
                e.uid = real.clone();
                e.node = node.clone();
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
        // Write the real node through, now that nothing points at the old uid.
        if let Err(e) = self.db.upsert_node(&node) {
            warn!(%real, error = %e, "db upsert_node failed after remap");
        }
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
        // In memory first, then the table it is written through to: the map is
        // this process's cache of it, and after a restart only the table has
        // the answer (which is the whole reason it is persisted).
        let own_rev = match self.own_sealed_revs.lock().get(uid).cloned() {
            Some(rev) => Some(rev),
            None => self
                .db
                .own_sealed_rev(&uid.to_string(), now_millis())
                .unwrap_or_default(),
        };
        if is_own_self_supersede(meta.complete, remote_rev.as_deref(), own_rev.as_deref()) {
            debug!(%uid, ?remote_rev,
                   "queued write chains onto our own sealed revision; not a conflict");
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
        // copy falls back to the root rather than being abandoned.
        let Some(parent) = place.map(|(p, _)| p).or_else(|| self.root_uid()) else {
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
        let guard =
            self.transfers
                .begin(&alt, meta.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(blob)?, &guard);
        self.rt.block_on(self.client.upload_file_from(
            &parent,
            &alt,
            media_type_for(&alt),
            reader,
            meta.len as i64,
            Vec::new(),
            None,
            false,
        ))?;
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
            let known = match self.db.node_by_uid(&op.uid) {
                Ok(known) => known.is_some(),
                // Never infer "deleted" from a failed read: that would discard
                // the bytes over a transient database error.
                Err(error) => {
                    debug!(%uid, %error, "cannot tell whether a parked node still exists");
                    continue;
                }
            };
            if !known {
                if let Err(error) = self.discard_queued_ops(&uid) {
                    warn!(%uid, name, ?error, "dropping a deleted transient's parked create failed");
                    continue;
                }
                dropped += 1;
                debug!(%uid, name, "dropped a parked create whose file was deleted");
                continue;
            }
            if park_verdict(op.created_at, now, self.uid_is_open(&uid)) != ParkVerdict::Release {
                continue;
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
        // Across every mount: the drain runs on the primary Core, but a node
        // written inside a sync folder is interned in that fork's inode space,
        // so asking `self.state` alone always missed it and fell through to the
        // DB. That still gave the right answer — the fallback below exists for
        // exactly this shape of miss — but it went to disk for something already
        // in memory, and only *because* of the fallback was it not a bug.
        let mut found = None;
        self.for_each_state(|st| {
            if found.is_some() {
                return;
            }
            if let Some(entry) = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino))
                && let Some(parent) = entry.node.parent_uid.clone()
            {
                found = Some((parent, entry.node.name.clone()));
            }
        });
        if found.is_some() {
            return found;
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
        let st = self.state();
        st.entries.get(&ROOT_INO).map(|e| e.uid.clone())
    }

    /// Upload a staged revision of a file the server already knows about.
    pub(crate) fn drain_revision(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let blob = op
            .blob_path
            .clone()
            .ok_or("pending op has no staged blob")?;
        let blob = PathBuf::from(blob);
        let meta: StagedWrite = serde_json::from_str(op.meta_json.as_deref().unwrap_or(""))?;
        let uid = parse_node_uid(&meta.uid).ok_or("staged write has an unparseable uid")?;

        if let Some(reason) = self.revision_conflict(&uid, &meta)? {
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
        let guard =
            self.transfers
                .begin(&name, meta.uid.clone(), TransferDirection::Upload, meta.len);
        // Registered before the first byte moves, so a write that lands while
        // this is on the wire can stop it rather than paying for a revision the
        // op it queued will immediately replace.
        let cancel = self.begin_cancellable_upload(&uid);
        let reader = CountingReader::new(File::open(&blob)?, &guard).with_cancel(cancel.flag());
        let started = Instant::now();
        let sent = self.rt.block_on(self.client.upload_new_revision_from(
            &uid,
            reader,
            meta.len as i64,
            Vec::new(),
            None,
        ));
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

        // Retire the op before dropping the blob: a crash between the two leaves
        // an orphaned file (harmless), whereas the reverse would leave a queued
        // op pointing at nothing.
        self.db.delete_op(op.id)?;
        self.release_pending(&uid, &blob);

        // The staged blob now matches the sealed revision, so a pinned file keeps
        // it as its cached content rather than re-downloading what we just sent.
        // Adopted by link, not by value: the blob is the whole file, and a pinned
        // video read into a `Vec` here is an OOM. `discard_staged` below drops the
        // staging name only — the cache keeps the inode.
        if self.cache.is_pinned(&uid) {
            let _ = self.cache.store_file(&uid, now_secs(), meta.len, &blob);
        }
        self.cache.discard_staged(&blob);
        self.evict_reader(&uid);
        self.refresh_after_upload(&uid);
        self.log_activity(ActivityKind::Upload, &name, "uploaded", true);
        info!(%uid, len = meta.len, "pending upload landed");
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
    /// Best effort: a failure here costs a spurious conflict copy on the next
    /// write, not this upload, which has already landed.
    pub(crate) fn refresh_after_upload(&self, uid: &NodeUid) {
        let node = match self.fetch_node_remote(uid) {
            Ok(Some(node)) => node,
            // Trashed or deleted under us: the tree will hear it from the event
            // sync, which is better placed to unhook the inode than we are.
            Ok(None) => return,
            Err(e) => {
                warn!(%uid, error = %e,
                      "refreshing metadata after an upload failed; \
                       the next write to this file may conflict with itself");
                return;
            }
        };
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
            let sealed_mtime = node.modification_time;
            let sealed_size = node_size(&node);
            let sealed_rev = node_revision_id(&node);
            // Remember it as ours, so a queued write that opened over an earlier
            // revision recognises this as a self-supersede rather than a foreign
            // change and does not fork (B70 layer B; consulted in
            // `revision_conflict`).
            if let Some(rev) = sealed_rev.clone() {
                self.own_sealed_revs.lock().insert(uid.clone(), rev.clone());
                // Write through: losing this at shutdown reopened the fork
                // window B70 layer B closed, for the first drain after a
                // restart.
                if let Err(e) = self
                    .db
                    .set_own_sealed_rev(&uid.to_string(), &rev, now_millis())
                {
                    warn!(%uid, error = %e, "persisting our sealed revision failed");
                }
            }
            self.for_each_state(|st| {
                for aw in st.active_writes.values_mut() {
                    if aw.uid == *uid {
                        aw.base_mtime = sealed_mtime;
                        aw.base_size = sealed_size;
                        aw.base_revision_id = sealed_rev.clone();
                        debug!(%uid, mtime = sealed_mtime, size = sealed_size,
                               "rebased open write handle onto the revision just uploaded");
                    }
                }
            });
        }
        // Ordered so that a write queued *during* the fetch above is still
        // caught: it took its baseline from the node's optimistic stamp, and
        // this overwrites it with the revision the server actually holds.
        if self.rebaseline_pending(uid, &node) {
            return;
        }
        self.for_each_state(|st| {
            let Some(parent) = st
                .by_uid
                .get(uid)
                .and_then(|ino| st.entries.get(ino))
                .map(|e| e.parent)
            else {
                return;
            };
            st.intern(parent, node.clone());
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

/// What the park sweep does with one parked create whose node still exists.
#[derive(Debug, PartialEq, Eq)]
enum ParkVerdict {
    /// Leave it parked: the rename it waits for can still come.
    Keep,
    /// Un-park it: nothing will rename it now, so the bytes go up as they are.
    Release,
}

/// Judge one parked create by age alone. An open node is kept whatever its age
/// — it is still being written, and the rename is the writer's last step.
fn park_verdict(created_at: i64, now: i64, open: bool) -> ParkVerdict {
    if open || now - created_at < PARK_EXPIRY_MS {
        return ParkVerdict::Keep;
    }
    ParkVerdict::Release
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfs_core::Access;
    use pdfs_core::db::Db;
    use proton_drive_rs::NodeKind;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};
    use std::cell::Cell;

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
            park_verdict(now - PARK_EXPIRY_MS + 1, now, false),
            ParkVerdict::Keep
        );
        // Old, but still open: a slow write is not an abandoned one.
        assert_eq!(
            park_verdict(now - PARK_EXPIRY_MS, now, true),
            ParkVerdict::Keep
        );
        // Old and closed: no rename is coming.
        assert_eq!(
            park_verdict(now - PARK_EXPIRY_MS, now, false),
            ParkVerdict::Release
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
