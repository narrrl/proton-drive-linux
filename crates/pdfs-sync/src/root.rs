//! The root reducer: what a mirror folder's sync knows about its coverage,
//! as one state machine per folder (`docs/SYNC_ENGINE.md`).
//!
//! [`step`] is pure: it takes the state and one input and returns the next
//! state. Time, the watcher, the event stream and the passes stay outside, as
//! inputs, so a sequence of inputs replays exactly.
//!
//! The states say how far the engine can trust what it has seen:
//!
//! - **Discovering**: no pass over the folder has finished yet.
//! - **Following**: a pass covered the whole folder, and nothing since says a
//!   change could have gone unreported.
//! - **Auditing**: something did (a lost watch, an overflowed queue, a gap in
//!   Drive's event stream, a restart, a suspension). Only a pass over the
//!   whole folder brings it back to Following.
//! - **Suspended**: the folder cannot be synced for now: its local root is
//!   missing, or sync is paused.
//! - **Draining**: a switch to on-demand is queued; passes only push.
//! - **HandedOff**: the folder is on-demand.
//!
//! For now the reducer is run alongside the engine and acted on by nothing.
//! What a pass reads on disk is decided apart from it: a pass brings the last
//! walk up to date from the local dirty set while the watch is trusted, and
//! walks the whole folder otherwise. Its transitions are logged, so they can
//! be compared with what the engine did before anything relies on them.

use std::collections::HashMap;
use std::fmt;

use parking_lot::Mutex;
use tracing::info;

/// Why a folder's coverage is in doubt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doubt {
    /// The daemon started: nothing was watched while it was down.
    Restart,
    /// The filesystem watcher reported an error.
    WatchLost,
    /// The watcher's event queue overflowed.
    Overflow,
    /// Drive's event stream lost changes.
    EventGap,
    /// The folder was suspended, and nothing was followed meanwhile.
    Suspension,
}

/// Why a folder is not synced for now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suspension {
    RootMissing,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootState {
    Discovering,
    Following,
    Auditing(Doubt),
    Suspended(Suspension),
    Draining,
    HandedOff,
}

/// Something that happened to a folder, as the reducer takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootInput {
    /// A pass over the whole folder finished.
    Covered,
    /// A change may have gone unreported.
    Doubted(Doubt),
    /// The folder cannot be synced for now.
    Suspended(Suspension),
    /// Sync is no longer paused. A missing root is only found again by a
    /// pass that covers it.
    Resumed,
    /// A switch to on-demand is queued.
    HandoffQueued,
    /// The folder is on-demand.
    HandedOff,
    /// The folder is a mirror again after being on-demand.
    Mirrored,
}

/// The state after `input`.
pub fn step(state: RootState, input: RootInput) -> RootState {
    use RootState as S;
    match (state, input) {
        // An on-demand folder is not synced: only going back to a mirror
        // changes it, and that is a first sync over again.
        (S::HandedOff, RootInput::Mirrored) => S::Discovering,
        (S::HandedOff, _) => S::HandedOff,
        (S::Draining, RootInput::HandedOff) => S::HandedOff,
        // A switch called off: a mirror again, which followed nothing while
        // it pushed.
        (S::Draining, RootInput::Mirrored) => S::Auditing(Doubt::Suspension),
        (S::Draining, _) => S::Draining,
        (_, RootInput::HandoffQueued) => S::Draining,
        (_, RootInput::HandedOff) => S::HandedOff,
        (S::Suspended(_), RootInput::Covered) => S::Following,
        (S::Suspended(Suspension::Paused), RootInput::Resumed) => S::Auditing(Doubt::Suspension),
        (S::Suspended(_), RootInput::Suspended(why)) => S::Suspended(why),
        (S::Suspended(why), _) => S::Suspended(why),
        (_, RootInput::Suspended(why)) => S::Suspended(why),
        (_, RootInput::Covered) => S::Following,
        (S::Following, RootInput::Doubted(why)) => S::Auditing(why),
        // A first sync and an audit cover everything anyway; the first doubt
        // is the one to report.
        (state, _) => state,
    }
}

impl fmt::Display for RootState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootState::Discovering => f.write_str("discovering"),
            RootState::Following => f.write_str("following"),
            RootState::Auditing(why) => write!(f, "auditing ({why:?})"),
            RootState::Suspended(why) => write!(f, "suspended ({why:?})"),
            RootState::Draining => f.write_str("draining"),
            RootState::HandedOff => f.write_str("handed off"),
        }
    }
}

