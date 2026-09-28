//! `pdfs-prompt` — the launcher: one search field over both Proton Drive and the
//! files on this machine, styled after Google Drive's search overlay.
//!
//! Bind it to a system shortcut (e.g. in Hyprland) for a quick HUD search. The
//! application is single-instance and keeps its window alive between summons,
//! so repeat activations only reset and present the existing widget tree. Every
//! daemon round-trip runs on a worker thread and lands back through an async
//! channel. Drive and local lookup share one request so a result set paints once.
//!
//! This file holds the command line, the result model and the ranking shared by
//! every front end. The GTK window lives in [`hud`].

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::glib;

use pdfs_core::config::AppDirs;
use pdfs_core::control::{LocalHit, Request, Response, SearchHit, SearchKind, send};
use pdfs_core::menu::PromptMode;

mod activation;
mod compat;
// Only the relative and full forms are used here; the short date is the app's.
#[allow(dead_code)]
#[path = "app/dates.rs"]
mod dates;
mod hud;
mod i18n;
mod theme;
use i18n::{gettext, gettext_f, gettext_noop, human_bytes, ngettext_f, pgettext};
mod dmenu;
mod fzf;
mod query;
use activation::mounted_or_relative;

const APP_ID: &str = "io.narl.proton-drive-linux-prompt";

/// Debounce before a keystroke turns into a daemon round-trip. Short enough to
/// feel live, long enough that typing a word is one search, not five.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(80);

/// Per-section result cap. The list is a launcher, not a file manager: more rows
/// than fit on screen only cost render time.
const SEARCH_LIMIT: usize = 20;

/// Which file kinds a chip narrows the results to. Applied client-side to hits
/// the daemon already returned, so switching chips never re-queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Folders,
    Documents,
    Images,
    Media,
}

/// The chip row, in Tab-cycle order.
const FILTERS: [(Filter, &str); 5] = [
    // Translators: a filter chip that shows every kind of result.
    (Filter::All, gettext_noop("All")),
    (Filter::Folders, gettext_noop("Folders")),
    (Filter::Documents, gettext_noop("Documents")),
    (Filter::Images, gettext_noop("Images")),
    // Translators: a filter chip for audio and video files.
    (Filter::Media, gettext_noop("Media")),
];

impl Filter {
    /// Whether a hit of this name/kind survives the chip. The daemon's own
    /// classification, so a chip never drops a hit the daemon returned for it.
    fn accepts(self, name: &str, is_dir: bool) -> bool {
        self.search_kind().accepts(name, is_dir)
    }

    fn search_kind(self) -> SearchKind {
        match self {
            Self::All => SearchKind::All,
            Self::Folders => SearchKind::Folders,
            Self::Documents => SearchKind::Documents,
            Self::Images => SearchKind::Images,
            Self::Media => SearchKind::Media,
        }
    }
}

fn extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

fn is_document(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        "pdf"
            | "doc"
            | "docx"
            | "odt"
            | "rtf"
            | "txt"
            | "md"
            | "xls"
            | "xlsx"
            | "ods"
            | "csv"
            | "ppt"
            | "pptx"
            | "odp"
            | "epub"
    )
}

fn is_image(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" | "avif" | "heic" | "tiff"
    )
}

fn is_media(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a"
    )
}

/// One row in the unified result list. The two sections hold different payloads
/// — a Drive hit must be hydrated through the daemon before it can be opened, a
/// local file is already on disk — but they share one keyboard cursor.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum Hit {
    Drive(SearchHit),
    Local(LocalHit),
}

impl Hit {
    fn name(&self) -> &str {
        match self {
            Hit::Drive(h) => &h.name,
            Hit::Local(h) => &h.name,
        }
    }

    fn is_dir(&self) -> bool {
        match self {
            Hit::Drive(h) => h.is_dir,
            Hit::Local(h) => h.is_dir,
        }
    }

