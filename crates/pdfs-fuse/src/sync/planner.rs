//! Pure reconciliation ordering, safety guards, signatures, and path rules.

use super::*;

/// The paths a reconcile pass will classify: the union of the local, remote,
/// and baseline states, minus anything the ignore rules exclude, shallowest
/// first so a parent folder is created before its children are placed in it.
///
/// The ignore filter belongs *here*, on the union, rather than on the local walk
/// alone — filtering the walk alone would be actively destructive. A file synced
/// before it became ignored is absent from `local` but present in `remote` and
/// `baseline`, which is precisely the "local deleted, remote untouched" shape
/// `do_reconcile` responds to by trashing the remote copy. Adding a line to
/// `.pdfsignore` must never delete anything from Drive.
///
/// Ignored baseline rows are deliberately left in the database rather than
/// removed: with the row intact, un-ignoring the path later re-adopts the
/// existing remote file instead of reading as a brand-new local one.
pub(super) fn classification_order<L, R, B>(
    local: &HashMap<String, L>,
    remote: &HashMap<String, R>,
    baseline: &HashMap<String, B>,
    rules: &IgnoreRules,
) -> Vec<String>
where
    L: HasKind,
    R: HasKind,
{
    let mut paths: HashSet<String> = HashSet::new();
    paths.extend(local.keys().cloned());
    paths.extend(remote.keys().cloned());
    paths.extend(baseline.keys().cloned());
    let mut order: Vec<String> = paths.into_iter().collect();
    if !rules.is_empty() {
        order.retain(|rel| {
            // A baseline-only path has no kind recorded, so it is tested as
            // both; see `filter_baseline` for why erring towards "ignored" is
            // the safe direction here.
            let kind = local
                .get(rel)
                .map(HasKind::is_dir)
                .or_else(|| remote.get(rel).map(HasKind::is_dir));
            match kind {
                Some(is_dir) => !rules.is_ignored(rel, is_dir),
                None => !rules.is_ignored(rel, false) && !rules.is_ignored(rel, true),
            }
        });
    }
    order.sort_by_key(|p| p.matches('/').count());
    order
}

/// Lets [`classification_order`] ask either walk's item whether it is a
/// directory without knowing which walk it came from.
pub(super) trait HasKind {
    fn is_dir(&self) -> bool;
}

impl HasKind for LocalItem {
    fn is_dir(&self) -> bool {
        self.is_dir
    }
}

impl HasKind for RemoteItem {
    fn is_dir(&self) -> bool {
        self.is_dir
    }
}

/// The baseline minus its ignored paths, for [`guard_local_wipe`].
///
/// The guard asks "did every synced path vanish locally?", and an ignored path
/// is absent from the local walk by rule rather than by loss. Left in, a rule
/// covering the whole tree would trip the guard on every pass and wedge that
/// folder's sync for good.
///
/// A baseline row does not record whether it was a file or a folder, so a path
/// counts as ignored if it matches as either. Erring towards "ignored" only ever
/// shrinks the set the guard checks, which weakens a safety net rather than
/// causing a deletion — the wrong direction to be wrong in is the other one.
pub(super) fn filter_baseline<'a, B>(
    baseline: &'a HashMap<String, B>,
    rules: &IgnoreRules,
) -> HashMap<String, &'a B> {
    baseline
        .iter()
        .filter(|(rel, _)| !rules.is_ignored(rel, false) && !rules.is_ignored(rel, true))
        .map(|(rel, entry)| (rel.clone(), entry))
        .collect()
}

/// The paths a local scan left out because it could not read them: a folder
/// it has no permission for, an entry that vanished between the listing and
/// its `stat`, a name that is not UTF-8.
///
/// A left-out path is not a deleted one. A pass drops everything an entry
/// covers from classification, exactly as it drops ignored paths, so a
/// baseline row under an unreadable folder is neither read as a local deletion
/// nor answered with a download into a folder the pass cannot write. Failing
/// the whole pass instead is what B55 did, and it left one root-owned folder
/// stopping every other file in the tree from syncing (B196).
#[derive(Default)]
pub(super) struct Unscanned {
    covered: HashSet<String>,
    /// The subset worth telling the user about, with why: an entry that only
    /// vanished mid-scan is not, since the next pass sees it settled.
    unreadable: Vec<String>,
}

impl Unscanned {
    /// Leave out `rel`, which vanished while the scan looked at it.
    pub(super) fn vanished(&mut self, rel: String) {
        self.covered.insert(rel);
    }

