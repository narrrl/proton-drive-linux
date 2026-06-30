//! `pdfs` — command-line front-end for the Proton Drive Linux client.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use pdfs_core::auth;
use pdfs_core::cache::ContentCache;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{Request as CtlRequest, Response as CtlResponse};

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
        Command::Pin { path } => cmd_pin(path),
        Command::Unpin { path } => cmd_unpin(path),
        Command::Pins => cmd_pins(),
        Command::Ls { path } => cmd_ls(path),
        Command::Photos { limit, offset } => cmd_photos(limit, offset),
        Command::OpenPhoto { uid } => cmd_open_photo(uid),
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
    let dirs = AppDirs::new()?;
    dirs.ensure()?;
    let mountpoint = mountpoint.unwrap_or_else(|| dirs.default_mountpoint());
    std::fs::create_dir_all(&mountpoint)
        .with_context(|| format!("create mountpoint {}", mountpoint.display()))?;

    let username = auth::load().map(|s| s.username).unwrap_or_default();
    let cache = ContentCache::open(
        dirs.content_cache_dir(),
        dirs.pins_path(),
        pdfs_core::config::DEFAULT_CACHE_BUDGET_BYTES,
    )
    .context("open content cache")?;
    let control_socket = dirs.control_socket();

    // Multi-threaded runtime: its worker threads keep servicing async SDK calls
    // while the main thread is parked inside the blocking FUSE session loop.
    let rt = tokio::runtime::Runtime::new()?;
    let (client, session) = rt
        .block_on(auth::resume_client())
        .context("resume session (run `pdfs login` first)")?;

    // Persist rotated tokens for the daemon's lifetime. Proton refresh tokens are
    // single-use: the 401-refresh path swaps in a new access+refresh pair, and if
    // that pair is never written back the keyring keeps a now-dead refresh token,
    // so the next `pdfs mount` fails with `InvalidRefreshToken`. The session
    // shares its token store with the Drive client (one `Arc`-backed token store),
    // so both the poll below and the shutdown flush see every rotation.
    //
    // `ProtonApiSession` is `Clone` over that shared store, so the polling task
    // gets its own handle and we keep `session` here for a final flush. The poll
    // is a backstop for long-lived mounts; the flush after `mount` returns is what
    // makes the common case correct — a refresh shortly before a clean unmount
    // would otherwise be lost in the poll gap, leaving a dead token behind.
    let poll_session = session.clone();
    rt.spawn(async move {
        let mut last = poll_session.current_tokens().await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let current = poll_session.current_tokens().await;
            if current.access_token == last.access_token
                && current.refresh_token == last.refresh_token
            {
                continue;
            }
            match auth::persist(&poll_session).await {
                Ok(()) => {
                    tracing::info!("persisted refreshed session tokens");
                    last = current;
                }
                Err(e) => tracing::warn!(error = %e, "failed to persist refreshed tokens"),
            }
        }
    });

    let handle = rt.handle().clone();
    let result = pdfs_fuse::mount(
        client,
        handle,
        &mountpoint,
        cache,
        &control_socket,
        username,
    );

    // Clean unmount: flush whatever tokens the store holds now so the keyring
    // always ends a session with the live refresh token, not a stale one. Runs
    // even if `mount` errored — a rotation may still have happened first.
    if let Err(e) = rt.block_on(auth::persist(&session)) {
        tracing::warn!(error = %e, "failed to persist tokens on shutdown");
    }

    result.context("mount failed")?;
    Ok(())
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
