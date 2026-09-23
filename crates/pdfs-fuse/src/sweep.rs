//! Background conflict sweep.
//!
//! A `(sync-conflict <ts>)` copy is only ever a fallback: the drain and the sync
//! engine cut one when they cannot prove a queued write is safe to apply, so the
//! user never loses bytes. Most of those copies turn out to be byte-identical to
//! the live file — a spurious conflict from a re-stamped revision (B16/B25) or a
//! first-sync of an already-identical file. This sweep reconciles them after the
//! fact:
//!
//! - **Identical** to the live sibling (same size *and* same content SHA-1): the
//!   copy is redundant, so it is trashed and logged as a [`ActivityKind::Trash`].
//! - **Divergent** (different bytes) or **orphaned** (no live sibling): left in
//!   place and surfaced once as an [`ActivityKind::Conflict`], because only the
//!   user can decide which version wins.
//!
//! Identity is proved without downloading anything: the SHA-1 comes from the
//! revision's decrypted extended attributes, which enumeration already reads. A
//! copy whose sibling has no recorded SHA-1 is treated as divergent — the sweep
//! never removes a file it cannot prove is a duplicate.

use std::collections::HashMap;
use std::time::Duration;

use pdfs_core::batch;
use pdfs_core::config::SweepMode;
use pdfs_core::control::ActivityKind;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{debug, info, warn};

use super::{Core, node_content_sha1, node_revision_id, node_size};

/// Time between sweeps. Conflicts are rare and non-urgent; a slow cadence keeps
/// the sweep off the hot path while still cleaning up within a few minutes.
const CONFLICT_SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Delay before the first sweep, so it does not compete with the burst of work a
/// fresh mount does at startup (hydrate, recover, initial enumeration).
const CONFLICT_SWEEP_WARMUP: Duration = Duration::from_secs(30);

/// Strip a ` (sync-conflict <stamp>)` (optionally `-<n>`) marker from `name`,
/// returning the original file name it was a copy of. `None` when `name` is not a
/// conflict copy. Inverse of the `conflict_name` / `conflict_path_with_suffix`
/// formats, so the two halves of the product agree on what a conflict is.
pub(crate) fn conflict_base_name(name: &str) -> Option<String> {
    const MARKER: &str = " (sync-conflict ";
    let start = name.find(MARKER)?;
    let after = start + MARKER.len();
    let close_rel = name[after..].find(')')?;
    let close = after + close_rel;
    let inner = &name[after..close];
    // The token is `<digits>` or `<digits>-<digits>`; reject anything else so a
    // file that merely happens to contain the phrase is not mistaken for a copy.
    let mut parts = inner.splitn(2, '-');
    let stamp = parts.next().unwrap_or_default();
    let suffix = parts.next();
    let is_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !is_digits(stamp) || suffix.is_some_and(|s| !is_digits(s)) {
        return None;
    }
    let mut base = String::with_capacity(start + name.len() - close);
    base.push_str(&name[..start]);
    base.push_str(&name[close + 1..]);
    Some(base)
}

impl Core {
    /// Sweep for stale conflict copies forever, on its own thread. See the module
    /// docs for the identical/divergent policy.
    pub(crate) fn run_conflict_sweep_loop(&self) {
        // Both waits are interruptible, so teardown does not have to sit out a
        // five-minute interval to join this thread (bugs.md B44).
        if !self.shutdown.sleep(CONFLICT_SWEEP_WARMUP) {
            return;
        }
        loop {
            self.sweep_conflicts_once();
            if !self.shutdown.sleep(CONFLICT_SWEEP_INTERVAL) {
                return;
            }
        }
    }

