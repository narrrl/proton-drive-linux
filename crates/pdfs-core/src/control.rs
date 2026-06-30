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
    /// Fetch a page of the photos timeline, newest first. Thumbnails for the
    /// page are fetched into the cache and their on-disk paths returned.
    PhotosTimeline { offset: usize, limit: usize },
    /// Download a photo's full content into the cache; replies with its path.
    OpenPhoto { uid: String },
    /// Download a Drive file's full content into the cache; replies with the
    /// on-disk path so the front-end can open it with the default app. `path`
    /// is mountpoint-relative.
    OpenFile { path: String },
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
    /// Node uid in `volume~link` form, for follow-up requests.
    pub uid: String,
}

/// One photo in a [`Request::PhotosTimeline`] page.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PhotoItem {
    /// Node uid in `volume~link` form.
    pub uid: String,
    /// Capture time, epoch seconds (the timeline is newest-first).
    pub capture_time: i64,
    /// On-disk path to the cached thumbnail, if one was available/fetched.
    pub thumb_path: Option<String>,
}

/// The daemon's reply to a [`Request`].
#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    /// Current mount status.
    Status {
        username: String,
        mountpoint: String,
        pinned: usize,
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
    /// An on-disk path the front-end can open (e.g. a downloaded photo).
    FilePath { path: String },
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
