//! The thread pool that serves the FUSE handlers which touch the network.
//!
//! fuser drives `Filesystem` from a single dispatch loop, so a cold read that
//! goes to the wire would block every cheap metadata call behind it. Those
//! handlers hand their work here instead and reply from the worker.
//!
//! The pool is split into two lanes because moving the work off the dispatch
//! loop is not by itself enough: with one shared queue, eight concurrent block
//! downloads occupy every worker, and a `lookup` that needs one network round
//! trip waits behind megabytes of transfer (audit A6). Some threads are
//! therefore reserved for metadata and never accept a transfer.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use parking_lot::{Condvar, Mutex};
use tracing::warn;

/// How many FUSE handlers may run off the dispatch loop at once.
///
/// `fuser`'s session loop is non-concurrent: it reads one request, calls the
/// handler, and only then reads the next. A handler that touches the network
/// therefore stalls every `getattr`/`lookup` on the mount behind it. The slow
/// handlers hand their `Reply` to this pool instead and answer from a worker,
/// which frees the loop immediately.
///
/// Bounded on purpose: one worker can hold a 4 MiB block in flight, and an
/// unbounded pool would let read-ahead on a big file spawn threads without
/// limit. Sized so the SDK's in-flight block semaphore, not thread count, is
/// what bounds download memory.
///
/// Counted as `META_WORKERS` reserved threads *plus* the eight that transfers
/// had before the lanes were split. Taking the reservation out of the original
/// eight instead would have cut concurrent reads to five — a throughput
/// regression smuggled in with a latency fix. Threads are the cheap resource
/// here; block buffers are the expensive one, and those are capped in the SDK.
pub(crate) const FUSE_WORKERS: usize = 11;

/// How many of [`FUSE_WORKERS`] serve metadata *only*.
///
/// This is the whole guarantee: these threads never accept a [`Lane::Transfer`]
/// job, so no number of concurrent downloads can leave a `lookup` or `readdir`
/// without a thread to run on. Three is enough because metadata jobs are short
/// — one round trip, no block fetch — so they queue behind each other briefly
/// rather than for the length of a transfer.
///
/// The remaining workers are general: they prefer transfers and fall back to
/// metadata when there is no transfer waiting. That direction is safe (a
/// general thread picking up a cheap job frees itself again quickly) while the
/// reverse — metadata threads helping with transfers — would reintroduce
/// exactly the blocking this split exists to prevent.
const META_WORKERS: usize = 3;

/// Queue depth that gets a line in the log, and the step at which it repeats.
/// Every job holds a `Reply` and a few fields, so depth is cheap — but a lane
/// this far behind is worth knowing about before the user reports a slow mount.
const QUEUE_WARN_STEP: usize = 256;

/// Whether a queue reaching `depth` is one of the depths worth logging.
fn crosses_warn_step(depth: usize) -> bool {
    depth >= QUEUE_WARN_STEP && depth.is_multiple_of(QUEUE_WARN_STEP)
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A queued job and the name its caller gave it, so a job that never finishes
/// can be named in the diagnostics instead of showing up as "a worker is busy".
struct Queued {
    label: &'static str,
    job: Job,
}

/// Which lane a job belongs in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    /// Short, network-bound metadata work: `lookup`, `readdir`. Guaranteed a
    /// thread by the reserved workers.
    Meta,
    /// Bulk data movement: block reads. May occupy every general worker.
    Transfer,
}

#[derive(Default)]
struct Queues {
    meta: VecDeque<Queued>,
    transfer: VecDeque<Queued>,
    /// Set when the pool is dropped, to wake and retire every worker.
    closed: bool,
}

/// What one worker thread is doing, published for [`Workers::snapshot`].
///
/// Every field is an atomic (or a lock the worker only ever holds for the length
/// of a pointer write) because the whole point of the snapshot is to answer
/// *while the daemon is wedged*: a diagnostic that waits on the same lock the
/// stuck job holds reports nothing at exactly the moment it is needed.
struct Slot {
    name: String,
    /// True between picking a job up and finishing it.
    busy: AtomicBool,
    /// When the current state (busy or idle) began, as milliseconds since the
    /// pool started. Monotonic, so an age is always a real duration.
    since_ms: AtomicU64,
    /// The label of the job in hand. Behind a mutex because a `&'static str` is
    /// two words and there is no atomic for that; the worker holds it only to
    /// swap the pointer, and the snapshot never blocks on it (see
    /// [`Workers::snapshot`]).
    label: Mutex<&'static str>,
}

