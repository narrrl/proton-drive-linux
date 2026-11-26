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
//!
//! A service can also be an async task on the mount's runtime
//! ([`Services::spawn_task`]), such as the loops that follow Drive's event
//! streams. A stop aborts it at its next `.await` and waits for it to go, up
//! to the same deadline. Such a task used to run until the runtime went
//! away, which on a stop is when the process exits.
//!
//! Work a request starts and does not wait for runs as a one-shot job
//! ([`ServiceBoard::spawn_job`] on a thread, [`ServiceBoard::spawn_task_job`]
//! on the runtime): listed on the board while it runs, and not joined on stop.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// The live view of the service threads, which were started and which are
/// still running, and of the one-shot jobs running now. Cheap to clone and to
/// read; never blocks on a service or a job.
#[derive(Clone, Default)]
pub struct ServiceBoard(Arc<Board>);

#[derive(Default)]
struct Board {
    services: Mutex<Vec<Entry>>,
    jobs: Mutex<Vec<Job>>,
    next_job: AtomicU64,
}

struct Entry {
    name: String,
    started: Instant,
    running: Arc<AtomicBool>,
}

struct Job {
    id: u64,
    name: String,
    started: Instant,
}

impl ServiceBoard {
    /// Every service thread started so far, in start order.
    pub fn health(&self) -> Vec<ServiceHealth> {
        self.0
            .services
            .lock()
            .iter()
            .map(|entry| ServiceHealth {
                name: entry.name.clone(),
                running: entry.running.load(Ordering::Relaxed),
                age: entry.started.elapsed(),
            })
            .collect()
    }

    /// The one-shot jobs running now, oldest first. A job leaves the list
    /// when its thread ends.
    pub fn jobs(&self) -> Vec<ServiceHealth> {
        self.0
            .jobs
            .lock()
            .iter()
            .map(|job| ServiceHealth {
                name: job.name.clone(),
                running: true,
                age: job.started.elapsed(),
            })
            .collect()
    }

    /// Start `run` on a thread named `name`, as a one-shot job: a request's
    /// work that outlasts the request, such as a cache check, an import or a
    /// folder pin. It shows in [`jobs`](Self::jobs) until it ends, by return
    /// or by panic.
    ///
    /// Not joined on stop. A job's work is either safe to cut short (the
    /// process exits) or bounded by the deadlines of the calls it makes.
    pub fn spawn_job<F>(&self, name: &str, run: F) -> std::io::Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        let guard = self.add_job(name);
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let _guard = guard;
                run();
            })
            // On an error the closure, and the guard in it, are already
            // dropped, so the job is off the list.
            .map(|_| ())
    }

    /// Start `task` on `rt` as a one-shot job, like [`spawn_job`](Self::spawn_job)
    /// for work that is async: a refresh a listing starts and does not wait
    /// for. It shows in [`jobs`](Self::jobs) until it ends, by return, panic
    /// or the runtime going away.
    pub fn spawn_task_job<F>(&self, rt: &tokio::runtime::Handle, name: &str, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let guard = self.add_job(name);
        rt.spawn(async move {
            let _guard = guard;
            task.await;
        });
    }

    fn add_job(&self, name: &str) -> JobGuard {
        let id = self.0.next_job.fetch_add(1, Ordering::Relaxed);
        self.0.jobs.lock().push(Job {
            id,
            name: name.to_owned(),
            started: Instant::now(),
        });
        JobGuard {
            board: self.0.clone(),
            id,
        }
    }

    fn add(&self, name: &str) -> Arc<AtomicBool> {
        let running = Arc::new(AtomicBool::new(true));
        self.0.services.lock().push(Entry {
            name: name.to_owned(),
            started: Instant::now(),
            running: running.clone(),
        });
        running
    }
}

/// Takes a job off the board when its thread ends, by return or by panic.
struct JobGuard {
    board: Arc<Board>,
    id: u64,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.board.jobs.lock().retain(|job| job.id != self.id);
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
    handle: Handle,
    wake: Option<Wake>,
}

/// A service is a thread, or an async task on the mount's runtime.
enum Handle {
    Thread(JoinHandle<()>),
    Task {
        name: String,
        task: tokio::task::JoinHandle<()>,
    },
}

