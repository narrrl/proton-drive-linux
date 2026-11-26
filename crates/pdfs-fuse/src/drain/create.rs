//! Draining a create or a mkdir: sending it, adopting a twin an unanswered
//! attempt left on Drive, and swapping the placeholder for the real node.

use super::*;

impl Core {
    /// Finish adopting the node `op` is for, if its create landed but could
    /// not be read back.
    ///
    /// Until it is, the tree has the node as it was made locally, and a write
    /// queued over it is based on that rather than on what Drive holds. Sent
    /// so, the write found our own create's revision in its way and landed as
    /// a conflict copy (`docs/BUGS.md` B143).
    pub(crate) fn finish_adoption(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let Some(real) = parse_node_uid(&op.uid) else {
            return Ok(());
        };
        let local = match self.drain.unadopted.lock().get(&real).cloned() {
            Some(local) => local,
            // The map does not outlive a restart, which also forgets the
            // stand-in: the row is under the real uid by then. A file as it was
            // made here has no revision of Drive's (`docs/BUGS.md` B182).
            None if op.kind == OP_REVISION && self.never_read_back(&real) => real.clone(),
            None => return Ok(()),
        };
        // A node gone since is the op's to deal with.
        if self.fetch_node_remote(&real)?.is_some() {
            self.adopt_real_uid(&local, &real, |_| {})?;
        }
        self.drain.unadopted.lock().remove(&real);
        debug!(%local, %real, "adopted a landed create");
        Ok(())
    }

    /// Whether the tree has the file `real` as it was made here: its create
    /// landed, but Drive's answer was never read back.
    pub(crate) fn never_read_back(&self, real: &NodeUid) -> bool {
        let st = self.state();
        st.by_uid
            .get(real)
            .and_then(|ino| st.entries.get(ino))
            .is_some_and(|entry| !entry.node.is_folder() && node_revision_id(&entry.node).is_none())
    }

    /// Make a node that so far exists only on this machine real, and adopt the
    /// uid the server gives it.
    pub(crate) fn drain_local_node(
        &self,
        op: &PendingOp,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Drive holds the name the create is sent with once it lands, whatever
        // became of the file meanwhile, until the rename or trash queued
        // behind it frees it (`Core::name_is_held`).
        if let (Some(parent), Some(name)) = (op.parent_uid.as_deref(), op.name.as_deref())
            && let Some(parent) = parse_node_uid(parent)
        {
            self.drain
                .creating
                .lock()
                .insert(op.id, (parent, name.to_string()));
        }
        let created = self.create_local_node(op);
        self.drain.creating.lock().remove(&op.id);
        created
    }

