//! The port through which the mirror engine reads and writes its own tables.
//!
//! The engine keeps four things in the database: the synced folders, the
//! per-file baseline of each, the saved walk of Drive with its dirty marks,
//! and the deletions each folder made lately.
//! [`SyncStore`] is exactly the calls a pass makes on them, so the engine does
//! not depend on the whole database, and a test can give it a store of its own.
//!
//! The daemon's database implements it below; every method forwards to the
//! [`Db`] method of the same name, which documents what it does.

use std::collections::HashMap;

use pdfs_core::Result;
use pdfs_store::db::{
    Db, DeleteWindow, SavedWalk, SavedWalkItem, StoredIntent, StoredSyncEntry, StoredSyncFolder,
};

/// The sync tables, as the mirror engine uses them.
pub trait SyncStore: Send + Sync {
    // Synced folders.

    /// Every synced folder, oldest first.
    fn sync_folder_list(&self) -> Result<Vec<StoredSyncFolder>>;
    /// One synced folder by id.
    fn sync_folder_get(&self, id: i64) -> Result<Option<StoredSyncFolder>>;
    /// Set a synced folder's state and its last sync time.
    fn sync_folder_set_state(&self, id: i64, state: &str, last_sync: i64) -> Result<()>;
    /// Move every folder a killed daemon left in `syncing` to `error`.
    fn sync_folders_reset_syncing(&self) -> Result<usize>;

    // The baseline.

    /// A synced folder's whole baseline, keyed by relative path.
    fn sync_entries(&self, folder_id: i64) -> Result<HashMap<String, StoredSyncEntry>>;
    /// How many baseline rows each synced folder has.
    fn sync_entry_counts(&self) -> Result<HashMap<i64, usize>>;
    /// Insert or replace one baseline row.
    fn sync_entry_upsert(&self, folder_id: i64, e: &StoredSyncEntry) -> Result<()>;
    /// Drop one baseline row.
    fn sync_entry_remove(&self, folder_id: i64, rel_path: &str) -> Result<()>;
    /// Drop the baseline rows at `rel_path` and below it.
    fn sync_entries_remove_subtree(&self, folder_id: i64, rel_path: &str) -> Result<()>;
    /// Fill in the revision id of baseline rows that have none, as
    /// `(rel_path, revision_id)`. Returns how many rows changed.
    fn sync_entries_set_revision(&self, folder_id: i64, rows: &[(String, String)])
    -> Result<usize>;

    // The saved walk of Drive, and the folders changed on Drive since.

    /// The walk of Drive a pass started and did not finish, if any.
    fn sync_walk_load(&self, folder_id: i64) -> Result<Option<SavedWalk>>;
    /// Start a new walk of Drive from the root, dropping any saved one.
    fn sync_walk_begin(&self, folder_id: i64, root_uid: &str, now: i64) -> Result<()>;
    /// Save the listing of one folder of the walk. `false` when the walk was
    /// dropped in the meantime.
    fn sync_walk_record(
        &self,
        folder_id: i64,
        dir: &str,
        short: bool,
        items: &[SavedWalkItem],
        removed: &[String],
    ) -> Result<bool>;
    /// Mark the walk complete and drop the dirty marks it has read past.
    /// `false` when there is no walk to complete.
    fn sync_walk_complete(&self, folder_id: i64) -> Result<bool>;
    /// Drop a folder's saved walk.
    fn sync_walk_clear(&self, folder_id: i64) -> Result<()>;
    /// The folders to list on Drive again, as `(seq, rel_path)`.
    fn sync_remote_dirty(&self, folder_id: i64) -> Result<Vec<(i64, String)>>;
    /// Drop the dirty marks up to and including `seq`.
    fn sync_remote_dirty_consume(&self, folder_id: i64, seq: i64) -> Result<()>;
    /// Mark folders to list on Drive again.
    fn sync_remote_mark(&self, folder_id: i64, dirs: &[String]) -> Result<()>;

    // The deletions made lately, for the wipe guard.

    /// The folder's deletion window, if one is open.
    fn sync_delete_window(&self, folder_id: i64) -> Result<Option<DeleteWindow>>;
    /// Record the folder's deletion window.
    fn sync_delete_window_set(&self, folder_id: i64, window: &DeleteWindow) -> Result<()>;

    // The intent of each effect sent (`effect`).

