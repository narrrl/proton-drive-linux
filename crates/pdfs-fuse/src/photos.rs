//! The photos timeline and its thumbnails.
//!
//! The timeline is a flat, newest-first projection of the photo share, persisted
//! by [`pdfs_store::db`] so the gallery opens instantly and refreshes behind the
//! user. Thumbnails the server has none for (anything a camera wrote rather than
//! a phone) are generated locally and stored as if the server had served them.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{FileExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::Mutex;
use pdfs_core::control::{
    FileThumbRequest, MappingProgress, PhotoItem, PhotoKind, PhotoLocation, PhotoThumb, PlaceInfo,
    ThumbnailBuildStatus, Topic, is_audio_file_name, is_raw_image_name, is_thumbnail_image_name,
    is_thumbnail_media_name,
};
use pdfs_core::{CoreError, CoreResult};
use pdfs_store::db::{self, StoredPhoto};

use proton_drive_rs::proton_sdk::error::Result as DriveResult;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{NodeKind, PhotoTag, PhotoTagsUpdate, Thumbnail, ThumbnailType};
use tracing::{debug, info, warn};

use super::ContentCache;
use super::drive::{DriveApi, RevisionRead};
use super::events::EventHub;
use super::shutdown::Shutdown;
use super::transfers::{CountingWriter, TransferRegistry};
use super::{
    Core, PHOTOS_AVAILABLE, PHOTOS_SYNCED_MS, TIMELINE_ENRICH_CHUNK, ThumbnailMissCaches,
    WriteAuthority, link, node_size, now_ms, parse_uid,
};
use pdfs_core::control::TransferDirection;
use pdfs_core::control::{ActivityKind, DirEntry};
use pdfs_core::lifecycle::ServiceBoard;
use proton_drive_rs::proton_sdk::error::ProtonError;
use proton_drive_rs::{ProtonDriveClient, ProtonPhotosClient};

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

/// Shots the similar-photo pass fetches thumbnails for at a time.
const SIMILAR_CHUNK: usize = 64;
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
static FFMPEG_MISSING_WARNING: Once = Once::new();
/// How much of a video's start a thumbnail downloads. The first frame, and
/// the one a second in, sit inside it at any common bitrate. A video is never
/// downloaded whole for one frame: a recording can be many gigabytes.
const VIDEO_HEAD_BYTES: u64 = 16 * 1024 * 1024;
/// A video no larger than this is downloaded whole: its head and tail would
/// cover most of it anyway.
const VIDEO_WHOLE_MAX_BYTES: u64 = 24 * 1024 * 1024;
/// Largest MP4/MOV index (`moov` box) fetched from past the head. An hour of
/// video indexes in a few MiB; anything larger is not worth one frame.
const VIDEO_INDEX_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Top-level boxes walked looking for the index before giving up.
const VIDEO_BOX_WALK_MAX: usize = 32;
/// A single frame is seconds of work even for 4K; this only stops a wedged
/// ffmpeg from holding a thumbnail permit.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(30);
/// A song no larger than this is downloaded whole for its cover.
const AUDIO_WHOLE_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// How much of a song's start is read first, to learn where its tags end.
const AUDIO_PROBE_BYTES: u64 = 64 * 1024;
/// How much of a song's start is read when its first bytes don't say where its
/// tags end: an Ogg comment packet, which holds the cover, has no size up front.
const AUDIO_HEAD_BYTES: u64 = 4 * 1024 * 1024;
/// Read past the tags at a song's start, so the first audio frames are there
/// for a reader that looks for them.
const AUDIO_HEAD_SLACK: u64 = 64 * 1024;
/// Largest start of a song read for its tags. A cover bigger than this is not
/// worth the download for a 512-pixel thumbnail.
const AUDIO_TAG_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// How much of a song's end is read in formats that keep their tags there
/// (APE tags in WavPack and Monkey's Audio, `id3 ` chunks in WAV and AIFF).
const AUDIO_TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// Names of a folder's cover image, best first, matched without case and
/// extension. A song without a picture in its tags gets this image's.
const FOLDER_COVER_STEMS: [&str; 6] = [
    "cover",
    "folder",
    "front",
    "album",
    "albumart",
    "albumartsmall",
];
/// Image types a folder cover is taken in.
const FOLDER_COVER_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];
/// Largest folder cover image downloaded whole for a song's thumbnail.
const FOLDER_COVER_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Largest thumbnail attached to an upload. Proton's own clients hold theirs to
/// this, stepping the JPEG quality down until it fits.
const UPLOAD_THUMB_MAX_BYTES: usize = 60 * 1024;
/// JPEG qualities tried, in order, for an upload thumbnail over that size.
const UPLOAD_THUMB_QUALITIES: [u8; 4] = [65, 50, 35, 20];
/// Largest still image read whole to render the thumbnail its upload carries.
const UPLOAD_IMAGE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// How much memory every thumbnail decode in the daemon may hold at once:
/// the encoded file plus its decoded pixels.
///
/// Without a limit, each of the 16 drain workers decoded its own full-size
/// photo. A batch of 100-megapixel JPEGs then held about 9 GiB, which swapped the
/// machine until the watchdog killed the daemon, on every restart
/// (`docs/BUGS.md` B280). With this budget, a phone photo still decodes beside
/// several others, and a 100-megapixel one decodes alone.
const DECODE_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// The process-wide [`DecodeBudget`] every thumbnail decode draws from.
static DECODE_BUDGET: DecodeBudget = DecodeBudget::new(DECODE_BUDGET_BYTES);

mod albums;
pub(crate) mod redate;
pub(crate) mod takeout;
pub(crate) use albums::AlbumOutcome;
pub(crate) use takeout::sha1_of;
mod thumbs;
mod timeline;
mod trash;

/// The Photos service: the timeline, albums and thumbnails.
///
/// It holds only the handles it calls, and reaches the filesystem through
/// [`PhotosHost`], so it does not depend on [`Core`]. [`Core::photos`] builds
/// one on demand, the same way [`Core::syncer`] builds the sync engine. A clone
/// shares everything, which is how a refresh or a thumbnail job moves onto a
/// background task.
#[derive(Clone)]
pub(crate) struct Photos {
    client: ProtonDriveClient,
    drive: Arc<dyn DriveApi>,
    rt: tokio::runtime::Handle,
    cache: Arc<ContentCache>,
    db: Arc<pdfs_store::db::Db>,
    shutdown: Arc<Shutdown>,
    services: ServiceBoard,
    events: Arc<EventHub>,
    transfers: Arc<TransferRegistry>,
    jobs: PhotoJobs,
    host: Arc<dyn PhotosHost>,
}

/// What the Photos service needs from the filesystem.
pub(crate) trait PhotosHost: Send + Sync {
    /// List a folder of the mount, for the recursive thumbnail build.
    fn list_dir(&self, rel: &Path) -> CoreResult<Vec<DirEntry>>;
    /// Record a change this daemon made, so its echo in the event feed is not
    /// taken for a remote edit.
    fn note_self_change(&self, uid: &NodeUid);
    /// Add an entry to the activity log.
    fn log_activity(&self, kind: ActivityKind, target: String, detail: String, ok: bool);
}

impl PhotosHost for Core {
    fn list_dir(&self, rel: &Path) -> CoreResult<Vec<DirEntry>> {
        Core::list_dir(self, rel)
    }

    fn note_self_change(&self, uid: &NodeUid) {
        Core::note_self_change(self, uid)
    }

    fn log_activity(&self, kind: ActivityKind, target: String, detail: String, ok: bool) {
        Core::log_activity(self, kind, target, detail, ok);
    }
}

impl Core {
    /// The Photos service, with this daemon as its host.
    pub(crate) fn photos(&self) -> Photos {
        Photos {
            client: self.client.clone(),
            drive: self.drive.clone(),
            rt: self.rt.clone(),
            cache: self.cache.clone(),
            db: self.db.clone(),
            shutdown: self.shutdown.clone(),
            services: self.services.clone(),
            events: self.events.clone(),
            transfers: self.transfers.clone(),
            jobs: self.photo_jobs.clone(),
            host: Arc::new(self.clone()),
        }
    }
}

impl Photos {
    /// The Photos API over the daemon's single authenticated session.
    fn photos_api(&self) -> ProtonPhotosClient {
        ProtonPhotosClient::from_drive_client(self.client.clone())
    }

