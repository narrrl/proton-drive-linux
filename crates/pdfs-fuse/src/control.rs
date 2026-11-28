//! The control socket: the daemon`s IPC surface.
//!
//! The CLI and the GUI never touch the mount or the database directly — they
//! open this Unix socket, write one JSON [`CtlRequest`] line, and read one JSON
//! [`CtlResponse`] line back. That keeps the daemon the single writer of both
//! the FUSE state and `cache.db`.
//!
//! Connections are served concurrently, one thread each, because requests differ
//! wildly in cost: an `OpenPhoto` downloads a whole photo, and a serial loop
//! would stall the GUI`s 2s status poll behind it.

use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pdfs_core::config::AppDirs;
use pdfs_core::control::ErrorKind;
use pdfs_core::control::{
    ActivityKind, Event, HEARTBEAT, PhotoMonth, RefreshScope, Request as CtlRequest,
    Response as CtlResponse, Topic, TransferDirection, TrashFailure,
};
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{debug, info, warn};

use super::transfers::CountingReader;
use super::{Core, count_noun, human_bytes, human_duration, parse_uid};

mod photos;
mod revisions;
mod share;
mod sync;
mod trash;

const MAX_CONTROL_REQUEST_BYTES: u64 = 1024 * 1024;
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Answer a [`CtlRequest::FolderSize`].
fn folder_size_reply(size: CoreResult<proton_drive_rs::FolderSize>) -> CtlResponse {
    match size {
        Ok(size) => CtlResponse::FolderSize {
            size: size.size,
            items: size.descendants,
        },
        Err(e) => CtlResponse::error(e),
    }
}

/// A client that connects, sends a request and then never reads its reply used
/// to park a handler thread forever in `write_all`; 64 of those exhaust
/// [`MAX_CONTROL_HANDLERS`] permanently. Generous enough for the largest
/// responses (photo bytes) over a local socket, finite so a wedged peer can't
/// hold a permit.
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONTROL_HANDLERS: usize = 64;
/// Ceiling on any client-supplied row `limit`. Every in-tree caller asks for far
/// less (200 for activity, 60 for a photo page), but the value goes straight
/// into `SQL LIMIT ?` under the daemon's single connection — an unclamped one
/// materialises the whole account, and then serialises it into one JSON line.
const MAX_ROW_LIMIT: usize = 1000;

/// Clamp a client-supplied row limit to [`MAX_ROW_LIMIT`].
fn clamp_limit(limit: usize) -> usize {
    limit.min(MAX_ROW_LIMIT)
}
/// Answer an album add or remove: parse the uids, run `change`, and report
/// per photo.
fn album_change(
    uid: &str,
    photos: &[String],
    change: impl FnOnce(&NodeUid, &[NodeUid]) -> CoreResult<super::photos::AlbumOutcome>,
) -> CtlResponse {
    let Some(album) = parse_uid(uid) else {
        return CtlResponse::error(CoreError::invalid(format!("bad album uid: {uid}")));
    };
    let photos = match Core::parse_uids(photos) {
        Ok(photos) => photos,
        Err(e) => return CtlResponse::error(e),
    };
    match change(&album, &photos) {
        Ok((changed, failed)) => CtlResponse::AlbumChanged {
            changed,
            failed: failed
                .into_iter()
                .map(|(uid, message)| TrashFailure { uid, message })
                .collect(),
        },
        Err(e) => CtlResponse::error(e),
    }
}

static ACTIVE_CONTROL_HANDLERS: AtomicUsize = AtomicUsize::new(0);

struct ControlHandlerPermit;

impl ControlHandlerPermit {
    fn acquire() -> Option<Self> {
        ACTIVE_CONTROL_HANDLERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONTROL_HANDLERS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for ControlHandlerPermit {
    fn drop(&mut self) {
        ACTIVE_CONTROL_HANDLERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Handler threads in use, for the diagnostics. At the limit the daemon refuses
/// connections, which a front-end cannot tell apart from a dead daemon.
pub(crate) fn active_handlers() -> usize {
    ACTIVE_CONTROL_HANDLERS.load(Ordering::Acquire)
}

pub(crate) fn handler_limit() -> usize {
    MAX_CONTROL_HANDLERS
}

fn read_request_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(take) > MAX_CONTROL_REQUEST_BYTES as usize {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control request exceeds size limit",
            ));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.ends_with(b"\n") {
            break;
        }
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() as u64 > MAX_CONTROL_REQUEST_BYTES || !bytes.ends_with(b"\n") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control request exceeds limit or is not newline terminated",
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Turn a CLI-supplied path into the mount that owns it and the path relative
/// to *that* mount's root.
///
/// [`rel_to_mount`] is the older, primary-only reading, and it is what most
/// requests still use. It rejects anything outside the primary
/// mountpoint, which leaves a secondary on-demand mount — a synced folder
/// switched out of mirror mode — with no way to be named at all: `pdfs ls` and
/// `pdfs refresh` both answered "is not under the mountpoint", so a folder
/// serving a stale listing had no manual escape hatch (`docs/BUGS.md` B86).
///
/// A relative path keeps the old meaning, relative to the primary mount, since
/// there is nothing in it to choose a mount by.
fn route_to_mount(core: &Core, mountpoint: &Path, path: &str) -> CoreResult<(Core, PathBuf)> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Ok((core.clone(), p.to_path_buf()));
    }
    if let Some(routed) = core.rooted_at(p) {
        return Ok(routed);
    }
    // Fall back to the primary reading so the error text stays the familiar one
    // when the path is simply outside everything this daemon has mounted.
    rel_to_mount(mountpoint, path).map(|rel| (core.clone(), rel))
}

/// [`route_to_mount`] for a request that changes what it names: trash, rename,
/// create a folder, resolve a conflict. Each has to run against the on-demand
/// mount serving the path, and `pdfs rm ~/Downloads/x` answered "is not under
/// the mountpoint" while `pdfs move` took the same path. The conflict listing
/// names a copy in a synced folder by its absolute local path (`docs/BUGS.md`
/// B99), so resolving one needs this too. A mirror folder has no mount to run
/// against: a file there is an ordinary local file.
fn route_change(core: &Core, mountpoint: &Path, path: &str) -> CoreResult<(Core, PathBuf)> {
    route_within(core, mountpoint, path, |path| {
        format!("{path} is in a mirrored folder; change the local file there instead")
    })
}

/// [`route_to_mount`] for keeping a node offline or releasing it. A file in an
/// on-demand synced folder is a different Drive node from any file of the
/// same name under the primary mount, and keeping one offline did nothing for
/// the other: a program reading the synced folder's copy downloaded it
/// (`docs/BUGS.md` B281). A mirror folder already holds every file on disk.
fn route_keep(core: &Core, mountpoint: &Path, path: &str) -> CoreResult<(Core, PathBuf)> {
    route_within(core, mountpoint, path, |path| {
        format!(
            "{path} is in a mirrored folder, which keeps all its files on this computer already"
        )
    })
}

/// [`route_to_mount`], refusing a path that leaves the mount it routed to and
/// answering a path in a mirror folder, which has no mount, with `mirrored`.
fn route_within(
    core: &Core,
    mountpoint: &Path,
    path: &str,
    mirrored: impl FnOnce(&str) -> String,
) -> CoreResult<(Core, PathBuf)> {
    let routed = route_to_mount(core, mountpoint, path).map_err(|e| {
        let p = Path::new(path);
        let in_mirror = core
            .db
            .sync_folder_list()
            .unwrap_or_default()
            .iter()
            .any(|folder| folder.mode == "mirror" && p.starts_with(&folder.local_path));
        if in_mirror {
            CoreError::invalid(mirrored(path))
        } else {
            e
        }
    })?;
    if routed
        .1
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        Ok(routed)
    } else {
        Err(CoreError::invalid(format!("{path} escapes the mountpoint")))
    }
}

