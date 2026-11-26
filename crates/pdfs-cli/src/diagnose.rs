//! `pdfs diagnose`: checks of this computer that need no running daemon.

use super::*;

#[derive(Default)]
pub(super) struct DiagnoseReport {
    warnings: usize,
    failures: usize,
}

impl DiagnoseReport {
    pub(super) fn finding(&mut self, level: DiagnoseLevel, label: &str, detail: impl AsRef<str>) {
        let mark = match level {
            DiagnoseLevel::Ok => "ok  ",
            DiagnoseLevel::Warn => {
                self.warnings += 1;
                "WARN"
            }
            DiagnoseLevel::Fail => {
                self.failures += 1;
                "FAIL"
            }
        };
        let detail = detail.as_ref();
        if detail.is_empty() {
            println!("[{mark}] {label}");
        } else {
            println!("[{mark}] {label}: {detail}");
        }
    }

    pub(super) fn check(&mut self, label: &str, ok: bool, detail: impl AsRef<str>) {
        self.finding(
            if ok {
                DiagnoseLevel::Ok
            } else {
                DiagnoseLevel::Fail
            },
            label,
            detail,
        );
    }
}

#[derive(Clone, Copy)]
pub(super) enum DiagnoseLevel {
    Ok,
    Warn,
    Fail,
}

pub(super) fn executable_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .any(|dir| std::fs::metadata(dir.join(name)).is_ok_and(|m| m.is_file()))
    })
}

/// Create, sync, and remove a new file without ever replacing an existing one.
pub(super) fn writable_probe(dir: &Path) -> std::io::Result<()> {
    let name = format!(".pdfs-diagnose-probe-{}", std::process::id());
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let result = (|| {
        file.write_all(b"probe")?;
        file.sync_all()
    })();
    drop(file);
    let remove = std::fs::remove_file(path);
    result.and(remove)
}