    /// Leave out `rel`, which cannot be read for `reason`.
    pub(super) fn unreadable(&mut self, rel: String, reason: &str) {
        self.unreadable.push(format!("{rel} ({reason})"));
        self.covered.insert(rel);
    }

    /// Whether `rel` is a left-out path or lies under one.
    pub(super) fn covers(&self, rel: &str) -> bool {
        if self.covered.is_empty() {
            return false;
        }
        let mut at = rel;
        loop {
            if self.covered.contains(at) {
                return true;
            }
            match at.rfind('/') {
                Some(i) => at = &at[..i],
                None => return false,
            }
        }
    }

    /// The unreadable paths with their reasons, sorted, for the activity feed.
    pub(super) fn report(&self) -> Vec<String> {
        let mut lines = self.unreadable.clone();
        lines.sort_unstable();
        lines
    }
}

/// The baseline paths a pass can see: [`filter_baseline`], minus everything
/// under a path the scan could not read. This is the set [`guard_local_wipe`]
/// checks and the set a push pass reads deletions from, since a path outside
/// it is absent from the local walk by rule or by failure, never by loss.
pub(super) fn tracked_baseline<'a, B>(
    baseline: &'a HashMap<String, B>,
    rules: &IgnoreRules,
    unscanned: &Unscanned,
) -> HashMap<String, &'a B> {
    let mut tracked = filter_baseline(baseline, rules);
    tracked.retain(|rel, _| !unscanned.covers(rel));
    tracked
}

/// The baseline paths a push pass has to trash on Drive: tracked, and gone
/// from the local walk. Shallowest first, so a trashed folder takes its
/// children with it.
///
/// Built from [`tracked_baseline`] rather than from the raw baseline. Reading
/// it raw trashed every synced path that later became ignored, because the walk
/// skips those, and the switch to on-demand that the push pass runs for then
/// deleted the local copy too (B197).
pub(super) fn local_deletions<B, L>(
    tracked: &HashMap<String, &B>,
    local: &HashMap<String, L>,
) -> Vec<String> {
    let mut missing: Vec<String> = tracked
        .keys()
        .filter(|rel| !local.contains_key(*rel))
        .cloned()
        .collect();
    missing.sort_by_key(|p| p.matches('/').count());
    missing
}

/// Refuse to run a pass whose local side has vanished in its entirety.
///
/// When every baseline path is absent locally, the likely cause is an unavailable
/// mount or unreadable folder rather than a deliberate whole-tree deletion.
/// One surviving path is enough to clear the guard.
pub(super) fn guard_local_wipe<B, L>(
    baseline: &HashMap<String, B>,
    local: &HashMap<String, L>,
) -> Result<(), String> {
    if !baseline.is_empty() && baseline.keys().all(|rel| !local.contains_key(rel)) {
        return Err(format!(
            "every one of the {} synced paths is missing locally; refusing to trash \
             them on Drive. Check that the folder is mounted and readable.",
            baseline.len()
        ));
    }
    Ok(())
}

/// The size the baseline recorded for `rel`, if its remote signature's mtime
/// still matches `mtime`.
pub(super) fn unchanged_remote_size(
    baseline: &HashMap<String, StoredSyncEntry>,
    rel: &str,
    mtime: i64,
) -> Option<i64> {
    match baseline.get(rel).and_then(remote_sig) {
        Some((recorded, size)) if recorded == mtime => Some(size),
        _ => None,
    }
}

/// Join a child `name` onto a walk's `prefix`, giving a rel path.
pub(super) fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// The stored remote signature of a baseline row, if it has one.
pub(super) fn remote_sig(e: &StoredSyncEntry) -> Option<(i64, i64)> {
    match (&e.remote_rev, &e.remote_hash) {
        (Some(m), Some(s)) => Some((m.parse().ok()?, s.parse().ok()?)),
        _ => None,
    }
}

/// The parent of a `/`-joined relative path (`""` for a top-level entry).
pub(super) fn parent_rel(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

/// The final component of a `/`-joined relative path.
pub(crate) fn base_name(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    }
}

/// Turn a `/`-joined relative path into an OS path (`/` is already the separator
/// on Linux, this keeps the intent explicit).
pub(super) fn rel_to_path(rel: &str) -> PathBuf {
    rel.split('/').collect()
}

/// The name for a conflict copy of `path`, e.g. `notes (sync-conflict 1700000000).txt`.
#[cfg(test)]
pub(super) fn conflict_path(path: &Path, stamp: i64) -> PathBuf {
    conflict_path_with_suffix(path, stamp, 0)
}

