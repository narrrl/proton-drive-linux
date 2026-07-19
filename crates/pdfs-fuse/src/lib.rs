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
//! [`WriteHandle`] whose buffer accumulates the full plaintext; on flush/release
//! the buffer is sealed as a new revision via
//! [`ProtonDriveClient::upload_new_revision`] (the SDK uploads whole revisions,
//! not byte ranges). New files are created empty up front so they get a real
//! uid; namespace ops map to `create_folder`, `trash_nodes`, `rename_node` and
//! `move_node`.
//!
//! Phase 4 adds Files-On-Demand pinning. A control socket (see [`control`])
//! lets the CLI pin/unpin files; a pinned file's plaintext is downloaded once
//! into the on-disk [`ContentCache`] and every later `read` is served from disk
//! instead of the network. Writes and remote events evict the cache so it never
//! goes stale.
//!
//! Reads of unpinned files no longer hit the network per call: [`Core::read_range`]
//! fetches and caches [`BLOCK_SIZE`]-aligned blocks, so sequential/sparse reads
//! reuse the on-disk block cache. Writes are disk-backed: each [`WriteHandle`]
//! stages authored bytes in a scratch file and tracks them with an [`Intervals`]
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::ReplyXattr;
use fuser::{
    BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, LockOwner, MountOption, Notifier, OpenAccMode, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, Session, TimeOrNow, WriteFlags,
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

mod control;
use control::run_control_socket;
mod devices;
mod drain;
mod photos;
mod sharing;
mod state;
mod sync;
mod transfers;
mod workers;
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

/// Longest edge, in px, of a thumbnail generated locally for a photo the server
/// has none for. Matches the server's own thumbnail scale closely enough that a
/// tile can't tell them apart.
const THUMB_EDGE: u32 = 512;
/// JPEG quality of a locally generated thumbnail.
const THUMB_QUALITY: u8 = 82;
/// How many photos may be downloaded at once to generate their missing
/// thumbnails. Bounded: a screenful of 20 MB digicam JPEGs would otherwise
/// saturate the link and starve the rest of the daemon.
const THUMB_GEN_CONCURRENCY: usize = 4;

/// A read of an unpinned video at least this large streams *without* persisting
/// its blocks to the on-disk cache. Playing a 2 GB film would otherwise pour it
/// through the block LRU and evict everything else the user actually wants kept —
/// and it re-streams cheaply enough on a rewatch that keeping it was never worth
/// that. Pinned videos (kept offline on purpose) and anything smaller cache as
/// usual.
const STREAM_BYPASS_MIN: u64 = 256 * 1024 * 1024;

/// How many bytes of streamed blocks the in-memory ring keeps. Bypassing the
/// on-disk cache must not mean re-fetching: the kernel asks for a streamed file
/// in reads far smaller than [`BLOCK_SIZE`], so without a ring every 128 KiB the
/// player consumes would download and decrypt a whole 4 MiB block again. Sized
/// for a handful of blocks per concurrently-streamed file.
const STREAM_RING_BYTES: u64 = 128 * 1024 * 1024;

/// Blocks fetched *past* the one a sequential streaming read needed, warmed into
/// the ring in the background so the player's next read is already in memory
/// instead of waiting a round-trip. Kept small so it cannot crowd out the
/// in-flight block governor.
const STREAM_READAHEAD: u64 = 4;

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

/// How many files the bulk uploader ships at once. Overlaps the per-file network
/// round-trips without letting an unbounded number of block buffers pile up.
const UPLOAD_CONCURRENCY: usize = 4;

/// One file queued for bulk upload, resolved during the directory walk so the
/// concurrent phase carries everything it needs (no shared state, no `block_on`).
struct UploadTask {
    /// Inode of the (already-created) remote parent folder, for interning the
    /// uploaded node afterwards.
    parent_ino: u64,
    parent_uid: NodeUid,
    name: String,
    /// Local filesystem path to stream from.
    path: PathBuf,
    size: u64,
}

/// Tally of a completed [`Core::upload_paths`] batch, for the daemon log.
#[derive(Default)]
struct UploadStats {
    uploaded: usize,
    failed: usize,
    /// Total plaintext bytes of the files that uploaded successfully.
    bytes: u64,
    /// Folders created (or reused) to mirror the local tree.
    folders: usize,
}

/// Upload every [`UploadTask`] with at most `limit` in flight at once, each
/// streamed straight from disk and ticking its own transfer-registry guard.
/// Returns, per file, either `(parent_ino, new_uid)` for the caller to intern or
/// `(name, error)` to log — one failure never sinks the batch.
///
/// `job` counts files finished (either way: a failure is still one file the batch
/// no longer waits on), so a front-end can show "12 of 40" over the per-file bars.
async fn run_uploads(
    core: Core,
    tasks: Vec<UploadTask>,
    limit: usize,
    job: Arc<JobGuard>,
) -> Vec<Result<(u64, NodeUid, u64), (String, String)>> {
    let sem = Arc::new(tokio::sync::Semaphore::new(limit));
    let mut set = tokio::task::JoinSet::new();
    for t in tasks {
        let core = core.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore not closed");
            let file = match std::fs::File::open(&t.path) {
                Ok(f) => f,
                Err(e) => return Err((t.name, format!("open {}: {e}", t.path.display()))),
            };
            let mtime = std::fs::metadata(&t.path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mt| mt.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let guard = core
                .transfers
                .begin(&t.name, "", TransferDirection::Upload, t.size);
            let reader = OwnedCountingReader::new(file, guard);
            match core
                .client
                .upload_file_from(
                    &t.parent_uid,
                    &t.name,
                    media_type_for(&t.name),
                    reader,
                    t.size as i64,
                    Vec::new(),
                    mtime,
                    false,
                )
                .await
            {
                Ok(uid) => Ok((t.parent_ino, uid, t.size)),
                Err(e) => Err((t.name, format!("upload: {e}"))),
            }
        });
    }
    let mut out = Vec::new();
    while let Some(joined) = set.join_next().await {
        job.step();
        match joined {
            Ok(result) => out.push(result),
            Err(e) => warn!(error = %e, "upload task panicked"),
        }
    }
    out
}

/// Shared engine behind both the FUSE callbacks and the control socket: the
/// How many [`RevisionReader`]s stay open at once.
///
/// A reader holds its revision's content key and block table — a few KB even for
/// a large file, so this is bounded for tidiness and staleness rather than
/// memory. Evicted least-recently-used; a dropped reader costs one re-open
/// (two API calls and a node-key unlock) the next time that file is read.
const MAX_OPEN_READERS: usize = 64;

/// An open reader plus the node metadata it was opened against.
///
/// The SDK pins a reader to the revision that was active at `open_revision`, so
/// a reader is only reusable while the node still reports the same
/// `(mtime, size)` — the same validity pair the content cache uses (a new
/// revision bumps mtime). On a mismatch the reader is dropped and reopened.
struct CachedReader {
    reader: Arc<RevisionReader>,
    mtime: i64,
    size: u64,
    /// For LRU eviction.
    last_used: Instant,
}

/// What the `readers` map holds for a node: an open reader, or the fact that
/// someone is already opening one.
///
/// The `Pending` arm is what makes the map single-flight rather than merely a
/// cache. Opening resolves link details, ancestor keys, an S2K node-key unlock
/// and the block table — five round-trips — and read-ahead on a cold file puts
/// several workers into the open at the same moment, so without this each
/// racer would redo all of it and discard every result but one.
enum ReaderSlot {
    Ready(CachedReader),
    Pending(PendingOpen),
}

/// An `open_revision` in flight. Racers clone `rx` and await the leader's result
/// instead of opening their own.
struct PendingOpen {
    /// Identifies this attempt so the leader can tell "my slot is still there"
    /// from "someone evicted or replaced it while I was on the network", and
    /// only publish its reader in the first case.
    id: u64,
    /// What the leader is opening against. A racer wanting a different revision
    /// must not join this open — it would get a reader for the wrong bytes.
    mtime: i64,
    size: u64,
    rx: tokio::sync::watch::Receiver<Option<Result<Arc<RevisionReader>, Errno>>>,
}

/// Distinguishes `PendingOpen`s. Only uniqueness matters, so a plain counter
/// does; ids are never compared for order.
static NEXT_OPEN_ID: AtomicU64 = AtomicU64::new(0);

/// Drive client, a Tokio handle to bridge the synchronous FUSE/socket threads
/// to the async SDK, the inode bookkeeping, and the on-disk content cache.
///
/// Cheaply cloneable (every field is a handle/`Arc`), so the control-socket task
/// gets its own copy while the FUSE session keeps another.
/// In-memory LRU of blocks belonging to a file streaming past the on-disk cache
/// (see [`STREAM_BYPASS_MIN`]). Bypassing the disk stops one film evicting the
/// block LRU; it must not also mean the same 4 MiB block is downloaded once per
/// 128 KiB the player reads out of it.
///
/// Validated by the same `(mtime, size)` pair the on-disk caches use: a node
/// whose tag no longer matches has its blocks dropped rather than served, so a
/// new revision can never be stitched together from the old one.
#[derive(Default)]
struct StreamRing {
    blocks: HashMap<(NodeUid, u64), Arc<Vec<u8>>>,
    /// Keys oldest-first, for eviction.
    order: VecDeque<(NodeUid, u64)>,
    /// Per-node validity tag, `(mtime, size)`.
    tags: HashMap<NodeUid, (i64, u64)>,
    bytes: u64,
}

impl StreamRing {
    fn get(&mut self, uid: &NodeUid, mtime: i64, size: u64, idx: u64) -> Option<Arc<Vec<u8>>> {
        if self.tags.get(uid) != Some(&(mtime, size)) {
            return None;
        }
        self.blocks.get(&(uid.clone(), idx)).cloned()
    }

    fn insert(&mut self, uid: &NodeUid, mtime: i64, size: u64, idx: u64, bytes: Arc<Vec<u8>>) {
        if self.tags.get(uid) != Some(&(mtime, size)) {
            // Revision changed under us (or first sight): anything held for this
            // node describes the old one.
            self.drop_node(uid);
            self.tags.insert(uid.clone(), (mtime, size));
        }
        let key = (uid.clone(), idx);
        if self.blocks.contains_key(&key) {
            return;
        }
        self.bytes += bytes.len() as u64;
        self.blocks.insert(key.clone(), bytes);
        self.order.push_back(key);
        while self.bytes > STREAM_RING_BYTES {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.blocks.remove(&victim) {
                self.bytes -= dropped.len() as u64;
            }
        }
    }

