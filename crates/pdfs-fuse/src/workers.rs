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

type Job = Box<dyn FnOnce() + Send + 'static>;

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
    meta: VecDeque<Job>,
    transfer: VecDeque<Job>,
    /// Set when the pool is dropped, to wake and retire every worker.
    closed: bool,
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
}

/// Bounded thread pool behind the network-touching FUSE handlers.
///
/// Shared by every session forked off one [`Core`] (the main mount plus each
/// on-demand sync folder), so the bound is per daemon rather than per mount.
pub(crate) struct Workers {
    inner: Arc<Pool>,
}

impl Workers {
    pub(crate) fn new(n: usize) -> std::io::Result<Self> {
        let inner = Arc::new(Pool {
            queues: Mutex::new(Queues::default()),
            meta_cv: Condvar::new(),
            general_cv: Condvar::new(),
        });
        // Never leave the pool without a general worker, however small `n` is.
        let meta_workers = META_WORKERS.min(n.saturating_sub(1));
        for i in 0..n {
            let inner = inner.clone();
            let meta_only = i < meta_workers;
            std::thread::Builder::new()
                .name(format!(
                    "pdfs-fuse-{}{i}",
                    if meta_only { "meta-" } else { "" }
                ))
                .spawn(move || {
                    let cv = if meta_only {
                        &inner.meta_cv
                    } else {
                        &inner.general_cv
                    };
                    loop {
                        let mut q = inner.queues.lock();
                        let job = loop {
                            // A general worker takes transfers first: metadata
                            // already has threads of its own, so draining the
                            // bulk queue is the useful thing for it to do.
                            let picked = if meta_only {
                                q.meta.pop_front()
                            } else {
                                q.transfer.pop_front().or_else(|| q.meta.pop_front())
                            };
                            if let Some(job) = picked {
                                break Some(job);
                            }
                            if q.closed {
                                break None;
                            }
                            cv.wait(&mut q);
                        };
                        drop(q);
                        let Some(job) = job else { break };
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
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                })?;
        }
        Ok(Self { inner })
    }

    /// Queue `job` in `lane`.
    ///
    /// Neither queue is bounded, and deliberately so: the only thing that could
    /// apply backpressure here is the fuser dispatch loop, and blocking *that*
    /// is the stall this module exists to avoid. Queue depth is cheap anyway —
    /// a pending job holds a `Reply` and a few fields, while the 4 MiB block
    /// buffer is allocated inside the job once it runs, and the SDK's in-flight
    /// semaphore is what bounds how many of those exist at once.
    pub(crate) fn run(&self, lane: Lane, job: impl FnOnce() + Send + 'static) {
        let mut q = self.inner.queues.lock();
        if q.closed {
            // Pre-pool behaviour: a shut-down pool degrades to a slow mount
            // rather than a mount that answers every read with EIO.
            drop(q);
            warn!("fuse worker pool is gone; serving inline");
            job();
            return;
        }
        match lane {
            Lane::Meta => q.meta.push_back(Box::new(job)),
            Lane::Transfer => q.transfer.push_back(Box::new(job)),
        }
        drop(q);
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
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.inner.queues.lock().closed = true;
        self.inner.meta_cv.notify_all();
        self.inner.general_cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

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
            pool.run(Lane::Transfer, move || {
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
            pool.run(Lane::Transfer, move || {
                let _ = release_rx.lock().recv();
            });
        }

        let (meta_tx, meta_rx) = mpsc::channel();
        pool.run(Lane::Meta, move || meta_tx.send(()).unwrap());
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
        pool.run(Lane::Meta, move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(5))
            .expect("a general worker takes metadata when no transfer is waiting");
    }

    /// A panicking job costs its caller an EIO, never the worker.
    #[test]
    fn a_panicking_job_does_not_retire_its_worker() {
        let pool = Workers::new(1).unwrap();
        pool.run(Lane::Meta, || panic!("boom"));
        let (tx, rx) = mpsc::channel();
        pool.run(Lane::Meta, move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the pool still serves work after a panic");
    }
}
