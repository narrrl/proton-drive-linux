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
    /// (bugs.md B25).
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
}

impl Outcome {
    /// Fold in one applied op.
    pub(super) fn record(&mut self, applied: &Applied) {
        match applied {
            Applied::Dir(..) => self.created += 1,
            Applied::Uploaded => self.uploaded += 1,
            Applied::Downloaded => self.downloaded += 1,
            Applied::Conflict => self.conflicts += 1,
            Applied::Vanished => {}
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
    /// Upload a changed local file as a new revision of `uid`.
    UploadRevision { rel: String, uid: NodeUid },
    /// Download remote `uid` to the local path, stamping `mtime`. `size` is the
    /// remote's reported size, used as the transfer's expected total.
    ///
    /// `local` is what the planning walk saw at this path, or `None` if nothing
    /// was there. A download can run for minutes, so the apply step revalidates
    /// against it immediately before it renames over the destination: a writer
    /// that finished an edit in the meantime must not be overwritten without a
    /// conflict copy (bugs.md B39).
    Download {
        rel: String,
        uid: NodeUid,
        mtime: i64,
        size: i64,
        local: Option<LocalSig>,
    },
    /// Both sides changed: set the local copy aside, then download remote `uid`.
    Conflict {
        rel: String,
        uid: NodeUid,
        mtime: i64,
        size: i64,
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
            Pending::Download { .. } | Pending::Conflict { .. } => ActivityKind::Download,
        }
    }

    /// The activity line's detail, which distinguishes ops that share a kind.
    pub(super) fn detail(&self) -> &'static str {
        match self {
            Pending::CreateDir { .. } => "on Drive",
            Pending::UploadNew { .. } => "new file",
            Pending::UploadRevision { .. } => "new version",
            Pending::Download { .. } => "from Drive",
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
