//! Importing a Google Photos Takeout export into Proton Photos.
//!
//! [`pdfs_core::takeout`] says what is in the archives; this is the part that
//! talks to Proton. One import runs on its own thread and moves through four
//! phases ([`ImportPhase`]), which [`Request::ImportStatus`] reports together
//! with the running counts:
//!
//! 1. **Scan** — read the archives' central directories and sidecars. An
//!    archive that cannot be read is left out and reported; the rest imports.
//! 2. **Check** — ask the photos volume which of the names already exist
//!    ([`ProtonPhotosClient::name_collisions`]), hash only those files, then
//!    confirm with the content digest. A library that is already half-synced
//!    by Proton Photos on a phone therefore costs one cheap request per 150
//!    names and reads no media at all for the misses.
//! 3. **Upload** the survivors, streaming each entry out of its zip. A photo
//!    that can have a thumbnail is copied to a staging file first, one at a
//!    time. The video half of a live or motion photo goes after its still and
//!    is filed with it.
//! 4. **Albums** — create the albums the export names that the account lacks,
//!    then add every photo of each album, the ones that were already on the
//!    account included.
//!
//! Resuming is re-running: phase 2 finds everything an earlier run uploaded
//! and skips it. The archives and the outcome of the last run are kept in the
//! state directory, so an import cut short by a reboot can be picked up again
//! with [`Request::ResumeImport`] without choosing the archives again.
//!
//! [`Request::ImportStatus`]: pdfs_core::control::Request::ImportStatus
//! [`Request::ResumeImport`]: pdfs_core::control::Request::ResumeImport
//! [`ProtonPhotosClient::name_collisions`]: proton_drive_rs::ProtonPhotosClient::name_collisions

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use pdfs_core::control::{
    ActivityKind, ImportPhase, ImportProgress, ImportSummary, TransferDirection,
};
use pdfs_core::takeout::{Bucket, TakeoutPhoto, TakeoutScan, scan_archives_until};
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{PhotoLocation, PhotoTag, PhotoTagsUpdate, PhotoUploadMetadata};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub(crate) use pdfs_drive::digest::sha1_of;

use super::Core;
use crate::photos::can_make_thumbnail;
use crate::transfers::CountingReader;

/// The one import this daemon may be running, and how the last one ended.
///
/// Process-global rather than a [`Core`] field because it must be *one* import
/// per daemon however many `Core` clones exist, and because a front-end asking
/// for the result after the import finished must still find it. Loaded from
/// the state directory on first use, so the last run survives a restart.
static IMPORT: std::sync::LazyLock<parking_lot::Mutex<ImportState>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(ImportState::load(saved_path())));

/// Photos uploaded between two timeline refreshes, so a long import shows up
/// in the gallery as it goes rather than only at the end.
const TIMELINE_REFRESH_EVERY: usize = 250;

/// Photos uploaded between two saves of the running counts, so a run cut short
/// by a power loss still reports roughly how far it got.
const CHECKPOINT_EVERY: usize = 25;

/// How far apart, in seconds, the still and the video of one live photo may
/// claim to have been taken before they are treated as two unrelated files
/// that happen to share a name.
const LIVE_PAIR_MAX_SKEW: i64 = 120;

#[derive(Default)]
struct ImportState {
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    archives: Vec<PathBuf>,
    dry_run: bool,
    /// Live counts while running, the final report after.
    summary: Option<ImportSummary>,
    progress: Option<ImportProgress>,
    /// Where the state is saved; `None` keeps it in memory only (tests).
    path: Option<PathBuf>,
}

/// What [`ImportState`] keeps on disk.
#[derive(Serialize, Deserialize, Default)]
struct SavedImport {
    archives: Vec<PathBuf>,
    dry_run: bool,
    running: bool,
    summary: Option<ImportSummary>,
}

fn saved_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    pdfs_core::config::AppDirs::new()
        .ok()
        .map(|dirs| dirs.takeout_import_path())
}

