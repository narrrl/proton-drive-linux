//! Remote-event invalidation and local-index background workers.

use super::*;
use crate::shutdown::Shutdown;

/// Drop `uid` from one mount's tree and tell that mount's kernel session the
/// entry is gone. Shared by the trash and delete paths, which differ only in
/// which event carried them.
///
/// Returns whether this mount actually held the node, so the caller can evict
/// its content blob once if *any* mount did.
fn forget_and_notify(st: &mut State, notify: &mut NotifyBatch, uid: &NodeUid) -> bool {
    // Capture the inode before `forget` clears the uid mapping.
    let child = st.by_uid.get(uid).copied();
    let Some((parent, name)) = st.forget(uid) else {
        return false;
    };
    match child {
        Some(child) => notify.delete(parent, child, name),
        None => notify.inval_entry(parent, name),
    }
    true
}

fn hide_foreign_deleted_and_notify(st: &mut State, notify: &mut NotifyBatch, uid: &NodeUid) {
    let child = st.by_uid.get(uid).copied();
    let dentry = child.and_then(|ino| {
        st.entries
            .get(&ino)
            .map(|entry| (entry.parent, entry.node.name.clone()))
    });
    let changed = st.downgrade_shared_subtree(uid);
    st.hide_foreign_subtree(uid);
    notify.extend_inodes(&changed);
    if let (Some((parent, name)), Some(child)) = (dentry, child) {
        notify.delete(parent, child, name);
    }
}

/// Persist the fail-closed part of an access event and install the same denial
/// in resident state even when SQLite rejects the write. The caller must not
/// acknowledge the event when this returns an error.
fn apply_access_downgrade(
    db: &Db,
    event: &DriveEvent,
    mut deny: impl FnMut(Option<&NodeUid>),
) -> pdfs_core::Result<()> {
    match event {
        DriveEvent::NodeDeleted { node_uid, .. } => {
            let result = db.set_share_access(node_uid, Access::Viewer);
            deny(Some(node_uid));
            result
        }
        DriveEvent::ContinuityLost { .. }
        | DriveEvent::ScopeAccessLost { .. }
        | DriveEvent::SharedWithMeUpdated { .. } => {
            let result = db.downgrade_all_share_access().map(|_| ());
            deny(None);
            result
        }
        _ => Ok(()),
    }
}

fn apply_foreign_delete(
    db: &Db,
    uid: &NodeUid,
    mut deny_and_hide: impl FnMut(&NodeUid),
) -> pdfs_core::Result<()> {
    let result = db.tombstone_foreign_subtree(uid);
    deny_and_hide(uid);
    result
}

fn is_foreign_node_delete(event: &DriveEvent, own_volume: &VolumeId) -> bool {
    match event {
        DriveEvent::NodeDeleted { node_uid, .. } => {
            !is_own_or_virtual_uid(node_uid, own_volume) && !is_local_uid(node_uid)
        }
        _ => false,
    }
}

fn event_serializes_shared_publication(event: &DriveEvent, own_volume: &VolumeId) -> bool {
    matches!(
        event,
        DriveEvent::ContinuityLost { .. }
            | DriveEvent::ScopeAccessLost { .. }
            | DriveEvent::SharedWithMeUpdated { .. }
    ) || is_foreign_node_delete(event, own_volume)
}

