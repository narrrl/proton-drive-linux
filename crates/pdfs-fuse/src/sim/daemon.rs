//! One simulated daemon: a real FUSE mount on its own state directory, whose
//! Drive is a [`FakeClient`].
//!
//! Everything the daemon does runs as in production (drain workers, sync
//! engine, event feed, online probe, control socket) except what the host
//! changes (`mount::Host::Simulation`): a channel stops it instead of a signal,
//! and it does not index the developer's home.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pdfs_core::cache::ContentCache;
use pdfs_core::config::AppConfig;
use pdfs_core::control::{self, PendingOpInfo, Request, Response};
use pdfs_core::db::Db;
use proton_drive_rs::ProtonDriveClient;
use proton_drive_rs::proton_sdk::config::ProtonClientConfiguration;
use proton_drive_rs::proton_sdk::session::{PasswordMode, ProtonApiSession, ResumeParameters};

use super::fake_drive::FakeClient;
use crate::mount::{Host, MountOptions, MountOutcome, mount_with};

/// How long a mount may take to come up.
const MOUNT_DEADLINE: Duration = Duration::from_secs(20);

/// How long a restart may wait for the last run to let go of its database.
const LAST_RUN_DEADLINE: Duration = Duration::from_secs(30);

pub(crate) struct Daemon {
    pub(crate) client: FakeClient,
    pub(crate) mountpoint: PathBuf,
    dir: PathBuf,
    socket: PathBuf,
    stop: SyncSender<()>,
    mount: Option<JoinHandle<io::Result<MountOutcome>>>,
    rt: Option<tokio::runtime::Runtime>,
}

impl Daemon {
    /// Mount a daemon on the state under `dir`, creating it on the first start.
    /// A later start on the same `dir` is a restart: it finds the queue, the
    /// cache and the database the last run left.
    pub(crate) fn start(dir: &Path, client: FakeClient) -> io::Result<Self> {
        log_to_test_output();
        let mountpoint = dir.join("mnt");
        let socket = dir.join("control.sock");
        std::fs::create_dir_all(&mountpoint)?;
        // A thread of the last run that is still in a request holds the
        // database, and with it the single-writer lock, until it returns. A
        // real restart waits for the old process to exit in the same way.
        let mut db = Db::open(&dir.join("pdfs.db"));
        let deadline = Instant::now() + LAST_RUN_DEADLINE;
        while db
            .as_ref()
            .is_err_and(|error| error.to_string().contains("already using"))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(100));
            db = Db::open(&dir.join("pdfs.db"));
        }
        let db = Arc::new(db.map_err(io::Error::other)?);
        let cache = ContentCache::open(dir.join("cache"), dir.join("pins.json"), 0, db.clone())
            .map_err(io::Error::other)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let (stop, stopped) = std::sync::mpsc::sync_channel(1);
        let mount = {
            let drive = Arc::new(client.clone());
            let handle = rt.handle().clone();
            let mountpoint = mountpoint.clone();
            let socket = socket.clone();
            std::thread::Builder::new()
                .name("sim-mount".into())
                .spawn(move || {
                    mount_with(
                        offline_client(),
                        drive,
                        handle,
                        &mountpoint,
                        cache,
                        &socket,
                        db,
                        MountOptions {
                            username: "sim".into(),
                            sweep_mode: AppConfig::default().resolved_conflict_sweep(),
                            upload_limit: 0,
                            download_limit: 0,
                            local_first: AppConfig::default().local_first.unwrap_or(true),
                        },
                        Host::Simulation(stopped),
                    )
                })?
        };
        let deadline = Instant::now() + MOUNT_DEADLINE;
        while !is_mounted(&mountpoint) {
            if mount.is_finished() {
                return Err(match mount.join() {
                    Ok(Err(error)) => error,
                    _ => io::Error::other("mount ended before it came up"),
                });
            }
            if Instant::now() > deadline {
                return Err(io::Error::other("mount did not come up"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(Self {
            client,
            mountpoint,
            dir: dir.to_path_buf(),
            socket,
            stop,
            mount: Some(mount),
            rt: Some(rt),
        })
    }

    /// What the daemon still owes Drive, as `pdfs sync queue` shows it.
    pub(crate) fn pending(&self) -> io::Result<Vec<PendingOpInfo>> {
        match control::send(&self.socket, &Request::ListPendingOps).map_err(io::Error::other)? {
            Response::PendingOps { items } => Ok(items),
            other => Err(io::Error::other(format!("unexpected answer {other:?}"))),
        }
    }

    /// Send one control request, as the `pdfs` CLI does, and fail on a
    /// refusal.
    pub(crate) fn request(&self, request: &Request) -> io::Result<Response> {
        match control::send(&self.socket, request).map_err(io::Error::other)? {
            Response::Error { message, .. } => Err(io::Error::other(message)),
            response => Ok(response),
        }
    }

    /// Stop the daemon and start it again on the same state and link.
    pub(crate) fn restart(mut self) -> io::Result<Self> {
        self.shut_down()?;
        Self::start(&self.dir.clone(), self.client.clone())
    }

    /// Stop the daemon the way `systemctl --user stop` does.
    pub(crate) fn stop(mut self) -> io::Result<MountOutcome> {
        self.shut_down()
    }

    fn shut_down(&mut self) -> io::Result<MountOutcome> {
        let Some(mount) = self.mount.take() else {
            return Ok(MountOutcome::Shutdown);
        };
        let _ = self.stop.try_send(());
        let outcome = mount
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("mount thread panicked")));
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
        outcome
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.shut_down();
    }
}

