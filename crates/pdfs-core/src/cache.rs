//! On-disk content cache and pin registry.
//!
//! Files-On-Demand keeps file *content* off local disk by default: a `read`
//! hydrates the requested byte range straight from the remote. Two things make
//! that cache-worthy:
//!
//! * **Pinning** — the user marks a file "always keep on this device". Its full
//!   plaintext is downloaded once and stored under [`AppDirs::cache_dir`]; every
//!   later `read` is served from disk with no network.
//! * **Validation** — Proton does not hand us a stable revision id on the public
//!   [`Node`], so a cached blob is validated against the node's
//!   `(modification_time, plaintext_size)` pair. A new revision changes at least
//!   the mtime, so a stale blob is detected and ignored (and the event poller
//!   evicts it eagerly anyway).
//!
//! [`AppDirs::cache_dir`]: crate::config::AppDirs::cache_dir
//! [`Node`]: proton_drive_rs::Node

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use proton_drive_rs::proton_sdk::ids::NodeUid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

use parking_lot::Mutex;

use crate::db::{CacheEntryInput, Db};
use crate::error::{Error, Result};

/// `cache_entries.kind` tag for whole-file content blobs.
const KIND_BLOB: &str = "blob";
static STAGING_N: AtomicU64 = AtomicU64::new(0);
/// `cache_entries.kind` tag for on-demand block-cache chunks.
const KIND_BLOCK: &str = "block";
/// `cache_entries.kind` tag for cached thumbnails.
const KIND_THUMB: &str = "thumb";

/// How the configured budget is divided between the three pools, in percent.
///
/// The alternative — one shared cap across all kinds — was rejected: the pools
/// have genuinely different lifetimes, and sharing lets one starve another. A
/// single streaming read is millions of block bytes and would evict the whole
/// blob cache behind it; a gallery scroll would do the same with thumbnails.
///
/// Whole-file blobs get the bulk: they are what "available offline" means, and
/// they are the only pool a user explicitly asks for (by pinning). Blocks are
/// transient partial reads, cheaply re-fetched. Thumbnails are tiny
/// individually — a few percent buys tens of thousands of them — but there is
/// one per photo, so the pool needs a ceiling, which is what C6 was about.
///
/// Must sum to 100; asserted at compile time below.
const SPLIT_BLOB: u64 = 70;
const SPLIT_BLOCK: u64 = 25;
const SPLIT_THUMB: u64 = 5;
const _: () = assert!(SPLIT_BLOB + SPLIT_BLOCK + SPLIT_THUMB == 100);

/// Current wall-clock time in unix *milliseconds*, for the LRU `last_accessed`
/// column. Milliseconds (not seconds) so two cache events in the same second
/// still order correctly — a coarse second-granularity clock would make LRU
/// eviction order indeterminate under bursty access.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Block size the on-demand block cache falls back to when a revision's real
/// geometry is not known yet. Matches the SDK's default content block size
/// (`DEFAULT_BLOCK_SIZE`, 4 MiB), which is what this client's own uploads
/// produce — so for a file this client wrote, the fallback *is* the geometry.
///
/// It is only a fallback, though. See [`BlockGeometry`].
pub const BLOCK_SIZE: u64 = 1 << 22;

/// Buffered LRU touches that trigger a flush. Sized so a sequential read of a
/// large file flushes a few times rather than per block, while an idle cache
/// never holds more than a few hundred pending keys.
const TOUCH_FLUSH_AT: usize = 256;

/// How long block `idx` of a file of `size` bytes must be: a full
/// [`BLOCK_SIZE`] except for the last one, which holds the remainder.
///
/// Public because it is the *contract* a block has to satisfy, not a detail of
/// this cache: the network read path validates against it too, so a block that
/// comes back short is refused before it can truncate a file (bugs.md B84).
pub fn block_len(size: u64, idx: u64) -> u64 {
    let start = idx.saturating_mul(BLOCK_SIZE);
    size.saturating_sub(start).min(BLOCK_SIZE)
}

/// One block of a revision, as the read path and the block cache address it:
/// its index in the revision's block table, and the plaintext range it covers.
///
/// `start` travels with `idx` because the index alone stopped being enough once
/// the geometry came from the revision rather than from a constant — see
/// [`BlockGeometry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockSpan {
    /// Position in the revision's block table.
    pub idx: u64,
    /// Plaintext offset the block begins at.
    pub start: u64,
    /// Plaintext length of the block.
    pub len: u64,
}

impl BlockSpan {
    /// One past the block's last plaintext byte.
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// Where a revision's content blocks actually begin and end in its plaintext.
///
/// The read path used to assume the answer: [`BLOCK_SIZE`] blocks with a
/// remainder at the end, encoded as `idx * BLOCK_SIZE` and [`block_len`]. That
/// holds for revisions this client uploaded, and it does not hold in general —
/// `RevisionReader::block_sizes()` reports per-block plaintext sizes that some
/// accounts have revisions for which are neither 4 MiB nor uniform
/// (`docs/BUGS.md` B85).
///
/// Getting it wrong is not a truncation: `read_at` plans over the real sizes
/// and clamps to the range asked for, so the bytes are right either way. What
/// it costs is work. A client block that straddles two server blocks makes the
/// reader fetch and decrypt both to answer for one, and the block written to
/// the on-disk cache then lines up with nothing the next read will ask for.
/// Planning from the revision restores the one-cached-block-is-one-fetched-block
/// property the cache was designed around.
///
/// [`BlockGeometry::uniform`] is the old assumption, kept as the answer for a
/// file whose real geometry has not been learned yet — the first read of a file
/// has no reader open, and resolving one just to plan would reintroduce the
/// per-read key derivation the block cache exists to avoid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockGeometry {
    /// Plaintext start offset of each block; same length as `sizes`.
    starts: Vec<u64>,
    sizes: Vec<u64>,
    size: u64,
}

impl BlockGeometry {
    /// The geometry a revision reports, as a prefix sum over its block sizes.
    ///
    /// Trailing zero-length blocks are dropped: they name no bytes, so they can
    /// only produce empty spans that every caller would have to special-case.
    pub fn from_sizes(sizes: &[u64]) -> Self {
        let mut kept: Vec<u64> = sizes.to_vec();
        while kept.last() == Some(&0) {
            kept.pop();
        }
        let mut starts = Vec::with_capacity(kept.len());
        let mut at = 0_u64;
        for &len in &kept {
            starts.push(at);
            at = at.saturating_add(len);
        }
        Self {
            starts,
            sizes: kept,
            size: at,
        }
    }

    /// The [`BLOCK_SIZE`] assumption, for a file of `size` bytes.
    pub fn uniform(size: u64) -> Self {
        let blocks = size.div_ceil(BLOCK_SIZE);
        Self::from_sizes(&(0..blocks).map(|i| block_len(size, i)).collect::<Vec<_>>())
    }

    /// Total plaintext size this geometry accounts for.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Number of blocks.
    pub fn blocks(&self) -> u64 {
        self.sizes.len() as u64
    }

    /// Block `idx`, or `None` past the end of the table.
    pub fn span(&self, idx: u64) -> Option<BlockSpan> {
        let i = usize::try_from(idx).ok()?;
        Some(BlockSpan {
            idx,
            start: *self.starts.get(i)?,
            len: self.sizes[i],
        })
    }

    /// The index of the block containing plaintext offset `at`, or `None` at or
    /// past the end.
    pub fn index_at(&self, at: u64) -> Option<u64> {
        if at >= self.size {
            return None;
        }
        // `starts` is sorted, so the containing block is the last one that
        // begins at or before `at`.
        let i = self.starts.partition_point(|&s| s <= at).checked_sub(1)?;
        Some(i as u64)
    }

    /// Every block overlapping the plaintext range `[offset, end)`, in order.
    /// Empty when the range is empty or lies wholly past the end.
    pub fn spans(&self, offset: u64, end: u64) -> Vec<BlockSpan> {
        let Some(first) = self.index_at(offset) else {
            return Vec::new();
        };
        if end <= offset {
            return Vec::new();
        }
        let last = self
            .index_at(end - 1)
            .unwrap_or_else(|| self.blocks().saturating_sub(1));
        (first..=last).filter_map(|i| self.span(i)).collect()
    }
}

/// How many eviction candidates to pull per batch when a pool is over budget.
/// Large enough that a normal overshoot (a store or two past the cap) is settled
/// in one query, small enough that a badly-over-budget cache does not read its
/// whole index to drop the oldest few.
const EVICT_BATCH: usize = 64;

/// Validity tag stored alongside a cached blob. A blob is fresh only if both
/// fields still match the node's current metadata.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
struct Meta {
    /// Node modification time, epoch seconds.
    mtime: i64,
    /// Plaintext size in bytes.
    size: u64,
}

/// Validity tag stored alongside a cached *block*: [`Meta`] plus where in the
/// plaintext the block begins.
///
/// The offset is what makes the block re-plannable. See
/// [`ContentCache::block_tag`] for why `(mtime, size)` alone stopped being
/// enough, and why `start` is optional.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
struct BlockMeta {
    mtime: i64,
    size: u64,
    /// Absent in sidecars written before the geometry became per-revision;
    /// those are read as "wherever the 4 MiB assumption would have put it".
    #[serde(default)]
    start: Option<u64>,
}

/// A revision's block geometry as cached on disk, so a read can plan without
/// opening a [`RevisionReader`](proton_sdk) first.
///
/// Learning the geometry costs a revision listing and the file's key
/// derivation, which is exactly the per-read work the block cache exists to
/// avoid. So it is learned as a side effect of the first read — which has to
/// open a reader anyway to fetch anything — written here, and used to plan
/// every read after it. The first read of a file therefore still plans on
/// [`BlockGeometry::uniform`]; if that turns out to be wrong, the blocks it
/// cached are simply not the blocks the next read asks for, and they age out.
#[derive(Serialize, Deserialize, Clone)]
struct GeometryMeta {
    mtime: i64,
    size: u64,
    /// Plaintext size of each content block, in block order.
    sizes: Vec<u64>,
}

/// One pin, as carried over the control socket and listed in `status`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pin {
    /// Node uid in `volume~link` display form.
    pub uid: String,
    /// Last path the node was pinned under, for display in `status`. Advisory
    /// only — the uid is the identity.
    pub path: String,
    /// A folder pin whose whole subtree is kept on disk. `false` for a single
    /// file. Defaulted so a legacy `pins.json` (file pins only) still parses.
    #[serde(default)]
    pub recursive: bool,
    /// Whether the pinned node is a folder, when the node cache knows.
    ///
    /// `recursive` is pin *policy*, not node kind: a folder pinned
    /// non-recursively has `recursive: false`, and a front-end reading it as
    /// "this is a file" draws a file icon and sends an invalid `OpenFile`. This
    /// is resolved from the `nodes` row at list time, so it is `None` for a pin
    /// whose node is not cached — and absent from an older daemon's reply,
    /// which is why it is defaulted.
    #[serde(default)]
    pub is_dir: Option<bool>,
}

/// The legacy JSON pin registry, kept only to import a pre-P5 `pins.json` into
/// the DB once on open. Live pins are owned by the `pins` table.
#[derive(Serialize, Deserialize, Default)]
struct PinFile {
    pins: BTreeMap<String, Pin>,
}

/// What a staged file actually contains, written beside it as `<name>.json`
/// (offline.md Phase 2).
///
/// A staged file is **not** necessarily valid whole-file content, and that is
/// the whole reason this exists. A partial overwrite commits by filling the
/// untouched regions from the remote base — which is exactly what fails when the
/// network is down, so what lands in staging is the authored bytes with *zeros*
/// in the gaps. Uploading that as-is would silently corrupt the file.
/// `authored` says which ranges are real; `complete` says whether the file can
/// be uploaded as it stands.
/// What `staging/` currently holds, from [`ContentCache::staging_usage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StagingUsage {
    /// Bytes across every staged blob.
    pub bytes: u64,
    /// Staged blobs, not counting their sidecars.
    pub files: u64,
    /// Age of the oldest staged blob, in seconds. Zero when there are none —
    /// what makes a figure here interesting is it being *large*, which is the
    /// shape of a write that is never going to drain by itself.
    pub oldest_secs: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StagedWrite {
    /// Node the write targeted, in `volume~link` display form.
    pub uid: String,
    /// Length of the intended new content.
    pub len: u64,
    /// Size and mtime of the revision the file's *untouched* ranges came from,
    /// i.e. what a gap-fill reads. Meaningless once `complete` is true.
    pub base_size: u64,
    pub base_mtime: i64,
    /// Locally authored `[start, end)` ranges. Everything else in the file is a
    /// zero-filled hole, not content.
    pub authored: Vec<(u64, u64)>,
    /// True when `authored` covers the whole file, i.e. the staged bytes are the
    /// complete new content and can be uploaded directly.
    pub complete: bool,
    /// The remote revision this change was made against, if it is known.
    ///
    /// Distinct from `base_size`/`base_mtime`, which describe wherever the
    /// untouched bytes are to be read from — for a write that supersedes an
    /// earlier queued one, that is the *previous staged blob*, not the remote.
    /// This instead always names the server's revision, carried across
    /// supersedes, so the drain can tell whether the remote moved on under a
    /// queued change and keep a conflict copy rather than clobber it.
    ///
    /// `None` on a sidecar written before this field existed, and for a node
    /// that has never existed remotely; both mean "do not conflict-check".
    #[serde(default)]
    pub based_on: Option<Baseline>,
}

/// The identity of a remote revision, as far as we can observe it.
///
/// `revision_id` is the authoritative identity when present: the server assigns a
/// fresh id to every sealed revision, so an unchanged id proves no one rewrote
/// the file — even when the server re-stamped its `mtime` on the *same* revision,
/// which is the drift that used to cut spurious conflict copies (B16/B25).
/// `mtime`/`size` remain the fallback for a baseline recorded before the id was
/// observable, or a node read through a surface that does not carry it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub mtime: i64,
    pub size: u64,
    #[serde(default)]
    pub hash: Option<String>,
    /// Server revision id this change was made against (`None` if unknown).
    #[serde(default)]
    pub revision_id: Option<String>,
}

/// A denied-write rescue that retained the bytes but could not fully publish
/// their recovery metadata.
#[derive(Debug)]
pub struct PreserveWriteError {
    /// The actual path that still contains the accepted bytes.
    pub surviving_path: PathBuf,
    /// The recovery metadata that still describes those bytes, when one could
    /// be made durable. Callers must never clear this after an error.
    pub marker_path: Option<PathBuf>,
    error: Error,
}

impl fmt::Display for PreserveWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (bytes retained at {})",
            self.error,
            self.surviving_path.display()
        )
    }
}

