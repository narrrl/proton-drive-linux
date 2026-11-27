//! What the mount accepted and has not yet sent to Drive (`WriteState`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use proton_drive_rs::proton_sdk::ids::NodeUid;

use pdfs_store::cache::Baseline;

use crate::ids::is_local_uid;
use crate::open::PendingRevision;

/// The write-back's state: writes accepted from the kernel and not yet sent,
/// nodes removed here that a stale listing may still return, and the changes
/// this daemon made that Drive's event feed will echo back.
///
/// The drain, the queue and recovery own it. Everything else, the read path,
/// listings, the pin keeper and the event feed, asks through the methods
/// below instead of taking its locks.
#[derive(Clone, Default)]
pub struct WriteState {
    /// Writes accepted from the kernel but not yet uploaded, keyed by node.
    /// The in-memory face of the `pending_op` table, from
    /// which it is rebuilt on mount.
    ///
    /// Two things read it: [`Core::read_range`], because until the op drains the
    /// staged blob *is* the file's content and the remote still holds the old
    /// revision; and the drain worker, which performs the uploads.
    pub pending: Arc<Mutex<HashMap<NodeUid, PendingRevision>>>,
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
    self_changes: Arc<Mutex<HashMap<NodeUid, SelfChange>>>,
}

impl WriteState {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            hidden: Arc::new(Mutex::new(HashSet::new())),
            self_changes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The write queued for `uid`, if any: its staged blob is the file's
    /// content until the drain sends it.
    pub fn queued(&self, uid: &NodeUid) -> Option<PendingRevision> {
        self.pending.lock().get(uid).cloned()
    }

    /// The remote revision a write to `uid` was made against, for the
    /// conflict check: the queued write's own base while one is queued, and
    /// otherwise the base the handle started from. `None` for a node that has
    /// never existed remotely: there is no revision to conflict with until its
    /// create drains.
    pub fn baseline(
        &self,
        uid: &NodeUid,
        base_mtime: i64,
        base_size: u64,
        base_revision_id: Option<String>,
    ) -> Option<Baseline> {
        if is_local_uid(uid) {
            return None;
        }
        match self.pending.lock().get(uid) {
            Some(p) => p.meta.based_on.clone(),
            None => Some(Baseline {
                mtime: base_mtime,
                size: base_size,
                hash: None,
                revision_id: base_revision_id,
            }),
        }
    }

    /// Whether a write for `uid` is queued and not yet sent. A remote change
    /// to such a node is not applied over it.
    pub fn is_queued(&self, uid: &NodeUid) -> bool {
        self.pending.lock().contains_key(uid)
    }

    /// The size the queued write for `uid` gives the file, if one is queued.
    pub fn queued_len(&self, uid: &NodeUid) -> Option<u64> {
        self.pending.lock().get(uid).map(|queued| queued.meta.len)
    }

    /// The size each queued write gives its file. Empty when nothing is
    /// queued.
    pub fn queued_sizes(&self) -> HashMap<NodeUid, u64> {
        self.pending
            .lock()
            .iter()
            .map(|(uid, queued)| (uid.clone(), queued.meta.len))
            .collect()
    }

    /// The nodes removed here that a listing must leave out, even if Drive
    /// still returns them.
    pub fn hidden(&self) -> HashSet<NodeUid> {
        self.hidden.lock().clone()
    }

    /// Leave `uid` out of listings from now on: it was removed here.
    pub fn hide(&self, uid: &NodeUid) {
        self.hidden.lock().insert(uid.clone());
    }

    /// List `uid` again: it was restored or put back.
    pub fn unhide(&self, uid: &NodeUid) {
        self.hidden.lock().remove(uid);
    }

    /// Record that this daemon just changed `uid` on Drive, so the event
    /// feed's echo of the change is recognised (see [`echoes`]).
    pub fn note_echo(&self, uid: &NodeUid, now_ms: i64) {
        note_self_change(&mut self.self_changes.lock(), uid, now_ms);
    }

    /// Claim one outstanding echo for `uid`: whether the event being applied
    /// is one this daemon caused.
    pub fn take_echo(&self, uid: &NodeUid, now_ms: i64) -> bool {
        take_self_change(&mut self.self_changes.lock(), uid, now_ms)
    }
}

/// How long a change this daemon made stays recognisable as its own. Generous
/// against a feed that can lag, but finite: past it, an event for the node is
/// applied like any other. See [`writes::WriteState`].
pub(crate) const SELF_CHANGE_TTL_MS: i64 = 120_000;

/// A remote change this daemon made, awaiting its echo from the event feed.
pub(crate) struct SelfChange {
    /// When we made it, in the same epoch millis as [`now_millis`].
    at_ms: i64,
    /// Echoes not yet seen. More than one because a single file can be created
    /// and then have a revision uploaded, and the feed reports both.
    echoes: u32,
}

/// Record one remote change this daemon made. Split from [`Core`] so the
/// expiry and echo-counting rules can be tested against a clock the test owns.
pub(crate) fn note_self_change(
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
pub(crate) fn take_self_change(
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

#[cfg(test)]
mod tests {
    use super::*;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    /// An event feed carries no revision id, so the only thing that tells a
    /// remote change apart from the echo of one this daemon just made is this
    /// bookkeeping. Getting it wrong is expensive in both directions: too eager
    /// and we evict the bytes we uploaded and re-download them from an API that
    /// may still be serving the previous revision (a SQLite file on the mount
    /// read back `malformed`); too generous and a real change from another device
    /// is ignored.
    #[test]
    fn a_self_change_is_claimed_once_and_expires() {
        let mut changes = HashMap::new();
        let uid = NodeUid::new(VolumeId::from("vol"), LinkId::from("link"));
        let other = NodeUid::new(VolumeId::from("vol"), LinkId::from("other"));
        let t0 = 1_000_000;

        assert!(
            !take_self_change(&mut changes, &uid, t0),
            "a node we never touched is never ours"
        );

        note_self_change(&mut changes, &uid, t0);
        assert!(
            take_self_change(&mut changes, &uid, t0 + 5_000),
            "the echo of our own change is claimed"
        );
        assert!(
            !take_self_change(&mut changes, &uid, t0 + 6_000),
            "and only once, so a later foreign change still applies"
        );

        // A create followed by a revision upload is two changes to one node, and
        // the feed reports both.
        note_self_change(&mut changes, &uid, t0);
        note_self_change(&mut changes, &uid, t0);
        assert!(take_self_change(&mut changes, &uid, t0 + 1));
        assert!(take_self_change(&mut changes, &uid, t0 + 2));
        assert!(!take_self_change(&mut changes, &uid, t0 + 3));

        // Past the window the attribution is a guess, and a wrong guess leaves
        // the user looking at a stale file.
        note_self_change(&mut changes, &uid, t0);
        assert!(!take_self_change(
            &mut changes,
            &uid,
            t0 + SELF_CHANGE_TTL_MS
        ));
        assert!(
            !changes.contains_key(&uid),
            "an expired change is dropped, not left to be re-tested"
        );

        // An echo that never arrives must not accumulate for the life of the
        // daemon; the next recorded change prunes it.
        note_self_change(&mut changes, &uid, t0);
        note_self_change(&mut changes, &other, t0 + SELF_CHANGE_TTL_MS);
        assert_eq!(
            changes.len(),
            1,
            "recording a change prunes the ones whose echo never came"
        );
    }
}
