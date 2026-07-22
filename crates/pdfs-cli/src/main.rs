//! `pdfs` — command-line front-end for the Proton Drive Linux client.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use pdfs_core::auth;
use pdfs_core::cache::ContentCache;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    ErrorKind, RefreshScope, Request as CtlRequest, Response as CtlResponse,
    RestoreItem as CtlRestoreItem, ShareEntryKind, SyncPhase, pending_summary,
};
use pdfs_core::db::Db;

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
    /// `devices list`, `transfers`, `activity`, `cache inspect`). Commands that
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
    },
    /// Forget the stored session.
    Logout,
    /// Show account login state, and mount status if a daemon is running.
    Status,
    /// Mount Proton Drive at the given (or default) path. Blocks until unmounted.
    Mount {
        /// Mountpoint; defaults to ~/ProtonDrive.
        mountpoint: Option<PathBuf>,
    },
    /// Run the auto-mount daemon: wait for login, mount, and keep it mounted
    /// until stopped. This is what the systemd user service runs; you normally
    /// don't invoke it by hand.
    Daemon {
        /// Mountpoint; defaults to ~/ProtonDrive.
        mountpoint: Option<PathBuf>,
    },
    /// Keep a file on this device (download + cache it). Needs a running mount.
    Pin {
        /// File path, inside the mountpoint or relative to it.
        path: PathBuf,
    },
    /// Stop keeping a file on this device, evicting its cached content.
    Unpin {
        /// File path, inside the mountpoint or relative to it.
        path: PathBuf,
    },
    /// List files pinned to this device.
    Pins,
    /// List a directory via the running daemon (in-app browser backend).
    Ls {
        /// Directory path, inside the mountpoint or relative to it. Root if omitted.
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
    },
    /// Rename a file or folder via the running daemon.
    Rename {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
        /// New name (a single path component).
        new_name: String,
    },
    /// Move a file or folder into another folder via the running daemon.
    Move {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
        /// Destination folder path, inside the mountpoint or relative to it.
        new_parent: PathBuf,
    },
    /// Trash a file or folder via the running daemon.
    Rm {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
    },
    /// Create a folder via the running daemon.
    Mkdir {
        /// Parent folder path, inside the mountpoint or relative to it.
        parent: PathBuf,
        /// New folder name (a single path component).
        name: String,
    },
    /// Upload local files and/or folders into a Drive folder via the running
    /// daemon. Folders are uploaded recursively. The daemon uploads in the
    /// background; watch progress with `pdfs transfers`.
    Upload {
        /// Local files and/or folders to upload.
        #[arg(required = true)]
        sources: Vec<PathBuf>,
        /// Destination folder, inside the mountpoint or relative to it.
        #[arg(short = 't', long = "to", default_value = ".")]
        parent: PathBuf,
    },
    /// Show the daemon's in-flight transfers (active uploads/downloads).
    Transfers,
    /// List what is in the account's trash, with the uids the restore and
    /// delete-forever commands take.
    Trash,
    /// Restore trashed items, by uid, to the folders they were trashed from.
    Restore {
        /// Node uids (`volume~link`), as printed by `pdfs trash`.
        #[arg(required = true)]
        uids: Vec<String>,
    },
    /// Permanently delete trashed items by uid. This cannot be undone.
    DeleteForever {
        /// Node uids (`volume~link`), as printed by `pdfs trash`.
        #[arg(required = true)]
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
        target: Option<String>,
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
    /// Share a file or folder with Proton and/or external email addresses.
    Share {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
        /// One or more invitee emails (Proton or external, auto-detected).
        #[arg(required = true)]
        emails: Vec<String>,
        /// Role to grant: viewer, editor or admin.
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Optional message included in the invitation email.
        #[arg(long)]
        message: Option<String>,
    },
    /// List a node's members, pending invitations and public link.
    Members {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
    },
    /// Change the role of a member or pending invitation on a node's share.
    ShareRole {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
        /// Entry id (membership or invitation id), from `pdfs members`.
        id: String,
        /// Entry kind: member, proton or external.
        kind: ShareKindArg,
        /// New role: viewer, editor or admin.
        role: String,
    },
    /// Remove a member or pending invitation from a node's share.
    Unshare {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
        /// Entry id (membership or invitation id), from `pdfs members`.
        id: String,
        /// Entry kind: member, proton or external.
        kind: ShareKindArg,
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
        uid: Option<String>,
    },
    /// Download a file shared with me by its uid (from `pdfs shared-with-me`).
    SharedGet {
        /// Node uid in `volume~link` form.
        uid: String,
        /// Where to write it. Omit to leave it in the cache and print the path.
        dest: Option<PathBuf>,
    },
    /// Leave a shared node by its uid (from `pdfs shared-with-me`).
    Leave {
        /// Node uid in `volume~link` form.
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

#[derive(Subcommand)]
enum DeviceCmd {
    /// List registered devices.
    List,
    /// Rename a device by uid.
    Rename { uid: String, name: String },
    /// Delete (deregister) a device by uid.
    Rm { uid: String },
    /// Adopt an existing device as this machine's, so a hostname change or a
    /// reinstall re-attaches to it instead of registering a duplicate.
    Adopt {
        /// Device uid (from `devices list`). Omit with `--clear`.
        uid: Option<String>,
        /// Drop the adoption and go back to matching by hostname.
        #[arg(long, conflicts_with = "uid")]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Add a local folder to this machine's device and upload its contents.
    Add {
        /// Local directory to sync.
        path: PathBuf,
    },
    /// List this machine's synced folders.
    List,
    /// Remove a synced folder by id.
    Rm {
        /// Synced-folder id (from `sync list`).
        id: i64,
        /// Also delete the folder's copy in Proton Drive.
        #[arg(long)]
        delete_remote: bool,
    },
    /// Force a sync pass now (all folders, or one by id).
    Now {
        /// Optional synced-folder id; omit to reconcile every folder.
        id: Option<i64>,
    },
    /// Switch a folder between full-copy sync and on-demand (FUSE, no local storage).
    Mode {
        /// Synced-folder id (from `sync list`).
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
    },
}

#[derive(Subcommand)]
enum PublicLinkCmd {
    /// Create a public link on a node.
    Create {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
        /// Role: viewer or editor.
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Optional custom password protecting the link.
        #[arg(long)]
        password: Option<String>,
        /// Optional expiry, Unix epoch seconds.
        #[arg(long)]
        expires: Option<i64>,
    },
    /// Remove a node's public link.
    Remove {
        /// File/folder path, inside the mountpoint or relative to it.
        path: PathBuf,
        /// Public-link id, from `pdfs members`.
        id: String,
    },
}

#[derive(Subcommand)]
enum InvitationCmd {
    /// List invitations addressed to me.
    List,
    /// Accept an invitation by id.
    Accept { id: String },
    /// Reject an invitation by id.
    Reject { id: String },
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
    Rm { token: String },
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // `pgp` logs a benign WARN when it re-serializes a Proton secret-key
                // packet with a longer (but equivalent) length encoding than the
                // original; drop it to error so normal runs stay quiet.
                .unwrap_or_else(|_| "info,pgp=error".into()),
        )
        .init();

    let cli = Cli::parse();
    JSON_OUTPUT.store(cli.json, std::sync::atomic::Ordering::Relaxed);
    match cli.command {
        Command::Login { username } => cmd_login(username),
        Command::Logout => cmd_logout(),
        Command::Status => cmd_status(),
        Command::Mount { mountpoint } => cmd_mount(mountpoint),
        Command::Daemon { mountpoint } => cmd_daemon(mountpoint),
        Command::Pin { path } => cmd_pin(path),
        Command::Unpin { path } => cmd_unpin(path),
        Command::Pins => cmd_pins(),
        Command::Ls { path } => cmd_ls(path),
        Command::Photos { limit, offset } => cmd_photos(limit, offset),
        Command::OpenPhoto { uid } => cmd_open_photo(uid),
        Command::Search { query, limit } => cmd_search(query, limit),
        Command::Rename { path, new_name } => cmd_rename(path, new_name),
        Command::Move { path, new_parent } => cmd_move(path, new_parent),
        Command::Rm { path } => cmd_rm(path),
        Command::Mkdir { parent, name } => cmd_mkdir(parent, name),
        Command::Upload { sources, parent } => cmd_upload(sources, parent),
        Command::Transfers => cmd_transfers(),
        Command::Trash => cmd_trash(),
        Command::Restore { uids } => cmd_restore(uids),
        Command::DeleteForever { uids } => cmd_delete_forever(uids),
        Command::EmptyTrash => cmd_empty_trash(),
        Command::Refresh { target } => cmd_refresh(target),
        Command::Devices { action } => cmd_devices(action),
        Command::Sync { action } => cmd_sync(action),
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
        Command::Diagnose => cmd_diagnose(),
    }
}

fn cmd_devices(action: DeviceCmd) -> Result<()> {
    match action {
        DeviceCmd::List => {
            let response = control_request(CtlRequest::ListDevices)?;
            if emit_json(&response)? {
                return Ok(());
            }
            match response {
                CtlResponse::Devices { items } if items.is_empty() => println!("No devices."),
                CtlResponse::Devices { items } => {
                    for d in items {
                        let sync = d
                            .last_sync
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "never".to_string());
                        let tag = match (d.adopted, d.this_device) {
                            (true, _) => "  *adopted*",
                            (false, true) => "  *this machine*",
                            _ => "",
                        };
                        println!(
                            "{}  {}  (synced: {sync})  [{}]{tag}",
                            d.device_type, d.name, d.uid
                        );
                    }
                }
                CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
                other => bail!("unexpected response: {other:?}"),
            }
        }
        DeviceCmd::Rename { uid, name } => {
            ok_or_bail(control_request(CtlRequest::RenameDevice { uid, name })?)?
        }
        DeviceCmd::Rm { uid } => ok_or_bail(control_request(CtlRequest::DeleteDevice { uid })?)?,
        DeviceCmd::Adopt { uid, clear } => {
            if uid.is_none() && !clear {
                bail!("give a device uid (see `pdfs devices list`), or --clear");
            }
            let uid = if clear { None } else { uid };
            ok_or_bail(control_request(CtlRequest::AdoptDevice { uid })?)?
        }
    }
    Ok(())
}

fn cmd_sync(action: SyncCmd) -> Result<()> {
    match action {
        SyncCmd::Add { path } => {
            let abs = std::fs::canonicalize(&path)
                .with_context(|| format!("resolve {}", path.display()))?;
            let local_path = abs
                .to_str()
                .ok_or_else(|| anyhow!("path is not valid UTF-8"))?
                .to_string();
            ok_or_bail(control_request(CtlRequest::AddSyncFolder { local_path })?)?
        }
        SyncCmd::List => {
            let response = control_request(CtlRequest::ListSyncFolders)?;
            if emit_json(&response)? {
                return Ok(());
            }
            match response {
                CtlResponse::SyncFolders { items } if items.is_empty() => {
                    println!("No synced folders.")
                }
                CtlResponse::SyncFolders { items } => {
                    for f in items {
                        let sync = if f.last_sync == 0 {
                            "never".to_string()
                        } else {
                            f.last_sync.to_string()
                        };
                        // A queued switch is reported as the mode the folder is moving
                        // to, so `sync list` never reads as if the request was dropped.
                        let mode = match &f.pending_mode {
                            Some(pending) => format!("{} → {pending}", f.mode),
                            None => f.mode.clone(),
                        };
                        println!(
                            "[{}]  {}  ({mode}, {}, synced: {sync})",
                            f.id, f.local_path, f.state
                        );
                        // A pass in flight says what it is doing, indented under it.
                        if let Some(p) = &f.progress {
                            match p.phase {
                                SyncPhase::Scanning if p.total == 0 => println!("      scanning…"),
                                SyncPhase::Scanning => {
                                    println!(
                                        "      scanning: {} of {}",
                                        p.done,
                                        p.total.max(p.done)
                                    )
                                }
                                SyncPhase::Applying => println!(
                                    "      {} of {}  {}",
                                    p.done + 1,
                                    p.total.max(p.done + 1),
                                    p.current
                                ),
                            }
                        }
                    }
                }
                CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
                other => bail!("unexpected response: {other:?}"),
            }
        }
        SyncCmd::Rm { id, delete_remote } => {
            ok_or_bail(control_request(CtlRequest::RemoveSyncFolder {
                id,
                delete_remote,
            })?)?
        }
        SyncCmd::Now { id } => ok_or_bail(control_request(CtlRequest::SyncNow { id })?)?,
        SyncCmd::Mode { id, mode } => {
            ok_or_bail(control_request(CtlRequest::SetSyncFolderMode { id, mode })?)?
        }
        SyncCmd::Restore { yes } => return cmd_sync_restore(yes),
    }
    Ok(())
}

/// `pdfs sync restore`: list what this machine's device holds, confirm where
/// each folder should land, then hand the mapping to the daemon.
///
/// The daemon only ever *proposes* local paths — a path recorded by another
/// machine may name a home directory that does not exist here — so the default
/// flow is propose-and-confirm. `--yes` accepts the proposals for scripting.
fn cmd_sync_restore(yes: bool) -> Result<()> {
    let response = control_request(CtlRequest::ListRestorableFolders)?;
    if emit_json(&response)? {
        return Ok(());
    }
    let items = match response {
        CtlResponse::RestorableFolders { items } => items,
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    };
    let candidates: Vec<_> = items.into_iter().filter(|f| !f.already_synced).collect();
    if candidates.is_empty() {
        println!("Nothing to restore: this device has no folders that aren't already synced here.");
        return Ok(());
    }

    let mut chosen: Vec<CtlRestoreItem> = Vec::new();
    for f in candidates {
        if yes {
            println!("{}  ->  {}  [{}]", f.name, f.local_path, f.mode);
            chosen.push(CtlRestoreItem {
                remote_uid: f.remote_uid,
                local_path: f.local_path,
                mode: f.mode,
            });
            continue;
        }
        // Empty answer takes the proposal, `n` skips, anything else is read as
        // the path to use instead — the three things a user wants here.
        let answer = prompt_line(&format!(
            "{} [{}] -> {} (Enter to accept, 'n' to skip, or a path)",
            f.name, f.mode, f.local_path
        ))?;
        match answer.as_str() {
            "n" | "N" => continue,
            "" => chosen.push(CtlRestoreItem {
                remote_uid: f.remote_uid,
                local_path: f.local_path,
                mode: f.mode,
            }),
            path => chosen.push(CtlRestoreItem {
                remote_uid: f.remote_uid,
                local_path: expand_tilde(path),
                mode: f.mode,
            }),
        }
    }
    if chosen.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }
    ok_or_bail(control_request(CtlRequest::RestoreSyncFolders {
        items: chosen,
    })?)
}

