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
use std::time::SystemTime;

use proton_drive_rs::proton_sdk::ids::NodeUid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;

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

/// One pinned file, as persisted to the pin registry.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pin {
    /// Node uid in `volume~link` display form.
    pub uid: String,
    /// Last path the file was pinned under, for display in `status`. Advisory
    /// only — the uid is the identity.
    pub path: String,
}

/// The pin registry, persisted as JSON. Keyed by uid display string so a pin
/// survives renames/moves of the node.
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
    /// the cache over budget.
    max_bytes: u64,
}

impl ContentCache {
    /// Open (and create) a cache under `content_dir`, with the pin registry at
    /// `pins_path` and a `max_bytes` size cap (`0` = unlimited). Both parent
    /// directories are created if missing.
    pub fn open(content_dir: PathBuf, pins_path: PathBuf, max_bytes: u64) -> Result<Self> {
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
        Ok(Self {
            content_dir,
            thumb_dir,
            block_dir,
            scratch_dir,
            pins_path,
            max_bytes,
        })
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
        // Record the access for LRU: bump the blob's mtime to now. Best effort —
        // a failed touch only makes eviction order slightly less accurate.
        let mut f = std::fs::File::open(&blob).ok()?;
        let _ = f.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()));
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
        self.enforce_budget();
        Ok(())
    }

    fn block_blob(&self, uid: &NodeUid, idx: u64) -> PathBuf {
        self.block_dir.join(format!("{}.b{idx}", Self::key(uid)))
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
        let _ = f.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()));
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
        self.enforce_block_budget();
        Ok(())
    }

    /// Evict least-recently-used block-cache files until the block dir fits
    /// `max_bytes`. No-op when the cap is disabled (`0`). All blocks are
    /// evictable — pinned files are served from whole-file blobs, never blocks.
    fn enforce_block_budget(&self) {
        if self.max_bytes == 0 {
            return;
        }
        let mut total: u64 = 0;
        let mut blobs: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.block_dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".meta") || name.ends_with(".tmp") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            let atime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            blobs.push((path, meta.len(), atime));
        }
        if total <= self.max_bytes {
            return;
        }
        blobs.sort_by_key(|(_, _, atime)| *atime);
        for (path, len, _) in blobs {
            if total <= self.max_bytes {
                break;
            }
            let meta = path.with_file_name(format!(
                "{}.meta",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(meta);
            total = total.saturating_sub(len);
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
        // Drop every cached block (and its meta/tmp) for this uid.
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
        if self.max_bytes == 0 {
            return;
        }
        // Pinned blobs (by cache key) are exempt from eviction.
        let pinned: HashSet<String> = self
            .load_pins()
            .pins
            .keys()
            .map(|uid| Self::key_str(uid))
            .collect();

        // Scan blobs (the `<key>` files; skip `.meta`/`.tmp` siblings), record
        // size + last-access (mtime) for each.
        let mut total: u64 = 0;
        let mut blobs: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.content_dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".meta") || name.ends_with(".tmp") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            if pinned.contains(name) {
                continue; // counts toward total but is never a victim
            }
            let atime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            blobs.push((path, meta.len(), atime));
        }
        if total <= self.max_bytes {
            return;
        }
        // Oldest access first.
        blobs.sort_by_key(|(_, _, atime)| *atime);
        for (path, len, _) in blobs {
            if total <= self.max_bytes {
                break;
            }
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("meta"));
            total = total.saturating_sub(len);
        }
    }

    fn load_pins(&self) -> PinFile {
        std::fs::read(&self.pins_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_pins(&self, pins: &PinFile) -> Result<()> {
        std::fs::write(&self.pins_path, serde_json::to_vec_pretty(pins)?)?;
        Ok(())
    }

    /// Whether `uid` is pinned (independent of whether its blob is currently
    /// cached).
    pub fn is_pinned(&self, uid: &NodeUid) -> bool {
        self.load_pins().pins.contains_key(&uid.to_string())
    }

    /// Record `uid` as pinned under `path`. The caller is responsible for having
    /// already cached the content via [`store`](Self::store).
    pub fn add_pin(&self, uid: &NodeUid, path: &Path) -> Result<()> {
        let mut file = self.load_pins();
        let key = uid.to_string();
        file.pins.insert(
            key.clone(),
            Pin {
                uid: key,
                path: path.display().to_string(),
            },
        );
        self.save_pins(&file)
    }

    /// Drop `uid` from the pin registry and evict its blob. No-op if not pinned.
    pub fn remove_pin(&self, uid: &NodeUid) -> Result<()> {
        let mut file = self.load_pins();
        if file.pins.remove(&uid.to_string()).is_some() {
            self.save_pins(&file)?;
        }
        self.evict(uid);
        Ok(())
    }

    /// All pinned files, ordered by uid.
    pub fn list_pins(&self) -> Vec<Pin> {
        self.load_pins().pins.into_values().collect()
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
        self.max_bytes
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
        let c = ContentCache::open(
            dir.path().join("content"),
            dir.path().join("pins.json"),
            max_bytes,
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
    fn pin_lifecycle_evicts_on_unpin() {
        let (c, _d) = cache();
        let u = uid("a");
        c.store(&u, 100, 3, b"abc").unwrap();
        c.add_pin(&u, Path::new("docs/a.txt")).unwrap();
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
        c.add_pin(&a, Path::new("a")).unwrap();
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
