//! A second adapter over the mounted-files service, with no kernel: files and
//! folders addressed by path, served by [`Files`] and [`Namespace`].
//!
//! It exists to show what the service needs from an adapter, and that nothing
//! of FUSE is left in it. The Linux FUSE adapter in `pdfs-fuse` answers the
//! kernel with what the services return; this one answers a caller with
//! [`Failure`]. The conformance scenarios (`conformance`, with the
//! `test-support` feature) run through both and expect the same outcomes.
//!
//! [`Headless`] has no Drive. Its host queues every create in the real store
//! as the daemon does offline, and nothing drains the queue. The bytes a
//! closed file committed are kept in memory, as the content of its next
//! revision would be.
//!
//! What the kernel does before it calls the FUSE adapter, this adapter does
//! itself: it resolves a path one name at a time, refuses to make a name that
//! is taken, and opens an existing file when asked to create it. The services
//! leave those checks to the adapter.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use pdfs_core::lockrank::{Rank, Ranked};
use pdfs_store::cache::ContentCache;
use pdfs_store::db::{Db, LOCAL_VOLUME, OP_CREATE, OP_MKDIR, PARK_UNTIL, PendingOp};
use proton_drive_rs::Node;
use proton_drive_rs::proton_sdk::ids::{LinkId, NodeUid, VolumeId};

use crate::files::{FileError, Files, FilesHost};
use crate::ids::is_local_uid;
use crate::namespace::{Namespace, NamespaceError, NamespaceHost, RenameFlags};
use crate::nodes::local_node;
use crate::open::{Intervals, ReadFailure, WriteHandle, assemble_open_read};
use crate::tree::{NodeError, State, StateGuard};
use crate::writes::WriteState;

/// Why the headless adapter refused or failed an operation. The FUSE adapter
/// answers each with the errno named here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// `ENOENT`: no such file or folder.
    NotFound,
    /// `EEXIST`: the name is taken.
    Exists,
    /// `EISDIR`: a folder where a file was needed.
    IsDirectory,
    /// `ENOTDIR`: a file where a folder was needed.
    NotDirectory,
    /// `ENOTEMPTY`: the folder has children.
    NotEmpty,
    /// `EINVAL`: an operation that cannot be done, such as moving a folder
    /// into itself.
    InvalidArgument,
    /// `EACCES`: the node may not be changed.
    Denied,
    /// `EBUSY`: try again.
    Busy,
    /// `EBADF`: no file is open under this handle.
    BadHandle,
    /// `ENOSPC`: the local disk is full.
    NoSpace,
    /// `EOPNOTSUPP`: the operation is not supported.
    Unsupported,
    /// `EIO`: something failed underneath.
    Io,
}

impl Failure {
    /// The failure an OS error stands for, as the FUSE adapter answers it.
    pub fn from_errno(errno: i32) -> Self {
        match errno {
            libc::ENOENT => Failure::NotFound,
            libc::EEXIST => Failure::Exists,
            libc::EISDIR => Failure::IsDirectory,
            libc::ENOTDIR => Failure::NotDirectory,
            libc::ENOTEMPTY => Failure::NotEmpty,
            libc::EINVAL => Failure::InvalidArgument,
            libc::EACCES | libc::EPERM => Failure::Denied,
            libc::EBUSY => Failure::Busy,
            libc::EBADF => Failure::BadHandle,
            libc::ENOSPC | libc::EDQUOT => Failure::NoSpace,
            libc::EOPNOTSUPP => Failure::Unsupported,
            _ => Failure::Io,
        }
    }
}

/// What the headless host fails with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    NotFound,
    Denied,
    Io,
}

impl From<HostError> for Failure {
    fn from(error: HostError) -> Self {
        match error {
            HostError::NotFound => Failure::NotFound,
            HostError::Denied => Failure::Denied,
            HostError::Io => Failure::Io,
        }
    }
}

