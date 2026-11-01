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

/// How many tracked paths a folder needs before a mass disappearance, short of
/// a total one, trips a wipe guard. Below this, a few survivors say little.
const MASS_LOSS_MIN_PATHS: usize = 20;

/// The share of a large folder's tracked paths that may vanish in one pass, in
/// tenths, before a wipe guard stops it: 9 of 10 and up is refused.
const MASS_LOSS_TENTHS: usize = 9;

/// Whether `missing` of `total` tracked paths gone at once looks like a lost
/// tree rather than deletions. All of them always does. Most of a large
/// folder does too: one surviving file used to clear the guard, so a
/// half-mounted disk, or a script that emptied the folder and wrote one file
/// back, trashed everything else on Drive (B270).
fn looks_like_a_wipe(missing: usize, total: usize) -> bool {
    total > 0
        && (missing == total
            || (total >= MASS_LOSS_MIN_PATHS && missing * 10 >= total * MASS_LOSS_TENTHS))
}

/// Refuse to run a pass whose local side has vanished in its entirety, or
/// nearly so ([`looks_like_a_wipe`]).
///
/// When the synced paths are all or nearly all absent locally, the likely
/// cause is an unavailable mount or unreadable folder rather than a deliberate
/// deletion of the whole tree.
pub(super) fn guard_local_wipe<B, L>(
    baseline: &HashMap<String, B>,
    local: &HashMap<String, L>,
) -> Result<(), String> {
    let missing = baseline
        .keys()
        .filter(|rel| !local.contains_key(*rel))
        .count();
    if looks_like_a_wipe(missing, baseline.len()) {
        return Err(format!(
            "{missing} of the {} synced paths are missing locally; refusing to trash \
             them on Drive. Check that the folder is mounted and readable; to download \
             them again, sync the folder from scratch.",
            baseline.len()
        ));
    }
    Ok(())
}

/// Refuse to run a pass whose remote side has vanished in its entirety, or
/// nearly so: the remote twin of [`guard_local_wipe`].
///
/// An empty or truncated remote walk reads to the planner as "Drive deleted
/// everything", which a pass answers by deleting the local copies. When all or
/// nearly all synced paths are gone from Drive at once, a failed listing is far
/// likelier than a deliberate wipe of the folder, so the pass stops instead.
pub(super) fn guard_remote_wipe<B, R>(
    baseline: &HashMap<String, B>,
    remote: &HashMap<String, R>,
) -> Result<(), String> {
    let missing = baseline
        .keys()
        .filter(|rel| !remote.contains_key(*rel))
        .count();
    if looks_like_a_wipe(missing, baseline.len()) {
        return Err(format!(
            "{missing} of the {} synced paths are missing on Proton Drive; refusing to \
             delete them here. If they were deleted on purpose, remove the folder from sync; \
             to upload them again, sync the folder from scratch.",
            baseline.len()
        ));
    }
    Ok(())
}

/// What a remote walk saw but could not read: a node the SDK left out because
/// it would not decrypt, or a file whose size could not be read.
///
/// A path missing from the walk is read as deleted on Drive, so a pass must
/// not classify one it only failed to read. Since a node that failed to decrypt
/// has no name, all a walk can say is that a folder's listing came back short;
/// every path under that folder that the walk did not return is then left out,
/// as [`Unscanned`] leaves out what the local scan could not read.
#[derive(Default)]
pub(super) struct RemoteGaps {
    /// Folders whose listing came back with fewer nodes than it named.
    short: HashSet<String>,
    /// Paths seen, but whose entry could not be read.
    unread: HashSet<String>,
    /// Folders whose listing failed outright, after the SDK's retries. Nothing
    /// under one is known, so all of it is left out of the pass (B271).
    unlisted: HashSet<String>,
}

impl RemoteGaps {
    /// The listing of `folder` named nodes that did not come back.
    pub(super) fn short_listing(&mut self, folder: &str) {
        self.short.insert(folder.to_string());
    }

    /// `rel` is on Drive, but the walk could not read what it needs of it.
    pub(super) fn unread(&mut self, rel: String) {
        self.unread.insert(rel);
    }

    /// The listing of `folder` failed, so nothing under it was walked.
    pub(super) fn unlisted(&mut self, folder: &str) {
        self.unlisted.insert(folder.to_string());
    }

