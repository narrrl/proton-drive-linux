//! `pdfs sync`: synced folders, their queue, issues, limits and ignore rules,
//! and `pdfs conflicts`.

use super::*;

pub(super) fn cmd_sync(action: SyncCmd) -> Result<()> {
    match action {
        SyncCmd::Add { path } => {
            let abs = std::fs::canonicalize(&path)
                .with_context(|| format!("resolve {}", path.display()))?;
            let local_path = abs
                .to_str()
                .ok_or_else(|| anyhow!("path is not valid UTF-8"))?
                .to_string();
            ok_or_bail(control_request(CtlRequest::AddSyncFolder { local_path })?)?
        }
        SyncCmd::Attach { path, to } => {
            let abs = std::fs::canonicalize(&path)
                .with_context(|| format!("resolve {}", path.display()))?;
            let local_path = abs
                .to_str()
                .ok_or_else(|| anyhow!("path is not valid UTF-8"))?
                .to_owned();
            let remote_path = to
                .to_str()
                .ok_or_else(|| anyhow!("remote path is not valid UTF-8"))?
                .to_owned();
            ok_or_bail(control_request(CtlRequest::AttachSyncFolder {
                local_path,
                remote_path,
            })?)?
        }
        SyncCmd::List => {
            let response = control_request(CtlRequest::ListSyncFolders)?;
            if emit_json(&response)? {
                return Ok(());
            }
            match response {
                CtlResponse::SyncFolders { items } if items.is_empty() => {
                    println!("No synced folders.")
                }
                CtlResponse::SyncFolders { items } => {
                    for f in items {
                        let sync = last_synced(Some(f.last_sync));
                        // A queued switch is reported as the mode the folder is moving
                        // to, so `sync list` never reads as if the request was dropped.
                        let mode = match &f.pending_mode {
                            Some(pending) => format!("{} → {pending}", f.mode),
                            None => f.mode.clone(),
                        };
                        let mode = match f.paused {
                            true => format!("{mode}, paused"),
                            false => mode,
                        };
                        println!(
                            "[{}]  {}  ({mode}, {}, synced: {sync})",
                            f.id, f.local_path, f.state
                        );
                        // A pass in flight says what it is doing, indented under it.
                        if let Some(p) = &f.progress {
                            match p.phase {
                                SyncPhase::Scanning if p.total == 0 && p.done == 0 => {
                                    println!("      scanning…")
                                }
                                // A first pass has no estimate yet; the count alone
                                // shows a long first scan is moving.
                                SyncPhase::Scanning if p.total == 0 => {
                                    println!("      scanning: {} checked", p.done)
                                }
                                SyncPhase::Scanning => {
                                    println!(
                                        "      scanning: {} of {}",
                                        p.done,
                                        p.total.max(p.done)
                                    )
                                }
                                SyncPhase::Applying => println!(
                                    "      {} of {}  {}",
                                    p.done + 1,
                                    p.total.max(p.done + 1),
                                    p.current
                                ),
                            }
                        }
                    }
                }
                CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
                other => bail!("unexpected response: {other:?}"),
            }
        }
        SyncCmd::Rm { id, delete_remote } => {
            ok_or_bail(control_request(CtlRequest::RemoveSyncFolder {
                id,
                delete_remote,
            })?)?
        }
        SyncCmd::Now { id } => ok_or_bail(control_request(CtlRequest::SyncNow { id })?)?,
        SyncCmd::Reset { id } => ok_or_bail(control_request(CtlRequest::ResetSyncFolder { id })?)?,
        SyncCmd::Pause {
            folder: Some(id), ..
        } => ok_or_bail(control_request(CtlRequest::SetSyncFolderPaused {
            id,
            paused: true,
        })?)?,
        SyncCmd::Resume { folder: Some(id) } => {
            ok_or_bail(control_request(CtlRequest::SetSyncFolderPaused {
                id,
                paused: false,
            })?)?
        }
        SyncCmd::Pause {
            duration,
            folder: None,
        } => {
            let until = duration.map(|secs| unix_now().saturating_add(secs as i64));
            ok_or_bail(control_request(CtlRequest::SetSyncPaused {
                paused: true,
                until,
            })?)?
        }
        SyncCmd::Queue => return cmd_sync_queue(),
        SyncCmd::Flush { timeout } => return cmd_sync_flush(timeout),
        SyncCmd::Issues => return cmd_sync_issues(),
        SyncCmd::Export { id, dest } => return cmd_sync_export(id, &dest),
        SyncCmd::Limit { up, down } => return cmd_sync_limit(up, down),
        SyncCmd::Ignore { action } => return cmd_sync_ignore(action),
        SyncCmd::Discard { id, yes } => {
            if !yes {
                confirm(&format!(
                    "Discard queued change {id} and undo it here? Content not exported is lost."
                ))?;
            }
            ok_or_bail(control_request(CtlRequest::DiscardPendingOp { id })?)?
        }
        SyncCmd::Retry { id } => ok_or_bail(control_request(CtlRequest::RetryPendingOp { id })?)?,
        SyncCmd::Resume { folder: None } => {
            ok_or_bail(control_request(CtlRequest::SetSyncPaused {
                paused: false,
                until: None,
            })?)?
        }
        SyncCmd::Mode { id, mode } => {
            ok_or_bail(control_request(CtlRequest::SetSyncFolderMode { id, mode })?)?
        }
        SyncCmd::Restore { yes, device } => return cmd_sync_restore(yes, device),
    }
    Ok(())
}

