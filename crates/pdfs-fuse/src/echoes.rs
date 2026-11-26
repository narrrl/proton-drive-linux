//! Our own changes echoed back by the event feed: which nodes this daemon
//! changed on Drive, so the first event for each is taken as the echo it is.

use super::*;

impl Core {
    /// Record that this daemon just changed `uid` on the remote, so the event
    /// feed's echo of that change is recognised instead of re-applied. Called
    /// from the drain once the remote has actually accepted the change and the
    /// tree has been brought level with it. See [`writes::WriteState`].
    pub(crate) fn note_self_change(&self, uid: &NodeUid) {
        self.writes.note_echo(uid, now_millis());
    }

    /// Claim one outstanding echo for `uid`, reporting whether this event is one
    /// this daemon caused. Consuming rather than merely testing is what bounds
    /// the suppression: the next event for the node applies normally.
    pub(super) fn take_self_change(&self, uid: &NodeUid) -> bool {
        self.writes.take_echo(uid, now_millis())
    }
}

/// How long a change this daemon made stays recognisable as its own. Generous
/// against a feed that can lag, but finite: past it, an event for the node is
/// applied like any other. See [`writes::WriteState`].
pub(super) const SELF_CHANGE_TTL_MS: i64 = 120_000;

/// A remote change this daemon made, awaiting its echo from the event feed.
pub(super) struct SelfChange {
    /// When we made it, in the same epoch millis as [`now_millis`].
    at_ms: i64,
    /// Echoes not yet seen. More than one because a single file can be created
    /// and then have a revision uploaded, and the feed reports both.
    echoes: u32,
}

/// Record one remote change this daemon made. Split from [`Core`] so the
/// expiry and echo-counting rules can be tested against a clock the test owns.
pub(super) fn note_self_change(
    changes: &mut HashMap<NodeUid, SelfChange>,
    uid: &NodeUid,
    now_ms: i64,
) {
    // Echoes that never arrived — a node trashed before its event came round, a
    // feed that skipped it — would otherwise accumulate for the life of the
    // daemon. Pruning on write keeps the map the size of the recent queue.
    changes.retain(|_, c| now_ms - c.at_ms < SELF_CHANGE_TTL_MS);
    let change = changes.entry(uid.clone()).or_insert(SelfChange {
        at_ms: now_ms,
        echoes: 0,
    });
    change.at_ms = now_ms;
    change.echoes += 1;
}

/// Claim one echo, per [`Core::take_self_change`].
pub(super) fn take_self_change(
    changes: &mut HashMap<NodeUid, SelfChange>,
    uid: &NodeUid,
    now_ms: i64,
) -> bool {
    let Some(change) = changes.get_mut(uid) else {
        return false;
    };
    // Too old to attribute: the feed is far enough behind that this is as likely
    // to be someone else's change as ours, and guessing wrong here costs the user
    // a stale file.
    if now_ms - change.at_ms >= SELF_CHANGE_TTL_MS {
        changes.remove(uid);
        return false;
    }
    change.echoes -= 1;
    if change.echoes == 0 {
        changes.remove(uid);
    }
    true
}
