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
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixListener;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::ReplyXattr;
use fuser::{
    AccessFlags, BackgroundSession, BsdFileFlags, Config, CopyFileRangeFlags, Errno, FileAttr,
    FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags, IoctlFlags,
    KernelConfig, LockOwner, MountOption, Notifier, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyIoctl, ReplyLseek,
    ReplyOpen, ReplyStatfs, ReplyWrite, Request, Session, TimeOrNow, WriteFlags,
};
use futures::StreamExt as _;
use pdfs_core::batch;
use pdfs_core::cache::{BLOCK_SIZE, Baseline, BlockGeometry, BlockSpan, ContentCache, StagedWrite};
use pdfs_core::config::{AppDirs, SweepMode};
use pdfs_core::control::{
    ActivityEntry, ActivityKind, DirEntry, ErrorKind, LocalHit, PhotoKind, PublicLinkInfo,
    SearchFilters, SearchHit, SearchSource, SyncFolderInfo, SyncPhase, SyncProgress,
    ThumbnailBuildStatus, TransferDirection,
};
use pdfs_core::db::{
    Db, LOCAL_VOLUME, OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH, PARK_UNTIL, PendingOp,
    PublishedSharedRoot, RenameMeta, StoredNode, StoredSyncFolder, StoredTrash,
};
use pdfs_core::localindex;
use pdfs_core::search::relevance_score;
use pdfs_core::syncignore::is_transient_name;
use pdfs_core::{Access, CoreError, CoreResult, access_for, perm_bits};
use proton_drive_rs::proton_sdk::api::ResponseCode;
use proton_drive_rs::proton_sdk::error::ProtonError;
use proton_drive_rs::proton_sdk::ids::{DriveEventId, LinkId, NodeUid, ShareId, VolumeId};
use proton_drive_rs::{
    DriveEvent, DriveEventScopeId, MemberRole, Node, NodeKind, ProtonDriveClient,
    ProtonPhotosClient, RevisionReader, SharedWithMeItem, ThumbnailType,
};

mod albums;
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
mod revisions;
mod sharing;
mod shutdown;
mod state;
mod sweep;
mod sync;
mod takeout;
mod transfers;
mod upload;
mod r#virtual;
mod workers;
use background::{run_event_sync, run_local_index};
pub(crate) use mount::is_stale_mount;
pub use mount::{MountOptions, MountOutcome, mount};
use mount::{
    SecondaryInsertRejection, SecondaryMount, clear_stale_mount, fuse_connection_id, spawn_session,
};
use reads::{BlockFlight, BlockRing, PREFETCH_BUDGET, Prefetch, ReaderSlot, STREAM_BYPASS_MIN};
use state::{Entry, Intervals, PendingRevision, State, StateGuard, WriteHandle, lock_state};
use tracing::{debug, error, info, warn};
use transfers::{CountingWriter, JobGuard, OwnedCountingReader, TransferRegistry};
use r#virtual::{
    SharedListingPlan, SharedRefreshDeadlines, disambiguate_shared_names, is_own_or_virtual_uid,
    is_primary_root_listing, is_virtual_uid, listing_needs_refresh, refresh_generation_is_current,
    shared_listing_plan, shared_with_me_uid, virtual_node, virtual_root_name,
};
use workers::{FUSE_WORKERS, Lane, Workers};

/// Attribute/entry cache lifetime handed back to the kernel. Long because the
/// Phase 2 event poller actively invalidates changed inodes; without a remote
/// change this is how long the kernel may serve stale metadata.
const TTL: Duration = Duration::from_secs(30);

/// How long a `statfs(2)` answer is served from [`Core::quota`] before the
/// account figures are refetched. See [`Core::account_quota_cached`].
const QUOTA_TTL: Duration = Duration::from_secs(60);

/// Block size reported to the kernel and to `statfs(2)`.
///
/// coreutils and most copy tools size their I/O buffers from `st_blksize`, and
/// the fuser default of 512 makes them issue reads two orders of magnitude
/// smaller than the [`BLOCK_SIZE`] this filesystem actually fetches in. 1 MiB is
/// the largest value that stays well inside the kernel's per-request limit.
const REPORTED_BLKSIZE: u32 = 1 << 20;

/// Ceiling on in-flight background requests (readahead, writeback). The kernel
/// default of 16 caps how much of a sequential read the kernel will run ahead of
/// the application, which on a high-latency link is the whole ballgame.
const MAX_BACKGROUND: u16 = 64;

/// Longest single path component, as reported by `statfs(2)`. The kernel's own
/// dirent limit; Proton's is not lower.
const MAX_NAME_LEN: u32 = 255;

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
/// Ceiling on the *adaptive* part of that grace period.
///
/// The fixed 2 seconds is enough for a preallocate-then-write tool, and wrong
/// for a file that takes half a minute to upload: an editor saving every ten
/// seconds queues a write, watches it start uploading, and supersedes it — each
/// save uploading bytes that the next one replaces, and never catching up.
/// Widening the debounce toward how long this node's last upload actually took
/// makes the supersede happen in the queue, where it is free, instead of on the
/// wire. Bounded so a file on a slow link still reaches Drive in a minute rather
/// than deferring indefinitely, and so a one-off stall cannot park a node.
const DRAIN_REVISION_DEBOUNCE_MAX: Duration = Duration::from_secs(60);
/// How many nodes' measured upload times to remember for that. Bounds a map
/// that would otherwise grow with every file ever written; the debounce falls
/// back to [`DRAIN_REVISION_DEBOUNCE`] for anything evicted, which is the
/// behaviour a node that has never been uploaded gets anyway.
const UPLOAD_TIME_MEMORY: usize = 512;
/// Threads draining the pending-op queue.
///
/// One meant a single 10 GiB upload blocked every queued rename, trash and
/// small write behind it. Ordering only has to hold per node — enforced by the
/// claim query, not by the thread count — so the rest is throughput. Kept small:
/// these are uploads, and past a handful they compete for the same uplink and
/// for the SDK's own per-request concurrency.
const DRAIN_WORKERS: usize = 3;

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
/// Shared roots and foreign-volume folders have no event manager. Revalidate
/// them on access while online; stale persisted listings remain usable offline.
const SHARED_LISTING_TTL: Duration = Duration::from_secs(60);
/// How many trashed nodes are materialized per
/// [`ProtonDriveClient::enumerate_nodes`] call, and persisted as a batch. The
/// trash of an account that has been through a conflict storm runs to thousands
/// of nodes, each costing an S2K unlock, so a single all-or-nothing call can
/// take minutes and lose everything it decrypted if it fails. Same value as the
/// SDK's own `MAX_BATCH_COUNT`: one chunk is exactly one request.
const TRASH_MATERIALIZE_CHUNK: usize = 150;
/// How long a `ListTrash` request waits for a first-ever refresh before
/// answering with whatever has materialized so far.
///
/// The refresh keeps running in the background either way — this bounds the
/// *reply*, not the work. It has to stay well under the front-ends' 120 s
/// control-socket read timeout (`pdfs_core::control`), because a request that
/// outlives that timeout is a hang from the user's side: the client gives up,
/// the user asks again, and the daemon accumulates another refresh.
const TRASH_FIRST_WAIT: Duration = Duration::from_secs(20);

/// `sync_state` keys for the freshness stamps of the two persisted listings, and
/// for whether the account has a photos volume at all (so an account without one
/// doesn't re-ask the server on every page request).
const PHOTOS_SYNCED_MS: &str = "photos_synced_ms";
const PHOTOS_AVAILABLE: &str = "photos_available";
/// Freshness stamp of the album listing, and of one album's contents (suffixed
/// with the album uid). Separate from the timeline's: albums are enumerated by
/// their own endpoints, and an album is only fetched when it is opened.
const ALBUMS_SYNCED_MS: &str = "albums_synced_ms";
const ALBUM_SYNCED_PREFIX: &str = "album_synced_ms:";
const TRASH_SYNCED_MS: &str = "trash_synced_ms";
const SHARED_WITH_ME_NAME: &str = "shared_with_me_name";
const SHARED_WITH_ME_SYNCED_MS: &str = "shared_with_me_synced_ms";
const SHARED_FOLDER_SYNCED_PREFIX: &str = "shared_folder_synced_ms:";

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

/// How many folders may have a size upgrade in flight at once.
///
/// Each one owns a thread and an API request stream, and the single-flight only
/// deduplicates *within* a folder — so without this a recursive listing scaled
/// both with the number of folders it walked. Eight keeps an interactive `ls -l`
/// (one folder, sometimes a couple) entirely unaffected while putting a ceiling
/// on the recursive case.
const MAX_SIZE_UPGRADES: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RootListingSnapshot {
    children: Vec<RootListingChild>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RootListingChild {
    ino: u64,
    entry: Option<RootListingEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RootListingEntry {
    uid: NodeUid,
    name: String,
    parent: u64,
    trashed: bool,
    unlinked: bool,
}

impl RootListingSnapshot {
    fn capture(state: &State, parent_ino: u64) -> Option<Self> {
        let children = state.children.get(&parent_ino)?;
        Some(Self {
            children: children
                .iter()
                .map(|ino| RootListingChild {
                    ino: *ino,
                    entry: state.entries.get(ino).and_then(|entry| {
                        (!is_virtual_uid(&entry.uid)).then(|| RootListingEntry {
                            uid: entry.uid.clone(),
                            name: entry.node.name.clone(),
                            parent: entry.parent,
                            trashed: entry.node.trashed,
                            unlinked: entry.unlinked,
                        })
                    }),
                })
                .collect(),
        })
    }

    fn real_names(&self) -> HashSet<String> {
        self.children
            .iter()
            .filter_map(|child| child.entry.as_ref())
            .map(|entry| entry.name.clone())
            .collect()
    }

    fn is_current(&self, state: &State, parent_ino: u64) -> bool {
        Self::capture(state, parent_ino).as_ref() == Some(self)
    }
}

struct VirtualRootPlan {
    node: Node,
    visible: bool,
}

/// The sync folders a search hit can be opened through, as
/// (Drive-relative root path, local directory) pairs.
///
/// Built once per search by [`Core::search_roots`]; resolving a hit against it
/// is string arithmetic over the paths the index already carries, with no
/// further queries.
struct SearchRoots {
    roots: Vec<(String, PathBuf)>,
}

impl SearchRoots {
    /// The absolute local path for a hit at Drive-relative `path`, through the
    /// most specific root covering it. A node can sit below several configured
    /// roots, so the one leaving the shortest descendant path wins.
    fn resolve(&self, path: &str) -> Option<String> {
        self.roots
            .iter()
            .filter_map(|(root, local)| {
                let relative = relative_to(root, path)?;
                Some((relative.split('/').count(), local.join(relative)))
            })
            .min_by_key(|(depth, _)| *depth)
            .map(|(_, path)| path.to_string_lossy().into_owned())
    }
}

/// Negative thumbnail knowledge has two different authorities. A remote miss
/// only says Proton has no thumbnail to download; it must never suppress local
/// generation. A local miss says this exact file revision was successfully
/// inspected and could not be decoded. Both maps are bounded independently.
#[derive(Default)]
struct ThumbnailMissCaches {
    remote: HashMap<(NodeUid, i32), i64>,
    local: HashMap<(NodeUid, i32), i64>,
}

impl ThumbnailMissCaches {
    fn remote_contains(&self, key: &(NodeUid, i32), tag: i64) -> bool {
        self.remote.get(key) == Some(&tag)
    }

    fn local_contains(&self, key: &(NodeUid, i32), tag: i64) -> bool {
        self.local.get(key) == Some(&tag)
    }

    fn remember_remote(&mut self, key: (NodeUid, i32), tag: i64) {
        Self::remember(&mut self.remote, key, tag);
    }

    fn remember_local(&mut self, key: (NodeUid, i32), tag: i64) {
        Self::remember(&mut self.local, key, tag);
    }

    fn forget_local(&mut self, key: &(NodeUid, i32)) {
        self.local.remove(key);
    }

    fn remember(map: &mut HashMap<(NodeUid, i32), i64>, key: (NodeUid, i32), tag: i64) {
        if map.len() >= MAX_THUMBNAIL_MISSES && !map.contains_key(&key) {
            map.clear();
        }
        map.insert(key, tag);
    }
}

/// `path` expressed relative to `root`, or `None` if it is not below it. The
/// root itself resolves to the empty path; an empty root is the mount itself
/// and covers everything.
fn relative_to<'a>(root: &str, path: &'a str) -> Option<&'a str> {
    if root.is_empty() {
        return Some(path);
    }
    if path == root {
        return Some("");
    }
    path.strip_prefix(root)?.strip_prefix('/')
}

#[derive(Clone)]
struct Core {
    client: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    /// The daemon's My Files root. Forked on-demand sessions retain this value
    /// so ownership checks never mistake their own mount root for account scope.
    primary_root_uid: NodeUid,
    /// False only for a forked on-demand device mount. The synthetic shared
    /// directory belongs exclusively to the primary My Files inode space.
    primary: bool,
    state: Arc<Mutex<State>>,
    cache: Arc<ContentCache>,
    /// Open [`RevisionReader`]s keyed by node, so the block fetches of a file
    /// resolve its keys and block table once instead of once per block.
    /// Validated by `(mtime, size)` exactly like the content cache, and bounded
    /// by [`MAX_OPEN_READERS`].
    readers: Arc<Mutex<HashMap<NodeUid, ReaderSlot>>>,
    /// Decrypted blocks held in memory, so the ~32 kernel reads a 4 MiB block is
    /// delivered in cost the block once — a download for a streaming file, a
    /// disk read for a cached one. See [`BlockRing`].
    block_ring: Arc<Mutex<BlockRing>>,
    /// Block fetches in flight, so concurrent readers of one block (demand read,
    /// kernel read-ahead, our own prefetch) share a single download and decrypt.
    /// See [`BlockFlight`].
    block_flight: Arc<BlockFlight>,
    /// One lock per file being repaired by [`Core::repair_block`], so the
    /// concurrent block reads that all discover the same untrustworthy block
    /// table share a single whole-file download instead of one each.
    content_repairs: Arc<tokio::sync::Mutex<HashMap<NodeUid, Arc<tokio::sync::Mutex<()>>>>>,
    /// Per-file sequential-read detection driving [`Core::prefetch`].
    prefetch: Arc<Prefetch>,
    /// Permits bounding speculative block fetches, so prefetch cannot queue in
    /// front of a read someone is waiting on.
    prefetch_budget: Arc<tokio::sync::Semaphore>,
    /// Threads that serve the FUSE handlers which touch the network, so the
    /// session's dispatch loop stays free to answer cheap metadata calls while a
    /// cold read is on the wire. See [`Workers`].
    workers: Arc<Workers>,
    /// Unified SQLite metadata cache: the persistence layer behind the in-memory
    /// `State` maps. Every mutation writes through here, and the maps rehydrate
    /// from it on mount (plan.md P1).
    db: Arc<Db>,
    /// Serializes only shared-list publication and access-loss invalidation.
    /// Network calls happen before taking it.
    shared_publication: Arc<Mutex<()>>,
    /// Invalidates in-flight shared responses when access/list events arrive.
    shared_generation: Arc<AtomicU64>,
    /// Successful refreshes suppress retries even if persisting their timestamp
    /// fails. Shared across primary/fork clones and cleared by access events.
    shared_refresh_deadlines: Arc<Mutex<SharedRefreshDeadlines>>,
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
    /// Nodes removed through this daemon which a briefly stale remote listing
    /// may still return. Uids are not reused; an explicit restore clears one.
    hidden: Arc<Mutex<HashSet<NodeUid>>>,
    /// Nudges the drain workers: set true and notify to have them re-examine
    /// the queue instead of waiting out their backoff.
    drain_wake: Arc<(Mutex<bool>, Condvar)>,
    /// The daemon's stop signal, shared with every background loop this `Core`
    /// was cloned into so teardown can end and join them (bugs.md B44).
    shutdown: Arc<shutdown::Shutdown>,
    /// How long this node's last upload actually took, in milliseconds, with
    /// the time it was measured for eviction. Feeds
    /// [`Core::revision_debounce`]: a file that takes 30 seconds to send needs
    /// a grace period on that scale, or every save supersedes an upload that
    /// was already on the wire. Bounded by [`UPLOAD_TIME_MEMORY`].
    upload_times: Arc<Mutex<HashMap<String, (u64, i64)>>>,
    /// Cancellation flags for uploads currently on the wire, keyed by node.
    ///
    /// Set when a newer revision of the same file is queued: the bytes in
    /// flight have just been superseded, and the only thing finishing them
    /// achieves is spending the user's uplink on a revision the next op
    /// replaces. Read by the upload's own reader ([`CountingReader`]), which is
    /// the one thing the SDK calls often enough to notice.
    upload_cancel: Arc<Mutex<HashMap<NodeUid, Arc<AtomicBool>>>>,
    /// True while a background refresh of the photos timeline (resp. the trash) is
    /// already running, so a burst of page requests against a stale listing kicks
    /// off one refresh rather than one per request.
    timeline_refreshing: Arc<AtomicBool>,
    /// The same, for the album listing.
    albums_refreshing: Arc<AtomicBool>,
    trash_refreshing: Arc<AtomicBool>,
    /// Fires whenever a trash refresh persists a batch or finishes, so a
    /// `ListTrash` request waiting on a first-ever refresh wakes on progress
    /// instead of polling or blocking for the whole run. See
    /// [`Core::await_trash_refresh`].
    trash_progress: Arc<tokio::sync::Notify>,
    /// Conflict copies the sweep has already flagged as needing attention this
    /// run, so a divergent `(sync-conflict …)` file is logged once rather than
    /// on every sweep pass. See [`Core::run_conflict_sweep_loop`].
    conflict_notified: Arc<Mutex<HashSet<NodeUid>>>,
    /// Whether the conflict sweep may actually trash the duplicates it finds, or
    /// only report them. Resolved once at mount from config + environment
    /// ([`AppConfig::resolved_conflict_sweep`]); [`SweepMode::Off`] means the
    /// sweep thread is never spawned at all. See `docs/BUGS.md` B71.
    sweep_mode: SweepMode,
    /// The latest server revision id this daemon has itself sealed, keyed by
    /// node. A queued write whose baseline names an *earlier* revision would
    /// normally fork into a `(sync-conflict)` copy when the drain finds the
    /// remote already moved on — but if the remote sits at a revision *we*
    /// sealed, no other device touched the file: it is a single-writer
    /// stall→resume (a browser download that closed and reopened its fd, then
    /// rewrote the whole file). [`Core::revision_conflict`] consults this to
    /// adopt our own revision as the base and supersede instead of forking
    /// (docs/BUGS.md B70, layer B). One entry per written node, overwritten as
    /// the node advances and dropped when it is trashed; lost on restart, which
    /// only widens the (already narrow) fork window across a daemon bounce.
    own_sealed_revs: Arc<Mutex<HashMap<NodeUid, String>>>,
    /// Photos whose missing thumbnail is being generated right now. A tile that is
    /// still on screen asks for its thumbnail again every few seconds, and each of
    /// those downloads is a full-size photo — so an in-flight uid is never started
    /// twice.
    thumb_gen: Arc<Mutex<HashSet<NodeUid>>>,
    /// Global permit pool for locally generated thumbnails. The old per-request
    /// limit multiplied when the GUI sent several batches, allowing dozens of
    /// full-size images to download at once.
    thumb_gen_budget: Arc<tokio::sync::Semaphore>,
    /// Current ordinary-file listing generation and a wake-up for tasks waiting
    /// on a permit or a download when that listing is abandoned.
    file_thumb_generation: Arc<AtomicU64>,
    file_thumb_cancel: Arc<tokio::sync::Notify>,
    /// Progress of the explicit recursive build started from the Files toolbar.
    thumbnail_build: Arc<Mutex<ThumbnailBuildStatus>>,
    /// Cancellation for the explicit recursive build. Separate from ordinary
    /// listing cancellation because the two jobs have independent lifetimes.
    thumbnail_build_cancelled: Arc<AtomicBool>,
    thumbnail_build_cancel: Arc<tokio::sync::Notify>,
    /// Negative thumbnail knowledge, separated by whether the remote lacked a
    /// thumbnail or local decoding proved this revision unsupported.
    ///
    /// Absence has to be cached or it costs a round trip every time it is asked
    /// for, and it is asked for constantly: an `ls -l` from an xattr-aware lister
    /// issues a `getxattr` per advertised name per entry, so a 65-file directory
    /// of videos re-probed 130 times per listing at ~186 ms each (B5). The mtime
    /// is the validity tag — a new revision may well have a thumbnail — matching
    /// how [`ContentCache::read_thumbnail`] validates the positive side.
    thumbnail_misses: Arc<Mutex<ThumbnailMissCaches>>,
    /// Last account quota answer and when it was learned, for `statfs(2)`.
    ///
    /// `df` and the free-space preflight of every file manager and installer ask
    /// for this, and the answer costs a round trip — so it is served from here
    /// for [`QUOTA_TTL`] and refreshed on a worker afterwards. Total account
    /// storage moves slowly enough that a minute-old number is a better answer
    /// than either a stall or the zeroes the default implementation returns.
    quota: Arc<Mutex<Option<(std::time::Instant, i64, i64)>>>,
    /// Database maintenance that has been moved off the request thread.
    ///
    /// `VACUUM` and a deep `integrity_check` both hold the single connection for
    /// tens of seconds against a real install, which stops every FUSE handler
    /// that needs the database — so the handler acknowledges the request and a
    /// thread does the work, exactly as the other long control requests already
    /// do. See [`Maintenance`].
    maintenance: Arc<Mutex<Maintenance>>,
    /// Folders whose listing was enumerated cheaply and is having its file sizes
    /// filled in right now, so a burst of `stat`s over a fresh listing starts one
    /// upgrade rather than one per entry. See [`Core::spawn_size_upgrade`].
    /// Size upgrades currently running, per folder inode. A `getattr` that
    /// needs a real size waits on the entry rather than issuing its own fetch —
    /// `ls -l` of a folder is one `getattr` per file, and they must collapse
    /// onto a single batch (bugs.md B14).
    size_upgrades: Arc<Mutex<HashMap<u64, Arc<SizeUpgrade>>>>,
    /// Replies parked on those upgrades. Shared across every mount for the same
    /// reason the pool is: the bound that matters is per daemon.
    size_waiters: Arc<SizeWaitQueue>,
    /// This mount's kernel notification channel, for telling the kernel to drop
    /// metadata it has cached. Set once the session exists — which is *after*
    /// the `Core` it is built from, hence the cell.
    ///
    /// Per mount, not per daemon: each on-demand fork runs its own session over
    /// its own inode space, so notifying through the primary mount's channel
    /// would name inodes that session has never heard of. [`Core::fork_state`]
    /// gives each fork an empty cell of its own.
    notifier: Arc<OnceLock<Notifier>>,
    /// True only after this inode space's FUSE session spawned successfully.
    /// Registration happens earlier for the primary drain, so state residency
    /// alone cannot answer whether the location is mounted.
    session_live: Arc<AtomicBool>,
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
    /// back to `mirror` and on daemon shutdown. Each entry retains the FUSE
    /// connection id and the fork's exact liveness flag so teardown first makes
    /// that inode space unroutable.
    mounts: Arc<Mutex<mount::SecondaryMountRegistry<SecondaryMount>>>,
    /// Per-sync-folder locks, held for a whole reconcile pass and for a whole
    /// mode switch. A `mirror→ondemand` flip evicts the local tree and mounts
    /// FUSE over it, so it must never overlap a pass that is walking and
    /// uploading that same tree — the engine would upload files as they vanish
    /// and then walk the FUSE mount as if it were local.
    sync_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
    /// Nodes this daemon changed on the remote itself, and how many echoes of
    /// those changes the event feed still owes us.
    ///
    /// The feed replays our own writes back at us seconds later, and a
    /// `NodeUpdated` carries no revision id — nothing in it distinguishes "another
    /// device changed this file" from "you changed this file". Treating the echo
    /// as foreign is not free: it evicts the content blob we just uploaded, so the
    /// next read re-downloads a revision the API may not be serving yet. That is
    /// how a SQLite database written on the mount came back `malformed`, and how a
    /// mount doing bulk uploads spent its time re-fetching its own bytes.
    ///
    /// So the first event for a node we changed ourselves, within
    /// [`SELF_CHANGE_TTL_MS`], is consumed rather than applied: the tree already
    /// holds what that event describes, because [`Core::refresh_after_upload`] put
    /// it there. Anything beyond that applies normally, which bounds the staleness
    /// a genuine remote change can suffer to one event.
    ///
    /// Shared across forks like `pending`: the drain that records the change and
    /// the event task that consumes the echo are both per-daemon.
    self_changes: Arc<Mutex<HashMap<NodeUid, SelfChange>>>,
    /// Every live inode space: this mount's own `state` plus one per on-demand
    /// fork ([`Core::fork_state`]). Shared by every fork, unlike `state` itself.
    ///
    /// There is exactly **one** drain thread per daemon, owned by the primary
    /// mount, and it serves the whole `pending_op` table — including ops queued
    /// by a forked mount, which keeps its nodes in its own `state`. So a drain
    /// that reaches for `self.state` looks in the wrong inode space and silently
    /// finds nothing: the fork's entry keeps its `local~` placeholder uid
    /// forever, and every read of it short-circuits to empty because
    /// [`Core::read_range`] refuses to ask the API about a `local~` uid. That
    /// was `docs/BUGS.md` B74. Background work that rewrites node identity must
    /// walk this instead ([`Core::for_each_state`]).
    ///
    /// `Weak`, so an unmounted fork's state is dropped rather than pinned here.
    states: Arc<StateRegistry>,
}