impl ImportState {
    /// The state as the last daemon left it. A run that was still marked as
    /// running did not get to finish: the daemon stopped under it.
    fn load(path: Option<PathBuf>) -> Self {
        let saved = path
            .as_deref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| {
                serde_json::from_slice::<SavedImport>(&bytes)
                    .inspect_err(|error| warn!(%error, "ignoring an unreadable import state"))
                    .ok()
            })
            .unwrap_or_default();
        let mut summary = saved.summary;
        if saved.running {
            summary.get_or_insert_with(Default::default).interrupted = true;
        }
        Self {
            archives: saved.archives,
            dry_run: saved.dry_run,
            summary,
            path,
            ..Default::default()
        }
    }

    /// Write the state out. Atomic, so a crash mid-write keeps the old one.
    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let saved = SavedImport {
            archives: self.archives.clone(),
            dry_run: self.dry_run,
            running: self.running,
            summary: self.summary.clone(),
        };
        let written = serde_json::to_vec_pretty(&saved)
            .map_err(std::io::Error::other)
            .and_then(|bytes| write_atomically(path, &bytes));
        if let Err(error) = written {
            warn!(path = %path.display(), %error, "cannot save the import state");
        }
    }

    /// The archives of the last run when it can be resumed: a real import that
    /// did not finish everything it set out to do.
    fn resumable(&self) -> Vec<String> {
        let unfinished = self
            .summary
            .as_ref()
            .is_some_and(|summary| !summary.complete());
        if self.running || self.dry_run || !unfinished {
            return Vec::new();
        }
        self.archives
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// What [`Request::ImportStatus`](pdfs_core::control::Request::ImportStatus)
/// answers.
pub(crate) struct ImportStatusReport {
    pub running: bool,
    pub summary: Option<ImportSummary>,
    pub progress: Option<ImportProgress>,
    pub resumable: Vec<String>,
}

/// Whether an import is running, its counts and step, and what can be resumed.
pub(crate) fn import_status() -> ImportStatusReport {
    let state = IMPORT.lock();
    ImportStatusReport {
        running: state.running,
        summary: state.summary.clone(),
        progress: state.running.then(|| state.progress.clone()).flatten(),
        resumable: state.resumable(),
    }
}

/// The archives [`Request::ResumeImport`](pdfs_core::control::Request::ResumeImport)
/// would import again, or `None` when there is nothing to resume.
pub(crate) fn resumable_archives() -> Option<Vec<PathBuf>> {
    let state = IMPORT.lock();
    (!state.resumable().is_empty()).then(|| state.archives.clone())
}

/// Ask the running import to stop. False when none is running.
pub(crate) fn cancel_import() -> bool {
    match &IMPORT.lock().cancel {
        Some(flag) => {
            flag.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Publishes a running import's step and counts into [`IMPORT`].
struct Reporter;

impl Reporter {
    fn phase(&self, phase: ImportPhase, total: u64) {
        IMPORT.lock().progress = Some(ImportProgress {
            phase,
            done: 0,
            total,
            current: None,
        });
    }

    fn step(&self) {
        if let Some(progress) = IMPORT.lock().progress.as_mut() {
            progress.done += 1;
        }
    }

    fn current(&self, name: &str) {
        if let Some(progress) = IMPORT.lock().progress.as_mut() {
            progress.current = Some(name.to_string());
        }
    }

    fn counts(&self, summary: &ImportSummary) {
        IMPORT.lock().summary = Some(summary.clone());
    }

    /// Save the running state, still marked as running.
    fn checkpoint(&self) {
        IMPORT.lock().save();
    }
}

/// Photos uploaded at once. Each upload waits on Proton between its blocks,
/// so two keep the link busy where one leaves it idle; more would mostly
/// stage more copies on disk.
const IMPORT_UPLOADS: usize = 2;

/// Where an upload reads the photo from.
enum Source<'a> {
    /// A copy staged on disk, which a thumbnail can be made from.
    Staged(&'a Path),
    /// Straight out of the open archive.
    Archive(&'a mut zip::ZipArchive<std::io::BufReader<std::fs::File>>),
}

/// A staged photo handed to an upload worker.
struct Upload {
    index: usize,
    path: PathBuf,
    photo: TakeoutPhoto,
    tags: Vec<PhotoTag>,
    main_uid: Option<NodeUid>,
}

impl Upload {
    fn landed(self, result: CoreResult<NodeUid>) -> Landed {
        Landed {
            index: self.index,
            live: self.main_uid.is_some(),
            photo: self.photo,
            result,
        }
    }

    fn streamed(
        index: usize,
        photo: TakeoutPhoto,
        main_uid: Option<NodeUid>,
        result: CoreResult<NodeUid>,
    ) -> Landed {
        Landed {
            index,
            live: main_uid.is_some(),
            photo,
            result,
        }
    }
}

/// A finished upload, to be counted on the import thread.
struct Landed {
    index: usize,
    /// The video of a live pair, filed with its still.
    live: bool,
    photo: TakeoutPhoto,
    result: CoreResult<NodeUid>,
}

/// How a planned photo relates to a live or motion photo pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveRole {
    /// The still of a pair: uploaded first, tagged as a live photo.
    Still(PhotoTag),
    /// The video of a pair, filed with the still at this index of the plan.
    Video(usize),
}

/// One photo to import, after the copies Google keeps per album are folded into
/// a single upload that joins several albums.
struct Planned {
    photo: TakeoutPhoto,
    /// Every album this photo belongs to, by export title.
    albums: Vec<String>,
    /// The live-photo keys of every folded copy (each copy sits in its own
    /// folder, so each has its own key).
    live_keys: Vec<String>,
    live: Option<LiveRole>,
    /// Filled in phase 2/3: where the photo ended up on the account.
    uid: Option<NodeUid>,
}

/// Fold the scan into one entry per distinct photo, then pair live photos.
///
/// Google stores a full copy of a photo in every album folder it appears in, so
/// the same file is in the export two or three times over. They are folded on
/// `(name, size)`: a byte-identical copy under the same name is the same photo,
/// and the album memberships of the copies are unioned onto the survivor. Two
/// genuinely different photos sharing both a name and an exact size would be
/// merged — vanishingly unlikely, and the cost of being wrong is one photo
/// filed under an extra album rather than lost data.
fn plan(scan: &TakeoutScan) -> Vec<Planned> {
    let mut order: Vec<(String, u64)> = Vec::new();
    let mut by_key: HashMap<(String, u64), Planned> = HashMap::new();

    for photo in &scan.photos {
        let key = (photo.name.clone(), photo.size);
        let entry = by_key.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Planned {
                photo: photo.clone(),
                albums: Vec::new(),
                live_keys: Vec::new(),
                live: None,
                uid: None,
            }
        });
        if let Bucket::Album(title) = &photo.bucket
            && !entry.albums.iter().any(|existing| existing == title)
        {
            entry.albums.push(title.clone());
        }
        if let Some(live_key) = &photo.live_key
            && !entry.live_keys.contains(live_key)
        {
            entry.live_keys.push(live_key.clone());
        }
        // A copy that carries metadata wins over one that does not: the album
        // copy and the year-folder copy do not always both have a sidecar.
        if entry.photo.capture_time.is_none() && photo.capture_time.is_some() {
            entry.photo.capture_time = photo.capture_time;
        }
        if entry.photo.creation_time.is_none() && photo.creation_time.is_some() {
            entry.photo.creation_time = photo.creation_time;
        }
        if entry.photo.location.is_none() && photo.location.is_some() {
            entry.photo.location = photo.location;
        }
        entry.photo.favorite |= photo.favorite;
    }

    let mut planned: Vec<Planned> = order
        .into_iter()
        .filter_map(|key| by_key.remove(&key))
        .collect();
    pair_live_photos(&mut planned);
    planned
}

/// Pair the still and the video of each live or motion photo.
///
/// The halves share a live key (same folder, same stem). The video takes the
/// still's date and place when it has none of its own, because a `.MOV` half
/// usually has no sidecar. Two files whose capture times disagree by minutes
/// are not a pair, whatever their names.
fn pair_live_photos(planned: &mut [Planned]) {
    let mut stills: HashMap<&str, usize> = HashMap::new();
    for (index, item) in planned.iter().enumerate() {
        if !item.photo.is_video() {
            for key in &item.live_keys {
                stills.entry(key.as_str()).or_insert(index);
            }
        }
    }

    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut taken = vec![false; planned.len()];
    for (video, item) in planned.iter().enumerate() {
        if !item.photo.is_video() {
            continue;
        }
        let Some(&still) = item
            .live_keys
            .iter()
            .find_map(|key| stills.get(key.as_str()))
        else {
            continue;
        };
        if taken[still] {
            continue;
        }
        if let (Some(a), Some(b)) = (item.photo.capture_time, planned[still].photo.capture_time)
            && (a - b).abs() > LIVE_PAIR_MAX_SKEW
        {
            continue;
        }
        taken[still] = true;
        pairs.push((still, video));
    }

    for (still, video) in pairs {
        let tag = if planned[video].photo.is_motion_photo() {
            PhotoTag::MotionPhoto
        } else {
            PhotoTag::LivePhoto
        };
        let (capture_time, location) = (
            planned[still].photo.capture_time,
            planned[still].photo.location,
        );
        planned[still].live = Some(LiveRole::Still(tag));
        let video = &mut planned[video];
        video.live = Some(LiveRole::Video(still));
        if video.photo.capture_time.is_none() {
            video.photo.capture_time = capture_time;
        }
        if video.photo.location.is_none() {
            video.photo.location = location;
        }
    }
}

/// Open one archive and stream `entry` through `sink`.
fn read_entry<R>(
    archive: &mut zip::ZipArchive<std::io::BufReader<std::fs::File>>,
    entry: &str,
    sink: impl FnOnce(&mut (dyn Read + Send)) -> R,
) -> CoreResult<R> {
    let mut file = archive
        .by_name(entry)
        .map_err(|e| CoreError::invalid(format!("{entry}: {e}")))?;
    Ok(sink(&mut file))
}

impl Core {
    /// Copy archive entry `entry` into the thumbnail staging folder and return
    /// its path. The caller removes it.
    fn stage_takeout_entry(
        &self,
        archive: &mut zip::ZipArchive<std::io::BufReader<std::fs::File>>,
        entry: &str,
    ) -> CoreResult<PathBuf> {
        static STAGED: AtomicU64 = AtomicU64::new(0);
        let path = self.cache.raw_thumbnail_staging_dir().join(format!(
            "takeout-{}-{}",
            std::process::id(),
            STAGED.fetch_add(1, Ordering::Relaxed)
        ));
        let copied = read_entry(archive, entry, |reader| {
            let mut file = std::fs::File::create(&path)?;
            std::io::copy(reader, &mut file)
        })?;
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&path);
            return Err(CoreError::internal(format!("copy {entry}: {e}")));
        }
        Ok(path)
    }

    /// Whether the import should stop taking on new work.
    fn import_should_stop(&self, cancel: &AtomicBool) -> bool {
        cancel.load(Ordering::Relaxed) || self.shutdown.is_stopping()
    }

    /// Import Google Photos Takeout archives. Blocking; run on its own thread.
    ///
    /// Never fails as a whole: whatever stops the run early is recorded in the
    /// summary's `error` next to the counts of what it did get done, and the
    /// photos it uploaded are still filed into their albums.
    ///
    /// `cancel` stops the run at the next photo boundary — an upload already on
    /// the wire is allowed to finish, so a cancelled import never leaves a
    /// half-written revision behind.
    fn import_takeout(
        &self,
        archives: &[PathBuf],
        dry_run: bool,
        cancel: &AtomicBool,
        job: &crate::transfers::JobGuard,
    ) -> ImportSummary {
        let report = Reporter;
        let mut summary = ImportSummary {
            dry_run,
            ..Default::default()
        };
        let mut planned = Vec::new();
        let mut albums = Vec::new();

        let outcome = self.run_import(
            archives,
            cancel,
            &mut summary,
            &mut planned,
            &mut albums,
            &report,
            job,
        );
        // Whether the run left work undone because it was asked to stop. A
        // cancel that arrives after the last photo is not a cancelled run.
        let stopped_early = match &outcome {
            Ok(stopped) => *stopped,
            Err(e) => {
                warn!(error = %e, "takeout import stopped");
                summary.error = Some(e.message.clone());
                summary.error_kind = Some(e.kind);
                false
            }
        };
        summary.interrupted = stopped_early && self.shutdown.is_stopping();
        summary.cancelled = stopped_early && !summary.interrupted;

        // Albums for whatever made it up, after a cancel or an error too — but
        // not while the daemon is shutting down, which must not wait on it. A
        // resumed run files them.
        if !dry_run && !summary.interrupted && planned.iter().any(|p| p.uid.is_some()) {
            if !albums.is_empty() {
                report.phase(ImportPhase::Albums, albums.len() as u64);
                job.detail("sorting photos into albums");
                if let Err(e) = self.link_albums(&planned, &albums, &mut summary, &report) {
                    warn!(error = %e, "takeout album linking failed");
                }
                // Album linking stops for a shutdown too; a resumed run files
                // the rest.
                summary.interrupted |= self.shutdown.is_stopping();
            }
            report.counts(&summary);
        }
        if summary.uploaded > 0 {
            self.spawn_timeline_refresh();
        }
        summary
    }

    #[allow(clippy::too_many_arguments)]
    fn run_import(
        &self,
        archives: &[PathBuf],
        cancel: &AtomicBool,
        summary: &mut ImportSummary,
        planned: &mut Vec<Planned>,
        albums: &mut Vec<String>,
        report: &Reporter,
        job: &crate::transfers::JobGuard,
    ) -> CoreResult<bool> {
        // Phase 1: scan.
        report.phase(ImportPhase::Scanning, 0);
        job.detail("reading archives");
        let scan = match scan_archives_until(archives, || self.import_should_stop(cancel)) {
            Ok(scan) => scan,
            Err(_) if self.import_should_stop(cancel) => return Ok(true),
            Err(e) => return Err(CoreError::conflict(e.to_string())),
        };
        *planned = plan(&scan);
        *albums = scan.albums.clone();
        summary.found = planned.len();
        summary.skipped_trashed = scan.skipped_trashed;
        summary.unreadable_archives = scan
            .unreadable_archives
            .iter()
            .map(|archive| archive.path.display().to_string())
            .collect();
        report.counts(summary);
        info!(
            archives = archives.len(),
            unreadable = scan.unreadable_archives.len(),
            photos = summary.found,
            albums = scan.albums.len(),
            trashed = summary.skipped_trashed,
            other = scan.skipped_other,
            "takeout scan complete"
        );
        if scan.photos.is_empty() && scan.unreadable_archives.len() == archives.len() {
            return Err(CoreError::invalid(
                "none of the archives could be read; are they Google Takeout .zip files?",
            ));
        }

        let photos = self.photos();
        if self
            .rt
            .block_on(photos.get_photos_root())
            .map_err(|e| CoreError::from_api(&e, "resolve photos library"))?
            .is_none()
        {
            return Err(CoreError::invalid(
                "this account has no Proton Photos library yet; open Proton Photos once to create it, then import again",
            ));
        }

        // Phase 2: which of these are already on the account.
        job.detail("checking for photos you already have");
        self.mark_existing(planned, archives, summary, cancel, report)?;
        if self.import_should_stop(cancel) {
            return Ok(true);
        }

        let remaining = planned.iter().filter(|p| p.uid.is_none());
        summary.to_upload = remaining.clone().count();
        summary.to_upload_bytes = remaining.map(|p| p.photo.size).sum();
        report.counts(summary);
        if summary.dry_run {
            return Ok(false);
        }

        // Favorites the export knows about for photos the account already had:
        // a first import may have missed them, and setting one is idempotent.
        let existing_favorites: Vec<NodeUid> = planned
            .iter()
            .filter(|p| p.photo.favorite)
            .filter_map(|p| p.uid.clone())
            .collect();
        self.mark_favorites(existing_favorites);

        // Phase 3: upload what is left.
        report.checkpoint();
        report.phase(ImportPhase::Uploading, summary.to_upload as u64);
        job.set_total(summary.to_upload as u64);
        Ok(self.upload_planned(planned, archives, summary, cancel, job, report))
    }

    /// Fill in `uid` for every planned photo the account already holds.
    ///
    /// Two passes on purpose: the name-only query is one request per 150 photos
    /// and needs no file contents, so the expensive SHA-1 pass only runs for the
    /// handful of names that actually collide.
    fn mark_existing(
        &self,
        planned: &mut [Planned],
        archives: &[PathBuf],
        summary: &mut ImportSummary,
        cancel: &AtomicBool,
        report: &Reporter,
    ) -> CoreResult<()> {
        report.phase(ImportPhase::Checking, 0);
        let photos = self.photos();
        let names: Vec<String> = planned.iter().map(|p| p.photo.name.clone()).collect();
        let collisions = self
            .rt
            .block_on(photos.name_collisions(&names))
            .map_err(|e| CoreError::from_api(&e, "check for existing photos"))?;

        // Hash the colliding files, grouped by archive so each zip opens once.
        let mut by_archive: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (index, planned) in planned.iter().enumerate() {
            if collisions.get(index).copied().unwrap_or(false) {
                by_archive
                    .entry(planned.photo.archive)
                    .or_default()
                    .push(index);
            }
        }
        report.phase(
            ImportPhase::Checking,
            by_archive.values().map(Vec::len).sum::<usize>() as u64,
        );

        // Kept as a pair list, not a map: the duplicate lookup answers in input
        // order, so the order these were built in is what maps answers back to
        // planned photos.
        let mut digests: Vec<(usize, String)> = Vec::new();
        'archives: for (archive_index, indices) in by_archive {
            let mut archive = match open_archive(archives, archive_index) {
                Ok(archive) => archive,
                Err(e) => {
                    // Not hashed means not recognised as a duplicate; the upload
                    // phase then fails these photos on the same archive.
                    warn!(error = %e, "cannot reopen a takeout archive to check for duplicates");
                    continue;
                }
            };
            for index in indices {
                if self.import_should_stop(cancel) {
                    break 'archives;
                }
                let entry = planned[index].photo.entry.clone();
                match read_entry(&mut archive, &entry, sha1_of) {
                    Ok(Ok(digest)) => digests.push((index, digest)),
                    Ok(Err(e)) | Err(e) => {
                        warn!(entry, error = %e, "cannot hash takeout entry");
                    }
                }
                report.step();
            }
        }
        if digests.is_empty() || self.import_should_stop(cancel) {
            return Ok(());
        }

        let items: Vec<(String, String)> = digests
            .iter()
            .map(|(index, digest)| (planned[*index].photo.name.clone(), digest.clone()))
            .collect();
        let matches = self
            .rt
            .block_on(photos.find_duplicates_many(&items))
            .map_err(|e| CoreError::from_api(&e, "check for existing photos"))?;

        for (slot, (index, _)) in digests.iter().enumerate() {
            if let Some(uid) = matches.get(slot).and_then(|uids| uids.first()) {
                planned[*index].uid = Some(uid.clone());
                summary.duplicates += 1;
            }
        }
        report.counts(summary);
        Ok(())
    }

    /// Upload every planned photo that is not already on the account.
    ///
    /// Two rounds: everything but the videos of live photos first, then those
    /// videos, so each one's still has a uid to be filed under. Within a round
    /// the photos go archive by archive, so each zip's directory is read once.
    /// Returns whether it stopped with photos left, on a cancel or a shutdown.
    fn upload_planned(
        &self,
        planned: &mut [Planned],
        archives: &[PathBuf],
        summary: &mut ImportSummary,
        cancel: &AtomicBool,
        job: &crate::transfers::JobGuard,
        report: &Reporter,
    ) -> bool {
        let mut favorites: Vec<NodeUid> = Vec::new();
        let mut stopped = false;

        for videos_round in [false, true] {
            let mut by_archive: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for (index, item) in planned.iter().enumerate() {
                let is_paired_video = matches!(item.live, Some(LiveRole::Video(_)));
                if item.uid.is_none() && is_paired_video == videos_round {
                    by_archive
                        .entry(item.photo.archive)
                        .or_default()
                        .push(index);
                }
            }

            // Staged photos go to IMPORT_UPLOADS workers while this thread
            // reads the next one out of its archive. A round ends only when
            // every upload in it has landed, so a pair's video always finds
            // its still's uid.
            let (jobs, queue) = std::sync::mpsc::sync_channel::<Upload>(0);
            let queue = std::sync::Mutex::new(queue);
            std::thread::scope(|scope| {
                let (landed_tx, landed) = std::sync::mpsc::channel::<Landed>();
                for _ in 0..IMPORT_UPLOADS {
                    let queue = &queue;
                    let landed_tx = landed_tx.clone();
                    scope.spawn(move || {
                        loop {
                            let next = queue.lock().map(|queue| queue.recv());
                            let Ok(Ok(upload)) = next else { break };
                            let result = self.upload_takeout_photo(
                                Source::Staged(&upload.path),
                                &upload.photo,
                                upload.tags.clone(),
                                upload.main_uid.clone(),
                            );
                            let _ = std::fs::remove_file(&upload.path);
                            if landed_tx.send(upload.landed(result)).is_err() {
                                break;
                            }
                        }
                    });
                }
                drop(landed_tx);

                'archives: for (archive_index, indices) in by_archive {
                    if self.import_should_stop(cancel) {
                        stopped = true;
                        break;
                    }
                    let mut archive = match open_archive(archives, archive_index) {
                        Ok(archive) => archive,
                        Err(e) => {
                            // The archive was readable at scan time; whatever
                            // broke it since costs its own photos, not the run.
                            warn!(error = %e, photos = indices.len(), "cannot reopen a takeout archive");
                            summary.failed += indices.len();
                            for _ in &indices {
                                job.step();
                                report.step();
                            }
                            report.counts(summary);
                            continue;
                        }
                    };

                    for index in indices {
                        for done in landed.try_iter() {
                            self.record_upload(planned, done, summary, &mut favorites, job, report);
                        }
                        if self.import_should_stop(cancel) {
                            stopped = true;
                            break 'archives;
                        }
                        let item = &planned[index];
                        let main_uid = match item.live {
                            Some(LiveRole::Video(still)) => planned[still].uid.clone(),
                            _ => None,
                        };
                        let tags = match item.live {
                            Some(LiveRole::Still(tag)) => vec![tag],
                            _ => Vec::new(),
                        };
                        let photo = item.photo.clone();
                        job.detail(photo.name.clone());
                        report.current(&photo.name);

                        // A thumbnail is made from a file, so a photo that can
                        // have one is copied out of the archive and uploaded
                        // from the copy by a worker. Without one, Proton's apps
                        // show a blank tile until they make their own.
                        let staged = can_make_thumbnail(&photo.name, photo.size)
                            .then(|| self.stage_takeout_entry(&mut archive, &photo.entry))
                            .and_then(|staged| {
                                staged
                                    .inspect_err(|e| {
                                        warn!(error = %e,
                                              "copying a takeout photo out failed; uploading it without a thumbnail")
                                    })
                                    .ok()
                            });
                        match staged {
                            Some(path) => {
                                let upload = Upload {
                                    index,
                                    path,
                                    photo,
                                    tags,
                                    main_uid,
                                };
                                if let Err(std::sync::mpsc::SendError(upload)) = jobs.send(upload) {
                                    let _ = std::fs::remove_file(&upload.path);
                                }
                            }
                            None => {
                                let result = self.upload_takeout_photo(
                                    Source::Archive(&mut archive),
                                    &photo,
                                    tags,
                                    main_uid.clone(),
                                );
                                let done = Upload::streamed(index, photo, main_uid, result);
                                self.record_upload(
                                    planned,
                                    done,
                                    summary,
                                    &mut favorites,
                                    job,
                                    report,
                                );
                            }
                        }
                    }
                }

                drop(jobs);
                for done in landed.iter() {
                    self.record_upload(planned, done, summary, &mut favorites, job, report);
                }
            });
            if stopped {
                break;
            }
        }

        self.mark_favorites(favorites);
        stopped
    }

    /// Count one finished upload and file its uid in the plan.
    fn record_upload(
        &self,
        planned: &mut [Planned],
        done: Landed,
        summary: &mut ImportSummary,
        favorites: &mut Vec<NodeUid>,
        job: &crate::transfers::JobGuard,
        report: &Reporter,
    ) {
        job.step();
        report.step();
        match done.result {
            Ok(uid) => {
                self.note_self_change(&uid);
                summary.uploaded += 1;
                summary.bytes += done.photo.size;
                if done.live {
                    summary.live_photos += 1;
                }
                if done.photo.favorite {
                    favorites.push(uid.clone());
                }
                planned[done.index].uid = Some(uid);
                if summary.uploaded.is_multiple_of(TIMELINE_REFRESH_EVERY) {
                    self.spawn_timeline_refresh();
                }
                if summary.uploaded.is_multiple_of(CHECKPOINT_EVERY) {
                    report.counts(summary);
                    report.checkpoint();
                }
            }
            Err(e) => {
                summary.failed += 1;
                warn!(error = %e, "takeout photo upload failed");
            }
        }
        report.counts(summary);
    }

    /// Upload one planned photo, from its staged copy or straight out of its
    /// open archive.
    fn upload_takeout_photo(
        &self,
        source: Source<'_>,
        photo: &TakeoutPhoto,
        tags: Vec<PhotoTag>,
        main_uid: Option<NodeUid>,
    ) -> CoreResult<NodeUid> {
        let photos = self.photos();
        let guard = self.transfers.begin(
            photo.name.clone(),
            "",
            TransferDirection::Upload,
            photo.size,
        );
        let metadata = |capture_time: Option<i64>| PhotoUploadMetadata {
            capture_time,
            last_modification_time: capture_time.and_then(pdfs_core::timestamp::modification_time),
            main_photo_uid: main_uid.clone(),
            tags: tags.clone(),
            location: photo
                .location
                .and_then(|p| PhotoLocation::from_claimed(Some(p.latitude), Some(p.longitude))),
            // A draft under this name on the photos volume is this import's own,
            // left by a run that was cut short — possibly under an earlier
            // login, whose session id no longer matches.
            replace_existing_draft: true,
        };

        match source {
            Source::Staged(path) => {
                let thumbnails = self.upload_thumbnails(path, &photo.name);
                // No sidecar date and none in the name: the photo's own EXIF,
                // then when Google received it, before Drive files it under
                // today.
                let capture_time = photo
                    .capture_time
                    .or_else(|| pdfs_core::exiftime::capture_time_of(path))
                    .or(photo.creation_time);
                std::fs::File::open(path)
                    .map_err(|e| CoreError::internal(format!("open {}: {e}", path.display())))
                    .and_then(|file| {
                        let counting = CountingReader::new(std::io::BufReader::new(file), &guard);
                        self.rt
                            .block_on(photos.upload_photo_from(
                                &photo.name,
                                &photo.media_type,
                                counting,
                                photo.size as i64,
                                thumbnails,
                                metadata(capture_time),
                                false,
                            ))
                            .map_err(|e| CoreError::from_api(&e, "upload"))
                    })
            }
            Source::Archive(archive) => read_entry(archive, &photo.entry, |reader| {
                let counting = CountingReader::new(reader, &guard);
                self.rt
                    .block_on(photos.upload_photo_from(
                        &photo.name,
                        &photo.media_type,
                        counting,
                        photo.size as i64,
                        Vec::new(),
                        metadata(photo.capture_time.or(photo.creation_time)),
                        false,
                    ))
                    .map_err(|e| CoreError::from_api(&e, "upload"))
            })
            .and_then(|result| result),
        }
    }

    /// Mark photos as favorites. A separate call: the upload seal takes
    /// classification tags, but `Favorite` is not one of them — it goes through
    /// the photos volume's dedicated favorite endpoint.
    fn mark_favorites(&self, uids: Vec<NodeUid>) {
        if uids.is_empty() {
            return;
        }
        let updates: Vec<PhotoTagsUpdate> = uids
            .into_iter()
            .map(|node_uid| PhotoTagsUpdate {
                node_uid,
                tags_to_add: vec![PhotoTag::Favorite],
                tags_to_remove: Vec::new(),
            })
            .collect();
        if let Err(e) = self.rt.block_on(self.photos().update_photos(&updates)) {
            warn!(error = %e, "marking imported favorites failed");
        }
    }

    /// Create the albums the export names that the account lacks, then add each
    /// album's photos to it.
    fn link_albums(
        &self,
        planned: &[Planned],
        albums: &[String],
        summary: &mut ImportSummary,
        report: &Reporter,
    ) -> CoreResult<()> {
        let photos = self.photos();

        // Existing albums by decrypted name, so a second import of the same
        // export adds to the album it made the first time instead of a twin.
        let mut existing: HashMap<String, NodeUid> = HashMap::new();
        let uids = self
            .rt
            .block_on(photos.enumerate_album_node_uids())
            .map_err(|e| CoreError::from_api(&e, "list albums"))?;
        if !uids.is_empty() {
            let albums_nodes = self
                .rt
                .block_on(photos.enumerate_nodes(&uids))
                .map_err(|e| CoreError::from_api(&e, "read albums"))?;
            for node in albums_nodes {
                existing.entry(node.name.clone()).or_insert(node.uid);
            }
        }

        for title in albums {
            if self.shutdown.is_stopping() {
                break;
            }
            report.step();
            let members: Vec<NodeUid> = planned
                .iter()
                .filter(|item| item.albums.iter().any(|album| album == title))
                .filter_map(|item| item.uid.clone())
                .collect();
            if members.is_empty() {
                continue;
            }

            let album_uid = match existing.get(title) {
                Some(uid) => uid.clone(),
                None => match self.rt.block_on(photos.create_album(title)) {
                    Ok(uid) => {
                        summary.albums_created += 1;
                        existing.insert(title.clone(), uid.clone());
                        uid
                    }
                    Err(e) => {
                        warn!(album = title, error = %e, "cannot create album");
                        continue;
                    }
                },
            };

            match self
                .rt
                .block_on(photos.add_photos_to_album(&album_uid, &members))
            {
                Ok(outcomes) => {
                    for (uid, outcome) in outcomes {
                        match outcome {
                            Ok(()) => summary.album_links += 1,
                            // A photo already in the album is the expected
                            // outcome of a re-run, not a failure worth counting.
                            Err(e) => {
                                tracing::debug!(album = title, %uid, error = %e, "cannot add photo to album")
                            }
                        }
                    }
                }
                Err(e) => warn!(album = title, error = %e, "album addition failed"),
            }
            report.counts(summary);
        }
        Ok(())
    }

    /// Run an import on a background thread, logging its outcome to the activity
    /// feed. Returns immediately.
    ///
    /// Refuses while one is already running: two imports of the same export
    /// would race each other's duplicate detection and upload everything twice.
    pub(crate) fn spawn_takeout_import(
        &self,
        archives: Vec<PathBuf>,
        dry_run: bool,
    ) -> CoreResult<()> {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = IMPORT.lock();
            if state.running {
                return Err(CoreError::conflict(
                    "an import is already running; wait for it to finish or cancel it",
                ));
            }
            state.running = true;
            state.cancel = Some(cancel.clone());
            state.archives = archives.clone();
            state.dry_run = dry_run;
            // A placeholder until the scan has counts, so a front-end can
            // tell a scan from an import from the start.
            state.summary = Some(ImportSummary {
                dry_run,
                ..Default::default()
            });
            state.progress = Some(ImportProgress::default());
            state.save();
        }

        let core = self.clone();
        std::thread::spawn(move || {
            // The job is dropped only after the final state is stored: the
            // job leaving the transfer snapshot is the front-ends' cue to ask
            // for the result, which must then be there.
            let job = core.transfers.begin_job("Importing Google Photos");
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                core.import_takeout(&archives, dry_run, &cancel, &job)
            }));
            // A panic must not leave the import marked as running for the
            // life of the daemon, refusing every later one.
            let summary = run.unwrap_or_else(|_| ImportSummary {
                dry_run,
                error: Some("the import stopped on an internal error".to_string()),
                error_kind: Some(pdfs_core::control::ErrorKind::Internal),
                ..IMPORT.lock().summary.clone().unwrap_or_default()
            });
            {
                let mut state = IMPORT.lock();
                state.running = false;
                state.cancel = None;
                state.progress = None;
                state.summary = Some(summary.clone());
                state.save();
            }
            drop(job);
            if !dry_run {
                core.log_activity(
                    ActivityKind::Upload,
                    "Google Photos import",
                    activity_line(&summary),
                    summary.complete(),
                );
            }
        });
        Ok(())
    }
}

