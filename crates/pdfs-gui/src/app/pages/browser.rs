use crate::activation::{DriveActivation, drive_activation, mounted_path, mounted_target_rel};
use crate::*;

pub(crate) struct BrowserState {
    // Files (browser) page.
    /// Shared model behind the grid and column views; repopulated per directory.
    pub(crate) model: gio::ListStore,
    pub(crate) back: gtk4::Button,
    /// Clickable breadcrumb trail (a button per path segment); rebuilt per load
    /// by [`repaint_crumb`] so each ancestor folder navigates on click.
    pub(crate) crumb: gtk4::Box,
    /// Swaps the Files content area between the grid/list views and the status
    /// page below; see [`browser_status`].
    pub(crate) content: gtk4::Stack,
    /// The Files empty/loading/error surface, shown in place of the views.
    pub(crate) status: adw::StatusPage,
    /// Sits in [`Self::status`]; shown when a load failed because the mount
    /// service is down (not merely starting); restarts the service and reloads.
    pub(crate) retry: gtk4::Button,
    /// The details pane's host; `show_sidebar` is what reveals/hides the pane.
    pub(crate) split: adw::OverlaySplitView,
    /// Mountpoint-relative path the browser is showing (empty = root).
    pub(crate) path: RefCell<String>,
    /// Debounced full-text search box in the browser header.
    pub(crate) search: gtk4::SearchEntry,
    /// The folder-level actions, insensitive while the mount is down: without a
    /// daemon they can only fail, and a greyed button says so before the click.
    pub(crate) new_folder: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    pub(crate) upload_folder: gtk4::Button,
    /// Starts a daemon-side recursive local-thumbnail build for [`Self::path`].
    pub(crate) build_thumbnails: gtk4::Button,
    pub(crate) thumbnail_build_row: gtk4::Box,
    pub(crate) thumbnail_progress: gtk4::ProgressBar,
    pub(crate) thumbnail_status: gtk4::Label,
    pub(crate) thumbnail_poll: RefCell<Option<glib::SourceId>>,
    pub(crate) thumbnail_build_running: Cell<bool>,
    pub(crate) thumbnail_cancel_pending: Cell<bool>,
    /// Pending debounce timer for the search box; replaced on every keystroke so
    /// only the last pause actually fires a [`Request::Search`].
    pub(crate) search_source: RefCell<Option<glib::SourceId>>,
    /// Identity of the newest folder/search request. Paths and queries can be
    /// requested repeatedly, so comparing their text alone cannot reject an
    /// older response that finishes after a manual refresh.
    pub(crate) load_generation: Cell<u64>,
    /// The grid/list view stack, read to find out which of the two selections is
    /// the one the user is actually working in.
    pub(crate) views: gtk4::Stack,
    /// The bulk-action bar, revealed once more than one entry is selected.
    pub(crate) bulk: gtk4::Revealer,
    pub(crate) bulk_label: gtk4::Label,
    /// Bulk buttons, sensitive only where they apply (offline state is a
    /// file-only notion, so a folder in the selection disables those two).
    pub(crate) bulk_trash: gtk4::Button,
    pub(crate) bulk_pin: gtk4::Button,
    pub(crate) bulk_unpin: gtk4::Button,
    /// Upload / New folder offered on the *empty folder* status page, so that
    /// state is a place to act rather than a dead end. Hidden on every other
    /// status (a load error is not the moment to offer an upload).
    pub(crate) empty_actions: gtk4::Box,
    /// Dolphin-style bottom status bar: current listing counts, the icon-grid
    /// zoom value, and Proton account storage usage.
    pub(crate) summary: gtk4::Label,
    pub(crate) zoom: gtk4::Scale,
    pub(crate) grid_thumbnail_size: Cell<i32>,
    /// Weak references to realised grid cells. Zoom resizes only these visible,
    /// recycled surfaces instead of invalidating the whole list model.
    pub(crate) grid_tiles: RefCell<Vec<(glib::WeakRef<gtk4::Overlay>, glib::WeakRef<gtk4::Label>)>>,
    pub(crate) quota_box: gtk4::Box,
    pub(crate) quota: gtk4::ProgressBar,
    pub(crate) quota_text: gtk4::Label,
}

/// Idle pause after the last keystroke before a search query is sent, so typing
/// doesn't fire a request per character.
pub(crate) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Cap on search hits requested from the daemon.
pub(crate) const SEARCH_LIMIT: usize = 200;

/// Browser grid thumbnail size controlled by the bottom zoom slider. The
/// default preserves the former fixed 72 px presentation.
pub(crate) const GRID_THUMB_MIN: i32 = 48;
pub(crate) const GRID_THUMB_MAX: i32 = 144;
pub(crate) const GRID_THUMB_DEFAULT: i32 = 72;
pub(crate) const GRID_THUMB_STEP: i32 = 8;

/// The Files page: a Nautilus-style file manager. A back/breadcrumb header with
/// a grid/list view toggle sits over a [`gtk4::Stack`] that swaps between an
/// **icon grid** ([`gtk4::GridView`]) and a **column list** ([`gtk4::ColumnView`]
/// with Name / Size / Modified columns). Both views are driven by one shared
/// [`gio::ListStore`] of [`BoxedAnyObject`]-wrapped [`DirEntry`]s, so a directory
/// load repopulates the model once and both views update.
///
/// The factories that render entries — and the columns — need the [`Ui`] handle
/// for activation and the right-click menu, so they're installed later in
/// [`wire_browser`]; this builder only assembles the empty widgets.
///
/// Empty / loading / error outcomes aren't a label under the header: the whole
/// content area swaps to a centred [`adw::StatusPage`] (see [`browser_status`]),
/// so "this folder is empty" and "the mount is down" read as first-class states
/// rather than a stray line above a blank grid.
pub(crate) struct BrowserWidgets {
    pub(crate) model: gio::ListStore,
    pub(crate) back: gtk4::Button,
    pub(crate) crumb: gtk4::Box,
    pub(crate) grid: gtk4::GridView,
    pub(crate) column_view: gtk4::ColumnView,
    /// Swaps the content area between the grid/list views and the status page.
    pub(crate) content: gtk4::Stack,
    /// The empty/loading/error surface shown in place of the views.
    pub(crate) status: adw::StatusPage,
    /// Sits in the status page; shown only when the mount service is down.
    pub(crate) retry: gtk4::Button,
    pub(crate) search: gtk4::SearchEntry,
    pub(crate) new_folder: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    pub(crate) upload_folder: gtk4::Button,
    pub(crate) build_thumbnails: gtk4::Button,
    pub(crate) thumbnail_build_row: gtk4::Box,
    pub(crate) thumbnail_progress: gtk4::ProgressBar,
    pub(crate) thumbnail_status: gtk4::Label,
    pub(crate) summary: gtk4::Label,
    pub(crate) zoom: gtk4::Scale,
    pub(crate) quota_box: gtk4::Box,
    pub(crate) quota: gtk4::ProgressBar,
    pub(crate) quota_text: gtk4::Label,
    pub(crate) refresh: gtk4::Button,
    /// Wraps the views + the details pane; the pane slides in on selection.
    pub(crate) split: adw::OverlaySplitView,
    pub(crate) details: DetailsWidgets,
    /// The two selection models, so a selection change can drive the details pane
    /// and so an action can re-read the entries the user has highlighted.
    pub(crate) grid_selection: gtk4::MultiSelection,
    pub(crate) list_selection: gtk4::MultiSelection,
    /// The grid/list stack, so the page can tell which selection is live.
    pub(crate) views: gtk4::Stack,
    pub(crate) bulk: gtk4::Revealer,
    pub(crate) bulk_label: gtk4::Label,
    pub(crate) bulk_trash: gtk4::Button,
    pub(crate) bulk_pin: gtk4::Button,
    pub(crate) bulk_unpin: gtk4::Button,
    pub(crate) bulk_clear: gtk4::Button,
    pub(crate) empty_actions: gtk4::Box,
    pub(crate) empty_upload: gtk4::Button,
    pub(crate) empty_new_folder: gtk4::Button,
}

