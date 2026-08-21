//! The photos timeline and its thumbnails.
//!
//! The timeline is a flat, newest-first projection of the photo share, persisted
//! by [`pdfs_core::db`] so the gallery opens instantly and refreshes behind the
//! user. Thumbnails the server has none for (anything a camera wrote rather than
//! a phone) are generated locally and stored as if the server had served them.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use pdfs_core::control::{
    FileThumbRequest, PhotoItem, PhotoKind, PhotoThumb, ThumbnailBuildStatus, is_raw_image_name,
    is_thumbnail_image_name,
};
use pdfs_core::db::{self, StoredPhoto};
use pdfs_core::{CoreError, CoreResult};

use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{NodeKind, PhotoTag, PhotoTagsUpdate, ThumbnailType};
use tracing::{info, warn};

use super::{
    Core, PHOTOS_AVAILABLE, PHOTOS_SYNCED_MS, TIMELINE_ENRICH_CHUNK, TIMELINE_TTL, node_size,
    now_ms, parse_uid,
};

/// Longest edge, in px, of a thumbnail generated locally for a photo the server
/// has none for. Matches the server's own thumbnail scale closely enough that a
/// tile can't tell them apart.
const THUMB_EDGE: u32 = 512;
/// JPEG quality of a locally generated thumbnail.
const THUMB_QUALITY: u8 = 82;
/// How many photos may be downloaded at once to generate their missing
/// thumbnails. Bounded: a screenful of 20 MB digicam JPEGs would otherwise
/// saturate the link and starve the rest of the daemon.
pub(crate) const THUMB_GEN_CONCURRENCY: usize = 4;
/// Images processed between recursive-build progress updates.
const THUMB_BUILD_CHUNK: usize = 16;
/// A recursive build may wait briefly for an opportunistic tile job that
/// already owns a uid, but never forever.
const THUMB_BUILD_CLAIM_TIMEOUT: Duration = Duration::from_secs(30);
/// Backstop for cancellation if a notification races with waiter registration.
const THUMB_CANCEL_POLL: Duration = Duration::from_millis(100);
/// A broken metadata helper must not hold one of the four thumbnail permits
/// indefinitely. Matches the deliberately generous KDE RAW thumbnail budget.
const EXIFTOOL_TIMEOUT: Duration = Duration::from_secs(20);
static RAW_TEMP_NONCE: AtomicU64 = AtomicU64::new(0);
static EXIFTOOL_MISSING_WARNING: Once = Once::new();

/// The aspect ratio (w/h) of an encoded image, read from its header alone — no
/// pixels are decoded. `None` when the format is unknown or the header is torn.
fn ratio_of(bytes: &[u8]) -> Option<f64> {
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    (height > 0).then(|| f64::from(width) / f64::from(height))
}

/// A thumbnail the daemon made itself, and the aspect ratio of the photo it was
/// made from (free at that point — the full image had to be decoded anyway).
struct GeneratedThumb {
    bytes: Vec<u8>,
    ratio: f64,
}

/// Scaling distinguishes a permanent content verdict from an environmental
/// failure. Only `Undecodable` may enter the local negative cache.
enum ScaleAttempt {
    Made(GeneratedThumb),
    Undecodable,
    Unavailable,
}

#[derive(Debug, Clone, Copy)]
struct RawPreviewUnavailable;

/// How one attempt at generating a missing thumbnail ended. The distinction that
/// matters is *permanent* versus *transient*: only bytes we cannot decode prove
/// the photo will never have a thumbnail, and only that verdict is persisted.
enum ThumbAttempt {
    Made(GeneratedThumb),
    /// Decoded nothing — a format this build has no decoder for. Permanent.
    Undecodable,
    /// The photo couldn't be downloaded. Transient: try again next time.
    Unavailable,
    /// Its ordinary-file listing was left while this work was queued or running.
    Cancelled,
}

/// Why a local thumbnail is being generated. Only opportunistic thumbnails for
/// ordinary file listings are cancellable on navigation; Photos and an explicit
/// recursive build are intentional work with their own lifecycle.
#[derive(Clone, Copy)]
enum ThumbJob {
    Photos,
    Files(u64),
    Build,
}

#[derive(Default)]
struct ThumbBatchSummary {
    made: u64,
    undecodable: u64,
    unavailable: u64,
    cancelled: u64,
}

impl ThumbBatchSummary {
    fn processed(&self) -> u64 {
        self.made + self.undecodable + self.unavailable + self.cancelled
    }

    fn failed(&self) -> u64 {
        self.undecodable + self.unavailable
    }
}

/// `true` means publish a new build, `false` means the caller may attach to the
/// already-running build for the same root. A different root is never silently
/// accepted as if it were the requested job.
fn thumbnail_build_may_start(status: &ThumbnailBuildStatus, path: &str) -> CoreResult<bool> {
    if !status.running {
        return Ok(true);
    }
    if status.path == path {
        return Ok(false);
    }
    Err(CoreError::invalid(format!(
        "a thumbnail build is already running for {}",
        if status.path.is_empty() {
            "Proton Drive"
        } else {
            &status.path
        }
    )))
}

/// Scale a full-size photo down to a thumbnail: at most [`THUMB_EDGE`] on its
/// longest side, JPEG, aspect ratio preserved. Environmental RAW-extraction
/// failures remain retryable; only bytes conclusively outside the supported
/// formats return an undecodable verdict.
///
/// CPU-bound (a 20 MP JPEG is real work), so callers run it on the blocking pool.
fn scale_thumbnail(bytes: &[u8], name: &str, staging_dir: &Path) -> ScaleAttempt {
    scale_thumbnail_with_exiftool(
        bytes,
        name,
        staging_dir,
        OsStr::new("exiftool"),
        EXIFTOOL_TIMEOUT,
    )
}

