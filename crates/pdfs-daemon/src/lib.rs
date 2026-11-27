//! The daemon's composition root.
//!
//! `pdfs daemon` runs [`run`]: it waits for a stored session, opens the
//! database and the content cache, resumes the session on a runtime of its
//! own, and mounts. It keeps the mount alive: a clean stop exits, an
//! external unmount remounts, and a failure is recorded and retried with a
//! backoff. The mount itself, and the services it starts, are `pdfs-fuse`.
//!
//! Step 3 of `docs/ARCHITECTURE.md` moves the building of those services
//! here, one at a time.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use pdfs_core::auth;
use pdfs_core::config::AppDirs;
use pdfs_store::cache::ContentCache;
use pdfs_store::db::Db;

/// How long to wait before the next login check, after `attempts` checks have
/// already come back [`pdfs_core::Error::NotLoggedIn`].
///
/// Doubles from 3s to a 15s ceiling. The interval is not just a sleep:
/// [`auth::load`] builds a fresh `keyring::Entry` on every call, so each check
/// opens a new D-Bus connection to the Secret Service and negotiates an
/// encrypted session over it. A flat 3s poll meant 28,800 of those a day from a
/// daemon that had nothing to do; the ceiling cuts that by 80%.
///
/// The ceiling stays low because the sleep is also the worst-case login →
/// mount latency for anything that does *not* poke systemd: front-ends restart
/// the unit on login (`service::enable_start`), which interrupts the sleep
/// immediately, but a hand-run `pdfs daemon` has no such wake-up path.
fn login_poll_delay(attempts: u32) -> std::time::Duration {
    const BASE_SECS: u64 = 3;
    const MAX_SECS: u64 = 15;

    // `checked_shl` rather than `<<`: the daemon can sit unauthenticated for
    // weeks, and a shift past the width of the type is a panic, not a big number.
    let factor = 1u64.checked_shl(attempts).unwrap_or(u64::MAX);
    std::time::Duration::from_secs(BASE_SECS.saturating_mul(factor).min(MAX_SECS))
}