    /// Like [`download_file_tracked_to`] for a photo, streaming through the
    /// photos client's [`download_photo_to`].
    ///
    /// [`download_file_tracked_to`]: crate::Core::download_file_tracked_to
    /// [`download_photo_to`]: ProtonPhotosClient::download_photo_to
    fn download_photo_tracked(
        &self,
        photos: &ProtonPhotosClient,
        uid: &NodeUid,
        name: &str,
        total: u64,
    ) -> std::result::Result<Vec<u8>, ProtonError> {
        let guard = self
            .transfers
            .begin(name, uid.to_string(), TransferDirection::Download, total);
        let mut out = CountingWriter::new(Vec::with_capacity(total as usize), &guard);
        self.rt.block_on(photos.download_photo_to(uid, &mut out))?;
        Ok(out.into_inner())
    }
}

/// What the Photos and thumbnail jobs are doing right now: which refreshes
/// and builds are running, how far they got, and which thumbnails are known
/// to be missing. Shared by every mount of the daemon; only this module and
/// the status reports read it.
#[derive(Clone)]
pub(crate) struct PhotoJobs {
    /// True while a background refresh of the photos timeline is already
    /// running, so a burst of page requests against a stale listing kicks off
    /// one refresh rather than one per request.
    pub(crate) timeline_refreshing: Arc<AtomicBool>,
    /// How far the running timeline refresh has got resolving photo metadata,
    /// as `(done, total)` photos; `None` when no refresh is resolving any. The
    /// Places view shows it while a library's locations are first read.
    pub(crate) timeline_progress: Arc<Mutex<Option<(usize, usize)>>>,
    /// The same, for the album listing.
    pub(crate) albums_refreshing: Arc<AtomicBool>,
    /// True while the similar-photo pass hashes thumbnails (see
    /// [`Photos::photo_similar`]), so asking again doesn't start a second one.
    pub(crate) similar_hashing: Arc<AtomicBool>,
    /// Photos whose missing thumbnail is being generated right now. A tile that is
    /// still on screen asks for its thumbnail again every few seconds, and each of
    /// those downloads is a full-size photo — so an in-flight uid is never started
    /// twice.
    pub(crate) thumb_gen: Arc<Mutex<HashSet<NodeUid>>>,
    /// Global permit pool for locally generated thumbnails. The old per-request
    /// limit multiplied when the GUI sent several batches, allowing dozens of
    /// full-size images to download at once.
    pub(crate) thumb_gen_budget: Arc<tokio::sync::Semaphore>,
    /// Current ordinary-file listing generation and a wake-up for tasks waiting
    /// on a permit or a download when that listing is abandoned.
    pub(crate) file_thumb_generation: Arc<AtomicU64>,
    pub(crate) file_thumb_cancel: Arc<tokio::sync::Notify>,
    /// Progress of the explicit recursive build started from the Files toolbar.
    pub(crate) thumbnail_build: Arc<Mutex<ThumbnailBuildStatus>>,
    /// Cancellation for the explicit recursive build. Separate from ordinary
    /// listing cancellation because the two jobs have independent lifetimes.
    pub(crate) thumbnail_build_cancelled: Arc<AtomicBool>,
    pub(crate) thumbnail_build_cancel: Arc<tokio::sync::Notify>,
    /// Negative thumbnail knowledge, separated by whether the remote lacked a
    /// thumbnail or local decoding proved this revision unsupported.
    ///
    /// Absence has to be cached or it costs a round trip every time it is asked
    /// for, and it is asked for constantly: an `ls -l` from an xattr-aware lister
    /// issues a `getxattr` per advertised name per entry, so a 65-file directory
    /// of videos re-probed 130 times per listing at ~186 ms each (B5). The mtime
    /// is the validity tag — a new revision may well have a thumbnail — matching
    /// how [`ContentCache::read_thumbnail`] validates the positive side.
    pub(crate) thumbnail_misses: Arc<Mutex<ThumbnailMissCaches>>,
}

impl PhotoJobs {
    pub(crate) fn new() -> Self {
        Self {
            timeline_refreshing: Arc::new(AtomicBool::new(false)),
            timeline_progress: Arc::new(Mutex::new(None)),
            albums_refreshing: Arc::new(AtomicBool::new(false)),
            similar_hashing: Arc::new(AtomicBool::new(false)),
            thumb_gen: Arc::new(Mutex::new(HashSet::new())),
            thumb_gen_budget: Arc::new(tokio::sync::Semaphore::new(THUMB_GEN_CONCURRENCY)),
            file_thumb_generation: Arc::new(AtomicU64::new(0)),
            file_thumb_cancel: Arc::new(tokio::sync::Notify::new()),
            thumbnail_build: Arc::new(Mutex::new(Default::default())),
            thumbnail_build_cancelled: Arc::new(AtomicBool::new(false)),
            thumbnail_build_cancel: Arc::new(tokio::sync::Notify::new()),
            thumbnail_misses: Arc::new(Mutex::new(Default::default())),
        }
    }
}

/// A counting gate over bytes: [`acquire`](Self::acquire) waits until the cost
/// fits beside what is already held.
struct DecodeBudget {
    limit: u64,
    used: std::sync::Mutex<u64>,
    freed: std::sync::Condvar,
}

/// A share of a [`DecodeBudget`], given back on drop.
struct DecodePermit<'a> {
    budget: &'a DecodeBudget,
    cost: u64,
}

impl DecodeBudget {
    const fn new(limit: u64) -> Self {
        Self {
            limit,
            used: std::sync::Mutex::new(0),
            freed: std::sync::Condvar::new(),
        }
    }

    /// Wait until `cost` bytes fit, and hold them. A cost above the limit is
    /// cut down to the limit, so the largest image waits for an idle budget
    /// and then runs alone, instead of waiting forever.
    fn acquire(&self, cost: u64) -> DecodePermit<'_> {
        let cost = cost.clamp(1, self.limit);
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *used + cost > self.limit {
            used = self
                .freed
                .wait(used)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *used += cost;
        DecodePermit { budget: self, cost }
    }
}

impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *used -= self.cost;
        self.budget.freed.notify_all();
    }
}

/// What decoding an image of `encoded_len` bytes and `dimensions` costs in
/// memory: the encoded bytes, the decoded pixels at up to 4 bytes each, and a
/// second copy of the pixels when [`decode_upright`] rotates them. When the
/// header gives no dimensions, assume the image is compressed 16 to 1.
fn decode_cost(encoded_len: u64, dimensions: Option<(u32, u32)>) -> u64 {
    let pixels = match dimensions {
        Some((width, height)) => u64::from(width) * u64::from(height) * 4 * 2,
        None => encoded_len.saturating_mul(16),
    };
    encoded_len.saturating_add(pixels)
}

