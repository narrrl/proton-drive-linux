//! Folder-sync bookkeeping: the folders this device mirrors, and the per-file
//! state each pass reconciles against.

use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::Result;
use std::collections::HashMap;

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

impl Db {
    pub fn sync_folder_add(
        &self,
        local_path: &str,
        remote_uid: &str,
        remote_share_id: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_folder (local_path, remote_uid, remote_share_id)
             VALUES (?1, ?2, ?3)",
            params![local_path, remote_uid, remote_share_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Every synced folder, oldest first.
    pub fn sync_folder_list(&self) -> Result<Vec<StoredSyncFolder>> {
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
        let mut conn = self.conn.lock();
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
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sync_folder SET mode = ?2, pending_mode = NULL WHERE id = ?1",
            params![id, mode],
        )?;
        Ok(())
    }

    /// Queue (or, with `None`, withdraw) a mode the folder should move to once it
    /// is safe to switch.
    pub fn sync_folder_set_pending_mode(&self, id: i64, mode: Option<&str>) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sync_folder SET pending_mode = ?2 WHERE id = ?1",
            params![id, mode],
        )?;
        Ok(())
    }

    /// Update a synced folder's state and stamp `last_sync` to now.
    pub fn sync_folder_set_state(&self, id: i64, state: &str, last_sync: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sync_folder SET state = ?2, last_sync = ?3 WHERE id = ?1",
            params![id, state, last_sync],
        )?;
        Ok(())
    }

    /// The whole per-file sync baseline for a folder, keyed by relative path.
    pub fn sync_entries(&self, folder_id: i64) -> Result<HashMap<String, StoredSyncEntry>> {
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM sync_entry WHERE folder_id = ?1 AND rel_path = ?2",
            params![folder_id, rel_path],
        )?;
        Ok(())
    }

    /// Drop the entire baseline for a folder. Used when flipping ondemand→mirror:
    /// the local tree was evicted, so the old baseline is stale and would make the
    /// next reconcile mistake "locally deleted" for "must re-download". Clearing it
    /// leaves an empty baseline + full remote = pure download (devices.md P3).
    pub fn sync_entries_clear(&self, folder_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM sync_entry WHERE folder_id = ?1",
            params![folder_id],
        )?;
        Ok(())
    }
}