impl From<FileError<HostError>> for Failure {
    fn from(error: FileError<HostError>) -> Self {
        match error {
            FileError::NotFound => Failure::NotFound,
            FileError::IsDirectory => Failure::IsDirectory,
            FileError::Denied => Failure::Denied,
            FileError::BadHandle => Failure::BadHandle,
            FileError::NoSpace | FileError::QuotaExceeded => Failure::NoSpace,
            FileError::Unsupported => Failure::Unsupported,
            FileError::Io | FileError::Storage(_) => Failure::Io,
            FileError::Host(error) => error.into(),
        }
    }
}

impl From<NamespaceError<HostError>> for Failure {
    fn from(error: NamespaceError<HostError>) -> Self {
        match error {
            NamespaceError::NotFound => Failure::NotFound,
            NamespaceError::IsDirectory => Failure::IsDirectory,
            NamespaceError::NotDirectory => Failure::NotDirectory,
            NamespaceError::NotEmpty => Failure::NotEmpty,
            NamespaceError::InvalidArgument => Failure::InvalidArgument,
            NamespaceError::Exists => Failure::Exists,
            NamespaceError::Busy => Failure::Busy,
            NamespaceError::Io => Failure::Io,
            NamespaceError::Host(error) => error.into(),
        }
    }
}

fn node_failure(error: NodeError) -> HostError {
    match error {
        NodeError::NotFound => HostError::NotFound,
        NodeError::NotWritable => HostError::Denied,
    }
}

/// A file open for writing through the headless adapter.
#[derive(Debug)]
pub struct Fd {
    ino: u64,
    fh: u64,
}

struct Inner {
    state: Arc<Ranked<State>>,
    db: Arc<Db>,
    cache: Arc<ContentCache>,
    /// What each file's last close committed: the content its next revision
    /// would upload.
    content: Mutex<HashMap<NodeUid, Vec<u8>>>,
}

/// The host of the headless adapter: offline, with the real store.
#[derive(Clone)]
struct LocalHost(Arc<Inner>);

impl LocalHost {
    fn state(&self) -> StateGuard<'_> {
        StateGuard::new(self.0.state.lock(), &self.0.db)
    }

    fn base(&self, uid: &NodeUid) -> Vec<u8> {
        self.0.content.lock().get(uid).cloned().unwrap_or_default()
    }

    fn drop_ops(&self, uid: &NodeUid) -> Result<(), HostError> {
        self.0
            .db
            .delete_ops_for_uid(&uid.to_string())
            .map(drop)
            .map_err(|_| HostError::Io)
    }
}

/// The bytes `[0, len)` of a file open for writing: what it wrote from
/// `file`, the rest from `base`.
fn assemble(file: &File, len: u64, written: &Intervals, base: &[u8]) -> Result<Vec<u8>, HostError> {
    let base_len = base.len() as u64;
    let read = assemble_open_read::<()>(file, len, base_len, written, 0, len, |offset, n| {
        Ok(Some(base[offset as usize..(offset + n) as usize].to_vec()))
    });
    match read {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) | Err(ReadFailure::Base(())) | Err(ReadFailure::Scratch(_)) => Err(HostError::Io),
    }
}

impl FilesHost for LocalHost {
    type Error = HostError;

    fn settle_size(&self, _ino: u64, _uid: &NodeUid) -> Option<u64> {
        None
    }

    fn fill_gaps(
        &self,
        uid: &NodeUid,
        file: &File,
        len: u64,
        _base_mtime: i64,
        _base_size: u64,
        written: &Intervals,
    ) -> Result<(), HostError> {
        use std::os::unix::fs::FileExt;
        let base = self.base(uid);
        file.set_len(len).map_err(|_| HostError::Io)?;
        for (start, end, authored) in written.segments(0, len) {
            let end = end.min(base.len() as u64);
            if !authored && start < end {
                file.write_all_at(&base[start as usize..end as usize], start)
                    .map_err(|_| HostError::Io)?;
            }
        }
        Ok(())
    }

    fn landed(&self, _uid: &NodeUid) -> Option<Node> {
        None
    }

