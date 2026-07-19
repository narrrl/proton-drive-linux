//! Filesystem walker behind the daemon's index of *local* (non-Drive) files.
//!
//! The launcher prompt searches Proton Drive and the machine's own files side by
//! side. Drive names come from the `nodes` FTS index; local names come from the
//! `local_files` index this module feeds. The daemon runs [`scan`] on a
//! background thread — never on a FUSE or control-socket thread — and streams
//! batches into [`crate::db::Db::local_upsert_batch`].
//!
//! Walking is deliberately shallow on cost: we stat each entry once (the walker
//! already has the `DirEntry` metadata) and skip the directories that dominate a
//! home directory's inode count without ever being interesting to search
//! (`node_modules`, `target`, caches, VCS internals). The Drive mountpoint is
//! always excluded — walking it would fault in every remote node through FUSE.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ignore::{WalkBuilder, WalkState};

/// Directory names skipped anywhere in the tree. These hold build artefacts,
/// dependency trees, and caches: high inode count, no search value.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "__pycache__",
    "venv",
    ".venv",
    "vendor",
    "dist-newstyle",
    "Trash",
];

/// Hard cap on indexed entries, so a pathological home directory cannot grow the
/// database without bound. Reaching it stops the walk early.
const MAX_ENTRIES: usize = 400_000;

/// Rows handed to the sink at a time. Large enough that the per-transaction cost
/// amortises, small enough that the writer lock is released often (FUSE
/// write-through shares the same connection).
const BATCH: usize = 2_000;

/// One indexed local file or directory.
#[derive(Debug, Clone)]
pub struct LocalEntry {
    /// Absolute path.
    pub path: String,
    /// Final path component.
    pub name: String,
    pub is_dir: bool,
    /// Size in bytes (0 for directories).
    pub size: i64,
    /// Modification time, epoch seconds.
    pub mtime: i64,
}

/// Walk `roots` in parallel, calling `sink` with batches of entries.
///
/// Hidden files and anything under `excludes` (the Drive mountpoint, our own
/// state/cache dirs) are skipped, as are the [`SKIP_DIRS`] names at any depth.
/// Symlinks are not followed, so a link loop cannot hang the scan. Returns the
/// number of entries handed to `sink`.
///
/// `sink` is called from the walker's worker threads, serialized by an internal
/// mutex; it must not block for long.
pub fn scan(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    sink: impl FnMut(Vec<LocalEntry>) + Send,
) -> usize {
    let Some((first, rest)) = roots.split_first() else {
        return 0;
    };

    let mut builder = WalkBuilder::new(first);
    for root in rest {
        builder.add(root);
    }
    let excludes: Vec<PathBuf> = excludes.to_vec();
    builder
        // `standard_filters` would honour .gitignore/.ignore files: a source
        // tree's ignored-but-present files are still files the user may want to
        // find, so only the hidden filter stays on.
        .standard_filters(false)
        .hidden(true)
        .follow_links(false)
        .same_file_system(true)
        .threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(8)))
        .filter_entry(move |entry| {
            let path = entry.path();
            if excludes.iter().any(|e| path.starts_with(e)) {
                return false;
            }
            !matches!(entry.file_name().to_str(), Some(name) if SKIP_DIRS.contains(&name))
        });

    let state = parking_lot::Mutex::new(SinkState {
        buf: Vec::with_capacity(BATCH),
        total: 0,
        sink,
    });

    builder.build_parallel().run(|| {
        Box::new(|result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            // Depth 0 is a root itself; indexing it adds nothing to search.
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            let Some(local) = to_entry(&entry) else {
                return WalkState::Continue;
            };

            let mut state = state.lock();
            state.push(local);
            if state.total >= MAX_ENTRIES {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });

    let mut state = state.into_inner();
    state.flush();
    state.total
}

/// Batching buffer shared by the walker's worker threads.
struct SinkState<F: FnMut(Vec<LocalEntry>)> {
    buf: Vec<LocalEntry>,
    total: usize,
    sink: F,
}

impl<F: FnMut(Vec<LocalEntry>)> SinkState<F> {
    fn push(&mut self, entry: LocalEntry) {
        self.buf.push(entry);
        self.total += 1;
        if self.buf.len() >= BATCH {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if !self.buf.is_empty() {
            let batch = std::mem::take(&mut self.buf);
            (self.sink)(batch);
        }
    }
}

/// Convert a walker entry into a [`LocalEntry`], reusing the metadata the walker
/// already fetched. Entries with non-UTF-8 paths are dropped: the index (and the
/// JSON wire format) is UTF-8 only.
fn to_entry(entry: &ignore::DirEntry) -> Option<LocalEntry> {
    let path = entry.path().to_str()?.to_string();
    let name = entry.file_name().to_str()?.to_string();
    let meta = entry.metadata().ok()?;
    let is_dir = meta.is_dir();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    Some(LocalEntry {
        path,
        name,
        is_dir,
        size: if is_dir { 0 } else { meta.len() as i64 },
        mtime,
    })
}

/// The paths a scan should never descend into: the Drive mountpoint (walking it
/// would fault every remote node in through FUSE) plus our own state and cache
/// dirs, which hold blobs no user searches for by name.
pub fn default_excludes(mountpoint: &Path, state_dir: &Path, cache_dir: &Path) -> Vec<PathBuf> {
    vec![
        mountpoint.to_path_buf(),
        state_dir.to_path_buf(),
        cache_dir.to_path_buf(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scan indexes plain files, skips the excluded subtree and the junk dirs,
    /// and reports names/paths the search index can match on.
    #[test]
    fn scan_indexes_files_and_honours_excludes() {
        let tmp = std::env::temp_dir().join(format!("pdfs-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("docs")).unwrap();
        std::fs::create_dir_all(tmp.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(tmp.join("mnt")).unwrap();
        std::fs::write(tmp.join("docs/report.pdf"), b"x").unwrap();
        std::fs::write(tmp.join("node_modules/pkg/index.js"), b"x").unwrap();
        std::fs::write(tmp.join("mnt/remote.txt"), b"x").unwrap();

        let mut got = Vec::new();
        let n = scan(std::slice::from_ref(&tmp), &[tmp.join("mnt")], |batch| {
            got.extend(batch)
        });

        let names: Vec<&str> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(n, got.len());
        assert!(names.contains(&"report.pdf"));
        assert!(names.contains(&"docs"));
        // Junk dir pruned, mountpoint-style exclude pruned.
        assert!(!names.contains(&"index.js"));
        assert!(!names.contains(&"node_modules"));
        assert!(!names.contains(&"remote.txt"));

        let report = got.iter().find(|e| e.name == "report.pdf").unwrap();
        assert!(!report.is_dir);
        assert_eq!(report.size, 1);
        assert!(report.path.ends_with("docs/report.pdf"));

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