    /// The dimmed second line: the containing folder, as the user thinks of it.
    ///
    /// A Drive hit is only "My files" when it really sits in the primary mount.
    /// A node inside a device folder resolves to a path on this machine
    /// (`~/Downloads/…`) — the daemon has already worked that out in
    /// `mounted_path`, and saying "My files / Downloads" for it names a folder
    /// the user cannot find and does not match where activating the row opens.
    fn location(&self) -> String {
        match self {
            Hit::Drive(h) => match h.mounted_path.as_deref() {
                Some(mounted) => home_relative(&parent_of(mounted)),
                None => {
                    let parent = parent_of(&h.path);
                    if parent.is_empty() {
                        gettext("My files")
                    } else {
                        // Translators: {path} is a folder path inside the user's Drive, such as "Work / Taxes".
                        gettext_f(
                            "My files / {path}",
                            &[("path", &parent.replace('/', " / "))],
                        )
                    }
                }
            },
            Hit::Local(h) => home_relative(&parent_of(&h.path)),
        }
    }

    fn size(&self) -> u64 {
        match self {
            Hit::Drive(h) => h.size,
            Hit::Local(h) => h.size,
        }
    }

    fn modified(&self) -> i64 {
        match self {
            Hit::Drive(h) => h.modified,
            Hit::Local(h) => h.modified,
        }
    }

    fn pinned(&self) -> bool {
        matches!(self, Hit::Drive(h) if h.pinned)
    }

    fn score(&self) -> i64 {
        match self {
            Self::Drive(hit) => hit.score,
            Self::Local(hit) => hit.score,
        }
    }

    /// Stable identity used to preserve the keyboard cursor across re-renders.
    /// The bool keeps a Drive and a local hit at the same path distinct.
    fn key(&self) -> (bool, String) {
        match self {
            Hit::Drive(h) => (false, h.path.clone()),
            Hit::Local(h) => (true, h.path.clone()),
        }
    }

    /// The thumbnail cache key. The modification time is part of it, so a
    /// replaced image never shows its previous contents.
    #[allow(dead_code)] // the GTK launcher only
    fn thumb_key(&self) -> String {
        match self {
            Hit::Drive(h) => format!("d:{}:{}", h.uid, h.modified),
            Hit::Local(h) => format!("l:{}:{}", h.path, h.modified),
        }
    }

    /// Where the file sits on this machine: the path itself for a local hit,
    /// the mounted path for a Drive hit. Nothing is checked on disk, so this
    /// is safe to call while a mount is frozen.
    #[allow(dead_code)] // the GTK launcher only
    fn fs_path(&self, mountpoint: &Path) -> PathBuf {
        match self {
            Hit::Drive(h) => mounted_or_relative(mountpoint, h),
            Hit::Local(h) => PathBuf::from(&h.path),
        }
    }
}

fn rank_hits(hits: &mut [Hit]) {
    rank_hits_by(hits, |_| 0);
}

/// [`rank_hits`] with an extra score per hit, such as the launcher's history
/// bonus for files the user opens often.
fn rank_hits_by(hits: &mut [Hit], bonus: impl Fn(&Hit) -> i64) {
    hits.sort_by_cached_key(|hit| {
        (
            std::cmp::Reverse(hit.score().saturating_add(bonus(hit))),
            hit.name().to_string(),
            hit.key(),
        )
    });
}

const USAGE: &str = "\
pdfs-prompt — search Proton Drive and this computer

Usage: pdfs-prompt [OPTIONS]

