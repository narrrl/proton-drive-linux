use crate::*;

pub(crate) struct TrashState {
    // Trash page. Trashed nodes are addressed by uid, not by path, so this page
    // keeps no current-directory state — it re-lists from the daemon on show.
    pub(crate) files: FileList,
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    /// Empties the trash; hidden while it is empty (or unread).
    pub(crate) empty: gtk4::Button,
    /// "12 items" under the page title.
    pub(crate) subtitle: adw::WindowTitle,
    /// The bottom bar acting on the selection; revealed while anything is selected.
    pub(crate) selection_bar: gtk4::Revealer,
    pub(crate) selection_label: gtk4::Label,
    /// Runs the loads; the rows stay up while one runs.
    pub(crate) loader: Rc<Loader>,
    /// Back, the breadcrumb and the search entry over the list.
    pub(crate) back: gtk4::Button,
    pub(crate) crumb: gtk4::Box,
    pub(crate) search: gtk4::SearchEntry,
    /// On: the trash as the folders things were deleted from. Off: one list.
    pub(crate) grouped: gtk4::ToggleButton,
    /// The whole trash as last read, newest deletion first.
    pub(crate) all: RefCell<Vec<DirEntry>>,
    /// Where the list is, below the top level.
    pub(crate) nav: RefCell<Vec<TrashNav>>,
    /// What the trashed folder on screen holds, once read.
    pub(crate) folder_rows: RefCell<Option<Vec<DirEntry>>>,
}

/// One step down from the top of the Trash page.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TrashNav {
    /// The things deleted from one folder: its path, `""` for My files, `None`
    /// when the folder is not known.
    Location { key: Option<String>, label: String },
    /// Inside a trashed folder.
    Folder { uid: String, name: String },
}

/// The widgets of the Trash page that a load repaints.
pub(crate) struct TrashWidgets {
    pub(crate) files: FileList,
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) empty: gtk4::Button,
    pub(crate) subtitle: adw::WindowTitle,
    pub(crate) selection_bar: gtk4::Revealer,
    pub(crate) selection_label: gtk4::Label,
    pub(crate) restore_selected: gtk4::Button,
    pub(crate) delete_selected: gtk4::Button,
    pub(crate) back: gtk4::Button,
    pub(crate) crumb: gtk4::Box,
    pub(crate) search: gtk4::SearchEntry,
    pub(crate) grouped: gtk4::ToggleButton,
}

/// The uid prefix of a row that stands for a folder things were deleted from.
/// Such a row is made here, never sent to the daemon.
const LOCATION_UID: &str = "trash-location:";
/// The location key of the folder that is not known.
const UNKNOWN_LOCATION: &str = "\u{0}";

