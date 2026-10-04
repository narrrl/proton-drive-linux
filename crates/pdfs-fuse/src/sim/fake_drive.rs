//! An in-memory Drive for the simulation tests (`docs/MILESTONE-3.0.0.md` §8.1).
//!
//! [`FakeDrive`] is the server: one tree of nodes with uids, revisions and
//! names, and an event log. Each simulated daemon reaches it through its own
//! [`FakeClient`], which implements [`DriveApi`] and carries what is particular
//! to one connection: whether its link is up, how slow it is, which faults it
//! injects, and an SDK-style entity cache.
//!
//! The faults are the ones Drive is known to have:
//!
//! - **Listing lag.** A read returns a node as it was [`Faults::listing_lag`]
//!   ago, so a create is not listed at once and a trashed node still is (B118,
//!   B48). A rename, move or new revision can lag by [`Faults::update_lag`],
//!   which Drive has not been seen doing; the presets leave it at zero.
//! - **Stale name hash.** A rename or move sends the name the client's entity
//!   cache holds, and Drive refuses it with `InvalidRequirements` when the node
//!   has been renamed or moved since. Like the real SDK, the cache is filled by
//!   reads and only emptied by `invalidate_caches_for_event`, not by the
//!   client's own rename (B111, B121).
//! - **Re-stamped modification time.** A rename or move sets a node's
//!   modification time to the time it happened, though its revision stays
//!   (B113).
//! - **No atomic replace.** A name that is taken refuses a create, upload,
//!   rename, move or restore with `AlreadyExists` (B13, B104, B106).
//! - **Events** delayed, duplicated or reordered, and our own changes echoed
//!   back, because every change is logged for every client (B70, B105).
//! - **A request** that stalls, fails before it reaches Drive, or succeeds and
//!   then loses its reply (B107, B121's half state).
//! - **Latency** per request, from a range ([`Faults::lan`], [`Faults::wifi`]).
//! - **Non-uniform blocks** ([`FakeDrive::set_block_pattern`]; B84, B85, B87).
//! - **A full account** ([`FakeDrive::set_quota`]) refusing uploads with
//!   `InsufficientQuota`.
//!
//! A test can also hold the next read of one node on its way to Drive
//! ([`FakeClient::hold_next_read`]), to make a change while the daemon waits
//! for the answer, or on its way back ([`FakeClient::hold_answer_to_next_read`]),
//! so the daemon gets an answer a change made meanwhile has overtaken. It can
//! also have a new file's next reads answered as Drive lists it before its
//! create is committed ([`FakeClient::answer_reads_unrevised`]; B163), and
//! hold a folder's listing on its way back with one of its files listed that
//! way ([`FakeClient::hold_answer_to_next_listing`]).

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::FutureExt as _;
use futures::StreamExt as _;
use parking_lot::Mutex;
use proton_drive_rs::proton_sdk::account::Quota;
use proton_drive_rs::proton_sdk::api::ResponseCode;
use proton_drive_rs::proton_sdk::error::{ProtonApiError, ProtonError, Result};
use proton_drive_rs::proton_sdk::ids::{DriveEventId, LinkId, NodeUid, VolumeId};
use proton_drive_rs::{
    DriveEvent, DriveEventScopeId, Node, NodeKind, NodeMoveItem, RevisionState, Thumbnail,
};
use sha1::{Digest as _, Sha1};

use super::rng::Rng;
use crate::drive::{DriveApi, NodeOutcomes, OutcomeStream, RevisionRead};

/// The volume every fake node lives on.
pub(crate) const VOLUME: &str = "simvol";

/// How long superseded versions of a node are kept for lagging reads. Longer
/// than any lag a test sets.
const HISTORY: Duration = Duration::from_secs(120);

/// What a file holds on the fake Drive.
#[derive(Clone, Debug)]
struct FileData {
    media_type: String,
    revision: String,
    content: Arc<Vec<u8>>,
    block_sizes: Vec<u64>,
    /// Every revision the file has had, oldest first, the active one last.
    revisions: Vec<(String, Arc<Vec<u8>>)>,
}

/// One node as Drive has it at one moment.
#[derive(Clone, Debug)]
struct Remote {
    parent: Option<NodeUid>,
    name: String,
    trashed: bool,
    created: i64,
    modified: i64,
    file: Option<FileData>,
}

/// One entry of the event log.
struct Logged {
    seq: u64,
    at: Instant,
    event: DriveEvent,
}

struct Server {
    root: NodeUid,
    /// Every version of every node within [`HISTORY`], oldest first. `None` is
    /// a permanent delete.
    nodes: HashMap<NodeUid, Vec<(Instant, Option<Remote>)>>,
    next_link: u64,
    next_revision: u64,
    events: Vec<Logged>,
    /// Plaintext block sizes of new content, cycled; the last one repeats.
    block_pattern: Vec<u64>,
    /// The account's storage limit in bytes.
    max_space: i64,
}

impl Server {
    fn current(&self, uid: &NodeUid) -> Option<&Remote> {
        self.nodes.get(uid)?.last()?.1.as_ref()
    }

    /// The node as a read `lag` behind the present sees it.
    fn visible(&self, uid: &NodeUid, lag: Lag, now: Instant) -> Option<Remote> {
        let versions = self.nodes.get(uid)?;
        let live = |remote: &Option<Remote>| remote.as_ref().is_some_and(|r| !r.trashed);
        let update = |i: usize| i > 0 && live(&versions[i - 1].1) && live(&versions[i].1);
        let passed =
            |i: usize| versions[i].0 + if update(i) { lag.update } else { lag.appear } <= now;
        // An update shows only once the node it updates has appeared.
        let appeared = |mut i: usize| {
            while update(i) {
                i -= 1;
            }
            passed(i)
        };
        (0..versions.len())
            .rev()
            .find(|&i| passed(i) && appeared(i))
            .and_then(|i| versions[i].1.clone())
    }

    fn live_child_named(&self, parent: &NodeUid, name: &str) -> Option<NodeUid> {
        self.nodes.iter().find_map(|(uid, versions)| {
            let remote = versions.last()?.1.as_ref()?;
            (remote.parent.as_ref() == Some(parent) && remote.name == name && !remote.trashed)
                .then(|| uid.clone())
        })
    }

    fn is_folder(&self, uid: &NodeUid) -> bool {
        self.current(uid)
            .is_some_and(|remote| remote.file.is_none() && !remote.trashed)
    }

    fn is_below(&self, uid: &NodeUid, ancestor: &NodeUid) -> bool {
        let mut cursor = Some(uid.clone());
        while let Some(at) = cursor {
            if &at == ancestor {
                return true;
            }
            cursor = self.current(&at).and_then(|remote| remote.parent.clone());
        }
        false
    }

    fn mint_uid(&mut self) -> NodeUid {
        self.next_link += 1;
        uid(&format!("L{}", self.next_link))
    }

    fn mint_revision(&mut self) -> String {
        self.next_revision += 1;
        format!("rev{}", self.next_revision)
    }

    fn blocks_of(&self, len: u64) -> Vec<u64> {
        let mut sizes = Vec::new();
        let mut left = len;
        let mut i = 0;
        while left > 0 {
            let size = self.block_pattern[i.min(self.block_pattern.len() - 1)].min(left);
            sizes.push(size);
            left -= size;
            i += 1;
        }
        sizes
    }

