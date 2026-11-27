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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pdfs_core::control::{Diagnostics, DriveCallStat, InflightRequest, ServiceState, WorkerState};

use pdfs_core::lifecycle::ServiceBoard;

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
    let name = name.unwrap_or("unknown");
    // A bare `Refresh` says nothing about which view wedged, and the three
    // scopes cost wildly different amounts of work: a directory refresh is two
    // cheap calls, a photos refresh can pull a whole library. The scope is one
    // more peek into the same line.
    if name == "Refresh"
        && let Some(scope) = tagged_name_after(line, "\"scope\"")
    {
        return format!("Refresh({scope})");
    }
    name.to_string()
}

/// The variant name serde wrote for the value of `key`, whether that value is a
/// bare string (a unit variant) or an object whose single key is the name.
fn tagged_name_after(line: &str, key: &str) -> Option<String> {
    let rest = line.split_once(key)?.1.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('{').map_or(rest, str::trim_start);
    rest.strip_prefix('"')?
        .split('"')
        .next()
        .map(str::to_string)
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
            sync: Some(self.sync.engine.report()),
            services: self
                .services
                .health()
                .into_iter()
                .map(|service| ServiceState {
                    name: service.name,
                    running: service.running,
                    age_secs: service.age.as_secs(),
                })
                .collect(),
            jobs: self
                .services
                .jobs()
                .into_iter()
                .map(|job| ServiceState {
                    name: job.name,
                    running: job.running,
                    age_secs: job.age.as_secs(),
                })
                .collect(),
            drive_calls: self
                .drive_calls
                .snapshot()
                .into_iter()
                .map(|stat| DriveCallStat {
                    call: stat.call.name().to_owned(),
                    count: stat.count,
                    errors: stat.errors,
                    total_ms: u64::try_from(stat.total.as_millis()).unwrap_or(u64::MAX),
                    longest_ms: u64::try_from(stat.longest.as_millis()).unwrap_or(u64::MAX),
                })
                .collect(),
        }
    }

    /// Queued mutations, or `None` when the database did not answer within
    /// [`PENDING_COUNT_DEADLINE`] or an earlier probe is still waiting on it.
    ///
    /// The supervisor asks for a report every tick. One thread per report
    /// would pile up behind a database that never answers, so a second probe
    /// is not started until the first one has returned.
    fn pending_ops_quickly(&self) -> Option<u64> {
        let db = self.db.clone();
        probe_once(
            &self.services,
            &self.pending_probe,
            PENDING_COUNT_DEADLINE,
            move || {
                let counts = db.pending_op_counts().ok()?;
                Some(counts.uploads.max(0) as u64 + counts.changes.max(0) as u64)
            },
        )
    }
}

/// Run `probe` on a thread of its own and wait up to `deadline` for it.
///
/// `None` when it did not answer in time, or when the previous probe sharing
/// `busy` has not returned yet: then no new thread is started at all.
fn probe_once<T: Send + 'static>(
    board: &ServiceBoard,
    busy: &Arc<AtomicBool>,
    deadline: Duration,
    probe: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    if busy.swap(true, Ordering::AcqRel) {
        return None;
    }
    let done = Arc::clone(busy);
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = board.spawn_job("pdfs-diagnostics", move || {
        let answer = probe();
        done.store(false, Ordering::Release);
        let _ = tx.send(answer);
    });
    if spawned.is_err() {
        busy.store(false, Ordering::Release);
        return None;
    }
    rx.recv_timeout(deadline).ok().flatten()
}

/// How long the report waits for the database before reporting without it.
const PENDING_COUNT_DEADLINE: Duration = Duration::from_millis(500);

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

    /// A stuck `Refresh` is only actionable if the report says which view it
    /// was refreshing, so the scope rides along with the name.
    #[test]
    fn a_refresh_reports_its_scope() {
        assert_eq!(
            request_kind("{\"Refresh\":{\"scope\":{\"Photos\":{\"full\":true}}}}"),
            "Refresh(Photos)"
        );
        assert_eq!(
            request_kind("{\"Refresh\":{\"scope\":{\"Dir\":{\"path\":\"a\"}}}}"),
            "Refresh(Dir)"
        );
        assert_eq!(
            request_kind("{\"Refresh\":{\"scope\":\"Trash\"}}"),
            "Refresh(Trash)"
        );
        // A shape this does not recognise still names the request.
        assert_eq!(request_kind("{\"Refresh\":{}}"), "Refresh");
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

    #[test]
    fn a_stuck_probe_is_not_started_again() {
        let board = ServiceBoard::default();
        let busy = Arc::new(AtomicBool::new(false));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let short = Duration::from_millis(50);

        let stuck = probe_once(&board, &busy, short, move || {
            let _ = wait.recv();
            Some(1)
        });
        assert_eq!(stuck, None, "a probe that does not answer reports nothing");

        let started = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&started);
        let second = probe_once(&board, &busy, short, move || {
            flag.store(true, Ordering::SeqCst);
            Some(2)
        });
        assert_eq!(second, None);
        assert!(
            !started.load(Ordering::SeqCst),
            "no second thread while the first is out"
        );

        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while busy.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the first probe never returned");
            std::thread::yield_now();
        }
        assert_eq!(
            probe_once(&board, &busy, Duration::from_secs(10), || Some(3)),
            Some(3)
        );
    }
}