impl std::error::Error for PreserveWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Content cache rooted at a directory, with a sibling pin-registry file.
pub struct ContentCache {
    /// Directory holding `<key>` blobs and `<key>.meta` tags.
    content_dir: PathBuf,
    /// Subdirectory holding cached thumbnails (`<key>.t<n>` + `.meta`). Kept out
    /// of `content_dir` so the budget scan never sees thumbnail files.
    thumb_dir: PathBuf,
    /// Owner-protected scratch space for decrypted camera RAWs passed to
    /// exiftool. Files are removed on drop and swept on the next cache open after
    /// an unclean shutdown.
    raw_thumb_dir: PathBuf,
    /// Subdirectory holding on-demand block-cache files (`<key>.b<idx>` +
    /// `.meta`). Kept out of `content_dir` so the whole-file budget scan never
    /// sees them; blocks carry their own LRU budget.
    block_dir: PathBuf,
    /// Subdirectory for write-handle scratch files (disk-backed write buffers).
    /// Emptied on open so a crashed run leaves no orphans.
    scratch_dir: PathBuf,
    /// Subdirectory holding the bytes of writes that have not been uploaded yet
    /// (offline.md Phase 2/3) — every released write passes through here, since
    /// the upload is a queued op performed later. Unlike `scratch_dir` this is
    /// **never** emptied on open: these are the only copy of content the user
    /// authored, and the whole point is that they outlive the daemon.
    staging_dir: PathBuf,
    /// Subdirectory holding scratch files rescued from an unclean shutdown —
    /// writes an application had `fsync`ed but not yet closed. Populated only at
    /// open, drained by the daemon into `staging_dir` once it can address the
    /// nodes they belong to. Like `staging_dir` and unlike `scratch_dir`, it is
    /// never emptied blindly.
    recovery_dir: PathBuf,
    /// JSON pin registry path.
    pins_path: PathBuf,
    /// Soft cap on total blob bytes. Exceeded only transiently: a `store`
    /// evicts least-recently-used *unpinned* blobs back under the cap. `0`
    /// disables the cap. Pinned blobs are never evicted, so pins alone may push
    /// the cache over budget. Atomic so the daemon can retune it at runtime
    /// (a Settings-page change) via [`set_budget`](Self::set_budget) without
    /// taking a lock on every cache read.
    max_bytes: AtomicU64,
    /// Running byte totals for the two budgeted pools, so the overwhelmingly
    /// common "still under budget" answer costs an atomic load instead of a
    /// database query.
    ///
    /// These are a *fast path*, not the source of truth — the `cache_entries`
    /// index is. They are seeded from it at open (right after `reconcile`, so
    /// they describe what is actually on disk), advanced on each store, and
    /// **re-seeded from the index after every eviction pass**, which is what
    /// keeps a long-running daemon from drifting: the only operations that
    /// change a total without going through here are external file deletions,
    /// and the next open reconciles those.
    blob_bytes: AtomicU64,
    block_bytes: AtomicU64,
    thumb_bytes: AtomicU64,
    /// Unified metadata DB. Its `cache_entries` table is the LRU index: every
    /// store/read/evict updates it, and the budget enforcers query it instead of
    /// scanning the cache directories (plan.md P4).
    db: Arc<Db>,
    /// LRU touches not yet written to `cache_entries`, keyed by cache key.
    ///
    /// A cache hit only reorders eviction candidates, so it does not have to be
    /// durable — but as one write transaction per hit it was 32 transactions per
    /// 4 MiB block delivered, taken on the read path, under the connection mutex
    /// every `lookup` also needs. Buffered here and flushed in one transaction
    /// once [`TOUCH_FLUSH_AT`] have accumulated, and always before an eviction
    /// pass reads the ordering these describe.
    touches: Mutex<HashMap<String, i64>>,
    /// Whether `recovery/` may hold a write the daemon has not queued yet.
    ///
    /// The drain asks for recovered writes on every idle pass — a `read_dir` of
    /// a directory that is empty in every run that did not crash. The directory
    /// gains entries in exactly two places (the rescue at open, and the release
    /// path that parks a still-authoritative scratch file), both of which set
    /// this; a scan that finds nothing actionable clears it.
    recovery_pending: AtomicBool,
}

