//! The thread that watches the daemon for the thing the daemon cannot report
//! about itself: that it has stopped making progress.
//!
//! Three jobs, all cheap, all on one timer:
//!
//! 1. **Say so in the journal.** A worker that has held one job for minutes, a
//!    queue that is not draining, a control request that never returned — each
//!    gets a WARN naming it. The 2026-09-18 hang left nothing in the journal at
//!    all; this is what turns the next one into a one-line diagnosis.
//! 2. **Ping the systemd watchdog**, but only after a real round trip over the
//!    control socket. Pinging unconditionally from a thread that does nothing
//!    else would keep the unit "healthy" through exactly the hang it is meant to
//!    catch: the probe is what makes the ping mean "a client can still talk to
//!    this daemon".
//! 3. **Sample memory.** That hang peaked at 5.2 GiB resident, and nobody knew
//!    until it was over.

use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pdfs_core::control::Request as CtlRequest;
use tracing::{info, warn};

use super::Core;
use super::systemd;

/// How often the supervisor looks. Fast enough that a stall is named while the
/// user is still watching it, slow enough to cost nothing.
const TICK: Duration = Duration::from_secs(15);

/// How long one job may hold a worker, or one control request may run, before it
/// is called out. Generous: a large upload chunk or a cold folder listing on a
/// slow link is minutes of legitimate work, and a false WARN every few minutes
/// would train everyone to ignore the real one.
const STALL_AFTER: Duration = Duration::from_secs(120);

/// How long a sync pass may go without reporting progress before it is called
/// out. Much longer than [`STALL_AFTER`]: one large file in a pass reports
/// nothing until its whole transfer is done.
const SYNC_STALL_AFTER: Duration = Duration::from_secs(900);

/// How often resident size goes into the log.
const RSS_EVERY: Duration = Duration::from_secs(300);

/// How long the liveness probe waits for the daemon to answer its own control
/// socket. Well under the watchdog period, so a probe that times out still
/// leaves time for the manager to hear nothing and act.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run until shutdown. Spawned once per mount, from [`super::mount::mount`].
pub(crate) fn run(core: Core, control_socket: PathBuf) {
    let watchdog = systemd::watchdog_period();
    if let Some(period) = watchdog {
        info!(period_secs = period.as_secs(), "systemd watchdog enabled");
    }

    let mut last_rss = Instant::now();
    let mut last_ping = Instant::now();
    // Per-lane completion counts from the previous tick, to tell a queue that is
    // being worked through from one that is not moving at all.
    let mut last_completed = (0_u64, 0_u64);

    while core.shutdown.sleep(TICK) {
        let report = core.diagnostics();
        let stalled = report_stalls(&report, last_completed);
        last_completed = (report.meta_completed, report.transfer_completed);

        if last_rss.elapsed() >= RSS_EVERY {
            info!(
                rss_bytes = report.rss_bytes,
                pending_ops = report.pending_ops,
                meta_queued = report.meta_queued,
                transfer_queued = report.transfer_queued,
                stalled,
                "daemon resource sample"
            );
            last_rss = Instant::now();
        }

        // A stalled lane is deliberately *not* a reason to withhold the ping:
        // the mount may still be answering everything else, and a restart would
        // throw away in-flight uploads. The probe decides that; a stall only
        // gets said out loud.
        if let Some(period) = watchdog
            && last_ping.elapsed() >= period
        {
            if probe(&control_socket) {
                systemd::watchdog_ping();
            } else {
                warn!(
                    "daemon did not answer its own control socket; withholding the watchdog ping"
                );
            }
            last_ping = Instant::now();
        }
    }
}

/// WARN about anything that has not moved for [`STALL_AFTER`]. Returns whether
/// anything was reported, so the caller can weigh it.
fn report_stalls(report: &pdfs_core::control::Diagnostics, last_completed: (u64, u64)) -> bool {
    let threshold = STALL_AFTER.as_secs();
    let mut stalled = false;

    for worker in &report.workers {
        if worker.busy && worker.age_secs >= threshold {
            stalled = true;
            warn!(
                worker = %worker.name,
                job = %worker.label,
                age_secs = worker.age_secs,
                "a fuse worker has held the same job for a long time"
            );
        }
    }
    if let Some(oldest) = report.inflight.first()
        && oldest.age_secs >= threshold
    {
        stalled = true;
        warn!(
            request = %oldest.kind,
            age_secs = oldest.age_secs,
            in_flight = report.inflight.len(),
            "a control request has been running for a long time"
        );
    }
    // A backed-up lane is normal; a backed-up lane that finished nothing since
    // the last tick is not.
    if report.meta_queued > 0 && report.meta_completed == last_completed.0 {
        stalled = true;
        warn!(
            queued = report.meta_queued,
            "the metadata lane has not completed a job since the last check"
        );
    }
    if report.transfer_queued > 0 && report.transfer_completed == last_completed.1 {
        stalled = true;
        warn!(
            queued = report.transfer_queued,
            "the transfer lane has not completed a job since the last check"
        );
    }
    // The engine is one thread for every synced folder, so a dead or stuck one
    // stops all of them while the worker pool looks idle (B259).
    if let Some(sync) = &report.sync {
        if !sync.running {
            stalled = true;
            warn!(
                "the sync engine is not running; synced folders do not sync until the daemon restarts"
            );
        } else if let Some(folder) = sync.pass_folder
            && sync.pass_idle_secs >= SYNC_STALL_AFTER.as_secs()
        {
            stalled = true;
            warn!(
                folder,
                age_secs = sync.pass_age_secs,
                idle_secs = sync.pass_idle_secs,
                "a sync pass has not reported progress for a long time"
            );
        }
    }
    stalled
}

