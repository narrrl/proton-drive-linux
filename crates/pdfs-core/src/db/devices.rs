//! This machine as a registered Proton Drive device.

use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::Result;

pub struct StoredDevice {
    pub uid: String,
    pub share_id: String,
    pub root_uid: String,
    pub name: String,
    pub created: i64,
}

impl Db {
    pub fn device_get(&self) -> Result<Option<StoredDevice>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT uid, share_id, root_uid, name, created FROM device LIMIT 1",
            [],
            |r| {
                Ok(StoredDevice {
                    uid: r.get(0)?,
                    share_id: r.get(1)?,
                    root_uid: r.get(2)?,
                    name: r.get(3)?,
                    created: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Persist (or replace) this machine's device. The table holds a single row.
    pub fn device_set(&self, dev: &StoredDevice) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM device", [])?;
        conn.execute(
            "INSERT INTO device (uid, share_id, root_uid, name, created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![dev.uid, dev.share_id, dev.root_uid, dev.name, dev.created],
        )?;
        Ok(())
    }
}
