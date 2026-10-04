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
    fn a_folder_made_while_the_drain_is_idle_reaches_drive_at_once() {
        // A new folder queued its create without waking the drain, so it sat
        // until the workers' 30 s idle poll (B160).
        let drive = FakeDrive::new();
        let dir = scratch("idle-mkdir");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            daemon.pending().is_ok_and(|items| items.is_empty())
        }));

        std::fs::create_dir(daemon.mountpoint.join("x")).unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || drive.lookup("x").is_some()),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_file_whose_upload_was_not_read_back_takes_two_writes_at_once() {
        // The read-back after an upload went unanswered, and the drain let go of
        // the uploaded bytes without caching them. A partial write had nothing
        // to fill its gaps from, and the next open was refused with EIO (B157).
        let drive = FakeDrive::new();
        let dir = scratch("upload-not-read-back");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let path = daemon.mountpoint.join("f.txt");
        let file = |bytes: &[u8]| Some(Entry::File(Arc::new(bytes.to_vec())));

        std::fs::write(&path, b"first").unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            daemon.pending().is_ok_and(|items| items.is_empty())
                && drive.tree().get("f.txt").cloned() == file(b"first")
        }));
        daemon.client.lose_read_back_of_next_revision();
        std::fs::write(&path, b"0123456789").unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            daemon.pending().is_ok_and(|items| items.is_empty())
                && drive.tree().get("f.txt").cloned() == file(b"0123456789")
        }));
        write_at(&path, 2, b"ab").unwrap();
        write_at(&path, 6, b"cd").unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty())
                    && drive.tree().get("f.txt").cloned() == file(b"01ab45cd89")
            }),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );
        assert_eq!(drive.tree().len(), 1, "{:?}", drive.tree());

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_file_moved_while_its_create_was_on_the_wire_keeps_its_new_name() {
        // The move queued behind the create landed while the drain read the
        // new node back. The answer still had the old name, and the drain no
        // longer found the move queued, so the mount showed the old name in
        // the new folder (B161).
        let drive = FakeDrive::new();
        let dir = scratch("moved-while-created");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let mnt = daemon.mountpoint.clone();
        std::fs::create_dir(mnt.join("x")).unwrap();
        std::fs::create_dir(mnt.join("y")).unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty()) && drive.lookup("y").is_some()
            }),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );

        let create = daemon.client.hold_reply_to_next_create();
        std::fs::write(mnt.join("x/c.bin"), b"abc").unwrap();
        assert!(wait_until(Duration::from_secs(30), || create.reached()));
        std::fs::rename(mnt.join("x/c.bin"), mnt.join("y/d")).unwrap();
        let uid = drive.lookup("x/c.bin").unwrap();
        // Each read-back is answered in turn until the create is retired; the
        // answer to the next one is held until the move has landed. A new
        // folder wakes the drain for the move.
        let mut held = create;
        let adopting = loop {
            let next = daemon.client.hold_answer_to_next_read(&uid);
            drop(held);
            assert!(wait_until(Duration::from_secs(30), || next.reached()));
            if daemon
                .pending()
                .is_ok_and(|items| items.iter().all(|op| op.kind != "create"))
            {
                break next;
            }
            held = next;
        };
        std::fs::create_dir(mnt.join("z")).unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || drive.lookup("y/d").is_some()),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );
        drop(adopting);

        let file = Some(Entry::File(Arc::new(b"abc".to_vec())));
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty())
                    && drive.tree().get("y/d").cloned() == file
            }),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );
        let names: Vec<_> = std::fs::read_dir(mnt.join("y"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["d"]);

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_file_read_back_before_drive_committed_it_keeps_its_bytes() {
        // Drive listed the new file a moment before its create was committed,
        // with no revision and no size. The drain took that answer for the
        // file, and a read right after the create landed found it empty
        // (B163).
        let drive = FakeDrive::new();
        let dir = scratch("read-back-unrevised");
        // The feed reports the file as it is long after the read.
        let faults = Faults {
            event_delay: Duration::from_secs(60),
            ..Faults::lan()
        };
        let daemon = Daemon::start(&dir, drive.client(1, faults)).unwrap();
        let mnt = daemon.mountpoint.clone();
        // A folder made here is listed from the tree, not from Drive.
        std::fs::create_dir(mnt.join("x")).unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty()) && drive.lookup("x").is_some()
            }),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );
        take_logged();

        let create = daemon.client.hold_reply_to_next_create();
        std::fs::write(mnt.join("x/c.bin"), b"abc").unwrap();
        assert!(wait_until(Duration::from_secs(30), || create.reached()));
        let uid = drive.lookup("x/c.bin").unwrap();
        // The read-back after the create, the one adopting it, and the size
        // fetch a listing makes.
        daemon.client.answer_reads_unrevised(&uid, 3);
        drop(create);
        let mut logged = Vec::new();
        assert!(
            wait_until(Duration::from_secs(30), || {
                logged.extend(take_logged());
                logged
                    .iter()
                    .any(|line| line.starts_with("pending create landed") && line.contains("c.bin"))
            }),
            "{logged:?}"
        );

        // A listing refreshes the size the kernel holds, as the account run's
        // did before it read the file.
        let sizes: Vec<_> = std::fs::read_dir(mnt.join("x"))
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .collect();
        assert_eq!(sizes, [3]);
        assert_eq!(std::fs::read(mnt.join("x/c.bin")).unwrap(), b"abc");

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_create_drive_lists_before_it_is_read_back_is_listed_once() {
        // The feed reported a new file while the drain still read it back, so
        // the folder was listed from Drive again, and the file came twice: as
        // Drive's node and as the queued create's stand-in. A 128-file folder
        // listed 129 (B164).
        let drive = FakeDrive::new();
        let dir = scratch("listed-before-retired");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let mnt = daemon.mountpoint.clone();
        std::fs::create_dir(mnt.join("x")).unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            daemon.pending().is_ok_and(|items| items.is_empty()) && drive.lookup("x").is_some()
        }));

        let create = daemon.client.hold_reply_to_next_create();
        std::fs::write(mnt.join("x/c.bin"), b"abc").unwrap();
        assert!(wait_until(Duration::from_secs(30), || create.reached()));
        let uid = drive.lookup("x/c.bin").unwrap();
        let read_back = daemon.client.hold_next_read(&uid);
        drop(create);
        assert!(wait_until(Duration::from_secs(30), || read_back.reached()));
        // Long enough for the next event poll to bring the file's event.
        std::thread::sleep(crate::POLL_INTERVAL + Duration::from_secs(3));

        let names: Vec<_> = std::fs::read_dir(mnt.join("x"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        drop(read_back);
        assert_eq!(names, ["c.bin"]);
        assert!(wait_until(Duration::from_secs(30), || {
            daemon.pending().is_ok_and(|items| items.is_empty())
        }));
        assert_eq!(std::fs::read(mnt.join("x/c.bin")).unwrap(), b"abc");

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_folder_made_again_while_the_old_one_is_trashed_is_a_new_folder() {
        // A folder removed and made again at once: the new one's create met
        // the old folder, its trash still on the wire, and took it for the
        // node an unanswered create of its own had made. What went into the
        // new folder went into the old one, and the trash took it all along
        // (B162).
        let drive = FakeDrive::new();
        let dir = scratch("remade-while-trashed");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let mnt = daemon.mountpoint.clone();
        std::fs::create_dir(mnt.join("z")).unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            daemon.pending().is_ok_and(|items| items.is_empty()) && drive.lookup("z").is_some()
        }));
        let old = drive.lookup("z").unwrap();

        let trash = daemon.client.hold_next_trash(&old);
        std::fs::remove_dir(mnt.join("z")).unwrap();
        assert!(wait_until(Duration::from_secs(30), || trash.reached()));
        std::fs::create_dir(mnt.join("z")).unwrap();
        std::fs::write(mnt.join("z/f"), b"abc").unwrap();
        // Sent, and either waiting for the name or landed.
        assert!(wait_until(Duration::from_secs(30), || {
            daemon.pending().is_ok_and(|items| {
                !items
                    .iter()
                    .any(|op| op.kind == "mkdir" && op.attempts == 0)
            })
        }));
        drop(trash);

        let file = Some(Entry::File(Arc::new(b"abc".to_vec())));
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty())
                    && drive.tree().get("z/f").cloned() == file
            }),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );
        assert_ne!(drive.lookup("z"), Some(old));
        assert_eq!(std::fs::read(mnt.join("z/f")).unwrap(), b"abc");

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
    fn a_create_whose_answer_was_lost_lands_once() {
        // Drive made the file, but its answer was lost. The retry found the
        // name taken by a node holding the file's bytes, which was not empty
        // and so not adopted, and uploaded the file again as a conflict copy
        // (B127).
        let drive = FakeDrive::new();
        let dir = scratch("lost-create-reply");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let file = |bytes: &[u8]| Some(Entry::File(Arc::new(bytes.to_vec())));

        daemon.client.lose_reply_to_next_create();
        std::fs::write(daemon.mountpoint.join("f.txt"), b"once").unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty())
                    && drive.tree().get("f.txt").cloned() == file(b"once")
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
    fn a_file_written_again_after_its_create_lost_its_answer_lands_once() {
        // Drive made the file, but its answer was lost, and the file was
        // written again before the retry. The retry found the name taken by a
        // node holding the bytes the first attempt sent, not the ones the op
        // held now, and uploaded the file as a conflict copy (B156).
        let drive = FakeDrive::new();
        let dir = scratch("lost-create-reply-rewritten");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let path = daemon.mountpoint.join("f.txt");
        let file = |bytes: &[u8]| Some(Entry::File(Arc::new(bytes.to_vec())));

        let held = daemon.client.hold_reply_to_next_create();
        daemon.client.lose_reply_to_next_create();
        std::fs::write(&path, b"first").unwrap();
        assert!(wait_until(Duration::from_secs(30), || held.reached()));
        std::fs::write(&path, b"second").unwrap();
        drop(held);
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty())
                    && drive.tree().get("f.txt").cloned() == file(b"second")
            }),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );
        assert_eq!(drive.tree().len(), 1, "{:?}", drive.tree());
        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_file_deleted_after_its_create_lost_its_answer_leaves_drive() {
        // Drive made the file, but its answer was lost, and the file was
        // deleted before the retry. The delete dropped the queued create and
        // left the node Drive had made in place (B151).
        let drive = FakeDrive::new();
        let dir = scratch("lost-create-reply-deleted");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let path = daemon.mountpoint.join("f.txt");

        daemon.client.lose_reply_to_next_create();
        std::fs::write(&path, b"gone").unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            drive.tree().contains_key("f.txt")
        }));
        std::fs::remove_file(&path).unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty()) && drive.tree().is_empty()
            }),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );
        assert!(!path.exists());

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_file_made_under_the_name_of_a_create_on_the_wire_waits_for_it() {
        // A file renamed while its create was on the wire, and a new one made
        // under its old name. The new file's create found the name held by the
        // first and woke itself again after every try: it spent its waits in
        // milliseconds and landed under a conflict name, which the mount then
        // showed instead of the user's (B154).
        let drive = FakeDrive::new();
        let dir = scratch("create-waits-for-create");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let mount = &daemon.mountpoint;
        let file = |bytes: &[u8]| Some(Entry::File(Arc::new(bytes.to_vec())));

        let held = daemon.client.hold_reply_to_next_create();
        std::fs::write(mount.join("c.bin"), b"first").unwrap();
        assert!(wait_until(Duration::from_secs(30), || held.reached()));
        std::fs::rename(mount.join("c.bin"), mount.join("d")).unwrap();
        std::fs::write(mount.join("c.bin"), b"second").unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            daemon
                .pending()
                .is_ok_and(|items| items.iter().any(|op| op.attempts > 0))
        }));
        // Long enough for the waits to run out, were they not waits.
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty())
                    && drive.tree().get("d").cloned() == file(b"first")
                    && drive.tree().get("c.bin").cloned() == file(b"second")
            }),
            "{:?} {:?}",
            daemon.pending(),
            drive.tree()
        );
        assert_eq!(drive.tree().len(), 2, "{:?}", drive.tree());
        assert_eq!(std::fs::read(mount.join("c.bin")).unwrap(), b"second");

        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_write_whose_answer_was_lost_lands_once() {
        // Drive made the revision, but its answer was lost. The retry found
        // the file at a revision it did not know and kept the write as a
        // conflict copy of itself (B155).
        let drive = FakeDrive::new();
        let dir = scratch("lost-revision-reply");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        let path = daemon.mountpoint.join("f.txt");
        let file = |bytes: &[u8]| Some(Entry::File(Arc::new(bytes.to_vec())));

        std::fs::write(&path, b"first").unwrap();
        assert!(wait_until(Duration::from_secs(30), || {
            daemon.pending().is_ok_and(|items| items.is_empty())
                && drive.tree().get("f.txt").cloned() == file(b"first")
        }));
        daemon.client.lose_reply_to_next_revision();
        std::fs::write(&path, b"second").unwrap();
        assert!(
            wait_until(Duration::from_secs(30), || {
                daemon.pending().is_ok_and(|items| items.is_empty())
                    && drive.tree().get("f.txt").cloned() == file(b"second")
            }),
            "{:?} {:?}",
            daemon.pending(),
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

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn half_of_a_wide_folder_unlinked_at_once_leaves_the_other_half() {
        // The acceptance suite's wide directory on Wi-Fi. A create that landed
        // while the folder's listing was on the wire was in neither Drive's
        // answer nor the queue read after it, and a file that was never
        // unlinked dropped out of the listing until the next one (B166).
        let drive = FakeDrive::new();
        let dir = scratch("wide-unlink");
        let daemon = Daemon::start(&dir, drive.client(1, Faults::wifi())).unwrap();
        let root = daemon.mountpoint.join("wide");
        std::fs::create_dir(&root).unwrap();
        let names: Vec<String> = (0..128).map(|i| format!("entry-{i:04}.txt")).collect();
        let in_parallel = |op: &(dyn Fn(&str) + Sync), names: &[String]| {
            std::thread::scope(|scope| {
                for chunk in names.chunks(names.len().div_ceil(8)) {
                    scope.spawn(move || chunk.iter().for_each(|name| op(name)));
                }
            });
        };
        in_parallel(
            &|name| {
                let mut file = std::fs::File::create(root.join(name)).unwrap();
                std::io::Write::write_all(&mut file, name.as_bytes()).unwrap();
                file.sync_all().unwrap();
            },
            &names,
        );
        let listed = |root: &std::path::Path| {
            let mut names: Vec<String> = std::fs::read_dir(root)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        assert_eq!(listed(&root), names);
        in_parallel(
            &|name| std::fs::remove_file(root.join(name)).unwrap(),
            &names[..64],
        );
        assert_eq!(listed(&root), names[64..]);
        let mut settled = None;
        while settled.is_none_or(|at: Instant| at.elapsed() < crate::POLL_INTERVAL * 2) {
            assert_eq!(listed(&root), names[64..]);
            if settled.is_none() && daemon.pending().is_ok_and(|ops| ops.is_empty()) {
                settled = Some(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
