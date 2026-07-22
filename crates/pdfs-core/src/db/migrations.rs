//! Forward-only schema migrations.
//!
//! Each `MIGRATION_V*` is the SQL that moves the schema from version N-1 to N.
//! They are applied in order and never edited once shipped: a released version
//! is already on disk somewhere, so a change here would silently diverge from
//! what an existing install actually has. Add a new one instead.

use super::Db;
use crate::Result;

/// Current schema version. Bump on every forward migration added below.
pub(super) const SCHEMA_VERSION: i64 = 16;

impl Db {
    pub(super) fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.lock();

        // `sync_state` is the key/value table holding `schema_version` and the
        // event cursor (later). Create it first so we can read the version.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT);",
        )?;

        let current: i64 = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        if current > SCHEMA_VERSION {
            return Err(crate::Error::Other(format!(
                "database schema {current} is newer than this build supports ({SCHEMA_VERSION}); refusing to open it to avoid corrupting user data"
            )));
        }
        if current == SCHEMA_VERSION {
            return Ok(());
        }

        let tx = conn.transaction()?;
        if current < 1 {
            tx.execute_batch(MIGRATION_V1)?;
        }
        if current < 2 {
            tx.execute_batch(MIGRATION_V2)?;
        }
        if current < 3 {
            tx.execute_batch(MIGRATION_V3)?;
        }
        if current < 4 {
            tx.execute_batch(MIGRATION_V4)?;
        }
        if current < 5 {
            tx.execute_batch(MIGRATION_V5)?;
        }
        if current < 6 {
            tx.execute_batch(MIGRATION_V6)?;
        }
        if current < 7 {
            tx.execute_batch(MIGRATION_V7)?;
        }
        if current < 8 {
            tx.execute_batch(MIGRATION_V8)?;
        }
        if current < 9 {
            tx.execute_batch(MIGRATION_V9)?;
        }
        if current < 10 {
            tx.execute_batch(MIGRATION_V10)?;
        }
        if current < 11 {
            tx.execute_batch(MIGRATION_V11)?;
        }
        if current < 12 {
            tx.execute_batch(MIGRATION_V12)?;
        }
        if current < 13 {
            tx.execute_batch(MIGRATION_V13)?;
        }
        if current < 14 {
            tx.execute_batch(MIGRATION_V14)?;
        }
        if current < 15 {
            tx.execute_batch(MIGRATION_V15)?;
        }
        if current < 16 {
            tx.execute_batch(MIGRATION_V16)?;
        }
        tx.execute(
            "INSERT INTO sync_state (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Schema v1: nodes + FTS5 trigram index + cache LRU. `sync_state` is created
/// in [`Db::migrate`] before this runs.
const MIGRATION_V1: &str = "
CREATE TABLE nodes (
  uid           TEXT PRIMARY KEY,
  parent_uid    TEXT,
  name          TEXT NOT NULL,
  is_dir        INTEGER NOT NULL,
  size          INTEGER,
  mtime         INTEGER NOT NULL,
  revision_hash TEXT,
  trashed       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_nodes_parent ON nodes(parent_uid);

CREATE VIRTUAL TABLE nodes_fts USING fts5(
  name, path, content='', tokenize='trigram'
);

CREATE TABLE cache_entries (
  cache_key     TEXT PRIMARY KEY,
  size_bytes    INTEGER,
  last_accessed INTEGER,
  is_pinned     INTEGER NOT NULL DEFAULT 0
);
";

/// Schema v2: node write-through (P1). `node_json` stores the full [`Node`] so
/// the hot-cache maps rehydrate losslessly; `listed` records whether a folder's
/// child listing was complete when persisted.
const MIGRATION_V2: &str = "
ALTER TABLE nodes ADD COLUMN node_json TEXT;
ALTER TABLE nodes ADD COLUMN listed INTEGER NOT NULL DEFAULT 0;
";

/// Schema v3: FTS search (P3). The v1 `nodes_fts` was contentless (`content=''`)
/// and never populated — drop it and recreate as a self-contained trigram index
/// carrying the `uid` (UNINDEXED, so retrievable but not tokenized) alongside the
/// indexed `name`. Backfill from the nodes already persisted by P1.
const MIGRATION_V3: &str = "
DROP TABLE IF EXISTS nodes_fts;
CREATE VIRTUAL TABLE nodes_fts USING fts5(
  uid UNINDEXED, name, tokenize='trigram'
);
INSERT INTO nodes_fts (uid, name)
  SELECT uid, name FROM nodes WHERE trashed = 0;
";

/// Schema v4: content-cache LRU index (P4). The v1 `cache_entries` keyed only by
/// `cache_key`; add a `kind` discriminator so the whole-file blob pool and the
/// block pool — which carry separate byte budgets — can be summed and evicted
/// independently. Existing rows (none in practice, the table was never written)
/// default to `'blob'`. The daemon rebuilds the index from disk on open, so no
/// backfill is needed here.
const MIGRATION_V4: &str = "
ALTER TABLE cache_entries ADD COLUMN kind TEXT NOT NULL DEFAULT 'blob';
";

/// Schema v5: pins move out of `pins.json` into the DB (P5). One row per
/// directly-pinned node, keyed by uid display string so a pin survives
/// renames/moves. `recursive` marks a folder pin whose whole subtree is kept;
/// descendants are resolved on demand against `nodes` via a CTE rather than
/// being expanded into rows here. `ContentCache::open` imports any legacy
/// `pins.json` into this table once, then deletes the file.
const MIGRATION_V5: &str = "
CREATE TABLE pins (
  uid       TEXT PRIMARY KEY,
  path      TEXT NOT NULL,
  recursive INTEGER NOT NULL DEFAULT 0
);
";

/// Schema v6: the index of *local* (non-Drive) files, so the launcher prompt can
/// search the machine alongside Drive. `local_files` is keyed by absolute path;
/// `scan_gen` stamps the scan that last saw a row, so a rescan prunes vanished
/// files with one `DELETE` instead of diffing.
///
/// `local_fts` is an *external-content* FTS5 index over `local_files.name`: the
/// text lives once in the base table and the index is rebuilt in bulk at the end
/// of a scan (`INSERT INTO local_fts(local_fts) VALUES('rebuild')`). That is far
/// cheaper than the delete-then-insert-per-row dance `nodes_fts` needs, because
/// a scan rewrites most rows at once rather than trickling single updates.
const MIGRATION_V6: &str = "
CREATE TABLE local_files (
  id       INTEGER PRIMARY KEY,
  path     TEXT NOT NULL UNIQUE,
  name     TEXT NOT NULL,
  is_dir   INTEGER NOT NULL,
  size     INTEGER NOT NULL DEFAULT 0,
  mtime    INTEGER NOT NULL DEFAULT 0,
  scan_gen INTEGER NOT NULL DEFAULT 0
);

CREATE VIRTUAL TABLE local_fts USING fts5(
  name, content='local_files', content_rowid='id', tokenize='trigram'
);
";

/// Schema v7: the photos timeline and the trash listing become persistent, so
/// opening the app paints them from disk instead of re-fetching the world (both
/// were memory-only: the timeline behind a 60 s TTL, the trash not cached at all).
///
/// `photos.seq` preserves the server's newest-first order, which is the only order
/// the timeline has — `capture_time` ties are common (a burst of shots) and would
/// otherwise shuffle between refreshes. `ratio` and `thumb_state` are *locally
/// learned*: they cost a download to rediscover, so `photos_replace` carries them
/// across a refresh while capture times and order come from the server.
const MIGRATION_V7: &str = "
CREATE TABLE photos (
  uid          TEXT PRIMARY KEY,
  capture_time INTEGER NOT NULL,
  name         TEXT,
  ratio        REAL,
  thumb_state  INTEGER NOT NULL DEFAULT 0,
  seq          INTEGER NOT NULL
);
CREATE INDEX idx_photos_seq ON photos(seq);

CREATE TABLE trash (
  uid    TEXT PRIMARY KEY,
  name   TEXT NOT NULL,
  is_dir INTEGER NOT NULL,
  size   INTEGER NOT NULL DEFAULT 0,
  mtime  INTEGER NOT NULL DEFAULT 0
);
";

/// Schema v8: device sync (devices.md). This machine registers as one Proton Drive
/// **Device** (a share + root folder on the main volume); `device` is a singleton
/// row cached so we reuse the same device across restarts instead of creating a
/// new one each run. `sync_folder` is one row per local folder the user added,
/// each mapped to a folder under the device root; `mode` is `mirror` (full local
/// copy, two-way synced) or `ondemand` (FUSE mount at `local_path`, no local
/// storage). `sync_entry` is the per-file baseline for three-way merge — added in
/// this migration so Phase 2 has the table, though Phase 1 leaves it empty.
const MIGRATION_V8: &str = "
CREATE TABLE device (
  uid      TEXT PRIMARY KEY,
  share_id TEXT NOT NULL,
  root_uid TEXT NOT NULL,
  name     TEXT NOT NULL,
  created  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE sync_folder (
  id              INTEGER PRIMARY KEY,
  local_path      TEXT NOT NULL UNIQUE,
  remote_uid      TEXT NOT NULL,
  remote_share_id TEXT NOT NULL,
  mode            TEXT NOT NULL DEFAULT 'mirror',
  state           TEXT NOT NULL DEFAULT 'idle',
  last_sync       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE sync_entry (
  folder_id   INTEGER NOT NULL,
  rel_path    TEXT NOT NULL,
  remote_uid  TEXT,
  local_mtime INTEGER NOT NULL DEFAULT 0,
  local_size  INTEGER NOT NULL DEFAULT 0,
  remote_hash TEXT,
  remote_rev  TEXT,
  PRIMARY KEY (folder_id, rel_path)
);
";

const MIGRATION_V9: &str = "
CREATE TABLE activity (
  id     INTEGER PRIMARY KEY,
  time   INTEGER NOT NULL,
  kind   TEXT NOT NULL,
  target TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  ok     INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX activity_time ON activity(time DESC);
";

/// Schema v10: a mode switch the user asked for is queued rather than rejected
/// when the folder is mid-pass or has un-uploaded changes, so `pending_mode`
/// records the intent until the engine can act on it. NULL is the resting state:
/// the folder is already where the user wants it.
const MIGRATION_V10: &str = "
ALTER TABLE sync_folder ADD COLUMN pending_mode TEXT;
";

/// Schema v11: writes no longer upload inside the FUSE handler. `release` stages
/// the bytes and records the intended upload here, and a drain worker performs it
/// (offline.md Phase 3). The row outlives the process, so a write survives both a
/// dead network and a restart.
///
/// `blob_path` points into the content cache's `staging/` dir and `meta_json` is
/// the [`StagedWrite`] sidecar describing which of its bytes are real — an
/// incomplete blob must be gap-filled from the remote base before it can be
/// uploaded. `next_attempt_at` is a ms deadline implementing retry backoff.
///
/// [`StagedWrite`]: crate::cache::StagedWrite
const MIGRATION_V11: &str = "
CREATE TABLE pending_op (
  id              INTEGER PRIMARY KEY,
  kind            TEXT NOT NULL,
  uid             TEXT NOT NULL,
  blob_path       TEXT,
  meta_json       TEXT,
  created_at      INTEGER NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0,
  last_error      TEXT,
  next_attempt_at INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX pending_op_uid ON pending_op(uid);
";

/// Schema v12: `pending_op` also carries mutations that *create* a node, which a
/// revision op never had to describe — it always addressed a node the server had
/// already minted a uid for (offline.md Phase 3b).
///
/// An offline `create`/`mkdir` cannot get a real uid, so the node is invented
/// locally under a `local~<uuid>` placeholder and the op records where it goes
/// (`parent_uid`) and what it is called (`name`). Both are nullable because a
/// revision op sets neither.
///
/// `parent_uid` is indexed: draining a folder rewrites every queued child that
/// points at its placeholder.
const MIGRATION_V12: &str = "
ALTER TABLE pending_op ADD COLUMN parent_uid TEXT;
ALTER TABLE pending_op ADD COLUMN name TEXT;

CREATE INDEX pending_op_parent ON pending_op(parent_uid);
";

/// Schema v13: photos record their media type and a derived `kind` so the
/// timeline can be split into Photos / Videos / Raw tabs. `media_type` is
/// nullable — it is filled once the daemon resolves a photo's node — while
/// `kind` (see [`crate::control::PhotoKind`]) defaults to `0` (still photo), the
/// classification anything unresolved falls back to. Existing rows are
/// reclassified for free on the next timeline refresh, so no backfill is needed.
const MIGRATION_V13: &str = "
ALTER TABLE photos ADD COLUMN media_type TEXT;
ALTER TABLE photos ADD COLUMN kind INTEGER NOT NULL DEFAULT 0;
";

/// Schema v14: index the content-cache LRU. `cache_entries` carried only its
/// `cache_key` primary key, so every budget check and every eviction pass was a
/// full scan plus a sort — on a path that runs once per cached 4 MiB block,
/// under the connection lock every FUSE metadata call also needs. `(kind,
/// last_accessed)` turns both into a range scan: the `SUM` reads one kind's
/// slice, and eviction reads its front and stops.
const MIGRATION_V14: &str = "
CREATE INDEX cache_entries_lru ON cache_entries(kind, last_accessed);
";

/// Schema v15: key the search index by rowid so a row can be replaced without
/// scanning the whole index (B12).
///
/// v3 built `nodes_fts` with the node's `uid` as an `UNINDEXED` column and
/// `upsert_nodes` refreshed a row with `DELETE FROM nodes_fts WHERE uid = ?`.
/// `UNINDEXED` means exactly what it says — the column is retrievable but not
/// searchable — so that predicate could only ever be a full scan of the index
/// (`EXPLAIN QUERY PLAN`: `SCAN nodes_fts VIRTUAL TABLE`). Every node written
/// scanned every node indexed: measured at 6.5 ms per node against a 17k-node
/// index, about half the cost of a cold listing, and *growing with the size of
/// the account* rather than the size of the listing.
///
/// FTS5 deletes by `rowid` efficiently, and `nodes` is an ordinary rowid table
/// (`uid` is its `TEXT PRIMARY KEY`, not `WITHOUT ROWID`), so its rowid is a
/// stable per-node key that `ON CONFLICT DO UPDATE` preserves. Rebuild the index
/// keyed by it and drop the `uid` column: searches now join on `rowid`, which is
/// also a cheaper join than the old one on a TEXT uid.
const MIGRATION_V15: &str = "
DROP TABLE IF EXISTS nodes_fts;
CREATE VIRTUAL TABLE nodes_fts USING fts5(
  name, tokenize='trigram'
);
INSERT INTO nodes_fts (rowid, name)
  SELECT rowid, name FROM nodes WHERE trashed = 0;
";

/// Schema v16: fuzzy search needs a bounded candidate set that includes parent
/// paths.  Keeping the path in the trigram indexes lets SQLite narrow both name
/// and path candidates without walking either base table for every keystroke.
/// Node paths are initially backfilled below and subsequently maintained by the
/// node write path (including descendants after a folder move/rename).
const MIGRATION_V16: &str = "
DROP TABLE IF EXISTS nodes_fts;
CREATE VIRTUAL TABLE nodes_fts USING fts5(
  name, path, tokenize='trigram'
);
CREATE INDEX IF NOT EXISTS idx_nodes_name_nocase ON nodes(name COLLATE NOCASE);
INSERT INTO nodes_fts (rowid, name, path)
WITH RECURSIVE paths(rowid, uid, path) AS (
  SELECT n.rowid, n.uid, '' FROM nodes n
   WHERE n.parent_uid IS NULL
      OR NOT EXISTS (SELECT 1 FROM nodes p WHERE p.uid = n.parent_uid)
  UNION ALL
  SELECT n.rowid, n.uid,
         CASE WHEN paths.path = '' THEN n.name ELSE paths.path || '/' || n.name END
    FROM nodes n JOIN paths ON n.parent_uid = paths.uid
)
SELECT n.rowid, n.name, paths.path
  FROM nodes n JOIN paths ON paths.rowid = n.rowid
 WHERE n.trashed = 0;

DROP TABLE IF EXISTS local_fts;
CREATE VIRTUAL TABLE local_fts USING fts5(
  name, path, content='local_files', content_rowid='id', tokenize='trigram'
);
CREATE INDEX IF NOT EXISTS idx_local_files_name_nocase
  ON local_files(name COLLATE NOCASE);
INSERT INTO local_fts(local_fts) VALUES('rebuild');
";
