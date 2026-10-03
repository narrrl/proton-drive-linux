//! Persisted effective access for roots shared with this account.

use std::collections::HashMap;

use proton_drive_rs::proton_sdk::ids::{LinkId, NodeUid, VolumeId};
use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::{Access, Error, Result};

impl Db {
    /// Load the complete, deliberately small shared-root authority map.
    ///
    /// Mount state keeps this resident so ordinary node interning and subtree
    /// propagation never turn into one SQLite query per inode. The database
    /// remains the restart/offline authority and every explicit role change is
    /// written through before this cache is updated.
    pub fn all_share_access(&self) -> Result<HashMap<NodeUid, Access>> {
        let conn = self.read();
        let mut stmt = conn.prepare("SELECT root_uid, access FROM share_access")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut access = HashMap::new();
        for row in rows {
            let (uid, value) = row?;
            let (volume, link) = uid.split_once('~').ok_or_else(|| {
                Error::Other(format!(
                    "invalid shared-root uid {uid:?} stored in database"
                ))
            })?;
            let access_value = Access::from_db_str(&value).ok_or_else(|| {
                Error::Other(format!("invalid share access {value:?} stored for {uid}"))
            })?;
            access.insert(
                NodeUid::new(VolumeId::from(volume), LinkId::from(link)),
                access_value,
            );
        }
        Ok(access)
    }

    /// Read the effective access recorded for a shared-tree root.
    pub fn share_access(&self, uid: &NodeUid) -> Result<Option<Access>> {
        let conn = self.read();
        let value = conn
            .query_row(
                "SELECT access FROM share_access WHERE root_uid = ?1",
                [uid.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        value
            .map(|value| {
                Access::from_db_str(&value).ok_or_else(|| {
                    Error::Other(format!("invalid share access {value:?} stored for {uid}"))
                })
            })
            .transpose()
    }

    /// Insert or replace the effective access for a shared-tree root.
    pub fn set_share_access(&self, uid: &NodeUid, access: Access) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO share_access (root_uid, access) VALUES (?1, ?2)
             ON CONFLICT(root_uid) DO UPDATE SET access = excluded.access",
            params![uid.to_string(), access.as_db_str()],
        )?;
        Ok(())
    }

    /// Forget a shared-tree root's effective access.
    pub fn delete_share_access(&self, uid: &NodeUid) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM share_access WHERE root_uid = ?1",
            [uid.to_string()],
        )?;
        Ok(())
    }

    /// Fail closed every persisted shared-tree authority.
    ///
    /// Owned and device trees have no row in `share_access`, so this only
    /// affects roots already identified as shared with this account.
    pub fn downgrade_all_share_access(&self) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "UPDATE share_access SET access = 'viewer' WHERE access != 'viewer'",
            [],
        )?)
    }

    /// Resolve a persisted node's effective access through its nearest recorded
    /// share root. `None` means the node itself is not persisted, which callers
    /// must not treat as owned: stale handles need to fail closed.
    pub fn effective_node_access(&self, uid: &NodeUid) -> Result<Option<Access>> {
        let conn = self.read();
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE uid = ?1 AND node_json IS NOT NULL)",
            [uid.to_string()],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let value = conn
            .query_row(
                "WITH RECURSIVE ancestors(uid, parent_uid, depth, path) AS (
                   SELECT uid, parent_uid, 0, char(31) || uid || char(31)
                     FROM nodes WHERE uid = ?1
                   UNION ALL
                   SELECT a.parent_uid, n.parent_uid, a.depth + 1,
                          a.path || a.parent_uid || char(31)
                     FROM ancestors a
                     LEFT JOIN nodes n ON n.uid = a.parent_uid
                    WHERE a.parent_uid IS NOT NULL
                      AND instr(
                            a.path,
                            char(31) || a.parent_uid || char(31)
                          ) = 0
                 )
                 SELECT sa.access
                   FROM ancestors a JOIN share_access sa ON sa.root_uid = a.uid
                  ORDER BY a.depth ASC
                  LIMIT 1",
                [uid.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        value
            .map(|value| {
                Access::from_db_str(&value).ok_or_else(|| {
                    Error::Other(format!(
                        "invalid effective share access {value:?} for {uid}"
                    ))
                })
            })
            .transpose()
            .map(|access| Some(access.unwrap_or(Access::Owner)))
    }
}