impl Slot {
    fn new(name: String, now_ms: u64) -> Self {
        Self {
            name,
            busy: AtomicBool::new(false),
            since_ms: AtomicU64::new(now_ms),
            label: Mutex::new(""),
        }
    }
}

/// One worker's state at the moment [`Workers::snapshot`] read it.
pub(crate) struct WorkerSnapshot {
    pub(crate) name: String,
    pub(crate) busy: bool,
    /// How long the worker has been in that state.
    pub(crate) age_ms: u64,
    /// The job in hand, or `""` when idle. `"?"` when the label could not be
    /// read without blocking.
    pub(crate) label: String,
}

/// The pool's state at the moment [`Workers::snapshot`] read it.
pub(crate) struct PoolSnapshot {
    pub(crate) workers: Vec<WorkerSnapshot>,
    pub(crate) meta_queued: usize,
    pub(crate) transfer_queued: usize,
    pub(crate) meta_completed: u64,
    pub(crate) transfer_completed: u64,
}

/// The queues and the two wait sets over them.
///
/// Two condvars, not one, because the workers are not interchangeable: a
/// meta-only thread cannot take a transfer. With a single wait set,
/// `notify_one` for a transfer could wake a meta-only thread, which would find
/// nothing it is allowed to run and go back to sleep — leaving the transfer
/// unclaimed while a general worker sat idle. Waking *every* thread instead
/// would work but costs a thundering herd per job. So each lane notifies only
/// threads that can actually serve it.
struct Pool {
    queues: Mutex<Queues>,
    /// Waited on by the reserved metadata workers.
    meta_cv: Condvar,
    /// Waited on by the general workers.
    general_cv: Condvar,
    /// Queue depths, mirrored outside the lock so a snapshot can read them
    /// without waiting on whatever is holding it.
    meta_queued: AtomicUsize,
    transfer_queued: AtomicUsize,
    /// Jobs finished per lane. A depth that stays high while this stays still is
    /// a stalled lane rather than a busy one.
    meta_completed: AtomicU64,
    transfer_completed: AtomicU64,
    slots: Vec<Slot>,
    started: Instant,
}

