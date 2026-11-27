//! The root reducer: what a mirror folder's sync knows about its coverage,
//! as one state machine per folder (`docs/SYNC_ENGINE.md`).
//!
//! [`step`] is pure: it takes the state and one input and returns the next
//! state. Time, the watcher, the event stream and the passes stay outside, as
//! inputs, so a sequence of inputs replays exactly.
//!
//! The phases say how far the engine can trust what it has seen:
//!
//! - **Discovering**: no pass over the folder has finished yet.
//! - **Following**: passes covered the whole folder, and nothing since says a
//!   change could have gone unreported. A pass may work from the dirty sets.
//! - **Auditing**: something did (a lost watch, an overflowed queue, a gap in
//!   Drive's event stream, a restart). Only a pass that walks the doubted
//!   side in full brings it back to Following.
//! - **Suspended**: the folder cannot be synced for now: its local root is
//!   missing, or sync is paused.
//! - **Draining**: a switch to on-demand is queued; passes only push.
//! - **HandedOff**: the folder is on-demand.
//!
//! A doubt is about one side. The disk is followed by the watcher and Drive
//! by its event stream, and each can fail on its own: a restart misses what
//! happened on disk while the daemon was down, but the event stream resumes
//! from its cursor. So the state keeps a doubt for each side ([`Owed`]), and
//! a pass walks in full only the side that is owed.
//!
//! A pass asks [`Roots::owed`] before it walks. The checks a pass made
//! before the reducer was acted on (a trusted watch, a fresh event stream,
//! the audit intervals) still apply on top: the reducer can make a pass walk
//! more, never less.

use std::collections::HashMap;
use std::fmt;

use parking_lot::Mutex;
use tracing::info;

/// Why a side of a folder is in doubt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doubt {
    /// Nothing has walked this side yet.
    FirstSync,
    /// The daemon started: nothing watched the disk while it was down.
    Restart,
    /// The filesystem watcher reported an error, or the folder's watch is
    /// gone.
    WatchLost,
    /// The watcher's event queue overflowed.
    Overflow,
    /// Drive's event stream lost changes.
    EventGap,
    /// The folder only pushed for a while, which drops its picture of Drive.
    Pushed,
}

impl Doubt {
    /// Whether this doubt is about the disk, and whether about Drive.
    fn sides(self) -> (bool, bool) {
        match self {
            Doubt::FirstSync => (true, true),
            Doubt::Restart | Doubt::WatchLost | Doubt::Overflow => (true, false),
            Doubt::EventGap | Doubt::Pushed => (false, true),
        }
    }
}

/// What a pass owes a folder: the sides it must walk in full, each with the
/// first doubt that made it so.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Owed {
    pub local: Option<Doubt>,
    pub remote: Option<Doubt>,
}

impl Owed {
    /// Both sides, for a folder nothing has walked.
    pub const ALL: Owed = Owed {
        local: Some(Doubt::FirstSync),
        remote: Some(Doubt::FirstSync),
    };

    pub fn is_empty(&self) -> bool {
        self.local.is_none() && self.remote.is_none()
    }

    /// Add `doubt` to the sides it is about. The first doubt on a side
    /// stays: it is the one to report.
    fn doubt(self, doubt: Doubt) -> Owed {
        let (local, remote) = doubt.sides();
        Owed {
            local: self.local.or(local.then_some(doubt)),
            remote: self.remote.or(remote.then_some(doubt)),
        }
    }

    /// What is left once a pass walked the sides given in full.
    fn cover(self, local: bool, remote: bool) -> Owed {
        Owed {
            local: self.local.filter(|_| !local),
            remote: self.remote.filter(|_| !remote),
        }
    }
}

impl fmt::Display for Owed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.local, self.remote) {
            (None, None) => f.write_str("nothing"),
            (Some(l), None) => write!(f, "local: {l:?}"),
            (None, Some(r)) => write!(f, "remote: {r:?}"),
            (Some(l), Some(r)) => write!(f, "local: {l:?}, remote: {r:?}"),
        }
    }
}

/// Why a folder is not synced for now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suspension {
    RootMissing,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Discovering,
    Following,
    Auditing,
    Suspended(Suspension),
    Draining,
    HandedOff,
}

/// A folder's state: its phase, and what a pass owes it. Following owes
/// nothing and Auditing owes something; a suspended folder keeps what it is
/// owed for when it comes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootState {
    pub phase: Phase,
    pub owed: Owed,
}

impl RootState {
    /// A folder no pass has walked.
    pub const NEW: RootState = RootState {
        phase: Phase::Discovering,
        owed: Owed::ALL,
    };

    /// A folder synced before the daemon started: what happened on disk
    /// meanwhile went unwatched. Drive's event stream resumes from its
    /// cursor, and reports a gap when it cannot.
    pub const RESTARTED: RootState = RootState {
        phase: Phase::Auditing,
        owed: Owed {
            local: Some(Doubt::Restart),
            remote: None,
        },
    };

