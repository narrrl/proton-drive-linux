//! Mount lifecycle and background-service orchestration.

use super::*;

/// Why a [`mount`] call returned. Lets the daemon decide whether to exit (clean
/// shutdown) or remount (the mount went away under it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountOutcome {
    /// The daemon was asked to stop (SIGTERM/SIGINT) and we lazily unmounted
    /// ourselves. The caller should exit cleanly.
    Shutdown,
    /// The kernel mount ended on its own (e.g. an external `fusermount -u`).
    /// The caller may want to remount.
    Unmounted,
}

/// Whether `path` is a mountpoint whose FUSE connection is dead — the state a
/// daemon killed before it could unmount leaves behind.
///
/// The kernel answers every operation on such a path with `ENOTCONN`, which the
/// ordinary existence checks (`is_dir`, `exists`) report as plain `false`: the
/// path looks *absent* rather than broken. Callers that would otherwise treat
/// that as "nothing to mount here" use this to tell the two apart.
pub(crate) fn is_stale_mount(path: &Path) -> bool {
    matches!(
        std::fs::metadata(path).map_err(|e| e.raw_os_error()),
        Err(Some(libc::ENOTCONN))
    )
}

/// A secondary (on-demand sync folder) FUSE session, paired with its FUSE
/// connection id so teardown can abort a mid-transfer connection rather than
/// block on it. See [`Core::mounts`] and [`umount_session_unblocked`].
pub(super) type SecondaryMount = (BackgroundSession, Option<u32>);

/// The kernel's id for the FUSE connection backing the mount at `mountpoint` —
/// the directory name under `/sys/fs/fuse/connections`, which is the minor
/// number of the mountpoint's device. Must be read *while still mounted*; after
/// unmount the path resolves to the underlying directory on another device.
/// `None` when the path can't be stat'd.
pub(super) fn fuse_connection_id(mountpoint: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(mountpoint).ok()?.dev();
    Some(libc::minor(dev))
}

/// Force the kernel to abort FUSE connection `id`, erroring every in-flight
/// request.
///
/// On a stop signal we unmount lazily (`MNT_DETACH`) so the call succeeds even
/// mid-transfer — but detach only removes the mountpoint; it does *not* end the
/// connection while a request is still in flight. fuser's session loop then
/// blocks on `/dev/fuse` waiting for a next request that never comes, so `join`
/// hangs — long enough during a transfer that systemd's stop timeout SIGKILLs
/// the daemon mid-unmount, stranding the on-demand mounts as dead endpoints.
/// Writing the connection's `abort` file makes the pending reads fail with
/// `ENODEV`, so the loop returns and `join` completes at once. Best-effort:
/// there is nothing more to do on the shutdown path if it fails.
pub(super) fn abort_fuse_connection(id: u32) {
    let path = format!("/sys/fs/fuse/connections/{id}/abort");
    if std::fs::write(&path, b"1").is_ok() {
        info!(id, "aborted FUSE connection to unblock unmount");
    }
}

/// Unmount a background session that may be mid-transfer without wedging: abort
/// its connection first (so the session loop exits promptly), then lazily
/// unmount and join. `conn` is the id captured at mount time; `None` skips the
/// abort and just unmounts (a healthy idle mount joins on its own `Destroy`).
pub(super) fn umount_session_unblocked(
    session: BackgroundSession,
    conn: Option<u32>,
) -> std::io::Result<()> {
    if let Some(id) = conn {
        abort_fuse_connection(id);
    }
    session.umount_and_join()
}

/// Best-effort teardown of a stale mount left behind by a crashed daemon. A
/// previous run that died without unmounting leaves the kernel mount in place,
/// so the fresh `Session::new` below would fail with EBUSY ("Device or resource
/// busy"). `fusermount3 -u -z` is the lazy (detach) unmount, which succeeds even
/// when the old mount is still busy. Swallow all output/errors: if there is no
/// stale mount this is simply a no-op.
pub(super) fn clear_stale_mount(mountpoint: &Path) {
    for bin in ["fusermount3", "fusermount"] {
        let ok = std::process::Command::new(bin)
            .arg("-u")
            .arg("-z")
            .arg(mountpoint)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            info!(mountpoint = %mountpoint.display(), "cleared stale mount before remount");
            return;
        }
    }
}

/// `sync_state` key holding the uid of the My Files root, so a later run can
/// recover it from `nodes` without the API (offline.md Phase 1).
const ROOT_UID_KEY: &str = "root_uid";

