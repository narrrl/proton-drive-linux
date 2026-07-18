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
    /// Open write handles keyed by file handle id. Read-only opens use fh 0 and
    /// have no entry here.
    pub(crate) handles: HashMap<u64, WriteHandle>,
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
            },
        );
        ino
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
    pub(crate) fn invalidate_listing(&mut self, ino: u64) {
        if self.children.remove(&ino).is_none() {
            return;
        }
        if let Some(e) = self.entries.get(&ino) {
            let uid = e.uid.clone();
            if let Err(err) = self.db.set_listed(&uid, false) {
                warn!(%uid, error = %err, "db set_listed(false) failed");
            }
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