/// `pdfs conflicts`: list conflict copies, or resolve one.
pub(super) fn cmd_conflicts(action: Option<ConflictCmd>) -> Result<()> {
    if let Some(ConflictCmd::Resolve { path, keep, name }) = action {
        let keep = match keep.as_str() {
            "original" => ConflictKeep::Original,
            "copy" => ConflictKeep::Copy,
            _ => ConflictKeep::Both {
                name: name.unwrap_or_default(),
            },
        };
        let path = path_arg(&path)?;
        return ok_or_bail(control_request(CtlRequest::ResolveConflict { path, keep })?);
    }
    let response = control_request(CtlRequest::ListConflicts)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Conflicts { items } if items.is_empty() => println!("No conflicts."),
        CtlResponse::Conflicts { items } => {
            for c in items {
                println!("{}", c.path);
                let verdict = match (c.original_exists, c.identical) {
                    (false, _) => "original is gone".to_string(),
                    (true, true) => format!("identical to {}", c.original_path),
                    (true, false) => format!(
                        "differs from {} ({} vs {})",
                        c.original_path,
                        human_bytes(c.size),
                        human_bytes(c.original_size.unwrap_or_default()),
                    ),
                };
                println!("      {verdict}");
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// `pdfs sync queue`: what has not reached Proton Drive yet, and why.
pub(super) fn cmd_sync_queue() -> Result<()> {
    let response = control_request(CtlRequest::ListPendingOps)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::PendingOps { items } if items.is_empty() => {
            println!("Nothing queued: everything is on Proton Drive.");
        }
        CtlResponse::PendingOps { items } => {
            let now = unix_now();
            for op in items {
                let state = match (op.parked, op.next_attempt_at) {
                    (true, _) => "waiting for its final name".to_string(),
                    (false, Some(at)) if at > now => {
                        format!("next try in {}", human_age((at - now) as u64))
                    }
                    (false, _) => "due now".to_string(),
                };
                println!("[{}]  {}  {}  ({state})", op.id, op.kind, op.path);
                if let Some(issue) = op.issue {
                    println!("      needs attention: {}", issue_text(issue));
                }
                if let Some(error) = &op.last_error {
                    println!("      failed {} time(s): {error}", op.attempts);
                }
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// `pdfs sync flush`: wait for the queue to drain.
///
/// Failed changes are retried at once rather than after their backoff. The
/// wait ends early, with exit code 3, when everything left is waiting on the
/// user: refused by Drive, failing again and again, or a temporary file
/// waiting for its final name. Paused or offline, it keeps waiting and says
/// why.
pub(super) fn cmd_sync_flush(timeout: Option<u64>) -> Result<()> {
    const POLL: std::time::Duration = std::time::Duration::from_secs(1);
    let deadline =
        timeout.map(|secs| std::time::Instant::now() + std::time::Duration::from_secs(secs));
    ok_or_bail(control_request(CtlRequest::RetryPendingOp { id: None })?)?;
    let mut last_line = String::new();
    loop {
        let items = match control_request(CtlRequest::ListPendingOps)? {
            CtlResponse::PendingOps { items } => items,
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        };
        let waiting = items
            .iter()
            .filter(|op| !op.parked && !op.failing && op.issue.is_none())
            .count();
        if items.is_empty() {
            finish_line(&last_line);
            println!("Everything is on Proton Drive.");
            return Ok(());
        }
        if waiting == 0 {
            finish_line(&last_line);
            eprintln!(
                "{} queued change(s) need you before they can be sent. See `pdfs sync issues`.",
                items.len()
            );
            std::process::exit(3);
        }
        let reason = match control_request(CtlRequest::Status)? {
            CtlResponse::Status { paused: true, .. } => " (sync is paused; `pdfs sync resume`)",
            CtlResponse::Status { online: false, .. } => " (offline; waiting for Proton Drive)",
            _ => "",
        };
        let line = format!("{waiting} change(s) left to send{reason}");
        if line != last_line {
            if !last_line.is_empty() {
                print!("\r\x1b[2K");
            }
            print!("{line}");
            last_line = line;
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            finish_line(&last_line);
            eprintln!("Timed out with {waiting} change(s) still queued.");
            std::process::exit(4);
        }
        std::thread::sleep(POLL);
    }
}

/// End a progress line so the next message starts on its own.
pub(super) fn finish_line(last_line: &str) {
    if !last_line.is_empty() {
        println!();
    }
}

/// `pdfs sync issues`: the queued changes that need the user, and what to do.
pub(super) fn cmd_sync_issues() -> Result<()> {
    let response = control_request(CtlRequest::ListPendingOps)?;
    let items = match response {
        CtlResponse::PendingOps { items } => items,
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    };
    let issues: Vec<_> = items.into_iter().filter(|op| op.failing).collect();
    if json_enabled() {
        println!("{}", serde_json::to_string_pretty(&issues)?);
        return Ok(());
    }
    if issues.is_empty() {
        println!("No sync issues.");
        return Ok(());
    }
    for op in issues {
        println!("[{}]  {}  {}", op.id, op.kind, op.path);
        match op.issue {
            Some(issue) => println!("      {}", issue_text(issue)),
            None => println!("      keeps failing; it is retried."),
        }
        if let Some(error) = &op.last_error {
            println!("      last error: {error}");
        }
        if op.exportable {
            println!(
                "      save a copy: pdfs sync export {} <file or folder>",
                op.id
            );
        }
    }
    Ok(())
}

/// What a sync issue means, and what the user can do about it.
pub(super) fn issue_text(issue: SyncIssue) -> &'static str {
    match issue {
        SyncIssue::Quota => {
            "Proton Drive storage is full. Free up space or upgrade; the change is retried."
        }
        SyncIssue::Access => {
            "you can no longer change this on Proton Drive, for example because a share was \
             revoked. Save a copy to keep the content."
        }
        SyncIssue::Trashed => "restore the item or folder from Trash, then retry the move.",
        SyncIssue::Missing => {
            "the file or its folder is gone from Proton Drive. Save a copy to keep the content."
        }
        SyncIssue::Limit => {
            "the folder holds too many items or is nested too deep. Move the file to another \
             folder."
        }
        SyncIssue::Rejected => {
            "Proton Drive rejected the change, for example the name. Rename the file."
        }
        SyncIssue::Unknown => "Proton Drive refused the change.",
    }
}

/// `pdfs sync export`: save a queued upload's content. A folder as `dest`
/// gets the file under its own name.
pub(super) fn cmd_sync_export(id: i64, dest: &Path) -> Result<()> {
    let mut dest =
        std::path::absolute(dest).with_context(|| format!("resolving {}", dest.display()))?;
    if dest.is_dir() {
        let items = match control_request(CtlRequest::ListPendingOps)? {
            CtlResponse::PendingOps { items } => items,
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        };
        let Some(op) = items.iter().find(|op| op.id == id) else {
            bail!("no queued change {id}; see `pdfs sync queue`");
        };
        let name = Path::new(&op.path)
            .file_name()
            .with_context(|| format!("{} has no file name", op.path))?;
        dest.push(name);
    }
    ok_or_bail(control_request(CtlRequest::ExportPendingOp {
        id,
        dest: dest.display().to_string(),
    })?)
}

/// `pdfs sync restore`: list what this machine's device holds, confirm where
/// each folder should land, then hand the mapping to the daemon.
///
/// The daemon only ever *proposes* local paths — a path recorded by another
/// machine may name a home directory that does not exist here — so the default
/// flow is propose-and-confirm. `--yes` accepts the proposals for scripting.
pub(super) fn cmd_sync_restore(yes: bool, device: Option<String>) -> Result<()> {
    let response = control_request(match &device {
        Some(device) => CtlRequest::ListDeviceRestorableFolders {
            device: device.clone(),
        },
        None => CtlRequest::ListRestorableFolders,
    })?;
    if emit_json(&response)? {
        return Ok(());
    }
    let items = match response {
        CtlResponse::RestorableFolders { items } => items,
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    };
    let candidates: Vec<_> = items.into_iter().filter(|f| !f.already_synced).collect();
    if candidates.is_empty() {
        println!("Nothing to restore: this device has no folders that aren't already synced here.");
        return Ok(());
    }

    let mut chosen: Vec<CtlRestoreItem> = Vec::new();
    for f in candidates {
        if yes {
            println!("{}  ->  {}  [{}]", f.name, f.local_path, f.mode);
            chosen.push(CtlRestoreItem {
                remote_uid: f.remote_uid,
                local_path: f.local_path,
                mode: f.mode,
            });
            continue;
        }
        // Empty answer takes the proposal, `n` skips, anything else is read as
        // the path to use instead — the three things a user wants here.
        let answer = prompt_line(&format!(
            "{} [{}] -> {} (Enter to accept, 'n' to skip, or a path)",
            f.name, f.mode, f.local_path
        ))?;
        match answer.as_str() {
            "n" | "N" => continue,
            "" => chosen.push(CtlRestoreItem {
                remote_uid: f.remote_uid,
                local_path: f.local_path,
                mode: f.mode,
            }),
            path => chosen.push(CtlRestoreItem {
                remote_uid: f.remote_uid,
                local_path: expand_tilde(path),
                mode: f.mode,
            }),
        }
    }
    if chosen.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }
    ok_or_bail(control_request(match device {
        Some(device) => CtlRequest::RestoreDeviceFolders {
            device,
            items: chosen,
        },
        None => CtlRequest::RestoreSyncFolders { items: chosen },
    })?)?;
    // The daemon acks before it works, so the outcome, and any folder it had
    // to skip, only shows up later.
    println!("Run `pdfs activity` to see which folders were restored.");
    Ok(())
}

/// Expand a leading `~` in a hand-typed path. The daemon requires an absolute
/// path, and a shell is not always the one doing the typing here.
pub(super) fn expand_tilde(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home)
                .join(rest)
                .to_string_lossy()
                .into_owned(),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

pub(super) fn cmd_share(
    path: PathBuf,
    emails: Vec<String>,
    role: String,
    message: Option<String>,
) -> Result<()> {
    ok_or_bail(control_request(CtlRequest::ShareNode {
        path: path_arg(&path)?,
        emails,
        role,
        message,
    })?)
}

/// Format an age in seconds for a report line, coarsely — the point of the
/// number is whether it is minutes or weeks.
/// Parse a `sync pause --for` duration: a whole number followed by `m`, `h` or
/// `d`. Returns seconds.
pub(super) fn parse_pause_duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text.char_indices().last().map_or(0, |(at, _)| at);
    let (number, unit) = text.split_at(split);
    let per_unit: u64 = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err("use a number followed by m, h or d, e.g. 30m".to_string()),
    };
    match number.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n.saturating_mul(per_unit)),
        _ => Err("use a positive whole number, e.g. 30m".to_string()),
    }
}

