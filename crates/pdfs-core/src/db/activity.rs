//! The activity feed: a bounded, newest-first log of what the daemon did.

use rusqlite::params;

use super::Db;
use crate::Result;
use crate::control::{ActivityEntry, ActivityKind};

/// How many activity rows to keep. Older rows are pruned on insert, so the feed
/// stays a bounded "recent history" rather than growing without limit.
pub(super) const ACTIVITY_KEEP: i64 = 2000;

impl Db {
    pub fn activity_add(&self, entry: &ActivityEntry) -> Result<()> {
        let kind = serde_json::to_string(&entry.kind)?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO activity (time, kind, target, detail, ok) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![entry.time, kind, entry.target, entry.detail, entry.ok],
        )?;
        tx.execute(
            "DELETE FROM activity WHERE id <= (
               SELECT id FROM activity ORDER BY id DESC LIMIT 1 OFFSET ?1
             )",
            params![ACTIVITY_KEEP],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Whether the kept log already holds an entry of `kind` for `target`. Lets
    /// the daemon report something once across restarts rather than once per
    /// run, without keeping a second table in step with the log.
    pub fn activity_has(&self, kind: ActivityKind, target: &str) -> Result<bool> {
        let kind = serde_json::to_string(&kind)?;
        let conn = self.read();
        let mut stmt =
            conn.prepare("SELECT 1 FROM activity WHERE kind = ?1 AND target = ?2 LIMIT 1")?;
        Ok(stmt.exists(params![kind, target])?)
    }

    /// The most recent activity, newest first, capped at `limit` entries. Rows
    /// whose stored `kind` no longer parses (written by an older build) are
    /// skipped rather than failing the whole read.
    pub fn activity_list(&self, limit: usize) -> Result<Vec<ActivityEntry>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT time, kind, target, detail, ok FROM activity
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (time, kind, target, detail, ok) = row?;
            let Ok(kind) = serde_json::from_str::<ActivityKind>(&kind) else {
                continue;
            };
            out.push(ActivityEntry {
                time,
                kind,
                target,
                detail,
                ok,
            });
        }
        Ok(out)
    }
}