/// Background database maintenance: what is running, and what the last deep
/// integrity check found.
///
/// One at a time — both operations are whole-database and there is nothing to
/// gain from overlapping them — and the result of a check outlives the request
/// that asked for it, because the request is answered before the check finishes.
#[derive(Default)]
struct Maintenance {
    /// A vacuum or integrity check is running right now.
    running: bool,
    /// Findings of the most recent completed deep check. `None` means none has
    /// finished this run, which is *not* the same as "found nothing".
    integrity: Option<Vec<String>>,
}

/// How long a change this daemon made stays recognisable as its own. Generous
/// against a feed that can lag, but finite: past it, an event for the node is
/// applied like any other. See [`Core::self_changes`].
const SELF_CHANGE_TTL_MS: i64 = 120_000;

/// A remote change this daemon made, awaiting its echo from the event feed.
struct SelfChange {
    /// When we made it, in the same epoch millis as [`now_millis`].
    at_ms: i64,
    /// Echoes not yet seen. More than one because a single file can be created
    /// and then have a revision uploaded, and the feed reports both.
    echoes: u32,
}

/// Record one remote change this daemon made. Split from [`Core`] so the
/// expiry and echo-counting rules can be tested against a clock the test owns.
fn note_self_change(changes: &mut HashMap<NodeUid, SelfChange>, uid: &NodeUid, now_ms: i64) {
    // Echoes that never arrived — a node trashed before its event came round, a
    // feed that skipped it — would otherwise accumulate for the life of the
    // daemon. Pruning on write keeps the map the size of the recent queue.
    changes.retain(|_, c| now_ms - c.at_ms < SELF_CHANGE_TTL_MS);
    let change = changes.entry(uid.clone()).or_insert(SelfChange {
        at_ms: now_ms,
        echoes: 0,
    });
    change.at_ms = now_ms;
    change.echoes += 1;
}

/// Claim one echo, per [`Core::take_self_change`].
fn take_self_change(
    changes: &mut HashMap<NodeUid, SelfChange>,
    uid: &NodeUid,
    now_ms: i64,
) -> bool {
    let Some(change) = changes.get_mut(uid) else {
        return false;
    };
    // Too old to attribute: the feed is far enough behind that this is as likely
    // to be someone else's change as ours, and guessing wrong here costs the user
    // a stale file.
    if now_ms - change.at_ms >= SELF_CHANGE_TTL_MS {
        changes.remove(uid);
        return false;
    }
    change.echoes -= 1;
    if change.echoes == 0 {
        changes.remove(uid);
    }
    true
}

/// One live inode space, as published to the shared registry.
///
/// The notifier travels with the state because the two are only meaningful
/// together: an inode number names a different node in every mount, so telling
/// the kernel to drop inode 42 is only correct on the session that minted it.
/// Background work that invalidates as well as mutates needs the matching pair,
/// which is what [`Core::for_each_mount`] hands it.
type LiveMount = (
    PathBuf,
    Arc<Mutex<State>>,
    Arc<OnceLock<Notifier>>,
    Arc<AtomicBool>,
);

struct MountedState {
    /// Absolute local root of this inode space.
    mountpoint: PathBuf,
    /// `Weak`, so an unmounted fork's state is dropped rather than pinned here;
    /// a dead entry is reaped on the next registry walk.
    state: std::sync::Weak<Mutex<State>>,
    /// This mount's kernel notification channel — the same cell as
    /// [`Core::notifier`], still empty until its session is spawned.
    notifier: Arc<OnceLock<Notifier>>,
    session_live: Arc<AtomicBool>,
    /// This mount's in-flight size upgrades, keyed by *its* folder inodes — so
    /// a `Core` rebuilt onto this inode space by [`Core::rooted_at`] shares the
    /// batch the mount's own session is already running rather than starting a
    /// second one against inode numbers that mean nothing to it.
    size_upgrades: Arc<Mutex<HashMap<u64, Arc<SizeUpgrade>>>>,
}

/// The per-mount half of a [`Core`] — every field [`Core::fork_state`] gives a
/// fork a fresh copy of — recovered from the registry.
///
/// This is what lets a control request naming a path under a secondary mount be
/// answered *in that mount's inode space* instead of being rejected for not
/// being under the primary mountpoint (`docs/BUGS.md` B86). See
/// [`Core::rooted_at`].
struct MountParts {
    mountpoint: PathBuf,
    state: Arc<Mutex<State>>,
    notifier: Arc<OnceLock<Notifier>>,
    session_live: Arc<AtomicBool>,
    size_upgrades: Arc<Mutex<HashMap<u64, Arc<SizeUpgrade>>>>,
}

/// Every mounted inode space in the daemon, shared by the primary `Core` and
/// every [`Core::fork_state`] clone of it.
///
/// Split out from `Core` so the reap-and-walk rule can be tested on its own: a
/// fork that fails to appear here, or one that lingers after unmount, is exactly
/// the failure `docs/BUGS.md` B74 was.
#[derive(Default)]
struct StateRegistry(Mutex<Vec<MountedState>>);

impl StateRegistry {
    /// Publish an inode space and the channel to the session that owns it.
    ///
    /// A state/path pair is registered exactly once by its owner. Session
    /// construction only flips `session_live`; it never republishes the state.
    fn register(
        &self,
        mountpoint: &Path,
        state: &Arc<Mutex<State>>,
        notifier: Arc<OnceLock<Notifier>>,
        session_live: Arc<AtomicBool>,
        size_upgrades: Arc<Mutex<HashMap<u64, Arc<SizeUpgrade>>>>,
    ) {
        let mut states = self.0.lock();
        states.retain(|m| m.state.strong_count() > 0);
        debug_assert!(
            !states
                .iter()
                .any(|mounted| mounted.state.ptr_eq(&Arc::downgrade(state))),
            "an inode state must be registered exactly once"
        );
        states.push(MountedState {
            mountpoint: mountpoint.to_path_buf(),
            state: Arc::downgrade(state),
            notifier,
            session_live,
            size_upgrades,
        });
    }

    /// Every live mount, reaping the entries whose session has gone. Returns
    /// owned handles so callers take the per-state locks one at a time rather
    /// than holding the registry lock across the work.
    fn live(&self) -> Vec<LiveMount> {
        let mut states = self.0.lock();
        states.retain(|m| m.state.strong_count() > 0);
        states
            .iter()
            .filter_map(|m| {
                Some((
                    m.mountpoint.clone(),
                    m.state.upgrade()?,
                    m.notifier.clone(),
                    m.session_live.clone(),
                ))
            })
            .collect()
    }

    /// The live session whose local root most specifically covers `path`.
    ///
    /// On-demand roots can be nested below another location. Selecting by the
    /// longest component prefix ensures the nested session wins rather than the
    /// broader primary mount. Callers that need the relative suffix derive it
    /// with `path.strip_prefix(mountpoint)`; [`StateRegistry::covering_parts`]
    /// is the variant that also hands back enough to *serve* a request there.
    fn covering(&self, path: &Path) -> Option<LiveMount> {
        self.live()
            .into_iter()
            .filter(|(mountpoint, _, _, live)| {
                live.load(Ordering::Acquire) && path.starts_with(mountpoint)
            })
            .max_by_key(|(mountpoint, _, _, _)| mountpoint.components().count())
    }

    /// [`StateRegistry::register`] with an empty size-upgrade map, for the
    /// tests that exercise the reap-and-route rules and nothing else.
    #[cfg(test)]
    fn register_bare(
        &self,
        mountpoint: &Path,
        state: &Arc<Mutex<State>>,
        notifier: Arc<OnceLock<Notifier>>,
        session_live: Arc<AtomicBool>,
    ) {
        self.register(
            mountpoint,
            state,
            notifier,
            session_live,
            Arc::new(Mutex::new(HashMap::new())),
        );
    }

    /// [`StateRegistry::covering`], but returning every per-mount field a
    /// [`Core`] needs to be re-rooted onto that inode space.
    ///
    /// Kept separate from `covering` so the common callers — which only ask
    /// *whether* a path is mounted — keep the cheaper tuple.
    fn covering_parts(&self, path: &Path) -> Option<MountParts> {
        let mut states = self.0.lock();
        states.retain(|m| m.state.strong_count() > 0);
        states
            .iter()
            .filter(|m| m.session_live.load(Ordering::Acquire) && path.starts_with(&m.mountpoint))
            .max_by_key(|m| m.mountpoint.components().count())
            .and_then(|m| {
                Some(MountParts {
                    mountpoint: m.mountpoint.clone(),
                    state: m.state.upgrade()?,
                    notifier: m.notifier.clone(),
                    session_live: m.session_live.clone(),
                    size_upgrades: m.size_upgrades.clone(),
                })
            })
    }

    fn is_mounted_at(&self, path: &Path) -> bool {
        self.covering(path).is_some_and(|(mountpoint, _, _, live)| {
            mountpoint == path && live.load(Ordering::Acquire)
        })
    }

    /// Whether any live inode space owns and currently exposes `uid`.
    ///
    /// Retained open inodes and revoked incoming shares remain interned after
    /// their dentries disappear, so residency alone is not authority. The
    /// state's mounted root also scopes the lookup to its own volume, excluding
    /// foreign shared-with-me residents from uid-addressed sharing.
    fn owns_visible_uid(&self, uid: &NodeUid) -> bool {
        self.live()
            .into_iter()
            .filter(|(_, _, _, live)| live.load(Ordering::Acquire))
            .any(|(_, state, _, _)| state.lock().owns_visible_uid(uid))
    }
}

/// Why [`Core::apply_sync_folder_mode`] did not switch a folder. The two cases are
/// answered very differently: `NotNow` is the normal state of a folder that is busy
/// or has local changes still to push, and the caller queues the request; `Failed`
/// is a real fault the user has to hear about.
enum SwitchBlocked {
    /// The folder is mid-pass, or not yet safe to switch. Try again after a pass.
    NotNow,
    /// A queued target was canceled or replaced before the folder lock became
    /// available. The newer intent owns the next transition.
    Superseded,
    /// The switch was attempted and broke.
    Failed(String),
}

impl Core {
    /// Start a background `VACUUM`, or refuse because maintenance is already
    /// running.
    ///
    /// Returns as soon as the thread is spawned; the caller acknowledges the
    /// request. The outcome is a log line and a job that disappears from
    /// `GetQueueStatus` when it is done — a vacuum has nothing to report that a
    /// follow-up `CacheInspect` cannot show.
    fn start_vacuum(&self) -> CoreResult<()> {
        {
            let mut m = self.maintenance.lock();
            if m.running {
                return Err(CoreError::conflict(
                    "database maintenance is already running",
                ));
            }
            m.running = true;
        }
        let core = self.clone();
        std::thread::spawn(move || {
            let job = core.transfers.begin_job("Compacting the cache database");
            job.detail("Rewriting the database file");
            let started = Instant::now();
            match core.db.vacuum() {
                Ok(outcome) => info!(
                    freed_bytes = outcome.freed_bytes(),
                    before_bytes = outcome.before_bytes,
                    after_bytes = outcome.after_bytes,
                    wal_frames = outcome.wal_frames_checkpointed,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "cache database vacuumed"
                ),
                Err(e) => warn!(error = %e, "vacuum failed"),
            }
            core.maintenance.lock().running = false;
        });
        Ok(())
    }

    /// Answer a deep `CacheInspect`: hand back the last completed integrity
    /// check, and start one if none is running.
    ///
    /// Returns `(problems, checked, running)`. A first request gets
    /// `(_, false, true)` and the answer lands on a later one — the check reads
    /// every page of the database under the connection mutex, so the choice is
    /// between answering late and freezing the mount until it finishes.
    fn start_or_read_integrity_check(&self) -> (Vec<String>, bool, bool) {
        let mut m = self.maintenance.lock();
        if let Some(problems) = m.integrity.clone() {
            return (problems, true, m.running);
        }
        if m.running {
            return (Vec::new(), false, true);
        }
        m.running = true;
        drop(m);

        let core = self.clone();
        std::thread::spawn(move || {
            let job = core.transfers.begin_job("Checking the cache database");
            job.detail("Verifying every page");
            let problems = match core.db.integrity_check() {
                Ok(problems) => problems,
                Err(e) => vec![format!("integrity check failed: {e:?}")],
            };
            if problems.is_empty() {
                info!("cache database integrity check found no problems");
            } else {
                warn!(problems = problems.len(), "cache database integrity check");
            }
            let mut m = core.maintenance.lock();
            m.integrity = Some(problems);
            m.running = false;
        });
        (Vec::new(), false, true)
    }

    /// Register an inode space so background work can reach it. Called once for
    /// the primary mount and once per on-demand fork.
    fn register_state(&self, mountpoint: &Path) {
        self.states.register(
            mountpoint,
            &self.state,
            self.notifier.clone(),
            self.session_live.clone(),
            self.size_upgrades.clone(),
        );
    }

    /// This `Core`, re-rooted onto whichever live mount most specifically covers
    /// the absolute path `abs`, together with `abs` relative to that mount's
    /// root.
    ///
    /// Everything a `Core` holds is per-daemon except the five fields
    /// [`Core::fork_state`] replaces, so swapping exactly those turns the
    /// primary `Core` into the fork that owns the path — sharing the fork's
    /// inode space, notification channel and size-upgrade batches rather than
    /// shadowing them. Without this, every path-addressed control request is
    /// answered against the primary inode space, so anything under a secondary
    /// on-demand mount is rejected as "not under the mountpoint" and the user
    /// has no way to re-enumerate a folder they can see (`docs/BUGS.md` B86).
    ///
    /// `None` when no live mount covers `abs`.
    fn rooted_at(&self, abs: &Path) -> Option<(Core, PathBuf)> {
        let parts = self.states.covering_parts(abs)?;
        let rel = abs.strip_prefix(&parts.mountpoint).ok()?.to_path_buf();
        let mut core = self.clone();
        if !Arc::ptr_eq(&parts.state, &self.state) {
            // A different inode space, so adopt its per-mount half wholesale.
            // `primary` is a property of the mount, and the only mount this
            // `Core` can be the primary of is the one it was built for.
            core.primary = false;
            core.state = parts.state;
            core.notifier = parts.notifier;
            core.session_live = parts.session_live;
            core.size_upgrades = parts.size_upgrades;
        }
        Some((core, rel))
    }

    /// Run `apply` against every live inode space — this daemon's primary mount
    /// and each on-demand fork — one lock at a time.
    ///
    /// The drain and the sync engine are per-daemon but node state is per-mount,
    /// so anything they change about a node has to be offered to whichever mount
    /// actually holds it. Which one that is cannot be known from the op: a
    /// `pending_op` row records a uid, not the session that queued it. Applying
    /// to all of them is correct because a uid is unique across mounts — at most
    /// one state has an entry for it, and the rest are no-ops.
    /// Take this mount's inode lock.
    ///
    /// The guard is what applies the mutation's write-throughs, after releasing
    /// the lock — see [`StateGuard`]. Every caller takes it this way; nothing
    /// locks `self.state` directly, or the writes it queues would be applied
    /// only when some later caller happened to take the guard.
    fn state(&self) -> StateGuard<'_> {
        StateGuard::new(self.state.lock(), &self.db)
    }

    fn for_each_state(&self, mut apply: impl FnMut(&mut State)) {
        for (_, state, notifier, _) in self.states.live() {
            let changed = {
                let mut state = lock_state(&state, &self.db);
                apply(&mut state);
                state.take_access_changes()
            };
            notify_access_changes(notifier.get(), &changed);
        }
    }

    /// Like [`Core::for_each_state`], but also hands over the mount's kernel
    /// notification channel, so work that invalidates cached metadata reaches
    /// the session that actually minted the inodes it is naming.
    ///
    /// The closure records what the kernel needs told into a [`NotifyBatch`]
    /// rather than sending it: notifications are flushed after this mount's
    /// State lock is released, the same rule [`Core::for_each_state`] and
    /// [`Core::flush_access_changes`] follow.
    fn for_each_mount(&self, mut apply: impl FnMut(&mut State, &mut NotifyBatch)) {
        for (_, state, notifier, _) in self.states.live() {
            let mut batch = NotifyBatch::default();
            {
                let mut state = lock_state(&state, &self.db);
                apply(&mut state, &mut batch);
            }
            batch.flush(notifier.get());
        }
    }

    /// Publish access changes accumulated by an intern/root refresh. The State
    /// lock is released before notifying the kernel because notifier callbacks
    /// may synchronously provoke more filesystem work.
    fn flush_access_changes(&self) {
        let changed = self.state().take_access_changes();
        notify_access_changes(self.notifier.get(), &changed);
    }

    fn require_writable(&self, ino: u64) -> Result<(), Errno> {
        self.state().require_writable(ino)
    }

    /// Mutation admission point. Persisted and every resident authority must
    /// agree that the node is writable. A check completed before a downgrade is
    /// admitted; later queued attempts are caught by drain. No global lock is
    /// held across a network call.
    ///
    /// The intersection closes both halves of an access-refresh race: a DB
    /// downgrade cannot be hidden by stale live state, and a live downgrade
    /// cannot be hidden by a restored DB row. Missing UIDs fail closed.
    fn require_uid_writable(&self, uid: &NodeUid) -> Result<(), Errno> {
        match self.uid_write_authority(uid) {
            WriteAuthority::Writable => Ok(()),
            // A uid nobody can speak for is refused here just as a denied one
            // is: a syscall is a live request against a live tree, and a stale
            // handle must not be admitted because the tree forgot the node.
            // The drain, which replays intent recorded long ago, is the one
            // caller that has to tell the two apart — see
            // [`Core::uid_write_authority`].
            WriteAuthority::Denied | WriteAuthority::Unknown => Err(Errno::EACCES),
        }
    }

    /// The full answer behind [`Core::require_uid_writable`]: whether every
    /// authority agrees this node is writable, refuses it, or has never heard
    /// of it.
    ///
    /// The third case is the one worth naming. A node absent from the local
    /// tree yields no access row, and collapsing that into "denied" told the
    /// drain to wait for a permission change that was never coming, because
    /// nothing about the node's permissions was ever the problem (B83).
    pub(crate) fn uid_write_authority(&self, uid: &NodeUid) -> WriteAuthority {
        let mut live_access = Vec::new();
        {
            let state = self.state();
            if let Some(access) = state.access_by_uid(uid) {
                live_access.push(access);
            }
        }
        for (_, state, _, _) in self.states.live() {
            if Arc::ptr_eq(&state, &self.state) {
                continue;
            }
            if let Some(access) = state.lock().access_by_uid(uid) {
                live_access.push(access);
            }
        }
        uid_write_authority(&self.db, uid, &live_access)
    }
}

/// Whether a node may be written, as agreed by every authority that has an
/// opinion — or the fact that none of them does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteAuthority {
    /// The persisted tree and every resident mount agree: writable.
    Writable,
    /// At least one authority refuses it, or could not be consulted. Fails
    /// closed, so a lookup error lands here.
    Denied,
    /// The node is not in the local tree, so there is no authority to consult.
    /// Not the same as a refusal, and callers that can ask the remote should.
    Unknown,
}

fn uid_write_authority(db: &Db, uid: &NodeUid, live_access: &[Access]) -> WriteAuthority {
    let persisted = match db.effective_node_access(uid) {
        Ok(Some(access)) => access,
        Ok(None) => return WriteAuthority::Unknown,
        Err(error) => {
            error!(%uid, %error, "db effective access lookup failed");
            return WriteAuthority::Denied;
        }
    };
    if persisted.writable() && live_access.iter().all(|access| access.writable()) {
        WriteAuthority::Writable
    } else {
        WriteAuthority::Denied
    }
}

/// One kernel notification, recorded while the State lock is held so it can be
/// sent once it is not.
///
/// `inval_inode` calls into `invalidate_inode_pages2`, which can block waiting
/// on pages that this daemon's own workers have to fill — and those workers take
/// the State lock. Sending from inside the lock is that deadlock's shape; a
/// batch is the smallest way to keep the inode/mount pairing while moving the
/// send outside it.
enum KernelNotice {
    InvalInode(u64),
    InvalEntry {
        parent: u64,
        name: String,
    },
    Delete {
        parent: u64,
        child: u64,
        name: String,
    },
}

/// Notifications accumulated under one mount's State lock. Flushed by
/// [`Core::for_each_mount`] against that mount's own channel — inode numbers are
/// per-mount, so the pairing has to survive the deferral.
#[derive(Default)]
struct NotifyBatch(Vec<KernelNotice>);

impl NotifyBatch {
    fn inval_inode(&mut self, ino: u64) {
        self.0.push(KernelNotice::InvalInode(ino));
    }

    fn inval_entry(&mut self, parent: u64, name: String) {
        self.0.push(KernelNotice::InvalEntry { parent, name });
    }

    fn delete(&mut self, parent: u64, child: u64, name: String) {
        self.0.push(KernelNotice::Delete {
            parent,
            child,
            name,
        });
    }

    fn extend_inodes(&mut self, inodes: &[u64]) {
        self.0
            .extend(inodes.iter().map(|&ino| KernelNotice::InvalInode(ino)));
    }

    /// Send everything recorded. A mount whose session has not been spawned has
    /// nothing to tell the kernel, so the batch is simply dropped.
    fn flush(self, notifier: Option<&Notifier>) {
        let Some(notifier) = notifier else { return };
        for notice in self.0 {
            let _ = match notice {
                KernelNotice::InvalInode(ino) => notifier.inval_inode(INodeNo(ino), 0, 0),
                KernelNotice::InvalEntry { parent, name } => {
                    notifier.inval_entry(INodeNo(parent), OsStr::new(&name))
                }
                KernelNotice::Delete {
                    parent,
                    child,
                    name,
                } => notifier.delete(INodeNo(parent), INodeNo(child), OsStr::new(&name)),
            };
        }
    }
}

