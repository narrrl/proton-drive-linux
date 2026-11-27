//! Kernel-facing FUSE callback adapter.

use super::*;
use pdfs_vfs::files::Files;
use pdfs_vfs::namespace::{Namespace, Renamed};
use pdfs_vfs::open::{DirListing, dir_page};

impl Filesystem for ProtonFs {
    /// Negotiate kernel-side behaviour once, before any other callback.
    ///
    /// Everything here is a hint the kernel is free to decline, so each is
    /// requested and its refusal logged rather than treated as fatal — an older
    /// kernel that lacks a capability must still mount.
    ///
    /// Deliberately *not* requested: `FUSE_ATOMIC_O_TRUNC` and
    /// `FUSE_DO_READDIRPLUS` change what the kernel sends (truncation folded
    /// into `open`, `readdirplus` instead of `readdir` + N lookups), so they
    /// need the matching handlers first; and `FOPEN_KEEP_CACHE` needs the write
    /// path to invalidate page cache on every locally staged revision, not just
    /// on remote events.
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Readahead the kernel issues per file. Matching the block size means a
        // sequential reader's readahead lands inside the block a demand read is
        // already fetching, instead of one 128 KiB slice at a time.
        if let Err(nearest) = config.set_max_readahead(BLOCK_SIZE as u32) {
            debug!(nearest, "kernel capped max_readahead");
            let _ = config.set_max_readahead(nearest);
        }
        if let Err(nearest) = config.set_max_background(MAX_BACKGROUND) {
            debug!(nearest, "kernel capped max_background");
        }
        // Without this the kernel serialises lookups within one directory, so a
        // cold `ls` of a folder is a chain of round trips rather than a fan-out.
        if let Err(unsupported) = config.add_capabilities(InitFlags::FUSE_PARALLEL_DIROPS) {
            debug!(?unsupported, "kernel does not support parallel dirops");
        }
        if let Err(unsupported) = config.add_capabilities(InitFlags::FUSE_CACHE_SYMLINKS) {
            debug!(?unsupported, "kernel does not support symlink caching");
        }
        Ok(())
    }

    /// Free and total space, as `df` and every "will this fit?" preflight ask.
    ///
    /// Reported in [`REPORTED_BLKSIZE`] units against the *account* quota, not
    /// the local cache: what a copy into the mount consumes is Drive storage.
    /// The default implementation answers all-zeroes, which reads as a full
    /// filesystem and makes installers and file managers refuse to write.
    ///
    /// On a worker, because a cold answer is a round trip
    /// ([`Core::account_quota_cached`] serves a warm one from memory).
    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let core = self.core.clone();
        self.core.workers.run(Lane::Meta, "statfs", move || {
            let (max_space, used_space) = core.account_quota_cached().unwrap_or((0, 0));
            let bsize = u64::from(REPORTED_BLKSIZE);
            let blocks = (max_space.max(0) as u64).div_ceil(bsize);
            let used = (used_space.max(0) as u64).div_ceil(bsize);
            let free = blocks.saturating_sub(used);
            // `files`/`ffree` are unknowable — Drive has no inode budget — so
            // they are reported as "plenty" rather than as zero, which some
            // tools read as "no more files may be created".
            reply.statfs(
                blocks,
                free,
                free,
                0,
                u64::MAX,
                REPORTED_BLKSIZE,
                MAX_NAME_LEN,
                REPORTED_BLKSIZE,
            );
        });
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = self.tree(parent).0;
        let name = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        // A folder that has not been listed yet is enumerated from the remote,
        // so serve it from a worker rather than stalling the dispatch loop. A
        // listed folder — the common case — is a map hit, and answering it
        // inline costs less than the handoff would.
        if self.core.children_cached(parent) {
            self.serve_lookup(parent, &name, reply, false);
            return;
        }
        let fs = self.clone();
        self.core.workers.run(Lane::Meta, "lookup", move || {
            fs.serve_lookup(parent, &name, reply, true)
        });
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let ino = self.tree(ino);
        let mut st = self.core.state();
        st.forget_lookup(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino = self.tree(ino);
        let (attr, provisional_in) = {
            let st = self.core.state();
            match st.entries.get(&ino.0) {
                Some(e) => {
                    // A file still carrying the cheap enumeration's placeholder
                    // size (B12).
                    let provisional = matches!(
                        &e.node.kind,
                        NodeKind::File {
                            claimed_size: None,
                            ..
                        }
                    )
                    .then(|| (e.parent, e.uid.clone()));
                    (self.attr(ino.0, &e.node, e.access), provisional)
                }
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // The common case: the size is real, answer from the lock we just held.
        let Some((parent, uid)) = provisional_in else {
            reply.attr(&TTL, &attr);
            return;
        };
        // A provisional size is the *ciphertext* size, which is larger than the
        // file. Publishing it makes every reader that trusts `st_size` — rsync,
        // mmap, sendfile, a sized read loop — run off the end of the file and
        // fail. So resolve it before answering rather than after (`docs/BUGS.md` B14).
        //
        // The cost is one batched round trip for the whole folder, not one per
        // file: `ls -l` is one `getattr` per entry and they collapse onto a
        // single upgrade. This is why B12's split is still worth having — the
        // plain listing never comes here at all.
        //
        // Starting the batch is local work — a `state` read and a thread spawn —
        // so it happens here; what goes to the network is the batch itself, and
        // this reply is parked on it rather than waiting on any thread of the
        // mount's (audit F4).
        let ino = ino.0;
        let fs = self.clone();
        let respond = move || {
            // Re-read: the upgrade writes through `state`, and on timeout or
            // failure this is simply the provisional attr we already had.
            let attr = {
                let st = fs.core.state();
                match st.entries.get(&ino) {
                    Some(e) => fs.attr(ino, &e.node, e.access),
                    // Forgotten while we waited.
                    None => attr,
                }
            };
            reply.attr(&TTL, &attr);
        };
        match self.core.upgrade_sizes_for_parent(ino, &uid, parent) {
            Some(slot) => self.core.await_size(ino, slot, respond),
            // Nothing to upgrade, so nothing to wait for.
            None => respond(),
        }
    }

    /// Freeze this directory's listing for the life of the handle.
    ///
    /// `readdir` is paged, and its continuation offset only means anything
    /// against a listing that does not move underneath it. Taking the snapshot
    /// here — once per `opendir`, which the kernel pairs exactly with one
    /// `releasedir` — is what makes the offsets stable, so a create or a trash
    /// during an enumeration can no longer make the caller skip or repeat an
    /// entry (`docs/BUGS.md` B43).
    ///
    /// Enumerating a cold folder is a remote call, so it goes to a worker for
    /// the same reason `lookup` and `readdir` do.
    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let ino = self.tree(ino).0;
        if self.core.children_cached(ino) {
            self.serve_opendir(ino, reply);
            return;
        }
        let fs = self.clone();
        self.core
            .workers
            .run(Lane::Meta, "opendir", move || fs.serve_opendir(ino, reply));
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        let ino = self.tree(ino).0;
        // The snapshot is the whole point: served from it, this is a pure
        // in-memory walk with no chance of a remote call, so it never needs a
        // worker. Only a handle whose `opendir` failed to snapshot (or a kernel
        // that issued `readdir` without one) falls back to the live path.
        if let Some(listing) = self.core.state().open.dir(fh.0) {
            reply_listing(&listing, offset, self.root, reply);
            return;
        }
        // Same split as `lookup`: only the cold, remote-enumerating path pays
        // for a worker.
        if self.core.children_cached(ino) {
            self.serve_readdir(ino, offset, reply);
            return;
        }
        let fs = self.clone();
        self.core.workers.run(Lane::Meta, "readdir", move || {
            fs.serve_readdir(ino, offset, reply)
        });
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.core.state().open.close_dir(fh.0);
        reply.ok();
    }

    /// A read-only open is a map lookup and answers inline; a write open has to
    /// allocate a scratch file and, when a queued revision is the base, duplicate
    /// a blob that is the whole file — so it is served from a worker rather than
    /// stalling every other request on the mount behind it.
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino = self.tree(ino);
        if flags.acc_mode() == OpenAccMode::O_RDONLY {
            self.serve_open(ino, flags, reply);
            return;
        }
        let fs = self.clone();
        self.core.workers.run(Lane::Transfer, "open", move || {
            fs.serve_open(ino, flags, reply)
        });
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
        let ino = self.tree(ino);
        // A file open for writing is served from its handle so reads see the
        // in-flight (possibly unsaved) content: authored bytes come from the
        // scratch file, untouched bytes from the base.
        //
        // Answered on the dispatch loop when everything it needs is on this
        // machine, which keeps the order `write` has with reads of the same
        // handle. A base range that has to be downloaded goes to a worker
        // instead, with the handle as it was when the read came in: the kernel
        // issued the read before any write that comes after it, so that is
        // the content it asked for. On the loop, the download held every
        // request of the mount (`docs/BUGS.md` B227).
        // A writer still open, or one whose release is still queueing it.
        let handle = self
            .core
            .state()
            .open
            .reader(ino.0)
            .map(|v| (v.file, v.len, v.uid, v.base_mtime, v.base_size, v.written));
        if let Some((file, len, uid, base_mtime, base_size, written)) = handle {
            match self.core.serve_open_read_local(
                &file,
                len,
                &uid,
                base_mtime,
                base_size,
                &written,
                offset,
                size as u64,
            ) {
                Ok(Some(bytes)) => reply.data(&bytes),
                Ok(None) => {
                    let fs = self.clone();
                    self.core.workers.run(Lane::Transfer, "read", move || {
                        match fs.core.serve_open_read(
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
                    });
                }
                Err(e) => reply.error(e),
            }
            return;
        }
        let (uid, mtime, fsize, provisional, is_video) = {
            let st = self.core.state();
            match st.entries.get(&ino.0) {
                Some(e) if e.node.is_file() => {
                    let media_type = match &e.node.kind {
                        NodeKind::File { media_type, .. } => Some(media_type.as_str()),
                        NodeKind::Folder => None,
                    };
                    let is_video =
                        PhotoKind::classify(Some(&e.node.name), media_type) == PhotoKind::Video;
                    let queued = self.core.writes.queued_len(&e.uid);
                    // Still the cheap enumeration's ciphertext size (B12).
                    let provisional = queued.is_none()
                        && matches!(
                            &e.node.kind,
                            NodeKind::File {
                                claimed_size: None,
                                ..
                            }
                        );
                    let fsize = queued.unwrap_or_else(|| node_size(&e.node));
                    (
                        e.uid.clone(),
                        e.node.modification_time,
                        fsize,
                        provisional,
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
        self.core.workers.run(Lane::Transfer, "read", move || {
            // The revision disagrees with a provisional size, and B84 fails
            // such a read outright, so settle the size first. On the network,
            // hence here and not above (`docs/BUGS.md` B100).
            let fsize = match provisional {
                true => core.resolve_provisional_size(ino.0, &uid).unwrap_or(fsize),
                false => fsize,
            };
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
        let parent = self.tree(parent).0;
        if let Err(error) = self.core.require_writable(parent) {
            reply.error(error);
            return;
        }
        let name = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        // Minting a node on the remote is one round trip plus a `fetch_node`,
        // and a cold parent adds an enumeration before either. Answering that
        // inline stalls the whole mount: fuser's dispatch loop is
        // non-concurrent, so eight applications each creating a file were served
        // one create at a time, with `ls` on the same mount timing out for as
        // long as the burst lasted. See [`Workers`].
        let fs = self.clone();
        self.core.workers.run(Lane::Meta, "create", move || {
            fs.serve_create(parent, &name, reply)
        });
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
        let ino = self.tree(ino);
        if let Err(error) = self.core.require_writable(ino.0) {
            reply.error(error);
            return;
        }
        match self.stage_write(ino.0, fh.0, offset, data) {
            Ok(()) => reply.written(data.len() as u32),
            Err(error) => reply.error(error),
        }
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
        let ino = self.tree(ino);
        if let Err(error) = self.core.require_writable(ino.0) {
            reply.error(error);
            return;
        }
        // Only resizes change remote state; everything else (mode/owner/times) is
        // accepted and ignored so tools that chmod/utimes after writing succeed.
        if let Some(size) = size {
            let fh = fh.map(|f| f.0).filter(|&f| f != 0);
            let handled = match self.files.resize(ino.0, size).map_err(file_errno) {
                Ok(handled) => handled,
                Err(error) => {
                    debug!(ino = ino.0, size, ?error, "setattr resize refused");
                    reply.error(error);
                    return;
                }
            };
            debug!(ino = ino.0, size, fh = ?fh, handled, "setattr resize");
            if !handled {
                // Path-based truncate with no open write handle — a shell's
                // `> file`. This is the second write path into the API, and
                // queueing it is what lets a redirect work offline at all: it
                // never reaches `release`, so without this it failed before any
                // byte was written.
                //
                // The one branch of `setattr` that can reach the network:
                // shrinking keeps the prefix, which `queue_truncate` gap-fills
                // from the remote. So it answers from a worker, while the
                // handle-backed resize above stays inline (it is a `set_len` on
                // the scratch file).
                let ino = ino.0;
                let fs = self.clone();
                self.core.workers.run(Lane::Transfer, "truncate", move || {
                    if let Err(e) = fs.core.queue_truncate(ino, size) {
                        reply.error(e);
                        return;
                    }
                    fs.reply_attr(ino, reply);
                });
                return;
            }
        }
        self.reply_attr(ino.0, reply);
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let ino = self.tree(ino);
        if let Err(error) = self.core.require_writable(ino.0) {
            reply.error(error);
            return;
        }
        match self
            .files
            .fallocate(ino.0, _fh.0, offset, length, mode)
            .map_err(file_errno)
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    /// `close(2)` calls flush before release, and flush is the only one of the
    /// two whose answer reaches the program: the kernel drops `release`'s. So
    /// the scratch file of a handle with writes on it is synced here, on a
    /// worker, and a full disk or a failing one is what `close` returns.
    ///
    /// Before, flush answered `ok` at once and `release` synced the file, so an
    /// `ENOSPC` that delayed allocation held back until then was reported to
    /// nobody, and the program took the save as done (`docs/BUGS.md` B222).
    /// It also means `release`, which stays on the dispatch loop for ordering,
    /// finds the pages already on disk instead of writing back a whole file
    /// while every other request on the mount waits.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let Some(file) = self.files.dirty_scratch(fh.0) else {
            reply.ok();
            return;
        };
        let fs = self.clone();
        self.core.workers.run(Lane::Transfer, "flush", move || {
            match fs.files.flush(fh.0, &file) {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(file_errno(error)),
            }
        });
    }

    /// Durability here means the scratch file, not the remote: a caller asking
    /// for fsync wants its bytes to survive a crash, and blocking it on an upload
    /// (which offline would never finish) buys nothing the queue does not already
    /// guarantee.
    ///
    /// Served on a worker: `sync_all` is a disk barrier and the sidecar write is
    /// two more file operations, and an application that fsyncs per record would
    /// otherwise stall every request on the mount at its barrier rate. Nothing
    /// orders this against other callbacks — the kernel waits for the reply
    /// either way, which is the whole contract.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let fs = self.clone();
        self.core
            .workers
            .run(Lane::Transfer, "fsync", move || fs.serve_fsync(fh, reply));
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let ino = self.tree(ino);
        // Deliberately **not** handed to a worker, unlike the other slow
        // handlers here. The kernel does not wait for a `release` reply before
        // letting `close(2)` return, so the only thing sequencing this against
        // the `open`+`read` an application issues immediately afterwards is that
        // both are served by the same dispatch loop. Off a worker, the read
        // overtakes the staging and sees the *remote's* older revision: the
        // acceptance suite's concurrency case failed with "concurrent file 1
        // mismatch" on all three runs the moment this moved.
        //
        // That inherited ordering is now affordable because the network came out
        // of this path: `queue_revision` no longer downloads the ranges an edit
        // did not touch, it stages an incomplete blob and lets the drain thread
        // finish it (audit F1). What is left is a rename, two fsyncs, a database
        // insert, and at most a bounded copy out of the local cache.
        match self.files.release(ino.0, fh.0) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(file_errno(error)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = self.tree(parent).0;
        if let Err(error) = self.core.require_writable(parent) {
            reply.error(error);
            return;
        }
        let name_str = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        // `trash_child` is a remote call; see `serve_create`.
        let fs = self.clone();
        self.core.workers.run(Lane::Meta, "unlink", move || {
            fs.serve_unlink(parent, &name_str, reply)
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = self.tree(parent).0;
        if let Err(error) = self.core.require_writable(parent) {
            reply.error(error);
            return;
        }
        let name_str = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        // Enumerates the folder to apply rmdir's emptiness rule, then trashes
        // it: two remote calls. See `serve_create`.
        let fs = self.clone();
        self.core.workers.run(Lane::Meta, "rmdir", move || {
            fs.serve_rmdir(parent, &name_str, reply)
        });
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
        let parent = self.tree(parent).0;
        if let Err(error) = self.core.require_writable(parent) {
            reply.error(error);
            return;
        }
        let name = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        // Off the dispatch loop for the same reason as `create`: this is a
        // remote round trip, and inline it blocks every other request on the
        // mount until it answers.
        let fs = self.clone();
        self.core.workers.run(Lane::Meta, "mkdir", move || {
            fs.serve_mkdir(parent, &name, reply)
        });
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent = self.tree(parent).0;
        let newparent = self.tree(newparent).0;
        if let Err(error) = self
            .core
            .require_writable(parent)
            .and_then(|()| self.core.require_writable(newparent))
        {
            reply.error(error);
            return;
        }
        let name = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let newname = match fuse_name(newname) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        // A rename that reaches the remote is up to two calls (rename, then
        // move), and the replaced-destination path adds a trash on top. See
        // `serve_create` for why none of that may run on the dispatch loop.
        let fs = self.clone();
        self.core.workers.run(Lane::Meta, "rename", move || {
            fs.serve_rename(parent, &name, newparent, &newname, flags, reply)
        });
    }

    fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let ino = self.tree(ino);
        let state = self.core.state();
        let Some(entry) = state.entries.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if !access_allowed(entry.node.is_folder(), entry.access, mask) {
            reply.error(Errno::EACCES);
        } else {
            reply.ok();
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let ino = self.tree(ino);
        // Always off the dispatch loop: a miss goes to the wire, and a lister
        // that stats a directory issues one of these per file per advertised
        // name — inline, that serialized the whole mount behind ~186 ms of
        // network per call (B5).
        let fs = self.clone();
        let name = name.to_os_string();
        self.core.workers.run(Lane::Meta, "getxattr", move || {
            fs.serve_getxattr(ino, &name, size, reply)
        });
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let ino = self.tree(ino);
        self.serve_listxattr(ino, size, reply);
    }

    fn ioctl(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: IoctlFlags,
        _cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: ReplyIoctl,
    ) {
        // Filesystems are not character devices or terminals; returning ENOTTY
        // for unhandled ioctls is standard Linux POSIX behavior and prevents
        // fuser warning logs when applications probe file descriptors.
        reply.error(Errno::ENOTTY);
    }

    // The callbacks below are deliberately unsupported. Each answers with the
    // same errno `fuser`'s default would, minus the `[Not Implemented]` WARN it
    // logs on every call — a probe is a normal thing for an application to do,
    // not an incident. git alone emits one `link` per loose object, which
    // buried real diagnostics in the journal (`docs/BUGS.md` B73). The errno
    // contract these keep is asserted by the acceptance suite's clean-refusal
    // case, so it must not drift.

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Drive stores files and folders; devices, FIFOs and sockets have no
        // representation. Regular files arrive through `create`, never here.
        reply.error(Errno::ENOSYS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        // EPERM, not ENOSYS: the operation is meaningful, Drive simply cannot
        // represent it, and callers treat EPERM as a definite "no" rather than
        // retrying against an older interface.
        reply.error(Errno::EPERM);
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        // A node has exactly one parent on Drive, so there are no hard links.
        // git falls back to rename() on EPERM, which this filesystem supports.
        reply.error(Errno::EPERM);
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // ENOSYS, which the kernel absorbs: it clears its fsyncdir probe and
        // reports success to the caller. There is no dirty directory buffer to
        // flush — directory changes are durable in SQLite before the callback
        // that made them returns.
        reply.error(Errno::ENOSYS);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        // Drive has no user-extended-attribute store. ENOSYS reaches userspace
        // as EOPNOTSUPP, the errno callers expect from a filesystem without
        // xattrs. Reads (`getxattr`/`listxattr`) are implemented, since the
        // thumbnail interface is exposed that way.
        reply.error(Errno::ENOSYS);
    }

    fn lseek(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: i64,
        _whence: i32,
        reply: ReplyLseek,
    ) {
        // Must stay ENOSYS. A file's holes are not knowable without fetching
        // its blocks, and ENOSYS is what makes the kernel stop asking and
        // emulate SEEK_DATA/SEEK_HOLE itself. Any other errno would surface to
        // the caller as a failed seek.
        reply.error(Errno::ENOSYS);
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        _fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        // Copied here when the source bytes are on this machine, as `write`
        // would stage them, minus a FUSE round trip per page: `cp` of a cached
        // 256 MiB file ran at about 100 MiB/s through read and write. Anything
        // that would have to be downloaded gets `EOPNOTSUPP`, which makes the
        // kernel copy that one call through read and write itself; `ENOSYS`
        // would turn the op off for the whole mount. There is no server-side
        // copy into a file `cp` has already created, so the copy still uploads.
        //
        // On the dispatch loop, like `write`, so it keeps its order with the
        // reads and writes of the same handle. [`COPY_RANGE_MAX`] bounds how
        // long one call holds the loop.
        let (ino_in, ino_out) = (self.tree(ino_in), self.tree(ino_out));
        if !flags.is_empty() || ino_in == ino_out {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        if let Err(error) = self.core.require_writable(ino_out.0) {
            reply.error(error);
            return;
        }
        let bytes = match self
            .core
            .read_local(ino_in.0, offset_in, len.min(COPY_RANGE_MAX))
        {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                debug!(ino_in = ino_in.0, offset_in, "copy range not local");
                reply.error(Errno::EOPNOTSUPP);
                return;
            }
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        if bytes.is_empty() {
            reply.written(0);
            return;
        }
        match self.stage_write(ino_out.0, fh_out.0, offset_out, &bytes) {
            Ok(()) => reply.written(bytes.len() as u32),
            Err(error) => reply.error(error),
        }
    }
}

/// The Proton Drive VFS. FUSE callbacks are synchronous, so the Tokio handle
/// bridges each one to the async SDK via `tokio::runtime::Handle::block_on`; the fuser
/// session thread is not a runtime worker, so blocking on it is sound.
/// Cloneable so a handler can move a copy onto a `Workers` thread and answer
/// from there; every field is a handle or a plain id.
#[derive(Clone)]
pub struct ProtonFs {
    core: Core,
    /// The open-file service, shared with every other mount of the daemon.
    files: Arc<Files<Core>>,
    namespace: Arc<Namespace<Core>>,
    uid: u32,
    gid: u32,
    /// The tree inode of this session's root, which the kernel knows as
    /// [`ROOT_INO`].
    root: u64,
}

/// The most one `copy_file_range` call copies; the caller loops for the rest.
/// A local copy of this much takes milliseconds, which is how long it holds
/// the dispatch loop.
const COPY_RANGE_MAX: u64 = 16 * 1024 * 1024;

/// Fill a `readdir` reply from `listing`, resuming at `offset`.
///
/// The cookie handed back is the index of the *next* entry, so a resumed call
/// continues exactly where the last one stopped — which is only true because
/// the listing behind those indexes is a snapshot (`docs/BUGS.md` B43).
fn reply_listing(listing: &DirListing, offset: u64, root: u64, mut reply: ReplyDirectory) {
    for (next, (ino, is_dir, name)) in dir_page(listing, offset) {
        let ft = if *is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        if reply.add(kernel_ino(root, *ino), next, ft, name) {
            break;
        }
    }
    reply.ok();
}

/// The inode the kernel knows tree inode `ino` by, in a session rooted at
/// `root`. The inverse of [`ProtonFs::tree`].
pub(crate) fn kernel_ino(root: u64, ino: u64) -> INodeNo {
    INodeNo(if ino == root { ROOT_INO } else { ino })
}

pub(crate) fn access_allowed(is_dir: bool, access: Access, mask: AccessFlags) -> bool {
    (!mask.contains(AccessFlags::W_OK) || access.writable())
        && (!mask.contains(AccessFlags::X_OK) || is_dir)
}

impl ProtonFs {
    /// Build the filesystem rooted at `root` (the user's My Files folder, or
    /// the folder an on-demand location shows).
    pub(super) fn new(core: Core, root: Node) -> Self {
        let root = core.state().install_root(root);
        let _ = core.session.root.set(root);
        // SAFETY: geteuid/getegid are infallible and have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            files: core.files(),
            namespace: core.namespace(),
            core,
            uid,
            gid,
            root,
        }
    }

    /// The tree inode the kernel means by `ino`: this session's root for
    /// [`ROOT_INO`], and every other inode as it is.
    fn tree(&self, ino: INodeNo) -> INodeNo {
        if ino.0 == ROOT_INO {
            INodeNo(self.root)
        } else {
            ino
        }
    }

    /// The body of [`Filesystem::lookup`], on whichever thread ends up serving it.
    /// `off_loop` says whether the caller is already on a worker, and so whether
    /// this may block. `lookup` answers a cached parent on the dispatch loop,
    /// where it may not.
    fn serve_lookup(&self, parent: u64, name: &str, reply: ReplyEntry, off_loop: bool) {
        if let Err(e) = self.core.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let hit = {
            let st = self.core.state();
            st.children.get(&parent).and_then(|kids| {
                kids.iter().copied().find_map(|ino| {
                    st.entries
                        .get(&ino)
                        .filter(|e| e.node.name == name)
                        .map(|e| {
                            // A `lookup` reply carries attrs with the same TTL a
                            // `getattr` would, so `ls -l` — which is one `lookup`
                            // per entry and no `getattr` at all — takes its sizes
                            // from here. Resolving only in `getattr` left the whole
                            // listing provisional (`docs/BUGS.md` B14).
                            let provisional = matches!(
                                &e.node.kind,
                                NodeKind::File {
                                    claimed_size: None,
                                    ..
                                }
                            )
                            .then(|| (e.parent, e.uid.clone()));
                            (ino, self.attr(ino, &e.node, e.access), provisional)
                        })
                })
            })
        };
        let Some((ino, attr, provisional)) = hit else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some((grandparent, uid)) = provisional else {
            reply.entry(&TTL, &attr, Generation(0));
            return;
        };
        // Resolving goes to the network. On the dispatch loop that is not
        // allowed (the B5 lesson), so hand off — but only from the loop: doing
        // it while already on a worker would queue a job onto the lane this
        // thread occupies.
        if !off_loop {
            let fs = self.clone();
            let name = name.to_string();
            self.core.workers.run(Lane::Meta, "lookup", move || {
                fs.serve_lookup(parent, &name, reply, true)
            });
            return;
        }
        let fs = self.clone();
        let respond = move || {
            // Re-read: on timeout or failure this is the provisional attr again,
            // which is no worse than what this path used to always send.
            let attr = {
                let st = fs.core.state();
                st.entries
                    .get(&ino)
                    .map_or(attr, |e| fs.attr(ino, &e.node, e.access))
            };
            reply.entry(&TTL, &attr, Generation(0));
        };
        match self.core.upgrade_sizes_for_parent(ino, &uid, grandparent) {
            Some(slot) => self.core.await_size(ino, slot, respond),
            None => respond(),
        }
    }

    /// The body of [`Filesystem::readdir`], on whichever thread ends up serving it.
    /// The body of [`Filesystem::opendir`]: enumerate the folder if it is cold,
    /// then publish one frozen listing under a fresh handle.
    ///
    /// A failure to enumerate is reported as a failed `opendir` rather than
    /// deferred to the first `readdir` — the caller gets the errno at the syscall
    /// that can still act on it.
    fn serve_opendir(&self, ino: u64, reply: ReplyOpen) {
        if let Err(e) = self.core.ensure_children(ino) {
            reply.error(e);
            return;
        }
        let mut st = self.core.state();
        let listing = st.listing(ino);
        // Shares the counter with write handles, so a directory handle can never
        // collide with a file handle (both are `fh` values in one kernel space).
        let fh = st.open.open_dir(listing);
        drop(st);
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    /// The live, un-snapshotted `readdir`. Reached only by a handle that has no
    /// snapshot — see [`Filesystem::readdir`].
    fn serve_readdir(&self, ino: u64, offset: u64, reply: ReplyDirectory) {
        if let Err(e) = self.core.ensure_children(ino) {
            reply.error(e);
            return;
        }
        let st = self.core.state();
        let listing = st.listing(ino);
        drop(st);
        reply_listing(&listing, offset, self.root, reply);
    }

    fn attr(&self, ino: u64, node: &Node, access: Access) -> FileAttr {
        let kind = match node.kind {
            NodeKind::Folder => FileType::Directory,
            NodeKind::File { .. } => FileType::RegularFile,
        };
        let perm = perm_bits(node.is_folder(), access);
        let size = node_size(node);
        let mtime = unix_secs(node.modification_time);
        let crtime = unix_secs(node.creation_time);
        FileAttr {
            ino: kernel_ino(self.root, ino),
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
            blksize: REPORTED_BLKSIZE,
            flags: 0,
        }
    }

    /// Trash the child `name` under `parent` on the remote, then drop it from the
    /// local cache. Backs both `unlink` and `rmdir` (Proton trashes whole
    /// subtrees, so an `rmdir` of a non-empty dir behaves the same).
    /// The body of [`Filesystem::open`], run off the dispatch loop for a write
    /// open.
    fn serve_open(&self, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        match self.files.open(ino.0, write) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(error) => reply.error(file_errno(error)),
        }
    }

    /// The body of [`Filesystem::fsync`], run off the dispatch loop.
    fn serve_fsync(&self, fh: FileHandle, reply: ReplyEmpty) {
        match self.files.fsync(fh.0) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(file_errno(error)),
        }
    }

    /// Stage `data` at `offset` of the write handle `fh` on `ino`.
    fn stage_write(&self, ino: u64, fh: u64, offset: u64, data: &[u8]) -> Result<(), Errno> {
        self.files.write(ino, fh, offset, data).map_err(file_errno)
    }

    /// The body of [`Filesystem::create`], run off the dispatch loop.
    fn serve_create(&self, parent: u64, name: &str, reply: ReplyCreate) {
        match self.namespace.create(parent, name) {
            Ok(made) => {
                let attr = self.attr(made.ino, &made.node, made.access);
                reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(made.fh),
                    FopenFlags::empty(),
                );
            }
            Err(error) => reply.error(namespace_errno(error)),
        }
    }

    /// The body of [`Filesystem::mkdir`], run off the dispatch loop.
    fn serve_mkdir(&self, parent: u64, name: &str, reply: ReplyEntry) {
        match self.namespace.mkdir(parent, name) {
            Ok(made) => {
                let attr = self.attr(made.ino, &made.node, made.access);
                reply.entry(&TTL, &attr, Generation(0));
            }
            Err(error) => reply.error(namespace_errno(error)),
        }
    }

    /// The body of [`Filesystem::unlink`], run off the dispatch loop.
    fn serve_unlink(&self, parent: u64, name: &str, reply: ReplyEmpty) {
        match self.namespace.unlink(parent, name) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(namespace_errno(error)),
        }
    }

    /// The body of [`Filesystem::rmdir`], run off the dispatch loop.
    fn serve_rmdir(&self, parent: u64, name: &str, reply: ReplyEmpty) {
        match self.namespace.rmdir(parent, name) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(namespace_errno(error)),
        }
    }

    /// The body of [`Filesystem::rename`], run off the dispatch loop.
    fn serve_rename(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let flags = pdfs_vfs::namespace::RenameFlags {
            no_replace: flags.contains(RenameFlags::RENAME_NOREPLACE),
            exchange: flags.contains(RenameFlags::RENAME_EXCHANGE),
        };
        match self
            .namespace
            .rename(parent, name, newparent, newname, flags)
        {
            Ok(Renamed::Done) => reply.ok(),
            Ok(Renamed::Queued) => {
                // Answer first: the kernel updates its cache while it
                // processes the answer, and an invalidation sent before it
                // would be overwritten, leaving an empty destination listing.
                reply.ok();
                if let Some(notifier) = self.core.session.notifier.get() {
                    let _ = notifier.inval_entry(parent, OsStr::new(name));
                    let _ = notifier.inval_entry(newparent, OsStr::new(newname));
                    let _ = notifier.inval_inode(parent);
                    if newparent != parent {
                        let _ = notifier.inval_inode(newparent);
                    }
                }
            }
            Err(error) => reply.error(namespace_errno(error)),
        }
    }

    /// Answer with an inode's current attributes, or `ENOENT` if it went away
    /// while the caller was doing something slower.
    fn reply_attr(&self, ino: u64, reply: ReplyAttr) {
        let st = self.core.state();
        match st.entries.get(&ino) {
            Some(e) => {
                let attr = self.attr(ino, &e.node, e.access);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
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
            let st = self.core.state();
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
