//! Turning what the mount was asked into queued ops: revisions, truncates,
//! creates, renames and trashes, and taking queued ops back again.

use super::*;

impl Core {
    /// Queue what the write handles the kernel never released hold.
    ///
    /// `close(2)` returns before the kernel sends `release`, and the stop
    /// aborts the FUSE connection with the releases still queued: a file a
    /// program had just saved was never queued, and Drive kept it as it was
    /// (`docs/BUGS.md` B190). Runs once every session has ended, so no release
    /// can race it. A handle still open in a program goes the same way, with
    /// what it wrote so far; one on an unlinked file only leaves its scratch.
    pub(crate) fn queue_unreleased_writes(&self) {
        let handles: Vec<(WriteHandle, bool)> = {
            let mut st = self.state();
            let writers = st.open.take_all();
            writers
                .into_iter()
                .map(|h| {
                    let unlinked = st.entries.get(&h.ino).is_some_and(|e| e.unlinked);
                    (h, unlinked)
                })
                .collect()
        };
        for (mut h, unlinked) in handles {
            if unlinked {
                self.cache.clear_scratch_durable(&h.path);
                let _ = std::fs::remove_file(&h.path);
                continue;
            }
            self.follow_landed_create(&mut h);
            match self.queue_revision(&mut h) {
                Ok(()) => info!(uid = %h.uid, "queued a write the kernel never released"),
                Err(error) => {
                    warn!(uid = %h.uid, ?error, "cannot queue a write the kernel never released")
                }
            }
        }
    }

    /// Accept a released write handle's bytes and queue their upload.
    ///
    /// This is what makes a copy into the mount run at disk speed: the caller's
    /// `close` returns once the bytes are staged on local disk and the intent is
    /// in `pending_op`, instead of waiting out a full upload inside the FUSE
    /// handler. It is also what makes an offline write succeed rather than EIO —
    /// the queued op simply waits for the network.
    ///
    /// The scratch file is *moved* into staging, never copied: it is the only
    /// copy of what the user wrote.
    pub(super) fn queue_revision(&self, h: &mut WriteHandle) -> Result<(), Errno> {
        // Closing an untouched writable handle is not a mutation. It remains
        // valid after a downgrade and only owes cleanup of its empty scratch.
        if !h.dirty {
            self.cache.clear_scratch_durable(&h.path);
            let _ = std::fs::remove_file(&h.path);
            // A new file's create waits for this release to wake the drain
            // (`Core::queue_local_node`). Closed empty, it attached nothing
            // that would, and sat until the idle poll (`docs/BUGS.md` B192).
            if is_local_uid(&h.uid) {
                self.wake_drain();
            }
            return Ok(());
        }
        preserve_on_access_denied(self.require_uid_writable(&h.uid), h.dirty, || {
            let meta = self.recovery_meta(h);
            self.stage_orphaned_write(
                &h.uid,
                h.ino,
                &h.path,
                &meta,
                "access changed before the write could be queued",
            );
        })?;
        // The handle is being retired either way, so any durability sidecar an
        // `fsync` left has done its job: from here the bytes are tracked as a
        // staged write and a queued op, and a sidecar outliving them would offer
        // recovery a second, stale copy of the same write.
        self.cache.clear_scratch_durable(&h.path);
        if is_local_uid(&h.uid) && !self.db.has_create_op(&h.uid.to_string()).unwrap_or(true) {
            // A create that landed since the handle last looked left no op
            // either. It moved the row to its uid in the same transaction, so
            // the row says which it was (`docs/BUGS.md` B176).
            self.follow_landed_create(h);
            if is_local_uid(&h.uid) {
                debug!(uid = %h.uid, "local node was unlinked before creation; dropping revision");
                let _ = std::fs::remove_file(&h.path);
                return Ok(());
            }
        }
        // Materialize as much of the full content as is free to materialize. A
        // complete blob is uploadable without the network and lets a later write
        // to the same file supersede this one without any reconstruction at all.
        //
        // What is deliberately *not* done here is fetching the untouched ranges
        // from the remote: that is a download of everything the edit did not
        // touch, and this runs on the FUSE dispatch loop, so closing a large
        // file after a small edit stopped every other request on the mount for
        // the length of it (audit F1). An incomplete blob is first-class —
        // `read_pending` resolves its gaps against the base, and `drain_revision`
        // fills them on the drain thread — so the network half of the fill
        // belongs there, and only there.
        //
        // A node that exists only locally has no remote base to fill from: its
        // untouched ranges live in the blob queued by an earlier write, which
        // `merge_over_pending` folds in below.
        if let Err(e) = h.file.set_len(h.len) {
            error!(uid = %h.uid, error = %e, "resize scratch file failed");
            return Err(Errno::EIO);
        }
        let mut written = h.written.clone();
        if !(is_local_uid(&h.uid) && h.base_size > 0) {
            // A local node has no remote base, so nothing of it can be fetched
            // or cached: passing zero says so, and leaves the fill doing only
            // the part that still applies — claiming the zeroed tail.
            let base_size = if is_local_uid(&h.uid) { 0 } else { h.base_size };
            self.fill_gaps_cached(
                &h.uid,
                &h.file,
                h.len,
                h.base_mtime,
                base_size,
                &mut written,
            );
        }
        let authored: Vec<(u64, u64)> = written
            .segments(0, h.len)
            .into_iter()
            .filter(|&(_, _, authored)| authored)
            .map(|(s, e, _)| (s, e))
            .collect();
        let meta = StagedWrite {
            uid: h.uid.to_string(),
            len: h.len,
            base_size: h.base_size,
            base_mtime: h.base_mtime,
            complete: StagedWrite::covers(&authored, h.len),
            authored,
            based_on: self.remote_baseline(
                &h.uid,
                h.base_mtime,
                h.base_size,
                h.base_revision_id.clone(),
            ),
        };
        let complete = meta.complete;
        self.enqueue_staged_write(&h.uid, h.ino, &h.path, meta)?;
        debug!(uid = %h.uid, len = h.len, complete, "queued revision upload");
        Ok(())
    }

