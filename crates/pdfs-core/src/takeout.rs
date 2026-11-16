//! Reading a Google Photos **Takeout** export: what is in the archives, which
//! album each photo belongs to, and when it was taken.
//!
//! This module is pure structure — it opens the archives' central directories
//! and reads the small JSON sidecars, never the media bytes. The import itself
//! (hashing, duplicate detection, upload, album creation) lives in the daemon;
//! everything here is offline-testable and has no Proton dependency.
//!
//! # What a Takeout export looks like
//!
//! Google splits an export into numbered zips (`takeout-20260817T...-001.zip`,
//! `-002.zip`, …) that are *independent* archives — no zip64 spanning — but a
//! photo and its JSON sidecar can land in different parts, and one album's
//! folder can be spread over several. So the scan treats the whole set as one
//! namespace keyed by the path inside the export:
//!
//! ```text
//! Takeout/Google Photos/Iceland 2019/IMG_0042.jpg
//! Takeout/Google Photos/Iceland 2019/IMG_0042.jpg.supplemental-metadata.json
//! Takeout/Google Photos/Iceland 2019/metadata.json        <- album title
//! Takeout/Google Photos/Photos from 2019/IMG_0043.jpg     <- no album
//! Takeout/Google Photos/Trash/IMG_0044.jpg                <- skipped
//! ```
//!
//! The service folder (`Google Photos`) is localized, as are the year folders
//! (`Photos from 2019`) and the trash folder, so nothing here matches on the
//! service name: the first component is dropped when it is the export root, the
//! second is the service, and the rest is `<folder>/<file>`. The Photos service
//! is the one whose sidecars carry a `photoTakenTime`; other services in the same
//! export (`Google Drive/`, `YouTube/`) are left out. A folder is an
//! **album** when it carries an album metadata sidecar naming it; anything else
//! is a plain bucket that imports into the timeline only. Trash is matched by
//! name against [`TRASH_FOLDERS`] — the one place a localized name is
//! unavoidable, because a trashed photo is not marked as such anywhere else.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

/// Folder names Google uses for the trash bucket, lowercased. A Takeout export
/// only contains one when the user asked for it, and its photos are deleted —
/// importing them would resurrect them, so they are skipped.
///
/// Localized, and necessarily incomplete: a locale not listed here imports its
/// trash as ordinary timeline photos. Extend as reports come in rather than
/// guessing at transliterations.
pub const TRASH_FOLDERS: &[&str] = &[
    "trash",
    "bin",
    "papierkorb",
    "corbeille",
    "papelera",
    "papelera de reciclaje",
    "cestino",
    "lixeira",
    "prullenbak",
    "kosz",
    "koš",
    "skräpkorgen",
    "papperskorg",
    "papirkurv",
    "roskakori",
    "kuka",
    "回收站",
    "回收筒",
    "ゴミ箱",
    "휴지통",
    "корзина",
    "кошик",
    "çöp kutusu",
    "σκουπίδια",
];

/// Sidecar file names that describe the *folder* (its album title) rather than
/// one photo, lowercased. Google has renamed this file more than once.
const ALBUM_METADATA_NAMES: &[&str] = &[
    "metadata.json",
    "album-metadata.json",
    "print-subscriptions.json",
    "shared_album_comments.json",
    "user-generated-memory-titles.json",
];

/// The suffix Google appends to a photo's sidecar, lowercased. Truncation means
/// only a *prefix* of it may survive (see [`strip_sidecar_suffix`]).
const SUPPLEMENTAL: &str = ".supplemental-metadata";

/// File extensions imported as photos or videos. Anything else in the export —
/// `archive_browser.html`, `.csv` activity logs, `print-subscriptions.json` —
/// is not media and is ignored.
const MEDIA_EXTENSIONS: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("avif", "image/avif"),
    ("dng", "image/x-adobe-dng"),
    ("cr2", "image/x-canon-cr2"),
    ("cr3", "image/x-canon-cr3"),
    ("nef", "image/x-nikon-nef"),
    ("arw", "image/x-sony-arw"),
    ("orf", "image/x-olympus-orf"),
    ("rw2", "image/x-panasonic-rw2"),
    ("raf", "image/x-fuji-raf"),
    ("mp4", "video/mp4"),
    ("m4v", "video/x-m4v"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
    ("3gp", "video/3gpp"),
    ("mts", "video/mp2t"),
    ("m2ts", "video/mp2t"),
    ("mpg", "video/mpeg"),
    ("mpeg", "video/mpeg"),
    // The video half of a Pixel motion photo, `PXL_….MP` beside `PXL_….MP.jpg`.
    ("mp", "video/mp4"),
];

/// Sidecars bigger than this are not read. A real one is a few kilobytes; a
/// `.json` this large is some other service's data and not worth the memory.
const SIDECAR_MAX_BYTES: u64 = 1024 * 1024;

/// The media type for `name`'s extension, or `None` when it is not media this
/// import handles.
pub fn media_type_of(name: &str) -> Option<&'static str> {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    MEDIA_EXTENSIONS
        .iter()
        .find(|(candidate, _)| *candidate == ext)
        .map(|(_, media_type)| *media_type)
}

/// Where a photo sits in the export, which decides whether it joins an album.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bucket {
    /// A named album folder — the photo joins that album *and* the timeline.
    Album(String),
    /// A year folder (`Photos from 2019`) or the export root: timeline only.
    Timeline,
    /// The archive bucket: timeline only, like Google shows it.
    Archived,
}

/// A position in WGS 84 decimal degrees, from a sidecar's `geoData`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoPoint {
    pub latitude: f64,
    pub longitude: f64,
}

/// One media file found in the export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TakeoutPhoto {
    /// Index into the archive list the scan was given.
    pub archive: usize,
    /// The entry's full path inside that archive — how the importer reopens it.
    pub entry: String,
    /// The name to upload under: the sidecar's `title` when it has one (Google
    /// mangles long names on disk but keeps the original there), else the file
    /// name on disk.
    pub name: String,
    /// Uncompressed size in bytes, from the zip's central directory.
    pub size: u64,
    /// Media type from the extension.
    pub media_type: String,
    /// Capture time in epoch seconds, from the sidecar's `photoTakenTime`,
    /// else from a date in the file name. `None` when neither has one; the
    /// importer then tries the file's EXIF, then [`creation_time`](Self::creation_time).
    pub capture_time: Option<i64>,
    /// The sidecar's `creationTime`: when the photo reached Google Photos, not
    /// when it was taken. Only a last resort for the capture time.
    #[serde(default)]
    pub creation_time: Option<i64>,
    /// Where the photo was taken, from the sidecar's `geoData` (or
    /// `geoDataExif`). Google writes `0, 0` for "unknown", which is dropped.
    #[serde(default)]
    pub location: Option<GeoPoint>,
    /// The sidecar's `favorited` flag.
    pub favorite: bool,
    /// Which bucket the file was found in.
    pub bucket: Bucket,
    /// Pairs the halves of a live or motion photo: a still and a video in the
    /// same folder carry the same key (`IMG_1.HEIC` + `IMG_1.MOV`,
    /// `PXL_1.MP.jpg` + `PXL_1.MP`). `None` for media that cannot be either half.
    #[serde(default)]
    pub live_key: Option<String>,
}

impl TakeoutPhoto {
    /// Whether this is the video half of a pair, as opposed to the still.
    pub fn is_video(&self) -> bool {
        self.media_type.starts_with("video/")
    }

    /// Whether the pair this belongs to is a Pixel motion photo (`.MP`) rather
    /// than an Apple live photo.
    pub fn is_motion_photo(&self) -> bool {
        self.live_key
            .as_deref()
            .is_some_and(|key| key.ends_with(".mp"))
    }
}

/// An archive the scan had to leave out, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableArchive {
    pub path: PathBuf,
    pub reason: String,
}

