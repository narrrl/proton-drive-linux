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
use pdfs_core::lockrank::{Rank, Ranked};
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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
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
use pdfs_core::config::{AppDirs, SweepMode};
use pdfs_core::control::{
    ActivityEntry, ActivityKind, DirEntry, ErrorKind, LocalHit, PhotoKind, PublicLinkInfo,
    SearchFilters, SearchHit, SearchSource, SyncFolderInfo, SyncIssue, SyncProgress, Topic,
    TransferDirection,
};
use pdfs_core::localindex;
use pdfs_core::search::relevance_score;
use pdfs_core::syncignore::is_transient_name;
use pdfs_core::{Access, CoreError, CoreResult, access_for, perm_bits};
use pdfs_store::cache::{
    BLOCK_SIZE, Baseline, BlockGeometry, BlockSpan, ContentCache, StagedWrite,
};
use pdfs_store::db::{
    Db, LOCAL_VOLUME, OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH, PARK_UNTIL, PendingOp,
    PublishedSharedRoot, RenameMeta, StoredNode, StoredSyncFolder, StoredTrash,
};
use proton_drive_rs::proton_sdk::api::ResponseCode;
use proton_drive_rs::proton_sdk::error::ProtonError;
use proton_drive_rs::proton_sdk::ids::{DriveEventId, LinkId, NodeUid, ShareId, VolumeId};
use proton_drive_rs::{
    AvailableNameTarget, DriveEvent, DriveEventScopeId, MemberRole, Node, NodeKind, NodeMoveItem,
    ProtonDriveClient, ProtonPhotosClient, SharedWithMeItem, ThumbnailType, TrashedNode,
};

mod activity;
mod attach;
mod background;
mod conflicts;
mod control;
mod devices;
mod diagnostics;
mod drain;
mod enqueue;
use drive::{DriveApi, RevisionRead};
use pdfs_core::timestamp::now_secs;
use pdfs_drive as drive;
use pdfs_drive::node_revision_id;
pub(crate) use pdfs_drive::{media_type_for, node_content_sha1, parse_uid, remote_name_is_safe};
#[cfg(test)]
use pdfs_sync::naming::{conflict_label, conflict_label_of, conflict_name_as};
use pdfs_sync::naming::{conflict_name, this_hostname};
mod echoes;
mod events;
mod filesystem;
mod gallery;
mod inhibit;
mod keeper;
mod link;
mod listing;
mod session;
mod writes;
pub use filesystem::ProtonFs;
mod maintenance;
mod mount;
mod pause;
mod photos;
mod pins;
mod profile;
mod queue;
mod quota;
mod reads;
mod recovery;
mod relocate;
mod remote;
mod requests;
mod resolve;
mod revisions;
mod search;
mod services;
mod shared;
mod sharing;
mod sizes;
mod trash;
use pdfs_core::shutdown;
#[cfg(test)]
mod sim;
mod state;
mod supervisor;
mod sweep;
mod sync;
mod systemd;
use pdfs_core::transfers;
mod upload;
mod r#virtual;
mod workers;
use background::{forget_and_notify, run_event_sync, run_photos_event_sync};
use link::BATCH_CALL_TIMEOUT;
pub(crate) use mount::is_stale_mount;
pub use mount::prepare_mountpoint;
pub use mount::{MountOptions, MountOutcome, mount};
use mount::{
    SecondaryInsertRejection, SecondaryMount, clear_stale_mount, fuse_connection_id, spawn_session,
};
use reads::STREAM_BYPASS_MIN;
use state::{Entry, Intervals, PendingRevision, State, StateGuard, WriteHandle};
pub use systemd::{
    idle_sleep, status as notify_status, stretch_watchdog_for_startup, wait_for_restart,
};
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
const POLL_INTERVAL: Duration = pdfs_sync::EVENT_POLL_INTERVAL;
/// Retry backoff for a queued upload, doubling per attempt between these. The
/// floor is short because the common failure is a brief network blip; the
/// ceiling keeps a persistently failing op from spinning.
const DRAIN_BACKOFF_MIN: Duration = Duration::from_secs(2);
const DRAIN_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// How long the drain worker sleeps when it has nothing due. It is woken
/// directly on a new write or a reconnect, so this only bounds how late a
/// backoff can fire.
const DRAIN_IDLE_POLL: Duration = Duration::from_secs(30);
/// How long the mount waits before asking again for a node it has just created
/// when Drive does not list it yet (`Core::fetch_minted_node`).
const MINTED_READ_BACK_DELAYS: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];
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
/// claim query, not by the thread count — so the rest is throughput. Most ops
/// spend their time waiting: a create waits for Drive to list the node it made
/// before it reads it back. Three workers left a thousand small creates taking
/// 0.3 s each on a LAN and 2.5 s on Wi-Fi, one after another. Uploads cannot
/// crowd each other out by number: the SDK caps the blocks in flight across
/// the client.
const DRAIN_WORKERS: usize = 16;

/// How long a staged blob that names no node is kept before it is retired.
///
/// Staging is exempt from the cache budget because it holds the only copy of
/// writes that have not been uploaded, so nothing there is ever evicted to
/// reclaim space. That is right for a blob that still describes something. It is
/// wrong for one whose sidecar is missing or names a `local~` placeholder whose
/// create is gone: no future run can address it, no UI accounts for it, and it
/// occupies disk for the life of the machine. Real instance: 95 such blobs,
/// ~445 MiB, the oldest two months old, re-announced on every mount.
///
/// A month is far longer than any transfer or any plausible "I will look at that
/// later", and the retirement is announced per blob with its size, so the one
/// case that matters — bytes a human still wants — has a month of warnings and a
/// path to go and fetch them from.
const STAGING_ORPHAN_RETAIN: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// First and longest delay between probes for the network coming back after an
/// offline mount. Doubles from min to max: a laptop shut in
/// a bag is the common case, so the steady state must be cheap, while a brief
/// blip should still recover in seconds.
const ONLINE_PROBE_MIN: Duration = Duration::from_secs(5);
const ONLINE_PROBE_MAX: Duration = Duration::from_secs(300);
/// How long the persisted photos timeline stays good before a page request
/// revalidates it. The SDK hands back the whole timeline at once, so it is stored
/// in the DB and every page is sliced from there; a stale one is still served
/// immediately and refreshed in the background.
///
/// Short, because the cost of a stale timeline is a photo the user deleted on
/// their phone still sitting in the gallery, and the cost of the check is one
/// listing call that only runs when somebody is actually looking at the page.
const TIMELINE_TTL: Duration = Duration::from_secs(60);
/// How many photo nodes are resolved per [`ProtonPhotosClient::enumerate_nodes`]
/// call when enriching a refreshed timeline with names and media types (for the
/// Photos / Videos / Raw split). Batched so a large library is a handful of
/// round-trips rather than one request per photo, and bounded so a single call
/// never asks the server to decrypt the whole library at once.
///
/// 150 because that is the SDK's own `MAX_BATCH_COUNT`: a larger chunk is split
/// there anyway, into one full request and a ragged tail.
const TIMELINE_ENRICH_CHUNK: usize = 150;
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
/// How long one server call of a trash refresh may take before the refresh
/// gives up.
///
/// Refreshes are single-flight, so a call that never returns wedges the trash
/// listing for the life of the daemon: every later request joins the stuck
/// refresh, times out, and answers with the listing from before it. That hid
/// a just-trashed folder from a permanent delete for as long as the daemon ran
/// (`docs/BUGS.md` B103). One call is one request, so this is generous.
const TRASH_CALL_TIMEOUT: Duration = Duration::from_secs(120);

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
/// Set when a trash refresh finishes and cleared when one starts, so it is only
/// present while the stored listing is the whole trash as of some moment. A node
/// that a refresh finds and that listing lacks went to the trash since then; one
/// found after an interrupted refresh may have been there all along. Unlike
/// [`TRASH_SYNCED_MS`], our own mutations leave it alone.
const TRASH_COMPLETE_MS: &str = "trash_complete_ms";
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
/// delay the waiters this chunking exists to release (`docs/BUGS.md` B14).
const SIZE_UPGRADE_CHUNK: usize = 150;

