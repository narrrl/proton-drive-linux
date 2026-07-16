//! `pdfs-tray` — system-tray front-end for the Proton Drive Linux client.
//!
//! Slice 1 of the GUI phase: a StatusNotifierItem (SNI) tray icon with a menu
//! showing login/mount status and offering mount, unmount, open-folder and quit.
//! It talks to the running mount daemon over the same control socket the CLI
//! uses, so the tray is a thin presentation layer over [`pdfs_core::control`].
//!
//! GTK4 has no tray widget (GNOME dropped the systray), so the tray itself is an
//! SNI item via the `ksni` crate; the GTK4/libadwaita settings window arrives in
//! a later slice.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use ksni::menu::StandardItem;
use ksni::{MenuItem, Tray, TrayService};
use pdfs_core::auth;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{JobItem, Request, Response, TransferDirection, TransferItem, send};
use pdfs_core::service;

/// How often the tray re-polls the daemon to refresh its menu.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Snapshot of what the tray knows about the daemon, recomputed each poll.
#[derive(Clone)]
struct DriveState {
    /// Human-readable first menu line ("Mounted at …", "Not running", …).
    line: String,
    /// Whether a mount daemon is currently serving the control socket.
    mounted: bool,
    /// Mountpoint to act on (from the daemon when mounted, else the default).
    mountpoint: PathBuf,
    /// One-line sync summary of in-flight transfers, empty when idle. Shown as a
    /// disabled menu line under the status line.
    sync: String,
}

/// Summarise a work snapshot into one menu line, or empty when idle. A single
/// transfer names the file and its percentage; several collapse to counts so the
/// menu stays one line regardless of queue depth. With nothing moving bytes, a
/// running job speaks for itself — a scan or an index rebuild is still "busy",
/// and the tray saying nothing there reads as "finished".
fn sync_line(items: &[TransferItem], jobs: &[JobItem]) -> String {
    match items {
        [] => match jobs.first() {
            Some(j) if j.total > 0 => format!("{} ({} of {})", j.title, j.done, j.total),
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
                (d, 0) => format!("↓ {d} downloading"),
                (0, u) => format!("↑ {u} uploading"),
                (d, u) => format!("↓ {d} · ↑ {u}"),
            }
        }
    }
}

struct DriveTray {
    state: DriveState,
}

/// Ask the daemon for its status, falling back to the stored session so the
/// menu can still say "Logged in …" / "Not logged in" when no mount is running.
fn poll_state(socket: &Path, default_mountpoint: &Path) -> DriveState {
    match send(socket, &Request::Status) {
        Ok(Response::Status {
            mountpoint, pinned, ..
        }) => DriveState {
            line: format!("Mounted at {mountpoint} ({pinned} pinned)"),
            mounted: true,
            mountpoint: PathBuf::from(mountpoint),
            // Same daemon is up, so a cheap follow-up poll gives the sync line.
            sync: match send(socket, &Request::GetQueueStatus) {
                Ok(Response::Transfers { items, jobs }) => sync_line(&items, &jobs),
                _ => String::new(),
            },
        },
        // Socket answered but with something unexpected — treat as up but odd.
        Ok(_) => DriveState {
            line: "Mount: unexpected daemon response".into(),
            mounted: true,
            mountpoint: default_mountpoint.to_path_buf(),
            sync: String::new(),
        },
        // No daemon: describe login state instead so the menu is still useful.
        Err(_) => {
            let line = match auth::load() {
                Ok(s) => format!("Logged in as {} — not mounted", s.username),
                Err(pdfs_core::Error::NotLoggedIn) => "Not logged in".into(),
                Err(e) => format!("Error: {e}"),
            };
            DriveState {
                line,
                mounted: false,
                mountpoint: default_mountpoint.to_path_buf(),
                sync: String::new(),
            }
        }
    }
}

fn open_folder(mountpoint: &Path) {
    if let Err(e) = Command::new("xdg-open").arg(mountpoint).spawn() {
        tracing::error!("failed to xdg-open {}: {e}", mountpoint.display());
    }
}

/// Launch (or, since it's `SingleMainWindow`, raise) the settings/management
/// window. The app binary lives next to the tray binary; fall back to the bare
/// name so a PATH install still works.
fn open_manager() {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("pdfs-app")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("pdfs-app"));
    if let Err(e) = Command::new(&exe).spawn() {
        tracing::error!("failed to launch {}: {e}", exe.display());
    }
}

impl Tray for DriveTray {
    fn id(&self) -> String {
        "io.narl.proton-drive-linux".into()
    }

    fn title(&self) -> String {
        "Proton Drive".into()
    }

    fn icon_name(&self) -> String {
        "folder-remote".into()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        // Value captured into the action closure (which only gets `&mut Self`).
        let open_mp = self.state.mountpoint.clone();

        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: self.state.line.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ];

        // Live sync status, shown only while something is transferring.
        if !self.state.sync.is_empty() {
            items.push(
                StandardItem {
                    label: self.state.sync.clone(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }

        items.extend([
            MenuItem::Separator,
            StandardItem {
                label: "Open Manager".into(),
                activate: Box::new(|_: &mut Self| open_manager()),
                ..Default::default()
            }
            .into(),
        ]);

        if self.state.mounted {
            items.push(
                StandardItem {
                    label: "Open folder".into(),
                    activate: Box::new(move |_: &mut Self| open_folder(&open_mp)),
                    ..Default::default()
                }
                .into(),
            );
            // Stop the systemd service (clean SIGTERM → lazy unmount). The next
            // login or reboot brings it back; this is a deliberate disconnect.
            items.push(
                StandardItem {
                    label: "Disconnect".into(),
                    activate: Box::new(move |this: &mut Self| {
                        service::stop();
                        this.state.mounted = false;
                        this.state.line = "Disconnecting…".into();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        } else {
            // Enable+start the service so it mounts now and on future logins.
            items.push(
                StandardItem {
                    label: "Connect".into(),
                    activate: Box::new(|this: &mut Self| {
                        service::enable_start();
                        this.state.line = "Connecting…".into();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let dirs = AppDirs::new().context("resolve app dirs")?;

    // Ensure only one instance of the tray runs.
    let tray_sock = dirs.tray_socket();
    if std::os::unix::net::UnixStream::connect(&tray_sock).is_ok() {
        tracing::info!("Another instance of pdfs-tray is already running; exiting.");
        return Ok(());
    }
    let _ = std::fs::remove_file(&tray_sock);
    let _lock_socket = std::os::unix::net::UnixListener::bind(&tray_sock)
        .context("failed to bind tray single-instance socket")?;

    let socket = dirs.control_socket();
    let default_mountpoint = dirs.default_mountpoint();

    let tray = DriveTray {
        state: poll_state(&socket, &default_mountpoint),
    };

    let service = TrayService::new(tray);
    let handle = service.handle();
    std::thread::spawn(move || {
        if let Err(e) = service.run() {
            tracing::error!("tray service stopped: {e:?}");
        }
    });

    // Poll the daemon forever, pushing each fresh snapshot into the tray so the
    // menu reflects mount/login changes made elsewhere (e.g. via the CLI).
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let st = poll_state(&socket, &default_mountpoint);
        handle.update(move |t: &mut DriveTray| t.state = st);
    }
}
