//! Filesystem watcher, polling, and debounce control plane.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use pdfs_core::control::SyncEngineState;
use tracing::error;

use super::*;

/// How long the watcher must go quiet before a burst counts as finished.
///
/// Measured from the *last* event, not the first: applications do not write a
/// file once. An editor saves by writing a temp file and renaming it over the
/// target; an export or a database dump writes continuously for as long as it
/// takes. Settling a fixed interval after the first event walks the tree while
/// the file is still growing, and uploads a torn snapshot as a real revision —
/// corrected on the next pass, but only after spending the encrypt-and-upload
/// on a file that was never in that state.
const DEBOUNCE: Duration = Duration::from_secs(2);
/// Ceiling on the settle wait, so a directory under sustained change still
/// syncs. Without it, copying a large tree in — events arriving forever —
/// would postpone the reconcile for as long as the copy ran.
const MAX_SETTLE: Duration = Duration::from_secs(30);
/// How often to re-walk remotes so changes made on other clients are pulled in.
const POLL_INTERVAL: Duration = Duration::from_secs(120);
/// How many uploads/downloads/folder-creations a reconcile runs at once. The
/// per-item work is a network round-trip, so a folder with thousands of files is
/// unusable done one-at-a-time; this bounds concurrency like the bulk-upload
/// engine does.
pub(crate) const SYNC_CONCURRENCY: usize = 8;
/// How long after a failed watch the engine tries the watches again. Each try
/// walks the folder's whole tree, so not on every burst.
const REWATCH_RETRY: Duration = Duration::from_secs(600);

/// A message to the sync engine's control thread.
pub(crate) enum SyncMsg {
    /// Reconcile one folder by id (a local change, or a targeted `SyncNow`).
    Reconcile(i64),
    /// Reconcile every mirror folder (periodic poll, or startup).
    ReconcileAll,
    /// Re-read the folder set and adjust the filesystem watches (after add/remove).
    Rewatch,
    /// The daemon is tearing down: finish the current pass and end the engine
    /// thread, so the mount can join it (`docs/BUGS.md` B44).
    Stop,
}

/// Start the sync engine: a control thread that owns the filesystem watcher and
/// serialises reconcile passes, plus a periodic poll thread. The engine receives
/// on `rx`; senders live in [`Core::sync_tx`].
pub(crate) fn spawn(core: Core, rx: Receiver<SyncMsg>) -> Option<std::thread::JoinHandle<()>> {
    match std::thread::Builder::new()
        .name("pdfs-sync".into())
        .spawn(move || engine_loop(core, rx))
    {
        Ok(handle) => Some(handle),
        Err(e) => {
            warn!(error = %e, "failed to start sync engine");
            None
        }
    }
}

/// Whether the engine is alive and which pass it is on, for
/// [`Core::diagnostics`] and the supervisor.
///
/// A panic in a pass used to end the engine thread. Every later add, sync-now,
/// file event and poll then went into a channel nobody read, the folder stayed
/// `syncing`, and nothing said so until a restart (`docs/BUGS.md` B259).
#[derive(Default)]
pub(crate) struct EngineHealth {
    running: AtomicBool,
    pass: Mutex<Option<PassClock>>,
    panics: AtomicU64,
}

#[derive(Clone, Copy)]
struct PassClock {
    folder: i64,
    started: Instant,
    moved: Instant,
}

impl EngineHealth {
    /// Note that the running pass reported progress.
    pub(crate) fn moved(&self) {
        if let Some(pass) = self.pass.lock().as_mut() {
            pass.moved = Instant::now();
        }
    }

    /// The engine's state now. `try_lock`, like the rest of the diagnostics:
    /// a report must not wait on the thing it reports about.
    pub(crate) fn report(&self) -> SyncEngineState {
        let pass = self.pass.try_lock().and_then(|pass| *pass);
        SyncEngineState {
            running: self.running.load(Ordering::Relaxed),
            pass_folder: pass.map(|p| p.folder),
            pass_age_secs: pass.map_or(0, |p| p.started.elapsed().as_secs()),
            pass_idle_secs: pass.map_or(0, |p| p.moved.elapsed().as_secs()),
            panics: self.panics.load(Ordering::Relaxed),
        }
    }

