//! How the SDK sent each upload's content, counted from its telemetry.
//!
//! A file whose encrypted size is under 128 KiB goes up in one request
//! (`small`). Any other file goes up as a draft, its blocks and a commit
//! (`blocks`). The SDK picks the path inside each upload call and reports it
//! only as the `backend` attribute of the `upload_file` and
//! `upload_new_revision` timers, so this module counts those events. If the
//! share of small uploads drops to zero for small files, the one-request path
//! is off or the server refuses it, and `pdfs diagnostics` shows that.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proton_sdk::telemetry::{Outcome, Telemetry, TelemetryEvent, TracingTelemetry};

/// The counts of every upload of this process, by path.
pub static UPLOAD_PATHS: UploadPaths = UploadPaths::new();

/// Upload counts for each path.
#[derive(Debug)]
pub struct UploadPaths {
    small: PathStat,
    blocks: PathStat,
}

#[derive(Debug)]
struct PathStat {
    count: AtomicU64,
    errors: AtomicU64,
    total_ms: AtomicU64,
    longest_ms: AtomicU64,
}

/// One path's counts at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPathStat {
    /// `small` or `blocks`.
    pub path: &'static str,
    pub count: u64,
    pub errors: u64,
    pub total_ms: u64,
    pub longest_ms: u64,
}

impl PathStat {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            total_ms: AtomicU64::new(0),
            longest_ms: AtomicU64::new(0),
        }
    }

    fn add(&self, failed: bool, took: Duration) {
        let ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        self.count.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.total_ms.fetch_add(ms, Ordering::Relaxed);
        self.longest_ms.fetch_max(ms, Ordering::Relaxed);
    }

    fn snapshot(&self, path: &'static str) -> UploadPathStat {
        UploadPathStat {
            path,
            count: self.count.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            total_ms: self.total_ms.load(Ordering::Relaxed),
            longest_ms: self.longest_ms.load(Ordering::Relaxed),
        }
    }
}

impl Default for UploadPaths {
    fn default() -> Self {
        Self::new()
    }
}

impl UploadPaths {
    pub const fn new() -> Self {
        Self {
            small: PathStat::new(),
            blocks: PathStat::new(),
        }
    }

    /// Count `event` if it is the end of an upload.
    pub fn record(&self, event: &TelemetryEvent) {
        if !matches!(event.operation, "upload_file" | "upload_new_revision") {
            return;
        }
        let backend = event
            .attributes
            .iter()
            .find_map(|(key, value)| (*key == "backend").then_some(value.as_str()));
        let stat = match backend {
            Some("small") => &self.small,
            Some("blocks") => &self.blocks,
            _ => return,
        };
        stat.add(event.outcome == Outcome::Failure, event.duration);
    }

    /// The counts so far, small first.
    pub fn snapshot(&self) -> [UploadPathStat; 2] {
        [self.small.snapshot("small"), self.blocks.snapshot("blocks")]
    }
}

/// The SDK's telemetry sink for this process: every event goes to `tracing`
/// as before, and uploads are also counted in [`UPLOAD_PATHS`].
pub fn telemetry() -> Arc<dyn Telemetry> {
    Arc::new(Counting {
        inner: TracingTelemetry::shared(),
    })
}

struct Counting {
    inner: Arc<dyn Telemetry>,
}

impl Telemetry for Counting {
    fn record(&self, event: &TelemetryEvent) {
        UPLOAD_PATHS.record(event);
        self.inner.record(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(operation: &'static str, backend: &str, outcome: Outcome, ms: u64) -> TelemetryEvent {
        TelemetryEvent {
            operation,
            outcome,
            duration: Duration::from_millis(ms),
            attributes: vec![
                ("backend", backend.to_owned()),
                ("aead", "false".to_owned()),
            ],
        }
    }

    #[test]
    fn uploads_are_counted_by_the_path_the_sdk_took() {
        let paths = UploadPaths::new();
        paths.record(&event("upload_file", "small", Outcome::Success, 300));
        paths.record(&event(
            "upload_new_revision",
            "small",
            Outcome::Failure,
            500,
        ));
        paths.record(&event("upload_file", "blocks", Outcome::Success, 2_000));

        let [small, blocks] = paths.snapshot();
        assert_eq!(
            small,
            UploadPathStat {
                path: "small",
                count: 2,
                errors: 1,
                total_ms: 800,
                longest_ms: 500,
            }
        );
        assert_eq!(
            (blocks.count, blocks.errors, blocks.longest_ms),
            (1, 0, 2_000)
        );
    }

    #[test]
    fn other_operations_and_events_without_a_backend_are_not_counted() {
        let paths = UploadPaths::new();
        paths.record(&event("http_request", "small", Outcome::Success, 10));
        paths.record(&TelemetryEvent {
            operation: "upload_file",
            outcome: Outcome::Success,
            duration: Duration::from_millis(10),
            attributes: Vec::new(),
        });

        assert!(paths.snapshot().iter().all(|stat| stat.count == 0));
    }
}
