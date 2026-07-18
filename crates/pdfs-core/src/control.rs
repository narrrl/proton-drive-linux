//! IPC protocol between the CLI and a running mount daemon.
//!
//! The daemon listens on a Unix domain socket under the state dir; the CLI
//! (`pdfs pin` / `unpin` / `status`) connects, sends one [`Request`] as a single
//! JSON line, and reads one JSON-line [`Response`]. Keeping the wire format
//! line-delimited JSON makes the socket trivially scriptable.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cache::Pin;
use crate::error::Result;

/// Cap on the *write* half of a round-trip. A crashed daemon can leave its
/// listening socket in the kernel (e.g. as a `<defunct>` zombie): `connect`
/// then succeeds but no one ever replies. A short write bound trips fast when
/// the daemon is wedged before it has read anything.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on the *read* half. Some requests make the daemon do real work before it
/// replies — `PhotosTimeline` downloads a page of thumbnails, `OpenFile`
/// downloads whole-file content — which easily takes more than a couple of
/// seconds. A 2s read bound timed those out and the GUI mistook the timeout for
/// "no daemon" (showing "Mount Proton Drive…" on a live mount). Give reads a
/// generous bound that still protects against a daemon that accepts but never
/// answers.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// A command sent from the CLI to the daemon.
#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    /// Report mount status (account, mountpoint, pin count).
    Status,
    /// Pin a file (path relative to the mountpoint, or absolute under it).
    Pin { path: String },
    /// Unpin a file, evicting its cached content.
    Unpin { path: String },
    /// List pinned files.
    ListPins,
    /// List a directory for the in-app file browser. `path` is
    /// mountpoint-relative (empty or "." = the mount root).
    ListDir { path: String },
    /// Fetch a page of the photos timeline, newest first. Returns metadata only:
    /// a thumbnail path comes back only for photos already in the cache, so the
    /// reply never waits on the network. Front-ends ask for the thumbnails they
    /// actually display with [`Request::PhotoThumbs`].
    PhotosTimeline {
        offset: usize,
        limit: usize,
        /// Restrict the page to one kind (Photos / Videos / Raw). `None` (the
        /// default, and what older front-ends send) returns everything. The
        /// offset is relative to the filtered timeline, so paging a single tab
        /// doesn't have to walk past the other kinds.
        #[serde(default)]
        kind: Option<PhotoKind>,
        /// Restrict the page to a `[from, to)` capture-time window (epoch
        /// seconds) — the date scrubber's jump to a month. `None` spans the whole
        /// timeline. Like `kind`, the offset is relative to the filtered set.
        #[serde(default)]
        range: Option<(i64, i64)>,
    },
    /// The months the timeline spans (newest first, with per-month counts) so a
    /// front-end can build a date scrubber without paging the whole library.
    /// `kind` scopes the counts to one tab when set.
    PhotoMonths {
        #[serde(default)]
        kind: Option<PhotoKind>,
    },
    /// Fetch thumbnails for the given photo uids, downloading the ones not
    /// already cached (one batched round-trip) and replying with their on-disk
    /// paths. Keep the batch small — it is served on demand, as tiles scroll in.
    PhotoThumbs { uids: Vec<String> },
    /// Download a photo's full content into the cache; replies with its path.
    OpenPhoto { uid: String },
    /// Upload a photo with the given name, media type, and content bytes.
    UploadPhoto {
        name: String,
        media_type: String,
        bytes: Vec<u8>,
        capture_time: Option<i64>,
    },
    /// Download a Drive file's full content into the cache; replies with the
    /// on-disk path so the front-end can open it with the default app. `path`
    /// is mountpoint-relative.
    OpenFile { path: String },
    /// Full-text search node names against the daemon's local metadata index.
    /// `limit` caps the number of hits returned. Replies with [`Response::SearchResults`].
    Search { query: String, limit: usize },
    /// Search the daemon's index of *local* (non-Drive) files on this machine.
    /// Independent of [`Request::Search`] so a front-end can fire both at once and
    /// render whichever lands first. Replies with [`Response::LocalResults`].
    SearchLocal { query: String, limit: usize },
    /// Rename a file or folder. `path` is mountpoint-relative; `new_name` is a
    /// single path component (no separators). Replies with [`Response::Ok`].
    Rename { path: String, new_name: String },
    /// Move a file or folder into a new parent folder. Both `path` and
    /// `new_parent` are mountpoint-relative. Replies with [`Response::Ok`].
    Move { path: String, new_parent: String },
    /// Trash a file or folder. `path` is mountpoint-relative. Replies with
    /// [`Response::Ok`].
    Delete { path: String },
    /// Create a new folder named `name` under the mountpoint-relative `parent`.
    /// Replies with [`Response::Ok`].
    CreateFolder { parent: String, name: String },
    /// Upload a file named `name` with content `bytes` into the
    /// mountpoint-relative `parent` folder. Replies with [`Response::Ok`].
    UploadFile {
        parent: String,
        name: String,
        bytes: Vec<u8>,
    },
    /// Bulk-upload local files and/or directory trees into the mountpoint-relative
    /// `parent` folder. `sources` are absolute paths on the daemon's own
    /// filesystem (the daemon is local): each file is uploaded, each directory is
    /// recreated remotely and walked recursively. The daemon acks immediately with
    /// [`Response::Ok`] and does the work in the background — a big tree far
    /// outlasts the socket read timeout — so progress is observed through
    /// [`Request::GetQueueStatus`] and completion through the transfer count
    /// falling to zero.
    UploadPaths {
        parent: String,
        sources: Vec<String>,
    },
    /// Delete all unpinned cached blobs and on-demand blocks, keeping pinned
    /// files intact. Replies with [`Response::Ok`] reporting the bytes freed.
    PurgeCache,
    /// Retune the on-disk cache's soft byte cap at runtime (`0` = unlimited) and
    /// persist it to config so the next mount keeps it. Replies with
    /// [`Response::Ok`].
    SetCacheBudget { bytes: u64 },
    /// Snapshot what the daemon is working on: in-flight transfers (active
    /// uploads/downloads) and the longer jobs around them (scans, folder
    /// skeletons, the local index, sync passes). Replies with
    /// [`Response::Transfers`]. Cheap to poll: the daemon keeps the registry in
    /// memory, so a front-end can render a live progress widget.
    GetQueueStatus,
    /// List what is in the account's trash. Replies with [`Response::Entries`];
    /// a trashed node has no path inside the mount, so each entry carries only
    /// its `uid` — the handle for [`Request::Restore`] and
    /// [`Request::DeleteForever`] — and its `path` is empty.
    ListTrash,
    /// Restore trashed nodes, by uid, to the folders they were trashed from.
    /// Replies with [`Response::Ok`].
    Restore { uids: Vec<String> },
    /// Permanently delete trashed nodes by uid. Irreversible: the content is
    /// gone from Proton Drive, not moved. Replies with [`Response::Ok`].
    DeleteForever { uids: Vec<String> },
    /// Permanently delete everything in the trash. Irreversible.
    /// Replies with [`Response::Ok`].
    EmptyTrash,

    /// Drop a cached listing so the *next* read of it re-enumerates from the
    /// server. Replies with [`Response::Ok`]. This is what a front-end's Refresh
    /// button raises: the daemon serves listings from its persisted cache, which
    /// only notices another client's changes when its TTL lapses, so a user who
    /// knows the cache is stale needs a way to say so. Cheap and idempotent —
    /// it invalidates, it does not fetch.
    Refresh { scope: RefreshScope },

    // ---- devices ----------------------------------------------------------
    /// List the account's registered devices. Replies with [`Response::Devices`].
    ListDevices,
    /// Rename a device by its uid. Replies with [`Response::Ok`].
    RenameDevice { uid: String, name: String },
    /// Delete (deregister) a device by its uid. Replies with [`Response::Ok`].
    DeleteDevice { uid: String },

    // ---- device folder sync (devices.md) ----------------------------------
    /// Add a local folder to this machine's device, uploading its tree and
    /// registering the device on first use. Replies with [`Response::Ok`].
    AddSyncFolder { local_path: String },
    /// List this device's synced folders. Replies with [`Response::SyncFolders`].
    ListSyncFolders,
    /// Remove a synced folder by id; `delete_remote` also trashes its cloud copy.
    /// Replies with [`Response::Ok`].
    RemoveSyncFolder { id: i64, delete_remote: bool },
    /// Switch a synced folder between `mirror` and `ondemand` (Phase 3). Replies
    /// with [`Response::Ok`].
    SetSyncFolderMode { id: i64, mode: String },
    /// Force a reconcile pass: one folder by id, or all when `id` is `None`.
    /// Replies with [`Response::Ok`].
    SyncNow { id: Option<i64> },

    // ---- sharing a node ---------------------------------------------------
    /// Invite `emails` (Proton and/or external addresses, auto-detected) to the
    /// node at mountpoint-relative `path` at `role` ("viewer"|"editor"|"admin"),
    /// with an optional email `message`. Replies with [`Response::Ok`].
    ShareNode {
        path: String,
        emails: Vec<String>,
        role: String,
        message: Option<String>,
    },
    /// List the members, pending invitations and public link of the node at
    /// mountpoint-relative `path`. Replies with [`Response::Share`].
    ListShare { path: String },
    /// Change the role of a share entry (member or pending invitation) on the node
    /// at `path`. `id` and `kind` identify the entry (from [`Response::Share`]).
    /// Replies with [`Response::Ok`].
    UpdateShareRole {
        path: String,
        id: String,
        kind: ShareEntryKind,
        role: String,
    },
    /// Remove a share entry (member, pending Proton invite, or external invite)
    /// from the node at `path`. Replies with [`Response::Ok`].
    RemoveShareEntry {
        path: String,
        id: String,
        kind: ShareEntryKind,
    },
    /// Create a public link on the node at `path`. `role` is "viewer" or "editor";
    /// `password` optionally adds a custom password; `expires` is an optional Unix
    /// expiry. Replies with [`Response::PublicLink`] (carrying the shareable URL).
    CreatePublicLink {
        path: String,
        role: String,
        password: Option<String>,
        expires: Option<i64>,
    },
    /// Remove the public link `id` from the node at `path`. Replies with
    /// [`Response::Ok`].
    RemovePublicLink { path: String, id: String },

    // ---- shared by me -----------------------------------------------------
    /// List the nodes I have shared with others — collaborative shares that still
    /// have members, pending invitations or a public link. Replies with
    /// [`Response::SharedByMe`]. Each item carries the share's state so the
    /// front-end can render it without a follow-up per node.
    ListSharedByMe,

    // ---- shared with me ---------------------------------------------------
    /// List nodes shared with me that I have accepted. Replies with
    /// [`Response::Entries`] (each entry carries its `uid`; `path` is empty since
    /// the item lives outside the mount tree).
    ListSharedWithMe,
    /// Leave a shared node by its `uid`, giving up my access. Replies with
    /// [`Response::Ok`].
    LeaveShared { uid: String },

    // ---- incoming invitations ---------------------------------------------
    /// List invitations addressed to me, pending accept or reject. Replies with
    /// [`Response::Invitations`].
    ListInvitations,
    /// Accept the invitation `id`, gaining access to the shared node. Replies with
    /// [`Response::Ok`].
    AcceptInvitation { id: String },
    /// Reject the invitation `id`, declining access. Replies with [`Response::Ok`].
    RejectInvitation { id: String },

    // ---- bookmarks --------------------------------------------------------
    /// List public links saved to my account. Replies with [`Response::Bookmarks`].
    ListBookmarks,
    /// Save a public link `url` (optionally password-protected) as a bookmark.
    /// Replies with [`Response::Ok`].
    CreateBookmark {
        url: String,
        password: Option<String>,
    },
    /// Remove a saved bookmark by its `token`. Replies with [`Response::Ok`].
    DeleteBookmark { token: String },

    // ---- activity ---------------------------------------------------------
    /// Fetch the daemon's recent activity log, newest first, capped at `limit`
    /// entries. Replies with [`Response::Activity`]. The log is persisted, so it
    /// survives a daemon restart: it records the mutations and transfers the
    /// daemon performs (uploads, downloads, deletes, renames, shares, sync
    /// passes, …), so a front-end can show a running "what happened" feed
    /// without re-deriving it from anywhere.
    ListActivity { limit: usize },
}

