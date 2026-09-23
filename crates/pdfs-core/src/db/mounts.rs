//! Unified mount/location presentation rows.
//!
//! A device row stores only its `sync_folder` identity. Its path, remote root,
//! mode, state, pending mode, and last-sync timestamp are selected from
//! `sync_folder`, which remains the authoritative sync-engine table.

use rusqlite::params;

use super::Db;
use crate::mounts::{MountAccess, MountKind, MountMode, MountSpec};
use crate::{Error, Result};

impl Db {
    /// Project the configured primary mount into the presentation table.
    ///
    /// `root_share_id` is optional because an offline startup may only have the
    /// cached root node. A previously learned share id is retained only when it
    /// belongs to the same root uid.
    pub fn mount_upsert_my_files(
        &self,
        local_path: &str,
        root_uid: &str,
        root_share_id: Option<&str>,
    ) -> Result<i64> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE mount
                SET local_path = ?1,
                    root_share_id = CASE
                        WHEN root_uid = ?2
                          THEN COALESCE(NULLIF(?3, ''), root_share_id)
                        ELSE COALESCE(?3, '')
                    END,
                    root_uid = ?2,
                    mode = 'ondemand',
                    access = 'rw'
              WHERE kind = 'myfiles'",
            params![local_path, root_uid, root_share_id],
        )?;
        let id = if updated == 0 {
            tx.execute(
                "INSERT INTO mount
                    (kind, local_path, root_uid, root_share_id, mode, access)
                 VALUES ('myfiles', ?1, ?2, COALESCE(?3, ''), 'ondemand', 'rw')",
                params![local_path, root_uid, root_share_id],
            )?;
            tx.last_insert_rowid()
        } else {
            tx.query_row("SELECT id FROM mount WHERE kind = 'myfiles'", [], |row| {
                row.get(0)
            })?
        };
        tx.commit()?;
        Ok(id)
    }

    /// Store a share id learned asynchronously, but only if the projection still
    /// names the root it was resolved for.
    pub fn mount_repair_my_files_share_id(
        &self,
        root_uid: &str,
        root_share_id: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "UPDATE mount
                SET root_share_id = ?2
              WHERE kind = 'myfiles'
                AND root_uid = ?1",
            params![root_uid, root_share_id],
        )? > 0)
    }

    /// Every local location, with device fields joined from `sync_folder`.
    pub fn mount_list(&self) -> Result<Vec<MountSpec>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT
                 m.id,
                 m.kind,
                 m.sync_folder_id,
                 m.share_root_uid,
                 CASE WHEN m.kind = 'device' THEN sf.local_path ELSE m.local_path END,
                 CASE WHEN m.kind = 'device' THEN sf.remote_uid ELSE m.root_uid END,
                 CASE WHEN m.kind = 'device' THEN sf.remote_share_id ELSE m.root_share_id END,
                 CASE WHEN m.kind = 'device' THEN sf.mode ELSE m.mode END,
                 m.access,
                 CASE WHEN m.kind = 'device' THEN sf.state ELSE 'idle' END,
                 CASE WHEN m.kind = 'device' THEN sf.last_sync ELSE 0 END,
                 CASE WHEN m.kind = 'device' THEN sf.pending_mode ELSE NULL END
               FROM mount AS m
               LEFT JOIN sync_folder AS sf ON sf.id = m.sync_folder_id
              ORDER BY CASE m.kind
                         WHEN 'myfiles' THEN 0
                         WHEN 'device' THEN 1
                         ELSE 2
                       END,
                       m.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })?;

        let mut mounts = Vec::new();
        for row in rows {
            let (
                id,
                kind,
                sync_folder_id,
                share_root_uid,
                local_path,
                root_uid,
                root_share_id,
                mode,
                access,
                state,
                last_sync,
                pending_mode,
            ) = row?;
            let kind = match kind.as_str() {
                "myfiles" => MountKind::MyFiles,
                "device" => MountKind::Device {
                    sync_folder_id: sync_folder_id.ok_or_else(|| {
                        Error::Other(format!("mount row {id} has no sync_folder_id"))
                    })?,
                },
                "shared" => MountKind::Shared {
                    share_root_uid: share_root_uid.ok_or_else(|| {
                        Error::Other(format!("mount row {id} has no share_root_uid"))
                    })?,
                },
                other => {
                    return Err(Error::Other(format!(
                        "mount row {id} has unknown kind {other:?}"
                    )));
                }
            };
            let access = match access.as_str() {
                "rw" => MountAccess::Rw,
                "ro" => MountAccess::Ro,
                other => {
                    return Err(Error::Other(format!(
                        "mount row {id} has unknown access {other:?}"
                    )));
                }
            };
            mounts.push(MountSpec {
                id,
                kind,
                local_path,
                root_uid,
                root_share_id,
                mode: MountMode::from(mode.as_str()),
                access,
                state,
                last_sync,
                pending_mode: pending_mode.as_deref().map(MountMode::from),
                mounted: false,
                progress: None,
                paused: false,
            });
        }
        Ok(mounts)
    }
}