/// How many folders may have a size upgrade in flight at once.
///
/// Each one owns a thread and an API request stream, and the single-flight only
/// deduplicates *within* a folder — so without this a recursive listing scaled
/// both with the number of folders it walked. Eight keeps an interactive `ls -l`
/// (one folder, sometimes a couple) entirely unaffected while putting a ceiling
/// on the recursive case. A folder past the cap is queued, not dropped: one of
/// the running threads takes it over when its own batch ends (`docs/BUGS.md` B100).
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
    /// Drive as the sync core uses it: listings, events, creates, uploads,
    /// downloads, renames, moves and trashes. The real client in production, an
    /// in-memory Drive in the simulation tests. See [`drive`].
    drive: Arc<dyn DriveApi>,
    /// What [`Core::drive`] has been asked, for the diagnostics report.
    drive_calls: Arc<pdfs_drive::counted::CallStats>,
    /// The concrete client, for what [`DriveApi`] does not cover: sharing,
    /// Photos, devices, invitations and public links.
    client: ProtonDriveClient,
    gallery: Option<Arc<gallery::Gallery>>,
    rt: tokio::runtime::Handle,
    /// The daemon's My Files root. Forked on-demand sessions retain this value
    /// so ownership checks never mistake their own mount root for account scope.
    primary_root_uid: NodeUid,
    /// What belongs to one mount's FUSE session rather than to the daemon: whether
    /// it is the primary My Files mount, the tree inode it is rooted at, its
    /// kernel channel and whether the session is up.
    ///
    /// Every other part of `Core` is shared between the primary mount and its
    /// on-demand forks. A fork gets fresh cells from [`SessionCells::fork`].
    session: session::SessionCells,
    /// The one tree every mount of the daemon serves from.
    state: Arc<Ranked<State>>,
    cache: Arc<ContentCache>,
    /// What the read path keeps between reads: open revision readers, decrypted
    /// blocks, fetches in flight and prefetch. See [`reads::ReadState`].
    reads: reads::ReadState,
    /// Threads that serve the FUSE handlers which touch the network, so the
    /// session's dispatch loop stays free to answer cheap metadata calls while a
    /// cold read is on the wire. See [`Workers`].
    workers: Arc<Workers>,
    /// Unified SQLite metadata cache: the persistence layer behind the in-memory
    /// `State` maps. Every mutation writes through here, and the maps rehydrate
    /// from it on mount.
    db: Arc<Db>,
    /// Publication of the shared listing, and what invalidates a listing on the
    /// wire or suppresses its next refresh.
    shared: shared::SharedState,
    /// Whether Drive answers, when the link went down and a condvar for its
    /// coming back (see [`link`]).
    link: Arc<link::Link>,
    /// Whether the user paused syncing, and until when. Persisted, so a pause
    /// survives a restart. See [`pause::SyncPause`].
    pause: pause::SyncPause,
    /// The write-back's state: writes accepted from the kernel and not yet sent,
    /// nodes removed here that a stale listing may still return, and the changes
    /// this daemon made that Drive's event feed will echo back.
    ///
    /// The drain, the queue and recovery own it. Everything else, the read path,
    /// listings, the pin keeper and the event feed, asks through the methods
    /// below instead of taking its locks.
    writes: writes::WriteState,
    /// The drain's own bookkeeping: its wake-up, the creates and uploads on
    /// the wire, and what they left behind. See [`drain::DrainState`].
    drain: drain::DrainState,
    /// The pins the keeper owes a check (`keeper`).
    pin_work: Arc<Mutex<keeper::PinWork>>,
    /// The daemon's stop signal, shared with every background loop this `Core`
    /// was cloned into so teardown can end and join them (`docs/BUGS.md` B44).
    shutdown: Arc<shutdown::Shutdown>,
    /// The long-lived threads the mount started, and which are still
    /// running, for diagnostics and the supervisor.
    services: pdfs_core::lifecycle::ServiceBoard,
    /// Whether a trash refresh is running, and the wake-up for requests waiting
    /// on its progress.
    trash_jobs: trash::TrashState,
    /// What the conflict sweep may do, and the copies it already checked this
    /// run.
    sweep: sweep::SweepState,
    /// A mkdir, create, rename or delete through the mount is recorded and
    /// queued, and the drain sends it; the syscall never waits for Drive. Off,
    /// they go to Drive inline while online, as before 3.0.
    local_first: bool,
    /// What the Photos and thumbnail jobs are doing right now. See
    /// [`photos::PhotoJobs`].
    photo_jobs: photos::PhotoJobs,
    /// The last account quota answer, for `statfs(2)`, and whether a refresh
    /// of it is running.
    quota: quota::QuotaCache,
    /// Sharing, public links, invitations and bookmarks. See
    /// [`sharing::Sharing`].
    sharing: sharing::Sharing,
    /// Database maintenance that has been moved off the request thread.
    ///
    /// `VACUUM` and a deep `integrity_check` both hold the single connection for
    /// tens of seconds against a real install, which stops every FUSE handler
    /// that needs the database — so the handler acknowledges the request and a
    /// thread does the work, exactly as the other long control requests already
    /// do. See [`Maintenance`].
    maintenance: maintenance::Maintenance,
    /// The size upgrades running now, and the replies parked on them. Shared
    /// by every mount: the bound that matters is per daemon.
    sizes: sizes::SizeState,
    /// In-flight upload/download progress, served to `GetQueueStatus`. Shared
    /// across the FUSE session and the control-socket task.
    transfers: Arc<TransferRegistry>,
    /// The folder-sync engine's own state, and the handles it shares with the
    /// rest of the daemon.
    sync: sync::SyncParts,
    /// Open [`Request::Subscribe`](pdfs_core::control::Request::Subscribe)
    /// connections, which every change a front-end shows is published to.
    events: Arc<events::EventHub>,
    /// True while the background scanner is rebuilding the local-file index, so
    /// `SearchLocal` can tell a front-end "still indexing" apart from "no match".
    indexing: Arc<AtomicBool>,
    /// Set while a diagnostics report's database probe is still out, so a
    /// database that does not answer costs one stuck thread, not one per report.
    pending_probe: Arc<AtomicBool>,
    /// Secondary FUSE sessions for `ondemand` sync folders, keyed by sync-folder
    /// id. Each is a `ProtonFs` rooted at the folder's remote
    /// node, mounted over its local path, sharing this Core's client/cache/db and
    /// tree but with its own root (`fork_state`). Held so we can unmount on
    /// toggle back to `mirror` and on daemon shutdown. Each entry retains the
    /// FUSE connection id and the fork's exact liveness flag so teardown first
    /// makes that mount unroutable.
    mounts: Arc<Mutex<mount::SecondaryMountRegistry<SecondaryMount>>>,
    /// Every live mount: the primary and one per on-demand fork
    /// ([`Core::fork_state`]), with its mountpoint, root and kernel channel.
    /// Shared by every fork.
    ///
    /// The mounts share one tree, so a change the drain makes is seen by every
    /// one of them. Each forked mount used to keep its nodes in a state of its
    /// own, where the drain never looked: the fork's entry kept its `local~`
    /// placeholder uid forever (`docs/BUGS.md` B74). What is still per mount is
    /// the kernel's cache, which [`Core::for_each_mount`] tells every session
    /// about.
    ///
    /// `Weak`, so an unmounted fork is dropped rather than pinned here.
    states: Arc<StateRegistry>,
}

/// One live mount, as published to the shared registry.
#[derive(Clone)]
struct LiveMount {
    mountpoint: PathBuf,
    /// The mount's kernel channel, empty until its session is spawned.
    notifier: Arc<OnceLock<KernelChannel>>,
    session_live: Arc<AtomicBool>,
    /// The tree inode the mount is rooted at, empty until its session is built.
    root: Arc<OnceLock<u64>>,
}

impl LiveMount {
    fn is_live(&self) -> bool {
        self.session_live.load(Ordering::Acquire)
    }
}

struct MountedState {
    /// Absolute local root of this mount.
    mountpoint: PathBuf,
    /// The same cell as [`Core::notifier`]. `Weak`, so an unmounted fork is
    /// dropped rather than pinned here; a dead entry is reaped on the next
    /// registry walk.
    notifier: std::sync::Weak<OnceLock<KernelChannel>>,
    session_live: Arc<AtomicBool>,
    root: Arc<OnceLock<u64>>,
}

/// Every mount in the daemon, shared by the primary `Core` and every
/// [`Core::fork_state`] clone of it.
///
/// Split out from `Core` so the reap-and-walk rule can be tested on its own: a
/// fork that fails to appear here, or one that lingers after unmount, is exactly
/// the failure `docs/BUGS.md` B74 was.
#[derive(Default)]
struct StateRegistry(Mutex<Vec<MountedState>>);

impl StateRegistry {
    /// Publish a mount and the channel to the session that serves it.
    ///
    /// A mount is registered exactly once by its owner. Session construction
    /// only flips `session_live`; it never republishes the mount.
    fn register(
        &self,
        mountpoint: &Path,
        notifier: &Arc<OnceLock<KernelChannel>>,
        session_live: Arc<AtomicBool>,
        root: Arc<OnceLock<u64>>,
    ) {
        let mut states = self.0.lock();
        states.retain(|m| m.notifier.strong_count() > 0);
        debug_assert!(
            !states
                .iter()
                .any(|mounted| mounted.notifier.ptr_eq(&Arc::downgrade(notifier))),
            "a mount must be registered exactly once"
        );
        states.push(MountedState {
            mountpoint: mountpoint.to_path_buf(),
            notifier: Arc::downgrade(notifier),
            session_live,
            root,
        });
    }

    /// Every registered mount, reaping the entries whose session has gone.
    /// Returns owned handles so callers do not hold the registry lock across
    /// the work.
    fn live(&self) -> Vec<LiveMount> {
        let mut states = self.0.lock();
        states.retain(|m| m.notifier.strong_count() > 0);
        states
            .iter()
            .filter_map(|m| {
                Some(LiveMount {
                    mountpoint: m.mountpoint.clone(),
                    notifier: m.notifier.upgrade()?,
                    session_live: m.session_live.clone(),
                    root: m.root.clone(),
                })
            })
            .collect()
    }

    /// The live session whose local root most specifically covers `path`.
    ///
    /// On-demand roots can be nested below another location. Selecting by the
    /// longest component prefix ensures the nested session wins rather than the
    /// broader primary mount. Callers that need the relative suffix derive it
    /// with `path.strip_prefix(mountpoint)`.
    fn covering(&self, path: &Path) -> Option<LiveMount> {
        self.live()
            .into_iter()
            .filter(|mount| mount.is_live() && path.starts_with(&mount.mountpoint))
            .max_by_key(|mount| mount.mountpoint.components().count())
    }

    fn is_mounted_at(&self, path: &Path) -> bool {
        self.covering(path)
            .is_some_and(|mount| mount.mountpoint == path)
    }

    /// The tree inodes of the sessions that are mounted.
    fn live_roots(&self) -> Vec<u64> {
        self.live()
            .into_iter()
            .filter(LiveMount::is_live)
            .filter_map(|mount| mount.root.get().copied())
            .collect()
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
    /// Register a mount so background work can reach its session. Called once
    /// for the primary mount and once per on-demand fork.
    fn register_state(&self, mountpoint: &Path) {
        self.states.register(
            mountpoint,
            &self.session.notifier,
            self.session.session_live.clone(),
            self.session.root.clone(),
        );
    }

    /// The tree inode this mount is rooted at. [`ROOT_INO`] until the session
    /// is built, which is where a test that builds a bare tree puts its root.
    pub(crate) fn root_ino(&self) -> u64 {
        self.session.root.get().copied().unwrap_or(ROOT_INO)
    }

    /// Whether `uid` names an owned node reachable from the root of a mounted
    /// session. See [`State::owns_visible_uid`].
    fn owns_visible_uid(&self, uid: &NodeUid) -> bool {
        let roots = self.states.live_roots();
        self.state().owns_visible_uid(uid, &roots)
    }

    /// This `Core`, re-rooted onto whichever live mount most specifically covers
    /// the absolute path `abs`, together with `abs` relative to that mount's
    /// root.
    ///
    /// Everything a `Core` holds is per-daemon except the fields
    /// [`Core::fork_state`] replaces, so swapping exactly those turns the
    /// primary `Core` into the fork that owns the path — resolving from the
    /// fork's root and notifying its session. Without this, every
    /// path-addressed control request is resolved from the primary root, so
    /// anything under a secondary on-demand mount is rejected as "not under the
    /// mountpoint" and the user has no way to re-enumerate a folder they can see
    /// (`docs/BUGS.md` B86).
    ///
    /// `None` when no live mount covers `abs`.
    fn rooted_at(&self, abs: &Path) -> Option<(Core, PathBuf)> {
        let mount = self.states.covering(abs)?;
        let rel = abs.strip_prefix(&mount.mountpoint).ok()?.to_path_buf();
        let mut core = self.clone();
        if !Arc::ptr_eq(&mount.notifier, &self.session.notifier) {
            // Another mount, so adopt its per-mount half wholesale. `primary`
            // is a property of the mount, and the only mount this `Core` can
            // be the primary of is the one it was built for.
            core.session = session::SessionCells {
                primary: false,
                notifier: mount.notifier,
                session_live: mount.session_live,
                root: mount.root,
            };
        }
        Some((core, rel))
    }

    /// Take the tree's lock.
    ///
    /// The guard is what applies the mutation's write-throughs, after releasing
    /// the lock — see [`StateGuard`]. Every caller takes it this way; nothing
    /// locks `self.state` directly, or the writes it queues would be applied
    /// only when some later caller happened to take the guard.
    fn state(&self) -> StateGuard<'_> {
        StateGuard::new(self.state.lock(), &self.db)
    }

