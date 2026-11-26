//! Files on the drive: mount, pins, listing, search, moves and copies,
//! versions, trash, new folders and uploads.

use super::*;

pub(super) fn cmd_versions(action: VersionCmd) -> Result<()> {
    match action {
        VersionCmd::List { path } => {
            let response = control_request(CtlRequest::ListRevisions {
                path: path_arg(&path)?,
            })?;
            if emit_json(&response)? {
                return Ok(());
            }
            match response {
                CtlResponse::Revisions { items } if items.is_empty() => {
                    println!("No versions.");
                }
                CtlResponse::Revisions { items } => {
                    for r in items {
                        // The claimed plaintext size is what the user recognises;
                        // the storage size is ciphertext and always larger.
                        let size = r
                            .claimed_size
                            .filter(|size| *size >= 0)
                            .unwrap_or(r.size_on_storage)
                            .max(0) as u64;
                        let mark = if r.is_active { "*" } else { " " };
                        let by = r
                            .signed_by
                            .map(|email| format!("  {email}"))
                            .unwrap_or_default();
                        println!(
                            "{mark} {:<20}  {:>10}  {}{by}",
                            r.id,
                            human_bytes(size),
                            format_epoch(r.created),
                        );
                    }
                    println!("(* = current)");
                }
                CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
                other => bail!("unexpected response: {other:?}"),
            }
            Ok(())
        }
        VersionCmd::Restore { path, id } => {
            ok_or_bail(control_request(CtlRequest::RestoreRevision {
                path: path_arg(&path)?,
                revision_id: id,
            })?)
        }
        VersionCmd::Save { path, id, dest } => {
            // The daemon writes the file, so it needs a path that means the same
            // thing in its process as in this one.
            let dest = std::path::absolute(&dest)
                .with_context(|| format!("resolve {}", dest.display()))?;
            ok_or_bail(control_request(CtlRequest::SaveRevisionAs {
                path: path_arg(&path)?,
                revision_id: id,
                dest: dest.to_string_lossy().into_owned(),
            })?)
        }
        VersionCmd::Rm { path, id } => ok_or_bail(control_request(CtlRequest::DeleteRevision {
            path: path_arg(&path)?,
            revision_id: id,
        })?),
    }
}

pub(super) fn cmd_mount(mountpoint: Option<PathBuf>) -> Result<()> {
    pdfs_daemon::mount_once(mountpoint)?;
    Ok(())
}