/// Block until a session is stored and unlocked, backing off between checks.
///
/// `load` and `sleep` are parameters rather than direct calls so the backoff
/// schedule can be tested without a keyring and without real time passing.
fn wait_for_session<L, S>(mut load: L, mut sleep: S) -> Result<()>
where
    L: FnMut() -> std::result::Result<(), pdfs_core::Error>,
    S: FnMut(std::time::Duration),
{
    let mut attempts: u32 = 0;
    loop {
        match load() {
            Ok(()) => return Ok(()),
            Err(pdfs_core::Error::NotLoggedIn) => {
                let delay = login_poll_delay(attempts);
                // Say it once at INFO. Repeating it every interval buried the
                // journal in tens of thousands of identical lines a day.
                if attempts == 0 {
                    tracing::info!("not logged in; waiting for a session…");
                } else {
                    tracing::debug!(delay_secs = delay.as_secs(), "still not logged in");
                }
                sleep(delay);
                attempts = attempts.saturating_add(1);
            }
            Err(pdfs_core::Error::Locked) => {
                let delay = login_poll_delay(attempts);
                if attempts == 0 {
                    tracing::info!("account is locked; waiting for `pdfs unlock`…");
                } else {
                    tracing::debug!(delay_secs = delay.as_secs(), "account still locked");
                }
                sleep(delay);
                attempts = attempts.saturating_add(1);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Run the daemon: the auto-mount loop. Waits for a stored login, mounts, and keeps
/// the mount alive: a clean stop (SIGTERM via `systemctl --user stop`) exits 0;
/// an external unmount triggers a remount; errors back off and retry. This is
/// the entry point for the systemd user service.
pub fn run(mountpoint: Option<PathBuf>) -> Result<()> {
    let state_dir = AppDirs::new()?.state_dir();
    // A failure an earlier daemon recorded is not this one's.
    pdfs_core::mountfail::clear(&state_dir);
    let mut failure: Option<pdfs_core::mountfail::MountFailure> = None;
    loop {
        // Wait until a session is stored. The GUI enables this service on login,
        // but the service may also start at boot before the user has logged in.
        wait_for_session(auth::ready, pdfs_fuse::idle_sleep)?;

        let outcome = mount_once(mountpoint.clone());
        if outcome.is_ok() {
            // The mount came up, so the next failure starts a new run of them.
            failure = None;
        }
        match outcome {
            Ok(pdfs_fuse::MountOutcome::Shutdown) => {
                tracing::info!("daemon stopping");
                return Ok(());
            }
            Ok(pdfs_fuse::MountOutcome::ResumeStored) => {
                tracing::info!("resuming the session stored in the keyring");
            }
            Ok(pdfs_fuse::MountOutcome::Unmounted) => {
                tracing::warn!("mount ended externally; remounting in 2s");
                pdfs_fuse::idle_sleep(std::time::Duration::from_secs(2));
            }
            Err(e) => {
                // `{:#}` so the whole anyhow chain lands in the log — the top
                // context alone ("mount failed") never says *why*.
                let error = format!("{e:#}");
                let now = unix_now();
                let attempts = failure.as_ref().map_or(1, |f| f.attempts.saturating_add(1));
                let delay = mount_retry_schedule(&e, attempts);
                match delay {
                    Some(delay) => tracing::error!(
                        error,
                        attempts,
                        retry_secs = delay.as_secs(),
                        "mount failed; retrying"
                    ),
                    None => tracing::error!(
                        error,
                        attempts,
                        "account keys could not open My Files; waiting for service restart"
                    ),
                }
                let record = pdfs_core::mountfail::MountFailure {
                    error: error.clone(),
                    since: failure.as_ref().map_or(now, |f| f.since),
                    attempts,
                    next_attempt: now + delay.map_or(0, |delay| delay.as_secs() as i64),
                    awaiting_keys: delay.is_none(),
                    not_empty: not_empty_cause(&e),
                };
                if let Err(e) = pdfs_core::mountfail::record(&state_dir, &record) {
                    tracing::warn!(error = %e, "could not record the mount failure");
                }
                pdfs_fuse::notify_status(&format!("Mount failing: {error}"));
                failure = Some(record);
                // Retry and a successful login restart the service. Keep the
                // watchdog alive while parked without contacting Proton.
                match delay {
                    Some(delay) => pdfs_fuse::idle_sleep(delay),
                    None => return pdfs_fuse::wait_for_restart().context("wait for key recovery"),
                }
            }
        }
    }
}

/// The folder in the way, when `error` is a mount refused over a folder that
/// already has files in it. The refusal travels inside an `io::Error`, whose
/// `source` skips the error it wraps, so each link is unwrapped by hand.
fn not_empty_cause(error: &anyhow::Error) -> Option<pdfs_core::mountpoint::NotEmpty> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<pdfs_core::mountpoint::NotEmpty>()
            .or_else(|| {
                cause
                    .downcast_ref::<std::io::Error>()?
                    .get_ref()?
                    .downcast_ref::<pdfs_core::mountpoint::NotEmpty>()
            })
            .cloned()
    })
}

fn mount_retry_schedule(error: &anyhow::Error, attempts: u32) -> Option<std::time::Duration> {
    let root = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<std::io::Error>()?
            .get_ref()?
            .downcast_ref::<pdfs_core::mountfail::RootLookup>()
    });
    if root.is_some_and(|root| root.needs_account_keys()) {
        None
    } else {
        Some(mount_retry_delay(attempts))
    }
}

/// How long to wait before mount attempt `attempts + 1`, after `attempts`
/// failures in a row.
///
/// Doubles from 5 s to a 2-minute ceiling. A flat 5 s retry sent about four
/// requests to Proton per attempt, tens of thousands a day from an account
/// whose mount fails on a key, which no retry fixes (`docs/BUGS.md` B263). The
/// ceiling stays at minutes because a cold start without a network fails the
/// same way, and must mount soon after the network returns.
fn mount_retry_delay(attempts: u32) -> std::time::Duration {
    const BASE_SECS: u64 = 5;
    const MAX_SECS: u64 = 120;

    let factor = 1u64
        .checked_shl(attempts.saturating_sub(1))
        .unwrap_or(u64::MAX);
    std::time::Duration::from_secs(BASE_SECS.saturating_mul(factor).min(MAX_SECS))
}

