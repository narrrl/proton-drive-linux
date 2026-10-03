//! The calls the sync core makes to Drive, behind a trait.
//!
//! `Core` used to hold a concrete [`ProtonDriveClient`], so no test could make
//! Drive answer late, answer stale, or not answer at all, and the interleavings
//! behind most recent bugs could only be found on a real account over a slow
//! link (`docs/MILESTONE-3.0.0.md` §2, §8.1). [`DriveApi`] is the part of the
//! client the mount, the drain, the event feed and the mirror engine use; the
//! real client implements it by forwarding, and the simulation tests implement
//! it with an in-memory Drive that injects Drive's known faults.
//!
//! Sharing, Photos, devices, invitations and public links stay on the concrete
//! client (`Core::client`) until a test needs them faked.
//!
//! The method names match the client's own, so a call site reads the same
//! whichever it goes through. The client's generic methods (a reader or writer
//! of any type) cannot be part of an object-safe trait, so the trait takes them
//! as trait objects and `impl dyn DriveApi` puts the generic signatures back.

use std::io::{Read, Write};
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use proton_drive_rs::proton_sdk::account::Quota;
use proton_drive_rs::proton_sdk::error::Result;
use proton_drive_rs::proton_sdk::ids::{DriveEventId, NodeUid};
use proton_drive_rs::{
    DriveEvent, DriveEventScopeId, Node, NodeMoveItem, ProtonDriveClient, RevisionReader, Thumbnail,
};

/// One outcome per node of a batch request; see [`ProtonDriveClient::trash_nodes`].
pub(crate) type NodeOutcomes = Vec<(NodeUid, Result<()>)>;

/// The outcomes of a streaming batch request, one item per node as it lands.
pub(crate) type OutcomeStream<'a> = BoxStream<'a, Result<(NodeUid, Result<()>)>>;

/// Drive as the sync core sees it. See the module documentation.
#[async_trait]
pub(crate) trait DriveApi: Send + Sync {
    async fn get_my_files_folder(&self) -> Result<Node>;

    async fn get_node(&self, uid: &NodeUid) -> Result<Option<Node>>;

    async fn enumerate_nodes(&self, uids: &[NodeUid]) -> Result<Vec<Node>>;

    /// As [`DriveApi::enumerate_nodes`], but files carry no claimed size or
    /// modification time; see [`ProtonDriveClient::enumerate_nodes_light`].
    async fn enumerate_nodes_light(&self, uids: &[NodeUid]) -> Result<Vec<Node>>;

    async fn enumerate_folder_children_node_uids(
        &self,
        folder_uid: &NodeUid,
    ) -> Result<Vec<NodeUid>>;

    async fn enumerate_events(
        &self,
        scope: &DriveEventScopeId,
        cursor: Option<&DriveEventId>,
    ) -> Result<Vec<DriveEvent>>;

    async fn invalidate_caches_for_event(&self, event: &DriveEvent) -> Result<()>;

    async fn create_folder(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        last_modification_time: Option<i64>,
    ) -> Result<NodeUid>;

    async fn upload_file(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        contents: &[u8],
    ) -> Result<NodeUid>;

    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<NodeUid>;

    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<NodeUid>;

    async fn upload_new_revision_from_dyn(
        &self,
        file_uid: &NodeUid,
        reader: &mut (dyn Read + Send),
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
    ) -> Result<()>;

    async fn open_revision(&self, uid: &NodeUid) -> Result<Arc<dyn RevisionRead>>;

    async fn download_file_to_dyn(
        &self,
        uid: &NodeUid,
        output: &mut (dyn Write + Send),
    ) -> Result<()>;

    async fn download_revision_to_dyn(
        &self,
        file_uid: &NodeUid,
        revision_id: &str,
        writer: &mut (dyn Write + Send),
    ) -> Result<()>;

    async fn rename_node(
        &self,
        uid: &NodeUid,
        new_name: &str,
        new_media_type: Option<&str>,
    ) -> Result<()>;

    async fn move_node(&self, uid: &NodeUid, new_parent: &NodeUid) -> Result<()>;

