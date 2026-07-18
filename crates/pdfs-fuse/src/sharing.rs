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
use proton_drive_rs::{MemberRole, NodeKind};
use proton_drive_rs::proton_sdk::ids::NodeUid;

use super::{Core, ROOT_INO, parse_uid, public_link_info, role_from_str, role_to_str};

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
    ) -> Result<(usize, usize), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let role = role_from_str(role)?;
        let invitees: Vec<(String, MemberRole)> =
            emails.iter().map(|e| (e.clone(), role)).collect();
        self.rt
            .block_on(self.client.invite_users(&uid, &invitees, message))
            .map_err(|e| format!("share: {e}"))
    }

    /// List the members, pending invitations and public link of the node at `rel`.
    pub(crate) fn list_share(&self, rel: &Path) -> Result<(Vec<ShareEntry>, Option<PublicLinkInfo>), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;

        let mut entries = Vec::new();
        for m in self
            .rt
            .block_on(self.client.list_share_members(&uid))
            .map_err(|e| format!("list members: {e}"))?
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
            .map_err(|e| format!("list invitations: {e}"))?
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
            .map_err(|e| format!("list external invitations: {e}"))?
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
            .map_err(|e| format!("get public link: {e}"))?
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
    ) -> Result<(), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let role = role_from_str(role)?;
        match kind {
            ShareEntryKind::Member => {
                let member = self
                    .rt
                    .block_on(self.client.list_share_members(&uid))
                    .map_err(|e| format!("list members: {e}"))?
                    .into_iter()
                    .find(|m| m.membership_id.to_string() == id)
                    .ok_or_else(|| "member not found".to_string())?;
                self.rt
                    .block_on(self.client.update_member_role(&member, role))
                    .map_err(|e| format!("update role: {e}"))
            }
            ShareEntryKind::ProtonInvite => {
                let inv = self
                    .rt
                    .block_on(self.client.list_share_invitations(&uid))
                    .map_err(|e| format!("list invitations: {e}"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| "invitation not found".to_string())?;
                self.rt
                    .block_on(self.client.update_invitation_role(&inv, role))
                    .map_err(|e| format!("update role: {e}"))
            }
            ShareEntryKind::ExternalInvite => {
                Err("an external invitation's role cannot be changed".to_string())
            }
        }
    }

    /// Remove a member, pending Proton invite, or external invite from the node
    /// at `rel`.
    pub(crate) fn remove_share_entry(&self, rel: &Path, id: &str, kind: ShareEntryKind) -> Result<(), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        match kind {
            ShareEntryKind::Member => {
                let member = self
                    .rt
                    .block_on(self.client.list_share_members(&uid))
                    .map_err(|e| format!("list members: {e}"))?
                    .into_iter()
                    .find(|m| m.membership_id.to_string() == id)
                    .ok_or_else(|| "member not found".to_string())?;
                self.rt
                    .block_on(self.client.remove_member(&member))
                    .map_err(|e| format!("remove member: {e}"))
            }
            ShareEntryKind::ProtonInvite => {
                let inv = self
                    .rt
                    .block_on(self.client.list_share_invitations(&uid))
                    .map_err(|e| format!("list invitations: {e}"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| "invitation not found".to_string())?;
                self.rt
                    .block_on(self.client.delete_invitation(&inv))
                    .map_err(|e| format!("revoke invitation: {e}"))
            }
            ShareEntryKind::ExternalInvite => {
                let ext = self
                    .rt
                    .block_on(self.client.list_external_invitations(&uid))
                    .map_err(|e| format!("list external invitations: {e}"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| "external invitation not found".to_string())?;
                self.rt
                    .block_on(self.client.delete_external_invitation(&ext))
                    .map_err(|e| format!("revoke external invitation: {e}"))
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
    ) -> Result<PublicLinkInfo, String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let role = role_from_str(role)?;
        let link = self
            .rt
            .block_on(
                self.client
                    .create_public_link(&uid, role, password, expires),
            )
            .map_err(|e| format!("create public link: {e}"))?;
        Ok(public_link_info(link))
    }

    /// Remove the public link `id` from the node at `rel`.
    pub(crate) fn remove_public_link(&self, rel: &Path, id: &str) -> Result<(), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let link = self
            .rt
            .block_on(self.client.get_public_link(&uid))
            .map_err(|e| format!("get public link: {e}"))?
            .filter(|l| l.public_link_id == id)
            .ok_or_else(|| "public link not found".to_string())?;
        self.rt
            .block_on(self.client.remove_public_link(&link))
            .map_err(|e| format!("remove public link: {e}"))
    }

    // ---- shared with me ---------------------------------------------------

    /// List nodes shared with me that I have accepted.
    pub(crate) fn list_shared_with_me(&self) -> Result<Vec<DirEntry>, String> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_shared_with_me_node_uids())
            .map_err(|e| format!("enumerate shared: {e}"))?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| format!("enumerate nodes: {e}"))?;
        Ok(nodes
            .into_iter()
            .map(|n| {
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
                    modified: 0,
                    pinned: false,
                    cached: false,
                    uid: n.uid.to_string(),
                    path: String::new(),
                }
            })
            .collect())
    }

    /// Leave a shared node by its uid.
    pub(crate) fn leave_shared(&self, uid: &str) -> Result<(), String> {
        let uid = parse_uid(uid).ok_or_else(|| format!("invalid uid: {uid}"))?;
        self.rt
            .block_on(self.client.leave_shared_node(&uid))
            .map_err(|e| format!("leave shared: {e}"))
    }

    // ---- incoming invitations ---------------------------------------------

    /// List invitations addressed to me, pending accept or reject.
    pub(crate) fn list_invitations(&self) -> Result<Vec<InvitationInfo>, String> {
        let invitations = self
            .rt
            .block_on(self.client.list_incoming_invitations())
            .map_err(|e| format!("list invitations: {e}"))?;
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
    pub(crate) fn accept_invitation(&self, id: &str) -> Result<(), String> {
        self.rt
            .block_on(self.client.accept_invitation(id))
            .map_err(|e| format!("accept invitation: {e}"))
    }

    /// Reject an invitation addressed to me by its id.
    pub(crate) fn reject_invitation(&self, id: &str) -> Result<(), String> {
        self.rt
            .block_on(self.client.reject_invitation(id))
            .map_err(|e| format!("reject invitation: {e}"))
    }

    // ---- bookmarks --------------------------------------------------------

    /// List public links saved to my account.
    pub(crate) fn list_bookmarks(&self) -> Result<Vec<BookmarkInfo>, String> {
        let bookmarks = self
            .rt
            .block_on(self.client.list_bookmarks())
            .map_err(|e| format!("list bookmarks: {e}"))?;
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
    pub(crate) fn create_bookmark(&self, url: &str, password: Option<&str>) -> Result<(), String> {
        self.rt
            .block_on(self.client.create_bookmark(url, password))
            .map_err(|e| format!("create bookmark: {e}"))
    }

    /// Remove a saved bookmark by its token.
    pub(crate) fn delete_bookmark(&self, token: &str) -> Result<(), String> {
        self.rt
            .block_on(self.client.delete_bookmark(token))
            .map_err(|e| format!("delete bookmark: {e}"))
    }

    // ---- shared by me -----------------------------------------------------

    /// List the nodes I have shared with others, each with a summary of its share
    /// state (members, pending invitations, public link). One list call enumerates
    /// the shared uids; the per-node detail is then gathered best-effort — a single
    /// node racing with an unshare drops from the list rather than failing the whole
    /// request.
    pub(crate) fn list_shared_by_me(&self) -> Result<Vec<SharedItem>, String> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_shared_by_me_node_uids())
            .map_err(|e| format!("enumerate shared-by-me: {e}"))?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| format!("enumerate nodes: {e}"))?;
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