pub(crate) fn build_browser_page() -> (gtk4::Widget, BrowserWidgets) {
    let model = gio::ListStore::new::<BoxedAnyObject>();

    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Up one folder")
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    back.add_css_class("flat");
    // Clickable breadcrumb trail; `repaint_crumb` fills it per load. Wrapped in a
    // horizontally-scrolling viewport so a deep path can't shove the search box
    // and view toggles off the right edge.
    let crumb = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    crumb.set_valign(gtk4::Align::Center);
    let crumb_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::External)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .hexpand(true)
        .child(&crumb)
        .build();

    // Folder-level actions: create a subfolder / upload a file into the current
    // directory. Wired in `wire_browser_actions`.
    let new_folder = gtk4::Button::builder()
        .icon_name("folder-new-symbolic")
        .tooltip_text("New folder")
        .valign(gtk4::Align::Center)
        .build();
    new_folder.add_css_class("flat");
    let upload = gtk4::Button::builder()
        .icon_name("document-send-symbolic")
        .tooltip_text("Upload files")
        .valign(gtk4::Align::Center)
        .build();
    upload.add_css_class("flat");
    let upload_folder = gtk4::Button::builder()
        .icon_name("pdfs-folder-upload-symbolic")
        .tooltip_text("Upload folder")
        .valign(gtk4::Align::Center)
        .build();
    upload_folder.add_css_class("flat");
    let build_thumbnails = gtk4::Button::builder()
        .icon_name("pdfs-build-thumbnails-symbolic")
        .tooltip_text("Build thumbnails in this folder and its subfolders")
        .valign(gtk4::Align::Center)
        .build();
    build_thumbnails.add_css_class("flat");

    // Linked grid/list toggle, top-right, Nautilus-style.
    let grid_toggle = gtk4::ToggleButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("Grid view")
        .active(true)
        .build();
    let list_toggle = gtk4::ToggleButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("List view")
        .build();
    list_toggle.set_group(Some(&grid_toggle));
    let toggles = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    toggles.add_css_class("linked");
    toggles.append(&grid_toggle);
    toggles.append(&list_toggle);

    let search = gtk4::SearchEntry::builder()
        .placeholder_text("Search Drive")
        .valign(gtk4::Align::Center)
        .build();
    search.set_width_chars(18);

    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&back);
    header.append(&crumb_scroll);
    header.append(&refresh);
    header.append(&build_thumbnails);
    header.append(&new_folder);
    header.append(&upload);
    header.append(&upload_folder);
    header.append(&search);
    header.append(&toggles);

    let thumbnail_status = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    thumbnail_status.add_css_class("caption");
    let thumbnail_progress = gtk4::ProgressBar::builder().hexpand(true).build();
    let thumbnail_build_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    thumbnail_build_row.append(&thumbnail_status);
    thumbnail_build_row.append(&thumbnail_progress);
    thumbnail_build_row.set_visible(false);

    // Empty / loading / error surface, shown in place of the views.
    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    // The empty-folder state's way out. `StatusPage` takes one child, so Retry
    // and these share a box; each is shown only for the state it belongs to.
    let empty_upload = gtk4::Button::builder().label("Upload files").build();
    empty_upload.add_css_class("pill");
    empty_upload.add_css_class("suggested-action");
    let empty_new_folder = gtk4::Button::builder().label("New folder").build();
    empty_new_folder.add_css_class("pill");
    let empty_actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    empty_actions.set_halign(gtk4::Align::Center);
    empty_actions.set_visible(false);
    empty_actions.append(&empty_upload);
    empty_actions.append(&empty_new_folder);

    let status_child = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    status_child.append(&retry);
    status_child.append(&empty_actions);

    let status = adw::StatusPage::builder()
        .icon_name("folder-symbolic")
        .vexpand(true)
        .child(&status_child)
        .build();
    status.add_css_class("compact");

    // Icon grid. Multi-select: acting on a batch is the common case for trashing
    // and for taking a folder's worth of files offline, and doing it one
    // confirmation dialog at a time is not a workflow.
    let grid_selection = gtk4::MultiSelection::new(Some(model.clone()));
    let grid = gtk4::GridView::builder()
        .model(&grid_selection)
        .min_columns(2)
        .max_columns(10)
        .build();
    grid.add_css_class("file-grid");
    let grid_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&grid)
        .build();

    // Column list, with its own selection over the same model.
    let list_selection = gtk4::MultiSelection::new(Some(model.clone()));
    let column_view = gtk4::ColumnView::builder().model(&list_selection).build();
    column_view.add_css_class("data-table");
    let column_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&column_view)
        .build();

    // Stack swapped by the toggle buttons.
    let view_stack = gtk4::Stack::new();
    view_stack.set_vexpand(true);
    view_stack.add_named(&grid_scroll, Some("grid"));
    view_stack.add_named(&column_scroll, Some("list"));
    let vs = view_stack.clone();
    grid_toggle.connect_toggled(move |b| {
        if b.is_active() {
            vs.set_visible_child_name("grid");
        }
    });
    let vs = view_stack.clone();
    list_toggle.connect_toggled(move |b| {
        if b.is_active() {
            vs.set_visible_child_name("list");
        }
    });

    // Outer stack: the views, or the status page when there's nothing to show.
    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&view_stack, Some("views"));
    content.add_named(&status, Some("status"));

    // The details pane slides in from the right when an entry is selected.
    let (details_pane, details) = build_details_pane();
    let split = adw::OverlaySplitView::builder()
        .sidebar_position(gtk4::PackType::End)
        .collapsed(true)
        .show_sidebar(false)
        .max_sidebar_width(300.0)
        .content(&content)
        .sidebar(&details_pane)
        .build();

    // Bulk-action bar. A revealer rather than a hidden box so it slides in
    // instead of making the whole view jump when a second entry is selected.
    let bulk_label = gtk4::Label::builder().hexpand(true).xalign(0.0).build();
    let bulk_pin = gtk4::Button::builder()
        .label("Keep offline")
        .valign(gtk4::Align::Center)
        .build();
    bulk_pin.add_css_class("flat");
    let bulk_unpin = gtk4::Button::builder()
        .label("Remove offline copy")
        .valign(gtk4::Align::Center)
        .build();
    bulk_unpin.add_css_class("flat");
    let bulk_trash = gtk4::Button::builder()
        .label("Move to Trash")
        .valign(gtk4::Align::Center)
        .build();
    bulk_trash.add_css_class("destructive-action");
    let bulk_clear = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Clear selection (Esc)")
        .valign(gtk4::Align::Center)
        .build();
    bulk_clear.add_css_class("flat");
    bulk_clear.add_css_class("circular");
    let bulk_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    bulk_box.add_css_class("toolbar");
    bulk_box.add_css_class("bulk-bar");
    bulk_box.append(&bulk_label);
    bulk_box.append(&bulk_pin);
    bulk_box.append(&bulk_unpin);
    bulk_box.append(&bulk_trash);
    bulk_box.append(&bulk_clear);
    let bulk = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideDown)
        .child(&bulk_box)
        .build();

    // Mirror DolphinStatusBar's full-width order: contextual text (stretch 1),
    // "Zoom:", its slider, then capacity information. Both visible troughs use
    // the same CSS dimensions.
    let summary = gtk4::Label::builder()
        .label("Loading…")
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Center)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .hexpand(true)
        .build();
    let zoom_label = gtk4::Label::new(Some("Zoom:"));
    zoom_label.set_valign(gtk4::Align::Center);
    let zoom = gtk4::Scale::with_range(
        gtk4::Orientation::Horizontal,
        f64::from(GRID_THUMB_MIN),
        f64::from(GRID_THUMB_MAX),
        f64::from(GRID_THUMB_STEP),
    );
    zoom.set_value(f64::from(GRID_THUMB_DEFAULT));
    zoom.set_draw_value(false);
    zoom.set_valign(gtk4::Align::Center);
    zoom.set_tooltip_text(Some("Size: 72 pixels"));
    zoom.add_css_class("browser-status-meter");

    let quota = gtk4::ProgressBar::new();
    quota.set_hexpand(false);
    quota.set_valign(gtk4::Align::Center);
    quota.set_tooltip_text(Some("Proton account storage"));
    quota.add_css_class("browser-status-meter");
    let quota_text = gtk4::Label::builder()
        .label("Loading…")
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Center)
        .margin_end(6)
        .tooltip_text("Proton account storage")
        .build();
    let quota_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    quota_box.set_valign(gtk4::Align::Center);
    quota_box.append(&quota);
    quota_box.append(&quota_text);
    quota_box.set_visible(false);

    let status_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    status_bar.add_css_class("browser-statusbar");
    status_bar.set_margin_start(6);
    status_bar.set_margin_end(2);
    status_bar.append(&summary);
    status_bar.append(&zoom_label);
    status_bar.append(&zoom);
    status_bar.append(&quota_box);

    let zoom_label_for_view = zoom_label.clone();
    let zoom_for_view = zoom.clone();
    let content_for_view = content.clone();
    view_stack.connect_visible_child_name_notify(move |stack| {
        let visible = content_for_view.visible_child_name().as_deref() == Some("views")
            && stack.visible_child_name().as_deref() == Some("grid");
        zoom_label_for_view.set_visible(visible);
        zoom_for_view.set_visible(visible);
    });
    let zoom_label_for_content = zoom_label.clone();
    let zoom_for_content = zoom.clone();
    let view_for_content = view_stack.clone();
    content.connect_visible_child_name_notify(move |stack| {
        let visible = stack.visible_child_name().as_deref() == Some("views")
            && view_for_content.visible_child_name().as_deref() == Some("grid");
        zoom_label_for_content.set_visible(visible);
        zoom_for_content.set_visible(visible);
    });

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    body.set_vexpand(true);
    body.set_margin_top(12);
    body.set_margin_bottom(6);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.append(&header);
    body.append(&thumbnail_build_row);
    body.append(&bulk);
    body.append(&split);

    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    page.append(&body);
    page.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    page.append(&status_bar);

    (
        page.upcast(),
        BrowserWidgets {
            model,
            back,
            crumb,
            grid,
            column_view,
            content,
            status,
            retry,
            search,
            new_folder,
            upload,
            upload_folder,
            build_thumbnails,
            thumbnail_build_row,
            thumbnail_progress,
            thumbnail_status,
            summary,
            zoom,
            quota_box,
            quota,
            quota_text,
            refresh,
            split,
            details,
            grid_selection,
            list_selection,
            views: view_stack,
            bulk,
            bulk_label,
            bulk_trash,
            bulk_pin,
            bulk_unpin,
            bulk_clear,
            empty_actions,
            empty_upload,
            empty_new_folder,
        },
    )
}

/// Swap the Files content area to the status page, with a Retry button only when
/// the failure is one the user can act on.
pub(crate) fn browser_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.browser.status.set_icon_name(Some(icon));
    ui.browser.status.set_title(title);
    ui.browser.status.set_description(Some(description));
    ui.browser.retry.set_visible(retry);
    // Only the empty-folder state offers a way to fill the folder; every other
    // status (loading, offline, error) turns them back off.
    ui.browser.empty_actions.set_visible(false);
    ui.browser.content.set_visible_child_name("status");
    hide_details(ui);
}

/// The selection model of whichever view is on screen. The grid and the list
/// each own one over the same model, so "what is selected" depends on which the
/// user is looking at.
pub(crate) fn active_selection(ui: &Rc<Ui>) -> gtk4::MultiSelection {
    if ui.browser.views.visible_child_name().as_deref() == Some("list") {
        ui.details.list_selection.clone()
    } else {
        ui.details.grid_selection.clone()
    }
}