    /// Run `pass` over `folder` as the current pass. Returns `false` when it
    /// panicked: the panic ends that pass, not the engine.
    pub(super) fn contain(&self, folder: i64, pass: impl FnOnce()) -> bool {
        let now = Instant::now();
        *self.pass.lock() = Some(PassClock {
            folder,
            started: now,
            moved: now,
        });
        let finished = std::panic::catch_unwind(AssertUnwindSafe(pass)).is_ok();
        *self.pass.lock() = None;
        if !finished {
            self.panics.fetch_add(1, Ordering::Relaxed);
        }
        finished
    }
}

/// Marks the engine running for as long as it lives.
pub(super) struct Running<'a>(&'a EngineHealth);

impl<'a> Running<'a> {
    pub(super) fn start(health: &'a EngineHealth) -> Self {
        health.running.store(true, Ordering::Relaxed);
        Self(health)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Relaxed);
        if std::thread::panicking() {
            error!("sync engine stopped by a panic; nothing syncs until the daemon restarts");
        }
    }
}

/// One folder's pass, contained: a panic in it sets the folder to `error` and
/// leaves the engine running for the next one.
fn run_pass(core: &Core, folder: &StoredSyncFolder) {
    if core
        .sync_engine
        .contain(folder.id, || core.reconcile_folder(folder))
    {
        return;
    }
    core.progress_end(folder.id);
    let _ = core
        .db
        .sync_folder_set_state(folder.id, "error", folder.last_sync);
    error!(
        id = folder.id,
        path = %folder.local_path,
        "sync: pass panicked; the folder is set to error and tried again on the next poll"
    );
    core.log_activity(
        ActivityKind::Sync,
        format!("sync failed for {}", base_name(&folder.local_path)),
        "an internal error stopped the pass; it is tried again on the next poll",
        false,
    );
}

/// The pass a `Reconcile(id)` asked for. A folder removed since, or one the
/// database cannot read, is said so rather than dropped in silence.
fn run_pass_by_id(core: &Core, id: i64) {
    match core.db.sync_folder_get(id) {
        Ok(Some(folder)) => run_pass(core, &folder),
        Ok(None) => debug!(id, "sync: folder removed before its pass; skipping"),
        Err(e) => warn!(id, error = ?e, "sync: cannot read folder; skipping its pass"),
    }
}