/// The root state of every mirror folder the engine has seen, by id.
#[derive(Default)]
pub struct Roots {
    states: Mutex<HashMap<i64, RootState>>,
}

impl Roots {
    /// Start following `folder`, unless it already is: from discovery for a
    /// folder never synced, and otherwise from an audit, since nothing
    /// watched it while the daemon was down.
    pub fn begin(&self, folder: i64, synced_before: bool) {
        self.states
            .lock()
            .entry(folder)
            .or_insert(if synced_before {
                RootState::Auditing(Doubt::Restart)
            } else {
                RootState::Discovering
            });
    }

    /// Feed `input` to `folder`'s reducer, and log the transition it makes.
    /// A folder not begun yet is left alone: its first pass begins it.
    pub fn input(&self, folder: i64, input: RootInput) {
        let mut states = self.states.lock();
        let Some(state) = states.get_mut(&folder) else {
            return;
        };
        let next = step(*state, input);
        if next != *state {
            info!(id = folder, from = %state, to = %next, ?input, "sync: root state");
            *state = next;
        }
    }

    /// Feed `input` to every folder's reducer.
    pub fn input_all(&self, input: RootInput) {
        let folders: Vec<i64> = self.states.lock().keys().copied().collect();
        for folder in folders {
            self.input(folder, input);
        }
    }

    /// The state of `folder`, if the engine has begun it.
    pub fn state(&self, folder: i64) -> Option<RootState> {
        self.states.lock().get(&folder).copied()
    }

    /// Forget a folder that is no longer synced.
    pub fn forget(&self, folder: i64) {
        self.states.lock().remove(&folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RootInput as I;
    use RootState as S;

    fn run(from: RootState, inputs: &[RootInput]) -> RootState {
        inputs.iter().fold(from, |state, input| step(state, *input))
    }

    #[test]
    fn a_first_sync_follows_once_a_pass_covers_the_folder() {
        assert_eq!(run(S::Discovering, &[I::Covered]), S::Following);
        // A doubt during discovery changes nothing: the pass covers it all.
        assert_eq!(
            run(S::Discovering, &[I::Doubted(Doubt::Overflow)]),
            S::Discovering
        );
    }

    #[test]
    fn a_doubt_takes_a_followed_folder_to_an_audit_until_a_pass_covers_it() {
        let audit = run(
            S::Following,
            &[I::Doubted(Doubt::WatchLost), I::Doubted(Doubt::EventGap)],
        );
        assert_eq!(
            audit,
            S::Auditing(Doubt::WatchLost),
            "the first doubt stays"
        );
        assert_eq!(step(audit, I::Covered), S::Following);
    }

    #[test]
    fn a_suspension_ends_in_an_audit_or_a_pass() {
        let paused = run(S::Following, &[I::Suspended(Suspension::Paused)]);
        assert_eq!(paused, S::Suspended(Suspension::Paused));
        assert_eq!(step(paused, I::Doubted(Doubt::Overflow)), paused);
        assert_eq!(step(paused, I::Resumed), S::Auditing(Doubt::Suspension));
        let missing = run(S::Following, &[I::Suspended(Suspension::RootMissing)]);
        assert_eq!(step(missing, I::Resumed), missing);
        assert_eq!(step(missing, I::Covered), S::Following);
    }

    #[test]
    fn a_handoff_drains_then_stays_until_the_folder_is_a_mirror_again() {
        let draining = step(S::Following, I::HandoffQueued);
        assert_eq!(draining, S::Draining);
        assert_eq!(step(draining, I::Covered), S::Draining);
        let off = step(draining, I::HandedOff);
        assert_eq!(off, S::HandedOff);
        assert_eq!(step(off, I::Covered), S::HandedOff);
        assert_eq!(step(off, I::Mirrored), S::Discovering);
        assert_eq!(
            step(draining, I::Mirrored),
            S::Auditing(Doubt::Suspension),
            "a switch called off"
        );
    }

    #[test]
    fn roots_begin_once_and_feed_every_folder() {
        let roots = Roots::default();
        roots.input(1, I::Covered);
        assert_eq!(roots.state(1), None, "not begun");
        roots.begin(1, false);
        roots.begin(2, true);
        assert_eq!(roots.state(2), Some(S::Auditing(Doubt::Restart)));
        roots.input_all(I::Covered);
        roots.begin(1, false);
        assert_eq!(roots.state(1), Some(S::Following), "begun only once");
        roots.input_all(I::Doubted(Doubt::EventGap));
        assert_eq!(roots.state(2), Some(S::Auditing(Doubt::EventGap)));
        roots.forget(2);
        assert_eq!(roots.state(2), None);
    }
}
