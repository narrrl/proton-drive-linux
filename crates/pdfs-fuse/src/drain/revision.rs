//! Draining a revision: the upload, the conflict check before it, and
//! placing what landed in the tree and the cache.

use super::*;

impl Core {
    /// How long a freshly queued revision of `uid` should wait before a worker
    /// may pick it up.
    ///
    /// [`DRAIN_REVISION_DEBOUNCE`] exists for a tool that preallocates a file
    /// and then writes it, where two seconds is the right scale. It is the wrong
    /// scale for a large file on a slow link: an editor saving a 200 MB project
    /// every ten seconds queues a write, the drain starts sending it, the next
    /// save supersedes it mid-flight, and the file never finishes uploading
    /// while the user keeps working. Waiting roughly as long as the last upload
    /// of *this* node took moves that supersede into the queue, where replacing
    /// a row costs nothing.
    ///
    /// Never shorter than the fixed debounce and never longer than
    /// [`DRAIN_REVISION_DEBOUNCE_MAX`]: a node whose upload is genuinely slow
    /// still has to reach Drive, and a single pathological stall must not park
    /// it.
    ///
    /// [`DRAIN_REVISION_DEBOUNCE`]: crate::DRAIN_REVISION_DEBOUNCE
    /// [`DRAIN_REVISION_DEBOUNCE_MAX`]: crate::DRAIN_REVISION_DEBOUNCE_MAX
    pub(crate) fn revision_debounce(&self, uid: &NodeUid) -> Duration {
        let measured = self
            .drain
            .upload_times
            .lock()
            .get(&uid.to_string())
            .map(|&(ms, _)| Duration::from_millis(ms));
        adaptive_debounce(measured)
    }

    /// Remember how long an upload of `uid` took, for the next write's
    /// debounce.
    ///
    /// Only a *completed* upload is a measurement. A cancelled or failed one
    /// says how long it ran before it stopped, which is not how long the file
    /// takes to send, and feeding that back would let one network fault widen
    /// the debounce for good.
    pub(crate) fn record_upload_time(&self, uid: &NodeUid, took: Duration) {
        let mut times = self.drain.upload_times.lock();
        let now = now_millis();
        if times.len() >= UPLOAD_TIME_MEMORY
            && !times.contains_key(&uid.to_string())
            && let Some(oldest) = times
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
        {
            times.remove(&oldest);
        }
        times.insert(uid.to_string(), (took.as_millis() as u64, now));
    }

    /// Register a cancellation flag for an upload of `uid` that is about to
    /// start, returning a guard that deregisters it.
    ///
    /// Deregistration is by identity rather than by key: a supersede that
    /// arrives just as this upload finishes may already have registered the
    /// *next* upload's flag under the same uid, and removing that one would
    /// leave the newer upload uncancellable.
    pub(super) fn begin_cancellable_upload(&self, uid: &NodeUid) -> UploadCancel {
        let flag = Arc::new(AtomicBool::new(false));
        self.drain
            .upload_cancel
            .lock()
            .insert(uid.clone(), flag.clone());
        UploadCancel {
            registry: self.drain.upload_cancel.clone(),
            uid: uid.clone(),
            flag,
        }
    }

    /// Tell an upload of `uid` that is on the wire to stop, if there is one.
    ///
    /// Called when a newer revision of the file is queued. The upload does not
    /// stop instantly — it stops when the SDK next asks its reader for bytes,
    /// which for a 4 MiB block is soon enough to matter and late enough to be
    /// safe: the staged blob it is reading stays open until it lets go.
    pub(crate) fn cancel_upload(&self, uid: &NodeUid) {
        if let Some(flag) = self.drain.upload_cancel.lock().get(uid) {
            flag.store(true, Ordering::Relaxed);
            debug!(%uid, "cancelling a superseded upload");
        }
    }

