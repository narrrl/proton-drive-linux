//! The control requests about synced folders and devices: adding, attaching, switching and removing folders, pausing, conflicts, the pending-change queue, and restoring a device's folders.

use super::*;

/// The requests [`answer`] serves, as a pattern, so the control socket's match
/// hands exactly these over and the compiler still checks that it covers every
/// request.
macro_rules! request {
    () => {
        ::pdfs_core::control::Request::ListDevices
            | ::pdfs_core::control::Request::ListLocations
            | ::pdfs_core::control::Request::RenameDevice { .. }
            | ::pdfs_core::control::Request::DeleteDevice { .. }
            | ::pdfs_core::control::Request::AdoptDevice { .. }
            | ::pdfs_core::control::Request::MigrateDevice { .. }
            | ::pdfs_core::control::Request::AddSyncFolder { .. }
            | ::pdfs_core::control::Request::AttachSyncFolder { .. }
            | ::pdfs_core::control::Request::ListSyncFolders
            | ::pdfs_core::control::Request::RemoveSyncFolder { .. }
            | ::pdfs_core::control::Request::SetSyncFolderMode { .. }
            | ::pdfs_core::control::Request::SetSyncFolderPaused { .. }
            | ::pdfs_core::control::Request::ResetSyncFolder { .. }
            | ::pdfs_core::control::Request::SyncNow { .. }
            | ::pdfs_core::control::Request::SetSyncPaused { .. }
            | ::pdfs_core::control::Request::ListConflicts
            | ::pdfs_core::control::Request::ResolveConflict { .. }
            | ::pdfs_core::control::Request::ListPendingOps
            | ::pdfs_core::control::Request::ExportPendingOp { .. }
            | ::pdfs_core::control::Request::DiscardPendingOp { .. }
            | ::pdfs_core::control::Request::RetryPendingOp { .. }
            | ::pdfs_core::control::Request::ListRestorableFolders
            | ::pdfs_core::control::Request::ListDeviceRestorableFolders { .. }
            | ::pdfs_core::control::Request::RestoreSyncFolders { .. }
            | ::pdfs_core::control::Request::RestoreDeviceFolders { .. }
    };
}
pub(super) use request;