/// Read a bytes-per-second rate: a whole number with an optional `K`, `M` or
/// `G` suffix in the same binary units `human_bytes` prints.
pub(super) fn parse_rate(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (number, scale) = match text.char_indices().last() {
        Some((at, 'K' | 'k')) => (&text[..at], 1024),
        Some((at, 'M' | 'm')) => (&text[..at], 1_048_576),
        Some((at, 'G' | 'g')) => (&text[..at], 1_073_741_824),
        _ => (text, 1),
    };
    number
        .parse::<u64>()
        .map(|n| n.saturating_mul(scale))
        .map_err(|_| "use a whole number with an optional K, M or G, e.g. 2M".to_string())
}

pub(super) fn human_rate(rate: u64) -> String {
    if rate == 0 {
        "unlimited".to_string()
    } else {
        format!("{}/s", human_bytes(rate))
    }
}

/// `pdfs sync limit`: set the bandwidth caps, keeping whichever one is not
/// given, or print both when neither is.
pub(super) fn cmd_sync_limit(up: Option<u64>, down: Option<u64>) -> Result<()> {
    let config = AppDirs::new()?.load_config();
    let upload = up.or(config.upload_limit).unwrap_or(0);
    let download = down.or(config.download_limit).unwrap_or(0);
    if up.is_none() && down.is_none() {
        println!("Upload     {}", human_rate(upload));
        println!("Download   {}", human_rate(download));
        return Ok(());
    }
    ok_or_bail(control_request(CtlRequest::SetBandwidthLimits {
        upload,
        download,
    })?)
}

