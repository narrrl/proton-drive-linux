use crate::*;

pub(crate) struct DetailsState {
    /// Widgets in the details pane, repainted from the selected entry.
    pub(crate) details: DetailsWidgets,
    /// The entry the details pane is currently showing, so its buttons act on the
    /// same one the user is looking at even after the model is repopulated.
    pub(crate) details_entry: RefCell<Option<DirEntry>>,
    /// Set while the details pane is being populated, so setting the offline
    /// switch programmatically doesn't fire a pin/unpin round-trip.
    pub(crate) details_suppress: Cell<bool>,
    /// The grid's and list's selection models. Both wrap the one browser model, so
    /// a selection in either drives the details pane.
    pub(crate) grid_selection: gtk4::SingleSelection,
    pub(crate) list_selection: gtk4::SingleSelection,
}

/// The widgets in the browser's details pane that a selection repaints.
pub(crate) struct DetailsWidgets {
    pub(crate) icon: gtk4::Image,
    pub(crate) name: gtk4::Label,
    pub(crate) kind: gtk4::Label,
    pub(crate) size_row: adw::ActionRow,
    pub(crate) modified_row: adw::ActionRow,
    pub(crate) path_row: adw::ActionRow,
    pub(crate) pin_row: adw::SwitchRow,
    pub(crate) open_button: gtk4::Button,
    pub(crate) rename_button: gtk4::Button,
    pub(crate) trash_button: gtk4::Button,
    pub(crate) close_button: gtk4::Button,
}

/// The details pane shown beside the file views: a big type icon over the entry's
/// name, its properties, an offline (pin) toggle and the primary actions. Built
/// empty; [`repaint_details`] fills it from the selected [`DirEntry`] and
/// [`wire_details`] connects the buttons.
pub(crate) fn build_details_pane() -> (gtk4::Widget, DetailsWidgets) {
    let close_button = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Close details")
        .halign(gtk4::Align::End)
        .build();
    close_button.add_css_class("flat");
    close_button.add_css_class("circular");

    let icon = gtk4::Image::builder()
        .icon_name("text-x-generic-symbolic")
        .pixel_size(64)
        .build();
    let name = gtk4::Label::builder()
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .justify(gtk4::Justification::Center)
        .build();
    name.add_css_class("title-4");
    let kind = gtk4::Label::new(None);
    kind.add_css_class("dim-label");
    kind.add_css_class("caption");

    let head = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    head.set_margin_bottom(6);
    head.append(&icon);
    head.append(&name);
    head.append(&kind);

    let props = adw::PreferencesGroup::new();
    let size_row = adw::ActionRow::builder()
        .title("Size")
        .subtitle("—")
        .build();
    size_row.add_css_class("property");
    let modified_row = adw::ActionRow::builder()
        .title("Modified")
        .subtitle("—")
        .build();
    modified_row.add_css_class("property");
    let path_row = adw::ActionRow::builder()
        .title("Location")
        .subtitle("—")
        .subtitle_lines(3)
        .build();
    path_row.add_css_class("property");
    props.add(&size_row);
    props.add(&modified_row);
    props.add(&path_row);

    let offline = adw::PreferencesGroup::new();
    let pin_row = adw::SwitchRow::builder()
        .title("Available offline")
        .subtitle("Keep a local copy on this device.")
        .build();
    offline.add(&pin_row);

    let open_button = gtk4::Button::builder().label("Open").build();
    open_button.add_css_class("suggested-action");
    open_button.add_css_class("pill");
    let rename_button = gtk4::Button::builder().label("Rename").build();
    rename_button.add_css_class("pill");
    let trash_button = gtk4::Button::builder().label("Move to Trash").build();
    trash_button.add_css_class("destructive-action");
    trash_button.add_css_class("pill");

    let actions = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    actions.set_margin_top(6);
    actions.append(&open_button);
    actions.append(&rename_button);
    actions.append(&trash_button);

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&close_button);
    inner.append(&head);
    inner.append(&props);
    inner.append(&offline);
    inner.append(&actions);

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&inner)
        .build();
    let pane = adw::ToolbarView::new();
    pane.set_content(Some(&scroll));
    pane.add_css_class("background");

    (
        pane.upcast(),
        DetailsWidgets {
            icon,
            name,
            kind,
            size_row,
            modified_row,
            path_row,
            pin_row,
            open_button,
            rename_button,
            trash_button,
            close_button,
        },
    )
}

