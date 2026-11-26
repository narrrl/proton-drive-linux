//! `pdfs` — command-line front-end for the Proton Drive Linux client.

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueHint};
use clap_complete::engine::{ArgValueCandidates, ArgValueCompleter};
use pdfs_core::auth;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    ConflictKeep, ErrorKind, LinkChange, MountKind, RefreshScope, Request as CtlRequest,
    Response as CtlResponse, RestoreItem as CtlRestoreItem, ShareEntryKind, SyncFolderInfo,
    SyncIssue, SyncPhase, daemon_outdated, pending_summary,
};
use pdfs_core::service;

mod account;
mod complete;
mod diagnose;
mod files;
mod journal;
mod photos;
mod sharing;
mod status;
mod sync;

use account::*;
use diagnose::*;
use files::*;
use photos::*;
use sharing::*;
use status::*;
use sync::*;

/// `println!` for this binary, shadowing std's, which panics when stdout is
/// closed: `pdfs ls | head` ended in a "Broken pipe" panic once `head` had its
/// lines. This one hands the failure to [`stdout_written`] instead.
///
/// Restoring the default `SIGPIPE` would also stop the panic, but the same
/// binary runs the daemon, which must not die when a socket's peer goes away.
macro_rules! println {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        $crate::stdout_written(writeln!(std::io::stdout(), $($arg)*))
    }};
}

/// [`println!`] without the newline.
macro_rules! print {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        $crate::stdout_written(write!(std::io::stdout(), $($arg)*))
    }};
}

/// Exit on a failed write to stdout. A closed pipe means the reader has all it
/// wanted, so that exit is a success.
fn stdout_written(result: std::io::Result<()>) {
    if let Err(error) = result {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("Error: writing to stdout: {error}");
        std::process::exit(1);
    }
}

#[derive(Parser)]
#[command(
    name = "pdfs",
    version,
    about = "Proton Drive for Linux (Files On-Demand)"
)]
struct Cli {
    /// Emit machine-readable JSON instead of formatted text.
    ///
    /// Applies to the query commands (`status`, `ls`, `pins`, `sync list`,
    /// `devices list`, `locations`, `transfers`, `activity`, `trash`, `cache inspect`). Commands that
    /// perform an action keep their human output — a script that needs to know
    /// whether one succeeded has the exit code.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

/// Whether `--json` was passed. A global rather than a threaded parameter
/// because it is read at the leaves — one line per query command — and
/// threading a display flag through every command signature would cost more
/// than it explains.
static JSON_OUTPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn json_enabled() -> bool {
    JSON_OUTPUT.load(std::sync::atomic::Ordering::Relaxed)
}

/// The daemon's reply as JSON, with the response-variant tag stripped.
///
/// Serializing [`CtlResponse`] directly yields serde's externally tagged form,
/// `{"SyncFolders": {"items": […]}}`, which makes every script write
/// `jq '.SyncFolders.items[]'` and couples it to the name of an internal enum
/// variant. The payload is what the caller asked for, so that is what is
/// emitted: `{"items": […]}`.
///
/// The reply is still serialized from the daemon's own type rather than
/// rebuilt by hand, so new fields appear in the JSON without anyone
/// remembering to add them here.
fn response_payload(response: &CtlResponse) -> Result<serde_json::Value> {
    let value = serde_json::to_value(response)?;
    // Every variant serializes as a one-key object keyed by the variant name;
    // anything else is left alone rather than guessed at.
    if let serde_json::Value::Object(map) = &value
        && map.len() == 1
        && let Some((_, payload)) = map.iter().next()
    {
        return Ok(payload.clone());
    }
    Ok(value)
}

/// Print the daemon's reply as JSON if `--json` is in effect, reporting whether
/// it did so — a `true` means the caller should skip its human formatting.
///
/// A daemon-side error still prints its payload, so a script can read the
/// `kind`, but then fails the command. Emitting the error and exiting `0` would
/// make every `--json` caller inspect the body to find out whether it worked,
/// when the exit code is the thing they already check.
fn emit_json(response: &CtlResponse) -> Result<bool> {
    if !json_enabled() {
        return Ok(false);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&response_payload(response)?)?
    );
    if let CtlResponse::Error { message, kind } = response {
        bail!("{}", cli_error(*kind, message));
    }
    Ok(true)
}