/// Every entry highlighted in the view on screen, in model order.
///
/// Walks the model rather than the selection bitset: a listing is at most a few
/// thousand rows, and asking each position whether it is selected keeps this
/// free of bitset-iterator lifetimes for no measurable cost.
pub(crate) fn selected_entries(ui: &Rc<Ui>) -> Vec<DirEntry> {
    let selection = active_selection(ui);
    let count = selection.n_items();
    (0..count)
        .filter(|i| selection.is_selected(*i))
        .filter_map(|i| entry_at(Some(&selection), i))
        .collect()
}

/// Drop the selection in both views, which also retracts the bulk bar.
pub(crate) fn clear_selection(ui: &Rc<Ui>) {
    ui.details.grid_selection.unselect_all();
    ui.details.list_selection.unselect_all();
}

/// Reflect the current selection in the bulk bar: how many are selected, and
/// which bulk actions apply to that mix. Offline state is a file-only notion, so
/// a folder anywhere in the selection disables the two pin buttons rather than
/// half-failing once pressed.
pub(crate) fn sync_bulk_bar(ui: &Rc<Ui>) {
    let entries = selected_entries(ui);
    // One selected entry is the details pane's job; the bar is for a batch.
    if entries.len() < 2 {
        ui.browser.bulk.set_reveal_child(false);
        return;
    }
    let files_only = entries.iter().all(|e| !e.is_dir);
    let any_pinned = entries.iter().any(|e| e.pinned);
    let any_unpinned = entries.iter().any(|e| !e.pinned);
    let mounted = *ui.mounted.borrow();
    ui.browser
        .bulk_label
        .set_label(&format!("{} selected", entries.len()));
    ui.browser.bulk_trash.set_sensitive(mounted);
    ui.browser
        .bulk_pin
        .set_sensitive(mounted && files_only && any_unpinned);
    ui.browser
        .bulk_unpin
        .set_sensitive(mounted && files_only && any_pinned);
    ui.browser.bulk.set_reveal_child(true);
}

/// Confirm and move every selected entry to Trash.
///
/// One dialog for the batch, one pass over the daemon, one toast — and one Undo
/// that restores the whole batch, because a mis-aimed bulk delete is exactly the
/// action a user most needs to take back.
pub(crate) fn prompt_delete_many(ui: &Rc<Ui>, entries: Vec<DirEntry>) {
    if entries.len() < 2 {
        if let Some(entry) = entries.first() {
            prompt_delete(ui, entry);
        }
        return;
    }
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Move to Trash")
        .body(format!(
            "Move {} items to Trash? Anything inside a selected folder goes with it.",
            entries.len()
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("trash", "Move to Trash");
    dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "trash" {
            run_bulk_delete(&ui, entries.clone());
        }
    });
    dialog.present(win.as_ref());
}

/// Trash a batch, one request at a time.
///
/// Sequential rather than parallel: the daemon serialises these anyway, and a
/// burst of threads against one socket buys nothing but a harder failure to
/// report. Partial success is reported as such — the entries that did move are
/// still restorable through the Undo.
pub(crate) fn run_bulk_delete(ui: &Rc<Ui>, entries: Vec<DirEntry>) {
    if !*ui.mounted.borrow() {
        toast_error(
            ui,
            "Couldn't move to Trash",
            "Proton Drive isn't connected.",
        );
        return;
    }
    let socket = ui.dirs.control_socket();
    // A one-entry batch is still a batch, but it is worth naming in the toast:
    // "Moved “notes.txt” to Trash" tells the user which file far better than a
    // count of one does.
    let single = (entries.len() == 1).then(|| entries[0].name.clone());
    let paths: Vec<(String, String)> = entries
        .iter()
        .map(|e| (entry_rel(ui, e), e.uid.clone()))
        .collect();
    ui.busy_begin();
    clear_selection(ui);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let mut trashed: Vec<String> = Vec::new();
        let mut failure: Option<String> = None;
        for (path, uid) in paths {
            let rx = spawn_request(socket.clone(), Request::Delete { path });
            match rx.recv().await {
                Ok(Ok(Response::Ok { .. })) => trashed.push(uid),
                Ok(Ok(Response::Error { message, .. })) => {
                    failure.get_or_insert(message);
                }
                _ => {
                    failure.get_or_insert_with(|| "The mount service didn't respond.".to_string());
                }
            }
        }
        ui.busy_end();
        reload_listing(&ui);
        match (trashed.len(), failure) {
            (0, Some(message)) => toast_error(&ui, "Couldn't move to Trash", &message),
            (0, None) => {}
            (n, failure) => {
                let message = match (failure, &single) {
                    (Some(_), _) => format!("Moved {n} items to Trash — some couldn't be moved"),
                    (None, Some(name)) => format!("Moved “{name}” to Trash"),
                    (None, None) => format!("Moved {n} items to Trash"),
                };
                toast_action(&ui, &message, "Undo", move |ui| {
                    restore_uids(ui, trashed.clone(), n);
                });
            }
        }
    });
}

/// Put trashed nodes back where they came from — the Undo behind a trash toast.
pub(crate) fn restore_uids(ui: &Rc<Ui>, uids: Vec<String>, count: usize) {
    run_mutation(
        ui,
        Request::Restore { uids },
        if count == 1 {
            "Restored from Trash".to_string()
        } else {
            format!("Restored {count} items from Trash")
        },
        "Couldn't restore from Trash",
    );
}

/// Pin or unpin every selected file, one request at a time.
pub(crate) fn run_bulk_pin(ui: &Rc<Ui>, entries: Vec<DirEntry>, pin: bool) {
    let socket = ui.dirs.control_socket();
    // Only the entries that need changing: re-pinning an already-pinned file is
    // a wasted round-trip, and it would inflate the count the toast reports.
    let paths: Vec<String> = entries
        .iter()
        .filter(|e| !e.is_dir && e.pinned != pin)
        .map(|e| entry_rel(ui, e))
        .collect();
    if paths.is_empty() {
        return;
    }
    ui.busy_begin();
    clear_selection(ui);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let mut done = 0usize;
        let mut failure: Option<String> = None;
        for path in paths {
            let req = if pin {
                Request::Pin { path }
            } else {
                Request::Unpin { path }
            };
            let rx = spawn_request(socket.clone(), req);
            match rx.recv().await {
                Ok(Ok(Response::Ok { .. })) => done += 1,
                Ok(Ok(Response::Error { message, .. })) => {
                    failure.get_or_insert(message);
                }
                _ => {
                    failure.get_or_insert_with(|| "The mount service didn't respond.".to_string());
                }
            }
        }
        ui.busy_end();
        load_browser(&ui);
        match (done, failure) {
            (0, Some(message)) => toast_error(&ui, "Couldn't change offline state", &message),
            (0, None) => {}
            (n, _) => toast(
                &ui,
                &if pin {
                    format!("{n} files are now available offline")
                } else {
                    format!("{n} files are no longer kept offline")
                },
            ),
        }
    });
}

/// Wire the bulk-action bar and the empty-folder state's buttons.
pub(crate) fn wire_bulk(
    ui: &Rc<Ui>,
    bulk_clear: &gtk4::Button,
    empty_upload: &gtk4::Button,
    empty_new_folder: &gtk4::Button,
) {
    let ui_trash = ui.clone();
    ui.browser
        .bulk_trash
        .connect_clicked(move |_| prompt_delete_many(&ui_trash, selected_entries(&ui_trash)));
    let ui_pin = ui.clone();
    ui.browser
        .bulk_pin
        .connect_clicked(move |_| run_bulk_pin(&ui_pin, selected_entries(&ui_pin), true));
    let ui_unpin = ui.clone();
    ui.browser
        .bulk_unpin
        .connect_clicked(move |_| run_bulk_pin(&ui_unpin, selected_entries(&ui_unpin), false));
    let ui_clear = ui.clone();
    bulk_clear.connect_clicked(move |_| {
        clear_selection(&ui_clear);
        sync_bulk_bar(&ui_clear);
    });

    // Switching views switches which selection is live, so the bar has to be
    // re-derived rather than left showing the other view's count.
    let ui_view = ui.clone();
    ui.browser
        .views
        .connect_visible_child_name_notify(move |_| sync_bulk_bar(&ui_view));

    let ui_upload = ui.clone();
    empty_upload.connect_clicked(move |_| prompt_upload(&ui_upload));
    let ui_new = ui.clone();
    empty_new_folder.connect_clicked(move |_| prompt_new_folder(&ui_new));
}

/// Swap the Files content area back to the grid/list views.
pub(crate) fn browser_views(ui: &Rc<Ui>) {
    ui.browser.content.set_visible_child_name("views");
}

