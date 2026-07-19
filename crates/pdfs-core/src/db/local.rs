//! The local-file index: a generation-stamped scan of the mount, so search can
//! answer over files the daemon has on disk as well as the remote tree.

use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::Result;
use crate::localindex::LocalEntry;

use super::utils::{TRIGRAM_MIN, like_escape};

/// One hit from [`Db::search_local`]: an indexed file on the machine itself, not
/// in Drive. `path` is absolute.
pub struct LocalFileHit {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    pub mtime: i64,
}

impl Db {
    pub fn local_begin_scan(&self) -> Result<i64> {
        let conn = self.conn.lock();
        let current: i64 = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'local_scan_gen'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES ('local_scan_gen', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [next.to_string()],
        )?;
        Ok(next)
    }

    /// Write one batch of walked entries under scan generation `generation`. The
    /// FTS index is *not* touched here — it is rebuilt once in
    /// [`local_finish_scan`](Self::local_finish_scan).
    pub fn local_upsert_batch(&self, generation: i64, entries: &[LocalEntry]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO local_files (path, name, is_dir, size, mtime, scan_gen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path) DO UPDATE SET
                   name     = excluded.name,
                   is_dir   = excluded.is_dir,
                   size     = excluded.size,
                   mtime    = excluded.mtime,
                   scan_gen = excluded.scan_gen",
            )?;
            for e in entries {
                stmt.execute(params![
                    e.path,
                    e.name,
                    e.is_dir as i64,
                    e.size,
                    e.mtime,
                    generation
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Close scan generation `generation`: drop every row an older scan wrote
    /// (those paths are gone from disk), rebuild the FTS index over what remains,
    /// and stamp the completion time. Returns the number of indexed entries.
    pub fn local_finish_scan(&self, generation: i64, finished_at: i64) -> Result<i64> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM local_files WHERE scan_gen != ?1",
            params![generation],
        )?;
        tx.execute_batch("INSERT INTO local_fts(local_fts) VALUES('rebuild');")?;
        tx.execute(
            "INSERT INTO sync_state (key, value) VALUES ('local_indexed_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [finished_at.to_string()],
        )?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM local_files", [], |r| r.get(0))?;
        tx.commit()?;
        Ok(count)
    }

    /// When the last local scan completed (epoch seconds), or `None` if the index
    /// has never been built. The daemon uses this to decide whether a fresh mount
    /// needs an immediate rescan or can serve the existing index.
    pub fn local_indexed_at(&self) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'local_indexed_at'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok()))
    }

    /// Substring search over indexed local file names, newest-modified first
    /// within a relevance tier. Mirrors [`search`](Self::search): the trigram
    /// index handles queries of at least [`TRIGRAM_MIN`] chars, shorter ones fall
    /// back to a `LIKE` scan.
    pub fn search_local(&self, query: &str, limit: usize) -> Result<Vec<LocalFileHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let mut stmt;
        let rows = if query.chars().count() < TRIGRAM_MIN {
            let pat = format!("%{}%", like_escape(query));
            stmt = conn.prepare(
                "SELECT path, name, is_dir, size, mtime FROM local_files
                 WHERE name LIKE ?1 ESCAPE '\\'
                 ORDER BY mtime DESC LIMIT ?2",
            )?;
            stmt.query_map(params![pat, limit as i64], local_hit)?
        } else {
            let phrase = query
                .split_whitespace()
                .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            stmt = conn.prepare(
                "SELECT f.path, f.name, f.is_dir, f.size, f.mtime
                 FROM local_fts x JOIN local_files f ON f.id = x.rowid
                 WHERE x.name MATCH ?1
                 ORDER BY x.rank LIMIT ?2",
            )?;
            stmt.query_map(params![phrase, limit as i64], local_hit)?
        };
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

/// Row → [`LocalFileHit`]. Every local-search query selects the same
/// columns in the same order.
pub(super) fn local_hit(row: &rusqlite::Row) -> rusqlite::Result<LocalFileHit> {
    Ok(LocalFileHit {
        path: row.get(0)?,
        name: row.get(1)?,
        is_dir: row.get::<_, i64>(2)? != 0,
        size: row.get(3)?,
        mtime: row.get(4)?,
    })
}
