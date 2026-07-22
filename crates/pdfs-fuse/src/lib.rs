//! On-demand FUSE filesystem for Proton Drive.
//!
//! Phase 1 is a read-only placeholder mount: directories are enumerated lazily
//! from the remote on first access and file content is hydrated on `read` via
//! [`ProtonDriveClient::download_range`]. Nothing is mirrored to local disk;
//! every byte is fetched on demand.
//!
//! Phase 2 adds live remote sync: a background task polls the volume event
//! cursor ([`ProtonDriveClient::enumerate_events`]) and pushes invalidations
//! into the kernel via a fuser [`Notifier`], so the cache TTL can be long while
//! remote changes still show up promptly.
//!
//! Phase 3 makes the mount writable. Each file opened for writing gets a
//! `WriteHandle` whose buffer accumulates the full plaintext; on flush/release
//! the buffer is sealed as a new revision via
//! [`ProtonDriveClient::upload_new_revision`] (the SDK uploads whole revisions,
//! not byte ranges). New files are created empty up front so they get a real
//! uid; namespace ops map to `create_folder`, `trash_nodes`, `rename_node` and
//! `move_node`.
//!
//! Phase 4 adds Files-On-Demand pinning. A control socket (see `control`)
//! lets the CLI pin/unpin files; a pinned file's plaintext is downloaded once
//! into the on-disk [`ContentCache`] and every later `read` is served from disk
//! instead of the network. Writes and remote events evict the cache so it never
//! goes stale.
//!
//! Reads of unpinned files no longer hit the network per call: `Core::read_range`
//! fetches and caches [`BLOCK_SIZE`]-aligned blocks, so sequential/sparse reads
//! reuse the on-disk block cache. Writes are disk-backed: each `WriteHandle`
//! stages authored bytes in a scratch file and tracks them with an `Intervals`
//! set, so a multi-GiB write never buffers in RAM and only the untouched
//! remainder of the file is pulled from the remote — lazily, at commit.

use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixListener;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::ReplyXattr;
use fuser::{
    BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, IoctlFlags, LockOwner, MountOption, Notifier, OpenAccMode,
    OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyIoctl, ReplyOpen, ReplyWrite, Request, Session, TimeOrNow, WriteFlags,
};
use pdfs_core::cache::{BLOCK_SIZE, Baseline, ContentCache, StagedWrite};
use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    ActivityEntry, ActivityKind, DirEntry, ErrorKind, LocalHit, PhotoKind, PublicLinkInfo,
    SearchHit, SyncFolderInfo, SyncPhase, SyncProgress, TransferDirection,
};
use pdfs_core::db::{
    Db, LOCAL_VOLUME, OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH, PendingOp, StoredNode,
    StoredSyncFolder, StoredTrash,
};
use pdfs_core::localindex;
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::api::ResponseCode;
use proton_drive_rs::proton_sdk::error::ProtonError;
use proton_drive_rs::proton_sdk::ids::{DriveEventId, LinkId, NodeUid, VolumeId};
use proton_drive_rs::{
    DriveEvent, DriveEventScopeId, MemberRole, Node, NodeKind, ProtonDriveClient,
    ProtonPhotosClient, RevisionReader, ThumbnailType,
};

mod background;
mod control;
use control::run_control_socket;
mod devices;
mod drain;
mod filesystem;
pub use filesystem::ProtonFs;
mod mount;
mod photos;
mod profile;
mod reads;
mod sharing;
mod state;
mod sync;
mod transfers;
mod upload;
mod workers;
use background::{run_event_sync, run_local_index};
pub(crate) use mount::is_stale_mount;
pub use mount::{MountOutcome, mount};
use mount::{SecondaryMount, clear_stale_mount, fuse_connection_id, umount_session_unblocked};
use reads::{ReaderSlot, STREAM_BYPASS_MIN, StreamRing};
use state::{Entry, Intervals, PendingRevision, State, WriteHandle};
use tracing::{debug, error, info, warn};
use transfers::{CountingWriter, JobGuard, OwnedCountingReader, TransferRegistry};
use workers::{FUSE_WORKERS, Lane, Workers};

/// Attribute/entry cache lifetime handed back to the kernel. Long because the
/// Phase 2 event poller actively invalidates changed inodes; without a remote
/// change this is how long the kernel may serve stale metadata.
const TTL: Duration = Duration::from_secs(30);

/// How often the background task polls the remote event cursor.
const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// First and longest delay between probes for the network coming back after an
/// offline mount (offline.md Phase 1). Doubles from min to max: a laptop shut in
/// a bag is the common case, so the steady state must be cheap, while a brief
/// blip should still recover in seconds.
/// Retry backoff for a queued upload, doubling per attempt between these. The
/// floor is short because the common failure is a brief network blip; the
/// ceiling keeps a persistently failing op from spinning.
const DRAIN_BACKOFF_MIN: Duration = Duration::from_secs(2);
const DRAIN_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// How long the drain worker sleeps when it has nothing due. It is woken
/// directly on a new write or a reconnect, so this only bounds how late a
/// backoff can fire.
const DRAIN_IDLE_POLL: Duration = Duration::from_secs(30);
/// Grace period before a queued revision becomes eligible for draining.
///
/// Tools like aria2c preallocate a file (truncate to target size) and then write
/// the real content, sometimes across separate open/close cycles. Without a
/// grace period the first close drains immediately, uploading the preallocated
/// (mostly-zero) content; the second close then finds its baseline stale and
/// creates a conflict copy of itself. Holding the op for a short window gives
/// the follow-up write time to supersede it.
const DRAIN_REVISION_DEBOUNCE: Duration = Duration::from_secs(2);

const ONLINE_PROBE_MIN: Duration = Duration::from_secs(5);
const ONLINE_PROBE_MAX: Duration = Duration::from_secs(300);
/// How long the persisted photos timeline stays good before a page request
/// revalidates it. The SDK hands back the whole timeline at once, so it is stored
/// in the DB and every page is sliced from there; a stale one is still served
/// immediately and refreshed in the background.
const TIMELINE_TTL: Duration = Duration::from_secs(5 * 60);
/// How many photo nodes are resolved per [`ProtonPhotosClient::enumerate_nodes`]
/// call when enriching a refreshed timeline with names and media types (for the
/// Photos / Videos / Raw split). Batched so a large library is a handful of
/// round-trips rather than one request per photo, and bounded so a single call
/// never asks the server to decrypt the whole library at once.
const TIMELINE_ENRICH_CHUNK: usize = 200;
/// The same, for the persisted trash listing. Shorter, because the trash is the
/// one listing a user changes and then immediately looks at — though our own
/// mutations also invalidate it outright, so this only covers other clients.
const TRASH_TTL: Duration = Duration::from_secs(60);

/// `sync_state` keys for the freshness stamps of the two persisted listings, and
/// for whether the account has a photos volume at all (so an account without one
/// doesn't re-ask the server on every page request).
const PHOTOS_SYNCED_MS: &str = "photos_synced_ms";
const PHOTOS_AVAILABLE: &str = "photos_available";
const TRASH_SYNCED_MS: &str = "trash_synced_ms";

/// How stale the local-file index may get before the background scanner rebuilds
/// it. A rescan is a full walk of `$HOME`, so this trades index freshness against
/// disk churn; the scanner also always rebuilds once per daemon start when the
/// index is older than this.
const LOCAL_INDEX_TTL: Duration = Duration::from_secs(15 * 60);

/// How often the scanner thread wakes to check whether the index went stale.
const LOCAL_INDEX_CHECK: Duration = Duration::from_secs(60);

/// The FUSE root inode is always 1.
const ROOT_INO: u64 = 1;

/// Parent inode for a persisted node whose parent row is missing from the DB.
/// No folder carries this inode, so such a node is listed by nobody and stays
/// inert until a live enumeration re-parents it.
const ORPHAN_INO: u64 = 0;

/// Extended attribute exposing the small server-side thumbnail of a file.
const XATTR_THUMBNAIL: &str = "user.proton.thumbnail";
/// Extended attribute exposing the larger server-side preview image of a file.
const XATTR_PREVIEW: &str = "user.proton.preview";

/// How many "this file has no thumbnail" answers [`Core::thumbnail`] remembers
/// before dropping the lot and re-learning them. Sized to cover a large browsing
/// session; each entry is a uid, a type tag and an mtime.
const MAX_THUMBNAIL_MISSES: usize = 8192;

/// How many nodes one size-upgrade request covers.
///
/// Matches the SDK's own `MAX_BATCH_COUNT`, so a chunk is exactly one request:
/// chunking smaller would add round trips, larger would be split anyway and
/// delay the waiters this chunking exists to release (bugs.md B14).
const SIZE_UPGRADE_CHUNK: usize = 150;

#[derive(Clone)]
struct Core {
    client: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    state: Arc<Mutex<State>>,
    cache: Arc<ContentCache>,
    /// Open [`RevisionReader`]s keyed by node, so the block fetches of a file
    /// resolve its keys and block table once instead of once per block.
    /// Validated by `(mtime, size)` exactly like the content cache, and bounded
    /// by [`MAX_OPEN_READERS`].
    readers: Arc<Mutex<HashMap<NodeUid, ReaderSlot>>>,
    /// Blocks of files streaming past the on-disk cache, held in memory so a
    /// player's small sequential reads are served from the last few 4 MiB blocks
    /// instead of re-downloading each one. See [`StreamRing`].
    stream_ring: Arc<Mutex<StreamRing>>,
    /// Threads that serve the FUSE handlers which touch the network, so the
    /// session's dispatch loop stays free to answer cheap metadata calls while a
    /// cold read is on the wire. See [`Workers`].
    workers: Arc<Workers>,
    /// Unified SQLite metadata cache: the persistence layer behind the in-memory
    /// `State` maps. Every mutation writes through here, and the maps rehydrate
    /// from it on mount (plan.md P1).
    db: Arc<Db>,
    /// False while the API is unreachable and we are serving the cached tree
    /// (offline.md Phase 1). Set by the probe thread; read by front-ends through
    /// `Response::Status` so the UI can say so rather than leaving the user to
    /// infer it from a wall of EIO.
    online: Arc<AtomicBool>,
    /// Writes accepted from the kernel but not yet uploaded, keyed by node
    /// (offline.md Phase 3). The in-memory face of the `pending_op` table, from
    /// which it is rebuilt on mount.
    ///
    /// Two things read it: [`Core::read_range`], because until the op drains the
    /// staged blob *is* the file's content and the remote still holds the old
    /// revision; and the drain worker, which performs the uploads.
    pending: Arc<Mutex<HashMap<NodeUid, PendingRevision>>>,
    /// Nudges the drain worker: set true and notify to have it re-examine the
    /// queue instead of waiting out its backoff.
    drain_wake: Arc<(Mutex<bool>, Condvar)>,
    /// True while a background refresh of the photos timeline (resp. the trash) is
    /// already running, so a burst of page requests against a stale listing kicks
    /// off one refresh rather than one per request.
    timeline_refreshing: Arc<AtomicBool>,
    trash_refreshing: Arc<AtomicBool>,
    /// Photos whose missing thumbnail is being generated right now. A tile that is
    /// still on screen asks for its thumbnail again every few seconds, and each of
    /// those downloads is a full-size photo — so an in-flight uid is never started
    /// twice.
    thumb_gen: Arc<Mutex<HashSet<NodeUid>>>,
    /// Nodes the remote has told us have *no* thumbnail of a given type, keyed by
    /// `(uid, thumbnail type)` and holding the mtime the answer was learned at.
    ///
    /// Absence has to be cached or it costs a round trip every time it is asked
    /// for, and it is asked for constantly: an `ls -l` from an xattr-aware lister
    /// issues a `getxattr` per advertised name per entry, so a 65-file directory
    /// of videos re-probed 130 times per listing at ~186 ms each (B5). The mtime
    /// is the validity tag — a new revision may well have a thumbnail — matching
    /// how [`ContentCache::read_thumbnail`] validates the positive side.
    no_thumbnail: Arc<Mutex<HashMap<(NodeUid, i32), i64>>>,
    /// Folders whose listing was enumerated cheaply and is having its file sizes
    /// filled in right now, so a burst of `stat`s over a fresh listing starts one
    /// upgrade rather than one per entry. See [`Core::spawn_size_upgrade`].
    /// Size upgrades currently running, per folder inode. A `getattr` that
    /// needs a real size waits on the entry rather than issuing its own fetch —
    /// `ls -l` of a folder is one `getattr` per file, and they must collapse
    /// onto a single batch (bugs.md B14).
    size_upgrades: Arc<Mutex<HashMap<u64, Arc<SizeUpgrade>>>>,
    /// This mount's kernel notification channel, for telling the kernel to drop
    /// metadata it has cached. Set once the session exists — which is *after*
    /// the `Core` it is built from, hence the cell.
    ///
    /// Per mount, not per daemon: each on-demand fork runs its own session over
    /// its own inode space, so notifying through the primary mount's channel
    /// would name inodes that session has never heard of. [`Core::fork_state`]
    /// gives each fork an empty cell of its own.
    notifier: Arc<OnceLock<Notifier>>,
    /// In-flight upload/download progress, served to `GetQueueStatus`. Shared
    /// across the FUSE session and the control-socket task.
    transfers: Arc<TransferRegistry>,
    /// True while the background scanner is rebuilding the local-file index, so
    /// `SearchLocal` can tell a front-end "still indexing" apart from "no match".
    indexing: Arc<AtomicBool>,
    /// Live per-folder sync progress, keyed by sync-folder id, so `ListSyncFolders`
    /// can say what a pass is doing rather than just "syncing". An entry exists
    /// only while that folder's reconcile pass is running.
    sync_progress: Arc<Mutex<HashMap<i64, SyncProgress>>>,
    /// Channel to the folder-sync engine (devices.md Phase 2): nudges it to
    /// reconcile a folder, reconcile everything, or re-scan its watch set.
    sync_tx: std::sync::mpsc::Sender<sync::SyncMsg>,
    /// Secondary FUSE sessions for `ondemand` sync folders, keyed by sync-folder
    /// id (devices.md Phase 3). Each is a `ProtonFs` rooted at the folder's remote
    /// node, mounted over its local path, sharing this Core's client/cache/db but
    /// with its own inode space (`fork_state`). Held so we can unmount on toggle
    /// back to `mirror` and on daemon shutdown. The `u32` is the FUSE connection
    /// id (see [`fuse_connection_id`]), captured at mount time so teardown can
    /// abort a mid-transfer connection instead of blocking on it.
    mounts: Arc<Mutex<HashMap<i64, SecondaryMount>>>,
    /// Per-sync-folder locks, held for a whole reconcile pass and for a whole
    /// mode switch. A `mirror→ondemand` flip evicts the local tree and mounts
    /// FUSE over it, so it must never overlap a pass that is walking and
    /// uploading that same tree — the engine would upload files as they vanish
    /// and then walk the FUSE mount as if it were local.
    sync_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
}

