//! File-manager integration: right-click actions inside the mounted Drive.
//!
//! Nautilus (and Nemo/Caja, which read the same directories) runs any executable
//! in its scripts folder with the selected paths in the environment, and shows it
//! under a "Scripts" submenu. That gives us pin/unpin from the file manager
//! without shipping a Python extension or a GNOME-version-specific plugin.
//!
//! Installation is idempotent and best-effort: the scripts are (re)written on
//! every app start so an upgraded `pdfs` never leaves a stale script behind, and
//! any failure is logged rather than surfaced — a missing file manager is not an
//! error the user needs to act on.

use std::io;
use std::path::PathBuf;

/// Scripts shipped into the file manager: `(file name, `pdfs` subcommand)`.
const SCRIPTS: [(&str, &str); 2] = [
    ("Keep offline (Proton Drive)", "pin"),
    ("Remove offline copy (Proton Drive)", "unpin"),
];

/// The script body. Nautilus passes the selection as newline-separated paths in
/// `NAUTILUS_SCRIPT_SELECTED_FILE_PATHS`; the positional args carry the same
/// selection when the script runs on a local (non-URI) folder, so we prefer the
/// environment and fall back to `"$@"` for the Nemo/Caja variants.
fn script_body(subcommand: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Installed by proton-drive-linux. Regenerated on every app start.\n\
         set -eu\n\
         \n\
         paths=\"${{NAUTILUS_SCRIPT_SELECTED_FILE_PATHS:-}}\"\n\
         if [ -z \"$paths\" ]; then\n\
         \x20   paths=\"${{NEMO_SCRIPT_SELECTED_FILE_PATHS:-}}\"\n\
         fi\n\
         if [ -z \"$paths\" ]; then\n\
         \x20   for arg in \"$@\"; do\n\
         \x20       paths=\"$paths$arg\\n\"\n\
         \x20   done\n\
         fi\n\
         \n\
         printf '%s' \"$paths\" | while IFS= read -r path; do\n\
         \x20   [ -n \"$path\" ] || continue\n\
         \x20   pdfs {subcommand} \"$path\" || exit 1\n\
         done\n"
    )
}

/// The scripts directories to populate, for whichever file managers are present.
fn script_dirs() -> Vec<PathBuf> {
    let Some(data) = dirs_data_home() else {
        return Vec::new();
    };
    ["nautilus/scripts", "nemo/scripts"]
        .iter()
        .map(|rel| data.join(rel))
        .collect()
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
fn dirs_data_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
}

/// Install (or refresh) the pin/unpin scripts. Best-effort: logs and moves on.
pub fn install_file_manager_scripts() {
    for dir in script_dirs() {
        // Only populate a file manager the user actually has: creating
        // `~/.local/share/nemo` on a GNOME box would be litter.
        if !dir.parent().is_some_and(|p| p.exists()) {
            continue;
        }
        if let Err(e) = write_scripts(&dir) {
            tracing::warn!("couldn't install file-manager scripts in {dir:?}: {e}");
        }
    }
}

/// Write both scripts into `dir`, creating it if needed, and make them
/// executable — a script without the exec bit is silently ignored by Nautilus.
fn write_scripts(dir: &PathBuf) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, subcommand) in SCRIPTS {
        let path = dir.join(name);
        std::fs::write(&path, script_body(subcommand))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    tracing::debug!("installed file-manager scripts in {dir:?}");
    Ok(())
}
