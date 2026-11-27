//! The namespace service: create, mkdir, unlink and rmdir, and the rules for
//! a replacing rename.
//!
//! [`Namespace`] does what the FUSE adapter in `pdfs-fuse` used to do inside
//! its callbacks, and returns [`NamespaceError`] for the adapter to answer
//! with. What it needs from Drive and from the upload queue goes through
//! [`NamespaceHost`].
//!
//! The rules settle a replacing rename that a stop cut off, on the next start
//! (`docs/BUGS.md` B302). The journal entry is
//! [`pdfs_store::db::ReplaceIntent`].

use std::sync::Arc;

use pdfs_core::Access;
use pdfs_core::lockrank::Ranked;
use pdfs_core::syncignore::is_transient_name;
use pdfs_store::cache::ContentCache;
use pdfs_store::db::{Db, ReplaceIntent};
use proton_drive_rs::Node;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{debug, error, warn};

use crate::ids::is_local_uid;
use crate::names::{ReplaceError, check_replaceable};
use crate::open::WriteHandle;
use crate::tree::{State, StateGuard};
use crate::writes::WriteState;

/// What to do with a replacing rename a previous run started and did not end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceOutcome {
    /// It reached its end state, or nothing is left to undo: forget it.
    Done,
    /// The victim's trash is still queued but the rename never was: drop the
    /// queued trash, so the victim stays where it is.
    WithdrawVictimTrash,
    /// The rename did not happen and the victim is in Drive's trash: restore
    /// it.
    RestoreVictim,
    /// Drive cannot be asked now: try again later.
    Wait,
}

/// What the queue says about a cut-off replace: whether the source's rename
/// was queued, and whether the victim's trash is.
///
/// Answers without Drive where it can, which is every case the queued path
/// left: a queued rename carries the end state and the drain finishes it, and
/// a queued trash without a queued rename means the stop came between the two.
/// A victim whose own create was still queued had nothing on Drive to restore.
pub fn settle_from_queue(
    rename_queued: bool,
    victim_trash_queued: bool,
    victim_local: bool,
) -> Option<ReplaceOutcome> {
    if rename_queued {
        return Some(ReplaceOutcome::Done);
    }
    if victim_trash_queued {
        return Some(ReplaceOutcome::WithdrawVictimTrash);
    }
    if victim_local {
        return Some(ReplaceOutcome::Done);
    }
    None
}

/// What Drive says about a cut-off replace that went to Drive inline.
///
/// `source` and `victim` are `None` when Drive could not be asked, and
/// `Some(None)` when it does not have the node. The rename happened when the
/// source sits at the new name; then the victim's removal was meant. Otherwise
/// the victim comes back if it is in the trash, and is left alone if it never
/// got there.
pub fn settle_from_drive(
    new_parent: &NodeUid,
    new_name: &str,
    source: Option<Option<&Node>>,
    victim: Option<Option<&Node>>,
) -> ReplaceOutcome {
    let Some(source) = source else {
        return ReplaceOutcome::Wait;
    };
    let renamed = source.is_some_and(|node| {
        !node.trashed && node.parent_uid.as_ref() == Some(new_parent) && node.name == new_name
    });
    if renamed {
        return ReplaceOutcome::Done;
    }
    match victim {
        None => ReplaceOutcome::Wait,
        Some(Some(node)) if node.trashed => ReplaceOutcome::RestoreVictim,
        Some(_) => ReplaceOutcome::Done,
    }
}

/// Why the namespace service refused or failed an operation.
///
/// `E` is the host's own error ([`NamespaceHost::Error`]), passed through
/// unchanged.
#[derive(Debug)]
pub enum NamespaceError<E> {
    /// The node or its folder is not in the tree.
    NotFound,
    /// `unlink` named a folder, or a rename would put a file over one.
    IsDirectory,
    /// `rmdir` named a file, or a rename would put a folder over one.
    NotDirectory,
    /// `rmdir` named a folder that still has children, or a rename would
    /// replace one.
    NotEmpty,
    /// A rename that cannot be done: a folder into its own subtree, or an
    /// exchange of two names.
    InvalidArgument,
    /// A rename that may not replace found its destination taken.
    Exists,
    /// The queued create of a node went away under its rename: try again.
    Busy,
    /// Something failed that the service already logged: the scratch file of
    /// a new file, the journal of a replacing rename, or the queued create a
    /// rename rewrites.
    Io,
    /// The host failed, with its own error.
    Host(E),
}

impl<E> From<E> for NamespaceError<E> {
    fn from(error: E) -> Self {
        NamespaceError::Host(error)
    }
}

/// What the namespace service needs from the rest of the daemon: the access
/// rules, the listing of a folder, Drive, and the upload queue.
pub trait NamespaceHost {
    /// The error the host fails with.
    type Error: std::fmt::Debug;

    /// Refuse a change under `ino` that its access does not allow.
    fn require_writable(&self, ino: u64) -> Result<(), Self::Error>;

    /// Refuse a change to `uid` on Drive that its access does not allow.
    fn require_uid_writable(&self, uid: &NodeUid) -> Result<(), Self::Error>;

    /// Make sure the tree holds the children of folder `ino`.
    fn ensure_children(&self, ino: u64) -> Result<(), Self::Error>;

    /// The child `name` of folder `parent`, as its inode and uid, or `None`
    /// when the folder has no such child.
    fn lookup_child(&self, parent: u64, name: &str) -> Result<Option<(u64, NodeUid)>, Self::Error>;

    /// Whether a change goes to Drive now rather than into the queue.
    fn sends_inline(&self) -> bool;

    /// Create an empty file, or a folder when `folder`, named `name` under
    /// `parent_uid` on Drive, and return its node. `None` asks for the create
    /// to be queued instead: the link went down on the way, or Drive made the
    /// node but it could not be read back, which the queued create adopts by
    /// name.
    fn mint(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        folder: bool,
    ) -> Result<Option<Node>, Self::Error>;

    /// Queue the create of `name` under `parent_uid` and return the node it is
    /// known by until the drain sends it. A `parked` create waits for a rename
    /// to a real name before it is sent.
    fn queue_create(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        folder: bool,
        parked: bool,
    ) -> Result<Node, Self::Error>;

    /// Publish the access changes the tree recorded.
    fn access_changed(&self);