/// Everything one scan found, ready to be turned into an import plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TakeoutScan {
    /// Every importable media file, in archive then entry order.
    pub photos: Vec<TakeoutPhoto>,
    /// Album names in the order first seen, so the importer creates them in a
    /// stable order.
    pub albums: Vec<String>,
    /// Media files skipped because they sit in the trash bucket.
    pub skipped_trashed: usize,
    /// Entries that were neither media nor a sidecar (`archive_browser.html`,
    /// activity `.csv`s), and media from other Google services in the same
    /// export (`Google Drive/`, `YouTube/`).
    pub skipped_other: usize,
    /// Archives that could not be opened or are not a zip. The rest of the
    /// export is still imported.
    #[serde(default)]
    pub unreadable_archives: Vec<UnreadableArchive>,
    /// Entries inside an otherwise readable archive that could not be read.
    #[serde(default)]
    pub unreadable_entries: usize,
}

impl TakeoutScan {
    /// Total bytes of media to be read, before duplicate detection.
    pub fn total_bytes(&self) -> u64 {
        self.photos.iter().map(|p| p.size).sum()
    }
}

/// Google's per-photo sidecar. Only the fields the import uses are read; the
/// file also carries people tags, a description and view counts.
#[derive(Debug, Default, Deserialize)]
struct PhotoSidecar {
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "photoTakenTime", default)]
    photo_taken_time: Option<SidecarTime>,
    #[serde(rename = "creationTime", default)]
    creation_time: Option<SidecarTime>,
    #[serde(default)]
    favorited: Option<bool>,
    /// The position as edited in Google Photos, else as the camera wrote it.
    #[serde(rename = "geoData", default)]
    geo_data: Option<SidecarGeo>,
    #[serde(rename = "geoDataExif", default)]
    geo_data_exif: Option<SidecarGeo>,
}

impl PhotoSidecar {
    fn location(&self) -> Option<GeoPoint> {
        self.geo_data
            .as_ref()
            .and_then(SidecarGeo::point)
            .or_else(|| self.geo_data_exif.as_ref().and_then(SidecarGeo::point))
    }
}

/// A sidecar timestamp: epoch seconds, written as a string, sometimes a number.
#[derive(Debug, Deserialize)]
struct SidecarTime {
    #[serde(default)]
    timestamp: Option<SidecarNumber>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SidecarNumber {
    Number(i64),
    Text(String),
}

impl SidecarTime {
    /// The timestamp, when it is a real moment. `0` and negative values are
    /// Google's "unknown" and would file the photo under 1970.
    fn epoch_seconds(&self) -> Option<i64> {
        let seconds = match self.timestamp.as_ref()? {
            SidecarNumber::Number(seconds) => *seconds,
            SidecarNumber::Text(text) => text.trim().parse().ok()?,
        };
        (seconds > 0).then_some(seconds)
    }
}

/// `geoData` in a sidecar. All-zero means Google has no position.
#[derive(Debug, Deserialize)]
struct SidecarGeo {
    #[serde(default)]
    latitude: Option<f64>,
    #[serde(default)]
    longitude: Option<f64>,
}

impl SidecarGeo {
    fn point(&self) -> Option<GeoPoint> {
        let (latitude, longitude) = (self.latitude?, self.longitude?);
        let on_globe = latitude.is_finite()
            && longitude.is_finite()
            && (-90.0..=90.0).contains(&latitude)
            && (-180.0..=180.0).contains(&longitude);
        (on_globe && (latitude != 0.0 || longitude != 0.0)).then_some(GeoPoint {
            latitude,
            longitude,
        })
    }
}

/// A folder-level sidecar. `title` is the album name as the user typed it,
/// which is what the folder name is derived (and sometimes truncated) from.
#[derive(Debug, Deserialize)]
struct AlbumSidecar {
    #[serde(default)]
    title: Option<String>,
}

/// One entry as the scan sees it, independent of where it came from.
struct RawEntry {
    archive: usize,
    entry: String,
    /// `<folder>/<file>` with the export root and service folder removed.
    relative: String,
    size: u64,
}

impl RawEntry {
    /// The folder component of `relative`, or `""` at the service root.
    fn folder(&self) -> &str {
        match self.relative.rfind('/') {
            Some(index) => &self.relative[..index],
            None => "",
        }
    }

    /// The file name component of `relative`.
    fn file_name(&self) -> &str {
        match self.relative.rfind('/') {
            Some(index) => &self.relative[index + 1..],
            None => &self.relative,
        }
    }
}

/// Scan a set of Takeout zip archives into one plan.
///
/// The archives are treated as one export: an album folder split across parts
/// is one album, and a sidecar in part 3 is matched to its photo in part 1.
/// Reads only the central directory and the JSON sidecars — no media bytes, so
/// this stays fast on a 200 GB export.
///
/// An archive that cannot be opened is left out and listed in
/// [`TakeoutScan::unreadable_archives`]; the rest still imports.
pub fn scan_archives(archives: &[PathBuf]) -> CoreResult<TakeoutScan> {
    scan_archives_until(archives, || false)
}

/// [`scan_archives`] that gives up when `stop` returns true, checked between
/// archives and every few hundred entries.
pub fn scan_archives_until(
    archives: &[PathBuf],
    stop: impl Fn() -> bool,
) -> CoreResult<TakeoutScan> {
    let mut entries: Vec<(String, RawEntry)> = Vec::new();
    let mut sidecars: Vec<(String, String, Vec<u8>)> = Vec::new();
    let mut unreadable_archives = Vec::new();
    let mut unreadable_entries = 0;

    for (index, path) in archives.iter().enumerate() {
        if stop() {
            return Err(CoreError::conflict("the import was cancelled"));
        }
        let opened = std::fs::File::open(path)
            .map_err(|e| e.to_string())
            .and_then(|file| {
                zip::ZipArchive::new(std::io::BufReader::new(file))
                    .map_err(|e| format!("not a readable zip: {e}"))
            });
        let mut zip = match opened {
            Ok(zip) => zip,
            Err(reason) => {
                tracing::warn!(archive = %path.display(), %reason, "skipping an unreadable takeout archive");
                unreadable_archives.push(UnreadableArchive {
                    path: path.clone(),
                    reason,
                });
                continue;
            }
        };

        for position in 0..zip.len() {
            if position % 512 == 0 && stop() {
                return Err(CoreError::conflict("the import was cancelled"));
            }
            let mut entry = match zip.by_index(position) {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::debug!(archive = %path.display(), position, error = %e, "unreadable takeout entry");
                    unreadable_entries += 1;
                    continue;
                }
            };
            if entry.is_dir() {
                continue;
            }
            // `enclosed_name` refuses paths that escape the archive root, which
            // is exactly the check we want before using one as a key.
            let Some(name) = entry.enclosed_name() else {
                continue;
            };
            let name = name.to_string_lossy().replace('\\', "/");
            let Some((service, relative)) = strip_export_prefix(&name) else {
                continue;
            };
            let (service, relative) = (service.to_string(), relative.to_string());
            let size = entry.size();

            // Sidecars are read now, while the entry is open: they are a few
            // hundred bytes each and reopening the archive per photo later would
            // be a second full pass.
            if relative.to_ascii_lowercase().ends_with(".json") {
                if size <= SIDECAR_MAX_BYTES {
                    let mut body = Vec::with_capacity(size as usize);
                    if entry.read_to_end(&mut body).is_ok() {
                        sidecars.push((service, relative, body));
                    } else {
                        unreadable_entries += 1;
                    }
                }
                continue;
            }
            entries.push((
                service,
                RawEntry {
                    archive: index,
                    entry: name,
                    relative,
                    size,
                },
            ));
        }
    }

    let (entries, sidecars, other_services) = keep_photos_services(entries, sidecars);
    let mut scan = assemble(entries, &sidecars);
    scan.skipped_other += other_services;
    scan.unreadable_archives = unreadable_archives;
    scan.unreadable_entries = unreadable_entries;
    Ok(scan)
}

