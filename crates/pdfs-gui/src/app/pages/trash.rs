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
}

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

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
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
            if e.is_dir {
                "—".to_string()
            } else {
                human_bytes(e.size)
            }
        }));

    let ui_empty = ui.clone();
    widgets
        .empty
        .connect_clicked(move |_| prompt_empty_trash(&ui_empty));

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

/// A trashed file can't be opened; say what would let it be.
fn activate_trashed(ui: &Rc<Ui>, entry: &DirEntry) {
    toast(
        ui,
        // Translators: {name} is a file or folder name.
        &gettext_f("Restore “{name}” to open it", &[("name", &entry.name)]),
    );
}

fn trash_entry_menu(ui: &Rc<Ui>, entry: &DirEntry) -> ActionMenu {
    let mut menu = ActionMenu::new();
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
    if ui.trash.files.model.n_items() > 0 {
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
            Ok(Ok(Response::Entries { entries })) => repaint_trash(&ui, &entries),
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
    ui.trash.status.set_description(Some(description));
    ui.trash.retry.set_visible(retry);
    ui.trash.content.set_visible_child_name("status");
    ui.trash.subtitle.set_subtitle("");
}

/// Repopulate the trash, most recently deleted first: the order in which a user
/// looks for what they just deleted.
pub(crate) fn repaint_trash(ui: &Rc<Ui>, entries: &[DirEntry]) {
    // An empty Trash has nothing to empty, so the button goes away rather than
    // sitting there greyed out.
    ui.trash.empty.set_sensitive(true);
    ui.trash.empty.set_visible(!entries.is_empty());
    if entries.is_empty() {
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
    ui.trash.content.set_visible_child_name("list");
    ui.trash.subtitle.set_subtitle(&ngettext_f(
        "{n} item",
        "{n} items",
        entries.len() as u64,
        &[],
    ));

    let mut sorted = entries.to_vec();
    sort_trash(&mut sorted);
    // Only the rows that changed are swapped, so a refresh keeps the
    // selection and the scroll position.
    replace_items(&ui.trash.files.model, &sorted);
    sync_trash_selection(ui);
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
    let count = ui.trash.files.model.n_items();
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
            cached: false,
            uid: name.into(),
            path: String::new(),
            role: String::new(),
            shared_by: String::new(),
            shared_at: 0,
            shared_by_unverified: false,
            trashed_at,
            trashed_from: from.map(str::to_string),
            issue: None,
        }
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
