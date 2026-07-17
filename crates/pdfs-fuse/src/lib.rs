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

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::ReplyXattr;
use fuser::{
    BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, LockOwner, MountOption, Notifier, OpenAccMode, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, Session, TimeOrNow, WriteFlags,
};
use pdfs_core::cache::{BLOCK_SIZE, ContentCache};
use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    ActivityEntry, ActivityKind, BookmarkInfo, DeviceInfo, DirEntry, InvitationInfo, JobItem,
    LocalHit, PhotoItem, PhotoThumb, PublicLinkInfo, Request as CtlRequest,
    Response as CtlResponse, SearchHit, ShareEntry, ShareEntryKind, SharedItem, SyncFolderInfo,
    SyncPhase, SyncProgress, TransferDirection,
};
use pdfs_core::db::{
    self, Db, StoredDevice, StoredNode, StoredPhoto, StoredSyncFolder, StoredTrash,
};
use pdfs_core::localindex;
use proton_drive_rs::proton_sdk::error::ProtonError;
use proton_drive_rs::proton_sdk::ids::{DeviceUid, DriveEventId, LinkId, NodeUid, VolumeId};
use proton_drive_rs::{
    DeviceType, DriveEvent, DriveEventScopeId, MemberRole, Node, NodeKind, ProtonDriveClient,
    ProtonPhotosClient, ThumbnailType,
};

mod sync;
mod transfers;
use tracing::{debug, error, info, warn};
use transfers::{CountingReader, CountingWriter, JobGuard, OwnedCountingReader, TransferRegistry};

/// Attribute/entry cache lifetime handed back to the kernel. Long because the
/// Phase 2 event poller actively invalidates changed inodes; without a remote
/// change this is how long the kernel may serve stale metadata.
const TTL: Duration = Duration::from_secs(30);

/// How often the background task polls the remote event cursor.
const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// How long the persisted photos timeline stays good before a page request
/// revalidates it. The SDK hands back the whole timeline at once, so it is stored
/// in the DB and every page is sliced from there; a stale one is still served
/// immediately and refreshed in the background.
const TIMELINE_TTL: Duration = Duration::from_secs(5 * 60);
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

/// A node known to the filesystem, addressed by its kernel inode.
struct Entry {
    uid: NodeUid,
    parent: u64,
    node: Node,
}

/// A set of non-overlapping `[start, end)` byte ranges, kept sorted and merged.
/// Tracks which bytes of a [`WriteHandle`]'s scratch file were authored locally
/// (vs. still living only in the remote base), so reads and the commit gap-fill
/// know which regions to pull from the network.
#[derive(Clone, Default)]
struct Intervals(Vec<(u64, u64)>);

impl Intervals {
    /// Mark `[start, end)` as authored, coalescing with any touching ranges.
    fn add(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        self.0.push((start, end));
        self.0.sort_by_key(|&(s, _)| s);
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.0.len());
        for &(s, e) in &self.0 {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        self.0 = merged;
    }

    /// Drop everything at or beyond `len` (a shrink/truncate).
    fn clip(&mut self, len: u64) {
        self.0.retain(|&(s, _)| s < len);
        for iv in &mut self.0 {
            iv.1 = iv.1.min(len);
        }
    }

    /// Split `[start, end)` into contiguous `(s, e, authored)` segments, in
    /// order. `authored == true` means the bytes live in the scratch file;
    /// `false` means they must come from the remote base (or are a hole).
    fn segments(&self, start: u64, end: u64) -> Vec<(u64, u64, bool)> {
        let mut out = Vec::new();
        let mut pos = start;
        for &(s, e) in &self.0 {
            if e <= start {
                continue;
            }
            if s >= end {
                break;
            }
            let ws = s.max(start);
            let we = e.min(end);
            if pos < ws {
                out.push((pos, ws, false));
            }
            out.push((ws, we, true));
            pos = we;
        }
        if pos < end {
            out.push((pos, end, false));
        }
        out
    }
}

/// State for a file opened for writing. Authored bytes are staged in an on-disk
/// scratch file (positional reads/writes) rather than RAM, so a multi-GiB write
/// never balloons the daemon. On flush/release the scratch file — gap-filled
/// from the remote base where untouched — is streamed up as one new revision,
/// since the SDK seals whole revisions rather than byte ranges.
struct WriteHandle {
    ino: u64,
    uid: NodeUid,
    /// Disk-backed staging buffer. Shared (`Arc`) so reads can use it without
    /// holding the state lock across I/O. Accessed positionally (`read_at`/
    /// `write_at`), so a clone never disturbs another's file offset.
    file: Arc<File>,
    /// Scratch file path, removed on release.
    path: PathBuf,
    /// Byte ranges authored into `file`. Everything else in `[0, len)` is base.
    written: Intervals,
    /// Logical file size (may exceed authored bytes after a truncate-extend).
    len: u64,
    /// Size of the remote base at open, for serving untouched ranges.
    base_size: u64,
    /// Modification time of the remote base at open, validating its block cache.
    base_mtime: i64,
    /// Whether anything diverged from the remote and needs an upload.
    dirty: bool,
}

/// Mutable inode bookkeeping, guarded by a mutex because fuser drives the
/// `Filesystem` trait through `&self`.
struct State {
    /// inode -> node metadata.
    entries: HashMap<u64, Entry>,
    /// Dedupe inodes by node uid so a node keeps a stable inode across lookups.
    by_uid: HashMap<NodeUid, u64>,
    /// Cached directory listings: parent inode -> child inodes. Presence of a
    /// key means the directory has been enumerated.
    children: HashMap<u64, Vec<u64>>,
    next_ino: u64,
    /// Open write handles keyed by file handle id. Read-only opens use fh 0 and
    /// have no entry here.
    handles: HashMap<u64, WriteHandle>,
    next_fh: u64,
    /// Unified SQLite metadata cache. Every map mutation below writes through to
    /// it inside the `State` lock so the DB stays the authoritative copy across
    /// restarts (see plan.md P1).
    db: Arc<Db>,
}

impl State {
    /// Allocate (or reuse) a stable inode for a node and store its metadata,
    /// writing the node through to the DB.
    fn intern(&mut self, parent: u64, node: Node) -> u64 {
        if let Err(e) = self.db.upsert_node(&node) {
            warn!(uid = %node.uid, error = %e, "db upsert_node failed");
        }
        if let Some(&ino) = self.by_uid.get(&node.uid) {
            if let Some(e) = self.entries.get_mut(&ino) {
                e.node = node;
                e.parent = parent;
            }
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.by_uid.insert(node.uid.clone(), ino);
        self.entries.insert(
            ino,
            Entry {
                uid: node.uid.clone(),
                parent,
                node,
            },
        );
        ino
    }

    /// Forget a node entirely: drop its inode, its uid mapping, its own cached
    /// listing, its slot in its parent's listing, and its DB row. Returns
    /// `(parent_ino, name)` when the node was known, so the caller can notify
    /// the kernel.
    fn forget(&mut self, uid: &NodeUid) -> Option<(u64, String)> {
        let ino = self.by_uid.remove(uid)?;
        if let Err(e) = self.db.delete_node(uid) {
            warn!(%uid, error = %e, "db delete_node failed");
        }
        let entry = self.entries.remove(&ino)?;
        self.children.remove(&ino);
        if let Some(kids) = self.children.get_mut(&entry.parent) {
            kids.retain(|&k| k != ino);
        }
        Some((entry.parent, entry.node.name))
    }

    /// Drop a directory's cached child listing and mark it unlisted in the DB,
    /// so the next access re-enumerates instead of trusting a stale listing.
    fn invalidate_listing(&mut self, ino: u64) {
        if self.children.remove(&ino).is_none() {
            return;
        }
        if let Some(e) = self.entries.get(&ino) {
            let uid = e.uid.clone();
            if let Err(err) = self.db.set_listed(&uid, false) {
                warn!(%uid, error = %err, "db set_listed(false) failed");
            }
        }
    }

    /// Update a file entry's recorded plaintext size so `getattr` reflects an
    /// in-progress write before the new revision is sealed.
    fn set_size(&mut self, ino: u64, size: u64) {
        if let Some(e) = self.entries.get_mut(&ino)
            && let NodeKind::File { claimed_size, .. } = &mut e.node.kind
        {
            *claimed_size = Some(size as i64);
        }
    }

    /// Update a file entry's modification time (epoch seconds).
    fn touch_mtime(&mut self, ino: u64, secs: i64) {
        if let Some(e) = self.entries.get_mut(&ino) {
            e.node.modification_time = secs;
        }
    }
}

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
/// Drive client, a Tokio handle to bridge the synchronous FUSE/socket threads
/// to the async SDK, the inode bookkeeping, and the on-disk content cache.
///
/// Cheaply cloneable (every field is a handle/`Arc`), so the control-socket task
/// gets its own copy while the FUSE session keeps another.
#[derive(Clone)]
struct Core {
    client: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    state: Arc<Mutex<State>>,
    cache: Arc<ContentCache>,
    /// Unified SQLite metadata cache: the persistence layer behind the in-memory
    /// `State` maps. Every mutation writes through here, and the maps rehydrate
    /// from it on mount (plan.md P1).
    db: Arc<Db>,
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
        let mut st = self.state.lock().unwrap();

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
            let StoredNode { node, listed } = sn;
            let Some(&ino) = st.by_uid.get(&node.uid) else {
                continue;
            };
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