/// Which kind of share entry a [`ShareEntry`] is, and which collection an
/// [`Request::UpdateShareRole`]/[`Request::RemoveShareEntry`] targets.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareEntryKind {
    /// An accepted member (identified by its membership id).
    Member,
    /// A pending invitation to a Proton user (identified by its invitation id).
    ProtonInvite,
    /// A pending invitation to a non-Proton email (identified by its invitation id).
    ExternalInvite,
}

/// Which cached listing a [`Request::Refresh`] drops.
///
/// Only the listings the daemon caches need naming here — the sharing, devices
/// and activity listings are always fetched live, so a front-end refreshes those
/// by simply re-asking.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum RefreshScope {
    /// One folder's child listing, by mountpoint-relative path (`""` = root).
    /// Only the folder itself, not its subtree: refreshing what the user is
    /// looking at shouldn't re-walk everything below it.
    Dir { path: String },
    /// The trash listing.
    Trash,
    /// The photos timeline.
    Photos,
}

/// A registered device in a [`Response::Devices`] listing.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeviceInfo {
    /// Device uid — the handle for [`Request::RenameDevice`]/[`Request::DeleteDevice`].
    pub uid: String,
    /// Decrypted device name, or a placeholder when it could not be decrypted.
    pub name: String,
    /// Platform: "Windows", "MacOs" or "Linux".
    pub device_type: String,
    /// Last sync time, epoch seconds; `None` if it never synced.
    pub last_sync: Option<i64>,
    /// Whether this is the device *this* machine syncs to. Deleting it would
    /// delete the cloud copy of the folders this machine is syncing, so a
    /// front-end must not offer that as casually as removing another computer.
    #[serde(default)]
    pub this_device: bool,
}

