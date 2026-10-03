//! File version history: listing a file's revisions, restoring one, deleting
//! one, and writing an old one out to a local path.
//!
//! Proton Drive keeps every revision a client ever committed, and until now the
//! daemon only ever addressed the active one — a file that a sync pass or a
//! `(sync-conflict)` fork overwrote could only be recovered from whatever the
//! local `recovery/` directory happened to hold. This is the remote equivalent,
//! and it is the reason a *restore* here is not a download-then-upload: the
//! server re-points the file at an existing revision, so no content crosses the
//! wire and nothing lands in the drain queue.
//!
//! Two things about the SDK surface leak into every method here:
//!
//! - **A restore is asynchronous.** The server answers 202 and swaps the active
//!   revision in the background, so the daemon drops its cached content for the
//!   file rather than trying to describe the new one. The next read repopulates
//!   from whatever is live by then.
//! - **The active revision cannot be deleted**, and the server — not this code —
//!   is what enforces that. [`Core::delete_revision_for_uid`] refuses locally
//!   too, so the user gets a sentence instead of an API code.

use std::path::Path;

use pdfs_core::control::{ActivityKind, RevisionInfo};
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::Revision;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::info;

use super::Core;

/// A [`Revision`] as the control protocol reports it.
fn revision_info(revision: Revision) -> RevisionInfo {
    RevisionInfo {
        is_active: revision.is_active(),
        id: revision.revision_id,
        created: revision.creation_time,
        size_on_storage: revision.size_on_storage,
        claimed_size: revision.claimed_size,
        claimed_modified: revision.claimed_modification_time,
        // An empty signer means the node key signed it anonymously, which is not
        // an address a front-end can show.
        signed_by: revision.signature_email.filter(|email| !email.is_empty()),
        has_thumbnails: revision.has_thumbnails,
    }
}

impl Core {
    /// The version history of the file at mountpoint-relative `rel`.
    pub(crate) fn list_revisions(&self, rel: &Path) -> CoreResult<Vec<RevisionInfo>> {
        let (_ino, uid) = self.resolve(rel)?;
        self.list_revisions_for_uid(&uid)
    }

    /// [`Core::list_revisions`] for a node addressed through any daemon location.
    pub(crate) fn list_revisions_by_uid(&self, uid: &str) -> CoreResult<Vec<RevisionInfo>> {
        let uid = self.resolve_anywhere(uid)?;
        self.list_revisions_for_uid(&uid)
    }

    fn list_revisions_for_uid(&self, uid: &NodeUid) -> CoreResult<Vec<RevisionInfo>> {
        let revisions = self
            .rt
            .block_on(self.client.enumerate_revisions(uid))
            .map_err(|e| CoreError::from_api(&e, "list revisions"))?;
        Ok(revisions.into_iter().map(revision_info).collect())
    }

