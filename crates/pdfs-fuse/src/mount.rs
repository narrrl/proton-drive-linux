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

/// A secondary (on-demand sync folder) FUSE session and the exact liveness flag
/// published with its forked inode state.
///
/// Keeping these together makes every teardown mark the fork unroutable before
/// unmounting, including paths that can block on an in-flight FUSE request.
pub(super) struct SecondaryMount {
    session: BackgroundSession,
    conn: Option<u32>,
    session_live: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SecondaryInsertRejection {
    Closed,
    Duplicate,
}

/// Secondary sessions and the daemon-wide gate controlling whether newly
/// spawned sessions may become visible. The gate and map share one lock so
/// shutdown cannot race an insertion between checking and publishing.
pub(super) struct SecondaryMountRegistry<T> {
    accepting: bool,
    entries: HashMap<i64, T>,
}

impl<T> Default for SecondaryMountRegistry<T> {
    fn default() -> Self {
        Self {
            accepting: false,
            entries: HashMap::new(),
        }
    }
}

impl<T> SecondaryMountRegistry<T> {
    pub(super) fn open(&mut self) {
        self.accepting = true;
    }

    pub(super) fn is_accepting(&self) -> bool {
        self.accepting
    }

    pub(super) fn close(&mut self) {
        self.accepting = false;
    }

    pub(super) fn insert(
        &mut self,
        id: i64,
        mount: T,
    ) -> Result<(), (SecondaryInsertRejection, T)> {
        if !self.accepting {
            return Err((SecondaryInsertRejection::Closed, mount));
        }
        if self.entries.contains_key(&id) {
            return Err((SecondaryInsertRejection::Duplicate, mount));
        }
        self.entries.insert(id, mount);
        Ok(())
    }

    pub(super) fn remove(&mut self, id: &i64) -> Option<T> {
        self.entries.remove(id)
    }

    pub(super) fn contains_key(&self, id: &i64) -> bool {
        self.entries.contains_key(id)
    }

    pub(super) fn close_and_drain(&mut self) -> Vec<(i64, T)> {
        self.close();
        self.entries.drain().collect()
    }
}

impl SecondaryMount {
    pub(super) fn new(
        session: BackgroundSession,
        conn: Option<u32>,
        session_live: Arc<AtomicBool>,
    ) -> Self {
        Self {
            session,
            conn,
            session_live,
        }
    }