fn notify_access_changes(notifier: Option<&Notifier>, changed: &[u64]) {
    let Some(notifier) = notifier else { return };
    for &ino in changed {
        let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
    }
}

fn require_rename_access(
    require: impl Fn(&NodeUid) -> Result<(), Errno>,
    uid: &NodeUid,
    old_parent_uid: &NodeUid,
    new_parent_uid: &NodeUid,
) -> Result<(), Errno> {
    for authority in [uid, old_parent_uid, new_parent_uid] {
        require(authority)?;
    }
    Ok(())
}

fn require_node_parent_access(
    require: impl Fn(&NodeUid) -> Result<(), Errno>,
    uid: &NodeUid,
    parent_uid: &NodeUid,
) -> Result<(), Errno> {
    require(uid)?;
    require(parent_uid)
}

fn preserve_on_access_denied(
    access: Result<(), Errno>,
    accepted_bytes: bool,
    preserve: impl FnOnce(),
) -> Result<(), Errno> {
    if let Err(error) = access {
        if error.code() == libc::EACCES && accepted_bytes {
            preserve();
        }
        return Err(error);
    }
    Ok(())
}

/// Fold the staged blob of an undrained write into the blob at `src` that is
/// about to supersede it, so the newer write inherits the older one's bytes
/// instead of losing them.
///
/// Only the ranges the older write authored and the newer one did not are
/// copied, so a small edit over a small edit moves almost nothing. What neither
/// wrote is untouched remote content, which is why `base_size`, `base_mtime` and
/// the baseline all come from the write being superseded: that is the revision
/// those ranges were last observed against, and the one the drain must gap-fill
/// and conflict-check them against.
fn merge_over_pending(
    meta: &mut StagedWrite,
    src: &Path,
    previous: &PendingRevision,
) -> std::io::Result<()> {
    let mut written = Intervals::default();
    for &(s, e) in &meta.authored {
        written.add(s, e);
    }
    let mut earlier = Intervals::default();
    for &(s, e) in &previous.meta.authored {
        earlier.add(s, e);
    }
    let blob = File::open(&previous.path)?;
    let dst = std::fs::OpenOptions::new().write(true).open(src)?;
    for (s, e, authored) in written.clone().segments(0, meta.len) {
        if authored {
            continue;
        }
        for (ps, pe, have) in earlier.segments(s, e.min(previous.meta.len)) {
            if !have {
                continue;
            }
            let mut buf = vec![0u8; (pe - ps) as usize];
            blob.read_exact_at(&mut buf, ps)?;
            dst.write_all_at(&buf, ps)?;
            written.add(ps, pe);
        }
    }
    dst.sync_all()?;
    meta.base_size = previous.meta.base_size;
    meta.base_mtime = previous.meta.base_mtime;
    meta.based_on = previous.meta.based_on.clone();
    meta.authored = written
        .segments(0, meta.len)
        .into_iter()
        .filter(|&(_, _, authored)| authored)
        .map(|(s, e, _)| (s, e))
        .collect();
    meta.complete = meta.authored == [(0, meta.len)];
    Ok(())
}

/// Retire the last resident reference to an unlinked inode without touching its
/// persisted node row. The namespace path already chose whether that row is
/// disposable or retained as queued-trash authority.
fn release_unlinked_entry(state: &mut State, ino: u64) -> Option<NodeUid> {
    let entry = state.entries.get_mut(&ino)?;
    entry.open_count = entry.open_count.saturating_sub(1);
    if entry.open_count != 0 || !entry.unlinked {
        return None;
    }
    let uid = entry.uid.clone();
    state.forget_mem(&uid);
    Some(uid)
}

fn release_must_retain_queued_trash(db: &Db, uid: &NodeUid) -> pdfs_core::Result<bool> {
    db.has_pending_op(&uid.to_string(), OP_TRASH)
}

fn release_can_discard_unlinked(db: &Db, uid: &NodeUid) -> bool {
    match release_must_retain_queued_trash(db, uid) {
        Ok(retain) => !retain,
        Err(error) => {
            warn!(%uid, %error, "queued-trash lookup failed; retaining unlinked state");
            false
        }
    }
}

struct AcceptedShares {
    uids: Vec<NodeUid>,
    /// `None` means duplicate listing rows disagreed on provenance.
    share_ids: HashMap<NodeUid, Option<ShareId>>,
}

fn accepted_share_provenance(items: Vec<SharedWithMeItem>) -> AcceptedShares {
    let mut uids = Vec::new();
    let mut share_ids = HashMap::new();
    for item in items {
        match share_ids.entry(item.uid.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                uids.push(item.uid);
                entry.insert(Some(item.share_id));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().as_ref() != Some(&item.share_id) {
                    entry.insert(None);
                }
            }
        }
    }
    AcceptedShares { uids, share_ids }
}

fn shared_root_access(nodes: &[&Node], expected_share: Option<&ShareId>) -> Access {
    let Some(expected_share) = expected_share else {
        return Access::Viewer;
    };
    let mut observed = None;
    for node in nodes {
        let Some(membership) = node.membership.as_ref() else {
            return Access::Viewer;
        };
        if &membership.share_id != expected_share {
            return Access::Viewer;
        }
        let Some(role) = membership.role_exact() else {
            return Access::Viewer;
        };
        let access = access_for(Some(role), Access::Viewer, true);
        if observed.is_some_and(|prior| prior != access) {
            return Access::Viewer;
        }
        observed = Some(access);
    }
    observed.unwrap_or(Access::Viewer)
}

