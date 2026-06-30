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

use std::path::Path;
use std::sync::Mutex;

use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;

/// Current schema version. Bump on every forward migration added below.
const SCHEMA_VERSION: i64 = 5;

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
            // Quote the user input as a single FTS phrase so its characters are
            // matched literally, never parsed as FTS query syntax.
            let phrase = format!("\"{}\"", query.replace('"', "\"\""));
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

#[cfg(test)]
mod tests {
    use super::*;
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

        // Substring match (trigram), not just prefix.
        let hits = db.search("port", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node.name, "report.pdf");
        // Path is mountpoint-relative, root excluded.
        assert_eq!(hits[0].path, "Documents/report.pdf");

        // Top-level file → bare name.
        let hits = db.search("notes", 10).unwrap();
        assert_eq!(hits[0].path, "notes.txt");
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
    fn migrate_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        // Second migrate is a no-op (already at head) and must not error.
        db.migrate().unwrap();
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
