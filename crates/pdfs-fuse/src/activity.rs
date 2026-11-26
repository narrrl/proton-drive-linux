//! The activity log the front-ends show.

use super::*;

impl Core {
    /// Append one entry to the activity log. Callable from any thread (the sync
    /// engine and the bulk uploader both log from background tasks). A failed
    /// write is logged and dropped: the feed is a record of work, never a reason
    /// to fail the work itself.
    pub(crate) fn log_activity(
        &self,
        kind: ActivityKind,
        target: impl Into<String>,
        // `Display` rather than `Into<String>` so a classified `CoreError` can be
        // logged as-is, without the caller flattening it first.
        detail: impl std::fmt::Display,
        ok: bool,
    ) {
        let entry = ActivityEntry {
            time: now_secs(),
            kind,
            target: target.into(),
            detail: detail.to_string(),
            ok,
        };
        if let Err(e) = self.db.activity_add(&entry) {
            warn!(error = ?e, "could not record activity");
            return;
        }
        // A sync pass is how a conflict copy reaches the listing, and the sweep
        // reports the ones it finds as conflicts.
        if matches!(entry.kind, ActivityKind::Conflict | ActivityKind::Sync) {
            self.events.publish(&[Topic::Activity, Topic::Conflicts]);
        } else {
            self.events.publish(&[Topic::Activity]);
        }
    }

    /// The recent activity, newest first, capped at `limit` entries.
    pub(super) fn list_activity(&self, limit: usize) -> Vec<ActivityEntry> {
        match self.db.activity_list(limit) {
            Ok(items) => items,
            Err(e) => {
                warn!(error = ?e, "could not read activity");
                Vec::new()
            }
        }
    }
}
