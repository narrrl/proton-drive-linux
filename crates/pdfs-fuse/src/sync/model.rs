//! Reconciliation snapshots, queued operations, and pass outcomes.

use super::*;

/// What a reconcile pass remembers about the local side of one path: enough to
/// recognise the same content again, and no more.
///
/// `mtime_ns` is the whole reason this is a struct rather than a pair — see
/// [`StoredSyncEntry::local_mtime_ns`](pdfs_core::db::StoredSyncEntry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LocalSig {
    pub(super) mtime: i64,
    pub(super) mtime_ns: Option<i64>,
    pub(super) size: i64,
}

impl LocalSig {
    /// Whether these two observations describe the same local content.
    ///
    /// Nanoseconds decide it when both sides have them; otherwise the comparison
    /// falls back to whole seconds, which is what a baseline row written before
    /// schema 28 — or a filesystem that reports no sub-second time — can support
    /// (`docs/BUGS.md` B25).
    pub(super) fn same_content(&self, other: &Self) -> bool {
        if self.size != other.size {
            return false;
        }
        match (self.mtime_ns, other.mtime_ns) {
            (Some(a), Some(b)) => a == b,
            _ => self.mtime == other.mtime,
        }
    }
}

impl From<&LocalItem> for LocalSig {
    fn from(item: &LocalItem) -> Self {
        Self {
            mtime: item.mtime,
            mtime_ns: item.mtime_ns,
            size: item.size,
        }
    }
}

impl From<&std::fs::Metadata> for LocalSig {
    fn from(meta: &std::fs::Metadata) -> Self {
        Self {
            mtime: system_mtime(meta),
            mtime_ns: system_mtime_ns(meta),
            size: meta.len() as i64,
        }
    }
}

/// The baseline's record of the local side, as [`LocalSig`] compares it.
impl From<&StoredSyncEntry> for LocalSig {
    fn from(entry: &StoredSyncEntry) -> Self {
        Self {
            mtime: entry.local_mtime,
            mtime_ns: entry.local_mtime_ns,
            size: entry.local_size,
        }
    }
}

pub(super) struct LocalItem {
    pub(super) is_dir: bool,
    pub(super) mtime: i64,
    /// [`LocalItem::mtime`] to nanosecond precision, when the filesystem
    /// reports one.
    pub(super) mtime_ns: Option<i64>,
    pub(super) size: i64,
    /// True when another process holds this file open for writing (detected via
    /// `/proc/*/fd`). An open-for-write file is kept in the map — so it is not
    /// misread as a deletion — but treated as unchanged for upload purposes,
    /// deferring the upload until the writer closes the file.
    pub(super) open_for_write: bool,
}

/// One item found while walking a remote tree.
pub(super) struct RemoteItem {
    pub(super) uid: NodeUid,
    pub(super) is_dir: bool,
    pub(super) mtime: i64,
    pub(super) size: i64,
    /// The active revision's id. The light walk carries it for every file, so
    /// it is the signal that tells another client's new revision from a link
    /// merely re-stamped.
    pub(super) revision: Option<String>,
    /// The active revision's content SHA-1, read only for files the walk
    /// enumerated in full (no baseline, or a moved mtime). `None` is never
    /// proof of anything.
    pub(super) sha1: Option<String>,
}

/// What a pass saw of a remote file: the signature its baseline records once
/// the file is synced, and what an upload checks Drive still holds before it
/// supersedes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RemoteSeen {
    pub(super) mtime: i64,
    pub(super) size: i64,
    pub(super) revision: Option<String>,
}

impl RemoteSeen {
    /// Whether `node` is still the revision this was taken from. The revision
    /// id decides it when both sides have one; otherwise the modification time
    /// and size, which is all a baseline row from before schema 44 holds.
    pub(super) fn still_current(&self, node: &Node) -> bool {
        match (&self.revision, crate::node_revision_id(node)) {
            (Some(seen), Some(now)) => *seen == now,
            _ => (self.mtime, self.size) == remote_file_sig(node),
        }
    }
}

impl From<&RemoteItem> for RemoteSeen {
    fn from(item: &RemoteItem) -> Self {
        Self {
            mtime: item.mtime,
            size: item.size,
            revision: item.revision.clone(),
        }
    }
}

/// The result of a reconcile pass: what it moved, how many paths were kept as
/// conflict copies, and how many failed to apply (and so still need another
/// pass). The counts drive both the folder's state and its activity summary.
#[derive(Default)]
pub(super) struct Outcome {
    pub(super) uploaded: usize,
    pub(super) downloaded: usize,
    pub(super) created: usize,
    pub(super) deleted: usize,
    pub(super) conflicts: usize,
    pub(super) errors: usize,
    /// Files skipped because another process held them open for writing.
    pub(super) deferred: usize,
    /// Paths the local scan could not read, left out of the pass. Not part of
    /// [`is_empty`](Self::is_empty) or the summary: an unreadable folder stays
    /// unreadable from one poll to the next, so the pass reports it once, when
    /// the set changes, rather than on every poll.
    pub(super) unscanned: Vec<String>,
}

impl Outcome {
    /// Fold in one applied op.
    pub(super) fn record(&mut self, applied: &Applied) {
        match applied {
            Applied::Dir(..) => self.created += 1,
            Applied::Uploaded => self.uploaded += 1,
            Applied::Downloaded => self.downloaded += 1,
            Applied::Conflict => self.conflicts += 1,
            Applied::Vanished | Applied::Adopted => {}
        }
    }

