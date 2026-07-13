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

use std::collections::HashMap;
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
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, Notifier, OpenAccMode, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, Request, Session, TimeOrNow, WriteFlags,
};
use pdfs_core::cache::{BLOCK_SIZE, ContentCache};
use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    DirEntry, LocalHit, PhotoItem, Request as CtlRequest, Response as CtlResponse, SearchHit,
    TransferDirection,
};
use pdfs_core::db::{Db, StoredNode};
use pdfs_core::localindex;
use proton_drive_rs::proton_sdk::error::ProtonError;
use proton_drive_rs::proton_sdk::ids::{DriveEventId, LinkId, NodeUid, VolumeId};
use proton_drive_rs::{
    DriveEvent, DriveEventScopeId, Node, NodeKind, PhotosTimelineItem, ProtonDriveClient,
    ProtonPhotosClient, ThumbnailType,
};

mod transfers;
use tracing::{debug, error, info, warn};
use transfers::{CountingReader, CountingWriter, TransferRegistry};

/// Attribute/entry cache lifetime handed back to the kernel. Long because the
/// Phase 2 event poller actively invalidates changed inodes; without a remote
/// change this is how long the kernel may serve stale metadata.
const TTL: Duration = Duration::from_secs(30);

/// How often the background task polls the remote event cursor.
const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// How long a fetched photos timeline stays good before the next page request
/// re-fetches it. The SDK hands back the whole timeline at once, so we cache it
/// in [`Core`] and serve every page from memory until it goes stale.
const TIMELINE_TTL: Duration = Duration::from_secs(60);

/// How stale the local-file index may get before the background scanner rebuilds
/// it. A rescan is a full walk of `$HOME`, so this trades index freshness against
/// disk churn; the scanner also always rebuilds once per daemon start when the
/// index is older than this.
const LOCAL_INDEX_TTL: Duration = Duration::from_secs(15 * 60);

/// How often the scanner thread wakes to check whether the index went stale.
const LOCAL_INDEX_CHECK: Duration = Duration::from_secs(60);

/// The FUSE root inode is always 1.
const ROOT_INO: u64 = 1;

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
    /// Cached full photos timeline (newest first), so paged `PhotosTimeline`
    /// requests slice memory instead of re-fetching the whole timeline per page.
    /// `None` until first fetched; refreshed once older than [`TIMELINE_TTL`].
    timeline: Arc<Mutex<Option<TimelineCache>>>,
    /// In-flight upload/download progress, served to `GetQueueStatus`. Shared
    /// across the FUSE session and the control-socket task.
    transfers: Arc<TransferRegistry>,
    /// True while the background scanner is rebuilding the local-file index, so
    /// `SearchLocal` can tell a front-end "still indexing" apart from "no match".
    indexing: Arc<AtomicBool>,
}

