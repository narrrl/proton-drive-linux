//! Draining a trash, and withdrawing a create that was removed before it
//! landed.

use super::*;

impl Core {
    /// Apply a queued trash to the remote.
    ///
    /// A node that is already gone is a success, not a failure: the outcome the
    /// op asked for holds either way, and retrying forever against a node the
    /// server has forgotten would wedge the queue.
    pub(crate) fn drain_trash(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let uid = parse_node_uid(&op.uid).ok_or("trash op has an unparseable uid")?;
        let name = op.name.clone().unwrap_or_else(|| op.uid.clone());
        // The trash a delete left of a create that was sent, on the wire or
        // before, and failed (`Db::delete_ops_for_uid`). A create that failed
        // may still have made the node and lost only its answer, and the node
        // would keep the deleted file's name and bytes (docs/BUGS.md B151).
        if is_local_uid_str(&op.uid) {
            match self.withdrawn_twin(op)? {
                Some(twin) => {
                    self.withdraw_landed(&twin);
                    match self
                        .block_on_bounded(self.drive.trash_nodes(std::slice::from_ref(&twin)))
                        .and_then(batch::into_unit)
                    {
                        Ok(()) => {}
                        Err(e) if is_gone(&e) => {}
                        Err(e) => return Err(e.into()),
                    }
                    self.invalidate_trash();
                    info!(%uid, %twin, name, "a withdrawn create had landed unanswered; trashed it");
                }
                None => debug!(%uid, name, "withdrawn create never landed; trash op satisfied"),
            }
            self.retire_trash_op(op, &uid)?;
            return Ok(());
        }
        match self
            .block_on_bounded(self.drive.trash_nodes(std::slice::from_ref(&uid)))
            .and_then(batch::into_unit)
        {
            Ok(()) => {}
            Err(e) if is_gone(&e) => {
                debug!(%uid, name, "node was already gone remotely; trash op satisfied");
            }
            Err(e) => return Err(e.into()),
        }
        self.retire_trash_op(op, &uid)?;
        // A file trashed while open kept the bytes its handle reads, and a
        // final close while the trash was queued left them to this
        // (`docs/BUGS.md` B186).
        if !self.is_open_anywhere(&uid) {
            if let Some(blob) = self.pending_blob(&uid) {
                self.release_pending(&uid, &blob);
                self.cache.discard_staged(&blob);
            }
            self.cache.evict(&uid);
            self.evict_reader(&uid);
        }
        if let Err(e) = self.db.clear_own_sealed_rev(&uid.to_string()) {
            debug!(%uid, error = %e, "clearing the sealed-revision record failed");
        }
        self.drain.placeholders.lock().remove(&uid);
        self.invalidate_trash();
        self.log_activity(ActivityKind::Trash, &name, "trashed", true);
        info!(%uid, name, "pending trash landed");
        Ok(())
    }

    /// Take a node a withdrawn create made on Drive out of every listing.
    ///
    /// Hiding keeps it out of listings from now on, not out of one taken while
    /// Drive listed it and the drain still waited for the answer. A file made
    /// under the name then opened the node being trashed, and every write to
    /// it was refused with EACCES (`docs/BUGS.md` B303).
    fn withdraw_landed(&self, uid: &NodeUid) {
        self.writes.hide(uid);
        self.for_each_mount(|st, notify| {
            if let Some((parent, name)) = st.forget_or_unlink(uid) {
                notify.inval_entry(parent, name);
            }
        });
    }

    /// Remove a trash op that landed, and the blob of a create it withdrew.
    pub(crate) fn retire_trash_op(
        &self,
        op: &PendingOp,
        uid: &NodeUid,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(blob) = self.db.complete_trash_op(op.id, uid)? {
            self.cache.discard_staged(Path::new(&blob));
        }
        Ok(())
    }

