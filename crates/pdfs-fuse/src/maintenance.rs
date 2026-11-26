//! Database maintenance moved off the request thread: `VACUUM` and the deep
//! integrity check, one at a time, with the last check's result kept.

use super::*;

impl Core {
    /// Start a background `VACUUM`, or refuse because maintenance is already
    /// running.
    ///
    /// Returns as soon as the thread is spawned; the caller acknowledges the
    /// request. The outcome is a log line and a job that disappears from
    /// `GetQueueStatus` when it is done — a vacuum has nothing to report that a
    /// follow-up `CacheInspect` cannot show.
    pub(super) fn start_vacuum(&self) -> CoreResult<()> {
        {
            let mut m = self.maintenance.lock();
            if m.running {
                return Err(CoreError::conflict(
                    "database maintenance is already running",
                ));
            }
            m.running = true;
        }
        let core = self.clone();
        std::thread::spawn(move || {
            let job = core.transfers.begin_job("Compacting the cache database");
            job.detail("Rewriting the database file");
            let started = Instant::now();
            match core.db.vacuum() {
                Ok(outcome) => info!(
                    freed_bytes = outcome.freed_bytes(),
                    before_bytes = outcome.before_bytes,
                    after_bytes = outcome.after_bytes,
                    wal_frames = outcome.wal_frames_checkpointed,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "cache database vacuumed"
                ),
                Err(e) => warn!(error = %e, "vacuum failed"),
            }
            core.maintenance.lock().running = false;
        });
        Ok(())
    }

    /// Answer a deep `CacheInspect`: hand back the last completed integrity
    /// check, and start one if none is running.
    ///
    /// Returns `(problems, checked, running)`. A first request gets
    /// `(_, false, true)` and the answer lands on a later one — the check reads
    /// every page of the database under the connection mutex, so the choice is
    /// between answering late and freezing the mount until it finishes.
    pub(super) fn start_or_read_integrity_check(&self) -> (Vec<String>, bool, bool) {
        let mut m = self.maintenance.lock();
        if let Some(problems) = m.integrity.clone() {
            return (problems, true, m.running);
        }
        if m.running {
            return (Vec::new(), false, true);
        }
        m.running = true;
        drop(m);

        let core = self.clone();
        std::thread::spawn(move || {
            let job = core
                .transfers
                .begin_background_job("Checking the cache database");
            job.detail("Verifying every page");
            let problems = match core.db.integrity_check() {
                Ok(problems) => problems,
                Err(e) => vec![format!("integrity check failed: {e:?}")],
            };
            if problems.is_empty() {
                info!("cache database integrity check found no problems");
            } else {
                warn!(problems = problems.len(), "cache database integrity check");
            }
            let mut m = core.maintenance.lock();
            m.integrity = Some(problems);
            m.running = false;
        });
        (Vec::new(), false, true)
    }
}

/// Background database maintenance: what is running, and what the last deep
/// integrity check found.
///
/// One at a time — both operations are whole-database and there is nothing to
/// gain from overlapping them — and the result of a check outlives the request
/// that asked for it, because the request is answered before the check finishes.
#[derive(Default)]
pub(super) struct Maintenance {
    /// A vacuum or integrity check is running right now.
    running: bool,
    /// Findings of the most recent completed deep check. `None` means none has
    /// finished this run, which is *not* the same as "found nothing".
    integrity: Option<Vec<String>>,
}