/// One synced local folder on this machine's device (in [`Response::SyncFolders`]).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SyncFolderInfo {
    /// Row id — the handle for [`Request::RemoveSyncFolder`]/[`Request::SetSyncFolderMode`].
    pub id: i64,
    /// Absolute local folder path.
    pub local_path: String,
    /// The uid of the folder's remote root under the device root.
    pub remote_uid: String,
    /// `mirror` (full local copy, two-way synced) or `ondemand` (FUSE mount).
    pub mode: String,
    /// A mode switch the user asked for that the daemon has queued: it applies
    /// once the folder's current pass has pushed any local changes up. `None`
    /// when nothing is queued. A front-end should paint the folder as already
    /// heading there — the request was accepted, not rejected.
    #[serde(default)]
    pub pending_mode: Option<String>,
    /// `idle` | `syncing` | `error` | `conflict`.
    pub state: String,
    /// Last successful sync, epoch seconds; `0` if never.
    pub last_sync: i64,
    /// What the folder's sync pass is doing right now, or `None` when no pass is
    /// running. Live daemon state, not a stored column.
    #[serde(default)]
    pub progress: Option<SyncProgress>,
}

/// Which stage a running sync pass is in, in a [`SyncProgress`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPhase {
    /// Walking the local tree, the remote tree and the stored baseline to work
    /// out what changed. `done` counts the items checked so far; `total` is how
    /// many the last pass saw, so it is an estimate the walk can overshoot.
    Scanning,
    /// Applying the diff: creating folders, uploading, downloading, deleting.
    Applying,
}

/// A snapshot of a sync pass in flight (in [`SyncFolderInfo::progress`]), so a
/// front-end can say what the daemon is doing rather than just "syncing".
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SyncProgress {
    pub phase: SyncPhase,
    /// Items checked ([`SyncPhase::Scanning`]) or applied ([`SyncPhase::Applying`])
    /// so far this pass.
    pub done: usize,
    /// How many items `done` is counting towards. Neither phase can fix this up
    /// front, so it moves: while scanning it is the size of the last pass's
    /// baseline — an estimate the walk may overshoot when the folder has grown —
    /// and while applying it *grows*, because paths are classified depth by depth
    /// (a folder must exist remotely before its children can be queued). `0` means
    /// no estimate exists (a folder that has never synced), i.e. indeterminate.
    pub total: usize,
    /// The name of an item currently being applied, or empty between items.
    /// Several run at once; this is just the most recently started.
    pub current: String,
}

/// One member or pending invitation on a node's share (in [`Response::Share`]).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShareEntry {
    /// Membership id (members) or invitation id (invitations) — the handle for
    /// [`Request::UpdateShareRole`]/[`Request::RemoveShareEntry`].
    pub id: String,
    /// The member's / invitee's email address.
    pub email: String,
    /// Role: "viewer", "editor", "admin" or "inherited".
    pub role: String,
    /// Whether this is a member, a pending Proton invite, or an external invite.
    pub kind: ShareEntryKind,
}

/// A node's public link (in [`Response::Share`] / [`Response::PublicLink`]).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PublicLinkInfo {
    /// Public-link id — the handle for [`Request::RemovePublicLink`].
    pub id: String,
    /// The shareable URL including the `#password` fragment, when known (always on
    /// creation; absent when only listed).
    pub url: Option<String>,
    /// Role granted to anyone with the link ("viewer" or "editor").
    pub role: String,
    /// Expiry, epoch seconds, if set.
    pub expires: Option<i64>,
    /// Whether a custom password additionally protects the link.
    pub has_password: bool,
}

