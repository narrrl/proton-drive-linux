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
use pdfs_core::db::Db;
use proton_drive_rs::ProtonDriveClient;
use proton_drive_rs::proton_sdk::config::ProtonClientConfiguration;
use proton_drive_rs::proton_sdk::session::{PasswordMode, ProtonApiSession, ResumeParameters};

use super::fake_drive::FakeClient;
use crate::mount::{Host, MountOptions, MountOutcome, mount_with};

/// How long a mount may take to come up.
const MOUNT_DEADLINE: Duration = Duration::from_secs(20);

pub(crate) struct Daemon {
    pub(crate) client: FakeClient,
    pub(crate) mountpoint: PathBuf,
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
        std::fs::create_dir_all(&mountpoint)?;
        let db = Arc::new(Db::open(&dir.join("pdfs.db")).map_err(io::Error::other)?);
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
            let socket = dir.join("control.sock");
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
            stop,
            mount: Some(mount),
            rt: Some(rt),
        })
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
fn log_to_test_output() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
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