/// The path the pin registry keeps for a pin made through `routed`: the
/// mount-relative path on the primary mount, as it always was, and the
/// absolute path the request named on an on-demand folder's mount, since
/// that node is not under the primary mountpoint (`docs/BUGS.md` B281).
fn pin_record(routed: &Core, rel: &Path, path: &str) -> PathBuf {
    if routed.session.primary {
        rel.to_path_buf()
    } else {
        Path::new(path).components().collect()
    }
}

/// [`route_change`] for a request that trashes or renames the node it names.
/// A mount's root is My files or a whole synced folder. An on-demand folder's
/// root has a parent on Drive, so nothing further down would stop `pdfs rm
/// ~/Downloads` from trashing the folder out from under its own mount.
fn route_node(core: &Core, mountpoint: &Path, path: &str) -> CoreResult<(Core, PathBuf)> {
    let routed = route_change(core, mountpoint, path)?;
    if below_root(&routed.1) {
        Ok(routed)
    } else {
        Err(CoreError::invalid(format!(
            "{path} is the root of a synced location and cannot be changed"
        )))
    }
}

/// Whether a mount-relative path names something below the mount's root.
fn below_root(rel: &Path) -> bool {
    rel.components()
        .any(|component| matches!(component, Component::Normal(_)))
}

/// Turn a CLI-supplied path into a mountpoint-relative path. An absolute path
/// must live under `mountpoint`; a relative path is taken as already relative to
/// the mount root.
fn rel_to_mount(mountpoint: &Path, path: &str) -> CoreResult<PathBuf> {
    let p = Path::new(path);
    let relative = if p.is_absolute() {
        p.strip_prefix(mountpoint)
            .map(Path::to_path_buf)
            .map_err(|_| CoreError::invalid(format!("{path} is not under the mountpoint")))
    } else {
        Ok(p.to_path_buf())
    }?;

    if relative
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        Ok(relative)
    } else {
        Err(CoreError::invalid(format!("{path} escapes the mountpoint")))
    }
}

/// Handle one control-socket connection: read a single JSON request line,
/// dispatch it against `core`, and write back a JSON response line.
/// Refresh scopes being served right now, one entry per running refresh.
///
/// A `Vec` because there are three scopes and at most `MAX_CONTROL_HANDLERS`
/// handlers, so a set would buy nothing.
static REFRESHING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// What makes two refreshes the same work: the scope, and for a directory the
/// folder, since refreshing one folder says nothing about another. A full
/// photos refresh is not the same work as an ordinary one — it forgets what the
/// daemon knows first — so it keys apart.
fn scope_key(scope: &RefreshScope) -> String {
    match scope {
        RefreshScope::Dir { path } => format!("dir:{path}"),
        RefreshScope::Trash => "trash".to_string(),
        RefreshScope::Photos { full: true } => "photos:full".to_string(),
        RefreshScope::Photos { full: false } => "photos".to_string(),
    }
}

/// Holds one refresh scope for as long as it is being served. `acquire`
/// returns `None` when that scope is already in flight.
struct RefreshGuard(String);

impl RefreshGuard {
    fn acquire(scope: &RefreshScope) -> Option<Self> {
        let key = scope_key(scope);
        let mut running = REFRESHING.lock().unwrap_or_else(|e| e.into_inner());
        if running.contains(&key) {
            return None;
        }
        running.push(key.clone());
        Some(Self(key))
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        let mut running = REFRESHING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = running.iter().position(|key| *key == self.0) {
            running.swap_remove(at);
        }
    }
}