pub(super) fn cmd_sync_ignore(action: IgnoreCmd) -> Result<()> {
    use pdfs_core::syncignore::{
        DEFAULT_IGNORE_PATTERNS, FolderRules, IgnoreRules, RuleSource, Verdict, add_folder_pattern,
        check_pattern, remove_folder_pattern,
    };
    let dirs = AppDirs::new()?;
    let change = |edit: &dyn Fn(&mut Vec<String>)| -> Result<()> {
        dirs.change_ignore_patterns(edit)?;
        // The next pass picks the change up anyway; this only hurries it.
        let _ = control_request(CtlRequest::SyncNow { id: None });
        Ok(())
    };
    let valid = |pattern: &str| -> Result<String> {
        let pattern = pattern.trim();
        if pattern.is_empty() || pattern.starts_with('#') {
            bail!("not a pattern: `{pattern}`");
        }
        check_pattern(pattern).map_err(|e| anyhow!("not a valid pattern: {e}"))?;
        Ok(pattern.to_string())
    };
    match action {
        IgnoreCmd::List { folder: None } => {
            let config = dirs.load_config();
            let list = config.resolved_ignore_patterns();
            if list.is_empty() {
                println!("No patterns: every file is synced.");
            }
            for pattern in list {
                if DEFAULT_IGNORE_PATTERNS.contains(&pattern.as_str()) {
                    println!("{pattern}  (built in)");
                } else {
                    println!("{pattern}");
                }
            }
        }
        IgnoreCmd::List {
            folder: Some(folder),
        } => {
            let (_, root) = mirror_folder_of(&folder)?;
            let rules = FolderRules::read(&root)?;
            let patterns = rules.patterns();
            if patterns.is_empty() {
                println!(
                    "{} has no own patterns; the ones for all synced folders apply.",
                    root.display()
                );
            }
            for pattern in patterns {
                println!("{pattern}");
            }
        }
        IgnoreCmd::Add {
            pattern,
            folder: None,
        } => {
            let pattern = valid(&pattern)?;
            change(&|list| {
                if !list.contains(&pattern) {
                    list.push(pattern.clone());
                }
            })?;
        }
        IgnoreCmd::Add {
            pattern,
            folder: Some(folder),
        } => {
            let pattern = valid(&pattern)?;
            let (id, root) = mirror_folder_of(&folder)?;
            if add_folder_pattern(&root, &pattern)? {
                let _ = control_request(CtlRequest::SyncNow { id: Some(id) });
            } else {
                println!("`{pattern}` is already in {}/.pdfsignore.", root.display());
            }
        }
        IgnoreCmd::Remove {
            pattern,
            folder: None,
        } => {
            let pattern = pattern.trim().to_string();
            let list = dirs.load_config().resolved_ignore_patterns();
            if !list.contains(&pattern) {
                bail!("`{pattern}` is not in the list; see `pdfs sync ignore list`");
            }
            change(&|list| list.retain(|p| *p != pattern))?;
        }
        IgnoreCmd::Remove {
            pattern,
            folder: Some(folder),
        } => {
            let (id, root) = mirror_folder_of(&folder)?;
            if !remove_folder_pattern(&root, &pattern)? {
                bail!(
                    "`{}` is not among the own patterns of {}; see `pdfs sync ignore list --folder {}`",
                    pattern.trim(),
                    root.display(),
                    root.display()
                );
            }
            let _ = control_request(CtlRequest::SyncNow { id: Some(id) });
        }
        IgnoreCmd::Reset => change(&|list| {
            *list = DEFAULT_IGNORE_PATTERNS
                .iter()
                .map(|p| (*p).to_string())
                .collect()
        })?,
        IgnoreCmd::Check { path } => {
            let path = std::path::absolute(&path)?;
            let folder = synced_folder_of(&path)?;
            if folder.mode != "mirror" {
                println!(
                    "{} is online only; patterns apply only to folders kept on this computer.",
                    folder.local_path
                );
                return Ok(());
            }
            let root = PathBuf::from(&folder.local_path);
            let rel = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy();
            let globals = dirs.load_config().resolved_ignore_patterns();
            let rules = IgnoreRules::load(&root, &globals);
            let source = |source: &RuleSource| match source {
                RuleSource::AllFolders => "the patterns for all synced folders".to_string(),
                RuleSource::ThisFolder => format!("{}/.pdfsignore", root.display()),
            };
            match rules.explain(&rel, path.is_dir()) {
                Verdict::Synced => println!("Synced: no pattern matches it."),
                Verdict::Excluded { pattern, source: s } => {
                    println!("Not synced: excluded by `{pattern}` in {}.", source(&s))
                }
                Verdict::Included { pattern, source: s } => {
                    println!("Synced: `{pattern}` in {} includes it again.", source(&s))
                }
            }
        }
    }
    Ok(())
}