/// Connect the details pane: mirror the grid's and list's selection into it, and
/// wire its buttons back onto the entry it's showing.
pub(crate) fn wire_details(ui: &Rc<Ui>) {
    // Both views share one model but have their own selection, so watch both.
    for selection in [
        ui.details.grid_selection.clone(),
        ui.details.list_selection.clone(),
    ] {
        let ui_sel = ui.clone();
        selection.connect_selection_changed(move |sel, _, _| {
            match entry_at(sel.model().as_ref(), sel.selected()) {
                Some(entry) => show_details(&ui_sel, &entry),
                None => hide_details(&ui_sel),
            }
        });
    }

    let ui_close = ui.clone();
    ui.details.details.close_button.connect_clicked(move |_| {
        ui_close
            .details
            .grid_selection
            .set_selected(gtk4::INVALID_LIST_POSITION);
        ui_close
            .details
            .list_selection
            .set_selected(gtk4::INVALID_LIST_POSITION);
        hide_details(&ui_close);
    });

    let ui_open = ui.clone();
    ui.details.details.open_button.connect_clicked(move |_| {
        // Bind the clone to a local so the `Ref` from `borrow()` is dropped before
        // `activate_entry` runs; it navigates and repaints the details pane, which
        // takes `details_entry.borrow_mut()` and would panic against a live borrow.
        let entry = ui_open.details.details_entry.borrow().clone();
        if let Some(entry) = entry {
            activate_entry(&ui_open, &entry);
        }
    });
    let ui_rename = ui.clone();
    ui.details.details.rename_button.connect_clicked(move |_| {
        let entry = ui_rename.details.details_entry.borrow().clone();
        if let Some(entry) = entry {
            prompt_rename(&ui_rename, &entry);
        }
    });
    let ui_trash = ui.clone();
    ui.details.details.trash_button.connect_clicked(move |_| {
        let entry = ui_trash.details.details_entry.borrow().clone();
        if let Some(entry) = entry {
            prompt_delete(&ui_trash, &entry);
        }
    });
    let ui_pin = ui.clone();
    ui.details
        .details
        .pin_row
        .connect_active_notify(move |row| {
            if ui_pin.details.details_suppress.get() {
                return;
            }
            let Some(entry) = ui_pin.details.details_entry.borrow().clone() else {
                return;
            };
            // The switch reads the *desired* state; `toggle_pin` derives the request
            // from the entry's current one, so only act when they actually differ.
            if row.is_active() != entry.pinned {
                toggle_pin(&ui_pin, &entry);
            }
        });
}

/// Reveal the details pane and paint it from `entry`.
pub(crate) fn show_details(ui: &Rc<Ui>, entry: &DirEntry) {
    ui.details.details_suppress.set(true);
    let d = &ui.details.details;
    d.icon.set_icon_name(Some(icon_base_for(entry)));
    d.name.set_label(&entry.name);
    d.kind
        .set_label(if entry.is_dir { "Folder" } else { "File" });
    d.size_row.set_subtitle(&if entry.is_dir {
        "—".to_string()
    } else {
        human_bytes(entry.size)
    });
    d.size_row.set_visible(!entry.is_dir);
    d.modified_row
        .set_subtitle(&format_modified(entry.modified));
    let rel = entry_rel(ui, entry);
    let parent = match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    };
    d.path_row.set_subtitle(if parent.is_empty() {
        "Proton Drive"
    } else {
        parent
    });
    d.pin_row.set_active(entry.pinned);
    d.pin_row.set_sensitive(*ui.mounted.borrow());
    d.open_button
        .set_label(if entry.is_dir { "Open folder" } else { "Open" });
    ui.details.details_suppress.set(false);

    *ui.details.details_entry.borrow_mut() = Some(entry.clone());
    // Reveal the pane on idle, not inline. This runs from `selection_changed`,
    // which fires on the *first* press of a double-click; mutating the widget
    // tree here cancels GtkGridView's multi-press tracking, so the second press
    // restarts the count and `activate` never fires — the folder needs two
    // double-clicks. Deferring lets the click gesture finish first. Guard on the
    // entry still being present so a navigation that clears it wins the race.
    let ui = ui.clone();
    glib::idle_add_local_once(move || {
        if ui.details.details_entry.borrow().is_some() {
            ui.browser.split.set_show_sidebar(true);
        }
    });
}

/// Hide the details pane and forget the entry it was showing, so a stale entry
/// can't be acted on after the listing moves on.
pub(crate) fn hide_details(ui: &Rc<Ui>) {
    ui.browser.split.set_show_sidebar(false);
    *ui.details.details_entry.borrow_mut() = None;
}

/// The entry highlighted in whichever browser view is on screen, if any. Backs the
/// F2 / Delete shortcuts.
pub(crate) fn selected_entry(ui: &Rc<Ui>) -> Option<DirEntry> {
    ui.details.details_entry.borrow().clone()
}
