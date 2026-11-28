//! A [`DriveApi`] that counts and times every call it forwards.
//!
//! Wrapping the client in [`Counted`] answers "what is the daemon asking Drive,
//! and how long does Drive take" without a debugger or a log at debug level:
//! [`CallStats::snapshot`] gives, per kind of call, how many were made, how
//! many failed, and their total and longest time. The diagnostics report shows
//! it.
//!
//! A streaming call (a batch move, restore or delete) is counted once when it
//! starts. Its time is not measured, because the stream outlives the call.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use proton_drive_rs::proton_sdk::account::Quota;
use proton_drive_rs::proton_sdk::error::Result;
use proton_drive_rs::proton_sdk::ids::{DriveEventId, NodeUid, ShareId};
use proton_drive_rs::{
    AvailableNameTarget, Device, DeviceType, DriveEvent, DriveEventScopeId, Node, NodeMoveItem,
    Thumbnail, ThumbnailType, TrashedNode,
};

use crate::{DriveApi, NodeOutcomes, OutcomeStream, RevisionRead, RevisionUpload};

/// Every kind of call [`DriveApi`] makes, one per method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveCall {
    GetMyFilesFolder,
    ContextShareId,
    GetNode,
    EnumerateNodes,
    EnumerateNodesLight,
    EnumerateChildren,
    EnumerateEvents,
    InvalidateCaches,
    CreateFolder,
    UploadFile,
    UploadFileFrom,
    UploadFileReplacingDraft,
    UploadNewRevision,
    OpenRevision,
    DownloadThumbnail,
    DownloadFile,
    DownloadRevision,
    RenameNode,
    MoveNode,
    CopyNode,
    GetAvailableName,
    MoveNodes,
    TrashNodes,
    RestoreNodes,
    RestoreNodesStreaming,
    DeleteNodes,
    EnumerateTrash,
    EmptyTrash,
    EnumerateTrashedNodes,
    Quota,
    EnumerateDevices,
    CreateDevice,
}

impl DriveCall {
    pub const ALL: [DriveCall; 32] = [
        DriveCall::GetMyFilesFolder,
        DriveCall::ContextShareId,
        DriveCall::GetNode,
        DriveCall::EnumerateNodes,
        DriveCall::EnumerateNodesLight,
        DriveCall::EnumerateChildren,
        DriveCall::EnumerateEvents,
        DriveCall::InvalidateCaches,
        DriveCall::CreateFolder,
        DriveCall::UploadFile,
        DriveCall::UploadFileFrom,
        DriveCall::UploadFileReplacingDraft,
        DriveCall::UploadNewRevision,
        DriveCall::OpenRevision,
        DriveCall::DownloadThumbnail,
        DriveCall::DownloadFile,
        DriveCall::DownloadRevision,
        DriveCall::RenameNode,
        DriveCall::MoveNode,
        DriveCall::CopyNode,
        DriveCall::GetAvailableName,
        DriveCall::MoveNodes,
        DriveCall::TrashNodes,
        DriveCall::RestoreNodes,
        DriveCall::RestoreNodesStreaming,
        DriveCall::DeleteNodes,
        DriveCall::EnumerateTrash,
        DriveCall::EmptyTrash,
        DriveCall::EnumerateTrashedNodes,
        DriveCall::Quota,
        DriveCall::EnumerateDevices,
        DriveCall::CreateDevice,
    ];

    /// The name the diagnostics report shows: the client method's own.
    pub fn name(self) -> &'static str {
        match self {
            DriveCall::GetMyFilesFolder => "get_my_files_folder",
            DriveCall::ContextShareId => "context_share_id",
            DriveCall::GetNode => "get_node",
            DriveCall::EnumerateNodes => "enumerate_nodes",
            DriveCall::EnumerateNodesLight => "enumerate_nodes_light",
            DriveCall::EnumerateChildren => "enumerate_folder_children_node_uids",
            DriveCall::EnumerateEvents => "enumerate_events",
            DriveCall::InvalidateCaches => "invalidate_caches_for_event",
            DriveCall::CreateFolder => "create_folder",
            DriveCall::UploadFile => "upload_file",
            DriveCall::UploadFileFrom => "upload_file_from",
            DriveCall::UploadFileReplacingDraft => "upload_file_replacing_draft_from",
            DriveCall::UploadNewRevision => "upload_new_revision_from",
            DriveCall::OpenRevision => "open_revision",
            DriveCall::DownloadThumbnail => "download_thumbnail",
            DriveCall::DownloadFile => "download_file_to",
            DriveCall::DownloadRevision => "download_revision_to",
            DriveCall::RenameNode => "rename_node",
            DriveCall::MoveNode => "move_node",
            DriveCall::CopyNode => "copy_node",
            DriveCall::GetAvailableName => "get_available_name",
            DriveCall::MoveNodes => "move_nodes_streaming",
            DriveCall::TrashNodes => "trash_nodes",
            DriveCall::RestoreNodes => "restore_nodes",
            DriveCall::RestoreNodesStreaming => "restore_nodes_streaming",
            DriveCall::DeleteNodes => "delete_nodes_streaming",
            DriveCall::EnumerateTrash => "enumerate_trash_node_uids",
            DriveCall::EmptyTrash => "empty_trash",
            DriveCall::EnumerateTrashedNodes => "enumerate_trashed_nodes_light",
            DriveCall::Quota => "quota",
            DriveCall::EnumerateDevices => "enumerate_devices",
            DriveCall::CreateDevice => "create_device",
        }
    }
}