/// Why [`Core::apply_sync_folder_mode`] did not switch a folder. The two cases are
/// answered very differently: `NotNow` is the normal state of a folder that is busy
/// or has local changes still to push, and the caller queues the request; `Failed`
/// is a real fault the user has to hear about.
enum SwitchBlocked {
    /// The folder is mid-pass, or not yet safe to switch. Try again after a pass.
    NotNow,
    /// The switch was attempted and broke.
    Failed(String),
}

impl Core {
    /// Rehydrate the in-memory `State` maps from the DB on mount, so a cold
    /// start serves previously-seen metadata (stable inodes, instant listings)
    /// without re-hitting the API. The root inode is already installed by
    /// [`ProtonFs::new`]; this fills in every other persisted node and rebuilds
    /// the child listings of folders the DB records as fully enumerated.
    fn hydrate(&self) {
        let stored = match self.db.load_all() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "db load_all failed; mounting cold");
                return;
            }
        };
        if stored.is_empty() {
            return;
        }
        // A write queued by a previous run carries the optimistic size the file
        // grew to, which the sealed remote revision does not yet reflect. The DB
        // node still holds the pre-write `claimed_size` (often 0 for a fresh
        // file), so without this a queued 500 MB file `stat`s as 0 bytes until
        // the drain lands — reads still serve from the staged blob, but `ls`
        // lying about the size reads as data loss. `hydrate_pending` ran before
        // us, so the pending map is populated; snapshot it here rather than hold
        // both locks at once.
        let pending_sizes: HashMap<NodeUid, u64> = {
            let pending = self.pending.lock();
            pending
                .iter()
                .map(|(uid, pr)| (uid.clone(), pr.meta.len))
                .collect()
        };
        let mut st = self.state.lock();

        // Pass 1: assign a stable inode to every uid (root is already mapped).
        for sn in &stored {
            if st.by_uid.contains_key(&sn.node.uid) {
                continue;
            }
            let ino = st.next_ino;
            st.next_ino += 1;
            st.by_uid.insert(sn.node.uid.clone(), ino);
        }

        // Pass 2: materialize entries, resolving each parent's inode by uid.
        // Track folders flagged complete so their listings rebuild in pass 3.
        let mut listed_dirs: Vec<u64> = Vec::new();
        for sn in stored {
            let StoredNode { mut node, listed } = sn;
            let Some(&ino) = st.by_uid.get(&node.uid) else {
                continue;
            };
            // Re-apply a queued write's optimistic size so `stat` matches what
            // reads (served from the staged blob) already return.
            if let Some(&len) = pending_sizes.get(&node.uid)
                && let NodeKind::File { claimed_size, .. } = &mut node.kind
            {
                *claimed_size = Some(len as i64);
            }
            if listed && node.is_folder() {
                listed_dirs.push(ino);
            }
            // The root entry is owned by `ProtonFs::new`; don't overwrite it.
            if ino == ROOT_INO {
                continue;
            }
            // A node whose parent row never made it to disk must not be adopted
            // by the root: it would surface as a phantom top-level entry.
            let parent = node
                .parent_uid
                .as_ref()
                .and_then(|p| st.by_uid.get(p).copied())
                .unwrap_or(ORPHAN_INO);
            let uid = node.uid.clone();
            st.entries.insert(
                ino,
                Entry {
                    uid,
                    parent,
                    node,
                    lookup_count: 1,
                    open_count: 0,
                    unlinked: false,
                },
            );
        }

        // Pass 3: rebuild child listings for fully-enumerated folders. The root
        // is its own parent (that is what `..` resolves to), so it would match
        // its own filter; a directory listed inside itself makes the kernel fail
        // the lookup with EIO, taking the whole listing down with it.
        for dir_ino in listed_dirs {
            let kids: Vec<u64> = st
                .entries
                .iter()
                .filter(|&(&ino, e)| ino != dir_ino && e.parent == dir_ino && !e.node.trashed)
                .map(|(&ino, _)| ino)
                .collect();
            st.children.insert(dir_ino, kids);
        }
        info!(nodes = st.entries.len(), "hydrated metadata cache from db");
    }

    /// Rebuild the in-memory pending map from the `pending_op` table on mount
    /// (offline.md Phase 3).
    ///
    /// A queued write survives a restart — that is the point of persisting it —
    /// so until the drain worker gets to it, reads of that file must still come
    /// from its staged blob rather than the remote's older revision.
    ///
    /// A row whose blob has gone missing is dropped: there is nothing left to
    /// upload, and keeping it would fail forever.
    fn hydrate_pending(&self) {
        let ops = match self.db.pending_ops() {
            Ok(ops) => ops,
            Err(e) => {
                error!(error = %e, "loading pending uploads failed");
                return;
            }
        };
        let mut map = self.pending.lock();
        let mut restored = 0usize;
        for op in ops {
            let Some(uid) = parse_node_uid(&op.uid) else {
                error!(uid = %op.uid, id = op.id, "pending op has an unparseable uid; dropping");
                let _ = self.db.delete_op(op.id);
                continue;
            };
            restored += 1;
            // Only a revision must have a blob. A create carries none until
            // something is written to it (`touch` offline is a legitimate op
            // with nothing to serve), and a rename or trash never has one. All
            // still have to be replayed, so only the blob — if any — is checked.
            if op.blob_path.is_none() && op.kind != OP_REVISION {
                continue;
            }
            let parsed = op
                .meta_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<StagedWrite>(j).ok())
                .zip(op.blob_path.as_deref().map(PathBuf::from));
            let Some((meta, path)) = parsed else {
                error!(uid = %op.uid, id = op.id, "pending op is unreadable; dropping");
                self.drop_unrecoverable_op(&op, &uid);
                restored -= 1;
                continue;
            };
            if !path.exists() {
                error!(%uid, path = %path.display(), "staged blob is gone; dropping pending op");
                self.drop_unrecoverable_op(&op, &uid);
                restored -= 1;
                continue;
            }
            map.insert(uid, PendingRevision { path, meta });
        }
        if restored > 0 {
            info!(count = restored, "restored pending ops");
        }
    }

    /// Queue the writes that an unclean shutdown caught between `fsync(2)` and
    /// `close(2)`.
    ///
    /// `fsync` promises the bytes survive a crash, but the queueing that makes a
    /// write outlive the daemon happens at `release`. A crash in between used to
    /// lose the data outright, because the scratch directory is cleared at open.
    /// Now `fsync` leaves a sidecar, `ContentCache::open` moves those blobs to
    /// `recovery/`, and this walks them into the same staging + queued-op path a
    /// normal release takes.
    ///
    /// Runs after [`hydrate_pending`](Self::hydrate_pending), which is what makes
    /// the incomplete-blob check in [`enqueue_staged_write`](Self::enqueue_staged_write)
    /// meaningful: a recovered partial write whose earlier write is still queued
    /// must not gap-fill from a remote revision that no longer describes the
    /// file. With `pending` already loaded, that case is detected and the bytes
    /// are parked rather than mis-filled.
    ///
    /// Failure is per-write and never fatal: a node that no longer exists, or an
    /// op that cannot be queued, leaves its blob in `recovery/` for the next run
    /// (and for a human) instead of taking the mount down.
    fn recover_fsynced_writes(&self) {
        let recovered = self.cache.recovered_writes();
        if recovered.is_empty() {
            return;
        }
        let mut queued = 0usize;
        for (blob, meta) in recovered {
            let Some(uid) = parse_node_uid(&meta.uid) else {
                error!(uid = %meta.uid, "recovered write has an unparseable uid; keeping bytes");
                continue;
            };

            // If the write is incomplete and an earlier edit is still queued, merging it
            // now would use the wrong remote base. enqueue_staged_write would abandon it
            // to staging; keep it in recovery instead so the drain loop can queue it
            // once the queue clears.
            if !meta.complete && self.pending.lock().contains_key(&uid) {
                continue;
            }

            // The inode is only used to stamp the in-memory tree with the new
            // size, and nothing is interned this early — `hydrate` reads the
            // size back off `pending` when the node is first looked up, so 0
            // (no such inode) is correct rather than merely tolerable.
            match self.enqueue_staged_write(&uid, 0, &blob, meta) {
                Ok(()) => {
                    self.cache.discard_recovered(&blob);
                    queued += 1;
                }
                Err(e) => {
                    error!(%uid, blob = %blob.display(), error = ?e,
                           "cannot queue a recovered write; bytes kept for the next run");
                    // Some failures (a partial write parked by
                    // `stage_orphaned_write`) still consume the blob. Its sidecar
                    // would then describe nothing, so retire it — the bytes are
                    // in staging, which is where a human looks for them.
                    if !blob.exists() {
                        self.cache.discard_recovered(&blob);
                    }
                }
            }
        }
        if queued > 0 {
            info!(
                count = queued,
                "recovered fsynced writes from an unclean shutdown"
            );
        }
    }

    /// Discard an op that can never be performed, because the bytes it was to
    /// upload are gone from staging (something outside the daemon deleted them).
    ///
    /// For a node that only ever existed locally, the placeholder goes too. Its
    /// content is unrecoverable and nothing will ever mint it a real uid, so
    /// leaving the row would strand a file in the tree that can be listed but
    /// never read and never uploaded.
    fn drop_unrecoverable_op(&self, op: &PendingOp, uid: &NodeUid) {
        let _ = self.db.delete_op(op.id);
        if is_local_uid(uid) {
            error!(%uid, name = op.name.as_deref().unwrap_or("?"),
                   "discarding a node whose only copy was lost");
            if let Err(e) = self.db.delete_node(uid) {
                warn!(%uid, error = %e, "db delete_node failed for lost local node");
            }
        }
    }

    /// Poll for the API becoming reachable again after an offline mount, then
    /// flip `online` and refresh the root (offline.md Phase 1). Runs on its own
    /// thread and returns once we are back online: nothing sets `online` false
    /// again, because a mount that has been online once keeps its live event
    /// sync, which does its own retrying.
    ///
    /// Backs off to [`ONLINE_PROBE_MAX`] rather than hammering a fixed interval —
    /// a laptop can sit offline for days, and each probe is a real API round trip.
    fn run_online_probe(&self) {
        let mut delay = ONLINE_PROBE_MIN;
        loop {
            std::thread::sleep(delay);
            match self.rt.block_on(self.client.get_my_files_folder()) {
                Ok(root) => {
                    {
                        let mut st = self.state.lock();
                        if let Some(e) = st.entries.get_mut(&ROOT_INO) {
                            e.node = root.clone();
                        }
                    }
                    if let Err(e) = self.db.upsert_node(&root) {
                        warn!(error = %e, "refresh root after reconnect failed");
                    }
                    self.online.store(true, Ordering::Relaxed);
                    // Anything written while offline is queued and waiting on
                    // exactly this.
                    self.wake_drain();
                    info!("back online");
                    return;
                }
                Err(e) => {
                    debug!(error = %e, ?delay, "online probe failed; still offline");
                    delay = (delay * 2).min(ONLINE_PROBE_MAX);
                }
            }
        }
    }

    /// Whether `ino`'s listing is already in memory, i.e. whether
    /// [`Core::ensure_children`] would return without touching the network.
    /// Lets a handler decide between answering inline and handing off to a
    /// worker, at the cost of one uncontended map lookup.
    fn children_cached(&self, ino: u64) -> bool {
        self.state.lock().children.contains_key(&ino)
    }

    /// Re-apply the optimistic size of any queued write to `nodes`.
    ///
    /// A node that arrives from the remote (or from its DB row) carries the size
    /// of the revision the *server* holds, which for a file with a write still
    /// queued is the pre-write size — often 0 for a file created moments ago.
    /// Interning it as-is silently reverts the optimistic size that
    /// `record_pending_write` stamped, and a file that stats as 0 bytes is a file
    /// the kernel will not issue a single `read` for: `cat` prints nothing and
    /// the staged blob that `read_range` would have served is never asked for.
    /// That reads as data loss even though nothing is lost (B11).
    ///
    /// [`Core::hydrate`] does the same thing for the restart case. This covers
    /// every *live* re-enumeration — which is what a rename or move triggers,
    /// since both invalidate the listings they touch.
    ///
    /// Snapshots the pending map and returns before any caller takes the state
    /// lock: no site in the daemon holds `pending` and `state` at once, and this
    /// is not the place to become the first.
    fn stamp_pending_sizes(&self, nodes: &mut [Node]) {
        let sizes: HashMap<NodeUid, u64> = {
            let pending = self.pending.lock();
            if pending.is_empty() {
                return;
            }
            pending
                .iter()
                .map(|(uid, pr)| (uid.clone(), pr.meta.len))
                .collect()
        };
        apply_pending_sizes(nodes, &sizes);
    }

    /// Enumerate `ino`'s children from the remote and cache them. No-op if the
    /// directory has already been listed. Network I/O happens without the lock
    /// held so concurrent metadata reads aren't blocked behind a fetch.
    fn ensure_children(&self, ino: u64) -> Result<(), Errno> {
        let folder_uid = {
            let st = self.state.lock();
            if st.children.contains_key(&ino) {
                return Ok(());
            }
            match st.entries.get(&ino) {
                Some(e) => e.uid.clone(),
                None => return Err(Errno::ENOENT),
            }
        };

        // Offline fast path: a folder the DB still records as fully enumerated
        // can be rebuilt from disk without hitting the API, even if its listing
        // was trimmed from the hot cache mid-run.
        match self.db.children_if_listed(&folder_uid) {
            Ok(Some(mut nodes)) => {
                // Before the lock: a DB row carries the size the server last
                // sealed, which a queued write is ahead of (B11).
                self.stamp_pending_sizes(&mut nodes);
                let mut st = self.state.lock();
                if st.children.contains_key(&ino) {
                    return Ok(());
                }
                let mut child_inos = Vec::with_capacity(nodes.len());
                let mut needs_size = Vec::new();
                for node in nodes {
                    if node.trashed || node.uid == folder_uid {
                        continue;
                    }
                    if matches!(
                        &node.kind,
                        NodeKind::File {
                            claimed_size: None,
                            ..
                        }
                    ) {
                        needs_size.push(node.uid.clone());
                    }
                    child_inos.push(st.intern_from_db(ino, node));
                }
                st.children.insert(ino, child_inos);
                drop(st);
                // Rows persisted from a cheap enumeration whose upgrade never
                // ran (a restart in between, say) still owe their real sizes.
                self.spawn_size_upgrade(ino, needs_size);
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => warn!(%folder_uid, error = %e, "db children_if_listed failed"),
        }

        let uids = self
            .rt
            .block_on(self.client.enumerate_folder_children_node_uids(&folder_uid))
            .map_err(|e| {
                error!(%folder_uid, error = %e, "enumerate folder children failed");
                Errno::EIO
            })?;
        // Cheap enumeration: `Light` skips unlocking each *file's* node key,
        // which is an S2K derivation per file and was ~74% of the cost of a cold
        // listing (B12 — measured with `perf`, 64% of cycles in SHA-256 alone).
        // Folders are unlocked either way; their keys are what the children are
        // decrypted with, so the walk cannot proceed without them.
        //
        // The price is that files come back without a `claimed_size`, so
        // `node_size` falls back to the *ciphertext* size until
        // `spawn_size_upgrade` below fills the real one in.
        let mut nodes = self
            .rt
            .block_on(self.client.enumerate_nodes_light(&uids))
            .map_err(|e| {
                error!(%folder_uid, error = %e, "enumerate nodes failed");
                Errno::EIO
            })?;
        // Same as the DB path above: the remote's size for a file with a write
        // still queued is the pre-write one (B11).
        self.stamp_pending_sizes(&mut nodes);

        let mut st = self.state.lock();
        // Lost the race? Another thread already populated it.
        if st.children.contains_key(&ino) {
            return Ok(());
        }
        let mut child_inos = Vec::with_capacity(nodes.len());
        let filtered_nodes: Vec<Node> = nodes
            .into_iter()
            .filter(|node| !node.trashed && node.uid != folder_uid)
            .collect();
        // Files whose real size the cheap enumeration could not read. Collected
        // before interning so the upgrade below has the uids without re-walking.
        let needs_size: Vec<NodeUid> = filtered_nodes
            .iter()
            .filter(|n| {
                matches!(
                    &n.kind,
                    NodeKind::File {
                        claimed_size: None,
                        ..
                    }
                )
            })
            .map(|n| n.uid.clone())
            .collect();
        let inos = st.intern_batch(ino, filtered_nodes);
        child_inos.extend(inos);
        st.children.insert(ino, child_inos);
        // Record the listing as complete so a later restart (or a trimmed hot
        // cache) can rebuild it from the DB without the API.
        if let Err(e) = self.db.set_listed(&folder_uid, true) {
            warn!(%folder_uid, error = %e, "db set_listed(true) failed");
        }
        drop(st);
        self.spawn_size_upgrade(ino, needs_size);
        Ok(())
    }

    /// Resolve the real size of every file in `parent`'s listing still carrying
    /// a provisional one, returning when they are in `state`.
    ///
    /// Called when a `stat` lands on such a file. That covers the paths
    /// [`Core::ensure_children`] cannot: a listing rebuilt from the DB, or one
    /// restored by [`Core::hydrate`] on mount, whose rows were persisted before
    /// an earlier upgrade had a chance to run. Gathers the whole folder rather
    /// than the one file asked for, because a `stat` of one entry in a listing
    /// almost always means a `stat` of all of them.
    fn upgrade_sizes_for_parent(&self, ino: u64, uid: &NodeUid, parent: u64) {
        let provisional = |e: &Entry| {
            matches!(
                &e.node.kind,
                NodeKind::File {
                    claimed_size: None,
                    ..
                }
            )
        };
        let (key, mut missing): (u64, Vec<NodeUid>) = {
            let st = self.state.lock();
            match st.children.get(&parent) {
                // The listing is resident: batch the whole folder under its
                // inode, so the rest of an `ls -l` rides along on this fetch.
                Some(kids) => (
                    parent,
                    kids.iter()
                        .filter_map(|k| st.entries.get(k))
                        .filter(|e| provisional(e))
                        .map(|e| e.uid.clone())
                        .collect(),
                ),
                // It is not, and returning here is what let a provisional size
                // reach the caller anyway: a rename invalidates its parents'
                // listings, so a freshly renamed file always landed in this
                // branch. Resolve the single node instead, keyed by its own
                // inode — folder and file inodes share one space, so the two
                // single-flight keys cannot collide. (Same shape as B4: an
                // early return that assumed the hot cache was authoritative.)
                None => (ino, Vec::new()),
            }
        };
        // The node that was actually asked about is never optional, whichever
        // branch produced the batch.
        if !missing.iter().any(|u| u == uid) {
            missing.push(uid.clone());
        }
        // Blocking, not spawning: the caller is a `getattr` that must not answer
        // with a provisional size (bugs.md B14).
        self.upgrade_sizes(key, missing, Some(ino));
    }

    /// Fill in the true sizes of files a `Light` enumeration returned without
    /// one, on a worker, after the listing has already been served.
    ///
    /// This is the other half of the split in [`Core::ensure_children`]. The
    /// listing itself needs only names and parentage, so it is served from the
    /// cheap enumeration immediately; the S2K-per-file work that produces
    /// `claimed_size` happens here, off the path the user is waiting on.
    ///
    /// **Sizes are provisional until this lands.** `node_size` falls back to
    /// `total_size_on_storage`, the *ciphertext* size, which is slightly larger
    /// than the real one. Reads are unaffected — the revision reader carries its
    /// own authoritative size — so this is a cosmetic discrepancy in `stat` that
    /// closes within a round trip, not a repeat of B11 (which reported **0** and
    /// so suppressed reads entirely).
    ///
    /// Single-flight per folder: a `stat` of every entry in a fresh listing is
    /// the normal case, and each one must not start its own upgrade.
    fn spawn_size_upgrade(&self, folder_ino: u64, uids: Vec<NodeUid>) {
        self.upgrade_sizes(folder_ino, uids, None);
    }

    /// Start (or join) the size upgrade for `key`, and — when `waiting_for` names
    /// an inode — block until that node's real size has landed.
    ///
    /// Single-flight per `key`. The fetch runs on its own thread rather than on
    /// the [`Workers`] pool: callers wait on `Lane::Meta`, so a batch queued onto
    /// that lane could have a wide enough `ls -l` fill it with threads waiting
    /// for a job that can never be scheduled. `Lane::Transfer` would swap that
    /// for starvation behind bulk reads. One short-lived thread per folder,
    /// bounded by the single-flight, avoids both.
    fn upgrade_sizes(&self, key: u64, uids: Vec<NodeUid>, waiting_for: Option<u64>) {
        if uids.is_empty() {
            return;
        }
        let slot = {
            let mut in_flight = self.size_upgrades.lock();
            match in_flight.get(&key) {
                // Someone else is already fetching this folder; their batch
                // covers us, so just wait on it.
                Some(existing) => existing.clone(),
                None => {
                    let slot = Arc::new(SizeUpgrade::default());
                    in_flight.insert(key, slot.clone());
                    let core = self.clone();
                    let worker = slot.clone();
                    std::thread::spawn(move || {
                        core.run_size_upgrade(key, uids, &worker);
                    });
                    slot
                }
            }
        };
        let Some(ino) = waiting_for else {
            return;
        };
        slot.wait_for(|| self.size_is_real(ino));
    }

    /// Fetch `uids` in chunks, applying and announcing each one as it lands.
    ///
    /// Chunked so a waiter is released as soon as *its* file is resolved. A
    /// single 793-node batch took ~80 s, which outran [`SizeUpgrade::WAIT`] and
    /// put provisional sizes back in front of callers — the bug this was
    /// supposed to fix (bugs.md B14).
    fn run_size_upgrade(&self, key: u64, uids: Vec<NodeUid>, slot: &SizeUpgrade) {
        for chunk in uids.chunks(SIZE_UPGRADE_CHUNK) {
            let result = self.rt.block_on(self.client.enumerate_nodes(chunk));
            self.apply_size_upgrade(key, result);
            slot.chunk_done();
        }
        // Both of these must happen however the loop ended: a folder whose
        // upgrade failed has to be retryable, and its waiters released.
        self.size_upgrades.lock().remove(&key);
        slot.finish();
    }

    /// Whether `ino` has a real size — the condition a waiter is waiting on.
    /// A node that vanished counts as resolved; there is nothing left to wait
    /// for, and its caller will find the `ENOENT` for itself.
    fn size_is_real(&self, ino: u64) -> bool {
        let st = self.state.lock();
        st.entries.get(&ino).is_none_or(|e| {
            !matches!(
                &e.node.kind,
                NodeKind::File {
                    claimed_size: None,
                    ..
                }
            )
        })
    }

    /// Adopt the sizes a completed upgrade fetched. Split from
    /// [`Core::upgrade_sizes`] so the single-flight bookkeeping there has one
    /// exit path rather than one per early return.
    fn apply_size_upgrade(&self, folder_ino: u64, result: Result<Vec<Node>, ProtonError>) {
        let core = self;
        {
            let mut nodes = match result {
                Ok(nodes) => nodes,
                Err(e) => {
                    // Not fatal: the listing stands, sizes just stay provisional
                    // until something invalidates and re-enumerates it.
                    warn!(folder_ino, error = %e, "size upgrade failed; sizes stay provisional");
                    return;
                }
            };
            // A queued write is ahead of anything the server can report, so its
            // optimistic size must survive this just as it survives a re-listing
            // (B11).
            core.stamp_pending_sizes(&mut nodes);
            let mut changed: Vec<u64> = Vec::new();
            let mut st = core.state.lock();
            for node in nodes {
                // Only adopt the size. Re-interning wholesale would also adopt a
                // name or parent that a rename/move may have changed locally
                // while this was in flight, undoing it.
                let Some(&ino) = st.by_uid.get(&node.uid) else {
                    continue;
                };
                changed.push(ino);
                let NodeKind::File { claimed_size, .. } = &node.kind else {
                    continue;
                };
                let (Some(size), Some(entry)) = (*claimed_size, st.entries.get_mut(&ino)) else {
                    continue;
                };
                if let NodeKind::File { claimed_size, .. } = &mut entry.node.kind {
                    *claimed_size = Some(size);
                }
            }
            let updated: Vec<Node> = st
                .children
                .get(&folder_ino)
                .map(|kids| {
                    kids.iter()
                        .filter_map(|k| st.entries.get(k).map(|e| e.node.clone()))
                        .collect()
                })
                .unwrap_or_default();
            drop(st);
            if let Err(e) = core.db.upsert_nodes(&updated) {
                warn!(folder_ino, error = %e, "persisting upgraded sizes failed");
            }
            // Without this the corrected size is invisible for the length of the
            // attr TTL: the kernel answers `stat` from the provisional attrs it
            // cached while the listing was being served, so `ls -l` reports the
            // ciphertext size for up to 30 s even though the daemon has had the
            // real one all along. Notify *after* the DB write, so a re-`getattr`
            // provoked by the invalidation cannot race the persistence.
            if let Some(notifier) = core.notifier.get() {
                for ino in changed {
                    let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
                }
            }
            debug!(folder_ino, files = updated.len(), "filled in listing sizes");
        }
    }

    /// Resolve a child `name` within `parent` to its `(inode, uid)`, ensuring
    /// the parent's listing is cached first.
    fn lookup_child(&self, parent: u64, name: &str) -> Result<(u64, NodeUid), Errno> {
        self.ensure_children(parent)?;
        let st = self.state.lock();
        st.children
            .get(&parent)
            .and_then(|kids| {
                kids.iter().copied().find_map(|ino| {
                    st.entries
                        .get(&ino)
                        .filter(|e| e.node.name == name)
                        .map(|e| (ino, e.uid.clone()))
                })
            })
            .ok_or(Errno::ENOENT)
    }

    /// Walk a mountpoint-relative path to its `(inode, uid)`, enumerating each
    /// directory on the way as needed. Leading `/` and `.` components are
    /// ignored; `..` is rejected.
    fn resolve_path(&self, rel: &Path) -> Result<(u64, NodeUid), Errno> {
        let mut ino = ROOT_INO;
        let mut uid = {
            let st = self.state.lock();
            st.entries
                .get(&ROOT_INO)
                .map(|e| e.uid.clone())
                .ok_or(Errno::ENOENT)?
        };
        for comp in rel.components() {
            match comp {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => {
                    let (child_ino, child_uid) = self.lookup_child(ino, &name.to_string_lossy())?;
                    ino = child_ino;
                    uid = child_uid;
                }
                _ => return Err(Errno::EINVAL),
            }
        }
        Ok((ino, uid))
    }

    /// [`resolve_path`](Self::resolve_path) for the request-serving side.
    ///
    /// The kernel-facing resolver answers in `Errno` because that is what the
    /// FUSE reply needs. A front-end needs the opposite: `{:?}` of a libc
    /// constant is not something to show a user, and "does not exist" and "the
    /// API is down" want different copy and different buttons. This is the one
    /// place that knows enough to tell them apart, so it is the place that does.
    fn resolve(&self, rel: &Path) -> CoreResult<(u64, NodeUid)> {
        self.resolve_path(rel)
            .map_err(|e| self.errno_error(e, &format!("could not resolve {}", rel.display())))
    }

    /// Classify a failure that arrived as an `Errno`.
    ///
    /// The internal paths speak `Errno` because they also serve FUSE, where a
    /// number is the whole vocabulary. Everything crossing the control socket
    /// has to be turned back into something a person can read, and this is the
    /// one place that knows how — a call site holding only an `Errno` has
    /// already lost the context needed to say what went wrong.
    ///
    /// `Errno` is neither `PartialEq` nor structural-match, so this compares
    /// raw codes rather than the `libc` constants.
    fn errno_error(&self, e: Errno, what: &str) -> CoreError {
        match e.code() {
            libc::ENOENT => CoreError::not_found(format!("{what}: no such file or folder")),
            libc::EACCES | libc::EPERM => CoreError::denied(format!("{what}: not allowed")),
            // These paths walk the tree lazily, so a cold node needs the API.
            // Offline that surfaces as EIO, which on its own would read to the
            // user as a broken file rather than a missing network.
            libc::EIO if !self.online.load(Ordering::Relaxed) => CoreError::offline(),
            libc::EINVAL => CoreError::invalid(format!("{what}: not a usable path")),
            libc::ENOSPC => CoreError::new(ErrorKind::Quota, format!("{what}: out of space")),
            libc::EEXIST => CoreError::conflict(format!("{what}: already exists")),
            libc::ENOTEMPTY => CoreError::conflict(format!("{what}: folder is not empty")),
            _ => CoreError::internal(format!("{what}: {e:?}")),
        }
    }

    /// Fetch a single node's current metadata from the remote.
    fn fetch_node(&self, uid: &NodeUid) -> Result<Node, Errno> {
        match self.fetch_node_remote(uid) {
            Ok(Some(node)) => Ok(node),
            Ok(None) => Err(Errno::ENOENT),
            Err(e) => {
                error!(%uid, error = %e, "enumerate node failed");
                Err(Errno::EIO)
            }
        }
    }

    /// [`Core::fetch_node`] without the collapse to an `Errno`, for the drain:
    /// resolving a conflict turns on *why* a call failed, and "the node is not
    /// there" (`Ok(None)`) is a different outcome from "we could not ask".
    fn fetch_node_remote(&self, uid: &NodeUid) -> Result<Option<Node>, ProtonError> {
        match self
            .rt
            .block_on(self.client.enumerate_nodes(std::slice::from_ref(uid)))
        {
            Ok(nodes) => Ok(nodes.into_iter().next()),
            // An unknown uid is reported either as an empty result or as an
            // outright refusal, depending on the endpoint.
            Err(e) if is_gone(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Accept a released write handle's bytes and queue their upload
    /// (offline.md Phase 3).
    ///
    /// This is what makes a copy into the mount run at disk speed: the caller's
    /// `close` returns once the bytes are staged on local disk and the intent is
    /// in `pending_op`, instead of waiting out a full upload inside the FUSE
    /// handler. It is also what makes an offline write succeed rather than EIO —
    /// the queued op simply waits for the network.
    ///
    /// The scratch file is *moved* into staging, never copied: it is the only
    /// copy of what the user wrote.
    fn queue_revision(&self, h: &WriteHandle) -> Result<(), Errno> {
        // The handle is being retired either way, so any durability sidecar an
        // `fsync` left has done its job: from here the bytes are tracked as a
        // staged write and a queued op, and a sidecar outliving them would offer
        // recovery a second, stale copy of the same write.
        self.cache.clear_scratch_durable(&h.path);
        if is_local_uid(&h.uid) && !self.db.has_create_op(&h.uid.to_string()).unwrap_or(true) {
            debug!(uid = %h.uid, "local node was unlinked before creation; dropping revision");
            let _ = std::fs::remove_file(&h.path);
            return Ok(());
        }
        if !h.dirty {
            let _ = std::fs::remove_file(&h.path);
            return Ok(());
        }
        // Materialize the full content now if we can. A complete blob is
        // uploadable without the network and, crucially, lets a later write to
        // the same file supersede this one safely.
        //
        // A node that exists only locally has no remote base to fill from: its
        // untouched ranges live in the blob queued by an earlier write, not on
        // any server. Filling is only safe — and only needed — while it is still
        // empty, which is exactly when `fill_gaps` skips the network anyway.
        let filled = if is_local_uid(&h.uid) && h.base_size > 0 {
            if let Err(e) = h.file.set_len(h.len) {
                error!(uid = %h.uid, error = %e, "resize scratch file failed");
                return Err(Errno::EIO);
            }
            false
        } else {
            self.fill_gaps(
                &h.uid,
                &h.file,
                h.len,
                h.base_mtime,
                h.base_size,
                &h.written,
            )
            .is_ok()
        };
        let authored: Vec<(u64, u64)> = if filled {
            vec![(0, h.len)]
        } else {
            h.written
                .segments(0, h.len)
                .into_iter()
                .filter(|&(_, _, authored)| authored)
                .map(|(s, e, _)| (s, e))
                .collect()
        };
        let meta = StagedWrite {
            uid: h.uid.to_string(),
            len: h.len,
            base_size: h.base_size,
            base_mtime: h.base_mtime,
            complete: authored == [(0, h.len)],
            authored,
            based_on: self.remote_baseline(&h.uid, h.base_mtime, h.base_size),
        };
        self.enqueue_staged_write(&h.uid, h.ino, &h.path, meta)?;
        debug!(uid = %h.uid, len = h.len, complete = filled, "queued revision upload");
        Ok(())
    }

    /// The remote revision a change to `uid` is being made against, for
    /// [`StagedWrite::based_on`].
    ///
    /// Normally that is simply the base the handle opened over. The exception is
    /// a write that supersedes a still-queued one: its "base" is the previous
    /// *staged blob*, whose size and mtime are ours, not the server's — so the
    /// baseline is inherited from the op being superseded, which is the last one
    /// that actually observed the remote. Without that, chaining two writes
    /// before the queue drains would leave the drain comparing the remote
    /// against a revision it never had, and cutting a conflict copy over
    /// nothing.
    ///
    /// `None` for a node that has never existed remotely: there is no revision
    /// to conflict with until its create drains.
    fn remote_baseline(&self, uid: &NodeUid, base_mtime: i64, base_size: u64) -> Option<Baseline> {
        if is_local_uid(uid) {
            return None;
        }
        match self.pending.lock().get(uid) {
            Some(p) => p.meta.based_on.clone(),
            None => Some(Baseline {
                mtime: base_mtime,
                size: base_size,
                hash: None,
            }),
        }
    }

    /// Move a file holding a node's intended new content into staging and queue
    /// the upload that will make it the remote's content. Shared by the release
    /// of a write handle and by a path-based truncate.
    ///
    /// `src` is consumed either way: on success it is *moved* into staging, and
    /// on the refusal below it is moved there too, just without an op to upload
    /// it. It is the only copy of what the user wrote, so nothing here may
    /// simply delete it.
    fn enqueue_staged_write(
        &self,
        uid: &NodeUid,
        ino: u64,
        src: &Path,
        meta: StagedWrite,
    ) -> Result<(), Errno> {
        // An incomplete blob's gaps refer to the *remote* base. If an earlier
        // write to this file is still queued, the remote no longer holds that
        // base — the previous staged blob does — so superseding it would fill
        // those gaps from the wrong revision. Rather than corrupt the file,
        // refuse the write and keep the bytes recoverable (Phase 2 behaviour).
        // Only reachable offline, editing in place a file whose previous edit
        // has not drained and whose base is not cached.
        if !meta.complete && self.pending.lock().contains_key(uid) {
            self.stage_orphaned_write(uid, ino, src, &meta);
            return Err(Errno::EIO);
        }
        let path = self.cache.stage_write(&meta, src).map_err(|e| {
            error!(%uid, error = %e, "staging write failed");
            Errno::EIO
        })?;
        let meta_json = serde_json::to_string(&meta).unwrap_or_default();
        let superseded = if is_local_uid(uid) {
            // The node has no server-side identity to hang a revision on, so the
            // bytes ride on the create that will mint it.
            let attached = self
                .db
                .attach_blob_to_create(&uid.to_string(), &path.to_string_lossy(), &meta_json)
                .map_err(|e| {
                    error!(%uid, error = %e, "attaching write to queued create failed");
                    Errno::EIO
                })?;
            match attached {
                Some(a) => a.superseded,
                None => {
                    // The create drained between `release` and here, so the node
                    // has a real uid now and this handle's is stale. The bytes are
                    // safe in staging, but nothing here can address them.
                    error!(%uid, staged = %path.display(),
                           "queued create vanished under a write; bytes kept in staging");
                    return Err(Errno::EIO);
                }
            }
        } else {
            let op = PendingOp {
                id: 0,
                kind: OP_REVISION.to_string(),
                uid: uid.to_string(),
                parent_uid: None,
                name: None,
                blob_path: Some(path.to_string_lossy().into_owned()),
                meta_json: Some(meta_json),
                created_at: now_millis(),
                attempts: 0,
                last_error: None,
                next_attempt_at: now_millis() + DRAIN_REVISION_DEBOUNCE.as_millis() as i64,
            };
            let (_id, superseded) = self.db.enqueue_op(&op).map_err(|e| {
                error!(%uid, error = %e, "queueing upload failed");
                Errno::EIO
            })?;
            superseded
        };
        if let Some(old) = superseded {
            self.cache.discard_staged(Path::new(&old));
        }
        let len = meta.len;
        self.pending
            .lock()
            .insert(uid.clone(), PendingRevision { path, meta });
        // Reflect the write in the tree straight away: `ls` must show the new
        // size and mtime even though the remote still holds the old revision.
        let now = now_secs();
        {
            let mut st = self.state.lock();
            st.record_pending_write(ino, len, now);
        }
        // Cached blobs and open readers describe the superseded revision. Reads
        // come from the staged blob until the op drains, so just drop them.
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.wake_drain();
        Ok(())
    }

    /// Queue the new content of a path-based truncate — `> file`, or any
    /// `setattr(size=…)` arriving without a write handle.
    ///
    /// No bytes have been authored at truncate time, which is why this path was
    /// never staged and instead resized the remote content inline. That is also
    /// why a shell redirect failed offline *before* the write that follows it:
    /// the truncate itself needed the network. Staging a blob describing the
    /// result puts it on the same queue as every other write.
    ///
    /// The blob is a hole of the new length; what is real about it is `authored`:
    ///
    /// - `> file` (size 0) is the whole point and needs nothing at all — an
    ///   empty file is complete content, so it queues and drains offline.
    /// - Extending past the end authors the new tail (zeros, by definition) and
    ///   leaves the head to be gap-filled from the base.
    /// - Shrinking authors nothing: every remaining byte still comes from the
    ///   base, so it is the drain that has to fetch it.
    fn queue_truncate(&self, ino: u64, size: u64) -> Result<(), Errno> {
        let (uid, base_mtime, base_size) = {
            let st = self.state.lock();
            match st.entries.get(&ino) {
                Some(e) if e.node.is_file() => {
                    (e.uid.clone(), e.node.modification_time, node_size(&e.node))
                }
                Some(_) => return Err(Errno::EISDIR),
                None => return Err(Errno::ENOENT),
            }
        };
        if size == base_size {
            return Ok(());
        }
        let (authored, complete) = if size == 0 {
            // An empty file has no content to be missing.
            (Vec::new(), true)
        } else if base_size == 0 {
            // Nothing to gap-fill from: every byte is a zero this truncate
            // defines.
            (vec![(0, size)], true)
        } else if size > base_size {
            (vec![(base_size, size)], false)
        } else {
            (Vec::new(), false)
        };
        let (file, path) = self.cache.create_scratch().map_err(|e| {
            error!(%uid, error = %e, "create scratch file for truncate failed");
            Errno::EIO
        })?;
        file.set_len(size).map_err(|e| {
            error!(%uid, error = %e, "resize scratch file for truncate failed");
            let _ = std::fs::remove_file(&path);
            Errno::EIO
        })?;
        let meta = StagedWrite {
            uid: uid.to_string(),
            len: size,
            base_size,
            base_mtime,
            authored,
            complete,
            based_on: self.remote_baseline(&uid, base_mtime, base_size),
        };
        self.enqueue_staged_write(&uid, ino, &path, meta)?;
        debug!(%uid, size, complete, "queued truncate");
        Ok(())
    }

    /// Invent a node under `parent_uid` and queue the op that will make it real
    /// (offline.md Phase 3b). Returns the node to intern, exactly as the online
    /// path returns the one the server minted.
    ///
    /// The parent may itself be a placeholder — `mkdir -p` offline, or `cp -r` of
    /// a tree. That is fine: the op records the parent it was made under, and the
    /// parent's own drain rewrites it to the real uid before this op can run.
    fn queue_local_node(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        is_dir: bool,
    ) -> Result<Node, Errno> {
        let uid = mint_local_uid();
        let op = PendingOp {
            id: 0,
            kind: if is_dir { OP_MKDIR } else { OP_CREATE }.to_string(),
            uid: uid.to_string(),
            parent_uid: Some(parent_uid.to_string()),
            name: Some(name.to_string()),
            blob_path: None,
            meta_json: None,
            created_at: now_millis(),
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };
        self.db.enqueue_op(&op).map_err(|e| {
            error!(%parent_uid, name, error = %e, "queueing local node failed");
            Errno::EIO
        })?;
        debug!(%uid, %parent_uid, name, is_dir, "created node offline; queued");
        Ok(local_node(
            uid,
            parent_uid.clone(),
            name.to_string(),
            is_dir,
        ))
    }

    /// Queue giving a node a new parent and/or name, and apply it to the tree
    /// now (offline.md Phase 3b).
    ///
    /// The op records the desired end state rather than the step, so it both
    /// supersedes any earlier queued rename and lets the drain skip whichever
    /// half the remote already agrees with.
    ///
    /// `new_parent_uid` may be a placeholder — moving a file into a folder
    /// created offline — which is why this cannot simply be the online call with
    /// a retry around it: the API would 404 on a `local~` parent. The op waits
    /// for that folder's drain to rewrite it, exactly as a queued create does.
    fn queue_rename(
        &self,
        ino: u64,
        uid: &NodeUid,
        new_parent_ino: u64,
        new_parent_uid: &NodeUid,
        new_name: &str,
    ) -> Result<(), Errno> {
        let op = PendingOp {
            id: 0,
            kind: OP_RENAME.to_string(),
            uid: uid.to_string(),
            parent_uid: Some(new_parent_uid.to_string()),
            name: Some(new_name.to_string()),
            blob_path: None,
            meta_json: None,
            created_at: now_millis(),
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };
        self.db.enqueue_op(&op).map_err(|e| {
            error!(%uid, new_name, error = %e, "queueing rename failed");
            Errno::EIO
        })?;
        self.state
            .lock()
            .rename_in_place(ino, new_parent_ino, new_parent_uid, new_name);
        self.wake_drain();
        debug!(%uid, %new_parent_uid, new_name, "renamed offline; queued");
        Ok(())
    }

    /// Queue trashing a node the server knows about, and drop it from the tree
    /// now (offline.md Phase 3b).
    ///
    /// Anything else queued for this node is discarded first: the user has said
    /// the file should not exist, so uploading bytes to it or renaming it are
    /// both work towards an outcome nobody wants any more. That does throw away
    /// staged bytes that never landed — which is precisely what deleting an
    /// un-uploaded file means, and the alternative (upload it, then trash it) is
    /// worse in every way.
    fn queue_trash(&self, uid: &NodeUid, name: &str) -> Result<(), Errno> {
        self.discard_queued_ops(uid);
        let op = PendingOp {
            id: 0,
            kind: OP_TRASH.to_string(),
            uid: uid.to_string(),
            parent_uid: None,
            name: Some(name.to_string()),
            blob_path: None,
            meta_json: None,
            created_at: now_millis(),
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        };
        self.db.enqueue_op(&op).map_err(|e| {
            error!(%uid, error = %e, "queueing trash failed");
            Errno::EIO
        })?;
        self.state.lock().forget(uid);
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.wake_drain();
        debug!(%uid, name, "trashed offline; queued");
        Ok(())
    }

    /// Drop every op queued against a node, and the staged bytes they own.
    fn discard_queued_ops(&self, uid: &NodeUid) {
        match self.db.delete_ops_for_uid(&uid.to_string()) {
            Ok(blobs) => {
                for blob in blobs {
                    self.cache.discard_staged(Path::new(&blob));
                }
            }
            Err(e) => error!(%uid, error = %e, "dropping queued ops failed"),
        }
        self.pending.lock().remove(uid);
    }

    /// Nudge the drain worker to re-examine the queue now.
    fn wake_drain(&self) {
        let (lock, cv) = &*self.drain_wake;
        *lock.lock() = true;
        cv.notify_all();
    }

    /// Keep a write we cannot safely queue, so the bytes are recoverable even
    /// though the caller is getting an error. See [`Core::queue_revision`].
    fn stage_orphaned_write(&self, uid: &NodeUid, ino: u64, src: &Path, meta: &StagedWrite) {
        match self.cache.stage_write(meta, src) {
            Ok(staged) => {
                error!(
                    %uid,
                    staged = %staged.display(),
                    "cannot queue write over an undrained edit; bytes kept in staging"
                );
                let name = {
                    let st = self.state.lock();
                    st.entries
                        .get(&ino)
                        .map(|e| e.node.name.clone())
                        .unwrap_or_default()
                };
                self.log_activity(
                    ActivityKind::Upload,
                    &name,
                    format!("write not queued; changes kept at {}", staged.display()),
                    false,
                );
            }
            Err(e) => {
                error!(%uid, error = %e, "staging write failed; bytes lost");
                let _ = std::fs::remove_file(src);
            }
        }
    }

    /// Download a whole file's plaintext, registering the transfer so
    /// `GetQueueStatus` can report its progress. `total` is the expected size
    /// (`0` if unknown). Streams through [`download_file_to`] so each block ticks
    /// the progress counter.
    ///
    /// [`download_file_to`]: ProtonDriveClient::download_file_to
    fn download_file_tracked(
        &self,
        uid: &NodeUid,
        name: &str,
        total: u64,
    ) -> std::result::Result<Vec<u8>, ProtonError> {
        let guard = self
            .transfers
            .begin(name, uid.to_string(), TransferDirection::Download, total);
        let mut out = CountingWriter::new(Vec::with_capacity(total as usize), &guard);
        self.rt
            .block_on(self.client.download_file_to(uid, &mut out))?;
        Ok(out.into_inner())
    }

    /// Like [`download_file_tracked`] for a photo, streaming through the photos
    /// client's [`download_photo_to`].
    ///
    /// [`download_file_tracked`]: Core::download_file_tracked
    /// [`download_photo_to`]: ProtonPhotosClient::download_photo_to
    fn download_photo_tracked(
        &self,
        photos: &ProtonPhotosClient,
        uid: &NodeUid,
        name: &str,
        total: u64,
    ) -> std::result::Result<Vec<u8>, ProtonError> {
        let guard = self
            .transfers
            .begin(name, uid.to_string(), TransferDirection::Download, total);
        let mut out = CountingWriter::new(Vec::with_capacity(total as usize), &guard);
        self.rt.block_on(photos.download_photo_to(uid, &mut out))?;
        Ok(out.into_inner())
    }

    /// Pin the node at mountpoint-relative `rel`. A file downloads its full
    /// plaintext into the content cache; a folder records a recursive pin and
    /// downloads every descendant file (selective sync). Returns a human message.
    fn pin(&self, rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let (name, is_folder, mtime, size) = {
            let st = self.state.lock();
            let e = st
                .entries
                .get(&ino)
                .ok_or_else(|| CoreError::not_found("node vanished"))?;
            (
                e.node.name.clone(),
                e.node.is_folder(),
                e.node.modification_time,
                node_size(&e.node),
            )
        };
        if is_folder {
            // Record the recursive pin first so every descendant is eviction-
            // exempt before we start filling the cache with the subtree.
            self.cache
                .add_pin(&uid, rel, true)
                .map_err(|e| CoreError::from_api(&e, "pin"))?;
            let n = self.pin_subtree(ino)?;
            return Ok(format!("{name} ({n} files)"));
        }
        let bytes = self
            .download_file_tracked(&uid, &name, size)
            .map_err(|e| CoreError::from_api(&e, "download"))?;
        self.cache
            .store(&uid, mtime, size, &bytes)
            .map_err(|e| CoreError::internal(format!("cache store: {e}")))?;
        self.cache
            .add_pin(&uid, rel, false)
            .map_err(|e| CoreError::from_api(&e, "pin"))?;
        Ok(name)
    }

    /// Download and cache every file in the subtree rooted at folder `ino`,
    /// returning the count cached (already-fresh blobs counted, not re-fetched).
    /// Walks the tree depth-first, enumerating each folder so a cold subtree is
    /// fully discovered; the lock is dropped before each network download.
    fn pin_subtree(&self, ino: u64) -> CoreResult<usize> {
        let mut files: Vec<(NodeUid, String, i64, u64)> = Vec::new();
        let mut stack = vec![ino];
        while let Some(dir) = stack.pop() {
            self.ensure_children(dir)
                .map_err(|e| self.errno_error(e, "enumerate"))?;
            let st = self.state.lock();
            if let Some(kids) = st.children.get(&dir) {
                for &k in kids {
                    if let Some(e) = st.entries.get(&k) {
                        if e.node.is_folder() {
                            stack.push(k);
                        } else {
                            files.push((
                                e.uid.clone(),
                                e.node.name.clone(),
                                e.node.modification_time,
                                node_size(&e.node),
                            ));
                        }
                    }
                }
            }
        }
        let mut count = 0;
        for (uid, name, mtime, size) in files {
            if self.cache.is_cached(&uid, mtime, size) {
                count += 1;
                continue;
            }
            match self.download_file_tracked(&uid, &name, size) {
                Ok(bytes) => {
                    if self.cache.store(&uid, mtime, size, &bytes).is_ok() {
                        count += 1;
                    }
                }
                Err(e) => warn!(%uid, error = %e, "pin subtree: download failed"),
            }
        }
        Ok(count)
    }

    /// Fetch a thumbnail of `ttype` for the file at `ino`, served from the cache
    /// when fresh and otherwise downloaded from the remote and cached. Returns
    /// `Ok(None)` when the node is not a file or has no thumbnail of that type.
    fn thumbnail(&self, ino: u64, ttype: ThumbnailType) -> Result<Option<Vec<u8>>, Errno> {
        let (uid, mtime) = {
            let st = self.state.lock();
            match st.entries.get(&ino) {
                Some(e) if e.node.is_file() => (e.uid.clone(), e.node.modification_time),
                Some(_) => return Ok(None),
                None => return Err(Errno::ENOENT),
            }
        };
        if let Some(bytes) = self.cache.read_thumbnail(&uid, ttype.as_i32(), mtime) {
            return Ok(Some(bytes));
        }
        // "This file has no thumbnail" is an answer worth remembering: without
        // it every listing pays a round trip per file to be told nothing (B5).
        let key = (uid.clone(), ttype.as_i32());
        if self.no_thumbnail.lock().get(&key) == Some(&mtime) {
            return Ok(None);
        }
        let bytes = self
            .rt
            .block_on(self.client.download_thumbnail(&uid, ttype))
            .map_err(|e| {
                warn!(%uid, error = %e, "download thumbnail failed");
                Errno::EIO
            })?;
        match &bytes {
            Some(bytes) => {
                let _ = self
                    .cache
                    .store_thumbnail(&uid, ttype.as_i32(), mtime, bytes);
            }
            None => {
                let mut misses = self.no_thumbnail.lock();
                // Bounded by clearing rather than by LRU: the entries are two
                // words each, the cap is far above any plausible working set,
                // and re-learning a miss costs one round trip. Not worth a
                // second data structure to order them.
                if misses.len() >= MAX_THUMBNAIL_MISSES {
                    misses.clear();
                }
                misses.insert(key, mtime);
            }
        }
        Ok(bytes)
    }

    /// Unpin the node at `rel`, evicting its cached content. For a folder, also
    /// evicts every descendant's cached blob (the subtree is no longer kept).
    fn unpin(&self, rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let (name, is_folder) = {
            let st = self.state.lock();
            st.entries
                .get(&ino)
                .map(|e| (e.node.name.clone(), e.node.is_folder()))
                .unwrap_or_default()
        };
        self.cache
            .remove_pin(&uid)
            .map_err(|e| CoreError::from_api(&e, "unpin"))?;
        // A recursively-pinned folder's descendants were eviction-exempt; now
        // that the pin is gone, reclaim their blobs eagerly instead of waiting
        // for budget pressure. Descendants come from the DB node tree.
        if is_folder && let Ok(uids) = self.db.descendants(&uid.to_string()) {
            for s in uids {
                if let Some(u) = parse_uid(&s) {
                    self.cache.evict(&u);
                    self.evict_reader(&u);
                }
            }
        }
        Ok(name)
    }

    /// A Photos API handle sharing this Core's Drive client and session, so it
    /// reuses the daemon's single authenticated session rather than logging in
    /// again (Proton refresh tokens are single-use). Cheap — the Drive client
    /// is `Clone` over `Arc`-backed state.
    fn photos(&self) -> ProtonPhotosClient {
        ProtonPhotosClient::from_drive_client(self.client.clone())
    }

    /// List the directory at mountpoint-relative `rel` for the in-app browser:
    /// the same lazy remote enumeration `readdir` uses, projected into
    /// serializable [`DirEntry`]s (with per-file pin state).
    fn list_dir(&self, rel: &Path) -> CoreResult<Vec<DirEntry>> {
        let (ino, _uid) = self.resolve(rel)?;
        self.ensure_children(ino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;
        // Snapshot the listing, then drop the lock before touching the on-disk
        // pin registry so a slow disk read doesn't block FUSE metadata ops.
        let rows: Vec<(String, bool, u64, i64, NodeUid)> = {
            let st = self.state.lock();
            st.children
                .get(&ino)
                .map(|kids| {
                    kids.iter()
                        .filter_map(|k| st.entries.get(k))
                        .map(|e| {
                            (
                                e.node.name.clone(),
                                e.node.is_folder(),
                                node_size(&e.node),
                                e.node.modification_time,
                                e.uid.clone(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(rows
            .into_iter()
            .map(|(name, is_dir, size, modified, uid)| DirEntry {
                name,
                is_dir,
                size,
                modified,
                pinned: self.cache.is_pinned(&uid),
                cached: !is_dir && self.cache.is_cached(&uid, modified, size),
                uid: uid.to_string(),
                // Listing entries live in the requested dir; the caller derives
                // the path from its name. Left empty.
                path: String::new(),
            })
            .collect())
    }

    /// Full-text search node names against the local SQLite index, mapping each
    /// DB hit to the wire [`SearchHit`] (resolving live pin state from the cache,
    /// which the DB doesn't track). Pure local lookup — never hits the network.
    fn search(&self, query: &str, limit: usize) -> CoreResult<Vec<SearchHit>> {
        let hits = self
            .db
            .search(query, limit)
            .map_err(|e| CoreError::from_api(&e, "search"))?;
        Ok(hits
            .into_iter()
            .map(|h| SearchHit {
                name: h.node.name.clone(),
                path: h.path,
                is_dir: h.node.is_folder(),
                size: node_size(&h.node),
                modified: h.node.modification_time,
                pinned: self.cache.is_pinned(&h.node.uid),
                uid: h.node.uid.to_string(),
            })
            .collect())
    }

    /// Search the index of files on this machine (outside Drive), built by the
    /// background scanner in [`run_local_index`]. Pure local lookup, never hits
    /// the network — and never touches the FUSE mount, which the scanner excludes.
    fn search_local(&self, query: &str, limit: usize) -> CoreResult<Vec<LocalHit>> {
        let hits = self
            .db
            .search_local(query, limit)
            .map_err(|e| CoreError::from_api(&e, "local search"))?;
        Ok(hits
            .into_iter()
            .map(|h| LocalHit {
                name: h.name,
                path: h.path,
                is_dir: h.is_dir,
                size: h.size.max(0) as u64,
                modified: h.mtime,
            })
            .collect())
    }

    /// A page of the photos timeline (newest first), sliced out of the DB.
    /// `Ok(None)` when the account has no photos volume.
    ///
    /// Stale-while-revalidate: a persisted timeline is served *immediately*, and
    /// refreshed on the runtime if it is older than [`TIMELINE_TTL`] — so opening
    /// the app paints from disk rather than waiting on a full `enumerate_timeline`
    /// (which returns the whole timeline, not a page). Only an empty DB blocks on
    /// the network, i.e. the very first run.
    ///
    /// Metadata only: a thumbnail path is attached for photos whose thumbnail is
    /// already cached, and nothing is downloaded here — the front-end pulls the
    /// thumbnails it actually paints via [`Core::photo_thumbs`].
    fn listing_stale(&self, key: &str, ttl: Duration) -> bool {
        match self.db.state_i64(key).ok().flatten() {
            Some(ms) => now_ms().saturating_sub(ms) >= ttl.as_millis() as i64,
            None => true,
        }
    }

    /// Re-fetch the whole photos timeline and persist it. Returns whether the
    /// account has a photos volume at all.
    /// Download the full content of the Drive file at mountpoint-relative `rel`
    /// into the content cache, returning its on-disk path (served from cache
    /// when a fresh blob already exists). Lets a front-end open the file with
    /// the user's default application without pinning it.
    fn open_file(&self, rel: &Path) -> CoreResult<PathBuf> {
        let (ino, uid) = self.resolve(rel)?;
        let (name, mtime, size) = {
            let st = self.state.lock();
            let e = st
                .entries
                .get(&ino)
                .ok_or_else(|| CoreError::not_found("node vanished"))?;
            if !e.node.is_file() {
                return Err(CoreError::invalid("not a regular file"));
            }
            (
                e.node.name.clone(),
                e.node.modification_time,
                node_size(&e.node),
            )
        };
        if let Some(p) = self.cache.cached_content_path(&uid, mtime, size) {
            return Ok(p);
        }
        let bytes = self
            .download_file_tracked(&uid, &name, size)
            .map_err(|e| CoreError::from_api(&e, "download"))?;
        self.cache
            .store(&uid, mtime, size, &bytes)
            .map_err(|e| CoreError::internal(format!("cache store: {e}")))?;
        Ok(self.cache.content_path(&uid))
    }

    /// Drop the cached child listing of `rel`'s parent directory so the next
    /// `ListDir` re-enumerates it from the server. No-op when the parent can't be
    /// resolved (e.g. `rel` is the root). Resolves the parent without holding the
    /// state lock, then invalidates under it.
    fn invalidate_parent_listing(&self, rel: &Path) {
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        if let Ok((pino, _)) = self.resolve_path(parent) {
            self.state.lock().invalidate_listing(pino);
        }
    }

    /// Rename a file or folder to `new_name`. `rel` is mountpoint-relative.
    /// Mirrors the FUSE `rename` write path: rename on the remote, forget the
    /// node so it re-interns under its new name, and drop the parent listing so
    /// the next `ListDir` re-enumerates.
    fn rename(&self, rel: &Path, new_name: &str) -> CoreResult<String> {
        if new_name.is_empty() || new_name.contains('/') {
            return Err(CoreError::invalid(format!("invalid name: {new_name:?}")));
        }
        let (_ino, uid) = self.resolve(rel)?;
        self.rt
            .block_on(self.client.rename_node(&uid, new_name, None))
            .map_err(|e| CoreError::from_api(&e, "rename"))?;
        self.state.lock().forget(&uid);
        self.invalidate_parent_listing(rel);
        Ok(new_name.to_string())
    }

    /// Move a file or folder into the folder at `new_parent_rel`. Both paths are
    /// mountpoint-relative. Forgets the node and invalidates both the source and
    /// destination listings so each re-enumerates on next access.
    fn move_to(&self, rel: &Path, new_parent_rel: &Path) -> CoreResult<String> {
        let (_ino, uid) = self.resolve(rel)?;
        let (pino, new_parent_uid) = self
            .resolve_path(new_parent_rel)
            .map_err(|e| self.errno_error(e, "resolve new parent"))?;
        self.rt
            .block_on(self.client.move_node(&uid, &new_parent_uid))
            .map_err(|e| CoreError::from_api(&e, "move"))?;
        let name = self
            .state
            .lock()
            .forget(&uid)
            .map(|(_, n)| n)
            .unwrap_or_default();
        self.invalidate_parent_listing(rel);
        self.state.lock().invalidate_listing(pino);
        Ok(name)
    }

    /// Trash a file or folder. `rel` is mountpoint-relative. Forgets the node,
    /// evicts any cached content, and invalidates the parent listing.
    fn delete(&self, rel: &Path) -> CoreResult<String> {
        let (_ino, uid) = self.resolve(rel)?;
        self.rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(&uid)))
            .map_err(|e| CoreError::from_api(&e, "trash"))?;
        let name = self
            .state
            .lock()
            .forget(&uid)
            .map(|(_, n)| n)
            .unwrap_or_default();
        self.cache.evict(&uid);
        self.evict_reader(&uid);
        self.invalidate_parent_listing(rel);
        self.invalidate_trash();
        Ok(name)
    }

    /// Remove the node a `rename` is about to replace, so the new name is free
    /// for the API call that follows.
    ///
    /// `rename(2)` promises to replace an existing destination atomically. Proton
    /// offers no such primitive — `rename_node` refuses a name that is already
    /// taken — so this is the first half of an emulation that is *not* atomic:
    /// see [`Core::restore_replaced`] for the other half (bugs.md B13).
    ///
    /// A node whose own creation is still queued has never reached the server, so
    /// dropping its queued ops is the whole removal; nothing goes to the wire and
    /// it works offline.
    fn remove_replaced(&self, uid: &NodeUid, name: &str) -> Result<(), Errno> {
        if is_local_uid(uid) {
            self.discard_queued_ops(uid);
            self.state.lock().forget(uid);
            debug!(%uid, name, "replaced a node whose create was still queued");
            return Ok(());
        }
        if let Err(e) = self
            .rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(uid)))
        {
            error!(%uid, name, error = %e, "trashing the node a rename replaces failed");
            self.log_activity(ActivityKind::Trash, name, e.to_string(), false);
            return Err(Errno::EIO);
        }
        self.discard_queued_ops(uid);
        self.state.lock().forget(uid);
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.invalidate_trash();
        // The node is recoverable from the trash, but only if the user knows it
        // went there — a rename is not an operation anyone expects to trash
        // something, so this is the only record that it happened.
        self.log_activity(
            ActivityKind::Trash,
            name,
            "replaced by a rename from the mount",
            true,
        );
        Ok(())
    }

    /// Put back the node [`Core::remove_replaced`] trashed, after the rename it
    /// was clearing the way for failed anyway.
    ///
    /// Best-effort by construction: if the restore also fails there is nothing
    /// further to try, and the node is still in the trash where `pdfs restore`
    /// can reach it. Says so loudly in that case, because the alternative is a
    /// file the user believes was only renamed quietly sitting in the trash.
    fn restore_replaced(&self, victim: Option<&(u64, NodeUid)>, name: &str) {
        let Some((_, uid)) = victim else { return };
        if is_local_uid(uid) {
            // Its queued create was discarded and cannot be reconstructed from
            // here; the caller's error is what the user acts on.
            warn!(%uid, name, "a rename failed after discarding a queued node it replaced");
            return;
        }
        match self
            .rt
            .block_on(self.client.restore_nodes(std::slice::from_ref(uid)))
        {
            Ok(()) => {
                self.invalidate_trash();
                debug!(%uid, name, "restored the node a failed rename had replaced");
            }
            Err(e) => {
                error!(%uid, name, error = %e, "restoring a replaced node failed; it stays in the trash");
                self.log_activity(ActivityKind::Restore, name, e.to_string(), false);
            }
        }
    }

    /// List the account's trash, from the DB. Trashed nodes are outside the
    /// mounted tree — the FUSE side forgot them when they were trashed — so the
    /// listing is persisted in its own table rather than derived from `State`, and
    /// each entry is identified by its uid (its only remaining handle) with an
    /// empty path.
    ///
    /// Stale-while-revalidate, like the photos timeline: a persisted listing comes
    /// back at DB speed and is refreshed in the background past [`TRASH_TTL`].
    /// Our own trash mutations invalidate it outright (see
    /// [`Core::invalidate_trash`]), so the TTL only covers changes made elsewhere.
    fn list_trash(&self) -> CoreResult<Vec<DirEntry>> {
        let stale = self.listing_stale(TRASH_SYNCED_MS, TRASH_TTL);
        // Never fetched: this request has to wait for it.
        if self.db.state_i64(TRASH_SYNCED_MS).ok().flatten().is_none() {
            self.rt.block_on(self.refresh_trash())?;
        } else if stale {
            self.spawn_trash_refresh();
        }

        Ok(self
            .db
            .trash_list()
            .map_err(CoreError::from)?
            .into_iter()
            .map(|item| DirEntry {
                name: item.name,
                is_dir: item.is_dir,
                size: item.size.max(0) as u64,
                modified: item.mtime,
                // A trashed node can't be pinned or served from the mount, so its
                // content is never current cache: report neither.
                pinned: false,
                cached: false,
                uid: item.uid,
                path: String::new(),
            })
            .collect())
    }

    /// Re-fetch the trash listing from the server and persist it.
    async fn refresh_trash(&self) -> CoreResult<()> {
        let uids = self
            .client
            .enumerate_trash_node_uids()
            .await
            .map_err(|e| CoreError::from_api(&e, "enumerate trash"))?;
        let nodes = if uids.is_empty() {
            Vec::new()
        } else {
            self.client
                .enumerate_nodes(&uids)
                .await
                .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?
        };
        let items: Vec<StoredTrash> = nodes
            .into_iter()
            .map(|node| StoredTrash {
                uid: node.uid.to_string(),
                name: node.name.clone(),
                is_dir: node.is_folder(),
                size: node_size(&node) as i64,
                mtime: node.modification_time,
            })
            .collect();
        self.db.trash_replace(&items).map_err(CoreError::from)?;
        let _ = self.db.set_state_i64(TRASH_SYNCED_MS, now_ms());
        Ok(())
    }

    /// Refresh the trash off the request path. At most one refresh at a time.
    fn spawn_trash_refresh(&self) {
        if self.trash_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let core = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = core.refresh_trash().await {
                warn!(error = %e, "background trash refresh failed");
            }
            core.trash_refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// Drop the persisted trash listing's freshness stamp after we changed the
    /// trash ourselves. The next Trash page then *waits* for a fresh listing
    /// rather than painting one from before the mutation — the user just made the
    /// change and is about to look straight at it.
    fn invalidate_trash(&self) {
        let _ = self.db.clear_state(TRASH_SYNCED_MS);
    }

    /// Drop the persisted photos timeline's freshness stamp, so the next timeline
    /// read fetches rather than serving what it already has.
    fn invalidate_photos(&self) {
        let _ = self.db.clear_state(PHOTOS_SYNCED_MS);
    }

    /// Drop one folder's cached child listing (`rel` is mountpoint-relative), so
    /// the next `ListDir`/`readdir` re-enumerates it from the server. Backs
    /// [`CtlRequest::Refresh`] with a [`RefreshScope::Dir`] scope.
    fn refresh_dir(&self, rel: &Path) -> CoreResult<()> {
        let (ino, _uid) = self.resolve(rel)?;
        self.state.lock().invalidate_listing(ino);
        Ok(())
    }

    /// Parse wire uids (`volume~link`) into [`NodeUid`]s, rejecting the whole
    /// batch if any is malformed — a partial trash mutation is worse than none.
    fn parse_uids(uids: &[String]) -> CoreResult<Vec<NodeUid>> {
        if uids.is_empty() {
            return Err(CoreError::invalid("no nodes given"));
        }
        uids.iter()
            .map(|u| parse_uid(u).ok_or_else(|| CoreError::invalid(format!("invalid uid: {u}"))))
            .collect()
    }

    /// Restore trashed nodes to the folders they were trashed from. The parents
    /// are read *before* the restore — a restored node reappears in a listing the
    /// daemon may already have cached, so each destination folder is invalidated
    /// and re-enumerated on next access.
    fn restore(&self, uids: &[String]) -> CoreResult<usize> {
        let parsed = Self::parse_uids(uids)?;
        let parents: Vec<NodeUid> = self
            .rt
            .block_on(self.client.enumerate_nodes(&parsed))
            .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?
            .into_iter()
            .filter_map(|n| n.parent_uid)
            .collect();
        self.rt
            .block_on(self.client.restore_nodes(&parsed))
            .map_err(|e| CoreError::from_api(&e, "restore"))?;
        {
            let mut st = self.state.lock();
            for parent in parents {
                if let Some(&ino) = st.by_uid.get(&parent) {
                    st.invalidate_listing(ino);
                }
            }
        }
        self.invalidate_trash();
        Ok(parsed.len())
    }

    /// Permanently delete trashed nodes. Irreversible on the server; locally it
    /// drops any metadata and cached content the node still owns.
    fn delete_forever(&self, uids: &[String]) -> CoreResult<usize> {
        let parsed = Self::parse_uids(uids)?;
        self.rt
            .block_on(self.client.delete_nodes(&parsed))
            .map_err(|e| CoreError::from_api(&e, "delete"))?;
        self.drop_local(&parsed);
        self.invalidate_trash();
        Ok(parsed.len())
    }

    /// Permanently delete everything in the trash. The uids are listed first so
    /// the blobs of items trashed by *another* client — which this daemon may
    /// still hold in its cache — are reclaimed too, not just the ones it trashed.
    fn empty_trash(&self) -> CoreResult<usize> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_trash_node_uids())
            .map_err(|e| CoreError::from_api(&e, "enumerate trash"))?;
        self.rt
            .block_on(self.client.empty_trash())
            .map_err(|e| CoreError::from_api(&e, "empty trash"))?;
        self.drop_local(&uids);
        self.invalidate_trash();
        Ok(uids.len())
    }

    /// Forget every trace of nodes that no longer exist anywhere: their inode and
    /// DB row, and their cached content.
    fn drop_local(&self, uids: &[NodeUid]) {
        let mut st = self.state.lock();
        for uid in uids {
            st.forget(uid);
        }
        drop(st);
        for uid in uids {
            self.cache.evict(uid);
            self.evict_reader(uid);
        }
    }

    /// Create a folder named `name` under the mountpoint-relative `parent_rel`.
    /// Interns the new node directly so it shows up without a re-enumeration.
    fn create_folder(&self, parent_rel: &Path, name: &str) -> CoreResult<String> {
        if name.is_empty() || name.contains('/') {
            return Err(CoreError::invalid(format!("invalid name: {name:?}")));
        }
        let (pino, parent_uid) = self.resolve(parent_rel)?;
        self.ensure_children(pino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;
        let new_uid = self
            .rt
            .block_on(
                self.client
                    .create_folder(&parent_uid, name, Some(now_secs())),
            )
            .map_err(|e| CoreError::from_api(&e, "create folder"))?;
        let node = self
            .fetch_node(&new_uid)
            .map_err(|e| self.errno_error(e, "fetch node"))?;
        let mut st = self.state.lock();
        let ino = st.intern(pino, node);
        if let Some(kids) = st.children.get_mut(&pino)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        Ok(name.to_string())
    }
    // ---- activity log -----------------------------------------------------

    /// Append one entry to the activity log. Callable from any thread (the sync
    /// engine and the bulk uploader both log from background tasks). A failed
    /// write is logged and dropped: the feed is a record of work, never a reason
    /// to fail the work itself.
    pub(crate) fn log_activity(
        &self,
        kind: ActivityKind,
        target: impl Into<String>,
        // `Display` rather than `Into<String>` so a classified `CoreError` can be
        // logged as-is, without the caller flattening it first.
        detail: impl std::fmt::Display,
        ok: bool,
    ) {
        let entry = ActivityEntry {
            time: now_secs(),
            kind,
            target: target.into(),
            detail: detail.to_string(),
            ok,
        };
        if let Err(e) = self.db.activity_add(&entry) {
            warn!(error = ?e, "could not record activity");
        }
    }

    /// The recent activity, newest first, capped at `limit` entries.
    fn list_activity(&self, limit: usize) -> Vec<ActivityEntry> {
        match self.db.activity_list(limit) {
            Ok(items) => items,
            Err(e) => {
                warn!(error = ?e, "could not read activity");
                Vec::new()
            }
        }
    }

    // ---- live sync progress -----------------------------------------------

    /// Start tracking a reconcile pass over `folder_id`, in [`SyncPhase::Scanning`].
    pub(crate) fn progress_begin(&self, folder_id: i64) {
        self.sync_progress.lock().insert(
            folder_id,
            SyncProgress {
                phase: SyncPhase::Scanning,
                done: 0,
                total: 0,
                current: String::new(),
            },
        );
    }

    /// Apply `f` to a folder's live progress, if a pass is running for it.
    fn progress_update(&self, folder_id: i64, f: impl FnOnce(&mut SyncProgress)) {
        if let Some(p) = self.sync_progress.lock().get_mut(&folder_id) {
            f(p);
        }
    }

    /// Set the number of items the scan expects to check, from the size of the
    /// last pass's baseline. Only an estimate — the folder may have grown since —
    /// but it turns the scan from an indeterminate pulse into a bar that moves,
    /// which is the difference between "it's stuck" and "it's working" on a folder
    /// whose walk takes minutes.
    pub(crate) fn progress_scan_total(&self, folder_id: i64, n: usize) {
        self.progress_update(folder_id, |p| p.total = n);
    }

    /// Note that the scan has checked one more item, named `name`.
    pub(crate) fn progress_scanned(&self, folder_id: i64, name: &str) {
        self.progress_update(folder_id, |p| {
            p.done += 1;
            p.current = name.to_string();
        });
    }

    /// Note that `n` more items have been queued for this pass, and that it has
    /// moved on from scanning to applying the diff. The scan's counts are dropped:
    /// they measured a different quantity (items checked, not items to apply), so
    /// carrying them over would start the applying bar at a meaningless fraction.
    pub(crate) fn progress_queued(&self, folder_id: i64, n: usize) {
        self.progress_update(folder_id, |p| {
            if p.phase == SyncPhase::Scanning {
                p.phase = SyncPhase::Applying;
                p.done = 0;
                p.total = 0;
                p.current.clear();
            }
            p.total += n;
        });
    }

    /// Note that work has started on `name` (shown as the pass's current item).
    pub(crate) fn progress_started(&self, folder_id: i64, name: &str) {
        self.progress_update(folder_id, |p| p.current = name.to_string());
    }

    /// Note that one queued item finished, whether it succeeded or not.
    pub(crate) fn progress_finished(&self, folder_id: i64) {
        self.progress_update(folder_id, |p| {
            p.done += 1;
            p.current.clear();
        });
    }

    /// Stop tracking a pass — no progress is reported for the folder until the
    /// next [`progress_begin`](Self::progress_begin).
    pub(crate) fn progress_end(&self, folder_id: i64) {
        self.sync_progress.lock().remove(&folder_id);
    }
}

/// Map a [`MemberRole`] to its wire string.
fn role_to_str(role: MemberRole) -> &'static str {
    match role {
        MemberRole::Viewer => "viewer",
        MemberRole::Editor => "editor",
        MemberRole::Admin => "admin",
        MemberRole::Inherited => "inherited",
    }
}

/// Parse a wire role string into a [`MemberRole`]. "inherited" is read-only and
/// rejected here, since it cannot be sent when inviting or updating.
fn role_from_str(role: &str) -> CoreResult<MemberRole> {
    match role.to_lowercase().as_str() {
        "viewer" => Ok(MemberRole::Viewer),
        "editor" => Ok(MemberRole::Editor),
        "admin" => Ok(MemberRole::Admin),
        other => Err(CoreError::invalid(format!("invalid role: {other}"))),
    }
}

/// Map a device type to a display string.
fn device_type_str(t: proton_drive_rs::DeviceType) -> &'static str {
    match t {
        proton_drive_rs::DeviceType::Windows => "Windows",
        proton_drive_rs::DeviceType::MacOs => "MacOs",
        proton_drive_rs::DeviceType::Linux => "Linux",
    }
}

/// Convert an SDK [`PublicLink`](proton_drive_rs::PublicLink) into the wire form.
fn public_link_info(link: proton_drive_rs::PublicLink) -> PublicLinkInfo {
    PublicLinkInfo {
        id: link.public_link_id,
        url: link.url,
        role: role_to_str(link.role).to_string(),
        expires: link.expiration_time,
        has_password: link.has_custom_password,
    }
}

/// Parse a `volume~link` uid display string back into a [`NodeUid`]. Front-ends
/// receive uids as strings over the control socket and pass them back verbatim.
fn parse_uid(s: &str) -> Option<NodeUid> {
    let (vol, link) = s.split_once('~')?;
    Some(NodeUid::new(VolumeId::from(vol), LinkId::from(link)))
}
/// Current wall-clock time as epoch seconds (0 if the clock is before the epoch).
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current wall-clock time as epoch milliseconds, the resolution `pending_op`
/// timestamps and backoff deadlines are kept in.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse a [`NodeUid`] back from its `Display` form (`volume~link`), which is
/// how one is persisted in `pending_op.uid` and a [`StagedWrite`] sidecar. The
/// SDK has no `FromStr`, and neither id contains a `~`.
fn parse_node_uid(s: &str) -> Option<NodeUid> {
    let (vol, link) = s.split_once('~')?;
    Some(NodeUid::new(VolumeId::from(vol), LinkId::from(link)))
}

/// Distinguishes placeholder uids minted by [`mint_local_uid`] within one run.
static LOCAL_UID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Invent a uid for a node created while offline, so it can be interned, listed
/// and written to before the server has ever heard of it (offline.md Phase 3b).
///
/// Uniqueness only has to hold among this machine's undrained ops, so the clock
/// (which separates runs) plus a counter (which separates nodes within a run) is
/// enough without taking on a uuid dependency.
fn mint_local_uid() -> NodeUid {
    let seq = LOCAL_UID_SEQ.fetch_add(1, Ordering::Relaxed);
    NodeUid::new(
        VolumeId::from(LOCAL_VOLUME),
        LinkId::from(format!("{}-{seq}", now_millis())),
    )
}

/// Whether this node exists only on this machine, so far. Such a uid is
/// meaningless to the API and must never be sent to it.
fn is_local_uid(uid: &NodeUid) -> bool {
    uid.volume_id.as_str() == LOCAL_VOLUME
}

/// [`is_local_uid`] for a uid in its persisted `Display` form.
fn is_local_uid_str(s: &str) -> bool {
    s.split_once('~')
        .is_some_and(|(vol, _)| vol == LOCAL_VOLUME)
}

/// The API's response code for a failed call, when it failed *at* the API.
///
/// Takes `&dyn Error` so it reads a [`ProtonError`] equally well through the
/// boxes the drain deals in, where the concrete type survives but the static one
/// does not. `None` covers both "not an API error at all" (a transport failure,
/// which is what being offline looks like) and "not a `ProtonError`".
fn api_code(e: &(dyn std::error::Error + 'static)) -> Option<ResponseCode> {
    match e.downcast_ref::<ProtonError>() {
        Some(ProtonError::Api(api)) => Some(api.code),
        _ => None,
    }
}

/// Whether a call failed because the name it asked for is already in use.
///
/// The queue makes this reachable in a way the synchronous path never was: a
/// mutation queued while offline is applied against a server that may have
/// gained a file of that name in the meantime.
fn is_already_exists(e: &(dyn std::error::Error + 'static)) -> bool {
    api_code(e) == Some(ResponseCode::AlreadyExists)
}

/// Whether a call failed because the node it addressed is not there.
fn is_gone(e: &(dyn std::error::Error + 'static)) -> bool {
    api_code(e) == Some(ResponseCode::DoesNotExist)
}

/// A variant of `name` to fall back on when the remote already has that name.
///
/// Deliberately the same shape the sync engine uses for its conflict copies
/// (`sync.rs`, `conflict_path`), so the two halves of the product name the same
/// situation the same way and a user only has to learn it once.
fn conflict_name(name: &str, stamp: i64) -> String {
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => format!("{stem} (sync-conflict {stamp}).{ext}"),
        None => format!("{stem} (sync-conflict {stamp})"),
    }
}

/// Fabricate the node the server would have returned, for a `create`/`mkdir`
/// that could not reach it. Everything the kernel asks about a fresh node —
/// name, kind, size, times — is knowable locally; the uid is the only invention,
/// and the drain replaces it with the real one.
fn local_node(uid: NodeUid, parent_uid: NodeUid, name: String, is_dir: bool) -> Node {
    let now = now_secs();
    Node {
        uid,
        parent_uid: Some(parent_uid),
        kind: if is_dir {
            NodeKind::Folder
        } else {
            NodeKind::File {
                media_type: media_type_for(&name).to_string(),
                total_size_on_storage: 0,
                // No revision has been sealed: nothing has been uploaded yet.
                active_revision_state: None,
                claimed_size: Some(0),
                claimed_modification_time: None,
            }
        },
        name,
        creation_time: now,
        modification_time: now,
        trashed: false,
        is_shared: false,
        is_shared_publicly: false,
        signature_email: None,
        // Nothing signed it: it has never been near the crypto layer.
        verification: Default::default(),
    }
}

/// This machine's hostname, used to name (and later recover) its Proton Drive
/// Device. Reads the live kernel hostname, falling back to a generic label.
fn this_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Linux device".to_string())
}

/// Whether `dir` has no entries.
fn dir_is_empty(dir: &Path) -> std::io::Result<bool> {
    Ok(std::fs::read_dir(dir)?.next().is_none())
}

/// Delete everything inside `dir` but keep `dir` itself (it stays as the FUSE
/// mountpoint). Used when a `mirror` folder flips to `ondemand`: the local files
/// are the disk we're reclaiming (devices.md Phase 3).
fn evict_dir_contents(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Convert a stored synced folder into its wire form for the front-ends, with
/// the live progress of its pass when one is running.
fn sync_folder_info(f: StoredSyncFolder, progress: Option<SyncProgress>) -> SyncFolderInfo {
    SyncFolderInfo {
        id: f.id,
        local_path: f.local_path,
        remote_uid: f.remote_uid,
        mode: f.mode,
        pending_mode: f.pending_mode,
        state: f.state,
        last_sync: f.last_sync,
        progress,
    }
}

/// A coarse MIME type guessed from a file name's extension; Proton stores this
/// on the node but an exact value is not required for correctness.
fn media_type_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("txt" | "md" | "log") => "text/plain",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("html" | "htm") => "text/html",
        _ => "application/octet-stream",
    }
}

/// The plaintext size, in bytes, that a node reports.
/// Overwrite each file node's `claimed_size` with the optimistic size of its
/// queued write, where it has one. Folders and nodes with nothing queued are
/// left alone. See [`Core::stamp_pending_sizes`] for why this exists.
fn apply_pending_sizes(nodes: &mut [Node], sizes: &HashMap<NodeUid, u64>) {
    for node in nodes {
        if let Some(&len) = sizes.get(&node.uid)
            && let NodeKind::File { claimed_size, .. } = &mut node.kind
        {
            *claimed_size = Some(len as i64);
        }
    }
}

/// A size upgrade in flight for one folder, so the `getattr`s that need its
/// result wait for the one batch instead of each fetching its own node.
///
/// A plain `Condvar` rather than a channel: the waiters do not want a value,
/// only the edge, and there may be hundreds of them for one folder.
#[derive(Default)]
struct Progress {
    /// The whole batch has been applied (or failed). Nothing more is coming.
    done: bool,
    /// Bumped every time a chunk lands, so a waiter can tell "no progress since
    /// I last looked" from "progress happened while I was looking".
    generation: u64,
}

/// A size upgrade in flight for one folder, so the `getattr`s and `lookup`s that
/// need its result wait for the one batch instead of each fetching its own node.
///
/// Waiters are released **per chunk**, not once at the end: a waiter only cares
/// about its own file, and waiting for the other 792 is what let a large folder
/// outrun the timeout (bugs.md B14).
#[derive(Default)]
struct SizeUpgrade {
    inner: Mutex<Progress>,
    ready: Condvar,
}

impl SizeUpgrade {
    /// How long a caller will wait for a real size before answering with the
    /// provisional one.
    ///
    /// A `stat` that never returns is far worse than one that is briefly wrong:
    /// on timeout the caller falls back to the pre-fix behaviour rather than
    /// wedging whatever is listing the directory.
    const WAIT: Duration = Duration::from_secs(10);

    /// Block until `resolved` reports the caller's own node has a real size, the
    /// batch finishes without producing one, or [`SizeUpgrade::WAIT`] elapses.
    ///
    /// `resolved` is called with **no lock of ours held**. It reaches into
    /// `state`, and the thread applying a chunk holds `state` before it signals
    /// here — taking them in the other order would close the cycle.
    fn wait_for(&self, resolved: impl Fn() -> bool) {
        let deadline = Instant::now() + Self::WAIT;
        loop {
            let seen = {
                let progress = self.inner.lock();
                if progress.done {
                    return;
                }
                progress.generation
            };
            if resolved() {
                return;
            }
            let mut progress = self.inner.lock();
            if progress.done {
                return;
            }
            // A chunk landed while we were checking, so re-check rather than
            // sleeping through the answer we were waiting for.
            if progress.generation == seen
                && self.ready.wait_until(&mut progress, deadline).timed_out()
            {
                return;
            }
        }
    }

    /// Announce that a chunk has been applied. Every waiter re-checks its own
    /// node; the ones it resolved return, the rest go back to sleep.
    fn chunk_done(&self) {
        self.inner.lock().generation += 1;
        self.ready.notify_all();
    }

    /// Release every waiter for good. The worker must reach this on all paths.
    fn finish(&self) {
        let mut progress = self.inner.lock();
        progress.done = true;
        progress.generation += 1;
        drop(progress);
        self.ready.notify_all();
    }
}

/// Whether a `rename` may replace an existing destination, per POSIX.
///
/// Split out from the handler because it is the part that is pure and the part
/// that is dangerous: every `Err` here is a refusal that happens *before*
/// anything is trashed, and getting one wrong turns a refusal into the
/// destruction of the destination (bugs.md B13).
///
/// `dst_empty` is only meaningful when `dst_dir`; pass `true` otherwise.
fn check_replaceable(src_dir: bool, dst_dir: bool, dst_empty: bool) -> Result<(), Errno> {
    match (src_dir, dst_dir) {
        // A non-directory may not replace a directory, or vice versa.
        (false, true) => Err(Errno::EISDIR),
        (true, false) => Err(Errno::ENOTDIR),
        // Proton trashes a folder with its whole subtree, so replacing a
        // non-empty one would silently take its contents with it. POSIX says
        // ENOTEMPTY, which is also the safe answer.
        (true, true) if !dst_empty => Err(Errno::ENOTEMPTY),
        _ => Ok(()),
    }
}

fn node_size(node: &Node) -> u64 {
    match &node.kind {
        NodeKind::Folder => 0,
        NodeKind::File {
            claimed_size,
            total_size_on_storage,
            ..
        } => claimed_size.unwrap_or(*total_size_on_storage).max(0) as u64,
    }
}

/// `"1 file"` / `"3 files"` — a count with a correctly pluralised noun, for
/// human-readable activity-log lines.
fn count_noun(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Bytes rendered with a binary unit and one decimal place (e.g. `"1.2 GB"`),
/// for the activity log. Uses 1024-based steps but the shorter SI labels.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

/// A compact elapsed-time label for the activity log: `"820ms"`, `"43s"`, or
/// `"2m 5s"`.
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        format!("{}ms", d.as_millis())
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// Wall clock in milliseconds since the epoch — the unit the persisted listings
/// stamp their freshness in.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
fn unix_secs(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

#[cfg(test)]
mod size_upgrade_tests {
    use super::SizeUpgrade;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// The point of the per-chunk design: a waiter returns as soon as *its own*
    /// file is resolved, without waiting for the rest of the folder. A single
    /// batch over 793 nodes took ~80 s, which outran the timeout and handed back
    /// the provisional size this is all meant to prevent.
    #[test]
    fn a_waiter_returns_on_the_chunk_that_resolves_it() {
        let slot = Arc::new(SizeUpgrade::default());
        let mine = Arc::new(AtomicBool::new(false));
        let worker = slot.clone();
        let flag = mine.clone();
        let t = std::thread::spawn(move || {
            // Our node lands in the first chunk; two more follow.
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::SeqCst);
            worker.chunk_done();
            std::thread::sleep(Duration::from_millis(200));
            worker.chunk_done();
            worker.finish();
        });
        let started = Instant::now();
        slot.wait_for(|| mine.load(Ordering::SeqCst));
        // Released by the first chunk, not the last.
        assert!(started.elapsed() < Duration::from_millis(150));
        t.join().unwrap();
    }

    /// A chunk that does not resolve this waiter must not wake it for good.
    #[test]
    fn an_unrelated_chunk_does_not_release_a_waiter() {
        let slot = Arc::new(SizeUpgrade::default());
        let worker = slot.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            worker.chunk_done();
            std::thread::sleep(Duration::from_millis(60));
            worker.finish();
        });
        let started = Instant::now();
        // Never resolved: only `finish` can end this wait.
        slot.wait_for(|| false);
        assert!(started.elapsed() >= Duration::from_millis(80));
        t.join().unwrap();
    }

    /// A batch that ends without resolving the node — a failed fetch — still
    /// releases its waiters, who fall back to the provisional size.
    #[test]
    fn finish_releases_a_waiter_that_was_never_resolved() {
        let slot = Arc::new(SizeUpgrade::default());
        let worker = slot.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            worker.finish();
        });
        let started = Instant::now();
        slot.wait_for(|| false);
        assert!(started.elapsed() < SizeUpgrade::WAIT);
        t.join().unwrap();
    }

    /// Already resolved before waiting: must not block at all. This is the
    /// follower that arrives after the chunk it needed has landed.
    #[test]
    fn an_already_resolved_waiter_does_not_block() {
        let slot = SizeUpgrade::default();
        let started = Instant::now();
        slot.wait_for(|| true);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// Finishing before anyone waits must not strand the late arrival — the
    /// flag is what is checked, not the notification, which it would miss.
    #[test]
    fn waiting_after_finish_returns_at_once() {
        let slot = SizeUpgrade::default();
        slot.finish();
        let started = Instant::now();
        slot.wait_for(|| false);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// Every waiter is released, not just the first: one `ls -l` puts one
    /// waiter per file on the same folder.
    #[test]
    fn all_waiters_are_released() {
        let slot = Arc::new(SizeUpgrade::default());
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let slot = slot.clone();
                std::thread::spawn(move || slot.wait_for(|| false))
            })
            .collect();
        std::thread::sleep(Duration::from_millis(20));
        slot.finish();
        for w in waiters {
            w.join().expect("every waiter returns");
        }
    }

    /// The timeout is the backstop: a batch that never finishes must not wedge
    /// the caller's `stat` forever.
    #[test]
    fn a_waiter_gives_up_at_the_timeout() {
        let slot = SizeUpgrade::default();
        let started = Instant::now();
        // Nothing will ever resolve or finish this.
        slot.wait_for(|| false);
        assert!(started.elapsed() >= SizeUpgrade::WAIT);
    }
}

#[cfg(test)]
mod replace_tests {
    use super::check_replaceable;

    /// The case that motivated all of this: rsync renaming its temp file over
    /// the real one. Two plain files, and it has to be allowed — refusing is
    /// what made every rsync transfer fail at the last step (bugs.md B13).
    #[test]
    fn a_file_may_replace_a_file() {
        assert!(check_replaceable(false, false, true).is_ok());
    }

    #[test]
    fn an_empty_directory_may_be_replaced_by_a_directory() {
        assert!(check_replaceable(true, true, true).is_ok());
    }

    /// Proton trashes a folder with its whole subtree, so allowing this would
    /// discard every file under the destination without ever naming them.
    #[test]
    fn a_non_empty_directory_is_never_replaced() {
        let e = check_replaceable(true, true, false).expect_err("must refuse");
        assert_eq!(e.code(), libc::ENOTEMPTY);
    }

    #[test]
    fn the_two_ends_must_agree_on_being_a_directory() {
        assert_eq!(
            check_replaceable(false, true, true)
                .expect_err("a file may not replace a directory")
                .code(),
            libc::EISDIR
        );
        assert_eq!(
            check_replaceable(true, false, true)
                .expect_err("a directory may not replace a file")
                .code(),
            libc::ENOTDIR
        );
    }

    /// `dst_empty` describes a directory; it must not leak into the file case,
    /// where callers pass `true` by convention.
    #[test]
    fn emptiness_is_ignored_when_the_destination_is_a_file() {
        assert!(check_replaceable(false, false, false).is_ok());
    }
}
#[cfg(test)]
mod local_uid_tests {
    use super::*;

    #[test]
    fn a_minted_uid_is_recognisable_and_round_trips() {
        let uid = mint_local_uid();
        assert!(is_local_uid(&uid));
        assert!(is_local_uid_str(&uid.to_string()));

        // It has to survive the trip through `pending_op.uid` as text, like any
        // other uid does.
        let parsed = parse_node_uid(&uid.to_string()).expect("parses back");
        assert_eq!(parsed, uid);
    }

    #[test]
    fn minted_uids_are_distinct_within_a_run() {
        // Two files created in the same millisecond must not collide — the whole
        // queue is keyed by uid.
        let a = mint_local_uid();
        let b = mint_local_uid();
        assert_ne!(a, b);
    }

    #[test]
    fn a_real_uid_is_never_mistaken_for_a_placeholder() {
        let real = NodeUid::new(VolumeId::from("vol1"), LinkId::from("link1"));
        assert!(!is_local_uid(&real));
        assert!(!is_local_uid_str("vol1~link1"));
        // Not a uid at all.
        assert!(!is_local_uid_str("local"));
        // The sentinel is the *volume*; a link that merely says "local" is real.
        assert!(!is_local_uid_str("vol1~local"));
    }

    #[test]
    fn a_placeholder_file_reports_itself_as_empty_and_unsealed() {
        let parent = NodeUid::new(VolumeId::from("vol1"), LinkId::from("dir"));
        let node = local_node(mint_local_uid(), parent.clone(), "notes.txt".into(), false);

        assert_eq!(node.name, "notes.txt");
        assert_eq!(node.parent_uid, Some(parent));
        assert!(!node.trashed);
        match node.kind {
            NodeKind::File {
                claimed_size,
                active_revision_state,
                ref media_type,
                ..
            } => {
                assert_eq!(claimed_size, Some(0));
                // Nothing has been uploaded, so there is no sealed revision.
                assert!(active_revision_state.is_none());
                assert_eq!(media_type, "text/plain");
            }
            NodeKind::Folder => panic!("expected a file"),
        }
    }

    #[test]
    fn a_placeholder_folder_is_a_folder() {
        let parent = NodeUid::new(VolumeId::from("vol1"), LinkId::from("root"));
        let node = local_node(mint_local_uid(), parent, "photos".into(), true);
        assert!(node.is_folder());
    }
}

#[cfg(test)]
mod pending_size_tests {
    use super::{Node, NodeKind, NodeUid, apply_pending_sizes, node_size};
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};
    use std::collections::HashMap;

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn file(link: &str, claimed: i64) -> Node {
        Node {
            uid: uid(link),
            parent_uid: Some(uid("parent")),
            kind: NodeKind::File {
                media_type: "text/plain".into(),
                total_size_on_storage: 0,
                active_revision_state: None,
                claimed_size: Some(claimed),
                claimed_modification_time: None,
            },
            name: link.into(),
            creation_time: 100,
            modification_time: 100,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            verification: Default::default(),
        }
    }

    fn folder(link: &str) -> Node {
        Node {
            kind: NodeKind::Folder,
            ..file(link, 0)
        }
    }

    /// B11: a re-enumeration mid-write must not revert the size to the
    /// server's. A file that stats as 0 gets no `read` from the kernel at all,
    /// so the staged blob is never served and the file reads as empty.
    #[test]
    fn a_queued_write_keeps_its_optimistic_size_through_a_re_enumeration() {
        let mut nodes = vec![file("queued", 0), file("settled", 4096)];
        let sizes = HashMap::from([(uid("queued"), 3)]);

        apply_pending_sizes(&mut nodes, &sizes);

        assert_eq!(
            node_size(&nodes[0]),
            3,
            "the remote's pre-write size must not win over the queued write's"
        );
        assert_eq!(
            node_size(&nodes[1]),
            4096,
            "a file with nothing queued keeps the size the server reported"
        );
    }

    /// The map is keyed by uid and says nothing about kind; a folder that
    /// somehow collides must not grow a `claimed_size`.
    #[test]
    fn folders_are_left_alone() {
        let mut nodes = vec![folder("dir")];
        apply_pending_sizes(&mut nodes, &HashMap::from([(uid("dir"), 999)]));
        assert!(matches!(nodes[0].kind, NodeKind::Folder));
        assert_eq!(node_size(&nodes[0]), 0);
    }

    /// The common case: nothing queued, nothing touched.
    #[test]
    fn an_empty_pending_map_changes_nothing() {
        let mut nodes = vec![file("a", 10), file("b", 20)];
        apply_pending_sizes(&mut nodes, &HashMap::new());
        assert_eq!(node_size(&nodes[0]), 10);
        assert_eq!(node_size(&nodes[1]), 20);
    }
}

#[cfg(test)]
mod tests {
    use super::{Intervals, conflict_name, is_stale_mount};

    /// The predicate must answer *only* for a dead FUSE connection. A healthy
    /// directory and an absent path are both "not stale" — widening it to any
    /// `metadata` error would make the on-demand restore lazily unmount paths
    /// that are simply missing.
    #[test]
    fn is_stale_mount_is_narrow() {
        let dir = std::env::temp_dir().join(format!("pdfs-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_stale_mount(&dir));
        assert!(!is_stale_mount(&dir.join("no-such-entry")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Flatten `segments` into a readable form for assertions.
    fn segs(iv: &Intervals, start: u64, end: u64) -> Vec<(u64, u64, bool)> {
        iv.segments(start, end)
    }

    /// The conflict copy has to stay openable, so the extension survives — and
    /// the shape matches the sync engine's `conflict_path` so the two features
    /// name the same situation the same way.
    #[test]
    fn conflict_name_keeps_the_extension() {
        assert_eq!(
            conflict_name("notes.txt", 1700),
            "notes (sync-conflict 1700).txt"
        );
        assert_eq!(conflict_name("README", 42), "README (sync-conflict 42)");
        assert_eq!(
            conflict_name("archive.tar.gz", 7),
            "archive.tar (sync-conflict 7).gz",
            "only the last extension is one, as everywhere else"
        );
    }

    #[test]
    fn append_merges_into_one_run() {
        // Sequential writes (an append) coalesce into a single authored range.
        let mut iv = Intervals::default();
        iv.add(0, 10);
        iv.add(10, 20);
        iv.add(20, 25);
        assert_eq!(segs(&iv, 0, 25), vec![(0, 25, true)]);
    }

    #[test]
    fn partial_overwrite_leaves_base_gap() {
        // Author [0,4) and [8,12); [4,8) stays base. A read of [0,12) must stitch
        // authored / base / authored in order.
        let mut iv = Intervals::default();
        iv.add(0, 4);
        iv.add(8, 12);
        assert_eq!(
            segs(&iv, 0, 12),
            vec![(0, 4, true), (4, 8, false), (8, 12, true)]
        );
    }

    #[test]
    fn overlapping_writes_coalesce() {
        let mut iv = Intervals::default();
        iv.add(0, 10);
        iv.add(5, 15);
        iv.add(14, 20);
        assert_eq!(segs(&iv, 0, 20), vec![(0, 20, true)]);
    }

    #[test]
    fn segments_clamp_to_request_window() {
        let mut iv = Intervals::default();
        iv.add(0, 100);
        // A sub-window of one big authored range is a single authored segment.
        assert_eq!(segs(&iv, 20, 50), vec![(20, 50, true)]);
        // A window entirely outside any authored range is all base.
        let empty = Intervals::default();
        assert_eq!(segs(&empty, 0, 8), vec![(0, 8, false)]);
    }

    #[test]
    fn truncate_shrink_drops_tail() {
        // Grow-then-shrink: clip removes/truncates authored ranges past the end.
        let mut iv = Intervals::default();
        iv.add(0, 100);
        iv.clip(40);
        assert_eq!(segs(&iv, 0, 40), vec![(0, 40, true)]);
        // Authored ranges wholly past the new end disappear.
        let mut iv2 = Intervals::default();
        iv2.add(0, 10);
        iv2.add(50, 60);
        iv2.clip(40);
        assert_eq!(segs(&iv2, 0, 40), vec![(0, 10, true), (10, 40, false)]);
    }

    #[test]
    fn truncate_extend_authors_zero_tail() {
        // setattr grow claims the new tail as authored (defined zeros), so commit
        // never pulls it from the base.
        let mut iv = Intervals::default();
        iv.add(0, 10); // base content authored over
        let old_len = 10u64;
        let new_len = 30u64;
        iv.add(old_len, new_len);
        assert_eq!(segs(&iv, 0, 30), vec![(0, 30, true)]);
    }

    use proton_drive_rs::proton_sdk::ids::{LinkId, NodeUid, VolumeId};
    use proton_drive_rs::{Node, NodeKind};

    #[test]
    fn test_posix_unlink_and_rmdir_checks() {
        let (mut st, _dir) = state_test_helper();
        let parent = st.intern(0, node_helper("parent_uid", "none", "parent", true));
        let folder_child = st.intern(
            parent,
            node_helper("folder_child", "parent_uid", "subfolder", true),
        );
        let file_child = st.intern(
            parent,
            node_helper("file_child", "parent_uid", "file.txt", false),
        );
        st.children.insert(parent, vec![folder_child, file_child]);

        // unlink on folder -> EISDIR check
        let folder_entry = st.entries.get(&folder_child).unwrap();
        assert!(folder_entry.node.is_folder(), "folder_child is a folder");

        // rmdir on file -> ENOTDIR check
        let file_entry = st.entries.get(&file_child).unwrap();
        assert!(!file_entry.node.is_folder(), "file_child is a file");

        // rmdir on non-empty parent -> ENOTEMPTY check
        assert!(st.has_children(parent), "parent is not empty");
    }

    struct TestDir(std::path::PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state_test_helper() -> (crate::state::State, TestDir) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir_path = std::env::temp_dir().join(format!(
            "pdfs-lib-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir_path).unwrap();
        let db = pdfs_core::db::Db::open(&dir_path.join("cache.db")).unwrap();
        let st = crate::state::State {
            entries: std::collections::HashMap::new(),
            by_uid: std::collections::HashMap::new(),
            children: std::collections::HashMap::new(),
            next_ino: 1,
            active_writes: std::collections::HashMap::new(),
            handles: std::collections::HashMap::new(),
            next_fh: 1,
            db: std::sync::Arc::new(db),
        };
        (st, TestDir(dir_path))
    }

    fn node_helper(id: &str, parent: &str, name: &str, is_dir: bool) -> Node {
        let uid = NodeUid::new(VolumeId::from("vol"), LinkId::from(id));
        let parent_uid = if parent == "none" {
            None
        } else {
            Some(NodeUid::new(VolumeId::from("vol"), LinkId::from(parent)))
        };
        Node {
            uid,
            parent_uid,
            name: name.to_string(),
            kind: if is_dir {
                NodeKind::Folder
            } else {
                NodeKind::File {
                    media_type: "text/plain".into(),
                    total_size_on_storage: 0,
                    active_revision_state: None,
                    claimed_size: Some(0),
                    claimed_modification_time: None,
                }
            },
            creation_time: 100,
            modification_time: 100,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            verification: Default::default(),
        }
    }
}