    pub(super) fn is_empty(&self) -> bool {
        self.short.is_empty() && self.unread.is_empty() && self.unlisted.is_empty()
    }

    /// Whether `rel` may be missing from `remote` only because the walk could
    /// not read it. A path the walk returned is never hidden.
    pub(super) fn hides<R>(
        &self,
        rel: &str,
        remote: &HashMap<String, R>,
        remote_dirs: &HashMap<String, NodeUid>,
    ) -> bool {
        if self.is_empty() || remote.contains_key(rel) {
            return false;
        }
        if self.unread.contains(rel) {
            return true;
        }
        // The deepest folder the walk did list decides: under a short listing,
        // anything not returned may be one of the nodes it could not read.
        let mut at = parent_rel(rel);
        loop {
            if remote_dirs.contains_key(at) {
                return self.short.contains(at) || self.unlisted.contains(at);
            }
            if at.is_empty() {
                return false;
            }
            at = parent_rel(at);
        }
    }

    /// The folders to name to the user, sorted.
    pub(super) fn report(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .short
            .iter()
            .map(|rel| {
                let shown = if rel.is_empty() { "the folder" } else { rel };
                format!("{shown} (items on Proton Drive that cannot be read)")
            })
            .chain(
                self.unread
                    .iter()
                    .map(|rel| format!("{rel} (cannot be read on Proton Drive)")),
            )
            .chain(self.unlisted.iter().map(|rel| {
                format!("{rel} (could not be listed on Proton Drive; tried again next pass)")
            }))
            .collect();
        lines.sort_unstable();
        lines
    }
}

/// Why the remote folder at `rel` must not be trashed although it was deleted
/// here: something under it is new or changed on Drive since the last sync,
/// so it is another device's work that this computer has never had.
///
/// Trashing the folder would take that work with it, and the next pass would
/// then delete the copy this pass just downloaded. `None` when everything under
/// it is exactly what the baseline recorded.
pub(super) fn remote_subtree_changed(
    rel: &str,
    remote: &HashMap<String, RemoteItem>,
    baseline: &HashMap<String, StoredSyncEntry>,
) -> Option<String> {
    let prefix = format!("{rel}/");
    let mut changed: Vec<&String> = remote
        .iter()
        .filter(|(path, _)| path.starts_with(&prefix))
        .filter(|(path, item)| match baseline.get(*path) {
            None => true,
            Some(_) if item.is_dir => false,
            Some(base) => {
                remote_sig(base) != Some((item.mtime, item.size))
                    || revision_moved(Some(base), item.revision.as_deref())
            }
        })
        .map(|(path, _)| path)
        .collect();
    changed.sort_unstable();
    changed
        .first()
        .map(|path| format!("{path} is new or changed on Proton Drive"))
}