    /// Record that an effect is about to be sent; returns the intent and the
    /// state it was in before, if any.
    fn sync_intent_issue(
        &self,
        folder_id: i64,
        rel_path: &str,
        kind: &str,
        now: i64,
    ) -> Result<(StoredIntent, Option<String>)>;
    /// Drop a settled intent.
    fn sync_intent_settle(&self, id: i64) -> Result<()>;
    /// Move an intent to `state`, with the error that put it there.
    fn sync_intent_set(&self, id: i64, state: &str, error: Option<&str>) -> Result<()>;
    /// Note that upload `kind` of `rel_path` streamed bytes with SHA-1 `sha1`
    /// and heard no success back.
    fn sync_intent_sent(
        &self,
        folder_id: i64,
        rel_path: &str,
        kind: &str,
        sha1: &str,
    ) -> Result<()>;
    /// Every intent not settled; only `folder_id`'s when given.
    fn sync_intents(&self, folder_id: Option<i64>) -> Result<Vec<StoredIntent>>;
    /// Move every intent in state `from` to `to`.
    fn sync_intents_move(&self, from: &str, to: &str) -> Result<usize>;
    /// Settle the intents of `folder_id` in one of `states` last sent before
    /// `before`.
    fn sync_intents_settle_before(
        &self,
        folder_id: i64,
        states: &[&str],
        before: i64,
    ) -> Result<Vec<StoredIntent>>;
}

