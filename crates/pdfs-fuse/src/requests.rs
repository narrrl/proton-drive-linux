//! Requests by path from the control socket: listing, opening, renaming,
//! moving, copying and deleting, and taking a replaced file back.

use super::*;

impl Core {
    /// List the directory at mountpoint-relative `rel` for the in-app browser:
    /// the same lazy remote enumeration `readdir` uses, projected into
    /// serializable [`DirEntry`]s (with per-file pin state).
    pub(super) fn list_dir(&self, rel: &Path) -> CoreResult<Vec<DirEntry>> {
        let (ino, uid) = self.resolve(rel)?;
        self.ensure_children(ino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;
        self.settle_listing_sizes(ino);
        // Every entry shares this folder as its parent, so one lookup says
        // whether they are all kept through it.
        let pinned_by_folder = self.cache.is_pinned(&uid);
        // Snapshot the listing, then drop the lock before touching the on-disk
        // pin registry so a slow disk read doesn't block FUSE metadata ops.
        let rows: Vec<(String, bool, u64, i64, NodeUid, u64)> = {
            let st = self.state();
            st.children
                .get(&ino)
                .map(|kids| {
                    kids.iter()
                        .filter_map(|&k| st.entries.get(&k).map(|e| (k, e)))
                        .map(|(k, e)| {
                            (
                                e.node.name.clone(),
                                e.node.is_folder(),
                                node_size(&e.node),
                                e.node.modification_time,
                                e.uid.clone(),
                                k,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let issues = self.db.node_issues().unwrap_or_default();
        Ok(rows
            .into_iter()
            .map(|(name, is_dir, size, modified, uid, ino)| DirEntry {
                name,
                is_dir,
                size,
                modified,
                pinned: pinned_by_folder || self.cache.is_pinned(&uid),
                pinned_by_folder,
                cached: !is_dir && self.cache.is_cached(&uid, modified, size),
                uid: uid.to_string(),
                // Listing entries live in the requested dir; the caller derives
                // the path from its name. Left empty.
                path: String::new(),
                // My own tree: a role only means something under a share, and
                // this listing never crosses into one.
                role: String::new(),
                shared_by: String::new(),
                shared_at: 0,
                shared_by_unverified: false,
                trashed_at: 0,
                trashed_from: None,
                trashed_parent: None,
                issue: state::ino_lid(ino)
                    .and_then(|lid| issues.get(&lid))
                    .map(|issue| SyncIssue::parse(issue)),
            })
            .collect())
    }

    /// Re-fetch the whole photos timeline and persist it. Returns whether the
    /// account has a photos volume at all.
    /// Download the full content of the Drive file at mountpoint-relative `rel`
    /// into the content cache, returning its on-disk path (served from cache
    /// when a fresh blob already exists). Lets a front-end open the file with
    /// the user's default application without pinning it.
    pub(super) fn open_file(&self, rel: &Path) -> CoreResult<PathBuf> {
        let (ino, uid) = self.resolve(rel)?;
        let (name, mtime, size) = {
            let st = self.state();
            let e = st
                .entries
                .get(&ino)
                .ok_or_else(|| CoreError::not_found("node vanished"))?;
            if !e.node.is_file() {
                return Err(CoreError::invalid("not a regular file"));
            }
            (
                e.node.name.clone(),
                e.node.modification_time,
                node_size(&e.node),
            )
        };
        self.fetch_content(&uid, &name, mtime, size)
    }

    /// [`open_file`](Self::open_file) addressed by uid instead of by path.
    ///
    /// A search hit is a metadata-index row, and the index covers every
    /// location this daemon knows — including on-demand sync folders and
    /// mirrors, whose nodes have no path inside the primary mount's tree.
    /// Walking such a hit's path from the mount root answers ENOENT even though
    /// the node is perfectly reachable, so front-ends address hits by uid.
    ///
    /// Metadata comes from the live tree, then the index, then the API — the
    /// last covers a node the index knows but no location holds. Unlike the
    /// mutating by-uid handlers this does not demand a
    /// [`resolve_anywhere`](Self::resolve_anywhere) residency proof: reading
    /// one's own file is what the API authorizes anyway, and requiring
    /// residency is exactly what turned an openable search hit into "no such
    /// file or folder". [`open_shared_file`](Self::open_shared_file) already
    /// works this way. Reserved (local/virtual) uids name no remote node and
    /// are still rejected.
    pub(super) fn open_file_uid(&self, raw_uid: &str) -> CoreResult<PathBuf> {
        let uid = parse_uid(raw_uid)
            .filter(|uid| !is_local_uid(uid) && !is_virtual_uid(uid))
            .ok_or_else(|| CoreError::invalid(format!("invalid uid: {raw_uid}")))?;
        let live = self
            .state
            .lock()
            .entries
            .values()
            .find(|e| e.uid == uid)
            .map(|e| e.node.clone());
        let node = match live {
            Some(node) => node,
            None => match self
                .db
                .node_by_uid(&uid.to_string())
                .map_err(|e| CoreError::from_api(&e, "load node"))?
            {
                Some(node) => node,
                None => self
                    .rt
                    .block_on(self.drive.get_node(&uid))
                    .map_err(|e| CoreError::from_api(&e, "get node"))?
                    .ok_or_else(|| CoreError::not_found("node vanished"))?,
            },
        };
        if !node.is_file() {
            return Err(CoreError::invalid("not a regular file"));
        }
        self.fetch_content(&uid, &node.name, node.modification_time, node_size(&node))
    }

    /// Materialise a file's full content into the content cache, returning its
    /// on-disk path. Serves a fresh cached blob without touching the network.
    pub(super) fn fetch_content(
        &self,
        uid: &NodeUid,
        name: &str,
        mtime: i64,
        size: u64,
    ) -> CoreResult<PathBuf> {
        if let Some(p) = self.cache.cached_content_path(uid, mtime, size) {
            return Ok(p);
        }

        // Keep the temporary file beside the final cache blob. `store_file`
        // can then adopt it with a hard link and atomic rename, while the SDK
        // streams decrypted blocks to disk instead of building a file-sized
        // `Vec<u8>`. A unique name also keeps concurrent opens of the same node
        // from sharing a partially written staging file.
        static OPEN_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = OPEN_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self.cache.content_path(uid).with_extension(format!(
            "open-{}-{}-{seq}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| CoreError::internal(format!("create download temp: {e}")))?;
        let file = match self.download_file_tracked_to(uid, name, size, file) {
            Ok(file) => file,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(CoreError::from_api(&e, "download"));
            }
        };
        if let Err(e) = file.sync_all() {
            let _ = std::fs::remove_file(&tmp);
            return Err(CoreError::internal(format!("sync download temp: {e}")));
        }
        if let Err(e) = self.cache.store_file(uid, mtime, size, &tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(CoreError::internal(format!("cache store: {e}")));
        }
        let _ = std::fs::remove_file(&tmp);
        Ok(self.cache.content_path(uid))
    }

    /// Rename a file or folder to `new_name`. `rel` is mountpoint-relative.
    /// Mirrors the FUSE `rename` write path: queued whenever the mount queues
    /// ([`Core::queue_relocation`]), otherwise renamed on the remote, then
    /// moved to its new name in every mount with the parent listing dropped
    /// so the next `ListDir` re-enumerates.
    pub(super) fn rename(&self, rel: &Path, new_name: &str) -> CoreResult<String> {
        if new_name.is_empty() || new_name.contains('/') {
            return Err(CoreError::invalid(format!("invalid name: {new_name:?}")));
        }
        let (ino, uid) = self.resolve(rel)?;
        let old_parent_uid = self.source_parent_uid(ino, rel)?;
        require_rename_access(
            |authority| self.require_uid_writable(authority),
            &uid,
            &old_parent_uid,
            &old_parent_uid,
        )
        .map_err(|error| self.errno_error(error, "rename access"))?;
        if self.queues_relocation(&uid, &old_parent_uid) {
            self.queue_relocation(
                ino,
                &uid,
                &old_parent_uid,
                &old_parent_uid,
                &rel.with_file_name(new_name),
                new_name,
            )?;
            return Ok(new_name.to_string());
        }
        self.rename_remote(&uid, new_name)
            .map_err(|e| CoreError::from_api(&e, "rename"))?;
        self.relocate_everywhere(&uid, &old_parent_uid, new_name);
        self.invalidate_parent_listing(rel);
        Ok(new_name.to_string())
    }

    /// Whether a rename or move asked for over the control socket goes
    /// through the queue. It does whenever the same change through the mount
    /// would, and whenever the node has anything queued: sent straight to
    /// Drive, it would land first, and the queued change landing after it
    /// would undo it (`docs/BUGS.md` B149).
    pub(super) fn queues_relocation(&self, uid: &NodeUid, new_parent_uid: &NodeUid) -> bool {
        !self.sends_inline()
            || is_local_uid(uid)
            || is_local_uid(new_parent_uid)
            || self.db.has_any_op(&uid.to_string()).unwrap_or(true)
    }

    /// Queue renaming or moving the node at `ino` to `dest`, whose last
    /// component is `name`, and apply it to every mount now, as the mount's
    /// own queued rename does.
    ///
    /// The listings stay: Drive has not been told yet, so listing either
    /// folder again would bring back the old name.
    pub(super) fn queue_relocation(
        &self,
        ino: u64,
        uid: &NodeUid,
        old_parent_uid: &NodeUid,
        new_parent_uid: &NodeUid,
        dest: &Path,
        name: &str,
    ) -> CoreResult<()> {
        // Drive refuses a taken name, but only once the queue sends the change.
        match self.resolve_path(dest) {
            Ok((found, _)) if found != ino => {
                return Err(CoreError::conflict(format!(
                    "{} already exists",
                    dest.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.code() == libc::ENOENT => {}
            Err(e) => {
                return Err(self.errno_error(e, &format!("could not resolve {}", dest.display())));
            }
        }
        let old_name = self
            .state()
            .entries
            .get(&ino)
            .map(|entry| entry.node.name.clone())
            .unwrap_or_default();
        let mut uid = uid.clone();
        // A node whose create is still queued is that op: rewriting its target
        // is the whole change, as through the mount.
        if is_local_uid(&uid) {
            match self
                .db
                .rewrite_op_target(&uid.to_string(), &new_parent_uid.to_string(), name)
            {
                Ok(true) => {
                    self.relocate_queued_everywhere(&uid, new_parent_uid, name);
                    // A parked transient file given a finished name is meant
                    // to reach Drive now (`docs/BUGS.md` B70).
                    if is_transient_name(&old_name)
                        && !is_transient_name(name)
                        && let Err(e) = self.db.set_create_hold(&uid.to_string(), false)
                    {
                        warn!(%uid, error = %e, "un-parking a finalized transient create failed");
                    }
                    self.wake_drain();
                    return Ok(());
                }
                // The create landed meanwhile: rename what Drive made.
                Ok(false) => {
                    uid = self.landed_row_uid(ino).ok_or_else(|| {
                        CoreError::conflict(format!(
                            "{old_name} changed while it was being renamed; try again"
                        ))
                    })?;
                }
                Err(e) => return Err(CoreError::internal(format!("db: {e}"))),
            }
        }
        self.enqueue_rename(ino, &uid, old_parent_uid, new_parent_uid, name)
            .map_err(|e| self.errno_error(e, "queue the change"))?;
        self.relocate_queued_everywhere(&uid, new_parent_uid, name);
        self.wake_drain();
        debug!(%uid, %new_parent_uid, name, "renamed over the control socket; queued");
        Ok(())
    }

    /// Bring every mount level with a rename or move Drive has made of `uid`,
    /// to `name` in `new_parent`, and have each kernel session drop the name it
    /// cached for it. Returns the name a mount knew the node by.
    ///
    /// For changes that do not come through the mount. The kernel made a
    /// rename through the mount itself, but one made over the control socket
    /// leaves the old name cached for [`TTL`] (`docs/BUGS.md` B114).
    ///
    /// The node is moved, not forgotten, and so is its row. Forgetting it, as
    /// this used to, deleted the row, and until a listing brought it back
    /// nothing local knew the node: `pdfs sync queue` showed its queued upload
    /// by raw id, and the drain re-read the node and put the upload off
    /// (`docs/BUGS.md` B117).
    pub(super) fn relocate_everywhere(
        &self,
        uid: &NodeUid,
        new_parent: &NodeUid,
        name: &str,
    ) -> Option<String> {
        self.relocate_with(uid, new_parent, name, false)
    }

    /// [`Core::relocate_everywhere`] for a change the queue has not sent yet.
    /// Each mount keeps its listings, which Drive cannot bring up to date
    /// until the change lands.
    pub(super) fn relocate_queued_everywhere(
        &self,
        uid: &NodeUid,
        new_parent: &NodeUid,
        name: &str,
    ) {
        self.relocate_with(uid, new_parent, name, true);
    }

    pub(super) fn relocate_with(
        &self,
        uid: &NodeUid,
        new_parent: &NodeUid,
        name: &str,
        queued: bool,
    ) -> Option<String> {
        let mut old_name = None;
        let mut moved_row = false;
        self.for_each_mount(|st, notify| {
            if let Some((from_name, relocated)) =
                relocate_in_mount(st, notify, uid, new_parent, name, queued)
            {
                old_name = Some(from_name);
                moved_row |= relocated;
            }
        });
        if !moved_row {
            self.move_row(uid, new_parent, name);
        }
        old_name
    }

    /// Point the persisted row of `uid` at `name` in `new_parent`, for a node
    /// no mount holds where it now is.
    pub(super) fn move_row(&self, uid: &NodeUid, new_parent: &NodeUid, name: &str) {
        let moved = self.db.node_by_uid(&uid.to_string()).and_then(|node| {
            let Some(mut node) = node else {
                return Ok(());
            };
            node.name = name.to_string();
            node.parent_uid = Some(new_parent.clone());
            self.db.upsert_node(&node)
        });
        if let Err(error) = moved {
            warn!(%uid, ?error, "could not move the node's row along with it");
        }
    }

    /// Forget `uid` in every mount, and have each kernel session drop the name
    /// it cached for it. Returns the name a mount knew the node by.
    ///
    /// For a removal that does not come through the mount. The kernel made an
    /// unlink through the mount itself, but one made over the control socket
    /// leaves the old name cached for [`TTL`], pointing at an inode the mount no
    /// longer has (`docs/BUGS.md` B114). Every mount, not only this one, so a
    /// fork showing the same node does not keep serving it (`docs/BUGS.md`
    /// B74).
    pub(super) fn forget_everywhere(&self, uid: &NodeUid) -> Option<String> {
        let mut name = None;
        self.for_each_mount(|st, notify| {
            if let Some(entry) = st.by_uid.get(uid).and_then(|ino| st.entries.get(ino)) {
                name = Some(entry.node.name.clone());
            }
            forget_and_notify(st, notify, uid);
        });
        name
    }

    /// Move a file or folder into the folder at `new_parent_rel`. Both paths are
    /// mountpoint-relative. Queued whenever the mount queues
    /// ([`Core::queue_relocation`]); otherwise moved on the remote, then in
    /// every mount, with both listings invalidated so each re-enumerates on
    /// next access.
    pub(super) fn move_to(&self, rel: &Path, new_parent_rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let old_parent_uid = self.source_parent_uid(ino, rel)?;
        let (pino, new_parent_uid) = self
            .resolve_path(new_parent_rel)
            .map_err(|e| self.errno_error(e, "resolve new parent"))?;
        require_rename_access(
            |authority| self.require_uid_writable(authority),
            &uid,
            &old_parent_uid,
            &new_parent_uid,
        )
        .map_err(|error| self.errno_error(error, "move access"))?;
        let name = self
            .state()
            .entries
            .get(&ino)
            .map(|entry| entry.node.name.clone())
            .unwrap_or_default();
        if self.queues_relocation(&uid, &new_parent_uid) {
            if self.state().is_ancestor_of(ino, pino) {
                return Err(CoreError::invalid("cannot move a folder into itself"));
            }
            self.queue_relocation(
                ino,
                &uid,
                &old_parent_uid,
                &new_parent_uid,
                &new_parent_rel.join(&name),
                &name,
            )?;
            return Ok(name);
        }
        self.move_remote(&uid, &new_parent_uid)
            .map_err(|e| CoreError::from_api(&e, "move"))?;
        self.relocate_everywhere(&uid, &new_parent_uid, &name);
        self.invalidate_parent_listing(rel);
        self.state().invalidate_listing(pino);
        Ok(name)
    }

    /// Copy a file or folder into `new_parent_rel` on Proton Drive, which copies
    /// the content there: nothing is downloaded or uploaded. Both paths are
    /// mountpoint-relative. A name already taken in the folder gets the next
    /// free ` (n)` suffix. Returns the copy's name.
    ///
    /// Drive copies what it holds, so the copy is refused offline and while the
    /// mount still owes Drive part of it: a node or folder only queued locally,
    /// a file with a queued upload or unsaved writes, or — for a folder, whose
    /// queued changes could sit anywhere below it — any queued op at all.
    pub(super) fn copy_to(&self, rel: &Path, new_parent_rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let (pino, new_parent_uid) = self
            .resolve_path(new_parent_rel)
            .map_err(|e| self.errno_error(e, "resolve new parent"))?;
        self.ensure_children(pino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;
        self.require_uid_writable(&new_parent_uid)
            .map_err(|error| self.errno_error(error, "copy access"))?;
        let (name, is_folder, unsaved) = {
            let st = self.state();
            let entry = st
                .entries
                .get(&ino)
                .ok_or_else(|| CoreError::not_found(format!("{} is gone", rel.display())))?;
            // Unsaved writes, or a release still queueing them, to the file or,
            // for a folder, to any file below it.
            let unsaved = st
                .active_writes
                .iter()
                .filter(|(_, w)| w.dirty)
                .map(|(&open, _)| open)
                .chain(st.releasing.iter().copied())
                .any(|open| relocate::ino_is_under(&st, open, ino));
            (entry.node.name.clone(), entry.node.is_folder(), unsaved)
        };
        // Unlike a move, a copy cannot wait in the queue: it needs the node as
        // Drive has it, so it goes now or not at all, local-first or not.
        if !self.is_online() {
            return Err(CoreError::offline());
        }
        let owed = unsaved
            || is_local_uid(&uid)
            || is_local_uid(&new_parent_uid)
            || self.db.has_any_op(&uid.to_string()).unwrap_or(true)
            || (is_folder
                && self
                    .db
                    .pending_op_counts()
                    .map(|counts| counts.uploads + counts.changes > 0)
                    .unwrap_or(true));
        if owed {
            return Err(CoreError::conflict(format!(
                "{name} has changes that have not reached Proton Drive yet; \
                 copy it once they have synced"
            )));
        }
        let target = if is_folder {
            AvailableNameTarget::Folder
        } else {
            AvailableNameTarget::File
        };
        let copy_name = self
            .rt
            .block_on(
                self.drive
                    .get_available_name(&new_parent_uid, &name, target),
            )
            .map_err(|e| CoreError::from_api(&e, "copy"))?;
        let new_uid = self
            .rt
            .block_on(
                self.drive
                    .copy_node(&uid, &new_parent_uid, Some(&copy_name)),
            )
            .map_err(|e| CoreError::from_api(&e, "copy"))?;
        let node = self
            .fetch_minted_node(&new_uid, &copy_name)
            .ok_or_else(|| minted_but_unlisted(&copy_name))?;
        let mut st = self.state();
        let copy_ino = st.intern(pino, node);
        if let Some(kids) = st.children.get_mut(&pino)
            && !kids.contains(&copy_ino)
        {
            kids.push(copy_ino);
        }
        drop(st);
        self.flush_access_changes();
        Ok(copy_name)
    }

    /// What the folder at `rel` holds on Proton Drive: bytes on storage and the
    /// number of items below it. Drive adds it up, so nothing is listed here;
    /// trashed items count, and the bytes are encrypted ones.
    pub(crate) fn folder_size(&self, rel: &Path) -> CoreResult<proton_drive_rs::FolderSize> {
        let (ino, uid) = self.resolve(rel)?;
        let is_folder = self
            .state()
            .entries
            .get(&ino)
            .is_some_and(|entry| entry.node.is_folder());
        if !is_folder {
            return Err(CoreError::invalid(format!(
                "{} is not a folder",
                rel.display()
            )));
        }
        self.folder_size_for_uid(&uid)
    }

    /// [`Core::folder_size`] for a folder addressed through any daemon location.
    pub(crate) fn folder_size_by_uid(&self, uid: &str) -> CoreResult<proton_drive_rs::FolderSize> {
        let uid = self.resolve_anywhere(uid)?;
        self.folder_size_for_uid(&uid)
    }

    pub(super) fn folder_size_for_uid(
        &self,
        uid: &NodeUid,
    ) -> CoreResult<proton_drive_rs::FolderSize> {
        if is_local_uid(uid) {
            return Err(CoreError::conflict("the folder is not on Proton Drive yet"));
        }
        if !self.is_online() {
            return Err(CoreError::offline());
        }
        self.rt
            .block_on(self.client.get_folder_size(uid))
            .map_err(|e| CoreError::from_api(&e, "folder size"))
    }

    /// Trash a file or folder. `rel` is mountpoint-relative. Forgets the node,
    /// evicts any cached content, and invalidates the parent listing.
    pub(super) fn delete(&self, rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let parent_uid = self.source_parent_uid(ino, rel)?;
        require_node_parent_access(
            |authority| self.require_uid_writable(authority),
            &uid,
            &parent_uid,
        )
        .map_err(|error| self.errno_error(error, "trash access"))?;
        if self.db.has_pending_moves_out_of(&uid.to_string())? {
            return Err(CoreError::conflict(
                "wait for pending moves out of this folder to sync before trashing it",
            ));
        }
        self.rt
            .block_on(self.drive.trash_nodes(std::slice::from_ref(&uid)))
            .and_then(batch::into_unit)
            .map_err(|e| CoreError::from_api(&e, "trash"))?;
        // Uploads queued for the node or anything below it would now only fail
        // against the trash, forever, holding their bytes (`docs/BUGS.md` B102).
        if let Err(error) = self.discard_queued_ops(&uid) {
            warn!(%uid, ?error, "remote trash landed but queued-op cleanup failed");
        }
        let name = self.forget_everywhere(&uid).unwrap_or_default();
        self.cache.evict(&uid);
        self.evict_reader(&uid);
        self.invalidate_parent_listing(rel);
        self.invalidate_trash();
        Ok(name)
    }

    /// Remove the node a `rename` is about to replace, so the new name is free
    /// for the API call that follows.
    ///
    /// `rename(2)` promises to replace an existing destination atomically. Proton
    /// offers no such primitive — `rename_node` refuses a name that is already
    /// taken — so this is the first half of an emulation that is *not* atomic:
    /// see [`Core::restore_replaced`] for the other half (`docs/BUGS.md` B13).
    ///
    /// A node whose own creation is still queued has never reached the server, so
    /// dropping its queued ops is the whole removal; nothing goes to the wire and
    /// it works offline. A create that landed before its ops were dropped left
    /// none: Drive has the node, so it is trashed there like any other
    /// (`docs/BUGS.md` B175).
    ///
    /// A handle still open on the replaced node keeps reading its bytes, as
    /// `rename(2)` promises: the inode stays as an unlinked entry and its
    /// staged bytes, cache and reader go at the final close, exactly as for an
    /// `unlink` under an open handle. Its queued ops go now, or the drain would
    /// upload to the node after it was trashed (`docs/BUGS.md` B106).
    pub(super) fn remove_replaced(&self, ino: u64, uid: &NodeUid, name: &str) -> Result<(), Errno> {
        // Asked under the uid the tree has now: a landed create's real uid is
        // not in the tree yet (`docs/BUGS.md` B257).
        let open = self.is_open_anywhere(uid);
        let landed;
        let uid = if is_local_uid(uid) {
            if open {
                self.withdraw_queued_ops(uid)?;
            } else {
                self.discard_queued_ops(uid)?;
            }
            self.for_each_state(|st| {
                st.forget_or_unlink(uid);
            });
            match self.landed_row_uid(ino) {
                Some(real) => {
                    debug!(local = %uid, %real, name, "create landed under a replace");
                    landed = real;
                    &landed
                }
                None => {
                    debug!(%uid, name, "replaced a node whose create was still queued");
                    return Ok(());
                }
            }
        } else {
            uid
        };
        // Local-first or offline: queue the trash and the rename that follows.
        if !self.sends_inline() {
            return self.queue_trash(uid, name, open);
        }
        if self.has_pending_moves_out_of(uid)? {
            return Err(Errno::EBUSY);
        }
        self.require_uid_writable(uid)?;
        if let Err(e) = self
            .block_on_bounded(self.drive.trash_nodes(std::slice::from_ref(uid)))
            .and_then(batch::into_unit)
        {
            if self.lost_link(&e, "trash a replaced node") {
                return self.queue_trash(uid, name, open);
            }
            error!(%uid, name, error = %e, "trashing the node a rename replaces failed");
            self.log_activity(ActivityKind::Trash, name, e.to_string(), false);
            return Err(Errno::EIO);
        }
        // The node was trashed on the server; withdraw it from every inode
        // space that had it, not only the mount the rename came through.
        let open_now = self.is_open_anywhere(uid);
        self.for_each_state(|st| {
            st.forget_or_unlink(uid);
        });
        self.hidden.lock().insert(uid.clone());
        if open_now {
            if let Err(error) = self.withdraw_queued_ops(uid) {
                error!(%uid, "remote replacement landed but queued-op cleanup failed");
                return Err(error);
            }
        } else {
            if let Err(error) = self.discard_queued_ops(uid) {
                error!(%uid, "remote replacement landed but queued-op cleanup failed");
                return Err(error);
            }
            self.cache.evict(uid);
            self.evict_reader(uid);
        }
        self.invalidate_trash();
        // The node is recoverable from the trash, but only if the user knows it
        // went there — a rename is not an operation anyone expects to trash
        // something, so this is the only record that it happened.
        self.log_activity(
            ActivityKind::Trash,
            name,
            "replaced by a rename from the mount",
            true,
        );
        Ok(())
    }

    /// Whether any mount holds an open handle on `uid`.
    pub(super) fn is_open_anywhere(&self, uid: &NodeUid) -> bool {
        let mut open = false;
        self.for_each_state(|st| {
            open |= st
                .by_uid
                .get(uid)
                .and_then(|ino| st.entries.get(ino))
                .is_some_and(|entry| entry.open_count > 0);
        });
        open
    }

    /// Put back the node [`Core::remove_replaced`] trashed, after the rename it
    /// was clearing the way for failed anyway.
    ///
    /// Best-effort by construction: if the restore also fails there is nothing
    /// further to try, and the node is still in the trash where `pdfs restore`
    /// can reach it. Says so loudly in that case, because the alternative is a
    /// file the user believes was only renamed quietly sitting in the trash.
    ///
    /// A restored node comes back to the mount as the ordinary restore brings
    /// it: out of `hidden`, and through a fresh listing of `parent` in every
    /// mount that shows it (`docs/BUGS.md` B124).
    pub(super) fn restore_replaced(
        &self,
        victim: Option<&(u64, NodeUid)>,
        parent: &NodeUid,
        name: &str,
    ) {
        let Some((_, uid)) = victim else { return };
        if is_local_uid(uid) {
            // Its queued create was discarded and cannot be reconstructed from
            // here; the caller's error is what the user acts on.
            warn!(%uid, name, "a rename failed after discarding a queued node it replaced");
            return;
        }
        match self
            .rt
            .block_on(self.drive.restore_nodes(std::slice::from_ref(uid)))
            .and_then(batch::into_unit)
        {
            Ok(()) => {
                self.hidden.lock().remove(uid);
                self.for_each_mount(|st, notify| {
                    if let Some(&ino) = st.by_uid.get(parent) {
                        st.invalidate_listing(ino);
                        notify.inval_entry(ino, name.to_string());
                    }
                });
                self.invalidate_trash();
                debug!(%uid, name, "restored the node a failed rename had replaced");
            }
            Err(e) => {
                error!(%uid, name, error = %e, "restoring a replaced node failed; it stays in the trash");
                self.log_activity(ActivityKind::Restore, name, e.to_string(), false);
            }
        }
    }

    /// Drop one folder's cached child listing (`rel` is mountpoint-relative), so
    /// the next `ListDir`/`readdir` re-enumerates it from the server. Backs
    /// [`CtlRequest::Refresh`] with a [`RefreshScope::Dir`] scope.
    pub(super) fn refresh_dir(&self, rel: &Path) -> CoreResult<()> {
        let (ino, _uid) = self.resolve(rel)?;
        self.state().invalidate_listing(ino);
        Ok(())
    }

    /// Parse wire uids (`volume~link`) into [`NodeUid`]s, rejecting the whole
    /// batch if any is malformed — a partial trash mutation is worse than none.
    pub(super) fn parse_uids(uids: &[String]) -> CoreResult<Vec<NodeUid>> {
        if uids.is_empty() {
            return Err(CoreError::invalid("no nodes given"));
        }
        uids.iter()
            .map(|u| parse_uid(u).ok_or_else(|| CoreError::invalid(format!("invalid uid: {u}"))))
            .collect()
    }

    /// Forget every trace of nodes that no longer exist anywhere: their inode and
    /// DB row, and their cached content.
    pub(super) fn drop_local(&self, uids: &[NodeUid]) {
        // These nodes are gone from the server, and a sync-folder fork showing
        // the same uid would otherwise keep serving them (`docs/BUGS.md` B74).
        self.for_each_state(|st| {
            for uid in uids {
                st.forget(uid);
            }
        });
        for uid in uids {
            self.cache.evict(uid);
            self.evict_reader(uid);
        }
    }

    /// Create a folder named `name` under the mountpoint-relative `parent_rel`.
    /// Interns the new node directly so it shows up without a re-enumeration.
    pub(super) fn create_folder(&self, parent_rel: &Path, name: &str) -> CoreResult<String> {
        if name.is_empty() || name.contains('/') {
            return Err(CoreError::invalid(format!("invalid name: {name:?}")));
        }
        let (pino, parent_uid) = self.resolve(parent_rel)?;
        self.ensure_children(pino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;
        self.require_uid_writable(&parent_uid)
            .map_err(|error| self.errno_error(error, "create folder access"))?;
        let new_uid = self
            .rt
            .block_on(
                self.drive
                    .create_folder(&parent_uid, name, Some(now_secs())),
            )
            .map_err(|e| CoreError::from_api(&e, "create folder"))?;
        let node = self
            .fetch_minted_node(&new_uid, name)
            .ok_or_else(|| minted_but_unlisted(name))?;
        let mut st = self.state();
        let ino = st.intern(pino, node);
        if let Some(kids) = st.children.get_mut(&pino)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        drop(st);
        self.flush_access_changes();
        Ok(name.to_string())
    }
}
