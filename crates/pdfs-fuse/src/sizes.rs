//! File sizes filled in after a cheap listing: one batch per folder, the
//! `getattr`s and `lookup`s that need a real size parked on it, and the
//! results applied to the tree as each chunk lands (`docs/BUGS.md` B14).

use super::*;

/// The size upgrades running now, and the replies parked on them. Shared
/// by every mount: the bound that matters is per daemon.
#[derive(Clone)]
pub(crate) struct SizeState {
    /// Folders whose listing was enumerated cheaply and is having its file sizes
    /// filled in right now, so a burst of `stat`s over a fresh listing starts one
    /// upgrade rather than one per entry. See [`Core::spawn_size_upgrade`].
    /// Size upgrades currently running, per folder inode, across every mount. A `getattr` that
    /// needs a real size waits on the entry rather than issuing its own fetch —
    /// `ls -l` of a folder is one `getattr` per file, and they must collapse
    /// onto a single batch (`docs/BUGS.md` B14).
    pub(crate) size_upgrades: Arc<Mutex<HashMap<u64, Arc<SizeUpgrade>>>>,
    /// Replies parked on those upgrades. Shared across every mount for the same
    /// reason the pool is: the bound that matters is per daemon.
    pub(crate) size_waiters: Arc<SizeWaitQueue>,
}

impl SizeState {
    pub(crate) fn new() -> Self {
        Self {
            size_upgrades: Arc::new(Mutex::new(HashMap::new())),
            size_waiters: Arc::new(Default::default()),
        }
    }
}

impl Core {
    /// Resolve the real size of every file in `parent`'s listing still carrying
    /// a provisional one, returning when they are in `state`.
    ///
    /// Called when a `stat` lands on such a file. That covers the paths
    /// [`Core::ensure_children`] cannot: a listing rebuilt from the DB, or one
    /// restored by [`Core::hydrate`] on mount, whose rows were persisted before
    /// an earlier upgrade had a chance to run. Gathers the whole folder rather
    /// than the one file asked for, because a `stat` of one entry in a listing
    /// almost always means a `stat` of all of them.
    pub(super) fn upgrade_sizes_for_parent(
        &self,
        ino: u64,
        uid: &NodeUid,
        parent: u64,
    ) -> Option<Arc<SizeUpgrade>> {
        let provisional = |e: &Entry| {
            matches!(
                &e.node.kind,
                NodeKind::File {
                    claimed_size: None,
                    ..
                }
            )
        };
        let (key, mut missing): (u64, Vec<NodeUid>) = {
            let st = self.state();
            match st.children.get(&parent) {
                // The listing is resident: batch the whole folder under its
                // inode, so the rest of an `ls -l` rides along on this fetch.
                Some(kids) => (
                    parent,
                    kids.iter()
                        .filter_map(|k| st.entries.get(k))
                        .filter(|e| provisional(e))
                        .map(|e| e.uid.clone())
                        .collect(),
                ),
                // It is not, and returning here is what let a provisional size
                // reach the caller anyway: a rename invalidates its parents'
                // listings, so a freshly renamed file always landed in this
                // branch. Resolve the single node instead, keyed by its own
                // inode — folder and file inodes share one space, so the two
                // single-flight keys cannot collide. (Same shape as B4: an
                // early return that assumed the hot cache was authoritative.)
                None => (ino, Vec::new()),
            }
        };
        // The node that was actually asked about is never optional, whichever
        // branch produced the batch.
        if !missing.iter().any(|u| u == uid) {
            missing.push(uid.clone());
        }
        // Returned, not awaited here: the caller is a `getattr` that must not
        // answer with a provisional size (`docs/BUGS.md` B14), and it parks its reply
        // on this batch rather than holding a thread until it lands.
        self.upgrade_sizes(key, missing)
    }

