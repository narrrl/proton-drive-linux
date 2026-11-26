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

/// Answer a request that [`request!`] matches.
pub(super) fn answer(ask: &Ask<'_>, request: CtlRequest) -> CtlResponse {
    let Ask {
        core, mountpoint, ..
    } = *ask;
    match request {
        CtlRequest::ListRevisions { path } => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.list_revisions(&rel) {
                Ok(items) => CtlResponse::Revisions { items },
                Err(e) => CtlResponse::error(e),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ListRevisionsByUid { uid } => match core.list_revisions_by_uid(&uid) {
            Ok(items) => CtlResponse::Revisions { items },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::RestoreRevision { path, revision_id } => {
            match rel_to_mount(mountpoint, &path) {
                Ok(rel) => match core.restore_revision(&rel, &revision_id) {
                    Ok(()) => CtlResponse::Ok {
                        message: format!("restoring version {revision_id} of {path}"),
                    },
                    Err(e) => CtlResponse::error(e),
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::RestoreRevisionByUid { uid, revision_id } => {
            match core.restore_revision_by_uid(&uid, &revision_id) {
                Ok(()) => CtlResponse::Ok {
                    message: format!("restoring version {revision_id}"),
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::DeleteRevision { path, revision_id } => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.delete_revision(&rel, &revision_id) {
                Ok(()) => CtlResponse::Ok {
                    message: format!("deleted version {revision_id} of {path}"),
                },
                Err(e) => CtlResponse::error(e),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::DeleteRevisionByUid { uid, revision_id } => {
            match core.delete_revision_by_uid(&uid, &revision_id) {
                Ok(()) => CtlResponse::Ok {
                    message: format!("deleted version {revision_id}"),
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::SaveRevisionAs {
            path,
            revision_id,
            dest,
        } => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.save_revision_as(&rel, &revision_id, Path::new(&dest)) {
                Ok(written) => CtlResponse::Ok {
                    message: format!("wrote {} to {dest}", human_bytes(written)),
                },
                Err(e) => CtlResponse::error(e),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::SaveRevisionAsByUid {
            uid,
            revision_id,
            dest,
        } => match core.save_revision_as_by_uid(&uid, &revision_id, Path::new(&dest)) {
            Ok(written) => CtlResponse::Ok {
                message: format!("wrote {} to {dest}", human_bytes(written)),
            },
            Err(e) => CtlResponse::error(e),
        },
        // `control` only hands over what `request!` matches.
        _ => CtlResponse::error(CoreError::internal("not a revisions request".to_string())),
    }
}