    /// The node a create that a trash withdrew made on Drive although its
    /// answer was lost: in the create's folder under its name, or under one
    /// an attempt was sent with before a rename (`docs/BUGS.md` B184).
    pub(crate) fn withdrawn_twin(
        &self,
        op: &PendingOp,
    ) -> Result<Option<NodeUid>, Box<dyn std::error::Error>> {
        let mut targets: Vec<(String, String)> = op
            .parent_uid
            .clone()
            .zip(op.name.clone())
            .into_iter()
            .collect();
        for target in self.db.create_targets(op.id)? {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        for (parent, name) in targets {
            if is_local_uid_str(&parent) {
                continue;
            }
            let Some(parent) = parse_node_uid(&parent) else {
                continue;
            };
            if let Some(twin) = self.withdrawn_twin_in(op, &parent, &name)? {
                return Ok(Some(twin));
            }
        }
        Ok(None)
    }

    /// The node in `parent` under `name` a withdrawn create made, if it is one
    /// the create would have adopted had it been retried ([`adoptable`]). A
    /// node with a change of ours queued is one the user is still working on.
    pub(crate) fn withdrawn_twin_in(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<Option<NodeUid>, Box<dyn std::error::Error>> {
        let uids =
            match self.block_on_bounded(self.drive.enumerate_folder_children_node_uids(parent)) {
                Ok(uids) => uids,
                Err(e) if is_gone(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
        let children = self.block_on_bounded(self.drive.enumerate_nodes_light(&uids))?;
        // As for a create's own twin (`Core::adoptable_twin`): a node this
        // mount trashed is not one, and a node in the tree was answered for.
        // A file of the user's renamed to the name the create was sent under
        // was taken for it, and trashed (`docs/BUGS.md` B193).
        let hidden = self.writes.hidden();
        let Some(twin) = children
            .into_iter()
            .find(|node| node.name == name && !node.trashed && !hidden.contains(&node.uid))
        else {
            return Ok(None);
        };
        if self.state().by_uid.contains_key(&twin.uid) {
            return Ok(None);
        }
        let queued = twin.uid.to_string();
        for kind in [OP_RENAME, OP_REVISION, OP_TRASH] {
            if self.db.has_pending_op(&queued, kind)? {
                return Ok(None);
            }
        }
        let is_dir = match twin.kind {
            // A folder made by a create that never answered has nothing in
            // it: what was queued inside waited for its uid.
            NodeKind::Folder => {
                let inside = self
                    .block_on_bounded(self.drive.enumerate_folder_children_node_uids(&twin.uid))?;
                if !inside.is_empty() {
                    return Ok(None);
                }
                true
            }
            NodeKind::File { .. } => false,
        };
        let twin = match is_dir {
            true => twin,
            false => match self.fetch_node_remote(&twin.uid)? {
                Some(node) => node,
                None => return Ok(None),
            },
        };
        // A blob gone from staging matches nothing, and leaves the node be.
        let current = match op.blob_path.as_deref() {
            Some(blob) if node_size(&twin) > 0 => staged_sha1(Path::new(blob)).ok(),
            _ => None,
        };
        let sent = self.create_sent_digests(op.id, current);
        Ok(adoptable(is_dir, &twin, self.first_sent(op), &sent).then_some(twin.uid))
    }

    /// Trash the node a create made after the file was deleted, or replaced by
    /// a rename, while its upload was on the wire (`docs/BUGS.md` B129).
    ///
    /// `Db::finish_create` queued the trash, claimed, in the transaction that
    /// found the create gone. It goes out now rather than behind the queue:
    /// until it lands the node holds its name on Drive, and a file created
    /// there again forks a conflict copy. A failure leaves it queued.
    pub(crate) fn trash_withdrawn_create(
        &self,
        op: &PendingOp,
        trash: i64,
        local: &NodeUid,
        real: &NodeUid,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.withdraw_landed(real);
        self.note_self_change(real);
        let trash = PendingOp {
            id: trash,
            kind: OP_TRASH.to_string(),
            uid: real.to_string(),
            parent_uid: None,
            name: Some(name.to_string()),
            blob_path: None,
            meta_json: None,
            created_at: op.created_at,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };
        info!(%local, %real, name, "a create landed after its file was deleted; trashing it");
        if let Err(e) = self.drain_trash(&trash) {
            warn!(%real, name, error = %e, "trashing a withdrawn create failed; queued");
            if let Err(e) = self.db.record_op_failure(
                trash.id,
                &e.to_string(),
                now_millis() + DRAIN_BACKOFF_MIN.as_millis() as i64,
            ) {
                error!(%real, error = %e, "recording a drain failure failed");
            }
            self.release_claim(&trash);
            self.wake_drain();
        }
        Ok(())
    }
}