fn scale_thumbnail_with_exiftool(
    bytes: &[u8],
    name: &str,
    staging_dir: &Path,
    exiftool: &OsStr,
    timeout: Duration,
) -> ScaleAttempt {
    let direct = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.decode().ok());
    let image = match direct {
        Some(image) => image,
        None if is_raw_image_name(name) => {
            let (preview, orientation) =
                match extract_raw_preview(bytes, name, staging_dir, exiftool, timeout) {
                    Ok(Some(preview)) => preview,
                    Ok(None) => return ScaleAttempt::Undecodable,
                    Err(_) => return ScaleAttempt::Unavailable,
                };
            let Some(mut image) = image::ImageReader::new(std::io::Cursor::new(preview))
                .with_guessed_format()
                .ok()
                .and_then(|reader| reader.decode().ok())
            else {
                return ScaleAttempt::Undecodable;
            };
            if let Some(orientation) = image::metadata::Orientation::from_exif(orientation) {
                image.apply_orientation(orientation);
            }
            image
        }
        None => return ScaleAttempt::Undecodable,
    };
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return ScaleAttempt::Undecodable;
    }
    let ratio = f64::from(width) / f64::from(height);

    // `thumbnail` fits the image *inside* the box, so the longest edge lands on
    // THUMB_EDGE and the ratio is untouched.
    let thumb = image.thumbnail(THUMB_EDGE, THUMB_EDGE).to_rgb8();
    let mut bytes = Vec::new();
    if image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, THUMB_QUALITY)
        .encode_image(&thumb)
        .is_err()
    {
        return ScaleAttempt::Unavailable;
    }
    ScaleAttempt::Made(GeneratedThumb { bytes, ratio })
}

/// Temporary file passed to exiftool. The helper identifies most RAWs from their
/// magic bytes, while preserving a safe extension also covers formats whose
/// detector uses the original name as a hint.
struct RawTempFile(PathBuf);

