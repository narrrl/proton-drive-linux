//! Pinned nodes: files and folders the user has asked to keep on disk, exempt
//! from LRU eviction. A recursive pin covers a whole subtree.

use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::Result;



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

    /// Every directly-pinned entry `(uid, path, recursive)`, ordered by uid.
    pub fn pin_list(&self) -> Result<Vec<(String, String, bool)>> {
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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

}
