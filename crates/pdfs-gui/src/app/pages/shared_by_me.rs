use crate::*;

pub(crate) struct SharedByMeState {
    // Shared (by me) page: one section listing the items I've shared, each with a
    // copy-link / manage affordance. Rebuilt wholesale on each load.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) rows: RefCell<Vec<gtk4::Widget>>,
    pub(crate) inflight: Cell<bool>,
    pub(crate) loaded_at: Cell<Option<Instant>>,
}

/// Widgets the Shared (by-me) page's load/repaint touch.
pub(crate) struct SharedByMeWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) retry: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
}

/// The Shared page: one section listing the items I have shared with others —
/// each row summarizing who has access, opening the item when activated, with
/// Copy link and a menu for the link and the per-node Share dialog.
pub(crate) fn build_shared_by_me_page() -> (gtk4::Widget, SharedByMeWidgets) {
    let refresh = refresh_button();

    let group = adw::PreferencesGroup::new();
    let clamp = adw::Clamp::builder().child(&group).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&clamp)
        .build();

    let retry = gtk4::Button::builder()
        .label(gettext("Retry"))
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("emblem-shared-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

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
    let (frame, header, _) = page_frame(&gettext("Shared by Me"), &inner);
    header.pack_end(&refresh);

    (
        frame.upcast(),
        SharedByMeWidgets {
            content,
            status,
            group,
            retry,
            refresh,
        },
    )
}

/// Install the Shared (by-me) page's retry button.
pub(crate) fn wire_shared_by_me(ui: &Rc<Ui>, retry: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_shared_by_me(&ui_retry);
    });
}

/// Show a status page in place of the Shared (by-me) list.
pub(crate) fn shared_by_me_status(
    ui: &Rc<Ui>,
    icon: &str,
    title: &str,
    description: &str,
    retry: bool,
) {
    ui.shared_by_me.status.set_icon_name(Some(icon));
    ui.shared_by_me.status.set_title(title);
    ui.shared_by_me.status.set_description(Some(description));
    ui.shared_by_me.retry.set_visible(retry);
    ui.shared_by_me.content.set_visible_child_name("status");
}

/// Fetch the shared-by-me listing and repaint the page.
pub(crate) fn load_shared_by_me(ui: &Rc<Ui>) {
    if ui.shared_by_me.inflight.get() {
        return;
    }
    cancel_file_thumbnails(ui);
    ui.shared_by_me.inflight.set(true);
    shared_by_me_status(
        ui,
        "emblem-shared-symbolic",
        &gettext("Loading…"),
        &gettext("Reading what you've shared."),
        false,
    );
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSharedByMe);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.shared_by_me.inflight.set(false);
        match result {
            Ok(Ok(Response::SharedByMe { items })) => {
                repaint_shared_by_me(&ui, &items);
                ui.shared_by_me.loaded_at.set(Some(Instant::now()));
            }
            Ok(Ok(Response::Error { message, .. })) => shared_by_me_status(
                &ui,
                "dialog-warning-symbolic",
                &gettext("Couldn't read your shares"),
                &message,
                false,
            ),
            Ok(Ok(_)) => shared_by_me_status(
                &ui,
                "dialog-warning-symbolic",
                &gettext("Couldn't read your shares"),
                &gettext("Unexpected reply from the mount service."),
                false,
            ),
            Ok(Err(_)) | Err(_) => {
                ui.shared_by_me.loaded_at.set(None);
                shared_by_me_unreachable(&ui);
            }
        }
    });
}

/// The daemon didn't answer the Shared (by-me) page.
pub(crate) fn shared_by_me_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        shared_by_me_status(
            ui,
            "network-offline-symbolic",
            &gettext("Not connected"),
            &gettext("The Proton Drive mount service isn't running."),
            true,
        );
        return;
    }
    shared_by_me_status(
        ui,
        "folder-remote-symbolic",
        &gettext("Connecting…"),
        &gettext("Waiting for the Proton Drive mount service to come up."),
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("sharedbyme") {
            load_shared_by_me(&ui);
        }
    });
}

/// Rebuild the Shared (by-me) section from a fresh listing.
pub(crate) fn repaint_shared_by_me(ui: &Rc<Ui>, items: &[SharedItem]) {
    for row in ui.shared_by_me.rows.borrow_mut().drain(..) {
        ui.shared_by_me.group.remove(&row);
    }
    if items.is_empty() {
        shared_by_me_status(
            ui,
            "emblem-shared-symbolic",
            &gettext("Nothing shared yet"),
            &gettext("Items you share with people or by link show up here."),
            false,
        );
        return;
    }
    ui.shared_by_me.content.set_visible_child_name("list");
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for item in items {
        let entry = shared_item_as_entry(item);
        let row = adw::ActionRow::builder()
            .title(&item.name)
            .subtitle(shared_item_summary(item))
            .build();
        row.add_prefix(&file_thumbnail(ui, &entry, 40, 24, true));

        // A node the daemon can place in my tree opens like it would in My Files.
        if !item.path.is_empty() {
            row.set_activatable(true);
            let ui_open = ui.clone();
            let entry_open = entry.clone();
            row.connect_activated(move |_| open_shared_by_me(&ui_open, &entry_open));
        }

        let url = item.link.as_ref().and_then(|l| l.url.clone());
        // The one action people come here for most gets its own button.
        if let Some(url) = url.clone() {
            let copy = gtk4::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text(gettext("Copy link"))
                .valign(gtk4::Align::Center)
                .build();
            copy.add_css_class("flat");
            let ui_copy = ui.clone();
            copy.connect_clicked(move |btn| {
                btn.clipboard().set_text(&url);
                toast(&ui_copy, &gettext("Link copied"));
            });
            row.add_suffix(&copy);
        }
        row.add_suffix(&shared_by_me_menu(ui, &entry, url));

        ui.shared_by_me.group.add(&row);
        rows.push(row.upcast());
    }
    *ui.shared_by_me.rows.borrow_mut() = rows;
}

