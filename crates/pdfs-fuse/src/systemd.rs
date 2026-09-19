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
    if notify("READY=1\n") {
        debug!("notified systemd: ready");
    }
}

/// Tell the manager the daemon is still healthy.
pub(crate) fn watchdog_ping() {
    let _ = notify("WATCHDOG=1\n");
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
