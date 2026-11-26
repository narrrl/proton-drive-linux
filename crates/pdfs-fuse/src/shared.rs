//! The mounted shared tree's listing: the synthetic root, when its folders
//! are listed again, and what invalidates a listing on the wire.

use super::*;

/// Publication of the shared listing, and what invalidates a listing on the
/// wire or suppresses its next refresh.
#[derive(Clone)]
pub(crate) struct SharedState {
    /// Serializes only shared-list publication and access-loss invalidation.
    /// Network calls happen before taking it.
    pub(crate) shared_publication: Arc<Ranked<()>>,
    /// Invalidates in-flight shared responses when access/list events arrive.
    pub(crate) shared_generation: Arc<AtomicU64>,
    /// Successful refreshes suppress retries even if persisting their timestamp
    /// fails. Shared across primary/fork clones and cleared by access events.
    pub(crate) shared_refresh_deadlines: Arc<Mutex<SharedRefreshDeadlines>>,
}

impl SharedState {
    pub(crate) fn new() -> Self {
        Self {
            shared_publication: Arc::new(Ranked::new(Rank::SharedPublication, ())),
            shared_generation: Arc::new(AtomicU64::new(0)),
            shared_refresh_deadlines: Arc::new(Mutex::new(SharedRefreshDeadlines::default())),
        }
    }
}

impl Core {
    /// Read the inputs for virtual-root publication without changing SQLite.
    ///
    /// The caller validates the listing snapshot after these reads. Keeping all
    /// writes out of this phase prevents an invalidated/repopulated root listing
    /// from changing the synthetic node's persisted visibility or FTS ancestry.
    pub(super) fn prepare_virtual_root(
        &self,
        real_names: &HashSet<String>,
    ) -> Result<VirtualRootPlan, Errno> {
        let pinned = self.db.state_str(SHARED_WITH_ME_NAME).map_err(|error| {
            error!(%error, "reading shared-root display name failed");
            Errno::EIO
        })?;
        let (name, visible) = virtual_root_name(real_names, pinned.as_deref());

        let uid = shared_with_me_uid();
        let previous_node = self.db.node_by_uid(&uid.to_string()).map_err(|error| {
            error!(%error, "reading synthetic shared root failed");
            Errno::EIO
        })?;
        let mut node = previous_node.unwrap_or_else(|| {
            virtual_node(self.primary_root_uid.clone(), name.clone(), now_secs())
        });
        node.parent_uid = Some(self.primary_root_uid.clone());
        node.name = name;
        node.kind = NodeKind::Folder;
        // A later real-name collision suppresses both the dentry and its search
        // hit. The row and pinned name remain, and clearing the collision
        // restores both by writing `trashed = false` again.
        node.trashed = !visible;
        node.membership = None;
        Ok(VirtualRootPlan { node, visible })
    }

    /// Reconcile the synthetic dentry against an already-known primary-root
    /// listing. The node remains persisted and pinned even while a real folder
    /// of the same name temporarily suppresses its dentry.
    pub(super) fn reconcile_virtual_root_dentry(
        &self,
        parent_ino: u64,
        snapshot: RootListingSnapshot,
    ) -> Result<(), Errno> {
        let _publication = self.shared.shared_publication.lock();
        let plan = self.prepare_virtual_root(&snapshot.real_names())?;
        let mut st = self.state();
        // State methods already establish the state -> DB lock order. No path
        // holds the DB connection while acquiring state, including shared event
        // publication, so keeping state locked across this one transaction
        // closes the crash window without introducing an inverse order.
        publish_virtual_root_in_listing(&self.db, &mut st, parent_ino, &snapshot, plan)?;
        drop(st);
        self.flush_access_changes();
        Ok(())
    }

    pub(super) fn resident_root_listing_snapshot(
        &self,
        ino: u64,
    ) -> Result<RootListingSnapshot, Errno> {
        RootListingSnapshot::capture(&self.state(), ino).ok_or(Errno::EAGAIN)
    }

    pub(super) fn shared_folder_freshness_key(uid: &NodeUid) -> String {
        format!("{SHARED_FOLDER_SYNCED_PREFIX}{uid}")
    }

    pub(super) fn shared_listing_stale(&self, key: &str) -> bool {
        if self
            .shared
            .shared_refresh_deadlines
            .lock()
            .is_fresh(key, Instant::now())
        {
            return false;
        }
        self.listing_stale(key, SHARED_LISTING_TTL)
    }

