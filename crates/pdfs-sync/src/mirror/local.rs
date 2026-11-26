//! The mirror engine's reads and writes of the local disk, in one place.
//!
//! A pass touches the local tree in a few ways only: it scans it, hashes and
//! opens files to upload, places downloads, removes what Drive deleted, and
//! sets a racing local edit aside as a conflict copy. Every one of them reads a
//! missing, unreadable or changed path the way the engine has to: as a fact to
//! report, never as a deletion. Keeping them here, apart from the planning and
//! the Drive calls, is the first step towards the `LocalFs` port in
//! `docs/ARCHITECTURE.md`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pdfs_core::syncignore::IgnoreRules;
use pdfs_core::timestamp::now_secs;
use pdfs_store::db::StoredSyncEntry;
use tracing::{debug, warn};

use super::model::{LocalItem, LocalSig, system_mtime, system_mtime_ns};
use super::planner::{
    Unscanned, conflict_path_with_suffix, filter_baseline, join_rel, rel_to_path,
};

/// A local file's identity and content SHA-1, both read from one descriptor,
/// or `None` when it is gone. The identity is taken after the hash, so a file
/// written to while it was hashed no longer matches the scan's.
pub(super) fn local_sha1(path: &Path) -> std::io::Result<Option<(LocalSig, String)>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let digest =
        pdfs_drive::digest::sha1_of(&mut file).map_err(|e| std::io::Error::other(e.message))?;
    Ok(Some((LocalSig::from(&file.metadata()?), digest)))
}

/// Open a local file for upload, or `None` when it is gone.
///
/// A scan plans uploads from a listing, and the file can be deleted before its
/// upload starts — a build or a test tree cleaned up mid-pass. That is not a
/// failure: the next pass sees the deletion. Counting it as one put the folder
/// in `error` for a file nobody wanted any more (B108).
pub(super) fn open_to_upload(path: &Path, rel: &str) -> Result<Option<std::fs::File>, String> {
    match std::fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(rel, "sync: file deleted before its upload; skipping");
            Ok(None)
        }
        Err(e) => Err(format!("open {rel}: {e}")),
    }
}

/// Say so when a file changed under an upload that was streaming it.
///
/// Purely diagnostic — the correction is already in place, because the baseline
/// records the identity the upload *read* rather than the one on disk now, so
/// the next pass classifies the path as locally changed and replaces the torn
/// revision. Without the log there is nothing in the journal to explain the
/// extra revision (`docs/BUGS.md` B40).
pub(super) fn warn_if_torn(rel: &str, path: &Path, streamed: LocalSig) {
    let now = local_sig(path);
    if !now.is_some_and(|now| now.same_content(&streamed)) {
        warn!(
            rel,
            ?streamed,
            ?now,
            "sync: file changed while uploading; the remote revision may be torn \
             and will be replaced on the next pass"
        );
    }
}

/// The identity a reconcile pass uses to recognise a local file it has already
/// looked at: `None` when nothing is at the path.
pub(super) fn local_sig(path: &Path) -> Option<LocalSig> {
    std::fs::metadata(path).ok().map(|m| LocalSig {
        mtime: system_mtime(&m),
        mtime_ns: system_mtime_ns(&m),
        size: m.len() as i64,
    })
}

/// Guard the destination of a download: if what is at `path` is no longer what
/// planning classified, move it aside as a conflict copy before the caller
/// renames over it.
///
/// A reconcile pass decides what to do from a walk that finished before the
/// transfer started, and a download of a large file can run for minutes. Without
/// this, a local edit completed inside that window was replaced by the remote
/// version with no copy of it kept anywhere — the one shape of loss a sync
/// engine must not have (`docs/BUGS.md` B39). Keeping the copy makes it re-upload as a
/// new file on the next pass, which is the same resolution a both-sides-changed
/// conflict already gets.
///
/// A path that is empty when it was expected to be is not a conflict: the
/// download is about to recreate it, and there is nothing to preserve.
pub(super) fn keep_racing_local_edit(
    path: &Path,
    expected: Option<LocalSig>,
) -> std::io::Result<()> {
    let actual = local_sig(path);
    let unchanged = match (actual, expected) {
        (Some(a), Some(e)) => a.same_content(&e),
        // Nothing there: the download is about to create it, and there is
        // nothing to preserve either way.
        (None, _) => true,
        (Some(_), None) => false,
    };
    if unchanged {
        return Ok(());
    }
    warn!(
        path = %path.display(),
        ?expected,
        ?actual,
        "sync: local file changed while downloading; keeping it as a conflict copy"
    );
    preserve_conflict_copy(path, now_secs()).map(|_| ())
}