    fn commit(&self, handle: &mut WriteHandle) -> Result<(), HostError> {
        let committed = if handle.dirty {
            let base = self.base(&handle.uid);
            Some(assemble(&handle.file, handle.len, &handle.written, &base)?)
        } else {
            None
        };
        self.0.cache.clear_scratch_durable(&handle.path);
        let _ = std::fs::remove_file(&handle.path);
        if let Some(bytes) = committed {
            self.0.content.lock().insert(handle.uid.clone(), bytes);
        }
        Ok(())
    }

    fn discard_unlinked(&self, uid: &NodeUid) {
        self.0.content.lock().remove(uid);
        let _ = self.drop_ops(uid);
    }
}

impl NamespaceHost for LocalHost {
    type Error = HostError;

    fn require_writable(&self, ino: u64) -> Result<(), HostError> {
        self.0
            .state
            .lock()
            .require_writable(ino)
            .map_err(node_failure)
    }

    fn require_uid_writable(&self, uid: &NodeUid) -> Result<(), HostError> {
        let st = self.0.state.lock();
        match st.by_uid.get(uid) {
            Some(&ino) => st.require_writable(ino).map_err(node_failure),
            None => Err(HostError::Denied),
        }
    }

    fn ensure_children(&self, _ino: u64) -> Result<(), HostError> {
        // Every folder is made here and listed from the start.
        Ok(())
    }

    fn lookup_child(&self, parent: u64, name: &str) -> Result<Option<(u64, NodeUid)>, HostError> {
        let st = self.0.state.lock();
        Ok(st
            .children
            .get(&parent)
            .into_iter()
            .flatten()
            .filter_map(|ino| st.entries.get(ino).map(|entry| (*ino, entry)))
            .find(|(_, entry)| entry.node.name == name)
            .map(|(ino, entry)| (ino, entry.uid.clone())))
    }

    fn sends_inline(&self) -> bool {
        false
    }

    fn mint(
        &self,
        _parent: &NodeUid,
        _name: &str,
        _folder: bool,
    ) -> Result<Option<Node>, HostError> {
        Ok(None)
    }

    fn queue_create(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        folder: bool,
        parked: bool,
    ) -> Result<Node, HostError> {
        let now = now_secs();
        let lid = self
            .0
            .db
            .add_local_row(&parent_uid.to_string(), name, folder, now)
            .map_err(|_| HostError::Io)?;
        let uid = NodeUid::new(VolumeId::from(LOCAL_VOLUME), LinkId::from(lid.to_string()));
        let op = PendingOp {
            id: 0,
            kind: if folder { OP_MKDIR } else { OP_CREATE }.to_string(),
            uid: uid.to_string(),
            parent_uid: Some(parent_uid.to_string()),
            name: Some(name.to_owned()),
            blob_path: None,
            meta_json: None,
            created_at: now * 1000,
            attempts: 0,
            last_error: None,
            next_attempt_at: if parked { PARK_UNTIL } else { 0 },
        };
        self.0.db.enqueue_op(&op).map_err(|_| HostError::Io)?;
        Ok(local_node(uid, parent_uid.clone(), name.to_owned(), folder))
    }

    fn access_changed(&self) {}

    fn drop_queued(&self, uid: &NodeUid, _keep_staged: bool) -> Result<(), HostError> {
        self.drop_ops(uid)
    }

    fn landed_uid(&self, _ino: u64) -> Option<NodeUid> {
        None
    }

    fn has_pending_moves_out_of(&self, _uid: &NodeUid) -> Result<bool, HostError> {
        Ok(false)
    }

    fn queue_trash(&self, uid: &NodeUid, _name: &str, open: bool) -> Result<(), HostError> {
        // Only the root is not made here, and it cannot be removed.
        self.state().forget_or_unlink(uid);
        if !open {
            self.0.content.lock().remove(uid);
        }
        Ok(())
    }

    fn trash_remote(&self, _uid: &NodeUid, _name: &str) -> Result<bool, HostError> {
        Ok(false)
    }

    fn remove_replaced(&self, _ino: u64, uid: &NodeUid, _name: &str) -> Result<(), HostError> {
        let open = {
            let st = self.0.state.lock();
            st.by_uid
                .get(uid)
                .and_then(|ino| st.entries.get(ino))
                .is_some_and(|entry| entry.open_count > 0)
        };
        if is_local_uid(uid) {
            self.drop_ops(uid)?;
        }
        self.state().forget_or_unlink(uid);
        if !open {
            self.0.content.lock().remove(uid);
        }
        Ok(())
    }

