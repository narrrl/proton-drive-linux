//! The walks of a pass: the local tree, Drive's folders (resumable, and only
//! the changed ones when the event feed can be trusted), and listing one.

use super::*;

impl Syncer {
    /// Recursively walk a local tree into `out`, keyed by `/`-joined relative
    /// path. Symlinks and other special files are skipped, and paths that cannot
    /// be read go to `unscanned`. Reports each entry to the pass's scan progress.
    pub(super) fn walk_local(
        &self,
        folder_id: i64,
        root: &Path,
        rules: &IgnoreRules,
        writing: &HashSet<PathBuf>,
        out: &mut HashMap<String, LocalItem>,
        unscanned: &mut Unscanned,
    ) -> Result<(), String> {
        walk_local_tree(root, rules, writing, out, unscanned, &mut |rel| {
            self.progress_scanned(folder_id, base_name(rel))
        })
    }

    /// Map every remote folder under `folder` to its uid by rel path, ignoring
    /// files. The cheap half of [`walk_remote`](Self::walk_remote): folders carry no
    /// content signature, so a light listing answers this without unlocking a single
    /// node key. Used by [`push_pass`](Self::push_pass) to tell a folder it must
    /// create from one it just hasn't recorded yet.
    pub(super) fn walk_remote_dirs(
        &self,
        folder_id: i64,
        folder: &NodeUid,
        prefix: &str,
        dirs: &mut HashMap<String, NodeUid>,
    ) -> Result<(), String> {
        let uids = self
            .rt
            .block_on(self.drive.enumerate_folder_children_node_uids(folder))
            .map_err(|e| format!("enumerate {folder}: {e}"))?;
        if uids.is_empty() {
            return Ok(());
        }
        let nodes = self
            .rt
            .block_on(self.drive.enumerate_nodes_light(&uids))
            .map_err(|e| format!("resolve nodes: {e}"))?;
        for node in nodes {
            if node.trashed || !node.is_folder() || !pdfs_drive::remote_name_is_safe(&node.name) {
                continue;
            }
            let rel = join_rel(prefix, &node.name);
            self.progress_scanned(folder_id, base_name(&rel));
            dirs.insert(rel.clone(), node.uid.clone());
            self.walk_remote_dirs(folder_id, &node.uid, &rel, dirs)?;
        }
        Ok(())
    }

