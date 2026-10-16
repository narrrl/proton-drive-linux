//! The local-file index: a generation-stamped scan of the mount, so search can
//! answer over files the daemon has on disk as well as the remote tree.

use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::Result;
use crate::localindex::LocalEntry;

use super::nodes::candidate_trigrams;
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

    /// Write one batch of walked entries under scan generation `generation`,
    /// keeping the FTS index in step with them.
    ///
    /// The index is maintained incrementally rather than rebuilt at the end of
    /// the scan, because a rebuild reads and re-tokenises every row in the table
    /// — trigrams over a few hundred thousand paths — inside one transaction on
    /// the single shared connection, which stalls the whole mount for as long as
    /// it takes. Incremental maintenance is possible here for a reason specific
    /// to this table: `local_fts` covers `name` and `path`, `path` is the
    /// table's key, and `name` is derived from it, so an existing row's indexed
    /// content can never change. Only insertions and deletions touch the index;
    /// a re-scan that finds the same files does no index work at all.
    ///
    /// Hence the update-then-insert shape below rather than an upsert: the
    /// distinction between "already indexed" and "new row" is exactly what
    /// decides whether the index needs a write.
    pub fn local_upsert_batch(&self, generation: i64, entries: &[LocalEntry]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut update = tx.prepare_cached(
                "UPDATE local_files
                    SET name = ?2, is_dir = ?3, size = ?4, mtime = ?5, scan_gen = ?6
                  WHERE path = ?1",
            )?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO local_files (path, name, is_dir, size, mtime, scan_gen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut index =
                tx.prepare_cached("INSERT INTO local_fts (rowid, name, path) VALUES (?1, ?2, ?3)")?;
            for e in entries {
                let row = params![e.path, e.name, e.is_dir as i64, e.size, e.mtime, generation];
                if update.execute(row)? == 0 {
                    insert.execute(row)?;
                    index.execute(params![tx.last_insert_rowid(), e.name, e.path])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Close scan generation `generation`: drop every row an older scan wrote
    /// (those paths are gone from disk), drop their FTS entries, and stamp the
    /// completion time. Returns the number of indexed entries.
    ///
    /// The FTS deletes name the old values explicitly, which is how an
    /// external-content FTS5 table is told to retract a row — see
    /// [`local_upsert_batch`](Self::local_upsert_batch) for why this is done
    /// per-row instead of by rebuilding the index.
    pub fn local_finish_scan(&self, generation: i64, finished_at: i64) -> Result<i64> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO local_fts (local_fts, rowid, name, path)
             SELECT 'delete', id, name, path FROM local_files WHERE scan_gen != ?1",
            params![generation],
        )?;
        tx.execute(
            "DELETE FROM local_files WHERE scan_gen != ?1",
            params![generation],
        )?;
        tx.execute(
            "INSERT INTO sync_state (key, value) VALUES ('local_indexed_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [finished_at.to_string()],
        )?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM local_files", [], |r| r.get(0))?;
        tx.commit()?;
        drop(conn);
        self.compact_search_indexes_if_due(finished_at)?;
        Ok(count)
    }

    /// Close a scan that did not see everything: it stopped at the entry cap,
    /// or a batch failed to write. Nothing is pruned, since a path it missed is
    /// not a path that is gone; the next complete scan prunes. Returns the
    /// number of indexed entries.
    ///
    /// Pruning after such a scan dropped every entry it had not reached, and
    /// the next scan put them back. That churn is most of what grew
    /// `local_fts` to 16 times its size (`docs/BUGS.md` B217).
    pub fn local_finish_partial_scan(&self, finished_at: i64) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_state (key, value) VALUES ('local_indexed_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [finished_at.to_string()],
        )?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM local_files", [], |r| r.get(0))?;
        Ok(count)
    }

    /// Compact both full-text indexes, at most once a day.
    ///
    /// FTS5 keeps a deleted entry's data until the segments holding it are
    /// merged, and with the few large segments these indexes settle into that
    /// is close to never. Every file the local scan sees disappear or appear
    /// leaves some behind, and every node row the tree replaces does too: on
    /// one machine `local_fts` held 317 MiB for 40,000 paths, against 20 MiB
    /// rebuilt, and a search took four times as long (`docs/BUGS.md` B217).
    ///
    /// `local_fts` is rebuilt from `local_files`, which also drops anything
    /// left over, and `nodes_fts`, which has no content table to rebuild from,
    /// is merged into one segment. Each runs in its own transaction on the
    /// write connection, about half a second on an index that size; once a
    /// day keeps the leftovers small without paying that at every scan.
    pub fn compact_search_indexes_if_due(&self, now: i64) -> Result<()> {
        const EVERY_SECS: i64 = 24 * 60 * 60;
        let last: i64 = self
            .read()
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'fts_compacted_at'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if now - last < EVERY_SECS {
            return Ok(());
        }
        let started = std::time::Instant::now();
        {
            let conn = self.conn.lock();
            conn.execute("INSERT INTO local_fts(local_fts) VALUES('rebuild')", [])?;
        }
        {
            let conn = self.conn.lock();
            conn.execute("INSERT INTO nodes_fts(nodes_fts) VALUES('optimize')", [])?;
            conn.execute(
                "INSERT INTO sync_state (key, value) VALUES ('fts_compacted_at', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [now.to_string()],
            )?;
        }
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "compacted the search indexes"
        );
        Ok(())
    }

    /// When the last local scan completed (epoch seconds), or `None` if the index
    /// has never been built. The daemon uses this to decide whether a fresh mount
    /// needs an immediate rescan or can serve the existing index.
    pub fn local_indexed_at(&self) -> Result<Option<i64>> {
        let conn = self.read();
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
        let conn = self.read();
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

    /// Bounded candidates for the fuzzy scorer, including matches in the full
    /// local path. ORed trigrams admit misspellings while the FTS index prevents
    /// this from becoming an unbounded filesystem-table scan.
    pub fn search_local_candidates(&self, query: &str, limit: usize) -> Result<Vec<LocalFileHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.read();
        let terms = candidate_trigrams(query);
        let lane_limit = limit.div_ceil(2);
        let mut rows: Vec<LocalFileHit> = if terms.is_empty() {
            let pat = format!("%{}%", like_escape(query));
            let mut stmt = conn.prepare(
                "SELECT path, name, is_dir, size, mtime FROM local_files
                 WHERE name LIKE ?1 ESCAPE '\\' OR path LIKE ?1 ESCAPE '\\'
                 ORDER BY mtime DESC LIMIT ?2",
            )?;
            stmt.query_map(params![pat, limit as i64], local_hit)?
                .collect::<std::result::Result<_, _>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT f.path, f.name, f.is_dir, f.size, f.mtime
                   FROM local_fts x JOIN local_files f ON f.id = x.rowid
                  WHERE local_fts MATCH ?1
                  ORDER BY x.rank LIMIT ?2",
            )?;
            stmt.query_map(params![terms.join(" OR "), lane_limit as i64], local_hit)?
                .collect::<std::result::Result<_, _>>()?
        };
        if !terms.is_empty()
            && let Some(first) = query.chars().next()
        {
            // Trigram rank says nothing about how well a name *starts* with the
            // query, so on a home directory full of dependency trees the pool
            // fills with vendored files before the obvious answer is reached
            // (measured: 73% of the 500 best-ranked candidates for "test" came
            // from one Go module cache). Give whole-query prefix matches their
            // own lane, newest first, and keep the single-character lane —
            // which recovers a typo that destroyed every trigram — behind it.
            let mut lanes = vec![(
                format!("{}%", like_escape(query)),
                "ORDER BY mtime DESC LIMIT ?2",
            )];
            lanes.push((
                format!("{}%", like_escape(&first.to_string())),
                "ORDER BY name COLLATE NOCASE LIMIT ?2",
            ));
            let mut extra = Vec::new();
            for (pattern, order) in lanes {
                let mut stmt = conn.prepare(&format!(
                    "SELECT path, name, is_dir, size, mtime FROM local_files
                     WHERE name COLLATE NOCASE LIKE ?1 ESCAPE '\\' {order}"
                ))?;
                extra.extend(
                    stmt.query_map(params![pattern, lane_limit as i64], local_hit)?
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                );
            }
            // Prefix hits go in front: `rows` is truncated to `limit` below, and
            // the lane exists precisely because trigram rank was burying them.
            let trigram = std::mem::take(&mut rows);
            for hit in extra.into_iter().chain(trigram) {
                if !rows.iter().any(|existing| existing.path == hit.path) {
                    rows.push(hit);
                }
            }
            rows.truncate(limit);
        }
        Ok(rows)
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
