//! `pdfs` — command-line front-end for the Proton Drive Linux client.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pdfs_core::auth;
use pdfs_core::config::AppDirs;

#[derive(Parser)]
#[command(name = "pdfs", version, about = "Proton Drive for Linux (Files On-Demand)")]
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
    /// Show the currently logged-in account.
    Status,
    /// Mount Proton Drive at the given (or default) path. Blocks until unmounted.
    Mount {
        /// Mountpoint; defaults to ~/ProtonDrive.
        mountpoint: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Login { username } => cmd_login(username),
        Command::Logout => cmd_logout(),
        Command::Status => cmd_status(),
        Command::Mount { mountpoint } => cmd_mount(mountpoint),
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
        prompt_line("2FA code")
            .map_err(|e| pdfs_core::Error::Other(format!("read 2FA code: {e}")))
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
        Err(pdfs_core::Error::NotLoggedIn) => println!("Not logged in. Run `pdfs login`."),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn cmd_mount(mountpoint: Option<PathBuf>) -> Result<()> {
    let dirs = AppDirs::new()?;
    dirs.ensure()?;
    let mountpoint = mountpoint.unwrap_or_else(|| dirs.default_mountpoint());
    std::fs::create_dir_all(&mountpoint)
        .with_context(|| format!("create mountpoint {}", mountpoint.display()))?;

    // Multi-threaded runtime: its worker threads keep servicing async SDK calls
    // while the main thread is parked inside the blocking FUSE session loop.
    let rt = tokio::runtime::Runtime::new()?;
    let (client, _session) = rt
        .block_on(auth::resume_client())
        .context("resume session (run `pdfs login` first)")?;

    let handle = rt.handle().clone();
    pdfs_fuse::mount(client, handle, &mountpoint).context("mount failed")?;
    Ok(())
}