/// One kind of call, as [`CallStats::snapshot`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallStat {
    pub call: DriveCall,
    pub count: u64,
    pub errors: u64,
    pub total: Duration,
    pub longest: Duration,
}

#[derive(Default)]
struct Counter {
    count: AtomicU64,
    errors: AtomicU64,
    total_us: AtomicU64,
    longest_us: AtomicU64,
}

/// The counts behind a [`Counted`] drive, shared with whoever reports them.
pub struct CallStats {
    counters: [Counter; DriveCall::ALL.len()],
}

impl Default for CallStats {
    fn default() -> Self {
        Self {
            counters: std::array::from_fn(|_| Counter::default()),
        }
    }
}

impl CallStats {
    fn record(&self, call: DriveCall, failed: bool, took: Option<Duration>) {
        let counter = &self.counters[call as usize];
        counter.count.fetch_add(1, Ordering::Relaxed);
        if failed {
            counter.errors.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(took) = took {
            let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
            counter.total_us.fetch_add(us, Ordering::Relaxed);
            counter.longest_us.fetch_max(us, Ordering::Relaxed);
        }
    }

    /// Every kind of call made at least once, in [`DriveCall::ALL`] order.
    pub fn snapshot(&self) -> Vec<CallStat> {
        DriveCall::ALL
            .iter()
            .filter_map(|&call| {
                let counter = &self.counters[call as usize];
                let count = counter.count.load(Ordering::Relaxed);
                (count > 0).then(|| CallStat {
                    call,
                    count,
                    errors: counter.errors.load(Ordering::Relaxed),
                    total: Duration::from_micros(counter.total_us.load(Ordering::Relaxed)),
                    longest: Duration::from_micros(counter.longest_us.load(Ordering::Relaxed)),
                })
            })
            .collect()
    }
}

/// A [`DriveApi`] that forwards to `inner` and records every call in `stats`.
pub struct Counted {
    inner: Arc<dyn DriveApi>,
    stats: Arc<CallStats>,
}

impl Counted {
    pub fn new(inner: Arc<dyn DriveApi>, stats: Arc<CallStats>) -> Self {
        Self { inner, stats }
    }

    async fn timed<T>(
        &self,
        call: DriveCall,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let started = Instant::now();
        let result = future.await;
        self.stats
            .record(call, result.is_err(), Some(started.elapsed()));
        result
    }
}

#[async_trait]
impl DriveApi for Counted {
    async fn get_my_files_folder(&self) -> Result<Node> {
        self.timed(
            DriveCall::GetMyFilesFolder,
            self.inner.get_my_files_folder(),
        )
        .await
    }

    async fn context_share_id(&self, uid: &NodeUid) -> Result<ShareId> {
        self.timed(DriveCall::ContextShareId, self.inner.context_share_id(uid))
            .await
    }

    async fn get_node(&self, uid: &NodeUid) -> Result<Option<Node>> {
        self.timed(DriveCall::GetNode, self.inner.get_node(uid))
            .await
    }

    async fn enumerate_nodes(&self, uids: &[NodeUid]) -> Result<Vec<Node>> {
        self.timed(DriveCall::EnumerateNodes, self.inner.enumerate_nodes(uids))
            .await
    }

    async fn enumerate_nodes_light(&self, uids: &[NodeUid]) -> Result<Vec<Node>> {
        self.timed(
            DriveCall::EnumerateNodesLight,
            self.inner.enumerate_nodes_light(uids),
        )
        .await
    }