/// The whole photos timeline plus when it was fetched, for TTL freshness.
struct TimelineCache {
    items: Vec<PhotosTimelineItem>,
    fetched: Instant,
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
            let parent = node
                .parent_uid
                .as_ref()
                .and_then(|p| st.by_uid.get(p).copied())
                .unwrap_or(ROOT_INO);
            let uid = node.uid.clone();
            st.entries.insert(ino, Entry { uid, parent, node });
        }

        // Pass 3: rebuild child listings for fully-enumerated folders.
        for dir_ino in listed_dirs {
            let kids: Vec<u64> = st
                .entries
                .iter()
                .filter(|(_, e)| e.parent == dir_ino && !e.node.trashed)
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
                    if node.trashed {
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
            if node.trashed {
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

    /// A page of the photos timeline (newest first), with the page's thumbnails
    /// fetched into the cache and their paths attached. `Ok(None)` when the
    /// account has no photos volume.
    ///
    /// The SDK's `enumerate_timeline` returns the *whole* timeline, so the first
    /// request (or the first after [`TIMELINE_TTL`] elapses) fetches and caches
    /// it in [`Core::timeline`]; every page then slices that cached vec, so
    /// "load more" scrolling doesn't re-hit the network per page.
    fn photos_timeline(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Option<Vec<PhotoItem>>, String> {
        let page: Vec<PhotosTimelineItem> = {
            // Refresh the cached timeline if absent or stale. The lock is only
            // held for the freshness check and the store, never across the fetch.
            let stale = {
                let guard = self.timeline.lock().unwrap();
                guard
                    .as_ref()
                    .is_none_or(|c| c.fetched.elapsed() >= TIMELINE_TTL)
            };
            if stale {
                let photos = self.photos();
                if self
                    .rt
                    .block_on(photos.get_photos_root())
                    .map_err(|e| format!("photos root: {e}"))?
                    .is_none()
                {
                    return Ok(None);
                }
                let items = self
                    .rt
                    .block_on(photos.enumerate_timeline())
                    .map_err(|e| format!("timeline: {e}"))?;
                *self.timeline.lock().unwrap() = Some(TimelineCache {
                    items,
                    fetched: Instant::now(),
                });
            }
            let guard = self.timeline.lock().unwrap();
            match guard.as_ref() {
                Some(c) => c.items.iter().skip(offset).take(limit).cloned().collect(),
                None => return Ok(None),
            }
        };
        let ttype = ThumbnailType::Thumbnail.as_i32();

        // Batch-fetch only the thumbnails not already cached fresh.
        let want: Vec<NodeUid> = page
            .iter()
            .filter(|it| {
                self.cache
                    .cached_thumbnail_path(&it.uid, ttype, it.capture_time)
                    .is_none()
            })
            .map(|it| it.uid.clone())
            .collect();
        if !want.is_empty() {
            let cap: HashMap<NodeUid, i64> = page
                .iter()
                .map(|it| (it.uid.clone(), it.capture_time))
                .collect();
            match self.rt.block_on(
                self.photos()
                    .enumerate_thumbnails(&want, ThumbnailType::Thumbnail),
            ) {
                Ok(thumbs) => {
                    for ft in thumbs {
                        if let (Ok(bytes), Some(&tag)) = (ft.result, cap.get(&ft.file_uid)) {
                            let _ = self.cache.store_thumbnail(&ft.file_uid, ttype, tag, &bytes);
                        }
                    }
                }
                Err(e) => warn!(error = %e, "batch photo thumbnails failed"),
            }
        }

        Ok(Some(
            page.into_iter()
                .map(|it| {
                    let thumb_path = self
                        .cache
                        .cached_thumbnail_path(&it.uid, ttype, it.capture_time)
                        .map(|p| p.display().to_string());
                    PhotoItem {
                        uid: it.uid.to_string(),
                        capture_time: it.capture_time,
                        thumb_path,
                    }
                })
                .collect(),
        ))
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
        Ok(name)
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
fn run_local_index(db: Arc<Db>, indexing: Arc<AtomicBool>, mountpoint: PathBuf) {
    loop {
        let age = db.local_indexed_at().ok().flatten();
        let stale =
            age.is_none_or(|at| now_secs().saturating_sub(at) >= LOCAL_INDEX_TTL.as_secs() as i64);
        if stale {
            scan_local_once(&db, &indexing, &mountpoint);
        }
        std::thread::sleep(LOCAL_INDEX_CHECK);
    }
}

/// Walk `$HOME` once and replace the local-file index with what it finds.
/// Batches stream straight into SQLite, so peak memory is one batch — not the
/// whole home directory.
fn scan_local_once(db: &Db, indexing: &AtomicBool, mountpoint: &Path) {
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

    let walked = localindex::scan(&[home], &excludes, |batch| {
        if let Err(e) = db.local_upsert_batch(generation, &batch) {
            warn!(error = %e, "local index: batch write failed");
        }
    });

    // Prune what this scan did not see and rebuild the FTS index over the rest,
    // even if some batches failed — a partial index still beats none.
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
                    *core.timeline.lock().unwrap() = None;
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
                Ok(name) => CtlResponse::Ok {
                    message: format!("renamed to {name}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::Move { path, new_parent }) => {
            match (
                rel_to_mount(mountpoint, &path),
                rel_to_mount(mountpoint, &new_parent),
            ) {
                (Ok(rel), Ok(parent_rel)) => match core.move_to(&rel, &parent_rel) {
                    Ok(name) => CtlResponse::Ok {
                        message: format!("moved {name}"),
                    },
                    Err(e) => CtlResponse::Error { message: e },
                },
                (Err(e), _) | (_, Err(e)) => CtlResponse::Error { message: e },
            }
        }
        Ok(CtlRequest::Delete { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.delete(&rel) {
                Ok(name) => CtlResponse::Ok {
                    message: format!("trashed {name}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::CreateFolder { parent, name }) => match rel_to_mount(mountpoint, &parent) {
            Ok(parent_rel) => match core.create_folder(&parent_rel, &name) {
                Ok(name) => CtlResponse::Ok {
                    message: format!("created folder {name}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
        },
        Ok(CtlRequest::UploadFile {
            parent,
            name,
            bytes,
        }) => match rel_to_mount(mountpoint, &parent) {
            Ok(parent_rel) => match core.upload(&parent_rel, &name, &bytes) {
                Ok(name) => CtlResponse::Ok {
                    message: format!("uploaded {name}"),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
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

/// Listen on the control socket, serving one CLI command per connection. Runs on
/// its own thread; returns only if the listener itself fails.
fn run_control_socket(core: Core, username: String, mountpoint: PathBuf, listener: UnixListener) {
    info!("control socket listening");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => handle_control_conn(&core, &username, &mountpoint, stream),
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
        timeline: Arc::new(Mutex::new(None)),
        transfers: TransferRegistry::new(),
        indexing: Arc::new(AtomicBool::new(false)),
    };

    // Keep the launcher prompt's "This computer" index fresh. Its own thread:
    // the walk is I/O-heavy and must never sit in front of a FUSE callback.
    {
        let db = core.db.clone();
        let indexing = core.indexing.clone();
        let mountpoint = mountpoint.to_path_buf();
        std::thread::Builder::new()
            .name("pdfs-localindex".into())
            .spawn(move || run_local_index(db, indexing, mountpoint))?;
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

    let _ = std::fs::remove_file(control_socket);
    Ok(outcome)
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