    /// Fill in the true sizes of files a `Light` enumeration returned without
    /// one, on a worker, after the listing has already been served.
    ///
    /// This is the other half of the split in [`Core::ensure_children`]. The
    /// listing itself needs only names and parentage, so it is served from the
    /// cheap enumeration immediately; the S2K-per-file work that produces
    /// `claimed_size` happens here, off the path the user is waiting on.
    ///
    /// **Sizes are provisional until this lands.** `node_size` falls back to
    /// `total_size_on_storage`, the *ciphertext* size, which is slightly larger
    /// than the real one. A read must not plan on it: since B84 a size the
    /// revision disagrees with fails the read, so `read` settles the size first
    /// ([`Core::resolve_provisional_size`], `docs/BUGS.md` B100). In `stat` it is a
    /// discrepancy that closes within a round trip, not a repeat of B11 (which
    /// reported **0** and so suppressed reads entirely).
    ///
    /// Single-flight per folder: a `stat` of every entry in a fresh listing is
    /// the normal case, and each one must not start its own upgrade.
    pub(super) fn spawn_size_upgrade(&self, folder_ino: u64, uids: Vec<NodeUid>) {
        self.upgrade_sizes(folder_ino, uids);
    }

    /// Start (or join) the size upgrade for `key`, returning the batch a caller
    /// can wait on — `None` when there was nothing to upgrade.
    ///
    /// Single-flight per `key`. The fetch runs on its own thread rather than on
    /// the [`Workers`] pool: callers wait on `Lane::Meta`, so a batch queued onto
    /// that lane could have a wide enough `ls -l` fill it with threads waiting
    /// for a job that can never be scheduled. `Lane::Transfer` would swap that
    /// for starvation behind bulk reads. One short-lived thread per folder,
    /// bounded by the single-flight, avoids both.
    pub(super) fn upgrade_sizes(&self, key: u64, uids: Vec<NodeUid>) -> Option<Arc<SizeUpgrade>> {
        if uids.is_empty() {
            return None;
        }
        let claim = claim_size_upgrade(&mut self.sizes.size_upgrades.lock(), key, uids);
        match claim {
            SizeUpgradeClaim::Joined(slot) | SizeUpgradeClaim::Queued(slot) => Some(slot),
            SizeUpgradeClaim::Start(slot, uids) => {
                let core = self.clone();
                let worker = slot.clone();
                if let Err(error) = self.services.spawn_job("pdfs-size-upgrade", move || {
                    core.run_size_upgrade(key, uids, worker)
                }) {
                    // The waiters give up on the slot at their deadline and
                    // answer with the size they have.
                    warn!(%error, "could not start a size upgrade");
                }
                Some(slot)
            }
        }
    }

    /// Hold `respond` back until `ino` has a real size, `slot`'s batch ends, or
    /// [`SizeUpgrade::WAIT`] elapses — then run it, on the queue's thread.
    ///
    /// The caller returns immediately. That is the whole point: the thread it
    /// was running on is a FUSE dispatch thread or a [`Workers`] one, and both
    /// are needed by everything else on the mount (audit F4).
    pub(super) fn await_size(
        &self,
        ino: u64,
        slot: Arc<SizeUpgrade>,
        respond: impl FnOnce() + Send + 'static,
    ) {
        let core = self.clone();
        let waiter = SizeWaiter {
            slot,
            resolved: Box::new(move || core.size_is_real(ino)),
            deadline: Instant::now() + SizeUpgrade::WAIT,
            respond: Box::new(respond),
        };
        if !self.sizes.size_waiters.park(waiter) {
            return;
        }
        let queue = self.sizes.size_waiters.clone();
        if let Err(error) = self
            .services
            .spawn_job("pdfs-size-waiters", move || queue.serve())
        {
            // Without a thread nothing would ever answer these. A provisional
            // size is wrong; a `stat` that never returns wedges the caller.
            warn!(%error, "no thread for parked size waiters; answering provisionally");
            self.sizes.size_waiters.answer_all();
        }
    }

    /// Fetch `uids` in chunks, applying and announcing each one as it lands.
    ///
    /// Chunked so a waiter is released as soon as *its* file is resolved. A
    /// single 793-node batch took ~80 s, which outran [`SizeUpgrade::WAIT`] and
    /// put provisional sizes back in front of callers — the bug this was
    /// supposed to fix (`docs/BUGS.md` B14).
    ///
    /// When the batch ends, the thread takes over the oldest batch the cap held
    /// back, so at most [`MAX_SIZE_UPGRADES`] threads run however many folders
    /// a recursive walk touches (`docs/BUGS.md` B100).
    pub(super) fn run_size_upgrade(&self, key: u64, uids: Vec<NodeUid>, slot: Arc<SizeUpgrade>) {
        let (mut key, mut uids, mut slot) = (key, uids, slot);
        loop {
            for chunk in uids.chunks(SIZE_UPGRADE_CHUNK) {
                // A stop joins this job; the rest of the folder can wait for
                // the next mount, which lists it afresh.
                if self.shutdown.is_stopping() {
                    break;
                }
                let result =
                    self.block_on_within(BATCH_CALL_TIMEOUT, self.drive.enumerate_nodes(chunk));
                self.apply_size_upgrade(key, result);
                slot.chunk_done();
            }
            // Both of these must happen however the loop ended: a folder whose
            // upgrade failed has to be retryable, and its waiters released.
            let next = next_size_upgrade(&mut self.sizes.size_upgrades.lock(), key);
            slot.finish();
            let Some(next) = next else {
                return;
            };
            (key, uids, slot) = next;
        }
    }