/// The activity-feed line for a finished import. The feed is the daemon's own
/// log, in English like the rest of it.
fn activity_line(summary: &ImportSummary) -> String {
    let mut parts = vec![format!("{} uploaded", summary.uploaded)];
    if summary.duplicates > 0 {
        parts.push(format!("{} already there", summary.duplicates));
    }
    if summary.albums_created > 0 {
        parts.push(format!("{} albums", summary.albums_created));
    }
    if summary.failed > 0 {
        parts.push(format!("{} failed", summary.failed));
    }
    if !summary.unreadable_archives.is_empty() {
        parts.push(format!(
            "{} archives unreadable",
            summary.unreadable_archives.len()
        ));
    }
    if summary.interrupted {
        parts.push("interrupted".to_string());
    } else if summary.cancelled {
        parts.push("cancelled".to_string());
    }
    if let Some(error) = &summary.error {
        parts.push(error.clone());
    }
    parts.join(" · ")
}

/// Open one of the import's archives by index.
fn open_archive(
    archives: &[PathBuf],
    index: usize,
) -> CoreResult<zip::ZipArchive<std::io::BufReader<std::fs::File>>> {
    let path = archives
        .get(index)
        .ok_or_else(|| CoreError::internal(format!("archive {index} is not in the import")))?;
    let file = std::fs::File::open(path)
        .map_err(|e| CoreError::invalid(format!("cannot open {}: {e}", path.display())))?;
    zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| CoreError::invalid(format!("{} is not a readable zip: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfs_core::takeout::{GeoPoint, TakeoutPhoto};

    fn photo(name: &str, size: u64, bucket: Bucket) -> TakeoutPhoto {
        let folder = match &bucket {
            Bucket::Album(title) => title.clone(),
            _ => "Photos from 2019".to_string(),
        };
        let (stem, ext) = name.rsplit_once('.').unwrap();
        let media_type = match ext.to_ascii_lowercase().as_str() {
            "mov" | "mp4" | "mp" => "video/mp4",
            _ => "image/jpeg",
        };
        TakeoutPhoto {
            archive: 0,
            entry: format!("Takeout/Google Photos/{folder}/{name}"),
            name: name.to_string(),
            size,
            media_type: media_type.to_string(),
            capture_time: None,
            creation_time: None,
            location: None,
            favorite: false,
            bucket,
            live_key: Some(format!(
                "{folder}/{}",
                if ext.eq_ignore_ascii_case("mp") {
                    name.to_lowercase()
                } else {
                    stem.to_lowercase()
                }
            )),
        }
    }

    #[test]
    fn album_copies_of_one_photo_fold_into_a_single_upload() {
        let scan = TakeoutScan {
            photos: vec![
                photo("IMG_1.jpg", 100, Bucket::Timeline),
                photo("IMG_1.jpg", 100, Bucket::Album("Iceland".into())),
                photo("IMG_1.jpg", 100, Bucket::Album("Best of".into())),
                photo("IMG_2.jpg", 200, Bucket::Timeline),
            ],
            albums: vec!["Iceland".into(), "Best of".into()],
            ..Default::default()
        };

        let planned = plan(&scan);
        assert_eq!(planned.len(), 2, "the three copies are one upload");
        assert_eq!(planned[0].albums, vec!["Iceland", "Best of"]);
        assert!(planned[1].albums.is_empty());
    }

    #[test]
    fn same_name_different_size_stays_two_photos() {
        let scan = TakeoutScan {
            photos: vec![
                photo("IMG_1.jpg", 100, Bucket::Timeline),
                photo("IMG_1.jpg", 101, Bucket::Album("Iceland".into())),
            ],
            ..Default::default()
        };

        assert_eq!(plan(&scan).len(), 2);
    }

    #[test]
    fn metadata_from_any_copy_survives_the_fold() {
        // Only the album copy carries a sidecar; the fold must not lose it.
        let bare = photo("IMG_1.jpg", 100, Bucket::Timeline);
        let mut rich = photo("IMG_1.jpg", 100, Bucket::Album("Iceland".into()));
        rich.capture_time = Some(1_560_000_000);
        rich.creation_time = Some(1_600_000_000);
        rich.location = Some(GeoPoint {
            latitude: 64.1,
            longitude: -21.9,
        });
        rich.favorite = true;

        let planned = plan(&TakeoutScan {
            photos: vec![bare, rich],
            ..Default::default()
        });

        assert_eq!(planned[0].photo.capture_time, Some(1_560_000_000));
        assert_eq!(planned[0].photo.creation_time, Some(1_600_000_000));
        assert!(planned[0].photo.location.is_some());
        assert!(planned[0].photo.favorite);
    }

    #[test]
    fn a_live_photo_video_is_filed_with_its_still_and_takes_its_date() {
        let mut still = photo("IMG_1.HEIC", 100, Bucket::Timeline);
        still.capture_time = Some(1_560_000_000);
        still.location = Some(GeoPoint {
            latitude: 64.1,
            longitude: -21.9,
        });
        let video = photo("IMG_1.MOV", 300, Bucket::Timeline);
        let plain = photo("IMG_2.jpg", 100, Bucket::Timeline);

        let planned = plan(&TakeoutScan {
            photos: vec![video, still, plain],
            ..Default::default()
        });

        assert_eq!(planned[0].live, Some(LiveRole::Video(1)));
        assert_eq!(planned[1].live, Some(LiveRole::Still(PhotoTag::LivePhoto)));
        assert_eq!(planned[2].live, None);
        assert_eq!(planned[0].photo.capture_time, Some(1_560_000_000));
        assert!(planned[0].photo.location.is_some());
    }

    #[test]
    fn a_pixel_motion_photo_pairs_on_the_mp_name() {
        let planned = plan(&TakeoutScan {
            photos: vec![
                photo("PXL_1.MP.jpg", 100, Bucket::Timeline),
                photo("PXL_1.MP", 300, Bucket::Timeline),
            ],
            ..Default::default()
        });
        assert_eq!(
            planned[0].live,
            Some(LiveRole::Still(PhotoTag::MotionPhoto))
        );
        assert_eq!(planned[1].live, Some(LiveRole::Video(0)));
    }

    #[test]
    fn halves_taken_minutes_apart_are_not_a_pair() {
        let mut still = photo("IMG_1.jpg", 100, Bucket::Timeline);
        still.capture_time = Some(1_560_000_000);
        let mut video = photo("IMG_1.MOV", 300, Bucket::Timeline);
        video.capture_time = Some(1_560_000_000 + 3_600);

        let planned = plan(&TakeoutScan {
            photos: vec![still, video],
            ..Default::default()
        });
        assert!(planned.iter().all(|p| p.live.is_none()));
    }

    #[test]
    fn an_album_copy_pairs_through_any_of_its_folders() {
        // The still was first seen in an album, the video only in the year
        // folder; the album copy of the still also sits in the year folder.
        let planned = plan(&TakeoutScan {
            photos: vec![
                photo("IMG_1.HEIC", 100, Bucket::Album("Trip".into())),
                photo("IMG_1.HEIC", 100, Bucket::Timeline),
                photo("IMG_1.MOV", 300, Bucket::Timeline),
            ],
            ..Default::default()
        });
        assert_eq!(planned.len(), 2);
        assert_eq!(planned[1].live, Some(LiveRole::Video(0)));
    }

    #[test]
    fn a_run_the_daemon_never_finished_loads_as_interrupted_and_resumable() {
        let path =
            std::env::temp_dir().join(format!("pdfs-takeout-state-{}.json", std::process::id()));
        let saved = SavedImport {
            archives: vec![PathBuf::from("/exports/takeout-001.zip")],
            dry_run: false,
            running: true,
            summary: None,
        };
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

        let state = ImportState::load(Some(path.clone()));
        let _ = std::fs::remove_file(&path);

        assert!(!state.running);
        assert!(state.summary.as_ref().unwrap().interrupted);
        assert_eq!(state.resumable(), vec!["/exports/takeout-001.zip"]);
    }

    #[test]
    fn a_finished_import_or_a_scan_is_not_resumable() {
        let done = ImportState {
            archives: vec![PathBuf::from("/a.zip")],
            summary: Some(ImportSummary {
                uploaded: 3,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(done.resumable().is_empty());

        let scan = ImportState {
            archives: vec![PathBuf::from("/a.zip")],
            dry_run: true,
            summary: Some(ImportSummary {
                cancelled: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(scan.resumable().is_empty());

        let failed = ImportState {
            archives: vec![PathBuf::from("/a.zip")],
            summary: Some(ImportSummary {
                failed: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(failed.resumable(), vec!["/a.zip"]);
    }

    #[test]
    fn state_survives_a_save_and_load() {
        let path = std::env::temp_dir().join(format!(
            "pdfs-takeout-roundtrip-{}.json",
            std::process::id()
        ));
        let state = ImportState {
            archives: vec![PathBuf::from("/a.zip")],
            summary: Some(ImportSummary {
                uploaded: 2,
                cancelled: true,
                ..Default::default()
            }),
            path: Some(path.clone()),
            ..Default::default()
        };
        state.save();
        let loaded = ImportState::load(Some(path.clone()));
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.summary, state.summary);
        assert_eq!(loaded.archives, state.archives);
    }

    #[test]
    fn sha1_matches_the_known_digest_of_abc() {
        let mut reader: &[u8] = b"abc";
        assert_eq!(
            sha1_of(&mut reader).unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
}