/// The Trash page: everything Drive is holding in the trash, as a list with
/// the folder each item came from and when it was deleted, or as a grid, where
/// photos are easier to recognise. Restore and Delete Permanently sit in the
/// right-click menu and, for a selection, in a bottom bar; Empty Trash is in
/// the header while there is something to empty.
///
/// A trashed node has no path inside the mount — the daemon forgets it when it
/// is trashed — so unlike the Files page this one addresses entries by uid. The
/// list's factories need the [`Ui`] handle, so they are installed in
/// [`wire_trash`].
pub(crate) fn build_trash_page() -> (gtk4::Widget, TrashWidgets) {
    let empty = gtk4::Button::builder()
        .label(gettext("Empty Trash…"))
        .tooltip_text(gettext("Permanently delete everything in the Trash"))
        .visible(false)
        .build();

    let retry = gtk4::Button::builder()
        .label(gettext("Retry"))
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("user-trash-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    // The list first: where an item came from and when it went are what tell
    // two copies of a file apart here.
    let files = FileList::new();
    files.show_list(true);
    let layout = layout_button(&files);

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&files.views, Some("list"));
    content.add_named(&status, Some("status"));

    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text(gettext("Back"))
        .visible(false)
        .build();
    back.add_css_class("flat");
    let crumb = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    crumb.set_valign(gtk4::Align::Center);
    let crumb_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::External)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .hexpand(true)
        .child(&crumb)
        .build();
    let search = gtk4::SearchEntry::builder()
        .placeholder_text(gettext("Search the trash"))
        .width_chars(24)
        .build();
    let path_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    path_bar.append(&back);
    path_bar.append(&crumb_scroll);
    path_bar.append(&search);

    let grouped = gtk4::ToggleButton::builder()
        .icon_name("folder-symbolic")
        .tooltip_text(gettext("Group by original folder"))
        .active(true)
        .build();

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&path_bar);
    inner.append(&content);

    // Bulk Delete Permanently always confirms with the count, so a stale
    // selection can't delete anything silently.
    let selection_label = gtk4::Label::new(None);
    let restore_selected = gtk4::Button::with_label(&pgettext("verb", "Restore"));
    let delete_selected = gtk4::Button::with_label(&gettext("Delete Permanently…"));
    delete_selected.add_css_class("destructive-action");
    let bar = gtk4::ActionBar::new();
    bar.set_center_widget(Some(&selection_label));
    bar.pack_start(&restore_selected);
    bar.pack_end(&delete_selected);
    let selection_bar = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideUp)
        .child(&bar)
        .build();
    inner.append(&selection_bar);

    let (frame, header, subtitle) = page_frame(&gettext("Trash"), &inner);
    header.pack_start(&empty);
    header.pack_end(&layout);
    header.pack_end(&grouped);

    (
        frame.upcast(),
        TrashWidgets {
            files,
            content,
            status,
            retry,
            empty,
            subtitle,
            selection_bar,
            selection_label,
            restore_selected,
            delete_selected,
            back,
            crumb,
            search,
            grouped,
        },
    )
}

/// Install the list's factories and columns, the Empty Trash button and the
/// selection bar.
pub(crate) fn wire_trash(ui: &Rc<Ui>, widgets: &TrashWidgets) {
    let files = &ui.trash.files;
    files.wire(
        ui,
        FileListBehavior {
            activate: activate_trashed,
            entry_menu: trash_entry_menu,
            bulk_menu: trash_bulk_menu,
            background_menu: trash_background_menu,
            badges: false,
            drag_and_drop: false,
        },
    );
    files
        .column_view
        .append_column(&text_column(&gettext("Original Location"), |e| {
            trashed_from_label(e)
        }));
    files
        .column_view
        .append_column(&text_column(&pgettext("column", "Deleted"), |e| {
            if e.trashed_at > 0 {
                dates::relative(e.trashed_at)
            } else {
                "—".to_string()
            }
        }));
    files
        .column_view
        .append_column(&text_column(&pgettext("column", "Size"), |e| {
            if location_key(&e.uid).is_some() {
                // A location row carries how many items it holds as its size.
                ngettext_f("{n} item", "{n} items", e.size, &[])
            } else if e.is_dir {
                "—".to_string()
            } else {
                human_bytes(e.size)
            }
        }));

    let ui_empty = ui.clone();
    widgets
        .empty
        .connect_clicked(move |_| prompt_empty_trash(&ui_empty));

    let ui_back = ui.clone();
    widgets.back.connect_clicked(move |_| {
        ui_back.trash.nav.borrow_mut().pop();
        show_trash_place(&ui_back);
    });
    let ui_search = ui.clone();
    widgets
        .search
        .connect_search_changed(move |_| render_trash(&ui_search));
    let ui_grouped = ui.clone();
    widgets.grouped.connect_toggled(move |_| {
        ui_grouped.trash.nav.borrow_mut().clear();
        show_trash_place(&ui_grouped);
    });

    let ui_sel = ui.clone();
    files
        .selection
        .connect_selection_changed(move |_, _, _| sync_trash_selection(&ui_sel));
    let ui_restore = ui.clone();
    widgets.restore_selected.connect_clicked(move |_| {
        let entries = ui_restore.trash.files.selected();
        if !entries.is_empty() {
            restore_entries(&ui_restore, &entries);
        }
    });
    let ui_delete = ui.clone();
    widgets.delete_selected.connect_clicked(move |_| {
        let entries = ui_delete.trash.files.selected();
        if !entries.is_empty() {
            prompt_delete_forever(&ui_delete, &entries);
        }
    });
}