    pub(super) fn mark_shared_refresh_success(&self, key: &str) {
        self.shared
            .shared_refresh_deadlines
            .lock()
            .mark(key, Instant::now(), SHARED_LISTING_TTL);
        if let Err(error) = self.db.set_state_i64(key, now_millis()) {
            warn!(key, %error, "persisting shared-list freshness failed");
        }
    }

    pub(super) fn invalidate_shared_refreshes(&self) {
        self.shared.shared_generation.fetch_add(1, Ordering::SeqCst);
        self.shared.shared_refresh_deadlines.lock().clear();
    }

    pub(super) fn ensure_shared_children(&self, ino: u64) -> Result<(), Errno> {
        let online = self.is_online();
        let resident = self.state().children.contains_key(&ino);
        match shared_listing_plan(
            resident,
            online,
            self.shared_listing_stale(SHARED_WITH_ME_SYNCED_MS),
        ) {
            SharedListingPlan::Resident => return Ok(()),
            SharedListingPlan::Persisted => {
                // A completed snapshot remains useful offline even after an
                // event expired its freshness stamp.
                let nodes = self
                    .db
                    .visible_children(&shared_with_me_uid())
                    .map_err(|error| {
                        error!(%error, "loading persisted shared roots failed");
                        Errno::EIO
                    })?;
                let mut st = self.state();
                if st.children.contains_key(&ino) {
                    return Ok(());
                }
                let children = nodes
                    .into_iter()
                    .map(|node| {
                        let access = st
                            .share_access
                            .get(&node.uid)
                            .copied()
                            .unwrap_or(Access::Viewer);
                        st.intern_published_share_root(ino, node, access)
                    })
                    .collect();
                st.children.insert(ino, children);
                drop(st);
                self.flush_access_changes();
                return Ok(());
            }
            SharedListingPlan::Refresh => {}
        }

        let generation = self.shared.shared_generation.load(Ordering::SeqCst);
        let accepted = accepted_share_provenance(
            self.block_on_within(BATCH_CALL_TIMEOUT, self.client.enumerate_shared_with_me())
                .map_err(|error| {
                    error!(%error, "enumerating shared roots failed");
                    Errno::EIO
                })?,
        );
        let nodes = if accepted.uids.is_empty() {
            Vec::new()
        } else {
            self.block_on_within(
                BATCH_CALL_TIMEOUT,
                self.drive.enumerate_nodes_light(&accepted.uids),
            )
            .map_err(|error| {
                error!(%error, "materializing shared roots failed");
                Errno::EIO
            })?
        };
        let virtual_uid = shared_with_me_uid();
        let published = prepare_shared_roots(&accepted, nodes, &virtual_uid);

        let _publication = self.shared.shared_publication.lock();
        if !refresh_generation_is_current(
            generation,
            self.shared.shared_generation.load(Ordering::SeqCst),
        ) {
            return Err(Errno::EAGAIN);
        }
        let removed = match self
            .db
            .publish_shared_roots(&virtual_uid, &accepted.uids, &published)
        {
            Ok(removed) => removed,
            Err(error) => {
                error!(%error, "publishing shared-root listing failed");
                let mut st = self.state();
                for uid in &accepted.uids {
                    st.downgrade_shared_subtree(uid);
                }
                drop(st);
                self.flush_access_changes();
                return Err(Errno::EIO);
            }
        };
        let snapshot = self.db.visible_children(&virtual_uid).map_err(|error| {
            error!(%error, "loading published shared-root listing failed");
            Errno::EIO
        })?;
        let published_access: HashMap<NodeUid, Access> = published
            .iter()
            .map(|root| (root.node.uid.clone(), root.access))
            .collect();
        let mut st = self.state();
        for uid in &removed {
            st.hide_shared_root(uid);
        }
        let mut children = Vec::with_capacity(snapshot.len());
        for node in snapshot {
            let uid = node.uid.clone();
            let access = published_access
                .get(&uid)
                .copied()
                .unwrap_or(Access::Viewer);
            let child = st.intern_published_share_root(ino, node, access);
            children.push(child);
        }
        st.children.insert(ino, children);
        drop(st);
        self.flush_access_changes();
        self.mark_shared_refresh_success(SHARED_WITH_ME_SYNCED_MS);
        if let Some(notifier) = self.session.notifier.get() {
            let _ = notifier.inval_inode(ino);
        }
        Ok(())
    }
}