    /// Describe the locally authored ranges without gap-filling them.
    ///
    /// Used when a write was accepted by the kernel but a queue-time access
    /// check now denies it. Building this metadata is side-effect free, so the
    /// permission guard still precedes cache, database, and tree mutation.
    pub(super) fn recovery_meta(&self, h: &WriteHandle) -> StagedWrite {
        let authored: Vec<(u64, u64)> = h
            .written
            .segments(0, h.len)
            .into_iter()
            .filter(|&(_, _, authored)| authored)
            .map(|(start, end, _)| (start, end))
            .collect();
        StagedWrite {
            uid: h.uid.to_string(),
            len: h.len,
            base_size: h.base_size,
            base_mtime: h.base_mtime,
            complete: StagedWrite::covers(&authored, h.len),
            authored,
            based_on: self.remote_baseline(
                &h.uid,
                h.base_mtime,
                h.base_size,
                h.base_revision_id.clone(),
            ),
        }
    }

    /// The remote revision a change to `uid` is being made against, for
    /// [`StagedWrite::based_on`].
    ///
    /// Normally that is simply the base the handle opened over. The exception is
    /// a write that supersedes a still-queued one: its "base" is the previous
    /// *staged blob*, whose size and mtime are ours, not the server's — so the
    /// baseline is inherited from the op being superseded, which is the last one
    /// that actually observed the remote. Without that, chaining two writes
    /// before the queue drains would leave the drain comparing the remote
    /// against a revision it never had, and cutting a conflict copy over
    /// nothing.
    ///
    /// `None` for a node that has never existed remotely: there is no revision
    /// to conflict with until its create drains.
    pub(super) fn remote_baseline(
        &self,
        uid: &NodeUid,
        base_mtime: i64,
        base_size: u64,
        base_revision_id: Option<String>,
    ) -> Option<Baseline> {
        if is_local_uid(uid) {
            return None;
        }
        match self.writes.pending.lock().get(uid) {
            Some(p) => p.meta.based_on.clone(),
            None => Some(Baseline {
                mtime: base_mtime,
                size: base_size,
                hash: None,
                revision_id: base_revision_id,
            }),
        }
    }