/// What became of one folder [`remove_local_dirs`] was asked to remove.
pub(super) enum DirRemoval {
    Removed(String),
    /// Left in place, with why: it holds something Drive does not have.
    Kept(String, String),
    Failed(String, std::io::Error),
}

/// Remove the mirror folders in `dirs`, which Drive deleted, deepest first,
/// except those that hold something Drive never had.
///
/// The uploads of a new file or an edit in such a folder failed earlier in the
/// pass for want of a parent, and an ignored `.git` is never uploaded at all,
/// so removing the folder would lose them for good (B211). `current` is the
/// baseline as the pass has left it: a row the pass already removed with its
/// file must not read as a deletion not carried up yet, and a subfolder removed
/// here takes its rows with it for the same reason. A kept folder loses its own
/// row, so its parent sees it as new and is kept too.
pub(super) fn remove_local_dirs(
    root: &Path,
    mut dirs: Vec<String>,
    current: &mut HashMap<String, StoredSyncEntry>,
) -> Vec<DirRemoval> {
    dirs.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    dirs.into_iter()
        .map(|rel| {
            if let Some(reason) = mirror_subtree_unsynced(root, &rel, current) {
                current.remove(&rel);
                return DirRemoval::Kept(rel, reason);
            }
            let prefix = format!("{rel}/");
            current.retain(|path, _| *path != rel && !path.starts_with(&prefix));
            match removed_locally(std::fs::remove_dir_all(root.join(rel_to_path(&rel)))) {
                Ok(()) => DirRemoval::Removed(rel),
                Err(e) => DirRemoval::Failed(rel, e),
            }
        })
        .collect()
}

/// Whether the file at `path` is no longer what the scan saw as `planned`.
/// A file that is gone has nothing left to lose, so it has not changed.
pub(super) fn changed_since_scan(path: &Path, planned: &LocalItem) -> bool {
    local_sig(path).is_some_and(|now| !now.same_content(&LocalSig::from(planned)))
}

/// Read a local removal as the sync engine has to read it: the *state* it was
/// asking for is "this path is gone", so a path that was already gone is a
/// success, not a failure.
///
/// It matters because the baseline is only dropped on success. Treating
/// `NotFound` as an error left the baseline in place for a file that no longer
/// exists, so the same removal was re-attempted and re-reported as an error on
/// every pass, forever. Every other error is still an error, and keeping the
/// baseline is exactly right there: the surviving file must not be mistaken for
/// a new local creation on the next pass and uploaded back (`docs/BUGS.md` B42).
pub(super) fn removed_locally(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The body of [`Core::walk_local`], without a `Core`: recursively collect the
/// local tree under `root` into `out`, reporting each entry to `scanned`.
///
/// **A path the scan cannot read is left out and recorded in `unscanned`, never
/// silently dropped.** Every pass drops what `unscanned` covers from
/// classification, so a baseline row under it is neither a local deletion nor a
/// download target (`docs/BUGS.md` B55). That covers a subfolder the pass has no
/// permission for, an entry that vanished between the listing and its `stat`,
/// and a name that is not UTF-8. Failing the whole pass on any of them, as this
/// did before, left one root-owned folder stopping everything else in the tree
/// from syncing, on every pass, for good (B196).
///
/// The root is the exception, and so is any other I/O error: an absent or
/// unreadable root fails the pass, since leaving the root out would leave
/// nothing in, and an error this cannot name a cause for is not one to guess
/// about.
///
/// Split out from the method so exactly that can be tested against a real tree
/// with a real unreadable directory in it, which needs no `Core` at all.
pub(super) fn walk_local_tree(
    root: &Path,
    rules: &IgnoreRules,
    writing: &HashSet<PathBuf>,
    out: &mut HashMap<String, LocalItem>,
    unscanned: &mut Unscanned,
    scanned: &mut dyn FnMut(&str),
) -> Result<(), String> {
    walk_local_dir(root, root, rules, writing, out, unscanned, scanned).map_err(|e| e.to_string())
}

/// Why [`walk_local_dir`] could not finish a folder.
pub(super) enum ScanError {
    /// An I/O error on `what`, which the caller may leave out of the pass.
    Io { what: String, error: std::io::Error },
    /// Anything else, which fails the pass.
    Failed(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Io { what, error } => write!(f, "{what}: {error}"),
            ScanError::Failed(message) => f.write_str(message),
        }
    }
}