/// The reply to [`CtlRequest::Status`], which subscriptions are also sent
/// whenever it changes.
pub(crate) fn status_response(core: &Core, username: &str, mountpoint: &Path) -> CtlResponse {
    let pins = core.cache.list_pins();
    let queued = core.db.pending_op_counts().unwrap_or_default();
    // Read after the queue: the drain counts an upload as landing before it
    // retires the op, so one of the two always has it (B119).
    let landing = core.drain.landing_uploads.load(Ordering::SeqCst);
    let staging = core.cache.staging_usage();
    CtlResponse::Status {
        parked_uploads: queued.parked.max(0) as u64,
        failing_ops: queued.failing.max(0) as u64,
        failing_error: queued.last_error.clone(),
        staged_bytes: staging.bytes,
        staged_oldest_secs: staging.oldest_secs,
        paused: core.sync_pause().is_some(),
        paused_until: core.sync_pause().flatten(),
        username: username.to_string(),
        mountpoint: mountpoint.display().to_string(),
        pinned: pins.len(),
        used: core.cache.usage(),
        budget: core.cache.budget(),
        pins,
        online: core.is_online(),
        session_expired: core.session_expired(),
        tokens_unsaved: pdfs_core::auth::tokens_unsaved(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        pending_uploads: queued.uploads.max(0) as u64 + landing,
        pending_changes: queued.changes.max(0) as u64,
    }
}

/// What a front-end shows that `request` changes when it succeeds. Requests
/// from the CLI publish the same way as the app's, so an open window follows
/// a change made in a terminal.
fn touched_topics(request: &CtlRequest) -> &'static [Topic] {
    use CtlRequest as R;
    match request {
        R::Pin { .. } | R::Unpin { .. } => &[Topic::Files],
        // Only locking invalidates a hidden view. Authentication does not
        // change the public catalog and must not emit this notification.
        R::DriveGallery {
            action: pdfs_core::control::GalleryAction::Lock,
        } => &[Topic::GalleryVisibility],
        R::CreateAlbum { .. }
        | R::RenameAlbum { .. }
        | R::DeleteAlbum { .. }
        | R::AddToAlbum { .. }
        | R::RemoveFromAlbum { .. }
        | R::SetPhotoFavorite { .. }
        | R::UploadPhoto { .. } => &[Topic::Photos],
        R::TrashNodes { .. } => &[Topic::Files, Topic::Photos, Topic::Trash, Topic::Conflicts],
        R::Rename { .. } | R::Move { .. } | R::CreateFolder { .. } | R::UploadPaths { .. } => {
            &[Topic::Files, Topic::Conflicts]
        }
        R::Delete { .. } => &[Topic::Files, Topic::Trash, Topic::Conflicts],
        R::Restore { .. } | R::DeleteForever { .. } | R::EmptyTrash => {
            &[Topic::Files, Topic::Photos, Topic::Trash, Topic::Conflicts]
        }
        R::Refresh { scope } => match scope {
            RefreshScope::Dir { .. } => &[Topic::Files],
            RefreshScope::Trash => &[Topic::Trash],
            // A photos refresh answers once it has started; the timeline
            // publishes again when it lands.
            RefreshScope::Photos { .. } => &[],
        },
        R::RenameDevice { .. } | R::DeleteDevice { .. } => &[Topic::Devices],
        R::AdoptDevice { .. } | R::MigrateDevice { .. } => &[Topic::Devices, Topic::Locations],
        R::AddSyncFolder { .. }
        | R::AttachSyncFolder { .. }
        | R::RemoveSyncFolder { .. }
        | R::SetSyncFolderMode { .. }
        | R::SetSyncFolderPaused { .. }
        | R::ResetSyncFolder { .. }
        | R::RestoreSyncFolders { .. }
        | R::RestoreDeviceFolders { .. } => &[Topic::Locations, Topic::Devices],
        R::RetryPendingOp { .. } => &[Topic::Queue],
        R::DiscardPendingOp { .. } => &[Topic::Queue, Topic::Files],
        R::ResolveConflict { .. } => &[Topic::Files, Topic::Trash, Topic::Conflicts],
        R::ShareNode { .. }
        | R::ShareNodeByUid { .. }
        | R::UpdateShareRole { .. }
        | R::UpdateShareRoleByUid { .. }
        | R::RemoveShareEntry { .. }
        | R::RemoveShareEntryByUid { .. }
        | R::CreatePublicLink { .. }
        | R::CreatePublicLinkByUid { .. }
        | R::RemovePublicLink { .. }
        | R::RemovePublicLinkByUid { .. }
        | R::UpdatePublicLink { .. }
        | R::UpdatePublicLinkByUid { .. }
        | R::StopSharing { .. }
        | R::StopSharingByUid { .. } => &[Topic::Shares, Topic::Files],
        R::LeaveShared { .. }
        | R::AcceptInvitation { .. }
        | R::RejectInvitation { .. }
        | R::CreateBookmark { .. }
        | R::DeleteBookmark { .. } => &[Topic::Shares],
        R::RestoreRevision { .. }
        | R::RestoreRevisionByUid { .. }
        | R::SaveRevisionAs { .. }
        | R::SaveRevisionAsByUid { .. } => &[Topic::Files],
        _ => &[],
    }
}