    /// Walk the remote tree of a mirror folder into `walk`, folder by folder,
    /// recording every descendant's relative path, and mapping each subfolder's
    /// rel path to its uid in `walk.dirs`.
    ///
    /// A file's size comes from its extended attributes, which only its own node
    /// key can decrypt — and unlocking that key costs an S2K derivation (tens of
    /// milliseconds) *per file*, which is the bulk of a pass over a large folder.
    /// So the walk enumerates cheaply ([`enumerate_nodes_light`]) and only pays
    /// for the files whose modification time has moved away from `baseline`: an
    /// unchanged mtime means the recorded size still stands.
    ///
    /// [`enumerate_nodes_light`]: proton_drive_rs::ProtonDriveClient::enumerate_nodes_light
    ///
    /// What the walk could not read goes to `walk.gaps`, never silently out of
    /// the map: a missing path reads as deleted on Drive.
    ///
    /// Each folder's listing is saved as it comes in. A walk cut short by a
    /// stop resumes from what was saved and lists only the folders it had not
    /// reached, where it used to start over and, on a folder large enough,
    /// never finish (B294). The database drops a saved walk whenever the
    /// folder's baseline changes, so a resumed walk never meets a baseline
    /// newer than its listings; it is as old as a long walk's first listings
    /// would be, and the apply step checks each file again before it acts.
    ///
    /// With `reuse` false, a complete saved walk is not used: the root state
    /// owes Drive a walk in full. Returns whether the walk covered all of
    /// Drive's side, rather than only the folders marked dirty.
    pub(super) fn walk_remote(
        &self,
        folder_id: i64,
        root: &NodeUid,
        walk: &mut RemoteWalk<'_>,
        baseline: &HashMap<String, StoredSyncEntry>,
        reuse: bool,
    ) -> Result<bool, PassAbort> {
        let WalkStart {
            mut todo,
            mut children,
            dirty_upto,
        } = self.start_walk(folder_id, root, walk, reuse);
        // Started from the saved picture, the walk lists only what changed.
        let in_full = children.is_none();
        // Folders to list again next pass: their listing failed or came back
        // short this time.
        let mut again: Vec<String> = Vec::new();
        let mut listed: HashSet<String> = HashSet::new();
        while let Some(next) = todo.pop_front() {
            // Checked per folder, not per pass: this walk is the long pole on a
            // large tree, and a user who asks for on-demand three minutes into
            // it should not wait out the rest of a survey of files they want
            // deleted. Bailing out here is safe precisely because the walk has
            // applied nothing — a *partial* remote map, on the other hand,
            // would be read as "the remote deleted everything we haven't
            // reached yet", so it must never reach the diff.
            if self.shutdown.is_stopping() {
                return Err(PassAbort::Stopped);
            }
            if self.pass_abandoned(folder_id) {
                return Err(PassAbort::Abandoned);
            }
            if self.ondemand_queued(folder_id) {
                return Err(PassAbort::Interrupted);
            }
            // Gone with a folder listed again before it, or listed already.
            if walk.dirs.get(&next.prefix) != Some(&next.uid) || !listed.insert(next.prefix.clone())
            {
                continue;
            }
            let prefix = next.prefix;
            // What an earlier listing could not read is read again now.
            walk.gaps.forget(&prefix);
            let Some(listing) = self.list_remote(folder_id, &next.uid, &prefix, walk, baseline)?
            else {
                // What the saved walk holds of it is older than the change
                // that marked it: its files are left out of this pass, as a
                // folder a full walk could not list is.
                if let Some(children) = &children {
                    for rel in children.of(&prefix) {
                        if !walk.dirs.contains_key(rel) {
                            walk.out.remove(rel);
                        }
                    }
                }
                again.push(prefix);
                continue;
            };
            if listing.short || !listing.unread.is_empty() {
                again.push(prefix.clone());
            }
            let mut removed = Vec::new();
            let mut kept: HashSet<String> = HashSet::new();
            if let Some(children) = &mut children {
                // Listed again: what it held before and no longer holds goes,
                // with all under it. A folder still there under the same uid
                // keeps what was saved under it.
                let fresh: HashMap<&str, &RemoteItem> = listing
                    .items
                    .iter()
                    .map(|(rel, item)| (rel.as_str(), item))
                    .collect();
                for old in children.take(&prefix) {
                    let same_folder = walk.dirs.get(&old).is_some_and(|uid| {
                        fresh
                            .get(old.as_str())
                            .is_some_and(|item| item.is_dir && &item.uid == uid)
                    });
                    if same_folder {
                        kept.insert(old);
                    } else {
                        children.remove_subtree(&old, walk, &mut removed);
                    }
                }
                children.add(
                    &prefix,
                    listing
                        .items
                        .iter()
                        .map(|(rel, _)| rel)
                        .chain(&listing.unread),
                );
            }
            let mut saved = Vec::with_capacity(listing.items.len() + listing.unread.len());
            for (rel, item) in listing.items {
                self.progress_scanned(folder_id, base_name(&rel));
                saved.push(SavedWalkItem {
                    rel_path: rel.clone(),
                    uid: Some(item.uid.to_string()),
                    is_dir: item.is_dir,
                    mtime: item.mtime,
                    size: item.size,
                    revision: item.revision.clone(),
                    sha1: item.sha1.clone(),
                });
                if item.is_dir {
                    walk.dirs.insert(rel.clone(), item.uid.clone());
                    if !kept.contains(&rel) {
                        todo.push_back(WalkNext {
                            prefix: rel.clone(),
                            uid: item.uid.clone(),
                        });
                    }
                }
                walk.out.insert(rel, item);
            }
            for rel in listing.unread {
                self.progress_scanned(folder_id, base_name(&rel));
                saved.push(SavedWalkItem {
                    rel_path: rel.clone(),
                    uid: None,
                    is_dir: false,
                    mtime: 0,
                    size: 0,
                    revision: None,
                    sha1: None,
                });
                walk.gaps.unread(rel);
            }
            // A walk that cannot be saved still runs; it just starts over if
            // it is cut short.
            if let Err(e) =
                self.store
                    .sync_walk_record(folder_id, &prefix, listing.short, &saved, &removed)
            {
                debug!(folder = prefix, error = ?e, "sync: cannot save the walk's progress");
            }
        }
        // A stop that came during the last listing still ends the pass here,
        // and the next one resumes a walk with nothing left to list.
        if self.shutdown.is_stopping() {
            return Err(PassAbort::Stopped);
        }
        self.finish_walk(folder_id, dirty_upto, &again)
            .map_err(|e| PassAbort::from(format!("save the walk of Drive: {e:?}")))?;
        Ok(in_full)
    }