/// Self-check the installation, reporting each finding and exiting non-zero if
/// any check failed.
///
/// Every check is written to work with no daemon running, because that is the
/// situation a user runs this in. A missing daemon is reported as a finding,
/// not an error that aborts the rest of the report.
pub(super) fn cmd_diagnose() -> Result<()> {
    let mut report = DiagnoseReport::default();

    let dirs = AppDirs::new().context("resolve application directories")?;
    let config = dirs.load_config();

    println!("Paths");
    let state = dirs.state_dir();
    report.check(
        "  state dir",
        state.is_dir(),
        format!("{}", state.display()),
    );
    let cache_dir = dirs.cache_dir();
    report.check(
        "  cache dir",
        cache_dir.is_dir(),
        format!("{}", cache_dir.display()),
    );
    // Writability is checked by actually writing: permission bits do not
    // account for a full disk, a read-only remount, or an immutable flag.
    match writable_probe(&state) {
        Ok(()) => report.finding(
            DiagnoseLevel::Ok,
            "  state dir writable",
            "write and fsync succeeded",
        ),
        Err(e) => report.finding(DiagnoseLevel::Fail, "  state dir writable", e.to_string()),
    }

    let db_path = dirs.db_path();
    if db_path.exists() {
        let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        report.check(
            "  database",
            db_path.is_file(),
            format!("{} ({})", db_path.display(), human_bytes(db_size)),
        );
    } else {
        // The daemon creates it on its first start, which waits for a sign-in,
        // so a fresh install has none yet (issue #39 read this as the fault).
        report.finding(
            DiagnoseLevel::Warn,
            "  database",
            format!(
                "{} not created yet (the daemon creates it on first start)",
                db_path.display()
            ),
        );
    }

    println!("\nFUSE");
    let fuse = Path::new("/dev/fuse");
    report.check("  /dev/fuse", fuse.exists(), fuse.display().to_string());
    let has_fusermount = executable_on_path("fusermount3") || executable_on_path("fusermount");
    report.check(
        "  unmount helper",
        has_fusermount,
        "fusermount3 or fusermount on PATH",
    );

    println!("\nAccount");
    let mut keys_unlocked = false;
    match pdfs_core::auth::load() {
        Ok(session) => {
            report.finding(DiagnoseLevel::Ok, "  keyring session", session.username);
            let key_state = key_state_name();
            keys_unlocked = key_state != "locked";
            match key_state {
                "locked" => report.finding(
                    DiagnoseLevel::Fail,
                    "  account keys",
                    "locked — run `pdfs unlock` (the daemon waits for it)",
                ),
                "session" => report.finding(
                    DiagnoseLevel::Ok,
                    "  account keys",
                    "unlocked for this login session",
                ),
                _ => report.finding(DiagnoseLevel::Ok, "  account keys", "stored in the keyring"),
            }
        }
        Err(e) => {
            // A locked keyring and an absent login look the same from here, so
            // the detail carries the distinction rather than the verdict.
            report.finding(DiagnoseLevel::Fail, "  keyring session", format!("{e}"));
        }
    }

    println!("\nDaemon");
    let socket = dirs.control_socket();
    let socket_exists = socket.exists();
    let socket_kind_ok =
        std::fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket());
    // A stale socket file with no listener is the common failure, so the reply
    // is what decides this rather than the file's existence.
    let status = control_request(CtlRequest::Status);
    report.finding(
        if socket_exists && !socket_kind_ok {
            DiagnoseLevel::Fail
        } else if socket_exists && status.is_ok() {
            DiagnoseLevel::Ok
        } else {
            DiagnoseLevel::Warn
        },
        "  control socket",
        if socket_exists && socket_kind_ok && status.is_err() {
            format!("{} (left over; nothing listens on it)", socket.display())
        } else {
            socket.display().to_string()
        },
    );
    let mut daemon_responding = false;
    match status {
        Ok(CtlResponse::Status {
            mountpoint,
            online,
            session_expired,
            tokens_unsaved,
            pending_uploads,
            pending_changes,
            failing_ops,
            failing_error,
            staged_bytes,
            staged_oldest_secs,
            paused,
            paused_until,
            version,
            ..
        }) => {
            daemon_responding = true;
            report.finding(DiagnoseLevel::Ok, "  daemon responding", "");
            if daemon_outdated(&version) {
                report.finding(
                    DiagnoseLevel::Warn,
                    "  daemon version",
                    outdated_daemon_note(&version),
                );
            }
            let live_type = pdfs_core::mountpoint::mounted_fs_type(Path::new(&mountpoint));
            let is_fuse = live_type
                .as_deref()
                .is_some_and(|kind| kind.starts_with("fuse"));
            report.finding(
                if is_fuse {
                    DiagnoseLevel::Ok
                } else {
                    DiagnoseLevel::Fail
                },
                "  active mount",
                live_type.map_or_else(
                    || format!("{mountpoint} is not mounted"),
                    |kind| format!("{mountpoint} ({kind})"),
                ),
            );
            // The keyring check above read the blob, which still holds the
            // tokens the daemon could not replace.
            if tokens_unsaved {
                report.finding(
                    DiagnoseLevel::Warn,
                    "  session tokens",
                    "the keyring refuses the refreshed ones; unlock it, or a reboot needs `pdfs login`",
                );
            }
            // A refused session reads as offline too, but no connection will
            // bring it back, and the keyring check above cannot tell.
            if session_expired {
                report.finding(
                    DiagnoseLevel::Fail,
                    "  session",
                    "Proton refused it (expired or revoked) — run `pdfs login`",
                );
            } else {
                report.finding(
                    if online {
                        DiagnoseLevel::Ok
                    } else {
                        DiagnoseLevel::Warn
                    },
                    "  network",
                    if online {
                        "online"
                    } else {
                        "offline; cached data remains available"
                    },
                );
            }
            let pending = pending_uploads + pending_changes;
            report.finding(
                if pending == 0 {
                    DiagnoseLevel::Ok
                } else {
                    DiagnoseLevel::Warn
                },
                "  queued writes",
                if pending == 0 {
                    "none".to_string()
                } else {
                    format!("{pending} waiting to upload")
                },
            );
            // The rest of the daemon's state that the report used to drop:
            // writes that keep failing, a pause, a folder in error. Each of
            // them stops changes from reaching Drive while everything above
            // reads as fine (`docs/BUGS.md` B268).
            for (level, check, detail) in sync_health(
                failing_ops,
                failing_error.as_deref(),
                staged_bytes,
                staged_oldest_secs,
                paused,
                paused_until.map(|until| until - unix_now()),
            ) {
                report.finding(level, check, detail);
            }
            if let Ok(CtlResponse::SyncFolders { items }) =
                control_request(CtlRequest::ListSyncFolders)
            {
                for (level, check, detail) in folder_health(&items) {
                    report.finding(level, &check, detail);
                }
            }
            match await_integrity_check() {
                Ok(CtlResponse::CacheReport {
                    integrity_problems,
                    integrity_checked: true,
                    ..
                }) if integrity_problems.is_empty() => report.finding(
                    DiagnoseLevel::Ok,
                    "  database integrity",
                    "SQLite integrity_check passed",
                ),
                Ok(CtlResponse::CacheReport {
                    integrity_checked: false,
                    ..
                }) => report.finding(
                    DiagnoseLevel::Warn,
                    "  database integrity",
                    "still checking — run `pdfs cache inspect --deep` for the result",
                ),
                Ok(CtlResponse::CacheReport {
                    integrity_problems, ..
                }) => report.finding(
                    DiagnoseLevel::Fail,
                    "  database integrity",
                    format!(
                        "{} problem(s): {}",
                        integrity_problems.len(),
                        integrity_problems.join("; ")
                    ),
                ),
                Ok(other) => report.finding(
                    DiagnoseLevel::Fail,
                    "  database integrity",
                    format!("unexpected: {other:?}"),
                ),
                Err(e) => report.finding(
                    DiagnoseLevel::Fail,
                    "  database integrity",
                    format!("check failed: {e:#}"),
                ),
            }
        }
        Ok(other) => {
            report.finding(
                DiagnoseLevel::Fail,
                "  daemon responding",
                format!("unexpected: {other:?}"),
            );
        }
        Err(e) => {
            // Not a failure of the installation: a stopped daemon is a normal
            // state, and the rest of the report is still worth printing.
            report.finding(
                DiagnoseLevel::Warn,
                "  daemon responding",
                format!("stopped or unreachable: {e:#}"),
            );
            match pdfs_core::mountfail::read(&dirs.state_dir()) {
                // A daemon whose mount keeps failing never opens the socket,
                // so this is the only place its reason shows (B263).
                Some(failure) => report.finding(
                    DiagnoseLevel::Fail,
                    "  mount",
                    format!(
                        "failing for {}, {} attempt(s): {}",
                        human_secs(unix_now().saturating_sub(failure.since).max(0) as u64),
                        failure.attempts,
                        if failure.awaiting_keys {
                            format!("{} — after recovering your account keys, restart proton-drive.service", failure.error)
                        } else {
                            failure.error
                        }
                    ),
                ),
                None => println!(
                    "         (start it with `systemctl --user start proton-drive.service`)"
                ),
            }
        }
    }

    let mountpoint = dirs.resolved_mountpoint(&config);
    report.finding(
        if mountpoint.is_dir() {
            DiagnoseLevel::Ok
        } else {
            DiagnoseLevel::Warn
        },
        "  configured mountpoint",
        mountpoint.display().to_string(),
    );

    // The daemon opens its control socket only once My Files is mounted, so a
    // daemon that answers has the keys it needs. One that does not may be stuck
    // on a key that does not open (#40); only then is Proton asked.
    if keys_unlocked && !daemon_responding {
        println!("\nKeys");
        diagnose_keys(&mut report);
    }

    if report.failures == 0 {
        if report.warnings == 0 {
            println!("\nNo problems found.");
        } else {
            println!(
                "\nNo critical problems found; {} warning(s) need attention.",
                report.warnings
            );
        }
        Ok(())
    } else {
        bail!(
            "{} check(s) failed and {} warning(s) were reported; see above",
            report.failures,
            report.warnings
        )
    }
}

