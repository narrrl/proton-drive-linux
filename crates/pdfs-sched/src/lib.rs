//! Admission: when a runnable step of background work may start.
//!
//! Step 6 of the 4.0 plan in `docs/ARCHITECTURE.md` gives the daemon one
//! account-wide admission policy for mirrors, reads and writeback
//! (`SCHEDULER_AND_WORKERS.md`). This crate is its port. A service describes
//! each step it is about to run as a [`Step`] and waits for a [`Permit`]
//! from an [`Admission`]; dropping the permit gives the capacity back.
//!
//! The sync engine is the first caller: every upload, download and folder
//! creation of a mirror pass is admitted here. [`Bounded`] is the policy the
//! daemon supplies for now, a fixed number of steps at once, which is what the
//! engine did on its own before. An account-wide controller that weighs
//! mirrors against mounted reads and the drain plugs in behind the same trait
//! later, without the engine changing.
//!
//! An admission decides only; it runs no step and does no I/O. It does say
//! what it is doing: [`Admission::load`] reports, per class, how many steps
//! run, how many wait and for how long, so a stall report can tell a step
//! waiting in line from one already on the network.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Semaphore;

/// The kind of work a step is, which a policy weighs against the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// One effect of a mirror pass: an upload, a download, or a folder made
    /// on Drive.
    Mirror,
}

impl Class {
    /// Every class, in the order a report lists them.
    pub const ALL: [Class; 1] = [Class::Mirror];

    /// The class's name in diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Class::Mirror => "mirror",
        }
    }
}

/// A step that is ready to run and asks to be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub class: Class,
    /// The root the step works for, such as the id of a synced folder, so a
    /// policy can be fair across roots.
    pub root: i64,
}

/// The capacity a step was admitted with. Dropping it gives the capacity
/// back.
pub struct Permit {
    _held: Box<dyn Send + Sync>,
}

impl Permit {
    /// A permit that holds `capacity` until it is dropped.
    pub fn new(capacity: impl Send + Sync + 'static) -> Self {
        Self {
            _held: Box::new(capacity),
        }
    }
}

/// The future an admission answers with: it resolves once the step may run.
pub type Admitting<'a> = Pin<Box<dyn Future<Output = Permit> + Send + 'a>>;

/// What an admission is doing for one class of step, in [`Admission::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Load {
    pub class: Class,
    /// Steps admitted and still holding their permit.
    pub running: usize,
    /// Steps that asked and have not been admitted yet.
    pub waiting: usize,
    /// How long the step that has waited longest has waited so far.
    pub oldest_wait: Option<Duration>,
    /// Steps admitted since the policy was made.
    pub admitted: u64,
    /// The time those steps spent waiting, in total and at most.
    pub waited: Duration,
    pub longest_wait: Duration,
}

/// A policy that decides when a step may start.
pub trait Admission: Send + Sync {
    /// Wait until `step` may run, and return the capacity it runs with.
    fn admit(&self, step: Step) -> Admitting<'_>;

    /// What the policy is doing now, one entry per class it has seen. It must
    /// answer without waiting on a step, so a stall report can ask it while
    /// every step is stuck.
    fn load(&self) -> Vec<Load> {
        Vec::new()
    }
}

/// The counters behind [`Load`], for one class.
#[derive(Default)]
struct Counters {
    running: usize,
    /// When each waiting step asked, by ticket.
    waiting: Vec<(u64, Instant)>,
    admitted: u64,
    waited: Duration,
    longest_wait: Duration,
}

/// The counters of every class, shared by a policy and the permits and
/// pending admissions it handed out. Each lock is held for a few field
/// updates and never across an await.
#[derive(Default)]
struct Meter {
    classes: Mutex<[Counters; Class::ALL.len()]>,
    next_ticket: std::sync::atomic::AtomicU64,
}

impl Meter {
    fn ask(self: &Arc<Self>, class: Class) -> Asking {
        let ticket = self
            .next_ticket
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let asked = Instant::now();
        self.classes.lock()[class as usize]
            .waiting
            .push((ticket, asked));
        Asking {
            meter: self.clone(),
            class,
            ticket,
            asked,
            admitted: false,
        }
    }

    fn load(&self) -> Vec<Load> {
        let now = Instant::now();
        let classes = self.classes.lock();
        Class::ALL
            .iter()
            .map(|class| {
                let c = &classes[*class as usize];
                Load {
                    class: *class,
                    running: c.running,
                    waiting: c.waiting.len(),
                    oldest_wait: c
                        .waiting
                        .iter()
                        .map(|(_, asked)| now.saturating_duration_since(*asked))
                        .max(),
                    admitted: c.admitted,
                    waited: c.waited,
                    longest_wait: c.longest_wait,
                }
            })
            .collect()
    }
}

/// A step that has asked and not been admitted yet. Dropped unadmitted, as
/// when the caller gives up waiting, it leaves the queue without counting.
struct Asking {
    meter: Arc<Meter>,
    class: Class,
    ticket: u64,
    asked: Instant,
    admitted: bool,
}