/// Drop what other Google services put into the same export.
///
/// A Takeout can hold `Google Drive/`, `YouTube/` and others next to
/// `Google Photos/`, and their media must not land in the photo timeline. The
/// service folder's name is localized (`Google Fotos`), so the Photos service
/// is recognized by its content instead: it is the folder whose sidecars carry
/// a `photoTakenTime`. When no folder does — an export without sidecars, or
/// one re-zipped without the service level — everything is kept, as before.
///
/// Returns the entries and sidecars to import, and how many media files were
/// left out.
fn keep_photos_services(
    entries: Vec<(String, RawEntry)>,
    sidecars: Vec<(String, String, Vec<u8>)>,
) -> (Vec<RawEntry>, HashMap<String, Vec<u8>>, usize) {
    #[derive(Deserialize)]
    struct Marker {
        #[serde(rename = "photoTakenTime")]
        photo_taken_time: Option<serde::de::IgnoredAny>,
    }
    let mut photos_services: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (service, _, body) in &sidecars {
        if photos_services.contains(service.as_str()) {
            continue;
        }
        if serde_json::from_slice::<Marker>(body).is_ok_and(|m| m.photo_taken_time.is_some()) {
            photos_services.insert(service.as_str());
        }
    }
    let keep = |service: &str| photos_services.is_empty() || photos_services.contains(service);

    let mut skipped = 0;
    let mut kept_entries = Vec::with_capacity(entries.len());
    for (service, raw) in entries {
        if keep(&service) {
            kept_entries.push(raw);
        } else if media_type_of(raw.file_name()).is_some() {
            skipped += 1;
        }
    }
    let kept_sidecars = sidecars
        .iter()
        .filter(|(service, _, _)| keep(service))
        .map(|(_, relative, body)| (relative.clone(), body.clone()))
        .collect();
    (kept_entries, kept_sidecars, skipped)
}

/// Build the scan from raw entries plus the sidecar bodies keyed by their path
/// inside the export. Split out from [`scan_archives`] so the matching rules can
/// be tested without building zip fixtures.
fn assemble(entries: Vec<RawEntry>, sidecars: &HashMap<String, Vec<u8>>) -> TakeoutScan {
    // Album titles per folder, from the folder-level sidecars.
    let mut album_titles: HashMap<String, String> = HashMap::new();
    for (path, body) in sidecars {
        let (folder, file) = split_folder(path);
        if !ALBUM_METADATA_NAMES.contains(&file.to_ascii_lowercase().as_str()) {
            continue;
        }
        if let Ok(album) = serde_json::from_slice::<AlbumSidecar>(body)
            && let Some(title) = album.title.filter(|t| !t.trim().is_empty())
        {
            album_titles.insert(folder.to_string(), title);
        }
    }

    // Per-photo sidecars, indexed by folder so a truncated name only ever
    // matches within its own folder.
    let mut per_folder_sidecars: HashMap<String, BTreeMap<String, &Vec<u8>>> = HashMap::new();
    for (path, body) in sidecars {
        let (folder, file) = split_folder(path);
        if ALBUM_METADATA_NAMES.contains(&file.to_ascii_lowercase().as_str()) {
            continue;
        }
        per_folder_sidecars
            .entry(folder.to_string())
            .or_default()
            .insert(file.to_string(), body);
    }

    let indexes: HashMap<&str, SidecarIndex<'_>> = per_folder_sidecars
        .iter()
        .map(|(folder, in_folder)| (folder.as_str(), SidecarIndex::new(in_folder)))
        .collect();

    let mut scan = TakeoutScan::default();
    let mut seen_albums: HashMap<String, ()> = HashMap::new();

    for raw in &entries {
        let folder = raw.folder().to_string();
        let file_name = raw.file_name().to_string();

        if is_trash_folder(&folder) {
            if media_type_of(&file_name).is_some() {
                scan.skipped_trashed += 1;
            }
            continue;
        }
        let Some(media_type) = media_type_of(&file_name) else {
            scan.skipped_other += 1;
            continue;
        };

        let found = indexes
            .get(folder.as_str())
            .and_then(|index| find_sidecar(&file_name, index));
        // An edited copy keeps its own name: the title is its original's.
        let title_is_ours = found.is_some_and(|((rule, _), _)| rule != EDITED_COPY);
        let sidecar = found
            .map(|(_, body)| body)
            .and_then(|body| serde_json::from_slice::<PhotoSidecar>(body).ok())
            .unwrap_or_default();

        let bucket = match album_titles.get(&folder) {
            Some(title) => Bucket::Album(title.clone()),
            None if is_archive_folder(&folder) => Bucket::Archived,
            None => Bucket::Timeline,
        };
        if let Bucket::Album(title) = &bucket
            && seen_albums.insert(title.clone(), ()).is_none()
        {
            scan.albums.push(title.clone());
        }

        // The sidecar's `title` is the name Google had before it mangled the
        // on-disk one (truncation, `(1)` suffixes, character replacement), so it
        // is the better name to upload under — but only when it still looks like
        // the same file, i.e. the extension agrees.
        let name = sidecar
            .title
            .as_deref()
            .filter(|_| title_is_ours)
            .map(str::trim)
            .filter(|title| !title.is_empty() && same_extension(title, &file_name))
            .unwrap_or(&file_name)
            .to_string();

        // Sidecar first, then the date stamped into the name. `creationTime`
        // is when the photo reached Google, which for a scan or an old photo
        // uploaded late is years off; the importer uses it only after EXIF.
        let capture_time = sidecar
            .photo_taken_time
            .as_ref()
            .and_then(SidecarTime::epoch_seconds)
            .or_else(|| capture_time_from_name(&name))
            .or_else(|| capture_time_from_name(&file_name));
        let creation_time = sidecar
            .creation_time
            .as_ref()
            .and_then(SidecarTime::epoch_seconds);

        scan.photos.push(TakeoutPhoto {
            archive: raw.archive,
            entry: raw.entry.clone(),
            name,
            size: raw.size,
            media_type: media_type.to_string(),
            capture_time,
            creation_time,
            location: sidecar.location(),
            favorite: sidecar.favorited.unwrap_or(false),
            bucket,
            live_key: live_key(&folder, &file_name),
        });
    }

    scan
}

/// Split a path inside the archive into the localized service folder and the
/// rest, dropping the export root (`Takeout/`): `("Google Fotos",
/// "<folder>/<file>")`.
///
/// `None` for a path with no service folder at all (the export's own
/// `archive_browser.html`), which carries nothing to import.
fn strip_export_prefix(path: &str) -> Option<(&str, &str)> {
    let mut rest = path;
    // The root is `Takeout/`, but an export unzipped and re-zipped by the user
    // may not have it. `Google Photos/<album>/<file>` then has three parts like
    // `Takeout/Google Photos/<file>`, so the count alone cannot tell them
    // apart: a root of another name is only taken for one with four parts.
    let (first, after) = rest.split_once('/')?;
    let components = rest.split('/').count();
    if (first.eq_ignore_ascii_case("takeout") && components >= 3) || components >= 4 {
        rest = after;
    }
    // The service folder (`Google Photos`, `Google Fotos`, …).
    rest.split_once('/')
}

/// The key that pairs the still and the video of one live or motion photo, or
/// `None` when `file_name` cannot be either half.
///
/// Apple writes `IMG_1.HEIC` (or `.JPG`) and `IMG_1.MOV`, which share the stem.
/// A Pixel writes `PXL_1.MP.jpg` and `PXL_1.MP`; there the still's stem is the
/// video's whole name. The folder is part of the key so two cameras' `IMG_1`
/// in different albums never pair.
fn live_key(folder: &str, file_name: &str) -> Option<String> {
    let lower = file_name.to_lowercase();
    let (stem, ext) = lower.rsplit_once('.')?;
    let key = match ext {
        "heic" | "heif" | "jpg" | "jpeg" => stem,
        // Apple's video half. A plain `.mp4` beside a photo of the same name is
        // far more often an unrelated clip than half of a live photo.
        "mov" => stem,
        "mp" => lower.as_str(),
        _ => return None,
    };
    (!key.is_empty()).then(|| format!("{folder}/{key}"))
}