/// Apply one remote event to the local cache and notify the kernel so it drops
/// any stale cached metadata/data for the affected inodes.
///
/// The cache is authoritative-by-absence: dropping a directory's `children`
/// entry forces the next `lookup`/`readdir` to re-enumerate from the remote, so
/// most events only need to invalidate listings rather than re-fetch eagerly.
///
/// Applied to **every** mounted inode space, not just the primary one. This task
/// is per-daemon while node state is per-mount, and a `DriveEvent` names a uid,
/// not a mount — so a file trashed from another device has to be withdrawn from
/// whichever of our sessions is showing it, which for anything under a sync
/// folder is a [`Core::fork_state`] fork rather than `core.state`. Reaching for
/// the primary state alone left forks serving a deleted file indefinitely, the
/// read-side twin of `docs/BUGS.md` B74. Inode numbers are per-mount, so each
/// mount must be notified through its **own** channel; that pairing is what
/// [`Core::for_each_mount`] exists to preserve.
fn apply_event(core: &Core, event: &DriveEvent, dirty: &mut DirtyParents) -> pdfs_core::Result<()> {
    let foreign_delete = is_foreign_node_delete(event, &core.primary_root_uid.volume_id);
    let serializes_shared_publication =
        event_serializes_shared_publication(event, &core.primary_root_uid.volume_id);
    let _publication = serializes_shared_publication.then(|| core.shared_publication.lock());
    if serializes_shared_publication {
        core.invalidate_shared_refreshes();
    }
    let mut apply_result = if let DriveEvent::NodeDeleted { node_uid, .. } = event
        && foreign_delete
    {
        apply_foreign_delete(&core.db, node_uid, |uid| {
            core.for_each_mount(|st, notify| {
                hide_foreign_deleted_and_notify(st, notify, uid);
            });
        })
    } else {
        apply_access_downgrade(&core.db, event, |root| {
            core.for_each_mount(|st, notify| {
                let changed = match root {
                    Some(uid) => st.downgrade_shared_subtree(uid),
                    None => st.downgrade_known_shared_access(),
                };
                notify.extend_inodes(&changed);
            });
        })
    };
    match event {
        DriveEvent::NodeUpdated {
            node_uid,
            parent_node_uid,
            is_trashed,
            ..
        } => {
            // A node we owe an upload for is *ahead* of the remote, not behind
            // it: this event is almost always the echo of our own empty-file
            // create, and re-fetching would replace the size and mtime of the
            // write we just accepted with the stale revision's — making a file
            // that was copied in seconds ago read as empty until its upload
            // lands (offline.md Phase 3).
            //
            // A write that has already drained is the same story one step later:
            // the drain brought the tree level with the revision it sealed, so
            // the feed's report of that revision has nothing to add, and acting
            // on it evicts the very bytes we uploaded. Claimed unconditionally,
            // even for a trash, so an echo we no longer care about does not
            // linger to suppress a later foreign change.
            let echo = core.take_self_change(node_uid);
            let ours = !*is_trashed && (echo || core.pending.lock().contains_key(node_uid));
            if ours {
                debug!(uid = %node_uid, "ignoring remote event for a node with a queued write");
            }
            let mut had_node = false;
            core.for_each_mount(|st, notify| {
                if *is_trashed {
                    // Trashing makes a node vanish from its parent listing.
                    had_node |= forget_and_notify(st, notify, node_uid);
                } else if !ours && let Some(&ino) = st.by_uid.get(node_uid) {
                    // Known node changed: drop its cached attrs/data (and
                    // listing if it is a directory) so the next access
                    // re-fetches. Its content blob may now be stale too.
                    had_node = true;
                    st.invalidate_listing(ino);
                    notify.inval_inode(ino);
                }
            });
            // Once, not once per mount: the content cache is per-daemon.
            if had_node {
                core.cache.evict(node_uid);
            }
            // A create, rename or move-in shows up as a change to the parent
            // listing too, which is recorded for the end of the batch rather
            // than acted on here — see [`DirtyParents`].
            if let Some(parent_uid) = parent_node_uid {
                match ours {
                    true => dirty
                        .ours
                        .entry(parent_uid.clone())
                        .or_default()
                        .insert(node_uid.clone()),
                    false => dirty.foreign.insert(parent_uid.clone()),
                };
            }
        }
        DriveEvent::NodeDeleted { node_uid, .. } => {
            core.cache.evict(node_uid);
            if !foreign_delete {
                core.for_each_mount(|st, notify| {
                    forget_and_notify(st, notify, node_uid);
                });
            }
        }
        // Losing event continuity or any shared-access signal makes persisted
        // shared roots untrustworthy. Keep owned/device trees unchanged, but
        // fail every known shared subtree closed until a root refresh carries a
        // current membership role.
        DriveEvent::ContinuityLost { .. }
        | DriveEvent::ScopeAccessLost { .. }
        | DriveEvent::SharedWithMeUpdated { .. } => {
            warn!("event access continuity lost; downgrading shared trees and resyncing lazily");
            apply_result = apply_result.and(core.db.clear_state(SHARED_WITH_ME_SYNCED_MS));
            core.for_each_mount(|st, notify| {
                let dirs: Vec<u64> = st.children.keys().copied().collect();
                for &ino in &dirs {
                    st.invalidate_listing(ino);
                    notify.inval_inode(ino);
                }
            });
        }
        // No substantive local change; the cursor advance is handled by the
        // caller persisting the event id.
        DriveEvent::CursorAdvanced { .. } => {}
    }
    apply_result
}