/// One invitation addressed to me (in [`Response::Invitations`]).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InvitationInfo {
    /// Invitation id — the handle for [`Request::AcceptInvitation`]/[`Request::RejectInvitation`].
    pub id: String,
    /// The email of the user who shared the item.
    pub inviter_email: String,
    /// The shared item's decrypted name, when available.
    pub name: Option<String>,
    /// Whether the shared item is a folder.
    pub is_dir: bool,
}

/// One saved public link (in [`Response::Bookmarks`]).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BookmarkInfo {
    /// Bookmark token — the handle for [`Request::DeleteBookmark`].
    pub token: String,
    /// The full public URL including the `#password` fragment.
    pub url: String,
    /// The bookmarked item's decrypted name, when available.
    pub name: Option<String>,
    /// Whether the bookmarked item is a folder.
    pub is_dir: bool,
}

/// One node I have shared with others (in [`Response::SharedByMe`]). Summarizes
/// the share's state so the "Shared" view renders in one pass: how many people
/// have access, how many invitations are still pending, and the public link if
/// the node has one.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SharedItem {
    /// Node uid in `volume~link` form — the handle for opening the node's share
    /// (via its mount path) or its details.
    pub uid: String,
    /// The shared node's decrypted name.
    pub name: String,
    pub is_dir: bool,
    /// Mountpoint-relative path, when the daemon can resolve it (the node lives in
    /// my own tree). Empty when the path is unknown.
    #[serde(default)]
    pub path: String,
    /// Accepted members (people who already have access), excluding me.
    pub member_count: usize,
    /// Pending invitations (Proton + external) not yet accepted.
    pub invite_count: usize,
    /// The node's public link, if it has one.
    pub link: Option<PublicLinkInfo>,
}

/// What happened, in an [`ActivityEntry`]. Kept coarse: a front-end maps each to
/// an icon and a verb, and the human detail lives in the entry's fields.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Upload,
    Download,
    /// A whole sync pass over one folder, summarising what it moved.
    Sync,
    Rename,
    Move,
    CreateFolder,
    Trash,
    Restore,
    DeleteForever,
    EmptyTrash,
    Share,
    PublicLink,
    Unshare,
}

/// One line in the daemon's activity log (in [`Response::Activity`]). Newest
/// first. Records a mutation or transfer the daemon performed, with enough
/// context to read as a sentence: "Uploaded report.pdf to /docs".
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ActivityEntry {
    /// When it happened, epoch seconds.
    pub time: i64,
    pub kind: ActivityKind,
    /// The primary item's name (a file/folder name, or a count like "3 items").
    pub target: String,
    /// Extra context: a destination path, a new name, an invitee, an error
    /// message. Empty when there is nothing to add.
    #[serde(default)]
    pub detail: String,
    /// Whether the operation succeeded. A failed entry still appears, so the log
    /// shows what was attempted.
    pub ok: bool,
}

/// Which way an active transfer is moving, in a [`TransferItem`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Download,
    Upload,
}

/// One long-running daemon job in a [`Response::Transfers`] snapshot: work that
/// takes long enough to need reporting but doesn't move bytes over the wire —
/// walking a local tree, building a remote folder skeleton, indexing `$HOME`.
/// Byte-moving work is a [`TransferItem`] instead.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JobItem {
    /// What the job is, as a front-end would title it ("Uploading files").
    pub title: String,
    /// What it is doing right now ("Scanning Photos/2024"), or empty.
    pub detail: String,
    /// Steps finished so far.
    pub done: u64,
    /// Steps known to need doing, or `0` when unknown (indeterminate progress).
    /// May *grow* mid-job as more work is discovered.
    pub total: u64,
}

/// One in-flight transfer in a [`Response::Transfers`] snapshot.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TransferItem {
    /// Node uid in `volume~link` form (empty for an upload whose uid isn't known
    /// until the draft is sealed).
    pub uid: String,
    /// File name being transferred.
    pub name: String,
    pub direction: TransferDirection,
    /// Bytes moved so far.
    pub bytes_completed: u64,
    /// Total bytes expected, or `0` when unknown (indeterminate progress).
    pub bytes_total: u64,
    /// Average throughput since the transfer began, bytes per second.
    pub speed_bytes_sec: u64,
}

/// One entry in a [`Request::ListDir`] listing.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DirEntry {
    /// Decrypted node name.
    pub name: String,
    pub is_dir: bool,
    /// Plaintext size in bytes (0 for folders).
    pub size: u64,
    /// Modification time, epoch seconds.
    pub modified: i64,
    /// Whether the file is pinned to this device.
    pub pinned: bool,
    /// Whether the file's full content is present in the local cache (a current,
    /// non-stale blob). Always false for folders. Defaulted for wire-compat with
    /// clients/daemons predating the field.
    #[serde(default)]
    pub cached: bool,
    /// Node uid in `volume~link` form, for follow-up requests.
    pub uid: String,
    /// Full mountpoint-relative path. Empty for a [`Request::ListDir`] listing
    /// (the entry lives in the requested directory, so the caller derives the
    /// path by joining its name) and for a [`Request::ListTrash`] listing (a
    /// trashed node has no path inside the mount at all); populated when an entry
    /// can live anywhere in the tree, as for search hits rendered through the
    /// browser.
    #[serde(default)]
    pub path: String,
}

/// One hit in a [`Request::Search`] result. Like [`DirEntry`] but carries the
/// full mountpoint-relative `path` (a hit can live anywhere in the tree), so the
/// front-end can navigate to or open it directly.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SearchHit {
    pub name: String,
    /// Mountpoint-relative path (`/`-joined, no leading slash).
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
    pub pinned: bool,
    /// Node uid in `volume~link` form.
    pub uid: String,
}

