//! The open-file service: open, write, resize, flush, barrier.
//!
//! The FUSE adapter in `pdfs-fuse` used to do all of this inside its callbacks,
//! with the kernel's reply in hand. Here it is a plain API over the tree and
//! the cache, which returns [`FileError`] and leaves the reply to the adapter.
//! What it needs from Drive and from the upload queue goes through
//! [`FilesHost`].

use std::fs::File;
use std::sync::Arc;

use pdfs_core::lockrank::Ranked;
use pdfs_drive::{node_revision_id, node_size};
use pdfs_store::cache::{ContentCache, StagedWrite};
use pdfs_store::db::Db;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use tracing::{debug, error, warn};

use crate::open::{Fallocate, Intervals, WriteError, WriteHandle};
use crate::tree::{State, StateGuard};
use crate::writes::WriteState;

/// How many times a write `open` will re-read its base when a `release`
/// published a newer queued revision underneath it (`docs/BUGS.md` B32).
///
/// One retry covers the race, which needs a close and an open of the same
/// file to overlap on different threads. More is not contention but a file
/// republished continuously, and looping on that would keep a worker busy
/// indefinitely.
pub const OPEN_BASE_ATTEMPTS: usize = 3;

/// Why the open-file service refused or failed an operation.
///
/// `E` is the host's own error for a failure to read the remote base
/// ([`FilesHost::fill_gaps`]), passed through unchanged.
#[derive(Debug)]
pub enum FileError<E> {
    /// No such node.
    NotFound,
    /// The node is a directory.
    IsDirectory,
    /// The node may be read here but not written.
    Denied,
    /// No file is open for writing under this handle.
    BadHandle,
    /// The local disk is full. The service already asked the cache to free
    /// space.
    NoSpace,
    /// The local disk quota is exhausted.
    QuotaExceeded,
    /// The scratch file failed in some other way, already logged.
    Io,
    /// The storage refused the operation itself, as `fallocate(2)` or
    /// `ftruncate(2)` can: the caller gets its error as is.
    Storage(std::io::Error),
    /// The operation is not one this handle can record.
    Unsupported,
    /// The remote base could not be read.
    Base(E),
}

/// What the open-file service needs from the rest of the daemon.
pub trait FilesHost {
    /// The error a read of the remote base fails with.
    type Error;

    /// Drive's plaintext size for a file listed with its ciphertext size, or
    /// `None` when it cannot be settled now.
    fn settle_size(&self, ino: u64, uid: &NodeUid) -> Option<u64>;

    /// Fill the bytes of `file` that `written` does not cover from Drive's
    /// revision `base_mtime`/`base_size`, after sizing it to `len`.
    fn fill_gaps(
        &self,
        uid: &NodeUid,
        file: &File,
        len: u64,
        base_mtime: i64,
        base_size: u64,
        written: &Intervals,
    ) -> Result<(), Self::Error>;

    /// The node an upload of `uid` just landed, while the tree does not have it
    /// yet.
    fn landed(&self, uid: &NodeUid) -> Option<Node>;
}

/// The open-file service of one daemon, shared by every mount.
pub struct Files<H> {
    state: Arc<Ranked<State>>,
    db: Arc<Db>,
    cache: Arc<ContentCache>,
    writes: WriteState,
    host: H,
}

impl<H: FilesHost> Files<H> {
    pub fn new(
        state: Arc<Ranked<State>>,
        db: Arc<Db>,
        cache: Arc<ContentCache>,
        writes: WriteState,
        host: H,
    ) -> Self {
        Self {
            state,
            db,
            cache,
            writes,
            host,
        }
    }