    async fn enumerate_folder_children_node_uids(
        &self,
        folder_uid: &NodeUid,
    ) -> Result<Vec<NodeUid>> {
        self.timed(
            DriveCall::EnumerateChildren,
            self.inner.enumerate_folder_children_node_uids(folder_uid),
        )
        .await
    }

    async fn enumerate_events(
        &self,
        scope: &DriveEventScopeId,
        cursor: Option<&DriveEventId>,
    ) -> Result<Vec<DriveEvent>> {
        self.timed(
            DriveCall::EnumerateEvents,
            self.inner.enumerate_events(scope, cursor),
        )
        .await
    }

    async fn invalidate_caches_for_event(&self, event: &DriveEvent) -> Result<()> {
        self.timed(
            DriveCall::InvalidateCaches,
            self.inner.invalidate_caches_for_event(event),
        )
        .await
    }

    async fn create_folder(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        last_modification_time: Option<i64>,
    ) -> Result<NodeUid> {
        self.timed(
            DriveCall::CreateFolder,
            self.inner
                .create_folder(parent_uid, name, last_modification_time),
        )
        .await
    }

    async fn upload_file(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        contents: &[u8],
    ) -> Result<NodeUid> {
        self.timed(
            DriveCall::UploadFile,
            self.inner
                .upload_file(parent_uid, name, media_type, contents),
        )
        .await
    }

    async fn upload_file_from_dyn(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        reader: &mut (dyn Read + Send),
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
        aead: bool,
    ) -> Result<NodeUid> {
        self.timed(
            DriveCall::UploadFileFrom,
            self.inner.upload_file_from_dyn(
                parent_uid,
                name,
                media_type,
                reader,
                intended_size,
                thumbnails,
                last_modification_time,
                aead,
            ),
        )
        .await
    }

    async fn upload_file_replacing_draft_from_dyn(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        reader: &mut (dyn Read + Send),
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
        aead: bool,
    ) -> Result<NodeUid> {
        self.timed(
            DriveCall::UploadFileReplacingDraft,
            self.inner.upload_file_replacing_draft_from_dyn(
                parent_uid,
                name,
                media_type,
                reader,
                intended_size,
                thumbnails,
                last_modification_time,
                aead,
            ),
        )
        .await
    }

    async fn upload_new_revision_from_dyn(
        &self,
        file_uid: &NodeUid,
        reader: &mut (dyn Read + Send),
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
    ) -> Result<()> {
        self.timed(
            DriveCall::UploadNewRevision,
            self.inner.upload_new_revision_from_dyn(
                file_uid,
                reader,
                intended_size,
                thumbnails,
                last_modification_time,
            ),
        )
        .await
    }

    async fn upload_new_revision_over_dyn(
        &self,
        file_uid: &NodeUid,
        expected: &str,
        reader: &mut (dyn Read + Send),
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
    ) -> Result<RevisionUpload> {
        // Counted with the other revision uploads: it is one, with its check.
        self.timed(
            DriveCall::UploadNewRevision,
            self.inner.upload_new_revision_over_dyn(
                file_uid,
                expected,
                reader,
                intended_size,
                thumbnails,
                last_modification_time,
            ),
        )
        .await
    }

    async fn open_revision(&self, uid: &NodeUid) -> Result<Arc<dyn RevisionRead>> {
        self.timed(DriveCall::OpenRevision, self.inner.open_revision(uid))
            .await
    }

    async fn download_thumbnail(
        &self,
        uid: &NodeUid,
        thumbnail_type: ThumbnailType,
    ) -> Result<Option<Vec<u8>>> {
        self.timed(
            DriveCall::DownloadThumbnail,
            self.inner.download_thumbnail(uid, thumbnail_type),
        )
        .await
    }

    async fn download_file_to_dyn(
        &self,
        uid: &NodeUid,
        output: &mut (dyn Write + Send),
    ) -> Result<()> {
        self.timed(
            DriveCall::DownloadFile,
            self.inner.download_file_to_dyn(uid, output),
        )
        .await
    }

    async fn download_revision_to_dyn(
        &self,
        file_uid: &NodeUid,
        revision_id: &str,
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        self.timed(
            DriveCall::DownloadRevision,
            self.inner
                .download_revision_to_dyn(file_uid, revision_id, writer),
        )
        .await
    }

    async fn rename_node(
        &self,
        uid: &NodeUid,
        new_name: &str,
        new_media_type: Option<&str>,
    ) -> Result<()> {
        self.timed(
            DriveCall::RenameNode,
            self.inner.rename_node(uid, new_name, new_media_type),
        )
        .await
    }