/// Answer a request that [`request!`] matches.
pub(super) fn answer(ask: &Ask<'_>, request: CtlRequest) -> CtlResponse {
    let Ask {
        core, mountpoint, ..
    } = *ask;
    match request {
        CtlRequest::ListDevices => match core.devices().list_devices() {
            Ok(items) => CtlResponse::Devices { items },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ListLocations => match core.list_locations() {
            Ok(items) => CtlResponse::Locations { items },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::RenameDevice { uid, name } => match core.devices().rename_device(&uid, &name) {
            Ok(()) => CtlResponse::Ok {
                message: format!("renamed device to {name}"),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::DeleteDevice { uid } => match core.devices().delete_device(&uid) {
            Ok(()) => CtlResponse::Ok {
                message: "device deleted".to_string(),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::AdoptDevice { uid } => match core.devices().adopt_device(uid.as_deref()) {
            Ok(message) => CtlResponse::Ok { message },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::MigrateDevice { uid } => match core.devices().migrate_device(&uid) {
            Ok(message) => CtlResponse::Ok { message },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::AddSyncFolder { local_path } => {
            // Registering the device and uploading a folder tree far outlasts the
            // socket read timeout, so ack immediately and work on a background
            // thread. The folder appears in ListSyncFolders once the row lands;
            // completion (and any failures) go to the activity log.
            let core = core.clone();
            let path = PathBuf::from(&local_path);
            if let Err(error) = core
                .services
                .clone()
                .spawn_job("pdfs-add-sync-folder", move || {
                    let started = Instant::now();
                    match core.add_sync_folder(&path) {
                        Ok(folder) => {
                            core.profile().touch_profile();
                            let name = Path::new(&folder.local_path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(&folder.local_path)
                                .to_string();
                            core.log_activity(
                                ActivityKind::Upload,
                                format!("synced {name}"),
                                human_duration(started.elapsed()),
                                folder.state != "error",
                            );
                        }
                        Err(e) => {
                            warn!(error = %e, "add sync folder failed");
                            core.log_activity(ActivityKind::Upload, "add sync folder", &e, false);
                        }
                    }
                })
            {
                tracing::warn!(%error, "could not start pdfs-add-sync-folder");
            }
            CtlResponse::Ok {
                message: format!("adding {local_path}"),
            }
        }
        CtlRequest::AttachSyncFolder {
            local_path,
            remote_path,
        } => {
            let result = rel_to_mount(mountpoint, &remote_path).and_then(|remote| {
                core.attach_sync_folder(Path::new(&local_path), &remote, mountpoint)
            });
            match result {
                Ok(folder) => CtlResponse::Ok {
                    message: format!("sync folder {} attached: {}", folder.id, folder.local_path),
                },
                Err(error) => CtlResponse::error(error),
            }
        }
        CtlRequest::ListSyncFolders => match core.list_sync_folders() {
            Ok(items) => CtlResponse::SyncFolders { items },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::RemoveSyncFolder { id, delete_remote } => {
            match core.remove_sync_folder(id, delete_remote) {
                Ok(()) => {
                    core.profile().touch_profile();
                    CtlResponse::Ok {
                        message: "removed synced folder".to_string(),
                    }
                }
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::SetSyncFolderMode { id, mode } => {
            match core.request_sync_folder_mode(id, &mode) {
                Ok(message) => {
                    core.profile().touch_profile();
                    core.log_activity(ActivityKind::Upload, &message, "", true);
                    CtlResponse::Ok { message }
                }
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::SetSyncFolderPaused { id, paused } => {
            match core.set_sync_folder_paused(id, paused) {
                Ok(()) => CtlResponse::Ok {
                    message: match paused {
                        true => format!("folder {id} paused"),
                        false => format!("folder {id} resumed"),
                    },
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::ResetSyncFolder { id } => match core.reset_sync_folder(id) {
            Ok(message) => {
                core.log_activity(ActivityKind::Sync, &message, "", true);
                CtlResponse::Ok { message }
            }
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::SyncNow { id } => {
            core.sync_now(id);
            CtlResponse::Ok {
                message: match id {
                    Some(id) => format!("reconciling folder {id}"),
                    None => "reconciling all folders".to_string(),
                },
            }
        }
        CtlRequest::SetSyncPaused { paused, until } => match core.set_sync_paused(paused, until) {
            Ok(()) => CtlResponse::Ok {
                message: match (paused, until) {
                    (false, _) => "sync resumed".to_string(),
                    (true, None) => "sync paused until resumed".to_string(),
                    (true, Some(_)) => "sync paused".to_string(),
                },
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ListConflicts => match core.list_conflicts() {
            Ok(items) => CtlResponse::Conflicts { items },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ResolveConflict { path, keep } => match route_change(core, mountpoint, &path) {
            Ok((core, rel)) => match core.resolve_conflict(&rel, &keep) {
                Ok(message) => CtlResponse::Ok { message },
                Err(e) => CtlResponse::error(e),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ListPendingOps => match core.pending_op_infos() {
            Ok(items) => CtlResponse::PendingOps { items },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ExportPendingOp { id, dest } => {
            match core.export_pending_op(id, Path::new(&dest)) {
                Ok(written) => CtlResponse::Ok {
                    message: format!("wrote {} to {dest}", human_bytes(written)),
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::DiscardPendingOp { id } => match core.discard_pending_op(id) {
            Ok(path) => CtlResponse::Ok {
                message: format!("discarded the queued change to {path}"),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::RetryPendingOp { id } => match core.retry_pending_ops(id) {
            Ok(0) if id.is_some() => CtlResponse::Ok {
                message: "nothing to retry: the operation is already due or waiting for a rename"
                    .to_string(),
            },
            Ok(moved) => CtlResponse::Ok {
                message: format!("retrying {moved} queued operation(s) now"),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ListRestorableFolders => match core.profile().list_restorable_folders(None) {
            Ok(items) => CtlResponse::RestorableFolders { items },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ListDeviceRestorableFolders { device } => {
            match core.profile().list_restorable_folders(Some(&device)) {
                Ok(items) => CtlResponse::RestorableFolders { items },
                Err(e) => CtlResponse::error(e),
            }
        }
        request @ (CtlRequest::RestoreSyncFolders { .. }
        | CtlRequest::RestoreDeviceFolders { .. }) => {
            let (device, items) = match request {
                CtlRequest::RestoreDeviceFolders { device, items } => (Some(device), items),
                CtlRequest::RestoreSyncFolders { items } => (None, items),
                _ => unreachable!("matched above"),
            };
            // Like AddSyncFolder: each restored folder downloads a whole tree,
            // which outlasts the socket timeout. Ack, then work.
            let core = core.clone();
            if let Err(error) =
                core.services
                    .clone()
                    .spawn_job("pdfs-restore-sync-folders", move || {
                        match core.restore_sync_folders(&items, device.as_deref()) {
                            Ok(outcome) => {
                                core.profile().touch_profile();
                                if !outcome.complete() {
                                    warn!(skipped = ?outcome.skipped, "restore skipped folders");
                                }
                                core.log_activity(
                                    ActivityKind::Download,
                                    outcome.summary(),
                                    outcome.skipped.join("; "),
                                    outcome.complete(),
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "restore failed");
                                core.log_activity(
                                    ActivityKind::Download,
                                    "restore folders",
                                    &e,
                                    false,
                                );
                            }
                        }
                    })
            {
                tracing::warn!(%error, "could not start pdfs-restore-sync-folders");
            }
            CtlResponse::Ok {
                message: "restoring folders".to_string(),
            }
        }
        // `control` only hands over what `request!` matches.
        _ => CtlResponse::error(CoreError::internal("not a sync request".to_string())),
    }
}