    fn restore_replaced(&self, _victim: Option<&(u64, NodeUid)>, _parent: &NodeUid, _name: &str) {}

    fn queue_rename(
        &self,
        ino: u64,
        _uid: &NodeUid,
        _old_parent_uid: &NodeUid,
        new_parent_ino: u64,
        new_parent_uid: &NodeUid,
        new_name: &str,
        _authorized: bool,
    ) -> Result<(), HostError> {
        self.state()
            .rename_in_place(ino, new_parent_ino, new_parent_uid, new_name);
        Ok(())
    }

    fn rename_remote(
        &self,
        _uid: &NodeUid,
        _new_parent: Option<&NodeUid>,
        _new_name: Option<&str>,
    ) -> Result<bool, HostError> {
        Ok(false)
    }

    fn wake_drain(&self) {}

    fn trashed(&self, _uid: &NodeUid, _name: &str, _open: bool) {}
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The mounted-files service of one root, served by path.
pub struct Headless {
    host: LocalHost,
    files: Files<LocalHost>,
    namespace: Namespace<LocalHost>,
    root: u64,
}

impl Headless {
    /// A new, empty root, with its store and cache in `dir`.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        let other = |e: &dyn std::fmt::Display| std::io::Error::other(e.to_string());
        std::fs::create_dir_all(dir)?;
        let db = Arc::new(Db::open(&dir.join("cache.db")).map_err(|e| other(&e))?);
        let cache = Arc::new(
            ContentCache::open(dir.join("content"), dir.join("pins"), 0, db.clone())
                .map_err(|e| other(&e))?,
        );
        let state = Arc::new(Ranked::new(
            Rank::State,
            State::new(db.clone(), HashMap::new()),
        ));
        let root_uid = NodeUid::new(VolumeId::from("headless"), LinkId::from("root"));
        let root = {
            let mut st = StateGuard::new(state.lock(), &db);
            let ino = st.install_root(local_node(root_uid.clone(), root_uid, String::new(), true));
            st.children.insert(ino, Vec::new());
            ino
        };
        let host = LocalHost(Arc::new(Inner {
            state: state.clone(),
            db: db.clone(),
            cache: cache.clone(),
            content: Mutex::default(),
        }));
        let writes = WriteState::default();
        Ok(Headless {
            files: Files::new(
                state.clone(),
                db.clone(),
                cache.clone(),
                writes.clone(),
                host.clone(),
            ),
            namespace: Namespace::new(state, db, cache, writes, host.clone()),
            host,
            root,
        })
    }

    /// The inode at `path`, resolved one name at a time from the root.
    fn resolve(&self, path: &str) -> Result<u64, Failure> {
        let mut ino = self.root;
        for name in components(path) {
            if !self.is_folder(ino)? {
                return Err(Failure::NotDirectory);
            }
            ino = self
                .host
                .lookup_child(ino, name)?
                .ok_or(Failure::NotFound)?
                .0;
        }
        Ok(ino)
    }

