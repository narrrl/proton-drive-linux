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
