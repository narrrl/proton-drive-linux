//! Pinned nodes: files and folders the user has asked to keep on disk, exempt
//! from LRU eviction. A recursive pin covers a whole subtree.

use rusqlite::{OptionalExtension, params};

use super::Db;
use super::utils::MAX_PATH_DEPTH;
use crate::Result;

/// One row of [`Db::pin_list`]. `recursive` is pin *policy* — whether the whole
/// subtree is kept — while `is_dir` is the node's kind as the node cache knows
/// it, or `None` for a pin whose node is not cached. Reading the former as the
/// latter is what made a non-recursively pinned folder render as a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinRow {
    pub uid: String,
    pub path: String,
    pub recursive: bool,
    pub is_dir: Option<bool>,
}

impl Db {
    pub fn pin_add(&self, uid: &str, path: &str, recursive: bool) -> Result<()> {
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM pins WHERE uid = ?1", params![uid])?;
        Ok(n > 0)
    }

    /// Every directly-pinned entry, ordered by uid.
    pub fn pin_list(&self) -> Result<Vec<PinRow>> {
        let conn = self.read();
        // `recursive` is pin policy; the node's kind comes from `nodes`, and is
        // NULL for a pin whose node was never cached.
        let mut stmt = conn.prepare(
            "SELECT p.uid, p.path, p.recursive, n.is_dir
               FROM pins p LEFT JOIN nodes n ON n.uid = p.uid
              ORDER BY p.uid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PinRow {
                uid: r.get(0)?,
                path: r.get(1)?,
                recursive: r.get::<_, i64>(2)? != 0,
                is_dir: r.get::<_, Option<i64>>(3)?.map(|is_dir| is_dir != 0),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Whether `uid` is pinned — directly, or because a strict ancestor folder
    /// is pinned recursively. The ancestor check walks `parent_lid` to the root
    /// via a CTE; a direct pin is honoured even when the node has no `nodes` row
    /// yet (e.g. a CLI that never hydrates the node cache).
    ///
    /// The walk is depth-capped like [`super::utils::path_of`]: a parent
    /// cycle is corrupt data the API can hand us, and `UNION ALL` over one never
    /// terminates — while holding the daemon's only SQLite connection, on a path
    /// that runs per cached read.
    pub fn is_pinned(&self, uid: &str) -> Result<bool> {
        let conn = self.read();
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
                &format!(
                    "WITH RECURSIVE anc(uid, parent_lid, depth) AS (
                       SELECT uid, parent_lid, 0 FROM nodes WHERE uid = ?1
                       UNION ALL
                       SELECT n.uid, n.parent_lid, anc.depth + 1
                       FROM nodes n JOIN anc ON n.lid = anc.parent_lid
                       WHERE anc.depth < {MAX_PATH_DEPTH}
                     )
                     SELECT 1 FROM anc a JOIN pins p ON p.uid = a.uid
                     WHERE a.uid != ?1 AND p.recursive = 1
                     LIMIT 1"
                ),
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
        let conn = self.read();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE sub(uid, lid) AS (
               SELECT p.uid, n.lid FROM pins p LEFT JOIN nodes n ON n.uid = p.uid
                WHERE p.recursive = 1
               UNION
               SELECT n.uid, n.lid FROM nodes n JOIN sub ON n.parent_lid = sub.lid
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

    /// Every descendant uid of `folder` (all depths), via a `parent_lid` CTE.
    /// Used to evict a recursively-pinned subtree's cached blobs on unpin.
    pub fn descendants(&self, folder: &str) -> Result<Vec<String>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE sub(uid, lid) AS (
               SELECT c.uid, c.lid FROM nodes c JOIN nodes f ON c.parent_lid = f.lid
                WHERE f.uid = ?1
               UNION
               SELECT n.uid, n.lid FROM nodes n JOIN sub ON n.parent_lid = sub.lid
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
}