/// Split `<folder>/<file>` into its parts; the folder is `""` at the root.
fn split_folder(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(index) => (&path[..index], &path[index + 1..]),
        None => ("", path),
    }
}

fn is_trash_folder(folder: &str) -> bool {
    let name = folder.rsplit('/').next().unwrap_or(folder).to_lowercase();
    TRASH_FOLDERS.contains(&name.as_str())
}

/// Google's archive bucket, which is not an album but is not the plain timeline
/// either. Only the English name is matched; a missed locale simply imports as
/// [`Bucket::Timeline`], which is the same upload either way.
fn is_archive_folder(folder: &str) -> bool {
    let name = folder.rsplit('/').next().unwrap_or(folder).to_lowercase();
    name == "archive"
}

/// Whether two file names end in the same extension, case-insensitively.
fn same_extension(a: &str, b: &str) -> bool {
    let ext = |name: &str| {
        Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
    };
    ext(a) == ext(b)
}

/// Strip a sidecar's `.json` and any `.supplemental-metadata` suffix, including
/// the *truncated* forms Google produces.
///
/// Google caps the sidecar's file name at 51 characters, so a long photo name
/// leaves only a prefix of the suffix behind — `IMG_1234.jpg.supplemental-me`,
/// `…-metad`, or nothing at all. Any non-empty prefix of the suffix is stripped;
/// a bare `.json` sidecar loses only that.
fn strip_sidecar_suffix(sidecar: &str) -> String {
    let stem = sidecar
        .strip_suffix(".json")
        .or_else(|| sidecar.strip_suffix(".JSON"))
        .unwrap_or(sidecar);
    // Longest match first so `.supplemental-metadata` wins over `.s`.
    for length in (2..=SUPPLEMENTAL.len()).rev() {
        let candidate = &SUPPLEMENTAL[..length];
        if let Some(head) = stem.strip_suffix(candidate) {
            return head.to_string();
        }
    }
    stem.to_string()
}

/// Move a trailing `(N)` from after the extension to before it: Google writes
/// the sidecar of `IMG_1234(1).jpg` as `IMG_1234.jpg(1).json`.
fn unswap_duplicate_marker(stem: &str) -> Option<String> {
    let marker_start = stem.rfind('(')?;
    let marker = &stem[marker_start..];
    if !marker.ends_with(')')
        || !marker[1..marker.len() - 1]
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let head = &stem[..marker_start];
    let dot = head.rfind('.')?;
    Some(format!("{}{}{}", &head[..dot], marker, &head[dot..]))
}

/// The capture time a *file name* implies, in epoch seconds, or `None` when it
/// carries no plausible date.
///
/// The last resort when a photo has no sidecar. Camera and messenger apps stamp
/// the date into the name — `PXL_20260818_171030868.jpg`,
/// `IMG_20230219_171030.jpg`, `Screenshot_20230101-102950.png`,
/// `IMG-20230219-WA0001.jpg`, `Screenshot from 2023-01-01 10-29-50.png` — and a
/// date that is merely *approximately* right still files the photo in the right
/// year, which "the moment it was imported" does not.
///
/// The wall clock in a name has no zone attached, so it is read as UTC. That can
/// be a few hours out; it is never the years out that the import-time fallback
/// is. A sidecar always wins over this.
pub fn capture_time_from_name(name: &str) -> Option<i64> {
    let bytes = name.as_bytes();
    let digit = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    let num =
        |start: usize, len: usize| -> Option<i64> { name.get(start..start + len)?.parse().ok() };

    for start in 0..bytes.len() {
        if !digit(start) || (start > 0 && digit(start - 1)) {
            continue;
        }
        let run = |from: usize, len: usize| (0..len).all(|k| digit(from + k));
        // `YYYYMMDD` as one run of exactly eight digits, or `YYYY-MM-DD` with
        // any single separator repeated between the parts.
        let (year, month, day, after) = if run(start, 8) && !digit(start + 8) {
            (
                num(start, 4)?,
                num(start + 4, 2)?,
                num(start + 6, 2)?,
                start + 8,
            )
        } else if run(start, 4)
            && !digit(start + 4)
            && bytes.get(start + 4) == bytes.get(start + 7)
            && run(start + 5, 2)
            && run(start + 8, 2)
            && !digit(start + 10)
        {
            (
                num(start, 4)?,
                num(start + 5, 2)?,
                num(start + 8, 2)?,
                start + 10,
            )
        } else {
            continue;
        };
        if !(1990..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day)
        {
            continue;
        }

        // An optional time right behind the date, past one separator character.
        let after = match bytes.get(after) {
            Some(b'_' | b'-' | b' ' | b'.' | b',' | b'T' | b't' | b'@') => after + 1,
            _ => after,
        };
        let time = if run(after, 6) {
            // Six digits, possibly with the milliseconds tail Pixel appends.
            Some((num(after, 2)?, num(after + 2, 2)?, num(after + 4, 2)?))
        } else if run(after, 2)
            && !digit(after + 2)
            && bytes.get(after + 2) == bytes.get(after + 5)
            && run(after + 3, 2)
            && run(after + 6, 2)
        {
            Some((num(after, 2)?, num(after + 3, 2)?, num(after + 6, 2)?))
        } else {
            None
        };
        let (hour, minute, second) = match time {
            Some((h, m, sec)) if h < 24 && m < 60 && sec < 60 => (h, m, sec),
            _ => (0, 0, 0),
        };

        return Some(
            days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second,
        );
    }
    None
}

/// Days between 1970-01-01 and a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`). Avoids pulling a date crate in for one conversion.
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`unswap_duplicate_marker`]: `IMG_1234(1).jpg` as Google
/// writes its sidecar, `IMG_1234.jpg(1)`. `None` when the name carries no
/// duplicate marker.
fn swap_duplicate_marker(file_name: &str) -> Option<String> {
    let marker_start = file_name.rfind('(')?;
    let marker_end = marker_start + file_name[marker_start..].find(')')? + 1;
    let marker = &file_name[marker_start..marker_end];
    if !marker[1..marker.len() - 1]
        .chars()
        .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let tail = &file_name[marker_end..];
    if !tail.starts_with('.') {
        return None;
    }
    Some(format!("{}{}{}", &file_name[..marker_start], tail, marker))
}

/// How long Google lets a sidecar's file name get, `.json` included. A longer
/// one is cut, into its suffix first and then into the photo name.
const SIDECAR_NAME_CAP: usize = 51;

/// The rank [`sidecar_rank`] gives an edited copy that shares its original's
/// sidecar. Its date is the original's, but its name is its own.
const EDITED_COPY: u8 = 4;

/// The sidecars of one folder by their `.json`-stripped stem, so a photo is
/// ranked only against those that can match it.
///
/// Every rule in [`sidecar_rank`] needs the stem to start with a prefix of the
/// photo name that the name itself determines, and a sorted map finds the
/// stems with a given prefix as one range. Ranking every photo against every
/// sidecar instead took 27 s for a folder of 10,000 photos.
struct SidecarIndex<'a> {
    by_stem: BTreeMap<&'a str, Vec<(&'a str, &'a Vec<u8>)>>,
}

impl<'a> SidecarIndex<'a> {
    fn new(in_folder: &'a BTreeMap<String, &'a Vec<u8>>) -> Self {
        let mut by_stem: BTreeMap<&'a str, Vec<(&'a str, &'a Vec<u8>)>> = BTreeMap::new();
        for (name, body) in in_folder {
            by_stem
                .entry(strip_json_suffix(name))
                .or_default()
                .push((name.as_str(), *body));
        }
        Self { by_stem }
    }

