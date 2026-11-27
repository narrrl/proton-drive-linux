//! The local dirty set: where a mirror folder changed on disk since a pass
//! last looked, as the filesystem watcher reports it (`docs/SYNC_ENGINE.md`).
//!
//! The watcher marks each path an event names. A pass claims every mark made
//! so far and, once it has applied what it found, retires them. Marks made
//! while it ran are newer than its claim and stay for the next pass.
//!
//! A claim is only as good as the watch behind it. It is *trusted* when the
//! folder's watch has been live since before the last pass claimed: then
//! every change since that pass was reported. A watch that failed, a watcher
//! error, an overflowed queue or a restart breaks that, and the next pass has
//! nothing to trust until a pass claims again with the watch live throughout.
//!
//! For now the set is checked, not used: a pass still walks the whole folder,
//! and counts the changes it finds that a trusted claim did not cover. Once
//! those stay at zero, a trusted claim can stand in for the walk.

use std::collections::{HashMap, HashSet};

use parking_lot::Mutex;

/// The dirty marks of every mirror folder, by folder id.
#[derive(Default)]
pub struct LocalDirty {
    inner: Mutex<Dirty>,
}

#[derive(Default)]
struct Dirty {
    /// The newest generation, across every folder: of a mark, or of a watch
    /// coming up.
    generation: u64,
    folders: HashMap<i64, Marks>,
}

impl Dirty {
    /// The next generation. A watch that comes up, or comes back after an
    /// overflow, takes one of its own, so it is always later than the last
    /// claim, even when no mark was made in between.
    fn tick(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }
}

#[derive(Default)]
struct Marks {
    /// Folders whose direct children changed, with the generation of the
    /// newest mark on each.
    shallow: HashMap<String, u64>,
    /// Paths that changed in full, with whatever is below them: a file
    /// written, or a folder that appeared with its contents.
    deep: HashMap<String, u64>,
    /// The generation from which the watch has reported every change, or
    /// `None` while the folder is not watched.
    live_since: Option<u64>,
    /// The generation the last pass claimed through.
    claimed: Option<u64>,
}

/// The marks a pass claimed: what the watcher reported up to `through`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Claim {
    pub through: u64,
    /// Whether the watch was live since before the last claim, so the marks
    /// cover every change since.
    pub trusted: bool,
    shallow: HashSet<String>,
    deep: HashSet<String>,
}

impl Claim {
    /// Whether a change to `rel` was reported: its folder's children
    /// changed, or it or a folder above it changed in full.
    pub fn covers(&self, rel: &str) -> bool {
        if self.shallow.contains(parent(rel)) {
            return true;
        }
        let mut at = rel;
        loop {
            if self.deep.contains(at) {
                return true;
            }
            if at.is_empty() {
                return false;
            }
            at = parent(at);
        }
    }

