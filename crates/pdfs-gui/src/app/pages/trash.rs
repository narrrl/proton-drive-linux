use crate::*;

pub(crate) struct TrashState {
    // Trash page. Trashed nodes are addressed by uid, not by path, so this page
    // keeps no current-directory state — it re-lists from the daemon on show.
    pub(crate) model: gio::ListStore,
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    /// Empties the trash; insensitive while it is empty (or unread).
    pub(crate) empty: gtk4::Button,
    /// "12 items" under the page title.
    pub(crate) subtitle: adw::WindowTitle,
    pub(crate) selection: gtk4::MultiSelection,
    /// The bottom bar acting on the selection; revealed while anything is selected.
    pub(crate) selection_bar: gtk4::Revealer,
    pub(crate) selection_label: gtk4::Label,
}

/// The widgets of the Trash page that a load repaints.
pub(crate) struct TrashWidgets {
    pub(crate) model: gio::ListStore,
    pub(crate) list: gtk4::ListView,
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) empty: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
    pub(crate) subtitle: adw::WindowTitle,
    pub(crate) selection: gtk4::MultiSelection,
    pub(crate) selection_bar: gtk4::Revealer,
    pub(crate) selection_label: gtk4::Label,
    pub(crate) restore_selected: gtk4::Button,
    pub(crate) delete_selected: gtk4::Button,
}

/// The Trash page: a flat list of everything Drive is holding in the trash, each
/// row offering Restore and Delete Forever, with Empty Trash in the header. A
/// selection (click, Shift/Ctrl+click, Ctrl+A) gets the same two actions in a
/// bottom bar.
///
/// A trashed node has no path inside the mount — the daemon forgets it when it is
/// trashed — so unlike the Files page this one addresses entries by uid and always
/// re-lists from the server rather than from a cached listing. Row rendering needs
/// the [`Ui`] handle for its buttons, so the factory is installed in [`wire_trash`].
pub(crate) fn build_trash_page() -> (gtk4::Widget, TrashWidgets) {
    let model = gio::ListStore::new::<BoxedAnyObject>();

    let empty = gtk4::Button::builder()
        .label(gettext("Empty Trash…"))
        .tooltip_text(gettext("Permanently delete everything in the Trash"))
        .sensitive(false)
        .build();
    let refresh = refresh_button();

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

    // Bulk Delete Forever always confirms with the count, so a stale selection
    // can't delete anything silently.
    let selection = gtk4::MultiSelection::new(Some(model.clone()));
    let list = gtk4::ListView::builder().model(&selection).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&list)
        .build();

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&scroll, Some("list"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(18);
    inner.set_margin_bottom(18);
    inner.set_margin_start(18);
    inner.set_margin_end(18);
    inner.append(&content);

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
    header.pack_end(&refresh);

    (
        frame.upcast(),
        TrashWidgets {
            model,
            list,
            content,
            status,
            retry,
            empty,
            refresh,
            subtitle,
            selection,
            selection_bar,
            selection_label,
            restore_selected,
            delete_selected,
        },
    )
}

/// Install the row factory and the Empty Trash button. The row's two buttons read
/// the entry off the [`gtk4::ListItem`] they were clicked on rather than a
/// captured copy, so a recycled row always acts on the item it currently shows.
pub(crate) fn wire_trash(ui: &Rc<Ui>, widgets: &TrashWidgets) {
    let list = &widgets.list;
    let empty = &widgets.empty;
    let factory = gtk4::SignalListItemFactory::new();
    let ui_setup = ui.clone();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();

        let thumbnail = file_thumbnail_widget(40, 32);
        let name = gtk4::Label::builder()
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .build();
        let meta = gtk4::Label::builder().halign(gtk4::Align::Start).build();
        meta.add_css_class("dim-label");
        meta.add_css_class("caption");
        let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.set_valign(gtk4::Align::Center);
        text.append(&name);
        text.append(&meta);

        let restore = gtk4::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text(gettext("Restore to its original folder"))
            .valign(gtk4::Align::Center)
            .build();
        restore.add_css_class("flat");
        let purge = gtk4::Button::builder()
            .icon_name("edit-delete-symbolic")
            .tooltip_text(gettext("Delete permanently"))
            .valign(gtk4::Align::Center)
            .build();
        purge.add_css_class("flat");

        let ui_restore = ui_setup.clone();
        let item_restore = item.clone();
        restore.connect_clicked(move |_| {
            if let Some(entry) = bound_entry(&item_restore) {
                restore_entry(&ui_restore, &entry);
            }
        });
        let ui_purge = ui_setup.clone();
        let item_purge = item.clone();
        purge.connect_clicked(move |_| {
            if let Some(entry) = bound_entry(&item_purge) {
                prompt_delete_forever(&ui_purge, std::slice::from_ref(&entry));
            }
        });

        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(6);
        row.set_margin_end(6);
        row.append(&thumbnail);
        row.append(&text);
        row.append(&restore);
        row.append(&purge);
        item.set_child(Some(&row));
    });
    factory.connect_bind({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let Some(entry) = bound_entry(item) else {
                return;
            };
            let row = item.child().and_downcast::<gtk4::Box>().unwrap();
            let Some(thumbnail) = row.first_child().and_downcast::<gtk4::Overlay>() else {
                return;
            };
            bind_file_thumbnail(&ui, &thumbnail, &entry, true);
            let Some(text) = thumbnail.next_sibling().and_downcast::<gtk4::Box>() else {
                return;
            };
            if let Some(name) = text.first_child().and_downcast::<gtk4::Label>() {
                name.set_label(&entry.name);
            }
            if let Some(meta) = text.last_child().and_downcast::<gtk4::Label>() {
                let kind = if entry.is_dir {
                    gettext("Folder")
                } else {
                    human_bytes(entry.size)
                };
                let modified = format_modified(entry.modified);
                // Translators: {kind} is "Folder" or a file size; {modified} is a date.
                let text = gettext_f(
                    "{kind} · {modified}",
                    &[("kind", &kind), ("modified", &modified)],
                );
                meta.set_label(&text);
            }
        }
    });
    list.set_factory(Some(&factory));

    let ui_empty = ui.clone();
    empty.connect_clicked(move |_| prompt_empty_trash(&ui_empty));

    let ui_sel = ui.clone();
    widgets
        .selection
        .connect_selection_changed(move |_, _, _| sync_trash_selection(&ui_sel));
    let ui_restore = ui.clone();
    widgets.restore_selected.connect_clicked(move |_| {
        let entries = selected_trash(&ui_restore);
        if !entries.is_empty() {
            restore_entries(&ui_restore, &entries);
        }
    });
    let ui_delete = ui.clone();
    widgets.delete_selected.connect_clicked(move |_| {
        let entries = selected_trash(&ui_delete);
        if !entries.is_empty() {
            prompt_delete_forever(&ui_delete, &entries);
        }
    });
}