/// The pixel dimensions in the header of an encoded image, without decoding it.
fn dimensions_of(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// [`dimensions_of`] for an image file. Reads only the header.
fn dimensions_of_file(path: &Path) -> Option<(u32, u32)> {
    image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

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

/// The difference hash of each cached thumbnail in `paths`, keyed like the
/// input, or [`pdfs_core::similar::NO_HASH`] for one that can't be decoded or
/// shows no picture. Decoding is spread over the machine's cores.
fn similar_hashes(paths: Vec<(String, PathBuf)>) -> Vec<(String, u64)> {
    fn hash(path: &Path) -> u64 {
        image::ImageReader::open(path)
            .ok()
            .and_then(|reader| reader.with_guessed_format().ok())
            .and_then(|reader| reader.decode().ok())
            .and_then(|image| {
                let luma = image.to_luma8();
                pdfs_core::similar::difference_hash(
                    luma.width() as usize,
                    luma.height() as usize,
                    luma.as_raw(),
                )
            })
            .unwrap_or(pdfs_core::similar::NO_HASH)
    }
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let per_thread = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = paths
            .chunks(per_thread)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(uid, path)| (uid.clone(), hash(path)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap_or_default())
            .collect()
    })
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
    /// Decoded nothing: a format this build has no decoder for, a video
    /// ffmpeg takes no frame from, or a `.ts` that is not one. Permanent.
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
/// Waits for its share of [`DECODE_BUDGET`] first.
fn scale_thumbnail(bytes: &[u8], name: &str, staging_dir: &Path) -> ScaleAttempt {
    let _permit = DECODE_BUDGET.acquire(decode_cost(bytes.len() as u64, dimensions_of(bytes)));
    scale_thumbnail_unbudgeted(bytes, name, staging_dir)
}

/// [`scale_thumbnail`] for a caller that already holds its [`DecodePermit`].
fn scale_thumbnail_unbudgeted(bytes: &[u8], name: &str, staging_dir: &Path) -> ScaleAttempt {
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
    let image = match decode_upright(bytes) {
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
    encode_thumb(&image)
}

/// Decode `bytes` and turn the picture the way its EXIF orientation says.
///
/// `ImageReader::decode` does not: a phone stores a portrait photo as landscape
/// pixels plus an orientation tag, so every such thumbnail came out on its
/// side, and the drain uploaded it to Drive that way for every client to show
/// (`docs/BUGS.md` B226).
fn decode_upright(bytes: &[u8]) -> Option<image::DynamicImage> {
    use image::ImageDecoder as _;
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = image::DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

/// Shrink a decoded image to a [`THUMB_EDGE`] JPEG, keeping its aspect ratio.
fn encode_thumb(image: &image::DynamicImage) -> ScaleAttempt {
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

/// How a bounded helper run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolError {
    /// The helper isn't installed.
    Missing,
    /// It couldn't be started or read, or overran its budget and was killed.
    Unavailable,
    /// It ran and exited non-zero.
    Failed,
}

/// Run a helper to completion within `timeout` and return its stdout, drained
/// concurrently. The latter matters for exiftool's full-size `JpgFromRaw`
/// fallbacks and for ffmpeg's frames, either of which can exceed a pipe buffer
/// long before the child exits.
fn run_bounded(mut command: Command, timeout: Duration) -> Result<Vec<u8>, ToolError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ToolError::Missing
        } else {
            ToolError::Unavailable
        }
    })?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ToolError::Unavailable);
    };
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status.success()),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader
        .join()
        .ok()
        .and_then(Result::ok)
        .ok_or(ToolError::Unavailable)?;
    match status {
        Some(true) => Ok(output),
        Some(false) => Err(ToolError::Failed),
        None => Err(ToolError::Unavailable),
    }
}

/// Run one bounded exiftool JSON query.
fn exiftool_query(
    exiftool: &OsStr,
    path: &Path,
    tags: &[&str],
    timeout: Duration,
) -> Result<serde_json::Value, RawPreviewUnavailable> {
    let mut command = Command::new(exiftool);
    command.args(["-j", "-b", "-n"]);
    command.args(tags);
    command.arg(path);
    let output = run_bounded(command, timeout).map_err(|error| {
        if error == ToolError::Missing {
            EXIFTOOL_MISSING_WARNING.call_once(|| {
                warn!(
                    "exiftool is not installed; camera RAW thumbnails are unavailable \
                     (Debian/Ubuntu: libimage-exiftool-perl)"
                );
            });
        }
        RawPreviewUnavailable
    })?;
    serde_json::from_slice(&output).map_err(|_| RawPreviewUnavailable)
}

/// Whether [`upload_thumbnails`] can make a thumbnail for a file `name` of
/// `size` bytes, asked before the file is on disk: an image small enough to
/// decode, or a video while ffmpeg is installed.
pub(crate) fn can_make_thumbnail(name: &str, size: u64) -> bool {
    if is_video_name(name) {
        return ffmpeg_available();
    }
    is_thumbnail_image_name(name) && size <= UPLOAD_IMAGE_MAX_BYTES
}

/// Whether `name` is a video, whose thumbnail is a frame ffmpeg grabs rather
/// than a picture the image decoder can scale.
fn is_video_name(name: &str) -> bool {
    PhotoKind::classify(Some(name), None) == PhotoKind::Video
}

/// Whether ffmpeg can be run at all. Asked once, before a video is downloaded:
/// without ffmpeg every scroll past a video would fetch it whole for nothing.
fn ffmpeg_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let mut command = Command::new("ffmpeg");
        command.arg("-version");
        let available = run_bounded(command, Duration::from_secs(5)).is_ok();
        if !available {
            warn_ffmpeg_missing();
        }
        available
    })
}

fn warn_ffmpeg_missing() {
    FFMPEG_MISSING_WARNING.call_once(|| {
        warn!("ffmpeg is not installed; video thumbnails are unavailable");
    });
}

/// A thumbnail from one frame of the video at `path`. A second in, past the
/// black lead-in most clips open on, and the first frame for a clip shorter than
/// that. ffmpeg applies the clip's rotation, so a portrait phone video comes out
/// upright.
///
/// Missing or wedged ffmpeg is retryable; a file ffmpeg can take no frame from
/// is the permanent verdict.
fn video_file_thumbnail(path: &Path, ffmpeg: &OsStr, timeout: Duration) -> ScaleAttempt {
    for seek in ["1", "0"] {
        let mut command = Command::new(ffmpeg);
        command.args(["-nostdin", "-loglevel", "error", "-ss", seek, "-i"]);
        command.arg(path);
        command.args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "mjpeg", "-"]);
        match run_bounded(command, timeout) {
            Ok(frame) if !frame.is_empty() => {
                return match image::load_from_memory(&frame) {
                    Ok(image) => encode_thumb(&image),
                    Err(_) => ScaleAttempt::Undecodable,
                };
            }
            // No frame that far in: try from the start.
            Ok(_) | Err(ToolError::Failed) => continue,
            Err(ToolError::Missing) => {
                warn_ffmpeg_missing();
                return ScaleAttempt::Unavailable;
            }
            Err(ToolError::Unavailable) => return ScaleAttempt::Unavailable,
        }
    }
    ScaleAttempt::Undecodable
}

/// What a thumbnail is made from.
enum Fetched {
    /// The thumbnail itself, from the server.
    Ready(Vec<u8>),
    /// Nothing: a video with no server thumbnail, and no ffmpeg to take one.
    Nothing,
    /// A whole image.
    Whole(Vec<u8>),
    Video(VideoSample),
    Audio(AudioSample),
}

/// The parts of a video its thumbnail needs, downloaded: the start of the
/// file, and for an MP4 or MOV whose index is further in, that index.
struct VideoSample {
    /// The whole file's length.
    size: u64,
    head: Vec<u8>,
    /// The `moov` box and its offset, when it is not inside `head`.
    index: Option<(u64, Vec<u8>)>,
}

impl VideoSample {
    /// Whether this could be an MPEG transport stream: one opens on the 0x47
    /// sync byte, and repeats it every 188-byte packet. Only asked of `.ts`
    /// files, which are TypeScript source as often as they are video.
    fn is_transport_stream(&self) -> bool {
        self.head.first() == Some(&0x47) && self.head.get(188).is_none_or(|&byte| byte == 0x47)
    }
}

/// Download what a thumbnail of the video `reader` reads needs. A file of up
/// to [`VIDEO_WHOLE_MAX_BYTES`] comes whole. A larger one brings its first
/// [`VIDEO_HEAD_BYTES`], which is all a stream format (Matroska, WebM, MPEG-TS,
/// AVI) needs for a frame, plus the index of an ISO media file (MP4, MOV, 3GP)
/// written with its index after the media data, which is how most cameras and
/// phones write it.
async fn sample_video(reader: &dyn RevisionRead) -> DriveResult<VideoSample> {
    let size = reader.size();
    if size <= VIDEO_WHOLE_MAX_BYTES {
        let head = reader.read_at(0, size).await?;
        return Ok(VideoSample {
            size,
            head,
            index: None,
        });
    }
    let head = reader.read_at(0, VIDEO_HEAD_BYTES).await?;
    let index = match iso_media_index(reader, &head, size).await? {
        Some((offset, len)) if offset.saturating_add(len) > head.len() as u64 => {
            Some((offset, reader.read_at(offset, len).await?))
        }
        _ => None,
    };
    Ok(VideoSample { size, head, index })
}

