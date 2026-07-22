//! `pdfs-prompt` — the launcher: one search field over both Proton Drive and the
//! files on this machine, styled after Google Drive's search overlay.
//!
//! Bind it to a system shortcut (e.g. in Hyprland) for a quick HUD search. The
//! application is single-instance and keeps its window alive between summons,
//! so repeat activations only reset and present the existing widget tree. Every
//! daemon round-trip runs on a worker thread and lands back through an async
//! channel. Drive and local lookup share one request so a result set paints once.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::{gio, glib};

use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    LocalHit, Request, Response, SearchFilters, SearchHit, SearchKind, SearchSource, send,
};

mod activation;
use activation::{DriveActivation, drive_activation, mounted_path};

const APP_ID: &str = "io.narl.proton-drive-linux-prompt";

/// Debounce before a keystroke turns into a daemon round-trip. Short enough to
/// feel live, long enough that typing a word is one search, not five.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

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
    (Filter::All, "All"),
    (Filter::Folders, "Folders"),
    (Filter::Documents, "Documents"),
    (Filter::Images, "Images"),
    (Filter::Media, "Media"),
];

impl Filter {
    /// Whether a hit of this name/kind survives the chip.
    fn accepts(self, name: &str, is_dir: bool) -> bool {
        match self {
            Filter::All => true,
            Filter::Folders => is_dir,
            Filter::Documents => !is_dir && is_document(name),
            Filter::Images => !is_dir && is_image(name),
            Filter::Media => !is_dir && is_media(name),
        }
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
#[derive(Clone)]
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
    fn location(&self) -> String {
        match self {
            Hit::Drive(h) => {
                let parent = parent_of(&h.path);
                if parent.is_empty() {
                    "My files".to_string()
                } else {
                    format!("My files / {}", parent.replace('/', " / "))
                }
            }
            Hit::Local(h) => {
                let parent = parent_of(&h.path);
                match dirs_home().and_then(|home| {
                    Path::new(&parent)
                        .strip_prefix(&home)
                        .ok()
                        .map(|rel| rel.display().to_string())
                }) {
                    Some(rel) if rel.is_empty() => "Home".to_string(),
                    Some(rel) => format!("Home / {}", rel.replace('/', " / ")),
                    None => parent,
                }
            }
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
}

fn rank_hits(hits: &mut [Hit]) {
    hits.sort_by(|a, b| {
        b.score()
            .cmp(&a.score())
            .then_with(|| a.name().cmp(b.name()))
            .then_with(|| a.key().cmp(&b.key()))
    });
}

/// Everything the window needs to render and act, in one place so the many
/// callbacks below capture a single `Rc` instead of a dozen clones each.
struct Ui {
    socket: PathBuf,
    mountpoint: RefCell<PathBuf>,

    entry: gtk4::Entry,
    spinner: gtk4::Spinner,
    stack: gtk4::Stack,
    scroller: gtk4::ScrolledWindow,
    results: gtk4::Box,
    search_section: Section,
    drive_section: Section,
    local_section: Section,
    placeholder: adw::StatusPage,
    hint: gtk4::Label,
    chips: RefCell<Vec<(Filter, gtk4::ToggleButton)>>,

    /// Raw (unfiltered) hits from the last reply of each search.
    drive_hits: RefCell<Vec<SearchHit>>,
    local_hits: RefCell<Vec<LocalHit>>,
    /// The filtered, flattened list the cursor indexes into — Drive rows first,
    /// then local ones, matching the visual order.
    visible: RefCell<Vec<Hit>>,
    cursor: Cell<Option<usize>>,
    /// The trimmed query the currently-rendered rows belong to. Enter compares
    /// against it so a keystroke that lands just before a fresher render can't
    /// open a file from a result set the user has already typed past.
    rendered_query: RefCell<String>,

    filter: Cell<Filter>,
    /// Monotonic query id. A reply whose id is stale (the user typed again while
    /// it was in flight) is dropped instead of overwriting fresher results.
    query_id: Cell<u64>,
    /// In-flight requests for the current query id.
    pending: Cell<u8>,
    /// True once a Drive open is running: the window is about to close, so
    /// further input is ignored.
    opening: Cell<bool>,
    indexing: Cell<bool>,
    /// Set when Enter arrives before the in-flight query has rendered (typing
    /// then hitting Enter faster than the search debounce). The open is honoured
    /// against the selected row once the fresh results settle, rather than being
    /// dropped — so a plain "type, Enter" always opens something.
    open_pending: Cell<bool>,
    /// Prevent lifecycle-driven entry resets from also scheduling the normal
    /// debounced empty-query request; activation performs one explicit daemon
    /// bootstrap instead.
    suppress_entry_change: Cell<bool>,
    /// The window, so a deferred open (above) can reach it from `render`.
    window: RefCell<Option<adw::ApplicationWindow>>,
}

/// A titled group of rows: header (hidden when empty) plus its list.
struct Section {
    header: gtk4::Box,
    title: gtk4::Label,
    list: gtk4::ListBox,
}

impl Section {
    fn new(title: &str, icon: &str) -> Self {
        let header = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .visible(false)
            .build();
        header.add_css_class("section-header");

        let image = gtk4::Image::from_icon_name(icon);
        image.add_css_class("section-icon");
        header.append(&image);

        let label = gtk4::Label::builder().label(title).xalign(0.0).build();
        label.add_css_class("section-title");
        header.append(&label);

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Single)
            .visible(false)
            .build();
        list.add_css_class("result-list");

        Self {
            header,
            title: label,
            list,
        }
    }

    fn attach(&self, parent: &gtk4::Box) {
        parent.append(&self.header);
        parent.append(&self.list);
    }

    /// Swap in a new set of rows. Rebuilds the list — at [`SEARCH_LIMIT`] rows a
    /// rebuild is a handful of widgets, far cheaper than diffing them.
    fn set_rows(&self, hits: &[Hit], subtitle: Option<&str>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for hit in hits {
            self.list.append(&build_row(hit));
        }
        let shown = !hits.is_empty();
        self.header.set_visible(shown);
        self.list.set_visible(shown);
        if let Some(text) = subtitle {
            self.title.set_label(text);
        }
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    // GtkApplication normally remains active while it owns a window, including
    // when that window is hidden. Hold it explicitly as well: residency is a
    // product requirement here, not an incidental window-lifetime side effect.
    let _resident = app.hold();
    app.connect_startup(|_| load_css());
    let prompt: Rc<RefCell<Option<Rc<Ui>>>> = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        let ui = if let Some(ui) = prompt.borrow().clone() {
            ui
        } else {
            let Some(ui) = build_window(app) else {
                return;
            };
            *prompt.borrow_mut() = Some(ui.clone());
            ui
        };
        ui.activate();
    });
    app.run()
}

fn build_window(app: &adw::Application) -> Option<Rc<Ui>> {
    let dirs = match AppDirs::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("cannot resolve app dirs: {e}");
            return None;
        }
    };

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Search Proton Drive")
        .default_width(720)
        .default_height(540)
        .resizable(false)
        .decorated(false)
        .build();
    window.add_css_class("launcher-window");

    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    card.add_css_class("launcher-card");
    window.set_content(Some(&card));

    // --- search bar -------------------------------------------------------
    let search_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    search_bar.add_css_class("search-bar");

    let search_icon = gtk4::Image::from_icon_name("system-search-symbolic");
    search_icon.add_css_class("search-icon");
    search_bar.append(&search_icon);

    let entry = gtk4::Entry::builder()
        .placeholder_text("Search in Drive and on this computer")
        .hexpand(true)
        .build();
    entry.add_css_class("search-entry");
    search_bar.append(&entry);

    let spinner = gtk4::Spinner::new();
    spinner.set_visible(false);
    search_bar.append(&spinner);

    let esc = gtk4::Label::new(Some("Esc"));
    esc.add_css_class("key-hint");
    search_bar.append(&esc);
    card.append(&search_bar);

    // --- filter chips -----------------------------------------------------
    let chip_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    chip_row.add_css_class("chip-row");
    card.append(&chip_row);

    // --- results ----------------------------------------------------------
    let results = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    results.add_css_class("results");

    let search_section = Section::new("Best matches", "system-search-symbolic");
    let drive_section = Section::new("Proton Drive", "folder-remote-symbolic");
    let local_section = Section::new("This computer", "drive-harddisk-symbolic");
    search_section.attach(&results);
    drive_section.attach(&results);
    local_section.attach(&results);

    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .child(&results)
        .build();

    let placeholder = adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title("No results")
        .build();
    placeholder.add_css_class("compact");

    // The daemon-offline page is a peer of the results, not a modal: the prompt
    // is still usable for nothing else, so it owns the whole view.
    let offline = adw::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title("Proton Drive is not running")
        .description("Start the mount daemon to search your Drive.")
        .build();
    offline.add_css_class("compact");
    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    offline.set_child(Some(&retry));

    let stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .transition_duration(120)
        .vexpand(true)
        .build();
    stack.add_named(&scroller, Some("results"));
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(&offline, Some("offline"));
    card.append(&stack);

    // --- footer -----------------------------------------------------------
    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    footer.add_css_class("footer");
    let hint = gtk4::Label::builder()
        .label("Connecting…")
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    hint.add_css_class("footer-text");
    footer.append(&hint);
    let keys = gtk4::Label::new(Some("↑↓ navigate · ↵ open · Tab filter"));
    keys.add_css_class("footer-text");
    footer.append(&keys);
    card.append(&footer);

    let ui = Rc::new(Ui {
        socket: dirs.control_socket(),
        mountpoint: RefCell::new(dirs.default_mountpoint()),
        entry: entry.clone(),
        spinner,
        stack,
        scroller,
        results,
        search_section,
        drive_section,
        local_section,
        placeholder,
        hint,
        chips: RefCell::new(Vec::new()),
        drive_hits: RefCell::new(Vec::new()),
        local_hits: RefCell::new(Vec::new()),
        visible: RefCell::new(Vec::new()),
        cursor: Cell::new(None),
        rendered_query: RefCell::new(String::new()),
        filter: Cell::new(Filter::All),
        query_id: Cell::new(0),
        pending: Cell::new(0),
        opening: Cell::new(false),
        indexing: Cell::new(false),
        open_pending: Cell::new(false),
        suppress_entry_change: Cell::new(false),
        window: RefCell::new(None),
    });
    *ui.window.borrow_mut() = Some(window.clone());

    for (filter, label) in FILTERS {
        let chip = gtk4::ToggleButton::builder()
            .label(label)
            .active(filter == Filter::All)
            .build();
        chip.add_css_class("chip");
        let ui_chip = ui.clone();
        chip.connect_clicked(move |btn| {
            // A chip is a radio, not a switch: clicking the active one keeps it on.
            if btn.is_active() {
                ui_chip.set_filter(filter);
            } else {
                btn.set_active(true);
            }
        });
        chip_row.append(&chip);
        ui.chips.borrow_mut().push((filter, chip));
    }

    // Clicking a row (or tapping it) opens it, same as Enter on the cursor. A
    // local row's index is relative to its own list, so the Drive rows above it
    // are added back to land on the same flattened index the cursor uses.
    for (list, is_local) in [
        (&ui.drive_section.list, false),
        (&ui.local_section.list, true),
    ] {
        let ui_row = ui.clone();
        list.connect_row_activated(move |_, row| {
            let offset = if is_local { ui_row.drive_count() } else { 0 };
            ui_row.open(offset + row.index() as usize);
        });
    }
    let ui_row = ui.clone();
    ui.search_section
        .list
        .connect_row_activated(move |_, row| ui_row.open(row.index() as usize));

    let ui_key = ui.clone();
    let keys = gtk4::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, state| {
        if ui_key.opening.get() {
            return glib::Propagation::Stop;
        }
        match key {
            gtk4::gdk::Key::Escape => {
                // Spotlight-style: first Escape clears a non-empty query, a
                // second (or an already-empty box) dismisses the launcher.
                if ui_key.entry.text().is_empty() {
                    ui_key.dismiss();
                } else {
                    ui_key.entry.set_text("");
                }
                glib::Propagation::Stop
            }
            gtk4::gdk::Key::Down => {
                ui_key.move_cursor(1);
                glib::Propagation::Stop
            }
            gtk4::gdk::Key::Up => {
                ui_key.move_cursor(-1);
                glib::Propagation::Stop
            }
            gtk4::gdk::Key::Tab | gtk4::gdk::Key::ISO_Left_Tab => {
                let back = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
                ui_key.cycle_filter(back);
                glib::Propagation::Stop
            }
            // Return is deliberately absent: `GtkText` binds it to `activate`
            // and consumes it, so a bubble-phase controller on the window never
            // sees it while the entry has focus (which is always, here). It is
            // handled on the entry's `activate` signal instead — see below.
            _ => glib::Propagation::Proceed,
        }
    });
    // On the window, not the entry: arrow keys must steer the list even when the
    // pointer has moved focus into it.
    window.add_controller(keys);

    // Enter opens the row under the cursor. This lives on the entry rather than
    // with the other keys on the window because `GtkText` claims Return for its
    // own `activate` binding, so a bubble-phase controller upstream of the
    // focused entry is never reached. Enter with focus in the list is covered by
    // `row_activated` above.
    let ui_activate = ui.clone();
    entry.connect_activate(move |_| {
        if ui_activate.opening.get() {
            return;
        }
        // Fall back to the top row when nothing is explicitly selected (e.g.
        // results haven't rendered yet); `open` guards the rest.
        ui_activate.open(ui_activate.cursor.get().unwrap_or(0));
    });

    // Some GTK/input-method combinations consume the physical Return binding in
    // GtkText without emitting Entry::activate. Capture only Return ahead of the
    // text widget and leave every other key to normal GTK/IME handling. Stopping
    // propagation also prevents the activate signal above from opening twice;
    // that signal remains the programmatic/accessibility activation path.
    let ui_return = ui.clone();
    let return_key = gtk4::EventControllerKey::new();
    return_key.set_propagation_phase(gtk4::PropagationPhase::Capture);
    return_key.connect_key_pressed(move |_, key, _, _| {
        if matches!(key, gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter) {
            if !ui_return.opening.get() {
                ui_return.open(ui_return.cursor.get().unwrap_or(0));
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    entry.add_controller(return_key);

    let debounce: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let ui_changed = ui.clone();
    entry.connect_changed(move |entry| {
        if ui_changed.suppress_entry_change.get() {
            return;
        }
        // A fresh keystroke supersedes any Enter that was waiting on results:
        // the user is still refining the query, not asking to open yet.
        ui_changed.open_pending.set(false);
        if let Some(source) = debounce.borrow_mut().take() {
            source.remove();
        }
        let query = entry.text().trim().to_string();
        let ui_timeout = ui_changed.clone();
        let debounce_timeout = debounce.clone();
        let source = glib::timeout_add_local_once(SEARCH_DEBOUNCE, move || {
            debounce_timeout.borrow_mut().take();
            ui_timeout.search(&query);
        });
        *debounce.borrow_mut() = Some(source);
    });

    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| ui_retry.connect());

    // Keep the widget tree alive when the compositor asks the window to close.
    // This also avoids running GTK's destruction path while async replies still
    // hold references to prompt widgets.
    let ui_close = ui.clone();
    window.connect_close_request(move |_| {
        ui_close.dismiss();
        glib::Propagation::Stop
    });
    let entry_map = entry.clone();
    window.connect_map(move |_| {
        entry_map.grab_focus();
    });

    Some(ui)
}

impl Ui {
    /// Reset and present the resident prompt in response to GApplication
    /// activation. A second invocation while a file is materialising merely
    /// raises the progress window; it must not make the in-flight open mutable.
    fn activate(self: &Rc<Self>) {
        let Some(window) = self.window.borrow().clone() else {
            return;
        };
        if !self.opening.get() {
            // Supersede replies from the previous showing before clearing the
            // query. The daemon work may finish, but it can no longer repaint
            // the newly activated prompt with stale rows.
            self.query_id.set(self.query_id.get() + 1);
            self.pending.set(0);
            self.open_pending.set(false);
            self.spinner.stop();
            self.spinner.set_visible(false);
            self.suppress_entry_change.set(true);
            self.entry.set_text("");
            self.suppress_entry_change.set(false);
            self.set_filter(Filter::All);
            self.connect();
        }
        window.present();
        self.entry.grab_focus();
    }

    /// Dismiss without destroying the application window. GTK continues to
    /// associate the hidden window with the single application instance, so a
    /// later desktop shortcut invocation is a cheap `activate` round-trip.
    fn dismiss(&self) {
        self.open_pending.set(false);
        if let Some(window) = self.window.borrow().as_ref() {
            window.set_visible(false);
        }
    }

    /// Ask the daemon for status and, if it answers, load the suggested (pinned)
    /// files. Runs off-thread: a dead daemon must not freeze the first frame.
    fn connect(self: &Rc<Self>) {
        self.hint.set_label("Connecting…");
        let rx = spawn_request(self.socket.clone(), Request::Status);
        let ui = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(Response::Status { mountpoint, .. })) => {
                    *ui.mountpoint.borrow_mut() = PathBuf::from(mountpoint);
                    ui.entry.set_sensitive(true);
                    ui.stack.set_visible_child_name("results");
                    ui.load_suggestions();
                }
                _ => {
                    ui.entry.set_sensitive(false);
                    ui.hint.set_label("Daemon offline");
                    ui.stack.set_visible_child_name("offline");
                }
            }
        });
    }

    /// The daemon stopped answering after the window was already open. Flip to
    /// the offline page and lock the entry; the retry button re-runs `connect`.
    fn go_offline(self: &Rc<Self>) {
        self.pending.set(0);
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.entry.set_sensitive(false);
        self.hint.set_label("Daemon offline");
        self.stack.set_visible_child_name("offline");
    }

    /// The empty-query view: pinned files, which are the ones the user chose to
    /// keep on this machine — the closest thing the daemon has to "recent".
    fn load_suggestions(self: &Rc<Self>) {
        let id = self.begin_query(1);
        let rx = spawn_request(self.socket.clone(), Request::ListPins);
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let reply = rx.recv().await;
            if !ui.finish_query(id) {
                return;
            }
            if matches!(reply, Ok(Err(_)) | Err(_)) {
                ui.go_offline();
                return;
            }
            let hits = match reply {
                Ok(Ok(Response::Pins { pins })) => pins
                    .into_iter()
                    .map(|pin| SearchHit {
                        name: file_name(&pin.path),
                        path: pin.path,
                        is_dir: pin.recursive,
                        size: 0,
                        modified: 0,
                        pinned: true,
                        uid: pin.uid,
                        score: 0,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            *ui.drive_hits.borrow_mut() = hits;
            ui.local_hits.borrow_mut().clear();
            ui.render();
        });
    }

    /// Search Drive and local metadata in one daemon round-trip. An empty query
    /// falls back to the suggestions view.
    fn search(self: &Rc<Self>, query: &str) {
        if query.is_empty() {
            self.load_suggestions();
            return;
        }

        let id = self.begin_query(1);
        let search = spawn_request(
            self.socket.clone(),
            Request::SearchV2 {
                query: query.to_string(),
                limit: SEARCH_LIMIT,
                filters: SearchFilters {
                    sources: vec![SearchSource::Drive, SearchSource::Local],
                    kind: self.filter.get().search_kind(),
                },
            },
        );
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let reply = search.recv().await;
            if !ui.finish_query(id) {
                return;
            }
            match reply {
                Ok(Ok(Response::SearchResultsV2 {
                    drive_hits,
                    local_hits,
                    local_indexing,
                })) => {
                    *ui.drive_hits.borrow_mut() = drive_hits;
                    *ui.local_hits.borrow_mut() = local_hits;
                    ui.indexing.set(local_indexing);
                    ui.render();
                }
                // Transport failure: the daemon is gone. Don't paint "No
                // results" over a crash — say so.
                Ok(Err(_)) | Err(_) => ui.go_offline(),
                _ => {
                    ui.drive_hits.borrow_mut().clear();
                    ui.local_hits.borrow_mut().clear();
                    ui.render();
                }
            }
        });
    }

    /// Open a new query id, expecting `requests` replies. Any reply still in
    /// flight for an older id is now stale and will be dropped by
    /// [`finish_query`](Self::finish_query).
    fn begin_query(&self, requests: u8) -> u64 {
        let id = self.query_id.get() + 1;
        self.query_id.set(id);
        self.pending.set(requests);
        self.spinner.set_visible(true);
        self.spinner.start();
        id
    }

    /// Account for one reply. Returns false if it belongs to a superseded query,
    /// in which case the caller must discard it.
    fn finish_query(&self, id: u64) -> bool {
        if self.query_id.get() != id {
            return false;
        }
        let left = self.pending.get().saturating_sub(1);
        self.pending.set(left);
        if left == 0 {
            self.spinner.stop();
            self.spinner.set_visible(false);
        }
        true
    }

    fn set_filter(self: &Rc<Self>, filter: Filter) {
        if self.filter.get() == filter {
            return;
        }
        self.filter.set(filter);
        for (chip_filter, chip) in self.chips.borrow().iter() {
            chip.set_active(*chip_filter == filter);
        }
        let query = self.entry.text().trim().to_string();
        if query.is_empty() {
            self.render();
        } else {
            self.search(&query);
        }
    }

    fn cycle_filter(self: &Rc<Self>, backwards: bool) {
        let current = FILTERS
            .iter()
            .position(|(f, _)| *f == self.filter.get())
            .unwrap_or(0);
        let count = FILTERS.len();
        let next = if backwards {
            (current + count - 1) % count
        } else {
            (current + 1) % count
        };
        self.set_filter(FILTERS[next].0);
    }

    fn drive_count(&self) -> usize {
        self.visible
            .borrow()
            .iter()
            .filter(|hit| matches!(hit, Hit::Drive(_)))
            .count()
    }

    /// Rebuild suggestions or the globally-ranked search section, preserving the
    /// selected identity when a result survives the update.
    fn render(self: &Rc<Self>) {
        let filter = self.filter.get();

        // Identity of the row under the cursor before we rebuild the list, so a
        // late-arriving second search can't yank the selection back to the top.
        let selected_key = self
            .cursor
            .get()
            .and_then(|i| self.visible.borrow().get(i).map(Hit::key));

        let drive: Vec<Hit> = self
            .drive_hits
            .borrow()
            .iter()
            .filter(|hit| filter.accepts(&hit.name, hit.is_dir))
            .cloned()
            .map(Hit::Drive)
            .collect();
        let local: Vec<Hit> = self
            .local_hits
            .borrow()
            .iter()
            .filter(|hit| filter.accepts(&hit.name, hit.is_dir))
            .cloned()
            .map(Hit::Local)
            .collect();

        let query = self.entry.text().trim().to_string();
        let searching = !query.is_empty();
        let total = drive.len() + local.len();
        let mut visible = drive;
        visible.extend(local);
        if searching {
            rank_hits(&mut visible);
            self.search_section.set_rows(&visible, Some("Best matches"));
            self.drive_section.set_rows(&[], None);
            self.local_section.set_rows(&[], None);
        } else {
            self.search_section.set_rows(&[], None);
            let drive_count = visible
                .iter()
                .take_while(|hit| matches!(hit, Hit::Drive(_)))
                .count();
            self.drive_section
                .set_rows(&visible[..drive_count], Some("Pinned in Proton Drive"));
            self.local_section.set_rows(&visible[drive_count..], None);
        }

        // Find where the previously-selected row landed in the rebuilt list.
        let restored = selected_key.and_then(|key| visible.iter().position(|hit| hit.key() == key));

        *self.visible.borrow_mut() = visible;
        *self.rendered_query.borrow_mut() = query;

        if total == 0 {
            self.placeholder.set_title(if searching {
                "No results"
            } else {
                "Search your Drive"
            });
            self.placeholder
                .set_description(Some(match (searching, self.indexing.get()) {
                    (true, true) => {
                        "Still indexing this computer — local results will fill in shortly."
                    }
                    (true, false) => "Try a different search, or another filter.",
                    _ => "Start typing to search Proton Drive and the files on this computer.",
                }));
            self.stack.set_visible_child_name("empty");
            self.cursor.set(None);
        } else {
            self.stack.set_visible_child_name("results");
            self.select(restored.unwrap_or(0));
        }

        self.hint.set_label(&self.status_text(total, searching));

        // Honour an Enter that arrived before this query rendered, now that the
        // results (and the cursor) are settled. Only once both searches are in,
        // so it lands on the final list, not a half-populated one.
        if self.open_pending.get() && self.pending.get() == 0 {
            self.open_pending.set(false);
            if let Some(index) = self.cursor.get() {
                self.open(index);
            }
        }
    }

    fn status_text(&self, total: usize, searching: bool) -> String {
        if !searching {
            return match total {
                0 => "No pinned files".to_string(),
                1 => "1 pinned file".to_string(),
                n => format!("{n} pinned files"),
            };
        }
        let drive = self.drive_count();
        let local = total - drive;
        let mut text = format!("{drive} in Drive · {local} on this computer");
        if self.indexing.get() {
            text.push_str(" · indexing…");
        }
        text
    }

    /// Move the cursor by `delta` rows, wrapping at both ends.
    fn move_cursor(self: &Rc<Self>, delta: i32) {
        let count = self.visible.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let current = self.cursor.get().map_or(0, |c| c as i32);
        let next = (current + delta).rem_euclid(count);
        self.select(next as usize);
    }

    /// Put the cursor on row `index` of the flattened list: select it in whichever
    /// section owns it, clear the other section's selection, and scroll it into
    /// view. Focus stays in the entry so typing never breaks.
    fn select(self: &Rc<Self>, index: usize) {
        if !self.rendered_query.borrow().is_empty() {
            self.drive_section.list.unselect_all();
            self.local_section.list.unselect_all();
            let Some(row) = self.search_section.list.row_at_index(index as i32) else {
                return;
            };
            self.search_section.list.select_row(Some(&row));
            self.cursor.set(Some(index));
            self.scroll_into_view(&row);
            return;
        }

        self.search_section.list.unselect_all();
        let drive_count = self.drive_count();
        let (list, other, local_index) = if index < drive_count {
            (
                &self.drive_section.list,
                &self.local_section.list,
                index as i32,
            )
        } else {
            (
                &self.local_section.list,
                &self.drive_section.list,
                (index - drive_count) as i32,
            )
        };
        other.unselect_all();
        let Some(row) = list.row_at_index(local_index) else {
            return;
        };
        list.select_row(Some(&row));
        self.cursor.set(Some(index));
        self.scroll_into_view(&row);
    }

    /// Keep the selected row inside the viewport, scrolling by the smallest
    /// amount that reveals it (like a native list, unlike jumping it to centre).
    fn scroll_into_view(&self, row: &gtk4::ListBoxRow) {
        let Some(bounds) = row.compute_bounds(&self.results) else {
            return;
        };
        let adjustment = self.scroller.vadjustment();
        let (top, bottom) = (
            f64::from(bounds.y()),
            f64::from(bounds.y() + bounds.height()),
        );
        let (value, page) = (adjustment.value(), adjustment.page_size());
        if top < value {
            adjustment.set_value(top);
        } else if bottom > value + page {
            adjustment.set_value(bottom - page);
        }
    }

    /// Open the hit at `index`. Local files are already on disk, so they hand
    /// straight to the desktop; a Drive hit must first be hydrated into the cache
    /// by the daemon (`OpenFile`), which can take a moment — the window stays up,
    /// with the spinner running, until the path comes back.
    fn open(self: &Rc<Self>, index: usize) {
        if self.opening.get() {
            return;
        }
        // The rows on screen may belong to an older query whose reply is still
        // settling. Don't act on them — Enter must never launch a file the user
        // has typed past. But don't drop the intent either: remember it and open
        // the right row once the fresh results land (see `render`).
        if self.entry.text().trim() != *self.rendered_query.borrow() {
            self.open_pending.set(true);
            return;
        }
        let Some(hit) = self.visible.borrow().get(index).cloned() else {
            return;
        };

        match hit {
            Hit::Local(local) => {
                xdg_open(Path::new(&local.path));
                self.dismiss();
            }
            Hit::Drive(drive) => {
                // Empty-query rows come from ListPins, whose `recursive` flag is
                // pin policy rather than reliable node kind metadata. Every pin
                // already has a mounted path, so activate that path directly;
                // this handles non-recursively pinned folders without mistaking
                // them for files and sending an invalid OpenFile request.
                if self.rendered_query.borrow().is_empty() {
                    let path = mounted_path(&self.mountpoint.borrow(), &drive.path);
                    xdg_open(&path);
                    self.dismiss();
                    return;
                }
                match drive_activation(&drive.name, drive.is_dir) {
                    DriveActivation::Folder | DriveActivation::MountedMedia => {
                        let path = mounted_path(&self.mountpoint.borrow(), &drive.path);
                        xdg_open(&path);
                        self.dismiss();
                        return;
                    }
                    DriveActivation::Materialize => {}
                }

                self.opening.set(true);
                self.spinner.set_visible(true);
                self.spinner.start();
                self.hint.set_label(&format!("Opening {}…", drive.name));

                let rx = spawn_request(
                    self.socket.clone(),
                    Request::OpenFile {
                        path: drive.path.clone(),
                    },
                );
                let ui = self.clone();
                glib::spawn_future_local(async move {
                    let reply = rx.recv().await;
                    ui.spinner.stop();
                    ui.spinner.set_visible(false);
                    ui.opening.set(false);
                    match reply {
                        Ok(Ok(Response::FilePath { path })) => {
                            xdg_open(Path::new(&path));
                            ui.dismiss();
                        }
                        Ok(Ok(Response::Error { message, .. })) => {
                            ui.hint.set_label(&format!("Could not open: {message}"));
                        }
                        _ => ui.hint.set_label("Could not reach the daemon"),
                    }
                });
            }
        }
    }
}

