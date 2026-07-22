//! Revision-reader cache and streamed range-read subsystem.

use super::*;

/// A read of an unpinned video at least this large streams *without* persisting
/// its blocks to the on-disk cache. Playing a 2 GB film would otherwise pour it
/// through the block LRU and evict everything else the user actually wants kept —
/// and it re-streams cheaply enough on a rewatch that keeping it was never worth
/// that. Pinned videos (kept offline on purpose) and anything smaller cache as
/// usual.
pub(super) const STREAM_BYPASS_MIN: u64 = 256 * 1024 * 1024;

/// How many bytes of streamed blocks the in-memory ring keeps. Bypassing the
/// on-disk cache must not mean re-fetching: the kernel asks for a streamed file
/// in reads far smaller than [`BLOCK_SIZE`], so without a ring every 128 KiB the
/// player consumes would download and decrypt a whole 4 MiB block again. Sized
/// for a handful of blocks per concurrently-streamed file.
const STREAM_RING_BYTES: u64 = 128 * 1024 * 1024;

/// Blocks fetched *past* the one a sequential streaming read needed, warmed into
/// the ring in the background so the player's next read is already in memory
/// instead of waiting a round-trip. Kept small so it cannot crowd out the
/// in-flight block governor.
const STREAM_READAHEAD: u64 = 4;

/// How many [`RevisionReader`]s stay open at once.
///
/// A reader holds its revision's content key and block table — a few KB even for
/// a large file, so this is bounded for tidiness and staleness rather than
/// memory. Evicted least-recently-used; a dropped reader costs one re-open
/// (two API calls and a node-key unlock) the next time that file is read.
const MAX_OPEN_READERS: usize = 64;

/// An open reader plus the node metadata it was opened against.
///
/// The SDK pins a reader to the revision that was active at `open_revision`, so
/// a reader is only reusable while the node still reports the same
/// `(mtime, size)` — the same validity pair the content cache uses (a new
/// revision bumps mtime). On a mismatch the reader is dropped and reopened.
pub(super) struct CachedReader {
    reader: Arc<RevisionReader>,
    mtime: i64,
    size: u64,
    /// For LRU eviction.
    last_used: Instant,
}

/// What the `readers` map holds for a node: an open reader, or the fact that
/// someone is already opening one.
///
/// The `Pending` arm is what makes the map single-flight rather than merely a
/// cache. Opening resolves link details, ancestor keys, an S2K node-key unlock
/// and the block table — five round-trips — and read-ahead on a cold file puts
/// several workers into the open at the same moment, so without this each
/// racer would redo all of it and discard every result but one.
pub(super) enum ReaderSlot {
    Ready(CachedReader),
    Pending(PendingOpen),
}

/// An `open_revision` in flight. Racers clone `rx` and await the leader's result
/// instead of opening their own.
pub(super) struct PendingOpen {
    /// Identifies this attempt so the leader can tell "my slot is still there"
    /// from "someone evicted or replaced it while I was on the network", and
    /// only publish its reader in the first case.
    id: u64,
    /// What the leader is opening against. A racer wanting a different revision
    /// must not join this open — it would get a reader for the wrong bytes.
    mtime: i64,
    size: u64,
    rx: tokio::sync::watch::Receiver<Option<Result<Arc<RevisionReader>, Errno>>>,
}

/// Distinguishes `PendingOpen`s. Only uniqueness matters, so a plain counter
/// does; ids are never compared for order.
static NEXT_OPEN_ID: AtomicU64 = AtomicU64::new(0);

/// Drive client, a Tokio handle to bridge the synchronous FUSE/socket threads
/// to the async SDK, the inode bookkeeping, and the on-disk content cache.
///
/// Cheaply cloneable (every field is a handle/`Arc`), so the control-socket task
/// gets its own copy while the FUSE session keeps another.
/// In-memory LRU of blocks belonging to a file streaming past the on-disk cache
/// (see [`STREAM_BYPASS_MIN`]). Bypassing the disk stops one film evicting the
/// block LRU; it must not also mean the same 4 MiB block is downloaded once per
/// 128 KiB the player reads out of it.
///
/// Validated by the same `(mtime, size)` pair the on-disk caches use: a node
/// whose tag no longer matches has its blocks dropped rather than served, so a
/// new revision can never be stitched together from the old one.
#[derive(Default)]
pub(super) struct StreamRing {
    blocks: HashMap<(NodeUid, u64), Arc<Vec<u8>>>,
    /// Keys oldest-first, for eviction.
    order: VecDeque<(NodeUid, u64)>,
    /// Per-node validity tag, `(mtime, size)`.
    tags: HashMap<NodeUid, (i64, u64)>,
    bytes: u64,
}

