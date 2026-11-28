//! What the daemon reports: status, locations, devices, diagnostics and the
//! content cache.

use super::*;

pub(super) fn cmd_devices(action: DeviceCmd) -> Result<()> {
    match action {
        DeviceCmd::List => {
            let response = control_request(CtlRequest::ListDevices)?;
            if emit_json(&response)? {
                return Ok(());
            }
            match response {
                CtlResponse::Devices { items } if items.is_empty() => println!("No devices."),
                CtlResponse::Devices { items } => {
                    for d in items {
                        let sync = last_synced(d.last_sync);
                        let tag = match (d.adopted, d.this_device) {
                            (true, _) => "  *adopted*",
                            (false, true) => "  *this machine*",
                            _ => "",
                        };
                        println!(
                            "{}  {}  (synced: {sync})  [{}]{tag}",
                            d.device_type, d.name, d.uid
                        );
                    }
                }
                CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
                other => bail!("unexpected response: {other:?}"),
            }
        }
        DeviceCmd::Rename { uid, name } => {
            ok_or_bail(control_request(CtlRequest::RenameDevice { uid, name })?)?
        }
        DeviceCmd::Rm { uid } => ok_or_bail(control_request(CtlRequest::DeleteDevice { uid })?)?,
        DeviceCmd::Adopt { uid, clear } => {
            if uid.is_none() && !clear {
                bail!("give a device uid (see `pdfs devices list`), or --clear");
            }
            let uid = if clear { None } else { uid };
            ok_or_bail(control_request(CtlRequest::AdoptDevice { uid })?)?
        }
        DeviceCmd::Join { uid, yes } => {
            if !yes {
                println!(
                    "This computer syncs into that device alongside it. Its folders are kept\n\
                     in step on both computers: an edit or a deletion on one reaches the other.\n\
                     New synced folders here go under that device too. Folders already synced\n\
                     here stay where they are."
                );
                confirm("Sync with it?")?;
            }
            let response = control_request(CtlRequest::AdoptDevice { uid: Some(uid) })?;
            // A script gets the one reply and runs `sync restore` itself.
            if emit_json(&response)? {
                return Ok(());
            }
            ok_or_bail(response)?;
            // The device is this machine's now, so the plain restore offers its
            // folders.
            cmd_sync_restore(yes, None)?
        }
        DeviceCmd::Migrate { uid, yes } => {
            if !yes {
                println!(
                    "Every folder that device backs up moves into this computer's backup.\n\
                     The device stays registered, without those folders. Stop Proton Drive\n\
                     on that computer first, or it sees its folders disappear."
                );
                confirm("Move them here?")?;
            }
            let response = control_request(CtlRequest::MigrateDevice { uid })?;
            // A script gets the one reply and runs `sync restore` itself.
            if emit_json(&response)? {
                return Ok(());
            }
            ok_or_bail(response)?;
            // The folders are this device's now, so the plain restore offers them.
            cmd_sync_restore(yes, None)?
        }
    }
    Ok(())
}

