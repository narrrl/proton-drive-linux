//! `pdfs-tray` — system-tray front-end for the Proton Drive Linux client.
//!
//! A StatusNotifierItem (SNI) tray icon whose icon and tooltip say what the
//! drive is doing (synced, syncing, paused, offline, needs attention), with a
//! menu to open the app or the folder, pause sync, connect, hide the icon and
//! stop Proton Drive. It talks to the running mount daemon over the same
//! control socket the CLI uses, so the tray is a thin presentation layer over
//! [`pdfs_core::control`]. Anything that needs a dialog opens `pdfs-app`.
//!
//! GTK4 has no tray widget (GNOME dropped the systray), so the tray itself is an
//! SNI item via the `ksni` crate.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{MenuItem, Status, ToolTip, Tray};
use pdfs_core::auth;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{JobItem, Request, Response, TransferDirection, TransferItem, send};
use pdfs_core::service;

mod i18n;
use i18n::{gettext, gettext_f, ngettext_f};

/// How often the tray re-polls the daemon to refresh its menu.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// The one word the icon says about the drive, derived from each poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Mounted, online, nothing queued.
    Synced,
    /// Mounted with changes queued or bytes moving.
    Syncing,
    Paused,
    /// Mounted but the API is unreachable: cached files only.
    Offline,
    /// Changes keep failing; the user has to look.
    Attention,
    /// Signed in, but no mount daemon is running.
    Disconnected,
    SignedOut,
}

impl Phase {
    /// A stock symbolic icon for the phase. Stock names follow the panel's
    /// theme and colour, and need nothing installed beside the binary.
    fn icon(self) -> &'static str {
        match self {
            Phase::Synced => "folder-remote-symbolic",
            Phase::Syncing => "emblem-synchronizing-symbolic",
            Phase::Paused => "media-playback-pause-symbolic",
            Phase::Offline => "network-offline-symbolic",
            Phase::Attention => "dialog-warning-symbolic",
            Phase::Disconnected | Phase::SignedOut => "network-offline-symbolic",
        }
    }
}

/// Snapshot of what the tray knows about the daemon, recomputed each poll.
#[derive(Clone)]
struct DriveState {
    /// Human-readable first menu line ("Mounted at …", "Not running", …), also
    /// the tooltip.
    line: String,
    phase: Phase,
    /// Whether a mount daemon is currently serving the control socket.
    mounted: bool,
    /// Mountpoint to act on (from the daemon when mounted, else the default).
    mountpoint: PathBuf,
    /// One-line sync summary of in-flight transfers, empty when idle. Shown as a
    /// disabled menu line under the status line.
    sync: String,
    /// Whether the user has sync paused; decides Pause vs Resume.
    paused: bool,
    /// Queued changes that keep failing, for the "N issues" item.
    failing: u64,
}

/// Summarise a work snapshot into one menu line, or empty when idle. A single
/// transfer names the file and its percentage; several collapse to counts so the
/// menu stays one line regardless of queue depth. With nothing moving bytes, a
/// running job speaks for itself — a scan or an index rebuild is still "busy",
/// and the tray saying nothing there reads as "finished".
fn sync_line(items: &[TransferItem], jobs: &[JobItem]) -> String {
    match items {
        [] => match jobs.first() {
            Some(j) if j.total > 0 => {
                let (done, total) = (j.done.to_string(), j.total.to_string());
                // Translators: {title} names a running job such as a scan; {done} and {total} are counts.
                gettext_f(
                    "{title} ({done} of {total})",
                    &[("title", &j.title), ("done", &done), ("total", &total)],
                )
            }
            Some(j) => format!("{}…", j.title),
            None => String::new(),
        },
        [t] => {
            let arrow = match t.direction {
                TransferDirection::Download => "↓",
                TransferDirection::Upload => "↑",
            };
            if t.bytes_total == 0 {
                format!("{arrow} {}…", t.name)
            } else {
                let pct = (t.bytes_completed * 100 / t.bytes_total.max(1)).min(100);
                format!("{arrow} {} ({pct}%)", t.name)
            }
        }
        _ => {
            let down = items
                .iter()
                .filter(|t| t.direction == TransferDirection::Download)
                .count();
            let up = items.len() - down;
            match (down, up) {
                // Translators: {n} files are downloading; keep the arrow.
                (d, 0) => ngettext_f("↓ {n} downloading", "↓ {n} downloading", d as u64, &[]),
                // Translators: {n} files are uploading; keep the arrow.
                (0, u) => ngettext_f("↑ {n} uploading", "↑ {n} uploading", u as u64, &[]),
                (d, u) => format!("↓ {d} · ↑ {u}"),
            }
        }
    }
}