/// Where a trashed entry goes back to on restore, for the Original Location
/// column.
fn trashed_from_label(entry: &DirEntry) -> String {
    match entry.trashed_from.as_deref() {
        None => "—".to_string(),
        Some("") => gettext("My files"),
        Some(path) => path.to_string(),
    }
}

/// Open a location or a trashed folder in place. A trashed file can't be
/// opened; say what would let it be.
fn activate_trashed(ui: &Rc<Ui>, entry: &DirEntry) {
    if let Some(key) = location_key(&entry.uid) {
        ui.trash.nav.borrow_mut().push(TrashNav::Location {
            key,
            label: entry.name.clone(),
        });
        show_trash_place(ui);
    } else if entry.is_dir {
        ui.trash.nav.borrow_mut().push(TrashNav::Folder {
            uid: entry.uid.clone(),
            name: entry.name.clone(),
        });
        show_trash_place(ui);
    } else {
        let name = match outer_folder(ui) {
            Some(folder) if !in_trash(ui, &entry.uid) => folder.name,
            _ => entry.name.clone(),
        };
        // Translators: {name} is a file or folder name.
        toast(
            ui,
            &gettext_f("Restore “{name}” to open it", &[("name", &name)]),
        );
    }
}

fn trash_entry_menu(ui: &Rc<Ui>, entry: &DirEntry) -> ActionMenu {
    let mut menu = ActionMenu::new();
    if location_key(&entry.uid).is_none()
        && !in_trash(ui, &entry.uid)
        && let Some(folder) = outer_folder(ui)
    {
        // Went to the trash with the folder, so only the folder goes back.
        let ui_c = ui.clone();
        let label = gettext_f(
            // Translators: {name} is a folder name.
            "Restore “{name}”",
            &[("name", &folder.name)],
        );
        menu.item(&label, move || {
            restore_entries(&ui_c, std::slice::from_ref(&folder))
        });
        return menu;
    }
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    menu.item(&pgettext("verb", "Restore"), move || {
        restore_entries(&ui_c, std::slice::from_ref(&entry_c))
    });
    menu.section();
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    menu.item(&gettext("Delete Permanently…"), move || {
        prompt_delete_forever(&ui_c, std::slice::from_ref(&entry_c))
    });
    menu
}

fn trash_bulk_menu(ui: &Rc<Ui>, entries: Vec<DirEntry>) -> ActionMenu {
    let mut menu = ActionMenu::new();
    menu.labelled_section(&ngettext_f(
        "{n} selected",
        "{n} selected",
        entries.len() as u64,
        &[],
    ));
    let (ui_c, batch) = (ui.clone(), entries.clone());
    menu.item(&pgettext("verb", "Restore"), move || {
        restore_entries(&ui_c, &batch)
    });
    menu.section();
    let ui_c = ui.clone();
    menu.item(&gettext("Delete Permanently…"), move || {
        prompt_delete_forever(&ui_c, &entries)
    });
    menu
}

fn trash_background_menu(ui: &Rc<Ui>) -> ActionMenu {
    let mut menu = ActionMenu::new();
    let ui_c = ui.clone();
    menu.item(&gettext("Select All"), move || {
        ui_c.trash.files.selection.select_all();
    });
    let ui_c = ui.clone();
    menu.item(&gettext("Refresh"), move || reload_current_page(&ui_c));
    if !ui.trash.all.borrow().is_empty() {
        menu.section();
        let ui_c = ui.clone();
        menu.item(&gettext("Empty Trash…"), move || {
            prompt_empty_trash(&ui_c)
        });
    }
    menu
}

/// Reveal the selection bar while anything is selected and count what is.
fn sync_trash_selection(ui: &Rc<Ui>) {
    let count = ui.trash.files.selection.selection().size() as usize;
    ui.trash.selection_bar.set_reveal_child(count > 0);
    if count > 0 {
        ui.trash.selection_label.set_label(&ngettext_f(
            "{n} item selected",
            "{n} items selected",
            count as u64,
            &[],
        ));
    }
}

