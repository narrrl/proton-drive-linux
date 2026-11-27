//! Folder-sync bookkeeping: the folders this device mirrors, and the per-file
//! state each pass reconciles against.

use rusqlite::{OptionalExtension, params};

use super::Db;
use pdfs_core::Result;
use std::collections::HashMap;

/// The deletions a mirror folder has made since a window began, which the
/// wipe guard adds to a pass's own before it lets that pass delete anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeleteWindow {
    /// When the window began, in seconds since the epoch.
    pub started_at: i64,
    /// The most paths the folder has tracked since then.
    pub reference: u64,
    /// The deletions made since then, on either side.
    pub deleted: u64,
}

/// What a mirror pass has walked of Drive, saved so a pass cut short
/// resumes the walk instead of listing every folder again (B294), and so a
/// finished walk serves as the next pass's picture of Drive.
///
/// While a walk is not complete, the database drops it whenever the folder's
/// baseline changes, so a resumed listing never meets a baseline newer than
/// itself. Once it is complete, a change to the baseline instead marks the
/// folder it happened in as dirty (see [`Db::sync_remote_dirty`]), and the
/// next pass lists that folder again.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedWalk {
    /// The folder's root on Drive when the walk began.
    pub root_uid: String,
    /// When the last full walk began. A complete walk is kept up to date
    /// folder by folder, so this is the age of its oldest listings.
    pub started: i64,
    /// Whether the walk reached every folder once. Some may have been found
    /// since, and wait unlisted in `dirs`.
    pub complete: bool,
    pub dirs: Vec<SavedWalkDir>,
    pub items: Vec<SavedWalkItem>,
}

/// A folder on Drive the saved walk found.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedWalkDir {
    pub rel_path: String,
    pub uid: String,
    /// Whether its listing is saved. One that failed stays unlisted, so a
    /// resumed walk asks again.
    pub listed: bool,
    /// Its listing named nodes that could not be read.
    pub short: bool,
}

/// A path the saved walk read on Drive. `uid` is `None` for a file it saw
/// but could not read the size of.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedWalkItem {
    pub rel_path: String,
    pub uid: Option<String>,
    pub is_dir: bool,
    pub mtime: i64,
    pub size: i64,
    pub revision: Option<String>,
    pub sha1: Option<String>,
}

pub struct StoredSyncFolder {
    pub id: i64,
    /// `device` backup or `attached` My files mirror.
    pub kind: String,
    pub local_path: String,
    pub remote_uid: String,
    pub remote_share_id: String,
    /// `mirror` (full local copy, two-way synced) or `ondemand` (FUSE mount).
    pub mode: String,
    /// A mode the user asked for that could not be applied on the spot — the
    /// folder was mid-pass, or had un-uploaded changes. The engine applies it
    /// once the folder is safe to switch, so the request is queued rather than
    /// rejected. `None` when the folder is where the user wants it.
    pub pending_mode: Option<String>,
    /// `idle` | `syncing` | `error` | `conflict`.
    pub state: String,
    pub last_sync: i64,
}

/// One per-file sync baseline row: what a path looked like on both sides at the
/// last successful sync, so the next reconcile can tell which side changed.
/// `remote_rev`/`remote_hash` hold the remote signature —
/// its modification time and size as strings — since no cheap content hash is
/// exposed; change detection is `(mtime, size)` on each side.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredSyncEntry {
    pub rel_path: String,
    pub remote_uid: Option<String>,
    pub local_mtime: i64,
    pub local_size: i64,
    /// The same modification time to nanosecond precision, when it is known.
    ///
    /// `local_mtime` alone cannot tell an edit that landed inside the same
    /// second as the last sync — and left the file the same length — from no
    /// edit at all, so those never uploaded (`docs/BUGS.md` B25). `None` on a row
    /// written before schema 28, which the comparison reads as "compare whole
    /// seconds", so upgrading does not make an entire mirror look changed.
    pub local_mtime_ns: Option<i64>,
    pub remote_rev: Option<String>,
    pub remote_hash: Option<String>,
    /// The id of the remote revision this row was synced at. A mirror upload
    /// checks it before it supersedes what Drive holds, because another client
    /// may have committed a revision since (`docs/BUGS.md` B249). `None` on a
    /// row written before schema 44, or by a pass that did not learn it; the
    /// check then falls back to `remote_rev` and `remote_hash`.
    pub remote_revision_id: Option<String>,
}

