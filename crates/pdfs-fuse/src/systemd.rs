//! The `sd_notify` half of the service contract, without linking libsystemd.
//!
//! The protocol is one datagram of `KEY=VALUE` lines to the socket in
//! `$NOTIFY_SOCKET`. That is little enough to write directly, and writing it
//! directly keeps the daemon buildable on a machine without systemd headers —
//! everything here degrades to "no socket, do nothing" off systemd.

use std::os::unix::net::UnixDatagram;
use std::time::Duration;

use tracing::debug;

/// Send one notification. `false` when there is no manager to send it to, or the
/// send failed — a caller has nothing useful to do about either.
pub(crate) fn notify(message: &str) -> bool {
    let Ok(socket_path) = std::env::var("NOTIFY_SOCKET") else {
        return false;
    };
    if socket_path.is_empty() {
        return false;
    }
    let Ok(socket) = UnixDatagram::unbound() else {
        return false;
    };
    if let Some(name) = socket_path.strip_prefix('@') {
        // Abstract namespace: the address is a NUL byte followed by the name,
        // which `std`'s path-based API cannot express.
        send_abstract(&socket, name, message)
    } else {
        socket.send_to(message.as_bytes(), &socket_path).is_ok()
    }
}

/// `sendto` on an abstract-namespace address, which needs a hand-built
/// `sockaddr_un` because the first byte of the name is NUL.
fn send_abstract(socket: &UnixDatagram, name: &str, message: &str) -> bool {
    use std::os::fd::AsRawFd;

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = name.as_bytes();
    // One byte for the leading NUL, and the path must still be addressable.
    if bytes.len() + 1 > addr.sun_path.len() {
        return false;
    }
    for (slot, byte) in addr.sun_path.iter_mut().skip(1).zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len();
    let sent = unsafe {
        libc::sendto(
            socket.as_raw_fd(),
            message.as_ptr().cast(),
            message.len(),
            0,
            std::ptr::from_ref(&addr).cast(),
            len as libc::socklen_t,
        )
    };
    sent >= 0
}

/// Tell the manager the daemon is up and serving. Called once the mount is live,
/// so `systemctl --user start` returns when the mount actually works rather than
/// when the process exists.
pub(crate) fn ready() {
    if notify("READY=1\nSTATUS=Mounted\n") {
        debug!("notified systemd: ready");
    }
}

/// Set the one-line status `systemctl --user status` shows for the unit, such
/// as why the mount keeps failing (`docs/BUGS.md` B263).
pub fn status(text: &str) {
    // A newline would end the `STATUS=` line and start another variable.
    let line = text.replace('\n', " ");
    notify(&format!("STATUS={line}\n"));
}

/// Tell the manager the daemon is still healthy.
pub(crate) fn watchdog_ping() {
    let _ = notify("WATCHDOG=1\n");
}

/// Sleep for `duration`, pinging the watchdog on the way.
///
/// For the stretches where the daemon is alive but has no mount, and so no
/// supervisor: waiting for a login, or backing off after a failed mount. The
/// manager arms the watchdog when the process starts, so without these pings a
/// daemon that is merely retrying is aborted after `WatchdogSec` and leaves a
/// coredump instead of the error it was logging. Never use this while mounted:
/// there the ping must prove the control socket answers.
pub fn idle_sleep(duration: Duration) {
    const CHUNK: Duration = Duration::from_secs(10);
    let mut left = duration;
    watchdog_ping();
    while !left.is_zero() {
        let step = left.min(CHUNK);
        std::thread::sleep(step);
        left -= step;
        watchdog_ping();
    }
}

/// Keep a cold daemon available for a service restart after key recovery.
pub fn wait_for_restart() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            let stop = async {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = interrupt.recv() => {}
                }
            };
            park_until(stop, Duration::from_secs(10), watchdog_ping).await;
            Ok(())
        })
}

/// Call `ping` every `every` until `stop` completes. The parked daemon's whole
/// loop, apart from which signals end it.
async fn park_until(
    stop: impl std::future::Future<Output = ()>,
    every: Duration,
    mut ping: impl FnMut(),
) {
    let mut tick = tokio::time::interval(every);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = tick.tick() => ping(),
        }
    }
}

/// How long a mount may take to start before the watchdog counts it as hung.
///
/// Startup has no control socket to probe yet, and on a large account it is
/// long: hydrating 48 000 nodes took 70 s on 2026-11-12, and the secondary
/// mounts followed. Under the 2-minute `WatchdogSec` that run was aborted just
/// before its first ping, and left six dead mounts behind (`docs/BUGS.md` B279).
const STARTUP_WATCHDOG: Duration = Duration::from_secs(600);

/// Give the mount about to start [`STARTUP_WATCHDOG`] before the watchdog fires.
///
/// Sends `WATCHDOG_USEC=`, which replaces the unit's timeout and restarts its
/// timer. The supervisor puts the configured timeout back with
/// [`restore_watchdog`] once it can prove the daemon answers. A startup that
/// wedges for good is still aborted, only later.
pub fn stretch_watchdog_for_startup() {
    if watchdog_period().is_some() {
        notify(&format!("WATCHDOG_USEC={}\n", STARTUP_WATCHDOG.as_micros()));
    }
}

/// Put back the timeout the unit configured, undoing
/// [`stretch_watchdog_for_startup`].
pub(crate) fn restore_watchdog() {
    if watchdog_period().is_none() {
        return;
    }
    if let Ok(usec) = std::env::var("WATCHDOG_USEC") {
        notify(&format!("WATCHDOG_USEC={usec}\n"));
    }
}

/// Tell the manager the daemon is stopping, so a slow teardown is not mistaken
/// for a hang by the watchdog it is racing.
pub(crate) fn stopping() {
    let _ = notify("STOPPING=1\n");
}

/// How often the manager expects a ping, from `$WATCHDOG_USEC`, or `None` when
/// the watchdog is off or belongs to another process.
///
/// Reports *half* the configured interval, the interval systemd documents for
/// pinging: a ping period equal to the deadline would restart the daemon on the
/// first late scheduling.
pub(crate) fn watchdog_period() -> Option<Duration> {
    // `WATCHDOG_PID` is set when the manager wants a specific process to answer;
    // in that case a ping from anyone else is not the one it is waiting for.
    if let Ok(pid) = std::env::var("WATCHDOG_PID")
        && pid.parse::<u32>().ok() != Some(std::process::id())
    {
        return None;
    }
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    (usec > 0).then(|| Duration::from_micros(usec / 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parked daemon keeps the watchdog fed and returns once stopped, so the
    /// restart after key recovery is a clean exit, not a kill.
    #[test]
    fn a_parked_daemon_pings_until_it_is_stopped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        let pings = std::cell::Cell::new(0);
        runtime.block_on(async {
            let stop = tokio::time::sleep(Duration::from_secs(35));
            park_until(stop, Duration::from_secs(10), || pings.set(pings.get() + 1)).await;
        });
        // Ticks at 0, 10, 20 and 30 seconds, then the stop at 35.
        assert_eq!(pings.get(), 4);
    }
}
