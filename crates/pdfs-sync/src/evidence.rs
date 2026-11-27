//! What a pass knows about a path on each side, as a typed fact.
//!
//! A path missing from a walk is not proof that it is gone. The local scan
//! leaves out what it cannot read, the remote walk what would not decrypt or
//! list, and the ignore rules what the user excluded. Before this module each
//! of those was a filter of its own ([`Unscanned`], [`RemoteGaps`],
//! [`IgnoreRules`]), applied at each place that read the walks. Here they
//! answer one question per side, [`Presence`]: the path is there, it is
//! proven absent, it is excluded by rule, or nothing is known of it and why.
//!
//! Only [`Presence::Absent`] may lead to a deletion. A pass classifies a path
//! only when both sides are known ([`Evidence::classifiable`]), and every
//! deletion it plans is checked against the evidence again
//! ([`Evidence::unproven_deletion`]). That check acts on nothing yet: it
//! counts and logs a deletion the evidence does not support, so a regression
//! in the filters shows up in the pass trace before it can cost data.

use std::collections::HashMap;

use proton_drive_rs::proton_sdk::ids::NodeUid;

use crate::planner::{HasKind, RemoteGaps, Unscanned};
use pdfs_core::syncignore::IgnoreRules;

/// What a pass knows about one path on one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The walk returned it.
    Present,
    /// The walk read the folder it would be in, completely, and it was not
    /// there.
    Absent,
    /// The ignore rules exclude it, so the walks do not look at it.
    Excluded,
    /// The walk could not tell.
    Unknown(Unknown),
}

/// Why a walk could not tell whether a path is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unknown {
    /// Here: the path, or a folder above it, cannot be read.
    Unreadable,
    /// Here: the path, or a folder above it, vanished while the scan looked
    /// at it. The next pass sees it settled.
    Vanished,
    /// On Drive: the path is listed, but what the pass needs of it could not
    /// be read.
    Unread,
    /// On Drive: the listing of the deepest folder above it named nodes that
    /// did not come back, so it may be one of them.
    ShortListing,
    /// On Drive: the listing of the deepest folder above it failed.
    Unlisted,
}

impl Presence {
    /// Whether the side is known: there, or proven gone.
    pub fn is_known(self) -> bool {
        matches!(self, Presence::Present | Presence::Absent)
    }
}

/// Which side a deletion acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The local copy is deleted, because the path is gone from Drive.
    Local,
    /// The copy on Drive is trashed, because the path is gone here.
    Remote,
}

/// The evidence of one pass: both walks and what each could not read.
pub struct Evidence<'a, L, R> {
    pub rules: &'a IgnoreRules,
    pub local: &'a HashMap<String, L>,
    pub unscanned: &'a Unscanned,
    /// `None` for a pass that does not walk Drive, such as the push pass
    /// before a switch to on-demand. Every remote path is then unknown.
    pub remote: Option<RemoteEvidence<'a, R>>,
}

/// The remote half of [`Evidence`].
pub struct RemoteEvidence<'a, R> {
    pub entries: &'a HashMap<String, R>,
    pub dirs: &'a HashMap<String, NodeUid>,
    pub gaps: &'a RemoteGaps,
}