    /// Add every sidecar whose stem starts with `prefix` to `out`.
    fn starting_with(&self, prefix: &str, out: &mut BTreeMap<&'a str, &'a Vec<u8>>) {
        let from = (
            std::ops::Bound::Included(prefix),
            std::ops::Bound::Unbounded,
        );
        for (stem, sidecars) in self.by_stem.range::<str, _>(from) {
            if !stem.starts_with(prefix) {
                break;
            }
            out.extend(sidecars.iter().copied());
        }
    }

    /// Add every sidecar whose stem is `head`, or starts with `head` and a `.`:
    /// those that have `head` as a boundary ([`suffix_boundaries`]).
    fn bounded_by(&self, head: &str, out: &mut BTreeMap<&'a str, &'a Vec<u8>>) {
        if let Some(sidecars) = self.by_stem.get(head) {
            out.extend(sidecars.iter().copied());
        }
        self.starting_with(&format!("{head}."), out);
    }

    /// Every sidecar [`sidecar_rank`] could match `file_name` to, by name. A
    /// superset: each one is still ranked.
    fn candidates(&self, file_name: &str) -> BTreeMap<&'a str, &'a Vec<u8>> {
        let mut out = BTreeMap::new();
        // Rules 0 and 2: the stem starts with the name, or with its swapped form.
        self.starting_with(file_name, &mut out);
        if let Some(swapped) = swap_duplicate_marker(file_name) {
            self.starting_with(&swapped, &mut out);
        }
        // Rule 1: the stem of `A(N).B` starts with `A.B(N)`, so with `A.`.
        for (paren, _) in file_name.match_indices('(') {
            self.starting_with(&format!("{}.", &file_name[..paren]), &mut out);
        }
        // Rule 3: a boundary of the stem is a prefix of the name.
        for end in file_name
            .char_indices()
            .map(|(index, _)| index)
            .skip(1)
            .chain([file_name.len()])
        {
            self.bounded_by(&file_name[..end], &mut out);
        }
        // Rule 4: `<original>-<word>.<ext>` has `<original>.<ext>` as a boundary.
        if let Some((_, extension)) = file_name.rsplit_once('.') {
            for (dash, _) in file_name.match_indices('-') {
                self.bounded_by(&format!("{}.{extension}", &file_name[..dash]), &mut out);
            }
        }
        out
    }
}

/// Find the sidecar belonging to `file_name` among the JSON files of its folder,
/// with the rank it matched at.
///
/// Tried in order of confidence: the exact name, the `(N)`-swapped name, an
/// unknown trailing suffix, a prefix match for the case where truncation cut
/// into the photo name itself, and last an edited copy's original. The
/// best-ranked candidate in the folder wins; of equal ranks, the first by
/// sidecar name.
fn find_sidecar<'a>(
    file_name: &str,
    index: &SidecarIndex<'a>,
) -> Option<((u8, usize), &'a Vec<u8>)> {
    let mut best: Option<((u8, usize), &'a Vec<u8>)> = None;
    for (sidecar_name, body) in index.candidates(file_name) {
        let Some(rank) = sidecar_rank(strip_json_suffix(sidecar_name), file_name) else {
            continue;
        };
        if best.as_ref().is_none_or(|(current, _)| rank < *current) {
            best = Some((rank, body));
        }
    }
    best
}

/// How well a sidecar's `.json`-stripped `stem` matches `file_name`, as a sort
/// key where **lower is better**: the leading number is the rule that matched,
/// the second breaks ties within it.
fn sidecar_rank(stem: &str, file_name: &str) -> Option<(u8, usize)> {
    // 0 — the stem is the photo name once the (English) suffix is off.
    let plain = strip_sidecar_suffix(stem);
    if plain == file_name {
        return Some((0, 0));
    }
    // 1 — the same, for `IMG_1234.jpg(1).json` against `IMG_1234(1).jpg`.
    if unswap_duplicate_marker(&plain).as_deref() == Some(file_name) {
        return Some((1, 0));
    }
    // 2 — the photo name is intact but a suffix we do not recognise follows it.
    // Google *localizes* `.supplemental-metadata` (a German export writes
    // `.ergänzende-Metadaten`) and has shipped misspelt variants of it, so the
    // suffix cannot be enumerated — matching on the intact name in front of it
    // is what stays locale-agnostic. The shortest leftover wins, so a folder
    // holding both `IMG_1.jpg.<suffix>` and `IMG_1.jpg(1).<suffix>` gives each
    // photo its own.
    for candidate in [
        Some(file_name.to_string()),
        swap_duplicate_marker(file_name),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(rest) = stem.strip_prefix(&candidate)
            && looks_like_sidecar_suffix(rest)
        {
            return Some((2, rest.len()));
        }
    }
    // 3 — truncation ate into the photo name itself, leaving only a prefix of
    // it. Every suffix boundary in the stem is tried, so this holds for a
    // localized suffix too; the longest surviving prefix wins.
    //
    // Only for a sidecar whose name is as long as Google lets one get: a name
    // that short of the cap was not cut, so a prefix of it is the name of a
    // different photo. Without this, `IMG_1.jpg`'s sidecar gave its name and
    // date to an `IMG_12.jpg` that had none (`docs/BUGS.md` B225).
    if stem.chars().count() + ".json".len() >= SIDECAR_NAME_CAP - 4 {
        let mut longest: Option<usize> = None;
        for head in suffix_boundaries(stem) {
            if !head.is_empty()
                && file_name.starts_with(head)
                && longest.is_none_or(|len| head.len() > len)
            {
                longest = Some(head.len());
            }
        }
        if let Some(len) = longest {
            return Some((3, usize::MAX - len));
        }
    }
    // 4 — an edited copy, `IMG_1-edited.jpg` (the word is localized), which
    // Google exports without a sidecar of its own next to `IMG_1.jpg`'s.
    suffix_boundaries(stem)
        .filter_map(|head| head.rsplit_once('.'))
        .any(|(original, extension)| is_edited_copy_of(file_name, original, extension))
        .then_some((EDITED_COPY, 0))
}

/// Whether `file_name` is `<original>-<word>.<extension>`, the name Google
/// gives an edited copy of `<original>.<extension>`.
fn is_edited_copy_of(file_name: &str, original: &str, extension: &str) -> bool {
    !original.is_empty()
        && file_name
            .strip_prefix(original)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|rest| rest.strip_suffix(extension))
            .and_then(|rest| rest.strip_suffix('.'))
            .is_some_and(|word| !word.is_empty() && word.chars().all(char::is_alphabetic))
}

/// The stem plus each of its prefixes ending before a `.`, longest first — the
/// possible boundaries between a photo name and a sidecar suffix.
fn suffix_boundaries(stem: &str) -> impl Iterator<Item = &str> {
    std::iter::once(stem).chain(
        stem.char_indices()
            .filter(|(_, c)| *c == '.')
            .map(|(index, _)| &stem[..index])
            .collect::<Vec<_>>()
            .into_iter()
            .rev(),
    )
}

/// Whether `rest` — what follows an intact photo name in a sidecar's stem —
/// looks like a metadata suffix rather than the tail of a different file's name.
/// Google's suffixes all start at a `.`, or at the `(N)` duplicate marker.
fn looks_like_sidecar_suffix(rest: &str) -> bool {
    rest.starts_with('.') || rest.starts_with('(')
}

