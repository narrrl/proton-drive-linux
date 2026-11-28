//! The control requests about sharing: who a node is shared with, its public
//! link, what others share with this account, invitations and saved links.
//!
//! Most of them come in two forms, one naming the node by its path under the
//! mount and one by its uid. Both are resolved to a uid here, once, and the
//! [`Sharing`](crate::sharing::Sharing) service is only ever asked about a
//! resolved node.

use std::path::Path;

use pdfs_core::CoreResult;
use pdfs_core::control::ErrorKind;
use pdfs_core::control::{ActivityKind, Request as CtlRequest, Response as CtlResponse};
use proton_drive_rs::proton_sdk::ids::NodeUid;

use super::rel_to_mount;
use crate::sharing::StopSharingOutcome;
use crate::{Core, parse_uid};

/// The requests [`answer`] serves, as a pattern, so the control socket's
/// match hands exactly these over and the compiler still checks that it covers
/// every request.
macro_rules! request {
    () => {
        ::pdfs_core::control::Request::ShareNode { .. }
            | ::pdfs_core::control::Request::ShareNodeByUid { .. }
            | ::pdfs_core::control::Request::ListShare { .. }
            | ::pdfs_core::control::Request::ListShareByUid { .. }
            | ::pdfs_core::control::Request::UpdateShareRole { .. }
            | ::pdfs_core::control::Request::UpdateShareRoleByUid { .. }
            | ::pdfs_core::control::Request::RemoveShareEntry { .. }
            | ::pdfs_core::control::Request::RemoveShareEntryByUid { .. }
            | ::pdfs_core::control::Request::CreatePublicLink { .. }
            | ::pdfs_core::control::Request::CreatePublicLinkByUid { .. }
            | ::pdfs_core::control::Request::UpdatePublicLink { .. }
            | ::pdfs_core::control::Request::UpdatePublicLinkByUid { .. }
            | ::pdfs_core::control::Request::RemovePublicLink { .. }
            | ::pdfs_core::control::Request::RemovePublicLinkByUid { .. }
            | ::pdfs_core::control::Request::StopSharing { .. }
            | ::pdfs_core::control::Request::StopSharingByUid { .. }
            | ::pdfs_core::control::Request::ListSharedWithMe
            | ::pdfs_core::control::Request::ListSharedFolder { .. }
            | ::pdfs_core::control::Request::OpenSharedFile { .. }
            | ::pdfs_core::control::Request::LeaveShared { .. }
            | ::pdfs_core::control::Request::ListInvitations
            | ::pdfs_core::control::Request::AcceptInvitation { .. }
            | ::pdfs_core::control::Request::RejectInvitation { .. }
            | ::pdfs_core::control::Request::ListBookmarks
            | ::pdfs_core::control::Request::CreateBookmark { .. }
            | ::pdfs_core::control::Request::DeleteBookmark { .. }
            | ::pdfs_core::control::Request::ListSharedByMe
    };
}
pub(super) use request;

/// The node a request is about, and how the activity feed names it: the path
/// the user gave, or the uid.
struct Target {
    label: String,
    /// The node, or why it could not be resolved. A failure is reported
    /// through the request's own reply, so a share that fails here is logged
    /// as a failed share, as it was before the split.
    node: CoreResult<NodeUid>,
}

impl Target {
    /// A node named by its path. A path outside the mount fails the request
    /// before it starts, without an activity entry.
    fn path(core: &Core, mountpoint: &Path, path: String) -> CoreResult<Self> {
        let rel = rel_to_mount(mountpoint, &path)?;
        Ok(Self {
            node: core.resolve(&rel).map(|(_ino, uid)| uid),
            label: path,
        })
    }

    /// A node named by its uid, which one of this daemon's locations, the
    /// Photos timeline, or Drive's list of what this account shared must show
    /// to be this account's. The last covers a node shared from elsewhere and
    /// never browsed to here: Shared by me lists it, and without it the share
    /// could not be managed, nor stopped, from this computer.
    fn uid(core: &Core, uid: String) -> Self {
        let node = core.resolve_shareable(&uid).or_else(|e| {
            if e.kind != ErrorKind::NotFound {
                return Err(e);
            }
            match parse_uid(&uid) {
                Some(node) if core.sharing.shared_by_me(&node)? => Ok(node),
                _ => Err(e),
            }
        });
        Self { node, label: uid }
    }
}

