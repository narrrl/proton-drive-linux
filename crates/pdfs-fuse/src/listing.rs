//! Folder listings: when one is listed again, and what a listing from Drive
//! is merged with (queued ops, landed creates, offline copies).

use super::*;

impl Core {
    /// Whether [`Core::ensure_children`] would answer for `ino` without
    /// touching the network. Lets a handler decide between answering inline
    /// and handing off to a worker.
    ///
    /// A resident listing is not enough on its own. A shared folder's, or the
    /// Shared with me root's, is fetched again once it is a minute old, and
    /// that fetch ran on the dispatch loop, so every request on the mount
    /// waited for Drive to answer (`docs/BUGS.md` B219).
    pub(super) fn children_cached(&self, ino: u64) -> bool {
        let (uid, resident) = {
            let st = self.state();
            match st.entries.get(&ino) {
                Some(e) => (e.uid.clone(), st.children.contains_key(&ino)),
                // `list_children` answers ENOENT without the network.
                None => return true,
            }
        };
        if !resident {
            return false;
        }
        let freshness = if is_virtual_uid(&uid) {
            Some(SHARED_WITH_ME_SYNCED_MS.to_string())
        } else if !self.is_own_or_virtual(&uid) && !is_local_uid(&uid) {
            Some(Self::shared_folder_freshness_key(&uid))
        } else {
            None
        };
        freshness.is_none_or(|key| {
            !listing_needs_refresh(
                self.online.load(Ordering::Relaxed),
                self.shared_listing_stale(&key),
            )
        })
    }

    /// Re-apply the optimistic size of any queued write to `nodes`.
    ///
    /// A node that arrives from the remote (or from its DB row) carries the size
    /// of the revision the *server* holds, which for a file with a write still
    /// queued is the pre-write size — often 0 for a file created moments ago.
    /// Interning it as-is silently reverts the optimistic size that
    /// `record_pending_write` stamped, and a file that stats as 0 bytes is a file
    /// the kernel will not issue a single `read` for: `cat` prints nothing and
    /// the staged blob that `read_range` would have served is never asked for.
    /// That reads as data loss even though nothing is lost (B11).
    ///
    /// [`Core::hydrate`] does the same thing for the restart case. This covers
    /// every *live* re-enumeration — which is what a rename or move triggers,
    /// since both invalidate the listings they touch.
    ///
    /// Snapshots the pending map and returns before any caller takes the state
    /// lock: no site in the daemon holds `pending` and `state` at once, and this
    /// is not the place to become the first.
    pub(super) fn stamp_pending_sizes(&self, nodes: &mut [Node]) {
        let sizes = self.writes.queued_sizes();
        if sizes.is_empty() {
            return;
        }
        apply_pending_sizes(nodes, &sizes);
    }

    pub(super) fn is_own_or_virtual(&self, uid: &NodeUid) -> bool {
        is_own_or_virtual_uid(uid, &self.primary_root_uid.volume_id)
    }

    /// The nodes of `before`, queued in `folder_uid` when its listing was asked
    /// of Drive, that are no longer queued in `after`, as the DB has them now.
    ///
    /// Their create landed while the listing was on the wire, after Drive took
    /// it, and so is in neither. One unlinked or moved away meanwhile is not
    /// under the folder by that name any more and stays out.
    pub(super) fn landed_meanwhile(
        &self,
        folder_uid: &NodeUid,
        before: Vec<Node>,
        after: &[Node],
    ) -> Vec<Node> {
        let still: HashSet<&str> = after.iter().map(|node| node.name.as_str()).collect();
        let landed: HashSet<String> = before
            .into_iter()
            .map(|node| node.name)
            .filter(|name| !still.contains(name.as_str()))
            .collect();
        if landed.is_empty() {
            return Vec::new();
        }
        match self.db.known_children(folder_uid) {
            Ok(known) => known
                .into_iter()
                .filter(|node| landed.contains(&node.name))
                .collect(),
            Err(e) => {
                warn!(%folder_uid, error = %e, "db known_children failed");
                Vec::new()
            }
        }
    }