    /// Why a queued write must not be applied to its node, or `None` when it
    /// still can be.
    ///
    /// A queued write is an edit of a specific revision. Time passes before it
    /// drains — indefinitely, if that is how long the network is gone — and in
    /// that window the node can be rewritten by another device, trashed, or
    /// deleted outright. Sending the blob anyway would silently drop whatever
    /// happened in between, which is exactly the thing the sync engine refuses
    /// to do.
    ///
    /// Only checkable against a recorded baseline: a write staged before
    /// [`StagedWrite::based_on`] existed, or one against a node that has never
    /// existed remotely, has nothing to compare and is applied as before.
    pub(crate) fn revision_conflict(
        &self,
        uid: &NodeUid,
        meta: &StagedWrite,
        blob: &Path,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let Some(ref base) = meta.based_on else {
            return Ok(None);
        };
        let Some(node) = self.fetch_node_remote(uid)? else {
            return Ok(Some("the file no longer exists remotely".into()));
        };
        if node.trashed {
            return Ok(Some("the file was trashed remotely".into()));
        }
        let Some(reason) = revision_changed(base, &node) else {
            return Ok(None);
        };
        // The remote moved on from the baseline — normally a conflict. But if it
        // sits at a revision *this daemon* sealed, no other device changed the
        // file: it is a single-writer stall→resume (a browser that closed and
        // reopened its download fd; B70 layer B). Chain onto it — supersede
        // instead of forking — so the finished bytes win the name rather than
        // being exiled to a `(sync-conflict)` copy.
        let remote_rev = node_revision_id(&node);
        // The table, so the answer survives a restart (`own_sealed_rev`).
        let own_rev = self
            .db
            .own_sealed_rev(&uid.to_string(), now_millis())
            .unwrap_or_default();
        if is_own_self_supersede(meta.complete, remote_rev.as_deref(), own_rev.as_deref()) {
            debug!(%uid, ?remote_rev,
                   "queued write chains onto our own sealed revision; not a conflict");
            return Ok(None);
        }
        // An earlier attempt at this write may have landed with its answer
        // lost: the remote then holds these very bytes in a revision we never
        // heard of. Forked, the write was a conflict copy of itself
        // (docs/BUGS.md B155); sent again, it is a revision of the same bytes.
        if meta.complete
            && node_size(&node) == meta.len
            && let NodeKind::File {
                content_sha1: Some(remote),
                ..
            } = &node.kind
            && remote.eq_ignore_ascii_case(&staged_sha1(blob)?)
        {
            debug!(%uid, ?remote_rev, "the remote already holds the queued write; not a conflict");
            return Ok(None);
        }
        // An upload of ours landed and its read-back went unanswered, so its
        // revision id was never recorded as ours. The bytes it sent say the
        // same: the remote holds them, so no other device wrote since
        // (docs/BUGS.md B157).
        let sealed = self.drain.sealed_unread.lock().get(uid).cloned();
        if meta.complete
            && let Some(sealed) = sealed
            && let NodeKind::File {
                content_sha1: Some(remote),
                ..
            } = &node.kind
            && remote.eq_ignore_ascii_case(&sealed)
        {
            debug!(%uid, ?remote_rev, "queued write chains onto our own unread revision; not a conflict");
            return Ok(None);
        }
        Ok(Some(reason))
    }

