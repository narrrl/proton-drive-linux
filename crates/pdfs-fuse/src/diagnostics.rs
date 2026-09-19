//! What the daemon is doing right now, for the case where it has stopped doing
//! anything at all.
//!
//! On 2026-09-18 the daemon hung: it consumed 43 minutes of CPU over 20 hours,
//! peaked at 5.2 GiB resident, went silent at 17:10, and only came back after a
//! manual `systemctl --user restart`. There was no panic, so nothing in the
//! journal said what it had been waiting on — and nothing in the daemon could be
//! asked, either. This module is the answer to "what was it stuck on?".
//!
//! Everything here is built to work *while* the daemon is wedged, which rules
//! out the obvious implementations: no lock that a stuck job might hold, no
//! database query on the calling thread, no waiting on the worker pool. A
//! diagnostic that blocks on the hang it is diagnosing reports nothing at
//! exactly the moment it matters.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use pdfs_core::control::{Diagnostics, InflightRequest, WorkerState};

use super::Core;

/// When this process started. Read at the first touch, which
/// [`super::mount::mount`] makes happen during startup so the uptime is the
/// daemon's, not the first diagnostics request's.
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Control requests currently being served, keyed by a monotonic ticket.
///
/// A `Vec` rather than a map because it holds at most `MAX_CONTROL_HANDLERS`
/// entries, and the only operations are push, remove-by-ticket and read-all.
static INFLIGHT: Mutex<Vec<(u64, String, Instant)>> = Mutex::new(Vec::new());
static NEXT_TICKET: AtomicU64 = AtomicU64::new(0);

/// Start the daemon's uptime clock. Called once, from the mount path.
pub(crate) fn start_clock() {
    LazyLock::force(&STARTED);
}

/// Registers one in-flight control request for as long as it is held.
pub(crate) struct InflightGuard(u64);

impl InflightGuard {
    pub(crate) fn new(kind: String) -> Self {
        let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
        INFLIGHT.lock().push((ticket, kind, Instant::now()));
        Self(ticket)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT.lock().retain(|(ticket, _, _)| *ticket != self.0);
    }
}

/// The variant name of a serialised [`pdfs_core::control::Request`], without
/// parsing it into one.
///
/// serde's external tagging makes this a property of the text: a unit variant is
/// the bare string `"Status"`, anything else is an object whose single key is
/// the variant name. Reading the name off the line rather than matching on 80
/// variants keeps this cheap, and keeps it correct when a variant is added.
pub(crate) fn request_kind(line: &str) -> String {
    let line = line.trim();
    let name = if let Some(rest) = line.strip_prefix('{') {
        rest.trim_start()
            .strip_prefix('"')
            .and_then(|rest| rest.split('"').next())
    } else {
        line.strip_prefix('"')
            .and_then(|rest| rest.split('"').next())
    };
    name.unwrap_or("unknown").to_string()
}

/// Resident set size in bytes, or `0` when `/proc` did not answer.
///
/// From `VmRSS` in `/proc/self/status`, which is already in kB — `statm` would
/// need the page size, and this needs no dependency at all.
pub(crate) fn rss_bytes() -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    parse_vm_rss(&status)
}

/// The `VmRSS` line of a `/proc/<pid>/status`, in bytes.
fn parse_vm_rss(status: &str) -> u64 {
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map_or(0, |kb| kb.saturating_mul(1024))
}

impl Core {
    /// Assemble a [`Diagnostics`] report.
    ///
    /// The pending-op count is the one field that needs the database, so it is
    /// read on a throwaway thread with a short deadline: a daemon wedged *on*
    /// the database must still be able to report everything else about itself.
    pub(crate) fn diagnostics(&self) -> Diagnostics {
        let pool = self.workers.snapshot();
        let now = Instant::now();
        let mut inflight: Vec<InflightRequest> = INFLIGHT
            .try_lock()
            .map(|entries| {
                entries
                    .iter()
                    .map(|(_, kind, started)| InflightRequest {
                        kind: kind.clone(),
                        age_secs: now.duration_since(*started).as_secs(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        inflight.sort_by_key(|request| std::cmp::Reverse(request.age_secs));

        Diagnostics {
            uptime_secs: STARTED.elapsed().as_secs(),
            rss_bytes: rss_bytes(),
            workers: pool
                .workers
                .into_iter()
                .map(|worker| WorkerState {
                    name: worker.name,
                    busy: worker.busy,
                    label: worker.label,
                    age_secs: worker.age_ms / 1000,
                })
                .collect(),
            meta_queued: pool.meta_queued,
            transfer_queued: pool.transfer_queued,
            meta_completed: pool.meta_completed,
            transfer_completed: pool.transfer_completed,
            inflight,
            control_handlers: super::control::active_handlers(),
            control_handler_limit: super::control::handler_limit(),
            pending_ops: self.pending_ops_quickly(),
        }
    }

    /// Queued mutations, or `0` if the database did not answer within
    /// [`PENDING_COUNT_DEADLINE`].
    fn pending_ops_quickly(&self) -> u64 {
        let db = self.db.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("pdfs-diagnostics".into())
            .spawn(move || {
                let counts = db.pending_op_counts().unwrap_or_default();
                let total = counts.uploads.max(0) as u64 + counts.changes.max(0) as u64;
                let _ = tx.send(total);
            })
            .is_err()
        {
            return 0;
        }
        rx.recv_timeout(PENDING_COUNT_DEADLINE).unwrap_or(0)
    }
}

/// How long the report waits for the database before reporting without it.
const PENDING_COUNT_DEADLINE: std::time::Duration = std::time::Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use super::*;

    /// The request name is read off the wire format, so it must match both
    /// shapes serde produces.
    #[test]
    fn the_request_kind_comes_off_both_wire_shapes() {
        assert_eq!(request_kind("\"Status\""), "Status");
        assert_eq!(request_kind("{\"Pin\":{\"path\":\"a\"}}"), "Pin");
        assert_eq!(request_kind(" { \"Restore\" : {} } "), "Restore");
        assert_eq!(request_kind("garbage"), "unknown");
    }

    /// A guard registers for its lifetime and nothing longer: a handler that
    /// returns must not leave a permanent "in flight" entry behind.
    #[test]
    fn a_guard_registers_only_while_it_lives() {
        let before = INFLIGHT.lock().len();
        {
            let _guard = InflightGuard::new("OpenFile".into());
            assert_eq!(INFLIGHT.lock().len(), before + 1);
        }
        assert_eq!(INFLIGHT.lock().len(), before);
    }

    /// Whatever else it reports, the resident size has to be a real reading.
    #[test]
    fn rss_is_readable() {
        assert!(rss_bytes() > 0);
    }

    /// `VmRSS` is in kB, and a status without one must read as unknown rather
    /// than as a daemon using no memory.
    #[test]
    fn vm_rss_is_kilobytes_and_absence_is_zero() {
        assert_eq!(
            parse_vm_rss("VmPeak:\t 100 kB\nVmRSS:\t  2048 kB\n"),
            2048 * 1024
        );
        assert_eq!(parse_vm_rss("VmPeak:\t 100 kB\n"), 0);
    }
}
