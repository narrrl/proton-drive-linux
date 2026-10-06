//! Trashed nodes, persisted so the trash view opens without a round-trip.

use std::collections::HashMap;

use rusqlite::params;

use super::Db;
use crate::Result;

pub struct StoredTrash {
    pub uid: String,
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    pub mtime: i64,
    /// The folder this node was trashed from, when it is known. `None` for a
    /// row written before schema v29, and for a node whose parent the server
    /// did not report.
    pub parent_uid: Option<String>,
    /// When the daemon first saw the node in the trash, epoch seconds. `None`
    /// when it cannot say (see schema v35).
    pub trashed_at: Option<i64>,
}

impl Db {
    pub fn trash_replace(&self, items: &[StoredTrash]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM trash", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO trash (uid, name, is_dir, size, mtime, parent_uid, trashed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for item in items {
                stmt.execute(params![
                    item.uid,
                    item.name,
                    item.is_dir as i64,
                    item.size,
                    item.mtime,
                    item.parent_uid,
                    item.trashed_at
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The persisted trash listing, folders first then by name — the order the
    /// Trash page shows it in.
    pub fn trash_list(&self) -> Result<Vec<StoredTrash>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT uid, name, is_dir, size, mtime, parent_uid, trashed_at FROM trash
             ORDER BY is_dir DESC, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StoredTrash {
                uid: r.get(0)?,
                name: r.get(1)?,
                is_dir: r.get::<_, i64>(2)? != 0,
                size: r.get(3)?,
                mtime: r.get(4)?,
                parent_uid: r.get(5)?,
                trashed_at: r.get(6)?,
            })
        })?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    /// Every trashed node paired with the folder it was trashed from, for the
    /// restore's tree walk. Kept separate from [`Db::trash_list`] because the
    /// restore wants only the links, not the names, sizes or ordering.
    pub fn trash_parents(&self) -> Result<Vec<(String, Option<String>)>> {
        let conn = self.read();
        let mut stmt = conn.prepare("SELECT uid, parent_uid FROM trash")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut pairs = Vec::new();
        for row in rows {
            pairs.push(row?);
        }
        Ok(pairs)
    }

    /// When each node now in the trash was first seen there, keyed by uid, so
    /// a refresh can carry the times over into the listing it writes.
    pub fn trash_seen(&self) -> Result<HashMap<String, Option<i64>>> {
        let conn = self.read();
        let mut stmt = conn.prepare("SELECT uid, trashed_at FROM trash")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut seen = HashMap::new();
        for row in rows {
            let (uid, at) = row?;
            seen.insert(uid, at);
        }
        Ok(seen)
    }

    // ---- device sync -----------------------------------------
}