    /// Record a new version of `uid` and log the change.
    fn put(&mut self, uid: &NodeUid, remote: Option<Remote>) {
        let now = Instant::now();
        let event_id = DriveEventId::from(format!("ev{}", self.events.len() + 1));
        let event = match &remote {
            Some(remote) => DriveEvent::NodeUpdated {
                id: event_id,
                node_uid: uid.clone(),
                parent_node_uid: remote.parent.clone(),
                is_trashed: remote.trashed,
                is_shared: false,
            },
            None => DriveEvent::NodeDeleted {
                id: event_id,
                node_uid: uid.clone(),
                parent_node_uid: self.current(uid).and_then(|r| r.parent.clone()),
            },
        };
        let versions = self.nodes.entry(uid.clone()).or_default();
        versions.push((now, remote));
        while versions.len() > 1 && versions[1].0 + HISTORY < now {
            versions.remove(0);
        }
        let seq = self.events.len() as u64 + 1;
        self.events.push(Logged {
            seq,
            at: now,
            event,
        });
    }

    /// Bytes stored: every revision of every node not deleted, trashed or not,
    /// as Drive counts them.
    fn used_space(&self) -> i64 {
        self.nodes
            .values()
            .filter_map(|versions| versions.last()?.1.as_ref()?.file.as_ref())
            .flat_map(|file| &file.revisions)
            .map(|(_, content)| content.len() as i64)
            .sum()
    }

    fn make_room(&self, bytes: usize) -> Result<()> {
        if self.used_space() + bytes as i64 > self.max_space {
            return Err(api(
                ResponseCode::InsufficientQuota,
                "storage quota exceeded",
            ));
        }
        Ok(())
    }

    fn new_file(
        &mut self,
        parent: &NodeUid,
        name: &str,
        media_type: &str,
        content: Vec<u8>,
        mtime: Option<i64>,
    ) -> Result<NodeUid> {
        if !self.is_folder(parent) {
            return Err(api(ResponseCode::DoesNotExist, "parent folder not found"));
        }
        if self.live_child_named(parent, name).is_some() {
            return Err(api(
                ResponseCode::AlreadyExists,
                "a file or folder with that name already exists",
            ));
        }
        self.make_room(content.len())?;
        let uid = self.mint_uid();
        let revision = self.mint_revision();
        let content = Arc::new(content);
        let now = now_secs();
        let file = FileData {
            media_type: media_type.to_owned(),
            revision: revision.clone(),
            block_sizes: self.blocks_of(content.len() as u64),
            content: content.clone(),
            revisions: vec![(revision, content)],
        };
        self.put(
            &uid,
            Some(Remote {
                parent: Some(parent.clone()),
                name: name.to_owned(),
                trashed: false,
                created: now,
                modified: mtime.unwrap_or(now),
                file: Some(file),
            }),
        );
        Ok(uid)
    }
}

/// An in-memory Drive shared by every simulated client. See the module
/// documentation.
pub(crate) struct FakeDrive {
    server: Mutex<Server>,
}

/// A node of [`FakeDrive::tree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Folder,
    File(Arc<Vec<u8>>),
}

impl FakeDrive {
    pub(crate) fn new() -> Arc<Self> {
        let root = uid("root");
        let mut server = Server {
            root: root.clone(),
            nodes: HashMap::new(),
            next_link: 0,
            next_revision: 0,
            events: Vec::new(),
            block_pattern: vec![pdfs_core::cache::BLOCK_SIZE],
            max_space: 1 << 40,
        };
        let now = now_secs();
        server.nodes.insert(
            root,
            vec![(
                Instant::now() - HISTORY,
                Some(Remote {
                    parent: None,
                    name: "root".into(),
                    trashed: false,
                    created: now,
                    modified: now,
                    file: None,
                }),
            )],
        );
        Arc::new(Self {
            server: Mutex::new(server),
        })
    }

    /// A connection to this Drive for one simulated daemon.
    pub(crate) fn client(self: &Arc<Self>, seed: u64, faults: Faults) -> FakeClient {
        FakeClient(Arc::new(ClientInner {
            drive: self.clone(),
            faults: Mutex::new(faults),
            rng: Mutex::new(Rng::new(seed)),
            entity_cache: Mutex::new(HashMap::new()),
            online: AtomicBool::new(true),
            requests: AtomicU64::new(0),
            hold: Mutex::new(None),
            hold_answer: Mutex::new(None),
            hold_trash: Mutex::new(None),
            drop_after_create: AtomicBool::new(false),
            lose_create_reply: AtomicBool::new(false),
            hold_create_reply: Mutex::new(None),
            lose_revision_reply: AtomicBool::new(false),
            lose_revision_read_back: AtomicBool::new(false),
            lose_read: Mutex::new(None),
            unrevised_reads: Mutex::new(None),
            hold_listing: Mutex::new(None),
        }))
    }

    /// Plaintext block sizes for content uploaded from now on, cycled with the
    /// last one repeating: `[1 MiB, 4 MiB]` makes a short first block.
    pub(crate) fn set_block_pattern(&self, pattern: Vec<u64>) {
        assert!(!pattern.is_empty() && pattern.iter().all(|&size| size > 0));
        self.server.lock().block_pattern = pattern;
    }

    /// The account's storage limit; a limit below what is stored refuses every
    /// upload until something is deleted.
    pub(crate) fn set_quota(&self, max_space: i64) {
        self.server.lock().max_space = max_space;
    }

    pub(crate) fn root(&self) -> NodeUid {
        self.server.lock().root.clone()
    }

    /// The live tree as Drive has it now, by path below the root: every node
    /// that is neither trashed nor below a trashed folder.
    pub(crate) fn tree(&self) -> BTreeMap<String, Entry> {
        let server = self.server.lock();
        let mut tree = BTreeMap::new();
        for uid in server.nodes.keys() {
            if *uid == server.root {
                continue;
            }
            let mut names = Vec::new();
            let mut cursor = Some(uid.clone());
            let mut live = true;
            while let Some(at) = cursor {
                if at == server.root {
                    break;
                }
                match server.current(&at) {
                    Some(remote) if !remote.trashed => {
                        names.push(remote.name.clone());
                        cursor = remote.parent.clone();
                    }
                    _ => {
                        live = false;
                        break;
                    }
                }
            }
            if !live {
                continue;
            }
            names.reverse();
            let entry = match &server.current(uid).and_then(|r| r.file.clone()) {
                Some(file) => Entry::File(file.content.clone()),
                None => Entry::Folder,
            };
            tree.insert(names.join("/"), entry);
        }
        tree
    }

    /// The uid at `path` below the root, as Drive has it now.
    pub(crate) fn lookup(&self, path: &str) -> Option<NodeUid> {
        let server = self.server.lock();
        let mut at = server.root.clone();
        for name in path.split('/').filter(|name| !name.is_empty()) {
            at = server.live_child_named(&at, name)?;
        }
        Some(at)
    }

    /// Every change Drive has logged, oldest first.
    pub(crate) fn event_count(&self) -> usize {
        self.server.lock().events.len()
    }

    /// Changes made by another device, straight on the server: what a second
    /// client would do, without the daemon that client would need.
    pub(crate) fn device(&self) -> Device<'_> {
        Device(self)
    }
}

/// See [`FakeDrive::device`].
pub(crate) struct Device<'a>(&'a FakeDrive);