    pub(super) fn teardown(self) -> std::io::Result<()> {
        teardown_session(&self.session_live, || {
            umount_session_unblocked(self.session, self.conn)
        })
    }
}

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

/// Per-mount settings resolved from [`pdfs_core::config::AppConfig`] by the
/// caller, so `mount` itself never reads config or the environment.
pub struct MountOptions {
    /// Proton account the session belongs to, for display and per-user paths.
    pub username: String,
    /// What the background conflict sweep may do. See `docs/BUGS.md` B71.
    pub sweep_mode: SweepMode,
}

/// Spawn one FUSE session rooted at an arbitrary remote node.
///
/// Primary My Files and secondary on-demand device locations use this same
/// construction path so mount options, stale-mount recovery, registry
/// liveness, and notifier ownership cannot drift apart. Each caller owns its
/// state/path registration and performs it exactly once before calling here.
pub(super) fn spawn_session(
    core: &Core,
    mountpoint: &Path,
    root: Node,
) -> std::io::Result<BackgroundSession> {
    let mut config = Config::default();
    // `n_threads` stays at fuser's default of 1 (with `clone_fd` off) on
    // purpose, for now. A concurrent dispatch loop is worth having, but
    // `release` currently relies on being serialised against the `open`+`read`
    // an application issues right after `close(2)` — see the comment on
    // `Filesystem::release`. Raising it before that ordering is made explicit
    // (a per-node staging-in-flight barrier) reintroduces the acceptance
    // suite's "concurrent file 1 mismatch". Kernel-side caps that do *not*
    // depend on that ordering are negotiated in `ProtonFs::init`.
    config.mount_options = vec![
        MountOption::FSName("protondrive".to_string()),
        MountOption::Subtype("protondrive".to_string()),
        MountOption::DefaultPermissions,
    ];

    clear_stale_mount(mountpoint);
    info!(mountpoint = %mountpoint.display(), "mounting Proton Drive location");
    let fs = ProtonFs::new(core.clone(), root);
    if core.primary {
        // Only My Files rehydrates the daemon-wide node cache. A secondary
        // inode space is rooted at one device subtree and must not import every
        // unrelated persisted node.
        core.hydrate();
    }
    let session = Session::new(fs, mountpoint, &config)?.spawn()?;
    let _ = core.notifier.set(session.notifier());
    core.session_live.store(true, Ordering::Release);
    Ok(session)
}

/// Make a session unroutable before beginning any teardown that may block.
fn teardown_session<T>(session_live: &AtomicBool, teardown: impl FnOnce() -> T) -> T {
    session_live.store(false, Ordering::Release);
    teardown()
}

impl Core {
    /// Resolve the primary share id away from startup and publish it only if the
    /// projected root has not changed while the request was in flight.
    pub(crate) fn repair_primary_share_id(&self, root_uid: &NodeUid) {
        match self.rt.block_on(self.client.context_share_id(root_uid)) {
            Ok(share_id) => {
                match self
                    .db
                    .mount_repair_my_files_share_id(&root_uid.to_string(), &share_id.to_string())
                {
                    Ok(true) => debug!(%root_uid, "repaired My Files share id"),
                    Ok(false) => {
                        debug!(%root_uid, "discarded stale My Files share-id result");
                    }
                    Err(error) => {
                        warn!(%root_uid, error = %error, "persist My Files share id failed");
                    }
                }
            }
            Err(error) => {
                debug!(%root_uid, error = %error, "resolve My Files share id failed");
            }
        }
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
    options: MountOptions,
) -> std::io::Result<MountOutcome> {
    let MountOptions {
        username,
        sweep_mode,
    } = options;
    let (root, online) = fetch_or_recall_root(&client, &rt, &db)?;
    let scope = root.tree_event_scope_id();
    db.mount_upsert_my_files(&mountpoint.to_string_lossy(), &root.uid.to_string(), None)
        .map_err(|error| std::io::Error::other(format!("project My Files mount: {error}")))?;
    let share_access = db
        .all_share_access()
        .map_err(|error| std::io::Error::other(format!("load shared access: {error}")))?;

    // The folder-sync engine (devices.md Phase 2) runs on its own thread and is
    // nudged over this channel; the sender lives in Core so control-socket
    // handlers can trigger reconciles.
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<sync::SyncMsg>();

    let core = Core {
        client: client.clone(),
        rt: rt.clone(),
        maintenance: Arc::new(Mutex::new(Default::default())),
        primary_root_uid: root.uid.clone(),
        primary: true,
        state: Arc::new(Mutex::new(State::new(db.clone(), share_access, 2))),
        cache: Arc::new(cache),
        readers: Arc::new(Mutex::new(HashMap::new())),
        block_ring: Arc::new(Mutex::new(BlockRing::default())),
        block_flight: Arc::new(BlockFlight::default()),
        content_repairs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        prefetch: Arc::new(Prefetch::default()),
        prefetch_budget: Arc::new(tokio::sync::Semaphore::new(PREFETCH_BUDGET)),
        workers: Arc::new(Workers::new(FUSE_WORKERS)?),
        db,
        shared_publication: Arc::new(Mutex::new(())),
        shared_generation: Arc::new(AtomicU64::new(0)),
        shared_refresh_deadlines: Arc::new(Mutex::new(SharedRefreshDeadlines::default())),
        online: Arc::new(AtomicBool::new(online)),
        pending: Arc::new(Mutex::new(HashMap::new())),
        hidden: Arc::new(Mutex::new(HashSet::new())),
        drain_wake: Arc::new((Mutex::new(false), Condvar::new())),
        shutdown: Arc::new(crate::shutdown::Shutdown::default()),
        upload_times: Arc::new(Mutex::new(HashMap::new())),
        upload_cancel: Arc::new(Mutex::new(HashMap::new())),
        timeline_refreshing: Arc::new(AtomicBool::new(false)),
        albums_refreshing: Arc::new(AtomicBool::new(false)),
        trash_refreshing: Arc::new(AtomicBool::new(false)),
        trash_progress: Arc::new(tokio::sync::Notify::new()),
        conflict_notified: Arc::new(Mutex::new(HashSet::new())),
        sweep_mode,
        own_sealed_revs: Arc::new(Mutex::new(HashMap::new())),
        self_changes: Arc::new(Mutex::new(HashMap::new())),
        thumb_gen: Arc::new(Mutex::new(HashSet::new())),
        thumb_gen_budget: Arc::new(tokio::sync::Semaphore::new(
            super::photos::THUMB_GEN_CONCURRENCY,
        )),
        file_thumb_generation: Arc::new(AtomicU64::new(0)),
        file_thumb_cancel: Arc::new(tokio::sync::Notify::new()),
        thumbnail_build: Arc::new(Mutex::new(Default::default())),
        thumbnail_build_cancelled: Arc::new(AtomicBool::new(false)),
        thumbnail_build_cancel: Arc::new(tokio::sync::Notify::new()),
        thumbnail_misses: Arc::new(Mutex::new(Default::default())),
        quota: Arc::new(Mutex::new(None)),
        size_upgrades: Arc::new(Mutex::new(HashMap::new())),
        size_waiters: Arc::new(Default::default()),
        notifier: Arc::new(OnceLock::new()),
        session_live: Arc::new(AtomicBool::new(false)),
        transfers: TransferRegistry::new(),
        indexing: Arc::new(AtomicBool::new(false)),
        sync_progress: Arc::new(Mutex::new(HashMap::new())),
        sync_tx,
        mounts: Arc::new(Mutex::new(SecondaryMountRegistry::default())),
        sync_locks: Arc::new(Mutex::new(HashMap::new())),
        states: Arc::new(StateRegistry::default()),
    };
    // Before anything can queue work against it: the drain thread below reaches
    // every mount's inode space through this registry, not through `core.state`.
    core.register_state(mountpoint);

    // Writes queued by a previous run (or left behind by a crash) are still owed
    // an upload, and reads must be served from their staged blobs until they land.
    core.hydrate_pending();
    // Then the writes that were fsynced but never closed, which the cache moved
    // aside at open. After `hydrate_pending`, so a recovered partial write can
    // see an already-queued write to the same node.
    core.recover_fsynced_writes();
    // And finally the staged blobs neither of those accounts for: bytes a
    // failed release left behind with nothing pointing at them.
    core.reconcile_staging();
    // Several drain workers over one queue. One thread meant a single large
    // upload held every queued rename, trash and small write behind it for as
    // long as it ran; the claim column (`Db::claim_next_due_op`) keeps the
    // workers off each other's nodes, which is the only ordering the queue
    // actually needs. Worker 0 additionally runs the queue's idle chores.
    //
    // Every long-lived thread started from here on is kept in `workers` and
    // joined at the end of this function. Leaving them running turned an
    // in-process remount into a second generation of drain workers, sync engines
    // and sweeps all operating on a mount that no longer exists (bugs.md B44).
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for worker in 0..DRAIN_WORKERS {
        let core = core.clone();
        workers.push(
            std::thread::Builder::new()
                .name(format!("pdfs-drain-{worker}"))
                .spawn(move || core.run_pending_drain(worker == 0))?,
        );
    }

    // Start the folder-sync engine. It watches every mirror folder, polls the
    // remotes, and reconciles on its own thread — never in front of a FUSE call.
    workers.extend(sync::spawn(core.clone(), sync_rx));

    // Reconcile leftover `(sync-conflict …)` copies: drop the ones that turned
    // out identical to the live file, surface the ones that genuinely diverge.
    // Its own thread — it enumerates and trashes, and must never block a FUSE call.
    // `Off` skips the thread entirely rather than starting an idle one, so the
    // setting is verifiable from the outside (`ls /proc/<pid>/task/*/comm`).
    if sweep_mode == SweepMode::Off {
        info!("conflict sweep disabled by configuration");
    } else {
        info!(mode = ?sweep_mode, "conflict sweep enabled");
        let core = core.clone();
        workers.push(
            std::thread::Builder::new()
                .name("pdfs-conflict-sweep".into())
                .spawn(move || core.run_conflict_sweep_loop())?,
        );
    }

    // Mounted from the cache: watch for the network coming back so the mount can
    // stop being read-only-ish without the user restarting the daemon.
    if !online {
        let core = core.clone();
        workers.push(
            std::thread::Builder::new()
                .name("pdfs-online-probe".into())
                .spawn(move || core.run_online_probe())?,
        );
    }

    // Keep the launcher prompt's "This computer" index fresh. Its own thread:
    // the walk is I/O-heavy and must never sit in front of a FUSE callback.
    {
        let db = core.db.clone();
        let indexing = core.indexing.clone();
        let transfers = core.transfers.clone();
        let mountpoint = mountpoint.to_path_buf();
        let shutdown = core.shutdown.clone();
        workers.push(
            std::thread::Builder::new()
                .name("pdfs-localindex".into())
                .spawn(move || run_local_index(db, indexing, transfers, mountpoint, shutdown))?,
        );
    }

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
    // Do not accept control requests until `spawn_session` has transitioned the
    // primary registration to live. The listener may queue a connection during
    // this short window, but its handler cannot observe `mounted = false`.
    let bg = match spawn_session(&core, mountpoint, root) {
        Ok(session) => session,
        Err(error) => {
            let _ = std::fs::remove_file(control_socket);
            return Err(error);
        }
    };
    core.mounts.lock().open();
    // The connection id, captured now while the mount is live, so a stop signal
    // mid-transfer can abort it rather than block `join` (see `abort_fuse_connection`).
    let main_conn = fuse_connection_id(mountpoint);
    {
        let control_core = core.clone();
        let username = username.clone();
        let mountpoint = mountpoint.to_path_buf();
        match std::thread::Builder::new()
            .name("pdfs-control".into())
            .spawn(move || run_control_socket(control_core, username, mountpoint, listener))
        {
            Ok(handle) => workers.push(handle),
            Err(error) => {
                // A failure here abandons the mount, so the background threads
                // started above have to be told to stop with it.
                stop_workers(&core, control_socket, workers);
                let secondaries = core.mounts.lock().close_and_drain();
                for (id, mount) in secondaries {
                    if let Err(unmount_error) = mount.teardown() {
                        warn!(id, error = %unmount_error, "secondary teardown after control startup failure failed");
                    }
                }
                if let Err(unmount_error) = teardown_session(&core.session_live, || {
                    umount_session_unblocked(bg, main_conn)
                }) {
                    warn!(error = %unmount_error, "unmount after control startup failure failed");
                }
                let _ = std::fs::remove_file(control_socket);
                return Err(error);
            }
        }
    }
    // Re-establish on-demand mounts only after the primary session is live.
    // Keep the worker so shutdown can close publication, wait for any in-flight
    // fetch to observe that closure, and then drain a stable registry.
    let restore_worker = {
        let restore_core = core.clone();
        match std::thread::Builder::new()
            .name("pdfs-restore-ondemand".into())
            .spawn(move || restore_core.restore_ondemand_mounts())
        {
            Ok(worker) => Some(worker),
            Err(error) => {
                warn!(%error, "start on-demand restore worker failed");
                None
            }
        }
    };
    rt.spawn(run_event_sync(client, scope, core.clone()));
    if online {
        let repair_core = core.clone();
        let root_uid = core.primary_root_uid.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("pdfs-share-id".into())
            .spawn(move || repair_core.repair_primary_share_id(&root_uid))
        {
            warn!(%error, "start My Files share-id repair failed");
        }
    }

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
                core.mounts.lock().close();
                if let Err(e) = teardown_session(&core.session_live, || {
                    umount_session_unblocked(bg, main_conn)
                }) {
                    warn!(error = %e, "umount_and_join failed");
                }
                break MountOutcome::Shutdown;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if bg.guard.is_finished() {
                    info!("mount ended externally");
                    core.mounts.lock().close();
                    if let Err(e) = teardown_session(&core.session_live, || bg.join()) {
                        warn!(error = %e, "session join failed");
                    }
                    break MountOutcome::Unmounted;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Signal task gone (failed to install); fall back to join.
                core.mounts.lock().close();
                let _ = teardown_session(&core.session_live, || bg.join());
                break MountOutcome::Unmounted;
            }
        }
    };