impl RawTempFile {
    fn create_in(staging_dir: &Path, bytes: &[u8], name: &str) -> Option<Self> {
        let extension = std::path::Path::new(name)
            .extension()
            .and_then(|value| value.to_str())
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 10
                    && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .unwrap_or("raw");
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..16 {
            let nonce = RAW_TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
            let path = staging_dir.join(format!(
                "pdfs-raw-{}-{started}-{nonce}.{extension}",
                std::process::id(),
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    if file.write_all(bytes).is_ok() {
                        return Some(Self(path));
                    }
                    let _ = std::fs::remove_file(path);
                    return None;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for RawTempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run one bounded exiftool JSON query while draining stdout concurrently. The
/// latter matters for full-size `JpgFromRaw` fallbacks, whose base64 can exceed a
/// pipe buffer long before the child exits.
fn exiftool_query(
    exiftool: &OsStr,
    path: &Path,
    tags: &[&str],
    timeout: Duration,
) -> Result<serde_json::Value, RawPreviewUnavailable> {
    let mut command = Command::new(exiftool);
    command.args(["-j", "-b", "-n"]);
    command.args(tags);
    command
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if error.kind() == std::io::ErrorKind::NotFound {
                EXIFTOOL_MISSING_WARNING.call_once(|| {
                    warn!(
                        "exiftool is not installed; camera RAW thumbnails are unavailable \
                         (Debian/Ubuntu: libimage-exiftool-perl)"
                    );
                });
            }
            return Err(RawPreviewUnavailable);
        }
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(RawPreviewUnavailable);
    };
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let started = Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let output = reader
        .join()
        .ok()
        .and_then(Result::ok)
        .ok_or(RawPreviewUnavailable)?;
    if !success {
        return Err(RawPreviewUnavailable);
    }
    serde_json::from_slice(&output).map_err(|_| RawPreviewUnavailable)
}

fn exiftool_object(
    value: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value.as_array()?.first()?.as_object()
}

fn exiftool_binary(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<Vec<u8>> {
    let encoded = object.get(key)?.as_str()?.strip_prefix("base64:")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    (!decoded.is_empty()).then_some(decoded)
}

/// Extract the embedded display preview from one concrete camera RAW. PreviewImage
/// is queried alone first so common files do not also serialize a discarded
/// full-resolution JpgFromRaw; rarer preview tags are requested only on a miss.
fn extract_raw_preview(
    bytes: &[u8],
    name: &str,
    staging_dir: &Path,
    exiftool: &OsStr,
    timeout: Duration,
) -> Result<Option<(Vec<u8>, u8)>, RawPreviewUnavailable> {
    let file = RawTempFile::create_in(staging_dir, bytes, name).ok_or(RawPreviewUnavailable)?;
    let primary = exiftool_query(
        exiftool,
        &file.0,
        &["-Orientation", "-PreviewImage"],
        timeout,
    )?;
    let primary = exiftool_object(&primary).ok_or(RawPreviewUnavailable)?;
    let orientation = primary
        .get("Orientation")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or(1);
    if let Some(preview) = exiftool_binary(primary, "PreviewImage") {
        return Ok(Some((preview, orientation)));
    }

    const FALLBACKS: [&str; 5] = [
        "OtherImage",
        "JpgFromRaw",
        "PreviewTIFF",
        "ThumbnailImage",
        "ThumbnailTIFF",
    ];
    let fallback = exiftool_query(
        exiftool,
        &file.0,
        &[
            "-OtherImage",
            "-JpgFromRaw",
            "-PreviewTIFF",
            "-ThumbnailImage",
            "-ThumbnailTIFF",
        ],
        timeout,
    )?;
    let fallback = exiftool_object(&fallback).ok_or(RawPreviewUnavailable)?;
    Ok(FALLBACKS
        .iter()
        .find_map(|key| exiftool_binary(fallback, key))
        .map(|preview| (preview, orientation)))
}

/// Formats supported by the local thumbnail decoder and exposed by the GUI.
impl Core {
    pub(crate) fn photos_timeline(
        &self,
        offset: usize,
        limit: usize,
        kind: Option<PhotoKind>,
        range: Option<(i64, i64)>,
        favorites: bool,
    ) -> CoreResult<Option<Vec<PhotoItem>>> {
        let count = self.db.photos_count().map_err(CoreError::from)?;
        if count == 0 {
            // Nothing to serve, so this one request has to wait for the fetch —
            // unless we already know the account has no photos volume and the
            // answer is a fresh "no".
            let known_empty = self.db.state_i64(PHOTOS_AVAILABLE).ok().flatten() == Some(0);
            if known_empty && !self.listing_stale(PHOTOS_SYNCED_MS, TIMELINE_TTL) {
                return Ok(None);
            }
            if !self.rt.block_on(self.refresh_timeline())? {
                return Ok(None);
            }
        } else if self.listing_stale(PHOTOS_SYNCED_MS, TIMELINE_TTL) {
            self.spawn_timeline_refresh();
        }

        let page = self
            .db
            .photos_page(offset, limit, kind, range, favorites)
            .map_err(CoreError::from)?;
        Ok(Some(page.into_iter().map(|p| self.photo_item(p)).collect()))
    }

    /// Project a persisted photo into the wire item the front-end paints: its
    /// learned aspect ratio, its thumbnail verdict, and the on-disk path of its
    /// thumbnail when one is cached (tagged with the capture time, which is the
    /// only revision handle a photo carries).
    pub(crate) fn photo_item(&self, photo: StoredPhoto) -> PhotoItem {
        let thumb_path = parse_uid(&photo.uid).and_then(|uid| {
            self.cache
                .cached_thumbnail_path(&uid, ThumbnailType::Thumbnail.as_i32(), photo.capture_time)
                .map(|p| p.display().to_string())
        });
        PhotoItem {
            uid: photo.uid,
            capture_time: photo.capture_time,
            thumb_path,
            name: photo.name,
            ratio: photo.ratio,
            no_thumb: photo.thumb_state == db::THUMB_NONE,
            kind: photo.kind,
            favorite: photo.favorite,
        }
    }

    /// Add or remove Proton's `Favorite` tag on a photo, then record the change
    /// locally so the gallery reflects it without waiting for a timeline refresh.
    ///
    /// The SDK reports one outcome per photo rather than failing the call, so the
    /// single outcome is unwrapped back into this request's result.
    pub(crate) fn set_photo_favorite(&self, uid: &NodeUid, favorite: bool) -> CoreResult<()> {
        let photos = self.photos();
        let update = PhotoTagsUpdate {
            node_uid: uid.clone(),
            tags_to_add: if favorite {
                vec![PhotoTag::Favorite]
            } else {
                Vec::new()
            },
            tags_to_remove: if favorite {
                Vec::new()
            } else {
                vec![PhotoTag::Favorite]
            },
        };
        let outcomes = self
            .rt
            .block_on(photos.update_photos(std::slice::from_ref(&update)))
            .map_err(|e| CoreError::from_api(&e, "update photo tags"))?;
        pdfs_core::batch::into_unit(outcomes)
            .map_err(|e| CoreError::from_api(&e, "update photo tags"))?;
        self.db
            .photos_set_favorite(&uid.to_string(), favorite)
            .map_err(CoreError::from)?;
        Ok(())
    }

    /// Thumbnails for `uids`, served from the cache, fetched from the server for
    /// whatever is missing, and — for the photos the server has no thumbnail for
    /// at all — *generated locally* from the full file (see
    /// [`Core::generate_thumbs`]). Requested on demand as tiles scroll into view,
    /// so a cold timeline paints immediately and only the photos actually on
    /// screen cost a round-trip.
    ///
    /// A photo absent from the persisted timeline is skipped: its capture time is
    /// the cache's validity tag, and guessing that would poison the cache.
    pub(crate) fn photo_thumbs(&self, uids: &[NodeUid]) -> Vec<PhotoThumb> {
        let ttype = ThumbnailType::Thumbnail.as_i32();
        let keys: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
        let mut stored = self.db.photos_by_uid(&keys).unwrap_or_default();
        // Album covers and the contents of an album shared with us are not in our
        // own timeline, so their capture time — the cache's validity tag — only
        // exists on the album rows. Fill in whatever the timeline didn't cover.
        {
            let have: HashSet<String> = stored.iter().map(|p| p.uid.clone()).collect();
            let missing: Vec<String> = keys
                .iter()
                .filter(|k| !have.contains(*k))
                .cloned()
                .collect();
            stored.extend(self.db.album_photos_by_uid(&missing).unwrap_or_default());
        }
        let stored = stored;
        let tags: HashMap<String, i64> = stored
            .iter()
            .map(|p| (p.uid.clone(), p.capture_time))
            .collect();
        let names: HashMap<String, String> = stored
            .iter()
            .filter_map(|photo| photo.name.clone().map(|name| (photo.uid.clone(), name)))
            .collect();

        // Ask the server only for photos that are missing a cached thumbnail and
        // haven't already been written off as un-thumbnailable.
        let want: Vec<NodeUid> = uids
            .iter()
            .filter(|uid| {
                let key = uid.to_string();
                stored
                    .iter()
                    .find(|p| p.uid == key)
                    .is_some_and(|p| p.thumb_state != db::THUMB_NONE)
                    && tags.get(&key).is_some_and(|&tag| {
                        self.cache.cached_thumbnail_path(uid, ttype, tag).is_none()
                    })
            })
            .cloned()
            .collect();

        if !want.is_empty() {
            let mut missing = want.clone();
            match self.rt.block_on(
                self.photos()
                    .enumerate_thumbnails(&want, ThumbnailType::Thumbnail),
            ) {
                Ok(thumbs) => {
                    for ft in thumbs {
                        let Some(&tag) = tags.get(&ft.file_uid.to_string()) else {
                            continue;
                        };
                        let Ok(bytes) = ft.result else { continue };
                        if bytes.is_empty() {
                            continue;
                        }
                        if self
                            .cache
                            .store_thumbnail(&ft.file_uid, ttype, tag, &bytes)
                            .is_ok()
                        {
                            missing.retain(|uid| uid != &ft.file_uid);
                            self.record_thumb(&ft.file_uid, db::THUMB_HAVE, ratio_of(&bytes));
                        }
                    }
                }
                // A failed batch is not a verdict: leave every uid in `missing` so
                // the local fallback still gives those tiles an image.
                Err(e) => warn!(error = %e, "batch photo thumbnails failed"),
            }

            // Whatever the server had nothing for gets a thumbnail made from its
            // own bytes — this is what fills in camera photos uploaded by clients
            // that never generated one. Off the request path: a full-size photo
            // takes far longer to fetch than the whole rest of the batch, and the
            // thumbnails that *are* ready must not wait behind it.
            if !missing.is_empty() {
                self.spawn_generate_thumbs(missing, &tags, &names, ThumbJob::Photos);
            }
        }

        let pending = self.thumb_gen.lock();
        uids.iter()
            .map(|uid| PhotoThumb {
                uid: uid.to_string(),
                path: tags.get(&uid.to_string()).and_then(|&tag| {
                    self.cache
                        .cached_thumbnail_path(uid, ttype, tag)
                        .map(|p| p.display().to_string())
                }),
                pending: pending.contains(uid),
            })
            .collect()
    }

    /// Thumbnails for image files shown by the regular file browser, Shared and
    /// Trash pages. Unlike [`Core::photo_thumbs`], these nodes need not be in the
    /// Photos timeline: the request supplies each file's modification time as
    /// the cache validity tag.
    pub(crate) fn file_thumbs(
        &self,
        items: &[FileThumbRequest],
        generation: u64,
    ) -> Option<Vec<PhotoThumb>> {
        let current =
            generation != 0 && self.file_thumb_generation.load(Ordering::SeqCst) == generation;
        if !current {
            return None;
        }
        let ttype = ThumbnailType::Thumbnail.as_i32();
        let parsed: Vec<(String, NodeUid, i64)> = items
            .iter()
            .filter_map(|item| {
                parse_uid(&item.uid).map(|uid| (item.uid.clone(), uid, item.modified))
            })
            .collect();
        let tags: HashMap<String, i64> = parsed
            .iter()
            .map(|(raw, _, modified)| (raw.clone(), *modified))
            .collect();
        let names: HashMap<String, String> = items
            .iter()
            .map(|item| (item.uid.clone(), item.name.clone()))
            .collect();

        // Proton only stores server-generated thumbnails for nodes in Photos.
        // These requests come exclusively from ordinary Drive, Shared and Trash
        // views, so downloading a remote thumbnail here would be a guaranteed
        // miss. Generate directly from the full image and cache the result.
        let mut wanted = Vec::new();
        let mut seen = HashSet::new();
        for (raw, uid, modified) in &parsed {
            if self
                .cache
                .cached_thumbnail_path(uid, ttype, *modified)
                .is_some()
                || self
                    .thumbnail_misses
                    .lock()
                    .local_contains(&(uid.clone(), ttype), *modified)
                || !seen.insert(raw.clone())
            {
                continue;
            }
            wanted.push(uid.clone());
        }

        if !wanted.is_empty() {
            self.spawn_generate_thumbs(wanted, &tags, &names, ThumbJob::Files(generation));
        }

        let pending = self.thumb_gen.lock();
        Some(
            items
                .iter()
                .map(|item| {
                    let uid = parse_uid(&item.uid);
                    PhotoThumb {
                        uid: item.uid.clone(),
                        path: uid.as_ref().and_then(|uid| {
                            self.cache
                                .cached_thumbnail_path(uid, ttype, item.modified)
                                .map(|path| path.display().to_string())
                        }),
                        pending: uid.as_ref().is_some_and(|uid| pending.contains(uid)),
                    }
                })
                .collect(),
        )
    }

    /// Reserve the next ordinary-file listing generation. This daemon-owned
    /// monotonic value replaces GUI wall-clock seeds, which can move backwards
    /// across NTP corrections, suspend, or dual boot.
    pub(crate) fn reserve_file_thumb_generation(&self) -> u64 {
        let next = |current| if current == u64::MAX { 1 } else { current + 1 };
        let previous = self
            .file_thumb_generation
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(next(current))
            })
            .expect("generation update always succeeds");
        let generation = next(previous);
        self.file_thumb_cancel.notify_waiters();
        generation
    }

    /// Advance the ordinary-file thumbnail generation. Every queued/download in
    /// an older listing observes the new value and exits; Photos and explicit
    /// recursive builds do not use this generation and continue normally.
    pub(crate) fn cancel_file_thumbs(&self, generation: u64) {
        let previous = self
            .file_thumb_generation
            .fetch_max(generation, Ordering::SeqCst);
        if generation > previous {
            self.file_thumb_cancel.notify_waiters();
        }
    }

    /// Generate the missing thumbnails on the runtime, skipping any photo already
    /// being generated. The uids are marked in-flight before the task starts, so
    /// the reply this call is about to send already reports them as pending.
    fn spawn_generate_thumbs(
        &self,
        uids: Vec<NodeUid>,
        tags: &HashMap<String, i64>,
        names: &HashMap<String, String>,
        job: ThumbJob,
    ) {
        let fresh: Vec<NodeUid> = {
            let mut inflight = self.thumb_gen.lock();
            uids.into_iter()
                .filter(|uid| inflight.insert(uid.clone()))
                .collect()
        };
        if fresh.is_empty() {
            return;
        }

        let core = self.clone();
        let tags = tags.clone();
        let names = names.clone();
        // `generate_thumbs` blocks on the runtime itself, so it belongs on the
        // blocking pool rather than on an async worker.
        self.rt.spawn_blocking(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                core.generate_thumbs(&fresh, &tags, &names, job)
            }));
            let mut inflight = core.thumb_gen.lock();
            for uid in &fresh {
                inflight.remove(uid);
            }
            drop(inflight);
            if result.is_err() {
                warn!("thumbnail generation worker panicked");
            }
        });
    }

    /// Make thumbnails for photos the server has none for: download each full
    /// file once, scale it to [`THUMB_EDGE`], and store the result in the thumbnail
    /// cache exactly as if the server had served it.
    ///
    /// Bounded by [`THUMB_GEN_CONCURRENCY`] — these are full-size originals, and a
    /// screenful of them at once would saturate the link. A photo whose bytes
    /// can't be decoded (a codec `image` doesn't speak) is marked
    /// [`db::THUMB_NONE`] and never attempted again.
    fn thumb_job_current(&self, job: ThumbJob) -> bool {
        match job {
            ThumbJob::Files(generation) => {
                self.file_thumb_generation.load(Ordering::SeqCst) == generation
            }
            ThumbJob::Photos => true,
            ThumbJob::Build => !self.thumbnail_build_cancelled.load(Ordering::SeqCst),
        }
    }

    fn generate_thumbs(
        &self,
        uids: &[NodeUid],
        tags: &HashMap<String, i64>,
        names: &HashMap<String, String>,
        job: ThumbJob,
    ) -> ThumbBatchSummary {
        info!(count = uids.len(), "generating local thumbnails");
        let results: Vec<(NodeUid, ThumbAttempt)> = self.rt.block_on(async {
            let mut out = Vec::with_capacity(uids.len());
            for chunk in uids.chunks(THUMB_GEN_CONCURRENCY) {
                let mut set = tokio::task::JoinSet::new();
                for uid in chunk {
                    let core = self.clone();
                    let client = self.client.clone();
                    let uid = uid.clone();
                    let name = names.get(&uid.to_string()).cloned().unwrap_or_default();
                    set.spawn(async move {
                        if !core.thumb_job_current(job) {
                            return (uid, ThumbAttempt::Cancelled);
                        }

                        // One shared permit pool bounds all thumbnail batches,
                        // rather than multiplying the limit by every control
                        // request the GUI happens to have in flight.
                        let acquire = core.thumb_gen_budget.clone().acquire_owned();
                        tokio::pin!(acquire);
                        let _permit = match job {
                            ThumbJob::Files(_) => loop {
                                tokio::select! {
                                    permit = &mut acquire => break permit.ok(),
                                    _ = core.file_thumb_cancel.notified() => {
                                        if !core.thumb_job_current(job) {
                                            return (uid, ThumbAttempt::Cancelled);
                                        }
                                    }
                                    _ = tokio::time::sleep(THUMB_CANCEL_POLL) => {
                                        if !core.thumb_job_current(job) {
                                            return (uid, ThumbAttempt::Cancelled);
                                        }
                                    }
                                }
                            },
                            ThumbJob::Build => loop {
                                tokio::select! {
                                    permit = &mut acquire => break permit.ok(),
                                    _ = core.thumbnail_build_cancel.notified() => {
                                        if !core.thumb_job_current(job) {
                                            return (uid, ThumbAttempt::Cancelled);
                                        }
                                    }
                                    _ = tokio::time::sleep(THUMB_CANCEL_POLL) => {
                                        if !core.thumb_job_current(job) {
                                            return (uid, ThumbAttempt::Cancelled);
                                        }
                                    }
                                }
                            },
                            ThumbJob::Photos => acquire.await.ok(),
                        };
                        let Some(_permit) = _permit else {
                            return (uid, ThumbAttempt::Unavailable);
                        };
                        if !core.thumb_job_current(job) {
                            return (uid, ThumbAttempt::Cancelled);
                        }

                        let download_uid = uid.clone();
                        let download = async move { client.download_file(&download_uid).await };
                        tokio::pin!(download);
                        let downloaded = match job {
                            ThumbJob::Files(_) => loop {
                                tokio::select! {
                                    result = &mut download => break Some(result),
                                    _ = core.file_thumb_cancel.notified() => {
                                        if !core.thumb_job_current(job) {
                                            break None;
                                        }
                                    }
                                    _ = tokio::time::sleep(THUMB_CANCEL_POLL) => {
                                        if !core.thumb_job_current(job) {
                                            break None;
                                        }
                                    }
                                }
                            },
                            ThumbJob::Build => loop {
                                tokio::select! {
                                    result = &mut download => break Some(result),
                                    _ = core.thumbnail_build_cancel.notified() => {
                                        if !core.thumb_job_current(job) {
                                            break None;
                                        }
                                    }
                                    _ = tokio::time::sleep(THUMB_CANCEL_POLL) => {
                                        if !core.thumb_job_current(job) {
                                            break None;
                                        }
                                    }
                                }
                            },
                            ThumbJob::Photos => Some(download.await),
                        };
                        let Some(downloaded) = downloaded else {
                            return (uid, ThumbAttempt::Cancelled);
                        };
                        let bytes = match downloaded {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                warn!(%uid, error = %e, "image download for thumbnail failed");
                                return (uid, ThumbAttempt::Unavailable);
                            }
                        };
                        if !core.thumb_job_current(job) {
                            return (uid, ThumbAttempt::Cancelled);
                        }
                        // Decoding + scaling a 20 MP JPEG is CPU-bound and would
                        // stall the runtime's worker; hand it to the blocking pool.
                        let staging_dir = core.cache.raw_thumbnail_staging_dir().to_path_buf();
                        let made = tokio::task::spawn_blocking(move || {
                            scale_thumbnail(&bytes, &name, &staging_dir)
                        })
                        .await
                        .unwrap_or(ScaleAttempt::Unavailable);
                        if !core.thumb_job_current(job) {
                            return (uid, ThumbAttempt::Cancelled);
                        }
                        match made {
                            ScaleAttempt::Made(thumb) => (uid, ThumbAttempt::Made(thumb)),
                            ScaleAttempt::Undecodable => (uid, ThumbAttempt::Undecodable),
                            ScaleAttempt::Unavailable => (uid, ThumbAttempt::Unavailable),
                        }
                    });
                }
                while let Some(joined) = set.join_next().await {
                    if let Ok(result) = joined {
                        out.push(result);
                    }
                }
            }
            out
        });

        let ttype = ThumbnailType::Thumbnail.as_i32();
        let mut summary = ThumbBatchSummary::default();
        for (uid, attempt) in results {
            match attempt {
                ThumbAttempt::Made(thumb) => {
                    let Some(&tag) = tags.get(&uid.to_string()) else {
                        summary.unavailable += 1;
                        continue;
                    };
                    match self.cache.store_thumbnail(&uid, ttype, tag, &thumb.bytes) {
                        Ok(()) => {
                            summary.made += 1;
                            self.thumbnail_misses
                                .lock()
                                .forget_local(&(uid.clone(), ttype));
                            self.record_thumb(&uid, db::THUMB_HAVE, Some(thumb.ratio));
                        }
                        Err(e) => {
                            summary.unavailable += 1;
                            warn!(%uid, error = %e, "storing generated thumbnail failed");
                        }
                    }
                }
                // The photo's own bytes aren't an image we can read: no thumbnail
                // will ever exist for it, so stop trying.
                ThumbAttempt::Undecodable => {
                    summary.undecodable += 1;
                    if let Some(&tag) = tags.get(&uid.to_string()) {
                        self.thumbnail_misses
                            .lock()
                            .remember_local((uid.clone(), ttype), tag);
                    }
                    self.record_thumb(&uid, db::THUMB_NONE, None);
                }
                // The download failed — a dropped connection, an expired link. That
                // is not a verdict on the photo: leave its state alone so the next
                // scroll past it tries again.
                ThumbAttempt::Unavailable => summary.unavailable += 1,
                ThumbAttempt::Cancelled => summary.cancelled += 1,
            }
        }
        summary
    }

    pub(crate) fn thumbnail_build_status(&self) -> ThumbnailBuildStatus {
        self.thumbnail_build.lock().clone()
    }

    /// Start one recursive local-thumbnail build. A request for the same root
    /// may attach to it; a different root is rejected explicitly instead of
    /// silently pretending the existing job belongs to the caller.
    pub(crate) fn start_thumbnail_build(&self, root: PathBuf) -> CoreResult<ThumbnailBuildStatus> {
        let path = root.to_string_lossy().into_owned();
        let status = {
            let mut status = self.thumbnail_build.lock();
            if !thumbnail_build_may_start(&status, &path)? {
                return Ok(status.clone());
            }
            self.thumbnail_build_cancelled
                .store(false, Ordering::SeqCst);
            *status = ThumbnailBuildStatus {
                running: true,
                scanning: true,
                path: path.clone(),
                folders_scanned: 0,
                images_found: 0,
                completed: 0,
                failed: 0,
                message: None,
            };
            status.clone()
        };

        let core = self.clone();
        self.rt.spawn_blocking(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                core.run_thumbnail_build(root)
            }))
            .is_err()
            {
                let mut status = core.thumbnail_build.lock();
                status.running = false;
                status.scanning = false;
                status.message = Some("Thumbnail generation stopped unexpectedly".to_string());
                core.thumbnail_build_cancelled
                    .store(false, Ordering::SeqCst);
                warn!(path = %status.path, "recursive thumbnail build panicked");
            }
        });
        Ok(status)
    }

    pub(crate) fn cancel_thumbnail_build(&self) -> ThumbnailBuildStatus {
        let (status, cancelled) = {
            let status = self.thumbnail_build.lock();
            let cancelled = status.running;
            if cancelled {
                self.thumbnail_build_cancelled.store(true, Ordering::SeqCst);
            }
            (status.clone(), cancelled)
        };
        if cancelled {
            self.thumbnail_build_cancel.notify_waiters();
        }
        status
    }

    fn finish_thumbnail_build(&self) {
        let mut status = self.thumbnail_build.lock();
        let cancelled = self.thumbnail_build_cancelled.swap(false, Ordering::SeqCst);
        status.running = false;
        status.scanning = false;
        if cancelled {
            status.message = Some("Thumbnail build cancelled".to_string());
        }
        info!(
            path = %status.path,
            images = status.images_found,
            failed = status.failed,
            cancelled,
            "recursive thumbnail build finished"
        );
    }

    fn run_thumbnail_build(&self, root: PathBuf) {
        let mut folders = vec![root];
        let mut images: Vec<(NodeUid, i64, String)> = Vec::new();
        let mut seen_folders = HashSet::new();
        let mut seen_images = HashSet::new();

        while let Some(folder) = folders.pop() {
            if self.thumbnail_build_cancelled.load(Ordering::SeqCst) {
                self.finish_thumbnail_build();
                return;
            }
            let listing = self.list_dir(&folder);
            {
                let mut status = self.thumbnail_build.lock();
                status.folders_scanned += 1;
            }
            let entries = match listing {
                Ok(entries) => entries,
                Err(error) => {
                    let mut status = self.thumbnail_build.lock();
                    if status.message.is_none() {
                        status.message = Some(format!(
                            "Some folders could not be read ({}: {error})",
                            folder.display()
                        ));
                    }
                    continue;
                }
            };

            let mut found_here = 0_u64;
            let mut invalid_here = 0_u64;
            for entry in entries {
                if entry.is_dir {
                    if seen_folders.insert(entry.uid) {
                        folders.push(folder.join(entry.name));
                    }
                } else if is_thumbnail_image_name(&entry.name) {
                    match parse_uid(&entry.uid) {
                        Some(uid) if seen_images.insert(uid.clone()) => {
                            found_here += 1;
                            images.push((uid, entry.modified, entry.name));
                        }
                        Some(_) => {}
                        None => {
                            found_here += 1;
                            invalid_here += 1;
                        }
                    }
                }
            }
            let mut status = self.thumbnail_build.lock();
            status.images_found += found_here;
            status.completed += invalid_here;
            status.failed += invalid_here;
        }

        self.thumbnail_build.lock().scanning = false;
        if self.thumbnail_build_cancelled.load(Ordering::SeqCst) {
            self.finish_thumbnail_build();
            return;
        }
        let ttype = ThumbnailType::Thumbnail.as_i32();
        let mut todo = Vec::new();
        for (uid, modified, name) in images {
            if self
                .cache
                .cached_thumbnail_path(&uid, ttype, modified)
                .is_some()
            {
                self.thumbnail_build.lock().completed += 1;
            } else if self
                .thumbnail_misses
                .lock()
                .local_contains(&(uid.clone(), ttype), modified)
            {
                let mut status = self.thumbnail_build.lock();
                status.completed += 1;
                status.failed += 1;
            } else {
                todo.push((uid, modified, name));
            }
        }

        for chunk in todo.chunks(THUMB_BUILD_CHUNK) {
            if self.thumbnail_build_cancelled.load(Ordering::SeqCst) {
                self.finish_thumbnail_build();
                return;
            }
            let mut claimed = Vec::with_capacity(chunk.len());
            let mut tags = HashMap::with_capacity(chunk.len());
            let mut names = HashMap::with_capacity(chunk.len());
            for (uid, modified, name) in chunk {
                // A visible tile may have started this image just before the
                // toolbar action. Wait for that one bounded job to finish rather
                // than downloading the full image twice.
                let waiting_since = Instant::now();
                loop {
                    if self.thumbnail_build_cancelled.load(Ordering::SeqCst) {
                        self.finish_thumbnail_build();
                        return;
                    }
                    if self
                        .cache
                        .cached_thumbnail_path(uid, ttype, *modified)
                        .is_some()
                    {
                        self.thumbnail_build.lock().completed += 1;
                        break;
                    }
                    if self
                        .thumbnail_misses
                        .lock()
                        .local_contains(&(uid.clone(), ttype), *modified)
                    {
                        let mut status = self.thumbnail_build.lock();
                        status.completed += 1;
                        status.failed += 1;
                        break;
                    }
                    if self.thumb_gen.lock().insert(uid.clone()) {
                        tags.insert(uid.to_string(), *modified);
                        names.insert(uid.to_string(), name.clone());
                        claimed.push(uid.clone());
                        break;
                    }
                    if waiting_since.elapsed() >= THUMB_BUILD_CLAIM_TIMEOUT {
                        let mut status = self.thumbnail_build.lock();
                        status.completed += 1;
                        status.failed += 1;
                        if status.message.is_none() {
                            status.message = Some(
                                "Some images stayed busy and were skipped after a timeout".into(),
                            );
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }

            if claimed.is_empty() {
                continue;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.generate_thumbs(&claimed, &tags, &names, ThumbJob::Build)
            }));
            {
                let mut inflight = self.thumb_gen.lock();
                for uid in &claimed {
                    inflight.remove(uid);
                }
            }
            let mut status = self.thumbnail_build.lock();
            match result {
                Ok(summary) => {
                    status.completed += summary.processed();
                    status.failed += summary.failed();
                }
                Err(_) => {
                    status.completed += claimed.len() as u64;
                    status.failed += claimed.len() as u64;
                    status.message = Some("Some thumbnail workers stopped unexpectedly".into());
                }
            }
        }
        self.finish_thumbnail_build();
    }

    /// Persist what a thumbnail attempt learned about a photo, against the
    /// timeline and against every album the photo sits in — one attempt, one
    /// verdict, wherever the photo is painted.
    pub(crate) fn record_thumb(&self, uid: &NodeUid, state: i64, ratio: Option<f64>) {
        let key = uid.to_string();
        if let Err(e) = self.db.photo_set_thumb(&key, state, ratio) {
            warn!(%uid, error = %e, "recording thumbnail state failed");
        }
        if let Err(e) = self.db.album_photo_set_thumb(&key, state, ratio) {
            warn!(%uid, error = %e, "recording album thumbnail state failed");
        }
    }

    /// Whether the listing stamped under `key` is older than `ttl` (or was never
    /// fetched).
    pub(crate) async fn refresh_timeline(&self) -> CoreResult<bool> {
        let photos = self.photos();
        if photos
            .get_photos_root()
            .await
            .map_err(|e| CoreError::from_api(&e, "photos root"))?
            .is_none()
        {
            let _ = self.db.set_state_i64(PHOTOS_AVAILABLE, 0);
            let _ = self.db.set_state_i64(PHOTOS_SYNCED_MS, now_ms());
            return Ok(false);
        }
        let items = photos
            .enumerate_timeline()
            .await
            .map_err(|e| CoreError::from_api(&e, "timeline"))?;

        // The timeline DTO carries only a uid and capture time, but the Photos
        // page has to split into Photos / Videos / Raw — which needs each photo's
        // name and media type. Resolve those in batches off the request path.
        // Best-effort: a photo whose node we fail to resolve keeps whatever was
        // learned before (or classifies from nothing, i.e. a still photo), so a
        // partial resolve never blanks the timeline.
        let uids: Vec<NodeUid> = items.iter().map(|it| it.uid.clone()).collect();
        let mut meta: HashMap<String, (Option<String>, Option<String>, bool)> = HashMap::new();
        for chunk in uids.chunks(TIMELINE_ENRICH_CHUNK) {
            match photos.enumerate_nodes(chunk).await {
                Ok(nodes) => {
                    for node in nodes {
                        let media_type = match &node.kind {
                            NodeKind::File { media_type, .. } => Some(media_type.clone()),
                            NodeKind::Folder => None,
                        };
                        // The favourite state is a tag on the photo node, so it
                        // rides along with the metadata resolve rather than
                        // costing a listing of its own.
                        let favorite = node
                            .photo
                            .as_ref()
                            .is_some_and(|p| p.tags.contains(&PhotoTag::Favorite));
                        meta.insert(
                            node.uid.to_string(),
                            (Some(node.name), media_type, favorite),
                        );
                    }
                }
                Err(e) => warn!(error = %e, "resolving photo metadata for a timeline chunk failed"),
            }
        }

        let rows: Vec<db::TimelineRow> = items
            .iter()
            .map(|it| {
                let key = it.uid.to_string();
                // An unresolved photo keeps whatever was learned before, for the
                // favourite flag as much as for the name and media type.
                match meta.get(&key).cloned() {
                    Some((name, media_type, favorite)) => {
                        (key, it.capture_time, name, media_type, Some(favorite))
                    }
                    None => (key, it.capture_time, None, None, None),
                }
            })
            .collect();
        self.db.photos_replace(&rows).map_err(CoreError::from)?;
        let _ = self.db.set_state_i64(PHOTOS_AVAILABLE, 1);
        let _ = self.db.set_state_i64(PHOTOS_SYNCED_MS, now_ms());
        Ok(true)
    }

    /// Refresh the timeline off the request path, so a stale page is still served
    /// at DB speed. At most one refresh runs at a time.
    pub(crate) fn spawn_timeline_refresh(&self) {
        if self.timeline_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let core = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = core.refresh_timeline().await {
                warn!(error = %e, "background timeline refresh failed");
            }
            core.timeline_refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// Download a photo's full content into the content cache, returning its
    /// on-disk path (served from cache when a fresh blob already exists).
    pub(crate) fn open_photo(&self, uid: &NodeUid) -> CoreResult<PathBuf> {
        let photos = self.photos();
        let node = self
            .rt
            .block_on(photos.get_node(uid))
            .map_err(|e| CoreError::from_api(&e, "photo node"))?
            .ok_or_else(|| CoreError::not_found("photo not found"))?;
        let (mtime, size) = (node.modification_time, node_size(&node));
        if let Some(p) = self.cache.cached_content_path(uid, mtime, size) {
            return Ok(p);
        }
        let bytes = self
            .download_photo_tracked(&photos, uid, &node.name, size)
            .map_err(|e| CoreError::from_api(&e, "download photo"))?;
        self.cache
            .store(uid, mtime, size, &bytes)
            .map_err(|e| CoreError::internal(format!("cache store: {e}")))?;
        Ok(self.cache.content_path(uid))
    }
}

#[cfg(test)]
mod thumb_tests {
    use super::{
        RawTempFile, ScaleAttempt, THUMB_EDGE, exiftool_binary, ratio_of, scale_thumbnail,
        scale_thumbnail_with_exiftool, thumbnail_build_may_start,
    };
    use pdfs_core::control::{ThumbnailBuildStatus, is_thumbnail_image_name};
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    /// A `width`×`height` JPEG, standing in for a camera photo the server never
    /// generated a thumbnail for.
    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode_image(&image)
            .unwrap();
        bytes
    }

    #[test]
    fn scaling_fits_the_long_edge_and_keeps_the_aspect_ratio() {
        let photo = jpeg(4000, 3000);
        let ScaleAttempt::Made(thumb) = scale_thumbnail(&photo, "photo.jpg", &std::env::temp_dir())
        else {
            panic!("a JPEG scales");
        };

        let (width, height) = image::ImageReader::new(std::io::Cursor::new(&thumb.bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!(width, THUMB_EDGE, "the long edge lands on the target");
        assert_eq!(height, THUMB_EDGE * 3 / 4, "and nothing is cropped");
        assert!((thumb.ratio - 4.0 / 3.0).abs() < 1e-6);
        assert!(
            thumb.bytes.len() < photo.len(),
            "a thumbnail that isn't smaller than its photo is no thumbnail"
        );
    }

    #[test]
    fn a_portrait_photo_fits_its_long_edge_too() {
        let ScaleAttempt::Made(thumb) =
            scale_thumbnail(&jpeg(1000, 2000), "portrait.jpeg", &std::env::temp_dir())
        else {
            panic!("a JPEG scales");
        };
        assert!(thumb.ratio < 1.0, "portrait stays portrait");
        assert_eq!(ratio_of(&thumb.bytes).map(|r| r < 1.0), Some(true));
    }

    #[test]
    fn undecodable_bytes_are_not_a_thumbnail() {
        // What a photo in a format this build has no decoder for looks like: the
        // caller writes it off as un-thumbnailable rather than retrying forever.
        assert!(matches!(
            scale_thumbnail(b"not an image at all", "broken.jpg", &std::env::temp_dir()),
            ScaleAttempt::Undecodable
        ));
        assert!(ratio_of(b"not an image at all").is_none());
    }

    #[test]
    fn ratio_is_read_from_the_header_alone() {
        assert_eq!(ratio_of(&jpeg(300, 200)), Some(1.5));
    }

    #[test]
    fn raw_staging_file_is_private_and_removed_on_drop() {
        let staging_dir = std::env::temp_dir();
        let file = RawTempFile::create_in(&staging_dir, b"private camera bytes", "shot.NEF")
            .expect("temporary RAW file");
        let path = file.0.clone();
        assert_eq!(path.parent(), Some(staging_dir.as_path()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("NEF")
        );
        drop(file);
        assert!(!path.exists());
    }

    /// Optional real-file regression harness. RAW fixtures are too large to keep
    /// in the repository; developers can point this at any supported camera file.
    #[test]
    #[ignore = "set PDFS_RAW_FIXTURE to a supported camera RAW file"]
    fn real_raw_fixture_extracts_and_scales() {
        let path = std::env::var_os("PDFS_RAW_FIXTURE").expect("PDFS_RAW_FIXTURE");
        let path = std::path::PathBuf::from(path);
        let name = path.file_name().unwrap().to_string_lossy();
        let bytes = std::fs::read(&path).unwrap();
        let ScaleAttempt::Made(thumb) = scale_thumbnail(&bytes, &name, &std::env::temp_dir())
        else {
            panic!("embedded RAW preview");
        };
        let (width, height) = image::ImageReader::new(std::io::Cursor::new(&thumb.bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert!(width <= THUMB_EDGE && height <= THUMB_EDGE);
        assert!(width > 0 && height > 0);
    }

    #[test]
    fn recursive_build_recognises_the_same_image_names_as_the_gui() {
        for name in [
            "photo.jpg",
            "SCAN.PNG",
            "animation.Gif",
            "archive.tiff",
            "camera.NEF",
            "negative.cr3",
            "sensor.orf",
            "camera.raw",
        ] {
            assert!(is_thumbnail_image_name(name), "{name}");
        }
        for name in [
            "notes.txt",
            "movie.mp4",
            "image.jpg.zip",
            "jpg",
            "unsupported.heic",
            "unsupported.avif",
            "vector.svg",
            "camera.rw",
        ] {
            assert!(!is_thumbnail_image_name(name), "{name}");
        }
    }

    #[test]
    fn exiftool_binary_accepts_only_prefixed_base64() {
        let value = serde_json::json!({
            "PreviewImage": "base64:aGVsbG8=",
            "OtherImage": "not-binary",
            "ThumbnailImage": "base64:"
        });
        let object = value.as_object().unwrap();
        assert_eq!(
            exiftool_binary(object, "PreviewImage"),
            Some(b"hello".to_vec())
        );
        assert_eq!(exiftool_binary(object, "OtherImage"), None);
        assert_eq!(exiftool_binary(object, "ThumbnailImage"), None);
        assert_eq!(exiftool_binary(object, "Missing"), None);
    }

    #[test]
    fn missing_exiftool_is_retryable_not_a_permanent_raw_miss() {
        let attempt = scale_thumbnail_with_exiftool(
            b"camera bytes that require an embedded preview",
            "shot.raw",
            &std::env::temp_dir(),
            OsStr::new("pdfs-exiftool-deliberately-absent"),
            Duration::from_millis(10),
        );
        assert!(matches!(attempt, ScaleAttempt::Unavailable));
    }

    #[test]
    fn a_running_build_only_accepts_the_same_root() {
        let status = ThumbnailBuildStatus {
            running: true,
            path: "pictures/first".into(),
            ..Default::default()
        };
        assert!(!thumbnail_build_may_start(&status, "pictures/first").unwrap());
        assert!(thumbnail_build_may_start(&status, "pictures/second").is_err());

        let idle = ThumbnailBuildStatus::default();
        assert!(thumbnail_build_may_start(&idle, "pictures/second").unwrap());
    }
}