    /// One sweep pass over every node the daemon currently knows.
    fn sweep_conflicts_once(&self) {
        let stored = match self.db.load_all() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = ?e, "conflict sweep: load nodes failed");
                return;
            }
        };
        let nodes: Vec<_> = stored
            .into_iter()
            .map(|stored| stored.node)
            .filter(|node| self.is_own_or_virtual(&node.uid))
            .collect();

        // Index live (non-trashed) files by (parent, name) for sibling lookup.
        let mut by_parent_name: HashMap<(String, &str), &_> = HashMap::new();
        for node in &nodes {
            if node.trashed || node.is_folder() {
                continue;
            }
            let parent = node
                .parent_uid
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_default();
            by_parent_name.insert((parent, node.name.as_str()), node);
        }

        let online = self.online.load(std::sync::atomic::Ordering::Relaxed);
        for node in &nodes {
            if node.trashed || node.is_folder() {
                continue;
            }
            let Some(base) = conflict_base_name(&node.name) else {
                continue;
            };
            let parent = node
                .parent_uid
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_default();
            let sibling = by_parent_name.get(&(parent, base.as_str())).copied();

            match decide(node, sibling, &base, self.sweep_mode) {
                SweepAction::Remove => {
                    // Only while online — the remote trash is the point. Offline,
                    // it waits for a later pass.
                    if online {
                        self.remove_conflict_copy(node, &base);
                    }
                }
                SweepAction::Flag(detail) => self.flag_conflict(&node.uid, &node.name, detail),
            }
        }
    }

    /// Trash a conflict copy proven identical to its live sibling, and unhook it
    /// locally — the same sequence the interactive unlink path uses after a trash.
    ///
    /// `snapshot` is the node as the pass saw it, which by now is stale: the pass
    /// enumerated every node up front and has been doing network I/O since. Before
    /// touching anything remote this re-reads the node and refuses to act unless
    /// it is *still* the same revision, still untrashed, still unwritten and still
    /// unopened (`docs/BUGS.md` B71). Nothing on this path discards a queued write
    /// or its staged blob: a write that arrives after that check is left for the
    /// drain to resolve against the now-trashed node.
    fn remove_conflict_copy(&self, snapshot: &proton_drive_rs::Node, base: &str) {
        let uid = &snapshot.uid;
        let name = snapshot.name.as_str();
        if !self.duplicate_still_removable(snapshot) {
            return;
        }
        match self
            .rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(uid)))
            .and_then(batch::into_unit)
        {
            Ok(()) => {}
            Err(e) if super::is_gone(&e) => {}
            Err(e) => {
                warn!(%uid, name, error = %e, "conflict sweep: trashing duplicate failed");
                return;
            }
        }
        // Every mount, not just the primary: a conflict copy inside a sync
        // folder lives in that fork's inode space, and leaving it hooked up
        // there keeps a node we have just trashed remotely visible and
        // readable there (`docs/BUGS.md` B74).
        self.for_each_state(|st| {
            st.forget_or_unlink(uid);
        });
        // Deliberately *not* `discard_queued_ops`: `duplicate_still_removable`
        // proved the queue empty before the trash above, but that proof is stale
        // the moment `trash_nodes` goes to the network. A `close(2)` landing in
        // that window stages bytes, and discarding here would unlink the staged
        // blob holding the only copy of them. Leaving the op queued is the
        // non-destructive outcome: the drain re-reads the node, sees it trashed,
        // and `revision_conflict` routes it to `keep_as_conflict_copy`.
        self.cache.evict(uid);
        self.evict_reader(uid);
        if let Err(e) = self.db.delete_node(uid) {
            warn!(%uid, error = ?e, "conflict sweep: db delete after trash failed");
        }
        self.invalidate_trash();
        self.conflict_notified.lock().remove(uid);
        self.log_activity(
            ActivityKind::Trash,
            name,
            format!("auto-removed duplicate of {base}"),
            true,
        );
        info!(%uid, name, base, "conflict sweep: removed identical conflict copy");
    }

    /// Re-check, immediately before the destructive call, everything the pass
    /// decided from its stale snapshot. Any doubt means "leave it alone": a
    /// conflict copy that survives to the next pass costs nothing, a wrongly
    /// removed one costs the user data.
    fn duplicate_still_removable(&self, snapshot: &proton_drive_rs::Node) -> bool {
        let uid = &snapshot.uid;
        let name = snapshot.name.as_str();

        // A queued op means bytes are still owed an upload; trashing the node
        // under them turns an ordinary upload into a conflict fork.
        match self.db.has_any_op(&uid.to_string()) {
            Ok(false) => {}
            Ok(true) => {
                debug!(%uid, name, "conflict sweep: skipping duplicate with queued ops");
                return false;
            }
            // Cannot prove the queue is empty — assume it is not.
            Err(e) => {
                warn!(%uid, name, error = ?e, "conflict sweep: op check failed; skipping");
                return false;
            }
        }

        // Someone is writing to it, or holds it open. Trashing now would pull the
        // node out from under a live handle.
        if self.is_busy(uid) {
            debug!(%uid, name, "conflict sweep: skipping duplicate that is open or being written");
            return false;
        }

        // Finally, the node itself must not have moved on since the snapshot.
        match self.db.node_by_uid(&uid.to_string()) {
            Ok(Some(fresh)) => {
                if fresh.trashed {
                    return false;
                }
                if fresh.name != snapshot.name || fresh.parent_uid != snapshot.parent_uid {
                    debug!(%uid, name, "conflict sweep: duplicate was renamed or moved; skipping");
                    return false;
                }
                if node_revision_id(&fresh) != node_revision_id(snapshot)
                    || node_size(&fresh) != node_size(snapshot)
                    || node_content_sha1(&fresh) != node_content_sha1(snapshot)
                {
                    debug!(%uid, name, "conflict sweep: duplicate changed since the pass began");
                    return false;
                }
                true
            }
            // Already gone locally, or unreadable — either way, not ours to remove.
            Ok(None) => false,
            Err(e) => {
                warn!(%uid, name, error = ?e, "conflict sweep: re-read failed; skipping");
                false
            }
        }
    }

    /// Whether a node has a write in flight or a file handle open against it.
    /// Read-only opens use fh 0 and are not tracked, so this is specifically the
    /// writer/holder check.
    /// Asked across **every** mount. The sweep runs on the primary Core but
    /// trashes nodes anywhere in the tree, and a file open inside a sync folder
    /// is tracked in that fork's `State` — so consulting `self.state` alone
    /// answered "idle" for every such file and let the sweep delete one with a
    /// writer attached (`docs/BUGS.md` B74). Busy anywhere means busy.
    pub(crate) fn is_busy(&self, uid: &NodeUid) -> bool {
        let mut busy = false;
        self.for_each_state(|st| {
            let Some(&ino) = st.by_uid.get(uid) else {
                return;
            };
            busy |=
                st.active_writes.contains_key(&ino) || st.handles.values().any(|&held| held == ino);
        });
        busy
    }

    /// Surface a conflict copy that needs the user's attention, at most once per
    /// daemon run so the activity feed is not spammed each pass.
    fn flag_conflict(&self, uid: &NodeUid, name: &str, detail: String) {
        if !self.conflict_notified.lock().insert(uid.clone()) {
            return;
        }
        debug!(%uid, name, detail, "conflict sweep: flagged divergent conflict copy");
        self.log_activity(ActivityKind::Conflict, name, detail, false);
    }
}

