use crate::*;

pub(crate) struct SharedState {
    // Shared page. Three sections rebuilt wholesale on each load: shared-with-me,
    // incoming invitations, and bookmarks. Rows are tracked so a reload can remove
    // the old ones before appending the new (adw groups have no clear-all).
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) with_me_group: adw::PreferencesGroup,
    pub(crate) invitations_group: adw::PreferencesGroup,
    pub(crate) bookmarks_group: adw::PreferencesGroup,
    /// Where in a shared folder the page currently is, as `(uid, name)` from the
    /// top level down. Empty = the top level. Shared subtrees have no path in the
    /// mount, so descending is uid-addressed and the stack *is* the breadcrumb.
    pub(crate) nav: RefCell<Vec<(String, String)>>,
    pub(crate) rows: RefCell<Vec<(adw::PreferencesGroup, gtk4::Widget)>>,
    /// Guards the Shared page's load so overlapping navigations don't stack.
    pub(crate) inflight: Cell<bool>,
    /// When the Shared page last painted good data. `None` = never / invalidated,
    /// forcing a fetch on next visit. See [`PAGE_TTL`].
    pub(crate) loaded_at: Cell<Option<Instant>>,
}

/// Widgets the Shared page's load/repaint touch.
pub(crate) struct SharedWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) shared_with_me: adw::PreferencesGroup,
    pub(crate) invitations: adw::PreferencesGroup,
    pub(crate) bookmarks: adw::PreferencesGroup,
    pub(crate) retry: gtk4::Button,
    pub(crate) add_bookmark: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
}

/// The Shared page: three stacked sections — items shared *with* me (each with a
/// Leave action), invitations addressed to me pending accept/reject, and public
/// links I've bookmarked (open / remove). All three live outside the mount tree,
/// so the page addresses them by uid/id/token and always re-lists from the daemon.
pub(crate) fn build_shared_page() -> (gtk4::Widget, SharedWidgets) {
    let title = gtk4::Label::builder()
        .label("Shared with me")
        .halign(gtk4::Align::Start)
        .build();
    title.add_css_class("title-2");
    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title);

    let add_bookmark = gtk4::Button::builder()
        .label("Add Bookmark")
        .valign(gtk4::Align::Center)
        .build();
    add_bookmark.add_css_class("flat");
    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&titles);
    header.append(&refresh);
    header.append(&add_bookmark);

    let shared_with_me = adw::PreferencesGroup::builder()
        .title("Shared with me")
        .build();
    let invitations = adw::PreferencesGroup::builder()
        .title("Invitations")
        .build();
    let bookmarks = adw::PreferencesGroup::builder().title("Bookmarks").build();

    let groups = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    groups.append(&shared_with_me);
    groups.append(&invitations);
    groups.append(&bookmarks);
    let clamp = adw::Clamp::builder().child(&groups).build();
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
        SharedWidgets {
            content,
            status,
            shared_with_me,
            invitations,
            bookmarks,
            retry,
            add_bookmark,
            refresh,
        },
    )
}

/// Install the Shared page's retry and Add-Bookmark buttons.
pub(crate) fn wire_shared(ui: &Rc<Ui>, retry: &gtk4::Button, add_bookmark: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_shared(&ui_retry);
    });
    let ui_add = ui.clone();
    add_bookmark.connect_clicked(move |_| prompt_add_bookmark(&ui_add));
}

/// Show a status page in place of the Shared sections.
pub(crate) fn shared_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.shared.status.set_icon_name(Some(icon));
    ui.shared.status.set_title(title);
    ui.shared.status.set_description(Some(description));
    ui.shared.retry.set_visible(retry);
    ui.shared.content.set_visible_child_name("status");
}

