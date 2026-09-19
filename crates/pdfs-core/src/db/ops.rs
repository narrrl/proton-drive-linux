//! The offline mutation queue (`pending_op`): writes the kernel has accepted but
//! the remote has not yet seen. Rebuilt into memory on mount and drained in row
//! order, so a child never drains before the parent that gives it a real uid.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::Db;
use crate::Result;

/// The `kind` of a [`PendingOp`] that uploads a staged file as a new revision.
pub const OP_REVISION: &str = "revision";

/// The `kind` of a [`PendingOp`] that creates a file that so far exists only
/// locally, under a `local:` placeholder uid (offline.md Phase 3b).
///
/// The written bytes ride along on the same row rather than as a follow-on
/// [`OP_REVISION`]: draining the create mints the node's real uid, which would
/// leave a separate revision op addressed to a uid that no longer exists.
pub const OP_CREATE: &str = "create";

/// The `kind` of a [`PendingOp`] that creates a folder that so far exists only
/// locally. Ordering matters: a child's op is queued after its parent's and
/// [`Db::pending_ops`] replays by row id, so the parent has a real uid by the
/// time the child drains.
pub const OP_MKDIR: &str = "mkdir";

/// The `kind` of a [`PendingOp`] that gives a node a new name, a new parent, or
/// both — the queued form of `mv` (offline.md Phase 3b).
///
/// `parent_uid` and `name` hold the node's *desired end state*, not a delta, so
/// a second rename simply replaces the row (see [`Db::enqueue_op`]) and the
/// drain can compare them against the remote and skip whichever half already
/// matches. The parent may be a `local~` placeholder — moving a file into a
/// folder that was itself created offline — in which case it is rewritten by
/// that folder's own drain, exactly as for [`OP_CREATE`].
pub const OP_RENAME: &str = "rename";

/// Drain-time authority retained by a queued rename.
///
/// The optimistic local move rewrites the node's persisted ancestry to the
/// destination immediately. Keeping the original parent here prevents that
/// rewrite from erasing the shared tree that admitted the move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameMeta {
    pub original_parent_uid: String,
}

/// The `kind` of a [`PendingOp`] that trashes a node the server knows about
/// (offline.md Phase 3b).
///
/// A node that only ever existed locally never gets one of these: there is
/// nothing to trash remotely, so deleting it just drops its queued ops
/// ([`Db::delete_ops_for_uid`]).
pub const OP_TRASH: &str = "trash";

/// Whether at most one op of this `kind` may be queued per node, so that a newer
/// one replaces the older rather than queueing behind it.
///
/// True exactly of the kinds that describe a node's desired *end state* rather
/// than a step towards it: the newest revision already contains every earlier
/// one's bytes, the newest name is the only name wanted, and a trash subsumes
/// anything queued before it. A `create`/`mkdir` is the opposite — it is the one
/// thing that will ever make the node exist, so it must never be replaced.
pub fn op_supersedes(kind: &str) -> bool {
    matches!(kind, OP_REVISION | OP_RENAME | OP_TRASH)
}

/// The volume id given to a node that exists only on this machine, so far. A
/// real [`NodeUid`] is `{volume}~{link}`, so a placeholder is `local~<uuid>` and
/// round-trips through the same `Display`/parse path as any other uid.
///
/// Nothing bearing this volume may be handed to the API — it would 404. The
/// drain replaces it with the uid the server assigns.
pub const LOCAL_VOLUME: &str = "local";

/// A `next_attempt_at` far enough in the future that [`Db::next_due_op`] never
/// selects the op (ms since epoch, ≈ year 2223). Used to *park* a queued create
/// for a transient file — a browser's in-flight `*.crdownload`/`*.part`, an
/// editor's `*.swp` — so its bytes never upload while it wears that name. A
/// rename to the finished name un-parks it (see [`Db::set_create_hold`]), which
/// is the only moment the completed file is meant to reach Drive. Without the
/// park, every partial and every abandoned temp would upload and, on a
/// stall+resume, fork a `(sync-conflict)` copy (docs/BUGS.md B70).
pub const PARK_UNTIL: i64 = 8_000_000_000_000;