/// Install the entry factories, columns, activation handlers and the back
/// button. Split out from [`build_browser_page`] because every renderer needs
/// the [`Ui`] handle to open entries and raise the context menu.
pub(crate) fn wire_browser(ui: &Rc<Ui>, grid: &gtk4::GridView, column_view: &gtk4::ColumnView) {
    // Back: pop one path segment and reload.
    let ui_back = ui.clone();
    ui.browser.back.clone().connect_clicked(move |_| {
        {
            let mut path = ui_back.browser.path.borrow_mut();
            *path = match path.rfind('/') {
                Some(i) => path[..i].to_string(),
                None => String::new(),
            };
        }
        load_browser(&ui_back);
    });

    // Resize only realised grid cells. Rebuilding the whole model for every
    // slider step would repeatedly tear down selection state while the pointer
    // is still moving.
    let zoom = ui.browser.zoom.clone();
    let ui_zoom = ui.clone();
    let grid_zoom = grid.clone();
    zoom.connect_value_changed(move |scale| {
        let size = (scale.value().round() as i32).clamp(GRID_THUMB_MIN, GRID_THUMB_MAX);
        scale.set_tooltip_text(Some(&format!("Size: {size} pixels")));
        if ui_zoom.browser.grid_thumbnail_size.replace(size) == size {
            return;
        }
        ui_zoom
            .browser
            .grid_tiles
            .borrow_mut()
            .retain(|(thumbnail_ref, label_ref)| {
                let (Some(thumbnail), Some(label)) = (thumbnail_ref.upgrade(), label_ref.upgrade())
                else {
                    return false;
                };
                resize_grid_tile(&thumbnail, &label, size);
                true
            });
        grid_zoom.queue_resize();
    });

    // Grid tiles: a thumbnail over an ellipsized name, with a right-click menu.
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let size = ui.browser.grid_thumbnail_size.get();
            let thumbnail = file_thumbnail_widget(size, grid_fallback_size(size));
            // Keep the sync-state badge inside the thumbnail surface.
            let badge = gtk4::Image::builder()
                .pixel_size(18)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Start)
                .margin_top(2)
                .margin_end(2)
                .build();
            badge.add_css_class("file-badge");
            thumbnail.add_overlay(&badge);
            // `WordChar` rather than the default `Word`: a name with no spaces
            // offers no word-break opportunity, so word wrapping cannot break it
            // at all and the label asks for its full natural width instead —
            // one tile stretches to the width of the window and the grid
            // collapses to a single column. Allowing a mid-word break is what
            // keeps the two-line-then-ellipsis budget below enforceable for
            // *every* name rather than only the ones that happen to have spaces.
            let label = gtk4::Label::builder()
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .justify(gtk4::Justification::Center)
                .max_width_chars(13)
                .width_chars(13)
                .wrap(true)
                .wrap_mode(gtk4::pango::WrapMode::WordChar)
                .lines(2)
                .build();
            ui.browser
                .grid_tiles
                .borrow_mut()
                .push((thumbnail.downgrade(), label.downgrade()));
            let tile = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            tile.add_css_class("file-tile");
            tile.append(&thumbnail);
            tile.append(&label);
            attach_context_menu(&ui, item, &tile);
            attach_drag(&ui, item, &tile);
            attach_drop(&ui, item, &tile);
            item.set_child(Some(&tile));
        }
    });
    factory.connect_bind({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let tile = item.child().and_downcast::<gtk4::Box>().unwrap();
            let thumbnail = tile.first_child().and_downcast::<gtk4::Overlay>().unwrap();
            let badge = thumbnail
                .last_child()
                .and_downcast::<gtk4::Image>()
                .unwrap();
            let label = thumbnail
                .next_sibling()
                .and_downcast::<gtk4::Label>()
                .unwrap();
            let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
            let entry = obj.borrow::<DirEntry>();
            let size = ui.browser.grid_thumbnail_size.get();
            resize_grid_tile(&thumbnail, &label, size);
            bind_file_thumbnail(&ui, &thumbnail, &entry, false);
            label.set_label(&entry.name);
            label.set_tooltip_text(Some(&entry.name));
            apply_badge(&badge, &entry);
        }
    });
    grid.set_factory(Some(&factory));

    let ui_grid = ui.clone();
    grid.connect_activate(move |grid, pos| {
        if let Some(entry) = entry_at(grid.model().as_ref(), pos) {
            activate_entry(&ui_grid, &entry);
        }
    });

    // Column list: Name (icon + label, right-clickable), Size, Modified.
    column_view.append_column(&name_column(ui));
    column_view.append_column(&text_column("Size", |e| {
        if e.is_dir {
            "—".to_string()
        } else {
            human_bytes(e.size)
        }
    }));
    column_view.append_column(&text_column("Modified", |e| format_modified(e.modified)));

    let ui_col = ui.clone();
    column_view.connect_activate(move |view, pos| {
        if let Some(entry) = entry_at(view.model().as_ref(), pos) {
            activate_entry(&ui_col, &entry);
        }
    });
}

fn resize_grid_tile(thumbnail: &gtk4::Overlay, label: &gtk4::Label, size: i32) {
    resize_file_thumbnail(thumbnail, size, grid_fallback_size(size));
    let name_width = (size / 6 + 1).clamp(8, 24);
    label.set_width_chars(name_width);
    label.set_max_width_chars(name_width);
}

fn grid_fallback_size(thumbnail_size: i32) -> i32 {
    (thumbnail_size * 8 / 9).clamp(24, thumbnail_size)
}

/// Build the Name column: a small thumbnail with its local-state badge overlaid,
/// followed by the name and the same right-click menu the grid tiles carry.
pub(crate) fn name_column(ui: &Rc<Ui>) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let thumbnail = file_thumbnail_widget(28, 16);
            let badge = gtk4::Image::builder()
                .pixel_size(14)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Start)
                .build();
            badge.add_css_class("file-badge");
            thumbnail.add_overlay(&badge);
            // Ellipsized so the Name column can be *narrower* than its longest
            // name. Without it the label's minimum width is the whole string,
            // the column inherits that minimum, and one long name pushes Size
            // and Modified off the right edge of the window for every row.
            let label = gtk4::Label::builder()
                .halign(gtk4::Align::Start)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .build();
            let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            cell.append(&thumbnail);
            cell.append(&label);
            attach_context_menu(&ui, item, &cell);
            attach_drag(&ui, item, &cell);
            attach_drop(&ui, item, &cell);
            item.set_child(Some(&cell));
        }
    });
    factory.connect_bind({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let cell = item.child().and_downcast::<gtk4::Box>().unwrap();
            let thumbnail = cell.first_child().and_downcast::<gtk4::Overlay>().unwrap();
            let badge = thumbnail
                .last_child()
                .and_downcast::<gtk4::Image>()
                .unwrap();
            let label = thumbnail
                .next_sibling()
                .and_downcast::<gtk4::Label>()
                .unwrap();
            let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
            let entry = obj.borrow::<DirEntry>();
            bind_file_thumbnail(&ui, &thumbnail, &entry, true);
            label.set_label(&entry.name);
            label.set_tooltip_text(Some(&entry.name));
            apply_badge(&badge, &entry);
        }
    });
    let column = gtk4::ColumnViewColumn::new(Some("Name"), Some(factory));
    column.set_expand(true);
    column
}

/// Build a trailing text column whose cell text is derived from each [`DirEntry`]
/// by `render`.
pub(crate) fn text_column(
    title: &str,
    render: impl Fn(&DirEntry) -> String + 'static,
) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
        label.add_css_class("dim-label");
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = item.child().and_downcast::<gtk4::Label>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        label.set_label(&render(&entry));
    });
    gtk4::ColumnViewColumn::new(Some(title), Some(factory))
}

/// Attach a secondary-button [`gtk4::GestureClick`] to a cell that pops a context
/// menu for whatever entry the owning `item` is currently bound to. Capturing the
/// [`gtk4::ListItem`] (rather than a snapshot of the entry) keeps the menu correct
/// as the view recycles cells while scrolling.
pub(crate) fn attach_context_menu(ui: &Rc<Ui>, item: &gtk4::ListItem, anchor: &gtk4::Box) {
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
    let ui = ui.clone();
    let item = item.clone();
    let target = anchor.clone();
    gesture.connect_pressed(move |_, _, x, y| {
        if let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() {
            let entry = obj.borrow::<DirEntry>().clone();
            // Right-clicking a row that is part of a multi-selection acts on the
            // batch; right-clicking outside one acts on the row, as before.
            let selected = selected_entries(&ui);
            if selected.len() > 1 && selected.iter().any(|e| e.uid == entry.uid) {
                show_bulk_context_menu(&ui, selected, &target, x, y);
            } else {
                show_context_menu(&ui, &entry, &target, x, y);
            }
        }
    });
    anchor.add_controller(gesture);
}

/// Pop a context menu next to `anchor` at the click point, offering Open and a
/// Pin/Unpin toggle (files only). Built fresh per click because the items are
/// entry-specific; it unparents itself once dismissed.
pub(crate) fn show_context_menu(ui: &Rc<Ui>, entry: &DirEntry, anchor: &gtk4::Box, x: f64, y: f64) {
    let menu = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let popover = gtk4::Popover::builder()
        .has_arrow(false)
        .position(gtk4::PositionType::Bottom)
        .pointing_to(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1))
        .child(&menu)
        .build();
    popover.set_parent(anchor);
    popover.connect_closed(|p| p.unparent());

    // Media leads with Play (stream from the mount, no download); the plain Open
    // below still downloads a local copy for anyone who wants one.
    if is_streamable_media_entry(entry) {
        let play = menu_item("Play (stream)", "media-playback-start-symbolic");
        let ui_play = ui.clone();
        let entry_play = entry.clone();
        let pop = popover.clone();
        play.connect_clicked(move |_| {
            pop.popdown();
            stream_entry(&ui_play, &entry_play);
        });
        menu.append(&play);
    }

    let open = menu_item("Open", "document-open-symbolic");
    let ui_open = ui.clone();
    let entry_open = entry.clone();
    let pop = popover.clone();
    open.connect_clicked(move |_| {
        pop.popdown();
        // Open always means "download a local copy and hand off", even for a
        // media file — `activate_entry` would otherwise stream it.
        if is_streamable_media_entry(&entry_open) {
            download_and_open(&ui_open, &entry_open);
        } else {
            activate_entry(&ui_open, &entry_open);
        }
    });
    menu.append(&open);

    if !entry.is_dir {
        let (label, icon) = if entry.pinned {
            ("Unpin", "non-starred-symbolic")
        } else {
            ("Keep offline", "starred-symbolic")
        };
        let pin = menu_item(label, icon);
        let ui_pin = ui.clone();
        let entry_pin = entry.clone();
        let pop = popover.clone();
        pin.connect_clicked(move |_| {
            pop.popdown();
            toggle_pin(&ui_pin, &entry_pin);
        });
        menu.append(&pin);
    }

    menu.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let rename = menu_item("Rename…", "document-edit-symbolic");
    let ui_rn = ui.clone();
    let entry_rn = entry.clone();
    let pop = popover.clone();
    rename.connect_clicked(move |_| {
        pop.popdown();
        prompt_rename(&ui_rn, &entry_rn);
    });
    menu.append(&rename);

    let move_it = menu_item("Move…", "folder-move-symbolic");
    let ui_mv = ui.clone();
    let entry_mv = entry.clone();
    let pop = popover.clone();
    move_it.connect_clicked(move |_| {
        pop.popdown();
        prompt_move(&ui_mv, &entry_mv);
    });
    menu.append(&move_it);

    menu.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let share = menu_item("Share…", "emblem-shared-symbolic");
    let ui_sh = ui.clone();
    let entry_sh = entry.clone();
    let pop = popover.clone();
    share.connect_clicked(move |_| {
        pop.popdown();
        open_share_dialog(&ui_sh, &entry_sh);
    });
    menu.append(&share);

    let trash = menu_item("Move to Trash", "user-trash-symbolic");
    let ui_tr = ui.clone();
    let entry_tr = entry.clone();
    let pop = popover.clone();
    trash.connect_clicked(move |_| {
        pop.popdown();
        prompt_delete(&ui_tr, &entry_tr);
    });
    menu.append(&trash);

    popover.popup();
}