fn prepare_shared_roots(
    accepted: &AcceptedShares,
    mut materialized: Vec<Node>,
    parent: &NodeUid,
) -> Vec<PublishedSharedRoot> {
    materialized.retain(|node| accepted.share_ids.contains_key(&node.uid));
    materialized.sort_by(|a, b| {
        a.uid
            .to_string()
            .cmp(&b.uid.to_string())
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut published = Vec::new();
    let mut start = 0usize;
    while start < materialized.len() {
        let uid = materialized[start].uid.clone();
        let mut end = start + 1;
        while end < materialized.len() && materialized[end].uid == uid {
            end += 1;
        }
        let access = shared_root_access(
            &materialized[start..end].iter().collect::<Vec<_>>(),
            accepted.share_ids.get(&uid).and_then(Option::as_ref),
        );
        let mut node = materialized[start].clone();
        node.parent_uid = Some(parent.clone());
        node.trashed = false;
        published.push(PublishedSharedRoot { node, access });
        start = end;
    }
    let mut named: Vec<Node> = published.iter().map(|root| root.node.clone()).collect();
    disambiguate_shared_names(&mut named);
    let names: HashMap<NodeUid, String> = named
        .into_iter()
        .map(|node| (node.uid, node.name))
        .collect();
    for root in &mut published {
        if let Some(name) = names.get(&root.node.uid) {
            root.node.name.clone_from(name);
        }
    }
    published
}

impl Core {
    /// Record that this daemon just changed `uid` on the remote, so the event
    /// feed's echo of that change is recognised instead of re-applied. Called
    /// from the drain once the remote has actually accepted the change and the
    /// tree has been brought level with it. See [`Core::self_changes`].
    pub(crate) fn note_self_change(&self, uid: &NodeUid) {
        note_self_change(&mut self.self_changes.lock(), uid, now_millis());
    }

    /// Claim one outstanding echo for `uid`, reporting whether this event is one
    /// this daemon caused. Consuming rather than merely testing is what bounds
    /// the suppression: the next event for the node applies normally.
    fn take_self_change(&self, uid: &NodeUid) -> bool {
        take_self_change(&mut self.self_changes.lock(), uid, now_millis())
    }

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
        // `hydrate_pending` runs first and restores queued-trash tombstones.
        // Their node rows remain persisted as drain-time access authority, but
        // must not reappear in the mounted namespace after a restart.
        let hidden = self.hidden.lock().clone();
        let mut st = self.state();

        // Pass 1: assign a stable inode to every uid (root is already mapped).
        for sn in &stored {
            if hidden.contains(&sn.node.uid) || st.by_uid.contains_key(&sn.node.uid) {
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
            if hidden.contains(&node.uid) {
                continue;
            }
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
                    access: pdfs_core::Access::Unknown,
                    lookup_count: 1,
                    open_count: 0,
                    unlinked: false,
                },
            );
        }
        st.hydrate_access();

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
            if op.kind == OP_TRASH {
                self.hidden.lock().insert(uid.clone());
            }
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

    /// Re-attach staged blobs that no queued op refers to.
    ///
    /// `staging/` holds the only copy of writes the kernel has already been
    /// told succeeded. Three release-path failures put bytes there and return
    /// `EIO` without leaving a `pending_op` row behind — an access change
    /// mid-release, a write overlapping an undrained incomplete edit, a create
    /// that drained out from under its own handle. Until now nothing ever
    /// walked the directory again: `hydrate_pending` reads the op table and
    /// [`recover_fsynced_writes`](Self::recover_fsynced_writes) reads
    /// `recovery/`, so an orphaned staged blob was invisible to the daemon and
    /// to the user alike, and stayed that way until someone found the file.
    ///
    /// Runs once at mount, after `hydrate_pending`, so `pending` already
    /// describes what the queue owns. An orphan whose sidecar names a real node
    /// is queued as an ordinary revision — the condition that made it fail is
    /// gone by definition, since the op it collided with has since drained.
    /// Anything that cannot be addressed (no sidecar, an unparseable uid, a
    /// `local~` placeholder whose create is gone) is left in place and named in
    /// the log: unexplained bytes are never evidence that user data is
    /// disposable.
    fn reconcile_staging(&self) {
        let staged = self.cache.staged_writes();
        if staged.is_empty() {
            return;
        }
        let claimed = match self.db.op_blob_paths() {
            Ok(paths) => paths,
            Err(e) => {
                error!(error = %e, "cannot read queued blob paths; skipping staging reconcile");
                return;
            }
        };
        let mut queued = 0usize;
        let mut stranded = 0usize;
        for (blob, meta) in staged {
            if claimed.contains(&blob.to_string_lossy().into_owned()) {
                continue;
            }
            let addressable = meta.as_ref().and_then(|m| parse_node_uid(&m.uid));
            let Some((meta, uid)) = meta.zip(addressable).filter(|(_, uid)| !is_local_uid(uid))
            else {
                stranded += 1;
                warn!(blob = %blob.display(),
                      "staged bytes belong to no queued upload and name no node; kept for recovery");
                continue;
            };
            if self.pending.lock().contains_key(&uid) {
                // A newer write for the same node is queued and carries its own
                // blob. These bytes are older; keeping them is right, uploading
                // them over the newer ones is not.
                stranded += 1;
                warn!(%uid, blob = %blob.display(),
                      "staged bytes are superseded by a queued write; kept for recovery");
                continue;
            }
            let op = PendingOp {
                id: 0,
                kind: OP_REVISION.to_string(),
                uid: uid.to_string(),
                parent_uid: None,
                name: None,
                blob_path: Some(blob.to_string_lossy().into_owned()),
                meta_json: Some(serde_json::to_string(&meta).unwrap_or_default()),
                created_at: now_millis(),
                attempts: 0,
                last_error: None,
                next_attempt_at: now_millis(),
            };
            match self.db.enqueue_op(&op) {
                Ok((_, superseded)) => {
                    if let Some(old) = superseded {
                        self.cache.discard_staged(Path::new(&old));
                    }
                    self.pending.lock().insert(
                        uid.clone(),
                        PendingRevision {
                            path: blob.clone(),
                            meta,
                        },
                    );
                    queued += 1;
                }
                Err(e) => {
                    stranded += 1;
                    error!(%uid, blob = %blob.display(), error = %e,
                           "cannot queue an orphaned staged write; bytes kept");
                }
            }
        }
        if queued > 0 || stranded > 0 {
            info!(queued, stranded, "reconciled orphaned staged writes");
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
            if !self.shutdown.sleep(delay) {
                return;
            }
            match self.rt.block_on(self.client.get_my_files_folder()) {
                Ok(root) => {
                    {
                        let mut st = self.state();
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
                    self.repair_primary_share_id(&root.uid);
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
        self.state().children.contains_key(&ino)
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

    /// Read the inputs for virtual-root publication without changing SQLite.
    ///
    /// The caller validates the listing snapshot after these reads. Keeping all
    /// writes out of this phase prevents an invalidated/repopulated root listing
    /// from changing the synthetic node's persisted visibility or FTS ancestry.
    fn prepare_virtual_root(&self, real_names: &HashSet<String>) -> Result<VirtualRootPlan, Errno> {
        let pinned = self.db.state_str(SHARED_WITH_ME_NAME).map_err(|error| {
            error!(%error, "reading shared-root display name failed");
            Errno::EIO
        })?;
        let (name, visible) = virtual_root_name(real_names, pinned.as_deref());

        let uid = shared_with_me_uid();
        let previous_node = self.db.node_by_uid(&uid.to_string()).map_err(|error| {
            error!(%error, "reading synthetic shared root failed");
            Errno::EIO
        })?;
        let mut node = previous_node.unwrap_or_else(|| {
            virtual_node(self.primary_root_uid.clone(), name.clone(), now_secs())
        });
        node.parent_uid = Some(self.primary_root_uid.clone());
        node.name = name;
        node.kind = NodeKind::Folder;
        // A later real-name collision suppresses both the dentry and its search
        // hit. The row and pinned name remain, and clearing the collision
        // restores both by writing `trashed = false` again.
        node.trashed = !visible;
        node.membership = None;
        Ok(VirtualRootPlan { node, visible })
    }

    /// Reconcile the synthetic dentry against an already-known primary-root
    /// listing. The node remains persisted and pinned even while a real folder
    /// of the same name temporarily suppresses its dentry.
    fn reconcile_virtual_root_dentry(
        &self,
        parent_ino: u64,
        snapshot: RootListingSnapshot,
    ) -> Result<(), Errno> {
        let _publication = self.shared_publication.lock();
        let plan = self.prepare_virtual_root(&snapshot.real_names())?;
        let mut st = self.state();
        // State methods already establish the state -> DB lock order. No path
        // holds the DB connection while acquiring state, including shared event
        // publication, so keeping state locked across this one transaction
        // closes the crash window without introducing an inverse order.
        publish_virtual_root_in_listing(&self.db, &mut st, parent_ino, &snapshot, plan)?;
        drop(st);
        self.flush_access_changes();
        Ok(())
    }

    fn resident_root_listing_snapshot(&self, ino: u64) -> Result<RootListingSnapshot, Errno> {
        RootListingSnapshot::capture(&self.state(), ino).ok_or(Errno::EAGAIN)
    }

    fn shared_folder_freshness_key(uid: &NodeUid) -> String {
        format!("{SHARED_FOLDER_SYNCED_PREFIX}{uid}")
    }

    fn shared_listing_stale(&self, key: &str) -> bool {
        if self
            .shared_refresh_deadlines
            .lock()
            .is_fresh(key, Instant::now())
        {
            return false;
        }
        self.listing_stale(key, SHARED_LISTING_TTL)
    }

    fn mark_shared_refresh_success(&self, key: &str) {
        self.shared_refresh_deadlines
            .lock()
            .mark(key, Instant::now(), SHARED_LISTING_TTL);
        if let Err(error) = self.db.set_state_i64(key, now_millis()) {
            warn!(key, %error, "persisting shared-list freshness failed");
        }
    }

    fn invalidate_shared_refreshes(&self) {
        self.shared_generation.fetch_add(1, Ordering::SeqCst);
        self.shared_refresh_deadlines.lock().clear();
    }

    fn is_own_or_virtual(&self, uid: &NodeUid) -> bool {
        is_own_or_virtual_uid(uid, &self.primary_root_uid.volume_id)
    }

    fn ensure_shared_children(&self, ino: u64) -> Result<(), Errno> {
        let online = self.online.load(Ordering::Relaxed);
        let resident = self.state().children.contains_key(&ino);
        match shared_listing_plan(
            resident,
            online,
            self.shared_listing_stale(SHARED_WITH_ME_SYNCED_MS),
        ) {
            SharedListingPlan::Resident => return Ok(()),
            SharedListingPlan::Persisted => {
                // A completed snapshot remains useful offline even after an
                // event expired its freshness stamp.
                let nodes = self
                    .db
                    .visible_children(&shared_with_me_uid())
                    .map_err(|error| {
                        error!(%error, "loading persisted shared roots failed");
                        Errno::EIO
                    })?;
                let mut st = self.state();
                if st.children.contains_key(&ino) {
                    return Ok(());
                }
                let children = nodes
                    .into_iter()
                    .map(|node| {
                        let access = st
                            .share_access
                            .get(&node.uid)
                            .copied()
                            .unwrap_or(Access::Viewer);
                        st.intern_published_share_root(ino, node, access)
                    })
                    .collect();
                st.children.insert(ino, children);
                drop(st);
                self.flush_access_changes();
                return Ok(());
            }
            SharedListingPlan::Refresh => {}
        }

        let generation = self.shared_generation.load(Ordering::SeqCst);
        let accepted = accepted_share_provenance(
            self.rt
                .block_on(self.client.enumerate_shared_with_me())
                .map_err(|error| {
                    error!(%error, "enumerating shared roots failed");
                    Errno::EIO
                })?,
        );
        let nodes = if accepted.uids.is_empty() {
            Vec::new()
        } else {
            self.rt
                .block_on(self.client.enumerate_nodes_light(&accepted.uids))
                .map_err(|error| {
                    error!(%error, "materializing shared roots failed");
                    Errno::EIO
                })?
        };
        let virtual_uid = shared_with_me_uid();
        let published = prepare_shared_roots(&accepted, nodes, &virtual_uid);

        let _publication = self.shared_publication.lock();
        if !refresh_generation_is_current(generation, self.shared_generation.load(Ordering::SeqCst))
        {
            return Err(Errno::EAGAIN);
        }
        let removed = match self
            .db
            .publish_shared_roots(&virtual_uid, &accepted.uids, &published)
        {
            Ok(removed) => removed,
            Err(error) => {
                error!(%error, "publishing shared-root listing failed");
                let mut st = self.state();
                for uid in &accepted.uids {
                    st.downgrade_shared_subtree(uid);
                }
                drop(st);
                self.flush_access_changes();
                return Err(Errno::EIO);
            }
        };
        let snapshot = self.db.visible_children(&virtual_uid).map_err(|error| {
            error!(%error, "loading published shared-root listing failed");
            Errno::EIO
        })?;
        let published_access: HashMap<NodeUid, Access> = published
            .iter()
            .map(|root| (root.node.uid.clone(), root.access))
            .collect();
        let mut st = self.state();
        for uid in &removed {
            st.hide_shared_root(uid);
        }
        let mut children = Vec::with_capacity(snapshot.len());
        for node in snapshot {
            let uid = node.uid.clone();
            let access = published_access
                .get(&uid)
                .copied()
                .unwrap_or(Access::Viewer);
            let child = st.intern_published_share_root(ino, node, access);
            children.push(child);
        }
        st.children.insert(ino, children);
        drop(st);
        self.flush_access_changes();
        self.mark_shared_refresh_success(SHARED_WITH_ME_SYNCED_MS);
        if let Some(notifier) = self.notifier.get() {
            let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
        }
        Ok(())
    }

    /// Enumerate `ino`'s children from the remote and cache them. No-op if the
    /// directory has already been listed. Network I/O happens without the lock
    /// held so concurrent metadata reads aren't blocked behind a fetch.
    fn ensure_children(&self, ino: u64) -> Result<(), Errno> {
        let (folder_uid, cached) = {
            let st = self.state();
            match st.entries.get(&ino) {
                Some(e) => (e.uid.clone(), st.children.contains_key(&ino)),
                None => return Err(Errno::ENOENT),
            }
        };
        if is_virtual_uid(&folder_uid) {
            return self.ensure_shared_children(ino);
        }

        let primary_root =
            is_primary_root_listing(self.primary, &folder_uid, &self.primary_root_uid);
        let foreign = !self.is_own_or_virtual(&folder_uid) && !is_local_uid(&folder_uid);
        let foreign_key = foreign.then(|| Self::shared_folder_freshness_key(&folder_uid));
        let refresh_foreign = foreign_key.as_deref().is_some_and(|key| {
            listing_needs_refresh(
                self.online.load(Ordering::Relaxed),
                self.shared_listing_stale(key),
            )
        });
        if cached && !refresh_foreign {
            if primary_root {
                self.reconcile_virtual_root_dentry(ino, self.resident_root_listing_snapshot(ino)?)?;
            }
            return Ok(());
        }
        // Offline fast path: a folder the DB still records as fully enumerated
        // can be rebuilt from disk without hitting the API, even if its listing
        // was trimmed from the hot cache mid-run.
        let cached_nodes = if refresh_foreign {
            Ok(None)
        } else {
            self.db.children_if_listed(&folder_uid)
        };
        match cached_nodes {
            Ok(Some(mut nodes)) => {
                if primary_root {
                    nodes.retain(|node| !is_virtual_uid(&node.uid));
                }
                // Before the lock: a DB row carries the size the server last
                // sealed, which a queued write is ahead of (B11).
                self.stamp_pending_sizes(&mut nodes);
                let hidden = self.hidden.lock().clone();
                let mut st = self.state();
                if st.children.contains_key(&ino) {
                    return Ok(());
                }
                let mut child_inos = Vec::with_capacity(nodes.len());
                let mut needs_size = Vec::new();
                for node in nodes {
                    if !node_visible(&node, &folder_uid, &hidden) {
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
                let root_snapshot =
                    primary_root.then(|| RootListingSnapshot::capture(&st, ino).unwrap());
                drop(st);
                self.flush_access_changes();
                if let Some(snapshot) = root_snapshot {
                    self.reconcile_virtual_root_dentry(ino, snapshot)?;
                }
                // Rows persisted from a cheap enumeration whose upgrade never
                // ran (a restart in between, say) still owe their real sizes.
                self.spawn_size_upgrade(ino, needs_size);
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => warn!(%folder_uid, error = %e, "db children_if_listed failed"),
        }

        let refresh_generation =
            refresh_foreign.then(|| self.shared_generation.load(Ordering::SeqCst));
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
        let hidden = self.hidden.lock().clone();
        let mut filtered_nodes: Vec<Node> = nodes
            .into_iter()
            .filter(|node| node_visible(node, &folder_uid, &hidden))
            .collect();
        if foreign {
            for node in &mut filtered_nodes {
                node.parent_uid = Some(folder_uid.clone());
            }
            let _publication = self.shared_publication.lock();
            if !refresh_generation_is_current(
                refresh_generation.expect("foreign refresh has a generation"),
                self.shared_generation.load(Ordering::SeqCst),
            ) {
                return Err(Errno::EAGAIN);
            }
            let removed = self
                .db
                .publish_foreign_children(&folder_uid, &uids, &filtered_nodes)
                .map_err(|error| {
                    error!(%folder_uid, %error, "publishing foreign-folder listing failed");
                    Errno::EIO
                })?;
            let mut snapshot = self.db.visible_children(&folder_uid).map_err(|error| {
                error!(%folder_uid, %error, "loading published foreign-folder listing failed");
                Errno::EIO
            })?;
            self.stamp_pending_sizes(&mut snapshot);
            snapshot.retain(|node| node_visible(node, &folder_uid, &hidden));
            let needs_size: Vec<NodeUid> = snapshot
                .iter()
                .filter(|node| {
                    matches!(
                        &node.kind,
                        NodeKind::File {
                            claimed_size: None,
                            ..
                        }
                    )
                })
                .map(|node| node.uid.clone())
                .collect();
            let mut st = self.state();
            for uid in &removed {
                st.hide_foreign_subtree(uid);
            }
            let children = snapshot
                .into_iter()
                .map(|node| st.intern_from_db(ino, node))
                .collect();
            st.children.insert(ino, children);
            drop(st);
            self.flush_access_changes();
            self.mark_shared_refresh_success(
                foreign_key
                    .as_deref()
                    .expect("foreign refresh has a freshness key"),
            );
            self.spawn_size_upgrade(ino, needs_size);
            return Ok(());
        }

        let mut st = self.state();
        // Lost the race? Another thread already populated it.
        if st.children.contains_key(&ino) {
            return Ok(());
        }
        let mut child_inos = Vec::with_capacity(filtered_nodes.len());
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
        let root_snapshot = primary_root.then(|| RootListingSnapshot::capture(&st, ino).unwrap());
        // Record the listing as complete so a later restart (or a trimmed hot
        // cache) can rebuild it from the DB without the API.
        if let Err(e) = self.db.set_listed(&folder_uid, true) {
            warn!(%folder_uid, error = %e, "db set_listed(true) failed");
        }
        drop(st);
        self.flush_access_changes();
        if let Some(snapshot) = root_snapshot {
            self.reconcile_virtual_root_dentry(ino, snapshot)?;
        }
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
    fn upgrade_sizes_for_parent(
        &self,
        ino: u64,
        uid: &NodeUid,
        parent: u64,
    ) -> Option<Arc<SizeUpgrade>> {
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
            let st = self.state();
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
        // Returned, not awaited here: the caller is a `getattr` that must not
        // answer with a provisional size (bugs.md B14), and it parks its reply
        // on this batch rather than holding a thread until it lands.
        self.upgrade_sizes(key, missing)
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
        self.upgrade_sizes(folder_ino, uids);
    }

    /// Start (or join) the size upgrade for `key`, returning the batch a caller
    /// can wait on — `None` when there was nothing to upgrade.
    ///
    /// Single-flight per `key`. The fetch runs on its own thread rather than on
    /// the [`Workers`] pool: callers wait on `Lane::Meta`, so a batch queued onto
    /// that lane could have a wide enough `ls -l` fill it with threads waiting
    /// for a job that can never be scheduled. `Lane::Transfer` would swap that
    /// for starvation behind bulk reads. One short-lived thread per folder,
    /// bounded by the single-flight, avoids both.
    fn upgrade_sizes(&self, key: u64, uids: Vec<NodeUid>) -> Option<Arc<SizeUpgrade>> {
        if uids.is_empty() {
            return None;
        }
        let slot = {
            let mut in_flight = self.size_upgrades.lock();
            match in_flight.get(&key) {
                // Someone else is already fetching this folder; their batch
                // covers us, so just wait on it.
                Some(existing) => existing.clone(),
                // Nothing bounded how many of these could be in flight: the
                // single-flight is per folder, so a recursive listing across
                // hundreds of cold folders started a thread and a batch for
                // each (audit F4). Past the cap, answer with the provisional
                // size — the same fallback the deadline already takes, and the
                // upgrade is retried by the next `stat` once a slot frees.
                None if in_flight.len() >= MAX_SIZE_UPGRADES => return None,
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
        Some(slot)
    }

    /// Hold `respond` back until `ino` has a real size, `slot`'s batch ends, or
    /// [`SizeUpgrade::WAIT`] elapses — then run it, on the queue's thread.
    ///
    /// The caller returns immediately. That is the whole point: the thread it
    /// was running on is a FUSE dispatch thread or a [`Workers`] one, and both
    /// are needed by everything else on the mount (audit F4).
    fn await_size(
        &self,
        ino: u64,
        slot: Arc<SizeUpgrade>,
        respond: impl FnOnce() + Send + 'static,
    ) {
        let core = self.clone();
        let waiter = SizeWaiter {
            slot,
            resolved: Box::new(move || core.size_is_real(ino)),
            deadline: Instant::now() + SizeUpgrade::WAIT,
            respond: Box::new(respond),
        };
        if !self.size_waiters.park(waiter) {
            return;
        }
        let queue = self.size_waiters.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("pdfs-size-waiters".into())
            .spawn(move || queue.serve())
        {
            // Without a thread nothing would ever answer these. A provisional
            // size is wrong; a `stat` that never returns wedges the caller.
            warn!(%error, "no thread for parked size waiters; answering provisionally");
            self.size_waiters.answer_all();
        }
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
        let st = self.state();
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
            let mut st = core.state();
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
        let st = self.state();
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
            let st = self.state();
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

    /// Resolve a wire-format uid only when one of this daemon's locations
    /// proves that it owns the node.
    ///
    /// The primary and on-demand mounts prove residency through
    /// [`StateRegistry`]. Mirror locations have no inode space, so their root
    /// and last-synced descendants are resolved through `sync_folder` and
    /// `sync_entry` instead.
    pub(crate) fn resolve_anywhere(&self, uid: &str) -> CoreResult<NodeUid> {
        resolve_anywhere_with(
            uid,
            |uid| self.states.owns_visible_uid(uid),
            |uid| self.db.mirror_contains_uid(&uid.to_string()),
        )
    }

    fn source_parent_uid(&self, ino: u64, rel: &Path) -> CoreResult<NodeUid> {
        let state = self.state();
        let entry = state
            .entries
            .get(&ino)
            .ok_or_else(|| CoreError::not_found(format!("could not resolve {}", rel.display())))?;
        entry
            .node
            .parent_uid
            .clone()
            .or_else(|| {
                state
                    .entries
                    .get(&entry.parent)
                    .map(|parent| parent.uid.clone())
            })
            .ok_or_else(|| CoreError::invalid("the mount root cannot be mutated"))
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
        // Closing an untouched writable handle is not a mutation. It remains
        // valid after a downgrade and only owes cleanup of its empty scratch.
        if !h.dirty {
            self.cache.clear_scratch_durable(&h.path);
            let _ = std::fs::remove_file(&h.path);
            return Ok(());
        }
        preserve_on_access_denied(self.require_uid_writable(&h.uid), h.dirty, || {
            let meta = self.recovery_meta(h);
            self.stage_orphaned_write(
                &h.uid,
                h.ino,
                &h.path,
                &meta,
                "access changed before the write could be queued",
            );
        })?;
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
        // Materialize as much of the full content as is free to materialize. A
        // complete blob is uploadable without the network and lets a later write
        // to the same file supersede this one without any reconstruction at all.
        //
        // What is deliberately *not* done here is fetching the untouched ranges
        // from the remote: that is a download of everything the edit did not
        // touch, and this runs on the FUSE dispatch loop, so closing a large
        // file after a small edit stopped every other request on the mount for
        // the length of it (audit F1). An incomplete blob is first-class —
        // `read_pending` resolves its gaps against the base, and `drain_revision`
        // fills them on the drain thread — so the network half of the fill
        // belongs there, and only there.
        //
        // A node that exists only locally has no remote base to fill from: its
        // untouched ranges live in the blob queued by an earlier write, which
        // `merge_over_pending` folds in below.
        if let Err(e) = h.file.set_len(h.len) {
            error!(uid = %h.uid, error = %e, "resize scratch file failed");
            return Err(Errno::EIO);
        }
        let mut written = h.written.clone();
        if !(is_local_uid(&h.uid) && h.base_size > 0) {
            // A local node has no remote base, so nothing of it can be fetched
            // or cached: passing zero says so, and leaves the fill doing only
            // the part that still applies — claiming the zeroed tail.
            let base_size = if is_local_uid(&h.uid) { 0 } else { h.base_size };
            self.fill_gaps_cached(
                &h.uid,
                &h.file,
                h.len,
                h.base_mtime,
                base_size,
                &mut written,
            );
        }
        let authored: Vec<(u64, u64)> = written
            .segments(0, h.len)
            .into_iter()
            .filter(|&(_, _, authored)| authored)
            .map(|(s, e, _)| (s, e))
            .collect();
        let meta = StagedWrite {
            uid: h.uid.to_string(),
            len: h.len,
            base_size: h.base_size,
            base_mtime: h.base_mtime,
            complete: authored == [(0, h.len)],
            authored,
            based_on: self.remote_baseline(
                &h.uid,
                h.base_mtime,
                h.base_size,
                h.base_revision_id.clone(),
            ),
        };
        let complete = meta.complete;
        self.enqueue_staged_write(&h.uid, h.ino, &h.path, meta)?;
        debug!(uid = %h.uid, len = h.len, complete, "queued revision upload");
        Ok(())
    }

    /// Describe the locally authored ranges without gap-filling them.
    ///
    /// Used when a write was accepted by the kernel but a queue-time access
    /// check now denies it. Building this metadata is side-effect free, so the
    /// permission guard still precedes cache, database, and tree mutation.
    fn recovery_meta(&self, h: &WriteHandle) -> StagedWrite {
        let authored: Vec<(u64, u64)> = h
            .written
            .segments(0, h.len)
            .into_iter()
            .filter(|&(_, _, authored)| authored)
            .map(|(start, end, _)| (start, end))
            .collect();
        StagedWrite {
            uid: h.uid.to_string(),
            len: h.len,
            base_size: h.base_size,
            base_mtime: h.base_mtime,
            complete: authored == [(0, h.len)],
            authored,
            based_on: self.remote_baseline(
                &h.uid,
                h.base_mtime,
                h.base_size,
                h.base_revision_id.clone(),
            ),
        }
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
    fn remote_baseline(
        &self,
        uid: &NodeUid,
        base_mtime: i64,
        base_size: u64,
        base_revision_id: Option<String>,
    ) -> Option<Baseline> {
        if is_local_uid(uid) {
            return None;
        }
        match self.pending.lock().get(uid) {
            Some(p) => p.meta.based_on.clone(),
            None => Some(Baseline {
                mtime: base_mtime,
                size: base_size,
                hash: None,
                revision_id: base_revision_id,
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
        mut meta: StagedWrite,
    ) -> Result<(), Errno> {
        preserve_on_access_denied(self.require_uid_writable(uid), true, || {
            self.stage_orphaned_write(
                uid,
                ino,
                src,
                &meta,
                "access changed before the staged write could be queued",
            );
        })?;
        // An incomplete blob's gaps refer to the *remote* base. If an earlier
        // write to this file is still queued, the remote no longer holds that
        // base — the previous staged blob does — so taking those gaps at face
        // value would fill them from the wrong revision. Fold the blob being
        // superseded into this one instead: its authored bytes are a local copy
        // of exactly the ranges this write did not touch, and what neither wrote
        // is still the remote content that the earlier write was based on, which
        // is why the baseline is inherited along with them.
        let previous = self.pending.lock().get(uid).cloned();
        if !meta.complete
            && let Some(previous) = previous
            && let Err(error) = merge_over_pending(&mut meta, src, &previous)
        {
            // Without the earlier bytes the gaps cannot be resolved against
            // anything, so this is where the write stops being uploadable.
            // Keep it recoverable rather than corrupting the file.
            warn!(%uid, %error, "folding in the undrained edit failed");
            self.stage_orphaned_write(
                uid,
                ino,
                src,
                &meta,
                "write overlaps an undrained incomplete edit",
            );
            return Err(Errno::EIO);
        }
        let path = self.cache.stage_write(&meta, src).map_err(|e| {
            error!(%uid, error = %e, "staging write failed");
            // The scratch file is still the only copy of these bytes, and the
            // caller is releasing the handle it belongs to — without a
            // durability sidecar `discard_unmarked_scratch` deletes it at the
            // next mount, the one place the "never delete on a failure path"
            // invariant would break. Mark it first, whatever went wrong.
            if let Err(mark) = self.cache.mark_scratch_durable(src, &meta) {
                error!(%uid, error = %mark,
                       "could not mark an unstageable write durable; bytes at risk");
            }
            if e.is_disk_full() {
                // Free cached blobs so the retry has somewhere to land, and tell
                // the caller what is actually wrong.
                self.cache.emergency_evict();
                Errno::ENOSPC
            } else {
                Errno::EIO
            }
        })?;
        let meta_json = serde_json::to_string(&meta).unwrap_or_default();
        // Whatever this write supersedes, it supersedes now — including an
        // upload already on the wire, whose bytes describe a revision the row
        // written just below replaces. Signalled before the enqueue so the
        // in-flight reader stops at its next block rather than after however
        // much of the file is left.
        self.cancel_upload(uid);
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
                next_attempt_at: now_millis() + self.revision_debounce(uid).as_millis() as i64,
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
        let updated = {
            let mut st = self.state();
            st.record_pending_write(ino, len, now)
        };
        // The only record that the file is as long as the caller was told. If it
        // cannot be written the write did not fully take, and saying so beats
        // acknowledging a `close(2)` and then serving the old size — the bytes
        // themselves are already staged and queued either way. Written out here
        // rather than inside `State` so the commit does not hold the inode lock.
        let recorded = match updated {
            Some(node) => self.db.upsert_node(&node),
            None => Ok(()),
        };
        // Cached blobs and open readers describe the superseded revision. Reads
        // come from the staged blob until the op drains, so just drop them.
        // Done even when the row above failed: the staged blob is the file's
        // content now either way, and leaving a stale one cached would serve it.
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.wake_drain();
        if let Err(e) = recorded {
            error!(%uid, error = %e, "recording a queued write's size failed");
            return Err(if e.is_disk_full() {
                Errno::ENOSPC
            } else {
                Errno::EIO
            });
        }
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
        let (uid, base_mtime, base_size, base_revision_id) = {
            let st = self.state();
            match st.entries.get(&ino) {
                Some(e) if e.node.is_file() => (
                    e.uid.clone(),
                    e.node.modification_time,
                    node_size(&e.node),
                    node_revision_id(&e.node),
                ),
                Some(_) => return Err(Errno::EISDIR),
                None => return Err(Errno::ENOENT),
            }
        };
        self.require_uid_writable(&uid)?;
        if size == base_size {
            return Ok(());
        }
        // A close immediately followed by truncate is common (`write`, close,
        // `truncate(2)`) and usually beats the write-back debounce. In that
        // case the remote is *not* our base: the complete staged revision is.
        // Compose from those local bytes so the truncate can supersede the
        // queued upload without fetching stale remote content or returning EIO.
        let pending_base = self.pending.lock().get(&uid).cloned();
        if pending_base
            .as_ref()
            .is_some_and(|pending| !pending.meta.complete)
        {
            // An incomplete pending blob still has holes referring to the
            // remote revision. Stacking another incomplete transform on it is
            // not representable safely; preserve the existing refusal.
            return Err(Errno::EIO);
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
        if let Some(pending) = &pending_base
            && let Err(e) = copy_pending_for_truncate(pending, &file)
        {
            error!(%uid, source = %pending.path.display(), error = %e,
                "copy pending revision for truncate failed");
            let _ = std::fs::remove_file(&path);
            return Err(Errno::EIO);
        }
        file.set_len(size).map_err(|e| {
            error!(%uid, error = %e, "resize scratch file for truncate failed");
            let _ = std::fs::remove_file(&path);
            Errno::EIO
        })?;
        let meta = match pending_base {
            Some(pending) => StagedWrite {
                uid: uid.to_string(),
                len: size,
                base_size: pending.meta.len,
                base_mtime: pending.meta.base_mtime,
                authored: if size == 0 {
                    Vec::new()
                } else {
                    vec![(0, size)]
                },
                complete: true,
                // Both local revisions descend from the same last-observed
                // remote revision. Never replace this with the optimistic
                // mtime/size currently published by State.
                based_on: pending.meta.based_on,
            },
            None => StagedWrite {
                uid: uid.to_string(),
                len: size,
                base_size,
                base_mtime,
                authored,
                complete,
                based_on: self.remote_baseline(&uid, base_mtime, base_size, base_revision_id),
            },
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
        hold: bool,
    ) -> Result<Node, Errno> {
        self.require_uid_writable(parent_uid)?;
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
            // Parked when the name is transient: its bytes ride on this create
            // (via `attach_blob_to_create`) and must not upload until a rename to
            // the finished name un-parks it (docs/BUGS.md B70).
            next_attempt_at: if hold { PARK_UNTIL } else { 0 },
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
        old_parent_uid: &NodeUid,
        new_parent_ino: u64,
        new_parent_uid: &NodeUid,
        new_name: &str,
    ) -> Result<(), Errno> {
        self.require_uid_writable(uid)?;
        self.require_uid_writable(old_parent_uid)?;
        self.require_uid_writable(new_parent_uid)?;
        self.queue_rename_authorized(
            ino,
            uid,
            old_parent_uid,
            new_parent_ino,
            new_parent_uid,
            new_name,
        )
    }

    /// Enqueue a rename whose access was linearized immediately before a
    /// replacement victim was removed. Rechecking here could reject after that
    /// destructive half had already completed.
    fn queue_rename_authorized(
        &self,
        ino: u64,
        uid: &NodeUid,
        old_parent_uid: &NodeUid,
        new_parent_ino: u64,
        new_parent_uid: &NodeUid,
        new_name: &str,
    ) -> Result<(), Errno> {
        let original_parent_uid = match self.db.pending_op_meta(&uid.to_string(), OP_RENAME) {
            Ok(Some(json)) => serde_json::from_str::<RenameMeta>(&json)
                .map(|meta| meta.original_parent_uid)
                .map_err(|error| {
                    error!(%uid, %error, "queued rename authority metadata is invalid");
                    Errno::EIO
                })?,
            Ok(None) => old_parent_uid.to_string(),
            Err(error) => {
                error!(%uid, %error, "reading queued rename authority failed");
                return Err(Errno::EIO);
            }
        };
        let meta_json = serde_json::to_string(&RenameMeta {
            original_parent_uid,
        })
        .map_err(|error| {
            error!(%uid, %error, "serializing queued rename authority failed");
            Errno::EIO
        })?;
        let op = PendingOp {
            id: 0,
            kind: OP_RENAME.to_string(),
            uid: uid.to_string(),
            parent_uid: Some(new_parent_uid.to_string()),
            name: Some(new_name.to_string()),
            blob_path: None,
            meta_json: Some(meta_json),
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
        self.require_uid_writable(uid)?;
        // The bytes below are about to be discarded, and a drain worker may be
        // reading them onto the wire right now. Stop it first: with several
        // workers the trash op this queues can be claimed while that upload is
        // still running, and the two would race to decide whether the file
        // exists.
        self.cancel_upload(uid);
        let (_, blobs) = self
            .db
            .replace_ops_with_trash(&uid.to_string(), name, now_millis())
            .map_err(|e| {
                error!(%uid, error = %e, "queueing trash failed");
                Errno::EIO
            })?;
        for blob in blobs {
            self.cache.discard_staged(Path::new(&blob));
        }
        self.pending.lock().remove(uid);
        self.hidden.lock().insert(uid.clone());
        // Withdrawn from every mount, not just the one the unlink came through:
        // a sync folder maps a remote folder that also exists under My Files, so
        // the same uid can be interned in two inode spaces at once and the other
        // one would go on serving a file the user just trashed
        // (`docs/BUGS.md` B74).
        self.for_each_state(|st| {
            st.unlink_mem(uid);
        });
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.wake_drain();
        debug!(%uid, name, "trashed offline; queued");
        Ok(())
    }

    /// Drop every op queued against a node, and the staged bytes they own.
    fn discard_queued_ops(&self, uid: &NodeUid) -> Result<(), Errno> {
        // Same reason as `queue_trash`: an upload may be reading the very blobs
        // this is about to unlink.
        self.cancel_upload(uid);
        let blobs = self.db.delete_ops_for_uid(&uid.to_string()).map_err(|e| {
            error!(%uid, error = %e, "dropping queued ops failed");
            Errno::EIO
        })?;
        for blob in blobs {
            self.cache.discard_staged(Path::new(&blob));
        }
        self.pending.lock().remove(uid);
        Ok(())
    }

    /// Which staged blob is currently queued for `uid`, if any.
    ///
    /// The path identifies the revision: publication stages a fresh file and
    /// swaps the map entry to point at it, so an unchanged path means the base a
    /// caller sampled earlier is still the base now (bugs.md B32).
    fn pending_blob(&self, uid: &NodeUid) -> Option<PathBuf> {
        self.pending.lock().get(uid).map(|p| p.path.clone())
    }

    /// Nudge the drain worker to re-examine the queue now.
    fn wake_drain(&self) {
        let (lock, cv) = &*self.drain_wake;
        *lock.lock() = true;
        cv.notify_all();
    }

    /// Keep a write we cannot safely queue, so the bytes are recoverable even
    /// though the caller is getting an error. See [`Core::queue_revision`].
    fn stage_orphaned_write(
        &self,
        uid: &NodeUid,
        ino: u64,
        src: &Path,
        meta: &StagedWrite,
        reason: &str,
    ) {
        match self.cache.preserve_write(meta, src) {
            Ok(staged) => {
                self.cache.clear_scratch_durable(src);
                error!(
                    %uid,
                    staged = %staged.display(),
                    %reason,
                    "cannot queue write; bytes kept in staging"
                );
                let name = {
                    let st = self.state();
                    st.entries
                        .get(&ino)
                        .map(|e| e.node.name.clone())
                        .unwrap_or_default()
                };
                self.log_activity(
                    ActivityKind::Upload,
                    &name,
                    format!(
                        "write not queued ({reason}); changes kept at {}",
                        staged.display()
                    ),
                    false,
                );
            }
            Err(e) => {
                let surviving = e.surviving_path.clone();
                let marker = e.marker_path.clone();
                error!(
                    %uid,
                    error = %e,
                    surviving = %surviving.display(),
                    marker = ?marker,
                    %reason,
                    "write rescue was only partially published; bytes retained"
                );
                let name = {
                    let st = self.state();
                    st.entries
                        .get(&ino)
                        .map(|entry| entry.node.name.clone())
                        .unwrap_or_default()
                };
                self.log_activity(
                    ActivityKind::Upload,
                    &name,
                    format!(
                        "write not queued ({reason}); changes retained at {}",
                        surviving.display()
                    ),
                    false,
                );
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

    /// Download a whole file directly into `out`, without retaining its
    /// plaintext in memory. This is the disk-backed twin of
    /// [`Core::download_file_tracked`], and preserves the same transfer
    /// accounting while allowing callers that only need an on-disk file to
    /// keep memory use independent of file size.
    fn download_file_tracked_to<W: Write>(
        &self,
        uid: &NodeUid,
        name: &str,
        total: u64,
        out: W,
    ) -> std::result::Result<W, ProtonError> {
        let guard = self
            .transfers
            .begin(name, uid.to_string(), TransferDirection::Download, total);
        let mut out = CountingWriter::new(out, &guard);
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
            let st = self.state();
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
            let st = self.state();
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
            let st = self.state();
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
        if self.thumbnail_misses.lock().remote_contains(&key, mtime) {
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
                self.thumbnail_misses.lock().remember_remote(key, mtime);
            }
        }
        Ok(bytes)
    }

    /// Unpin the node at `rel`, evicting its cached content. For a folder, also
    /// evicts every descendant's cached blob (the subtree is no longer kept).
    fn unpin(&self, rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let (name, is_folder) = {
            let st = self.state();
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
            let st = self.state();
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
                // My own tree: a role only means something under a share, and
                // this listing never crosses into one.
                role: String::new(),
            })
            .collect())
    }

    /// Every uid that is pinned right now, as one set.
    ///
    /// `cache.is_pinned` is a direct lookup plus an ancestor walk; asking it per
    /// candidate was a thousand walks per keystroke. The recursive expansion is
    /// the same query, run once.
    fn pinned_set(&self) -> HashSet<String> {
        self.db
            .pinned_uids()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// The local locations a search result can be opened through, resolved once
    /// per search rather than once per hit.
    ///
    /// An on-demand folder only answers while its mount is live — without the
    /// mount its `local_path` is an ordinary empty directory. A mirror folder
    /// needs no mount: its files are real local copies the sync loop pushes
    /// back, so it is a valid open target whenever it is configured.
    ///
    /// This used to be a per-hit call that re-read the sync-folder list and ran
    /// an ancestor walk per folder, against a candidate pool of up to a
    /// thousand: the whole cost of a keystroke was in here.
    fn search_roots(&self) -> SearchRoots {
        let live_mounts = self.mounts.lock();
        let folders = self.db.sync_folder_list().unwrap_or_default();
        SearchRoots {
            roots: folders
                .into_iter()
                .filter(|folder| match folder.mode.as_str() {
                    "ondemand" => live_mounts.contains_key(&folder.id),
                    "mirror" => true,
                    _ => false,
                })
                .filter_map(|folder| {
                    let root = self.db.node_path(&folder.remote_uid).ok().flatten()?;
                    Some((root, PathBuf::from(folder.local_path)))
                })
                .collect(),
        }
    }

    /// Full-text search node names against the local SQLite index, mapping each
    /// DB hit to the wire [`SearchHit`] (resolving live pin state from the cache,
    /// which the DB doesn't track). Pure local lookup — never hits the network.
    fn search(&self, query: &str, limit: usize) -> CoreResult<Vec<SearchHit>> {
        let hits = self
            .db
            .search(query, limit)
            .map_err(|e| CoreError::from_api(&e, "search"))?;
        let roots = self.search_roots();
        let pinned = self.pinned_set();
        Ok(hits
            .into_iter()
            .map(|h| SearchHit {
                name: h.node.name.clone(),
                is_dir: h.node.is_folder(),
                size: node_size(&h.node),
                modified: h.node.modification_time,
                pinned: pinned.contains(&h.node.uid.to_string()),
                uid: h.node.uid.to_string(),
                mounted_path: roots.resolve(&h.path),
                path: h.path,
                score: 0,
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
                score: 0,
            })
            .collect())
    }

    /// Search both prompt sources with identical query and per-source limits.
    /// Keeping the composition in the daemon core gives every control surface
    /// the same semantics while preserving the legacy source-specific methods.
    fn search_v2(
        &self,
        query: &str,
        limit: usize,
        filters: &SearchFilters,
    ) -> CoreResult<(Vec<SearchHit>, Vec<LocalHit>)> {
        if query.trim().is_empty() || limit == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        // Candidate generation is intentionally broader than the final result
        // cap: filtering and fuzzy rejection happen below, so limiting at the
        // SQLite boundary to `limit` would recreate the old empty-filter bug.
        let candidate_limit = limit.saturating_mul(10).clamp(100, 1_000);
        let mut drive_hits = if filters.sources.contains(&SearchSource::Drive) {
            let roots = self.search_roots();
            let pinned = self.pinned_set();
            self.db
                .search_candidates(query, candidate_limit)
                .map_err(|e| CoreError::from_api(&e, "search candidates"))?
                .into_iter()
                .filter(|hit| filters.kind.accepts(&hit.node.name, hit.node.is_folder()))
                .filter_map(|hit| {
                    let parent = Path::new(&hit.path)
                        .parent()
                        .map_or_else(String::new, |path| path.display().to_string());
                    let uid = hit.node.uid.to_string();
                    let is_pinned = pinned.contains(&uid);
                    let score = relevance_score(query, &hit.node.name, &parent)?
                        + if is_pinned { 250 } else { 0 };
                    Some(SearchHit {
                        name: hit.node.name.clone(),
                        is_dir: hit.node.is_folder(),
                        size: node_size(&hit.node),
                        modified: hit.node.modification_time,
                        pinned: is_pinned,
                        uid,
                        mounted_path: roots.resolve(&hit.path),
                        path: hit.path,
                        score,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut local_hits = if filters.sources.contains(&SearchSource::Local) {
            self.db
                .search_local_candidates(query, candidate_limit)
                .map_err(|e| CoreError::from_api(&e, "local search candidates"))?
                .into_iter()
                .filter(|hit| filters.kind.accepts(&hit.name, hit.is_dir))
                .filter_map(|hit| {
                    let parent = Path::new(&hit.path)
                        .parent()
                        .map_or_else(String::new, |path| path.display().to_string());
                    Some(LocalHit {
                        score: relevance_score(query, &hit.name, &parent)?,
                        name: hit.name,
                        path: hit.path,
                        is_dir: hit.is_dir,
                        size: hit.size.max(0) as u64,
                        modified: hit.mtime,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        drive_hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.path.cmp(&b.path))
        });
        local_hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.path.cmp(&b.path))
        });
        drive_hits.truncate(limit);
        local_hits.truncate(limit);
        Ok((drive_hits, local_hits))
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
            let st = self.state();
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
        self.fetch_content(&uid, &name, mtime, size)
    }

    /// [`open_file`](Self::open_file) addressed by uid instead of by path.
    ///
    /// A search hit is a metadata-index row, and the index covers every
    /// location this daemon knows — including on-demand sync folders and
    /// mirrors, whose nodes have no path inside the primary mount's tree.
    /// Walking such a hit's path from the mount root answers ENOENT even though
    /// the node is perfectly reachable, so front-ends address hits by uid.
    ///
    /// Metadata comes from the live tree, then the index, then the API — the
    /// last covers a node the index knows but no location holds. Unlike the
    /// mutating by-uid handlers this does not demand a
    /// [`resolve_anywhere`](Self::resolve_anywhere) residency proof: reading
    /// one's own file is what the API authorizes anyway, and requiring
    /// residency is exactly what turned an openable search hit into "no such
    /// file or folder". [`open_shared_file`](Self::open_shared_file) already
    /// works this way. Reserved (local/virtual) uids name no remote node and
    /// are still rejected.
    fn open_file_uid(&self, raw_uid: &str) -> CoreResult<PathBuf> {
        let uid = parse_uid(raw_uid)
            .filter(|uid| !is_local_uid(uid) && !is_virtual_uid(uid))
            .ok_or_else(|| CoreError::invalid(format!("invalid uid: {raw_uid}")))?;
        let live = self
            .state
            .lock()
            .entries
            .values()
            .find(|e| e.uid == uid)
            .map(|e| e.node.clone());
        let node = match live {
            Some(node) => node,
            None => match self
                .db
                .node_by_uid(&uid.to_string())
                .map_err(|e| CoreError::from_api(&e, "load node"))?
            {
                Some(node) => node,
                None => self
                    .rt
                    .block_on(self.client.get_node(&uid))
                    .map_err(|e| CoreError::from_api(&e, "get node"))?
                    .ok_or_else(|| CoreError::not_found("node vanished"))?,
            },
        };
        if !node.is_file() {
            return Err(CoreError::invalid("not a regular file"));
        }
        self.fetch_content(&uid, &node.name, node.modification_time, node_size(&node))
    }

    /// Materialise a file's full content into the content cache, returning its
    /// on-disk path. Serves a fresh cached blob without touching the network.
    fn fetch_content(
        &self,
        uid: &NodeUid,
        name: &str,
        mtime: i64,
        size: u64,
    ) -> CoreResult<PathBuf> {
        if let Some(p) = self.cache.cached_content_path(uid, mtime, size) {
            return Ok(p);
        }

        // Keep the temporary file beside the final cache blob. `store_file`
        // can then adopt it with a hard link and atomic rename, while the SDK
        // streams decrypted blocks to disk instead of building a file-sized
        // `Vec<u8>`. A unique name also keeps concurrent opens of the same node
        // from sharing a partially written staging file.
        static OPEN_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = OPEN_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self.cache.content_path(uid).with_extension(format!(
            "open-{}-{}-{seq}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| CoreError::internal(format!("create download temp: {e}")))?;
        let file = match self.download_file_tracked_to(uid, name, size, file) {
            Ok(file) => file,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(CoreError::from_api(&e, "download"));
            }
        };
        if let Err(e) = file.sync_all() {
            let _ = std::fs::remove_file(&tmp);
            return Err(CoreError::internal(format!("sync download temp: {e}")));
        }
        if let Err(e) = self.cache.store_file(uid, mtime, size, &tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(CoreError::internal(format!("cache store: {e}")));
        }
        let _ = std::fs::remove_file(&tmp);
        Ok(self.cache.content_path(uid))
    }

    /// Drop the cached child listing of `rel`'s parent directory so the next
    /// `ListDir` re-enumerates it from the server. No-op when the parent can't be
    /// resolved (e.g. `rel` is the root). Resolves the parent without holding the
    /// state lock, then invalidates under it.
    fn invalidate_parent_listing(&self, rel: &Path) {
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        if let Ok((pino, _)) = self.resolve_path(parent) {
            self.state().invalidate_listing(pino);
        }
    }

    /// Drop the cached child listing of the folder `parent`, everywhere it is
    /// held — naming the folder by uid rather than by inode.
    ///
    /// [`Core::invalidate_parent_listing`] is the path-based sibling, and it
    /// only reaches *this* mount's inode space. A remote mutation made by the
    /// sync engine needs more than that, for two reasons. The folder it changed
    /// is usually not resident in any inode space at the time — a mirror folder
    /// has no FUSE session at all — so there is no inode to name. And the stale
    /// listing that outlives the pass is not a hot-cache map but the `listed`
    /// flag on the folder's DB row, which survives a daemon restart and is what
    /// a later `ensure_children` trusts.
    ///
    /// So the flag is cleared unconditionally, and the resident inode spaces are
    /// invalidated on top of it. That is what makes a mirror folder switched to
    /// on-demand re-enumerate instead of serving a snapshot that predates the
    /// engine's own uploads — `docs/BUGS.md` B86.
    pub(crate) fn invalidate_children_of(&self, parent: &NodeUid) {
        if let Err(e) = self.db.set_listed(parent, false) {
            warn!(uid = %parent, error = ?e, "could not clear cached listing");
        }
        self.for_each_state(|st| {
            if let Some(&ino) = st.by_uid.get(parent) {
                st.invalidate_listing(ino);
            }
        });
    }

    /// Rename a file or folder to `new_name`. `rel` is mountpoint-relative.
    /// Mirrors the FUSE `rename` write path: rename on the remote, forget the
    /// node so it re-interns under its new name, and drop the parent listing so
    /// the next `ListDir` re-enumerates.
    fn rename(&self, rel: &Path, new_name: &str) -> CoreResult<String> {
        if new_name.is_empty() || new_name.contains('/') {
            return Err(CoreError::invalid(format!("invalid name: {new_name:?}")));
        }
        let (ino, uid) = self.resolve(rel)?;
        let old_parent_uid = self.source_parent_uid(ino, rel)?;
        require_rename_access(
            |authority| self.require_uid_writable(authority),
            &uid,
            &old_parent_uid,
            &old_parent_uid,
        )
        .map_err(|error| self.errno_error(error, "rename access"))?;
        self.rt
            .block_on(self.client.rename_node(&uid, new_name, None))
            .map_err(|e| CoreError::from_api(&e, "rename"))?;
        // Every mount, so a fork showing the same node re-interns it under the
        // new name instead of keeping the old one (`docs/BUGS.md` B74).
        self.for_each_state(|st| {
            st.forget(&uid);
        });
        self.invalidate_parent_listing(rel);
        Ok(new_name.to_string())
    }

    /// Move a file or folder into the folder at `new_parent_rel`. Both paths are
    /// mountpoint-relative. Forgets the node and invalidates both the source and
    /// destination listings so each re-enumerates on next access.
    fn move_to(&self, rel: &Path, new_parent_rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let old_parent_uid = self.source_parent_uid(ino, rel)?;
        let (pino, new_parent_uid) = self
            .resolve_path(new_parent_rel)
            .map_err(|e| self.errno_error(e, "resolve new parent"))?;
        require_rename_access(
            |authority| self.require_uid_writable(authority),
            &uid,
            &old_parent_uid,
            &new_parent_uid,
        )
        .map_err(|error| self.errno_error(error, "move access"))?;
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
        self.state().invalidate_listing(pino);
        Ok(name)
    }

    /// Trash a file or folder. `rel` is mountpoint-relative. Forgets the node,
    /// evicts any cached content, and invalidates the parent listing.
    fn delete(&self, rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let parent_uid = self.source_parent_uid(ino, rel)?;
        require_node_parent_access(
            |authority| self.require_uid_writable(authority),
            &uid,
            &parent_uid,
        )
        .map_err(|error| self.errno_error(error, "trash access"))?;
        self.rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(&uid)))
            .and_then(batch::into_unit)
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
            self.discard_queued_ops(uid)?;
            self.for_each_state(|st| {
                st.forget(uid);
            });
            debug!(%uid, name, "replaced a node whose create was still queued");
            return Ok(());
        }
        self.require_uid_writable(uid)?;
        if let Err(e) = self
            .rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(uid)))
            .and_then(batch::into_unit)
        {
            error!(%uid, name, error = %e, "trashing the node a rename replaces failed");
            self.log_activity(ActivityKind::Trash, name, e.to_string(), false);
            return Err(Errno::EIO);
        }
        if let Err(error) = self.discard_queued_ops(uid) {
            error!(%uid, "remote replacement landed but queued-op cleanup failed");
            return Err(error);
        }
        // The node was trashed on the server; withdraw it from every inode
        // space that had it, not only the mount the rename came through.
        self.for_each_state(|st| {
            st.forget(uid);
        });
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
            .and_then(batch::into_unit)
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
    ///
    /// The refresh always runs *off* the request: a never-fetched trash waits for
    /// it, but only for [`TRASH_FIRST_WAIT`], and then answers with the batches
    /// that have landed so far. Blocking the request on the whole refresh is what
    /// made a large trash unlistable — the refresh outran the front-end's read
    /// timeout, the user asked again, and each attempt left another full refresh
    /// grinding in the daemon (`docs/BUGS.md` B72).
    fn list_trash(&self) -> CoreResult<Vec<DirEntry>> {
        let never_fetched = self.db.state_i64(TRASH_SYNCED_MS).ok().flatten().is_none();
        if never_fetched || self.listing_stale(TRASH_SYNCED_MS, TRASH_TTL) {
            self.spawn_trash_refresh();
        }
        if never_fetched {
            self.await_trash_refresh(TRASH_FIRST_WAIT);
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
                role: String::new(),
            })
            .collect())
    }

    /// Re-fetch the trash listing from the server and persist it.
    ///
    /// Materialized in chunks of [`TRASH_MATERIALIZE_CHUNK`], each persisted as
    /// it lands: decrypting a trashed node costs an S2K unlock, so a trash of a
    /// few thousand nodes takes minutes, and an all-or-nothing write would throw
    /// that away on the first failure and show the user nothing until the very
    /// end. Every batch is a usable listing.
    async fn refresh_trash(&self) -> CoreResult<()> {
        let started = Instant::now();
        let uids = self
            .client
            .enumerate_trash_node_uids()
            .await
            .map_err(|e| CoreError::from_api(&e, "enumerate trash"))?;
        info!(
            count = uids.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "trash refresh: enumerated"
        );

        if uids.is_empty() {
            self.db.trash_replace(&[]).map_err(CoreError::from)?;
        }
        let mut items: Vec<StoredTrash> = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(TRASH_MATERIALIZE_CHUNK) {
            let chunk_started = Instant::now();
            let nodes = self
                .client
                .enumerate_nodes(chunk)
                .await
                .map_err(|e| CoreError::from_api(&e, "enumerate nodes"))?;
            items.extend(nodes.into_iter().map(|node| StoredTrash {
                uid: node.uid.to_string(),
                name: node.name.clone(),
                is_dir: node.is_folder(),
                size: node_size(&node) as i64,
                mtime: node.modification_time,
            }));
            // Cumulative, so the table is always a prefix of the real trash
            // rather than a mix of this refresh and the last one.
            self.db.trash_replace(&items).map_err(CoreError::from)?;
            self.trash_progress.notify_waiters();
            debug!(
                materialized = items.len(),
                total = uids.len(),
                chunk_ms = chunk_started.elapsed().as_millis() as u64,
                "trash refresh: batch"
            );
        }

        let _ = self.db.set_state_i64(TRASH_SYNCED_MS, now_ms());
        info!(
            count = items.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "trash refresh: done"
        );
        Ok(())
    }

    /// Refresh the trash off the request path. At most one refresh at a time —
    /// including the one a first-ever [`Core::list_trash`] waits on, so a burst
    /// of requests against an unfetched trash joins one refresh instead of
    /// starting one apiece.
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
            // After the flag, so a waiter that wakes on this sees it cleared.
            core.trash_progress.notify_waiters();
        });
    }

    /// Wait for the in-flight trash refresh to make progress, for at most
    /// `budget`. Returns early when it finishes; never cancels it — the refresh
    /// is a detached task, so giving up here costs the work nothing.
    fn await_trash_refresh(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        self.rt.block_on(async {
            while self.trash_refreshing.load(Ordering::SeqCst) {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                let notified = self.trash_progress.notified();
                tokio::pin!(notified);
                // Register before re-checking the flag: a refresh that finishes
                // in between must not leave us waiting for a notify that has
                // already fired.
                notified.as_mut().enable();
                if !self.trash_refreshing.load(Ordering::SeqCst) {
                    break;
                }
                if tokio::time::timeout(left, notified).await.is_err() {
                    warn!(
                        budget_s = budget.as_secs(),
                        "trash refresh still running; answering with what has materialized"
                    );
                    break;
                }
            }
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
        self.state().invalidate_listing(ino);
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
        // Streamed per-node outcomes: a uid the server refuses (already restored,
        // gone) leaves the rest of the batch restored, and each node is unhidden
        // as *its* batch lands rather than after the last one — a daemon killed
        // half way through a large restore leaves local state describing what the
        // server actually did. Only a batch that restored nothing is an error the
        // caller can act on.
        let mut restored: Vec<NodeUid> = Vec::with_capacity(parsed.len());
        let mut first_error: Option<ProtonError> = None;
        self.rt
            .block_on(async {
                let mut outcomes = std::pin::pin!(self.client.restore_nodes_streaming(&parsed));
                while let Some(item) = outcomes.next().await {
                    let (uid, outcome) = item?;
                    match outcome {
                        Ok(()) => {
                            self.hidden.lock().remove(&uid);
                            restored.push(uid);
                        }
                        Err(e) => {
                            warn!(%uid, error = %e, "restore failed for a node");
                            first_error.get_or_insert(e);
                        }
                    }
                }
                Ok::<(), ProtonError>(())
            })
            .map_err(|e| CoreError::from_api(&e, "restore"))?;
        if restored.is_empty()
            && let Some(error) = first_error
        {
            return Err(CoreError::from_api(&error, "restore"));
        }
        // Keyed by uid, so it applies to every mount: a restored node reappears
        // in whichever inode spaces show its parent, not only the primary one.
        self.for_each_state(|st| {
            for parent in &parents {
                if let Some(&ino) = st.by_uid.get(parent) {
                    st.invalidate_listing(ino);
                }
            }
        });
        self.invalidate_trash();
        Ok(restored.len())
    }

    /// Permanently delete trashed nodes. Irreversible on the server; locally it
    /// drops any metadata and cached content the node still owns.
    fn delete_forever(&self, uids: &[String]) -> CoreResult<usize> {
        let parsed = Self::parse_uids(uids)?;
        // Streamed, and each node's local state is dropped as its batch lands.
        // A permanent delete is irreversible, so the useful property is that a
        // daemon interrupted mid-batch has already forgotten exactly the nodes
        // the server destroyed — never more, never fewer.
        let mut deleted = 0usize;
        let mut first_error: Option<ProtonError> = None;
        self.rt
            .block_on(async {
                let mut outcomes = std::pin::pin!(self.client.delete_nodes_streaming(&parsed));
                while let Some(item) = outcomes.next().await {
                    let (uid, outcome) = item?;
                    match outcome {
                        Ok(()) => {
                            self.drop_local(std::slice::from_ref(&uid));
                            deleted += 1;
                        }
                        Err(e) => {
                            warn!(%uid, error = %e, "permanent delete failed for a node");
                            first_error.get_or_insert(e);
                        }
                    }
                }
                Ok::<(), ProtonError>(())
            })
            .map_err(|e| CoreError::from_api(&e, "delete"))?;
        if deleted == 0
            && let Some(error) = first_error
        {
            return Err(CoreError::from_api(&error, "delete"));
        }
        self.invalidate_trash();
        Ok(deleted)
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
        // Every mount: these nodes are gone from the server, and a sync-folder
        // fork showing the same uid would otherwise keep serving them
        // (`docs/BUGS.md` B74). A uid is unique across inode spaces, so this is
        // a no-op in every mount but the one that holds it.
        self.for_each_state(|st| {
            for uid in uids {
                st.forget(uid);
            }
        });
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
        self.require_uid_writable(&parent_uid)
            .map_err(|error| self.errno_error(error, "create folder access"))?;
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
        let mut st = self.state();
        let ino = st.intern(pino, node);
        if let Some(kids) = st.children.get_mut(&pino)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        drop(st);
        self.flush_access_changes();
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

fn publish_virtual_root_in_listing(
    db: &Db,
    state: &mut State,
    parent_ino: u64,
    snapshot: &RootListingSnapshot,
    plan: VirtualRootPlan,
) -> Result<(), Errno> {
    if !snapshot.is_current(state, parent_ino) {
        return Err(Errno::EAGAIN);
    }

    db.publish_virtual_root(SHARED_WITH_ME_NAME, &plan.node)
        .map_err(|error| {
            error!(%error, "atomically publishing synthetic shared root failed");
            Errno::EIO
        })?;

    let uid = plan.node.uid.clone();
    state.share_access.insert(uid, Access::Viewer);
    let virtual_ino = state.intern_mem(parent_ino, plan.node);
    let published =
        reconcile_virtual_root_in_listing(state, parent_ino, snapshot, virtual_ino, plan.visible);
    debug_assert!(
        published,
        "snapshot cannot change while the state lock is held"
    );
    Ok(())
}

/// Update the synthetic dentry only while the parent listing is still known.
///
/// Event invalidation can remove the listing after its real names were captured.
/// Recreating it here would publish a synthetic-only partial snapshot as complete.
fn reconcile_virtual_root_in_listing(
    state: &mut State,
    parent_ino: u64,
    snapshot: &RootListingSnapshot,
    virtual_ino: u64,
    visible: bool,
) -> bool {
    if !snapshot.is_current(state, parent_ino) {
        return false;
    }
    let Some(children) = state.children.get_mut(&parent_ino) else {
        return false;
    };
    children.retain(|ino| *ino != virtual_ino);
    if visible {
        children.push(virtual_ino);
    }
    true
}

fn resolve_anywhere_with(
    raw_uid: &str,
    resident: impl FnOnce(&NodeUid) -> bool,
    mirrored: impl FnOnce(&NodeUid) -> pdfs_core::Result<bool>,
) -> CoreResult<NodeUid> {
    let uid =
        parse_uid(raw_uid).ok_or_else(|| CoreError::invalid(format!("invalid uid: {raw_uid}")))?;
    if is_local_uid(&uid) || is_virtual_uid(&uid) {
        return Err(CoreError::invalid(format!("reserved uid: {raw_uid}")));
    }
    if resident(&uid) {
        return Ok(uid);
    }
    if mirrored(&uid)? {
        return Ok(uid);
    }
    Err(CoreError::not_found(format!(
        "node is not present in any location: {raw_uid}"
    )))
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
    if vol.is_empty() || link.is_empty() || link.contains('~') {
        return None;
    }
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
    parse_uid(s)
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
                active_revision_id: None,
                content_sha1: None,
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
        membership: None,
        // A locally fabricated node is on the main volume, never a photo/album.
        photo: None,
        album: None,
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

    /// Whether the batch has ended, however it ended. A waiter that sees this
    /// has nothing left to wait for.
    fn is_finished(&self) -> bool {
        self.inner.lock().done
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

/// How often [`SizeWaitQueue::serve`] re-tests its parked waiters.
///
/// The predicates read `state`, which the applying thread writes under its own
/// lock, so this polls rather than waiting on each slot's condvar: one thread
/// cannot wait on a hundred condvars, and the whole point of the queue is that
/// there is only one thread. Twenty milliseconds is far below the round trip a
/// waiter is waiting on and only ticks while something is actually parked.
const SIZE_WAIT_POLL: Duration = Duration::from_millis(20);

/// A reply held back until a size upgrade resolves the node it describes.
struct SizeWaiter {
    /// The batch this reply is waiting on. When it ends, so does the wait.
    slot: Arc<SizeUpgrade>,
    /// Whether *this* waiter's node now has a real size.
    ///
    /// Called with no queue lock held: it reaches into `state`, and the thread
    /// applying a chunk holds `state` before it announces the chunk — taking
    /// them in the other order would close the cycle.
    resolved: Box<dyn Fn() -> bool + Send>,
    /// When to answer with the provisional size anyway. A `stat` that never
    /// returns is far worse than one that is briefly wrong.
    deadline: Instant,
    /// Sends the FUSE reply. Runs on the serving thread.
    respond: Box<dyn FnOnce() + Send>,
}

#[derive(Default)]
struct Parked {
    waiters: Vec<SizeWaiter>,
    /// Whether a thread is currently serving `waiters`. The thread clears this
    /// as it retires, so the next park starts a new one.
    running: bool,
}

/// Every reply parked on a size upgrade, served by at most one thread.
///
/// This exists because waiting used to happen on the [`Workers`] pool. Sizes
/// are single-flighted per folder, so an `ls -l` across N cold folders parks a
/// waiter per entry — and each of those held a pool thread asleep on a condvar
/// for up to [`SizeUpgrade::WAIT`]. With enough folders that is every thread in
/// both lanes, including the ones reserved for metadata precisely so that this
/// could not happen (audit F4). Parking the `Reply` instead costs one thread
/// for the whole mount, and only while something is parked.
#[derive(Default)]
struct SizeWaitQueue {
    parked: Mutex<Parked>,
    /// Woken when a waiter is added, so a serving thread re-computes how long
    /// it may sleep.
    added: Condvar,
}

impl SizeWaitQueue {
    /// Park `waiter`. Returns whether the caller must start a serving thread.
    fn park(&self, waiter: SizeWaiter) -> bool {
        let mut parked = self.parked.lock();
        parked.waiters.push(waiter);
        if parked.running {
            drop(parked);
            self.added.notify_all();
            return false;
        }
        parked.running = true;
        true
    }

    /// Answer every waiter whose node has resolved, whose batch has ended, or
    /// whose deadline has passed; sleep until the next one might. Returns when
    /// nothing is parked any more.
    fn serve(&self) {
        loop {
            // Taken out of the lock wholesale: the predicates and the replies
            // both take locks of their own, and none of them may be called
            // while this one is held.
            let taken = std::mem::take(&mut self.parked.lock().waiters);
            let now = Instant::now();
            let mut keep = Vec::with_capacity(taken.len());
            for waiter in taken {
                if now >= waiter.deadline || waiter.slot.is_finished() || (waiter.resolved)() {
                    (waiter.respond)();
                } else {
                    keep.push(waiter);
                }
            }
            let mut parked = self.parked.lock();
            parked.waiters.append(&mut keep);
            if parked.waiters.is_empty() {
                parked.running = false;
                return;
            }
            let until = parked
                .waiters
                .iter()
                .map(|w| w.deadline)
                .min()
                .unwrap_or(now)
                .min(Instant::now() + SIZE_WAIT_POLL);
            self.added.wait_until(&mut parked, until);
        }
    }

    /// Answer everything parked, immediately, and retire the serving thread.
    /// Used when there is no thread to serve with: a provisional size now beats
    /// a reply that never comes.
    fn answer_all(&self) {
        let taken = {
            let mut parked = self.parked.lock();
            parked.running = false;
            std::mem::take(&mut parked.waiters)
        };
        for waiter in taken {
            (waiter.respond)();
        }
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

/// The server revision id of a node's active revision, if it is a file that has
/// one. The stable identity the drain conflict-checks against (see [`Baseline`]).
fn node_revision_id(node: &Node) -> Option<String> {
    match &node.kind {
        NodeKind::Folder => None,
        NodeKind::File {
            active_revision_id, ..
        } => active_revision_id.clone(),
    }
}

/// The plaintext content SHA-1 of a file node, if its active revision carried one.
/// A download-free content fingerprint the conflict sweep uses to prove two files
/// hold identical bytes before it removes one.
fn node_content_sha1(node: &Node) -> Option<String> {
    match &node.kind {
        NodeKind::Folder => None,
        NodeKind::File { content_sha1, .. } => content_sha1.clone(),
    }
}

/// `"1 file"` / `"3 files"` — a count with a correctly pluralised noun, for
/// human-readable activity-log lines.
fn count_noun(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Seed a truncate scratch file from a complete queued revision.
///
/// `File` writes use an explicit offset so this is independent of either
/// descriptor's cursor and remains correct when the destination came from the
/// cache's scratch allocator.
fn copy_pending_for_truncate(pending: &PendingRevision, destination: &File) -> std::io::Result<()> {
    let source = File::open(&pending.path)?;
    destination.set_len(0)?;
    let mut offset = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = source.read_at(&mut buffer, offset)?;
        if count == 0 {
            break;
        }
        destination.write_all_at(&buffer[..count], offset)?;
        offset += count as u64;
    }
    destination.set_len(offset)
}

/// Whether a rename must be represented as a durable desired end state.
/// Offline operations and operations targeting a not-yet-uploaded directory
/// cannot use the API immediately. Online remote-to-remote operations stay
/// synchronous so a successful kernel reply means the remote namespace has
/// already reached the same end state.
fn rename_needs_queue(
    online: bool,
    destination_is_local: bool,
    _parent_changed: bool,
    _name_changed: bool,
) -> bool {
    !online || destination_is_local
}

/// Convert one kernel pathname component into the UTF-8 name accepted by Drive.
/// Linux filesystems conventionally cap a component at `NAME_MAX` (255 bytes),
/// but the FUSE kernel path can pass a longer component through to userspace.
fn fuse_name(name: &OsStr) -> Result<String, Errno> {
    let bytes = name.as_bytes();
    if bytes.len() > 255 {
        return Err(Errno::ENAMETOOLONG);
    }
    if bytes.is_empty() || bytes == b"." || bytes == b".." {
        return Err(Errno::EINVAL);
    }
    name.to_str().map(str::to_owned).ok_or(Errno::EILSEQ)
}

fn node_visible(node: &Node, folder_uid: &NodeUid, hidden: &HashSet<NodeUid>) -> bool {
    !node.trashed && node.uid != *folder_uid && !hidden.contains(&node.uid)
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
mod merge_over_pending_tests {
    use super::{PendingRevision, merge_over_pending};
    use pdfs_core::cache::{Baseline, StagedWrite};
    use std::io::Write as _;
    use std::path::PathBuf;

    /// A unique temp directory removed on drop; avoids a dev-dependency, as in
    /// `state.rs`.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-merge-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn staged(len: u64, base_size: u64, authored: &[(u64, u64)]) -> StagedWrite {
        StagedWrite {
            uid: "vol~link".into(),
            len,
            base_size,
            base_mtime: 100,
            complete: authored == [(0, len)],
            authored: authored.to_vec(),
            based_on: Some(Baseline {
                mtime: 100,
                size: base_size,
                hash: None,
                revision_id: Some("r1".into()),
            }),
        }
    }

    /// Write a file of `len` bytes with `fill` at every offset in `ranges`.
    fn blob(
        dir: &std::path::Path,
        name: &str,
        len: u64,
        ranges: &[(u64, u64)],
        fill: u8,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut bytes = vec![0u8; len as usize];
        for &(s, e) in ranges {
            bytes[s as usize..e as usize].fill(fill);
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&bytes).unwrap();
        path
    }

    /// The case the merge exists for: two small edits to different parts of a
    /// file, the second closing before the first has drained. The second blob
    /// must come out holding both, or the first edit is silently lost.
    #[test]
    fn the_superseding_write_inherits_the_earlier_ones_bytes() {
        let dir = TempDir::new();
        let previous = PendingRevision {
            path: blob(&dir.0, "old", 64, &[(0, 16)], 0xAA),
            meta: staged(64, 64, &[(0, 16)]),
        };
        let src = blob(&dir.0, "new", 64, &[(32, 48)], 0xBB);
        let mut meta = staged(64, 64, &[(32, 48)]);

        merge_over_pending(&mut meta, &src, &previous).unwrap();

        assert_eq!(meta.authored, vec![(0, 16), (32, 48)]);
        assert!(
            !meta.complete,
            "the untouched middle still comes from the base"
        );
        let merged = std::fs::read(&src).unwrap();
        assert!(
            merged[0..16].iter().all(|&b| b == 0xAA),
            "earlier edit kept"
        );
        assert!(merged[32..48].iter().all(|&b| b == 0xBB), "newer edit kept");
    }

    /// Where they overlap, the newer write wins — it is the later state of the
    /// file, and the older bytes were never visible to anyone after it landed.
    #[test]
    fn the_newer_write_wins_the_ranges_it_authored() {
        let dir = TempDir::new();
        let previous = PendingRevision {
            path: blob(&dir.0, "old", 32, &[(0, 32)], 0xAA),
            meta: staged(32, 32, &[(0, 32)]),
        };
        let src = blob(&dir.0, "new", 32, &[(8, 16)], 0xBB);
        let mut meta = staged(32, 32, &[(8, 16)]);

        merge_over_pending(&mut meta, &src, &previous).unwrap();

        assert_eq!(meta.authored, vec![(0, 32)]);
        assert!(meta.complete, "between them they cover the file");
        let merged = std::fs::read(&src).unwrap();
        assert!(
            merged[8..16].iter().all(|&b| b == 0xBB),
            "newer edit survives"
        );
        assert!(merged[16..32].iter().all(|&b| b == 0xAA));
    }

    /// The baseline has to come from the write being superseded. This one's own
    /// "base" is the earlier *staged blob*, whose size and mtime are ours, not
    /// the server's — filling or conflict-checking against that would compare
    /// the remote to a revision it never had.
    #[test]
    fn the_baseline_is_inherited_from_the_write_being_superseded() {
        let dir = TempDir::new();
        let mut earlier = staged(64, 64, &[(0, 16)]);
        earlier.base_mtime = 42;
        earlier.base_size = 50;
        earlier.based_on = Some(Baseline {
            mtime: 42,
            size: 50,
            hash: None,
            revision_id: Some("original".into()),
        });
        let previous = PendingRevision {
            path: blob(&dir.0, "old", 64, &[(0, 16)], 0xAA),
            meta: earlier,
        };
        let src = blob(&dir.0, "new", 64, &[(32, 48)], 0xBB);
        // Opened over the staged blob, so this write thinks its base is 64/100.
        let mut meta = staged(64, 64, &[(32, 48)]);

        merge_over_pending(&mut meta, &src, &previous).unwrap();

        assert_eq!(meta.base_mtime, 42);
        assert_eq!(meta.base_size, 50);
        assert_eq!(
            meta.based_on.unwrap().revision_id.as_deref(),
            Some("original")
        );
    }

    /// A shrink must not drag the earlier write's tail back in.
    #[test]
    fn a_truncating_write_does_not_inherit_past_its_own_length() {
        let dir = TempDir::new();
        let previous = PendingRevision {
            path: blob(&dir.0, "old", 64, &[(0, 64)], 0xAA),
            meta: staged(64, 64, &[(0, 64)]),
        };
        let src = blob(&dir.0, "new", 16, &[(8, 16)], 0xBB);
        let mut meta = staged(16, 64, &[(8, 16)]);

        merge_over_pending(&mut meta, &src, &previous).unwrap();

        assert_eq!(meta.authored, vec![(0, 16)]);
        assert!(meta.complete);
        assert_eq!(std::fs::metadata(&src).unwrap().len(), 16);
    }
}

#[cfg(test)]
mod size_upgrade_tests {
    use super::{SizeUpgrade, SizeWaitQueue, SizeWaiter};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Park a waiter the way [`Core::await_size`] does, and hand back the
    /// channel its reply is sent down, so a test can time the release.
    fn park(
        queue: &Arc<SizeWaitQueue>,
        slot: Arc<SizeUpgrade>,
        resolved: impl Fn() -> bool + Send + 'static,
        deadline: Instant,
    ) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let waiter = SizeWaiter {
            slot,
            resolved: Box::new(resolved),
            deadline,
            respond: Box::new(move || {
                let _ = tx.send(());
            }),
        };
        if queue.park(waiter) {
            let queue = queue.clone();
            std::thread::spawn(move || queue.serve());
        }
        rx
    }

    /// Long enough that a test never trips the deadline by accident, short
    /// enough that one which is *meant* to does not take ten seconds.
    fn far_off() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    /// The point of the per-chunk design: a waiter is answered as soon as *its
    /// own* file is resolved, without waiting for the rest of the folder. A
    /// single batch over 793 nodes took ~80 s, which outran the timeout and
    /// handed back the provisional size this is all meant to prevent.
    #[test]
    fn a_waiter_returns_on_the_chunk_that_resolves_it() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        let mine = Arc::new(AtomicBool::new(false));
        let worker = slot.clone();
        let flag = mine.clone();
        let t = std::thread::spawn(move || {
            // Our node lands in the first chunk; two more follow.
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::SeqCst);
            worker.chunk_done();
            std::thread::sleep(Duration::from_millis(300));
            worker.chunk_done();
            worker.finish();
        });
        let started = Instant::now();
        let rx = park(&queue, slot, move || mine.load(Ordering::SeqCst), far_off());
        rx.recv().expect("the waiter is answered");
        // Released by the first chunk, not the last.
        assert!(started.elapsed() < Duration::from_millis(200));
        t.join().unwrap();
    }

    /// A chunk that does not resolve this waiter must not answer it.
    #[test]
    fn an_unrelated_chunk_does_not_release_a_waiter() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        let worker = slot.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            worker.chunk_done();
            std::thread::sleep(Duration::from_millis(120));
            worker.finish();
        });
        let started = Instant::now();
        // Never resolved: only `finish` can end this wait.
        let rx = park(&queue, slot, || false, far_off());
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() >= Duration::from_millis(140));
        t.join().unwrap();
    }

    /// A batch that ends without resolving the node — a failed fetch — still
    /// answers its waiters, who fall back to the provisional size.
    #[test]
    fn finish_releases_a_waiter_that_was_never_resolved() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        let worker = slot.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            worker.finish();
        });
        let started = Instant::now();
        let rx = park(&queue, slot, || false, far_off());
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() < SizeUpgrade::WAIT);
        t.join().unwrap();
    }

    /// Already resolved before parking: answered on the first pass. This is the
    /// follower that arrives after the chunk it needed has landed.
    #[test]
    fn an_already_resolved_waiter_does_not_block() {
        let queue = Arc::new(SizeWaitQueue::default());
        let started = Instant::now();
        let rx = park(&queue, Arc::new(SizeUpgrade::default()), || true, far_off());
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// Finishing before anyone parks must not strand the late arrival — the
    /// flag is what is checked, not the notification, which it would miss.
    #[test]
    fn waiting_after_finish_returns_at_once() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        slot.finish();
        let started = Instant::now();
        let rx = park(&queue, slot, || false, far_off());
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// Every waiter is answered, not just the first: one `ls -l` puts one
    /// waiter per file on the same folder.
    #[test]
    fn all_waiters_are_released() {
        let queue = Arc::new(SizeWaitQueue::default());
        let slot = Arc::new(SizeUpgrade::default());
        let waiters: Vec<_> = (0..8)
            .map(|_| park(&queue, slot.clone(), || false, far_off()))
            .collect();
        std::thread::sleep(Duration::from_millis(20));
        slot.finish();
        for rx in waiters {
            rx.recv().expect("every waiter is answered");
        }
    }

    /// The deadline is the backstop: a batch that never finishes must not wedge
    /// the caller's `stat` forever.
    #[test]
    fn a_waiter_gives_up_at_the_deadline() {
        let queue = Arc::new(SizeWaitQueue::default());
        let started = Instant::now();
        // Nothing will ever resolve or finish this.
        let rx = park(
            &queue,
            Arc::new(SizeUpgrade::default()),
            || false,
            Instant::now() + Duration::from_millis(120),
        );
        rx.recv().expect("the waiter is answered");
        assert!(started.elapsed() >= Duration::from_millis(120));
    }

    /// The serving thread retires when nothing is parked, and a later park
    /// starts a new one — otherwise the second `ls -l` of the session would
    /// hang forever.
    #[test]
    fn the_queue_serves_again_after_it_has_drained() {
        let queue = Arc::new(SizeWaitQueue::default());
        for _ in 0..3 {
            let rx = park(&queue, Arc::new(SizeUpgrade::default()), || true, far_off());
            rx.recv().expect("the waiter is answered");
        }
    }

    /// With no thread to serve them, waiters are answered on the spot rather
    /// than never: a provisional size beats a `stat` that does not return.
    #[test]
    fn answer_all_releases_everything_parked() {
        let queue = SizeWaitQueue::default();
        let (tx, rx) = mpsc::channel();
        for _ in 0..4 {
            let tx = tx.clone();
            queue.park(SizeWaiter {
                slot: Arc::new(SizeUpgrade::default()),
                resolved: Box::new(|| false),
                deadline: far_off(),
                respond: Box::new(move || {
                    let _ = tx.send(());
                }),
            });
        }
        drop(tx);
        queue.answer_all();
        assert_eq!(rx.into_iter().count(), 4);
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
                active_revision_id: None,
                content_sha1: None,
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
            membership: None,
            photo: None,
            album: None,
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
    use super::{
        Access, AccessFlags, Errno, HashMap, Intervals, PendingRevision, RootListingSnapshot,
        SELF_CHANGE_TTL_MS, ShareId, SharedWithMeItem, StateRegistry, VirtualRootPlan,
        accepted_share_provenance, conflict_name, copy_pending_for_truncate, fuse_name,
        is_stale_mount, node_visible, note_self_change, parse_node_uid, prepare_shared_roots,
        preserve_on_access_denied, publish_virtual_root_in_listing,
        reconcile_virtual_root_in_listing, release_can_discard_unlinked,
        release_must_retain_queued_trash, release_unlinked_entry, rename_needs_queue,
        require_node_parent_access, require_rename_access, resolve_anywhere_with,
        shared_with_me_uid, take_self_change, uid_write_authority, virtual_node,
    };
    use super::{Db, WriteAuthority};

    /// The collapse [`Core::require_uid_writable`] performs, without a `Core`:
    /// every non-writable authority is one `EACCES` on the syscall path.
    fn require_uid_access(db: &Db, uid: &NodeUid, live: &[Access]) -> Result<(), Errno> {
        match uid_write_authority(db, uid, live) {
            WriteAuthority::Writable => Ok(()),
            WriteAuthority::Denied | WriteAuthority::Unknown => Err(Errno::EACCES),
        }
    }
    use crate::filesystem::access_allowed;
    use pdfs_core::cache::{Baseline, StagedWrite};
    use pdfs_core::db::{OP_REVISION, OP_TRASH, PendingOp};

    fn session_flag(live: bool) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(live))
    }

    /// The registry every mount publishes itself into is what lets the daemon's
    /// single drain thread reach a node living in an on-demand fork's inode
    /// space. A fork missing from it reads as an empty file for the life of the
    /// daemon (`docs/BUGS.md` B74), so the walk has to see *every* registered
    /// mount — and only the ones still mounted.
    #[test]
    fn state_registry_walks_every_live_mount_and_reaps_the_rest() {
        let registry = StateRegistry::default();
        assert!(registry.live().is_empty(), "a fresh registry has no mounts");

        // The primary mount plus two on-demand forks.
        let (primary, _p) = state_test_helper();
        let (fork_a, _a) = state_test_helper();
        let (fork_b, _b) = state_test_helper();
        let primary = std::sync::Arc::new(parking_lot::Mutex::new(primary));
        let fork_a = std::sync::Arc::new(parking_lot::Mutex::new(fork_a));
        let fork_b = std::sync::Arc::new(parking_lot::Mutex::new(fork_b));
        for (mountpoint, state) in [
            ("/mnt/primary", &primary),
            ("/mnt/fork-a", &fork_a),
            ("/mnt/fork-b", &fork_b),
        ] {
            registry.register_bare(
                std::path::Path::new(mountpoint),
                state,
                std::sync::Arc::new(std::sync::OnceLock::new()),
                session_flag(false),
            );
        }
        assert_eq!(registry.live().len(), 3, "every registered mount is walked");

        // `live()` must not itself pin a mount alive, or an unmounted fork could
        // never be reaped.
        assert_eq!(
            std::sync::Arc::strong_count(&fork_a),
            1,
            "the registry holds forks weakly"
        );

        // Unmounting a fork drops its state; the dead entry goes on the next walk.
        drop(fork_a);
        assert_eq!(registry.live().len(), 2, "an unmounted fork is dropped");
        assert_eq!(
            registry.0.lock().len(),
            2,
            "and its slot is reaped, not merely skipped"
        );

        drop(primary);
        drop(fork_b);
        assert!(
            registry.live().is_empty(),
            "the last mount going away empties the registry"
        );
    }

    #[test]
    fn state_registry_covering_uses_longest_prefix_and_ignores_dead_forks() {
        let registry = StateRegistry::default();
        let (primary, _primary_dir) = rooted_state("primary-volume", "primary");
        let (nested, _nested_dir) = rooted_state("nested-volume", "nested");
        let (sibling, _sibling_dir) = rooted_state("sibling-volume", "sibling");
        let primary_uid = primary.entries[&super::ROOT_INO].uid.clone();
        let primary = std::sync::Arc::new(parking_lot::Mutex::new(primary));
        let nested = std::sync::Arc::new(parking_lot::Mutex::new(nested));
        let sibling = std::sync::Arc::new(parking_lot::Mutex::new(sibling));
        let primary_path = std::path::Path::new("/home/me/ProtonDrive");
        let nested_path = std::path::Path::new("/home/me/ProtonDrive/Device");
        let sibling_path = std::path::Path::new("/home/me/Archive");
        let primary_live = session_flag(false);
        let nested_live = session_flag(false);
        let sibling_live = session_flag(true);

        registry.register_bare(
            primary_path,
            &primary,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            primary_live.clone(),
        );
        registry.register_bare(
            nested_path,
            &nested,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            nested_live.clone(),
        );
        registry.register_bare(
            sibling_path,
            &sibling,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            sibling_live,
        );
        assert_eq!(registry.live().len(), 3);
        assert!(!registry.is_mounted_at(primary_path));
        assert!(!registry.is_mounted_at(nested_path));
        assert!(
            !registry.owns_visible_uid(&primary_uid),
            "an unspawned session must not authorize resident uids"
        );
        assert!(
            registry
                .covering(&nested_path.join("folder/file.txt"))
                .is_none(),
            "a registered but unspawned session must not win path covering"
        );
        primary_live.store(true, std::sync::atomic::Ordering::Release);
        nested_live.store(true, std::sync::atomic::Ordering::Release);
        assert!(registry.owns_visible_uid(&primary_uid));

        let (covering, covering_state, _, _) = registry
            .covering(&nested_path.join("folder/file.txt"))
            .unwrap();
        assert_eq!(covering, nested_path);
        assert!(std::sync::Arc::ptr_eq(&covering_state, &nested));
        drop(covering_state);
        let (covering, _, _, _) = registry.covering(nested_path).unwrap();
        assert_eq!(covering, nested_path, "an exact mountpoint covers itself");
        let (covering, _, _, _) = registry
            .covering(std::path::Path::new("/home/me/ProtonDrive/Device/folder/"))
            .unwrap();
        assert_eq!(
            covering, nested_path,
            "a trailing separator does not change component-prefix selection"
        );
        let (covering, _, _, _) = registry
            .covering(std::path::Path::new(
                "/home/me/ProtonDrive/DeviceBackup/file",
            ))
            .unwrap();
        assert_eq!(
            covering, primary_path,
            "/Device must not string-prefix-match /DeviceBackup"
        );
        let (covering, covering_state, _, _) =
            registry.covering(&sibling_path.join("file")).unwrap();
        assert_eq!(covering, sibling_path);
        assert!(std::sync::Arc::ptr_eq(&covering_state, &sibling));
        assert!(
            registry
                .covering(std::path::Path::new("/home/me/ProtonDriveBackup/file"))
                .is_none(),
            "/ProtonDrive must not string-prefix-match /ProtonDriveBackup"
        );
        assert!(
            registry
                .covering(std::path::Path::new("/tmp/outside"))
                .is_none(),
            "an outside path has no covering mount"
        );
        assert!(registry.is_mounted_at(primary_path));
        assert!(registry.is_mounted_at(nested_path));
        assert!(
            !registry.is_mounted_at(&nested_path.join("folder")),
            "a path covered by a session is not itself a mountpoint"
        );

        // This is also the failed-fork case: the caller registered the fork,
        // session construction failed, and its last strong `Core`/state handle
        // was dropped. It must not make ListLocations report mounted.
        drop(nested);
        assert!(!registry.is_mounted_at(nested_path));
        assert_eq!(registry.live().len(), 2);
    }

    /// The routing half of `docs/BUGS.md` B86: a path under a secondary mount
    /// has to resolve to *that* mount's inode space and a suffix relative to
    /// *its* root, or `pdfs ls` and `pdfs refresh` can only ever name the
    /// primary mount and a stale on-demand folder has no escape hatch.
    #[test]
    fn covering_parts_route_a_path_to_the_mount_that_owns_it() {
        let registry = StateRegistry::default();
        let (primary, _primary_dir) = rooted_state("primary-volume", "primary");
        let (nested, _nested_dir) = rooted_state("nested-volume", "nested");
        let primary = std::sync::Arc::new(parking_lot::Mutex::new(primary));
        let nested = std::sync::Arc::new(parking_lot::Mutex::new(nested));
        let primary_path = std::path::Path::new("/home/me/ProtonDrive");
        let nested_path = std::path::Path::new("/home/me/ProtonDrive/Device");
        let primary_upgrades = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let nested_upgrades = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));

        registry.register(
            primary_path,
            &primary,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            session_flag(true),
            primary_upgrades.clone(),
        );
        registry.register(
            nested_path,
            &nested,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            session_flag(true),
            nested_upgrades.clone(),
        );

        let deep = nested_path.join("sub/file.txt");
        let parts = registry.covering_parts(&deep).expect("nested mount covers");
        assert_eq!(parts.mountpoint, nested_path, "the nested mount wins");
        assert!(
            std::sync::Arc::ptr_eq(&parts.state, &nested),
            "the request must be answered in the nested inode space"
        );
        assert!(
            std::sync::Arc::ptr_eq(&parts.size_upgrades, &nested_upgrades),
            "and share that mount's in-flight size upgrades, not the primary's"
        );
        assert_eq!(
            deep.strip_prefix(&parts.mountpoint).unwrap(),
            std::path::Path::new("sub/file.txt"),
            "the suffix is relative to the mount that owns it, not the primary"
        );

        let shallow = primary_path.join("Documents");
        let parts = registry
            .covering_parts(&shallow)
            .expect("primary mount covers");
        assert!(
            std::sync::Arc::ptr_eq(&parts.state, &primary),
            "a path outside the nested root still routes to the primary"
        );

        assert!(
            registry
                .covering_parts(std::path::Path::new("/tmp/outside"))
                .is_none(),
            "a path under no mount routes nowhere, and the caller reports that"
        );
    }

    #[test]
    fn state_registry_resolves_uids_from_any_resident_mount() {
        let registry = StateRegistry::default();
        let (primary, _primary_dir) = rooted_state("primary-volume", "primary");
        let (mut fork, _fork_dir) = rooted_state("device-volume", "device");
        let primary_uid = primary.entries[&super::ROOT_INO].uid.clone();
        let fork_node =
            node_helper_in_volume("device-volume", "fork", Some("device"), "fork", true);
        let fork_uid = fork_node.uid.clone();
        let fork_ino = fork.intern(super::ROOT_INO, fork_node);
        fork.children.insert(super::ROOT_INO, vec![fork_ino]);
        let primary = std::sync::Arc::new(parking_lot::Mutex::new(primary));
        let fork = std::sync::Arc::new(parking_lot::Mutex::new(fork));
        registry.register_bare(
            std::path::Path::new("/mnt/primary"),
            &primary,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            session_flag(true),
        );
        registry.register_bare(
            std::path::Path::new("/mnt/device"),
            &fork,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            session_flag(true),
        );

        assert!(registry.owns_visible_uid(&primary_uid));
        assert!(registry.owns_visible_uid(&fork_uid));
        assert!(!registry.owns_visible_uid(&super::parse_uid("device-volume~missing").unwrap()));

        drop(fork);
        assert!(
            !registry.owns_visible_uid(&fork_uid),
            "an unmounted on-demand state must stop authorizing its uids"
        );
    }

    #[test]
    fn state_registry_treats_missing_listing_as_unknown_not_absent() {
        let registry = StateRegistry::default();
        let (mut state, _dir) = rooted_state("device-volume", "device");
        let child_node =
            node_helper_in_volume("device-volume", "child", Some("device"), "child.txt", false);
        let child_uid = child_node.uid.clone();
        let child_ino = state.intern(super::ROOT_INO, child_node);
        state.children.insert(super::ROOT_INO, vec![child_ino]);
        let state = std::sync::Arc::new(parking_lot::Mutex::new(state));
        registry.register_bare(
            std::path::Path::new("/mnt/device"),
            &state,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            session_flag(true),
        );

        assert!(registry.owns_visible_uid(&child_uid));

        state.lock().invalidate_listing(super::ROOT_INO);
        assert!(
            registry.owns_visible_uid(&child_uid),
            "an invalidated listing is unknown and must not hide a valid resident child"
        );

        state.lock().children.insert(super::ROOT_INO, Vec::new());
        assert!(
            !registry.owns_visible_uid(&child_uid),
            "a present listing that omits the child proves it is no longer visible"
        );
    }

    #[test]
    fn virtual_root_reconcile_does_not_recreate_an_invalidated_listing() {
        let registry = StateRegistry::default();
        let (mut state, _dir) = rooted_state("primary-volume", "root");
        let root_uid = state.entries[&super::ROOT_INO].uid.clone();
        let child_node =
            node_helper_in_volume("primary-volume", "child", Some("root"), "child.txt", false);
        let child_uid = child_node.uid.clone();
        let child_ino = state.intern(super::ROOT_INO, child_node);
        state.children.insert(super::ROOT_INO, vec![child_ino]);

        // Simulate ensure_children observing a complete primary-root listing.
        let snapshot = RootListingSnapshot::capture(&state, super::ROOT_INO).unwrap();
        let detected_names = snapshot.real_names();
        assert_eq!(
            detected_names,
            std::collections::HashSet::from(["child.txt".into()])
        );

        // An event wins the race before virtual-root dentry publication.
        state.invalidate_listing(super::ROOT_INO);
        let virtual_ino = state.intern_mem(
            super::ROOT_INO,
            virtual_node(root_uid, "Shared with me".into(), 0),
        );
        assert!(
            !reconcile_virtual_root_in_listing(
                &mut state,
                super::ROOT_INO,
                &snapshot,
                virtual_ino,
                true,
            ),
            "the invalidated snapshot must not publish"
        );
        assert!(
            !state.children.contains_key(&super::ROOT_INO),
            "reconciliation must leave an invalidated listing absent"
        );

        let state = std::sync::Arc::new(parking_lot::Mutex::new(state));
        registry.register_bare(
            std::path::Path::new("/mnt/device"),
            &state,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            session_flag(true),
        );
        assert!(
            registry.owns_visible_uid(&child_uid),
            "the absent listing remains unknown until refresh, so a valid resident uid is allowed"
        );
    }

    #[test]
    fn virtual_root_reconcile_rejects_repopulated_changed_snapshot() {
        let (mut state, _dir) = rooted_state("primary-volume", "root");
        let root_uid = state.entries[&super::ROOT_INO].uid.clone();
        let child =
            node_helper_in_volume("primary-volume", "child", Some("root"), "child.txt", false);
        let child_ino = state.intern(super::ROOT_INO, child);
        state.children.insert(super::ROOT_INO, vec![child_ino]);
        let stale_snapshot = RootListingSnapshot::capture(&state, super::ROOT_INO).unwrap();

        let mut persisted_virtual = virtual_node(root_uid.clone(), "Shared with me".into(), 0);
        persisted_virtual.trashed = true;
        state
            .flushed_db()
            .publish_virtual_root(super::SHARED_WITH_ME_NAME, &persisted_virtual)
            .unwrap();
        let mut indexed_descendant = node_helper_in_volume(
            "foreign-volume",
            "inside",
            None,
            "SnapshotRaceFindable",
            false,
        );
        indexed_descendant.parent_uid = Some(shared_with_me_uid());
        state.flushed_db().upsert_node(&indexed_descendant).unwrap();
        assert!(
            state
                .db
                .search("SnapshotRaceFindable", 10)
                .unwrap()
                .is_empty(),
            "the hidden synthetic ancestor must hide descendant search hits"
        );

        state.invalidate_listing(super::ROOT_INO);
        state.entries.get_mut(&child_ino).unwrap().node.name = "Shared with me".into();
        state.children.insert(super::ROOT_INO, vec![child_ino]);
        let replacement_snapshot = RootListingSnapshot::capture(&state, super::ROOT_INO).unwrap();
        assert_ne!(stale_snapshot, replacement_snapshot);
        assert_eq!(
            replacement_snapshot.real_names(),
            std::collections::HashSet::from(["Shared with me".into()])
        );

        let mut stale_node = persisted_virtual.clone();
        stale_node.trashed = false;
        let stale_plan = VirtualRootPlan {
            node: stale_node.clone(),
            visible: true,
        };
        let db = state.flushed_db();
        let error = publish_virtual_root_in_listing(
            &db,
            &mut state,
            super::ROOT_INO,
            &stale_snapshot,
            stale_plan,
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            Errno::EAGAIN.code(),
            "stale work must fail before persistence or dentry publication"
        );
        assert_eq!(
            state.children[&super::ROOT_INO],
            vec![child_ino],
            "the replacement listing must remain untouched"
        );
        assert!(
            state
                .db
                .node_by_uid(&shared_with_me_uid().to_string())
                .unwrap()
                .unwrap()
                .trashed,
            "stale work must not change persisted visibility"
        );
        assert!(
            state
                .db
                .search("SnapshotRaceFindable", 10)
                .unwrap()
                .is_empty(),
            "stale work must not change FTS ancestry"
        );

        let virtual_ino = state.intern_mem(super::ROOT_INO, stale_node);
        state
            .children
            .get_mut(&super::ROOT_INO)
            .unwrap()
            .push(virtual_ino);
        let current_snapshot = RootListingSnapshot::capture(&state, super::ROOT_INO).unwrap();
        state.entries.get_mut(&virtual_ino).unwrap().node.trashed = false;
        assert!(
            reconcile_virtual_root_in_listing(
                &mut state,
                super::ROOT_INO,
                &current_snapshot,
                virtual_ino,
                false,
            ),
            "the synthetic node's own visibility update must not stale the real-child snapshot"
        );
        assert_eq!(state.children[&super::ROOT_INO], vec![child_ino]);
    }

    #[test]
    fn virtual_root_publication_failure_is_atomic_and_leaves_state_unchanged() {
        let (mut state, _dir) = rooted_state("primary-volume", "root");
        let root_uid = state.entries[&super::ROOT_INO].uid.clone();
        let child =
            node_helper_in_volume("primary-volume", "child", Some("root"), "child.txt", false);
        let child_ino = state.intern(super::ROOT_INO, child);
        state.children.insert(super::ROOT_INO, vec![child_ino]);
        let snapshot = RootListingSnapshot::capture(&state, super::ROOT_INO).unwrap();

        let mut descendant = node_helper_in_volume(
            "foreign-volume",
            "inside",
            None,
            "AtomicRollbackFindable",
            false,
        );
        descendant.parent_uid = Some(shared_with_me_uid());
        state.flushed_db().upsert_node(&descendant).unwrap();
        // A node whose parent row is not cached — the synthetic root is not one
        // until this publication commits — is indexed under the path it can
        // actually be resolved to, which is its bare name. Publication reindexes
        // the subtree beneath the synthetic ancestor, so that path is what the
        // rollback has to restore.
        let path_before = {
            let hits = state
                .flushed_db()
                .search("AtomicRollbackFindable", 10)
                .unwrap();
            assert_eq!(hits.len(), 1, "the descendant starts out searchable");
            hits[0].path.clone()
        };
        assert_eq!(path_before, "AtomicRollbackFindable");

        // The pin is the transaction's final statement. Failing it proves the
        // earlier access, node, and descendant FTS work rolls back with it.
        state
            .flushed_db()
            .with_conn(|conn| {
                conn.execute_batch(
                    "CREATE TRIGGER reject_virtual_root_pin
                     BEFORE INSERT ON sync_state
                     WHEN NEW.key = 'shared_with_me_name'
                     BEGIN
                       SELECT RAISE(ABORT, 'injected virtual-root pin failure');
                     END;",
                )?;
                Ok(())
            })
            .unwrap();

        let entries_before = state.entries.len();
        let by_uid_before = state.by_uid.len();
        let next_ino_before = state.next_ino;
        let access_before = state.share_access.clone();
        let db = state.flushed_db();
        let result = publish_virtual_root_in_listing(
            &db,
            &mut state,
            super::ROOT_INO,
            &snapshot,
            VirtualRootPlan {
                node: virtual_node(root_uid, "Shared with me".into(), 0),
                visible: true,
            },
        );

        assert_eq!(result.unwrap_err().code(), Errno::EIO.code());
        assert_eq!(
            RootListingSnapshot::capture(&state, super::ROOT_INO),
            Some(snapshot),
            "the resident listing must not change on DB failure"
        );
        assert_eq!(state.entries.len(), entries_before);
        assert_eq!(state.by_uid.len(), by_uid_before);
        assert_eq!(state.next_ino, next_ino_before);
        assert_eq!(state.share_access, access_before);
        assert!(
            !state.by_uid.contains_key(&shared_with_me_uid()),
            "the synthetic inode must not be interned before commit"
        );

        assert_eq!(
            state
                .flushed_db()
                .state_str(super::SHARED_WITH_ME_NAME)
                .unwrap(),
            None
        );
        assert!(
            state
                .db
                .node_by_uid(&shared_with_me_uid().to_string())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .flushed_db()
                .share_access(&shared_with_me_uid())
                .unwrap(),
            None
        );
        let hits = state
            .flushed_db()
            .search("AtomicRollbackFindable", 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].path, path_before,
            "the descendant's indexed path must roll back with the failed pin, \
             not keep the synthetic ancestor the publication tried to add"
        );
    }

    #[test]
    fn state_registry_rejects_open_unlinked_and_revoked_residents() {
        let registry = StateRegistry::default();
        let (mut state, _dir) = rooted_state("device-volume", "device");

        let open_node =
            node_helper_in_volume("device-volume", "open", Some("device"), "open.txt", false);
        let open_uid = open_node.uid.clone();
        let open_ino = state.intern(super::ROOT_INO, open_node);
        state.children.insert(super::ROOT_INO, vec![open_ino]);
        state.entries.get_mut(&open_ino).unwrap().open_count = 1;

        let revoked_node =
            node_helper_in_volume("device-volume", "revoked", Some("device"), "revoked", true);
        let revoked_uid = revoked_node.uid.clone();
        let revoked_ino = state.intern(super::ROOT_INO, revoked_node);
        let child_node = node_helper_in_volume(
            "device-volume",
            "revoked-child",
            Some("revoked"),
            "child.txt",
            false,
        );
        let child_uid = child_node.uid.clone();
        let child_ino = state.intern(revoked_ino, child_node);
        state
            .children
            .get_mut(&super::ROOT_INO)
            .unwrap()
            .push(revoked_ino);
        state.children.insert(revoked_ino, vec![child_ino]);

        let state = std::sync::Arc::new(parking_lot::Mutex::new(state));
        registry.register_bare(
            std::path::Path::new("/mnt/device"),
            &state,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            session_flag(true),
        );
        assert!(registry.owns_visible_uid(&open_uid));
        assert!(registry.owns_visible_uid(&child_uid));

        state.lock().unlink_mem(&open_uid);
        assert!(
            !registry.owns_visible_uid(&open_uid),
            "an open inode retained after unlink is not addressable"
        );

        state.lock().hide_shared_root(&revoked_uid);
        assert!(!registry.owns_visible_uid(&revoked_uid));
        assert!(
            !registry.owns_visible_uid(&child_uid),
            "a retained descendant beneath a revoked root is not reachable"
        );
    }

    #[test]
    fn state_registry_rejects_foreign_broken_and_cyclic_residents() {
        let registry = StateRegistry::default();
        let (mut state, _dir) = rooted_state("own-volume", "root");

        let foreign_node =
            node_helper_in_volume("foreign-volume", "shared", Some("root"), "shared", true);
        let foreign_uid = foreign_node.uid.clone();
        let foreign_ino = state.intern(super::ROOT_INO, foreign_node);

        let broken_node =
            node_helper_in_volume("own-volume", "broken", Some("root"), "broken", true);
        let broken_uid = broken_node.uid.clone();
        let _broken_ino = state.intern(super::ROOT_INO, broken_node);

        let cycle_a_node = node_helper_in_volume("own-volume", "cycle-a", Some("root"), "a", true);
        let cycle_a_uid = cycle_a_node.uid.clone();
        let cycle_a = state.intern(super::ROOT_INO, cycle_a_node);
        let cycle_b_node =
            node_helper_in_volume("own-volume", "cycle-b", Some("cycle-a"), "b", true);
        let cycle_b = state.intern(cycle_a, cycle_b_node);
        let cycle_b_uid = state.entries[&cycle_b].uid.clone();
        let cycle_a_entry = state.entries.get_mut(&cycle_a).unwrap();
        cycle_a_entry.parent = cycle_b;
        cycle_a_entry.node.parent_uid = Some(cycle_b_uid);

        state.children.insert(super::ROOT_INO, vec![foreign_ino]);
        state.children.insert(cycle_a, vec![cycle_b]);
        state.children.insert(cycle_b, vec![cycle_a]);

        let state = std::sync::Arc::new(parking_lot::Mutex::new(state));
        registry.register_bare(
            std::path::Path::new("/mnt/device"),
            &state,
            std::sync::Arc::new(std::sync::OnceLock::new()),
            session_flag(true),
        );

        assert!(
            !registry.owns_visible_uid(&foreign_uid),
            "incoming-share residents are outside the mounted root's volume"
        );
        assert!(
            !registry.owns_visible_uid(&broken_uid),
            "an interned node without a visible parent dentry is not reachable"
        );
        assert!(
            !registry.owns_visible_uid(&cycle_a_uid),
            "a cyclic resident parent chain is not reachable"
        );
    }

    #[test]
    fn resolve_anywhere_prefers_resident_state_then_falls_back_to_mirror() {
        let resident = resolve_anywhere_with(
            "vol~resident",
            |_| true,
            |_| panic!("resident uid must not query mirror bookkeeping"),
        )
        .unwrap();
        assert_eq!(resident.to_string(), "vol~resident");

        let mirror = resolve_anywhere_with("vol~mirror", |_| false, |_| Ok(true)).unwrap();
        assert_eq!(mirror.to_string(), "vol~mirror");
    }

    #[test]
    fn resolve_anywhere_classifies_invalid_missing_and_db_errors() {
        for raw_uid in ["not-a-uid", "~link", "vol~", "vol~link~extra"] {
            let invalid = resolve_anywhere_with(
                raw_uid,
                |_| panic!("invalid uid must not query live state"),
                |_| panic!("invalid uid must not query mirror bookkeeping"),
            )
            .unwrap_err();
            assert_eq!(
                invalid.kind,
                pdfs_core::control::ErrorKind::Invalid,
                "{raw_uid}"
            );
        }

        for raw_uid in ["local~pending", "virtual~sharedwithme"] {
            let reserved = resolve_anywhere_with(
                raw_uid,
                |_| panic!("reserved uid must not query live state"),
                |_| panic!("reserved uid must not query mirror bookkeeping"),
            )
            .unwrap_err();
            assert_eq!(
                reserved.kind,
                pdfs_core::control::ErrorKind::Invalid,
                "{raw_uid}"
            );
        }

        let missing = resolve_anywhere_with("vol~missing", |_| false, |_| Ok(false)).unwrap_err();
        assert_eq!(missing.kind, pdfs_core::control::ErrorKind::NotFound);

        let db_error = resolve_anywhere_with(
            "vol~db",
            |_| false,
            |_| Err(pdfs_core::Error::Other("lookup failed".into())),
        )
        .unwrap_err();
        assert_eq!(db_error.kind, pdfs_core::control::ErrorKind::Internal);
        assert!(db_error.message.contains("lookup failed"));
    }

    /// An event feed carries no revision id, so the only thing that tells a
    /// remote change apart from the echo of one this daemon just made is this
    /// bookkeeping. Getting it wrong is expensive in both directions: too eager
    /// and we evict the bytes we uploaded and re-download them from an API that
    /// may still be serving the previous revision (a SQLite file on the mount
    /// read back `malformed`); too generous and a real change from another device
    /// is ignored.
    #[test]
    fn a_self_change_is_claimed_once_and_expires() {
        let mut changes = HashMap::new();
        let uid = parse_node_uid("vol~link").unwrap();
        let other = parse_node_uid("vol~other").unwrap();
        let t0 = 1_000_000;

        assert!(
            !take_self_change(&mut changes, &uid, t0),
            "a node we never touched is never ours"
        );

        note_self_change(&mut changes, &uid, t0);
        assert!(
            take_self_change(&mut changes, &uid, t0 + 5_000),
            "the echo of our own change is claimed"
        );
        assert!(
            !take_self_change(&mut changes, &uid, t0 + 6_000),
            "and only once, so a later foreign change still applies"
        );

        // A create followed by a revision upload is two changes to one node, and
        // the feed reports both.
        note_self_change(&mut changes, &uid, t0);
        note_self_change(&mut changes, &uid, t0);
        assert!(take_self_change(&mut changes, &uid, t0 + 1));
        assert!(take_self_change(&mut changes, &uid, t0 + 2));
        assert!(!take_self_change(&mut changes, &uid, t0 + 3));

        // Past the window the attribution is a guess, and a wrong guess leaves
        // the user looking at a stale file.
        note_self_change(&mut changes, &uid, t0);
        assert!(!take_self_change(
            &mut changes,
            &uid,
            t0 + SELF_CHANGE_TTL_MS
        ));
        assert!(
            !changes.contains_key(&uid),
            "an expired change is dropped, not left to be re-tested"
        );

        // An echo that never arrives must not accumulate for the life of the
        // daemon; the next recorded change prunes it.
        note_self_change(&mut changes, &uid, t0);
        note_self_change(&mut changes, &other, t0 + SELF_CHANGE_TTL_MS);
        assert_eq!(
            changes.len(),
            1,
            "recording a change prunes the ones whose echo never came"
        );
    }

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

    fn pending_revision(dir: &TestDir, bytes: &[u8], complete: bool) -> PendingRevision {
        let path = dir.0.join("pending");
        std::fs::write(&path, bytes).unwrap();
        PendingRevision {
            path,
            meta: StagedWrite {
                uid: "vol~file".into(),
                len: bytes.len() as u64,
                base_size: 3,
                base_mtime: 42,
                authored: if complete {
                    vec![(0, bytes.len() as u64)]
                } else {
                    vec![(3, bytes.len() as u64)]
                },
                complete,
                based_on: Some(Baseline {
                    mtime: 42,
                    size: 3,
                    hash: None,
                    revision_id: None,
                }),
            },
        }
    }

    #[test]
    fn complete_pending_revision_can_seed_immediate_shrink() {
        let (_, dir) = state_test_helper();
        let pending = pending_revision(&dir, b"0123456789", true);
        let destination_path = dir.0.join("shrink");
        let destination = std::fs::File::create(&destination_path).unwrap();
        copy_pending_for_truncate(&pending, &destination).unwrap();
        destination.set_len(4).unwrap();
        assert_eq!(std::fs::read(destination_path).unwrap(), b"0123");
    }

    #[test]
    fn complete_pending_revision_can_seed_immediate_growth_and_zero() {
        let (_, dir) = state_test_helper();
        let pending = pending_revision(&dir, b"0123456789", true);
        let destination_path = dir.0.join("grow");
        let destination = std::fs::File::create(&destination_path).unwrap();
        copy_pending_for_truncate(&pending, &destination).unwrap();
        destination.set_len(14).unwrap();
        assert_eq!(
            std::fs::read(&destination_path).unwrap(),
            b"0123456789\0\0\0\0"
        );
        destination.set_len(0).unwrap();
        assert!(std::fs::read(destination_path).unwrap().is_empty());
    }

    #[test]
    fn incomplete_pending_revision_is_not_a_safe_truncate_base() {
        let (_, dir) = state_test_helper();
        let pending = pending_revision(&dir, b"\0\0\0authored", false);
        assert!(!pending.meta.complete);
        assert_ne!(pending.meta.authored, vec![(0, pending.meta.len)]);
    }

    #[test]
    fn online_combined_cross_directory_rename_stays_synchronous() {
        assert!(!rename_needs_queue(true, false, true, true));
        assert!(rename_needs_queue(false, false, false, true));
        assert!(rename_needs_queue(true, true, true, false));
    }

    #[test]
    fn one_step_online_namespace_changes_stay_synchronous() {
        assert!(!rename_needs_queue(true, false, false, true));
        assert!(!rename_needs_queue(true, false, true, false));
        assert!(!rename_needs_queue(true, false, false, false));
    }

    #[test]
    fn fuse_names_enforce_linux_component_limits() {
        use std::ffi::OsStr;

        assert_eq!(
            fuse_name(OsStr::new("x".repeat(255).as_str()))
                .unwrap()
                .len(),
            255
        );
        assert_eq!(
            fuse_name(OsStr::new("x".repeat(256).as_str()))
                .expect_err("256-byte component must be refused")
                .code(),
            libc::ENAMETOOLONG
        );
        assert_eq!(
            fuse_name(OsStr::new("."))
                .expect_err("dot is not an ordinary component")
                .code(),
            libc::EINVAL
        );
        assert_eq!(
            fuse_name(OsStr::new("unicodé-文件")).unwrap(),
            "unicodé-文件"
        );
    }

    #[test]
    fn local_trash_tombstones_hide_stale_remote_entries() {
        let folder_uid = NodeUid::new(VolumeId::from("vol"), LinkId::from("folder"));
        let node = node_helper("child", "folder", "child", false);
        assert!(node_visible(
            &node,
            &folder_uid,
            &std::collections::HashSet::new()
        ));
        assert!(!node_visible(
            &node,
            &folder_uid,
            &std::collections::HashSet::from([node.uid.clone()])
        ));
    }

    #[test]
    fn denied_queue_guard_preserves_accepted_bytes_and_pending_count() {
        use std::cell::Cell;

        let (mut state, _dir) = state_test_helper();
        let root = state.intern(0, node_helper("root", "none", "root", true));
        let shared = state.intern(root, node_helper("shared", "root", "shared", true));
        state.entries.get_mut(&shared).unwrap().access = Access::Viewer;
        let uid = state.entries[&shared].uid.clone();
        let db = state.flushed_db();
        let before = db.pending_ops().unwrap().len();

        let preserved = Cell::new(false);
        let result = preserve_on_access_denied(
            require_uid_access(&db, &uid, &[Access::Viewer]),
            true,
            || preserved.set(true),
        );
        assert_eq!(result.unwrap_err().code(), libc::EACCES);
        assert!(preserved.get(), "accepted bytes must enter recovery");
        assert_eq!(db.pending_ops().unwrap().len(), before);

        let untouched = Cell::new(false);
        assert!(preserve_on_access_denied(Ok(()), true, || untouched.set(true)).is_ok());
        assert!(
            !untouched.get(),
            "a permitted write must continue to the normal queue path"
        );

        // The same fail-closed rule applies before hydration and to a stale UID.
        db.set_share_access(&uid, Access::Viewer).unwrap();
        assert_eq!(
            require_uid_access(&db, &uid, &[]).unwrap_err().code(),
            libc::EACCES
        );
        let stale = NodeUid::new(VolumeId::from("vol"), LinkId::from("stale"));
        assert_eq!(
            require_uid_access(&db, &stale, &[]).unwrap_err().code(),
            libc::EACCES
        );
        // Same refusal on the syscall path, but a distinguishable one
        // underneath: the drain asks the remote about `Unknown` rather than
        // waiting on a permission change (B83).
        assert_eq!(
            uid_write_authority(&db, &stale, &[]),
            WriteAuthority::Unknown,
            "a uid the tree has no row for is unknown, not denied"
        );
        assert_eq!(
            uid_write_authority(&db, &uid, &[]),
            WriteAuthority::Denied,
            "a uid the tree knows and refuses is denied"
        );
        assert_eq!(db.pending_ops().unwrap().len(), before);
    }

    #[test]
    fn persisted_and_live_queue_authorities_must_both_be_writable() {
        let (mut state, _dir) = state_test_helper();
        let root = state.intern(0, node_helper("root", "none", "root", true));
        let node = state.intern(root, node_helper("shared", "root", "shared", true));
        let uid = state.entries[&node].uid.clone();
        let db = state.flushed_db();

        db.set_share_access(&uid, Access::Viewer).unwrap();
        assert_eq!(
            require_uid_access(&db, &uid, &[Access::Editor])
                .unwrap_err()
                .code(),
            libc::EACCES,
            "a persisted downgrade must beat stale writable live state"
        );

        db.set_share_access(&uid, Access::Editor).unwrap();
        assert_eq!(
            require_uid_access(&db, &uid, &[Access::Viewer])
                .unwrap_err()
                .code(),
            libc::EACCES,
            "a live downgrade must beat restored writable persisted state"
        );
        assert!(require_uid_access(&db, &uid, &[Access::Editor]).is_ok());
    }

    #[test]
    fn shared_listing_provenance_must_agree_before_editor_is_granted() {
        use proton_drive_rs::ShareMembership;
        use proton_drive_rs::proton_sdk::ids::ShareMembershipId;

        let uid = NodeUid::new(VolumeId::from("foreign"), LinkId::from("root"));
        let share_a = ShareId::from("share-a");
        let share_b = ShareId::from("share-b");
        let parent = shared_with_me_uid();
        let item = |share_id: ShareId| SharedWithMeItem {
            uid: uid.clone(),
            share_id,
        };
        let node = |share_id: Option<ShareId>, permissions: i32| {
            let mut node = node_helper("root", "none", "Shared", true);
            node.uid = uid.clone();
            node.membership = share_id.map(|share_id| ShareMembership {
                share_id,
                membership_id: ShareMembershipId::from("membership"),
                permissions,
            });
            node
        };

        let accepted =
            accepted_share_provenance(vec![item(share_a.clone()), item(share_a.clone())]);
        assert_eq!(
            prepare_shared_roots(&accepted, vec![node(Some(share_a.clone()), 6)], &parent)[0]
                .access,
            Access::Editor
        );
        for materialized in [
            node(None, 6),
            node(Some(share_b.clone()), 6),
            node(Some(share_a.clone()), 12345),
        ] {
            assert_eq!(
                prepare_shared_roots(&accepted, vec![materialized], &parent)[0].access,
                Access::Viewer
            );
        }

        let conflicting = accepted_share_provenance(vec![item(share_a), item(share_b)]);
        assert_eq!(
            prepare_shared_roots(
                &conflicting,
                vec![node(Some(ShareId::from("share-a")), 6)],
                &parent
            )[0]
            .access,
            Access::Viewer,
            "duplicate UID provenance disagreement must fail closed"
        );
    }

    #[test]
    fn virtual_bulk_upload_destination_is_denied_by_runtime_authority() {
        let (mut state, _dir) = state_test_helper();
        let root = state.intern(0, node_helper("root", "none", "My Files", true));
        let mut virtual_node = node_helper("sharedwithme", "root", "Shared with me", true);
        virtual_node.uid = shared_with_me_uid();
        state
            .flushed_db()
            .set_share_access(&virtual_node.uid, Access::Viewer)
            .unwrap();
        state
            .share_access
            .insert(virtual_node.uid.clone(), Access::Viewer);
        let virtual_ino = state.intern(root, virtual_node);
        let uid = state.entries[&virtual_ino].uid.clone();
        assert_eq!(
            require_uid_access(&state.db, &uid, &[Access::Viewer])
                .unwrap_err()
                .code(),
            libc::EACCES
        );
    }

    fn function_source<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source.find(signature).expect("function signature exists");
        let body = &source[start..];
        let open = body.find('{').expect("function body starts");
        let mut depth = 0usize;
        for (offset, byte) in body.as_bytes()[open..].iter().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &body[..open + offset + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("function body closes")
    }

    fn assert_before(source: &str, first: &str, second: &str) {
        let first = source.find(first).expect("first operation exists");
        let second = source.find(second).expect("second operation exists");
        assert!(first < second, "{first} must precede {second}");
    }

    #[test]
    fn production_queue_guards_precede_mutating_work() {
        let source = include_str!("lib.rs");

        let revision = function_source(source, "fn queue_revision(");
        assert_before(revision, "if !h.dirty", "preserve_on_access_denied");
        assert_before(revision, "preserve_on_access_denied", "fill_gaps");

        let staged = function_source(source, "fn enqueue_staged_write(");
        assert_before(staged, "preserve_on_access_denied", "self.pending.lock()");
        assert_before(
            staged,
            "preserve_on_access_denied",
            "self.cache.stage_write",
        );

        let truncate = function_source(source, "fn queue_truncate(");
        assert_before(truncate, "require_uid_writable", "self.pending.lock()");
        assert_before(truncate, "require_uid_writable", "create_scratch");

        for (signature, first_side_effect) in [
            ("fn queue_local_node(", "mint_local_uid"),
            ("fn queue_rename(", "queue_rename_authorized"),
            ("fn queue_trash(", "replace_ops_with_trash"),
        ] {
            let function = function_source(source, signature);
            assert_before(function, "require_uid_writable", first_side_effect);
        }
        let trash = function_source(source, "fn queue_trash(");
        assert_before(trash, "replace_ops_with_trash", "discard_staged");

        // §5, parallel drain: every path that unlinks a staged blob or hands
        // the same node's work to another worker has to stop the upload that
        // may be reading those bytes *first*. Late is a superseded revision on
        // the wire, or two workers deciding whether the file exists.
        for (signature, mutation) in [
            ("fn enqueue_staged_write(", "attach_blob_to_create"),
            ("fn queue_trash(", "replace_ops_with_trash"),
            ("fn discard_queued_ops(", "delete_ops_for_uid"),
        ] {
            let function = function_source(source, signature);
            assert_before(function, "cancel_upload", mutation);
        }

        let filesystem = include_str!("filesystem.rs");
        for signature in ["fn serve_unlink(", "fn serve_rmdir("] {
            let function = function_source(filesystem, signature);
            assert_before(function, "require_writable", "lookup_child");
        }

        let rename = function_source(filesystem, "fn serve_rename(");
        assert_before(rename, "require_rename_access", "remove_replaced");
        assert_before(rename, "remove_replaced", "queue_rename_authorized");
        for (signature, gate, remote_call) in [
            ("fn serve_create(", "require_uid_writable", "upload_file"),
            ("fn serve_mkdir(", "require_uid_writable", "create_folder"),
            ("fn trash_child(", "require_uid_writable", "trash_nodes"),
            ("fn serve_rename(", "require_rename_access", "rename_node"),
        ] {
            let function = function_source(filesystem, signature);
            assert_before(function, gate, remote_call);
        }
        let control_mkdir = function_source(source, "fn create_folder(");
        assert_before(
            control_mkdir,
            "require_uid_writable",
            ".create_folder(&parent_uid",
        );
        for (signature, gate, remote_call) in [
            (
                "fn rename(&self, rel: &Path",
                "require_rename_access",
                ".rename_node",
            ),
            (
                "fn move_to(&self, rel: &Path",
                "require_rename_access",
                ".move_node",
            ),
            (
                "fn delete(&self, rel: &Path",
                "require_node_parent_access",
                ".trash_nodes",
            ),
        ] {
            let function = function_source(source, signature);
            assert_before(function, "source_parent_uid", gate);
            assert_before(function, gate, remote_call);
        }

        let upload = include_str!("upload.rs");
        let run_uploads = function_source(upload, "async fn run_uploads(");
        assert_before(run_uploads, "require_uid_writable", ".upload_file_from");
        let bulk = function_source(upload, "pub(super) fn upload_paths(");
        assert_before(bulk, "require_uid_writable", "collect_uploads");
        let folder = function_source(upload, "fn ensure_remote_folder(");
        assert_before(folder, "require_uid_writable(parent_uid)", ".create_folder");
    }

    #[test]
    fn replacement_rename_authorizes_source_and_both_parents() {
        use std::cell::RefCell;

        let source = NodeUid::new(VolumeId::from("vol"), LinkId::from("source"));
        let old_parent = NodeUid::new(VolumeId::from("vol"), LinkId::from("old"));
        let new_parent = NodeUid::new(VolumeId::from("vol"), LinkId::from("new"));
        let checked = RefCell::new(Vec::new());

        require_rename_access(
            |uid| {
                checked.borrow_mut().push(uid.to_string());
                Ok(())
            },
            &source,
            &old_parent,
            &new_parent,
        )
        .unwrap();

        assert_eq!(
            checked.into_inner(),
            vec!["vol~source", "vol~old", "vol~new"]
        );
    }

    #[test]
    fn writable_nested_node_cannot_mutate_a_viewer_parent_namespace() {
        let source = NodeUid::new(VolumeId::from("vol"), LinkId::from("nested-editor"));
        let viewer_parent = NodeUid::new(VolumeId::from("vol"), LinkId::from("viewer-parent"));
        let writable_destination = NodeUid::new(VolumeId::from("vol"), LinkId::from("destination"));
        let access = |uid: &NodeUid| {
            if uid == &viewer_parent {
                Err(Errno::EACCES)
            } else {
                Ok(())
            }
        };

        assert_eq!(
            require_node_parent_access(access, &source, &viewer_parent)
                .unwrap_err()
                .code(),
            libc::EACCES,
            "delete must authorize the source namespace"
        );
        assert_eq!(
            require_rename_access(access, &source, &viewer_parent, &writable_destination,)
                .unwrap_err()
                .code(),
            libc::EACCES,
            "rename and move must authorize the original parent"
        );
    }

    #[test]
    fn access_handler_logic_matches_advertised_permissions() {
        assert!(access_allowed(false, Access::Viewer, AccessFlags::F_OK));
        assert!(access_allowed(false, Access::Viewer, AccessFlags::R_OK));
        assert!(!access_allowed(false, Access::Viewer, AccessFlags::W_OK));
        assert!(!access_allowed(false, Access::Owner, AccessFlags::X_OK));
        assert!(access_allowed(true, Access::Viewer, AccessFlags::X_OK));
        assert!(access_allowed(
            true,
            Access::Editor,
            AccessFlags::R_OK | AccessFlags::W_OK | AccessFlags::X_OK
        ));
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
        let share_access = db.all_share_access().unwrap();
        let st = crate::state::State::new(std::sync::Arc::new(db), share_access, 1);
        (st, TestDir(dir_path))
    }

    fn rooted_state(volume: &str, root_link: &str) -> (crate::state::State, TestDir) {
        let (mut state, dir) = state_test_helper();
        let root = node_helper_in_volume(volume, root_link, None, "root", true);
        let root_ino = state.intern(0, root);
        assert_eq!(root_ino, super::ROOT_INO);
        state.entries.get_mut(&root_ino).unwrap().parent = root_ino;
        (state, dir)
    }

    fn node_helper_in_volume(
        volume: &str,
        id: &str,
        parent: Option<&str>,
        name: &str,
        is_dir: bool,
    ) -> Node {
        let mut node = node_helper(id, "none", name, is_dir);
        node.uid = NodeUid::new(VolumeId::from(volume), LinkId::from(id));
        node.parent_uid =
            parent.map(|parent| NodeUid::new(VolumeId::from(volume), LinkId::from(parent)));
        node
    }

    #[test]
    fn queued_trash_of_open_file_survives_last_release_until_atomic_completion() {
        let (mut state, _dir) = state_test_helper();
        let parent = state.intern(0, node_helper("parent", "none", "parent", true));
        let file_node = node_helper("open-trash", "parent", "dirty.txt", false);
        let uid = file_node.uid.clone();
        let ino = state.intern(parent, file_node);
        state.children.insert(parent, vec![ino]);
        state.entries.get_mut(&ino).unwrap().open_count = 1;

        // The typed DB operation atomically replaces the old revision with the
        // one intent that must survive the open handle's final release.
        state
            .flushed_db()
            .enqueue_op(&PendingOp {
                id: 0,
                kind: OP_REVISION.to_string(),
                uid: uid.to_string(),
                parent_uid: None,
                name: None,
                blob_path: None,
                meta_json: None,
                created_at: 1,
                attempts: 0,
                last_error: None,
                next_attempt_at: 0,
            })
            .unwrap();
        let (_, blobs) = state
            .flushed_db()
            .replace_ops_with_trash(&uid.to_string(), "dirty.txt", 2)
            .unwrap();
        assert!(blobs.is_empty());
        let queued_counts = state.flushed_db().pending_op_counts().unwrap();

        state.unlink_mem(&uid);
        assert!(state.entries[&ino].unlinked);
        assert!(!state.children[&parent].contains(&ino));
        assert!(
            state
                .flushed_db()
                .node_by_uid(&uid.to_string())
                .unwrap()
                .is_some()
        );

        let released = release_unlinked_entry(&mut state, ino).unwrap();
        assert_eq!(released, uid);
        assert!(!state.by_uid.contains_key(&uid));
        assert!(release_must_retain_queued_trash(&state.db, &uid).unwrap());
        assert!(
            state
                .flushed_db()
                .node_by_uid(&uid.to_string())
                .unwrap()
                .is_some(),
            "last release keeps persisted drain authority"
        );
        let pending = state.flushed_db().pending_ops().unwrap();
        assert_eq!(pending.len(), 1, "release must not add a revision");
        assert_eq!(pending[0].kind, OP_TRASH);
        let after_release = state.flushed_db().pending_op_counts().unwrap();
        assert_eq!(after_release.uploads, 0);
        assert_eq!(after_release.changes, 1);
        assert_eq!(after_release.uploads, queued_counts.uploads);
        assert_eq!(after_release.changes, queued_counts.changes);

        state
            .flushed_db()
            .complete_trash_op(pending[0].id, &uid)
            .unwrap();
        assert!(state.flushed_db().pending_ops().unwrap().is_empty());
        assert!(
            state
                .flushed_db()
                .node_by_uid(&uid.to_string())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn queued_trash_lookup_failure_conservatively_retains_unlinked_state() {
        let (mut state, _dir) = state_test_helper();
        let uid = NodeUid::new(VolumeId::from("vol"), LinkId::from("uncertain-trash"));
        state
            .flushed_db()
            .with_conn(|conn| {
                conn.execute_batch("DROP TABLE pending_op")?;
                Ok(())
            })
            .unwrap();

        assert!(release_must_retain_queued_trash(&state.db, &uid).is_err());
        assert!(
            !release_can_discard_unlinked(&state.db, &uid),
            "database uncertainty must retain the tombstone and authority row"
        );
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
                    active_revision_id: None,
                    content_sha1: None,
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
            membership: None,
            photo: None,
            album: None,
            verification: Default::default(),
        }
    }
}

#[cfg(test)]
mod thumbnail_miss_tests {
    use super::*;

    #[test]
    fn remote_misses_never_suppress_local_generation() {
        let uid = parse_uid("volume~node").expect("uid");
        let key = (uid, ThumbnailType::Thumbnail.as_i32());
        let mut misses = ThumbnailMissCaches::default();

        misses.remember_remote(key.clone(), 42);
        assert!(misses.remote_contains(&key, 42));
        assert!(!misses.local_contains(&key, 42));

        misses.remember_local(key.clone(), 42);
        assert!(misses.local_contains(&key, 42));
        misses.forget_local(&key);
        assert!(!misses.local_contains(&key, 42));
        assert!(misses.remote_contains(&key, 42));
    }

    #[test]
    fn local_miss_cache_is_bounded_on_gui_only_paths() {
        let mut misses = ThumbnailMissCaches::default();
        for index in 0..=MAX_THUMBNAIL_MISSES {
            let uid = parse_uid(&format!("volume~node-{index}")).expect("uid");
            misses.remember_local((uid, ThumbnailType::Thumbnail.as_i32()), 42);
        }
        assert_eq!(misses.local.len(), 1);
    }
}

#[cfg(test)]
mod search_root_tests {
    use super::*;

    fn roots(pairs: &[(&str, &str)]) -> SearchRoots {
        SearchRoots {
            roots: pairs
                .iter()
                .map(|(root, local)| ((*root).to_string(), PathBuf::from(local)))
                .collect(),
        }
    }

    #[test]
    fn a_hit_below_a_root_resolves_through_it() {
        let roots = roots(&[("Videos/Anime", "/home/u/anime")]);
        assert_eq!(
            roots.resolve("Videos/Anime/s01/e01.mkv").as_deref(),
            Some("/home/u/anime/s01/e01.mkv")
        );
    }

    #[test]
    fn a_hit_outside_every_root_resolves_nowhere() {
        let roots = roots(&[("Videos/Anime", "/home/u/anime")]);
        assert_eq!(roots.resolve("Documents/tax.pdf"), None);
    }

    #[test]
    fn a_sibling_sharing_a_name_prefix_is_not_below_the_root() {
        // `Videos/Anime2` starts with `Videos/Anime` as a string and is a
        // different folder; only a `/` boundary makes it a descendant.
        let roots = roots(&[("Videos/Anime", "/home/u/anime")]);
        assert_eq!(roots.resolve("Videos/Anime2/e01.mkv"), None);
    }

    #[test]
    fn the_most_specific_root_wins() {
        let roots = roots(&[
            ("Videos", "/home/u/videos"),
            ("Videos/Anime", "/home/u/anime"),
        ]);
        assert_eq!(
            roots.resolve("Videos/Anime/e01.mkv").as_deref(),
            Some("/home/u/anime/e01.mkv"),
            "the root leaving the shortest descendant path is the one the user \
             configured for this content"
        );
    }

    #[test]
    fn a_root_covering_the_whole_drive_resolves_everything() {
        let roots = roots(&[("", "/home/u/drive")]);
        assert_eq!(
            roots.resolve("Documents/tax.pdf").as_deref(),
            Some("/home/u/drive/Documents/tax.pdf")
        );
    }

    #[test]
    fn the_root_folder_itself_resolves_to_its_local_directory() {
        let roots = roots(&[("Videos/Anime", "/home/u/anime")]);
        assert_eq!(
            roots.resolve("Videos/Anime").as_deref(),
            Some("/home/u/anime/")
        );
    }
}