/// Fetch the trash listing and repaint the page.
pub(crate) fn load_trash(ui: &Rc<Ui>) {
    cancel_file_thumbnails(ui);
    // The rows stay up while the trash is read again. Trashed items are
    // addressed by uid, so acting on one that has gone meanwhile fails
    // cleanly rather than hitting something else.
    let ui_p = ui.clone();
    let ticket = ui.trash.loader.refresh(move || {
        clear_trash(&ui_p);
        ui_p.trash.empty.set_sensitive(false);
        trash_status(
            &ui_p,
            "user-trash-symbolic",
            &gettext("Loading…"),
            &gettext("Reading the trash."),
            false,
        );
    });

    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListTrash);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        if !ticket.is_current() {
            return;
        }
        if !matches!(result, Ok(Ok(Response::Entries { .. }))) {
            clear_trash(&ui);
        }
        match result {
            Ok(Ok(Response::Entries { entries })) => {
                let mut entries = entries;
                sort_trash(&mut entries);
                *ui.trash.all.borrow_mut() = entries;
                keep_trash_place(&ui);
                if matches!(ui.trash.nav.borrow().last(), Some(TrashNav::Folder { .. })) {
                    load_trashed_folder(&ui);
                } else {
                    render_trash(&ui);
                }
            }
            Ok(Ok(Response::Error { message, .. })) => trash_status(
                &ui,
                "dialog-warning-symbolic",
                &gettext("Couldn't read the trash"),
                &message,
                false,
            ),
            Ok(Ok(_)) => trash_status(
                &ui,
                "dialog-warning-symbolic",
                &gettext("Couldn't read the trash"),
                &gettext("Unexpected reply from the Proton Drive service."),
                false,
            ),
            Ok(Err(_)) | Err(_) => trash_unreachable(&ui),
        }
    });
}

/// The daemon didn't answer. Same split as the Files page: still starting (poll
/// again, no button) versus actually down (Retry, which restarts the service).
pub(crate) fn trash_unreachable(ui: &Rc<Ui>) {
    service_unreachable(ui, "trash", trash_status, load_trash);
}

/// Show a status page in place of the trash list.
pub(crate) fn trash_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.trash.status.set_icon_name(Some(icon));
    ui.trash.status.set_title(title);
    ui.trash
        .status
        .set_description(Some(&gtk4::glib::markup_escape_text(description)));
    ui.trash.retry.set_visible(retry);
    ui.trash.content.set_visible_child_name("status");
    ui.trash.subtitle.set_subtitle("");
}

/// Go to the place `nav` now points at: read a trashed folder, or show
/// what the stored listing already holds.
fn show_trash_place(ui: &Rc<Ui>) {
    if !ui.trash.search.text().is_empty() {
        ui.trash.search.set_text("");
    }
    ui.trash.files.selection.unselect_all();
    ui.trash.files.scroll_to_top();
    if matches!(ui.trash.nav.borrow().last(), Some(TrashNav::Folder { .. })) {
        load_trashed_folder(ui);
    } else {
        render_trash(ui);
    }
}

/// Read what the trashed folder on screen holds, then show it.
fn load_trashed_folder(ui: &Rc<Ui>) {
    let Some(TrashNav::Folder { uid, .. }) = ui.trash.nav.borrow().last().cloned() else {
        return;
    };
    ui.trash.folder_rows.borrow_mut().take();
    let ui_p = ui.clone();
    let ticket = ui.trash.loader.replace(move || {
        clear_trash(&ui_p);
        trash_status(
            &ui_p,
            "folder-open-symbolic",
            &gettext("Loading…"),
            &gettext("Reading this folder."),
            false,
        );
    });
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListTrashedFolder { uid: uid.clone() },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        if !ticket.is_current() {
            return;
        }
        let still_here = matches!(
            ui.trash.nav.borrow().last(),
            Some(TrashNav::Folder { uid: on, .. }) if *on == uid
        );
        if !still_here {
            return;
        }
        match result {
            Ok(Ok(Response::Entries { entries })) => {
                *ui.trash.folder_rows.borrow_mut() = Some(entries);
                render_trash(&ui);
            }
            Ok(Ok(Response::Error { message, .. })) => {
                clear_trash(&ui);
                trash_status(
                    &ui,
                    "dialog-warning-symbolic",
                    &gettext("Couldn't open the folder"),
                    &message,
                    false,
                );
            }
            Ok(Ok(_)) => {
                clear_trash(&ui);
                trash_status(
                    &ui,
                    "dialog-warning-symbolic",
                    &gettext("Couldn't open the folder"),
                    &gettext("Unexpected reply from the Proton Drive service."),
                    false,
                );
            }
            Ok(Err(_)) | Err(_) => trash_unreachable(&ui),
        }
    });
}