/// One result row: type icon, name, location, then size/age on the right.
fn build_row(hit: &Hit) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.add_css_class("result-row");

    let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);

    let icon = gtk4::Image::from_gicon(&icon_for(hit.name(), hit.is_dir()));
    icon.set_pixel_size(24);
    content.append(&icon);

    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.set_valign(gtk4::Align::Center);

    let name = gtk4::Label::builder()
        .label(hit.name())
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .build();
    name.add_css_class("result-name");
    text.append(&name);

    let location = gtk4::Label::builder()
        .label(hit.location())
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::Start)
        .build();
    location.add_css_class("result-location");
    text.append(&location);
    content.append(&text);

    let meta = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    meta.set_valign(gtk4::Align::Center);
    if hit.pinned() {
        let pin = gtk4::Image::from_icon_name("view-pin-symbolic");
        pin.add_css_class("pin-icon");
        meta.append(&pin);
    }
    if !hit.is_dir() && hit.size() > 0 {
        let size = gtk4::Label::new(Some(&format_size(hit.size())));
        size.add_css_class("result-meta");
        meta.append(&size);
    }
    if hit.modified() > 0 {
        let modified = gtk4::Label::new(Some(&format_age(hit.modified())));
        modified.add_css_class("result-meta");
        meta.append(&modified);
    }
    content.append(&meta);

    row.set_child(Some(&content));
    row
}