/// Ask Proton which account and address keys open, and which of them the My
/// Files share needs. The findings hold key IDs and no secrets.
pub(super) fn diagnose_keys(report: &mut DiagnoseReport) {
    use pdfs_core::keycheck::{self, Level};

    let checked = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|rt| {
            rt.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(30), keycheck::run())
                    .await
                    .map_err(|_| anyhow!("Proton did not answer within 30 seconds"))?
                    .map_err(anyhow::Error::from)
            })
        });
    let (keys, share) = match checked {
        Ok(checked) => checked,
        Err(e) => {
            report.finding(DiagnoseLevel::Warn, "  key check", format!("{e:#}"));
            return;
        }
    };
    let findings = keycheck::findings(&keys, &share);
    let any_failed = findings.iter().any(|f| f.level != Level::Ok);
    for finding in findings {
        let level = match finding.level {
            Level::Ok => DiagnoseLevel::Ok,
            Level::Warn => DiagnoseLevel::Warn,
            Level::Fail => DiagnoseLevel::Fail,
        };
        report.finding(level, &format!("  {}", finding.label), finding.detail);
    }
    if any_failed {
        println!("         (key IDs are not secret; include this section when you report it)");
    }
}

#[cfg(test)]
mod diagnose_tests {
    use super::*;

