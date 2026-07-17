//! Unified SQLite metadata cache — the single persistence layer behind FUSE
//! inode bookkeeping, full-text search, content-cache LRU tracking, and pins.
//!
//! Only the daemon (`pdfs-fuse`) opens this for writes; the GUI and CLI reach
//! the same data through the control socket. The connection is wrapped in a
//! `Mutex` because the FUSE callbacks are synchronous and already serialize
//! behind the `State` lock, so a connection pool would be overkill.
//!
//! This module is the P0 foundation: it opens the database, enables WAL, and
//! applies the forward-only schema migrations. Write-through of nodes, the
//! event cursor, FTS, and the cache index land in later phases on this schema.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::control::{ActivityEntry, ActivityKind};
use crate::localindex::LocalEntry;

/// Current schema version. Bump on every forward migration added below.
const SCHEMA_VERSION: i64 = 12;

/// How many activity rows to keep. Older rows are pruned on insert, so the feed
/// stays a bounded "recent history" rather than growing without limit.
const ACTIVITY_KEEP: i64 = 2000;

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

/// Size the WAL is truncated back to after a checkpoint. Comfortably above the
/// steady-state working set (a few MB), so the truncation only claws back the
/// outliers rather than fighting the normal write path for disk.
const WAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// A photo whose thumbnail state is not known yet: it has never been asked for.
pub const THUMB_UNKNOWN: i64 = 0;
/// A thumbnail exists for this photo — served by the server, or generated locally
/// from the full file when the server had none.
pub const THUMB_HAVE: i64 = 1;
/// This photo can never be given a thumbnail: the server has none and the bytes
/// could not be decoded locally either. Never retried.
pub const THUMB_NONE: i64 = 2;

/// Below this length the trigram tokenizer indexes nothing (it needs 3-char
/// grams), so short queries fall back to a `LIKE` scan over `nodes.name`.
const TRIGRAM_MIN: usize = 3;

/// A node loaded back from the database, paired with whether its directory
/// listing was complete (`listed`) when last persisted. Only meaningful for
/// folders; always `false` for files.
pub struct StoredNode {
    pub node: Node,
    pub listed: bool,
}

/// One full-text search match: the stored [`Node`] plus its mountpoint-relative
/// path (`/`-joined, root excluded) so the front-end can navigate to or open it.
pub struct SearchHit {
    pub node: Node,
    pub path: String,
}

/// One photo of the persisted timeline. The timeline itself is server-ordered
/// (newest first) and stored with that order in `seq`; `ratio` and `thumb_state`
/// are locally learned and survive a refresh of the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredPhoto {
    pub uid: String,
    pub capture_time: i64,
    pub name: Option<String>,
    /// Aspect ratio (w/h), known once a thumbnail has been decoded.
    pub ratio: Option<f64>,
    /// One of [`THUMB_UNKNOWN`] / [`THUMB_HAVE`] / [`THUMB_NONE`].
    pub thumb_state: i64,
}

/// One trashed node, as persisted for the Trash page. Trashed nodes live outside
/// the mounted tree, so a uid is their only handle.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredTrash {
    pub uid: String,
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    pub mtime: i64,
}

/// The one Proton Drive Device this machine is registered as (devices.md).
/// Cached so restarts reuse the same device rather than creating a new one.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredDevice {
    pub uid: String,
    pub share_id: String,
    pub root_uid: String,
    pub name: String,
    pub created: i64,
}

/// One local folder the user added to this device's sync set.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredSyncFolder {
    pub id: i64,
    pub local_path: String,
    pub remote_uid: String,
    pub remote_share_id: String,
    /// `mirror` (full local copy, two-way synced) or `ondemand` (FUSE mount).
    pub mode: String,
    /// A mode the user asked for that could not be applied on the spot — the
    /// folder was mid-pass, or had un-uploaded changes. The engine applies it
    /// once the folder is safe to switch, so the request is queued rather than
    /// rejected. `None` when the folder is where the user wants it.
    pub pending_mode: Option<String>,
    /// `idle` | `syncing` | `error` | `conflict`.
    pub state: String,
    pub last_sync: i64,
}

/// One per-file sync baseline row: what a path looked like on both sides at the
/// last successful sync, so the next reconcile can tell which side changed
/// (devices.md Phase 2). `remote_rev`/`remote_hash` hold the remote signature —
/// its modification time and size as strings — since no cheap content hash is
/// exposed; change detection is `(mtime, size)` on each side.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredSyncEntry {
    pub rel_path: String,
    pub remote_uid: Option<String>,
    pub local_mtime: i64,
    pub local_size: i64,
    pub remote_rev: Option<String>,
    pub remote_hash: Option<String>,
}

