//! `pdfs` — command-line front-end for the Proton Drive Linux client.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use pdfs_core::auth;
use pdfs_core::cache::ContentCache;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{Request as CtlRequest, Response as CtlResponse};
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
    /// Upload a local file into a Drive folder via the running daemon.
    Upload {
        /// Local file to upload.
        file: PathBuf,
        /// Destination folder path, inside the mountpoint or relative to it.
        parent: PathBuf,
    },
    /// Show the daemon's in-flight transfers (active uploads/downloads).
    Transfers,
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
        Command::Upload { file, parent } => cmd_upload(file, parent),
        Command::Transfers => cmd_transfers(),
    }
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
            mountpoint, pinned, ..
        }) => {
            println!("Mounted at {mountpoint} ({pinned} pinned)");
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
                tracing::error!(error = %e, "mount failed; retrying in 5s");
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
        CtlResponse::Transfers { items } if items.is_empty() => println!("No active transfers."),
        CtlResponse::Transfers { items } => {
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
    match control_request(CtlRequest::PhotosTimeline { offset, limit })? {
        CtlResponse::Photos {
            available: false, ..
        } => {
            println!("This account has no photos volume.")
        }
        CtlResponse::Photos { items, .. } if items.is_empty() => println!("No photos."),
        CtlResponse::Photos { items, .. } => {
            for p in items {
                let thumb = p.thumb_path.as_deref().unwrap_or("(no thumbnail)");
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

fn cmd_upload(file: PathBuf, parent: PathBuf) -> Result<()> {
    let bytes = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("source file has no valid name"))?
        .to_owned();
    match control_request(CtlRequest::UploadFile {
        parent: path_arg(&parent)?,
        name,
        bytes,
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