/// After a new listing, step back out of places that are gone from it.
fn keep_trash_place(ui: &Rc<Ui>) {
    let all = ui.trash.all.borrow();
    let mut nav = ui.trash.nav.borrow_mut();
    let keep = kept_steps(&nav, &all);
    nav.truncate(keep);
}

/// How many steps of `nav` still lead somewhere in `all`. A location stays
/// while something deleted from it is listed, and the trashed folder opened
/// from it while that folder is; the folders below it went with it.
fn kept_steps(nav: &[TrashNav], all: &[DirEntry]) -> usize {
    let mut inside = false;
    for (i, step) in nav.iter().enumerate() {
        let there = match step {
            TrashNav::Location { key, .. } => all
                .iter()
                .any(|e| e.trashed_parent.is_none() && e.trashed_from == *key),
            TrashNav::Folder { uid, .. } => inside || all.iter().any(|e| e.uid == *uid),
        };
        if !there {
            return i;
        }
        inside |= matches!(step, TrashNav::Folder { .. });
    }
    nav.len()
}

/// Show the rows for the current place, search and grouping.
pub(crate) fn render_trash(ui: &Rc<Ui>) {
    let all = ui.trash.all.borrow();
    // An empty Trash has nothing to empty, so the button goes away rather than
    // sitting there greyed out.
    ui.trash.empty.set_sensitive(true);
    ui.trash.empty.set_visible(!all.is_empty());
    repaint_trash_crumb(ui);
    if all.is_empty() {
        drop(all);
        trash_status(
            ui,
            "user-trash-symbolic",
            &gettext("Trash is empty"),
            &gettext("Items you delete from Proton Drive show up here."),
            false,
        );
        clear_trash(ui);
        return;
    }
    let subtitle = ngettext_f("{n} item", "{n} items", all.len() as u64, &[]);

    let query = ui.trash.search.text().to_lowercase();
    let nav = ui.trash.nav.borrow().clone();
    let rows: Vec<DirEntry> = if !query.is_empty() {
        all.iter()
            .filter(|e| e.name.to_lowercase().contains(&query))
            .cloned()
            .collect()
    } else if !ui.trash.grouped.is_active() {
        all.clone()
    } else {
        match nav.last() {
            None => trash_locations(&all),
            Some(TrashNav::Location { key, .. }) => all
                .iter()
                .filter(|e| e.trashed_parent.is_none() && e.trashed_from == *key)
                .cloned()
                .collect(),
            Some(TrashNav::Folder { .. }) => {
                ui.trash.folder_rows.borrow().clone().unwrap_or_default()
            }
        }
    };
    drop(all);
    if rows.is_empty() {
        clear_trash(ui);
        if query.is_empty() {
            trash_status(
                ui,
                "folder-open-symbolic",
                &gettext("This folder is empty"),
                &gettext("Nothing else was in this folder when it was deleted."),
                false,
            );
        } else {
            trash_status(
                ui,
                "edit-find-symbolic",
                &gettext("No results"),
                &gettext("Nothing in the trash has that name."),
                false,
            );
        }
        ui.trash.subtitle.set_subtitle(&subtitle);
        return;
    }
    ui.trash.content.set_visible_child_name("list");
    ui.trash.subtitle.set_subtitle(&subtitle);
    // Only the rows that changed are swapped, so a refresh keeps the
    // selection and the scroll position.
    replace_items(&ui.trash.files.model, &rows);
    sync_trash_selection(ui);
}