impl SyncStore for Db {
    fn sync_folder_list(&self) -> Result<Vec<StoredSyncFolder>> {
        Db::sync_folder_list(self)
    }
    fn sync_folder_get(&self, id: i64) -> Result<Option<StoredSyncFolder>> {
        Db::sync_folder_get(self, id)
    }
    fn sync_folder_set_state(&self, id: i64, state: &str, last_sync: i64) -> Result<()> {
        Db::sync_folder_set_state(self, id, state, last_sync)
    }
    fn sync_folders_reset_syncing(&self) -> Result<usize> {
        Db::sync_folders_reset_syncing(self)
    }
    fn sync_entries(&self, folder_id: i64) -> Result<HashMap<String, StoredSyncEntry>> {
        Db::sync_entries(self, folder_id)
    }
    fn sync_entry_counts(&self) -> Result<HashMap<i64, usize>> {
        Db::sync_entry_counts(self)
    }
    fn sync_entry_upsert(&self, folder_id: i64, e: &StoredSyncEntry) -> Result<()> {
        Db::sync_entry_upsert(self, folder_id, e)
    }
    fn sync_entry_remove(&self, folder_id: i64, rel_path: &str) -> Result<()> {
        Db::sync_entry_remove(self, folder_id, rel_path)
    }
    fn sync_entries_remove_subtree(&self, folder_id: i64, rel_path: &str) -> Result<()> {
        Db::sync_entries_remove_subtree(self, folder_id, rel_path)
    }
    fn sync_entries_set_revision(
        &self,
        folder_id: i64,
        rows: &[(String, String)],
    ) -> Result<usize> {
        Db::sync_entries_set_revision(self, folder_id, rows)
    }
    fn sync_walk_load(&self, folder_id: i64) -> Result<Option<SavedWalk>> {
        Db::sync_walk_load(self, folder_id)
    }
    fn sync_walk_begin(&self, folder_id: i64, root_uid: &str, now: i64) -> Result<()> {
        Db::sync_walk_begin(self, folder_id, root_uid, now)
    }
    fn sync_walk_record(
        &self,
        folder_id: i64,
        dir: &str,
        short: bool,
        items: &[SavedWalkItem],
        removed: &[String],
    ) -> Result<bool> {
        Db::sync_walk_record(self, folder_id, dir, short, items, removed)
    }
    fn sync_walk_complete(&self, folder_id: i64) -> Result<bool> {
        Db::sync_walk_complete(self, folder_id)
    }
    fn sync_walk_clear(&self, folder_id: i64) -> Result<()> {
        Db::sync_walk_clear(self, folder_id)
    }
    fn sync_remote_dirty(&self, folder_id: i64) -> Result<Vec<(i64, String)>> {
        Db::sync_remote_dirty(self, folder_id)
    }
    fn sync_remote_dirty_consume(&self, folder_id: i64, seq: i64) -> Result<()> {
        Db::sync_remote_dirty_consume(self, folder_id, seq)
    }
    fn sync_remote_mark(&self, folder_id: i64, dirs: &[String]) -> Result<()> {
        Db::sync_remote_mark(self, folder_id, dirs)
    }
    fn sync_delete_window(&self, folder_id: i64) -> Result<Option<DeleteWindow>> {
        Db::sync_delete_window(self, folder_id)
    }
    fn sync_delete_window_set(&self, folder_id: i64, window: &DeleteWindow) -> Result<()> {
        Db::sync_delete_window_set(self, folder_id, window)
    }
    fn sync_intent_issue(
        &self,
        folder_id: i64,
        rel_path: &str,
        kind: &str,
        now: i64,
    ) -> Result<(StoredIntent, Option<String>)> {
        Db::sync_intent_issue(self, folder_id, rel_path, kind, now)
    }
    fn sync_intent_settle(&self, id: i64) -> Result<()> {
        Db::sync_intent_settle(self, id)
    }
    fn sync_intent_set(&self, id: i64, state: &str, error: Option<&str>) -> Result<()> {
        Db::sync_intent_set(self, id, state, error)
    }
    fn sync_intent_sent(
        &self,
        folder_id: i64,
        rel_path: &str,
        kind: &str,
        sha1: &str,
    ) -> Result<()> {
        Db::sync_intent_sent(self, folder_id, rel_path, kind, sha1)
    }
    fn sync_intents(&self, folder_id: Option<i64>) -> Result<Vec<StoredIntent>> {
        Db::sync_intents(self, folder_id)
    }
    fn sync_intents_move(&self, from: &str, to: &str) -> Result<usize> {
        Db::sync_intents_move(self, from, to)
    }
    fn sync_intents_settle_before(
        &self,
        folder_id: i64,
        states: &[&str],
        before: i64,
    ) -> Result<Vec<StoredIntent>> {
        Db::sync_intents_settle_before(self, folder_id, states, before)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn entry(rel_path: &str) -> StoredSyncEntry {
        StoredSyncEntry {
            rel_path: rel_path.into(),
            remote_uid: Some(format!("vol~{rel_path}")),
            local_mtime: 5,
            local_mtime_ns: None,
            local_size: 6,
            remote_rev: Some("7".into()),
            remote_hash: Some("8".into()),
            remote_revision_id: None,
        }
    }

    #[test]
    fn the_database_serves_the_engine_through_the_port() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let id = db
            .sync_folder_add("/home/me/Docs", "vol~docs", "share")
            .unwrap();
        let store: Arc<dyn SyncStore> = Arc::new(db);

        assert_eq!(store.sync_folder_list().unwrap().len(), 1);
        store.sync_folder_set_state(id, "syncing", 9).unwrap();
        assert_eq!(store.sync_folders_reset_syncing().unwrap(), 1);
        let folder = store.sync_folder_get(id).unwrap().unwrap();
        assert_eq!((folder.state.as_str(), folder.last_sync), ("error", 9));

        for rel in ["a.txt", "d", "d/b.txt"] {
            store.sync_entry_upsert(id, &entry(rel)).unwrap();
        }
        let set = [("a.txt".to_string(), "rev-a".to_string())];
        assert_eq!(store.sync_entries_set_revision(id, &set).unwrap(), 1);
        store.sync_entries_remove_subtree(id, "d").unwrap();
        let rows = store.sync_entries(id).unwrap();
        assert_eq!(rows.keys().collect::<Vec<_>>(), ["a.txt"]);
        assert_eq!(rows["a.txt"].remote_revision_id.as_deref(), Some("rev-a"));
        store.sync_entry_remove(id, "a.txt").unwrap();
        assert_eq!(store.sync_entry_counts().unwrap().get(&id), None);

        assert!(store.sync_walk_load(id).unwrap().is_none());
        store.sync_walk_begin(id, "vol~docs", 10).unwrap();
        assert!(store.sync_walk_load(id).unwrap().is_some());
        store.sync_walk_clear(id).unwrap();
        assert!(store.sync_walk_load(id).unwrap().is_none());
        assert!(!store.sync_walk_complete(id).unwrap());

        store.sync_remote_mark(id, &["d".to_string()]).unwrap();
        let dirty = store.sync_remote_dirty(id).unwrap();
        assert_eq!(
            dirty.iter().map(|(_, p)| p.as_str()).collect::<Vec<_>>(),
            ["d"]
        );
        store.sync_remote_dirty_consume(id, dirty[0].0).unwrap();
        assert!(store.sync_remote_dirty(id).unwrap().is_empty());

        let (intent, _) = store.sync_intent_issue(id, "a.txt", "upload", 1).unwrap();
        store.sync_intent_set(intent.id, "retry", None).unwrap();
        assert_eq!(store.sync_intents(Some(id)).unwrap()[0].state, "retry");
        assert_eq!(store.sync_intents_move("retry", "blocked").unwrap(), 1);
        assert_eq!(
            store
                .sync_intents_settle_before(id, &["blocked"], 2)
                .unwrap()
                .len(),
            1
        );
        let (intent, _) = store.sync_intent_issue(id, "a.txt", "upload", 3).unwrap();
        store.sync_intent_settle(intent.id).unwrap();
        assert!(store.sync_intents(None).unwrap().is_empty());
    }
}