/// The ⋮ menu on a shared item: open it, the link, the Share dialog, and
/// Stop Sharing.
///
/// Manage access opens the per-node Share dialog, which addresses a pathless
/// node by uid, so every shared item can be managed from here.
fn shared_by_me_menu(ui: &Rc<Ui>, entry: &DirEntry, url: Option<String>) -> gtk4::MenuButton {
    let mut items: Vec<(String, MenuAction)> = Vec::new();
    if !entry.path.is_empty() {
        let (ui_c, entry_c) = (ui.clone(), entry.clone());
        items.push((
            pgettext("verb", "Open"),
            Box::new(move || open_shared_by_me(&ui_c, &entry_c)),
        ));
        let (ui_c, entry_c) = (ui.clone(), entry.clone());
        items.push((
            gettext("Show in My Files"),
            Box::new(move || show_in_my_files(&ui_c, &entry_c)),
        ));
    }
    if let Some(url) = url {
        items.push((gettext("Open Link"), Box::new(move || open_uri(&url))));
    }
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    items.push((
        gettext("Manage Access…"),
        Box::new(move || open_share_dialog(&ui_c, &entry_c)),
    ));
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    items.push((
        gettext("Stop Sharing…"),
        Box::new(move || prompt_stop_sharing(&ui_c, &entry_c)),
    ));
    let (labels, actions): (Vec<String>, Vec<MenuAction>) = items.into_iter().unzip();
    more_menu_button(labels.iter().map(String::as_str).zip(actions).collect())
}

/// Confirm, then remove every member, invitation and the public link.
pub(crate) fn prompt_stop_sharing(ui: &Rc<Ui>, entry: &DirEntry) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Stop sharing?"))
        // Translators: {name} is the shared item's name.
        .body(gettext_f(
            "Everyone you invited loses access to “{name}”, pending invitations are withdrawn and its public link stops working.",
            &[("name", &entry.name)],
        ))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("stop", &gettext("Stop Sharing"));
    dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui_c = ui.clone();
    let uid = entry.uid.clone();
    let name = entry.name.clone();
    dialog.connect_response(Some("stop"), move |_, _| {
        run_mutation(
            &ui_c,
            Request::StopSharingByUid { uid: uid.clone() },
            // Translators: {name} is the item's name.
            gettext_f("Stopped sharing {name}", &[("name", &name)]),
            gettext_noop("Couldn't stop sharing"),
        );
    });
    dialog.present(ui_window(ui).as_ref());
}

/// Open a shared item: a folder in My Files, a file the way My Files would.
fn open_shared_by_me(ui: &Rc<Ui>, entry: &DirEntry) {
    if entry.is_dir {
        ui.stack.set_visible_child_name("browser");
        ui.browser.search.set_text("");
        browse_to(ui, entry.path.clone());
    } else {
        activate_entry(ui, entry);
    }
}

/// Go to the folder holding a shared item in My Files.
pub(crate) fn show_in_my_files(ui: &Rc<Ui>, entry: &DirEntry) {
    let parent = entry
        .path
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default();
    ui.stack.set_visible_child_name("browser");
    ui.browser.search.set_text("");
    browse_to(ui, parent);
}

/// A one-line summary of who can reach a shared item, for its row subtitle.
pub(crate) fn shared_item_summary(item: &SharedItem) -> String {
    let mut parts = Vec::new();
    if item.member_count > 0 {
        // Translators: how many people have access to a shared item.
        parts.push(ngettext_f(
            "{n} person",
            "{n} people",
            item.member_count as u64,
            &[],
        ));
    }
    if item.invite_count > 0 {
        // Translators: how many invitations to a shared item are not accepted yet.
        parts.push(ngettext_f(
            "{n} pending",
            "{n} pending",
            item.invite_count as u64,
            &[],
        ));
    }
    if item.link.is_some() {
        parts.push(gettext("Public link"));
    }
    if parts.is_empty() {
        // Translators: a shared item's summary when no people or link can be counted.
        pgettext("state", "Shared")
    } else {
        parts.join(" · ")
    }
}

/// Build a [`DirEntry`] from a [`SharedItem`] so the path-keyed Share dialog can
/// open on it. Only used when the item's path is known.
pub(crate) fn shared_item_as_entry(item: &SharedItem) -> DirEntry {
    DirEntry {
        name: item.name.clone(),
        is_dir: item.is_dir,
        size: 0,
        modified: item.modified,
        pinned: false,
        cached: false,
        uid: item.uid.clone(),
        path: item.path.clone(),
        // Shared *by* me: I own it, so there is no role of mine to report.
        role: String::new(),
        shared_by: String::new(),
        shared_at: 0,
        shared_by_unverified: false,
    }
}