/// One hit in a [`Request::SearchLocal`] result: a file on this machine, outside
/// Proton Drive. Unlike [`SearchHit`] there is no uid or pin state — the file is
/// already local, so the front-end opens `path` directly.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LocalHit {
    pub name: String,
    /// Absolute path on this machine.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Modification time, epoch seconds.
    pub modified: i64,
}

/// What kind of media a timeline entry is, so the Photos page can split into
/// Photos / Videos / Raw tabs. Derived from a photo's media type when the daemon
/// has resolved it, falling back to its file-name extension (see
/// [`PhotoKind::classify`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhotoKind {
    /// A normal, directly viewable still image (JPEG, PNG, HEIC, …).
    Photo,
    /// A video clip (mp4, mkv, mov, …).
    Video,
    /// A camera raw file (CR2, NEF, ARW, DNG, …) — an image, but one that needs
    /// developing and is worth separating from ready-to-view photos.
    Raw,
}

/// File-name extensions that denote a video, matched case-insensitively.
const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "mov", "avi", "webm", "m4v", "flv", "wmv", "mpg", "mpeg", "ts", "3gp", "m2ts",
    "mts", "ogv",
];

/// File-name extensions that denote a camera raw. The server media type for
/// these is frequently a generic `application/octet-stream`, so the extension is
/// the authoritative signal.
const RAW_EXTS: &[&str] = &[
    "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "dng", "raf", "orf", "rw2", "srw", "pef",
    "raw", "rwl", "iiq", "3fr", "dcr", "kdc", "mrw", "x3f",
];

/// File-name extensions that denote a ready-to-view still photo. Listed so a
/// known image name classifies as a photo outright, without deferring to a media
/// type that might disagree.
const PHOTO_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "bmp", "tif", "tiff",
];

impl PhotoKind {
    /// Classify a photo from what the daemon knows about it. A recognised
    /// file-name extension is authoritative — it is reliable even before the
    /// node's media type is resolved, and for raw files the media type is often a
    /// useless generic — so the media type is only consulted for names that carry
    /// no extension we know. Anything still unresolved is a still photo.
    pub fn classify(name: Option<&str>, media_type: Option<&str>) -> PhotoKind {
        if let Some(ext) = name
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, e)| e.to_ascii_lowercase())
        {
            if RAW_EXTS.contains(&ext.as_str()) {
                return PhotoKind::Raw;
            }
            if VIDEO_EXTS.contains(&ext.as_str()) {
                return PhotoKind::Video;
            }
            if PHOTO_EXTS.contains(&ext.as_str()) {
                return PhotoKind::Photo;
            }
        }
        if let Some(mt) = media_type
            && mt.starts_with("video/")
        {
            return PhotoKind::Video;
        }
        PhotoKind::Photo
    }

    /// The stable integer this kind is persisted as (see the `kind` column of the
    /// `photos` table). Chosen once and never reordered.
    pub fn as_i64(self) -> i64 {
        match self {
            PhotoKind::Photo => 0,
            PhotoKind::Video => 1,
            PhotoKind::Raw => 2,
        }
    }

    /// Inverse of [`PhotoKind::as_i64`]; any unrecognised value reads as a still
    /// photo, the safe default for a tab that would otherwise show nothing.
    pub fn from_i64(v: i64) -> PhotoKind {
        match v {
            1 => PhotoKind::Video,
            2 => PhotoKind::Raw,
            _ => PhotoKind::Photo,
        }
    }
}

/// One month the timeline spans, with how many photos it holds — a tick on the
/// date scrubber (reply to [`Request::PhotoMonths`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhotoMonth {
    /// Local-time calendar year, e.g. `2026`.
    pub year: i32,
    /// Local-time month, `1..=12`.
    pub month: i32,
    /// Photos captured in that month (within the requested kind, if any).
    pub count: usize,
}

/// One photo in a [`Request::PhotosTimeline`] page.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PhotoItem {
    /// Node uid in `volume~link` form.
    pub uid: String,
    /// Capture time, epoch seconds (the timeline is newest-first).
    pub capture_time: i64,
    /// On-disk path to the cached thumbnail, when one is *already* cached. A
    /// `None` here means "not fetched yet", not "has no thumbnail" — ask for it
    /// with [`Request::PhotoThumbs`].
    pub thumb_path: Option<String>,
    /// File name, when the daemon knows it.
    pub name: Option<String>,
    /// Aspect ratio (w/h), remembered by the daemon from the last time this
    /// photo's thumbnail was decoded. Lets the gallery justify its rows correctly
    /// on the first frame instead of guessing and reflowing as images land.
    pub ratio: Option<f64>,
    /// True when this photo can never be given a thumbnail — the server has none
    /// and its bytes could not be decoded locally. The tile shows a placeholder
    /// rather than waiting for an image that will never come.
    pub no_thumb: bool,
    /// Which Photos-page tab this entry belongs to. Older daemons that predate
    /// the split omit it; a front-end then treats everything as a still photo.
    #[serde(default = "default_photo_kind")]
    pub kind: PhotoKind,
}

/// A daemon too old to classify a timeline entry is assumed to have served a
/// still photo — that was the only kind the Photos page showed before the split.
fn default_photo_kind() -> PhotoKind {
    PhotoKind::Photo
}

/// One thumbnail in a [`Response::Thumbs`] batch.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PhotoThumb {
    pub uid: String,
    /// On-disk path, or `None` when there is no thumbnail to serve *yet*.
    pub path: Option<String>,
    /// True when the daemon is making this thumbnail itself, because the server
    /// has none: the photo's full file is downloading and will be scaled when it
    /// lands. A `None` path with `pending` set means "ask again shortly"; a `None`
    /// path *without* it means the photo can never have a thumbnail, and a
    /// front-end should stop asking.
    ///
    /// Generation is not made to block the reply: one 20 MB camera photo takes
    /// far longer to fetch than the whole rest of a batch, and holding the batch
    /// for it would leave a screenful of ready thumbnails unpainted.
    pub pending: bool,
}

