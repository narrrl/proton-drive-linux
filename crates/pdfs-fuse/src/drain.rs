//! The write-back drain: turning queued mutations into remote calls.
//!
//! Every write the kernel hands us is answered the moment its bytes and its
//! `pending_op` row are on disk (offline.md Phase 3), which is what lets a `cp`
//! into the mount run at disk speed and lets an offline write succeed at all.
//! This module is the other half: the worker that walks that queue and performs
//! the uploads, creates, renames and trashes it recorded.
//!
//! Two invariants matter more than anything else here, because between them
//! they are the only thing standing between a queued write and lost data:
//!
//! 1. A staged blob is the *only* copy of the user`s bytes. It is dropped only
//!    after the op it belongs to has provably landed — never before, and never
//!    on a failure path that might be retried.
//! 2. A failure never pauses the queue. Recording it pushes that op`s
//!    `next_attempt_at` past now, so one file wedged against a vanished parent
//!    cannot hold up an unrelated upload behind it.
//!
//! When the remote has moved on underneath a queued revision, the losing bytes
//! are kept as a conflict copy rather than discarded — see
//! [`Core::revision_conflict`].

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pdfs_core::cache::{Baseline, StagedWrite};
use pdfs_core::control::{ActivityKind, TransferDirection};
use pdfs_core::db::{OP_CREATE, OP_MKDIR, OP_RENAME, OP_REVISION, OP_TRASH, PendingOp};
use proton_drive_rs::Node;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{debug, error, info, warn};

use super::state::Intervals;
use super::transfers::CountingReader;
use super::{
    Core, DRAIN_BACKOFF_MAX, DRAIN_BACKOFF_MIN, DRAIN_IDLE_POLL, ROOT_INO, conflict_name,
    is_already_exists, is_gone, is_local_uid_str, media_type_for, node_size, now_millis, now_secs,
    parse_node_uid,
};

impl Core {
    /// Drain the pending-op queue: the background half of every write
    /// (offline.md Phase 3).
    ///
    /// Runs for the life of the mount. Ops are replayed oldest-first, each
    /// retried with doubling backoff and *never* dropped on failure — the staged
    /// blob is the only copy of the user's bytes, so a failed op stays queued
    /// until it lands or the user deletes the file.
    ///
    /// A failure does not pause the queue. Recording it pushes that op's
    /// `next_attempt_at` past `now`, so the next pass simply picks the next op
    /// that is due — one file wedged against a folder that no longer exists must
    /// not hold up an unrelated upload behind it. The worker only blocks once
    /// nothing is due at all.
    pub(crate) fn run_pending_drain(&self) {
        loop {
            let now = now_millis();
            // One row, chosen by the database. Reading the whole queue to pick
            // one op made a long queue quadratic to drain, and held the shared
            // connection — and so every FUSE metadata call — for the duration.
            let due = self.db.next_due_op(now).unwrap_or_default();

            let Some(op) = due.filter(|_| self.online.load(Ordering::Relaxed)) else {
                self.wait_for_drain_work();
                continue;
            };
            if let Err(e) = self.drain_op(&op) {
                let attempts = op.attempts + 1;
                let backoff = DRAIN_BACKOFF_MIN
                    .saturating_mul(1u32 << attempts.min(6))
                    .min(DRAIN_BACKOFF_MAX);
                warn!(uid = %op.uid, attempts, error = %e, "pending upload failed; will retry");
                if let Err(e) = self.db.record_op_failure(
                    op.id,
                    &e.to_string(),
                    now_millis() + backoff.as_millis() as i64,
                ) {
                    // The backoff is the only thing keeping a failing op from
                    // being picked again immediately, so without it the loop
                    // would spin on this op as fast as the API can refuse it.
                    error!(uid = %op.uid, error = %e, "recording a drain failure failed");
                    self.wait_for_drain_work();
                }
            }
        }
    }

