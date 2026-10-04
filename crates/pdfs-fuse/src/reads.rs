//! Revision-reader cache and streamed range-read subsystem.

use super::*;

/// A read of an unpinned video at least this large streams *without* persisting
/// its blocks to the on-disk cache. Playing a 2 GB film would otherwise pour it
/// through the block LRU and evict everything else the user actually wants kept —
/// and it re-streams cheaply enough on a rewatch that keeping it was never worth
/// that. Pinned videos (kept offline on purpose) and anything smaller cache as
/// usual.
pub(super) const STREAM_BYPASS_MIN: u64 = 256 * 1024 * 1024;

/// How many bytes of decrypted blocks the in-memory ring keeps.
///
/// The kernel asks for a file in reads far smaller than [`BLOCK_SIZE`], so
/// without a ring every 128 KiB it consumes costs a whole 4 MiB block again —
/// downloaded and decrypted for a streaming file, or re-read off disk and
/// re-allocated for a cached one (audit T3: 128 MiB of I/O to deliver 4 MiB).
/// Sized for a handful of blocks per concurrently-read file. Bounded and only
/// ever filled by active reads.
const RING_BYTES: u64 = 128 * 1024 * 1024;

/// Deepest read-ahead window, in blocks, for a read that has proved sequential.
const PREFETCH_MAX: u64 = 8;

/// Window a sequential read starts at, and returns to after a seek.
const PREFETCH_MIN: u64 = 2;

/// Largest file [`Core::repair_block`] will pull down whole to work around a
/// revision whose block table understates it. Past this a single read would cost
/// gigabytes of download, so the read fails instead and says to pin the file.
pub(super) const REPAIR_MAX: u64 = 512 * 1024 * 1024;

/// Blocks that may be prefetching at once, across every file. Prefetch takes a
/// permit and gives up rather than queueing, so it can never sit in front of a
/// demand read in the SDK's flat block semaphore.
pub(super) const PREFETCH_BUDGET: usize = 8;

/// How long one read may wait on the network before it fails with `EIO`.
///
/// The SDK bounds each request, not a read: a block that times out is retried,
/// and a read can need several blocks, a block table and a repair. On a link
/// that stalled without dropping, one `read` held a FUSE worker for 34 minutes
/// and the application behind it with it (`docs/BUGS.md` B107). An error the
/// caller can retry beats a process stuck in `D` state.
pub(super) const READ_FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a read of blocks nothing has cached waits for a lost link to come
/// back before failing. Long enough to ride out a Wi-Fi blip, short enough
/// that a program reading while the laptop is really offline gets its `EIO`
/// before the user gives up on it.
const READ_OFFLINE_WAIT: Duration = Duration::from_secs(30);

/// Cap on tracked sequential streams. Only reads in progress matter, so the map
/// is cleared wholesale rather than aged — losing the ramp costs one read.
const MAX_PREFETCH_STREAMS: usize = 256;

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
    reader: Arc<dyn RevisionRead>,
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
    rx: tokio::sync::watch::Receiver<Option<Result<Arc<dyn RevisionRead>, Errno>>>,
}

/// Distinguishes `PendingOpen`s. Only uniqueness matters, so a plain counter
/// does; ids are never compared for order.
static NEXT_OPEN_ID: AtomicU64 = AtomicU64::new(0);

/// In-memory LRU of decrypted 4 MiB blocks, in front of both block paths: the
/// on-disk block cache, and the disk-bypassing stream of a large unpinned video
/// (see [`STREAM_BYPASS_MIN`], which stops one film evicting the whole disk
/// LRU). Either way the point is the same — the kernel reads a block out in ~32
/// pieces, and none of them should cost the block again.
///
/// Validated by the same `(mtime, size)` pair the on-disk caches use: a node
/// whose tag no longer matches has its blocks dropped rather than served, so a
/// new revision can never be stitched together from the old one.
/// A ring entry's identity: the node, and the plaintext range the block covers.
///
/// The range, rather than the block index, because the index alone stopped
/// naming one range when the geometry started coming from the revision — a
/// block cached under the [`BlockGeometry::uniform`] fallback and the block at
/// the same index under the real geometry are different bytes
/// (`docs/BUGS.md` B85). A range is the same identity under either plan, so a
/// re-planned read misses rather than being served the wrong bytes.
type RingKey = (NodeUid, BlockSpan);

#[derive(Default)]
pub(super) struct BlockRing {
    blocks: HashMap<RingKey, Arc<Vec<u8>>>,
    /// Keys least-recently-used first, for eviction.
    order: VecDeque<RingKey>,
    /// Per-node validity tag, `(mtime, size)`.
    tags: HashMap<NodeUid, (i64, u64)>,
    bytes: u64,
}