Options:
  --dmenu               Present the results in an external launcher (fuzzel,
                        rofi, …) instead of the built-in GTK window. Also
                        settable permanently as \"prompt\": { \"mode\": \"dmenu\" }
                        in config.json.
  --fzf                 Search in fzf, in a terminal, re-querying the daemon on
                        every keystroke — results appear as you type, unlike
                        --dmenu. Also settable as \"prompt\": { \"mode\": \"fzf\" }.
  --gtk                 Force the built-in window, overriding that setting.
  --preload             Start the built-in window hidden and stay resident, so
                        the first summon opens at once. For session autostart;
                        does nothing if the prompt is already running.
  --menu <COMMAND>      Launcher command line for --dmenu, e.g.
                        --menu 'fuzzel --dmenu --width 60'. Overrides
                        \"prompt\": { \"menu\": [...] }.
  --query <TEXT>        Search for TEXT immediately instead of opening on the
                        pinned-files list (--dmenu and --fzf only).
  --feed <TEXT>         Print the hits for TEXT, one per line, and exit. This is
                        what --fzf re-runs on each keystroke; it is not meant to
                        be typed.
  --inner               Run fzf here rather than spawning a terminal. Set by the
                        terminal --fzf spawns; not meant to be typed.
  -h, --help            Show this help.
";

/// What the command line asked for, before any GTK setup happens: the launcher
/// path must not pay for — or fail on — a display connection it never uses.
struct Args {
    mode: Option<PromptMode>,
    menu: Option<Vec<String>>,
    query: Option<String>,
    /// `--feed`: print hits for this text and exit. `Some(None)` is `--feed`
    /// with an empty argument, which fzf sends for an empty input — a real
    /// request for the pinned list, not an absent flag.
    feed: Option<Option<String>>,
    /// `--inner`: we are the process a spawned terminal is running.
    inner: bool,
    /// `--preload`: become the resident GTK instance without showing it.
    preload: bool,
}

fn parse_args(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        mode: None,
        menu: None,
        query: None,
        feed: None,
        inner: false,
        preload: false,
    };
    let mut argv = argv.peekable();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--dmenu" => args.mode = Some(PromptMode::Dmenu),
            "--fzf" => args.mode = Some(PromptMode::Fzf),
            "--gtk" => args.mode = Some(PromptMode::Gtk),
            "--inner" => args.inner = true,
            "--preload" => args.preload = true,
            // fzf passes an empty argument for an empty input, so a missing
            // value here is that case rather than a user error.
            "--feed" => args.feed = Some(argv.next().filter(|text| !text.trim().is_empty())),
            // A launcher command is one shell-ish string so it can live in a
            // keybinding; splitting on whitespace is enough for flags, and a
            // launcher argument needing spaces belongs in config.json.
            "--menu" => {
                let value = argv
                    .next()
                    .ok_or_else(|| "--menu needs a command".to_string())?;
                let parts: Vec<String> = value.split_whitespace().map(str::to_string).collect();
                if parts.is_empty() {
                    return Err("--menu needs a command".to_string());
                }
                args.menu = Some(parts);
                // A launcher was named explicitly; that only makes sense here.
                args.mode.get_or_insert(PromptMode::Dmenu);
            }
            "--query" => {
                args.query = Some(
                    argv.next()
                        .ok_or_else(|| "--query needs a search term".to_string())?,
                );
            }
            other => return Err(format!("unrecognised argument: {other}")),
        }
    }
    // Only the GTK window has a resident process worth warming up.
    if args.preload {
        if matches!(args.mode, Some(PromptMode::Dmenu | PromptMode::Fzf)) || args.query.is_some() {
            return Err("--preload only applies to the built-in window".to_string());
        }
        args.mode = Some(PromptMode::Gtk);
    }
    Ok(args)
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    i18n::init();

    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }
    let args = match parse_args(raw.into_iter()) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("pdfs-prompt: {message}\n\n{USAGE}");
            return glib::ExitCode::FAILURE;
        }
    };

    // A feed is fzf's own reload child, not a front end: it must print hits and
    // nothing else, so it is answered before the stored mode is even read.
    if let Some(query) = args.feed {
        fzf::feed(query.as_deref());
        return glib::ExitCode::SUCCESS;
    }

    // The stored mode makes an existing keybinding switch front ends without
    // being re-bound; an explicit flag still wins.
    let stored = AppDirs::new()
        .map(|dirs| dirs.load_config().resolved_prompt().resolved_mode())
        .unwrap_or_default();
    let launcher = match args.mode.unwrap_or(stored) {
        PromptMode::Dmenu => Some(dmenu::run(dmenu::Options {
            menu: args.menu,
            query: args.query,
        })),
        PromptMode::Fzf => Some(fzf::run(fzf::Options {
            query: args.query,
            inner: args.inner,
        })),
        PromptMode::Gtk => None,
    };
    if let Some(result) = launcher {
        return match result {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("pdfs-prompt: {message}");
                glib::ExitCode::FAILURE
            }
        };
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    // GtkApplication normally remains active while it owns a window, including
    // when that window is hidden. Hold it explicitly as well: residency is a
    // product requirement here, not an incidental window-lifetime side effect.
    // Only the primary instance runs startup; a hold taken before registration
    // would also keep every remote invocation alive after it has forwarded
    // its activation.
    let resident = Rc::new(RefCell::new(None));
    app.connect_startup(move |app| {
        *resident.borrow_mut() = Some(app.hold());
        theme::load_resources("/de/nils/protondrivelinux/prompt.css");
    });
    if args.preload {
        if let Err(e) = app.register(gtk4::gio::Cancellable::NONE) {
            eprintln!("pdfs-prompt: {e}");
            return glib::ExitCode::FAILURE;
        }
        // Another instance is already resident; activating it would show it.
        // Dropping a registered application that never ran makes GIO warn
        // about the D-Bus registration, so leave it to process exit.
        if app.is_remote() {
            std::mem::forget(app);
            return glib::ExitCode::SUCCESS;
        }
    }
    let prompt: Rc<RefCell<Option<Rc<hud::Ui>>>> = Rc::new(RefCell::new(None));
    // The first activation of a preloading instance is its own startup, not a
    // summon: build the window but leave it hidden.
    let quiet = std::cell::Cell::new(args.preload);
    app.connect_activate(move |app| {
        let ui = if let Some(ui) = prompt.borrow().clone() {
            ui
        } else {
            let Some(ui) = hud::build_window(app) else {
                return;
            };
            *prompt.borrow_mut() = Some(ui.clone());
            ui
        };
        if quiet.replace(false) {
            ui.preload();
            return;
        }
        ui.activate();
    });
    // GTK must not try to parse our own flags; they were consumed above.
    app.run_with_args::<&str>(&[])
}