/// Whether a file system is mounted at `path`: it sits on another device than
/// its parent.
fn is_mounted(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let device = |path: &Path| std::fs::metadata(path).map(|meta| meta.dev()).ok();
    let parent = path.parent().and_then(device);
    device(path).is_some_and(|dev| Some(dev) != parent)
}

/// The daemons' logs, filtered by `RUST_LOG` and shown with `--nocapture`.
/// What they log at `INFO` and above is also kept for [`take_logged`],
/// whatever the filter.
fn log_to_test_output() {
    use tracing_subscriber::prelude::*;
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_test_writer()
                .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
        )
        .with(KeepLogged.with_filter(tracing_subscriber::filter::LevelFilter::INFO))
        .try_init();
}

/// What the daemons logged at `INFO` and above since the last call, each as
/// its message followed by its fields: `message key=value ...`.
pub(crate) fn take_logged() -> Vec<String> {
    std::mem::take(&mut *LOGGED.lock())
}

static LOGGED: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

struct KeepLogged;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for KeepLogged {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        LOGGED.lock().push(message.0);
    }
}

struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.insert_str(0, &format!("{value:?}"));
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
}

/// A client for what `DriveApi` does not cover (sharing, Photos, devices),
/// pointed at a closed local port so none of it leaves the machine.
fn offline_client() -> ProtonDriveClient {
    let config = ProtonClientConfiguration::new("external-drive-pdfs-sim@0.0.0-dev")
        .with_base_url("http://127.0.0.1:9");
    let session = ProtonApiSession::resume(
        config,
        ResumeParameters {
            session_id: "sim".to_owned().into(),
            username: "sim".into(),
            user_id: "sim".to_owned().into(),
            access_token: String::new(),
            refresh_token: String::new(),
            scopes: Vec::new(),
            is_waiting_for_second_factor_code: false,
            password_mode: PasswordMode::Single,
        },
    )
    .expect("an offline session needs no network");
    ProtonDriveClient::new(&session, Vec::new())
}

/// A fresh directory for one simulation, under the system temp dir.
pub(crate) fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pdfs-sim-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create a simulation directory");
    dir
}