/// How long a subscription's writes may block. A client that stops reading
/// is dropped after this rather than holding its handler forever.
const SUBSCRIPTION_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve a [`CtlRequest::Subscribe`]: send the current status and transfers
/// and say every topic changed, so the client catches up, then send whatever
/// the [`EventHub`](super::events::EventHub) collects, with a heartbeat when
/// nothing does. Ends when the client goes away or the daemon stops.
fn serve_subscription(core: &Core, username: &str, mountpoint: &Path, mut stream: UnixStream) {
    let Some(mailbox) = core.events.subscribe() else {
        warn!("control: refusing a subscription at the limit");
        if let Ok(mut out) = serde_json::to_vec(&CtlResponse::error(CoreError::remote(
            "too many subscriptions",
        ))) {
            out.push(b'\n');
            let _ = stream.write_all(&out);
        }
        return;
    };
    struct Unsubscribe<'a>(&'a Core, std::sync::Arc<super::events::Mailbox>);
    impl Drop for Unsubscribe<'_> {
        fn drop(&mut self) {
            self.0.events.unsubscribe(&self.1);
        }
    }
    let _unsubscribe = Unsubscribe(core, mailbox.clone());
    if let Err(e) = stream.set_write_timeout(Some(SUBSCRIPTION_WRITE_TIMEOUT)) {
        warn!(error = %e, "control: could not set subscription timeout");
        return;
    }
    let heartbeat = super::events::event_line(&Event::Heartbeat);
    let mut lines: Vec<_> = [
        super::events::event_line(&Event::Status(Box::new(status_response(
            core, username, mountpoint,
        )))),
        super::events::transfers_line(core.transfers.snapshot(), core.jobs_snapshot()),
        super::events::event_line(&Event::Changed {
            topics: Topic::ALL.to_vec(),
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    loop {
        for line in &lines {
            if stream.write_all(line.as_bytes()).is_err() {
                return;
            }
        }
        if core.shutdown.is_stopping() {
            return;
        }
        lines = mailbox.take(HEARTBEAT);
        if lines.is_empty() {
            lines.extend(heartbeat.clone());
        }
    }
}

/// What a request's answer may need besides the request itself.
#[derive(Clone, Copy)]
struct Ask<'a> {
    core: &'a Core,
    mountpoint: &'a Path,
    /// The connection, for requests that check who is asking.
    stream: &'a UnixStream,
    /// What the hidden gallery keeps out of Photos answers.
    visibility: &'a super::gallery::Visibility,
    /// [`Ask::visibility`]'s hidden media, as the JSON the database filters on.
    excluded: &'a str,
}

fn handle_control_conn(core: &Core, username: &str, mountpoint: &Path, stream: UnixStream) {
    if let Err(e) = stream.set_read_timeout(Some(CONTROL_READ_TIMEOUT)) {
        warn!(error = %e, "control: could not set request timeout");
        return;
    }
    if let Err(e) = stream.set_write_timeout(Some(CONTROL_WRITE_TIMEOUT)) {
        warn!(error = %e, "control: could not set response timeout");
        return;
    }
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "control: clone stream failed");
            return;
        }
    });
    let line = match read_request_line(&mut reader) {
        Ok(Some(line)) if !line.trim().is_empty() => line,
        Ok(_) => return,
        Err(e) => {
            warn!(error = %e, "control: rejected request");
            return;
        }
    };
    let request = serde_json::from_str::<CtlRequest>(line.trim());
    // A subscription lasts as long as its client, so it is not a request in
    // flight and stays out of the diagnostics' list of them.
    if let Ok(CtlRequest::Subscribe) = request {
        serve_subscription(core, username, mountpoint, stream);
        return;
    }
    let touched = request.as_ref().map_or(&[][..], touched_topics);
    // Registered before the request is even parsed, so a handler that wedges
    // inside the daemon shows up by name in `pdfs diagnostics`.
    let _inflight = super::diagnostics::InflightGuard::new(super::diagnostics::request_kind(&line));
    if let Ok(CtlRequest::PhotoMonths { kind }) = &request {
        let started = std::time::Instant::now();
        let visibility = core.cached_gallery_visibility();
        let visibility_ms = started.elapsed().as_millis() as u64;
        let mut database_ms = None;
        let mut response_started = std::time::Instant::now();
        let response = match visibility {
            Ok(visibility) => {
                let excluded = serde_json::to_string(&visibility.media)
                    .expect("a set of file identifiers serializes");
                let query = std::time::Instant::now();
                let months = core.db.photos_months_visible(*kind, &excluded);
                response_started = std::time::Instant::now();
                database_ms = Some(response_started.duration_since(query).as_millis() as u64);
                match months {
                    Ok(months) => CtlResponse::PhotoMonths {
                        months: months
                            .into_iter()
                            .map(|(year, month, count)| PhotoMonth { year, month, count })
                            .collect(),
                    },
                    Err(error) => CtlResponse::error(error.into()),
                }
            }
            Err(error) => CtlResponse::error(error),
        };
        write_response(stream, response);
        tracing::info!(
            visibility_ms,
            database_ms,
            response_ms = response_started.elapsed().as_millis() as u64,
            total_ms = started.elapsed().as_millis() as u64,
            "PhotoMonths timings"
        );
        return;
    }
    let visibility = if matches!(
        &request,
        Ok(CtlRequest::PhotosTimeline { .. }
            | CtlRequest::PhotoDuplicates
            | CtlRequest::PhotoSimilar
            | CtlRequest::PhotoPlaces
            | CtlRequest::PlacePhotos { .. }
            | CtlRequest::PlaceLocations { .. }
            | CtlRequest::PhotosOnThisDay { .. }
            | CtlRequest::PhotoGroup { .. }
            | CtlRequest::PhotoAlbums
            | CtlRequest::AlbumPhotos { .. }
            | CtlRequest::PhotoThumbs { .. }
            | CtlRequest::OpenGalleryImage { private: false, .. }
            | CtlRequest::OpenPhoto { .. })
    ) {
        core.gallery_visibility()
    } else {
        Ok(std::sync::Arc::default())
    };
    let visibility = match visibility {
        Ok(visibility) => visibility,
        Err(error) => return write_response(stream, CtlResponse::error(error)),
    };
    let excluded =
        serde_json::to_string(&visibility.media).expect("a set of file identifiers serializes");
    let ask = Ask {
        core,
        mountpoint,
        stream: &stream,
        visibility: &visibility,
        excluded: &excluded,
    };
    let response = match request {
        Ok(request @ (share::request!())) => share::answer(&ask, request),
        Ok(request @ (photos::request!())) => photos::answer(&ask, request),
        Ok(request @ (sync::request!())) => sync::answer(&ask, request),
        Ok(request @ (revisions::request!())) => revisions::answer(&ask, request),
        Ok(request @ (trash::request!())) => trash::answer(&ask, request),
        Ok(CtlRequest::Status) => status_response(core, username, mountpoint),
        Ok(CtlRequest::Subscribe) => {
            CtlResponse::error(CoreError::invalid("a subscription is served on its own"))
        }
        Ok(CtlRequest::Pin { path }) => match route_keep(core, mountpoint, &path) {
            Ok((routed, rel)) => match routed.pin(&rel, &pin_record(&routed, &rel, &path)) {
                Ok(name) => {
                    core.profile().touch_profile();
                    CtlResponse::Ok {
                        message: format!("pinned {name}"),
                    }
                }
                Err(e) => CtlResponse::error(e),
            },
            Err(e) => CtlResponse::error(e),
        },
        Ok(CtlRequest::Unpin { path }) => {
            let unpinned = route_keep(core, mountpoint, &path)
                .and_then(|(routed, rel)| routed.unpin(&rel))
                .or_else(|e| match e.kind {
                    // The node is gone, and its pin with it unless the pin
                    // was left behind; drop that one by the path it is listed
                    // under.
                    ErrorKind::NotFound => match core.unpin_stale(&path) {
                        Ok(true) => Ok(Path::new(&path)
                            .file_name()
                            .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned())),
                        Ok(false) => Err(e),
                        Err(stale) => Err(stale),
                    },
                    _ => Err(e),
                });
            match unpinned {
                Ok(name) => {
                    core.profile().touch_profile();
                    CtlResponse::Ok {
                        message: format!("unpinned {name}"),
                    }
                }
                Err(e) => CtlResponse::error(e),
            }
        }
        Ok(CtlRequest::ListPins) => CtlResponse::Pins {
            pins: core.list_pins(),
        },
        Ok(CtlRequest::ListDir { path }) => match route_to_mount(core, mountpoint, &path) {
            Ok((core, rel)) => match core.list_dir(&rel) {
                Ok(entries) => CtlResponse::Entries { entries },
                Err(e) => CtlResponse::error(e),
            },
            Err(e) => CtlResponse::error(e),
        },
        Ok(CtlRequest::Refresh { scope }) => match RefreshGuard::acquire(&scope) {
            // The same scope is already being refreshed by another handler.
            // Running it again cannot make the answer newer, and the duplicate
            // would spend its whole run contending for the state and database
            // locks the first one holds — which is how five stacked photo
            // refreshes sat in flight for twenty-three minutes at mount time.
            None => {
                debug!(scope = %scope_key(&scope), "a refresh of this scope is already running");
                CtlResponse::Ok {
                    message: "refreshing".to_string(),
                }
            }
            Some(_refreshing) => {
                let result = match &scope {
                    RefreshScope::Dir { path } => route_to_mount(core, mountpoint, path)
                        .and_then(|(core, rel)| core.refresh_dir(&rel)),
                    RefreshScope::Trash => {
                        core.invalidate_trash();
                        Ok(())
                    }
                    RefreshScope::Photos { full }
                        if !full && core.photos().timeline_refreshing() =>
                    {
                        // A refresh is already running and will publish exactly the
                        // timeline this request asks for. Doing the work again
                        // cannot make it newer, and the invalidation plus the
                        // album read contend with the running refresh for the
                        // database lock — which is how a handful of stacked photo
                        // refreshes at mount time left five control requests
                        // in flight for twenty-three minutes.
                        debug!("a photos refresh is already running; joining it");
                        Ok(())
                    }
                    RefreshScope::Photos { full } => {
                        core.invalidate_photos();
                        // A full refresh is "forget what you know", not a second code
                        // path: the ordinary refresh then finds every photo stale.
                        if let Some(Err(e)) = full.then(|| core.db.photos_mark_all_unresolved()) {
                            return write_response(stream, CtlResponse::error(e.into()));
                        }
                        // Whatever changed the timeline can just as easily have
                        // changed an album; one scope covers the whole photos view.
                        core.photos().invalidate_albums();
                        // The refresh is started here, not awaited. A library of
                        // twelve thousand photos is re-read in chunks of
                        // `TIMELINE_ENRICH_CHUNK` nodes, which takes minutes —
                        // longer than the client's read bound, so awaiting it
                        // returned `Resource temporarily unavailable` to a caller
                        // whose refresh was in fact running fine (B92). The front
                        // end follows it with `PhotosRefreshStatus`.
                        core.photos().spawn_timeline_refresh();
                        Ok(())
                    }
                };
                match result {
                    Ok(()) => CtlResponse::Ok {
                        message: match scope {
                            RefreshScope::Photos { .. } => "refreshing".to_string(),
                            _ => "refreshed".to_string(),
                        },
                    },
                    Err(e) => CtlResponse::error(e),
                }
            }
        },
        Ok(CtlRequest::TrashNodes { uids, files_only }) => match Core::parse_uids(&uids) {
            Ok(parsed) => match core.trash_photos(&parsed, files_only) {
                Ok((trashed, failed)) => CtlResponse::Trashed {
                    trashed,
                    failed: failed
                        .into_iter()
                        .map(|(uid, message)| TrashFailure { uid, message })
                        .collect(),
                },
                Err(e) => CtlResponse::error(e),
            },
            Err(e) => CtlResponse::error(e),
        },
        // The scan alone walks every archive's central directory, so even the
        // dry run goes to a background thread and is read back through
        // `ImportStatus` rather than answered inline.
        // The scan reads the whole persisted timeline and every candidate is a
        // download plus an upload, so this goes to a background thread and is
        // read back through `RedateStatus`.
        // A uid addresses the node wherever it lives; the path only works for
        // the primary mount's tree. Prefer the uid when the client sent one.
        Ok(CtlRequest::OpenFile { path, uid }) => {
            let opened = match uid.as_deref() {
                Some(uid) => core.open_file_uid(uid),
                None => rel_to_mount(mountpoint, &path).and_then(|rel| core.open_file(&rel)),
            };
            match opened {
                Ok(p) => CtlResponse::FilePath {
                    path: p.display().to_string(),
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        Ok(CtlRequest::Search {
            query,
            limit,
            scope,
        }) => match core
            .searcher()
            .search(&query, clamp_limit(limit), scope.as_deref())
        {
            Ok(hits) => CtlResponse::SearchResults { hits },
            Err(e) => CtlResponse::error(e),
        },
        Ok(CtlRequest::SearchLocal { query, limit }) => {
            match core.searcher().search_local(&query, clamp_limit(limit)) {
                Ok(hits) => CtlResponse::LocalResults {
                    hits,
                    indexing: core.searcher().indexing(),
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        Ok(CtlRequest::SearchV2 {
            query,
            limit,
            filters,
        }) => match core
            .searcher()
            .search_v2(&query, clamp_limit(limit), &filters)
        {
            Ok((drive_hits, local_hits)) => CtlResponse::SearchResultsV2 {
                drive_hits,
                local_hits,
                local_indexing: core.searcher().indexing(),
            },
            Err(e) => CtlResponse::error(e),
        },
        Ok(CtlRequest::Rename { path, new_name }) => match route_node(core, mountpoint, &path) {
            Ok((core, rel)) => match core.rename(&rel, &new_name) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Rename, &name, format!("was {path}"), true);
                    CtlResponse::Ok {
                        message: format!("renamed to {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Rename, &path, &e, false);
                    CtlResponse::error(e)
                }
            },
            Err(e) => CtlResponse::error(e),
        },
        // Two absolute paths may name two different locations, which only
        // `move_between` can resolve; a relative pair keeps the old meaning.
        Ok(CtlRequest::Move { path, new_parent })
            if Path::new(&path).is_absolute() && Path::new(&new_parent).is_absolute() =>
        {
            match core.move_between(Path::new(&path), Path::new(&new_parent)) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Move, &name, format!("to {new_parent}"), true);
                    CtlResponse::Ok {
                        message: format!("moved {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Move, &path, &e, false);
                    CtlResponse::error(e)
                }
            }
        }
        Ok(CtlRequest::Move { path, new_parent }) => {
            match (
                rel_to_mount(mountpoint, &path),
                rel_to_mount(mountpoint, &new_parent),
            ) {
                (Ok(rel), Ok(parent_rel)) => match core.move_to(&rel, &parent_rel) {
                    Ok(name) => {
                        let dest = if new_parent.is_empty() {
                            "My files".to_string()
                        } else {
                            new_parent.clone()
                        };
                        core.log_activity(ActivityKind::Move, &name, format!("to {dest}"), true);
                        CtlResponse::Ok {
                            message: format!("moved {name}"),
                        }
                    }
                    Err(e) => {
                        core.log_activity(ActivityKind::Move, &path, &e, false);
                        CtlResponse::error(e)
                    }
                },
                (Err(e), _) | (_, Err(e)) => CtlResponse::error(e),
            }
        }
        Ok(CtlRequest::Copy { path, new_parent }) => {
            match (
                rel_to_mount(mountpoint, &path),
                rel_to_mount(mountpoint, &new_parent),
            ) {
                (Ok(rel), Ok(parent_rel)) => match core.copy_to(&rel, &parent_rel) {
                    Ok(name) => {
                        let dest = if new_parent.is_empty() {
                            "My files".to_string()
                        } else {
                            new_parent.clone()
                        };
                        core.log_activity(ActivityKind::Copy, &name, format!("to {dest}"), true);
                        CtlResponse::Ok {
                            message: format!("copied to {name}"),
                        }
                    }
                    Err(e) => {
                        core.log_activity(ActivityKind::Copy, &path, &e, false);
                        CtlResponse::error(e)
                    }
                },
                (Err(e), _) | (_, Err(e)) => CtlResponse::error(e),
            }
        }
        Ok(CtlRequest::FolderSize { path }) => match rel_to_mount(mountpoint, &path) {
            Ok(rel) => folder_size_reply(core.folder_size(&rel)),
            Err(e) => CtlResponse::error(e),
        },
        Ok(CtlRequest::FolderSizeByUid { uid }) => folder_size_reply(core.folder_size_by_uid(&uid)),
        Ok(CtlRequest::Delete { path }) => match route_node(core, mountpoint, &path) {
            Ok((core, rel)) => match core.delete(&rel) {
                Ok(name) => {
                    core.log_activity(ActivityKind::Trash, &name, "", true);
                    CtlResponse::Ok {
                        message: format!("trashed {name}"),
                    }
                }
                Err(e) => {
                    core.log_activity(ActivityKind::Trash, &path, &e, false);
                    CtlResponse::error(e)
                }
            },
            Err(e) => CtlResponse::error(e),
        },
        Ok(CtlRequest::CreateFolder { parent, name }) => {
            match route_change(core, mountpoint, &parent) {
                Ok((core, parent_rel)) => match core.create_folder(&parent_rel, &name) {
                    Ok(name) => {
                        core.log_activity(ActivityKind::CreateFolder, &name, "", true);
                        CtlResponse::Ok {
                            message: format!("created folder {name}"),
                        }
                    }
                    Err(e) => {
                        core.log_activity(ActivityKind::CreateFolder, &name, &e, false);
                        CtlResponse::error(e)
                    }
                },
                Err(e) => CtlResponse::error(e),
            }
        }
        Ok(CtlRequest::UploadPaths { parent, sources }) => {
            match rel_to_mount(mountpoint, &parent) {
                // Ack immediately and upload on a background thread: a directory tree
                // far outlasts the socket read timeout. Progress and completion are
                // observed through GetQueueStatus; the activity log gets the summary
                // when the whole batch finishes.
                Ok(parent_rel) => {
                    let core = core.clone();
                    let paths: Vec<PathBuf> = sources.into_iter().map(PathBuf::from).collect();
                    let n = paths.len();
                    if let Err(error) = core.services.clone().spawn_job("pdfs-upload", move || {
                        let started = Instant::now();
                        match core.upload_paths(&parent_rel, &paths) {
                            Ok(stats) => {
                                // e.g. "Uploaded 700 files to Photos" — name the destination
                                // so the log reads like a sentence rather than a bare count.
                                let dest = parent_rel
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("your Drive");
                                let target = format!(
                                    "{} to {dest}",
                                    count_noun(stats.uploaded, "file", "files")
                                );
                                // Size · folders · elapsed, with a trailing failure count
                                // when some files didn't make it.
                                let mut parts = vec![human_bytes(stats.bytes)];
                                if stats.folders > 0 {
                                    parts.push(count_noun(stats.folders, "folder", "folders"));
                                }
                                parts.push(human_duration(started.elapsed()));
                                if stats.failed > 0 {
                                    parts.push(format!("{} failed", stats.failed));
                                }
                                core.log_activity(
                                    ActivityKind::Upload,
                                    target,
                                    parts.join(" · "),
                                    stats.failed == 0,
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "bulk upload failed");
                                core.log_activity(ActivityKind::Upload, "bulk upload", &e, false);
                            }
                        }
                    }) {
                        tracing::warn!(%error, "could not start pdfs-upload");
                    }
                    CtlResponse::Ok {
                        message: format!("uploading {n} item(s)"),
                    }
                }
                Err(e) => CtlResponse::error(e),
            }
        }
        Ok(CtlRequest::Diagnostics) => CtlResponse::Diagnostics {
            diagnostics: core.diagnostics(),
        },
        // Ack and vacuum on a thread. `VACUUM` rewrites the whole database
        // under the connection mutex — minutes on a large one — and the
        // request's own socket timeout is 10 s, so running it inline both froze
        // the mount and lost its own answer. Progress shows up as a job in
        // `GetQueueStatus`; the outcome goes to the log.
        Ok(CtlRequest::GetQueueStatus) => CtlResponse::Transfers {
            items: core.transfers.snapshot(),
            jobs: core.jobs_snapshot(),
        },
        Ok(CtlRequest::SetBandwidthLimits { upload, download }) => {
            core.transfers.set_limits(upload, download);
            // Persisted like the cache budget: best-effort, the live caps are
            // already applied.
            match AppDirs::new().map(|dirs| {
                dirs.update_config(|cfg| {
                    cfg.upload_limit = Some(upload);
                    cfg.download_limit = Some(download);
                })
            }) {
                Ok(Ok(())) => CtlResponse::Ok {
                    message: format!(
                        "bandwidth limits set to {upload} B/s up, {download} B/s down"
                    ),
                },
                Ok(Err(e)) => CtlResponse::error(CoreError::internal(format!(
                    "limits applied but config write failed: {e}"
                ))),
                Err(e) => CtlResponse::error(CoreError::internal(format!(
                    "limits applied but config unavailable: {e}"
                ))),
            }
        }
        Ok(CtlRequest::AccountQuota) => match core.account_quota() {
            Ok((max_space, used_space)) => CtlResponse::AccountQuota {
                max_space,
                used_space,
            },
            Err(e) => CtlResponse::error(e),
        },
        Ok(CtlRequest::ListActivity { limit }) => CtlResponse::Activity {
            items: core.list_activity(clamp_limit(limit)),
        },
        // The request did not parse: the caller sent something malformed.
        Err(e) => CtlResponse::error(CoreError::invalid(format!("bad request: {e}"))),
    };
    let response = filter_gallery_response(response, &visibility);
    if !touched.is_empty() && !matches!(response, CtlResponse::Error { .. }) {
        core.events.publish(touched);
    }
    write_response(stream, response);
}

fn write_response(mut stream: UnixStream, response: CtlResponse) {
    let mut out = match serde_json::to_vec(&response) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "control: serialize response failed");
            return;
        }
    };
    out.push(b'\n');
    let _ = stream.write_all(&out);
}