struct DriveTray {
    state: DriveState,
    socket: PathBuf,
}

/// Ask the daemon for its status, falling back to the stored session so the
/// menu can still say "Logged in …" / "Not logged in" when no mount is running.
fn poll_state(socket: &Path, default_mountpoint: &Path) -> DriveState {
    match send(socket, &Request::Status) {
        Ok(Response::Status {
            mountpoint,
            pinned,
            online,
            pending_uploads,
            pending_changes,
            failing_ops,
            paused,
            ..
        }) => {
            // Same daemon is up, so a cheap follow-up poll gives the sync line.
            let sync = match send(socket, &Request::GetQueueStatus) {
                Ok(Response::Transfers { items, jobs }) => sync_line(&items, &jobs),
                _ => String::new(),
            };
            let queued = i18n::pending_summary(pending_uploads, pending_changes);
            let phase = phase_of(
                paused,
                failing_ops,
                online,
                queued.is_some() || !sync.is_empty(),
            );
            DriveState {
                line: match (online, queued) {
                    _ if paused => gettext("Sync paused"),
                    _ if failing_ops > 0 => ngettext_f(
                        "{n} change needs attention",
                        "{n} changes need attention",
                        failing_ops,
                        &[],
                    ),
                    // Translators: {mountpoint} is a folder path; {n} is the number of pinned items.
                    (true, None) => ngettext_f(
                        "Mounted at {mountpoint} ({n} pinned)",
                        "Mounted at {mountpoint} ({n} pinned)",
                        pinned as u64,
                        &[("mountpoint", &mountpoint)],
                    ),
                    // Translators: {queued} is a queue summary such as "3 uploads queued"; {n} is the number of pinned items.
                    (true, Some(q)) => ngettext_f(
                        "Syncing — {queued} ({n} pinned)",
                        "Syncing — {queued} ({n} pinned)",
                        pinned as u64,
                        &[("queued", &q)],
                    ),
                    // Translators: {n} is the number of pinned items.
                    (false, None) => ngettext_f(
                        "Offline — cached files only ({n} pinned)",
                        "Offline — cached files only ({n} pinned)",
                        pinned as u64,
                        &[],
                    ),
                    // Translators: {queued} is a queue summary such as "3 uploads queued".
                    (false, Some(q)) => gettext_f("Offline — {queued}", &[("queued", &q)]),
                },
                phase,
                mounted: true,
                paused,
                mountpoint: PathBuf::from(mountpoint),
                sync,
                failing: failing_ops,
            }
        }
        // Socket answered but with something unexpected — treat as up but odd.
        Ok(_) => DriveState {
            line: gettext("Mount: unexpected daemon response"),
            phase: Phase::Attention,
            mounted: true,
            mountpoint: default_mountpoint.to_path_buf(),
            sync: String::new(),
            paused: false,
            failing: 0,
        },
        // No daemon: describe login state instead so the menu is still useful.
        Err(_) => {
            let (line, phase) = match auth::load() {
                Ok(s) => (
                    // Translators: {username} is the Proton account name.
                    gettext_f(
                        "Logged in as {username} — not mounted",
                        &[("username", &s.username)],
                    ),
                    Phase::Disconnected,
                ),
                Err(pdfs_core::Error::NotLoggedIn) => (gettext("Not signed in"), Phase::SignedOut),
                Err(e) => (
                    // Translators: {error} is an error message, in English.
                    gettext_f("Error: {error}", &[("error", &e.to_string())]),
                    Phase::Attention,
                ),
            };
            DriveState {
                line,
                phase,
                mounted: false,
                mountpoint: default_mountpoint.to_path_buf(),
                sync: String::new(),
                paused: false,
                failing: 0,
            }
        }
    }
}

