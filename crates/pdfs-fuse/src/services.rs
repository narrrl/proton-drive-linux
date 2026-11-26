//! The long-lived loops a mount runs, each as a [`Service`].
//!
//! Each type here names its thread, says how a stop reaches it and whether the
//! mount can do without it. `mount.rs` builds the list and starts it with
//! [`Services::start`](pdfs_core::lifecycle::Services::start); a stop wakes and
//! joins them in reverse order.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use pdfs_core::lifecycle::{Service, Wake};
use pdfs_core::shutdown::Shutdown;
use pdfs_store::db::Db;

use super::Core;
use super::background::run_local_index;
use super::control::run_control_socket;
use super::events::{EventHub, run_event_sampler};
use super::sync;
use super::transfers::TransferRegistry;

/// One of the threads that send queued changes to Drive. The drain workers wait
/// on one condvar, which a stop has to notify; one notify wakes them all, so
/// only the first worker carries the wake.
pub(crate) struct DrainWorker {
    pub(crate) core: Core,
    pub(crate) index: usize,
}

impl Service for DrainWorker {
    fn name(&self) -> String {
        format!("pdfs-drain-{}", self.index)
    }

    fn wake(&self) -> Option<Wake> {
        let core = self.core.clone();
        (self.index == 0).then(|| Box::new(move || core.wake_drain()) as Wake)
    }

    fn run(self: Box<Self>) {
        self.core.run_pending_drain(self.index == 0);
    }
}

/// The folder-sync engine. It watches every mirror folder, polls the remotes and
/// reconciles on its own thread, never in front of a FUSE call. It waits on its
/// channel, so a stop sends it `Stop`.
///
/// Optional: the mount still serves files without it.
pub(crate) struct SyncEngine {
    pub(crate) syncer: sync::Syncer,
    pub(crate) tx: std::sync::mpsc::Sender<sync::SyncMsg>,
    pub(crate) rx: std::sync::mpsc::Receiver<sync::SyncMsg>,
}

impl Service for SyncEngine {
    fn name(&self) -> String {
        "pdfs-sync".to_owned()
    }

    fn required(&self) -> bool {
        false
    }

    fn wake(&self) -> Option<Wake> {
        let tx = self.tx.clone();
        Some(Box::new(move || {
            let _ = tx.send(sync::SyncMsg::Stop);
        }))
    }

    fn run(self: Box<Self>) {
        let Self { syncer, rx, .. } = *self;
        sync::run(syncer, rx);
    }
}

/// Reconciles leftover `(sync-conflict …)` copies: drops the ones that turned
/// out identical to the live file and surfaces the ones that differ. It
/// enumerates and trashes, so it must never block a FUSE call.
pub(crate) struct ConflictSweep(pub(crate) Core);

impl Service for ConflictSweep {
    fn name(&self) -> String {
        "pdfs-conflict-sweep".to_owned()
    }

    fn run(self: Box<Self>) {
        self.0.run_conflict_sweep_loop();
    }
}

/// Watches the link for the life of the mount: a mount from the cache waits
/// for the network to come back, and an online one can lose it at any moment.
pub(crate) struct OnlineProbe(pub(crate) Core);

impl Service for OnlineProbe {
    fn name(&self) -> String {
        "pdfs-online-probe".to_owned()
    }

    fn run(self: Box<Self>) {
        self.0.run_online_probe();
    }
}

/// Keeps pinned files on this computer through changes made elsewhere, and
/// fetches the ones a failed or missed download left out (`docs/BUGS.md`
/// B282). A check lists folders and downloads files.
pub(crate) struct PinKeeper(pub(crate) Core);

impl Service for PinKeeper {
    fn name(&self) -> String {
        "pdfs-pin-keeper".to_owned()
    }

    fn run(self: Box<Self>) {
        self.0.run_pin_keeper();
    }
}