fn filter_gallery_response(
    mut response: CtlResponse,
    visibility: &super::gallery::Visibility,
) -> CtlResponse {
    let visible = |item: &pdfs_core::control::PhotoItem| !visibility.media.contains(&item.uid);
    match &mut response {
        CtlResponse::Photos { items, .. } => items.retain(visible),
        CtlResponse::Albums { items, .. } => {
            items.retain(|item| !visibility.albums.contains(&item.uid));
            for album in items {
                if album
                    .cover_uid
                    .as_ref()
                    .is_some_and(|uid| visibility.media.contains(uid))
                {
                    album.cover_uid = None;
                }
            }
        }
        CtlResponse::PhotoDuplicates { sets } | CtlResponse::PhotoSimilar { sets, .. } => {
            for set in sets.iter_mut() {
                set.retain(visible);
            }
            sets.retain(|set| set.len() > 1);
        }
        CtlResponse::PhotoLocations { items } => items.retain(|located| visible(&located.photo)),
        _ => {}
    }
    response
}

/// Listen on the control socket, serving one command per connection, each on its
/// own thread. Runs on its own thread; returns only if the listener itself fails.
///
/// Concurrent rather than serial because requests differ wildly in cost: an
/// `OpenPhoto` downloads a whole photo, and while it ran the accept loop used to
/// stall every other caller behind it — the GUI's 2s status poll, and the
/// thumbnail batches the gallery needs to paint. [`Core`] is a bundle of handles
/// (`Arc`/`Clone`), so each connection gets its own copy of it.
pub(crate) fn run_control_socket(
    core: Core,
    username: String,
    mountpoint: PathBuf,
    listener: UnixListener,
) {
    info!("control socket listening");
    for conn in listener.incoming() {
        // Checked on every accept, including the deliberate poke teardown sends
        // to wake this thread out of a blocking accept (`docs/BUGS.md` B44).
        if core.shutdown.is_stopping() {
            info!("control socket closing");
            return;
        }
        match conn {
            Ok(stream) => {
                let Some(permit) = ControlHandlerPermit::acquire() else {
                    warn!(
                        limit = MAX_CONTROL_HANDLERS,
                        "control: rejecting connection at handler limit"
                    );
                    continue;
                };
                let core = core.clone();
                let username = username.clone();
                let mountpoint = mountpoint.clone();
                // A job, so a stop joins it; a subscription ends on the
                // stop's wake (`EventHub::wake_all`).
                let board = core.services.clone();
                if let Err(e) = board.spawn_job("pdfs-control-conn", move || {
                    let _permit = permit;
                    handle_control_conn(&core, &username, &mountpoint, stream);
                }) {
                    warn!(error = %e, "control: spawn handler failed");
                }
            }
            Err(e) => {
                warn!(error = %e, "control: accept failed");
            }
        }
    }
}