    /// The folder `path` names a child of, and the child's name.
    fn split<'p>(&self, path: &'p str) -> Result<(u64, &'p str), Failure> {
        let mut names: Vec<&str> = components(path).collect();
        let name = names.pop().ok_or(Failure::InvalidArgument)?;
        let parent = self.resolve(&names.join("/"))?;
        if !self.is_folder(parent)? {
            return Err(Failure::NotDirectory);
        }
        Ok((parent, name))
    }

    fn is_folder(&self, ino: u64) -> Result<bool, Failure> {
        self.host
            .0
            .state
            .lock()
            .entries
            .get(&ino)
            .map(|entry| entry.node.is_folder())
            .ok_or(Failure::NotFound)
    }

    fn child(&self, parent: u64, name: &str) -> Result<Option<u64>, Failure> {
        Ok(self.host.lookup_child(parent, name)?.map(|(ino, _)| ino))
    }

    /// Create the file at `path`, or empty it when it exists, and open it for
    /// writing, as `open(O_CREAT | O_TRUNC)` does.
    pub fn create(&self, path: &str) -> Result<Fd, Failure> {
        let (parent, name) = self.split(path)?;
        if let Some(ino) = self.child(parent, name)? {
            let fd = self.open_ino(ino)?;
            self.set_len(&fd, 0)?;
            return Ok(fd);
        }
        let made = self.namespace.create(parent, name)?;
        Ok(Fd {
            ino: made.ino,
            fh: made.fh,
        })
    }

    /// Open the file at `path` for writing.
    pub fn open_write(&self, path: &str) -> Result<Fd, Failure> {
        let ino = self.resolve(path)?;
        self.open_ino(ino)
    }

    fn open_ino(&self, ino: u64) -> Result<Fd, Failure> {
        let fh = self.files.open(ino, true)?;
        Ok(Fd { ino, fh })
    }

    pub fn write(&self, fd: &Fd, offset: u64, data: &[u8]) -> Result<(), Failure> {
        Ok(self.files.write(fd.ino, fd.fh, offset, data)?)
    }

    pub fn set_len(&self, fd: &Fd, size: u64) -> Result<(), Failure> {
        match self.files.resize(fd.ino, size)? {
            true => Ok(()),
            false => Err(Failure::BadHandle),
        }
    }

    pub fn fsync(&self, fd: &Fd) -> Result<(), Failure> {
        Ok(self.files.fsync(fd.fh)?)
    }

    pub fn close(&self, fd: Fd) -> Result<(), Failure> {
        Ok(self.files.release(fd.ino, fd.fh)?)
    }

    /// The whole content of the file at `path`: what a handle open on it
    /// holds, or else what its last close committed.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, Failure> {
        let ino = self.resolve(path)?;
        let (uid, size, view) = {
            let st = self.host.0.state.lock();
            let entry = st.entries.get(&ino).ok_or(Failure::NotFound)?;
            if entry.node.is_folder() {
                return Err(Failure::IsDirectory);
            }
            (
                entry.uid.clone(),
                pdfs_drive::node_size(&entry.node),
                st.open.writer(ino).map(WriteHandle::read_view),
            )
        };
        let base = self.host.base(&uid);
        match view {
            Some(view) => Ok(assemble(&view.file, view.len, &view.written, &base)?),
            None => {
                let mut bytes = base;
                bytes.resize(size as usize, 0);
                Ok(bytes)
            }
        }
    }

    pub fn mkdir(&self, path: &str) -> Result<(), Failure> {
        let (parent, name) = self.split(path)?;
        if self.child(parent, name)?.is_some() {
            return Err(Failure::Exists);
        }
        self.namespace.mkdir(parent, name)?;
        Ok(())
    }

    pub fn unlink(&self, path: &str) -> Result<(), Failure> {
        let (parent, name) = self.split(path)?;
        Ok(self.namespace.unlink(parent, name)?)
    }

    pub fn rmdir(&self, path: &str) -> Result<(), Failure> {
        let (parent, name) = self.split(path)?;
        Ok(self.namespace.rmdir(parent, name)?)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), Failure> {
        let (parent, name) = self.split(from)?;
        let (new_parent, new_name) = self.split(to)?;
        self.child(parent, name)?.ok_or(Failure::NotFound)?;
        self.namespace
            .rename(parent, name, new_parent, new_name, RenameFlags::default())?;
        Ok(())
    }

    /// The names in the folder at `path`, sorted.
    pub fn list(&self, path: &str) -> Result<Vec<String>, Failure> {
        let ino = self.resolve(path)?;
        if !self.is_folder(ino)? {
            return Err(Failure::NotDirectory);
        }
        let st = self.host.0.state.lock();
        let mut names: Vec<String> = st
            .children
            .get(&ino)
            .into_iter()
            .flatten()
            .filter_map(|kid| st.entries.get(kid).map(|entry| entry.node.name.clone()))
            .collect();
        names.sort();
        Ok(names)
    }
}

fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/')
        .filter(|name| !name.is_empty() && *name != ".")
}