/// One mount run: resume the session, mount, and block until the mount ends
/// (clean shutdown or external unmount). Returns the [`MountOutcome`] so the
/// daemon loop can decide whether to exit or remount.
pub fn mount_once(mountpoint: Option<PathBuf>) -> Result<pdfs_fuse::MountOutcome> {
    let dirs = AppDirs::new()?;
    dirs.ensure()?;
    let config = dirs.load_config();
    // An explicit `--mountpoint` wins; otherwise honor the Settings-page choice
    // (config), falling back to the default location.
    let mountpoint = mountpoint.unwrap_or_else(|| dirs.resolved_mountpoint(&config));
    // Hydrating a large account and remounting its folders can outlast the
    // unit's watchdog before the supervisor sends its first ping (B279).
    pdfs_fuse::stretch_watchdog_for_startup();
    pdfs_fuse::prepare_mountpoint(&mountpoint)
        .with_context(|| format!("create mountpoint {}", mountpoint.display()))?;

    let username = auth::load().map(|s| s.username).unwrap_or_default();
    // The daemon owns the unified DB (single writer). Open it here so the content
    // cache and the mount share one handle: the cache uses its `cache_entries`
    // table as the LRU index, the mount uses it for nodes/search/event cursor.
    let db = Arc::new(Db::open(&dirs.db_path()).context("open cache db")?);
    let cache = ContentCache::open(
        dirs.content_cache_dir(),
        dirs.pins_path(),
        config.resolved_cache_budget(),
        db.clone(),
    )
    .context("open content cache")?;
    let control_socket = dirs.control_socket();

    // Multi-threaded runtime: its worker threads keep servicing async SDK calls
    // while the main thread is parked inside the blocking FUSE session loop.
    let rt = tokio::runtime::Runtime::new()?;
    let (client, session) = rt
        .block_on(auth::resume_client())
        .context("resume session (run `pdfs login` first)")?;
    // On unless the config turns it off: the one-request upload for small
    // files is new, and this lets it be switched off without a new build.
    let small_uploads = config.small_uploads.unwrap_or(true);
    if !small_uploads {
        tracing::info!("small files are uploaded in blocks: small_uploads is off in the config");
    }
    let client = client.with_small_file_upload(small_uploads);

    // Token updates are persisted automatically in real-time. The SDK session has
    // a token refresh callback registered during `resume_client` that writes updated
    // credentials back to the OS keyring immediately upon rotation. This prevents
    // the system keyring from holding stale refresh tokens even if the daemon crashes,
    // the system is suspended, or the network changes.
    let handle = rt.handle().clone();
    let result = pdfs_fuse::mount(
        client,
        handle,
        &mountpoint,
        cache,
        &control_socket,
        db,
        pdfs_fuse::MountOptions {
            username,
            gallery_session: Some(session.clone()),
            sweep_mode: config.resolved_conflict_sweep(),
            upload_limit: config.upload_limit.unwrap_or(0),
            download_limit: config.download_limit.unwrap_or(0),
            local_first: config.local_first.unwrap_or(true),
        },
    );

    // Last chance for a rotation the keyring refused when it happened. Only
    // that: by now the keyring may hold a newer login, the one this stop was
    // restarted for, and the tokens of this session would overwrite it.
    if let Err(e) = auth::persist(&session) {
        tracing::error!(
            error = %e,
            "could not save the refreshed session tokens; the next start may need `pdfs login`"
        );
    }
    release_runtime(rt, matches!(result, Ok(pdfs_fuse::MountOutcome::Shutdown)));

    let outcome = result.context("mount failed")?;
    Ok(outcome)
}