    /// Following once nothing is owed. Otherwise a folder still discovering
    /// stays so, and any other is audited.
    fn settled(phase: Phase, owed: Owed) -> RootState {
        let phase = match phase {
            _ if owed.is_empty() => Phase::Following,
            Phase::Discovering => Phase::Discovering,
            _ => Phase::Auditing,
        };
        RootState { phase, owed }
    }
}

/// Something that happened to a folder, as the reducer takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootInput {
    /// A pass finished. It walked the disk in full when `local`, and Drive
    /// when `remote`; otherwise it worked from that side's dirty set.
    Covered { local: bool, remote: bool },
    /// A change may have gone unreported.
    Doubted(Doubt),
    /// The folder cannot be synced for now.
    Suspended(Suspension),
    /// Sync is no longer paused. The watcher and the event stream kept
    /// running meanwhile, so the folder owes what it owed before. A missing
    /// root is only found again by a pass that covers it.
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
    use Phase as P;
    let RootState { phase, owed } = state;
    let with = |phase| RootState { phase, owed };
    match (phase, input) {
        // An on-demand folder is not synced: only going back to a mirror
        // changes it, and that is a first sync over again.
        (P::HandedOff, RootInput::Mirrored) => RootState::NEW,
        (P::HandedOff, _) => state,
        (P::Draining, RootInput::HandedOff) => with(P::HandedOff),
        // A switch called off: a mirror again. The push passes dropped the
        // picture of Drive; the disk stayed watched.
        (P::Draining, RootInput::Mirrored) => {
            RootState::settled(P::Auditing, owed.doubt(Doubt::Pushed))
        }
        (P::Draining, _) => state,
        (_, RootInput::HandoffQueued) => with(P::Draining),
        (_, RootInput::HandedOff) => with(P::HandedOff),
        // A suspended folder is still watched, and owes what went wrong
        // meanwhile when it comes back.
        (P::Suspended(_), RootInput::Doubted(why)) => RootState {
            phase,
            owed: owed.doubt(why),
        },
        (P::Suspended(_), RootInput::Covered { local, remote }) => {
            RootState::settled(P::Auditing, owed.cover(local, remote))
        }
        (P::Suspended(Suspension::Paused), RootInput::Resumed) => {
            RootState::settled(P::Auditing, owed)
        }
        (P::Suspended(_), RootInput::Suspended(why)) => with(P::Suspended(why)),
        (P::Suspended(_), _) => state,
        (_, RootInput::Suspended(why)) => with(P::Suspended(why)),
        (_, RootInput::Doubted(why)) => RootState::settled(phase, owed.doubt(why)),
        (_, RootInput::Covered { local, remote }) => {
            RootState::settled(phase, owed.cover(local, remote))
        }
        (_, RootInput::Resumed | RootInput::Mirrored) => state,
    }
}

impl fmt::Display for RootState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.phase {
            Phase::Discovering => f.write_str("discovering")?,
            Phase::Following => f.write_str("following")?,
            Phase::Auditing => f.write_str("auditing")?,
            Phase::Suspended(why) => write!(f, "suspended ({why:?})")?,
            Phase::Draining => f.write_str("draining")?,
            Phase::HandedOff => f.write_str("handed off")?,
        }
        if !self.owed.is_empty() && self.phase != Phase::Discovering {
            write!(f, " ({})", self.owed)?;
        }
        Ok(())
    }
}

/// The root state of every mirror folder the engine has seen, by id.
#[derive(Default)]
pub struct Roots {
    states: Mutex<HashMap<i64, RootState>>,
}

