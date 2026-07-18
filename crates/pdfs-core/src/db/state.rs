//! The `state` key/value table: small scalars that outlive a mount — the volume
//! event cursor, listing freshness stamps, the local-index generation.

use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::Result;

impl Db {
    pub fn get_event_cursor(&self) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let v = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'event_cursor'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Persist the incremental-sync cursor after a batch of events is applied.
    pub fn set_event_cursor(&self, cursor: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES ('event_cursor', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![cursor],
        )?;
        Ok(())
    }

    /// Read a `sync_state` value as a string.
    pub fn state_str(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let v = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Write a `sync_state` string value.
    pub fn set_state_str(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Queue an op to be performed by the drain worker, returning its row id.
    ///
    /// One *revision* op per node: a second write to the same file before the
    /// first has drained replaces it, since the newer blob already contains
    /// everything the older one did. The superseded blob's path is returned so
    /// the caller can delete it.
    ///
    /// Rows supersede only their own kind, and only the kinds
    /// [`op_supersedes`] names. A create op for the same uid must survive —
    /// dropping it would leave a file that exists nowhere but this machine with
    /// nothing left to create it. (Writes to a node that is itself still queued
    pub fn state_i64(&self, key: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|v| v.parse().ok()))
    }

    /// Write a `sync_state` integer value.
    pub fn set_state_i64(&self, key: &str, value: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    /// Drop a `sync_state` key, so whatever it stamped counts as never fetched.
    pub fn clear_state(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM sync_state WHERE key = ?1", params![key])?;
        Ok(())
    }

}
