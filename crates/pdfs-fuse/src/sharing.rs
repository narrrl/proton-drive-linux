//! Sharing: per-node share membership, public links, the invitations this
//! account has received, saved public-link bookmarks, and the reverse view of
//! everything this account shares out.
//!
//! All of it is remote-only state — nothing here touches the inode maps or the
//! content cache, so every method is a round-trip wrapped in the crate error
//! shape the control socket expects.

use std::path::Path;

use pdfs_core::control::{
    BookmarkInfo, DirEntry, InvitationInfo, PublicLinkInfo, ShareEntry, ShareEntryKind, SharedItem,
};
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{MemberRole, NodeKind};

use proton_drive_rs::Node;

use super::{Core, ROOT_INO, node_size, parse_uid, public_link_info, role_from_str, role_to_str};

/// A node from a *shared* listing as a [`DirEntry`]. Shared nodes live outside
/// the mount, so the entry is uid-addressed: `path` is empty, and the local-state
/// flags (`pinned`, `cached`) are false — they describe my own tree, not this one.
fn shared_entry(n: Node) -> DirEntry {
    let is_dir = n.is_folder();
    let size = match &n.kind {
        NodeKind::File {
            claimed_size,
            total_size_on_storage,
            ..
        } => claimed_size.unwrap_or(*total_size_on_storage).max(0) as u64,
        NodeKind::Folder => 0,
    };
    DirEntry {
        name: n.name,
        is_dir,
        size,
        modified: n.modification_time,
        pinned: false,
        cached: false,
        uid: n.uid.to_string(),
        path: String::new(),
    }
}

impl Core {
    // ---- sharing a node ---------------------------------------------------

    /// Invite Proton and/or external emails to the node at `rel` at `role`.
    /// Returns `(proton_invited, external_invited)`.
    pub(crate) fn share_node(
        &self,
        rel: &Path,
        emails: &[String],
        role: &str,
        message: Option<&str>,
    ) -> CoreResult<(usize, usize)> {
        let (_ino, uid) = self.resolve(rel)?;
        let role = role_from_str(role)?;
        let invitees: Vec<(String, MemberRole)> =
            emails.iter().map(|e| (e.clone(), role)).collect();
        self.rt
            .block_on(self.client.invite_users(&uid, &invitees, message))
            .map_err(|e| CoreError::from_api(&e, "share"))
    }

    /// List the members, pending invitations and public link of the node at `rel`.
    pub(crate) fn list_share(
        &self,
        rel: &Path,
    ) -> CoreResult<(Vec<ShareEntry>, Option<PublicLinkInfo>)> {
        let (_ino, uid) = self.resolve(rel)?;

        let mut entries = Vec::new();
        for m in self
            .rt
            .block_on(self.client.list_share_members(&uid))
            .map_err(|e| CoreError::from_api(&e, "list members"))?
        {
            entries.push(ShareEntry {
                id: m.membership_id.to_string(),
                email: m.email,
                role: role_to_str(m.role).to_string(),
                kind: ShareEntryKind::Member,
            });
        }
        for inv in self
            .rt
            .block_on(self.client.list_share_invitations(&uid))
            .map_err(|e| CoreError::from_api(&e, "list invitations"))?
        {
            entries.push(ShareEntry {
                id: inv.invitation_id,
                email: inv.invitee_email,
                role: role_to_str(inv.role).to_string(),
                kind: ShareEntryKind::ProtonInvite,
            });
        }
        for ext in self
            .rt
            .block_on(self.client.list_external_invitations(&uid))
            .map_err(|e| CoreError::from_api(&e, "list external invitations"))?
        {
            entries.push(ShareEntry {
                id: ext.invitation_id,
                email: ext.invitee_email,
                role: role_to_str(ext.role).to_string(),
                kind: ShareEntryKind::ExternalInvite,
            });
        }

        let link = self
            .rt
            .block_on(self.client.get_public_link(&uid))
            .map_err(|e| CoreError::from_api(&e, "get public link"))?
            .map(public_link_info);

        Ok((entries, link))
    }

