//! Forward-only schema migrations.
//!
//! Each `MIGRATION_V*` is the SQL that moves the schema from version N-1 to N.
//! They are applied in order and never edited once shipped: a released version
//! is already on disk somewhere, so a change here would silently diverge from
//! what an existing install actually has. Add a new one instead.

use super::Db;
use crate::Result;

/// Current schema version. Bump on every forward migration added below.
pub(super) const SCHEMA_VERSION: i64 = 30;

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
        if current < 17 {
            tx.execute_batch(MIGRATION_V17)?;
        }
        if current < 18 {
            tx.execute_batch(MIGRATION_V18)?;
        }
        if current < 19 {
            tx.execute_batch(MIGRATION_V19)?;
        }
        if current < 20 {
            tx.execute_batch(MIGRATION_V20)?;
        }
        if current < 21 {
            // A V17-and-older fixture may carry only the tables its own test
            // cares about; there is nothing to reindex without `nodes`.
            let has_nodes: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'nodes'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if has_nodes {
                tx.execute_batch(MIGRATION_V21)?;
            }
        }
        if current < 22 {
            // Older fixtures (and a database that never grew a table because
            // its feature was never used) may be missing any of these, and an
            // index on an absent table is a hard error rather than a no-op.
            for (table, sql) in MIGRATION_V22 {
                let present: bool = tx.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get::<_, i64>(0),
                )? > 0;
                if present {
                    tx.execute_batch(sql)?;
                }
            }
            // `activity_time` indexes `time DESC` while both activity queries
            // order by `id`: it has only ever cost writes.
            tx.execute_batch("DROP INDEX IF EXISTS activity_time;")?;
        }
        if current < 23 {
            // One last full rebuild of the local index, because from here on the
            // scan maintains it incrementally (`local_upsert_batch`) and so can
            // no longer repair a `local_fts` that drifted from `local_files`.
            // Whatever it costs, it costs once, at startup, before the mount
            // serves anything.
            let present: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'local_fts'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if present {
                tx.execute_batch("INSERT INTO local_fts(local_fts) VALUES('rebuild');")?;
            }
        }
        if current < 24 {
            tx.execute_batch(MIGRATION_V24)?;
        }
        if current < 25 {
            // Same guard as V21: an old fixture may not have `nodes` at all,
            // and there is no path to materialise without it.
            let has_nodes: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'nodes'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if has_nodes {
                tx.execute_batch(MIGRATION_V25)?;
            }
        }
        if current < 26 {
            // Same guard as V22: a fixture that never exercised the queue may
            // not have `pending_op`, and altering an absent table is an error
            // rather than a no-op.
            // A fixture built from the current schema and then rewound to an
            // older `schema_version` already has the column, and `ADD COLUMN`
            // has no `IF NOT EXISTS`.
            let has_column: bool = tx.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pending_op') WHERE name = 'claimed_at'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            let has_ops: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pending_op'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if has_ops && !has_column {
                tx.execute_batch(MIGRATION_V26)?;
            }
        }
        if current < 27 {
            // Same guards as V26, for the same two reasons: the table may not
            // exist in an old fixture, and a fixture rewound from the current
            // schema already has the column.
            let has_column: bool = tx.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pending_op') \
                 WHERE name = 'access_deferred_since'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            let has_ops: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pending_op'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if has_ops && !has_column {
                tx.execute_batch(MIGRATION_V27)?;
            }
        }
        if current < 28 {
            // Same guards as V26/V27, for the same two reasons.
            let has_column: bool = tx.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('sync_entry') \
                 WHERE name = 'local_mtime_ns'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            let has_entries: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sync_entry'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if has_entries && !has_column {
                tx.execute_batch(MIGRATION_V28)?;
            }
        }
        if current < 29 {
            // Same guards as V26/V27/V28: an old fixture may not have `trash`,
            // and a fixture rewound from the current schema already has the
            // column.
            let has_column: bool = tx.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('trash') WHERE name = 'parent_uid'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            let has_trash: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'trash'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if has_trash && !has_column {
                tx.execute_batch(MIGRATION_V29)?;
            }
        }
        if current < 30 {
            // Same guards as V26-V29: the column may already be there on a
            // fixture rewound from the current schema, and a very old fixture
            // may not have `photos` at all.
            let has_column: bool = tx.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('photos') WHERE name = 'group_key'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            let has_photos: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'photos'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if has_photos && !has_column {
                tx.execute_batch(MIGRATION_V30)?;
            }
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

/// Schema v17: effective access for roots shared with this account. The role is
/// persisted separately from `nodes.node_json` so a cold/offline mount can
/// enforce the last known permission even when the SDK cannot refresh metadata.
const MIGRATION_V17: &str = "
CREATE TABLE share_access (
  root_uid TEXT PRIMARY KEY,
  access   TEXT NOT NULL
           CHECK (access IN ('owner', 'editor', 'viewer', 'unknown'))
);
";

/// Schema v18: unified location presentation. Device rows carry only their
/// `sync_folder` identity; reads join all device state from that authoritative
/// table. The trigger projects future folders, while the foreign key removes a
/// presentation row with its sync folder.
const MIGRATION_V18: &str = "
CREATE TABLE mount (
  id              INTEGER PRIMARY KEY,
  kind            TEXT NOT NULL
                  CHECK (kind IN ('myfiles', 'device', 'shared')),
  sync_folder_id  INTEGER REFERENCES sync_folder(id) ON DELETE CASCADE,
  share_root_uid  TEXT,
  local_path      TEXT NOT NULL DEFAULT '',
  root_uid        TEXT NOT NULL DEFAULT '',
  root_share_id   TEXT NOT NULL DEFAULT '',
  mode            TEXT NOT NULL DEFAULT 'unknown',
  access          TEXT NOT NULL DEFAULT 'rw'
                  CHECK (access IN ('rw', 'ro')),
  CHECK (
    (kind = 'myfiles' AND sync_folder_id IS NULL AND share_root_uid IS NULL)
    OR
    (kind = 'device' AND sync_folder_id IS NOT NULL AND share_root_uid IS NULL)
    OR
    (kind = 'shared' AND sync_folder_id IS NULL AND share_root_uid IS NOT NULL)
  )
);

CREATE UNIQUE INDEX mount_myfiles
  ON mount(kind) WHERE kind = 'myfiles';
CREATE UNIQUE INDEX mount_device
  ON mount(sync_folder_id) WHERE kind = 'device';
CREATE UNIQUE INDEX mount_shared
  ON mount(share_root_uid) WHERE kind = 'shared';

INSERT INTO mount (kind, sync_folder_id)
SELECT 'device', id FROM sync_folder;

CREATE TRIGGER mount_sync_folder_insert
AFTER INSERT ON sync_folder
BEGIN
  INSERT OR IGNORE INTO mount (kind, sync_folder_id)
  VALUES ('device', NEW.id);
END;
";

/// Schema v19: photo albums and their contents. Both are projections of the
/// server's listings — like `photos`, they are replaced wholesale on refresh —
/// so the Albums view opens from disk instead of re-enumerating every launch.
///
/// `album_photos` carries its own `ratio` / `thumb_state` rather than joining
/// `photos`: an album shared with us lives on the sharer's volume, so its
/// photos are not in our timeline at all and would otherwise have nowhere to
/// remember what a thumbnail attempt learned.
const MIGRATION_V19: &str = "
CREATE TABLE albums (
  uid           TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  photo_count   INTEGER NOT NULL DEFAULT 0,
  cover_uid     TEXT,
  last_activity INTEGER,
  shared        INTEGER NOT NULL DEFAULT 0,
  seq           INTEGER NOT NULL
);

CREATE TABLE album_photos (
  album_uid    TEXT NOT NULL,
  uid          TEXT NOT NULL,
  capture_time INTEGER NOT NULL,
  name         TEXT,
  media_type   TEXT,
  kind         INTEGER NOT NULL DEFAULT 0,
  ratio        REAL,
  thumb_state  INTEGER NOT NULL DEFAULT 0,
  seq          INTEGER NOT NULL,
  PRIMARY KEY (album_uid, uid)
);

CREATE INDEX idx_album_photos_seq ON album_photos(album_uid, seq);
CREATE INDEX idx_album_photos_uid ON album_photos(uid);
";

/// V20: whether a photo is a favourite.
///
/// A column rather than a tag table: `Favorite` is the only Proton photo tag a
/// user sets by hand (the rest — Video, Screenshot, Selfie, Raw… — are the
/// server's own classification, and the Photos page already derives its tabs
/// from the media type). Defaulting to 0 means an existing timeline stays
/// intact and each row learns its real value on the next refresh, the same way
/// `media_type` did.
const MIGRATION_V20: &str = "
ALTER TABLE photos ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_photos_favorite ON photos(favorite, seq);
";

/// V21: rebuild the node search index after fixing what counts as indexable.
///
/// The v16 backfill rooted its path walk at any node whose parent is null *or*
/// not cached, but the write path's `node_is_indexable_tx` demanded a
/// null-parent ancestor. Only the My Files root has one — a device folder's
/// root is stored in `device`/`sync_folder`, never in `nodes` — so every node
/// under a device folder was dropped from `nodes_fts` the first time it was
/// upserted after the backfill, and was unfindable from the prompt (2,705 of
/// 9,252 nodes on the account this was diagnosed against).
///
/// The rule now matches the backfill, so this replays it: same recursive walk
/// as v16, over the current `nodes`.
/// Two details the v16 backfill did not carry, both cheap here: a node under a
/// trashed folder stays out (the write path has always excluded it, and
/// `tombstone_subtree_tx` only marks a subtree it was told about), and the walk
/// is depth-capped so a parent cycle in a corrupted database cannot spin the
/// migration forever.
const MIGRATION_V21: &str = "
DROP TABLE IF EXISTS nodes_fts;
CREATE VIRTUAL TABLE nodes_fts USING fts5(
  name, path, tokenize='trigram'
);
INSERT INTO nodes_fts (rowid, name, path)
WITH RECURSIVE paths(rowid, uid, path, trashed, depth) AS (
  -- Only a true root (no parent at all) contributes nothing to the path: its
  -- name is the mount. A row whose parent is merely uncached is an ordinary
  -- folder and keeps its name, which is what `path_of` does at runtime.
  SELECT n.rowid, n.uid,
         CASE WHEN n.parent_uid IS NULL THEN '' ELSE n.name END, n.trashed, 0
    FROM nodes n
   WHERE n.parent_uid IS NULL
      OR NOT EXISTS (SELECT 1 FROM nodes p WHERE p.uid = n.parent_uid)
  UNION ALL
  SELECT n.rowid, n.uid,
         CASE WHEN paths.path = '' THEN n.name ELSE paths.path || '/' || n.name END,
         MAX(paths.trashed, n.trashed), paths.depth + 1
    FROM nodes n JOIN paths ON n.parent_uid = paths.uid
   WHERE paths.depth < 256
)
SELECT n.rowid, n.name, paths.path
  FROM nodes n JOIN paths ON paths.rowid = n.rowid
 WHERE paths.trashed = 0;
";

/// V22: indexes for the lookups that were scanning, each paired with the table
/// it needs to exist for.
///
/// Each backs a query on a hot path that had no index to use:
///
/// - `sync_entry(remote_uid)` — the reconcile resolves a remote uid to its
///   local entry once per node, per pass.
/// - `local_files(scan_gen)` — the sweep that retires the previous generation
///   after a local index scan.
/// - `nodes(parent_uid) WHERE trashed = 0` — every listing, with the partial
///   index keeping it to the live rows a listing actually wants.
///
/// Deliberately *not* here: an index on `pending_op(next_attempt_at, id)`.
/// `next_due_op` orders by `id`, which is the rowid, so it already stops at the
/// first due row; offering the planner a `next_attempt_at` index makes it scan
/// that index and then sort the matches by `id` instead —
/// `next_due_op_does_not_scale_with_queue_length` measures the regression.
const MIGRATION_V22: [(&str, &str); 3] = [
    (
        "sync_entry",
        "CREATE INDEX IF NOT EXISTS idx_sync_entry_remote_uid ON sync_entry(remote_uid);",
    ),
    (
        "local_files",
        "CREATE INDEX IF NOT EXISTS idx_local_files_scan_gen ON local_files(scan_gen);",
    ),
    (
        "nodes",
        "CREATE INDEX IF NOT EXISTS idx_nodes_parent_live ON nodes(parent_uid) WHERE trashed = 0;",
    ),
];

/// V24: the revisions this daemon sealed itself, so restarting does not reopen
/// the conflict hole B70 layer B closed.
///
/// `Core::own_sealed_revs` held these in memory: a queued write whose baseline
/// names an earlier revision is a conflict *unless* the remote sits at a
/// revision we sealed, in which case it is one writer stalling and resuming and
/// must supersede rather than fork a `(sync-conflict)` copy. Losing the map at
/// shutdown meant the first drain after a restart could not tell the two apart.
///
/// `sealed_at` is what bounds the table: entries older than the window in which
/// a stall→resume is plausible are pruned on write.
const MIGRATION_V24: &str = "
CREATE TABLE IF NOT EXISTS own_sealed_rev (
  uid         TEXT PRIMARY KEY,
  revision_id TEXT NOT NULL,
  sealed_at   INTEGER NOT NULL
);
";

/// V25: `nodes.path` — a node's mountpoint-relative path, stored rather than
/// walked.
///
/// `path_of` was a recursive CTE per node, and search ran it once per candidate
/// (up to a thousand per keystroke) on top of the one the write path already ran
/// per descendant of every folder upsert. The value only changes when a node's
/// name or parent changes, which is exactly when the write path is already
/// walking the subtree — so it belongs in a column.
///
/// The backfill is the same walk V21 used for `nodes_fts`, minus the trashed
/// filter: a trashed node stays out of the index but still has a path. Roots get
/// `''` (their name is the mount), and a node whose parent is merely uncached
/// keeps its own name — both matching what `path_of` returned at runtime.
///
/// Materialised into a temp table rather than written by a correlated subquery,
/// so the recursive walk runs once instead of once per row.
const MIGRATION_V25: &str = "
ALTER TABLE nodes ADD COLUMN path TEXT;

CREATE TEMP TABLE node_paths (uid TEXT PRIMARY KEY, path TEXT NOT NULL);

INSERT INTO node_paths (uid, path)
WITH RECURSIVE paths(uid, path, depth) AS (
  SELECT n.uid, CASE WHEN n.parent_uid IS NULL THEN '' ELSE n.name END, 0
    FROM nodes n
   WHERE n.parent_uid IS NULL
      OR NOT EXISTS (SELECT 1 FROM nodes p WHERE p.uid = n.parent_uid)
  UNION ALL
  SELECT n.uid,
         CASE WHEN paths.path = '' THEN n.name ELSE paths.path || '/' || n.name END,
         paths.depth + 1
    FROM nodes n JOIN paths ON n.parent_uid = paths.uid
   WHERE paths.depth < 256
)
SELECT uid, path FROM paths;

UPDATE nodes
   SET path = (SELECT p.path FROM node_paths p WHERE p.uid = nodes.uid)
 WHERE EXISTS (SELECT 1 FROM node_paths p WHERE p.uid = nodes.uid);

DROP TABLE node_paths;
";

/// V26: `pending_op.claimed_at` — the drain's claim column.
///
/// The drain was one thread taking one op at a time, so a 10 GiB upload held
/// every queued rename and small write behind it for as long as it ran. Several
/// workers can share the queue only if a claim is visible to the others: this
/// column is what makes "already being drained by someone" a fact about the row
/// rather than about one thread's local variable.
///
/// `0` is unclaimed; anything else is the ms at which a worker took it. A
/// claim is process-local state that outlives nothing — the single-writer lock
/// means a non-zero value found at open belongs to a crashed run, and
/// [`Db::clear_op_claims`](super::Db::clear_op_claims) drops the lot.
///
/// The index is partial so it costs nothing on the ordinary row: only the
/// handful of claimed rows are in it, which is exactly the set the claim query
/// has to subtract to keep two workers off one node.
const MIGRATION_V26: &str = "
ALTER TABLE pending_op ADD COLUMN claimed_at INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_pending_op_claimed
    ON pending_op(uid) WHERE claimed_at <> 0;
";

/// Schema v27: when an op was first deferred by the local access check.
///
/// An access deferral consumes no attempt and records no error — the op has not
/// failed remotely, it has not been sent at all. That is right for a permission
/// change the user is about to undo, and wrong for a condition that never
/// clears: the row is re-deferred every
/// [`DRAIN_ACCESS_RECHECK`](../../../pdfs_fuse/drain/constant.DRAIN_ACCESS_RECHECK.html)
/// forever while `attempts`, `last_error`, `failing` and the logs all stay
/// empty, so a queue that can never drain is indistinguishable from a busy one.
/// Found in the field as 28 revision ops holding 18 GiB of accepted writes for
/// 30 days, none of them visible anywhere.
///
/// `0` means "not currently access-deferred"; anything else is the ms at which
/// the first consecutive deferral happened, which is what
/// [`Db::defer_op_for_access`](super::Db::defer_op_for_access) measures the
/// deferral against.
const MIGRATION_V27: &str = "
ALTER TABLE pending_op ADD COLUMN access_deferred_since INTEGER NOT NULL DEFAULT 0;
";

/// Schema v28: sub-second precision for a mirror folder's local baseline.
///
/// A reconcile pass decides whether the local side of a path changed by
/// comparing `(local_mtime, local_size)` against the baseline. Both sides of
/// that comparison were whole seconds, so an edit that landed in the same second
/// as the last sync and left the file the same length — a fixed-size record
/// rewritten, an image re-exported, a database page updated in place — read as
/// *unchanged* and was never uploaded. The remote copy silently stayed at the
/// older content until something else about the file moved (bugs.md B25).
///
/// Nullable rather than a converted `local_mtime * 1_000_000_000`: the
/// nanosecond part of an existing baseline is genuinely unknown, and inventing
/// zero for it would make every already-synced file compare as changed and
/// re-upload an entire mirror on first start. `NULL` means "this row predates
/// the column", which the comparison reads as "fall back to whole seconds" —
/// exactly the behaviour that row was written under. Each row gains a real value
/// the next time its path is synced.
const MIGRATION_V28: &str = "
ALTER TABLE sync_entry ADD COLUMN local_mtime_ns INTEGER;
";

/// Schema v29: the trash listing remembers each trashed node's parent, so a
/// restore can reason about the shape of what it is putting back.
///
/// Trash is a flat list of uids on the wire, but the user deletes a *tree*: a
/// folder and, often, items inside it that were trashed separately. Restoring
/// one of those in isolation is what the parent link fixes — a child restored
/// under a still-trashed parent lands somewhere invisible, and a folder restored
/// without its separately-trashed contents comes back empty.
///
/// `NULL` means "this row predates the column" (or the node is a volume root);
/// the next trash refresh fills it in, and until then the restore behaves as it
/// did before.
const MIGRATION_V29: &str = "
ALTER TABLE trash ADD COLUMN parent_uid TEXT;
CREATE INDEX IF NOT EXISTS idx_trash_parent ON trash(parent_uid);
";

/// Schema v30: a RAW and the JPEG of the same shot are one photo to the person
/// who took it, so the timeline records what ties them together.
///
/// `content_hash` and `main_uid` come from the server's own photo properties —
/// the duplicate-detection hash, and the main photo a related one belongs to.
/// `group_key` is what the gallery pages by: every member of one shot carries
/// the same key, and the member the grid shows is the one whose `main_uid` is
/// its own uid.
///
/// All three are `NULL` until the next timeline refresh resolves the photo's
/// node, and a row without a `group_key` is its own group — which is exactly how
/// the gallery behaved before this column existed.
const MIGRATION_V30: &str = "
ALTER TABLE photos ADD COLUMN content_hash TEXT;
ALTER TABLE photos ADD COLUMN main_uid TEXT;
ALTER TABLE photos ADD COLUMN group_key TEXT;
CREATE INDEX IF NOT EXISTS idx_photos_group ON photos(group_key);
";