/// Run one blocking control-socket round-trip on a worker thread. The GTK main
/// loop never blocks on the daemon: the reply arrives through the channel.
fn spawn_request(
    socket: PathBuf,
    request: Request,
) -> async_channel::Receiver<Result<Response, String>> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(send(&socket, &request).map_err(|e| e.to_string()));
    });
    rx
}

fn dirs_home() -> Option<PathBuf> {
    AppDirs::new().ok().and_then(|dirs| dirs.home_dir())
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// An absolute directory path as "Home / …", falling back to the path itself
/// for anything outside the home directory.
fn home_relative(parent: &str) -> String {
    match dirs_home().and_then(|home| {
        Path::new(parent)
            .strip_prefix(&home)
            .ok()
            .map(|rel| rel.display().to_string())
    }) {
        // Translators: the user's home folder, as the location of a result.
        Some(rel) if rel.is_empty() => gettext("Home"),
        // Translators: {path} is a folder path inside the home folder, folder names separated by " / ".
        Some(rel) => gettext_f("Home / {path}", &[("path", &rel.replace('/', " / "))]),
        None => parent.to_string(),
    }
}

fn parent_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Coarse relative age, in the granularity a launcher row has room for.
fn format_age(epoch_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let diff = now - epoch_secs;
    match diff {
        // Translators: the age of a file modified under a minute ago.
        d if d < 60 => pgettext("age", "now"),
        // Translators: a compact file age; {count} is a number of minutes.
        d if d < 3600 => gettext_f("{count}m", &[("count", &(d / 60).to_string())]),
        // Translators: a compact file age; {count} is a number of hours.
        d if d < 86_400 => gettext_f("{count}h", &[("count", &(d / 3600).to_string())]),
        // Translators: a compact file age; {count} is a number of days.
        d if d < 2_592_000 => gettext_f("{count}d", &[("count", &(d / 86_400).to_string())]),
        // Translators: a compact file age; {count} is a number of months.
        d if d < 31_536_000 => gettext_f("{count}mo", &[("count", &(d / 2_592_000).to_string())]),
        // Translators: a compact file age; {count} is a number of years.
        d => gettext_f("{count}y", &[("count", &(d / 31_536_000).to_string())]),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str, score: i64) -> Hit {
        Hit::Local(LocalHit {
            name: name.into(),
            path: format!("/home/me/{name}"),
            is_dir: false,
            size: 1,
            modified: 1,
            score,
        })
    }

    #[test]
    fn visit_bonus_breaks_ties_but_cannot_beat_a_much_better_match() {
        let mut hits = vec![
            local("a-report.pdf", 1000),
            local("b-report.pdf", 1000),
            local("exact.pdf", 20_000),
        ];
        let visited = hits[1].key();
        rank_hits_by(&mut hits, |hit| if hit.key() == visited { 6000 } else { 0 });
        let names: Vec<&str> = hits.iter().map(Hit::name).collect();
        assert_eq!(names, ["exact.pdf", "b-report.pdf", "a-report.pdf"]);
    }

    #[test]
    fn ranked_hits_merge_sources_on_one_score_scale() {
        let mut hits = vec![
            Hit::Drive(SearchHit {
                name: "alpha.pdf".into(),
                path: "Drive/alpha.pdf".into(),
                is_dir: false,
                size: 1,
                modified: 1,
                pinned: false,
                cached: false,
                uid: "v~a".into(),
                mounted_path: None,
                score: 100,
            }),
            Hit::Local(LocalHit {
                name: "better.pdf".into(),
                path: "/home/me/better.pdf".into(),
                is_dir: false,
                size: 1,
                modified: 1,
                score: 300,
            }),
        ];

        rank_hits(&mut hits);
        assert!(matches!(&hits[0], Hit::Local(hit) if hit.name == "better.pdf"));
        assert!(matches!(&hits[1], Hit::Drive(hit) if hit.name == "alpha.pdf"));
    }

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|a| (*a).to_string()))
    }

    #[test]
    fn no_arguments_defer_the_mode_to_config() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.mode, None);
        assert_eq!(args.menu, None);
        assert_eq!(args.query, None);
    }

    #[test]
    fn naming_a_launcher_implies_dmenu_but_an_explicit_mode_still_wins() {
        let args = parse(&["--menu", "fuzzel --dmenu"]).unwrap();
        assert_eq!(args.mode, Some(PromptMode::Dmenu));
        assert_eq!(
            args.menu,
            Some(vec!["fuzzel".to_string(), "--dmenu".to_string()])
        );

        let args = parse(&["--gtk", "--menu", "rofi -dmenu"]).unwrap();
        assert_eq!(args.mode, Some(PromptMode::Gtk));
    }

    #[test]
    fn a_flag_without_its_value_is_an_error_not_a_default() {
        assert!(parse(&["--menu"]).is_err());
        assert!(parse(&["--query"]).is_err());
        assert!(parse(&["--nonsense"]).is_err());
    }

    #[test]
    fn a_query_is_taken_verbatim_including_spaces() {
        let args = parse(&["--dmenu", "--query", "tax return 2024"]).unwrap();
        assert_eq!(args.query.as_deref(), Some("tax return 2024"));
    }

    #[test]
    fn preload_forces_the_gtk_window_and_rejects_the_launchers() {
        let args = parse(&["--preload"]).unwrap();
        assert!(args.preload);
        assert_eq!(args.mode, Some(PromptMode::Gtk));

        assert!(parse(&["--preload", "--dmenu"]).is_err());
        assert!(parse(&["--fzf", "--preload"]).is_err());
        assert!(parse(&["--preload", "--menu", "fuzzel --dmenu"]).is_err());
        assert!(parse(&["--preload", "--query", "tax"]).is_err());
    }
}