/// The trashed entries currently selected, in list order.
fn selected_trash(ui: &Rc<Ui>) -> Vec<DirEntry> {
    let selected = ui.trash.selection.selection();
    (0..selected.size())
        .filter_map(|i| ui.trash.model.item(selected.nth(i as u32)))
        .filter_map(|obj| obj.downcast::<BoxedAnyObject>().ok())
        .map(|obj| obj.borrow::<DirEntry>().clone())
        .collect()
}

/// Reveal the selection bar while anything is selected and count what is.
fn sync_trash_selection(ui: &Rc<Ui>) {
    let count = ui.trash.selection.selection().size() as usize;
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

/// The [`DirEntry`] a list item is currently bound to, or `None` for an unbound
/// (recycled) row.
pub(crate) fn bound_entry(item: &gtk4::ListItem) -> Option<DirEntry> {
    let obj = item.item().and_downcast::<BoxedAnyObject>()?;
    let entry = obj.borrow::<DirEntry>().clone();
    Some(entry)
}

/// Fetch the trash listing and repaint the page.
pub(crate) fn load_trash(ui: &Rc<Ui>) {
    cancel_file_thumbnails(ui);
    // Drop the old rows first: a stale row here would offer Restore on something
    // that may already be gone.
    ui.trash.model.remove_all();
    ui.trash.empty.set_sensitive(false);
    sync_trash_selection(ui);
    trash_status(
        ui,
        "user-trash-symbolic",
        &gettext("Loading…"),
        &gettext("Reading the trash."),
        false,
    );

    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListTrash);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
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
                &gettext("Unexpected reply from the mount service."),
                false,
            ),
            Ok(Err(_)) | Err(_) => trash_unreachable(&ui),
        }
    });
}

/// The daemon didn't answer. Same split as the Files page: still starting (poll
/// again, no button) versus actually down (Retry, which restarts the service).
pub(crate) fn trash_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        trash_status(
            ui,
            "network-offline-symbolic",
            &gettext("Not connected"),
            &gettext("The Proton Drive mount service isn't running."),
            true,
        );
        return;
    }
    trash_status(
        ui,
        "folder-remote-symbolic",
        &gettext("Connecting…"),
        &gettext("Waiting for the Proton Drive mount service to come up."),
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("trash") {
            load_trash(&ui);
        }
    });
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

/// Repopulate the trash list, most recently modified first — the order in which a
/// user looks for what they just deleted.
pub(crate) fn repaint_trash(ui: &Rc<Ui>, entries: &[DirEntry]) {
    ui.trash.model.remove_all();
    sync_trash_selection(ui);
    ui.trash.empty.set_sensitive(!entries.is_empty());
    if entries.is_empty() {
        trash_status(
            ui,
            "user-trash-symbolic",
            &gettext("Trash is empty"),
            &gettext("Items you delete from Proton Drive show up here."),
            false,
        );
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
    sorted.sort_by_key(|e| std::cmp::Reverse(e.modified));
    for entry in sorted {
        ui.trash.model.append(&BoxedAnyObject::new(entry));
    }
}

/// Restore one trashed entry to the folder it was trashed from.
pub(crate) fn restore_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    restore_entries(ui, std::slice::from_ref(entry));
}

/// Restore trashed entries to the folders they were trashed from.
pub(crate) fn restore_entries(ui: &Rc<Ui>, entries: &[DirEntry]) {
    run_mutation(
        ui,
        Request::Restore {
            uids: entries.iter().map(|e| e.uid.clone()).collect(),
        },
        match entries {
            // Translators: {name} is a file or folder name.
            [one] => gettext_f("Restored “{name}”", &[("name", &one.name)]),
            _ => ngettext_f(
                "Restored {n} item",
                "Restored {n} items",
                entries.len() as u64,
                &[],
            ),
        },
        gettext_noop("Couldn't restore"),
    );
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
        .heading(gettext("Delete Permanently"))
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
    let count = ui.trash.model.n_items();
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Empty Trash"))
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