/// The phase a running mount is in. A pause is the user's own doing and says
/// so first; failures outrank everything else the daemon reports.
fn phase_of(paused: bool, failing: u64, online: bool, busy: bool) -> Phase {
    if paused {
        Phase::Paused
    } else if failing > 0 {
        Phase::Attention
    } else if !online {
        Phase::Offline
    } else if busy {
        Phase::Syncing
    } else {
        Phase::Synced
    }
}

/// "3 issues — View": the menu item that opens the Sync page's queue.
fn issues_label(failing: u64) -> String {
    // Translators: a menu item that opens the list of failing changes.
    ngettext_f("{n} issue — View", "{n} issues — View", failing, &[])
}

fn open_folder(mountpoint: &Path) {
    pdfs_core::opener::open_default(mountpoint, true);
}

/// Launch (or, since it's `SingleMainWindow`, raise) the desktop app with
/// `args`. The app binary lives next to the tray binary; fall back to the bare
/// name so a PATH install still works.
fn open_manager(args: &[&str]) {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("pdfs-app")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("pdfs-app"));
    if let Err(e) = Command::new(&exe).args(args).spawn() {
        tracing::error!("failed to launch {}: {e}", exe.display());
    }
}

/// Remember that the user hid the icon, so the next login's autostart and the
/// app both leave it away, then exit.
fn hide_tray() {
    if let Ok(dirs) = AppDirs::new() {
        let mut config = dirs.load_config();
        config.tray_hidden = true;
        if let Err(e) = dirs.save_config(&config) {
            tracing::warn!("hide tray icon: {e}");
        }
    }
    std::process::exit(0);
}

impl Tray for DriveTray {
    fn id(&self) -> String {
        "io.narl.proton-drive-linux".into()
    }

    fn title(&self) -> String {
        "Proton Drive".into()
    }

    fn icon_name(&self) -> String {
        self.state.phase.icon().into()
    }

    fn status(&self) -> Status {
        match self.state.phase {
            Phase::Attention => Status::NeedsAttention,
            _ => Status::Active,
        }
    }

    fn tool_tip(&self) -> ToolTip {
        let mut description = self.state.line.clone();
        if !self.state.sync.is_empty() {
            description = format!("{description}\n{}", self.state.sync);
        }
        ToolTip {
            title: "Proton Drive".into(),
            description,
            ..Default::default()
        }
    }

