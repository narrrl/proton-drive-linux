//! What the mount accepted and has not yet sent to Drive (`WriteState`).

use super::*;

/// The write-back's state: writes accepted from the kernel and not yet sent,
/// nodes removed here that a stale listing may still return, and the changes
/// this daemon made that Drive's event feed will echo back.
///
/// The drain, the queue and recovery own it. Everything else, the read path,
/// listings, the pin keeper and the event feed, asks through the methods
/// below instead of taking its locks.
#[derive(Clone)]
pub(crate) struct WriteState {
    /// Writes accepted from the kernel but not yet uploaded, keyed by node.
    /// The in-memory face of the `pending_op` table, from
    /// which it is rebuilt on mount.
    ///
    /// Two things read it: [`Core::read_range`], because until the op drains the
    /// staged blob *is* the file's content and the remote still holds the old
    /// revision; and the drain worker, which performs the uploads.
    pub(crate) pending: Arc<Mutex<HashMap<NodeUid, PendingRevision>>>,
    /// Nodes removed through this daemon which a briefly stale remote listing
    /// may still return. Uids are not reused; an explicit restore clears one.
    hidden: Arc<Mutex<HashSet<NodeUid>>>,
    /// Nodes this daemon changed on the remote itself, and how many echoes of
    /// those changes the event feed still owes us.
    ///
    /// The feed replays our own writes back at us seconds later, and a
    /// `NodeUpdated` carries no revision id — nothing in it distinguishes "another
    /// device changed this file" from "you changed this file". Treating the echo
    /// as foreign is not free: it evicts the content blob we just uploaded, so the
    /// next read re-downloads a revision the API may not be serving yet. That is
    /// how a SQLite database written on the mount came back `malformed`, and how a
    /// mount doing bulk uploads spent its time re-fetching its own bytes.
    ///
    /// So the first event for a node we changed ourselves, within
    /// [`SELF_CHANGE_TTL_MS`], is consumed rather than applied: the tree already
    /// holds what that event describes, because [`Core::refresh_after_upload`] put
    /// it there. Anything beyond that applies normally, which bounds the staleness
    /// a genuine remote change can suffer to one event.
    ///
    /// Shared across forks like `pending`: the drain that records the change and
    /// the event task that consumes the echo are both per-daemon.
    self_changes: Arc<Mutex<HashMap<NodeUid, echoes::SelfChange>>>,
}

impl WriteState {
    pub(crate) fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            hidden: Arc::new(Mutex::new(HashSet::new())),
            self_changes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The write queued for `uid`, if any: its staged blob is the file's
    /// content until the drain sends it.
    pub(crate) fn queued(&self, uid: &NodeUid) -> Option<PendingRevision> {
        self.pending.lock().get(uid).cloned()
    }

    /// Whether a write for `uid` is queued and not yet sent. A remote change
    /// to such a node is not applied over it.
    pub(crate) fn is_queued(&self, uid: &NodeUid) -> bool {
        self.pending.lock().contains_key(uid)
    }

    /// The size the queued write for `uid` gives the file, if one is queued.
    pub(crate) fn queued_len(&self, uid: &NodeUid) -> Option<u64> {
        self.pending.lock().get(uid).map(|queued| queued.meta.len)
    }

    /// The size each queued write gives its file. Empty when nothing is
    /// queued.
    pub(crate) fn queued_sizes(&self) -> HashMap<NodeUid, u64> {
        self.pending
            .lock()
            .iter()
            .map(|(uid, queued)| (uid.clone(), queued.meta.len))
            .collect()
    }

    /// The nodes removed here that a listing must leave out, even if Drive
    /// still returns them.
    pub(crate) fn hidden(&self) -> HashSet<NodeUid> {
        self.hidden.lock().clone()
    }

    /// Leave `uid` out of listings from now on: it was removed here.
    pub(crate) fn hide(&self, uid: &NodeUid) {
        self.hidden.lock().insert(uid.clone());
    }

    /// List `uid` again: it was restored or put back.
    pub(crate) fn unhide(&self, uid: &NodeUid) {
        self.hidden.lock().remove(uid);
    }

    /// Record that this daemon just changed `uid` on Drive, so the event
    /// feed's echo of the change is recognised (see [`echoes`]).
    pub(crate) fn note_echo(&self, uid: &NodeUid, now_ms: i64) {
        echoes::note_self_change(&mut self.self_changes.lock(), uid, now_ms);
    }

    /// Claim one outstanding echo for `uid`: whether the event being applied
    /// is one this daemon caused.
    pub(crate) fn take_echo(&self, uid: &NodeUid, now_ms: i64) -> bool {
        echoes::take_self_change(&mut self.self_changes.lock(), uid, now_ms)
    }
}