/// The My Files root, and whether we got it from the API (`true`) or from the
/// cache because the API was unreachable (`false`).
///
/// A successful fetch also records the root's uid, which is what makes the
/// fallback possible on a later run: `nodes` is keyed by uid, so without this we
/// would have the root's row on disk and no way to tell which one it is.
///
/// Failing to fetch is only fatal on a genuinely cold start — no cached root
/// means an empty tree, and mounting that would show the user an empty Drive
/// rather than an honest error.
fn fetch_or_recall_root(
    client: &ProtonDriveClient,
    rt: &tokio::runtime::Handle,
    db: &Db,
) -> std::io::Result<(Node, bool)> {
    let err = match rt.block_on(client.get_my_files_folder()) {
        Ok(root) => {
            if let Err(e) = db.set_state_str(ROOT_UID_KEY, &root.uid.to_string()) {
                warn!(error = %e, "persist root uid failed");
            }
            return Ok((root, true));
        }
        Err(e) => e,
    };
    let cached = db
        .state_str(ROOT_UID_KEY)
        .ok()
        .flatten()
        .and_then(|uid| db.node_by_uid(&uid).ok().flatten());
    match cached {
        Some(root) => {
            warn!(error = %err, "fetch My Files root failed; mounting from cache (offline)");
            Ok((root, false))
        }
        None => Err(std::io::Error::other(format!("fetch My Files root: {err}"))),
    }
}