/// The context menu for a multi-selection: the batch-capable actions only.
/// Rename, Move, Share and the revision history all need a single subject, so
/// they are simply absent here rather than offered and then refused.
pub(crate) fn show_bulk_context_menu(
    ui: &Rc<Ui>,
    entries: Vec<DirEntry>,
    anchor: &gtk4::Box,
    x: f64,
    y: f64,
) {
    let menu = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let popover = gtk4::Popover::builder()
        .has_arrow(false)
        .position(gtk4::PositionType::Bottom)
        .pointing_to(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1))
        .child(&menu)
        .build();
    popover.set_parent(anchor);
    popover.connect_closed(|p| p.unparent());

    let header = gtk4::Label::builder()
        .label(format!("{} selected", entries.len()))
        .halign(gtk4::Align::Start)
        .build();
    header.add_css_class("dim-label");
    header.add_css_class("caption");
    header.set_margin_top(6);
    header.set_margin_bottom(4);
    header.set_margin_start(10);
    menu.append(&header);

    let files_only = entries.iter().all(|e| !e.is_dir);
    if files_only && entries.iter().any(|e| !e.pinned) {
        let pin = menu_item("Keep offline", "starred-symbolic");
        let ui_pin = ui.clone();
        let batch = entries.clone();
        let pop = popover.clone();
        pin.connect_clicked(move |_| {
            pop.popdown();
            run_bulk_pin(&ui_pin, batch.clone(), true);
        });
        menu.append(&pin);
    }
    if files_only && entries.iter().any(|e| e.pinned) {
        let unpin = menu_item("Remove offline copy", "non-starred-symbolic");
        let ui_unpin = ui.clone();
        let batch = entries.clone();
        let pop = popover.clone();
        unpin.connect_clicked(move |_| {
            pop.popdown();
            run_bulk_pin(&ui_unpin, batch.clone(), false);
        });
        menu.append(&unpin);
    }

    menu.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let trash = menu_item("Move to Trash", "user-trash-symbolic");
    let ui_tr = ui.clone();
    let pop = popover.clone();
    trash.connect_clicked(move |_| {
        pop.popdown();
        prompt_delete_many(&ui_tr, entries.clone());
    });
    menu.append(&trash);

    popover.popup();
}

/// A left-aligned, flat icon+label button for the context menu.
pub(crate) fn menu_item(label: &str, icon: &str) -> gtk4::Button {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.append(&gtk4::Image::from_icon_name(icon));
    row.append(
        &gtk4::Label::builder()
            .label(label)
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .build(),
    );
    let button = gtk4::Button::builder().child(&row).build();
    button.add_css_class("flat");
    button
}

/// Fetch the [`DirEntry`] backing the model item at `pos`, if any.
pub(crate) fn entry_at(model: Option<&impl IsA<gio::ListModel>>, pos: u32) -> Option<DirEntry> {
    let obj = model?.item(pos).and_downcast::<BoxedAnyObject>()?;
    let entry = obj.borrow::<DirEntry>().clone();
    Some(entry)
}

/// Whether an entry can be opened through FUSE instead of fully materialized.
pub(crate) fn is_streamable_media_entry(entry: &DirEntry) -> bool {
    drive_activation(&entry.name, entry.is_dir) == DriveActivation::MountedMedia
}

/// Stream media straight from the mount, no download. Drive folders *are* part
/// of the FUSE mount, so a player pointed at `<mountpoint>/<rel>` reads the file
/// through [`Core::read_range_remote`] — 4 MB blocks fetched on demand as it
/// seeks and buffers — instead of waiting for the whole file to land like
/// [`Request::OpenFile`] does. This is the point of the feature: a 2 GB HEVC
/// `.mkv` starts playing in seconds.
pub(crate) fn stream_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let mountpoint = ui.dirs.resolved_mountpoint(&ui.dirs.load_config());
    let abs = mounted_path(&mountpoint, &rel);
    let Some(path) = abs.to_str() else {
        toast_error(
            ui,
            "Couldn't play media",
            "The file path isn't valid UTF-8.",
        );
        return;
    };
    toast(ui, &format!("Streaming “{}”…", entry.name));
    play_external(path);
}

/// Open an entry the Nautilus way: folders descend, media streams from the mount,
/// other files download-and-open.
pub(crate) fn activate_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    if entry.is_dir {
        // Descending into a search hit: clear the query so the folder listing
        // isn't immediately re-masked by a stale search.
        if !entry.path.is_empty() {
            ui.browser.search.set_text("");
        }
        *ui.browser.path.borrow_mut() = rel;
        load_browser(ui);
    } else if drive_activation(&entry.name, entry.is_dir) == DriveActivation::MountedMedia {
        // Media streams rather than downloads: that is exactly the "play it,
        // don't fetch the whole thing" behaviour this is for.
        stream_entry(ui, entry);
    } else {
        download_and_open(ui, entry);
    }
}

/// Hand a file to the user's default application: through the mount when it is
/// there, otherwise downloaded into the cache first. The open path behind both
/// a plain double-click and the context menu's "Open" (including for a video,
/// when the user wants their default application rather than the player).
///
/// The mount comes first because the cache blob is a *copy* keyed by content
/// hash — an application saving into it writes where Drive will never look, and
/// the cache may evict it afterwards. See
/// [`mounted_target`](crate::activation::mounted_target).
pub(crate) fn download_and_open(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let mountpoint = ui.dirs.resolved_mountpoint(&ui.dirs.load_config());
    if let Some(path) = mounted_target_rel(&mountpoint, &rel) {
        open_path(&path.to_string_lossy());
        return;
    }
    // Ignore a repeat activation of a file already downloading, so an impatient
    // double-click doesn't kick off a second round-trip.
    if !ui.opening.borrow_mut().insert(rel.clone()) {
        return;
    }
    ui.busy_begin();
    let name = entry.name.clone();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenFile {
            path: rel.clone(),
            uid: Some(entry.uid.clone()),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.opening.borrow_mut().remove(&rel);
        match result {
            // A cache blob is named by content hash: match the open rules
            // against the Drive name the user clicked, not that path.
            Ok(Ok(Response::FilePath { path })) => open_named_path(&path, &name),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't open file", &message, kind)
            }
            _ => toast_error(
                &ui,
                "Couldn't open file",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Pin or unpin an entry through the daemon, then reload to reflect the new
/// state.
pub(crate) fn toggle_pin(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let req = if entry.pinned {
        Request::Unpin { path: rel }
    } else {
        Request::Pin { path: rel }
    };
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let name = entry.name.clone();
    let pinned = entry.pinned;
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't change offline state", &message, kind);
                // The details switch may be showing the state we failed to reach.
                load_browser(&ui);
            }
            Ok(Ok(_)) => {
                load_browser(&ui);
                toast(
                    &ui,
                    &if pinned {
                        format!("“{name}” is no longer kept offline")
                    } else {
                        format!("“{name}” is now available offline")
                    },
                );
            }
            _ => {
                toast_error(
                    &ui,
                    "Couldn't change offline state",
                    "The mount service didn't respond.",
                );
                load_browser(&ui);
            }
        }
    });
}

/// Join the entry name onto the current browser directory to get its
/// mountpoint-relative path.
pub(crate) fn entry_rel(ui: &Rc<Ui>, entry: &DirEntry) -> String {
    // Search hits carry an absolute (mountpoint-relative) path since they can
    // live anywhere; plain listing entries derive it from the current folder.
    if !entry.path.is_empty() {
        return entry.path.clone();
    }
    let base = ui.browser.path.borrow();
    if base.is_empty() {
        entry.name.clone()
    } else {
        format!("{base}/{}", entry.name)
    }
}

