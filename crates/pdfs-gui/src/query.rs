//! The daemon side of both launcher front ends: ask for hits, open a chosen one.
//!
//! `--dmenu` and `--fzf` differ only in how they present a list and collect a
//! choice. Everything between the control socket and the chosen file lives here
//! so the two cannot drift on what a result set is or what "open" means — the
//! same reason both already share [`Hit`] and [`rank_hits`] with the GTK prompt.
//!
//! Everything here is blocking and GTK-free: the callers are one-shot processes
//! with no main loop to keep free.

use std::path::{Path, PathBuf};

use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    Request, Response, SearchFilters, SearchHit, SearchKind, SearchSource, send,
};
use pdfs_core::opener::{self, OpenWith};

use crate::activation::{DriveActivation, drive_activation, mounted_or_relative, mounted_target};
use crate::{Hit, file_name, is_document, is_image, is_media, rank_hits};

/// Where the mount lives, for turning a Drive path into something an
/// application can open. Falls back to the configured default so a momentarily
/// unreachable daemon still produces a plausible path rather than an error.
pub(crate) fn mountpoint(socket: &Path, dirs: &AppDirs) -> PathBuf {
    match request(socket, Request::Status) {
        Ok(Response::Status { mountpoint, .. }) => PathBuf::from(mountpoint),
        _ => dirs.default_mountpoint(),
    }
}

/// Pinned files — the same "you asked to keep these" set the GTK prompt opens
/// with, and the only listing the daemon offers without a query.
pub(crate) fn pins(socket: &Path) -> Result<Vec<Hit>, String> {
    let Response::Pins { pins } = request(socket, Request::ListPins)? else {
        return Ok(Vec::new());
    };
    Ok(pins
        .into_iter()
        .map(|pin| {
            Hit::Drive(SearchHit {
                name: file_name(&pin.path),
                path: pin.path,
                is_dir: pin.is_dir.unwrap_or(pin.recursive),
                size: 0,
                modified: 0,
                pinned: true,
                uid: pin.uid,
                mounted_path: None,
                score: 0,
            })
        })
        .collect())
}

pub(crate) fn search(socket: &Path, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let reply = request(
        socket,
        Request::SearchV2 {
            query: query.to_string(),
            limit,
            filters: SearchFilters {
                sources: vec![SearchSource::Drive, SearchSource::Local],
                kind: SearchKind::All,
            },
        },
    )?;
    let Response::SearchResultsV2 {
        drive_hits,
        local_hits,
        ..
    } = reply
    else {
        return Ok(Vec::new());
    };
    let mut hits: Vec<Hit> = drive_hits.into_iter().map(Hit::Drive).collect();
    hits.extend(local_hits.into_iter().map(Hit::Local));
    rank_hits(&mut hits);
    hits.truncate(limit);
    Ok(hits)
}

/// Icon theme name for a hit.
///
/// Coarse on purpose: `content_type_guess` would give an exact MIME name, but
/// only with file *contents* to sniff — a Drive hit is metadata, and passing it
/// an empty buffer answers `application-x-zerosize` for everything. The generic
/// names below exist in every icon theme.
pub(crate) fn icon_name(hit: &Hit) -> String {
    if hit.is_dir() {
        return "folder".to_string();
    }
    let name = hit.name();
    if is_image(name) {
        "image-x-generic"
    } else if is_media(name) {
        "video-x-generic"
    } else if is_document(name) {
        "text-x-generic"
    } else {
        "application-x-generic"
    }
    .to_string()
}

/// Open a chosen hit, mirroring the GTK prompt: local files are already on
/// disk, a Drive folder or streamable file goes through the mount, and anything
/// else is materialised by the daemon first.
pub(crate) fn open(
    socket: &Path,
    mountpoint: &Path,
    policy: &OpenWith,
    hit: &Hit,
    from_pins: bool,
) -> Result<(), String> {
    match hit {
        Hit::Local(local) => {
            opener::open(policy, Path::new(&local.path), local.is_dir);
            Ok(())
        }
        Hit::Drive(drive) => {
            // A folder has no materialised form, and a pin row's `is_dir` is pin
            // policy rather than node kind, so both go to the mount unchecked.
            let unconditional = from_pins
                || matches!(
                    drive_activation(&drive.name, drive.is_dir),
                    DriveActivation::Folder | DriveActivation::MountedMedia
                );
            if unconditional {
                opener::open(
                    policy,
                    &mounted_or_relative(mountpoint, drive),
                    drive.is_dir,
                );
                return Ok(());
            }
            // Anything else the mount exposes opens there too, so an editor
            // saves back into Drive instead of into a cache blob.
            if let Some(path) = mounted_target(mountpoint, drive) {
                opener::open(policy, &path, false);
                return Ok(());
            }
            match request(
                socket,
                Request::OpenFile {
                    path: drive.path.clone(),
                    uid: Some(drive.uid.clone()),
                },
            )? {
                Response::FilePath { path } => {
                    // The reply is a content-hash blob; the rules key off the
                    // Drive name, which is what the user actually opened.
                    opener::open_named(policy, Path::new(&path), &drive.name, false);
                    Ok(())
                }
                Response::Error { message, .. } => Err(format!("could not open: {message}")),
                _ => Err("unexpected reply from the daemon".to_string()),
            }
        }
    }
}

/// One control-socket round-trip, with the daemon-is-down case phrased for a
/// terminal rather than a status page.
pub(crate) fn request(socket: &Path, request: Request) -> Result<Response, String> {
    send(socket, &request).map_err(|e| format!("cannot reach the Proton Drive daemon: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(name: &str, is_dir: bool) -> Hit {
        Hit::Drive(SearchHit {
            name: name.into(),
            path: name.into(),
            is_dir,
            size: 0,
            modified: 0,
            pinned: false,
            uid: "uid".into(),
            mounted_path: None,
            score: 0,
        })
    }

    #[test]
    fn icons_are_theme_names_the_launcher_can_look_up() {
        assert_eq!(icon_name(&drive("Photos", true)), "folder");
        assert_eq!(icon_name(&drive("a.png", false)), "image-x-generic");
        assert_eq!(icon_name(&drive("a.md", false)), "text-x-generic");
        assert_eq!(icon_name(&drive("a.bin", false)), "application-x-generic");
    }
}