/// How long a create may stay parked before the park is treated as permanent
/// and the bytes are let through anyway (ms).
///
/// The park has exactly one exit: a rename from the transient name to the
/// finished one. Every writer that reaches that rename does so in seconds — an
/// atomic-write scratch file immediately, a download when it completes. A park
/// still standing an hour later belongs to a writer that will never perform
/// that rename: an editor that deletes its swap file instead, a download the
/// user abandoned, or a file that simply *is* named `report.tmp` and is never
/// going to be called anything else.
///
/// Before this existed such a row sat at [`PARK_UNTIL`] for the life of the
/// database, holding the only copy of the file's bytes in `staging/` and
/// counting against the queue while promising progress that could not come. The
/// sweep un-parks it instead of dropping it: bytes the user can see in the mount
/// are bytes the user expects on Drive, and invariant 1 of the drain (a staged
/// blob is dropped only once its op has landed) admits no other answer.
pub const PARK_EXPIRY_MS: i64 = 60 * 60 * 1000;

/// A mutation that has been accepted locally but not yet performed against the
/// API — the durable half of the write-back queue (offline.md Phase 3).
///
/// The daemon answers the FUSE call the moment this row and its staged blob are
/// on disk, so a `cp` into the mount runs at disk speed and the upload happens
/// behind it. That also makes an offline write succeed rather than EIO: the row
/// simply waits for the network.
#[derive(Debug, Clone)]
pub struct PendingOp {
    /// Row id, `0` on a value being inserted.
    pub id: i64,
    /// See [`OP_REVISION`], [`OP_CREATE`], [`OP_MKDIR`].
    pub kind: String,
    /// Node this op targets. For [`OP_CREATE`]/[`OP_MKDIR`] this is the
    /// `local~<uuid>` placeholder the node is known by until it drains.
    pub uid: String,
    /// Where the new node goes; only set for [`OP_CREATE`]/[`OP_MKDIR`]. May
    /// itself be a placeholder when the parent folder is also still queued, in
    /// which case it is rewritten when the parent drains.
    pub parent_uid: Option<String>,
    /// The new node's name; only set for [`OP_CREATE`]/[`OP_MKDIR`].
    pub name: Option<String>,
    /// Staged blob holding the bytes to upload.
    pub blob_path: Option<String>,
    /// Serialized [`StagedWrite`](crate::cache::StagedWrite).
    pub meta_json: Option<String>,
    /// When the op was queued (ms since epoch).
    pub created_at: i64,
    pub attempts: i64,
    pub last_error: Option<String>,
    /// Earliest ms at which to retry, for backoff.
    pub next_attempt_at: i64,
}

/// How much work the queue owes the server, by whether it carries bytes.
#[derive(Debug, Clone, Default)]
pub struct PendingCounts {
    /// Queued `create`/`revision` ops: files whose content is not on the remote.
    pub uploads: i64,
    /// Queued `mkdir`/`rename`/`trash` ops: metadata the remote has not been
    /// told about.
    pub changes: i64,
    /// Ops parked at [`PARK_UNTIL`]: a transient file's bytes, held back until
    /// the rename that gives it its finished name. They are queued but will
    /// never drain on their own, so counting them as "waiting to upload" would
    /// have the front-end promise progress that is not coming.
    pub parked: i64,
    /// Ops that have failed at least [`FAILING_ATTEMPTS`] times. A permanently
    /// failing op never wedges the queue — the backoff sees to that — which is
    /// exactly why it needs surfacing: it retries forever, invisibly.
    pub failing: i64,
    /// The most recent error text from a failing op, for the front-end to show
    /// instead of the bare count.
    pub last_error: Option<String>,
}

/// Failed attempts after which an op is reported as failing rather than merely
/// retrying. Six is where the backoff has reached its ceiling, so an op past it
/// is retrying at the slowest rate it ever will and has been for minutes.
pub const FAILING_ATTEMPTS: i64 = 6;

/// The outcome of folding freshly written bytes into a queued create.
#[derive(Debug, Clone)]
pub struct AttachedBlob {
    /// Row id of the create the bytes were attached to.
    pub id: i64,
    /// Blob the create held before, now orphaned.
    pub superseded: Option<String>,
}

impl Db {
    /// Whether a specific desired-state operation is still queued for a node.
    pub fn has_pending_op(&self, uid: &str, kind: &str) -> Result<bool> {
        let conn = self.read();
        conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pending_op WHERE uid = ?1 AND kind = ?2
             )",
            params![uid, kind],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Read an existing operation's metadata before a superseding enqueue.
    pub fn pending_op_meta(&self, uid: &str, kind: &str) -> Result<Option<String>> {
        let conn = self.read();
        conn.query_row(
            "SELECT meta_json FROM pending_op WHERE uid = ?1 AND kind = ?2",
            params![uid, kind],
            |row| row.get(0),
        )
        .optional()
        .map(|value| value.flatten())
        .map_err(Into::into)
    }