    /// Enumerate `ino`'s children from the remote and cache them. No-op if the
    /// directory has already been listed. Network I/O happens without the lock
    /// held so concurrent metadata reads aren't blocked behind a fetch.
    fn ensure_children(&self, ino: u64) -> Result<(), Errno> {
        let folder_uid = {
            let st = self.state.lock().unwrap();
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
                let mut st = self.state.lock().unwrap();
                if st.children.contains_key(&ino) {
                    return Ok(());
                }
                let mut child_inos = Vec::with_capacity(nodes.len());
                for node in nodes {
                    if node.trashed || node.uid == folder_uid {
                        continue;
                    }
                    child_inos.push(st.intern(ino, node));
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

        let mut st = self.state.lock().unwrap();
        // Lost the race? Another thread already populated it.
        if st.children.contains_key(&ino) {
            return Ok(());
        }
        let mut child_inos = Vec::with_capacity(nodes.len());
        for node in nodes {
            // A folder listed among its own children would alias its inode into
            // its own listing, which the kernel rejects with EIO.
            if node.trashed || node.uid == folder_uid {
                continue;
            }
            child_inos.push(st.intern(ino, node));
        }
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
        let st = self.state.lock().unwrap();
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
            let st = self.state.lock().unwrap();
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

    /// Fetch a single node's current metadata from the remote.
    fn fetch_node(&self, uid: &NodeUid) -> Result<Node, Errno> {
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(std::slice::from_ref(uid)))
            .map_err(|e| {
                error!(%uid, error = %e, "enumerate node failed");
                Errno::EIO
            })?;
        nodes.into_iter().next().ok_or(Errno::ENOENT)
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
        let mut blocks: Vec<Option<Vec<u8>>> = Vec::with_capacity((last - first + 1) as usize);
        let mut misses: Vec<u64> = Vec::new();
        for bidx in first..=last {
            match self.cache.cached_block(uid, mtime, fsize, bidx) {
                Some(b) => blocks.push(Some(b)),
                None => {
                    blocks.push(None);
                    misses.push(bidx);
                }
            }
        }

        if !misses.is_empty() {
            let fetched = self.rt.block_on(async {
                let mut set = tokio::task::JoinSet::new();
                for &bidx in &misses {
                    let client = self.client.clone();
                    let uid = uid.clone();
                    let bstart = bidx * BLOCK_SIZE;
                    let blen = BLOCK_SIZE.min(fsize - bstart);
                    set.spawn(async move {
                        client
                            .download_range(&uid, bstart, blen)
                            .await
                            .map(|bytes| (bidx, bytes))
                            .map_err(|e| {
                                warn!(%uid, bstart, blen, error = %e, "download_range failed");
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
                let _ = self.cache.store_block(uid, mtime, fsize, bidx, &bytes);
                blocks[(bidx - first) as usize] = Some(bytes);
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
                    )?);
                }
                // Anything past the base is a hole: zero-fill.
                let zeros = e.saturating_sub(s.max(base_size));
                out.resize(out.len() + zeros as usize, 0);
            }
        }
        Ok(out)
    }

    /// Upload a write handle's scratch file as a new revision if it is dirty.
    /// Untouched bytes within the base are filled from the remote first (reusing
    /// the block cache), so a partial overwrite never had to pre-download the
    /// whole file. On success the handle's base is advanced to the just-sealed
    /// revision and `written` cleared, so later reads of untouched regions see
    /// the new content. No-op for a clean (or unknown) handle. Network I/O runs
    /// without the lock held.
    fn commit(&self, fh: u64) -> Result<(), Errno> {
        let (uid, file, path, len, base_mtime, base_size, written, ino) = {
            let st = self.state.lock().unwrap();
            match st.handles.get(&fh) {
                Some(h) if h.dirty => (
                    h.uid.clone(),
                    h.file.clone(),
                    h.path.clone(),
                    h.len,
                    h.base_mtime,
                    h.base_size,
                    h.written.clone(),
                    h.ino,
                ),
                _ => return Ok(()),
            }
        };
        // Materialize the full new content in the scratch file: ensure its length,
        // then fill every untouched range that overlaps the base with base bytes.
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
            let bytes = self.read_range(&uid, base_mtime, base_size, s, bend - s)?;
            file.write_all_at(&bytes, s).map_err(|err| {
                error!(%uid, error = %err, "scratch gap-fill write failed");
                Errno::EIO
            })?;
        }
        // Stream the scratch file up as one revision (fresh handle reads from 0).
        let reader = File::open(&path).map_err(|e| {
            error!(%uid, error = %e, "reopen scratch file failed");
            Errno::EIO
        })?;
        let name = {
            let st = self.state.lock().unwrap();
            st.entries
                .get(&ino)
                .map(|e| e.node.name.clone())
                .unwrap_or_default()
        };
        let guard = self
            .transfers
            .begin(name, uid.to_string(), TransferDirection::Upload, len);
        let reader = CountingReader::new(reader, &guard);
        self.rt
            .block_on(self.client.upload_new_revision_from(
                &uid,
                reader,
                len as i64,
                Vec::new(),
                None,
            ))
            .map_err(|e| {
                error!(%uid, error = %e, "upload new revision failed");
                Errno::EIO
            })?;
        drop(guard);
        let now = now_secs();
        {
            let mut st = self.state.lock().unwrap();
            if let Some(h) = st.handles.get_mut(&fh) {
                h.dirty = false;
                // The scratch file now equals the sealed revision; treat it all as
                // base so further reads of untouched bytes hit the new content.
                h.written = Intervals::default();
                h.base_mtime = now;
                h.base_size = len;
            }
            st.set_size(ino, len);
            st.touch_mtime(ino, now);
        }
        // The sealed content differs from any cached blob/blocks; refresh a pinned
        // file's whole-file blob, otherwise evict so reads re-fetch fresh.
        if self.cache.is_pinned(&uid) {
            if let Ok(bytes) = std::fs::read(&path) {
                let _ = self.cache.store(&uid, now, len, &bytes);
            }
        } else {
            self.cache.evict(&uid);
        }
        Ok(())
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
    fn pin(&self, rel: &Path) -> Result<String, String> {
        let (ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let (name, is_folder, mtime, size) = {
            let st = self.state.lock().unwrap();
            let e = st.entries.get(&ino).ok_or("node vanished")?;
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
                .map_err(|e| format!("pin: {e}"))?;
            let n = self.pin_subtree(ino)?;
            return Ok(format!("{name} ({n} files)"));
        }
        let bytes = self
            .download_file_tracked(&uid, &name, size)
            .map_err(|e| format!("download: {e}"))?;
        self.cache
            .store(&uid, mtime, size, &bytes)
            .map_err(|e| format!("cache store: {e}"))?;
        self.cache
            .add_pin(&uid, rel, false)
            .map_err(|e| format!("pin: {e}"))?;
        Ok(name)
    }

    /// Download and cache every file in the subtree rooted at folder `ino`,
    /// returning the count cached (already-fresh blobs counted, not re-fetched).
    /// Walks the tree depth-first, enumerating each folder so a cold subtree is
    /// fully discovered; the lock is dropped before each network download.
    fn pin_subtree(&self, ino: u64) -> Result<usize, String> {
        let mut files: Vec<(NodeUid, String, i64, u64)> = Vec::new();
        let mut stack = vec![ino];
        while let Some(dir) = stack.pop() {
            self.ensure_children(dir)
                .map_err(|e| format!("enumerate: {e:?}"))?;
            let st = self.state.lock().unwrap();
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
            let st = self.state.lock().unwrap();
            match st.entries.get(&ino) {
                Some(e) if e.node.is_file() => (e.uid.clone(), e.node.modification_time),
                Some(_) => return Ok(None),
                None => return Err(Errno::ENOENT),
            }
        };
        if let Some(bytes) = self.cache.read_thumbnail(&uid, ttype.as_i32(), mtime) {
            return Ok(Some(bytes));
        }
        let bytes = self
            .rt
            .block_on(self.client.download_thumbnail(&uid, ttype))
            .map_err(|e| {
                warn!(%uid, error = %e, "download thumbnail failed");
                Errno::EIO
            })?;
        if let Some(bytes) = &bytes {
            let _ = self
                .cache
                .store_thumbnail(&uid, ttype.as_i32(), mtime, bytes);
        }
        Ok(bytes)
    }

    /// Unpin the node at `rel`, evicting its cached content. For a folder, also
    /// evicts every descendant's cached blob (the subtree is no longer kept).
    fn unpin(&self, rel: &Path) -> Result<String, String> {
        let (ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let (name, is_folder) = {
            let st = self.state.lock().unwrap();
            st.entries
                .get(&ino)
                .map(|e| (e.node.name.clone(), e.node.is_folder()))
                .unwrap_or_default()
        };
        self.cache
            .remove_pin(&uid)
            .map_err(|e| format!("unpin: {e}"))?;
        // A recursively-pinned folder's descendants were eviction-exempt; now
        // that the pin is gone, reclaim their blobs eagerly instead of waiting
        // for budget pressure. Descendants come from the DB node tree.
        if is_folder && let Ok(uids) = self.db.descendants(&uid.to_string()) {
            for s in uids {
                if let Some(u) = parse_uid(&s) {
                    self.cache.evict(&u);
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
    fn list_dir(&self, rel: &Path) -> Result<Vec<DirEntry>, String> {
        let (ino, _uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        self.ensure_children(ino)
            .map_err(|e| format!("enumerate: {e:?}"))?;
        // Snapshot the listing, then drop the lock before touching the on-disk
        // pin registry so a slow disk read doesn't block FUSE metadata ops.
        let rows: Vec<(String, bool, u64, i64, NodeUid)> = {
            let st = self.state.lock().unwrap();
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
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let hits = self
            .db
            .search(query, limit)
            .map_err(|e| format!("search: {e}"))?;
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
    fn search_local(&self, query: &str, limit: usize) -> Result<Vec<LocalHit>, String> {
        let hits = self
            .db
            .search_local(query, limit)
            .map_err(|e| format!("local search: {e}"))?;
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
    fn photos_timeline(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Option<Vec<PhotoItem>>, String> {
        let count = self.db.photos_count().map_err(|e| e.to_string())?;
        if count == 0 {
            // Nothing to serve, so this one request has to wait for the fetch —
            // unless we already know the account has no photos volume and the
            // answer is a fresh "no".
            let known_empty = self.db.state_i64(PHOTOS_AVAILABLE).ok().flatten() == Some(0);
            if known_empty && !self.listing_stale(PHOTOS_SYNCED_MS, TIMELINE_TTL) {
                return Ok(None);
            }
            if !self.rt.block_on(self.refresh_timeline())? {
                return Ok(None);
            }
        } else if self.listing_stale(PHOTOS_SYNCED_MS, TIMELINE_TTL) {
            self.spawn_timeline_refresh();
        }

        let page = self
            .db
            .photos_page(offset, limit)
            .map_err(|e| e.to_string())?;
        Ok(Some(page.into_iter().map(|p| self.photo_item(p)).collect()))
    }

    /// Project a persisted photo into the wire item the front-end paints: its
    /// learned aspect ratio, its thumbnail verdict, and the on-disk path of its
    /// thumbnail when one is cached (tagged with the capture time, which is the
    /// only revision handle a photo carries).
    fn photo_item(&self, photo: StoredPhoto) -> PhotoItem {
        let thumb_path = parse_uid(&photo.uid).and_then(|uid| {
            self.cache
                .cached_thumbnail_path(&uid, ThumbnailType::Thumbnail.as_i32(), photo.capture_time)
                .map(|p| p.display().to_string())
        });
        PhotoItem {
            uid: photo.uid,
            capture_time: photo.capture_time,
            thumb_path,
            name: photo.name,
            ratio: photo.ratio,
            no_thumb: photo.thumb_state == db::THUMB_NONE,
        }
    }

    /// Thumbnails for `uids`, served from the cache, fetched from the server for
    /// whatever is missing, and — for the photos the server has no thumbnail for
    /// at all — *generated locally* from the full file (see
    /// [`Core::generate_thumbs`]). Requested on demand as tiles scroll into view,
    /// so a cold timeline paints immediately and only the photos actually on
    /// screen cost a round-trip.
    ///
    /// A photo absent from the persisted timeline is skipped: its capture time is
    /// the cache's validity tag, and guessing that would poison the cache.
    fn photo_thumbs(&self, uids: &[NodeUid]) -> Vec<PhotoThumb> {
        let ttype = ThumbnailType::Thumbnail.as_i32();
        let keys: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
        let stored = self.db.photos_by_uid(&keys).unwrap_or_default();
        let tags: HashMap<String, i64> = stored
            .iter()
            .map(|p| (p.uid.clone(), p.capture_time))
            .collect();

        // Ask the server only for photos that are missing a cached thumbnail and
        // haven't already been written off as un-thumbnailable.
        let want: Vec<NodeUid> = uids
            .iter()
            .filter(|uid| {
                let key = uid.to_string();
                stored
                    .iter()
                    .find(|p| p.uid == key)
                    .is_some_and(|p| p.thumb_state != db::THUMB_NONE)
                    && tags.get(&key).is_some_and(|&tag| {
                        self.cache.cached_thumbnail_path(uid, ttype, tag).is_none()
                    })
            })
            .cloned()
            .collect();

        if !want.is_empty() {
            let mut missing = want.clone();
            match self.rt.block_on(
                self.photos()
                    .enumerate_thumbnails(&want, ThumbnailType::Thumbnail),
            ) {
                Ok(thumbs) => {
                    for ft in thumbs {
                        let Some(&tag) = tags.get(&ft.file_uid.to_string()) else {
                            continue;
                        };
                        let Ok(bytes) = ft.result else { continue };
                        if bytes.is_empty() {
                            continue;
                        }
                        if self
                            .cache
                            .store_thumbnail(&ft.file_uid, ttype, tag, &bytes)
                            .is_ok()
                        {
                            missing.retain(|uid| uid != &ft.file_uid);
                            self.record_thumb(&ft.file_uid, db::THUMB_HAVE, ratio_of(&bytes));
                        }
                    }
                }
                // A failed batch is not a verdict: leave every uid in `missing` so
                // the local fallback still gives those tiles an image.
                Err(e) => warn!(error = %e, "batch photo thumbnails failed"),
            }

            // Whatever the server had nothing for gets a thumbnail made from its
            // own bytes — this is what fills in camera photos uploaded by clients
            // that never generated one. Off the request path: a full-size photo
            // takes far longer to fetch than the whole rest of the batch, and the
            // thumbnails that *are* ready must not wait behind it.
            if !missing.is_empty() {
                self.spawn_generate_thumbs(missing, &tags);
            }
        }

        let pending = self.thumb_gen.lock().unwrap();
        uids.iter()
            .map(|uid| PhotoThumb {
                uid: uid.to_string(),
                path: tags.get(&uid.to_string()).and_then(|&tag| {
                    self.cache
                        .cached_thumbnail_path(uid, ttype, tag)
                        .map(|p| p.display().to_string())
                }),
                pending: pending.contains(uid),
            })
            .collect()
    }

    /// Generate the missing thumbnails on the runtime, skipping any photo already
    /// being generated. The uids are marked in-flight before the task starts, so
    /// the reply this call is about to send already reports them as pending.
    fn spawn_generate_thumbs(&self, uids: Vec<NodeUid>, tags: &HashMap<String, i64>) {
        let fresh: Vec<NodeUid> = {
            let mut inflight = self.thumb_gen.lock().unwrap();
            uids.into_iter()
                .filter(|uid| inflight.insert(uid.clone()))
                .collect()
        };
        if fresh.is_empty() {
            return;
        }

        let core = self.clone();
        let tags = tags.clone();
        // `generate_thumbs` blocks on the runtime itself, so it belongs on the
        // blocking pool rather than on an async worker.
        self.rt.spawn_blocking(move || {
            core.generate_thumbs(&fresh, &tags);
            let mut inflight = core.thumb_gen.lock().unwrap();
            for uid in &fresh {
                inflight.remove(uid);
            }
        });
    }

    /// Make thumbnails for photos the server has none for: download each full
    /// file once, scale it to [`THUMB_EDGE`], and store the result in the thumbnail
    /// cache exactly as if the server had served it.
    ///
    /// Bounded by [`THUMB_GEN_CONCURRENCY`] — these are full-size originals, and a
    /// screenful of them at once would saturate the link. A photo whose bytes
    /// can't be decoded (a codec `image` doesn't speak) is marked
    /// [`db::THUMB_NONE`] and never attempted again.
    fn generate_thumbs(&self, uids: &[NodeUid], tags: &HashMap<String, i64>) {
        info!(
            count = uids.len(),
            "generating thumbnails the server has none for"
        );
        let results: Vec<(NodeUid, ThumbAttempt)> = self.rt.block_on(async {
            let mut out = Vec::with_capacity(uids.len());
            for chunk in uids.chunks(THUMB_GEN_CONCURRENCY) {
                let mut set = tokio::task::JoinSet::new();
                for uid in chunk {
                    let client = self.client.clone();
                    let uid = uid.clone();
                    set.spawn(async move {
                        let bytes = match client.download_file(&uid).await {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                warn!(%uid, error = %e, "photo download for thumbnail failed");
                                return (uid, ThumbAttempt::Unavailable);
                            }
                        };
                        // Decoding + scaling a 20 MP JPEG is CPU-bound and would
                        // stall the runtime's worker; hand it to the blocking pool.
                        let made = tokio::task::spawn_blocking(move || scale_thumbnail(&bytes))
                            .await
                            .unwrap_or(None);
                        match made {
                            Some(thumb) => (uid, ThumbAttempt::Made(thumb)),
                            None => (uid, ThumbAttempt::Undecodable),
                        }
                    });
                }
                while let Some(joined) = set.join_next().await {
                    if let Ok(result) = joined {
                        out.push(result);
                    }
                }
            }
            out
        });

        let ttype = ThumbnailType::Thumbnail.as_i32();
        for (uid, attempt) in results {
            match attempt {
                ThumbAttempt::Made(thumb) => {
                    let Some(&tag) = tags.get(&uid.to_string()) else {
                        continue;
                    };
                    match self.cache.store_thumbnail(&uid, ttype, tag, &thumb.bytes) {
                        Ok(()) => self.record_thumb(&uid, db::THUMB_HAVE, Some(thumb.ratio)),
                        Err(e) => warn!(%uid, error = %e, "storing generated thumbnail failed"),
                    }
                }
                // The photo's own bytes aren't an image we can read: no thumbnail
                // will ever exist for it, so stop trying.
                ThumbAttempt::Undecodable => self.record_thumb(&uid, db::THUMB_NONE, None),
                // The download failed — a dropped connection, an expired link. That
                // is not a verdict on the photo: leave its state alone so the next
                // scroll past it tries again.
                ThumbAttempt::Unavailable => {}
            }
        }
    }

    /// Persist what a thumbnail attempt learned about a photo.
    fn record_thumb(&self, uid: &NodeUid, state: i64, ratio: Option<f64>) {
        if let Err(e) = self.db.photo_set_thumb(&uid.to_string(), state, ratio) {
            warn!(%uid, error = %e, "recording thumbnail state failed");
        }
    }

    /// Whether the listing stamped under `key` is older than `ttl` (or was never
    /// fetched).
    fn listing_stale(&self, key: &str, ttl: Duration) -> bool {
        match self.db.state_i64(key).ok().flatten() {
            Some(ms) => now_ms().saturating_sub(ms) >= ttl.as_millis() as i64,
            None => true,
        }
    }

    /// Re-fetch the whole photos timeline and persist it. Returns whether the
    /// account has a photos volume at all.
    async fn refresh_timeline(&self) -> Result<bool, String> {
        let photos = self.photos();
        if photos
            .get_photos_root()
            .await
            .map_err(|e| format!("photos root: {e}"))?
            .is_none()
        {
            let _ = self.db.set_state_i64(PHOTOS_AVAILABLE, 0);
            let _ = self.db.set_state_i64(PHOTOS_SYNCED_MS, now_ms());
            return Ok(false);
        }
        let items = photos
            .enumerate_timeline()
            .await
            .map_err(|e| format!("timeline: {e}"))?;
        let rows: Vec<(String, i64, Option<String>)> = items
            .iter()
            .map(|it| (it.uid.to_string(), it.capture_time, None))
            .collect();
        self.db.photos_replace(&rows).map_err(|e| e.to_string())?;
        let _ = self.db.set_state_i64(PHOTOS_AVAILABLE, 1);
        let _ = self.db.set_state_i64(PHOTOS_SYNCED_MS, now_ms());
        Ok(true)
    }

    /// Refresh the timeline off the request path, so a stale page is still served
    /// at DB speed. At most one refresh runs at a time.
    fn spawn_timeline_refresh(&self) {
        if self.timeline_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let core = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = core.refresh_timeline().await {
                warn!(error = %e, "background timeline refresh failed");
            }
            core.timeline_refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// Download a photo's full content into the content cache, returning its
    /// on-disk path (served from cache when a fresh blob already exists).
    fn open_photo(&self, uid: &NodeUid) -> Result<PathBuf, String> {
        let photos = self.photos();
        let node = self
            .rt
            .block_on(photos.get_node(uid))
            .map_err(|e| format!("photo node: {e}"))?
            .ok_or("photo not found")?;
        let (mtime, size) = (node.modification_time, node_size(&node));
        if let Some(p) = self.cache.cached_content_path(uid, mtime, size) {
            return Ok(p);
        }
        let bytes = self
            .download_photo_tracked(&photos, uid, &node.name, size)
            .map_err(|e| format!("download photo: {e}"))?;
        self.cache
            .store(uid, mtime, size, &bytes)
            .map_err(|e| format!("cache store: {e}"))?;
        Ok(self.cache.content_path(uid))
    }

    /// Download the full content of the Drive file at mountpoint-relative `rel`
    /// into the content cache, returning its on-disk path (served from cache
    /// when a fresh blob already exists). Lets a front-end open the file with
    /// the user's default application without pinning it.
    fn open_file(&self, rel: &Path) -> Result<PathBuf, String> {
        let (ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let (name, mtime, size) = {
            let st = self.state.lock().unwrap();
            let e = st.entries.get(&ino).ok_or("node vanished")?;
            if !e.node.is_file() {
                return Err("not a regular file".into());
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
            .map_err(|e| format!("download: {e}"))?;
        self.cache
            .store(&uid, mtime, size, &bytes)
            .map_err(|e| format!("cache store: {e}"))?;
        Ok(self.cache.content_path(&uid))
    }

    /// Drop the cached child listing of `rel`'s parent directory so the next
    /// `ListDir` re-enumerates it from the server. No-op when the parent can't be
    /// resolved (e.g. `rel` is the root). Resolves the parent without holding the
    /// state lock, then invalidates under it.
    fn invalidate_parent_listing(&self, rel: &Path) {
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        if let Ok((pino, _)) = self.resolve_path(parent) {
            self.state.lock().unwrap().invalidate_listing(pino);
        }
    }

    /// Rename a file or folder to `new_name`. `rel` is mountpoint-relative.
    /// Mirrors the FUSE `rename` write path: rename on the remote, forget the
    /// node so it re-interns under its new name, and drop the parent listing so
    /// the next `ListDir` re-enumerates.
    fn rename(&self, rel: &Path, new_name: &str) -> Result<String, String> {
        if new_name.is_empty() || new_name.contains('/') {
            return Err(format!("invalid name: {new_name:?}"));
        }
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        self.rt
            .block_on(self.client.rename_node(&uid, new_name, None))
            .map_err(|e| format!("rename: {e}"))?;
        self.state.lock().unwrap().forget(&uid);
        self.invalidate_parent_listing(rel);
        Ok(new_name.to_string())
    }

    /// Move a file or folder into the folder at `new_parent_rel`. Both paths are
    /// mountpoint-relative. Forgets the node and invalidates both the source and
    /// destination listings so each re-enumerates on next access.
    fn move_to(&self, rel: &Path, new_parent_rel: &Path) -> Result<String, String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let (pino, new_parent_uid) = self
            .resolve_path(new_parent_rel)
            .map_err(|e| format!("resolve new parent: {e:?}"))?;
        self.rt
            .block_on(self.client.move_node(&uid, &new_parent_uid))
            .map_err(|e| format!("move: {e}"))?;
        let name = self
            .state
            .lock()
            .unwrap()
            .forget(&uid)
            .map(|(_, n)| n)
            .unwrap_or_default();
        self.invalidate_parent_listing(rel);
        self.state.lock().unwrap().invalidate_listing(pino);
        Ok(name)
    }

    /// Trash a file or folder. `rel` is mountpoint-relative. Forgets the node,
    /// evicts any cached content, and invalidates the parent listing.
    fn delete(&self, rel: &Path) -> Result<String, String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        self.rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(&uid)))
            .map_err(|e| format!("trash: {e}"))?;
        let name = self
            .state
            .lock()
            .unwrap()
            .forget(&uid)
            .map(|(_, n)| n)
            .unwrap_or_default();
        self.cache.evict(&uid);
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
    fn list_trash(&self) -> Result<Vec<DirEntry>, String> {
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
            .map_err(|e| e.to_string())?
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
    async fn refresh_trash(&self) -> Result<(), String> {
        let uids = self
            .client
            .enumerate_trash_node_uids()
            .await
            .map_err(|e| format!("enumerate trash: {e}"))?;
        let nodes = if uids.is_empty() {
            Vec::new()
        } else {
            self.client
                .enumerate_nodes(&uids)
                .await
                .map_err(|e| format!("enumerate nodes: {e}"))?
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
        self.db.trash_replace(&items).map_err(|e| e.to_string())?;
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

    /// Parse wire uids (`volume~link`) into [`NodeUid`]s, rejecting the whole
    /// batch if any is malformed — a partial trash mutation is worse than none.
    fn parse_uids(uids: &[String]) -> Result<Vec<NodeUid>, String> {
        if uids.is_empty() {
            return Err("no nodes given".to_string());
        }
        uids.iter()
            .map(|u| parse_uid(u).ok_or_else(|| format!("invalid uid: {u}")))
            .collect()
    }

    /// Restore trashed nodes to the folders they were trashed from. The parents
    /// are read *before* the restore — a restored node reappears in a listing the
    /// daemon may already have cached, so each destination folder is invalidated
    /// and re-enumerated on next access.
    fn restore(&self, uids: &[String]) -> Result<usize, String> {
        let parsed = Self::parse_uids(uids)?;
        let parents: Vec<NodeUid> = self
            .rt
            .block_on(self.client.enumerate_nodes(&parsed))
            .map_err(|e| format!("enumerate nodes: {e}"))?
            .into_iter()
            .filter_map(|n| n.parent_uid)
            .collect();
        self.rt
            .block_on(self.client.restore_nodes(&parsed))
            .map_err(|e| format!("restore: {e}"))?;
        {
            let mut st = self.state.lock().unwrap();
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
    fn delete_forever(&self, uids: &[String]) -> Result<usize, String> {
        let parsed = Self::parse_uids(uids)?;
        self.rt
            .block_on(self.client.delete_nodes(&parsed))
            .map_err(|e| format!("delete: {e}"))?;
        self.drop_local(&parsed);
        self.invalidate_trash();
        Ok(parsed.len())
    }

    /// Permanently delete everything in the trash. The uids are listed first so
    /// the blobs of items trashed by *another* client — which this daemon may
    /// still hold in its cache — are reclaimed too, not just the ones it trashed.
    fn empty_trash(&self) -> Result<usize, String> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_trash_node_uids())
            .map_err(|e| format!("enumerate trash: {e}"))?;
        self.rt
            .block_on(self.client.empty_trash())
            .map_err(|e| format!("empty trash: {e}"))?;
        self.drop_local(&uids);
        self.invalidate_trash();
        Ok(uids.len())
    }

    /// Forget every trace of nodes that no longer exist anywhere: their inode and
    /// DB row, and their cached content.
    fn drop_local(&self, uids: &[NodeUid]) {
        let mut st = self.state.lock().unwrap();
        for uid in uids {
            st.forget(uid);
        }
        drop(st);
        for uid in uids {
            self.cache.evict(uid);
        }
    }

    /// Create a folder named `name` under the mountpoint-relative `parent_rel`.
    /// Interns the new node directly so it shows up without a re-enumeration.
    fn create_folder(&self, parent_rel: &Path, name: &str) -> Result<String, String> {
        if name.is_empty() || name.contains('/') {
            return Err(format!("invalid name: {name:?}"));
        }
        let (pino, parent_uid) = self
            .resolve_path(parent_rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        self.ensure_children(pino)
            .map_err(|e| format!("enumerate: {e:?}"))?;
        let new_uid = self
            .rt
            .block_on(
                self.client
                    .create_folder(&parent_uid, name, Some(now_secs())),
            )
            .map_err(|e| format!("create folder: {e}"))?;
        let node = self
            .fetch_node(&new_uid)
            .map_err(|e| format!("fetch node: {e:?}"))?;
        let mut st = self.state.lock().unwrap();
        let ino = st.intern(pino, node);
        if let Some(kids) = st.children.get_mut(&pino)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        Ok(name.to_string())
    }

    /// Upload a file named `name` with content `bytes` into the
    /// mountpoint-relative `parent_rel` folder. Interns the new node directly.
    fn upload(&self, parent_rel: &Path, name: &str, bytes: &[u8]) -> Result<String, String> {
        if name.is_empty() || name.contains('/') {
            return Err(format!("invalid name: {name:?}"));
        }
        let (pino, parent_uid) = self
            .resolve_path(parent_rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        self.ensure_children(pino)
            .map_err(|e| format!("enumerate: {e:?}"))?;
        let guard = self
            .transfers
            .begin(name, "", TransferDirection::Upload, bytes.len() as u64);
        let reader = CountingReader::new(std::io::Cursor::new(bytes), &guard);
        let new_uid = self
            .rt
            .block_on(self.client.upload_file_from(
                &parent_uid,
                name,
                media_type_for(name),
                reader,
                bytes.len() as i64,
                Vec::new(),
                None,
                false,
            ))
            .map_err(|e| format!("upload: {e}"))?;
        drop(guard);
        let node = self
            .fetch_node(&new_uid)
            .map_err(|e| format!("fetch node: {e:?}"))?;
        let mut st = self.state.lock().unwrap();
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
    fn upload_paths(&self, parent_rel: &Path, sources: &[PathBuf]) -> Result<UploadStats, String> {
        let (pino, parent_uid) = self
            .resolve_path(parent_rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        self.ensure_children(pino)
            .map_err(|e| format!("enumerate: {e:?}"))?;

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
                            let mut st = self.state.lock().unwrap();
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
    ) -> Result<(u64, NodeUid), String> {
        if name.is_empty() || name.contains('/') {
            return Err(format!("invalid folder name: {name:?}"));
        }
        self.ensure_children(pino)
            .map_err(|e| format!("enumerate: {e:?}"))?;
        {
            let st = self.state.lock().unwrap();
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
            .map_err(|e| format!("create folder {name}: {e}"))?;
        let node = self
            .fetch_node(&new_uid)
            .map_err(|e| format!("fetch node: {e:?}"))?;
        let mut st = self.state.lock().unwrap();
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
    ) -> Result<(), String> {
        let meta =
            std::fs::symlink_metadata(src).map_err(|e| format!("stat {}: {e}", src.display()))?;
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("unusable name: {}", src.display()))?
            .to_string();

        if meta.is_dir() {
            job.detail(&name);
            let (child_ino, child_uid) = self.ensure_remote_folder(pino, parent_uid, &name)?;
            *folders += 1;
            let mut entries: Vec<PathBuf> = std::fs::read_dir(src)
                .map_err(|e| format!("read dir {}: {e}", src.display()))?
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
                return Err(format!("invalid file name: {name:?}"));
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

    // ---- devices ----------------------------------------------------------

    /// List the account's registered devices.
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, String> {
        let devices = self
            .rt
            .block_on(self.client.enumerate_devices())
            .map_err(|e| format!("list devices: {e}"))?;
        Ok(devices
            .into_iter()
            .map(|d| DeviceInfo {
                uid: d.uid.to_string(),
                name: d.name.unwrap_or_else(|_| "(unnamed device)".to_string()),
                device_type: device_type_str(d.device_type).to_string(),
                last_sync: d.last_sync_time,
            })
            .collect())
    }

    /// Rename a device by its uid.
    fn rename_device(&self, uid: &str, name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("device name must not be empty".to_string());
        }
        let device_uid = DeviceUid::from(uid);
        self.rt
            .block_on(self.client.rename_device(&device_uid, name))
            .map_err(|e| format!("rename device: {e}"))?;
        Ok(())
    }

    /// Delete (deregister) a device by its uid.
    fn delete_device(&self, uid: &str) -> Result<(), String> {
        let device_uid = DeviceUid::from(uid);
        self.rt
            .block_on(self.client.delete_device(&device_uid))
            .map_err(|e| format!("delete device: {e}"))?;
        Ok(())
    }

    // ---- device folder sync (devices.md, Phase 1) -------------------------

    /// Auto-register (or recover) this machine as a Proton Drive Device, caching
    /// it so restarts reuse the same device. Recovery matches an existing remote
    /// Linux device by name before creating a new one, so a lost local record
    /// doesn't orphan the device's root folder.
    fn ensure_device(&self) -> Result<StoredDevice, String> {
        let name = this_hostname();
        // Enumerate the remote devices once: used both to validate any cached
        // record and to recover an existing device by name.
        let remote = self
            .rt
            .block_on(self.client.enumerate_devices())
            .map_err(|e| format!("enumerate devices: {e}"))?;

        // A cached device is only trustworthy if it still exists remotely. A
        // device deleted from another client (or the web UI) leaves a stale row
        // whose root folder is gone, so creating folders under it fails with
        // "parent node is not a folder". Re-register in that case.
        if let Some(dev) = self.db.device_get().map_err(|e| format!("db: {e:?}"))? {
            if remote.iter().any(|d| d.uid.to_string() == dev.uid) {
                return Ok(dev);
            }
            warn!(uid = %dev.uid, "cached device is gone remotely; re-registering");
        }

        // Recover: an existing remote Linux device with the same name is ours.
        let existing = remote.into_iter().find(|d| {
            d.device_type == DeviceType::Linux && d.name.as_deref().ok() == Some(name.as_str())
        });
        let dev = match existing {
            Some(d) => StoredDevice {
                uid: d.uid.to_string(),
                share_id: d.share_id.to_string(),
                root_uid: d.root_folder_uid.to_string(),
                name,
                created: d.creation_time,
            },
            None => {
                let d = self
                    .rt
                    .block_on(self.client.create_device(&name, DeviceType::Linux))
                    .map_err(|e| format!("create device: {e}"))?;
                StoredDevice {
                    uid: d.uid.to_string(),
                    share_id: d.share_id.to_string(),
                    root_uid: d.root_folder_uid.to_string(),
                    name,
                    created: d.creation_time,
                }
            }
        };
        self.db.device_set(&dev).map_err(|e| format!("db: {e:?}"))?;
        Ok(dev)
    }

    /// An untrashed folder named `name` directly under the device root, if one
    /// already exists.
    fn find_device_child_folder(
        &self,
        root_uid: &NodeUid,
        name: &str,
    ) -> Result<Option<NodeUid>, String> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_folder_children_node_uids(root_uid))
            .map_err(|e| format!("list device root: {e}"))?;
        if uids.is_empty() {
            return Ok(None);
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| format!("resolve device root children: {e}"))?;
        Ok(nodes
            .into_iter()
            .find(|n| n.is_folder() && !n.trashed && n.name == name)
            .map(|n| n.uid))
    }

    /// Add a local folder to this device's sync set: register the device if
    /// needed, create a matching folder under the device root, upload the local
    /// tree into it once, and record the mapping. Phase 1 is a one-shot upload —
    /// the two-way engine (Phase 2) reconciles later changes.
    fn add_sync_folder(&self, local: &Path) -> Result<StoredSyncFolder, String> {
        let meta =
            std::fs::metadata(local).map_err(|e| format!("stat {}: {e}", local.display()))?;
        if !meta.is_dir() {
            return Err(format!("{} is not a directory", local.display()));
        }
        let local = local
            .canonicalize()
            .map_err(|e| format!("canonicalize {}: {e}", local.display()))?;
        let local_str = local.to_string_lossy().to_string();

        // Reject duplicates up front for a clear error (UNIQUE would also catch it).
        if self
            .db
            .sync_folder_list()
            .map_err(|e| format!("db: {e:?}"))?
            .iter()
            .any(|f| f.local_path == local_str)
        {
            return Err(format!("{} is already synced", local.display()));
        }

        let name = local
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("unusable folder name: {}", local.display()))?
            .to_string();

        let device = self.ensure_device()?;
        let root_uid = parse_uid(&device.root_uid)
            .ok_or_else(|| format!("bad device root uid: {}", device.root_uid))?;

        // The synced folder's remote root: the folder under the device root named
        // after the local basename. Reuse an existing one rather than creating a
        // second folder with the same name — re-adding a folder (after a removal, or
        // after a failed add that had already created it) must land back on the
        // original, not leave the user with two "Downloads" in their Drive. The
        // reconcile treats an existing remote tree correctly: unmatched paths read as
        // a conflict, not as data loss.
        let remote_root = match self.find_device_child_folder(&root_uid, &name)? {
            Some(uid) => {
                info!(name, "reusing existing device folder");
                uid
            }
            None => self
                .rt
                .block_on(
                    self.client
                        .create_folder(&root_uid, &name, Some(now_secs())),
                )
                .map_err(|e| format!("create device folder {name}: {e}"))?,
        };

        let id = self
            .db
            .sync_folder_add(&local_str, &remote_root.to_string(), &device.share_id)
            .map_err(|e| format!("db: {e:?}"))?;

        // Hand the initial upload to the sync engine: an empty baseline against a
        // full local tree reconciles as "upload everything", and the folder is
        // added to the filesystem watch set in the same pass.
        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
        let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));

        info!(local = %local.display(), id, "added sync folder");
        self.db
            .sync_folder_get(id)
            .map_err(|e| format!("db: {e:?}"))?
            .ok_or_else(|| "sync folder vanished after insert".to_string())
    }

    /// List this device's synced folders for the front-ends, each carrying the
    /// live progress of its pass when one is running.
    fn list_sync_folders(&self) -> Result<Vec<SyncFolderInfo>, String> {
        let progress = self.sync_progress.lock().unwrap();
        Ok(self
            .db
            .sync_folder_list()
            .map_err(|e| format!("db: {e:?}"))?
            .into_iter()
            .map(|f| {
                let live = progress.get(&f.id).cloned();
                sync_folder_info(f, live)
            })
            .collect())
    }

    /// Everything the daemon is chewing on that isn't moving bytes, for
    /// `GetQueueStatus`: the registered jobs (bulk-upload scans, the local index)
    /// plus a synthesized job per running sync pass, so one Activity view answers
    /// "is anything still happening?" without also polling `ListSyncFolders`.
    ///
    /// The sync passes are folded in here rather than tracked as registry jobs
    /// because the Devices page needs them per folder anyway
    /// ([`SyncFolderInfo::progress`]) — this keeps one source of truth and hits
    /// the db only while a pass is actually running.
    fn jobs_snapshot(&self) -> Vec<JobItem> {
        let mut jobs = self.transfers.jobs_snapshot();
        let mut passes: Vec<(i64, SyncProgress)> = self
            .sync_progress
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| (*id, p.clone()))
            .collect();
        if passes.is_empty() {
            return jobs;
        }
        passes.sort_by_key(|(id, _)| *id);

        let names: HashMap<i64, String> = self
            .db
            .sync_folder_list()
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.id, f.local_path))
            .collect();
        for (id, p) in passes {
            // The row is titled with the folder's own name; the full local path
            // is what the Devices page shows, and is far too long for this line.
            let folder = names
                .get(&id)
                .and_then(|path| Path::new(path).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "folder".to_string());
            jobs.push(match p.phase {
                // No counts exist until the walk has classified something, so a
                // scan reports indeterminate rather than a fake 0 of 0.
                SyncPhase::Scanning => JobItem {
                    title: format!("Checking {folder}"),
                    detail: "Looking for changes".to_string(),
                    done: 0,
                    total: 0,
                },
                SyncPhase::Applying => JobItem {
                    title: format!("Syncing {folder}"),
                    detail: p.current.clone(),
                    done: p.done as u64,
                    total: p.total.max(p.done) as u64,
                },
            });
        }
        jobs
    }

    /// The lock guarding sync-folder `id` against concurrent reconcile/mode-switch.
    pub(crate) fn sync_lock(&self, id: i64) -> Arc<Mutex<()>> {
        self.sync_locks
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .clone()
    }

    /// Remove a synced folder from the sync set. `delete_remote` also deletes its
    /// folder under the device root; otherwise the cloud copy is left in place.
    fn remove_sync_folder(&self, id: i64, delete_remote: bool) -> Result<(), String> {
        let folder = self
            .db
            .sync_folder_get(id)
            .map_err(|e| format!("db: {e:?}"))?
            .ok_or_else(|| format!("no synced folder with id {id}"))?;
        if delete_remote
            && let Some(uid) = parse_uid(&folder.remote_uid)
            && let Err(e) = self.rt.block_on(self.client.trash_nodes(&[uid]))
        {
            warn!(id, error = %e, "delete remote device folder failed");
        }
        if !self
            .db
            .sync_folder_remove(id)
            .map_err(|e| format!("db: {e:?}"))?
        {
            return Err(format!("no synced folder with id {id}"));
        }
        // Stop watching the folder we just dropped.
        let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
        Ok(())
    }

    /// Trigger a reconcile: one folder by id, or every folder when `id` is `None`.
    fn sync_now(&self, id: Option<i64>) {
        let _ = match id {
            Some(id) => self.sync_tx.send(sync::SyncMsg::Reconcile(id)),
            None => self.sync_tx.send(sync::SyncMsg::ReconcileAll),
        };
    }

    /// A sibling Core that shares this one's client/rt/cache/db (and transfer,
    /// activity, mount registries) but gets a **fresh, empty `State`** — its own
    /// inode space starting at [`ROOT_INO`]. Used to root a secondary FUSE session
    /// at an `ondemand` sync folder without colliding with the main mount's inodes
    /// (devices.md Phase 3).
    fn fork_state(&self) -> Core {
        let mut fork = self.clone();
        fork.state = Arc::new(Mutex::new(State {
            entries: HashMap::new(),
            by_uid: HashMap::new(),
            children: HashMap::new(),
            next_ino: 2,
            handles: HashMap::new(),
            next_fh: 1,
            db: self.db.clone(),
        }));
        fork
    }

    /// Flip a synced folder between `mirror` (full local copy + two-way sync) and
    /// `ondemand` (a FUSE mount over the local path; no local storage). Returns a
    /// human message for the reply.
    ///
    /// - **mirror→ondemand**: require the folder in-sync (`idle`), stop watching it,
    ///   evict the local files to reclaim disk, then mount a secondary `ProtonFs`
    ///   rooted at the folder's remote node over its local path.
    /// - **ondemand→mirror**: unmount, clear the stale baseline (the local tree was
    ///   evicted), then hand the folder back to the engine, which re-downloads it.
    fn set_sync_folder_mode(&self, id: i64, mode: &str) -> Result<String, String> {
        if mode != "mirror" && mode != "ondemand" {
            return Err(format!("unknown mode {mode:?} (want mirror|ondemand)"));
        }
        // Hold the folder's lock for the whole switch so no reconcile pass can be
        // running over the tree we are about to evict (or start while we mount over
        // it). A pass in flight holds the lock for its full duration, so `try_lock`
        // failing is exactly "still syncing" — and it is the only reliable signal:
        // the `state` column is still `idle` in the window between `add_sync_folder`
        // inserting the row and the engine picking it up.
        let lock = self.sync_lock(id);
        let Ok(_guard) = lock.try_lock() else {
            return Err("folder is syncing right now — wait for it to finish".to_string());
        };
        // Re-read under the lock: a pass that finished while we waited may have
        // changed `state`.
        let folder = self
            .db
            .sync_folder_get(id)
            .map_err(|e| format!("db: {e:?}"))?
            .ok_or_else(|| format!("no synced folder with id {id}"))?;
        if folder.mode == mode {
            return Ok(format!("already {mode}"));
        }
        let local = PathBuf::from(&folder.local_path);

        match mode {
            "ondemand" => {
                // Only flip a folder that is fully in sync — a failed reconcile means
                // local edits could still be un-uploaded, and we are about to delete
                // the local copy.
                if folder.state != "idle" {
                    return Err(format!(
                        "folder is '{}', not idle — sync it before switching to on-demand",
                        folder.state
                    ));
                }
                let root_uid = parse_uid(&folder.remote_uid)
                    .ok_or_else(|| format!("bad remote uid: {}", folder.remote_uid))?;
                let root = self
                    .rt
                    .block_on(self.client.enumerate_nodes(std::slice::from_ref(&root_uid)))
                    .map_err(|e| format!("fetch remote root: {e}"))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| "remote folder not found".to_string())?;

                // Reclaim the disk: empty the local dir (keep it as the mountpoint).
                evict_dir_contents(&local)
                    .map_err(|e| format!("evict {}: {e}", local.display()))?;

                let session = self.spawn_ondemand_mount(&local, root)?;
                self.mounts.lock().unwrap().insert(id, session);
                // Persist the mode only now that the mount is actually up. Writing it
                // first would strand the folder on any failure below: the engine skips
                // non-mirror folders, so an `ondemand` row with no mount is a folder
                // that is neither mirrored nor browsable, and nothing retries it.
                // Failing before this point leaves it `mirror`, and the next pass
                // re-downloads whatever eviction removed.
                self.db
                    .sync_folder_set_mode(id, "ondemand")
                    .map_err(|e| format!("db: {e:?}"))?;
                let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
                self.db.sync_folder_set_state(id, "idle", now_secs()).ok();
                info!(id, path = %local.display(), "mounted sync folder on-demand");
                Ok(format!("{} is now on-demand", local.display()))
            }
            _ => {
                // ondemand→mirror: tear down the secondary mount first.
                if let Some(session) = self.mounts.lock().unwrap().remove(&id)
                    && let Err(e) = session.umount_and_join()
                {
                    warn!(id, error = %e, "unmount on-demand folder failed");
                }
                // The evicted local tree makes the old baseline lie ("everything
                // deleted locally"); clear it so the reconcile is a pure download.
                self.db
                    .sync_entries_clear(id)
                    .map_err(|e| format!("db: {e:?}"))?;
                self.db
                    .sync_folder_set_mode(id, "mirror")
                    .map_err(|e| format!("db: {e:?}"))?;
                let _ = self.sync_tx.send(sync::SyncMsg::Rewatch);
                let _ = self.sync_tx.send(sync::SyncMsg::Reconcile(id));
                info!(id, path = %local.display(), "restored sync folder to mirror");
                Ok(format!(
                    "{} is mirroring again; downloading",
                    local.display()
                ))
            }
        }
    }

    /// Spawn a secondary FUSE session rooted at `root` over `local` on a forked
    /// inode space. Clears any stale kernel mount first (a crashed run can leave
    /// one, which would fail the fresh mount with EBUSY).
    fn spawn_ondemand_mount(&self, local: &Path, root: Node) -> Result<BackgroundSession, String> {
        clear_stale_mount(local);
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName("protondrive".to_string()),
            MountOption::Subtype("protondrive".to_string()),
            MountOption::DefaultPermissions,
        ];
        let fs = ProtonFs::new(self.fork_state(), root);
        Session::new(fs, local, &config)
            .and_then(|s| s.spawn())
            .map_err(|e| format!("mount {}: {e}", local.display()))
    }

    /// Re-establish FUSE mounts for folders left in `ondemand` mode across a daemon
    /// restart (their local dirs are empty on disk — the files live in the cloud).
    /// Best-effort per folder: a missing local path or a failed remote fetch marks
    /// the folder `error` and moves on rather than aborting the rest. Runs on its
    /// own thread from `mount` so the network fetches never block startup
    /// (devices.md Phase 4).
    fn restore_ondemand_mounts(&self) {
        let folders = match self.db.sync_folder_list() {
            Ok(f) => f,
            Err(e) => {
                warn!(error = ?e, "restore on-demand: cannot list folders");
                return;
            }
        };
        for folder in folders {
            if folder.mode != "ondemand" {
                continue;
            }
            let local = PathBuf::from(&folder.local_path);
            if !local.is_dir() {
                warn!(id = folder.id, path = %local.display(), "restore on-demand: local path missing");
                let _ = self
                    .db
                    .sync_folder_set_state(folder.id, "error", now_secs());
                continue;
            }
            // An `ondemand` folder's local dir is empty by construction — the switch
            // evicts it. Finding files there means the row is lying (a switch that
            // died between persisting the mode and evicting), and mounting over them
            // would hide real local data behind the remote tree. Leave the files
            // alone and let the user resolve it.
            match dir_is_empty(&local) {
                Ok(true) => {}
                Ok(false) => {
                    warn!(
                        id = folder.id,
                        path = %local.display(),
                        "restore on-demand: local dir is not empty; refusing to mount over it"
                    );
                    let _ = self
                        .db
                        .sync_folder_set_state(folder.id, "error", now_secs());
                    continue;
                }
                Err(e) => {
                    warn!(id = folder.id, path = %local.display(), error = %e, "restore on-demand: cannot read local dir");
                    let _ = self
                        .db
                        .sync_folder_set_state(folder.id, "error", now_secs());
                    continue;
                }
            }
            let Some(root_uid) = parse_uid(&folder.remote_uid) else {
                warn!(id = folder.id, "restore on-demand: bad remote uid");
                continue;
            };
            let root = match self
                .rt
                .block_on(self.client.enumerate_nodes(std::slice::from_ref(&root_uid)))
            {
                Ok(v) => match v.into_iter().next() {
                    Some(n) => n,
                    None => {
                        warn!(id = folder.id, "restore on-demand: remote folder gone");
                        let _ = self
                            .db
                            .sync_folder_set_state(folder.id, "error", now_secs());
                        continue;
                    }
                },
                Err(e) => {
                    warn!(id = folder.id, error = %e, "restore on-demand: fetch remote failed");
                    let _ = self
                        .db
                        .sync_folder_set_state(folder.id, "error", now_secs());
                    continue;
                }
            };
            match self.spawn_ondemand_mount(&local, root) {
                Ok(session) => {
                    self.mounts.lock().unwrap().insert(folder.id, session);
                    let _ = self.db.sync_folder_set_state(folder.id, "idle", now_secs());
                    info!(id = folder.id, path = %local.display(), "remounted on-demand folder");
                }
                Err(e) => {
                    warn!(id = folder.id, error = %e, "restore on-demand: mount failed");
                    let _ = self
                        .db
                        .sync_folder_set_state(folder.id, "error", now_secs());
                }
            }
        }
    }

    // ---- sharing a node ---------------------------------------------------

    /// Invite Proton and/or external emails to the node at `rel` at `role`.
    /// Returns `(proton_invited, external_invited)`.
    fn share_node(
        &self,
        rel: &Path,
        emails: &[String],
        role: &str,
        message: Option<&str>,
    ) -> Result<(usize, usize), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let role = role_from_str(role)?;
        let invitees: Vec<(String, MemberRole)> =
            emails.iter().map(|e| (e.clone(), role)).collect();
        self.rt
            .block_on(self.client.invite_users(&uid, &invitees, message))
            .map_err(|e| format!("share: {e}"))
    }

    /// List the members, pending invitations and public link of the node at `rel`.
    fn list_share(&self, rel: &Path) -> Result<(Vec<ShareEntry>, Option<PublicLinkInfo>), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;

        let mut entries = Vec::new();
        for m in self
            .rt
            .block_on(self.client.list_share_members(&uid))
            .map_err(|e| format!("list members: {e}"))?
        {
            entries.push(ShareEntry {
                id: m.membership_id.to_string(),
                email: m.email,
                role: role_to_str(m.role).to_string(),
                kind: ShareEntryKind::Member,
            });
        }
        for inv in self
            .rt
            .block_on(self.client.list_share_invitations(&uid))
            .map_err(|e| format!("list invitations: {e}"))?
        {
            entries.push(ShareEntry {
                id: inv.invitation_id,
                email: inv.invitee_email,
                role: role_to_str(inv.role).to_string(),
                kind: ShareEntryKind::ProtonInvite,
            });
        }
        for ext in self
            .rt
            .block_on(self.client.list_external_invitations(&uid))
            .map_err(|e| format!("list external invitations: {e}"))?
        {
            entries.push(ShareEntry {
                id: ext.invitation_id,
                email: ext.invitee_email,
                role: role_to_str(ext.role).to_string(),
                kind: ShareEntryKind::ExternalInvite,
            });
        }

        let link = self
            .rt
            .block_on(self.client.get_public_link(&uid))
            .map_err(|e| format!("get public link: {e}"))?
            .map(public_link_info);

        Ok((entries, link))
    }

    /// Change the role of a member or pending Proton invitation on the node at
    /// `rel`. External invitations have no role-update endpoint.
    fn update_share_role(
        &self,
        rel: &Path,
        id: &str,
        kind: ShareEntryKind,
        role: &str,
    ) -> Result<(), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let role = role_from_str(role)?;
        match kind {
            ShareEntryKind::Member => {
                let member = self
                    .rt
                    .block_on(self.client.list_share_members(&uid))
                    .map_err(|e| format!("list members: {e}"))?
                    .into_iter()
                    .find(|m| m.membership_id.to_string() == id)
                    .ok_or_else(|| "member not found".to_string())?;
                self.rt
                    .block_on(self.client.update_member_role(&member, role))
                    .map_err(|e| format!("update role: {e}"))
            }
            ShareEntryKind::ProtonInvite => {
                let inv = self
                    .rt
                    .block_on(self.client.list_share_invitations(&uid))
                    .map_err(|e| format!("list invitations: {e}"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| "invitation not found".to_string())?;
                self.rt
                    .block_on(self.client.update_invitation_role(&inv, role))
                    .map_err(|e| format!("update role: {e}"))
            }
            ShareEntryKind::ExternalInvite => {
                Err("an external invitation's role cannot be changed".to_string())
            }
        }
    }

    /// Remove a member, pending Proton invite, or external invite from the node
    /// at `rel`.
    fn remove_share_entry(&self, rel: &Path, id: &str, kind: ShareEntryKind) -> Result<(), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        match kind {
            ShareEntryKind::Member => {
                let member = self
                    .rt
                    .block_on(self.client.list_share_members(&uid))
                    .map_err(|e| format!("list members: {e}"))?
                    .into_iter()
                    .find(|m| m.membership_id.to_string() == id)
                    .ok_or_else(|| "member not found".to_string())?;
                self.rt
                    .block_on(self.client.remove_member(&member))
                    .map_err(|e| format!("remove member: {e}"))
            }
            ShareEntryKind::ProtonInvite => {
                let inv = self
                    .rt
                    .block_on(self.client.list_share_invitations(&uid))
                    .map_err(|e| format!("list invitations: {e}"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| "invitation not found".to_string())?;
                self.rt
                    .block_on(self.client.delete_invitation(&inv))
                    .map_err(|e| format!("revoke invitation: {e}"))
            }
            ShareEntryKind::ExternalInvite => {
                let ext = self
                    .rt
                    .block_on(self.client.list_external_invitations(&uid))
                    .map_err(|e| format!("list external invitations: {e}"))?
                    .into_iter()
                    .find(|i| i.invitation_id == id)
                    .ok_or_else(|| "external invitation not found".to_string())?;
                self.rt
                    .block_on(self.client.delete_external_invitation(&ext))
                    .map_err(|e| format!("revoke external invitation: {e}"))
            }
        }
    }

    /// Create a public link on the node at `rel`, returning its info (with URL).
    fn create_public_link(
        &self,
        rel: &Path,
        role: &str,
        password: Option<&str>,
        expires: Option<i64>,
    ) -> Result<PublicLinkInfo, String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let role = role_from_str(role)?;
        let link = self
            .rt
            .block_on(
                self.client
                    .create_public_link(&uid, role, password, expires),
            )
            .map_err(|e| format!("create public link: {e}"))?;
        Ok(public_link_info(link))
    }

    /// Remove the public link `id` from the node at `rel`.
    fn remove_public_link(&self, rel: &Path, id: &str) -> Result<(), String> {
        let (_ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let link = self
            .rt
            .block_on(self.client.get_public_link(&uid))
            .map_err(|e| format!("get public link: {e}"))?
            .filter(|l| l.public_link_id == id)
            .ok_or_else(|| "public link not found".to_string())?;
        self.rt
            .block_on(self.client.remove_public_link(&link))
            .map_err(|e| format!("remove public link: {e}"))
    }

    // ---- shared with me ---------------------------------------------------

    /// List nodes shared with me that I have accepted.
    fn list_shared_with_me(&self) -> Result<Vec<DirEntry>, String> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_shared_with_me_node_uids())
            .map_err(|e| format!("enumerate shared: {e}"))?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| format!("enumerate nodes: {e}"))?;
        Ok(nodes
            .into_iter()
            .map(|n| {
                let is_dir = n.is_folder();
                let size = match &n.kind {
                    NodeKind::File {
                        claimed_size,
                        total_size_on_storage,
                        ..
                    } => claimed_size.unwrap_or(*total_size_on_storage).max(0) as u64,
                    NodeKind::Folder => 0,
                };
                DirEntry {
                    name: n.name,
                    is_dir,
                    size,
                    modified: 0,
                    pinned: false,
                    cached: false,
                    uid: n.uid.to_string(),
                    path: String::new(),
                }
            })
            .collect())
    }

    /// Leave a shared node by its uid.
    fn leave_shared(&self, uid: &str) -> Result<(), String> {
        let uid = parse_uid(uid).ok_or_else(|| format!("invalid uid: {uid}"))?;
        self.rt
            .block_on(self.client.leave_shared_node(&uid))
            .map_err(|e| format!("leave shared: {e}"))
    }

    // ---- incoming invitations ---------------------------------------------

    /// List invitations addressed to me, pending accept or reject.
    fn list_invitations(&self) -> Result<Vec<InvitationInfo>, String> {
        let invitations = self
            .rt
            .block_on(self.client.list_incoming_invitations())
            .map_err(|e| format!("list invitations: {e}"))?;
        Ok(invitations
            .into_iter()
            .map(|i| InvitationInfo {
                id: i.invitation_id,
                inviter_email: i.inviter_email,
                name: i.node_name,
                is_dir: i.is_folder,
            })
            .collect())
    }

    /// Accept an invitation addressed to me by its id.
    fn accept_invitation(&self, id: &str) -> Result<(), String> {
        self.rt
            .block_on(self.client.accept_invitation(id))
            .map_err(|e| format!("accept invitation: {e}"))
    }

    /// Reject an invitation addressed to me by its id.
    fn reject_invitation(&self, id: &str) -> Result<(), String> {
        self.rt
            .block_on(self.client.reject_invitation(id))
            .map_err(|e| format!("reject invitation: {e}"))
    }

    // ---- bookmarks --------------------------------------------------------

    /// List public links saved to my account.
    fn list_bookmarks(&self) -> Result<Vec<BookmarkInfo>, String> {
        let bookmarks = self
            .rt
            .block_on(self.client.list_bookmarks())
            .map_err(|e| format!("list bookmarks: {e}"))?;
        Ok(bookmarks
            .into_iter()
            .map(|b| BookmarkInfo {
                token: b.token,
                url: b.url,
                name: b.node_name,
                is_dir: b.is_folder,
            })
            .collect())
    }

    /// Save a public link (optionally password-protected) as a bookmark.
    fn create_bookmark(&self, url: &str, password: Option<&str>) -> Result<(), String> {
        self.rt
            .block_on(self.client.create_bookmark(url, password))
            .map_err(|e| format!("create bookmark: {e}"))
    }

    /// Remove a saved bookmark by its token.
    fn delete_bookmark(&self, token: &str) -> Result<(), String> {
        self.rt
            .block_on(self.client.delete_bookmark(token))
            .map_err(|e| format!("delete bookmark: {e}"))
    }

    // ---- shared by me -----------------------------------------------------

    /// List the nodes I have shared with others, each with a summary of its share
    /// state (members, pending invitations, public link). One list call enumerates
    /// the shared uids; the per-node detail is then gathered best-effort — a single
    /// node racing with an unshare drops from the list rather than failing the whole
    /// request.
    fn list_shared_by_me(&self) -> Result<Vec<SharedItem>, String> {
        let uids = self
            .rt
            .block_on(self.client.enumerate_shared_by_me_node_uids())
            .map_err(|e| format!("enumerate shared-by-me: {e}"))?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let nodes = self
            .rt
            .block_on(self.client.enumerate_nodes(&uids))
            .map_err(|e| format!("enumerate nodes: {e}"))?;
        let mut items = Vec::with_capacity(nodes.len());
        for n in nodes {
            let uid = n.uid.clone();
            let members = self
                .rt
                .block_on(self.client.list_share_members(&uid))
                .map(|m| m.len())
                .unwrap_or(0);
            let proton_invites = self
                .rt
                .block_on(self.client.list_share_invitations(&uid))
                .map(|i| i.len())
                .unwrap_or(0);
            let external_invites = self
                .rt
                .block_on(self.client.list_external_invitations(&uid))
                .map(|i| i.len())
                .unwrap_or(0);
            let link = self
                .rt
                .block_on(self.client.get_public_link(&uid))
                .ok()
                .flatten()
                .map(public_link_info);
            items.push(SharedItem {
                uid: uid.to_string(),
                is_dir: n.is_folder(),
                name: n.name,
                path: self.rel_path_for_uid(&uid).unwrap_or_default(),
                member_count: members,
                invite_count: proton_invites + external_invites,
                link,
            });
        }
        Ok(items)
    }

    /// Best-effort mountpoint-relative path for a node already interned in the live
    /// tree, by walking parent inodes to the root. `None` when the node has never
    /// been seen through the mount (e.g. shared but not browsed to this session) —
    /// the caller then leaves the path empty.
    fn rel_path_for_uid(&self, uid: &NodeUid) -> Option<String> {
        let st = self.state.lock().unwrap();
        let mut ino = *st.by_uid.get(uid)?;
        let mut parts = Vec::new();
        while ino != ROOT_INO {
            let entry = st.entries.get(&ino)?;
            parts.push(entry.node.name.clone());
            ino = entry.parent;
        }
        parts.reverse();
        Some(parts.join("/"))
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
        detail: impl Into<String>,
        ok: bool,
    ) {
        let entry = ActivityEntry {
            time: now_secs(),
            kind,
            target: target.into(),
            detail: detail.into(),
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
        self.sync_progress.lock().unwrap().insert(
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
        if let Some(p) = self.sync_progress.lock().unwrap().get_mut(&folder_id) {
            f(p);
        }
    }

    /// Note that `n` more items have been queued for this pass, and that it has
    /// moved on from scanning to applying the diff.
    pub(crate) fn progress_queued(&self, folder_id: i64, n: usize) {
        self.progress_update(folder_id, |p| {
            p.phase = SyncPhase::Applying;
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
        self.sync_progress.lock().unwrap().remove(&folder_id);
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
fn role_from_str(role: &str) -> Result<MemberRole, String> {
    match role.to_lowercase().as_str() {
        "viewer" => Ok(MemberRole::Viewer),
        "editor" => Ok(MemberRole::Editor),
        "admin" => Ok(MemberRole::Admin),
        other => Err(format!("invalid role: {other}")),
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
pub struct ProtonFs {
    core: Core,
    uid: u32,
    gid: u32,
}

impl ProtonFs {
    /// Build the filesystem rooted at `root` (the user's My Files folder).
    fn new(core: Core, root: Node) -> Self {
        {
            let mut st = core.state.lock().unwrap();
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
        if let Err(e) = self
            .core
            .rt
            .block_on(self.core.client.trash_nodes(std::slice::from_ref(&uid)))
        {
            error!(%uid, error = %e, "trash failed");
            reply.error(Errno::EIO);
            return;
        }
        self.core.state.lock().unwrap().forget(&uid);
        self.core.cache.evict(&uid);
        self.core.invalidate_trash();
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
        if let Err(e) = self.core.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let name = name.to_string_lossy();
        let st = self.core.state.lock().unwrap();
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

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let st = self.core.state.lock().unwrap();
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
        mut reply: ReplyDirectory,
    ) {
        let ino = ino.0;
        if let Err(e) = self.core.ensure_children(ino) {
            reply.error(e);
            return;
        }
        let st = self.core.state.lock().unwrap();
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

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let (uid, base_mtime, base_size) = {
            let st = self.core.state.lock().unwrap();
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
        let mut st = self.core.state.lock().unwrap();
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
        let handle = {
            let st = self.core.state.lock().unwrap();
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
        let (uid, mtime, fsize) = {
            let st = self.core.state.lock().unwrap();
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
        match self
            .core
            .read_range(&uid, mtime, fsize, offset, size as u64)
        {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(e),
        }
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
            let st = self.core.state.lock().unwrap();
            match st.entries.get(&parent) {
                Some(e) => e.uid.clone(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
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
        let node = match self.core.fetch_node(&new_uid) {
            Ok(n) => n,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let (file, path) = match self.core.cache.create_scratch() {
            Ok(x) => x,
            Err(e) => {
                error!(%new_uid, error = %e, "create scratch file failed");
                reply.error(Errno::EIO);
                return;
            }
        };
        let mut st = self.core.state.lock().unwrap();
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
                base_mtime: now_secs(),
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
            let st = self.core.state.lock().unwrap();
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
            let mut st = self.core.state.lock().unwrap();
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
            let handled = match fh {
                Some(fh) => {
                    let mut st = self.core.state.lock().unwrap();
                    match st.handles.get_mut(&fh) {
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
                }
                None => false,
            };
            if !handled {
                // Path-based truncate with no open write handle: resize the
                // remote content and seal it now.
                let uid = {
                    let st = self.core.state.lock().unwrap();
                    match st.entries.get(&ino.0) {
                        Some(e) if e.node.is_file() => e.uid.clone(),
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
                let mut content = if size == 0 {
                    Vec::new()
                } else {
                    match self.core.rt.block_on(self.core.client.download_file(&uid)) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(%uid, error = %e, "truncate base download failed");
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                };
                content.resize(size as usize, 0);
                if let Err(e) = self
                    .core
                    .rt
                    .block_on(self.core.client.upload_new_revision(&uid, &content))
                {
                    error!(%uid, error = %e, "truncate upload failed");
                    reply.error(Errno::EIO);
                    return;
                }
                // Keep any pinned cache consistent with the new content.
                if self.core.cache.is_pinned(&uid) {
                    let _ = self.core.cache.store(&uid, now_secs(), size, &content);
                } else {
                    self.core.cache.evict(&uid);
                }
            }
            self.core.state.lock().unwrap().set_size(ino.0, size);
        }
        let st = self.core.state.lock().unwrap();
        match st.entries.get(&ino.0) {
            Some(e) => {
                let attr = self.attr(ino.0, &e.node);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.core.commit(fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.core.commit(fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
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
        let res = self.core.commit(fh.0);
        let handle = self.core.state.lock().unwrap().handles.remove(&fh.0);
        if let Some(h) = handle {
            let _ = std::fs::remove_file(&h.path);
        }
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
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
            let st = self.core.state.lock().unwrap();
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
        let node = match self.core.fetch_node(&new_uid) {
            Ok(n) => n,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let mut st = self.core.state.lock().unwrap();
        let ino = st.intern(parent, node);
        if let Some(kids) = st.children.get_mut(&parent)
            && !kids.contains(&ino)
        {
            kids.push(ino);
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
        let (_ino, uid) = match self.core.lookup_child(parent, &name) {
            Ok(x) => x,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        // Move first if the parent changed, then rename if the name changed.
        if newparent != parent {
            if let Err(e) = self.core.ensure_children(newparent) {
                reply.error(e);
                return;
            }
            let new_parent_uid = {
                let st = self.core.state.lock().unwrap();
                match st.entries.get(&newparent) {
                    Some(e) => e.uid.clone(),
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                }
            };
            if let Err(e) = self
                .core
                .rt
                .block_on(self.core.client.move_node(&uid, &new_parent_uid))
            {
                error!(%uid, error = %e, "move failed");
                reply.error(Errno::EIO);
                return;
            }
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
        // Forget the node so it re-interns under its new parent, and drop the
        // destination listing so it re-enumerates on next access.
        let mut st = self.core.state.lock().unwrap();
        st.forget(&uid);
        st.children.remove(&newparent);
        reply.ok();
    }

    /// Expose a file's server-side thumbnail/preview as an extended attribute, so
    /// a previewing client can fetch it without downloading the whole file. The
    /// bytes are fetched on demand and cached; absence yields `ENODATA`.
    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
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

    /// Advertise the thumbnail/preview attribute names for regular files. The
    /// names are listed unconditionally for files; a `getxattr` for one a given
    /// file lacks returns `ENODATA`.
    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let is_file = {
            let st = self.core.state.lock().unwrap();
            match st.entries.get(&ino.0) {
                Some(e) => e.node.is_file(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // xattr names are returned as a NUL-terminated, concatenated list.
        let mut buf = Vec::new();
        if is_file {
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
            let mut st = state.lock().unwrap();
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
            let mut st = state.lock().unwrap();
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
            let mut st = state.lock().unwrap();
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
    notifier: Notifier,
) {
    let mut cursor: Option<DriveEventId> = match db.get_event_cursor() {
        // Resume: pick up exactly where the last run left off.
        Ok(Some(saved)) => Some(DriveEventId::from(saved)),
        // First mount: a `None` cursor yields a single `CursorAdvanced` at the
        // server head; persist it so the next restart resumes instead of
        // reseeding (which would skip everything that changed offline).
        Ok(None) => match client.enumerate_events(&scope, None).await {
            Ok(events) => {
                let head = events.last().map(|e| e.id().clone());
                if let Some(c) = &head
                    && let Err(e) = db.set_event_cursor(c.as_str())
                {
                    warn!(error = %e, "persist seed cursor failed");
                }
                head
            }
            Err(e) => {
                error!(error = %e, "failed to seed event cursor; live sync disabled");
                return;
            }
        },
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
            apply_event(&state, &content, &notifier, event);
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

/// Turn a CLI-supplied path into a mountpoint-relative path. An absolute path
/// must live under `mountpoint`; a relative path is taken as already relative to
/// the mount root.
fn rel_to_mount(mountpoint: &Path, path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if p.is_absolute() {
        p.strip_prefix(mountpoint)
            .map(Path::to_path_buf)
            .map_err(|_| format!("{path} is not under the mountpoint"))
    } else {
        Ok(p.to_path_buf())
    }
}

/// Handle one control-socket connection: read a single JSON request line,
/// dispatch it against `core`, and write back a JSON response line.
fn handle_control_conn(core: &Core, username: &str, mountpoint: &Path, stream: UnixStream) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "control: clone stream failed");
            return;
        }
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let response = match serde_json::from_str::<CtlRequest>(line.trim()) {
        Ok(CtlRequest::Status) => {
            let pins = core.cache.list_pins();
            CtlResponse::Status {
                username: username.to_string(),
                mountpoint: mountpoint.display().to_string(),
                pinned: pins.len(),
                used: core.cache.usage(),
                budget: core.cache.budget(),
                pins,
            }
        }
        Ok(CtlRequest::Pin { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.pin(&rel) {
                Ok(name) => CtlResponse::Ok {
                    message: format!("pinned {name}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Unpin { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.unpin(&rel) {
                Ok(name) => CtlResponse::Ok {
                    message: format!("unpinned {name}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListPins) => CtlResponse::Pins {
            pins: core.cache.list_pins(),
        },
        Ok(CtlRequest::ListDir { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.list_dir(&rel) {
                Ok(entries) => CtlResponse::Entries { entries },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::PhotosTimeline { offset, limit }) => {
            match core.photos_timeline(offset, limit) {
                Ok(Some(items)) => CtlResponse::Photos {
                    available: true,
                    items,
                },
                Ok(None) => CtlResponse::Photos {
                    available: false,
                    items: Vec::new(),
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::PhotoThumbs { uids }) => {
            let parsed: Vec<NodeUid> = uids.iter().filter_map(|u| parse_uid(u)).collect();
            CtlResponse::Thumbs {
                items: core.photo_thumbs(&parsed),
            }
        }
        Ok(CtlRequest::OpenPhoto { uid }) => match parse_uid(&uid) {
            Some(u) => match core.open_photo(&u) {
                Ok(p) => CtlResponse::FilePath {
                    path: p.display().to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            None => CtlResponse::Error {
                message: format!("bad uid: {uid}"),
            },
        },
        Ok(CtlRequest::UploadPhoto {
            name,
            media_type,
            bytes,
            capture_time,
        }) => {
            let photos = core.photos();
            let metadata = proton_drive_rs::PhotoUploadMetadata {
                capture_time,
                ..Default::default()
            };
            let guard = core.transfers.begin(
                name.clone(),
                "",
                TransferDirection::Upload,
                bytes.len() as u64,
            );
            let reader = CountingReader::new(std::io::Cursor::new(&bytes), &guard);
            match core.rt.block_on(photos.upload_photo_from(
                &name,
                &media_type,
                reader,
                bytes.len() as i64,
                Vec::new(),
                metadata,
                false,
            )) {
                Ok(uid) => {
                    // The photo we just uploaded belongs at the head of the
                    // timeline, and the GUI reloads the gallery the moment this
                    // reply lands — so refresh now rather than leaving it to a
                    // background pass that would land just after that reload.
                    if let Err(e) = core.rt.block_on(core.refresh_timeline()) {
                        warn!(error = %e, "timeline refresh after upload failed");
                    }
                    CtlResponse::Ok {
                        message: format!("uploaded photo with uid {uid}"),
                    }
                }
                Err(e) => CtlResponse::Error {
                    message: format!("upload photo failed: {e}"),
                },
            }
        }
        Ok(CtlRequest::OpenFile { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.open_file(&rel) {
                Ok(p) => CtlResponse::FilePath {
                    path: p.display().to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Search { query, limit }) => match core.search(&query, limit) {
            Ok(hits) => CtlResponse::SearchResults { hits },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::SearchLocal { query, limit }) => match core.search_local(&query, limit) {
            Ok(hits) => CtlResponse::LocalResults {
                hits,
                indexing: core.indexing.load(Ordering::Relaxed),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Rename { path, new_name }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.rename(&rel, &new_name) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Rename, &name, format!("was {path}"), true);
                    CtlResponse::Ok {
                        message: format!("renamed to {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Rename, &path, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Move { path, new_parent }) => {
            match (
                rel_to_mount(mountpoint, &path),
                rel_to_mount(mountpoint, &new_parent),
            ) {
                (Ok(rel), Ok(parent_rel)) => match core.move_to(&rel, &parent_rel) {
                    Ok(name) => {
                        let dest = if new_parent.is_empty() {
                            "My files".to_string()
                        } else {
                            new_parent.clone()
                        };
                        core.log_activity(ActivityKind::Move, &name, format!("to {dest}"), true);
                        CtlResponse::Ok {
                            message: format!("moved {name}"),
                        }
                    }
                    Err(e) => {
                        core.log_activity(ActivityKind::Move, &path, &e, false);
                        CtlResponse::Error { message: e }
                    }
                },
                (Err(e), _) | (_, Err(e)) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::Delete { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.delete(&rel) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Trash, &name, "", true);
                    CtlResponse::Ok {
                        message: format!("trashed {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Trash, &path, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::CreateFolder { parent, name }) => match rel_to_mount(mountpoint, &parent) {
            Ok(parent_rel) => match core.create_folder(&parent_rel, &name) {
                Ok(name) => {
                    core.log_activity(ActivityKind::CreateFolder, &name, "", true);
                    CtlResponse::Ok {
                        message: format!("created folder {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::CreateFolder, &name, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::UploadFile {
            parent,
            name,
            bytes,
        }) => match rel_to_mount(mountpoint, &parent) {
            Ok(parent_rel) => match core.upload(&parent_rel, &name, &bytes) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Upload, &name, "", true);
                    CtlResponse::Ok {
                        message: format!("uploaded {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Upload, &name, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::UploadPaths { parent, sources }) => {
            match rel_to_mount(mountpoint, &parent) {
                // Ack immediately and upload on a background thread: a directory tree
                // far outlasts the socket read timeout. Progress and completion are
                // observed through GetQueueStatus; the activity log gets the summary
                // when the whole batch finishes.
                Ok(parent_rel) => {
                    let core = core.clone();
                    let paths: Vec<PathBuf> = sources.into_iter().map(PathBuf::from).collect();
                    let n = paths.len();
                    std::thread::spawn(move || {
                        let started = Instant::now();
                        match core.upload_paths(&parent_rel, &paths) {
                            Ok(stats) => {
                                // e.g. "Uploaded 700 files to Photos" — name the destination
                                // so the log reads like a sentence rather than a bare count.
                                let dest = parent_rel
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("your Drive");
                                let target = format!(
                                    "{} to {dest}",
                                    count_noun(stats.uploaded, "file", "files")
                                );
                                // Size · folders · elapsed, with a trailing failure count
                                // when some files didn't make it.
                                let mut parts = vec![human_bytes(stats.bytes)];
                                if stats.folders > 0 {
                                    parts.push(count_noun(stats.folders, "folder", "folders"));
                                }
                                parts.push(human_duration(started.elapsed()));
                                if stats.failed > 0 {
                                    parts.push(format!("{} failed", stats.failed));
                                }
                                core.log_activity(
                                    ActivityKind::Upload,
                                    target,
                                    parts.join(" · "),
                                    stats.failed == 0,
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "bulk upload failed");
                                core.log_activity(ActivityKind::Upload, "bulk upload", &e, false);
                            }
                        }
                    });
                    CtlResponse::Ok {
                        message: format!("uploading {n} item(s)"),
                    }
                }
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::ListTrash) => match core.list_trash() {
            Ok(entries) => CtlResponse::Entries { entries },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Restore { uids }) => match core.restore(&uids) {
            Ok(n) => {
                core.log_activity(ActivityKind::Restore, format!("{n} item(s)"), "", true);
                CtlResponse::Ok {
                    message: format!("restored {n} item(s)"),
                }
            }
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::DeleteForever { uids }) => match core.delete_forever(&uids) {
            Ok(n) => {
                core.log_activity(
                    ActivityKind::DeleteForever,
                    format!("{n} item(s)"),
                    "",
                    true,
                );
                CtlResponse::Ok {
                    message: format!("permanently deleted {n} item(s)"),
                }
            }
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::EmptyTrash) => match core.empty_trash() {
            Ok(n) => {
                core.log_activity(ActivityKind::EmptyTrash, format!("{n} item(s)"), "", true);
                CtlResponse::Ok {
                    message: format!("emptied trash ({n} item(s))"),
                }
            }
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::PurgeCache) => {
            let freed = core.cache.clear_unpinned();
            CtlResponse::Ok {
                message: format!(
                    "purged {:.1} MiB of unpinned cache",
                    freed as f64 / 1_048_576.0
                ),
            }
        }
        Ok(CtlRequest::GetQueueStatus) => CtlResponse::Transfers {
            items: core.transfers.snapshot(),
            jobs: core.jobs_snapshot(),
        },
        Ok(CtlRequest::SetCacheBudget { bytes }) => {
            core.cache.set_budget(bytes);
            // Persist so the next mount keeps the new cap. Best-effort: a config
            // write failure is reported but the live cap is already applied.
            match AppDirs::new().map(|dirs| {
                let mut cfg = dirs.load_config();
                cfg.cache_budget = Some(bytes);
                dirs.save_config(&cfg)
            }) {
                Ok(Ok(())) => CtlResponse::Ok {
                    message: format!("cache budget set to {bytes} bytes"),
                },
                Ok(Err(e)) => CtlResponse::Error {
                    message: format!("budget applied but config write failed: {e}"),
                },
                Err(e) => CtlResponse::Error {
                    message: format!("budget applied but config unavailable: {e}"),
                },
            }
        }
        Ok(CtlRequest::ListDevices) => match core.list_devices() {
            Ok(items) => CtlResponse::Devices { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RenameDevice { uid, name }) => match core.rename_device(&uid, &name) {
            Ok(()) => CtlResponse::Ok {
                message: format!("renamed device to {name}"),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::DeleteDevice { uid }) => match core.delete_device(&uid) {
            Ok(()) => CtlResponse::Ok {
                message: "device deleted".to_string(),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::AddSyncFolder { local_path }) => {
            // Registering the device and uploading a folder tree far outlasts the
            // socket read timeout, so ack immediately and work on a background
            // thread. The folder appears in ListSyncFolders once the row lands;
            // completion (and any failures) go to the activity log.
            let core = core.clone();
            let path = PathBuf::from(&local_path);
            std::thread::spawn(move || {
                let started = Instant::now();
                match core.add_sync_folder(&path) {
                    Ok(folder) => {
                        let name = Path::new(&folder.local_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(&folder.local_path)
                            .to_string();
                        core.log_activity(
                            ActivityKind::Upload,
                            format!("synced {name}"),
                            human_duration(started.elapsed()),
                            folder.state != "error",
                        );
                    }
                    Err(e) => {
                        warn!(error = %e, "add sync folder failed");
                        core.log_activity(ActivityKind::Upload, "add sync folder", &e, false);
                    }
                }
            });
            CtlResponse::Ok {
                message: format!("adding {local_path}"),
            }
        }
        Ok(CtlRequest::ListSyncFolders) => match core.list_sync_folders() {
            Ok(items) => CtlResponse::SyncFolders { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RemoveSyncFolder { id, delete_remote }) => {
            match core.remove_sync_folder(id, delete_remote) {
                Ok(()) => CtlResponse::Ok {
                    message: "removed synced folder".to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::SetSyncFolderMode { id, mode }) => {
            match core.set_sync_folder_mode(id, &mode) {
                Ok(message) => {
                    core.log_activity(ActivityKind::Upload, &message, "", true);
                    CtlResponse::Ok { message }
                }
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::SyncNow { id }) => {
            core.sync_now(id);
            CtlResponse::Ok {
                message: match id {
                    Some(id) => format!("reconciling folder {id}"),
                    None => "reconciling all folders".to_string(),
                },
            }
        }
        Ok(CtlRequest::ShareNode {
            path,
            emails,
            role,
            message,
        }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.share_node(&rel, &emails, &role, message.as_deref()) {
                Ok((proton, external)) => {
                    core.log_activity(
                        ActivityKind::Share,
                        &path,
                        format!("{} recipient(s) as {role}", proton + external),
                        true,
                    );
                    CtlResponse::Ok {
                        message: format!("invited {proton} Proton and {external} external user(s)"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Share, &path, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListShare { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.list_share(&rel) {
                Ok((entries, link)) => CtlResponse::Share { entries, link },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::UpdateShareRole {
            path,
            id,
            kind,
            role,
        }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.update_share_role(&rel, &id, kind, &role) {
                Ok(()) => CtlResponse::Ok {
                    message: format!("role updated to {role}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RemoveShareEntry { path, id, kind }) => {
            match rel_to_mount(mountpoint, &path) {
                Ok(rel) => match core.remove_share_entry(&rel, &id, kind) {
                    Ok(()) => {
                        core.log_activity(ActivityKind::Unshare, &path, "access removed", true);
                        CtlResponse::Ok {
                            message: "removed".to_string(),
                        }
                    }
                    Err(e) => CtlResponse::Error { message: e },
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::CreatePublicLink {
            path,
            role,
            password,
            expires,
        }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.create_public_link(&rel, &role, password.as_deref(), expires) {
                Ok(link) => {
                    core.log_activity(ActivityKind::PublicLink, &path, "link created", true);
                    CtlResponse::PublicLink { link }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::PublicLink, &path, &e, false);
                    CtlResponse::Error { message: e }
                }
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RemovePublicLink { path, id }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.remove_public_link(&rel, &id) {
                Ok(()) => {
                    core.log_activity(ActivityKind::Unshare, &path, "link removed", true);
                    CtlResponse::Ok {
                        message: "public link removed".to_string(),
                    }
                }
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListSharedWithMe) => match core.list_shared_with_me() {
            Ok(entries) => CtlResponse::Entries { entries },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::LeaveShared { uid }) => match core.leave_shared(&uid) {
            Ok(()) => {
                core.log_activity(ActivityKind::Unshare, "shared item", "left", true);
                CtlResponse::Ok {
                    message: "left shared item".to_string(),
                }
            }
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListInvitations) => match core.list_invitations() {
            Ok(items) => CtlResponse::Invitations { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::AcceptInvitation { id }) => match core.accept_invitation(&id) {
            Ok(()) => CtlResponse::Ok {
                message: "invitation accepted".to_string(),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::RejectInvitation { id }) => match core.reject_invitation(&id) {
            Ok(()) => CtlResponse::Ok {
                message: "invitation rejected".to_string(),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListBookmarks) => match core.list_bookmarks() {
            Ok(items) => CtlResponse::Bookmarks { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::CreateBookmark { url, password }) => {
            match core.create_bookmark(&url, password.as_deref()) {
                Ok(()) => CtlResponse::Ok {
                    message: "bookmark saved".to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::DeleteBookmark { token }) => match core.delete_bookmark(&token) {
            Ok(()) => CtlResponse::Ok {
                message: "bookmark removed".to_string(),
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListSharedByMe) => match core.list_shared_by_me() {
            Ok(items) => CtlResponse::SharedByMe { items },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::ListActivity { limit }) => CtlResponse::Activity {
            items: core.list_activity(limit),
        },
        Err(e) => CtlResponse::Error {
            message: format!("bad request: {e}"),
        },
    };
    let mut out = match serde_json::to_vec(&response) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "control: serialize response failed");
            return;
        }
    };
    out.push(b'\n');
    let mut stream = stream;
    let _ = stream.write_all(&out);
}

/// Listen on the control socket, serving one command per connection, each on its
/// own thread. Runs on its own thread; returns only if the listener itself fails.
///
/// Concurrent rather than serial because requests differ wildly in cost: an
/// `OpenPhoto` downloads a whole photo, and while it ran the accept loop used to
/// stall every other caller behind it — the GUI's 2s status poll, and the
/// thumbnail batches the gallery needs to paint. [`Core`] is a bundle of handles
/// (`Arc`/`Clone`), so each connection gets its own copy of it.
fn run_control_socket(core: Core, username: String, mountpoint: PathBuf, listener: UnixListener) {
    info!("control socket listening");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let core = core.clone();
                let username = username.clone();
                let mountpoint = mountpoint.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("pdfs-control".into())
                    .spawn(move || handle_control_conn(&core, &username, &mountpoint, stream))
                {
                    warn!(error = %e, "control: spawn handler failed");
                }
            }
            Err(e) => {
                warn!(error = %e, "control: accept failed");
            }
        }
    }
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

/// Mount the filesystem at `mountpoint` and block until it is unmounted or the
/// daemon is asked to stop.
///
/// Fetches the My Files root up front (so an auth/network failure surfaces
/// before the kernel mount), then spawns the Phase 2 event-sync task, the
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
    let root = rt
        .block_on(client.get_my_files_folder())
        .map_err(|e| std::io::Error::other(format!("fetch My Files root: {e}")))?;
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
        db,
        timeline_refreshing: Arc::new(AtomicBool::new(false)),
        trash_refreshing: Arc::new(AtomicBool::new(false)),
        thumb_gen: Arc::new(Mutex::new(HashSet::new())),
        transfers: TransferRegistry::new(),
        indexing: Arc::new(AtomicBool::new(false)),
        sync_progress: Arc::new(Mutex::new(HashMap::new())),
        sync_tx,
        mounts: Arc::new(Mutex::new(HashMap::new())),
        sync_locks: Arc::new(Mutex::new(HashMap::new())),
    };

    // Start the folder-sync engine. It watches every mirror folder, polls the
    // remotes, and reconciles on its own thread — never in front of a FUSE call.
    sync::spawn(core.clone(), sync_rx);

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
        client, scope, core.state, core.cache, core.db, notifier,
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
    let secondaries: Vec<_> = core.mounts.lock().unwrap().drain().collect();
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
mod tests {
    use super::Intervals;

    /// Flatten `segments` into a readable form for assertions.
    fn segs(iv: &Intervals, start: u64, end: u64) -> Vec<(u64, u64, bool)> {
        iv.segments(start, end)
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
