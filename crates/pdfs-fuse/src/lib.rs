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

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, Notifier, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    ReplyOpen, Request, Session,
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
            NodeKind::Folder => (FileType::Directory, 0o555),
            NodeKind::File { .. } => (FileType::RegularFile, 0o444),
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

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let st = self.state.lock().unwrap();
        match st.entries.get(&ino.0) {
            Some(e) if e.node.is_file() => reply.opened(FileHandle(0), FopenFlags::empty()),
            Some(_) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
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
        MountOption::RO,
        MountOption::NoExec,
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