/// Mount the filesystem at `mountpoint` and block until it is unmounted or the
/// daemon is asked to stop.
///
/// Resolves the My Files root up front — from the API, or from the cached tree
/// when the API is unreachable (`fetch_or_recall_root`) — then spawns the
/// Phase 2 event-sync task, the
/// Phase 4 control socket, and runs the FUSE session on its own thread while
/// this thread waits for either a stop signal (SIGTERM/SIGINT) or the kernel
/// mount ending on its own. On a stop signal we lazily unmount ourselves
/// (`umount_and_join`, the MNT_DETACH path that succeeds even while downloads
/// are in flight), so `systemctl --user stop` is always a clean teardown.
/// `rt` must be a handle to a *running* multi-threaded runtime.
pub fn mount(
    client: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    mountpoint: &Path,
    cache: ContentCache,
    control_socket: &Path,
    db: Arc<Db>,
    username: String,
) -> std::io::Result<MountOutcome> {
    let (root, online) = fetch_or_recall_root(&client, &rt, &db)?;
    let scope = root.tree_event_scope_id();

    // The folder-sync engine (devices.md Phase 2) runs on its own thread and is
    // nudged over this channel; the sender lives in Core so control-socket
    // handlers can trigger reconciles.
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<sync::SyncMsg>();

    let core = Core {
        client: client.clone(),
        rt: rt.clone(),
        state: Arc::new(Mutex::new(State {
            entries: HashMap::new(),
            by_uid: HashMap::new(),
            children: HashMap::new(),
            next_ino: 2,
            active_writes: HashMap::new(),
            handles: HashMap::new(),
            next_fh: 1,
            db: db.clone(),
        })),
        cache: Arc::new(cache),
        readers: Arc::new(Mutex::new(HashMap::new())),
        stream_ring: Arc::new(Mutex::new(StreamRing::default())),
        workers: Arc::new(Workers::new(FUSE_WORKERS)?),
        db,
        online: Arc::new(AtomicBool::new(online)),
        pending: Arc::new(Mutex::new(HashMap::new())),
        hidden: Arc::new(Mutex::new(HashSet::new())),
        drain_wake: Arc::new((Mutex::new(false), Condvar::new())),
        timeline_refreshing: Arc::new(AtomicBool::new(false)),
        trash_refreshing: Arc::new(AtomicBool::new(false)),
        thumb_gen: Arc::new(Mutex::new(HashSet::new())),
        no_thumbnail: Arc::new(Mutex::new(HashMap::new())),
        size_upgrades: Arc::new(Mutex::new(HashMap::new())),
        notifier: Arc::new(OnceLock::new()),
        transfers: TransferRegistry::new(),
        indexing: Arc::new(AtomicBool::new(false)),
        sync_progress: Arc::new(Mutex::new(HashMap::new())),
        sync_tx,
        mounts: Arc::new(Mutex::new(HashMap::new())),
        sync_locks: Arc::new(Mutex::new(HashMap::new())),
    };

    // Writes queued by a previous run (or left behind by a crash) are still owed
    // an upload, and reads must be served from their staged blobs until they land.
    core.hydrate_pending();
    // Then the writes that were fsynced but never closed, which the cache moved
    // aside at open. After `hydrate_pending`, so a recovered partial write can
    // see an already-queued write to the same node.
    core.recover_fsynced_writes();
    {
        let core = core.clone();
        std::thread::Builder::new()
            .name("pdfs-drain".into())
            .spawn(move || core.run_pending_drain())?;
    }

    // Start the folder-sync engine. It watches every mirror folder, polls the
    // remotes, and reconciles on its own thread — never in front of a FUSE call.
    sync::spawn(core.clone(), sync_rx);

    // Mounted from the cache: watch for the network coming back so the mount can
    // stop being read-only-ish without the user restarting the daemon.
    if !online {
        let core = core.clone();
        std::thread::Builder::new()
            .name("pdfs-online-probe".into())
            .spawn(move || core.run_online_probe())?;
    }

    // Re-establish on-demand mounts left over from a previous run (devices.md
    // Phase 4). On its own thread: each remount fetches a remote node, and we
    // must not block the main mount below on the network.
    {
        let core = core.clone();
        std::thread::Builder::new()
            .name("pdfs-restore-ondemand".into())
            .spawn(move || core.restore_ondemand_mounts())?;
    }

    // Keep the launcher prompt's "This computer" index fresh. Its own thread:
    // the walk is I/O-heavy and must never sit in front of a FUSE callback.
    {
        let db = core.db.clone();
        let indexing = core.indexing.clone();
        let transfers = core.transfers.clone();
        let mountpoint = mountpoint.to_path_buf();
        std::thread::Builder::new()
            .name("pdfs-localindex".into())
            .spawn(move || run_local_index(db, indexing, transfers, mountpoint))?;
    }

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("protondrive".to_string()),
        MountOption::Subtype("protondrive".to_string()),
        MountOption::DefaultPermissions,
    ];

    // A crashed previous run can leave the kernel mount in place, which makes
    // the fresh mount below fail with EBUSY. Lazily detach any leftover first.
    clear_stale_mount(mountpoint);
    info!(mountpoint = %mountpoint.display(), "mounting Proton Drive");

    // Bind the control socket before the FUSE session takes over the thread. A
    // stale socket file from a previous run would block the bind, so clear it.
    let _ = std::fs::remove_file(control_socket);
    let old_umask = unsafe { libc::umask(0o77) };
    let listener_res = UnixListener::bind(control_socket);
    unsafe { libc::umask(old_umask) };
    let listener = listener_res?;
    // Owner-only before anything can connect: a peer on this socket commands the
    // daemon's authenticated session without a credential of its own (B6).
    if let Err(e) = pdfs_core::config::restrict_socket(control_socket) {
        error!(error = %e, "cannot restrict control socket permissions; refusing to serve");
        let _ = std::fs::remove_file(control_socket);
        return Err(std::io::Error::other(format!(
            "control socket permissions: {e}"
        )));
    }
    {
        let core = core.clone();
        let username = username.clone();
        let mountpoint = mountpoint.to_path_buf();
        std::thread::Builder::new()
            .name("pdfs-control".into())
            .spawn(move || run_control_socket(core, username, mountpoint, listener))?;
    }

    let fs = ProtonFs::new(core.clone(), root);
    // Warm the in-memory maps from the DB so a cold start serves previously
    // seen metadata without re-hitting the API (plan.md P1).
    core.hydrate();

    // Build the session explicitly (not `mount2`) so we can grab a `Notifier`
    // for the event task. `spawn` runs the session loop on its own background
    // thread; we then wait here for either a stop signal or the mount ending.
    let bg = Session::new(fs, mountpoint, &config)?.spawn()?;
    // The connection id, captured now while the mount is live, so a stop signal
    // mid-transfer can abort it rather than block `join` (see `abort_fuse_connection`).
    let main_conn = fuse_connection_id(mountpoint);
    let notifier = bg.notifier();
    // Same channel, kept on the `Core` so background work (a size upgrade, say)
    // can invalidate kernel-cached metadata without threading a handle through.
    let _ = core.notifier.set(notifier.clone());
    rt.spawn(run_event_sync(
        client,
        scope,
        core.state,
        core.cache,
        core.db,
        core.pending,
        notifier,
    ));

    // Stop signals (SIGTERM from `systemctl --user stop`, SIGINT from Ctrl-C)
    // are delivered onto the async runtime; bridge them onto a sync channel so
    // the loop below can react without blocking a worker thread. A bounded
    // channel of 1 is enough — we only need to know that *a* stop arrived.
    let (sig_tx, sig_rx) = std::sync::mpsc::sync_channel::<()>(1);
    rt.spawn(async move {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "install SIGTERM handler failed");
                    return;
                }
            };
        let mut sigint =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "install SIGINT handler failed");
                    return;
                }
            };
        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM"),
            _ = sigint.recv() => info!("received SIGINT"),
        }
        let _ = sig_tx.try_send(());
    });

    // Wait for whichever happens first: a stop signal (→ we unmount ourselves
    // via the lazy MNT_DETACH path, clean even mid-download), or the kernel
    // mount ending on its own (→ the session thread finishes). Poll instead of
    // blocking on `join` so we can also notice the signal.
    let outcome = loop {
        match sig_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(()) => {
                info!("stop requested; unmounting");
                if let Err(e) = umount_session_unblocked(bg, main_conn) {
                    warn!(error = %e, "umount_and_join failed");
                }
                break MountOutcome::Shutdown;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if bg.guard.is_finished() {
                    info!("mount ended externally");
                    if let Err(e) = bg.join() {
                        warn!(error = %e, "session join failed");
                    }
                    break MountOutcome::Unmounted;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Signal task gone (failed to install); fall back to join.
                let _ = bg.join();
                break MountOutcome::Unmounted;
            }
        }
    };

    // Unmount every on-demand sync folder too, or the kernel mounts linger as
    // stale and the next start fails with EBUSY (devices.md Phase 3).
    let secondaries: Vec<_> = core.mounts.lock().drain().collect();
    for (id, (session, conn)) in secondaries {
        if let Err(e) = umount_session_unblocked(session, conn) {
            warn!(id, error = %e, "unmount on-demand folder failed");
        }
    }

    let _ = std::fs::remove_file(control_socket);
    Ok(outcome)
}
