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
    "dist",
    "dist-newstyle",
    "build",
    "Trash",
];

/// Directory *paths* skipped when they end with one of these, for caches whose
/// own name is too generic to blacklist (`mod`). The Go module cache was 63,399
/// of the 110,187 entries indexed on the machine this was measured on — 58% of
/// the index, crowding real files out of the bounded candidate pool the fuzzy
/// scorer draws from. The other big dependency caches (`.cargo`, `.rustup`,
/// `.gradle`, `.m2`) are already skipped by the hidden-file filter.
const SKIP_PATH_SUFFIXES: &[&str] = &["go/pkg/mod"];

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
            if matches!(entry.file_name().to_str(), Some(name) if SKIP_DIRS.contains(&name)) {
                return false;
            }
            !is_skipped_path(path)
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

/// Whether `path` is (or is inside) one of the [`SKIP_PATH_SUFFIXES`] caches.
/// Matched on the path's own components so a directory merely *named* `mod`
/// keeps being indexed.
fn is_skipped_path(path: &Path) -> bool {
    let path = path.to_string_lossy();
    SKIP_PATH_SUFFIXES.iter().any(|suffix| {
        path.strip_suffix(suffix)
            .is_some_and(|head| head.is_empty() || head.ends_with('/'))
    })
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
///
/// Callers should extend this with [`nested_mount_points`] for each scan root —
/// see that function for why `same_file_system(true)` is not enough on its own.
pub fn default_excludes(mountpoint: &Path, state_dir: &Path, cache_dir: &Path) -> Vec<PathBuf> {
    vec![
        mountpoint.to_path_buf(),
        state_dir.to_path_buf(),
        cache_dir.to_path_buf(),
    ]
}

/// Every mountpoint strictly below `root`, read from `/proc/self/mounts`.
///
/// [`scan`] already asks the walker to stay on one filesystem, but that check is
/// a *stat of the mountpoint* (`ignore`'s `is_same_file_system`), performed after
/// the directory is queued — so the walker has to touch a foreign mount to learn
/// it should skip it. On a dead network FUSE mount (an sshfs whose server is
/// gone, an unresponsive rclone) that stat blocks in `request_wait_answer` with
/// nobody left to answer it, and blocks *uninterruptibly*: the walker thread then
/// ignores SIGKILL, so the daemon can never exit, systemd's stop times out, and
/// the half-dead process keeps the cache.db lock that the next start needs
/// (docs/BUGS.md B90).
///
/// Excluding the mountpoints by path avoids that stat entirely. It does not
/// change what ends up indexed: a nested mount is a different device, so
/// `same_file_system(true)` was already going to skip it.
pub fn nested_mount_points(root: &Path) -> Vec<PathBuf> {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    parse_nested_mount_points(&mounts, root)
}

/// The mountpoint-field parse behind [`nested_mount_points`], split out so it can
/// be tested without a real `/proc`.
fn parse_nested_mount_points(mounts: &str, root: &Path) -> Vec<PathBuf> {
    mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|field| PathBuf::from(unescape_mount_field(field)))
        // Strictly below: `root` itself being a mountpoint must not exclude the
        // whole scan.
        .filter(|path| path.starts_with(root) && path != root)
        .collect()
}

/// Undo the octal escaping `/proc/self/mounts` applies to space, tab, newline and
/// backslash in path fields. An un-unescaped `\040` would simply fail to match a
/// real path, silently putting a mount with a space in its name back in the walk.
fn unescape_mount_field(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let escape = rest[at + 1..].get(..3);
        match escape.and_then(|digits| u8::from_str_radix(digits, 8).ok()) {
            Some(byte) => {
                out.push(byte as char);
                rest = &rest[at + 4..];
            }
            // Not an octal escape after all — keep the backslash verbatim.
            None => {
                out.push('\\');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
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

    /// The Go module cache is the single largest source of index noise on a
    /// developer's machine (58% of the entries on the one this was measured
    /// on), and it cannot be pruned by directory name: `mod` is far too
    /// common a name to blacklist outright.
    #[test]
    fn the_go_module_cache_is_pruned_but_a_plain_mod_dir_is_not() {
        let tmp = std::env::temp_dir().join(format!("pdfs-scan-gomod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("go/pkg/mod/example.com/lib")).unwrap();
        std::fs::create_dir_all(tmp.join("kernel/mod")).unwrap();
        std::fs::write(tmp.join("go/pkg/mod/example.com/lib/vendored.go"), b"x").unwrap();
        std::fs::write(tmp.join("kernel/mod/mine.conf"), b"x").unwrap();

        let mut got = Vec::new();
        scan(std::slice::from_ref(&tmp), &[], |batch| got.extend(batch));

        let names: Vec<&str> = got.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"vendored.go"), "{names:?}");
        assert!(names.contains(&"mine.conf"), "{names:?}");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// A dead network FUSE mount under the scan root must be skipped by *path*,
    /// before anything stats it — see `nested_mount_points`. The scan root itself
    /// being a mountpoint must not exclude the entire walk.
    #[test]
    fn nested_mount_points_are_excluded_but_the_root_mount_is_not() {
        let mounts = "\
/dev/sda2 /home ext4 rw 0 0
narl@narl.io:/opt /home/narl/remote/narl.io fuse.sshfs rw 0 0
google_drive: /home/narl/remote/gdrive fuse.rclone rw 0 0
tmpfs /run/user/1000 tmpfs rw 0 0
";
        let nested = parse_nested_mount_points(mounts, Path::new("/home/narl"));
        assert_eq!(
            nested,
            vec![
                PathBuf::from("/home/narl/remote/narl.io"),
                PathBuf::from("/home/narl/remote/gdrive"),
            ]
        );

        // `/home` is an ancestor, not a descendant: excluding it would empty the
        // scan. `/run/user/1000` is unrelated.
        assert!(parse_nested_mount_points(mounts, Path::new("/home")).len() == 2);
    }

    /// `/proc/self/mounts` octal-escapes a space; leaving it escaped would put a
    /// mount whose path contains one back into the walk.
    #[test]
    fn mount_paths_are_unescaped() {
        let mounts = "srv:/x /home/narl/My\\040Cloud fuse.sshfs rw 0 0\n";
        assert_eq!(
            parse_nested_mount_points(mounts, Path::new("/home/narl")),
            vec![PathBuf::from("/home/narl/My Cloud")]
        );
        assert_eq!(unescape_mount_field("/a\\134b"), "/a\\b");
        assert_eq!(unescape_mount_field("/plain/path"), "/plain/path");
    }
}