    async fn move_node(&self, uid: &NodeUid, new_parent: &NodeUid) -> Result<()> {
        self.timed(DriveCall::MoveNode, self.inner.move_node(uid, new_parent))
            .await
    }

    async fn copy_node(
        &self,
        uid: &NodeUid,
        new_parent: &NodeUid,
        name: Option<&str>,
    ) -> Result<NodeUid> {
        self.timed(
            DriveCall::CopyNode,
            self.inner.copy_node(uid, new_parent, name),
        )
        .await
    }

    async fn get_available_name(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        target: AvailableNameTarget,
    ) -> Result<String> {
        self.timed(
            DriveCall::GetAvailableName,
            self.inner.get_available_name(parent_uid, name, target),
        )
        .await
    }

    fn move_nodes_streaming<'a>(
        &'a self,
        items: Vec<NodeMoveItem>,
        new_parent: NodeUid,
    ) -> OutcomeStream<'a> {
        self.stats.record(DriveCall::MoveNodes, false, None);
        self.inner.move_nodes_streaming(items, new_parent)
    }

    async fn trash_nodes(&self, uids: &[NodeUid]) -> Result<NodeOutcomes> {
        self.timed(DriveCall::TrashNodes, self.inner.trash_nodes(uids))
            .await
    }

    async fn restore_nodes(&self, uids: &[NodeUid]) -> Result<NodeOutcomes> {
        self.timed(DriveCall::RestoreNodes, self.inner.restore_nodes(uids))
            .await
    }

    fn restore_nodes_streaming<'a>(&'a self, uids: &[NodeUid]) -> OutcomeStream<'a> {
        self.stats
            .record(DriveCall::RestoreNodesStreaming, false, None);
        self.inner.restore_nodes_streaming(uids)
    }

    fn delete_nodes_streaming<'a>(&'a self, uids: &[NodeUid]) -> OutcomeStream<'a> {
        self.stats.record(DriveCall::DeleteNodes, false, None);
        self.inner.delete_nodes_streaming(uids)
    }

    async fn enumerate_trash_node_uids(&self) -> Result<Vec<NodeUid>> {
        self.timed(
            DriveCall::EnumerateTrash,
            self.inner.enumerate_trash_node_uids(),
        )
        .await
    }

    async fn empty_trash(&self) -> Result<()> {
        self.timed(DriveCall::EmptyTrash, self.inner.empty_trash())
            .await
    }

    async fn enumerate_trashed_nodes_light(&self, uids: &[NodeUid]) -> Result<Vec<TrashedNode>> {
        self.timed(
            DriveCall::EnumerateTrashedNodes,
            self.inner.enumerate_trashed_nodes_light(uids),
        )
        .await
    }

    async fn quota(&self) -> Result<Quota> {
        self.timed(DriveCall::Quota, self.inner.quota()).await
    }

    async fn enumerate_devices(&self) -> Result<Vec<Device>> {
        self.timed(DriveCall::EnumerateDevices, self.inner.enumerate_devices())
            .await
    }

    async fn create_device(&self, name: &str, device_type: DeviceType) -> Result<Device> {
        self.timed(
            DriveCall::CreateDevice,
            self.inner.create_device(name, device_type),
        )
        .await
    }

    fn landing(&self, uid: &NodeUid) {
        self.inner.landing(uid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_call_has_its_own_counter() {
        for (index, call) in DriveCall::ALL.iter().enumerate() {
            assert_eq!(*call as usize, index, "{} is out of order", call.name());
        }
        let names: std::collections::HashSet<_> =
            DriveCall::ALL.iter().map(|call| call.name()).collect();
        assert_eq!(names.len(), DriveCall::ALL.len());
    }

    #[test]
    fn a_call_is_counted_with_its_failures_and_time() {
        let stats = CallStats::default();
        stats.record(DriveCall::GetNode, false, Some(Duration::from_millis(30)));
        stats.record(DriveCall::GetNode, true, Some(Duration::from_millis(10)));
        stats.record(DriveCall::DeleteNodes, false, None);

        assert_eq!(
            stats.snapshot(),
            vec![
                CallStat {
                    call: DriveCall::GetNode,
                    count: 2,
                    errors: 1,
                    total: Duration::from_millis(40),
                    longest: Duration::from_millis(30),
                },
                CallStat {
                    call: DriveCall::DeleteNodes,
                    count: 1,
                    errors: 0,
                    total: Duration::ZERO,
                    longest: Duration::ZERO,
                },
            ]
        );
    }
}
