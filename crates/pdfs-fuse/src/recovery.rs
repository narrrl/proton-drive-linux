//! Startup: rebuilding the tree and the queue from the database, and
//! recovering writes that were fsynced but not queued when the daemon stopped.

use super::*;

impl Core {
    /// Rehydrate the in-memory `State` maps from the DB on mount, so a cold
    /// start serves previously-seen metadata (stable inodes, instant listings)
    /// without re-hitting the API. The root inode is already installed by
    /// [`ProtonFs::new`]; this fills in every other persisted node and rebuilds
    /// the child listings of folders the DB records as fully enumerated.
    pub(super) fn hydrate(&self) {
        let stored = match self.db.load_all() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "db load_all failed; mounting cold");
                return;
            }
        };
        if stored.is_empty() {
            return;
        }
        // A write queued by a previous run carries the optimistic size the file
        // grew to, which the sealed remote revision does not yet reflect. The DB
        // node still holds the pre-write `claimed_size` (often 0 for a fresh
        // file), so without this a queued 500 MB file `stat`s as 0 bytes until
        // the drain lands — reads still serve from the staged blob, but `ls`
        // lying about the size reads as data loss. `hydrate_pending` ran before
        // us, so the pending map is populated; snapshot it here rather than hold
        // both locks at once.
        let pending_sizes: HashMap<NodeUid, u64> = {
            let pending = self.pending.lock();
            pending
                .iter()
                .map(|(uid, pr)| (uid.clone(), pr.meta.len))
                .collect()
        };
        // `hydrate_pending` runs first and restores queued-trash tombstones.
        // Their node rows remain persisted as drain-time access authority, but
        // must not reappear in the mounted namespace after a restart.
        let hidden = self.hidden.lock().clone();
        let mut st = self.state();

        // Pass 1: give every uid its inode from its local id (the root is
        // already mapped).
        for sn in &stored {
            if hidden.contains(&sn.node.uid) || st.by_uid.contains_key(&sn.node.uid) {
                continue;
            }
            st.by_uid
                .insert(sn.node.uid.clone(), state::lid_ino(sn.lid));
        }

        // Pass 2: materialize entries, resolving each parent's inode by uid.
        // Track folders flagged complete so their listings rebuild in pass 3.
        let mut listed_dirs: Vec<u64> = Vec::new();
        for sn in stored {
            let StoredNode {
                mut node, listed, ..
            } = sn;
            if hidden.contains(&node.uid) {
                continue;
            }
            let Some(&ino) = st.by_uid.get(&node.uid) else {
                continue;
            };
            // Re-apply a queued write's optimistic size so `stat` matches what
            // reads (served from the staged blob) already return.
            if let Some(&len) = pending_sizes.get(&node.uid)
                && let NodeKind::File { claimed_size, .. } = &mut node.kind
            {
                *claimed_size = Some(len as i64);
            }
            if listed && node.is_folder() {
                listed_dirs.push(ino);
            }
            // A root entry is owned by `ProtonFs::new`; don't overwrite it.
            if st.roots.contains(&ino) {
                continue;
            }
            // A node whose parent row never made it to disk must not be adopted
            // by the root: it would surface as a phantom top-level entry.
            let parent = node
                .parent_uid
                .as_ref()
                .and_then(|p| st.by_uid.get(p).copied())
                .unwrap_or(ORPHAN_INO);
            let uid = node.uid.clone();
            st.entries.insert(
                ino,
                Entry {
                    uid,
                    parent,
                    node,
                    access: pdfs_core::Access::Unknown,
                    lookup_count: 1,
                    open_count: 0,
                    unlinked: false,
                },
            );
        }
        st.hydrate_access();

        // Pass 3: rebuild child listings for fully-enumerated folders.
        let listings = listed_children(&st.entries, listed_dirs);
        st.children.extend(listings);
        info!(nodes = st.entries.len(), "hydrated metadata cache from db");
    }

    /// Rebuild the in-memory pending map from the `pending_op` table on mount.
    ///
    /// A queued write survives a restart — that is the point of persisting it —
    /// so until the drain worker gets to it, reads of that file must still come
    /// from its staged blob rather than the remote's older revision.
    ///
    /// A row whose blob has gone missing is dropped: there is nothing left to
    /// upload, and keeping it would fail forever.
    pub(super) fn hydrate_pending(&self) {
        let ops = match self.db.pending_ops() {
            Ok(ops) => ops,
            Err(e) => {
                error!(error = %e, "loading pending uploads failed");
                return;
            }
        };
        let mut map = self.pending.lock();
        let mut restored = 0usize;
        for op in ops {
            let Some(uid) = parse_node_uid(&op.uid) else {
                error!(uid = %op.uid, id = op.id, "pending op has an unparseable uid; dropping");
                let _ = self.db.delete_op(op.id);
                continue;
            };
            if op.kind == OP_TRASH {
                self.hidden.lock().insert(uid.clone());
            }
            restored += 1;
            // Only a revision must have a blob. A create carries none until
            // something is written to it (`touch` offline is a legitimate op
            // with nothing to serve), and a rename never has one. A trash keeps
            // the blob of a create it withdrew only to find the node by, not
            // to serve. All still have to be replayed, so only the blob — if
            // any — is checked.
            if op.kind == OP_TRASH || (op.blob_path.is_none() && op.kind != OP_REVISION) {
                continue;
            }
            let parsed = op
                .meta_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<StagedWrite>(j).ok())
                .zip(op.blob_path.as_deref().map(PathBuf::from));
            let Some((meta, path)) = parsed else {
                error!(uid = %op.uid, id = op.id, "pending op is unreadable; dropping");
                self.drop_unrecoverable_op(&op, &uid);
                restored -= 1;
                continue;
            };
            if !path.exists() {
                error!(%uid, path = %path.display(), "staged blob is gone; dropping pending op");
                self.drop_unrecoverable_op(&op, &uid);
                restored -= 1;
                continue;
            }
            map.insert(uid, PendingRevision { path, meta });
        }
        if restored > 0 {
            info!(count = restored, "restored pending ops");
        }
    }

    /// Re-attach staged blobs that no queued op refers to.
    ///
    /// `staging/` holds the only copy of writes the kernel has already been
    /// told succeeded. Three release-path failures put bytes there and return
    /// `EIO` without leaving a `pending_op` row behind — an access change
    /// mid-release, a write overlapping an undrained incomplete edit, a create
    /// that drained out from under its own handle. Until now nothing ever
    /// walked the directory again: `hydrate_pending` reads the op table and
    /// [`recover_fsynced_writes`](Self::recover_fsynced_writes) reads
    /// `recovery/`, so an orphaned staged blob was invisible to the daemon and
    /// to the user alike, and stayed that way until someone found the file.
    ///
    /// Runs once at mount, after `hydrate_pending`, so `pending` already
    /// describes what the queue owns. An orphan whose sidecar names a real node
    /// is queued as an ordinary revision — the condition that made it fail is
    /// gone by definition, since the op it collided with has since drained.
    /// Anything that cannot be addressed (no sidecar, an unparseable uid, a
    /// `local~` placeholder whose create is gone) is left in place and named in
    /// the log: unexplained bytes are never evidence that user data is
    /// disposable.
    pub(super) fn reconcile_staging(&self) {
        let staged = self.cache.staged_writes();
        if staged.is_empty() {
            return;
        }
        let claimed = match self.db.op_blob_paths() {
            Ok(paths) => paths,
            Err(e) => {
                error!(error = %e, "cannot read queued blob paths; skipping staging reconcile");
                return;
            }
        };
        let mut queued = 0usize;
        let mut stranded = 0usize;
        let mut retired = 0usize;
        let mut retired_bytes = 0u64;
        let mut example: Option<PathBuf> = None;
        for (blob, meta) in staged {
            if claimed.contains(&blob.to_string_lossy().into_owned()) {
                continue;
            }
            let addressable = meta.as_ref().and_then(|m| parse_node_uid(&m.uid));
            let Some((meta, uid)) = meta.zip(addressable).filter(|(_, uid)| !is_local_uid(uid))
            else {
                if let Some(bytes) = orphan_past_retention(&blob) {
                    retired += 1;
                    retired_bytes += bytes;
                    warn!(blob = %blob.display(), bytes,
                          age_days = STAGING_ORPHAN_RETAIN.as_secs() / 86_400,
                          "retiring staged bytes that name no node and have outlived retention");
                    self.cache.discard_staged(&blob);
                    continue;
                }
                stranded += 1;
                example.get_or_insert_with(|| blob.clone());
                continue;
            };
            if self.pending.lock().contains_key(&uid) {
                // A newer write for the same node is queued and carries its own
                // blob. These bytes are older; keeping them is right, uploading
                // them over the newer ones is not.
                stranded += 1;
                warn!(%uid, blob = %blob.display(),
                      "staged bytes are superseded by a queued write; kept for recovery");
                continue;
            }
            if self
                .db
                .has_pending_op(&uid.to_string(), OP_TRASH)
                .unwrap_or(false)
            {
                // The node was deleted while a handle still read these bytes,
                // and the daemon stopped before its trash went out. Queued again,
                // they lost their blob when the trash landed and retried on it
                // for good (`docs/BUGS.md` B188).
                info!(%uid, blob = %blob.display(),
                      "staged bytes belong to a node whose trash is queued; discarded");
                self.cache.discard_staged(&blob);
                continue;
            }
            let op = PendingOp {
                id: 0,
                kind: OP_REVISION.to_string(),
                uid: uid.to_string(),
                parent_uid: None,
                name: None,
                blob_path: Some(blob.to_string_lossy().into_owned()),
                meta_json: Some(serde_json::to_string(&meta).unwrap_or_default()),
                created_at: now_millis(),
                attempts: 0,
                last_error: None,
                next_attempt_at: now_millis(),
            };
            match self.db.enqueue_op(&op) {
                Ok((_, superseded)) => {
                    if let Some(old) = superseded {
                        self.cache.discard_staged(Path::new(&old));
                    }
                    self.pending.lock().insert(
                        uid.clone(),
                        PendingRevision {
                            path: blob.clone(),
                            meta,
                        },
                    );
                    queued += 1;
                }
                Err(e) => {
                    stranded += 1;
                    error!(%uid, blob = %blob.display(), error = %e,
                           "cannot queue an orphaned staged write; bytes kept");
                }
            }
        }
        if stranded > 0 {
            // One line, not one per blob. A tree of them says exactly as much as
            // a single count and an example does, and said it on every mount:
            // 86 identical warnings for the same 86 files, boot after boot.
            warn!(
                count = stranded,
                example = %example.map(|b| b.display().to_string()).unwrap_or_default(),
                "staged bytes belong to no queued upload and name no node; kept for recovery"
            );
        }
        if queued > 0 || stranded > 0 || retired > 0 {
            info!(
                queued,
                stranded, retired, retired_bytes, "reconciled orphaned staged writes"
            );
        }
    }

    /// Queue the writes that an unclean shutdown caught between `fsync(2)` and
    /// `close(2)`.
    ///
    /// `fsync` promises the bytes survive a crash, but the queueing that makes a
    /// write outlive the daemon happens at `release`. A crash in between used to
    /// lose the data outright, because the scratch directory is cleared at open.
    /// Now `fsync` leaves a sidecar, `ContentCache::open` moves those blobs to
    /// `recovery/`, and this walks them into the same staging + queued-op path a
    /// normal release takes.
    ///
    /// Runs after [`hydrate_pending`](Self::hydrate_pending), which is what makes
    /// the incomplete-blob check in [`enqueue_staged_write`](Self::enqueue_staged_write)
    /// meaningful: a recovered partial write whose earlier write is still queued
    /// must not gap-fill from a remote revision that no longer describes the
    /// file. With `pending` already loaded, that case is detected and the bytes
    /// are parked rather than mis-filled.
    ///
    /// Failure is per-write and never fatal: a node that no longer exists, or an
    /// op that cannot be queued, leaves its blob in `recovery/` for the next run
    /// (and for a human) instead of taking the mount down.
    pub(super) fn recover_fsynced_writes(&self) {
        let recovered = self.cache.recovered_writes();
        if recovered.is_empty() {
            return;
        }
        let mut queued = 0usize;
        for (blob, meta) in recovered {
            let Some(uid) = parse_node_uid(&meta.uid) else {
                error!(uid = %meta.uid, "recovered write has an unparseable uid; keeping bytes");
                continue;
            };

            // If the write is incomplete and an earlier edit is still queued, merging it
            // now would use the wrong remote base. enqueue_staged_write would abandon it
            // to staging; keep it in recovery instead so the drain loop can queue it
            // once the queue clears.
            if !meta.complete && self.pending.lock().contains_key(&uid) {
                continue;
            }

            // The inode is only used to stamp the in-memory tree with the new
            // size, and nothing is interned this early — `hydrate` reads the
            // size back off `pending` when the node is first looked up, so 0
            // (no such inode) is correct rather than merely tolerable.
            match self.enqueue_staged_write(&uid, 0, &blob, meta) {
                Ok(()) => {
                    self.cache.discard_recovered(&blob);
                    queued += 1;
                }
                Err(e) => {
                    error!(%uid, blob = %blob.display(), error = ?e,
                           "cannot queue a recovered write; bytes kept for the next run");
                    // Some failures (a partial write parked by
                    // `stage_orphaned_write`) still consume the blob. Its sidecar
                    // would then describe nothing, so retire it — the bytes are
                    // in staging, which is where a human looks for them.
                    if !blob.exists() {
                        self.cache.discard_recovered(&blob);
                    }
                }
            }
        }
        if queued > 0 {
            info!(
                count = queued,
                "recovered fsynced writes from an unclean shutdown"
            );
        }
    }

    /// Discard an op that can never be performed, because the bytes it was to
    /// upload are gone from staging (something outside the daemon deleted them).
    ///
    /// For a node that only ever existed locally, the placeholder goes too. Its
    /// content is unrecoverable and nothing will ever mint it a real uid, so
    /// leaving the row would strand a file in the tree that can be listed but
    /// never read and never uploaded.
    pub(super) fn drop_unrecoverable_op(&self, op: &PendingOp, uid: &NodeUid) {
        let _ = self.db.delete_op(op.id);
        if is_local_uid(uid) {
            error!(%uid, name = op.name.as_deref().unwrap_or("?"),
                   "discarding a node whose only copy was lost");
            if let Err(e) = self.db.delete_node(uid) {
                warn!(%uid, error = %e, "db delete_node failed for lost local node");
            }
        }
    }
}
