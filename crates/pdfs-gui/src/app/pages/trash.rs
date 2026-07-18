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
    pub(crate) subtitle: gtk4::Label,
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
    pub(crate) subtitle: gtk4::Label,
}

/// The Trash page: a flat list of everything Drive is holding in the trash, each
/// row offering Restore and Delete Forever, with Empty Trash in the header.
///
/// A trashed node has no path inside the mount — the daemon forgets it when it is
/// trashed — so unlike the Files page this one addresses entries by uid and always
/// re-lists from the server rather than from a cached listing. Row rendering needs
/// the [`Ui`] handle for its buttons, so the factory is installed in [`wire_trash`].
pub(crate) fn build_trash_page() -> (gtk4::Widget, TrashWidgets) {
    let model = gio::ListStore::new::<BoxedAnyObject>();

    let title = gtk4::Label::builder()
        .label("Trash")
        .halign(gtk4::Align::Start)
        .build();
    title.add_css_class("title-2");
    let subtitle = gtk4::Label::builder().halign(gtk4::Align::Start).build();
    subtitle.add_css_class("dim-label");
    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title);
    titles.append(&subtitle);

    let empty = gtk4::Button::builder()
        .label("Empty Trash")
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    empty.add_css_class("destructive-action");
    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&titles);
    header.append(&refresh);
    header.append(&empty);

    let retry = gtk4::Button::builder()
        .label("Retry")
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

    // No selection model: every action lives on the row it acts on, so a
    // mis-aimed Delete Forever isn't one click away from a stale selection.
    let list = gtk4::ListView::builder()
        .model(&gtk4::NoSelection::new(Some(model.clone())))
        .build();
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
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&content);

    (
        inner.upcast(),
        TrashWidgets {
            model,
            list,
            content,
            status,
            retry,
            empty,
            refresh,
            subtitle,
        },
    )
}

/// Install the row factory and the Empty Trash button. The row's two buttons read
/// the entry off the [`gtk4::ListItem`] they were clicked on rather than a
/// captured copy, so a recycled row always acts on the item it currently shows.
pub(crate) fn wire_trash(ui: &Rc<Ui>, list: &gtk4::ListView, empty: &gtk4::Button) {
    let factory = gtk4::SignalListItemFactory::new();
    let ui_setup = ui.clone();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();

        let icon = gtk4::Image::builder().pixel_size(32).build();
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
            .tooltip_text("Restore to its original folder")
            .valign(gtk4::Align::Center)
            .build();
        restore.add_css_class("flat");
        let purge = gtk4::Button::builder()
            .icon_name("edit-delete-symbolic")
            .tooltip_text("Delete permanently")
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
                prompt_delete_forever(&ui_purge, &entry);
            }
        });

        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(6);
        row.set_margin_end(6);
        row.append(&icon);
        row.append(&text);
        row.append(&restore);
        row.append(&purge);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let Some(entry) = bound_entry(item) else {
            return;
        };
        let row = item.child().and_downcast::<gtk4::Box>().unwrap();
        let Some(icon) = row.first_child().and_downcast::<gtk4::Image>() else {
            return;
        };
        icon.set_icon_name(Some(&format!("{}-symbolic", icon_base_for(&entry))));
        let Some(text) = icon.next_sibling().and_downcast::<gtk4::Box>() else {
            return;
        };
        if let Some(name) = text.first_child().and_downcast::<gtk4::Label>() {
            name.set_label(&entry.name);
        }
        if let Some(meta) = text.last_child().and_downcast::<gtk4::Label>() {
            let kind = if entry.is_dir {
                "Folder".to_string()
            } else {
                human_bytes(entry.size)
            };
            meta.set_label(&format!("{kind} · {}", format_modified(entry.modified)));
        }
    });
    list.set_factory(Some(&factory));

    let ui_empty = ui.clone();
    empty.connect_clicked(move |_| prompt_empty_trash(&ui_empty));
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
    // Drop the old rows first: a stale row here would offer Restore on something
    // that may already be gone.
    ui.trash.model.remove_all();
    ui.trash.empty.set_sensitive(false);
    trash_status(
        ui,
        "user-trash-symbolic",
        "Loading…",
        "Reading the trash.",
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
                "Couldn't read the trash",
                &message,
                false,
            ),
            Ok(Ok(_)) => trash_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read the trash",
                "Unexpected reply from the mount service.",
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
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    trash_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
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
    ui.trash.subtitle.set_label("");
}

/// Repopulate the trash list, most recently modified first — the order in which a
/// user looks for what they just deleted.
pub(crate) fn repaint_trash(ui: &Rc<Ui>, entries: &[DirEntry]) {
    ui.trash.model.remove_all();
    ui.trash.empty.set_sensitive(!entries.is_empty());
    if entries.is_empty() {
        trash_status(
            ui,
            "user-trash-symbolic",
            "Trash is empty",
            "Items you delete from Proton Drive show up here.",
            false,
        );
        return;
    }
    ui.trash.content.set_visible_child_name("list");
    ui.trash.subtitle.set_label(&match entries.len() {
        1 => "1 item".to_string(),
        n => format!("{n} items"),
    });

    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.modified));
    for entry in sorted {
        ui.trash.model.append(&BoxedAnyObject::new(entry));
    }
}

/// Restore one trashed entry to the folder it was trashed from.
pub(crate) fn restore_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let name = entry.name.clone();
    run_mutation(
        ui,
        Request::Restore {
            uids: vec![entry.uid.clone()],
        },
        format!("Restored “{name}”"),
        "Couldn't restore",
    );
}

/// Confirm, then permanently delete one trashed entry. Irreversible, so it asks.
pub(crate) fn prompt_delete_forever(ui: &Rc<Ui>, entry: &DirEntry) {
    let win = ui_window(ui);
    let uid = entry.uid.clone();
    let name = entry.name.clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Delete permanently")
        .body(format!(
            "Permanently delete “{name}”? This cannot be undone."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete Permanently");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "delete" {
            run_mutation(
                &ui,
                Request::DeleteForever {
                    uids: vec![uid.clone()],
                },
                format!("Deleted “{name}” permanently"),
                "Couldn't delete",
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
        .heading("Empty Trash")
        .body(format!(
            "Permanently delete all {count} item(s) in the trash? This cannot be undone."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("empty", "Empty Trash");
    dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "empty" {
            run_mutation(
                &ui,
                Request::EmptyTrash,
                "Trash emptied".to_string(),
                "Couldn't empty the trash",
            );
        }
    });
    dialog.present(win.as_ref());
}