    /// Keep a walk that reached its end as the next pass's picture of Drive.
    /// A walk that listed every folder drops the dirty marks it read past; a
    /// walk of the dirty folders only drops the marks it read, up to
    /// `dirty_upto`. Either way, the folders in `again` stay dirty, so the
    /// next pass lists them again.
    pub(super) fn finish_walk(
        &self,
        folder_id: i64,
        dirty_upto: Option<i64>,
        again: &[String],
    ) -> pdfs_core::Result<()> {
        if !again.is_empty() {
            self.store.sync_remote_mark(folder_id, again)?;
        }
        match dirty_upto {
            Some(seq) => self.store.sync_remote_dirty_consume(folder_id, seq),
            None => {
                if !self.store.sync_walk_complete(folder_id)? {
                    debug!(
                        id = folder_id,
                        "sync: the walk of Drive was dropped before its end"
                    );
                }
                Ok(())
            }
        }
    }

    /// Where a walk of Drive starts.
    ///
    /// - A complete walk, kept up to date by the event stream, is loaded into
    ///   `walk`, and only the folders marked dirty since are listed again.
    /// - A walk an earlier pass did not finish is loaded and continues, when
    ///   it is recent enough.
    /// - Otherwise a new walk starts at the root.
    pub(super) fn start_walk(
        &self,
        folder_id: i64,
        root: &NodeUid,
        walk: &mut RemoteWalk<'_>,
        reuse: bool,
    ) -> WalkStart {
        let root_uid = root.to_string();
        let saved = match self.store.sync_walk_load(folder_id) {
            Ok(saved) => saved,
            Err(e) => {
                warn!(id = folder_id, error = ?e, "sync: cannot read the saved walk; starting over");
                None
            }
        };
        if let Some(saved) = saved
            && saved.root_uid == root_uid
        {
            let age = now_secs().saturating_sub(saved.started);
            let fresh = saved.complete
                && age < AUDIT_INTERVAL.as_secs() as i64
                && self.events_trusted(root);
            if fresh && !reuse {
                info!(
                    id = folder_id,
                    "sync: the root state asks for a walk of all of Drive"
                );
            }
            if fresh
                && reuse
                && let Some(start) = self.walk_dirty(folder_id, &saved, walk)
            {
                return start;
            }
            if !saved.complete
                && age < WALK_MAX_AGE.as_secs() as i64
                && let Some(restored) = restore_walk(&saved, walk)
            {
                self.progress_scanned_many(folder_id, restored.items);
                self.trace_count(Count::Resumed, restored.items);
                info!(
                    id = folder_id,
                    restored = restored.items,
                    folders_left = restored.todo.len(),
                    age_s = age,
                    "sync: resuming the walk of Drive"
                );
                return WalkStart {
                    todo: restored.todo,
                    children: None,
                    dirty_upto: None,
                };
            }
            // Loaded into `walk`, and of no use: start from nothing.
            walk.out.clear();
            walk.dirs.retain(|rel, _| rel.is_empty());
            *walk.gaps = RemoteGaps::default();
        }
        if let Err(e) = self.store.sync_walk_begin(folder_id, &root_uid, now_secs()) {
            debug!(id = folder_id, error = ?e, "sync: cannot save the walk's progress");
        }
        WalkStart {
            todo: VecDeque::from([WalkNext {
                prefix: String::new(),
                uid: root.clone(),
            }]),
            children: None,
            dirty_upto: None,
        }
    }

