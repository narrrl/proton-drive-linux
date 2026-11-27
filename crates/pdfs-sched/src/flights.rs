//! Single flight: one caller does a piece of work, the others asking for the
//! same thing at the same time wait for it.
//!
//! The first caller for a key leads and gets a [`Lead`]; it does the work and
//! drops the lead when done, whether it succeeded, failed or panicked. A
//! caller that finds a lead out for its key follows: [`Flights::join`] blocks
//! until that lead is dropped and returns `None`. The follower then looks
//! again at what the leader left behind; when the leader failed it does the
//! work itself, so every caller still gets an answer of its own.
//!
//! The wait is a blocking one, for the threads that serve the mount. A
//! follower must hold nothing its leader needs.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

/// The work in flight, by key.
pub struct Flights<K: Eq + Hash + Clone> {
    out: Mutex<HashMap<K, Arc<Flight>>>,
}

/// One piece of work in flight, which its followers wait on.
#[derive(Default)]
struct Flight {
    landed: Mutex<bool>,
    wake: Condvar,
}

/// The right and the duty to do the work for one key. Dropping it lets the
/// followers go.
pub struct Lead<'a, K: Eq + Hash + Clone> {
    flights: &'a Flights<K>,
    key: K,
    flight: Arc<Flight>,
}

impl<K: Eq + Hash + Clone> Default for Flights<K> {
    fn default() -> Self {
        Self {
            out: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone> Flights<K> {
    /// Lead the work for `key`, or, when another caller leads it already,
    /// wait until that caller is done and return `None`.
    pub fn join(&self, key: &K) -> Option<Lead<'_, K>> {
        let flight = {
            let mut out = self.out.lock();
            match out.get(key) {
                Some(flight) => flight.clone(),
                None => {
                    let flight = Arc::new(Flight::default());
                    out.insert(key.clone(), flight.clone());
                    return Some(Lead {
                        flights: self,
                        key: key.clone(),
                        flight,
                    });
                }
            }
        };
        let mut landed = flight.landed.lock();
        while !*landed {
            flight.wake.wait(&mut landed);
        }
        None
    }

    /// How many keys have work in flight.
    pub fn len(&self) -> usize {
        self.out.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K: Eq + Hash + Clone> Drop for Lead<'_, K> {
    fn drop(&mut self) {
        // Out of the map first, so a caller that comes after the followers
        // wake leads anew rather than following a flight that has landed.
        self.flights.out.lock().remove(&self.key);
        *self.flight.landed.lock() = true;
        self.flight.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn callers_that_ask_at_once_share_one_flight() {
        let flights = Arc::new(Flights::<u32>::default());
        let worked = Arc::new(AtomicUsize::new(0));
        let lead = flights.join(&7).expect("the first caller leads");
        let followers: Vec<_> = (0..4)
            .map(|_| {
                let (flights, worked) = (flights.clone(), worked.clone());
                thread::spawn(move || {
                    if flights.join(&7).is_some() {
                        worked.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(30));
        assert!(followers.iter().all(|f| !f.is_finished()));
        drop(lead);
        for follower in followers {
            follower.join().unwrap();
        }
        assert_eq!(worked.load(Ordering::SeqCst), 0);
        assert!(flights.is_empty());
    }

    #[test]
    fn different_keys_do_not_wait_for_each_other() {
        let flights = Flights::<u32>::default();
        let _a = flights.join(&1).expect("leads 1");
        let _b = flights.join(&2).expect("leads 2");
        assert_eq!(flights.len(), 2);
    }

    #[test]
    fn a_leader_that_panics_still_lets_its_followers_go() {
        let flights = Arc::new(Flights::<u32>::default());
        let (led, lead_now) = std::sync::mpsc::channel();
        let leader = {
            let flights = flights.clone();
            thread::spawn(move || {
                let _lead = flights.join(&3).expect("leads");
                led.send(()).unwrap();
                thread::sleep(Duration::from_millis(20));
                panic!("the work failed");
            })
        };
        lead_now.recv().unwrap();
        assert!(flights.join(&3).is_none());
        assert!(leader.join().is_err());
        // The next caller leads again.
        assert!(flights.join(&3).is_some());
    }
}