/// Leave `rel` out of the pass for `failure`, or hand it back when it is one
/// that fails the pass. Permission denied is reported to the user; an entry
/// that vanished mid-scan is not, since the next pass sees it settled.
pub(super) fn leave_out(
    rel: &str,
    failure: ScanError,
    unscanned: &mut Unscanned,
) -> Result<(), ScanError> {
    let ScanError::Io { what, error } = &failure else {
        return Err(failure);
    };
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => {
            warn!(%what, %error, "sync: cannot read; leaving it out of the pass");
            unscanned.unreadable(rel.to_string(), &error.to_string());
            Ok(())
        }
        std::io::ErrorKind::NotFound => {
            debug!(%what, "sync: gone mid-scan; leaving it out of the pass");
            unscanned.vanished(rel.to_string());
            Ok(())
        }
        _ => Err(failure),
    }
}

pub(super) fn walk_local_dir(
    root: &Path,
    dir: &Path,
    rules: &IgnoreRules,
    writing: &HashSet<PathBuf>,
    out: &mut HashMap<String, LocalItem>,
    unscanned: &mut Unscanned,
    scanned: &mut dyn FnMut(&str),
) -> Result<(), ScanError> {
    let io = |what: String| move |error| ScanError::Io { what, error };
    let entries = std::fs::read_dir(dir).map_err(io(format!("read {}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(io(format!("read entry in {}", dir.display())))?;
        let path = entry.path();
        let stripped = path.strip_prefix(root).map_err(|e| {
            ScanError::Failed(format!(
                "local path {} escaped sync root {}: {e}",
                path.display(),
                root.display()
            ))
        })?;
        let Some(rel) = stripped.to_str() else {
            // Nothing can be keyed by this name: not the baseline, which only
            // ever held paths this scan produced, and not Drive, whose names are
            // UTF-8. Leaving it out cannot read as a deletion of anything.
            warn!(path = %path.display(), "sync: name is not valid UTF-8; leaving it out of the pass");
            unscanned.unreadable(
                stripped.to_string_lossy().into_owned(),
                "name is not valid UTF-8",
            );
            continue;
        };
        // Ignore our own in-flight download temp files.
        if rel.contains(".pdfs-tmp-") {
            continue;
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // Gone since the listing, or in a folder that lists but does not
            // allow `stat`. Either way this pass cannot say what it is.
            Err(error) => {
                let what = format!("stat {}", path.display());
                leave_out(rel, ScanError::Io { what, error }, unscanned)?;
                continue;
            }
        };
        // Ignored paths are dropped here as well as from the classification
        // union — not for correctness (the union filter is what makes this
        // safe) but so an ignored `node_modules/` is never descended into or
        // counted against scan progress.
        if rules.is_ignored(rel, meta.is_dir()) {
            continue;
        }
        if meta.is_dir() {
            out.insert(
                rel.to_string(),
                LocalItem {
                    is_dir: true,
                    mtime: 0,
                    mtime_ns: None,
                    size: 0,
                    open_for_write: false,
                },
            );
            scanned(rel);
            if let Err(failure) =
                walk_local_dir(root, &path, rules, writing, out, unscanned, scanned)
            {
                leave_out(rel, failure, unscanned)?;
                // What the folder did list before failing goes with it: a
                // half-read folder is as unknown as an unread one.
                let prefix = format!("{rel}/");
                out.retain(|k, _| k != rel && !k.starts_with(&prefix));
            }
        } else if meta.is_file() {
            // Check whether any process holds this file open for writing.
            // The canonical path is used because /proc/*/fd links resolve
            // to canonical targets.
            let ofw = std::fs::canonicalize(&path)
                .map(|canon| writing.contains(&canon))
                .unwrap_or(false);
            out.insert(
                rel.to_string(),
                LocalItem {
                    is_dir: false,
                    mtime: system_mtime(&meta),
                    mtime_ns: system_mtime_ns(&meta),
                    size: meta.len() as i64,
                    open_for_write: ofw,
                },
            );
            scanned(rel);
        }
    }
    Ok(())
}

/// Why the mirror copy at `rel` holds something Proton Drive does not, or
/// `None` when everything under it is on Drive exactly as it is on disk.
///
/// A move out of a mirror folder removes the local copy once Drive has moved
/// the node, so anything only this disk has would be lost with it: a file not
/// uploaded yet, one edited since, a deletion not carried up yet, or anything
/// the engine never uploads. That is why ignored names count here, and why the
/// walk is its own rather than [`walk_local_tree`], which skips symlinks.
pub fn mirror_subtree_unsynced(
    root: &Path,
    rel: &str,
    baseline: &HashMap<String, StoredSyncEntry>,
) -> Option<String> {
    let mut seen = HashSet::new();
    if let Some(reason) = unsynced_local(root, rel, baseline, None, &mut seen) {
        return Some(reason);
    }
    let prefix = format!("{rel}/");
    baseline
        .keys()
        .filter(|r| *r == rel || r.starts_with(&prefix))
        .find(|r| !seen.contains(r.as_str()))
        .map(|r| format!("{r} was deleted here but not on Proton Drive yet"))
}

/// [`mirror_subtree_unsynced`] for a whole mirror folder: why its local copy
/// holds something Proton Drive does not, or `None` when evicting it loses
/// nothing. The switch to on-demand asks this before it empties the folder.
///
/// A push pass that ends `idle` is not enough on its own. It never uploads an
/// ignored path, a symlink or a folder it cannot read, it defers a file open
/// for writing, and a file can change after it finishes; the eviction deleted
/// all of those for good (B198).
///
/// A tracked path deleted here but still in the baseline counts, since the
/// mount would show it again. An ignored one does not: ignoring never carries
/// a deletion to Drive, so its row stays in the baseline for good, and holding
/// the switch on it would hold it forever.
pub fn mirror_tree_unsynced(
    root: &Path,
    baseline: &HashMap<String, StoredSyncEntry>,
    rules: &IgnoreRules,
) -> Option<String> {
    let mut seen = HashSet::new();
    if let Some(reason) = unsynced_local(root, "", baseline, Some(rules), &mut seen) {
        return Some(reason);
    }
    let mut missing: Vec<String> = filter_baseline(baseline, rules)
        .into_keys()
        .filter(|r| !seen.contains(r.as_str()))
        .collect();
    missing.sort_unstable();
    missing
        .first()
        .map(|r| format!("{r} was deleted here but not on Proton Drive yet"))
}

/// Whether a mirror folder's local copy is gone in full: the folder is still
/// there, but not one tracked path of its baseline is.
///
/// [`mirror_tree_unsynced`] reads that as deletions not carried up yet, which
/// would refuse the switch to on-demand for good. It is the wipe guard's case
/// instead, which the push pass a queued switch runs settles without trashing
/// anything on Drive (#46), so the switch waits for that pass.
pub fn mirror_tree_gone(
    root: &Path,
    baseline: &HashMap<String, StoredSyncEntry>,
    rules: &IgnoreRules,
) -> bool {
    let tracked = filter_baseline(baseline, rules);
    root.is_dir()
        && !tracked.is_empty()
        && tracked.keys().all(|rel| {
            std::fs::symlink_metadata(root.join(rel_to_path(rel)))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        })
}

/// The walk behind both checks. `rules`, when given, only names an ignored
/// path as such in the reason; it is still something only this disk has.
pub(super) fn unsynced_local(
    root: &Path,
    rel: &str,
    baseline: &HashMap<String, StoredSyncEntry>,
    rules: Option<&IgnoreRules>,
    seen: &mut HashSet<String>,
) -> Option<String> {
    let path = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_to_path(rel))
    };
    // The folder itself has no baseline row; only what is in it does.
    if !rel.is_empty() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => return Some(format!("cannot read {rel}: {e}")),
        };
        let Some(base) = baseline.get(rel) else {
            if rules.is_some_and(|rules| rules.is_ignored(rel, meta.is_dir())) {
                return Some(format!(
                    "{rel} is excluded from sync, so it is only on this computer"
                ));
            }
            return Some(format!("{rel} is not on Proton Drive yet"));
        };
        seen.insert(rel.to_string());
        if meta.is_file() {
            return (!LocalSig::from(&meta).same_content(&LocalSig::from(base)))
                .then(|| format!("{rel} has changes that are not on Proton Drive yet"));
        }
        if !meta.is_dir() {
            return Some(format!("{rel} is not a regular file or folder"));
        }
    }
    let shown = if rel.is_empty() { "the folder" } else { rel };
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(e) => return Some(format!("cannot read {shown}: {e}")),
    };
    for entry in entries {
        let name = match entry {
            Ok(entry) => entry.file_name(),
            Err(e) => return Some(format!("cannot read {shown}: {e}")),
        };
        let Some(name) = name.to_str() else {
            return Some(format!("{shown} holds a name that is not valid UTF-8"));
        };
        // A download the engine left half done; it is the engine's to clean up.
        if name.contains(".pdfs-tmp-") {
            continue;
        }
        if let Some(reason) = unsynced_local(root, &join_rel(rel, name), baseline, rules, seen) {
            return Some(reason);
        }
    }
    None
}

