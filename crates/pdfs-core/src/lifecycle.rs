//! The daemon's long-lived threads, started, watched and stopped in one place.
//!
//! Each long-lived thread used to be started on its own, with its own rules
//! for how it learns to stop, and the mount kept a bare list of join handles.
//! Waking a thread that blocks somewhere other than [`Shutdown::sleep`] (the
//! drain's condvar, the sync engine's channel, the control socket's `accept`)
//! was a separate list of calls far from the spawn, and nothing could say which
//! threads were still alive.
//!
//! [`Services`] keeps all of it together. Each thread is started with its name
//! and the call that wakes it, if it needs one. [`Services::stop`] stops them in
//! reverse start order: it signals [`Shutdown`], wakes each one, and joins each
//! one up to a shared deadline, naming any that is still running then.
//! [`ServiceBoard`] is the live view of the same threads, for diagnostics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::shutdown::Shutdown;

/// Wakes a thread that blocks somewhere a [`Shutdown::stop`] alone does not reach.
pub type Wake = Box<dyn Fn() + Send + Sync>;

/// Whether one service thread is still running, for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceHealth {
    /// The thread's name, such as `pdfs-sync`.
    pub name: String,
    pub running: bool,
    /// How long ago the thread was started.
    pub age: Duration,
}

/// The live view of the service threads: which ones were started, and which
/// are still running. Cheap to clone and to read; never blocks on a service.
#[derive(Clone, Default)]
pub struct ServiceBoard(Arc<Mutex<Vec<Entry>>>);

struct Entry {
    name: String,
    started: Instant,
    running: Arc<AtomicBool>,
}

impl ServiceBoard {
    /// Every service thread started so far, in start order.
    pub fn health(&self) -> Vec<ServiceHealth> {
        self.0
            .lock()
            .iter()
            .map(|entry| ServiceHealth {
                name: entry.name.clone(),
                running: entry.running.load(Ordering::Relaxed),
                age: entry.started.elapsed(),
            })
            .collect()
    }

    fn add(&self, name: &str) -> Arc<AtomicBool> {
        let running = Arc::new(AtomicBool::new(true));
        self.0.lock().push(Entry {
            name: name.to_owned(),
            started: Instant::now(),
            running: running.clone(),
        });
        running
    }
}

/// Clears a service's `running` flag when its thread ends, by return or by panic.
struct Running(Arc<AtomicBool>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

struct Service {
    handle: JoinHandle<()>,
    wake: Option<Wake>,
}

/// The service threads one mount started, and how to stop them.
pub struct Services {
    board: ServiceBoard,
    started: Vec<Service>,
}

impl Services {
    /// Start an empty set whose threads show on `board`.
    pub fn new(board: ServiceBoard) -> Self {
        Self {
            board,
            started: Vec::new(),
        }
    }

    /// Start `run` on a thread named `name`. `wake`, if given, is called on
    /// [`stop`](Self::stop) after the [`Shutdown`] signal, for a thread that
    /// blocks where that signal does not reach.
    pub fn spawn<F>(&mut self, name: &str, wake: Option<Wake>, run: F) -> std::io::Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        let running = self.board.add(name);
        let guard = Running(running.clone());
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let _guard = guard;
                run();
            });
        match spawned {
            Ok(handle) => {
                self.started.push(Service { handle, wake });
                Ok(())
            }
            Err(error) => {
                // The closure, and the guard in it, are dropped with the error,
                // so the board already shows the thread as not running.
                Err(error)
            }
        }
    }

    /// How many threads were started.
    pub fn len(&self) -> usize {
        self.started.len()
    }

    /// Whether no thread was started.
    pub fn is_empty(&self) -> bool {
        self.started.is_empty()
    }

    /// Stop every thread: signal `shutdown`, wake each thread in reverse start
    /// order, then join them in the same order until `deadline`. Returns the
    /// names of the threads still running at the deadline; they are left
    /// behind rather than waited for.
    pub fn stop(self, shutdown: &Shutdown, deadline: Instant) -> Vec<String> {
        shutdown.stop();
        for service in self.started.iter().rev() {
            if let Some(wake) = &service.wake {
                wake();
            }
        }
        join_until(
            self.started.into_iter().rev().map(|service| service.handle),
            deadline,
        )
    }
}

/// Join `handles` until `deadline`, and name the ones still running then.
///
/// A stop used to wait for each thread however long it took. The sync engine's
/// mirror pass waits on Drive with no deadline, and a stop that waited for it
/// was killed by systemd (`docs/BUGS.md` B165).
pub fn join_until(
    handles: impl IntoIterator<Item = JoinHandle<()>>,
    deadline: Instant,
) -> Vec<String> {
    let mut running = Vec::new();
    for handle in handles {
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !handle.is_finished() {
            running.push(handle.thread().name().unwrap_or("unnamed").to_string());
        } else if handle.join().is_err() {
            tracing::warn!("a background worker panicked before shutdown");
        }
    }
    running
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn a_stop_wakes_in_reverse_order_and_names_what_is_still_running() {
        let board = ServiceBoard::default();
        let shutdown = Arc::new(Shutdown::default());
        let mut services = Services::new(board.clone());
        let woken = Arc::new(Mutex::new(Vec::new()));

        // Ends on the shutdown signal alone.
        let signalled = shutdown.clone();
        services
            .spawn("sleeper", None, move || {
                while signalled.sleep(Duration::from_secs(3600)) {}
            })
            .unwrap();
        // Blocks on a channel: only its wake ends it.
        let (tx, rx) = mpsc::channel::<()>();
        let order = woken.clone();
        services
            .spawn(
                "listener",
                Some(Box::new(move || {
                    order.lock().push("listener");
                    let _ = tx.send(());
                })),
                move || {
                    let _ = rx.recv();
                },
            )
            .unwrap();
        // Never ends within the deadline.
        let (_hold, stuck) = mpsc::channel::<()>();
        let order = woken.clone();
        services
            .spawn(
                "stuck",
                Some(Box::new(move || order.lock().push("stuck"))),
                move || {
                    let _ = stuck.recv();
                },
            )
            .unwrap();
        assert_eq!(services.len(), 3);
        assert!(board.health().iter().all(|service| service.running));

        let left = services.stop(&shutdown, Instant::now() + Duration::from_millis(500));

        assert_eq!(left, ["stuck"]);
        assert_eq!(*woken.lock(), ["stuck", "listener"]);
        let running: Vec<_> = board
            .health()
            .into_iter()
            .map(|service| (service.name, service.running))
            .collect();
        assert_eq!(
            running,
            [
                ("sleeper".to_string(), false),
                ("listener".to_string(), false),
                ("stuck".to_string(), true),
            ]
        );
    }

    #[test]
    fn a_service_that_panics_shows_as_not_running() {
        let board = ServiceBoard::default();
        let mut services = Services::new(board.clone());
        services.spawn("doomed", None, || panic!("boom")).unwrap();
        let left = services.stop(
            &Shutdown::default(),
            Instant::now() + Duration::from_secs(5),
        );
        assert!(left.is_empty());
        assert!(!board.health()[0].running);
    }
}