/// The synced folder holding `path`: the innermost one, as the daemon lists
/// them.
pub(super) fn synced_folder_of(path: &Path) -> Result<SyncFolderInfo> {
    let path = std::path::absolute(path)?;
    let folders = match control_request(CtlRequest::ListSyncFolders)? {
        CtlResponse::SyncFolders { items } => items,
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected reply: {other:?}"),
    };
    folders
        .into_iter()
        .filter(|f| path.starts_with(&f.local_path))
        .max_by_key(|f| f.local_path.len())
        .ok_or_else(|| anyhow!("{} is not in a synced folder", path.display()))
}

/// The id and root of the synced folder kept on this computer that holds
/// `path`. Own patterns mean nothing to an online-only folder.
pub(super) fn mirror_folder_of(path: &Path) -> Result<(i64, PathBuf)> {
    let folder = synced_folder_of(path)?;
    if folder.mode != "mirror" {
        bail!(
            "{} is online only; patterns apply only to folders kept on this computer",
            folder.local_path
        );
    }
    Ok((folder.id, PathBuf::from(folder.local_path)))
}

/// How long the oldest write may wait to upload before `diagnose` calls it
/// out. Under a day it is a slow link or a big file; past it, something is
/// holding the queue.
pub(super) const STAGED_STALE_SECS: u64 = 86_400;