    /// Settle the size of a file whose listing only carries the provisional
    /// one, before a read plans its blocks on it or a write handle starts from
    /// it. Returns the real size, or `None` when the fetch did not produce one.
    ///
    /// A provisional size is the ciphertext size. The read path checks the
    /// size it is given against the revision's own block table and fails the
    /// read on a mismatch (B84), so reading on the provisional size is an `EIO`
    /// every time. `getattr` usually settles the size first, but not when its
    /// wait times out, so the read cannot rely on it (`docs/BUGS.md` B100).
    pub(super) fn resolve_provisional_size(&self, ino: u64, uid: &NodeUid) -> Option<u64> {
        let parent = self.state().entries.get(&ino)?.parent;
        // Without a network the answer is `None` either way; the bound keeps a
        // dead link from holding the open or read for the SDK's two minutes.
        if !self.is_online() {
            return None;
        }
        let result = self.block_on_bounded(self.drive.enumerate_nodes(std::slice::from_ref(uid)));
        if let Err(e) = &result {
            self.lost_link(e, "settle a provisional size");
        }
        self.apply_size_upgrade(parent, result);
        let st = self.state();
        match &st.entries.get(&ino)?.node.kind {
            NodeKind::File {
                claimed_size: Some(size),
                ..
            } => Some((*size).max(0) as u64),
            _ => None,
        }
    }