    fn move_nodes_streaming<'a>(
        &'a self,
        items: Vec<NodeMoveItem>,
        new_parent: NodeUid,
    ) -> OutcomeStream<'a>;

    async fn trash_nodes(&self, uids: &[NodeUid]) -> Result<NodeOutcomes>;

    async fn restore_nodes(&self, uids: &[NodeUid]) -> Result<NodeOutcomes>;

    fn restore_nodes_streaming<'a>(&'a self, uids: &[NodeUid]) -> OutcomeStream<'a>;

    fn delete_nodes_streaming<'a>(&'a self, uids: &[NodeUid]) -> OutcomeStream<'a>;

    async fn enumerate_trash_node_uids(&self) -> Result<Vec<NodeUid>>;

    /// The account's storage, across every Proton product.
    async fn quota(&self) -> Result<Quota>;
}

/// The generic signatures of the client, over the object-safe ones above.
impl dyn DriveApi {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn upload_file_from<R: Read + Send>(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        mut reader: R,
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
        aead: bool,
    ) -> Result<NodeUid> {
        self.upload_file_from_dyn(
            parent_uid,
            name,
            media_type,
            &mut reader,
            intended_size,
            thumbnails,
            last_modification_time,
            aead,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn upload_file_replacing_draft_from<R: Read + Send>(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        mut reader: R,
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
        aead: bool,
    ) -> Result<NodeUid> {
        self.upload_file_replacing_draft_from_dyn(
            parent_uid,
            name,
            media_type,
            &mut reader,
            intended_size,
            thumbnails,
            last_modification_time,
            aead,
        )
        .await
    }

    pub(crate) async fn upload_new_revision_from<R: Read + Send>(
        &self,
        file_uid: &NodeUid,
        mut reader: R,
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
    ) -> Result<()> {
        self.upload_new_revision_from_dyn(
            file_uid,
            &mut reader,
            intended_size,
            thumbnails,
            last_modification_time,
        )
        .await
    }

    pub(crate) async fn download_file_to<W: Write + Send>(
        &self,
        uid: &NodeUid,
        output: &mut W,
    ) -> Result<()> {
        self.download_file_to_dyn(uid, output).await
    }

    pub(crate) async fn download_revision_to<W: Write + Send>(
        &self,
        file_uid: &NodeUid,
        revision_id: &str,
        writer: &mut W,
    ) -> Result<()> {
        self.download_revision_to_dyn(file_uid, revision_id, writer)
            .await
    }
}

/// An open revision that block reads are served from; see [`RevisionReader`].
#[async_trait]
pub(crate) trait RevisionRead: Send + Sync {
    /// Plaintext size of the revision.
    fn size(&self) -> u64;

    /// Plaintext size of each block, in block order.
    fn block_sizes(&self) -> &[u64];

    async fn read_at(&self, offset: u64, length: u64) -> Result<Vec<u8>>;
}

#[async_trait]
impl RevisionRead for RevisionReader {
    fn size(&self) -> u64 {
        RevisionReader::size(self)
    }

    fn block_sizes(&self) -> &[u64] {
        RevisionReader::block_sizes(self)
    }

    async fn read_at(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        RevisionReader::read_at(self, offset, length).await
    }
}

#[async_trait]
impl DriveApi for ProtonDriveClient {
    async fn get_my_files_folder(&self) -> Result<Node> {
        ProtonDriveClient::get_my_files_folder(self).await
    }

    async fn get_node(&self, uid: &NodeUid) -> Result<Option<Node>> {
        ProtonDriveClient::get_node(self, uid).await
    }

    async fn enumerate_nodes(&self, uids: &[NodeUid]) -> Result<Vec<Node>> {
        ProtonDriveClient::enumerate_nodes(self, uids).await
    }

    async fn enumerate_nodes_light(&self, uids: &[NodeUid]) -> Result<Vec<Node>> {
        ProtonDriveClient::enumerate_nodes_light(self, uids).await
    }

    async fn enumerate_folder_children_node_uids(
        &self,
        folder_uid: &NodeUid,
    ) -> Result<Vec<NodeUid>> {
        ProtonDriveClient::enumerate_folder_children_node_uids(self, folder_uid).await
    }

    async fn enumerate_events(
        &self,
        scope: &DriveEventScopeId,
        cursor: Option<&DriveEventId>,
    ) -> Result<Vec<DriveEvent>> {
        ProtonDriveClient::enumerate_events(self, scope, cursor).await
    }

    async fn invalidate_caches_for_event(&self, event: &DriveEvent) -> Result<()> {
        ProtonDriveClient::invalidate_caches_for_event(self, event).await
    }

    async fn create_folder(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        last_modification_time: Option<i64>,
    ) -> Result<NodeUid> {
        ProtonDriveClient::create_folder(self, parent_uid, name, last_modification_time).await
    }

    async fn upload_file(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        contents: &[u8],
    ) -> Result<NodeUid> {
        ProtonDriveClient::upload_file(self, parent_uid, name, media_type, contents).await
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
        ProtonDriveClient::upload_file_from(
            self,
            parent_uid,
            name,
            media_type,
            reader,
            intended_size,
            thumbnails,
            last_modification_time,
            aead,
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
        ProtonDriveClient::upload_file_replacing_draft_from(
            self,
            parent_uid,
            name,
            media_type,
            reader,
            intended_size,
            thumbnails,
            last_modification_time,
            aead,
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
        ProtonDriveClient::upload_new_revision_from(
            self,
            file_uid,
            reader,
            intended_size,
            thumbnails,
            last_modification_time,
        )
        .await
    }

    async fn open_revision(&self, uid: &NodeUid) -> Result<Arc<dyn RevisionRead>> {
        let reader = ProtonDriveClient::open_revision(self, uid).await?;
        Ok(Arc::new(reader))
    }

    async fn download_file_to_dyn(
        &self,
        uid: &NodeUid,
        output: &mut (dyn Write + Send),
    ) -> Result<()> {
        ProtonDriveClient::download_file_to(self, uid, &mut &mut *output).await
    }

    async fn download_revision_to_dyn(
        &self,
        file_uid: &NodeUid,
        revision_id: &str,
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        ProtonDriveClient::download_revision_to(self, file_uid, revision_id, &mut &mut *writer)
            .await
    }

    async fn rename_node(
        &self,
        uid: &NodeUid,
        new_name: &str,
        new_media_type: Option<&str>,
    ) -> Result<()> {
        ProtonDriveClient::rename_node(self, uid, new_name, new_media_type).await
    }

    async fn move_node(&self, uid: &NodeUid, new_parent: &NodeUid) -> Result<()> {
        ProtonDriveClient::move_node(self, uid, new_parent).await
    }

    fn move_nodes_streaming<'a>(
        &'a self,
        items: Vec<NodeMoveItem>,
        new_parent: NodeUid,
    ) -> OutcomeStream<'a> {
        ProtonDriveClient::move_nodes_streaming(self, items, new_parent).boxed()
    }

    async fn trash_nodes(&self, uids: &[NodeUid]) -> Result<NodeOutcomes> {
        ProtonDriveClient::trash_nodes(self, uids).await
    }

    async fn restore_nodes(&self, uids: &[NodeUid]) -> Result<NodeOutcomes> {
        ProtonDriveClient::restore_nodes(self, uids).await
    }

    fn restore_nodes_streaming<'a>(&'a self, uids: &[NodeUid]) -> OutcomeStream<'a> {
        ProtonDriveClient::restore_nodes_streaming(self, uids).boxed()
    }

    fn delete_nodes_streaming<'a>(&'a self, uids: &[NodeUid]) -> OutcomeStream<'a> {
        ProtonDriveClient::delete_nodes_streaming(self, uids).boxed()
    }

    async fn enumerate_trash_node_uids(&self) -> Result<Vec<NodeUid>> {
        ProtonDriveClient::enumerate_trash_node_uids(self).await
    }

    async fn quota(&self) -> Result<Quota> {
        ProtonDriveClient::quota(self).await
    }
}