/// Let go of a mount's runtime once the mount has ended.
///
/// On a stop the process exits right after, so the runtime is left running
/// for it to end with. Shutting it down first broke every thread the mount
/// does not join, such as a size lookup or a profile backup a request started
/// during the stop: its next call to Drive panicked with "A Tokio 1.x context
/// was found, but it is being shutdown" (`docs/BUGS.md` B276). A mount that
/// is about to be tried again does shut it down, because a new runtime
/// replaces it. Dropping it would wait for every blocking task, however long;
/// a call still stuck on Drive would hold the retry (B165).
fn release_runtime(rt: tokio::runtime::Runtime, exiting: bool) {
    if exiting {
        std::mem::forget(rt);
    } else {
        rt.shutdown_timeout(std::time::Duration::from_secs(2));
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod release_runtime_tests {
    use super::*;
    use std::time::Duration;

    /// A thread the mount never joined still reaches Drive after a stop. It
    /// used to panic on its first timer once the runtime was shut down.
    #[test]
    fn a_thread_left_running_at_a_stop_can_still_use_the_runtime() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let straggler = std::thread::spawn(move || {
            released_rx.recv().unwrap();
            handle.block_on(async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                7
            })
        });
        release_runtime(rt, true);
        released_tx.send(()).unwrap();
        assert_eq!(straggler.join().expect("the thread must not panic"), 7);
    }

    #[test]
    fn a_mount_tried_again_shuts_its_runtime_down() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        rt.spawn(async move {
            let _done = done_tx;
            std::future::pending::<()>().await;
        });
        release_runtime(rt, false);
        // The task is dropped with the runtime, and its sender with it.
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The mount retry backs off from 5 s to a ceiling of minutes, rather than
    /// asking Proton every 5 s forever (`docs/BUGS.md` B263).
    #[test]
    fn a_failing_mount_backs_off_to_a_ceiling() {
        let secs: Vec<u64> = (1..=8).map(|n| mount_retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [5, 10, 20, 40, 80, 120, 120, 120]);
        assert_eq!(mount_retry_delay(u32::MAX).as_secs(), 120);
    }

    #[test]
    fn cold_root_key_failures_wait_for_a_restart() {
        use pdfs_core::proton_sdk::{ProtonError, crypto::CryptoError};
        for (cause, parked) in [
            (ProtonError::KeysLocked, true),
            (CryptoError::Decrypt("missing key".into()).into(), true),
            (CryptoError::Decrypt("malformed data".into()).into(), false),
            (ProtonError::invalid_operation("missing key"), false),
        ] {
            let io = std::io::Error::other(pdfs_core::mountfail::RootLookup(cause));
            let error = anyhow::Error::new(io).context("mount failed");
            assert_eq!(mount_retry_schedule(&error, 1).is_none(), parked);
        }
        let io = std::io::Error::from(std::io::ErrorKind::NetworkUnreachable);
        assert_eq!(
            mount_retry_schedule(&io.into(), 1),
            Some(mount_retry_delay(1))
        );
    }

    /// A refusal over a folder with files in it reaches the failure record
    /// through the `io::Error` and the context around it (`docs/BUGS.md` B277).
    #[test]
    fn a_mount_refused_over_files_is_recorded_as_such() {
        let found = pdfs_core::mountpoint::NotEmpty {
            path: PathBuf::from("/home/you/ProtonDrive"),
            entries: vec!["Work".into()],
            count: 1,
        };
        let error =
            anyhow::Error::new(std::io::Error::other(found.clone())).context("mount failed");
        assert_eq!(not_empty_cause(&error), Some(found));
        let other =
            anyhow::Error::new(std::io::Error::other("missing key")).context("mount failed");
        assert_eq!(not_empty_cause(&other), None);
    }

    #[test]
    fn poll_delay_doubles_then_holds_at_the_ceiling() {
        assert_eq!(login_poll_delay(0), Duration::from_secs(3));
        assert_eq!(login_poll_delay(1), Duration::from_secs(6));
        assert_eq!(login_poll_delay(2), Duration::from_secs(12));
        assert_eq!(login_poll_delay(3), Duration::from_secs(15));
        assert_eq!(login_poll_delay(4), Duration::from_secs(15));
        // A daemon left unauthenticated for weeks reaches attempt counts well
        // past the width of the shift; that must cap, not panic.
        assert_eq!(login_poll_delay(64), Duration::from_secs(15));
        assert_eq!(login_poll_delay(u32::MAX), Duration::from_secs(15));
    }

    #[test]
    fn waiting_for_a_session_stops_hammering_the_keyring() {
        let mut checks_left = 20;
        let mut slept: Vec<Duration> = Vec::new();

        wait_for_session(
            || {
                if checks_left == 0 {
                    Ok(())
                } else {
                    checks_left -= 1;
                    Err(pdfs_core::Error::NotLoggedIn)
                }
            },
            |delay| slept.push(delay),
        )
        .unwrap();

        assert_eq!(slept.len(), 20, "one wait per failed check");
        assert!(
            slept.windows(2).all(|w| w[1] >= w[0]),
            "the interval must never shrink: {slept:?}"
        );

        // The point of the change. Every check is a fresh D-Bus session against
        // the Secret Service, so 20 of them must not fit inside a single minute
        // the way a flat 3s poll did.
        let total: Duration = slept.iter().sum();
        assert!(
            total >= Duration::from_secs(4 * 60),
            "20 checks should span at least 4 minutes, spanned {total:?}"
        );
    }

    #[test]
    fn a_locked_account_is_waited_on_until_it_is_unlocked() {
        let mut checks = vec![
            Ok(()),
            Err(pdfs_core::Error::Locked),
            Err(pdfs_core::Error::NotLoggedIn),
            Err(pdfs_core::Error::Locked),
        ];
        let mut slept = 0;

        wait_for_session(|| checks.pop().unwrap(), |_| slept += 1).unwrap();

        assert_eq!(slept, 3, "locked and signed-out are both waited out");
    }

    #[test]
    fn a_stored_session_is_not_waited_for_at_all() {
        let mut slept = 0;
        wait_for_session(|| Ok(()), |_| slept += 1).unwrap();
        assert_eq!(slept, 0);
    }
}