    /// Change the role of a member or pending Proton invitation on the node at
    /// `rel`. External invitations have no role-update endpoint.
    pub(crate) fn update_share_role(
        &self,
        rel: &Path,
        id: &str,
        kind: ShareEntryKind,
        role: &str,
    ) -> CoreResult<()> {
        let (_ino, uid) = self.resolve(rel)?;
        let role = role_from_str(role)?;
        match kind {
            ShareEntryKind::Member => {
                let member = self
                    .rt
                    .block_on(self.client.list_share_members(&uid))
                    .map_err(|e| CoreError::from_api(&e, "list members"))?
                    .into_iter()
                    .find(|m| m.membership_id.to_string() == id)
                    .ok_or_else(|| CoreError::not_found("member not found"))?;
                self.rt
                    .block_on(self.client.update_member_role(&member, role))
                    .map_err(|e| CoreError::from_api(&e, "update role"))
            }
            ShareEntryKind::ProtonInvite => {
                let inv = self
                    .rt
                    .block_on(self.client.list_share_invitations(&uid))
                    .map_err(|e| CoreError::from_api(&e, "list invitations"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| CoreError::not_found("invitation not found"))?;
                self.rt
                    .block_on(self.client.update_invitation_role(&inv, role))
                    .map_err(|e| CoreError::from_api(&e, "update role"))
            }
            ShareEntryKind::ExternalInvite => Err(CoreError::invalid(
                "an external invitation's role cannot be changed",
            )),
        }
    }

    /// Remove a member, pending Proton invite, or external invite from the node
    /// at `rel`.
    pub(crate) fn remove_share_entry(
        &self,
        rel: &Path,
        id: &str,
        kind: ShareEntryKind,
    ) -> CoreResult<()> {
        let (_ino, uid) = self.resolve(rel)?;
        match kind {
            ShareEntryKind::Member => {
                let member = self
                    .rt
                    .block_on(self.client.list_share_members(&uid))
                    .map_err(|e| CoreError::from_api(&e, "list members"))?
                    .into_iter()
                    .find(|m| m.membership_id.to_string() == id)
                    .ok_or_else(|| CoreError::not_found("member not found"))?;
                self.rt
                    .block_on(self.client.remove_member(&member))
                    .map_err(|e| CoreError::from_api(&e, "remove member"))
            }
            ShareEntryKind::ProtonInvite => {
                let inv = self
                    .rt
                    .block_on(self.client.list_share_invitations(&uid))
                    .map_err(|e| CoreError::from_api(&e, "list invitations"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| CoreError::not_found("invitation not found"))?;
                self.rt
                    .block_on(self.client.delete_invitation(&inv))
                    .map_err(|e| CoreError::from_api(&e, "revoke invitation"))
            }
            ShareEntryKind::ExternalInvite => {
                let ext = self
                    .rt
                    .block_on(self.client.list_external_invitations(&uid))
                    .map_err(|e| CoreError::from_api(&e, "list external invitations"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| CoreError::not_found("external invitation not found"))?;
                self.rt
                    .block_on(self.client.delete_external_invitation(&ext))
                    .map_err(|e| CoreError::from_api(&e, "revoke external invitation"))
            }
        }
    }

    /// Create a public link on the node at `rel`, returning its info (with URL).
    pub(crate) fn create_public_link(
        &self,
        rel: &Path,
        role: &str,
        password: Option<&str>,
        expires: Option<i64>,
    ) -> CoreResult<PublicLinkInfo> {
        let (_ino, uid) = self.resolve(rel)?;
        let role = role_from_str(role)?;
        let link = self
            .rt
            .block_on(
                self.client
                    .create_public_link(&uid, role, password, expires),
            )
            .map_err(|e| CoreError::from_api(&e, "create public link"))?;
        Ok(public_link_info(link))
    }

    /// Remove the public link `id` from the node at `rel`.
    pub(crate) fn remove_public_link(&self, rel: &Path, id: &str) -> CoreResult<()> {
        let (_ino, uid) = self.resolve(rel)?;
        let link = self
            .rt
            .block_on(self.client.get_public_link(&uid))
            .map_err(|e| CoreError::from_api(&e, "get public link"))?
            .filter(|l| l.public_link_id == id)
            .ok_or_else(|| CoreError::not_found("public link not found"))?;
        self.rt
            .block_on(self.client.remove_public_link(&link))
            .map_err(|e| CoreError::from_api(&e, "remove public link"))
    }

    // ---- shared with me ---------------------------------------------------

    /// List nodes shared with me that I have accepted.
    pub(crate) fn list_shared_with_me(&self) -> CoreResult<Vec<DirEntry>> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_shared_with_me_node_uids())
            .map_err(|e| CoreError::from_api(&e, "enumerate shared"))?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?;
        Ok(nodes.into_iter().map(shared_entry).collect())
    }

    /// List the children of a folder shared with me, addressed by uid.
    ///
    /// A shared subtree has no path in this account's mount — the FUSE tree only
    /// spans my own volume — so browsing into one is uid-addressed the whole way
    /// down, each listing handing the front-end the uids it descends with next.
    pub(crate) fn list_shared_folder(&self, uid: &str) -> CoreResult<Vec<DirEntry>> {
        let uid =
            parse_uid(uid).ok_or_else(|| CoreError::invalid(format!("invalid uid: {uid}")))?;
        let child_uids = self
            .rt
            .block_on(self.client.enumerate_folder_children_node_uids(&uid))
            .map_err(|e| CoreError::from_api(&e, "enumerate shared children"))?;
        if child_uids.is_empty() {
            return Ok(Vec::new());
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&child_uids))
            .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?;
        Ok(nodes.into_iter().map(shared_entry).collect())
    }