impl<L: HasKind, R: HasKind> Evidence<'_, L, R> {
    /// Whether the ignore rules exclude `rel`. A path neither walk returned
    /// has no kind, so it is tested as both: erring towards "excluded" only
    /// leaves a path alone, never deletes it.
    pub fn excluded(&self, rel: &str) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let kind = self.local.get(rel).map(HasKind::is_dir).or_else(|| {
            self.remote
                .as_ref()
                .and_then(|remote| remote.entries.get(rel))
                .map(HasKind::is_dir)
        });
        match kind {
            Some(is_dir) => self.rules.is_ignored(rel, is_dir),
            None => self.rules.is_ignored(rel, false) || self.rules.is_ignored(rel, true),
        }
    }

    /// What the local walk says of `rel`.
    pub fn local(&self, rel: &str) -> Presence {
        if self.excluded(rel) {
            return Presence::Excluded;
        }
        // Not even a path the scan returned is known when a folder above it
        // could not be read: a pass leaves the whole subtree alone (B196).
        if let Some(why) = self.unscanned.why(rel) {
            return Presence::Unknown(why);
        }
        if self.local.contains_key(rel) {
            Presence::Present
        } else {
            Presence::Absent
        }
    }

    /// What the remote walk says of `rel`.
    pub fn remote(&self, rel: &str) -> Presence {
        if self.excluded(rel) {
            return Presence::Excluded;
        }
        let Some(remote) = &self.remote else {
            return Presence::Unknown(Unknown::Unlisted);
        };
        if remote.entries.contains_key(rel) {
            return Presence::Present;
        }
        match remote.gaps.why(rel, remote.entries, remote.dirs) {
            Some(why) => Presence::Unknown(why),
            None => Presence::Absent,
        }
    }

    /// Whether a pass may classify `rel`: neither side excluded, and both
    /// known. A path under a folder the local scan could not read is left out
    /// even when Drive has it, since a download has nowhere to be written.
    pub fn classifiable(&self, rel: &str) -> bool {
        self.local(rel).is_known() && self.remote(rel).is_known()
    }

    /// What the evidence says of the side a deletion of `rel` is based on,
    /// when that is not proof of absence. A deletion of the local copy needs
    /// `rel` proven gone from Drive, and a trash on Drive needs it proven
    /// gone here.
    pub fn unproven_deletion(&self, rel: &str, side: Side) -> Option<Presence> {
        let basis = match side {
            Side::Local => self.remote(rel),
            Side::Remote => self.local(rel),
        };
        (basis != Presence::Absent).then_some(basis)
    }
}

#[cfg(test)]
mod tests {
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    use super::*;

    fn none() -> IgnoreRules {
        IgnoreRules::load(std::path::Path::new("/nonexistent-sync-root"), &[])
    }

    struct Item(bool);

    impl HasKind for Item {
        fn is_dir(&self) -> bool {
            self.0
        }
    }

    fn map(paths: &[(&str, bool)]) -> HashMap<String, Item> {
        paths
            .iter()
            .map(|(rel, dir)| (rel.to_string(), Item(*dir)))
            .collect()
    }

    fn dirs(paths: &[&str]) -> HashMap<String, NodeUid> {
        paths
            .iter()
            .map(|rel| {
                (
                    rel.to_string(),
                    NodeUid::new(VolumeId::from("v"), LinkId::from(*rel)),
                )
            })
            .collect()
    }

    #[test]
    fn a_path_missing_from_a_complete_walk_is_absent() {
        let rules = none();
        let local = map(&[("a.txt", false)]);
        let remote = map(&[("a.txt", false), ("b.txt", false)]);
        let remote_dirs = dirs(&[""]);
        let (unscanned, gaps) = (Unscanned::default(), RemoteGaps::default());
        let evidence = Evidence {
            rules: &rules,
            local: &local,
            unscanned: &unscanned,
            remote: Some(RemoteEvidence {
                entries: &remote,
                dirs: &remote_dirs,
                gaps: &gaps,
            }),
        };
        assert_eq!(evidence.local("a.txt"), Presence::Present);
        assert_eq!(evidence.local("b.txt"), Presence::Absent);
        assert_eq!(evidence.remote("c.txt"), Presence::Absent);
        assert!(evidence.classifiable("b.txt"));
        assert_eq!(evidence.unproven_deletion("b.txt", Side::Remote), None);
        assert_eq!(
            evidence.unproven_deletion("a.txt", Side::Remote),
            Some(Presence::Present)
        );
    }