    /// A left click opens the app, as it does for most tray icons.
    fn activate(&mut self, _x: i32, _y: i32) {
        open_manager(&[]);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        // Value captured into the action closure (which only gets `&mut Self`).
        let open_mp = self.state.mountpoint.clone();
        let label = |text: &str| -> MenuItem<Self> {
            StandardItem {
                label: text.to_string(),
                enabled: false,
                ..Default::default()
            }
            .into()
        };
        let item = |text: &str, run: fn(&mut Self)| -> MenuItem<Self> {
            StandardItem {
                label: text.to_string(),
                activate: Box::new(run),
                ..Default::default()
            }
            .into()
        };

        let mut items: Vec<MenuItem<Self>> = vec![label(&self.state.line)];
        // Live sync status, shown only while something is transferring.
        if !self.state.sync.is_empty() {
            items.push(label(&self.state.sync));
        }
        if self.state.failing > 0 {
            items.push(item(&issues_label(self.state.failing), |_| {
                open_manager(&["--page", "locations"])
            }));
        }
        items.push(MenuItem::Separator);

        match self.state.phase {
            // Nothing to mount without a session; the app's sign-in page is the
            // way in.
            Phase::SignedOut => items.push(item(&gettext("Sign In…"), |_| open_manager(&[]))),
            _ => items.push(item(&gettext("Open Proton Drive"), |_| open_manager(&[]))),
        }

        if self.state.mounted {
            items.push(
                StandardItem {
                    label: gettext("Open Folder"),
                    activate: Box::new(move |_: &mut Self| open_folder(&open_mp)),
                    ..Default::default()
                }
                .into(),
            );
            // Pause holds uploads back but leaves the mount readable, so it
            // sits apart from Stop, which takes the whole mount away.
            let paused = self.state.paused;
            items.push(
                StandardItem {
                    label: if paused {
                        gettext("Resume Sync")
                    } else {
                        gettext("Pause Sync")
                    },
                    activate: Box::new(move |this: &mut Self| {
                        let request = Request::SetSyncPaused {
                            paused: !paused,
                            until: None,
                        };
                        match send(&this.socket, &request) {
                            Ok(Response::Ok { .. }) => this.state.paused = !paused,
                            Ok(other) => tracing::warn!("pause sync: {other:?}"),
                            Err(e) => tracing::warn!("pause sync: {e}"),
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
        } else if self.state.phase == Phase::Disconnected {
            // Enable+start the service so it mounts now and on future logins.
            items.push(item(&gettext("Connect"), |this| {
                service::enable_start();
                this.state.line = gettext("Connecting…");
            }));
        }

        items.push(MenuItem::Separator);
        items.push(item(&gettext("Hide Tray Icon"), |_| hide_tray()));
        if self.state.mounted {
            // Stopping unmounts the drive, so the app asks first; the tray has no
            // GTK of its own to ask with.
            items.push(item(&gettext("Stop Proton Drive…"), |_| {
                open_manager(&["--confirm-stop"])
            }));
        } else {
            items.push(item(&gettext("Quit"), |_| std::process::exit(0)));
        }
        items
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    i18n::init();

    let dirs = AppDirs::new().context("resolve app dirs")?;
    // Hidden from its own menu: the session's autostart still runs the tray, and
    // this is where that choice is kept.
    if dirs.load_config().tray_hidden {
        tracing::info!("tray icon is hidden in the config; exiting.");
        return Ok(());
    }

    // Ensure only one instance of the tray runs.
    let tray_sock = dirs.tray_socket();
    if std::os::unix::net::UnixStream::connect(&tray_sock).is_ok() {
        tracing::info!("Another instance of pdfs-tray is already running; exiting.");
        return Ok(());
    }
    let _ = std::fs::remove_file(&tray_sock);
    let lock_socket = std::os::unix::net::UnixListener::bind(&tray_sock)
        .context("failed to bind tray single-instance socket")?;
    // Owner-only like the control socket (B6). This one only guards single
    // instancing, so a failure is logged rather than fatal — but there is no
    // reason to leave it reachable by other local users either.
    if let Err(e) = pdfs_core::config::restrict_socket(&tray_sock) {
        tracing::warn!(error = %e, "could not restrict tray socket permissions");
    }

    // The app asks the tray to leave through its socket, when the user hides
    // the icon from Preferences or stops Proton Drive.
    std::thread::spawn(move || {
        for stream in lock_socket.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_ok()
                && line.trim() == pdfs_core::tray::QUIT
            {
                std::process::exit(0);
            }
        }
    });

    let socket = dirs.control_socket();
    let default_mountpoint = dirs.default_mountpoint();

    let tray = DriveTray {
        state: poll_state(&socket, &default_mountpoint),
        socket: socket.clone(),
    };

    // The tray autostarts with the session and can beat the desktop's SNI
    // watcher onto the bus. Without `assume_sni_available` that race is a hard
    // spawn error; with it the item waits and registers once the watcher shows.
    let handle = tray
        .assume_sni_available(true)
        .spawn()
        .context("start tray service")?;

    // Poll the daemon forever, pushing each fresh snapshot into the tray so the
    // menu reflects mount/login changes made elsewhere (e.g. via the CLI).
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let st = poll_state(&socket, &default_mountpoint);
        handle.update(move |t: &mut DriveTray| t.state = st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_outranks_failures_and_failures_outrank_the_rest() {
        assert_eq!(phase_of(true, 3, false, true), Phase::Paused);
        assert_eq!(phase_of(false, 3, false, true), Phase::Attention);
        assert_eq!(phase_of(false, 0, false, true), Phase::Offline);
        assert_eq!(phase_of(false, 0, true, true), Phase::Syncing);
        assert_eq!(phase_of(false, 0, true, false), Phase::Synced);
    }

    #[test]
    fn the_issues_item_counts_in_the_singular_and_plural() {
        assert_eq!(issues_label(1), "1 issue — View");
        assert_eq!(issues_label(4), "4 issues — View");
    }
}
