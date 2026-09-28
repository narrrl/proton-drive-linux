//! The built-in GTK launcher: a search entry over Drive and this computer,
//! the results on the left and a preview of the selected one on the right.
//!
//! Focus never leaves the search entry. The arrow keys move a cursor through
//! the results, Enter opens, Ctrl+Enter shows the file in its folder and
//! Ctrl+C copies its path. A single click selects a row, a double-click opens
//! it, and a row can be dragged out as a file.

mod highlight;
mod history;
mod preview;
mod row;
mod thumbs;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::{gdk, gio, glib};

use pdfs_core::config::AppDirs;
use pdfs_core::control::{Request, Response, SearchFilters, SearchHit, SearchSource};
use pdfs_core::opener::{self, OpenWith};

use crate::activation::{DriveActivation, drive_activation, mounted_or_relative, mounted_target};
use crate::compat::{Spinner, spinner};
use crate::theme::set_proton_theme;
use crate::{
    FILTERS, Filter, Hit, SEARCH_DEBOUNCE, SEARCH_LIMIT, file_name, gettext, gettext_f, ngettext_f,
    rank_hits_by, spawn_request,
};
use history::History;
use preview::Preview;
use thumbs::Thumbs;

/// Recent files shown above the pinned ones while the query is empty.
const RECENT_LIMIT: usize = 8;

/// How long opening waits for the mount to answer whether it has a file. A
/// frozen mount must not freeze the launcher; the daemon is asked instead.
const STAT_TIMEOUT: Duration = Duration::from_secs(2);

/// The titled groups of the result list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Recent,
    Pinned,
    Matches,
}

impl Section {
    fn title(self) -> String {
        match self {
            // Translators: the launcher's section of recently opened files.
            Section::Recent => gettext("Recent"),
            Section::Pinned => gettext("Pinned in Proton Drive"),
            Section::Matches => gettext("Best matches"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Section::Recent => "document-open-recent-symbolic",
            Section::Pinned => "view-pin-symbolic",
            Section::Matches => "system-search-symbolic",
        }
    }

    fn header(self) -> gtk4::Box {
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        header.add_css_class("section-header");
        let image = gtk4::Image::from_icon_name(self.icon());
        image.add_css_class("section-icon");
        header.append(&image);
        let label = gtk4::Label::builder()
            .label(self.title())
            .xalign(0.0)
            .build();
        label.add_css_class("section-title");
        header.append(&label);
        header
    }
}

/// Everything the window needs to render and act, in one place so the many
/// callbacks below capture a single `Rc` instead of a dozen clones each.
pub(crate) struct Ui {
    socket: PathBuf,
    mountpoint: RefCell<PathBuf>,
    /// How a chosen file is launched. Re-read on every activation, so a
    /// change in the app applies the next time the launcher opens.
    opener: RefCell<OpenWith>,

    window: adw::ApplicationWindow,
    entry: gtk4::Entry,
    spinner: Spinner,
    stack: gtk4::Stack,
    scroller: gtk4::ScrolledWindow,
    list: gtk4::ListBox,
    /// The section that starts at each row index, for the header function.
    headers: Rc<RefCell<Vec<Option<Section>>>>,
    placeholder: adw::StatusPage,
    hint: gtk4::Label,
    chips: RefCell<Vec<(Filter, gtk4::ToggleButton)>>,
    preview: Preview,
    /// The hit the preview shows, so a repeated selection does not redo it.
    previewed: RefCell<Option<(bool, String)>>,

    thumbs: Rc<Thumbs>,
    history: History,

