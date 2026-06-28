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

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, MountOption, Notifier, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
    Session, TimeOrNow, WriteFlags,
};
use proton_drive_rs::proton_sdk::ids::{DriveEventId, NodeUid};
use proton_drive_rs::{DriveEvent, DriveEventScopeId, Node, NodeKind, ProtonDriveClient};
use tracing::{debug, error, info, warn};

/// Attribute/entry cache lifetime handed back to the kernel. Long because the
/// Phase 2 event poller actively invalidates changed inodes; without a remote
/// change this is how long the kernel may serve stale metadata.
const TTL: Duration = Duration::from_secs(30);

/// How often the background task polls the remote event cursor.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// The FUSE root inode is always 1.
const ROOT_INO: u64 = 1;

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
        self.entries
            .insert(ino, Entry { uid: node.uid.clone(), parent, node });
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

/// The Proton Drive VFS. FUSE callbacks are synchronous, so the Tokio handle
/// bridges each one to the async SDK via [`Handle::block_on`]; the fuser
/// session thread is not a runtime worker, so blocking on it is sound.
pub struct ProtonFs {
    client: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    state: Arc<Mutex<State>>,
    uid: u32,
    gid: u32,
}

impl ProtonFs {
    /// Build the filesystem rooted at `root` (the user's My Files folder).
    pub fn new(client: ProtonDriveClient, rt: tokio::runtime::Handle, root: Node) -> Self {
        let mut entries = HashMap::new();
        let mut by_uid = HashMap::new();
        by_uid.insert(root.uid.clone(), ROOT_INO);
        entries.insert(
            ROOT_INO,
            Entry { uid: root.uid.clone(), parent: ROOT_INO, node: root },
        );
        // SAFETY: geteuid/getegid are infallible and have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            client,
            rt,
            state: Arc::new(Mutex::new(State {
                entries,
                by_uid,
                children: HashMap::new(),
                next_ino: 2,
                handles: HashMap::new(),
                next_fh: 1,
            })),
            uid,
            gid,
        }
    }

    /// A handle to the shared inode state, for the event poller.
    fn shared_state(&self) -> Arc<Mutex<State>> {
        Arc::clone(&self.state)
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut st = self.state.lock().unwrap();
        if let Some(h) = st.handles.get_mut(&fh) {
            h.dirty = false;
        }
        st.set_size(ino, buf.len() as u64);
        st.touch_mtime(ino, now);
        Ok(())
    }

    /// Trash the child `name` under `parent` on the remote, then drop it from the
    /// local cache. Backs both `unlink` and `rmdir` (Proton trashes whole
    /// subtrees, so an `rmdir` of a non-empty dir behaves the same).
    fn trash_child(&self, parent: u64, name: &str, reply: ReplyEmpty) {
        let (_ino, uid) = match self.lookup_child(parent, name) {
            Ok(x) => x,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Err(e) = self.rt.block_on(self.client.trash_nodes(std::slice::from_ref(&uid))) {
            error!(%uid, error = %e, "trash failed");
            reply.error(Errno::EIO);
            return;
        }
        self.state.lock().unwrap().forget(&uid);
        reply.ok();
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
        NodeKind::File { claimed_size, total_size_on_storage, .. } => {
            claimed_size.unwrap_or(*total_size_on_storage).max(0) as u64
        }
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
        if let Err(e) = self.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let name = name.to_string_lossy();
        let st = self.state.lock().unwrap();
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
        let st = self.state.lock().unwrap();
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
        if let Err(e) = self.ensure_children(ino) {
            reply.error(e);
            return;
        }
        let st = self.state.lock().unwrap();
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
            let st = self.state.lock().unwrap();
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
        let mut st = self.state.lock().unwrap();
        let fh = st.next_fh;
        st.next_fh += 1;
        st.handles.insert(
            fh,
            WriteHandle { ino: ino.0, uid, buf: Vec::new(), seeded: false, dirty: false },
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
        let uid = {
            let st = self.state.lock().unwrap();
            // If the file is open for writing and its buffer is populated, serve
            // from there so reads see the in-flight (possibly unsaved) content.
            if let Some(h) = st.handles.values().find(|h| h.ino == ino.0 && h.seeded) {
                let off = (offset as usize).min(h.buf.len());
                let end = (off + size as usize).min(h.buf.len());
                reply.data(&h.buf[off..end]);
                return;
            }
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
        match self
            .rt
            .block_on(self.client.download_range(&uid, offset, size as u64))
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
        if let Err(e) = self.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let name = name.to_string_lossy().into_owned();
        let parent_uid = {
            let st = self.state.lock().unwrap();
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
        let new_uid = match self.rt.block_on(self.client.upload_file(
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
        let node = match self.fetch_node(&new_uid) {
            Ok(n) => n,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let mut st = self.state.lock().unwrap();
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
            WriteHandle { ino, uid: new_uid, buf: Vec::new(), seeded: true, dirty: false },
        );
        let attr = self.attr(ino, &st.entries.get(&ino).unwrap().node);
        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
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
            let st = self.state.lock().unwrap();
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
            let base = match self.rt.block_on(self.client.download_file(&uid)) {
                Ok(b) => b,
                Err(e) => {
                    warn!(%uid, error = %e, "seed write buffer failed");
                    reply.error(Errno::EIO);
                    return;
                }
            };
            let mut st = self.state.lock().unwrap();
            if let Some(h) = st.handles.get_mut(&fh)
                && !h.seeded
            {
                h.buf = base;
                h.seeded = true;
            }
        }
        let off = offset as usize;
        let new_len = {
            let mut st = self.state.lock().unwrap();
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
        debug!(ino = ino.0, fh, offset, len = data.len(), new_len, "buffered write");
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
                    let mut st = self.state.lock().unwrap();
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
                    let st = self.state.lock().unwrap();
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
                    match self.rt.block_on(self.client.download_file(&uid)) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(%uid, error = %e, "truncate base download failed");
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                };
                content.resize(size as usize, 0);
                if let Err(e) = self.rt.block_on(self.client.upload_new_revision(&uid, &content)) {
                    error!(%uid, error = %e, "truncate upload failed");
                    reply.error(Errno::EIO);
                    return;
                }
            }
            self.state.lock().unwrap().set_size(ino.0, size);
        }
        let st = self.state.lock().unwrap();
        match st.entries.get(&ino.0) {
            Some(e) => {
                let attr = self.attr(ino.0, &e.node);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        match self.commit(fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        match self.commit(fh.0) {
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
        let res = self.commit(fh.0);
        self.state.lock().unwrap().handles.remove(&fh.0);
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
        if let Err(e) = self.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let name = name.to_string_lossy().into_owned();
        let parent_uid = {
            let st = self.state.lock().unwrap();
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
        let new_uid = match self.rt.block_on(self.client.create_folder(&parent_uid, &name, now)) {
            Ok(u) => u,
            Err(e) => {
                error!(%parent_uid, name, error = %e, "create folder failed");
                reply.error(Errno::EIO);
                return;
            }
        };
        let node = match self.fetch_node(&new_uid) {
            Ok(n) => n,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let mut st = self.state.lock().unwrap();
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
        let (_ino, uid) = match self.lookup_child(parent, &name) {
            Ok(x) => x,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        // Move first if the parent changed, then rename if the name changed.
        if newparent != parent {
            if let Err(e) = self.ensure_children(newparent) {
                reply.error(e);
                return;
            }
            let new_parent_uid = {
                let st = self.state.lock().unwrap();
                match st.entries.get(&newparent) {
                    Some(e) => e.uid.clone(),
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                }
            };
            if let Err(e) = self.rt.block_on(self.client.move_node(&uid, &new_parent_uid)) {
                error!(%uid, error = %e, "move failed");
                reply.error(Errno::EIO);
                return;
            }
        }
        if newname != name
            && let Err(e) = self.rt.block_on(self.client.rename_node(&uid, &newname, None))
        {
            error!(%uid, error = %e, "rename failed");
            reply.error(Errno::EIO);
            return;
        }
        // Forget the node so it re-interns under its new parent, and drop the
        // destination listing so it re-enumerates on next access.
        let mut st = self.state.lock().unwrap();
        st.forget(&uid);
        st.children.remove(&newparent);
        reply.ok();
    }
}

/// Apply one remote event to the local cache and notify the kernel so it drops
/// any stale cached metadata/data for the affected inodes.
///
/// The cache is authoritative-by-absence: dropping a directory's `children`
/// entry forces the next `lookup`/`readdir` to re-enumerate from the remote, so
/// most events only need to invalidate listings rather than re-fetch eagerly.
fn apply_event(state: &Mutex<State>, notifier: &Notifier, event: &DriveEvent) {
    match event {
        DriveEvent::NodeUpdated { node_uid, parent_node_uid, is_trashed, .. } => {
            let mut st = state.lock().unwrap();
            if *is_trashed {
                // Trashing makes a node vanish from its parent listing.
                let child = st.by_uid.get(node_uid).copied();
                if let Some((parent, name)) = st.forget(node_uid) {
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
                // it is a directory) so the next access re-fetches.
                st.children.remove(&ino);
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
            apply_event(&state, &notifier, event);
        }
        cursor = events.last().map(|e| e.id().clone());
    }
}

/// Mount the filesystem at `mountpoint` and block until it is unmounted.
///
/// Fetches the My Files root up front (so an auth/network failure surfaces
/// before the kernel mount), then spawns the Phase 2 event-sync task and takes
/// over the calling thread in the FUSE session loop. `rt` must be a handle to a
/// *running* multi-threaded runtime.
pub fn mount(
    client: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    mountpoint: &Path,
) -> std::io::Result<()> {
    let root = rt
        .block_on(client.get_my_files_folder())
        .map_err(|e| std::io::Error::other(format!("fetch My Files root: {e}")))?;
    let scope = root.tree_event_scope_id();

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("protondrive".to_string()),
        MountOption::Subtype("protondrive".to_string()),
        MountOption::DefaultPermissions,
    ];
    info!(mountpoint = %mountpoint.display(), "mounting Proton Drive");

    let fs = ProtonFs::new(client.clone(), rt.clone(), root);
    let shared = fs.shared_state();

    // Build the session explicitly (not `mount2`) so we can grab a `Notifier`
    // for the event task. `spawn` runs the session loop on its own thread;
    // `join` then blocks here until the filesystem is unmounted, preserving
    // `mount`'s blocking contract.
    let session = Session::new(fs, mountpoint, &config)?.spawn()?;
    let notifier = session.notifier();
    rt.spawn(run_event_sync(client, scope, shared, notifier));

    session.join()
}
