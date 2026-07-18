use crate::*;

pub(crate) struct ActivityState {
    // Activity page: one section, newest-first, rebuilt wholesale on each load.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) rows: RefCell<Vec<gtk4::Widget>>,
    pub(crate) inflight: Cell<bool>,
    /// Fingerprint of the feed currently on screen (see [`activity_key`]). The
    /// page polls every couple of seconds and usually gets back exactly what it
    /// is already showing; rebuilding all 200 rows for that would throw away the
    /// user's scroll position several times a minute.
    pub(crate) key: RefCell<Option<String>>,
}

/// Widgets the Activity page's load/repaint touch.
pub(crate) struct ActivityWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) retry: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
}

/// The Activity page: a newest-first feed of the mutations and transfers the
/// daemon performed this session (uploads, deletes, shares, …).
pub(crate) fn build_activity_page() -> (gtk4::Widget, ActivityWidgets) {
    let title = gtk4::Label::builder()
        .label("Activity")
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
        .icon_name("document-open-recent-symbolic")
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
        ActivityWidgets {
            content,
            status,
            group,
            retry,
            refresh,
        },
    )
}

/// Install the Activity page's retry button.
pub(crate) fn wire_activity(ui: &Rc<Ui>, retry: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_activity(&ui_retry);
    });
}

/// Show a status page in place of the Activity list.
pub(crate) fn activity_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.activity.status.set_icon_name(Some(icon));
    ui.activity.status.set_title(title);
    ui.activity.status.set_description(Some(description));
    ui.activity.retry.set_visible(retry);
    ui.activity.content.set_visible_child_name("status");
    // The list is no longer what is on screen, so the next repaint must not skip
    // itself as a no-op and leave this status view up.
    *ui.activity.key.borrow_mut() = None;
}

/// Refresh the Activity feed in place, with no status flash and no spinner.
/// Driven by the periodic tick while the page is on screen, so a running sync
/// pass fills the feed as it works rather than only once it is done. Anything
/// other than a good answer leaves the rows alone until the next tick.
pub(crate) fn refresh_activity(ui: &Rc<Ui>) {
    if ui.activity.inflight.get() {
        return;
    }
    ui.activity.inflight.set(true);
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListActivity { limit: 200 },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.activity.inflight.set(false);
        // The page may have been navigated away from while the request was in
        // flight.
        if let Ok(Ok(Response::Activity { items })) = result
            && ui.stack.visible_child_name().as_deref() == Some("activity")
        {
            repaint_activity(&ui, &items);
        }
    });
}

pub(crate) fn load_activity(ui: &Rc<Ui>) {
    if ui.activity.inflight.get() {
        return;
    }
    ui.activity.inflight.set(true);
    activity_status(
        ui,
        "document-open-recent-symbolic",
        "Loading…",
        "Reading recent activity.",
        false,
    );
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListActivity { limit: 200 },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.activity.inflight.set(false);
        match result {
            Ok(Ok(Response::Activity { items })) => repaint_activity(&ui, &items),
            Ok(Ok(Response::Error { message, .. })) => activity_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read activity",
                &message,
                false,
            ),
            Ok(Ok(_)) => activity_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read activity",
                "Unexpected reply from the mount service.",
                false,
            ),
            Ok(Err(_)) | Err(_) => activity_unreachable(&ui),
        }
    });
}

/// The daemon didn't answer the Activity page.
pub(crate) fn activity_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        activity_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    activity_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("activity") {
            load_activity(&ui);
        }
    });
}

/// A cheap fingerprint of a feed: how many entries, and what the newest one is.
/// The log is append-only and newest-first, so a feed with the same length and
/// the same newest entry is the same feed.
pub(crate) fn activity_key(items: &[ActivityEntry]) -> String {
    match items.first() {
        Some(a) => format!("{}:{}:{}:{}", items.len(), a.time, a.target, a.detail),
        None => String::new(),
    }
}

/// Rebuild the Activity section from a fresh log, unless it is already showing
/// exactly this feed.
pub(crate) fn repaint_activity(ui: &Rc<Ui>, items: &[ActivityEntry]) {
    let key = activity_key(items);
    if ui.activity.key.borrow().as_deref() == Some(key.as_str()) {
        return;
    }
    *ui.activity.key.borrow_mut() = Some(key);
    for row in ui.activity.rows.borrow_mut().drain(..) {
        ui.activity.group.remove(&row);
    }
    if items.is_empty() {
        activity_status(
            ui,
            "document-open-recent-symbolic",
            "Nothing yet",
            "Uploads, moves, shares and other changes appear here as they happen.",
            false,
        );
        return;
    }
    ui.activity.content.set_visible_child_name("list");
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for a in items {
        let mut subtitle = activity_time(a.time);
        if !a.detail.is_empty() {
            subtitle = format!("{subtitle} · {}", a.detail);
        }
        let row = adw::ActionRow::builder()
            .title(format!("{} {}", activity_verb(a.kind), a.target))
            .subtitle(subtitle)
            .build();
        let icon = if a.ok {
            activity_icon(a.kind)
        } else {
            "dialog-warning-symbolic"
        };
        row.add_prefix(&gtk4::Image::from_icon_name(icon));
        row.set_activatable(false);
        ui.activity.group.add(&row);
        rows.push(row.upcast());
    }
    *ui.activity.rows.borrow_mut() = rows;
}

/// A human verb for an activity kind, used as the row title's lead word.
pub(crate) fn activity_verb(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Upload => "Uploaded",
        ActivityKind::Download => "Downloaded",
        ActivityKind::Sync => "Synced",
        ActivityKind::Rename => "Renamed",
        ActivityKind::Move => "Moved",
        ActivityKind::CreateFolder => "Created folder",
        ActivityKind::Trash => "Trashed",
        ActivityKind::Restore => "Restored",
        ActivityKind::DeleteForever => "Deleted",
        ActivityKind::EmptyTrash => "Emptied trash —",
        ActivityKind::Share => "Shared",
        ActivityKind::PublicLink => "Linked",
        ActivityKind::Unshare => "Unshared",
    }
}

/// A themed icon for an activity kind.
pub(crate) fn activity_icon(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Upload => "document-send-symbolic",
        ActivityKind::Download => "document-save-symbolic",
        ActivityKind::Sync => "emblem-synchronizing-symbolic",
        ActivityKind::Rename => "document-edit-symbolic",
        ActivityKind::Move => "go-jump-symbolic",
        ActivityKind::CreateFolder => "folder-new-symbolic",
        ActivityKind::Trash | ActivityKind::DeleteForever | ActivityKind::EmptyTrash => {
            "user-trash-symbolic"
        }
        ActivityKind::Restore => "edit-undo-symbolic",
        ActivityKind::Share | ActivityKind::PublicLink => "emblem-shared-symbolic",
        ActivityKind::Unshare => "action-unavailable-symbolic",
    }
}

/// Format an epoch-seconds timestamp for the Activity feed, in local time.
pub(crate) fn activity_time(secs: i64) -> String {
    glib::DateTime::from_unix_local(secs)
        .and_then(|dt| dt.format("%b %-d, %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}
