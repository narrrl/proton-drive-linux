//! Unified SQLite metadata cache — the single persistence layer behind FUSE
//! inode bookkeeping, full-text search, content-cache LRU tracking, and pins.
//!
//! Only the daemon (`pdfs-fuse`) opens this for writes; the GUI and CLI reach
//! the same data through the control socket. There is one write connection
//! behind a `Mutex` — SQLite allows exactly one writer, and the single-instance
//! `flock` makes sure it is ours — and a small pool of read-only connections
//! behind [`Db::read`], which every `SELECT`-only method uses.
//!
//! The pool is what makes WAL worth having. The original justification for a
//! single connection was that "FUSE callbacks already serialize behind the
//! `State` lock"; that stopped being true when reads moved onto an 11-thread
//! worker pool, and a lookup then queued behind whatever listing happened to be
//! committing. Under WAL a reader never blocks the writer and vice versa, so the
//! only thing left serialising them was this process's own mutex.
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

use crate::{Error, Result};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

mod activity;
mod albums;
mod cache;
mod devices;
mod local;
mod maintenance;
mod migrations;
mod mounts;
mod nodes;
mod ops;
mod photos;
mod pins;
mod share_access;
mod state;
mod sync;
mod trash;
mod utils;

pub use albums::StoredAlbum;
pub use cache::CacheEntryInput;
pub use devices::StoredDevice;
pub use local::LocalFileHit;
pub use maintenance::{DbStats, VacuumOutcome};
pub use nodes::{PublishedSharedRoot, SearchHit, StoredNode};
pub use ops::{
    AttachedBlob, LOCAL_VOLUME, OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH,
    PARK_EXPIRY_MS, PARK_UNTIL, PendingCounts, PendingOp, RenameMeta, op_supersedes,
};
pub use photos::{StoredPhoto, THUMB_HAVE, THUMB_NONE, THUMB_UNKNOWN, TimelineRow};
pub use pins::PinRow;
pub use sync::{StoredSyncEntry, StoredSyncFolder};
pub use trash::StoredTrash;

/// Size the WAL is truncated back to after a checkpoint. Comfortably above the
/// steady-state working set (a few MB), so the truncation only claws back the
/// outliers rather than fighting the normal write path for disk.
const WAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// How long a statement waits for a database lock before giving up.
///
/// Long enough to outlast any transaction this daemon writes (the largest, a
/// full-index FTS rebuild, is well under a second), short enough that a
/// genuinely stuck holder surfaces as an error rather than an apparent hang.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Page cache, in KiB (applied negated, which is how SQLite reads a size in KiB
/// rather than in pages). 32 MiB against a database that is typically tens of
/// MiB: large enough to hold the working set of a listing-heavy session, small
/// enough to be unremarkable in a desktop daemon's RSS.
const CACHE_SIZE_KIB: i64 = 32 * 1024;

/// Ceiling on the memory-mapped window over the database file. 256 MiB covers
/// any realistic index while keeping the mapping bounded.
const MMAP_SIZE: i64 = 256 * 1024 * 1024;

/// WAL pages between automatic checkpoints (default 1000, i.e. ~4 MiB). Halved
/// so the synchronous checkpoint a committing thread inherits stays short.
const WAL_AUTOCHECKPOINT_PAGES: i64 = 500;

/// Handle to the unified metadata database.
///
/// Cheap to wrap in an `Arc`; clone the `Arc`, not this. All access goes through
/// the inner `Mutex<Connection>`.
pub struct Db {
    conn: Mutex<Connection>,
    /// Read-only connections for `SELECT`-only methods. `None` for an in-memory
    /// database, where a second connection would open a different, empty one —
    /// those fall back to the write connection.
    readers: Option<ReadPool>,
    /// The single-writer lock, held open for as long as this handle lives.
    ///
    /// Never read; dropping the `File` is what releases the `flock`, so it has
    /// to be owned here rather than by `open`'s stack frame. `None` for
    /// in-memory databases, which nothing else can reach.
    _single_writer: Option<File>,
}

/// Idle read-only connections, opened on demand and kept for reuse.
///
/// Not a bounded pool: the number in flight is bounded by the number of threads
/// that can be inside a read at once (the FUSE worker lanes plus the control
/// handlers), and blocking one of those on a permit would reintroduce exactly
/// the queueing this exists to remove. Only the *idle* set is capped, so a burst
/// does not leave a connection per thread parked forever.
struct ReadPool {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
}

/// How many idle read connections to keep. Enough that the steady state never
/// reopens, small enough to be unremarkable.
const MAX_IDLE_READERS: usize = 4;

impl ReadPool {
    fn take(&self) -> Option<Connection> {
        if let Some(conn) = self.idle.lock().pop() {
            return Some(conn);
        }
        match open_read_only(&self.path) {
            Ok(conn) => Some(conn),
            Err(error) => {
                // Falling back to the write connection is correct, just slower,
                // so this is a warning and not a failed read.
                tracing::warn!(%error, "could not open a read-only connection");
                None
            }
        }
    }