    /// Make `revision_id` the current content of the file at `rel`.
    pub(crate) fn restore_revision(&self, rel: &Path, revision_id: &str) -> CoreResult<()> {
        let (_ino, uid) = self.resolve(rel)?;
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| uid.to_string());
        self.restore_revision_for_uid(&uid, revision_id, &name)?;
        // The size and mtime a listing shows describe the revision that was
        // active a moment ago.
        self.invalidate_parent_listing(rel);
        Ok(())
    }

    /// [`Core::restore_revision`] for a node addressed through any daemon
    /// location.
    pub(crate) fn restore_revision_by_uid(&self, uid: &str, revision_id: &str) -> CoreResult<()> {
        let uid = self.resolve_anywhere(uid)?;
        let name = uid.to_string();
        self.restore_revision_for_uid(&uid, revision_id, &name)
    }

    fn restore_revision_for_uid(
        &self,
        uid: &NodeUid,
        revision_id: &str,
        name: &str,
    ) -> CoreResult<()> {
        // Restoring rewrites the file's content: the same authority a write
        // needs, not the one a read needs.
        self.require_uid_writable(uid)
            .map_err(|error| self.errno_error(error, "restore revision"))?;
        if let Err(e) = self
            .rt
            .block_on(self.client.restore_revision(uid, revision_id))
        {
            let error = CoreError::from_api(&e, "restore revision");
            self.log_activity(ActivityKind::Restore, name, &error, false);
            return Err(error);
        }
        // The active revision is changing behind our back — anything describing
        // the old one (cached blocks, an open reader, the size in a listing) is
        // stale from here on, and the server may not have applied the swap yet.
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.log_activity(
            ActivityKind::Restore,
            name,
            format!("restored version {revision_id}"),
            true,
        );
        info!(%uid, revision_id, name, "restored an earlier revision");
        Ok(())
    }

    /// Permanently delete `revision_id` from the history of the file at `rel`.
    pub(crate) fn delete_revision(&self, rel: &Path, revision_id: &str) -> CoreResult<()> {
        let (_ino, uid) = self.resolve(rel)?;
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| uid.to_string());
        self.delete_revision_for_uid(&uid, revision_id, &name)
    }

    /// [`Core::delete_revision`] for a node addressed through any daemon
    /// location.
    pub(crate) fn delete_revision_by_uid(&self, uid: &str, revision_id: &str) -> CoreResult<()> {
        let uid = self.resolve_anywhere(uid)?;
        let name = uid.to_string();
        self.delete_revision_for_uid(&uid, revision_id, &name)
    }

    fn delete_revision_for_uid(
        &self,
        uid: &NodeUid,
        revision_id: &str,
        name: &str,
    ) -> CoreResult<()> {
        self.require_uid_writable(uid)
            .map_err(|error| self.errno_error(error, "delete revision"))?;
        // The server rejects deleting the active revision; saying so here is a
        // sentence the user can act on instead of an API code, and it costs one
        // listing the front-end already made to show the version list.
        let revisions = self
            .rt
            .block_on(self.client.enumerate_revisions(uid))
            .map_err(|e| CoreError::from_api(&e, "list revisions"))?;
        let target = revisions
            .iter()
            .find(|r| r.revision_id == revision_id)
            .ok_or_else(|| CoreError::not_found(format!("no version {revision_id} on {name}")))?;
        if target.is_active() {
            return Err(CoreError::invalid(format!(
                "version {revision_id} is the current content of {name}; restore another version first"
            )));
        }
        self.rt
            .block_on(self.client.delete_revision(uid, revision_id))
            .map_err(|e| CoreError::from_api(&e, "delete revision"))?;
        self.log_activity(
            ActivityKind::DeleteForever,
            name,
            format!("deleted version {revision_id}"),
            true,
        );
        info!(%uid, revision_id, name, "deleted a revision");
        Ok(())
    }

    /// Write `revision_id` of the file at `rel` to the absolute local path
    /// `dest`, leaving the file itself untouched.
    pub(crate) fn save_revision_as(
        &self,
        rel: &Path,
        revision_id: &str,
        dest: &Path,
    ) -> CoreResult<u64> {
        let (_ino, uid) = self.resolve(rel)?;
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| uid.to_string());
        self.save_revision_for_uid(&uid, revision_id, &name, dest)
    }

    /// [`Core::save_revision_as`] for a node addressed through any daemon
    /// location.
    pub(crate) fn save_revision_as_by_uid(
        &self,
        uid: &str,
        revision_id: &str,
        dest: &Path,
    ) -> CoreResult<u64> {
        let uid = self.resolve_anywhere(uid)?;
        let name = uid.to_string();
        self.save_revision_for_uid(&uid, revision_id, &name, dest)
    }

    fn save_revision_for_uid(
        &self,
        uid: &NodeUid,
        revision_id: &str,
        name: &str,
        dest: &Path,
    ) -> CoreResult<u64> {
        if !dest.is_absolute() {
            return Err(CoreError::invalid(format!(
                "destination must be an absolute path: {}",
                dest.display()
            )));
        }
        // Never silently replace something the user already has: an old version
        // written over the wrong file is exactly the loss this feature exists to
        // undo.
        if dest.exists() {
            return Err(CoreError::invalid(format!(
                "{} already exists",
                dest.display()
            )));
        }
        let file = std::fs::File::create(dest)
            .map_err(|e| CoreError::internal(format!("create {}: {e}", dest.display())))?;
        let mut out = std::io::BufWriter::new(file);
        let written = match self
            .rt
            .block_on(self.drive.download_revision_to(uid, revision_id, &mut out))
            .map_err(|e| CoreError::from_api(&e, "download revision"))
            .and_then(|()| {
                use std::io::Write as _;
                out.flush()
                    .map_err(|e| CoreError::internal(format!("write {}: {e}", dest.display())))?;
                let written = std::fs::metadata(dest)
                    .map(|m| m.len())
                    .map_err(|e| CoreError::internal(format!("stat {}: {e}", dest.display())))?;
                Ok(written)
            }) {
            Ok(written) => written,
            Err(error) => {
                // A half-written file looks like a successful export to every
                // tool that opens it afterwards.
                let _ = std::fs::remove_file(dest);
                self.log_activity(ActivityKind::Download, name, &error, false);
                return Err(error);
            }
        };
        self.log_activity(
            ActivityKind::Download,
            name,
            format!("saved version {revision_id} to {}", dest.display()),
            true,
        );
        info!(%uid, revision_id, dest = %dest.display(), "wrote an earlier revision to disk");
        Ok(written)
    }
}