/// What the sweep concluded about one `(sync-conflict …)` copy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SweepAction {
    /// Provably redundant: trash it, subject to the runtime interlocks in
    /// [`Core::duplicate_still_removable`].
    Remove,
    /// Only the user can resolve this one; surface it with this detail.
    Flag(String),
}

/// Decide what to do about conflict copy `node` given its live sibling (if any)
/// and the configured `mode`.
///
/// Pure, so the policy that governs deletion is testable without a mount, a
/// runtime, or an account — the same split used for `revision_changed` and
/// `is_own_self_supersede`. Everything that can fail or race lives in the caller.
fn decide(
    node: &proton_drive_rs::Node,
    sibling: Option<&proton_drive_rs::Node>,
    base: &str,
    mode: SweepMode,
) -> SweepAction {
    match sibling {
        Some(sib) if is_identical(node, sib) => {
            if mode.is_enforcing() {
                SweepAction::Remove
            } else {
                // Report-only: say what would happen, remove nothing. The point is
                // to accumulate field evidence that the identity check is right
                // before it is trusted with deletion (docs/BUGS.md B71).
                SweepAction::Flag(format!(
                    "identical to {base}; would be removed (sweep is report-only)"
                ))
            }
        }
        Some(_) => SweepAction::Flag(format!("differs from {base}")),
        None => SweepAction::Flag(format!("no original {base} to reconcile against")),
    }
}