/// Where the `moov` box of an ISO media file sits, as `(offset, length)`.
/// `None` for a file that is not one, or whose index is missing, oversized or
/// past [`VIDEO_BOX_WALK_MAX`] boxes. Box headers past `head` are read one at a
/// time, 16 bytes each.
async fn iso_media_index(
    reader: &dyn RevisionRead,
    head: &[u8],
    size: u64,
) -> DriveResult<Option<(u64, u64)>> {
    const FIRST_BOXES: [&[u8; 4]; 6] = [b"ftyp", b"moov", b"mdat", b"wide", b"free", b"skip"];
    if !head
        .get(4..8)
        .is_some_and(|kind| FIRST_BOXES.iter().any(|known| kind == *known))
    {
        return Ok(None);
    }
    let mut offset = 0u64;
    for _ in 0..VIDEO_BOX_WALK_MAX {
        if offset.saturating_add(8) > size {
            break;
        }
        let header = match usize::try_from(offset)
            .ok()
            .and_then(|start| head.get(start..start.checked_add(16)?))
        {
            Some(header) => header.to_vec(),
            None => reader.read_at(offset, 16).await?,
        };
        let Some((kind, len)) = iso_box_header(&header, size - offset) else {
            break;
        };
        if kind == *b"moov" {
            return Ok((len <= VIDEO_INDEX_MAX_BYTES).then_some((offset, len)));
        }
        offset = offset.saturating_add(len);
    }
    Ok(None)
}

/// The type and length of the ISO box whose header starts `header`, of a box
/// that may run at most `left` bytes to the end of the file.
fn iso_box_header(header: &[u8], left: u64) -> Option<([u8; 4], u64)> {
    let size = u32::from_be_bytes(header.get(0..4)?.try_into().ok()?);
    let kind: [u8; 4] = header.get(4..8)?.try_into().ok()?;
    let len = match size {
        // Runs to the end of the file.
        0 => left,
        // The real length follows as 64 bits.
        1 => u64::from_be_bytes(header.get(8..16)?.try_into().ok()?),
        len => u64::from(len),
    };
    (8..=left).contains(&len).then_some((kind, len))
}

/// A thumbnail from one frame of a downloaded [`VideoSample`]. The sample is
/// laid out in a staging file at the offsets it came from, so ffmpeg finds an
/// index where the file's own boxes say it is. The gap between holds no data
/// and costs no disk.
fn video_sample_thumbnail(
    sample: &VideoSample,
    name: &str,
    staging_dir: &Path,
    ffmpeg: &OsStr,
    timeout: Duration,
) -> ScaleAttempt {
    let transport_stream_name = Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"));
    if transport_stream_name && !sample.is_transport_stream() {
        return ScaleAttempt::Undecodable;
    }
    let Some(file) = RawTempFile::create_in(staging_dir, &sample.head, name) else {
        return ScaleAttempt::Unavailable;
    };
    if let Some((offset, index)) = &sample.index {
        let laid_out = OpenOptions::new()
            .write(true)
            .open(&file.0)
            .and_then(|staged| {
                staged.write_all_at(index, *offset)?;
                staged.set_len(sample.size.max(offset + index.len() as u64))
            });
        if laid_out.is_err() {
            return ScaleAttempt::Unavailable;
        }
    }
    video_file_thumbnail(&file.0, ffmpeg, timeout)
}

/// The parts of a song its cover needs, downloaded: the start, where most
/// formats keep their tags, an M4A's index, and for formats that keep their
/// tags at the end, the end.
struct AudioSample {
    /// The whole file's length.
    size: u64,
    /// Each part and the offset it came from.
    parts: Vec<(u64, Vec<u8>)>,
}

/// Where the tags at the start of a song end, read from its first `probe`
/// bytes: past an ID3v2 tag, and past FLAC's metadata blocks, which hold its
/// picture. `None` when the start says nothing, as for Ogg. A FLAC block
/// header past `probe` stops the walk with a guess of [`AUDIO_HEAD_BYTES`]
/// more.
fn audio_tags_end(probe: &[u8]) -> Option<u64> {
    let mut offset = 0u64;
    let mut known = false;
    if probe.get(0..3) == Some(b"ID3") {
        let size = probe.get(6..10)?;
        if size.iter().any(|byte| byte & 0x80 != 0) {
            return None;
        }
        let size = size
            .iter()
            .fold(0u64, |acc, byte| (acc << 7) | u64::from(*byte));
        let footer = probe.get(5).is_some_and(|flags| flags & 0x10 != 0);
        offset = 10 + size + if footer { 10 } else { 0 };
        known = true;
    }
    let start = usize::try_from(offset).ok()?;
    if probe.get(start..start + 4) == Some(b"fLaC") {
        offset += 4;
        loop {
            let at = usize::try_from(offset).ok()?;
            let Some(header) = probe.get(at..at + 4) else {
                return Some(offset + AUDIO_HEAD_BYTES);
            };
            let len = u64::from_be_bytes([0, 0, 0, 0, 0, header[1], header[2], header[3]]);
            offset += 4 + len;
            if header[0] & 0x80 != 0 {
                return Some(offset);
            }
        }
    }
    known.then_some(offset)
}

/// Whether a song in format `name` may keep its tags at its end.
fn audio_tags_at_end(name: &str) -> bool {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(extension.as_str(), "wv" | "ape" | "wav" | "aif" | "aiff")
}

/// Download what the cover of the song `reader` reads, named `name`, needs. A
/// song of up to [`AUDIO_WHOLE_MAX_BYTES`] comes whole; a larger one brings its
/// tags, never its audio.
async fn sample_audio(reader: &dyn RevisionRead, name: &str) -> DriveResult<AudioSample> {
    let size = reader.size();
    if size <= AUDIO_WHOLE_MAX_BYTES {
        let whole = reader.read_at(0, size).await?;
        return Ok(AudioSample {
            size,
            parts: vec![(0, whole)],
        });
    }
    let probe = reader.read_at(0, AUDIO_PROBE_BYTES.min(size)).await?;
    // An M4A keeps its cover in its index, fetched below wherever it sits.
    let iso_media = probe.get(4..8) == Some(b"ftyp");
    let want = match audio_tags_end(&probe) {
        _ if iso_media => 0,
        Some(end) => end.saturating_add(AUDIO_HEAD_SLACK),
        None => AUDIO_HEAD_BYTES,
    }
    .min(AUDIO_TAG_MAX_BYTES)
    .min(size);
    let head = if want > probe.len() as u64 {
        reader.read_at(0, want).await?
    } else {
        probe
    };
    let mut parts = Vec::new();
    match iso_media_index(reader, &head, size).await? {
        Some((offset, len)) if offset.saturating_add(len) > head.len() as u64 => {
            parts.push((offset, reader.read_at(offset, len).await?));
        }
        _ => {}
    }
    let head_len = head.len() as u64;
    parts.push((0, head));
    if audio_tags_at_end(name) {
        let start = size.saturating_sub(AUDIO_TAIL_BYTES).max(head_len);
        if start < size {
            parts.push((start, reader.read_at(start, size - start).await?));
        }
    }
    Ok(AudioSample { size, parts })
}

/// The picture a song's tags carry: the front cover, or else the first one.
/// `None` for a song without one, or one lofty can't read.
fn embedded_cover(path: &Path) -> Option<Vec<u8>> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::TaggedFileExt as _;
    use lofty::picture::PictureType;

    let tagged = lofty::probe::Probe::open(path)
        .ok()?
        .options(
            ParseOptions::new()
                .read_properties(false)
                .parsing_mode(ParsingMode::Relaxed),
        )
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let pictures = || tagged.tags().iter().flat_map(|tag| tag.pictures());
    pictures()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures().next())
        .map(|picture| picture.data().to_vec())
        .filter(|data| !data.is_empty())
}

/// A thumbnail from the cover in a downloaded [`AudioSample`]'s tags. The
/// sample is laid out in a staging file at the offsets it came from, so the
/// tags are where the file's own headers say they are; the audio between
/// holds no data and costs no disk. A song without a cover, or with one the
/// image decoder can't read, is [`ScaleAttempt::Undecodable`].
fn audio_cover_thumbnail(sample: &AudioSample, name: &str, staging_dir: &Path) -> ScaleAttempt {
    let Some(file) = RawTempFile::create_in(staging_dir, &[], name) else {
        return ScaleAttempt::Unavailable;
    };
    let laid_out = OpenOptions::new()
        .write(true)
        .open(&file.0)
        .and_then(|staged| {
            for (offset, bytes) in &sample.parts {
                staged.write_all_at(bytes, *offset)?;
            }
            staged.set_len(sample.size)
        });
    if laid_out.is_err() {
        return ScaleAttempt::Unavailable;
    }
    match embedded_cover(&file.0) {
        Some(picture) => scale_thumbnail(&picture, "cover", staging_dir),
        None => ScaleAttempt::Undecodable,
    }
}