pub(super) fn cmd_status() -> Result<()> {
    // `status` is the one query that merges two sources — the local keyring and
    // the daemon — so its JSON is composed here rather than being a daemon reply
    // passed through. A script asking for status wants both halves in one
    // object, including the "logged in but no daemon" case that has no reply at
    // all.
    if json_enabled() {
        let session = match auth::load() {
            Ok(s) => Some(s),
            Err(pdfs_core::Error::NotLoggedIn) => None,
            Err(e) => return Err(e.into()),
        };
        let mount = match control_request(CtlRequest::Status) {
            Ok(r @ CtlResponse::Status { .. }) => response_payload(&r)?,
            _ => serde_json::Value::Null,
        };
        let out = serde_json::json!({
            "logged_in": session.is_some(),
            "username": session.as_ref().map(|s| s.username.clone()),
            "user_id": session.as_ref().map(|s| s.user_id.clone()),
            "scopes": session.as_ref().map(|s| s.scopes.clone()),
            "keys": session.as_ref().map(|_| key_state_name()),
            "mount": mount,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    match auth::load() {
        Ok(s) => {
            println!("Logged in as {} (user {})", s.username, s.user_id);
            println!("Scopes: {}", s.scopes.join(", "));
            match key_state_name() {
                "locked" => println!("Keys: locked — run `pdfs unlock`"),
                "session" => println!("Keys: unlocked for this login session"),
                _ => {}
            }
        }
        Err(pdfs_core::Error::NotLoggedIn) => {
            println!("Not logged in. Run `pdfs login`.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    }
    // If a mount daemon is running, report live status from its control socket.
    match control_request(CtlRequest::Status) {
        Ok(CtlResponse::Status {
            mountpoint,
            pinned,
            online,
            pending_uploads,
            pending_changes,
            parked_uploads,
            failing_ops,
            failing_error,
            staged_bytes,
            staged_oldest_secs,
            paused,
            paused_until,
            session_expired,
            tokens_unsaved,
            version,
            ..
        }) => {
            let state = match (online, paused) {
                _ if session_expired => ", session expired: run `pdfs login`",
                (_, true) => ", sync paused",
                (false, false) => ", offline",
                (true, false) => "",
            };
            let queued = match pending_summary(pending_uploads, pending_changes) {
                Some(s) => format!(", {s}"),
                None => String::new(),
            };
            println!("Mounted at {mountpoint} ({pinned} pinned{state}{queued})");
            // Everything below is a queue that is not making progress on its
            // own. Printed only when it exists, because in the ordinary case
            // there is nothing to say.
            if staged_bytes > 0 {
                println!(
                    "Staged     {} not yet uploaded (oldest {})",
                    human_bytes(staged_bytes),
                    human_age(staged_oldest_secs),
                );
            }
            if let Some(until) = paused_until {
                let left = until.saturating_sub(unix_now()).max(0) as u64;
                println!("Paused     sync resumes in {}", human_age(left.max(1)));
            }
            if parked_uploads > 0 {
                println!("Parked     {parked_uploads} upload(s) waiting for a final filename");
            }
            if failing_ops > 0 {
                println!(
                    "Stuck      {failing_ops} queued operation(s) keep failing: {}",
                    failing_error.as_deref().unwrap_or("no error recorded"),
                );
            }
            if tokens_unsaved {
                println!(
                    "Keyring    refuses the refreshed sign-in; unlock it, or a reboot needs `pdfs login`"
                );
            }
            if daemon_outdated(&version) {
                println!("Daemon     {}", outdated_daemon_note(&version));
            }
        }
        Ok(other) => println!("Mount: unexpected response {other:?}"),
        Err(_) => println!("Mount: not running."),
    }
    Ok(())
}

/// What `pdfs status` and `pdfs diagnose` say about a daemon still running
/// an older release than this CLI.
pub(super) fn outdated_daemon_note(version: &str) -> String {
    let running = if version.is_empty() {
        "an older release".to_string()
    } else {
        version.to_string()
    };
    format!(
        "still runs {running}, this is {}; run `systemctl --user restart proton-drive` to finish the update",
        env!("CARGO_PKG_VERSION"),
    )
}

pub(super) fn cmd_locations() -> Result<()> {
    let response = control_request(CtlRequest::ListLocations)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Locations { items } if items.is_empty() => {
            println!("No Proton Drive locations.");
        }
        CtlResponse::Locations { items } => {
            for location in items {
                let kind = match &location.kind {
                    MountKind::MyFiles => "My Files".to_string(),
                    MountKind::Device { sync_folder_id } => {
                        format!("Device folder {sync_folder_id}")
                    }
                    MountKind::Attached { sync_folder_id } => {
                        format!("My files mirror {sync_folder_id}")
                    }
                    MountKind::Shared { .. } => "Shared folder".to_string(),
                };
                let mode = match location.pending_mode {
                    Some(pending) => format!("{} -> {pending}", location.mode),
                    None => location.mode.to_string(),
                };
                let mounted = if location.mounted {
                    "mounted"
                } else {
                    "not mounted"
                };
                println!(
                    "{kind}: {} ({mode}, {}, {}, {mounted})",
                    location.local_path, location.access, location.state
                );
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// The sync engine's line in `pdfs diagnostics`: whether it runs, and which
/// folder's pass it is on.
pub(super) fn sync_engine_line(sync: &pdfs_core::control::SyncEngineState) -> String {
    let mut line = match (sync.running, sync.pass_folder) {
        (false, _) => "STOPPED; synced folders do not sync until the daemon restarts".to_string(),
        (true, None) => "idle".to_string(),
        (true, Some(folder)) => format!(
            "pass on folder {folder} for {}, last progress {} ago",
            human_secs(sync.pass_age_secs),
            human_secs(sync.pass_idle_secs),
        ),
    };
    if sync.panics > 0 {
        line.push_str(&format!("; {} pass(es) panicked", sync.panics));
    }
    line
}

/// One class of background step in `pdfs diagnostics`: `mirror: 8 running,
/// 3 waiting (oldest 2s); 120 admitted, waited 14s in total, 950ms at most`.
pub(super) fn admission_line(load: &pdfs_core::control::AdmissionLoad) -> String {
    let mut line = format!(
        "{}: {} running, {} waiting",
        load.class, load.running, load.waiting
    );
    if let Some(ms) = load.oldest_wait_ms {
        line.push_str(&format!(" (oldest {})", wait_ms(ms)));
    }
    line.push_str(&format!(
        "; {} admitted, waited {} in total, {} at most",
        load.admitted,
        wait_ms(load.waited_ms),
        wait_ms(load.longest_wait_ms),
    ));
    line
}

/// A wait, in ms below a second, where most admission waits fall.
fn wait_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        human_secs(ms / 1000)
    }
}

pub(super) fn cmd_diagnostics() -> Result<()> {
    let response = control_request(CtlRequest::Diagnostics)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Diagnostics { diagnostics: d } => {
            println!(
                "uptime {}  rss {}  pending ops {}",
                human_secs(d.uptime_secs),
                human_bytes(d.rss_bytes),
                d.pending_ops
                    .map_or_else(|| "unknown (database busy)".to_owned(), |n| n.to_string()),
            );
            println!(
                "control handlers {}/{}",
                d.control_handlers, d.control_handler_limit,
            );
            println!(
                "queued: meta {} (done {}), transfer {} (done {})",
                d.meta_queued, d.meta_completed, d.transfer_queued, d.transfer_completed,
            );
            println!("workers:");
            for worker in &d.workers {
                let what = if worker.busy {
                    worker.label.as_str()
                } else {
                    "idle"
                };
                println!(
                    "  {:<20} {:<12} {}",
                    worker.name,
                    what,
                    human_secs(worker.age_secs)
                );
            }
            if !d.services.is_empty() {
                let ended: Vec<&str> = d
                    .services
                    .iter()
                    .filter(|service| !service.running)
                    .map(|service| service.name.as_str())
                    .collect();
                if ended.is_empty() {
                    println!("services: {} running", d.services.len());
                } else {
                    println!(
                        "services: {} running, ended: {}",
                        d.services.len() - ended.len(),
                        ended.join(", ")
                    );
                }
            }
            if !d.jobs.is_empty() {
                let jobs: Vec<String> = d
                    .jobs
                    .iter()
                    .map(|job| format!("{} ({})", job.name, human_secs(job.age_secs)))
                    .collect();
                println!("jobs: {}", jobs.join(", "));
            }
            if let Some(sync) = &d.sync {
                println!("sync engine: {}", sync_engine_line(sync));
                for trace in &sync.traces {
                    let how = if trace.ended.is_empty() {
                        "running"
                    } else {
                        trace.ended.as_str()
                    };
                    println!(
                        "  folder {} pass ({how}, {}): {trace}",
                        trace.folder_id,
                        human_secs(trace.elapsed_ms / 1000),
                    );
                }
            }
            if !d.admission.is_empty() {
                println!("admission of background steps:");
                for load in &d.admission {
                    println!("  {}", admission_line(load));
                }
            }
            if !d.drive_calls.is_empty() {
                println!("calls to Drive since start:");
                for call in &d.drive_calls {
                    println!(
                        "  {:<38} {:>7} calls {:>5} failed  {:>9} ms total {:>7} ms longest",
                        call.call, call.count, call.errors, call.total_ms, call.longest_ms,
                    );
                }
            }
            if d.upload_paths.iter().any(|stat| stat.count > 0) {
                println!("uploads since start, by path:");
                for stat in &d.upload_paths {
                    println!(
                        "  {:<38} {:>7} files {:>5} failed  {:>9} ms total {:>7} ms longest",
                        stat.path, stat.count, stat.errors, stat.total_ms, stat.longest_ms,
                    );
                }
            }
            if d.inflight.is_empty() {
                println!("in-flight requests: none");
            } else {
                println!("in-flight requests (oldest first):");
                for request in &d.inflight {
                    println!("  {:<20} {}", request.kind, human_secs(request.age_secs));
                }
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_cache(action: CacheCmd) -> Result<()> {
    match action {
        CacheCmd::Inspect { deep } => cmd_cache_inspect(deep),
        CacheCmd::Vacuum => match control_request(CtlRequest::CacheVacuum)? {
            CtlResponse::Ok { message } => {
                println!("{message}");
                Ok(())
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        CacheCmd::Clear => match control_request(CtlRequest::PurgeCache)? {
            CtlResponse::Ok { message } => {
                println!("{message}");
                Ok(())
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
    }
}

/// Ask for a deep integrity check and wait for it to finish.
///
/// The daemon answers the request immediately and runs the check on a thread —
/// it holds the single database connection for as long as it takes, which would
/// otherwise stall the mount — so a caller that needs the verdict polls for it.
/// `pdfs doctor` is that caller: it is a diagnostic the user is watching, so
/// waiting is right, and the bound is generous enough for a large database
/// without hanging forever if the daemon never finishes.
pub(super) fn await_integrity_check() -> Result<CtlResponse> {
    const POLL: std::time::Duration = std::time::Duration::from_secs(1);
    const GIVE_UP_AFTER: std::time::Duration = std::time::Duration::from_secs(120);

    let started = std::time::Instant::now();
    loop {
        let response = control_request(CtlRequest::CacheInspect { deep: true })?;
        match &response {
            CtlResponse::CacheReport {
                integrity_running: true,
                integrity_checked: false,
                ..
            } if started.elapsed() < GIVE_UP_AFTER => std::thread::sleep(POLL),
            _ => return Ok(response),
        }
    }
}

pub(super) fn cmd_cache_inspect(deep: bool) -> Result<()> {
    let response = control_request(CtlRequest::CacheInspect { deep })?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::CacheReport {
            schema_version,
            db_bytes,
            db_reclaimable_bytes,
            tables,
            cache_used,
            cache_budget,
            integrity_problems,
            integrity_checked,
            integrity_running,
        } => {
            println!(
                "Database   schema v{schema_version}, {}",
                human_bytes(db_bytes)
            );
            if db_reclaimable_bytes > 0 {
                println!(
                    "           {} reclaimable — run `pdfs cache vacuum`",
                    human_bytes(db_reclaimable_bytes)
                );
            }
            let budget = if cache_budget == 0 {
                "unlimited".to_string()
            } else {
                human_bytes(cache_budget)
            };
            println!("Content    {} cached of {budget}", human_bytes(cache_used));

            println!("\nRows");
            for (name, count) in tables {
                println!("  {name:<16} {count:>10}");
            }

            if integrity_checked {
                if integrity_problems.is_empty() {
                    println!("\nIntegrity  ok");
                } else {
                    println!("\nIntegrity  {} problem(s):", integrity_problems.len());
                    for problem in integrity_problems {
                        println!("  {problem}");
                    }
                }
            } else if integrity_running {
                // The daemon runs the check on a thread rather than holding its
                // only database connection for the length of it, so the answer
                // belongs to a later request.
                println!("\nIntegrity  checking in the background — run this again for the result");
            } else {
                println!("\nIntegrity  not checked (pass --deep)");
            }
            Ok(())
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use pdfs_core::control::AdmissionLoad;

    use super::admission_line;

    #[test]
    fn an_admission_line_says_who_waits_and_for_how_long() {
        let load = AdmissionLoad {
            class: "mirror".into(),
            running: 8,
            waiting: 3,
            oldest_wait_ms: Some(2_400),
            admitted: 120,
            waited_ms: 14_000,
            longest_wait_ms: 950,
        };
        assert_eq!(
            admission_line(&load),
            "mirror: 8 running, 3 waiting (oldest 2s); 120 admitted, waited 14s in total, 950ms at most"
        );
        let idle = AdmissionLoad {
            oldest_wait_ms: None,
            waiting: 0,
            running: 0,
            ..load
        };
        assert_eq!(
            admission_line(&idle),
            "mirror: 0 running, 0 waiting; 120 admitted, waited 14s in total, 950ms at most"
        );
    }
}