/// A daemon too old to report connectivity is assumed online — it could not
/// have mounted at all otherwise.
fn default_online() -> bool {
    true
}

/// Phrase the queue depth of a [`Response::Status`] for a human, or `None` when
/// there is nothing queued and the caller should say nothing at all.
///
/// Lives here, next to the counts it describes, because the tray, the CLI and
/// the manager window all have to draw the same distinction between bytes that
/// have not reached the remote and metadata that has not.
pub fn pending_summary(uploads: u64, changes: u64) -> Option<String> {
    let part = |n: u64, one: &str, many: &str| match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    let parts: Vec<String> = [
        part(uploads, "upload", "uploads"),
        part(changes, "change", "changes"),
    ]
    .into_iter()
    .flatten()
    .collect();
    match parts.is_empty() {
        true => None,
        false => Some(format!("{} queued", parts.join(", "))),
    }
}

/// The daemon's reply to a [`Request`].
#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    /// Current mount status. Carries the cache stats the daemon already holds
    /// (`used`/`budget` bytes and the pin list) so a front-end never has to open
    /// the on-disk cache itself on its UI thread.
    Status {
        username: String,
        mountpoint: String,
        pinned: usize,
        /// Bytes of cached content blobs (see [`crate::cache::ContentCache::usage`]).
        used: u64,
        /// Configured soft byte cap (`0` = unlimited).
        budget: u64,
        /// The pin registry.
        pins: Vec<Pin>,
        /// False when the daemon is serving the cached tree because the API is
        /// unreachable (offline.md Phase 1). Cached and pinned content still
        /// reads; anything else fails until the network is back.
        #[serde(default = "default_online")]
        online: bool,
        /// Writes accepted locally but not yet uploaded (offline.md Phase 3).
        /// Non-zero means the mount is ahead of the remote — either a copy is
        /// still draining, or it cannot drain because we are offline.
        #[serde(default)]
        pending_uploads: u64,
        /// Queued mutations that carry no bytes: `mkdir`, `rename`, `trash`
        /// (offline.md Phase 3b). Counted apart from `pending_uploads` because
        /// calling a queued `mkdir` an upload is a lie.
        #[serde(default)]
        pending_changes: u64,
    },
    /// A human-readable success message.
    Ok { message: String },
    /// The pin registry.
    Pins { pins: Vec<Pin> },
    /// A directory listing (reply to [`Request::ListDir`]) or a trash listing
    /// (reply to [`Request::ListTrash`]).
    Entries { entries: Vec<DirEntry> },
    /// A page of the photos timeline. `available` is false when the account
    /// has no photos volume.
    Photos {
        available: bool,
        items: Vec<PhotoItem>,
        /// Whole-timeline tab counts `(photos, videos, raw)`, so a front-end can
        /// label its Photos / Videos / Raw filter without paging the library.
        /// The counts describe the *whole* timeline regardless of the page's own
        /// `kind` filter. Older daemons omit it; a front-end then shows no counts.
        #[serde(default)]
        counts: Option<(usize, usize, usize)>,
    },
    /// The months the timeline spans (reply to [`Request::PhotoMonths`]),
    /// newest first.
    PhotoMonths { months: Vec<PhotoMonth> },
    /// Thumbnails for a [`Request::PhotoThumbs`] batch.
    Thumbs { items: Vec<PhotoThumb> },
    /// An on-disk path the front-end can open (e.g. a downloaded photo).
    FilePath { path: String },
    /// Full-text search results (reply to [`Request::Search`]).
    SearchResults { hits: Vec<SearchHit> },
    /// Local-file search results (reply to [`Request::SearchLocal`]). `indexing`
    /// is true while a scan of the machine is still running, so a front-end can
    /// say "still indexing" instead of "no matches" on a cold first launch.
    LocalResults { hits: Vec<LocalHit>, indexing: bool },
    /// A snapshot of what the daemon is working on (reply to
    /// [`Request::GetQueueStatus`]): `items` are byte-moving transfers, `jobs`
    /// the longer non-transfer work around them (scans, folder skeletons, the
    /// local index, sync passes). Both empty means the daemon is idle.
    Transfers {
        items: Vec<TransferItem>,
        #[serde(default)]
        jobs: Vec<JobItem>,
    },
    /// The account's devices (reply to [`Request::ListDevices`]).
    Devices { items: Vec<DeviceInfo> },
    /// This device's synced folders (reply to [`Request::ListSyncFolders`]).
    SyncFolders { items: Vec<SyncFolderInfo> },
    /// A node's share: members + pending invitations, and its public link if any
    /// (reply to [`Request::ListShare`]).
    Share {
        entries: Vec<ShareEntry>,
        link: Option<PublicLinkInfo>,
    },
    /// A freshly created public link, carrying the shareable URL (reply to
    /// [`Request::CreatePublicLink`]).
    PublicLink { link: PublicLinkInfo },
    /// Invitations addressed to me (reply to [`Request::ListInvitations`]).
    Invitations { items: Vec<InvitationInfo> },
    /// Saved public links (reply to [`Request::ListBookmarks`]).
    Bookmarks { items: Vec<BookmarkInfo> },
    /// Nodes I have shared with others (reply to [`Request::ListSharedByMe`]).
    SharedByMe { items: Vec<SharedItem> },
    /// The daemon's recent activity, newest first (reply to
    /// [`Request::ListActivity`]).
    Activity { items: Vec<ActivityEntry> },
    /// The request failed. `message` is for the user; `kind` is for the code —
    /// a front-end decides its copy and whether to offer a retry from `kind`,
    /// never by matching on the text.
    Error {
        message: String,
        #[serde(default)]
        kind: ErrorKind,
    },
}