impl BlockRing {
    fn get(
        &mut self,
        uid: &NodeUid,
        mtime: i64,
        size: u64,
        span: BlockSpan,
    ) -> Option<Arc<Vec<u8>>> {
        if self.tags.get(uid) != Some(&(mtime, size)) {
            return None;
        }
        let key = (uid.clone(), span);
        let hit = self.blocks.get(&key).cloned()?;
        // Move to the young end: a block being read repeatedly (the whole reason
        // this exists) must not age out under a sequential read passing through.
        if let Some(at) = self.order.iter().position(|k| *k == key) {
            self.order.remove(at);
            self.order.push_back(key);
        }
        Some(hit)
    }

    fn insert(
        &mut self,
        uid: &NodeUid,
        mtime: i64,
        size: u64,
        span: BlockSpan,
        bytes: Arc<Vec<u8>>,
    ) {
        if self.tags.get(uid) != Some(&(mtime, size)) {
            // Revision changed under us (or first sight): anything held for this
            // node describes the old one.
            self.drop_node(uid);
            self.tags.insert(uid.clone(), (mtime, size));
        }
        let key = (uid.clone(), span);
        if self.blocks.contains_key(&key) {
            return;
        }
        self.bytes += bytes.len() as u64;
        self.blocks.insert(key.clone(), bytes);
        self.order.push_back(key);
        while self.bytes > RING_BYTES {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.blocks.remove(&victim) {
                self.bytes -= dropped.len() as u64;
            }
            // A node whose last block just aged out keeps no tag: otherwise the
            // tag map is the one thing here that grows with every file ever read.
            if !self.blocks.keys().any(|(u, _)| *u == victim.0) {
                self.tags.remove(&victim.0);
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

/// The result of fetching one block, shared between everyone who wanted it.
type BlockResult = Result<Arc<Vec<u8>>, Errno>;

/// Identifies a block *of a revision*: node, validity tag, and the plaintext
/// range the block covers — see [`RingKey`] for why it is the range and not the
/// index.
type BlockKey = (NodeUid, i64, u64, BlockSpan);

/// A claim on a block key: who owns the fetch, and where its result is published.
type Claim = (u64, tokio::sync::watch::Receiver<Option<BlockResult>>);

/// Block fetches in flight, so a block is downloaded and decrypted once however
/// many readers want it at that moment (audit T1).
///
/// With `FUSE_ASYNC_READ` the demand read and the kernel's read-ahead land in
/// the same 4 MiB block for 31 of every 32 reads, and read-ahead of our own
/// (see [`Core::prefetch`]) aims at blocks a demand read may be about to ask
/// for. Without this each of them pays the full round-trip and decrypt and all
/// but one result is discarded — roughly double the bandwidth and CPU in steady
/// state. Same shape as [`ReaderSlot::Pending`] one level up.
#[derive(Default)]
pub(super) struct BlockFlight {
    inflight: Mutex<HashMap<BlockKey, Claim>>,
}

/// Distinguishes attempts at the same key, so a leader only retires its own.
static NEXT_FLIGHT_ID: AtomicU64 = AtomicU64::new(0);

/// The leader's claim on a key. Retires it on drop — including when the task is
/// cancelled mid-fetch, which is what keeps a dropped `JoinSet` from leaving a
/// key nobody will ever publish.
struct Flight {
    flight: Arc<BlockFlight>,
    key: BlockKey,
    id: u64,
}

impl Drop for Flight {
    fn drop(&mut self) {
        let mut inflight = self.flight.inflight.lock();
        if inflight
            .get(&self.key)
            .is_some_and(|(id, _)| *id == self.id)
        {
            inflight.remove(&self.key);
        }
    }
}

/// Where each file's reader is expected to read next, so sequential reads can be
/// told from random ones and only the former prefetched (audit T2).
#[derive(Default)]
pub(super) struct Prefetch {
    /// Per node: the block index a sequential reader should ask for next, and
    /// the window depth it has earned.
    streams: Mutex<HashMap<NodeUid, (u64, u64)>>,
}

impl Prefetch {
    /// How far past `last` to read ahead for a read of blocks `[first, last]`,
    /// and record where that read leaves the file's cursor.
    ///
    /// A read that starts where the last one stopped doubles the window (from
    /// [`PREFETCH_MIN`], up to [`PREFETCH_MAX`]); anything else is a seek and
    /// earns nothing, so random access — a database file, a linker — never pays
    /// for speculative blocks it will not read. The first read of a file counts
    /// as sequential: `(0, 0)` is both the initial cursor and block zero.
    fn window(&self, uid: &NodeUid, first: u64, last: u64) -> u64 {
        let mut streams = self.streams.lock();
        if streams.len() > MAX_PREFETCH_STREAMS {
            streams.clear();
        }
        let stream = streams.entry(uid.clone()).or_insert((0, 0));
        let depth = if stream.0 == first {
            (stream.1 * 2).clamp(PREFETCH_MIN, PREFETCH_MAX)
        } else {
            0
        };
        *stream = (last + 1, depth);
        depth
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
    ) -> Result<Arc<dyn RevisionRead>, Errno> {
        loop {
            // Decide under the lock, act outside it — network I/O must never run
            // under a std `Mutex`, and an await must never hold one.
            enum Act {
                Hit(Arc<dyn RevisionRead>),
                Join(tokio::sync::watch::Receiver<Option<Result<Arc<dyn RevisionRead>, Errno>>>),
                Lead(
                    u64,
                    tokio::sync::watch::Sender<Option<Result<Arc<dyn RevisionRead>, Errno>>>,
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
                    let result = match self.drive.open_revision(uid).await {
                        Ok(reader) => Ok(reader),
                        Err(e) => {
                            self.lost_link(&e, "open a revision");
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
        // The ring holds decrypted bytes of the same revision, and an eviction is
        // exactly the statement that those are no longer the file's content.
        self.block_ring.lock().drop_node(uid);
        self.prefetch.streams.lock().remove(uid);
    }

    /// How this revision's plaintext is divided into content blocks.
    ///
    /// The revision's own geometry when a previous read has learned and recorded
    /// it, and the [`BLOCK_SIZE`] assumption when it has not — which is every
    /// first read, because learning it means opening a `RevisionReader`, and
    /// opening one per read is precisely the per-file key derivation the block
    /// cache exists to avoid (bugs.md B12, B85).
    ///
    /// Being wrong here is not a correctness problem: `read_at` plans over the
    /// real sizes and clamps to what was asked for either way. It costs a
    /// straddling fetch and a cached block the next read will not ask for, and
    /// both settle once the geometry is known.
    fn block_geometry(&self, uid: &NodeUid, mtime: i64, fsize: u64) -> BlockGeometry {
        self.cache
            .block_geometry(uid, mtime, fsize)
            .unwrap_or_else(|| BlockGeometry::uniform(fsize))
    }

    /// Serve bytes `[offset, offset + len)` of `uid`'s active revision, hitting
    /// the on-disk caches before the network: a whole-file blob (pinned files)
    /// first, then the block cache — fetching only the blocks that overlap the
    /// request, on the revision's own boundaries ([`Core::block_geometry`]), and
    /// caching each. `mtime`/`fsize` validate both caches. Network I/O runs
    /// without any lock held.
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
        // earn a 404 (offline.md Phase 3b). Unless its create landed since the
        // read looked: the landing moved the queued write to the uid Drive
        // gave it, and the row says which (`docs/BUGS.md` B179).
        if is_local_uid(uid) {
            return match self.landed_stand_in_uid(uid) {
                Some(real) => self.read_range(&real, mtime, fsize, offset, len, cache_blocks),
                None => Ok(Vec::new()),
            };
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

    /// Fetch the block at `span`, joining a fetch already in flight rather than
    /// issuing a second one, and publish the result to everyone who joined.
    ///
    /// See [`BlockFlight`]. A follower whose leader disappears (a panic, or a
    /// cancelled `JoinSet`) fetches the block itself rather than failing — the
    /// leader's claim is gone by then, so this cannot recurse.
    async fn fetch_block(
        &self,
        reader: &Arc<dyn RevisionRead>,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        span: BlockSpan,
        cache_blocks: bool,
    ) -> BlockResult {
        let key: BlockKey = (uid.clone(), mtime, fsize, span);
        let claim = {
            let mut inflight = self.block_flight.inflight.lock();
            match inflight.get(&key) {
                Some((_, rx)) => Err(rx.clone()),
                None => {
                    let id = NEXT_FLIGHT_ID.fetch_add(1, Ordering::Relaxed);
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    inflight.insert(key.clone(), (id, rx));
                    Ok((
                        tx,
                        Flight {
                            flight: self.block_flight.clone(),
                            key: key.clone(),
                            id,
                        },
                    ))
                }
            }
        };
        let (tx, flight) = match claim {
            Ok(lead) => lead,
            Err(mut rx) => {
                loop {
                    if let Some(result) = rx.borrow_and_update().clone() {
                        return result;
                    }
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                return self
                    .read_block(reader, uid, mtime, fsize, span, cache_blocks)
                    .await;
            }
        };
        let result = self
            .read_block(reader, uid, mtime, fsize, span, cache_blocks)
            .await;
        // Retire the claim before publishing, so a racer that wakes and re-enters
        // finds no stale entry to join.
        drop(flight);
        let _ = tx.send(Some(result.clone()));
        result
    }

    /// Download and decrypt one block, then put it where the next reader will
    /// find it: the in-memory ring always, the on-disk block cache too unless
    /// this is a disk-bypassing stream.
    ///
    /// The disk store is handed to a blocking thread — it is a 4 MiB write plus
    /// a cache-index update, and neither the reactor nor the kernel read waiting
    /// on these bytes should be held up by it.
    ///
    /// A block that comes back shorter than the file's size says it must be is
    /// never served, cached or promoted: it is repaired from the whole-file
    /// download, or the read fails. See [`Core::repair_block`] (bugs.md B84).
    async fn read_block(
        &self,
        reader: &Arc<dyn RevisionRead>,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        span: BlockSpan,
        cache_blocks: bool,
    ) -> BlockResult {
        // The reader derives every block boundary from the revision's recorded
        // block sizes. If they do not add up to the size the node reports, this
        // file's boundaries are wrong, and the failure is not confined to short
        // reads: sizes that *overstate* a block shift every block after it and
        // return full-length reads of the wrong bytes. So a disagreement of any
        // kind takes the file off the range path entirely (bugs.md B84).
        let bytes = if reader.size() != fsize {
            warn!(
                %uid, bidx = span.idx, fsize,
                reader_size = reader.size(),
                reader_blocks = reader.block_sizes().len(),
                "the revision's block table disagrees with the node size"
            );
            self.repair_block(uid, mtime, fsize, span).await?
        } else {
            // Learned here rather than on a path of its own: this is where a
            // reader is open anyway, and recording the real geometry is what
            // lets the *next* read plan its blocks on the revision's own
            // boundaries instead of the 4 MiB fallback (bugs.md B85).
            if cache_blocks {
                self.cache
                    .store_block_geometry(uid, mtime, fsize, reader.block_sizes());
            }
            let bytes = reader.read_at(span.start, span.len).await.map_err(|e| {
                self.lost_link(&e, "read a block");
                warn!(%uid, start = span.start, len = span.len, error = %e, "block read failed");
                Errno::EIO
            })?;
            if bytes.len() as u64 == span.len {
                bytes
            } else {
                warn!(
                    %uid, bidx = span.idx, blen = span.len, got = bytes.len(), fsize,
                    "block came back shorter than the range asked for"
                );
                self.repair_block(uid, mtime, fsize, span).await?
            }
        };
        let bytes = Arc::new(bytes);
        if cache_blocks {
            let cache = self.cache.clone();
            let uid = uid.clone();
            let bytes = bytes.clone();
            tokio::task::spawn_blocking(move || {
                let _ = cache.store_block(&uid, mtime, fsize, span, &bytes);
            });
        }
        self.block_ring
            .lock()
            .insert(uid, mtime, fsize, span, bytes.clone());
        Ok(bytes)
    }

    /// Recover the block at `span` of a file whose per-block reads cannot be
    /// by fetching the whole revision through the manifest-verified download and
    /// serving the block out of that.
    ///
    /// The range path derives every block boundary from the revision's recorded
    /// block sizes. When those understate the file, reads come back short with
    /// no error anywhere, and the caller hands userspace a truncated file that
    /// looks like a clean EOF (bugs.md B84). `download_file_to` does not do that
    /// arithmetic — it streams the block list — so it returns the whole file
    /// where the range path cannot, which is why pinning was the workaround.
    /// This does the same thing on the user's behalf, once, and the resulting
    /// blob answers every later read of the file before the block path is
    /// consulted at all.
    ///
    /// Bounded by [`REPAIR_MAX`]: past that, one unlucky read would pull down
    /// gigabytes, and a clear `EIO` plus a `pdfs pin` is the better trade.
    async fn repair_block(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        span: BlockSpan,
    ) -> Result<Vec<u8>, Errno> {
        let (bstart, blen) = (span.start, span.len);
        if fsize > REPAIR_MAX {
            warn!(
                %uid, fsize,
                "file is too large to repair from a whole-file download; pin it to read it"
            );
            return Err(Errno::EIO);
        }
        // One download per file, not per block: every block of a file with a bad
        // block table discovers the same problem at once.
        let lock = {
            let mut locks = self.content_repairs.lock().await;
            locks.entry(uid.clone()).or_default().clone()
        };
        let guard = lock.clone().lock_owned().await;
        let downloaded = match self.cache.read_range(uid, mtime, fsize, bstart, blen) {
            // The first arrival did the work; the rest just read its blob.
            Some(bytes) if bytes.len() as u64 == blen => Ok(Some(bytes)),
            _ => self
                .download_whole_revision(uid, mtime, fsize)
                .await
                .map(|()| None),
        };
        // Before releasing, so the map entry is only dropped once no waiter is
        // still holding it.
        drop(guard);
        self.release_repair_lock(uid, lock).await;
        if let Some(bytes) = downloaded? {
            return Ok(bytes);
        }
        match self.cache.read_range(uid, mtime, fsize, bstart, blen) {
            Some(bytes) if bytes.len() as u64 == blen => {
                info!(%uid, bidx = span.idx, "served a short block from a repaired whole-file download");
                Ok(bytes)
            }
            _ => {
                error!(%uid, bidx = span.idx, fsize, "repairing a short block did not produce the block");
                Err(Errno::EIO)
            }
        }
    }

    /// Download the whole of `uid`'s active revision to disk and adopt it as the
    /// cached blob, so the range path is bypassed entirely for this file.
    ///
    /// Streamed into a scratch file rather than memory: this runs for files up
    /// to [`REPAIR_MAX`], and buffering that per concurrent reader is not a cost
    /// a read should carry. A download that does not produce exactly `fsize`
    /// bytes is discarded — a short blob would fail the cache's own validity
    /// check anyway, and storing it would only hide the disagreement one layer
    /// further down.
    async fn download_whole_revision(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
    ) -> Result<(), Errno> {
        let (mut file, path) = self.cache.create_scratch().map_err(|error| {
            error!(%uid, %error, "creating a scratch file for a repair download failed");
            Errno::EIO
        })?;
        let outcome = self.drive.download_file_to(uid, &mut file).await;
        let stored = (|| {
            outcome.map_err(|error| {
                warn!(%uid, %error, "repair download failed");
                Errno::EIO
            })?;
            file.sync_all().map_err(|error| {
                warn!(%uid, %error, "syncing a repair download failed");
                Errno::EIO
            })?;
            let got = file.metadata().map(|m| m.len()).unwrap_or(0);
            if got != fsize {
                warn!(%uid, fsize, got, "repair download did not match the node size");
                return Err(Errno::EIO);
            }
            self.cache
                .store_file(uid, mtime, fsize, &path)
                .map_err(|error| {
                    error!(%uid, %error, "adopting a repair download as cached content failed");
                    Errno::EIO
                })
        })();
        // The blob is a hardlink to the same inode, so unlinking the scratch
        // name leaves it intact.
        let _ = std::fs::remove_file(&path);
        stored
    }

    /// Drop a repair lock's map entry once nobody else holds it, so a file
    /// repaired long ago does not keep a mutex alive forever.
    async fn release_repair_lock(&self, uid: &NodeUid, lock: Arc<tokio::sync::Mutex<()>>) {
        let mut locks = self.content_repairs.lock().await;
        // Two: the map's and ours. Anything more means a waiter still needs it.
        if Arc::strong_count(&lock) <= 2 {
            locks.remove(uid);
        }
    }

    /// Warm the blocks after a sequential read into the caches, in the
    /// background, so the next read finds them instead of paying a round-trip.
    ///
    /// A read that continues where the last one stopped doubles the window, up
    /// to [`PREFETCH_MAX`]; a seek drops it back to nothing and one more
    /// sequential read is needed to earn it again. Without this every read is a
    /// serial fetch/serve/stall chain: on a 100 ms link that ceilings a file at
    /// ~24 MB/s of the ~125 MB/s available, and the SDK's block concurrency is
    /// never exercised at all.
    ///
    /// Fire-and-forget, and permit-bounded ([`PREFETCH_BUDGET`]) so speculative
    /// work never sits in front of a read someone is waiting on. A failure here
    /// is not an error: the read that needs those bytes will fetch them itself.
    fn prefetch_ahead(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        first: u64,
        last: u64,
        cache_blocks: bool,
    ) {
        let depth = self.prefetch.window(uid, first, last);
        if depth == 0 {
            return;
        }
        let geometry = self.block_geometry(uid, mtime, fsize);
        let last_block = geometry.blocks().saturating_sub(1);
        let wanted: Vec<BlockSpan> = ((last + 1)..=(last + depth).min(last_block))
            .filter_map(|bidx| geometry.span(bidx))
            .filter(|&span| {
                // Blocks already in memory need nothing; blocks already on disk
                // are cheap enough to serve that spending a round-trip to move
                // them into the ring would be the wrong trade.
                self.block_ring
                    .lock()
                    .get(uid, mtime, fsize, span)
                    .is_none()
                    && !(cache_blocks && self.cache.has_block(uid, mtime, fsize, span))
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
            // round-trips, and the reader catches up with the window before it
            // finishes.
            let mut set = tokio::task::JoinSet::new();
            for span in wanted {
                let Ok(permit) = core.prefetch_budget.clone().try_acquire_owned() else {
                    break; // the budget is spent; the demand path can have it
                };
                let (core, reader, uid) = (core.clone(), reader.clone(), uid.clone());
                set.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = core
                        .fetch_block(&reader, &uid, mtime, fsize, span, cache_blocks)
                        .await
                    {
                        debug!(%uid, bidx = span.idx, ?error, "prefetch failed");
                    }
                });
            }
            while set.join_next().await.is_some() {}
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
        // On the revision's own boundaries where they are known, and on the
        // 4 MiB fallback where they are not (bugs.md B85).
        let wanted = self.block_geometry(uid, mtime, fsize).spans(offset, end);
        let (Some(&head), Some(&tail)) = (wanted.first(), wanted.last()) else {
            return Ok(out);
        };
        let (first, last) = (head.idx, tail.idx);

        // Collect the blocks overlapping the request, serving any already cached
        // and fetching the rest concurrently. A multi-block read (e.g. a media
        // player buffering, or a large sequential read split into one FUSE call)
        // would otherwise stall on each block round-trip in turn; downloading the
        // misses in parallel saturates the connection and bounds latency at the
        // slowest single block instead of their sum.
        let mut blocks: Vec<Option<Arc<Vec<u8>>>> = Vec::with_capacity(wanted.len());
        let mut misses: Vec<BlockSpan> = Vec::new();
        for &span in &wanted {
            // The in-memory ring comes first on both paths: a streaming read has
            // no other cache at all, and a cached one would otherwise pay two
            // opens, a JSON parse and a 4 MiB read+alloc for every 128 KiB the
            // kernel asks for (audit T3).
            let hit = self.block_ring.lock().get(uid, mtime, fsize, span);
            let hit = hit.or_else(|| {
                if !cache_blocks {
                    return None;
                }
                let bytes = Arc::new(self.cache.cached_block(uid, mtime, fsize, span)?);
                // Promote it, so the other ~31 reads of this block are answered
                // from memory.
                self.block_ring
                    .lock()
                    .insert(uid, mtime, fsize, span, bytes.clone());
                Some(bytes)
            });
            match hit {
                Some(b) => blocks.push(Some(b)),
                None => {
                    blocks.push(None);
                    misses.push(span);
                }
            }
        }

        if !misses.is_empty() {
            // One deadline for the whole read, including any wait for a lost
            // link. A fetch that failed because the link went down is tried
            // once more when it is back, so a blip is a slow read, not an `EIO`.
            let deadline = Instant::now() + READ_FETCH_TIMEOUT;
            let mut retried = false;
            let fetched = loop {
                if !self.is_online() {
                    let wait =
                        READ_OFFLINE_WAIT.min(deadline.saturating_duration_since(Instant::now()));
                    if !self.wait_online(wait) {
                        debug!(%uid, offset, len, "read of uncached blocks while offline; failing it");
                        return Err(Errno::EIO);
                    }
                }
                let fetch = async {
                    // Resolve the file's keys and block table once, then read every
                    // missing block through the shared reader. Previously each block
                    // called `download_range`, which redid that resolution per block.
                    let reader = self.revision_reader(uid, mtime, fsize).await?;

                    let mut set = tokio::task::JoinSet::new();
                    for &span in &misses {
                        let (core, reader, uid) = (self.clone(), reader.clone(), uid.clone());
                        set.spawn(async move {
                            core.fetch_block(&reader, &uid, mtime, fsize, span, cache_blocks)
                                .await
                                .map(|bytes| (span.idx, bytes))
                        });
                    }
                    let mut out = Vec::with_capacity(misses.len());
                    while let Some(joined) = set.join_next().await {
                        // A join error means the task panicked; surface it as EIO.
                        out.push(joined.map_err(|_| Errno::EIO)??);
                    }
                    Ok::<_, Errno>(out)
                };
                // Dropping the fetch on the deadline aborts the block tasks with
                // it; blocks that already landed stay cached for the retry. The
                // timer is built inside the runtime: a FUSE worker has no reactor
                // of its own, and building it out here panicked every remote read.
                let budget = deadline.saturating_duration_since(Instant::now());
                match self
                    .rt
                    .block_on(async { tokio::time::timeout(budget, fetch).await })
                {
                    Ok(Ok(fetched)) => break fetched,
                    // The fetch saw the link go down; the top of the loop waits
                    // for it within what is left of the deadline.
                    Ok(Err(_)) if !retried && !self.is_online() => retried = true,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        warn!(
                            %uid, offset, len, secs = READ_FETCH_TIMEOUT.as_secs(),
                            "read got no answer from the network in time; failing it"
                        );
                        self.evict_reader(uid);
                        self.mark_offline("read", &crate::link::no_answer(READ_FETCH_TIMEOUT));
                        return Err(Errno::EIO);
                    }
                }
            };
            for (bidx, bytes) in fetched {
                blocks[(bidx - first) as usize] = Some(bytes);
            }
        }
        // Also after a wholly-cached read: that is what keeps the window sliding
        // ahead of a sequential reader instead of stalling once it catches up.
        self.prefetch_ahead(uid, mtime, fsize, first, last, cache_blocks);

        for (span, block) in wanted.into_iter().zip(blocks) {
            let bstart = span.start;
            // Every slot is populated: cache hits up front, misses by the fetch above.
            let block = block.expect("block fetched or cached");
            // Both sources guarantee the length the geometry says the block has
            // — the cache validates it on read, the network path in `read_block`
            // — so this refuses rather than quietly contributing fewer bytes
            // than the range covers, which is what turned a bad block into a
            // truncated file (bugs.md B84).
            if block.len() as u64 != span.len {
                error!(
                    %uid, bidx = span.idx, len = block.len(), expected = span.len, fsize,
                    "assembling a read hit a short block"
                );
                return Err(Errno::EIO);
            }
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

    /// The largest amount [`Core::fill_gaps_cached`] will copy out of the local
    /// caches while a `release(2)` is in progress.
    ///
    /// The point of the cached fill is that editing a file the mount already
    /// holds costs nothing extra; it is not to reconstruct arbitrarily large
    /// files on the dispatch loop. Past this budget the blob stays incomplete
    /// and the drain finishes it, which is the same work in a place where it
    /// blocks nobody.
    const CACHED_FILL_BUDGET: u64 = 64 * 1024 * 1024;

    /// Bytes `[offset, offset + len)` of `uid`'s base revision, but only if the
    /// local caches can answer in full — the whole-file blob first, then every
    /// overlapping block. `None` the moment anything would have to be fetched.
    fn cached_range(
        &self,
        uid: &NodeUid,
        mtime: i64,
        fsize: u64,
        offset: u64,
        len: u64,
    ) -> Option<Vec<u8>> {
        if len == 0 {
            return Some(Vec::new());
        }
        if let Some(bytes) = self.cache.read_range(uid, mtime, fsize, offset, len) {
            return Some(bytes);
        }
        let end = offset.checked_add(len)?.min(fsize);
        if offset >= end {
            return None;
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        for span in self.block_geometry(uid, mtime, fsize).spans(offset, end) {
            let ring = self.block_ring.lock().get(uid, mtime, fsize, span);
            let block = match ring {
                Some(bytes) => bytes,
                None => Arc::new(self.cache.cached_block(uid, mtime, fsize, span)?),
            };
            let bstart = span.start;
            let s = (offset.max(bstart) - bstart) as usize;
            let e = (end.min(bstart + block.len() as u64) - bstart) as usize;
            if s >= e {
                return None;
            }
            out.extend_from_slice(&block[s..e]);
        }
        (out.len() as u64 == end - offset).then_some(out)
    }

    /// Fill what [`Core::fill_gaps`] would, but only from bytes already on this
    /// machine, and only up to [`Core::CACHED_FILL_BUDGET`]. Ranges past the
    /// base are zeros the caller's `set_len` already wrote, so they count as
    /// filled. `written` is extended with everything that ends up resolved.
    ///
    /// This is the release-time half of gap filling. Doing the network half
    /// there as well is what froze the whole mount for the length of a download
    /// (audit F1): the closing file's untouched ranges are allowed to stay
    /// unresolved in the staged blob — `StagedWrite::complete` is false, reads
    /// resolve the gaps against the base, and `drain_revision` fills them on the
    /// drain thread before it uploads. What is left here is the case where
    /// filling is free, which is also the common one: a file that was read
    /// before it was edited.
    pub(super) fn fill_gaps_cached(
        &self,
        uid: &NodeUid,
        file: &File,
        len: u64,
        base_mtime: i64,
        base_size: u64,
        written: &mut Intervals,
    ) {
        let mut budget = Self::CACHED_FILL_BUDGET;
        for (s, e, authored) in written.clone().segments(0, len) {
            if authored {
                continue;
            }
            let bend = e.min(base_size);
            if s < bend {
                let want = bend - s;
                if want > budget {
                    continue;
                }
                let Some(bytes) = self.cached_range(uid, base_mtime, base_size, s, want) else {
                    continue;
                };
                if let Err(error) = file.write_all_at(&bytes, s) {
                    warn!(%uid, %error, "cached gap-fill write failed");
                    continue;
                }
                budget -= want;
                written.add(s, bend);
            }
            // Past the base there is nothing to fetch: the resize zeroed it, so
            // those bytes are already the file's content.
            if e > base_size {
                written.add(s.max(base_size), e);
            }
        }
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

#[cfg(test)]
mod block_ring_tests {
    use super::*;

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    /// One block's worth of bytes, so byte accounting is exercised at the scale
    /// the ring actually sees.
    fn block(byte: u8) -> Arc<Vec<u8>> {
        Arc::new(vec![byte; BLOCK_SIZE as usize])
    }

    /// How many whole blocks fit before the ring starts evicting.
    fn capacity() -> u64 {
        RING_BYTES / BLOCK_SIZE
    }

    /// Block `idx` under the uniform geometry — the shape these tests are
    /// written against, since what they exercise is tagging and eviction rather
    /// than block layout.
    fn span(idx: u64) -> BlockSpan {
        BlockSpan {
            idx,
            start: idx * BLOCK_SIZE,
            len: BLOCK_SIZE,
        }
    }

    #[test]
    fn a_stored_block_is_served_back() {
        let mut ring = BlockRing::default();
        let node = uid("a");
        ring.insert(&node, 7, 100, span(3), block(0xab));
        assert_eq!(ring.get(&node, 7, 100, span(3)).map(|b| b[0]), Some(0xab));
    }

    #[test]
    fn a_block_of_another_revision_is_never_served() {
        let mut ring = BlockRing::default();
        let node = uid("a");
        ring.insert(&node, 7, 100, span(0), block(1));
        // Same node, newer revision: the tag moved, so the old bytes go.
        assert!(ring.get(&node, 8, 100, span(0)).is_none());
        assert!(ring.get(&node, 7, 101, span(0)).is_none());
    }

    #[test]
    fn a_new_revision_drops_what_the_old_one_left() {
        let mut ring = BlockRing::default();
        let node = uid("a");
        ring.insert(&node, 7, 100, span(0), block(1));
        ring.insert(&node, 8, 100, span(0), block(2));
        assert_eq!(ring.get(&node, 8, 100, span(0)).map(|b| b[0]), Some(2));
        assert_eq!(ring.bytes, BLOCK_SIZE);
    }

    #[test]
    fn the_ring_evicts_the_least_recently_used_block() {
        let mut ring = BlockRing::default();
        let node = uid("a");
        for idx in 0..capacity() {
            ring.insert(&node, 1, u64::MAX, span(idx), block(idx as u8));
        }
        // Re-reading block 0 makes block 1 the oldest instead.
        assert!(ring.get(&node, 1, u64::MAX, span(0)).is_some());
        ring.insert(&node, 1, u64::MAX, span(capacity()), block(0xff));
        assert!(ring.get(&node, 1, u64::MAX, span(0)).is_some());
        assert!(ring.get(&node, 1, u64::MAX, span(1)).is_none());
        assert_eq!(ring.bytes, RING_BYTES);
    }

    #[test]
    fn a_node_whose_last_block_ages_out_keeps_no_tag() {
        let mut ring = BlockRing::default();
        let (old, new) = (uid("old"), uid("new"));
        ring.insert(&old, 1, u64::MAX, span(0), block(1));
        for idx in 0..capacity() {
            ring.insert(&new, 1, u64::MAX, span(idx), block(2));
        }
        assert!(!ring.tags.contains_key(&old));
        assert!(ring.tags.contains_key(&new));
    }

    #[test]
    fn dropping_a_node_frees_only_its_own_bytes() {
        let mut ring = BlockRing::default();
        let (a, b) = (uid("a"), uid("b"));
        ring.insert(&a, 1, 100, span(0), block(1));
        ring.insert(&b, 1, 100, span(0), block(2));
        ring.drop_node(&a);
        assert!(ring.get(&a, 1, 100, span(0)).is_none());
        assert_eq!(ring.get(&b, 1, 100, span(0)).map(|x| x[0]), Some(2));
        assert_eq!(ring.bytes, BLOCK_SIZE);
    }
}

#[cfg(test)]
mod prefetch_tests {
    use super::*;

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    #[test]
    fn a_sequential_reader_ramps_up_to_the_cap() {
        let prefetch = Prefetch::default();
        let node = uid("a");
        let depths: Vec<u64> = (0..6)
            .map(|next| prefetch.window(&node, next, next))
            .collect();
        assert_eq!(depths, vec![2, 4, 8, 8, 8, 8]);
    }

    #[test]
    fn a_seek_forfeits_the_window() {
        let prefetch = Prefetch::default();
        let node = uid("a");
        assert_eq!(prefetch.window(&node, 0, 0), PREFETCH_MIN);
        assert_eq!(prefetch.window(&node, 40, 40), 0);
        // And has to be earned again from the bottom.
        assert_eq!(prefetch.window(&node, 41, 41), PREFETCH_MIN);
    }

    #[test]
    fn a_multi_block_read_advances_the_cursor_past_all_of_it() {
        let prefetch = Prefetch::default();
        let node = uid("a");
        assert_eq!(prefetch.window(&node, 0, 3), PREFETCH_MIN);
        assert_eq!(prefetch.window(&node, 4, 7), 4);
    }

    #[test]
    fn files_are_tracked_independently() {
        let prefetch = Prefetch::default();
        let (a, b) = (uid("a"), uid("b"));
        assert_eq!(prefetch.window(&a, 0, 0), PREFETCH_MIN);
        assert_eq!(prefetch.window(&b, 9, 9), 0);
        assert_eq!(prefetch.window(&a, 1, 1), 4);
    }

    #[test]
    fn the_stream_map_cannot_grow_without_bound() {
        let prefetch = Prefetch::default();
        for n in 0..=MAX_PREFETCH_STREAMS + 1 {
            prefetch.window(&uid(&n.to_string()), 0, 0);
        }
        assert!(prefetch.streams.lock().len() <= MAX_PREFETCH_STREAMS + 1);
    }
}