impl StreamRing {
    fn get(&mut self, uid: &NodeUid, mtime: i64, size: u64, idx: u64) -> Option<Arc<Vec<u8>>> {
        if self.tags.get(uid) != Some(&(mtime, size)) {
            return None;
        }
        self.blocks.get(&(uid.clone(), idx)).cloned()
    }

    fn insert(&mut self, uid: &NodeUid, mtime: i64, size: u64, idx: u64, bytes: Arc<Vec<u8>>) {
        if self.tags.get(uid) != Some(&(mtime, size)) {
            // Revision changed under us (or first sight): anything held for this
            // node describes the old one.
            self.drop_node(uid);
            self.tags.insert(uid.clone(), (mtime, size));
        }
        let key = (uid.clone(), idx);
        if self.blocks.contains_key(&key) {
            return;
        }
        self.bytes += bytes.len() as u64;
        self.blocks.insert(key.clone(), bytes);
        self.order.push_back(key);
        while self.bytes > STREAM_RING_BYTES {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.blocks.remove(&victim) {
                self.bytes -= dropped.len() as u64;
            }
        }
    }

    fn drop_node(&mut self, uid: &NodeUid) {
        self.order.retain(|(u, _)| u != uid);
        let mut freed = 0u64;
        self.blocks.retain(|(u, _), bytes| {
            if u == uid {
                freed += bytes.len() as u64;
                false
            } else {
                true
            }
        });
        self.bytes -= freed;
        self.tags.remove(uid);
    }
}

