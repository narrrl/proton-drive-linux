//! A file open for writing, and a released write waiting for its upload.
//!
//! Authored bytes go to a scratch file on disk, not to memory. [`Intervals`]
//! says which bytes of it were written here; the rest of the file still comes
//! from the remote base. On release the handle becomes a [`PendingRevision`]:
//! the staged blob and what its gaps refer to.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;

use pdfs_drive::{node_revision_id, node_size};
use pdfs_store::cache::StagedWrite;
use proton_drive_rs::Node;
use proton_drive_rs::proton_sdk::ids::NodeUid;

/// A set of non-overlapping `[start, end)` byte ranges, kept sorted and merged.
/// Tracks which bytes of a [`WriteHandle`]'s scratch file were authored locally
/// (vs. still living only in the remote base), so reads and the commit gap-fill
/// know which regions to pull from the network.
#[derive(Clone, Default)]
pub struct Intervals(pub Vec<(u64, u64)>);

impl Intervals {
    /// Mark `[start, end)` as authored, coalescing with any touching ranges.
    pub fn add(&mut self, start: u64, end: u64) {
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
    pub fn clip(&mut self, len: u64) {
        self.0.retain(|&(s, _)| s < len);
        for iv in &mut self.0 {
            iv.1 = iv.1.min(len);
        }
    }

    /// Split `[start, end)` into contiguous `(s, e, authored)` segments, in
    /// order. `authored == true` means the bytes live in the scratch file;
    /// `false` means they must come from the remote base (or are a hole).
    pub fn segments(&self, start: u64, end: u64) -> Vec<(u64, u64, bool)> {
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
pub struct WriteHandle {
    pub ino: u64,
    pub uid: NodeUid,
    /// Disk-backed staging buffer. Shared (`Arc`) so reads can use it without
    /// holding the state lock across I/O. Accessed positionally (`read_at`/
    /// `write_at`), so a clone never disturbs another's file offset.
    pub file: Arc<File>,
    /// Scratch file path, removed on release.
    pub path: PathBuf,
    /// Byte ranges authored into `file`. Everything else in `[0, len)` is base.
    pub written: Intervals,
    /// Logical file size (may exceed authored bytes after a truncate-extend).
    pub len: u64,
    /// Size of the remote base at open, for serving untouched ranges.
    pub base_size: u64,
    /// Modification time of the remote base at open, validating its block cache.
    pub base_mtime: i64,
    /// Server revision id of the remote base at open, if it had one. This is the
    /// stable identity the drain conflict-checks against: `base_mtime` alone
    /// drifts when the server re-stamps the *same* revision, which reads as a
    /// spurious conflict (see B16/B25). `None` for a file with no sealed remote
    /// revision, or one read through a surface that does not carry the id.
    pub base_revision_id: Option<String>,
    /// Whether anything diverged from the remote and needs an upload.
    pub dirty: bool,
    /// Number of file handles currently sharing this scratch state.
    pub open_count: usize,
}

impl WriteHandle {
    /// A handle on a file `create` just made: an empty base, so everything
    /// written through it is authored.
    ///
    /// The base is `node` as it actually exists — the empty file the server just
    /// minted, or a queued create's placeholder — never the local clock or a
    /// blank. `queue_revision` turns it into `StagedWrite::based_on` and the
    /// drain compares the remote against that. A `now_secs()` mtime differs from
    /// the server's stamp by the create round-trip. A missing revision id drops
    /// the check to `(mtime, size)`, and moving or renaming the file advances
    /// its mtime on the server. Either way the first write to a brand-new file
    /// lands as a conflict copy of its own empty create (`docs/BUGS.md` B113).
    pub fn created(ino: u64, node: &Node, file: File, path: PathBuf) -> Self {
        Self {
            ino,
            uid: node.uid.clone(),
            file: Arc::new(file),
            path,
            written: Intervals::default(),
            len: 0,
            base_size: 0,
            base_mtime: node.modification_time,
            base_revision_id: node_revision_id(node),
            dirty: false,
            open_count: 0,
        }
    }

    /// Write over `node` from now on: a revision this daemon has just put on the
    /// server under the handle, by sealing an upload or by landing the queued
    /// create the handle was opened on. The base the handle opened over is gone,
    /// and keeping it would make the drain read our own revision as another
    /// device's edit.
    pub fn rebase_onto(&mut self, node: &Node) {
        self.base_mtime = node.modification_time;
        self.base_size = node_size(node);
        self.base_revision_id = node_revision_id(node);
    }

    /// Resize the file to `size`, as `setattr` does through an open handle.
    ///
    /// The scratch file goes first: a resize it refuses (a full disk on a
    /// grow) fails with nothing recorded. Otherwise the length and the
    /// authored ranges would describe bytes the scratch file does not hold,
    /// and the call would have succeeded.
    pub fn resize(&mut self, size: u64) -> std::io::Result<()> {
        self.file.set_len(size)?;
        if size < self.len {
            // Shrink: drop authored ranges past the new end.
            self.written.clip(size);
        } else if size > self.len {
            // Grow: the new tail is defined as zeros, so claim it as authored
            // rather than base content.
            self.written.add(self.len, size);
        }
        self.len = size;
        self.dirty = true;
        Ok(())
    }

    /// Account for `[offset, end)` having been written through this handle.
    ///
    /// A write past the end leaves a hole that reads as zeros, exactly like a
    /// grow through `setattr`, so the hole is authored too. Left unauthored, a
    /// hole below the base size was refilled from the base at commit: a file
    /// shrunk and then written past its new end got its old bytes back.
    pub fn record_write(&mut self, offset: u64, end: u64) {
        if offset > self.len {
            self.written.add(self.len, offset);
        }
        self.written.add(offset, end);
        self.len = self.len.max(end);
        self.dirty = true;
    }
}

/// The files open through one tree: write handles by inode, file handles
/// by `fh`, and the inodes whose last handle is being released.
///
/// One `fh` counter serves file and directory handles, so the two never
/// collide in the kernel's one space of handle numbers.
pub struct OpenFiles {
    /// Write state by inode. Concurrent writers share the scratch file.
    writes: HashMap<u64, WriteHandle>,
    /// Inodes whose last write handle a release has taken out of
    /// [`OpenFiles::writes`] and is still queueing. `close(2)` returns before
    /// that, and in between the edit is in neither the handles nor the queue,
    /// so a check for changes not on Drive yet looks here too.
    releasing: HashSet<u64>,
    /// The inode each write `fh` is open on. Read-only opens use `fh` 0 and
    /// have no entry.
    handles: HashMap<u64, u64>,
    next_fh: u64,
}

impl Default for OpenFiles {
    fn default() -> Self {
        Self {
            writes: HashMap::new(),
            releasing: HashSet::new(),
            handles: HashMap::new(),
            next_fh: 1,
        }
    }
}

/// What a release of one file handle left behind.
pub enum Released {
    /// The handle was not a write handle, or was already gone.
    NotWriting,
    /// Other handles still share the inode's write state.
    StillOpen { ino: u64 },
    /// The last handle: the write state, now the caller's to queue. The inode
    /// stays [releasing](OpenFiles::is_owed) until
    /// [`OpenFiles::finish_release`].
    Last(WriteHandle),
}

impl OpenFiles {
    /// A new handle number, for a file or a directory.
    pub fn next_fh(&mut self) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        fh
    }

    /// Open a write handle on `ino`: share the inode's write state, or start
    /// it with `start`. Returns the handle number.
    pub fn open_writer(&mut self, ino: u64, start: impl FnOnce() -> WriteHandle) -> u64 {
        let fh = self.next_fh();
        self.writes.entry(ino).or_insert_with(start).open_count += 1;
        self.handles.insert(fh, ino);
        fh
    }

    /// The write state of `ino`, if a handle has it open for writing.
    pub fn writer(&self, ino: u64) -> Option<&WriteHandle> {
        self.writes.get(&ino)
    }

    pub fn writer_mut(&mut self, ino: u64) -> Option<&mut WriteHandle> {
        self.writes.get_mut(&ino)
    }

    /// The inode write handle `fh` is open on.
    pub fn ino_of(&self, fh: u64) -> Option<u64> {
        self.handles.get(&fh).copied()
    }

    /// The write state behind file handle `fh`.
    pub fn by_fh(&self, fh: u64) -> Option<&WriteHandle> {
        self.writes.get(&self.ino_of(fh)?)
    }

    pub fn by_fh_mut(&mut self, fh: u64) -> Option<&mut WriteHandle> {
        let ino = self.ino_of(fh)?;
        self.writes.get_mut(&ino)
    }

    /// Whether `ino` is open for writing.
    pub fn is_writing(&self, ino: u64) -> bool {
        self.writes.contains_key(&ino)
    }

    /// Whether `ino` is being written or held by a write handle.
    pub fn is_busy(&self, ino: u64) -> bool {
        self.is_writing(ino) || self.handles.values().any(|&held| held == ino)
    }

    /// Every inode with changes that may not be queued yet: open for writing,
    /// or released and still queueing. `dirty_only` leaves out write handles
    /// that changed nothing.
    pub fn owed(&self, dirty_only: bool) -> impl Iterator<Item = u64> + '_ {
        self.writes
            .iter()
            .filter(move |(_, w)| !dirty_only || w.dirty)
            .map(|(&ino, _)| ino)
            .chain(self.releasing.iter().copied())
    }

    /// Every write state, to rebase or inspect in place.
    pub fn writers_mut(&mut self) -> impl Iterator<Item = &mut WriteHandle> {
        self.writes.values_mut()
    }

    /// Release file handle `fh`. On the last handle of an inode, the write
    /// state is handed back and the inode is marked as releasing.
    pub fn release(&mut self, fh: u64) -> Released {
        let Some(ino) = self.handles.remove(&fh) else {
            return Released::NotWriting;
        };
        let Some(writer) = self.writes.get_mut(&ino) else {
            return Released::NotWriting;
        };
        writer.open_count = writer.open_count.saturating_sub(1);
        if writer.open_count > 0 {
            return Released::StillOpen { ino };
        }
        let writer = self.writes.remove(&ino).expect("looked up above");
        self.releasing.insert(ino);
        Released::Last(writer)
    }

    /// The release of `ino` has queued what it wrote.
    pub fn finish_release(&mut self, ino: u64) {
        self.releasing.remove(&ino);
    }

    /// Take every write state, for a stop that queues what open handles
    /// wrote. Forgets every handle.
    pub fn take_all(&mut self) -> Vec<WriteHandle> {
        self.handles.clear();
        self.writes.drain().map(|(_, writer)| writer).collect()
    }

    /// Put write state in place directly, for tests that build a tree.
    pub fn insert_writer(&mut self, writer: WriteHandle) {
        self.writes.insert(writer.ino, writer);
    }

    /// Drop the write state of `ino` without queueing it, for tests.
    pub fn remove_writer(&mut self, ino: u64) -> Option<WriteHandle> {
        self.writes.remove(&ino)
    }
}

/// Why staging a write failed.
#[derive(Debug)]
pub enum WriteError {
    /// The disk holding the scratch file is full. Not a broken file: the
    /// caller says so, so the application can take its out-of-space path, and
    /// frees what the cache holds on the same disk.
    NoSpace,
    /// Any other failure to write the scratch file.
    Io(std::io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::NoSpace => f.write_str("no space left for the scratch file"),
            WriteError::Io(error) => write!(f, "scratch write failed: {error}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// Write `data` at `offset` into a handle's scratch file. Positional, so it
/// needs no lock on the handle: the caller records the range with
/// [`WriteHandle::record_write`] afterwards, and only on success.
pub fn stage_write(file: &File, offset: u64, data: &[u8]) -> Result<(), WriteError> {
    file.write_all_at(data, offset).map_err(|error| {
        if error.raw_os_error() == Some(libc::ENOSPC) {
            WriteError::NoSpace
        } else {
            WriteError::Io(error)
        }
    })
}

/// A released write whose upload has not happened yet.
///
/// The bytes live in the content cache's staging dir and the intent lives in the
/// `pending_op` table; this pairs them in memory so a read can be served without
/// a database round trip.
#[derive(Clone)]
pub struct PendingRevision {
    /// Staged blob holding the written bytes.
    pub path: PathBuf,
    /// Which of those bytes are real, and what base the gaps refer to.
    pub meta: StagedWrite,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_drive_rs::NodeKind;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

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

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn node(link: &str, _parent: &str, name: &str, _is_dir: bool) -> Node {
        Node {
            uid: uid(link),
            parent_uid: Some(uid("root")),
            kind: NodeKind::File {
                media_type: "text/plain".into(),
                total_size_on_storage: 0,
                active_revision_state: None,
                active_revision_id: None,
                content_sha1: None,
                claimed_size: Some(0),
                claimed_modification_time: None,
            },
            name: name.into(),
            creation_time: 100,
            modification_time: 100,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            membership: None,
            photo: None,
            album: None,
            verification: Default::default(),
            direct_role: None,
            share_id: None,
        }
    }

    fn handle(len: u64, base_size: u64) -> WriteHandle {
        let path = std::env::temp_dir().join(format!("pdfs-handle-test-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        WriteHandle {
            ino: 2,
            uid: uid("file"),
            file: Arc::new(file),
            path,
            written: Intervals::default(),
            len,
            base_size,
            base_mtime: 0,
            base_revision_id: None,
            dirty: false,
            open_count: 1,
        }
    }

    #[test]
    fn a_resize_the_scratch_file_refuses_changes_nothing() {
        let mut h = handle(4, 4);
        h.written.add(0, 4);
        // A read-only descriptor refuses `ftruncate`, as a full disk does a grow.
        let path = std::env::temp_dir().join(format!("pdfs-resize-test-{}", std::process::id()));
        std::fs::write(&path, b"kept").unwrap();
        h.file = Arc::new(File::open(&path).unwrap());
        let _ = std::fs::remove_file(&path);

        assert!(h.resize(10).is_err());
        assert_eq!((h.len, h.dirty), (4, false));
        assert_eq!(h.written.segments(4, 10), vec![(4, 10, false)]);

        let mut h = handle(4, 4);
        h.resize(10).unwrap();
        assert_eq!((h.len, h.dirty), (10, true));
        assert_eq!(h.written.segments(4, 10), vec![(4, 10, true)]);
    }

    /// A file created offline is written while its queued create lands. The
    /// placeholder had no revision, so the handle takes the empty file the
    /// create made as its base (docs/BUGS.md B113).
    #[test]
    fn a_handle_on_a_placeholder_is_based_on_the_create_that_landed() {
        let mut h = handle(0, 0);
        assert_eq!(h.base_revision_id, None);
        let mut landed = node("real", "root", "f.mkv", false);
        landed.modification_time = 160;
        if let NodeKind::File {
            active_revision_id, ..
        } = &mut landed.kind
        {
            *active_revision_id = Some("empty".into());
        }
        h.rebase_onto(&landed);
        assert_eq!(h.base_mtime, 160);
        assert_eq!(h.base_size, 0);
        assert_eq!(h.base_revision_id.as_deref(), Some("empty"));
    }

    #[test]
    fn a_write_past_the_end_authors_the_hole_it_leaves() {
        // Opened over a 100-byte base, shrunk to 10, then written at 50: bytes
        // 10..50 are zeros now, not the base's bytes 10..50.
        let mut h = handle(100, 100);
        h.written.clip(10);
        h.len = 10;
        h.record_write(50, 60);
        assert_eq!(h.len, 60);
        assert_eq!(
            h.written.segments(0, 60),
            vec![(0, 10, false), (10, 60, true)],
            "only the head still comes from the base"
        );
    }

    #[test]
    fn a_write_inside_the_file_leaves_the_rest_to_the_base() {
        let mut h = handle(100, 100);
        h.record_write(40, 50);
        assert_eq!(h.len, 100);
        assert_eq!(
            h.written.segments(0, 100),
            vec![(0, 40, false), (40, 50, true), (50, 100, false)]
        );
    }

    #[test]
    fn a_write_to_a_full_disk_is_no_space() {
        let full = File::options().write(true).open("/dev/full").unwrap();
        assert!(matches!(
            stage_write(&full, 0, b"bytes"),
            Err(WriteError::NoSpace)
        ));
    }

    #[test]
    fn the_last_release_hands_back_the_write_and_marks_it_releasing() {
        let mut open = OpenFiles::default();
        // As a fresh open starts it: no handle shares it yet.
        let fresh = || WriteHandle {
            open_count: 0,
            ..handle(0, 0)
        };
        let first = open.open_writer(2, fresh);
        let second = open.open_writer(2, || unreachable!("the state is shared"));
        assert_ne!(first, second);
        assert!(open.is_busy(2));

        assert!(matches!(
            open.release(first),
            Released::StillOpen { ino: 2 }
        ));
        assert!(open.is_writing(2));
        let Released::Last(writer) = open.release(second) else {
            panic!("the second release is the last");
        };
        assert_eq!(writer.ino, 2);
        assert!(!open.is_writing(2));
        assert_eq!(open.owed(false).collect::<Vec<_>>(), [2]);

        open.finish_release(2);
        assert_eq!(open.owed(false).count(), 0);
        assert!(matches!(open.release(second), Released::NotWriting));
    }

    #[test]
    fn a_directory_handle_never_takes_a_file_handle_number() {
        let mut open = OpenFiles::default();
        let dir = open.next_fh();
        let file = open.open_writer(2, || handle(0, 0));
        assert_ne!(dir, file);
        assert_eq!(open.ino_of(file), Some(2));
        assert_eq!(open.ino_of(dir), None);
    }
}
