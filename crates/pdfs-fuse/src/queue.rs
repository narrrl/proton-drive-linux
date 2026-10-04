//! The user's view of the pending-op queue: what has not reached the remote
//! yet, why, and a way to stop waiting out a backoff.
//!
//! Read-only apart from "retry now", which only moves a row's next-attempt time,
//! "export", which copies a staged blob out, and "discard". Only a discard the
//! user asks for drops a queued op: its staged blob is the only copy of the
//! user's write (see `docs/RECOVERY.md`), so it is the user's call, made after
//! an export or for a change with no content.

use super::*;
use std::io::Write;

use pdfs_core::control::{PendingOpInfo, SyncIssue};
use pdfs_core::db::{FAILING_ATTEMPTS, PARK_UNTIL};

impl Core {
    /// Every queued op, oldest first, with a path a person can read.
    pub(crate) fn pending_op_infos(&self) -> CoreResult<Vec<PendingOpInfo>> {
        let ops = self
            .db
            .pending_ops()
            .map_err(|e| CoreError::internal(format!("listing the queue: {e}")))?;
        let issues = self
            .db
            .op_issues()
            .map_err(|e| CoreError::internal(format!("listing sync issues: {e}")))?;
        Ok(ops
            .into_iter()
            .map(|op| {
                let parked = op.next_attempt_at >= PARK_UNTIL;
                let exportable = exportable(&op).is_some();
                let path = self
                    .db
                    .node_path(&op.uid)
                    .ok()
                    .flatten()
                    .filter(|path| !path.is_empty())
                    .or_else(|| op.name.clone())
                    .unwrap_or_else(|| op.uid.clone());
                PendingOpInfo {
                    id: op.id,
                    kind: op.kind,
                    path,
                    attempts: op.attempts,
                    last_error: op.last_error,
                    queued_at: op.created_at / 1000,
                    next_attempt_at: (!parked).then_some(op.next_attempt_at / 1000),
                    parked,
                    failing: op.attempts >= FAILING_ATTEMPTS || issues.contains_key(&op.id),
                    issue: issues.get(&op.id).map(|issue| SyncIssue::parse(issue)),
                    exportable,
                }
            })
            .collect())
    }

    /// Retry one backed-off op now, or every failed one when `id` is `None`.
    /// Returns how many ops were moved up.
    pub(crate) fn retry_pending_ops(&self, id: Option<i64>) -> CoreResult<usize> {
        let now = now_millis();
        let moved = match id {
            Some(id) => self.db.retry_op_now(id, now).map(usize::from),
            None => self.db.retry_failed_ops_now(now),
        }
        .map_err(|e| CoreError::internal(format!("retrying the queue: {e}")))?;
        if moved > 0 {
            self.wake_drain();
        }
        Ok(moved)
    }

    /// Copy the content of queued op `id` to `dest`, which must not exist yet.
    /// The op stays queued. Returns the bytes written.
    pub(crate) fn export_pending_op(&self, id: i64, dest: &Path) -> CoreResult<u64> {
        if !dest.is_absolute() {
            return Err(CoreError::invalid(format!(
                "destination must be an absolute path: {}",
                dest.display()
            )));
        }
        let ops = self
            .db
            .pending_ops()
            .map_err(|e| CoreError::internal(format!("listing the queue: {e}")))?;
        let op = ops
            .iter()
            .find(|op| op.id == id)
            .ok_or_else(|| CoreError::not_found(format!("no queued change {id}")))?;
        let blob = exportable(op).ok_or_else(|| {
            CoreError::invalid(format!("queued change {id} carries no file content"))
        })?;
        let written = copy_to_new_file(blob, dest)?;
        info!(id, dest = %dest.display(), written, "exported a queued change's content");
        Ok(written)
    }

    /// Drop queued op `id` and undo the local change it stood for. Returns the
    /// node's path.
    ///
    /// A node only made here goes, as if it had been deleted before it was
    /// sent. A node Drive has goes back to what Drive has: every change queued
    /// on it goes, since each was made on top of the one before. What is queued
    /// below it stays.
    pub(crate) fn discard_pending_op(&self, id: i64) -> CoreResult<String> {
        let ops = self
            .db
            .pending_ops()
            .map_err(|e| CoreError::internal(format!("listing the queue: {e}")))?;
        let op = ops
            .into_iter()
            .find(|op| op.id == id)
            .ok_or_else(|| CoreError::not_found(format!("no queued change {id}")))?;
        let uid = parse_node_uid(&op.uid)
            .ok_or_else(|| CoreError::internal(format!("queued change {id} names {}", op.uid)))?;
        let path = self
            .db
            .node_path(&op.uid)
            .ok()
            .flatten()
            .filter(|path| !path.is_empty())
            .or(op.name)
            .unwrap_or_else(|| op.uid.clone());
        if self.is_open_anywhere(&uid) {
            return Err(CoreError::conflict(format!(
                "{path} is open; close it first"
            )));
        }
        if is_local_uid(&uid) {
            self.discard_local_node(&uid, &path)?;
        } else {
            self.discard_remote_change(&uid, &path)?;
        }
        info!(id, %uid, path, "discarded a queued change");
        Ok(path)
    }

