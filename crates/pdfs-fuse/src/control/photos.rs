//! The control requests about Photos: the timeline, albums, thumbnails, the gallery, and Takeout imports and redating.

use super::*;

/// The requests [`answer`] serves, as a pattern, so the control socket's match
/// hands exactly these over and the compiler still checks that it covers every
/// request.
macro_rules! request {
    () => {
        ::pdfs_core::control::Request::PhotosTimeline { .. }
            | ::pdfs_core::control::Request::PhotoDuplicates
            | ::pdfs_core::control::Request::PhotoSimilar
            | ::pdfs_core::control::Request::PhotoPlaces
            | ::pdfs_core::control::Request::PlacePhotos { .. }
            | ::pdfs_core::control::Request::PlaceLocations { .. }
            | ::pdfs_core::control::Request::PhotosOnThisDay { .. }
            | ::pdfs_core::control::Request::PhotoGroup { .. }
            | ::pdfs_core::control::Request::SetPhotoFavorite { .. }
            | ::pdfs_core::control::Request::PhotoAlbums
            | ::pdfs_core::control::Request::AlbumPhotos { .. }
            | ::pdfs_core::control::Request::CreateAlbum { .. }
            | ::pdfs_core::control::Request::RenameAlbum { .. }
            | ::pdfs_core::control::Request::DeleteAlbum { .. }
            | ::pdfs_core::control::Request::AddToAlbum { .. }
            | ::pdfs_core::control::Request::RemoveFromAlbum { .. }
            | ::pdfs_core::control::Request::PhotoPresence { .. }
            | ::pdfs_core::control::Request::PhotoMonths { .. }
            | ::pdfs_core::control::Request::PhotoThumbs { .. }
            | ::pdfs_core::control::Request::FileThumbs { .. }
            | ::pdfs_core::control::Request::ReserveFileThumbGeneration
            | ::pdfs_core::control::Request::CancelFileThumbs { .. }
            | ::pdfs_core::control::Request::StartThumbnailBuild { .. }
            | ::pdfs_core::control::Request::CancelThumbnailBuild
            | ::pdfs_core::control::Request::ThumbnailBuildStatus
            | ::pdfs_core::control::Request::PhotosRefreshStatus
            | ::pdfs_core::control::Request::OpenPhoto { .. }
            | ::pdfs_core::control::Request::DriveGallery { .. }
            | ::pdfs_core::control::Request::OpenGalleryVideo { .. }
            | ::pdfs_core::control::Request::OpenGalleryImage { .. }
            | ::pdfs_core::control::Request::CloseGalleryVideo { .. }
            | ::pdfs_core::control::Request::GalleryThumbnail { .. }
            | ::pdfs_core::control::Request::ImportTakeout { .. }
            | ::pdfs_core::control::Request::ImportStatus
            | ::pdfs_core::control::Request::ResumeImport
            | ::pdfs_core::control::Request::CancelImport
            | ::pdfs_core::control::Request::RedatePhotos { .. }
            | ::pdfs_core::control::Request::RedateStatus
            | ::pdfs_core::control::Request::CancelRedate
            | ::pdfs_core::control::Request::UploadPhoto { .. }
    };
}
pub(super) use request;