    /// Land a queued write that can no longer be applied to its own node as a
    /// *new* file beside it, and retire the op.
    ///
    /// The non-destructive resolution, and the same one the sync engine reaches
    /// for: the remote keeps whatever it has, the user keeps their bytes, and
    /// the name says which is which.
    ///
    /// An incomplete blob is gap-filled from whatever the node holds *now* —
    /// mixing revisions, which is only defensible because the result is
    /// explicitly a conflict copy rather than anyone's file. When even that is
    /// impossible the op is dropped but the staged bytes are deliberately left
    /// on disk: unreachable through the mount, but not destroyed, and the
    /// activity log says where they are.
    pub(crate) fn keep_as_conflict_copy(
        &self,
        op: &PendingOp,
        blob: &Path,
        meta: &StagedWrite,
        uid: &NodeUid,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let place = self.node_place(uid);
        let name = place
            .as_ref()
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| self.node_name(uid));
        // A node the tree has forgotten still has bytes worth keeping, so the
        // copy falls back to the root rather than being abandoned. So does one
        // whose folder went with it: an upload into a folder Drive no longer
        // has fails on every retry (B150).
        let folder = match place.map(|(p, _)| p) {
            Some(p) if self.fetch_node_remote(&p)?.is_some_and(|n| !n.trashed) => Some(p),
            _ => None,
        };
        let Some(parent) = folder.or_else(|| self.root_uid()) else {
            return self.abandon_to_staging(op, blob, uid, &name, reason);
        };
        warn!(%uid, name, reason, "queued write conflicts; keeping a conflict copy");

        if !meta.complete {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(blob)?;
            let mut written = Intervals::default();
            for &(s, e) in &meta.authored {
                written.add(s, e);
            }
            if let Err(e) = self.fill_gaps(
                uid,
                &file,
                meta.len,
                meta.base_mtime,
                meta.base_size,
                &written,
            ) {
                error!(%uid, name, error = ?e, "cannot complete a conflicted partial write");
                return self.abandon_to_staging(op, blob, uid, &name, reason);
            }
        }

        let alt = conflict_name(&name, now_secs());
        let thumbnails = self.upload_thumbnails(blob, &alt);
        let guard =
            self.transfers
                .begin(&alt, meta.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(blob)?, &guard);
        self.block_on_within(
            link::upload_deadline(meta.len, self.transfers.upload_cap()),
            self.drive.upload_file_from(
                &parent,
                &alt,
                media_type_for(&alt),
                reader,
                meta.len as i64,
                thumbnails,
                None,
                false,
            ),
        )?;
        drop(guard);

