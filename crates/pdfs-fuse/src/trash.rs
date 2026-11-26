//! The Drive trash as the front-ends see it: listing it, refreshing that
//! listing in the background, restoring from it, deleting from it for good.

use super::*;

/// Whether a trash refresh is running, and the wake-up for requests waiting
/// on its progress.
#[derive(Clone)]
pub(crate) struct TrashState {
    /// True while a background refresh of the trash is already running, so a
    /// burst of requests against a stale listing starts one refresh.
    pub(crate) trash_refreshing: Arc<AtomicBool>,
    /// Fires whenever a trash refresh persists a batch or finishes, so a
    /// `ListTrash` request waiting on a first-ever refresh wakes on progress
    /// instead of polling or blocking for the whole run. See
    /// [`Core::await_trash_refresh`].
    pub(crate) trash_progress: Arc<tokio::sync::Notify>,
}

impl TrashState {
    pub(crate) fn new() -> Self {
        Self {
            trash_refreshing: Arc::new(AtomicBool::new(false)),
            trash_progress: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

impl Core {
    /// List the account's trash, from the DB. Trashed nodes are outside the
    /// mounted tree — the FUSE side forgot them when they were trashed — so the
    /// listing is persisted in its own table rather than derived from `State`, and
    /// each entry is identified by its uid (its only remaining handle) with an
    /// empty path.
    ///
    /// Stale-while-revalidate, like the photos timeline: a persisted listing comes
    /// back at DB speed and is refreshed in the background past [`TRASH_TTL`].
    /// Our own trash mutations invalidate it outright (see
    /// [`Core::invalidate_trash`]), so the TTL only covers changes made elsewhere.
    ///
    /// The refresh always runs *off* the request: a never-fetched trash waits for
    /// it, but only for [`TRASH_FIRST_WAIT`], and then answers with the batches
    /// that have landed so far. Blocking the request on the whole refresh is what
    /// made a large trash unlistable — the refresh outran the front-end's read
    /// timeout, the user asked again, and each attempt left another full refresh
    /// grinding in the daemon (`docs/BUGS.md` B72).
    pub(super) fn list_trash(&self) -> CoreResult<Vec<DirEntry>> {
        let never_fetched = self.db.state_i64(TRASH_SYNCED_MS).ok().flatten().is_none();
        if never_fetched || self.listing_stale(TRASH_SYNCED_MS, TRASH_TTL) {
            self.spawn_trash_refresh();
        }
        if never_fetched {
            self.await_trash_refresh(TRASH_FIRST_WAIT);
        }

        let items = self.db.trash_list().map_err(CoreError::from)?;
        let trashed: HashSet<&str> = items.iter().map(|item| item.uid.as_str()).collect();
        let trashed_parents: Vec<Option<String>> = items
            .iter()
            .map(|item| {
                item.parent_uid
                    .clone()
                    .filter(|parent| trashed.contains(parent.as_str()))
            })
            .collect();
        // Most of a trash comes from a handful of folders.
        let mut origins: HashMap<String, Option<String>> = HashMap::new();
        Ok(items
            .into_iter()
            .zip(trashed_parents)
            .map(|(item, trashed_parent)| DirEntry {
                trashed_from: item.parent_uid.as_ref().and_then(|parent| {
                    origins
                        .entry(parent.clone())
                        .or_insert_with(|| self.db.node_path(parent).ok().flatten())
                        .clone()
                }),
                trashed_parent,
                trashed_at: item.trashed_at.unwrap_or(0),
                name: item.name,
                is_dir: item.is_dir,
                size: item.size.max(0) as u64,
                modified: item.mtime,
                // A trashed node can't be pinned or served from the mount, so its
                // content is never current cache: report neither.
                pinned: false,
                pinned_by_folder: false,
                cached: false,
                uid: item.uid,
                path: String::new(),
                role: String::new(),
                shared_by: String::new(),
                shared_at: 0,
                shared_by_unverified: false,
                issue: None,
            })
            .collect())
    }

    /// What a trashed folder holds: its children on the server, and the
    /// stored trash entries trashed inside it before it went.
    pub(super) fn list_trashed_folder(&self, uid: &str) -> CoreResult<Vec<DirEntry>> {
        let folder =
            parse_uid(uid).ok_or_else(|| CoreError::invalid(format!("invalid uid: {uid}")))?;
        let children = self
            .rt
            .block_on(within_trash_call(
                "enumerate trashed folder",
                self.drive.enumerate_folder_children_node_uids(&folder),
            ))?
            .map_err(|e| CoreError::from_api(&e, "enumerate trashed folder"))?;
        let mut nodes = Vec::with_capacity(children.len());
        for chunk in children.chunks(TRASH_MATERIALIZE_CHUNK) {
            nodes.extend(
                self.rt
                    .block_on(within_trash_call(
                        "enumerate nodes",
                        self.drive.enumerate_trashed_nodes_light(chunk),
                    ))?
                    .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?,
            );
        }
        let stored = self.db.trash_list().map_err(CoreError::from)?;
        let stored_at: HashMap<&str, Option<i64>> = stored
            .iter()
            .map(|item| (item.uid.as_str(), item.trashed_at))
            .collect();
        let mut entries: Vec<DirEntry> = nodes
            .iter()
            .map(|TrashedNode { node, trash_time }| {
                let uid = node.uid.to_string();
                let trashed_at = trash_time
                    .or_else(|| stored_at.get(uid.as_str()).copied().flatten())
                    .unwrap_or(0);
                trashed_entry(
                    uid,
                    node.name.clone(),
                    node.is_folder(),
                    node_size(node) as i64,
                    node.modification_time,
                    trashed_at,
                    folder.to_string(),
                )
            })
            .collect();
        let listed: HashSet<String> = entries.iter().map(|e| e.uid.clone()).collect();
        entries.extend(
            stored
                .into_iter()
                .filter(|item| {
                    item.parent_uid.as_deref() == Some(uid) && !listed.contains(&item.uid)
                })
                .map(|item| {
                    trashed_entry(
                        item.uid,
                        item.name,
                        item.is_dir,
                        item.size,
                        item.mtime,
                        item.trashed_at.unwrap_or(0),
                        folder.to_string(),
                    )
                }),
        );
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// Re-fetch the trash listing from the server and persist it.
    ///
    /// Only nodes the stored listing does not hold yet are read, and light: a
    /// trashed file needs no content key, so none is unlocked. Each chunk of
    /// [`TRASH_MATERIALIZE_CHUNK`] is stored as it lands, so a large trash
    /// fills the page while it loads. Rows the server no longer lists go once
    /// the whole listing is in, never earlier.
    pub(super) async fn refresh_trash(&self) -> CoreResult<()> {
        let started = Instant::now();
        let uids = within_trash_call("enumerate trash", self.drive.enumerate_trash_node_uids())
            .await?
            .map_err(|e| CoreError::from_api(&e, "enumerate trash"))?;
        info!(
            count = uids.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "trash refresh: enumerated"
        );

        let seen = self.db.trash_seen().unwrap_or_default();
        let complete = self
            .db
            .state_i64(TRASH_COMPLETE_MS)
            .ok()
            .flatten()
            .is_some();
        let now = now_secs();

        let listed: HashSet<String> = uids.iter().map(ToString::to_string).collect();
        let removed = self.db.trash_retain(&listed).map_err(CoreError::from)?;
        if removed > 0 {
            self.trash.trash_progress.notify_waiters();
        }
        let missing: Vec<NodeUid> = uids
            .into_iter()
            .filter(|uid| !seen.contains_key(&uid.to_string()))
            .collect();
        let mut materialized = 0usize;
        for chunk in missing.chunks(TRASH_MATERIALIZE_CHUNK) {
            let chunk_started = Instant::now();
            let nodes = within_trash_call(
                "enumerate nodes",
                self.drive.enumerate_trashed_nodes_light(chunk),
            )
            .await?
            .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?;
            let items: Vec<StoredTrash> = nodes
                .into_iter()
                .map(|TrashedNode { node, trash_time }| {
                    let uid = node.uid.to_string();
                    StoredTrash {
                        trashed_at: trash_time
                            .or_else(|| first_seen_in_trash(&seen, &uid, complete, now)),
                        uid,
                        name: node.name.clone(),
                        is_dir: node.is_folder(),
                        size: node_size(&node) as i64,
                        mtime: node.modification_time,
                        // The folder the node will go back to, which is also what
                        // tells a restore whether that folder is itself in the trash.
                        parent_uid: node.parent_uid.as_ref().map(|uid| uid.to_string()),
                    }
                })
                .collect();
            materialized += items.len();
            self.db.trash_upsert(&items).map_err(CoreError::from)?;
            self.trash.trash_progress.notify_waiters();
            self.events.publish(&[Topic::Trash]);
            debug!(
                materialized,
                total = missing.len(),
                chunk_ms = chunk_started.elapsed().as_millis() as u64,
                "trash refresh: batch"
            );
        }

        let _ = self.db.set_state_i64(TRASH_SYNCED_MS, now_ms());
        let _ = self.db.set_state_i64(TRASH_COMPLETE_MS, now_ms());
        info!(
            count = listed.len(),
            new = materialized,
            removed,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "trash refresh: done"
        );
        Ok(())
    }

    /// Refresh the trash off the request path. At most one refresh at a time —
    /// including the one a first-ever [`Core::list_trash`] waits on, so a burst
    /// of requests against an unfetched trash joins one refresh instead of
    /// starting one apiece.
    pub(super) fn spawn_trash_refresh(&self) {
        if self.trash.trash_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let core = self.clone();
        self.services
            .spawn_task_job(&self.rt, "pdfs-trash-refresh", async move {
                let refreshed = core.refresh_trash().await;
                core.trash.trash_refreshing.store(false, Ordering::SeqCst);
                match refreshed {
                    Ok(_) => core.events.publish(&[Topic::Trash]),
                    Err(e) => warn!(error = %e, "background trash refresh failed"),
                }
                // After the flag, so a waiter that wakes on this sees it cleared.
                core.trash.trash_progress.notify_waiters();
            });
    }

    /// Wait for the in-flight trash refresh to make progress, for at most
    /// `budget`. Returns early when it finishes; never cancels it — the refresh
    /// is a detached task, so giving up here costs the work nothing.
    pub(super) fn await_trash_refresh(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        self.rt.block_on(async {
            while self.trash.trash_refreshing.load(Ordering::SeqCst) {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                let notified = self.trash.trash_progress.notified();
                tokio::pin!(notified);
                // Register before re-checking the flag: a refresh that finishes
                // in between must not leave us waiting for a notify that has
                // already fired.
                notified.as_mut().enable();
                if !self.trash.trash_refreshing.load(Ordering::SeqCst) {
                    break;
                }
                if tokio::time::timeout(left, notified).await.is_err() {
                    warn!(
                        budget_s = budget.as_secs(),
                        "trash refresh still running; answering with what has materialized"
                    );
                    break;
                }
            }
        });
    }

    /// Drop the persisted trash listing's freshness stamp after we changed the
    /// trash ourselves. The next Trash page then *waits* for a fresh listing
    /// rather than painting one from before the mutation — the user just made the
    /// change and is about to look straight at it.
    pub(super) fn invalidate_trash(&self) {
        let _ = self.db.clear_state(TRASH_SYNCED_MS);
    }

    /// Restore trashed nodes to the folders they were trashed from, together
    /// with the rest of the trashed tree they belong to.
    ///
    /// The trash is a flat list, but a user deletes a shape: a folder, and often
    /// items inside it that were trashed separately beforehand. Restoring one of
    /// those in isolation is not what anyone means by it — a folder would come
    /// back without the contents that were deleted with it, and a file would come
    /// back inside a folder that is still in the trash, which is to say nowhere
    /// the user can see. So a restore takes the whole connected piece:
    /// every trashed descendant of what was asked for, and every trashed ancestor
    /// above it. See [`expand_restore`] for the exact rule.
    ///
    /// The waves are restored shallowest first, because a node cannot be put back
    /// under a parent that is still trashed.
    ///
    /// The destination parents are read *before* the restore — a restored node
    /// reappears in a listing the daemon may already have cached, so each one is
    /// invalidated and re-enumerated on next access.
    pub(super) fn restore(&self, uids: &[String]) -> CoreResult<usize> {
        let requested = Self::parse_uids(uids)?;
        // A listing we could not read is not a reason to refuse the restore: it
        // only costs the expansion, and the requested nodes still go back.
        let relations = self.db.trash_parents().unwrap_or_default();
        let waves = expand_restore(uids, &relations);
        let waves: Vec<Vec<NodeUid>> = if waves.is_empty() {
            vec![requested.clone()]
        } else {
            waves
                .iter()
                .map(|wave| {
                    wave.iter()
                        .filter_map(|uid| parse_uid(uid))
                        .collect::<Vec<_>>()
                })
                .filter(|wave: &Vec<NodeUid>| !wave.is_empty())
                .collect()
        };

        let all: Vec<NodeUid> = waves.iter().flatten().cloned().collect();
        let parents: Vec<NodeUid> = self
            .rt
            .block_on(self.drive.enumerate_nodes(&all))
            .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?
            .into_iter()
            .filter_map(|n| n.parent_uid)
            .collect();
        // Streamed per-node outcomes: a uid the server refuses (already restored,
        // gone) leaves the rest of the batch restored, and each node is unhidden
        // as *its* batch lands rather than after the last one — a daemon killed
        // half way through a large restore leaves local state describing what the
        // server actually did. Only a batch that restored nothing is an error the
        // caller can act on.
        let mut restored: Vec<NodeUid> = Vec::with_capacity(all.len());
        let mut first_error: Option<ProtonError> = None;
        for wave in &waves {
            self.rt
                .block_on(async {
                    let mut outcomes = std::pin::pin!(self.drive.restore_nodes_streaming(wave));
                    while let Some(item) = outcomes.next().await {
                        let (uid, outcome) = item?;
                        match outcome {
                            Ok(()) => {
                                self.writes.unhide(&uid);
                                restored.push(uid);
                            }
                            Err(e) => {
                                warn!(%uid, error = %e, "restore failed for a node");
                                first_error.get_or_insert(e);
                            }
                        }
                    }
                    Ok::<(), ProtonError>(())
                })
                .map_err(|e| CoreError::from_api(&e, "restore"))?;
        }
        if restored.is_empty()
            && let Some(error) = first_error
        {
            return Err(CoreError::from_api(&error, "restore"));
        }
        // Keyed by uid, so it applies to every mount: a restored node reappears
        // in whichever mounts show its parent, not only the primary one.
        self.for_each_state(|st| {
            for parent in &parents {
                if let Some(&ino) = st.by_uid.get(parent) {
                    st.invalidate_listing(ino);
                }
            }
        });
        let back: Vec<String> = restored.iter().map(ToString::to_string).collect();
        let _ = self.db.trash_remove(&back);
        self.invalidate_trash();
        Ok(restored.len())
    }

    /// Permanently delete trashed nodes. Irreversible on the server; locally it
    /// drops any metadata and cached content the node still owns.
    pub(super) fn delete_forever(&self, uids: &[String]) -> CoreResult<usize> {
        let parsed = Self::parse_uids(uids)?;
        // Streamed, and each node's local state is dropped as its batch lands.
        // A permanent delete is irreversible, so the useful property is that a
        // daemon interrupted mid-batch has already forgotten exactly the nodes
        // the server destroyed — never more, never fewer.
        let mut deleted = 0usize;
        let mut gone: Vec<String> = Vec::new();
        let mut first_error: Option<ProtonError> = None;
        self.rt
            .block_on(async {
                let mut outcomes = std::pin::pin!(self.drive.delete_nodes_streaming(&parsed));
                while let Some(item) = outcomes.next().await {
                    let (uid, outcome) = item?;
                    match outcome {
                        Ok(()) => {
                            self.drop_local(std::slice::from_ref(&uid));
                            gone.push(uid.to_string());
                            deleted += 1;
                        }
                        Err(e) => {
                            warn!(%uid, error = %e, "permanent delete failed for a node");
                            first_error.get_or_insert(e);
                        }
                    }
                }
                Ok::<(), ProtonError>(())
            })
            .map_err(|e| CoreError::from_api(&e, "delete"))?;
        if deleted == 0
            && let Some(error) = first_error
        {
            return Err(CoreError::from_api(&error, "delete"));
        }
        let _ = self.db.trash_remove(&gone);
        self.invalidate_trash();
        Ok(deleted)
    }

    /// Permanently delete everything in the trash.
    ///
    /// The stored listing says what goes, so the blobs of items trashed by
    /// another client, which this daemon may still hold, are reclaimed too. It
    /// is not read from the server again first: on a large trash that listing
    /// is what kept the delete from ever being sent (`docs/BUGS.md` B293).
    /// The stored listing is cleared at once, and the server is asked again
    /// after [`TRASH_TTL`], so anything it still holds then shows up again.
    pub(super) fn empty_trash(&self) -> CoreResult<usize> {
        let uids: Vec<String> = self
            .db
            .trash_seen()
            .map_err(CoreError::from)?
            .into_keys()
            .collect();
        self.rt
            .block_on(within_trash_call("empty trash", self.drive.empty_trash()))?
            .map_err(|e| CoreError::from_api(&e, "empty trash"))?;
        let parsed: Vec<NodeUid> = uids.iter().filter_map(|uid| parse_uid(uid)).collect();
        self.drop_local(&parsed);
        let _ = self.db.trash_replace(&[]);
        let _ = self.db.set_state_i64(TRASH_SYNCED_MS, now_ms());
        self.events.publish(&[Topic::Trash]);
        Ok(uids.len())
    }
}

/// An entry inside a trashed folder, for [`Core::list_trashed_folder`].
pub(super) fn trashed_entry(
    uid: String,
    name: String,
    is_dir: bool,
    size: i64,
    mtime: i64,
    trashed_at: i64,
    parent: String,
) -> DirEntry {
    DirEntry {
        name,
        is_dir,
        size: size.max(0) as u64,
        modified: mtime,
        pinned: false,
        pinned_by_folder: false,
        cached: false,
        uid,
        path: String::new(),
        role: String::new(),
        shared_by: String::new(),
        shared_at: 0,
        shared_by_unverified: false,
        trashed_at,
        trashed_from: None,
        trashed_parent: Some(parent),
        issue: None,
    }
}

/// When a node a trash refresh found went to the trash, as far as the daemon
/// can tell. A node the stored listing already had keeps the time it had. A new
/// one went there since the last refresh, so now is close, but only when that
/// refresh saw the whole trash (`complete`): after a first or interrupted one,
/// a node missing from the listing may be years old, and no date beats a wrong
/// one.
pub(super) fn first_seen_in_trash(
    seen: &HashMap<String, Option<i64>>,
    uid: &str,
    complete: bool,
    now: i64,
) -> Option<i64> {
    match seen.get(uid) {
        Some(at) => *at,
        None if complete => Some(now),
        None => None,
    }
}

/// Expand a restore request across the trashed tree, and order it so a parent is
/// always restored before its children.
///
/// `relations` is the persisted trash listing as `(uid, parent_uid)` pairs; a
/// parent that is not itself in the listing is a live folder, which ends the
/// walk upward. The rule is:
///
/// - every trashed descendant of a requested node joins the restore, so a folder
///   comes back with the items that were trashed inside it;
/// - every trashed ancestor of a requested node joins it too, so a file never
///   lands in a folder that is still in the trash;
/// - a requested uid the listing does not know about is restored on its own,
///   because a stale or half-materialised listing must not silently drop it.
///
/// The result is one wave per depth, shallowest first. Waves are disjoint and
/// together contain each uid exactly once.
pub(super) fn expand_restore(
    requested: &[String],
    relations: &[(String, Option<String>)],
) -> Vec<Vec<String>> {
    let parents: HashMap<&str, Option<&str>> = relations
        .iter()
        .map(|(uid, parent)| (uid.as_str(), parent.as_deref()))
        .collect();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for (uid, parent) in relations {
        // Only a trashed parent: a live one keeps its child as a root here.
        if let Some(parent) = parent.as_deref()
            && parents.contains_key(parent)
        {
            children.entry(parent).or_default().push(uid.as_str());
        }
    }

    let mut selected: HashSet<&str> = HashSet::new();
    for uid in requested {
        let uid = uid.as_str();
        if !parents.contains_key(uid) {
            selected.insert(uid);
            continue;
        }
        // Upward: every trashed ancestor, so the destination folder exists again.
        let mut cursor = Some(uid);
        while let Some(current) = cursor {
            // A parent outside the listing is live: the walk stops under it.
            if !parents.contains_key(current) || !selected.insert(current) {
                break;
            }
            cursor = parents.get(current).copied().flatten();
        }
        // Downward: the whole trashed subtree, so the folder comes back full.
        let mut stack = vec![uid];
        while let Some(current) = stack.pop() {
            for child in children.get(current).into_iter().flatten() {
                if selected.insert(child) {
                    stack.push(child);
                }
            }
        }
    }

    // Depth within the *trash*, not within the drive: a node whose parent is live
    // (or unknown) is at depth 0 and can be restored straight away.
    let mut waves: Vec<Vec<String>> = Vec::new();
    for uid in &selected {
        let mut depth = 0usize;
        let mut cursor = parents.get(uid).copied().flatten();
        // Only trashed ancestors count, and the walk is bounded by the listing's
        // size: server data that somehow describes a cycle must not spin here.
        while let Some(current) = cursor
            && parents.contains_key(current)
            && depth < parents.len()
        {
            depth += 1;
            cursor = parents.get(current).copied().flatten();
        }
        if waves.len() <= depth {
            waves.resize(depth + 1, Vec::new());
        }
        waves[depth].push((*uid).to_string());
    }
    // Stable output, so a restore of the same trash is the same sequence of calls
    // every time — the batches are what shows up in the log and in the tests.
    for wave in &mut waves {
        wave.sort();
    }
    waves.retain(|wave| !wave.is_empty());
    waves
}
