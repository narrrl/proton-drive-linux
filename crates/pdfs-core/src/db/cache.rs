//! The content-cache index: one row per cached blob, carrying the access stamp
//! the LRU eviction pass sorts on.

use rusqlite::params;

use super::Db;
use crate::Result;

pub struct CacheEntryInput<'a> {
    /// On-disk filename of the blob or block.
    pub key: &'a str,
    /// `'blob'` or `'block'` — the two byte budgets are counted separately.
    pub kind: &'a str,
    pub size: u64,
    /// Initial LRU key, in unix milliseconds.
    pub last_accessed: i64,
}

impl Db {
    pub fn cache_touch(&self, key: &str, kind: &str, size: u64, now: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO cache_entries (cache_key, kind, size_bytes, last_accessed)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(cache_key) DO UPDATE SET
               kind          = excluded.kind,
               size_bytes    = excluded.size_bytes,
               last_accessed = excluded.last_accessed",
            params![key, kind, size as i64, now],
        )?;
        Ok(())
    }

    /// Bump only `last_accessed` on a cache hit (LRU ordering). A missing row is
    /// a no-op — best effort, mirroring the old best-effort mtime touch.
    pub fn cache_accessed(&self, key: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE cache_entries SET last_accessed = ?2 WHERE cache_key = ?1",
            params![key, now],
        )?;
        Ok(())
    }

    /// Drop a single cache-entry row (one evicted blob or block).
    pub fn cache_remove(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM cache_entries WHERE cache_key = ?1",
            params![key],
        )?;
        Ok(())
    }

    /// Drop the whole-file blob row for `key` plus every block row for the same
    /// uid (`<key>.b<idx>`). Called by `ContentCache::evict`, which removes the
    /// blob and all of a uid's cached blocks at once.
    pub fn cache_remove_all(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM cache_entries WHERE cache_key = ?1 OR cache_key LIKE ?2",
            params![key, format!("{key}.b%")],
        )?;
        Ok(())
    }

    /// Every cache entry of `kind`, ordered least-recently-accessed first, as
    /// `(cache_key, size_bytes)`. The budget enforcer sums the sizes and evicts
    /// from the front until the cache fits.
    pub fn cache_entries_by_kind(&self, kind: &str) -> Result<Vec<(String, u64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT cache_key, size_bytes FROM cache_entries
             WHERE kind = ?1 ORDER BY last_accessed ASC",
        )?;
        let rows = stmt.query_map(params![kind], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Replace the whole cache index with `entries`, as one transaction. The
    /// daemon calls this on open, so a crash or an external file deletion can
    /// never leave a phantom row inflating the budget total.
    ///
    /// The clear and the refill are the same commit deliberately: they are one
    /// operation — "the index now describes what is on disk" — and a crash
    /// between them would otherwise leave an empty index against a full cache
    /// directory, i.e. a budget total of zero and nothing eviction can see.
    ///
    /// One commit also matters for speed. A mount is the only caller and it runs
    /// on the open path, so a cache of thousands of blobs used to pay thousands
    /// of autocommit fsyncs before the mountpoint appeared.
    pub fn cache_rebuild(&self, entries: &[CacheEntryInput<'_>]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM cache_entries", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO cache_entries (cache_key, kind, size_bytes, last_accessed)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(cache_key) DO UPDATE SET
                   kind          = excluded.kind,
                   size_bytes    = excluded.size_bytes,
                   last_accessed = excluded.last_accessed",
            )?;
            for e in entries {
                stmt.execute(params![e.key, e.kind, e.size as i64, e.last_accessed])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // --- Pins (P5) --------------------------------------------------------
    //
    // The pin registry, formerly `pins.json`. One row per directly-pinned node,
    // keyed by uid display string. `recursive` marks a folder pin: its whole
    // subtree counts as pinned, resolved against `nodes` via a CTE so a new
    // descendant is covered the moment it lands in the node cache.

}