impl Device<'_> {
    /// Write `content` at `path`, as a new file or a new revision of one.
    pub(crate) fn write(&self, path: &str, content: &[u8]) -> NodeUid {
        let (parent, name) = split_path(path);
        let parent = self
            .0
            .lookup(parent)
            .expect("device write into a missing folder");
        let mut server = self.0.server.lock();
        match server.live_child_named(&parent, name) {
            Some(uid) => {
                let mut remote = server.current(&uid).cloned().expect("live child");
                let revision = server.mint_revision();
                let content = Arc::new(content.to_vec());
                let blocks = server.blocks_of(content.len() as u64);
                let file = remote.file.as_mut().expect("device write over a folder");
                file.revisions.push((revision.clone(), content.clone()));
                file.revision = revision;
                file.content = content;
                file.block_sizes = blocks;
                remote.modified = now_secs();
                server.put(&uid, Some(remote));
                uid
            }
            None => server
                .new_file(
                    &parent,
                    name,
                    "application/octet-stream",
                    content.to_vec(),
                    None,
                )
                .expect("device create"),
        }
    }

    /// Move the node at `path` to the trash.
    pub(crate) fn trash(&self, path: &str) {
        let uid = self.0.lookup(path).expect("device trash of a missing node");
        let mut server = self.0.server.lock();
        let mut remote = server.current(&uid).cloned().expect("live node");
        remote.trashed = true;
        server.put(&uid, Some(remote));
    }

    /// Rename the node at `path` in place.
    pub(crate) fn rename(&self, path: &str, new_name: &str) {
        let uid = self
            .0
            .lookup(path)
            .expect("device rename of a missing node");
        let mut server = self.0.server.lock();
        let mut remote = server.current(&uid).cloned().expect("live node");
        remote.name = new_name.to_owned();
        remote.modified = now_secs();
        server.put(&uid, Some(remote));
    }
}

/// How far behind the present a read is, by the kind of change.
#[derive(Clone, Copy, Debug)]
struct Lag {
    appear: Duration,
    update: Duration,
}

/// The faults and timing of one connection. See the module documentation.
#[derive(Clone, Debug)]
pub(crate) struct Faults {
    /// Each request takes a time drawn from this range, in milliseconds.
    pub(crate) latency_ms: (u64, u64),
    /// How far behind the present reads are about a node appearing or going
    /// to the trash.
    pub(crate) listing_lag: Duration,
    /// How far behind the present reads are about a live node's name, folder
    /// or revision.
    pub(crate) update_lag: Duration,
    /// How long a change takes to reach the event feed.
    pub(crate) event_delay: Duration,
    /// Chance that an event is delivered twice.
    pub(crate) duplicate_events: f64,
    /// Chance that two neighbouring events of a page swap places.
    pub(crate) reorder_events: f64,
    /// Chance that a request never reaches Drive.
    pub(crate) fail: f64,
    /// Chance that a change Drive made loses its reply.
    pub(crate) drop_reply: f64,
    /// Chance that a request stalls for [`Faults::stall_for`] and then fails.
    pub(crate) stall: f64,
    pub(crate) stall_for: Duration,
    /// Whether renames and moves send the entity cache's name, as the SDK does.
    pub(crate) stale_name_cache: bool,
}

impl Faults {
    /// No latency and no faults beyond the SDK's own stale name cache.
    pub(crate) fn none() -> Self {
        Self {
            latency_ms: (0, 0),
            listing_lag: Duration::ZERO,
            update_lag: Duration::ZERO,
            event_delay: Duration::ZERO,
            duplicate_events: 0.0,
            reorder_events: 0.0,
            fail: 0.0,
            drop_reply: 0.0,
            stall: 0.0,
            stall_for: Duration::ZERO,
            stale_name_cache: true,
        }
    }

    /// A good link: a few milliseconds a request, listings barely behind.
    pub(crate) fn lan() -> Self {
        Self {
            latency_ms: (2, 15),
            listing_lag: Duration::from_millis(50),
            event_delay: Duration::from_millis(200),
            ..Self::none()
        }
    }

    /// A good link whose event feed is slow and untidy: every echo of our own
    /// changes comes late, often twice, often out of order.
    pub(crate) fn echoes() -> Self {
        Self {
            event_delay: Duration::from_secs(1),
            duplicate_events: 0.3,
            reorder_events: 0.3,
            ..Self::lan()
        }
    }

    /// The Wi-Fi of the 2026-10 account runs: slow and lossy, with listings
    /// and events well behind.
    pub(crate) fn wifi() -> Self {
        Self {
            latency_ms: (60, 400),
            listing_lag: Duration::from_millis(800),
            event_delay: Duration::from_secs(2),
            duplicate_events: 0.05,
            reorder_events: 0.1,
            fail: 0.02,
            drop_reply: 0.02,
            stall: 0.005,
            stall_for: Duration::from_secs(3),
            ..Self::none()
        }
    }
}

struct ClientInner {
    drive: Arc<FakeDrive>,
    faults: Mutex<Faults>,
    rng: Mutex<Rng>,
    /// What the SDK's entity cache holds: filled by reads, emptied only by
    /// `invalidate_caches_for_event`.
    entity_cache: Mutex<HashMap<NodeUid, Node>>,
    online: AtomicBool,
    requests: AtomicU64,
    /// The read [`FakeClient::hold_next_read`] is waiting for.
    hold: Mutex<Option<Arc<HoldState>>>,
    /// The read [`FakeClient::hold_answer_to_next_read`] is waiting for.
    hold_answer: Mutex<Option<Arc<HoldState>>>,
    /// The trash [`FakeClient::hold_next_trash`] is waiting for.
    hold_trash: Mutex<Option<Arc<HoldState>>>,
    /// Set by [`FakeClient::drop_link_after_next_create`].
    drop_after_create: AtomicBool,
    /// Set by [`FakeClient::lose_reply_to_next_create`].
    lose_create_reply: AtomicBool,
    /// The create [`FakeClient::hold_reply_to_next_create`] is waiting for.
    hold_create_reply: Mutex<Option<Arc<HoldState>>>,
    /// Set by [`FakeClient::lose_reply_to_next_revision`].
    lose_revision_reply: AtomicBool,
    /// Set by [`FakeClient::lose_read_back_of_next_revision`].
    lose_revision_read_back: AtomicBool,
    /// The file whose next read goes unanswered, once its revision is made.
    lose_read: Mutex<Option<NodeUid>>,
    /// Set by [`FakeClient::answer_reads_unrevised`]: the file and how many
    /// more of its reads are answered without an active revision.
    unrevised_reads: Mutex<Option<(NodeUid, usize)>>,
    /// The listing [`FakeClient::hold_answer_to_next_listing`] is waiting
    /// for, and the file it lists without an active revision.
    hold_listing: Mutex<Option<(Arc<HoldState>, NodeUid)>>,
}

struct HoldState {
    uid: NodeUid,
    reached: AtomicBool,
    released: AtomicBool,
}

/// Wait while `state` holds a call.
/// `node` as Drive lists a file whose create it has not committed yet: no
/// active revision, no size.
fn unrevise(node: &mut Node) {
    if let NodeKind::File {
        total_size_on_storage,
        active_revision_state,
        active_revision_id,
        claimed_size,
        content_sha1,
        ..
    } = &mut node.kind
    {
        *total_size_on_storage = 0;
        *active_revision_state = None;
        *active_revision_id = None;
        *claimed_size = None;
        *content_sha1 = None;
    }
}

