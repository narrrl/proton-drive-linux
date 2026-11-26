//! The control requests about a file's earlier revisions: listing, restoring, deleting and saving one as a new file, by path or by uid.

use super::*;

/// The requests [`answer`] serves, as a pattern, so the control socket's match
/// hands exactly these over and the compiler still checks that it covers every
/// request.
macro_rules! request {
    () => {
        ::pdfs_core::control::Request::ListRevisions { .. }
            | ::pdfs_core::control::Request::ListRevisionsByUid { .. }
            | ::pdfs_core::control::Request::RestoreRevision { .. }
            | ::pdfs_core::control::Request::RestoreRevisionByUid { .. }
            | ::pdfs_core::control::Request::DeleteRevision { .. }
            | ::pdfs_core::control::Request::DeleteRevisionByUid { .. }
            | ::pdfs_core::control::Request::SaveRevisionAs { .. }
            | ::pdfs_core::control::Request::SaveRevisionAsByUid { .. }
    };
}
pub(super) use request;

/// The file a request is about, and how the activity feed names it: the file
/// name under the mount, or the uid.
struct Target {
    uid: NodeUid,
    name: String,
}

impl Target {
    fn path(core: &Core, rel: &Path) -> CoreResult<Self> {
        let (_ino, uid) = core.resolve(rel)?;
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| uid.to_string());
        Ok(Self { uid, name })
    }

    fn uid(core: &Core, uid: &str) -> CoreResult<Self> {
        let uid = core.resolve_anywhere(uid)?;
        let name = uid.to_string();
        Ok(Self { uid, name })
    }
}

/// Answer a request that [`request!`] matches.
pub(super) fn answer(ask: &Ask<'_>, request: CtlRequest) -> CtlResponse {
    let Ask {
        core, mountpoint, ..
    } = *ask;
    let revisions = core.revisions();
    let at = |path: &str| rel_to_mount(mountpoint, path);
    let reply = |result: CoreResult<String>| match result {
        Ok(message) => CtlResponse::Ok { message },
        Err(e) => CtlResponse::error(e),
    };
    match request {
        CtlRequest::ListRevisions { path } => {
            match at(&path).and_then(|rel| Target::path(core, &rel)) {
                Ok(file) => listed(revisions.list(&file.uid)),
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::ListRevisionsByUid { uid } => match Target::uid(core, &uid) {
            Ok(file) => listed(revisions.list(&file.uid)),
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::RestoreRevision { path, revision_id } => reply(at(&path).and_then(|rel| {
            let file = Target::path(core, &rel)?;
            revisions.restore(&file.uid, &revision_id, &file.name)?;
            // The size and mtime a listing shows describe the revision that
            // was active a moment ago.
            core.invalidate_parent_listing(&rel);
            Ok(format!("restoring version {revision_id} of {path}"))
        })),
        CtlRequest::RestoreRevisionByUid { uid, revision_id } => {
            reply(Target::uid(core, &uid).and_then(|file| {
                revisions.restore(&file.uid, &revision_id, &file.name)?;
                Ok(format!("restoring version {revision_id}"))
            }))
        }
        CtlRequest::DeleteRevision { path, revision_id } => reply(at(&path).and_then(|rel| {
            let file = Target::path(core, &rel)?;
            revisions.delete(&file.uid, &revision_id, &file.name)?;
            Ok(format!("deleted version {revision_id} of {path}"))
        })),
        CtlRequest::DeleteRevisionByUid { uid, revision_id } => {
            reply(Target::uid(core, &uid).and_then(|file| {
                revisions.delete(&file.uid, &revision_id, &file.name)?;
                Ok(format!("deleted version {revision_id}"))
            }))
        }
        CtlRequest::SaveRevisionAs {
            path,
            revision_id,
            dest,
        } => reply(at(&path).and_then(|rel| {
            let file = Target::path(core, &rel)?;
            let written =
                revisions.save_as(&file.uid, &revision_id, &file.name, Path::new(&dest))?;
            Ok(format!("wrote {} to {dest}", human_bytes(written)))
        })),
        CtlRequest::SaveRevisionAsByUid {
            uid,
            revision_id,
            dest,
        } => reply(Target::uid(core, &uid).and_then(|file| {
            let written =
                revisions.save_as(&file.uid, &revision_id, &file.name, Path::new(&dest))?;
            Ok(format!("wrote {} to {dest}", human_bytes(written)))
        })),
        // `control` only hands over what `request!` matches.
        _ => CtlResponse::error(CoreError::internal("not a revisions request".to_string())),
    }
}

fn listed(result: CoreResult<Vec<pdfs_core::control::RevisionInfo>>) -> CtlResponse {
    match result {
        Ok(items) => CtlResponse::Revisions { items },
        Err(e) => CtlResponse::error(e),
    }
}