/// Rebuild the clickable breadcrumb trail for the mountpoint-relative `path`. The
/// root is always present ("Proton Drive"); each segment becomes a flat button
/// that navigates to that ancestor, except the last (the current folder), shown
/// as a plain heading label.
pub(crate) fn repaint_crumb(ui: &Rc<Ui>, path: &str) {
    while let Some(child) = ui.browser.crumb.first_child() {
        ui.browser.crumb.remove(&child);
    }
    let segments: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    };
    ui.browser
        .crumb
        .append(&crumb_node(ui, "Proton Drive", "", segments.is_empty()));
    let mut acc = String::new();
    for (i, seg) in segments.iter().enumerate() {
        let sep = gtk4::Label::new(Some("›"));
        sep.add_css_class("dim-label");
        ui.browser.crumb.append(&sep);
        acc = if acc.is_empty() {
            seg.to_string()
        } else {
            format!("{acc}/{seg}")
        };
        let current = i == segments.len() - 1;
        ui.browser.crumb.append(&crumb_node(ui, seg, &acc, current));
    }
}

/// One breadcrumb segment: a plain heading label for the current folder, or a
/// flat button that navigates to `target` (clearing any active search first).
pub(crate) fn crumb_node(ui: &Rc<Ui>, label: &str, target: &str, current: bool) -> gtk4::Widget {
    if current {
        let l = gtk4::Label::builder()
            .label(label)
            .ellipsize(gtk4::pango::EllipsizeMode::Start)
            .build();
        l.add_css_class("heading");
        return l.upcast();
    }
    let button = gtk4::Button::builder().label(label).build();
    button.add_css_class("flat");
    let ui = ui.clone();
    let target = target.to_string();
    button.connect_clicked(move |_| {
        ui.browser.search.set_text("");
        *ui.browser.path.borrow_mut() = target.clone();
        load_browser(&ui);
    });
    button.upcast()
}

/// Wire the browser header's New-folder, Upload-files and Upload-folder buttons.
pub(crate) fn wire_browser_actions(
    ui: &Rc<Ui>,
    new_folder: &gtk4::Button,
    upload: &gtk4::Button,
    upload_folder: &gtk4::Button,
    build_thumbnails: &gtk4::Button,
) {
    let ui_nf = ui.clone();
    new_folder.connect_clicked(move |_| prompt_new_folder(&ui_nf));
    let ui_up = ui.clone();
    upload.connect_clicked(move |_| prompt_upload(&ui_up));
    let ui_uf = ui.clone();
    upload_folder.connect_clicked(move |_| prompt_upload_folder(&ui_uf));
    let ui_thumbs = ui.clone();
    build_thumbnails.connect_clicked(move |_| {
        if ui_thumbs.browser.thumbnail_build_running.get() {
            cancel_thumbnail_build(&ui_thumbs);
        } else {
            start_thumbnail_build(&ui_thumbs);
        }
    });
}

const THUMBNAIL_BUILD_POLL: Duration = Duration::from_millis(500);

/// Start the daemon's deliberate recursive job. Opportunistic requests for
/// visible rows are cancelled first; the explicit job has its own lifetime and
/// continues if the user navigates while watching its progress.
fn start_thumbnail_build(ui: &Rc<Ui>) {
    if !*ui.mounted.borrow() {
        toast_error(
            ui,
            "Couldn't build thumbnails",
            "Proton Drive isn't connected.",
        );
        return;
    }
    cancel_file_thumbnails(ui);
    ui.browser.thumbnail_build_running.set(true);
    // Do not expose Cancel until the daemon has acknowledged Start; the two
    // requests use separate control connections and could otherwise reorder.
    ui.browser.thumbnail_cancel_pending.set(true);
    repaint_thumbnail_build_action(ui, true);
    ui.browser.thumbnail_build_row.set_visible(true);
    ui.browser.thumbnail_progress.set_fraction(0.0);
    ui.browser
        .thumbnail_status
        .set_label("Starting thumbnail build…");

    let path = ui.browser.path.borrow().clone();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::StartThumbnailBuild { path },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::ThumbnailBuild { status })) => {
                ui.browser.thumbnail_cancel_pending.set(false);
                repaint_thumbnail_build(&ui, &status);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                thumbnail_build_failed(&ui);
                toast_failure(&ui, "Couldn't build thumbnails", &message, kind);
            }
            _ => {
                let message = "The mount service didn't respond.";
                thumbnail_build_failed(&ui);
                toast_error(&ui, "Couldn't build thumbnails", message);
            }
        }
    });
}

fn cancel_thumbnail_build(ui: &Rc<Ui>) {
    if ui.browser.thumbnail_cancel_pending.replace(true) {
        return;
    }
    ui.browser.build_thumbnails.set_sensitive(false);
    ui.browser
        .thumbnail_status
        .set_label("Stopping thumbnail build…");
    let rx = spawn_request(ui.dirs.control_socket(), Request::CancelThumbnailBuild);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::ThumbnailBuild { status })) if !status.running => {
                repaint_thumbnail_build(&ui, &status)
            }
            Ok(Ok(Response::ThumbnailBuild { .. })) => schedule_thumbnail_build_poll(&ui),
            Ok(Ok(Response::Error { message, kind })) => {
                thumbnail_cancel_failed(&ui);
                toast_failure(&ui, "Couldn't cancel thumbnail build", &message, kind);
            }
            _ => {
                thumbnail_cancel_failed(&ui);
                toast_error(
                    &ui,
                    "Couldn't cancel thumbnail build",
                    "The mount service didn't respond.",
                );
            }
        }
    });
}

fn thumbnail_cancel_failed(ui: &Rc<Ui>) {
    ui.browser.thumbnail_cancel_pending.set(false);
    ui.browser.thumbnail_build_running.set(true);
    repaint_thumbnail_build_action(ui, true);
    schedule_thumbnail_build_poll(ui);
}

fn schedule_thumbnail_build_poll(ui: &Rc<Ui>) {
    if let Some(source) = ui.browser.thumbnail_poll.borrow_mut().take() {
        source.remove();
    }
    let ui_poll = ui.clone();
    let source = glib::timeout_add_local_once(THUMBNAIL_BUILD_POLL, move || {
        ui_poll.browser.thumbnail_poll.borrow_mut().take();
        let rx = spawn_request(ui_poll.dirs.control_socket(), Request::ThumbnailBuildStatus);
        let ui_result = ui_poll.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(Response::ThumbnailBuild { status })) => {
                    repaint_thumbnail_build(&ui_result, &status)
                }
                Ok(Ok(Response::Error { message, .. })) => {
                    thumbnail_build_failed(&ui_result);
                    toast_error(&ui_result, "Thumbnail build stopped", &message);
                }
                _ => {
                    thumbnail_build_failed(&ui_result);
                    toast_error(
                        &ui_result,
                        "Thumbnail build stopped",
                        "The mount service stopped reporting thumbnail progress.",
                    );
                }
            }
        });
    });
    *ui.browser.thumbnail_poll.borrow_mut() = Some(source);
}

fn repaint_thumbnail_build(ui: &Rc<Ui>, status: &ThumbnailBuildStatus) {
    ui.browser
        .thumbnail_build_row
        .set_visible(show_thumbnail_build_progress(status));
    let root = if status.path.is_empty() {
        "Proton Drive"
    } else {
        &status.path
    };
    let text = if status.scanning {
        ui.browser.thumbnail_progress.pulse();
        format!(
            "Scanning {root}… {} folders, {} images",
            status.folders_scanned, status.images_found
        )
    } else if status.running {
        let fraction = if status.images_found == 0 {
            0.0
        } else {
            status.completed as f64 / status.images_found as f64
        };
        ui.browser
            .thumbnail_progress
            .set_fraction(fraction.clamp(0.0, 1.0));
        format!(
            "Building thumbnails in {root}… {} of {}",
            status.completed, status.images_found
        )
    } else {
        ui.browser.thumbnail_progress.set_fraction(1.0);
        let available = status.completed.saturating_sub(status.failed);
        match status.failed {
            0 => format!("Thumbnails ready for {available} images in {root}"),
            failed => {
                format!("Thumbnails ready for {available} images in {root}; {failed} unavailable")
            }
        }
    };
    let text = match status.message.as_deref() {
        Some(message) => format!("{text}. {message}"),
        None => text,
    };
    ui.browser.thumbnail_status.set_label(&text);
    ui.browser.thumbnail_status.set_tooltip_text(Some(&text));

    if status.running {
        ui.browser.thumbnail_build_running.set(true);
        repaint_thumbnail_build_action(ui, true);
        schedule_thumbnail_build_poll(ui);
    } else {
        ui.browser.thumbnail_build_running.set(false);
        ui.browser.thumbnail_cancel_pending.set(false);
        repaint_thumbnail_build_action(ui, false);
        if ui.stack.visible_child_name().as_deref() == Some("browser") {
            reload_listing(ui);
        }
    }
}

/// The progress row is transient: once the daemon reports completion, both its
/// label and bar leave the browser instead of retaining a stale success state.
fn show_thumbnail_build_progress(status: &ThumbnailBuildStatus) -> bool {
    status.running
}

fn repaint_thumbnail_build_action(ui: &Rc<Ui>, running: bool) {
    ui.browser.build_thumbnails.set_icon_name(if running {
        "process-stop-symbolic"
    } else {
        "pdfs-build-thumbnails-symbolic"
    });
    ui.browser
        .build_thumbnails
        .set_tooltip_text(Some(if running {
            "Cancel thumbnail build"
        } else {
            "Build thumbnails in this folder and its subfolders"
        }));
    ui.browser.build_thumbnails.set_sensitive(
        *ui.mounted.borrow() && (!running || !ui.browser.thumbnail_cancel_pending.get()),
    );
}

fn thumbnail_build_failed(ui: &Rc<Ui>) {
    ui.browser.thumbnail_build_running.set(false);
    ui.browser.thumbnail_cancel_pending.set(false);
    repaint_thumbnail_build_action(ui, false);
    ui.browser.thumbnail_build_row.set_visible(false);
    ui.browser.thumbnail_progress.set_fraction(0.0);
    ui.browser.thumbnail_status.set_label("");
    ui.browser.thumbnail_status.set_tooltip_text(None);
}

