//! Filesystem watcher, polling, and debounce control plane.

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
pub(super) const SYNC_CONCURRENCY: usize = 8;

/// A message to the sync engine's control thread.
pub(crate) enum SyncMsg {
    /// Reconcile one folder by id (a local change, or a targeted `SyncNow`).
    Reconcile(i64),
    /// Reconcile every mirror folder (periodic poll, or startup).
    ReconcileAll,
    /// Re-read the folder set and adjust the filesystem watches (after add/remove).
    Rewatch,
}

/// Start the sync engine: a control thread that owns the filesystem watcher and
/// serialises reconcile passes, plus a periodic poll thread. The engine receives
/// on `rx`; senders live in [`Core::sync_tx`].
pub(crate) fn spawn(core: Core, rx: Receiver<SyncMsg>) {
    if let Err(e) = std::thread::Builder::new()
        .name("pdfs-sync".into())
        .spawn(move || engine_loop(core, rx))
    {
        warn!(error = %e, "failed to start sync engine");
    }
}

/// The engine control loop. Single-threaded so reconcile passes never overlap.
fn engine_loop(core: Core, rx: Receiver<SyncMsg>) {
    // Paths the watcher currently covers, mapped to their folder id, so an event
    // path can be resolved back to the folder it belongs to.
    let watched: Mutex<Vec<(PathBuf, i64)>> = Mutex::new(Vec::new());

    let tx_events = core.sync_tx.clone();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // Ignore pure metadata/access noise; content changes are what matter.
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let _ = tx_events.send(SyncMsg::ReconcileAll);
        }) {
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
        std::thread::Builder::new()
            .name("pdfs-sync-poll".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(POLL_INTERVAL);
                    if tx.send(SyncMsg::ReconcileAll).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }

    rewatch(&core, &mut watcher, &watched);
    reconcile_all(&core);

    while let Ok(msg) = rx.recv() {
        let mut ids: HashSet<i64> = HashSet::new();
        let mut all = false;
        let mut do_rewatch = false;
        classify(msg, &mut ids, &mut all, &mut do_rewatch);
        settle(&rx, &mut ids, &mut all, &mut do_rewatch);

        if do_rewatch {
            rewatch(&core, &mut watcher, &watched);
        }
        if all {
            reconcile_all(&core);
        } else {
            for id in ids {
                if let Ok(Some(folder)) = core.db.sync_folder_get(id) {
                    core.reconcile_folder(&folder);
                }
            }
        }
    }
}

/// Fallback loop when no filesystem watcher could be created: poll + SyncNow only.
fn poll_only_loop(core: &Core, rx: Receiver<SyncMsg>) {
    {
        let tx = core.sync_tx.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL_INTERVAL);
                if tx.send(SyncMsg::ReconcileAll).is_err() {
                    break;
                }
            }
        });
    }
    reconcile_all(core);
    while let Ok(msg) = rx.recv() {
        let mut ids: HashSet<i64> = HashSet::new();
        let mut all = false;
        let mut do_rewatch = false;
        classify(msg, &mut ids, &mut all, &mut do_rewatch);
        settle(&rx, &mut ids, &mut all, &mut do_rewatch);
        if all {
            reconcile_all(core);
        } else {
            for id in ids {
                if let Ok(Some(folder)) = core.db.sync_folder_get(id) {
                    core.reconcile_folder(&folder);
                }
            }
        }
    }
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
) {
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
            Ok(m) => classify(m, ids, all, do_rewatch),
            // Quiet for a full window: whatever was being written has stopped.
            Err(RecvTimeoutError::Timeout) => break,
            // Every sender is gone; the caller's loop is about to end anyway.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// [`settle_with`] at the real timings.
fn settle(rx: &Receiver<SyncMsg>, ids: &mut HashSet<i64>, all: &mut bool, do_rewatch: &mut bool) {
    settle_with(rx, ids, all, do_rewatch, DEBOUNCE, MAX_SETTLE);
}

pub(super) fn classify(msg: SyncMsg, ids: &mut HashSet<i64>, all: &mut bool, rewatch: &mut bool) {
    match msg {
        SyncMsg::Reconcile(id) => {
            ids.insert(id);
        }
        SyncMsg::ReconcileAll => *all = true,
        SyncMsg::Rewatch => *rewatch = true,
    }
}

/// Reconcile every mirror folder in turn.
fn reconcile_all(core: &Core) {
    let folders = match core.db.sync_folder_list() {
        Ok(f) => f,
        Err(e) => {
            warn!(error = ?e, "sync: cannot list folders");
            return;
        }
    };
    for folder in folders {
        core.reconcile_folder(&folder);
    }
}

/// Bring the filesystem watches in line with the current mirror-folder set:
/// watch newly-added folders, drop removed ones.
fn rewatch(
    core: &Core,
    watcher: &mut notify::RecommendedWatcher,
    watched: &Mutex<Vec<(PathBuf, i64)>>,
) {
    let want: Vec<(PathBuf, i64)> = match core.db.sync_folder_list() {
        Ok(f) => f
            .into_iter()
            .filter(|f| f.mode == "mirror")
            .map(|f| (PathBuf::from(f.local_path), f.id))
            .collect(),
        Err(e) => {
            warn!(error = ?e, "sync: cannot list folders to watch");
            return;
        }
    };
    let mut have = watched.lock();
    // Drop watches no longer wanted.
    have.retain(|(path, _)| {
        if want.iter().any(|(p, _)| p == path) {
            true
        } else {
            let _ = watcher.unwatch(path);
            false
        }
    });
    // Add newly wanted watches.
    for (path, id) in &want {
        if have.iter().any(|(p, _)| p == path) {
            continue;
        }
        match watcher.watch(path, RecursiveMode::Recursive) {
            Ok(()) => have.push((path.clone(), *id)),
            Err(e) => warn!(path = %path.display(), error = %e, "sync: watch failed"),
        }
    }
}

// ---- reconcile ------------------------------------------------------------