/// Fetch the three Shared sections (shared-with-me, invitations, bookmarks) in
/// parallel and repaint the page once all three land.
///
/// Inside a shared folder ([`SharedState::nav`] non-empty) only that folder's
/// children are fetched: invitations and bookmarks belong to the top level, and
/// carrying them down a subtree would read as if they lived there.
pub(crate) fn load_shared(ui: &Rc<Ui>) {
    if ui.shared.inflight.get() {
        return;
    }
    let current = ui.shared.nav.borrow().last().cloned();
    if let Some((uid, _)) = current {
        load_shared_folder(ui, uid);
        return;
    }
    ui.shared.inflight.set(true);
    shared_status(
        ui,
        "emblem-shared-symbolic",
        "Loading…",
        "Reading your shared items.",
        false,
    );

    ui.busy_begin();
    let socket = ui.dirs.control_socket();
    let shared_rx = spawn_request(socket.clone(), Request::ListSharedWithMe);
    let invites_rx = spawn_request(socket.clone(), Request::ListInvitations);
    let bookmarks_rx = spawn_request(socket, Request::ListBookmarks);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let shared = shared_rx.recv().await;
        let invites = invites_rx.recv().await;
        let bookmarks = bookmarks_rx.recv().await;
        ui.busy_end();
        ui.shared.inflight.set(false);

        // A transport failure on any of the three means the daemon isn't up.
        if matches!(shared, Ok(Err(_)) | Err(_))
            || matches!(invites, Ok(Err(_)) | Err(_))
            || matches!(bookmarks, Ok(Err(_)) | Err(_))
        {
            ui.shared.loaded_at.set(None);
            shared_unreachable(&ui);
            return;
        }

        let shared_items = match shared {
            Ok(Ok(Response::Entries { entries })) => entries,
            _ => Vec::new(),
        };
        let invitations = match invites {
            Ok(Ok(Response::Invitations { items })) => items,
            _ => Vec::new(),
        };
        let bookmark_items = match bookmarks {
            Ok(Ok(Response::Bookmarks { items })) => items,
            _ => Vec::new(),
        };
        repaint_shared(&ui, &shared_items, &invitations, &bookmark_items);
        ui.shared.loaded_at.set(Some(Instant::now()));
    });
}

/// List one shared folder's children and repaint the page as that folder's view.
/// The uid comes from the row that was activated (or from the nav stack on a
/// reload) — a shared subtree is reachable no other way.
fn load_shared_folder(ui: &Rc<Ui>, uid: String) {
    ui.shared.inflight.set(true);
    shared_status(
        ui,
        "folder-symbolic",
        "Loading…",
        "Reading this shared folder.",
        false,
    );
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSharedFolder { uid });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.shared.inflight.set(false);
        match result {
            Ok(Ok(Response::Entries { entries })) => {
                repaint_shared_folder(&ui, &entries);
                ui.shared.loaded_at.set(Some(Instant::now()));
            }
            Ok(Ok(Response::Error { message, kind })) => {
                // The folder is gone or access was revoked: fall back to the top
                // level rather than stranding the page on a dead uid.
                ui.shared.nav.borrow_mut().pop();
                toast_failure(&ui, "Couldn't open shared folder", &message, kind);
                load_shared(&ui);
            }
            _ => {
                ui.shared.loaded_at.set(None);
                shared_unreachable(&ui);
            }
        }
    });
}

/// Paint the contents of the shared folder at the top of the nav stack: a row to
/// go back up, then the children. Invitations and bookmarks are hidden here —
/// they are top-level things, not contents of this folder.
fn repaint_shared_folder(ui: &Rc<Ui>, entries: &[DirEntry]) {
    for (group, row) in ui.shared.rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    ui.shared.content.set_visible_child_name("list");
    ui.shared.invitations_group.set_visible(false);
    ui.shared.bookmarks_group.set_visible(false);

    let nav = ui.shared.nav.borrow().clone();
    let title = nav
        .iter()
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(" / ");
    ui.shared.with_me_group.set_title(&title);

    let mut rows: Vec<(adw::PreferencesGroup, gtk4::Widget)> = Vec::new();
    let up = adw::ActionRow::builder()
        .title("Back")
        .activatable(true)
        .build();
    up.add_prefix(&gtk4::Image::from_icon_name("go-up-symbolic"));
    let ui_up = ui.clone();
    up.connect_activated(move |_| {
        ui_up.shared.nav.borrow_mut().pop();
        load_shared(&ui_up);
    });
    ui.shared.with_me_group.add(&up);
    rows.push((ui.shared.with_me_group.clone(), up.upcast()));

    if entries.is_empty() {
        let row = dim_row("This folder is empty.");
        ui.shared.with_me_group.add(&row);
        rows.push((ui.shared.with_me_group.clone(), row.upcast()));
    } else {
        for entry in entries {
            let row = shared_entry_row(ui, entry);
            ui.shared.with_me_group.add(&row);
            rows.push((ui.shared.with_me_group.clone(), row.upcast()));
        }
    }
    *ui.shared.rows.borrow_mut() = rows;
}

