//! The user's view of the pending-op queue: what has not reached the remote
//! yet, why, and a way to stop waiting out a backoff.
//!
//! Read-only apart from "retry now", which only moves a row's next-attempt time.
//! Nothing here drops a queued op: its staged blob is the only copy of the
//! user's write (see `docs/RECOVERY.md`).

use super::*;
use pdfs_core::control::PendingOpInfo;
use pdfs_core::db::{FAILING_ATTEMPTS, PARK_UNTIL};

impl Core {
    /// Every queued op, oldest first, with a path a person can read.
    pub(crate) fn pending_op_infos(&self) -> CoreResult<Vec<PendingOpInfo>> {
        let ops = self
            .db
            .pending_ops()
            .map_err(|e| CoreError::internal(format!("listing the queue: {e}")))?;
        Ok(ops
            .into_iter()
            .map(|op| {
                let parked = op.next_attempt_at >= PARK_UNTIL;
                let path = self
                    .db
                    .node_path(&op.uid)
                    .ok()
                    .flatten()
                    .filter(|path| !path.is_empty())
                    .or_else(|| op.name.clone())
                    .unwrap_or_else(|| op.uid.clone());
                PendingOpInfo {
                    id: op.id,
                    kind: op.kind,
                    path,
                    attempts: op.attempts,
                    last_error: op.last_error,
                    queued_at: op.created_at / 1000,
                    next_attempt_at: (!parked).then_some(op.next_attempt_at / 1000),
                    parked,
                    failing: op.attempts >= FAILING_ATTEMPTS,
                }
            })
            .collect())
    }

    /// Retry one backed-off op now, or every failed one when `id` is `None`.
    /// Returns how many ops were moved up.
    pub(crate) fn retry_pending_ops(&self, id: Option<i64>) -> CoreResult<usize> {
        let now = now_millis();
        let moved = match id {
            Some(id) => self.db.retry_op_now(id, now).map(usize::from),
            None => self.db.retry_failed_ops_now(now),
        }
        .map_err(|e| CoreError::internal(format!("retrying the queue: {e}")))?;
        if moved > 0 {
            self.wake_drain();
        }
        Ok(moved)
    }
}