    // Unmount every on-demand sync folder too, or the kernel mounts linger as
    // stale and the next start fails with EBUSY (devices.md Phase 3).
    if let Some(worker) = restore_worker
        && worker.join().is_err()
    {
        warn!("on-demand restore worker panicked");
    }
    let secondaries = core.mounts.lock().close_and_drain();
    for (id, mount) in secondaries {
        if let Err(e) = mount.teardown() {
            warn!(id, error = %e, "unmount on-demand folder failed");
        }
    }

    // Last, because the workers may legitimately be finishing an upload, and
    // nothing they can still do is harmful once the mounts are down.
    stop_workers(&core, control_socket, workers);
    Ok(outcome)
}

/// Signal every background loop, wake the ones that are blocked, and join them.
///
/// The mount used to return with its drain workers, sync engine, sweep, online
/// probe, indexer and control listener all still running on `Core` clones. The
/// process usually exited straight afterwards and hid it; an in-process remount
/// did not, and started a second full set on top of the first (bugs.md B44).
///
/// Two of the waits need waking rather than merely flagging: the drain workers
/// block on their own condvar (`wake_drain`), and the control listener blocks
/// inside `accept`, which only returns when a connection arrives — so it gets
/// one, from us, after the flag is already set. A join that takes a while is
/// expected and correct: a worker part-way through an upload finishes it rather
/// than abandoning the user's bytes mid-flight.
fn stop_workers(core: &Core, control_socket: &Path, workers: Vec<std::thread::JoinHandle<()>>) {
    core.shutdown.stop();
    core.wake_drain();
    let _ = core.sync_tx.send(sync::SyncMsg::Stop);
    // Connect while the socket still exists — that is the whole point of the
    // poke — then unlink it so nothing else can arrive behind us. The listener
    // re-checks the flag after every accept, so even a connection that races the
    // unlink cannot put it back to sleep.
    let _ = std::os::unix::net::UnixStream::connect(control_socket);
    let _ = std::fs::remove_file(control_socket);
    for worker in workers {
        if worker.join().is_err() {
            warn!("a background worker panicked before shutdown");
        }
    }
    debug!("background workers stopped");
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{SecondaryInsertRejection, SecondaryMountRegistry, teardown_session};

    #[test]
    fn session_is_unroutable_before_teardown_starts() {
        let live = AtomicBool::new(true);
        teardown_session(&live, || {
            assert!(
                !live.load(Ordering::Acquire),
                "teardown must never run while the registry still reports mounted"
            );
        });
    }

    #[test]
    fn secondary_registry_rejects_closed_and_duplicate_insertions() {
        let mut mounts = SecondaryMountRegistry::default();
        assert_eq!(
            mounts.insert(7, "closed"),
            Err((SecondaryInsertRejection::Closed, "closed"))
        );

        mounts.open();
        assert_eq!(mounts.insert(7, "first"), Ok(()));
        assert_eq!(
            mounts.insert(7, "duplicate"),
            Err((SecondaryInsertRejection::Duplicate, "duplicate"))
        );
        assert!(mounts.contains_key(&7));

        assert_eq!(mounts.close_and_drain(), vec![(7, "first")]);
        assert!(!mounts.is_accepting());
    }
}