    /// Drop the queued operations of `uid`. `keep_staged` keeps the bytes an
    /// open handle still writes into.
    fn drop_queued(&self, uid: &NodeUid, keep_staged: bool) -> Result<(), Self::Error>;

    /// The uid the queued create of `ino` landed as, when it landed before its
    /// queued operations were dropped.
    fn landed_uid(&self, ino: u64) -> Option<NodeUid>;

    /// Whether Drive still holds children that were moved out of `uid` here
    /// and not sent yet.
    fn has_pending_moves_out_of(&self, uid: &NodeUid) -> Result<bool, Self::Error>;

    /// Queue the trash of `uid`. `open` keeps the bytes an open handle still
    /// writes into.
    fn queue_trash(&self, uid: &NodeUid, name: &str, open: bool) -> Result<(), Self::Error>;

    /// Trash `uid` on Drive. `Ok(false)` asks for the trash to be queued
    /// instead, as the link went down on the way. The host logs a failure.
    fn trash_remote(&self, uid: &NodeUid, name: &str) -> Result<bool, Self::Error>;

    /// Remove the node `uid` that a rename to `name` replaces: trash it on
    /// Drive or queue its trash.
    fn remove_replaced(&self, ino: u64, uid: &NodeUid, name: &str) -> Result<(), Self::Error>;

    /// Put back the node a failed rename to `name` under `parent` removed.
    fn restore_replaced(&self, victim: Option<&(u64, NodeUid)>, parent: &NodeUid, name: &str);

    /// Queue the rename of `uid` to `new_name` under `new_parent_uid`.
    /// `authorized` says the access to replace a destination was already
    /// checked.
    #[allow(clippy::too_many_arguments)]
    fn queue_rename(
        &self,
        ino: u64,
        uid: &NodeUid,
        old_parent_uid: &NodeUid,
        new_parent_ino: u64,
        new_parent_uid: &NodeUid,
        new_name: &str,
        authorized: bool,
    ) -> Result<(), Self::Error>;

    /// Move `uid` to `new_parent` and rename it to `new_name` on Drive, in
    /// one request; `None` leaves that part as it is. `Ok(false)` asks for
    /// the rename to be queued instead, as the link went down on the way. The
    /// host logs a failure.
    fn rename_remote(
        &self,
        uid: &NodeUid,
        new_parent: Option<&NodeUid>,
        new_name: Option<&str>,
    ) -> Result<bool, Self::Error>;

    /// Let the drain look at the queue again.
    fn wake_drain(&self);

    /// Drive accepted the trash of `uid`, and its queued operations and
    /// cached bytes are gone: forget what else the host keeps of it and record
    /// the trash. `open` means a handle still has it open, and reads of it
    /// still work.
    fn trashed(&self, uid: &NodeUid, name: &str, open: bool);
}

/// A node the namespace service made, as the tree holds it once made.
#[derive(Debug, Clone)]
pub struct Made {
    pub ino: u64,
    /// The write handle a new file is open under; 0 for a folder.
    pub fh: u64,
    pub node: Node,
    pub access: Access,
}

/// How a rename treats an existing destination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenameFlags {
    /// Fail with [`NamespaceError::Exists`] instead of replacing it.
    pub no_replace: bool,
    /// Swap the two names, which Drive cannot do.
    pub exchange: bool,
}

/// How a rename ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renamed {
    /// Drive has the new name, or nothing on Drive had to change.
    Done,
    /// The rename is queued. The kernel's cached entries for both names are
    /// stale: drop them after the answer, since the kernel updates its cache
    /// while it processes the answer and would overwrite an earlier
    /// invalidation.
    Queued,
}

/// Whether a rename is queued rather than sent now. Local-first, offline, or
/// into a folder not created on Drive yet, the API is not called in the
/// syscall. Only renames between nodes Drive has, sent inline, stay
/// synchronous, so a successful answer means Drive already has the end state.
pub fn rename_needs_queue(inline: bool, destination_is_local: bool) -> bool {
    !inline || destination_is_local
}

/// A replacing rename's journal entry, ended when the rename returns, however
/// it returns. Only a stop leaves it behind, and that is what it is for.
struct ReplaceJournal {
    db: Arc<Db>,
    source: String,
}

impl ReplaceJournal {
    fn begin(
        db: &Arc<Db>,
        source: &NodeUid,
        victim: &NodeUid,
        new_parent: &NodeUid,
        new_name: &str,
    ) -> pdfs_core::Result<Self> {
        let source = source.to_string();
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        db.begin_replace(&ReplaceIntent {
            source_uid: source.clone(),
            victim_uid: victim.to_string(),
            new_parent_uid: new_parent.to_string(),
            new_name: new_name.to_owned(),
            started_ms,
            run: 0,
        })?;
        Ok(Self {
            db: db.clone(),
            source,
        })
    }
}

impl Drop for ReplaceJournal {
    fn drop(&mut self) {
        if let Err(error) = self.db.end_replace(&self.source) {
            warn!(source = %self.source, %error, "ending a replacing rename's journal entry failed");
        }
    }
}

/// The namespace service of one daemon, shared by every mount: create,
/// mkdir, unlink, rmdir and rename.
pub struct Namespace<H> {
    state: Arc<Ranked<State>>,
    db: Arc<Db>,
    cache: Arc<ContentCache>,
    writes: WriteState,
    host: H,
}

impl<H: NamespaceHost> Namespace<H> {
    pub fn new(
        state: Arc<Ranked<State>>,
        db: Arc<Db>,
        cache: Arc<ContentCache>,
        writes: WriteState,
        host: H,
    ) -> Self {
        Self {
            state,
            db,
            cache,
            writes,
            host,
        }
    }