/// The engine control loop. Single-threaded so reconcile passes never overlap.
fn engine_loop(core: Core, rx: Receiver<SyncMsg>) {
    let health = core.sync_engine.clone();
    let _running = Running::start(&health);
    // Paths the watcher currently covers, mapped to their folder id, so an event
    // path can be resolved back to the folder it belongs to.
    let watched: Arc<Mutex<Vec<(PathBuf, i64)>>> = Arc::new(Mutex::new(Vec::new()));

    let tx_events = core.sync_tx.clone();
    let watched_events = watched.clone();
    let handler = move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(event) => event,
            // Running out of inotify watches for a new subfolder, say: that
            // subfolder's changes are then found by the poll only (B262).
            Err(e) => {
                warn!(error = %e, "sync: filesystem watcher error");
                return;
            }
        };
        // Ignore pure metadata/access noise; content changes are what matter.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        let msg = event_target(&watched_events.lock(), &event.paths);
        let _ = tx_events.send(msg);
    };
    // The local walk skips symlinks, so following them here would only watch
    // trees outside the folder, and run passes over it for changes made there.
    let config = notify::Config::default().with_follow_symlinks(false);
    let mut watcher = match notify::RecommendedWatcher::new(handler, config) {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "filesystem watcher unavailable; sync is poll-only");
            // Still run the loop so polling and explicit SyncNow work.
            poll_only_loop(&core, rx);
            return;
        }
    };

    // Periodic remote poll.
    {
        let tx = core.sync_tx.clone();
        let shutdown = core.shutdown.clone();
        std::thread::Builder::new()
            .name("pdfs-sync-poll".into())
            .spawn(move || {
                while shutdown.sleep(POLL_INTERVAL) {
                    if tx.send(SyncMsg::ReconcileAll).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }

    let mut unwatched = rewatch(&core, &mut watcher, &watched);
    let mut last_rewatch = Instant::now();
    run_burst(&core, &HashSet::new(), true);

    while let Ok(msg) = rx.recv() {
        let mut ids: HashSet<i64> = HashSet::new();
        let mut all = false;
        let mut do_rewatch = false;
        if !classify(msg, &mut ids, &mut all, &mut do_rewatch)
            || !settle(&rx, &mut ids, &mut all, &mut do_rewatch)
        {
            info!("sync engine stopping");
            return;
        }

        // A folder whose watch failed is tried again now and then, not only
        // when the folder set changes.
        if unwatched && last_rewatch.elapsed() >= REWATCH_RETRY {
            do_rewatch = true;
        }
        if do_rewatch {
            unwatched = rewatch(&core, &mut watcher, &watched);
            last_rewatch = Instant::now();
        }
        run_burst(&core, &ids, all);
    }
}

/// Fallback loop when no filesystem watcher could be created: poll + SyncNow only.
fn poll_only_loop(core: &Core, rx: Receiver<SyncMsg>) {
    {
        let tx = core.sync_tx.clone();
        let shutdown = core.shutdown.clone();
        std::thread::spawn(move || {
            while shutdown.sleep(POLL_INTERVAL) {
                if tx.send(SyncMsg::ReconcileAll).is_err() {
                    break;
                }
            }
        });
    }
    run_burst(core, &HashSet::new(), true);
    while let Ok(msg) = rx.recv() {
        let mut ids: HashSet<i64> = HashSet::new();
        let mut all = false;
        let mut do_rewatch = false;
        if !classify(msg, &mut ids, &mut all, &mut do_rewatch)
            || !settle(&rx, &mut ids, &mut all, &mut do_rewatch)
        {
            info!("sync engine stopping");
            return;
        }
        run_burst(core, &ids, all);
    }
}

/// Run the passes one burst asked for: the folders named by id, then, when the
/// burst also holds a `ReconcileAll`, every other folder.
///
/// The named folders go first. A poll's `ReconcileAll` in the same burst used
/// to take their place, and a folder just added, which has the highest id,
/// waited behind a full pass over every other folder (`docs/BUGS.md` B260).
///
/// Skipped while sync is paused, and cut short when a pause lands between
/// passes: the resume sends a fresh [`SyncMsg::ReconcileAll`], which walks
/// everything the watcher reported meanwhile.
fn run_burst(core: &Core, ids: &HashSet<i64>, all: bool) {
    if core.sync_paused() {
        debug!("sync: paused, skipping reconcile");
        return;
    }
    let folders = if all {
        match core.db.sync_folder_list() {
            Ok(folders) => folders,
            Err(e) => {
                warn!(error = ?e, "sync: cannot list folders");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let listed: Vec<i64> = folders.iter().map(|f| f.id).collect();
    for id in burst_order(ids, &listed) {
        if core.sync_paused() {
            break;
        }
        // A folder named by id is read again: it may have been added or
        // changed after the list was read.
        match folders.iter().find(|f| f.id == id) {
            Some(folder) if !ids.contains(&id) => run_pass(core, folder),
            _ => run_pass_by_id(core, id),
        }
    }
}

/// The order of a burst's passes: the folders named by id, lowest first, then
/// the rest of `listed` in its own order.
pub(super) fn burst_order(ids: &HashSet<i64>, listed: &[i64]) -> Vec<i64> {
    let mut order: Vec<i64> = ids.iter().copied().collect();
    order.sort_unstable();
    order.extend(listed.iter().copied().filter(|id| !ids.contains(id)));
    order
}

/// Absorb the rest of an event burst, returning once the watcher has been quiet
/// for `quiet` or `cap` has elapsed since the first event — whichever comes
/// first. The caller has already classified the event that opened the burst.
///
/// Split out with its timings as parameters so the settling behaviour can be
/// tested in milliseconds rather than in the tens of seconds the real constants
/// describe.
pub(super) fn settle_with(
    rx: &Receiver<SyncMsg>,
    ids: &mut HashSet<i64>,
    all: &mut bool,
    do_rewatch: &mut bool,
    quiet: Duration,
    cap: Duration,
) -> bool {
    let deadline = Instant::now() + cap;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // Never wait past the cap, however quiet the watcher goes.
        let wait = quiet.min(deadline - now);
        match rx.recv_timeout(wait) {
            // Another event: the burst is still going, so the quiet window
            // starts again from here.
            // A stop lands mid-burst: give up the rest of the burst rather
            // than settling for it, and let the caller end the loop.
            Ok(m) => {
                if !classify(m, ids, all, do_rewatch) {
                    return false;
                }
            }
            // Quiet for a full window: whatever was being written has stopped.
            Err(RecvTimeoutError::Timeout) => break,
            // Every sender is gone; the caller's loop is about to end anyway.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    true
}

/// [`settle_with`] at the real timings.
fn settle(
    rx: &Receiver<SyncMsg>,
    ids: &mut HashSet<i64>,
    all: &mut bool,
    do_rewatch: &mut bool,
) -> bool {
    settle_with(rx, ids, all, do_rewatch, DEBOUNCE, MAX_SETTLE)
}

/// Fold one message into the pending work of the current burst. Returns `false`
/// for [`SyncMsg::Stop`], which is not work but an instruction to end the loop.
pub(super) fn classify(
    msg: SyncMsg,
    ids: &mut HashSet<i64>,
    all: &mut bool,
    rewatch: &mut bool,
) -> bool {
    match msg {
        SyncMsg::Reconcile(id) => {
            ids.insert(id);
        }
        SyncMsg::ReconcileAll => *all = true,
        SyncMsg::Rewatch => *rewatch = true,
        SyncMsg::Stop => return false,
    }
    true
}

/// The reconcile a watcher event asks for: the one mirror folder all its paths
/// lie in, so an edit in one folder does not walk every other one too. An event
/// that spans folders, or that no watched folder claims (a watch list a moment
/// out of date), falls back to every folder.
pub(super) fn event_target(watched: &[(PathBuf, i64)], paths: &[PathBuf]) -> SyncMsg {
    let mut ids = paths.iter().map(|path| {
        watched
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            // The deepest root, should one mirror folder sit inside another.
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, id)| *id)
    });
    match ids.next() {
        Some(Some(first)) if ids.all(|id| id == Some(first)) => SyncMsg::Reconcile(first),
        _ => SyncMsg::ReconcileAll,
    }
}

/// Bring the filesystem watches in line with the current mirror-folder set:
/// watch newly-added folders, drop removed ones. Returns whether a folder is
/// left unwatched because its watch failed.
///
/// The watch calls run on a copy, outside the lock: the watcher's event
/// callback takes the same lock, and a backend that hands a watch request to
/// its event thread and waits for the answer would deadlock against it.
fn rewatch(
    core: &Core,
    watcher: &mut notify::RecommendedWatcher,
    watched: &Mutex<Vec<(PathBuf, i64)>>,
) -> bool {
    let want: Vec<(PathBuf, i64)> = match core.db.sync_folder_list() {
        Ok(f) => f
            .into_iter()
            .filter(|f| f.mode == "mirror")
            .map(|f| (PathBuf::from(f.local_path), f.id))
            .collect(),
        Err(e) => {
            warn!(error = ?e, "sync: cannot list folders to watch");
            return false;
        }
    };
    let mut have = watched.lock().clone();
    // Drop watches no longer wanted.
    for path in keep_wanted(&mut have, &want) {
        let _ = watcher.unwatch(&path);
    }
    // Add newly wanted watches.
    let mut failed = false;
    for (path, id) in &want {
        if have.iter().any(|(p, _)| p == path) {
            continue;
        }
        match watcher.watch(path, RecursiveMode::Recursive) {
            Ok(()) => have.push((path.clone(), *id)),
            Err(e) => {
                // A recursive watch that fails partway keeps the watches it had
                // added. Their events would match no folder and run a pass over
                // every one, and they hold watches the next folder needs (B262).
                let _ = watcher.unwatch(path);
                failed = true;
                warn!(
                    path = %path.display(),
                    error = %e,
                    "sync: watch failed; the poll finds this folder's changes until a retry \
                     works. A low fs.inotify.max_user_watches is the usual cause"
                );
            }
        }
    }
    *watched.lock() = have;
    failed
}

/// Keep the watches in `have` that `want` still asks for, and return the paths
/// of the others, to unwatch.
///
/// A path is matched alone, but takes the id `want` gives it: a folder removed
/// and added again in one burst keeps its watch, and its events go to the new
/// row rather than to the removed one (`docs/BUGS.md` B272).
pub(super) fn keep_wanted(have: &mut Vec<(PathBuf, i64)>, want: &[(PathBuf, i64)]) -> Vec<PathBuf> {
    let mut dropped = Vec::new();
    have.retain_mut(|(path, id)| match want.iter().find(|(p, _)| p == path) {
        Some((_, wanted)) => {
            *id = *wanted;
            true
        }
        None => {
            dropped.push(path.clone());
            false
        }
    });
    dropped
}

// ---- reconcile ------------------------------------------------------------