    fn drop_node(&mut self, uid: &NodeUid) {
        self.order.retain(|(u, _)| u != uid);
        let mut freed = 0u64;
        self.blocks.retain(|(u, _), bytes| {
            if u == uid {
                freed += bytes.len() as u64;
                false
            } else {
                true
            }
        });
        self.bytes -= freed;
        self.tags.remove(uid);
    }
}

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
    /// back to `mirror` and on daemon shutdown.
    mounts: Arc<Mutex<HashMap<i64, BackgroundSession>>>,
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
            st.entries.insert(ino, Entry { uid, parent, node });
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
            Ok(Some(nodes)) => {
                let mut st = self.state.lock();
                if st.children.contains_key(&ino) {
                    return Ok(());
                }
                let mut child_inos = Vec::with_capacity(nodes.len());
                for node in nodes {
                    if node.trashed || node.uid == folder_uid {
                        continue;
                    }
                    child_inos.push(st.intern_from_db(ino, node));
                }
                st.children.insert(ino, child_inos);
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
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| {
                error!(%folder_uid, error = %e, "enumerate nodes failed");
                Errno::EIO
            })?;

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
        let inos = st.intern_batch(ino, filtered_nodes);
        child_inos.extend(inos);
        st.children.insert(ino, child_inos);
        // Record the listing as complete so a later restart (or a trimmed hot
        // cache) can rebuild it from the DB without the API.
        if let Err(e) = self.db.set_listed(&folder_uid, true) {
            warn!(%folder_uid, error = %e, "db set_listed(true) failed");
        }
        Ok(())
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

    /// An open [`RevisionReader`] for `uid`, reusing the cached one when it is
    /// still valid for `(mtime, fsize)` and opening a fresh one otherwise.
    ///
    /// Opening resolves the file's link details, ancestor keys, node key (an S2K
    /// unlock) and block table. Doing that once per file rather than once per
    /// block is the whole point: a cold 100 MB read is 25 block misses, which
    /// used to mean 25 full resolutions (50 API calls, 25 unlocks) and now means
    /// one.
    ///
    /// Racing reads of one cold file open it **once**: the first caller claims
    /// the slot and the rest await its result. That matters now that `read` is
    /// served off the dispatch loop, because read-ahead on a first touch issues
    /// several parallel reads of the same file as a matter of course.
    async fn revision_reader(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
    ) -> Result<Arc<RevisionReader>, Errno> {
        loop {
            // Decide under the lock, act outside it — network I/O must never run
            // under a std `Mutex`, and an await must never hold one.
            enum Act {
                Hit(Arc<RevisionReader>),
                Join(tokio::sync::watch::Receiver<Option<Result<Arc<RevisionReader>, Errno>>>),
                Lead(
                    u64,
                    tokio::sync::watch::Sender<Option<Result<Arc<RevisionReader>, Errno>>>,
                ),
            }
            let act = {
                let mut readers = self.readers.lock();
                match readers.get_mut(uid) {
                    Some(ReaderSlot::Ready(entry))
                        if entry.mtime == mtime && entry.size == fsize =>
                    {
                        entry.last_used = Instant::now();
                        Act::Hit(entry.reader.clone())
                    }
                    Some(ReaderSlot::Pending(p)) if p.mtime == mtime && p.size == fsize => {
                        Act::Join(p.rx.clone())
                    }
                    // Either nothing here, or something for a revision we no
                    // longer want (a stale reader, or an open for one). Ours
                    // replaces it: the newer `(mtime, size)` is the truth.
                    _ => {
                        let id = NEXT_OPEN_ID.fetch_add(1, Ordering::Relaxed);
                        let (tx, rx) = tokio::sync::watch::channel(None);
                        readers.insert(
                            uid.clone(),
                            ReaderSlot::Pending(PendingOpen {
                                id,
                                mtime,
                                size: fsize,
                                rx,
                            }),
                        );
                        Act::Lead(id, tx)
                    }
                }
            };

            match act {
                Act::Hit(reader) => return Ok(reader),
                Act::Join(mut rx) => {
                    // `changed()` errors only if the leader dropped its sender
                    // without publishing — it panicked. Retry from the top
                    // rather than fail: the slot it left is gone, so this pass
                    // leads its own open.
                    loop {
                        if let Some(result) = rx.borrow_and_update().clone() {
                            return result;
                        }
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                Act::Lead(id, tx) => {
                    debug!(%uid, mtime, fsize, "opening revision");
                    let result = match self.client.open_revision(uid).await {
                        Ok(reader) => Ok(Arc::new(reader)),
                        Err(e) => {
                            warn!(%uid, error = %e, "open_revision failed");
                            Err(Errno::EIO)
                        }
                    };
                    {
                        let mut readers = self.readers.lock();
                        // Publish only into the slot we claimed. If it is gone
                        // or belongs to a later attempt, an eviction or a newer
                        // revision overtook us: our reader still answers the
                        // callers waiting on it, but it must not be cached.
                        let ours = matches!(
                            readers.get(uid),
                            Some(ReaderSlot::Pending(p)) if p.id == id
                        );
                        if ours {
                            match &result {
                                Ok(reader) => {
                                    readers.insert(
                                        uid.clone(),
                                        ReaderSlot::Ready(CachedReader {
                                            reader: reader.clone(),
                                            mtime,
                                            size: fsize,
                                            last_used: Instant::now(),
                                        }),
                                    );
                                }
                                // Never cache a failure: the next read retries.
                                Err(_) => {
                                    readers.remove(uid);
                                }
                            }
                        }
                        Self::trim_readers(&mut readers);
                    }
                    // After the map is updated, so a racer that wakes and
                    // re-enters finds the finished slot rather than racing us.
                    let _ = tx.send(Some(result.clone()));
                    return result;
                }
            }
        }
    }

    /// Bound the reader map: drop least-recently-used entries until it is back
    /// at [`MAX_OPEN_READERS`]. Only `Ready` slots are eviction candidates — a
    /// `Pending` has callers waiting on it and no reader to drop yet.
    fn trim_readers(readers: &mut HashMap<NodeUid, ReaderSlot>) {
        while readers.len() > MAX_OPEN_READERS {
            let victim = readers
                .iter()
                .filter_map(|(uid, slot)| match slot {
                    ReaderSlot::Ready(entry) => Some((uid, entry.last_used)),
                    ReaderSlot::Pending(_) => None,
                })
                .min_by_key(|(_, last_used)| *last_used)
                .map(|(uid, _)| uid.clone());
            let Some(victim) = victim else {
                break; // every slot is an open in flight; nothing to drop
            };
            readers.remove(&victim);
        }
    }

    /// Drop any open reader for `uid`, so the next read reopens against the
    /// current revision. Called wherever cached content is evicted.
    ///
    /// Drops an in-flight open too: its leader finds the slot gone and declines
    /// to cache what it opened, which is what we want — the eviction says that
    /// revision is no longer the truth.
    fn evict_reader(&self, uid: &NodeUid) {
        self.readers.lock().remove(uid);
    }

    /// Serve bytes `[offset, offset + len)` of `uid`'s active revision, hitting
    /// the on-disk caches before the network: a whole-file blob (pinned files)
    /// first, then the block cache — fetching only the [`BLOCK_SIZE`]-aligned
    /// blocks that overlap the request and caching each. `mtime`/`fsize` validate
    /// both caches. Network I/O runs without any lock held.
    fn read_range(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        offset: u64,
        len: u64,
        cache_blocks: bool,
    ) -> Result<Vec<u8>, Errno> {
        // A queued write has not reached the remote yet, so the remote's current
        // revision is stale and the staged blob is the truth. Serve from it until
        // the drain worker lands the upload (offline.md Phase 3).
        if let Some(pending) = self.pending.lock().get(uid).cloned() {
            return self.read_pending(&pending, offset, len);
        }
        // A node created offline and never written has no blob and no remote: it
        // is an empty file, and asking the API about a `local~` uid would only
        // earn a 404 (offline.md Phase 3b).
        if is_local_uid(uid) {
            return Ok(Vec::new());
        }
        self.read_range_remote(uid, mtime, fsize, offset, len, cache_blocks)
    }

    /// Serve a read from a staged blob, falling back to the remote base for any
    /// range the write did not author (an incomplete [`StagedWrite`] holds zeros
    /// there, which must never be handed out as content).
    fn read_pending(
        &self,
        pending: &PendingRevision,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, Errno> {
        let m = &pending.meta;
        if offset >= m.len || len == 0 {
            return Ok(Vec::new());
        }
        let uid = parse_node_uid(&m.uid).ok_or(Errno::EIO)?;
        let file = File::open(&pending.path).map_err(|e| {
            error!(%uid, path = %pending.path.display(), error = %e, "open staged blob failed");
            Errno::EIO
        })?;
        let mut written = Intervals::default();
        for &(s, e) in &m.authored {
            written.add(s, e);
        }
        // Same merge as `serve_open_read`, but resolving gaps against the remote
        // rather than through `read_range` — going through `read_range` would find
        // this very pending op and recurse.
        let end = offset.saturating_add(len).min(m.len);
        let mut out = Vec::with_capacity((end - offset) as usize);
        for (s, e, authored) in written.segments(offset, end) {
            if authored {
                let mut buf = vec![0u8; (e - s) as usize];
                file.read_exact_at(&mut buf, s).map_err(|err| {
                    warn!(%uid, error = %err, "staged blob read failed");
                    Errno::EIO
                })?;
                out.extend_from_slice(&buf);
                continue;
            }
            let bend = e.min(m.base_size);
            if s < bend {
                out.extend_from_slice(&self.read_range_remote(
                    &uid,
                    m.base_mtime,
                    m.base_size,
                    s,
                    bend - s,
                    true,
                )?);
            }
            // Past the base: a hole the write extended over.
            out.resize(out.len() + e.saturating_sub(s.max(m.base_size)) as usize, 0);
        }
        Ok(out)
    }

    /// Warm the [`STREAM_READAHEAD`] blocks after `last` into the ring, in the
    /// background, so a player reading forward finds its next block already in
    /// memory rather than paying a fetch + decrypt round-trip at each boundary.
    ///
    /// Fire-and-forget: a failure here is not an error, it only means the read
    /// that needs those bytes will fetch them itself.
    fn stream_readahead(&self, uid: &NodeUid, mtime: i64, fsize: u64, last: u64) {
        let last_block = (fsize.saturating_sub(1)) / BLOCK_SIZE;
        let wanted: Vec<u64> = ((last + 1)..=(last + STREAM_READAHEAD).min(last_block))
            .filter(|&bidx| {
                self.stream_ring
                    .lock()
                    .get(uid, mtime, fsize, bidx)
                    .is_none()
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        let core = self.clone();
        let uid = uid.clone();
        self.rt.spawn(async move {
            let Ok(reader) = core.revision_reader(&uid, mtime, fsize).await else {
                return;
            };
            // Concurrently, for the same reason the demand path fetches its
            // misses concurrently: serially awaited blocks cost the sum of their
            // round-trips, and the player catches up with the readahead before it
            // finishes. The SDK's in-flight block governor bounds the fan-out.
            let mut set = tokio::task::JoinSet::new();
            for bidx in wanted {
                let reader = reader.clone();
                let bstart = bidx * BLOCK_SIZE;
                let blen = BLOCK_SIZE.min(fsize - bstart);
                set.spawn(async move { (bidx, reader.read_at(bstart, blen).await) });
            }
            while let Some(joined) = set.join_next().await {
                let Ok((bidx, read)) = joined else { continue };
                match read {
                    Ok(bytes) => {
                        core.stream_ring
                            .lock()
                            .insert(&uid, mtime, fsize, bidx, Arc::new(bytes))
                    }
                    Err(e) => debug!(%uid, bidx, error = %e, "stream readahead failed"),
                }
            }
        });
    }

    /// Read from the content cache, else the remote. The base-content path, with
    /// no awareness of queued writes — callers wanting the file's *current*
    /// content want [`Core::read_range`]. Gap-filling a staged blob is the one
    /// caller that genuinely means "the base", since that is what its zeroed
    /// ranges have to be filled from.
    fn read_range_remote(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        offset: u64,
        len: u64,
        cache_blocks: bool,
    ) -> Result<Vec<u8>, Errno> {
        if let Some(bytes) = self.cache.read_range(uid, mtime, fsize, offset, len) {
            return Ok(bytes);
        }
        if offset >= fsize || len == 0 {
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(len).min(fsize);
        let mut out = Vec::with_capacity((end - offset) as usize);
        let first = offset / BLOCK_SIZE;
        let last = (end - 1) / BLOCK_SIZE;

        // Collect the blocks overlapping the request, serving any already cached
        // and fetching the rest concurrently. A multi-block read (e.g. a media
        // player buffering, or a large sequential read split into one FUSE call)
        // would otherwise stall on each block round-trip in turn; downloading the
        // misses in parallel saturates the connection and bounds latency at the
        // slowest single block instead of their sum.
        let mut blocks: Vec<Option<Arc<Vec<u8>>>> = Vec::with_capacity((last - first + 1) as usize);
        let mut misses: Vec<u64> = Vec::new();
        for bidx in first..=last {
            // A streaming read keeps its blocks only in memory, so the ring is
            // the only cache it has — check it before the disk (which will not
            // hold them) and before the network (which would refetch the same
            // 4 MiB block for every 128 KiB the player consumes).
            let hit = if cache_blocks {
                self.cache
                    .cached_block(uid, mtime, fsize, bidx)
                    .map(Arc::new)
            } else {
                self.stream_ring.lock().get(uid, mtime, fsize, bidx)
            };
            match hit {
                Some(b) => blocks.push(Some(b)),
                None => {
                    blocks.push(None);
                    misses.push(bidx);
                }
            }
        }

        if !misses.is_empty() {
            let fetched = self.rt.block_on(async {
                // Resolve the file's keys and block table once, then read every
                // missing block through the shared reader. Previously each block
                // called `download_range`, which redid that resolution per block.
                let reader = self.revision_reader(uid, mtime, fsize).await?;

                let mut set = tokio::task::JoinSet::new();
                for &bidx in &misses {
                    let reader = reader.clone();
                    let uid = uid.clone();
                    let bstart = bidx * BLOCK_SIZE;
                    let blen = BLOCK_SIZE.min(fsize - bstart);
                    set.spawn(async move {
                        reader
                            .read_at(bstart, blen)
                            .await
                            .map(|bytes| (bidx, bytes))
                            .map_err(|e| {
                                warn!(%uid, bstart, blen, error = %e, "block read failed");
                                Errno::EIO
                            })
                    });
                }
                let mut out = Vec::with_capacity(misses.len());
                while let Some(joined) = set.join_next().await {
                    // A join error means the task panicked; surface it as EIO.
                    out.push(joined.map_err(|_| Errno::EIO)??);
                }
                Ok::<_, Errno>(out)
            })?;
            for (bidx, bytes) in fetched {
                // A streaming read (large unpinned video) skips the on-disk cache
                // so it can't evict the rest of it; it keeps the block in the
                // in-memory ring instead, which is bounded and dies with the read.
                let bytes = Arc::new(bytes);
                if cache_blocks {
                    let _ = self.cache.store_block(uid, mtime, fsize, bidx, &bytes);
                } else {
                    self.stream_ring
                        .lock()
                        .insert(uid, mtime, fsize, bidx, bytes.clone());
                }
                blocks[(bidx - first) as usize] = Some(bytes);
            }
            if !cache_blocks {
                self.stream_readahead(uid, mtime, fsize, last);
            }
        }

        for (i, block) in blocks.into_iter().enumerate() {
            let bidx = first + i as u64;
            let bstart = bidx * BLOCK_SIZE;
            // Every slot is populated: cache hits up front, misses by the fetch above.
            let block = block.expect("block fetched or cached");
            let s = (offset.max(bstart) - bstart) as usize;
            let e = (end.min(bstart + block.len() as u64) - bstart) as usize;
            if s < e {
                out.extend_from_slice(&block[s..e]);
            }
        }
        Ok(out)
    }

    /// Serve a read against an open write handle: stitch authored ranges (from
    /// the scratch file) and untouched ranges (from the remote base via the
    /// block cache) in order, zero-filling any region past the base.
    #[allow(clippy::too_many_arguments)]
    fn serve_open_read(
        &self,
        file: &Arc<File>,
        len: u64,
        uid: &NodeUid,
        base_mtime: i64,
        base_size: u64,
        written: &Intervals,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>, Errno> {
        if offset >= len || size == 0 {
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(size).min(len);
        let mut out = Vec::with_capacity((end - offset) as usize);
        for (s, e, authored) in written.segments(offset, end) {
            if authored {
                let mut buf = vec![0u8; (e - s) as usize];
                file.read_exact_at(&mut buf, s).map_err(|err| {
                    warn!(%uid, error = %err, "scratch read failed");
                    Errno::EIO
                })?;
                out.extend_from_slice(&buf);
            } else {
                let bend = e.min(base_size);
                if s < bend {
                    out.extend_from_slice(&self.read_range(
                        uid,
                        base_mtime,
                        base_size,
                        s,
                        bend - s,
                        true,
                    )?);
                }
                // Anything past the base is a hole: zero-fill.
                let zeros = e.saturating_sub(s.max(base_size));
                out.resize(out.len() + zeros as usize, 0);
            }
        }
        Ok(out)
    }

    /// Fill every unauthored range of a scratch/staged file that overlaps its
    /// base with the base's bytes, so the file becomes the complete new content.
    ///
    /// This is the step a partial overwrite cannot skip: only the authored bytes
    /// were ever written to disk, and a revision upload sends the whole file.
    /// The gaps come from the *remote base* (through the block cache, so a small
    /// edit of a large file does not pull all of it), which is exactly why this
    /// can fail with no network — see `StagedWrite`.
    fn fill_gaps(
        &self,
        uid: &NodeUid,
        file: &File,
        len: u64,
        base_mtime: i64,
        base_size: u64,
        written: &Intervals,
    ) -> Result<(), Errno> {
        file.set_len(len).map_err(|e| {
            error!(%uid, error = %e, "resize scratch file failed");
            Errno::EIO
        })?;
        for (s, e, authored) in written.segments(0, len) {
            if authored {
                continue;
            }
            let bend = e.min(base_size);
            if s >= bend {
                continue; // wholly past the base: already zero-filled on disk
            }
            let bytes = self.read_range_remote(uid, base_mtime, base_size, s, bend - s, true)?;
            file.write_all_at(&bytes, s).map_err(|err| {
                error!(%uid, error = %err, "scratch gap-fill write failed");
                Errno::EIO
            })?;
        }
        Ok(())
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
            Some(p) => p.meta.based_on,
            None => Some(Baseline {
                mtime: base_mtime,
                size: base_size,
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
                next_attempt_at: 0,
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

    /// Bulk-upload local files and directory trees under `sources` into the
    /// mountpoint-relative `parent_rel` folder. Directories are recreated (or
    /// merged into an existing same-named folder) and walked; the resulting flat
    /// set of files is uploaded with bounded concurrency, each ticking the
    /// transfer registry so a front-end sees live progress. Runs on a background
    /// thread — a large tree far outlasts the control socket's read timeout — so
    /// it reports only a summary for the log. Individual failures are counted and
    /// logged rather than aborting the whole batch.
    fn upload_paths(&self, parent_rel: &Path, sources: &[PathBuf]) -> CoreResult<UploadStats> {
        let (pino, parent_uid) = self.resolve(parent_rel)?;
        self.ensure_children(pino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;

        // Phase 1 (sequential): build the remote folder skeleton and collect the
        // flat list of files to upload. Folders must exist before their children,
        // so this can't be parallelised. On a deep tree this is a folder-creation
        // round-trip per directory before a single byte moves — long enough that
        // it needs a job of its own, or the daemon looks idle for minutes.
        let mut tasks = Vec::new();
        let mut folders = 0usize;
        {
            let job = self.transfers.begin_job("Preparing upload");
            for src in sources {
                if let Err(e) =
                    self.collect_uploads(pino, &parent_uid, src, &mut tasks, &mut folders, &job)
                {
                    warn!(source = %src.display(), error = %e, "skipping source");
                }
            }
        }

        // Phase 2 (concurrent): upload the files, up to UPLOAD_CONCURRENCY at once.
        // Each file reports its own bytes; this job is the batch's "N of M files",
        // which is the number a user actually waits on.
        let job = Arc::new(self.transfers.begin_job(match tasks.len() {
            1 => "Uploading 1 file".to_string(),
            n => format!("Uploading {n} files"),
        }));
        job.set_total(tasks.len() as u64);
        let outcomes = self.rt.block_on(run_uploads(
            self.clone(),
            tasks,
            UPLOAD_CONCURRENCY,
            job.clone(),
        ));
        drop(job);

        // Phase 3 (sequential): intern each uploaded node so it shows up in the
        // listing without a re-enumeration. fetch_node uses `block_on`, so it must
        // run here rather than inside the async batch — and it is a round-trip per
        // file, so it too gets a job rather than a silent tail.
        let mut stats = UploadStats {
            folders,
            ..UploadStats::default()
        };
        let job = self.transfers.begin_job("Finishing upload");
        job.set_total(outcomes.len() as u64);
        for outcome in outcomes {
            job.step();
            match outcome {
                Ok((parent_ino, uid, size)) => {
                    stats.uploaded += 1;
                    stats.bytes += size;
                    match self.fetch_node(&uid) {
                        Ok(node) => {
                            let mut st = self.state.lock();
                            let ino = st.intern(parent_ino, node);
                            if let Some(kids) = st.children.get_mut(&parent_ino)
                                && !kids.contains(&ino)
                            {
                                kids.push(ino);
                            }
                        }
                        // Uploaded fine but the metadata refresh failed; it will
                        // appear on the next directory enumeration regardless.
                        Err(e) => warn!(%uid, error = ?e, "uploaded node metadata refresh failed"),
                    }
                }
                Err((name, msg)) => {
                    stats.failed += 1;
                    warn!(name, error = %msg, "file upload failed");
                }
            }
        }
        info!(
            uploaded = stats.uploaded,
            failed = stats.failed,
            "bulk upload finished"
        );
        Ok(stats)
    }

    /// Resolve a remote child folder named `name` under `pino`, creating it if it
    /// doesn't exist, and return its `(inode, uid)`. Reusing an existing same-named
    /// folder makes re-uploading a directory merge into it rather than fail on a
    /// duplicate name.
    fn ensure_remote_folder(
        &self,
        pino: u64,
        parent_uid: &NodeUid,
        name: &str,
    ) -> CoreResult<(u64, NodeUid)> {
        if name.is_empty() || name.contains('/') {
            return Err(CoreError::invalid(format!("invalid folder name: {name:?}")));
        }
        self.ensure_children(pino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;
        {
            let st = self.state.lock();
            if let Some(kids) = st.children.get(&pino) {
                for &ino in kids {
                    if let Some(e) = st.entries.get(&ino)
                        && e.node.is_folder()
                        && e.node.name == name
                    {
                        return Ok((ino, e.uid.clone()));
                    }
                }
            }
        }
        let new_uid = self
            .rt
            .block_on(
                self.client
                    .create_folder(parent_uid, name, Some(now_secs())),
            )
            .map_err(|e| CoreError::from_api(&e, &format!("create folder {name}")))?;
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
        Ok((ino, new_uid))
    }

    /// Walk one local source path, appending its files to `tasks`. A file becomes
    /// one task; a directory is recreated remotely and recursed into (children
    /// sorted for a stable order). Symlinks and other special files are skipped.
    ///
    /// `job` narrates the walk with the folder currently being mirrored. It stays
    /// indeterminate: nothing knows the size of the tree until the walk has ended.
    fn collect_uploads(
        &self,
        pino: u64,
        parent_uid: &NodeUid,
        src: &Path,
        tasks: &mut Vec<UploadTask>,
        folders: &mut usize,
        job: &JobGuard,
    ) -> CoreResult<()> {
        let meta = std::fs::symlink_metadata(src)
            .map_err(|e| CoreError::internal(format!("stat {}: {e}", src.display())))?;
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| CoreError::invalid(format!("unusable name: {}", src.display())))?
            .to_string();

        if meta.is_dir() {
            job.detail(&name);
            let (child_ino, child_uid) = self.ensure_remote_folder(pino, parent_uid, &name)?;
            *folders += 1;
            let mut entries: Vec<PathBuf> = std::fs::read_dir(src)
                .map_err(|e| CoreError::internal(format!("read dir {}: {e}", src.display())))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            entries.sort();
            for entry in entries {
                if let Err(e) =
                    self.collect_uploads(child_ino, &child_uid, &entry, tasks, folders, job)
                {
                    warn!(source = %entry.display(), error = %e, "skipping entry");
                }
            }
            // Deeper folders have retitled the job by now; put this one back so the
            // line tracks the walk's position rather than its deepest leaf.
            job.detail(&name);
        } else if meta.is_file() {
            if name.contains('/') {
                return Err(CoreError::invalid(format!("invalid file name: {name:?}")));
            }
            tasks.push(UploadTask {
                parent_ino: pino,
                parent_uid: parent_uid.clone(),
                name,
                path: src.to_path_buf(),
                size: meta.len(),
            });
        }
        Ok(())
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

/// The Proton Drive VFS. FUSE callbacks are synchronous, so the Tokio handle
/// bridges each one to the async SDK via [`Handle::block_on`]; the fuser
/// session thread is not a runtime worker, so blocking on it is sound.
/// Cloneable so a handler can move a copy onto a [`Workers`] thread and answer
/// from there; every field is a handle or a plain id.
#[derive(Clone)]
pub struct ProtonFs {
    core: Core,
    uid: u32,
    gid: u32,
}

impl ProtonFs {
    /// Build the filesystem rooted at `root` (the user's My Files folder).
    fn new(core: Core, root: Node) -> Self {
        {
            let mut st = core.state.lock();
            if let Err(e) = st.db.upsert_node(&root) {
                warn!(uid = %root.uid, error = %e, "db upsert root failed");
            }
            st.by_uid.insert(root.uid.clone(), ROOT_INO);
            st.entries.insert(
                ROOT_INO,
                Entry {
                    uid: root.uid.clone(),
                    parent: ROOT_INO,
                    node: root,
                },
            );
        }
        // SAFETY: geteuid/getegid are infallible and have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self { core, uid, gid }
    }

    /// The body of [`Filesystem::lookup`], on whichever thread ends up serving it.
    fn serve_lookup(&self, parent: u64, name: &str, reply: ReplyEntry) {
        if let Err(e) = self.core.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let st = self.core.state.lock();
        let hit = st.children.get(&parent).and_then(|kids| {
            kids.iter().copied().find_map(|ino| {
                st.entries
                    .get(&ino)
                    .filter(|e| e.node.name == name)
                    .map(|e| (ino, &e.node))
            })
        });
        match hit {
            Some((ino, node)) => {
                let attr = self.attr(ino, node);
                reply.entry(&TTL, &attr, Generation(0));
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    /// The body of [`Filesystem::readdir`], on whichever thread ends up serving it.
    fn serve_readdir(&self, ino: u64, offset: u64, mut reply: ReplyDirectory) {
        if let Err(e) = self.core.ensure_children(ino) {
            reply.error(e);
            return;
        }
        let st = self.core.state.lock();
        let parent = st.entries.get(&ino).map_or(ROOT_INO, |e| e.parent);

        // "." and ".." occupy offsets 0 and 1; real children follow.
        let mut listing: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent, FileType::Directory, "..".to_string()),
        ];
        if let Some(kids) = st.children.get(&ino) {
            for &kid in kids {
                if let Some(e) = st.entries.get(&kid) {
                    let ft = if e.node.is_folder() {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    listing.push((kid, ft, e.node.name.clone()));
                }
            }
        }
        drop(st);

        for (i, (ino, ft, name)) in listing.into_iter().enumerate().skip(offset as usize) {
            // The stored offset is that of the *next* entry to resume at.
            if reply.add(INodeNo(ino), (i + 1) as u64, ft, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn attr(&self, ino: u64, node: &Node) -> FileAttr {
        let (kind, perm) = match node.kind {
            NodeKind::Folder => (FileType::Directory, 0o755),
            NodeKind::File { .. } => (FileType::RegularFile, 0o644),
        };
        let size = node_size(node);
        let mtime = unix_secs(node.modification_time);
        let crtime = unix_secs(node.creation_time);
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Trash the child `name` under `parent` on the remote, then drop it from the
    /// local cache. Backs both `unlink` and `rmdir` (Proton trashes whole
    /// subtrees, so an `rmdir` of a non-empty dir behaves the same).
    fn trash_child(&self, parent: u64, name: &str, reply: ReplyEmpty) {
        let (_ino, uid) = match self.core.lookup_child(parent, name) {
            Ok(x) => x,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        // A node the server has never heard of cannot be trashed there; deleting
        // it just means its queued creation is no longer wanted. This works
        // offline, which the remote path below cannot (offline.md Phase 3b).
        if is_local_uid(&uid) {
            self.core.discard_queued_ops(&uid);
            self.core.state.lock().forget(&uid);
            debug!(%uid, name, "deleted a node that had not been created remotely yet");
            reply.ok();
            return;
        }
        // Offline: queue it. Trashing is the one mutation a user expects to work
        // regardless — the file is gone from their point of view the moment the
        // command returns (offline.md Phase 3b).
        if !self.core.online.load(Ordering::Relaxed) {
            match self.core.queue_trash(&uid, name) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
            return;
        }
        if let Err(e) = self
            .core
            .rt
            .block_on(self.core.client.trash_nodes(std::slice::from_ref(&uid)))
        {
            error!(%uid, error = %e, "trash failed");
            self.core
                .log_activity(ActivityKind::Trash, name, e.to_string(), false);
            reply.error(Errno::EIO);
            return;
        }
        self.core.discard_queued_ops(&uid);
        self.core.state.lock().forget(&uid);
        self.core.cache.evict(&uid);
        self.core.evict_reader(&uid);
        self.core.invalidate_trash();
        // Every other trash site records itself; this one did not, which made a
        // file found in the trash impossible to attribute after the fact — the
        // activity log was the only record and it showed nothing (bugs.md B2).
        self.core
            .log_activity(ActivityKind::Trash, name, "trashed from the mount", true);
        reply.ok();
    }
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

/// The aspect ratio (w/h) of an encoded image, read from its header alone — no
/// pixels are decoded. `None` when the format is unknown or the header is torn.
fn ratio_of(bytes: &[u8]) -> Option<f64> {
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    (height > 0).then(|| f64::from(width) / f64::from(height))
}

/// A thumbnail the daemon made itself, and the aspect ratio of the photo it was
/// made from (free at that point — the full image had to be decoded anyway).
struct GeneratedThumb {
    bytes: Vec<u8>,
    ratio: f64,
}

/// How one attempt at generating a missing thumbnail ended. The distinction that
/// matters is *permanent* versus *transient*: only bytes we cannot decode prove
/// the photo will never have a thumbnail, and only that verdict is persisted.
enum ThumbAttempt {
    Made(GeneratedThumb),
    /// Decoded nothing — a format this build has no decoder for. Permanent.
    Undecodable,
    /// The photo couldn't be downloaded. Transient: try again next time.
    Unavailable,
}

/// Scale a full-size photo down to a thumbnail: at most [`THUMB_EDGE`] on its
/// longest side, JPEG, aspect ratio preserved. `None` when the bytes aren't an
/// image this build can decode — the caller then writes the photo off as
/// un-thumbnailable.
///
/// CPU-bound (a 20 MP JPEG is real work), so callers run it on the blocking pool.
fn scale_thumbnail(bytes: &[u8]) -> Option<GeneratedThumb> {
    let image = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return None;
    }
    let ratio = f64::from(width) / f64::from(height);

    // `thumbnail` fits the image *inside* the box, so the longest edge lands on
    // THUMB_EDGE and the ratio is untouched.
    let thumb = image.thumbnail(THUMB_EDGE, THUMB_EDGE).to_rgb8();
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, THUMB_QUALITY)
        .encode_image(&thumb)
        .ok()?;
    Some(GeneratedThumb { bytes, ratio })
}

/// Convert a Unix timestamp (seconds, possibly negative) to a `SystemTime`.
fn unix_secs(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

impl Filesystem for ProtonFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = parent.0;
        let name = name.to_string_lossy().into_owned();
        // A folder that has not been listed yet is enumerated from the remote,
        // so serve it from a worker rather than stalling the dispatch loop. A
        // listed folder — the common case — is a map hit, and answering it
        // inline costs less than the handoff would.
        if self.core.children_cached(parent) {
            self.serve_lookup(parent, &name, reply);
            return;
        }
        let fs = self.clone();
        self.core
            .workers
            .run(Lane::Meta, move || fs.serve_lookup(parent, &name, reply));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let st = self.core.state.lock();
        match st.entries.get(&ino.0) {
            Some(e) => {
                let attr = self.attr(ino.0, &e.node);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        let ino = ino.0;
        // Same split as `lookup`: only the cold, remote-enumerating path pays
        // for a worker.
        if self.core.children_cached(ino) {
            self.serve_readdir(ino, offset, reply);
            return;
        }
        let fs = self.clone();
        self.core
            .workers
            .run(Lane::Meta, move || fs.serve_readdir(ino, offset, reply));
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let (uid, base_mtime, base_size) = {
            let st = self.core.state.lock();
            match st.entries.get(&ino.0) {
                Some(e) if e.node.is_file() => {
                    (e.uid.clone(), e.node.modification_time, node_size(&e.node))
                }
                Some(_) => {
                    reply.error(Errno::EISDIR);
                    return;
                }
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        debug!(ino = ino.0, ?flags, base_size, base_mtime, "open");
        // Read-only opens stay stateless (fh 0). A write open allocates a
        // disk-backed handle; bytes are authored into a scratch file and the
        // untouched remainder is pulled from the base lazily (on read / commit).
        if flags.acc_mode() == OpenAccMode::O_RDONLY {
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }
        let (file, path) = match self.core.cache.create_scratch() {
            Ok(x) => x,
            Err(e) => {
                error!(%uid, error = %e, "create scratch file failed");
                reply.error(Errno::EIO);
                return;
            }
        };
        let mut st = self.core.state.lock();
        let fh = st.next_fh;
        st.next_fh += 1;
        st.handles.insert(
            fh,
            WriteHandle {
                ino: ino.0,
                uid,
                file: Arc::new(file),
                path,
                written: Intervals::default(),
                // Starts at the current size; reads in [0, base_size) come from
                // the base until overwritten.
                len: base_size,
                base_size,
                base_mtime,
                dirty: false,
            },
        );
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        // A file open for writing is served from its handle so reads see the
        // in-flight (possibly unsaved) content: authored bytes come from the
        // scratch file, untouched bytes from the base.
        //
        // This path stays on the dispatch loop. `write` runs there too, so
        // keeping its reads there as well preserves the ordering the write path
        // has always had between a write and a read of the same handle; a mostly
        // local read is not worth reasoning about that for.
        let handle = {
            let st = self.core.state.lock();
            st.handles.values().find(|h| h.ino == ino.0).map(|h| {
                (
                    h.file.clone(),
                    h.len,
                    h.uid.clone(),
                    h.base_mtime,
                    h.base_size,
                    h.written.clone(),
                )
            })
        };
        if let Some((file, len, uid, base_mtime, base_size, written)) = handle {
            match self.core.serve_open_read(
                &file,
                len,
                &uid,
                base_mtime,
                base_size,
                &written,
                offset,
                size as u64,
            ) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => reply.error(e),
            }
            return;
        }
        let (uid, mtime, fsize, is_video) = {
            let st = self.core.state.lock();
            match st.entries.get(&ino.0) {
                Some(e) if e.node.is_file() => {
                    let media_type = match &e.node.kind {
                        NodeKind::File { media_type, .. } => Some(media_type.as_str()),
                        NodeKind::Folder => None,
                    };
                    let is_video =
                        PhotoKind::classify(Some(&e.node.name), media_type) == PhotoKind::Video;
                    (
                        e.uid.clone(),
                        e.node.modification_time,
                        node_size(&e.node),
                        is_video,
                    )
                }
                Some(_) => {
                    reply.error(Errno::EISDIR);
                    return;
                }
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // A large unpinned video streams without polluting the block cache (see
        // [`STREAM_BYPASS_MIN`]); everything else caches its blocks as usual.
        let cache_blocks =
            !(is_video && fsize >= STREAM_BYPASS_MIN && !self.core.cache.is_pinned(&uid));
        // Off the dispatch loop: this is the one handler that routinely goes to
        // the network (block fetch + decrypt, hundreds of ms on a cold file),
        // and until it returns the kernel's next request for this mount is not
        // even read off the FUSE device. It only reads `state`, so moving it
        // races with nothing. FUSE does not require replies in request order.
        let core = self.core.clone();
        self.core.workers.run(Lane::Transfer, move || {
            match core.read_range(&uid, mtime, fsize, offset, size as u64, cache_blocks) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => reply.error(e),
            }
        });
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let parent = parent.0;
        if let Err(e) = self.core.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let name = name.to_string_lossy().into_owned();
        let parent_uid = {
            let st = self.core.state.lock();
            match st.entries.get(&parent) {
                Some(e) => e.uid.clone(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // Offline the server cannot mint a uid, so invent one and queue the
        // create. The file is real to the caller either way; only its identity is
        // provisional until the drain (offline.md Phase 3b).
        //
        // A parent that is itself still queued forces the same path even when we
        // are online: the API has no folder to put this in yet.
        let node = if self.core.online.load(Ordering::Relaxed) && !is_local_uid(&parent_uid) {
            // Create an empty file on the remote so it has a real uid immediately;
            // written bytes are buffered and sealed as a new revision on close.
            let new_uid = match self.core.rt.block_on(self.core.client.upload_file(
                &parent_uid,
                &name,
                media_type_for(&name),
                b"",
            )) {
                Ok(u) => u,
                Err(e) => {
                    error!(%parent_uid, name, error = %e, "create file failed");
                    reply.error(Errno::EIO);
                    return;
                }
            };
            match self.core.fetch_node(&new_uid) {
                Ok(n) => n,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        } else {
            match self.core.queue_local_node(&parent_uid, &name, false) {
                Ok(n) => n,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        };
        let new_uid = node.uid.clone();
        // The base this handle writes over is the node as it actually exists —
        // the empty file the server just minted — so its modification time comes
        // from the node, never from the local clock. `queue_revision` turns this
        // into `StagedWrite::based_on` and the drain compares that against the
        // remote: a `now_secs()` here differs from the server's stamp by however
        // long the create round-trip took, so the first write to a brand-new file
        // conflicts with its own create whenever the second happens to tick in
        // between.
        let base_mtime = node.modification_time;
        let (file, path) = match self.core.cache.create_scratch() {
            Ok(x) => x,
            Err(e) => {
                error!(%new_uid, error = %e, "create scratch file failed");
                reply.error(Errno::EIO);
                return;
            }
        };
        let mut st = self.core.state.lock();
        let ino = st.intern(parent, node);
        if let Some(kids) = st.children.get_mut(&parent)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        let fh = st.next_fh;
        st.next_fh += 1;
        st.handles.insert(
            fh,
            // A brand-new file: empty base, everything written is authored.
            WriteHandle {
                ino,
                uid: new_uid,
                file: Arc::new(file),
                path,
                written: Intervals::default(),
                len: 0,
                base_size: 0,
                base_mtime,
                dirty: false,
            },
        );
        let attr = self.attr(ino, &st.entries.get(&ino).unwrap().node);
        reply.created(
            &TTL,
            &attr,
            Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let fh = fh.0;
        // Stage the bytes straight into the scratch file (no base download): only
        // the untouched remainder is pulled from the remote, and only at commit.
        let file = {
            let st = self.core.state.lock();
            match st.handles.get(&fh) {
                Some(h) => h.file.clone(),
                None => {
                    reply.error(Errno::EBADF);
                    return;
                }
            }
        };
        if let Err(e) = file.write_all_at(data, offset) {
            error!(ino = ino.0, fh, error = %e, "scratch write failed");
            reply.error(Errno::EIO);
            return;
        }
        let new_len = {
            let mut st = self.core.state.lock();
            let Some(h) = st.handles.get_mut(&fh) else {
                reply.error(Errno::EBADF);
                return;
            };
            let end = offset + data.len() as u64;
            h.written.add(offset, end);
            h.len = h.len.max(end);
            h.dirty = true;
            let len = h.len;
            st.set_size(ino.0, len);
            len
        };
        debug!(
            ino = ino.0,
            fh,
            offset,
            len = data.len(),
            new_len,
            "staged write"
        );
        reply.written(data.len() as u32);
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Only resizes change remote state; everything else (mode/owner/times) is
        // accepted and ignored so tools that chmod/utimes after writing succeed.
        if let Some(size) = size {
            let fh = fh.map(|f| f.0).filter(|&f| f != 0);
            let handled = {
                let mut st = self.core.state.lock();
                // The kernel does not put `O_TRUNC` in the `open` flags unless
                // the mount enables `atomic_o_trunc`; it opens, then truncates
                // with a *separate* `setattr` carrying no file handle. So an fh
                // is a hint, not a precondition — fall back to the write handle
                // for this inode, the same lookup `read` does and for the same
                // reason. Without it a `cp` over an existing file becomes two
                // independent queued ops: a truncate, plus a release that still
                // believes the file is its old length. They then conflict with
                // each other and the write is diverted into a conflict copy.
                let target = match fh {
                    Some(fh) if st.handles.contains_key(&fh) => Some(fh),
                    _ => st
                        .handles
                        .iter()
                        .find(|(_, h)| h.ino == ino.0)
                        .map(|(&fh, _)| fh),
                };
                match target.and_then(|fh| st.handles.get_mut(&fh)) {
                    Some(h) => {
                        if size < h.len {
                            // Shrink: drop authored ranges past the new end.
                            h.written.clip(size);
                        } else if size > h.len {
                            // Grow: the new tail is defined as zeros, so claim
                            // it as authored rather than base content.
                            h.written.add(h.len, size);
                        }
                        let _ = h.file.set_len(size);
                        h.len = size;
                        h.dirty = true;
                        true
                    }
                    None => false,
                }
            };
            debug!(ino = ino.0, size, fh = ?fh, handled, "setattr resize");
            if !handled {
                // Path-based truncate with no open write handle — a shell's
                // `> file`. This is the second write path into the API, and
                // queueing it is what lets a redirect work offline at all: it
                // never reaches `release`, so without this it failed before any
                // byte was written (offline.md Phase 2/3b).
                if let Err(e) = self.core.queue_truncate(ino.0, size) {
                    reply.error(e);
                    return;
                }
            } else {
                self.core.state.lock().set_size(ino.0, size);
            }
        }
        let st = self.core.state.lock();
        match st.entries.get(&ino.0) {
            Some(e) => {
                let attr = self.attr(ino.0, &e.node);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    /// `close(2)` calls flush before release. The upload is queued in `release`
    /// and performed in the background (offline.md Phase 3), so there is nothing
    /// to push here — the written bytes are already in the scratch file.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// Durability here means the scratch file, not the remote: a caller asking
    /// for fsync wants its bytes to survive a crash, and blocking it on an upload
    /// (which offline would never finish) buys nothing the queue does not already
    /// guarantee.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // Everything the durability sidecar needs, copied out so no lock is held
        // across the sync.
        let handle = self.core.state.lock().handles.get(&fh.0).map(|h| {
            (
                h.file.clone(),
                h.path.clone(),
                h.uid.clone(),
                h.dirty,
                h.written.clone(),
                h.len,
                h.base_size,
                h.base_mtime,
            )
        });
        let Some((f, path, uid, dirty, written, len, base_size, base_mtime)) = handle else {
            reply.error(Errno::EBADF);
            return;
        };
        if let Err(e) = f.sync_all() {
            error!(fh = fh.0, error = %e, "fsync of scratch file failed");
            reply.error(Errno::EIO);
            return;
        }
        // A clean handle has nothing on it that the remote does not already
        // hold, so there is nothing for a crash to lose.
        if !dirty {
            reply.ok();
            return;
        }
        // The bytes are on stable storage, but only the scratch file knows that,
        // and the scratch directory is cleared at open. Record what the blob is
        // so a restart can hand it to the upload queue instead of deleting it.
        //
        // Deliberately *not* the full release path: staging the blob here would
        // move the file out from under a handle the application still has open,
        // and queueing an upload per `fsync` would push a revision for every
        // barrier in a write loop. This only makes the existing file findable.
        let authored: Vec<(u64, u64)> = written
            .segments(0, len)
            .into_iter()
            .filter(|&(_, _, authored)| authored)
            .map(|(s, e, _)| (s, e))
            .collect();
        let meta = StagedWrite {
            uid: uid.to_string(),
            len,
            base_size,
            base_mtime,
            complete: authored == [(0, len)],
            authored,
            based_on: self.core.remote_baseline(&uid, base_mtime, base_size),
        };
        // A failed sidecar means the write is not durable, and `fsync` promising
        // otherwise is the defect this exists to fix — so it is an error, not a
        // warning.
        if let Err(e) = self.core.cache.mark_scratch_durable(&path, &meta) {
            error!(%uid, error = %e, "recording fsync durability failed");
            reply.error(Errno::EIO);
            return;
        }
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self.core.state.lock().handles.remove(&fh.0);
        // Hand the bytes to the queue rather than uploading them here: the
        // scratch file is the only copy of what was just written, and blocking
        // the caller on the network is what made a copy into the mount run at
        // upload speed (and fail outright offline).
        match handle {
            Some(h) => match self.core.queue_revision(&h) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            },
            None => reply.ok(),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.trash_child(parent.0, &name.to_string_lossy(), reply);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.trash_child(parent.0, &name.to_string_lossy(), reply);
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent = parent.0;
        if let Err(e) = self.core.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let name = name.to_string_lossy().into_owned();
        let parent_uid = {
            let st = self.core.state.lock();
            match st.entries.get(&parent) {
                Some(e) => e.uid.clone(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .ok();
        // As in `create`: offline — or under a parent that is itself still
        // queued — the folder becomes a placeholder that the drain turns into a
        // real one (offline.md Phase 3b).
        let node = if self.core.online.load(Ordering::Relaxed) && !is_local_uid(&parent_uid) {
            let new_uid =
                match self
                    .core
                    .rt
                    .block_on(self.core.client.create_folder(&parent_uid, &name, now))
                {
                    Ok(u) => u,
                    Err(e) => {
                        error!(%parent_uid, name, error = %e, "create folder failed");
                        reply.error(Errno::EIO);
                        return;
                    }
                };
            match self.core.fetch_node(&new_uid) {
                Ok(n) => n,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        } else {
            match self.core.queue_local_node(&parent_uid, &name, true) {
                Ok(n) => n,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        };
        let mut st = self.core.state.lock();
        let local = is_local_uid(&node.uid);
        let uid = node.uid.clone();
        let ino = st.intern(parent, node);
        if let Some(kids) = st.children.get_mut(&parent)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        // A folder that exists only here has nothing to enumerate, and asking the
        // API to enumerate a `local~` uid would 404. Record it as fully listed and
        // empty, which it is, so reads stay offline-clean across a restart too.
        if local {
            st.children.insert(ino, Vec::new());
            if let Err(e) = self.core.db.set_listed(&uid, true) {
                warn!(%uid, error = %e, "db set_listed(true) failed for local folder");
            }
        }
        let attr = self.attr(ino, &st.entries.get(&ino).unwrap().node);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent = parent.0;
        let newparent = newparent.0;
        let name = name.to_string_lossy().into_owned();
        let newname = newname.to_string_lossy().into_owned();
        let (ino, uid) = match self.core.lookup_child(parent, &name) {
            Ok(x) => x,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        // The destination has to be listed either way: the queued path pushes
        // the node into that listing, and the online one drops it to force a
        // re-enumeration.
        if newparent != parent
            && let Err(e) = self.core.ensure_children(newparent)
        {
            reply.error(e);
            return;
        }
        let new_parent_uid = {
            let st = self.core.state.lock();
            match st.entries.get(&newparent) {
                Some(e) => e.uid.clone(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // A node whose own creation is still queued has no server-side identity
        // to rename: the queued op *is* the node, so rewriting its target is the
        // whole rename. Nothing reaches the API, which is why this works offline
        // (offline.md Phase 3b).
        if is_local_uid(&uid) {
            match self.core.db.rewrite_op_target(
                &uid.to_string(),
                &new_parent_uid.to_string(),
                &newname,
            ) {
                Ok(true) => {
                    self.core.state.lock().rename_in_place(
                        ino,
                        newparent,
                        &new_parent_uid,
                        &newname,
                    );
                    debug!(%uid, newname, "renamed a node whose create is still queued");
                    reply.ok();
                }
                // The create drained underneath us, so the node has a real uid
                // now and this handle's is stale. A retry resolves it.
                Ok(false) => {
                    warn!(%uid, name, newname, "queued create vanished under a rename");
                    reply.error(Errno::EBUSY);
                }
                Err(e) => {
                    error!(%uid, error = %e, "rewriting a queued create's target failed");
                    reply.error(Errno::EIO);
                }
            }
            return;
        }
        // Offline, or into a folder that does not exist remotely yet: queue the
        // rename rather than 404 or fail. Online moves into a real parent still
        // take the synchronous path, so a genuine API refusal (permissions, a
        // name clash) surfaces to the caller instead of becoming a queued op
        // that can only ever conflict.
        if !self.core.online.load(Ordering::Relaxed) || is_local_uid(&new_parent_uid) {
            match self
                .core
                .queue_rename(ino, &uid, newparent, &new_parent_uid, &newname)
            {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
            return;
        }
        // Move first if the parent changed, then rename if the name changed.
        if newparent != parent
            && let Err(e) = self
                .core
                .rt
                .block_on(self.core.client.move_node(&uid, &new_parent_uid))
        {
            error!(%uid, error = %e, "move failed");
            reply.error(Errno::EIO);
            return;
        }
        if newname != name
            && let Err(e) = self
                .core
                .rt
                .block_on(self.core.client.rename_node(&uid, &newname, None))
        {
            error!(%uid, error = %e, "rename failed");
            reply.error(Errno::EIO);
            return;
        }
        self.core
            .state
            .lock()
            .relocate(ino, parent, newparent, &new_parent_uid, &newname);
        reply.ok();
    }

    /// Expose a file's server-side thumbnail/preview as an extended attribute, so
    /// a previewing client can fetch it without downloading the whole file. The
    /// bytes are fetched on demand and cached; absence yields `ENODATA`.
    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        // Always off the dispatch loop: a miss goes to the wire, and a lister
        // that stats a directory issues one of these per file per advertised
        // name — inline, that serialized the whole mount behind ~186 ms of
        // network per call (B5).
        let fs = self.clone();
        let name = name.to_os_string();
        self.core.workers.run(Lane::Meta, move || {
            fs.serve_getxattr(ino, &name, size, reply)
        });
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.serve_listxattr(ino, size, reply);
    }
}

impl ProtonFs {
    /// The body of [`Filesystem::getxattr`], on a worker thread.
    fn serve_getxattr(&self, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let ttype = match name.to_str() {
            Some(XATTR_THUMBNAIL) => ThumbnailType::Thumbnail,
            Some(XATTR_PREVIEW) => ThumbnailType::Preview,
            // Any other attribute simply does not exist on this filesystem.
            _ => {
                reply.error(Errno::ENODATA);
                return;
            }
        };
        let bytes = match self.core.thumbnail(ino.0, ttype) {
            Ok(Some(b)) => b,
            Ok(None) => {
                reply.error(Errno::ENODATA);
                return;
            }
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let len = bytes.len() as u32;
        // The xattr protocol: a zero `size` is a probe for the length; otherwise
        // the caller's buffer must be large enough or it gets `ERANGE`.
        if size == 0 {
            reply.size(len);
        } else if size < len {
            reply.error(Errno::ERANGE);
        } else {
            reply.data(&bytes);
        }
    }

    /// Advertise the thumbnail/preview attribute names, but only for files whose
    /// media type can actually carry one.
    ///
    /// Listing them for every file is what made `ls -l` expensive: an xattr-aware
    /// lister asks for each advertised name, and each ask that misses is a network
    /// round trip (B5). Proton only ever generates thumbnails for images and
    /// videos, so advertising them on a `.mkv` or a `.pdf` is an invitation to do
    /// work that can only end in `ENODATA`. `getxattr` still serves an explicit
    /// request for an unadvertised name, so nothing becomes unreachable.
    fn serve_listxattr(&self, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let thumbnailable = {
            let st = self.core.state.lock();
            match st.entries.get(&ino.0) {
                Some(e) => match &e.node.kind {
                    NodeKind::File { media_type, .. } => {
                        media_type.starts_with("image/") || media_type.starts_with("video/")
                    }
                    NodeKind::Folder => false,
                },
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // xattr names are returned as a NUL-terminated, concatenated list.
        let mut buf = Vec::new();
        if thumbnailable {
            for name in [XATTR_THUMBNAIL, XATTR_PREVIEW] {
                buf.extend_from_slice(name.as_bytes());
                buf.push(0);
            }
        }
        let len = buf.len() as u32;
        if size == 0 {
            reply.size(len);
        } else if size < len {
            reply.error(Errno::ERANGE);
        } else {
            reply.data(&buf);
        }
    }
}

/// Apply one remote event to the local cache and notify the kernel so it drops
/// any stale cached metadata/data for the affected inodes.
///
/// The cache is authoritative-by-absence: dropping a directory's `children`
/// entry forces the next `lookup`/`readdir` to re-enumerate from the remote, so
/// most events only need to invalidate listings rather than re-fetch eagerly.
fn apply_event(
    state: &Mutex<State>,
    content: &ContentCache,
    pending: &Mutex<HashMap<NodeUid, PendingRevision>>,
    notifier: &Notifier,
    event: &DriveEvent,
) {
    match event {
        DriveEvent::NodeUpdated {
            node_uid,
            parent_node_uid,
            is_trashed,
            ..
        } => {
            let mut st = state.lock();
            if *is_trashed {
                // Trashing makes a node vanish from its parent listing.
                let child = st.by_uid.get(node_uid).copied();
                if let Some((parent, name)) = st.forget(node_uid) {
                    content.evict(node_uid);
                    match child {
                        Some(child) => {
                            let _ =
                                notifier.delete(INodeNo(parent), INodeNo(child), OsStr::new(&name));
                        }
                        None => {
                            let _ = notifier.inval_entry(INodeNo(parent), OsStr::new(&name));
                        }
                    }
                }
            } else if pending.lock().contains_key(node_uid) {
                // A node we owe an upload for is *ahead* of the remote, not
                // behind it: this event is almost always the echo of our own
                // empty-file create, and re-fetching would replace the size and
                // mtime of the write we just accepted with the stale revision's
                // — making a file that was copied in seconds ago read as empty
                // until its upload lands (offline.md Phase 3).
                debug!(uid = %node_uid, "ignoring remote event for a node with a queued write");
            } else if let Some(&ino) = st.by_uid.get(node_uid) {
                // Known node changed: drop its cached attrs/data (and listing if
                // it is a directory) so the next access re-fetches. Its content
                // blob may now be stale, so evict it too.
                st.invalidate_listing(ino);
                content.evict(node_uid);
                let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
            }
            // A create (or move-in) shows up as a change to the parent listing;
            // drop it so the new child is picked up on the next readdir.
            if let Some(parent_uid) = parent_node_uid
                && let Some(&parent) = st.by_uid.get(parent_uid)
            {
                st.invalidate_listing(parent);
                let _ = notifier.inval_inode(INodeNo(parent), 0, 0);
            }
        }
        DriveEvent::NodeDeleted { node_uid, .. } => {
            let mut st = state.lock();
            // Capture the inode before `forget` clears the uid mapping.
            let child = st.by_uid.get(node_uid).copied();
            content.evict(node_uid);
            if let Some((parent, name)) = st.forget(node_uid) {
                match child {
                    Some(child) => {
                        let _ = notifier.delete(INodeNo(parent), INodeNo(child), OsStr::new(&name));
                    }
                    None => {
                        let _ = notifier.inval_entry(INodeNo(parent), OsStr::new(&name));
                    }
                }
            }
        }
        // Continuity or scope was lost: our cached listings may be arbitrarily
        // stale, so drop every listing and tell the kernel to forget all
        // metadata. Inodes stay stable; dirs simply re-enumerate on next access.
        DriveEvent::ContinuityLost { .. } | DriveEvent::ScopeAccessLost { .. } => {
            warn!("event continuity lost; dropping all cached listings, resyncing lazily");
            let mut st = state.lock();
            let dirs: Vec<u64> = st.children.keys().copied().collect();
            for &ino in &dirs {
                st.invalidate_listing(ino);
                let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
            }
        }
        // No substantive local change; the cursor advance is handled by the
        // caller persisting the event id.
        DriveEvent::CursorAdvanced { .. } | DriveEvent::SharedWithMeUpdated { .. } => {}
    }
}

/// Poll the remote event cursor forever, applying each batch to the shared
/// state. Resumes from the cursor persisted in the DB so changes made while
/// unmounted are applied; only a first-ever mount seeds from the server head.
/// The cursor is persisted after every batch. Runs as a Tokio task; returns
/// only on fatal error.
async fn run_event_sync(
    client: ProtonDriveClient,
    scope: DriveEventScopeId,
    state: Arc<Mutex<State>>,
    content: Arc<ContentCache>,
    db: Arc<Db>,
    pending: Arc<Mutex<HashMap<NodeUid, PendingRevision>>>,
    notifier: Notifier,
) {
    let mut cursor: Option<DriveEventId> = match db.get_event_cursor() {
        // Resume: pick up exactly where the last run left off.
        Ok(Some(saved)) => Some(DriveEventId::from(saved)),
        // First mount: a `None` cursor yields a single `CursorAdvanced` at the
        // server head; persist it so the next restart resumes instead of
        // reseeding (which would skip everything that changed offline).
        // Seeding needs the network, and this task also runs on mounts that
        // started offline (offline.md Phase 1) — so retry rather than giving up,
        // which used to disable live sync for the life of the daemon.
        Ok(None) => {
            let mut delay = ONLINE_PROBE_MIN;
            loop {
                match client.enumerate_events(&scope, None).await {
                    Ok(events) => {
                        let head = events.last().map(|e| e.id().clone());
                        if let Some(c) = &head
                            && let Err(e) = db.set_event_cursor(c.as_str())
                        {
                            warn!(error = %e, "persist seed cursor failed");
                        }
                        break head;
                    }
                    Err(e) => {
                        warn!(error = %e, ?delay, "seed event cursor failed; retrying");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(ONLINE_PROBE_MAX);
                    }
                }
            }
        }
        Err(e) => {
            error!(error = %e, "read persisted cursor failed; live sync disabled");
            return;
        }
    };
    info!(?cursor, "event sync started");

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let events = match client.enumerate_events(&scope, cursor.as_ref()).await {
            Ok(events) => events,
            Err(e) => {
                warn!(error = %e, "event poll failed; retrying after interval");
                continue;
            }
        };
        if events.is_empty() {
            continue;
        }
        debug!(count = events.len(), "applying remote events");
        for event in &events {
            // Converge the SDK's own caches (folder keys, entity cache) on the
            // server before applying the event to our tree. Without this, a node
            // re-keyed/moved by another client keeps a stale key in the SDK for
            // the life of the daemon (SDK plan #9). `apply_event` only touches
            // our FUSE state, so nothing else does this.
            if let Err(e) = client.invalidate_caches_for_event(event).await {
                warn!(error = %e, "sdk cache invalidation for event failed");
            }
            apply_event(&state, &content, &pending, &notifier, event);
        }
        cursor = events.last().map(|e| e.id().clone());
        if let Some(c) = &cursor
            && let Err(e) = db.set_event_cursor(c.as_str())
        {
            warn!(error = %e, "persist event cursor failed");
        }
    }
}

/// Keep the local-file index fresh for the launcher prompt's "This computer"
/// results. Rebuilds the index whenever it is older than [`LOCAL_INDEX_TTL`],
/// then sleeps; runs on its own thread for the life of the daemon.
///
/// The walk is the one part of the daemon that touches the wider filesystem, so
/// it is deliberately kept off every hot path: it never runs on a FUSE or
/// control-socket thread, and it excludes the mountpoint (walking it would fault
/// every remote node in through FUSE, defeating on-demand hydration).
fn run_local_index(
    db: Arc<Db>,
    indexing: Arc<AtomicBool>,
    transfers: Arc<TransferRegistry>,
    mountpoint: PathBuf,
) {
    loop {
        let age = db.local_indexed_at().ok().flatten();
        let stale =
            age.is_none_or(|at| now_secs().saturating_sub(at) >= LOCAL_INDEX_TTL.as_secs() as i64);
        if stale {
            scan_local_once(&db, &indexing, &transfers, &mountpoint);
        }
        std::thread::sleep(LOCAL_INDEX_CHECK);
    }
}

/// Walk `$HOME` once and replace the local-file index with what it finds.
/// Batches stream straight into SQLite, so peak memory is one batch — not the
/// whole home directory.
///
/// Reports itself as a job: the first scan after a fresh install walks the whole
/// home directory, and `indexing` alone only tells the launcher prompt to say
/// "still indexing" — nothing else showed that the daemon was busy.
fn scan_local_once(
    db: &Db,
    indexing: &AtomicBool,
    transfers: &Arc<TransferRegistry>,
    mountpoint: &Path,
) {
    let dirs = match AppDirs::new() {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "local index: cannot resolve app dirs");
            return;
        }
    };
    let Some(home) = dirs.home_dir() else {
        warn!("local index: cannot resolve home directory");
        return;
    };
    let generation = match db.local_begin_scan() {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "local index: cannot open scan generation");
            return;
        }
    };

    let excludes = localindex::default_excludes(mountpoint, &dirs.state_dir(), &dirs.cache_dir());
    indexing.store(true, Ordering::Relaxed);
    let started = Instant::now();

    // The walk has no idea how many files it will find, so the job counts what it
    // has seen and stays indeterminate.
    let job = transfers.begin_job("Indexing this computer");
    job.detail("Scanning your files");
    let walked = localindex::scan(&[home], &excludes, |batch| {
        if let Err(e) = db.local_upsert_batch(generation, &batch) {
            warn!(error = %e, "local index: batch write failed");
        }
    });

    // Prune what this scan did not see and rebuild the FTS index over the rest,
    // even if some batches failed — a partial index still beats none.
    job.detail("Building the search index");
    match db.local_finish_scan(generation, now_secs()) {
        Ok(indexed) => info!(
            walked,
            indexed,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "local index rebuilt"
        ),
        Err(e) => warn!(error = %e, "local index: finish failed"),
    }
    indexing.store(false, Ordering::Relaxed);
}

/// Why a [`mount`] call returned. Lets the daemon decide whether to exit (clean
/// shutdown) or remount (the mount went away under it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountOutcome {
    /// The daemon was asked to stop (SIGTERM/SIGINT) and we lazily unmounted
    /// ourselves. The caller should exit cleanly.
    Shutdown,
    /// The kernel mount ended on its own (e.g. an external `fusermount -u`).
    /// The caller may want to remount.
    Unmounted,
}

/// Best-effort teardown of a stale mount left behind by a crashed daemon. A
/// previous run that died without unmounting leaves the kernel mount in place,
/// so the fresh `Session::new` below would fail with EBUSY ("Device or resource
/// busy"). `fusermount3 -u -z` is the lazy (detach) unmount, which succeeds even
/// when the old mount is still busy. Swallow all output/errors: if there is no
/// stale mount this is simply a no-op.
fn clear_stale_mount(mountpoint: &Path) {
    for bin in ["fusermount3", "fusermount"] {
        let ok = std::process::Command::new(bin)
            .arg("-u")
            .arg("-z")
            .arg(mountpoint)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            info!(mountpoint = %mountpoint.display(), "cleared stale mount before remount");
            return;
        }
    }
}

/// `sync_state` key holding the uid of the My Files root, so a later run can
/// recover it from `nodes` without the API (offline.md Phase 1).
const ROOT_UID_KEY: &str = "root_uid";

/// The My Files root, and whether we got it from the API (`true`) or from the
/// cache because the API was unreachable (`false`).
///
/// A successful fetch also records the root's uid, which is what makes the
/// fallback possible on a later run: `nodes` is keyed by uid, so without this we
/// would have the root's row on disk and no way to tell which one it is.
///
/// Failing to fetch is only fatal on a genuinely cold start — no cached root
/// means an empty tree, and mounting that would show the user an empty Drive
/// rather than an honest error.
fn fetch_or_recall_root(
    client: &ProtonDriveClient,
    rt: &tokio::runtime::Handle,
    db: &Db,
) -> std::io::Result<(Node, bool)> {
    let err = match rt.block_on(client.get_my_files_folder()) {
        Ok(root) => {
            if let Err(e) = db.set_state_str(ROOT_UID_KEY, &root.uid.to_string()) {
                warn!(error = %e, "persist root uid failed");
            }
            return Ok((root, true));
        }
        Err(e) => e,
    };
    let cached = db
        .state_str(ROOT_UID_KEY)
        .ok()
        .flatten()
        .and_then(|uid| db.node_by_uid(&uid).ok().flatten());
    match cached {
        Some(root) => {
            warn!(error = %err, "fetch My Files root failed; mounting from cache (offline)");
            Ok((root, false))
        }
        None => Err(std::io::Error::other(format!("fetch My Files root: {err}"))),
    }
}

/// Mount the filesystem at `mountpoint` and block until it is unmounted or the
/// daemon is asked to stop.
///
/// Resolves the My Files root up front — from the API, or from the cached tree
/// when the API is unreachable ([`fetch_or_recall_root`]) — then spawns the
/// Phase 2 event-sync task, the
/// Phase 4 control socket, and runs the FUSE session on its own thread while
/// this thread waits for either a stop signal (SIGTERM/SIGINT) or the kernel
/// mount ending on its own. On a stop signal we lazily unmount ourselves
/// (`umount_and_join`, the MNT_DETACH path that succeeds even while downloads
/// are in flight), so `systemctl --user stop` is always a clean teardown.
/// `rt` must be a handle to a *running* multi-threaded runtime.
pub fn mount(
    client: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    mountpoint: &Path,
    cache: ContentCache,
    control_socket: &Path,
    db: Arc<Db>,
    username: String,
) -> std::io::Result<MountOutcome> {
    let (root, online) = fetch_or_recall_root(&client, &rt, &db)?;
    let scope = root.tree_event_scope_id();

    // The folder-sync engine (devices.md Phase 2) runs on its own thread and is
    // nudged over this channel; the sender lives in Core so control-socket
    // handlers can trigger reconciles.
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<sync::SyncMsg>();

    let core = Core {
        client: client.clone(),
        rt: rt.clone(),
        state: Arc::new(Mutex::new(State {
            entries: HashMap::new(),
            by_uid: HashMap::new(),
            children: HashMap::new(),
            next_ino: 2,
            handles: HashMap::new(),
            next_fh: 1,
            db: db.clone(),
        })),
        cache: Arc::new(cache),
        readers: Arc::new(Mutex::new(HashMap::new())),
        stream_ring: Arc::new(Mutex::new(StreamRing::default())),
        workers: Arc::new(Workers::new(FUSE_WORKERS)?),
        db,
        online: Arc::new(AtomicBool::new(online)),
        pending: Arc::new(Mutex::new(HashMap::new())),
        drain_wake: Arc::new((Mutex::new(false), Condvar::new())),
        timeline_refreshing: Arc::new(AtomicBool::new(false)),
        trash_refreshing: Arc::new(AtomicBool::new(false)),
        thumb_gen: Arc::new(Mutex::new(HashSet::new())),
        no_thumbnail: Arc::new(Mutex::new(HashMap::new())),
        transfers: TransferRegistry::new(),
        indexing: Arc::new(AtomicBool::new(false)),
        sync_progress: Arc::new(Mutex::new(HashMap::new())),
        sync_tx,
        mounts: Arc::new(Mutex::new(HashMap::new())),
        sync_locks: Arc::new(Mutex::new(HashMap::new())),
    };

    // Writes queued by a previous run (or left behind by a crash) are still owed
    // an upload, and reads must be served from their staged blobs until they land.
    core.hydrate_pending();
    // Then the writes that were fsynced but never closed, which the cache moved
    // aside at open. After `hydrate_pending`, so a recovered partial write can
    // see an already-queued write to the same node.
    core.recover_fsynced_writes();
    {
        let core = core.clone();
        std::thread::Builder::new()
            .name("pdfs-drain".into())
            .spawn(move || core.run_pending_drain())?;
    }

    // Start the folder-sync engine. It watches every mirror folder, polls the
    // remotes, and reconciles on its own thread — never in front of a FUSE call.
    sync::spawn(core.clone(), sync_rx);

    // Mounted from the cache: watch for the network coming back so the mount can
    // stop being read-only-ish without the user restarting the daemon.
    if !online {
        let core = core.clone();
        std::thread::Builder::new()
            .name("pdfs-online-probe".into())
            .spawn(move || core.run_online_probe())?;
    }

    // Re-establish on-demand mounts left over from a previous run (devices.md
    // Phase 4). On its own thread: each remount fetches a remote node, and we
    // must not block the main mount below on the network.
    {
        let core = core.clone();
        std::thread::Builder::new()
            .name("pdfs-restore-ondemand".into())
            .spawn(move || core.restore_ondemand_mounts())?;
    }

    // Keep the launcher prompt's "This computer" index fresh. Its own thread:
    // the walk is I/O-heavy and must never sit in front of a FUSE callback.
    {
        let db = core.db.clone();
        let indexing = core.indexing.clone();
        let transfers = core.transfers.clone();
        let mountpoint = mountpoint.to_path_buf();
        std::thread::Builder::new()
            .name("pdfs-localindex".into())
            .spawn(move || run_local_index(db, indexing, transfers, mountpoint))?;
    }

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("protondrive".to_string()),
        MountOption::Subtype("protondrive".to_string()),
        MountOption::DefaultPermissions,
    ];

    // A crashed previous run can leave the kernel mount in place, which makes
    // the fresh mount below fail with EBUSY. Lazily detach any leftover first.
    clear_stale_mount(mountpoint);
    info!(mountpoint = %mountpoint.display(), "mounting Proton Drive");

    // Bind the control socket before the FUSE session takes over the thread. A
    // stale socket file from a previous run would block the bind, so clear it.
    let _ = std::fs::remove_file(control_socket);
    let listener = UnixListener::bind(control_socket)?;
    // Owner-only before anything can connect: a peer on this socket commands the
    // daemon's authenticated session without a credential of its own (B6).
    if let Err(e) = pdfs_core::config::restrict_socket(control_socket) {
        error!(error = %e, "cannot restrict control socket permissions; refusing to serve");
        let _ = std::fs::remove_file(control_socket);
        return Err(std::io::Error::other(format!(
            "control socket permissions: {e}"
        )));
    }
    {
        let core = core.clone();
        let username = username.clone();
        let mountpoint = mountpoint.to_path_buf();
        std::thread::Builder::new()
            .name("pdfs-control".into())
            .spawn(move || run_control_socket(core, username, mountpoint, listener))?;
    }

    let fs = ProtonFs::new(core.clone(), root);
    // Warm the in-memory maps from the DB so a cold start serves previously
    // seen metadata without re-hitting the API (plan.md P1).
    core.hydrate();

    // Build the session explicitly (not `mount2`) so we can grab a `Notifier`
    // for the event task. `spawn` runs the session loop on its own background
    // thread; we then wait here for either a stop signal or the mount ending.
    let bg = Session::new(fs, mountpoint, &config)?.spawn()?;
    let notifier = bg.notifier();
    rt.spawn(run_event_sync(
        client,
        scope,
        core.state,
        core.cache,
        core.db,
        core.pending,
        notifier,
    ));

    // Stop signals (SIGTERM from `systemctl --user stop`, SIGINT from Ctrl-C)
    // are delivered onto the async runtime; bridge them onto a sync channel so
    // the loop below can react without blocking a worker thread. A bounded
    // channel of 1 is enough — we only need to know that *a* stop arrived.
    let (sig_tx, sig_rx) = std::sync::mpsc::sync_channel::<()>(1);
    rt.spawn(async move {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "install SIGTERM handler failed");
                    return;
                }
            };
        let mut sigint =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "install SIGINT handler failed");
                    return;
                }
            };
        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM"),
            _ = sigint.recv() => info!("received SIGINT"),
        }
        let _ = sig_tx.try_send(());
    });

    // Wait for whichever happens first: a stop signal (→ we unmount ourselves
    // via the lazy MNT_DETACH path, clean even mid-download), or the kernel
    // mount ending on its own (→ the session thread finishes). Poll instead of
    // blocking on `join` so we can also notice the signal.
    let outcome = loop {
        match sig_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(()) => {
                info!("stop requested; unmounting");
                if let Err(e) = bg.umount_and_join() {
                    warn!(error = %e, "umount_and_join failed");
                }
                break MountOutcome::Shutdown;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if bg.guard.is_finished() {
                    info!("mount ended externally");
                    if let Err(e) = bg.join() {
                        warn!(error = %e, "session join failed");
                    }
                    break MountOutcome::Unmounted;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Signal task gone (failed to install); fall back to join.
                let _ = bg.join();
                break MountOutcome::Unmounted;
            }
        }
    };

    // Unmount every on-demand sync folder too, or the kernel mounts linger as
    // stale and the next start fails with EBUSY (devices.md Phase 3).
    let secondaries: Vec<_> = core.mounts.lock().drain().collect();
    for (id, session) in secondaries {
        if let Err(e) = session.umount_and_join() {
            warn!(id, error = %e, "unmount on-demand folder failed");
        }
    }

    let _ = std::fs::remove_file(control_socket);
    Ok(outcome)
}

#[cfg(test)]
mod thumb_tests {
    use super::{THUMB_EDGE, ratio_of, scale_thumbnail};

    /// A `width`×`height` JPEG, standing in for a camera photo the server never
    /// generated a thumbnail for.
    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode_image(&image)
            .unwrap();
        bytes
    }

    #[test]
    fn scaling_fits_the_long_edge_and_keeps_the_aspect_ratio() {
        let photo = jpeg(4000, 3000);
        let thumb = scale_thumbnail(&photo).expect("a JPEG scales");

        let (width, height) = image::ImageReader::new(std::io::Cursor::new(&thumb.bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!(width, THUMB_EDGE, "the long edge lands on the target");
        assert_eq!(height, THUMB_EDGE * 3 / 4, "and nothing is cropped");
        assert!((thumb.ratio - 4.0 / 3.0).abs() < 1e-6);
        assert!(
            thumb.bytes.len() < photo.len(),
            "a thumbnail that isn't smaller than its photo is no thumbnail"
        );
    }

    #[test]
    fn a_portrait_photo_fits_its_long_edge_too() {
        let thumb = scale_thumbnail(&jpeg(1000, 2000)).expect("a JPEG scales");
        assert!(thumb.ratio < 1.0, "portrait stays portrait");
        assert_eq!(ratio_of(&thumb.bytes).map(|r| r < 1.0), Some(true));
    }

    #[test]
    fn undecodable_bytes_are_not_a_thumbnail() {
        // What a photo in a format this build has no decoder for looks like: the
        // caller writes it off as un-thumbnailable rather than retrying forever.
        assert!(scale_thumbnail(b"not an image at all").is_none());
        assert!(ratio_of(b"not an image at all").is_none());
    }

    #[test]
    fn ratio_is_read_from_the_header_alone() {
        assert_eq!(ratio_of(&jpeg(300, 200)), Some(1.5));
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
mod tests {
    use super::{Intervals, conflict_name};

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
}