    /// Raw (unfiltered) hits from the last reply: search results, or the
    /// pinned files while the query is empty.
    drive_hits: RefCell<Vec<SearchHit>>,
    local_hits: RefCell<Vec<pdfs_core::control::LocalHit>>,
    /// The rows in list order; the cursor indexes into it.
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
    /// The ticket of the Drive open in progress. Escape clears it, and a
    /// reply for a ticket that is no longer current launches nothing.
    opening: Cell<Option<u64>>,
    last_ticket: Cell<u64>,
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
}

pub(crate) fn build_window(app: &adw::Application) -> Option<Rc<Ui>> {
    let dirs = match AppDirs::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("cannot resolve app dirs: {e}");
            return None;
        }
    };

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(gettext("Search Proton Drive"))
        .default_width(860)
        .default_height(560)
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
        .placeholder_text(gettext("Search in Drive and on this computer"))
        .hexpand(true)
        .build();
    entry.add_css_class("search-entry");
    search_bar.append(&entry);

    let spinner = spinner();
    spinner.set_visible(false);
    search_bar.append(&spinner);

    // Translators: the Escape key, shown as a hint that it closes the window.
    let esc = gtk4::Label::new(Some(&gettext("Esc")));
    esc.add_css_class("key-hint");
    search_bar.append(&esc);
    card.append(&search_bar);

    // --- filter chips -----------------------------------------------------
    let chip_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    chip_row.add_css_class("chip-row");
    card.append(&chip_row);

    // --- results and preview ----------------------------------------------
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::Single)
        .activate_on_single_click(false)
        .focusable(false)
        .build();
    list.add_css_class("result-list");
    let headers: Rc<RefCell<Vec<Option<Section>>>> = Rc::new(RefCell::new(Vec::new()));
    let header_of = headers.clone();
    list.set_header_func(move |row, _| {
        let section = header_of
            .borrow()
            .get(row.index() as usize)
            .copied()
            .flatten();
        match section {
            Some(section) => row.set_header(Some(&section.header())),
            None => row.set_header(gtk4::Widget::NONE),
        }
    });

    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .child(&list)
        .build();
    scroller.add_css_class("results");

    let placeholder = adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title(gettext("No results"))
        .build();
    placeholder.add_css_class("compact");

    // The daemon-offline page is a peer of the results, not a modal: the prompt
    // is still usable for nothing else, so it owns the whole view.
    let offline = adw::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title(gettext("Proton Drive is not running"))
        .description(gettext("Start Proton Drive to search your files."))
        .build();
    offline.add_css_class("compact");
    let retry = gtk4::Button::builder()
        .label(gettext("Retry"))
        .halign(gtk4::Align::Center)
        .focus_on_click(false)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    offline.set_child(Some(&retry));

    let stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .transition_duration(120)
        .vexpand(true)
        .hexpand(true)
        .build();
    stack.add_named(&scroller, Some("results"));
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(&offline, Some("offline"));

    let preview = Preview::new();
    let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    body.append(&stack);
    body.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    body.append(&preview.root);
    card.append(&body);

    // --- footer -----------------------------------------------------------
    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    footer.add_css_class("footer");
    let hint = gtk4::Label::builder()
        .label(gettext("Connecting…"))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    hint.add_css_class("footer-text");
    footer.append(&hint);
    // Translators: keyboard hints in the footer; keep the arrow and Enter symbols.
    let keys = gtk4::Label::new(Some(&gettext("↑↓ navigate · ↵ open · Tab filter")));
    keys.add_css_class("footer-text");
    footer.append(&keys);
    card.append(&footer);

    let socket = dirs.control_socket();
    let ui = Rc::new_cyclic(|weak: &std::rc::Weak<Ui>| {
        let weak = weak.clone();
        Ui {
            thumbs: Thumbs::new(socket.clone()),
            // A visit recorded elsewhere, or the first load finishing, adds
            // to the recent list the user may be looking at.
            history: History::start(dirs.state_dir().join("prompt-history.json"), move || {
                if let Some(ui) = weak.upgrade()
                    && ui.showing_suggestions()
                {
                    ui.render();
                }
            }),
            socket,
            mountpoint: RefCell::new(dirs.default_mountpoint()),
            opener: RefCell::new(dirs.load_config().resolved_open_with()),
            window: window.clone(),
            entry: entry.clone(),
            spinner,
            stack,
            scroller,
            list: list.clone(),
            headers,
            placeholder,
            hint,
            chips: RefCell::new(Vec::new()),
            preview,
            previewed: RefCell::new(None),
            drive_hits: RefCell::new(Vec::new()),
            local_hits: RefCell::new(Vec::new()),
            visible: RefCell::new(Vec::new()),
            cursor: Cell::new(None),
            rendered_query: RefCell::new(String::new()),
            filter: Cell::new(Filter::All),
            query_id: Cell::new(0),
            pending: Cell::new(0),
            opening: Cell::new(None),
            last_ticket: Cell::new(0),
            indexing: Cell::new(false),
            open_pending: Cell::new(false),
            suppress_entry_change: Cell::new(false),
        }
    });

    for (filter, label) in FILTERS {
        let chip = gtk4::ToggleButton::builder()
            .label(gettext(label))
            .active(filter == Filter::All)
            .focus_on_click(false)
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

    // A click selects a row and previews it; a double-click opens it.
    let ui_selected = ui.clone();
    list.connect_row_selected(move |_, row| {
        if let Some(row) = row {
            ui_selected.cursor.set(Some(row.index() as usize));
            ui_selected.show_preview();
        }
    });
    let ui_activated = ui.clone();
    list.connect_row_activated(move |_, row| ui_activated.open(row.index() as usize));

    let ui_open = ui.clone();
    ui.preview
        .open
        .connect_clicked(move |_| ui_open.open(ui_open.cursor.get().unwrap_or(0)));
    let ui_reveal = ui.clone();
    ui.preview.reveal.connect_clicked(move |_| {
        if let Some(index) = ui_reveal.cursor.get() {
            ui_reveal.show_in_folder(index);
        }
    });
    let ui_copy = ui.clone();
    ui.preview.copy.connect_clicked(move |_| {
        if let Some(index) = ui_copy.cursor.get() {
            ui_copy.copy_path(index);
        }
    });

    let ui_key = ui.clone();
    let keys = gtk4::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, state| {
        if ui_key.opening.get().is_some() {
            // Escape gives up on a Drive file that is still materialising;
            // everything else waits for it.
            if key == gdk::Key::Escape {
                ui_key.cancel_open();
            }
            return glib::Propagation::Stop;
        }
        match key {
            gdk::Key::Escape => {
                // Spotlight-style: first Escape clears a non-empty query, a
                // second (or an already-empty box) dismisses the launcher.
                if ui_key.entry.text().is_empty() {
                    ui_key.dismiss();
                } else {
                    ui_key.entry.set_text("");
                }
                glib::Propagation::Stop
            }
            gdk::Key::Down => {
                ui_key.move_cursor(1);
                glib::Propagation::Stop
            }
            gdk::Key::Up => {
                ui_key.move_cursor(-1);
                glib::Propagation::Stop
            }
            gdk::Key::Tab | gdk::Key::ISO_Left_Tab => {
                let back = state.contains(gdk::ModifierType::SHIFT_MASK);
                ui_key.cycle_filter(back);
                glib::Propagation::Stop
            }
            // Return is deliberately absent: `GtkText` binds it to `activate`
            // and consumes it, so a bubble-phase controller on the window never
            // sees it while the entry has focus (which is always, here). It is
            // handled on the entry itself — see below.
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(keys);

    // Enter opens the row under the cursor. This lives on the entry rather than
    // with the other keys on the window because `GtkText` claims Return for its
    // own `activate` binding, so a bubble-phase controller upstream of the
    // focused entry is never reached.
    let ui_activate = ui.clone();
    entry.connect_activate(move |_| {
        if ui_activate.opening.get().is_some() {
            return;
        }
        // Fall back to the top row when nothing is explicitly selected (e.g.
        // results haven't rendered yet); `open` guards the rest.
        ui_activate.open(ui_activate.cursor.get().unwrap_or(0));
    });

    // Some GTK/input-method combinations consume the physical Return binding in
    // GtkText without emitting Entry::activate. Capture Return ahead of the text
    // widget and leave every other key to normal GTK/IME handling. Stopping
    // propagation also prevents the activate signal above from opening twice;
    // that signal remains the programmatic/accessibility activation path.
    //
    // Ctrl+C is taken here too, but only while the entry has no selection, so
    // copying part of the query still works; Ctrl+Shift+C always copies the path.
    let ui_return = ui.clone();
    let entry_keys = gtk4::EventControllerKey::new();
    entry_keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    entry_keys.connect_key_pressed(move |_, key, _, state| {
        let control = state.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter => {
                if ui_return.opening.get().is_none() {
                    let index = ui_return.cursor.get().unwrap_or(0);
                    if control {
                        ui_return.show_in_folder(index);
                    } else {
                        ui_return.open(index);
                    }
                }
                glib::Propagation::Stop
            }
            gdk::Key::c | gdk::Key::C if control => {
                let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
                if !shift && ui_return.entry.selection_bounds().is_some() {
                    return glib::Propagation::Proceed;
                }
                if let Some(index) = ui_return.cursor.get() {
                    ui_return.copy_path(index);
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    entry.add_controller(entry_keys);

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

/// Whether the mount has `hit`, asked on a worker thread and given up on
/// after [`STAT_TIMEOUT`]. `None` means "not there" or "no answer"; either
/// way the daemon is the fallback.
async fn mounted_target_within(mountpoint: PathBuf, hit: SearchHit) -> Option<PathBuf> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(mounted_target(&mountpoint, &hit));
    });
    match glib::future_with_timeout(STAT_TIMEOUT, rx.recv()).await {
        Ok(Ok(target)) => target,
        Ok(Err(_)) => None,
        Err(_) => {
            tracing::warn!("the mount did not answer in time; opening through the daemon");
            None
        }
    }
}

impl Ui {
    /// `--preload`: create the surface, renderer and style state now, without
    /// mapping the window, so the first real summon only has to present it.
    pub(crate) fn preload(&self) {
        WidgetExt::realize(&self.window);
    }

    /// Reset and present the resident prompt in response to GApplication
    /// activation. A second invocation while a file is materialising merely
    /// raises the progress window; it must not make the in-flight open mutable.
    pub(crate) fn activate(self: &Rc<Self>) {
        if self.opening.get().is_none() {
            // Preferences may have changed in the app while the launcher was
            // hidden.
            if let Ok(dirs) = AppDirs::new() {
                let config = dirs.load_config();
                set_proton_theme(config.proton_theme.unwrap_or(false));
                *self.opener.borrow_mut() = config.resolved_open_with();
            }
            self.history.prune();
            // Supersede replies from the previous showing before clearing the
            // query. The daemon work may finish, but it can no longer repaint
            // the newly activated prompt with stale rows.
            self.query_id.set(self.query_id.get() + 1);
            self.pending.set(0);
            self.open_pending.set(false);
            self.spinner.set_visible(false);
            self.suppress_entry_change.set(true);
            self.entry.set_text("");
            self.suppress_entry_change.set(false);
            self.set_filter(Filter::All);
            self.connect();
        }
        self.window.present();
        self.entry.grab_focus();
    }

    /// Dismiss without destroying the application window. GTK continues to
    /// associate the hidden window with the single application instance, so a
    /// later desktop shortcut invocation is a cheap `activate` round-trip.
    fn dismiss(&self) {
        self.open_pending.set(false);
        self.thumbs.cancel_all();
        self.window.set_visible(false);
    }

    /// Whether the empty-query view is on screen, so a history update may
    /// repaint it.
    fn showing_suggestions(&self) -> bool {
        self.opening.get().is_none()
            && self.rendered_query.borrow().is_empty()
            && self.entry.text().trim().is_empty()
            && self.stack.visible_child_name().as_deref() != Some("offline")
    }

    /// Ask the daemon for status and, if it answers, load the suggested (pinned)
    /// files. Runs off-thread: a dead daemon must not freeze the first frame.
    fn connect(self: &Rc<Self>) {
        self.hint.set_label(&gettext("Connecting…"));
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
                    ui.hint.set_label(&gettext("Proton Drive isn't running"));
                    ui.stack.set_visible_child_name("offline");
                    ui.preview.show(None, &ui.thumbs, &ui.mountpoint.borrow());
                }
            }
        });
    }

    /// The daemon stopped answering after the window was already open. Flip to
    /// the offline page and lock the entry; the retry button re-runs `connect`.
    fn go_offline(self: &Rc<Self>) {
        self.pending.set(0);
        self.spinner.set_visible(false);
        self.entry.set_sensitive(false);
        self.hint.set_label(&gettext("Proton Drive isn't running"));
        self.stack.set_visible_child_name("offline");
        self.preview
            .show(None, &self.thumbs, &self.mountpoint.borrow());
    }

    /// The empty-query view: recently opened files, then the pinned ones.
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
                        is_dir: pin.is_dir.unwrap_or(pin.recursive),
                        size: 0,
                        modified: 0,
                        pinned: true,
                        cached: pin.cached,
                        uid: pin.uid,
                        mounted_path: None,
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
        if left == 0 && self.opening.get().is_none() {
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

    /// The rows for the current state, with the section each one starts.
    fn rows(&self, query: &str) -> (Vec<Hit>, Vec<Option<Section>>) {
        let filter = self.filter.get();
        let mut hits: Vec<Hit> = self
            .drive_hits
            .borrow()
            .iter()
            .filter(|hit| filter.accepts(&hit.name, hit.is_dir))
            .cloned()
            .map(Hit::Drive)
            .collect();
        hits.extend(
            self.local_hits
                .borrow()
                .iter()
                .filter(|hit| filter.accepts(&hit.name, hit.is_dir))
                .cloned()
                .map(Hit::Local),
        );

        if !query.is_empty() {
            let bonuses = self.history.bonuses();
            rank_hits_by(&mut hits, |hit| {
                bonuses.get(&hit.key()).copied().unwrap_or_default()
            });
            let mut headers = vec![None; hits.len()];
            if let Some(first) = headers.first_mut() {
                *first = Some(Section::Matches);
            }
            return (hits, headers);
        }

        // Recent files first, without the pinned files listed below them.
        let pinned: std::collections::HashSet<(bool, String)> = hits.iter().map(Hit::key).collect();
        let recent: Vec<Hit> = self
            .history
            .recent(usize::MAX)
            .into_iter()
            .filter(|hit| filter.accepts(hit.name(), hit.is_dir()))
            .filter(|hit| !pinned.contains(&hit.key()))
            .take(RECENT_LIMIT)
            .collect();
        let mut headers = vec![None; recent.len() + hits.len()];
        if !recent.is_empty() {
            headers[0] = Some(Section::Recent);
        }
        if !hits.is_empty() {
            headers[recent.len()] = Some(Section::Pinned);
        }
        let mut rows = recent;
        rows.extend(hits);
        (rows, headers)
    }

    /// Rebuild the list, preserving the selected identity when a result
    /// survives the update.
    fn render(self: &Rc<Self>) {
        // Identity of the row under the cursor before we rebuild the list, so a
        // late-arriving second search can't yank the selection back to the top.
        let selected_key = self
            .cursor
            .get()
            .and_then(|i| self.visible.borrow().get(i).map(Hit::key));

        let query = self.entry.text().trim().to_string();
        let searching = !query.is_empty();
        let (visible, headers) = self.rows(&query);
        let total = visible.len();

        self.thumbs.begin_render();
        *self.headers.borrow_mut() = headers;
        self.cursor.set(None);
        self.previewed.borrow_mut().take();
        self.list.remove_all();
        let mountpoint = self.mountpoint.borrow().clone();
        for hit in &visible {
            let row = row::build_row(hit, &query, &self.thumbs, &mountpoint);
            let ui = Rc::downgrade(self);
            let dragged = hit.clone();
            row::attach_drag(&row, hit.fs_path(&mountpoint), move || {
                if let Some(ui) = ui.upgrade() {
                    ui.history.record(dragged.clone());
                    ui.dismiss();
                }
            });
            self.list.append(&row);
        }
        self.list.invalidate_headers();
        self.thumbs.end_render();

        // Find where the previously-selected row landed in the rebuilt list.
        let restored = selected_key.and_then(|key| visible.iter().position(|hit| hit.key() == key));

        *self.visible.borrow_mut() = visible;
        *self.rendered_query.borrow_mut() = query;

        if total == 0 {
            self.placeholder.set_title(&if searching {
                gettext("No results")
            } else {
                gettext("Search your Drive")
            });
            self.placeholder
                .set_description(Some(&match (searching, self.indexing.get()) {
                    (true, true) => gettext(
                        "Still indexing this computer — local results will fill in shortly.",
                    ),
                    (true, false) => gettext("Try a different search, or another filter."),
                    _ => gettext(
                        "Start typing to search Proton Drive and the files on this computer.",
                    ),
                }));
            self.stack.set_visible_child_name("empty");
            self.preview.show(None, &self.thumbs, &mountpoint);
        } else {
            self.stack.set_visible_child_name("results");
            self.select(restored.unwrap_or(0));
        }

        self.refresh_hint();

        // Honour an Enter that arrived before this query rendered, now that the
        // results (and the cursor) are settled.
        if self.open_pending.get() && self.pending.get() == 0 {
            self.open_pending.set(false);
            if let Some(index) = self.cursor.get() {
                self.open(index);
            }
        }
    }

    /// The footer's summary of what the list shows.
    fn refresh_hint(&self) {
        let visible = self.visible.borrow();
        let text = if self.rendered_query.borrow().is_empty() {
            match visible.iter().filter(|hit| hit.pinned()).count() {
                0 => gettext("No pinned files"),
                n => ngettext_f("{n} pinned file", "{n} pinned files", n as u64, &[]),
            }
        } else {
            let drive_count = visible
                .iter()
                .filter(|hit| matches!(hit, Hit::Drive(_)))
                .count();
            let drive = drive_count.to_string();
            let local = (visible.len() - drive_count).to_string();
            let args = [("drive", drive.as_str()), ("local", local.as_str())];
            if self.indexing.get() {
                // Translators: result counts in the footer while this computer is still being indexed; {drive} and {local} are numbers.
                gettext_f(
                    "{drive} in Drive · {local} on this computer · indexing…",
                    &args,
                )
            } else {
                // Translators: result counts in the footer; {drive} and {local} are numbers.
                gettext_f("{drive} in Drive · {local} on this computer", &args)
            }
        };
        self.hint.set_label(&text);
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

    /// Put the cursor on row `index`, preview it and scroll it into view.
    /// Focus stays in the entry so typing never breaks.
    fn select(self: &Rc<Self>, index: usize) {
        let Some(row) = self.list.row_at_index(index as i32) else {
            return;
        };
        self.list.select_row(Some(&row));
        self.cursor.set(Some(index));
        self.show_preview();
        self.scroll_into_view(&row);
    }

    fn show_preview(&self) {
        let hit = self
            .cursor
            .get()
            .and_then(|index| self.visible.borrow().get(index).cloned());
        let key = hit.as_ref().map(Hit::key);
        if *self.previewed.borrow() == key && key.is_some() {
            return;
        }
        *self.previewed.borrow_mut() = key;
        self.preview
            .show(hit.as_ref(), &self.thumbs, &self.mountpoint.borrow());
    }

    /// Keep the selected row inside the viewport, scrolling by the smallest
    /// amount that reveals it (like a native list, unlike jumping it to centre).
    /// A section header above the first row of a section counts as part of it.
    fn scroll_into_view(&self, row: &gtk4::ListBoxRow) {
        let target = row.header().unwrap_or_else(|| row.clone().upcast());
        let (Some(top), Some(bottom)) = (
            target.compute_bounds(&self.list),
            row.compute_bounds(&self.list),
        ) else {
            return;
        };
        let adjustment = self.scroller.vadjustment();
        let (top, bottom) = (f64::from(top.y()), f64::from(bottom.y() + bottom.height()));
        let (value, page) = (adjustment.value(), adjustment.page_size());
        if top < value {
            adjustment.set_value(top);
        } else if bottom > value + page {
            adjustment.set_value(bottom - page);
        }
    }

    /// The hit at `index`, if it belongs to the query on screen. The rows may
    /// still show an older query whose reply is settling; nothing may act on
    /// them then.
    fn current_hit(&self, index: usize) -> Option<Hit> {
        if self.entry.text().trim() != *self.rendered_query.borrow() {
            return None;
        }
        self.visible.borrow().get(index).cloned()
    }

    /// Hand a path to the configured opener — `xdg-open` unless `open_with` in
    /// `config.json` says otherwise — remember the visit, and close.
    fn launch(&self, hit: &Hit, path: &Path, is_dir: bool) {
        opener::open(&self.opener.borrow(), path, is_dir);
        self.history.record(hit.clone());
        self.dismiss();
    }

    /// Open the hit at `index`. Local files are already on disk, so they hand
    /// straight to the desktop. A Drive file opens from the mount when the
    /// mount has it, and is otherwise materialised by the daemon (`OpenFile`),
    /// which can take a moment — the window stays up, with the spinner
    /// running, until the path comes back or Escape gives up on it.
    fn open(self: &Rc<Self>, index: usize) {
        if self.opening.get().is_some() {
            return;
        }
        // Enter must never launch a file the user has typed past. But don't
        // drop the intent either: remember it and open the right row once the
        // fresh results land (see `render`).
        if self.entry.text().trim() != *self.rendered_query.borrow() {
            self.open_pending.set(true);
            return;
        }
        let Some(hit) = self.current_hit(index) else {
            return;
        };
        let drive = match &hit {
            Hit::Local(local) => {
                self.launch(&hit, Path::new(&local.path), local.is_dir);
                return;
            }
            Hit::Drive(drive) => drive.clone(),
        };

        // Pins come from ListPins, whose `recursive` flag is pin policy rather
        // than reliable node kind metadata. Every pin already has a mounted
        // path, so activate that path directly; this handles non-recursively
        // pinned folders without mistaking them for files and sending an
        // invalid OpenFile request.
        let pin_row = self.rendered_query.borrow().is_empty() && drive.pinned;
        if pin_row
            || matches!(
                drive_activation(&drive.name, drive.is_dir),
                DriveActivation::Folder | DriveActivation::MountedMedia
            )
        {
            let path = mounted_or_relative(&self.mountpoint.borrow(), &drive);
            self.launch(&hit, &path, drive.is_dir);
            return;
        }

        let ticket = self.last_ticket.get() + 1;
        self.last_ticket.set(ticket);
        self.opening.set(Some(ticket));
        self.spinner.set_visible(true);
        // Translators: {name} is a file name.
        let opening = gettext_f("Opening {name}…", &[("name", &drive.name)]);
        self.hint.set_label(&opening);

        let mountpoint = self.mountpoint.borrow().clone();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            // The mount holds the real file; the cache holds a copy an editor
            // would save into for nothing. Prefer the mount, and materialise
            // only what it does not expose.
            if let Some(path) = mounted_target_within(mountpoint, drive.clone()).await {
                if ui.finish_open(ticket) {
                    ui.launch(&hit, &path, false);
                }
                return;
            }
            if ui.opening.get() != Some(ticket) {
                return;
            }
            let rx = spawn_request(
                ui.socket.clone(),
                Request::OpenFile {
                    path: drive.path.clone(),
                    uid: Some(drive.uid.clone()),
                },
            );
            let reply = rx.recv().await;
            if !ui.finish_open(ticket) {
                return;
            }
            match reply {
                Ok(Ok(Response::FilePath { path })) => {
                    // The blob is named after its content hash, so the open
                    // rules have to be matched against the Drive name instead.
                    opener::open_named(&ui.opener.borrow(), Path::new(&path), &drive.name, false);
                    ui.history.record(hit.clone());
                    ui.dismiss();
                }
                Ok(Ok(Response::Error { message, .. })) => {
                    // Translators: {message} is an error from the daemon, in English.
                    ui.hint.set_label(&gettext_f(
                        "Could not open: {message}",
                        &[("message", &message)],
                    ));
                }
                _ => ui.hint.set_label(&gettext("Couldn't reach Proton Drive")),
            }
        });
    }

    /// End the open with `ticket`. False when it was cancelled or superseded,
    /// in which case the caller must not launch anything.
    fn finish_open(&self, ticket: u64) -> bool {
        if self.opening.get() != Some(ticket) {
            return false;
        }
        self.opening.set(None);
        self.spinner.set_visible(self.pending.get() > 0);
        true
    }

    /// Give up on the Drive open in progress. The daemon may still finish
    /// materialising the file, but it is not launched.
    fn cancel_open(&self) {
        self.opening.set(None);
        self.spinner.set_visible(self.pending.get() > 0);
        self.refresh_hint();
    }

    /// Show the hit at `index` selected in the file manager, through the
    /// freedesktop `FileManager1` interface, or open its folder when no file
    /// manager offers that.
    fn show_in_folder(self: &Rc<Self>, index: usize) {
        if self.opening.get().is_some() {
            return;
        }
        let Some(hit) = self.current_hit(index) else {
            return;
        };
        let path = hit.fs_path(&self.mountpoint.borrow());
        let uri = gio::File::for_path(&path).uri().to_string();
        let Some(connection) = self
            .window
            .application()
            .and_then(|app| app.dbus_connection())
        else {
            self.open_parent(&hit, &path);
            return;
        };
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let shown = connection
                .call_future(
                    Some("org.freedesktop.FileManager1"),
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1",
                    "ShowItems",
                    Some(&(vec![uri], String::new()).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    2000,
                )
                .await;
            match shown {
                Ok(_) => {
                    ui.history.record(hit);
                    ui.dismiss();
                }
                Err(e) => {
                    tracing::debug!("FileManager1.ShowItems failed: {e}");
                    ui.open_parent(&hit, &path);
                }
            }
        });
    }

    fn open_parent(&self, hit: &Hit, path: &Path) {
        let Some(parent) = path.parent() else {
            return;
        };
        opener::open(&self.opener.borrow(), parent, true);
        self.history.record(hit.clone());
        self.dismiss();
    }

    /// Copy the path of the hit at `index`: the file itself for a local hit,
    /// its place in the mount for a Drive hit.
    fn copy_path(&self, index: usize) {
        let Some(hit) = self.current_hit(index) else {
            return;
        };
        let path = hit.fs_path(&self.mountpoint.borrow());
        self.entry.clipboard().set_text(&path.display().to_string());
        self.preview.flash_copied();
    }
}