    /// Start from a complete walk: load it into `walk`, and list again the
    /// folders marked dirty since, plus any found since and not listed yet.
    /// `None` when it cannot be read back.
    pub(super) fn walk_dirty(
        &self,
        folder_id: i64,
        saved: &SavedWalk,
        walk: &mut RemoteWalk<'_>,
    ) -> Option<WalkStart> {
        let dirty = match self.store.sync_remote_dirty(folder_id) {
            Ok(dirty) => dirty,
            Err(e) => {
                warn!(id = folder_id, error = ?e, "sync: cannot read what changed on Drive");
                return None;
            }
        };
        let restored = restore_walk(saved, walk)?;
        let mut todo = restored.todo;
        let mut dirs: Vec<&str> = dirty.iter().map(|(_, dir)| dir.as_str()).collect();
        dirs.sort_by_key(|dir| (dir.matches('/').count(), *dir));
        dirs.dedup();
        // Shallowest first: a folder listed again may turn out new or gone,
        // and is then listed whole or dropped with what is under it.
        for dir in &dirs {
            match walk.dirs.get(*dir) {
                Some(uid) => todo.push_back(WalkNext {
                    prefix: dir.to_string(),
                    uid: uid.clone(),
                }),
                // Gone from Drive, or new and not listed yet: listing its
                // parent, marked too, settles it.
                None => debug!(id = folder_id, dir, "sync: a dirty folder is not on Drive"),
            }
        }
        self.progress_scanned_many(folder_id, restored.items);
        self.trace_count(Count::Reused, restored.items);
        self.trace_count(Count::Dirty, dirs.len());
        info!(
            id = folder_id,
            known = restored.items,
            dirty = dirs.len(),
            to_list = todo.len(),
            "sync: listing only the folders changed on Drive"
        );
        Some(WalkStart {
            todo,
            children: Some(restored.children),
            dirty_upto: Some(dirty.iter().map(|(seq, _)| *seq).max().unwrap_or(0)),
        })
    }

    /// Whether Drive's event stream for `root` is being followed, so a
    /// change on Drive is marked dirty by the time a pass looks. The stream
    /// covers the account's own volume only, and must have been read lately.
    ///
    /// Right after a start, the stream has not been read yet: wait a little
    /// for it rather than walk all of Drive again.
    pub(super) fn events_trusted(&self, root: &NodeUid) -> bool {
        if root.volume_id != self.primary_root_uid.volume_id {
            return false;
        }
        let started = Instant::now();
        while self.events_read_at.load(Ordering::Relaxed) == 0
            && started.elapsed() < EVENTS_FRESH
            && self.shutdown.sleep(Duration::from_millis(250))
        {}
        let read_at = self.events_read_at.load(Ordering::Relaxed);
        read_at > 0 && now_secs().saturating_sub(read_at) < EVENTS_FRESH.as_secs() as i64
    }