impl Asking {
    /// The step was admitted: it stops waiting and starts running.
    fn admitted(mut self) -> Running {
        let waited = self.asked.elapsed();
        {
            let mut classes = self.meter.classes.lock();
            let c = &mut classes[self.class as usize];
            c.waiting.retain(|(t, _)| *t != self.ticket);
            c.running += 1;
            c.admitted += 1;
            c.waited += waited;
            c.longest_wait = c.longest_wait.max(waited);
        }
        self.admitted = true;
        Running {
            meter: self.meter.clone(),
            class: self.class,
        }
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        if !self.admitted {
            self.meter.classes.lock()[self.class as usize]
                .waiting
                .retain(|(t, _)| *t != self.ticket);
        }
    }
}

/// A step that runs; dropping it counts the step as done.
struct Running {
    meter: Arc<Meter>,
    class: Class,
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut classes = self.meter.classes.lock();
        let c = &mut classes[self.class as usize];
        c.running = c.running.saturating_sub(1);
    }
}

/// At most a fixed number of steps at once, whatever their class or root,
/// admitted in the order they asked.
pub struct Bounded {
    slots: Arc<Semaphore>,
    meter: Arc<Meter>,
}

impl Bounded {
    /// A policy that runs at most `limit` steps at once. `limit` must be at
    /// least 1.
    pub fn new(limit: usize) -> Self {
        assert!(limit > 0, "an admission that admits nothing never finishes");
        Self {
            slots: Arc::new(Semaphore::new(limit)),
            meter: Arc::default(),
        }
    }

    /// How many more steps could start now.
    pub fn available(&self) -> usize {
        self.slots.available_permits()
    }
}

impl Admission for Bounded {
    fn admit(&self, step: Step) -> Admitting<'_> {
        let slots = self.slots.clone();
        let asking = self.meter.ask(step.class);
        Box::pin(async move {
            // The semaphore is never closed, so the acquire cannot fail.
            let slot = slots.acquire_owned().await.expect("semaphore never closed");
            // A tuple drops its fields in order: the step stops counting as
            // running before its slot goes back, so
            // a report never shows more running than the limit.
            Permit::new((asking.admitted(), slot))
        })
    }

    fn load(&self) -> Vec<Load> {
        self.meter.load()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn step(root: i64) -> Step {
        Step {
            class: Class::Mirror,
            root,
        }
    }

    #[test]
    fn a_bounded_admission_never_runs_more_than_its_limit() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_time()
            .build()
            .unwrap();
        let admission = Arc::new(Bounded::new(3));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        rt.block_on(async {
            let mut set = tokio::task::JoinSet::new();
            for root in 0..20 {
                let (admission, running, peak) = (admission.clone(), running.clone(), peak.clone());
                set.spawn(async move {
                    let _permit = admission.admit(step(root)).await;
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                });
            }
            while set.join_next().await.is_some() {}
        });
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(admission.available(), 3);
    }

    #[test]
    fn dropping_a_permit_gives_its_capacity_back() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let admission = Bounded::new(1);
        rt.block_on(async {
            let first = admission.admit(step(1)).await;
            assert_eq!(admission.available(), 0);
            drop(first);
            assert_eq!(admission.available(), 1);
            let _second = admission.admit(step(2)).await;
            assert_eq!(admission.available(), 0);
        });
    }

    #[test]
    fn the_load_counts_running_and_waiting_steps() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let admission = Bounded::new(1);
        rt.block_on(async {
            let first = admission.admit(step(1)).await;
            let mut second = admission.admit(step(2));
            // Polled once, the second step is in line behind the first.
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut second)
                    .await
                    .is_err()
            );
            let load = &admission.load()[0];
            assert_eq!((load.running, load.waiting, load.admitted), (1, 1, 1));
            assert!(load.oldest_wait.unwrap() >= Duration::from_millis(20));

            drop(first);
            let second = second.await;
            let load = &admission.load()[0];
            assert_eq!((load.running, load.waiting, load.admitted), (1, 0, 2));
            assert_eq!(load.oldest_wait, None);
            assert!(load.longest_wait >= Duration::from_millis(20));
            assert!(load.waited >= load.longest_wait);

            drop(second);
            assert_eq!(admission.load()[0].running, 0);
        });
    }

    #[test]
    fn a_step_that_stops_waiting_leaves_the_line() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let admission = Bounded::new(1);
        rt.block_on(async {
            let _first = admission.admit(step(1)).await;
            let gave_up =
                tokio::time::timeout(Duration::from_millis(5), admission.admit(step(2))).await;
            assert!(gave_up.is_err());
            let load = &admission.load()[0];
            assert_eq!((load.running, load.waiting, load.admitted), (1, 0, 1));
        });
    }

    #[test]
    #[should_panic(expected = "admits nothing")]
    fn a_bounded_admission_of_nothing_is_refused() {
        let _ = Bounded::new(0);
    }
}