/// A row for one node shared with me: folders descend into, files download and
/// open with the user's default application.
fn shared_entry_row(ui: &Rc<Ui>, entry: &DirEntry) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&entry.name)
        .activatable(true)
        .build();
    if !entry.is_dir {
        row.set_subtitle(&human_bytes(entry.size));
    }
    row.add_prefix(&gtk4::Image::from_icon_name(if entry.is_dir {
        "folder-symbolic"
    } else {
        "text-x-generic-symbolic"
    }));
    let ui_act = ui.clone();
    let uid = entry.uid.clone();
    let name = entry.name.clone();
    let is_dir = entry.is_dir;
    row.connect_activated(move |_| {
        if is_dir {
            ui_act
                .shared
                .nav
                .borrow_mut()
                .push((uid.clone(), name.clone()));
            load_shared(&ui_act);
        } else {
            open_shared_file(&ui_act, &uid, &name);
        }
    });
    row
}

/// Download a file shared with me into the daemon's cache and hand it to the
/// user's default application — the shared-item twin of the browser's
/// download-and-open, addressed by uid because the file lives outside the mount.
fn open_shared_file(ui: &Rc<Ui>, uid: &str, name: &str) {
    // Ignore a repeat activation of a file already downloading, so an impatient
    // double-click doesn't kick off a second round-trip.
    if !ui.opening.borrow_mut().insert(uid.to_string()) {
        return;
    }
    ui.busy_begin();
    toast(ui, &format!("Downloading “{name}”…"));
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenSharedFile {
            uid: uid.to_string(),
        },
    );
    let ui = ui.clone();
    let uid = uid.to_string();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.opening.borrow_mut().remove(&uid);
        match result {
            Ok(Ok(Response::FilePath { path })) => open_path(&path),
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

/// The daemon didn't answer the Shared page. Same still-starting vs. down split
/// as the other pages.
pub(crate) fn shared_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        shared_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    shared_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("shared") {
            load_shared(&ui);
        }
    });
}

/// Rebuild the three Shared sections. Rows added last time are removed first —
/// adw groups have no clear-all — then each section is repopulated, showing a
/// dim placeholder row when empty.
pub(crate) fn repaint_shared(
    ui: &Rc<Ui>,
    shared: &[DirEntry],
    invitations: &[InvitationInfo],
    bookmarks: &[BookmarkInfo],
) {
    for (group, row) in ui.shared.rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    ui.shared.content.set_visible_child_name("list");
    ui.shared.invitations_group.set_visible(true);
    ui.shared.bookmarks_group.set_visible(true);
    ui.shared.with_me_group.set_title("Shared with me");
    let mut rows: Vec<(adw::PreferencesGroup, gtk4::Widget)> = Vec::new();

    // Shared with me: name + Leave.
    if shared.is_empty() {
        let row = dim_row("Nothing is shared with you.");
        ui.shared.with_me_group.add(&row);
        rows.push((ui.shared.with_me_group.clone(), row.upcast()));
    } else {
        for entry in shared {
            let row = shared_entry_row(ui, entry);
            let leave = gtk4::Button::builder()
                .label("Leave")
                .valign(gtk4::Align::Center)
                .build();
            leave.add_css_class("flat");
            let ui_leave = ui.clone();
            let uid = entry.uid.clone();
            let name = entry.name.clone();
            leave.connect_clicked(move |_| prompt_leave_shared(&ui_leave, &uid, &name));
            row.add_suffix(&leave);
            ui.shared.with_me_group.add(&row);
            rows.push((ui.shared.with_me_group.clone(), row.upcast()));
        }
    }

    // Invitations: inviter + item, Accept / Reject.
    if invitations.is_empty() {
        let row = dim_row("No pending invitations.");
        ui.shared.invitations_group.add(&row);
        rows.push((ui.shared.invitations_group.clone(), row.upcast()));
    } else {
        for inv in invitations {
            let item = inv
                .name
                .clone()
                .unwrap_or_else(|| "a shared item".to_string());
            let row = adw::ActionRow::builder()
                .title(&item)
                .subtitle(format!("from {}", inv.inviter_email))
                .build();
            row.add_prefix(&gtk4::Image::from_icon_name(if inv.is_dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));
            let reject = gtk4::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text("Reject")
                .valign(gtk4::Align::Center)
                .build();
            reject.add_css_class("flat");
            let accept = gtk4::Button::builder()
                .label("Accept")
                .valign(gtk4::Align::Center)
                .build();
            accept.add_css_class("suggested-action");
            let ui_acc = ui.clone();
            let id_acc = inv.id.clone();
            accept.connect_clicked(move |_| {
                respond_invitation(&ui_acc, &id_acc, true);
            });
            let ui_rej = ui.clone();
            let id_rej = inv.id.clone();
            reject.connect_clicked(move |_| {
                respond_invitation(&ui_rej, &id_rej, false);
            });
            row.add_suffix(&accept);
            row.add_suffix(&reject);
            ui.shared.invitations_group.add(&row);
            rows.push((ui.shared.invitations_group.clone(), row.upcast()));
        }
    }

    // Bookmarks: name/URL, Open / Remove.
    if bookmarks.is_empty() {
        let row = dim_row("No saved bookmarks.");
        ui.shared.bookmarks_group.add(&row);
        rows.push((ui.shared.bookmarks_group.clone(), row.upcast()));
    } else {
        for bm in bookmarks {
            let title = bm.name.clone().unwrap_or_else(|| "Shared link".to_string());
            let row = adw::ActionRow::builder()
                .title(&title)
                .subtitle(&bm.url)
                .build();
            row.add_prefix(&gtk4::Image::from_icon_name("emblem-symbolic-link"));
            let remove = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove bookmark")
                .valign(gtk4::Align::Center)
                .build();
            remove.add_css_class("flat");
            let open = gtk4::Button::builder()
                .icon_name("external-link-symbolic")
                .tooltip_text("Open in browser")
                .valign(gtk4::Align::Center)
                .build();
            open.add_css_class("flat");
            let url_open = bm.url.clone();
            open.connect_clicked(move |_| open_uri(&url_open));
            let ui_rm = ui.clone();
            let token = bm.token.clone();
            let name_rm = title.clone();
            remove.connect_clicked(move |_| prompt_remove_bookmark(&ui_rm, &token, &name_rm));
            row.add_suffix(&open);
            row.add_suffix(&remove);
            ui.shared.bookmarks_group.add(&row);
            rows.push((ui.shared.bookmarks_group.clone(), row.upcast()));
        }
    }

    *ui.shared.rows.borrow_mut() = rows;
}