/// Whether two file nodes provably hold the same bytes: equal size and equal,
/// present content SHA-1. A missing digest on either side is not proof, so the
/// sweep treats it as divergent rather than risk removing a real difference.
fn is_identical(a: &proton_drive_rs::Node, b: &proton_drive_rs::Node) -> bool {
    if node_size(a) != node_size(b) {
        return false;
    }
    match (node_content_sha1(a), node_content_sha1(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_drive_rs::NodeKind;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    /// A file node with the two fields the sweep's identity check reads.
    fn file(name: &str, size: i64, sha1: Option<&str>) -> proton_drive_rs::Node {
        proton_drive_rs::Node {
            uid: NodeUid::new(VolumeId::from("v"), LinkId::from(name)),
            parent_uid: None,
            kind: NodeKind::File {
                media_type: "text/plain".into(),
                total_size_on_storage: size,
                active_revision_state: None,
                active_revision_id: Some("rev".into()),
                claimed_size: Some(size),
                claimed_modification_time: None,
                content_sha1: sha1.map(String::from),
            },
            name: name.into(),
            creation_time: 0,
            modification_time: 0,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            membership: None,
            photo: None,
            album: None,
            verification: Default::default(),
            direct_role: None,
            share_id: None,
        }
    }

    /// The only input shape that may ever produce a removal: same size, same
    /// present digest, live sibling, enforcing mode.
    #[test]
    fn removes_only_a_provably_identical_copy_when_enforcing() {
        let copy = file("f (sync-conflict 1).txt", 100, Some("aaa"));
        let live = file("f.txt", 100, Some("aaa"));
        assert_eq!(
            decide(&copy, Some(&live), "f.txt", SweepMode::Enforce),
            SweepAction::Remove
        );
    }

    #[test]
    fn report_only_never_removes_but_still_says_what_it_would_do() {
        let copy = file("f (sync-conflict 1).txt", 100, Some("aaa"));
        let live = file("f.txt", 100, Some("aaa"));
        // Same inputs as the removal case above; only the mode differs.
        let SweepAction::Flag(detail) = decide(&copy, Some(&live), "f.txt", SweepMode::Report)
        else {
            panic!("report mode must never remove");
        };
        assert!(detail.contains("would be removed"), "{detail}");
        assert!(detail.contains("report-only"), "{detail}");
    }

    #[test]
    fn a_differing_size_is_a_real_conflict() {
        let copy = file("f (sync-conflict 1).txt", 101, Some("aaa"));
        let live = file("f.txt", 100, Some("aaa"));
        assert_eq!(
            decide(&copy, Some(&live), "f.txt", SweepMode::Enforce),
            SweepAction::Flag("differs from f.txt".into())
        );
    }

    #[test]
    fn a_missing_digest_is_never_proof_of_identity() {
        // Equal sizes are not enough: two different 100-byte files are common.
        // A copy the sweep cannot fingerprint must survive, in either direction.
        for (a, b) in [(None, Some("aaa")), (Some("aaa"), None), (None, None)] {
            let copy = file("f (sync-conflict 1).txt", 100, a);
            let live = file("f.txt", 100, b);
            assert_eq!(
                decide(&copy, Some(&live), "f.txt", SweepMode::Enforce),
                SweepAction::Flag("differs from f.txt".into()),
                "{a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn a_differing_digest_at_equal_size_is_a_real_conflict() {
        let copy = file("f (sync-conflict 1).txt", 100, Some("aaa"));
        let live = file("f.txt", 100, Some("bbb"));
        assert_eq!(
            decide(&copy, Some(&live), "f.txt", SweepMode::Enforce),
            SweepAction::Flag("differs from f.txt".into())
        );
    }

    #[test]
    fn an_orphaned_copy_is_surfaced_not_removed() {
        // No live original left to reconcile against — the copy may be the only
        // surviving version of the file, so it is never the sweep's to remove.
        let copy = file("f (sync-conflict 1).txt", 100, Some("aaa"));
        assert_eq!(
            decide(&copy, None, "f.txt", SweepMode::Enforce),
            SweepAction::Flag("no original f.txt to reconcile against".into())
        );
    }

    #[test]
    fn identity_needs_both_size_and_digest_to_match() {
        let a = file("a", 100, Some("aaa"));
        assert!(is_identical(&a, &file("b", 100, Some("aaa"))));
        assert!(!is_identical(&a, &file("b", 101, Some("aaa"))));
        assert!(!is_identical(&a, &file("b", 100, Some("bbb"))));
        assert!(!is_identical(&a, &file("b", 100, None)));
    }

    #[test]
    fn parses_conflict_names_and_leaves_others_alone() {
        assert_eq!(
            conflict_base_name("test_export (sync-conflict 1784898786).xml").as_deref(),
            Some("test_export.xml")
        );
        assert_eq!(
            conflict_base_name("README (sync-conflict 42)").as_deref(),
            Some("README")
        );
        // The sync engine's de-duplicated suffix form.
        assert_eq!(
            conflict_base_name("notes (sync-conflict 1700-3).txt").as_deref(),
            Some("notes.txt")
        );
        // Not a conflict copy.
        assert_eq!(conflict_base_name("report.xml"), None);
        // Looks close but the token is not a stamp.
        assert_eq!(conflict_base_name("x (sync-conflict abc).txt"), None);
    }

    #[test]
    fn a_dotted_stem_keeps_its_inner_dots() {
        assert_eq!(
            conflict_base_name("archive.tar (sync-conflict 7).gz").as_deref(),
            Some("archive.tar.gz")
        );
    }
}