    /// Run `apply` against the tree, then tell every mounted session about the
    /// access changes it made.
    ///
    /// For the drain and the sync engine, which are per-daemon: a `pending_op`
    /// row records a uid, not the session that queued it, and any mount may be
    /// showing the node.
    fn for_each_state(&self, apply: impl FnOnce(&mut State)) {
        let changed = {
            let mut state = self.state();
            apply(&mut state);
            state.take_access_changes()
        };
        self.notify_access_changes(&changed);
    }

    /// Like [`Core::for_each_state`], but the closure also records what the
    /// kernel needs told into a [`NotifyBatch`], which goes to every mounted
    /// session.
    ///
    /// Recorded rather than sent: notifications are flushed after the State
    /// lock is released, the same rule [`Core::for_each_state`] and
    /// [`Core::flush_access_changes`] follow.
    fn for_each_mount(&self, apply: impl FnOnce(&mut State, &mut NotifyBatch)) {
        let mut batch = NotifyBatch::default();
        apply(&mut self.state(), &mut batch);
        for mount in self.states.live() {
            batch.flush(mount.notifier.get());
        }
    }

    /// Publish access changes accumulated by an intern/root refresh. The State
    /// lock is released before notifying the kernel because notifier callbacks
    /// may synchronously provoke more filesystem work.
    fn flush_access_changes(&self) {
        let changed = self.state().take_access_changes();
        self.notify_access_changes(&changed);
    }