pub(super) fn cmd_pin(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Pin {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_unpin(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Unpin {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_transfers() -> Result<()> {
    use pdfs_core::control::TransferDirection;
    let response = control_request(CtlRequest::GetQueueStatus)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Transfers { items, jobs } if items.is_empty() && jobs.is_empty() => {
            println!("Nothing in progress.")
        }
        CtlResponse::Transfers { items, jobs } => {
            // Jobs first: they are the context for the transfers under them ("of
            // 40 files", "still scanning") rather than a separate list.
            for j in jobs {
                let count = if j.total > 0 {
                    format!("{} of {}", j.done, j.total)
                } else {
                    "—".to_string()
                };
                let detail = if j.detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", j.detail)
                };
                println!("… {:>9}  {}{detail}", count, j.title);
            }
            for t in items {
                let arrow = match t.direction {
                    TransferDirection::Download => "↓",
                    TransferDirection::Upload => "↑",
                };
                let pct = if t.bytes_total > 0 {
                    format!(
                        "{:.0}%",
                        100.0 * t.bytes_completed as f64 / t.bytes_total as f64
                    )
                } else {
                    "—".to_string()
                };
                let speed = t.speed_bytes_sec as f64 / 1_048_576.0;
                println!(
                    "{arrow} {:>4}  {:>8} / {:>8}  {speed:>6.2} MiB/s  {}",
                    pct, t.bytes_completed, t.bytes_total, t.name
                );
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_pins() -> Result<()> {
    let response = control_request(CtlRequest::ListPins)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Pins { pins } if pins.is_empty() => println!("No pinned files."),
        CtlResponse::Pins { pins } => {
            for p in pins {
                println!("{}", p.path);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_ls(path: Option<PathBuf>) -> Result<()> {
    let path = match path {
        Some(p) => path_arg(&p)?,
        None => String::new(),
    };
    let response = control_request(CtlRequest::ListDir { path })?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Entries { entries } if entries.is_empty() => println!("(empty)"),
        CtlResponse::Entries { entries } => {
            for e in entries {
                let kind = if e.is_dir { "d" } else { "-" };
                let pin = if e.pinned { "*" } else { " " };
                println!("{kind}{pin} {:>12}  {}  [{}]", e.size, e.name, e.uid);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_search(query: String, limit: usize, scope: Option<PathBuf>) -> Result<()> {
    let scope = scope.map(|p| path_arg(&p)).transpose()?;
    match control_request(CtlRequest::Search {
        query,
        limit,
        scope,
    })? {
        CtlResponse::SearchResults { hits } if hits.is_empty() => println!("(no matches)"),
        CtlResponse::SearchResults { hits } => {
            for h in hits {
                let kind = if h.is_dir { "d" } else { "-" };
                let pin = if h.pinned { "*" } else { " " };
                println!("{kind}{pin} {:>12}  {}", h.size, h.path);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_rename(path: PathBuf, new_name: String) -> Result<()> {
    match control_request(CtlRequest::Rename {
        path: path_arg(&path)?,
        new_name,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// Move each of `sources` into `new_parent`, one request each. A refusal names
/// its source and the rest still move; the command fails if any did not.
pub(super) fn cmd_move(sources: Vec<PathBuf>, new_parent: PathBuf) -> Result<()> {
    let new_parent = path_arg(&new_parent)?;
    let mut failed = 0;
    for source in &sources {
        let outcome = path_arg(source).and_then(|path| {
            match control_request(CtlRequest::Move {
                path,
                new_parent: new_parent.clone(),
            })? {
                CtlResponse::Ok { message } => Ok(message),
                CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
                other => bail!("unexpected response: {other:?}"),
            }
        });
        match outcome {
            Ok(message) => println!("{message}"),
            Err(e) if sources.len() == 1 => return Err(e),
            Err(e) => {
                eprintln!("{}: {e:#}", source.display());
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} could not be moved", sources.len());
    }
    Ok(())
}

pub(super) fn cmd_copy(sources: Vec<PathBuf>, new_parent: PathBuf) -> Result<()> {
    let new_parent = path_arg(&new_parent)?;
    let mut failed = 0;
    for source in &sources {
        let outcome = path_arg(source).and_then(|path| {
            match control_request(CtlRequest::Copy {
                path,
                new_parent: new_parent.clone(),
            })? {
                CtlResponse::Ok { message } => Ok(message),
                CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
                other => bail!("unexpected response: {other:?}"),
            }
        });
        match outcome {
            Ok(message) => println!("{message}"),
            Err(e) if sources.len() == 1 => return Err(e),
            Err(e) => {
                eprintln!("{}: {e:#}", source.display());
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} could not be copied", sources.len());
    }
    Ok(())
}

pub(super) fn cmd_du(path: PathBuf) -> Result<()> {
    let response = control_request(CtlRequest::FolderSize {
        path: path_arg(&path)?,
    })?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::FolderSize { size, items } => {
            let noun = if items == 1 { "item" } else { "items" };
            println!(
                "{} in {items} {noun} (on Proton Drive, trash included)",
                human_bytes(size)
            );
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_rm(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Delete {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_trash(by_folder: bool, folder: Option<String>) -> Result<()> {
    let request = match folder {
        Some(uid) => CtlRequest::ListTrashedFolder { uid },
        None => CtlRequest::ListTrash,
    };
    let response = control_request(request)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Entries { entries } if entries.is_empty() => println!("(nothing here)"),
        CtlResponse::Entries { entries } if by_folder => {
            let mut groups: BTreeMap<String, Vec<&pdfs_core::control::DirEntry>> = BTreeMap::new();
            for e in entries.iter().filter(|e| e.trashed_parent.is_none()) {
                let folder = match e.trashed_from.as_deref() {
                    None => "(unknown folder)".to_string(),
                    Some("") => "/".to_string(),
                    Some(path) => format!("/{path}"),
                };
                groups.entry(folder).or_default().push(e);
            }
            for (folder, items) in groups {
                println!("{folder}  ({} item(s))", items.len());
                for e in items {
                    print_trashed(e, "  ");
                }
            }
        }
        CtlResponse::Entries { entries } => {
            for e in &entries {
                print_trashed(e, "");
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn print_trashed(e: &pdfs_core::control::DirEntry, indent: &str) {
    let kind = if e.is_dir { "d" } else { "-" };
    println!("{indent}{kind} {:>12}  {}  [{}]", e.size, e.name, e.uid);
}

pub(super) fn cmd_restore(uids: Vec<String>) -> Result<()> {
    match control_request(CtlRequest::Restore { uids })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_delete_forever(uids: Vec<String>) -> Result<()> {
    confirm(&format!(
        "Permanently delete {} item(s)? This cannot be undone.",
        uids.len()
    ))?;
    match control_request(CtlRequest::DeleteForever { uids })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_empty_trash() -> Result<()> {
    confirm("Permanently delete everything in the trash? This cannot be undone.")?;
    match control_request(CtlRequest::EmptyTrash)? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// Drop a cached listing. `trash` and `photos` name those two listings; anything
/// else is read as a folder path, so a folder actually called "trash" is still
/// reachable as `./trash`.
pub(super) fn cmd_refresh(target: Option<String>, full: bool) -> Result<()> {
    let scope = match target.as_deref() {
        Some("trash") => RefreshScope::Trash,
        Some("photos") => RefreshScope::Photos { full },
        path => RefreshScope::Dir {
            path: path.unwrap_or("").to_string(),
        },
    };
    let photos = matches!(scope, RefreshScope::Photos { .. });
    match control_request(CtlRequest::Refresh { scope })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    if photos {
        wait_for_photos_refresh()?;
    }
    Ok(())
}

/// How often [`wait_for_photos_refresh`] asks the daemon whether the timeline
/// refresh it started is done.
pub(super) const PHOTOS_REFRESH_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Block until the daemon's timeline refresh has finished.
///
/// The daemon answers `Refresh { Photos }` as soon as the refresh is running,
/// because re-reading a large library takes minutes and a control reply may not.
/// `pdfs refresh photos` means "the timeline is up to date when I return", so
/// the waiting happens here, where no socket read bound applies.
pub(super) fn wait_for_photos_refresh() -> Result<()> {
    loop {
        std::thread::sleep(PHOTOS_REFRESH_POLL);
        match control_request(CtlRequest::PhotosRefreshStatus)? {
            CtlResponse::PhotosRefresh { running: true } => continue,
            CtlResponse::PhotosRefresh { running: false } => {
                println!("refreshed");
                return Ok(());
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        }
    }
}

pub(super) fn cmd_mkdir(parent: PathBuf, name: String) -> Result<()> {
    match control_request(CtlRequest::CreateFolder {
        parent: path_arg(&parent)?,
        name,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_upload(sources: Vec<PathBuf>, parent: PathBuf) -> Result<()> {
    // The daemon reads the sources off its own (local) filesystem, so send it
    // absolute, canonicalized paths.
    let mut abs = Vec::with_capacity(sources.len());
    for src in &sources {
        let p = std::fs::canonicalize(src).with_context(|| format!("resolve {}", src.display()))?;
        abs.push(
            p.to_str()
                .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", p.display()))?
                .to_owned(),
        );
    }
    match control_request(CtlRequest::UploadPaths {
        parent: path_arg(&parent)?,
        sources: abs,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}
