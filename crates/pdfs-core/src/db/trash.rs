//! Trashed nodes, persisted so the trash view opens without a round-trip.

use rusqlite::params;

use super::Db;
use crate::Result;

pub struct StoredTrash {
    pub uid: String,
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    pub mtime: i64,
}

impl Db {
    pub fn trash_replace(&self, items: &[StoredTrash]) -> Result<()> {
        let mut conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
}