    /// Download a file shared with me into the content cache, returning its
    /// on-disk path (served from cache when a fresh blob already exists).
    ///
    /// The by-uid twin of [`Core::open_file`]: same cache and same tracked
    /// download, but the node is fetched from the server rather than resolved
    /// through the inode tree, which does not cover other people's volumes.
    ///
    /// [`Core::open_file`]: super::Core::open_file
    pub(crate) fn open_shared_file(&self, uid: &str) -> CoreResult<std::path::PathBuf> {
        let uid =
            parse_uid(uid).ok_or_else(|| CoreError::invalid(format!("invalid uid: {uid}")))?;
        let node = self
            .rt
            .block_on(self.client.get_node(&uid))
            .map_err(|e| CoreError::from_api(&e, "get shared node"))?
            .ok_or_else(|| CoreError::not_found("shared node not found"))?;
        if !node.is_file() {
            return Err(CoreError::invalid("not a regular file"));
        }
        let size = node_size(&node);
        let mtime = node.modification_time;
        if let Some(p) = self.cache.cached_content_path(&uid, mtime, size) {
            return Ok(p);
        }
        let bytes = self
            .download_file_tracked(&uid, &node.name, size)
            .map_err(|e| CoreError::from_api(&e, "download"))?;
        self.cache
            .store(&uid, mtime, size, &bytes)
            .map_err(|e| CoreError::internal(format!("cache store: {e}")))?;
        Ok(self.cache.content_path(&uid))
    }

    /// Leave a shared node by its uid.
    pub(crate) fn leave_shared(&self, uid: &str) -> CoreResult<()> {
        let uid =
            parse_uid(uid).ok_or_else(|| CoreError::invalid(format!("invalid uid: {uid}")))?;
        self.rt
            .block_on(self.client.leave_shared_node(&uid))
            .map_err(|e| CoreError::from_api(&e, "leave shared"))
    }

    // ---- incoming invitations ---------------------------------------------

    /// List invitations addressed to me, pending accept or reject.
    pub(crate) fn list_invitations(&self) -> CoreResult<Vec<InvitationInfo>> {
        let invitations = self
            .rt
            .block_on(self.client.list_incoming_invitations())
            .map_err(|e| CoreError::from_api(&e, "list invitations"))?;
        Ok(invitations
            .into_iter()
            .map(|i| InvitationInfo {
                id: i.invitation_id,
                inviter_email: i.inviter_email,
                name: i.node_name,
                is_dir: i.is_folder,
            })
            .collect())
    }

    /// Accept an invitation addressed to me by its id.
    pub(crate) fn accept_invitation(&self, id: &str) -> CoreResult<()> {
        self.rt
            .block_on(self.client.accept_invitation(id))
            .map_err(|e| CoreError::from_api(&e, "accept invitation"))
    }

