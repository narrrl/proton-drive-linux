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
/// each row summarizing who has access and carrying its public link (copy/open)
/// and a Manage shortcut into the per-node Share dialog.
pub(crate) fn build_shared_by_me_page() -> (gtk4::Widget, SharedByMeWidgets) {
    let title = gtk4::Label::builder()
        .label("Shared")
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .build();
    title.add_css_class("title-2");
    let refresh = refresh_button();
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.set_hexpand(true);
    header.append(&title);
    header.append(&refresh);

    let group = adw::PreferencesGroup::new();
    let clamp = adw::Clamp::builder().child(&group).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&clamp)
        .build();

    let retry = gtk4::Button::builder()
        .label("Retry")
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
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&content);

    (
        inner.upcast(),
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
    ui.shared_by_me.inflight.set(true);
    shared_by_me_status(
        ui,
        "emblem-shared-symbolic",
        "Loading…",
        "Reading what you've shared.",
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
                "Couldn't read your shares",
                &message,
                false,
            ),
            Ok(Ok(_)) => shared_by_me_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read your shares",
                "Unexpected reply from the mount service.",
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
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    shared_by_me_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
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
            "Nothing shared yet",
            "Items you share with people or by link show up here.",
            false,
        );
        return;
    }
    ui.shared_by_me.content.set_visible_child_name("list");
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for item in items {
        let row = adw::ActionRow::builder()
            .title(&item.name)
            .subtitle(shared_item_summary(item))
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name(if item.is_dir {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        }));

        // Copy / open the public link, when there is one with a recovered URL.
        if let Some(url) = item.link.as_ref().and_then(|l| l.url.clone()) {
            let copy = gtk4::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy link")
                .valign(gtk4::Align::Center)
                .build();
            copy.add_css_class("flat");
            let ui_copy = ui.clone();
            let url_copy = url.clone();
            copy.connect_clicked(move |btn| {
                btn.clipboard().set_text(&url_copy);
                toast(&ui_copy, "Link copied");
            });
            let open = gtk4::Button::builder()
                .icon_name("external-link-symbolic")
                .tooltip_text("Open link")
                .valign(gtk4::Align::Center)
                .build();
            open.add_css_class("flat");
            let url_open = url.clone();
            open.connect_clicked(move |_| open_uri(&url_open));
            row.add_suffix(&copy);
            row.add_suffix(&open);
        }

        // Manage opens the per-node Share dialog — only when the node's path is
        // known (it has been browsed to this session); the dialog is path-keyed.
        if !item.path.is_empty() {
            let manage = gtk4::Button::builder()
                .label("Manage")
                .valign(gtk4::Align::Center)
                .build();
            manage.add_css_class("flat");
            let ui_manage = ui.clone();
            let entry = shared_item_as_entry(item);
            manage.connect_clicked(move |_| open_share_dialog(&ui_manage, &entry));
            row.add_suffix(&manage);
        }

        ui.shared_by_me.group.add(&row);
        rows.push(row.upcast());
    }
    *ui.shared_by_me.rows.borrow_mut() = rows;
}

/// A one-line summary of who can reach a shared item, for its row subtitle.
pub(crate) fn shared_item_summary(item: &SharedItem) -> String {
    let mut parts = Vec::new();
    if item.member_count > 0 {
        parts.push(format!(
            "{} {}",
            item.member_count,
            if item.member_count == 1 {
                "person"
            } else {
                "people"
            }
        ));
    }
    if item.invite_count > 0 {
        parts.push(format!("{} pending", item.invite_count));
    }
    if item.link.is_some() {
        parts.push("Public link".to_string());
    }
    if parts.is_empty() {
        "Shared".to_string()
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
        modified: 0,
        pinned: false,
        cached: false,
        uid: item.uid.clone(),
        path: item.path.clone(),
    }
}
