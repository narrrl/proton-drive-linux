//! Finding nodes: by path or uid in the tree, in the database, and on Drive,
//! including a create that landed while its placeholder was still in use.

use super::*;

impl Core {
    /// Resolve a child `name` within `parent` to its `(inode, uid)`, ensuring
    /// the parent's listing is cached first.
    pub(super) fn lookup_child(&self, parent: u64, name: &str) -> Result<(u64, NodeUid), Errno> {
        self.ensure_children(parent)?;
        let st = self.state();
        st.children
            .get(&parent)
            .and_then(|kids| {
                kids.iter().copied().find_map(|ino| {
                    st.entries
                        .get(&ino)
                        .filter(|e| e.node.name == name)
                        .map(|e| (ino, e.uid.clone()))
                })
            })
            .ok_or(Errno::ENOENT)
    }

    /// Walk a mountpoint-relative path to its `(inode, uid)`, enumerating each
    /// directory on the way as needed. Leading `/` and `.` components are
    /// ignored; `..` is rejected.
    pub(super) fn resolve_path(&self, rel: &Path) -> Result<(u64, NodeUid), Errno> {
        let mut ino = self.root_ino();
        let mut uid = {
            let st = self.state();
            st.entries
                .get(&ino)
                .map(|e| e.uid.clone())
                .ok_or(Errno::ENOENT)?
        };
        for comp in rel.components() {
            match comp {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => {
                    let (child_ino, child_uid) = self.lookup_child(ino, &name.to_string_lossy())?;
                    ino = child_ino;
                    uid = child_uid;
                }
                _ => return Err(Errno::EINVAL),
            }
        }
        Ok((ino, uid))
    }

    /// [`resolve_path`](Self::resolve_path) for the request-serving side.
    ///
    /// The kernel-facing resolver answers in `Errno` because that is what the
    /// FUSE reply needs. A front-end needs the opposite: `{:?}` of a libc
    /// constant is not something to show a user, and "does not exist" and "the
    /// API is down" want different copy and different buttons. This is the one
    /// place that knows enough to tell them apart, so it is the place that does.
    pub(super) fn resolve(&self, rel: &Path) -> CoreResult<(u64, NodeUid)> {
        self.resolve_path(rel)
            .map_err(|e| self.errno_error(e, &format!("could not resolve {}", rel.display())))
    }

    /// Resolve a wire-format uid only when one of this daemon's locations
    /// proves that it owns the node.
    ///
    /// The primary and on-demand mounts prove residency through the tree and
    /// the roots in [`StateRegistry`]. Mirror locations have no tree, so their root
    /// and last-synced descendants are resolved through `sync_folder` and
    /// `sync_entry` instead.
    pub(crate) fn resolve_anywhere(&self, uid: &str) -> CoreResult<NodeUid> {
        resolve_anywhere_with(
            uid,
            |uid| self.owns_visible_uid(uid),
            |uid| self.db.mirror_contains_uid(&uid.to_string()),
        )
    }

    /// [`Core::resolve_anywhere`], widened to the Photos timeline for sharing.
    ///
    /// A photo lives on the photos volume, which no location shows, so the
    /// residency proof above never finds it. The timeline in the `photos` table
    /// is this daemon's proof instead: it lists only photos on the user's own
    /// volume, which are theirs to share. A photo in an album someone else
    /// shared is not in it and stays refused.
    pub(crate) fn resolve_shareable(&self, uid: &str) -> CoreResult<NodeUid> {
        resolve_anywhere_with(
            uid,
            |uid| self.owns_visible_uid(uid),
            |uid| {
                let uid = uid.to_string();
                Ok(self.db.mirror_contains_uid(&uid)?
                    || !self
                        .db
                        .photos_by_uid(std::slice::from_ref(&uid))?
                        .is_empty())
            },
        )
    }

    pub(super) fn source_parent_uid(&self, ino: u64, rel: &Path) -> CoreResult<NodeUid> {
        let state = self.state();
        let entry = state
            .entries
            .get(&ino)
            .ok_or_else(|| CoreError::not_found(format!("could not resolve {}", rel.display())))?;
        entry
            .node
            .parent_uid
            .clone()
            .or_else(|| {
                state
                    .entries
                    .get(&entry.parent)
                    .map(|parent| parent.uid.clone())
            })
            .ok_or_else(|| CoreError::invalid("the mount root cannot be mutated"))
    }

    /// Classify a failure that arrived as an `Errno`.
    ///
    /// The internal paths speak `Errno` because they also serve FUSE, where a
    /// number is the whole vocabulary. Everything crossing the control socket
    /// has to be turned back into something a person can read, and this is the
    /// one place that knows how — a call site holding only an `Errno` has
    /// already lost the context needed to say what went wrong.
    ///
    /// `Errno` is neither `PartialEq` nor structural-match, so this compares
    /// raw codes rather than the `libc` constants.
    pub(super) fn errno_error(&self, e: Errno, what: &str) -> CoreError {
        match e.code() {
            libc::ENOENT => CoreError::not_found(format!("{what}: no such file or folder")),
            libc::EACCES | libc::EPERM => CoreError::denied(format!("{what}: not allowed")),
            // These paths walk the tree lazily, so a cold node needs the API.
            // Offline that surfaces as EIO, which on its own would read to the
            // user as a broken file rather than a missing network.
            libc::EIO if !self.online.load(Ordering::Relaxed) => CoreError::offline(),
            libc::EINVAL => CoreError::invalid(format!("{what}: not a usable path")),
            libc::ENOSPC => CoreError::new(ErrorKind::Quota, format!("{what}: out of space")),
            libc::EEXIST => CoreError::conflict(format!("{what}: already exists")),
            libc::ENOTEMPTY => CoreError::conflict(format!("{what}: folder is not empty")),
            _ => CoreError::internal(format!("{what}: {e:?}")),
        }
    }

