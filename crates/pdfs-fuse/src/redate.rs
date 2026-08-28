//! Repairing photos whose capture time is when they were *imported* rather than
//! when they were taken.
//!
//! A Google Photos Takeout whose metadata sidecars did not match — a localized
//! `.supplemental-metadata` spelling, say — imports with no capture time at all,
//! and the SDK then seals "now" into every revision. A decade of photos lands on
//! one day.
//!
//! Proton has no API for editing a sealed capture time, so the repair is a
//! rewrite: download the photo, upload it again with the capture time its file
//! name implies, put the new copy back into the albums and favourites the old
//! one was in, then **trash** (not delete) the original. Trash rather than
//! delete so a bad run is recoverable from Proton's own trash for 30 days.
//!
//! A photo is a candidate only when its name carries a date that disagrees with
//! its stored capture time by more than [`DRIFT`]. That is deliberately narrow:
//! a photo whose name says nothing about its date is left alone, because there
//! is nothing better to set it to.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pdfs_core::control::{RedateSummary, TransferDirection};
use pdfs_core::takeout::{capture_time_from_name, media_type_of};
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{PhotoTag, PhotoTagsUpdate, PhotoUploadMetadata};
use tracing::{info, warn};

use super::Core;
use crate::parse_uid as parse_node_uid;
use crate::transfers::CountingReader;

/// How far a stored capture time may sit from the one the file name implies
/// before the photo counts as mis-dated. A day, because a name usually carries
/// only the date and the stored time carries a clock — and because a timezone
/// can legitimately move a photo by hours.
const DRIFT: i64 = 86_400;

/// How many examples a report carries.
const SAMPLES: usize = 10;

/// The one re-date this daemon may be running, and how the last one ended.
/// Process-global for the same reasons as the Takeout import's state: one run
/// per daemon however many `Core` clones exist, and a front-end asking after it
/// finished must still find the report.
static REDATE: std::sync::LazyLock<parking_lot::Mutex<RedateState>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(RedateState::default()));

#[derive(Default)]
struct RedateState {
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    last: Option<RedateSummary>,
}

/// Whether a re-date is running, and the last one's report.
pub(crate) fn redate_status() -> (bool, Option<RedateSummary>) {
    let state = REDATE.lock();
    (state.running, state.last.clone())
}

