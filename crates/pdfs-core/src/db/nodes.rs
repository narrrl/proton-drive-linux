//! Node rows: the persisted mirror of the remote tree, plus the trigram
//! full-text index over names and paths that backs `Request::Search`.

use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::Result;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};

use super::utils::{TRIGRAM_MIN, collect_pairs, like_escape, pair, path_of};

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

impl Db {
    pub fn upsert_node(&self, node: &Node) -> Result<()> {
        self.upsert_nodes(std::slice::from_ref(node))
    }

    /// Write-through a batch of nodes as one transaction — a whole directory
    /// listing, typically. Otherwise identical to [`upsert_node`](Self::upsert_node),
    /// which is the single-node case of it.
    ///
    /// The commit count is the point: SQLite autocommits every statement that is
    /// not in an explicit transaction, so interning a folder of a thousand
    /// children row-by-row cost a thousand fsyncs, and `ls` waited for all of
    /// them.
    pub fn upsert_nodes(&self, nodes: &[Node]) -> Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
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
            // FTS5 has no UPSERT, so refresh the row by delete-then-insert.
            // Trashed nodes are kept out of the index entirely so they never
            // surface in search results.
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
        let mut conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE nodes SET listed = ?2 WHERE uid = ?1",
            params![uid.to_string(), listed as i64],
        )?;
        Ok(())
    }

    /// Load every persisted node for cold-start hydration of the `State` maps.
    pub fn load_all(&self) -> Result<Vec<StoredNode>> {
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
    pub fn children_if_listed(&self, parent: &NodeUid) -> Result<Option<Vec<Node>>> {
        let conn = self.conn.lock();
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
}

/// Effective plaintext size of a node for the indexed `size` column: the
/// claimed size when known, else the on-storage size; folders are 0.
pub(super) fn node_size(node: &Node) -> i64 {
    match &node.kind {
        NodeKind::Folder => 0,
        NodeKind::File {
            total_size_on_storage,
            claimed_size,
            ..
        } => claimed_size.unwrap_or(*total_size_on_storage).max(0),
    }
}
