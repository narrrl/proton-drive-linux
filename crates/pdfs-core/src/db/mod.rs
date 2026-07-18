//! Unified SQLite metadata cache — the single persistence layer behind FUSE
//! inode bookkeeping, full-text search, content-cache LRU tracking, and pins.
//!
//! Only the daemon (`pdfs-fuse`) opens this for writes; the GUI and CLI reach
//! the same data through the control socket. The connection is wrapped in a
//! `Mutex` because the FUSE callbacks are synchronous and already serialize
//! behind the `State` lock, so a connection pool would be overkill.
//!
//! This module is the P0 foundation: it opens the database, enables WAL, and
//! applies the forward-only schema migrations. Write-through of nodes, the
//! event cursor, FTS, and the cache index land in later phases on this schema.
//!
//! The surface is one `Db` type; its methods live in a submodule per table group
//! (`nodes`, `photos`, `pins`, …) as separate `impl Db` blocks. Everything those
//! modules define is re-exported here, so callers keep saying `db::StoredPhoto`
//! and never name a submodule.

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::Result;
use std::path::Path;

mod activity;
mod cache;
mod devices;
mod local;
mod migrations;
mod nodes;
mod ops;
mod photos;
mod pins;
mod state;
mod sync;
mod trash;
mod utils;

pub use cache::CacheEntryInput;
pub use devices::StoredDevice;
pub use local::LocalFileHit;
pub use nodes::{SearchHit, StoredNode};
pub use ops::{
    LOCAL_VOLUME,
    AttachedBlob, OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH, PendingCounts,
    PendingOp, op_supersedes,
};
pub use photos::{StoredPhoto, THUMB_HAVE, THUMB_NONE, THUMB_UNKNOWN};
pub use sync::{StoredSyncEntry, StoredSyncFolder};
pub use trash::StoredTrash;

/// Size the WAL is truncated back to after a checkpoint. Comfortably above the
/// steady-state working set (a few MB), so the truncation only claws back the
/// outliers rather than fighting the normal write path for disk.
const WAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// Handle to the unified metadata database.
///
/// Cheap to wrap in an `Arc`; clone the `Arc`, not this. All access goes through
/// the inner `Mutex<Connection>`.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if absent) the database at `path`, enable WAL, and bring
    /// the schema up to [`SCHEMA_VERSION`].
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        // WAL: readers never block the single writer. NORMAL sync is the
        // standard durability/throughput tradeoff for WAL.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Hand the WAL's disk back after a checkpoint. Without a limit SQLite
        // reuses the file in place but never shrinks it, so a single large
        // transaction (a full-index FTS rebuild in `local_finish_scan` is the
        // one that reaches this size) leaves the WAL at its high-water mark
        // forever — a multi-GB file next to a database two orders of magnitude
        // smaller. Checkpointing is unaffected; only the file is truncated back.
        conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory database. For tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Run a closure with the locked connection. Escape hatch for callers that
    /// need a query no typed method covers yet.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock();
        f(&conn)
    }
}

#[cfg(test)]
mod tests;