    /// List one folder on Drive. `None` when the listing failed: that is
    /// noted in `walk.gaps`, and the folder is not saved as listed, so a
    /// resumed walk asks again. A failure at the root fails the pass.
    pub(super) fn list_remote(
        &self,
        folder_id: i64,
        folder: &NodeUid,
        prefix: &str,
        walk: &mut RemoteWalk<'_>,
        baseline: &HashMap<String, StoredSyncEntry>,
    ) -> Result<Option<Listing>, PassAbort> {
        let listing = Instant::now();
        let step = Instant::now();
        let uids = self
            .rt
            .block_on(self.drive.enumerate_folder_children_node_uids(folder))
            .map_err(|e| format!("enumerate {folder}: {e}"));
        self.trace_call(Call::ListChildren, step.elapsed());
        self.trace_count(Count::RemoteFolders, 1);
        let Some(uids) = unlisted_gap(walk, prefix, uids)? else {
            return Ok(None);
        };
        let mut out = Listing::default();
        if uids.is_empty() {
            return Ok(Some(out));
        }
        let step = Instant::now();
        let nodes = self
            .rt
            .block_on(self.drive.enumerate_nodes_light(&uids))
            .map_err(|e| format!("resolve nodes: {e}"));
        self.trace_call(Call::ReadNodes, step.elapsed());
        let Some(nodes) = unlisted_gap(walk, prefix, nodes)? else {
            return Ok(None);
        };
        // The SDK leaves out a node it cannot decrypt, and says so only in its
        // log. Its name went with it, so all this can record is the folder.
        if nodes.len() < uids.len() {
            warn!(
                folder = prefix,
                listed = uids.len(),
                read = nodes.len(),
                "sync: some nodes on Drive cannot be read; leaving them out"
            );
            walk.gaps.short_listing(prefix);
            out.short = true;
        }
        let nodes: Vec<Node> = nodes
            .into_iter()
            .filter(|node| {
                let safe = pdfs_drive::remote_name_is_safe(&node.name);
                if !safe {
                    warn!(folder = prefix, uid = %node.uid, "sync: a name on Drive cannot be a local file name; leaving it out");
                }
                safe
            })
            .collect();

        // Files whose recorded mtime no longer matches: their size has to be
        // read for real, so these are the only node keys worth unlocking.
        let stale: Vec<NodeUid> = nodes
            .iter()
            .filter(|n| !n.trashed && !n.is_folder())
            .filter(|n| {
                unchanged_remote_size(
                    baseline,
                    &join_rel(prefix, &n.name),
                    n.modification_time,
                    pdfs_drive::node_revision_id(n).as_deref(),
                )
                .is_none()
            })
            .map(|n| n.uid.clone())
            .collect();
        // The full enumeration also decrypts the content digest, which the
        // first sync of a path compares against the local file (B248).
        let sized: HashMap<String, (i64, Option<String>)> = if stale.is_empty() {
            HashMap::new()
        } else {
            let step = Instant::now();
            let full = self
                .rt
                .block_on(self.drive.enumerate_nodes(&stale))
                .map_err(|e| format!("resolve nodes: {e}"));
            self.trace_call(Call::ReadNodesFull, step.elapsed());
            self.trace_count(Count::NodesFull, stale.len());
            let Some(full) = unlisted_gap(walk, prefix, full)? else {
                return Ok(None);
            };
            full.iter()
                .map(|n| {
                    (
                        n.uid.to_string(),
                        (remote_file_sig(n).1, pdfs_drive::node_content_sha1(n)),
                    )
                })
                .collect()
        };
        let stale: HashSet<String> = stale.iter().map(|u| u.to_string()).collect();
        self.record_revisions(folder_id, prefix, &nodes, &sized, baseline);
        self.trace_count(Count::Nodes, nodes.len());
        self.engine.trace(|t| t.listing(prefix, listing.elapsed()));

        for node in nodes {
            if node.trashed {
                continue;
            }
            let rel = join_rel(prefix, &node.name);
            if node.is_folder() {
                out.items.push((
                    rel,
                    RemoteItem {
                        uid: node.uid,
                        is_dir: true,
                        mtime: 0,
                        size: 0,
                        revision: None,
                        sha1: None,
                    },
                ));
                continue;
            }
            let (size, sha1) = if stale.contains(&node.uid.to_string()) {
                match sized.get(&node.uid.to_string()) {
                    Some(sized) => sized.clone(),
                    // Its size was needed but could not be read (the full
                    // enumeration skipped it as undecryptable). Leave it out
                    // rather than record a stale size, and say so: missing,
                    // it would read as deleted on Drive.
                    None => {
                        out.unread.push(rel);
                        continue;
                    }
                }
            } else {
                match unchanged_remote_size(
                    baseline,
                    &rel,
                    node.modification_time,
                    pdfs_drive::node_revision_id(&node).as_deref(),
                ) {
                    Some(size) => (size, None),
                    None => continue,
                }
            };
            out.items.push((
                rel,
                RemoteItem {
                    revision: pdfs_drive::node_revision_id(&node),
                    uid: node.uid,
                    is_dir: false,
                    mtime: node.modification_time,
                    size,
                    sha1,
                },
            ));
        }
        Ok(Some(out))
    }
}