    /// Move a file holding a node's intended new content into staging and queue
    /// the upload that will make it the remote's content. Shared by the release
    /// of a write handle and by a path-based truncate.
    ///
    /// `src` is consumed either way: on success it is *moved* into staging, and
    /// on the refusal below it is moved there too, just without an op to upload
    /// it. It is the only copy of what the user wrote, so nothing here may
    /// simply delete it.
    pub(super) fn enqueue_staged_write(
        &self,
        uid: &NodeUid,
        ino: u64,
        src: &Path,
        mut meta: StagedWrite,
    ) -> Result<(), Errno> {
        preserve_on_access_denied(self.require_uid_writable(uid), true, || {
            self.stage_orphaned_write(
                uid,
                ino,
                src,
                &meta,
                "access changed before the staged write could be queued",
            );
        })?;
        // An incomplete blob's gaps refer to the *remote* base. If an earlier
        // write to this file is still queued, the remote no longer holds that
        // base — the previous staged blob does — so taking those gaps at face
        // value would fill them from the wrong revision. Fold the blob being
        // superseded into this one instead: its authored bytes are a local copy
        // of exactly the ranges this write did not touch, and what neither wrote
        // is still the remote content that the earlier write was based on, which
        // is why the baseline is inherited along with them.
        let previous = self.writes.queued(uid);
        if !meta.complete
            && let Some(previous) = previous
            && let Err(error) = merge_over_pending(&mut meta, src, &previous)
        {
            // Without the earlier bytes the gaps cannot be resolved against
            // anything, so this is where the write stops being uploadable.
            // Keep it recoverable rather than corrupting the file.
            warn!(%uid, %error, "folding in the undrained edit failed");
            self.stage_orphaned_write(
                uid,
                ino,
                src,
                &meta,
                "write overlaps an undrained incomplete edit",
            );
            return Err(Errno::EIO);
        }
        let path = self.cache.stage_write(&meta, src).map_err(|e| {
            error!(%uid, error = %e, "staging write failed");
            // The scratch file is still the only copy of these bytes, and the
            // caller is releasing the handle it belongs to — without a
            // durability sidecar `discard_unmarked_scratch` deletes it at the
            // next mount, the one place the "never delete on a failure path"
            // invariant would break. Mark it first, whatever went wrong.
            if let Err(mark) = self.cache.mark_scratch_durable(src, &meta) {
                error!(%uid, error = %mark,
                       "could not mark an unstageable write durable; bytes at risk");
            }
            if e.is_disk_full() {
                // Free cached blobs so the retry has somewhere to land, and tell
                // the caller what is actually wrong.
                self.cache.emergency_evict();
                Errno::ENOSPC
            } else {
                Errno::EIO
            }
        })?;
        // Whatever this write supersedes, it supersedes now — including an
        // upload already on the wire, whose bytes describe a revision the row
        // written just below replaces. Signalled before the enqueue so the
        // in-flight reader stops at its next block rather than after however
        // much of the file is left.
        self.cancel_upload(uid);
        let mut uid = uid.clone();
        let len = meta.len;
        let superseded = 'queued: {
            if is_local_uid(&uid) {
                // The node has no server-side identity to hang a revision on, so
                // the bytes ride on the create that will mint it. Attached and
                // recorded under one hold of `pending`, which is what the drain
                // takes to retire a landed create: it either sees this blob and
                // carries it over, or has already finished and this write
                // queues against the real uid below (`Core::retire_create`).
                let meta_json = serde_json::to_string(&meta).unwrap_or_default();
                let mut pending = self.writes.pending.lock();
                let attached = self
                    .db
                    .attach_blob_to_create(&uid.to_string(), &path.to_string_lossy(), &meta_json)
                    .map_err(|e| {
                        error!(%uid, error = %e, "attaching write to queued create failed");
                        Errno::EIO
                    })?;
                if let Some(attached) = attached {
                    pending.insert(uid.clone(), PendingRevision { path, meta });
                    break 'queued attached.superseded;
                }
                drop(pending);
                // The create landed while this handle was open, and its row
                // already has the uid Drive gave it.
                let Some(real) = self.landed_row_uid(ino) else {
                    error!(%uid, staged = %path.display(),
                           "queued create vanished under a write; bytes kept in staging");
                    return Err(Errno::EIO);
                };
                debug!(local = %uid, %real, "create landed under an open write; queueing a revision");
                uid = real;
                meta.uid = uid.to_string();
                let previous = self.writes.queued(&uid);
                meta.based_on = previous.as_ref().and_then(|p| p.meta.based_on.clone());
                if !meta.complete
                    && let Some(previous) = previous
                    && let Err(error) = merge_over_pending(&mut meta, &path, &previous)
                {
                    error!(%uid, %error, staged = %path.display(),
                           "folding in the write the create carried failed; bytes kept in staging");
                    return Err(Errno::EIO);
                }
            }
            let op = PendingOp {
                id: 0,
                kind: OP_REVISION.to_string(),
                uid: uid.to_string(),
                parent_uid: None,
                name: None,
                blob_path: Some(path.to_string_lossy().into_owned()),
                meta_json: Some(serde_json::to_string(&meta).unwrap_or_default()),
                created_at: now_millis(),
                attempts: 0,
                last_error: None,
                next_attempt_at: now_millis() + self.revision_debounce(&uid).as_millis() as i64,
            };
            let (_id, superseded) = self.db.enqueue_op(&op).map_err(|e| {
                error!(%uid, error = %e, "queueing upload failed");
                Errno::EIO
            })?;
            self.writes
                .pending
                .lock()
                .insert(uid.clone(), PendingRevision { path, meta });
            superseded
        };
        if let Some(old) = superseded {
            self.cache.discard_staged(Path::new(&old));
        }
        let uid = &uid;
        // Reflect the write in the tree straight away: `ls` must show the new
        // size and mtime even though the remote still holds the old revision.
        let now = now_secs();
        let updated = {
            let mut st = self.state();
            st.record_pending_write(ino, len, now)
        };
        // The only record that the file is as long as the caller was told. If it
        // cannot be written the write did not fully take, and saying so beats
        // acknowledging a `close(2)` and then serving the old size — the bytes
        // themselves are already staged and queued either way. Written out here
        // rather than inside `State` so the commit does not hold the inode lock.
        let recorded = match updated {
            Some(node) => self.db.upsert_node(&node),
            None => Ok(()),
        };
        // Cached blobs and open readers describe the superseded revision. Reads
        // come from the staged blob until the op drains, so just drop them.
        // Done even when the row above failed: the staged blob is the file's
        // content now either way, and leaving a stale one cached would serve it.
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.wake_drain();
        if let Err(e) = recorded {
            error!(%uid, error = %e, "recording a queued write's size failed");
            return Err(if e.is_disk_full() {
                Errno::ENOSPC
            } else {
                Errno::EIO
            });
        }
        Ok(())
    }

    /// Queue the new content of a path-based truncate — `> file`, or any
    /// `setattr(size=…)` arriving without a write handle.
    ///
    /// No bytes have been authored at truncate time, which is why this path was
    /// never staged and instead resized the remote content inline. That is also
    /// why a shell redirect failed offline *before* the write that follows it:
    /// the truncate itself needed the network. Staging a blob describing the
    /// result puts it on the same queue as every other write.
    ///
    /// The blob is a hole of the new length; what is real about it is `authored`:
    ///
    /// - `> file` (size 0) is the whole point and needs nothing at all — an
    ///   empty file is complete content, so it queues and drains offline.
    /// - Extending past the end authors the new tail (zeros, by definition) and
    ///   leaves the head to be gap-filled from the base.
    /// - Shrinking authors nothing: every remaining byte still comes from the
    ///   base, so it is the drain that has to fetch it.
    pub(super) fn queue_truncate(&self, ino: u64, size: u64) -> Result<(), Errno> {
        let (uid, base_mtime, base_size, base_revision_id) = {
            let st = self.state();
            match st.entries.get(&ino) {
                Some(e) if e.node.is_file() => (
                    e.uid.clone(),
                    e.node.modification_time,
                    node_size(&e.node),
                    node_revision_id(&e.node),
                ),
                Some(_) => return Err(Errno::EISDIR),
                None => return Err(Errno::ENOENT),
            }
        };
        self.require_uid_writable(&uid)?;
        if size == base_size {
            return Ok(());
        }
        // A close immediately followed by truncate is common (`write`, close,
        // `truncate(2)`) and usually beats the write-back debounce. In that
        // case the remote is *not* our base: the complete staged revision is.
        // Compose from those local bytes so the truncate can supersede the
        // queued upload without fetching stale remote content or returning EIO.
        let pending_base = self.writes.queued(&uid);
        if pending_base
            .as_ref()
            .is_some_and(|pending| !pending.meta.complete)
        {
            // An incomplete pending blob still has holes referring to the
            // remote revision. Stacking another incomplete transform on it is
            // not representable safely; preserve the existing refusal.
            return Err(Errno::EIO);
        }
        let (authored, complete) = if size == 0 {
            // An empty file has no content to be missing.
            (Vec::new(), true)
        } else if base_size == 0 {
            // Nothing to gap-fill from: every byte is a zero this truncate
            // defines.
            (vec![(0, size)], true)
        } else if size > base_size {
            (vec![(base_size, size)], false)
        } else {
            (Vec::new(), false)
        };
        let (file, path) = self.cache.create_scratch().map_err(|e| {
            error!(%uid, error = %e, "create scratch file for truncate failed");
            Errno::EIO
        })?;
        if let Some(pending) = &pending_base
            && let Err(e) = copy_pending_for_truncate(pending, &file)
        {
            error!(%uid, source = %pending.path.display(), error = %e,
                "copy pending revision for truncate failed");
            let _ = std::fs::remove_file(&path);
            return Err(Errno::EIO);
        }
        file.set_len(size).map_err(|e| {
            error!(%uid, error = %e, "resize scratch file for truncate failed");
            let _ = std::fs::remove_file(&path);
            Errno::EIO
        })?;
        let meta = match pending_base {
            Some(pending) => StagedWrite {
                uid: uid.to_string(),
                len: size,
                base_size: pending.meta.len,
                base_mtime: pending.meta.base_mtime,
                authored: if size == 0 {
                    Vec::new()
                } else {
                    vec![(0, size)]
                },
                complete: true,
                // Both local revisions descend from the same last-observed
                // remote revision. Never replace this with the optimistic
                // mtime/size currently published by State.
                based_on: pending.meta.based_on,
            },
            None => StagedWrite {
                uid: uid.to_string(),
                len: size,
                base_size,
                base_mtime,
                authored,
                complete,
                based_on: self.remote_baseline(&uid, base_mtime, base_size, base_revision_id),
            },
        };
        self.enqueue_staged_write(&uid, ino, &path, meta)?;
        debug!(%uid, size, complete, "queued truncate");
        Ok(())
    }

    /// Invent a node under `parent_uid` and queue the op that will make it real.
    /// Returns the node to intern, exactly as the online
    /// path returns the one the server minted.
    ///
    /// The parent may itself be a placeholder — `mkdir -p` offline, or `cp -r` of
    /// a tree. That is fine: the op records the parent it was made under by local
    /// id, and is not sent until the parent's row has the uid it landed as.
    pub(super) fn queue_local_node(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        is_dir: bool,
        hold: bool,
    ) -> Result<Node, Errno> {
        self.require_uid_writable(parent_uid)?;
        // The row the tree would give it anyway, now: the node goes by its
        // local id until it lands.
        let lid = self
            .db
            .add_local_row(&parent_uid.to_string(), name, is_dir, now_secs())
            .map_err(|e| {
                error!(%parent_uid, name, error = %e, "giving a local node its row failed");
                Errno::EIO
            })?;
        let uid = local_node_uid(lid);
        let node = local_node(uid.clone(), parent_uid.clone(), name.to_string(), is_dir);
        let op = PendingOp {
            id: 0,
            kind: if is_dir { OP_MKDIR } else { OP_CREATE }.to_string(),
            uid: uid.to_string(),
            parent_uid: Some(parent_uid.to_string()),
            name: Some(name.to_string()),
            blob_path: None,
            meta_json: None,
            created_at: now_millis(),
            attempts: 0,
            last_error: None,
            // Parked when the name is transient: its bytes ride on this create
            // (via `attach_blob_to_create`) and must not upload until a rename to
            // the finished name un-parks it (docs/BUGS.md B70).
            next_attempt_at: if hold { PARK_UNTIL } else { 0 },
        };
        self.db.enqueue_op(&op).map_err(|e| {
            error!(%parent_uid, name, error = %e, "queueing local node failed");
            Errno::EIO
        })?;
        debug!(%uid, %parent_uid, name, is_dir, "created node offline; queued");
        // A folder is due now; without this the workers slept through it to
        // the idle poll (B160). A file is not: its bytes ride on the create, and
        // its release wakes the drain, with bytes or without (B192).
        if is_dir {
            self.wake_drain();
        }
        Ok(node)
    }

    /// Queue giving a node a new parent and/or name, and apply it to the tree
    /// now.
    ///
    /// The op records the desired end state rather than the step, so it both
    /// supersedes any earlier queued rename and lets the drain skip whichever
    /// half the remote already agrees with.
    ///
    /// `new_parent_uid` may be a placeholder — moving a file into a folder
    /// created offline — which is why this cannot simply be the online call with
    /// a retry around it: the API would 404 on a `local~` parent. The op waits
    /// for that folder's drain to rewrite it, exactly as a queued create does.
    pub(super) fn queue_rename(
        &self,
        ino: u64,
        uid: &NodeUid,
        old_parent_uid: &NodeUid,
        new_parent_ino: u64,
        new_parent_uid: &NodeUid,
        new_name: &str,
    ) -> Result<(), Errno> {
        self.require_uid_writable(uid)?;
        self.require_uid_writable(old_parent_uid)?;
        self.require_uid_writable(new_parent_uid)?;
        self.queue_rename_authorized(
            ino,
            uid,
            old_parent_uid,
            new_parent_ino,
            new_parent_uid,
            new_name,
        )
    }

    /// Enqueue a rename whose access was linearized immediately before a
    /// replacement victim was removed. Rechecking here could reject after that
    /// destructive half had already completed.
    pub(super) fn queue_rename_authorized(
        &self,
        ino: u64,
        uid: &NodeUid,
        old_parent_uid: &NodeUid,
        new_parent_ino: u64,
        new_parent_uid: &NodeUid,
        new_name: &str,
    ) -> Result<(), Errno> {
        self.enqueue_rename(ino, uid, old_parent_uid, new_parent_uid, new_name)?;
        self.state
            .lock()
            .rename_in_place(ino, new_parent_ino, new_parent_uid, new_name);
        self.wake_drain();
        debug!(%uid, %new_parent_uid, new_name, "renamed offline; queued");
        Ok(())
    }

    /// Record the queued rename op, and nothing else: the caller moves the node
    /// in the tree, through the kernel or around it.
    pub(super) fn enqueue_rename(
        &self,
        ino: u64,
        uid: &NodeUid,
        old_parent_uid: &NodeUid,
        new_parent_uid: &NodeUid,
        new_name: &str,
    ) -> Result<(), Errno> {
        // A rename that replaces a queued one keeps where the node started.
        let meta = match self.db.pending_op_meta(&uid.to_string(), OP_RENAME) {
            Ok(Some(json)) => serde_json::from_str::<RenameMeta>(&json).map_err(|error| {
                error!(%uid, %error, "queued rename authority metadata is invalid");
                Errno::EIO
            })?,
            Ok(None) => RenameMeta {
                original_parent_uid: old_parent_uid.to_string(),
                original_name: self
                    .state
                    .lock()
                    .entries
                    .get(&ino)
                    .map(|entry| entry.node.name.clone()),
            },
            Err(error) => {
                error!(%uid, %error, "reading queued rename authority failed");
                return Err(Errno::EIO);
            }
        };
        let meta_json = serde_json::to_string(&meta).map_err(|error| {
            error!(%uid, %error, "serializing queued rename authority failed");
            Errno::EIO
        })?;
        let op = PendingOp {
            id: 0,
            kind: OP_RENAME.to_string(),
            uid: uid.to_string(),
            parent_uid: Some(new_parent_uid.to_string()),
            name: Some(new_name.to_string()),
            blob_path: None,
            meta_json: Some(meta_json),
            created_at: now_millis(),
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };
        self.db.enqueue_op(&op).map_err(|e| {
            error!(%uid, new_name, error = %e, "queueing rename failed");
            Errno::EIO
        })?;
        Ok(())
    }

    pub(super) fn has_pending_moves_out_of(&self, uid: &NodeUid) -> Result<bool, Errno> {
        self.db
            .has_pending_moves_out_of(&uid.to_string())
            .map_err(|error| {
                error!(%uid, %error, "checking for moves out of a trash target failed");
                Errno::EIO
            })
    }

    /// Queue trashing a node the server knows about, and drop it from the tree
    /// now.
    ///
    /// Anything else queued for this node is discarded first: the user has said
    /// the file should not exist, so uploading bytes to it or renaming it are
    /// both work towards an outcome nobody wants any more. That does throw away
    /// staged bytes that never landed — which is precisely what deleting an
    /// un-uploaded file means, and the alternative (upload it, then trash it) is
    /// worse in every way.
    ///
    /// A file still open keeps the bytes its handle reads, the queued ones and
    /// the cached ones, as when the trash is sent at once: a partial write
    /// fills its gaps from them, offline too (`docs/BUGS.md` B186). The final
    /// close or the trash landing, whichever comes last, lets go of them.
    ///
    /// `open` is whether the caller found the file open. A create that landed
    /// under a delete is trashed by its real uid before the tree has it, so
    /// asking the tree alone took an open file for closed (`docs/BUGS.md`
    /// B257).
    pub(super) fn queue_trash(&self, uid: &NodeUid, name: &str, open: bool) -> Result<(), Errno> {
        self.require_uid_writable(uid)?;
        // The bytes below are about to be discarded, and a drain worker may be
        // reading them onto the wire right now. Stop it first: with several
        // workers the trash op this queues can be claimed while that upload is
        // still running, and the two would race to decide whether the file
        // exists.
        self.cancel_upload(uid);
        let (_, blobs) = self
            .db
            .replace_ops_with_trash(&uid.to_string(), name, now_millis())
            .map_err(|e| {
                error!(%uid, error = %e, "queueing trash failed");
                Errno::EIO
            })?;
        let open_now = open || self.is_open_anywhere(uid);
        if open_now {
            let read_from = self.pending_blob(uid);
            self.release_dropped_below(
                blobs
                    .into_iter()
                    .filter(|blob| read_from.as_deref() != Some(Path::new(blob)))
                    .collect(),
            );
        } else {
            self.release_dropped_ops(uid, blobs);
        }
        self.writes.hide(uid);
        // Withdrawn from the tree, which every mount serves from, so no other
        // mount goes on serving a file the user just trashed (`docs/BUGS.md`
        // B74).
        self.for_each_state(|st| {
            st.unlink_mem(uid);
        });
        if !open_now {
            self.cache.evict(uid);
            self.evict_reader(uid);
        }
        self.wake_drain();
        debug!(%uid, name, "trashed offline; queued");
        Ok(())
    }

    /// Drop every op queued against a node, and the staged bytes they own.
    pub(super) fn discard_queued_ops(&self, uid: &NodeUid) -> Result<(), Errno> {
        // Same reason as `queue_trash`: an upload may be reading the very blobs
        // this is about to unlink.
        self.cancel_upload(uid);
        let blobs = self.db.delete_ops_for_uid(&uid.to_string()).map_err(|e| {
            error!(%uid, error = %e, "dropping queued ops failed");
            Errno::EIO
        })?;
        self.release_dropped_ops(uid, blobs);
        Ok(())
    }

    /// Drop every op queued against a node that just left the namespace while
    /// a handle still has it open, but keep the bytes that handle reads.
    ///
    /// The ops cannot wait for the final close: the node is already trashed, and
    /// a revision the drain sends in the meantime lands on it after the trash —
    /// an editor's save loop then found the name it was renaming onto still
    /// taken (`docs/BUGS.md` B106). The pending entry and its staged blob are
    /// what the open handle reads, so they stay until the final close drops
    /// them.
    pub(super) fn withdraw_queued_ops(&self, uid: &NodeUid) -> Result<(), Errno> {
        self.cancel_upload(uid);
        let blobs = self.db.delete_ops_for_uid(&uid.to_string()).map_err(|e| {
            error!(%uid, error = %e, "withdrawing queued ops failed");
            Errno::EIO
        })?;
        let read_from = self.pending_blob(uid);
        let blobs = blobs
            .into_iter()
            .filter(|blob| read_from.as_deref() != Some(Path::new(blob)))
            .collect();
        self.release_dropped_below(blobs);
        Ok(())
    }

    /// Let go of what the ops just dropped for `uid` held: their pending
    /// entries, any upload still reading their bytes, and the bytes.
    ///
    /// Removing a folder also drops the queued revisions of the files below it
    /// (`docs/BUGS.md` B102), so the entries to forget are found by blob, not
    /// only by `uid`.
    pub(super) fn release_dropped_ops(&self, uid: &NodeUid, blobs: Vec<String>) {
        self.writes.pending.lock().remove(uid);
        self.release_dropped_below(blobs);
    }

    /// [`Core::release_dropped_ops`] without touching the node's own pending
    /// entry.
    pub(super) fn release_dropped_below(&self, blobs: Vec<String>) {
        let below: Vec<NodeUid> = {
            let mut pending = self.writes.pending.lock();
            let below: Vec<NodeUid> = pending
                .iter()
                .filter(|(_, p)| blobs.iter().any(|b| p.path == Path::new(b)))
                .map(|(k, _)| k.clone())
                .collect();
            for k in &below {
                pending.remove(k);
            }
            below
        };
        for k in &below {
            self.cancel_upload(k);
        }
        for blob in blobs {
            self.cache.discard_staged(Path::new(&blob));
        }
    }

    /// Keep a write we cannot safely queue, so the bytes are recoverable even
    /// though the caller is getting an error. See [`Core::queue_revision`].
    pub(super) fn stage_orphaned_write(
        &self,
        uid: &NodeUid,
        ino: u64,
        src: &Path,
        meta: &StagedWrite,
        reason: &str,
    ) {
        match self.cache.preserve_write(meta, src) {
            Ok(staged) => {
                self.cache.clear_scratch_durable(src);
                error!(
                    %uid,
                    staged = %staged.display(),
                    %reason,
                    "cannot queue write; bytes kept in staging"
                );
                let name = {
                    let st = self.state();
                    st.entries
                        .get(&ino)
                        .map(|e| e.node.name.clone())
                        .unwrap_or_default()
                };
                self.log_activity(
                    ActivityKind::Upload,
                    &name,
                    format!(
                        "write not queued ({reason}); changes kept at {}",
                        staged.display()
                    ),
                    false,
                );
            }
            Err(e) => {
                let surviving = e.surviving_path.clone();
                let marker = e.marker_path.clone();
                error!(
                    %uid,
                    error = %e,
                    surviving = %surviving.display(),
                    marker = ?marker,
                    %reason,
                    "write rescue was only partially published; bytes retained"
                );
                let name = {
                    let st = self.state();
                    st.entries
                        .get(&ino)
                        .map(|entry| entry.node.name.clone())
                        .unwrap_or_default()
                };
                self.log_activity(
                    ActivityKind::Upload,
                    &name,
                    format!(
                        "write not queued ({reason}); changes retained at {}",
                        surviving.display()
                    ),
                    false,
                );
            }
        }
    }
}