/// Publish a conflict copy without overwriting an earlier conflict bearing the
/// same timestamp. The original is removed only after the new copy is durable.
pub(super) fn preserve_conflict_copy(path: &Path, stamp: i64) -> std::io::Result<PathBuf> {
    let metadata = std::fs::metadata(path)?;
    for suffix in 0..=u32::MAX {
        let conflict = conflict_path_with_suffix(path, stamp, suffix);
        let mut source = std::fs::File::open(path)?;
        let mut target = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&conflict)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Err(e) = std::io::copy(&mut source, &mut target).and_then(|_| target.sync_all()) {
            let _ = std::fs::remove_file(&conflict);
            return Err(e);
        }
        target.set_permissions(metadata.permissions())?;
        if let Ok(modified) = metadata.modified() {
            target.set_modified(modified)?;
        }
        target.sync_all()?;
        sync_parent_dir(&conflict)?;
        std::fs::remove_file(path)?;
        sync_parent_dir(&conflict)?;
        return Ok(conflict);
    }
    unreachable!("u32 conflict suffix space exhausted")
}

pub(super) fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) => std::fs::File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

/// Where a download into `path` is written before it is placed: next to it, so
/// the rename that places it stays on one filesystem. A name with
/// `.pdfs-tmp-` in it is never scanned or synced.
pub(super) fn staging_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "pdfs-tmp-{}",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

