//! Inode bookkeeping: the maps that give a remote node a stable kernel inode,
//! the open write handles, and the interval set that tracks which bytes of a
//! staged write are authored locally.

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use pdfs_core::cache::StagedWrite;
use pdfs_core::db::Db;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use tracing::warn;

/// A node known to the filesystem, addressed by its kernel inode.
pub(crate) struct Entry {
    pub(crate) uid: NodeUid,
    pub(crate) parent: u64,
    pub(crate) node: Node,
    pub(crate) lookup_count: u64,
    pub(crate) open_count: u32,
    pub(crate) unlinked: bool,
}

/// A set of non-overlapping `[start, end)` byte ranges, kept sorted and merged.
/// Tracks which bytes of a [`WriteHandle`]'s scratch file were authored locally
/// (vs. still living only in the remote base), so reads and the commit gap-fill
/// know which regions to pull from the network.
#[derive(Clone, Default)]
pub(crate) struct Intervals(pub(crate) Vec<(u64, u64)>);

impl Intervals {
    /// Mark `[start, end)` as authored, coalescing with any touching ranges.
    pub(crate) fn add(&mut self, start: u64, end: u64) {
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
    pub(crate) fn clip(&mut self, len: u64) {
        self.0.retain(|&(s, _)| s < len);
        for iv in &mut self.0 {
            iv.1 = iv.1.min(len);
        }
    }

    /// Split `[start, end)` into contiguous `(s, e, authored)` segments, in
    /// order. `authored == true` means the bytes live in the scratch file;
    /// `false` means they must come from the remote base (or are a hole).
    pub(crate) fn segments(&self, start: u64, end: u64) -> Vec<(u64, u64, bool)> {
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
pub(crate) struct WriteHandle {
    pub(crate) ino: u64,
    pub(crate) uid: NodeUid,
    /// Disk-backed staging buffer. Shared (`Arc`) so reads can use it without
    /// holding the state lock across I/O. Accessed positionally (`read_at`/
    /// `write_at`), so a clone never disturbs another's file offset.
    pub(crate) file: Arc<File>,
    /// Scratch file path, removed on release.
    pub(crate) path: PathBuf,
    /// Byte ranges authored into `file`. Everything else in `[0, len)` is base.
    pub(crate) written: Intervals,
    /// Logical file size (may exceed authored bytes after a truncate-extend).
    pub(crate) len: u64,
    /// Size of the remote base at open, for serving untouched ranges.
    pub(crate) base_size: u64,
    /// Modification time of the remote base at open, validating its block cache.
    pub(crate) base_mtime: i64,
    /// Whether anything diverged from the remote and needs an upload.
    pub(crate) dirty: bool,
    /// Number of file handles currently sharing this scratch state.
    pub(crate) open_count: usize,
}

/// A released write whose upload has not happened yet (offline.md Phase 3).
///
/// The bytes live in the content cache's staging dir and the intent lives in the
/// `pending_op` table; this pairs them in memory so a read can be served without
/// a database round trip.
#[derive(Clone)]
pub(crate) struct PendingRevision {
    /// Staged blob holding the written bytes.
    pub(crate) path: PathBuf,
    /// Which of those bytes are real, and what base the gaps refer to.
    pub(crate) meta: StagedWrite,
}

/// Mutable inode bookkeeping, guarded by a mutex because fuser drives the
/// `Filesystem` trait through `&self`.
pub(crate) struct State {
    /// inode -> node metadata.
    pub(crate) entries: HashMap<u64, Entry>,
    /// Dedupe inodes by node uid so a node keeps a stable inode across lookups.
    pub(crate) by_uid: HashMap<NodeUid, u64>,
    /// Cached directory listings: parent inode -> child inodes. Presence of a
    /// key means the directory has been enumerated.
    pub(crate) children: HashMap<u64, Vec<u64>>,
    pub(crate) next_ino: u64,
    /// Shared write state keyed by inode. Concurrent writers share the scratch file.
    pub(crate) active_writes: HashMap<u64, WriteHandle>,
    /// Maps file handle id (fh) to inode (ino). Read-only opens use fh 0 and
    /// have no entry here.
    pub(crate) handles: HashMap<u64, u64>,
    pub(crate) next_fh: u64,
    /// Unified SQLite metadata cache. Every map mutation below writes through to
    /// it inside the `State` lock so the DB stays the authoritative copy across
    /// restarts (see plan.md P1).
    pub(crate) db: Arc<Db>,
}

impl State {
    pub(crate) fn intern(&mut self, parent: u64, node: Node) -> u64 {
        if let Err(e) = self.db.upsert_node(&node) {
            warn!(uid = %node.uid, error = %e, "db upsert_node failed");
        }
        self.intern_mem(parent, node)
    }

    /// Allocate (or reuse) a stable inode for a node, updating the hot-cache maps
    /// only. Every caller owes the DB a write-through — see the callers below;
    /// the split exists so a batch can pay for one transaction instead of `n`.
    pub(crate) fn intern_mem(&mut self, parent: u64, node: Node) -> u64 {
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
                lookup_count: 1,
                open_count: 0,
                unlinked: false,
            },
        );
        ino
    }

    /// Decrement lookup count for an inode and prune if lookup_count == 0 && open_count == 0 && unlinked.
    pub(crate) fn forget_lookup(&mut self, ino: u64, nlookup: u64) -> Option<(u64, String)> {
        if let Some(entry) = self.entries.get_mut(&ino) {
            entry.lookup_count = entry.lookup_count.saturating_sub(nlookup);
            if entry.lookup_count == 0 && entry.open_count == 0 && entry.unlinked {
                let uid = entry.uid.clone();
                return self.forget(&uid);
            }
        }
        None
    }

    /// Allocate (or reuse) a stable inode for a node that came *from* the
    /// database, which is why nothing is written back.
    pub(crate) fn intern_from_db(&mut self, parent: u64, node: Node) -> u64 {
        self.intern_mem(parent, node)
    }

    /// Allocate (or reuse) stable inodes for a whole listing, writing every node
    /// through in a single transaction.
    ///
    /// This is what keeps `ls` on a large folder quick: one commit for the
    /// listing, rather than one autocommit — and one fsync — per child.
    pub(crate) fn intern_batch(&mut self, parent: u64, nodes: Vec<Node>) -> Vec<u64> {
        if let Err(e) = self.db.upsert_nodes(&nodes) {
            warn!(error = %e, "db upsert_nodes failed");
        }
        nodes
            .into_iter()
            .map(|node| self.intern_mem(parent, node))
            .collect()
    }

    /// Check if a directory inode has any child nodes in memory or in the database.
    pub(crate) fn has_children(&self, parent: u64) -> bool {
        if let Some(kids) = self.children.get(&parent)
            && !kids.is_empty()
        {
            return true;
        }
        if let Some(entry) = self.entries.get(&parent)
            && let Ok(has_kids) = self.db.has_children(&entry.uid)
        {
            return has_kids;
        }
        false
    }

    /// Check if `ancestor_ino` is `target_ino` or an ancestor of `target_ino` (for rename cycle prevention).
    pub(crate) fn is_ancestor_of(&self, ancestor_ino: u64, mut target_ino: u64) -> bool {
        if ancestor_ino == target_ino {
            return true;
        }
        let mut visited = std::collections::HashSet::new();
        visited.insert(target_ino);
        while let Some(entry) = self.entries.get(&target_ino) {
            let parent = entry.parent;
            if parent == ancestor_ino {
                return true;
            }
            if parent == 0 || !visited.insert(parent) {
                break;
            }
            target_ino = parent;
        }
        false
    }

    /// Forget a node or, if open handles exist (open_count > 0), mark it unlinked
    /// and remove it from parent children so lookups fail while open reads succeed.
    pub(crate) fn forget_or_unlink(&mut self, uid: &NodeUid) -> Option<(u64, String)> {
        if let Some(&ino) = self.by_uid.get(uid)
            && let Some(entry) = self.entries.get_mut(&ino)
            && entry.open_count > 0
        {
            entry.unlinked = true;
            if let Some(kids) = self.children.get_mut(&entry.parent) {
                kids.retain(|&k| k != ino);
            }
            return Some((entry.parent, entry.node.name.clone()));
        }
        self.forget(uid)
    }

    /// Forget a node entirely: drop its inode, its uid mapping, its own cached
    /// listing, its slot in its parent's listing, and its DB row. Returns
    /// `(parent_ino, name)` when the node was known, so the caller can notify
    /// the kernel.
    pub(crate) fn forget(&mut self, uid: &NodeUid) -> Option<(u64, String)> {
        let ino = self.by_uid.remove(uid)?;
        if let Err(e) = self.db.delete_node(uid) {
            warn!(%uid, error = %e, "db delete_node failed");
        }
        let entry = self.entries.remove(&ino)?;
        self.children.remove(&ino);
        if let Some(kids) = self.children.get_mut(&entry.parent) {
            kids.retain(|&k| k != ino);
        }
        Some((entry.parent, entry.node.name))
    }

    /// Move a node to a new parent and/or name within the tree, writing it
    /// through like any other mutation.
    ///
    /// The online rename instead forgets the node and lets the destination
    /// re-enumerate, which is the cheaper way to stay honest about what the
    /// server did. A queued rename cannot: re-enumerating needs the network, and
    /// the server has not been told yet in any case — so this *is* the tree's
    /// new truth until the op drains (offline.md Phase 3b).
    pub(crate) fn rename_in_place(
        &mut self,
        ino: u64,
        new_parent: u64,
        new_parent_uid: &NodeUid,
        name: &str,
    ) {
        let Some(entry) = self.entries.get_mut(&ino) else {
            return;
        };
        let old_parent = entry.parent;
        entry.parent = new_parent;
        entry.node.name = name.to_string();
        entry.node.parent_uid = Some(new_parent_uid.clone());
        let node = entry.node.clone();
        if old_parent != new_parent {
            if let Some(kids) = self.children.get_mut(&old_parent) {
                kids.retain(|&k| k != ino);
            }
            // Only if the destination is listed: pushing into a listing that was
            // never enumerated would invent a one-child folder.
            if let Some(kids) = self.children.get_mut(&new_parent)
                && !kids.contains(&ino)
            {
                kids.push(ino);
            }
        }
        if let Err(e) = self.db.upsert_node(&node) {
            warn!(uid = %node.uid, error = %e, "db upsert_node failed for a queued rename");
        }
    }

    /// Drop a directory's cached child listing and mark it unlisted in the DB,
    /// so the next access re-enumerates instead of trusting a stale listing.
    ///
    /// The DB flag is cleared whether or not the listing was resident. A folder
    /// trimmed from the hot cache but still `listed` in the DB is exactly the
    /// case that needs invalidating — returning early there would leave
    /// `ensure_children` free to rebuild the stale listing from disk.
    pub(crate) fn invalidate_listing(&mut self, ino: u64) {
        self.children.remove(&ino);
        if let Some(e) = self.entries.get(&ino) {
            let uid = e.uid.clone();
            if let Err(err) = self.db.set_listed(&uid, false) {
                warn!(%uid, error = %err, "db set_listed(false) failed");
            }
        }
    }

    /// Settle the local state after a node has been moved (and possibly renamed)
    /// on the remote: rewrite it in place, and drop **both** directories'
    /// listings so each re-enumerates.
    ///
    /// Both, because each is stale for its own reason. The destination has
    /// gained a child it does not know about; the source has lost one. The
    /// source is the subtler half, and was audit A5: pruning the moved node from
    /// the source's in-memory children looks sufficient while that entry stays
    /// resident, but the source's DB row is left `listed = 1`. Once it is
    /// evicted from the hot cache, or the daemon restarts, `ensure_children`
    /// rebuilds the listing from the DB and declares it complete, so anything
    /// else that changed remotely under the source since is never seen.
    ///
    /// What this must *not* do is `forget` the node. Forgetting drops its
    /// `by_uid` mapping, so the re-enumeration hands it a fresh inode — while
    /// the kernel has already carried the renamed dentry over to the *old* one.
    /// Every lookup through that dentry then resolves to an inode `entries` no
    /// longer holds and fails `ENOENT`, so the renamed directory reads as
    /// missing (`ls` on it errors while `ls` of its parent lists it) until the
    /// entry TTL expires. Keeping the inode keeps the kernel's dentry valid.
    ///
    /// A pure rename (`from == to`) invalidates that one directory once.
    pub(crate) fn relocate(
        &mut self,
        ino: u64,
        from_parent: u64,
        to_parent: u64,
        to_parent_uid: &NodeUid,
        name: &str,
    ) {
        self.rename_in_place(ino, to_parent, to_parent_uid, name);
        self.invalidate_listing(to_parent);
        if from_parent != to_parent {
            self.invalidate_listing(from_parent);
        }
    }

    /// Update a file entry's recorded plaintext size so `getattr` reflects an
    /// in-progress write before the new revision is sealed.
    pub(crate) fn set_size(&mut self, ino: u64, size: u64) {
        if let Some(e) = self.entries.get_mut(&ino)
            && let NodeKind::File { claimed_size, .. } = &mut e.node.kind
        {
            *claimed_size = Some(size as i64);
        }
    }

    /// Update a file entry's modification time (epoch seconds).
    pub(crate) fn touch_mtime(&mut self, ino: u64, secs: i64) {
        if let Some(e) = self.entries.get_mut(&ino) {
            e.node.modification_time = secs;
        }
    }

    /// Record the size and mtime of a write that has been accepted but not yet
    /// uploaded, persisting them like any other node mutation.
    ///
    /// The write-through is what makes the new size outlive the process: until
    /// the op drains, the remote still holds the old revision (or, for a node
    /// created offline, nothing at all), so this row is the only record that the
    /// file is as long as the caller was told it is. Without it a restart serves
    /// the stale size and the file reads as truncated — or empty — while its
    /// bytes sit safely in staging (offline.md Phase 3).
    pub(crate) fn record_pending_write(&mut self, ino: u64, size: u64, mtime: i64) {
        self.set_size(ino, size);
        self.touch_mtime(ino, mtime);
        if let Some(e) = self.entries.get(&ino)
            && let Err(err) = self.db.upsert_node(&e.node)
        {
            warn!(uid = %e.uid, error = %err, "db upsert_node failed for a queued write");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn node(link: &str, parent: &str, name: &str, is_dir: bool) -> Node {
        Node {
            uid: uid(link),
            parent_uid: Some(uid(parent)),
            kind: if is_dir {
                NodeKind::Folder
            } else {
                NodeKind::File {
                    media_type: "text/plain".into(),
                    total_size_on_storage: 0,
                    active_revision_state: None,
                    claimed_size: Some(0),
                    claimed_modification_time: None,
                }
            },
            name: name.into(),
            creation_time: 100,
            modification_time: 100,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            verification: Default::default(),
        }
    }

    /// A unique temp directory removed on drop; avoids a dev-dependency.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-state-{}-{}",
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

    /// `Db::open_in_memory` is `#[cfg(test)]` inside `pdfs-core`, so it does not
    /// exist for this crate's tests — a temp file is the equivalent here. The
    /// directory outlives the state because the state holds the open database.
    fn state() -> (State, TempDir) {
        let dir = TempDir::new();
        let db = Db::open(&dir.0.join("cache.db")).unwrap();
        let st = State {
            entries: HashMap::new(),
            by_uid: HashMap::new(),
            children: HashMap::new(),
            next_ino: 1,
            active_writes: HashMap::new(),
            handles: HashMap::new(),
            next_fh: 1,
            db: Arc::new(db),
        };
        (st, dir)
    }

    /// Two folders, each holding one file, both listed — the state a move
    /// starts from. Returns `(state, src_ino, dst_ino)`.
    fn two_folders() -> (State, TempDir, u64, u64) {
        let (mut st, dir) = state();
        let src = st.intern(0, node("src", "root", "src", true));
        let dst = st.intern(0, node("dst", "root", "dst", true));
        let f = st.intern(src, node("f", "src", "f.txt", false));
        st.children.insert(src, vec![f]);
        st.children.insert(dst, vec![]);
        st.db.set_listed(&uid("src"), true).unwrap();
        st.db.set_listed(&uid("dst"), true).unwrap();
        (st, dir, src, dst)
    }

    /// Audit A5. Moving a file out of a folder leaves that folder's DB row
    /// claiming a complete listing, so once the hot cache drops it, remote
    /// changes under it stop being seen. `relocate` has to clear both sides.
    #[test]
    fn relocate_invalidates_the_source_as_well_as_the_destination() {
        let (mut st, _dir, src, dst) = two_folders();
        let f = st.by_uid[&uid("f")];
        st.relocate(f, src, dst, &uid("dst"), "f.txt");

        assert!(
            st.db.children_if_listed(&uid("dst")).unwrap().is_none(),
            "destination re-enumerates: it gained a child whose row was just deleted"
        );
        assert!(
            st.db.children_if_listed(&uid("src")).unwrap().is_none(),
            "source re-enumerates: its listing predates the move, and a stale \
             `listed` flag would hide every later remote change under it"
        );
        assert!(!st.children.contains_key(&src));
        assert!(!st.children.contains_key(&dst));
        assert_eq!(
            st.by_uid.get(&uid("f")),
            Some(&f),
            "the inode survives the move: the kernel has already pointed the \
             renamed dentry at it, so re-interning under a fresh one would make \
             every lookup through that dentry ENOENT"
        );
        assert_eq!(st.entries[&f].parent, dst);
    }

    /// A pure rename never leaves the directory, so there is one listing to
    /// drop, not two — and dropping it must not depend on the parents differing.
    #[test]
    fn relocate_within_one_directory_still_invalidates_it() {
        let (mut st, _dir, src, _dst) = two_folders();
        let f = st.by_uid[&uid("f")];
        st.relocate(f, src, src, &uid("src"), "renamed.txt");
        assert!(st.db.children_if_listed(&uid("src")).unwrap().is_none());
        assert!(!st.children.contains_key(&src));
        assert_eq!(st.by_uid.get(&uid("f")), Some(&f), "the inode is stable");
        assert_eq!(st.entries[&f].node.name, "renamed.txt");
    }

    /// The failure mode the fix exists for, stated directly: forgetting the node
    /// prunes it from the resident listing but leaves the DB claiming the source
    /// is fully enumerated. This pins *why* `relocate` cannot just call `forget`.
    #[test]
    fn forget_alone_leaves_the_source_claiming_a_complete_listing() {
        let (mut st, _dir, _src, _dst) = two_folders();
        st.forget(&uid("f"));
        let listed = st.db.children_if_listed(&uid("src")).unwrap();
        assert!(
            listed.is_some(),
            "forget does not clear the flag — which is exactly why relocate must"
        );
        assert!(
            listed.unwrap().is_empty(),
            "and the listing it would serve is the moved file's absence, \
             with no way to notice anything else changed"
        );
    }

    #[test]
    fn test_is_ancestor_of_hierarchy() {
        let (mut st, _dir) = state();
        let root = st.intern(0, node("root_id", "none", "root", true));
        let p1 = st.intern(root, node("p1_id", "root_id", "p1", true));
        let p2 = st.intern(p1, node("p2_id", "p1_id", "p2", true));
        let child = st.intern(p2, node("child_id", "p2_id", "child", true));

        assert!(st.is_ancestor_of(root, root), "self is ancestor");
        assert!(st.is_ancestor_of(root, p1), "root is ancestor of p1");
        assert!(
            st.is_ancestor_of(root, child),
            "root is ancestor of deep child"
        );
        assert!(st.is_ancestor_of(p1, child), "p1 is ancestor of deep child");
        assert!(
            !st.is_ancestor_of(child, root),
            "child is not ancestor of root"
        );
        assert!(!st.is_ancestor_of(child, p1), "child is not ancestor of p1");
    }

    #[test]
    fn test_state_has_children_mem_and_db() {
        let (mut st, _dir) = state();
        let folder = st.intern_mem(0, node("dir_uid", "none", "dir", true));
        assert!(!st.has_children(folder), "initially empty");

        // Memory-only children (using intern_mem so DB is not populated yet)
        let f1 = st.intern_mem(folder, node("file1_uid", "dir_uid", "file1.txt", false));
        st.children.insert(folder, vec![f1]);
        assert!(st.has_children(folder), "has child in memory");

        st.children.insert(folder, vec![]);
        assert!(!st.has_children(folder), "cleared memory children");

        // DB children
        st.db
            .upsert_node(&node("file2_uid", "dir_uid", "file2.txt", false))
            .unwrap();
        assert!(st.has_children(folder), "has child in db");
    }
}
