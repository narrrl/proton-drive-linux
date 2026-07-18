//! The thread pool that serves the FUSE handlers which touch the network.
//!
//! fuser drives `Filesystem` from a single dispatch loop, so a cold read that
//! goes to the wire would block every cheap metadata call behind it. Those
//! handlers hand their work here instead and reply from the worker.

use std::sync::Arc;

use parking_lot::Mutex;
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
/// limit. Sized just above the SDK's in-flight block cap so the semaphore
/// there, not thread count, is what bounds download memory.
pub(crate) const FUSE_WORKERS: usize = 8;

/// Bounded thread pool behind the network-touching FUSE handlers.
///
/// Shared by every session forked off one [`Core`] (the main mount plus each
/// on-demand sync folder), so the bound is per daemon rather than per mount.
pub(crate) struct Workers {
    pub(crate) tx: std::sync::mpsc::Sender<Box<dyn FnOnce() + Send + 'static>>,
}

impl Workers {
    pub(crate) fn new(n: usize) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send + 'static>>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..n {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("pdfs-fuse-{i}"))
                .spawn(move || {
                    loop {
                        // Held only to take the next job, never across it.
                        let job = rx.lock().recv();
                        match job {
                            // A panicking handler must not cost the pool a
                            // worker for the rest of the run. The dropped
                            // `Reply` answers EIO on its own, so the caller of
                            // the failed op is told; the next job is unaffected.
                            //
                            // This only holds because the shared state is behind
                            // `parking_lot` mutexes: a `std` mutex held at the
                            // point of the panic would come back poisoned, and
                            // the worker we just rescued would die on its next
                            // acquisition — as would every other thread.
                            Ok(job) => {
                                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                            }
                            // Every sender is gone: the daemon is shutting down.
                            Err(_) => break,
                        }
                    }
                })?;
        }
        Ok(Self { tx })
    }

    /// Run `job` on a worker. Falls back to running it inline — the pre-pool
    /// behaviour — if no worker is left to take it, so a shut-down pool degrades
    /// to a slow mount rather than a mount that answers every read with EIO.
    pub(crate) fn run(&self, job: impl FnOnce() + Send + 'static) {
        if let Err(e) = self.tx.send(Box::new(job)) {
            warn!("fuse worker pool is gone; serving inline");
            (e.0)();
        }
    }
}
