//! A mutex that knows where it sits in the daemon's lock order.
//!
//! Most of the daemon's deadlocks were two threads taking the same pair of
//! locks in opposite orders (`docs/BUGS.md` B110, the cold size upgrade, the
//! diagnostics threads behind the database writer). Each such order was
//! written down in a comment, and the next feature did not read it.
//!
//! A [`Ranked`] lock carries a [`Rank`]. In a debug build, a thread that blocks
//! on a lock while it holds one of a higher rank panics, naming both, at the
//! line that would deadlock under the wrong interleaving. Release builds keep
//! no record and check nothing.
//!
//! Locks of equal rank may be held together. The code that takes several of
//! them orders them itself (several synced folders, lowest id first). A
//! `try_lock` cannot wait, so it is never refused, but what it takes is
//! recorded like any other lock.

use std::ops::{Deref, DerefMut};
use std::time::Duration;

use parking_lot::{Mutex, MutexGuard};

/// Where a lock sits in the order. A thread takes locks in increasing rank.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// One synced folder's lock, held by its sync pass for as long as the pass
    /// runs.
    SyncFolder = 10,
    /// Publishing a shared change to every mounted session.
    SharedPublication = 20,
    /// The inode tree.
    State = 30,
    /// The database's single write connection.
    DbWriter = 40,
}

/// A mutex with a [`Rank`]; see the module documentation.
pub struct Ranked<T> {
    rank: Rank,
    inner: Mutex<T>,
}

impl<T> Ranked<T> {
    pub const fn new(rank: Rank, value: T) -> Self {
        Self {
            rank,
            inner: Mutex::new(value),
        }
    }

    pub fn rank(&self) -> Rank {
        self.rank
    }

    /// Block until the lock is free. In a debug build, panics when this
    /// thread holds a lock of a higher rank.
    pub fn lock(&self) -> RankedGuard<'_, T> {
        held::check(self.rank);
        RankedGuard::new(self.inner.lock(), self.rank)
    }

    /// Take the lock if it is free.
    pub fn try_lock(&self) -> Option<RankedGuard<'_, T>> {
        self.inner
            .try_lock()
            .map(|guard| RankedGuard::new(guard, self.rank))
    }

    /// Wait up to `timeout` for the lock. It waits, so it is checked as
    /// [`Ranked::lock`] is.
    pub fn try_lock_for(&self, timeout: Duration) -> Option<RankedGuard<'_, T>> {
        held::check(self.rank);
        self.inner
            .try_lock_for(timeout)
            .map(|guard| RankedGuard::new(guard, self.rank))
    }

    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}

/// A held [`Ranked`] lock.
pub struct RankedGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    _held: held::Held,
}

impl<'a, T> RankedGuard<'a, T> {
    fn new(guard: MutexGuard<'a, T>, rank: Rank) -> Self {
        Self {
            guard,
            _held: held::Held::new(rank),
        }
    }
}

impl<T> Deref for RankedGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for RankedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(debug_assertions)]
mod held {
    use std::cell::RefCell;

    use super::Rank;

    thread_local! {
        /// The ranks this thread holds, in the order it took them.
        static HELD: RefCell<Vec<Rank>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn check(rank: Rank) {
        HELD.with(|held| {
            let held = held.borrow();
            if let Some(higher) = held.iter().find(|&&other| other > rank) {
                panic!(
                    "lock order: waiting on {rank:?} while holding {higher:?} \
                     (held: {held:?}); take {rank:?} first (see pdfs_core::lockrank)"
                );
            }
        });
    }

    pub(super) struct Held(Rank);

    impl Held {
        pub(super) fn new(rank: Rank) -> Self {
            HELD.with(|held| held.borrow_mut().push(rank));
            Self(rank)
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            HELD.with(|held| {
                let mut held = held.borrow_mut();
                // Guards can drop out of order; forget the latest of this rank.
                if let Some(at) = held.iter().rposition(|&rank| rank == self.0) {
                    held.remove(at);
                }
            });
        }
    }
}

#[cfg(not(debug_assertions))]
mod held {
    use super::Rank;

    #[inline]
    pub(super) fn check(_: Rank) {}

    pub(super) struct Held;

    impl Held {
        #[inline]
        pub(super) fn new(_: Rank) -> Self {
            Held
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn locks_taken_in_rank_order_are_allowed() {
        let state = Ranked::new(Rank::State, 1);
        let db = Ranked::new(Rank::DbWriter, 2);
        let a = state.lock();
        let b = db.lock();
        assert_eq!(*a + *b, 3);
    }

    #[test]
    #[should_panic(expected = "lock order: waiting on State while holding DbWriter")]
    fn a_lower_rank_taken_under_a_higher_one_panics() {
        let state = Ranked::new(Rank::State, ());
        let db = Ranked::new(Rank::DbWriter, ());
        let _db = db.lock();
        let _state = state.lock();
    }

    #[test]
    fn locks_of_equal_rank_may_be_held_together() {
        let one = Ranked::new(Rank::SyncFolder, ());
        let two = Ranked::new(Rank::SyncFolder, ());
        let _one = one.lock();
        let _two = two.lock();
    }

    #[test]
    fn a_released_lock_no_longer_counts() {
        let state = Ranked::new(Rank::State, ());
        let db = Ranked::new(Rank::DbWriter, ());
        drop(db.lock());
        let _state = state.lock();
        let _db = db.lock();
    }

    #[test]
    fn guards_dropped_out_of_order_are_both_forgotten() {
        let publication = Ranked::new(Rank::SharedPublication, ());
        let state = Ranked::new(Rank::State, ());
        let db = Ranked::new(Rank::DbWriter, ());
        let p = publication.lock();
        let s = state.lock();
        drop(p);
        drop(s);
        let _db = db.lock();
        drop(_db);
        let _p = publication.lock();
    }

    #[test]
    fn a_try_lock_is_never_refused_but_still_counts() {
        let state = Ranked::new(Rank::State, ());
        let db = Ranked::new(Rank::DbWriter, ());
        let _db = db.lock();
        let s = state.try_lock().expect("free");
        drop(_db);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Ranked::new(Rank::SharedPublication, ()).lock();
        }));
        drop(s);
        assert!(result.is_err(), "the try_lock'd State still counts as held");
    }
}
