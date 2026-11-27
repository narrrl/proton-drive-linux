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
//! An admission decides only; it runs no step and does no I/O.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Semaphore;

/// The kind of work a step is, which a policy weighs against the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// One effect of a mirror pass: an upload, a download, or a folder made
    /// on Drive.
    Mirror,
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

/// A policy that decides when a step may start.
pub trait Admission: Send + Sync {
    /// Wait until `step` may run, and return the capacity it runs with.
    fn admit(&self, step: Step) -> Admitting<'_>;
}

/// At most a fixed number of steps at once, whatever their class or root,
/// admitted in the order they asked.
pub struct Bounded {
    slots: Arc<Semaphore>,
}

impl Bounded {
    /// A policy that runs at most `limit` steps at once. `limit` must be at
    /// least 1.
    pub fn new(limit: usize) -> Self {
        assert!(limit > 0, "an admission that admits nothing never finishes");
        Self {
            slots: Arc::new(Semaphore::new(limit)),
        }
    }

    /// How many more steps could start now.
    pub fn available(&self) -> usize {
        self.slots.available_permits()
    }
}

impl Admission for Bounded {
    fn admit(&self, _step: Step) -> Admitting<'_> {
        let slots = self.slots.clone();
        Box::pin(async move {
            // The semaphore is never closed, so the acquire cannot fail.
            Permit::new(slots.acquire_owned().await.expect("semaphore never closed"))
        })
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
    #[should_panic(expected = "admits nothing")]
    fn a_bounded_admission_of_nothing_is_refused() {
        let _ = Bounded::new(0);
    }
}