/// One row per folder things were deleted from, the most recent first. Items
/// trashed inside a trashed folder are found by opening that folder.
fn trash_locations(all: &[DirEntry]) -> Vec<DirEntry> {
    let mut rows: Vec<DirEntry> = Vec::new();
    let mut index: HashMap<Option<String>, usize> = HashMap::new();
    for entry in all.iter().filter(|e| e.trashed_parent.is_none()) {
        let key = entry.trashed_from.clone();
        let at = *index.entry(key.clone()).or_insert_with(|| {
            rows.push(location_row(key));
            rows.len() - 1
        });
        let row = &mut rows[at];
        row.size += 1;
        row.trashed_at = row.trashed_at.max(entry.trashed_at);
        row.modified = row.modified.max(entry.modified);
    }
    sort_trash(&mut rows);
    rows
}

/// The row that stands for the folder `key`, still holding nothing.
fn location_row(key: Option<String>) -> DirEntry {
    let (uid, name) = match key.as_deref() {
        None => (
            format!("{LOCATION_UID}{UNKNOWN_LOCATION}"),
            gettext("Unknown folder"),
        ),
        Some("") => (LOCATION_UID.to_string(), gettext("My files")),
        Some(path) => (
            format!("{LOCATION_UID}{path}"),
            path.rsplit('/').next().unwrap_or(path).to_string(),
        ),
    };
    DirEntry {
        name,
        is_dir: true,
        size: 0,
        modified: 0,
        pinned: false,
        pinned_by_folder: false,
        cached: false,
        uid,
        path: String::new(),
        role: String::new(),
        shared_by: String::new(),
        shared_at: 0,
        shared_by_unverified: false,
        trashed_at: 0,
        trashed_from: key,
        trashed_parent: None,
        issue: None,
    }
}

/// The folder a location row stands for, or `None` for any other row.
fn location_key(uid: &str) -> Option<Option<String>> {
    let key = uid.strip_prefix(LOCATION_UID)?;
    Some((key != UNKNOWN_LOCATION).then(|| key.to_string()))
}

/// Whether the stored listing holds `uid`, so it can be restored on its own.
fn in_trash(ui: &Ui, uid: &str) -> bool {
    ui.trash.all.borrow().iter().any(|e| e.uid == uid)
}

/// The trashed folder the user opened from a location, when inside one.
fn outer_folder(ui: &Ui) -> Option<DirEntry> {
    let nav = ui.trash.nav.borrow();
    let uid = nav.iter().find_map(|step| match step {
        TrashNav::Folder { uid, .. } => Some(uid.clone()),
        TrashNav::Location { .. } => None,
    })?;
    ui.trash.all.borrow().iter().find(|e| e.uid == uid).cloned()
}

/// What acting on `entries` acts on: a location row stands for everything
/// deleted from that folder, and an item that went with its folder for the
/// folder. Each node once.
fn trashed_targets(ui: &Ui, entries: &[DirEntry]) -> Vec<DirEntry> {
    let all = ui.trash.all.borrow();
    let outer = outer_folder(ui);
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for entry in entries {
        let picked: Vec<DirEntry> = if let Some(key) = location_key(&entry.uid) {
            all.iter()
                .filter(|e| e.trashed_parent.is_none() && e.trashed_from == key)
                .cloned()
                .collect()
        } else if all.iter().any(|e| e.uid == entry.uid) {
            vec![entry.clone()]
        } else {
            outer.clone().into_iter().collect()
        };
        for target in picked {
            if seen.insert(target.uid.clone()) {
                targets.push(target);
            }
        }
    }
    targets
}

/// Rebuild the path bar for `nav`: "Trash", then each place down to the
/// current one. Hidden at the top, where there is nowhere to go back to.
fn repaint_trash_crumb(ui: &Rc<Ui>) {
    let nav = ui.trash.nav.borrow();
    let crumb = &ui.trash.crumb;
    while let Some(child) = crumb.first_child() {
        crumb.remove(&child);
    }
    ui.trash.back.set_visible(!nav.is_empty());
    crumb.append(&trash_crumb_node(ui, &gettext("Trash"), 0, nav.is_empty()));
    for (i, step) in nav.iter().enumerate() {
        let label = match step {
            TrashNav::Location { label, .. } => label,
            TrashNav::Folder { name, .. } => name,
        };
        crumb.append(&crumb_separator());
        crumb.append(&trash_crumb_node(ui, label, i + 1, i + 1 == nav.len()));
    }
}