impl Core {
    /// An open [`RevisionReader`] for `uid`, reusing the cached one when it is
    /// still valid for `(mtime, fsize)` and opening a fresh one otherwise.
    ///
    /// Opening resolves the file's link details, ancestor keys, node key (an S2K
    /// unlock) and block table. Doing that once per file rather than once per
    /// block is the whole point: a cold 100 MB read is 25 block misses, which
    /// used to mean 25 full resolutions (50 API calls, 25 unlocks) and now means
    /// one.
    ///
    /// Racing reads of one cold file open it **once**: the first caller claims
    /// the slot and the rest await its result. That matters now that `read` is
    /// served off the dispatch loop, because read-ahead on a first touch issues
    /// several parallel reads of the same file as a matter of course.
    async fn revision_reader(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
    ) -> Result<Arc<RevisionReader>, Errno> {
        loop {
            // Decide under the lock, act outside it — network I/O must never run
            // under a std `Mutex`, and an await must never hold one.
            enum Act {
                Hit(Arc<RevisionReader>),
                Join(tokio::sync::watch::Receiver<Option<Result<Arc<RevisionReader>, Errno>>>),
                Lead(
                    u64,
                    tokio::sync::watch::Sender<Option<Result<Arc<RevisionReader>, Errno>>>,
                ),
            }
            let act = {
                let mut readers = self.readers.lock();
                match readers.get_mut(uid) {
                    Some(ReaderSlot::Ready(entry))
                        if entry.mtime == mtime && entry.size == fsize =>
                    {
                        entry.last_used = Instant::now();
                        Act::Hit(entry.reader.clone())
                    }
                    Some(ReaderSlot::Pending(p)) if p.mtime == mtime && p.size == fsize => {
                        Act::Join(p.rx.clone())
                    }
                    // Either nothing here, or something for a revision we no
                    // longer want (a stale reader, or an open for one). Ours
                    // replaces it: the newer `(mtime, size)` is the truth.
                    _ => {
                        let id = NEXT_OPEN_ID.fetch_add(1, Ordering::Relaxed);
                        let (tx, rx) = tokio::sync::watch::channel(None);
                        readers.insert(
                            uid.clone(),
                            ReaderSlot::Pending(PendingOpen {
                                id,
                                mtime,
                                size: fsize,
                                rx,
                            }),
                        );
                        Act::Lead(id, tx)
                    }
                }
            };

            match act {
                Act::Hit(reader) => return Ok(reader),
                Act::Join(mut rx) => {
                    // `changed()` errors only if the leader dropped its sender
                    // without publishing — it panicked. Retry from the top
                    // rather than fail: the slot it left is gone, so this pass
                    // leads its own open.
                    loop {
                        if let Some(result) = rx.borrow_and_update().clone() {
                            return result;
                        }
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                Act::Lead(id, tx) => {
                    debug!(%uid, mtime, fsize, "opening revision");
                    let result = match self.client.open_revision(uid).await {
                        Ok(reader) => Ok(Arc::new(reader)),
                        Err(e) => {
                            warn!(%uid, error = %e, "open_revision failed");
                            Err(Errno::EIO)
                        }
                    };
                    {
                        let mut readers = self.readers.lock();
                        // Publish only into the slot we claimed. If it is gone
                        // or belongs to a later attempt, an eviction or a newer
                        // revision overtook us: our reader still answers the
                        // callers waiting on it, but it must not be cached.
                        let ours = matches!(
                            readers.get(uid),
                            Some(ReaderSlot::Pending(p)) if p.id == id
                        );
                        if ours {
                            match &result {
                                Ok(reader) => {
                                    readers.insert(
                                        uid.clone(),
                                        ReaderSlot::Ready(CachedReader {
                                            reader: reader.clone(),
                                            mtime,
                                            size: fsize,
                                            last_used: Instant::now(),
                                        }),
                                    );
                                }
                                // Never cache a failure: the next read retries.
                                Err(_) => {
                                    readers.remove(uid);
                                }
                            }
                        }
                        Self::trim_readers(&mut readers);
                    }
                    // After the map is updated, so a racer that wakes and
                    // re-enters finds the finished slot rather than racing us.
                    let _ = tx.send(Some(result.clone()));
                    return result;
                }
            }
        }
    }

    /// Bound the reader map: drop least-recently-used entries until it is back
    /// at [`MAX_OPEN_READERS`]. Only `Ready` slots are eviction candidates — a
    /// `Pending` has callers waiting on it and no reader to drop yet.
    fn trim_readers(readers: &mut HashMap<NodeUid, ReaderSlot>) {
        while readers.len() > MAX_OPEN_READERS {
            let victim = readers
                .iter()
                .filter_map(|(uid, slot)| match slot {
                    ReaderSlot::Ready(entry) => Some((uid, entry.last_used)),
                    ReaderSlot::Pending(_) => None,
                })
                .min_by_key(|(_, last_used)| *last_used)
                .map(|(uid, _)| uid.clone());
            let Some(victim) = victim else {
                break; // every slot is an open in flight; nothing to drop
            };
            readers.remove(&victim);
        }
    }

    /// Drop any open reader for `uid`, so the next read reopens against the
    /// current revision. Called wherever cached content is evicted.
    ///
    /// Drops an in-flight open too: its leader finds the slot gone and declines
    /// to cache what it opened, which is what we want — the eviction says that
    /// revision is no longer the truth.
    pub(super) fn evict_reader(&self, uid: &NodeUid) {
        self.readers.lock().remove(uid);
    }

    /// Serve bytes `[offset, offset + len)` of `uid`'s active revision, hitting
    /// the on-disk caches before the network: a whole-file blob (pinned files)
    /// first, then the block cache — fetching only the [`BLOCK_SIZE`]-aligned
    /// blocks that overlap the request and caching each. `mtime`/`fsize` validate
    /// both caches. Network I/O runs without any lock held.
    pub(super) fn read_range(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        offset: u64,
        len: u64,
        cache_blocks: bool,
    ) -> Result<Vec<u8>, Errno> {
        // A queued write has not reached the remote yet, so the remote's current
        // revision is stale and the staged blob is the truth. Serve from it until
        // the drain worker lands the upload (offline.md Phase 3).
        if let Some(pending) = self.pending.lock().get(uid).cloned() {
            return self.read_pending(&pending, offset, len);
        }
        // A node created offline and never written has no blob and no remote: it
        // is an empty file, and asking the API about a `local~` uid would only
        // earn a 404 (offline.md Phase 3b).
        if is_local_uid(uid) {
            return Ok(Vec::new());
        }
        self.read_range_remote(uid, mtime, fsize, offset, len, cache_blocks)
    }

    /// Serve a read from a staged blob, falling back to the remote base for any
    /// range the write did not author (an incomplete [`StagedWrite`] holds zeros
    /// there, which must never be handed out as content).
    fn read_pending(
        &self,
        pending: &PendingRevision,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, Errno> {
        let m = &pending.meta;
        if offset >= m.len || len == 0 {
            return Ok(Vec::new());
        }
        let uid = parse_node_uid(&m.uid).ok_or(Errno::EIO)?;
        let file = File::open(&pending.path).map_err(|e| {
            error!(%uid, path = %pending.path.display(), error = %e, "open staged blob failed");
            Errno::EIO
        })?;
        let mut written = Intervals::default();
        for &(s, e) in &m.authored {
            written.add(s, e);
        }
        // Same merge as `serve_open_read`, but resolving gaps against the remote
        // rather than through `read_range` — going through `read_range` would find
        // this very pending op and recurse.
        let end = offset.saturating_add(len).min(m.len);
        let mut out = Vec::with_capacity((end - offset) as usize);
        for (s, e, authored) in written.segments(offset, end) {
            if authored {
                let mut buf = vec![0u8; (e - s) as usize];
                file.read_exact_at(&mut buf, s).map_err(|err| {
                    warn!(%uid, error = %err, "staged blob read failed");
                    Errno::EIO
                })?;
                out.extend_from_slice(&buf);
                continue;
            }
            let bend = e.min(m.base_size);
            if s < bend {
                out.extend_from_slice(&self.read_range_remote(
                    &uid,
                    m.base_mtime,
                    m.base_size,
                    s,
                    bend - s,
                    true,
                )?);
            }
            // Past the base: a hole the write extended over.
            out.resize(out.len() + e.saturating_sub(s.max(m.base_size)) as usize, 0);
        }
        Ok(out)
    }

    /// Warm the [`STREAM_READAHEAD`] blocks after `last` into the ring, in the
    /// background, so a player reading forward finds its next block already in
    /// memory rather than paying a fetch + decrypt round-trip at each boundary.
    ///
    /// Fire-and-forget: a failure here is not an error, it only means the read
    /// that needs those bytes will fetch them itself.
    fn stream_readahead(&self, uid: &NodeUid, mtime: i64, fsize: u64, last: u64) {
        let last_block = (fsize.saturating_sub(1)) / BLOCK_SIZE;
        let wanted: Vec<u64> = ((last + 1)..=(last + STREAM_READAHEAD).min(last_block))
            .filter(|&bidx| {
                self.stream_ring
                    .lock()
                    .get(uid, mtime, fsize, bidx)
                    .is_none()
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        let core = self.clone();
        let uid = uid.clone();
        self.rt.spawn(async move {
            let Ok(reader) = core.revision_reader(&uid, mtime, fsize).await else {
                return;
            };
            // Concurrently, for the same reason the demand path fetches its
            // misses concurrently: serially awaited blocks cost the sum of their
            // round-trips, and the player catches up with the readahead before it
            // finishes. The SDK's in-flight block governor bounds the fan-out.
            let mut set = tokio::task::JoinSet::new();
            for bidx in wanted {
                let reader = reader.clone();
                let bstart = bidx * BLOCK_SIZE;
                let blen = BLOCK_SIZE.min(fsize - bstart);
                set.spawn(async move { (bidx, reader.read_at(bstart, blen).await) });
            }
            while let Some(joined) = set.join_next().await {
                let Ok((bidx, read)) = joined else { continue };
                match read {
                    Ok(bytes) => {
                        core.stream_ring
                            .lock()
                            .insert(&uid, mtime, fsize, bidx, Arc::new(bytes))
                    }
                    Err(e) => debug!(%uid, bidx, error = %e, "stream readahead failed"),
                }
            }
        });
    }

    /// Read from the content cache, else the remote. The base-content path, with
    /// no awareness of queued writes — callers wanting the file's *current*
    /// content want [`Core::read_range`]. Gap-filling a staged blob is the one
    /// caller that genuinely means "the base", since that is what its zeroed
    /// ranges have to be filled from.
    fn read_range_remote(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        offset: u64,
        len: u64,
        cache_blocks: bool,
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
        let mut blocks: Vec<Option<Arc<Vec<u8>>>> = Vec::with_capacity((last - first + 1) as usize);
        let mut misses: Vec<u64> = Vec::new();
        for bidx in first..=last {
            // A streaming read keeps its blocks only in memory, so the ring is
            // the only cache it has — check it before the disk (which will not
            // hold them) and before the network (which would refetch the same
            // 4 MiB block for every 128 KiB the player consumes).
            let hit = if cache_blocks {
                self.cache
                    .cached_block(uid, mtime, fsize, bidx)
                    .map(Arc::new)
            } else {
                self.stream_ring.lock().get(uid, mtime, fsize, bidx)
            };
            match hit {
                Some(b) => blocks.push(Some(b)),
                None => {
                    blocks.push(None);
                    misses.push(bidx);
                }
            }
        }

        if !misses.is_empty() {
            let fetched = self.rt.block_on(async {
                // Resolve the file's keys and block table once, then read every
                // missing block through the shared reader. Previously each block
                // called `download_range`, which redid that resolution per block.
                let reader = self.revision_reader(uid, mtime, fsize).await?;

                let mut set = tokio::task::JoinSet::new();
                for &bidx in &misses {
                    let reader = reader.clone();
                    let uid = uid.clone();
                    let bstart = bidx * BLOCK_SIZE;
                    let blen = BLOCK_SIZE.min(fsize - bstart);
                    set.spawn(async move {
                        reader
                            .read_at(bstart, blen)
                            .await
                            .map(|bytes| (bidx, bytes))
                            .map_err(|e| {
                                warn!(%uid, bstart, blen, error = %e, "block read failed");
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
                // A streaming read (large unpinned video) skips the on-disk cache
                // so it can't evict the rest of it; it keeps the block in the
                // in-memory ring instead, which is bounded and dies with the read.
                let bytes = Arc::new(bytes);
                if cache_blocks {
                    let _ = self.cache.store_block(uid, mtime, fsize, bidx, &bytes);
                } else {
                    self.stream_ring
                        .lock()
                        .insert(uid, mtime, fsize, bidx, bytes.clone());
                }
                blocks[(bidx - first) as usize] = Some(bytes);
            }
            if !cache_blocks {
                self.stream_readahead(uid, mtime, fsize, last);
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
    pub(super) fn serve_open_read(
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
                        true,
                    )?);
                }
                // Anything past the base is a hole: zero-fill.
                let zeros = e.saturating_sub(s.max(base_size));
                out.resize(out.len() + zeros as usize, 0);
            }
        }
        Ok(out)
    }

    /// Fill every unauthored range of a scratch/staged file that overlaps its
    /// base with the base's bytes, so the file becomes the complete new content.
    ///
    /// This is the step a partial overwrite cannot skip: only the authored bytes
    /// were ever written to disk, and a revision upload sends the whole file.
    /// The gaps come from the *remote base* (through the block cache, so a small
    /// edit of a large file does not pull all of it), which is exactly why this
    /// can fail with no network — see `StagedWrite`.
    pub(super) fn fill_gaps(
        &self,
        uid: &NodeUid,
        file: &File,
        len: u64,
        base_mtime: i64,
        base_size: u64,
        written: &Intervals,
    ) -> Result<(), Errno> {
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
            let bytes = self.read_range_remote(uid, base_mtime, base_size, s, bend - s, true)?;
            file.write_all_at(&bytes, s).map_err(|err| {
                error!(%uid, error = %err, "scratch gap-fill write failed");
                Errno::EIO
            })?;
        }
        Ok(())
    }
}
