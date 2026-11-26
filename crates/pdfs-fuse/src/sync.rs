//! The daemon's side of the mirror engine, which lives in `pdfs-sync`:
//! [`Core`] is the engine's [`SyncHost`].

use std::path::Path;
use std::sync::Arc;

use pdfs_core::control::ActivityKind;
use proton_drive_rs::Thumbnail;
use proton_drive_rs::proton_sdk::ids::NodeUid;

pub(crate) use pdfs_sync::mirror::*;

use crate::Core;

impl SyncHost for Core {
    fn log_activity(&self, kind: ActivityKind, target: String, detail: String, ok: bool) {
        Core::log_activity(self, kind, target, detail, ok);
    }

    fn invalidate_children_of(&self, parent: &NodeUid) {
        Core::invalidate_children_of(self, parent);
    }

    fn attach_ancestors(&self, start: &NodeUid) -> pdfs_core::CoreResult<Vec<NodeUid>> {
        Core::attach_ancestors(self, start)
    }

    fn settle_pending_mode(&self, id: i64) {
        Core::settle_pending_mode(self, id);
    }

    fn sync_paused(&self) -> bool {
        Core::sync_paused(self)
    }

    fn sync_folder_paused(&self, id: i64) -> bool {
        Core::sync_folder_paused(self, id)
    }

    fn upload_thumbnails(&self, path: &Path, name: &str) -> Vec<Thumbnail> {
        Core::upload_thumbnails(self, path, name)
    }
}

impl Core {
    /// The sync engine, with this daemon as its host.
    pub fn syncer(&self) -> Syncer {
        Syncer::new(self.sync.clone(), Arc::new(self.clone()))
    }
}
