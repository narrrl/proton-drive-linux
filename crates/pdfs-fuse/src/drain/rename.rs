//! Draining a rename or a move.

use super::*;

impl Core {
    /// Apply a queued rename/move to the remote.
    ///
    /// The op is the desired end state, so the remote's current state decides
    /// what actually has to be called: either half may already match (the event
    /// sync saw someone else do it, or an earlier attempt got half way through
    /// before failing). That also makes the whole thing idempotent, which a
    /// retrying queue needs.
    pub(crate) fn drain_rename(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let uid = parse_node_uid(&op.uid).ok_or("rename op has an unparseable uid")?;
        let parent_str = op.parent_uid.as_deref().ok_or("rename op has no parent")?;
        if is_local_uid_str(parent_str) {
            return Err(format!("destination {parent_str} has not been created yet").into());
        }
        let parent = parse_node_uid(parent_str).ok_or("rename op has an unparseable parent")?;
        let name = op.name.clone().ok_or("rename op has no name")?;

        // A trashed node can be restored. Only retire the operation if the node
        // is gone entirely.
        let node = match self.fetch_node_remote(&uid)? {
            Some(n) if n.trashed => return Err(TrashedMove(n.name).into()),
            Some(n) => n,
            None => {
                warn!(%uid, name, "renamed node is gone remotely; dropping the rename");
                self.db.delete_op(op.id)?;
                return Ok(());
            }
        };
        let meta: Option<RenameMeta> = op
            .meta_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        if let Some(meta) = &meta
            && moved_elsewhere(meta, &node, &parent, &name)
        {
            // Someone moved or renamed it while ours was queued. Ours still
            // wins, as the last change does on Drive.
            info!(%uid, from = %node.name, to = %name,
                  "node moved remotely while its rename was queued");
        }
        let landed = if node.parent_uid.as_ref() == Some(&parent) {
            self.drain_rename_in_place(op, &uid, Some(&parent), &node.name, &name)?
        } else {
            // Move and rename land as one request (`move-multiple` takes a
            // target name), so there is no half-applied state between them and
            // no window for the requirements to go stale between two calls —
            // the race B46 queued this op to get away from.
            let target = (node.name != name).then_some(name.as_str());
            let try_move = |target| {
                self.move_rename_remote(&uid, &parent, target).map_err(
                    |error| -> Box<dyn std::error::Error> {
                        if (is_gone(&error) || refusal(&error) == Some(Some(SyncIssue::Access)))
                            && let Some(name) = trashed_move_folder(
                                node.parent_uid.iter().chain(std::iter::once(&parent)),
                                |uid| self.fetch_node_remote(uid).ok().flatten(),
                            )
                        {
                            return TrashedMove(name).into();
                        }
                        error.into()
                    },
                )
            };
            match try_move(target) {
                Ok(()) => name.clone(),
                // The destination holds that name already. Landing under a
                // *different* name is the non-destructive resolution: it neither
                // clobbers their file nor drops ours, and it is visible.
                Err(e) if is_already_exists(e.as_ref()) => {
                    if self.name_is_held(op, Some(&parent), &name)? {
                        return Err(held_by_queued_change(&name));
                    }
                    // What held it may have let go since Drive answered
                    // (docs/BUGS.md B144).
                    match try_move(target) {
                        Ok(()) => name.clone(),
                        Err(e) if is_already_exists(e.as_ref()) => {
                            let alt = conflict_name(&name, now_secs());
                            warn!(%uid, name, alt, "destination already holds that name; using a conflict name");
                            try_move(Some(&alt))?;
                            self.adopt_drained_name(&uid, &alt);
                            self.log_activity(
                                ActivityKind::Rename,
                                &name,
                                format!("destination already had that name; moved as {alt}"),
                                false,
                            );
                            alt
                        }
                        Err(e) => return Err(e),
                    }
                }
                // The destination folder is gone. Leaving the node in its current
                // parent is the honest outcome: it is not where the user asked for
                // it, but it exists and it is where it has always been. The name
                // the user chose still applies there. Retrying the move could
                // only fail again — the folder is not coming back — and would
                // wedge the queue.
                Err(e) if is_gone(e.as_ref()) => {
                    warn!(%uid, name, %parent, "move destination is gone; leaving the node where it is");
                    let landed = match self.drain_rename_in_place(
                        op,
                        &uid,
                        node.parent_uid.as_ref(),
                        &node.name,
                        &name,
                    ) {
                        Ok(landed) => landed,
                        // The node went with it; nothing is left to rename.
                        Err(e) if is_gone(e.as_ref()) => node.name.clone(),
                        Err(e) => return Err(e),
                    };
                    self.log_activity(
                        ActivityKind::Rename,
                        &landed,
                        "destination folder no longer exists; the file was left in place"
                            .to_string(),
                        false,
                    );
                    landed
                }
                Err(e) => return Err(e),
            }
        };
        self.db.delete_op(op.id)?;
        // An op that wanted the name this node let go of waits no longer.
        if let Some(from) = meta.and_then(|m| m.original_name)
            && from != landed
        {
            self.db.wake_ops_waiting_for(&from)?;
            self.wake_drain();
        }
        info!(%uid, name = %landed, "pending rename landed");
        Ok(())
    }

    /// Rename `uid` without moving it, from `current` to `name`, and return the
    /// name it landed under. Someone may have taken the name while we were
    /// offline; the node then lands under a conflict name instead.
    pub(crate) fn drain_rename_in_place(
        &self,
        op: &PendingOp,
        uid: &NodeUid,
        parent: Option<&NodeUid>,
        current: &str,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        if current == name {
            return Ok(current.to_string());
        }
        match self.rename_remote(uid, name) {
            Ok(()) => Ok(name.to_string()),
            Err(e) if is_already_exists(&e) => {
                if self.name_is_held(op, parent, name)? {
                    return Err(held_by_queued_change(name));
                }
                // What held it may have let go since Drive answered
                // (docs/BUGS.md B144).
                match self.rename_remote(uid, name) {
                    Ok(()) => return Ok(name.to_string()),
                    Err(e) if is_already_exists(&e) => {}
                    Err(e) => return Err(e.into()),
                }
                let alt = conflict_name(name, now_secs());
                warn!(%uid, name, alt, "rename target name is taken; using a conflict name");
                self.rename_remote(uid, &alt)?;
                self.adopt_drained_name(uid, &alt);
                self.log_activity(
                    ActivityKind::Rename,
                    name,
                    format!("name was taken remotely; renamed to {alt}"),
                    false,
                );
                Ok(alt)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Record the name the remote actually gave a node, after a conflict forced
    /// it away from the one the user asked for. Best effort: the event sync
    /// would correct the tree anyway, but not before the user has looked at it.
    pub(crate) fn adopt_drained_name(&self, uid: &NodeUid, name: &str) {
        self.for_each_state(|st| {
            let Some(&ino) = st.by_uid.get(uid) else {
                return;
            };
            let Some(entry) = st.entries.get_mut(&ino) else {
                return;
            };
            entry.node.name = name.to_string();
            let node = entry.node.clone();
            if let Err(e) = st.db.upsert_node(&node) {
                warn!(%uid, error = %e, "db upsert_node failed after a conflict rename");
            }
        });
    }
}