impl Handle {
    fn is_finished(&self) -> bool {
        match self {
            Handle::Thread(handle) => handle.is_finished(),
            Handle::Task { task, .. } => task.is_finished(),
        }
    }
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
                self.started.push(Service {
                    handle: Handle::Thread(handle),
                    wake,
                });
                Ok(())
            }
            Err(error) => {
                // The closure, and the guard in it, are dropped with the error,
                // so the board already shows the thread as not running.
                Err(error)
            }
        }
    }

    /// Start `task` on `rt` as a service named `name`. [`stop`](Self::stop)
    /// aborts it, so it ends at its next `.await`; a task does not need to
    /// watch the [`Shutdown`] signal itself. Code between two `.await`s runs
    /// to its end, so a write it makes there is not cut in half.
    pub fn spawn_task<F>(&mut self, rt: &tokio::runtime::Handle, name: &str, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let guard = Running(self.board.add(name));
        let task = rt.spawn(async move {
            let _guard = guard;
            task.await;
        });
        self.started.push(Service {
            handle: Handle::Task {
                name: name.to_owned(),
                task,
            },
            wake: None,
        });
    }

    /// How many services were started.
    pub fn len(&self) -> usize {
        self.started.len()
    }

    /// Whether no service was started.
    pub fn is_empty(&self) -> bool {
        self.started.is_empty()
    }

    /// Stop every service: signal `shutdown`, wake each thread and abort each
    /// task in reverse start order, then join them in the same order until
    /// `deadline`. Returns the names of the services still running at the
    /// deadline; they are left behind rather than waited for.
    pub fn stop(self, shutdown: &Shutdown, deadline: Instant) -> Vec<String> {
        shutdown.stop();
        for service in self.started.iter().rev() {
            if let Some(wake) = &service.wake {
                wake();
            }
            if let Handle::Task { task, .. } = &service.handle {
                task.abort();
            }
        }
        let mut running = Vec::new();
        for service in self.started.into_iter().rev() {
            while !service.handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            match service.handle {
                Handle::Thread(handle) => running.extend(join_until([handle], deadline)),
                Handle::Task { name, task } if !task.is_finished() => running.push(name),
                Handle::Task { .. } => {}
            }
        }
        running
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

    #[test]
    fn a_job_shows_while_it_runs_and_leaves_when_it_ends() {
        let board = ServiceBoard::default();
        let (release, wait) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        board
            .spawn_job("pdfs-test-job", move || {
                let _ = wait.recv();
                let _done = done_tx;
            })
            .unwrap();
        let jobs = board.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "pdfs-test-job");
        assert!(board.health().is_empty(), "a job is not a service");

        release.send(()).unwrap();
        let _ = done_rx.recv_timeout(Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !board.jobs().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(board.jobs().is_empty());
    }

    #[test]
    fn a_stop_aborts_a_task_and_shows_it_ended() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let board = ServiceBoard::default();
        let mut services = Services::new(board.clone());
        // Never ends on its own and never looks at the shutdown signal.
        services.spawn_task(rt.handle(), "poller", async {
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        assert_eq!(services.len(), 1);
        assert!(board.health()[0].running);

        let left = services.stop(
            &Shutdown::default(),
            Instant::now() + Duration::from_secs(5),
        );

        assert!(left.is_empty(), "still running: {left:?}");
        assert!(!board.health()[0].running);
    }

    #[test]
    fn a_task_stuck_between_awaits_is_named_at_the_deadline() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut services = Services::new(ServiceBoard::default());
        let (release, wait) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        // Blocks its worker thread, so an abort cannot reach it until it yields.
        services.spawn_task(rt.handle(), "blocking", async move {
            let _ = started_tx.send(());
            let _ = wait.recv();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let left = services.stop(
            &Shutdown::default(),
            Instant::now() + Duration::from_millis(100),
        );

        assert_eq!(left, ["blocking"]);
        drop(release);
    }

    #[test]
    fn a_task_job_shows_while_it_runs_and_leaves_when_it_ends() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let board = ServiceBoard::default();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        board.spawn_task_job(rt.handle(), "pdfs-test-refresh", async move {
            let _ = wait.await;
        });
        let jobs = board.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "pdfs-test-refresh");

        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !board.jobs().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(board.jobs().is_empty());
    }

    #[test]
    fn a_job_that_panics_leaves_the_list() {
        let board = ServiceBoard::default();
        board.spawn_job("doomed-job", || panic!("boom")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !board.jobs().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(board.jobs().is_empty());
    }
}