fn trash_crumb_node(ui: &Rc<Ui>, label: &str, depth: usize, current: bool) -> gtk4::Widget {
    if current {
        return crumb_current(label);
    }
    let button = gtk4::Button::builder().label(label).build();
    button.add_css_class("flat");
    let ui = ui.clone();
    button.connect_clicked(move |_| {
        ui.trash.nav.borrow_mut().truncate(depth);
        show_trash_place(&ui);
    });
    button.upcast()
}

/// Newest deletion first. Entries with no known deletion time go last, newest
/// modification first among themselves.
fn sort_trash(entries: &mut [DirEntry]) {
    entries.sort_by_key(|e| {
        (
            e.trashed_at == 0,
            std::cmp::Reverse(e.trashed_at),
            std::cmp::Reverse(e.modified),
        )
    });
}

/// Drop every row, and with them the selection and its bar.
fn clear_trash(ui: &Rc<Ui>) {
    ui.trash.files.model.remove_all();
    sync_trash_selection(ui);
}

/// Restore trashed entries to the folders they were trashed from. When they
/// all came from one known folder, the toast offers to show it.
pub(crate) fn restore_entries(ui: &Rc<Ui>, entries: &[DirEntry]) {
    let entries = &trashed_targets(ui, entries)[..];
    if entries.is_empty() {
        return;
    }
    let message = match entries {
        // Translators: {name} is a file or folder name.
        [one] => gettext_f("Restored “{name}”", &[("name", &one.name)]),
        _ => ngettext_f(
            "Restored {n} item",
            "Restored {n} items",
            entries.len() as u64,
            &[],
        ),
    };
    let folder = common_origin(entries);
    run_mutation_then(
        ui,
        Request::Restore {
            uids: entries.iter().map(|e| e.uid.clone()).collect(),
        },
        gettext_noop("Couldn't restore"),
        move |ui| match folder {
            Some(folder) => toast_action(ui, &message, &gettext("Show"), move |ui| {
                open_in_my_files(ui, folder.clone())
            }),
            None => toast(ui, &message),
        },
    );
}

/// The folder every entry goes back to, when that is one known folder.
fn common_origin(entries: &[DirEntry]) -> Option<String> {
    let first = entries.first()?.trashed_from.clone()?;
    entries
        .iter()
        .all(|e| e.trashed_from.as_deref() == Some(first.as_str()))
        .then_some(first)
}