impl Roots {
    /// Start following `folder`, unless it already is: from discovery for a
    /// folder never synced, and otherwise from an audit of the disk, since
    /// nothing watched it while the daemon was down.
    pub fn begin(&self, folder: i64, synced_before: bool) {
        self.states
            .lock()
            .entry(folder)
            .or_insert(if synced_before {
                RootState::RESTARTED
            } else {
                RootState::NEW
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

    /// What a pass over `folder` must walk in full. A folder not begun is
    /// owed everything.
    pub fn owed(&self, folder: i64) -> Owed {
        self.state(folder).map_or(Owed::ALL, |state| state.owed)
    }

    /// Forget a folder that is no longer synced.
    pub fn forget(&self, folder: i64) {
        self.states.lock().remove(&folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Phase as P;
    use RootInput as I;

    fn run(from: RootState, inputs: &[RootInput]) -> RootState {
        inputs.iter().fold(from, |state, input| step(state, *input))
    }

    const BOTH: RootInput = I::Covered {
        local: true,
        remote: true,
    };
    const NEITHER: RootInput = I::Covered {
        local: false,
        remote: false,
    };
    const LOCAL: RootInput = I::Covered {
        local: true,
        remote: false,
    };
    const REMOTE: RootInput = I::Covered {
        local: false,
        remote: true,
    };
    const FOLLOWING: RootState = RootState {
        phase: P::Following,
        owed: Owed {
            local: None,
            remote: None,
        },
    };

    fn auditing(local: Option<Doubt>, remote: Option<Doubt>) -> RootState {
        RootState {
            phase: P::Auditing,
            owed: Owed { local, remote },
        }
    }

    #[test]
    fn a_first_sync_follows_once_a_pass_walks_both_sides() {
        assert_eq!(run(RootState::NEW, &[BOTH]), FOLLOWING);
        let half = run(RootState::NEW, &[LOCAL]);
        assert_eq!(half.phase, P::Discovering);
        assert_eq!(half.owed.remote, Some(Doubt::FirstSync));
        assert_eq!(step(half, REMOTE), FOLLOWING);
        // A doubt during discovery changes nothing: the pass walks it all.
        assert_eq!(
            run(RootState::NEW, &[I::Doubted(Doubt::Overflow)]),
            RootState::NEW
        );
    }

    #[test]
    fn a_pass_from_the_dirty_sets_keeps_a_followed_folder_following() {
        assert_eq!(run(FOLLOWING, &[NEITHER, NEITHER]), FOLLOWING);
    }

    #[test]
    fn a_doubt_is_owed_on_its_side_until_a_pass_walks_that_side() {
        let audit = run(
            FOLLOWING,
            &[
                I::Doubted(Doubt::WatchLost),
                I::Doubted(Doubt::Overflow),
                I::Doubted(Doubt::EventGap),
            ],
        );
        assert_eq!(
            audit,
            auditing(Some(Doubt::WatchLost), Some(Doubt::EventGap)),
            "the first doubt on each side stays"
        );
        let local_done = step(audit, LOCAL);
        assert_eq!(local_done, auditing(None, Some(Doubt::EventGap)));
        assert_eq!(step(local_done, NEITHER), local_done);
        assert_eq!(step(local_done, REMOTE), FOLLOWING);
    }

    #[test]
    fn a_restart_owes_the_disk_and_not_drive() {
        assert_eq!(RootState::RESTARTED, auditing(Some(Doubt::Restart), None));
        assert_eq!(step(RootState::RESTARTED, LOCAL), FOLLOWING);
        assert_eq!(step(RootState::RESTARTED, REMOTE), RootState::RESTARTED);
    }

    #[test]
    fn a_suspension_keeps_what_is_owed_and_what_goes_wrong_meanwhile() {
        let paused = step(FOLLOWING, I::Suspended(Suspension::Paused));
        assert_eq!(paused.phase, P::Suspended(Suspension::Paused));
        assert_eq!(
            step(paused, I::Resumed),
            FOLLOWING,
            "nothing went unwatched"
        );
        let overflowed = step(paused, I::Doubted(Doubt::Overflow));
        assert_eq!(overflowed.phase, P::Suspended(Suspension::Paused));
        assert_eq!(
            step(overflowed, I::Resumed),
            auditing(Some(Doubt::Overflow), None)
        );
        let missing = step(FOLLOWING, I::Suspended(Suspension::RootMissing));
        assert_eq!(step(missing, I::Resumed), missing);
        assert_eq!(step(missing, NEITHER), FOLLOWING);
        let lost = run(
            FOLLOWING,
            &[
                I::Suspended(Suspension::RootMissing),
                I::Doubted(Doubt::WatchLost),
            ],
        );
        assert_eq!(step(lost, LOCAL), FOLLOWING);
        // Paused during discovery: still owed everything after.
        let paused_new = run(
            RootState::NEW,
            &[I::Suspended(Suspension::Paused), I::Resumed],
        );
        assert_eq!(paused_new.owed, Owed::ALL);
    }

    #[test]
    fn a_handoff_drains_then_stays_until_the_folder_is_a_mirror_again() {
        let draining = step(FOLLOWING, I::HandoffQueued);
        assert_eq!(draining.phase, P::Draining);
        assert_eq!(step(draining, BOTH), draining);
        let off = step(draining, I::HandedOff);
        assert_eq!(off.phase, P::HandedOff);
        assert_eq!(step(off, BOTH), off);
        assert_eq!(step(off, I::Mirrored), RootState::NEW);
        assert_eq!(
            step(draining, I::Mirrored),
            auditing(None, Some(Doubt::Pushed)),
            "a switch called off walks Drive again"
        );
    }

    #[test]
    fn roots_begin_once_and_feed_every_folder() {
        let roots = Roots::default();
        roots.input(1, BOTH);
        assert_eq!(roots.state(1), None, "not begun");
        assert_eq!(roots.owed(1), Owed::ALL);
        roots.begin(1, false);
        roots.begin(2, true);
        assert_eq!(roots.state(2), Some(RootState::RESTARTED));
        roots.input_all(BOTH);
        roots.begin(1, false);
        assert_eq!(roots.state(1), Some(FOLLOWING), "begun only once");
        roots.input_all(I::Doubted(Doubt::EventGap));
        assert_eq!(
            roots.owed(2),
            Owed {
                local: None,
                remote: Some(Doubt::EventGap)
            }
        );
        roots.forget(2);
        assert_eq!(roots.state(2), None);
    }
}