    /// How many marks the claim holds.
    pub fn len(&self) -> usize {
        self.shallow.len() + self.deep.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl LocalDirty {
    /// The watch on `folder` is in place: every change from now on is
    /// reported.
    pub fn watched(&self, folder: i64) {
        let mut d = self.inner.lock();
        let now = d.tick();
        let marks = d.folders.entry(folder).or_default();
        if marks.live_since.is_none() {
            marks.live_since = Some(now);
        }
    }

    /// The watch on `folder` is gone, or never came up.
    pub fn unwatched(&self, folder: i64) {
        if let Some(marks) = self.inner.lock().folders.get_mut(&folder) {
            marks.live_since = None;
        }
    }

    /// The watcher lost events it cannot name: an error, or an overflowed
    /// queue. No folder's marks can be trusted until it is watched again.
    pub fn lost(&self) {
        for marks in self.inner.lock().folders.values_mut() {
            marks.live_since = None;
        }
    }

    /// The watcher's queue overflowed: the changes in it are lost, but the
    /// watches stand, so every folder still watched reports changes again
    /// from now on.
    pub fn overflowed(&self) {
        let mut d = self.inner.lock();
        let now = d.tick();
        for marks in d.folders.values_mut() {
            if marks.live_since.is_some() {
                marks.live_since = Some(now);
            }
        }
    }

    /// Note a change the watcher reported at `rel` in `folder`.
    pub fn mark(&self, folder: i64, rel: &str) {
        let mut d = self.inner.lock();
        let generation = d.tick();
        let marks = d.folders.entry(folder).or_default();
        marks.deep.insert(rel.to_string(), generation);
        marks.shallow.insert(parent(rel).to_string(), generation);
    }

    /// Claim every mark of `folder` made so far, for a pass about to look.
    pub fn claim(&self, folder: i64) -> Claim {
        let mut d = self.inner.lock();
        let through = d.generation;
        let marks = d.folders.entry(folder).or_default();
        Claim {
            through,
            trusted: matches!(
                (marks.live_since, marks.claimed),
                (Some(live), Some(claimed)) if live <= claimed
            ),
            shallow: marks.shallow.keys().cloned().collect(),
            deep: marks.deep.keys().cloned().collect(),
        }
    }

    /// A pass that claimed `claim` has looked. With `applied`, it applied
    /// everything it found, and the marks it claimed are retired; without,
    /// they stay for the next pass to claim again. Either way the next claim
    /// is measured from this one.
    pub fn looked(&self, folder: i64, claim: &Claim, applied: bool) {
        let mut d = self.inner.lock();
        let marks = d.folders.entry(folder).or_default();
        if applied {
            marks.shallow.retain(|_, g| *g > claim.through);
            marks.deep.retain(|_, g| *g > claim.through);
        }
        marks.claimed = Some(claim.through);
    }

    /// Forget a folder that is no longer synced.
    pub fn forget(&self, folder: i64) {
        self.inner.lock().folders.remove(&folder);
    }
}

/// The folder `rel` is in; the root's is the root.
fn parent(rel: &str) -> &str {
    rel.rfind('/').map_or("", |i| &rel[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_covers_a_change_and_its_siblings_folder_but_not_cousins() {
        let dirty = LocalDirty::default();
        dirty.mark(1, "docs/a.txt");
        let claim = dirty.claim(1);
        assert!(claim.covers("docs/a.txt"));
        // A file made or deleted next to it shows in the same folder.
        assert!(claim.covers("docs/b.txt"));
        assert!(!claim.covers("other/c.txt"));
        assert!(!claim.covers("docs/sub/d.txt"));
        assert!(!claim.covers("top.txt"));
    }

    #[test]
    fn a_folder_moved_in_covers_everything_below_it() {
        let dirty = LocalDirty::default();
        dirty.mark(1, "photos/2024");
        let claim = dirty.claim(1);
        assert!(claim.covers("photos/2024/jan/1.jpg"));
        assert!(claim.covers("photos/other"));
        assert!(!claim.covers("music/x.mp3"));
    }

    #[test]
    fn a_claim_is_trusted_only_with_the_watch_live_since_the_last_one() {
        let dirty = LocalDirty::default();
        dirty.watched(1);
        // The first pass after the watch came up has nothing to measure from.
        let first = dirty.claim(1);
        assert!(!first.trusted);
        dirty.looked(1, &first, true);
        assert!(dirty.claim(1).trusted);

        // Events lost: the next claim is not trusted, nor the one after the
        // watch comes back, until a pass has claimed with it live.
        dirty.lost();
        let lost = dirty.claim(1);
        assert!(!lost.trusted);
        dirty.looked(1, &lost, true);
        dirty.watched(1);
        dirty.mark(1, "a.txt");
        let back = dirty.claim(1);
        assert!(!back.trusted, "watched again after the last claim");
        dirty.looked(1, &back, true);
        assert!(dirty.claim(1).trusted);
    }

    #[test]
    fn marks_made_during_a_pass_stay_for_the_next_one() {
        let dirty = LocalDirty::default();
        dirty.mark(1, "a.txt");
        let claim = dirty.claim(1);
        dirty.mark(1, "b/c.txt");
        dirty.looked(1, &claim, true);
        let next = dirty.claim(1);
        assert!(!next.covers("a.txt"));
        assert!(next.covers("b/c.txt"));
    }

    #[test]
    fn a_pass_that_did_not_apply_everything_keeps_its_marks() {
        let dirty = LocalDirty::default();
        dirty.mark(1, "a.txt");
        let claim = dirty.claim(1);
        dirty.looked(1, &claim, false);
        assert!(dirty.claim(1).covers("a.txt"));
    }

    #[test]
    fn an_overflow_breaks_trust_until_a_pass_claims_again() {
        let dirty = LocalDirty::default();
        dirty.watched(1);
        let first = dirty.claim(1);
        dirty.looked(1, &first, true);
        dirty.mark(1, "a.txt");
        dirty.overflowed();
        let lost = dirty.claim(1);
        assert!(!lost.trusted);
        dirty.looked(1, &lost, true);
        assert!(dirty.claim(1).trusted);
    }

    #[test]
    fn folders_keep_their_own_marks() {
        let dirty = LocalDirty::default();
        dirty.mark(1, "a.txt");
        assert!(dirty.claim(2).is_empty());
        dirty.forget(1);
        assert!(dirty.claim(1).is_empty());
    }
}