/// Send a mutating request (rename / move / delete / mkdir / upload, or a trash
/// restore / purge) on a worker thread, then reload the listing it changed and
/// confirm with a toast, or report the daemon's error in one. `done` is the
/// past-tense confirmation ("Renamed to “x”"); `failed` names the attempt
/// ("Couldn't rename").
pub(crate) fn run_mutation(ui: &Rc<Ui>, req: Request, done: String, failed: &'static str) {
    if !*ui.mounted.borrow() {
        toast_error(ui, failed, "Proton Drive isn't connected.");
        return;
    }
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => {
                // The listing the mutation changed is stale now; reload it, then
                // confirm, so the toast lands over the updated view.
                reload_listing(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, failed, &message, kind),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}

/// Reload the listing a completed mutation invalidated: whichever of the two
/// listing pages is on screen, since that is the one the action was raised from.
pub(crate) fn reload_listing(ui: &Rc<Ui>) {
    match ui.stack.visible_child_name().as_deref() {
        Some("browser") => load_browser(ui),
        Some("trash") => load_trash(ui),
        _ => {}
    }
}

/// Prompt for a new name and rename the entry through the daemon.
pub(crate) fn prompt_rename(ui: &Rc<Ui>, entry: &DirEntry) {
    let parent = ui_window(ui);
    let rel = entry_rel(ui, entry);
    let original = entry.name.clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Rename")
        .body(format!("Rename “{original}”."))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("New name")
        .activates_default(true)
        .build();
    row.set_text(&original);
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Rename");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let new_name = row.text().trim().to_string();
        if new_name.is_empty() || new_name == original {
            return;
        }
        let done = format!("Renamed to “{new_name}”");
        run_mutation(
            &ui,
            Request::Rename {
                path: rel.clone(),
                new_name,
            },
            done,
            "Couldn't rename",
        );
    });
    dialog.present(parent.as_ref());
}

/// Prompt for a destination folder (mountpoint-relative, empty = Drive root) and
/// move the entry there through the daemon.
pub(crate) fn prompt_move(ui: &Rc<Ui>, entry: &DirEntry) {
    let parent = ui_window(ui);
    let rel = entry_rel(ui, entry);
    let name = entry.name.clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Move")
        .body(format!(
            "Move “{}” into another folder. Enter its path from the Drive root \
             (leave blank for the root).",
            entry.name
        ))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("Destination folder")
        .activates_default(true)
        .build();
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Move");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let new_parent = row.text().trim().trim_matches('/').to_string();
        let done = match new_parent.as_str() {
            "" => format!("Moved “{name}” to Proton Drive"),
            dest => format!("Moved “{name}” to “{dest}”"),
        };
        run_mutation(
            &ui,
            Request::Move {
                path: rel.clone(),
                new_parent,
            },
            done,
            "Couldn't move",
        );
    });
    dialog.present(parent.as_ref());
}

/// Confirm and move the entry to Trash through the daemon.
pub(crate) fn prompt_delete(ui: &Rc<Ui>, entry: &DirEntry) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Move to Trash")
        .body(format!("Move “{}” to Trash?", entry.name))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("trash", "Move to Trash");
    dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    // Routed through the batch path so a single trash gets the same Undo: one
    // mis-click on one file is no less worth taking back than fifty.
    let entry = entry.clone();
    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "trash" {
            run_bulk_delete(&ui, vec![entry.clone()]);
        }
    });
    dialog.present(win.as_ref());
}

/// Prompt for a folder name and create it under the current browser directory.
pub(crate) fn prompt_new_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let parent = ui.browser.path.borrow().clone();
    let dialog = adw::AlertDialog::builder()
        .heading("New folder")
        .body("Create a folder in the current directory.")
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("Folder name")
        .activates_default(true)
        .build();
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Create");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let name = row.text().trim().to_string();
        if name.is_empty() {
            return;
        }
        let done = format!("Created “{name}”");
        run_mutation(
            &ui,
            Request::CreateFolder {
                parent: parent.clone(),
                name,
            },
            done,
            "Couldn't create folder",
        );
    });
    dialog.present(win.as_ref());
}

/// Pick one or more local files and upload them into the current browser
/// directory. The daemon streams them from disk itself, so nothing is read into
/// the GUI — even a large multi-file selection.
pub(crate) fn prompt_upload(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = gtk4::FileDialog::builder().title("Upload Files").build();
    let ui = ui.clone();
    dialog.open_multiple(win.as_ref(), gio::Cancellable::NONE, move |res| {
        let Ok(files) = res else { return };
        let sources: Vec<String> = files
            .into_iter()
            .filter_map(|f| f.ok())
            .filter_map(|obj| obj.downcast::<gio::File>().ok())
            .filter_map(|f| f.path())
            .filter_map(|p| p.to_str().map(str::to_string))
            .collect();
        start_upload(&ui, sources);
    });
}

/// Pick a local folder and upload it — with its whole subtree — into the current
/// browser directory. The daemon recreates the directory structure remotely.
pub(crate) fn prompt_upload_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = gtk4::FileDialog::builder().title("Upload Folder").build();
    let ui = ui.clone();
    dialog.select_folder(win.as_ref(), gio::Cancellable::NONE, move |res| {
        let Ok(folder) = res else { return };
        let Some(path) = folder.path().and_then(|p| p.to_str().map(str::to_string)) else {
            return;
        };
        start_upload(&ui, vec![path]);
    });
}

/// Hand a set of local source paths to the daemon for background bulk upload.
/// The daemon acks at once and does the work off-socket, so we confirm the
/// hand-off with a toast; the Activity group then shows live progress and the
/// listing refreshes itself when the transfers finish (see [`repaint_transfers`]).
pub(crate) fn start_upload(ui: &Rc<Ui>, sources: Vec<String>) {
    if sources.is_empty() {
        return;
    }
    if !*ui.mounted.borrow() {
        toast_error(ui, "Couldn't upload", "Proton Drive isn't connected.");
        return;
    }
    let parent = ui.browser.path.borrow().clone();
    let n = sources.len();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::UploadPaths { parent, sources },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => {
                let what = if n == 1 {
                    "Uploading…".to_string()
                } else {
                    format!("Uploading {n} items…")
                };
                toast(&ui, &what);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't upload", &message, kind)
            }
            _ => toast_error(&ui, "Couldn't upload", "The mount service didn't respond."),
        }
    });
}

/// The sync-state badge for an entry: `(icon, css-class)`, or `None` for folders
/// (which carry no per-file cache state). Pinned (kept offline) ranks above merely
/// cached (downloaded, evictable); everything else is online-only.
pub(crate) fn badge_for(entry: &DirEntry) -> Option<(&'static str, &'static str)> {
    if entry.is_dir {
        return None;
    }
    Some(if entry.pinned {
        ("starred-symbolic", "badge-pinned")
    } else if entry.cached {
        ("emblem-ok-symbolic", "badge-cached")
    } else {
        ("weather-overcast-symbolic", "badge-cloud")
    })
}

/// Paint `badge` to reflect the entry's sync state (see [`badge_for`]). Clears any
/// prior colour class first, since list factories recycle cells.
pub(crate) fn apply_badge(badge: &gtk4::Image, entry: &DirEntry) {
    for class in ["badge-pinned", "badge-cached", "badge-cloud"] {
        badge.remove_css_class(class);
    }
    match badge_for(entry) {
        Some((icon, class)) => {
            badge.set_icon_name(Some(icon));
            badge.add_css_class(class);
            badge.set_visible(true);
        }
        None => badge.set_visible(false),
    }
}

/// Make a browser cell draggable, carrying the bound entry's mountpoint-relative
/// path as the drag payload. Reads the entry live at drag time (via the captured
/// [`gtk4::ListItem`]) so a recycled cell drags whatever it currently shows.
pub(crate) fn attach_drag(ui: &Rc<Ui>, item: &gtk4::ListItem, anchor: &gtk4::Box) {
    let source = gtk4::DragSource::new();
    source.set_actions(gtk4::gdk::DragAction::MOVE);
    let ui = ui.clone();
    let item = item.clone();
    source.connect_prepare(move |_, _, _| {
        let obj = item.item().and_downcast::<BoxedAnyObject>()?;
        let rel = entry_rel(&ui, &obj.borrow::<DirEntry>());
        Some(gtk4::gdk::ContentProvider::for_value(&glib::Value::from(
            rel.as_str(),
        )))
    });
    anchor.add_controller(source);
}

/// Make a browser cell a drop target: dropping a dragged path onto a *folder* cell
/// moves the source into it through the daemon. Drops onto files, onto the item
/// itself, or that would move a folder into its own subtree are rejected.
pub(crate) fn attach_drop(ui: &Rc<Ui>, item: &gtk4::ListItem, anchor: &gtk4::Box) {
    let target = gtk4::DropTarget::new(glib::types::Type::STRING, gtk4::gdk::DragAction::MOVE);
    let ui = ui.clone();
    let item = item.clone();
    target.connect_drop(move |_, value, _, _| {
        let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() else {
            return false;
        };
        let dest = obj.borrow::<DirEntry>();
        if !dest.is_dir {
            return false;
        }
        let Ok(src) = value.get::<String>() else {
            return false;
        };
        let dest_path = entry_rel(&ui, &dest);
        // No-op onto self, and never move a folder into itself or a descendant.
        if src == dest_path || dest_path.starts_with(&format!("{src}/")) {
            return false;
        }
        let done = format!("Moved into “{}”", dest.name);
        run_mutation(
            &ui,
            Request::Move {
                path: src,
                new_parent: dest_path,
            },
            done,
            "Couldn't move",
        );
        true
    });
    anchor.add_controller(target);
}