async fn wait_until_released(state: &HoldState) {
    state.reached.store(true, Ordering::SeqCst);
    while !state.released.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A call held on its way to or from Drive, from [`FakeClient::hold_next_read`]
/// or [`FakeClient::hold_reply_to_next_create`]. It goes on when this is
/// dropped.
pub(crate) struct Held(Arc<HoldState>);

impl Held {
    /// Whether the read has come and is being held.
    pub(crate) fn reached(&self) -> bool {
        self.0.reached.load(Ordering::SeqCst)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.released.store(true, Ordering::SeqCst);
    }
}

/// One daemon's connection to a [`FakeDrive`]. Cheap to clone; clones share
/// the link, the faults and the entity cache.
#[derive(Clone)]
pub(crate) struct FakeClient(Arc<ClientInner>);

impl FakeClient {
    pub(crate) fn set_online(&self, online: bool) {
        self.0.online.store(online, Ordering::SeqCst);
    }

    pub(crate) fn set_faults(&self, faults: Faults) {
        *self.0.faults.lock() = faults;
    }

    /// Take the link down as soon as the next file create has been answered,
    /// so whatever the daemon asks next goes unanswered.
    pub(crate) fn drop_link_after_next_create(&self) {
        self.0.drop_after_create.store(true, Ordering::SeqCst);
    }

    /// Make the next file create on Drive and lose its answer, as a link that
    /// drops it on the way back does.
    pub(crate) fn lose_reply_to_next_create(&self) {
        self.0.lose_create_reply.store(true, Ordering::SeqCst);
    }

    /// Hold the next read of `uid` alone, before it reaches Drive, until the
    /// answer is dropped. Only one read is held at a time.
    pub(crate) fn hold_next_read(&self, uid: &NodeUid) -> Held {
        let state = Arc::new(HoldState {
            uid: uid.clone(),
            reached: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        *self.0.hold.lock() = Some(state.clone());
        Held(state)
    }

    /// Answer the next read of `uid` alone with what Drive holds now, then
    /// hold the answer until it is dropped, as a slow link does. Only one
    /// answer is held at a time.
    pub(crate) fn hold_answer_to_next_read(&self, uid: &NodeUid) -> Held {
        let state = Arc::new(HoldState {
            uid: uid.clone(),
            reached: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        *self.0.hold_answer.lock() = Some(state.clone());
        Held(state)
    }

    /// Hold the next trash of `uid` alone, before it reaches Drive, until the
    /// answer is dropped.
    pub(crate) fn hold_next_trash(&self, uid: &NodeUid) -> Held {
        let state = Arc::new(HoldState {
            uid: uid.clone(),
            reached: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        *self.0.hold_trash.lock() = Some(state.clone());
        Held(state)
    }

    /// Make the next file create on Drive, then hold its answer until the
    /// answer is dropped, as a slow link does.
    pub(crate) fn hold_reply_to_next_create(&self) -> Held {
        let state = Arc::new(HoldState {
            uid: self.0.drive.root(),
            reached: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        *self.0.hold_create_reply.lock() = Some(state.clone());
        Held(state)
    }

    /// Make the next revision of a file on Drive and lose its answer.
    pub(crate) fn lose_reply_to_next_revision(&self) {
        self.0.lose_revision_reply.store(true, Ordering::SeqCst);
    }

    /// Make the next revision of a file on Drive and answer it, then lose the
    /// answer to the next read of that file alone, as the read-back after an
    /// upload meets on a link that drops it.
    pub(crate) fn lose_read_back_of_next_revision(&self) {
        self.0.lose_revision_read_back.store(true, Ordering::SeqCst);
    }

    /// Answer the next `reads` reads of file `uid` alone that find it as Drive
    /// lists a file whose create it has not committed yet: no active revision,
    /// no size.
    pub(crate) fn answer_reads_unrevised(&self, uid: &NodeUid, reads: usize) {
        *self.0.unrevised_reads.lock() = Some((uid.clone(), reads));
    }

    /// Answer the next listing of `folder`'s nodes with what Drive holds now,
    /// `unrevised` in it as Drive lists a file whose create it has not
    /// committed yet, then hold the answer until it is dropped.
    pub(crate) fn hold_answer_to_next_listing(
        &self,
        folder: &NodeUid,
        unrevised: &NodeUid,
    ) -> Held {
        let state = Arc::new(HoldState {
            uid: folder.clone(),
            reached: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        *self.0.hold_listing.lock() = Some((state.clone(), unrevised.clone()));
        Held(state)
    }

    /// Wait here if `uids` is the read a [`Held`] in `slot` is for.
    async fn wait_if_held(&self, slot: &Mutex<Option<Arc<HoldState>>>, uids: &[NodeUid]) {
        let state = {
            let mut hold = slot.lock();
            match hold.as_ref() {
                Some(state) if uids == std::slice::from_ref(&state.uid) => hold.take(),
                _ => None,
            }
        };
        let Some(state) = state else { return };
        wait_until_released(&state).await;
    }

    /// Requests made through this client so far.
    pub(crate) fn requests(&self) -> u64 {
        self.0.requests.load(Ordering::SeqCst)
    }

    /// Run one request against the server: wait out the latency, apply the
    /// connection's faults, and call `apply` if the request reaches Drive.
    async fn request<T>(
        &self,
        change: bool,
        apply: impl FnOnce(&mut Server) -> Result<T>,
    ) -> Result<T> {
        self.0.requests.fetch_add(1, Ordering::SeqCst);
        let faults = self.0.faults.lock().clone();
        let (latency, stall, fail, drop_reply) = {
            let mut rng = self.0.rng.lock();
            (
                Duration::from_millis(rng.between(faults.latency_ms.0, faults.latency_ms.1)),
                rng.chance(faults.stall),
                rng.chance(faults.fail),
                change && rng.chance(faults.drop_reply),
            )
        };
        if !self.0.online.load(Ordering::SeqCst) {
            tokio::time::sleep(latency.min(Duration::from_millis(20))).await;
            return Err(no_answer());
        }
        tokio::time::sleep(latency / 2).await;
        if stall {
            tokio::time::sleep(faults.stall_for).await;
            return Err(no_answer());
        }
        if fail || !self.0.online.load(Ordering::SeqCst) {
            return Err(no_answer());
        }
        let outcome = apply(&mut self.0.drive.server.lock());
        tokio::time::sleep(latency - latency / 2).await;
        if drop_reply || !self.0.online.load(Ordering::SeqCst) {
            return Err(no_answer());
        }
        outcome
    }

    fn lag(&self) -> Lag {
        let faults = self.0.faults.lock();
        Lag {
            appear: faults.listing_lag,
            update: faults.update_lag,
        }
    }

    /// The `(parent, name)` a rename or move of `uid` sends as its original
    /// hash: the entity cache's copy, or Drive's as a read sees it.
    fn original(&self, server: &Server, uid: &NodeUid) -> Option<(Option<NodeUid>, String)> {
        if self.0.faults.lock().stale_name_cache {
            if let Some(node) = self.0.entity_cache.lock().get(uid) {
                return Some((node.parent_uid.clone(), node.name.clone()));
            }
            let remote = server.visible(uid, self.lag(), Instant::now())?;
            self.0
                .entity_cache
                .lock()
                .insert(uid.clone(), to_node(uid, &remote, false));
            return Some((remote.parent, remote.name));
        }
        server
            .current(uid)
            .map(|r| (r.parent.clone(), r.name.clone()))
    }

    fn cache(&self, nodes: &[Node]) {
        let mut cache = self.0.entity_cache.lock();
        for node in nodes {
            cache.insert(node.uid.clone(), node.clone());
        }
    }

    fn rename_or_move(
        &self,
        server: &mut Server,
        uid: &NodeUid,
        new_parent: Option<&NodeUid>,
        new_name: Option<&str>,
    ) -> Result<()> {
        let original = self.original(server, uid);
        let Some(mut remote) = server.current(uid).cloned() else {
            return Err(api(ResponseCode::DoesNotExist, "node not found"));
        };
        if original != Some((remote.parent.clone(), remote.name.clone())) {
            return Err(api(
                ResponseCode::InvalidRequirements,
                "original hash is out of date",
            ));
        }
        let parent = new_parent.cloned().or(remote.parent.clone());
        let name = new_name.unwrap_or(&remote.name).to_owned();
        if let Some(parent) = &parent {
            if !server.is_folder(parent) {
                return Err(api(
                    ResponseCode::DoesNotExist,
                    "destination folder not found",
                ));
            }
            if server.is_below(parent, uid) {
                return Err(api(
                    ResponseCode::InvalidRequirements,
                    "cannot move a folder into itself",
                ));
            }
            if server
                .live_child_named(parent, &name)
                .is_some_and(|other| &other != uid)
            {
                return Err(api(
                    ResponseCode::AlreadyExists,
                    "a file or folder with that name already exists",
                ));
            }
        }
        remote.parent = parent;
        remote.name = name;
        remote.modified = now_secs();
        server.put(uid, Some(remote));
        Ok(())
    }

    fn batch<'a>(
        &'a self,
        uids: &[NodeUid],
        each: fn(&mut Server, &NodeUid) -> Result<()>,
    ) -> futures::future::BoxFuture<'a, Result<NodeOutcomes>> {
        let uids = uids.to_vec();
        async move {
            self.request(true, |server| {
                Ok(uids
                    .iter()
                    .map(|uid| (uid.clone(), each(server, uid)))
                    .collect())
            })
            .await
        }
        .boxed()
    }
}

fn outcome_stream(
    outcomes: futures::future::BoxFuture<'_, Result<NodeOutcomes>>,
) -> OutcomeStream<'_> {
    outcomes
        .map(|outcomes| match outcomes {
            Ok(outcomes) => futures::stream::iter(outcomes.into_iter().map(Ok).collect::<Vec<_>>()),
            Err(error) => futures::stream::iter(vec![Err(error)]),
        })
        .flatten_stream()
        .boxed()
}

fn trash_one(server: &mut Server, uid: &NodeUid) -> Result<()> {
    let Some(mut remote) = server.current(uid).cloned() else {
        return Err(api(ResponseCode::DoesNotExist, "node not found"));
    };
    if !remote.trashed {
        remote.trashed = true;
        server.put(uid, Some(remote));
    }
    Ok(())
}

fn restore_one(server: &mut Server, uid: &NodeUid) -> Result<()> {
    let Some(mut remote) = server.current(uid).cloned() else {
        return Err(api(ResponseCode::DoesNotExist, "node not found"));
    };
    if !remote.trashed {
        return Ok(());
    }
    if let Some(parent) = &remote.parent
        && server.live_child_named(parent, &remote.name).is_some()
    {
        return Err(api(
            ResponseCode::AlreadyExists,
            "a file or folder with that name already exists",
        ));
    }
    remote.trashed = false;
    server.put(uid, Some(remote));
    Ok(())
}

fn delete_one(server: &mut Server, uid: &NodeUid) -> Result<()> {
    if server.current(uid).is_none() {
        return Err(api(ResponseCode::DoesNotExist, "node not found"));
    }
    server.put(uid, None);
    Ok(())
}

#[async_trait]
impl DriveApi for FakeClient {
    async fn get_my_files_folder(&self) -> Result<Node> {
        self.request(false, |server| {
            let root = server.root.clone();
            let remote = server.current(&root).expect("the root always exists");
            Ok(to_node(&root, remote, false))
        })
        .await
    }

    async fn get_node(&self, uid: &NodeUid) -> Result<Option<Node>> {
        if let Some(node) = self.0.entity_cache.lock().get(uid) {
            return Ok(Some(node.clone()));
        }
        let lag = self.lag();
        let node = self
            .request(false, |server| {
                Ok(server
                    .visible(uid, lag, Instant::now())
                    .map(|remote| to_node(uid, &remote, false)))
            })
            .await?;
        self.cache(node.as_slice());
        Ok(node)
    }

    async fn enumerate_nodes(&self, uids: &[NodeUid]) -> Result<Vec<Node>> {
        self.wait_if_held(&self.0.hold, uids).await;
        {
            let mut lose = self.0.lose_read.lock();
            if lose
                .as_ref()
                .is_some_and(|uid| uids == std::slice::from_ref(uid))
            {
                *lose = None;
                return Err(no_answer());
            }
        }
        let lag = self.lag();
        let nodes: Vec<Node> = self
            .request(false, |server| {
                let now = Instant::now();
                Ok(uids
                    .iter()
                    .filter_map(|uid| Some(to_node(uid, &server.visible(uid, lag, now)?, false)))
                    .collect())
            })
            .await?;
        self.wait_if_held(&self.0.hold_answer, uids).await;
        let mut nodes = nodes;
        {
            let mut unrevised = self.0.unrevised_reads.lock();
            if let Some((uid, left)) = unrevised.as_mut()
                && uids == std::slice::from_ref(uid)
                && !nodes.is_empty()
            {
                *left -= 1;
                if *left == 0 {
                    *unrevised = None;
                }
                nodes.iter_mut().for_each(unrevise);
            }
        }
        self.cache(&nodes);
        Ok(nodes)
    }

    async fn enumerate_nodes_light(&self, uids: &[NodeUid]) -> Result<Vec<Node>> {
        let lag = self.lag();
        let mut nodes: Vec<Node> = self
            .request(false, |server| {
                let now = Instant::now();
                Ok(uids
                    .iter()
                    .filter_map(|uid| Some(to_node(uid, &server.visible(uid, lag, now)?, true)))
                    .collect())
            })
            .await?;
        let held = self.0.hold_listing.lock().take_if(|(state, _)| {
            nodes
                .iter()
                .any(|node| node.parent_uid.as_ref() == Some(&state.uid))
        });
        if let Some((state, unrevised)) = held {
            nodes
                .iter_mut()
                .filter(|node| node.uid == unrevised)
                .for_each(unrevise);
            wait_until_released(&state).await;
        }
        Ok(nodes)
    }

    async fn enumerate_folder_children_node_uids(
        &self,
        folder_uid: &NodeUid,
    ) -> Result<Vec<NodeUid>> {
        let lag = self.lag();
        self.request(false, |server| {
            let now = Instant::now();
            if server.visible(folder_uid, lag, now).is_none() {
                return Err(api(ResponseCode::DoesNotExist, "folder not found"));
            }
            let mut children: Vec<NodeUid> = server
                .nodes
                .keys()
                .filter(|uid| {
                    server.visible(uid, lag, now).is_some_and(|remote| {
                        remote.parent.as_ref() == Some(folder_uid) && !remote.trashed
                    })
                })
                .cloned()
                .collect();
            children.sort_by(|a, b| a.link_id.cmp(&b.link_id));
            Ok(children)
        })
        .await
    }

    async fn enumerate_events(
        &self,
        _scope: &DriveEventScopeId,
        cursor: Option<&DriveEventId>,
    ) -> Result<Vec<DriveEvent>> {
        let faults = self.0.faults.lock().clone();
        let mut events = self
            .request(false, |server| {
                let Some(cursor) = cursor else {
                    let head = DriveEventId::from(format!("ev{}", server.events.len()));
                    return Ok(vec![DriveEvent::CursorAdvanced { id: head }]);
                };
                let Some(after) = cursor
                    .as_str()
                    .strip_prefix("ev")
                    .and_then(|seq| seq.parse::<u64>().ok())
                else {
                    return Ok(vec![DriveEvent::ContinuityLost { id: cursor.clone() }]);
                };
                let now = Instant::now();
                Ok(server
                    .events
                    .iter()
                    .filter(|logged| logged.seq > after)
                    .take_while(|logged| logged.at + faults.event_delay <= now)
                    .map(|logged| logged.event.clone())
                    .collect())
            })
            .await?;
        let mut rng = self.0.rng.lock();
        let mut i = 0;
        while i < events.len() {
            if rng.chance(faults.duplicate_events) {
                events.insert(i + 1, events[i].clone());
                i += 1;
            }
            i += 1;
        }
        for i in 1..events.len() {
            if rng.chance(faults.reorder_events) {
                events.swap(i - 1, i);
            }
        }
        Ok(events)
    }

    async fn invalidate_caches_for_event(&self, event: &DriveEvent) -> Result<()> {
        let mut cache = self.0.entity_cache.lock();
        match event {
            DriveEvent::NodeUpdated { node_uid, .. } | DriveEvent::NodeDeleted { node_uid, .. } => {
                cache.remove(node_uid);
            }
            DriveEvent::ContinuityLost { .. } | DriveEvent::ScopeAccessLost { .. } => cache.clear(),
            DriveEvent::CursorAdvanced { .. } | DriveEvent::SharedWithMeUpdated { .. } => {}
        }
        Ok(())
    }

    async fn create_folder(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        last_modification_time: Option<i64>,
    ) -> Result<NodeUid> {
        self.request(true, |server| {
            if !server.is_folder(parent_uid) {
                return Err(api(ResponseCode::DoesNotExist, "parent folder not found"));
            }
            if server.live_child_named(parent_uid, name).is_some() {
                return Err(api(
                    ResponseCode::AlreadyExists,
                    "a file or folder with that name already exists",
                ));
            }
            let uid = server.mint_uid();
            let now = now_secs();
            server.put(
                &uid,
                Some(Remote {
                    parent: Some(parent_uid.clone()),
                    name: name.to_owned(),
                    trashed: false,
                    created: now,
                    modified: last_modification_time.unwrap_or(now),
                    file: None,
                }),
            );
            Ok(uid)
        })
        .await
    }

    async fn upload_file(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        contents: &[u8],
    ) -> Result<NodeUid> {
        self.request(true, |server| {
            server.new_file(parent_uid, name, media_type, contents.to_vec(), None)
        })
        .await
    }

    async fn upload_file_from_dyn(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        reader: &mut (dyn Read + Send),
        _intended_size: i64,
        _thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
        _aead: bool,
    ) -> Result<NodeUid> {
        let content = read_all(reader)?;
        let made = self
            .request(true, |server| {
                server.new_file(
                    parent_uid,
                    name,
                    media_type,
                    content,
                    last_modification_time,
                )
            })
            .await;
        if made.is_ok() && self.0.drop_after_create.swap(false, Ordering::SeqCst) {
            self.set_online(false);
        }
        let held = self.0.hold_create_reply.lock().take();
        if made.is_ok()
            && let Some(state) = held
        {
            wait_until_released(&state).await;
        }
        if made.is_ok() && self.0.lose_create_reply.swap(false, Ordering::SeqCst) {
            return Err(no_answer());
        }
        made
    }

    async fn upload_file_replacing_draft_from_dyn(
        &self,
        parent_uid: &NodeUid,
        name: &str,
        media_type: &str,
        reader: &mut (dyn Read + Send),
        intended_size: i64,
        thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
        aead: bool,
    ) -> Result<NodeUid> {
        // The fake Drive keeps no drafts, so there is never one to replace.
        self.upload_file_from_dyn(
            parent_uid,
            name,
            media_type,
            reader,
            intended_size,
            thumbnails,
            last_modification_time,
            aead,
        )
        .await
    }

    async fn upload_new_revision_from_dyn(
        &self,
        file_uid: &NodeUid,
        reader: &mut (dyn Read + Send),
        _intended_size: i64,
        _thumbnails: Vec<Thumbnail>,
        last_modification_time: Option<i64>,
    ) -> Result<()> {
        let content = Arc::new(read_all(reader)?);
        self.request(true, |server| {
            let Some(mut remote) = server.current(file_uid).cloned() else {
                return Err(api(ResponseCode::DoesNotExist, "file not found"));
            };
            if remote.file.is_none() {
                return Err(api(ResponseCode::InvalidRequirements, "not a file"));
            }
            server.make_room(content.len())?;
            let revision = server.mint_revision();
            let blocks = server.blocks_of(content.len() as u64);
            let file = remote.file.as_mut().expect("checked above");
            file.revisions.push((revision.clone(), content.clone()));
            file.revision = revision;
            file.content = content;
            file.block_sizes = blocks;
            remote.modified = last_modification_time.unwrap_or_else(now_secs);
            server.put(file_uid, Some(remote));
            Ok(())
        })
        .await?;
        if self.0.lose_revision_reply.swap(false, Ordering::SeqCst) {
            return Err(no_answer());
        }
        if self.0.lose_revision_read_back.swap(false, Ordering::SeqCst) {
            *self.0.lose_read.lock() = Some(file_uid.clone());
        }
        Ok(())
    }

    async fn open_revision(&self, uid: &NodeUid) -> Result<Arc<dyn RevisionRead>> {
        let file = self
            .request(false, |server| {
                server
                    .current(uid)
                    .and_then(|remote| remote.file.clone())
                    .ok_or_else(|| api(ResponseCode::DoesNotExist, "file not found"))
            })
            .await?;
        Ok(Arc::new(FakeRevision {
            client: self.clone(),
            content: file.content,
            block_sizes: file.block_sizes,
        }))
    }

    async fn download_file_to_dyn(
        &self,
        uid: &NodeUid,
        output: &mut (dyn Write + Send),
    ) -> Result<()> {
        let content = self
            .request(false, |server| {
                server
                    .current(uid)
                    .and_then(|remote| remote.file.as_ref().map(|file| file.content.clone()))
                    .ok_or_else(|| api(ResponseCode::DoesNotExist, "file not found"))
            })
            .await?;
        output.write_all(&content).map_err(io_error)
    }

    async fn download_revision_to_dyn(
        &self,
        file_uid: &NodeUid,
        revision_id: &str,
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        let content = self
            .request(false, |server| {
                server
                    .current(file_uid)
                    .and_then(|remote| remote.file.as_ref())
                    .and_then(|file| file.revisions.iter().find(|(id, _)| id == revision_id))
                    .map(|(_, content)| content.clone())
                    .ok_or_else(|| api(ResponseCode::DoesNotExist, "revision not found"))
            })
            .await?;
        writer.write_all(&content).map_err(io_error)
    }

    async fn rename_node(
        &self,
        uid: &NodeUid,
        new_name: &str,
        _new_media_type: Option<&str>,
    ) -> Result<()> {
        self.request(true, |server| {
            self.rename_or_move(server, uid, None, Some(new_name))
        })
        .await
    }

    async fn move_node(&self, uid: &NodeUid, new_parent: &NodeUid) -> Result<()> {
        self.request(true, |server| {
            self.rename_or_move(server, uid, Some(new_parent), None)
        })
        .await
    }

    fn move_nodes_streaming<'a>(
        &'a self,
        items: Vec<NodeMoveItem>,
        new_parent: NodeUid,
    ) -> OutcomeStream<'a> {
        outcome_stream(
            async move {
                self.request(true, |server| {
                    Ok(items
                        .iter()
                        .map(|item| {
                            let outcome = self.rename_or_move(
                                server,
                                &item.uid,
                                Some(&new_parent),
                                item.target_name.as_deref(),
                            );
                            (item.uid.clone(), outcome)
                        })
                        .collect())
                })
                .await
            }
            .boxed(),
        )
    }

    async fn trash_nodes(&self, uids: &[NodeUid]) -> Result<NodeOutcomes> {
        self.wait_if_held(&self.0.hold_trash, uids).await;
        self.batch(uids, trash_one).await
    }

    async fn restore_nodes(&self, uids: &[NodeUid]) -> Result<NodeOutcomes> {
        self.batch(uids, restore_one).await
    }

    fn restore_nodes_streaming<'a>(&'a self, uids: &[NodeUid]) -> OutcomeStream<'a> {
        outcome_stream(self.batch(uids, restore_one))
    }

    fn delete_nodes_streaming<'a>(&'a self, uids: &[NodeUid]) -> OutcomeStream<'a> {
        outcome_stream(self.batch(uids, delete_one))
    }

    async fn quota(&self) -> Result<Quota> {
        self.request(false, |server| {
            Ok(Quota {
                max_space: server.max_space,
                used_space: server.used_space(),
            })
        })
        .await
    }

    async fn enumerate_trash_node_uids(&self) -> Result<Vec<NodeUid>> {
        self.request(false, |server| {
            Ok(server
                .nodes
                .iter()
                .filter(|(_, versions)| {
                    versions
                        .last()
                        .and_then(|(_, remote)| remote.as_ref())
                        .is_some_and(|remote| remote.trashed)
                })
                .map(|(uid, _)| uid.clone())
                .collect())
        })
        .await
    }
}

/// An open revision of a fake file. Its block reads are requests like any
/// other, so a link that drops under a read fails it.
struct FakeRevision {
    client: FakeClient,
    content: Arc<Vec<u8>>,
    block_sizes: Vec<u64>,
}

#[async_trait]
impl RevisionRead for FakeRevision {
    fn size(&self) -> u64 {
        self.content.len() as u64
    }

    fn block_sizes(&self) -> &[u64] {
        &self.block_sizes
    }

    async fn read_at(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        let content = self.content.clone();
        self.client
            .request(false, move |_| {
                let start = (offset as usize).min(content.len());
                let end = (offset.saturating_add(length) as usize).min(content.len());
                Ok(content[start..end].to_vec())
            })
            .await
    }
}

/// A uid on the fake volume.
pub(crate) fn uid(link: &str) -> NodeUid {
    NodeUid::new(VolumeId::from(VOLUME), LinkId::from(link))
}

fn to_node(uid: &NodeUid, remote: &Remote, light: bool) -> Node {
    let kind = match &remote.file {
        None => NodeKind::Folder,
        Some(file) => {
            let size = file.content.len() as i64;
            NodeKind::File {
                media_type: file.media_type.clone(),
                // Ciphertext is larger than the plaintext: a packet header per
                // block, which is what B12's provisional size was off by.
                total_size_on_storage: size + 512 * file.block_sizes.len().max(1) as i64,
                active_revision_state: Some(RevisionState::Active),
                active_revision_id: Some(file.revision.clone()),
                claimed_size: (!light).then_some(size),
                claimed_modification_time: None,
                content_sha1: (!light).then(|| {
                    Sha1::digest(file.content.as_slice())
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect()
                }),
            }
        }
    };
    Node {
        uid: uid.clone(),
        parent_uid: remote.parent.clone(),
        kind,
        name: remote.name.clone(),
        creation_time: remote.created,
        modification_time: remote.modified,
        trashed: remote.trashed,
        is_shared: false,
        is_shared_publicly: false,
        signature_email: None,
        membership: None,
        direct_role: None,
        share_id: None,
        photo: None,
        album: None,
        verification: Default::default(),
    }
}

fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

fn read_all(reader: &mut (dyn Read + Send)) -> Result<Vec<u8>> {
    let mut content = Vec::new();
    reader.read_to_end(&mut content).map_err(io_error)?;
    Ok(content)
}

fn io_error(error: std::io::Error) -> ProtonError {
    ProtonError::invalid_operation(format!("local i/o: {error}"))
}

fn api(code: ResponseCode, message: &str) -> ProtonError {
    tracing::debug!(?code, message, "fake drive refused a request");
    let http_status = match code {
        ResponseCode::DoesNotExist => 404,
        ResponseCode::AlreadyExists | ResponseCode::InvalidRequirements => 422,
        _ => 400,
    };
    ProtonError::Api(ProtonApiError {
        code,
        http_status,
        message: format!("fake drive: {message}"),
        details: None,
    })
}

/// What a request that never got an answer reports; the daemon reads it as
/// the network (see `link::is_network_error`).
fn no_answer() -> ProtonError {
    ProtonError::Api(ProtonApiError {
        code: ResponseCode::RequestTimeout,
        http_status: 408,
        message: "fake drive: no answer".into(),
        details: None,
    })
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::is_network_error;

    fn code(error: &ProtonError) -> Option<ResponseCode> {
        match error {
            ProtonError::Api(api) => Some(api.code),
            _ => None,
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn a_tree_built_through_the_api_reads_back() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            let root = client.get_my_files_folder().await.unwrap();
            let docs = client.create_folder(&root.uid, "docs", None).await.unwrap();
            let file = client
                .upload_file(&docs, "a.txt", "text/plain", b"hello")
                .await
                .unwrap();
            let children = client
                .enumerate_folder_children_node_uids(&docs)
                .await
                .unwrap();
            assert_eq!(children, vec![file.clone()]);
            let node = client
                .enumerate_nodes(std::slice::from_ref(&file))
                .await
                .unwrap()
                .remove(0);
            assert!(matches!(
                node.kind,
                NodeKind::File {
                    claimed_size: Some(5),
                    ..
                }
            ));
            let light = client
                .enumerate_nodes_light(std::slice::from_ref(&file))
                .await
                .unwrap()
                .remove(0);
            assert!(matches!(
                light.kind,
                NodeKind::File {
                    claimed_size: None,
                    ..
                }
            ));
            let mut out = Vec::new();
            (&client as &dyn DriveApi)
                .download_file_to(&file, &mut out)
                .await
                .unwrap();
            assert_eq!(out, b"hello");
            assert_eq!(
                drive.tree(),
                BTreeMap::from([
                    ("docs".to_owned(), Entry::Folder),
                    (
                        "docs/a.txt".to_owned(),
                        Entry::File(Arc::new(b"hello".to_vec()))
                    ),
                ])
            );
        });
    }

    #[test]
    fn a_taken_name_refuses_a_create_and_a_rename() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            let root = drive.root();
            client.upload_file(&root, "a", "", b"1").await.unwrap();
            let b = client.upload_file(&root, "b", "", b"2").await.unwrap();
            let taken = client.upload_file(&root, "a", "", b"3").await.unwrap_err();
            assert_eq!(code(&taken), Some(ResponseCode::AlreadyExists));
            let taken = client.rename_node(&b, "a", None).await.unwrap_err();
            assert_eq!(code(&taken), Some(ResponseCode::AlreadyExists));
        });
    }

    #[test]
    fn a_second_rename_sends_the_cached_name_until_an_event_evicts_it() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            let root = drive.root();
            let file = client.upload_file(&root, "a", "", b"1").await.unwrap();
            client.rename_node(&file, "b", None).await.unwrap();
            let stale = client.rename_node(&file, "c", None).await.unwrap_err();
            assert_eq!(code(&stale), Some(ResponseCode::InvalidRequirements));
            let changed = DriveEvent::NodeUpdated {
                id: "x".into(),
                node_uid: file.clone(),
                parent_node_uid: None,
                is_trashed: false,
                is_shared: false,
            };
            client.invalidate_caches_for_event(&changed).await.unwrap();
            client.rename_node(&file, "c", None).await.unwrap();
            assert!(drive.lookup("c").is_some());
        });
    }

    #[test]
    fn a_lagging_listing_does_not_show_a_fresh_create() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(
                1,
                Faults {
                    listing_lag: Duration::from_millis(200),
                    ..Faults::none()
                },
            );
            let root = drive.root();
            let file = client.upload_file(&root, "a", "", b"1").await.unwrap();
            assert!(
                client
                    .enumerate_folder_children_node_uids(&root)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                client
                    .enumerate_nodes(std::slice::from_ref(&file))
                    .await
                    .unwrap()
                    .is_empty()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(
                client
                    .enumerate_folder_children_node_uids(&root)
                    .await
                    .unwrap(),
                vec![file]
            );
        });
    }

    #[test]
    fn a_held_read_answers_with_what_drive_holds_when_released() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            let root = drive.root();
            let file = client.upload_file(&root, "a", "", b"1").await.unwrap();
            let held = client.hold_next_read(&file);
            let read = tokio::spawn({
                let client = client.clone();
                let file = file.clone();
                async move { client.enumerate_nodes(std::slice::from_ref(&file)).await }
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(held.reached());
            assert!(!read.is_finished());
            drive.device().trash("a");
            drop(held);
            let nodes = read.await.unwrap().unwrap();
            assert!(nodes[0].trashed);
            // Only the next read is held.
            assert_eq!(client.enumerate_nodes(&[file]).await.unwrap().len(), 1);
        });
    }

    #[test]
    fn a_held_answer_is_what_drive_held_when_read() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            let root = drive.root();
            let file = client.upload_file(&root, "a", "", b"1").await.unwrap();
            let held = client.hold_answer_to_next_read(&file);
            let read = tokio::spawn({
                let client = client.clone();
                let file = file.clone();
                async move { client.enumerate_nodes(std::slice::from_ref(&file)).await }
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(held.reached());
            assert!(!read.is_finished());
            drive.device().trash("a");
            drop(held);
            let nodes = read.await.unwrap().unwrap();
            assert!(!nodes[0].trashed);
        });
    }

    #[test]
    fn a_rename_lags_only_by_the_update_lag() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            let file = drive.device().write("a", b"1");
            client.rename_node(&file, "b", None).await.unwrap();
            let names = || async {
                let nodes = client
                    .enumerate_nodes_light(std::slice::from_ref(&file))
                    .await
                    .unwrap();
                nodes.into_iter().map(|n| n.name).collect::<Vec<_>>()
            };
            let lagging = |listing_lag, update_lag| Faults {
                listing_lag,
                update_lag,
                ..Faults::none()
            };
            let minute = Duration::from_secs(60);
            client.set_faults(lagging(minute, Duration::ZERO));
            assert!(names().await.is_empty());
            client.set_faults(lagging(Duration::ZERO, Duration::ZERO));
            assert_eq!(names().await, ["b"]);
            client.set_faults(lagging(Duration::ZERO, minute));
            assert_eq!(names().await, ["a"]);
        });
    }

    #[test]
    fn every_client_sees_every_change_on_the_feed() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let (one, two) = (
                drive.client(1, Faults::none()),
                drive.client(2, Faults::none()),
            );
            let scope = DriveEventScopeId::new(VolumeId::from(VOLUME));
            let seed = two.enumerate_events(&scope, None).await.unwrap();
            let cursor = seed[0].id().clone();
            let file = one.upload_file(&drive.root(), "a", "", b"1").await.unwrap();
            one.trash_nodes(std::slice::from_ref(&file)).await.unwrap();
            let events = two.enumerate_events(&scope, Some(&cursor)).await.unwrap();
            assert!(matches!(
                &events[..],
                [
                    DriveEvent::NodeUpdated {
                        is_trashed: false,
                        ..
                    },
                    DriveEvent::NodeUpdated {
                        is_trashed: true,
                        ..
                    }
                ]
            ));
            let own = one.enumerate_events(&scope, Some(&cursor)).await.unwrap();
            assert_eq!(own.len(), 2, "our own changes are echoed back");
        });
    }

    #[test]
    fn a_lost_reply_still_changed_drive() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(
                1,
                Faults {
                    drop_reply: 1.0,
                    ..Faults::none()
                },
            );
            let error = client
                .create_folder(&drive.root(), "half", None)
                .await
                .unwrap_err();
            assert!(is_network_error(&error));
            assert!(drive.lookup("half").is_some());
        });
    }

    #[test]
    fn an_offline_client_reaches_nothing() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            client.set_online(false);
            let error = client
                .create_folder(&drive.root(), "x", None)
                .await
                .unwrap_err();
            assert!(is_network_error(&error));
            assert!(drive.lookup("x").is_none());
        });
    }

    #[test]
    fn an_exhausted_quota_refuses_uploads_until_space_is_freed() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            let root = drive.root();
            let big = client.upload_file(&root, "big", "", &[0; 8]).await.unwrap();
            drive.set_quota(10);
            let full = client
                .upload_file(&root, "more", "", &[0; 4])
                .await
                .unwrap_err();
            assert_eq!(code(&full), Some(ResponseCode::InsufficientQuota));
            assert_eq!(client.quota().await.unwrap().used_space, 8);
            let mut deleted = client.delete_nodes_streaming(std::slice::from_ref(&big));
            assert!(deleted.next().await.unwrap().unwrap().1.is_ok());
            drop(deleted);
            client
                .upload_file(&root, "more", "", &[0; 4])
                .await
                .unwrap();
        });
    }

    #[test]
    fn content_is_split_by_the_block_pattern() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            drive.set_block_pattern(vec![3, 5]);
            let client = drive.client(1, Faults::none());
            let file = client
                .upload_file(&drive.root(), "f", "", b"0123456789abc")
                .await
                .unwrap();
            let reader = client.open_revision(&file).await.unwrap();
            assert_eq!(reader.block_sizes(), &[3, 5, 5]);
            assert_eq!(reader.read_at(3, 5).await.unwrap(), b"34567");
        });
    }

    #[test]
    fn a_rename_by_another_device_makes_our_cached_name_stale() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::none());
            let file = client
                .upload_file(&drive.root(), "a", "", b"1")
                .await
                .unwrap();
            client.get_node(&file).await.unwrap();
            drive.device().rename("a", "b");
            let stale = client.rename_node(&file, "c", None).await.unwrap_err();
            assert_eq!(code(&stale), Some(ResponseCode::InvalidRequirements));
        });
    }

    #[test]
    fn faults_set_later_apply_to_the_next_request() {
        rt().block_on(async {
            let drive = FakeDrive::new();
            let client = drive.client(1, Faults::lan());
            client.get_my_files_folder().await.unwrap();
            client.set_faults(Faults {
                fail: 1.0,
                ..Faults::wifi()
            });
            let error = client.get_my_files_folder().await.unwrap_err();
            assert!(is_network_error(&error));
            assert_eq!(client.requests(), 2);
        });
    }

    #[test]
    fn a_device_change_lands_on_the_server_and_the_feed() {
        let drive = FakeDrive::new();
        drive.device().write("notes", b"v1");
        drive.device().write("notes", b"v2");
        assert_eq!(drive.tree()["notes"], Entry::File(Arc::new(b"v2".to_vec())));
        drive.device().trash("notes");
        assert!(drive.tree().is_empty());
        assert_eq!(drive.event_count(), 3);
    }
}
