//! Kernel-facing FUSE callback adapter.

use super::*;

impl Filesystem for ProtonFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = parent.0;
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
        self.core.workers.run(Lane::Meta, move || {
            fs.serve_lookup(parent, &name, reply, true)
        });
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let mut st = self.core.state.lock();
        st.forget_lookup(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let (attr, provisional_in) = {
            let st = self.core.state.lock();
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
                    (self.attr(ino.0, &e.node), provisional)
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
        // fail. So resolve it before answering rather than after (bugs.md B14).
        //
        // The cost is one batched round trip for the whole folder, not one per
        // file: `ls -l` is one `getattr` per entry and they collapse onto a
        // single upgrade. This is why B12's split is still worth having — the
        // plain listing never comes here at all.
        //
        // Off the dispatch loop, because it goes to the network: blocking there
        // would stall every other operation on the mount (the B5 lesson).
        let fs = self.clone();
        self.core.workers.run(Lane::Meta, move || {
            fs.core.upgrade_sizes_for_parent(ino.0, &uid, parent);
            // Re-read: the upgrade writes through `state`, and on timeout or
            // failure this is simply the provisional attr we already had.
            let attr = {
                let st = fs.core.state.lock();
                match st.entries.get(&ino.0) {
                    Some(e) => fs.attr(ino.0, &e.node),
                    // Forgotten while we waited.
                    None => attr,
                }
            };
            reply.attr(&TTL, &attr);
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        let ino = ino.0;
        // Same split as `lookup`: only the cold, remote-enumerating path pays
        // for a worker.
        if self.core.children_cached(ino) {
            self.serve_readdir(ino, offset, reply);
            return;
        }
        let fs = self.clone();
        self.core
            .workers
            .run(Lane::Meta, move || fs.serve_readdir(ino, offset, reply));
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let (uid, base_mtime, base_size) = {
            let mut st = self.core.state.lock();
            match st.entries.get_mut(&ino.0) {
                Some(e) if e.node.is_file() => {
                    e.open_count = e.open_count.saturating_add(1);
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
        debug!(ino = ino.0, ?flags, base_size, base_mtime, "open");
        // Read-only opens stay stateless (fh 0). A write open allocates a
        // disk-backed handle; bytes are authored into a scratch file and the
        // untouched remainder is pulled from the base lazily (on read / commit).
        if flags.acc_mode() == OpenAccMode::O_RDONLY {
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }
        // A previous close may already have newer bytes queued locally while
        // the server still holds the older revision. That staged blob, not the
        // remote, is the base for this handle. In particular, `truncate file`
        // opens for writing and then shrinks via setattr; starting its scratch
        // as a sparse zero file would turn the preserved prefix into zeros.
        //
        // Complete pending blobs are the common case and can be copied without
        // any network access. An incomplete blob still has gaps referring to
        // the remote base; stacking another write on it cannot be represented
        // safely by WriteHandle, so fail the open rather than risk corruption.
        let pending_base = self.core.pending.lock().get(&uid).cloned();
        if pending_base.as_ref().is_some_and(|p| !p.meta.complete) {
            if let Some(entry) = self.core.state.lock().entries.get_mut(&ino.0) {
                entry.open_count = entry.open_count.saturating_sub(1);
            }
            error!(%uid, "refusing write over incomplete queued revision");
            reply.error(Errno::EIO);
            return;
        }
        let (file, path) = match self.core.cache.create_scratch() {
            Ok(x) => x,
            Err(e) => {
                if let Some(entry) = self.core.state.lock().entries.get_mut(&ino.0) {
                    entry.open_count = entry.open_count.saturating_sub(1);
                }
                error!(%uid, error = %e, "create scratch file failed");
                reply.error(Errno::EIO);
                return;
            }
        };
        let mut initial_written = Intervals::default();
        if let Some(pending) = &pending_base {
            if let Err(e) = std::fs::copy(&pending.path, &path) {
                if let Some(entry) = self.core.state.lock().entries.get_mut(&ino.0) {
                    entry.open_count = entry.open_count.saturating_sub(1);
                }
                let _ = std::fs::remove_file(&path);
                error!(%uid, source = %pending.path.display(), error = %e,
                    "copy queued revision into write scratch failed");
                reply.error(Errno::EIO);
                return;
            }
            initial_written.add(0, pending.meta.len);
        }
        let mut st = self.core.state.lock();
        let fh = st.next_fh;
        st.next_fh += 1;
        let aw = st.active_writes.entry(ino.0).or_insert_with(|| {
            WriteHandle {
                ino: ino.0,
                uid,
                file: Arc::new(file),
                path,
                written: initial_written,
                // Starts at the current size; reads in [0, base_size) come from
                // the base until overwritten.
                len: base_size,
                base_size,
                base_mtime,
                dirty: false,
                open_count: 0,
            }
        });
        aw.open_count += 1;
        st.handles.insert(fh, ino.0);
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
        //
        // This path stays on the dispatch loop. `write` runs there too, so
        // keeping its reads there as well preserves the ordering the write path
        // has always had between a write and a read of the same handle; a mostly
        // local read is not worth reasoning about that for.
        let handle = {
            let st = self.core.state.lock();
            st.active_writes.get(&ino.0).map(|h| {
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
        let (uid, mtime, fsize, is_video) = {
            let st = self.core.state.lock();
            match st.entries.get(&ino.0) {
                Some(e) if e.node.is_file() => {
                    let media_type = match &e.node.kind {
                        NodeKind::File { media_type, .. } => Some(media_type.as_str()),
                        NodeKind::Folder => None,
                    };
                    let is_video =
                        PhotoKind::classify(Some(&e.node.name), media_type) == PhotoKind::Video;
                    let fsize = self
                        .core
                        .pending
                        .lock()
                        .get(&e.uid)
                        .map(|p| p.meta.len)
                        .unwrap_or_else(|| node_size(&e.node));
                    (e.uid.clone(), e.node.modification_time, fsize, is_video)
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
        self.core.workers.run(Lane::Transfer, move || {
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
        let parent = parent.0;
        if let Err(e) = self.core.ensure_children(parent) {
            reply.error(e);
            return;
        }
        let name = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let parent_uid = {
            let st = self.core.state.lock();
            match st.entries.get(&parent) {
                Some(e) => e.uid.clone(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // Offline the server cannot mint a uid, so invent one and queue the
        // create. The file is real to the caller either way; only its identity is
        // provisional until the drain (offline.md Phase 3b).
        //
        // A parent that is itself still queued forces the same path even when we
        // are online: the API has no folder to put this in yet.
        let node = if self.core.online.load(Ordering::Relaxed) && !is_local_uid(&parent_uid) {
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
            match self.core.fetch_node(&new_uid) {
                Ok(n) => n,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        } else {
            match self.core.queue_local_node(&parent_uid, &name, false) {
                Ok(n) => n,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        };
        let new_uid = node.uid.clone();
        // The base this handle writes over is the node as it actually exists —
        // the empty file the server just minted — so its modification time comes
        // from the node, never from the local clock. `queue_revision` turns this
        // into `StagedWrite::based_on` and the drain compares that against the
        // remote: a `now_secs()` here differs from the server's stamp by however
        // long the create round-trip took, so the first write to a brand-new file
        // conflicts with its own create whenever the second happens to tick in
        // between.
        let base_mtime = node.modification_time;
        let (file, path) = match self.core.cache.create_scratch() {
            Ok(x) => x,
            Err(e) => {
                error!(%new_uid, error = %e, "create scratch file failed");
                reply.error(Errno::EIO);
                return;
            }
        };
        let mut st = self.core.state.lock();
        let ino = st.intern(parent, node);
        if let Some(entry) = st.entries.get_mut(&ino) {
            entry.open_count = entry.open_count.saturating_add(1);
        }
        if let Some(kids) = st.children.get_mut(&parent)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        let fh = st.next_fh;
        st.next_fh += 1;
        let aw = st.active_writes.entry(ino).or_insert_with(|| {
            // A brand-new file: empty base, everything written is authored.
            WriteHandle {
                ino,
                uid: new_uid,
                file: Arc::new(file),
                path,
                written: Intervals::default(),
                len: 0,
                base_size: 0,
                base_mtime,
                dirty: false,
                open_count: 0,
            }
        });
        aw.open_count += 1;
        st.handles.insert(fh, ino);
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
            let st = self.core.state.lock();
            match st.handles.get(&fh).and_then(|&i| st.active_writes.get(&i)) {
                Some(aw) => aw.file.clone(),
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
            let mut st = self.core.state.lock();
            let Some(aw) = st
                .handles
                .get(&fh)
                .copied()
                .and_then(|i| st.active_writes.get_mut(&i))
            else {
                reply.error(Errno::EBADF);
                return;
            };
            let end = offset + data.len() as u64;
            aw.written.add(offset, end);
            aw.len = aw.len.max(end);
            aw.dirty = true;
            let len = aw.len;
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
            let handled = {
                let mut st = self.core.state.lock();
                match st.active_writes.get_mut(&ino.0) {
                    Some(aw) => {
                        if size < aw.len {
                            // Shrink: drop authored ranges past the new end.
                            aw.written.clip(size);
                        } else if size > aw.len {
                            // Grow: the new tail is defined as zeros, so claim
                            // it as authored rather than base content.
                            aw.written.add(aw.len, size);
                        }
                        let _ = aw.file.set_len(size);
                        aw.len = size;
                        aw.dirty = true;
                        true
                    }
                    None => false,
                }
            };
            debug!(ino = ino.0, size, fh = ?fh, handled, "setattr resize");
            if !handled {
                // Path-based truncate with no open write handle — a shell's
                // `> file`. This is the second write path into the API, and
                // queueing it is what lets a redirect work offline at all: it
                // never reaches `release`, so without this it failed before any
                // byte was written (offline.md Phase 2/3b).
                if let Err(e) = self.core.queue_truncate(ino.0, size) {
                    reply.error(e);
                    return;
                }
            } else {
                self.core.state.lock().set_size(ino.0, size);
            }
        }
        let st = self.core.state.lock();
        match st.entries.get(&ino.0) {
            Some(e) => {
                let attr = self.attr(ino.0, &e.node);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
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
        let new_end = offset.saturating_add(length);
        let keep_size = (mode & 1) != 0; // FALLOC_FL_KEEP_SIZE

        let is_punch_hole = (mode & 0x02) != 0; // FALLOC_FL_PUNCH_HOLE

        let res = {
            let mut st = self.core.state.lock();
            match st
                .handles
                .get(&_fh.0)
                .copied()
                .and_then(|i| st.active_writes.get_mut(&i))
            {
                Some(aw) => {
                    use std::os::unix::io::AsRawFd;
                    let fd = aw.file.as_raw_fd();
                    let ret = unsafe { libc::fallocate(fd, mode, offset as i64, length as i64) };
                    if ret == 0 {
                        if is_punch_hole {
                            // A punched range reads as zero. Mark it authored so
                            // queue_revision does not refill it from the remote
                            // baseline and silently undo the hole at commit.
                            aw.written.add(offset, new_end.min(aw.len));
                            aw.dirty = true;
                        } else if !keep_size && new_end > aw.len {
                            aw.written.add(aw.len, new_end);
                            aw.len = new_end;
                            aw.dirty = true;
                            st.set_size(ino.0, new_end);
                        }
                        Ok(())
                    } else {
                        Err(Errno::EIO)
                    }
                }
                None => {
                    if st.entries.contains_key(&ino.0) {
                        Err(Errno::EBADF)
                    } else {
                        Err(Errno::ENOENT)
                    }
                }
            }
        };

        match res {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    /// `close(2)` calls flush before release. The upload is queued in `release`
    /// and performed in the background (offline.md Phase 3), so there is nothing
    /// to push here — the written bytes are already in the scratch file.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// Durability here means the scratch file, not the remote: a caller asking
    /// for fsync wants its bytes to survive a crash, and blocking it on an upload
    /// (which offline would never finish) buys nothing the queue does not already
    /// guarantee.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // Everything the durability sidecar needs, copied out so no lock is held
        // across the sync.
        let handle = {
            let st = self.core.state.lock();
            let ino = st.handles.get(&fh.0).copied();
            ino.and_then(|i| st.active_writes.get(&i)).map(|aw| {
                (
                    aw.file.clone(),
                    aw.path.clone(),
                    aw.uid.clone(),
                    aw.dirty,
                    aw.written.clone(),
                    aw.len,
                    aw.base_size,
                    aw.base_mtime,
                )
            })
        };
        let Some((f, path, uid, dirty, written, len, base_size, base_mtime)) = handle else {
            reply.error(Errno::EBADF);
            return;
        };
        if let Err(e) = f.sync_all() {
            error!(fh = fh.0, error = %e, "fsync of scratch file failed");
            reply.error(Errno::EIO);
            return;
        }
        // A clean handle has nothing on it that the remote does not already
        // hold, so there is nothing for a crash to lose.
        if !dirty {
            reply.ok();
            return;
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
            complete: authored == [(0, len)],
            authored,
            based_on: self.core.remote_baseline(&uid, base_mtime, base_size),
        };
        // A failed sidecar means the write is not durable, and `fsync` promising
        // otherwise is the defect this exists to fix — so it is an error, not a
        // warning.
        if let Err(e) = self.core.cache.mark_scratch_durable(&path, &meta) {
            error!(%uid, error = %e, "recording fsync durability failed");
            reply.error(Errno::EIO);
            return;
        }
        reply.ok();
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
        let (handle, unlinked_uid) = {
            let mut st = self.core.state.lock();
            let unlinked_uid = if let Some(entry) = st.entries.get_mut(&_ino.0) {
                entry.open_count = entry.open_count.saturating_sub(1);
                if entry.open_count == 0 && entry.unlinked {
                    let uid = entry.uid.clone();
                    st.forget(&uid);
                    Some(uid)
                } else {
                    None
                }
            } else {
                None
            };
            let ino = st.handles.remove(&fh.0);
            let h = ino.and_then(|i| {
                let aw = st.active_writes.get_mut(&i)?;
                aw.open_count = aw.open_count.saturating_sub(1);
                if aw.open_count == 0 {
                    st.active_writes.remove(&i)
                } else {
                    None
                }
            });
            (h, unlinked_uid)
        };
        let was_unlinked = unlinked_uid.is_some();
        if let Some(uid) = unlinked_uid {
            self.core.discard_queued_ops(&uid);
            self.core.cache.evict(&uid);
            self.core.evict_reader(&uid);
        }
        // Hand the bytes to the queue rather than uploading them here: the
        // scratch file is the only copy of what was just written, and blocking
        // the caller on the network is what made a copy into the mount run at
        // upload speed (and fail outright offline).
        match (handle, was_unlinked) {
            (Some(h), true) => {
                // POSIX keeps an unlinked file usable until the last close, but
                // closing it must not resurrect those bytes as a new revision.
                if let Err(e) = std::fs::remove_file(&h.path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(path = %h.path.display(), error = %e, "discarding unlinked scratch file failed");
                }
                reply.ok();
            }
            (Some(h), false) => match self.core.queue_revision(&h) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            },
            (None, _) => reply.ok(),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        if let Ok((ino, _)) = self.core.lookup_child(parent.0, &name_str) {
            let st = self.core.state.lock();
            if let Some(entry) = st.entries.get(&ino)
                && entry.node.is_folder()
            {
                reply.error(Errno::EISDIR);
                return;
            }
        }
        self.trash_child(parent.0, &name_str, reply);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        if let Ok((ino, _)) = self.core.lookup_child(parent.0, &name_str) {
            {
                let st = self.core.state.lock();
                if st
                    .entries
                    .get(&ino)
                    .is_some_and(|entry| !entry.node.is_folder())
                {
                    reply.error(Errno::ENOTDIR);
                    return;
                }
            }
            // A missing cached listing says "unknown", not "empty". Proton's
            // trash call is recursive, so enumerate before applying rmdir's
            // POSIX emptiness rule.
            if let Err(e) = self.core.ensure_children(ino) {
                reply.error(e);
                return;
            }
            if self.core.state.lock().has_children(ino) {
                reply.error(Errno::ENOTEMPTY);
                return;
            }
        }
        self.trash_child(parent.0, &name_str, reply);
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
        let name = match fuse_name(name) {
            Ok(name) => name,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let parent_uid = {
            let st = self.core.state.lock();
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
        // As in `create`: offline — or under a parent that is itself still
        // queued — the folder becomes a placeholder that the drain turns into a
        // real one (offline.md Phase 3b).
        let node = if self.core.online.load(Ordering::Relaxed) && !is_local_uid(&parent_uid) {
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
            match self.core.fetch_node(&new_uid) {
                Ok(n) => n,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        } else {
            match self.core.queue_local_node(&parent_uid, &name, true) {
                Ok(n) => n,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        };
        let mut st = self.core.state.lock();
        let local = is_local_uid(&node.uid);
        let uid = node.uid.clone();
        let ino = st.intern(parent, node);
        if let Some(kids) = st.children.get_mut(&parent)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        // A folder that exists only here has nothing to enumerate, and asking the
        // API to enumerate a `local~` uid would 404. Record it as fully listed and
        // empty, which it is, so reads stay offline-clean across a restart too.
        if local {
            st.children.insert(ino, Vec::new());
            if let Err(e) = self.core.db.set_listed(&uid, true) {
                warn!(%uid, error = %e, "db set_listed(true) failed for local folder");
            }
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
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent = parent.0;
        let newparent = newparent.0;
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
        let (ino, uid) = match self.core.lookup_child(parent, &name) {
            Ok(x) => x,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        // Ancestor cycle check: moving a directory into itself or one of its descendants is forbidden by POSIX (EINVAL).
        {
            let st = self.core.state.lock();
            if let Some(entry) = st.entries.get(&ino)
                && entry.node.is_folder()
                && st.is_ancestor_of(ino, newparent)
            {
                reply.error(Errno::EINVAL);
                return;
            }
        }
        // The destination has to be listed either way: the queued path pushes
        // the node into that listing, and the online one drops it to force a
        // re-enumeration.
        if newparent != parent
            && let Err(e) = self.core.ensure_children(newparent)
        {
            reply.error(e);
            return;
        }
        let new_parent_uid = {
            let st = self.core.state.lock();
            match st.entries.get(&newparent) {
                Some(e) => e.uid.clone(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        // `rename(2)` replaces an existing destination; Proton has no replacing
        // rename, so the victim has to be removed first and the operation stops
        // being atomic. Without this the 422 surfaced as a blanket EIO and every
        // write-to-temp-then-rename tool — rsync, atomic editor saves — failed
        // at the very end of its transfer (bugs.md B13).
        //
        // `RENAME_EXCHANGE` has no Proton primitive and cannot be emulated
        // without a window in which one of the two names does not exist.
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            reply.error(Errno::EINVAL);
            return;
        }
        let victim = match self.core.lookup_child(newparent, &newname) {
            // Renaming a node onto its own name is a no-op, never a self-replace.
            Ok((_, vuid)) if vuid == uid => None,
            Ok(x) => Some(x),
            // Nothing to replace: the overwhelmingly common case.
            Err(e) if e.code() == libc::ENOENT => None,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Some((victim_ino, victim_uid)) = &victim {
            if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                reply.error(Errno::EEXIST);
                return;
            }
            // POSIX requires both ends to agree on being a directory, and the
            // check has to happen before anything is trashed: getting it wrong
            // turns a refusal into the destruction of the destination.
            let (src_dir, dst_dir) = {
                let st = self.core.state.lock();
                let is_dir = |i: &u64| {
                    st.entries
                        .get(i)
                        .map(|e| matches!(e.node.kind, NodeKind::Folder))
                };
                match (is_dir(&ino), is_dir(victim_ino)) {
                    (Some(a), Some(b)) => (a, b),
                    _ => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                }
            };
            // Replacing a directory means trashing it, and Proton trashes a
            // folder with its whole subtree — so the destination's emptiness has
            // to be known before the decision is made.
            let dst_empty = if dst_dir {
                if let Err(e) = self.core.ensure_children(*victim_ino) {
                    reply.error(e);
                    return;
                }
                self.core
                    .state
                    .lock()
                    .children
                    .get(victim_ino)
                    .is_none_or(|kids| kids.is_empty())
            } else {
                true
            };
            if let Err(e) = check_replaceable(src_dir, dst_dir, dst_empty) {
                reply.error(e);
                return;
            }
            if let Err(e) = self.core.remove_replaced(victim_uid, &newname) {
                reply.error(e);
                return;
            }
        }
        // A node whose own creation is still queued has no server-side identity
        // to rename: the queued op *is* the node, so rewriting its target is the
        // whole rename. Nothing reaches the API, which is why this works offline
        // (offline.md Phase 3b).
        if is_local_uid(&uid) {
            match self.core.db.rewrite_op_target(
                &uid.to_string(),
                &new_parent_uid.to_string(),
                &newname,
            ) {
                Ok(true) => {
                    self.core.state.lock().rename_in_place(
                        ino,
                        newparent,
                        &new_parent_uid,
                        &newname,
                    );
                    debug!(%uid, newname, "renamed a node whose create is still queued");
                    reply.ok();
                }
                // The create drained underneath us, so the node has a real uid
                // now and this handle's is stale. A retry resolves it.
                Ok(false) => {
                    warn!(%uid, name, newname, "queued create vanished under a rename");
                    self.core.restore_replaced(victim.as_ref(), &newname);
                    reply.error(Errno::EBUSY);
                }
                Err(e) => {
                    error!(%uid, error = %e, "rewriting a queued create's target failed");
                    self.core.restore_replaced(victim.as_ref(), &newname);
                    reply.error(Errno::EIO);
                }
            }
            return;
        }
        // Offline, or into a folder that does not exist remotely yet: queue the
        // rename rather than 404 or fail. Online moves into a real parent still
        // take the synchronous path, so a genuine API refusal (permissions, a
        // name clash) surfaces to the caller instead of becoming a queued op
        // that can only ever conflict.
        if rename_needs_queue(
            self.core.online.load(Ordering::Relaxed),
            is_local_uid(&new_parent_uid),
            newparent != parent,
            newname != name,
        ) {
            match self
                .core
                .queue_rename(ino, &uid, newparent, &new_parent_uid, &newname)
            {
                Ok(()) => {
                    // Send the rename response first. Kernel-side rename cache
                    // updates happen while processing that response; notifying
                    // before it lets those updates overwrite our invalidation
                    // and retain an empty destination readdir page.
                    reply.ok();
                    if let Some(notifier) = self.core.notifier.get() {
                        let _ = notifier.inval_entry(INodeNo(parent), OsStr::new(&name));
                        let _ = notifier.inval_entry(INodeNo(newparent), OsStr::new(&newname));
                        let _ = notifier.inval_inode(INodeNo(parent), 0, 0);
                        if newparent != parent {
                            let _ = notifier.inval_inode(INodeNo(newparent), 0, 0);
                        }
                    }
                }
                Err(e) => {
                    self.core.restore_replaced(victim.as_ref(), &newname);
                    reply.error(e);
                }
            }
            return;
        }
        // Rename first if both halves change. Moving first makes the encrypted
        // name requirements stale and repeatedly failed with InvalidRequirements.
        // A failure past this point has already trashed any node the rename was
        // replacing, so put it back rather than leaving the caller with neither
        // the source moved nor the destination intact.
        if newname != name {
            if let Err(e) = self
                .core
                .rt
                .block_on(self.core.client.rename_node(&uid, &newname, None))
            {
                error!(%uid, error = %e, "rename failed");
                self.core.restore_replaced(victim.as_ref(), &newname);
                reply.error(Errno::EIO);
                return;
            }
        }
        if newparent != parent {
            let mut attempts = 0u32;
            let moved = loop {
                match self
                    .core
                    .rt
                    .block_on(self.core.client.move_node(&uid, &new_parent_uid))
                {
                    Ok(()) => break Ok(()),
                    Err(e)
                        if newname != name
                            && api_code(&e) == Some(ResponseCode::InvalidRequirements)
                            && attempts < 20 =>
                    {
                        attempts += 1;
                        // Requirement propagation routinely takes longer than
                        // three seconds. Keep a bounded ten-second window; a
                        // successful POSIX reply must mean both remote halves
                        // landed, not merely that the second half was queued.
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Err(e) => break Err(e),
                }
            };
            if let Err(e) = moved {
                error!(%uid, attempts, error = %e, "move after rename failed");
                // The rename half landed in the source directory.
                let mut state = self.core.state.lock();
                let old_parent_uid = state
                    .entries
                    .get(&parent)
                    .map(|entry| entry.uid.clone())
                    .unwrap_or_else(|| uid.clone());
                state.rename_in_place(ino, parent, &old_parent_uid, &newname);
                drop(state);
                self.core.restore_replaced(victim.as_ref(), &newname);
                reply.error(Errno::EIO);
                return;
            }
        }
        self.core
            .state
            .lock()
            .relocate(ino, parent, newparent, &new_parent_uid, &newname);
        reply.ok();
    }

    /// Expose a file's server-side thumbnail/preview as an extended attribute, so
    /// a previewing client can fetch it without downloading the whole file. The
    /// bytes are fetched on demand and cached; absence yields `ENODATA`.
    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        // Always off the dispatch loop: a miss goes to the wire, and a lister
        // that stats a directory issues one of these per file per advertised
        // name — inline, that serialized the whole mount behind ~186 ms of
        // network per call (B5).
        let fs = self.clone();
        let name = name.to_os_string();
        self.core.workers.run(Lane::Meta, move || {
            fs.serve_getxattr(ino, &name, size, reply)
        });
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
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
}

/// The Proton Drive VFS. FUSE callbacks are synchronous, so the Tokio handle
/// bridges each one to the async SDK via `tokio::runtime::Handle::block_on`; the fuser
/// session thread is not a runtime worker, so blocking on it is sound.
/// Cloneable so a handler can move a copy onto a `Workers` thread and answer
/// from there; every field is a handle or a plain id.
#[derive(Clone)]
pub struct ProtonFs {
    core: Core,
    uid: u32,
    gid: u32,
}

impl ProtonFs {
    /// Build the filesystem rooted at `root` (the user's My Files folder).
    pub(super) fn new(core: Core, root: Node) -> Self {
        {
            let mut st = core.state.lock();
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
                    lookup_count: 1,
                    open_count: 0,
                    unlinked: false,
                },
            );
        }
        // SAFETY: geteuid/getegid are infallible and have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self { core, uid, gid }
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
            let st = self.core.state.lock();
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
                            // listing provisional (bugs.md B14).
                            let provisional = matches!(
                                &e.node.kind,
                                NodeKind::File {
                                    claimed_size: None,
                                    ..
                                }
                            )
                            .then(|| (e.parent, e.uid.clone()));
                            (ino, self.attr(ino, &e.node), provisional)
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
            self.core.workers.run(Lane::Meta, move || {
                fs.serve_lookup(parent, &name, reply, true)
            });
            return;
        }
        self.core.upgrade_sizes_for_parent(ino, &uid, grandparent);
        // Re-read: on timeout or failure this is the provisional attr again,
        // which is no worse than what this path used to always send.
        let attr = {
            let st = self.core.state.lock();
            st.entries
                .get(&ino)
                .map_or(attr, |e| self.attr(ino, &e.node))
        };
        reply.entry(&TTL, &attr, Generation(0));
    }

    /// The body of [`Filesystem::readdir`], on whichever thread ends up serving it.
    fn serve_readdir(&self, ino: u64, offset: u64, mut reply: ReplyDirectory) {
        if let Err(e) = self.core.ensure_children(ino) {
            reply.error(e);
            return;
        }
        let st = self.core.state.lock();
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
        let (ino, uid) = match self.core.lookup_child(parent, name) {
            Ok(x) => x,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let open_now = self
            .core
            .state
            .lock()
            .entries
            .get(&ino)
            .is_some_and(|e| e.open_count > 0);
        // A node the server has never heard of cannot be trashed there; deleting
        // it just means its queued creation is no longer wanted. This works
        // offline, which the remote path below cannot (offline.md Phase 3b).
        if is_local_uid(&uid) {
            if !open_now {
                self.core.discard_queued_ops(&uid);
            }
            self.core.state.lock().forget_or_unlink(&uid);
            debug!(%uid, name, "deleted a node that had not been created remotely yet");
            reply.ok();
            return;
        }
        // Offline: queue it. Trashing is the one mutation a user expects to work
        // regardless — the file is gone from their point of view the moment the
        // command returns (offline.md Phase 3b).
        if !self.core.online.load(Ordering::Relaxed) {
            match self.core.queue_trash(&uid, name) {
                Ok(()) => {
                    self.core.state.lock().forget_or_unlink(&uid);
                    reply.ok();
                }
                Err(e) => reply.error(e),
            }
            return;
        }
        if let Err(e) = self
            .core
            .rt
            .block_on(self.core.client.trash_nodes(std::slice::from_ref(&uid)))
        {
            error!(%uid, error = %e, "trash failed");
            self.core
                .log_activity(ActivityKind::Trash, name, e.to_string(), false);
            reply.error(Errno::EIO);
            return;
        }
        // Only remove the namespace entry after Drive accepted the mutation.
        // Otherwise an EIO response still made the path disappear locally.
        self.core.hidden.lock().insert(uid.clone());
        self.core.state.lock().forget_or_unlink(&uid);
        if !open_now {
            self.core.discard_queued_ops(&uid);
            self.core.cache.evict(&uid);
            self.core.evict_reader(&uid);
        }
        self.core.invalidate_trash();
        // Every other trash site records itself; this one did not, which made a
        // file found in the trash impossible to attribute after the fact — the
        // activity log was the only record and it showed nothing (bugs.md B2).
        self.core
            .log_activity(ActivityKind::Trash, name, "trashed from the mount", true);
        reply.ok();
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
            let st = self.core.state.lock();
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
