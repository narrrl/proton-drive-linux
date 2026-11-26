//! Photos: the timeline, favorites, albums, the Google Photos import and
//! re-dating.

use super::*;

pub(super) fn cmd_photos(limit: usize, offset: usize, favorites: bool) -> Result<()> {
    match control_request(CtlRequest::PhotosTimeline {
        offset,
        limit,
        kind: None,
        range: None,
        favorites,
        not_in_album: false,
    })? {
        CtlResponse::Photos {
            available: false, ..
        } => {
            println!("This account has no photos volume.")
        }
        CtlResponse::Photos { items, .. } if items.is_empty() => println!("No photos."),
        CtlResponse::Photos { items, .. } => print_photo_page(items)?,
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// Print one page of photos as `capture-time  uid  thumbnail-path`.
///
/// A timeline or album reply only carries thumbnails already in the cache, so
/// the rest of the page's are pulled in one batch rather than left blank.
pub(super) fn cmd_favorite(uid: String, remove: bool) -> Result<()> {
    ok_or_bail(control_request(CtlRequest::SetPhotoFavorite {
        uid,
        favorite: !remove,
    })?)
}

pub(super) fn print_photo_page(items: Vec<pdfs_core::control::PhotoItem>) -> Result<()> {
    let uids: Vec<String> = items.iter().map(|p| p.uid.clone()).collect();
    let thumbs: HashMap<String, Option<String>> =
        match control_request(CtlRequest::PhotoThumbs { uids })? {
            CtlResponse::Thumbs { items } => items.into_iter().map(|t| (t.uid, t.path)).collect(),
            _ => HashMap::new(),
        };
    for p in items {
        let thumb = thumbs
            .get(&p.uid)
            .and_then(|p| p.as_deref())
            .or(p.thumb_path.as_deref())
            .unwrap_or("(no thumbnail)");
        let fav = if p.favorite { "*" } else { " " };
        println!("{fav} {}  {}  {thumb}", p.capture_time, p.uid);
    }
    Ok(())
}

pub(super) fn cmd_albums() -> Result<()> {
    match control_request(CtlRequest::PhotoAlbums)? {
        CtlResponse::Albums {
            available: false, ..
        } => println!("This account has no photos volume."),
        CtlResponse::Albums { items, .. } if items.is_empty() => println!("No albums."),
        CtlResponse::Albums { items, .. } => {
            for album in items {
                let shared = if album.shared {
                    " (shared with me)"
                } else {
                    ""
                };
                println!(
                    "{:>6}  {}  {}{shared}",
                    album.photo_count, album.uid, album.name
                );
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_new_album(name: String) -> Result<()> {
    match control_request(CtlRequest::CreateAlbum { name })? {
        CtlResponse::AlbumCreated { uid } => println!("{uid}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// Send an album add or remove and report it per photo; any failed photo
/// fails the command.
pub(super) fn cmd_album_change(request: CtlRequest, verb: &str) -> Result<()> {
    match control_request(request)? {
        CtlResponse::AlbumChanged { changed, failed } => {
            println!("{} {verb}", photo_count(changed.len()));
            for failure in &failed {
                eprintln!("{}: {}", failure.uid, failure.message);
            }
            if !failed.is_empty() {
                bail!("{} failed", photo_count(failed.len()));
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn photo_count(n: usize) -> String {
    if n == 1 {
        "1 photo".to_string()
    } else {
        format!("{n} photos")
    }
}

pub(super) fn cmd_import_google_photos(
    archives: Vec<PathBuf>,
    dry_run: bool,
    resume: bool,
    wait: bool,
) -> Result<()> {
    let request = if resume {
        CtlRequest::ResumeImport
    } else {
        // The daemon opens the archives itself off the shared filesystem, so it
        // gets absolute paths — the same arrangement `pdfs upload` uses.
        let mut absolute = Vec::with_capacity(archives.len());
        for archive in &archives {
            let path = std::fs::canonicalize(archive)
                .with_context(|| format!("resolve {}", archive.display()))?;
            absolute.push(
                path.to_str()
                    .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))?
                    .to_owned(),
            );
        }
        CtlRequest::ImportTakeout {
            archives: absolute,
            dry_run,
        }
    };

    match control_request(request)? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    if !wait {
        println!("Run `pdfs import-status` to follow it.");
        return Ok(());
    }

    // Poll rather than stream: the control protocol is request/response, and an
    // import runs for hours — a held-open socket would be the only long-lived
    // connection the daemon has. A line is printed when the step changes and
    // every few percent of it, not on every poll.
    let mut last_line = String::new();
    let mut last_printed = std::time::Instant::now();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match control_request(CtlRequest::ImportStatus)? {
            CtlResponse::ImportStatus {
                running: true,
                progress,
                ..
            } => {
                let line = progress.as_ref().map(progress_line).unwrap_or_default();
                let step_changed = line.split(':').next() != last_line.split(':').next();
                if !line.is_empty()
                    && line != last_line
                    && (step_changed || last_printed.elapsed().as_secs() >= 30)
                {
                    println!("{line}");
                    last_line = line;
                    last_printed = std::time::Instant::now();
                }
            }
            CtlResponse::ImportStatus {
                summary, resumable, ..
            } => {
                print_import_summary(summary.as_ref(), &resumable);
                return import_outcome(summary.as_ref());
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        }
    }
}

/// One line saying where a running import is.
pub(super) fn progress_line(progress: &pdfs_core::control::ImportProgress) -> String {
    use pdfs_core::control::ImportPhase;
    let step = match progress.phase {
        ImportPhase::Scanning => "Reading archives",
        ImportPhase::Checking => "Checking for photos already on Proton",
        ImportPhase::Uploading => "Uploading",
        ImportPhase::Albums => "Filing photos into albums",
    };
    match (progress.total, &progress.current) {
        (0, _) => format!("{step}…"),
        (total, Some(name)) => format!("{step}: {} of {total} ({name})", progress.done),
        (total, None) => format!("{step}: {} of {total}", progress.done),
    }
}

/// How an import `--wait` followed ended, as the exit status: an error unless
/// it did everything it set out to do, so a script can tell.
pub(super) fn import_outcome(summary: Option<&pdfs_core::control::ImportSummary>) -> Result<()> {
    let Some(summary) = summary else {
        bail!("the import ended without a result; the service may have restarted");
    };
    if let Some(error) = &summary.error {
        bail!("the import failed: {error}");
    }
    if summary.interrupted {
        bail!("the import was interrupted; run `pdfs import-google-photos --resume` to continue");
    }
    if summary.cancelled {
        bail!("the import was cancelled");
    }
    if summary.failed > 0 || !summary.unreadable_archives.is_empty() {
        bail!(
            "the import finished, but {} photo(s) and {} archive(s) failed",
            summary.failed,
            summary.unreadable_archives.len()
        );
    }
    Ok(())
}

pub(super) fn cmd_redate_photos(
    dry_run: bool,
    limit: Option<usize>,
    from: Option<String>,
    to: Option<String>,
    wait: bool,
) -> Result<()> {
    let range = match (from.as_deref(), to.as_deref()) {
        (None, None) => None,
        (from, to) => Some((
            from.map(parse_day).transpose()?.unwrap_or(i64::MIN),
            to.map(parse_day).transpose()?.unwrap_or(i64::MAX),
        )),
    };
    match control_request(CtlRequest::RedatePhotos {
        dry_run,
        limit,
        range,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    if !wait {
        println!("Run `pdfs redate-status` to follow it.");
        return Ok(());
    }
    // Polled for the same reason the import is: request/response protocol, and a
    // run over a whole library is a download plus an upload per photo.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match control_request(CtlRequest::RedateStatus)? {
            CtlResponse::RedateStatus { running: true, .. } => continue,
            CtlResponse::RedateStatus { summary, .. } => {
                print_redate_summary(summary.as_ref());
                return finished_run(
                    "the re-date",
                    summary.map(|s| (s.failed, s.cancelled, s.error)),
                );
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        }
    }
}

/// How a run `--wait` followed ended, as the exit status: an error when it
/// failed, was cancelled, or left photos failed, so a script can tell.
/// `outcome` is `(failed, cancelled, error)`; `None` when the daemon has no
/// result, which happens when it restarted during the run.
pub(super) fn finished_run(
    what: &str,
    outcome: Option<(usize, bool, Option<String>)>,
) -> Result<()> {
    match outcome {
        None => bail!("{what} ended without a result; the service may have restarted"),
        Some((_, _, Some(error))) => bail!("{what} failed: {error}"),
        Some((_, true, None)) => bail!("{what} was cancelled"),
        Some((failed, false, None)) if failed > 0 => {
            bail!("{what} finished, but {failed} photo(s) failed")
        }
        Some(_) => Ok(()),
    }
}

/// A `YYYY-MM-DD` day as epoch seconds at UTC midnight. Reuses the Takeout
/// scanner's date reader, which already accepts exactly this spelling.
pub(super) fn parse_day(day: &str) -> Result<i64> {
    pdfs_core::takeout::capture_time_from_name(day)
        .ok_or_else(|| anyhow!("not a date (expected YYYY-MM-DD): {day}"))
}

pub(super) fn cmd_redate_status() -> Result<()> {
    match control_request(CtlRequest::RedateStatus)? {
        CtlResponse::RedateStatus { running, summary } => {
            if running {
                println!("Re-date running. `pdfs transfers` shows its progress.");
            }
            print_redate_summary(summary.as_ref());
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_cancel_redate() -> Result<()> {
    match control_request(CtlRequest::CancelRedate)? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn print_redate_summary(summary: Option<&pdfs_core::control::RedateSummary>) {
    let Some(summary) = summary else {
        println!("No re-date has run yet.");
        return;
    };
    if let Some(error) = &summary.error {
        println!("Failed           : {error}");
        return;
    }
    println!("Photos examined  : {}", summary.examined);
    println!("Mis-dated        : {}", summary.candidates);
    if summary.dry_run {
        println!("(dry run — nothing was uploaded or trashed)");
    } else {
        println!("Re-dated         : {}", summary.redated);
        if summary.album_links > 0 {
            println!("Albums refiled   : {}", summary.album_links);
        }
        if summary.failed > 0 {
            println!("Failed           : {}", summary.failed);
        }
        if summary.bytes > 0 {
            println!("Uploaded bytes   : {}", summary.bytes);
        }
    }
    for (name, stored, wanted) in &summary.samples {
        // Epoch seconds, as `pdfs photos` prints capture times.
        println!("  {name}: {stored} -> {wanted}");
    }
    if summary.candidates > summary.samples.len() {
        println!(
            "  … and {} more",
            summary.candidates - summary.samples.len()
        );
    }
    if summary.cancelled {
        println!("Run was cancelled before it finished.");
    }
}

pub(super) fn cmd_import_status() -> Result<()> {
    match control_request(CtlRequest::ImportStatus)? {
        CtlResponse::ImportStatus {
            running,
            summary,
            progress,
            resumable,
        } => {
            if running {
                match progress.as_ref() {
                    Some(progress) => println!("Import running. {}", progress_line(progress)),
                    None => println!("Import running."),
                }
            }
            print_import_summary(summary.as_ref(), &resumable);
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_cancel_import() -> Result<()> {
    match control_request(CtlRequest::CancelImport)? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn print_import_summary(
    summary: Option<&pdfs_core::control::ImportSummary>,
    resumable: &[String],
) {
    let Some(summary) = summary else {
        println!("No import has run yet.");
        return;
    };
    if summary.dry_run {
        println!("Scan only; nothing was uploaded.");
    }
    if let Some(error) = &summary.error {
        println!("Stopped by       : {error}");
    }
    println!("Photos in export : {}", summary.found);
    if summary.dry_run {
        println!("Would upload     : {}", summary.to_upload);
        println!("Bytes to upload  : {}", summary.to_upload_bytes);
    } else {
        println!("Uploaded         : {}", summary.uploaded);
    }
    println!("Already on Proton: {}", summary.duplicates);
    if summary.live_photos > 0 {
        println!("Live photos      : {}", summary.live_photos);
    }
    if summary.skipped_trashed > 0 {
        println!("Skipped (trash)  : {}", summary.skipped_trashed);
    }
    if summary.albums_created > 0 || summary.album_links > 0 {
        println!(
            "Albums           : {} created, {} photos filed",
            summary.albums_created, summary.album_links
        );
    }
    if summary.failed > 0 {
        println!("Failed           : {}", summary.failed);
    }
    for archive in &summary.unreadable_archives {
        println!("Unreadable       : {archive}");
    }
    if summary.bytes > 0 {
        println!("Uploaded bytes   : {}", summary.bytes);
    }
    if summary.interrupted {
        println!("The service stopped before the import finished.");
    } else if summary.cancelled {
        println!("Import was cancelled before it finished.");
    }
    if !resumable.is_empty() {
        println!("Run `pdfs import-google-photos --resume` to continue it.");
    }
}

pub(super) fn cmd_album(uid: String, limit: usize, offset: usize) -> Result<()> {
    match control_request(CtlRequest::AlbumPhotos { uid, offset, limit })? {
        CtlResponse::Photos { items, .. } if items.is_empty() => println!("No photos."),
        CtlResponse::Photos { items, .. } => print_photo_page(items)?,
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_open_photo(uid: String) -> Result<()> {
    match control_request(CtlRequest::OpenPhoto { uid })? {
        CtlResponse::FilePath { path } => println!("{path}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}