/// Confirm, then permanently delete trashed entries. Irreversible, so it asks.
pub(crate) fn prompt_delete_forever(ui: &Rc<Ui>, entries: &[DirEntry]) {
    let entries = &trashed_targets(ui, entries)[..];
    if entries.is_empty() {
        return;
    }
    let win = ui_window(ui);
    let uids: Vec<String> = entries.iter().map(|e| e.uid.clone()).collect();
    let count = entries.len() as u64;
    let (body, done) = match entries {
        [one] => {
            let args = [("name", one.name.as_str())];
            (
                // Translators: {name} is a file or folder name.
                gettext_f("Permanently delete “{name}”? This cannot be undone.", &args),
                // Translators: {name} is a file or folder name.
                gettext_f("Deleted “{name}” permanently", &args),
            )
        }
        _ => (
            ngettext_f(
                "Permanently delete {n} item? This cannot be undone.",
                "Permanently delete {n} items? This cannot be undone.",
                count,
                &[],
            ),
            ngettext_f(
                "Deleted {n} item permanently",
                "Deleted {n} items permanently",
                count,
                &[],
            ),
        ),
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Delete Permanently?"))
        .body(body)
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("delete", &gettext("Delete Permanently"));
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "delete" {
            run_mutation(
                &ui,
                Request::DeleteForever { uids: uids.clone() },
                done.clone(),
                gettext_noop("Couldn't delete"),
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Confirm, then permanently delete everything in the trash.
pub(crate) fn prompt_empty_trash(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let count = ui.trash.all.borrow().len();
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Empty Trash?"))
        .body(ngettext_f(
            "Permanently delete all {n} item in the trash? This cannot be undone.",
            "Permanently delete all {n} items in the trash? This cannot be undone.",
            count as u64,
            &[],
        ))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("empty", &gettext("Empty Trash"));
    dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "empty" {
            run_mutation(
                &ui,
                Request::EmptyTrash,
                gettext("Trash emptied"),
                gettext_noop("Couldn't empty the trash"),
            );
        }
    });
    dialog.present(win.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trashed(name: &str, trashed_at: i64, modified: i64, from: Option<&str>) -> DirEntry {
        DirEntry {
            name: name.into(),
            is_dir: false,
            size: 0,
            modified,
            pinned: false,
            pinned_by_folder: false,
            cached: false,
            uid: name.into(),
            path: String::new(),
            role: String::new(),
            shared_by: String::new(),
            shared_at: 0,
            shared_by_unverified: false,
            trashed_at,
            trashed_from: from.map(str::to_string),
            trashed_parent: None,
            issue: None,
        }
    }

    fn inside(name: &str, parent: &str) -> DirEntry {
        DirEntry {
            trashed_parent: Some(parent.into()),
            ..trashed(name, 5, 1, None)
        }
    }

    #[test]
    fn locations_group_top_level_items_by_folder() {
        let all = vec![
            trashed("a", 30, 1, Some("Photos/2019")),
            trashed("b", 10, 1, Some("Photos/2019")),
            trashed("c", 20, 1, Some("")),
            trashed("d", 5, 1, None),
            inside("e", "a"),
        ];
        let rows = trash_locations(&all);
        let summary: Vec<(&str, u64, i64)> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.size, r.trashed_at))
            .collect();
        assert_eq!(
            summary,
            [
                ("2019", 2, 30),
                ("My files", 1, 20),
                ("Unknown folder", 1, 5)
            ]
        );
        assert_eq!(location_key(&rows[0].uid), Some(Some("Photos/2019".into())));
        assert_eq!(location_key(&rows[1].uid), Some(Some(String::new())));
        assert_eq!(location_key(&rows[2].uid), Some(None));
        assert_eq!(location_key("vol~link"), None);
    }

    #[test]
    fn a_place_stays_only_while_the_listing_still_leads_there() {
        let all = vec![trashed("album", 1, 1, Some("Photos")), inside("x", "album")];
        let location = TrashNav::Location {
            key: Some("Photos".into()),
            label: "Photos".into(),
        };
        let folder = |uid: &str| TrashNav::Folder {
            uid: uid.into(),
            name: uid.into(),
        };
        // Folders below the trashed one are not listed themselves.
        let nav = [location.clone(), folder("album"), folder("child")];
        assert_eq!(kept_steps(&nav, &all), 3);
        assert_eq!(kept_steps(&[location.clone(), folder("gone")], &all), 1);
        assert_eq!(kept_steps(&nav, &[]), 0);
    }

    #[test]
    fn trash_lists_newest_deletion_first_and_undated_last() {
        let mut entries = vec![
            trashed("undated-old", 0, 1, None),
            trashed("older", 10, 50, None),
            trashed("undated-new", 0, 9, None),
            trashed("newest", 20, 5, None),
        ];
        sort_trash(&mut entries);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["newest", "older", "undated-new", "undated-old"]);
    }

    #[test]
    fn restore_offers_a_folder_only_when_all_share_one() {
        let docs = trashed("a", 1, 1, Some("Documents"));
        let docs2 = trashed("b", 1, 1, Some("Documents"));
        let root = trashed("c", 1, 1, Some(""));
        let unknown = trashed("d", 1, 1, None);
        assert_eq!(
            common_origin(&[docs.clone(), docs2]).as_deref(),
            Some("Documents")
        );
        assert_eq!(
            common_origin(std::slice::from_ref(&root)).as_deref(),
            Some("")
        );
        assert_eq!(common_origin(&[docs.clone(), root]), None);
        assert_eq!(common_origin(&[docs, unknown]), None);
        assert_eq!(common_origin(&[]), None);
    }
}
