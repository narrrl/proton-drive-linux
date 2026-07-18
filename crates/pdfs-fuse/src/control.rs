//! The control socket: the daemon`s IPC surface.
//!
//! The CLI and the GUI never touch the mount or the database directly — they
//! open this Unix socket, write one JSON [`CtlRequest`] line, and read one JSON
//! [`CtlResponse`] line back. That keeps the daemon the single writer of both
//! the FUSE state and `cache.db`.
//!
//! Connections are served concurrently, one thread each, because requests differ
//! wildly in cost: an `OpenPhoto` downloads a whole photo, and a serial loop
//! would stall the GUI`s 2s status poll behind it.

use std::io::{BufRead, BufReader, Write as _};
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    ActivityKind, PhotoMonth, RefreshScope, Request as CtlRequest, Response as CtlResponse,
    TransferDirection,
};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{info, warn};

use super::transfers::CountingReader;
use super::{Core, count_noun, human_bytes, human_duration, parse_uid};

/// Turn a CLI-supplied path into a mountpoint-relative path. An absolute path
/// must live under `mountpoint`; a relative path is taken as already relative to
/// the mount root.
fn rel_to_mount(mountpoint: &Path, path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if p.is_absolute() {
        p.strip_prefix(mountpoint)
            .map(Path::to_path_buf)
            .map_err(|_| format!("{path} is not under the mountpoint"))
    } else {
        Ok(p.to_path_buf())
    }
}