    /// Forget a node made here, and the ops that would have sent it.
    fn discard_local_node(&self, uid: &NodeUid, path: &str) -> CoreResult<()> {
        // Its children's ops go with its own, and their content with them.
        if self
            .db
            .has_children(uid)
            .map_err(|e| CoreError::internal(format!("listing {path}: {e}")))?
        {
            return Err(CoreError::conflict(format!(
                "{path} is not empty; discard what is in it first"
            )));
        }
        self.discard_queued_ops(uid)
            .map_err(|_| CoreError::internal(format!("dropping the changes to {path}")))?;
        self.for_each_mount(|st, notify| {
            background::forget_and_notify(st, notify, uid);
        });
        self.cache.evict(uid);
        self.evict_reader(uid);
        Ok(())
    }

    /// Put a node Drive has back as Drive has it.
    fn discard_remote_change(&self, uid: &NodeUid, path: &str) -> CoreResult<()> {
        // Asked first: the change is only dropped once what replaces it is known.
        let node = self
            .fetch_node_remote(uid)
            .map_err(|e| CoreError::from_api(&e, "reading the node from Drive"))?
            .filter(|node| !node.trashed);
        self.cancel_upload(uid);
        let blobs = self
            .db
            .discard_ops_on_node(&uid.to_string())
            .map_err(|e| CoreError::internal(format!("dropping the changes to {path}: {e}")))?
            .ok_or_else(|| {
                CoreError::conflict(format!("{path} is being sent right now; try again"))
            })?;
        self.release_dropped_ops(uid, blobs);
        self.hidden.lock().remove(uid);
        match node {
            Some(node) => self.for_each_mount(|st, notify| {
                put_back(st, notify, node);
            }),
            None => self.for_each_mount(|st, notify| {
                background::forget_and_notify(st, notify, uid);
            }),
        }
        self.cache.evict(uid);
        self.evict_reader(uid);
        Ok(())
    }
}

/// Show `node` as Drive has it, where Drive has it, under the inode it has.
fn put_back(st: &mut State, notify: &mut NotifyBatch, node: Node) {
    let parent = node.parent_uid.as_ref().and_then(|parent| {
        st.by_uid
            .get(parent)
            .copied()
            .map(|ino| (ino, parent.clone()))
    });
    let here = st.by_uid.get(&node.uid).and_then(|ino| {
        st.entries
            .get(ino)
            .map(|entry| (*ino, entry.parent, entry.node.name.clone()))
    });
    match (here, parent) {
        (Some((ino, from, name)), Some((to, to_uid))) => {
            st.relocate(ino, from, to, &to_uid, &node.name);
            st.write_through(node.clone());
            st.intern_mem(to, node);
            notify.inval_entry(from, name);
            notify.inval_inode(ino);
        }
        (here, parent) => {
            if let Some((_, from, name)) = here {
                st.forget_mem(&node.uid);
                st.invalidate_listing(from);
                notify.inval_entry(from, name);
            }
            if let Some((to, _)) = parent {
                st.invalidate_listing(to);
                notify.inval_entry(to, node.name.clone());
            }
            st.write_through(node);
        }
    }
}

/// Copy `blob` to `dest`, which must not exist: never replace something the
/// user already has.
fn copy_to_new_file(blob: &Path, dest: &Path) -> CoreResult<u64> {
    let mut source = File::open(blob)
        .map_err(|e| CoreError::internal(format!("open {}: {e}", blob.display())))?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                CoreError::invalid(format!("{} already exists", dest.display()))
            }
            _ => CoreError::internal(format!("create {}: {e}", dest.display())),
        })?;
    std::io::copy(&mut source, &mut out)
        .and_then(|written| out.flush().and(out.sync_all()).map(|()| written))
        .map_err(|e| CoreError::internal(format!("write {}: {e}", dest.display())))
}

/// The staged blob of an op that carries file content.
fn exportable(op: &PendingOp) -> Option<&Path> {
    matches!(op.kind.as_str(), OP_CREATE | OP_REVISION)
        .then_some(op.blob_path.as_deref())
        .flatten()
        .map(Path::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(kind: &str, blob: Option<&str>) -> PendingOp {
        PendingOp {
            id: 1,
            kind: kind.to_string(),
            uid: "vol~a".to_string(),
            parent_uid: None,
            name: None,
            blob_path: blob.map(str::to_string),
            meta_json: None,
            created_at: 0,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        }
    }

    #[test]
    fn only_a_change_that_carries_file_content_can_be_exported() {
        assert!(exportable(&op(OP_CREATE, Some("/staging/a"))).is_some());
        assert!(exportable(&op(OP_REVISION, Some("/staging/a"))).is_some());
        assert!(exportable(&op(OP_CREATE, None)).is_none());
        assert!(exportable(&op(OP_RENAME, None)).is_none());
        assert!(exportable(&op(OP_TRASH, None)).is_none());
    }

    #[test]
    fn an_export_copies_the_bytes_and_never_replaces_a_file() {
        let dir = std::env::temp_dir().join(format!("pdfs-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blob = dir.join("blob");
        std::fs::write(&blob, b"queued bytes").unwrap();

        let dest = dir.join("copy.txt");
        assert_eq!(copy_to_new_file(&blob, &dest).unwrap(), 12);
        assert_eq!(std::fs::read(&dest).unwrap(), b"queued bytes");

        std::fs::write(&dest, b"the user's own").unwrap();
        assert!(copy_to_new_file(&blob, &dest).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"the user's own");
        // The staged blob is the only copy of the change: an export leaves it.
        assert_eq!(std::fs::read(&blob).unwrap(), b"queued bytes");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