/// Answer a request that [`request!`] matches.
pub(super) fn answer(ask: &super::Ask<'_>, request: CtlRequest) -> CtlResponse {
    let (core, mountpoint) = (ask.core, ask.mountpoint);
    match answer_or_refuse(core, mountpoint, request) {
        Ok(response) => response,
        Err(e) => CtlResponse::error(e),
    }
}

fn answer_or_refuse(
    core: &Core,
    mountpoint: &Path,
    request: CtlRequest,
) -> CoreResult<CtlResponse> {
    let sharing = &core.sharing;
    Ok(match request {
        CtlRequest::ShareNode {
            path,
            emails,
            role,
            message,
        } => share_node(
            core,
            Target::path(core, mountpoint, path)?,
            &emails,
            &role,
            message,
        ),
        CtlRequest::ShareNodeByUid {
            uid,
            emails,
            role,
            message,
        } => share_node(core, Target::uid(core, uid), &emails, &role, message),
        CtlRequest::ListShare { path } => list_share(core, Target::path(core, mountpoint, path)?),
        CtlRequest::ListShareByUid { uid } => list_share(core, Target::uid(core, uid)),
        CtlRequest::UpdateShareRole {
            path,
            id,
            kind,
            role,
        } => {
            let target = Target::path(core, mountpoint, path)?;
            target
                .node
                .and_then(|uid| sharing.update_share_role(&uid, &id, kind, &role))?;
            role_updated(&role)
        }
        CtlRequest::UpdateShareRoleByUid {
            uid,
            id,
            kind,
            role,
        } => {
            let target = Target::uid(core, uid);
            target
                .node
                .and_then(|uid| sharing.update_share_role(&uid, &id, kind, &role))?;
            role_updated(&role)
        }
        CtlRequest::RemoveShareEntry { path, id, kind } => {
            let target = Target::path(core, mountpoint, path)?;
            remove_share_entry(core, target, &id, kind)?
        }
        CtlRequest::RemoveShareEntryByUid { uid, id, kind } => {
            remove_share_entry(core, Target::uid(core, uid), &id, kind)?
        }
        CtlRequest::CreatePublicLink {
            path,
            role,
            password,
            expires,
        } => {
            let target = Target::path(core, mountpoint, path)?;
            create_public_link(core, target, &role, password.as_deref(), expires)
        }
        CtlRequest::CreatePublicLinkByUid {
            uid,
            role,
            password,
            expires,
        } => create_public_link(
            core,
            Target::uid(core, uid),
            &role,
            password.as_deref(),
            expires,
        ),
        CtlRequest::UpdatePublicLink {
            path,
            id,
            role,
            password,
            expires,
        } => {
            let uid = Target::path(core, mountpoint, path)?.node?;
            let link = sharing.update_public_link(&uid, &id, role.as_deref(), password, expires)?;
            CtlResponse::PublicLink { link }
        }
        CtlRequest::UpdatePublicLinkByUid {
            uid,
            id,
            role,
            password,
            expires,
        } => {
            let uid = Target::uid(core, uid).node?;
            let link = sharing.update_public_link(&uid, &id, role.as_deref(), password, expires)?;
            CtlResponse::PublicLink { link }
        }
        CtlRequest::RemovePublicLink { path, id } => {
            remove_public_link(core, Target::path(core, mountpoint, path)?, &id)?
        }
        CtlRequest::RemovePublicLinkByUid { uid, id } => {
            remove_public_link(core, Target::uid(core, uid), &id)?
        }
        CtlRequest::StopSharing { path } => {
            stop_sharing(core, Target::path(core, mountpoint, path)?)
        }
        CtlRequest::StopSharingByUid { uid } => stop_sharing(core, Target::uid(core, uid)),
        CtlRequest::ListSharedWithMe => CtlResponse::Entries {
            entries: sharing.list_shared_with_me()?,
        },
        CtlRequest::ListSharedFolder { uid } => CtlResponse::Entries {
            entries: sharing.list_shared_folder(&uid)?,
        },
        CtlRequest::OpenSharedFile { uid } => CtlResponse::FilePath {
            path: sharing.open_shared_file(&uid)?.display().to_string(),
        },
        CtlRequest::LeaveShared { uid } => {
            sharing.leave_shared(&uid)?;
            core.log_activity(ActivityKind::Unshare, "shared item", "left", true);
            ok("left shared item")
        }
        CtlRequest::ListInvitations => CtlResponse::Invitations {
            items: sharing.list_invitations()?,
        },
        CtlRequest::AcceptInvitation { id } => {
            sharing.accept_invitation(&id)?;
            ok("invitation accepted")
        }
        CtlRequest::RejectInvitation { id } => {
            sharing.reject_invitation(&id)?;
            ok("invitation rejected")
        }
        CtlRequest::ListBookmarks => CtlResponse::Bookmarks {
            items: sharing.list_bookmarks()?,
        },
        CtlRequest::CreateBookmark { url, password } => {
            sharing.create_bookmark(&url, password.as_deref())?;
            ok("bookmark saved")
        }
        CtlRequest::DeleteBookmark { token } => {
            sharing.delete_bookmark(&token)?;
            ok("bookmark removed")
        }
        CtlRequest::ListSharedByMe => CtlResponse::SharedByMe {
            items: sharing.list_shared_by_me()?,
        },
        // `control` only hands over what `request!` matches.
        _ => CtlResponse::error(pdfs_core::CoreError::internal(
            "not a sharing request".to_string(),
        )),
    })
}

