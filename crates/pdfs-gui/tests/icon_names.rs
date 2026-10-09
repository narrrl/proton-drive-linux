//! Every symbolic icon the app window and the search prompt ask for must
//! exist on a stock GNOME desktop.
//!
//! An icon name that no theme ships shows up as a missing-image glyph, and
//! nothing fails until someone looks. So each `"…-symbolic"` literal in the
//! sources has to be either one of the `pdfs-*` icons bundled in
//! `resources/icons` (and listed in the gresource manifest), or on
//! [`STOCK`], a list of names checked against Adwaita 50, libadwaita and
//! GTK's own built-in icons.
//!
//! The tray is left out: its icons are drawn by the panel, from the panel's
//! theme, not from our resources.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Stock symbolic icons the app may use. Before adding a name, check that
/// Adwaita has it outside its `legacy` folder, or that libadwaita or GTK
/// ship it themselves.
const STOCK: &[&str] = &[
    // libadwaita
    "adw-external-link-symbolic",
    // GTK's built-in icons
    "info-outline-symbolic",
    // Adwaita
    "checkbox-checked-symbolic",
    "checkbox-symbolic",
    "computer-symbolic",
    "dialog-error-symbolic",
    "dialog-password-symbolic",
    "dialog-warning-symbolic",
    "document-edit-symbolic",
    "document-open-recent-symbolic",
    "document-open-symbolic",
    "document-save-symbolic",
    "drive-harddisk-symbolic",
    "edit-copy-symbolic",
    "edit-delete-symbolic",
    "edit-find-symbolic",
    "edit-undo-symbolic",
    "folder-download-symbolic",
    "folder-new-symbolic",
    "folder-open-symbolic",
    "folder-remote-symbolic",
    "folder-symbolic",
    "go-down-symbolic",
    "go-jump-symbolic",
    "go-next-symbolic",
    "go-previous-symbolic",
    "go-up-symbolic",
    "image-missing-symbolic",
    "image-x-generic-symbolic",
    "list-add-symbolic",
    "mark-location-symbolic",
    "media-playback-pause-symbolic",
    "media-playback-start-symbolic",
    "network-idle-symbolic",
    "network-offline-symbolic",
    "non-starred-symbolic",
    "object-select-symbolic",
    "preferences-system-symbolic",
    "process-stop-symbolic",
    "starred-symbolic",
    "system-search-symbolic",
    "text-x-generic-symbolic",
    "user-trash-symbolic",
    "view-grid-symbolic",
    "view-list-symbolic",
    "view-more-symbolic",
    "view-paged-symbolic",
    "view-pin-symbolic",
    "view-refresh-symbolic",
    "window-close-symbolic",
    "zoom-fit-best-symbolic",
    "zoom-in-symbolic",
    "zoom-out-symbolic",
];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `"name-symbolic"` string literal in `source`. Found by suffix rather
/// than by pairing quotes, so an escaped quote or a `'"'` elsewhere in the
/// file can't hide one.
fn symbolic_literals(source: &str) -> Vec<String> {
    const SUFFIX: &str = "-symbolic\"";
    let is_name = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    source
        .match_indices(SUFFIX)
        .filter_map(|(at, _)| {
            let head = &source[..at];
            let start = head.trim_end_matches(is_name).len();
            (start > 0 && head[..start].ends_with('"'))
                .then(|| format!("{}-symbolic", &head[start..]))
        })
        .collect()
}

/// The `pdfs-*` icons in `resources/icons`, by name.
fn bundled() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let root = crate_dir().join("resources/icons/scalable");
    for context in fs::read_dir(root).unwrap() {
        for icon in fs::read_dir(context.unwrap().path()).unwrap() {
            let path = icon.unwrap().path();
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                names.insert(name.to_string());
            }
        }
    }
    names
}

#[test]
fn every_symbolic_icon_exists_on_a_stock_desktop() {
    let mut files = vec![crate_dir().join("src/prompt.rs")];
    rust_files(&crate_dir().join("src/app"), &mut files);
    let bundled = bundled();
    let mut unknown = BTreeSet::new();
    for file in files {
        let source = fs::read_to_string(&file).unwrap();
        for name in symbolic_literals(&source) {
            if !bundled.contains(&name) && !STOCK.contains(&name.as_str()) {
                let file = file
                    .strip_prefix(crate_dir())
                    .unwrap()
                    .display()
                    .to_string();
                unknown.insert(format!("{name} ({file})"));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "icons that are neither bundled nor on the stock list:\n  {}",
        unknown.into_iter().collect::<Vec<_>>().join("\n  ")
    );
}

#[test]
fn every_bundled_icon_is_in_the_resource_manifest() {
    let manifest = fs::read_to_string(crate_dir().join("resources/pdfs.gresource.xml")).unwrap();
    let missing: Vec<String> = bundled()
        .into_iter()
        .filter(|name| name.starts_with("pdfs-") && !manifest.contains(&format!("/{name}.svg<")))
        .collect();
    assert!(missing.is_empty(), "not in pdfs.gresource.xml: {missing:?}");
}