/// Drop a trailing `.json`, case-insensitively.
fn strip_json_suffix(sidecar: &str) -> &str {
    match sidecar.len().checked_sub(5) {
        Some(cut) if sidecar[cut..].eq_ignore_ascii_case(".json") => &sidecar[..cut],
        _ => sidecar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(archive: usize, path: &str, size: u64) -> RawEntry {
        RawEntry {
            archive,
            entry: path.to_string(),
            relative: strip_export_prefix(path).unwrap().1.to_string(),
            size,
        }
    }

    fn sidecar(map: &mut HashMap<String, Vec<u8>>, path: &str, body: &str) {
        let relative = strip_export_prefix(path).unwrap().1.to_string();
        map.insert(relative, body.as_bytes().to_vec());
    }

    #[test]
    fn strips_export_root_and_localized_service_folder() {
        assert_eq!(
            strip_export_prefix("Takeout/Google Fotos/Island 2019/IMG_1.jpg"),
            Some(("Google Fotos", "Island 2019/IMG_1.jpg"))
        );
        // Already unwrapped by the user: no `Takeout/` root left.
        assert_eq!(
            strip_export_prefix("Google Photos/IMG_1.jpg"),
            Some(("Google Photos", "IMG_1.jpg"))
        );
        // ... and an album keeps its folder.
        assert_eq!(
            strip_export_prefix("Google Photos/Iceland/IMG_1.jpg"),
            Some(("Google Photos", "Iceland/IMG_1.jpg"))
        );
        // Nothing below a service folder is nothing to import.
        assert_eq!(strip_export_prefix("archive_browser.html"), None);
    }

    #[test]
    fn album_folder_becomes_an_album_year_folder_does_not() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Iceland 2019/metadata.json",
            r#"{"title":"Iceland 2019"}"#,
        );
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Iceland 2019/IMG_1.jpg", 10),
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_2.jpg", 20),
            ],
            &sidecars,
        );

        assert_eq!(scan.albums, vec!["Iceland 2019"]);
        assert_eq!(scan.photos[0].bucket, Bucket::Album("Iceland 2019".into()));
        assert_eq!(scan.photos[1].bucket, Bucket::Timeline);
        assert_eq!(scan.total_bytes(), 30);
    }

    #[test]
    fn trashed_photos_are_skipped_and_counted() {
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Papierkorb/IMG_1.jpg", 10),
                entry(0, "Takeout/Google Photos/Bin/IMG_2.jpg", 10),
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_3.jpg", 10),
            ],
            &HashMap::new(),
        );

        assert_eq!(scan.skipped_trashed, 2);
        assert_eq!(scan.photos.len(), 1);
        assert_eq!(scan.photos[0].name, "IMG_3.jpg");
    }

    #[test]
    fn non_media_entries_are_ignored() {
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_1.jpg", 10),
                entry(0, "Takeout/Google Photos/Photos from 2019/notes.txt", 10),
            ],
            &HashMap::new(),
        );

        assert_eq!(scan.photos.len(), 1);
        assert_eq!(scan.skipped_other, 1);
    }

    #[test]
    fn sidecar_supplies_capture_time_and_favorite() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.jpg.supplemental-metadata.json",
            r#"{"title":"IMG_1.jpg","photoTakenTime":{"timestamp":"1560000000"},"favorited":true}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/IMG_1.jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(1_560_000_000));
        assert!(scan.photos[0].favorite);
    }

    #[test]
    fn creation_time_is_kept_apart_as_the_last_resort() {
        // `creationTime` is when the photo reached Google. It must not beat the
        // date in the name, and the importer tries EXIF before it.
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.jpg.json",
            r#"{"creationTime":{"timestamp":"1500000000"}}"#,
        );
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_20190612_101500.jpg.json",
            r#"{"creationTime":{"timestamp":"1700000000"}}"#,
        );
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_1.jpg", 10),
                entry(
                    0,
                    "Takeout/Google Photos/Photos from 2019/IMG_20190612_101500.jpg",
                    10,
                ),
            ],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, None);
        assert_eq!(scan.photos[0].creation_time, Some(1_500_000_000));
        assert_eq!(
            scan.photos[1].capture_time,
            capture_time_from_name("IMG_20190612_101500.jpg")
        );
        assert_eq!(scan.photos[1].creation_time, Some(1_700_000_000));
    }

    #[test]
    fn sidecar_timestamps_tolerate_numbers_zero_and_null() {
        let mut sidecars = HashMap::new();
        for (name, body) in [
            ("A.jpg", r#"{"photoTakenTime":{"timestamp":1560000000}}"#),
            (
                "B.jpg",
                r#"{"photoTakenTime":{"timestamp":"0"},"favorited":null}"#,
            ),
            (
                "C.jpg",
                r#"{"photoTakenTime":{"timestamp":null},"geoData":null}"#,
            ),
        ] {
            sidecar(
                &mut sidecars,
                &format!("Takeout/Google Photos/Photos from 2019/{name}.json"),
                body,
            );
        }
        let scan = assemble(
            ["A.jpg", "B.jpg", "C.jpg"]
                .iter()
                .map(|name| {
                    entry(
                        0,
                        &format!("Takeout/Google Photos/Photos from 2019/{name}"),
                        1,
                    )
                })
                .collect(),
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(1_560_000_000));
        assert_eq!(scan.photos[1].capture_time, None, "0 is not 1970");
        assert!(!scan.photos[1].favorite);
        assert_eq!(scan.photos[2].capture_time, None);
    }

    #[test]
    fn location_comes_from_geo_data_and_zero_means_none() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Iceland/IMG_1.jpg.json",
            r#"{"geoData":{"latitude":64.1466,"longitude":-21.9426,"altitude":0.0}}"#,
        );
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Iceland/IMG_2.jpg.json",
            r#"{"geoData":{"latitude":0.0,"longitude":0.0},
                "geoDataExif":{"latitude":63.4186,"longitude":-19.0060}}"#,
        );
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Iceland/IMG_3.jpg.json",
            r#"{"geoData":{"latitude":0.0,"longitude":0.0}}"#,
        );
        let scan = assemble(
            (1..=3)
                .map(|n| entry(0, &format!("Takeout/Google Photos/Iceland/IMG_{n}.jpg"), 1))
                .collect(),
            &sidecars,
        );

        assert_eq!(
            scan.photos[0].location,
            Some(GeoPoint {
                latitude: 64.1466,
                longitude: -21.9426
            })
        );
        assert_eq!(
            scan.photos[1].location.map(|p| p.latitude),
            Some(63.4186),
            "geoDataExif fills in for an empty geoData"
        );
        assert_eq!(scan.photos[2].location, None);
    }

    #[test]
    fn live_and_motion_photo_halves_share_a_key() {
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Trip/IMG_1.HEIC", 1),
                entry(0, "Takeout/Google Photos/Trip/IMG_1.MOV", 1),
                entry(0, "Takeout/Google Photos/Trip/PXL_2.MP.jpg", 1),
                entry(0, "Takeout/Google Photos/Trip/PXL_2.MP", 1),
                entry(0, "Takeout/Google Photos/Other/IMG_1.MOV", 1),
                entry(0, "Takeout/Google Photos/Trip/scan.png", 1),
            ],
            &HashMap::new(),
        );
        let key = |i: usize| scan.photos[i].live_key.clone();

        assert_eq!(key(0), key(1));
        assert!(!scan.photos[0].is_motion_photo());
        assert_eq!(key(2), key(3));
        assert!(scan.photos[3].is_motion_photo());
        assert!(scan.photos[3].is_video(), "the .MP half is a video");
        assert_ne!(key(1), key(4), "another folder's IMG_1 does not pair");
        assert_eq!(key(5), None);
    }

    #[test]
    fn a_localized_supplemental_suffix_still_matches() {
        // A German export writes `.ergänzende-Metadaten` where an English one
        // writes `.supplemental-metadata`; the photo name in front of it is
        // what the match hangs on.
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Fotos/Fotos von 2023/IMG-20230219-WA0001.jpg.ergänzende-Metadaten.json",
            r#"{"photoTakenTime":{"timestamp":"1676800000"}}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Fotos/Fotos von 2023/IMG-20230219-WA0001.jpg",
                10,
            )],
            &sidecars,
        );
        assert_eq!(scan.photos[0].capture_time, Some(1_676_800_000));
    }

    #[test]
    fn a_localized_suffix_does_not_let_one_sidecar_claim_a_sibling() {
        let mut sidecars = HashMap::new();
        for name in ["IMG_1.jpg", "IMG_1(1).jpg"] {
            sidecar(
                &mut sidecars,
                &format!("Takeout/Google Fotos/Album/{name}.ergänzende-Metadaten.json"),
                r#"{"photoTakenTime":{"timestamp":"1"}}"#,
            );
        }
        // Google writes the duplicate's sidecar with the marker after the
        // extension, so this is the spelling that actually ships.
        sidecars.remove("Album/IMG_1(1).jpg.ergänzende-Metadaten.json");
        sidecar(
            &mut sidecars,
            "Takeout/Google Fotos/Album/IMG_1.jpg(1).ergänzende-Metadaten.json",
            r#"{"photoTakenTime":{"timestamp":"2"}}"#,
        );
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Fotos/Album/IMG_1.jpg", 10),
                entry(0, "Takeout/Google Fotos/Album/IMG_1(1).jpg", 10),
            ],
            &sidecars,
        );
        assert_eq!(scan.photos[0].capture_time, Some(1));
        assert_eq!(scan.photos[1].capture_time, Some(2));
    }

    #[test]
    fn the_file_name_date_is_the_fallback_when_no_sidecar_matches() {
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Fotos/Fotos von 2023/IMG-20230219-WA0001.jpg",
                10,
            )],
            &HashMap::new(),
        );
        // 2023-02-19 00:00:00 UTC.
        assert_eq!(scan.photos[0].capture_time, Some(1_676_764_800));
    }

    #[test]
    fn file_name_dates_cover_the_common_camera_and_messenger_spellings() {
        // 2026-08-18 17:10:30 UTC, with Pixel's milliseconds tail.
        assert_eq!(
            capture_time_from_name("PXL_20260818_171030868.RAW-01.COVER.jpg"),
            Some(1_787_073_030)
        );
        assert_eq!(
            capture_time_from_name("IMG_20260818_171030.jpg"),
            Some(1_787_073_030)
        );
        assert_eq!(
            capture_time_from_name("Screenshot_20260818-171030.png"),
            Some(1_787_073_030)
        );
        assert_eq!(
            capture_time_from_name("Screenshot from 2026-08-18 17-10-30.png"),
            Some(1_787_073_030)
        );
        // A date with no usable time lands at midnight, not at import time.
        assert_eq!(
            capture_time_from_name("IMG-20260818-WA0001.jpg"),
            Some(1_787_011_200)
        );
    }

    #[test]
    fn a_name_without_a_plausible_date_yields_nothing() {
        assert_eq!(capture_time_from_name("100_1234.JPG"), None);
        assert_eq!(capture_time_from_name("DSC00042.jpg"), None);
        // A nine-digit run is not a date; neither is an out-of-range month.
        assert_eq!(capture_time_from_name("IMG_202608181.jpg"), None);
        assert_eq!(capture_time_from_name("IMG_20261818.jpg"), None);
    }

    #[test]
    fn truncated_supplemental_suffixes_still_match() {
        // Google caps the sidecar name at 51 chars, cutting into the suffix.
        assert_eq!(
            strip_sidecar_suffix("IMG_1.jpg.supplemental-me.json"),
            "IMG_1.jpg"
        );
        assert_eq!(
            strip_sidecar_suffix("IMG_1.jpg.supplemental-metadata.json"),
            "IMG_1.jpg"
        );
        assert_eq!(strip_sidecar_suffix("IMG_1.jpg.json"), "IMG_1.jpg");
    }

    #[test]
    fn duplicate_marker_after_the_extension_is_moved_back() {
        assert_eq!(
            unswap_duplicate_marker("IMG_1.jpg(1)").as_deref(),
            Some("IMG_1(1).jpg")
        );
        assert_eq!(unswap_duplicate_marker("IMG_1.jpg"), None);

        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.jpg(1).json",
            r#"{"photoTakenTime":{"timestamp":"1600000000"}}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/IMG_1(1).jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(1_600_000_000));
    }

    #[test]
    fn longest_prefix_wins_when_the_photo_name_itself_was_truncated() {
        // Both sidecar names are at Google's cap, so both were cut into the
        // photo name; the longer surviving prefix is the better claim.
        let short = "Takeout/Google Photos/Photos from 2019/PXL_20190101_120000000.PORTRAIT_ORIGINAL_E.json";
        let long = "Takeout/Google Photos/Photos from 2019/PXL_20190101_120000000.PORTRAIT_ORIGINAL_EXPO.json";
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            short,
            r#"{"photoTakenTime":{"timestamp":"1"}}"#,
        );
        sidecar(
            &mut sidecars,
            long,
            r#"{"photoTakenTime":{"timestamp":"2"}}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/PXL_20190101_120000000.PORTRAIT_ORIGINAL_EXPOSURE.jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(2));
    }

    /// A short sidecar name was not cut, so a prefix of it names another photo:
    /// `IMG_1.jpg`'s sidecar gave its title and date to `IMG_12.jpg`
    /// (`docs/BUGS.md` B225).
    #[test]
    fn a_photo_without_a_sidecar_does_not_take_a_shorter_named_siblings() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.jpg.supplemental-metadata.json",
            r#"{"title":"IMG_1.jpg","photoTakenTime":{"timestamp":"1"}}"#,
        );
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_1.jpg", 10),
                entry(0, "Takeout/Google Photos/Photos from 2019/IMG_12.jpg", 10),
            ],
            &sidecars,
        );

        assert_eq!(scan.photos[0].capture_time, Some(1));
        assert_eq!(scan.photos[1].name, "IMG_12.jpg");
        assert_eq!(scan.photos[1].capture_time, None);
    }

    /// An edited copy has no sidecar of its own. It takes its original's date,
    /// but keeps its own name rather than the original's title.
    #[test]
    fn an_edited_copy_takes_its_originals_date_and_keeps_its_name() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Fotos/Urlaub/IMG_2.jpg.ergänzende-Metadaten.json",
            r#"{"title":"Holiday.jpg","photoTakenTime":{"timestamp":"7"}}"#,
        );
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Fotos/Urlaub/IMG_2.jpg", 10),
                entry(0, "Takeout/Google Fotos/Urlaub/IMG_2-bearbeitet.jpg", 10),
            ],
            &sidecars,
        );

        assert_eq!(scan.photos[0].name, "Holiday.jpg");
        assert_eq!(scan.photos[1].name, "IMG_2-bearbeitet.jpg");
        assert_eq!(scan.photos[1].capture_time, Some(7));
    }

    /// The index looks only at the sidecars that can match, and must find the
    /// same one that ranking every sidecar in the folder finds.
    #[test]
    fn the_sidecar_index_finds_what_ranking_every_sidecar_finds() {
        let long = "A_very_long_holiday_photo_name_from_the_beach_2019";
        let names = [
            "IMG_1.jpg".to_string(),
            "IMG_12.jpg".to_string(),
            "IMG_1(1).jpg".to_string(),
            "IMG_1-edited.jpg".to_string(),
            "IMG_1-bearbeitet.jpg".to_string(),
            "my.photo(2).jpg".to_string(),
            "x(1).jpg".to_string(),
            "Ölbild.png".to_string(),
            "PXL_20230101_101010.mp4".to_string(),
            format!("{long}.jpg"),
            format!("{long}_2.jpg"),
            "nothing.heic".to_string(),
        ];
        let sidecar_names = [
            "IMG_1.jpg.supplemental-metadata.json".to_string(),
            "IMG_1.jpg(1).supplemental-metadata.json".to_string(),
            "IMG_12.jpg.ergänzende-Metadaten.json".to_string(),
            "my.photo.jpg(2).json".to_string(),
            "x.jpg(1).json".to_string(),
            "Ölbild.png.suppl.json".to_string(),
            "PXL_20230101_101010.mp4.JSON".to_string(),
            format!("{}.json", &long[..44]),
            format!("{long}.jp.json"),
            "IMG.json".to_string(),
            "unrelated.json".to_string(),
        ];
        let bodies: Vec<Vec<u8>> = (0..sidecar_names.len())
            .map(|i| i.to_string().into_bytes())
            .collect();
        let in_folder: BTreeMap<String, &Vec<u8>> =
            sidecar_names.iter().cloned().zip(bodies.iter()).collect();
        let index = SidecarIndex::new(&in_folder);
        for name in &names {
            let mut every: Option<((u8, usize), &Vec<u8>)> = None;
            for (sidecar_name, body) in &in_folder {
                if let Some(rank) = sidecar_rank(strip_json_suffix(sidecar_name), name)
                    && every.as_ref().is_none_or(|(current, _)| rank < *current)
                {
                    every = Some((rank, *body));
                }
            }
            assert_eq!(find_sidecar(name, &index), every, "{name}");
        }
    }

    #[test]
    fn sidecar_title_replaces_a_mangled_on_disk_name() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/a_very_long_name_that_google_.jpg.json",
            r#"{"title":"a_very_long_name_that_google_truncated.jpg"}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/a_very_long_name_that_google_.jpg",
                10,
            )],
            &sidecars,
        );

        assert_eq!(
            scan.photos[0].name,
            "a_very_long_name_that_google_truncated.jpg"
        );
    }

    #[test]
    fn a_title_with_a_different_extension_is_not_trusted() {
        // Motion photos name their `.MP` part after the `.jpg`; taking the title
        // verbatim would upload the video under an image name.
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Photos from 2019/IMG_1.mp4.json",
            r#"{"title":"IMG_1.jpg"}"#,
        );
        let scan = assemble(
            vec![entry(
                0,
                "Takeout/Google Photos/Photos from 2019/IMG_1.mp4",
                10,
            )],
            &sidecars,
        );

        assert_eq!(scan.photos[0].name, "IMG_1.mp4");
        assert_eq!(scan.photos[0].media_type, "video/mp4");
    }

    #[test]
    fn an_album_split_across_archives_stays_one_album() {
        let mut sidecars = HashMap::new();
        sidecar(
            &mut sidecars,
            "Takeout/Google Photos/Iceland 2019/metadata.json",
            r#"{"title":"Iceland 2019"}"#,
        );
        let scan = assemble(
            vec![
                entry(0, "Takeout/Google Photos/Iceland 2019/IMG_1.jpg", 10),
                entry(1, "Takeout/Google Photos/Iceland 2019/IMG_2.jpg", 10),
            ],
            &sidecars,
        );

        assert_eq!(scan.albums, vec!["Iceland 2019"]);
        assert_eq!(scan.photos.len(), 2);
        assert_eq!(scan.photos[1].archive, 1);
    }

    #[test]
    fn archive_bucket_imports_to_the_timeline() {
        let scan = assemble(
            vec![entry(0, "Takeout/Google Photos/Archive/IMG_1.jpg", 10)],
            &HashMap::new(),
        );

        assert_eq!(scan.photos[0].bucket, Bucket::Archived);
        assert!(scan.albums.is_empty());
    }

    /// Write a zip holding `files` and return its path. No `tempfile`
    /// dev-dependency: the daemon crates deliberately carry none, and one
    /// uniquely named file under the temp dir is all this needs.
    fn write_zip(tag: &str, files: &[(&str, &[u8])]) -> PathBuf {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "pdfs-takeout-test-{tag}-{}.zip",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn scans_a_two_part_export_where_the_sidecar_is_in_the_other_part() {
        // The case that makes the whole set one scan: part 1 has the photo,
        // part 2 has its metadata and the album title.
        let first = write_zip(
            "part1",
            &[(
                "Takeout/Google Photos/Iceland 2019/IMG_1.jpg",
                b"the photo bytes",
            )],
        );
        let second = write_zip(
            "part2",
            &[
                (
                    "Takeout/Google Photos/Iceland 2019/metadata.json",
                    br#"{"title":"Iceland 2019"}"#.as_slice(),
                ),
                (
                    "Takeout/Google Photos/Iceland 2019/IMG_1.jpg.supplemental-metadata.json",
                    br#"{"title":"IMG_1.jpg","photoTakenTime":{"timestamp":"1560000000"}}"#
                        .as_slice(),
                ),
                (
                    "Takeout/Google Photos/Trash/IMG_9.jpg",
                    b"deleted".as_slice(),
                ),
            ],
        );

        let scan = scan_archives(&[first.clone(), second.clone()]).unwrap();
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);

        assert_eq!(scan.photos.len(), 1);
        assert_eq!(scan.photos[0].name, "IMG_1.jpg");
        assert_eq!(scan.photos[0].archive, 0, "the photo is in part 1");
        assert_eq!(scan.photos[0].size, "the photo bytes".len() as u64);
        assert_eq!(scan.photos[0].capture_time, Some(1_560_000_000));
        assert_eq!(scan.photos[0].bucket, Bucket::Album("Iceland 2019".into()));
        assert_eq!(scan.albums, vec!["Iceland 2019"]);
        assert_eq!(scan.skipped_trashed, 1);
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_left_out_and_the_rest_imports() {
        let broken =
            std::env::temp_dir().join(format!("pdfs-takeout-not-a-zip-{}", std::process::id()));
        std::fs::write(&broken, b"not a zip at all").unwrap();
        let good = write_zip(
            "beside-broken",
            &[("Takeout/Google Photos/Photos from 2019/IMG_1.jpg", b"x")],
        );

        let scan = scan_archives(&[broken.clone(), good.clone()]).unwrap();
        let _ = std::fs::remove_file(&broken);
        let _ = std::fs::remove_file(&good);

        assert_eq!(scan.photos.len(), 1);
        assert_eq!(scan.photos[0].archive, 1);
        assert_eq!(scan.unreadable_archives.len(), 1);
        assert_eq!(scan.unreadable_archives[0].path, broken);
        assert!(
            scan.unreadable_archives[0]
                .reason
                .contains("not a readable zip"),
            "unexpected reason: {}",
            scan.unreadable_archives[0].reason
        );
    }

    #[test]
    fn other_services_in_the_same_export_are_not_imported() {
        let zip = write_zip(
            "services",
            &[
                (
                    "Takeout/Google Fotos/Fotos von 2019/IMG_1.jpg",
                    b"x".as_slice(),
                ),
                (
                    "Takeout/Google Fotos/Fotos von 2019/IMG_1.jpg.json",
                    br#"{"photoTakenTime":{"timestamp":"1560000000"}}"#.as_slice(),
                ),
                ("Takeout/Drive/My Drive/holiday.jpg", b"y".as_slice()),
                ("Takeout/YouTube/videos/clip.mp4", b"z".as_slice()),
            ],
        );

        let scan = scan_archives(std::slice::from_ref(&zip)).unwrap();
        let _ = std::fs::remove_file(&zip);

        assert_eq!(scan.photos.len(), 1);
        assert_eq!(scan.photos[0].name, "IMG_1.jpg");
        assert_eq!(scan.skipped_other, 2);
    }

    #[test]
    fn an_export_without_sidecars_keeps_every_service() {
        let zip = write_zip(
            "no-sidecars",
            &[("Takeout/Google Photos/Photos from 2019/IMG_1.jpg", b"x")],
        );
        let scan = scan_archives(std::slice::from_ref(&zip)).unwrap();
        let _ = std::fs::remove_file(&zip);
        assert_eq!(scan.photos.len(), 1);
    }

    #[test]
    fn a_stop_request_ends_the_scan() {
        let zip = write_zip(
            "stop",
            &[("Takeout/Google Photos/Photos from 2019/IMG_1.jpg", b"x")],
        );
        let result = scan_archives_until(std::slice::from_ref(&zip), || true);
        let _ = std::fs::remove_file(&zip);
        assert!(result.is_err());
    }
}