/// Answer a request that [`request!`] matches.
pub(super) fn answer(ask: &Ask<'_>, request: CtlRequest) -> CtlResponse {
    let Ask {
        core,
        mountpoint,
        stream,
        visibility,
        excluded,
        ..
    } = *ask;
    match request {
        CtlRequest::PhotosTimeline {
            offset,
            limit,
            kind,
            range,
            favorites,
            not_in_album,
        } => match core.photos_timeline(
            offset,
            clamp_limit(limit),
            kind,
            range,
            favorites,
            not_in_album,
            excluded,
        ) {
            Ok(Some(items)) => CtlResponse::Photos {
                available: true,
                items,
                counts: core.db.photos_counts_visible(excluded).ok(),
            },
            Ok(None) => CtlResponse::Photos {
                available: false,
                items: Vec::new(),
                counts: None,
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::PhotoDuplicates => match core.photo_duplicates() {
            Ok(sets) => CtlResponse::PhotoDuplicates { sets },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::PhotoSimilar => match core.photo_similar() {
            Ok((sets, hashing, uncompared)) => CtlResponse::PhotoSimilar {
                sets,
                hashing,
                uncompared,
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::PhotoPlaces => match core.photo_places(excluded) {
            Ok(items) => CtlResponse::Places {
                items,
                mapping: core.timeline_progress(),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::PlacePhotos { id, offset, limit } => {
            match core.place_photos(id, offset, clamp_limit(limit), excluded) {
                Ok(items) => CtlResponse::Photos {
                    available: true,
                    items,
                    counts: None,
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::PlaceLocations { id } => match core.place_locations(id) {
            Ok(items) => CtlResponse::PhotoLocations { items },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::PhotosOnThisDay { limit } => {
            match core.photos_on_this_day(clamp_limit(limit), excluded) {
                Ok(items) => CtlResponse::Photos {
                    available: true,
                    items,
                    counts: None,
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        CtlRequest::PhotoGroup { uid } => match core.photo_group(&uid) {
            Ok(items) => CtlResponse::Photos {
                available: true,
                items,
                counts: None,
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::SetPhotoFavorite { uid, favorite } => match parse_uid(&uid) {
            Some(node_uid) => match core.set_photo_favorite(&node_uid, favorite) {
                Ok(()) => CtlResponse::Ok {
                    message: if favorite {
                        "added to favorites".to_string()
                    } else {
                        "removed from favorites".to_string()
                    },
                },
                Err(e) => CtlResponse::error(e),
            },
            None => CtlResponse::error(CoreError::invalid(format!("invalid uid: {uid}"))),
        },
        CtlRequest::PhotoAlbums => match core.albums() {
            Ok(Some(items)) => CtlResponse::Albums {
                available: true,
                items,
            },
            Ok(None) => CtlResponse::Albums {
                available: false,
                items: Vec::new(),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::AlbumPhotos { uid, offset, limit } => match parse_uid(&uid) {
            // An album page is a timeline page as far as the front-end is
            // concerned, so it comes back in the same reply — with `counts`
            // unset, which describe the whole timeline rather than this album.
            Some(album) => match visibility
                .permits(&uid)
                .and_then(|()| core.album_photos(&album, offset, clamp_limit(limit), excluded))
            {
                Ok(items) => CtlResponse::Photos {
                    available: true,
                    items,
                    counts: None,
                },
                Err(e) => CtlResponse::error(e),
            },
            None => CtlResponse::error(CoreError::invalid(format!("bad album uid: {uid}"))),
        },
        CtlRequest::CreateAlbum { name } => match core.create_album(&name) {
            Ok(uid) => CtlResponse::AlbumCreated { uid },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::RenameAlbum { uid, name } => match parse_uid(&uid) {
            Some(album) => match core.rename_album(&album, &name) {
                Ok(()) => CtlResponse::Ok {
                    message: format!("renamed album to {name}"),
                },
                Err(e) => CtlResponse::error(e),
            },
            None => CtlResponse::error(CoreError::invalid(format!("bad album uid: {uid}"))),
        },
        CtlRequest::DeleteAlbum { uid, delete_photos } => match parse_uid(&uid) {
            Some(album) => match core.delete_album(&album, delete_photos) {
                Ok(()) => CtlResponse::Ok {
                    message: "album deleted".to_string(),
                },
                Err(e) => CtlResponse::error(e),
            },
            None => CtlResponse::error(CoreError::invalid(format!("bad album uid: {uid}"))),
        },
        CtlRequest::AddToAlbum { uid, photos } => album_change(&uid, &photos, |album, photos| {
            core.add_to_album(album, photos)
        }),
        CtlRequest::RemoveFromAlbum { uid, photos } => {
            album_change(&uid, &photos, |album, photos| {
                core.remove_from_album(album, photos)
            })
        }
        CtlRequest::PhotoPresence { uid, album } => match core.cached_gallery_visibility() {
            Ok(visibility)
                if visibility.media.contains(&uid)
                    || album
                        .as_ref()
                        .is_some_and(|uid| visibility.albums.contains(uid)) =>
            {
                CtlResponse::PhotoPresence { present: false }
            }
            Ok(_) => match core.db.photos_contains(&uid, album.as_deref()) {
                Ok(present) => CtlResponse::PhotoPresence { present },
                Err(error) => CtlResponse::error(error.into()),
            },
            Err(error) => CtlResponse::error(error),
        },
        CtlRequest::PhotoMonths { .. } => unreachable!("handled before visibility requests"),
        CtlRequest::PhotoThumbs { uids } => {
            let parsed: Vec<NodeUid> = uids
                .iter()
                .filter(|uid| !visibility.media.contains(*uid))
                .filter_map(|u| parse_uid(u))
                .collect();
            CtlResponse::Thumbs {
                items: core.photo_thumbs(&parsed),
            }
        }
        CtlRequest::FileThumbs { items, generation } => {
            match core.file_thumbs(&items, generation) {
                Some(items) => CtlResponse::Thumbs { items },
                None => CtlResponse::FileThumbsStale,
            }
        }
        CtlRequest::ReserveFileThumbGeneration => CtlResponse::FileThumbGeneration {
            generation: core.reserve_file_thumb_generation(),
        },
        CtlRequest::CancelFileThumbs { generation } => {
            core.cancel_file_thumbs(generation);
            CtlResponse::Ok {
                message: "thumbnail requests cancelled".to_string(),
            }
        }
        CtlRequest::StartThumbnailBuild { path } => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.start_thumbnail_build(rel) {
                Ok(status) => CtlResponse::ThumbnailBuild { status },
                Err(error) => CtlResponse::error(error),
            },
            Err(error) => CtlResponse::error(error),
        },
        CtlRequest::CancelThumbnailBuild => CtlResponse::ThumbnailBuild {
            status: core.cancel_thumbnail_build(),
        },
        CtlRequest::ThumbnailBuildStatus => CtlResponse::ThumbnailBuild {
            status: core.thumbnail_build_status(),
        },
        CtlRequest::PhotosRefreshStatus => CtlResponse::PhotosRefresh {
            running: core.timeline_refreshing(),
        },
        CtlRequest::OpenPhoto { uid } => match parse_uid(&uid) {
            Some(u) => match visibility.permits(&uid).and_then(|()| core.open_photo(&u)) {
                Ok(p) => CtlResponse::FilePath {
                    path: p.display().to_string(),
                },
                Err(e) => CtlResponse::error(e),
            },
            None => CtlResponse::error(CoreError::invalid(format!("bad uid: {uid}"))),
        },
        CtlRequest::DriveGallery {
            action: pdfs_core::control::GalleryAction::AuthenticateHidden,
        } => core
            .authenticate_hidden_gallery(stream)
            .unwrap_or_else(CtlResponse::error),
        CtlRequest::DriveGallery { action } => core
            .drive_gallery(action)
            .unwrap_or_else(CtlResponse::error),
        CtlRequest::OpenGalleryVideo { uid, private } => core
            .open_gallery_video(&uid, private)
            .unwrap_or_else(CtlResponse::error),
        CtlRequest::OpenGalleryImage { uid, private: true } => {
            core.gallery_image(&uid).unwrap_or_else(CtlResponse::error)
        }
        CtlRequest::OpenGalleryImage {
            uid,
            private: false,
        } => match visibility
            .permits(&uid)
            .and_then(|()| core.sharing.open_shared_file(&uid))
        {
            Ok(path) => CtlResponse::FilePath {
                path: path.display().to_string(),
            },
            Err(error) => CtlResponse::error(error),
        },
        CtlRequest::CloseGalleryVideo { token } => core
            .close_gallery_video(&token)
            .unwrap_or_else(CtlResponse::error),
        CtlRequest::GalleryThumbnail { uid, private } => core
            .gallery_thumbnail(&uid, private)
            .unwrap_or_else(CtlResponse::error),
        CtlRequest::ImportTakeout { archives, dry_run } => {
            if archives.is_empty() {
                CtlResponse::error(CoreError::invalid("no Takeout archives given"))
            } else {
                let paths = archives.into_iter().map(PathBuf::from).collect();
                start_takeout_import(core, paths, dry_run)
            }
        }
        CtlRequest::ImportStatus => {
            let report = crate::takeout::import_status();
            CtlResponse::ImportStatus {
                running: report.running,
                summary: report.summary,
                progress: report.progress,
                resumable: report.resumable,
            }
        }
        CtlRequest::ResumeImport => match crate::takeout::resumable_archives() {
            None => CtlResponse::error(CoreError::not_found("there is no import to resume")),
            Some(paths) => start_takeout_import(core, paths, false),
        },
        CtlRequest::CancelImport => {
            if crate::takeout::cancel_import() {
                CtlResponse::Ok {
                    message: "stopping the import".to_string(),
                }
            } else {
                CtlResponse::error(CoreError::invalid("no import is running"))
            }
        }
        CtlRequest::RedatePhotos {
            dry_run,
            limit,
            range,
        } => match core.spawn_redate(dry_run, limit, range) {
            Ok(()) => CtlResponse::Ok {
                message: if dry_run {
                    "checking photo dates".to_string()
                } else {
                    "fixing photo dates".to_string()
                },
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::RedateStatus => {
            let (running, summary) = crate::redate::redate_status();
            CtlResponse::RedateStatus { running, summary }
        }
        CtlRequest::CancelRedate => {
            if crate::redate::cancel_redate() {
                CtlResponse::Ok {
                    message: "stopping the re-date".to_string(),
                }
            } else {
                CtlResponse::error(CoreError::invalid("no photo re-date is running"))
            }
        }
        CtlRequest::UploadPhoto {
            name,
            media_type,
            source_path,
            capture_time,
        } => {
            // Streamed off disk rather than carried over the socket: see the note
            // on `Request::UploadPhoto`. The length comes from the same handle the
            // bytes do, so a file changing size under us cannot make the two
            // disagree.
            match std::fs::File::open(&source_path).and_then(|f| f.metadata().map(|m| (f, m.len())))
            {
                Err(e) => CtlResponse::error(CoreError::invalid(format!(
                    "cannot read {source_path}: {e}"
                ))),
                Ok((file, len)) => {
                    let photos = core.photos();
                    // Unless the caller knows better: the photo's own EXIF,
                    // then the file's modification time, which a copy resets.
                    let capture_time = capture_time.or_else(|| {
                        let path = Path::new(&source_path);
                        pdfs_core::exiftime::capture_time_of(path).or_else(|| {
                            let modified =
                                std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
                            let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
                            Some(since.as_secs() as i64)
                        })
                    });
                    let metadata = proton_drive_rs::PhotoUploadMetadata {
                        capture_time,
                        last_modification_time: capture_time
                            .and_then(pdfs_core::timestamp::modification_time),
                        ..Default::default()
                    };
                    let thumbnails = core.upload_thumbnails(Path::new(&source_path), &name);
                    let guard =
                        core.transfers
                            .begin(name.clone(), "", TransferDirection::Upload, len);
                    let reader = CountingReader::new(std::io::BufReader::new(file), &guard);
                    match core.rt.block_on(photos.upload_photo_from(
                        &name,
                        &media_type,
                        reader,
                        len as i64,
                        thumbnails,
                        metadata,
                        false,
                    )) {
                        Ok(uid) => {
                            // The photo we just uploaded belongs at the head of the
                            // timeline, and the GUI reloads the gallery the moment this
                            // reply lands — so refresh now rather than leaving it to a
                            // background pass that would land just after that reload.
                            core.invalidate_photos();
                            if let Err(e) = core.refresh_timeline_joined() {
                                warn!(error = %e, "timeline refresh after upload failed");
                            }
                            CtlResponse::Ok {
                                message: format!("uploaded photo with uid {uid}"),
                            }
                        }
                        Err(e) => {
                            // The reply goes to the app's toast only; without
                            // this the journal shows a failure and no reason.
                            warn!(error = %e, "photo upload failed");
                            CtlResponse::error(CoreError::from_api(&e, "upload photo failed"))
                        }
                    }
                }
            }
        }
        // `control` only hands over what `request!` matches.
        _ => CtlResponse::error(CoreError::internal("not a photos request".to_string())),
    }
}