impl Response {
    /// Build a failure reply from a classified error, so the `kind` a
    /// request-serving method decided survives the trip to the front-end.
    pub fn error(e: crate::error::CoreError) -> Self {
        Response::Error {
            message: e.message,
            kind: e.kind,
        }
    }
}

/// What class of thing went wrong, as opposed to what it read like.
///
/// The daemon answers most calls with prose assembled from whatever layer
/// failed (`"resolve path: ENOENT"`), which is fine to show and useless to
/// branch on. This is the branchable half: enough to pick the right copy, to
/// know whether retrying is meaningful, and to tell a caller's mistake apart
/// from an outage.
///
/// Deliberately coarse. A variant earns its place by changing what a front-end
/// *does*, not by naming a distinct cause — anything finer belongs in `message`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The API is unreachable and the request needed it. The cached tree is
    /// still being served, so this is a "not right now", not a failure of the
    /// thing the user asked for.
    Offline,
    /// No node at that path or uid. Usually means the front-end's listing is
    /// stale, so the useful response is to reload it rather than to retry.
    NotFound,
    /// The account may not do that to that node — a viewer trying to write, a
    /// share whose role was downgraded. Retrying changes nothing.
    Denied,
    /// The remote moved underneath the request: a name already taken, a
    /// revision superseded. The caller has to decide, so never auto-retried.
    Conflict,
    /// The request itself was malformed — an empty name, a path with a `/` in
    /// it, an unparseable uid. A bug in the caller, not a condition to retry.
    Invalid,
    /// The API was reached and refused, or the transfer broke. The one class
    /// where an unchanged retry can legitimately succeed.
    Remote,
    /// The account is out of storage. Distinct from [`Denied`](Self::Denied)
    /// because the user *can* fix it, and distinct from [`Remote`](Self::Remote)
    /// because retrying an upload that did not fit will not make it fit.
    Quota,
    /// Something on this machine failed: the database, the content cache, the
    /// filesystem. Not the user's doing and not theirs to fix.
    #[default]
    Internal,
}

impl ErrorKind {
    /// Whether repeating the identical request could plausibly succeed.
    ///
    /// Drives whether a front-end offers "Try again" at all: offering it for a
    /// [`NotFound`](Self::NotFound) or an [`Invalid`](Self::Invalid) teaches the
    /// user that the button does nothing.
    pub fn retryable(self) -> bool {
        matches!(self, ErrorKind::Offline | ErrorKind::Remote)
    }
}