    /// Serve a listing without the network: the one already in memory if any,
    /// else whatever children the DB still knows. The fallback only answers
    /// until the link is back, when [`Core::relist_offline_listings`] drops it:
    /// a listing dropped as stale may lack a child another device made, and a
    /// create over that name lands as a conflict copy rather than an `EIO` now.
    pub(super) fn adopt_stale_listing(
        &self,
        ino: u64,
        folder_uid: &NodeUid,
        cached: bool,
        primary_root: bool,
    ) -> Result<(), Errno> {
        if cached {
            return Ok(());
        }
        let nodes = self.db.known_children(folder_uid).map_err(|e| {
            warn!(%folder_uid, error = %e, "db known_children failed");
            Errno::EIO
        })?;
        debug!(%folder_uid, count = nodes.len(), "offline: listing a folder from what the DB knows");
        self.link.stale_listings.lock().insert(ino);
        self.adopt_db_listing(ino, folder_uid, nodes, primary_root)
    }

    /// Relist every folder [`Core::adopt_stale_listing`] served while offline.
    pub(crate) fn relist_offline_listings(&self) {
        let stale = std::mem::take(&mut *self.link.stale_listings.lock());
        if stale.is_empty() {
            return;
        }
        let mut st = self.state();
        for &ino in &stale {
            st.invalidate_listing(ino);
        }
        drop(st);
        for mount in self.states.live() {
            if let Some(notifier) = mount.notifier.get() {
                for &ino in &stale {
                    let _ = notifier.inval_inode(ino);
                }
            }
        }
    }

    /// Install `nodes`, read from the DB, as `ino`'s listing.
    pub(super) fn adopt_db_listing(
        &self,
        ino: u64,
        folder_uid: &NodeUid,
        mut nodes: Vec<Node>,
        primary_root: bool,
    ) -> Result<(), Errno> {
        let folder_uid = folder_uid.clone();
        if primary_root {
            nodes.retain(|node| !is_virtual_uid(&node.uid));
        }
        // Before the lock: a DB row carries the size the server last
        // sealed, which a queued write is ahead of (B11).
        self.stamp_pending_sizes(&mut nodes);
        let hidden = self.writes.hidden();
        let mut st = self.state();
        if st.children.contains_key(&ino) {
            return Ok(());
        }
        let mut child_inos = Vec::with_capacity(nodes.len());
        let mut needs_size = Vec::new();
        for node in nodes {
            if !node_visible(&node, &folder_uid, &hidden) {
                continue;
            }
            if matches!(
                &node.kind,
                NodeKind::File {
                    claimed_size: None,
                    ..
                }
            ) {
                needs_size.push(node.uid.clone());
            }
            child_inos.push(st.intern_from_db(ino, node));
        }
        st.children.insert(ino, child_inos);
        let root_snapshot = primary_root.then(|| RootListingSnapshot::capture(&st, ino).unwrap());
        drop(st);
        self.flush_access_changes();
        if let Some(snapshot) = root_snapshot {
            self.reconcile_virtual_root_dentry(ino, snapshot)?;
        }
        // Rows persisted from a cheap enumeration whose upgrade never
        // ran (a restart in between, say) still owe their real sizes.
        self.spawn_size_upgrade(ino, needs_size);
        Ok(())
    }

    /// Enumerate `ino`'s children from the remote and cache them. No-op if the
    /// directory has already been listed. Network I/O happens without the lock
    /// held so concurrent metadata reads aren't blocked behind a fetch.
    ///
    /// A listing that changed while it was being published answers EAGAIN: the
    /// root listed again after a restart, an event, a newer refresh of a shared
    /// folder. The listing is still owed, so it is taken again rather than
    /// handed to the lookup or open that asked (B159).
    pub(super) fn ensure_children(&self, ino: u64) -> Result<(), Errno> {
        for _ in 1..LISTING_ATTEMPTS {
            match self.list_children(ino) {
                Err(e) if e.code() == Errno::EAGAIN.code() => {
                    debug!(ino, "listing changed while it was published; listing again");
                }
                done => return done,
            }
        }
        self.list_children(ino)
    }