    fn put(&self, conn: Connection) {
        let mut idle = self.idle.lock();
        if idle.len() < MAX_IDLE_READERS {
            idle.push(conn);
        }
    }
}

/// A connection to read through: one borrowed from [`ReadPool`], or the write
/// connection when there is no pool (in-memory) or opening one failed.
///
/// Derefs to `Connection`, so a method converts by swapping `self.conn.lock()`
/// for `self.read()` and nothing else.
pub(super) struct Reader<'a> {
    pooled: Option<Connection>,
    writer: Option<parking_lot::MutexGuard<'a, Connection>>,
    pool: Option<&'a ReadPool>,
}

impl std::ops::Deref for Reader<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.pooled
            .as_ref()
            .or(self.writer.as_deref())
            .expect("a Reader always holds one of the two")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if let (Some(conn), Some(pool)) = (self.pooled.take(), self.pool) {
            pool.put(conn);
        }
    }
}

/// Open a second connection to the same file for reads only.
///
/// `SQLITE_OPEN_READ_ONLY` plus `query_only` is belt and braces: the flag stops
/// a routing mistake at the file, the pragma stops it at the statement. The rest
/// mirror [`Db::open_configured`] — a read connection has its own page cache,
/// its own temp store and its own busy timeout, and gets nothing from the
/// writer's.
///
/// `journal_mode` is deliberately absent: it is a property of the database file,
/// which the writer already set to WAL, and a read-only connection cannot change
/// it anyway.
fn open_read_only(path: &Path) -> Result<Connection> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "cache_size", -CACHE_SIZE_KIB)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

impl Db {
    /// Open (creating if absent) the database at `path`, enable WAL, and bring
    /// the schema up to [`SCHEMA_VERSION`].
    ///
    /// Takes the single-instance lock first (see
    /// [`acquire_single_writer_lock`]), and rebuilds from empty if what is on
    /// disk turns out not to be a readable database (see [`is_corrupt`]).
    pub fn open(path: &Path) -> Result<Self> {
        let single_writer = acquire_single_writer_lock(path)?;
        let conn = match Self::open_configured(path) {
            Ok(conn) => conn,
            Err(e) if is_corrupt(&e) => {
                // Every byte in here is derived from the account or from the
                // content cache directory, so the recovery is to start over —
                // loudly, and keeping the damaged file for a post-mortem rather
                // than deleting evidence.
                tracing::error!(
                    db = %path.display(),
                    error = %e,
                    "cache database is corrupt — moving it aside and rebuilding \
                     (metadata will be re-fetched; no user data is stored here)"
                );
                quarantine_corrupt_db(path)?;
                Self::open_configured(path)?
            }
            Err(e) => return Err(e),
        };
        let db = Self {
            conn: Mutex::new(conn),
            // No pool for `:memory:`. A second connection to that name is a
            // second, empty database rather than another view of this one, so
            // every routed read would find a schema that does not exist.
            readers: (!is_in_memory(path)).then(|| ReadPool {
                path: path.to_path_buf(),
                idle: Mutex::new(Vec::new()),
            }),
            _single_writer: single_writer,
        };
        // A drain claim names a worker thread in the process that took it, and
        // the single-writer lock above says that process is not running. Any
        // claim still on disk is therefore a crashed run's, and leaving it would
        // hide those ops from every worker for good.
        match db.clear_op_claims() {
            Ok(0) => {}
            Ok(n) => tracing::info!(ops = n, "released drain claims left by a previous run"),
            Err(e) => tracing::warn!(error = %e, "clearing stale drain claims failed"),
        }
        Ok(db)
    }