    #[test]
    fn what_a_walk_could_not_read_is_unknown_never_absent() {
        let rules = none();
        let local = map(&[("locked", true)]);
        let mut unscanned = Unscanned::default();
        unscanned.unreadable("locked".into(), "permission denied");
        unscanned.vanished("tmp".into());
        let remote = map(&[("docs", true), ("docs/a.txt", false), ("pics", true)]);
        let remote_dirs = dirs(&["", "docs", "pics"]);
        let mut gaps = RemoteGaps::default();
        gaps.short_listing("docs");
        gaps.unlisted("pics");
        gaps.unread("odd.bin".into());
        let evidence = Evidence {
            rules: &rules,
            local: &local,
            unscanned: &unscanned,
            remote: Some(RemoteEvidence {
                entries: &remote,
                dirs: &remote_dirs,
                gaps: &gaps,
            }),
        };
        assert_eq!(
            evidence.local("locked"),
            Presence::Unknown(Unknown::Unreadable)
        );
        assert_eq!(
            evidence.local("locked/deep/x.txt"),
            Presence::Unknown(Unknown::Unreadable)
        );
        assert_eq!(
            evidence.local("tmp/y"),
            Presence::Unknown(Unknown::Vanished)
        );
        assert_eq!(
            evidence.remote("docs/b.txt"),
            Presence::Unknown(Unknown::ShortListing)
        );
        assert_eq!(evidence.remote("docs/a.txt"), Presence::Present);
        assert_eq!(
            evidence.remote("pics/2019/x.jpg"),
            Presence::Unknown(Unknown::Unlisted)
        );
        assert_eq!(
            evidence.remote("odd.bin"),
            Presence::Unknown(Unknown::Unread)
        );
        assert_eq!(evidence.remote("top.txt"), Presence::Absent);
        assert!(!evidence.classifiable("locked/deep/x.txt"));
        assert!(!evidence.classifiable("docs/b.txt"));
        assert_eq!(
            evidence.unproven_deletion("locked/deep/x.txt", Side::Remote),
            Some(Presence::Unknown(Unknown::Unreadable))
        );
        assert_eq!(
            evidence.unproven_deletion("docs/b.txt", Side::Local),
            Some(Presence::Unknown(Unknown::ShortListing))
        );
    }

    #[test]
    fn an_ignored_path_is_excluded_on_both_sides() {
        let rules =
            IgnoreRules::with_folder_text(std::path::Path::new("/r"), &[], "*.tmp\nbuild/\n");
        let local = map(&[("a.tmp", false), ("build", true)]);
        let remote: HashMap<String, Item> = HashMap::new();
        let (unscanned, remote_dirs, gaps) =
            (Unscanned::default(), dirs(&[""]), RemoteGaps::default());
        let evidence = Evidence {
            rules: &rules,
            local: &local,
            unscanned: &unscanned,
            remote: Some(RemoteEvidence {
                entries: &remote,
                dirs: &remote_dirs,
                gaps: &gaps,
            }),
        };
        assert_eq!(evidence.local("a.tmp"), Presence::Excluded);
        assert_eq!(evidence.remote("a.tmp"), Presence::Excluded);
        assert_eq!(evidence.local("build"), Presence::Excluded);
        // Neither walk returned it, so it is tested as a folder too.
        assert_eq!(evidence.remote("build"), Presence::Excluded);
        assert!(!evidence.classifiable("a.tmp"));
        assert_eq!(
            evidence.unproven_deletion("a.tmp", Side::Remote),
            Some(Presence::Excluded)
        );
    }

    #[test]
    fn a_pass_that_does_not_walk_drive_knows_nothing_there() {
        let rules = none();
        let local = map(&[]);
        let unscanned = Unscanned::default();
        let evidence: Evidence<'_, Item, Item> = Evidence {
            rules: &rules,
            local: &local,
            unscanned: &unscanned,
            remote: None,
        };
        assert_eq!(evidence.local("gone.txt"), Presence::Absent);
        assert!(!evidence.remote("gone.txt").is_known());
        assert!(!evidence.classifiable("gone.txt"));
        assert_eq!(evidence.unproven_deletion("gone.txt", Side::Remote), None);
    }
}