    /// Reject an invitation addressed to me by its id.
    pub(crate) fn reject_invitation(&self, id: &str) -> CoreResult<()> {
        self.rt
            .block_on(self.client.reject_invitation(id))
            .map_err(|e| CoreError::from_api(&e, "reject invitation"))
    }

    // ---- bookmarks --------------------------------------------------------

    /// List public links saved to my account.
    pub(crate) fn list_bookmarks(&self) -> CoreResult<Vec<BookmarkInfo>> {
        let bookmarks = self
            .rt
            .block_on(self.client.list_bookmarks())
            .map_err(|e| CoreError::from_api(&e, "list bookmarks"))?;
        Ok(bookmarks
            .into_iter()
            .map(|b| BookmarkInfo {
                token: b.token,
                url: b.url,
                name: b.node_name,
                is_dir: b.is_folder,
            })
            .collect())
    }

    /// Save a public link (optionally password-protected) as a bookmark.
    pub(crate) fn create_bookmark(&self, url: &str, password: Option<&str>) -> CoreResult<()> {
        self.rt
            .block_on(self.client.create_bookmark(url, password))
            .map_err(|e| CoreError::from_api(&e, "create bookmark"))
    }

    /// Remove a saved bookmark by its token.
    pub(crate) fn delete_bookmark(&self, token: &str) -> CoreResult<()> {
        self.rt
            .block_on(self.client.delete_bookmark(token))
            .map_err(|e| CoreError::from_api(&e, "delete bookmark"))
    }

    // ---- account ----------------------------------------------------------

    /// Total account storage usage `(max_space, used_space)` in bytes, across all
    /// Proton products (not Drive-only). A remote round-trip; nothing here is
    /// cached.
    pub(crate) fn account_quota(&self) -> CoreResult<(i64, i64)> {
        let q = self
            .rt
            .block_on(self.client.quota())
            .map_err(|e| CoreError::from_api(&e, "account quota"))?;
        Ok((q.max_space, q.used_space))
    }

    // ---- shared by me -----------------------------------------------------

    /// List the nodes I have shared with others, each with a summary of its share
    /// state (members, pending invitations, public link). One list call enumerates
    /// the shared uids; the per-node detail is then gathered best-effort — a single
    /// node racing with an unshare drops from the list rather than failing the whole
    /// request.
    pub(crate) fn list_shared_by_me(&self) -> CoreResult<Vec<SharedItem>> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_shared_by_me_node_uids())
            .map_err(|e| CoreError::from_api(&e, "enumerate shared-by-me"))?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?;
        let mut items = Vec::with_capacity(nodes.len());
        for n in nodes {
            let uid = n.uid.clone();
            let members = self
                .rt
                .block_on(self.client.list_share_members(&uid))
                .map(|m| m.len())
                .unwrap_or(0);
            let proton_invites = self
                .rt
                .block_on(self.client.list_share_invitations(&uid))
                .map(|i| i.len())
                .unwrap_or(0);
            let external_invites = self
                .rt
                .block_on(self.client.list_external_invitations(&uid))
                .map(|i| i.len())
                .unwrap_or(0);
            let link = self
                .rt
                .block_on(self.client.get_public_link(&uid))
                .ok()
                .flatten()
                .map(public_link_info);
            items.push(SharedItem {
                uid: uid.to_string(),
                is_dir: n.is_folder(),
                name: n.name,
                path: self.rel_path_for_uid(&uid).unwrap_or_default(),
                member_count: members,
                invite_count: proton_invites + external_invites,
                link,
            });
        }
        Ok(items)
    }

    /// Best-effort mountpoint-relative path for a node already interned in the live
    /// tree, by walking parent inodes to the root. `None` when the node has never
    /// been seen through the mount (e.g. shared but not browsed to this session) —
    /// the caller then leaves the path empty.
    pub(crate) fn rel_path_for_uid(&self, uid: &NodeUid) -> Option<String> {
        let st = self.state.lock();
        let mut ino = *st.by_uid.get(uid)?;
        let mut parts = Vec::new();
        while ino != ROOT_INO {
            let entry = st.entries.get(&ino)?;
            parts.push(entry.node.name.clone());
            ino = entry.parent;
        }
        parts.reverse();
        Some(parts.join("/"))
    }
}