/// Handle to the unified metadata database.
///
/// Cheap to wrap in an `Arc`; clone the `Arc`, not this. All access goes through
/// the inner `Mutex<Connection>`.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if absent) the database at `path`, enable WAL, and bring
    /// the schema up to [`SCHEMA_VERSION`].
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        // WAL: readers never block the single writer. NORMAL sync is the
        // standard durability/throughput tradeoff for WAL.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Hand the WAL's disk back after a checkpoint. Without a limit SQLite
        // reuses the file in place but never shrinks it, so a single large
        // transaction (a full-index FTS rebuild in `local_finish_scan` is the
        // one that reaches this size) leaves the WAL at its high-water mark
        // forever — a multi-GB file next to a database two orders of magnitude
        // smaller. Checkpointing is unaffected; only the file is truncated back.
        conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory database. For tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Run forward-only migrations from the stored `schema_version` up to
    /// [`SCHEMA_VERSION`]. Each step is wrapped in its own transaction.
    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();

        // `sync_state` is the key/value table holding `schema_version` and the
        // event cursor (later). Create it first so we can read the version.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT);",
        )?;

        let current: i64 = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        if current >= SCHEMA_VERSION {
            return Ok(());
        }

        let tx = conn.transaction()?;
        if current < 1 {
            tx.execute_batch(MIGRATION_V1)?;
        }
        if current < 2 {
            tx.execute_batch(MIGRATION_V2)?;
        }
        if current < 3 {
            tx.execute_batch(MIGRATION_V3)?;
        }
        if current < 4 {
            tx.execute_batch(MIGRATION_V4)?;
        }
        if current < 5 {
            tx.execute_batch(MIGRATION_V5)?;
        }
        if current < 6 {
            tx.execute_batch(MIGRATION_V6)?;
        }
        if current < 7 {
            tx.execute_batch(MIGRATION_V7)?;
        }
        if current < 8 {
            tx.execute_batch(MIGRATION_V8)?;
        }
        if current < 9 {
            tx.execute_batch(MIGRATION_V9)?;
        }
        if current < 10 {
            tx.execute_batch(MIGRATION_V10)?;
        }
        if current < 11 {
            tx.execute_batch(MIGRATION_V11)?;
        }
        if current < 12 {
            tx.execute_batch(MIGRATION_V12)?;
        }
        tx.execute(
            "INSERT INTO sync_state (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Run a closure with the locked connection. Escape hatch for callers in
    /// later phases until typed query methods are added.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    /// Write-through a node from the in-memory cache. Indexed columns are kept
    /// for query/search/eviction; the full [`Node`] is stored as JSON so the
    /// hot-cache maps rehydrate losslessly on the next mount. `listed` is never
    /// changed here — it is owned by [`set_listed`](Self::set_listed).
    pub fn upsert_node(&self, node: &Node) -> Result<()> {
        let json = serde_json::to_string(node)?;
        let uid = node.uid.to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO nodes
               (uid, parent_uid, name, is_dir, size, mtime, trashed, node_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(uid) DO UPDATE SET
               parent_uid = excluded.parent_uid,
               name       = excluded.name,
               is_dir     = excluded.is_dir,
               size       = excluded.size,
               mtime      = excluded.mtime,
               trashed    = excluded.trashed,
               node_json  = excluded.node_json",
            params![
                uid,
                node.parent_uid.as_ref().map(|u| u.to_string()),
                node.name,
                node.is_folder() as i64,
                node_size(node),
                node.modification_time,
                node.trashed as i64,
                json,
            ],
        )?;
        // FTS5 has no UPSERT, so refresh the row by delete-then-insert. Trashed
        // nodes are kept out of the index entirely so they never surface in
        // search results.
        tx.execute("DELETE FROM nodes_fts WHERE uid = ?1", params![uid])?;
        if !node.trashed {
            tx.execute(
                "INSERT INTO nodes_fts (uid, name) VALUES (?1, ?2)",
                params![uid, node.name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Write-through multiple nodes in a single database transaction for performance.
    pub fn upsert_nodes(&self, nodes: &[Node]) -> Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for node in nodes {
            let json = serde_json::to_string(node)?;
            let uid = node.uid.to_string();
            tx.execute(
                "INSERT INTO nodes
                   (uid, parent_uid, name, is_dir, size, mtime, trashed, node_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(uid) DO UPDATE SET
                   parent_uid = excluded.parent_uid,
                   name       = excluded.name,
                   is_dir     = excluded.is_dir,
                   size       = excluded.size,
                   mtime      = excluded.mtime,
                   trashed    = excluded.trashed,
                   node_json  = excluded.node_json",
                params![
                    uid,
                    node.parent_uid.as_ref().map(|u| u.to_string()),
                    node.name,
                    node.is_folder() as i64,
                    node_size(node),
                    node.modification_time,
                    node.trashed as i64,
                    json,
                ],
            )?;
            tx.execute("DELETE FROM nodes_fts WHERE uid = ?1", params![uid])?;
            if !node.trashed {
                tx.execute(
                    "INSERT INTO nodes_fts (uid, name) VALUES (?1, ?2)",
                    params![uid, node.name],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop a node row (delete or trash from the hot cache). Children rows are
    /// not cascaded here; the daemon forgets a whole subtree node-by-node.
    pub fn delete_node(&self, uid: &NodeUid) -> Result<()> {
        let uid = uid.to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM nodes WHERE uid = ?1", params![uid])?;
        tx.execute("DELETE FROM nodes_fts WHERE uid = ?1", params![uid])?;
        tx.commit()?;
        Ok(())
    }

    /// Full-text search over node names, newest schema's trigram index giving
    /// substring (not just prefix) matches. Returns up to `limit` non-trashed
    /// hits, each with its mountpoint-relative path resolved. Queries shorter
    /// than [`TRIGRAM_MIN`] fall back to a `LIKE` scan since trigram indexes
    /// nothing below 3 chars.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let rows: Vec<(String, String)> = if query.chars().count() < TRIGRAM_MIN {
            let pat = format!("%{}%", like_escape(query));
            let mut stmt = conn.prepare(
                "SELECT node_json, uid FROM nodes
                 WHERE name LIKE ?1 ESCAPE '\\' AND trashed = 0 AND node_json IS NOT NULL
                 ORDER BY name LIMIT ?2",
            )?;
            collect_pairs(stmt.query_map(params![pat, limit as i64], pair)?)?
        } else {
            // Escape double quotes and quote each term, then combine with AND so
            // all terms must match but can appear in any order or position.
            let phrase = query
                .split_whitespace()
                .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut stmt = conn.prepare(
                "SELECT n.node_json, n.uid
                 FROM nodes_fts f JOIN nodes n ON n.uid = f.uid
                 WHERE f.name MATCH ?1 AND n.trashed = 0 AND n.node_json IS NOT NULL
                 ORDER BY f.rank LIMIT ?2",
            )?;
            collect_pairs(stmt.query_map(params![phrase, limit as i64], pair)?)?
        };

        let mut hits = Vec::with_capacity(rows.len());
        for (json, uid) in rows {
            let node: Node = serde_json::from_str(&json)?;
            let path = path_of(&conn, &uid)?;
            hits.push(SearchHit { node, path });
        }
        Ok(hits)
    }

    /// Mark (or unmark) a folder's child listing as complete. A listed folder
    /// rehydrates its `children` map on mount even when empty; an unlisted one
    /// re-enumerates from the remote on next access.
    pub fn set_listed(&self, uid: &NodeUid, listed: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET listed = ?2 WHERE uid = ?1",
            params![uid.to_string(), listed as i64],
        )?;
        Ok(())
    }

    /// Load every persisted node for cold-start hydration of the `State` maps.
    pub fn load_all(&self) -> Result<Vec<StoredNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT node_json, listed FROM nodes WHERE node_json IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            let json: String = row.get(0)?;
            let listed: i64 = row.get(1)?;
            Ok((json, listed != 0))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (json, listed) = row?;
            let node: Node = serde_json::from_str(&json)?;
            out.push(StoredNode { node, listed });
        }
        Ok(out)
    }

    /// Load one persisted node back by uid. Used to recover the My Files root
    /// when the API is unreachable, so the mount can still serve the cached tree
    /// (offline.md Phase 1).
    pub fn node_by_uid(&self, uid: &str) -> Result<Option<Node>> {
        let conn = self.conn.lock().unwrap();
        let json: Option<String> = conn
            .query_row(
                "SELECT node_json FROM nodes WHERE uid = ?1 AND node_json IS NOT NULL",
                params![uid],
                |r| r.get(0),
            )
            .optional()?;
        match json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Read the persisted incremental-sync cursor (a `DriveEventId`), if any.
    /// The daemon resumes from this on restart instead of reseeding to the
    /// server head, so changes made while unmounted are still applied (P2).
    pub fn get_event_cursor(&self) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'event_cursor'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Persist the incremental-sync cursor after a batch of events is applied.
    pub fn set_event_cursor(&self, cursor: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES ('event_cursor', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![cursor],
        )?;
        Ok(())
    }

    /// Read a `sync_state` value as a string.
    pub fn state_str(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Write a `sync_state` string value.
    pub fn set_state_str(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Queue an op to be performed by the drain worker, returning its row id.
    ///
    /// One *revision* op per node: a second write to the same file before the
    /// first has drained replaces it, since the newer blob already contains
    /// everything the older one did. The superseded blob's path is returned so
    /// the caller can delete it.
    ///
    /// Rows supersede only their own kind, and only the kinds
    /// [`op_supersedes`] names. A create op for the same uid must survive —
    /// dropping it would leave a file that exists nowhere but this machine with
    /// nothing left to create it. (Writes to a node that is itself still queued
    /// fold into the create row instead; see [`Db::attach_blob_to_create`].)
    pub fn enqueue_op(&self, op: &PendingOp) -> Result<(i64, Option<String>)> {
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let mut conn = self.conn.lock().unwrap();
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
        let mut conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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

    /// Drop a queued op, once its upload has actually landed.
    pub fn delete_op(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM pending_op WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Record a failed attempt and when to next try. Leaves the row in place —
    /// the staged bytes are still the only copy of the user's write.
    pub fn record_op_failure(&self, id: i64, error: &str, next_attempt_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pending_op
             SET attempts = attempts + 1, last_error = ?2, next_attempt_at = ?3
             WHERE id = ?1",
            params![id, error, next_attempt_at],
        )?;
        Ok(())
    }

    /// Read a `sync_state` value as an integer (the freshness stamps of the
    /// photos timeline and the trash listing are kept there).
    pub fn state_i64(&self, key: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|v| v.parse().ok()))
    }

    /// Write a `sync_state` integer value.
    pub fn set_state_i64(&self, key: &str, value: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    /// Drop a `sync_state` key, so whatever it stamped counts as never fetched.
    pub fn clear_state(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sync_state WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Replace the persisted photos timeline with `items` (newest first), keeping
    /// what was locally learned about the photos that are still there: a ratio or
    /// a thumbnail verdict costs a download to rediscover, while the server order
    /// and capture times are cheap and authoritative.
    ///
    /// Photos no longer in the timeline are dropped, so a deletion on another
    /// client doesn't leave a ghost tile behind.
    pub fn photos_replace(&self, items: &[(String, i64, Option<String>)]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let learned: HashMap<String, (Option<f64>, i64)> = {
            let mut stmt = tx.prepare("SELECT uid, ratio, thumb_state FROM photos")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        tx.execute("DELETE FROM photos", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO photos (uid, capture_time, name, ratio, thumb_state, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (seq, (uid, capture_time, name)) in items.iter().enumerate() {
                let (ratio, thumb_state) =
                    learned.get(uid).copied().unwrap_or((None, THUMB_UNKNOWN));
                stmt.execute(params![
                    uid,
                    capture_time,
                    name,
                    ratio,
                    thumb_state,
                    seq as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// One page of the persisted timeline, newest first.
    pub fn photos_page(&self, offset: usize, limit: usize) -> Result<Vec<StoredPhoto>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT uid, capture_time, name, ratio, thumb_state FROM photos
             ORDER BY seq LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(params![limit as i64, offset as i64], |r| {
            Ok(StoredPhoto {
                uid: r.get(0)?,
                capture_time: r.get(1)?,
                name: r.get(2)?,
                ratio: r.get(3)?,
                thumb_state: r.get(4)?,
            })
        })?;
        let mut photos = Vec::new();
        for row in rows {
            photos.push(row?);
        }
        Ok(photos)
    }

    /// The stored photos for `uids`, in no particular order. Used by the thumbnail
    /// path, which needs each photo's capture time (the cache validity tag) and
    /// its thumbnail verdict.
    pub fn photos_by_uid(&self, uids: &[String]) -> Result<Vec<StoredPhoto>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let placeholders = vec!["?"; uids.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT uid, capture_time, name, ratio, thumb_state FROM photos
             WHERE uid IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(uids), |r| {
            Ok(StoredPhoto {
                uid: r.get(0)?,
                capture_time: r.get(1)?,
                name: r.get(2)?,
                ratio: r.get(3)?,
                thumb_state: r.get(4)?,
            })
        })?;
        let mut photos = Vec::new();
        for row in rows {
            photos.push(row?);
        }
        Ok(photos)
    }

    /// Record what a thumbnail attempt learned: whether the photo now has one
    /// ([`THUMB_HAVE`] / [`THUMB_NONE`]), and its aspect ratio if the pixels were
    /// seen. A `None` ratio leaves any previously learned one alone.
    pub fn photo_set_thumb(&self, uid: &str, state: i64, ratio: Option<f64>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE photos SET thumb_state = ?2, ratio = COALESCE(?3, ratio) WHERE uid = ?1",
            params![uid, state, ratio],
        )?;
        Ok(())
    }

    /// Number of photos in the persisted timeline.
    pub fn photos_count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }

    /// Replace the persisted trash listing.
    pub fn trash_replace(&self, items: &[StoredTrash]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM trash", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO trash (uid, name, is_dir, size, mtime) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for item in items {
                stmt.execute(params![
                    item.uid,
                    item.name,
                    item.is_dir as i64,
                    item.size,
                    item.mtime
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The persisted trash listing, folders first then by name — the order the
    /// Trash page shows it in.
    pub fn trash_list(&self) -> Result<Vec<StoredTrash>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT uid, name, is_dir, size, mtime FROM trash
             ORDER BY is_dir DESC, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StoredTrash {
                uid: r.get(0)?,
                name: r.get(1)?,
                is_dir: r.get::<_, i64>(2)? != 0,
                size: r.get(3)?,
                mtime: r.get(4)?,
            })
        })?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    // ---- device sync (devices.md) -----------------------------------------

    /// The registered device for this machine, if one has been created/cached.
    pub fn device_get(&self) -> Result<Option<StoredDevice>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT uid, share_id, root_uid, name, created FROM device LIMIT 1",
            [],
            |r| {
                Ok(StoredDevice {
                    uid: r.get(0)?,
                    share_id: r.get(1)?,
                    root_uid: r.get(2)?,
                    name: r.get(3)?,
                    created: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Persist (or replace) this machine's device. The table holds a single row.
    pub fn device_set(&self, dev: &StoredDevice) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM device", [])?;
        conn.execute(
            "INSERT INTO device (uid, share_id, root_uid, name, created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![dev.uid, dev.share_id, dev.root_uid, dev.name, dev.created],
        )?;
        Ok(())
    }

    /// Add a synced folder, returning its new row id.
    pub fn sync_folder_add(
        &self,
        local_path: &str,
        remote_uid: &str,
        remote_share_id: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_folder (local_path, remote_uid, remote_share_id)
             VALUES (?1, ?2, ?3)",
            params![local_path, remote_uid, remote_share_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Every synced folder, oldest first.
    pub fn sync_folder_list(&self) -> Result<Vec<StoredSyncFolder>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, local_path, remote_uid, remote_share_id, mode, pending_mode, state, last_sync
             FROM sync_folder ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StoredSyncFolder {
                id: r.get(0)?,
                local_path: r.get(1)?,
                remote_uid: r.get(2)?,
                remote_share_id: r.get(3)?,
                mode: r.get(4)?,
                pending_mode: r.get(5)?,
                state: r.get(6)?,
                last_sync: r.get(7)?,
            })
        })?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    /// Look up one synced folder by id.
    pub fn sync_folder_get(&self, id: i64) -> Result<Option<StoredSyncFolder>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, local_path, remote_uid, remote_share_id, mode, pending_mode, state, last_sync
             FROM sync_folder WHERE id = ?1",
            params![id],
            |r| {
                Ok(StoredSyncFolder {
                    id: r.get(0)?,
                    local_path: r.get(1)?,
                    remote_uid: r.get(2)?,
                    remote_share_id: r.get(3)?,
                    mode: r.get(4)?,
                    pending_mode: r.get(5)?,
                    state: r.get(6)?,
                    last_sync: r.get(7)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Remove a synced folder and its per-file baseline.
    pub fn sync_folder_remove(&self, id: i64) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM sync_entry WHERE folder_id = ?1", params![id])?;
        let n = tx.execute("DELETE FROM sync_folder WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Update a synced folder's mode (`mirror`/`ondemand`). The folder has
    /// reached the mode it was asked for, so any queued request is satisfied and
    /// cleared in the same write — a `pending_mode` outliving the switch it asked
    /// for would have the engine try to apply it again on the next pass.
    pub fn sync_folder_set_mode(&self, id: i64, mode: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sync_folder SET mode = ?2, pending_mode = NULL WHERE id = ?1",
            params![id, mode],
        )?;
        Ok(())
    }

    /// Queue (or, with `None`, withdraw) a mode the folder should move to once it
    /// is safe to switch.
    pub fn sync_folder_set_pending_mode(&self, id: i64, mode: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sync_folder SET pending_mode = ?2 WHERE id = ?1",
            params![id, mode],
        )?;
        Ok(())
    }

    /// Update a synced folder's state and stamp `last_sync` to now.
    pub fn sync_folder_set_state(&self, id: i64, state: &str, last_sync: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sync_folder SET state = ?2, last_sync = ?3 WHERE id = ?1",
            params![id, state, last_sync],
        )?;
        Ok(())
    }

    /// The whole per-file sync baseline for a folder, keyed by relative path.
    pub fn sync_entries(&self, folder_id: i64) -> Result<HashMap<String, StoredSyncEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT rel_path, remote_uid, local_mtime, local_size, remote_rev, remote_hash
             FROM sync_entry WHERE folder_id = ?1",
        )?;
        let rows = stmt.query_map(params![folder_id], |r| {
            Ok(StoredSyncEntry {
                rel_path: r.get(0)?,
                remote_uid: r.get(1)?,
                local_mtime: r.get(2)?,
                local_size: r.get(3)?,
                remote_rev: r.get(4)?,
                remote_hash: r.get(5)?,
            })
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let e = row?;
            map.insert(e.rel_path.clone(), e);
        }
        Ok(map)
    }

    /// Insert or replace one baseline row.
    pub fn sync_entry_upsert(&self, folder_id: i64, e: &StoredSyncEntry) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_entry
               (folder_id, rel_path, remote_uid, local_mtime, local_size, remote_rev, remote_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(folder_id, rel_path) DO UPDATE SET
               remote_uid  = excluded.remote_uid,
               local_mtime = excluded.local_mtime,
               local_size  = excluded.local_size,
               remote_rev  = excluded.remote_rev,
               remote_hash = excluded.remote_hash",
            params![
                folder_id,
                e.rel_path,
                e.remote_uid,
                e.local_mtime,
                e.local_size,
                e.remote_rev,
                e.remote_hash,
            ],
        )?;
        Ok(())
    }

    /// Drop one baseline row (a path that left the sync set on both sides).
    pub fn sync_entry_remove(&self, folder_id: i64, rel_path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM sync_entry WHERE folder_id = ?1 AND rel_path = ?2",
            params![folder_id, rel_path],
        )?;
        Ok(())
    }

    // ---- activity ---------------------------------------------------------

    /// Append one entry to the activity log, pruning back to [`ACTIVITY_KEEP`]
    /// rows. `kind` round-trips through serde rather than a hand-written string
    /// table, so adding an [`ActivityKind`] variant needs no change here.
    pub fn activity_add(&self, entry: &ActivityEntry) -> Result<()> {
        let kind = serde_json::to_string(&entry.kind)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO activity (time, kind, target, detail, ok) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![entry.time, kind, entry.target, entry.detail, entry.ok],
        )?;
        tx.execute(
            "DELETE FROM activity WHERE id <= (
               SELECT id FROM activity ORDER BY id DESC LIMIT 1 OFFSET ?1
             )",
            params![ACTIVITY_KEEP],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The most recent activity, newest first, capped at `limit` entries. Rows
    /// whose stored `kind` no longer parses (written by an older build) are
    /// skipped rather than failing the whole read.
    pub fn activity_list(&self, limit: usize) -> Result<Vec<ActivityEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT time, kind, target, detail, ok FROM activity
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (time, kind, target, detail, ok) = row?;
            let Ok(kind) = serde_json::from_str::<ActivityKind>(&kind) else {
                continue;
            };
            out.push(ActivityEntry {
                time,
                kind,
                target,
                detail,
                ok,
            });
        }
        Ok(out)
    }

    /// Drop the entire baseline for a folder. Used when flipping ondemand→mirror:
    /// the local tree was evicted, so the old baseline is stale and would make the
    /// next reconcile mistake "locally deleted" for "must re-download". Clearing it
    /// leaves an empty baseline + full remote = pure download (devices.md P3).
    pub fn sync_entries_clear(&self, folder_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM sync_entry WHERE folder_id = ?1",
            params![folder_id],
        )?;
        Ok(())
    }

    /// Children of `parent`, for the offline `ensure_children` fast path when a
    /// listed folder's `children` map entry was trimmed mid-run. Returns `None`
    /// when the folder is not marked `listed` (listing unknown, must re-fetch).
    pub fn children_if_listed(&self, parent: &NodeUid) -> Result<Option<Vec<Node>>> {
        let conn = self.conn.lock().unwrap();
        let listed: Option<i64> = conn
            .query_row(
                "SELECT listed FROM nodes WHERE uid = ?1",
                params![parent.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if listed != Some(1) {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT node_json FROM nodes
             WHERE parent_uid = ?1 AND node_json IS NOT NULL AND trashed = 0",
        )?;
        let rows = stmt.query_map(params![parent.to_string()], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(Some(out))
    }

    // --- Content-cache LRU index (P4) -------------------------------------
    //
    // Replaces the per-eviction `read_dir` scans in `ContentCache`. Each cached
    // blob/block carries one row keyed by its on-disk filename (`cache_key`),
    // tagged with `kind` ('blob' | 'block') so the two byte budgets stay
    // separate. `last_accessed` (unix seconds) is the LRU key. The daemon owns
    // the on-disk cache and rebuilds this index from disk on open, then keeps it
    // in sync on every store/read/evict, so it is authoritative for eviction.

    /// Insert or refresh a cache entry: set its on-disk `size` and bump
    /// `last_accessed` to `now`. Called on every store of a blob or block.
    pub fn cache_touch(&self, key: &str, kind: &str, size: u64, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO cache_entries (cache_key, kind, size_bytes, last_accessed)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(cache_key) DO UPDATE SET
               kind          = excluded.kind,
               size_bytes    = excluded.size_bytes,
               last_accessed = excluded.last_accessed",
            params![key, kind, size as i64, now],
        )?;
        Ok(())
    }

    /// Bump only `last_accessed` on a cache hit (LRU ordering). A missing row is
    /// a no-op — best effort, mirroring the old best-effort mtime touch.
    pub fn cache_accessed(&self, key: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE cache_entries SET last_accessed = ?2 WHERE cache_key = ?1",
            params![key, now],
        )?;
        Ok(())
    }

    /// Drop a single cache-entry row (one evicted blob or block).
    pub fn cache_remove(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM cache_entries WHERE cache_key = ?1",
            params![key],
        )?;
        Ok(())
    }

    /// Drop the whole-file blob row for `key` plus every block row for the same
    /// uid (`<key>.b<idx>`). Called by `ContentCache::evict`, which removes the
    /// blob and all of a uid's cached blocks at once.
    pub fn cache_remove_all(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM cache_entries WHERE cache_key = ?1 OR cache_key LIKE ?2",
            params![key, format!("{key}.b%")],
        )?;
        Ok(())
    }

    /// Every cache entry of `kind`, ordered least-recently-accessed first, as
    /// `(cache_key, size_bytes)`. The budget enforcer sums the sizes and evicts
    /// from the front until the cache fits.
    pub fn cache_entries_by_kind(&self, kind: &str) -> Result<Vec<(String, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT cache_key, size_bytes FROM cache_entries
             WHERE kind = ?1 ORDER BY last_accessed ASC",
        )?;
        let rows = stmt.query_map(params![kind], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Wipe the entire cache index. The daemon calls this on open before
    /// rebuilding the index from the on-disk cache, so a crash or external file
    /// deletion can never leave a phantom row inflating the budget total.
    pub fn cache_clear(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM cache_entries", [])?;
        Ok(())
    }

    // --- Pins (P5) --------------------------------------------------------
    //
    // The pin registry, formerly `pins.json`. One row per directly-pinned node,
    // keyed by uid display string. `recursive` marks a folder pin: its whole
    // subtree counts as pinned, resolved against `nodes` via a CTE so a new
    // descendant is covered the moment it lands in the node cache.

    /// Record `uid` as pinned under display `path`. `recursive` keeps the whole
    /// subtree of a folder. Idempotent — re-pinning refreshes the path/flag.
    pub fn pin_add(&self, uid: &str, path: &str, recursive: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pins (uid, path, recursive) VALUES (?1, ?2, ?3)
             ON CONFLICT(uid) DO UPDATE SET
               path = excluded.path, recursive = excluded.recursive",
            params![uid, path, recursive as i64],
        )?;
        Ok(())
    }

    /// Drop `uid`'s pin row. Returns whether a row existed.
    pub fn pin_remove(&self, uid: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM pins WHERE uid = ?1", params![uid])?;
        Ok(n > 0)
    }

    /// Every directly-pinned entry `(uid, path, recursive)`, ordered by uid.
    pub fn pin_list(&self) -> Result<Vec<(String, String, bool)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT uid, path, recursive FROM pins ORDER BY uid")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Whether `uid` is pinned — directly, or because a strict ancestor folder
    /// is pinned recursively. The ancestor check walks `parent_uid` to the root
    /// via a CTE; a direct pin is honoured even when the node has no `nodes` row
    /// yet (e.g. a CLI that never hydrates the node cache).
    pub fn is_pinned(&self, uid: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let direct: Option<i64> = conn
            .query_row("SELECT 1 FROM pins WHERE uid = ?1", params![uid], |r| {
                r.get(0)
            })
            .optional()?;
        if direct.is_some() {
            return Ok(true);
        }
        let anc: Option<i64> = conn
            .query_row(
                "WITH RECURSIVE anc(uid, parent_uid) AS (
                   SELECT uid, parent_uid FROM nodes WHERE uid = ?1
                   UNION ALL
                   SELECT n.uid, n.parent_uid FROM nodes n JOIN anc ON n.uid = anc.parent_uid
                 )
                 SELECT 1 FROM anc a JOIN pins p ON p.uid = a.uid
                 WHERE a.uid != ?1 AND p.recursive = 1
                 LIMIT 1",
                params![uid],
                |r| r.get(0),
            )
            .optional()?;
        Ok(anc.is_some())
    }

    /// Every uid that is pinned, directly or transitively: all direct pins plus
    /// every descendant of a recursively-pinned folder. Used to build the
    /// eviction-exempt set (the budget enforcer hashes these into cache keys).
    pub fn pinned_uids(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE sub(uid) AS (
               SELECT uid FROM pins WHERE recursive = 1
               UNION
               SELECT n.uid FROM nodes n JOIN sub ON n.parent_uid = sub.uid
             )
             SELECT uid FROM sub
             UNION
             SELECT uid FROM pins",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Every descendant uid of `folder` (all depths), via a `parent_uid` CTE.
    /// Used to evict a recursively-pinned subtree's cached blobs on unpin.
    pub fn descendants(&self, folder: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE sub(uid) AS (
               SELECT uid FROM nodes WHERE parent_uid = ?1
               UNION
               SELECT n.uid FROM nodes n JOIN sub ON n.parent_uid = sub.uid
             )
             SELECT uid FROM sub",
        )?;
        let rows = stmt.query_map(params![folder], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ---- local (non-Drive) file index (schema v6) ----

    /// Open a new local-index scan generation. Rows written under it survive
    /// [`local_finish_scan`](Self::local_finish_scan); rows still carrying an
    /// older generation are pruned there as "no longer on disk".
    pub fn local_begin_scan(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let current: i64 = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'local_scan_gen'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES ('local_scan_gen', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [next.to_string()],
        )?;
        Ok(next)
    }

    /// Write one batch of walked entries under scan generation `generation`. The
    /// FTS index is *not* touched here — it is rebuilt once in
    /// [`local_finish_scan`](Self::local_finish_scan).
    pub fn local_upsert_batch(&self, generation: i64, entries: &[LocalEntry]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO local_files (path, name, is_dir, size, mtime, scan_gen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path) DO UPDATE SET
                   name     = excluded.name,
                   is_dir   = excluded.is_dir,
                   size     = excluded.size,
                   mtime    = excluded.mtime,
                   scan_gen = excluded.scan_gen",
            )?;
            for e in entries {
                stmt.execute(params![
                    e.path,
                    e.name,
                    e.is_dir as i64,
                    e.size,
                    e.mtime,
                    generation
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Close scan generation `generation`: drop every row an older scan wrote
    /// (those paths are gone from disk), rebuild the FTS index over what remains,
    /// and stamp the completion time. Returns the number of indexed entries.
    pub fn local_finish_scan(&self, generation: i64, finished_at: i64) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM local_files WHERE scan_gen != ?1",
            params![generation],
        )?;
        tx.execute_batch("INSERT INTO local_fts(local_fts) VALUES('rebuild');")?;
        tx.execute(
            "INSERT INTO sync_state (key, value) VALUES ('local_indexed_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [finished_at.to_string()],
        )?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM local_files", [], |r| r.get(0))?;
        tx.commit()?;
        Ok(count)
    }

    /// When the last local scan completed (epoch seconds), or `None` if the index
    /// has never been built. The daemon uses this to decide whether a fresh mount
    /// needs an immediate rescan or can serve the existing index.
    pub fn local_indexed_at(&self) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'local_indexed_at'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok()))
    }

    /// Substring search over indexed local file names, newest-modified first
    /// within a relevance tier. Mirrors [`search`](Self::search): the trigram
    /// index handles queries of at least [`TRIGRAM_MIN`] chars, shorter ones fall
    /// back to a `LIKE` scan.
    pub fn search_local(&self, query: &str, limit: usize) -> Result<Vec<LocalFileHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt;
        let rows = if query.chars().count() < TRIGRAM_MIN {
            let pat = format!("%{}%", like_escape(query));
            stmt = conn.prepare(
                "SELECT path, name, is_dir, size, mtime FROM local_files
                 WHERE name LIKE ?1 ESCAPE '\\'
                 ORDER BY mtime DESC LIMIT ?2",
            )?;
            stmt.query_map(params![pat, limit as i64], local_hit)?
        } else {
            let phrase = query
                .split_whitespace()
                .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            stmt = conn.prepare(
                "SELECT f.path, f.name, f.is_dir, f.size, f.mtime
                 FROM local_fts x JOIN local_files f ON f.id = x.rowid
                 WHERE x.name MATCH ?1
                 ORDER BY x.rank LIMIT ?2",
            )?;
            stmt.query_map(params![phrase, limit as i64], local_hit)?
        };
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

/// One hit from [`Db::search_local`]: an indexed file on the machine itself, not
/// in Drive. `path` is absolute.
pub struct LocalFileHit {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    pub mtime: i64,
}

/// Row mapper for [`Db::search_local`]'s two query paths, which select the same
/// columns in the same order.
fn local_hit(row: &rusqlite::Row) -> rusqlite::Result<LocalFileHit> {
    Ok(LocalFileHit {
        path: row.get(0)?,
        name: row.get(1)?,
        is_dir: row.get::<_, i64>(2)? != 0,
        size: row.get(3)?,
        mtime: row.get(4)?,
    })
}

/// Row mapper for the `(node_json, uid)` pairs both search paths return.
fn pair(row: &rusqlite::Row) -> rusqlite::Result<(String, String)> {
    Ok((row.get(0)?, row.get(1)?))
}

/// Drain a `query_map` of [`pair`] rows into a `Vec`, propagating row errors.
fn collect_pairs(
    rows: impl Iterator<Item = rusqlite::Result<(String, String)>>,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Escape `LIKE` wildcards in a user query so `%` and `_` match literally
/// (paired with `ESCAPE '\'` in the statement).
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Resolve a node's mountpoint-relative path by walking `parent_uid` to the
/// root via a recursive CTE. The root (the node with no parent) is excluded, so
/// a top-level file `report.pdf` yields `"report.pdf"`, not `"My Files/report.pdf"`.
fn path_of(conn: &Connection, uid: &str) -> Result<String> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE anc(uid, parent_uid, name, depth) AS (
           SELECT uid, parent_uid, name, 0 FROM nodes WHERE uid = ?1
           UNION ALL
           SELECT n.uid, n.parent_uid, n.name, anc.depth + 1
           FROM nodes n JOIN anc ON n.uid = anc.parent_uid
         )
         SELECT name, parent_uid FROM anc ORDER BY depth DESC",
    )?;
    let rows = stmt.query_map(params![uid], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut parts = Vec::new();
    for r in rows {
        let (name, parent_uid) = r?;
        // Skip the root node (no parent); its name is the mount itself.
        if parent_uid.is_some() {
            parts.push(name);
        }
    }
    Ok(parts.join("/"))
}

/// Effective plaintext size of a node for the indexed `size` column: the
/// claimed size when known, else the on-storage size; folders are 0.
fn node_size(node: &Node) -> i64 {
    match &node.kind {
        NodeKind::Folder => 0,
        NodeKind::File {
            total_size_on_storage,
            claimed_size,
            ..
        } => claimed_size.unwrap_or(*total_size_on_storage).max(0),
    }
}

/// Schema v1: nodes + FTS5 trigram index + cache LRU. `sync_state` is created
/// in [`Db::migrate`] before this runs.
const MIGRATION_V1: &str = "
CREATE TABLE nodes (
  uid           TEXT PRIMARY KEY,
  parent_uid    TEXT,
  name          TEXT NOT NULL,
  is_dir        INTEGER NOT NULL,
  size          INTEGER,
  mtime         INTEGER NOT NULL,
  revision_hash TEXT,
  trashed       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_nodes_parent ON nodes(parent_uid);

CREATE VIRTUAL TABLE nodes_fts USING fts5(
  name, path, content='', tokenize='trigram'
);

CREATE TABLE cache_entries (
  cache_key     TEXT PRIMARY KEY,
  size_bytes    INTEGER,
  last_accessed INTEGER,
  is_pinned     INTEGER NOT NULL DEFAULT 0
);
";

/// Schema v2: node write-through (P1). `node_json` stores the full [`Node`] so
/// the hot-cache maps rehydrate losslessly; `listed` records whether a folder's
/// child listing was complete when persisted.
const MIGRATION_V2: &str = "
ALTER TABLE nodes ADD COLUMN node_json TEXT;
ALTER TABLE nodes ADD COLUMN listed INTEGER NOT NULL DEFAULT 0;
";

/// Schema v3: FTS search (P3). The v1 `nodes_fts` was contentless (`content=''`)
/// and never populated — drop it and recreate as a self-contained trigram index
/// carrying the `uid` (UNINDEXED, so retrievable but not tokenized) alongside the
/// indexed `name`. Backfill from the nodes already persisted by P1.
const MIGRATION_V3: &str = "
DROP TABLE IF EXISTS nodes_fts;
CREATE VIRTUAL TABLE nodes_fts USING fts5(
  uid UNINDEXED, name, tokenize='trigram'
);
INSERT INTO nodes_fts (uid, name)
  SELECT uid, name FROM nodes WHERE trashed = 0;
";

/// Schema v4: content-cache LRU index (P4). The v1 `cache_entries` keyed only by
/// `cache_key`; add a `kind` discriminator so the whole-file blob pool and the
/// block pool — which carry separate byte budgets — can be summed and evicted
/// independently. Existing rows (none in practice, the table was never written)
/// default to `'blob'`. The daemon rebuilds the index from disk on open, so no
/// backfill is needed here.
const MIGRATION_V4: &str = "
ALTER TABLE cache_entries ADD COLUMN kind TEXT NOT NULL DEFAULT 'blob';
";

/// Schema v5: pins move out of `pins.json` into the DB (P5). One row per
/// directly-pinned node, keyed by uid display string so a pin survives
/// renames/moves. `recursive` marks a folder pin whose whole subtree is kept;
/// descendants are resolved on demand against `nodes` via a CTE rather than
/// being expanded into rows here. `ContentCache::open` imports any legacy
/// `pins.json` into this table once, then deletes the file.
const MIGRATION_V5: &str = "
CREATE TABLE pins (
  uid       TEXT PRIMARY KEY,
  path      TEXT NOT NULL,
  recursive INTEGER NOT NULL DEFAULT 0
);
";

/// Schema v6: the index of *local* (non-Drive) files, so the launcher prompt can
/// search the machine alongside Drive. `local_files` is keyed by absolute path;
/// `scan_gen` stamps the scan that last saw a row, so a rescan prunes vanished
/// files with one `DELETE` instead of diffing.
///
/// `local_fts` is an *external-content* FTS5 index over `local_files.name`: the
/// text lives once in the base table and the index is rebuilt in bulk at the end
/// of a scan (`INSERT INTO local_fts(local_fts) VALUES('rebuild')`). That is far
/// cheaper than the delete-then-insert-per-row dance `nodes_fts` needs, because
/// a scan rewrites most rows at once rather than trickling single updates.
const MIGRATION_V6: &str = "
CREATE TABLE local_files (
  id       INTEGER PRIMARY KEY,
  path     TEXT NOT NULL UNIQUE,
  name     TEXT NOT NULL,
  is_dir   INTEGER NOT NULL,
  size     INTEGER NOT NULL DEFAULT 0,
  mtime    INTEGER NOT NULL DEFAULT 0,
  scan_gen INTEGER NOT NULL DEFAULT 0
);

CREATE VIRTUAL TABLE local_fts USING fts5(
  name, content='local_files', content_rowid='id', tokenize='trigram'
);
";

/// Schema v7: the photos timeline and the trash listing become persistent, so
/// opening the app paints them from disk instead of re-fetching the world (both
/// were memory-only: the timeline behind a 60 s TTL, the trash not cached at all).
///
/// `photos.seq` preserves the server's newest-first order, which is the only order
/// the timeline has — `capture_time` ties are common (a burst of shots) and would
/// otherwise shuffle between refreshes. `ratio` and `thumb_state` are *locally
/// learned*: they cost a download to rediscover, so `photos_replace` carries them
/// across a refresh while capture times and order come from the server.
const MIGRATION_V7: &str = "
CREATE TABLE photos (
  uid          TEXT PRIMARY KEY,
  capture_time INTEGER NOT NULL,
  name         TEXT,
  ratio        REAL,
  thumb_state  INTEGER NOT NULL DEFAULT 0,
  seq          INTEGER NOT NULL
);
CREATE INDEX idx_photos_seq ON photos(seq);

CREATE TABLE trash (
  uid    TEXT PRIMARY KEY,
  name   TEXT NOT NULL,
  is_dir INTEGER NOT NULL,
  size   INTEGER NOT NULL DEFAULT 0,
  mtime  INTEGER NOT NULL DEFAULT 0
);
";

/// Schema v8: device sync (devices.md). This machine registers as one Proton Drive
/// **Device** (a share + root folder on the main volume); `device` is a singleton
/// row cached so we reuse the same device across restarts instead of creating a
/// new one each run. `sync_folder` is one row per local folder the user added,
/// each mapped to a folder under the device root; `mode` is `mirror` (full local
/// copy, two-way synced) or `ondemand` (FUSE mount at `local_path`, no local
/// storage). `sync_entry` is the per-file baseline for three-way merge — added in
/// this migration so Phase 2 has the table, though Phase 1 leaves it empty.
const MIGRATION_V8: &str = "
CREATE TABLE device (
  uid      TEXT PRIMARY KEY,
  share_id TEXT NOT NULL,
  root_uid TEXT NOT NULL,
  name     TEXT NOT NULL,
  created  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE sync_folder (
  id              INTEGER PRIMARY KEY,
  local_path      TEXT NOT NULL UNIQUE,
  remote_uid      TEXT NOT NULL,
  remote_share_id TEXT NOT NULL,
  mode            TEXT NOT NULL DEFAULT 'mirror',
  state           TEXT NOT NULL DEFAULT 'idle',
  last_sync       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE sync_entry (
  folder_id   INTEGER NOT NULL,
  rel_path    TEXT NOT NULL,
  remote_uid  TEXT,
  local_mtime INTEGER NOT NULL DEFAULT 0,
  local_size  INTEGER NOT NULL DEFAULT 0,
  remote_hash TEXT,
  remote_rev  TEXT,
  PRIMARY KEY (folder_id, rel_path)
);
";

const MIGRATION_V9: &str = "
CREATE TABLE activity (
  id     INTEGER PRIMARY KEY,
  time   INTEGER NOT NULL,
  kind   TEXT NOT NULL,
  target TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  ok     INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX activity_time ON activity(time DESC);
";

/// Schema v10: a mode switch the user asked for is queued rather than rejected
/// when the folder is mid-pass or has un-uploaded changes, so `pending_mode`
/// records the intent until the engine can act on it. NULL is the resting state:
/// the folder is already where the user wants it.
const MIGRATION_V10: &str = "
ALTER TABLE sync_folder ADD COLUMN pending_mode TEXT;
";

/// Schema v11: writes no longer upload inside the FUSE handler. `release` stages
/// the bytes and records the intended upload here, and a drain worker performs it
/// (offline.md Phase 3). The row outlives the process, so a write survives both a
/// dead network and a restart.
///
/// `blob_path` points into the content cache's `staging/` dir and `meta_json` is
/// the [`StagedWrite`] sidecar describing which of its bytes are real — an
/// incomplete blob must be gap-filled from the remote base before it can be
/// uploaded. `next_attempt_at` is a ms deadline implementing retry backoff.
///
/// [`StagedWrite`]: crate::cache::StagedWrite
const MIGRATION_V11: &str = "
CREATE TABLE pending_op (
  id              INTEGER PRIMARY KEY,
  kind            TEXT NOT NULL,
  uid             TEXT NOT NULL,
  blob_path       TEXT,
  meta_json       TEXT,
  created_at      INTEGER NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0,
  last_error      TEXT,
  next_attempt_at INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX pending_op_uid ON pending_op(uid);
";

/// Schema v12: `pending_op` also carries mutations that *create* a node, which a
/// revision op never had to describe — it always addressed a node the server had
/// already minted a uid for (offline.md Phase 3b).
///
/// An offline `create`/`mkdir` cannot get a real uid, so the node is invented
/// locally under a `local~<uuid>` placeholder and the op records where it goes
/// (`parent_uid`) and what it is called (`name`). Both are nullable because a
/// revision op sets neither.
///
/// `parent_uid` is indexed: draining a folder rewrites every queued child that
/// points at its placeholder.
const MIGRATION_V12: &str = "
ALTER TABLE pending_op ADD COLUMN parent_uid TEXT;
ALTER TABLE pending_op ADD COLUMN name TEXT;

CREATE INDEX pending_op_parent ON pending_op(parent_uid);
";

#[cfg(test)]
mod tests {
    use super::*;

    /// `Db::open` applies the pragmas that `open_in_memory` skips, so the WAL
    /// settings can only be checked against a real file.
    #[test]
    fn open_bounds_the_wal_size() {
        let path = std::env::temp_dir().join(format!("pdfs-db-wal-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        let conn = db.conn.lock().unwrap();

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        // Without this the WAL is reused in place but never shrinks, so one
        // oversized transaction strands its high-water mark on disk forever.
        let limit: i64 = conn
            .query_row("PRAGMA journal_size_limit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(limit, WAL_SIZE_LIMIT);

        drop(conn);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    // `NodeVerification` is not re-exported, so build test nodes by
    // deserializing JSON (the field has a serde default and can be omitted).
    fn node_from(
        parent: serde_json::Value,
        link: &str,
        name: &str,
        kind: serde_json::Value,
    ) -> Node {
        let v = serde_json::json!({
            "uid": {"volume_id": "vol", "link_id": link},
            "parent_uid": parent,
            "kind": kind,
            "name": name,
            "creation_time": 100,
            "modification_time": 200,
            "trashed": false,
            "signature_email": null,
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn photos_replace_keeps_what_was_learned_and_drops_what_left() {
        let db = Db::open_in_memory().unwrap();
        db.photos_replace(&[
            ("p1".into(), 300, None),
            ("p2".into(), 200, None),
            ("p3".into(), 100, None),
        ])
        .unwrap();

        // A thumbnail attempt teaches us p1's ratio and that p2 can never have one.
        db.photo_set_thumb("p1", THUMB_HAVE, Some(1.5)).unwrap();
        db.photo_set_thumb("p2", THUMB_NONE, None).unwrap();

        // The next refresh brings a new photo, keeps p1 and p2, and loses p3.
        db.photos_replace(&[
            ("p0".into(), 400, Some("new.jpg".into())),
            ("p1".into(), 300, None),
            ("p2".into(), 200, None),
        ])
        .unwrap();

        let page = db.photos_page(0, 10).unwrap();
        assert_eq!(
            page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
            ["p0", "p1", "p2"],
            "server order is preserved, and the dropped photo is gone"
        );
        assert_eq!(page[0].name.as_deref(), Some("new.jpg"));
        // Ratios and verdicts cost a download to rediscover: they survive a refresh.
        assert_eq!(page[1].ratio, Some(1.5));
        assert_eq!(page[1].thumb_state, THUMB_HAVE);
        assert_eq!(page[2].thumb_state, THUMB_NONE);
        // A photo we know nothing about yet starts blank.
        assert_eq!(page[0].ratio, None);
        assert_eq!(page[0].thumb_state, THUMB_UNKNOWN);

        assert_eq!(db.photos_count().unwrap(), 3);
        let by_uid = db.photos_by_uid(&["p2".into()]).unwrap();
        assert_eq!(by_uid.len(), 1);
        assert_eq!(by_uid[0].capture_time, 200);
    }

    #[test]
    fn photos_page_slices_the_timeline_in_order() {
        let db = Db::open_in_memory().unwrap();
        let items: Vec<_> = (0..5)
            .map(|i| (format!("p{i}"), 500 - i as i64, None))
            .collect();
        db.photos_replace(&items).unwrap();

        let page = db.photos_page(2, 2).unwrap();
        assert_eq!(
            page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
            ["p2", "p3"]
        );
        assert!(db.photos_page(9, 2).unwrap().is_empty());
    }

    #[test]
    fn trash_replace_lists_folders_first() {
        let db = Db::open_in_memory().unwrap();
        db.trash_replace(&[
            StoredTrash {
                uid: "t1".into(),
                name: "zeta.txt".into(),
                is_dir: false,
                size: 10,
                mtime: 1,
            },
            StoredTrash {
                uid: "t2".into(),
                name: "Alpha".into(),
                is_dir: true,
                size: 0,
                mtime: 2,
            },
        ])
        .unwrap();

        let items = db.trash_list().unwrap();
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["Alpha", "zeta.txt"]
        );

        // A replace is a replace: emptying the trash on the server empties it here.
        db.trash_replace(&[]).unwrap();
        assert!(db.trash_list().unwrap().is_empty());
    }

    #[test]
    fn state_stamps_round_trip_and_clear() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.state_i64("photos_synced_ms").unwrap(), None);
        db.set_state_i64("photos_synced_ms", 1234).unwrap();
        assert_eq!(db.state_i64("photos_synced_ms").unwrap(), Some(1234));
        db.clear_state("photos_synced_ms").unwrap();
        assert_eq!(
            db.state_i64("photos_synced_ms").unwrap(),
            None,
            "a cleared stamp reads as never fetched, so the next request blocks on a refresh"
        );
    }

    fn folder(link: &str, parent: Option<&str>, name: &str) -> Node {
        let parent = match parent {
            Some(p) => serde_json::json!({"volume_id": "vol", "link_id": p}),
            None => serde_json::Value::Null,
        };
        node_from(parent, link, name, serde_json::json!("Folder"))
    }

    fn file(link: &str, parent: &str, name: &str, size: i64) -> Node {
        let kind = serde_json::json!({
            "File": {
                "media_type": "text/plain",
                "total_size_on_storage": size + 10,
                "claimed_size": size,
                "claimed_modification_time": null,
            }
        });
        node_from(
            serde_json::json!({"volume_id": "vol", "link_id": parent}),
            link,
            name,
            kind,
        )
    }

    /// Recovering the root by uid is what lets the daemon mount offline
    /// (offline.md Phase 1): the uid is remembered in `sync_state`, the node
    /// itself comes back out of `nodes`.
    #[test]
    fn node_by_uid_recovers_a_stored_node() {
        let db = Db::open_in_memory().unwrap();
        let root = folder("root", None, "My Files");
        db.upsert_node(&root).unwrap();
        db.set_state_str("root_uid", &root.uid.to_string()).unwrap();

        let key = db.state_str("root_uid").unwrap().unwrap();
        let got = db.node_by_uid(&key).unwrap().expect("root recovered");
        assert_eq!(got.uid, root.uid);
        assert_eq!(got.name, "My Files");
        assert!(got.is_folder());

        assert!(db.node_by_uid("vol~nope").unwrap().is_none());
        assert!(db.state_str("never_written").unwrap().is_none());
    }

    #[test]
    fn upsert_and_load_all_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let root = folder("root", None, "My Files");
        let child = file("f1", "root", "hello.txt", 42);
        db.upsert_node(&root).unwrap();
        db.upsert_node(&child).unwrap();

        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 2);
        let f = loaded.iter().find(|s| s.node.uid == uid("f1")).unwrap();
        assert_eq!(f.node.name, "hello.txt");
        assert!(!f.listed);
        match &f.node.kind {
            NodeKind::File { claimed_size, .. } => assert_eq!(*claimed_size, Some(42)),
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn upsert_nodes_and_load_all_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let root = folder("root", None, "My Files");
        let child1 = file("f1", "root", "hello.txt", 42);
        let child2 = file("f2", "root", "world.txt", 100);
        db.upsert_nodes(&[root, child1, child2]).unwrap();

        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 3);
        let f1 = loaded.iter().find(|s| s.node.uid == uid("f1")).unwrap();
        assert_eq!(f1.node.name, "hello.txt");
        let f2 = loaded.iter().find(|s| s.node.uid == uid("f2")).unwrap();
        assert_eq!(f2.node.name, "world.txt");
    }

    #[test]
    fn upsert_is_idempotent_update() {
        let db = Db::open_in_memory().unwrap();
        let mut n = folder("root", None, "My Files");
        db.upsert_node(&n).unwrap();
        n.name = "Renamed".into();
        db.upsert_node(&n).unwrap();
        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].node.name, "Renamed");
    }

    #[test]
    fn delete_node_removes_row() {
        let db = Db::open_in_memory().unwrap();
        let n = folder("root", None, "My Files");
        db.upsert_node(&n).unwrap();
        db.delete_node(&uid("root")).unwrap();
        assert!(db.load_all().unwrap().is_empty());
    }

    #[test]
    fn children_if_listed_gated_on_flag() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&file("f1", "root", "a.txt", 1)).unwrap();
        db.upsert_node(&file("f2", "root", "b.txt", 2)).unwrap();

        // Not listed yet → unknown, force a re-fetch.
        assert!(db.children_if_listed(&uid("root")).unwrap().is_none());

        db.set_listed(&uid("root"), true).unwrap();
        let kids = db.children_if_listed(&uid("root")).unwrap().unwrap();
        assert_eq!(kids.len(), 2);
    }

    #[test]
    fn children_if_listed_excludes_trashed() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        let mut trashed = file("f1", "root", "a.txt", 1);
        trashed.trashed = true;
        db.upsert_node(&trashed).unwrap();
        db.set_listed(&uid("root"), true).unwrap();
        assert_eq!(
            db.children_if_listed(&uid("root")).unwrap().unwrap().len(),
            0
        );
    }

    fn local(path: &str, name: &str, is_dir: bool) -> LocalEntry {
        LocalEntry {
            path: path.into(),
            name: name.into(),
            is_dir,
            size: 10,
            mtime: 5,
        }
    }

    /// A local scan is searchable by substring, and a *later* scan prunes the
    /// paths it no longer sees — including out of the FTS index, so a deleted
    /// file cannot keep surfacing in the prompt.
    #[test]
    fn local_scan_indexes_then_prunes_stale_paths() {
        let db = Db::open_in_memory().unwrap();

        let gen1 = db.local_begin_scan().unwrap();
        db.local_upsert_batch(
            gen1,
            &[
                local("/home/u/docs/report.pdf", "report.pdf", false),
                local("/home/u/docs/notes.md", "notes.md", false),
            ],
        )
        .unwrap();
        assert_eq!(db.local_finish_scan(gen1, 1_000).unwrap(), 2);

        // Trigram index gives substring (not just prefix) matches.
        let hits = db.search_local("port", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/home/u/docs/report.pdf");
        assert!(!hits[0].is_dir);
        assert_eq!(db.local_indexed_at().unwrap(), Some(1_000));

        // Second scan sees only notes.md → report.pdf is gone from disk.
        let gen2 = db.local_begin_scan().unwrap();
        db.local_upsert_batch(gen2, &[local("/home/u/docs/notes.md", "notes.md", false)])
            .unwrap();
        assert_eq!(db.local_finish_scan(gen2, 2_000).unwrap(), 1);
        assert!(db.search_local("report", 10).unwrap().is_empty());
        assert_eq!(db.search_local("notes", 10).unwrap().len(), 1);
    }

    /// Queries below the trigram minimum still match, via the `LIKE` fallback.
    #[test]
    fn local_search_short_query_like_fallback() {
        let db = Db::open_in_memory().unwrap();
        let g = db.local_begin_scan().unwrap();
        db.local_upsert_batch(g, &[local("/home/u/a.txt", "a.txt", false)])
            .unwrap();
        db.local_finish_scan(g, 1).unwrap();
        assert_eq!(db.search_local("a", 10).unwrap().len(), 1);
        assert!(db.search_local("", 10).unwrap().is_empty());
    }

    #[test]
    fn event_cursor_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        // Absent before first write → seed from server head on first mount.
        assert!(db.get_event_cursor().unwrap().is_none());
        db.set_event_cursor("evt-1").unwrap();
        assert_eq!(db.get_event_cursor().unwrap().as_deref(), Some("evt-1"));
        // Overwrites, not appends.
        db.set_event_cursor("evt-2").unwrap();
        assert_eq!(db.get_event_cursor().unwrap().as_deref(), Some("evt-2"));
    }

    #[test]
    fn search_trigram_substring_and_path() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&folder("docs", Some("root"), "Documents"))
            .unwrap();
        db.upsert_node(&file("f1", "docs", "report.pdf", 1))
            .unwrap();
        db.upsert_node(&file("f2", "root", "notes.txt", 2)).unwrap();
        db.upsert_node(&file(
            "f3",
            "root",
            "Rampage Open Air 2026 - order 166765244.pdf",
            3,
        ))
        .unwrap();

        // Substring match (trigram), not just prefix.
        let hits = db.search("port", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node.name, "report.pdf");
        // Path is mountpoint-relative, root excluded.
        assert_eq!(hits[0].path, "Documents/report.pdf");

        // Top-level file → bare name.
        let hits = db.search("notes", 10).unwrap();
        assert_eq!(hits[0].path, "notes.txt");

        // Multi-term FTS5 query matching (out of order, separated terms)
        let hits = db.search("rampage 2026", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].node.name,
            "Rampage Open Air 2026 - order 166765244.pdf"
        );
    }

    #[test]
    fn search_excludes_trashed_and_respects_limit() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&file("f1", "root", "alpha.txt", 1)).unwrap();
        let mut gone = file("f2", "root", "alphb.txt", 1);
        gone.trashed = true;
        db.upsert_node(&gone).unwrap();

        let hits = db.search("alph", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node.name, "alpha.txt");

        db.upsert_node(&file("f3", "root", "alphc.txt", 1)).unwrap();
        assert_eq!(db.search("alph", 1).unwrap().len(), 1);
    }

    #[test]
    fn search_short_query_like_fallback() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&file("f1", "root", "ab.txt", 1)).unwrap();
        // Under trigram min length → LIKE path still finds it.
        let hits = db.search("ab", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node.name, "ab.txt");
    }

    #[test]
    fn search_drops_fts_row_on_delete_and_trash() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&file("f1", "root", "unique.txt", 1))
            .unwrap();
        assert_eq!(db.search("unique", 10).unwrap().len(), 1);

        // Re-upsert as trashed → leaves the index.
        let mut t = file("f1", "root", "unique.txt", 1);
        t.trashed = true;
        db.upsert_node(&t).unwrap();
        assert_eq!(db.search("unique", 10).unwrap().len(), 0);

        // Resurrect, then hard-delete.
        db.upsert_node(&file("f1", "root", "unique.txt", 1))
            .unwrap();
        assert_eq!(db.search("unique", 10).unwrap().len(), 1);
        db.delete_node(&uid("f1")).unwrap();
        assert_eq!(db.search("unique", 10).unwrap().len(), 0);
    }

    fn activity(target: &str, kind: ActivityKind, ok: bool) -> ActivityEntry {
        ActivityEntry {
            time: 1700,
            kind,
            target: target.into(),
            detail: "detail".into(),
            ok,
        }
    }

    #[test]
    fn activity_reads_back_newest_first() {
        let db = Db::open_in_memory().unwrap();
        db.activity_add(&activity("a.txt", ActivityKind::Upload, true))
            .unwrap();
        db.activity_add(&activity("b.txt", ActivityKind::Download, false))
            .unwrap();

        let items = db.activity_list(10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].target, "b.txt");
        assert_eq!(items[0].kind, ActivityKind::Download);
        assert!(!items[0].ok);
        assert_eq!(items[0].detail, "detail");
        assert_eq!(items[0].time, 1700);
        assert_eq!(items[1].target, "a.txt");

        assert_eq!(db.activity_list(1).unwrap().len(), 1);
    }

    #[test]
    fn activity_prunes_to_the_keep_limit() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..(ACTIVITY_KEEP + 10) {
            db.activity_add(&activity(&format!("f{i}"), ActivityKind::Upload, true))
                .unwrap();
        }
        let items = db.activity_list(ACTIVITY_KEEP as usize * 2).unwrap();
        assert_eq!(items.len(), ACTIVITY_KEEP as usize);
        // The newest survive; the oldest are the ones dropped.
        assert_eq!(items[0].target, format!("f{}", ACTIVITY_KEEP + 9));
    }

    #[test]
    fn opens_and_migrates() {
        let db = Db::open_in_memory().unwrap();
        let version: String = db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT value FROM sync_state WHERE key = 'schema_version'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn a_second_write_supersedes_the_first_pending_op() {
        let db = Db::open_in_memory().unwrap();
        let op = |blob: &str| PendingOp {
            id: 0,
            kind: OP_REVISION.to_string(),
            uid: uid("a").to_string(),
            parent_uid: None,
            name: None,
            blob_path: Some(blob.to_string()),
            meta_json: Some("{}".to_string()),
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };

        let (_, superseded) = db.enqueue_op(&op("/staging/first")).unwrap();
        assert_eq!(superseded, None, "nothing to supersede on the first write");

        // The newer blob already contains everything the older one did, so the
        // older op must go — and its blob must be reported so it can be deleted
        // rather than leaked.
        let (id2, superseded) = db.enqueue_op(&op("/staging/second")).unwrap();
        assert_eq!(superseded.as_deref(), Some("/staging/first"));

        let ops = db.pending_ops().unwrap();
        assert_eq!(ops.len(), 1, "one queued upload per node");
        assert_eq!(ops[0].id, id2);
        assert_eq!(ops[0].blob_path.as_deref(), Some("/staging/second"));
        assert_eq!(db.pending_op_counts().unwrap().uploads, 1);
    }

    /// Deleting a folder that was created offline must take the ops queued
    /// underneath it with it: they name a placeholder parent that will now never
    /// become real, so nothing could ever drain them and nothing is left to
    /// rewrite them.
    #[test]
    fn deleting_a_queued_folder_takes_its_queued_children_with_it() {
        let db = Db::open_in_memory().unwrap();
        let op = |kind: &str, uid: &str, parent: &str, blob: Option<&str>| PendingOp {
            id: 0,
            kind: kind.to_string(),
            uid: uid.to_string(),
            parent_uid: Some(parent.to_string()),
            name: Some("n".to_string()),
            blob_path: blob.map(str::to_string),
            meta_json: None,
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };
        let root = uid("root").to_string();
        db.enqueue_op(&op(OP_MKDIR, "local~dir", &root, None))
            .unwrap();
        db.enqueue_op(&op(OP_MKDIR, "local~sub", "local~dir", None))
            .unwrap();
        db.enqueue_op(&op(
            OP_CREATE,
            "local~deep",
            "local~sub",
            Some("/staging/deep"),
        ))
        .unwrap();
        // A sibling outside the doomed subtree must survive.
        db.enqueue_op(&op(OP_CREATE, "local~other", &root, Some("/staging/other")))
            .unwrap();

        let blobs = db.delete_ops_for_uid("local~dir").unwrap();
        assert_eq!(
            blobs,
            vec!["/staging/deep"],
            "the subtree's bytes come back"
        );

        let left: Vec<String> = db
            .pending_ops()
            .unwrap()
            .into_iter()
            .map(|o| o.uid)
            .collect();
        assert_eq!(left, vec!["local~other"]);
    }

    /// A rename is the node's desired end state, so the newest one is the only
    /// one worth performing — but it must not disturb the queued *upload* of the
    /// same node, which is unrelated work.
    #[test]
    fn a_second_rename_supersedes_the_first_but_leaves_the_upload_alone() {
        let db = Db::open_in_memory().unwrap();
        let rename = |name: &str| PendingOp {
            id: 0,
            kind: OP_RENAME.to_string(),
            uid: uid("a").to_string(),
            parent_uid: Some(uid("parent").to_string()),
            name: Some(name.to_string()),
            blob_path: None,
            meta_json: None,
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };
        db.enqueue_op(&PendingOp {
            id: 0,
            kind: OP_REVISION.to_string(),
            uid: uid("a").to_string(),
            parent_uid: None,
            name: None,
            blob_path: Some("/staging/blob".to_string()),
            meta_json: Some("{}".to_string()),
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        })
        .unwrap();

        db.enqueue_op(&rename("first")).unwrap();
        let (_, superseded) = db.enqueue_op(&rename("second")).unwrap();
        assert_eq!(superseded, None, "a rename owns no blob to clean up");

        let ops = db.pending_ops().unwrap();
        assert_eq!(ops.len(), 2, "the queued upload survives the rename");
        let renames: Vec<_> = ops.iter().filter(|o| o.kind == OP_RENAME).collect();
        assert_eq!(renames.len(), 1, "one rename per node");
        assert_eq!(renames[0].name.as_deref(), Some("second"));

        let counts = db.pending_op_counts().unwrap();
        assert_eq!(counts.uploads, 1, "the revision is the only upload");
        assert_eq!(counts.changes, 1, "the rename carries no bytes");
    }

    /// Renaming a node whose create has not drained rewrites the intent rather
    /// than queueing a rename against a uid the server has never issued.
    #[test]
    fn renaming_a_queued_create_rewrites_its_target() {
        let db = Db::open_in_memory().unwrap();
        let local = "local~abc";
        db.enqueue_op(&PendingOp {
            id: 0,
            kind: OP_CREATE.to_string(),
            uid: local.to_string(),
            parent_uid: Some(uid("old").to_string()),
            name: Some("draft.txt".to_string()),
            blob_path: Some("/staging/blob".to_string()),
            meta_json: Some("{}".to_string()),
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        })
        .unwrap();

        let rewritten = db
            .rewrite_op_target(local, &uid("new").to_string(), "final.txt")
            .unwrap();
        assert!(rewritten);

        let ops = db.pending_ops().unwrap();
        assert_eq!(ops.len(), 1, "a rewrite is not a second op");
        assert_eq!(ops[0].name.as_deref(), Some("final.txt"));
        assert_eq!(
            ops[0].parent_uid.as_deref(),
            Some(uid("new").to_string()).as_deref()
        );
        assert_eq!(
            ops[0].blob_path.as_deref(),
            Some("/staging/blob"),
            "the bytes riding on the create are untouched"
        );

        // Once the create has drained there is no intent left to rewrite, and the
        // caller has to rename the real node instead.
        db.delete_op(ops[0].id).unwrap();
        assert!(
            !db.rewrite_op_target(local, &uid("new").to_string(), "final.txt")
                .unwrap()
        );
    }

    #[test]
    fn a_write_folds_into_a_queued_create_instead_of_superseding_it() {
        let db = Db::open_in_memory().unwrap();
        let local = "local~abc";
        db.enqueue_op(&PendingOp {
            id: 0,
            kind: OP_CREATE.to_string(),
            uid: local.to_string(),
            parent_uid: Some(uid("parent").to_string()),
            name: Some("new.txt".to_string()),
            blob_path: None,
            meta_json: None,
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        })
        .unwrap();

        let first = db
            .attach_blob_to_create(local, "/staging/first", "{}")
            .unwrap()
            .expect("create is still queued");
        assert_eq!(first.superseded, None);

        // Rewriting the file before it drains replaces the bytes but must leave
        // the create itself alone: it is the only thing that will ever bring this
        // file into existence remotely.
        let second = db
            .attach_blob_to_create(local, "/staging/second", "{}")
            .unwrap()
            .expect("create is still queued");
        assert_eq!(second.superseded.as_deref(), Some("/staging/first"));

        let ops = db.pending_ops().unwrap();
        assert_eq!(ops.len(), 1, "still exactly one create");
        assert_eq!(ops[0].kind, OP_CREATE);
        assert_eq!(ops[0].blob_path.as_deref(), Some("/staging/second"));
        assert_eq!(ops[0].name.as_deref(), Some("new.txt"));
    }

    #[test]
    fn attaching_to_an_already_drained_create_reports_it_is_gone() {
        let db = Db::open_in_memory().unwrap();
        // No create queued: the caller must fall back to a revision op rather
        // than silently dropping the bytes.
        let out = db
            .attach_blob_to_create("local~gone", "/staging/x", "{}")
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn draining_a_folder_repoints_its_queued_children() {
        let db = Db::open_in_memory().unwrap();
        let local_dir = "local~dir";
        let real_dir = uid("realdir").to_string();
        db.enqueue_op(&PendingOp {
            id: 0,
            kind: OP_CREATE.to_string(),
            uid: "local~child".to_string(),
            parent_uid: Some(local_dir.to_string()),
            name: Some("inside.txt".to_string()),
            blob_path: Some("/staging/child".to_string()),
            meta_json: Some("{}".to_string()),
            created_at: 2,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        })
        .unwrap();

        db.remap_local_uid(local_dir, &real_dir).unwrap();

        // The child was queued against a folder that did not exist yet. Once the
        // folder is real, the child must target the server's uid — otherwise the
        // upload would address `local~dir` and 404.
        let ops = db.pending_ops().unwrap();
        assert_eq!(ops[0].parent_uid.as_deref(), Some(real_dir.as_str()));
    }

    #[test]
    fn a_failed_op_stays_queued_with_backoff() {
        let db = Db::open_in_memory().unwrap();
        db.enqueue_op(&PendingOp {
            id: 0,
            kind: OP_REVISION.to_string(),
            uid: uid("a").to_string(),
            parent_uid: None,
            name: None,
            blob_path: Some("/staging/blob".to_string()),
            meta_json: Some("{}".to_string()),
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        })
        .unwrap();
        let id = db.pending_ops().unwrap()[0].id;

        db.record_op_failure(id, "network unreachable", 5_000)
            .unwrap();

        // The staged blob is the only copy of the user's bytes: a failure must
        // never drop the row, only defer it.
        let ops = db.pending_ops().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].attempts, 1);
        assert_eq!(ops[0].last_error.as_deref(), Some("network unreachable"));
        assert_eq!(ops[0].next_attempt_at, 5_000);

        db.record_op_failure(id, "still down", 9_000).unwrap();
        assert_eq!(db.pending_ops().unwrap()[0].attempts, 2);

        db.delete_op(id).unwrap();
        assert_eq!(db.pending_op_counts().unwrap().uploads, 0);
    }

    #[test]
    fn migrate_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        // Second migrate is a no-op (already at head) and must not error.
        db.migrate().unwrap();
    }

    /// A queued mode switch is a promise the daemon has to keep across a restart,
    /// so it lives in the row, and reaching the mode is what retires it.
    #[test]
    fn pending_mode_is_queued_until_the_mode_is_reached() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .sync_folder_add("/home/me/Downloads", "v~l", "s")
            .unwrap();
        assert_eq!(db.sync_folder_get(id).unwrap().unwrap().pending_mode, None);

        db.sync_folder_set_pending_mode(id, Some("ondemand"))
            .unwrap();
        assert_eq!(
            db.sync_folder_get(id)
                .unwrap()
                .unwrap()
                .pending_mode
                .as_deref(),
            Some("ondemand")
        );
        // The listing carries it too — it is what the front-ends paint from.
        assert_eq!(
            db.sync_folder_list().unwrap()[0].pending_mode.as_deref(),
            Some("ondemand")
        );

        // Landing the switch satisfies the request: a `pending_mode` outliving it
        // would have the engine try to apply the same switch on every later pass.
        db.sync_folder_set_mode(id, "ondemand").unwrap();
        let folder = db.sync_folder_get(id).unwrap().unwrap();
        assert_eq!(folder.mode, "ondemand");
        assert_eq!(folder.pending_mode, None);

        // And the user can withdraw a request that hasn't landed yet.
        db.sync_folder_set_pending_mode(id, Some("mirror")).unwrap();
        db.sync_folder_set_pending_mode(id, None).unwrap();
        let folder = db.sync_folder_get(id).unwrap().unwrap();
        assert_eq!(folder.mode, "ondemand");
        assert_eq!(folder.pending_mode, None);
    }

    #[test]
    fn cache_index_touch_access_and_lru_order() {
        let db = Db::open_in_memory().unwrap();
        db.cache_touch("k1", "blob", 100, 10).unwrap();
        db.cache_touch("k2", "blob", 200, 20).unwrap();
        // LRU-first: k1 (older access) before k2.
        let rows = db.cache_entries_by_kind("blob").unwrap();
        assert_eq!(rows, vec![("k1".into(), 100), ("k2".into(), 200)]);

        // Accessing k1 moves it to the back (most recent).
        db.cache_accessed("k1", 30).unwrap();
        let rows = db.cache_entries_by_kind("blob").unwrap();
        assert_eq!(rows[0].0, "k2");
        assert_eq!(rows[1].0, "k1");

        // Re-touch updates size, not just time.
        db.cache_touch("k1", "blob", 150, 40).unwrap();
        let rows = db.cache_entries_by_kind("blob").unwrap();
        assert_eq!(rows.iter().find(|(k, _)| k == "k1").unwrap().1, 150);
    }

    #[test]
    fn cache_index_kinds_are_separate() {
        let db = Db::open_in_memory().unwrap();
        db.cache_touch("blob1", "blob", 100, 1).unwrap();
        db.cache_touch("blk1.b0", "block", 50, 1).unwrap();
        assert_eq!(db.cache_entries_by_kind("blob").unwrap().len(), 1);
        assert_eq!(db.cache_entries_by_kind("block").unwrap().len(), 1);
    }

    #[test]
    fn cache_index_remove_and_remove_all() {
        let db = Db::open_in_memory().unwrap();
        // A blob plus two of its blocks (key prefix shared).
        db.cache_touch("abc", "blob", 1, 1).unwrap();
        db.cache_touch("abc.b0", "block", 1, 1).unwrap();
        db.cache_touch("abc.b1", "block", 1, 1).unwrap();
        // An unrelated entry that must survive.
        db.cache_touch("xyz", "blob", 1, 1).unwrap();

        db.cache_remove("abc.b0").unwrap();
        assert_eq!(db.cache_entries_by_kind("block").unwrap().len(), 1);

        // remove_all drops the blob row and every remaining block of that uid.
        db.cache_remove_all("abc").unwrap();
        assert!(db.cache_entries_by_kind("block").unwrap().is_empty());
        let blobs = db.cache_entries_by_kind("blob").unwrap();
        assert_eq!(blobs, vec![("xyz".into(), 1)]);
    }

    #[test]
    fn cache_index_clear() {
        let db = Db::open_in_memory().unwrap();
        db.cache_touch("k1", "blob", 1, 1).unwrap();
        db.cache_touch("k2", "block", 1, 1).unwrap();
        db.cache_clear().unwrap();
        assert!(db.cache_entries_by_kind("blob").unwrap().is_empty());
        assert!(db.cache_entries_by_kind("block").unwrap().is_empty());
    }

    #[test]
    fn pin_add_list_remove_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        db.pin_add("vol~a", "docs/a.txt", false).unwrap();
        db.pin_add("vol~d", "docs", true).unwrap();
        let list = db.pin_list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], ("vol~a".into(), "docs/a.txt".into(), false));
        assert_eq!(list[1], ("vol~d".into(), "docs".into(), true));

        // Re-pin refreshes path/flag, not a duplicate row.
        db.pin_add("vol~a", "moved/a.txt", false).unwrap();
        let list = db.pin_list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].1, "moved/a.txt");

        assert!(db.pin_remove("vol~a").unwrap());
        assert!(!db.pin_remove("vol~a").unwrap());
        assert_eq!(db.pin_list().unwrap().len(), 1);
    }

    #[test]
    fn is_pinned_direct_without_node_row() {
        // A direct pin counts even when the node was never hydrated into `nodes`.
        let db = Db::open_in_memory().unwrap();
        db.pin_add("vol~a", "a.txt", false).unwrap();
        assert!(db.is_pinned("vol~a").unwrap());
        assert!(!db.is_pinned("vol~b").unwrap());
    }

    #[test]
    fn recursive_folder_pin_covers_subtree() {
        let db = Db::open_in_memory().unwrap();
        // root/docs/{report.pdf, sub/deep.txt}, root/loose.txt
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&folder("docs", Some("root"), "Documents"))
            .unwrap();
        db.upsert_node(&file("rep", "docs", "report.pdf", 1))
            .unwrap();
        db.upsert_node(&folder("sub", Some("docs"), "Sub")).unwrap();
        db.upsert_node(&file("deep", "sub", "deep.txt", 1)).unwrap();
        db.upsert_node(&file("loose", "root", "loose.txt", 1))
            .unwrap();

        // Pin the Documents folder recursively (uids are `vol~link` display form).
        let du = |l: &str| uid(l).to_string();
        db.pin_add(&du("docs"), "Documents", true).unwrap();

        // Everything under docs (any depth) is pinned; loose.txt is not.
        assert!(db.is_pinned(&du("docs")).unwrap());
        assert!(db.is_pinned(&du("rep")).unwrap());
        assert!(db.is_pinned(&du("deep")).unwrap());
        assert!(!db.is_pinned(&du("loose")).unwrap());

        // pinned_uids expands the subtree (folder + descendants), no loose.txt.
        let mut got = db.pinned_uids().unwrap();
        got.sort();
        assert_eq!(got, vec![du("deep"), du("docs"), du("rep"), du("sub")]);

        // descendants() lists the subtree of a folder (excludes the folder).
        let mut desc = db.descendants(&du("docs")).unwrap();
        desc.sort();
        assert_eq!(desc, vec![du("deep"), du("rep"), du("sub")]);
    }

    #[test]
    fn non_recursive_folder_pin_does_not_cover_children() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&folder("docs", Some("root"), "Documents"))
            .unwrap();
        db.upsert_node(&file("rep", "docs", "report.pdf", 1))
            .unwrap();
        // A non-recursive pin on the folder covers only the folder itself.
        let du = |l: &str| uid(l).to_string();
        db.pin_add(&du("docs"), "Documents", false).unwrap();
        assert!(db.is_pinned(&du("docs")).unwrap());
        assert!(!db.is_pinned(&du("rep")).unwrap());
    }

    #[test]
    fn schema_objects_exist() {
        let db = Db::open_in_memory().unwrap();
        let count: i64 = db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM sqlite_master
                     WHERE name IN ('nodes', 'nodes_fts', 'cache_entries')",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count, 3);
    }
}