    pub fn enqueue_op(&self, op: &PendingOp) -> Result<(i64, Option<String>)> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let superseded: Option<String> = if op_supersedes(&op.kind) {
            let blob: Option<String> = tx
                .query_row(
                    "SELECT blob_path FROM pending_op WHERE uid = ?1 AND kind = ?2",
                    params![op.uid, op.kind],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            tx.execute(
                "DELETE FROM pending_op WHERE uid = ?1 AND kind = ?2",
                params![op.uid, op.kind],
            )?;
            blob
        } else {
            None
        };
        tx.execute(
            "INSERT INTO pending_op
               (kind, uid, parent_uid, name, blob_path, meta_json, created_at, next_attempt_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                op.kind,
                op.uid,
                op.parent_uid,
                op.name,
                op.blob_path,
                op.meta_json,
                op.created_at,
                op.next_attempt_at
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok((id, superseded))
    }

    /// Atomically replace every queued operation for `uid` and its queued
    /// descendants with one remote trash intent.
    ///
    /// Staged blob paths are returned only after the transaction commits. A
    /// failed trash insert therefore rolls the deletions back and leaves every
    /// prior operation owning its bytes.
    pub fn replace_ops_with_trash(
        &self,
        uid: &str,
        name: &str,
        created_at: i64,
    ) -> Result<(i64, Vec<String>)> {
        const SUBTREE: &str = "
            WITH RECURSIVE doomed(uid) AS (
              SELECT ?1
              UNION
              SELECT p.uid FROM pending_op p JOIN doomed d ON p.parent_uid = d.uid
            )";
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let blobs: Vec<String> = {
            let mut stmt = tx.prepare(&format!(
                "{SUBTREE}
                 SELECT blob_path FROM pending_op
                 WHERE uid IN (SELECT uid FROM doomed) AND blob_path IS NOT NULL"
            ))?;
            let rows = stmt.query_map(params![uid], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        tx.execute(
            &format!("{SUBTREE} DELETE FROM pending_op WHERE uid IN (SELECT uid FROM doomed)"),
            params![uid],
        )?;
        tx.execute(
            "INSERT INTO pending_op
               (kind, uid, parent_uid, name, blob_path, meta_json, created_at, next_attempt_at)
             VALUES (?1, ?2, NULL, ?3, NULL, NULL, ?4, 0)",
            params![OP_TRASH, uid, name, created_at],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok((id, blobs))
    }

    /// Point a queued create at the bytes that were just written to it, returning
    /// any blob it previously held so the caller can discard it.
    ///
    /// This is what `release` does for a file that only exists locally: the node
    /// has no uid to hang a revision op on yet, so the bytes ride on the create.
    /// Repeated writes before the drain simply replace the blob.
    ///
    /// Returns `Ok(None)` and touches nothing if the create has already drained —
    /// the caller must then queue an ordinary revision against the real uid.
    pub fn attach_blob_to_create(
        &self,
        uid: &str,
        blob_path: &str,
        meta_json: &str,
    ) -> Result<Option<AttachedBlob>> {
        let conn = self.conn.lock();
        let existing: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT id, blob_path FROM pending_op WHERE uid = ?1 AND kind = ?2",
                params![uid, OP_CREATE],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, superseded)) = existing else {
            return Ok(None);
        };
        // Clearing the backoff lets the fresh bytes be tried promptly — except
        // when the create is *parked* (a transient file, next_attempt_at at the
        // PARK_UNTIL sentinel). Attaching a new revision to a still-growing
        // download must not wake it; only the finalize rename does. So preserve a
        // park and reset an ordinary backoff.
        conn.execute(
            "UPDATE pending_op
             SET blob_path = ?2, meta_json = ?3, attempts = 0,
                 next_attempt_at = CASE WHEN next_attempt_at >= ?4
                                        THEN next_attempt_at ELSE 0 END
             WHERE id = ?1",
            params![id, blob_path, meta_json, PARK_UNTIL],
        )?;
        Ok(Some(AttachedBlob { id, superseded }))
    }

    /// Replace the sidecar of a queued op, for a baseline that has moved under
    /// it.
    ///
    /// Exists for one caller: when *our own* upload seals a new revision while a
    /// further write to the same node is already queued, that queued write's
    /// [`StagedWrite::based_on`](crate::cache::StagedWrite::based_on) still
    /// names the revision we just replaced. Left alone it would read as "another
    /// device changed this file" and divert the write into a conflict copy over
    /// nothing — a self-conflict. Restamping it is what keeps a chain of writes
    /// to one file a chain rather than a pile of copies.
    ///
    /// Deliberately narrow: only the sidecar changes, so the blob, the attempt
    /// count and the backoff all survive. Returns false when no such op is
    /// queued, which is the ordinary case.
    pub fn update_op_meta(&self, uid: &str, kind: &str, meta_json: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE pending_op SET meta_json = ?3 WHERE uid = ?1 AND kind = ?2",
            params![uid, kind, meta_json],
        )?;
        Ok(n > 0)
    }

    /// Point a queued create at a new parent and name, for a node renamed or
    /// moved before it ever reached the server.
    ///
    /// A `local~` uid means nothing to the API, so there is no rename call to
    /// make — the node is still only a queued intent, and rewriting that intent
    /// *is* the rename. Returns false when the create has already drained, in
    /// which case the node has a real uid and the caller must rename it there
    /// instead (offline.md Phase 3b).
    pub fn rewrite_op_target(&self, uid: &str, parent_uid: &str, name: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE pending_op SET parent_uid = ?2, name = ?3
             WHERE uid = ?1 AND kind IN (?4, ?5)",
            params![uid, parent_uid, name, OP_CREATE, OP_MKDIR],
        )?;
        Ok(n > 0)
    }

    /// Park or un-park a queued create/mkdir for `uid`, returning whether a row
    /// was touched.
    ///
    /// Parking sets `next_attempt_at` to [`PARK_UNTIL`] so [`Db::next_due_op`]
    /// skips it indefinitely; un-parking sets it due now. Used to keep a
    /// transient file's bytes off Drive until it is renamed to its finished name
    /// (docs/BUGS.md B70). Un-parking a create that was never parked is harmless:
    /// it just makes the create due immediately, which for a normal local node it
    /// already is.
    pub fn set_create_hold(&self, uid: &str, held: bool) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE pending_op SET next_attempt_at = ?2
             WHERE uid = ?1 AND kind IN (?3, ?4)",
            params![uid, if held { PARK_UNTIL } else { 0 }, OP_CREATE, OP_MKDIR],
        )?;
        Ok(n > 0)
    }

    /// Check if a queued create or mkdir op exists for `uid`.
    pub fn has_create_op(&self, uid: &str) -> Result<bool> {
        let conn = self.read();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pending_op WHERE uid = ?1 AND kind IN (?2, ?3)",
            params![uid, OP_CREATE, OP_MKDIR],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check whether *any* op of any kind is queued against `uid`.
    ///
    /// Used as a safety interlock by callers that are about to remove a node
    /// they only know about from a stale snapshot: a queued op means the user's
    /// bytes are still owed an upload, and dropping it would discard the staged
    /// blob that holds them (`docs/BUGS.md` B71).
    pub fn has_any_op(&self, uid: &str) -> Result<bool> {
        let conn = self.read();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pending_op WHERE uid = ?1",
            params![uid],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Drop every op targeting a node **or anything queued beneath it**,
    /// returning the staged blobs they held so the caller can delete them.
    ///
    /// Used when a node that only ever existed locally is deleted: there is
    /// nothing on the server to trash, so the queued work simply stops being
    /// wanted.
    ///
    /// The descent is what keeps the queue alive. Deleting a folder created
    /// offline drops the `mkdir` that would have given it a real uid, and any
    /// op still queued under that placeholder is then unreachable forever: it
    /// can never be attempted (its parent is a `local~` uid, so `op_is_ready`
    /// refuses it) and nothing is left to remap it. It would sit in the queue,
    /// and in the user's pending count, for the life of the database.
    ///
    /// Only `create`/`mkdir` ops carry a `parent_uid`, so for a file this
    /// recursion finds nothing and costs one query.
    pub fn delete_ops_for_uid(&self, uid: &str) -> Result<Vec<String>> {
        const SUBTREE: &str = "
            WITH RECURSIVE doomed(uid) AS (
              SELECT ?1
              UNION
              SELECT p.uid FROM pending_op p JOIN doomed d ON p.parent_uid = d.uid
            )";
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let blobs: Vec<String> = {
            let mut stmt = tx.prepare(&format!(
                "{SUBTREE}
                 SELECT blob_path FROM pending_op
                 WHERE uid IN (SELECT uid FROM doomed) AND blob_path IS NOT NULL"
            ))?;
            let rows = stmt.query_map(params![uid], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        tx.execute(
            &format!("{SUBTREE} DELETE FROM pending_op WHERE uid IN (SELECT uid FROM doomed)"),
            params![uid],
        )?;
        tx.commit()?;
        Ok(blobs)
    }

    /// Rewrite every queued op that points at a placeholder parent, once that
    /// parent has drained and has a real uid. Also moves the node rows whose
    /// parent column still names the placeholder, so listings keep resolving.
    pub fn remap_local_uid(&self, local: &str, real: &str) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE pending_op SET parent_uid = ?2 WHERE parent_uid = ?1",
            params![local, real],
        )?;
        tx.execute(
            "UPDATE nodes SET parent_uid = ?2 WHERE parent_uid = ?1",
            params![local, real],
        )?;
        tx.execute("DELETE FROM nodes WHERE uid = ?1", params![local])?;
        tx.commit()?;
        Ok(())
    }

    /// Every queued op, oldest first. The drain worker replays them in this order
    /// so a file's writes land in the order they were made.
    /// The oldest queued op that is due and not blocked, or `None`.
    ///
    /// This is the drain loop's per-iteration query, and it is deliberately not
    /// `pending_ops().find(...)`. That variant read every row — including each
    /// op's full `meta_json` — sorted them, and threw all but one away, once per
    /// drained op *and* once per retry, while holding the single connection
    /// mutex that every FUSE `lookup` also needs. A queue of a few thousand ops
    /// (an ordinary offline `cp -r`) made draining quadratic in the queue length.
    ///
    /// Both filters are in SQL so the row is decided by the database rather than
    /// materialized and rejected in Rust:
    ///
    /// * `next_attempt_at <= now` — the backoff.
    /// * a `local~` parent is skipped. A node created inside a folder that was
    ///   itself created offline cannot be sent anywhere until that folder is
    ///   real. Ops replay in queue order so the parent normally drains first;
    ///   this matters when the parent is backing off, where the child must wait
    ///   rather than burn its own retries.
    ///
    /// `id` is `INTEGER PRIMARY KEY`, i.e. the rowid, so `ORDER BY id LIMIT 1`
    /// walks the table in insertion order and stops at the first match — no
    /// sort, and no separate index to maintain. The common case (something is
    /// due) exits on the first row.
    pub fn next_due_op(&self, now: i64) -> Result<Option<PendingOp>> {
        let conn = self.read();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT id, kind, uid, parent_uid, name, blob_path, meta_json, created_at,
                    attempts, last_error, next_attempt_at
             FROM pending_op
             WHERE next_attempt_at <= ?1
               AND (parent_uid IS NULL OR substr(parent_uid, 1, {n}) <> '{v}~')
             ORDER BY id LIMIT 1",
            v = LOCAL_VOLUME,
            n = LOCAL_VOLUME.len() + 1,
        ))?;
        let op = stmt
            .query_row(params![now], |r| {
                Ok(PendingOp {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    uid: r.get(2)?,
                    parent_uid: r.get(3)?,
                    name: r.get(4)?,
                    blob_path: r.get(5)?,
                    meta_json: r.get(6)?,
                    created_at: r.get(7)?,
                    attempts: r.get(8)?,
                    last_error: r.get(9)?,
                    next_attempt_at: r.get(10)?,
                })
            })
            .optional()?;
        Ok(op)
    }

    /// Take the oldest due op for this worker, marking it claimed so no other
    /// drain worker takes it, or `None` when there is nothing to do.
    ///
    /// The drain is several workers over one queue, which is only safe because
    /// the claim is a column: a row is offered to exactly one worker, and the
    /// worker that took it is the only one that may retire, defer or fail it.
    ///
    /// Two exclusions on top of [`next_due_op`](Self::next_due_op)'s:
    ///
    /// * `claimed_at = 0` — not already in flight somewhere else.
    /// * no *other* claimed op shares this `uid`. Ordering only has to hold per
    ///   node, but there it has to hold absolutely: a queued rename and a queued
    ///   revision of one file must land in the order they were made, and two
    ///   workers on one node would also race over its staged blob and its
    ///   pending entry. `ORDER BY id` plus this exclusion gives per-uid serial
    ///   order for free — the older op is claimed first and the newer one is
    ///   invisible until it retires.
    ///
    /// Select and mark are one transaction, so two workers cannot both see the
    /// row as unclaimed.
    pub fn claim_next_due_op(&self, now: i64) -> Result<Option<PendingOp>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let op = {
            let mut stmt = tx.prepare_cached(&format!(
                "SELECT id, kind, uid, parent_uid, name, blob_path, meta_json, created_at,
                        attempts, last_error, next_attempt_at
                 FROM pending_op
                 WHERE next_attempt_at <= ?1
                   AND claimed_at = 0
                   AND (parent_uid IS NULL OR substr(parent_uid, 1, {n}) <> '{v}~')
                   AND uid NOT IN (SELECT uid FROM pending_op WHERE claimed_at <> 0)
                 ORDER BY id LIMIT 1",
                v = LOCAL_VOLUME,
                n = LOCAL_VOLUME.len() + 1,
            ))?;
            stmt.query_row(params![now], |r| {
                Ok(PendingOp {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    uid: r.get(2)?,
                    parent_uid: r.get(3)?,
                    name: r.get(4)?,
                    blob_path: r.get(5)?,
                    meta_json: r.get(6)?,
                    created_at: r.get(7)?,
                    attempts: r.get(8)?,
                    last_error: r.get(9)?,
                    next_attempt_at: r.get(10)?,
                })
            })
            .optional()?
        };
        if let Some(op) = op.as_ref() {
            tx.execute(
                "UPDATE pending_op SET claimed_at = ?2 WHERE id = ?1",
                params![op.id, now.max(1)],
            )?;
        }
        tx.commit()?;
        Ok(op)
    }

    /// Give a claimed op back to the queue, whatever became of the attempt.
    ///
    /// Called on every path out of a drain attempt rather than only the failing
    /// ones: a handler that retires its own row leaves nothing to release (the
    /// `UPDATE` matches nothing), and a handler that returns without retiring
    /// *and* without failing would otherwise leave the row claimed by a worker
    /// that has moved on — invisible to every worker, forever.
    pub fn release_op_claim(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE pending_op SET claimed_at = 0 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Drop every claim in the table.
    ///
    /// A claim describes a worker in *this* process, so one surviving on disk
    /// means the previous run died holding it. The single-writer lock
    /// ([`Db::open`]) is what makes that inference safe: nobody else can be
    /// draining this queue while we are opening it.
    pub fn clear_op_claims(&self) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "UPDATE pending_op SET claimed_at = 0 WHERE claimed_at <> 0",
            [],
        )?)
    }

    /// The earliest `next_attempt_at` among all queued ops, or `None` if the
    /// queue is empty. Used by the drain loop to sleep exactly until the next
    /// debounced or backed-off op becomes eligible rather than waiting the full
    /// idle-poll interval.
    ///
    /// Claimed ops are excluded: another worker is already on them, so they are
    /// not work this one is waiting for, and counting them would have an idle
    /// worker spin on a row it cannot have.
    pub fn earliest_due_at(&self) -> Result<Option<i64>> {
        let conn = self.read();
        let ts: Option<i64> = conn
            .query_row(
                &format!(
                    "SELECT MIN(next_attempt_at) FROM pending_op \
                     WHERE claimed_at = 0 \
                       AND (parent_uid IS NULL OR substr(parent_uid, 1, {n}) <> '{v}~')",
                    v = LOCAL_VOLUME,
                    n = LOCAL_VOLUME.len() + 1,
                ),
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(ts)
    }

    /// Every queued op, oldest first. For status read-outs and the CLI — the
    /// drain wants [`next_due_op`](Self::next_due_op) instead.
    pub fn pending_ops(&self) -> Result<Vec<PendingOp>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT id, kind, uid, parent_uid, name, blob_path, meta_json, created_at,
                    attempts, last_error, next_attempt_at
             FROM pending_op ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PendingOp {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    uid: r.get(2)?,
                    parent_uid: r.get(3)?,
                    name: r.get(4)?,
                    blob_path: r.get(5)?,
                    meta_json: r.get(6)?,
                    created_at: r.get(7)?,
                    attempts: r.get(8)?,
                    last_error: r.get(9)?,
                    next_attempt_at: r.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every parked create/mkdir, oldest first, for the park sweep.
    ///
    /// Parked rows are invisible to [`next_due_op`](Self::next_due_op) by
    /// construction, so nothing else in the drain ever looks at them. This is
    /// the one query that does.
    pub fn parked_create_ops(&self) -> Result<Vec<PendingOp>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT id, kind, uid, parent_uid, name, blob_path, meta_json, created_at,
                    attempts, last_error, next_attempt_at
             FROM pending_op
             WHERE next_attempt_at >= ?1 AND kind IN (?2, ?3)
             ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![PARK_UNTIL, OP_CREATE, OP_MKDIR], |r| {
                Ok(PendingOp {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    uid: r.get(2)?,
                    parent_uid: r.get(3)?,
                    name: r.get(4)?,
                    blob_path: r.get(5)?,
                    meta_json: r.get(6)?,
                    created_at: r.get(7)?,
                    attempts: r.get(8)?,
                    last_error: r.get(9)?,
                    next_attempt_at: r.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// How many queued ops there are of each kind, for `Response::Status`.
    ///
    /// Split because "3 uploads queued" has to mean three files whose bytes are
    /// not on the remote yet. A queued `mkdir`/`rename`/`trash` is also work the
    /// mount owes the server, but it carries no bytes and reporting it as an
    /// upload is simply untrue.
    pub fn pending_op_counts(&self) -> Result<PendingCounts> {
        let conn = self.read();
        let uploads = conn.query_row(
            "SELECT COUNT(*) FROM pending_op WHERE kind IN (?1, ?2)",
            params![OP_REVISION, OP_CREATE],
            |r| r.get(0),
        )?;
        let changes = conn.query_row(
            "SELECT COUNT(*) FROM pending_op WHERE kind NOT IN (?1, ?2)",
            params![OP_REVISION, OP_CREATE],
            |r| r.get(0),
        )?;
        let parked = conn.query_row(
            "SELECT COUNT(*) FROM pending_op WHERE next_attempt_at >= ?1",
            params![PARK_UNTIL],
            |r| r.get(0),
        )?;
        let failing = conn.query_row(
            "SELECT COUNT(*) FROM pending_op WHERE attempts >= ?1",
            params![FAILING_ATTEMPTS],
            |r| r.get(0),
        )?;
        let last_error = conn
            .query_row(
                "SELECT last_error FROM pending_op
                  WHERE attempts >= ?1 AND last_error IS NOT NULL
                  ORDER BY attempts DESC LIMIT 1",
                params![FAILING_ATTEMPTS],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(PendingCounts {
            uploads,
            changes,
            parked,
            failing,
            last_error,
        })
    }

    /// Nothing bearing this volume may be handed to the API — it would 404. The
    /// drain replaces it with the uid the server assigns.
    pub const LOCAL_VOLUME: &str = "local";

    /// Drop a queued op, once its upload has actually landed.
    pub fn delete_op(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM pending_op WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Record a failed attempt and when to next try. Leaves the row in place —
    /// the staged bytes are still the only copy of the user's write.
    ///
    /// Clears any access-deferral window: whatever the op was waiting for has
    /// now been reported, so the next uncleared deferral starts its own window
    /// rather than escalating again on the following recheck.
    pub fn record_op_failure(&self, id: i64, error: &str, next_attempt_at: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE pending_op
             SET attempts = attempts + 1, last_error = ?2, next_attempt_at = ?3,
                 access_deferred_since = 0
             WHERE id = ?1",
            params![id, error, next_attempt_at],
        )?;
        Ok(())
    }

    /// Defer a due operation without recording a failed remote attempt.
    ///
    /// Access checks happen before the drain calls Proton, so a locally denied
    /// operation has not failed remotely and must not consume a retry. Preserve
    /// the long-lived transient-create sentinel if this is ever called for one.
    pub fn defer_op_without_attempt(&self, id: i64, next_attempt_at: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE pending_op
             SET next_attempt_at = CASE
                 WHEN next_attempt_at >= ?3 THEN next_attempt_at
                 ELSE ?2
             END
             WHERE id = ?1",
            params![id, next_attempt_at, PARK_UNTIL],
        )?;
        Ok(())
    }

    /// Defer an op the local access check refused, and report how long it has
    /// been refused for.
    ///
    /// Identical to [`defer_op_without_attempt`](Self::defer_op_without_attempt)
    /// on the retry clock, plus the one thing that was missing: the first
    /// deferral of a run stamps `access_deferred_since`, and every deferral
    /// returns that stamp. A caller can therefore tell a deferral that is one
    /// recheck old from one that has been repeating unchanged for days, which is
    /// the difference between a permission the user is mid-way through changing
    /// and a queue that will never drain.
    ///
    /// Returns the ms at which the current run of deferrals began — `now` on the
    /// first one.
    pub fn defer_op_for_access(&self, id: i64, now: i64, next_attempt_at: i64) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE pending_op
             SET next_attempt_at = CASE
                 WHEN next_attempt_at >= ?4 THEN next_attempt_at
                 ELSE ?3
             END,
             access_deferred_since = CASE
                 WHEN access_deferred_since = 0 THEN ?2
                 ELSE access_deferred_since
             END
             WHERE id = ?1",
            params![id, now, next_attempt_at, PARK_UNTIL],
        )?;
        let since = conn
            .query_row(
                "SELECT access_deferred_since FROM pending_op WHERE id = ?1",
                params![id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        // A row that drained out from under this call leaves nothing to measure;
        // reporting `now` says "this run just started", which is the reading that
        // never escalates something that no longer exists.
        Ok(since.filter(|since| *since != 0).unwrap_or(now))
    }

    /// Forget an op's access-deferral window, after an attempt the access check
    /// let through. The op is no longer blocked, so a later deferral is the
    /// start of a new run rather than the continuation of an old one.
    pub fn clear_op_access_deferral(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE pending_op SET access_deferred_since = 0
             WHERE id = ?1 AND access_deferred_since <> 0",
            params![id],
        )?;
        Ok(())
    }

    /// Every staged blob the queue still refers to.
    ///
    /// The startup staging reconcile subtracts this from what is actually in
    /// `staging/`: the difference is bytes the kernel was told were written and
    /// that nothing is going to upload.
    pub fn op_blob_paths(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.read();
        let mut stmt =
            conn.prepare("SELECT blob_path FROM pending_op WHERE blob_path IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Record that this daemon sealed `revision_id` on `uid`, and prune entries
    /// past [`OWN_SEALED_TTL_MS`].
    ///
    /// One row per node, overwritten as the node advances. See
    /// [`own_sealed_rev`](Self::own_sealed_rev) for what reads it.
    pub fn set_own_sealed_rev(&self, uid: &str, revision_id: &str, now_ms: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO own_sealed_rev (uid, revision_id, sealed_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(uid) DO UPDATE SET
               revision_id = excluded.revision_id,
               sealed_at   = excluded.sealed_at",
            params![uid, revision_id, now_ms],
        )?;
        conn.execute(
            "DELETE FROM own_sealed_rev WHERE sealed_at < ?1",
            params![now_ms - OWN_SEALED_TTL_MS],
        )?;
        Ok(())
    }

    /// The last revision of `uid` this daemon sealed itself, if it is still
    /// within [`OWN_SEALED_TTL_MS`].
    ///
    /// The drain asks before treating a moved-on remote as a conflict: a
    /// revision we sealed means one writer stalled and resumed, which
    /// supersedes rather than forking a `(sync-conflict)` copy (docs/BUGS.md
    /// B70 layer B).
    pub fn own_sealed_rev(&self, uid: &str, now_ms: i64) -> Result<Option<String>> {
        let conn = self.read();
        Ok(conn
            .query_row(
                "SELECT revision_id FROM own_sealed_rev
                  WHERE uid = ?1 AND sealed_at >= ?2",
                params![uid, now_ms - OWN_SEALED_TTL_MS],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Forget what we sealed on `uid` — it has been trashed, so there is no
    /// revision left to chain onto.
    pub fn clear_own_sealed_rev(&self, uid: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM own_sealed_rev WHERE uid = ?1", params![uid])?;
        Ok(())
    }
}

/// How long a revision this daemon sealed stays recognisable as its own.
///
/// Bounds a table that would otherwise grow with every write for the life of
/// the install. A week is far longer than any stall→resume — a browser download
/// that pauses and continues, an editor that reopens its handle — and short
/// enough that the table tracks recent work rather than accumulating history.
pub const OWN_SEALED_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
