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

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use proton_drive_rs::proton_sdk::ids::NodeUid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::error::Result;

/// `cache_entries.kind` tag for whole-file content blobs.
const KIND_BLOB: &str = "blob";
/// `cache_entries.kind` tag for on-demand block-cache chunks.
const KIND_BLOCK: &str = "block";

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

/// Block size for the on-demand block cache. Matches the SDK content block size
/// (`DEFAULT_BLOCK_SIZE`, 4 MiB) so each cached block maps to exactly one
/// `download_range` fetch with no straddling.
pub const BLOCK_SIZE: u64 = 1 << 22;

/// Validity tag stored alongside a cached blob. A blob is fresh only if both
/// fields still match the node's current metadata.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
struct Meta {
    /// Node modification time, epoch seconds.
    mtime: i64,
    /// Plaintext size in bytes.
    size: u64,
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
}

/// The legacy JSON pin registry, kept only to import a pre-P5 `pins.json` into
/// the DB once on open. Live pins are owned by the `pins` table.
#[derive(Serialize, Deserialize, Default)]
struct PinFile {
    pins: BTreeMap<String, Pin>,
}

/// Content cache rooted at a directory, with a sibling pin-registry file.
pub struct ContentCache {
    /// Directory holding `<key>` blobs and `<key>.meta` tags.
    content_dir: PathBuf,
    /// Subdirectory holding cached thumbnails (`<key>.t<n>` + `.meta`). Kept out
    /// of `content_dir` so the budget scan never sees thumbnail files.
    thumb_dir: PathBuf,
    /// Subdirectory holding on-demand block-cache files (`<key>.b<idx>` +
    /// `.meta`). Kept out of `content_dir` so the whole-file budget scan never
    /// sees them; blocks carry their own LRU budget.
    block_dir: PathBuf,
    /// Subdirectory for write-handle scratch files (disk-backed write buffers).
    /// Emptied on open so a crashed run leaves no orphans.
    scratch_dir: PathBuf,
    /// JSON pin registry path.
    pins_path: PathBuf,
    /// Soft cap on total blob bytes. Exceeded only transiently: a `store`
    /// evicts least-recently-used *unpinned* blobs back under the cap. `0`
    /// disables the cap. Pinned blobs are never evicted, so pins alone may push
    /// the cache over budget. Atomic so the daemon can retune it at runtime
    /// (a Settings-page change) via [`set_budget`](Self::set_budget) without
    /// taking a lock on every cache read.
    max_bytes: AtomicU64,
    /// Unified metadata DB. Its `cache_entries` table is the LRU index: every
    /// store/read/evict updates it, and the budget enforcers query it instead of
    /// scanning the cache directories (plan.md P4).
    db: Arc<Db>,
}

