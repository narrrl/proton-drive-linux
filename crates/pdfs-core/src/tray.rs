//! Talking to a running `pdfs-tray`.
//!
//! The tray binds [`AppDirs::tray_socket`] to keep a single instance. The same
//! socket takes one command, [`QUIT`], so the desktop app can take the icon
//! away when the user hides it or stops Proton Drive.

use std::io::Write;
use std::os::unix::net::UnixStream;

use crate::config::AppDirs;

/// The line that asks the tray to exit.
pub const QUIT: &str = "quit";

/// Ask a running tray to exit. False when no tray answers.
pub fn quit(dirs: &AppDirs) -> bool {
    UnixStream::connect(dirs.tray_socket())
        .and_then(|mut stream| stream.write_all(format!("{QUIT}\n").as_bytes()))
        .is_ok()
}