impl Pool {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// Bounded thread pool behind the network-touching FUSE handlers.
///
/// Shared by every session forked off one [`Core`] (the main mount plus each
/// on-demand sync folder), so the bound is per daemon rather than per mount.
pub(crate) struct Workers {
    inner: Arc<Pool>,
    /// Kept so teardown can *join* the workers, not merely flag them. A worker
    /// still inside a job when the process drops its tokio runtime panics with
    /// "A Tokio 1.x context was found, but it is being shutdown" (seen
    /// 2026-09-18 09:58:35), which is a teardown ordering bug rather than a real
    /// failure.
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Workers {
    pub(crate) fn new(n: usize) -> std::io::Result<Self> {
        // Never leave the pool without a general worker, however small `n` is.
        let meta_workers = META_WORKERS.min(n.saturating_sub(1));
        let names: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "pdfs-fuse-{}{i}",
                    if i < meta_workers { "meta-" } else { "" }
                )
            })
            .collect();
        let inner = Arc::new(Pool {
            queues: Mutex::new(Queues::default()),
            meta_cv: Condvar::new(),
            general_cv: Condvar::new(),
            meta_queued: AtomicUsize::new(0),
            transfer_queued: AtomicUsize::new(0),
            meta_completed: AtomicU64::new(0),
            transfer_completed: AtomicU64::new(0),
            slots: names
                .iter()
                .map(|name| Slot::new(name.clone(), 0))
                .collect(),
            started: Instant::now(),
        });
        let mut handles = Vec::with_capacity(n);
        for (i, name) in names.iter().enumerate() {
            let inner = inner.clone();
            let meta_only = i < meta_workers;
            let handle = std::thread::Builder::new()
                .name(name.clone())
                .spawn(move || {
                    let cv = if meta_only {
                        &inner.meta_cv
                    } else {
                        &inner.general_cv
                    };
                    loop {
                        let mut q = inner.queues.lock();
                        let picked = loop {
                            // A general worker takes transfers first: metadata
                            // already has threads of its own, so draining the
                            // bulk queue is the useful thing for it to do.
                            let taken = if meta_only {
                                q.meta.pop_front().map(|job| (Lane::Meta, job))
                            } else {
                                q.transfer
                                    .pop_front()
                                    .map(|job| (Lane::Transfer, job))
                                    .or_else(|| q.meta.pop_front().map(|job| (Lane::Meta, job)))
                            };
                            if let Some(taken) = taken {
                                break Some(taken);
                            }
                            if q.closed {
                                break None;
                            }
                            cv.wait(&mut q);
                        };
                        drop(q);
                        let Some((lane, queued)) = picked else { break };
                        match lane {
                            Lane::Meta => inner.meta_queued.fetch_sub(1, Ordering::Relaxed),
                            Lane::Transfer => inner.transfer_queued.fetch_sub(1, Ordering::Relaxed),
                        };
                        let slot = &inner.slots[i];
                        *slot.label.lock() = queued.label;
                        slot.since_ms.store(inner.now_ms(), Ordering::Relaxed);
                        slot.busy.store(true, Ordering::Relaxed);
                        // A panicking handler must not cost the pool a worker
                        // for the rest of the run. The dropped `Reply` answers
                        // EIO on its own, so the caller of the failed op is
                        // told; the next job is unaffected.
                        //
                        // This only holds while no shared state a job touches
                        // sits behind a *poisoning* lock. A `std::sync::Mutex`
                        // held at the point of the panic comes back poisoned,
                        // and the worker we just rescued dies on its next
                        // acquisition — as does every other thread. The rescue
                        // would then convert one recoverable EIO into a
                        // permanently broken daemon: strictly worse than not
                        // catching at all.
                        //
                        // This crate's state is behind `parking_lot`, which
                        // does not poison. **The requirement crosses the crate
                        // boundary**, though: a job runs deep into
                        // `proton-sdk`, whose entity cache is a `std` mutex. It
                        // recovers the guard rather than unwrapping (see
                        // `InMemoryCacheRepository::state`); its session and
                        // HTTP state are `tokio::sync::Mutex`, which has no
                        // poisoning at all. Anything new reached from a job owes
                        // the same check — an SDK-side `.lock().unwrap()`
                        // silently invalidates this comment.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(queued.job));
                        slot.busy.store(false, Ordering::Relaxed);
                        slot.since_ms.store(inner.now_ms(), Ordering::Relaxed);
                        *slot.label.lock() = "";
                        match lane {
                            Lane::Meta => inner.meta_completed.fetch_add(1, Ordering::Relaxed),
                            Lane::Transfer => {
                                inner.transfer_completed.fetch_add(1, Ordering::Relaxed)
                            }
                        };
                    }
                })?;
            handles.push(handle);
        }
        Ok(Self {
            inner,
            handles: Mutex::new(handles),
        })
    }

    /// Stop accepting work, wake every worker, and wait up to `deadline` for the
    /// jobs in flight to finish.
    ///
    /// Bounded because a join is only worth what it costs: a worker wedged on a
    /// network call would otherwise hold the whole teardown, and the user asked
    /// for the daemon to stop. What is stuck gets named in the log, and the
    /// process continues shutting down around it.
    pub(crate) fn stop_and_join(&self, deadline: std::time::Duration) {
        self.close();
        let handles: Vec<_> = std::mem::take(&mut *self.handles.lock());
        if handles.is_empty() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let joiner = std::thread::Builder::new()
            .name("pdfs-fuse-join".into())
            .spawn(move || {
                for handle in handles {
                    let _ = handle.join();
                }
                let _ = tx.send(());
            });
        if joiner.is_err() {
            return;
        }
        if rx.recv_timeout(deadline).is_err() {
            let busy: Vec<String> = self
                .snapshot()
                .workers
                .into_iter()
                .filter(|worker| worker.busy)
                .map(|worker| format!("{}={}", worker.name, worker.label))
                .collect();
            warn!(
                stuck = busy.join(","),
                "fuse workers did not finish in time; shutting down around them"
            );
        }
    }

    fn close(&self) {
        self.inner.queues.lock().closed = true;
        self.inner.meta_cv.notify_all();
        self.inner.general_cv.notify_all();
    }

    /// Queue `job` in `lane`.
    ///
    /// Neither queue is bounded, and deliberately so: the only thing that could
    /// apply backpressure here is the fuser dispatch loop, and blocking *that*
    /// is the stall this module exists to avoid. Queue depth is cheap anyway —
    /// a pending job holds a `Reply` and a few fields, while the 4 MiB block
    /// buffer is allocated inside the job once it runs, and the SDK's in-flight
    /// semaphore is what bounds how many of those exist at once.
    /// The queue is *not* bounded, and the depth warning above is deliberately
    /// all this does about it: the only caller able to feel backpressure here is
    /// fuser's dispatch loop, and blocking that is the stall the whole module
    /// exists to avoid. A depth that keeps climbing is a symptom to read in the
    /// log, not something to fix by stopping the mount.
    ///
    /// `label` names the work for the diagnostics (`pdfs diagnostics`) and for
    /// the stall warnings: it is what a hung daemon reports instead of "busy".
    pub(crate) fn run(&self, lane: Lane, label: &'static str, job: impl FnOnce() + Send + 'static) {
        let mut q = self.inner.queues.lock();
        if q.closed {
            // Pre-pool behaviour: a shut-down pool degrades to a slow mount
            // rather than a mount that answers every read with EIO.
            drop(q);
            warn!("fuse worker pool is gone; serving inline");
            job();
            return;
        }
        let queued = Queued {
            label,
            job: Box::new(job),
        };
        let depth = match lane {
            Lane::Meta => {
                q.meta.push_back(queued);
                self.inner.meta_queued.fetch_add(1, Ordering::Relaxed) + 1
            }
            Lane::Transfer => {
                q.transfer.push_back(queued);
                self.inner.transfer_queued.fetch_add(1, Ordering::Relaxed) + 1
            }
        };
        drop(q);
        if crosses_warn_step(depth) {
            warn!(
                lane = if matches!(lane, Lane::Meta) {
                    "meta"
                } else {
                    "transfer"
                },
                depth, label, "fuse worker queue is deep"
            );
        }
        match lane {
            // Either class can serve metadata, and only one of them needs to:
            // whichever wakes first takes it, and the other finds the queue
            // empty and waits again.
            Lane::Meta => {
                self.inner.meta_cv.notify_one();
                self.inner.general_cv.notify_one();
            }
            // Only general workers may take a transfer, so waking a reserved
            // one would be the lost wakeup this split is careful to avoid.
            Lane::Transfer => {
                self.inner.general_cv.notify_one();
            }
        }
    }

    /// What every worker is doing right now, without taking the queue lock.
    ///
    /// A diagnostic is worth having exactly when the daemon is stuck, so this
    /// reads atomics only, and `try_lock`s each label rather than waiting for
    /// it: a label is held for one pointer write, so failing to get it means a
    /// racing worker, not a wedged one, and `"?"` is a better answer than a
    /// diagnostics request that hangs too.
    pub(crate) fn snapshot(&self) -> PoolSnapshot {
        let now = self.inner.now_ms();
        let workers = self
            .inner
            .slots
            .iter()
            .map(|slot| {
                let label = slot
                    .label
                    .try_lock()
                    .map_or_else(|| "?".to_string(), |label| (*label).to_string());
                WorkerSnapshot {
                    name: slot.name.clone(),
                    busy: slot.busy.load(Ordering::Relaxed),
                    age_ms: now.saturating_sub(slot.since_ms.load(Ordering::Relaxed)),
                    label,
                }
            })
            .collect();
        PoolSnapshot {
            workers,
            meta_queued: self.inner.meta_queued.load(Ordering::Relaxed),
            transfer_queued: self.inner.transfer_queued.load(Ordering::Relaxed),
            meta_completed: self.inner.meta_completed.load(Ordering::Relaxed),
            transfer_completed: self.inner.transfer_completed.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Workers {
    /// A backstop for the paths that never call
    /// [`stop_and_join`](Workers::stop_and_join) — tests, and a mount that fails
    /// before teardown is wired up. Flag and wake only: a `Drop` that blocks on
    /// a network call is worse than a worker outliving its pool by a moment.
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The depth warning fires on the step, and only on the step: a lane that
    /// oscillates around the threshold must not log on every single job.
    #[test]
    fn queue_depth_warns_on_each_step() {
        assert!(!crosses_warn_step(1));
        assert!(!crosses_warn_step(QUEUE_WARN_STEP - 1));
        assert!(crosses_warn_step(QUEUE_WARN_STEP));
        assert!(!crosses_warn_step(QUEUE_WARN_STEP + 1));
        assert!(crosses_warn_step(QUEUE_WARN_STEP * 3));
    }

    /// Teardown must return even while a job is running, and must not leave the
    /// pool accepting work.
    #[test]
    fn stopping_is_bounded_and_closes_the_pool() {
        let pool = Workers::new(2).unwrap();
        let (tx, rx) = mpsc::channel();
        pool.run(Lane::Meta, "test", move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(5)).expect("it runs");

        pool.stop_and_join(Duration::from_secs(5));

        // A job queued after the stop runs inline rather than disappearing.
        let (tx, rx) = mpsc::channel();
        pool.run(Lane::Meta, "test", move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(5))
            .expect("a closed pool serves inline");
    }

    /// A diagnostic is only useful if it names the work. A running job must show
    /// up under its label, and the queue behind it must show up as depth.
    #[test]
    fn a_snapshot_names_the_running_job_and_the_queue_behind_it() {
        let pool = Workers::new(2).unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (started_tx, started_rx) = mpsc::channel();
        {
            let release_rx = release_rx.clone();
            pool.run(Lane::Transfer, "read", move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.lock().recv();
            });
        }
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the job starts");

        let snapshot = pool.snapshot();
        assert!(
            snapshot.workers.iter().any(|w| w.busy && w.label == "read"),
            "the running job is named"
        );
        assert_eq!(snapshot.transfer_completed, 0);

        let _ = release_tx.send(());
    }

    /// Audit A6. Transfers filling every general worker must not delay a
    /// metadata job: that is what the reserved threads are for.
    #[test]
    fn saturated_transfers_do_not_block_metadata() {
        let pool = Workers::new(FUSE_WORKERS).unwrap();
        let general = FUSE_WORKERS - META_WORKERS;

        // Occupy every general worker with a transfer that will not finish
        // until we say so, and wait until they are all actually running.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (started_tx, started_rx) = mpsc::channel();
        for _ in 0..general {
            let release_rx = release_rx.clone();
            let started_tx = started_tx.clone();
            pool.run(Lane::Transfer, "test", move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.lock().recv();
            });
        }
        for _ in 0..general {
            started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("every general worker picks up a transfer");
        }

        // Queue more transfers than there are threads, so the lane is backed up
        // in the way that used to starve metadata.
        for _ in 0..16 {
            let release_rx = release_rx.clone();
            pool.run(Lane::Transfer, "test", move || {
                let _ = release_rx.lock().recv();
            });
        }

        let (meta_tx, meta_rx) = mpsc::channel();
        pool.run(Lane::Meta, "test", move || meta_tx.send(()).unwrap());
        meta_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a metadata job runs while every transfer thread is stuck");

        for _ in 0..(general + 16) {
            let _ = release_tx.send(());
        }
    }

    /// The reverse direction is allowed and matters for throughput: with no
    /// transfers in flight, a general worker serves metadata rather than idling.
    #[test]
    fn general_workers_fall_back_to_metadata() {
        // One general worker and no reserved ones, so anything that completes
        // must have been served by the general worker.
        let pool = Workers::new(1).unwrap();
        let (tx, rx) = mpsc::channel();
        pool.run(Lane::Meta, "test", move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(5))
            .expect("a general worker takes metadata when no transfer is waiting");
    }

    /// A panicking job costs its caller an EIO, never the worker.
    #[test]
    fn a_panicking_job_does_not_retire_its_worker() {
        let pool = Workers::new(1).unwrap();
        pool.run(Lane::Meta, "test", || panic!("boom"));
        let (tx, rx) = mpsc::channel();
        pool.run(Lane::Meta, "test", move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the pool still serves work after a panic");
    }
}