/// One round trip over the control socket: connect, ask for the cheapest
/// request there is, read the answer.
///
/// This is the liveness the watchdog ping stands for. It covers the accept loop,
/// the handler pool and the reply path — everything a `pdfs` command or the GUI
/// depends on — without touching the network or the database.
fn probe(control_socket: &Path) -> bool {
    let Ok(stream) = UnixStream::connect(control_socket) else {
        return false;
    };
    if stream.set_read_timeout(Some(PROBE_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(PROBE_TIMEOUT)).is_err()
    {
        return false;
    }
    let Ok(request) = serde_json::to_string(&CtlRequest::Diagnostics) else {
        return false;
    };
    let mut stream = stream;
    if stream.write_all(format!("{request}\n").as_bytes()).is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).is_ok() && !line.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfs_core::control::{Diagnostics, InflightRequest, SyncEngineState, WorkerState};

    fn busy_worker(age_secs: u64) -> WorkerState {
        WorkerState {
            name: "pdfs-fuse-3".into(),
            busy: true,
            label: "read".into(),
            age_secs,
        }
    }

    /// A worker doing normal work must not be reported: a WARN every tick is a
    /// WARN nobody reads.
    #[test]
    fn ordinary_work_is_not_a_stall() {
        let report = Diagnostics {
            workers: vec![busy_worker(5)],
            ..Default::default()
        };
        assert!(!report_stalls(&report, (0, 0)));
    }

    /// A worker on the same job past the threshold is the shape of the hang.
    #[test]
    fn a_long_held_job_is_a_stall() {
        let report = Diagnostics {
            workers: vec![busy_worker(STALL_AFTER.as_secs() + 1)],
            ..Default::default()
        };
        assert!(report_stalls(&report, (0, 0)));
    }

    /// So is a control request that never returned.
    #[test]
    fn a_long_running_request_is_a_stall() {
        let report = Diagnostics {
            inflight: vec![InflightRequest {
                kind: "OpenFile".into(),
                age_secs: STALL_AFTER.as_secs() + 1,
            }],
            ..Default::default()
        };
        assert!(report_stalls(&report, (0, 0)));
    }

    /// A queue that is draining is busy, not stalled — that is what comparing
    /// the completion counts is for.
    #[test]
    fn a_draining_queue_is_not_a_stall() {
        let report = Diagnostics {
            transfer_queued: 12,
            transfer_completed: 40,
            ..Default::default()
        };
        assert!(!report_stalls(&report, (0, 30)));
        assert!(report_stalls(&report, (0, 40)));
    }

    fn with_sync(sync: SyncEngineState) -> Diagnostics {
        Diagnostics {
            sync: Some(sync),
            ..Default::default()
        }
    }

    /// A sync engine that has ended stops every synced folder while the
    /// workers look idle, which is what issue #53 looked like (B259).
    #[test]
    fn a_stopped_sync_engine_is_a_stall() {
        assert!(report_stalls(
            &with_sync(SyncEngineState::default()),
            (0, 0)
        ));
    }

    /// A pass that keeps reporting progress is working, however long it runs;
    /// one that has gone quiet past the threshold is not.
    #[test]
    fn only_a_quiet_sync_pass_is_a_stall() {
        let busy = SyncEngineState {
            running: true,
            pass_folder: Some(3),
            pass_age_secs: SYNC_STALL_AFTER.as_secs() * 4,
            pass_idle_secs: 5,
            panics: 0,
        };
        assert!(!report_stalls(&with_sync(busy.clone()), (0, 0)));
        let quiet = SyncEngineState {
            pass_idle_secs: SYNC_STALL_AFTER.as_secs(),
            ..busy
        };
        assert!(report_stalls(&with_sync(quiet), (0, 0)));
        let idle = SyncEngineState {
            running: true,
            ..Default::default()
        };
        assert!(!report_stalls(&with_sync(idle), (0, 0)));
    }
}
