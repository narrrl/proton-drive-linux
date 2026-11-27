//! The write-back drain: turning queued mutations into remote calls.
//!
//! Every write the kernel hands us is answered the moment its bytes and its
//! `pending_op` row are on disk, which is what lets a `cp`
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

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use pdfs_core::batch;
use pdfs_core::control::{ActivityKind, SyncIssue, TransferDirection};
use pdfs_store::cache::{Baseline, StagedWrite};
use pdfs_store::db::{
    CreateLanding, CreateRetired, OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH,
    PARK_EXPIRY_MS, PendingOp, RenameMeta,
};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use tracing::{debug, error, info, warn};

use super::link::{self, is_network_error};
use super::photos::sha1_of;
use super::state::{Intervals, PendingRevision};
use super::transfers::CountingReader;
use super::{
    Core, DRAIN_BACKOFF_MAX, DRAIN_BACKOFF_MIN, DRAIN_IDLE_POLL, DRAIN_REVISION_DEBOUNCE,
    DRAIN_REVISION_DEBOUNCE_MAX, UPLOAD_TIME_MEMORY, WriteAuthority, conflict_name,
    is_already_exists, is_gone, is_local_uid_str, media_type_for, node_revision_id, node_size,
    now_millis, now_secs, parse_node_uid,
};
use proton_drive_rs::proton_sdk::api::ResponseCode;
use proton_drive_rs::proton_sdk::error::ProtonError;

mod create;
mod rename;
mod revision;
mod trash;

/// The drain's own bookkeeping, shared by its workers and by every mount of
/// the daemon: the wake-up for idle workers, the creates and uploads on the
/// wire, and what a landed one left behind until the tree catches up.
#[derive(Clone)]
pub(crate) struct DrainState {
    /// Nudges the drain workers: set true and notify to have them re-examine
    /// the queue instead of waiting out their backoff.
    pub(crate) drain_wake: Arc<(Mutex<bool>, Condvar)>,
    /// The folder and name each create on the wire was sent with, by op id.
    /// Drive holds that name until the create lands and a rename or trash
    /// queued behind it frees it again.
    pub(crate) creating: Arc<Mutex<HashMap<i64, (NodeUid, String)>>>,
    /// Creates that landed but could not be read back, by the uid Drive gave
    /// them, with the placeholder's. The drain adopts one before it sends
    /// anything else for it.
    pub(crate) unadopted: Arc<Mutex<HashMap<NodeUid, NodeUid>>>,
    /// The SHA-1 of the last revision this daemon uploaded to a file and could
    /// not read back, so its revision id is not known (`docs/BUGS.md` B157).
    pub(crate) sealed_unread: Arc<Mutex<HashMap<NodeUid, String>>>,
    /// The node an upload has just put on Drive, from when the drain lets go
    /// of its blob until the tree has the node (`docs/BUGS.md` B170).
    pub(crate) landed: Arc<Mutex<HashMap<NodeUid, Node>>>,
    /// How long this node's last upload actually took, in milliseconds, with
    /// the time it was measured for eviction. Feeds
    /// [`Core::revision_debounce`]: a file that takes 30 seconds to send needs
    /// a grace period on that scale, or every save supersedes an upload that
    /// was already on the wire. Bounded by [`UPLOAD_TIME_MEMORY`].
    pub(crate) upload_times: Arc<Mutex<HashMap<String, (u64, i64)>>>,
    /// Cancellation flags for uploads currently on the wire, keyed by node.
    ///
    /// Set when a newer revision of the same file is queued: the bytes in
    /// flight have just been superseded, and the only thing finishing them
    /// achieves is spending the user's uplink on a revision the next op
    /// replaces. Read by the upload's own reader ([`CountingReader`]), which is
    /// the one thing the SDK calls often enough to notice.
    pub(crate) upload_cancel: Arc<Mutex<HashMap<NodeUid, Arc<AtomicBool>>>>,
    /// Uploads whose op is retired but whose bytes the content cache has not
    /// adopted yet, because the revision just sealed is still being read back.
    /// The status counts them as pending, so an empty queue means every file
    /// written through the mount reads without the network (`docs/BUGS.md`
    /// B119).
    pub(crate) landing_uploads: Arc<AtomicU64>,
}

impl DrainState {
    pub(crate) fn new() -> Self {
        Self {
            drain_wake: Arc::new((Mutex::new(false), Condvar::new())),
            creating: Arc::new(Mutex::new(HashMap::new())),
            unadopted: Arc::new(Mutex::new(HashMap::new())),
            sealed_unread: Arc::new(Mutex::new(HashMap::new())),
            landed: Arc::new(Mutex::new(HashMap::new())),
            upload_times: Arc::new(Mutex::new(HashMap::new())),
            upload_cancel: Arc::new(Mutex::new(HashMap::new())),
            landing_uploads: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Debug)]
struct TrashedMove(String);

impl std::fmt::Display for TrashedMove {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "‘{}’ is in Proton Drive Trash. Restore it before retrying this move.",
            self.0
        )
    }
}