    fn state(&self) -> StateGuard<'_> {
        StateGuard::new(self.state.lock(), &self.db)
    }

    /// The uid of folder `parent`, after its children are in the tree.
    fn listed_parent(&self, parent: u64, what: &str) -> Result<NodeUid, NamespaceError<H::Error>> {
        self.host.require_writable(parent)?;
        if let Err(error) = self.host.ensure_children(parent) {
            // The parent was looked up a moment ago, so a failure here often
            // means it left the tree in between. Log it: the caller only sees
            // "no such file" on a create.
            warn!(
                parent,
                what,
                ?error,
                "the parent folder could not be listed"
            );
            return Err(NamespaceError::Host(error));
        }
        match self.state.lock().entries.get(&parent) {
            Some(entry) => Ok(entry.uid.clone()),
            None => {
                warn!(
                    parent,
                    what, "the parent folder left the tree during its listing"
                );
                Err(NamespaceError::NotFound)
            }
        }
    }

    /// Mint `name` under `parent_uid` on Drive when changes go there now and
    /// the parent exists there, or queue its create otherwise.
    ///
    /// Offline the server cannot mint a uid, so the queue invents one. A
    /// parent that is itself still queued forces the same path even online:
    /// the API has no folder to put this in yet. A create that set out online
    /// and lost the network on the way lands in the queue too, so a Wi-Fi drop
    /// costs the caller a wait, never an error.
    fn make(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        folder: bool,
        parked: bool,
    ) -> Result<Node, NamespaceError<H::Error>> {
        if !parked && self.host.sends_inline() && !is_local_uid(parent_uid) {
            self.host.require_uid_writable(parent_uid)?;
            if let Some(node) = self.host.mint(parent_uid, name, folder)? {
                return Ok(node);
            }
        }
        Ok(self.host.queue_create(parent_uid, name, folder, parked)?)
    }

    /// Create the file `name` in folder `parent` and open it for writing.
    /// Returns the file and its write handle.
    ///
    /// A transient scratch name, such as a browser's in-flight `*.crdownload`
    /// or an editor's `*.swp`, is kept local and parked: no remote node is
    /// minted and its bytes never upload while it has that name. The rename
    /// to the real name un-parks the create, so only the finished file reaches
    /// Drive. Minting an empty remote node here instead let a stalled and
    /// resumed download fork a conflict copy (`docs/BUGS.md` B70).
    pub fn create(&self, parent: u64, name: &str) -> Result<Made, NamespaceError<H::Error>> {
        let parent_uid = self.listed_parent(parent, "create")?;
        let node = self.make(&parent_uid, name, false, is_transient_name(name))?;
        let (file, path) = self.cache.create_scratch().map_err(|e| {
            error!(uid = %node.uid, error = %e, "create scratch file failed");
            NamespaceError::Io
        })?;
        let mut st = self.state();
        let ino = st.intern(parent, node.clone());
        if let Some(entry) = st.entries.get_mut(&ino) {
            entry.open_count = entry.open_count.saturating_add(1);
        }
        if let Some(kids) = st.children.get_mut(&parent)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        let fh = st
            .open
            .open_writer(ino, || WriteHandle::created(ino, &node, file, path));
        let made = made(&st, ino, fh);
        drop(st);
        self.host.access_changed();
        made
    }

    /// Create the folder `name` in folder `parent`.
    pub fn mkdir(&self, parent: u64, name: &str) -> Result<Made, NamespaceError<H::Error>> {
        let parent_uid = self.listed_parent(parent, "mkdir")?;
        let node = self.make(&parent_uid, name, true, false)?;
        let mut st = self.state();
        let local = is_local_uid(&node.uid);
        let uid = node.uid.clone();
        let ino = st.intern(parent, node);
        if let Some(kids) = st.children.get_mut(&parent)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        // A folder that exists only here has nothing to enumerate, and asking
        // the API to enumerate a `local~` uid would fail. Record it as fully
        // listed and empty, which it is, so reads stay offline across a
        // restart too.
        if local {
            st.children.insert(ino, Vec::new());
            st.record_listed(uid);
        }
        let made = made(&st, ino, 0);
        drop(st);
        self.host.access_changed();
        made
    }

    /// Remove the file `name` from folder `parent`.
    pub fn unlink(&self, parent: u64, name: &str) -> Result<(), NamespaceError<H::Error>> {
        self.host.require_writable(parent)?;
        if let Ok(Some((ino, _))) = self.host.lookup_child(parent, name)
            && self
                .state
                .lock()
                .entries
                .get(&ino)
                .is_some_and(|entry| entry.node.is_folder())
        {
            return Err(NamespaceError::IsDirectory);
        }
        self.trash_child(parent, name)
    }

    /// Remove the empty folder `name` from folder `parent`.
    pub fn rmdir(&self, parent: u64, name: &str) -> Result<(), NamespaceError<H::Error>> {
        self.host.require_writable(parent)?;
        if let Ok(Some((ino, _))) = self.host.lookup_child(parent, name) {
            if self
                .state
                .lock()
                .entries
                .get(&ino)
                .is_some_and(|entry| !entry.node.is_folder())
            {
                return Err(NamespaceError::NotDirectory);
            }
            // A missing cached listing says "unknown", not "empty". Proton's
            // trash call is recursive, so enumerate before applying rmdir's
            // POSIX emptiness rule.
            self.host.ensure_children(ino)?;
            if self.state.lock().has_children(ino) {
                return Err(NamespaceError::NotEmpty);
            }
        }
        self.trash_child(parent, name)
    }

    /// Rename the child `name` of folder `parent` to `newname` in folder
    /// `newparent`.
    ///
    /// `rename(2)` replaces an existing destination. Drive has no replacing
    /// rename, so the victim is removed first and the operation stops being
    /// atomic. Without that, Drive's refusal surfaced as an error and every
    /// write-to-temp-then-rename tool (rsync, atomic editor saves) failed at
    /// the very end of its transfer (`docs/BUGS.md` B13). A journal entry
    /// written before the victim is removed, and ended when this returns,
    /// lets a stop in between be settled on the next start (B302).
    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        flags: RenameFlags,
    ) -> Result<Renamed, NamespaceError<H::Error>> {
        self.host.require_writable(parent)?;
        self.host.require_writable(newparent)?;
        let (ino, mut uid) = self
            .host
            .lookup_child(parent, name)?
            .ok_or(NamespaceError::NotFound)?;
        // Moving a folder into itself or one of its descendants is forbidden
        // by POSIX.
        {
            let st = self.state.lock();
            if st.entries.get(&ino).is_some_and(|e| e.node.is_folder())
                && st.is_ancestor_of(ino, newparent)
            {
                return Err(NamespaceError::InvalidArgument);
            }
        }
        // The destination has to be listed either way: the queued path pushes
        // the node into that listing, and the online one drops it to force a
        // re-enumeration.
        if newparent != parent {
            self.host.ensure_children(newparent)?;
        }
        let (old_parent_uid, new_parent_uid) = {
            let st = self.state.lock();
            match (st.entries.get(&parent), st.entries.get(&newparent)) {
                (Some(old), Some(new)) => (old.uid.clone(), new.uid.clone()),
                _ => return Err(NamespaceError::NotFound),
            }
        };
        // `RENAME_EXCHANGE` has no Drive primitive and cannot be emulated
        // without a window in which one of the two names does not exist.
        if flags.exchange {
            return Err(NamespaceError::InvalidArgument);
        }
        let victim = match self.host.lookup_child(newparent, newname)? {
            // Renaming a node onto its own name is a no-op, never a
            // self-replace.
            Some((_, victim_uid)) if victim_uid == uid => None,
            found => found,
        };
        let _journal = match &victim {
            Some((victim_ino, victim_uid)) => {
                if flags.no_replace {
                    return Err(NamespaceError::Exists);
                }
                let journal = self.authorize_replace(
                    ino,
                    &uid,
                    *victim_ino,
                    victim_uid,
                    &old_parent_uid,
                    &new_parent_uid,
                    newname,
                )?;
                self.host
                    .remove_replaced(*victim_ino, victim_uid, newname)?;
                Some(journal)
            }
            None => None,
        };
        let replacing = victim.is_some();
        let restore = || {
            self.host
                .restore_replaced(victim.as_ref(), &new_parent_uid, newname)
        };
        // A node whose own creation is still queued has no identity on Drive
        // to rename: the queued operation is the node, so rewriting its target
        // is the whole rename. Nothing reaches the API, which is why this
        // works offline. A create that lands meanwhile leaves no operation to
        // rewrite; the node is then renamed by the uid Drive gave it
        // (`docs/BUGS.md` B142).
        if is_local_uid(&uid) {
            match self
                .db
                .rewrite_op_target(&uid.to_string(), &new_parent_uid.to_string(), newname)
            {
                Ok(true) => {
                    self.state()
                        .rename_in_place(ino, newparent, &new_parent_uid, newname);
                    // A create that failed on its old name is due again.
                    self.host.wake_drain();
                    // A transient scratch file (its create parked, bytes held
                    // back) renamed to a finished name is the moment the file
                    // is meant to reach Drive: un-park the create so the drain
                    // uploads it once (`docs/BUGS.md` B70). Renaming to another
                    // transient name leaves it parked.
                    if is_transient_name(name) && !is_transient_name(newname) {
                        match self.db.set_create_hold(&uid.to_string(), false) {
                            Ok(_) => {
                                self.host.wake_drain();
                                debug!(%uid, newname, "un-parked a finalized transient create");
                            }
                            Err(e) => {
                                warn!(%uid, error = %e, "un-parking a finalized transient create failed")
                            }
                        }
                    }
                    debug!(%uid, newname, "renamed a node whose create is still queued");
                    return Ok(Renamed::Done);
                }
                Ok(false) => match self.host.landed_uid(ino) {
                    Some(real) => {
                        debug!(local = %uid, %real, newname, "create landed under a rename");
                        uid = real;
                    }
                    None => {
                        warn!(%uid, name, newname, "queued create vanished under a rename");
                        restore();
                        return Err(NamespaceError::Busy);
                    }
                },
                Err(e) => {
                    error!(%uid, error = %e, "rewriting a queued create's target failed");
                    restore();
                    return Err(NamespaceError::Io);
                }
            }
        }
        // Offline, or into a folder that does not exist on Drive yet, the
        // rename is queued. The network going away under the inline path
        // lands there too, with whatever half already reached Drive: the
        // queued operation records the end state, so the drain finishes the
        // rest.
        let queue = || match self.host.queue_rename(
            ino,
            &uid,
            &old_parent_uid,
            newparent,
            &new_parent_uid,
            newname,
            replacing,
        ) {
            Ok(()) => Ok(Renamed::Queued),
            Err(error) => {
                restore();
                Err(NamespaceError::Host(error))
            }
        };
        if rename_needs_queue(self.host.sends_inline(), is_local_uid(&new_parent_uid)) {
            return queue();
        }
        // A failure past this point has already trashed any node the rename
        // was replacing, so put it back rather than leaving the caller with
        // neither the source moved nor the destination intact.
        if let Err(error) = self.require_rename_access(&uid, &old_parent_uid, &new_parent_uid) {
            restore();
            return Err(error);
        }
        // A move that also renames is one request, so the name only has to be
        // free in the destination, and the answer never stands for half of
        // it.
        let to_parent = (newparent != parent).then_some(&new_parent_uid);
        let to_name = (newname != name).then_some(newname);
        match self.host.rename_remote(&uid, to_parent, to_name) {
            Ok(true) => {}
            Ok(false) => return queue(),
            Err(error) => {
                restore();
                return Err(NamespaceError::Host(error));
            }
        }
        // Through the guard, so the database has both folders unlisted before
        // the answer. The next lookup rebuilds a folder still marked listed
        // from the database, which did not have the move yet
        // (`docs/BUGS.md` B135).
        self.state()
            .relocate(ino, parent, newparent, &new_parent_uid, newname);
        Ok(Renamed::Done)
    }

    /// Check that `uid` may replace the victim, and record the replace in the
    /// journal. Nothing is removed yet.
    #[allow(clippy::too_many_arguments)]
    fn authorize_replace(
        &self,
        ino: u64,
        uid: &NodeUid,
        victim_ino: u64,
        victim_uid: &NodeUid,
        old_parent_uid: &NodeUid,
        new_parent_uid: &NodeUid,
        newname: &str,
    ) -> Result<ReplaceJournal, NamespaceError<H::Error>> {
        // POSIX requires both ends to agree on being a directory, and the
        // check has to happen before anything is trashed: getting it wrong
        // turns a refusal into the destruction of the destination.
        let (src_dir, dst_dir) = {
            let st = self.state.lock();
            let is_dir = |i: u64| st.entries.get(&i).map(|e| e.node.is_folder());
            match (is_dir(ino), is_dir(victim_ino)) {
                (Some(a), Some(b)) => (a, b),
                _ => return Err(NamespaceError::NotFound),
            }
        };
        // Replacing a folder means trashing it, and Drive trashes a folder
        // with its whole subtree, so the destination's emptiness has to be
        // known before the decision is made.
        let dst_empty = if dst_dir {
            self.host.ensure_children(victim_ino)?;
            self.state
                .lock()
                .children
                .get(&victim_ino)
                .is_none_or(|kids| kids.is_empty())
        } else {
            true
        };
        check_replaceable(src_dir, dst_dir, dst_empty).map_err(|error| match error {
            ReplaceError::IsDirectory => NamespaceError::IsDirectory,
            ReplaceError::NotDirectory => NamespaceError::NotDirectory,
            ReplaceError::NotEmpty => NamespaceError::NotEmpty,
        })?;
        self.require_rename_access(uid, old_parent_uid, new_parent_uid)?;
        ReplaceJournal::begin(&self.db, uid, victim_uid, new_parent_uid, newname).map_err(|error| {
            error!(%uid, %error, "recording a replacing rename failed; refusing it");
            NamespaceError::Io
        })
    }

    /// A rename changes the node and both of its folders on Drive, so all
    /// three have to allow it.
    fn require_rename_access(
        &self,
        uid: &NodeUid,
        old_parent_uid: &NodeUid,
        new_parent_uid: &NodeUid,
    ) -> Result<(), NamespaceError<H::Error>> {
        for authority in [uid, old_parent_uid, new_parent_uid] {
            self.host.require_uid_writable(authority)?;
        }
        Ok(())
    }

    /// Trash the child `name` of folder `parent`, on Drive or through the
    /// queue.
    fn trash_child(&self, parent: u64, name: &str) -> Result<(), NamespaceError<H::Error>> {
        self.host.require_writable(parent)?;
        let (ino, mut uid) = self
            .host
            .lookup_child(parent, name)?
            .ok_or(NamespaceError::NotFound)?;
        let open_now = self
            .state
            .lock()
            .entries
            .get(&ino)
            .is_some_and(|e| e.open_count > 0);
        // A node the server has never heard of cannot be trashed there;
        // deleting it just means its queued creation is no longer wanted. This
        // works offline, which the remote path below cannot.
        if is_local_uid(&uid) {
            self.host.drop_queued(&uid, open_now)?;
            self.state().forget_or_unlink(&uid);
            // A create that landed before the operations were dropped left
            // none: Drive has the node, and the tree had not taken its uid
            // yet. It is trashed there like any other (`docs/BUGS.md` B173).
            match self.host.landed_uid(ino) {
                Some(real) => {
                    debug!(local = %uid, %real, name, "create landed under a delete");
                    uid = real;
                }
                None => {
                    debug!(%uid, name, "deleted a node that had not been created remotely yet");
                    return Ok(());
                }
            }
        }
        // Queue a folder trash while Drive still holds children that were
        // moved out locally. The drain waits for those moves before sending
        // it.
        let queue = !self.host.sends_inline() || self.host.has_pending_moves_out_of(&uid)?;
        if queue {
            return Ok(self.host.queue_trash(&uid, name, open_now)?);
        }
        self.host.require_uid_writable(&uid)?;
        if !self.host.trash_remote(&uid, name)? {
            return Ok(self.host.queue_trash(&uid, name, open_now)?);
        }
        // Only remove the entry after Drive accepted the trash. Otherwise an
        // error answer still made the path disappear here.
        self.writes.hide(&uid);
        self.state().forget_or_unlink(&uid);
        // An open handle keeps its staged bytes, and the drain must not send
        // them to a node that is already in the trash (`docs/BUGS.md` B106).
        if let Err(error) = self.host.drop_queued(&uid, open_now) {
            warn!(%uid, ?error, "remote trash landed but queued-op cleanup failed");
        }
        if !open_now {
            self.cache.evict(&uid);
        }
        self.host.trashed(&uid, name, open_now);
        Ok(())
    }
}

