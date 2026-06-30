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

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::ReplyXattr;
use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, Notifier, OpenAccMode, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, Request, Session, TimeOrNow, WriteFlags,
};
use pdfs_core::cache::ContentCache;
use pdfs_core::control::{DirEntry, PhotoItem, Request as CtlRequest, Response as CtlResponse};
use proton_drive_rs::proton_sdk::ids::{DriveEventId, LinkId, NodeUid, VolumeId};
use proton_drive_rs::{
    DriveEvent, DriveEventScopeId, Node, NodeKind, PhotosTimelineItem, ProtonDriveClient,
    ProtonPhotosClient, ThumbnailType,
};
use tracing::{debug, error, info, warn};

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

/// Buffered state for a file opened for writing. The full plaintext is
/// assembled here and uploaded as one new revision on flush/release, because
/// the SDK seals whole revisions rather than byte ranges.
struct WriteHandle {
    ino: u64,
    uid: NodeUid,
    /// The plaintext being assembled; valid only once `seeded`.
    buf: Vec<u8>,
    /// Whether `buf` holds the file's current content (or was emptied by a
    /// truncate). Until then a partial write must hydrate the base first.
    seeded: bool,
    /// Whether `buf` diverged from the remote and needs an upload.
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
}

impl State {
    /// Allocate (or reuse) a stable inode for a node and store its metadata.
    fn intern(&mut self, parent: u64, node: Node) -> u64 {
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
    /// listing, and its slot in its parent's listing. Returns `(parent_ino,
    /// name)` when the node was known, so the caller can notify the kernel.
    fn forget(&mut self, uid: &NodeUid) -> Option<(u64, String)> {
        let ino = self.by_uid.remove(uid)?;
        let entry = self.entries.remove(&ino)?;
        self.children.remove(&ino);
        if let Some(kids) = self.children.get_mut(&entry.parent) {
            kids.retain(|&k| k != ino);
        }
        Some((entry.parent, entry.node.name))
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
    /// Cached full photos timeline (newest first), so paged `PhotosTimeline`
    /// requests slice memory instead of re-fetching the whole timeline per page.
    /// `None` until first fetched; refreshed once older than [`TIMELINE_TTL`].
    timeline: Arc<Mutex<Option<TimelineCache>>>,
}

/// The whole photos timeline plus when it was fetched, for TTL freshness.
struct TimelineCache {
    items: Vec<PhotosTimelineItem>,
    fetched: Instant,
}

impl Core {
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

    /// Upload a write handle's buffer as a new revision if it is dirty, clearing
    /// the dirty flag and refreshing the entry's size/mtime on success. No-op for
    /// a clean (or unknown) handle. Network I/O happens without the lock held.
    fn commit(&self, fh: u64) -> Result<(), Errno> {
        let (uid, buf, ino) = {
            let st = self.state.lock().unwrap();
            match st.handles.get(&fh) {
                Some(h) if h.dirty => (h.uid.clone(), h.buf.clone(), h.ino),
                _ => return Ok(()),
            }
        };
        self.rt
            .block_on(self.client.upload_new_revision(&uid, &buf))
            .map_err(|e| {
                error!(%uid, error = %e, "upload new revision failed");
                Errno::EIO
            })?;
        let now = now_secs();
        let mut st = self.state.lock().unwrap();
        if let Some(h) = st.handles.get_mut(&fh) {
            h.dirty = false;
        }
        st.set_size(ino, buf.len() as u64);
        st.touch_mtime(ino, now);
        drop(st);
        // The sealed content differs from any cached blob; if the file is
        // pinned, refresh the cache so reads stay served from disk.
        if self.cache.is_pinned(&uid) {
            let _ = self.cache.store(&uid, now, buf.len() as u64, &buf);
        } else {
            self.cache.evict(&uid);
        }
        Ok(())
    }

    /// Pin the file at mountpoint-relative `rel`: download its full plaintext
    /// into the content cache and record it in the pin registry. Returns the
    /// resolved node name on success.
    fn pin(&self, rel: &Path) -> Result<String, String> {
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
        let bytes = self
            .rt
            .block_on(self.client.download_file(&uid))
            .map_err(|e| format!("download: {e}"))?;
        self.cache
            .store(&uid, mtime, size, &bytes)
            .map_err(|e| format!("cache store: {e}"))?;
        self.cache
            .add_pin(&uid, rel)
            .map_err(|e| format!("pin: {e}"))?;
        Ok(name)
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

    /// Unpin the file at `rel`, evicting its cached content.
    fn unpin(&self, rel: &Path) -> Result<String, String> {
        let (ino, uid) = self
            .resolve_path(rel)
            .map_err(|e| format!("resolve path: {e:?}"))?;
        let name = {
            let st = self.state.lock().unwrap();
            st.entries
                .get(&ino)
                .map(|e| e.node.name.clone())
                .unwrap_or_default()
        };
        self.cache
            .remove_pin(&uid)
            .map_err(|e| format!("unpin: {e}"))?;
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
                uid: uid.to_string(),
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
            .rt
            .block_on(photos.download_photo(uid))
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
        let (mtime, size) = {
            let st = self.state.lock().unwrap();
            let e = st.entries.get(&ino).ok_or("node vanished")?;
            if !e.node.is_file() {
                return Err("not a regular file".into());
            }
            (e.node.modification_time, node_size(&e.node))
        };
        if let Some(p) = self.cache.cached_content_path(&uid, mtime, size) {
            return Ok(p);
        }
        let bytes = self
            .rt
            .block_on(self.client.download_file(&uid))
            .map_err(|e| format!("download: {e}"))?;
        self.cache
            .store(&uid, mtime, size, &bytes)
            .map_err(|e| format!("cache store: {e}"))?;
        Ok(self.cache.content_path(&uid))
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
        // Read-only opens stay stateless (fh 0). A write open allocates a buffer
        // handle, hydrated lazily on the first write that needs the base content.
        if flags.acc_mode() == OpenAccMode::O_RDONLY {
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }
        let mut st = self.core.state.lock().unwrap();
        let fh = st.next_fh;
        st.next_fh += 1;
        st.handles.insert(
            fh,
            WriteHandle {
                ino: ino.0,
                uid,
                buf: Vec::new(),
                seeded: false,
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
        let (uid, mtime, fsize) = {
            let st = self.core.state.lock().unwrap();
            // If the file is open for writing and its buffer is populated, serve
            // from there so reads see the in-flight (possibly unsaved) content.
            if let Some(h) = st.handles.values().find(|h| h.ino == ino.0 && h.seeded) {
                let off = (offset as usize).min(h.buf.len());
                let end = (off + size as usize).min(h.buf.len());
                reply.data(&h.buf[off..end]);
                return;
            }
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
        // Pinned (or otherwise cached) files are served straight from disk.
        if let Some(bytes) = self
            .core
            .cache
            .read_range(&uid, mtime, fsize, offset, size as u64)
        {
            reply.data(&bytes);
            return;
        }
        match self
            .core
            .rt
            .block_on(self.core.client.download_range(&uid, offset, size as u64))
        {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => {
                warn!(%uid, offset, size, error = %e, "download_range failed");
                reply.error(Errno::EIO);
            }
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
            WriteHandle {
                ino,
                uid: new_uid,
                buf: Vec::new(),
                seeded: true,
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
        // Partial writes need the file's current content; hydrate it once.
        let need_seed = {
            let st = self.core.state.lock().unwrap();
            match st.handles.get(&fh) {
                Some(h) => !h.seeded,
                None => {
                    reply.error(Errno::EBADF);
                    return;
                }
            }
        };
        if need_seed {
            let uid = self
                .core
                .state
                .lock()
                .unwrap()
                .handles
                .get(&fh)
                .map(|h| h.uid.clone());
            let Some(uid) = uid else {
                reply.error(Errno::EBADF);
                return;
            };
            let base = match self.core.rt.block_on(self.core.client.download_file(&uid)) {
                Ok(b) => b,
                Err(e) => {
                    warn!(%uid, error = %e, "seed write buffer failed");
                    reply.error(Errno::EIO);
                    return;
                }
            };
            let mut st = self.core.state.lock().unwrap();
            if let Some(h) = st.handles.get_mut(&fh)
                && !h.seeded
            {
                h.buf = base;
                h.seeded = true;
            }
        }
        let off = offset as usize;
        let new_len = {
            let mut st = self.core.state.lock().unwrap();
            let Some(h) = st.handles.get_mut(&fh) else {
                reply.error(Errno::EBADF);
                return;
            };
            let end = off + data.len();
            if h.buf.len() < end {
                h.buf.resize(end, 0);
            }
            h.buf[off..end].copy_from_slice(data);
            h.dirty = true;
            let len = h.buf.len() as u64;
            st.set_size(ino.0, len);
            len
        };
        debug!(
            ino = ino.0,
            fh,
            offset,
            len = data.len(),
            new_len,
            "buffered write"
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
                            h.buf.resize(size as usize, 0);
                            h.seeded = true;
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
        self.core.state.lock().unwrap().handles.remove(&fh.0);
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
                st.children.remove(&ino);
                content.evict(node_uid);
                let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
            }
            // A create (or move-in) shows up as a change to the parent listing;
            // drop it so the new child is picked up on the next readdir.
            if let Some(parent_uid) = parent_node_uid
                && let Some(&parent) = st.by_uid.get(parent_uid)
                && st.children.remove(&parent).is_some()
            {
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
            st.children.clear();
            for ino in dirs {
                let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
            }
        }
        // No substantive local change; the cursor advance is handled by the
        // caller persisting the event id.
        DriveEvent::CursorAdvanced { .. } | DriveEvent::SharedWithMeUpdated { .. } => {}
    }
}

/// Poll the remote event cursor forever, applying each batch to the shared
/// state. Seeds the cursor from the current server head so we only react to
/// changes made after mount. Runs as a Tokio task; returns only on fatal error.
async fn run_event_sync(
    client: ProtonDriveClient,
    scope: DriveEventScopeId,
    state: Arc<Mutex<State>>,
    content: Arc<ContentCache>,
    notifier: Notifier,
) {
    // Seed: a `None` cursor yields a single `CursorAdvanced` at the server head.
    let mut cursor: Option<DriveEventId> = match client.enumerate_events(&scope, None).await {
        Ok(events) => events.last().map(|e| e.id().clone()),
        Err(e) => {
            error!(error = %e, "failed to seed event cursor; live sync disabled");
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
    }
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
        Ok(CtlRequest::Status) => CtlResponse::Status {
            username: username.to_string(),
            mountpoint: mountpoint.display().to_string(),
            pinned: core.cache.list_pins().len(),
        },
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
        Ok(CtlRequest::OpenFile { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => match core.open_file(&rel) {
                Ok(p) => CtlResponse::FilePath {
                    path: p.display().to_string(),
                },
                Err(e) => CtlResponse::Error { message: e },
            },
            Err(e) => CtlResponse::Error { message: e },
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

/// Mount the filesystem at `mountpoint` and block until it is unmounted.
///
/// Fetches the My Files root up front (so an auth/network failure surfaces
/// before the kernel mount), then spawns the Phase 2 event-sync task, the
/// Phase 4 control socket, and takes over the calling thread in the FUSE session
/// loop. `rt` must be a handle to a *running* multi-threaded runtime.
pub fn mount(
    client: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    mountpoint: &Path,
    cache: ContentCache,
    control_socket: &Path,
    username: String,
) -> std::io::Result<()> {
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
        })),
        cache: Arc::new(cache),
        timeline: Arc::new(Mutex::new(None)),
    };

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("protondrive".to_string()),
        MountOption::Subtype("protondrive".to_string()),
        MountOption::DefaultPermissions,
    ];
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

    // Build the session explicitly (not `mount2`) so we can grab a `Notifier`
    // for the event task. `spawn` runs the session loop on its own thread;
    // `join` then blocks here until the filesystem is unmounted, preserving
    // `mount`'s blocking contract.
    let session = Session::new(fs, mountpoint, &config)?.spawn()?;
    let notifier = session.notifier();
    rt.spawn(run_event_sync(
        client, scope, core.state, core.cache, notifier,
    ));

    let result = session.join();
    let _ = std::fs::remove_file(control_socket);
    result
}