/// Pick a freedesktop icon base name for an entry from its kind / extension.
/// Callers append `-symbolic` for the column view's small icons.
pub(crate) fn icon_base_for(entry: &DirEntry) -> &'static str {
    if entry.is_dir {
        return "folder";
    }
    let ext = entry
        .name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "heic" | "tiff" => {
            "image-x-generic"
        }
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => "video-x-generic",
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" => "audio-x-generic",
        "pdf" | "doc" | "docx" | "odt" => "x-office-document",
        "xls" | "xlsx" | "ods" | "csv" => "x-office-spreadsheet",
        "ppt" | "pptx" | "odp" => "x-office-presentation",
        "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" => "package-x-generic",
        _ => "text-x-generic",
    }
}

/// Format an epoch-seconds modification time as a short local date.
pub(crate) fn format_modified(secs: i64) -> String {
    match glib::DateTime::from_unix_local(secs) {
        Ok(dt) => dt
            .format("%-d %b %Y")
            .map(|s| s.to_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// Request the current browser directory from the daemon and repaint both views.
pub(crate) fn load_browser(ui: &Rc<Ui>) {
    cancel_file_thumbnails(ui);
    let generation = ui.browser.load_generation.get().wrapping_add(1);
    ui.browser.load_generation.set(generation);
    let path = ui.browser.path.borrow().clone();
    repaint_crumb(ui, &path);
    ui.browser.back.set_sensitive(!path.is_empty());
    ui.browser.summary.set_label("Loading…");

    // Drop the previous folder's rows up front: a slow reply must not leave stale
    // entries visible, where clicking one would open with a wrong relative path.
    ui.browser.model.remove_all();
    browser_status(
        ui,
        "folder-symbolic",
        "Loading…",
        "Reading this folder.",
        false,
    );

    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListDir { path: path.clone() },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        // The user may have navigated on while this folder was loading. A stale
        // out-of-order reply must not repaint rows for a folder we've left, or
        // the breadcrumb and the grid would disagree.
        if ui.browser.load_generation.get() != generation || *ui.browser.path.borrow() != path {
            return;
        }
        match result {
            Ok(Ok(Response::Entries { entries })) => repaint_browser(&ui, &entries),
            Ok(Ok(Response::Error { message, kind })) => browser_failed(&ui, &message, kind),
            Ok(Ok(_)) => browser_failed(
                &ui,
                "Unexpected reply from the mount service.",
                ErrorKind::Internal,
            ),
            Ok(Err(_)) | Err(_) => browser_unreachable(&ui),
        }
    });
}

/// Clear the model and show the daemon's error on the status page. Used for
/// in-band failures (a bad path, a permission error) — the mount is up, so Retry
/// (which restarts the service) wouldn't help and isn't offered.
pub(crate) fn browser_failed(ui: &Rc<Ui>, message: &str, kind: ErrorKind) {
    ui.browser.model.remove_all();
    ui.browser.summary.set_label("Folder unavailable");
    browser_status(
        ui,
        "dialog-warning-symbolic",
        error_headline(kind, "Couldn't open this folder"),
        message,
        // Offer Retry only where repeating the request could actually work.
        // A folder that is gone stays gone however many times it is asked for.
        kind.retryable(),
    );
}

/// The daemon didn't answer. Distinguish *still starting* (auto-retry, no
/// button) from *down* (actionable error + Retry), so a cold start self-heals
/// once the systemd mount comes up but a real failure stays visible.
pub(crate) fn browser_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        ui.browser.summary.set_label("Not connected");
        browser_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    ui.browser.summary.set_label("Connecting…");
    browser_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        // Only keep polling while the Files page is the one on screen.
        if ui.stack.visible_child_name().as_deref() == Some("browser") {
            load_browser(&ui);
        }
    });
}

/// Repopulate the shared model — folders first, then case-insensitive by name —
/// which refreshes both the grid and the column list.
pub(crate) fn repaint_browser(ui: &Rc<Ui>, entries: &[DirEntry]) {
    ui.browser.model.remove_all();
    ui.browser.summary.set_label(&listing_summary(
        entries.iter().filter(|entry| !entry.is_dir).count(),
        entries.iter().filter(|entry| entry.is_dir).count(),
        entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum(),
    ));
    if entries.is_empty() {
        browser_status(
            ui,
            "folder-open-symbolic",
            "This folder is empty",
            "Upload a file or create a folder to get started.",
            false,
        );
        ui.browser.empty_actions.set_visible(true);
        return;
    }
    browser_views(ui);

    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for entry in sorted {
        ui.browser.model.append(&BoxedAnyObject::new(entry));
    }
}

/// Wire the browser header's search box: debounce keystrokes, then either run a
/// search or — when cleared — restore the current directory listing.
pub(crate) fn wire_search(ui: &Rc<Ui>) {
    let ui_s = ui.clone();
    ui.browser.search.connect_search_changed(move |_| {
        // Replace any pending debounce so only the last keystroke's pause fires.
        if let Some(src) = ui_s.browser.search_source.borrow_mut().take() {
            src.remove();
        }
        let ui_t = ui_s.clone();
        let src = glib::timeout_add_local_once(SEARCH_DEBOUNCE, move || {
            ui_t.browser.search_source.borrow_mut().take();
            let query = ui_t.browser.search.text().trim().to_string();
            if query.is_empty() {
                load_browser(&ui_t);
            } else {
                run_search(&ui_t, &query);
            }
        });
        *ui_s.browser.search_source.borrow_mut() = Some(src);
    });
}

/// Send a [`Request::Search`] to the daemon and render the hits in the browser
/// views, reusing the same row model so click-to-open and pin work unchanged
/// (each hit carries its full path; see [`entry_rel`]).
pub(crate) fn run_search(ui: &Rc<Ui>, query: &str) {
    cancel_file_thumbnails(ui);
    let generation = ui.browser.load_generation.get().wrapping_add(1);
    ui.browser.load_generation.set(generation);
    ui.browser.model.remove_all();
    ui.browser.summary.set_label("Searching…");
    browser_status(
        ui,
        "system-search-symbolic",
        "Searching…",
        &format!("Looking for “{query}”."),
        false,
    );

    ui.busy_begin();
    let query = query.to_string();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::Search {
            query: query.clone(),
            limit: SEARCH_LIMIT,
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        // The box may have been cleared or typed past while the reply was in
        // flight; if the query no longer matches, a fresher load/search already
        // owns the model — drop this stale, possibly out-of-order result.
        if ui.browser.load_generation.get() != generation
            || ui.browser.search.text().trim() != query
        {
            return;
        }
        match result {
            Ok(Ok(Response::SearchResults { hits })) => repaint_search(&ui, &hits),
            Ok(Ok(Response::Error { message, kind })) => browser_failed(&ui, &message, kind),
            Ok(Ok(_)) => browser_failed(
                &ui,
                "Unexpected reply from the mount service.",
                ErrorKind::Internal,
            ),
            Ok(Err(_)) | Err(_) => browser_unreachable(&ui),
        }
    });
}

/// Repopulate the model with search hits — folders first, then by name — mapping
/// each [`SearchHit`] to a path-carrying [`DirEntry`] the existing renderers and
/// handlers already understand.
pub(crate) fn repaint_search(ui: &Rc<Ui>, hits: &[SearchHit]) {
    ui.browser.model.remove_all();
    let counts = listing_summary(
        hits.iter().filter(|hit| !hit.is_dir).count(),
        hits.iter().filter(|hit| hit.is_dir).count(),
        hits.iter()
            .filter(|hit| !hit.is_dir)
            .map(|hit| hit.size)
            .sum(),
    );
    ui.browser
        .summary
        .set_label(&format!("{counts} — search results"));
    if hits.is_empty() {
        browser_status(
            ui,
            "system-search-symbolic",
            "No matches",
            "No files or folders match that search.",
            false,
        );
        return;
    }
    browser_views(ui);

    let mut entries: Vec<DirEntry> = hits
        .iter()
        .map(|h| DirEntry {
            name: h.name.clone(),
            is_dir: h.is_dir,
            size: h.size,
            modified: h.modified,
            pinned: h.pinned,
            // Search hits don't carry cache state; the badge shows in listings.
            cached: false,
            uid: h.uid.clone(),
            path: h.path.clone(),
            role: String::new(),
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for entry in entries {
        ui.browser.model.append(&BoxedAnyObject::new(entry));
    }
}

fn listing_summary(files: usize, folders: usize, file_bytes: u64) -> String {
    let file_word = if files == 1 { "file" } else { "files" };
    let folder_word = if folders == 1 { "folder" } else { "folders" };
    match (folders, files) {
        (0, 0) => "0 folders, 0 files".to_string(),
        (_, 0) => format!("{folders} {folder_word}"),
        (0, _) => format!("{files} {file_word} ({})", human_bytes(file_bytes)),
        _ => format!(
            "{folders} {folder_word}, {files} {file_word} ({})",
            human_bytes(file_bytes)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{listing_summary, show_thumbnail_build_progress};
    use pdfs_core::control::ThumbnailBuildStatus;

    #[test]
    fn listing_summary_matches_dolphin_order_and_wording() {
        assert_eq!(listing_summary(0, 0, 0), "0 folders, 0 files");
        assert_eq!(listing_summary(0, 2, 0), "2 folders");
        assert_eq!(listing_summary(1, 0, 1024), "1 file (1.0 KiB)");
        assert_eq!(listing_summary(3, 1, 3072), "1 folder, 3 files (3.0 KiB)");
    }

    #[test]
    fn thumbnail_progress_disappears_when_build_finishes() {
        let mut status = ThumbnailBuildStatus {
            running: true,
            scanning: true,
            ..Default::default()
        };
        assert!(show_thumbnail_build_progress(&status));

        status.scanning = false;
        assert!(show_thumbnail_build_progress(&status));

        status.running = false;
        assert!(!show_thumbnail_build_progress(&status));
    }
}