/// Wait up to `limit` for `done`, polling.
pub(crate) fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::fake_drive::{Entry, FakeDrive, Faults};
    use std::collections::BTreeMap;

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_daemon_on_the_fake_drive_syncs_both_ways() {
        let drive = FakeDrive::new();
        let dir = scratch("smoke");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();

        std::fs::write(daemon.mountpoint.join("hello.txt"), b"hi").unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                drive.tree().get("hello.txt") == Some(&Entry::File(Arc::new(b"hi".to_vec())))
            }),
            "a local write landed on Drive: {:?}",
            drive.tree()
        );

        drive.device().write("phone.txt", b"from the phone");
        let remote = daemon.mountpoint.join("phone.txt");
        assert!(
            wait_until(Duration::from_secs(30), || {
                std::fs::read(&remote).ok().as_deref() == Some(b"from the phone".as_slice())
            }),
            "a remote write reached the mount"
        );

        daemon.client.set_online(false);
        std::fs::write(daemon.mountpoint.join("offline.txt"), b"queued").unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert!(!drive.tree().contains_key("offline.txt"));
        daemon.client.set_online(true);
        assert!(
            wait_until(Duration::from_secs(60), || drive
                .tree()
                .contains_key("offline.txt")),
            "a write made offline landed once the link came back"
        );

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write `bytes` at `offset` into an existing file, without truncating it.
    fn write_at(path: &Path, offset: u64, bytes: &[u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .write_all_at(bytes, offset)
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_file_closed_empty_takes_a_second_write_at_once() {
        // The empty revision never counted as complete, so the next write
        // open before it drained was refused with EIO (B123).
        let drive = FakeDrive::new();
        let dir = scratch("empty-rewrite");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();

        let path = daemon.mountpoint.join("empty.txt");
        std::fs::write(&path, b"old").unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            drive.tree().contains_key("empty.txt")
        }));
        std::fs::write(&path, b"").unwrap();
        write_at(&path, 0, b"filled").unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                drive.tree().get("empty.txt") == Some(&Entry::File(Arc::new(b"filled".to_vec())))
            }),
            "the second write landed: {:?}",
            drive.tree()
        );

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn the_echo_of_our_own_rename_keeps_the_cached_bytes() {
        // The echo read as someone else's change and evicted the bytes, so a
        // partial write could not gap-fill, and the next write open on the
        // file was refused with EIO (B123).
        let drive = FakeDrive::new();
        let dir = scratch("rename-echo");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();

        let before = daemon.mountpoint.join("a.txt");
        std::fs::write(&before, b"0123456789").unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            drive.tree().contains_key("a.txt")
        }));
        assert_eq!(std::fs::read(&before).unwrap(), b"0123456789");
        let after = daemon.mountpoint.join("b.txt");
        std::fs::rename(&before, &after).unwrap();
        // Long enough for the next event poll to bring the echo.
        std::thread::sleep(crate::POLL_INTERVAL + Duration::from_secs(3));

        write_at(&after, 3, b"x").unwrap();
        write_at(&after, 5, b"y").unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                drive.tree().get("b.txt") == Some(&Entry::File(Arc::new(b"012x4y6789".to_vec())))
            }),
            "both writes landed: {:?}",
            drive.tree()
        );

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_rename_across_folders_ignores_the_new_name_in_the_old_folder() {
        // The rename went to Drive as a rename in the old folder and then a
        // move, so a sibling there with the new name failed it with EIO
        // (B122).
        let drive = FakeDrive::new();
        let dir = scratch("rename-across");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();

        let mount = &daemon.mountpoint;
        std::fs::create_dir(mount.join("x")).unwrap();
        std::fs::create_dir(mount.join("z")).unwrap();
        std::fs::write(mount.join("x/f.txt"), b"moved").unwrap();
        std::fs::write(mount.join("x/d"), b"sibling").unwrap();
        std::fs::write(mount.join("z/d"), b"replaced").unwrap();
        let file = |bytes: &[u8]| Some(Entry::File(Arc::new(bytes.to_vec())));
        assert!(wait_until(Duration::from_secs(30), || {
            let tree = drive.tree();
            [
                ("x/f.txt", b"moved".as_slice()),
                ("x/d", b"sibling"),
                ("z/d", b"replaced"),
            ]
            .iter()
            .all(|(path, bytes)| tree.get(*path).cloned() == file(bytes))
        }));

        std::fs::rename(mount.join("x/f.txt"), mount.join("z/d")).unwrap();
        assert_eq!(std::fs::read(mount.join("z/d")).unwrap(), b"moved");
        // The rename is recorded locally and sent from the queue.
        assert!(
            wait_until(Duration::from_secs(30), || {
                let tree = drive.tree();
                tree.get("z/d").cloned() == file(b"moved")
                    && tree.get("x/d").cloned() == file(b"sibling")
                    && !tree.contains_key("x/f.txt")
            }),
            "{:?}",
            drive.tree().keys().collect::<Vec<_>>()
        );

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_write_over_a_create_whose_read_back_failed_is_not_a_conflict() {
        // The create landed and the link went down before it was read back.
        // The next write was based on the file as it was made locally, and
        // landed as a conflict copy of it (B143).
        let drive = FakeDrive::new();
        let dir = scratch("unadopted");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let file = |bytes: &[u8]| Some(Entry::File(Arc::new(bytes.to_vec())));
        let path = daemon.mountpoint.join("f.txt");

        // Drive stamps a create with the second it arrives, so it has to land
        // a second after the file was made here for the two to differ.
        daemon.client.set_online(false);
        std::fs::write(&path, b"first").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        daemon.client.drop_link_after_next_create();
        daemon.client.set_online(true);
        assert!(wait_until(Duration::from_secs(30), || {
            drive.tree().get("f.txt").cloned() == file(b"first")
        }));
        write_at(&path, 5, b" and second").unwrap();
        daemon.client.set_online(true);
        assert!(
            wait_until(Duration::from_secs(30), || {
                drive.tree().get("f.txt").cloned() == file(b"first and second")
            }),
            "{:?}",
            drive.tree()
        );
        assert_eq!(drive.tree().len(), 1, "{:?}", drive.tree());

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_pdfs_rename_after_a_queued_rename_is_not_undone() {
        // `pdfs rename` and `pdfs move` went straight to Drive, and the
        // mount's own rename, still queued, landed after them and put the old
        // name back (B149, the account run's B121 case).
        let drive = FakeDrive::new();
        let dir = scratch("pdfs-rename-after-queued");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();

        let mount = &daemon.mountpoint;
        std::fs::create_dir_all(mount.join("b/inner")).unwrap();
        std::fs::write(mount.join("b/first.txt"), b"moved").unwrap();
        let file = |bytes: &[u8]| Some(Entry::File(Arc::new(bytes.to_vec())));
        assert!(wait_until(Duration::from_secs(30), || {
            drive.tree().get("b/first.txt").cloned() == file(b"moved")
        }));

        let paused = |paused| Request::SetSyncPaused {
            paused,
            until: None,
        };
        daemon.request(&paused(true)).unwrap();
        std::fs::rename(mount.join("b/first.txt"), mount.join("b/back.txt")).unwrap();
        let path = |rel: &str| mount.join(rel).to_string_lossy().into_owned();
        daemon
            .request(&Request::Rename {
                path: path("b/back.txt"),
                new_name: "first.txt".into(),
            })
            .unwrap();
        daemon
            .request(&Request::Move {
                path: path("b/first.txt"),
                new_parent: path("b/inner"),
            })
            .unwrap();
        assert_eq!(
            std::fs::read(mount.join("b/inner/first.txt")).unwrap(),
            b"moved"
        );
        daemon.request(&paused(false)).unwrap();

        assert!(
            wait_until(Duration::from_secs(30), || {
                let tree = drive.tree();
                tree.get("b/inner/first.txt").cloned() == file(b"moved")
                    && daemon.pending().is_ok_and(|ops| ops.is_empty())
            }),
            "{:?}",
            drive.tree().keys().collect::<Vec<_>>()
        );
        let tree = drive.tree();
        assert!(!tree.contains_key("b/back.txt") && !tree.contains_key("b/inner/back.txt"));

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_queued_write_whose_folder_was_trashed_lands_in_the_root() {
        // The conflict copy of a write to a file trashed elsewhere went into
        // the file's folder. With that folder gone too, the upload failed on
        // every retry, every two minutes (B150).
        let drive = FakeDrive::new();
        let dir = scratch("conflict-copy-folder-gone");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();

        let mount = &daemon.mountpoint;
        std::fs::create_dir(mount.join("d")).unwrap();
        std::fs::write(mount.join("d/f.txt"), b"first").unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            drive.tree().contains_key("d/f.txt") && daemon.pending().is_ok_and(|ops| ops.is_empty())
        }));

        let paused = |paused| Request::SetSyncPaused {
            paused,
            until: None,
        };
        daemon.request(&paused(true)).unwrap();
        std::fs::write(mount.join("d/f.txt"), b"second").unwrap();
        drive.device().trash("d/f.txt");
        drive.device().trash("d");
        daemon.request(&paused(false)).unwrap();

        let copy = |tree: &BTreeMap<String, Entry>| {
            tree.iter()
                .find(|(path, _)| path.starts_with("f (sync-conflict "))
                .map(|(_, entry)| entry.clone())
        };
        assert!(
            wait_until(Duration::from_secs(30), || {
                copy(&drive.tree()) == Some(Entry::File(Arc::new(b"second".to_vec())))
                    && daemon.pending().is_ok_and(|ops| ops.is_empty())
            }),
            "{:?} {:?}",
            drive.tree().keys().collect::<Vec<_>>(),
            daemon.pending()
        );

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Make 100 folders of 10 empty files each through a mount on `faults`, and
    /// time the syscalls and then the drain until Drive holds all of it.
    fn time_a_thousand_files(faults: Faults) -> (Duration, Duration) {
        let drive = FakeDrive::new();
        let dir = scratch("thousand");
        let daemon = Daemon::start(&dir, drive.client(1, faults)).unwrap();
        let start = Instant::now();
        for folder in 0..100 {
            let folder = daemon.mountpoint.join(format!("d{folder:03}"));
            std::fs::create_dir(&folder).unwrap();
            for file in 0..10 {
                std::fs::File::create(folder.join(format!("f{file}"))).unwrap();
            }
        }
        let syscalls = start.elapsed();
        let start = Instant::now();
        assert!(
            wait_until(Duration::from_secs(1800), || {
                drive.tree().len() == 1100 && daemon.pending().is_ok_and(|ops| ops.is_empty())
            }),
            "the drain did not finish: {} of 1100 on Drive",
            drive.tree().len()
        );
        let drain = start.elapsed();
        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
        (syscalls, drain)
    }

    /// The milestone's speed target: a thousand files reach Drive as fast on
    /// Wi-Fi as on a LAN. It only measures for now, and takes minutes, so it
    /// runs when `PDFS_SIM_MEASURE` is set.
    #[test]
    #[ignore = "mounts FUSE: run with `PDFS_SIM_MEASURE=1 cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_thousand_files_drain_as_fast_on_wifi_as_on_lan() {
        if std::env::var_os("PDFS_SIM_MEASURE").is_none() {
            eprintln!("skipped: set PDFS_SIM_MEASURE to measure the drain");
            return;
        }
        let (lan_syscalls, lan_drain) = time_a_thousand_files(Faults::lan());
        let (wifi_syscalls, wifi_drain) = time_a_thousand_files(Faults::wifi());
        eprintln!("lan:  syscalls {lan_syscalls:?}, drain {lan_drain:?}");
        eprintln!("wifi: syscalls {wifi_syscalls:?}, drain {wifi_drain:?}");
    }
}
