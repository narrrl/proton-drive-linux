//! Trashing photos by uid. This stays on [`Core`] rather than [`Photos`]: it
//! checks write authority against the tree, drops open readers and clears the
//! trash listing, all of which belong to the filesystem side.

use super::*;

impl Core {
    /// Mutation admission for a photo, which [`Core::require_uid_writable`]
    /// cannot answer on its own.
    ///
    /// Photos live on the photo volume, and that volume is not part of the FUSE
    /// tree: a photo has no `nodes` row, so every authority comes back
    /// [`WriteAuthority::Unknown`] and the plain check refuses the whole
    /// library. Membership of our own timeline is the authority that does exist
    /// here — the timeline is enumerated from our photo share, so a uid in it is
    /// ours to trash. Anything else (an album photo on someone else's volume)
    /// still fails closed.
    pub(crate) fn require_photo_writable(&self, uid: &NodeUid) -> Result<(), fuser::Errno> {
        match self.uid_write_authority(uid) {
            WriteAuthority::Writable => Ok(()),
            WriteAuthority::Denied => Err(fuser::Errno::EACCES),
            WriteAuthority::Unknown => {
                let key = uid.to_string();
                let known = self
                    .db
                    .photos_by_uid(std::slice::from_ref(&key))
                    .map(|rows| !rows.is_empty())
                    .unwrap_or(false);
                if known {
                    Ok(())
                } else {
                    Err(fuser::Errno::EACCES)
                }
            }
        }
    }

    /// Trash photos by uid, and forget them locally.
    ///
    /// The gallery is not part of the FUSE mount, so this is the photos-side
    /// counterpart of [`Core::delete`]: same destination (Proton trash, so the
    /// Trash page can restore them), addressed by uid because that is all a tile
    /// has.
    ///
    /// The server answers per node, and so does this: a batch where one photo
    /// fails still removes the ones that went, and the caller is told which is
    /// which. Only what actually left the server is forgotten locally.
    ///
    /// A photo that is one file of a group takes its whole group with it. The
    /// gallery shows a RAW+JPEG shot as one tile, so deleting that tile has to
    /// delete the shot — leaving the RAW behind would put the photo back on the
    /// Raw tab and nowhere else. `files_only` turns that off, for removing
    /// one copy of a shot that is stored twice.
    pub(crate) fn trash_photos(
        &self,
        uids: &[NodeUid],
        files_only: bool,
    ) -> CoreResult<TrashOutcome> {
        if uids.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let expanded;
        let uids = if files_only {
            uids
        } else {
            expanded = self.photos().expand_photo_groups(uids);
            &expanded
        };
        for uid in uids {
            self.require_photo_writable(uid)
                .map_err(|errno| self.errno_error(errno, "trash access"))?;
        }
        let outcomes = self
            .rt
            .block_on(self.drive.trash_nodes(uids))
            .map_err(|e| CoreError::from_api(&e, "trash"))?;
        let (trashed, failed) = pdfs_core::batch::split(outcomes);

        let trashed: Vec<String> = trashed.iter().map(|uid| uid.to_string()).collect();
        for uid in &trashed {
            if let Some(parsed) = parse_uid(uid) {
                self.cache.evict(&parsed);
                self.evict_reader(&parsed);
            }
        }
        // The timeline is served from the database, so removing the rows is what
        // makes the photos disappear now rather than at the next refresh.
        if let Err(error) = self.db.photos_delete(&trashed) {
            warn!(%error, "trashed photos could not be removed from the timeline");
        }
        self.invalidate_photos();
        self.photos().invalidate_albums();
        self.invalidate_trash();

        let failed = failed
            .into_iter()
            .map(|(uid, error)| (uid.to_string(), error.to_string()))
            .collect();
        Ok((trashed, failed))
    }
}