fn ok(message: &str) -> CtlResponse {
    CtlResponse::Ok {
        message: message.to_string(),
    }
}

fn role_updated(role: &str) -> CtlResponse {
    CtlResponse::Ok {
        message: format!("role updated to {role}"),
    }
}

fn share_node(
    core: &Core,
    target: Target,
    emails: &[String],
    role: &str,
    message: Option<String>,
) -> CtlResponse {
    let shared = target.node.and_then(|uid| {
        core.sharing
            .share_node(&uid, emails, role, message.as_deref())
    });
    match shared {
        Ok((proton, external)) => {
            core.log_activity(
                ActivityKind::Share,
                &target.label,
                format!("{} recipient(s) as {role}", proton + external),
                true,
            );
            ok(&format!(
                "invited {proton} Proton and {external} external user(s)"
            ))
        }
        Err(e) => {
            core.log_activity(ActivityKind::Share, &target.label, &e, false);
            CtlResponse::error(e)
        }
    }
}

fn list_share(core: &Core, target: Target) -> CtlResponse {
    match target.node.and_then(|uid| core.sharing.list_share(&uid)) {
        Ok((entries, link)) => CtlResponse::Share { entries, link },
        Err(e) => CtlResponse::error(e),
    }
}

fn remove_share_entry(
    core: &Core,
    target: Target,
    id: &str,
    kind: pdfs_core::control::ShareEntryKind,
) -> CoreResult<CtlResponse> {
    target
        .node
        .and_then(|uid| core.sharing.remove_share_entry(&uid, id, kind))?;
    core.log_activity(ActivityKind::Unshare, &target.label, "access removed", true);
    Ok(ok("removed"))
}

fn create_public_link(
    core: &Core,
    target: Target,
    role: &str,
    password: Option<&str>,
    expires: Option<i64>,
) -> CtlResponse {
    let created = target.node.and_then(|uid| {
        core.sharing
            .create_public_link(&uid, role, password, expires)
    });
    match created {
        Ok(link) => {
            core.log_activity(
                ActivityKind::PublicLink,
                &target.label,
                "link created",
                true,
            );
            CtlResponse::PublicLink { link }
        }
        Err(e) => {
            core.log_activity(ActivityKind::PublicLink, &target.label, &e, false);
            CtlResponse::error(e)
        }
    }
}

fn remove_public_link(core: &Core, target: Target, id: &str) -> CoreResult<CtlResponse> {
    target
        .node
        .and_then(|uid| core.sharing.remove_public_link(&uid, id))?;
    core.log_activity(ActivityKind::Unshare, &target.label, "link removed", true);
    Ok(ok("public link removed"))
}

/// Log a stop-sharing request, and turn a partial removal into an error that
/// names what is still shared.
fn stop_sharing(core: &Core, target: Target) -> CtlResponse {
    let result: CoreResult<StopSharingOutcome> =
        target.node.and_then(|uid| core.sharing.stop_sharing(&uid));
    match result.map(|outcome| outcome.into_result()) {
        Ok(Ok(message)) => {
            core.log_activity(
                ActivityKind::Unshare,
                &target.label,
                "stopped sharing",
                true,
            );
            CtlResponse::Ok { message }
        }
        Ok(Err(left)) => {
            core.log_activity(ActivityKind::Unshare, &target.label, &left, false);
            CtlResponse::error(pdfs_core::CoreError::remote(left))
        }
        Err(e) => CtlResponse::error(e),
    }
}
