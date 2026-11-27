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
use pdfs_store::db::Db;
use proton_drive_rs::Node;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{debug, error, warn};

use crate::ids::is_local_uid;
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
    /// The parent folder is not in the tree.
    NotFound,
    /// `unlink` named a folder.
    IsDirectory,
    /// `rmdir` named a file.
    NotDirectory,
    /// `rmdir` named a folder that still has children.
    NotEmpty,
    /// Something failed that the service already logged: the scratch file of
    /// a new file.
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

    /// The child `name` of folder `parent`, as its inode and uid.
    fn lookup_child(&self, parent: u64, name: &str) -> Result<(u64, NodeUid), Self::Error>;

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

/// The namespace service of one daemon, shared by every mount: create,
/// mkdir, unlink and rmdir.
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
        if let Ok((ino, _)) = self.host.lookup_child(parent, name)
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
        if let Ok((ino, _)) = self.host.lookup_child(parent, name) {
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

    /// Trash the child `name` of folder `parent`, on Drive or through the
    /// queue.
    fn trash_child(&self, parent: u64, name: &str) -> Result<(), NamespaceError<H::Error>> {
        self.host.require_writable(parent)?;
        let (ino, mut uid) = self.host.lookup_child(parent, name)?;
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
        inline: bool,
        link_up: bool,
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

        fn lookup_child(&self, parent: u64, name: &str) -> Result<(u64, NodeUid), &'static str> {
            let st = self.state.lock();
            st.children
                .get(&parent)
                .into_iter()
                .flatten()
                .filter_map(|ino| st.entries.get(ino).map(|entry| (*ino, entry)))
                .find(|(_, entry)| entry.node.name == name)
                .map(|(ino, entry)| (ino, entry.uid.clone()))
                .ok_or("no such child")
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
                inline,
                link_up,
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
            self.host.lookup_child(self.dir, name).is_ok()
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
        let sub = fx.host.lookup_child(fx.dir, "sub").unwrap().0;
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
}