    /// A connection for a `SELECT`-only method.
    ///
    /// Reads see the last committed state, which is the same thing they saw
    /// through the write connection: every method here commits before it
    /// returns, and nothing holds a transaction open across calls.
    pub(super) fn read(&self) -> Reader<'_> {
        if let Some(pool) = &self.readers
            && let Some(conn) = pool.take()
        {
            return Reader {
                pooled: Some(conn),
                writer: None,
                pool: Some(pool),
            };
        }
        Reader {
            pooled: None,
            writer: Some(self.conn.lock()),
            pool: None,
        }
    }

    /// Open the file, apply every PRAGMA, and migrate. Split out of
    /// [`open`](Self::open) so the corruption path can retry it verbatim.
    fn open_configured(path: &Path) -> Result<Connection> {
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
        // Wait for a lock rather than failing instantly on SQLITE_BUSY.
        //
        // This is deliberately explicit and deliberately redundant: rusqlite
        // already applies a five-second busy timeout of its own, so this changes
        // nothing today. It is here so the value is *ours* — a behaviour the
        // daemon depends on should not be an inherited default that a dependency
        // bump can silently change. `open_bounds_the_wal_size` pins it.
        //
        // (Bare SQLite does default to 0, which is where the belief that this
        // was unset came from. That default is not what we get.)
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Page cache. The default is 2 MiB, against a schema whose hot table
        // stores a JSON blob per node — a listing of a few thousand files
        // evicts itself while it is being read.
        conn.pragma_update(None, "cache_size", -CACHE_SIZE_KIB)?;
        // Sorts and recursive CTEs materialise their intermediate results.
        // Every `ORDER BY … COLLATE NOCASE` listing and every ancestor walk
        // spills those to a file in `/tmp` by default; they are small and
        // short-lived, so memory is both faster and less surprising.
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // Read the database through the page cache of the OS rather than
        // `pread` per page. Bounded rather than unlimited so the daemon's RSS
        // does not track the database size.
        conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
        // Checkpointing is synchronous in whichever thread happens to commit
        // the transaction that crosses the threshold — often a FUSE callback.
        // A smaller autocheckpoint makes that debt smaller and more frequent
        // rather than rare and multi-second.
        conn.pragma_update(None, "wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES)?;

        let db = Self {
            conn: Mutex::new(conn),
            readers: None,
            _single_writer: None,
        };
        db.migrate()?;
        Ok(db.into_conn())
    }

    /// Unwrap the connection back out of a temporary handle. Only
    /// [`open_configured`](Self::open_configured) uses it, to run `migrate`
    /// (which is written against `&self`) before the real handle exists.
    fn into_conn(self) -> Connection {
        self.conn.into_inner()
    }

    /// Open an in-memory database. For tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // Keep relational behavior identical to `open`: V18 relies on the
        // `mount.sync_folder_id` cascade, and SQLite disables foreign keys per
        // connection unless explicitly enabled.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self {
            conn: Mutex::new(conn),
            readers: None,
            _single_writer: None,
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

/// Whether `path` names the in-memory database rather than a file. Both
/// spellings SQLite accepts are checked, because neither has a lock file or a
/// corruption story.
fn is_in_memory(path: &Path) -> bool {
    let p = path.as_os_str();
    p == ":memory:" || p.is_empty()
}

/// Take the single-instance write lock for the database at `path`.
///
/// The daemon is the only writer by design — the front-ends go through the
/// control socket — but nothing stopped a hand-run `pdfs mount` from starting
/// alongside the systemd unit, at which point two processes own the same inode
/// space, the same content cache, and the same drain queue. SQLite's own
/// locking makes each *statement* safe and does nothing about that.
///
/// `flock` on a sibling `.lock` file rather than on the database itself:
/// SQLite uses POSIX record locks, so the two never interact, and a lock file
/// left behind carries no state — the lock lives in the kernel and dies with
/// the process, however it exits.
fn acquire_single_writer_lock(path: &Path) -> Result<Option<File>> {
    if is_in_memory(path) {
        return Ok(None);
    }
    let lock_path = lock_path_for(path);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    // SAFETY: `file` owns the descriptor and outlives the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(Error::Other(format!(
                "another Proton Drive daemon is already using {} — \
                 stop it first (`systemctl --user stop proton-drive.service`)",
                path.display()
            )));
        }
        return Err(Error::Io(e));
    }
    Ok(Some(file))
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Whether `e` says the file is not a usable database, as opposed to any other
/// failure. These are the only two conditions rebuilding fixes; a disk error, a
/// permission problem or a newer-than-supported schema must all still fail.
fn is_corrupt(e: &Error) -> bool {
    matches!(
        e,
        Error::Db(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::DatabaseCorrupt
                || inner.code == rusqlite::ErrorCode::NotADatabase
    )
}

/// Move a damaged database (and its WAL sidecars) out of the way so the next
/// open starts from empty.
///
/// Renamed rather than deleted: this is a cache, but it is also the only
/// evidence of what went wrong, and it costs one file the user can remove.
fn quarantine_corrupt_db(path: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for suffix in ["", "-wal", "-shm"] {
        let mut from = path.as_os_str().to_owned();
        from.push(suffix);
        let from = PathBuf::from(from);
        if !from.exists() {
            continue;
        }
        let mut to = path.as_os_str().to_owned();
        to.push(format!(".corrupt-{stamp}{suffix}"));
        let to = PathBuf::from(to);
        // A failure to rename the sidecars is survivable — SQLite discards a
        // WAL whose database header no longer matches — but failing to move the
        // database itself is not, because the retry would hit the same file.
        match std::fs::rename(&from, &to) {
            Ok(()) => tracing::warn!(from = %from.display(), to = %to.display(), "quarantined"),
            Err(e) if suffix.is_empty() => return Err(Error::Io(e)),
            Err(e) => tracing::warn!(file = %from.display(), error = %e, "could not quarantine"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