    pub(crate) fn create_local_node(
        &self,
        op: &PendingOp,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let local = parse_node_uid(&op.uid).ok_or("pending op has an unparseable uid")?;
        let parent_str = op.parent_uid.as_deref().ok_or("create op has no parent")?;
        // `run_pending_drain` will not offer an op whose parent is still a
        // placeholder, so reaching here with one is a bug, not a wait.
        if is_local_uid_str(parent_str) {
            return Err(format!("parent {parent_str} has not been created yet").into());
        }
        let parent = parse_node_uid(parent_str).ok_or("create op has an unparseable parent")?;
        let wanted = op.name.clone().ok_or("create op has no name")?;
        if self.db.has_withdrawn_create(parent_str, &wanted)? {
            return Err(held_by_queued_change(&wanted));
        }

        // Someone else may have taken the name while this sat in the queue —
        // reachable only because the op waited, which is the whole point of the
        // queue. Never overwrite theirs and never drop ours: land under a
        // conflict name, exactly as the sync engine does.
        //
        // Two things can hold the name that are not someone else's file. The
        // node a create made before the network dropped its answer is ours: the
        // create was queued because nobody heard back, so adopt it. And a node
        // whose trash is still queued is about to let go of it.
        let mut name = wanted.clone();
        let mut home = parent.clone();
        let mut sent = (parent_str.to_string(), wanted.clone());
        let mut blob_landed = true;
        // An earlier attempt, sent before the file was renamed or moved, may
        // have made it and lost the answer. That node is ours where it was
        // sent, and the rename follows it (`docs/BUGS.md` B184).
        let mut real = match self.earlier_twin(op, parent_str, &wanted)? {
            Some(EarlierTwin {
                parent: was_in,
                name: was_named,
                twin,
                holds_blob,
            }) => {
                info!(%local, %twin, was_named, wanted, "an earlier attempt landed unanswered under another name; adopting it");
                sent = (was_in.to_string(), was_named.clone());
                home = was_in;
                name = was_named;
                blob_landed = holds_blob;
                Ok(twin)
            }
            None => self.create_drained_node(op, &parent, &name),
        };
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref()))
            && let Some((twin, holds_blob)) = self.adoptable_twin(op, &parent, &wanted)?
        {
            info!(%local, %twin, wanted, "the name is held by our own unanswered create; adopting it");
            real = Ok(twin);
            blob_landed = holds_blob;
        }
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref()))
            && self.name_is_held(op, Some(&parent), &wanted)?
        {
            return Err(held_by_queued_change(&wanted));
        }
        // What held it may have let go since Drive answered (docs/BUGS.md
        // B144).
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
            real = self.create_drained_node(op, &parent, &wanted);
        }
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
            name = conflict_name(&wanted, now_secs());
            warn!(%local, wanted, name, "name is taken remotely; creating under a conflict name");
            real = self.create_drained_node(op, &parent, &name);
            if real.is_ok() {
                self.log_activity(
                    ActivityKind::Upload,
                    &wanted,
                    format!("name was taken remotely; created as {name}"),
                    false,
                );
            }
        }
        // The folder this node was created in may have been trashed remotely
        // while the op waited, in which case nothing will ever make the op
        // succeed as written and retrying it forever wedges the queue behind a
        // file that has bytes to save. Re-home it to the root: not where the
        // user put it, but it exists, it is visible, and the bytes are intact.
        //
        // Not for a create a delete withdrew while it was on the wire: the
        // folder is most often gone because the user removed it, with this
        // file, and the file would come back in the root (`docs/BUGS.md` B168).
        if let Some(root) = self.root_uid()
            && real.is_err()
            && !self.db.has_pending_op(&op.uid, OP_TRASH)?
            && self.parent_is_gone(&parent)
        {
            warn!(%local, name, "parent folder is gone remotely; creating in the root instead");
            real = self.create_drained_node(op, &root, &name);
            if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
                name = conflict_name(&wanted, now_secs());
                real = self.create_drained_node(op, &root, &name);
            }
            home = root;
            if real.is_ok() {
                self.log_activity(
                    ActivityKind::Upload,
                    &wanted,
                    format!("its folder was trashed remotely; created in the root as {name}"),
                    false,
                );
            }
        }
        let real = real?;
        // Retiring the op makes the ops behind it due: a child created in a new
        // folder, a rename made while the create was on the wire. Drive answers
        // a create before it lists the node, and those ops sent before then
        // found no folder or dropped the rename as gone. Best effort:
        // `adopt_real_uid` reads it back again and reports the failure.
        let _ = self.read_back(&real);

        // Retire the op before touching anything else: if we crash here the node
        // exists remotely and the local placeholder is reconciled by the event
        // sync, whereas a surviving op would create the file a second time.
        // An adopted node made empty has not had a blob this op carries
        // uploaded: it goes on as a revision of the node instead.
        let uploaded = blob_landed.then_some(op.blob_path.as_deref()).flatten();
        let landing = CreateLanding {
            local: &local.to_string(),
            real: &real.to_string(),
            sent: (&sent.0, &sent.1),
            landed: (&home.to_string(), &name),
        };
        // Counted from the op's retirement until the bytes are cached and the
        // tree has the real uid, as an upload is. Uncounted, the queue read
        // empty while that still ran: a delete in between took an open file
        // for closed and dropped what its handle reads, and a script that
        // waited for an empty queue read the file before it was cached
        // (`docs/BUGS.md` B257).
        let _landing = LandingUpload::begin(&self.drain.landing_uploads);
        let newer = match self.retire_create(op, uploaded, &landing, &local, &real)? {
            CreateRetired::Withdrawn { trash } => {
                return self.trash_withdrawn_create(op, trash, &local, &real, &name);
            }
            retired => matches!(retired, CreateRetired::Newer { .. }),
        };
        // The uploaded blob is the new file's content, so it becomes the cached
        // content, as for a revision. A partial write fills its gaps from it;
        // without it the next write open was refused (`docs/BUGS.md` B123).
        let landed: Option<StagedWrite> = uploaded
            .filter(|_| !newer)
            .and(op.meta_json.as_deref())
            .and_then(|json| serde_json::from_str(json).ok());
        // The op is gone, so the create has landed whatever the read-back
        // says. One that fails is finished before anything else is sent for
        // the node (`docs/BUGS.md` B143).
        if let Err(e) = self.adopt_real_uid(&local, &real, |node| {
            if let (Some(blob), Some(meta)) = (uploaded, &landed)
                && node_size(node) == meta.len
                && keeps_landed_upload(self.cache.is_pinned(&real), meta.len, self.cache.budget())
            {
                let _ =
                    self.cache
                        .store_file(&real, node.modification_time, meta.len, Path::new(blob));
            }
        }) {
            warn!(%local, %real, error = %e, "reading back a landed create failed; adopting it later");
            self.drain
                .unadopted
                .lock()
                .insert(real.clone(), local.clone());
        }
        // The feed will report this create back to us; the tree already has it
        // under its real uid, so that event is ours to ignore (`WriteState::note_echo`).
        self.note_self_change(&real);
        // The uploaded blob is done with either way: a write that replaced it
        // while it was on the wire already discarded it when it attached.
        if let Some(blob) = uploaded {
            self.cache.discard_staged(Path::new(blob));
        }
        if newer {
            info!(%local, %real, name, "a write landed during the create; queued it as a revision");
            self.wake_drain();
        }
        self.log_activity(ActivityKind::Upload, &name, "created", true);
        info!(%local, %real, name, kind = %op.kind, "pending create landed");
        Ok(())
    }

    /// Whether `name` is held on Drive by a change of ours that is to free it:
    /// a queued trash, a queued rename away from it, or another create on the
    /// wire that was sent with it in `parent`, which a rename or trash follows
    /// (`Db::finish_create`). The op that wants the name waits for it rather
    /// than land under a conflict name.
    ///
    /// A trash is waited for as long as it takes. The rest only for
    /// [`NAME_HOLD_ATTEMPTS`]: two files swapping names each hold the name the
    /// other wants.
    pub(crate) fn name_is_held(
        &self,
        op: &PendingOp,
        parent: Option<&NodeUid>,
        name: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let parent_uid = parent.map(NodeUid::to_string);
        if self
            .db
            .has_pending_trash_named(name, parent_uid.as_deref())?
        {
            return Ok(true);
        }
        if op.attempts >= NAME_HOLD_ATTEMPTS {
            return Ok(false);
        }
        let creating = self
            .drain
            .creating
            .lock()
            .iter()
            .any(|(id, (p, n))| *id != op.id && Some(p) == parent && n == name);
        Ok(creating || self.db.has_pending_move_from(name)?)
    }

    /// Whether a create other than `op` is on the wire under `name`, in any
    /// folder.
    pub(crate) fn creating_named(&self, op: &PendingOp, name: &str) -> bool {
        self.drain
            .creating
            .lock()
            .iter()
            .any(|(id, (_, n))| *id != op.id && n == name)
    }

    /// The node an earlier attempt at create `op` made under a parent or name
    /// the op has been moved from since ([`Core::adoptable_twin`]).
    pub(super) fn earlier_twin(
        &self,
        op: &PendingOp,
        parent: &str,
        name: &str,
    ) -> Result<Option<EarlierTwin>, Box<dyn std::error::Error>> {
        for (was_in, was_named) in self.db.create_targets(op.id)? {
            if (was_in.as_str(), was_named.as_str()) == (parent, name) {
                continue;
            }
            let Some(was_in) = parse_node_uid(&was_in) else {
                continue;
            };
            match self.adoptable_twin(op, &was_in, &was_named) {
                Ok(Some((twin, holds_blob))) => {
                    return Ok(Some(EarlierTwin {
                        parent: was_in,
                        name: was_named,
                        twin,
                        holds_blob,
                    }));
                }
                Ok(None) => {}
                Err(e) if is_gone(e.as_ref()) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// The node already holding `name` under `parent`, if it is the one an
    /// earlier attempt at this op made before its answer was lost, and whether
    /// it holds the op's blob already.
    ///
    /// A create that timed out may still have landed; the op was queued because
    /// the caller never heard back. Without this, its replay forked a
    /// `(sync-conflict)` copy of a file nobody else had touched.
    pub(crate) fn adoptable_twin(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<Option<(NodeUid, bool)>, Box<dyn std::error::Error>> {
        // A second file queued under the same name, after the first was
        // renamed away, finds the first one's node fresh and empty while its
        // create is on the wire. Taking it put both files on one node, and the
        // first one's rename took the second one's bytes along
        // (docs/BUGS.md B147).
        let racing = self
            .drain
            .creating
            .lock()
            .iter()
            .any(|(id, (p, n))| *id != op.id && p == parent && n == name);
        if racing {
            return Ok(None);
        }
        let uids = self.block_on_bounded(self.drive.enumerate_folder_children_node_uids(parent))?;
        let children = self.block_on_bounded(self.drive.enumerate_nodes_light(&uids))?;
        // A node this mount trashed is not one an unanswered create made, even
        // while its trash is on the wire or a listing still shows it. Taking a
        // folder removed and made again put the new one's contents into the
        // old one, and its trash took them along (docs/BUGS.md B162).
        let hidden = self.writes.hidden();
        let Some(twin) = children
            .into_iter()
            .find(|node| node.name == name && !node.trashed && !hidden.contains(&node.uid))
        else {
            return Ok(None);
        };
        // A node in the tree was answered for, by this mount or by a listing:
        // the same race after the first create landed. Without the adoption,
        // an unanswered twin forks a conflict copy, which loses nothing.
        if self.state().by_uid.contains_key(&twin.uid) {
            return Ok(None);
        }
        // The light listing carries no file size or digest.
        let twin = match twin.kind {
            NodeKind::Folder => twin,
            NodeKind::File { .. } => match self.fetch_node_remote(&twin.uid)? {
                Some(node) => node,
                None => return Ok(None),
            },
        };
        // A create uploads its bytes in the same call, so the node a lost
        // answer leaves holds them (docs/BUGS.md B127): the ones the op holds
        // now, or ones a write has replaced since (B156), which still go up.
        let current = match op.blob_path.as_deref() {
            Some(blob) if node_size(&twin) > 0 => Some(staged_sha1(Path::new(blob))?),
            _ => None,
        };
        let holds_blob = current
            .as_deref()
            .is_some_and(|sha| holds_bytes(&twin, sha));
        let sent = self.create_sent_digests(op.id, current);
        Ok(
            adoptable(op.kind == OP_MKDIR, &twin, self.first_sent(op), &sent)
                .then_some((twin.uid, holds_blob)),
        )
    }

    /// The SHA-1s of the bytes queued create `id` may have made its file with:
    /// `current`, those of the blob it holds now, and those each earlier
    /// attempt uploaded.
    pub(crate) fn create_sent_digests(&self, id: i64, current: Option<String>) -> Vec<String> {
        let mut sent = self.db.create_sent(id).unwrap_or_else(|e| {
            warn!(id, error = %e, "reading what a queued create sent failed");
            Vec::new()
        });
        sent.extend(current);
        sent
    }

    /// When create `op` first went out, which is the earliest a node its lost
    /// answer left can have been made. An op queued offline may wait for days
    /// before that; measured from when it was queued, the adoption window let
    /// it take a node another device made in the meantime. Ops sent before the
    /// time was recorded fall back to when they were queued.
    pub(crate) fn first_sent(&self, op: &PendingOp) -> i64 {
        match self.db.create_first_sent(op.id) {
            Ok(Some(at)) => at.max(op.created_at),
            Ok(None) => op.created_at,
            Err(e) => {
                warn!(id = op.id, error = %e, "reading when a queued create was sent failed");
                op.created_at
            }
        }
    }

    /// Make one queued `create`/`mkdir` real under a given name, and hand back
    /// the API's own error so the caller can tell a name clash from a failure.
    ///
    /// Split out because the conflict path has to run it twice: the second time
    /// under a different name.
    pub(crate) fn create_drained_node(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<NodeUid, Box<dyn std::error::Error>> {
        self.db
            .note_create_target(op.id, &parent.to_string(), name, now_millis())?;
        match op.kind == OP_MKDIR {
            true => Ok(self.block_on_bounded(self.drive.create_folder(
                parent,
                name,
                Some(now_secs()),
            ))?),
            false => self.upload_created_file(op, parent, name),
        }
    }

    /// Upload the bytes a queued create accumulated, if any. A file that was
    /// created but never written (`touch`) has no blob and uploads as empty.
    pub(crate) fn upload_created_file(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<NodeUid, Box<dyn std::error::Error>> {
        let Some(blob) = op.blob_path.as_deref() else {
            return Ok(self.block_on_bounded(self.drive.upload_file(
                parent,
                name,
                media_type_for(name),
                b"",
            ))?);
        };
        let meta: StagedWrite = serde_json::from_str(op.meta_json.as_deref().unwrap_or(""))?;
        // An incomplete blob would be authored bytes over zeros, and there is no
        // base to repair it from — the file has never existed remotely. Refusing
        // to queue that is `queue_revision`'s job, so reaching here means the blob
        // is whole.
        if !meta.complete {
            return Err("queued create holds an incomplete blob".into());
        }
        // The call may make the file and lose its answer, and the op may hold
        // a newer blob by the time it is retried (`Core::adoptable_twin`).
        let sha = staged_sha1(Path::new(blob))?;
        self.db.note_create_sent(op.id, &sha)?;
        let thumbnails = self.photos().upload_thumbnails(Path::new(blob), name);
        let guard = self
            .transfers
            .begin(name, op.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(blob)?, &guard);
        let uid = self.block_on_within(
            link::upload_deadline(meta.len, self.transfers.upload_cap()),
            self.drive.upload_file_from(
                parent,
                name,
                media_type_for(name),
                reader,
                meta.len as i64,
                thumbnails,
                None,
                false,
            ),
        )?;
        Ok(uid)
    }

    /// Retire a landed create's op and move its queued state to the real uid,
    /// unless the create was withdrawn while it was on the wire.
    ///
    /// Runs under the `pending` lock, which `Core::enqueue_staged_write` also
    /// holds from attaching a blob to a create until it records the pending
    /// entry. So a write either attached before this ran and is carried over,
    /// or finds the create gone and queues against the real uid itself. The
    /// open write handles are repointed right after, for the same reason:
    /// a handle released later must not queue against the placeholder.
    pub(crate) fn retire_create(
        &self,
        op: &PendingOp,
        uploaded: Option<&str>,
        landing: &CreateLanding<'_>,
        local: &NodeUid,
        real: &NodeUid,
    ) -> Result<CreateRetired, Box<dyn std::error::Error>> {
        let retired = {
            let mut pending = self.writes.pending.lock();
            let retired = self.db.finish_create(op.id, uploaded, landing, |json| {
                let mut meta: StagedWrite = serde_json::from_str(json).ok()?;
                meta.uid = landing.real.to_string();
                serde_json::to_string(&meta).ok()
            })?;
            // A withdrawn create's placeholder is the delete's to clean up, and
            // an open handle may still read its pending entry.
            if let CreateRetired::Withdrawn { .. } = retired {
                return Ok(retired);
            }
            pending.remove(local);
            if let CreateRetired::Newer { blob, meta } = &retired {
                let meta: StagedWrite = serde_json::from_str(meta)?;
                pending.insert(
                    real.clone(),
                    PendingRevision {
                        path: PathBuf::from(blob),
                        meta,
                    },
                );
            }
            retired
        };
        #[cfg(test)]
        self.drive.landing(real);
        if retired == CreateRetired::Landed
            && let Some(blob) = uploaded
        {
            self.cache_created(op, local, real, blob);
        }
        self.for_each_state(|st| {
            if let Some(ino) = st.by_uid.remove(local) {
                st.by_uid.insert(real.clone(), ino);
                // The node too: the next write of it upserts the row by the
                // node's uid, and would bring the placeholder row back.
                if let Some(e) = st.entries.get_mut(&ino) {
                    e.uid = real.clone();
                    e.node.uid = real.clone();
                }
                // Its children still name the placeholder as their parent, and
                // the next write of one would put it back on the row.
                for kid in st.children.get(&ino).cloned().unwrap_or_default() {
                    if let Some(e) = st.entries.get_mut(&kid)
                        && e.node.parent_uid.as_ref() == Some(local)
                    {
                        e.node.parent_uid = Some(real.clone());
                    }
                }
            }
            for aw in st.active_writes.values_mut() {
                if aw.uid == *local {
                    aw.uid = real.clone();
                }
            }
        });
        Ok(retired)
    }

    /// Cache a landed create's uploaded `blob` under `real`, keyed as the tree
    /// has the file.
    ///
    /// A write handle open on the file is repointed at `real` right after this,
    /// and a release fills its gaps from the cache by the size and mtime it
    /// opened over. Drive lists the new node a moment after the create, so
    /// `adopt_real_uid` caches the bytes only once it can read the node back.
    /// A release in between found nothing to fill from, and the next write open
    /// was refused (docs/BUGS.md B131).
    pub(crate) fn cache_created(
        &self,
        op: &PendingOp,
        local: &NodeUid,
        real: &NodeUid,
        blob: &str,
    ) {
        let Some(meta) = op
            .meta_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<StagedWrite>(json).ok())
        else {
            return;
        };
        let mut mtime = None;
        self.for_each_state(|st| {
            if let Some(entry) = st.by_uid.get(local).and_then(|ino| st.entries.get(ino)) {
                mtime = Some(entry.node.modification_time);
            }
        });
        if let Some(mtime) = mtime
            && keeps_landed_upload(self.cache.is_pinned(real), meta.len, self.cache.budget())
        {
            let _ = self
                .cache
                .store_file(real, mtime, meta.len, Path::new(blob));
        }
    }

    /// Swap a placeholder uid for the real one across everything that keyed off
    /// it: queued children, the DB, the in-memory tree, and the caches.
    ///
    /// The inode is deliberately kept, so anything already holding the file open
    /// keeps working across the drain.
    ///
    /// Applied to the tree every mount shares. A node created inside an
    /// on-demand sync folder once lived in that fork's own state, where the
    /// primary mount's drain never looked, and stayed on its `local~` uid —
    /// an empty file for as long as the daemon ran (`docs/BUGS.md` B74).
    ///
    /// `adopt` sees the node as Drive made it, before anything is rebased onto
    /// it.
    pub(crate) fn adopt_real_uid(
        &self,
        local: &NodeUid,
        real: &NodeUid,
        adopt: impl FnOnce(&Node),
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A rename made while the create was on the wire is queued against the
        // real uid (`Db::finish_create`). Until it lands, the tree keeps the
        // name and folder the user gave it. Asked before the read-back: a
        // rename that lands while Drive answers leaves no op behind, and the
        // answer may still have the old name (`docs/BUGS.md` B161).
        let real_key = real.to_string();
        let renaming = self.db.has_pending_op(&real_key, OP_RENAME)?;
        // Drive answers a create before it lists the new node.
        let mut node = self
            .read_back(real)
            .map_err(|e| self.errno_error(e, "fetch node"))?;
        // Before open handles are rebased onto the node, so one released in
        // between finds what `adopt` keeps (`docs/BUGS.md` B123).
        adopt(&node);
        // What Drive holds is the base open handles and queued writes are
        // rebased onto below. The tree shows a write queued over it, as a
        // listing does: a write open sized from the landed node over a shorter
        // queued blob left a gap that refused the next open (`docs/BUGS.md`
        // B123).
        let remote = node.clone();
        self.stamp_pending_sizes(std::slice::from_mut(&mut node));
        // The rows and queued ops already follow the node: `retire_create`
        // moved its row to the real uid, and ops find it by local id.

        self.for_each_state(|st| {
            if let Some(ino) = st.by_uid.remove(local) {
                st.by_uid.insert(real.clone(), ino);
            }
            let Some(&ino) = st.by_uid.get(real) else {
                st.write_through(node.clone());
                return;
            };
            // Asked again under the lock: `Core::queue_rename` queues its op
            // before it renames the entry under this lock. Asked only before,
            // a rename made since was overwritten with the name the create was
            // sent with, here and in the node row (`docs/BUGS.md` B152).
            let renaming =
                renaming || matches!(self.db.has_pending_op(&real_key, OP_RENAME), Ok(true));
            if renaming && let Some(e) = st.entries.get(&ino) {
                node.name = e.node.name.clone();
                node.parent_uid = e.node.parent_uid.clone();
            }
            st.write_through(node.clone());
            let mut landed = node.clone();
            st.keep_open_write_size(ino, &mut landed);
            if let Some(e) = st.entries.get_mut(&ino) {
                e.uid = real.clone();
                e.node = landed;
            }
            // Where the op said the node goes and where it actually landed can
            // differ — a conflict re-homes it — so the tree follows the parent
            // the server reports rather than the one we asked for.
            if let Some(parent) = node.parent_uid.clone()
                && let Some(&pino) = st.by_uid.get(&parent)
                && st.entries.get(&ino).is_some_and(|e| e.parent != pino)
            {
                let old = st.entries.get(&ino).map(|e| e.parent);
                if let Some(old) = old
                    && let Some(kids) = st.children.get_mut(&old)
                {
                    kids.retain(|&k| k != ino);
                }
                if let Some(e) = st.entries.get_mut(&ino) {
                    e.parent = pino;
                }
                if let Some(kids) = st.children.get_mut(&pino)
                    && !kids.contains(&ino)
                {
                    kids.push(ino);
                }
            }
        });
        // A handle still open on the placeholder was based on no revision at
        // all: a local-clock mtime and no revision id. Released as it is, its
        // write would land as a conflict copy of the empty file this create just
        // made (`docs/BUGS.md` B113). Rebased before the restamp below, so a
        // release that races this is caught there.
        //
        // A release that took its base before the rebase and queues after the
        // restamp is caught by neither. The revision this create sealed is
        // ours, as one a revision upload sealed is, so the drain chains that
        // write onto it instead of forking a conflict copy (B70 layer B).
        if let Some(rev) = node_revision_id(&remote)
            && let Err(e) = self
                .db
                .set_own_sealed_rev(&real.to_string(), &rev, now_millis())
        {
            warn!(%real, error = %e, "persisting our sealed revision failed");
        }
        self.rebase_open_writes(real, &remote);
        // A write carried over from the create was made against no revision at
        // all; the one just created is what it now replaces.
        self.rebaseline_pending(real, &remote);
        if node.is_folder() {
            // It was recorded as listed while local (it was empty and had nothing
            // to enumerate). That still holds: its queued children re-intern under
            // the real uid as they drain.
            if let Err(e) = self.db.set_listed(real, true) {
                warn!(%real, error = %e, "db set_listed(true) failed after remap");
            }
        }
        Ok(())
    }

    /// Retire parks that are never going to end (docs/BUGS.md B70).
    ///
    /// A parked create waits for one event: the rename from the transient name
    /// to the finished one. Nothing else ever un-parks it, and plenty of writers
    /// never perform it — an editor unlinks its swap file, a download is
    /// abandoned, or the file was always going to be called `index.tmp`. Such a
    /// row stayed at [`PARK_UNTIL`] for the life of the database, pinning the
    /// only copy of its bytes in `staging/` and counting against the queue.
    ///
    /// Two outcomes, and neither deletes bytes a user can still see:
    ///
    /// * The node is gone from the tree — the file was deleted while its create
    ///   was parked, so the create is no longer wanted. The op and its blob go,
    ///   which is what [`Core::discard_queued_ops`] would have done had the
    ///   delete path reached it.
    /// * The node is still there and has been parked past
    ///   [`PARK_EXPIRY_MS`] with nothing holding it open — the park has outlived
    ///   the writer it was protecting. Un-park it, and the ordinary drain
    ///   uploads it like any other create.
    ///
    /// An open node is left alone whatever its age: it is still being written.
    pub(crate) fn sweep_parked_creates(&self) {
        let parked = match self.db.parked_create_ops() {
            Ok(parked) if !parked.is_empty() => parked,
            Ok(_) => return,
            Err(error) => {
                warn!(%error, "reading parked creates failed; skipping the park sweep");
                return;
            }
        };
        let now = now_millis();
        let (mut released, mut dropped) = (0usize, 0usize);
        for op in &parked {
            let Some(uid) = parse_node_uid(&op.uid) else {
                continue;
            };
            let name = op.name.as_deref().unwrap_or("?");
            let listed = match self.db.node_by_uid(&op.uid) {
                Ok(known) => known.is_some(),
                // Never infer "deleted" from a failed read: that would discard
                // the bytes over a transient database error.
                Err(error) => {
                    debug!(%uid, %error, "cannot tell whether a parked node still exists");
                    continue;
                }
            };
            match park_verdict(op.created_at, now, self.uid_is_open(&uid), listed) {
                ParkVerdict::Keep => continue,
                ParkVerdict::Drop => {
                    if let Err(error) = self.discard_queued_ops(&uid) {
                        warn!(%uid, name, ?error, "dropping a deleted transient's parked create failed");
                        continue;
                    }
                    dropped += 1;
                    debug!(%uid, name, "dropped a parked create whose file was deleted");
                    continue;
                }
                ParkVerdict::Release => {}
            }
            match self.db.set_create_hold(&op.uid, false) {
                Ok(true) => {
                    released += 1;
                    info!(%uid, name, parked_ms = now - op.created_at,
                          "un-parked a transient create that was never renamed; uploading it");
                }
                Ok(false) => {}
                Err(error) => warn!(%uid, name, %error, "un-parking an expired park failed"),
            }
        }
        if released > 0 || dropped > 0 {
            info!(released, dropped, "swept parked creates");
        }
        if released > 0 {
            self.wake_drain();
        }
    }
}