/// The desktop's own icon for a file name, so results look like the user's file
/// manager instead of a bespoke palette. Falls back to a generic document icon
/// for names the content-type database cannot place.
fn icon_for(name: &str, is_dir: bool) -> gio::Icon {
    if is_dir {
        return gio::ThemedIcon::new("folder").upcast();
    }
    let (content_type, _uncertain) = gio::functions::content_type_guess(Some(name), &[]);
    gio::functions::content_type_get_icon(&content_type)
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

fn xdg_open(path: &Path) {
    tracing::info!(path = %path.display(), "opening");
    if let Err(e) = Command::new("xdg-open").arg(path).spawn() {
        tracing::error!("xdg-open failed: {e}");
    }
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

fn parent_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [
        (1024 * 1024 * 1024, "GB"),
        (1024 * 1024, "MB"),
        (1024, "KB"),
    ];
    for (scale, unit) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

/// Coarse relative age, in the granularity a launcher row has room for.
fn format_age(epoch_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let diff = now - epoch_secs;
    match diff {
        d if d < 60 => "now".to_string(),
        d if d < 3600 => format!("{}m", d / 60),
        d if d < 86_400 => format!("{}h", d / 3600),
        d if d < 2_592_000 => format!("{}d", d / 86_400),
        d if d < 31_536_000 => format!("{}mo", d / 2_592_000),
        d => format!("{}y", d / 31_536_000),
    }
}

/// Styling. Everything is expressed against libadwaita's named colors, so the
/// launcher follows the system light/dark theme instead of pinning its own.
fn load_css() {
    let css = "
        /* Pin the accent to Proton Purple so the HUD matches the main app,
           which its own process/theme would otherwise leave at the system
           default (often blue or orange). */
        @define-color accent_bg_color #6d4aff;
        @define-color accent_color #6d4aff;
        @define-color accent_fg_color #ffffff;

        window.launcher-window {
            background: transparent;
        }

        .launcher-card {
            background-color: @view_bg_color;
            border: 1px solid alpha(@borders, 0.8);
            border-radius: 14px;
            box-shadow: 0 16px 48px alpha(black, 0.35);
        }

        .search-bar {
            padding: 14px 18px;
            border-bottom: 1px solid alpha(@borders, 0.6);
        }

        .search-icon {
            -gtk-icon-size: 20px;
            color: alpha(currentColor, 0.55);
        }

        .search-entry {
            background: none;
            border: none;
            box-shadow: none;
            outline: none;
            padding: 2px 0;
            font-size: 1.15rem;
        }

        .key-hint {
            padding: 2px 8px;
            border-radius: 6px;
            background-color: alpha(currentColor, 0.08);
            font-size: 0.75rem;
            opacity: 0.55;
        }

        .chip-row {
            padding: 10px 14px;
            border-bottom: 1px solid alpha(@borders, 0.6);
        }

        .chip {
            padding: 4px 14px;
            min-height: 0;
            border-radius: 999px;
            background: none;
            border: 1px solid alpha(@borders, 0.9);
            box-shadow: none;
            font-size: 0.86rem;
            font-weight: 500;
        }

        .chip:hover {
            background-color: alpha(currentColor, 0.06);
        }

        .chip:checked {
            background-color: @accent_bg_color;
            color: @accent_fg_color;
            border-color: @accent_bg_color;
        }

        .results {
            padding: 6px 8px 10px 8px;
        }

        .section-header {
            padding: 12px 10px 6px 10px;
        }

        .section-title {
            font-size: 0.78rem;
            font-weight: 700;
            letter-spacing: 0.06em;
            text-transform: uppercase;
            opacity: 0.55;
        }

        .section-icon {
            -gtk-icon-size: 14px;
            opacity: 0.55;
        }

        .result-list {
            background: none;
        }

        .result-row {
            padding: 9px 10px;
            border-radius: 10px;
        }

        .result-row:hover {
            background-color: alpha(currentColor, 0.05);
        }

        .result-row:selected {
            background-color: @accent_bg_color;
            color: @accent_fg_color;
        }

        .result-name {
            font-weight: 500;
        }

        .result-location, .result-meta {
            font-size: 0.8rem;
            opacity: 0.55;
        }

        .result-row:selected .result-location,
        .result-row:selected .result-meta {
            opacity: 0.75;
        }

        .footer {
            padding: 8px 16px;
            border-top: 1px solid alpha(@borders, 0.6);
        }

        .footer-text {
            font-size: 0.78rem;
            opacity: 0.5;
        }

        statuspage.compact > scrolledwindow > viewport > box {
            margin: 24px 12px;
        }
    ";

    let provider = gtk4::CssProvider::new();
    provider.load_from_string(css);
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                uid: "v~a".into(),
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
}
