//! The offline mutation queue (`pending_op`): writes the kernel has accepted but
//! the remote has not yet seen. Rebuilt into memory on mount and drained in row
//! order, so a child never drains before the parent that gives it a real uid.

use rusqlite::{OptionalExtension, params};

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
#[derive(Debug, Clone, Copy, Default)]
pub struct PendingCounts {
    /// Queued `create`/`revision` ops: files whose content is not on the remote.
    pub uploads: i64,
    /// Queued `mkdir`/`rename`/`trash` ops: metadata the remote has not been
    /// told about.
    pub changes: i64,
}

/// The outcome of folding freshly written bytes into a queued create.
#[derive(Debug, Clone)]
pub struct AttachedBlob {
    /// Row id of the create the bytes were attached to.
    pub id: i64,
    /// Blob the create held before, now orphaned.
    pub superseded: Option<String>,
}

impl Db {
    pub fn enqueue_op(&self, op: &PendingOp) -> Result<(i64, Option<String>)> {
        let conn = self.conn.lock();
        let superseded: Option<String> = if op_supersedes(&op.kind) {
            let blob: Option<String> = conn
                .query_row(
                    "SELECT blob_path FROM pending_op WHERE uid = ?1 AND kind = ?2",
                    params![op.uid, op.kind],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            conn.execute(
                "DELETE FROM pending_op WHERE uid = ?1 AND kind = ?2",
                params![op.uid, op.kind],
            )?;
            blob
        } else {
            None
        };
        conn.execute(
            "INSERT INTO pending_op
               (kind, uid, parent_uid, name, blob_path, meta_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                op.kind,
                op.uid,
                op.parent_uid,
                op.name,
                op.blob_path,
                op.meta_json,
                op.created_at
            ],
        )?;
        Ok((conn.last_insert_rowid(), superseded))
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
        conn.execute(
            "UPDATE pending_op
             SET blob_path = ?2, meta_json = ?3, attempts = 0, next_attempt_at = 0
             WHERE id = ?1",
            params![id, blob_path, meta_json],
        )?;
        Ok(Some(AttachedBlob { id, superseded }))
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
    pub fn pending_ops(&self) -> Result<Vec<PendingOp>> {
        let conn = self.conn.lock();
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

    /// How many queued ops there are of each kind, for `Response::Status`.
    ///
    /// Split because "3 uploads queued" has to mean three files whose bytes are
    /// not on the remote yet. A queued `mkdir`/`rename`/`trash` is also work the
    /// mount owes the server, but it carries no bytes and reporting it as an
    /// upload is simply untrue.
    pub fn pending_op_counts(&self) -> Result<PendingCounts> {
        let conn = self.conn.lock();
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
        Ok(PendingCounts { uploads, changes })
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
    pub fn record_op_failure(&self, id: i64, error: &str, next_attempt_at: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE pending_op
             SET attempts = attempts + 1, last_error = ?2, next_attempt_at = ?3
             WHERE id = ?1",
            params![id, error, next_attempt_at],
        )?;
        Ok(())
    }

}
