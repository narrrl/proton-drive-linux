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
//!
//! A single mirror folder can be paused on its own as well: its reconcile is
//! skipped, and with it any mode switch queued behind that reconcile, until the
//! folder is resumed. On-demand folders cannot be paused alone — their writes
//! go through the shared queue, which only the global pause holds back.

use super::*;

/// The `state` key holding the resume time, in Unix seconds.
const PAUSED_UNTIL_KEY: &str = "sync_paused_until";

/// The `state` key prefix marking one synced folder paused; its id follows.
const FOLDER_PAUSED_PREFIX: &str = "sync_folder_paused:";

fn folder_paused_key(id: i64) -> String {
    format!("{FOLDER_PAUSED_PREFIX}{id}")
}

/// Whether synced folder `id` is paused on its own. A row that cannot be read
/// counts as running, like the global pause.
fn load_folder_paused(db: &Db, id: i64) -> bool {
    match db.state_i64(&folder_paused_key(id)) {
        Ok(value) => value.is_some_and(|v| v != 0),
        Err(error) => {
            warn!(id, %error, "reading a folder pause failed; treating it as running");
            false
        }
    }
}

/// Persist synced folder `id`'s own pause, or clear it.
fn store_folder_paused(db: &Db, id: i64, paused: bool) -> pdfs_core::Result<()> {
    match paused {
        true => db.set_state_i64(&folder_paused_key(id), 1),
        false => db.clear_state(&folder_paused_key(id)),
    }
}

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

    /// True while the user has this one synced folder paused. A row that cannot
    /// be read counts as running, like the global pause.
    pub(crate) fn sync_folder_paused(&self, id: i64) -> bool {
        load_folder_paused(&self.db, id)
    }

    /// Pause or resume one mirror folder's reconcile. Resuming reconciles it
    /// straight away to catch up on what the watcher saw meanwhile.
    pub(crate) fn set_sync_folder_paused(&self, id: i64, paused: bool) -> Result<(), CoreError> {
        let folder = self
            .db
            .sync_folder_get(id)
            .map_err(|e| CoreError::internal(format!("db: {e:?}")))?
            .ok_or_else(|| CoreError::not_found(format!("no synced folder with id {id}")))?;
        if paused && folder.mode != "mirror" {
            return Err(CoreError::invalid(
                "only a mirrored folder can be paused on its own; an on-demand folder uploads \
                 through the shared queue, so pause all sync instead",
            ));
        }
        if let Err(error) = store_folder_paused(&self.db, id, paused) {
            return Err(CoreError::internal(format!(
                "saving the folder pause: {error}"
            )));
        }
        if paused {
            info!(id, path = %folder.local_path, "folder sync paused");
        } else {
            info!(id, path = %folder.local_path, "folder sync resumed");
            self.sync_now(Some(id));
        }
        Ok(())
    }

    /// Forget a removed folder's pause, so a later folder reusing its id does
    /// not start out paused.
    pub(crate) fn clear_sync_folder_paused(&self, id: i64) {
        if let Err(error) = store_folder_paused(&self.db, id, false) {
            warn!(id, %error, "clearing a removed folder's pause failed");
        }
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

    #[test]
    fn a_folder_pause_is_kept_per_folder_until_cleared() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        assert!(!load_folder_paused(&db, 3));
        store_folder_paused(&db, 3, true).unwrap();
        assert!(load_folder_paused(&db, 3));
        assert!(!load_folder_paused(&db, 4));
        assert!(!load_folder_paused(&db, 33));
        store_folder_paused(&db, 3, false).unwrap();
        assert!(!load_folder_paused(&db, 3));
    }
}
