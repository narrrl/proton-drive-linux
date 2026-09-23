//! Pausing sync: the user's "stop uploading for a while".
//!
//! A pause holds back the two things that push local state to the remote — the
//! drain worker's queued uploads and changes, and the mirror-folder reconcile.
//! Nothing else stops: reads through the mount keep hydrating on demand (a read
//! is not a sync), writes keep being accepted and staged, and the watcher and
//! poll keep noticing changes. Resuming lets all of it catch up at once.
//!
//! The pause is persisted in the database, so a restart does not quietly undo
//! it. A timed pause ends by itself; [`Core::set_sync_paused`] arms a timer for
//! that, and [`Core::sync_paused`] compares against the clock anyway, so a
//! deadline that passed while the daemon was down is already over on mount.

use super::*;

/// The `state` key holding the resume time, in Unix seconds.
const PAUSED_UNTIL_KEY: &str = "sync_paused_until";

/// A pause with no end time: it lasts until the user resumes.
pub(crate) const PAUSED_INDEFINITELY: i64 = i64::MAX;

/// The persisted pause, or `0` when there is none (or it cannot be read — a
/// broken state row must not keep sync paused forever).
pub(crate) fn load_paused_until(db: &Db) -> i64 {
    match db.state_i64(PAUSED_UNTIL_KEY) {
        Ok(Some(until)) => until,
        Ok(None) => 0,
        Err(error) => {
            warn!(%error, "reading the sync pause failed; treating sync as running");
            0
        }
    }
}

/// Whether a pause lasting until `until` is still in force at `now` (both Unix
/// seconds). `0` is "not paused".
pub(crate) fn paused_at(until: i64, now: i64) -> bool {
    until != 0 && now < until
}

impl Core {
    /// True while the user has sync paused.
    pub(crate) fn sync_paused(&self) -> bool {
        paused_at(self.sync_paused_until.load(Ordering::Relaxed), now_secs())
    }

    /// The pause's end, for status reporting: `None` when not paused,
    /// `Some(None)` when paused until resumed, `Some(Some(t))` when it ends at
    /// Unix second `t`.
    pub(crate) fn sync_pause(&self) -> Option<Option<i64>> {
        let until = self.sync_paused_until.load(Ordering::Relaxed);
        match paused_at(until, now_secs()) {
            false => None,
            true if until == PAUSED_INDEFINITELY => Some(None),
            true => Some(Some(until)),
        }
    }

    /// Pause sync until Unix second `until` (`None`: until resumed), or resume
    /// it when `paused` is false.
    pub(crate) fn set_sync_paused(
        &self,
        paused: bool,
        until: Option<i64>,
    ) -> Result<(), CoreError> {
        let value = match (paused, until) {
            (false, _) => 0,
            (true, None) => PAUSED_INDEFINITELY,
            (true, Some(until)) if until <= now_secs() => {
                return Err(CoreError::invalid("the pause would already be over"));
            }
            (true, Some(until)) => until,
        };
        let persisted = match value {
            0 => self.db.clear_state(PAUSED_UNTIL_KEY),
            value => self.db.set_state_i64(PAUSED_UNTIL_KEY, value),
        };
        if let Err(error) = persisted {
            return Err(CoreError::internal(format!(
                "saving the sync pause: {error}"
            )));
        }
        self.sync_paused_until.store(value, Ordering::Relaxed);
        match value {
            0 => {
                info!("sync resumed");
                self.resume_sync_work();
            }
            PAUSED_INDEFINITELY => info!("sync paused until resumed"),
            until => {
                info!(until, "sync paused");
                self.arm_pause_timer(until);
            }
        }
        Ok(())
    }

    /// Wake everything a pause held back: the drain worker, and a full
    /// reconcile to pick up whatever the watcher saw meanwhile.
    fn resume_sync_work(&self) {
        self.wake_drain();
        self.sync_now(None);
    }

    /// End a timed pause when its time comes. A later pause or resume replaces
    /// the stored deadline, which the timer checks before acting, so a stale
    /// timer does nothing.
    pub(crate) fn arm_pause_timer(&self, until: i64) {
        let core = self.clone();
        let wait = Duration::from_secs(until.saturating_sub(now_secs()).max(0) as u64);
        let spawned = std::thread::Builder::new()
            .name("pdfs-pause".into())
            .spawn(move || {
                if !core.shutdown.sleep(wait) {
                    return;
                }
                if core
                    .sync_paused_until
                    .compare_exchange(until, 0, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    if let Err(error) = core.db.clear_state(PAUSED_UNTIL_KEY) {
                        warn!(%error, "clearing an expired sync pause failed");
                    }
                    info!("sync pause ended");
                    core.resume_sync_work();
                }
            });
        if let Err(error) = spawned {
            // The drain and the reconcile still read the clock, so the pause
            // still ends — just at the next idle poll instead of on time.
            warn!(%error, "starting the sync pause timer failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_is_in_force_until_its_deadline() {
        assert!(!paused_at(0, 100));
        assert!(paused_at(200, 100));
        assert!(!paused_at(200, 200));
        assert!(!paused_at(200, 300));
        assert!(paused_at(PAUSED_INDEFINITELY, i64::MAX - 1));
    }
}
