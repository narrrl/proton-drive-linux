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

/// Refuse to run a pass whose local side has vanished in its entirety.
///
/// When every baseline path is absent locally, the likely cause is an unavailable
/// mount or unreadable folder rather than a deliberate whole-tree deletion.
/// One surviving path is enough to clear the guard.
pub(super) fn guard_local_wipe<B, L>(
    baseline: &HashMap<String, B>,
    local: &HashMap<String, L>,
) -> Result<(), String> {
    if baseline.len() >= 2 && baseline.keys().all(|rel| !local.contains_key(rel)) {
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
pub(super) fn conflict_path(path: &Path, stamp: i64) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    let name = match ext {
        Some(ext) => format!("{stem} (sync-conflict {stamp}).{ext}"),
        None => format!("{stem} (sync-conflict {stamp})"),
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
        baseline.is_none_or(|base| base.local_mtime != item.mtime || base.local_size != item.size)
    });
    let remote_changed = remote.is_some_and(|item| {
        baseline.is_none_or(|base| remote_sig(base) != Some((item.mtime, item.size)))
    });

    match (local, remote) {
        (Some(_), Some(_)) => match (local_changed, remote_changed) {
            (false, false) => FilePlan::Unchanged,
            (true, false) => FilePlan::UploadRevision,
            (false, true) => FilePlan::Download,
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
            local_size,
            remote_rev: Some(remote_mtime.to_string()),
            remote_hash: Some(remote_size.to_string()),
        }
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
