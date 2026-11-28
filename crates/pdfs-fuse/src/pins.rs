//! Pinning: keeping a file or a folder's files on this computer, and the
//! requests that list and undo pins. The keeper (`keeper.rs`) keeps them.

use super::*;

impl Core {
    /// Pin the node at mountpoint-relative `rel`. A file downloads its full
    /// plaintext into the content cache before this returns. A folder records a
    /// recursive pin and downloads every descendant file on a background thread,
    /// reported as a job: a big folder far outlasts the socket read timeout.
    /// Returns a human message.
    ///
    /// `recorded` is the path the pin registry keeps for the front ends to
    /// open the pin by: `rel` itself on the primary mount, which they join to
    /// the mountpoint, and the absolute local path on an on-demand folder's
    /// mount, whose node is not reachable under the primary mountpoint at all.
    pub(super) fn pin(&self, rel: &Path, recorded: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let (name, is_folder, mtime, size) = {
            let st = self.state();
            let e = st
                .entries
                .get(&ino)
                .ok_or_else(|| CoreError::not_found("node vanished"))?;
            (
                e.node.name.clone(),
                e.node.is_folder(),
                e.node.modification_time,
                node_size(&e.node),
            )
        };
        if is_folder {
            // Offline, every download would fail and the folder would only look
            // kept. Refuse instead, as a file pin does.
            if !self.is_online() {
                return Err(CoreError::offline());
            }
            // Record the recursive pin first so every descendant is eviction-
            // exempt before we start filling the cache with the subtree.
            self.cache
                .add_pin(&uid, recorded, true)
                .map_err(|e| CoreError::from_api(&e, "pin"))?;
            let core = self.clone();
            let folder = name.clone();
            self.services
                .spawn_job("pdfs-pin-folder", move || {
                    core.pin_subtree(ino, &uid, &folder)
                })
                .map_err(|e| CoreError::internal(format!("spawn pin: {e}")))?;
            return Ok(format!("{name}: downloading its files in the background"));
        }
        // Stored under a provisional size, the copy would never match the
        // plaintext it holds (`docs/BUGS.md` B282).
        let size = match self.kept_file(ino) {
            Some(file) if file.uid == uid => file.size,
            _ => size,
        };
        self.fetch_content(&uid, &name, mtime, size)?;
        self.cache
            .add_pin(&uid, recorded, false)
            .map_err(|e| CoreError::from_api(&e, "pin"))?;
        Ok(name)
    }

    /// Download and cache every file in the subtree rooted at the pinned folder
    /// `ino` (`folder`, named `name`), as a job the front ends show. Walks the
    /// tree depth-first, enumerating each folder so a cold subtree is fully
    /// discovered; the lock is dropped before each network download. Stops when
    /// the folder is unpinned or the daemon stops.
    pub(super) fn pin_subtree(&self, ino: u64, folder: &NodeUid, name: &str) {
        let job = self
            .transfers
            .begin_job(format!("Making {name} available offline"));
        match self.pin_subtree_files(ino) {
            Ok(files) => {
                job.set_total(files.len() as u64);
                let mut failed = 0usize;
                for ino in files {
                    if self.shutdown.is_stopping() || !self.cache.is_pinned(folder) {
                        return;
                    }
                    // A file with a queued write, or no readable size, is
                    // left to the drain and the keeper (`keeper::kept_file`).
                    if let Some(file) = self.kept_file(ino) {
                        job.detail(file.name.clone());
                        if let Err(e) =
                            self.fetch_pinned(&file.uid, &file.name, file.mtime, file.size)
                        {
                            warn!(uid = %file.uid, error = %e, "pin folder: download failed");
                            failed += 1;
                        }
                    }
                    job.step();
                }
                info!(folder = %name, failed, "pin folder: done");
            }
            Err(e) => warn!(folder = %name, error = %e, "pin folder: enumerate failed"),
        }
    }

    /// Cache the file `uid` for a folder pin. Its `mtime` and `size` were read
    /// before the download, and a create landing or a remote edit can change
    /// them while it runs. The blob is then stored under the old ones and reads
    /// as stale, so the file is listed as kept but not available offline. Fetch
    /// it again under the new ones, a few times at most.
    pub(super) fn fetch_pinned(
        &self,
        uid: &NodeUid,
        name: &str,
        mtime: i64,
        size: u64,
    ) -> CoreResult<()> {
        let mut fetched = (mtime, size);
        for _ in 0..3 {
            self.fetch_content(uid, name, fetched.0, fetched.1)?;
            let current = {
                let st = self.state();
                st.by_uid
                    .get(uid)
                    .and_then(|ino| st.entries.get(ino))
                    .map(|e| (e.node.modification_time, node_size(&e.node)))
            };
            match current {
                Some(now) if now != fetched => fetched = now,
                _ => break,
            }
        }
        Ok(())
    }