    /// Fetch a single node's current metadata from the remote.
    pub(super) fn fetch_node(&self, uid: &NodeUid) -> Result<Node, Errno> {
        match self.fetch_node_remote(uid) {
            Ok(Some(node)) => Ok(node),
            Ok(None) => Err(Errno::ENOENT),
            Err(e) => {
                if !self.lost_link(&e, "fetch node") {
                    error!(%uid, error = %e, "enumerate node failed");
                }
                Err(Errno::EIO)
            }
        }
    }

    /// Read back a node just created on the remote.
    ///
    /// Drive can answer a create before a lookup lists the new node, so an
    /// empty answer is asked again for a moment. `None` means the node exists
    /// but could not be read back. The mount then queues the create, and the
    /// drain finds the node by name and adopts it: the application gets its
    /// file or folder either way, never an `ENOENT` for something that exists.
    pub(super) fn fetch_minted_node(&self, uid: &NodeUid, name: &str) -> Option<Node> {
        // The feed echoes the create. Unclaimed, it would take the echo the
        // upload of the file's bytes records, and that one would then evict
        // the bytes just uploaded (docs/BUGS.md B123).
        self.note_self_change(uid);
        let node = self.read_back(uid).ok();
        if node.is_none() {
            warn!(%uid, name, "Drive made the node but did not hand it back");
        }
        node
    }

    /// [`Core::fetch_node`] for a node Drive has just made, asked again for a
    /// moment while Drive does not list it yet.
    ///
    /// A file Drive lists without an active revision is not listed yet either:
    /// the create commits one before it answers, so this answer is from before
    /// the commit, with no size. Taken, it emptied the new file in the tree,
    /// and a read of it right after found nothing (`docs/BUGS.md` B163).
    pub(crate) fn read_back(&self, uid: &NodeUid) -> Result<Node, Errno> {
        let mut delays = MINTED_READ_BACK_DELAYS.iter();
        loop {
            let fetched = match self.fetch_node(uid) {
                Ok(node) if is_unrevised(&node) => Err(Errno::ENOENT),
                fetched => fetched,
            };
            match fetched {
                Err(e)
                    if e == Errno::ENOENT
                        && let Some(delay) = delays.next() =>
                {
                    std::thread::sleep(*delay);
                }
                fetched => return fetched,
            }
        }
    }

    /// [`Core::fetch_node`] without the collapse to an `Errno`, for the drain:
    /// resolving a conflict turns on *why* a call failed, and "the node is not
    /// there" (`Ok(None)`) is a different outcome from "we could not ask".
    pub(super) fn fetch_node_remote(&self, uid: &NodeUid) -> Result<Option<Node>, ProtonError> {
        match self.block_on_bounded(self.drive.enumerate_nodes(std::slice::from_ref(uid))) {
            Ok(nodes) => Ok(nodes.into_iter().next()),
            // An unknown uid is reported either as an empty result or as an
            // outright refusal, depending on the endpoint.
            Err(e) if is_gone(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Point a handle closed while its file's create landed at the uid Drive
    /// gave the node.
    ///
    /// `Core::retire_create` moves the row to that uid before it repoints the
    /// open handles. A handle closed in between still named the placeholder,
    /// which no row has any more, and its write was refused as if access had
    /// changed. The inode is the row's local id, which finds it either way.
    pub(crate) fn follow_landed_create(&self, h: &mut WriteHandle) {
        if !is_local_uid(&h.uid) {
            return;
        }
        if let Some(real) = self.landed_row_uid(h.ino) {
            debug!(local = %h.uid, %real, "write handle closed as its create landed");
            h.uid = real;
        }
    }

    /// The uid Drive gave the node at `ino`, once its create has landed, or
    /// `None` while it is still queued. Read from the row, which has it before
    /// the tree does.
    pub(crate) fn landed_row_uid(&self, ino: u64) -> Option<NodeUid> {
        state::ino_lid(ino).and_then(|lid| self.landed_lid_uid(lid))
    }

    /// [`Core::landed_row_uid`] for a node known by its stand-in `uid`.
    pub(crate) fn landed_stand_in_uid(&self, uid: &NodeUid) -> Option<NodeUid> {
        pdfs_store::db::local_lid(&uid.to_string()).and_then(|lid| self.landed_lid_uid(lid))
    }

    pub(super) fn landed_lid_uid(&self, lid: i64) -> Option<NodeUid> {
        self.db
            .uid_of_lid(lid)
            .ok()
            .flatten()
            .and_then(|uid| parse_node_uid(&uid))
            .filter(|uid| !is_local_uid(uid))
    }
}