/// How good a folder cover `name` is: its place in [`FOLDER_COVER_STEMS`],
/// lowest best, or `None` for a file that is no cover image.
fn cover_rank(name: &str) -> Option<usize> {
    let (stem, extension) = name.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();
    if !FOLDER_COVER_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }
    let stem = stem.to_ascii_lowercase();
    FOLDER_COVER_STEMS.iter().position(|known| *known == stem)
}

/// The thumbnail to attach to an upload of the local file at `path`, which is
/// named `name` in Drive. Without one, Drive's own apps show a generic icon and
/// the gallery has to download the whole file to make its own.
///
/// Best effort: a file that isn't an image, a video or a song with a cover in
/// its tags, is too large to read, or can't be decoded uploads without a
/// thumbnail, as it always did. Blocking and CPU-bound, so async callers run it
/// on the blocking pool.
pub(crate) fn upload_thumbnails(path: &Path, name: &str, staging_dir: &Path) -> Vec<Thumbnail> {
    if is_video_name(name) && !ffmpeg_available() {
        return Vec::new();
    }
    upload_thumbnails_with(path, name, staging_dir, OsStr::new("ffmpeg"))
}

fn upload_thumbnails_with(
    path: &Path,
    name: &str,
    staging_dir: &Path,
    ffmpeg: &OsStr,
) -> Vec<Thumbnail> {
    let attempt = if is_video_name(name) {
        if !is_video_file(path, name) {
            return Vec::new();
        }
        video_file_thumbnail(path, ffmpeg, FFMPEG_TIMEOUT)
    } else if is_thumbnail_image_name(name) {
        let Some(len) = std::fs::metadata(path)
            .ok()
            .map(|meta| meta.len())
            .filter(|len| *len <= UPLOAD_IMAGE_MAX_BYTES)
        else {
            return Vec::new();
        };
        // Taken before the read, so the encoded bytes count against the
        // budget too.
        let _permit = DECODE_BUDGET.acquire(decode_cost(len, dimensions_of_file(path)));
        let Ok(bytes) = std::fs::read(path) else {
            return Vec::new();
        };
        scale_thumbnail_unbudgeted(&bytes, name, staging_dir)
    } else if is_audio_file_name(name) {
        // Only the song's own picture: a cover image beside it is not known
        // here, where `path` may be a staging copy rather than the song's
        // folder. The daemon finds that one when a listing asks.
        match embedded_cover(path) {
            Some(picture) => scale_thumbnail(&picture, "cover", staging_dir),
            None => return Vec::new(),
        }
    } else {
        return Vec::new();
    };
    let ScaleAttempt::Made(thumb) = attempt else {
        debug!(name, "uploading without a thumbnail; none could be made");
        return Vec::new();
    };
    fit_upload_thumb(thumb.bytes)
        .and_then(|bytes| Thumbnail::new(ThumbnailType::Thumbnail, bytes).ok())
        .into_iter()
        .collect()
}

/// `.ts` is TypeScript as often as it is an MPEG transport stream, so a source
/// tree must not cost an ffmpeg run per file. A transport stream opens on its
/// 0x47 sync byte; every other video extension is taken at its word.
fn is_video_file(path: &Path, name: &str) -> bool {
    if !name.to_ascii_lowercase().ends_with(".ts") {
        return true;
    }
    let mut first = [0u8; 1];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut first))
        .is_ok_and(|()| first[0] == 0x47)
}

/// Re-encode a thumbnail at falling quality until it fits
/// [`UPLOAD_THUMB_MAX_BYTES`], and at half the size if even the lowest quality
/// doesn't. `None` if nothing fits.
fn fit_upload_thumb(bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.len() <= UPLOAD_THUMB_MAX_BYTES {
        return Some(bytes);
    }
    let image = image::load_from_memory(&bytes).ok()?;
    let half = image.thumbnail(image.width() / 2, image.height() / 2);
    [image.to_rgb8(), half.to_rgb8()].iter().find_map(|image| {
        UPLOAD_THUMB_QUALITIES.iter().find_map(|&quality| {
            let mut smaller = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut smaller, quality)
                .encode_image(image)
                .ok()?;
            (smaller.len() <= UPLOAD_THUMB_MAX_BYTES).then_some(smaller)
        })
    })
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

/// What a [`Core::trash_photos`] batch did: the uids that left the server, and
/// the ones that did not with the reason each was refused.
pub(crate) type TrashOutcome = (Vec<String>, Vec<(String, String)>);

/// Formats supported by the local thumbnail decoder and exposed by the GUI.
/// Clears the timeline refresh's progress when dropped, so an error or a
/// cancelled refresh doesn't leave a bar standing.
struct ProgressGuard<'a>(&'a parking_lot::Mutex<Option<(usize, usize)>>);

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        *self.0.lock() = None;
    }
}

#[cfg(test)]
mod decode_budget_tests {
    use super::{DecodeBudget, decode_cost};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// A decode that does not fit beside the ones running waits for them,
    /// instead of every drain worker decoding at once (`docs/BUGS.md` B280).
    #[test]
    fn a_decode_over_the_budget_waits_for_the_running_one() {
        let budget = Arc::new(DecodeBudget::new(100));
        let first = budget.acquire(60);
        let started = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (budget, started) = (budget.clone(), started.clone());
            std::thread::spawn(move || {
                let _second = budget.acquire(60);
                started.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !started.load(Ordering::SeqCst),
            "second decode ran over budget"
        );
        drop(first);
        waiter.join().expect("waiter");
        assert!(started.load(Ordering::SeqCst));
    }

    /// Decodes that fit together run together.
    #[test]
    fn small_decodes_share_the_budget() {
        let budget = DecodeBudget::new(100);
        let _a = budget.acquire(30);
        let _b = budget.acquire(30);
        let _c = budget.acquire(30);
    }

    /// An image bigger than the whole budget still decodes, alone, rather than
    /// waiting forever.
    #[test]
    fn an_image_over_the_whole_budget_still_decodes() {
        let budget = DecodeBudget::new(100);
        drop(budget.acquire(10_000));
        let _again = budget.acquire(100);
    }

    /// The cost of the 102-megapixel JPEG that set off B280 fills the real
    /// budget, so it never decodes beside another one.
    #[test]
    fn a_hundred_megapixel_photo_costs_the_whole_budget() {
        let cost = decode_cost(97_266_141, Some((11_656, 8_742)));
        assert!(cost >= super::DECODE_BUDGET_BYTES);
        let phone = decode_cost(4_000_000, Some((4_000, 3_000)));
        assert!(phone * 4 < super::DECODE_BUDGET_BYTES);
    }
}

#[cfg(test)]
mod thumb_tests {
    use super::{
        AUDIO_HEAD_BYTES, AUDIO_TAIL_BYTES, AudioSample, audio_cover_thumbnail, audio_tags_end,
        cover_rank, sample_audio,
    };
    use super::{
        RawTempFile, ScaleAttempt, THUMB_EDGE, UPLOAD_IMAGE_MAX_BYTES, UPLOAD_THUMB_MAX_BYTES,
        VIDEO_HEAD_BYTES, VIDEO_WHOLE_MAX_BYTES, VideoSample, can_make_thumbnail, exiftool_binary,
        ffmpeg_available, fit_upload_thumb, is_video_name, ratio_of, sample_video, scale_thumbnail,
        scale_thumbnail_with_exiftool, similar_hashes, thumbnail_build_may_start,
        upload_thumbnails_with, video_sample_thumbnail,
    };
    use crate::drive::RevisionRead;
    use pdfs_core::control::{ThumbnailBuildStatus, is_thumbnail_image_name};
    use proton_drive_rs::ThumbnailType;
    use proton_drive_rs::proton_sdk::error::Result as DriveResult;
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};
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