impl Db {
    pub fn sync_folder_add(
        &self,
        local_path: &str,
        remote_uid: &str,
        remote_share_id: &str,
    ) -> Result<i64> {
        self.sync_folder_insert(local_path, remote_uid, remote_share_id, "device")
    }

    /// Attach an existing My files folder, without creating a device backup.
    pub fn sync_folder_attach(
        &self,
        local_path: &str,
        remote_uid: &str,
        remote_share_id: &str,
    ) -> Result<i64> {
        self.sync_folder_insert(local_path, remote_uid, remote_share_id, "attached")
    }

    fn sync_folder_insert(
        &self,
        local_path: &str,
        remote_uid: &str,
        remote_share_id: &str,
        kind: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_folder (local_path, remote_uid, remote_share_id, kind)
             VALUES (?1, ?2, ?3, ?4)",
            params![local_path, remote_uid, remote_share_id, kind],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Every synced folder, oldest first.
    pub fn sync_folder_list(&self) -> Result<Vec<StoredSyncFolder>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, local_path, remote_uid, remote_share_id, mode, pending_mode, state, last_sync, kind
             FROM sync_folder ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StoredSyncFolder {
                id: r.get(0)?,
                local_path: r.get(1)?,
                remote_uid: r.get(2)?,
                remote_share_id: r.get(3)?,
                mode: r.get(4)?,
                pending_mode: r.get(5)?,
                state: r.get(6)?,
                last_sync: r.get(7)?,
                kind: r.get(8)?,
            })
        })?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    /// Look up one synced folder by id.
    pub fn sync_folder_get(&self, id: i64) -> Result<Option<StoredSyncFolder>> {
        let conn = self.read()?;
        conn.query_row(
            "SELECT id, local_path, remote_uid, remote_share_id, mode, pending_mode, state, last_sync, kind
             FROM sync_folder WHERE id = ?1",
            params![id],
            |r| {
                Ok(StoredSyncFolder {
                    id: r.get(0)?,
                    local_path: r.get(1)?,
                    remote_uid: r.get(2)?,
                    remote_share_id: r.get(3)?,
                    mode: r.get(4)?,
                    pending_mode: r.get(5)?,
                    state: r.get(6)?,
                    last_sync: r.get(7)?,
                    kind: r.get(8)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Remove a synced folder and its per-file baseline.
    pub fn sync_folder_remove(&self, id: i64) -> Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM sync_entry WHERE folder_id = ?1", params![id])?;
        let n = tx.execute("DELETE FROM sync_folder WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Update a synced folder's mode (`mirror`/`ondemand`). A queued request for
    /// that mode is satisfied and cleared in the same write. An opposite pending
    /// mode arrived after this transition started, so preserve it as the newer
    /// last-request intent.
    pub fn sync_folder_set_mode(&self, id: i64, mode: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sync_folder
                SET mode = ?2,
                    pending_mode = CASE WHEN pending_mode = ?2 THEN NULL ELSE pending_mode END
              WHERE id = ?1",
            params![id, mode],
        )?;
        Ok(())
    }

    /// Re-point a synced folder at the share that now holds its remote folder,
    /// after the folder moved to another device without changing its uid.
    pub fn sync_folder_set_share(&self, id: i64, share_id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sync_folder SET remote_share_id = ?2 WHERE id = ?1",
            params![id, share_id],
        )?;
        Ok(())
    }

    /// Queue (or, with `None`, withdraw) a mode the folder should move to once it
    /// is safe to switch.
    pub fn sync_folder_set_pending_mode(&self, id: i64, mode: Option<&str>) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sync_folder SET pending_mode = ?2 WHERE id = ?1",
            params![id, mode],
        )?;
        Ok(())
    }

    /// Withdraw a queued mode only if it is still the intent the caller
    /// inspected. A newer request must survive an older settlement attempt.
    pub fn sync_folder_clear_pending_mode_if(&self, id: i64, expected: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE sync_folder SET pending_mode = NULL
             WHERE id = ?1 AND pending_mode = ?2",
            params![id, expected],
        )?;
        Ok(changed > 0)
    }

    /// Update a synced folder's state and stamp `last_sync` to now.
    pub fn sync_folder_set_state(&self, id: i64, state: &str, last_sync: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sync_folder SET state = ?2, last_sync = ?3 WHERE id = ?1",
            params![id, state, last_sync],
        )?;
        Ok(())
    }

    /// The whole per-file sync baseline for a folder, keyed by relative path.
    pub fn sync_entries(&self, folder_id: i64) -> Result<HashMap<String, StoredSyncEntry>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT rel_path, remote_uid, local_mtime, local_size, remote_rev, remote_hash,
                    local_mtime_ns, remote_revision_id
             FROM sync_entry WHERE folder_id = ?1",
        )?;
        let rows = stmt.query_map(params![folder_id], |r| {
            Ok(StoredSyncEntry {
                rel_path: r.get(0)?,
                remote_uid: r.get(1)?,
                local_mtime: r.get(2)?,
                local_size: r.get(3)?,
                remote_rev: r.get(4)?,
                remote_hash: r.get(5)?,
                local_mtime_ns: r.get(6)?,
                remote_revision_id: r.get(7)?,
            })
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let e = row?;
            map.insert(e.rel_path.clone(), e);
        }
        Ok(map)
    }

    /// Whether `uid` is represented by a mirror location.
    ///
    /// An on-demand location proves residency through its live inode state.
    /// Mirror locations have no FUSE state, so their folder root comes from
    /// `sync_folder` and descendants come from the last successful
    /// `sync_entry` baseline.
    pub fn mirror_contains_uid(&self, uid: &str) -> Result<bool> {
        let conn = self.read()?;
        conn.query_row(
            "SELECT EXISTS(
                 SELECT 1
                   FROM sync_folder AS folder
              LEFT JOIN sync_entry AS entry ON entry.folder_id = folder.id
                  WHERE folder.mode = 'mirror'
                    AND (folder.remote_uid = ?1 OR entry.remote_uid = ?1)
             )",
            params![uid],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Insert or replace one baseline row.
    pub fn sync_entry_upsert(&self, folder_id: i64, e: &StoredSyncEntry) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_entry
               (folder_id, rel_path, remote_uid, local_mtime, local_size, remote_rev,
                remote_hash, local_mtime_ns, remote_revision_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(folder_id, rel_path) DO UPDATE SET
               remote_uid         = excluded.remote_uid,
               local_mtime        = excluded.local_mtime,
               local_size         = excluded.local_size,
               remote_rev         = excluded.remote_rev,
               remote_hash        = excluded.remote_hash,
               local_mtime_ns     = excluded.local_mtime_ns,
               remote_revision_id = excluded.remote_revision_id",
            params![
                folder_id,
                e.rel_path,
                e.remote_uid,
                e.local_mtime,
                e.local_size,
                e.remote_rev,
                e.remote_hash,
                e.local_mtime_ns,
                e.remote_revision_id,
            ],
        )?;
        Ok(())
    }

    /// How many baseline rows each synced folder has, by folder id. A folder
    /// with none is absent.
    pub fn sync_entry_counts(&self) -> Result<HashMap<i64, usize>> {
        let conn = self.read()?;
        let mut stmt =
            conn.prepare("SELECT folder_id, COUNT(*) FROM sync_entry GROUP BY folder_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut counts = HashMap::new();
        for row in rows {
            let (id, n) = row?;
            counts.insert(id, usize::try_from(n).unwrap_or(0));
        }
        Ok(counts)
    }

    /// Give baseline rows that have no revision id the one Drive reports, in
    /// one transaction. `rows` holds `(rel_path, revision_id)`. A row that has
    /// an id already keeps it. Returns how many rows changed.
    ///
    /// A remote walk calls this as it goes, so a pass cut short still keeps
    /// what it learned. Without it, every row without an id cost a key unlock
    /// on every walk until one pass got all the way to the end (B294).
    pub fn sync_entries_set_revision(
        &self,
        folder_id: i64,
        rows: &[(String, String)],
    ) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut changed = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE sync_entry SET remote_revision_id = ?3
                 WHERE folder_id = ?1 AND rel_path = ?2 AND remote_revision_id IS NULL",
            )?;
            for (rel_path, revision) in rows {
                changed += stmt.execute(params![folder_id, rel_path, revision])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// The saved walk of Drive of a mirror folder, if a pass started one
    /// that has not finished. See [`SavedWalk`].
    pub fn sync_walk_load(&self, folder_id: i64) -> Result<Option<SavedWalk>> {
        let conn = self.conn.lock();
        let Some((root_uid, started, complete)) = conn
            .query_row(
                "SELECT root_uid, started, complete FROM sync_walk WHERE folder_id = ?1",
                params![folder_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)? != 0,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut stmt = conn.prepare(
            "SELECT rel_path, uid, listed, short FROM sync_walk_dir WHERE folder_id = ?1",
        )?;
        let dirs = stmt
            .query_map(params![folder_id], |row| {
                Ok(SavedWalkDir {
                    rel_path: row.get(0)?,
                    uid: row.get(1)?,
                    listed: row.get::<_, i64>(2)? != 0,
                    short: row.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stmt = conn.prepare(
            "SELECT rel_path, uid, is_dir, mtime, size, revision, sha1
             FROM sync_walk_item WHERE folder_id = ?1",
        )?;
        let items = stmt
            .query_map(params![folder_id], |row| {
                Ok(SavedWalkItem {
                    rel_path: row.get(0)?,
                    uid: row.get(1)?,
                    is_dir: row.get::<_, i64>(2)? != 0,
                    mtime: row.get(3)?,
                    size: row.get(4)?,
                    revision: row.get(5)?,
                    sha1: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(SavedWalk {
            root_uid,
            started,
            complete,
            dirs,
            items,
        }))
    }

    /// Start a saved walk of Drive for a mirror folder, from its root,
    /// dropping any saved before.
    ///
    /// The walk notes the last dirty mark made so far: a full walk reads
    /// everything those marks name, so [`Db::sync_walk_complete`] drops them,
    /// and keeps only the marks made while the walk ran.
    pub fn sync_walk_begin(&self, folder_id: i64, root_uid: &str, now: i64) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        clear_walk(&tx, folder_id)?;
        tx.execute(
            "INSERT INTO sync_walk (folder_id, root_uid, started, seq)
             VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(seq), 0) FROM sync_remote_dirty))",
            params![folder_id, root_uid, now],
        )?;
        tx.execute(
            "INSERT INTO sync_walk_dir (folder_id, rel_path, uid) VALUES (?1, '', ?2)",
            params![folder_id, root_uid],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Save the listing of one folder of a saved walk: what it holds, and
    /// the folders in it still to be listed. One transaction, so a stop leaves
    /// the folder either listed with all it holds or not listed at all.
    ///
    /// `removed` names the paths a listing done again no longer holds, with
    /// everything under them; they go first.
    ///
    /// Does nothing once the walk has been dropped, by a change to the
    /// folder's baseline in between, for one.
    pub fn sync_walk_record(
        &self,
        folder_id: i64,
        dir: &str,
        short: bool,
        items: &[SavedWalkItem],
        removed: &[String],
    ) -> Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let listed = tx.execute(
            "UPDATE sync_walk_dir SET listed = 1, short = ?3
             WHERE folder_id = ?1 AND rel_path = ?2",
            params![folder_id, dir, short],
        )?;
        if listed == 0 {
            return Ok(false);
        }
        {
            let mut item =
                tx.prepare("DELETE FROM sync_walk_item WHERE folder_id = ?1 AND rel_path = ?2")?;
            let mut sub =
                tx.prepare("DELETE FROM sync_walk_dir WHERE folder_id = ?1 AND rel_path = ?2")?;
            for rel in removed {
                item.execute(params![folder_id, rel])?;
                sub.execute(params![folder_id, rel])?;
            }
        }
        {
            let mut item = tx.prepare(
                "INSERT OR REPLACE INTO sync_walk_item
                   (folder_id, rel_path, uid, is_dir, mtime, size, revision, sha1)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            let mut sub = tx.prepare(
                "INSERT OR IGNORE INTO sync_walk_dir (folder_id, rel_path, uid)
                 VALUES (?1, ?2, ?3)",
            )?;
            for i in items {
                item.execute(params![
                    folder_id, i.rel_path, i.uid, i.is_dir, i.mtime, i.size, i.revision, i.sha1
                ])?;
                if let (true, Some(uid)) = (i.is_dir, &i.uid) {
                    sub.execute(params![folder_id, i.rel_path, uid])?;
                }
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Mark a mirror folder's walk of Drive complete, so later passes start
    /// from it instead of walking Drive again, and drop the dirty marks the
    /// walk has read past: those made before it began. `false` when there is
    /// no walk to complete: a change to the baseline dropped it meanwhile.
    pub fn sync_walk_complete(&self, folder_id: i64) -> Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let seq: Option<i64> = tx
            .query_row(
                "SELECT seq FROM sync_walk WHERE folder_id = ?1",
                params![folder_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(seq) = seq else {
            return Ok(false);
        };
        tx.execute(
            "UPDATE sync_walk SET complete = 1 WHERE folder_id = ?1",
            params![folder_id],
        )?;
        tx.execute(
            "DELETE FROM sync_remote_dirty WHERE folder_id = ?1 AND seq <= ?2",
            params![folder_id, seq],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The folders of a mirror folder to list on Drive again, as
    /// `(seq, rel_path)`: changed on Drive since they were listed, by another
    /// device or by a pass. [`Db::sync_remote_changed`] and the baseline's
    /// triggers mark them.
    pub fn sync_remote_dirty(&self, folder_id: i64) -> Result<Vec<(i64, String)>> {
        let conn = self.read()?;
        let mut stmt =
            conn.prepare("SELECT seq, dir FROM sync_remote_dirty WHERE folder_id = ?1")?;
        let rows = stmt
            .query_map(params![folder_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop the dirty marks of a mirror folder up to `seq`, once a pass has
    /// listed what they name. A mark made since keeps its folder dirty.
    pub fn sync_remote_dirty_consume(&self, folder_id: i64, seq: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM sync_remote_dirty WHERE folder_id = ?1 AND seq <= ?2",
            params![folder_id, seq],
        )?;
        Ok(())
    }

    /// Mark folders of a mirror folder to list on Drive again.
    pub fn sync_remote_mark(&self, folder_id: i64, dirs: &[String]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut mark = tx.prepare(
                "INSERT OR REPLACE INTO sync_remote_dirty (folder_id, dir) VALUES (?1, ?2)",
            )?;
            for dir in dirs {
                mark.execute(params![folder_id, dir])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Note that Drive reported a change to `node_uid`, now in `parent_uid`:
    /// mark dirty the folder that holds it in a saved walk, and the folder it
    /// is in now. A move names only its new parent, so the old one is found
    /// through the walk. Returns the mirror folders marked.
    pub fn sync_remote_changed(
        &self,
        node_uid: &str,
        parent_uid: Option<&str>,
    ) -> Result<Vec<i64>> {
        // Looked up on a reader: most events are about nodes no walk holds,
        // and those must not queue behind the writer.
        let marked: Vec<(i64, String)> = {
            let conn = self.read()?;
            let mut marked: Vec<(i64, String)> = conn
                .prepare_cached(
                    "SELECT folder_id, rtrim(rtrim(rel_path, replace(rel_path, '/', '')), '/')
                       FROM sync_walk_item WHERE uid = ?1
                     UNION SELECT folder_id, '' FROM sync_walk WHERE root_uid = ?1",
                )?
                .query_map(params![node_uid], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if let Some(parent) = parent_uid {
                marked.extend(
                    conn.prepare_cached(
                        "SELECT folder_id, rel_path FROM sync_walk_item
                           WHERE uid = ?1 AND is_dir = 1
                         UNION SELECT folder_id, '' FROM sync_walk WHERE root_uid = ?1",
                    )?
                    .query_map(params![parent], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<(i64, String)>>>()?,
                );
            }
            marked
        };
        if marked.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut mark = tx.prepare(
                "INSERT OR REPLACE INTO sync_remote_dirty (folder_id, dir) VALUES (?1, ?2)",
            )?;
            for (folder_id, dir) in &marked {
                mark.execute(params![folder_id, dir])?;
            }
        }
        tx.commit()?;
        let mut folders: Vec<i64> = marked.into_iter().map(|(id, _)| id).collect();
        folders.sort_unstable();
        folders.dedup();
        Ok(folders)
    }

    /// Drop every saved walk of Drive and every dirty mark. Used when the
    /// event stream lost changes, so no saved picture of Drive can be
    /// trusted to be up to date.
    pub fn sync_walks_clear_all(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "DELETE FROM sync_walk; DELETE FROM sync_walk_dir; DELETE FROM sync_walk_item;
             DELETE FROM sync_remote_dirty;",
        )?;
        Ok(())
    }

    /// Drop a mirror folder's saved walk of Drive.
    pub fn sync_walk_clear(&self, folder_id: i64) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        clear_walk(&tx, folder_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Mark folders a killed daemon left in `syncing` as `error`, keeping
    /// their last sync time. Nothing else moves a folder out of `syncing`
    /// until its next pass ends, and that can be hours away (B294).
    pub fn sync_folders_reset_syncing(&self) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "UPDATE sync_folder SET state = 'error' WHERE state = 'syncing'",
            [],
        )?)
    }

    /// Drop one baseline row (a path that left the sync set on both sides).
    pub fn sync_entry_remove(&self, folder_id: i64, rel_path: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM sync_entry WHERE folder_id = ?1 AND rel_path = ?2",
            params![folder_id, rel_path],
        )?;
        Ok(())
    }

    /// Drop the baseline rows of `rel_path` and everything under it.
    ///
    /// Used when a subtree leaves a mirror folder through a move on Drive: its
    /// local copy is about to be removed, and rows left behind would read as
    /// "deleted locally" and trash the moved nodes at their new place.
    pub fn sync_entries_remove_subtree(&self, folder_id: i64, rel_path: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM sync_entry
              WHERE folder_id = ?1
                AND (rel_path = ?2 OR substr(rel_path, 1, length(?2) + 1) = ?2 || '/')",
            params![folder_id, rel_path],
        )?;
        Ok(())
    }

    /// Move the baseline rows of `from_rel` and everything under it to `to_rel`
    /// in folder `to_folder`, in one transaction.
    ///
    /// Used when a move on Drive took a mirror subtree along with its local
    /// copy: the rows still describe both sides correctly, only under a new
    /// path, so the next pass on either folder has nothing to do. The two
    /// folders may be the same one.
    pub fn sync_entries_move(
        &self,
        from_folder: i64,
        from_rel: &str,
        to_folder: i64,
        to_rel: &str,
    ) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut select = tx.prepare(
                "SELECT rel_path, remote_uid, local_mtime, local_size, remote_rev, remote_hash,
                        local_mtime_ns, remote_revision_id
                   FROM sync_entry
                  WHERE folder_id = ?1
                    AND (rel_path = ?2 OR substr(rel_path, 1, length(?2) + 1) = ?2 || '/')",
            )?;
            let rows = select
                .query_map(params![from_folder, from_rel], |r| {
                    Ok(StoredSyncEntry {
                        rel_path: r.get(0)?,
                        remote_uid: r.get(1)?,
                        local_mtime: r.get(2)?,
                        local_size: r.get(3)?,
                        remote_rev: r.get(4)?,
                        remote_hash: r.get(5)?,
                        local_mtime_ns: r.get(6)?,
                        remote_revision_id: r.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut delete =
                tx.prepare("DELETE FROM sync_entry WHERE folder_id = ?1 AND rel_path = ?2")?;
            for row in &rows {
                delete.execute(params![from_folder, row.rel_path])?;
            }
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO sync_entry
                   (folder_id, rel_path, remote_uid, local_mtime, local_size, remote_rev,
                    remote_hash, local_mtime_ns, remote_revision_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for row in rows {
                let rel = format!("{to_rel}{}", &row.rel_path[from_rel.len()..]);
                insert.execute(params![
                    to_folder,
                    rel,
                    row.remote_uid,
                    row.local_mtime,
                    row.local_size,
                    row.remote_rev,
                    row.remote_hash,
                    row.local_mtime_ns,
                    row.remote_revision_id,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The deletion window of a mirror folder, if one is open.
    pub fn sync_delete_window(&self, folder_id: i64) -> Result<Option<DeleteWindow>> {
        let conn = self.read()?;
        conn.query_row(
            "SELECT started_at, reference, deleted FROM sync_delete_window WHERE folder_id = ?1",
            params![folder_id],
            |row| {
                Ok(DeleteWindow {
                    started_at: row.get(0)?,
                    reference: row.get::<_, i64>(1)?.max(0) as u64,
                    deleted: row.get::<_, i64>(2)?.max(0) as u64,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Record the deletion window of a mirror folder.
    pub fn sync_delete_window_set(&self, folder_id: i64, window: &DeleteWindow) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_delete_window (folder_id, started_at, reference, deleted)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(folder_id) DO UPDATE SET started_at = excluded.started_at,
               reference = excluded.reference, deleted = excluded.deleted",
            params![
                folder_id,
                window.started_at,
                i64::try_from(window.reference).unwrap_or(i64::MAX),
                i64::try_from(window.deleted).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    /// Drop the entire baseline for a folder. Used when flipping ondemand→mirror:
    /// the local tree was evicted, so the old baseline is stale and would make the
    /// next reconcile mistake "locally deleted" for "must re-download". Clearing it
    /// leaves an empty baseline + full remote = pure download.
    pub fn sync_entries_clear(&self, folder_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM sync_entry WHERE folder_id = ?1",
            params![folder_id],
        )?;
        Ok(())
    }
}

fn clear_walk(tx: &rusqlite::Transaction<'_>, folder_id: i64) -> Result<()> {
    for table in [
        "sync_walk",
        "sync_walk_dir",
        "sync_walk_item",
        "sync_remote_dirty",
    ] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE folder_id = ?1"),
            params![folder_id],
        )?;
    }
    Ok(())
}
