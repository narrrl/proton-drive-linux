//! The effect reducer: where one effect a mirror pass sends stands, from the
//! moment it is about to be sent until its result is in
//! (`docs/SYNC_ENGINE.md`).
//!
//! An effect is an upload, a download, a folder made on Drive, and the other
//! [`Pending`](crate::model::Pending) ops. Before one is sent, an intent for it
//! is written to the database (`sync_intent`); when its result is in, the
//! intent is settled or kept with the failure. An intent still `issued` when
//! the daemon starts was sent by a daemon that stopped before it heard back,
//! so the effect may or may not have happened: it is `unsure`.
//!
//! [`step`] is pure, as the root reducer is: the I/O results, the restart and
//! the pass that looks again are inputs.
//!
//! For now the intents are kept in shadow: they record and log what happens,
//! and nothing reads them to decide what to send. The apply step's own checks
//! (a create that may have landed is looked up before it is sent again, B266;
//! a download checks the disk before it replaces a file, B39) still do that.

use std::fmt;

/// Failures in a row after which an effect is reported as blocked.
pub const BLOCKED_AFTER: u32 = 5;

/// The kind of intent for a file or folder removed from disk because Drive
/// deleted it. The other kinds are named by [`Pending::intent`](crate::model::Pending::intent).
pub const DELETE_LOCAL: &str = "delete-local";

/// The kind of intent for a file or folder moved to Drive's trash because it
/// was deleted on disk.
pub const TRASH: &str = "trash";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectState {
    /// Sent, or about to be; no result yet.
    Issued,
    /// Found issued at a start: the daemon stopped before the result came,
    /// so the effect may or may not have happened.
    Unsure,
    /// It failed, and a later pass may send it again.
    Retry,
    /// It failed [`BLOCKED_AFTER`] times in a row.
    Blocked,
    /// Its result is in. The intent goes.
    Settled,
}

impl EffectState {
    /// The name the database keeps.
    pub fn name(self) -> &'static str {
        match self {
            EffectState::Issued => "issued",
            EffectState::Unsure => "unsure",
            EffectState::Retry => "retry",
            EffectState::Blocked => "blocked",
            EffectState::Settled => "settled",
        }
    }

    pub fn from_name(name: &str) -> Option<EffectState> {
        Some(match name {
            "issued" => EffectState::Issued,
            "unsure" => EffectState::Unsure,
            "retry" => EffectState::Retry,
            "blocked" => EffectState::Blocked,
            "settled" => EffectState::Settled,
            _ => return None,
        })
    }
}

impl fmt::Display for EffectState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Something that happened to an effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectInput {
    /// It is sent (again); `attempts` counts this one.
    Issued { attempts: u32 },
    /// It did what it was sent to do.
    Succeeded,
    /// It failed.
    Failed,
    /// The daemon started.
    Restarted,
    /// A pass that began after it was sent looked at its path and did not
    /// send it again: what it was for has changed, or is done.
    Superseded,
}

/// The state after `input`.
pub fn step(state: EffectState, input: EffectInput) -> EffectState {
    use EffectState as S;
    match (state, input) {
        (S::Settled, _) => S::Settled,
        (_, EffectInput::Issued { .. }) => S::Issued,
        (_, EffectInput::Succeeded | EffectInput::Superseded) => S::Settled,
        (S::Issued, EffectInput::Restarted) => S::Unsure,
        (state, EffectInput::Restarted) => state,
        (_, EffectInput::Failed) => S::Retry,
    }
}

/// The state after a failure on the `attempts`th send. Counts are kept
/// outside [`step`], with the intent.
pub fn failed(attempts: u32) -> EffectState {
    if attempts >= BLOCKED_AFTER {
        EffectState::Blocked
    } else {
        step(EffectState::Issued, EffectInput::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use EffectInput as I;
    use EffectState as S;

    #[test]
    fn an_effect_settles_once_it_succeeds() {
        let sent = step(S::Retry, I::Issued { attempts: 2 });
        assert_eq!(sent, S::Issued);
        assert_eq!(step(sent, I::Succeeded), S::Settled);
        assert_eq!(step(S::Settled, I::Failed), S::Settled, "settled stays");
    }

    #[test]
    fn an_effect_sent_before_a_stop_is_unsure_after_the_start() {
        assert_eq!(step(S::Issued, I::Restarted), S::Unsure);
        // Only one that was sent: a failure is known, and stays one.
        assert_eq!(step(S::Retry, I::Restarted), S::Retry);
        assert_eq!(step(S::Blocked, I::Restarted), S::Blocked);
        // Sent again, or found needless by a later pass.
        assert_eq!(step(S::Unsure, I::Issued { attempts: 2 }), S::Issued);
        assert_eq!(step(S::Unsure, I::Superseded), S::Settled);
    }

    #[test]
    fn an_effect_that_keeps_failing_is_blocked() {
        assert_eq!(failed(1), S::Retry);
        assert_eq!(failed(BLOCKED_AFTER - 1), S::Retry);
        assert_eq!(failed(BLOCKED_AFTER), S::Blocked);
        assert_eq!(step(S::Blocked, I::Superseded), S::Settled);
    }

    #[test]
    fn every_state_reads_back_by_its_name() {
        for state in [S::Issued, S::Unsure, S::Retry, S::Blocked, S::Settled] {
            assert_eq!(EffectState::from_name(state.name()), Some(state));
        }
        assert_eq!(EffectState::from_name("planned"), None);
    }
}
