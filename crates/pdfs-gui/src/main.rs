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
use pdfs_core::control::{Request, Response, send};

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
}

struct DriveTray {
    state: DriveState,
}

/// Ask the daemon for its status, falling back to the stored session so the
/// menu can still say "Logged in …" / "Not logged in" when no mount is running.
fn poll_state(socket: &Path, default_mountpoint: &Path) -> DriveState {
    match send(socket, &Request::Status) {
        Ok(Response::Status { mountpoint, pinned, .. }) => DriveState {
            line: format!("Mounted at {mountpoint} ({pinned} pinned)"),
            mounted: true,
            mountpoint: PathBuf::from(mountpoint),
        },
        // Socket answered but with something unexpected — treat as up but odd.
        Ok(_) => DriveState {
            line: "Mount: unexpected daemon response".into(),
            mounted: true,
            mountpoint: default_mountpoint.to_path_buf(),
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
            }
        }
    }
}

/// Launch `pdfs mount` detached so the tray keeps running independently.
fn spawn_mount() {
    match Command::new("pdfs").arg("mount").spawn() {
        Ok(_) => tracing::info!("spawned `pdfs mount`"),
        Err(e) => tracing::error!("failed to spawn `pdfs mount`: {e}"),
    }
}

/// Unmount the FUSE filesystem. `fusermount3 -u` is the FUSE-native teardown;
/// fall back to `fusermount` on older systems.
fn unmount(mountpoint: &Path) {
    let try_unmount = |bin: &str| Command::new(bin).arg("-u").arg(mountpoint).status();
    let result = try_unmount("fusermount3").or_else(|_| try_unmount("fusermount"));
    match result {
        Ok(s) if s.success() => tracing::info!("unmounted {}", mountpoint.display()),
        Ok(s) => tracing::error!("unmount exited with {s}"),
        Err(e) => tracing::error!("failed to run fusermount: {e}"),
    }
}

fn open_folder(mountpoint: &Path) {
    if let Err(e) = Command::new("xdg-open").arg(mountpoint).spawn() {
        tracing::error!("failed to xdg-open {}: {e}", mountpoint.display());
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
        // Values captured into the action closures (which only get `&mut Self`).
        let mountpoint = self.state.mountpoint.clone();
        let open_mp = mountpoint.clone();

        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: self.state.line.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];

        if self.state.mounted {
            items.push(
                StandardItem {
                    label: "Open folder".into(),
                    activate: Box::new(move |_: &mut Self| open_folder(&open_mp)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Unmount".into(),
                    activate: Box::new(move |this: &mut Self| {
                        unmount(&mountpoint);
                        this.state.mounted = false;
                        this.state.line = "Unmounting…".into();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        } else {
            items.push(
                StandardItem {
                    label: "Mount".into(),
                    activate: Box::new(|this: &mut Self| {
                        spawn_mount();
                        this.state.line = "Mounting…".into();
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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let dirs = AppDirs::new().context("resolve app dirs")?;
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
