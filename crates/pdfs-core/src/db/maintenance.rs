//! Database health reporting and repair (features.md 4.3).
//!
//! Everything here exists for the case where something has already gone wrong:
//! a crash mid-write, a full disk, a cache that has grown out of proportion to
//! what it holds. The daemon never calls these on its own — they are what
//! `pdfs cache inspect`, `pdfs cache vacuum`, and `pdfs diagnose` are built on,
//! so a user with a suspect install can get an answer without being told to
//! delete their state directory and re-sync from scratch.

use rusqlite::OptionalExtension;

use super::Db;
use crate::Result;

/// A point-in-time picture of the database's size and contents.
///
/// Sizes are computed from SQLite's own page accounting rather than by stat'ing
/// the file, so they stay right when the file is sparse or a checkpoint is
/// pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbStats {
    /// Schema version the file is currently at.
    pub schema_version: i64,
    /// Total pages allocated to the database.
    pub page_count: i64,
    /// Pages allocated but unused — what a `VACUUM` would reclaim.
    pub free_pages: i64,
    /// Bytes per page.
    pub page_size: i64,
    /// Row counts for the tables worth reporting, in display order.
    pub tables: Vec<(String, i64)>,
}

impl DbStats {
    /// Bytes the database file accounts for.
    pub fn total_bytes(&self) -> u64 {
        (self.page_count * self.page_size).max(0) as u64
    }

    /// Bytes a `VACUUM` could hand back to the filesystem.
    pub fn reclaimable_bytes(&self) -> u64 {
        (self.free_pages * self.page_size).max(0) as u64
    }
}

/// What a maintenance run reclaimed.
///
/// Deliberately does not carry an orphaned-row count: the content cache already
/// rebuilds its index from what is actually on disk every time it opens (see
/// [`crate::cache::ContentCache`]'s `reconcile`), so a second pruning pass here
/// would be a duplicate of that logic with a worse view of the truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VacuumOutcome {
    /// Database bytes before the run.
    pub before_bytes: u64,
    /// Database bytes after it.
    pub after_bytes: u64,
    /// WAL frames written back into the database by the checkpoint.
    pub wal_frames_checkpointed: i64,
}

impl VacuumOutcome {
    /// Bytes handed back to the filesystem. Saturating: a database can legitimately
    /// grow across a vacuum (a checkpoint folds WAL frames in), and reporting that
    /// as a huge positive number via wraparound would be worse than reporting zero.
    pub fn freed_bytes(&self) -> u64 {
        self.before_bytes.saturating_sub(self.after_bytes)
    }
}

/// The tables `inspect` reports on, in the order a reader wants them.
const REPORTED_TABLES: &[&str] = &[
    "nodes",
    "cache_entries",
    "pins",
    "photos",
    "trash",
    "device",
    "sync_folder",
    "sync_entry",
    "pending_op",
    "activity",
    "local_files",
];

impl Db {
    /// SQLite's own consistency check.
    ///
    /// Returns an empty vector when the database is sound; otherwise each entry
    /// is one problem SQLite found. Read-only and safe against a live daemon,
    /// though it reads every page, so it is not free on a large file.
    pub fn integrity_check(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("PRAGMA integrity_check")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut problems = Vec::new();
        for row in rows {
            let row = row?;
            // A sound database reports the single line "ok"; anything else is a
            // finding.
            if row != "ok" {
                problems.push(row);
            }
        }
        Ok(problems)
    }

    /// Size and row-count snapshot for `pdfs cache inspect`.
    pub fn stats(&self) -> Result<DbStats> {
        let conn = self.conn.lock();
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let free_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        // The schema version lives in the `sync_state` key/value table, not in
        // `PRAGMA user_version` — see `migrations`.
        let schema_version: i64 = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut tables = Vec::new();
        for name in REPORTED_TABLES {
            // A table listed here may not exist yet on an older schema, and a
            // report is not worth failing over a missing one.
            let exists: Option<String> = conn
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
                    [name],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                continue;
            }
            let count: i64 =
                conn.query_row(&format!("SELECT count(*) FROM \"{name}\""), [], |r| {
                    r.get(0)
                })?;
            tables.push(((*name).to_string(), count));
        }