    /// Tell every mounted session that `changed` have new permissions.
    fn notify_access_changes(&self, changed: &[u64]) {
        if changed.is_empty() {
            return;
        }
        for mount in self.states.live() {
            notify_access_changes(mount.notifier.get(), changed);
        }
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
        let live_access: Vec<Access> = self.state().access_by_uid(uid).into_iter().collect();
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

/// Move `uid` to `name` in `new_parent` within one mount, for a rename or move
/// Drive has made, and record what the kernel has to forget. Returns the name
/// the mount knew the node by, and whether the node was moved rather than
/// forgotten, which writes its row through. `None` when the mount does not
/// hold the node.
///
/// The inode is kept wherever the mount shows the new parent, as the mount's
/// own rename keeps it (`State::relocate`). A mount that does not show the new
/// parent only forgets the node; to it, the node is gone. The row stays either
/// way (`docs/BUGS.md` B117).
fn relocate_in_mount(
    st: &mut State,
    notify: &mut NotifyBatch,
    uid: &NodeUid,
    new_parent: &NodeUid,
    name: &str,
    queued: bool,
) -> Option<(String, bool)> {
    let ino = *st.by_uid.get(uid)?;
    let entry = st.entries.get(&ino)?;
    let (from_parent, from_name) = (entry.parent, entry.node.name.clone());
    match st.by_uid.get(new_parent).copied() {
        Some(to_parent) => {
            if queued {
                // Listing again would ask Drive, which still has the old name;
                // the kernel's pages of both folders go instead.
                st.rename_in_place(ino, to_parent, new_parent, name);
                notify.inval_inode(from_parent);
                if to_parent != from_parent {
                    notify.inval_inode(to_parent);
                }
            } else {
                st.relocate(ino, from_parent, to_parent, new_parent, name);
            }
            notify.inval_entry(from_parent, from_name.clone());
            notify.inval_entry(to_parent, name.to_string());
            Some((from_name, true))
        }
        None => {
            st.forget_mem(uid);
            notify.delete(from_parent, ino, from_name.clone());
            Some((from_name, false))
        }
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

/// Notifications accumulated under the State lock. Flushed by
/// [`Core::for_each_mount`] to every session once the lock is released.
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

    /// Send everything recorded to one session. A mount whose session has not
    /// been spawned has nothing to tell the kernel. A session that never looked
    /// a node up answers `ENOENT`, which is ignored like any other refusal.
    fn flush(&self, notifier: Option<&KernelChannel>) {
        let Some(notifier) = notifier else { return };
        for notice in &self.0 {
            let _ = match notice {
                KernelNotice::InvalInode(ino) => notifier.inval_inode(*ino),
                KernelNotice::InvalEntry { parent, name } => {
                    notifier.inval_entry(*parent, OsStr::new(name))
                }
                KernelNotice::Delete {
                    parent,
                    child,
                    name,
                } => notifier.delete(*parent, *child, OsStr::new(name)),
            };
        }
    }
}

/// One FUSE session's notification channel. The tree has one inode space for
/// every mount, and each session knows its own root as [`ROOT_INO`], so this
/// names the root that way and every other inode as it is.
pub(crate) struct KernelChannel {
    notifier: Notifier,
    /// The tree inode of the session's root.
    root: u64,
}

impl KernelChannel {
    fn new(notifier: Notifier, root: u64) -> Self {
        Self { notifier, root }
    }

    pub(crate) fn inval_inode(&self, ino: u64) -> std::io::Result<()> {
        let ino = filesystem::kernel_ino(self.root, ino);
        off_the_runtime(|| self.notifier.inval_inode(ino, 0, 0))
    }

    /// Drop the cached attributes of `ino` but not its page cache.
    fn inval_attrs(&self, ino: u64) -> std::io::Result<()> {
        let ino = filesystem::kernel_ino(self.root, ino);
        off_the_runtime(|| self.notifier.inval_inode(ino, -1, 0))
    }

    pub(crate) fn inval_entry(&self, parent: u64, name: &OsStr) -> std::io::Result<()> {
        let parent = filesystem::kernel_ino(self.root, parent);
        off_the_runtime(|| self.notifier.inval_entry(parent, name))
    }

    fn delete(&self, parent: u64, child: u64, name: &OsStr) -> std::io::Result<()> {
        let parent = filesystem::kernel_ino(self.root, parent);
        let child = filesystem::kernel_ino(self.root, child);
        off_the_runtime(|| self.notifier.delete(parent, child, name))
    }
}

/// Send a notice to the kernel without holding up the runtime.
///
/// The kernel answers an invalidation only once it has the pages' locks, and
/// a read holds those until the daemon answers it. Sent from a runtime worker,
/// the notice blocked that worker in the kernel; when it was the one driving
/// the timers and I/O, nothing drove them any more, so the read's fetch never
/// finished and the daemon hung (`docs/BUGS.md` B130). On a worker the notice
/// hands the worker's tasks to another thread first; anywhere else it is sent
/// as it is.
fn off_the_runtime<T>(notice: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(notice)
        }
        _ => notice(),
    }
}

fn notify_access_changes(notifier: Option<&KernelChannel>, changed: &[u64]) {
    let Some(notifier) = notifier else { return };
    for &ino in changed {
        let _ = notifier.inval_inode(ino);
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
    meta.complete = StagedWrite::covers(&meta.authored, meta.len);
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

/// The size of a staged blob that has outlived [`STAGING_ORPHAN_RETAIN`], or
/// `None` while it is younger than that — or while its age cannot be read at
/// all, which is never grounds for deleting bytes.
fn orphan_past_retention(blob: &Path) -> Option<u64> {
    let meta = std::fs::metadata(blob).ok()?;
    let age = meta.modified().ok()?.elapsed().ok()?;
    (age >= STAGING_ORPHAN_RETAIN).then_some(meta.len())
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
    /// Which staged blob is currently queued for `uid`, if any.
    ///
    /// The path identifies the revision: publication stages a fresh file and
    /// swaps the map entry to point at it, so an unchanged path means the base a
    /// caller sampled earlier is still the base now (`docs/BUGS.md` B32).
    fn pending_blob(&self, uid: &NodeUid) -> Option<PathBuf> {
        self.writes.queued(uid).map(|queued| queued.path)
    }

    /// Nudge the drain worker to re-examine the queue now.
    fn wake_drain(&self) {
        let (lock, cv) = &*self.drain.drain_wake;
        *lock.lock() = true;
        cv.notify_all();
    }

    /// Download a whole file into `out`, registering the transfer so
    /// `GetQueueStatus` can report its progress. `total` is the expected size
    /// (`0` if unknown). Streams through [`download_file_to`] so each block ticks
    /// the progress counter, and nothing is held in memory.
    ///
    /// [`download_file_to`]: ProtonDriveClient::download_file_to
    fn download_file_tracked_to<W: Write + Send>(
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
            .block_on(self.drive.download_file_to(uid, &mut out))?;
        Ok(out.into_inner())
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
        if self
            .photo_jobs
            .thumbnail_misses
            .lock()
            .remote_contains(&key, mtime)
        {
            return Ok(None);
        }
        // A file manager asks for one per file in a folder, on the metadata
        // workers. Offline, or on a stalled link, each one held a worker for
        // the SDK's whole retry budget, and every cold lookup, create and
        // rename on the mount queued behind them (`docs/BUGS.md` B220).
        if !self.is_online() {
            return Ok(None);
        }
        let bytes = self
            .block_on_bounded(self.client.download_thumbnail(&uid, ttype))
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
                self.photo_jobs
                    .thumbnail_misses
                    .lock()
                    .remember_remote(key, mtime);
            }
        }
        Ok(bytes)
    }

    /// A Photos API handle sharing this Core's Drive client and session, so it
    /// reuses the daemon's single authenticated session rather than logging in
    /// again (Proton refresh tokens are single-use). Cheap — the Drive client
    /// is `Clone` over `Arc`-backed state.
    fn photos_api(&self) -> ProtonPhotosClient {
        ProtonPhotosClient::from_drive_client(self.client.clone())
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
    /// only reaches *this* mount's paths. A remote mutation made by the
    /// sync engine needs more than that, for two reasons. The folder it changed
    /// is usually not resident in the tree at the time — a mirror folder
    /// has no FUSE session at all — so there is no inode to name. And the stale
    /// listing that outlives the pass is not a hot-cache map but the `listed`
    /// flag on the folder's DB row, which survives a daemon restart and is what
    /// a later `ensure_children` trusts.
    ///
    /// So the flag is cleared unconditionally, and the resident listing is
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

    /// Drop the persisted photos timeline's freshness stamp, so the next timeline
    /// read fetches rather than serving what it already has.
    fn invalidate_photos(&self) {
        let _ = self.db.photos_invalidate();
    }

    // ---- activity log -----------------------------------------------------
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
        created: link.creation_time,
    }
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

/// The uid a node made on this machine goes by until it lands, so it can be
/// interned, listed and written to before the server has ever heard of it:
/// its row's local id, as [`pdfs_store::db::local_uid`]
/// has it.
fn local_node_uid(lid: i64) -> NodeUid {
    NodeUid::new(VolumeId::from(LOCAL_VOLUME), LinkId::from(lid.to_string()))
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

/// How often [`Core::ensure_children`] takes a listing that changed while it
/// was being published.
const LISTING_ATTEMPTS: usize = 4;

/// How often a rename or move that Drive calls out of date is retried, and how
/// long apart.
const OUT_OF_DATE_RETRIES: u32 = 20;
const OUT_OF_DATE_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Run a remote rename or move, retrying while Drive answers
/// `InvalidRequirements`.
///
/// Both send the node's current name hash. The SDK takes it from its entity
/// cache, which still holds the old hash when the node changed since it was
/// cached, and Drive then rejects the request as "out of date" (docs/BUGS.md
/// B111, B121). `wait` drops that entry before the next attempt. Any other
/// error returns at once.
fn retry_out_of_date(
    mut call: impl FnMut() -> Result<(), ProtonError>,
    mut wait: impl FnMut(),
) -> Result<(), ProtonError> {
    let mut attempts = 0u32;
    loop {
        match call() {
            Err(e)
                if api_code(&e) == Some(ResponseCode::InvalidRequirements)
                    && attempts < OUT_OF_DATE_RETRIES =>
            {
                attempts += 1;
                debug!(attempts, error = %e, "Drive called the request out of date; retrying");
                wait();
            }
            result => return result,
        }
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
/// The error for a folder Drive created but did not hand back, where there is
/// no queue to adopt it later. Saying it was created keeps the caller from
/// taking a retry's "already exists" for someone else's folder.
fn minted_but_unlisted(name: &str) -> CoreError {
    CoreError::remote(format!(
        "created folder {name}, but Drive did not list it yet; it shows on the next refresh"
    ))
}

fn is_gone(e: &(dyn std::error::Error + 'static)) -> bool {
    api_code(e) == Some(ResponseCode::DoesNotExist)
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
        direct_role: None,
        share_id: None,
    }
}

/// Whether `dir` has no entries.
fn dir_is_empty(dir: &Path) -> std::io::Result<bool> {
    Ok(std::fs::read_dir(dir)?.next().is_none())
}

/// Delete everything inside `dir` but keep `dir` itself (it stays as the FUSE
/// mountpoint). Used when a `mirror` folder flips to `ondemand`: the local files
/// are the disk we're reclaiming.
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
fn sync_folder_info(
    f: StoredSyncFolder,
    progress: Option<SyncProgress>,
    paused: bool,
) -> SyncFolderInfo {
    SyncFolderInfo {
        id: f.id,
        kind: f.kind,
        local_path: f.local_path,
        remote_uid: f.remote_uid,
        mode: f.mode,
        pending_mode: f.pending_mode,
        state: f.state,
        last_sync: f.last_sync,
        progress,
        paused,
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

/// Whether a `rename` may replace an existing destination, per POSIX.
///
/// Split out from the handler because it is the part that is pure and the part
/// that is dangerous: every `Err` here is a refusal that happens *before*
/// anything is trashed, and getting one wrong turns a refusal into the
/// destruction of the destination (`docs/BUGS.md` B13).
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

/// Whether `node` is a file Drive lists without an active revision, as it does
/// one whose create has not been committed.
fn is_unrevised(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::File {
            active_revision_id: None,
            ..
        }
    )
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

/// `call`, failed with a remote error once it has run for
/// [`TRASH_CALL_TIMEOUT`].
async fn within_trash_call<T>(what: &str, call: impl Future<Output = T>) -> CoreResult<T> {
    tokio::time::timeout(TRASH_CALL_TIMEOUT, call)
        .await
        .map_err(|_| {
            CoreError::new(
                ErrorKind::Remote,
                format!("{what}: no answer within {}s", TRASH_CALL_TIMEOUT.as_secs()),
            )
        })
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
/// Local-first, offline, or into a not-yet-uploaded directory, the API is not
/// called in the syscall. Only remote-to-remote renames sent inline
/// ([`Core::sends_inline`]) stay synchronous, so a successful kernel reply means
/// the remote namespace has already reached the same end state.
fn rename_needs_queue(
    inline: bool,
    destination_is_local: bool,
    _parent_changed: bool,
    _name_changed: bool,
) -> bool {
    !inline || destination_is_local
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
    if !remote_name_is_safe(&node.name) {
        warn!(uid = %node.uid, "a remote name cannot be a local file name; leaving it out");
        return false;
    }
    !node.trashed && node.uid != *folder_uid && !hidden.contains(&node.uid)
}

/// The children `entries` hold for each folder in `listed`, gathered in one
/// pass over them.
///
/// The root is its own parent (that is what `..` resolves to), so it would
/// count as its own child; a directory listed inside itself makes the kernel
/// fail the lookup with EIO, taking the whole listing down with it.
///
/// A pass per listed folder took 1.7 s on a tree of 37,000 nodes with 4,600 of
/// them listed, at every start before the mount came up (`docs/BUGS.md` B167).
fn listed_children(entries: &HashMap<u64, Entry>, listed: Vec<u64>) -> HashMap<u64, Vec<u64>> {
    let mut children: HashMap<u64, Vec<u64>> =
        listed.into_iter().map(|ino| (ino, Vec::new())).collect();
    for (&ino, entry) in entries {
        if ino != entry.parent
            && !entry.node.trashed
            && let Some(kids) = children.get_mut(&entry.parent)
        {
            kids.push(ino);
        }
    }
    children
}

/// Lay what is queued for a folder over Drive's listing of it, from
/// `Db::queued_children`: a node a queued rename moved away is dropped, and a
/// node a queued create, mkdir or rename put there is listed as the DB has it.
///
/// That node holds its name here: a create Drive already has but whose answer
/// is still on its way, or which the drain is still reading back, is listed by
/// Drive under its new uid and by the queue under its stand-in. Both were
/// listed, and a 128-file folder listed 129 (`docs/BUGS.md` B164).
fn overlay_queued(nodes: &mut Vec<Node>, queued: Vec<Node>, gone: &HashSet<String>) {
    let here: HashSet<String> = queued.iter().map(|node| node.uid.to_string()).collect();
    let names: HashSet<&str> = queued.iter().map(|node| node.name.as_str()).collect();
    nodes.retain(|node| {
        let uid = node.uid.to_string();
        !gone.contains(&uid) && !here.contains(&uid) && !names.contains(node.name.as_str())
    });
    nodes.extend(queued);
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
    use pdfs_store::cache::{Baseline, StagedWrite};
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
            complete: StagedWrite::covers(authored, len),
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
mod replace_tests {
    use super::check_replaceable;

    /// The case that motivated all of this: rsync renaming its temp file over
    /// the real one. Two plain files, and it has to be allowed — refusing is
    /// what made every rsync transfer fail at the last step (`docs/BUGS.md` B13).
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
    fn a_local_node_uid_is_recognisable_and_round_trips() {
        let uid = local_node_uid(42);
        assert!(is_local_uid(&uid));
        assert!(is_local_uid_str(&uid.to_string()));
        assert_eq!(uid.to_string(), pdfs_store::db::local_uid(42));

        // It has to survive the trip through `pending_op.uid` as text, like any
        // other uid does, and give back the row it stands for.
        let parsed = parse_node_uid(&uid.to_string()).expect("parses back");
        assert_eq!(parsed, uid);
        assert_eq!(pdfs_store::db::local_lid(&uid.to_string()), Some(42));
    }

    #[test]
    fn local_node_uids_are_distinct_per_row() {
        // Two files created in the same millisecond must not collide — the whole
        // queue is keyed by uid.
        assert_ne!(local_node_uid(1), local_node_uid(2));
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
        let node = local_node(local_node_uid(7), parent.clone(), "notes.txt".into(), false);

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
        let node = local_node(local_node_uid(7), parent, "photos".into(), true);
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
            direct_role: None,
            share_id: None,
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
    use super::echoes::{SELF_CHANGE_TTL_MS, note_self_change, take_self_change};
    use super::trash::{expand_restore, first_seen_in_trash};
    use super::{
        Access, AccessFlags, Errno, HashMap, Intervals, PendingRevision, RootListingSnapshot,
        ShareId, SharedWithMeItem, StateRegistry, VirtualRootPlan, accepted_share_provenance,
        conflict_label, conflict_label_of, conflict_name, conflict_name_as,
        copy_pending_for_truncate, fuse_name, is_stale_mount, node_visible, off_the_runtime,
        orphan_past_retention, parse_node_uid, prepare_shared_roots, preserve_on_access_denied,
        publish_virtual_root_in_listing, reconcile_virtual_root_in_listing,
        release_can_discard_unlinked, release_must_retain_queued_trash, release_unlinked_entry,
        rename_needs_queue, require_node_parent_access, require_rename_access,
        resolve_anywhere_with, shared_with_me_uid, uid_write_authority, virtual_node,
    };
    use super::{Db, WriteAuthority};
    use std::time::Duration;

    /// Staged bytes that name no node are kept for a month, because the only
    /// copy of a write must outlive the bug that stranded it. Past that, one
    /// boot retires them instead of re-announcing them every boot forever.
    #[test]
    fn a_stranded_blob_is_retired_only_once_it_outlives_retention() {
        let dir = std::env::temp_dir().join(format!("pdfs-orphan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blob = dir.join("blob");
        std::fs::write(&blob, b"twelve bytes").unwrap();

        // Written just now: still recoverable, still kept.
        assert_eq!(orphan_past_retention(&blob), None);

        set_mtime_ago(
            &blob,
            super::STAGING_ORPHAN_RETAIN + Duration::from_secs(60),
        );
        assert_eq!(orphan_past_retention(&blob), Some(12));

        // A blob that is not there is not a blob to retire.
        std::fs::remove_file(&blob).unwrap();
        assert_eq!(orphan_past_retention(&blob), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Backdate a file's mtime, which is what the retention check reads.
    fn set_mtime_ago(path: &std::path::Path, ago: Duration) {
        let when = std::time::SystemTime::now() - ago;
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let times = [
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
        ];
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: both pointers are valid for the call and nothing keeps them.
        let rc = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes: {}", std::io::Error::last_os_error());
    }

    /// `(uid, parent)` pairs in the shape [`Db::trash_parents`] returns.
    fn relations(pairs: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        pairs
            .iter()
            .map(|(uid, parent)| (uid.to_string(), parent.map(str::to_string)))
            .collect()
    }

    fn uids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    /// Restoring a folder brings back what was trashed inside it, and the folder
    /// goes first: the server has nowhere to put a child whose parent is trashed.
    #[test]
    fn restoring_a_folder_restores_its_trashed_contents_parents_first() {
        let trash = relations(&[
            ("dir", Some("live")),
            ("file", Some("dir")),
            ("deep", Some("sub")),
            ("sub", Some("dir")),
        ]);
        assert_eq!(
            expand_restore(&uids(&["dir"]), &trash),
            vec![
                vec!["dir".to_string()],
                vec!["file".to_string(), "sub".to_string()],
                vec!["deep".to_string()],
            ]
        );
    }

    /// Restoring a file inside a trashed folder restores the folder too —
    /// otherwise the file comes back somewhere the user cannot reach.
    #[test]
    fn restoring_content_restores_the_trashed_parents_above_it() {
        let trash = relations(&[
            ("dir", Some("live")),
            ("sub", Some("dir")),
            ("file", Some("sub")),
            ("other", Some("dir")),
        ]);
        // `other` is a sibling of `sub`, not an ancestor of `file`: it stays put.
        assert_eq!(
            expand_restore(&uids(&["file"]), &trash),
            vec![
                vec!["dir".to_string()],
                vec!["sub".to_string()],
                vec!["file".to_string()],
            ]
        );
    }

    /// A node whose parent is not in the trash is already at the top: one wave,
    /// no ancestors invented.
    #[test]
    fn a_node_under_a_live_parent_restores_on_its_own() {
        let trash = relations(&[("file", Some("live")), ("elsewhere", None)]);
        assert_eq!(
            expand_restore(&uids(&["file"]), &trash),
            vec![vec!["file".to_string()]]
        );
    }

    /// The listing is materialised in chunks and can be stale, so a uid it does
    /// not know about must still be restored rather than dropped.
    #[test]
    fn an_unlisted_uid_is_still_restored() {
        let trash = relations(&[("dir", None)]);
        assert_eq!(
            expand_restore(&uids(&["missing"]), &trash),
            vec![vec!["missing".to_string()]]
        );
    }

    /// Asking for both a folder and something inside it restores each node once.
    #[test]
    fn overlapping_requests_do_not_duplicate_nodes() {
        let trash = relations(&[("dir", None), ("file", Some("dir"))]);
        assert_eq!(
            expand_restore(&uids(&["dir", "file"]), &trash),
            vec![vec!["dir".to_string()], vec!["file".to_string()]]
        );
    }

    /// A node keeps the time it was first seen in the trash; a new one is dated
    /// now only when the previous refresh saw the whole trash.
    #[test]
    fn trash_dates_are_kept_and_only_guessed_after_a_complete_refresh() {
        let seen = HashMap::from([("old".to_string(), Some(5)), ("unknown".to_string(), None)]);
        assert_eq!(first_seen_in_trash(&seen, "old", true, 9), Some(5));
        assert_eq!(first_seen_in_trash(&seen, "old", false, 9), Some(5));
        assert_eq!(first_seen_in_trash(&seen, "unknown", true, 9), None);
        assert_eq!(first_seen_in_trash(&seen, "new", true, 9), Some(9));
        assert_eq!(first_seen_in_trash(&seen, "new", false, 9), None);
    }

    /// A parent chain that loops back on itself must terminate.
    #[test]
    fn a_cyclic_listing_terminates() {
        let trash = relations(&[("a", Some("b")), ("b", Some("a"))]);
        let waves = expand_restore(&uids(&["a"]), &trash);
        let total: usize = waves.iter().map(Vec::len).sum();
        assert_eq!(total, 2);
    }

    /// The collapse [`Core::require_uid_writable`] performs, without a `Core`:
    /// every non-writable authority is one `EACCES` on the syscall path.
    fn require_uid_access(db: &Db, uid: &NodeUid, live: &[Access]) -> Result<(), Errno> {
        match uid_write_authority(db, uid, live) {
            WriteAuthority::Writable => Ok(()),
            WriteAuthority::Denied | WriteAuthority::Unknown => Err(Errno::EACCES),
        }
    }
    use crate::filesystem::access_allowed;
    use pdfs_store::cache::{Baseline, StagedWrite};
    use pdfs_store::db::{OP_REVISION, OP_TRASH, PendingOp};

    fn session_flag(live: bool) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(live))
    }

    /// A mount as the registry sees it, with the cells its `Core` would own.
    struct TestMount {
        notifier: std::sync::Arc<std::sync::OnceLock<super::KernelChannel>>,
        live: std::sync::Arc<std::sync::atomic::AtomicBool>,
        root: std::sync::Arc<std::sync::OnceLock<u64>>,
    }

    impl TestMount {
        fn register(registry: &StateRegistry, mountpoint: &str, root: Option<u64>) -> Self {
            let mount = Self {
                notifier: std::sync::Arc::new(std::sync::OnceLock::new()),
                live: session_flag(false),
                root: std::sync::Arc::new(std::sync::OnceLock::new()),
            };
            if let Some(root) = root {
                mount.root.set(root).unwrap();
            }
            registry.register(
                std::path::Path::new(mountpoint),
                &mount.notifier,
                mount.live.clone(),
                mount.root.clone(),
            );
            mount
        }

        fn spawn(&self) {
            self.live.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Whether `uid` is visible from the root a single-mount test installs.
    fn visible(state: &crate::state::State, uid: &NodeUid) -> bool {
        state.owns_visible_uid(uid, &[super::ROOT_INO])
    }

    /// The registry is how background work reaches every mounted session's
    /// kernel cache, so the walk has to see *every* registered mount — and
    /// only the ones still mounted (`docs/BUGS.md` B74).
    #[test]
    fn state_registry_walks_every_live_mount_and_reaps_the_rest() {
        let registry = StateRegistry::default();
        assert!(registry.live().is_empty(), "a fresh registry has no mounts");

        // The primary mount plus two on-demand forks.
        let primary = TestMount::register(&registry, "/mnt/primary", None);
        let fork_a = TestMount::register(&registry, "/mnt/fork-a", None);
        let fork_b = TestMount::register(&registry, "/mnt/fork-b", None);
        assert_eq!(registry.live().len(), 3, "every registered mount is walked");

        // `live()` must not itself pin a mount alive, or an unmounted fork could
        // never be reaped.
        assert_eq!(
            std::sync::Arc::strong_count(&fork_a.notifier),
            1,
            "the registry holds forks weakly"
        );

        // Unmounting a fork drops its `Core`; the dead entry goes on the next walk.
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
        let primary_path = std::path::Path::new("/home/me/ProtonDrive");
        let nested_path = std::path::Path::new("/home/me/ProtonDrive/Device");
        let sibling_path = std::path::Path::new("/home/me/Archive");
        let primary = TestMount::register(&registry, "/home/me/ProtonDrive", Some(2));
        let nested = TestMount::register(&registry, "/home/me/ProtonDrive/Device", Some(3));
        let sibling = TestMount::register(&registry, "/home/me/Archive", Some(4));
        sibling.spawn();

        assert_eq!(registry.live().len(), 3);
        assert!(!registry.is_mounted_at(primary_path));
        assert!(!registry.is_mounted_at(nested_path));
        assert_eq!(
            registry.live_roots(),
            vec![4],
            "an unspawned session's root must not authorize resident uids"
        );
        assert!(
            registry
                .covering(&nested_path.join("folder/file.txt"))
                .is_none(),
            "a registered but unspawned session must not win path covering"
        );
        primary.spawn();
        nested.spawn();
        let mut roots = registry.live_roots();
        roots.sort();
        assert_eq!(roots, vec![2, 3, 4]);

        let covering = registry
            .covering(&nested_path.join("folder/file.txt"))
            .unwrap();
        assert_eq!(covering.mountpoint, nested_path);
        assert!(std::sync::Arc::ptr_eq(&covering.notifier, &nested.notifier));
        drop(covering);
        let covering = registry.covering(nested_path).unwrap().mountpoint;
        assert_eq!(covering, nested_path, "an exact mountpoint covers itself");
        let covering = registry
            .covering(std::path::Path::new("/home/me/ProtonDrive/Device/folder/"))
            .unwrap()
            .mountpoint;
        assert_eq!(
            covering, nested_path,
            "a trailing separator does not change component-prefix selection"
        );
        let covering = registry
            .covering(std::path::Path::new(
                "/home/me/ProtonDrive/DeviceBackup/file",
            ))
            .unwrap()
            .mountpoint;
        assert_eq!(
            covering, primary_path,
            "/Device must not string-prefix-match /DeviceBackup"
        );
        let covering = registry.covering(&sibling_path.join("file")).unwrap();
        assert_eq!(covering.mountpoint, sibling_path);
        assert!(std::sync::Arc::ptr_eq(
            &covering.notifier,
            &sibling.notifier
        ));
        drop(covering);
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
        // session construction failed, and its last `Core` was dropped. It
        // must not make ListLocations report mounted.
        drop(nested);
        assert!(!registry.is_mounted_at(nested_path));
        assert_eq!(registry.live().len(), 2);
    }

    /// The routing half of `docs/BUGS.md` B86: a path under a secondary mount
    /// has to resolve to *that* mount's root and a suffix relative to it, or
    /// `pdfs ls` and `pdfs refresh` can only ever name the primary mount and a
    /// stale on-demand folder has no escape hatch.
    #[test]
    fn covering_routes_a_path_to_the_mount_that_owns_it() {
        let registry = StateRegistry::default();
        let primary_path = std::path::Path::new("/home/me/ProtonDrive");
        let nested_path = std::path::Path::new("/home/me/ProtonDrive/Device");
        let primary = TestMount::register(&registry, "/home/me/ProtonDrive", Some(2));
        let nested = TestMount::register(&registry, "/home/me/ProtonDrive/Device", Some(3));
        primary.spawn();
        nested.spawn();

        let deep = nested_path.join("sub/file.txt");
        let mount = registry.covering(&deep).expect("nested mount covers");
        assert_eq!(mount.mountpoint, nested_path, "the nested mount wins");
        assert_eq!(
            mount.root.get(),
            Some(&3),
            "the request must be resolved from the nested root"
        );
        assert!(
            std::sync::Arc::ptr_eq(&mount.notifier, &nested.notifier),
            "and tell that mount's session, not the primary's"
        );
        assert_eq!(
            deep.strip_prefix(&mount.mountpoint).unwrap(),
            std::path::Path::new("sub/file.txt"),
            "the suffix is relative to the mount that owns it, not the primary"
        );

        let shallow = primary_path.join("Documents");
        let mount = registry.covering(&shallow).expect("primary mount covers");
        assert_eq!(
            mount.root.get(),
            Some(&2),
            "a path outside the nested root still routes to the primary"
        );

        assert!(
            registry
                .covering(std::path::Path::new("/tmp/outside"))
                .is_none(),
            "a path under no mount routes nowhere, and the caller reports that"
        );
    }

    /// Every mount shares one tree, so a node under a fork's root and one under
    /// the primary root are both found there — but only while the mount whose
    /// root they sit under is mounted.
    #[test]
    fn a_uid_is_visible_under_the_root_of_any_mounted_session() {
        let (mut state, _dir) = rooted_state("primary-volume", "primary");
        let primary_uid = state.entries[&super::ROOT_INO].uid.clone();
        let device_root = node_helper_in_volume("device-volume", "device", None, "device", true);
        let fork_root = state.install_root(device_root);
        let fork_node =
            node_helper_in_volume("device-volume", "fork", Some("device"), "fork", true);
        let fork_uid = fork_node.uid.clone();
        let fork_ino = state.intern(fork_root, fork_node);
        state.children.insert(fork_root, vec![fork_ino]);

        let both = [super::ROOT_INO, fork_root];
        assert!(state.owns_visible_uid(&primary_uid, &both));
        assert!(state.owns_visible_uid(&fork_uid, &both));
        assert!(
            !state.owns_visible_uid(&super::parse_uid("device-volume~missing").unwrap(), &both)
        );

        assert!(
            !state.owns_visible_uid(&fork_uid, &[super::ROOT_INO]),
            "an unmounted on-demand root must stop authorizing its uids"
        );
    }

    /// Each session calls its own root inode 1, and nothing else may come out
    /// as 1 or come in from it.
    #[test]
    fn each_session_names_its_own_root_as_the_kernel_root() {
        let fork_root = 41;
        assert_eq!(
            super::filesystem::kernel_ino(fork_root, fork_root).0,
            super::ROOT_INO
        );
        assert_eq!(super::filesystem::kernel_ino(fork_root, 7).0, 7);
        assert_eq!(super::filesystem::kernel_ino(super::ROOT_INO, 7).0, 7);
    }

    #[test]
    fn a_missing_listing_is_unknown_not_absent() {
        let (mut state, _dir) = rooted_state("device-volume", "device");
        let child_node =
            node_helper_in_volume("device-volume", "child", Some("device"), "child.txt", false);
        let child_uid = child_node.uid.clone();
        let child_ino = state.intern(super::ROOT_INO, child_node);
        state.children.insert(super::ROOT_INO, vec![child_ino]);

        assert!(visible(&state, &child_uid));

        state.invalidate_listing(super::ROOT_INO);
        assert!(
            visible(&state, &child_uid),
            "an invalidated listing is unknown and must not hide a valid resident child"
        );

        state.children.insert(super::ROOT_INO, Vec::new());
        assert!(
            !visible(&state, &child_uid),
            "a present listing that omits the child proves it is no longer visible"
        );
    }

    #[test]
    fn virtual_root_reconcile_does_not_recreate_an_invalidated_listing() {
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

        assert!(
            visible(&state, &child_uid),
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
    fn open_unlinked_and_revoked_residents_are_not_visible() {
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

        assert!(visible(&state, &open_uid));
        assert!(visible(&state, &child_uid));

        state.unlink_mem(&open_uid);
        assert!(
            !visible(&state, &open_uid),
            "an open inode retained after unlink is not addressable"
        );

        state.hide_shared_root(&revoked_uid);
        assert!(!visible(&state, &revoked_uid));
        assert!(
            !visible(&state, &child_uid),
            "a retained descendant beneath a revoked root is not reachable"
        );
    }

    #[test]
    fn foreign_broken_and_cyclic_residents_are_not_visible() {
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

        assert!(
            !visible(&state, &foreign_uid),
            "incoming-share residents are outside the mounted root's volume"
        );
        assert!(
            !visible(&state, &broken_uid),
            "an interned node without a visible parent dentry is not reachable"
        );
        assert!(
            !visible(&state, &cycle_a_uid),
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
            conflict_name_as("notes.txt", "laptop", 1700),
            "notes (sync-conflict laptop 1700).txt"
        );
        assert_eq!(
            conflict_name_as("README", "laptop", 42),
            "README (sync-conflict laptop 42)"
        );
        assert_eq!(
            conflict_name_as("archive.tar.gz", "laptop", 7),
            "archive.tar (sync-conflict laptop 7).gz",
            "only the last extension is one, as everywhere else"
        );
        assert_eq!(
            conflict_name_as("notes.txt", "", 1700),
            "notes (sync-conflict 1700).txt"
        );
        assert_eq!(
            conflict_name("notes.txt", 1700),
            conflict_name_as("notes.txt", conflict_label(), 1700)
        );
    }

    /// A hostname goes into a file name, so it is cut down to one safe token
    /// (B251).
    #[test]
    fn a_hostname_becomes_one_safe_token() {
        assert_eq!(conflict_label_of("laptop"), "laptop");
        assert_eq!(conflict_label_of("my-pc.local"), "my-pc.local");
        assert_eq!(conflict_label_of("Linux device"), "Linux-device");
        assert_eq!(conflict_label_of("a/b) c"), "a-b--c");
        assert_eq!(conflict_label_of("  "), "");
        assert_eq!(conflict_label_of(&"x".repeat(40)).len(), 32);
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
    fn a_remote_name_that_is_not_one_path_component_is_left_out() {
        let folder_uid = NodeUid::new(VolumeId::from("vol"), LinkId::from("folder"));
        let none = std::collections::HashSet::new();
        for name in [
            "a/b",
            "../../.bashrc",
            "..",
            ".",
            "",
            "a\0b",
            &"x".repeat(256),
        ] {
            let node = node_helper("child", "folder", name, false);
            assert!(!node_visible(&node, &folder_uid, &none), "{name:?}");
        }
        for name in [".hidden", "a..b", "unicodé-文件", &"x".repeat(255)] {
            let node = node_helper("child", "folder", name, false);
            assert!(node_visible(&node, &folder_uid, &none), "{name:?}");
        }
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
                invite_time: None,
                inviter_email: None,
                inviter_verification: None,
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

    /// The drain's source: `drain.rs` and the modules it was split into.
    const DRAIN_SOURCE: &str = concat!(
        include_str!("drain/create.rs"),
        include_str!("drain/rename.rs"),
        include_str!("drain/revision.rs"),
        include_str!("drain/trash.rs"),
        include_str!("drain.rs"),
    );

    /// `Core`'s source: `lib.rs` and the modules its methods were split into.
    const CORE_SOURCE: &str = concat!(
        include_str!("activity.rs"),
        include_str!("echoes.rs"),
        include_str!("enqueue.rs"),
        include_str!("listing.rs"),
        include_str!("maintenance.rs"),
        include_str!("pins.rs"),
        include_str!("recovery.rs"),
        include_str!("remote.rs"),
        include_str!("requests.rs"),
        include_str!("resolve.rs"),
        include_str!("search.rs"),
        include_str!("shared.rs"),
        include_str!("sizes.rs"),
        include_str!("trash.rs"),
        // Last: the tests below quote signatures, and a search must find the
        // function before it finds the quote.
        include_str!("lib.rs"),
    );

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
        let source = CORE_SOURCE;

        let revision = function_source(source, "fn queue_revision(");
        assert_before(revision, "if !h.dirty", "preserve_on_access_denied");
        assert_before(revision, "preserve_on_access_denied", "fill_gaps");

        let staged = function_source(source, "fn enqueue_staged_write(");
        assert_before(staged, "preserve_on_access_denied", "self.writes.queued(");
        assert_before(
            staged,
            "preserve_on_access_denied",
            "self.cache.stage_write",
        );

        let truncate = function_source(source, "fn queue_truncate(");
        assert_before(truncate, "require_uid_writable", "self.writes.queued(");
        assert_before(truncate, "require_uid_writable", "create_scratch");

        for (signature, first_side_effect) in [
            ("fn queue_local_node(", "add_local_row"),
            ("fn queue_rename(", "queue_rename_authorized"),
            ("fn queue_trash(", "replace_ops_with_trash"),
        ] {
            let function = function_source(source, signature);
            assert_before(function, "require_uid_writable", first_side_effect);
        }
        let trash = function_source(source, "fn queue_trash(");
        assert_before(trash, "replace_ops_with_trash", "release_dropped_ops");

        // §5, parallel drain: every path that unlinks a staged blob or hands
        // the same node's work to another worker has to stop the upload that
        // may be reading those bytes *first*. Late is a superseded revision on
        // the wire, or two workers deciding whether the file exists.
        for (signature, mutation) in [
            ("fn enqueue_staged_write(", "attach_blob_to_create"),
            ("fn queue_trash(", "replace_ops_with_trash"),
            ("fn discard_queued_ops(", "delete_ops_for_uid"),
            ("fn withdraw_queued_ops(", "delete_ops_for_uid"),
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
            ("fn serve_rename(", "require_rename_access", "rename_remote"),
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
        for (signature, gate, remote_call, settle) in [
            (
                "fn rename(&self, rel: &Path",
                "require_rename_access",
                ".rename_remote",
                "self.relocate_everywhere(&uid",
            ),
            (
                "fn move_to(&self, rel: &Path",
                "require_rename_access",
                ".move_remote",
                "self.relocate_everywhere(&uid",
            ),
            (
                "fn delete(&self, rel: &Path",
                "require_node_parent_access",
                ".trash_nodes",
                "self.forget_everywhere(&uid)",
            ),
        ] {
            let function = function_source(source, signature);
            assert_before(function, "source_parent_uid", gate);
            assert_before(function, gate, remote_call);
            // The kernel did not see this change, so it has to be told the
            // old name is gone (`docs/BUGS.md` B114).
            assert_before(function, remote_call, settle);
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
    fn a_listing_for_the_cli_settles_sizes_before_reading_them() {
        // A fresh listing holds ciphertext sizes until its upgrade lands
        // (`docs/BUGS.md` B115).
        let list = function_source(CORE_SOURCE, "fn list_dir(&self, rel: &Path)");
        assert_before(
            list,
            "self.ensure_children(ino)",
            "self.settle_listing_sizes(ino)",
        );
        assert_before(list, "self.settle_listing_sizes(ino)", "node_size(&e.node)");
    }

    #[test]
    fn a_replaced_node_survives_its_open_handles() {
        // A rename over a file someone is reading must leave that reader on the
        // replaced bytes; forgetting the inode outright made its next read EIO.
        let source = CORE_SOURCE;
        let replaced = function_source(source, "fn remove_replaced(");
        assert!(replaced.contains("forget_or_unlink(uid)"));
        assert!(!replaced.contains("st.forget(uid)"));
        assert_before(
            replaced,
            "let open = self.is_open_anywhere(uid)",
            "self.discard_queued_ops(uid)",
        );
        assert_before(replaced, "if open_now", "self.cache.evict(uid)");
        // ...but its queued ops go at once, open or not: a revision drained
        // after the trash landed on the trashed node and kept its name taken.
        assert_eq!(replaced.matches("self.withdraw_queued_ops(uid)").count(), 2);
    }

    #[test]
    fn a_write_closed_as_its_create_lands_is_queued() {
        // The release followed a landing create, found none, and the create
        // landed before it asked for the create op. Finding none, it took the
        // file for one unlinked before it was created and dropped the write
        // (B176). The landing removes the op and moves the row at once, so a
        // missing op is checked against the row.
        let revision = function_source(CORE_SOURCE, "fn queue_revision(");
        assert_before(
            revision,
            "self.db.has_create_op(",
            "self.follow_landed_create(h)",
        );
        assert_before(
            revision,
            "self.follow_landed_create(h)",
            "dropping revision",
        );
    }

    #[test]
    fn a_replaced_node_put_back_shows_in_the_mount_again() {
        // The restore after a failed rename left the node hidden and out of
        // the listing, so a create by its name failed with ENOENT (B124).
        let restore = function_source(CORE_SOURCE, "fn restore_replaced(");
        assert_before(
            restore,
            "self.drive.restore_nodes(",
            "self.writes.unhide(uid)",
        );
        assert!(restore.contains("st.invalidate_listing(ino)"));
        assert!(restore.contains("notify.inval_entry(ino, name.to_string())"));
    }

    #[test]
    fn a_size_upgrade_leaves_the_page_cache_alone() {
        // Reads resolve provisional sizes inline, while the kernel holds the
        // page lock of that read; dropping pages here waited on it forever.
        let upgrade = function_source(CORE_SOURCE, "fn apply_size_upgrade(");
        assert!(upgrade.contains("notifier.inval_attrs(ino)"));
        assert!(!upgrade.contains("notifier.inval_inode("));
        let read = function_source(include_str!("filesystem.rs"), "    fn read(");
        assert!(read.contains("core.resolve_provisional_size(ino.0, &uid)"));
    }

    fn api_error(code: super::ResponseCode) -> super::ProtonError {
        super::ProtonError::Api(proton_drive_rs::proton_sdk::error::ProtonApiError {
            code,
            http_status: 422,
            message: "This file or folder was out of date, rename failed.".into(),
            details: None,
        })
    }

    #[test]
    fn a_rename_called_out_of_date_is_retried_until_it_lands() {
        let mut calls = 0;
        let mut waits = 0;
        let result = super::retry_out_of_date(
            || {
                calls += 1;
                if calls < 3 {
                    Err(api_error(super::ResponseCode::InvalidRequirements))
                } else {
                    Ok(())
                }
            },
            || waits += 1,
        );
        assert!(result.is_ok());
        assert_eq!((calls, waits), (3, 2));
    }

    #[test]
    fn a_rename_gives_up_on_other_errors_and_after_its_retries() {
        let mut calls = 0;
        let taken = super::retry_out_of_date(
            || {
                calls += 1;
                Err(api_error(super::ResponseCode::AlreadyExists))
            },
            || {},
        );
        assert!(taken.is_err());
        assert_eq!(calls, 1, "a taken name is not a stale read");

        let mut calls = 0;
        let stale = super::retry_out_of_date(
            || {
                calls += 1;
                Err(api_error(super::ResponseCode::InvalidRequirements))
            },
            || {},
        );
        assert!(stale.is_err());
        assert_eq!(calls, super::OUT_OF_DATE_RETRIES + 1);
    }

    #[test]
    fn every_remote_rename_and_move_rides_out_a_stale_hash() {
        for source in [
            CORE_SOURCE,
            include_str!("filesystem.rs"),
            DRAIN_SOURCE,
            include_str!("relocate.rs"),
        ] {
            assert_eq!(
                source.matches(concat!("drive.rename_node", "(")).count(),
                usize::from(source.contains("fn rename_remote(")),
                "call Core::rename_remote instead of the SDK rename directly"
            );
            assert_eq!(
                source.matches(concat!("drive.move_node", "(")).count(),
                usize::from(source.contains("fn move_remote(")),
                "call Core::move_remote instead of the SDK move directly"
            );
            assert_eq!(
                source
                    .matches(concat!("drive.move_nodes_streaming", "("))
                    .count(),
                usize::from(source.contains("fn move_rename_remote(")),
                "call Core::move_rename_remote instead of the SDK move directly"
            );
        }
    }

    #[test]
    fn a_rename_or_move_drops_the_sdk_copy_and_expects_its_echo() {
        // The SDK would send the old name hash with the node's next rename or
        // move until the event poll caught up (B121), and the echo of the
        // change, taken for someone else's, evicted the cached bytes (B123).
        let change = function_source(CORE_SOURCE, "fn change_remote(");
        assert!(
            change.contains("self.forget_sdk_node(uid)"),
            "forget the node before each retry"
        );
        assert!(change.contains("self.changed_remote(uid)"));
        let moved = function_source(CORE_SOURCE, "fn move_rename_remote(");
        assert!(moved.contains("self.change_remote(uid"));
        let changed = function_source(CORE_SOURCE, "fn changed_remote(");
        assert!(changed.contains("self.forget_sdk_node(uid)"));
        assert!(changed.contains("self.note_self_change(uid)"));
    }

    #[test]
    fn a_listing_changed_while_published_is_taken_again() {
        // The root listed again after a restart while a lookup was publishing
        // the "Shared with me" entry, and the lookup answered EAGAIN (B159).
        let ensure = function_source(CORE_SOURCE, "fn ensure_children(");
        assert_before(
            ensure,
            "for _ in 1..LISTING_ATTEMPTS",
            "Errno::EAGAIN.code()",
        );
        assert_eq!(ensure.matches("self.list_children(ino)").count(), 2);
        assert!(
            !ensure.contains("?"),
            "an attempt's error must not skip the retry"
        );
    }

    #[test]
    fn a_folder_lists_from_the_db_while_the_link_is_down() {
        // A listing dropped as stale mid-run, then a drop: every create in that
        // folder looks the name up first, and answered EIO for want of it.
        let ensure = function_source(CORE_SOURCE, "fn list_children(");
        assert_before(
            ensure,
            "if !self.is_online()",
            "enumerate_folder_children_node_uids",
        );
        assert_eq!(ensure.matches("adopt_stale_listing(").count(), 3);
        let online = function_source(include_str!("link.rs"), "fn after_reconnect(");
        assert!(online.contains("self.relist_offline_listings();"));
    }

    #[test]
    fn a_stale_shared_listing_is_fetched_off_the_dispatch_loop() {
        // A resident listing that `list_children` would fetch again anyway
        // must not be answered inline (B219).
        let cached = function_source(CORE_SOURCE, "fn children_cached(");
        assert!(cached.contains("SHARED_WITH_ME_SYNCED_MS"));
        assert!(cached.contains("Self::shared_folder_freshness_key(&uid)"));
        assert!(cached.contains("listing_needs_refresh("));
        assert!(cached.contains("self.shared_listing_stale(&key)"));
    }

    #[test]
    fn a_handler_does_not_wait_out_the_sdk_retry_budget() {
        // Thumbnails, size upgrades and the share list held mount threads for
        // minutes on a stalled link (B220).
        let lib = CORE_SOURCE;
        let thumbnail = function_source(lib, "fn thumbnail(");
        assert_before(thumbnail, "if !self.is_online()", "download_thumbnail");
        assert!(thumbnail.contains(".block_on_bounded(self.client.download_thumbnail("));
        let upgrade = function_source(CORE_SOURCE, "fn run_size_upgrade(");
        assert!(upgrade.contains("block_on_within(BATCH_CALL_TIMEOUT"));
        let shared = function_source(CORE_SOURCE, "fn ensure_shared_children(");
        assert!(shared.contains("block_on_within(BATCH_CALL_TIMEOUT"));
        assert!(!shared.contains("self.rt"));
    }

    #[test]
    fn a_remote_read_gives_up_at_its_deadline() {
        // The SDK bounds requests, not reads; a stalled link held one `read`
        // for 34 minutes.
        let remote = function_source(include_str!("reads.rs"), "fn read_range_remote(");
        assert_before(
            remote,
            "let fetch = async",
            "tokio::time::timeout(budget, fetch)",
        );
        assert!(remote.contains("let deadline = Instant::now() + READ_FETCH_TIMEOUT;"));
        // Built inside the runtime: a FUSE worker has no reactor, and a timer
        // built there panicked every remote read.
        assert!(remote.contains("async { tokio::time::timeout(budget, fetch).await }"));
    }

    #[test]
    fn a_kernel_notice_sent_from_the_runtime_leaves_its_timers_running() {
        // A notice waits in the kernel for a read whose fetch waits on a
        // timer of the same runtime (B130). Here the runtime's one worker sends
        // it, and the timer has to fire for the notice to return.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (fetched, fetch) = std::sync::mpsc::channel();
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = fetched.send(());
        });
        let notice =
            rt.spawn(async move { off_the_runtime(|| fetch.recv_timeout(Duration::from_secs(5))) });
        assert!(rt.block_on(notice).unwrap().is_ok());

        // Every notice to a session goes through it.
        let lib = CORE_SOURCE;
        for method in [
            "fn inval_inode(&self",
            "fn inval_attrs(&self",
            "fn inval_entry(&self",
            "fn delete(&self, parent: u64",
        ] {
            let source = function_source(lib, method);
            assert_eq!(source.matches("self.notifier.").count(), 1, "{method}");
            assert!(
                source.contains("off_the_runtime(|| self.notifier."),
                "{method}"
            );
        }
    }

    #[test]
    fn a_deadline_built_under_block_on_needs_no_ambient_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let answered = std::thread::spawn(move || {
            rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), async { 7 }).await })
        })
        .join()
        .expect("a worker thread outside the runtime can wait on the deadline");
        assert_eq!(answered.unwrap(), 7);
    }

    #[test]
    fn a_trashed_open_node_sends_nothing_more() {
        // Trashing a file under an open handle withdraws its queued upload at
        // once; the bytes the handle reads go with the last close.
        let withdraw = function_source(CORE_SOURCE, "fn withdraw_queued_ops(");
        assert_before(withdraw, "self.pending_blob(uid)", "release_dropped_below");
        assert!(!withdraw.contains("self.writes.pending.lock().remove(uid)"));

        let filesystem = include_str!("filesystem.rs");
        let trash = function_source(filesystem, "fn trash_child(");
        assert_eq!(
            trash.matches("self.core.withdraw_queued_ops(&uid)").count(),
            2
        );
        let release = function_source(filesystem, "fn finish_release(");
        assert_before(
            release,
            "let withdrawn = self.core.pending_blob(&uid)",
            "discard_queued_ops",
        );
        assert_before(release, "discard_queued_ops", "discard_staged(&blob)");

        // A drain worker that claimed the op before it was withdrawn checks
        // again before any byte leaves.
        let drain = function_source(DRAIN_SOURCE, "pub(crate) fn drain_revision(");
        let upload = &drain[drain.find("begin_cancellable_upload").unwrap()..];
        assert_before(upload, "self.db.op_exists(op.id)", "File::open(&blob)");
    }

    #[test]
    fn an_online_rename_writes_the_move_through_before_it_answers() {
        // Through the raw lock, the move stayed in the state's outbox, and the
        // next lookup rebuilt the destination from a database without it
        // (B135).
        let rename: String = function_source(include_str!("filesystem.rs"), "fn serve_rename(")
            .split_whitespace()
            .collect();
        assert!(rename.contains("self.core.state().relocate("));
        assert!(!rename.contains(".lock().relocate("));
    }

    #[test]
    fn a_paused_drain_claims_nothing() {
        // A claimed create counts as sent, so trashing its folder kept it as
        // a trash, although the paused worker only handed it back (B178).
        let worker: String = function_source(DRAIN_SOURCE, "pub(crate) fn run_pending_drain(")
            .split_whitespace()
            .collect();
        assert!(worker.contains("(online&&!paused).then(||self.db.claim_next_due_op(now))"));
    }

    #[test]
    fn a_landed_upload_stays_pending_until_its_bytes_are_cached() {
        let drain = function_source(DRAIN_SOURCE, "pub(crate) fn drain_revision(");
        assert_before(
            drain,
            "let _landing = LandingUpload::begin(&self.drain.landing_uploads)",
            "self.db.delete_op(op.id)",
        );
        assert_before(drain, "self.db.delete_op(op.id)", ".store_file(&uid,");
        let status = function_source(include_str!("control.rs"), "fn status_response(");
        assert_before(
            status,
            "core.db.pending_op_counts()",
            "core.drain.landing_uploads.load(",
        );
        assert!(status.contains("queued.uploads.max(0) as u64 + landing"));
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
        let db = pdfs_store::db::Db::open(&dir_path.join("cache.db")).unwrap();
        let share_access = db.all_share_access().unwrap();
        let st = crate::state::State::new(std::sync::Arc::new(db), share_access);
        (st, TestDir(dir_path))
    }

    fn rooted_state(volume: &str, root_link: &str) -> (crate::state::State, TestDir) {
        let (mut state, dir) = state_test_helper();
        let root = node_helper_in_volume(volume, root_link, None, "root", true);
        // At the root inode, as `ProtonFs::new` puts it.
        state.by_uid.insert(root.uid.clone(), super::ROOT_INO);
        state.entries.insert(
            super::ROOT_INO,
            crate::state::Entry {
                uid: root.uid.clone(),
                parent: super::ROOT_INO,
                node: root,
                access: Access::Owner,
                lookup_count: 1,
                open_count: 0,
                unlinked: false,
            },
        );
        state.roots.insert(super::ROOT_INO);
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

    /// A rename or move made over the control socket keeps the node and its
    /// row, so the queue and the drain still know it (`docs/BUGS.md` B117),
    /// and tells the kernel the old name is gone (`docs/BUGS.md` B114).
    #[test]
    fn a_control_move_keeps_the_node_and_its_row_and_drops_the_old_name() {
        use super::{KernelNotice, NotifyBatch, relocate_in_mount};

        let (mut state, _dir) = state_test_helper();
        let src = state.intern(0, node_helper("src", "none", "src", true));
        let dst = state.intern(0, node_helper("dst", "none", "dst", true));
        let moved = node_helper("moved", "src", "a.txt", false);
        let uid = moved.uid.clone();
        let ino = state.intern(src, moved);
        state.children.insert(src, vec![ino]);
        state.children.insert(dst, vec![]);
        let dst_uid = state.entries[&dst].uid.clone();

        let mut notify = NotifyBatch::default();
        let outcome = relocate_in_mount(&mut state, &mut notify, &uid, &dst_uid, "b.txt", false);
        assert_eq!(outcome, Some(("a.txt".to_string(), true)));
        assert_eq!(state.by_uid.get(&uid), Some(&ino), "the inode survives");
        assert_eq!(state.entries[&ino].parent, dst);
        let row = state
            .flushed_db()
            .node_by_uid(&uid.to_string())
            .unwrap()
            .expect("the row survives the move");
        assert_eq!(row.name, "b.txt");
        assert_eq!(row.parent_uid.as_ref(), Some(&dst_uid));
        assert!(notify.0.iter().any(|notice| matches!(
            notice,
            KernelNotice::InvalEntry { parent, name } if *parent == src && name == "a.txt"
        )));

        // A mount that does not show the new parent forgets the node, and the
        // kernel the name, but the row stays for the caller to move.
        let elsewhere = node_helper("elsewhere", "none", "elsewhere", true).uid;
        let mut notify = NotifyBatch::default();
        let outcome = relocate_in_mount(&mut state, &mut notify, &uid, &elsewhere, "c.txt", false);
        assert_eq!(outcome, Some(("b.txt".to_string(), false)));
        assert!(!state.by_uid.contains_key(&uid));
        assert!(notify.0.iter().any(|notice| matches!(
            notice,
            KernelNotice::Delete { parent, child, name } if *parent == dst && *child == ino && name == "b.txt"
        )));
        assert!(
            state
                .flushed_db()
                .node_by_uid(&uid.to_string())
                .unwrap()
                .is_some()
        );
    }

    /// A control-socket move that is queued keeps both listings: Drive still
    /// has the old name, so listing again would bring it back (B149).
    #[test]
    fn a_queued_control_move_keeps_both_listings() {
        use super::{KernelNotice, NotifyBatch, relocate_in_mount};

        let (mut state, _dir) = state_test_helper();
        let src = state.intern(0, node_helper("src", "none", "src", true));
        let dst = state.intern(0, node_helper("dst", "none", "dst", true));
        let moved = node_helper("moved", "src", "a.txt", false);
        let uid = moved.uid.clone();
        let ino = state.intern(src, moved);
        state.children.insert(src, vec![ino]);
        state.children.insert(dst, vec![]);
        let dst_uid = state.entries[&dst].uid.clone();

        let mut notify = NotifyBatch::default();
        let outcome = relocate_in_mount(&mut state, &mut notify, &uid, &dst_uid, "b.txt", true);
        assert_eq!(outcome, Some(("a.txt".to_string(), true)));
        assert_eq!(state.children.get(&src), Some(&vec![]));
        assert_eq!(state.children.get(&dst), Some(&vec![ino]));
        assert_eq!(state.entries[&ino].node.name, "b.txt");
        for folder in [src, dst] {
            assert!(
                notify.0.iter().any(
                    |notice| matches!(notice, KernelNotice::InvalInode(ino) if *ino == folder)
                )
            );
        }
        assert!(notify.0.iter().any(|notice| matches!(
            notice,
            KernelNotice::InvalEntry { parent, name } if *parent == src && name == "a.txt"
        )));
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

    #[test]
    fn hydrated_listings_hold_each_folders_live_children() {
        let entry = |id: &str, parent: u64, is_dir: bool, trashed: bool| {
            let mut node = node_helper(id, "none", id, is_dir);
            node.trashed = trashed;
            crate::state::Entry {
                uid: node.uid.clone(),
                parent,
                node,
                access: Access::Owner,
                lookup_count: 1,
                open_count: 0,
                unlinked: false,
            }
        };
        let entries: HashMap<u64, crate::state::Entry> = [
            (1, entry("root", 1, true, false)),
            (2, entry("a", 1, true, false)),
            (3, entry("b", 1, true, false)),
            (4, entry("a1", 2, false, false)),
            (5, entry("a2", 2, false, true)),
            (6, entry("b1", 3, false, false)),
        ]
        .into_iter()
        .collect();
        let mut listings = super::listed_children(&entries, vec![1, 2]);
        listings.values_mut().for_each(|kids| kids.sort());
        assert_eq!(listings, HashMap::from([(1, vec![2, 3]), (2, vec![4])]));
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
            direct_role: None,
            share_id: None,
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