        self.db.delete_op(op.id)?;
        // Dropping the pending entry hands the node back to the remote's truth:
        // reads stop coming from the staged blob, and the event sync stops
        // skipping it as "ahead of the server". Only if it
        // is still *this* write's entry — see [`Core::release_pending`].
        self.release_pending(uid, blob);
        self.cache.discard_staged(blob);
        self.cache.evict(uid);
        self.evict_reader(uid);
        // The conflict copy is a node the tree has never seen.
        self.for_each_state(|st| {
            if let Some(&ino) = st.by_uid.get(&parent) {
                st.invalidate_listing(ino);
            }
        });
        self.log_activity(
            ActivityKind::Upload,
            &name,
            format!("{reason}; local changes uploaded as {alt}"),
            false,
        );
        self.events.publish(&[
            pdfs_core::control::Topic::Files,
            pdfs_core::control::Topic::Conflicts,
        ]);
        info!(%uid, name, alt, "queued write landed as a conflict copy");
        Ok(())
    }

    /// Give up on placing a queued write anywhere the mount can see, without
    /// destroying it: the op goes (it could only fail forever) but the staged
    /// blob deliberately stays on disk, and the activity log says where.
    ///
    /// The last resort of the conflict path, and the same bargain
    /// [`Core::stage_orphaned_write`] strikes at the other end: bytes we cannot
    /// place are still bytes we do not get to delete.
    pub(crate) fn abandon_to_staging(
        &self,
        op: &PendingOp,
        blob: &Path,
        uid: &NodeUid,
        name: &str,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        error!(%uid, name, reason, staged = %blob.display(),
               "cannot place a conflicted write; bytes kept in staging");
        self.db.delete_op(op.id)?;
        self.release_pending(uid, blob);
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.log_activity(
            ActivityKind::Upload,
            name,
            format!("{reason}; local changes kept at {}", blob.display()),
            false,
        );
        Ok(())
    }

    /// Upload a staged revision of a file the server already knows about.
    pub(crate) fn drain_revision(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let op_started = Instant::now();
        let blob = op
            .blob_path
            .clone()
            .ok_or("pending op has no staged blob")?;
        let blob = PathBuf::from(blob);
        let mut meta: StagedWrite = serde_json::from_str(op.meta_json.as_deref().unwrap_or(""))?;
        let uid = parse_node_uid(&meta.uid).ok_or("staged write has an unparseable uid")?;

        if let Some(reason) = self.revision_conflict(&uid, &meta, &blob)? {
            // Trashing the file from the mount drops its queued write, but not
            // from under a worker already holding it. That worker then finds the
            // node trashed — by us — and would upload the withdrawn bytes as a
            // conflict copy (a `recovered-…` file in the root when the tree has
            // already forgotten the node). A write nobody queues any more is
            // not a conflict.
            if !self.db.op_exists(op.id)? {
                debug!(%uid, reason, "queued write withdrawn while it drained; dropping it");
                return Ok(());
            }
            return self.keep_as_conflict_copy(op, &blob, &meta, &uid, &reason);
        }

        // An incomplete blob is authored bytes over zeros; the untouched ranges
        // have to be filled from the base before it can be sent. This is the case
        // the write could not resolve at release time (it was offline), and the
        // reason it is safe to do now is that we are not.
        if !meta.complete {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&blob)?;
            let mut written = Intervals::default();
            for &(s, e) in &meta.authored {
                written.add(s, e);
            }
            self.fill_gaps(
                &uid,
                &file,
                meta.len,
                meta.base_mtime,
                meta.base_size,
                &written,
            )
            .map_err(|e| self.errno_error(e, "gap-fill from base failed"))?;
            // The blob is whole now. Recorded, so that a retry after a lost
            // answer can match these bytes against the server's revision
            // (`revision_conflict`) instead of forking a conflict copy of the
            // user's own write.
            meta.complete = true;
            match serde_json::to_string(&meta) {
                Ok(json) => {
                    let path = op.blob_path.as_deref().unwrap_or_default();
                    if let Err(e) = self.db.update_op_meta_for_blob(op.id, path, &json) {
                        warn!(%uid, error = %e,
                              "recording a gap-filled blob as whole failed; \
                               a lost answer may land it as a conflict copy");
                    }
                }
                Err(e) => warn!(%uid, error = %e, "serializing a gap-filled sidecar failed"),
            }
        }

        let name = self.node_name(&uid);
        let thumbnails = self.upload_thumbnails(&blob, &name);
        let guard =
            self.transfers
                .begin(&name, meta.uid.clone(), TransferDirection::Upload, meta.len);
        // Registered before the first byte moves, so a write that lands while
        // this is on the wire can stop it rather than paying for a revision the
        // op it queued will immediately replace.
        let cancel = self.begin_cancellable_upload(&uid);
        // An unlink that withdrew this op before the flag above existed had no
        // upload to cancel; the row is the record that it happened.
        if !self.db.op_exists(op.id)? {
            debug!(%uid, "queued write withdrawn before it went out; dropping it");
            return Ok(());
        }
        let reader = CountingReader::new(File::open(&blob)?, &guard).with_cancel(cancel.flag());
        let started = Instant::now();
        let sent = self.block_on_within(
            link::upload_deadline(meta.len, self.transfers.upload_cap()),
            self.drive
                .upload_new_revision_from(&uid, reader, meta.len as i64, thumbnails, None),
        );
        let took = started.elapsed();
        if let Err(e) = sent {
            if cancel.cancelled() {
                // Not a failure, and not this op's row to retire: the write that
                // cancelled us deleted it and took ownership of the staged blob
                // when it superseded this revision. Leaving the blob alone here
                // is the whole point — it is either already unlinked or it
                // belongs to the newer op.
                info!(%uid, "upload abandoned; a newer write superseded it");
                return Ok(());
            }
            return Err(e.into());
        }
        drop(cancel);
        drop(guard);
        // Only a completed upload times how long this file takes to send, which
        // is what the next write's debounce is scaled from.
        self.record_upload_time(&uid, took);

        // Still pending to whoever asks until the bytes are cached below, and
        // counted before the op goes so the status never reads empty in between.
        // An empty queue is how a script or the app knows a written file reads
        // without the network; retiring the op alone left the read-back's round
        // trip in which it did not (B119). The op itself has to go first: one
        // restored after a crash would re-send a revision this daemon has not
        // yet recorded as its own, and come back as a conflict copy.
        let _landing = LandingUpload::begin(&self.drain.landing_uploads);
        // Retire the op before dropping the blob: a crash between the two leaves
        // an orphaned file (harmless), whereas the reverse would leave a queued
        // op pointing at nothing.
        self.db.delete_op(op.id)?;
        self.evict_reader(&uid);
        // The staged blob now matches the sealed revision, so it becomes the
        // file's cached content, stamped like the revision just read back.
        // Reading the file again then needs no network, which an editor or a
        // build does at once and a flaky link cannot always give (`docs/BUGS.md`
        // B119). Adopted by link, not by value: the blob is the whole file, and
        // a video read into a `Vec` here is an OOM. `discard_staged` below drops
        // the staging name only — the cache keeps the inode.
        self.refresh_after_upload(&uid, &blob, |mtime, size| {
            if size == meta.len
                && keeps_landed_upload(self.cache.is_pinned(&uid), meta.len, self.cache.budget())
            {
                let _ = self.cache.store_file(&uid, mtime, meta.len, &blob);
            }
        });
        self.cache.discard_staged(&blob);
        self.log_activity(ActivityKind::Upload, &name, "uploaded", true);
        // Both times, so a slow drain shows whether the upload itself or the
        // checks and gap-fill around it are where the time goes.
        info!(
            %uid,
            len = meta.len,
            upload_ms = took.as_millis() as u64,
            total_ms = op_started.elapsed().as_millis() as u64,
            "pending upload landed"
        );
        Ok(())
    }

    /// Adopt the server's metadata for a node we have just uploaded a revision
    /// for.
    ///
    /// [`State::record_pending_write`] deliberately stamps the node with the
    /// moment we *accepted* the write, so `ls` reflects it before the upload
    /// lands. The server stamps the sealed revision with its own time, and the
    /// two differ by however long the upload took. That difference is not
    /// cosmetic: [`Core::remote_baseline`] reads this node to build
    /// [`StagedWrite::based_on`], and [`Core::revision_conflict`] compares that
    /// baseline against the remote — so leaving our optimistic time in place
    /// makes the *next* write to this file look like another device changed it
    /// underneath us, and diverts it into a conflict copy over nothing.
    ///
    /// A write queued while this upload was on the wire is handled instead by
    /// [`Core::rebaseline_pending`]: its optimistic size/mtime must stay on the
    /// node (the same reason `apply_event` refuses to overwrite a node with a
    /// queued write), but its *baseline* still has to learn about the revision
    /// we just sealed. Skipping both, as this used to, is what produced a file
    /// that conflicted with itself and left a full-size duplicate behind.
    ///
    /// `blob` is the revision that landed, which stays the pending one until
    /// open write handles are rebased off it, and `adopt` gets the mtime and
    /// size of the node read back before that, so the uploaded bytes can be
    /// cached under its revision first. A handle released in between fills its
    /// gaps from one or the other; with the blob let go first and the cache
    /// filled last, it found neither, and the next write open was refused
    /// (docs/BUGS.md B123). When the node cannot be read back, `adopt` gets
    /// the stamp the tree still has, which is the base a write opened next
    /// takes: without it the blob went and nothing replaced it (B157). The
    /// blob's digest then stands in for the revision id the read-back did
    /// not bring, so the next write knows the revision as its own.
    ///
    /// Best effort: a failure here costs a spurious conflict copy on the next
    /// write, not this upload, which has already landed. Returns the node as
    /// the server now has it, or `None` when it could not be read back.
    pub(crate) fn refresh_after_upload(
        &self,
        uid: &NodeUid,
        blob: &Path,
        adopt: impl FnOnce(i64, u64),
    ) -> Option<Node> {
        // Asked before the read-back: the op went before it, so a rename queued
        // behind the upload can land while Drive answers, and the answer may
        // still have the old name (`docs/BUGS.md` B172).
        let placed = self.placement(uid);
        let renaming = self
            .db
            .has_pending_op(&uid.to_string(), OP_RENAME)
            .unwrap_or(false);
        let fetched = self.fetch_node_remote(uid);
        if fetched.is_ok() {
            self.drain.sealed_unread.lock().remove(uid);
        }
        let node = match fetched {
            Ok(Some(node)) => node,
            // Trashed or deleted under us: the tree will hear it from the event
            // sync, which is better placed to unhook the inode than we are.
            Ok(None) => {
                self.release_pending(uid, blob);
                return None;
            }
            Err(e) => {
                warn!(%uid, error = %e,
                      "refreshing metadata after an upload failed; \
                       the next write to this file may conflict with itself");
                match staged_sha1(blob) {
                    Ok(sha) => {
                        self.drain.sealed_unread.lock().insert(uid.clone(), sha);
                    }
                    Err(_) => {
                        self.drain.sealed_unread.lock().remove(uid);
                    }
                }
                // No newer write took over, so the tree's stamp is this upload's.
                if self.pending_blob(uid).as_deref() == Some(blob) {
                    let mut stamp = None;
                    self.for_each_state(|st| {
                        if let Some(entry) = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino))
                        {
                            stamp = Some((entry.node.modification_time, node_size(&entry.node)));
                        }
                    });
                    if let Some((mtime, size)) = stamp {
                        adopt(mtime, size);
                    }
                }
                self.release_pending(uid, blob);
                return None;
            }
        };
        adopt(node.modification_time, node_size(&node));
        // This function's whole job is to bring the tree level with the revision
        // we just sealed, so the feed's report of that revision has nothing left
        // to tell us. Claimed here rather than at the upload call so it is only
        // recorded when the node was actually re-read (`Core::self_changes`).
        self.note_self_change(uid);
        // Rebase any *open* write handles targeting this node. A handle opened
        // before this upload carries a stale base — its `base_mtime`/`base_size`
        // name the revision we just replaced. Without this update,
        // `remote_baseline` will build a `based_on` from those stale values, the
        // drain will compare them against the revision we sealed, and the write
        // will be diverted into a conflict copy of itself.
        //
        // This is the open-handle counterpart of `rebaseline_pending`, which
        // covers the same gap for *queued* ops.
        {
            // Remember it as ours, so a queued write that opened over an earlier
            // revision recognises this as a self-supersede rather than a foreign
            // change and does not fork (B70 layer B; consulted in
            // `revision_conflict`).
            if let Some(rev) = node_revision_id(&node) {
                // In the table, not in memory: losing it at shutdown reopened
                // the fork window B70 layer B closed, for the first drain
                // after a restart.
                if let Err(e) = self
                    .db
                    .set_own_sealed_rev(&uid.to_string(), &rev, now_millis())
                {
                    warn!(%uid, error = %e, "persisting our sealed revision failed");
                }
            }
            // A write open from here until the tree has the node takes it as
            // its base: the blob it would have copied is let go of below, and
            // the tree's stamp names the revision this one replaced
            // (docs/BUGS.md B170).
            self.drain.landed.lock().insert(uid.clone(), node.clone());
            self.rebase_open_writes(uid, &node);
        }
        self.release_pending(uid, blob);
        #[cfg(test)]
        self.drive.landing(uid);
        self.place_landed(uid, &node, placed, renaming);
        self.drain.landed.lock().remove(uid);
        Some(node)
    }

    /// Put a node whose upload just landed into the tree, unless a newer write
    /// is queued for it. `before` is where the tree had the node and
    /// `renaming` whether a rename of it was queued, both from before the
    /// read-back.
    pub(crate) fn place_landed(
        &self,
        uid: &NodeUid,
        node: &Node,
        before: Option<(String, Option<NodeUid>)>,
        renaming: bool,
    ) {
        // Ordered so that a write queued *during* the fetch above is still
        // caught: it took its baseline from the node's optimistic stamp, and
        // this overwrites it with the revision the server actually holds.
        if self.rebaseline_pending(uid, node) {
            return;
        }
        // A rename queued behind the upload has not reached Drive yet, or
        // landed after Drive read the node back, and the tree keeps the name
        // and folder the user gave the file, as `adopt_real_uid` does. Taking
        // Drive's put the file back under its old name. A rename queued since
        // the read-back began has already moved the entry when the tree is
        // updated, so a name that changed in between is the user's too
        // (docs/BUGS.md B146, B172).
        let renaming = renaming
            || self
                .db
                .has_pending_op(&uid.to_string(), OP_RENAME)
                .unwrap_or(false);
        self.for_each_state(|st| {
            let moved = placed_in(st, uid) != before;
            let Some(entry) = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino)) else {
                return;
            };
            let parent = entry.parent;
            let mut node = node.clone();
            if renaming || moved {
                node.name = entry.node.name.clone();
                node.parent_uid = entry.node.parent_uid.clone();
            }
            st.intern(parent, node);
        });
    }

    /// Where the tree has `uid`: its name and parent.
    pub(crate) fn placement(&self, uid: &NodeUid) -> Option<(String, Option<NodeUid>)> {
        placed_in(&self.state(), uid)
    }

    /// Rebase every write handle open on `uid` onto `node`, a revision this
    /// daemon has just put there itself (`WriteHandle::rebase_onto`).
    pub(crate) fn rebase_open_writes(&self, uid: &NodeUid, node: &Node) {
        self.for_each_state(|st| {
            for aw in st.active_writes.values_mut() {
                if aw.uid == *uid {
                    aw.rebase_onto(node);
                    debug!(%uid, mtime = aw.base_mtime, size = aw.base_size,
                           "rebased open write handle onto the revision just landed");
                }
            }
        });
    }

    /// Point a still-queued write at the revision this upload just sealed, and
    /// report whether there was one.
    ///
    /// The revision we sent *is* the base the queued write will be applied over,
    /// but nothing else says so. [`Core::remote_baseline`] carries a baseline
    /// across a supersede because the op it inherits from "is the last one that
    /// actually observed the remote" — true only until that op drains. Once it
    /// has, the inherited baseline names a revision we replaced ourselves, and
    /// [`Core::revision_conflict`] reads that as another device having moved the
    /// file. It is a self-conflict, and it is expensive: the whole staged blob
    /// is re-uploaded as a second file.
    ///
    /// Both copies of the sidecar are updated — the in-memory one the next
    /// `release` inherits from, and the persisted one the drain reloads after a
    /// restart. Best effort on the DB half; the in-memory half is what the
    /// immediate next write reads.
    pub(crate) fn rebaseline_pending(&self, uid: &NodeUid, sealed: &Node) -> bool {
        let base = Baseline {
            mtime: sealed.modification_time,
            size: node_size(sealed),
            hash: None,
            revision_id: node_revision_id(sealed),
        };
        let mut pending = self.pending.lock();
        let Some(p) = pending.get_mut(uid) else {
            return false;
        };
        p.meta.based_on = Some(base.clone());
        match serde_json::to_string(&p.meta) {
            Ok(json) => {
                if let Err(e) = self.db.update_op_meta(&uid.to_string(), OP_REVISION, &json) {
                    warn!(%uid, error = %e,
                          "restamping a queued write's baseline failed; \
                           it may land as a conflict copy of itself");
                }
            }
            Err(e) => warn!(%uid, error = %e, "serializing a restamped sidecar failed"),
        }
        debug!(%uid, mtime = base.mtime, size = base.size,
               "queued write rebaselined onto the revision just uploaded");
        true
    }
}