/// What the tree holds for `ino`, read under the same lock that made it, so
/// the answer cannot see a later unlink.
fn made<E>(st: &State, ino: u64, fh: u64) -> Result<Made, NamespaceError<E>> {
    let entry = st.entries.get(&ino).ok_or(NamespaceError::NotFound)?;
    Ok(Made {
        ino,
        fh,
        node: entry.node.clone(),
        access: entry.access,
    })
}

#[cfg(test)]
mod tests {
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    use super::*;

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn node(link: &str, parent: &str, name: &str, trashed: bool) -> Node {
        let mut node = crate::nodes::local_node(uid(link), uid(parent), name.to_owned(), false);
        node.trashed = trashed;
        node
    }

    #[test]
    fn the_queue_settles_what_the_queued_path_left() {
        use ReplaceOutcome::*;
        assert_eq!(settle_from_queue(true, true, false), Some(Done));
        assert_eq!(
            settle_from_queue(false, true, false),
            Some(WithdrawVictimTrash)
        );
        assert_eq!(settle_from_queue(false, false, true), Some(Done));
        assert_eq!(settle_from_queue(false, false, false), None);
    }

    /// The stop came after the victim went to the trash and before the
    /// source was renamed: the victim comes back.
    #[test]
    fn a_trashed_victim_comes_back_when_the_rename_did_not_happen() {
        let source = node("src", "dir", "report.odt.tmp", false);
        let victim = node("old", "dir", "report.odt", true);
        assert_eq!(
            settle_from_drive(
                &uid("dir"),
                "report.odt",
                Some(Some(&source)),
                Some(Some(&victim))
            ),
            ReplaceOutcome::RestoreVictim
        );
    }

