//! `pdfs` — command-line front-end for the Proton Drive Linux client.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use pdfs_core::auth;
use pdfs_core::cache::ContentCache;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    RefreshScope, Request as CtlRequest, Response as CtlResponse, ShareEntryKind, SyncPhase,
    pending_summary,
};
use pdfs_core::db::Db;

#[derive(Parser)]
#[command(
    name = "pdfs",
    version,
    about = "Proton Drive for Linux (Files On-Demand)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
    /// List nodes shared with me that I have accepted.
    SharedWithMe,
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
        Command::SharedWithMe => cmd_shared_with_me(),
        Command::Leave { uid } => cmd_leave(uid),
        Command::Invitations { action } => cmd_invitations(action),
        Command::Bookmarks { action } => cmd_bookmarks(action),
        Command::Activity { limit } => cmd_activity(limit),
    }
}

fn cmd_devices(action: DeviceCmd) -> Result<()> {
    match action {
        DeviceCmd::List => match control_request(CtlRequest::ListDevices)? {
            CtlResponse::Devices { items } if items.is_empty() => println!("No devices."),
            CtlResponse::Devices { items } => {
                for d in items {
                    let sync = d
                        .last_sync
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "never".to_string());
                    println!(
                        "{}  {}  (synced: {sync})  [{}]",
                        d.device_type, d.name, d.uid
                    );
                }
            }
            CtlResponse::Error { message } => bail!("{message}"),
            other => bail!("unexpected response: {other:?}"),
        },
        DeviceCmd::Rename { uid, name } => {
            ok_or_bail(control_request(CtlRequest::RenameDevice { uid, name })?)?
        }
        DeviceCmd::Rm { uid } => ok_or_bail(control_request(CtlRequest::DeleteDevice { uid })?)?,
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
        SyncCmd::List => match control_request(CtlRequest::ListSyncFolders)? {
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
                                println!("      scanning: {} of {}", p.done, p.total.max(p.done))
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
            CtlResponse::Error { message } => bail!("{message}"),
            other => bail!("unexpected response: {other:?}"),
        },
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
    }
    Ok(())
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
        CtlResponse::Error { message } => bail!("{message}"),
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
            CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_shared_with_me() -> Result<()> {
    match control_request(CtlRequest::ListSharedWithMe)? {
        CtlResponse::Entries { entries } if entries.is_empty() => {
            println!("Nothing shared with you.")
        }
        CtlResponse::Entries { entries } => {
            for e in entries {
                let kind = if e.is_dir { "d" } else { "-" };
                println!("{kind} {:>12}  {}  [{}]", e.size, e.name, e.uid);
            }
        }
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_activity(limit: usize) -> Result<()> {
    match control_request(CtlRequest::ListActivity { limit })? {
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
        CtlResponse::Error { message } => bail!("{message}"),
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
            CtlResponse::Error { message } => bail!("{message}"),
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
            CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
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
    rt.block_on(auth::login(&username, &password, get_totp))
        .context("login failed")?;

    println!("Logged in as {username}. Session stored in the system keyring.");
    Ok(())
}

fn cmd_logout() -> Result<()> {
    auth::logout()?;
    println!("Stored session cleared.");
    Ok(())
}

fn cmd_status() -> Result<()> {
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
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_unpin(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Unpin {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_transfers() -> Result<()> {
    use pdfs_core::control::TransferDirection;
    match control_request(CtlRequest::GetQueueStatus)? {
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
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_pins() -> Result<()> {
    match control_request(CtlRequest::ListPins)? {
        CtlResponse::Pins { pins } if pins.is_empty() => println!("No pinned files."),
        CtlResponse::Pins { pins } => {
            for p in pins {
                println!("{}", p.path);
            }
        }
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_ls(path: Option<PathBuf>) -> Result<()> {
    let path = match path {
        Some(p) => path_arg(&p)?,
        None => String::new(),
    };
    match control_request(CtlRequest::ListDir { path })? {
        CtlResponse::Entries { entries } if entries.is_empty() => println!("(empty)"),
        CtlResponse::Entries { entries } => {
            for e in entries {
                let kind = if e.is_dir { "d" } else { "-" };
                let pin = if e.pinned { "*" } else { " " };
                println!("{kind}{pin} {:>12}  {}  [{}]", e.size, e.name, e.uid);
            }
        }
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_open_photo(uid: String) -> Result<()> {
    match control_request(CtlRequest::OpenPhoto { uid })? {
        CtlResponse::FilePath { path } => println!("{path}"),
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_rm(path: PathBuf) -> Result<()> {
    match control_request(CtlRequest::Delete {
        path: path_arg(&path)?,
    })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_restore(uids: Vec<String>) -> Result<()> {
    match control_request(CtlRequest::Restore { uids })? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_empty_trash() -> Result<()> {
    confirm("Permanently delete everything in the trash? This cannot be undone.")?;
    match control_request(CtlRequest::EmptyTrash)? {
        CtlResponse::Ok { message } => println!("{message}"),
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
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
        CtlResponse::Error { message } => bail!("{message}"),
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

/// Send one request to the running mount daemon's control socket and read its
/// reply. Errors if no daemon is listening.
fn control_request(req: CtlRequest) -> Result<CtlResponse> {
    let socket = AppDirs::new()?.control_socket();
    pdfs_core::control::send(&socket, &req)
        .with_context(|| format!("talk to mount daemon at {}", socket.display()))
}