        Ok(DbStats {
            schema_version,
            page_count,
            free_pages,
            page_size,
            tables,
        })
    }

    /// Fold the WAL back into the database and compact the file.
    ///
    /// `VACUUM` rewrites the whole database into a fresh file, so it needs room
    /// for a second copy and takes a write lock for the duration. That makes it
    /// a deliberate, user-invoked operation rather than something the daemon
    /// does on a timer.
    ///
    /// The checkpoint runs *after* the vacuum. Checkpointing only beforehand
    /// folds a log that the vacuum is about to rewrite anyway, and leaves the
    /// rewrite itself sitting in the WAL: measured on a live install, a 170 MiB
    /// database vacuumed to 136 MiB while its WAL grew 67 MiB → 143 MiB,
    /// erasing the reclaim in net disk terms and reporting `0 frames`
    /// checkpointed because it ran when there was nothing to fold.
    ///
    /// Ordering it afterwards is strictly better but is *not* a proven fix for
    /// that inflation. The live case inflates because the daemon's other
    /// connections hold read transactions, and a `TRUNCATE` checkpoint waits on
    /// readers — so under a busy daemon this can still return having moved
    /// little. `journal_size_limit` remains the backstop that caps the file.
    /// Confirming the fix needs a measurement against a live daemon, not a
    /// single-connection test, which folds the WAL on its own either way.
    pub fn vacuum(&self) -> Result<VacuumOutcome> {
        let before = self.stats()?.total_bytes();

        {
            let conn = self.conn.lock();
            conn.execute_batch("VACUUM")?;
        }

        let wal_frames_checkpointed = self.checkpoint_wal()?;

        let after = self.stats()?.total_bytes();
        Ok(VacuumOutcome {
            before_bytes: before,
            after_bytes: after,
            wal_frames_checkpointed,
        })
    }

    /// Checkpoint the WAL and truncate it, returning the number of frames
    /// written back into the database.
    ///
    /// `TRUNCATE` blocks on readers rather than giving up part-way, which is
    /// what makes the result meaningful: a `PASSIVE` checkpoint against a busy
    /// daemon can legitimately move nothing and report success.
    pub fn checkpoint_wal(&self) -> Result<i64> {
        let conn = self.conn.lock();
        // Columns are (busy, log frames, frames checkpointed).
        let frames: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(2))?;
        Ok(frames.max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_database_passes_its_integrity_check() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.integrity_check().unwrap().is_empty());
    }

    #[test]
    fn stats_report_the_schema_and_the_known_tables() {
        let db = Db::open_in_memory().unwrap();
        let stats = db.stats().unwrap();

        assert!(stats.schema_version > 0, "migrations set a user_version");
        assert!(stats.page_size > 0);
        assert!(
            stats.tables.iter().any(|(name, _)| name == "nodes"),
            "the nodes table should be reported; got {:?}",
            stats.tables
        );
        // A fresh database has no rows in any of them.
        assert!(stats.tables.iter().all(|(_, count)| *count == 0));
    }

    #[test]
    fn total_and_reclaimable_bytes_derive_from_the_page_counts() {
        let stats = DbStats {
            schema_version: 1,
            page_count: 10,
            free_pages: 4,
            page_size: 4096,
            tables: Vec::new(),
        };
        assert_eq!(stats.total_bytes(), 40960);
        assert_eq!(stats.reclaimable_bytes(), 16384);
    }

    /// A vacuum that leaves the file larger (a checkpoint folded WAL frames in)
    /// must report nothing freed, not an underflowed enormous number.
    #[test]
    fn freed_bytes_does_not_underflow_when_the_file_grew() {
        let outcome = VacuumOutcome {
            before_bytes: 100,
            after_bytes: 400,
            ..VacuumOutcome::default()
        };
        assert_eq!(outcome.freed_bytes(), 0);
    }

    #[test]
    fn freed_bytes_reports_the_difference_when_the_file_shrank() {
        let outcome = VacuumOutcome {
            before_bytes: 4096,
            after_bytes: 1024,
            ..VacuumOutcome::default()
        };
        assert_eq!(outcome.freed_bytes(), 3072);
    }

    #[test]
    fn vacuuming_a_fresh_database_leaves_it_sound() {
        let db = Db::open_in_memory().unwrap();
        db.vacuum().unwrap();
        assert!(db.integrity_check().unwrap().is_empty());
    }

    /// A unique temp directory removed on drop; avoids a dev-dependency, as in
    /// [`crate::cache`]'s tests. Needed here because an in-memory database has
    /// no WAL file to measure.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-maint-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A file-backed vacuum reclaims space and leaves the file sound.
    ///
    /// Note what this does *not* pin: the WAL-inflation behaviour that motivated
    /// the checkpoint ordering in [`Db::vacuum`]. Measured here, both orderings
    /// report `frames=0` and leave a 0-byte WAL, because a single-connection
    /// test lets SQLite fold the log itself. The live case inflated only because
    /// the daemon holds concurrent readers that block a checkpoint, and
    /// reproducing that needs a second connection parked in a read transaction.
    /// An assertion on frame count or WAL size here would pass under both
    /// orderings and so would be decoration — see the note in `vacuum`.
    #[test]
    fn a_file_backed_vacuum_reclaims_space_and_stays_sound() {
        let dir = TempDir::new();
        let path = dir.0.join("cache.db");
        let db = Db::open(&path).unwrap();

        for i in 0..2000 {
            db.set_state_str(&format!("k{i}"), &"v".repeat(256))
                .unwrap();
        }
        let full = db.stats().unwrap().total_bytes();
        for i in 0..2000 {
            db.set_state_str(&format!("k{i}"), "").unwrap();
        }

        let outcome = db.vacuum().unwrap();

        assert!(
            outcome.after_bytes < full,
            "vacuum did not reclaim: {} → {} (peak {full})",
            outcome.before_bytes,
            outcome.after_bytes
        );
        assert!(db.integrity_check().unwrap().is_empty());
    }
}