    pub(super) fn list_children(&self, ino: u64) -> Result<(), Errno> {
        let (folder_uid, cached) = {
            let st = self.state();
            match st.entries.get(&ino) {
                Some(e) => (e.uid.clone(), st.children.contains_key(&ino)),
                None => return Err(Errno::ENOENT),
            }
        };
        if is_virtual_uid(&folder_uid) {
            return self.ensure_shared_children(ino);
        }

        let primary_root =
            is_primary_root_listing(self.primary, &folder_uid, &self.primary_root_uid);
        let foreign = !self.is_own_or_virtual(&folder_uid) && !is_local_uid(&folder_uid);
        let foreign_key = foreign.then(|| Self::shared_folder_freshness_key(&folder_uid));
        let refresh_foreign = foreign_key.as_deref().is_some_and(|key| {
            listing_needs_refresh(
                self.online.load(Ordering::Relaxed),
                self.shared_listing_stale(key),
            )
        });
        if cached && !refresh_foreign {
            if primary_root {
                self.reconcile_virtual_root_dentry(ino, self.resident_root_listing_snapshot(ino)?)?;
            }
            return Ok(());
        }
        // Offline fast path: a folder the DB still records as fully enumerated
        // can be rebuilt from disk without hitting the API, even if its listing
        // was trimmed from the hot cache mid-run.
        let cached_nodes = if refresh_foreign {
            Ok(None)
        } else {
            self.db.children_if_listed(&folder_uid)
        };
        match cached_nodes {
            Ok(Some(nodes)) => return self.adopt_db_listing(ino, &folder_uid, nodes, primary_root),
            Ok(None) => {}
            Err(e) => warn!(%folder_uid, error = %e, "db children_if_listed failed"),
        }
        // Drive knows nothing by a stand-in. Until its mkdir lands the folder
        // is only here, and once it has, the DB holds what Drive does. A
        // listing dropped meanwhile was asked of Drive, and every lookup in
        // the folder failed with EIO (`docs/BUGS.md` B125).
        if is_local_uid(&folder_uid) {
            let nodes = self.db.known_children(&folder_uid).map_err(|e| {
                warn!(%folder_uid, error = %e, "db known_children failed");
                Errno::EIO
            })?;
            return self.adopt_db_listing(ino, &folder_uid, nodes, primary_root);
        }
        if !self.is_online() {
            return self.adopt_stale_listing(ino, &folder_uid, cached, primary_root);
        }

        let refresh_generation =
            refresh_foreign.then(|| self.shared.shared_generation.load(Ordering::SeqCst));
        // What is queued before Drive is asked: a create that lands while the
        // listing is on the wire is in neither Drive's answer nor the queue read
        // after it (`docs/BUGS.md` B166).
        let queued_before = (!foreign)
            .then(|| self.db.queued_children(&folder_uid).ok())
            .flatten();
        let uids = match self
            .rt
            .block_on(self.drive.enumerate_folder_children_node_uids(&folder_uid))
        {
            Ok(uids) => uids,
            Err(e) if self.lost_link(&e, "list folder") => {
                return self.adopt_stale_listing(ino, &folder_uid, cached, primary_root);
            }
            Err(e) => {
                error!(%folder_uid, error = %e, "enumerate folder children failed");
                return Err(Errno::EIO);
            }
        };
        // Cheap enumeration: `Light` skips unlocking each *file's* node key,
        // which is an S2K derivation per file and was ~74% of the cost of a cold
        // listing (B12 — measured with `perf`, 64% of cycles in SHA-256 alone).
        // Folders are unlocked either way; their keys are what the children are
        // decrypted with, so the walk cannot proceed without them.
        //
        // The price is that files come back without a `claimed_size`, so
        // `node_size` falls back to the *ciphertext* size until
        // `spawn_size_upgrade` below fills the real one in.
        let mut nodes = match self.rt.block_on(self.drive.enumerate_nodes_light(&uids)) {
            Ok(nodes) => nodes,
            Err(e) if self.lost_link(&e, "list folder") => {
                return self.adopt_stale_listing(ino, &folder_uid, cached, primary_root);
            }
            Err(e) => {
                error!(%folder_uid, error = %e, "enumerate nodes failed");
                return Err(Errno::EIO);
            }
        };
        // Drive shows none of what is still queued for the folder (B132).
        // A shared folder's listing is published as Drive has it.
        if !foreign {
            match self.db.queued_children(&folder_uid) {
                Ok((mut queued, gone)) => {
                    if let Some((before, _)) = queued_before {
                        queued.extend(self.landed_meanwhile(&folder_uid, before, &queued));
                    }
                    overlay_queued(&mut nodes, queued, &gone)
                }
                Err(e) => warn!(%folder_uid, error = %e, "db queued_children failed"),
            }
        }
        // Same as the DB path above: the remote's size for a file with a write
        // still queued is the pre-write one (B11).
        self.stamp_pending_sizes(&mut nodes);
        let hidden = self.writes.hidden();
        let mut filtered_nodes: Vec<Node> = nodes
            .into_iter()
            .filter(|node| node_visible(node, &folder_uid, &hidden))
            .collect();
        if foreign {
            for node in &mut filtered_nodes {
                node.parent_uid = Some(folder_uid.clone());
            }
            let _publication = self.shared.shared_publication.lock();
            if !refresh_generation_is_current(
                refresh_generation.expect("foreign refresh has a generation"),
                self.shared.shared_generation.load(Ordering::SeqCst),
            ) {
                return Err(Errno::EAGAIN);
            }
            let removed = self
                .db
                .publish_foreign_children(&folder_uid, &uids, &filtered_nodes)
                .map_err(|error| {
                    error!(%folder_uid, %error, "publishing foreign-folder listing failed");
                    Errno::EIO
                })?;
            let mut snapshot = self.db.visible_children(&folder_uid).map_err(|error| {
                error!(%folder_uid, %error, "loading published foreign-folder listing failed");
                Errno::EIO
            })?;
            self.stamp_pending_sizes(&mut snapshot);
            snapshot.retain(|node| node_visible(node, &folder_uid, &hidden));
            let needs_size: Vec<NodeUid> = snapshot
                .iter()
                .filter(|node| {
                    matches!(
                        &node.kind,
                        NodeKind::File {
                            claimed_size: None,
                            ..
                        }
                    )
                })
                .map(|node| node.uid.clone())
                .collect();
            let mut st = self.state();
            for uid in &removed {
                st.hide_foreign_subtree(uid);
            }
            let children = snapshot
                .into_iter()
                .map(|node| st.intern_from_db(ino, node))
                .collect();
            st.children.insert(ino, children);
            drop(st);
            self.flush_access_changes();
            self.mark_shared_refresh_success(
                foreign_key
                    .as_deref()
                    .expect("foreign refresh has a freshness key"),
            );
            self.spawn_size_upgrade(ino, needs_size);
            return Ok(());
        }

        let preassigned = state::inos_for(&self.db, &filtered_nodes);
        let mut st = self.state();
        // Lost the race? Another thread already populated it.
        if st.children.contains_key(&ino) {
            return Ok(());
        }
        let mut child_inos = Vec::with_capacity(filtered_nodes.len());
        // Files whose real size the cheap enumeration could not read. Collected
        // before interning so the upgrade below has the uids without re-walking.
        let needs_size: Vec<NodeUid> = filtered_nodes
            .iter()
            .filter(|n| {
                matches!(
                    &n.kind,
                    NodeKind::File {
                        claimed_size: None,
                        ..
                    }
                )
            })
            .map(|n| n.uid.clone())
            .collect();
        let inos = st.intern_batch_with_inos(ino, filtered_nodes, preassigned);
        child_inos.extend(inos);
        st.children.insert(ino, child_inos);
        let root_snapshot = primary_root.then(|| RootListingSnapshot::capture(&st, ino).unwrap());
        // Record the listing as complete so a later restart (or a trimmed hot
        // cache) can rebuild it from the DB without the API. Written with the
        // children's rows when the lock goes, not under it.
        st.record_listed(folder_uid.clone());
        drop(st);
        self.flush_access_changes();
        if let Some(snapshot) = root_snapshot {
            self.reconcile_virtual_root_dentry(ino, snapshot)?;
        }
        self.spawn_size_upgrade(ino, needs_size);
        Ok(())
    }

    /// A page of the photos timeline (newest first), sliced out of the DB.
    /// `Ok(None)` when the account has no photos volume.
    ///
    /// Stale-while-revalidate: a persisted timeline is served *immediately*, and
    /// refreshed on the runtime if it is older than [`TIMELINE_TTL`] — so opening
    /// the app paints from disk rather than waiting on a full `enumerate_timeline`
    /// (which returns the whole timeline, not a page). Only an empty DB blocks on
    /// the network, i.e. the very first run.
    ///
    /// Metadata only: a thumbnail path is attached for photos whose thumbnail is
    /// already cached, and nothing is downloaded here — the front-end pulls the
    /// thumbnails it actually paints via [`Core::photo_thumbs`].
    pub(super) fn listing_stale(&self, key: &str, ttl: Duration) -> bool {
        match self.db.state_i64(key).ok().flatten() {
            Some(ms) => now_ms().saturating_sub(ms) >= ttl.as_millis() as i64,
            None => true,
        }
    }
}