/// As [`conflict_path`], with a deterministic suffix used when that name already
/// exists. This keeps conflict preservation from ever replacing an older copy.
pub(super) fn conflict_path_with_suffix(path: &Path, stamp: i64, suffix: u32) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    let suffix = if suffix == 0 {
        String::new()
    } else {
        format!("-{suffix}")
    };
    let name = match ext {
        Some(ext) => format!("{stem} (sync-conflict {stamp}{suffix}).{ext}"),
        None => format!("{stem} (sync-conflict {stamp}{suffix})"),
    };
    match path.parent() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

/// A pure decision for one file-shaped path. Transfer identities and filesystem
/// effects are attached by the executor after this classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FilePlan {
    Unchanged,
    Deferred,
    UploadNew,
    UploadRevision,
    Download,
    Conflict,
    DeleteLocal,
    DeleteRemote,
    ForgetBaseline,
    /// First sync of a file that already exists on both sides with identical
    /// content: record the baseline without transferring anything. Without this,
    /// a folder synced for the first time cuts a `(sync-conflict)` copy of every
    /// pre-existing file, because a missing baseline makes both sides read as
    /// "changed" (see the mass first-sync conflict storm in docs/BUGS.md).
    AdoptBaseline,
}

pub(super) fn plan_file(
    local: Option<&LocalItem>,
    remote: Option<&RemoteItem>,
    baseline: Option<&StoredSyncEntry>,
) -> FilePlan {
    if local.is_some_and(|item| item.open_for_write) {
        return FilePlan::Deferred;
    }

    let local_changed = local.is_some_and(|item| {
        // Nanoseconds where both sides have them (`docs/BUGS.md` B25): whole seconds
        // could not see an edit made in the same second as the last sync that
        // left the file the same length, so it was never uploaded.
        baseline.is_none_or(|base| !LocalSig::from(item).same_content(&LocalSig::from(base)))
    });
    let remote_changed = remote.is_some_and(|item| {
        baseline.is_none_or(|base| remote_sig(base) != Some((item.mtime, item.size)))
    });

    match (local, remote) {
        (Some(l), Some(r)) => match (local_changed, remote_changed) {
            (false, false) => FilePlan::Unchanged,
            (true, false) => FilePlan::UploadRevision,
            (false, true) => FilePlan::Download,
            // With a baseline, both-changed is a genuine concurrent edit and must
            // be preserved as a conflict. Without one, "both changed" only means
            // this is the first sync and there was nothing to compare to: adopt
            // the baseline when the sizes already match rather than conflict-copy
            // an already-identical file. Divergent sizes stay a real conflict.
            (true, true) if baseline.is_none() && l.size == r.size => FilePlan::AdoptBaseline,
            (true, true) => FilePlan::Conflict,
        },
        (Some(_), None) if baseline.is_none() || local_changed => FilePlan::UploadNew,
        (Some(_), None) => FilePlan::DeleteLocal,
        (None, Some(_)) if baseline.is_none() || remote_changed => FilePlan::Download,
        (None, Some(_)) => FilePlan::DeleteRemote,
        (None, None) => FilePlan::ForgetBaseline,
    }
}

#[cfg(test)]
mod file_plan_tests {
    use super::*;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    fn local(mtime: i64, size: i64) -> LocalItem {
        LocalItem {
            mtime_ns: None,
            is_dir: false,
            mtime,
            size,
            open_for_write: false,
        }
    }

    fn remote(mtime: i64, size: i64) -> RemoteItem {
        RemoteItem {
            uid: NodeUid::new(VolumeId::from("v"), LinkId::from("l")),
            is_dir: false,
            mtime,
            size,
        }
    }

    fn baseline(
        local_mtime: i64,
        local_size: i64,
        remote_mtime: i64,
        remote_size: i64,
    ) -> StoredSyncEntry {
        StoredSyncEntry {
            rel_path: "file".into(),
            remote_uid: Some("v~l".into()),
            local_mtime,
            local_mtime_ns: None,
            local_size,
            remote_rev: Some(remote_mtime.to_string()),
            remote_hash: Some(remote_size.to_string()),
        }
    }

