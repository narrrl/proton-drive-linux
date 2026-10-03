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
}