fn acknowledge_applied_event(
    db: &Db,
    event: &DriveEvent,
    applied: pdfs_core::Result<()>,
) -> pdfs_core::Result<DriveEventId> {
    applied?;
    let id = event.id().clone();
    db.set_event_cursor(id.as_str())?;
    Ok(id)
}

/// The folders one batch of events changed, collected so the listings are
/// dropped once at the end of the batch rather than once per event.
///
/// Both halves of that matter. A burst of uploads produces one event per file
/// all naming the *same* folder, and dropping the listing per event made every
/// interleaved lookup re-enumerate that folder from the API — quadratic in its
/// size, and slow enough to stall a busy mount. Splitting foreign changes from
/// our own then removes the re-enumeration entirely for the common case: a file
/// this daemon created is already in the tree under the right parent, so there
/// is nothing for a re-enumeration to discover.
///
/// Neither is a weakening. The state after the batch is what it always was.
#[derive(Default)]
struct DirtyParents {
    /// Folders changed by someone else. Their listings must go: an event names
    /// no name, so a rename inside a folder is indistinguishable from a write to
    /// a file in it, and only re-enumerating can tell us which happened.
    foreign: HashSet<NodeUid>,
    /// Folders changed by this daemon, and the children the changes were about.
    /// Checked per mount — a mount that already lists every one of those
    /// children under that parent keeps its listing.
    ours: HashMap<NodeUid, HashSet<NodeUid>>,
}

/// Whether this mount's cached listing of `parent_uid` already accounts for
/// `child_uid`, so a change we made to that child leaves nothing to re-read.
///
/// "Nothing cached" counts as current: an absent listing is re-enumerated on the
/// next `readdir` regardless, so there is nothing to drop.
fn listing_accounts_for(st: &State, parent_uid: &NodeUid, child_uid: &NodeUid) -> bool {
    let Some(&parent) = st.by_uid.get(parent_uid) else {
        return true;
    };
    let Some(children) = st.children.get(&parent) else {
        return true;
    };
    let Some(&child) = st.by_uid.get(child_uid) else {
        return false;
    };
    children.contains(&child) && st.entries.get(&child).is_some_and(|e| e.parent == parent)
}

/// Drop the cached listing of every folder a batch of events changed, in every
/// mount that holds one, so the next `readdir`/`lookup` re-enumerates it.
///
/// Runs strictly after the whole batch is applied: doing it per event would
/// leave a window where a concurrent `readdir` re-populates a listing that a
/// later event in the same batch then has to drop again.
fn flush_dirty_parents(core: &Core, dirty: &DirtyParents) {
    if dirty.foreign.is_empty() && dirty.ours.is_empty() {
        return;
    }
    fn drop_listing(st: &mut State, notify: &mut NotifyBatch, parent_uid: &NodeUid) {
        let Some(&parent) = st.by_uid.get(parent_uid) else {
            return;
        };
        st.invalidate_listing(parent);
        notify.inval_inode(parent);
    }
    core.for_each_mount(|st, notify| {
        for parent_uid in &dirty.foreign {
            drop_listing(st, notify, parent_uid);
        }
        for (parent_uid, children) in &dirty.ours {
            if dirty.foreign.contains(parent_uid) {
                continue;
            }
            // One child this mount has not placed is enough: the listing it is
            // serving is missing something the user just made.
            if children
                .iter()
                .all(|child| listing_accounts_for(st, parent_uid, child))
            {
                continue;
            }
            drop_listing(st, notify, parent_uid);
        }
    });
}

