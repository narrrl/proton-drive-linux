//! One shutdown signal shared by every long-lived worker in the daemon.
//!
//! The mount used to end by unmounting and returning, leaving the drain
//! workers, the sync engine, the conflict sweep, the online probe and the local
//! indexer running on `Core` clones. In a process that exits immediately that is
//! invisible; in an in-process remount — which is how the daemon recovers from a
//! mount going away underneath it — it leaks a whole generation of threads that
//! keep writing to the database and calling the API on behalf of a mount that no
//! longer exists (`docs/BUGS.md` B44).
//!
//! The primitive is deliberately small: a flag plus a condvar. Every loop that
//! would otherwise call `thread::sleep` calls [`Shutdown::sleep`] instead, which
//! returns early — and reports `false` — the moment [`Shutdown::stop`] is
//! called, so teardown does not have to wait out a five-minute sweep interval to
//! join a thread.

use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// The daemon's stop signal. Set once, never cleared: a `Core` that has been
/// told to stop belongs to a mount that is going away.
#[derive(Default)]
pub(crate) struct Shutdown {
    stopping: Mutex<bool>,
    changed: Condvar,
}

impl Shutdown {
    /// Whether teardown has begun. Loops check this where they would otherwise
    /// start another unit of work.
    pub(crate) fn is_stopping(&self) -> bool {
        *self.stopping.lock()
    }

    /// Begin teardown and wake everything waiting in [`sleep`](Self::sleep).
    /// Idempotent, because more than one path can reach teardown (a signal, the
    /// kernel mount ending, a failed startup).
    pub(crate) fn stop(&self) {
        let mut stopping = self.stopping.lock();
        *stopping = true;
        self.changed.notify_all();
    }

    /// Wait up to `duration`, or until teardown starts.
    ///
    /// Returns `false` if the daemon is stopping — including when it was already
    /// stopping on entry, so `while shutdown.sleep(interval)` is a complete and
    /// correct loop condition and no separate check is needed at the top.
    pub(crate) fn sleep(&self, duration: Duration) -> bool {
        let mut stopping = self.stopping.lock();
        if !*stopping {
            self.changed.wait_for(&mut stopping, duration);
        }
        !*stopping
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn sleeping_returns_early_once_stopped() {
        let shutdown = Arc::new(Shutdown::default());
        assert!(!shutdown.is_stopping());

        let waker = shutdown.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.stop();
        });

        let started = Instant::now();
        // An hour's worth of interval, cut short by the stop.
        assert!(!shutdown.sleep(Duration::from_secs(3600)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stop must interrupt the wait, not wait it out"
        );
        assert!(shutdown.is_stopping());
        // Already stopping: the next wait does not sleep at all.
        let started = Instant::now();
        assert!(!shutdown.sleep(Duration::from_secs(3600)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn sleeping_runs_to_the_timeout_while_running() {
        let shutdown = Shutdown::default();
        assert!(shutdown.sleep(Duration::from_millis(10)));
        assert!(!shutdown.is_stopping());
    }
}
