//! Importing a Google Photos Takeout export into Proton Photos.
//!
//! [`pdfs_core::takeout`] says what is in the archives; this is the part that
//! talks to Proton. One import runs on its own thread and moves through four
//! phases, reporting into the transfer registry as a job so the GUI's transfer
//! list shows it without a bespoke widget:
//!
//! 1. **Scan** — read the archives' central directories and sidecars.
//! 2. **Deduplicate against the account** — ask the photos volume which of the
//!    names already exist ([`ProtonPhotosClient::name_collisions`]), hash only
//!    those files, then confirm with the content digest. A library that is
//!    already half-synced by Proton Photos on a phone therefore costs one cheap
//!    request per 150 names and reads no media at all for the misses.
//! 3. **Upload** the survivors, streaming each entry straight out of its zip —
//!    no extraction to disk, so the import needs no scratch space.
//! 4. **Albums** — create the albums the export names that the account lacks,
//!    then add every photo of each album, the ones that were already on the
//!    account included.
//!
//! Re-running an interrupted import is the resume story: phase 2 finds
//! everything the previous run uploaded and skips it. Nothing is persisted
//! between runs.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pdfs_core::control::{ActivityKind, ImportSummary, TransferDirection};
use pdfs_core::takeout::{Bucket, TakeoutPhoto, TakeoutScan, scan_archives};
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{PhotoTag, PhotoTagsUpdate, PhotoUploadMetadata};
use sha1::{Digest, Sha1};
use tracing::{info, warn};

use super::Core;
use crate::transfers::CountingReader;

/// The one import this daemon may be running, and how the last one ended.
///
/// Process-global rather than a [`Core`] field because it must be *one* import
/// per daemon however many `Core` clones exist (the forked on-demand mounts each
/// hold one), and because a front-end asking for the result after the import
/// finished must still find it.
static IMPORT: std::sync::LazyLock<parking_lot::Mutex<ImportState>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(ImportState::default()));

#[derive(Default)]
struct ImportState {
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    last: Option<ImportSummary>,
}

