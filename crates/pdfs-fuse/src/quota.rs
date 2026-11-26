//! The account's storage quota, as `statfs(2)` and the control socket ask
//! for it.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use parking_lot::Mutex;
use pdfs_core::{CoreError, CoreResult};
use tracing::debug;

use super::{Core, QUOTA_TTL};

/// The last account quota answer, for `statfs(2)`, and whether a refresh
/// of it is running.
#[derive(Clone)]
pub(crate) struct QuotaCache {
    /// Last account quota answer and when it was learned, for `statfs(2)`.
    ///
    /// `df` and the free-space preflight of every file manager and installer ask
    /// for this, and the answer costs a round trip — so it is served from here
    /// for [`QUOTA_TTL`] and refreshed on a worker afterwards. Total account
    /// storage moves slowly enough that a minute-old number is a better answer
    /// than either a stall or the zeroes the default implementation returns.
    last: Arc<Mutex<Option<(std::time::Instant, i64, i64)>>>,
    /// Set while a `statfs` is refreshing [`QuotaCache::last`], so the others serve
    /// the last figures instead of each holding a worker on the same request.
    refreshing: Arc<AtomicBool>,
}

impl QuotaCache {
    pub(crate) fn new() -> Self {
        Self {
            last: Arc::new(Mutex::new(None)),
            refreshing: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Core {
    /// Total account storage usage `(max_space, used_space)` in bytes, across all
    /// Proton products (not Drive-only). A remote round-trip; nothing here is
    /// cached.
    pub(crate) fn account_quota(&self) -> CoreResult<(i64, i64)> {
        let q = self.block_on_bounded(self.drive.quota()).map_err(|e| {
            self.lost_link(&e, "account quota");
            CoreError::from_api(&e, "account quota")
        })?;
        *self.quota.last.lock() = Some((std::time::Instant::now(), q.max_space, q.used_space));
        Ok((q.max_space, q.used_space))
    }

    /// The account quota as `statfs(2)` needs it: cached for [`QUOTA_TTL`],
    /// refetched after that, and `None` when there is no usable answer at all —
    /// offline with nothing cached.
    ///
    /// Never blocks on the network for a *fresh* value when a recent one exists,
    /// because the callers are `df` and every "is there room?" preflight, and a
    /// stale byte count is worth more to them than a stalled syscall. Offline,
    /// the last known figures keep being served rather than degrading to zero.
    pub(crate) fn account_quota_cached(&self) -> Option<(i64, i64)> {
        if let Some((at, max_space, used_space)) = *self.quota.last.lock()
            && at.elapsed() < QUOTA_TTL
        {
            return Some((max_space, used_space));
        }
        let last = || self.quota.last.lock().map(|(_, max, used)| (max, used));
        if !self.is_online()
            || self
                .quota
                .refreshing
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return last();
        }
        let refreshed = self.account_quota();
        self.quota
            .refreshing
            .store(false, std::sync::atomic::Ordering::Release);
        match refreshed {
            Ok(q) => Some(q),
            Err(error) => {
                debug!(%error, "account quota refresh failed; serving the last known figures");
                last()
            }
        }
    }
}