#[derive(Subcommand)]
enum Command {
    /// Log in to a Proton account (SRP + optional 2FA) and store the session.
    Login {
        /// Account email; prompted if omitted.
        #[arg(short, long)]
        username: Option<String>,
        /// For a two-password account, keep the mailbox password's unlock in the
        /// system keyring so the daemon starts without asking after every login.
        /// Without it the unlock lives in kernel memory for this session only.
        #[arg(long)]
        remember: bool,
    },
    /// Enter the mailbox password of a two-password account so the daemon can
    /// unlock your files. Needed once per login session.
    Unlock {
        /// Keep the unlock in the system keyring instead of kernel memory only.
        #[arg(long)]
        remember: bool,
    },
    /// Forget the unlock held in kernel memory; the account is locked again.
    Lock {
        /// Also remove an unlock kept in the keyring with `--remember`.
        #[arg(long)]
        forget: bool,
    },
    /// Forget the stored session. Asks first while changes are still queued.
    Logout {
        /// Do not ask first.
        #[arg(long)]
        yes: bool,
    },
    /// Show account login state, and mount status if a daemon is running.
    Status,
    /// List every local Proton Drive location.
    Locations,
    /// Mount Proton Drive at the given (or default) path. Blocks until unmounted.
    Mount {
        /// Mountpoint; defaults to ~/ProtonDrive.
        #[arg(value_hint = ValueHint::DirPath)]
        mountpoint: Option<PathBuf>,
    },
    /// Run the auto-mount daemon: wait for login, mount, and keep it mounted
    /// until stopped. This is what the systemd user service runs; you normally
    /// don't invoke it by hand.
    Daemon {
        /// Mountpoint; defaults to ~/ProtonDrive.
        #[arg(value_hint = ValueHint::DirPath)]
        mountpoint: Option<PathBuf>,
    },
    /// Keep a file or folder on this device (download + cache it). A folder's
    /// files download in the background; `pdfs transfers` shows the progress.
    /// Needs a running mount.
    Pin {
        /// File or folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
    },
    /// Stop keeping a file or folder on this device, evicting its cached content.
    Unpin {
        /// File or folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
    },
    /// List files and folders pinned to this device.
    Pins,
    /// List a directory via the running daemon (in-app browser backend).
    Ls {
        /// Directory path, inside the mountpoint or relative to it. Root if omitted.
        #[arg(add = ArgValueCompleter::new(complete::drive_dir))]
        path: Option<PathBuf>,
    },
    /// List the photos timeline (newest first) via the running daemon.
    Photos {
        /// Max photos to list.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Skip this many photos from the start of the timeline.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// List only favorited photos.
        #[arg(long)]
        favorites: bool,
    },
    /// Add or remove a photo's favorite mark.
    Favorite {
        /// Photo node uid in `volume~link` form (from `pdfs photos`).
        uid: String,
        /// Remove the mark instead of adding it.
        #[arg(long)]
        remove: bool,
    },
    /// List the photo albums (including ones shared with this account).
    Albums,
    /// Import a Google Photos Takeout export into Proton Photos.
    ///
    /// Pass every zip of the export at once — Google splits one export across
    /// numbered parts, and a photo's metadata often sits in a different part
    /// than the photo. Albums in the export are recreated as Proton albums, and
    /// photos already on the account (same name and same content) are skipped,
    /// so an interrupted import can simply be run again, or resumed with
    /// `--resume` without naming the archives again.
    ImportGooglePhotos {
        /// The export's `.zip` files (e.g. `takeout-*.zip`).
        #[arg(required_unless_present = "resume", value_hint = ValueHint::FilePath)]
        archives: Vec<PathBuf>,
        /// Report what would be imported without uploading anything.
        #[arg(long, conflicts_with = "resume")]
        dry_run: bool,
        /// Start the last import again with the same archives, after it was
        /// interrupted, stopped or failed.
        #[arg(long, conflicts_with = "archives")]
        resume: bool,
        /// Stay attached and print progress until the import finishes.
        #[arg(long)]
        wait: bool,
    },
    /// Fix photos filed under the day they were imported instead of the day
    /// they were taken.
    ///
    /// A Takeout whose metadata sidecars did not match imports with no capture
    /// time, and Proton then stamps every photo with the moment it arrived. The
    /// date is still in the file name (`IMG-20230219-WA0001.jpg`), so this finds
    /// photos whose name disagrees with their capture time by more than a day
    /// and rewrites them at the right time.
    ///
    /// Proton cannot edit a sealed capture time, so a rewrite means re-uploading
    /// the photo and trashing the original — favorites and albums are carried
    /// across, and the original goes to Proton's trash, not away. Run it with
    /// `--dry-run` first.
    RedatePhotos {
        /// Report what would be re-dated without uploading or trashing anything.
        #[arg(long)]
        dry_run: bool,
        /// Stop after this many photos.
        #[arg(long)]
        limit: Option<usize>,
        /// Only touch photos whose current capture time is on or after this
        /// date (`YYYY-MM-DD`, UTC) — aim the repair at the days an import
        /// landed on.
        #[arg(long)]
        from: Option<String>,
        /// Only touch photos whose current capture time is before this date
        /// (`YYYY-MM-DD`, UTC).
        #[arg(long)]
        to: Option<String>,
        /// Stay attached and print the report when the run finishes.
        #[arg(long)]
        wait: bool,
    },
    /// Show how the running (or last) photo re-date is doing.
    RedateStatus,
    /// Stop the running photo re-date.
    CancelRedate,
    /// Show how the running (or last) Google Photos import is doing.
    ImportStatus,
    /// Stop the running Google Photos import.
    CancelImport,
    /// List the photos in one album (newest capture first).
    Album {
        /// Album node uid in `volume~link` form (from `pdfs albums`).
        #[arg(add = ArgValueCandidates::new(complete::album_uid))]
        uid: String,
        /// Max photos to list.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Skip this many photos from the start of the album.
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Create a photo album and print its uid.
    NewAlbum {
        /// Album name.
        name: String,
    },
    /// Rename a photo album.
    RenameAlbum {
        /// Album node uid in `volume~link` form (from `pdfs albums`).
        #[arg(add = ArgValueCandidates::new(complete::album_uid))]
        uid: String,
        /// New name.
        name: String,
    },
    /// Delete a photo album. Its photos stay in the timeline.
    DeleteAlbum {
        /// Album node uid in `volume~link` form (from `pdfs albums`).
        #[arg(add = ArgValueCandidates::new(complete::album_uid))]
        uid: String,
        /// Also delete photos that exist only in this album (saved from a
        /// shared album); without it the server refuses to delete such an album.
        #[arg(long)]
        with_photos: bool,
    },
    /// Add photos to an album.
    AddToAlbum {
        /// Album node uid in `volume~link` form (from `pdfs albums`).
        #[arg(add = ArgValueCandidates::new(complete::album_uid))]
        uid: String,
        /// Photo node uids (from `pdfs photos`).
        #[arg(required = true)]
        photos: Vec<String>,
    },
    /// Take photos out of an album. They stay in the timeline.
    RemoveFromAlbum {
        /// Album node uid in `volume~link` form (from `pdfs albums`).
        #[arg(add = ArgValueCandidates::new(complete::album_uid))]
        uid: String,
        /// Photo node uids (from `pdfs album <uid>`).
        #[arg(required = true)]
        photos: Vec<String>,
    },
    /// Download a photo by uid (`volume~link`) and print its cached path.
    OpenPhoto {
        /// Photo node uid in `volume~link` form (from `pdfs photos`).
        uid: String,
    },
    /// Search file/folder names via the running daemon's local index.
    Search {
        /// Query string (substring match).
        query: String,
        /// Max hits to return.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Only search below this folder (under the mountpoint).
        #[arg(long = "in", value_name = "FOLDER", add = ArgValueCompleter::new(complete::drive_dir))]
        scope: Option<PathBuf>,
    },
    /// Rename a file or folder via the running daemon.
    Rename {
        /// File/folder path, inside My files or an on-demand synced folder, or
        /// relative to the mountpoint.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// New name (a single path component).
        new_name: String,
    },
    /// Move files or folders into another folder via the running daemon.
    ///
    /// The paths may be in different locations: My files and any synced
    /// folder, on-demand or mirrored. The move happens on Proton Drive, so
    /// nothing is downloaded or uploaded again, unlike `mv` between two
    /// locations. The last path is the destination folder.
    Move {
        /// Files/folders to move, then the destination folder: each inside a
        /// location, or relative to the mountpoint.
        #[arg(required = true, num_args = 2.., value_name = "PATH", add = ArgValueCompleter::new(complete::drive_path))]
        paths: Vec<PathBuf>,
    },
    /// Copy files or folders into another folder in My files via the running
    /// daemon.
    ///
    /// The copy happens on Proton Drive, so nothing is downloaded or uploaded,
    /// unlike `cp` inside the mount. A name already taken in the destination
    /// gets a " (1)" suffix. The last path is the destination folder.
    Copy {
        /// Files/folders to copy, then the destination folder: inside My files,
        /// or relative to the mountpoint.
        #[arg(required = true, num_args = 2.., value_name = "PATH", add = ArgValueCompleter::new(complete::drive_path))]
        paths: Vec<PathBuf>,
    },
    /// Show how much a folder holds on Proton Drive.
    ///
    /// Proton Drive adds it up, so nothing is downloaded. The size is the
    /// encrypted bytes on storage and includes items in the trash.
    Du {
        /// Folder path, inside My files or relative to the mountpoint.
        #[arg(add = ArgValueCompleter::new(complete::drive_dir))]
        path: PathBuf,
    },
    /// Trash a file or folder via the running daemon.
    Rm {
        /// File/folder path, inside My files or an on-demand synced folder, or
        /// relative to the mountpoint.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
    },
    /// Create a folder via the running daemon.
    Mkdir {
        /// Parent folder path, inside My files or an on-demand synced folder,
        /// or relative to the mountpoint.
        #[arg(add = ArgValueCompleter::new(complete::drive_dir))]
        parent: PathBuf,
        /// New folder name (a single path component).
        name: String,
    },
    /// Upload local files and/or folders into a Drive folder via the running
    /// daemon. Folders are uploaded recursively. The daemon uploads in the
    /// background; watch progress with `pdfs transfers`.
    Upload {
        /// Local files and/or folders to upload.
        #[arg(required = true, value_hint = ValueHint::AnyPath)]
        sources: Vec<PathBuf>,
        /// Destination folder, inside the mountpoint or relative to it.
        #[arg(short = 't', long = "to", default_value = ".", add = ArgValueCompleter::new(complete::drive_dir))]
        parent: PathBuf,
    },
    /// Show the daemon's in-flight transfers (active uploads/downloads).
    Transfers,
    /// Show what the daemon is doing right now: worker threads, queue depths,
    /// in-flight control requests and memory. For a daemon that has stopped
    /// answering rather than one that is merely busy.
    Diagnostics,
    /// List what is in the account's trash, with the uids the restore and
    /// delete-forever commands take.
    Trash {
        /// Group the trash by the folder each item was deleted from.
        #[arg(long, conflicts_with = "folder")]
        by_folder: bool,
        /// List what a trashed folder holds, by its uid.
        #[arg(long, value_name = "UID")]
        folder: Option<String>,
    },
    /// Restore trashed items, by uid, to the folders they were trashed from.
    Restore {
        /// Node uids (`volume~link`), as printed by `pdfs trash`.
        #[arg(required = true, add = ArgValueCandidates::new(complete::trash_uid))]
        uids: Vec<String>,
    },
    /// Permanently delete trashed items by uid. This cannot be undone.
    DeleteForever {
        /// Node uids (`volume~link`), as printed by `pdfs trash`.
        #[arg(required = true, add = ArgValueCandidates::new(complete::trash_uid))]
        uids: Vec<String>,
    },
    /// Permanently delete everything in the trash. This cannot be undone.
    EmptyTrash,

    /// Drop a cached listing so the next read re-fetches it from the server.
    /// Use it when another client changed something and the daemon hasn't
    /// noticed yet.
    Refresh {
        /// What to refresh: a folder path (inside the mountpoint or relative to
        /// it), `trash`, or `photos`. The mount root if omitted.
        #[arg(add = ArgValueCompleter::new(complete::refresh_target))]
        target: Option<String>,
        /// With `photos`: read every photo's metadata again instead of only what
        /// changed. Minutes on a large library; for a gallery that disagrees with
        /// the account.
        #[arg(long)]
        full: bool,
    },

    /// Manage the account's registered devices.
    Devices {
        #[command(subcommand)]
        action: DeviceCmd,
    },
    /// Sync local folders to this machine's Proton Drive device.
    Sync {
        #[command(subcommand)]
        action: SyncCmd,
    },
    /// List `(sync-conflict …)` copies, or resolve one.
    Conflicts {
        #[command(subcommand)]
        action: Option<ConflictCmd>,
    },
    /// Share a file or folder with Proton and/or external email addresses.
    Share {
        /// File/folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// One or more invitee emails (Proton or external, auto-detected).
        #[arg(required = true)]
        emails: Vec<String>,
        /// Role to grant: viewer, editor or admin.
        #[arg(long, default_value = "viewer", value_parser = ["viewer", "editor", "admin"], ignore_case = true)]
        role: String,
        /// Optional message included in the invitation email.
        #[arg(long)]
        message: Option<String>,
    },
    /// List a node's members, pending invitations and public link.
    Members {
        /// File/folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
    },
    /// Change the role of a member or pending invitation on a node's share.
    ShareRole {
        /// File/folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// Entry id (membership or invitation id), from `pdfs members`.
        #[arg(add = ArgValueCompleter::new(complete::share_entry_id))]
        id: String,
        /// Entry kind: member, proton or external.
        kind: ShareKindArg,
        /// New role: viewer, editor or admin.
        #[arg(value_parser = ["viewer", "editor", "admin"], ignore_case = true)]
        role: String,
    },
    /// Stop sharing a node, or remove one member or pending invitation.
    ///
    /// With only a path, removes the public link, every invitation and every
    /// member. With an id and kind (from `pdfs members`), removes that entry.
    Unshare {
        /// File/folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// Entry id (membership or invitation id), from `pdfs members`.
        #[arg(requires = "kind", add = ArgValueCompleter::new(complete::share_entry_id))]
        id: Option<String>,
        /// Entry kind: member, proton or external.
        kind: Option<ShareKindArg>,
    },
    /// Inspect and restore a file's earlier versions.
    Versions {
        #[command(subcommand)]
        action: VersionCmd,
    },
    /// Manage a node's public link.
    PublicLink {
        #[command(subcommand)]
        action: PublicLinkCmd,
    },
    /// List the items I have shared with others (members, invites or a link).
    Shared,
    /// List nodes shared with me that I have accepted, or the children of one.
    SharedWithMe {
        /// Folder uid to list into, in `volume~link` form (from a previous run).
        /// Omit for the top level.
        #[arg(add = ArgValueCandidates::new(complete::shared_uid))]
        uid: Option<String>,
    },
    /// Download a file shared with me by its uid (from `pdfs shared-with-me`).
    SharedGet {
        /// Node uid in `volume~link` form.
        #[arg(add = ArgValueCandidates::new(complete::shared_uid))]
        uid: String,
        /// Where to write it. Omit to leave it in the cache and print the path.
        #[arg(value_hint = ValueHint::AnyPath)]
        dest: Option<PathBuf>,
    },
    /// Leave a shared node by its uid (from `pdfs shared-with-me`).
    Leave {
        /// Node uid in `volume~link` form.
        #[arg(add = ArgValueCandidates::new(complete::shared_uid))]
        uid: String,
    },
    /// Manage invitations addressed to me.
    Invitations {
        #[command(subcommand)]
        action: InvitationCmd,
    },
    /// Manage saved public-link bookmarks.
    Bookmarks {
        #[command(subcommand)]
        action: BookmarkCmd,
    },
    /// Show the daemon's recent activity, newest first.
    Activity {
        /// Maximum entries to show.
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
    },
    /// Show account storage usage (used of total, across all Proton products).
    Quota,
    /// Inspect and maintain the local metadata database and content cache.
    Cache {
        #[command(subcommand)]
        action: CacheCmd,
    },
    /// Check this installation for problems and print a report.
    ///
    /// Runs without a daemon on purpose: the state worth diagnosing is usually
    /// the state where the daemon will not start.
    Diagnose,
    /// Print the shell completion script for bash, zsh, fish, elvish or
    /// PowerShell.
    ///
    /// Besides commands and options, it completes values from the running
    /// service: paths in My files, synced-folder and queue ids, computers,
    /// trash, albums, shares and versions. Load it with, for example,
    /// `pdfs completions fish | source` or `source <(pdfs completions bash)`.
    Completions {
        /// The shell to write the script for.
        shell: complete::ShellArg,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Report database size, row counts, and cache usage.
    Inspect {
        /// Also run SQLite's integrity check. Reads every page, so it is slow
        /// on a large database — worth it when you suspect corruption.
        #[arg(long)]
        deep: bool,
    },
    /// Checkpoint the write-ahead log and compact the database.
    ///
    /// Takes a write lock for the duration and needs room for a second copy of
    /// the database while it runs.
    Vacuum,
    /// Delete cached file content, keeping pinned files.
    Clear,
}

/// Which collection a share entry lives in, for `share-role` / `unshare`.
#[derive(Clone, Copy, clap::ValueEnum)]
enum ShareKindArg {
    /// An accepted member.
    Member,
    /// A pending invitation to a Proton user.
    Proton,
    /// A pending invitation to a non-Proton email.
    External,
}

impl ShareKindArg {
    fn to_kind(self) -> ShareEntryKind {
        match self {
            ShareKindArg::Member => ShareEntryKind::Member,
            ShareKindArg::Proton => ShareEntryKind::ProtonInvite,
            ShareKindArg::External => ShareEntryKind::ExternalInvite,
        }
    }
}

/// `pdfs versions …` — a file's revision history.
#[derive(Subcommand)]
enum VersionCmd {
    /// List a file's versions, newest first.
    List {
        /// File path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
    },
    /// Make an earlier version the file's current content.
    ///
    /// The server applies this in the background, so the new content may take a
    /// moment to show up in the mount.
    Restore {
        /// File path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// Version id, from `pdfs versions list`.
        #[arg(add = ArgValueCompleter::new(complete::version_id))]
        id: String,
    },
    /// Write an earlier version to a local file, leaving the file untouched.
    Save {
        /// File path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// Version id, from `pdfs versions list`.
        #[arg(add = ArgValueCompleter::new(complete::version_id))]
        id: String,
        /// Where to write it. Must not already exist.
        #[arg(value_hint = ValueHint::AnyPath)]
        dest: PathBuf,
    },
    /// Permanently delete an earlier version. The content is unrecoverable, and
    /// the version that is currently active cannot be deleted.
    Rm {
        /// File path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// Version id, from `pdfs versions list`.
        #[arg(add = ArgValueCompleter::new(complete::version_id))]
        id: String,
    },
}

#[derive(Subcommand)]
enum DeviceCmd {
    /// List registered devices.
    List,
    /// Rename a device by uid.
    Rename {
        #[arg(add = ArgValueCandidates::new(complete::device_uid))]
        uid: String,
        name: String,
    },
    /// Delete (deregister) a device by uid.
    Rm {
        #[arg(add = ArgValueCandidates::new(complete::device_uid))]
        uid: String,
    },
    /// Adopt an existing device as this machine's, so a hostname change or a
    /// reinstall re-attaches to it instead of registering a duplicate.
    Adopt {
        /// Device uid (from `devices list`). Omit with `--clear`.
        #[arg(add = ArgValueCandidates::new(complete::device_uid))]
        uid: Option<String>,
        /// Drop the adoption and go back to matching by hostname.
        #[arg(long, conflicts_with = "uid")]
        clear: bool,
    },
    /// Sync the same folders as another computer: this machine continues that
    /// computer's device alongside it, and its folders are offered for syncing
    /// here. Both computers keep them in step.
    Join {
        /// Device uid (from `devices list`).
        #[arg(add = ArgValueCandidates::new(complete::device_uid))]
        uid: String,
        /// Skip the confirmation and accept every proposed local path.
        #[arg(long)]
        yes: bool,
    },
    /// Move another computer's folders into this computer's backup and sync
    /// them here. The other computer stays registered, without those folders.
    Migrate {
        /// Device uid (from `devices list`).
        #[arg(add = ArgValueCandidates::new(complete::device_uid))]
        uid: String,
        /// Skip the confirmation and accept every proposed local path.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ConflictCmd {
    /// Decide which side of a conflict to keep. Whatever is removed goes to Trash.
    Resolve {
        /// The conflict copy, absolute or relative to the mountpoint.
        #[arg(add = ArgValueCandidates::new(complete::conflict_path))]
        path: PathBuf,
        /// `original` trashes the copy; `copy` trashes the original and gives the
        /// copy its name; `both` renames the copy to `--name`.
        #[arg(long, value_parser = ["original", "copy", "both"])]
        keep: String,
        /// New name for the copy with `--keep both`.
        #[arg(long, required_if_eq("keep", "both"))]
        name: Option<String>,
    },
}

/// `pdfs sync ignore`: the patterns in `ignore_patterns`, for every synced
/// folder, or with `--folder` the ones in that folder's own `.pdfsignore`.
#[derive(Subcommand, Debug)]
enum IgnoreCmd {
    /// List the patterns every synced folder leaves out, or one folder's own.
    List {
        /// List the own patterns of the synced folder holding this path.
        #[arg(long, value_hint = ValueHint::DirPath)]
        folder: Option<PathBuf>,
    },
    /// Leave out files matching a gitignore-style pattern, e.g. `*.db` or `build/`.
    Add {
        /// The pattern; end it with `/` to match folders only.
        pattern: String,
        /// Add it only for the synced folder holding this path.
        #[arg(long, value_hint = ValueHint::DirPath)]
        folder: Option<PathBuf>,
    },
    /// Stop leaving out files matching a pattern.
    Remove {
        /// The pattern, as `sync ignore list` shows it.
        pattern: String,
        /// Remove it from the own patterns of the synced folder holding this path.
        #[arg(long, value_hint = ValueHint::DirPath)]
        folder: Option<PathBuf>,
    },
    /// Go back to the built-in patterns.
    Reset,
    /// Say whether a file in a synced folder is synced, and which pattern decides.
    Check {
        /// A file or folder inside a synced folder.
        #[arg(value_hint = ValueHint::AnyPath)]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Add a local folder to this machine's device and upload its contents.
    Add {
        /// Local directory to sync.
        #[arg(value_hint = ValueHint::DirPath)]
        path: PathBuf,
    },
    /// Sync a real local folder with an existing folder in My files.
    /// The first pass preserves local-only files and keeps differing versions.
    Attach {
        /// Existing local directory; it remains a real local directory.
        #[arg(value_hint = ValueHint::DirPath)]
        path: PathBuf,
        /// Existing folder in My files, relative to the cloud mount or inside it.
        #[arg(long, add = ArgValueCompleter::new(complete::drive_dir))]
        to: PathBuf,
    },
    /// List this machine's synced folders.
    List,
    /// Remove a synced folder by id.
    Rm {
        /// Synced-folder id (from `sync list`).
        #[arg(add = ArgValueCandidates::new(complete::sync_folder_id))]
        id: i64,
        /// Also delete the folder's copy in Proton Drive.
        #[arg(long)]
        delete_remote: bool,
    },
    /// Force a sync pass now (all folders, or one by id).
    Now {
        /// Optional synced-folder id; omit to reconcile every folder.
        #[arg(add = ArgValueCandidates::new(complete::sync_folder_id))]
        id: Option<i64>,
    },
    /// Forget what a synced folder synced before and compare it with Proton
    /// Drive afresh: what one side has alone is copied to the other, and a file
    /// that differs keeps both versions. Nothing is deleted.
    Reset {
        /// Synced-folder id (from `sync list`).
        #[arg(add = ArgValueCandidates::new(complete::sync_folder_id))]
        id: i64,
    },
    /// Pause syncing: queued uploads and mirror folders wait, reads keep working.
    Pause {
        /// End the pause by itself after this long, e.g. `30m`, `1h`, `2d`.
        /// Omit to pause until `pdfs sync resume`.
        #[arg(long = "for", value_parser = parse_pause_duration, conflicts_with = "folder")]
        duration: Option<u64>,
        /// Pause only this mirror folder (id from `sync list`) until resumed.
        #[arg(long, add = ArgValueCandidates::new(complete::sync_folder_id))]
        folder: Option<i64>,
    },
    /// Resume syncing after `pdfs sync pause`.
    Resume {
        /// Resume only this folder, paused with `sync pause --folder`.
        #[arg(long, add = ArgValueCandidates::new(complete::sync_folder_id))]
        folder: Option<i64>,
    },
    /// Cap upload and download speed, or show the caps with no options.
    Limit {
        /// Upload cap per second, e.g. `500K` or `2M`; `0` removes it.
        #[arg(long, value_parser = parse_rate)]
        up: Option<u64>,
        /// Download cap per second, e.g. `500K` or `2M`; `0` removes it.
        #[arg(long, value_parser = parse_rate)]
        down: Option<u64>,
    },
    /// List uploads and changes not yet on Proton Drive.
    Queue,
    /// Wait until every queued change is on Proton Drive. Exits 0 once nothing
    /// is left, 3 when what is left needs you (see `sync issues`), 4 on timeout.
    Flush {
        /// Give up after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// List queued changes Proton Drive refused or that keep failing, and what to do.
    Issues,
    /// Save the content of a queued upload (by id from `sync queue`) to a file.
    Export {
        /// Queue id.
        #[arg(add = ArgValueCandidates::new(complete::queue_id))]
        id: i64,
        /// Where to save it: a new file, or an existing folder to save it in.
        #[arg(value_hint = ValueHint::AnyPath)]
        dest: PathBuf,
    },
    /// Drop a queued change (by id from `sync queue`) and undo it here: a new file
    /// or folder goes, anything else goes back to what Proton Drive has. Its
    /// content is lost unless saved with `sync export` first.
    Discard {
        /// Queue id.
        #[arg(add = ArgValueCandidates::new(complete::queue_id))]
        id: i64,
        /// Do not ask first.
        #[arg(long)]
        yes: bool,
    },
    /// Retry a queued operation now (by id from `sync queue`), or every failed one.
    Retry {
        /// Queue id; omit to retry every failed operation.
        #[arg(add = ArgValueCandidates::new(complete::queue_id))]
        id: Option<i64>,
    },
    /// Show or change the patterns every synced folder leaves out, or check a path.
    Ignore {
        #[command(subcommand)]
        action: IgnoreCmd,
    },
    /// Switch a folder between full-copy sync and on-demand (FUSE, no local storage).
    Mode {
        /// Synced-folder id (from `sync list`).
        #[arg(add = ArgValueCandidates::new(complete::sync_folder_id))]
        id: i64,
        /// `mirror` (full local copy) or `ondemand` (FUSE mount, reclaims disk).
        #[arg(value_parser = ["mirror", "ondemand"])]
        mode: String,
    },
    /// Re-attach this machine's device folders to local directories, then sync
    /// them down. Use after adopting a device on a new machine.
    Restore {
        /// Accept every proposed local path without asking.
        #[arg(long)]
        yes: bool,
        /// Restore another computer's folders instead, by device uid (from
        /// `pdfs devices list`).
        #[arg(long, add = ArgValueCandidates::new(complete::device_uid))]
        device: Option<String>,
    },
}

#[derive(Subcommand)]
enum PublicLinkCmd {
    /// Create a public link on a node.
    Create {
        /// File/folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// Role: viewer or editor.
        #[arg(long, default_value = "viewer", value_parser = ["viewer", "editor"], ignore_case = true)]
        role: String,
        /// Optional custom password protecting the link.
        #[arg(long)]
        password: Option<String>,
        /// Optional expiry, Unix epoch seconds.
        #[arg(long)]
        expires: Option<i64>,
    },
    /// Change a node's public link. The URL stays the same; an option not
    /// given is kept.
    Update {
        /// File/folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// Public-link id, from `pdfs members`.
        #[arg(add = ArgValueCompleter::new(complete::public_link_id))]
        id: String,
        /// New role: viewer or editor.
        #[arg(long, value_parser = ["viewer", "editor"], ignore_case = true)]
        role: Option<String>,
        /// New custom password protecting the link.
        #[arg(long, conflicts_with = "no_password")]
        password: Option<String>,
        /// Remove the custom password.
        #[arg(long)]
        no_password: bool,
        /// New expiry, Unix epoch seconds.
        #[arg(long, conflicts_with = "no_expiry")]
        expires: Option<i64>,
        /// Remove the expiry.
        #[arg(long)]
        no_expiry: bool,
    },
    /// Remove a node's public link.
    Remove {
        /// File/folder path, inside the mountpoint or relative to it.
        #[arg(add = ArgValueCompleter::new(complete::drive_path))]
        path: PathBuf,
        /// Public-link id, from `pdfs members`.
        #[arg(add = ArgValueCompleter::new(complete::public_link_id))]
        id: String,
    },
}

#[derive(Subcommand)]
enum InvitationCmd {
    /// List invitations addressed to me.
    List,
    /// Accept an invitation by id.
    Accept {
        #[arg(add = ArgValueCandidates::new(complete::invitation_id))]
        id: String,
    },
    /// Reject an invitation by id.
    Reject {
        #[arg(add = ArgValueCandidates::new(complete::invitation_id))]
        id: String,
    },
}

#[derive(Subcommand)]
enum BookmarkCmd {
    /// List saved public-link bookmarks.
    List,
    /// Save a public link (optionally password-protected) as a bookmark.
    Add {
        /// Public link URL, including the `#password` fragment.
        url: String,
        /// Optional custom password for the link.
        #[arg(long)]
        password: Option<String>,
    },
    /// Remove a saved bookmark by its token.
    Rm {
        #[arg(add = ArgValueCandidates::new(complete::bookmark_token))]
        token: String,
    },
}

/// Render a daemon failure for a terminal.
///
/// Offline is the one class worth rewording: the daemon's prose names whichever
/// call happened to hit the network, which tells someone at a shell nothing they
/// can act on. Every other class already says something specific, so it is
/// printed as sent.
fn cli_error(kind: ErrorKind, message: &str) -> String {
    match kind {
        ErrorKind::Offline => "offline: the Proton Drive API is unreachable".to_string(),
        // The daemon's wording names the failed call ("upload x: ..."), which
        // does not say the thing the user has to act on.
        ErrorKind::Quota => format!("out of storage: {message}"),
        _ => message.to_string(),
    }
}

fn main() -> Result<()> {
    // A bad `PDFS_INSTANCE` is refused before anything reads a path or the
    // keyring; falling back to the default instance would touch the user's
    // own account.
    pdfs_core::config::check_instance()?;
    // Next: a completion request answers on stdout and exits, so nothing may
    // write there before it.
    complete::handle_request();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        // `pgp` logs a benign WARN when it re-serializes a Proton secret-key
        // packet with a longer (but equivalent) length encoding than the
        // original; drop it to error so normal runs stay quiet.
        .unwrap_or_else(|_| "info,pgp=error".into());
    if journal::stdout_is_journal() {
        // journalctl colours lines by the priority the prefix gives them, so
        // the escape codes would only be noise in the stored text.
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .event_format(journal::Prioritized(tracing_subscriber::fmt::format()))
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    let cli = Cli::parse();
    JSON_OUTPUT.store(cli.json, std::sync::atomic::Ordering::Relaxed);
    // The commands that hold the password, the key passphrases or the session
    // tokens in memory: keep them out of core dumps and away from same-user
    // ptrace. Only the daemon did, though `mount` holds the same keys for as
    // long as it runs.
    if matches!(
        cli.command,
        Command::Login { .. }
            | Command::Unlock { .. }
            | Command::Logout { .. }
            | Command::Mount { .. }
            | Command::Daemon { .. }
    ) {
        pdfs_core::kernelkey::forbid_dumps();
    }
    match cli.command {
        Command::Login { username, remember } => cmd_login(username, remember),
        Command::Unlock { remember } => cmd_unlock(remember),
        Command::Lock { forget } => cmd_lock(forget),
        Command::Logout { yes } => cmd_logout(yes),
        Command::Status => cmd_status(),
        Command::Locations => cmd_locations(),
        Command::Mount { mountpoint } => cmd_mount(mountpoint),
        Command::Daemon { mountpoint } => pdfs_daemon::run(mountpoint),
        Command::Pin { path } => cmd_pin(path),
        Command::Unpin { path } => cmd_unpin(path),
        Command::Pins => cmd_pins(),
        Command::Ls { path } => cmd_ls(path),
        Command::Photos {
            limit,
            offset,
            favorites,
        } => cmd_photos(limit, offset, favorites),
        Command::Favorite { uid, remove } => cmd_favorite(uid, remove),
        Command::Albums => cmd_albums(),
        Command::ImportGooglePhotos {
            archives,
            dry_run,
            resume,
            wait,
        } => cmd_import_google_photos(archives, dry_run, resume, wait),
        Command::RedatePhotos {
            dry_run,
            limit,
            from,
            to,
            wait,
        } => cmd_redate_photos(dry_run, limit, from, to, wait),
        Command::RedateStatus => cmd_redate_status(),
        Command::CancelRedate => cmd_cancel_redate(),
        Command::ImportStatus => cmd_import_status(),
        Command::CancelImport => cmd_cancel_import(),
        Command::Album { uid, limit, offset } => cmd_album(uid, limit, offset),
        Command::NewAlbum { name } => cmd_new_album(name),
        Command::RenameAlbum { uid, name } => {
            ok_or_bail(control_request(CtlRequest::RenameAlbum { uid, name })?)
        }
        Command::DeleteAlbum { uid, with_photos } => {
            ok_or_bail(control_request(CtlRequest::DeleteAlbum {
                uid,
                delete_photos: with_photos,
            })?)
        }
        Command::AddToAlbum { uid, photos } => {
            cmd_album_change(CtlRequest::AddToAlbum { uid, photos }, "added")
        }
        Command::RemoveFromAlbum { uid, photos } => {
            cmd_album_change(CtlRequest::RemoveFromAlbum { uid, photos }, "removed")
        }
        Command::OpenPhoto { uid } => cmd_open_photo(uid),
        Command::Search {
            query,
            limit,
            scope,
        } => cmd_search(query, limit, scope),
        Command::Rename { path, new_name } => cmd_rename(path, new_name),
        Command::Move { mut paths } => {
            let new_parent = paths.pop().expect("clap requires at least two paths");
            cmd_move(paths, new_parent)
        }
        Command::Copy { mut paths } => {
            let new_parent = paths.pop().expect("clap requires at least two paths");
            cmd_copy(paths, new_parent)
        }
        Command::Du { path } => cmd_du(path),
        Command::Rm { path } => cmd_rm(path),
        Command::Mkdir { parent, name } => cmd_mkdir(parent, name),
        Command::Upload { sources, parent } => cmd_upload(sources, parent),
        Command::Transfers => cmd_transfers(),
        Command::Diagnostics => cmd_diagnostics(),
        Command::Trash { by_folder, folder } => cmd_trash(by_folder, folder),
        Command::Restore { uids } => cmd_restore(uids),
        Command::DeleteForever { uids } => cmd_delete_forever(uids),
        Command::EmptyTrash => cmd_empty_trash(),
        Command::Refresh { target, full } => cmd_refresh(target, full),
        Command::Devices { action } => cmd_devices(action),
        Command::Sync { action } => cmd_sync(action),
        Command::Conflicts { action } => cmd_conflicts(action),
        Command::Share {
            path,
            emails,
            role,
            message,
        } => cmd_share(path, emails, role, message),
        Command::Members { path } => cmd_members(path),
        Command::ShareRole {
            path,
            id,
            kind,
            role,
        } => cmd_share_role(path, id, kind, role),
        Command::Unshare { path, id, kind } => cmd_unshare(path, id, kind),
        Command::Versions { action } => cmd_versions(action),
        Command::PublicLink { action } => cmd_public_link(action),
        Command::Shared => cmd_shared(),
        Command::SharedWithMe { uid } => cmd_shared_with_me(uid),
        Command::SharedGet { uid, dest } => cmd_shared_get(uid, dest),
        Command::Leave { uid } => cmd_leave(uid),
        Command::Invitations { action } => cmd_invitations(action),
        Command::Bookmarks { action } => cmd_bookmarks(action),
        Command::Activity { limit } => cmd_activity(limit),
        Command::Quota => cmd_quota(),
        Command::Cache { action } => cmd_cache(action),
        Command::Completions { shell } => cmd_completions(shell),
        Command::Diagnose => cmd_diagnose(),
    }
}

/// `YYYY-MM-DD` (UTC) for epoch seconds, without pulling a date crate into the
/// CLI for one column. Civil-from-days per Howard Hinnant's algorithm.
fn format_epoch_utc(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Format an epoch-seconds timestamp as a compact local-ish `MM-DD HH:MM`.
/// Deliberately dependency-free: derived directly from the Unix time via UTC.
fn format_epoch(secs: i64) -> String {
    // Minimal civil-time conversion (UTC) — good enough for an activity feed.
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi) = (tod / 3600, (tod % 3600) / 60);
    // Days since 1970-01-01 → y/m/d (Howard Hinnant's civil_from_days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let _ = y;
    format!("{m:02}-{d:02} {h:02}:{mi:02}")
}

/// Print an `Ok` message or bail on an `Error`, for the many commands whose only
/// reply is one of those two.
fn ok_or_bail(resp: CtlResponse) -> Result<()> {
    match resp {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// Seconds as something a human reads at a glance: the ages here run from
/// "still starting" to "wedged since yesterday".
fn human_secs(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h{}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{}h", s / 86400, (s % 86400) / 3600),
    }
}

/// Ask for a typed `yes` before an irreversible destroy. Errors (aborting the
/// command) on anything else, including a non-interactive stdin: a piped or
/// scripted invocation must not silently wipe the trash.
fn confirm(prompt: &str) -> Result<()> {
    print!("{prompt} [type \"yes\" to confirm] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != "yes" {
        bail!("aborted");
    }
    Ok(())
}

/// Normalize a user-supplied path argument for sending to the daemon. Absolute
/// paths are canonicalized (so the daemon can strip its mountpoint prefix);
/// relative paths are passed through and resolved against the mount root.
fn path_arg(path: &Path) -> Result<String> {
    let p = if path.is_absolute() {
        std::fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))?
    } else {
        path.to_path_buf()
    };
    p.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("path is not valid UTF-8"))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn human_age(secs: u64) -> String {
    const UNITS: [(&str, u64); 4] = [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)];
    for (unit, scale) in UNITS {
        if secs >= scale {
            return format!("{}{unit}", secs / scale);
        }
    }
    "just now".to_string()
}

/// Format a byte count for a report line.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [
        ("GiB", 1_073_741_824.0),
        ("MiB", 1_048_576.0),
        ("KiB", 1024.0),
        ("B", 1.0),
    ];
    let b = bytes as f64;
    for (unit, scale) in UNITS {
        if b >= scale {
            return format!("{:.1} {unit}", b / scale);
        }
    }
    "0 B".to_string()
}

/// Print the completion script for `shell`. Touches no account state, so
/// packaging can run it at build time.
fn cmd_completions(shell: complete::ShellArg) -> Result<()> {
    let mut script = Vec::new();
    complete::write_script(shell, &mut script)?;
    print!("{}", String::from_utf8_lossy(&script));
    Ok(())
}

/// Send one request to the running mount daemon's control socket and read its
/// reply. Errors if no daemon is listening.
fn control_request(req: CtlRequest) -> Result<CtlResponse> {
    let socket = AppDirs::new()?.control_socket();
    pdfs_core::control::send(&socket, &req)
        .with_context(|| format!("talk to mount daemon at {}", socket.display()))
}

#[cfg(test)]
mod pause_tests {
    use super::*;

    #[test]
    fn pause_durations_read_minutes_hours_and_days() {
        assert_eq!(parse_pause_duration("30m"), Ok(1800));
        assert_eq!(parse_pause_duration("1h"), Ok(3600));
        assert_eq!(parse_pause_duration("2d"), Ok(172_800));
    }

    #[test]
    fn pause_durations_reject_what_they_cannot_read() {
        assert!(parse_pause_duration("").is_err());
        assert!(parse_pause_duration("0h").is_err());
        assert!(parse_pause_duration("1w").is_err());
        assert!(parse_pause_duration("h").is_err());
        assert!(parse_pause_duration("-1h").is_err());
        assert!(parse_pause_duration("1ä").is_err());
    }

    #[test]
    fn rates_read_plain_bytes_and_binary_suffixes() {
        assert_eq!(parse_rate("0"), Ok(0));
        assert_eq!(parse_rate("1500"), Ok(1500));
        assert_eq!(parse_rate("500K"), Ok(512_000));
        assert_eq!(parse_rate("2m"), Ok(2_097_152));
        assert!(parse_rate("").is_err());
        assert!(parse_rate("M").is_err());
        assert!(parse_rate("1.5M").is_err());
        assert!(parse_rate("2MB").is_err());
    }
}

#[cfg(test)]
mod login_wait_tests {
    use super::*;

    #[test]
    fn login_failure_preserves_only_actionable_upgrade_advice() {
        use pdfs_core::proton_sdk::{ProtonApiError, ProtonError, api::ResponseCode};
        for (code, blocked) in [
            (ResponseCode::Blocked, true),
            (ResponseCode::OutdatedApp, false),
        ] {
            let error = ProtonError::Api(ProtonApiError {
                code,
                http_status: 422,
                message: "Please update the app".into(),
                details: None,
            });
            let rendered = format!("{:#}", login_failure(error.into()));
            assert_eq!(rendered.contains("2028"), blocked);
            assert_eq!(rendered.contains("Please update"), !blocked);
        }
    }

    /// The file-manager script passes each selected path after `--`, so a
    /// file named like an option is still the path.
    #[test]
    fn a_path_after_a_double_dash_is_a_path_even_when_it_looks_like_a_flag() {
        let cli = Cli::try_parse_from(["pdfs", "pin", "--", "--json"]).unwrap();
        assert!(!cli.json);
        match cli.command {
            Command::Pin { path } => assert_eq!(path, PathBuf::from("--json")),
            _ => panic!("not a pin"),
        }
    }

    #[test]
    fn a_waited_for_run_that_did_not_fully_succeed_exits_with_an_error() {
        assert!(finished_run("the import", Some((0, false, None))).is_ok());
        assert!(finished_run("the import", Some((3, false, None))).is_err());
        assert!(finished_run("the import", Some((0, true, None))).is_err());
        let failed = finished_run("the import", Some((0, false, Some("not a zip".into()))));
        assert!(failed.unwrap_err().to_string().contains("not a zip"));
        assert!(finished_run("the import", None).is_err());
    }

    #[test]
    fn a_waited_for_import_that_did_not_finish_exits_with_an_error() {
        use pdfs_core::control::ImportSummary;
        let done = ImportSummary {
            uploaded: 4,
            ..Default::default()
        };
        assert!(import_outcome(Some(&done)).is_ok());
        assert!(import_outcome(None).is_err());
        for broken in [
            ImportSummary {
                interrupted: true,
                ..Default::default()
            },
            ImportSummary {
                unreadable_archives: vec!["/exports/takeout-002.zip".into()],
                ..Default::default()
            },
            ImportSummary {
                error: Some("check for existing photos: offline".into()),
                ..Default::default()
            },
        ] {
            assert!(import_outcome(Some(&broken)).is_err(), "{broken:?}");
        }
        let interrupted = import_outcome(Some(&ImportSummary {
            interrupted: true,
            ..Default::default()
        }));
        assert!(interrupted.unwrap_err().to_string().contains("--resume"));
    }

    #[test]
    fn resume_needs_no_archives_and_excludes_them() {
        let cli = Cli::try_parse_from(["pdfs", "import-google-photos", "--resume"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::ImportGooglePhotos { resume: true, .. }
        ));
        assert!(Cli::try_parse_from(["pdfs", "import-google-photos"]).is_err());
    }

    #[test]
    fn sync_ignore_takes_a_pattern_that_looks_like_an_option_after_dashes() {
        let cli = Cli::try_parse_from(["pdfs", "sync", "ignore", "add", "--", "-temp*"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Sync {
                action: SyncCmd::Ignore {
                    action: IgnoreCmd::Add {
                        ref pattern,
                        folder: None
                    }
                }
            } if pattern == "-temp*"
        ));
        assert!(Cli::try_parse_from(["pdfs", "sync", "ignore", "add"]).is_err());
        assert!(Cli::try_parse_from(["pdfs", "sync", "ignore", "reset"]).is_ok());
        let cli =
            Cli::try_parse_from(["pdfs", "sync", "ignore", "add", "*.db", "--folder", "Work"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Command::Sync {
                action: SyncCmd::Ignore {
                    action: IgnoreCmd::Add {
                        ref pattern,
                        folder: Some(ref folder)
                    }
                }
            } if pattern == "*.db" && folder == Path::new("Work")
        ));
        assert!(
            Cli::try_parse_from(["pdfs", "sync", "ignore", "list", "--folder", "Work"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["pdfs", "import-google-photos", "--resume", "a.zip"]).is_err()
        );
    }

    #[test]
    fn a_link_flag_pair_keeps_removes_or_sets() {
        assert_eq!(link_change::<i64>(None, false), LinkChange::Keep);
        assert_eq!(link_change::<i64>(None, true), LinkChange::Remove);
        assert_eq!(link_change(Some(5), false), LinkChange::Set(5));
    }

    #[test]
    fn epoch_seconds_format_as_utc_calendar_dates() {
        assert_eq!(format_epoch_utc(0), "1970-01-01");
        assert_eq!(format_epoch_utc(951_782_400), "2000-02-29");
        assert_eq!(format_epoch_utc(1_700_000_000), "2023-11-14");
        assert_eq!(format_epoch_utc(-86_400), "1969-12-31");
    }

    #[test]
    fn last_synced_shows_a_time_not_epoch_seconds() {
        // B278: issue #46 showed `synced: 1793570850`.
        assert_eq!(last_synced(Some(1_793_570_850)), "11-01 22:07");
        assert_eq!(last_synced(Some(0)), "never");
        assert_eq!(last_synced(None), "never");
    }

    #[test]
    fn a_forged_looking_invitation_is_flagged_not_stated() {
        let mut entry = pdfs_core::control::DirEntry {
            name: "Budget.ods".into(),
            is_dir: false,
            size: 1,
            modified: 0,
            pinned: false,
            pinned_by_folder: false,
            cached: false,
            uid: "vol~link".into(),
            path: String::new(),
            role: "viewer".into(),
            shared_by: String::new(),
            shared_at: 0,
            shared_by_unverified: false,
            trashed_at: 0,
            trashed_from: None,
            trashed_parent: None,
            issue: None,
        };
        assert_eq!(shared_by_line(&entry), None, "no invitation, no line");
        entry.shared_by = "alice@proton.me".into();
        entry.shared_at = 1_700_000_000;
        assert_eq!(
            shared_by_line(&entry).as_deref(),
            Some("shared by alice@proton.me at 2023-11-14")
        );
        entry.shared_by_unverified = true;
        assert!(shared_by_line(&entry).unwrap().contains("WARNING"));
    }
}

#[cfg(test)]
mod unshare_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn unshare_with_only_a_path_stops_sharing() {
        let cli = Cli::try_parse_from(["pdfs", "unshare", "docs/report.pdf"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Unshare {
                id: None,
                kind: None,
                ..
            }
        ));
    }

    #[test]
    fn unshare_with_an_id_needs_its_kind() {
        assert!(Cli::try_parse_from(["pdfs", "unshare", "docs", "abc"]).is_err());
        let cli = Cli::try_parse_from(["pdfs", "unshare", "docs", "abc", "member"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Unshare {
                id: Some(_),
                kind: Some(_),
                ..
            }
        ));
    }
}