/// The size the baseline recorded for `rel`, if its remote signature's mtime
/// still matches `mtime` and its revision is still `revision`.
///
/// A row without a revision id, while Drive reports one, cannot vouch for the
/// revision, so the size is read for real. Such a row comes from before
/// schema 44, or from an upload Drive did not list yet when it was read back.
/// Another computer's revision in the same second as that upload kept the
/// modification time, and only the size could show it (B254). The pass then
/// records the id on the row, so this costs one read per row.
pub(super) fn unchanged_remote_size(
    baseline: &HashMap<String, StoredSyncEntry>,
    rel: &str,
    mtime: i64,
    revision: Option<&str>,
) -> Option<i64> {
    let base = baseline.get(rel)?;
    if revision_moved(Some(base), revision)
        || (base.remote_revision_id.is_none() && revision.is_some())
    {
        return None;
    }
    match remote_sig(base) {
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

/// Whether Drive's active revision `now` is not the one the baseline row was
/// synced at. Only a revision id on both sides can say so; a row from before
/// schema 44, or a node read without one, leaves it to the modification time
/// and size.
pub(super) fn revision_moved(base: Option<&StoredSyncEntry>, now: Option<&str>) -> bool {
    match (base.and_then(|b| b.remote_revision_id.as_deref()), now) {
        (Some(recorded), Some(now)) => recorded != now,
        _ => false,
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

/// The name for a conflict copy of `path`, e.g.
/// `notes (sync-conflict laptop 1700000000).txt`.
#[cfg(test)]
pub(super) fn conflict_path(path: &Path, stamp: i64) -> PathBuf {
    conflict_path_with_suffix(path, stamp, 0)
}

/// As [`conflict_path`], with a deterministic suffix used when that name already
/// exists. This keeps conflict preservation from ever replacing an older copy.
pub(super) fn conflict_path_with_suffix(path: &Path, stamp: i64, suffix: u32) -> PathBuf {
    conflict_path_as(path, crate::conflict_label(), stamp, suffix)
}

/// [`conflict_path_with_suffix`] with the machine's label given.
pub(super) fn conflict_path_as(path: &Path, label: &str, stamp: i64, suffix: u32) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    let tag = crate::conflict_tag(label, stamp, suffix);
    let name = match ext {
        Some(ext) => format!("{stem} (sync-conflict {tag}).{ext}"),
        None => format!("{stem} (sync-conflict {tag})"),
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
    /// First sync of a file that exists on both sides at the same size, with a
    /// digest on Drive: record the baseline without transferring anything if
    /// the local file hashes the same, else keep a conflict copy. Without this,
    /// a folder synced for the first time cuts a `(sync-conflict)` copy of every
    /// pre-existing file, because a missing baseline makes both sides read as
    /// "changed" (see the mass first-sync conflict storm in docs/BUGS.md). Size
    /// alone is not enough: a second machine joining the folder would read a
    /// different file of the same size as in step (B248).
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
    // Another client's new revision shows in its id even when the link's
    // modification time and the size come out the same.
    let remote_changed = remote.is_some_and(|item| {
        baseline.is_none_or(|base| {
            remote_sig(base) != Some((item.mtime, item.size))
                || revision_moved(Some(base), item.revision.as_deref())
        })
    });

    match (local, remote) {
        (Some(l), Some(r)) => match (local_changed, remote_changed) {
            (false, false) => FilePlan::Unchanged,
            (true, false) => FilePlan::UploadRevision,
            (false, true) => FilePlan::Download,
            // With a baseline, both-changed is a genuine concurrent edit and must
            // be preserved as a conflict. Without one, "both changed" only means
            // this is the first sync and there was nothing to compare to: when the
            // sizes match and Drive has a digest to compare against, the apply
            // step hashes the local file before adopting it. Divergent sizes, or
            // no digest, stay a real conflict: a missing digest proves nothing.
            (true, true) if baseline.is_none() && l.size == r.size && r.sha1.is_some() => {
                FilePlan::AdoptBaseline
            }
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
            revision: None,
            sha1: None,
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
            remote_revision_id: None,
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
        // conflict copy of every file in the folder. The apply step still
        // hashes the local file before it records anything.
        let local = local(10, 40);
        let remote = RemoteItem {
            sha1: Some("da39a3ee".into()),
            ..remote(30, 40)
        };
        assert_eq!(
            plan_file(Some(&local), Some(&remote), None),
            FilePlan::AdoptBaseline
        );
    }

    /// A file on Drive without a digest cannot be compared without a download,
    /// and the same size is not the same content (B248).
    #[test]
    fn first_sync_without_a_remote_digest_is_a_conflict() {
        let local = local(10, 40);
        let remote = remote(30, 40);
        assert_eq!(
            plan_file(Some(&local), Some(&remote), None),
            FilePlan::Conflict
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

    /// Another computer's new revision can leave the modification time and
    /// the size as they were. The revision id still tells it apart (B249).
    #[test]
    fn a_new_remote_revision_is_a_change_even_at_the_same_mtime_and_size() {
        let mut base = baseline(10, 20, 30, 40);
        base.remote_revision_id = Some("r1".into());
        let unedited = local(10, 20);
        let same = RemoteItem {
            revision: Some("r1".into()),
            ..remote(30, 40)
        };
        let newer = RemoteItem {
            revision: Some("r2".into()),
            ..remote(30, 40)
        };
        assert_eq!(
            plan_file(Some(&unedited), Some(&same), Some(&base)),
            FilePlan::Unchanged
        );
        assert_eq!(
            plan_file(Some(&unedited), Some(&newer), Some(&base)),
            FilePlan::Download
        );
        let edited = local(11, 20);
        assert_eq!(
            plan_file(Some(&edited), Some(&newer), Some(&base)),
            FilePlan::Conflict
        );

        // A row from before schema 44 has no id to compare, and a node read
        // without one says nothing: both fall back to the signature.
        let old = baseline(10, 20, 30, 40);
        assert_eq!(
            plan_file(Some(&unedited), Some(&newer), Some(&old)),
            FilePlan::Unchanged
        );
        assert_eq!(
            plan_file(Some(&unedited), Some(&remote(30, 40)), Some(&base)),
            FilePlan::Unchanged
        );
    }

    /// The walk reuses the recorded size only while the revision is the one
    /// the baseline holds.
    #[test]
    fn a_moved_revision_makes_the_walk_read_the_size_again() {
        let mut base = baseline(10, 20, 30, 40);
        base.remote_revision_id = Some("r1".into());
        let rows = HashMap::from([("file".to_string(), base)]);
        assert_eq!(
            unchanged_remote_size(&rows, "file", 30, Some("r1")),
            Some(40)
        );
        assert_eq!(unchanged_remote_size(&rows, "file", 30, None), Some(40));
        assert_eq!(unchanged_remote_size(&rows, "file", 30, Some("r2")), None);
        assert_eq!(unchanged_remote_size(&rows, "file", 31, Some("r1")), None);
    }

    /// A row without a revision id reuses its size only while Drive gives no
    /// id either (B254).
    #[test]
    fn a_row_without_a_revision_makes_the_walk_read_the_size_again() {
        let rows = HashMap::from([("file".to_string(), baseline(10, 20, 30, 40))]);
        assert_eq!(unchanged_remote_size(&rows, "file", 30, Some("r1")), None);
        assert_eq!(unchanged_remote_size(&rows, "file", 30, None), Some(40));
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

    fn uid(id: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("v"), LinkId::from(id))
    }

    fn folder() -> RemoteItem {
        RemoteItem {
            is_dir: true,
            mtime: 0,
            size: 0,
            ..remote(0, 0)
        }
    }

    /// An empty or failed remote listing must not read as "Drive deleted
    /// everything" and delete the local copies.
    #[test]
    fn the_remote_wipe_guard_blocks_a_total_disappearance() {
        let base = HashMap::from([("a".to_string(), ()), ("b".to_string(), ())]);
        let none: HashMap<String, RemoteItem> = HashMap::new();
        assert!(guard_remote_wipe(&base, &none).is_err());
        let one = HashMap::from([("b".to_string(), remote(1, 1))]);
        assert!(guard_remote_wipe(&base, &one).is_ok());
        let empty: HashMap<String, ()> = HashMap::new();
        assert!(guard_remote_wipe(&empty, &none).is_ok());
    }

    /// One path left of a large folder no longer clears either guard: most
    /// of it gone at once is a lost tree, not a cleanup (B270).
    #[test]
    fn one_survivor_of_a_large_folder_does_not_clear_the_wipe_guards() {
        let base: HashMap<String, ()> = (0..40).map(|n| (format!("f{n}.txt"), ())).collect();
        let survivors = |n: usize| -> HashMap<String, ()> {
            (0..n).map(|n| (format!("f{n}.txt"), ())).collect()
        };
        assert!(guard_local_wipe(&base, &survivors(1)).is_err());
        assert!(guard_remote_wipe(&base, &survivors(1)).is_err());
        // 36 of 40 gone is nine tenths: still refused.
        assert!(guard_local_wipe(&base, &survivors(4)).is_err());
        // 35 of 40 gone is under nine tenths: a pass may delete that much.
        assert!(guard_local_wipe(&base, &survivors(5)).is_ok());
        assert!(guard_remote_wipe(&base, &survivors(5)).is_ok());
        let reason = guard_local_wipe(&base, &survivors(1)).unwrap_err();
        assert!(reason.starts_with("39 of the 40 synced paths"), "{reason}");
    }

    /// A small folder still loses most of its files to an ordinary cleanup, so
    /// only a total loss stops it.
    #[test]
    fn a_small_folder_trips_the_wipe_guards_only_when_everything_is_gone() {
        let base: HashMap<String, ()> = (0..19).map(|n| (format!("f{n}.txt"), ())).collect();
        let one: HashMap<String, ()> = [("f0.txt".to_string(), ())].into_iter().collect();
        assert!(guard_local_wipe(&base, &one).is_ok());
        assert!(guard_remote_wipe(&base, &one).is_ok());
        let none: HashMap<String, ()> = HashMap::new();
        assert!(guard_local_wipe(&base, &none).is_err());
    }

    /// A node that failed to decrypt has no name, so everything a short
    /// listing did not return is left out, and nothing else.
    #[test]
    fn a_short_remote_listing_hides_what_it_did_not_return() {
        let remote = HashMap::from([
            ("docs".to_string(), folder()),
            ("docs/a.txt".to_string(), remote(1, 1)),
            ("pics".to_string(), folder()),
        ]);
        let dirs = HashMap::from([
            (String::new(), uid("root")),
            ("docs".to_string(), uid("docs")),
            ("pics".to_string(), uid("pics")),
        ]);
        let mut gaps = RemoteGaps::default();
        assert!(!gaps.hides("docs/b.txt", &remote, &dirs));
        gaps.short_listing("docs");
        // Returned paths stand; unreturned ones under the short folder hide,
        // and so does anything under a subfolder it failed to return.
        assert!(!gaps.hides("docs/a.txt", &remote, &dirs));
        assert!(gaps.hides("docs/b.txt", &remote, &dirs));
        assert!(gaps.hides("docs/sub/deep.txt", &remote, &dirs));
        // A complete sibling listing still reads deletions as deletions.
        assert!(!gaps.hides("pics/gone.jpg", &remote, &dirs));
        assert!(!gaps.hides("top.txt", &remote, &dirs));
        gaps.short_listing("");
        assert!(gaps.hides("top.txt", &remote, &dirs));
    }

    /// A folder whose listing failed was walked no further: nothing under it
    /// reads as deleted on Drive, and a sibling that listed still does
    /// (`docs/BUGS.md` B271).
    #[test]
    fn a_folder_that_failed_to_list_hides_everything_under_it() {
        let remote = HashMap::from([
            ("docs".to_string(), folder()),
            ("pics".to_string(), folder()),
        ]);
        let dirs = HashMap::from([
            (String::new(), uid("root")),
            ("docs".to_string(), uid("docs")),
            ("pics".to_string(), uid("pics")),
        ]);
        let mut gaps = RemoteGaps::default();
        gaps.unlisted("docs");
        assert!(gaps.hides("docs/a.txt", &remote, &dirs));
        assert!(gaps.hides("docs/sub/deep.txt", &remote, &dirs));
        assert!(!gaps.hides("docs", &remote, &dirs));
        assert!(!gaps.hides("pics/gone.jpg", &remote, &dirs));
        assert_eq!(
            gaps.report(),
            ["docs (could not be listed on Proton Drive; tried again next pass)"]
        );
    }

    #[test]
    fn a_file_whose_size_could_not_be_read_is_hidden() {
        let remote: HashMap<String, RemoteItem> = HashMap::new();
        let dirs = HashMap::from([(String::new(), uid("root"))]);
        let mut gaps = RemoteGaps::default();
        gaps.unread("a.txt".to_string());
        assert!(gaps.hides("a.txt", &remote, &dirs));
        assert!(!gaps.hides("b.txt", &remote, &dirs));
        assert_eq!(gaps.report(), ["a.txt (cannot be read on Proton Drive)"]);
    }

    /// A folder deleted here holds another device's new work on Drive, so
    /// trashing it would lose that work.
    #[test]
    fn a_remote_folder_with_new_or_changed_files_is_not_trashed() {
        let mut base = HashMap::from([
            ("x".to_string(), baseline(0, 0, 0, 0)),
            ("x/old.txt".to_string(), baseline(1, 1, 5, 6)),
        ]);
        let mut remote_tree = HashMap::from([
            ("x".to_string(), folder()),
            ("x/old.txt".to_string(), remote(5, 6)),
            ("xy/new.txt".to_string(), remote(1, 1)),
        ]);
        // Unchanged under x; a name-prefixed sibling does not count.
        assert_eq!(remote_subtree_changed("x", &remote_tree, &base), None);

        remote_tree.insert("x/old.txt".to_string(), remote(9, 6));
        assert!(remote_subtree_changed("x", &remote_tree, &base).is_some());

        remote_tree.insert("x/old.txt".to_string(), remote(5, 6));
        remote_tree.insert("x/sub".to_string(), folder());
        assert!(remote_subtree_changed("x", &remote_tree, &base).is_some());
        base.insert("x/sub".to_string(), baseline(0, 0, 0, 0));
        assert_eq!(remote_subtree_changed("x", &remote_tree, &base), None);
    }
}