    fn state(&self) -> StateGuard<'_> {
        StateGuard::new(self.state.lock(), &self.db)
    }

    /// Open the file `ino`, for writing when `write`, and return its handle.
    ///
    /// A read-only open stays stateless and gets handle 0. A write open
    /// allocates a scratch file on disk: bytes are authored into it, and the
    /// untouched remainder is pulled from the base lazily, on read or at
    /// commit.
    pub fn open(&self, ino: u64, write: bool) -> Result<u64, FileError<H::Error>> {
        let (uid, base_mtime, base_size, provisional, base_revision_id) = {
            let mut st = self.state();
            match st.entries.get_mut(&ino) {
                Some(e) if e.node.is_file() => {
                    if write && !e.writable() {
                        return Err(FileError::Denied);
                    }
                    e.open_count = e.open_count.saturating_add(1);
                    (
                        e.uid.clone(),
                        e.node.modification_time,
                        node_size(&e.node),
                        // Still the cheap enumeration's ciphertext size (B12).
                        is_provisional(&e.node),
                        node_revision_id(&e.node),
                    )
                }
                Some(_) => return Err(FileError::IsDirectory),
                None => return Err(FileError::NotFound),
            }
        };
        debug!(ino, write, base_size, base_mtime, "open");
        if !write {
            return Ok(0);
        }
        let base = Base {
            mtime: base_mtime,
            size: base_size,
            provisional,
            revision_id: base_revision_id,
        };
        let outcome = self.install_writer(ino, &uid, base);
        if outcome.is_err() {
            // The open count was raised before any of this could fail.
            if let Some(entry) = self.state().entries.get_mut(&ino) {
                entry.open_count = entry.open_count.saturating_sub(1);
            }
        }
        outcome
    }

    /// Prepare a scratch file for a write open of `ino` and install its handle.
    ///
    /// A previous close may already have newer bytes queued locally while
    /// the server still holds the older revision. That staged blob, not the
    /// remote, is the base for this handle. In particular, `truncate file`
    /// opens for writing and then shrinks via setattr; starting its scratch
    /// as a sparse zero file would turn the preserved prefix into zeros.
    ///
    /// Complete pending blobs are the common case and can be copied without
    /// any network access. An incomplete blob still has gaps referring to
    /// the remote base, and stacking another write on it cannot be
    /// represented by WriteHandle, so the copy's gaps are filled from that
    /// base first, as the drain fills them before an upload. Only when that
    /// fails (offline, or the base is gone) does the open fail: refusing
    /// every such open failed a create over a file whose partial edit had
    /// not drained yet (`docs/BUGS.md` B204).
    ///
    /// Reading that blob and installing the handle cannot be done under one
    /// lock — the copy is the size of the file — so the base is re-checked at
    /// the moment the handle goes in, and a `release` that published a newer
    /// revision in between sends this around again. Without the re-check the
    /// new handle carried the *older* base, and its own release published
    /// content that silently discarded the revision closed a moment earlier
    /// (`docs/BUGS.md` B32). A second attempt is already unlikely and a third would
    /// mean something is republishing continuously, so give up rather than
    /// spin: the caller's `open` fails instead of its data being lost.
    fn install_writer(
        &self,
        ino: u64,
        uid: &NodeUid,
        mut base: Base,
    ) -> Result<u64, FileError<H::Error>> {
        for attempt in 0..OPEN_BASE_ATTEMPTS {
            // The handle starts at the base's size. A provisional one is the
            // ciphertext size, so the file would grow by the encryption
            // overhead and reading the base would fail (`docs/BUGS.md` B100). A
            // queued revision stamps a real size, so this only goes to the
            // network for a file the listing has not sized yet.
            if base.provisional && self.writes.queued(uid).is_none() {
                match self.host.settle_size(ino, uid) {
                    Some(size) => {
                        base.size = size;
                        base.provisional = false;
                    }
                    None => {
                        error!(%uid, "could not settle the size of the base for a write open");
                        return Err(FileError::Io);
                    }
                }
            }
            let pending_base = self.writes.queued(uid);
            let (file, path) = match self.cache.create_scratch() {
                Ok(x) => x,
                Err(e) => {
                    error!(%uid, error = %e, "create scratch file failed");
                    return Err(FileError::Io);
                }
            };
            let mut initial_written = Intervals::default();
            if let Some(pending) = &pending_base {
                // Reflink where the filesystem can: this blob is the whole
                // file, and a byte-for-byte copy of a multi-GB pending
                // revision is the kind of work `open` must not do
                // (`ContentCache::clone_or_copy`).
                if let Err(e) = ContentCache::clone_or_copy(&pending.path, &path) {
                    let _ = std::fs::remove_file(&path);
                    // The drain lets go of the pending entry before it
                    // discards the blob, so a blob that went away under the
                    // copy is a revision that landed or was replaced: the
                    // same race the re-check below catches, only earlier.
                    if self.writes.queued(uid).map(|p| p.path) != Some(pending.path.clone()) {
                        warn!(%uid, attempt, "queued revision went away while opening for write; retrying");
                        continue;
                    }
                    error!(%uid, source = %pending.path.display(), error = %e,
                        "copy queued revision into write scratch failed");
                    return Err(FileError::Io);
                }
                if !pending.meta.complete {
                    let mut authored = Intervals::default();
                    for &(s, e) in &pending.meta.authored {
                        authored.add(s, e);
                    }
                    if let Err(error) = self.host.fill_gaps(
                        uid,
                        &file,
                        pending.meta.len,
                        pending.meta.base_mtime,
                        pending.meta.base_size,
                        &authored,
                    ) {
                        let _ = std::fs::remove_file(&path);
                        error!(%uid, "cannot complete the queued revision a write open is based on");
                        return Err(FileError::Base(error));
                    }
                    debug!(%uid, "write open completed an incomplete queued revision");
                }
                initial_written.add(0, pending.meta.len);
            }
            // Sampled *before* the state lock, never under it: no site in
            // the daemon holds `pending` and `state` at once (see
            // `stamp_pending_sizes`) and this must not become the first.
            // What that costs is a window of a few instructions instead of
            // a lock-tight comparison — against the multi-gigabyte copy
            // above, which is the window that actually loses data.
            let current = self.writes.queued(uid).map(|p| p.path);
            let landed = self.host.landed(uid);
            let mut st = self.state();
            // Only a handle this open is about to *create* can carry a stale
            // base; if one already exists, that handle is authoritative and
            // the scratch prepared above is discarded either way.
            let creating = !st.open.is_writing(ino);
            if creating && current.as_ref() != pending_base.as_ref().map(|p| &p.path) {
                drop(st);
                let _ = std::fs::remove_file(&path);
                warn!(%uid, attempt, "queued revision changed while opening for write; retrying");
                continue;
            }
            // With no queued revision, the base's bytes come from the cache,
            // keyed by the base's mtime and size. The ones sampled above can
            // name a revision this daemon has since replaced: an upload that
            // lands lets go of its blob before the tree has its node, and
            // the cache holds its bytes under the node only. So the base is
            // the landing node while there is one, and the tree's otherwise
            // (docs/BUGS.md B170).
            if creating && pending_base.is_none() {
                let now = landed.or_else(|| st.entries.get(&ino).map(|e| e.node.clone()));
                if let Some(node) = now
                    && (node.modification_time != base.mtime
                        || node_revision_id(&node) != base.revision_id)
                {
                    base = Base {
                        mtime: node.modification_time,
                        size: node_size(&node),
                        provisional: is_provisional(&node),
                        revision_id: node_revision_id(&node),
                    };
                    debug!(%uid, base.mtime, base.size, "write open took the base the file has now");
                    if base.provisional {
                        drop(st);
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                }
            }
            let fh = st.open.open_writer(ino, || {
                WriteHandle {
                    ino,
                    uid: uid.clone(),
                    file: Arc::new(file),
                    path,
                    written: initial_written,
                    // Starts at the current size; reads in [0, base_size)
                    // come from the base until overwritten.
                    len: base.size,
                    base_size: base.size,
                    base_mtime: base.mtime,
                    base_revision_id: base.revision_id.clone(),
                    dirty: false,
                    open_count: 0,
                    generation: 0,
                    barrier: Default::default(),
                }
            });
            return Ok(fh);
        }
        error!(%uid, "queued revision kept changing while opening for write");
        Err(FileError::Io)
    }

    /// Stage `data` at `offset` of the write handle `fh` on `ino`, straight
    /// into its scratch file (no base download): only the untouched remainder
    /// is pulled from the remote, and only at commit.
    pub fn write(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(), FileError<H::Error>> {
        let file = {
            let st = self.state();
            match st.open.by_fh(fh) {
                Some(aw) => aw.file.clone(),
                None => return Err(FileError::BadHandle),
            }
        };
        match crate::open::stage_write(&file, offset, data) {
            Ok(()) => {}
            // A full local disk is not a broken file: answer `ENOSPC` so the
            // application can say so (and so `cp`/editors take their own
            // out-of-space path), and free what the cache is holding — the
            // scratch file the next write goes to shares that filesystem.
            Err(WriteError::NoSpace) => {
                error!(ino, fh, "scratch write failed: no space");
                self.cache.emergency_evict();
                return Err(FileError::NoSpace);
            }
            Err(WriteError::Io(e)) => {
                error!(ino, fh, error = %e, "scratch write failed");
                return Err(FileError::Io);
            }
        }
        let new_len = {
            let mut st = self.state();
            let Some(aw) = st.open.by_fh_mut(fh) else {
                return Err(FileError::BadHandle);
            };
            aw.record_write(offset, offset + data.len() as u64);
            let len = aw.len;
            st.set_size(ino, len);
            len
        };
        debug!(ino, fh, offset, len = data.len(), new_len, "staged write");
        Ok(())
    }

    /// Resize the file open for writing on `ino` to `size`. `Ok(false)` when
    /// no handle writes it, which leaves the resize to the caller's
    /// path-based truncate.
    pub fn resize(&self, ino: u64, size: u64) -> Result<bool, FileError<H::Error>> {
        let mut st = self.state();
        let Some(aw) = st.open.writer_mut(ino) else {
            return Ok(false);
        };
        aw.resize(size).map_err(FileError::Storage)?;
        st.set_size(ino, size);
        Ok(true)
    }

    /// Apply `fallocate(2)` with `mode` to the scratch file of `fh`, and record
    /// its effect on the written ranges and the length.
    pub fn fallocate(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: i32,
    ) -> Result<(), FileError<H::Error>> {
        let new_end = offset.saturating_add(length);
        // Only the modes whose effect on the scratch file this handle can
        // record. Anything else went through to the scratch file untracked:
        // a zeroed range was refilled from the base at commit, and a collapse
        // or insert shifted bytes that `len` and `written` did not follow
        // (`docs/BUGS.md` B224). `EOPNOTSUPP` is also what makes glibc's
        // `posix_fallocate` fall back to writing zeros.
        let Some(kind) = Fallocate::from_mode(mode) else {
            return Err(FileError::Unsupported);
        };
        let file = {
            let st = self.state();
            match st.open.by_fh(fh) {
                Some(aw) => aw.file.clone(),
                None if st.entries.contains_key(&ino) => return Err(FileError::BadHandle),
                None => return Err(FileError::NotFound),
            }
        };
        // The syscall can take a while on a large range; the state lock is
        // not held across it, as `write` does not hold it across its I/O.
        let ret = {
            use std::os::unix::io::AsRawFd;
            // SAFETY: the descriptor stays open for the call, through `file`.
            unsafe { libc::fallocate(file.as_raw_fd(), mode, offset as i64, length as i64) }
        };
        if ret != 0 {
            return Err(FileError::Storage(std::io::Error::last_os_error()));
        }
        let mut st = self.state();
        let grown = st
            .open
            .by_fh_mut(fh)
            .and_then(|aw| aw.record_fallocate(kind, offset, new_end));
        if let Some(end) = grown {
            st.set_size(ino, end);
        }
        Ok(())
    }

    /// The scratch file of `fh` when it has writes on it, for a flush to sync.
    /// `None` for a handle with nothing of its own to sync.
    pub fn dirty_scratch(&self, fh: u64) -> Option<Arc<File>> {
        let st = self.state();
        st.open
            .by_fh(fh)
            .filter(|aw| aw.dirty)
            .map(|aw| aw.file.clone())
    }

    /// Sync the data of `file`, the scratch file [`Files::dirty_scratch`]
    /// returned.
    pub fn flush(&self, fh: u64, file: &File) -> Result<(), FileError<H::Error>> {
        file.sync_data().map_err(|e| {
            error!(fh, error = %e, "flush of scratch file failed");
            self.sync_error(&e)
        })
    }

    /// Make the bytes written through `fh` survive a crash.
    ///
    /// Durability here means the scratch file, not the remote: a caller asking
    /// for fsync wants its bytes to survive a crash, and blocking it on an
    /// upload (which offline would never finish) buys nothing the queue does
    /// not already guarantee.
    pub fn fsync(&self, fh: u64) -> Result<(), FileError<H::Error>> {
        // The handle's barrier first, and only then what it holds: whatever this
        // fsync records is at least as new as what any fsync before it on the
        // same file recorded, however the two were scheduled.
        let Some(barrier) = self.state().open.by_fh(fh).map(|aw| aw.barrier.clone()) else {
            // A handle opened for reading has nothing of its own to make
            // durable, and POSIX allows `fsync` on it; `EBADF` failed `sync
            // FILE` and backup tools that sync what they read (`docs/BUGS.md`
            // B222).
            return Ok(());
        };
        let mut durable = barrier.lock();
        // Everything the durability sidecar needs, copied out so no state lock
        // is held across the sync.
        let handle = {
            let st = self.state();
            st.open.by_fh(fh).map(|aw| {
                (
                    aw.file.clone(),
                    aw.path.clone(),
                    aw.uid.clone(),
                    aw.dirty,
                    aw.written.clone(),
                    aw.len,
                    aw.base_size,
                    aw.base_mtime,
                    aw.base_revision_id.clone(),
                    aw.generation,
                )
            })
        };
        // Released while this waited for the barrier: the release staged it.
        let Some((
            f,
            path,
            uid,
            dirty,
            written,
            len,
            base_size,
            base_mtime,
            base_revision_id,
            generation,
        )) = handle
        else {
            return Ok(());
        };
        if let Err(e) = f.sync_all() {
            error!(fh, error = %e, "fsync of scratch file failed");
            return Err(self.sync_error(&e));
        }
        // A clean handle has nothing on it that the remote does not already
        // hold, so there is nothing for a crash to lose.
        if !dirty {
            return Ok(());
        }
        // An earlier fsync already recorded this very generation.
        if generation == *durable {
            return Ok(());
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
            complete: StagedWrite::covers(&authored, len),
            authored,
            based_on: self
                .writes
                .baseline(&uid, base_mtime, base_size, base_revision_id),
        };
        // A failed sidecar means the write is not durable, and `fsync` promising
        // otherwise is the defect this exists to fix — so it is an error, not a
        // warning.
        if let Err(e) = self.cache.mark_scratch_durable(&path, &meta) {
            error!(%uid, error = %e, "recording fsync durability failed");
            if e.is_disk_full() {
                self.cache.emergency_evict();
                return Err(FileError::NoSpace);
            }
            return Err(FileError::Io);
        }
        *durable = generation;
        Ok(())
    }

    /// The error for a failed sync of a scratch file. Delayed allocation means
    /// a full disk usually surfaces at the sync rather than at `write(2)`, and
    /// running out of space is what a program checks for.
    fn sync_error(&self, e: &std::io::Error) -> FileError<H::Error> {
        match e.raw_os_error() {
            Some(libc::ENOSPC) => {
                self.cache.emergency_evict();
                FileError::NoSpace
            }
            Some(libc::EDQUOT) => FileError::QuotaExceeded,
            _ => FileError::Io,
        }
    }
}

/// The revision a write open starts from.
struct Base {
    mtime: i64,
    size: u64,
    /// Whether `size` is still the listing's ciphertext size.
    provisional: bool,
    revision_id: Option<String>,
}

/// Whether `node` still carries the cheap enumeration's ciphertext size
/// instead of its plaintext one (B12).
fn is_provisional(node: &Node) -> bool {
    matches!(
        &node.kind,
        NodeKind::File {
            claimed_size: None,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use pdfs_core::Access;
    use pdfs_core::lockrank::Rank;
    use pdfs_store::cache::{Baseline, StagedWrite};
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    use super::*;
    use crate::open::PendingRevision;

    /// A unique temp directory removed on drop; avoids a dev-dependency.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-files-{}-{}",
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

    /// A host with no network: no size settles and no gap can be filled.
    struct Offline;

    impl FilesHost for Offline {
        type Error = &'static str;

        fn settle_size(&self, _ino: u64, _uid: &NodeUid) -> Option<u64> {
            None
        }

        fn fill_gaps(
            &self,
            _uid: &NodeUid,
            _file: &File,
            _len: u64,
            _base_mtime: i64,
            _base_size: u64,
            _written: &Intervals,
        ) -> Result<(), &'static str> {
            Err("offline")
        }

        fn landed(&self, _uid: &NodeUid) -> Option<Node> {
            None
        }
    }

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    struct Fixture {
        files: Files<Offline>,
        state: Arc<Ranked<State>>,
        writes: WriteState,
        db: Arc<Db>,
        _dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new();
            let db = Arc::new(Db::open(&dir.0.join("cache.db")).unwrap());
            let cache = Arc::new(
                ContentCache::open(dir.0.join("content"), dir.0.join("pins"), 0, db.clone())
                    .unwrap(),
            );
            let state = Arc::new(Ranked::new(
                Rank::State,
                State::new(db.clone(), HashMap::new()),
            ));
            let writes = WriteState::default();
            let files = Files::new(state.clone(), db.clone(), cache, writes.clone(), Offline);
            Fixture {
                files,
                state,
                writes,
                db,
                _dir: dir,
            }
        }

        fn state(&self) -> StateGuard<'_> {
            StateGuard::new(self.state.lock(), &self.db)
        }

        /// Put a writable node in the tree and return its inode.
        fn add(&self, link: &str, is_dir: bool) -> u64 {
            let node = crate::nodes::local_node(uid(link), uid("root"), link.to_owned(), is_dir);
            let mut st = self.state();
            let ino = st.intern(0, node);
            st.entries.get_mut(&ino).unwrap().access = Access::Owner;
            ino
        }

        fn size(&self, ino: u64) -> u64 {
            node_size(&self.state().entries[&ino].node)
        }

        fn open_count(&self, ino: u64) -> u32 {
            self.state().entries[&ino].open_count
        }
    }

    #[test]
    fn a_write_open_takes_writes_and_resizes_and_survives_an_fsync() {
        let fx = Fixture::new();
        let ino = fx.add("f", false);
        let fh = fx.files.open(ino, true).unwrap();
        assert_ne!(fh, 0);
        fx.files.write(ino, fh, 0, b"hello world").unwrap();
        assert_eq!(fx.size(ino), 11);
        assert!(fx.files.resize(ino, 5).unwrap());
        assert_eq!(fx.size(ino), 5);
        fx.files.fsync(fh).unwrap();

        // What a restart finds: the cache opened again over the same disk.
        let restarted = ContentCache::open(
            fx._dir.0.join("content"),
            fx._dir.0.join("pins"),
            0,
            fx.db.clone(),
        )
        .unwrap();
        let recovered = restarted.recovered_writes();
        assert_eq!(recovered.len(), 1);
        let (path, meta) = &recovered[0];
        assert_eq!(meta.len, 5);
        assert!(meta.complete);
        assert_eq!(std::fs::read(path).unwrap(), b"hello");
    }

    /// An fsync that has to wait for an earlier one on the same file records
    /// what the file holds once it gets its turn, not what it held when it
    /// was called. Both used to read the handle first, so the one that
    /// finished last could write the older sidecar over the newer.
    #[test]
    fn an_fsync_records_what_the_file_holds_when_its_turn_comes() {
        let fx = Fixture::new();
        let ino = fx.add("f", false);
        let fh = fx.files.open(ino, true).unwrap();
        fx.files.write(ino, fh, 0, b"one").unwrap();
        let barrier = fx.state().open.by_fh(fh).unwrap().barrier.clone();
        let earlier = barrier.lock();
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| fx.files.fsync(fh));
            std::thread::sleep(std::time::Duration::from_millis(50));
            fx.files.write(ino, fh, 3, b" two").unwrap();
            drop(earlier);
            waiting.join().unwrap().unwrap();
        });
        let generation = fx.state().open.by_fh(fh).unwrap().generation;
        assert_eq!(*barrier.lock(), generation);

        let restarted = ContentCache::open(
            fx._dir.0.join("content"),
            fx._dir.0.join("pins"),
            0,
            fx.db.clone(),
        )
        .unwrap();
        let recovered = restarted.recovered_writes();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].1.len, 7);
        assert_eq!(std::fs::read(&recovered[0].0).unwrap(), b"one two");
    }

    #[test]
    fn a_read_only_open_is_stateless_and_a_directory_is_not_opened() {
        let fx = Fixture::new();
        let file = fx.add("f", false);
        let dir = fx.add("d", true);
        assert_eq!(fx.files.open(file, false).unwrap(), 0);
        assert!(!fx.state().open.is_writing(file));
        assert!(matches!(
            fx.files.open(dir, true),
            Err(FileError::IsDirectory)
        ));
        assert!(matches!(
            fx.files.open(999_999, false),
            Err(FileError::NotFound)
        ));
    }

    #[test]
    fn a_write_open_of_a_read_only_node_is_denied() {
        let fx = Fixture::new();
        let ino = fx.add("f", false);
        fx.state().entries.get_mut(&ino).unwrap().access = Access::Viewer;
        assert!(matches!(fx.files.open(ino, true), Err(FileError::Denied)));
        assert_eq!(fx.files.open(ino, false).unwrap(), 0);
    }

    #[test]
    fn a_handle_that_is_not_open_is_refused() {
        let fx = Fixture::new();
        let ino = fx.add("f", false);
        assert!(matches!(
            fx.files.write(ino, 42, 0, b"x"),
            Err(FileError::BadHandle)
        ));
        assert!(!fx.files.resize(ino, 0).unwrap());
        assert!(fx.files.fsync(42).is_ok());
        assert!(fx.files.dirty_scratch(42).is_none());
    }

    #[test]
    fn an_fallocate_mode_the_handle_cannot_record_is_unsupported() {
        let fx = Fixture::new();
        let ino = fx.add("f", false);
        let fh = fx.files.open(ino, true).unwrap();
        assert!(matches!(
            fx.files
                .fallocate(ino, fh, 0, 4096, libc::FALLOC_FL_COLLAPSE_RANGE),
            Err(FileError::Unsupported)
        ));
        fx.files.fallocate(ino, fh, 0, 4096, 0).unwrap();
        assert_eq!(fx.size(ino), 4096);
    }

    /// A file still listed with its ciphertext size cannot be sized offline,
    /// and the failed open gives back the open count it took.
    #[test]
    fn a_failed_write_open_gives_back_its_open_count() {
        let fx = Fixture::new();
        let ino = fx.add("f", false);
        if let NodeKind::File { claimed_size, .. } =
            &mut fx.state().entries.get_mut(&ino).unwrap().node.kind
        {
            *claimed_size = None;
        }
        assert!(matches!(fx.files.open(ino, true), Err(FileError::Io)));
        assert_eq!(fx.open_count(ino), 0);
    }

    /// A write open over a partial queued edit has to fill its gaps from
    /// Drive first. Offline it fails with the host's own error.
    #[test]
    fn a_write_open_over_a_partial_queued_edit_reports_the_base_failure() {
        let fx = Fixture::new();
        let ino = fx.add("f", false);
        let path = fx._dir.0.join("queued");
        std::fs::write(&path, b"\0\0\0edit").unwrap();
        fx.writes.pending.lock().insert(
            uid("f"),
            PendingRevision {
                path,
                meta: StagedWrite {
                    uid: uid("f").to_string(),
                    len: 7,
                    base_size: 3,
                    base_mtime: 1,
                    authored: vec![(3, 7)],
                    complete: false,
                    based_on: Some(Baseline {
                        mtime: 1,
                        size: 3,
                        hash: None,
                        revision_id: None,
                    }),
                },
            },
        );
        assert!(matches!(
            fx.files.open(ino, true),
            Err(FileError::Base("offline"))
        ));
        assert_eq!(fx.open_count(ino), 0);
    }
}
