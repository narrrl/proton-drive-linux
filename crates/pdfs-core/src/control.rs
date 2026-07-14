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
    PhotosTimeline { offset: usize, limit: usize },
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
    /// Delete all unpinned cached blobs and on-demand blocks, keeping pinned
    /// files intact. Replies with [`Response::Ok`] reporting the bytes freed.
    PurgeCache,
    /// Retune the on-disk cache's soft byte cap at runtime (`0` = unlimited) and
    /// persist it to config so the next mount keeps it. Replies with
    /// [`Response::Ok`].
    SetCacheBudget { bytes: u64 },
    /// Snapshot the daemon's in-flight transfers (active uploads/downloads).
    /// Replies with [`Response::Transfers`]. Cheap to poll: the daemon keeps the
    /// registry in memory, so a front-end can render a live progress widget.
    GetQueueStatus,
}

/// Which way an active transfer is moving, in a [`TransferItem`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Download,
    Upload,
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
    /// path by joining its name); populated when an entry can live anywhere in
    /// the tree, as for search hits rendered through the browser.
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
}

/// One thumbnail in a [`Response::Thumbs`] batch.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PhotoThumb {
    pub uid: String,
    /// On-disk path, or `None` when the photo genuinely has no thumbnail (or the
    /// fetch failed) — a front-end can then stop asking for it.
    pub path: Option<String>,
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
    },
    /// A human-readable success message.
    Ok { message: String },
    /// The pin registry.
    Pins { pins: Vec<Pin> },
    /// A directory listing (reply to [`Request::ListDir`]).
    Entries { entries: Vec<DirEntry> },
    /// A page of the photos timeline. `available` is false when the account
    /// has no photos volume.
    Photos {
        available: bool,
        items: Vec<PhotoItem>,
    },
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
    /// A snapshot of in-flight transfers (reply to [`Request::GetQueueStatus`]).
    Transfers { items: Vec<TransferItem> },
    /// The request failed.
    Error { message: String },
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
        ];
        for req in reqs {
            let line = serde_json::to_string(&req).unwrap();
            assert!(!line.contains('\n'), "wire form must be a single line");
            let back: Request = serde_json::from_str(&line).unwrap();
            // Round-trip is lossless: re-serializing yields the same bytes.
            assert_eq!(line, serde_json::to_string(&back).unwrap());
        }
    }
}