    /// Block until there is plausibly something to do: a new op, a reconnect, or
    /// the shortest outstanding backoff elapsing.
    pub(crate) fn wait_for_drain_work(&self) {
        let (lock, cv) = &*self.drain_wake;
        let mut woken = lock.lock();
        if !*woken {
            cv.wait_for(&mut woken, DRAIN_IDLE_POLL);
        }
        *woken = false;
    }

    /// Perform one queued op and retire it.
    pub(crate) fn drain_op(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        match op.kind.as_str() {
            OP_REVISION => self.drain_revision(op),
            OP_CREATE | OP_MKDIR => self.drain_local_node(op),
            OP_RENAME => self.drain_rename(op),
            OP_TRASH => self.drain_trash(op),
            other => Err(format!("unknown pending op kind {other:?}").into()),
        }
    }

    /// Apply a queued rename/move to the remote.
    ///
    /// The op is the desired end state, so the remote's current state decides
    /// what actually has to be called: either half may already match (the event
    /// sync saw someone else do it, or an earlier attempt got half way through
    /// before failing). That also makes the whole thing idempotent, which a
    /// retrying queue needs.
    pub(crate) fn drain_rename(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let uid = parse_node_uid(&op.uid).ok_or("rename op has an unparseable uid")?;
        let parent_str = op.parent_uid.as_deref().ok_or("rename op has no parent")?;
        if is_local_uid_str(parent_str) {
            return Err(format!("destination {parent_str} has not been created yet").into());
        }
        let parent = parse_node_uid(parent_str).ok_or("rename op has an unparseable parent")?;
        let name = op.name.clone().ok_or("rename op has no name")?;

        // The node we were asked to rename may be gone or trashed by now. Either
        // way there is nothing to rename and nothing to lose — a rename holds no
        // bytes — so the op is satisfied rather than retried forever.
        let node = match self.fetch_node_remote(&uid)? {
            Some(n) if !n.trashed => n,
            _ => {
                warn!(%uid, name, "renamed node is gone or trashed remotely; dropping the rename");
                self.db.delete_op(op.id)?;
                return Ok(());
            }
        };
        // The name half goes first, so that a collision on the move half below is
        // about the name the node will actually land under rather than the one it
        // is about to lose.
        let mut landed = node.name.clone();
        if landed != name {
            match self.rt.block_on(self.client.rename_node(&uid, &name, None)) {
                Ok(()) => landed = name.clone(),
                // Someone took the name while we were offline. Renaming to a
                // *different* name is the non-destructive resolution: it neither
                // clobbers their file nor drops ours, and it is visible.
                Err(e) if is_already_exists(&e) => {
                    let alt = conflict_name(&name, now_secs());
                    warn!(%uid, name, alt, "rename target name is taken; using a conflict name");
                    self.rt
                        .block_on(self.client.rename_node(&uid, &alt, None))?;
                    landed = alt.clone();
                    self.adopt_drained_name(&uid, &alt);
                    self.log_activity(
                        ActivityKind::Rename,
                        &name,
                        format!("name was taken remotely; renamed to {alt}"),
                        false,
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        if node.parent_uid.as_ref() != Some(&parent) {
            match self.rt.block_on(self.client.move_node(&uid, &parent)) {
                Ok(()) => {}
                // The destination holds a `landed` of its own. Same resolution as
                // a name collision, and it has to happen before the move: the API
                // has no move-and-rename, so the node is renamed out of the way
                // here and moved second.
                Err(e) if is_already_exists(&e) => {
                    let alt = conflict_name(&landed, now_secs());
                    warn!(%uid, name = %landed, alt, "destination already holds that name; using a conflict name");
                    self.rt
                        .block_on(self.client.rename_node(&uid, &alt, None))?;
                    self.rt.block_on(self.client.move_node(&uid, &parent))?;
                    self.adopt_drained_name(&uid, &alt);
                    self.log_activity(
                        ActivityKind::Rename,
                        &landed,
                        format!("destination already had that name; moved as {alt}"),
                        false,
                    );
                }
                // The destination folder is gone. Leaving the node in its current
                // parent is the honest outcome: it is not where the user asked for
                // it, but it exists, it is where it has always been, and the
                // rename half above still applied. Retrying could only fail again
                // — the folder is not coming back — and would wedge the queue.
                Err(e) if is_gone(&e) => {
                    warn!(%uid, name = %landed, %parent, "move destination is gone; leaving the node where it is");
                    self.log_activity(
                        ActivityKind::Rename,
                        &landed,
                        "destination folder no longer exists; the file was left in place"
                            .to_string(),
                        false,
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        self.db.delete_op(op.id)?;
        info!(%uid, name = %landed, "pending rename landed");
        Ok(())
    }

    /// Apply a queued trash to the remote.
    ///
    /// A node that is already gone is a success, not a failure: the outcome the
    /// op asked for holds either way, and retrying forever against a node the
    /// server has forgotten would wedge the queue.
    pub(crate) fn drain_trash(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let uid = parse_node_uid(&op.uid).ok_or("trash op has an unparseable uid")?;
        let name = op.name.clone().unwrap_or_else(|| op.uid.clone());
        match self
            .rt
            .block_on(self.client.trash_nodes(std::slice::from_ref(&uid)))
        {
            Ok(()) => {}
            Err(e) if is_gone(&e) => {
                debug!(%uid, name, "node was already gone remotely; trash op satisfied");
            }
            Err(e) => return Err(e.into()),
        }
        self.db.delete_op(op.id)?;
        self.invalidate_trash();
        self.log_activity(ActivityKind::Trash, &name, "trashed", true);
        info!(%uid, name, "pending trash landed");
        Ok(())
    }

    /// Record the name the remote actually gave a node, after a conflict forced
    /// it away from the one the user asked for. Best effort: the event sync
    /// would correct the tree anyway, but not before the user has looked at it.
    pub(crate) fn adopt_drained_name(&self, uid: &NodeUid, name: &str) {
        let mut st = self.state.lock();
        let Some(&ino) = st.by_uid.get(uid) else {
            return;
        };
        let Some(entry) = st.entries.get_mut(&ino) else {
            return;
        };
        entry.node.name = name.to_string();
        let node = entry.node.clone();
        if let Err(e) = st.db.upsert_node(&node) {
            warn!(%uid, error = %e, "db upsert_node failed after a conflict rename");
        }
    }

    /// Make a node that so far exists only on this machine real, and adopt the
    /// uid the server gives it (offline.md Phase 3b).
    pub(crate) fn drain_local_node(
        &self,
        op: &PendingOp,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let local = parse_node_uid(&op.uid).ok_or("pending op has an unparseable uid")?;
        let parent_str = op.parent_uid.as_deref().ok_or("create op has no parent")?;
        // `run_pending_drain` will not offer an op whose parent is still a
        // placeholder, so reaching here with one is a bug, not a wait.
        if is_local_uid_str(parent_str) {
            return Err(format!("parent {parent_str} has not been created yet").into());
        }
        let parent = parse_node_uid(parent_str).ok_or("create op has an unparseable parent")?;
        let wanted = op.name.clone().ok_or("create op has no name")?;

        // Someone else may have taken the name while this sat in the queue —
        // reachable only because the op waited, which is the whole point of the
        // queue. Never overwrite theirs and never drop ours: land under a
        // conflict name, exactly as the sync engine does.
        let mut name = wanted.clone();
        let mut real = self.create_drained_node(op, &parent, &name);
        if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
            name = conflict_name(&wanted, now_secs());
            warn!(%local, wanted, name, "name is taken remotely; creating under a conflict name");
            real = self.create_drained_node(op, &parent, &name);
            if real.is_ok() {
                self.log_activity(
                    ActivityKind::Upload,
                    &wanted,
                    format!("name was taken remotely; created as {name}"),
                    false,
                );
            }
        }
        // The folder this node was created in may have been trashed remotely
        // while the op waited, in which case nothing will ever make the op
        // succeed as written and retrying it forever wedges the queue behind a
        // file that has bytes to save. Re-home it to the root: not where the
        // user put it, but it exists, it is visible, and the bytes are intact.
        if let Some(root) = self.root_uid()
            && real.is_err()
            && self.parent_is_gone(&parent)
        {
            warn!(%local, name, "parent folder is gone remotely; creating in the root instead");
            real = self.create_drained_node(op, &root, &name);
            if real.as_ref().is_err_and(|e| is_already_exists(e.as_ref())) {
                name = conflict_name(&wanted, now_secs());
                real = self.create_drained_node(op, &root, &name);
            }
            if real.is_ok() {
                self.log_activity(
                    ActivityKind::Upload,
                    &wanted,
                    format!("its folder was trashed remotely; created in the root as {name}"),
                    false,
                );
            }
        }
        let real = real?;

        // Retire the op before touching anything else: if we crash here the node
        // exists remotely and the local placeholder is reconciled by the event
        // sync, whereas a surviving op would create the file a second time.
        self.db.delete_op(op.id)?;
        self.adopt_real_uid(&local, &real)?;
        if let Some(blob) = op.blob_path.as_deref() {
            self.cache.discard_staged(Path::new(blob));
        }
        self.pending.lock().remove(&local);
        self.log_activity(ActivityKind::Upload, &name, "created", true);
        info!(%local, %real, name, kind = %op.kind, "pending create landed");
        Ok(())
    }

    /// Make one queued `create`/`mkdir` real under a given name, and hand back
    /// the API's own error so the caller can tell a name clash from a failure.
    ///
    /// Split out because the conflict path has to run it twice: the second time
    /// under a different name.
    pub(crate) fn create_drained_node(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<NodeUid, Box<dyn std::error::Error>> {
        match op.kind == OP_MKDIR {
            true => {
                Ok(self
                    .rt
                    .block_on(self.client.create_folder(parent, name, Some(now_secs())))?)
            }
            false => self.upload_created_file(op, parent, name),
        }
    }

    /// Upload the bytes a queued create accumulated, if any. A file that was
    /// created but never written (`touch`) has no blob and uploads as empty.
    pub(crate) fn upload_created_file(
        &self,
        op: &PendingOp,
        parent: &NodeUid,
        name: &str,
    ) -> Result<NodeUid, Box<dyn std::error::Error>> {
        let Some(blob) = op.blob_path.as_deref() else {
            return Ok(self.rt.block_on(self.client.upload_file(
                parent,
                name,
                media_type_for(name),
                b"",
            ))?);
        };
        let meta: StagedWrite = serde_json::from_str(op.meta_json.as_deref().unwrap_or(""))?;
        // An incomplete blob would be authored bytes over zeros, and there is no
        // base to repair it from — the file has never existed remotely. Refusing
        // to queue that is `queue_revision`'s job, so reaching here means the blob
        // is whole.
        if !meta.complete {
            return Err("queued create holds an incomplete blob".into());
        }
        let guard = self
            .transfers
            .begin(name, op.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(blob)?, &guard);
        let uid = self.rt.block_on(self.client.upload_file_from(
            parent,
            name,
            media_type_for(name),
            reader,
            meta.len as i64,
            Vec::new(),
            None,
            false,
        ))?;
        Ok(uid)
    }

    /// Swap a placeholder uid for the real one across everything that keyed off
    /// it: queued children, the DB, the in-memory tree, and the caches.
    ///
    /// The inode is deliberately kept, so anything already holding the file open
    /// keeps working across the drain.
    pub(crate) fn adopt_real_uid(
        &self,
        local: &NodeUid,
        real: &NodeUid,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let node = self
            .fetch_node(real)
            .map_err(|e| self.errno_error(e, "fetch node"))?;
        // Repoints queued children and node rows, and drops the placeholder row.
        self.db
            .remap_local_uid(&local.to_string(), &real.to_string())?;

        let mut st = self.state.lock();
        if let Some(ino) = st.by_uid.remove(local) {
            st.by_uid.insert(real.clone(), ino);
            if let Some(e) = st.entries.get_mut(&ino) {
                e.uid = real.clone();
                e.node = node.clone();
            }
            // Where the op said the node goes and where it actually landed can
            // differ — a conflict re-homes it — so the tree follows the parent
            // the server reports rather than the one we asked for.
            if let Some(parent) = node.parent_uid.clone()
                && let Some(&pino) = st.by_uid.get(&parent)
                && st.entries.get(&ino).is_some_and(|e| e.parent != pino)
            {
                let old = st.entries.get(&ino).map(|e| e.parent);
                if let Some(old) = old
                    && let Some(kids) = st.children.get_mut(&old)
                {
                    kids.retain(|&k| k != ino);
                }
                if let Some(e) = st.entries.get_mut(&ino) {
                    e.parent = pino;
                }
                if let Some(kids) = st.children.get_mut(&pino)
                    && !kids.contains(&ino)
                {
                    kids.push(ino);
                }
            }
        }
        drop(st);
        // Write the real node through, now that nothing points at the old uid.
        if let Err(e) = self.db.upsert_node(&node) {
            warn!(%real, error = %e, "db upsert_node failed after remap");
        }
        if node.is_folder() {
            // It was recorded as listed while local (it was empty and had nothing
            // to enumerate). That still holds: its queued children re-intern under
            // the real uid as they drain.
            if let Err(e) = self.db.set_listed(real, true) {
                warn!(%real, error = %e, "db set_listed(true) failed after remap");
            }
        }
        Ok(())
    }

    /// Why a queued write must not be applied to its node, or `None` when it
    /// still can be.
    ///
    /// A queued write is an edit of a specific revision. Time passes before it
    /// drains — indefinitely, if that is how long the network is gone — and in
    /// that window the node can be rewritten by another device, trashed, or
    /// deleted outright. Sending the blob anyway would silently drop whatever
    /// happened in between, which is exactly the thing the sync engine refuses
    /// to do (offline.md Phase 3b).
    ///
    /// Only checkable against a recorded baseline: a write staged before
    /// [`StagedWrite::based_on`] existed, or one against a node that has never
    /// existed remotely, has nothing to compare and is applied as before.
    pub(crate) fn revision_conflict(
        &self,
        uid: &NodeUid,
        meta: &StagedWrite,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let Some(base) = meta.based_on else {
            return Ok(None);
        };
        let Some(node) = self.fetch_node_remote(uid)? else {
            return Ok(Some("the file no longer exists remotely".into()));
        };
        if node.trashed {
            return Ok(Some("the file was trashed remotely".into()));
        }
        let (mtime, size) = (node.modification_time, node_size(&node));
        if mtime != base.mtime || size != base.size {
            return Ok(Some(format!(
                "the remote revision changed under the queued write \
                 (expected {} bytes at mtime {}, found {size} at {mtime})",
                base.size, base.mtime
            )));
        }
        Ok(None)
    }

    /// Land a queued write that can no longer be applied to its own node as a
    /// *new* file beside it, and retire the op.
    ///
    /// The non-destructive resolution, and the same one the sync engine reaches
    /// for: the remote keeps whatever it has, the user keeps their bytes, and
    /// the name says which is which.
    ///
    /// An incomplete blob is gap-filled from whatever the node holds *now* —
    /// mixing revisions, which is only defensible because the result is
    /// explicitly a conflict copy rather than anyone's file. When even that is
    /// impossible the op is dropped but the staged bytes are deliberately left
    /// on disk: unreachable through the mount, but not destroyed, and the
    /// activity log says where they are.
    pub(crate) fn keep_as_conflict_copy(
        &self,
        op: &PendingOp,
        blob: &Path,
        meta: &StagedWrite,
        uid: &NodeUid,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let place = self.node_place(uid);
        let name = place
            .as_ref()
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| self.node_name(uid));
        // A node the tree has forgotten still has bytes worth keeping, so the
        // copy falls back to the root rather than being abandoned.
        let Some(parent) = place.map(|(p, _)| p).or_else(|| self.root_uid()) else {
            return self.abandon_to_staging(op, blob, uid, &name, reason);
        };
        warn!(%uid, name, reason, "queued write conflicts; keeping a conflict copy");

        if !meta.complete {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(blob)?;
            let mut written = Intervals::default();
            for &(s, e) in &meta.authored {
                written.add(s, e);
            }
            if let Err(e) = self.fill_gaps(
                uid,
                &file,
                meta.len,
                meta.base_mtime,
                meta.base_size,
                &written,
            ) {
                error!(%uid, name, error = ?e, "cannot complete a conflicted partial write");
                return self.abandon_to_staging(op, blob, uid, &name, reason);
            }
        }

        let alt = conflict_name(&name, now_secs());
        let guard =
            self.transfers
                .begin(&alt, meta.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(blob)?, &guard);
        self.rt.block_on(self.client.upload_file_from(
            &parent,
            &alt,
            media_type_for(&alt),
            reader,
            meta.len as i64,
            Vec::new(),
            None,
            false,
        ))?;
        drop(guard);

        self.db.delete_op(op.id)?;
        // Dropping the pending entry hands the node back to the remote's truth:
        // reads stop coming from the staged blob, and the event sync stops
        // skipping it as "ahead of the server" (offline.md Phase 3a).
        self.pending.lock().remove(uid);
        self.cache.discard_staged(blob);
        self.cache.evict(uid);
        self.evict_reader(uid);
        // The conflict copy is a node the tree has never seen.
        let mut st = self.state.lock();
        if let Some(&ino) = st.by_uid.get(&parent) {
            st.invalidate_listing(ino);
        }
        drop(st);
        self.log_activity(
            ActivityKind::Upload,
            &name,
            format!("{reason}; local changes uploaded as {alt}"),
            false,
        );
        info!(%uid, name, alt, "queued write landed as a conflict copy");
        Ok(())
    }

    /// Give up on placing a queued write anywhere the mount can see, without
    /// destroying it: the op goes (it could only fail forever) but the staged
    /// blob deliberately stays on disk, and the activity log says where.
    ///
    /// The last resort of the conflict path, and the same bargain
    /// [`Core::stage_orphaned_write`] strikes at the other end: bytes we cannot
    /// place are still bytes we do not get to delete.
    pub(crate) fn abandon_to_staging(
        &self,
        op: &PendingOp,
        blob: &Path,
        uid: &NodeUid,
        name: &str,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        error!(%uid, name, reason, staged = %blob.display(),
               "cannot place a conflicted write; bytes kept in staging");
        self.db.delete_op(op.id)?;
        self.pending.lock().remove(uid);
        self.cache.evict(uid);
        self.evict_reader(uid);
        self.log_activity(
            ActivityKind::Upload,
            name,
            format!("{reason}; local changes kept at {}", blob.display()),
            false,
        );
        Ok(())
    }

    /// Whether a folder a queued op targets has stopped being a place a node can
    /// go. A failure to ask is not an answer: only a definite "trashed" or "not
    /// there" counts, so a network fault leaves the op to retry normally.
    pub(crate) fn parent_is_gone(&self, parent: &NodeUid) -> bool {
        match self.fetch_node_remote(parent) {
            Ok(Some(node)) => node.trashed,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// A node's parent uid and name, as the tree currently has them.
    pub(crate) fn node_place(&self, uid: &NodeUid) -> Option<(NodeUid, String)> {
        {
            let st = self.state.lock();
            if let Some(entry) = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino))
                && let Some(parent) = entry.node.parent_uid.clone()
            {
                return Some((parent, entry.node.name.clone()));
            }
        }
        // The in-memory tree only holds what has been walked to since the daemon
        // started, and the drain routinely runs before anything has walked to
        // this file — after a restart, or for a write that arrived by path. The
        // DB knows it anyway. Without this fallback a conflict copy was named
        // after the node's *uid* and dumped in the root, which is how a file
        // called `G88km…==~c2do…== (sync-conflict 1784429627)` appears at the
        // top of someone's Drive.
        let node = self.db.node_by_uid(&uid.to_string()).ok().flatten()?;
        Some((node.parent_uid.clone()?, node.name.clone()))
    }

    /// The name to show a user for `uid`, for a transfer entry or an activity
    /// log line.
    ///
    /// Falls back to something short and human rather than to the uid: a uid is
    /// 130 characters of base64 that identifies the file to us and to nobody
    /// else, and it has ended up in both the activity log and on real files in
    /// the Drive root. The link prefix keeps it traceable without pretending to
    /// be a name.
    pub(crate) fn node_name(&self, uid: &NodeUid) -> String {
        self.node_place(uid).map(|(_, n)| n).unwrap_or_else(|| {
            let short: String = uid.link_id.to_string().chars().take(8).collect();
            format!("recovered-{short}")
        })
    }

    /// The uid of the My Files root, which every node in the mount descends
    /// from — the last resort for placing a file whose own parent is unknown.
    ///
    /// `None` only for a [`Core::fork_state`] sibling that has not interned its
    /// root yet, which is not where the drain runs; the drain must not panic
    /// over it regardless, since that would stop the queue for good.
    pub(crate) fn root_uid(&self) -> Option<NodeUid> {
        let st = self.state.lock();
        st.entries.get(&ROOT_INO).map(|e| e.uid.clone())
    }

    /// Upload a staged revision of a file the server already knows about.
    pub(crate) fn drain_revision(&self, op: &PendingOp) -> Result<(), Box<dyn std::error::Error>> {
        let blob = op
            .blob_path
            .clone()
            .ok_or("pending op has no staged blob")?;
        let blob = PathBuf::from(blob);
        let meta: StagedWrite = serde_json::from_str(op.meta_json.as_deref().unwrap_or(""))?;
        let uid = parse_node_uid(&meta.uid).ok_or("staged write has an unparseable uid")?;

        if let Some(reason) = self.revision_conflict(&uid, &meta)? {
            return self.keep_as_conflict_copy(op, &blob, &meta, &uid, &reason);
        }

        // An incomplete blob is authored bytes over zeros; the untouched ranges
        // have to be filled from the base before it can be sent. This is the case
        // the write could not resolve at release time (it was offline), and the
        // reason it is safe to do now is that we are not.
        if !meta.complete {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&blob)?;
            let mut written = Intervals::default();
            for &(s, e) in &meta.authored {
                written.add(s, e);
            }
            self.fill_gaps(
                &uid,
                &file,
                meta.len,
                meta.base_mtime,
                meta.base_size,
                &written,
            )
            .map_err(|e| self.errno_error(e, "gap-fill from base failed"))?;
        }

        let name = self.node_name(&uid);
        let guard =
            self.transfers
                .begin(&name, meta.uid.clone(), TransferDirection::Upload, meta.len);
        let reader = CountingReader::new(File::open(&blob)?, &guard);
        self.rt.block_on(self.client.upload_new_revision_from(
            &uid,
            reader,
            meta.len as i64,
            Vec::new(),
            None,
        ))?;
        drop(guard);

        // Retire the op before dropping the blob: a crash between the two leaves
        // an orphaned file (harmless), whereas the reverse would leave a queued
        // op pointing at nothing.
        self.db.delete_op(op.id)?;
        self.pending.lock().remove(&uid);

        // The staged blob now matches the sealed revision, so a pinned file keeps
        // it as its cached content rather than re-downloading what we just sent.
        if self.cache.is_pinned(&uid)
            && let Ok(bytes) = std::fs::read(&blob)
        {
            let _ = self.cache.store(&uid, now_secs(), meta.len, &bytes);
        }
        self.cache.discard_staged(&blob);
        self.evict_reader(&uid);
        self.refresh_after_upload(&uid);
        self.log_activity(ActivityKind::Upload, &name, "uploaded", true);
        info!(%uid, len = meta.len, "pending upload landed");
        Ok(())
    }

    /// Adopt the server's metadata for a node we have just uploaded a revision
    /// for.
    ///
    /// [`State::record_pending_write`] deliberately stamps the node with the
    /// moment we *accepted* the write, so `ls` reflects it before the upload
    /// lands. The server stamps the sealed revision with its own time, and the
    /// two differ by however long the upload took. That difference is not
    /// cosmetic: [`Core::remote_baseline`] reads this node to build
    /// [`StagedWrite::based_on`], and [`Core::revision_conflict`] compares that
    /// baseline against the remote — so leaving our optimistic time in place
    /// makes the *next* write to this file look like another device changed it
    /// underneath us, and diverts it into a conflict copy over nothing.
    ///
    /// A write queued while this upload was on the wire is handled instead by
    /// [`Core::rebaseline_pending`]: its optimistic size/mtime must stay on the
    /// node (the same reason `apply_event` refuses to overwrite a node with a
    /// queued write), but its *baseline* still has to learn about the revision
    /// we just sealed. Skipping both, as this used to, is what produced a file
    /// that conflicted with itself and left a full-size duplicate behind.
    ///
    /// Best effort: a failure here costs a spurious conflict copy on the next
    /// write, not this upload, which has already landed.
    pub(crate) fn refresh_after_upload(&self, uid: &NodeUid) {
        let node = match self.fetch_node_remote(uid) {
            Ok(Some(node)) => node,
            // Trashed or deleted under us: the tree will hear it from the event
            // sync, which is better placed to unhook the inode than we are.
            Ok(None) => return,
            Err(e) => {
                warn!(%uid, error = %e,
                      "refreshing metadata after an upload failed; \
                       the next write to this file may conflict with itself");
                return;
            }
        };
        // Ordered so that a write queued *during* the fetch above is still
        // caught: it took its baseline from the node's optimistic stamp, and
        // this overwrites it with the revision the server actually holds.
        if self.rebaseline_pending(uid, &node) {
            return;
        }
        let mut st = self.state.lock();
        let Some(parent) = st
            .by_uid
            .get(uid)
            .and_then(|ino| st.entries.get(ino))
            .map(|e| e.parent)
        else {
            return;
        };
        st.intern(parent, node);
    }

    /// Point a still-queued write at the revision this upload just sealed, and
    /// report whether there was one.
    ///
    /// The revision we sent *is* the base the queued write will be applied over,
    /// but nothing else says so. [`Core::remote_baseline`] carries a baseline
    /// across a supersede because the op it inherits from "is the last one that
    /// actually observed the remote" — true only until that op drains. Once it
    /// has, the inherited baseline names a revision we replaced ourselves, and
    /// [`Core::revision_conflict`] reads that as another device having moved the
    /// file. It is a self-conflict, and it is expensive: the whole staged blob
    /// is re-uploaded as a second file.
    ///
    /// Both copies of the sidecar are updated — the in-memory one the next
    /// `release` inherits from, and the persisted one the drain reloads after a
    /// restart. Best effort on the DB half; the in-memory half is what the
    /// immediate next write reads.
    fn rebaseline_pending(&self, uid: &NodeUid, sealed: &Node) -> bool {
        let base = Baseline {
            mtime: sealed.modification_time,
            size: node_size(sealed),
        };
        let mut pending = self.pending.lock();
        let Some(p) = pending.get_mut(uid) else {
            return false;
        };
        p.meta.based_on = Some(base);
        match serde_json::to_string(&p.meta) {
            Ok(json) => {
                if let Err(e) = self.db.update_op_meta(&uid.to_string(), OP_REVISION, &json) {
                    warn!(%uid, error = %e,
                          "restamping a queued write's baseline failed; \
                           it may land as a conflict copy of itself");
                }
            }
            Err(e) => warn!(%uid, error = %e, "serializing a restamped sidecar failed"),
        }
        debug!(%uid, mtime = base.mtime, size = base.size,
               "queued write rebaselined onto the revision just uploaded");
        true
    }
}