/// Keeps the launcher prompt's "This computer" index fresh. The walk is
/// I/O-heavy and must never sit in front of a FUSE callback.
pub(crate) struct LocalIndex {
    pub(crate) db: Arc<Db>,
    pub(crate) indexing: Arc<AtomicBool>,
    pub(crate) transfers: Arc<TransferRegistry>,
    pub(crate) mountpoint: PathBuf,
    pub(crate) shutdown: Arc<Shutdown>,
}

impl Service for LocalIndex {
    fn name(&self) -> String {
        "pdfs-localindex".to_owned()
    }

    fn run(self: Box<Self>) {
        let Self {
            db,
            indexing,
            transfers,
            mountpoint,
            shutdown,
        } = *self;
        run_local_index(db, indexing, transfers, mountpoint, shutdown);
    }
}

/// Answers the control socket, one job per connection.
///
/// The listener blocks in `accept`, which only returns when a connection
/// arrives, so a stop connects to it. It connects while the socket still
/// exists, which is the point of the poke, then unlinks it so nothing else can
/// arrive behind it. The listener checks the stop flag after every accept, so
/// a connection that races the unlink cannot put it back to sleep. The
/// connections it handed off are jobs, which a stop joins; a subscription
/// among them waits on its mailbox, so the wake rings every mailbox too.
pub(crate) struct ControlListener {
    pub(crate) core: Core,
    pub(crate) username: String,
    pub(crate) mountpoint: PathBuf,
    pub(crate) listener: UnixListener,
    pub(crate) socket: PathBuf,
}

impl Service for ControlListener {
    fn name(&self) -> String {
        "pdfs-control".to_owned()
    }

    fn wake(&self) -> Option<Wake> {
        let socket = self.socket.clone();
        let events: Arc<EventHub> = self.core.events.clone();
        Some(Box::new(move || {
            let _ = std::os::unix::net::UnixStream::connect(&socket);
            let _ = std::fs::remove_file(&socket);
            events.wake_all();
        }))
    }

    fn run(self: Box<Self>) {
        let Self {
            core,
            username,
            mountpoint,
            listener,
            ..
        } = *self;
        run_control_socket(core, username, mountpoint, listener);
    }
}

/// Publishes what changes without one place to publish it from (status,
/// transfers, sync progress) to the front ends subscribed on the control
/// socket.
///
/// Optional: front ends still get every published change, only not the
/// sampled ones.
pub(crate) struct EventSampler {
    pub(crate) core: Core,
    pub(crate) username: String,
    pub(crate) mountpoint: PathBuf,
}

impl Service for EventSampler {
    fn name(&self) -> String {
        "pdfs-events".to_owned()
    }

    fn required(&self) -> bool {
        false
    }

    fn run(self: Box<Self>) {
        run_event_sampler(self.core, self.username, self.mountpoint);
    }
}

/// Watches the worker pool and answers the systemd watchdog. Its liveness
/// probe is a round trip over the control socket, so it starts after the
/// listener.
///
/// Optional: the mount works without it, only it is no longer diagnosable if
/// it wedges.
pub(crate) struct Supervisor {
    pub(crate) core: Core,
    pub(crate) socket: PathBuf,
}

impl Service for Supervisor {
    fn name(&self) -> String {
        "pdfs-supervisor".to_owned()
    }

    fn required(&self) -> bool {
        false
    }

    fn run(self: Box<Self>) {
        super::supervisor::run(self.core, self.socket);
    }
}

/// Holds off a shutdown while changes exist only here (B66).
///
/// Optional: without it a shutdown is not delayed, and queued changes go out
/// on the next start.
pub(crate) struct Inhibitor(pub(crate) Core);

impl Service for Inhibitor {
    fn name(&self) -> String {
        "pdfs-inhibit".to_owned()
    }

    fn required(&self) -> bool {
        false
    }

    fn run(self: Box<Self>) {
        super::inhibit::run(self.0);
    }
}