/// Expand a leading `~` in a hand-typed path. The daemon requires an absolute
/// path, and a shell is not always the one doing the typing here.
fn expand_tilde(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home)
                .join(rest)
                .to_string_lossy()
                .into_owned(),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

fn cmd_share(
    path: PathBuf,
    emails: Vec<String>,
    role: String,
    message: Option<String>,
) -> Result<()> {
    ok_or_bail(control_request(CtlRequest::ShareNode {
        path: path_arg(&path)?,
        emails,
        role,
        message,
    })?)
}

fn cmd_members(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::ListShare {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Share { entries, link } => {
            if entries.is_empty() {
                println!("No members or pending invitations.");
            }
            for e in entries {
                let kind = match e.kind {
                    ShareEntryKind::Member => "member",
                    ShareEntryKind::ProtonInvite => "invited (proton)",
                    ShareEntryKind::ExternalInvite => "invited (external)",
                };
                println!("{:<18} {:<8} {}  [{}]", kind, e.role, e.email, e.id);
            }
            match link {
                Some(l) => {
                    let url = l.url.as_deref().unwrap_or("(url hidden)");
                    let pw = if l.has_password { " +password" } else { "" };
                    println!("public link ({}{pw}): {url}  [{}]", l.role, l.id);
                }
                None => println!("public link: none"),
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_share_role(path: PathBuf, id: String, kind: ShareKindArg, role: String) -> Result<()> {
    ok_or_bail(control_request(CtlRequest::UpdateShareRole {
        path: path_arg(&path)?,
        id,
        kind: kind.to_kind(),
        role,
    })?)
}

fn cmd_unshare(path: PathBuf, id: String, kind: ShareKindArg) -> Result<()> {
    ok_or_bail(control_request(CtlRequest::RemoveShareEntry {
        path: path_arg(&path)?,
        id,
        kind: kind.to_kind(),
    })?)
}

fn cmd_public_link(action: PublicLinkCmd) -> Result<()> {
    match action {
        PublicLinkCmd::Create {
            path,
            role,
            password,
            expires,
        } => match control_request(CtlRequest::CreatePublicLink {
            path: path_arg(&path)?,
            role,
            password,
            expires,
        })? {
            CtlResponse::PublicLink { link } => {
                println!("{}", link.url.as_deref().unwrap_or("(no url returned)"));
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        PublicLinkCmd::Remove { path, id } => {
            ok_or_bail(control_request(CtlRequest::RemovePublicLink {
                path: path_arg(&path)?,
                id,
            })?)?
        }
    }
    Ok(())
}

fn cmd_shared() -> Result<()> {
    match control_request(CtlRequest::ListSharedByMe)? {
        CtlResponse::SharedByMe { items } if items.is_empty() => {
            println!("You haven't shared anything.")
        }
        CtlResponse::SharedByMe { items } => {
            for it in items {
                let kind = if it.is_dir { "d" } else { "-" };
                let mut tags = Vec::new();
                if it.member_count > 0 {
                    tags.push(format!("{} member(s)", it.member_count));
                }
                if it.invite_count > 0 {
                    tags.push(format!("{} pending", it.invite_count));
                }
                if let Some(link) = &it.link {
                    tags.push(match &link.url {
                        Some(url) => format!("link: {url}"),
                        None => "link".to_string(),
                    });
                }
                let tags = if tags.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", tags.join(", "))
                };
                println!("{kind} {}{tags}  [{}]", it.name, it.uid);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// List what is shared with me, or — given a folder uid — that folder's
/// children. Shared items have no path in the mount, so a uid is the only handle
/// there is: every row prints one, and it is what the next level down (or
/// `shared-get`) is addressed with.
fn cmd_shared_with_me(uid: Option<String>) -> Result<()> {
    let request = match uid {
        Some(uid) => CtlRequest::ListSharedFolder { uid },
        None => CtlRequest::ListSharedWithMe,
    };
    let response = control_request(request)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Entries { entries } if entries.is_empty() => {
            println!("Nothing shared with you.")
        }
        CtlResponse::Entries { entries } => {
            for e in entries {
                let kind = if e.is_dir { "d" } else { "-" };
                println!("{kind} {:>12}  {}  [{}]", e.size, e.name, e.uid);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// Download a file shared with me. The daemon puts the plaintext in its content
/// cache and replies with that path; with a `dest` we copy it out, so the caller
/// gets a file it owns rather than one the cache may later evict.
fn cmd_shared_get(uid: String, dest: Option<PathBuf>) -> Result<()> {
    let path = match control_request(CtlRequest::OpenSharedFile { uid })? {
        CtlResponse::FilePath { path } => PathBuf::from(path),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    };
    match dest {
        Some(dest) => {
            std::fs::copy(&path, &dest).with_context(|| format!("writing {}", dest.display()))?;
            println!("{}", dest.display());
        }
        None => println!("{}", path.display()),
    }
    Ok(())
}

fn cmd_quota() -> Result<()> {
    let response = control_request(CtlRequest::AccountQuota)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::AccountQuota {
            max_space,
            used_space,
        } => {
            let used = used_space.max(0) as u64;
            if max_space > 0 {
                let total = max_space as u64;
                let pct = (used as f64 / total as f64 * 100.0).round() as u64;
                println!(
                    "{} of {} used ({pct}%)",
                    human_bytes(used),
                    human_bytes(total)
                );
            } else {
                println!("{} used", human_bytes(used));
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_activity(limit: usize) -> Result<()> {
    let response = control_request(CtlRequest::ListActivity { limit })?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Activity { items } if items.is_empty() => println!("No recent activity."),
        CtlResponse::Activity { items } => {
            for a in items {
                let when = format_epoch(a.time);
                let verb = activity_verb(a.kind);
                let mark = if a.ok { " " } else { "!" };
                let detail = if a.detail.is_empty() {
                    String::new()
                } else {
                    format!("  {}", a.detail)
                };
                println!("{mark} {when}  {verb:<8} {}{detail}", a.target);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// A short verb for an activity kind, for the CLI's one-line-per-event feed.
fn activity_verb(kind: pdfs_core::control::ActivityKind) -> &'static str {
    use pdfs_core::control::ActivityKind::*;
    match kind {
        Upload => "upload",
        Download => "download",
        Sync => "sync",
        Rename => "rename",
        Move => "move",
        CreateFolder => "mkdir",
        Trash => "trash",
        Restore => "restore",
        DeleteForever => "delete",
        EmptyTrash => "empty",
        Share => "share",
        PublicLink => "link",
        Unshare => "unshare",
    }
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

fn cmd_leave(uid: String) -> Result<()> {
    ok_or_bail(control_request(CtlRequest::LeaveShared { uid })?)
}

fn cmd_invitations(action: InvitationCmd) -> Result<()> {
    match action {
        InvitationCmd::List => match control_request(CtlRequest::ListInvitations)? {
            CtlResponse::Invitations { items } if items.is_empty() => {
                println!("No pending invitations.")
            }
            CtlResponse::Invitations { items } => {
                for i in items {
                    let kind = if i.is_dir { "folder" } else { "file" };
                    let name = i.name.as_deref().unwrap_or("(name hidden)");
                    println!("{} shared {kind} \"{name}\"  [{}]", i.inviter_email, i.id);
                }
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        InvitationCmd::Accept { id } => {
            ok_or_bail(control_request(CtlRequest::AcceptInvitation { id })?)?
        }
        InvitationCmd::Reject { id } => {
            ok_or_bail(control_request(CtlRequest::RejectInvitation { id })?)?
        }
    }
    Ok(())
}

fn cmd_bookmarks(action: BookmarkCmd) -> Result<()> {
    match action {
        BookmarkCmd::List => match control_request(CtlRequest::ListBookmarks)? {
            CtlResponse::Bookmarks { items } if items.is_empty() => println!("No bookmarks."),
            CtlResponse::Bookmarks { items } => {
                for b in items {
                    let name = b.name.as_deref().unwrap_or("(name hidden)");
                    println!("{name}  {}  [{}]", b.url, b.token);
                }
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        BookmarkCmd::Add { url, password } => {
            ok_or_bail(control_request(CtlRequest::CreateBookmark {
                url,
                password,
            })?)?
        }
        BookmarkCmd::Rm { token } => {
            ok_or_bail(control_request(CtlRequest::DeleteBookmark { token })?)?
        }
    }
    Ok(())
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

/// Read a line from stdin with a prompt (echoed).
fn prompt_line(label: &str) -> Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

fn cmd_login(username: Option<String>) -> Result<()> {
    let username = match username {
        Some(u) => u,
        None => prompt_line("Proton email")?,
    };
    let password = rpassword::prompt_password("Password: ").context("read password")?;

    // The 2FA prompt fires lazily — only if the account actually requires it.
    let get_totp = || -> pdfs_core::Result<String> {
        prompt_line("2FA code").map_err(|e| pdfs_core::Error::Other(format!("read 2FA code: {e}")))
    };

    let rt = tokio::runtime::Runtime::new()?;
    // Solving a CAPTCHA needs a browser engine, which the CLI has no business
    // carrying. Point at the app that does rather than failing with the raw API
    // message, which reads as "your login is broken".
    rt.block_on(auth::login(&username, &password, get_totp))
        .map_err(|e| match e {
            pdfs_core::Error::HumanVerificationRequired(_) => anyhow::anyhow!(
                "Proton is asking for a CAPTCHA before it will accept this sign-in.\n\
                 Run `pdfs-app` and sign in there — it can show the verification page.\n\
                 (This is usually triggered by a VPN or an unfamiliar IP; signing in from \
                 your usual network often avoids it.)"
            ),
            other => anyhow::Error::new(other).context("login failed"),
        })?;

    println!("Logged in as {username}. Session stored in the system keyring.");
    Ok(())
}

fn cmd_logout() -> Result<()> {
    auth::logout()?;
    println!("Stored session cleared.");
    Ok(())
}

fn cmd_status() -> Result<()> {
    // `status` is the one query that merges two sources — the local keyring and
    // the daemon — so its JSON is composed here rather than being a daemon reply
    // passed through. A script asking for status wants both halves in one
    // object, including the "logged in but no daemon" case that has no reply at
    // all.
    if json_enabled() {
        let session = match auth::load() {
            Ok(s) => Some(s),
            Err(pdfs_core::Error::NotLoggedIn) => None,
            Err(e) => return Err(e.into()),
        };
        let mount = match control_request(CtlRequest::Status) {
            Ok(r @ CtlResponse::Status { .. }) => response_payload(&r)?,
            _ => serde_json::Value::Null,
        };
        let out = serde_json::json!({
            "logged_in": session.is_some(),
            "username": session.as_ref().map(|s| s.username.clone()),
            "user_id": session.as_ref().map(|s| s.user_id.clone()),
            "scopes": session.as_ref().map(|s| s.scopes.clone()),
            "mount": mount,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    match auth::load() {
        Ok(s) => {
            println!("Logged in as {} (user {})", s.username, s.user_id);
            println!("Scopes: {}", s.scopes.join(", "));
        }
        Err(pdfs_core::Error::NotLoggedIn) => {
            println!("Not logged in. Run `pdfs login`.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    }
    // If a mount daemon is running, report live status from its control socket.
    match control_request(CtlRequest::Status) {
        Ok(CtlResponse::Status {
            mountpoint,
            pinned,
            online,
            pending_uploads,
            pending_changes,
            ..
        }) => {
            let state = if online { "" } else { ", offline" };
            let queued = match pending_summary(pending_uploads, pending_changes) {
                Some(s) => format!(", {s}"),
                None => String::new(),
            };
            println!("Mounted at {mountpoint} ({pinned} pinned{state}{queued})");
        }
        Ok(other) => println!("Mount: unexpected response {other:?}"),
        Err(_) => println!("Mount: not running."),
    }
    Ok(())
}

fn cmd_mount(mountpoint: Option<PathBuf>) -> Result<()> {
    mount_once(mountpoint)?;
    Ok(())
}

/// Run the auto-mount daemon loop. Waits for a stored login, mounts, and keeps
/// the mount alive: a clean stop (SIGTERM via `systemctl --user stop`) exits 0;
/// an external unmount triggers a remount; errors back off and retry. This is
/// the entry point for the systemd user service.
fn cmd_daemon(mountpoint: Option<PathBuf>) -> Result<()> {
    loop {
        // Wait until a session is stored. The GUI enables this service on login,
        // but the service may also start at boot before the user has logged in.
        loop {
            match auth::load() {
                Ok(_) => break,
                Err(pdfs_core::Error::NotLoggedIn) => {
                    tracing::info!("not logged in; waiting…");
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
                Err(e) => return Err(e.into()),
            }
        }

        match mount_once(mountpoint.clone()) {
            Ok(pdfs_fuse::MountOutcome::Shutdown) => {
                tracing::info!("daemon stopping");
                return Ok(());
            }
            Ok(pdfs_fuse::MountOutcome::Unmounted) => {
                tracing::warn!("mount ended externally; remounting in 2s");
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(e) => {
                // `{:#}` so the whole anyhow chain lands in the log — the top
                // context alone ("mount failed") never says *why*.
                tracing::error!(error = format!("{e:#}"), "mount failed; retrying in 5s");
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    }
}

/// One mount run: resume the session, mount, and block until the mount ends
/// (clean shutdown or external unmount). Returns the [`MountOutcome`] so the
/// daemon loop can decide whether to exit or remount.
fn mount_once(mountpoint: Option<PathBuf>) -> Result<pdfs_fuse::MountOutcome> {
    let dirs = AppDirs::new()?;
    dirs.ensure()?;
    let config = dirs.load_config();
    // An explicit `--mountpoint` wins; otherwise honor the Settings-page choice
    // (config), falling back to the default location.
    let mountpoint = mountpoint.unwrap_or_else(|| dirs.resolved_mountpoint(&config));
    std::fs::create_dir_all(&mountpoint)
        .with_context(|| format!("create mountpoint {}", mountpoint.display()))?;

    let username = auth::load().map(|s| s.username).unwrap_or_default();
    // The daemon owns the unified DB (single writer). Open it here so the content
    // cache and the mount share one handle: the cache uses its `cache_entries`
    // table as the LRU index, the mount uses it for nodes/search/event cursor.
    let db = Arc::new(Db::open(&dirs.db_path()).context("open cache db")?);
    let cache = ContentCache::open(
        dirs.content_cache_dir(),
        dirs.pins_path(),
        config.resolved_cache_budget(),
        db.clone(),
    )
    .context("open content cache")?;
    let control_socket = dirs.control_socket();

    // Multi-threaded runtime: its worker threads keep servicing async SDK calls
    // while the main thread is parked inside the blocking FUSE session loop.
    let rt = tokio::runtime::Runtime::new()?;
    let (client, session) = rt
        .block_on(auth::resume_client())
        .context("resume session (run `pdfs login` first)")?;

    // Token updates are persisted automatically in real-time. The SDK session has
    // a token refresh callback registered during `resume_client` that writes updated
    // credentials back to the OS keyring immediately upon rotation. This prevents
    // the system keyring from holding stale refresh tokens even if the daemon crashes,
    // the system is suspended, or the network changes.
    let handle = rt.handle().clone();
    let result = pdfs_fuse::mount(
        client,
        handle,
        &mountpoint,
        cache,
        &control_socket,
        db,
        username,
    );

    // Clean unmount check (as a best-effort final backup).
    if let Err(e) = rt.block_on(auth::persist(&session)) {
        tracing::debug!(error = %e, "no new tokens to persist on shutdown");
    }

    let outcome = result.context("mount failed")?;
    Ok(outcome)
}

fn cmd_pin(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Pin {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_unpin(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Unpin {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_transfers() -> Result<()> {
    use pdfs_core::control::TransferDirection;
    let response = control_request(CtlRequest::GetQueueStatus)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Transfers { items, jobs } if items.is_empty() && jobs.is_empty() => {
            println!("Nothing in progress.")
        }
        CtlResponse::Transfers { items, jobs } => {
            // Jobs first: they are the context for the transfers under them ("of
            // 40 files", "still scanning") rather than a separate list.
            for j in jobs {
                let count = if j.total > 0 {
                    format!("{} of {}", j.done, j.total)
                } else {
                    "—".to_string()
                };
                let detail = if j.detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", j.detail)
                };
                println!("… {:>9}  {}{detail}", count, j.title);
            }
            for t in items {
                let arrow = match t.direction {
                    TransferDirection::Download => "↓",
                    TransferDirection::Upload => "↑",
                };
                let pct = if t.bytes_total > 0 {
                    format!(
                        "{:.0}%",
                        100.0 * t.bytes_completed as f64 / t.bytes_total as f64
                    )
                } else {
                    "—".to_string()
                };
                let speed = t.speed_bytes_sec as f64 / 1_048_576.0;
                println!(
                    "{arrow} {:>4}  {:>8} / {:>8}  {speed:>6.2} MiB/s  {}",
                    pct, t.bytes_completed, t.bytes_total, t.name
                );
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_pins() -> Result<()> {
    let response = control_request(CtlRequest::ListPins)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Pins { pins } if pins.is_empty() => println!("No pinned files."),
        CtlResponse::Pins { pins } => {
            for p in pins {
                println!("{}", p.path);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_ls(path: Option<PathBuf>) -> Result<()> {
    let path = match path {
        Some(p) => path_arg(&p)?,
        None => String::new(),
    };
    let response = control_request(CtlRequest::ListDir { path })?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Entries { entries } if entries.is_empty() => println!("(empty)"),
        CtlResponse::Entries { entries } => {
            for e in entries {
                let kind = if e.is_dir { "d" } else { "-" };
                let pin = if e.pinned { "*" } else { " " };
                println!("{kind}{pin} {:>12}  {}  [{}]", e.size, e.name, e.uid);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_photos(limit: usize, offset: usize) -> Result<()> {
    match control_request(CtlRequest::PhotosTimeline {
        offset,
        limit,
        kind: None,
        range: None,
    })? {
        CtlResponse::Photos {
            available: false, ..
        } => {
            println!("This account has no photos volume.")
        }
        CtlResponse::Photos { items, .. } if items.is_empty() => println!("No photos."),
        CtlResponse::Photos { items, .. } => {
            // The timeline reply only carries thumbnails already in the cache, so
            // pull the rest of the page's in one batch to print a path per photo.
            let uids: Vec<String> = items.iter().map(|p| p.uid.clone()).collect();
            let thumbs: HashMap<String, Option<String>> =
                match control_request(CtlRequest::PhotoThumbs { uids })? {
                    CtlResponse::Thumbs { items } => {
                        items.into_iter().map(|t| (t.uid, t.path)).collect()
                    }
                    _ => HashMap::new(),
                };
            for p in items {
                let thumb = thumbs
                    .get(&p.uid)
                    .and_then(|p| p.as_deref())
                    .or(p.thumb_path.as_deref())
                    .unwrap_or("(no thumbnail)");
                println!("{}  {}  {thumb}", p.capture_time, p.uid);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_open_photo(uid: String) -> Result<()> {
    match control_request(CtlRequest::OpenPhoto { uid })? {
        CtlResponse::FilePath { path } => println!("{path}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_search(query: String, limit: usize) -> Result<()> {
    match control_request(CtlRequest::Search { query, limit })? {
        CtlResponse::SearchResults { hits } if hits.is_empty() => println!("(no matches)"),
        CtlResponse::SearchResults { hits } => {
            for h in hits {
                let kind = if h.is_dir { "d" } else { "-" };
                let pin = if h.pinned { "*" } else { " " };
                println!("{kind}{pin} {:>12}  {}", h.size, h.path);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_rename(path: PathBuf, new_name: String) -> Result<()> {
    match control_request(CtlRequest::Rename {
        path: path_arg(&path)?,
        new_name,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_move(path: PathBuf, new_parent: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Move {
        path: path_arg(&path)?,
        new_parent: path_arg(&new_parent)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_rm(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Delete {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_trash() -> Result<()> {
    match control_request(CtlRequest::ListTrash)? {
        CtlResponse::Entries { entries } if entries.is_empty() => println!("(trash is empty)"),
        CtlResponse::Entries { entries } => {
            for e in entries {
                let kind = if e.is_dir { "d" } else { "-" };
                println!("{kind} {:>12}  {}  [{}]", e.size, e.name, e.uid);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_restore(uids: Vec<String>) -> Result<()> {
    match control_request(CtlRequest::Restore { uids })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_delete_forever(uids: Vec<String>) -> Result<()> {
    confirm(&format!(
        "Permanently delete {} item(s)? This cannot be undone.",
        uids.len()
    ))?;
    match control_request(CtlRequest::DeleteForever { uids })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_empty_trash() -> Result<()> {
    confirm("Permanently delete everything in the trash? This cannot be undone.")?;
    match control_request(CtlRequest::EmptyTrash)? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// Drop a cached listing. `trash` and `photos` name those two listings; anything
/// else is read as a folder path, so a folder actually called "trash" is still
/// reachable as `./trash`.
fn cmd_refresh(target: Option<String>) -> Result<()> {
    let scope = match target.as_deref() {
        Some("trash") => RefreshScope::Trash,
        Some("photos") => RefreshScope::Photos,
        path => RefreshScope::Dir {
            path: path.unwrap_or("").to_string(),
        },
    };
    match control_request(CtlRequest::Refresh { scope })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
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

fn cmd_mkdir(parent: PathBuf, name: String) -> Result<()> {
    match control_request(CtlRequest::CreateFolder {
        parent: path_arg(&parent)?,
        name,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_upload(sources: Vec<PathBuf>, parent: PathBuf) -> Result<()> {
    // The daemon reads the sources off its own (local) filesystem, so send it
    // absolute, canonicalized paths.
    let mut abs = Vec::with_capacity(sources.len());
    for src in &sources {
        let p = std::fs::canonicalize(src).with_context(|| format!("resolve {}", src.display()))?;
        abs.push(
            p.to_str()
                .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", p.display()))?
                .to_owned(),
        );
    }
    match control_request(CtlRequest::UploadPaths {
        parent: path_arg(&parent)?,
        sources: abs,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
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

fn cmd_cache(action: CacheCmd) -> Result<()> {
    match action {
        CacheCmd::Inspect { deep } => cmd_cache_inspect(deep),
        CacheCmd::Vacuum => match control_request(CtlRequest::CacheVacuum)? {
            CtlResponse::Ok { message } => {
                println!("{message}");
                Ok(())
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        CacheCmd::Clear => match control_request(CtlRequest::PurgeCache)? {
            CtlResponse::Ok { message } => {
                println!("{message}");
                Ok(())
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
    }
}

fn cmd_cache_inspect(deep: bool) -> Result<()> {
    let response = control_request(CtlRequest::CacheInspect { deep })?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::CacheReport {
            schema_version,
            db_bytes,
            db_reclaimable_bytes,
            tables,
            cache_used,
            cache_budget,
            integrity_problems,
            integrity_checked,
        } => {
            println!(
                "Database   schema v{schema_version}, {}",
                human_bytes(db_bytes)
            );
            if db_reclaimable_bytes > 0 {
                println!(
                    "           {} reclaimable — run `pdfs cache vacuum`",
                    human_bytes(db_reclaimable_bytes)
                );
            }
            let budget = if cache_budget == 0 {
                "unlimited".to_string()
            } else {
                human_bytes(cache_budget)
            };
            println!("Content    {} cached of {budget}", human_bytes(cache_used));

            println!("\nRows");
            for (name, count) in tables {
                println!("  {name:<16} {count:>10}");
            }

            if integrity_checked {
                if integrity_problems.is_empty() {
                    println!("\nIntegrity  ok");
                } else {
                    println!("\nIntegrity  {} problem(s):", integrity_problems.len());
                    for problem in integrity_problems {
                        println!("  {problem}");
                    }
                }
            } else {
                println!("\nIntegrity  not checked (pass --deep)");
            }
            Ok(())
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
}

#[derive(Default)]
struct DiagnoseReport {
    warnings: usize,
    failures: usize,
}

impl DiagnoseReport {
    fn finding(&mut self, level: DiagnoseLevel, label: &str, detail: impl AsRef<str>) {
        let mark = match level {
            DiagnoseLevel::Ok => "ok  ",
            DiagnoseLevel::Warn => {
                self.warnings += 1;
                "WARN"
            }
            DiagnoseLevel::Fail => {
                self.failures += 1;
                "FAIL"
            }
        };
        let detail = detail.as_ref();
        if detail.is_empty() {
            println!("[{mark}] {label}");
        } else {
            println!("[{mark}] {label}: {detail}");
        }
    }

    fn check(&mut self, label: &str, ok: bool, detail: impl AsRef<str>) {
        self.finding(
            if ok {
                DiagnoseLevel::Ok
            } else {
                DiagnoseLevel::Fail
            },
            label,
            detail,
        );
    }
}

#[derive(Clone, Copy)]
enum DiagnoseLevel {
    Ok,
    Warn,
    Fail,
}

fn executable_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .any(|dir| std::fs::metadata(dir.join(name)).is_ok_and(|m| m.is_file()))
    })
}

fn mounted_fs_type(path: &Path) -> Option<String> {
    let wanted = path.canonicalize().ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mounts
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let separator = fields.iter().position(|field| *field == "-")?;
            let encoded = *fields.get(4)?;
            let decoded = encoded.replace("\\040", " ").replace("\\011", "\t");
            let mountpoint = PathBuf::from(decoded);
            if mountpoint.canonicalize().ok()? == wanted {
                Some(fields.get(separator + 1)?.to_string())
            } else {
                None
            }
        })
        .next()
}

/// Create, sync, and remove a new file without ever replacing an existing one.
fn writable_probe(dir: &Path) -> std::io::Result<()> {
    let name = format!(".pdfs-diagnose-probe-{}", std::process::id());
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let result = (|| {
        file.write_all(b"probe")?;
        file.sync_all()
    })();
    drop(file);
    let remove = std::fs::remove_file(path);
    result.and(remove)
}

/// Self-check the installation, reporting each finding and exiting non-zero if
/// any check failed.
///
/// Every check is written to work with no daemon running, because that is the
/// situation a user runs this in. A missing daemon is reported as a finding,
/// not an error that aborts the rest of the report.
fn cmd_diagnose() -> Result<()> {
    let mut report = DiagnoseReport::default();

    let dirs = AppDirs::new().context("resolve application directories")?;
    let config = dirs.load_config();

    println!("Paths");
    let state = dirs.state_dir();
    report.check(
        "  state dir",
        state.is_dir(),
        format!("{}", state.display()),
    );
    let cache_dir = dirs.cache_dir();
    report.check(
        "  cache dir",
        cache_dir.is_dir(),
        format!("{}", cache_dir.display()),
    );
    // Writability is checked by actually writing: permission bits do not
    // account for a full disk, a read-only remount, or an immutable flag.
    match writable_probe(&state) {
        Ok(()) => report.finding(
            DiagnoseLevel::Ok,
            "  state dir writable",
            "write and fsync succeeded",
        ),
        Err(e) => report.finding(DiagnoseLevel::Fail, "  state dir writable", e.to_string()),
    }

    let db_path = dirs.db_path();
    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    report.check(
        "  database",
        db_path.is_file(),
        format!("{} ({})", db_path.display(), human_bytes(db_size)),
    );

    println!("\nFUSE");
    let fuse = Path::new("/dev/fuse");
    report.check("  /dev/fuse", fuse.exists(), fuse.display().to_string());
    let has_fusermount = executable_on_path("fusermount3") || executable_on_path("fusermount");
    report.check(
        "  unmount helper",
        has_fusermount,
        "fusermount3 or fusermount on PATH",
    );

    println!("\nAccount");
    match pdfs_core::auth::load() {
        Ok(session) => {
            report.finding(DiagnoseLevel::Ok, "  keyring session", session.username);
        }
        Err(e) => {
            // A locked keyring and an absent login look the same from here, so
            // the detail carries the distinction rather than the verdict.
            report.finding(DiagnoseLevel::Fail, "  keyring session", format!("{e}"));
        }
    }

    println!("\nDaemon");
    let socket = dirs.control_socket();
    let socket_exists = socket.exists();
    let socket_kind_ok =
        std::fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket());
    report.finding(
        if socket_exists && !socket_kind_ok {
            DiagnoseLevel::Fail
        } else if socket_exists {
            DiagnoseLevel::Ok
        } else {
            DiagnoseLevel::Warn
        },
        "  control socket",
        socket.display().to_string(),
    );
    // A stale socket file with no listener is the common failure, so the reply
    // is what decides this rather than the file's existence.
    match control_request(CtlRequest::Status) {
        Ok(CtlResponse::Status {
            mountpoint,
            online,
            pending_uploads,
            pending_changes,
            ..
        }) => {
            report.finding(DiagnoseLevel::Ok, "  daemon responding", "");
            let live_type = mounted_fs_type(Path::new(&mountpoint));
            let is_fuse = live_type
                .as_deref()
                .is_some_and(|kind| kind.starts_with("fuse"));
            report.finding(
                if is_fuse {
                    DiagnoseLevel::Ok
                } else {
                    DiagnoseLevel::Fail
                },
                "  active mount",
                live_type.map_or_else(
                    || format!("{mountpoint} is not mounted"),
                    |kind| format!("{mountpoint} ({kind})"),
                ),
            );
            report.finding(
                if online {
                    DiagnoseLevel::Ok
                } else {
                    DiagnoseLevel::Warn
                },
                "  network",
                if online {
                    "online"
                } else {
                    "offline; cached data remains available"
                },
            );
            let pending = pending_uploads + pending_changes;
            report.finding(
                if pending == 0 {
                    DiagnoseLevel::Ok
                } else {
                    DiagnoseLevel::Warn
                },
                "  queued writes",
                if pending == 0 {
                    "none".to_string()
                } else {
                    format!("{pending} waiting to upload")
                },
            );
            match control_request(CtlRequest::CacheInspect { deep: true }) {
                Ok(CtlResponse::CacheReport {
                    integrity_problems, ..
                }) if integrity_problems.is_empty() => report.finding(
                    DiagnoseLevel::Ok,
                    "  database integrity",
                    "SQLite integrity_check passed",
                ),
                Ok(CtlResponse::CacheReport {
                    integrity_problems, ..
                }) => report.finding(
                    DiagnoseLevel::Fail,
                    "  database integrity",
                    format!(
                        "{} problem(s): {}",
                        integrity_problems.len(),
                        integrity_problems.join("; ")
                    ),
                ),
                Ok(other) => report.finding(
                    DiagnoseLevel::Fail,
                    "  database integrity",
                    format!("unexpected: {other:?}"),
                ),
                Err(e) => report.finding(
                    DiagnoseLevel::Fail,
                    "  database integrity",
                    format!("check failed: {e:#}"),
                ),
            }
        }
        Ok(other) => {
            report.finding(
                DiagnoseLevel::Fail,
                "  daemon responding",
                format!("unexpected: {other:?}"),
            );
        }
        Err(e) => {
            // Not a failure of the installation: a stopped daemon is a normal
            // state, and the rest of the report is still worth printing.
            report.finding(
                DiagnoseLevel::Warn,
                "  daemon responding",
                format!("stopped or unreachable: {e:#}"),
            );
            println!("         (start it with `systemctl --user start proton-drive.service`)");
        }
    }

    let mountpoint = dirs.resolved_mountpoint(&config);
    report.finding(
        if mountpoint.is_dir() {
            DiagnoseLevel::Ok
        } else {
            DiagnoseLevel::Warn
        },
        "  configured mountpoint",
        mountpoint.display().to_string(),
    );

    if report.failures == 0 {
        if report.warnings == 0 {
            println!("\nNo problems found.");
        } else {
            println!(
                "\nNo critical problems found; {} warning(s) need attention.",
                report.warnings
            );
        }
        Ok(())
    } else {
        bail!(
            "{} check(s) failed and {} warning(s) were reported; see above",
            report.failures,
            report.warnings
        )
    }
}

/// Send one request to the running mount daemon's control socket and read its
/// reply. Errors if no daemon is listening.
fn control_request(req: CtlRequest) -> Result<CtlResponse> {
    let socket = AppDirs::new()?.control_socket();
    pdfs_core::control::send(&socket, &req)
        .with_context(|| format!("talk to mount daemon at {}", socket.display()))
}

#[cfg(test)]
mod diagnose_tests {
    use super::*;

    #[test]
    fn finding_severity_is_counted_centrally() {
        let mut report = DiagnoseReport::default();
        report.finding(DiagnoseLevel::Ok, "ok", "");
        report.finding(DiagnoseLevel::Warn, "warn", "");
        report.finding(DiagnoseLevel::Fail, "fail", "");
        assert_eq!(report.warnings, 1);
        assert_eq!(report.failures, 1);
    }

    #[test]
    fn writable_probe_never_replaces_an_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-diagnose-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let collision = dir.join(format!(".pdfs-diagnose-probe-{}", std::process::id()));
        std::fs::write(&collision, b"keep me").unwrap();

        assert_eq!(
            writable_probe(&dir).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&collision).unwrap(), b"keep me");

        std::fs::remove_file(collision).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
