//! The tray's own status icons, drawn from the app icon's folder.
//!
//! The packages install them into the hicolor theme. The tray also writes
//! them to a directory of its own at start and names it as the item's
//! `IconThemePath`, so a build run from source, or a panel that does not
//! read the system theme again after an update, still finds them.

use std::path::Path;

macro_rules! icon {
    ($state:literal) => {
        (
            concat!("io.narl.proton-drive-linux-tray-", $state, "-symbolic"),
            include_str!(concat!(
                "../resources/icons/tray/io.narl.proton-drive-linux-tray-",
                $state,
                "-symbolic.svg"
            )),
        )
    };
}

/// Every icon, by name, with its SVG.
pub const ICONS: [(&str, &str); 7] = [
    icon!("synced"),
    icon!("syncing"),
    icon!("paused"),
    icon!("offline"),
    icon!("attention"),
    icon!("session-expired"),
    icon!("disconnected"),
];

/// A theme index just big enough for one scalable status directory.
const INDEX: &str = "[Icon Theme]\nName=Hicolor\nComment=Proton Drive tray icons\n\
Directories=scalable/status\n\n[scalable/status]\nSize=16\nMinSize=8\nMaxSize=512\n\
Type=Scalable\nContext=Status\n";

/// Write the icons under `base`: as a hicolor theme, which KDE looks for in
/// an `IconThemePath`, and loose, which GTK hosts also accept. A file that
/// already holds the right bytes is left alone.
pub fn install(base: &Path) -> std::io::Result<()> {
    let status = base.join("hicolor/scalable/status");
    std::fs::create_dir_all(&status)?;
    write_if_changed(&base.join("hicolor/index.theme"), INDEX)?;
    for (name, svg) in ICONS {
        let file = format!("{name}.svg");
        write_if_changed(&status.join(&file), svg)?;
        write_if_changed(&base.join(&file), svg)?;
    }
    Ok(())
}

fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|have| have == contents.as_bytes()) {
        return Ok(());
    }
    std::fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_one_recolourable_path() {
        for (name, svg) in ICONS {
            assert!(name.ends_with("-symbolic"), "{name}");
            assert_eq!(svg.matches("<path").count(), 1, "{name}");
            for banned in ["stroke", "mask", "opacity", "<circle", "<rect"] {
                assert!(!svg.contains(banned), "{name} has {banned}");
            }
        }
    }

    #[test]
    fn install_writes_a_theme_and_loose_copies() {
        let dir = std::env::temp_dir().join(format!("pdfs-tray-icons-{}", std::process::id()));
        install(&dir).unwrap();
        install(&dir).unwrap();
        assert!(dir.join("hicolor/index.theme").is_file());
        for (name, svg) in ICONS {
            let themed = dir.join(format!("hicolor/scalable/status/{name}.svg"));
            assert_eq!(std::fs::read_to_string(themed).unwrap(), svg);
            assert!(dir.join(format!("{name}.svg")).is_file());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