    /// A healthy queue says nothing; writes that keep failing, an old unsent
    /// write and a pause each say so (`docs/BUGS.md` B268).
    #[test]
    fn a_stuck_queue_or_a_pause_is_reported() {
        assert!(sync_health(0, None, 0, 0, false, None).is_empty());
        assert!(sync_health(0, None, 4096, STAGED_STALE_SECS - 1, false, None).is_empty());
        let checks: Vec<&str> = sync_health(
            2,
            Some("upload a.txt: quota"),
            4096,
            STAGED_STALE_SECS,
            true,
            Some(600),
        )
        .into_iter()
        .map(|(level, check, _)| {
            assert!(matches!(level, DiagnoseLevel::Warn));
            check.trim()
        })
        .collect();
        assert_eq!(checks, ["failing writes", "oldest unsent write", "sync"]);
    }

    /// Issue #53: a folder that would not sync, and `diagnose` found nothing.
    #[test]
    fn a_synced_folder_in_error_is_reported() {
        let folder = |id: i64, state: &str| pdfs_core::control::SyncFolderInfo {
            kind: "device".into(),
            id,
            local_path: format!("/home/u/f{id}"),
            remote_uid: "vol~link".into(),
            mode: "mirror".into(),
            state: state.into(),
            last_sync: 0,
            pending_mode: None,
            progress: None,
            paused: false,
        };
        let findings =
            folder_health(&[folder(1, "idle"), folder(2, "error"), folder(3, "syncing")]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].1.trim(), "synced folder [2]");
        assert!(findings[0].2.starts_with("/home/u/f2: sync error"));
    }

    #[test]
    fn finding_severity_is_counted_centrally() {
        let mut report = DiagnoseReport::default();
        report.finding(DiagnoseLevel::Ok, "ok", "");
        report.finding(DiagnoseLevel::Warn, "warn", "");
        report.finding(DiagnoseLevel::Fail, "fail", "");
        assert_eq!(report.warnings, 1);
        assert_eq!(report.failures, 1);
    }

    #[test]
    fn writable_probe_never_replaces_an_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-diagnose-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let collision = dir.join(format!(".pdfs-diagnose-probe-{}", std::process::id()));
        std::fs::write(&collision, b"keep me").unwrap();

        assert_eq!(
            writable_probe(&dir).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&collision).unwrap(), b"keep me");

        std::fs::remove_file(collision).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