/// `diagnose` findings for the queue and the pause, from the `Status` reply.
/// `paused_for` is how long a timed pause still runs, in seconds.
pub(super) fn sync_health(
    failing_ops: u64,
    failing_error: Option<&str>,
    staged_bytes: u64,
    staged_oldest_secs: u64,
    paused: bool,
    paused_for: Option<i64>,
) -> Vec<(DiagnoseLevel, &'static str, String)> {
    let mut findings = Vec::new();
    if failing_ops > 0 {
        findings.push((
            DiagnoseLevel::Warn,
            "  failing writes",
            match failing_error {
                Some(error) => format!("{failing_ops} keep failing; the last error: {error}"),
                None => format!("{failing_ops} keep failing"),
            },
        ));
    }
    if staged_bytes > 0 && staged_oldest_secs >= STAGED_STALE_SECS {
        findings.push((
            DiagnoseLevel::Warn,
            "  oldest unsent write",
            format!(
                "{} old, {} only on this computer",
                human_secs(staged_oldest_secs),
                human_bytes(staged_bytes)
            ),
        ));
    }
    if paused {
        findings.push((
            DiagnoseLevel::Warn,
            "  sync",
            match paused_for {
                Some(left) if left > 0 => {
                    format!("paused for another {}", human_secs(left as u64))
                }
                _ => "paused until resumed (`pdfs sync resume`)".to_string(),
            },
        ));
    }
    findings
}

/// `diagnose` findings for the synced folders that need someone to look.
pub(super) fn folder_health(
    folders: &[pdfs_core::control::SyncFolderInfo],
) -> Vec<(DiagnoseLevel, String, String)> {
    folders
        .iter()
        .filter_map(|folder| {
            let what = match folder.state.as_str() {
                "error" => "sync error; `pdfs sync list` and the activity feed say more",
                "conflict" => "conflict copies to look at",
                _ => return None,
            };
            Some((
                DiagnoseLevel::Warn,
                format!("  synced folder [{}]", folder.id),
                format!("{}: {what}", folder.local_path),
            ))
        })
        .collect()
}