/// Poll the remote event cursor forever, applying each batch to the shared
/// state. Resumes from the cursor persisted in the DB so changes made while
/// unmounted are applied; only a first-ever mount seeds from the server head.
/// The cursor is persisted after every batch. Runs as a Tokio task; returns
/// only on fatal error.
///
/// Takes the whole `Core` rather than the primary mount's pieces: an event has
/// to be applied to every mounted inode space, and the registry that enumerates
/// them lives on the `Core` (see [`apply_event`]).
pub(super) async fn run_event_sync(
    client: ProtonDriveClient,
    scope: DriveEventScopeId,
    core: Core,
) {
    let db = core.db.clone();
    let mut cursor: Option<DriveEventId> = match db.get_event_cursor() {
        // Resume: pick up exactly where the last run left off.
        Ok(Some(saved)) => Some(DriveEventId::from(saved)),
        // First mount: a `None` cursor yields a single `CursorAdvanced` at the
        // server head; persist it so the next restart resumes instead of
        // reseeding (which would skip everything that changed offline).
        // Seeding needs the network, and this task also runs on mounts that
        // started offline (offline.md Phase 1) — so retry rather than giving up,
        // which used to disable live sync for the life of the daemon.
        Ok(None) => {
            // Nothing says what changed before this cursor, so a persisted SDK
            // entity cache from an earlier run cannot be trusted: drop it rather
            // than serve metadata no event will ever invalidate.
            if let Some(cache) = pdfs_core::sdkcache::SdkCache::opened()
                && let Err(e) = cache.clear_now()
            {
                warn!(error = %e, "clearing the persisted SDK entity cache failed");
            }
            let mut delay = ONLINE_PROBE_MIN;
            loop {
                match client.enumerate_events(&scope, None).await {
                    Ok(events) => {
                        let head = events.last().map(|e| e.id().clone());
                        let Some(c) = &head else {
                            break None;
                        };
                        if let Err(e) = db.set_event_cursor(c.as_str()) {
                            warn!(error = %e, ?delay, "persist seed cursor failed; retrying");
                            tokio::time::sleep(delay).await;
                            delay = (delay * 2).min(ONLINE_PROBE_MAX);
                            continue;
                        }
                        break Some(c.clone());
                    }
                    Err(e) => {
                        warn!(error = %e, ?delay, "seed event cursor failed; retrying");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(ONLINE_PROBE_MAX);
                    }
                }
            }
        }
        Err(e) => {
            error!(error = %e, "read persisted cursor failed; live sync disabled");
            return;
        }
    };
    info!(?cursor, "event sync started");

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let events = match client.enumerate_events(&scope, cursor.as_ref()).await {
            Ok(events) => events,
            Err(e) => {
                warn!(error = %e, "event poll failed; retrying after interval");
                continue;
            }
        };
        if events.is_empty() {
            continue;
        }
        debug!(count = events.len(), "applying remote events");
        // Folders whose listing this batch invalidates, applied once at the end
        // rather than per event — see [`DirtyParents`]. Every `break` below falls
        // through to that flush, so an interrupted batch still publishes what it
        // did apply.
        let mut dirty = DirtyParents::default();
        for event in &events {
            // Converge the SDK's own caches (folder keys, entity cache) on the
            // server before applying the event to our tree. Without this, a node
            // re-keyed/moved by another client keeps a stale key in the SDK for
            // the life of the daemon (SDK plan #9). `apply_event` only touches
            // our FUSE state, so nothing else does this.
            if let Err(e) = client.invalidate_caches_for_event(event).await {
                warn!(error = %e, "sdk cache invalidation for event failed; retaining cursor for retry");
                break;
            }
            let applied = match acknowledge_applied_event(
                &db,
                event,
                apply_event(&core, event, &mut dirty),
            ) {
                Ok(applied) => applied,
                Err(e) => {
                    warn!(error = %e, "event application or acknowledgment failed; retaining prior cursor for retry");
                    break;
                }
            };
            // Advance only after this exact event is durably acknowledged. If a
            // later event fails, polling resumes from here and replays nothing
            // that was not already made visible locally.
            cursor = Some(applied);
        }
        flush_dirty_parents(&core, &dirty);
    }
}