/// Check that every archive is there, then start the import in the background.
fn start_takeout_import(core: &Core, paths: Vec<PathBuf>, dry_run: bool) -> CtlResponse {
    let missing: Vec<String> = paths
        .iter()
        .filter(|path| !path.is_file())
        .map(|path| path.display().to_string())
        .collect();
    if !missing.is_empty() {
        return CtlResponse::error(CoreError::not_found(format!(
            "cannot read: {}",
            missing.join(", ")
        )));
    }
    let count = paths.len();
    match core.photos().spawn_takeout_import(paths, dry_run) {
        Ok(()) => CtlResponse::Ok {
            message: format!(
                "{} {count} archive(s)",
                if dry_run { "scanning" } else { "importing" }
            ),
        },
        Err(e) => CtlResponse::error(e),
    }
}

#[cfg(test)]
mod request_limit_tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn accepts_one_bounded_newline_terminated_request() {
        let mut input = Cursor::new(b"{\"Status\":null}\n".to_vec());
        assert_eq!(
            read_request_line(&mut input).unwrap().as_deref(),
            Some("{\"Status\":null}\n")
        );
    }

    #[test]
    fn rejects_a_request_without_a_terminating_newline() {
        let mut input = Cursor::new(b"{\"Status\":null}".to_vec());
        assert_eq!(
            read_request_line(&mut input).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_a_request_larger_than_the_limit() {
        let mut bytes = vec![b'x'; MAX_CONTROL_REQUEST_BYTES as usize + 1];
        bytes.push(b'\n');
        let mut input = Cursor::new(bytes);
        assert_eq!(
            read_request_line(&mut input).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn accepts_paths_inside_the_mountpoint() {
        let mountpoint = Path::new("/mnt/proton");
        assert_eq!(
            rel_to_mount(mountpoint, "Photos/2026").unwrap(),
            Path::new("Photos/2026")
        );
        assert_eq!(
            rel_to_mount(mountpoint, "/mnt/proton/Photos/2026").unwrap(),
            Path::new("Photos/2026")
        );
        assert_eq!(
            rel_to_mount(mountpoint, "/mnt/proton").unwrap(),
            Path::new("")
        );
    }

    #[test]
    fn rejects_paths_that_escape_the_mountpoint() {
        let mountpoint = Path::new("/mnt/proton");
        assert!(rel_to_mount(mountpoint, "../escape").is_err());
        assert!(rel_to_mount(mountpoint, "Photos/../../escape").is_err());
        assert!(rel_to_mount(mountpoint, "/mnt/elsewhere").is_err());
    }

    /// `pdfs rm` and `pdfs rename` reach on-demand mounts, whose root is a whole
    /// synced folder with a parent on Drive. Only a path below a root may get
    /// through to the trash or rename call.
    #[test]
    fn only_a_path_below_a_mount_root_names_a_node() {
        assert!(below_root(Path::new("Photos")));
        assert!(below_root(Path::new("./Photos/2026")));
        assert!(!below_root(Path::new("")));
        assert!(!below_root(Path::new(".")));
        assert!(!below_root(Path::new("./.")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two refreshes of the same scope are one piece of work; two refreshes of
    /// different scopes are not.
    #[test]
    fn a_scope_refreshes_once_at_a_time() {
        let photos = RefreshScope::Photos { full: false };
        let held = RefreshGuard::acquire(&photos).expect("first refresh runs");
        assert!(RefreshGuard::acquire(&photos).is_none());
        // A full refresh forgets what the daemon knows, so it is other work.
        assert!(RefreshGuard::acquire(&RefreshScope::Photos { full: true }).is_some());
        assert!(
            RefreshGuard::acquire(&RefreshScope::Dir {
                path: "a".to_string()
            })
            .is_some()
        );

        drop(held);
        assert!(RefreshGuard::acquire(&photos).is_some());
    }
}