    /// Whole seconds cannot see an edit made inside the same second that leaves the
    /// file the same length, so a mirror silently stopped uploading it. With a
    /// nanosecond time on both sides it is an ordinary local change (`docs/BUGS.md` B25).
    #[test]
    fn a_same_second_same_size_edit_is_still_a_local_change() {
        let mut base = baseline(10, 20, 30, 40);
        base.local_mtime_ns = Some(10_000_000_000);
        let edited = LocalItem {
            mtime_ns: Some(10_400_000_000),
            ..local(10, 20)
        };
        let remote = remote(30, 40);
        assert_eq!(
            plan_file(Some(&edited), Some(&remote), Some(&base)),
            FilePlan::UploadRevision
        );

        // The same file untouched still reads as unchanged.
        let same = LocalItem {
            mtime_ns: Some(10_000_000_000),
            ..local(10, 20)
        };
        assert_eq!(
            plan_file(Some(&same), Some(&remote), Some(&base)),
            FilePlan::Unchanged
        );

        // And a row from before the column exists compares on seconds, exactly
        // as it did when it was written — no mass re-upload on upgrade.
        let old = baseline(10, 20, 30, 40);
        assert_eq!(
            plan_file(Some(&edited), Some(&remote), Some(&old)),
            FilePlan::Unchanged
        );
    }

    #[test]
    fn file_plan_covers_every_presence_and_change_shape() {
        let same_local = local(10, 20);
        let changed_local = local(11, 20);
        let same_remote = remote(30, 40);
        let changed_remote = remote(31, 40);
        let base = baseline(10, 20, 30, 40);
        let cases = [
            (None, None, None, FilePlan::ForgetBaseline),
            (None, None, Some(&base), FilePlan::ForgetBaseline),
            (Some(&same_local), None, None, FilePlan::UploadNew),
            (Some(&same_local), None, Some(&base), FilePlan::DeleteLocal),
            (Some(&changed_local), None, Some(&base), FilePlan::UploadNew),
            (None, Some(&same_remote), None, FilePlan::Download),
            (
                None,
                Some(&same_remote),
                Some(&base),
                FilePlan::DeleteRemote,
            ),
            (None, Some(&changed_remote), Some(&base), FilePlan::Download),
            (
                Some(&same_local),
                Some(&same_remote),
                None,
                FilePlan::Conflict,
            ),
            (
                Some(&same_local),
                Some(&same_remote),
                Some(&base),
                FilePlan::Unchanged,
            ),
            (
                Some(&changed_local),
                Some(&same_remote),
                Some(&base),
                FilePlan::UploadRevision,
            ),
            (
                Some(&same_local),
                Some(&changed_remote),
                Some(&base),
                FilePlan::Download,
            ),
            (
                Some(&changed_local),
                Some(&changed_remote),
                Some(&base),
                FilePlan::Conflict,
            ),
        ];

        for (local, remote, baseline, expected) in cases {
            assert_eq!(plan_file(local, remote, baseline), expected);
        }
    }

    #[test]
    fn an_open_writer_overrides_every_other_file_decision() {
        let mut local = local(11, 20);
        local.open_for_write = true;
        let remote = remote(31, 40);
        let base = baseline(10, 20, 30, 40);

        for (remote, baseline) in [
            (None, None),
            (None, Some(&base)),
            (Some(&remote), None),
            (Some(&remote), Some(&base)),
        ] {
            assert_eq!(
                plan_file(Some(&local), remote, baseline),
                FilePlan::Deferred
            );
        }
    }

    #[test]
    fn first_sync_of_an_identical_file_adopts_the_baseline_not_a_conflict() {
        // No baseline yet (first sync). Same size on both sides is a pre-existing
        // identical file, not a concurrent edit: adopt it rather than cut a
        // conflict copy of every file in the folder.
        let local = local(10, 40);
        let remote = remote(30, 40);
        assert_eq!(
            plan_file(Some(&local), Some(&remote), None),
            FilePlan::AdoptBaseline
        );
    }

    #[test]
    fn first_sync_of_differing_sizes_is_still_a_conflict() {
        // No baseline and the sizes disagree: a genuine both-sides divergence,
        // preserved non-destructively as a conflict.
        let local = local(10, 20);
        let remote = remote(30, 40);
        assert_eq!(
            plan_file(Some(&local), Some(&remote), None),
            FilePlan::Conflict
        );
    }

    #[test]
    fn a_missing_remote_signature_is_treated_as_changed() {
        let local = local(10, 20);
        let remote = remote(30, 40);
        let mut base = baseline(10, 20, 30, 40);
        base.remote_hash = None;

        assert_eq!(
            plan_file(Some(&local), Some(&remote), Some(&base)),
            FilePlan::Download
        );
    }
}