/// Send one [`Request`] to the daemon listening on `socket` and read its
/// [`Response`]. Errors (e.g. [`Error::Io`]) if no daemon is listening.
///
/// Shared by the CLI and GUI so both speak the wire format identically.
pub fn send(socket: &Path, req: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    Ok(serde_json::from_str(resp.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_kind_classifies_by_extension_then_mime() {
        use PhotoKind::*;
        // Raw is extension-driven: the server media type is often generic.
        assert_eq!(PhotoKind::classify(Some("IMG_1.CR2"), None), Raw);
        assert_eq!(
            PhotoKind::classify(Some("shot.dng"), Some("application/octet-stream")),
            Raw
        );
        // Video by extension or by mime when the name has no useful extension.
        assert_eq!(PhotoKind::classify(Some("anime.mkv"), None), Video);
        assert_eq!(PhotoKind::classify(Some("clip.MP4"), None), Video);
        assert_eq!(PhotoKind::classify(None, Some("video/quicktime")), Video);
        // Everything else is a still photo, including a mismatched raw mime whose
        // name says it is a normal JPEG.
        assert_eq!(PhotoKind::classify(Some("pic.jpg"), None), Photo);
        assert_eq!(PhotoKind::classify(Some("pic.heic"), None), Photo);
        assert_eq!(PhotoKind::classify(None, None), Photo);
        // Extension wins over mime: a name ending .jpg is a photo even if the mime
        // is nonsense.
        assert_eq!(PhotoKind::classify(Some("x.jpg"), Some("video/mp4")), Photo);
    }

    #[test]
    fn photo_kind_i64_round_trips() {
        for k in [PhotoKind::Photo, PhotoKind::Video, PhotoKind::Raw] {
            assert_eq!(PhotoKind::from_i64(k.as_i64()), k);
        }
        // An unknown persisted value degrades to a still photo.
        assert_eq!(PhotoKind::from_i64(99), PhotoKind::Photo);
    }

    /// The whole point of the split counts: a queued `mkdir` is work, but it is
    /// not an upload and must never be reported as one.
    #[test]
    fn pending_summary_separates_uploads_from_other_changes() {
        assert_eq!(pending_summary(0, 0), None);
        assert_eq!(pending_summary(1, 0).as_deref(), Some("1 upload queued"));
        assert_eq!(pending_summary(3, 0).as_deref(), Some("3 uploads queued"));
        assert_eq!(pending_summary(0, 1).as_deref(), Some("1 change queued"));
        assert_eq!(
            pending_summary(2, 4).as_deref(),
            Some("2 uploads, 4 changes queued")
        );
    }

    /// The mutation requests must survive a line-delimited JSON round-trip, since
    /// that is exactly how they cross the control socket.
    #[test]
    fn mutation_requests_roundtrip() {
        let reqs = [
            Request::Rename {
                path: "a/b.txt".into(),
                new_name: "c.txt".into(),
            },
            Request::Move {
                path: "a/b.txt".into(),
                new_parent: "d".into(),
            },
            Request::Delete {
                path: "a/b.txt".into(),
            },
            Request::CreateFolder {
                parent: "a".into(),
                name: "new".into(),
            },
            Request::UploadFile {
                parent: "a".into(),
                name: "f.bin".into(),
                bytes: vec![0, 1, 2, 255],
            },
            Request::UploadPaths {
                parent: "a".into(),
                sources: vec!["/home/u/x.txt".into(), "/home/u/pics".into()],
            },
        ];
        for req in reqs {
            let line = serde_json::to_string(&req).unwrap();
            assert!(!line.contains('\n'), "wire form must be a single line");
            let back: Request = serde_json::from_str(&line).unwrap();
            // Round-trip is lossless: re-serializing yields the same bytes.
            assert_eq!(line, serde_json::to_string(&back).unwrap());
        }
    }

    /// The trash requests carry uids rather than paths; they must survive the same
    /// line-delimited round-trip, since a mangled uid would restore or destroy the
    /// wrong node.
    #[test]
    fn trash_requests_roundtrip() {
        let reqs = [
            Request::ListTrash,
            Request::Restore {
                uids: vec!["vol~link".into(), "vol~other".into()],
            },
            Request::DeleteForever {
                uids: vec!["vol~link".into()],
            },
            Request::EmptyTrash,
            Request::Refresh {
                scope: RefreshScope::Dir { path: "a/b".into() },
            },
            Request::Refresh {
                scope: RefreshScope::Dir {
                    path: String::new(),
                },
            },
            Request::Refresh {
                scope: RefreshScope::Trash,
            },
            Request::Refresh {
                scope: RefreshScope::Photos,
            },
        ];
        for req in reqs {
            let line = serde_json::to_string(&req).unwrap();
            assert!(!line.contains('\n'), "wire form must be a single line");
            let back: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(line, serde_json::to_string(&back).unwrap());
        }
    }

    /// The sharing and devices requests must survive the same line-delimited JSON
    /// round-trip: a mangled id or role would touch the wrong share or member.
    #[test]
    fn sharing_requests_roundtrip() {
        let reqs = [
            Request::ListDevices,
            Request::RenameDevice {
                uid: "dev-1".into(),
                name: "laptop".into(),
            },
            Request::DeleteDevice {
                uid: "dev-1".into(),
            },
            Request::AddSyncFolder {
                local_path: "/home/me/Docs".into(),
            },
            Request::ListSyncFolders,
            Request::RemoveSyncFolder {
                id: 3,
                delete_remote: true,
            },
            Request::SetSyncFolderMode {
                id: 3,
                mode: "ondemand".into(),
            },
            Request::SyncNow { id: Some(3) },
            Request::ShareNode {
                path: "a/b".into(),
                emails: vec!["x@proton.me".into(), "y@example.com".into()],
                role: "editor".into(),
                message: Some("hi".into()),
            },
            Request::ListShare { path: "a/b".into() },
            Request::UpdateShareRole {
                path: "a/b".into(),
                id: "mid-1".into(),
                kind: ShareEntryKind::Member,
                role: "admin".into(),
            },
            Request::RemoveShareEntry {
                path: "a/b".into(),
                id: "iid-1".into(),
                kind: ShareEntryKind::ExternalInvite,
            },
            Request::CreatePublicLink {
                path: "a/b".into(),
                role: "viewer".into(),
                password: Some("pw".into()),
                expires: Some(1_700_000_000),
            },
            Request::RemovePublicLink {
                path: "a/b".into(),
                id: "url-1".into(),
            },
            Request::ListSharedByMe,
            Request::ListActivity { limit: 100 },
            Request::ListSharedWithMe,
            Request::LeaveShared {
                uid: "vol~link".into(),
            },
            Request::ListInvitations,
            Request::AcceptInvitation { id: "inv-1".into() },
            Request::RejectInvitation { id: "inv-1".into() },
            Request::ListBookmarks,
            Request::CreateBookmark {
                url: "https://drive.proton.me/urls/tok#pw".into(),
                password: None,
            },
            Request::DeleteBookmark {
                token: "tok".into(),
            },
        ];
        for req in reqs {
            let line = serde_json::to_string(&req).unwrap();
            assert!(!line.contains('\n'), "wire form must be a single line");
            let back: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(line, serde_json::to_string(&back).unwrap());
        }
    }

    /// A daemon built before `kind` existed sends `Error` without the field.
    /// It must still parse, and land on the class that promises the least.
    #[test]
    fn an_error_without_a_kind_reads_as_internal() {
        let wire = r#"{"Error":{"message":"something broke"}}"#;
        let back: Response = serde_json::from_str(wire).unwrap();
        match back {
            Response::Error { message, kind } => {
                assert_eq!(message, "something broke");
                assert_eq!(kind, ErrorKind::Internal);
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn error_kind_survives_the_wire() {
        for kind in [
            ErrorKind::Offline,
            ErrorKind::NotFound,
            ErrorKind::Denied,
            ErrorKind::Conflict,
            ErrorKind::Invalid,
            ErrorKind::Remote,
            ErrorKind::Internal,
        ] {
            let line = serde_json::to_string(&Response::Error {
                message: "x".into(),
                kind,
            })
            .unwrap();
            let back: Response = serde_json::from_str(&line).unwrap();
            match back {
                Response::Error { kind: got, .. } => assert_eq!(got, kind),
                other => panic!("expected an error, got {other:?}"),
            }
        }
    }

    /// Retry is offered to the user off the back of this, so it has to mean
    /// "an identical request could work", not "this looks recoverable".
    #[test]
    fn only_offline_and_remote_are_worth_retrying() {
        assert!(ErrorKind::Offline.retryable());
        assert!(ErrorKind::Remote.retryable());
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::Denied,
            ErrorKind::Conflict,
            ErrorKind::Invalid,
            ErrorKind::Internal,
        ] {
            assert!(!kind.retryable(), "{kind:?} must not offer a retry");
        }
    }

    #[test]
    fn response_error_carries_a_core_errors_classification() {
        let r = Response::error(crate::error::CoreError::not_found("no such file"));
        match r {
            Response::Error { message, kind } => {
                assert_eq!(message, "no such file");
                assert_eq!(kind, ErrorKind::NotFound);
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