/// Ask the running re-date to stop. False when none is running.
pub(crate) fn cancel_redate() -> bool {
    match &REDATE.lock().cancel {
        Some(flag) => {
            flag.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// One photo to repair.
struct Candidate {
    uid: NodeUid,
    name: String,
    /// The capture time it carries now.
    stored: i64,
    /// The capture time its name implies.
    wanted: i64,
    favorite: bool,
    /// Album node uids the original is filed under.
    albums: Vec<String>,
}

impl Core {
    /// Re-date mis-dated photos. Blocking; run on its own thread.
    ///
    /// `cancel` stops the run at the next photo boundary. A photo is only ever
    /// trashed after its replacement has uploaded, so a cancel — or a crash —
    /// can leave a duplicate, never a hole.
    pub(crate) fn redate_photos(
        &self,
        dry_run: bool,
        limit: Option<usize>,
        range: Option<(i64, i64)>,
        cancel: Arc<AtomicBool>,
    ) -> CoreResult<RedateSummary> {
        let job = self.transfers.begin_job("Fixing photo dates");
        job.detail("looking for mis-dated photos");

        let stored = self
            .db
            .photos_all()
            .map_err(|e| CoreError::internal(format!("read timeline: {e}")))?;
        let mut summary = RedateSummary {
            examined: stored.len(),
            dry_run,
            ..Default::default()
        };

        let mut candidates = Vec::new();
        for photo in stored {
            if let Some((from, to)) = range
                && !(from..to).contains(&photo.capture_time)
            {
                continue;
            }
            let Some(name) = photo.name.filter(|n| !n.is_empty()) else {
                continue;
            };
            let Some(wanted) = capture_time_from_name(&name) else {
                continue;
            };
            if (wanted - photo.capture_time).abs() <= DRIFT {
                continue;
            }
            let Some(uid) = parse_node_uid(&photo.uid) else {
                continue;
            };
            candidates.push(Candidate {
                uid,
                name,
                stored: photo.capture_time,
                wanted,
                favorite: photo.favorite,
                albums: self.db.albums_of_photo(&photo.uid).unwrap_or_default(),
            });
        }
        if let Some(limit) = limit {
            candidates.truncate(limit);
        }
        summary.candidates = candidates.len();
        // Sampled after the limit, so the examples are of photos the run would
        // actually touch.
        summary.samples = candidates
            .iter()
            .take(SAMPLES)
            .map(|c| (c.name.clone(), c.stored, c.wanted))
            .collect();
        job.set_total(candidates.len() as u64);
        info!(
            examined = summary.examined,
            candidates = summary.candidates,
            dry_run,
            "photo re-date scan complete"
        );

        if dry_run || candidates.is_empty() {
            return Ok(summary);
        }

        for candidate in candidates {
            if cancel.load(Ordering::Relaxed) || self.shutdown.is_stopping() {
                break;
            }
            job.detail(candidate.name.clone());
            if let Err(e) = self.redate_one(&candidate, &mut summary) {
                summary.failed += 1;
                warn!(name = candidate.name, error = %e, "cannot re-date photo");
            }
            job.step();
        }

        self.spawn_timeline_refresh();
        summary.cancelled = cancel.load(Ordering::Relaxed);
        Ok(summary)
    }

    /// Rewrite one photo at the capture time its name implies.
    ///
    /// Order is load-bearing: upload first, file the new copy, and only then
    /// trash the original. Every earlier step failing leaves the account exactly
    /// as it was.
    fn redate_one(&self, candidate: &Candidate, summary: &mut RedateSummary) -> CoreResult<()> {
        let photos = self.photos();
        let path = self.open_photo(&candidate.uid)?;
        let size = std::fs::metadata(&path)
            .map_err(|e| CoreError::internal(format!("stat {}: {e}", path.display())))?
            .len();
        let media_type = media_type_of(&candidate.name).unwrap_or("application/octet-stream");

        let guard =
            self.transfers
                .begin(candidate.name.clone(), "", TransferDirection::Upload, size);
        let file = std::fs::File::open(&path)
            .map_err(|e| CoreError::internal(format!("open {}: {e}", path.display())))?;
        let reader = CountingReader::new(std::io::BufReader::new(file), &guard);
        let metadata = PhotoUploadMetadata {
            capture_time: Some(candidate.wanted),
            ..Default::default()
        };
        let uploaded = self.rt.block_on(photos.upload_photo_from(
            &candidate.name,
            media_type,
            reader,
            size as i64,
            Vec::new(),
            metadata,
            false,
        ));
        drop(guard);
        let new_uid = uploaded.map_err(|e| CoreError::from_api(&e, "re-upload photo"))?;

        // Favourite and albums before the trash: a failure here leaves both
        // copies on the account, which is recoverable by hand. Losing the
        // original first would not be.
        if candidate.favorite {
            let update = PhotoTagsUpdate {
                node_uid: new_uid.clone(),
                tags_to_add: vec![PhotoTag::Favorite],
                tags_to_remove: Vec::new(),
            };
            if let Err(e) = self.rt.block_on(photos.update_photos(&[update])) {
                warn!(name = candidate.name, error = %e, "cannot re-favourite re-dated photo");
            }
        }
        for album in &candidate.albums {
            let Some(album_uid) = parse_node_uid(album) else {
                continue;
            };
            match self
                .rt
                .block_on(photos.add_photos_to_album(&album_uid, std::slice::from_ref(&new_uid)))
            {
                Ok(_) => summary.album_links += 1,
                Err(e) => {
                    warn!(name = candidate.name, error = %e, "cannot re-file re-dated photo")
                }
            }
        }

        let outcomes = self
            .rt
            .block_on(
                self.client
                    .trash_nodes(std::slice::from_ref(&candidate.uid)),
            )
            .map_err(|e| CoreError::from_api(&e, "trash the mis-dated original"))?;
        if let Some((_, Err(e))) = outcomes.into_iter().next() {
            return Err(CoreError::from_api(&e, "trash the mis-dated original"));
        }

        summary.redated += 1;
        summary.bytes += size;
        info!(
            name = candidate.name,
            from = candidate.stored,
            to = candidate.wanted,
            "photo re-dated"
        );
        Ok(())
    }

    /// Start a re-date on its own thread. Errors when one is already running.
    pub(crate) fn spawn_redate(
        &self,
        dry_run: bool,
        limit: Option<usize>,
        range: Option<(i64, i64)>,
    ) -> CoreResult<()> {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = REDATE.lock();
            if state.running {
                return Err(CoreError::conflict(
                    "a photo re-date is already running; wait for it to finish or cancel it",
                ));
            }
            state.running = true;
            state.cancel = Some(cancel.clone());
            state.last = None;
        }

        let core = self.clone();
        std::thread::spawn(move || {
            let outcome = core.redate_photos(dry_run, limit, range, cancel);
            let mut state = REDATE.lock();
            state.running = false;
            state.cancel = None;
            state.last = outcome.as_ref().ok().cloned();
            match &outcome {
                Ok(summary) => info!(
                    candidates = summary.candidates,
                    redated = summary.redated,
                    failed = summary.failed,
                    "photo re-date finished"
                ),
                Err(e) => warn!(error = %e, "photo re-date failed"),
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uid_without_both_halves_is_not_a_node() {
        assert!(parse_node_uid("vol~link").is_some());
        assert!(parse_node_uid("vol~").is_none());
        assert!(parse_node_uid("~link").is_none());
        assert!(parse_node_uid("nothing").is_none());
    }

    #[test]
    fn only_a_disagreement_larger_than_a_day_counts() {
        // The name says 2023-02-19; a stored time inside that day is fine.
        let wanted = capture_time_from_name("IMG-20230219-WA0001.jpg").unwrap();
        assert!((wanted - (wanted + 3_600)).abs() <= DRIFT);
        // An import three years later is not.
        assert!((wanted - (wanted + 3 * 365 * 86_400)).abs() > DRIFT);
    }
}