/// Keep the local-file index fresh for the launcher prompt's "This computer"
/// results. Rebuilds the index whenever it is older than [`LOCAL_INDEX_TTL`],
/// then sleeps; runs on its own thread for the life of the daemon.
///
/// The walk is the one part of the daemon that touches the wider filesystem, so
/// it is deliberately kept off every hot path: it never runs on a FUSE or
/// control-socket thread, and it excludes the mountpoint (walking it would fault
/// every remote node in through FUSE, defeating on-demand hydration).
pub(super) fn run_local_index(
    db: Arc<Db>,
    indexing: Arc<AtomicBool>,
    transfers: Arc<TransferRegistry>,
    mountpoint: PathBuf,
    shutdown: Arc<Shutdown>,
) {
    loop {
        let age = db.local_indexed_at().ok().flatten();
        let stale =
            age.is_none_or(|at| now_secs().saturating_sub(at) >= LOCAL_INDEX_TTL.as_secs() as i64);
        if stale {
            scan_local_once(&db, &indexing, &transfers, &mountpoint);
        }
        // Interruptible, so teardown joins this thread instead of leaving it to
        // keep rewriting the index for a mount that is gone (bugs.md B44).
        if !shutdown.sleep(LOCAL_INDEX_CHECK) {
            return;
        }
    }
}

