//! Changes sent straight to Drive (renames, moves, content changes) and what
//! each one changes in the tree once Drive has answered.

use super::*;

impl Core {
    /// Rename `uid` on Drive, riding out an "out of date" answer from a stale
    /// name hash. See [`retry_out_of_date`].
    pub(crate) fn rename_remote(&self, uid: &NodeUid, name: &str) -> Result<(), ProtonError> {
        self.change_remote(uid, || {
            self.block_on_bounded(self.drive.rename_node(uid, name, None))
        })
    }

    /// Move `uid` into the folder `parent` on Drive, like [`Core::rename_remote`].
    pub(crate) fn move_remote(&self, uid: &NodeUid, parent: &NodeUid) -> Result<(), ProtonError> {
        self.change_remote(uid, || {
            self.block_on_bounded(self.drive.move_node(uid, parent))
        })
    }

    /// Move `uid` into the folder `parent` on Drive, renaming it to
    /// `target_name` on the way when one is given, in one request.
    ///
    /// Done as a rename and then a move, a sibling in the source folder that
    /// held the new name made Drive refuse the first half, although the
    /// destination was free (docs/BUGS.md B122). One request also leaves no
    /// half-applied state between the two.
    pub(crate) fn move_rename_remote(
        &self,
        uid: &NodeUid,
        parent: &NodeUid,
        target_name: Option<&str>,
    ) -> Result<(), ProtonError> {
        self.change_remote(uid, || {
            let item = NodeMoveItem {
                uid: uid.clone(),
                target_name: target_name.map(str::to_string),
            };
            self.block_on_bounded(async {
                let mut outcomes =
                    std::pin::pin!(self.drive.move_nodes_streaming(vec![item], parent.clone()));
                match outcomes.next().await {
                    Some(Ok((_, outcome))) => outcome,
                    Some(Err(e)) => Err(e),
                    None => Err(ProtonError::invalid_operation(format!(
                        "move of {uid} reported no outcome"
                    ))),
                }
            })
        })
    }

    /// Run a rename or move of `uid`, then drop the SDK's now stale copy of it.
    pub(super) fn change_remote(
        &self,
        uid: &NodeUid,
        call: impl FnMut() -> Result<(), ProtonError>,
    ) -> Result<(), ProtonError> {
        retry_out_of_date(call, || {
            self.forget_sdk_node(uid);
            std::thread::sleep(OUT_OF_DATE_RETRY_DELAY);
        })
        .inspect(|()| self.changed_remote(uid))
    }

    /// Bookkeeping after a rename or move of `uid` landed: the SDK's copy of it
    /// is stale, and the feed will echo the change back. Without the second,
    /// that echo reads as someone else's edit and evicts the cached bytes a
    /// partial write is about to gap-fill from (docs/BUGS.md B123).
    pub(crate) fn changed_remote(&self, uid: &NodeUid) {
        self.forget_sdk_node(uid);
        self.note_self_change(uid);
    }

    /// Drop the SDK's cached copy of `uid` after this daemon renamed or moved it.
    ///
    /// The SDK sends a node's name hash from its entity cache, and nothing but
    /// the event feed's echo of the change refreshes that entry. Until the next
    /// poll, up to [`POLL_INTERVAL`] later, each rename or move of the node would
    /// send the old hash and Drive would refuse it as out of date (docs/BUGS.md
    /// B121). Feeding the SDK the echo now makes the next call read the link.
    pub(crate) fn forget_sdk_node(&self, uid: &NodeUid) {
        let changed = DriveEvent::NodeUpdated {
            id: DriveEventId::from("pdfs-local-change"),
            node_uid: uid.clone(),
            parent_node_uid: None,
            is_trashed: false,
            is_shared: false,
        };
        if let Err(error) = self.block_on_bounded(self.drive.invalidate_caches_for_event(&changed))
        {
            debug!(%uid, %error, "could not drop the SDK's cached node");
        }
    }
}