/// Handle one control-socket connection: read a single JSON request line,
/// dispatch it against `core`, and write back a JSON response line.
fn handle_control_conn(core: &Core, username: &str, mountpoint: &Path, stream: UnixStream) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "control: clone stream failed");
            return;
        }
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let response = match serde_json::from_str::<CtlRequest>(line.trim()) {
        Ok(CtlRequest::Status) => {
            let pins = core.cache.list_pins();
            let queued = core.db.pending_op_counts().unwrap_or_default();
            CtlResponse::Status {
                username: username.to_string(),
                mountpoint: mountpoint.display().to_string(),
                pinned: pins.len(),
                used: core.cache.usage(),
                budget: core.cache.budget(),
                pins,
                online: core.online.load(Ordering::Relaxed),
                pending_uploads: queued.uploads.max(0) as u64,
                pending_changes: queued.changes.max(0) as u64,
            }
        }
        Ok(CtlRequest::Pin { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.pin(&rel) {
                Ok(name) => CtlResponse::Ok {
                    message: format!("pinned {name}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Unpin { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.unpin(&rel) {
                Ok(name) => CtlResponse::Ok {
                    message: format!("unpinned {name}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListPins) => CtlResponse::Pins {
            pins: core.cache.list_pins(),
        },
        Ok(CtlRequest::ListDir { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.list_dir(&rel) {
                Ok(entries) => CtlResponse::Entries { entries },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Refresh { scope }) => {
            let result = match &scope {
                RefreshScope::Dir { path } => {
                    rel_to_mount(mountpoint, path).and_then(|rel| core.refresh_dir(&rel))
                }
                RefreshScope::Trash => {
                    core.invalidate_trash();
                    Ok(())
                }
                RefreshScope::Photos => {
                    core.invalidate_photos();
                    Ok(())
                }
            };
            match result {
                Ok(()) => CtlResponse::Ok {
                    message: "refreshed".to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::PhotosTimeline {
            offset,
            limit,
            kind,
            range,
        }) => {
            match core.photos_timeline(offset, limit, kind, range) {
                Ok(Some(items)) => CtlResponse::Photos {
                    available: true,
                    items,
                    counts: core.db.photos_counts().ok(),
                },
                Ok(None) => CtlResponse::Photos {
                    available: false,
                    items: Vec::new(),
                    counts: None,
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::PhotoMonths { kind }) => match core.db.photos_months(kind) {
            Ok(months) => CtlResponse::PhotoMonths {
                months: months
                    .into_iter()
                    .map(|(year, month, count)| PhotoMonth {
                        year,
                        month,
                        count,
                    })
                    .collect(),
            },
            Err(e) => CtlResponse::Error {
                message: e.to_string(),
            },
        },
        Ok(CtlRequest::PhotoThumbs { uids }) => {
            let parsed: Vec<NodeUid> = uids.iter().filter_map(|u| parse_uid(u)).collect();
            CtlResponse::Thumbs {
                items: core.photo_thumbs(&parsed),
            }
        }
        Ok(CtlRequest::OpenPhoto { uid }) => match parse_uid(&uid) {
            Some(u) => match core.open_photo(&u) {
                Ok(p) => CtlResponse::FilePath {
                    path: p.display().to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            None => CtlResponse::Error {
                message: format!("bad uid: {uid}"),
            },
        },
        Ok(CtlRequest::UploadPhoto {
            name,
            media_type,
            bytes,
            capture_time,
        }) => {
            let photos = core.photos();
            let metadata = proton_drive_rs::PhotoUploadMetadata {
                capture_time,
                ..Default::default()
            };
            let guard = core.transfers.begin(
                name.clone(),
                "",
                TransferDirection::Upload,
                bytes.len() as u64,
            );
            let reader = CountingReader::new(std::io::Cursor::new(&bytes), &guard);
            match core.rt.block_on(photos.upload_photo_from(
                &name,
                &media_type,
                reader,
                bytes.len() as i64,
                Vec::new(),
                metadata,
                false,
            )) {
                Ok(uid) => {
                    // The photo we just uploaded belongs at the head of the
                    // timeline, and the GUI reloads the gallery the moment this
                    // reply lands — so refresh now rather than leaving it to a
                    // background pass that would land just after that reload.
                    if let Err(e) = core.rt.block_on(core.refresh_timeline()) {
                        warn!(error = %e, "timeline refresh after upload failed");
                    }
                    CtlResponse::Ok {
                        message: format!("uploaded photo with uid {uid}"),
                    }
                }
                Err(e) => CtlResponse::Error {
                    message: format!("upload photo failed: {e}"),
                },
            }
        }
        Ok(CtlRequest::OpenFile { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.open_file(&rel) {
                Ok(p) => CtlResponse::FilePath {
                    path: p.display().to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Search { query, limit }) => match core.search(&query, limit) {
            Ok(hits) => CtlResponse::SearchResults { hits },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::SearchLocal { query, limit }) => match core.search_local(&query, limit) {
            Ok(hits) => CtlResponse::LocalResults {
                hits,
                indexing: core.indexing.load(Ordering::Relaxed),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Rename { path, new_name }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.rename(&rel, &new_name) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Rename, &name, format!("was {path}"), true);
                    CtlResponse::Ok {
                        message: format!("renamed to {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Rename, &path, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Move { path, new_parent }) => {
            match (
                rel_to_mount(mountpoint, &path),
                rel_to_mount(mountpoint, &new_parent),
            ) {
                (Ok(rel), Ok(parent_rel)) => match core.move_to(&rel, &parent_rel) {
                    Ok(name) => {
                        let dest = if new_parent.is_empty() {
                            "My files".to_string()
                        } else {
                            new_parent.clone()
                        };
                        core.log_activity(ActivityKind::Move, &name, format!("to {dest}"), true);
                        CtlResponse::Ok {
                            message: format!("moved {name}"),
                        }
                    }
                    Err(e) => {
                        core.log_activity(ActivityKind::Move, &path, &e, false);
                        CtlResponse::Error { message: e }
                    }
                },
                (Err(e), _) | (_, Err(e)) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::Delete { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.delete(&rel) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Trash, &name, "", true);
                    CtlResponse::Ok {
                        message: format!("trashed {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Trash, &path, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::CreateFolder { parent, name }) => match rel_to_mount(mountpoint, &parent) {
            Ok(parent_rel) => match core.create_folder(&parent_rel, &name) {
                Ok(name) => {
                    core.log_activity(ActivityKind::CreateFolder, &name, "", true);
                    CtlResponse::Ok {
                        message: format!("created folder {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::CreateFolder, &name, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::UploadFile {
            parent,
            name,
            bytes,
        }) => match rel_to_mount(mountpoint, &parent) {
            Ok(parent_rel) => match core.upload(&parent_rel, &name, &bytes) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Upload, &name, "", true);
                    CtlResponse::Ok {
                        message: format!("uploaded {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Upload, &name, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::UploadPaths { parent, sources }) => {
            match rel_to_mount(mountpoint, &parent) {
                // Ack immediately and upload on a background thread: a directory tree
                // far outlasts the socket read timeout. Progress and completion are
                // observed through GetQueueStatus; the activity log gets the summary
                // when the whole batch finishes.
                Ok(parent_rel) => {
                    let core = core.clone();
                    let paths: Vec<PathBuf> = sources.into_iter().map(PathBuf::from).collect();
                    let n = paths.len();
                    std::thread::spawn(move || {
                        let started = Instant::now();
                        match core.upload_paths(&parent_rel, &paths) {
                            Ok(stats) => {
                                // e.g. "Uploaded 700 files to Photos" — name the destination
                                // so the log reads like a sentence rather than a bare count.
                                let dest = parent_rel
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("your Drive");
                                let target = format!(
                                    "{} to {dest}",
                                    count_noun(stats.uploaded, "file", "files")
                                );
                                // Size · folders · elapsed, with a trailing failure count
                                // when some files didn't make it.
                                let mut parts = vec![human_bytes(stats.bytes)];
                                if stats.folders > 0 {
                                    parts.push(count_noun(stats.folders, "folder", "folders"));
                                }
                                parts.push(human_duration(started.elapsed()));
                                if stats.failed > 0 {
                                    parts.push(format!("{} failed", stats.failed));
                                }
                                core.log_activity(
                                    ActivityKind::Upload,
                                    target,
                                    parts.join(" · "),
                                    stats.failed == 0,
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "bulk upload failed");
                                core.log_activity(ActivityKind::Upload, "bulk upload", &e, false);
                            }
                        }
                    });
                    CtlResponse::Ok {
                        message: format!("uploading {n} item(s)"),
                    }
                }
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::ListTrash) => match core.list_trash() {
            Ok(entries) => CtlResponse::Entries { entries },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Restore { uids }) => match core.restore(&uids) {
            Ok(n) => {
                core.log_activity(ActivityKind::Restore, format!("{n} item(s)"), "", true);
                CtlResponse::Ok {
                    message: format!("restored {n} item(s)"),
                }
            }
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::DeleteForever { uids }) => match core.delete_forever(&uids) {
            Ok(n) => {
                core.log_activity(
                    ActivityKind::DeleteForever,
                    format!("{n} item(s)"),
                    "",
                    true,
                );
                CtlResponse::Ok {
                    message: format!("permanently deleted {n} item(s)"),
                }
            }
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::EmptyTrash) => match core.empty_trash() {
            Ok(n) => {
                core.log_activity(ActivityKind::EmptyTrash, format!("{n} item(s)"), "", true);
                CtlResponse::Ok {
                    message: format!("emptied trash ({n} item(s))"),
                }
            }
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::PurgeCache) => {
            let freed = core.cache.clear_unpinned();
            CtlResponse::Ok {
                message: format!(
                    "purged {:.1} MiB of unpinned cache",
                    freed as f64 / 1_048_576.0
                ),
            }
        }
        Ok(CtlRequest::GetQueueStatus) => CtlResponse::Transfers {
            items: core.transfers.snapshot(),
            jobs: core.jobs_snapshot(),
        },
        Ok(CtlRequest::SetCacheBudget { bytes }) => {
            core.cache.set_budget(bytes);
            // Persist so the next mount keeps the new cap. Best-effort: a config
            // write failure is reported but the live cap is already applied.
            match AppDirs::new().map(|dirs| {
                let mut cfg = dirs.load_config();
                cfg.cache_budget = Some(bytes);
                dirs.save_config(&cfg)
            }) {
                Ok(Ok(())) => CtlResponse::Ok {
                    message: format!("cache budget set to {bytes} bytes"),
                },
                Ok(Err(e)) => CtlResponse::Error {
                    message: format!("budget applied but config write failed: {e}"),
                },
                Err(e) => CtlResponse::Error {
                    message: format!("budget applied but config unavailable: {e}"),
                },
            }
        }
        Ok(CtlRequest::ListDevices) => match core.list_devices() {
            Ok(items) => CtlResponse::Devices { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RenameDevice { uid, name }) => match core.rename_device(&uid, &name) {
            Ok(()) => CtlResponse::Ok {
                message: format!("renamed device to {name}"),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::DeleteDevice { uid }) => match core.delete_device(&uid) {
            Ok(()) => CtlResponse::Ok {
                message: "device deleted".to_string(),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::AddSyncFolder { local_path }) => {
            // Registering the device and uploading a folder tree far outlasts the
            // socket read timeout, so ack immediately and work on a background
            // thread. The folder appears in ListSyncFolders once the row lands;
            // completion (and any failures) go to the activity log.
            let core = core.clone();
            let path = PathBuf::from(&local_path);
            std::thread::spawn(move || {
                let started = Instant::now();
                match core.add_sync_folder(&path) {
                    Ok(folder) => {
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
            });
            CtlResponse::Ok {
                message: format!("adding {local_path}"),
            }
        }
        Ok(CtlRequest::ListSyncFolders) => match core.list_sync_folders() {
            Ok(items) => CtlResponse::SyncFolders { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RemoveSyncFolder { id, delete_remote }) => {
            match core.remove_sync_folder(id, delete_remote) {
                Ok(()) => CtlResponse::Ok {
                    message: "removed synced folder".to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::SetSyncFolderMode { id, mode }) => {
            match core.request_sync_folder_mode(id, &mode) {
                Ok(message) => {
                    core.log_activity(ActivityKind::Upload, &message, "", true);
                    CtlResponse::Ok { message }
                }
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::SyncNow { id }) => {
            core.sync_now(id);
            CtlResponse::Ok {
                message: match id {
                    Some(id) => format!("reconciling folder {id}"),
                    None => "reconciling all folders".to_string(),
                },
            }
        }
        Ok(CtlRequest::ShareNode {
            path,
            emails,
            role,
            message,
        }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.share_node(&rel, &emails, &role, message.as_deref()) {
                Ok((proton, external)) => {
                    core.log_activity(
                        ActivityKind::Share,
                        &path,
                        format!("{} recipient(s) as {role}", proton + external),
                        true,
                    );
                    CtlResponse::Ok {
                        message: format!("invited {proton} Proton and {external} external user(s)"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Share, &path, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListShare { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.list_share(&rel) {
                Ok((entries, link)) => CtlResponse::Share { entries, link },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::UpdateShareRole {
            path,
            id,
            kind,
            role,
        }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.update_share_role(&rel, &id, kind, &role) {
                Ok(()) => CtlResponse::Ok {
                    message: format!("role updated to {role}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RemoveShareEntry { path, id, kind }) => {
            match rel_to_mount(mountpoint, &path) {
                Ok(rel) => match core.remove_share_entry(&rel, &id, kind) {
                    Ok(()) => {
                        core.log_activity(ActivityKind::Unshare, &path, "access removed", true);
                        CtlResponse::Ok {
                            message: "removed".to_string(),
                        }
                    }
                    Err(e) => CtlResponse::Error { message: e },
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::CreatePublicLink {
            path,
            role,
            password,
            expires,
        }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.create_public_link(&rel, &role, password.as_deref(), expires) {
                Ok(link) => {
                    core.log_activity(ActivityKind::PublicLink, &path, "link created", true);
                    CtlResponse::PublicLink { link }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::PublicLink, &path, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RemovePublicLink { path, id }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.remove_public_link(&rel, &id) {
                Ok(()) => {
                    core.log_activity(ActivityKind::Unshare, &path, "link removed", true);
                    CtlResponse::Ok {
                        message: "public link removed".to_string(),
                    }
                }
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListSharedWithMe) => match core.list_shared_with_me() {
            Ok(entries) => CtlResponse::Entries { entries },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::LeaveShared { uid }) => match core.leave_shared(&uid) {
            Ok(()) => {
                core.log_activity(ActivityKind::Unshare, "shared item", "left", true);
                CtlResponse::Ok {
                    message: "left shared item".to_string(),
                }
            }
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListInvitations) => match core.list_invitations() {
            Ok(items) => CtlResponse::Invitations { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::AcceptInvitation { id }) => match core.accept_invitation(&id) {
            Ok(()) => CtlResponse::Ok {
                message: "invitation accepted".to_string(),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RejectInvitation { id }) => match core.reject_invitation(&id) {
            Ok(()) => CtlResponse::Ok {
                message: "invitation rejected".to_string(),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListBookmarks) => match core.list_bookmarks() {
            Ok(items) => CtlResponse::Bookmarks { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::CreateBookmark { url, password }) => {
            match core.create_bookmark(&url, password.as_deref()) {
                Ok(()) => CtlResponse::Ok {
                    message: "bookmark saved".to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::DeleteBookmark { token }) => match core.delete_bookmark(&token) {
            Ok(()) => CtlResponse::Ok {
                message: "bookmark removed".to_string(),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListSharedByMe) => match core.list_shared_by_me() {
            Ok(items) => CtlResponse::SharedByMe { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListActivity { limit }) => CtlResponse::Activity {
            items: core.list_activity(limit),
        },
        Err(e) => CtlResponse::Error {
            message: format!("bad request: {e}"),
        },
    };
    let mut out = match serde_json::to_vec(&response) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "control: serialize response failed");
            return;
        }
    };
    out.push(b'\n');
    let mut stream = stream;
    let _ = stream.write_all(&out);
}

/// Listen on the control socket, serving one command per connection, each on its
/// own thread. Runs on its own thread; returns only if the listener itself fails.
///
/// Concurrent rather than serial because requests differ wildly in cost: an
/// `OpenPhoto` downloads a whole photo, and while it ran the accept loop used to
/// stall every other caller behind it — the GUI's 2s status poll, and the
/// thumbnail batches the gallery needs to paint. [`Core`] is a bundle of handles
/// (`Arc`/`Clone`), so each connection gets its own copy of it.
pub(crate) fn run_control_socket(core: Core, username: String, mountpoint: PathBuf, listener: UnixListener) {
    info!("control socket listening");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let core = core.clone();
                let username = username.clone();
                let mountpoint = mountpoint.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("pdfs-control".into())
                    .spawn(move || handle_control_conn(&core, &username, &mountpoint, stream))
                {
                    warn!(error = %e, "control: spawn handler failed");
                }
            }
            Err(e) => {
                warn!(error = %e, "control: accept failed");
            }
        }
    }
}