/// Walk `$HOME` once and replace the local-file index with what it finds.
/// Batches stream straight into SQLite, so peak memory is one batch — not the
/// whole home directory.
///
/// Reports itself as a job: the first scan after a fresh install walks the whole
/// home directory, and `indexing` alone only tells the launcher prompt to say
/// "still indexing" — nothing else showed that the daemon was busy.
fn scan_local_once(
    db: &Db,
    indexing: &AtomicBool,
    transfers: &Arc<TransferRegistry>,
    mountpoint: &Path,
) {
    let dirs = match AppDirs::new() {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "local index: cannot resolve app dirs");
            return;
        }
    };
    let Some(home) = dirs.home_dir() else {
        warn!("local index: cannot resolve home directory");
        return;
    };
    let generation = match db.local_begin_scan() {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "local index: cannot open scan generation");
            return;
        }
    };

    let mut excludes =
        localindex::default_excludes(mountpoint, &dirs.state_dir(), &dirs.cache_dir());
    // Resolved per scan rather than once at startup: mounts come and go, and a
    // mount that appeared since the daemon started is exactly the one most
    // likely to be a half-alive network filesystem (see `nested_mount_points`).
    excludes.extend(localindex::nested_mount_points(&home));
    indexing.store(true, Ordering::Relaxed);
    let started = Instant::now();

    // The walk has no idea how many files it will find, so the job counts what it
    // has seen and stays indeterminate.
    let job = transfers.begin_job("Indexing this computer");
    job.detail("Scanning your files");
    let walked = localindex::scan(&[home], &excludes, |batch| {
        if let Err(e) = db.local_upsert_batch(generation, &batch) {
            warn!(error = %e, "local index: batch write failed");
        }
    });

    // Prune what this scan did not see and rebuild the FTS index over the rest,
    // even if some batches failed — a partial index still beats none.
    job.detail("Building the search index");
    match db.local_finish_scan(generation, now_secs()) {
        Ok(indexed) => info!(
            walked,
            indexed,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "local index rebuilt"
        ),
        Err(e) => warn!(error = %e, "local index: finish failed"),
    }
    indexing.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn folder(volume: &str, link: &str, parent: Option<NodeUid>, name: &str) -> Node {
        Node {
            uid: NodeUid::new(VolumeId::from(volume), LinkId::from(link)),
            parent_uid: parent,
            kind: NodeKind::Folder,
            name: name.to_string(),
            creation_time: 1,
            modification_time: 1,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            membership: None,
            photo: None,
            album: None,
            verification: Default::default(),
            direct_role: None,
            share_id: None,
        }
    }

    #[test]
    fn foreign_delete_invalidates_an_earlier_refresh_generation() {
        let own_volume = VolumeId::from("own");
        let event = DriveEvent::NodeDeleted {
            id: DriveEventId::from("foreign-delete"),
            node_uid: NodeUid::new(VolumeId::from("foreign"), LinkId::from("child")),
            parent_node_uid: None,
        };
        let generation = AtomicU64::new(7);
        let captured = generation.load(Ordering::SeqCst);

        assert!(event_serializes_shared_publication(&event, &own_volume));
        generation.fetch_add(1, Ordering::SeqCst);

        assert!(!refresh_generation_is_current(
            captured,
            generation.load(Ordering::SeqCst)
        ));
        let own_delete = DriveEvent::NodeDeleted {
            id: DriveEventId::from("own-delete"),
            node_uid: NodeUid::new(own_volume.clone(), LinkId::from("child")),
            parent_node_uid: None,
        };
        assert!(!event_serializes_shared_publication(
            &own_delete,
            &own_volume
        ));
    }

    #[test]
    fn foreign_delete_tombstones_subtree_and_advances_cursor_after_success() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let root = folder("own", "root", None, "My Files");
        let deleted = folder(
            "foreign",
            "deleted",
            Some(root.uid.clone()),
            "Deleted foreign",
        );
        let deep = folder(
            "foreign",
            "deep",
            Some(deleted.uid.clone()),
            "Searchable descendant",
        );
        db.upsert_nodes(&[root, deleted.clone(), deep.clone()])
            .unwrap();
        db.set_share_access(&deleted.uid, Access::Editor).unwrap();
        db.enqueue_op(&PendingOp {
            id: 0,
            kind: OP_RENAME.to_string(),
            uid: deep.uid.to_string(),
            parent_uid: Some(deleted.uid.to_string()),
            name: Some("pending".to_string()),
            blob_path: None,
            meta_json: Some("{}".to_string()),
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        })
        .unwrap();
        let event = DriveEvent::NodeDeleted {
            id: DriveEventId::from("foreign-delete-success"),
            node_uid: deleted.uid.clone(),
            parent_node_uid: deleted.parent_uid.clone(),
        };
        let denied = Cell::new(false);

        let applied = apply_foreign_delete(&db, &deleted.uid, |_| denied.set(true));
        let acknowledged = acknowledge_applied_event(&db, &event, applied).unwrap();

        assert!(denied.get());
        assert_eq!(acknowledged.as_str(), "foreign-delete-success");
        assert_eq!(
            db.get_event_cursor().unwrap().as_deref(),
            Some("foreign-delete-success")
        );
        for uid in [&deleted.uid, &deep.uid] {
            assert!(db.node_by_uid(&uid.to_string()).unwrap().unwrap().trashed);
        }
        assert!(db.search("Searchable", 10).unwrap().is_empty());
        assert_eq!(db.pending_ops().unwrap().len(), 1);
        assert_eq!(db.share_access(&deleted.uid).unwrap(), Some(Access::Viewer));
    }

    #[test]
    fn foreign_delete_db_failure_denies_memory_and_retains_prior_cursor() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let root = folder("own", "root", None, "My Files");
        let deleted = folder(
            "foreign",
            "deleted",
            Some(root.uid.clone()),
            "Deleted foreign",
        );
        let deep = folder(
            "foreign",
            "deep",
            Some(deleted.uid.clone()),
            "Still searchable",
        );
        db.upsert_nodes(&[root, deleted.clone(), deep.clone()])
            .unwrap();
        db.set_share_access(&deleted.uid, Access::Editor).unwrap();
        db.set_event_cursor("prior-event").unwrap();
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER reject_foreign_tombstone
                 BEFORE UPDATE OF trashed ON nodes
                 WHEN OLD.uid = 'foreign~deleted'
                 BEGIN SELECT RAISE(ABORT, 'forced tombstone failure'); END;",
            )?;
            Ok(())
        })
        .unwrap();
        let event = DriveEvent::NodeDeleted {
            id: DriveEventId::from("foreign-delete-failed"),
            node_uid: deleted.uid.clone(),
            parent_node_uid: deleted.parent_uid.clone(),
        };
        let denied = Cell::new(false);

        let applied = apply_foreign_delete(&db, &deleted.uid, |_| denied.set(true));

        assert!(applied.is_err());
        assert!(
            denied.get(),
            "resident state must fail closed on DB failure"
        );
        assert!(acknowledge_applied_event(&db, &event, applied).is_err());
        assert_eq!(
            db.get_event_cursor().unwrap().as_deref(),
            Some("prior-event")
        );
        assert!(
            !db.node_by_uid(&deleted.uid.to_string())
                .unwrap()
                .unwrap()
                .trashed
        );
        assert_eq!(db.search("searchable", 10).unwrap().len(), 1);
        assert_eq!(
            db.share_access(&deleted.uid).unwrap(),
            Some(Access::Editor),
            "the failed atomic transaction must not partially publish"
        );
    }

    #[test]
    fn deleted_share_downgrade_failure_denies_memory_and_retains_cursor() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let root = uid("shared");
        db.set_share_access(&root, Access::Editor).unwrap();
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER reject_share_update
                 BEFORE UPDATE ON share_access
                 BEGIN SELECT RAISE(FAIL, 'forced downgrade failure'); END;",
            )?;
            Ok(())
        })
        .unwrap();
        let event = DriveEvent::NodeDeleted {
            id: DriveEventId::from("event-1"),
            node_uid: root,
            parent_node_uid: None,
        };
        let denied = Cell::new(false);

        let applied = apply_access_downgrade(&db, &event, |root| {
            denied.set(root.is_some());
        });
        assert!(applied.is_err());
        assert!(denied.get(), "resident state must fail closed immediately");
        assert!(acknowledge_applied_event(&db, &event, applied).is_err());
        assert!(
            db.get_event_cursor().unwrap().is_none(),
            "failed downgrade must leave the event unacknowledged"
        );
    }

    #[test]
    fn deleted_resident_root_without_share_row_is_denied_and_tombstoned() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let root = uid("resident-without-row");
        assert!(db.share_access(&root).unwrap().is_none());
        let resident = std::cell::RefCell::new(std::collections::HashMap::from([(
            root.clone(),
            Access::Editor,
        )]));
        let event = DriveEvent::NodeDeleted {
            id: DriveEventId::from("event-missing-row"),
            node_uid: root.clone(),
            parent_node_uid: None,
        };

        let applied = apply_access_downgrade(&db, &event, |target| {
            assert_eq!(target, Some(&root));
            resident.borrow_mut().insert(root.clone(), Access::Viewer);
        });
        assert!(applied.is_ok());
        assert_eq!(resident.borrow().get(&root), Some(&Access::Viewer));
        assert_eq!(db.share_access(&root).unwrap(), Some(Access::Viewer));

        let acknowledged = acknowledge_applied_event(&db, &event, applied).unwrap();
        assert_eq!(acknowledged.as_str(), "event-missing-row");
        assert_eq!(
            db.get_event_cursor().unwrap().as_deref(),
            Some("event-missing-row")
        );
    }

    #[test]
    fn continuity_downgrade_failure_denies_memory_and_retains_cursor() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let root = uid("shared");
        db.set_share_access(&root, Access::Editor).unwrap();
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER reject_all_share_updates
                 BEFORE UPDATE ON share_access
                 BEGIN SELECT RAISE(FAIL, 'forced global downgrade failure'); END;",
            )?;
            Ok(())
        })
        .unwrap();
        let event = DriveEvent::ContinuityLost {
            id: DriveEventId::from("event-2"),
        };
        let denied_all = Cell::new(false);

        let applied = apply_access_downgrade(&db, &event, |root| {
            denied_all.set(root.is_none());
        });
        assert!(applied.is_err());
        assert!(
            denied_all.get(),
            "all resident shared roots must fail closed immediately"
        );
        assert!(acknowledge_applied_event(&db, &event, applied).is_err());
        assert!(db.get_event_cursor().unwrap().is_none());
    }
}