/// Whether an import is running, and the last one's report.
pub(crate) fn import_status() -> (bool, Option<ImportSummary>) {
    let state = IMPORT.lock();
    (state.running, state.last.clone())
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

/// One photo to import, after the copies Google keeps per album are folded into
/// a single upload that joins several albums.
struct Planned {
    photo: TakeoutPhoto,
    /// Every album this photo belongs to, by export title.
    albums: Vec<String>,
    /// Filled in phase 2/3: where the photo ended up on the account.
    uid: Option<NodeUid>,
}

/// Fold the scan into one entry per distinct photo.
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
                uid: None,
            }
        });
        if let Bucket::Album(title) = &photo.bucket
            && !entry.albums.iter().any(|existing| existing == title)
        {
            entry.albums.push(title.clone());
        }
        // A copy that carries a capture time wins over one that does not: the
        // album copy and the year-folder copy do not always both have a sidecar.
        if entry.photo.capture_time.is_none() && photo.capture_time.is_some() {
            entry.photo.capture_time = photo.capture_time;
        }
        entry.photo.favorite |= photo.favorite;
    }

    order
        .into_iter()
        .filter_map(|key| by_key.remove(&key))
        .collect()
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
    /// Import Google Photos Takeout archives. Blocking; run on its own thread.
    ///
    /// `cancel` stops the run at the next photo boundary — an upload already on
    /// the wire is allowed to finish, so a cancelled import never leaves a
    /// half-written revision behind.
    pub(crate) fn import_takeout(
        &self,
        archives: Vec<PathBuf>,
        dry_run: bool,
        cancel: Arc<AtomicBool>,
    ) -> CoreResult<ImportSummary> {
        let job = self.transfers.begin_job("Importing Google Photos");
        job.detail("reading archives");

        let scan = scan_archives(&archives)?;
        let mut planned = plan(&scan);
        let mut summary = ImportSummary {
            found: planned.len(),
            skipped_trashed: scan.skipped_trashed,
            ..Default::default()
        };
        job.set_total(planned.len() as u64);
        info!(
            archives = archives.len(),
            photos = summary.found,
            albums = scan.albums.len(),
            trashed = summary.skipped_trashed,
            "takeout scan complete"
        );

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
        self.mark_existing(&mut planned, &archives, &mut summary, &cancel)?;

        if dry_run {
            summary.duplicates = planned.iter().filter(|p| p.uid.is_some()).count();
            return Ok(summary);
        }

        // Phase 3: upload what is left, one archive at a time so each zip's
        // central directory is parsed once.
        self.upload_planned(&mut planned, &archives, &mut summary, &cancel, &job)?;

        // Phase 4: albums. Attempted even after a cancel — the photos that did
        // upload should still land in their albums.
        if !scan.albums.is_empty() {
            job.detail("sorting photos into albums");
            if let Err(e) = self.link_albums(&planned, &scan.albums, &mut summary) {
                warn!(error = %e, "takeout album linking failed");
            }
        }

        self.spawn_timeline_refresh();
        summary.cancelled = cancel.load(Ordering::Relaxed);
        Ok(summary)
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
    ) -> CoreResult<()> {
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

        // Kept as a pair list, not a map: the duplicate lookup answers in input
        // order, so the order these were built in is what maps answers back to
        // planned photos.
        let mut digests: Vec<(usize, String)> = Vec::new();
        for (archive_index, indices) in by_archive {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let mut archive = open_archive(archives, archive_index)?;
            for index in indices {
                let entry = planned[index].photo.entry.clone();
                match read_entry(&mut archive, &entry, sha1_of) {
                    Ok(Ok(digest)) => digests.push((index, digest)),
                    Ok(Err(e)) | Err(e) => {
                        warn!(entry, error = %e, "cannot hash takeout entry");
                    }
                }
            }
        }
        if digests.is_empty() {
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
        Ok(())
    }

    /// Upload every planned photo that is not already on the account.
    fn upload_planned(
        &self,
        planned: &mut [Planned],
        archives: &[PathBuf],
        summary: &mut ImportSummary,
        cancel: &AtomicBool,
        job: &crate::transfers::JobGuard,
    ) -> CoreResult<()> {
        let photos = self.photos();
        let mut favorites: Vec<NodeUid> = Vec::new();

        let mut by_archive: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (index, item) in planned.iter().enumerate() {
            if item.uid.is_none() {
                by_archive
                    .entry(item.photo.archive)
                    .or_default()
                    .push(index);
            }
        }

        for (archive_index, indices) in by_archive {
            if cancel.load(Ordering::Relaxed) || self.shutdown.is_stopping() {
                break;
            }
            let mut archive = open_archive(archives, archive_index)?;

            for index in indices {
                if cancel.load(Ordering::Relaxed) || self.shutdown.is_stopping() {
                    break;
                }
                let photo = planned[index].photo.clone();
                job.detail(photo.name.clone());

                let guard = self.transfers.begin(
                    photo.name.clone(),
                    "",
                    TransferDirection::Upload,
                    photo.size,
                );
                let metadata = PhotoUploadMetadata {
                    capture_time: photo.capture_time,
                    ..Default::default()
                };
                let uploaded = read_entry(&mut archive, &photo.entry, |reader| {
                    let counting = CountingReader::new(reader, &guard);
                    self.rt.block_on(photos.upload_photo_from(
                        &photo.name,
                        &photo.media_type,
                        counting,
                        photo.size as i64,
                        Vec::new(),
                        metadata,
                        false,
                    ))
                });
                drop(guard);
                job.step();

                match uploaded {
                    Ok(Ok(uid)) => {
                        summary.uploaded += 1;
                        summary.bytes += photo.size;
                        if photo.favorite {
                            favorites.push(uid.clone());
                        }
                        planned[index].uid = Some(uid);
                    }
                    Ok(Err(e)) => {
                        summary.failed += 1;
                        warn!(name = photo.name, error = %e, "takeout photo upload failed");
                    }
                    Err(e) => {
                        summary.failed += 1;
                        warn!(name = photo.name, error = %e, "cannot read takeout entry");
                    }
                }
            }
        }

        // Favorites are a separate call: the upload seal takes classification
        // tags, but `Favorite` is not one of them — it goes through the photos
        // volume's dedicated favorite endpoint.
        if !favorites.is_empty() {
            let updates: Vec<PhotoTagsUpdate> = favorites
                .into_iter()
                .map(|node_uid| PhotoTagsUpdate {
                    node_uid,
                    tags_to_add: vec![PhotoTag::Favorite],
                    tags_to_remove: Vec::new(),
                })
                .collect();
            if let Err(e) = self.rt.block_on(photos.update_photos(&updates)) {
                warn!(error = %e, "marking imported favorites failed");
            }
        }
        Ok(())
    }

    /// Create the albums the export names that the account lacks, then add each
    /// album's photos to it.
    fn link_albums(
        &self,
        planned: &[Planned],
        albums: &[String],
        summary: &mut ImportSummary,
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
                                warn!(album = title, %uid, error = %e, "cannot add photo to album")
                            }
                        }
                    }
                }
                Err(e) => warn!(album = title, error = %e, "album addition failed"),
            }
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
            state.last = None;
        }

        let core = self.clone();
        std::thread::spawn(move || {
            let outcome = core.import_takeout(archives, dry_run, cancel);
            {
                let mut state = IMPORT.lock();
                state.running = false;
                state.cancel = None;
                state.last = outcome.as_ref().ok().cloned();
            }
            match outcome {
                Ok(summary) => {
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
                    if summary.cancelled {
                        parts.push("cancelled".to_string());
                    }
                    core.log_activity(
                        ActivityKind::Upload,
                        "Google Photos import",
                        parts.join(" · "),
                        summary.failed == 0 && !summary.cancelled,
                    );
                }
                Err(e) => {
                    warn!(error = %e, "takeout import failed");
                    core.log_activity(ActivityKind::Upload, "Google Photos import", &e, false);
                }
            }
        });
        Ok(())
    }
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

/// The lowercase-hex SHA-1 of everything `reader` yields — the digest Proton's
/// photo duplicate detection is keyed on.
fn sha1_of(reader: &mut (dyn Read + Send)) -> CoreResult<String> {
    let mut hasher = Sha1::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| CoreError::invalid(format!("read failed: {e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfs_core::takeout::TakeoutPhoto;

    fn photo(name: &str, size: u64, bucket: Bucket) -> TakeoutPhoto {
        TakeoutPhoto {
            archive: 0,
            entry: format!("Takeout/Google Photos/x/{name}"),
            name: name.to_string(),
            size,
            media_type: "image/jpeg".to_string(),
            capture_time: None,
            favorite: false,
            bucket,
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
        let mut bare = photo("IMG_1.jpg", 100, Bucket::Timeline);
        bare.capture_time = None;
        let mut rich = photo("IMG_1.jpg", 100, Bucket::Album("Iceland".into()));
        rich.capture_time = Some(1_560_000_000);
        rich.favorite = true;

        let planned = plan(&TakeoutScan {
            photos: vec![bare, rich],
            ..Default::default()
        });

        assert_eq!(planned[0].photo.capture_time, Some(1_560_000_000));
        assert!(planned[0].photo.favorite);
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