impl ContentCache {
    fn staging_path(&self, meta: &StagedWrite) -> PathBuf {
        // The uid goes in the name so a staged file can be tied back to its node
        // without a database; `/` in a uid would otherwise open a path.
        let safe_uid = meta.uid.replace(['/', '\\'], "_");
        self.staging_dir.join(format!(
            "{}-{}-{}",
            safe_uid,
            now_millis(),
            STAGING_N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Open (and create) a cache under `content_dir`, with the pin registry at
    /// `pins_path` and a `max_bytes` size cap (`0` = unlimited). Both parent
    /// directories are created if missing. `db` is the shared metadata database
    /// whose `cache_entries` table backs LRU eviction; the on-disk cache is
    /// reconciled into it on open.
    pub fn open(
        content_dir: PathBuf,
        pins_path: PathBuf,
        max_bytes: u64,
        db: Arc<Db>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&content_dir)?;
        let thumb_dir = content_dir.join("thumbs");
        std::fs::create_dir_all(&thumb_dir)?;
        let raw_thumb_dir = content_dir.join("raw-thumbnails");
        std::fs::create_dir_all(&raw_thumb_dir)?;
        std::fs::set_permissions(&raw_thumb_dir, std::fs::Permissions::from_mode(0o700))?;
        if let Ok(entries) = std::fs::read_dir(&raw_thumb_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("pdfs-raw-")
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let block_dir = content_dir.join("blocks");
        std::fs::create_dir_all(&block_dir)?;
        // Scratch holds disk-backed write buffers. A previous run's leftovers are
        // worthless *except* where the application asked for durability: a handle
        // that was `fsync`ed but never closed left a sidecar behind, and POSIX
        // says those bytes are on stable storage. Rescue those, discard the rest.
        let scratch_dir = content_dir.join("scratch");
        let recovery_dir = content_dir.join("recovery");
        std::fs::create_dir_all(&recovery_dir)?;
        Self::rescue_scratch(&scratch_dir, &recovery_dir);
        // A failed rescue may leave the only durable copy here. Never clear the
        // directory wholesale; discard only blobs that carry no fsync marker.
        Self::discard_unmarked_scratch(&scratch_dir);
        std::fs::create_dir_all(&scratch_dir)?;
        // Staging, by contrast, is deliberately preserved across runs.
        let staging_dir = content_dir.join("staging");
        std::fs::create_dir_all(&staging_dir)?;
        if let Some(parent) = pins_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let cache = Self {
            content_dir,
            thumb_dir,
            raw_thumb_dir,
            block_dir,
            scratch_dir,
            staging_dir,
            recovery_dir,
            pins_path,
            max_bytes: AtomicU64::new(max_bytes),
            blob_bytes: AtomicU64::new(0),
            block_bytes: AtomicU64::new(0),
            thumb_bytes: AtomicU64::new(0),
            db,
            touches: Mutex::new(HashMap::new()),
            // The rescue above has already run, so start by looking once.
            recovery_pending: AtomicBool::new(true),
        };
        cache.reconcile()?;
        // After reconcile, so the totals describe the index that now describes
        // the disk.
        cache.reseed_totals();
        cache.import_legacy_pins()?;
        Ok(cache)
    }

    /// One-time migration of a pre-P5 `pins.json` into the DB `pins` table, then
    /// delete the file so it never re-imports. Legacy pins were all whole-file
    /// (non-recursive). Absent file → nothing to do.
    fn import_legacy_pins(&self) -> Result<()> {
        let Ok(bytes) = std::fs::read(&self.pins_path) else {
            return Ok(());
        };
        if let Ok(file) = serde_json::from_slice::<PinFile>(&bytes) {
            for (uid, pin) in file.pins {
                self.db.pin_add(&uid, &pin.path, false)?;
            }
        }
        let _ = std::fs::remove_file(&self.pins_path);
        Ok(())
    }

    /// Rebuild the `cache_entries` LRU index from the on-disk cache. Called once
    /// on open: the index is cleared, then every blob (`content_dir`) and block
    /// (`block_dir`) file is re-registered with its on-disk size and last-modified
    /// time as the initial LRU key. This makes the DB authoritative even after a
    /// crash or an external file deletion, and picks up caches written by builds
    /// predating the index. In-run accesses then refine the ordering.
    fn reconcile(&self) -> Result<()> {
        // Both directories are walked before anything is written: the index is
        // replaced in a single transaction (see [`Db::cache_rebuild`]), so the
        // rows have to be in hand first. Names are owned for the same reason —
        // `DirEntry::file_name` does not outlive the iteration.
        let mut names: Vec<(String, &str, u64, i64)> = Vec::new();
        for (dir, kind) in [
            (&self.content_dir, KIND_BLOB),
            (&self.block_dir, KIND_BLOCK),
            (&self.thumb_dir, KIND_THUMB),
        ] {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                // `.geom` is a block-geometry sidecar (see `GeometryMeta`), tens
                // of bytes and not a cached block; like `.meta` it is neither
                // budgeted nor evictable on its own.
                if name.ends_with(".meta") || name.ends_with(".geom") {
                    continue;
                }
                // A `.tmp` is a staging file from a store that never completed
                // its rename — a crashed or killed run. Nothing will ever claim
                // it, and no pass other than this one looks in these
                // directories, so it would sit there forever. Sweep it.
                if name.ends_with(".tmp") {
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let at = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                names.push((name.to_string(), kind, meta.len(), at));
            }
        }
        let entries: Vec<CacheEntryInput<'_>> = names
            .iter()
            .map(|(key, kind, size, at)| CacheEntryInput {
                key,
                kind,
                size: *size,
                last_accessed: *at,
            })
            .collect();
        self.db.cache_rebuild(&entries)?;
        Ok(())
    }

    /// Filesystem-safe, fixed-length cache key for a uid display string: the
    /// SHA-256 of `s`, hex-encoded (64 chars). A plain hex of the uid would
    /// *double* its length, and Proton's `volume~link` ids are long enough
    /// (each half can be 60+ chars) that the doubled key overflows `NAME_MAX`
    /// (255) — `store` then fails with ENAMETOOLONG. Hashing bounds the key at
    /// 64 chars; reversibility is not needed and collisions are negligible.
    fn key_str(s: &str) -> String {
        let digest = Sha256::digest(s.as_bytes());
        let mut out = String::with_capacity(digest.len() * 2);
        for b in digest {
            out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        out
    }

    /// Filesystem-safe cache key for `uid`.
    fn key(uid: &NodeUid) -> String {
        Self::key_str(&uid.to_string())
    }

    fn blob_path(&self, uid: &NodeUid) -> PathBuf {
        self.content_dir.join(Self::key(uid))
    }

    fn meta_path(&self, uid: &NodeUid) -> PathBuf {
        self.content_dir.join(format!("{}.meta", Self::key(uid)))
    }

    /// The validated blob path for `uid`, or `None` if absent or stale against
    /// `(mtime, size)`.
    fn valid_blob(&self, uid: &NodeUid, mtime: i64, size: u64) -> Option<PathBuf> {
        let want = Meta { mtime, size };
        let meta: Meta = serde_json::from_slice(&std::fs::read(self.meta_path(uid)).ok()?).ok()?;
        if meta != want {
            return None;
        }
        let blob = self.blob_path(uid);
        // Guard against a torn write: the blob must match the recorded size.
        match std::fs::metadata(&blob) {
            Ok(m) if m.len() == size => Some(blob),
            _ => None,
        }
    }

    /// Whether a fresh cached blob exists for `uid`.
    pub fn is_cached(&self, uid: &NodeUid, mtime: i64, size: u64) -> bool {
        self.valid_blob(uid, mtime, size).is_some()
    }

    /// Serve `len` bytes from `offset` out of the cached blob, or `None` on a
    /// miss/stale entry. Reads only the requested window off disk.
    pub fn read_range(
        &self,
        uid: &NodeUid,
        mtime: i64,
        size: u64,
        offset: u64,
        len: u64,
    ) -> Option<Vec<u8>> {
        let blob = self.valid_blob(uid, mtime, size)?;
        // Record the access for LRU in the cache index. Best effort — a failed
        // update only makes eviction order slightly less accurate.
        self.touch(Self::key(uid));
        let mut f = std::fs::File::open(&blob).ok()?;
        if offset >= size {
            return Some(Vec::new());
        }
        let end = offset.saturating_add(len).min(size);
        let want = (end - offset) as usize;
        f.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; want];
        f.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    /// Store `bytes` as the cached content for `uid`, tagged with `(mtime,
    /// size)`. The blob is written to a temp file then renamed so a concurrent
    /// reader never sees a partial blob; the meta tag is written last so a
    /// crash mid-store fails validation rather than serving truncated data.
    pub fn store(&self, uid: &NodeUid, mtime: i64, size: u64, bytes: &[u8]) -> Result<()> {
        let blob = self.blob_path(uid);
        // `with_extension` is safe *here* only because a blob path is a bare
        // 64-char hex key with no `.`, so this appends rather than replaces.
        // `store_thumbnail` cannot use the same idiom — see the note there.
        let tmp = blob.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            use std::io::Write;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &blob)?;
        let meta = serde_json::to_vec(&Meta { mtime, size })?;
        std::fs::write(self.meta_path(uid), meta)?;
        // Register the blob in the LRU index before enforcing the budget so the
        // newest entry is weighed (and ordered most-recent) like any other.
        self.db
            .cache_touch(&Self::key(uid), KIND_BLOB, bytes.len() as u64, now_millis())?;
        self.blob_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.enforce_budget();
        Ok(())
    }

    /// Adopt an existing on-disk file as `uid`'s cached content, without reading
    /// it into memory.
    ///
    /// [`Self::store`] takes the bytes as a slice, which means a caller holding a
    /// file has to load all of it first — fine for a thumbnail, an OOM for a
    /// pinned video. This takes the path instead and hardlinks it into place,
    /// falling back to a copy when `src` lives on another filesystem.
    ///
    /// The link makes the cache blob a second name for the same inode, so `src`
    /// must not be rewritten in place afterwards — unlinking it (the staging
    /// case, which is what this exists for) is fine and leaves the blob intact.
    pub fn store_file(&self, uid: &NodeUid, mtime: i64, size: u64, src: &Path) -> Result<()> {
        let blob = self.blob_path(uid);
        let tmp = blob.with_extension("tmp");
        // A stale tmp from an interrupted run would fail the link with EEXIST.
        let _ = std::fs::remove_file(&tmp);
        if std::fs::hard_link(src, &tmp).is_err() {
            std::fs::copy(src, &tmp)?;
        }
        if let Ok(f) = std::fs::File::open(&tmp) {
            let _ = f.sync_all();
        }
        let bytes = std::fs::metadata(&tmp)?.len();
        std::fs::rename(&tmp, &blob)?;
        let meta = serde_json::to_vec(&Meta { mtime, size })?;
        std::fs::write(self.meta_path(uid), meta)?;
        self.db
            .cache_touch(&Self::key(uid), KIND_BLOB, bytes, now_millis())?;
        self.blob_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.enforce_budget();
        Ok(())
    }

    /// Cache-index key for block `idx` of `uid` — the block file's name, which
    /// is also its `cache_entries.cache_key`.
    fn block_key(&self, uid: &NodeUid, idx: u64) -> String {
        format!("{}.b{idx}", Self::key(uid))
    }

    fn block_blob(&self, uid: &NodeUid, idx: u64) -> PathBuf {
        self.block_dir.join(self.block_key(uid, idx))
    }

    fn block_meta(&self, uid: &NodeUid, idx: u64) -> PathBuf {
        self.block_dir
            .join(format!("{}.b{idx}.meta", Self::key(uid)))
    }

    fn geometry_meta(&self, uid: &NodeUid) -> PathBuf {
        self.block_dir.join(format!("{}.geom", Self::key(uid)))
    }

    /// The block geometry recorded for this revision of `uid`, or `None` if it
    /// has not been learned (or belongs to an older revision).
    ///
    /// Callers fall back to [`BlockGeometry::uniform`], which is what every
    /// read did unconditionally before `docs/BUGS.md` B85.
    pub fn block_geometry(&self, uid: &NodeUid, mtime: i64, size: u64) -> Option<BlockGeometry> {
        let meta: GeometryMeta =
            serde_json::from_slice(&std::fs::read(self.geometry_meta(uid)).ok()?).ok()?;
        if meta.mtime != mtime || meta.size != size {
            return None;
        }
        let geometry = BlockGeometry::from_sizes(&meta.sizes);
        // A table that does not add up to the node's size describes a different
        // file than the one being read; planning on it would place every block
        // after the discrepancy wrongly. Refusing it costs a fallback to the
        // uniform assumption, which is where B84's repair path takes over.
        (geometry.size() == size).then_some(geometry)
    }

    /// Record the block geometry `sizes` for this revision of `uid`.
    ///
    /// Best effort and disposable, like every other file under the block cache:
    /// losing it costs the next read a plan on the uniform assumption, never
    /// correctness. Not counted against the block budget — it is tens of bytes
    /// per file and is what keeps the blocks that *are* counted addressable.
    pub fn store_block_geometry(&self, uid: &NodeUid, mtime: i64, size: u64, sizes: &[u64]) {
        let meta = GeometryMeta {
            mtime,
            size,
            sizes: sizes.to_vec(),
        };
        if let Ok(bytes) = serde_json::to_vec(&meta) {
            let _ = std::fs::write(self.geometry_meta(uid), bytes);
        }
    }

    /// Whether `span` of `uid` is cached and fresh for `(mtime, size)`, without
    /// reading it. For a caller deciding whether a block is worth *fetching* —
    /// reading 4 MiB to answer that would defeat the point.
    pub fn has_block(&self, uid: &NodeUid, mtime: i64, size: u64, span: BlockSpan) -> bool {
        self.block_tag(uid, mtime, size, span).is_some()
            && std::fs::metadata(self.block_blob(uid, span.idx)).is_ok_and(|m| m.len() == span.len)
    }

    /// The sidecar of block `span.idx`, if it describes the block the caller
    /// means.
    ///
    /// `(mtime, size)` catches a new revision, and used to be the whole check:
    /// with the geometry fixed at [`BLOCK_SIZE`], an index named one byte range
    /// and only one. It no longer does — the same index names a different range
    /// under the revision's own geometry than under the fallback — so the
    /// sidecar records the block's plaintext `start` and it has to agree too
    /// (`docs/BUGS.md` B85).
    ///
    /// A sidecar written before that field existed deserialises with `start`
    /// absent. Those are honoured only where the old assumption and the caller
    /// agree on the offset, which is every file this client uploaded: the
    /// existing cache survives intact for them, and is quietly refetched for the
    /// files it was placing wrongly.
    fn block_tag(
        &self,
        uid: &NodeUid,
        mtime: i64,
        size: u64,
        span: BlockSpan,
    ) -> Option<BlockMeta> {
        let meta: BlockMeta =
            serde_json::from_slice(&std::fs::read(self.block_meta(uid, span.idx)).ok()?).ok()?;
        if meta.mtime != mtime || meta.size != size {
            return None;
        }
        let recorded = meta
            .start
            .unwrap_or_else(|| span.idx.saturating_mul(BLOCK_SIZE));
        (recorded == span.start).then_some(meta)
    }

    /// Serve the cached block at `span` of `uid`, or `None` on miss/stale.
    /// Validated against `(mtime, size)` and the block's plaintext start, so
    /// both a new revision and a re-planned geometry are detected. Bumps the
    /// block's mtime for LRU, best effort.
    pub fn cached_block(
        &self,
        uid: &NodeUid,
        mtime: i64,
        size: u64,
        span: BlockSpan,
    ) -> Option<Vec<u8>> {
        self.block_tag(uid, mtime, size, span)?;
        let mut f = std::fs::File::open(self.block_blob(uid, span.idx)).ok()?;
        // The blob is not fsynced on the way in (see `store_block`), so a crash
        // can leave a short one behind the sidecar that describes it. Length is
        // what makes that detectable, the same check `valid_blob` applies to a
        // whole-file blob.
        if f.metadata().ok()?.len() != span.len {
            return None;
        }
        // LRU touch, buffered rather than written (see `touch`).
        self.touch(self.block_key(uid, span.idx));
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        Some(buf)
    }

    /// Store `bytes` as the cached block at `span` of `uid`, tagged
    /// `(mtime, size)` and the block's plaintext start. Temp-file-then-rename
    /// like [`store`](Self::store); meta written last so a crash mid-store fails
    /// validation. Enforces the block-cache LRU budget.
    ///
    /// The caller owes it exactly `span.len` bytes; anything else reads back as
    /// a miss ([`cached_block`](Self::cached_block) checks), so a bad block
    /// cannot escape this cache even though this does not check. Validating a
    /// block is the *reader's* job, at the point it is fetched — see
    /// `Core::read_block` (bugs.md B84).
    pub fn store_block(
        &self,
        uid: &NodeUid,
        mtime: i64,
        size: u64,
        span: BlockSpan,
        bytes: &[u8],
    ) -> Result<()> {
        let idx = span.idx;
        let blob = self.block_blob(uid, idx);
        let tmp = self
            .block_dir
            .join(format!("{}.b{idx}.tmp", Self::key(uid)));
        {
            let mut f = std::fs::File::create(&tmp)?;
            use std::io::Write;
            f.write_all(bytes)?;
            // Deliberately not fsynced. This runs inline on the thread a kernel
            // read is waiting on, and the block cache is disposable: the worst a
            // crash can do is leave a short blob, which `cached_block` rejects
            // on length and refetches.
        }
        std::fs::rename(&tmp, &blob)?;
        std::fs::write(
            self.block_meta(uid, idx),
            serde_json::to_vec(&BlockMeta {
                mtime,
                size,
                start: Some(span.start),
            })?,
        )?;
        self.db.cache_touch(
            &self.block_key(uid, idx),
            KIND_BLOCK,
            bytes.len() as u64,
            now_millis(),
        )?;
        self.block_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.enforce_block_budget();
        Ok(())
    }

    /// Record a cache hit for LRU ordering, without touching the database.
    ///
    /// Later touches of the same key overwrite earlier ones — only the most
    /// recent matters — so a hot file costs one map entry however many times it
    /// is read.
    fn touch(&self, key: String) {
        let flush = {
            let mut touches = self.touches.lock();
            touches.insert(key, now_millis());
            touches.len() >= TOUCH_FLUSH_AT
        };
        if flush {
            self.flush_touches();
        }
    }

    /// Write the buffered LRU touches out. Best effort: losing them costs
    /// eviction accuracy, nothing else.
    ///
    /// Public because eviction ordering is only as good as what has been
    /// flushed, and the daemon flushes on its idle passes as well as here.
    pub fn flush_touches(&self) {
        let batch: Vec<(String, i64)> = {
            let mut touches = self.touches.lock();
            if touches.is_empty() {
                return;
            }
            touches.drain().collect()
        };
        if let Err(error) = self.db.cache_accessed_batch(&batch) {
            debug!(?error, "flushing cache LRU touches failed");
        }
    }

    /// Re-read every pool total from the cache index, which is authoritative.
    fn reseed_totals(&self) {
        for kind in [KIND_BLOB, KIND_BLOCK, KIND_THUMB] {
            if let Ok(n) = self.db.cache_total_bytes(kind) {
                self.pool(kind).store(n, Ordering::Relaxed);
            }
        }
    }

    /// The running total for a pool.
    fn pool(&self, kind: &str) -> &AtomicU64 {
        match kind {
            KIND_BLOCK => &self.block_bytes,
            KIND_THUMB => &self.thumb_bytes,
            _ => &self.blob_bytes,
        }
    }

    /// This pool's share of the configured budget (`0` = unlimited, as for the
    /// whole cache). See [`SPLIT_BLOB`] for why the budget is divided rather
    /// than shared.
    fn pool_cap(&self, kind: &str) -> u64 {
        let cap = self.cap();
        if cap == 0 {
            return 0;
        }
        let share = match kind {
            KIND_BLOCK => SPLIT_BLOCK,
            KIND_THUMB => SPLIT_THUMB,
            _ => SPLIT_BLOB,
        };
        // Multiply before dividing: `cap / 100 * share` truncates a small budget
        // to zero, which `pool_cap`'s callers would read as "unlimited" — the
        // opposite of what a small budget asks for.
        //
        // At least one byte, for the same reason.
        (cap.saturating_mul(share) / 100).max(1)
    }

    /// Evict least-recently-used block-cache files until the block dir fits
    /// `max_bytes`. No-op when the cap is disabled (`0`). All blocks are
    /// evictable — pinned files are served from whole-file blobs, never blocks.
    fn enforce_block_budget(&self) {
        self.enforce_pool(KIND_BLOCK, &self.block_dir, None);
    }

    /// Evict least-recently-used entries of one pool until it fits the cap.
    ///
    /// Runs on every store, so the under-budget case is the one that matters:
    /// it is an atomic load and nothing else. Only once the running total says
    /// we are over does this touch the database, and then it works in bounded
    /// batches rather than reading the whole index — a cache that is 10 GB over
    /// budget should not materialize every row to drop the oldest few.
    ///
    /// `exempt` names keys that count toward the total but are never victims
    /// (pinned blobs). A pass that can find no eligible victim stops rather than
    /// looping: a cache held entirely by pins legitimately stays over budget.
    fn enforce_pool(&self, kind: &str, dir: &Path, exempt: Option<&HashSet<String>>) {
        let cap = self.pool_cap(kind);
        if cap == 0 {
            return;
        }
        let total = self.pool(kind);
        if total.load(Ordering::Relaxed) <= cap {
            return; // the common case: one atomic load, no query
        }

        // Over budget by the running count — now do the accurate work. The
        // buffered hits are what say which entries are cold, so they have to
        // reach the index before it is asked.
        self.flush_touches();
        let mut running = match self.db.cache_total_bytes(kind) {
            Ok(n) => n,
            Err(_) => return,
        };
        let mut skipped = 0usize;
        while running > cap {
            // Fetch a batch past whatever we have already skipped as exempt.
            let want = skipped + EVICT_BATCH;
            let Ok(batch) = self.db.cache_eviction_candidates(kind, want) else {
                break;
            };
            if batch.len() <= skipped {
                break; // nothing new to consider
            }
            let mut evicted_any = false;
            for (key, size) in batch.into_iter().skip(skipped) {
                if running <= cap {
                    break;
                }
                if exempt.is_some_and(|set| set.contains(&key)) {
                    skipped += 1; // counts toward the total, never a victim
                    continue;
                }
                let _ = std::fs::remove_file(dir.join(&key));
                let _ = std::fs::remove_file(dir.join(format!("{key}.meta")));
                let _ = self.db.cache_remove(&key);
                running = running.saturating_sub(size);
                evicted_any = true;
            }
            if !evicted_any {
                break; // every remaining candidate is exempt
            }
        }
        // The index is the truth; realign the fast path with it.
        self.reseed_totals();
    }

    /// Duplicate `src` onto `dst`, preferring a reflink.
    ///
    /// The caller is copying a staged revision — a whole file, up to whatever
    /// the user stores — so the difference between the three strategies is the
    /// difference between instant and minutes:
    ///
    /// 1. `FICLONE` shares the extents copy-on-write. On btrfs, XFS with
    ///    `reflink=1`, and bcachefs this is O(1) and consumes no extra space.
    /// 2. `copy_file_range` still copies, but entirely inside the kernel — no
    ///    round trip through a user-space buffer.
    /// 3. `std::fs::copy` is the portable fallback, and what this used to
    ///    always do.
    ///
    /// `dst` is truncated. Returns the number of bytes the destination ends up
    /// holding.
    pub fn clone_or_copy(src: &Path, dst: &Path) -> Result<u64> {
        use std::os::fd::AsRawFd as _;

        // `_IOW(0x94, 9, int)` — the whole-file reflink ioctl.
        const FICLONE: libc::c_ulong = 0x4004_9409;

        let source = std::fs::File::open(src)?;
        let len = source.metadata()?.len();
        let target = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(dst)?;

        // SAFETY: both descriptors are owned and open; FICLONE takes the source
        // fd by value and writes nothing back through the pointer-shaped arg.
        if unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0 {
            return Ok(len);
        }

        let mut copied = 0u64;
        while copied < len {
            let want = (len - copied) as usize;
            // SAFETY: owned descriptors; null offsets advance the file offsets,
            // which is what a whole-file copy wants.
            let n = unsafe {
                libc::copy_file_range(
                    source.as_raw_fd(),
                    std::ptr::null_mut(),
                    target.as_raw_fd(),
                    std::ptr::null_mut(),
                    want,
                    0,
                )
            };
            if n <= 0 {
                break;
            }
            copied += n as u64;
        }
        if copied == len {
            return Ok(len);
        }
        // A short or refused `copy_file_range` (cross-filesystem on older
        // kernels, an unsupported fs) leaves a partial destination; the
        // portable copy truncates and rewrites it from scratch.
        Ok(std::fs::copy(src, dst)?)
    }

    /// Create a fresh, empty read-write scratch file for a disk-backed write
    /// handle. Returns the open file and its path (for cleanup on release).
    pub fn create_scratch(&self) -> Result<(std::fs::File, PathBuf)> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = self.scratch_dir.join(format!(
                "w-{}-{}-{}",
                std::process::id(),
                now_millis(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)
            {
                Ok(file) => return Ok((file, path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// The sidecar path for a scratch file, holding the [`StagedWrite`] that
    /// describes it. Written by `fsync` and removed when the write is released;
    /// its presence at open is what marks a scratch file as durable rather than
    /// as a crashed run's rubbish.
    ///
    /// Safe as `with_extension` only because scratch names are dot-free (see
    /// [`Self::create_scratch`]), so this appends rather than replaces.
    pub fn scratch_sidecar(scratch: &Path) -> PathBuf {
        scratch.with_extension("json")
    }

    /// Record that the write buffered in `scratch` is durable, so a crash before
    /// `close(2)` does not lose it.
    ///
    /// Called from `fsync(2)`, whose contract is that the bytes survive a crash.
    /// The scratch file itself does survive — it is a real file — but nothing
    /// else knows it holds anything worth keeping, and open() clears the scratch
    /// directory. The sidecar is that knowledge: uid, logical length, and which
    /// ranges are authored rather than holes, which is exactly what recovery
    /// needs to hand the blob to the drain.
    ///
    /// Written whole to a temp name and renamed, so a crash mid-write leaves the
    /// previous sidecar rather than a truncated one. The caller is expected to
    /// have synced `scratch` first: a sidecar promising bytes that never reached
    /// the disk is worse than no sidecar.
    pub fn mark_scratch_durable(&self, scratch: &Path, meta: &StagedWrite) -> Result<()> {
        use std::io::Write as _;

        let side = Self::scratch_sidecar(scratch);
        let tmp = side.with_extension("json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(meta)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &side)?;
        // The rename is what publishes the sidecar; fsync the directory so the
        // rename itself survives, not just the bytes it points at.
        if let Some(dir) = side.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Drop a scratch file's durability sidecar, once the write it described has
    /// been staged (or turned out not to be dirty at all). Best effort: a
    /// leftover sidecar whose blob is gone is ignored by recovery anyway.
    pub fn clear_scratch_durable(&self, scratch: &Path) {
        let _ = std::fs::remove_file(Self::scratch_sidecar(scratch));
    }

    /// Move every scratch file that carries a sidecar into `recovery`, before the
    /// caller empties the scratch directory. A sidecar that does not parse is
    /// dropped and its blob moved anyway, unaccompanied — see below.
    ///
    /// Blob first, then sidecar: a blob in recovery without its sidecar is
    /// dropped on the next open (indistinguishable from rubbish), whereas a
    /// sidecar whose blob never arrived would describe nothing. Neither order is
    /// lossless if we crash mid-rescue, but this one fails toward "unrecovered
    /// bytes on disk" rather than "op queued against missing content".
    fn rescue_scratch(scratch_dir: &Path, recovery_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(scratch_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let blob = entry.path();
            if blob.extension().is_some_and(|e| e == "json") {
                continue;
            }
            let side = Self::scratch_sidecar(&blob);
            if !side.exists() {
                // Not marked durable, or durability marker was explicitly cleared on release
                continue;
            }
            // A sidecar that does not parse describes nothing, and inventing one
            // is worse than having none: a fabricated uid parses, so recovery
            // would queue an op against a node that does not exist and retry it
            // forever, while a fabricated `complete: true` claims the blob is the
            // whole file and stops any gap-fill. The blob still moves to
            // recovery — sidecar-less, which `recovered_writes` skips and so
            // leaves on disk for a human, the right outcome for bytes we cannot
            // describe.
            let has_valid_sidecar = std::fs::read(&side)
                .ok()
                .and_then(|b| serde_json::from_slice::<StagedWrite>(&b).ok())
                .is_some();
            let Some(name) = blob.file_name() else {
                continue;
            };
            if std::fs::rename(&blob, recovery_dir.join(name)).is_err() {
                continue;
            }
            if !has_valid_sidecar {
                let _ = std::fs::remove_file(&side);
                continue;
            }
            let Some(side_name) = side.file_name() else {
                continue;
            };
            let _ = std::fs::rename(&side, recovery_dir.join(side_name));
        }
    }

    fn discard_unmarked_scratch(scratch_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(scratch_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let blob = entry.path();
            if blob.extension().is_some_and(|e| e == "json") {
                continue;
            }
            if !Self::scratch_sidecar(&blob).exists() {
                let _ = std::fs::remove_file(blob);
            }
        }
    }

    /// Emergency eviction of unpinned blobs when local disk runs out of space (ENOSPC).
    pub fn emergency_evict(&self) {
        let pinned: HashSet<String> = self
            .db
            .pinned_uids()
            .unwrap_or_default()
            .iter()
            .map(|uid| Self::key_str(uid))
            .collect();
        let content_dir = self.content_dir.clone();
        // Force pool enforcement ignoring current cap check to free disk space immediately
        let Ok(list) = self.db.cache_eviction_candidates(KIND_BLOB, 10) else {
            return;
        };
        for (key, bytes) in list {
            if pinned.contains(&key) {
                continue;
            }
            let p = content_dir.join(&key);
            let m = content_dir.join(format!("{key}.meta"));
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::remove_file(&m);
            let _ = self.db.cache_remove(&key);
            self.blob_bytes.fetch_sub(bytes, Ordering::Relaxed);
            break; // Evict one item at a time until space is freed
        }
    }

    /// Writes rescued from an unclean shutdown, as `(blob, meta)` pairs for the
    /// daemon to hand to the upload queue. A blob whose sidecar did not survive
    /// is not actionable automatically, but is retained for manual recovery:
    /// unexplained bytes are never evidence that user data is disposable.
    ///
    /// Asked for on every idle drain pass, so the usual answer — a directory
    /// that has been empty since the last clean shutdown — costs an atomic load
    /// rather than a `read_dir` (see [`recovery_pending`](Self::recovery_pending)).
    pub fn recovered_writes(&self) -> Vec<(PathBuf, StagedWrite)> {
        if !self.recovery_pending.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let Ok(entries) = std::fs::read_dir(&self.recovery_dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let blob = entry.path();
            if blob.extension().is_some_and(|e| e == "json") {
                continue;
            }
            if let Some(meta) = std::fs::read(Self::scratch_sidecar(&blob))
                .ok()
                .and_then(|b| serde_json::from_slice::<StagedWrite>(&b).ok())
            {
                out.push((blob, meta));
            }
        }
        // Blobs whose sidecar did not survive stay here for a human and are
        // never actionable, so "nothing to queue" is the condition that stops
        // the polling — not "the directory is empty".
        if out.is_empty() {
            self.recovery_pending.store(false, Ordering::Relaxed);
        }
        out
    }

    /// Every blob in `staging/`, paired with its sidecar when it has a readable
    /// one.
    ///
    /// For the startup reconcile: staging holds the only copy of writes the
    /// kernel has already been told succeeded, and three failure paths can
    /// leave one there with no `pending_op` row pointing at it. Nothing walked
    /// this directory, so those bytes were invisible to the daemon and to the
    /// user both — `hydrate_pending` walks the op table and
    /// [`recovered_writes`](Self::recovered_writes) walks `recovery/`.
    pub fn staged_writes(&self) -> Vec<(PathBuf, Option<StagedWrite>)> {
        let Ok(entries) = std::fs::read_dir(&self.staging_dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let blob = entry.path();
            if blob.extension().is_some_and(|e| e == "json") {
                continue;
            }
            let meta = std::fs::read(Self::scratch_sidecar(&blob))
                .ok()
                .and_then(|b| serde_json::from_slice::<StagedWrite>(&b).ok());
            out.push((blob, meta));
        }
        out
    }

    /// Retire a recovered write. `stage_write` has already moved the blob out,
    /// so this is the sidecar left behind — and the blob too if the caller could
    /// not use it and said so by leaving it in place.
    pub fn discard_recovered(&self, blob: &Path) {
        let _ = std::fs::remove_file(Self::scratch_sidecar(blob));
        let _ = std::fs::remove_file(blob);
    }

    /// Move a released scratch file into the staging directory with a
    /// [`StagedWrite`] sidecar, and return where it landed.
    ///
    /// This is what makes a write survive its upload: the caller is releasing a
    /// write handle and would otherwise delete the file, so until the bytes are
    /// on the remote, staging holds the only copy. Every dirty handle goes
    /// through here — the upload is a queued op performed later (offline.md
    /// Phase 3), and a staged file is also what a human can recover from if the
    /// queue never drains.
    ///
    /// Falls back to a copy when the rename crosses a filesystem boundary; a
    /// failure here means we could not save the bytes at all, so it is reported
    /// rather than swallowed.
    pub fn stage_write(&self, meta: &StagedWrite, scratch: &Path) -> Result<PathBuf> {
        let path = self.staging_path(meta);
        self.stage_write_at(meta, scratch, &path)
    }

    /// [`stage_write`](Self::stage_write) with the destination named by the
    /// caller.
    ///
    /// The public entry point picks a timestamped name, which is right for
    /// production and untestable: a test cannot arrange for publication to fail
    /// at a path it cannot predict. This is the seam `preserve_write_at` is for
    /// the rescue path.
    fn stage_write_at(&self, meta: &StagedWrite, scratch: &Path, path: &Path) -> Result<PathBuf> {
        use std::io::Write as _;

        let path = path.to_path_buf();
        let copied = if let Err(e) = std::fs::rename(scratch, &path) {
            if e.kind() != std::io::ErrorKind::CrossesDevices {
                return Err(e.into());
            }
            std::fs::copy(scratch, &path)?;
            true
        } else {
            false
        };
        std::fs::File::open(&path)?.sync_all()?;
        // Sidecar last: a staged file without one is still recoverable bytes,
        // while a sidecar without a file would describe nothing.
        let side = path.with_extension("json");
        let side_tmp = path.with_extension("json.tmp");
        let mut side_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&side_tmp)?;
        side_file.write_all(&serde_json::to_vec_pretty(meta)?)?;
        side_file.sync_all()?;
        drop(side_file);
        std::fs::rename(&side_tmp, &side)?;
        std::fs::File::open(&self.staging_dir)?.sync_all()?;
        if copied {
            std::fs::remove_file(scratch)?;
        }
        Ok(path)
    }

    /// Preserve accepted bytes which cannot be queued.
    ///
    /// Scratch and staging normally share a filesystem, so the normal path is
    /// an atomic rename. If that is impossible, the source first receives a
    /// durable recovery marker and remains in place until a copied destination,
    /// its sidecar, and the staging directory are all synced.
    pub fn preserve_write(
        &self,
        meta: &StagedWrite,
        scratch: &Path,
    ) -> std::result::Result<PathBuf, PreserveWriteError> {
        let path = self.staging_path(meta);
        self.preserve_write_at(meta, scratch, &path)
    }

    fn preserve_write_at(
        &self,
        meta: &StagedWrite,
        scratch: &Path,
        path: &Path,
    ) -> std::result::Result<PathBuf, PreserveWriteError> {
        use std::io::Write as _;

        let retained = |surviving_path: &Path, error: Error| {
            let adjacent = Self::scratch_sidecar(surviving_path);
            let source = Self::scratch_sidecar(scratch);
            let marker_path = [adjacent, source].into_iter().find(|path| path.is_file());
            PreserveWriteError {
                surviving_path: surviving_path.to_path_buf(),
                marker_path,
                error,
            }
        };
        // If rename cannot happen, this marker is what lets the next open move
        // the still-authoritative source into recovery instead of deleting it.
        let marker_result = self.mark_scratch_durable(scratch, meta);
        let side = path.with_extension("json");
        let side_tmp = path.with_extension("json.tmp");
        let publish_sidecar = || -> Result<()> {
            let mut side_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&side_tmp)?;
            side_file.write_all(&serde_json::to_vec_pretty(meta)?)?;
            side_file.sync_all()?;
            drop(side_file);
            std::fs::rename(&side_tmp, &side)?;
            std::fs::File::open(&self.staging_dir)?.sync_all()?;
            Ok(())
        };

        match std::fs::rename(scratch, path) {
            Ok(()) => {
                if let Err(error) = std::fs::File::open(path).and_then(|file| file.sync_all()) {
                    if std::fs::rename(path, scratch).is_ok() {
                        if !Self::scratch_sidecar(scratch).is_file() {
                            let _ = self.mark_scratch_durable(scratch, meta);
                        }
                        let _ =
                            std::fs::File::open(&self.scratch_dir).and_then(|dir| dir.sync_all());
                        return Err(retained(scratch, error.into()));
                    }
                    return Err(retained(path, error.into()));
                }
                let scratch_side = Self::scratch_sidecar(scratch);
                let side_result = if marker_result.is_ok() {
                    std::fs::rename(&scratch_side, &side)
                        .map_err(Error::from)
                        .and_then(|()| {
                            std::fs::File::open(&self.staging_dir)?
                                .sync_all()
                                .map_err(Error::from)
                        })
                } else {
                    publish_sidecar()
                };
                if let Err(error) = side_result {
                    let _ = std::fs::remove_file(&side_tmp);
                    // Publication did not complete. Put the blob back beside
                    // the source marker so startup recovery still has a
                    // reconstructable pair, rather than an unexplained staging
                    // blob and a marker whose source vanished.
                    if std::fs::rename(path, scratch).is_ok() {
                        if !Self::scratch_sidecar(scratch).is_file() {
                            let _ = self.mark_scratch_durable(scratch, meta);
                        }
                        let _ =
                            std::fs::File::open(&self.scratch_dir).and_then(|dir| dir.sync_all());
                        return Err(retained(scratch, error));
                    }
                    return Err(retained(path, error));
                }
                Ok(path.to_path_buf())
            }
            Err(rename_error) => {
                let source_marked =
                    marker_result.is_ok() || Self::scratch_sidecar(scratch).exists();
                if let Err(marker_error) = marker_result {
                    tracing::warn!(
                        error = %marker_error,
                        path = %scratch.display(),
                        "could not mark rescue source durable before copy fallback"
                    );
                }
                if let Err(error) = std::fs::copy(scratch, path) {
                    if !source_marked {
                        let recovery = self.recovery_dir.join(
                            path.file_name()
                                .unwrap_or_else(|| std::ffi::OsStr::new("denied-write")),
                        );
                        if std::fs::rename(scratch, &recovery).is_ok() {
                            let _ = std::fs::File::open(&self.recovery_dir)
                                .and_then(|dir| dir.sync_all());
                            // These bytes are unaccompanied, but re-arm the
                            // scan anyway: the flag exists to skip a directory
                            // nothing has written to, and something just did.
                            self.recovery_pending.store(true, Ordering::Relaxed);
                            return Err(retained(
                                &recovery,
                                Error::Other(format!(
                                    "rename failed ({rename_error}); copy fallback failed ({error})"
                                )),
                            ));
                        }
                    }
                    return Err(retained(
                        scratch,
                        Error::Other(format!(
                            "rename failed ({rename_error}); copy fallback failed ({error})"
                        )),
                    ));
                }
                if let Err(error) = std::fs::File::open(path).and_then(|file| file.sync_all()) {
                    let surviving = if source_marked { scratch } else { path };
                    return Err(retained(surviving, error.into()));
                }
                if let Err(error) = publish_sidecar() {
                    let _ = std::fs::remove_file(&side_tmp);
                    let surviving = if source_marked { scratch } else { path };
                    return Err(retained(surviving, error));
                }
                // Deliberately last: fallback never removes its marked source
                // until destination publication is complete.
                if let Err(error) = std::fs::remove_file(scratch) {
                    return Err(retained(path, error.into()));
                }
                self.clear_scratch_durable(scratch);
                Ok(path.to_path_buf())
            }
        }
    }

    /// Drop a staged blob and its sidecar, once its bytes are safely on the
    /// remote (or the op that owned them has been superseded). Best effort:
    /// leftovers cost disk, while failing here would strand a drained op.
    pub fn discard_staged(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("json"));
    }

    /// Remove any cached blob + meta for `uid`, including its thumbnails (best
    /// effort; absence is fine). Thumbnails are tied to the revision, so a
    /// content eviction must drop them too.
    pub fn evict(&self, uid: &NodeUid) {
        let _ = std::fs::remove_file(self.blob_path(uid));
        let _ = std::fs::remove_file(self.meta_path(uid));
        for ttype in [1, 2] {
            let _ = std::fs::remove_file(self.thumb_blob(uid, ttype));
            let _ = std::fs::remove_file(self.thumb_meta(uid, ttype));
        }
        // Drop every cached block (and its meta/tmp) for this uid, plus the
        // recorded block geometry — it describes the revision these blocks came
        // from, and keeping it past them would have the next read plan against a
        // table for a file it no longer holds. The block dir is still scanned
        // here because eviction targets one uid's files by name prefix, not by
        // LRU order; the index rows are removed in one shot by
        // `cache_remove_all` below.
        let _ = std::fs::remove_file(self.geometry_meta(uid));
        let prefix = format!("{}.b", Self::key(uid));
        if let Ok(rd) = std::fs::read_dir(&self.block_dir) {
            for entry in rd.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix))
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        // Forget the blob and all of this uid's block rows in the LRU index.
        let _ = self.db.cache_remove_all(&Self::key(uid));
        // Rows just left the index; the running totals must follow.
        self.reseed_totals();
    }

    /// Cache-index key for the `ttype` thumbnail of `uid` — the thumbnail file's
    /// name, so eviction can remove the file by joining it onto `thumb_dir`.
    fn thumb_key(&self, uid: &NodeUid, ttype: i32) -> String {
        format!("{}.t{ttype}", Self::key(uid))
    }

    fn thumb_blob(&self, uid: &NodeUid, ttype: i32) -> PathBuf {
        self.thumb_dir.join(self.thumb_key(uid, ttype))
    }

    fn thumb_meta(&self, uid: &NodeUid, ttype: i32) -> PathBuf {
        self.thumb_dir
            .join(format!("{}.t{ttype}.meta", Self::key(uid)))
    }

    /// Owner-protected transient directory used to stage decrypted camera RAWs
    /// for metadata helpers. Callers still remove individual files promptly.
    pub fn raw_thumbnail_staging_dir(&self) -> &Path {
        &self.raw_thumb_dir
    }

    /// Serve a cached thumbnail of `ttype` for `uid`, or `None` on miss/stale.
    /// Validated by node `mtime` alone: a new revision (which is what changes the
    /// thumbnail) bumps the mtime, so a stale thumbnail is detected. Thumbnails
    /// are small, so the whole blob is read at once.
    pub fn read_thumbnail(&self, uid: &NodeUid, ttype: i32, mtime: i64) -> Option<Vec<u8>> {
        let recorded: i64 =
            serde_json::from_slice(&std::fs::read(self.thumb_meta(uid, ttype)).ok()?).ok()?;
        if recorded != mtime {
            return None;
        }
        let bytes = std::fs::read(self.thumb_blob(uid, ttype)).ok()?;
        // Record the access for LRU, as the blob and block readers do. Without
        // it the pool evicts in insertion order, so scrolling back up a gallery
        // would drop exactly the tiles being looked at.
        self.touch(self.thumb_key(uid, ttype));
        Some(bytes)
    }

    /// On-disk path of the cached content blob for `uid`, validated against
    /// `(mtime, size)`; `None` on miss/stale. Lets a front-end open the blob
    /// directly instead of streaming its bytes back over the control socket.
    pub fn cached_content_path(&self, uid: &NodeUid, mtime: i64, size: u64) -> Option<PathBuf> {
        self.valid_blob(uid, mtime, size)
    }

    /// On-disk path where `uid`'s content blob lives once stored (the file may
    /// not exist yet, or may be stale — pair with [`store`](Self::store)).
    pub fn content_path(&self, uid: &NodeUid) -> PathBuf {
        self.blob_path(uid)
    }

    /// On-disk path of the cached `ttype` thumbnail for `uid`, fresh against
    /// `tag` (the validity tag last passed to [`store_thumbnail`](Self::store_thumbnail));
    /// `None` on miss/stale. For Drive files the tag is the node mtime; for
    /// photos it is the capture time.
    pub fn cached_thumbnail_path(&self, uid: &NodeUid, ttype: i32, tag: i64) -> Option<PathBuf> {
        let recorded: i64 =
            serde_json::from_slice(&std::fs::read(self.thumb_meta(uid, ttype)).ok()?).ok()?;
        if recorded != tag {
            return None;
        }
        let blob = self.thumb_blob(uid, ttype);
        if !blob.exists() {
            return None;
        }
        self.touch(self.thumb_key(uid, ttype));
        Some(blob)
    }

    /// On-disk path where `uid`'s `ttype` thumbnail lives once stored.
    pub fn thumbnail_path(&self, uid: &NodeUid, ttype: i32) -> PathBuf {
        self.thumb_blob(uid, ttype)
    }

    /// Cache `bytes` as the `ttype` thumbnail for `uid`, tagged with `mtime`.
    /// Blob written to a temp file then renamed; the meta tag is written last so
    /// a crash mid-store fails validation rather than serving a torn thumbnail.
    ///
    /// The temp name carries `ttype`, and must. `Path::with_extension` would
    /// *replace* the `.t{ttype}` suffix rather than append to it, giving every
    /// thumbnail type of one node the same staging file — so a type-1 and a
    /// type-2 store racing (the gallery caches type 1 from a control-socket
    /// thread while `getxattr` caches type 2 on the FUSE dispatch loop) would
    /// publish one type's bytes under the other's name. See
    /// `concurrent_thumbnail_types_do_not_share_a_temp_file`.
    pub fn store_thumbnail(
        &self,
        uid: &NodeUid,
        ttype: i32,
        mtime: i64,
        bytes: &[u8],
    ) -> Result<()> {
        let blob = self.thumb_blob(uid, ttype);
        let tmp = self
            .thumb_dir
            .join(format!("{}.t{ttype}.tmp", Self::key(uid)));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &blob)?;
        std::fs::write(self.thumb_meta(uid, ttype), serde_json::to_vec(&mtime)?)?;
        self.db.cache_touch(
            &self.thumb_key(uid, ttype),
            KIND_THUMB,
            bytes.len() as u64,
            now_millis(),
        )?;
        self.thumb_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.enforce_thumb_budget();
        Ok(())
    }

    /// Evict least-recently-used thumbnails until the pool fits its share of the
    /// budget. Every thumbnail is evictable, including those of pinned nodes: a
    /// thumbnail is derived data that the gallery re-fetches or regenerates, and
    /// exempting them would mean a large pinned library could hold the pool
    /// permanently over its ceiling — the exact unbounded growth C6 was about.
    fn enforce_thumb_budget(&self) {
        self.enforce_pool(KIND_THUMB, &self.thumb_dir, None);
    }

    /// Evict least-recently-used *unpinned* blobs until total blob bytes fit the
    /// configured `max_bytes` cap. No-op when the cap is disabled (`0`) or the
    /// cache already fits. Pinned blobs are skipped, so a cache held entirely by
    /// pins can legitimately stay over budget.
    fn enforce_budget(&self) {
        let cap = self.pool_cap(KIND_BLOB);
        if cap == 0 || self.blob_bytes.load(Ordering::Relaxed) <= cap {
            // The common case, settled without a query — and in particular
            // without the recursive pin CTE below, which used to run on every
            // single store.
            return;
        }
        // Pinned blobs (by cache key) are exempt from eviction. Resolves direct
        // pins plus every descendant of a recursively-pinned folder, hashed into
        // cache keys to match on-disk filenames.
        let pinned: HashSet<String> = self
            .db
            .pinned_uids()
            .unwrap_or_default()
            .iter()
            .map(|uid| Self::key_str(uid))
            .collect();
        let content_dir = self.content_dir.clone();
        self.enforce_pool(KIND_BLOB, &content_dir, Some(&pinned));
    }

    /// Whether `uid` is pinned — directly, or because an ancestor folder is
    /// pinned recursively (resolved in the DB against the node tree). Independent
    /// of whether its blob is currently cached. Best effort: a DB error reads as
    /// "not pinned" rather than failing the calling FUSE op.
    pub fn is_pinned(&self, uid: &NodeUid) -> bool {
        self.db.is_pinned(&uid.to_string()).unwrap_or(false)
    }

    /// Record `uid` as pinned under `path`. `recursive` marks a folder pin whose
    /// whole subtree is kept. The caller is responsible for having cached the
    /// content (a file via [`store`](Self::store); a folder's descendants by the
    /// daemon walking the subtree).
    pub fn add_pin(&self, uid: &NodeUid, path: &Path, recursive: bool) -> Result<()> {
        self.db
            .pin_add(&uid.to_string(), &path.display().to_string(), recursive)
    }

    /// Drop `uid` from the pin registry and evict its blob. No-op if not pinned.
    /// A recursively-pinned folder's descendants are evicted by the daemon
    /// (`Core::unpin`), which knows the node tree.
    pub fn remove_pin(&self, uid: &NodeUid) -> Result<()> {
        self.db.pin_remove(&uid.to_string())?;
        self.evict(uid);
        Ok(())
    }

    /// All pins (files and recursive folders), ordered by uid.
    pub fn list_pins(&self) -> Vec<Pin> {
        self.db
            .pin_list()
            .unwrap_or_default()
            .into_iter()
            .map(|row| Pin {
                uid: row.uid,
                path: row.path,
                recursive: row.recursive,
                is_dir: row.is_dir,
            })
            .collect()
    }

    /// Total bytes the cache holds on disk — blobs, blocks and thumbnails
    /// together — which is what the configured budget caps and therefore the
    /// only honest thing to show next to it.
    ///
    /// This used to scan `content_dir` alone, which excluded both the block dir
    /// and the thumb dir: the Settings page could report a few hundred megabytes
    /// while the cache held several gigabytes. Now it reads the same running
    /// totals the budget enforcers use, so the displayed number and the enforced
    /// number cannot disagree — and it costs three atomic loads instead of a
    /// directory scan.
    ///
    /// Excludes the `.meta` sidecars (a few dozen bytes each) and the scratch
    /// and staging dirs, which hold unuploaded user writes rather than cache and
    /// are deliberately not evictable.
    pub fn usage(&self) -> u64 {
        [KIND_BLOB, KIND_BLOCK, KIND_THUMB]
            .iter()
            .map(|k| self.pool(k).load(Ordering::Relaxed))
            .sum()
    }

    /// Bytes and file count held in `staging/`, with the age of the oldest
    /// entry in seconds.
    ///
    /// Staging is not part of [`usage`](Self::usage) and is not subject to the
    /// budget — it holds the *only* copy of writes that have not been uploaded,
    /// so nothing here may ever be evicted to reclaim space. That is precisely
    /// why it has to be reportable: a parked transient create (three abandoned
    /// 4 GiB downloads, say) otherwise occupies disk with nothing in any UI
    /// accounting for it.
    ///
    /// A `read_dir` per call, which is fine for the callers it has: a status
    /// request, polled every couple of seconds against a directory that is
    /// empty whenever the queue is drained.
    pub fn staging_usage(&self) -> StagingUsage {
        let Ok(entries) = std::fs::read_dir(&self.staging_dir) else {
            return StagingUsage::default();
        };
        let mut usage = StagingUsage::default();
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            // Sidecars describe the blob beside them; counting both would double
            // the file count and report metadata as user bytes.
            if entry.path().extension().is_some_and(|e| e == "json") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            usage.files += 1;
            usage.bytes += meta.len();
            if let Ok(age) = meta.modified().and_then(|m| {
                now.duration_since(m)
                    .map_err(|_| std::io::Error::other("modified in the future"))
            }) {
                usage.oldest_secs = usage.oldest_secs.max(age.as_secs());
            }
        }
        usage
    }

    /// Configured soft byte cap (`0` = unlimited), for display alongside
    /// [`usage`](Self::usage). This is the whole-cache figure; internally it is
    /// divided between the pools (see [`SPLIT_BLOB`]), but the user set one
    /// number and gets that number.
    pub fn budget(&self) -> u64 {
        self.cap()
    }

    /// The current soft byte cap. A plain atomic load; the value can change
    /// under [`set_budget`](Self::set_budget) while the daemon runs.
    fn cap(&self) -> u64 {
        self.max_bytes.load(Ordering::Relaxed)
    }

    /// Retune the soft byte cap (`0` = unlimited) at runtime and immediately
    /// enforce it: blobs and blocks are LRU-evicted back under the new cap (a
    /// lower cap frees space now; a higher one is a no-op until the next store).
    /// Called by the daemon when the Settings page changes the cache budget.
    pub fn set_budget(&self, bytes: u64) {
        self.max_bytes.store(bytes, Ordering::Relaxed);
        self.enforce_budget();
        self.enforce_block_budget();
        self.enforce_thumb_budget();
    }

    /// Delete every *unpinned* cached blob plus all on-demand block chunks,
    /// keeping pinned files intact. Pinned files keep their whole-file blobs
    /// (and are never served from blocks), so dropping all blocks is safe.
    /// Returns the number of bytes freed, for a user-facing confirmation.
    pub fn clear_unpinned(&self) -> u64 {
        // Pinned blobs are exempt; resolve them to cache keys to match filenames.
        let pinned: HashSet<String> = self
            .db
            .pinned_uids()
            .unwrap_or_default()
            .iter()
            .map(|uid| Self::key_str(uid))
            .collect();
        let mut freed = 0u64;
        // Unpinned whole-file blobs.
        if let Ok(entries) = self.db.cache_entries_by_kind(KIND_BLOB) {
            for (key, size) in entries {
                if pinned.contains(&key) {
                    continue;
                }
                let _ = std::fs::remove_file(self.content_dir.join(&key));
                let _ = std::fs::remove_file(self.content_dir.join(format!("{key}.meta")));
                let _ = self.db.cache_remove(&key);
                freed = freed.saturating_add(size);
            }
        }
        // Every on-demand block (transient partial reads, re-fetched on demand)
        // and every thumbnail (derived, re-fetched or regenerated on demand).
        // Both pools are dropped wholesale rather than filtered by pin: neither
        // is what "keep this available offline" means, and leaving them behind
        // would make "clear cache" free visibly less than it reported.
        for (kind, dir) in [(KIND_BLOCK, &self.block_dir), (KIND_THUMB, &self.thumb_dir)] {
            if let Ok(entries) = self.db.cache_entries_by_kind(kind) {
                for (key, size) in entries {
                    let _ = std::fs::remove_file(dir.join(&key));
                    let _ = std::fs::remove_file(dir.join(format!("{key}.meta")));
                    let _ = self.db.cache_remove(&key);
                    freed = freed.saturating_add(size);
                }
            }
        }
        self.reseed_totals();
        freed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    /// A unique temp directory removed on drop; avoids a dev-dependency.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-cache-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cache() -> (ContentCache, TempDir) {
        cache_capped(0)
    }

    /// A cache whose `kind` pool holds exactly `bytes`.
    ///
    /// The configured budget is a whole-cache figure divided between the pools
    /// (C7), while a test that exercises eviction cares about one pool at a
    /// time. So the test states the pool size it means and this inverts the
    /// split, rather than every such test hard-coding a number that silently
    /// changes meaning if the split is ever retuned.
    fn cache_with_pool_cap(kind: &str, bytes: u64) -> (ContentCache, TempDir) {
        let share = match kind {
            KIND_BLOCK => SPLIT_BLOCK,
            KIND_THUMB => SPLIT_THUMB,
            _ => SPLIT_BLOB,
        };
        // Round up, so the pool holds at least `bytes` rather than one short.
        let (c, d) = cache_capped((bytes * 100).div_ceil(share));
        assert!(
            c.pool_cap(kind) >= bytes,
            "helper must give the {kind} pool at least {bytes} bytes"
        );
        (c, d)
    }

    fn cache_capped(max_bytes: u64) -> (ContentCache, TempDir) {
        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let c = ContentCache::open(
            dir.path().join("content"),
            dir.path().join("pins.json"),
            max_bytes,
            db,
        )
        .unwrap();
        (c, dir)
    }

    #[test]
    fn store_then_read_range() {
        let (c, _d) = cache();
        let u = uid("a");
        let data = b"hello world";
        c.store(&u, 100, data.len() as u64, data).unwrap();
        assert!(c.is_cached(&u, 100, data.len() as u64));
        assert_eq!(
            c.read_range(&u, 100, data.len() as u64, 6, 5).unwrap(),
            b"world"
        );
        // Offset past EOF yields an empty slice, not a miss.
        assert_eq!(
            c.read_range(&u, 100, data.len() as u64, 100, 5).unwrap(),
            b""
        );
    }

    /// The drain promotes a staged upload into the cache by path and then
    /// unlinks the staging name. The cached copy has to survive that, and the
    /// promote has to charge the LRU index like [`ContentCache::store`] does.
    #[test]
    fn store_file_adopts_blob_and_survives_source_unlink() {
        let (c, d) = cache();
        let u = uid("a");
        let data = b"staged contents";
        let src = d.path().join("staged-blob");
        std::fs::write(&src, data).unwrap();

        c.store_file(&u, 100, data.len() as u64, &src).unwrap();
        std::fs::remove_file(&src).unwrap();

        assert!(c.is_cached(&u, 100, data.len() as u64));
        assert_eq!(
            c.read_range(&u, 100, data.len() as u64, 7, 8).unwrap(),
            b"contents"
        );
        assert_eq!(c.usage(), data.len() as u64);
    }

    /// A cross-filesystem staging dir cannot be hardlinked from; the copy
    /// blob and its metadata must both be replaced wholesale — a stale `.meta`
    /// left beside new content would validate a read of the wrong bytes. The
    /// link goes to a temp name and is renamed over the blob for exactly this,
    /// so re-storing over an existing entry is the case worth pinning.
    #[test]
    fn store_file_replaces_an_existing_blob() {
        let (c, d) = cache();
        let u = uid("a");
        c.store(&u, 100, 3, b"old").unwrap();

        let src = d.path().join("new-blob");
        std::fs::write(&src, b"newer").unwrap();
        c.store_file(&u, 200, 5, &src).unwrap();

        assert!(!c.is_cached(&u, 100, 3), "stale meta must not validate");
        assert_eq!(c.read_range(&u, 200, 5, 0, 5).unwrap(), b"newer");
    }

    #[test]
    fn reconcile_rebuilds_index_from_disk() {
        let dir = TempDir::new();
        let content = dir.path().join("content");
        let pins = dir.path().join("pins.json");

        // First session: unlimited cap, store three blobs onto disk.
        let db1 = Arc::new(Db::open_in_memory().unwrap());
        {
            let c = ContentCache::open(content.clone(), pins.clone(), 0, db1).unwrap();
            c.store(&uid("a"), 1, 4, b"aaaa").unwrap();
            c.store(&uid("b"), 1, 4, b"bbbb").unwrap();
            c.store(&uid("c"), 1, 4, b"cccc").unwrap();
        }

        // Second session with a *fresh* index (as if predating P4, or after a
        // crash) but the same on-disk cache: reconcile repopulates from disk.
        let db2 = Arc::new(Db::open_in_memory().unwrap());
        assert!(db2.cache_entries_by_kind("blob").unwrap().is_empty());
        let c = ContentCache::open(content, pins, 8, db2.clone()).unwrap();
        assert_eq!(db2.cache_entries_by_kind("blob").unwrap().len(), 3);

        // With the index rebuilt, a further store enforces the budget against the
        // reconciled entries instead of leaking them.
        c.store(&uid("d"), 1, 4, b"dddd").unwrap();
        let total: u64 = db2
            .cache_entries_by_kind("blob")
            .unwrap()
            .iter()
            .map(|(_, s)| s)
            .sum();
        assert!(total <= 8, "budget enforced after reconcile, total={total}");
    }

    #[test]
    fn stale_metadata_is_a_miss() {
        let (c, _d) = cache();
        let u = uid("a");
        c.store(&u, 100, 3, b"abc").unwrap();
        // A newer mtime (new revision) invalidates the cached blob.
        assert!(!c.is_cached(&u, 101, 3));
        assert!(c.read_range(&u, 101, 3, 0, 3).is_none());
        // A size mismatch also invalidates it.
        assert!(c.read_range(&u, 100, 4, 0, 3).is_none());
    }

    #[test]
    fn legacy_pins_json_imported_then_removed() {
        let dir = TempDir::new();
        let pins = dir.path().join("pins.json");
        // Hand-write a pre-P5 pins.json (file pins only, no `recursive` field).
        std::fs::write(
            &pins,
            r#"{"pins":{"vol~a":{"uid":"vol~a","path":"docs/a.txt"}}}"#,
        )
        .unwrap();

        let db = Arc::new(Db::open_in_memory().unwrap());
        let c = ContentCache::open(dir.path().join("content"), pins.clone(), 0, db).unwrap();

        // Imported into the DB as a non-recursive pin...
        assert!(c.is_pinned(&uid("a")));
        let listed = c.list_pins();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "docs/a.txt");
        assert!(!listed[0].recursive);
        // ...and the legacy file is gone so it never re-imports.
        assert!(!pins.exists());
    }

    #[test]
    fn pin_lifecycle_evicts_on_unpin() {
        let (c, _d) = cache();
        let u = uid("a");
        c.store(&u, 100, 3, b"abc").unwrap();
        c.add_pin(&u, Path::new("docs/a.txt"), false).unwrap();
        assert!(c.is_pinned(&u));
        assert_eq!(c.list_pins().len(), 1);
        assert_eq!(c.list_pins()[0].path, "docs/a.txt");

        c.remove_pin(&u).unwrap();
        assert!(!c.is_pinned(&u));
        assert!(c.list_pins().is_empty());
        // Unpin evicts the cached content.
        assert!(!c.is_cached(&u, 100, 3));
    }

    /// The under-budget path — which is every store until the cache fills — must
    /// not touch the database at all. It is on the cold-read hot path: one call
    /// per cached 4 MiB block, under the connection lock every FUSE metadata
    /// call also needs.
    #[test]
    fn budget_check_is_free_when_under_budget() {
        use std::time::Instant;

        // Cap far above what we store, so every store stays under budget.
        let (c, _d) = cache_capped(1 << 30);
        let payload = vec![0u8; 4096];

        // Cost of a store against a near-empty index...
        let t = Instant::now();
        for i in 0..500u64 {
            c.store_block(&uid("a"), 1, 1 << 30, blk(1 << 30, i), &payload)
                .unwrap();
        }
        let empty = t.elapsed() / 500;

        // ...and against one with a few thousand entries in it.
        for i in 500..2500u64 {
            c.store_block(&uid("a"), 1, 1 << 30, blk(1 << 30, i), &payload)
                .unwrap();
        }
        let t = Instant::now();
        for i in 2500..3000u64 {
            c.store_block(&uid("a"), 1, 1 << 30, blk(1 << 30, i), &payload)
                .unwrap();
        }
        let full = t.elapsed() / 500;

        println!("B4: store_block under budget — {empty:?} at ~0 entries, {full:?} at 2500");
        // A ratio, not a wall-clock bound: the claim is that the under-budget
        // path does not grow with the index, and only a ratio tests that. An
        // absolute threshold measures the machine's load instead, and this test
        // runs alongside the rest of the suite.
        assert!(
            full < empty * 3,
            "under-budget store should not scale with the index: \
             {empty:?} at ~0 entries vs {full:?} at 2500"
        );
    }

    /// The running totals are a fast path, not a second source of truth: after
    /// an eviction pass they are realigned with the index, so repeated
    /// over-budget stores cannot drift them.
    #[test]
    fn running_totals_stay_aligned_with_the_index() {
        let (c, _d) = cache_with_pool_cap(KIND_BLOCK, 8);
        for i in 0..12u64 {
            c.store_block(&uid("a"), 1, 100, blk(100, i), b"aaaa")
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let indexed: u64 =
            c.db.cache_entries_by_kind("block")
                .unwrap()
                .iter()
                .map(|(_, s)| s)
                .sum();
        assert_eq!(
            c.block_bytes.load(Ordering::Relaxed),
            indexed,
            "running total matches the index after repeated eviction"
        );
        assert!(indexed <= 8, "and the cap is actually held: {indexed}");
    }

    #[test]
    fn budget_evicts_lru_unpinned() {
        // Blob pool fits two 4-byte blobs but not three.
        let (c, _d) = cache_with_pool_cap(KIND_BLOB, 8);
        let (a, b, d) = (uid("a"), uid("b"), uid("d"));

        c.store(&a, 1, 4, b"aaaa").unwrap();
        // Distinct mtimes so LRU order is deterministic (oldest = a).
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store(&b, 1, 4, b"bbbb").unwrap();
        // Touch `a` so `b` becomes the least-recently-used.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(c.read_range(&a, 1, 4, 0, 4).is_some());

        // Third store pushes over budget → evicts LRU unpinned (`b`).
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store(&d, 1, 4, b"dddd").unwrap();
        assert!(c.is_cached(&a, 1, 4), "recently-read blob survives");
        assert!(!c.is_cached(&b, 1, 4), "LRU blob evicted");
        assert!(c.is_cached(&d, 1, 4), "newest blob survives");
    }

    #[test]
    fn budget_never_evicts_pinned() {
        let (c, _d) = cache_with_pool_cap(KIND_BLOB, 8);
        let (a, b, d) = (uid("a"), uid("b"), uid("d"));

        c.store(&a, 1, 4, b"aaaa").unwrap();
        c.add_pin(&a, Path::new("a"), false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store(&b, 1, 4, b"bbbb").unwrap();
        // Over budget: `a` is pinned (oldest) so `b` must be the victim.
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store(&d, 1, 4, b"dddd").unwrap();
        assert!(c.is_cached(&a, 1, 4), "pinned blob never evicted");
        assert!(!c.is_cached(&b, 1, 4), "unpinned LRU blob evicted");
        assert!(c.is_cached(&d, 1, 4));
    }

    #[test]
    fn clear_unpinned_keeps_pinned() {
        let (c, _d) = cache();
        let (a, b) = (uid("a"), uid("b"));
        c.store(&a, 1, 4, b"aaaa").unwrap();
        c.add_pin(&a, Path::new("a"), false).unwrap();
        c.store(&b, 1, 4, b"bbbb").unwrap();
        // A stray on-demand block of the unpinned file is purged too.
        c.store_block(&b, 1, 8, blk(8, 0), b"bbbb").unwrap();

        let freed = c.clear_unpinned();
        assert_eq!(freed, 8, "one 4-byte blob + one 4-byte block freed");
        assert!(c.is_cached(&a, 1, 4), "pinned blob survives purge");
        assert!(!c.is_cached(&b, 1, 4), "unpinned blob purged");
        assert!(
            c.cached_block(&b, 1, 8, blk(8, 0)).is_none(),
            "block purged"
        );
    }

    #[test]
    fn set_budget_evicts_immediately() {
        // Start unlimited so three blobs all fit, then tighten the cap.
        let (c, _d) = cache();
        let (a, b, d) = (uid("a"), uid("b"), uid("d"));
        c.store(&a, 1, 4, b"aaaa").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store(&b, 1, 4, b"bbbb").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store(&d, 1, 4, b"dddd").unwrap();

        // Tightening to a budget whose blob share is 8 bytes evicts the LRU
        // unpinned blob(s) now, not on the next store.
        c.set_budget((8 * 100u64).div_ceil(SPLIT_BLOB));
        assert!(!c.is_cached(&a, 1, 4), "oldest blob evicted on tighten");
        assert!(c.is_cached(&b, 1, 4));
        assert!(c.is_cached(&d, 1, 4));
        // `budget()` reports the whole-cache figure that was set, not one
        // pool's share of it.
        assert_eq!(c.budget(), (8 * 100u64).div_ceil(SPLIT_BLOB));
    }

    /// **The A2 reproduce.** Thumbnail types 1 and 2 of one node must not share
    /// a staging file. They are stored from different subsystems — the Photos
    /// gallery caches type 1 from a control-socket thread while `getxattr`
    /// caches type 2 on the FUSE dispatch loop — so the two genuinely overlap.
    ///
    /// The failure is silent: the loser's rename either publishes the *other*
    /// type's bytes under this type's name, or vanishes with ENOENT. Both are
    /// reported here, because a fix that only removed the error would leave the
    /// corruption.
    #[test]
    fn concurrent_thumbnail_types_do_not_share_a_temp_file() {
        use std::sync::Arc as StdArc;

        let (c, _d) = cache();
        let c = StdArc::new(c);
        let u = uid("a");
        // Big enough that the write is not a single atomic-looking syscall, and
        // distinguishable by content and length.
        let one = vec![0xA1u8; 96 * 1024];
        let two = vec![0xB2u8; 64 * 1024];

        let mut handles = Vec::new();
        for (ttype, payload) in [(1i32, one.clone()), (2i32, two.clone())] {
            let c = c.clone();
            let u = u.clone();
            handles.push(std::thread::spawn(move || {
                let mut errors = 0usize;
                for _ in 0..400 {
                    if c.store_thumbnail(&u, ttype, 100, &payload).is_err() {
                        errors += 1;
                    }
                }
                errors
            }));
        }

        let mut store_errors = 0usize;
        for h in handles {
            store_errors += h.join().expect("store thread panicked");
        }

        // Whatever is on disk for each type must be that type's bytes.
        if let Some(got) = c.read_thumbnail(&u, 1, 100) {
            assert_eq!(got, one, "type 1 served type 2's bytes");
        }
        if let Some(got) = c.read_thumbnail(&u, 2, 100) {
            assert_eq!(got, two, "type 2 served type 1's bytes");
        }
        assert_eq!(
            store_errors, 0,
            "a store lost its staging file to the other type"
        );
    }

    #[test]
    fn thumbnail_store_read_and_evict() {
        let (c, _d) = cache();
        let u = uid("a");
        c.store_thumbnail(&u, 1, 100, b"thumb1").unwrap();
        c.store_thumbnail(&u, 2, 100, b"preview2").unwrap();
        assert_eq!(c.read_thumbnail(&u, 1, 100).unwrap(), b"thumb1");
        assert_eq!(c.read_thumbnail(&u, 2, 100).unwrap(), b"preview2");
        // A newer revision (mtime bump) invalidates the cached thumbnail.
        assert!(c.read_thumbnail(&u, 1, 101).is_none());
        // Content eviction drops thumbnails too.
        c.evict(&u);
        assert!(c.read_thumbnail(&u, 1, 100).is_none());
        assert!(c.read_thumbnail(&u, 2, 100).is_none());
    }

    /// Total bytes of every real file under `root`, recursively — what `du`
    /// would report, and what `usage()` claims to be reporting.
    fn bytes_on_disk(root: &Path) -> u64 {
        let Ok(rd) = std::fs::read_dir(root) else {
            return 0;
        };
        let mut total = 0;
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += bytes_on_disk(&entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
        total
    }

    /// C6/C7. Two claims the cache makes and does not keep.
    ///
    /// The configured budget is the whole promise of the Settings page: it is
    /// the only thing standing between an on-demand filesystem and a full disk.
    /// A thumbnail per photo, on a library of tens of thousands, is not a
    /// rounding error.
    #[test]
    fn every_pool_is_budgeted_and_visible() {
        const CAP: u64 = 256 * 1024;
        let (c, dir) = cache_capped(CAP);
        let payload = vec![0u8; 8 * 1024];

        // A gallery scroll: one thumbnail per photo, far past the cap.
        for i in 0..200u64 {
            c.store_thumbnail(&uid(&format!("photo{i}")), 1, 1, &payload)
                .unwrap();
        }
        // Plus enough blob and block traffic to fill both other pools.
        for i in 0..100u64 {
            c.store(&uid(&format!("file{i}")), 1, payload.len() as u64, &payload)
                .unwrap();
            c.store_block(&uid("streamed"), 1, 1 << 30, blk(1 << 30, i), &payload)
                .unwrap();
        }

        let on_disk = bytes_on_disk(dir.path());
        let reported = c.usage();

        // 1. The cap is a cap on the cache, not on one pool of it.
        assert!(
            on_disk <= CAP + CAP / 8,
            "cache holds {on_disk} bytes against a {CAP}-byte budget"
        );
        // 2. And the number shown to the user is the number on disk. Meta
        //    sidecars are small and untracked, hence the tolerance.
        let drift = on_disk.abs_diff(reported);
        assert!(
            drift <= on_disk / 8,
            "usage() reports {reported} against {on_disk} on disk"
        );
    }

    /// The thumbnail pool must evict by *use*, not by insertion order. A gallery
    /// scrolled down and back up re-reads the tiles it already has; under FIFO
    /// those are exactly the ones dropped, so every scroll back would re-fetch.
    #[test]
    fn thumbnails_evict_least_recently_used() {
        // Pool fits two 4-byte thumbnails but not three.
        let (c, _d) = cache_with_pool_cap(KIND_THUMB, 8);
        let (a, b, d) = (uid("a"), uid("b"), uid("d"));

        c.store_thumbnail(&a, 1, 1, b"aaaa").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store_thumbnail(&b, 1, 1, b"bbbb").unwrap();
        // Look at `a` again, making `b` the least-recently-used.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(c.read_thumbnail(&a, 1, 1).is_some());
        std::thread::sleep(std::time::Duration::from_millis(10));

        c.store_thumbnail(&d, 1, 1, b"dddd").unwrap();
        assert!(c.read_thumbnail(&a, 1, 1).is_some(), "recently read, kept");
        assert!(c.read_thumbnail(&b, 1, 1).is_none(), "least recently used");
        assert!(c.read_thumbnail(&d, 1, 1).is_some(), "just stored");
    }

    /// Evicting a node drops its thumbnails from the index as well as from disk.
    /// A row left behind would keep counting bytes that are gone, and the
    /// running total would drift above the truth until the next restart.
    #[test]
    fn evicting_a_node_clears_its_thumbnail_accounting() {
        let (c, _d) = cache();
        let u = uid("a");
        c.store(&u, 1, 4, b"aaaa").unwrap();
        c.store_thumbnail(&u, 1, 1, b"thumb1").unwrap();
        c.store_thumbnail(&u, 2, 1, b"preview2").unwrap();
        assert!(c.thumb_bytes.load(Ordering::Relaxed) > 0);

        c.evict(&u);
        assert_eq!(c.thumb_bytes.load(Ordering::Relaxed), 0);
        assert!(c.db.cache_entries_by_kind(KIND_THUMB).unwrap().is_empty());
        assert_eq!(c.usage(), 0, "nothing cached, nothing reported");
    }

    #[test]
    fn evict_removes_blob() {
        let (c, _d) = cache();
        let u = uid("a");
        c.store(&u, 1, 3, b"abc").unwrap();
        c.evict(&u);
        assert!(!c.is_cached(&u, 1, 3));
    }

    #[test]
    fn block_store_then_read() {
        let (c, _d) = cache();
        let u = uid("a");
        // A 10-byte file is one block of 10 bytes: the length a reader expects
        // is derived from the file size, so the fixture has to be geometrically
        // real or the torn-write check rejects it.
        c.store_block(&u, 100, 10, blk(10, 0), b"block-zero")
            .unwrap();
        assert_eq!(
            c.cached_block(&u, 100, 10, blk(10, 0)).unwrap(),
            b"block-zero"
        );
        // A different index is a separate cache entry, absent here.
        assert!(c.cached_block(&u, 100, 10, blk(10, 1)).is_none());
        // A new revision (mtime/size bump) invalidates the block.
        assert!(c.cached_block(&u, 101, 10, blk(10, 0)).is_none());
        assert!(c.cached_block(&u, 100, 5000, blk(5000, 0)).is_none());
    }

    /// The [`BlockSpan`] the uniform geometry gives block `idx` of a file of
    /// `size` bytes — the shape these tests were written against, and the one
    /// this client's own uploads still have.
    fn blk(size: u64, idx: u64) -> BlockSpan {
        BlockSpan {
            idx,
            start: idx.saturating_mul(BLOCK_SIZE),
            len: block_len(size, idx),
        }
    }

    /// The geometry the whole read path validates against, here and over the
    /// network (bugs.md B84): full blocks, then whatever is left.
    #[test]
    fn block_len_is_full_blocks_then_the_remainder() {
        let size = BLOCK_SIZE * 2 + 17;
        assert_eq!(block_len(size, 0), BLOCK_SIZE);
        assert_eq!(block_len(size, 1), BLOCK_SIZE);
        assert_eq!(block_len(size, 2), 17);
        // Past the end there is no block, not a short one.
        assert_eq!(block_len(size, 3), 0);
        // An exact multiple has no short tail at all.
        assert_eq!(block_len(BLOCK_SIZE, 0), BLOCK_SIZE);
        assert_eq!(block_len(BLOCK_SIZE, 1), 0);
    }

    /// The uniform geometry has to reproduce the constant-block arithmetic it
    /// replaces exactly, or every file this client uploaded changes shape.
    #[test]
    fn the_uniform_geometry_is_the_old_block_arithmetic() {
        let size = BLOCK_SIZE * 2 + 17;
        let g = BlockGeometry::uniform(size);
        assert_eq!(g.size(), size);
        assert_eq!(g.blocks(), 3);
        for idx in 0..3 {
            let span = g.span(idx).unwrap();
            assert_eq!(span.start, idx * BLOCK_SIZE);
            assert_eq!(span.len, block_len(size, idx));
        }
        assert!(g.span(3).is_none(), "past the end there is no block");

        // An empty file has no blocks at all, and no range covers anything.
        let empty = BlockGeometry::uniform(0);
        assert_eq!(empty.blocks(), 0);
        assert!(empty.spans(0, 10).is_empty());
    }

    /// The B85 case: block sizes that are neither 4 MiB nor uniform. Offsets
    /// have to come from the prefix sum, not from a constant.
    #[test]
    fn a_non_uniform_geometry_places_blocks_by_prefix_sum() {
        let sizes = [5_239_804_u64, 5_670_228, 5_233_034];
        let g = BlockGeometry::from_sizes(&sizes);
        assert_eq!(g.size(), sizes.iter().sum::<u64>());
        assert_eq!(g.blocks(), 3);
        assert_eq!(g.span(0).unwrap().start, 0);
        assert_eq!(g.span(1).unwrap().start, 5_239_804);
        assert_eq!(g.span(2).unwrap().start, 5_239_804 + 5_670_228);
        assert_eq!(g.span(2).unwrap().end(), g.size());

        // A one-byte read in the middle of block 1 asks for block 1 alone —
        // under the 4 MiB assumption the same offset falls in block 2.
        let at = 6_000_000;
        assert_eq!(g.index_at(at), Some(1));
        assert_eq!(BlockGeometry::uniform(g.size()).index_at(at), Some(1));
        let spans = g.spans(at, at + 1);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].idx, 1);

        // A range crossing a boundary asks for both, and only both.
        let crossing = g.spans(5_239_803, 5_239_805);
        assert_eq!(
            crossing.iter().map(|s| s.idx).collect::<Vec<_>>(),
            vec![0, 1]
        );

        // At and past the end there is nothing to read.
        assert_eq!(g.index_at(g.size()), None);
        assert!(g.spans(g.size(), g.size() + 100).is_empty());
        assert!(g.spans(10, 10).is_empty(), "an empty range wants no blocks");

        // The single-block case from the same log: one block holding the whole
        // file, so there is no block 1 to ask for.
        let whole = BlockGeometry::from_sizes(&[4_939_506]);
        assert_eq!(whole.blocks(), 1);
        assert!(whole.span(1).is_none());
        assert_eq!(whole.spans(0, 4_939_506).len(), 1);
    }

    /// A cached block is addressed by the range it covers, not by its index
    /// alone. Re-planning a file onto its real geometry must therefore miss the
    /// blocks the uniform fallback cached, rather than serve their bytes at the
    /// wrong offset (bugs.md B85).
    #[test]
    fn a_cached_block_is_not_served_to_a_different_geometry() {
        let (c, _d) = cache();
        let u = uid("geom");
        let size = BLOCK_SIZE * 2;

        let uniform = BlockGeometry::uniform(size);
        let first = uniform.span(0).unwrap();
        let payload = vec![7u8; first.len as usize];
        c.store_block(&u, 1, size, first, &payload).unwrap();
        assert!(c.has_block(&u, 1, size, first), "cached where it was put");

        // The same file, re-planned: block 0 is now a different length, and
        // block 1 starts somewhere the old plan never had a block.
        let real = BlockGeometry::from_sizes(&[BLOCK_SIZE + 1, BLOCK_SIZE - 1]);
        assert_eq!(real.size(), size);
        assert!(
            !c.has_block(&u, 1, size, real.span(0).unwrap()),
            "same index, different length: not the block that was cached"
        );
        assert!(
            c.cached_block(&u, 1, size, real.span(1).unwrap()).is_none(),
            "same index, different start: not the block that was cached"
        );
    }

    /// The geometry sidecar is what lets a later read plan without opening a
    /// reader. It is tied to the revision, and refused if it does not describe
    /// a file of the size being read.
    #[test]
    fn a_recorded_geometry_is_returned_only_for_the_revision_it_describes() {
        let (c, _d) = cache();
        let u = uid("geom");
        let sizes = [5_239_804_u64, 5_670_228];
        let size: u64 = sizes.iter().sum();

        assert!(
            c.block_geometry(&u, 1, size).is_none(),
            "nothing is known before the first read"
        );

        c.store_block_geometry(&u, 1, size, &sizes);
        assert_eq!(
            c.block_geometry(&u, 1, size),
            Some(BlockGeometry::from_sizes(&sizes))
        );
        assert!(
            c.block_geometry(&u, 2, size).is_none(),
            "a new revision bumps the mtime, and this table described the old one"
        );
        assert!(c.block_geometry(&u, 1, size + 1).is_none());

        // A table that does not add up to the file is refused outright rather
        // than used to place blocks that would land wrongly.
        c.store_block_geometry(&u, 1, size, &[1, 2, 3]);
        assert!(c.block_geometry(&u, 1, size).is_none());

        // Evicting the file's content takes its geometry with it, so the next
        // read does not plan against a table for bytes the cache no longer has.
        c.store_block_geometry(&u, 1, size, &sizes);
        c.evict(&u);
        assert!(c.block_geometry(&u, 1, size).is_none());
    }

    /// A block of the wrong length cannot escape the cache even though the
    /// store does not check: the read validates it and reports a miss, so the
    /// next read refetches rather than serving a hole (bugs.md B84).
    #[test]
    fn a_block_of_the_wrong_length_reads_as_a_miss() {
        let (c, _d) = cache();
        let u = uid("a");
        // A 10-byte file's only block is 10 bytes; 5 is not a short tail.
        c.store_block(&u, 100, 10, blk(10, 0), b"block").unwrap();
        assert!(c.cached_block(&u, 100, 10, blk(10, 0)).is_none());
        assert!(!c.has_block(&u, 100, 10, blk(10, 0)));
    }

    /// The block cache is written without an fsync, so a crash can leave a blob
    /// shorter than the sidecar beside it claims. Serving that as content would
    /// hand the reader a hole; it has to read as a miss.
    #[test]
    fn a_truncated_block_reads_as_a_miss() {
        let (c, _d) = cache();
        let u = uid("a");
        c.store_block(&u, 100, 10, blk(10, 0), b"block-zero")
            .unwrap();
        std::fs::write(c.block_blob(&u, 0), b"block").unwrap();
        assert!(c.cached_block(&u, 100, 10, blk(10, 0)).is_none());
    }

    #[test]
    fn evict_drops_blocks() {
        let (c, _d) = cache();
        let u = uid("a");
        // Both blocks are stored at their real geometry (a 4-byte file is one
        // block), so this fails if `evict` misses them rather than because the
        // fixture was unreadable to begin with.
        c.store_block(&u, 1, 4, blk(4, 0), b"aaaa").unwrap();
        assert!(c.cached_block(&u, 1, 4, blk(4, 0)).is_some());
        c.evict(&u);
        assert!(c.cached_block(&u, 1, 4, blk(4, 0)).is_none());
    }

    #[test]
    fn block_budget_evicts_lru() {
        // Block pool fits two 4-byte blocks but not three. One block per file,
        // so each fixture is a geometrically real single-block file.
        let (c, _d) = cache_with_pool_cap(KIND_BLOCK, 8);
        let (a, b, d) = (uid("a"), uid("b"), uid("d"));
        c.store_block(&a, 1, 4, blk(4, 0), b"aaaa").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store_block(&b, 1, 4, blk(4, 0), b"bbbb").unwrap();
        // Touch a's block so b's is the least-recently-used.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(c.cached_block(&a, 1, 4, blk(4, 0)).is_some());
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store_block(&d, 1, 4, blk(4, 0), b"cccc").unwrap();
        assert!(
            c.cached_block(&a, 1, 4, blk(4, 0)).is_some(),
            "recently-read survives"
        );
        assert!(
            c.cached_block(&b, 1, 4, blk(4, 0)).is_none(),
            "LRU block evicted"
        );
        assert!(
            c.cached_block(&d, 1, 4, blk(4, 0)).is_some(),
            "newest survives"
        );
    }

    /// A failed upload must never cost the user their bytes (offline.md Phase 2):
    /// the scratch file moves to staging intact, and staging survives a reopen
    /// the way scratch deliberately does not.
    #[test]
    fn failed_write_is_staged_and_survives_reopen() {
        use std::io::Write as _;
        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let content = dir.path().join("content");
        let c = ContentCache::open(content.clone(), dir.path().join("pins.json"), 0, db.clone())
            .unwrap();

        let (mut f, scratch) = c.create_scratch().unwrap();
        f.write_all(b"unsent work").unwrap();
        drop(f);

        // A partial overwrite: only the first 11 bytes are real, so the staged
        // file must not be mistaken for uploadable whole-file content.
        let meta = StagedWrite {
            uid: uid("a").to_string(),
            len: 40,
            base_size: 40,
            base_mtime: 100,
            authored: vec![(0, 11)],
            complete: false,
            based_on: Some(Baseline {
                mtime: 100,
                size: 40,
                hash: None,
                revision_id: None,
            }),
        };
        let staged = c.stage_write(&meta, &scratch).unwrap();
        assert!(!scratch.exists(), "scratch is moved, not copied");
        assert_eq!(std::fs::read(&staged).unwrap(), b"unsent work");

        let sidecar: StagedWrite =
            serde_json::from_slice(&std::fs::read(staged.with_extension("json")).unwrap()).unwrap();
        assert_eq!(sidecar.authored, vec![(0, 11)]);
        assert!(
            !sidecar.complete,
            "gaps are zeros, not content: uploading this as-is would corrupt the file"
        );

        // Reopening wipes scratch (worthless leftovers) but must keep staging.
        drop(c);
        let _c2 = ContentCache::open(content, dir.path().join("pins.json"), 0, db).unwrap();
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            b"unsent work",
            "staged bytes outlive the daemon that failed to upload them"
        );
    }

    fn preservation_meta() -> StagedWrite {
        StagedWrite {
            uid: uid("preserved").to_string(),
            len: 14,
            base_size: 0,
            base_mtime: 100,
            authored: vec![(0, 14)],
            complete: true,
            based_on: None,
        }
    }

    #[test]
    fn orphan_preservation_publishes_exact_bytes_then_removes_source() {
        use std::io::Write as _;

        let (c, _dir) = cache();
        let (mut file, scratch) = c.create_scratch().unwrap();
        file.write_all(b"accepted bytes").unwrap();
        file.sync_all().unwrap();
        drop(file);

        let preserved = c.preserve_write(&preservation_meta(), &scratch).unwrap();

        assert!(!scratch.exists());
        assert_eq!(std::fs::read(&preserved).unwrap(), b"accepted bytes");
        let sidecar: StagedWrite =
            serde_json::from_slice(&std::fs::read(preserved.with_extension("json")).unwrap())
                .unwrap();
        let expected = preservation_meta();
        assert_eq!(sidecar.uid, expected.uid);
        assert_eq!(sidecar.len, expected.len);
        assert_eq!(sidecar.authored, expected.authored);
        assert_eq!(sidecar.complete, expected.complete);
    }

    #[test]
    fn orphan_preservation_sidecar_failure_reports_surviving_blob_across_reopen() {
        use std::io::Write as _;

        let (c, dir) = cache();
        let (mut file, scratch) = c.create_scratch().unwrap();
        file.write_all(b"only surviving copy").unwrap();
        file.sync_all().unwrap();
        drop(file);

        let destination = c.staging_dir.join("forced-preservation-failure");
        let side = destination.with_extension("json");
        std::fs::create_dir(&side).unwrap();

        let error = c
            .preserve_write_at(&preservation_meta(), &scratch, &destination)
            .unwrap_err();
        assert_eq!(error.surviving_path, scratch);
        assert!(
            scratch.exists(),
            "failed sidecar publication must roll the blob back beside its marker"
        );
        assert_eq!(
            std::fs::read(&error.surviving_path).unwrap(),
            b"only surviving copy"
        );
        assert_eq!(
            error.marker_path,
            Some(ContentCache::scratch_sidecar(&scratch))
        );
        assert!(
            side.is_dir(),
            "the induced sidecar failure remains observable"
        );

        let content = dir.path().join("content");
        let pins = dir.path().join("pins-reopen.json");
        drop(c);
        let reopened =
            ContentCache::open(content, pins, 0, Arc::new(Db::open_in_memory().unwrap())).unwrap();
        let recovered = reopened.recovered_writes();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            std::fs::read(&recovered[0].0).unwrap(),
            b"only surviving copy"
        );
        assert_eq!(recovered[0].1.uid, preservation_meta().uid);
        drop(reopened);
    }

    /// `clone_or_copy` has three strategies and only the last one is portable,
    /// so what matters is that the destination is byte-identical whichever the
    /// filesystem under `/tmp` happens to take — including when it already held
    /// longer content, which a reflink or a short `copy_file_range` must not
    /// leave a tail of.
    #[test]
    fn clone_or_copy_reproduces_the_source_over_any_existing_destination() {
        let dir = TempDir::new();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        std::fs::write(&src, b"the new content").unwrap();
        std::fs::write(&dst, vec![b'x'; 4096]).unwrap();

        let len = ContentCache::clone_or_copy(&src, &dst).unwrap();

        assert_eq!(len, 15);
        assert_eq!(std::fs::read(&dst).unwrap(), b"the new content");
    }

    /// An empty staged blob is a real case — a file truncated to zero and
    /// closed — and `FICLONE` of a zero-length source is a documented `EINVAL`.
    #[test]
    fn clone_or_copy_handles_an_empty_source() {
        let dir = TempDir::new();
        let src = dir.path().join("empty");
        let dst = dir.path().join("dst");
        std::fs::write(&src, b"").unwrap();
        std::fs::write(&dst, b"stale").unwrap();

        assert_eq!(ContentCache::clone_or_copy(&src, &dst).unwrap(), 0);
        assert!(std::fs::read(&dst).unwrap().is_empty());
    }

    /// A2: `fsync(2)` promises the bytes survive a crash, but queueing happens
    /// at `release`. A scratch file marked durable must therefore outlive the
    /// open that clears the scratch directory — and an unmarked one must not,
    /// since that is a crashed run's rubbish.
    #[test]
    fn fsynced_scratch_survives_reopen_and_unmarked_scratch_does_not() {
        use std::io::Write as _;
        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let content = dir.path().join("content");
        let pins = dir.path().join("pins.json");
        let c = ContentCache::open(content.clone(), pins.clone(), 0, db.clone()).unwrap();

        let (mut synced, synced_path) = c.create_scratch().unwrap();
        synced.write_all(b"fsynced bytes").unwrap();
        synced.sync_all().unwrap();
        let meta = StagedWrite {
            uid: uid("a").to_string(),
            len: 13,
            base_size: 0,
            base_mtime: 100,
            authored: vec![(0, 13)],
            complete: true,
            based_on: None,
        };
        c.mark_scratch_durable(&synced_path, &meta).unwrap();

        // A second handle that was written but never fsynced: no promise was
        // made about it, and it is indistinguishable from a torn buffer.
        let (mut loose, loose_path) = c.create_scratch().unwrap();
        loose.write_all(b"never fsynced").unwrap();

        // Crash: no release, no staging, just a restart.
        drop(c);
        let c2 = ContentCache::open(content, pins, 0, db).unwrap();

        assert!(!synced_path.exists(), "scratch dir is still cleared");
        assert!(!loose_path.exists());

        let recovered = c2.recovered_writes();
        assert_eq!(recovered.len(), 1, "only the fsynced write is recoverable");
        let (blob, got) = &recovered[0];
        assert_eq!(std::fs::read(blob).unwrap(), b"fsynced bytes");
        assert_eq!(got.uid, uid("a").to_string());
        assert_eq!(got.authored, vec![(0, 13)]);

        // Staging it is what a restart does with it; the sidecar goes after.
        let staged = c2.stage_write(got, blob).unwrap();
        c2.discard_recovered(blob);
        assert_eq!(std::fs::read(&staged).unwrap(), b"fsynced bytes");
        assert!(c2.recovered_writes().is_empty(), "recovery does not repeat");
    }

    #[test]
    fn recovery_keeps_an_orphan_blob_when_its_sidecar_is_missing() {
        let (c, dir) = cache();
        let orphan = dir.path().join("content/recovery/orphaned-user-bytes");
        std::fs::write(&orphan, b"retain me").unwrap();

        assert!(c.recovered_writes().is_empty());
        assert_eq!(
            std::fs::read(&orphan).unwrap(),
            b"retain me",
            "metadata loss must not trigger deletion of unexplained content"
        );
    }

    #[test]
    fn failed_scratch_rescue_does_not_delete_the_durable_source() {
        use std::io::Write as _;
        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let content = dir.path().join("content");
        let pins = dir.path().join("pins.json");
        let c = ContentCache::open(content.clone(), pins.clone(), 0, db.clone()).unwrap();
        let (mut file, scratch) = c.create_scratch().unwrap();
        file.write_all(b"durable source").unwrap();
        file.sync_all().unwrap();
        let meta = StagedWrite {
            uid: uid("rescue").to_string(),
            len: 14,
            base_size: 0,
            base_mtime: 0,
            authored: vec![(0, 14)],
            complete: true,
            based_on: None,
        };
        c.mark_scratch_durable(&scratch, &meta).unwrap();
        let blocking_destination = content.join("recovery").join(scratch.file_name().unwrap());
        std::fs::create_dir(&blocking_destination).unwrap();
        drop(file);
        drop(c);

        let _c2 = ContentCache::open(content, pins, 0, db).unwrap();
        assert_eq!(std::fs::read(&scratch).unwrap(), b"durable source");
        assert!(ContentCache::scratch_sidecar(&scratch).exists());
    }

    /// Releasing a write hands the bytes to staging, so the sidecar an earlier
    /// `fsync` left must not offer recovery a second, stale copy of it.
    #[test]
    fn cleared_durability_marker_stops_recovery() {
        use std::io::Write as _;
        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let content = dir.path().join("content");
        let pins = dir.path().join("pins.json");
        let c = ContentCache::open(content.clone(), pins.clone(), 0, db.clone()).unwrap();

        let (mut f, path) = c.create_scratch().unwrap();
        f.write_all(b"released").unwrap();
        let meta = StagedWrite {
            uid: uid("a").to_string(),
            len: 8,
            base_size: 0,
            base_mtime: 100,
            authored: vec![(0, 8)],
            complete: true,
            based_on: None,
        };
        c.mark_scratch_durable(&path, &meta).unwrap();
        c.clear_scratch_durable(&path);

        drop(c);
        let c2 = ContentCache::open(content, pins, 0, db).unwrap();
        assert!(c2.recovered_writes().is_empty());
    }

    /// A directory on a *different* filesystem from the cache, so the
    /// cross-device fallbacks can be driven for real rather than reasoned
    /// about. `None` when this machine offers no second filesystem to use, in
    /// which case the callers skip rather than fail — the fallback is still
    /// covered by the same-device paths, and a green suite that quietly means
    /// "not checked" is worse than a skip that says so.
    fn other_device_dir(label: &str) -> Option<PathBuf> {
        use std::os::unix::fs::MetadataExt as _;
        let here = std::fs::metadata(std::env::temp_dir()).ok()?.dev();
        let candidate = Path::new("/dev/shm");
        if std::fs::metadata(candidate).ok()?.dev() == here {
            return None;
        }
        let dir = candidate.join(format!("pdfs-xdev-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    // ---- B50: an orphaned write's only copy is never the one deleted -------

    /// A staging directory that cannot be written to at all. Rename fails, the
    /// copy fallback fails, and there is nowhere else to put the bytes — so the
    /// scratch file, which is the only copy, has to still be there and still be
    /// the bytes the user wrote.
    #[test]
    fn orphan_preservation_keeps_the_scratch_copy_when_staging_is_unwritable() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let (c, _dir) = cache();
        let (mut file, scratch) = c.create_scratch().unwrap();
        file.write_all(b"the only copy there is").unwrap();
        file.sync_all().unwrap();
        drop(file);

        std::fs::set_permissions(&c.staging_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let error = c
            .preserve_write(&preservation_meta(), &scratch)
            .unwrap_err();
        std::fs::set_permissions(&c.staging_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(error.surviving_path, scratch);
        assert_eq!(
            std::fs::read(&scratch).unwrap(),
            b"the only copy there is",
            "byte-identical, and still where the error says it is"
        );
        assert!(
            error.marker_path.is_some(),
            "and carrying the marker that makes it recoverable"
        );
    }

    /// The same failure, seen from the next startup: the retained scratch file
    /// is not just present, it is *discoverable* — recovery finds it with its
    /// metadata, which is what makes it a preserved write rather than debris.
    #[test]
    fn a_retained_orphan_write_is_discoverable_after_a_restart() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let content = dir.path().join("content");
        let pins = dir.path().join("pins.json");
        let c = ContentCache::open(content.clone(), pins.clone(), 0, db.clone()).unwrap();

        let (mut file, scratch) = c.create_scratch().unwrap();
        file.write_all(b"accepted bytes").unwrap();
        file.sync_all().unwrap();
        drop(file);

        std::fs::set_permissions(&c.staging_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(c.preserve_write(&preservation_meta(), &scratch).is_err());
        std::fs::set_permissions(&c.staging_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(c);

        let reopened = ContentCache::open(content, pins, 0, db).unwrap();
        let recovered = reopened.recovered_writes();
        assert_eq!(recovered.len(), 1);
        assert_eq!(std::fs::read(&recovered[0].0).unwrap(), b"accepted bytes");
        assert_eq!(recovered[0].1.uid, preservation_meta().uid);
        assert_eq!(recovered[0].1.authored, preservation_meta().authored);
    }

    /// The copy fallback, driven across a real device boundary. Its whole point
    /// is ordering: the destination and its sidecar are published and synced
    /// *first*, and only then is the source removed.
    #[test]
    fn orphan_preservation_across_a_device_boundary_publishes_before_removing() {
        use std::io::Write as _;

        let Some(elsewhere) = other_device_dir("preserve") else {
            eprintln!("skipped: no second filesystem available to cross");
            return;
        };
        let (c, _dir) = cache();
        let scratch = elsewhere.join("scratch-blob");
        {
            let mut file = std::fs::File::create(&scratch).unwrap();
            file.write_all(b"accepted bytes").unwrap();
            file.sync_all().unwrap();
        }

        let preserved = c.preserve_write(&preservation_meta(), &scratch).unwrap();

        assert_eq!(std::fs::read(&preserved).unwrap(), b"accepted bytes");
        assert!(
            preserved.with_extension("json").is_file(),
            "the sidecar is published before the source is dropped"
        );
        assert!(!scratch.exists(), "and only then is the source removed");
        assert!(
            !ContentCache::scratch_sidecar(&scratch).exists(),
            "the source's durability marker goes with it"
        );
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// And when publication across that boundary fails partway, the marked
    /// source stays put. This is the case B50 was: the old code removed it.
    #[test]
    fn a_failed_cross_device_preservation_keeps_the_marked_source() {
        use std::io::Write as _;

        let Some(elsewhere) = other_device_dir("preserve-fail") else {
            eprintln!("skipped: no second filesystem available to cross");
            return;
        };
        let (c, _dir) = cache();
        let scratch = elsewhere.join("scratch-blob");
        {
            let mut file = std::fs::File::create(&scratch).unwrap();
            file.write_all(b"only surviving copy").unwrap();
            file.sync_all().unwrap();
        }

        let destination = c.staging_dir.join("cross-device-failure");
        std::fs::create_dir(destination.with_extension("json")).unwrap();

        let error = c
            .preserve_write_at(&preservation_meta(), &scratch, &destination)
            .unwrap_err();

        assert_eq!(error.surviving_path, scratch);
        assert_eq!(
            std::fs::read(&scratch).unwrap(),
            b"only surviving copy",
            "the source outlives a publication that did not finish"
        );
        assert_eq!(
            error.marker_path,
            Some(ContentCache::scratch_sidecar(&scratch)),
            "and is marked, so the next startup rescues it"
        );
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    // ---- B51: the scratch sidecar is a durable, self-validating record -----

    /// A sidecar is published by rename, so a crash leaves either the whole
    /// record or none of it — never a half-written one under the real name.
    /// The temporary it is renamed from must also not be mistaken for a record.
    #[test]
    fn a_half_written_scratch_sidecar_is_never_read_as_a_record() {
        use std::io::Write as _;

        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let content = dir.path().join("content");
        let pins = dir.path().join("pins.json");
        let c = ContentCache::open(content.clone(), pins.clone(), 0, db.clone()).unwrap();

        let (mut file, scratch) = c.create_scratch().unwrap();
        file.write_all(b"authored bytes").unwrap();
        file.sync_all().unwrap();
        drop(file);

        // A crash between writing the temporary and renaming it: exactly what
        // is on disk if the process dies inside `mark_scratch_durable`.
        let side = ContentCache::scratch_sidecar(&scratch);
        std::fs::write(side.with_extension("json.tmp"), b"{\"uid\":\"prot").unwrap();
        assert!(!side.exists(), "no record was ever published");

        drop(c);
        let reopened = ContentCache::open(content, pins, 0, db).unwrap();
        assert!(
            reopened.recovered_writes().is_empty(),
            "an unpublished temporary describes no write"
        );
    }

    /// A sidecar that survived but does not parse — a torn tail, a truncated
    /// file — must not take its blob down with it. The bytes are the user's
    /// work; the metadata only says what to do with them.
    #[test]
    fn a_corrupt_scratch_sidecar_still_rescues_its_bytes() {
        use std::io::Write as _;

        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let content = dir.path().join("content");
        let pins = dir.path().join("pins.json");
        let c = ContentCache::open(content.clone(), pins.clone(), 0, db.clone()).unwrap();

        let (mut file, scratch) = c.create_scratch().unwrap();
        file.write_all(b"user work").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let meta = StagedWrite {
            uid: uid("corrupt").to_string(),
            len: 9,
            base_size: 0,
            base_mtime: 0,
            authored: vec![(0, 9)],
            complete: true,
            based_on: None,
        };
        c.mark_scratch_durable(&scratch, &meta).unwrap();

        // Truncate the published sidecar to a valid JSON prefix.
        let side = ContentCache::scratch_sidecar(&scratch);
        let full = std::fs::read(&side).unwrap();
        std::fs::write(&side, &full[..full.len() / 2]).unwrap();

        drop(c);
        let reopened = ContentCache::open(content.clone(), pins, 0, db).unwrap();

        assert!(
            reopened.recovered_writes().is_empty(),
            "an unparseable record cannot be replayed"
        );
        let rescued: Vec<PathBuf> = std::fs::read_dir(content.join("recovery"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_none_or(|e| e != "json"))
            .collect();
        assert_eq!(rescued.len(), 1, "the blob is still moved to recovery");
        assert_eq!(std::fs::read(&rescued[0]).unwrap(), b"user work");
    }

    /// `mark_scratch_durable` is called repeatedly as a file is fsynced. Each
    /// call has to leave one complete, parseable record and no debris.
    #[test]
    fn remarking_a_scratch_file_replaces_its_record_atomically() {
        use std::io::Write as _;

        let (c, _dir) = cache();
        let (mut file, scratch) = c.create_scratch().unwrap();
        file.write_all(b"first").unwrap();
        file.sync_all().unwrap();
        drop(file);

        let mut meta = StagedWrite {
            uid: uid("remark").to_string(),
            len: 5,
            base_size: 0,
            base_mtime: 0,
            authored: vec![(0, 5)],
            complete: true,
            based_on: None,
        };
        c.mark_scratch_durable(&scratch, &meta).unwrap();
        meta.len = 11;
        meta.authored = vec![(0, 11)];
        c.mark_scratch_durable(&scratch, &meta).unwrap();

        let side = ContentCache::scratch_sidecar(&scratch);
        let got: StagedWrite = serde_json::from_slice(&std::fs::read(&side).unwrap()).unwrap();
        assert_eq!(got.len, 11);
        assert_eq!(got.authored, vec![(0, 11)]);
        assert!(
            !side.with_extension("json.tmp").exists(),
            "the temporary is renamed, not left behind"
        );
    }

    // ---- B52: staged-write publication is crash-safe -----------------------

    /// The cross-filesystem half of `stage_write`. The source is copied, not
    /// renamed, and must not be removed until the destination and its sidecar
    /// are both on disk.
    #[test]
    fn staging_across_a_device_boundary_removes_the_source_last() {
        use std::io::Write as _;

        let Some(elsewhere) = other_device_dir("stage") else {
            eprintln!("skipped: no second filesystem available to cross");
            return;
        };
        let (c, _dir) = cache();
        let scratch = elsewhere.join("scratch-blob");
        {
            let mut file = std::fs::File::create(&scratch).unwrap();
            file.write_all(b"staged across devices").unwrap();
            file.sync_all().unwrap();
        }

        let meta = preservation_meta();
        let staged = c.stage_write(&meta, &scratch).unwrap();

        assert_eq!(std::fs::read(&staged).unwrap(), b"staged across devices");
        let sidecar: StagedWrite =
            serde_json::from_slice(&std::fs::read(staged.with_extension("json")).unwrap()).unwrap();
        assert_eq!(sidecar.uid, meta.uid);
        assert!(
            !scratch.exists(),
            "the source goes last, and only on success"
        );
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// And when the sidecar cannot be published, the copied source stays: a
    /// staged blob nothing describes is not a substitute for the bytes.
    #[test]
    fn a_failed_staging_sidecar_keeps_the_cross_device_source() {
        use std::io::Write as _;

        let Some(elsewhere) = other_device_dir("stage-fail") else {
            eprintln!("skipped: no second filesystem available to cross");
            return;
        };
        let (c, _dir) = cache();
        let scratch = elsewhere.join("scratch-blob");
        {
            let mut file = std::fs::File::create(&scratch).unwrap();
            file.write_all(b"still the source").unwrap();
            file.sync_all().unwrap();
        }

        // Block the sidecar's temporary name with a directory, so the copy
        // lands and only publication fails.
        let meta = preservation_meta();
        let destination = c.staging_dir.join("blocked-sidecar");
        std::fs::create_dir(destination.with_extension("json.tmp")).unwrap();

        assert!(c.stage_write_at(&meta, &scratch, &destination).is_err());
        assert_eq!(
            std::fs::read(&scratch).unwrap(),
            b"still the source",
            "an unpublished staging blob does not authorise deleting the source"
        );
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// A staged blob whose sidecar never landed is still user data. Startup
    /// must leave both it and the leftover temporary alone rather than tidying
    /// the bytes away.
    #[test]
    fn an_unaccompanied_staged_blob_survives_a_restart() {
        let dir = TempDir::new();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let content = dir.path().join("content");
        let pins = dir.path().join("pins.json");
        let c = ContentCache::open(content.clone(), pins.clone(), 0, db.clone()).unwrap();

        let orphan = c.staging_dir.join("unaccompanied-bytes");
        std::fs::write(&orphan, b"nobody describes me").unwrap();
        let leftover = c.staging_dir.join("interrupted.json.tmp");
        std::fs::write(&leftover, b"{\"uid\":\"prot").unwrap();

        drop(c);
        let _reopened = ContentCache::open(content, pins, 0, db).unwrap();
        assert_eq!(
            std::fs::read(&orphan).unwrap(),
            b"nobody describes me",
            "staging is never cleared on startup"
        );
        assert!(leftover.exists());
    }

    #[test]
    fn scratch_file_is_writable_and_isolated() {
        use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
        let (c, _d) = cache();
        let (mut f1, p1) = c.create_scratch().unwrap();
        let (_f2, p2) = c.create_scratch().unwrap();
        assert_ne!(p1, p2, "each scratch file is unique");
        f1.write_all(b"scratch").unwrap();
        f1.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        f1.read_to_string(&mut s).unwrap();
        assert_eq!(s, "scratch");
    }
}
