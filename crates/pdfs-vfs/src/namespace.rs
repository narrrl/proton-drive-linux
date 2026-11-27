//! Rules for namespace operations that need neither the kernel nor Drive.
//!
//! So far: how a replacing rename that a stop cut off is settled on the next
//! start (`docs/BUGS.md` B302). The journal entry is
//! [`pdfs_store::db::ReplaceIntent`].

use proton_drive_rs::Node;
use proton_drive_rs::proton_sdk::ids::NodeUid;

/// What to do with a replacing rename a previous run started and did not end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceOutcome {
    /// It reached its end state, or nothing is left to undo: forget it.
    Done,
    /// The victim's trash is still queued but the rename never was: drop the
    /// queued trash, so the victim stays where it is.
    WithdrawVictimTrash,
    /// The rename did not happen and the victim is in Drive's trash: restore
    /// it.
    RestoreVictim,
    /// Drive cannot be asked now: try again later.
    Wait,
}

/// What the queue says about a cut-off replace: whether the source's rename
/// was queued, and whether the victim's trash is.
///
/// Answers without Drive where it can, which is every case the queued path
/// left: a queued rename carries the end state and the drain finishes it, and
/// a queued trash without a queued rename means the stop came between the two.
/// A victim whose own create was still queued had nothing on Drive to restore.
pub fn settle_from_queue(
    rename_queued: bool,
    victim_trash_queued: bool,
    victim_local: bool,
) -> Option<ReplaceOutcome> {
    if rename_queued {
        return Some(ReplaceOutcome::Done);
    }
    if victim_trash_queued {
        return Some(ReplaceOutcome::WithdrawVictimTrash);
    }
    if victim_local {
        return Some(ReplaceOutcome::Done);
    }
    None
}

/// What Drive says about a cut-off replace that went to Drive inline.
///
/// `source` and `victim` are `None` when Drive could not be asked, and
/// `Some(None)` when it does not have the node. The rename happened when the
/// source sits at the new name; then the victim's removal was meant. Otherwise
/// the victim comes back if it is in the trash, and is left alone if it never
/// got there.
pub fn settle_from_drive(
    new_parent: &NodeUid,
    new_name: &str,
    source: Option<Option<&Node>>,
    victim: Option<Option<&Node>>,
) -> ReplaceOutcome {
    let Some(source) = source else {
        return ReplaceOutcome::Wait;
    };
    let renamed = source.is_some_and(|node| {
        !node.trashed && node.parent_uid.as_ref() == Some(new_parent) && node.name == new_name
    });
    if renamed {
        return ReplaceOutcome::Done;
    }
    match victim {
        None => ReplaceOutcome::Wait,
        Some(Some(node)) if node.trashed => ReplaceOutcome::RestoreVictim,
        Some(_) => ReplaceOutcome::Done,
    }
}

#[cfg(test)]
mod tests {
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    use super::*;

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn node(link: &str, parent: &str, name: &str, trashed: bool) -> Node {
        let mut node = crate::nodes::local_node(uid(link), uid(parent), name.to_owned(), false);
        node.trashed = trashed;
        node
    }

    #[test]
    fn the_queue_settles_what_the_queued_path_left() {
        use ReplaceOutcome::*;
        assert_eq!(settle_from_queue(true, true, false), Some(Done));
        assert_eq!(
            settle_from_queue(false, true, false),
            Some(WithdrawVictimTrash)
        );
        assert_eq!(settle_from_queue(false, false, true), Some(Done));
        assert_eq!(settle_from_queue(false, false, false), None);
    }

    /// The stop came after the victim went to the trash and before the
    /// source was renamed: the victim comes back.
    #[test]
    fn a_trashed_victim_comes_back_when_the_rename_did_not_happen() {
        let source = node("src", "dir", "report.odt.tmp", false);
        let victim = node("old", "dir", "report.odt", true);
        assert_eq!(
            settle_from_drive(
                &uid("dir"),
                "report.odt",
                Some(Some(&source)),
                Some(Some(&victim))
            ),
            ReplaceOutcome::RestoreVictim
        );
    }

    #[test]
    fn a_finished_rename_keeps_its_victim_in_the_trash() {
        let source = node("src", "dir", "report.odt", false);
        let victim = node("old", "dir", "report.odt", true);
        assert_eq!(
            settle_from_drive(
                &uid("dir"),
                "report.odt",
                Some(Some(&source)),
                Some(Some(&victim))
            ),
            ReplaceOutcome::Done
        );
    }

    /// The stop came before the victim was removed: nothing to undo.
    #[test]
    fn a_victim_never_trashed_is_left_alone() {
        let source = node("src", "dir", "report.odt.tmp", false);
        let victim = node("old", "dir", "report.odt", false);
        assert_eq!(
            settle_from_drive(
                &uid("dir"),
                "report.odt",
                Some(Some(&source)),
                Some(Some(&victim))
            ),
            ReplaceOutcome::Done
        );
        assert_eq!(
            settle_from_drive(&uid("dir"), "report.odt", Some(Some(&source)), Some(None)),
            ReplaceOutcome::Done
        );
    }

    #[test]
    fn without_drive_it_waits() {
        let source = node("src", "dir", "report.odt.tmp", false);
        assert_eq!(
            settle_from_drive(&uid("dir"), "report.odt", None, None),
            ReplaceOutcome::Wait
        );
        assert_eq!(
            settle_from_drive(&uid("dir"), "report.odt", Some(Some(&source)), None),
            ReplaceOutcome::Wait
        );
    }
}