    /// Wait, up to [`SizeUpgrade::WAIT`] as a `stat` would, for the real size
    /// of every file in `ino`'s listing that still carries a provisional one.
    ///
    /// For callers that read a whole listing's sizes straight from `state` and
    /// do not run on a FUSE thread: `pdfs ls` and the app's file browser. A
    /// listing just fetched has the ciphertext size of every file in it until
    /// its upgrade lands, and they showed those (`docs/BUGS.md` B115).
    pub(super) fn settle_listing_sizes(&self, ino: u64) {
        // Offline the fetch cannot succeed; the wait would only delay the
        // answer it ends up giving anyway.
        if !self.is_online() {
            return;
        }
        let missing: Vec<NodeUid> = {
            let st = self.state();
            st.children
                .get(&ino)
                .map(|kids| {
                    kids.iter()
                        .filter_map(|k| st.entries.get(k))
                        .filter(|e| {
                            !is_local_uid(&e.uid)
                                && matches!(
                                    &e.node.kind,
                                    NodeKind::File {
                                        claimed_size: None,
                                        ..
                                    }
                                )
                        })
                        .map(|e| e.uid.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        if let Some(slot) = self.upgrade_sizes(ino, missing) {
            slot.wait_finished(Instant::now() + SizeUpgrade::WAIT);
        }
    }

    /// Whether `ino` has a real size — the condition a waiter is waiting on.
    /// A node that vanished counts as resolved; there is nothing left to wait
    /// for, and its caller will find the `ENOENT` for itself.
    pub(super) fn size_is_real(&self, ino: u64) -> bool {
        let st = self.state();
        st.entries.get(&ino).is_none_or(|e| {
            !matches!(
                &e.node.kind,
                NodeKind::File {
                    claimed_size: None,
                    ..
                }
            )
        })
    }

    /// Adopt the sizes a completed upgrade fetched. Split from
    /// [`Core::upgrade_sizes`] so the single-flight bookkeeping there has one
    /// exit path rather than one per early return.
    pub(super) fn apply_size_upgrade(
        &self,
        folder_ino: u64,
        result: Result<Vec<Node>, ProtonError>,
    ) {
        let core = self;
        {
            let mut nodes = match result {
                Ok(nodes) => nodes,
                Err(e) => {
                    // Not fatal: the listing stands, sizes just stay provisional
                    // until something invalidates and re-enumerates it.
                    warn!(folder_ino, error = %e, "size upgrade failed; sizes stay provisional");
                    return;
                }
            };
            // A queued write is ahead of anything the server can report, so its
            // optimistic size must survive this just as it survives a re-listing
            // (B11).
            core.stamp_pending_sizes(&mut nodes);
            let mut changed: Vec<u64> = Vec::new();
            let mut st = core.state();
            for node in nodes {
                // Only adopt the size. Re-interning wholesale would also adopt a
                // name or parent that a rename/move may have changed locally
                // while this was in flight, undoing it.
                let Some(&ino) = st.by_uid.get(&node.uid) else {
                    continue;
                };
                changed.push(ino);
                // An open write handle is ahead of the server too (B109).
                if st.active_writes.contains_key(&ino) {
                    continue;
                }
                let NodeKind::File { claimed_size, .. } = &node.kind else {
                    continue;
                };
                let (Some(size), Some(entry)) = (*claimed_size, st.entries.get_mut(&ino)) else {
                    continue;
                };
                if let NodeKind::File { claimed_size, .. } = &mut entry.node.kind {
                    *claimed_size = Some(size);
                }
            }
            let updated: Vec<Node> = st
                .children
                .get(&folder_ino)
                .map(|kids| {
                    kids.iter()
                        .filter_map(|k| st.entries.get(k).map(|e| e.node.clone()))
                        .collect()
                })
                .unwrap_or_default();
            drop(st);
            if let Err(e) = core.db.upsert_nodes(&updated) {
                warn!(folder_ino, error = %e, "persisting upgraded sizes failed");
            }
            // Without this the corrected size is invisible for the length of the
            // attr TTL: the kernel answers `stat` from the provisional attrs it
            // cached while the listing was being served, so `ls -l` reports the
            // ciphertext size for up to 30 s even though the daemon has had the
            // real one all along. Notify *after* the DB write, so a re-`getattr`
            // provoked by the invalidation cannot race the persistence.
            //
            // Attributes only (offset -1): the content did not change, and a
            // page-cache invalidation deadlocks when this runs inside a `read`
            // (B100 resolves sizes there). The kernel holds the page lock of
            // that read until we reply, and the invalidation waits for the
            // same lock, so the read never answered (docs/BUGS.md B110).
            for mount in core.states.live() {
                if let Some(notifier) = mount.notifier.get() {
                    for &ino in &changed {
                        let _ = notifier.inval_attrs(ino);
                    }
                }
            }
            debug!(folder_ino, files = updated.len(), "filled in listing sizes");
        }
    }
}

/// A size upgrade in flight for one folder, so the `getattr`s that need its
/// result wait for the one batch instead of each fetching its own node.
///
/// A plain `Condvar` rather than a channel: the waiters do not want a value,
/// only the edge, and there may be hundreds of them for one folder.
#[derive(Default)]
pub(super) struct Progress {
    /// The whole batch has been applied (or failed). Nothing more is coming.
    done: bool,
    /// Bumped every time a chunk lands, so a waiter can tell "no progress since
    /// I last looked" from "progress happened while I was looking".
    generation: u64,
}

/// A size upgrade in flight for one folder, so the `getattr`s and `lookup`s that
/// need its result wait for the one batch instead of each fetching its own node.
///
/// Waiters are released **per chunk**, not once at the end: a waiter only cares
/// about its own file, and waiting for the other 792 is what let a large folder
/// outrun the timeout (`docs/BUGS.md` B14).
#[derive(Default)]
pub(super) struct SizeUpgrade {
    inner: Mutex<Progress>,
    ready: Condvar,
    /// The batch, while it waits for a thread because [`MAX_SIZE_UPGRADES`]
    /// were already running, with when it was queued so the oldest goes first.
    /// `None` once a thread has it.
    queued: Mutex<Option<(Instant, Vec<NodeUid>)>>,
}

/// What [`Core::upgrade_sizes`] does with one folder's batch.
pub(super) enum SizeUpgradeClaim {
    /// A batch for this folder is already running or queued; wait on it.
    Joined(Arc<SizeUpgrade>),
    /// The cap is reached. The batch waits for a running thread to take it.
    Queued(Arc<SizeUpgrade>),
    /// Start a thread for the batch.
    Start(Arc<SizeUpgrade>, Vec<NodeUid>),
}

/// Register the batch for `key` in `in_flight` and decide who runs it.
///
/// Past the cap this used to return nothing, and the caller answered with the
/// provisional size at once. Nothing retried the folder until the next `stat`,
/// so a recursive walk over more than [`MAX_SIZE_UPGRADES`] fresh folders showed
/// ciphertext sizes for most of them (`docs/BUGS.md` B100). Queued, the batch is still
/// fetched, and a waiter gets the real size unless [`SizeUpgrade::WAIT`] runs out.
pub(super) fn claim_size_upgrade(
    in_flight: &mut HashMap<u64, Arc<SizeUpgrade>>,
    key: u64,
    uids: Vec<NodeUid>,
) -> SizeUpgradeClaim {
    if let Some(existing) = in_flight.get(&key) {
        return SizeUpgradeClaim::Joined(existing.clone());
    }
    let running = in_flight
        .values()
        .filter(|slot| slot.queued.lock().is_none())
        .count();
    let slot = Arc::new(SizeUpgrade::default());
    in_flight.insert(key, slot.clone());
    if running >= MAX_SIZE_UPGRADES {
        *slot.queued.lock() = Some((Instant::now(), uids));
        return SizeUpgradeClaim::Queued(slot);
    }
    SizeUpgradeClaim::Start(slot, uids)
}

/// Retire the finished batch for `done` and hand its thread the oldest queued
/// batch, if there is one.
pub(super) fn next_size_upgrade(
    in_flight: &mut HashMap<u64, Arc<SizeUpgrade>>,
    done: u64,
) -> Option<(u64, Vec<NodeUid>, Arc<SizeUpgrade>)> {
    in_flight.remove(&done);
    let (key, slot) = in_flight
        .iter()
        .filter_map(|(key, slot)| slot.queued.lock().as_ref().map(|(at, _)| (*at, *key, slot)))
        .min_by_key(|(at, _, _)| *at)
        .map(|(_, key, slot)| (key, slot.clone()))?;
    let (_, uids) = slot.queued.lock().take()?;
    Some((key, uids, slot))
}

impl SizeUpgrade {
    /// How long a caller will wait for a real size before answering with the
    /// provisional one.
    ///
    /// A `stat` that never returns is far worse than one that is briefly wrong:
    /// on timeout the caller falls back to the pre-fix behaviour rather than
    /// wedging whatever is listing the directory.
    const WAIT: Duration = Duration::from_secs(10);

    /// Whether the batch has ended, however it ended. A waiter that sees this
    /// has nothing left to wait for.
    fn is_finished(&self) -> bool {
        self.inner.lock().done
    }

    /// Announce that a chunk has been applied. Every waiter re-checks its own
    /// node; the ones it resolved return, the rest go back to sleep.
    fn chunk_done(&self) {
        self.inner.lock().generation += 1;
        self.ready.notify_all();
    }

    /// Block until the batch has ended or `deadline` has passed. Only for a
    /// caller that is not on a FUSE or [`Workers`] thread; those park their
    /// reply with [`Core::await_size`] instead.
    fn wait_finished(&self, deadline: Instant) {
        let mut progress = self.inner.lock();
        while !progress.done {
            if self.ready.wait_until(&mut progress, deadline).timed_out() {
                return;
            }
        }
    }

    /// Release every waiter for good. The worker must reach this on all paths.
    fn finish(&self) {
        let mut progress = self.inner.lock();
        progress.done = true;
        progress.generation += 1;
        drop(progress);
        self.ready.notify_all();
    }
}

/// How often [`SizeWaitQueue::serve`] re-tests its parked waiters.
///
/// The predicates read `state`, which the applying thread writes under its own
/// lock, so this polls rather than waiting on each slot's condvar: one thread
/// cannot wait on a hundred condvars, and the whole point of the queue is that
/// there is only one thread. Twenty milliseconds is far below the round trip a
/// waiter is waiting on and only ticks while something is actually parked.
pub(super) const SIZE_WAIT_POLL: Duration = Duration::from_millis(20);

/// A reply held back until a size upgrade resolves the node it describes.
pub(super) struct SizeWaiter {
    /// The batch this reply is waiting on. When it ends, so does the wait.
    slot: Arc<SizeUpgrade>,
    /// Whether *this* waiter's node now has a real size.
    ///
    /// Called with no queue lock held: it reaches into `state`, and the thread
    /// applying a chunk holds `state` before it announces the chunk — taking
    /// them in the other order would close the cycle.
    resolved: Box<dyn Fn() -> bool + Send>,
    /// When to answer with the provisional size anyway. A `stat` that never
    /// returns is far worse than one that is briefly wrong.
    deadline: Instant,
    /// Sends the FUSE reply. Runs on the serving thread.
    respond: Box<dyn FnOnce() + Send>,
}

#[derive(Default)]
pub(super) struct Parked {
    waiters: Vec<SizeWaiter>,
    /// Whether a thread is currently serving `waiters`. The thread clears this
    /// as it retires, so the next park starts a new one.
    running: bool,
}

/// Every reply parked on a size upgrade, served by at most one thread.
///
/// This exists because waiting used to happen on the [`Workers`] pool. Sizes
/// are single-flighted per folder, so an `ls -l` across N cold folders parks a
/// waiter per entry — and each of those held a pool thread asleep on a condvar
/// for up to [`SizeUpgrade::WAIT`]. With enough folders that is every thread in
/// both lanes, including the ones reserved for metadata precisely so that this
/// could not happen (audit F4). Parking the `Reply` instead costs one thread
/// for the whole mount, and only while something is parked.
#[derive(Default)]
pub(super) struct SizeWaitQueue {
    parked: Mutex<Parked>,
    /// Woken when a waiter is added, so a serving thread re-computes how long
    /// it may sleep.
    added: Condvar,
}

impl SizeWaitQueue {
    /// Park `waiter`. Returns whether the caller must start a serving thread.
    fn park(&self, waiter: SizeWaiter) -> bool {
        let mut parked = self.parked.lock();
        parked.waiters.push(waiter);
        if parked.running {
            drop(parked);
            self.added.notify_all();
            return false;
        }
        parked.running = true;
        true
    }

    /// Answer every waiter whose node has resolved, whose batch has ended, or
    /// whose deadline has passed; sleep until the next one might. Returns when
    /// nothing is parked any more.
    fn serve(&self) {
        loop {
            // Taken out of the lock wholesale: the predicates and the replies
            // both take locks of their own, and none of them may be called
            // while this one is held.
            let taken = std::mem::take(&mut self.parked.lock().waiters);
            let now = Instant::now();
            let mut keep = Vec::with_capacity(taken.len());
            for waiter in taken {
                if now >= waiter.deadline || waiter.slot.is_finished() || (waiter.resolved)() {
                    (waiter.respond)();
                } else {
                    keep.push(waiter);
                }
            }
            let mut parked = self.parked.lock();
            parked.waiters.append(&mut keep);
            if parked.waiters.is_empty() {
                parked.running = false;
                return;
            }
            let until = parked
                .waiters
                .iter()
                .map(|w| w.deadline)
                .min()
                .unwrap_or(now)
                .min(Instant::now() + SIZE_WAIT_POLL);
            self.added.wait_until(&mut parked, until);
        }
    }

    /// Answer everything parked, immediately, and retire the serving thread.
    /// Used when there is no thread to serve with: a provisional size now beats
    /// a reply that never comes.
    fn answer_all(&self) {
        let taken = {
            let mut parked = self.parked.lock();
            parked.running = false;
            std::mem::take(&mut parked.waiters)
        };
        for waiter in taken {
            (waiter.respond)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LinkId, MAX_SIZE_UPGRADES, NodeUid, SizeUpgrade, SizeUpgradeClaim, SizeWaitQueue,
        SizeWaiter, VolumeId, claim_size_upgrade, next_size_upgrade,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Park a waiter the way [`Core::await_size`] does, and hand back the
    /// channel its reply is sent down, so a test can time the release.
    fn park(
        queue: &Arc<SizeWaitQueue>,
        slot: Arc<SizeUpgrade>,
        resolved: impl Fn() -> bool + Send + 'static,
        deadline: Instant,
    ) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let waiter = SizeWaiter {
            slot,
            resolved: Box::new(resolved),
            deadline,
            respond: Box::new(move || {
                let _ = tx.send(());
            }),
        };
        if queue.park(waiter) {
            let queue = queue.clone();
            std::thread::spawn(move || queue.serve());
        }
        rx
    }

    /// Long enough that a test never trips the deadline by accident, short
    /// enough that one which is *meant* to does not take ten seconds.
    fn far_off() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    /// The point of the per-chunk design: a waiter is answered as soon as *its
    /// own* file is resolved, without waiting for the rest of the folder. A
    /// single batch over 793 nodes took ~80 s, which outran the timeout and
    /// handed back the provisional size this is all meant to prevent.
    #[test]
    fn a_waiter_returns_on_the_chunk_that_resolves_it() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        let mine = Arc::new(AtomicBool::new(false));
        let worker = slot.clone();
        let flag = mine.clone();
        let t = std::thread::spawn(move || {
            // Our node lands in the first chunk; two more follow.
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::SeqCst);
            worker.chunk_done();
            std::thread::sleep(Duration::from_millis(300));
            worker.chunk_done();
            worker.finish();
        });
        let started = Instant::now();
        let rx = park(&queue, slot, move || mine.load(Ordering::SeqCst), far_off());
        rx.recv().expect("the waiter is answered");
        // Released by the first chunk, not the last.
        assert!(started.elapsed() < Duration::from_millis(200));
        t.join().unwrap();
    }

    /// `pdfs ls` waits for the whole batch, so it lists no ciphertext size
    /// (`docs/BUGS.md` B115), but no longer than a `stat` would.
    #[test]
    fn a_listing_waits_for_the_whole_size_batch_but_not_forever() {
        let slot = Arc::new(SizeUpgrade::default());
        let worker = slot.clone();
        // Read before the spawn: the worker's sleeps can start before this
        // thread runs again, which made the bound below fail on a busy runner.
        let started = Instant::now();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            worker.chunk_done();
            std::thread::sleep(Duration::from_millis(60));
            worker.finish();
        });
        slot.wait_finished(started + Duration::from_secs(5));
        // Past the first chunk, up to the end of the batch.
        assert!(started.elapsed() >= Duration::from_millis(90));
        assert!(slot.is_finished());
        t.join().unwrap();

        let stuck = SizeUpgrade::default();
        let started = Instant::now();
        stuck.wait_finished(started + Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!stuck.is_finished());
    }

    /// A chunk that does not resolve this waiter must not answer it.
    #[test]
    fn an_unrelated_chunk_does_not_release_a_waiter() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        let worker = slot.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            worker.chunk_done();
            std::thread::sleep(Duration::from_millis(120));
            worker.finish();
        });
        let started = Instant::now();
        // Never resolved: only `finish` can end this wait.
        let rx = park(&queue, slot, || false, far_off());
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() >= Duration::from_millis(140));
        t.join().unwrap();
    }

    /// A batch that ends without resolving the node — a failed fetch — still
    /// answers its waiters, who fall back to the provisional size.
    #[test]
    fn finish_releases_a_waiter_that_was_never_resolved() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        let worker = slot.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            worker.finish();
        });
        let started = Instant::now();
        let rx = park(&queue, slot, || false, far_off());
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() < SizeUpgrade::WAIT);
        t.join().unwrap();
    }

    /// Already resolved before parking: answered on the first pass. This is the
    /// follower that arrives after the chunk it needed has landed.
    #[test]
    fn an_already_resolved_waiter_does_not_block() {
        let queue = Arc::new(SizeWaitQueue::default());
        let started = Instant::now();
        let rx = park(&queue, Arc::new(SizeUpgrade::default()), || true, far_off());
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// Finishing before anyone parks must not strand the late arrival — the
    /// flag is what is checked, not the notification, which it would miss.
    #[test]
    fn waiting_after_finish_returns_at_once() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        slot.finish();
        let started = Instant::now();
        let rx = park(&queue, slot, || false, far_off());
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// Every waiter is answered, not just the first: one `ls -l` puts one
    /// waiter per file on the same folder.
    #[test]
    fn all_waiters_are_released() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        let waiters: Vec<_> = (0..8)
            .map(|_| park(&queue, slot.clone(), || false, far_off()))
            .collect();
        std::thread::sleep(Duration::from_millis(20));
        slot.finish();
        for rx in waiters {
            rx.recv().expect("every waiter is answered");
        }
    }

    /// The deadline is the backstop: a batch that never finishes must not wedge
    /// the caller's `stat` forever.
    #[test]
    fn a_waiter_gives_up_at_the_deadline() {
        let queue = Arc::new(SizeWaitQueue::default());
        let started = Instant::now();
        // Nothing will ever resolve or finish this.
        let rx = park(
            &queue,
            Arc::new(SizeUpgrade::default()),
            || false,
            Instant::now() + Duration::from_millis(120),
        );
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() >= Duration::from_millis(120));
    }

    /// The serving thread retires when nothing is parked, and a later park
    /// starts a new one — otherwise the second `ls -l` of the session would
    /// hang forever.
    #[test]
    fn the_queue_serves_again_after_it_has_drained() {
        let queue = Arc::new(SizeWaitQueue::default());
        for _ in 0..3 {
            let rx = park(&queue, Arc::new(SizeUpgrade::default()), || true, far_off());
            rx.recv().expect("the waiter is answered");
        }
    }

    /// With no thread to serve them, waiters are answered on the spot rather
    /// than never: a provisional size beats a `stat` that does not return.
    #[test]
    fn answer_all_releases_everything_parked() {
        let queue = SizeWaitQueue::default();
        let (tx, rx) = mpsc::channel();
        for _ in 0..4 {
            let tx = tx.clone();
            queue.park(SizeWaiter {
                slot: Arc::new(SizeUpgrade::default()),
                resolved: Box::new(|| false),
                deadline: far_off(),
                respond: Box::new(move || {
                    let _ = tx.send(());
                }),
            });
        }
        drop(tx);
        queue.answer_all();
        assert_eq!(rx.into_iter().count(), 4);
    }

    fn uids(links: &[&str]) -> Vec<NodeUid> {
        links
            .iter()
            .map(|link| NodeUid::new(VolumeId::from("vol"), LinkId::from(*link)))
            .collect()
    }

    /// Past the cap a folder used to get no upgrade at all, so a recursive
    /// walk reported ciphertext sizes for most of what it listed (`docs/BUGS.md` B100).
    #[test]
    fn a_folder_past_the_cap_is_queued_not_dropped() {
        let mut in_flight = HashMap::new();
        for key in 0..MAX_SIZE_UPGRADES as u64 {
            let claim = claim_size_upgrade(&mut in_flight, key, uids(&["a"]));
            assert!(matches!(claim, SizeUpgradeClaim::Start(..)));
        }
        let key = MAX_SIZE_UPGRADES as u64;
        let claim = claim_size_upgrade(&mut in_flight, key, uids(&["late"]));
        let SizeUpgradeClaim::Queued(slot) = claim else {
            panic!("expected the batch to be queued");
        };
        assert!(Arc::ptr_eq(&slot, &in_flight[&key]));
        // A second caller for the queued folder waits on the same batch.
        let again = claim_size_upgrade(&mut in_flight, key, uids(&["late"]));
        assert!(matches!(again, SizeUpgradeClaim::Joined(s) if Arc::ptr_eq(&s, &slot)));
    }

    #[test]
    fn a_finished_batch_hands_its_thread_the_oldest_queued_one() {
        let mut in_flight = HashMap::new();
        for key in 0..MAX_SIZE_UPGRADES as u64 {
            claim_size_upgrade(&mut in_flight, key, uids(&["a"]));
        }
        let first = MAX_SIZE_UPGRADES as u64;
        claim_size_upgrade(&mut in_flight, first, uids(&["first"]));
        std::thread::sleep(Duration::from_millis(2));
        claim_size_upgrade(&mut in_flight, first + 1, uids(&["second"]));

        let (key, taken, slot) = next_size_upgrade(&mut in_flight, 0).expect("a queued batch");
        assert_eq!((key, taken), (first, uids(&["first"])));
        assert!(slot.queued.lock().is_none());
        assert!(!in_flight.contains_key(&0));
        // Still at the cap: the handed-over batch counts as running.
        let running = in_flight
            .values()
            .filter(|s| s.queued.lock().is_none())
            .count();
        assert_eq!(running, MAX_SIZE_UPGRADES);

        let (key, _, _) = next_size_upgrade(&mut in_flight, 1).expect("the other one");
        assert_eq!(key, first + 1);
        assert!(next_size_upgrade(&mut in_flight, 2).is_none());
    }
}
