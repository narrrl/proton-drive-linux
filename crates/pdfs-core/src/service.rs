//! Thin wrappers over `systemctl --user` for the auto-mount daemon.
//!
//! The mount is owned by a systemd *user* service (`proton-drive.service`, see
//! `packaging/`). Front-ends never spawn `pdfs mount` directly anymore: logging
//! in enables+starts the unit, logging out disables+stops it, and the tray's
//! "Disconnect" just stops it. systemd then handles restart-on-failure and a
//! clean SIGTERM stop (which the daemon turns into a lazy unmount).
//!
//! Every call is best-effort: a failure is logged but not fatal, since the user
//! can always drive `systemctl --user` by hand.

use std::process::{Command, Stdio};

/// The systemd user unit that runs `pdfs daemon`.
pub const SERVICE: &str = "proton-drive.service";

/// Run one `systemctl --user <args> proton-drive.service`, returning whether it
/// succeeded. Output is suppressed; failures are logged.
fn systemctl(args: &[&str]) -> bool {
    let ok = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .arg(SERVICE)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        tracing::warn!(args = ?args, "systemctl --user {SERVICE} failed");
    }
    ok
}

/// Like [`systemctl`] but treats a non-zero exit as a plain `false` answer
/// rather than a failure to log. For query verbs (`is-active`, `is-failed`)
/// where "no" is the normal, expected result.
fn systemctl_query(args: &[&str]) -> bool {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .arg(SERVICE)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Enable the service so it survives reboots/logins and start it now. Called on
/// successful login.
pub fn enable_start() -> bool {
    systemctl(&["enable", "--now"])
}

/// Stop the running mount without disabling it. Called by the tray's
/// "Disconnect": the next login (or reboot) brings it back.
pub fn stop() -> bool {
    systemctl(&["stop"])
}

/// Restart the unit now. Used by a front-end "Retry" action when the mount has
/// failed and the user wants to bring it back without re-logging-in.
pub fn restart() -> bool {
    systemctl(&["restart"])
}

/// Whether the unit currently reports `active` (running, or starting up).
/// `systemctl is-active` exits 0 only while the unit is active, so a front-end
/// can use this to tell "service still coming up" from "service is down".
pub fn is_active() -> bool {
    systemctl_query(&["is-active"])
}

/// Whether the unit is in the `failed` state. `systemctl is-failed` exits 0
/// only when the unit has failed, letting a front-end show an actionable error
/// (with Retry) rather than an indefinite "connecting…" spinner.
pub fn is_failed() -> bool {
    systemctl_query(&["is-failed"])
}

/// Disable the service so it no longer auto-starts, and stop it now. Called on
/// logout.
pub fn disable_stop() -> bool {
    systemctl(&["disable", "--now"])
}