impl ContentCache {
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
        let block_dir = content_dir.join("blocks");
        std::fs::create_dir_all(&block_dir)?;
        // Scratch holds disk-backed write buffers; a previous run's leftovers are
        // worthless, so start clean.
        let scratch_dir = content_dir.join("scratch");
        let _ = std::fs::remove_dir_all(&scratch_dir);
        std::fs::create_dir_all(&scratch_dir)?;
        if let Some(parent) = pins_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let cache = Self {
            content_dir,
            thumb_dir,
            block_dir,
            scratch_dir,
            pins_path,
            max_bytes: AtomicU64::new(max_bytes),
            db,
        };
        cache.reconcile()?;
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
        self.db.cache_clear()?;
        for (dir, kind) in [
            (&self.content_dir, KIND_BLOB),
            (&self.block_dir, KIND_BLOCK),
        ] {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if name.ends_with(".meta") || name.ends_with(".tmp") {
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
                self.db.cache_touch(name, kind, meta.len(), at)?;
            }
        }
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
        let _ = self.db.cache_accessed(&Self::key(uid), now_millis());
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
        let tmp = blob.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &blob)?;
        let meta = serde_json::to_vec(&Meta { mtime, size })?;
        std::fs::write(self.meta_path(uid), meta)?;
        // Register the blob in the LRU index before enforcing the budget so the
        // newest entry is weighed (and ordered most-recent) like any other.
        self.db
            .cache_touch(&Self::key(uid), KIND_BLOB, bytes.len() as u64, now_millis())?;
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

    /// Serve cached block `idx` (a [`BLOCK_SIZE`]-aligned chunk) of `uid`, or
    /// `None` on miss/stale. Validated against `(mtime, size)` like a whole-file
    /// blob, so a new revision (which bumps the mtime) is detected. Bumps the
    /// block's mtime for LRU, best effort.
    pub fn cached_block(&self, uid: &NodeUid, mtime: i64, size: u64, idx: u64) -> Option<Vec<u8>> {
        let want = Meta { mtime, size };
        let meta: Meta =
            serde_json::from_slice(&std::fs::read(self.block_meta(uid, idx)).ok()?).ok()?;
        if meta != want {
            return None;
        }
        let mut f = std::fs::File::open(self.block_blob(uid, idx)).ok()?;
        // LRU touch in the cache index (best effort).
        let _ = self
            .db
            .cache_accessed(&self.block_key(uid, idx), now_millis());
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        Some(buf)
    }

    /// Store `bytes` as cached block `idx` of `uid`, tagged `(mtime, size)`.
    /// Temp-file-then-rename like [`store`](Self::store); meta written last so a
    /// crash mid-store fails validation. Enforces the block-cache LRU budget.
    pub fn store_block(
        &self,
        uid: &NodeUid,
        mtime: i64,
        size: u64,
        idx: u64,
        bytes: &[u8],
    ) -> Result<()> {
        let blob = self.block_blob(uid, idx);
        let tmp = self
            .block_dir
            .join(format!("{}.b{idx}.tmp", Self::key(uid)));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &blob)?;
        std::fs::write(
            self.block_meta(uid, idx),
            serde_json::to_vec(&Meta { mtime, size })?,
        )?;
        self.db.cache_touch(
            &self.block_key(uid, idx),
            KIND_BLOCK,
            bytes.len() as u64,
            now_millis(),
        )?;
        self.enforce_block_budget();
        Ok(())
    }

    /// Evict least-recently-used block-cache files until the block dir fits
    /// `max_bytes`. No-op when the cap is disabled (`0`). All blocks are
    /// evictable — pinned files are served from whole-file blobs, never blocks.
    fn enforce_block_budget(&self) {
        let cap = self.cap();
        if cap == 0 {
            return;
        }
        // LRU-ordered (oldest first) block entries straight from the index — no
        // directory scan. All blocks are evictable; pinned files are served from
        // whole-file blobs, never blocks.
        let Ok(entries) = self.db.cache_entries_by_kind(KIND_BLOCK) else {
            return;
        };
        let mut total: u64 = entries.iter().map(|(_, size)| *size).sum();
        if total <= cap {
            return;
        }
        for (key, size) in entries {
            if total <= cap {
                break;
            }
            // `key` is the block file's name (and its `.meta` sibling's stem).
            let _ = std::fs::remove_file(self.block_dir.join(&key));
            let _ = std::fs::remove_file(self.block_dir.join(format!("{key}.meta")));
            let _ = self.db.cache_remove(&key);
            total = total.saturating_sub(size);
        }
    }

    /// Create a fresh, empty read-write scratch file for a disk-backed write
    /// handle. Returns the open file and its path (for cleanup on release).
    pub fn create_scratch(&self) -> Result<(std::fs::File, PathBuf)> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = self.scratch_dir.join(format!(
            "w-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;
        Ok((file, path))
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
        // Drop every cached block (and its meta/tmp) for this uid. The block
        // dir is still scanned here because eviction targets one uid's files by
        // name prefix, not by LRU order; the index rows are removed in one shot
        // by `cache_remove_all` below.
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
    }

    fn thumb_blob(&self, uid: &NodeUid, ttype: i32) -> PathBuf {
        self.thumb_dir.join(format!("{}.t{ttype}", Self::key(uid)))
    }

    fn thumb_meta(&self, uid: &NodeUid, ttype: i32) -> PathBuf {
        self.thumb_dir
            .join(format!("{}.t{ttype}.meta", Self::key(uid)))
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
        std::fs::read(self.thumb_blob(uid, ttype)).ok()
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
        blob.exists().then_some(blob)
    }

    /// On-disk path where `uid`'s `ttype` thumbnail lives once stored.
    pub fn thumbnail_path(&self, uid: &NodeUid, ttype: i32) -> PathBuf {
        self.thumb_blob(uid, ttype)
    }

    /// Cache `bytes` as the `ttype` thumbnail for `uid`, tagged with `mtime`.
    /// Blob written to a temp file then renamed; the meta tag is written last so
    /// a crash mid-store fails validation rather than serving a torn thumbnail.
    pub fn store_thumbnail(
        &self,
        uid: &NodeUid,
        ttype: i32,
        mtime: i64,
        bytes: &[u8],
    ) -> Result<()> {
        let blob = self.thumb_blob(uid, ttype);
        let tmp = blob.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &blob)?;
        std::fs::write(self.thumb_meta(uid, ttype), serde_json::to_vec(&mtime)?)?;
        Ok(())
    }

    /// Evict least-recently-used *unpinned* blobs until total blob bytes fit the
    /// configured `max_bytes` cap. No-op when the cap is disabled (`0`) or the
    /// cache already fits. Pinned blobs are skipped, so a cache held entirely by
    /// pins can legitimately stay over budget.
    fn enforce_budget(&self) {
        let cap = self.cap();
        if cap == 0 {
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

        // LRU-ordered (oldest first) blob entries from the index — no directory
        // scan. Pinned blobs still count toward the total (so pins alone can hold
        // the cache over budget) but are never chosen as victims.
        let Ok(entries) = self.db.cache_entries_by_kind(KIND_BLOB) else {
            return;
        };
        let mut total: u64 = entries.iter().map(|(_, size)| *size).sum();
        if total <= cap {
            return;
        }
        for (key, size) in entries {
            if total <= cap {
                break;
            }
            if pinned.contains(&key) {
                continue; // counts toward total but is never a victim
            }
            // `key` is the blob file's name (and its `.meta` sibling's stem).
            let _ = std::fs::remove_file(self.content_dir.join(&key));
            let _ = std::fs::remove_file(self.content_dir.join(format!("{key}.meta")));
            let _ = self.db.cache_remove(&key);
            total = total.saturating_sub(size);
        }
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
            .map(|(uid, path, recursive)| Pin {
                uid,
                path,
                recursive,
            })
            .collect()
    }

    /// Total bytes of cached content blobs (pinned + unpinned), matching what
    /// [`enforce_budget`](Self::enforce_budget) weighs against the cap.
    /// Thumbnails live in a sibling dir and are not counted. Cheap directory
    /// scan, safe to call from a front-end for a usage read-out.
    pub fn usage(&self) -> u64 {
        let Ok(rd) = std::fs::read_dir(&self.content_dir) else {
            return 0;
        };
        let mut total = 0u64;
        for entry in rd.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.ends_with(".meta") || name.ends_with(".tmp") {
                continue;
            }
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                total += meta.len();
            }
        }
        total
    }

    /// Configured soft byte cap (`0` = unlimited), for display alongside
    /// [`usage`](Self::usage).
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
        // Every on-demand block (transient partial reads, re-fetched on demand).
        if let Ok(entries) = self.db.cache_entries_by_kind(KIND_BLOCK) {
            for (key, size) in entries {
                let _ = std::fs::remove_file(self.block_dir.join(&key));
                let _ = std::fs::remove_file(self.block_dir.join(format!("{key}.meta")));
                let _ = self.db.cache_remove(&key);
                freed = freed.saturating_add(size);
            }
        }
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

    #[test]
    fn budget_evicts_lru_unpinned() {
        // Cap fits two 4-byte blobs but not three.
        let (c, _d) = cache_capped(8);
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
        let (c, _d) = cache_capped(8);
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
        c.store_block(&b, 1, 8, 0, b"bbbb").unwrap();

        let freed = c.clear_unpinned();
        assert_eq!(freed, 8, "one 4-byte blob + one 4-byte block freed");
        assert!(c.is_cached(&a, 1, 4), "pinned blob survives purge");
        assert!(!c.is_cached(&b, 1, 4), "unpinned blob purged");
        assert!(c.cached_block(&b, 1, 8, 0).is_none(), "block purged");
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

        // Tightening to 8 bytes evicts the LRU unpinned blob(s) now, not on the
        // next store.
        c.set_budget(8);
        assert!(!c.is_cached(&a, 1, 4), "oldest blob evicted on tighten");
        assert!(c.is_cached(&b, 1, 4));
        assert!(c.is_cached(&d, 1, 4));
        assert_eq!(c.budget(), 8);
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
        c.store_block(&u, 100, 4096, 0, b"block-zero").unwrap();
        assert_eq!(c.cached_block(&u, 100, 4096, 0).unwrap(), b"block-zero");
        // A different index is a separate cache entry, absent here.
        assert!(c.cached_block(&u, 100, 4096, 1).is_none());
        // A new revision (mtime/size bump) invalidates the block.
        assert!(c.cached_block(&u, 101, 4096, 0).is_none());
        assert!(c.cached_block(&u, 100, 5000, 0).is_none());
    }

    #[test]
    fn evict_drops_blocks() {
        let (c, _d) = cache();
        let u = uid("a");
        c.store_block(&u, 1, 8, 0, b"aaaa").unwrap();
        c.store_block(&u, 1, 8, 1, b"bbbb").unwrap();
        c.evict(&u);
        assert!(c.cached_block(&u, 1, 8, 0).is_none());
        assert!(c.cached_block(&u, 1, 8, 1).is_none());
    }

    #[test]
    fn block_budget_evicts_lru() {
        // Cap fits two 4-byte blocks but not three.
        let (c, _d) = cache_capped(8);
        let u = uid("a");
        c.store_block(&u, 1, 12, 0, b"aaaa").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store_block(&u, 1, 12, 1, b"bbbb").unwrap();
        // Touch block 0 so block 1 is the least-recently-used.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(c.cached_block(&u, 1, 12, 0).is_some());
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.store_block(&u, 1, 12, 2, b"cccc").unwrap();
        assert!(
            c.cached_block(&u, 1, 12, 0).is_some(),
            "recently-read survives"
        );
        assert!(c.cached_block(&u, 1, 12, 1).is_none(), "LRU block evicted");
        assert!(c.cached_block(&u, 1, 12, 2).is_some(), "newest survives");
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