    /// The inode of every file in the subtree rooted at folder `ino`.
    pub(super) fn pin_subtree_files(&self, ino: u64) -> CoreResult<Vec<u64>> {
        let mut files: Vec<u64> = Vec::new();
        let mut stack = vec![ino];
        while let Some(dir) = stack.pop() {
            if self.shutdown.is_stopping() {
                break;
            }
            self.ensure_children(dir)
                .map_err(|e| self.errno_error(e, "enumerate"))?;
            let st = self.state();
            if let Some(kids) = st.children.get(&dir) {
                for &k in kids {
                    if let Some(e) = st.entries.get(&k) {
                        if e.node.is_folder() {
                            stack.push(k);
                        } else {
                            files.push(k);
                        }
                    }
                }
            }
        }
        Ok(files)
    }

    /// Unpin the node at `rel`, evicting its cached content. For a folder, also
    /// evicts every descendant's cached blob (the subtree is no longer kept).
    /// A node kept only because a folder above it is pinned is refused: there
    /// is no pin of its own to remove, and it would stay kept all the same.
    pub(super) fn unpin(&self, rel: &Path) -> CoreResult<String> {
        let (ino, uid) = self.resolve(rel)?;
        let (name, is_folder) = {
            let st = self.state();
            st.entries
                .get(&ino)
                .map(|e| (e.node.name.clone(), e.node.is_folder()))
                .unwrap_or_default()
        };
        if self.pinned_by_folder(ino) {
            return Err(CoreError::conflict(format!(
                "{name} is kept offline because a folder above it is; unpin that folder instead"
            )));
        }
        self.cache
            .remove_pin(&uid)
            .map_err(|e| CoreError::from_api(&e, "unpin"))?;
        // A recursively-pinned folder's descendants were eviction-exempt; now
        // that the pin is gone, reclaim their blobs eagerly instead of waiting
        // for budget pressure. Descendants come from the DB node tree. One
        // pinned on its own, or under a pinned subfolder, is still kept.
        if is_folder && let Ok(uids) = self.db.descendants(&uid.to_string()) {
            let still_pinned = self.pinned_set();
            for s in uids {
                if still_pinned.contains(&s) {
                    continue;
                }
                if let Some(u) = parse_uid(&s) {
                    self.cache.evict(&u);
                    self.evict_reader(&u);
                }
            }
        }
        Ok(name)
    }

    /// Drop the pins recorded under `path`, the path a front end lists a pin
    /// by, when their node is gone. A deleted node leaves no path to resolve,
    /// so [`unpin`](Self::unpin) cannot find it, and the pin would stay listed
    /// as kept offline for good. Returns whether any pin was dropped.
    pub(super) fn unpin_stale(&self, path: &str) -> CoreResult<bool> {
        let uids = self
            .cache
            .remove_pins_at(path)
            .map_err(|e| CoreError::from_api(&e, "unpin"))?;
        for uid in uids.iter().filter_map(|s| parse_uid(s)) {
            self.cache.evict(&uid);
            self.evict_reader(&uid);
        }
        Ok(!uids.is_empty())
    }

    /// Whether the node at `ino` is kept offline because a folder above it is
    /// pinned, whether or not it is also pinned on its own.
    pub(super) fn pinned_by_folder(&self, ino: u64) -> bool {
        let parent = {
            let st = self.state();
            st.entries
                .get(&ino)
                .filter(|e| e.parent != ino)
                .and_then(|e| st.entries.get(&e.parent))
                .map(|p| p.uid.clone())
        };
        parent.is_some_and(|uid| self.cache.is_pinned(&uid))
    }

    /// Every uid that is pinned right now, as one set.
    ///
    /// `cache.is_pinned` is a direct lookup plus an ancestor walk; asking it per
    /// candidate was a thousand walks per keystroke. The recursive expansion is
    /// the same query, run once.
    pub(super) fn pinned_set(&self) -> HashSet<String> {
        self.db
            .pinned_uids()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// Full-text search node names against the local SQLite index, mapping each
    /// DB hit to the wire [`SearchHit`] (resolving live pin state from the cache,
    /// which the DB doesn't track). Pure local lookup — never hits the network.
    /// Every pin, with `cached` saying whether a pinned file's content is on
    /// disk, so a front-end listing pins can badge them without a round-trip
    /// per file.
    pub(super) fn list_pins(&self) -> Vec<pdfs_core::control::Pin> {
        let mut pins = self.cache.list_pins();
        for pin in pins.iter_mut().filter(|pin| pin.is_dir == Some(false)) {
            if let Ok(Some(node)) = self.db.node_by_uid(&pin.uid) {
                pin.cached =
                    self.cache
                        .is_cached(&node.uid, node.modification_time, node_size(&node));
            }
        }
        pins
    }
}