impl std::error::Error for TrashedMove {}

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
/// [`Db::effective_node_access`]: pdfs_store::db::Db::effective_node_access
/// [`FAILING_ATTEMPTS`]: pdfs_store::db::FAILING_ATTEMPTS
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
    /// Drain the pending-op queue: the background half of every write.
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
    /// [`Db::claim_next_due_op`]: pdfs_store::db::Db::claim_next_due_op
    pub(crate) fn run_pending_drain(&self, primary: bool) {
        loop {
            // Between ops, never inside one: an op that has started is either
            // retired or released, so stopping here cannot leave the queue with
            // a row claimed by a worker that no longer exists (`docs/BUGS.md` B44).
            if self.shutdown.is_stopping() {
                debug!(primary, "drain worker stopping");
                return;
            }
            let now = now_millis();
            // Offline or paused, nothing is claimed. A claimed create may be on
            // the wire, so trashing its folder kept it as a trash; claimed
            // only to be handed back, it was kept although nothing had sent
            // it (`docs/BUGS.md` B178).
            let online = self.is_online();
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
                    self.wait_for_drain_work_or_due(online && !paused);
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
                let backoff = drain_backoff(attempts, held, op.id);
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
    /// [`Db::record_op_failure`]: pdfs_store::db::Db::record_op_failure
    /// [`FAILING_ATTEMPTS`]: pdfs_store::db::FAILING_ATTEMPTS
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

    /// Block until there is plausibly something to do: a new op, a reconnect, or
    /// the shortest outstanding backoff elapsing.
    pub(crate) fn wait_for_drain_work(&self) {
        sleep_for_drain_work(
            &self.drain.drain_wake,
            || self.shutdown.is_stopping(),
            DRAIN_IDLE_POLL,
        );
    }

    /// Like [`wait_for_drain_work`](Self::wait_for_drain_work), but first
    /// checks whether a debounced or backed-off op is due before the idle-poll
    /// ceiling. A 2-second debounce must not sleep 30 seconds.
    ///
    /// `claiming` is false while the worker claims nothing (offline or
    /// paused); see [`idle_wait`].
    fn wait_for_drain_work_or_due(&self, claiming: bool) {
        let earliest = if claiming {
            self.db.earliest_due_at().ok().flatten()
        } else {
            None
        };
        let timeout = idle_wait(claiming, earliest, now_millis());
        sleep_for_drain_work(
            &self.drain.drain_wake,
            || self.shutdown.is_stopping(),
            timeout,
        );
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
        let mut pending = self.writes.pending.lock();
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

/// A child can have a normal metadata record even when an ancestor is trashed.
/// Check both live parent chains before interpreting a refused move as an
/// access problem. Shared ancestors only need to be fetched once.
fn trashed_move_folder<'a>(
    parents: impl IntoIterator<Item = &'a NodeUid>,
    mut fetch: impl FnMut(&NodeUid) -> Option<Node>,
) -> Option<String> {
    let mut seen = HashSet::new();
    for parent in parents {
        let mut next = Some(parent.clone());
        while let Some(uid) = next.take()
            && seen.insert(uid.clone())
            && let Some(node) = fetch(&uid)
        {
            if node.trashed {
                return Some(node.name);
            }
            next = node.parent_uid;
        }
    }
    None
}

/// The answer from Drive somewhere in `e`'s chain, if it got that far.
fn proton_error<'e>(e: &'e (dyn std::error::Error + 'static)) -> Option<&'e ProtonError> {
    std::iter::successors(Some(e), |e| e.source()).find_map(|e| e.downcast_ref::<ProtonError>())
}

/// What a failed attempt says about its op's sync issue: `None` when Drive
/// could not be reached, which says nothing; otherwise the issue Drive's answer
/// is, or `Some(None)` for a failure that is not a refusal.
fn refusal(e: &(dyn std::error::Error + 'static)) -> Option<Option<SyncIssue>> {
    if e.is::<TrashedMove>() {
        return Some(Some(SyncIssue::Trashed));
    }
    let proton = proton_error(e)?;
    // A refused session refuses everything, so it says nothing about this
    // op either; the mount is offline for it (`Core::lost_link`).
    if is_network_error(proton) || pdfs_core::error::session_revoked(proton) {
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

/// How long an op waits after its `attempts`-th failure: doubling from
/// [`DRAIN_BACKOFF_MIN`] up to [`DRAIN_BACKOFF_MAX`], plus up to a fifth more,
/// different for each op (`seed`), so that ops that failed together are not
/// all retried in the same second. One that waits for a name (`held`) waits
/// the minimum.
///
/// The doubling stopped at 2^6, 128 s, so the maximum was never reached.
fn drain_backoff(attempts: i64, held: bool, seed: i64) -> Duration {
    if held {
        return DRAIN_BACKOFF_MIN;
    }
    let base = DRAIN_BACKOFF_MIN
        .saturating_mul(1u32 << attempts.clamp(0, 8))
        .min(DRAIN_BACKOFF_MAX);
    // A cheap, stable spread: the same op and attempt always wait the same.
    let spread = (seed.wrapping_mul(0x9E37_79B9).wrapping_add(attempts) as u64 % 21) as u32;
    base + base * spread / 100
}

/// How many attempts an op waits for a rename or a create on the wire to free
/// the name it wants. Two files swapping names each wait for the other, so the
/// wait has to end; a conflict name ends it.
const NAME_HOLD_ATTEMPTS: i64 = 4;

/// How much older than its op's first send a remote node may be and still be
/// the one an unanswered call made. The node is made during that call, so it
/// is at most one call's deadline older; the rest is clock skew between this
/// machine and Proton.
const ADOPT_WINDOW_MS: i64 = 2 * 60 * 1000;

/// Whether `twin`, found holding the name a queued create wants, is the node an
/// earlier, unanswered attempt at that create made: the same kind, created no
/// earlier than around when the op was first sent (`first_sent_ms`), and for a file still empty, as the mount's
/// create leaves it, or holding exactly bytes the op uploaded, whose SHA-1 is
/// one of `sent`.
fn adoptable(is_dir: bool, twin: &Node, first_sent_ms: i64, sent: &[String]) -> bool {
    if twin.creation_time.saturating_mul(1000) < first_sent_ms - ADOPT_WINDOW_MS {
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

/// How long an idle drain worker sleeps: until the earliest queued op falls
/// due, at most the idle poll.
///
/// A worker that claims nothing — offline or paused — sleeps the full idle
/// poll whatever is due. Every op already queued is due "now" for it, so
/// sleeping until then was a zero wait, and all the workers spun on the queue
/// for as long as the outage lasted (`docs/BUGS.md` B200). A reconnect or a
/// resume wakes the drain, so nothing waits the full poll for it.
fn idle_wait(claiming: bool, earliest_due: Option<i64>, now: i64) -> Duration {
    match earliest_due {
        Some(ts) if claiming => {
            Duration::from_millis((ts - now).max(0) as u64).min(DRAIN_IDLE_POLL)
        }
        _ => DRAIN_IDLE_POLL,
    }
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
    use pdfs_store::db::Db;
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

    #[test]
    fn an_offline_worker_waits_the_idle_poll_for_a_past_due_op() {
        // The op fell due long ago, and offline nobody attempts it, so it
        // stays due: waiting for it was a zero wait, forever (B200).
        assert_eq!(idle_wait(false, Some(0), 10_000), DRAIN_IDLE_POLL);
    }

    #[test]
    fn a_claiming_worker_waits_only_until_the_next_op_falls_due() {
        assert_eq!(
            idle_wait(true, Some(12_000), 10_000),
            Duration::from_secs(2)
        );
        assert_eq!(idle_wait(true, Some(0), 10_000), Duration::ZERO);
        assert_eq!(idle_wait(true, None, 10_000), DRAIN_IDLE_POLL);
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
    fn a_refused_move_identifies_a_trashed_ancestor_on_either_side() {
        let source = folder_node("source", Some("old"));
        let destination = folder_node("destination", Some("new"));
        let mut old = folder_node("old", None);
        let mut new = folder_node("new", None);
        let find = |old: &Node, new: &Node| {
            trashed_move_folder([&source.uid, &destination.uid], |uid| {
                [&source, &destination, old, new]
                    .into_iter()
                    .find(|node| node.uid == *uid)
                    .cloned()
            })
        };
        old.trashed = true;
        assert_eq!(find(&old, &new).as_deref(), Some("old"));
        old.trashed = false;
        new.trashed = true;
        let error = TrashedMove(find(&old, &new).unwrap());
        assert_eq!(refusal(&error), Some(Some(SyncIssue::Trashed)));
        assert!(error.to_string().contains("‘new’"));
        new.trashed = false;
        assert_eq!(find(&old, &new), None);
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
    fn a_failing_op_backs_off_to_the_maximum_with_a_spread() {
        let at = |attempts| drain_backoff(attempts, false, 7);
        assert!(at(1) >= DRAIN_BACKOFF_MIN * 2);
        assert!(at(2) > at(1));
        // Reached, which the doubling capped at 2^6 never did.
        assert!(at(8) >= DRAIN_BACKOFF_MAX);
        assert!(at(30) <= DRAIN_BACKOFF_MAX + DRAIN_BACKOFF_MAX / 5);
        // Ops that failed together do not all come back together.
        let waits: std::collections::HashSet<_> =
            (1..=20).map(|id| drain_backoff(8, false, id)).collect();
        assert!(waits.len() > 5, "{waits:?}");
        // A wait for a name stays short however often it repeats.
        assert_eq!(drain_backoff(30, true, 7), DRAIN_BACKOFF_MIN);
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