/// Put a finished, durable download at `tmp` in place at `path`, stamped with
/// the remote modification time `mtime`.
///
/// What is at `path` is checked one last time first: anything other than
/// `expected` means a writer got there first, and its bytes are the ones
/// nothing else has a copy of, so they are set aside as a conflict copy
/// (`docs/BUGS.md` B39). On that failure `tmp` is removed.
pub(super) fn place_download(
    tmp: &Path,
    path: &Path,
    expected: Option<LocalSig>,
    mtime: i64,
    rel: &str,
) -> Result<(), String> {
    if let Err(e) = keep_racing_local_edit(path, expected) {
        let _ = std::fs::remove_file(tmp);
        return Err(format!("set aside racing local edit for {rel}: {e}"));
    }
    std::fs::rename(tmp, path).map_err(|e| format!("place {rel}: {e}"))?;
    // Match local mtime to the remote's so neither side looks "changed" next pass.
    let f = std::fs::File::options()
        .write(true)
        .open(path)
        .map_err(|e| format!("open placed file {rel}: {e}"))?;
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64))
        .map_err(|e| format!("set mtime for {rel}: {e}"))?;
    f.sync_all()
        .map_err(|e| format!("sync placed file {rel}: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| format!("sync directory for {rel}: {e}"))?;
    }
    Ok(())
}
