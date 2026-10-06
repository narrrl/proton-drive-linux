//! Node rows: the persisted mirror of the remote tree, plus the trigram
//! full-text index over names and paths that backs `Request::Search`.

use std::collections::HashSet;

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params};

use super::Db;
use super::ops::{OP_CREATE, OP_MKDIR, OP_RENAME, local_lid, local_uid, wake_ops_waiting_for_tx};
use crate::{Access, Result};
use proton_drive_rs::proton_sdk::ids::{LinkId, NodeUid, VolumeId};
use proton_drive_rs::{Node, NodeKind};

use super::utils::{
    HitRow, TRIGRAM_MIN, collect_hits, hit_row, join_path, like_escape, path_of, walk_path_of,
};

pub struct StoredNode {
    pub node: Node,
    pub listed: bool,
    /// The row's local id ([`Db::lid_of`]).
    pub lid: i64,
}

/// One full-text search match: the stored [`Node`] plus its mountpoint-relative
/// path (`/`-joined, root excluded) so the front-end can navigate to or open it.
pub struct SearchHit {
    pub node: Node,
    pub path: String,
}

pub struct PublishedSharedRoot {
    pub node: Node,
    pub access: Access,
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
            upsert_node_tx(&tx, node)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Return visible direct children without consulting the parent's `listed`
    /// flag. Synthetic shared listings use this to serve the last completed
    /// snapshot while offline, including after a live event expired its TTL.
    pub fn visible_children(&self, parent: &NodeUid) -> Result<Vec<Node>> {
        let conn = self.read();
        let (key, parent) = parent_key(&conn, &parent.to_string())?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_NOW} FROM nodes n LEFT JOIN nodes p ON p.lid = n.parent_lid
              WHERE n.{key} = ?1 AND n.trashed = 0 AND n.node_json IS NOT NULL
              ORDER BY n.name, n.uid"
        ))?;
        let rows = stmt.query_map([parent], |row| row.get::<_, String>(0))?;
        let mut nodes = Vec::new();
        for row in rows {
            nodes.push(serde_json::from_str(&row?)?);
        }
        Ok(nodes)
    }

    /// Atomically publish one completed `Shared with me` root listing.
    ///
    /// Roots absent from `accepted` and every persisted descendant are
    /// tombstoned, not deleted. Accepted roots omitted from materialization keep
    /// their visible snapshot but are downgraded to Viewer until membership is
    /// verified again. Both cases preserve queued operations and staged writes.
    pub fn publish_shared_roots(
        &self,
        parent: &NodeUid,
        accepted: &[NodeUid],
        roots: &[PublishedSharedRoot],
    ) -> Result<Vec<NodeUid>> {
        let parent = parent.to_string();
        let present: HashSet<String> = accepted.iter().map(NodeUid::to_string).collect();
        let materialized: HashSet<String> = roots
            .iter()
            .map(|published| published.node.uid.to_string())
            .collect();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        let existing = direct_child_uids_tx(&tx, &parent)?;
        let removed: Vec<String> = existing
            .into_iter()
            .filter(|uid| !present.contains(uid))
            .collect();

        for published in roots {
            if !present.contains(&published.node.uid.to_string()) {
                continue;
            }
            upsert_node_tx(&tx, &published.node)?;
            upsert_share_access_tx(&tx, &published.node.uid.to_string(), published.access)?;
        }
        for uid in present.difference(&materialized) {
            upsert_share_access_tx(&tx, uid, Access::Viewer)?;
        }
        for root in &removed {
            tombstone_subtree_tx(&tx, root)?;
            upsert_share_access_tx(&tx, root, Access::Viewer)?;
        }
        tx.execute(
            "UPDATE nodes SET listed = 1 WHERE uid = ?1",
            params![parent],
        )?;
        tx.commit()?;

        Ok(removed
            .into_iter()
            .filter_map(|uid| parse_node_uid(&uid))
            .collect())
    }

    /// Atomically withdraw a deleted foreign subtree and deny stale handles.
    ///
    /// Node rows and queued operations remain so retries and staged writes keep
    /// their references. FTS and completed-listing state are withdrawn for the
    /// whole subtree, and the deleted UID becomes a fail-closed authority.
    pub fn tombstone_foreign_subtree(&self, uid: &NodeUid) -> Result<()> {
        let uid = uid.to_string();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tombstone_subtree_tx(&tx, &uid)?;
        upsert_share_access_tx(&tx, &uid, Access::Viewer)?;
        tx.commit()?;
        Ok(())
    }

    /// Publish a completed foreign-folder listing from its authoritative UID
    /// list and the subset that materialized successfully. Accepted-but-omitted
    /// children retain their previous snapshot; only absent UIDs are tombstoned.
    pub fn publish_foreign_children(
        &self,
        parent: &NodeUid,
        accepted: &[NodeUid],
        materialized: &[Node],
    ) -> Result<Vec<NodeUid>> {
        let parent = parent.to_string();
        let present: HashSet<String> = accepted.iter().map(NodeUid::to_string).collect();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let removed: Vec<String> = direct_child_uids_tx(&tx, &parent)?
            .into_iter()
            .filter(|uid| !present.contains(uid))
            .collect();
        for node in materialized {
            if present.contains(&node.uid.to_string()) {
                upsert_node_tx(&tx, node)?;
            }
        }
        for uid in &removed {
            tombstone_subtree_tx(&tx, uid)?;
        }
        tx.execute(
            "UPDATE nodes SET listed = 1 WHERE uid = ?1",
            params![parent],
        )?;
        tx.commit()?;
        Ok(removed
            .into_iter()
            .filter_map(|uid| parse_node_uid(&uid))
            .collect())
    }

    /// Atomically publish the synthetic root's pinned name, node/FTS state, and
    /// Viewer authority.
    ///
    /// The pin is inserted last so any failure there rolls back the node and
    /// its descendant FTS updates as well as the access row. Returning `false`
    /// guarantees no write occurred, which keeps cached root lookups O(1).
    pub fn publish_virtual_root(&self, pinned_name_key: &str, node: &Node) -> Result<bool> {
        let uid = node.uid.to_string();
        let desired_parent = node.parent_uid.as_ref().map(NodeUid::to_string);
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let pinned_name: Option<String> = tx
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                params![pinned_name_key],
                |row| row.get(0),
            )
            .optional()?;
        let stored: Option<(Option<String>, String, i64, i64)> = tx
            .query_row(
                "SELECT parent_uid, name, is_dir, trashed FROM nodes WHERE uid = ?1",
                params![uid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let access: Option<String> = tx
            .query_row(
                "SELECT access FROM share_access WHERE root_uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .optional()?;
        let unchanged = stored.is_some_and(|(parent, name, is_dir, trashed)| {
            parent == desired_parent
                && name == node.name
                && is_dir == 1
                && trashed == node.trashed as i64
        }) && access.as_deref() == Some(Access::Viewer.as_db_str())
            && pinned_name.is_some();
        if unchanged {
            tx.commit()?;
            return Ok(false);
        }
        upsert_share_access_tx(&tx, &uid, Access::Viewer)?;
        upsert_node_tx(&tx, node)?;
        tx.execute(
            "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO NOTHING",
            params![pinned_name_key, node.name],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Drop a node row (delete or trash from the hot cache). Children rows are
    /// not cascaded here; the daemon forgets a whole subtree node-by-node.
    pub fn delete_node(&self, uid: &NodeUid) -> Result<()> {
        let uid = uid.to_string();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        // Read the rowid before dropping the node: it is the search index's key
        // (B12), and it is gone once the row is. A node with no row leaves
        // nothing to unindex — the index is keyed off `nodes`, so it cannot
        // hold an entry the table never had.
        let rowid: Option<i64> = tx
            .query_row(
                "SELECT rowid FROM nodes WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute("DELETE FROM nodes WHERE uid = ?1", params![uid])?;
        if let Some(rowid) = rowid {
            tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", params![rowid])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically retire a landed trash operation and its retained authority.
    ///
    /// Queued trash keeps the node row so drain-time permission checks can
    /// resolve its shared-tree access. Removing the op first could resurrect
    /// that row after a crash; removing the row first could strand a now
    /// unauthorizable op. One transaction closes both windows.
    ///
    /// An op that found the name still held by this trash failed with a
    /// backoff, and the name is free now. It is made due at once
    /// ([`Db::wake_ops_waiting_for`]).
    ///
    /// Returns the staged blob of the create the trash withdrew, if it kept
    /// one, for the caller to discard.
    pub fn complete_trash_op(&self, op_id: i64, uid: &NodeUid) -> Result<Option<String>> {
        let uid = uid.to_string();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let (name, blob): (Option<String>, Option<String>) = tx
            .query_row(
                "SELECT name, blob_path FROM pending_op WHERE id = ?1",
                params![op_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or_default();
        let rowid: Option<i64> = tx
            .query_row(
                "SELECT rowid FROM nodes WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute("DELETE FROM nodes WHERE uid = ?1", params![uid])?;
        if let Some(rowid) = rowid {
            tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", params![rowid])?;
        }
        tx.execute("DELETE FROM pending_op WHERE id = ?1", params![op_id])?;
        if let Some(name) = name {
            wake_ops_waiting_for_tx(&tx, &name)?;
        }
        tx.commit()?;
        Ok(blob)
    }

    /// Check if a folder node has any non-trashed children in the database.
    pub fn has_children(&self, parent_uid: &NodeUid) -> Result<bool> {
        let conn = self.read();
        let (key, parent) = parent_key(&conn, &parent_uid.to_string())?;
        let count: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM nodes
                 WHERE {key} = ?1 AND trashed = 0 AND node_json IS NOT NULL"
            ),
            params![parent],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// A node's mountpoint-relative path, or `None` when it is not cached.
    ///
    /// One indexed read of the stored `path`. Callers that need to relate many
    /// nodes to a handful of roots — search decorating each hit with its
    /// mountpoint — resolve the roots once through this and do the rest as
    /// string arithmetic, instead of a `path_relative_to` walk per pair.
    pub fn node_path(&self, uid: &str) -> Result<Option<String>> {
        let conn = self.read();
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM nodes WHERE uid = ?1 AND node_json IS NOT NULL",
                params![uid],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(None);
        }
        Ok(Some(path_of(&conn, uid)?))
    }

    /// Resolve `uid` relative to an ancestor node.
    ///
    /// This is used when a remote subtree has its own local sync mount: the
    /// sync folder stores the ancestor UID, while search results identify the
    /// selected descendant. Returning `None` for an unrelated or incomplete
    /// chain keeps callers from accidentally joining a Drive-wide path onto the
    /// wrong mountpoint. The ancestor itself resolves to the empty path.
    pub fn path_relative_to(&self, ancestor_uid: &str, uid: &str) -> Result<Option<String>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE anc(uid, parent_lid, name, depth) AS (
               SELECT uid, parent_lid, name, 0 FROM nodes WHERE uid = ?1
               UNION ALL
               SELECT n.uid, n.parent_lid, n.name, anc.depth + 1
               FROM nodes n JOIN anc ON n.lid = anc.parent_lid
               WHERE anc.depth < 1024
             )
             SELECT uid, name FROM anc ORDER BY depth",
        )?;
        let rows = stmt.query_map(params![uid], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut parts = Vec::new();
        for row in rows {
            let (current_uid, name) = row?;
            if current_uid == ancestor_uid {
                parts.reverse();
                return Ok(Some(parts.join("/")));
            }
            parts.push(name);
        }
        Ok(None)
    }

    /// Full-text search over node names, newest schema's trigram index giving
    /// substring (not just prefix) matches. Returns up to `limit` non-trashed
    /// hits, each with its mountpoint-relative path resolved. Queries shorter
    /// than [`TRIGRAM_MIN`] fall back to a `LIKE` scan since trigram indexes
    /// nothing below 3 chars.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.search_in(query, limit, None)
    }

    /// [`search`](Self::search) restricted to the subtree under the
    /// mountpoint-relative folder `scope`. `None` or an empty scope searches
    /// everything.
    pub fn search_in(
        &self,
        query: &str,
        limit: usize,
        scope: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let scope = scope.map(|s| s.trim_matches('/')).filter(|s| !s.is_empty());
        // Rows whose path is not persisted yet pass the SQL filter and are
        // checked once their path is walked below.
        let scope_pat = scope.map(|s| format!("{}/%", like_escape(s)));
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read();
        let rows: Vec<HitRow> = if query.chars().count() < TRIGRAM_MIN {
            let pat = format!("%{}%", like_escape(query));
            let mut stmt = conn.prepare(&format!(
                "SELECT {NODE_NOW}, n.uid, n.path
                 FROM nodes n LEFT JOIN nodes p ON p.lid = n.parent_lid
                 WHERE n.name LIKE ?1 ESCAPE '\\' AND n.trashed = 0 AND n.node_json IS NOT NULL
                   AND (?3 IS NULL OR n.path IS NULL OR n.path LIKE ?3 ESCAPE '\\')
                 ORDER BY n.name LIMIT ?2"
            ))?;
            collect_hits(stmt.query_map(params![pat, limit as i64, scope_pat], hit_row)?)?
        } else {
            // Escape double quotes and quote each term, then combine with AND so
            // all terms must match but can appear in any order or position.
            let phrase = query
                .split_whitespace()
                .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut stmt = conn.prepare(&format!(
                "SELECT {NODE_NOW}, n.uid, n.path
                 FROM nodes_fts f JOIN nodes n ON n.rowid = f.rowid
                 LEFT JOIN nodes p ON p.lid = n.parent_lid
                 WHERE f.name MATCH ?1 AND n.trashed = 0 AND n.node_json IS NOT NULL
                   AND (?3 IS NULL OR n.path IS NULL OR n.path LIKE ?3 ESCAPE '\\')
                 ORDER BY f.rank LIMIT ?2"
            ))?;
            collect_hits(stmt.query_map(params![phrase, limit as i64, scope_pat], hit_row)?)?
        };

        let mut hits = Vec::with_capacity(rows.len());
        for (json, uid, path) in rows {
            let node: Node = serde_json::from_str(&json)?;
            let path = match path {
                Some(path) => path,
                None => walk_path_of(&conn, &uid)?,
            };
            if let Some(scope) = scope
                && !path
                    .strip_prefix(scope)
                    .is_some_and(|rest| rest.starts_with('/'))
            {
                continue;
            }
            hits.push(SearchHit { node, path });
        }
        Ok(hits)
    }

    /// Return a bounded, deliberately broad candidate pool for fuzzy ranking.
    /// Unlike [`search`](Self::search), trigram terms are ORed across both the
    /// basename and parent path, so a typo can still share enough trigrams to
    /// enter the pool. Final relevance ordering belongs to the caller.
    pub fn search_candidates(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.read();
        let terms = candidate_trigrams(query);
        let lane_limit = limit.div_ceil(2);
        let mut rows: Vec<HitRow> = if terms.is_empty() {
            let pat = format!("%{}%", like_escape(query));
            let mut stmt = conn.prepare(&format!(
                "SELECT {NODE_NOW}, n.uid, n.path
                 FROM nodes n LEFT JOIN nodes p ON p.lid = n.parent_lid
                 WHERE n.name LIKE ?1 ESCAPE '\\' AND n.trashed = 0 AND n.node_json IS NOT NULL
                 ORDER BY n.name LIMIT ?2"
            ))?;
            collect_hits(stmt.query_map(params![pat, limit as i64], hit_row)?)?
        } else {
            let expression = terms.join(" OR ");
            let mut stmt = conn.prepare(&format!(
                "SELECT {NODE_NOW}, n.uid, n.path
                   FROM nodes_fts f JOIN nodes n ON n.rowid = f.rowid
                   LEFT JOIN nodes p ON p.lid = n.parent_lid
                  WHERE nodes_fts MATCH ?1 AND n.trashed = 0 AND n.node_json IS NOT NULL
                  ORDER BY f.rank LIMIT ?2"
            ))?;
            collect_hits(stmt.query_map(params![expression, lane_limit as i64], hit_row)?)?
        };
        // Trigram rank does not know that a name *starts* with the query, and a
        // short substitution can destroy every trigram outright (`vedio` vs
        // `video`). Two indexed lanes cover both without scanning the node
        // table: whole-query prefix first, then same-initial.
        if !terms.is_empty()
            && let Some(first) = query.chars().next()
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT {NODE_NOW}, n.uid, n.path
                 FROM nodes n LEFT JOIN nodes p ON p.lid = n.parent_lid
                 WHERE n.name COLLATE NOCASE LIKE ?1 ESCAPE '\\'
                   AND n.trashed = 0 AND n.node_json IS NOT NULL
                 ORDER BY n.name COLLATE NOCASE LIMIT ?2"
            ))?;
            let mut extra = Vec::new();
            for pattern in [
                format!("{}%", like_escape(query)),
                format!("{}%", like_escape(&first.to_string())),
            ] {
                extra.extend(collect_hits(
                    stmt.query_map(params![pattern, lane_limit as i64], hit_row)?,
                )?);
            }
            let trigram = std::mem::take(&mut rows);
            for row in extra.into_iter().chain(trigram) {
                if !rows.iter().any(|(_, uid, _)| uid == &row.1) {
                    rows.push(row);
                }
            }
            rows.truncate(limit);
        }
        rows.into_iter()
            .map(|(json, uid, path)| {
                Ok(SearchHit {
                    node: serde_json::from_str(&json)?,
                    path: match path {
                        Some(path) => path,
                        None => walk_path_of(&conn, &uid)?,
                    },
                })
            })
            .collect()
    }

    /// Mark (or unmark) a folder's child listing as complete. A listed folder
    /// rehydrates its `children` map on mount even when empty; an unlisted one
    /// re-enumerates from the remote on next access.
    pub fn set_listed(&self, uid: &NodeUid, listed: bool) -> Result<()> {
        let conn = self.conn.lock();
        let (key, uid) = row_key(&uid.to_string());
        conn.execute(
            &format!("UPDATE nodes SET listed = ?2 WHERE {key} = ?1"),
            params![uid, listed as i64],
        )?;
        Ok(())
    }

    /// Load every persisted node for cold-start hydration of the `State` maps.
    ///
    /// Each node names its parent as the parent's row has it now, found by
    /// local id: a folder that landed has its new uid there, while the node
    /// stored below it may still hold the placeholder.
    pub fn load_all(&self) -> Result<Vec<StoredNode>> {
        let conn = self.read();
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_NOW}, n.listed, n.lid
             FROM nodes n LEFT JOIN nodes p ON p.lid = n.parent_lid
             WHERE n.node_json IS NOT NULL"
        ))?;
        let rows = stmt.query_map([], |row| {
            let json: String = row.get(0)?;
            let listed: i64 = row.get(1)?;
            Ok((json, listed != 0, row.get(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (json, listed, lid) = row?;
            let node: Node = serde_json::from_str(&json)?;
            out.push(StoredNode { node, listed, lid });
        }
        Ok(out)
    }

    /// Load one persisted node back by uid. Used to recover the My Files root
    /// when the API is unreachable, so the mount can still serve the cached tree.
    pub fn node_by_uid(&self, uid: &str) -> Result<Option<Node>> {
        let conn = self.read();
        let json: Option<String> = conn
            .query_row(
                &format!(
                    "SELECT {NODE_NOW} FROM nodes n LEFT JOIN nodes p ON p.lid = n.parent_lid
                     WHERE n.uid = ?1 AND n.node_json IS NOT NULL"
                ),
                params![uid],
                |r| r.get(0),
            )
            .optional()?;
        match json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// The uid of the node with local id `lid`: `None` when it has no row, or
    /// is not on Drive yet.
    pub fn uid_of_lid(&self, lid: i64) -> Result<Option<String>> {
        let conn = self.read();
        Ok(conn
            .query_row("SELECT uid FROM nodes WHERE lid = ?1", params![lid], |r| {
                r.get(0)
            })
            .optional()?
            .flatten())
    }

    /// The local id of the node stored under `uid`, or `None` when it has no
    /// row. It stays the same for as long as the row exists, including when a
    /// create lands and the row takes the uid Drive gave it: a node made on
    /// this machine is found by its stand-in ([`local_uid`]) before and after.
    pub fn lid_of(&self, uid: &str) -> Result<Option<i64>> {
        node_lid(&self.read(), uid)
    }

    /// The local id of each node, in order, giving a row to each that has none.
    ///
    /// A new row is only a stub: it has no `node_json`, so no listing or search
    /// returns it until the node is upserted, which the caller still owes. That
    /// keeps this cheap enough to call with the inode lock held, which is
    /// where an inode number is first needed.
    pub fn lids_for(&self, nodes: &[&Node]) -> Result<Vec<i64>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut lids = Vec::with_capacity(nodes.len());
        {
            let mut known = tx.prepare_cached("SELECT lid FROM nodes WHERE uid = ?1")?;
            let mut stub = tx.prepare_cached(
                "INSERT INTO nodes (uid, parent_uid, name, is_dir, mtime, trashed, parent_lid)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for node in nodes {
                let uid = node.uid.to_string();
                let lid = match known.query_row(params![uid], |r| r.get(0)).optional()? {
                    Some(lid) => lid,
                    None => {
                        let parent = node.parent_uid.as_ref().map(ToString::to_string);
                        let parent_lid = match &parent {
                            Some(parent) => node_lid(&tx, parent)?,
                            None => None,
                        };
                        stub.execute(params![
                            uid,
                            parent,
                            node.name,
                            node.is_folder() as i64,
                            node.modification_time,
                            node.trashed as i64,
                            parent_lid,
                        ])?;
                        let lid = tx.last_insert_rowid();
                        adopt_children_tx(&tx, &uid, lid)?;
                        lid
                    }
                };
                lids.push(lid);
            }
        }
        tx.commit()?;
        Ok(lids)
    }

    /// Give a node being made on this machine its row under `parent_uid`, and
    /// return its local id. The row goes by [`local_uid`] of that id until
    /// the node lands.
    ///
    /// Like [`Db::lids_for`], only a stub: the caller upserts the node.
    pub fn add_local_row(
        &self,
        parent_uid: &str,
        name: &str,
        is_dir: bool,
        mtime: i64,
    ) -> Result<i64> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO nodes (uid, parent_uid, name, is_dir, mtime, trashed, parent_lid)
             VALUES (NULL, ?1, ?2, ?3, ?4, 0,
                     COALESCE(?5, (SELECT lid FROM nodes WHERE uid = ?1)))",
            params![
                parent_uid,
                name,
                is_dir as i64,
                mtime,
                local_lid(parent_uid)
            ],
        )?;
        let lid = tx.last_insert_rowid();
        tx.execute(
            "UPDATE nodes SET uid = ?2 WHERE lid = ?1",
            params![lid, local_uid(lid)],
        )?;
        tx.commit()?;
        Ok(lid)
    }

    /// Read the persisted incremental-sync cursor (a `DriveEventId`), if any.
    /// The daemon resumes from this on restart instead of reseeding to the
    /// server head, so changes made while unmounted are still applied (P2).
    pub fn children_if_listed(&self, parent: &NodeUid) -> Result<Option<Vec<Node>>> {
        let conn = self.read();
        let (key, folder) = row_key(&parent.to_string());
        let listed: Option<i64> = conn
            .query_row(
                &format!("SELECT listed FROM nodes WHERE {key} = ?1"),
                params![folder],
                |r| r.get(0),
            )
            .optional()?;
        if listed != Some(1) {
            return Ok(None);
        }
        drop(conn);
        self.known_children(parent).map(Some)
    }

    /// The children the DB knows under `parent`, whether or not the folder's
    /// listing is still marked complete. Only for when nothing better can be
    /// had: a listing dropped as stale may miss or keep a child.
    pub fn known_children(&self, parent: &NodeUid) -> Result<Vec<Node>> {
        let conn = self.read();
        let (key, parent) = parent_key(&conn, &parent.to_string())?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_NOW} FROM nodes n LEFT JOIN nodes p ON p.lid = n.parent_lid
             WHERE n.{key} = ?1 AND n.node_json IS NOT NULL AND n.trashed = 0"
        ))?;
        let rows = stmt.query_map(params![parent], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(out)
    }

    /// What a queued change does to `parent`'s listing, which Drive does not
    /// show until it lands: the children made or moved in by a queued create,
    /// mkdir or rename, as the DB holds them, and the nodes a queued rename
    /// moves elsewhere.
    ///
    /// A listing read from Drive is laid over with these, or a folder listed
    /// while a create in it is queued, after a restart say, loses the file
    /// until the create lands (`docs/BUGS.md` B132).
    pub fn queued_children(&self, parent: &NodeUid) -> Result<(Vec<Node>, HashSet<String>)> {
        let conn = self.read();
        let (key, parent) = parent_key(&conn, &parent.to_string())?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_NOW} FROM nodes n LEFT JOIN nodes p ON p.lid = n.parent_lid
             WHERE n.{key} = ?1 AND n.node_json IS NOT NULL AND n.trashed = 0
               AND EXISTS (SELECT 1 FROM pending_op o
                           WHERE (o.lid = n.lid OR o.uid = n.uid) AND o.kind IN (?2, ?3, ?4))"
        ))?;
        let rows = stmt.query_map(params![parent, OP_CREATE, OP_MKDIR, OP_RENAME], |r| {
            r.get::<_, String>(0)
        })?;
        let mut here = Vec::new();
        for json in rows {
            here.push(serde_json::from_str(&json?)?);
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT n.uid FROM pending_op p JOIN nodes n ON n.lid = p.lid OR n.uid = p.uid
             WHERE p.kind = ?2 AND n.{key} IS NOT ?1"
        ))?;
        let gone = stmt
            .query_map(params![parent, OP_RENAME], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok((here, gone))
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

/// What a node's row looked like before this upsert, for the fields that decide
/// whether the subtree below it has to be touched at all.
struct PriorRow {
    parent_uid: Option<String>,
    name: String,
    path: Option<String>,
    trashed: bool,
}

/// [`Db::lid_of`] on `conn`, which may be a transaction.
pub(super) fn node_lid(conn: &rusqlite::Connection, uid: &str) -> Result<Option<i64>> {
    let row = match local_lid(uid) {
        Some(lid) => conn.query_row("SELECT lid FROM nodes WHERE lid = ?1", params![lid], |r| {
            r.get(0)
        }),
        None => conn.query_row("SELECT lid FROM nodes WHERE uid = ?1", params![uid], |r| {
            r.get(0)
        }),
    };
    Ok(row.optional()?)
}

/// The column and value that find `uid`'s own row. A stand-in is found by its
/// local id: a create that lands gives the row the uid Drive made before the
/// tree has it, and the tree asks by the stand-in until then (`docs/BUGS.md`
/// B177).
fn row_key(uid: &str) -> (&'static str, Value) {
    match local_lid(uid) {
        Some(lid) => ("lid", Value::Integer(lid)),
        None => ("uid", Value::Text(uid.to_owned())),
    }
}

/// The column and value a row names `parent` by: its local id when it has a
/// row, which the children keep when it lands, or its uid when it has none,
/// as a device folder's root does.
fn parent_key(conn: &rusqlite::Connection, parent: &str) -> Result<(&'static str, Value)> {
    Ok(match node_lid(conn, parent)? {
        Some(lid) => ("parent_lid", Value::Integer(lid)),
        None => ("parent_uid", Value::Text(parent.to_owned())),
    })
}

/// `n.node_json` naming its parent as the parent's row `p`, joined by
/// `n.parent_lid`, has it now. A folder that lands takes its real uid in its
/// own row only, so a node stored below it still names the stand-in.
const NODE_NOW: &str = "CASE WHEN p.uid IS NULL OR p.uid = n.parent_uid THEN n.node_json
     ELSE json_set(n.node_json, '$.parent_uid', json_object(
            'volume_id', substr(p.uid, 1, instr(p.uid, '~') - 1),
            'link_id', substr(p.uid, instr(p.uid, '~') + 1))) END";

/// Hand a drained placeholder's row to the real uid it landed as.
///
/// Nothing else is rewritten. The rows below it and the ops made inside it
/// name it by its local id, which stays; what reads them for a uid takes the
/// one the row has now ([`NODE_NOW`], `PARENT_NOW` in `ops.rs`).
///
/// Dropping the placeholder row and waiting for the server's copy of the real
/// node left a window with no row for either uid. A queued child drained in
/// that window found no authority for its parent, re-read it over the network
/// and was deferred five seconds behind a folder that had just landed
/// (`docs/BUGS.md` B120). The placeholder's own node, readdressed, stands in
/// until the server's copy is upserted over it. A row the real uid already
/// has is newer than the placeholder, so its node is kept, under the
/// placeholder's `lid`: the node was known by that one first.
pub(super) fn adopt_placeholder_row_tx(
    tx: &Transaction<'_>,
    local: &str,
    real: &str,
) -> Result<()> {
    // A folder pinned before it landed stays pinned: the pin keyed by the
    // placeholder was dropped with it (`docs/BUGS.md` B148).
    tx.execute(
        "UPDATE OR IGNORE pins SET uid = ?2 WHERE uid = ?1",
        params![local, real],
    )?;
    tx.execute("DELETE FROM pins WHERE uid = ?1", params![local])?;
    let row: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT lid, node_json FROM nodes WHERE uid = ?1",
            params![local],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    // Ops queued before schema 39 have no `parent_lid`, nor do ops made
    // under a placeholder that never had a row.
    tx.execute(
        "UPDATE pending_op SET parent_uid = ?2 WHERE parent_lid IS NULL AND parent_uid = ?1",
        params![local, real],
    )?;
    let Some((lid, json)) = row else {
        return Ok(());
    };
    let known: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT lid, node_json FROM nodes WHERE uid = ?1",
            params![real],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((known_lid, _)) = known {
        // Ops queued against the row Drive listed now name the landing one.
        tx.execute(
            "UPDATE pending_op SET lid = ?2 WHERE lid = ?1",
            params![known_lid, lid],
        )?;
        tx.execute(
            "UPDATE pending_op SET parent_lid = ?2 WHERE parent_lid = ?1",
            params![known_lid, lid],
        )?;
        tx.execute(
            "UPDATE nodes SET parent_lid = ?2 WHERE parent_lid = ?1",
            params![known_lid, lid],
        )?;
        tx.execute("DELETE FROM nodes WHERE lid = ?1", params![known_lid])?;
        tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", params![known_lid])?;
    }
    tx.execute(
        "UPDATE nodes SET uid = ?2 WHERE lid = ?1",
        params![lid, real],
    )?;
    let json = known.and_then(|(_, known_json)| known_json).or(json);
    let (Some(json), Some(real)) = (json, parse_node_uid(real)) else {
        return Ok(());
    };
    let mut node: Node = serde_json::from_str(&json)?;
    node.uid = real;
    upsert_node_tx(tx, &node)
}

/// Link the rows that name `uid` as their parent to its new row `lid`: they
/// were stored before it, or under a row of the same uid since dropped.
fn adopt_children_tx(tx: &Transaction<'_>, uid: &str, lid: i64) -> Result<()> {
    tx.execute(
        "UPDATE nodes SET parent_lid = ?2 WHERE parent_uid = ?1 AND parent_lid IS NOT ?2",
        params![uid, lid],
    )?;
    Ok(())
}

fn upsert_node_tx(tx: &Transaction<'_>, node: &Node) -> Result<()> {
    let uid = node.uid.to_string();
    // A node written under its stand-in after it landed is its row, which has
    // the uid Drive gave it by now: written under the stand-in, it would bring
    // the placeholder row back beside it.
    if let Some(lid) = local_lid(&uid) {
        let landed: Option<String> = tx
            .query_row(
                "SELECT uid FROM nodes WHERE lid = ?1 AND uid IS NOT ?2",
                params![lid, uid],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(real) = landed.as_deref().and_then(parse_node_uid) {
            let mut node = node.clone();
            node.uid = real;
            return upsert_node_tx(tx, &node);
        }
    }
    let json = serde_json::to_string(node)?;
    let parent_uid = node.parent_uid.as_ref().map(|u| u.to_string());

    let prior: Option<PriorRow> = tx
        .query_row(
            "SELECT parent_uid, name, path, trashed FROM nodes WHERE uid = ?1",
            params![uid],
            |row| {
                Ok(PriorRow {
                    parent_uid: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    trashed: row.get::<_, i64>(3)? != 0,
                })
            },
        )
        .optional()?;

    // A node's path is its parent's path plus its own name — one indexed lookup,
    // where resolving it from scratch is a recursive walk to the root. A node
    // whose parent is not cached (a device folder's root lives in `device`, never
    // in `nodes`) starts a path of its own, which is what the walk did too.
    let (parent_lid, path) = match &parent_uid {
        None => (None, String::new()),
        Some(parent) => {
            let parent_row: Option<(i64, Option<String>)> = match node_lid(tx, parent)? {
                Some(lid) => tx
                    .query_row(
                        "SELECT lid, path FROM nodes WHERE lid = ?1",
                        params![lid],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?,
                None => None,
            };
            let (lid, parent_path) = parent_row.unzip();
            (
                lid,
                join_path(parent_path.flatten().as_deref().unwrap_or(""), &node.name),
            )
        }
    };

    tx.execute(
        "INSERT INTO nodes
           (uid, parent_uid, name, is_dir, size, mtime, trashed, node_json, path, parent_lid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(uid) DO UPDATE SET
           parent_uid = excluded.parent_uid,
           parent_lid = excluded.parent_lid,
           name       = excluded.name,
           is_dir     = excluded.is_dir,
           size       = excluded.size,
           mtime      = excluded.mtime,
           trashed    = excluded.trashed,
           node_json  = excluded.node_json,
           path       = excluded.path",
        params![
            uid,
            parent_uid,
            node.name,
            node.is_folder() as i64,
            node_size(node),
            node.modification_time,
            node.trashed as i64,
            json,
            path,
            parent_lid,
        ],
    )?;
    let rowid: i64 = tx.query_row(
        "SELECT rowid FROM nodes WHERE uid = ?1",
        params![uid],
        |row| row.get(0),
    )?;
    if prior.is_none() {
        adopt_children_tx(tx, &uid, rowid)?;
    }

    // FTS5 has no UPSERT, so the node's own row is always replaced.
    let indexable = node_is_indexable_tx(tx, &uid)?;
    tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", params![rowid])?;
    if indexable {
        tx.execute(
            "INSERT INTO nodes_fts (rowid, name, path) VALUES (?1, ?2, ?3)",
            params![rowid, node.name, path],
        )?;
    }

    // Everything below only matters when the subtree's paths or reachability
    // actually moved. Re-listing a folder upserts it unchanged, and paying for a
    // full descendant walk on each of those was most of what a large tree spent
    // its time on.
    let subtree_moved = prior.is_none_or(|prior| {
        prior.parent_uid != parent_uid
            || prior.name != node.name
            || prior.path.as_deref() != Some(path.as_str())
            || prior.trashed != node.trashed
    });
    if node.is_folder() && subtree_moved {
        reindex_subtree_tx(tx, rowid, &path, indexable)?;
    }
    Ok(())
}

/// Rewrite every descendant's stored path and search-index row after `folder`
/// moved, was renamed, or changed reachability.
///
/// One recursive walk carries both the new path and whether anything on the way
/// down is trashed, so each descendant costs two trivial statements instead of a
/// `path_of` walk plus an ancestor walk of its own. `folder_indexable` is the
/// answer `node_is_indexable_tx` gave for the folder itself: what is above the
/// subtree is the same for every node in it.
///
/// The depth cap is a liveness guard, not a policy: `UNION ALL` over a parent
/// cycle (a's parent is b, b's parent is a — corrupt data, but the API can hand
/// it to us) never terminates, and this runs inside the write transaction that
/// holds the daemon's only SQLite connection. Real Drive trees are nowhere near
/// this deep.
fn reindex_subtree_tx(
    tx: &Transaction<'_>,
    folder: i64,
    folder_path: &str,
    folder_indexable: bool,
) -> Result<()> {
    let descendants: Vec<(i64, String, bool)> = {
        let mut stmt = tx.prepare(
            "WITH RECURSIVE sub(rowid, name, path, blocked, depth) AS (
               SELECT n.rowid, n.name,
                      CASE WHEN ?2 = '' THEN n.name ELSE ?2 || '/' || n.name END,
                      n.trashed, 0
                 FROM nodes n WHERE n.parent_lid = ?1
               UNION ALL
               SELECT n.rowid, n.name,
                      sub.path || '/' || n.name,
                      MAX(sub.blocked, n.trashed), sub.depth + 1
                 FROM nodes n JOIN sub ON n.parent_lid = sub.rowid
                WHERE sub.depth < 256
             )
             SELECT rowid, path, blocked FROM sub",
        )?;
        let rows = stmt.query_map(params![folder, folder_path], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        out
    };
    if descendants.is_empty() {
        return Ok(());
    }

    let mut set_path = tx.prepare("UPDATE nodes SET path = ?2 WHERE rowid = ?1")?;
    let mut unindex = tx.prepare("DELETE FROM nodes_fts WHERE rowid = ?1")?;
    let mut index = tx.prepare(
        // Not a stub row (`Db::lids_for`): it is indexed when its node is
        // written.
        "INSERT INTO nodes_fts (rowid, name, path)
         SELECT rowid, name, ?2 FROM nodes WHERE rowid = ?1 AND node_json IS NOT NULL",
    )?;
    for (rowid, path, blocked) in descendants {
        set_path.execute(params![rowid, path])?;
        unindex.execute(params![rowid])?;
        if folder_indexable && !blocked {
            index.execute(params![rowid, path])?;
        }
    }
    Ok(())
}

fn direct_child_uids_tx(tx: &Transaction<'_>, parent: &str) -> Result<Vec<String>> {
    let (key, parent) = parent_key(tx, parent)?;
    let mut stmt = tx.prepare(&format!("SELECT uid FROM nodes WHERE {key} = ?1"))?;
    let rows = stmt.query_map([parent], |row| row.get::<_, String>(0))?;
    let mut uids = Vec::new();
    for row in rows {
        uids.push(row?);
    }
    Ok(uids)
}

/// Whether a node belongs in the search index: it exists, and nothing on the
/// way up to the top of its cached chain is trashed.
///
/// The chain does *not* have to reach a `parent_uid IS NULL` row. Only the My
/// Files root is stored that way; a device folder's root lives in `device` /
/// `sync_folder` and never gets a `nodes` row, so requiring a null-parent
/// ancestor silently excluded every device-folder subtree from search — 29% of
/// the nodes on the account this was found on. A subtree whose topmost cached
/// row has an uncached parent is ordinary, reachable data.
///
/// What is still excluded is a *cycle*: the walk's own guard stops it, but
/// [`path_of`] has no such guard, so an indexed cycle would hang a search. A
/// clean chain ends at a row whose parent is null or simply not cached; a cycle
/// ends at one whose parent is a row we have already visited.
fn node_is_indexable_tx(tx: &Transaction<'_>, uid: &str) -> Result<bool> {
    let indexable: i64 = tx.query_row(
        "WITH RECURSIVE ancestors(lid, parent_lid, trashed, depth, path) AS (
           SELECT lid, parent_lid, trashed, 0, ',' || lid || ','
             FROM nodes WHERE uid = ?1
           UNION ALL
           SELECT n.lid, n.parent_lid, n.trashed, a.depth + 1,
                  a.path || n.lid || ','
             FROM ancestors a JOIN nodes n ON n.lid = a.parent_lid
            WHERE instr(a.path, ',' || n.lid || ',') = 0
         )
         SELECT CASE
           WHEN COUNT(*) = 0 THEN 0
           WHEN MAX(trashed) != 0 THEN 0
           WHEN EXISTS (
             SELECT 1 FROM ancestors a
              WHERE a.depth = (SELECT MAX(depth) FROM ancestors)
                AND EXISTS (SELECT 1 FROM nodes p WHERE p.lid = a.parent_lid)
           ) THEN 0
           ELSE 1
         END
         FROM ancestors",
        [uid],
        |row| row.get(0),
    )?;
    Ok(indexable == 1)
}

fn tombstone_subtree_tx(tx: &Transaction<'_>, root_uid: &str) -> Result<()> {
    let rows = {
        let mut stmt = tx.prepare(
            "WITH RECURSIVE subtree(rowid, uid, node_json) AS (
               SELECT rowid, uid, node_json FROM nodes WHERE uid = ?1
               UNION
               SELECT n.rowid, n.uid, n.node_json FROM nodes n
                 JOIN subtree s ON n.parent_lid = s.rowid
             )
             SELECT rowid, uid, node_json FROM subtree",
        )?;
        let mapped = stmt.query_map([root_uid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut rows = Vec::new();
        for row in mapped {
            rows.push(row?);
        }
        rows
    };
    for (rowid, uid, json) in rows {
        let json = match json {
            Some(json) => {
                let mut node: Node = serde_json::from_str(&json)?;
                node.trashed = true;
                Some(serde_json::to_string(&node)?)
            }
            None => None,
        };
        tx.execute(
            "UPDATE nodes SET trashed = 1, listed = 0, node_json = ?2 WHERE uid = ?1",
            params![uid, json],
        )?;
        tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", [rowid])?;
    }
    Ok(())
}

fn upsert_share_access_tx(tx: &Transaction<'_>, uid: &str, access: Access) -> Result<()> {
    tx.execute(
        "INSERT INTO share_access (root_uid, access) VALUES (?1, ?2)
         ON CONFLICT(root_uid) DO UPDATE SET access = excluded.access",
        params![uid, access.as_db_str()],
    )?;
    Ok(())
}

fn parse_node_uid(value: &str) -> Option<NodeUid> {
    let (volume, link) = value.split_once('~')?;
    Some(NodeUid::new(VolumeId::from(volume), LinkId::from(link)))
}

/// Unique quoted character trigrams suitable for an FTS5 OR expression.
pub(super) fn candidate_trigrams(query: &str) -> Vec<String> {
    let chars: Vec<char> = query.to_lowercase().chars().collect();
    let mut terms = Vec::new();
    for window in chars.windows(TRIGRAM_MIN) {
        if window.iter().all(|c| c.is_whitespace()) {
            continue;
        }
        let raw: String = window.iter().collect();
        let quoted = format!("\"{}\"", raw.replace('"', "\"\""));
        if !terms.contains(&quoted) {
            terms.push(quoted);
        }
    }
    terms
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