    /// `jpeg` with an EXIF APP1 segment that carries only `orientation`.
    fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let plain = jpeg(width, height);
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let mut out = plain[..2].to_vec();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap().to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&plain[2..]);
        out
    }

    /// A phone stores a portrait photo as landscape pixels and an EXIF
    /// orientation; its thumbnail came out on its side (`docs/BUGS.md` B226).
    /// A Takeout photo is copied out of its archive only when a thumbnail can
    /// be made from the copy.
    #[test]
    fn only_a_photo_that_can_have_a_thumbnail_is_worth_copying_out() {
        assert!(can_make_thumbnail("IMG_1.jpg", 4 << 20));
        assert!(can_make_thumbnail("Screenshot.png", 4 << 20));
        assert!(!can_make_thumbnail("IMG_1.jpg", UPLOAD_IMAGE_MAX_BYTES + 1));
        assert!(!can_make_thumbnail("notes.txt", 10));
        assert_eq!(can_make_thumbnail("clip.mp4", 1 << 30), ffmpeg_available());
    }

    #[test]
    fn a_thumbnail_is_turned_the_way_its_exif_says() {
        let ScaleAttempt::Made(thumb) = scale_thumbnail(
            &jpeg_with_orientation(2000, 1000, 6),
            "portrait.jpg",
            &std::env::temp_dir(),
        ) else {
            panic!("a JPEG scales");
        };
        assert!(thumb.ratio < 1.0, "turned upright: {}", thumb.ratio);
        assert_eq!(ratio_of(&thumb.bytes).map(|r| r < 1.0), Some(true));

        let ScaleAttempt::Made(level) = scale_thumbnail(
            &jpeg_with_orientation(2000, 1000, 1),
            "landscape.jpg",
            &std::env::temp_dir(),
        ) else {
            panic!("a JPEG scales");
        };
        assert!(level.ratio > 1.0);
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

    /// An executable shell script standing in for ffmpeg, in its own directory
    /// under `tmp` so parallel tests never share one.
    fn fake_ffmpeg(tmp: &std::path::Path, body: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(tmp).unwrap();
        let path = tmp.join("ffmpeg");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn scratch(test: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pdfs-{test}-{}", std::process::id()))
    }

    /// A sample of a video small enough to come whole.
    fn whole(bytes: &[u8]) -> VideoSample {
        VideoSample {
            size: bytes.len() as u64,
            head: bytes.to_vec(),
            index: None,
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// A Drive revision of `size` bytes, zero except for the parts written in,
    /// that counts the bytes read from it. Stands for a recording far larger
    /// than a test should hold in memory.
    struct FakeRevision {
        size: u64,
        parts: Vec<(u64, Vec<u8>)>,
        read: AtomicU64,
    }

    impl FakeRevision {
        fn new(size: u64) -> Self {
            Self {
                size,
                parts: Vec::new(),
                read: AtomicU64::new(0),
            }
        }

        fn with(mut self, offset: u64, bytes: Vec<u8>) -> Self {
            self.parts.push((offset, bytes));
            self
        }
    }

    #[async_trait::async_trait]
    impl RevisionRead for FakeRevision {
        fn size(&self) -> u64 {
            self.size
        }

        fn block_sizes(&self) -> &[u64] {
            &[]
        }

        async fn read_at(&self, offset: u64, length: u64) -> DriveResult<Vec<u8>> {
            let end = offset.saturating_add(length).min(self.size);
            let mut out = vec![0u8; end.saturating_sub(offset) as usize];
            for (start, bytes) in &self.parts {
                let part_end = start + bytes.len() as u64;
                let (from, to) = (offset.max(*start), end.min(part_end));
                if from < to {
                    out[(from - offset) as usize..(to - offset) as usize]
                        .copy_from_slice(&bytes[(from - start) as usize..(to - start) as usize]);
                }
            }
            self.read.fetch_add(out.len() as u64, Ordering::Relaxed);
            Ok(out)
        }
    }

    /// An ISO media box of `len` bytes, of which only the header is returned.
    fn iso_box_header(kind: &[u8; 4], len: u64) -> Vec<u8> {
        if let Ok(len) = u32::try_from(len) {
            [&len.to_be_bytes()[..], kind].concat()
        } else {
            [&1u32.to_be_bytes()[..], kind, &len.to_be_bytes()[..]].concat()
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn a_large_stream_video_downloads_its_head_alone() {
        let mut head = vec![0u8; 188 * 2];
        head[0] = 0x47;
        head[188] = 0x47;
        let revision = FakeRevision::new(9 * GIB).with(0, head);
        let sample = block_on(sample_video(&revision)).unwrap();
        assert_eq!(sample.size, 9 * GIB);
        assert_eq!(sample.head.len() as u64, VIDEO_HEAD_BYTES);
        assert!(sample.index.is_none());
        assert!(sample.is_transport_stream());
        assert_eq!(revision.read.load(Ordering::Relaxed), VIDEO_HEAD_BYTES);
    }

    /// A camera MP4 keeps its index after gigabytes of media data. The sample
    /// is the head, the box headers on the way, and the index itself.
    #[test]
    fn a_large_mp4_downloads_its_head_and_its_index() {
        let ftyp = [
            iso_box_header(b"ftyp", 24),
            b"isom\0\0\0\0mp41isom".to_vec(),
        ]
        .concat();
        let mdat_len = 6 * GIB;
        let moov_at = ftyp.len() as u64 + mdat_len;
        let moov = [iso_box_header(b"moov", 4096), vec![7u8; 4088]].concat();
        let size = moov_at + moov.len() as u64;
        let revision = FakeRevision::new(size)
            .with(0, ftyp.clone())
            .with(ftyp.len() as u64, iso_box_header(b"mdat", mdat_len))
            .with(moov_at, moov.clone());

        let sample = block_on(sample_video(&revision)).unwrap();
        assert_eq!(sample.index, Some((moov_at, moov)));
        assert!(
            revision.read.load(Ordering::Relaxed) <= VIDEO_HEAD_BYTES + 4096 + 16,
            "read {} bytes",
            revision.read.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn an_mp4_with_its_index_up_front_downloads_its_head_alone() {
        let ftyp = [iso_box_header(b"ftyp", 16), b"isom\0\0\0\0".to_vec()].concat();
        let moov = [iso_box_header(b"moov", 64), vec![1u8; 56]].concat();
        let revision = FakeRevision::new(GIB)
            .with(0, ftyp.clone())
            .with(ftyp.len() as u64, moov)
            .with(80, iso_box_header(b"mdat", GIB - 80));
        let sample = block_on(sample_video(&revision)).unwrap();
        assert!(sample.index.is_none());
        assert_eq!(revision.read.load(Ordering::Relaxed), VIDEO_HEAD_BYTES);
    }

    #[test]
    fn a_small_video_downloads_whole() {
        let revision = FakeRevision::new(VIDEO_WHOLE_MAX_BYTES).with(0, vec![3u8; 64]);
        let sample = block_on(sample_video(&revision)).unwrap();
        assert_eq!(sample.head.len() as u64, VIDEO_WHOLE_MAX_BYTES);
        assert!(sample.index.is_none());
    }

    /// `.ts` that is TypeScript reaches ffmpeg never: the sync byte is missing,
    /// and that is a final answer, not a retry.
    #[test]
    fn a_ts_without_the_sync_byte_is_no_video() {
        let attempt = video_sample_thumbnail(
            &whole(b"export const answer = 42;"),
            "bundle.ts",
            &std::env::temp_dir(),
            OsStr::new("pdfs-ffmpeg-deliberately-absent"),
            Duration::from_millis(10),
        );
        assert!(matches!(attempt, ScaleAttempt::Undecodable));
    }

    /// ffmpeg reads the index where the file's own boxes put it, so the staging
    /// file has the full length and the index at its offset.
    #[test]
    fn the_index_is_staged_at_its_own_offset() {
        let tmp = scratch("video-index");
        let frame = tmp.join("frame.jpg");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&frame, jpeg(640, 360)).unwrap();
        let size: u64 = 3 * GIB;
        let at: u64 = size - 4096;
        // Arguments: -nostdin -loglevel error -ss <seek> -i <path> …
        let ffmpeg = fake_ffmpeg(
            &tmp,
            &format!(
                "[ \"$(stat -c %s \"$7\")\" = {size} ] && \
                 [ \"$(tail -c +{} \"$7\" | head -c 4)\" = moov ] && cat '{}'",
                // The box type, after its 4-byte length; `tail` counts from 1.
                at + 5,
                frame.display()
            ),
        );
        let sample = VideoSample {
            size,
            head: vec![0u8; 1024],
            index: Some((
                at,
                [iso_box_header(b"moov", 4096), vec![0u8; 4088]].concat(),
            )),
        };
        let attempt = video_sample_thumbnail(
            &sample,
            "clip.mp4",
            &tmp,
            ffmpeg.as_os_str(),
            Duration::from_secs(10),
        );
        std::fs::remove_dir_all(&tmp).unwrap();
        assert!(matches!(attempt, ScaleAttempt::Made(_)));
    }

    #[test]
    fn a_video_frame_becomes_a_thumbnail() {
        let tmp = scratch("video-frame");
        let frame = tmp.join("frame.jpg");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&frame, jpeg(1080, 1920)).unwrap();
        let ffmpeg = fake_ffmpeg(&tmp, &format!("cat '{}'", frame.display()));

        let ScaleAttempt::Made(thumb) = video_sample_thumbnail(
            &whole(b"video bytes"),
            "clip.mp4",
            &tmp,
            ffmpeg.as_os_str(),
            Duration::from_secs(10),
        ) else {
            panic!("a frame scales");
        };
        assert!((thumb.ratio - 1080.0 / 1920.0).abs() < 1e-6);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn a_video_ffmpeg_cannot_read_is_a_permanent_miss() {
        let tmp = scratch("video-broken");
        let ffmpeg = fake_ffmpeg(&tmp, "exit 1");
        let attempt = video_sample_thumbnail(
            &whole(b"not a video"),
            "clip.mp4",
            &tmp,
            ffmpeg.as_os_str(),
            Duration::from_secs(10),
        );
        assert!(matches!(attempt, ScaleAttempt::Undecodable));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn missing_ffmpeg_is_retryable() {
        let attempt = video_sample_thumbnail(
            &whole(b"video bytes"),
            "clip.mp4",
            &std::env::temp_dir(),
            OsStr::new("pdfs-ffmpeg-deliberately-absent"),
            Duration::from_millis(10),
        );
        assert!(matches!(attempt, ScaleAttempt::Unavailable));
    }

    /// Optional real-ffmpeg check, like the RAW fixture harness above.
    #[test]
    #[ignore = "set PDFS_VIDEO_FIXTURE to a video file; needs ffmpeg"]
    fn real_video_fixture_yields_a_frame() {
        let path = std::env::var_os("PDFS_VIDEO_FIXTURE").expect("PDFS_VIDEO_FIXTURE");
        let path = std::path::PathBuf::from(path);
        let name = path.file_name().unwrap().to_string_lossy();
        // Through the same partial download a Drive video gets.
        let revision = FakeRevision::new(std::fs::read(&path).unwrap().len() as u64)
            .with(0, std::fs::read(&path).unwrap());
        let sample = block_on(sample_video(&revision)).unwrap();
        let ScaleAttempt::Made(thumb) = video_sample_thumbnail(
            &sample,
            &name,
            &std::env::temp_dir(),
            OsStr::new("ffmpeg"),
            Duration::from_secs(30),
        ) else {
            panic!("a frame from the video");
        };
        assert!(thumb.ratio > 0.0);
    }

    /// Optional check against real songs, through the same partial download a
    /// Drive song gets. `PDFS_AUDIO_FIXTURE` lists files separated by `:`.
    #[test]
    #[ignore = "set PDFS_AUDIO_FIXTURE to songs with a cover in their tags"]
    fn real_audio_fixture_yields_its_cover() {
        let paths = std::env::var_os("PDFS_AUDIO_FIXTURE").expect("PDFS_AUDIO_FIXTURE");
        for path in std::env::split_paths(&paths) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path).unwrap();
            let revision = FakeRevision::new(bytes.len() as u64).with(0, bytes);
            let sample = block_on(sample_audio(&revision, &name)).unwrap();
            let read = revision.read.load(Ordering::Relaxed);
            let attempt = audio_cover_thumbnail(&sample, &name, &std::env::temp_dir());
            assert!(
                matches!(attempt, ScaleAttempt::Made(_)),
                "{name}: no cover, read {read} of {} bytes",
                revision.size
            );
            eprintln!("{name}: read {read} of {} bytes", revision.size);
        }
    }

    #[test]
    fn videos_are_told_apart_by_name() {
        for name in [
            "VID-20260804-WA0023.mp4",
            "PXL_1.TS.mp4",
            "clip.MOV",
            "a.webm",
        ] {
            assert!(is_video_name(name), "{name}");
        }
        for name in ["photo.jpg", "shot.NEF", "notes.txt", "mp4"] {
            assert!(!is_video_name(name), "{name}");
        }
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

    #[test]
    fn thumbnails_hash_alike_across_sizes_and_undecodable_ones_get_no_hash() {
        let tmp = scratch("similar-hashes");
        std::fs::create_dir_all(&tmp).unwrap();
        // One picture at two sizes: a diagonal gradient with a bright corner.
        let picture = |width: u32, height: u32| {
            let image = image::GrayImage::from_fn(width, height, |x, y| {
                let (fx, fy) = (x * 4 / width, y * 4 / height);
                image::Luma([if fx + fy == 0 {
                    250
                } else {
                    (fx * 40 + fy * 20) as u8
                }])
            });
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new(&mut bytes)
                .encode_image(&image)
                .unwrap();
            bytes
        };
        std::fs::write(tmp.join("big"), picture(512, 384)).unwrap();
        std::fs::write(tmp.join("small"), picture(256, 192)).unwrap();
        std::fs::write(tmp.join("torn"), b"not a picture").unwrap();

        let hashes: HashMap<String, u64> = similar_hashes(
            ["big", "small", "torn", "gone"]
                .iter()
                .map(|name| (name.to_string(), tmp.join(name)))
                .collect(),
        )
        .into_iter()
        .collect();
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(hashes.len(), 4);
        assert_ne!(hashes["big"], pdfs_core::similar::NO_HASH);
        assert!(pdfs_core::similar::alike(hashes["big"], hashes["small"]));
        assert_eq!(hashes["torn"], pdfs_core::similar::NO_HASH);
        assert_eq!(hashes["gone"], pdfs_core::similar::NO_HASH);
    }

    #[test]
    fn an_uploaded_photo_carries_its_thumbnail() {
        let tmp = scratch("upload-photo");
        std::fs::create_dir_all(&tmp).unwrap();
        let photo = tmp.join("staged-blob");
        std::fs::write(&photo, jpeg(4000, 3000)).unwrap();

        let thumbnails = upload_thumbnails_with(&photo, "IMG_0001.jpg", &tmp, OsStr::new("ffmpeg"));
        let _ = std::fs::remove_dir_all(&tmp);
        let [thumbnail] = thumbnails.as_slice() else {
            panic!("one thumbnail, got {}", thumbnails.len());
        };
        assert!(matches!(thumbnail.thumbnail_type, ThumbnailType::Thumbnail));
        assert!(thumbnail.content.len() <= UPLOAD_THUMB_MAX_BYTES);
        assert_eq!(ratio_of(&thumbnail.content), Some(4.0 / 3.0));
    }

    #[test]
    fn an_uploaded_video_carries_a_frame() {
        let tmp = scratch("upload-video");
        let frame = tmp.join("frame.jpg");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&frame, jpeg(1080, 1920)).unwrap();
        std::fs::write(tmp.join("clip"), b"video bytes").unwrap();
        let ffmpeg = fake_ffmpeg(&tmp, &format!("cat '{}'", frame.display()));

        let thumbnails = upload_thumbnails_with(
            &tmp.join("clip"),
            "VID-WA0001.mp4",
            &tmp,
            ffmpeg.as_os_str(),
        );
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(thumbnails.len(), 1);
        assert!(ratio_of(&thumbnails[0].content).is_some_and(|ratio| ratio < 1.0));
    }

    #[test]
    fn other_files_upload_without_a_thumbnail() {
        let tmp = scratch("upload-other");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("file"), b"export const answer = 42;\n").unwrap();
        // An ffmpeg that would hand back a frame, to prove it is never asked.
        std::fs::write(tmp.join("frame.jpg"), jpeg(64, 64)).unwrap();
        let ffmpeg = fake_ffmpeg(&tmp, &format!("cat '{}'", tmp.join("frame.jpg").display()));

        for name in ["notes.txt", "answer.ts", "broken.jpg"] {
            assert!(
                upload_thumbnails_with(&tmp.join("file"), name, &tmp, ffmpeg.as_os_str())
                    .is_empty(),
                "{name} uploads bare"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_oversized_thumbnail_is_squeezed_under_the_upload_limit() {
        // Noise is the worst case for JPEG: at the gallery's quality a 512 px
        // square of it lands far over the limit.
        let mut seed = 0x2545_f491_u32;
        let noise = image::RgbImage::from_fn(THUMB_EDGE, THUMB_EDGE, |_, _| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let [r, g, b, _] = seed.to_le_bytes();
            image::Rgb([r, g, b])
        });
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, super::THUMB_QUALITY)
            .encode_image(&noise)
            .unwrap();
        assert!(
            bytes.len() > UPLOAD_THUMB_MAX_BYTES,
            "the fixture is oversized"
        );

        let fitted = fit_upload_thumb(bytes).expect("noise fits at half size");
        assert!(fitted.len() <= UPLOAD_THUMB_MAX_BYTES);
        assert_eq!(ratio_of(&fitted), Some(1.0), "shrinking keeps the shape");
        let small = jpeg(64, 64);
        assert_eq!(
            fit_upload_thumb(small.clone()),
            Some(small),
            "a small one is untouched"
        );
    }

    /// A `width`×`height` PNG, the cover a tag carries.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    /// An ID3v2.3 tag holding `picture` as its front cover.
    fn id3_with_cover(picture: &[u8]) -> Vec<u8> {
        let mut body = vec![0u8];
        body.extend_from_slice(b"image/png\0");
        body.push(3);
        body.push(0);
        body.extend_from_slice(picture);
        let mut frame = b"APIC".to_vec();
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&body);
        let len = frame.len() as u32;
        let mut tag = b"ID3\x03\x00\x00".to_vec();
        tag.extend((0..4).rev().map(|i| ((len >> (7 * i)) & 0x7f) as u8));
        tag.extend_from_slice(&frame);
        tag
    }

    /// Ten silent MPEG-1 Layer III frames at 128 kbit/s and 44.1 kHz.
    fn mpeg_frames() -> Vec<u8> {
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        frame.repeat(10)
    }

    /// A FLAC stream's metadata: STREAMINFO, then `picture` as its front cover.
    fn flac_with_cover(picture: &[u8]) -> Vec<u8> {
        let mut info = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
        let packed: u64 = (44_100 << 44) | (1 << 41) | (15 << 36);
        info.extend_from_slice(&packed.to_be_bytes());
        info.extend_from_slice(&[0; 16]);
        let mut block = Vec::new();
        block.extend_from_slice(&3u32.to_be_bytes());
        block.extend_from_slice(&9u32.to_be_bytes());
        block.extend_from_slice(b"image/png");
        block.extend_from_slice(&0u32.to_be_bytes());
        for value in [16u32, 8, 24, 0, picture.len() as u32] {
            block.extend_from_slice(&value.to_be_bytes());
        }
        block.extend_from_slice(picture);
        let mut out = b"fLaC".to_vec();
        out.push(0);
        out.extend_from_slice(&(info.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(&info);
        out.push(0x80 | 6);
        out.extend_from_slice(&(block.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(&block);
        out
    }

    fn whole_song(bytes: Vec<u8>) -> AudioSample {
        AudioSample {
            size: bytes.len() as u64,
            parts: vec![(0, bytes)],
        }
    }

    #[test]
    fn a_song_shows_the_cover_in_its_tags() {
        let tmp = scratch("song-cover");
        std::fs::create_dir_all(&tmp).unwrap();
        let mut mp3 = id3_with_cover(&png(16, 8));
        mp3.extend(mpeg_frames());
        let ScaleAttempt::Made(thumb) = audio_cover_thumbnail(&whole_song(mp3), "song.mp3", &tmp)
        else {
            panic!("an ID3 cover scales");
        };
        assert!((thumb.ratio - 2.0).abs() < 1e-6);

        let ScaleAttempt::Made(thumb) =
            audio_cover_thumbnail(&whole_song(flac_with_cover(&png(8, 16))), "song.flac", &tmp)
        else {
            panic!("a FLAC picture scales");
        };
        assert!((thumb.ratio - 0.5).abs() < 1e-6);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn a_song_without_a_cover_is_no_thumbnail_of_its_own() {
        let tmp = scratch("song-bare");
        std::fs::create_dir_all(&tmp).unwrap();
        let attempt = audio_cover_thumbnail(&whole_song(mpeg_frames()), "song.mp3", &tmp);
        assert!(matches!(attempt, ScaleAttempt::Undecodable));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn the_start_of_a_song_says_where_its_tags_end() {
        let tag = id3_with_cover(&png(4, 4));
        assert_eq!(audio_tags_end(&tag), Some(tag.len() as u64));
        let flac = flac_with_cover(&png(4, 4));
        assert_eq!(audio_tags_end(&flac), Some(flac.len() as u64));
        // FLAC after an ID3 tag, as some taggers write it.
        let both = [tag.clone(), flac.clone()].concat();
        assert_eq!(audio_tags_end(&both), Some(both.len() as u64));
        // The picture's block header cut off by the end of the probe, just
        // past STREAMINFO: a guess past it.
        let cut = &flac[..4 + 4 + 34 + 2];
        assert!(audio_tags_end(cut).is_some_and(|end| end > cut.len() as u64));
        assert_eq!(audio_tags_end(b"OggS\0\0\0\0"), None);
        assert_eq!(
            audio_tags_end(b"ID3\x03\x00\x00\x80\0\0\0"),
            None,
            "not synchsafe"
        );
    }

    #[test]
    fn a_large_song_downloads_its_tags_not_its_audio() {
        const SIZE: u64 = 300 << 20;
        let mut start = id3_with_cover(&png(16, 8));
        start.extend(mpeg_frames());
        let revision = FakeRevision::new(SIZE).with(0, start);
        let sample = block_on(sample_audio(&revision, "long mix.mp3")).unwrap();
        let read = revision.read.load(Ordering::Relaxed);
        assert!(read < 1 << 20, "read {read} bytes");

        let tmp = scratch("song-sparse");
        std::fs::create_dir_all(&tmp).unwrap();
        let ScaleAttempt::Made(thumb) = audio_cover_thumbnail(&sample, "long mix.mp3", &tmp) else {
            panic!("the cover comes from the start alone");
        };
        assert!((thumb.ratio - 2.0).abs() < 1e-6);
        std::fs::remove_dir_all(&tmp).unwrap();

        // Ogg says nothing up front: a bounded head. WavPack keeps its tags at
        // the end: the tail too.
        let revision = FakeRevision::new(SIZE);
        block_on(sample_audio(&revision, "live.ogg")).unwrap();
        assert!(revision.read.load(Ordering::Relaxed) <= AUDIO_HEAD_BYTES + (64 << 10));
        let revision = FakeRevision::new(SIZE);
        let sample = block_on(sample_audio(&revision, "live.wv")).unwrap();
        assert!(
            sample
                .parts
                .iter()
                .any(|(offset, bytes)| *offset == SIZE - AUDIO_TAIL_BYTES
                    && bytes.len() as u64 == AUDIO_TAIL_BYTES)
        );
    }

    #[test]
    fn an_uploaded_song_carries_its_cover() {
        let tmp = scratch("upload-song");
        std::fs::create_dir_all(&tmp).unwrap();
        // A staging blob has no extension: the format comes from the bytes.
        let blob = tmp.join("blob");
        let mut mp3 = id3_with_cover(&png(64, 64));
        mp3.extend(mpeg_frames());
        std::fs::write(&blob, mp3).unwrap();
        let ffmpeg = fake_ffmpeg(&tmp, "exit 1");
        let thumbs = upload_thumbnails_with(&blob, "song.mp3", &tmp, ffmpeg.as_os_str());
        let [thumbnail] = thumbs.as_slice() else {
            panic!("one thumbnail, got {}", thumbs.len());
        };
        assert!(matches!(thumbnail.thumbnail_type, ThumbnailType::Thumbnail));
        assert_eq!(ratio_of(&thumbnail.content), Some(1.0));

        std::fs::write(&blob, mpeg_frames()).unwrap();
        assert!(
            upload_thumbnails_with(&blob, "song.mp3", &tmp, ffmpeg.as_os_str()).is_empty(),
            "a song without a cover uploads bare"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_folder_cover_is_found_by_its_usual_names() {
        assert_eq!(cover_rank("cover.jpg"), Some(0));
        assert_eq!(cover_rank("Cover.JPG"), Some(0));
        assert_eq!(cover_rank("folder.png"), Some(1));
        assert_eq!(cover_rank("AlbumArtSmall.jpg"), Some(5));
        assert!(cover_rank("cover.jpg") < cover_rank("front.webp"));
        for name in [
            "cover.txt",
            "my cover.jpg",
            "cover",
            "back.jpg",
            "cover.jpg.part",
        ] {
            assert_eq!(cover_rank(name), None, "{name}");
        }
    }
}