    #[test]
    fn a_finished_rename_keeps_its_victim_in_the_trash() {
        let source = node("src", "dir", "report.odt", false);
        let victim = node("old", "dir", "report.odt", true);
        assert_eq!(
            settle_from_drive(
                &uid("dir"),
                "report.odt",
                Some(Some(&source)),
                Some(Some(&victim))
            ),
            ReplaceOutcome::Done
        );
    }

    /// The stop came before the victim was removed: nothing to undo.
    #[test]
    fn a_victim_never_trashed_is_left_alone() {
        let source = node("src", "dir", "report.odt.tmp", false);
        let victim = node("old", "dir", "report.odt", false);
        assert_eq!(
            settle_from_drive(
                &uid("dir"),
                "report.odt",
                Some(Some(&source)),
                Some(Some(&victim))
            ),
            ReplaceOutcome::Done
        );
        assert_eq!(
            settle_from_drive(&uid("dir"), "report.odt", Some(Some(&source)), Some(None)),
            ReplaceOutcome::Done
        );
    }

    #[test]
    fn without_drive_it_waits() {
        let source = node("src", "dir", "report.odt.tmp", false);
        assert_eq!(
            settle_from_drive(&uid("dir"), "report.odt", None, None),
            ReplaceOutcome::Wait
        );
        assert_eq!(
            settle_from_drive(&uid("dir"), "report.odt", Some(Some(&source)), None),
            ReplaceOutcome::Wait
        );
    }
}