    /// Whether the pass moved nothing at all — the common case on a poll of an
    /// unchanged folder, which should not add a line to the activity feed.
    pub(super) fn is_empty(&self) -> bool {
        self.uploaded == 0
            && self.downloaded == 0
            && self.created == 0
            && self.deleted == 0
            && self.conflicts == 0
            && self.errors == 0
            && self.deferred == 0
    }

    /// A human summary of the pass: "3 uploaded, 1 downloaded, 2 failed".
    pub(super) fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (n, label) in [
            (self.uploaded, "uploaded"),
            (self.downloaded, "downloaded"),
            (self.created, "folder(s) created"),
            (self.deleted, "deleted"),
            (self.conflicts, "conflicted"),
            (self.deferred, "deferred (open for write)"),
            (self.errors, "failed"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {label}"));
            }
        }
        parts.join(", ")
    }
}

/// A network operation queued during classification and run concurrently in a
/// per-depth batch. Parent uids are resolved up front (the parent folder is one
/// depth shallower and already created), so tasks share nothing mutable.
pub(super) enum Pending {
    /// Create a new remote folder under `parent`.
    CreateDir { rel: String, parent: NodeUid },
    /// Upload a brand-new local file into `parent`.
    UploadNew { rel: String, parent: NodeUid },
    /// Upload a changed local file as a new revision of `uid`, if Drive still
    /// holds the revision `seen` describes (B249).
    UploadRevision {
        rel: String,
        uid: NodeUid,
        seen: RemoteSeen,
    },
    /// Download remote `uid` to the local path, stamping `seen.mtime`.
    /// `seen.size` is the remote's reported size, used as the transfer's
    /// expected total.
    ///
    /// `local` is what the planning walk saw at this path, or `None` if nothing
    /// was there. A download can run for minutes, so the apply step revalidates
    /// against it immediately before it renames over the destination: a writer
    /// that finished an edit in the meantime must not be overwritten without a
    /// conflict copy (`docs/BUGS.md` B39).
    Download {
        rel: String,
        uid: NodeUid,
        seen: RemoteSeen,
        local: Option<LocalSig>,
    },
    /// First sync of a path present on both sides at the same size, with a
    /// digest on Drive: hash the local file, and record the baseline if it holds
    /// the same bytes. A different file of the same size goes the way of
    /// [`Pending::Conflict`]: two machines syncing one folder must not read as
    /// in step while they hold different content.
    Adopt {
        rel: String,
        uid: NodeUid,
        seen: RemoteSeen,
        sha1: String,
        local: LocalSig,
    },
    /// Both sides changed: set the local copy aside, then download remote `uid`.
    Conflict {
        rel: String,
        uid: NodeUid,
        seen: RemoteSeen,
    },
    /// Both sides changed during a [`push pass`](Core::push_pass): upload the local
    /// copy into `parent` under a conflict name, leaving the remote file as it is.
    PushConflict { rel: String, parent: NodeUid },
}

impl Pending {
    /// The path this op acts on, relative to the folder root.
    pub(super) fn rel(&self) -> &str {
        match self {
            Pending::CreateDir { rel, .. }
            | Pending::UploadNew { rel, .. }
            | Pending::UploadRevision { rel, .. }
            | Pending::Download { rel, .. }
            | Pending::Adopt { rel, .. }
            | Pending::Conflict { rel, .. }
            | Pending::PushConflict { rel, .. } => rel,
        }
    }

    /// How this op reads in the activity feed.
    pub(super) fn kind(&self) -> ActivityKind {
        match self {
            Pending::CreateDir { .. } => ActivityKind::CreateFolder,
            Pending::UploadNew { .. }
            | Pending::UploadRevision { .. }
            | Pending::PushConflict { .. } => ActivityKind::Upload,
            Pending::Download { .. } | Pending::Adopt { .. } | Pending::Conflict { .. } => {
                ActivityKind::Download
            }
        }
    }

    /// The activity line's detail, which distinguishes ops that share a kind.
    pub(super) fn detail(&self) -> &'static str {
        match self {
            Pending::CreateDir { .. } => "on Drive",
            Pending::UploadNew { .. } => "new file",
            Pending::UploadRevision { .. } => "new version",
            Pending::Download { .. } => "from Drive",
            Pending::Adopt { .. } => "already the same on Drive",
            Pending::Conflict { .. } => "local changes kept as a conflict copy",
            Pending::PushConflict { .. } => "local changes uploaded as a conflict copy",
        }
    }
}

/// Why a full reconcile pass stopped without a result.
pub(super) enum PassAbort {
    /// An on-demand switch was queued while the pass was running. Everything left
    /// to do is work on a local copy about to be evicted, so the pass gives up its
    /// remaining work to a [`push pass`](Core::push_pass) instead. Not a failure —
    /// nothing is left half-applied, because each step is applied whole.
    Interrupted,
    /// The pass could not establish its diff (a walk or the baseline load failed).
    Failed(String),
    /// The daemon is stopping. What the pass sent before the stop has landed or
    /// will be cancelled with the runtime; nothing more is sent, and the next
    /// start's pass picks up from the baseline.
    Stopped,
}

impl From<String> for PassAbort {
    fn from(e: String) -> Self {
        PassAbort::Failed(e)
    }
}
