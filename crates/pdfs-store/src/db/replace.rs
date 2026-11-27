//! The journal of replacing renames.
//!
//! Drive has no rename that replaces its destination, so a `rename(2)` over an
//! existing name removes the node there first and renames the source second.
//! A stop between the two leaves the destination in Drive's trash and the
//! source under its old name: neither the old state nor the new one. An entry
//! here is written before the first step and removed after the last, so the
//! next start finds every replace that was cut off and settles it
//! (`docs/BUGS.md` B302).
//!
//! Kept in `sync_state` under [`REPLACE_PREFIX`], so the schema does not
//! change.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::Db;
use pdfs_core::Result;

/// The `sync_state` key prefix of a journal entry. The rest of the key is the
/// source's uid.
const REPLACE_PREFIX: &str = "replace:";

/// A replacing rename that has started: `source` is to be renamed to
/// `new_name` in `new_parent`, over `victim`. Uids are in their `Display`
/// form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceIntent {
    pub source_uid: String,
    pub victim_uid: String,
    pub new_parent_uid: String,
    pub new_name: String,
    /// When the rename started, in milliseconds since the epoch.
    pub started_ms: i64,
    /// The opening of the database the rename started under. Set by
    /// [`Db::begin_replace`].
    #[serde(default)]
    pub run: u64,
}

impl ReplaceIntent {
    fn run_is(&self, run: u64) -> bool {
        self.run == run
    }
}

impl Db {
    /// Record that a replacing rename is about to remove its victim. Committed
    /// with a synced WAL before it returns, as the queue's own rows are: the
    /// removal that follows may reach Drive at once.
    pub fn begin_replace(&self, intent: &ReplaceIntent) -> Result<()> {
        let intent = ReplaceIntent {
            run: self.run,
            ..intent.clone()
        };
        let value = serde_json::to_string(&intent)?;
        let conn = self.durable()?;
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![format!("{REPLACE_PREFIX}{}", intent.source_uid), value],
        )?;
        Ok(())
    }

    /// Forget the replacing rename of `source_uid`: it finished, failed and
    /// was undone, or was settled after a restart.
    pub fn end_replace(&self, source_uid: &str) -> Result<()> {
        self.conn.lock().execute(
            "DELETE FROM sync_state WHERE key = ?1",
            params![format!("{REPLACE_PREFIX}{source_uid}")],
        )?;
        Ok(())
    }

    /// Every replacing rename an earlier run started and did not end. This
    /// run's own are in progress, and are not returned. An entry that cannot
    /// be read is dropped, with a warning: it cannot be settled.
    pub fn open_replaces(&self) -> Result<Vec<ReplaceIntent>> {
        let rows: Vec<(String, String)> = {
            let conn = self.read()?;
            let mut stmt = conn.prepare(
                "SELECT key, value FROM sync_state WHERE key >= ?1 AND key < ?2 ORDER BY key",
            )?;
            stmt.query_map(params![REPLACE_PREFIX, "replace;"], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            match serde_json::from_str::<ReplaceIntent>(&value) {
                Ok(intent) if intent.run_is(self.run) => {}
                Ok(intent) => out.push(intent),
                Err(error) => {
                    tracing::warn!(key, %error, "dropping an unreadable replace intent");
                    self.conn
                        .lock()
                        .execute("DELETE FROM sync_state WHERE key = ?1", params![key])?;
                }
            }
        }
        Ok(out)
    }
}