/// Accept or reject an invitation, then reload the Shared page.
pub(crate) fn respond_invitation(ui: &Rc<Ui>, id: &str, accept: bool) {
    let req = if accept {
        Request::AcceptInvitation { id: id.to_string() }
    } else {
        Request::RejectInvitation { id: id.to_string() }
    };
    let (done, failed) = if accept {
        ("Invitation accepted", "Couldn't accept the invitation")
    } else {
        ("Invitation rejected", "Couldn't reject the invitation")
    };
    run_shared_mutation(ui, req, done, failed);
}

/// Confirm, then leave a node shared with me.
pub(crate) fn prompt_leave_shared(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Leave shared item")
        .body(format!(
            "Leave “{name}”? You'll lose access until you're invited again."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("leave", "Leave");
    dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp == "leave" {
            run_shared_mutation(
                &ui,
                Request::LeaveShared { uid: uid.clone() },
                "Left shared item",
                "Couldn't leave the shared item",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Confirm, then remove a saved bookmark.
pub(crate) fn prompt_remove_bookmark(ui: &Rc<Ui>, token: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Remove bookmark")
        .body(format!("Remove the bookmark for “{name}”?"))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Remove");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let token = token.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp == "remove" {
            run_shared_mutation(
                &ui,
                Request::DeleteBookmark {
                    token: token.clone(),
                },
                "Bookmark removed",
                "Couldn't remove the bookmark",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Prompt for a public-link URL (and optional password) and save it as a bookmark.
pub(crate) fn prompt_add_bookmark(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Add bookmark")
        .body("Paste a Proton Drive public link to save it here.")
        .build();
    let group = adw::PreferencesGroup::new();
    let url_row = adw::EntryRow::builder()
        .title("Public link URL")
        .activates_default(true)
        .build();
    let pw_row = adw::PasswordEntryRow::builder()
        .title("Password (if the link has one)")
        .activates_default(true)
        .build();
    group.add(&url_row);
    group.add(&pw_row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Add");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "add" {
            return;
        }
        let url = url_row.text().trim().to_string();
        if url.is_empty() {
            toast_error(&ui, "Couldn't add bookmark", "A URL is required.");
            return;
        }
        let pw = pw_row.text().to_string();
        let password = if pw.is_empty() { None } else { Some(pw) };
        run_shared_mutation(
            &ui,
            Request::CreateBookmark { url, password },
            "Bookmark saved",
            "Couldn't add the bookmark",
        );
    });
    dialog.present(win.as_ref());
}

/// Run a mutation raised from the Shared page and reload the page on success.
pub(crate) fn run_shared_mutation(
    ui: &Rc<Ui>,
    req: Request,
    done: &'static str,
    failed: &'static str,
) {
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    let done = done.to_string();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => {
                load_shared(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, failed, &message, kind),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}