#[cfg(test)]
mod service_tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use parking_lot::Mutex;
    use pdfs_core::lockrank::Rank;
    use pdfs_store::db::LOCAL_VOLUME;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

    use super::*;
    use crate::nodes::local_node;

    /// A unique temp directory removed on drop; avoids a dev-dependency.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-namespace-{}-{}",
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

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn local_uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from(LOCAL_VOLUME), LinkId::from(link))
    }

    /// A host that records what it was asked to do. `inline` says whether
    /// changes go to Drive now; `link_up` whether Drive answers when asked.
    struct Recorder {
        state: Arc<Ranked<State>>,
        db: Arc<Db>,
        inline: bool,
        link_up: bool,
        /// Drive refuses a rename outright.
        refuse_rename: bool,
        calls: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn record(&self, call: String) {
            self.calls.lock().push(call);
        }
    }

    impl NamespaceHost for &'static Recorder {
        type Error = &'static str;

        fn require_writable(&self, _ino: u64) -> Result<(), &'static str> {
            Ok(())
        }

        fn require_uid_writable(&self, _uid: &NodeUid) -> Result<(), &'static str> {
            Ok(())
        }

        fn ensure_children(&self, _ino: u64) -> Result<(), &'static str> {
            Ok(())
        }

        fn lookup_child(
            &self,
            parent: u64,
            name: &str,
        ) -> Result<Option<(u64, NodeUid)>, &'static str> {
            let st = self.state.lock();
            Ok(st
                .children
                .get(&parent)
                .into_iter()
                .flatten()
                .filter_map(|ino| st.entries.get(ino).map(|entry| (*ino, entry)))
                .find(|(_, entry)| entry.node.name == name)
                .map(|(ino, entry)| (ino, entry.uid.clone())))
        }

        fn sends_inline(&self) -> bool {
            self.inline
        }

        fn mint(
            &self,
            parent_uid: &NodeUid,
            name: &str,
            folder: bool,
        ) -> Result<Option<Node>, &'static str> {
            self.record(format!("mint {name}"));
            Ok(self.link_up.then(|| {
                local_node(
                    uid(&format!("minted-{name}")),
                    parent_uid.clone(),
                    name.to_owned(),
                    folder,
                )
            }))
        }

        fn queue_create(
            &self,
            parent_uid: &NodeUid,
            name: &str,
            folder: bool,
            parked: bool,
        ) -> Result<Node, &'static str> {
            self.record(format!("queue create {name} parked={parked}"));
            Ok(local_node(
                local_uid(&format!("queued-{name}")),
                parent_uid.clone(),
                name.to_owned(),
                folder,
            ))
        }

        fn access_changed(&self) {}

        fn drop_queued(&self, uid: &NodeUid, keep_staged: bool) -> Result<(), &'static str> {
            self.record(format!("drop queued {uid} keep_staged={keep_staged}"));
            Ok(())
        }

        fn landed_uid(&self, _ino: u64) -> Option<NodeUid> {
            None
        }

        fn has_pending_moves_out_of(&self, _uid: &NodeUid) -> Result<bool, &'static str> {
            Ok(false)
        }

        fn queue_trash(&self, uid: &NodeUid, _name: &str, open: bool) -> Result<(), &'static str> {
            self.record(format!("queue trash {uid} open={open}"));
            Ok(())
        }

        fn trash_remote(&self, uid: &NodeUid, _name: &str) -> Result<bool, &'static str> {
            self.record(format!("trash {uid}"));
            Ok(self.link_up)
        }

        fn trashed(&self, uid: &NodeUid, _name: &str, open: bool) {
            self.record(format!("trashed {uid} open={open}"));
        }

        fn remove_replaced(
            &self,
            ino: u64,
            uid: &NodeUid,
            _name: &str,
        ) -> Result<(), &'static str> {
            // The tests replace with the node "a": its journal entry has to be
            // on disk before anything is removed.
            let journaled = self
                .db
                .state_str(&format!("replace:{}", self::uid("a")))
                .unwrap()
                .is_some();
            self.record(format!("remove replaced {uid} journaled={journaled}"));
            let _ = ino;
            StateGuard::new(self.state.lock(), &self.db).forget_or_unlink(uid);
            Ok(())
        }

        fn restore_replaced(
            &self,
            victim: Option<&(u64, NodeUid)>,
            _parent: &NodeUid,
            _name: &str,
        ) {
            if let Some((_, uid)) = victim {
                self.record(format!("restore {uid}"));
            }
        }

        fn queue_rename(
            &self,
            _ino: u64,
            uid: &NodeUid,
            _old_parent_uid: &NodeUid,
            _new_parent_ino: u64,
            _new_parent_uid: &NodeUid,
            new_name: &str,
            authorized: bool,
        ) -> Result<(), &'static str> {
            self.record(format!(
                "queue rename {uid} to {new_name} authorized={authorized}"
            ));
            Ok(())
        }

        fn rename_remote(
            &self,
            uid: &NodeUid,
            new_parent: Option<&NodeUid>,
            new_name: Option<&str>,
        ) -> Result<bool, &'static str> {
            self.record(format!(
                "rename {uid} parent={} name={}",
                new_parent.map_or("-".to_owned(), |p| p.to_string()),
                new_name.unwrap_or("-")
            ));
            if self.refuse_rename {
                return Err("refused");
            }
            Ok(self.link_up)
        }

        fn wake_drain(&self) {}
    }

    struct Fixture {
        namespace: Namespace<&'static Recorder>,
        host: &'static Recorder,
        state: Arc<Ranked<State>>,
        db: Arc<Db>,
        /// The folder the tests work in.
        dir: u64,
        _dir: TempDir,
    }

    impl Fixture {
        fn new(inline: bool, link_up: bool) -> Self {
            Self::with(inline, link_up, false)
        }

        fn with(inline: bool, link_up: bool, refuse_rename: bool) -> Self {
            let tmp = TempDir::new();
            let db = Arc::new(Db::open(&tmp.0.join("cache.db")).unwrap());
            let cache = Arc::new(
                ContentCache::open(tmp.0.join("content"), tmp.0.join("pins"), 0, db.clone())
                    .unwrap(),
            );
            let state = Arc::new(Ranked::new(
                Rank::State,
                State::new(db.clone(), HashMap::new()),
            ));
            let host: &'static Recorder = Box::leak(Box::new(Recorder {
                state: state.clone(),
                db: db.clone(),
                inline,
                link_up,
                refuse_rename,
                calls: Mutex::default(),
            }));
            let dir = {
                let mut st = StateGuard::new(state.lock(), &db);
                let node = local_node(uid("dir"), uid("root"), "dir".to_owned(), true);
                let ino = st.intern(0, node);
                st.entries.get_mut(&ino).unwrap().access = Access::Owner;
                st.children.insert(ino, Vec::new());
                ino
            };
            let namespace = Namespace::new(
                state.clone(),
                db.clone(),
                cache,
                WriteState::default(),
                host,
            );
            Fixture {
                namespace,
                host,
                state,
                db,
                dir,
                _dir: tmp,
            }
        }

        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.host.calls.lock())
        }

        fn listed(&self, name: &str) -> bool {
            self.child(self.dir, name).is_some()
        }

        fn child(&self, parent: u64, name: &str) -> Option<(u64, NodeUid)> {
            self.host.lookup_child(parent, name).unwrap()
        }

        /// Put a node Drive has into folder `parent`, and return its inode.
        fn put(&self, parent: u64, link: &str, folder: bool) -> u64 {
            let parent_uid = self.state.lock().entries[&parent].uid.clone();
            let node = local_node(uid(link), parent_uid, link.to_owned(), folder);
            let mut st = self.state();
            let ino = st.intern(parent, node);
            st.children.get_mut(&parent).unwrap().push(ino);
            if folder {
                st.children.insert(ino, Vec::new());
            }
            ino
        }

        fn state(&self) -> StateGuard<'_> {
            StateGuard::new(self.state.lock(), &self.db)
        }
    }

    #[test]
    fn an_online_create_mints_the_file_and_opens_it_for_writing() {
        let fx = Fixture::new(true, true);
        let made = fx.namespace.create(fx.dir, "a.txt").unwrap();
        assert_eq!(fx.calls(), ["mint a.txt"]);
        assert_eq!(made.node.uid, uid("minted-a.txt"));
        assert!(made.fh > 0);
        assert!(fx.listed("a.txt"));
        let st = fx.state();
        assert_eq!(st.entries[&made.ino].open_count, 1);
        assert!(st.open.writer(made.ino).is_some());
    }

    #[test]
    fn a_create_that_loses_the_link_is_queued() {
        let fx = Fixture::new(true, false);
        let made = fx.namespace.create(fx.dir, "a.txt").unwrap();
        assert_eq!(
            fx.calls(),
            ["mint a.txt", "queue create a.txt parked=false"]
        );
        assert!(is_local_uid(&made.node.uid));
    }

    #[test]
    fn a_transient_name_is_parked_and_never_minted() {
        let fx = Fixture::new(true, true);
        fx.namespace.create(fx.dir, "song.mp3.crdownload").unwrap();
        assert_eq!(fx.calls(), ["queue create song.mp3.crdownload parked=true"]);
    }

    #[test]
    fn an_offline_mkdir_is_queued_and_listed_empty() {
        let fx = Fixture::new(false, true);
        let made = fx.namespace.mkdir(fx.dir, "sub").unwrap();
        assert_eq!(fx.calls(), ["queue create sub parked=false"]);
        assert_eq!(made.fh, 0);
        assert_eq!(fx.state().children.get(&made.ino), Some(&Vec::new()));
        // Written through when the guard drops: a restart lists it from the
        // database without asking Drive.
        let listed = fx.db.children_if_listed(&made.node.uid).unwrap();
        assert!(listed.is_some_and(|kids| kids.is_empty()));
    }

    #[test]
    fn unlink_and_rmdir_keep_to_their_kind_of_node() {
        let fx = Fixture::new(true, true);
        fx.namespace.mkdir(fx.dir, "sub").unwrap();
        fx.namespace.create(fx.dir, "a.txt").unwrap();
        let sub = fx.child(fx.dir, "sub").unwrap().0;
        fx.namespace.create(sub, "inner.txt").unwrap();
        fx.calls();

        assert!(matches!(
            fx.namespace.unlink(fx.dir, "sub"),
            Err(NamespaceError::IsDirectory)
        ));
        assert!(matches!(
            fx.namespace.rmdir(fx.dir, "a.txt"),
            Err(NamespaceError::NotDirectory)
        ));
        assert!(matches!(
            fx.namespace.rmdir(fx.dir, "sub"),
            Err(NamespaceError::NotEmpty)
        ));
        assert!(fx.calls().is_empty());
        assert!(fx.listed("sub") && fx.listed("a.txt"));
    }

    #[test]
    fn an_open_file_trashed_on_drive_keeps_its_bytes() {
        let fx = Fixture::new(true, true);
        let made = fx.namespace.create(fx.dir, "a.txt").unwrap();
        fx.calls();
        fx.namespace.unlink(fx.dir, "a.txt").unwrap();
        let uid = made.node.uid;
        assert_eq!(
            fx.calls(),
            [
                format!("trash {uid}"),
                format!("drop queued {uid} keep_staged=true"),
                format!("trashed {uid} open=true"),
            ]
        );
        assert!(!fx.listed("a.txt"));
        assert!(fx.state().entries[&made.ino].unlinked);
    }

    #[test]
    fn a_trash_that_loses_the_link_is_queued_and_keeps_the_entry_until_then() {
        let fx = Fixture::new(true, false);
        let on_drive = uid("on-drive");
        let ino = {
            let node = local_node(on_drive.clone(), uid("dir"), "b.txt".to_owned(), false);
            let mut st = fx.state();
            let ino = st.intern(fx.dir, node);
            st.children.get_mut(&fx.dir).unwrap().push(ino);
            ino
        };
        fx.namespace.unlink(fx.dir, "b.txt").unwrap();
        assert_eq!(
            fx.calls(),
            [
                format!("trash {on_drive}"),
                format!("queue trash {on_drive} open=false"),
            ]
        );
        // The queued trash hides it; the service does not drop it ahead.
        assert!(fx.state().entries.contains_key(&ino));
    }

    #[test]
    fn deleting_a_queued_create_drops_it_without_a_trash() {
        let fx = Fixture::new(false, false);
        let made = fx.namespace.create(fx.dir, "a.txt").unwrap();
        fx.state().entries.get_mut(&made.ino).unwrap().open_count = 0;
        fx.calls();
        fx.namespace.unlink(fx.dir, "a.txt").unwrap();
        let uid = made.node.uid;
        assert_eq!(fx.calls(), [format!("drop queued {uid} keep_staged=false")]);
        assert!(!fx.listed("a.txt"));
    }

    #[test]
    fn an_online_move_is_one_request_and_moves_the_entry() {
        let fx = Fixture::new(true, true);
        let sub = fx.put(fx.dir, "sub", true);
        let a = fx.put(fx.dir, "a", false);
        let renamed = fx
            .namespace
            .rename(fx.dir, "a", sub, "b", RenameFlags::default())
            .unwrap();
        assert_eq!(renamed, Renamed::Done);
        assert_eq!(
            fx.calls(),
            [format!("rename {} parent={} name=b", uid("a"), uid("sub"))]
        );
        // Both folders are unlisted so the next lookup asks Drive again
        // (`docs/BUGS.md` B135); the entry already has its new place.
        assert!(!fx.listed("a"));
        let st = fx.state();
        assert_eq!(st.entries[&a].parent, sub);
        assert_eq!(st.entries[&a].node.name, "b");
    }

    #[test]
    fn an_offline_rename_is_queued() {
        let fx = Fixture::new(false, false);
        fx.put(fx.dir, "a", false);
        let renamed = fx
            .namespace
            .rename(fx.dir, "a", fx.dir, "b", RenameFlags::default())
            .unwrap();
        assert_eq!(renamed, Renamed::Queued);
        assert_eq!(
            fx.calls(),
            [format!("queue rename {} to b authorized=false", uid("a"))]
        );
    }

    #[test]
    fn a_rename_that_loses_the_link_is_queued() {
        let fx = Fixture::new(true, false);
        fx.put(fx.dir, "a", false);
        let renamed = fx
            .namespace
            .rename(fx.dir, "a", fx.dir, "b", RenameFlags::default())
            .unwrap();
        assert_eq!(renamed, Renamed::Queued);
        assert_eq!(
            fx.calls(),
            [
                format!("rename {} parent=- name=b", uid("a")),
                format!("queue rename {} to b authorized=false", uid("a")),
            ]
        );
    }

    #[test]
    fn a_folder_cannot_move_into_itself() {
        let fx = Fixture::new(true, true);
        let sub = fx.put(fx.dir, "sub", true);
        let inner = fx.put(sub, "inner", true);
        assert!(matches!(
            fx.namespace
                .rename(fx.dir, "sub", inner, "sub", RenameFlags::default()),
            Err(NamespaceError::InvalidArgument)
        ));
        assert!(matches!(
            fx.namespace.rename(
                fx.dir,
                "sub",
                fx.dir,
                "other",
                RenameFlags {
                    exchange: true,
                    ..RenameFlags::default()
                }
            ),
            Err(NamespaceError::InvalidArgument)
        ));
        assert!(fx.calls().is_empty());
    }

    #[test]
    fn a_refused_replace_removes_nothing() {
        let fx = Fixture::new(true, true);
        fx.put(fx.dir, "a", false);
        let sub = fx.put(fx.dir, "sub", true);
        fx.put(sub, "inner", false);
        fx.put(fx.dir, "b", false);
        let no_replace = RenameFlags {
            no_replace: true,
            ..RenameFlags::default()
        };
        assert!(matches!(
            fx.namespace.rename(fx.dir, "a", fx.dir, "b", no_replace),
            Err(NamespaceError::Exists)
        ));
        assert!(matches!(
            fx.namespace
                .rename(fx.dir, "a", fx.dir, "sub", RenameFlags::default()),
            Err(NamespaceError::IsDirectory)
        ));
        fx.put(fx.dir, "empty", true);
        assert!(matches!(
            fx.namespace
                .rename(fx.dir, "empty", fx.dir, "sub", RenameFlags::default()),
            Err(NamespaceError::NotEmpty)
        ));
        assert!(fx.calls().is_empty());
        assert!(fx.db.open_replaces().unwrap().is_empty());
    }

    #[test]
    fn a_replace_journals_before_removing_and_ends_the_entry() {
        let fx = Fixture::new(true, true);
        let a = fx.put(fx.dir, "a", false);
        fx.put(fx.dir, "b", false);
        let renamed = fx
            .namespace
            .rename(fx.dir, "a", fx.dir, "b", RenameFlags::default())
            .unwrap();
        assert_eq!(renamed, Renamed::Done);
        assert_eq!(
            fx.calls(),
            [
                format!("remove replaced {} journaled=true", uid("b")),
                format!("rename {} parent=- name=b", uid("a")),
            ]
        );
        assert_eq!(fx.state().entries[&a].node.name, "b");
        // Ended when the rename returned.
        assert!(
            fx.db
                .state_str(&format!("replace:{}", uid("a")))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_replace_drive_refuses_brings_the_victim_back() {
        let fx = Fixture::with(true, true, true);
        fx.put(fx.dir, "a", false);
        fx.put(fx.dir, "b", false);
        assert!(matches!(
            fx.namespace
                .rename(fx.dir, "a", fx.dir, "b", RenameFlags::default()),
            Err(NamespaceError::Host("refused"))
        ));
        assert_eq!(
            fx.calls(),
            [
                format!("remove replaced {} journaled=true", uid("b")),
                format!("rename {} parent=- name=b", uid("a")),
                format!("restore {}", uid("b")),
            ]
        );
        assert!(fx.listed("a"));
    }

    #[test]
    fn a_rename_whose_queued_create_vanished_asks_to_try_again() {
        // The recording host keeps no queue in the database, so the create
        // this local node stands for is gone, and Drive never landed it.
        let fx = Fixture::new(false, false);
        fx.namespace.create(fx.dir, "a").unwrap();
        fx.calls();
        assert!(matches!(
            fx.namespace
                .rename(fx.dir, "a", fx.dir, "b", RenameFlags::default()),
            Err(NamespaceError::Busy)
        ));
        assert!(fx.calls().is_empty());
        assert!(fx.listed("a"));
    }
}
