//! Explicitly bind an existing My files folder to a real local mirror.
//!
//! Unlike a FUSE mount, this never covers or evicts the local directory.
//! The existing three-way engine performs the initial, non-destructive union.

use super::{Core, parse_uid, sync};
use pdfs_core::db::StoredSyncFolder;
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

impl Core {
    /// A bounded, server-verified ancestry walk. Do not decide ownership or
    /// remote-root overlap from a possibly incomplete cached folder listing.
    fn attach_ancestors(&self, start: &NodeUid) -> CoreResult<Vec<NodeUid>> {
        let mut ancestors = Vec::new();
        let mut seen = HashSet::new();
        let mut next = Some(start.clone());
        while let Some(uid) = next {
            if !seen.insert(uid.clone()) || ancestors.len() >= 256 {
                return Err(CoreError::invalid(
                    "invalid or excessively deep remote folder ancestry",
                ));
            }
            ancestors.push(uid.clone());
            if uid == self.primary_root_uid {
                break;
            }
            let nodes = self
                .rt
                .block_on(self.drive.enumerate_nodes(std::slice::from_ref(&uid)))
                .map_err(|e| CoreError::from_api(&e, "verify sync folder ancestry"))?;
            let node = nodes
                .into_iter()
                .next()
                .ok_or_else(|| CoreError::not_found("remote sync folder is missing"))?;
            if node.trashed {
                return Err(CoreError::invalid("remote sync folder is in the trash"));
            }
            next = node.parent_uid;
        }
        Ok(ancestors)
    }

    pub(crate) fn attach_sync_folder(
        &self,
        local: &Path,
        remote: &Path,
        mountpoint: &Path,
    ) -> CoreResult<StoredSyncFolder> {
        let _registration = super::devices::SYNC_ROOT_REGISTRATION.lock();
        if !self.primary {
            return Err(CoreError::invalid("attach must use My files"));
        }
        if remote.is_absolute()
            || remote
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(CoreError::invalid(
                "remote folder must stay inside My files",
            ));
        }
        let local = local
            .canonicalize()
            .map_err(|e| CoreError::invalid(format!("resolve {}: {e}", local.display())))?;
        if !local.is_dir() {
            return Err(CoreError::invalid("local sync path is not a directory"));
        }
        let existing = self
            .db
            .sync_folder_list()
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
        let others = existing
            .iter()
            .map(|f| PathBuf::from(&f.local_path))
            .collect::<Vec<_>>();
        let mut reserved = crate::devices::sync_root_reserved();
        reserved.mountpoint = Some(
            mountpoint
                .canonicalize()
                .map_err(|e| CoreError::internal(format!("resolve mountpoint: {e}")))?,
        );
        if let Some(problem) = pdfs_core::syncroot::problem(&local, &others, &reserved) {
            return Err(CoreError::invalid(problem));
        }
        let (ino, remote_uid) = self.resolve(remote)?;
        let is_folder = self
            .state()
            .entries
            .get(&ino)
            .is_some_and(|e| e.node.is_folder() && !e.node.trashed);
        if !is_folder {
            return Err(CoreError::invalid("remote sync path is not a folder"));
        }
        let ancestors = self.attach_ancestors(&remote_uid)?;
        if remote_uid == self.primary_root_uid || !ancestors.contains(&self.primary_root_uid) {
            return Err(CoreError::invalid(
                "choose an owned folder below My files, not its root or a shared folder",
            ));
        }
        for other in &existing {
            let uid = parse_uid(&other.remote_uid)
                .ok_or_else(|| CoreError::internal("invalid existing remote sync root"))?;
            if uid == remote_uid
                || ancestors.contains(&uid)
                || self.attach_ancestors(&uid)?.contains(&remote_uid)
            {
                return Err(CoreError::invalid(
                    "remote folder overlaps an existing synced folder",
                ));
            }
        }
        let share_id = self
            .rt
            .block_on(self.drive.context_share_id(&self.primary_root_uid))
            .map_err(|e| CoreError::from_api(&e, "resolve My files share"))?
            .to_string();
        if share_id.is_empty() {
            return Err(CoreError::invalid("My files share is unavailable"));
        }
        let id = self
            .db
            .sync_folder_add(&local.to_string_lossy(), &remote_uid.to_string(), &share_id)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?;
        // An empty baseline makes one-sided files new, never deletions. The
        // engine hashes matching-size files and preserves divergent versions.
        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
        let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));
        self.db
            .sync_folder_get(id)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .ok_or_else(|| CoreError::not_found("new sync folder disappeared"))
    }
}
