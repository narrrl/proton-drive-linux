//! Hold off a shutdown while queued changes exist only on this computer.
//!
//! A change made through the mount is staged here and sent by the drain. Until
//! it lands, this computer has the only copy, so powering off then risks it
//! more than the user expects (`docs/BUGS.md` B66). While the queue holds work
//! the drain can still send, the daemon takes a logind shutdown inhibitor
//! through `systemd-inhibit`, which desktops show in their power-off dialog
//! with the reason. It is let go as soon as the queue empties.
//!
//! Work that cannot drain on its own — a temporary file waiting for its final
//! name, or a change Drive refused or that keeps failing — never holds the
//! lock, or a shutdown would be held up for something that is not coming.
//! Without `systemd-inhibit` nothing happens.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use pdfs_core::shutdown::Shutdown;
use pdfs_store::db::{Db, PendingCounts};
use tracing::{debug, warn};

/// How often the queue is looked at.
const CHECK_EVERY: Duration = Duration::from_secs(5);

/// Changes the drain can still send by itself.
fn sendable(counts: &PendingCounts) -> i64 {
    (counts.uploads + counts.changes - counts.parked - counts.failing).max(0)
}

/// Run until the daemon stops, holding the inhibitor while changes wait.
pub(crate) fn run(db: &Db, shutdown: &Shutdown) {
    let mut held: Option<Child> = None;
    loop {
        let waiting = db
            .pending_op_counts()
            .map(|counts| sendable(&counts))
            .unwrap_or(0);
        // The helper can go away (logind restarted, or it was killed); the
        // lock is then taken again on the next round.
        if let Some(child) = &mut held
            && !matches!(child.try_wait(), Ok(None))
        {
            held = None;
        }
        match (&mut held, waiting > 0) {
            (None, true) => held = take(waiting),
            (Some(child), false) => {
                release(child);
                held = None;
            }
            _ => {}
        }
        if !shutdown.sleep(CHECK_EVERY) {
            break;
        }
    }
    if let Some(mut child) = held {
        release(&mut child);
    }
}

fn take(waiting: i64) -> Option<Child> {
    let why = if waiting == 1 {
        "Sending 1 change to Proton Drive".to_string()
    } else {
        format!("Sending {waiting} changes to Proton Drive")
    };
    match Command::new("systemd-inhibit")
        .args([
            "--what=shutdown",
            "--who=Proton Drive",
            &format!("--why={why}"),
            "--mode=block",
            "sleep",
            "infinity",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            debug!(waiting, "holding off shutdown while changes are queued");
            Some(child)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            warn!(%error, "taking the shutdown inhibitor failed");
            None
        }
    }
}

fn release(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
    debug!("queue drained; shutdown no longer held off");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(uploads: i64, changes: i64, parked: i64, failing: i64) -> PendingCounts {
        PendingCounts {
            uploads,
            changes,
            parked,
            failing,
            last_error: None,
        }
    }

    #[test]
    fn only_work_the_drain_can_send_holds_off_a_shutdown() {
        assert_eq!(sendable(&counts(0, 0, 0, 0)), 0);
        assert_eq!(sendable(&counts(2, 1, 0, 0)), 3);
        assert_eq!(sendable(&counts(1, 0, 1, 0)), 0);
        assert_eq!(sendable(&counts(2, 0, 0, 2)), 0);
        assert_eq!(sendable(&counts(3, 2, 1, 1)), 3);
    }

    #[test]
    fn counts_that_overlap_never_go_negative() {
        assert_eq!(sendable(&counts(1, 0, 1, 1)), 0);
    }
}
