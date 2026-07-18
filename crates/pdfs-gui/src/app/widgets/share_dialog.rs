use crate::*;

/// The roles offered in the Share dialog's dropdowns, in index order.
pub(crate) const SHARE_ROLES: [&str; 3] = ["Viewer", "Editor", "Admin"];

/// Map a role dropdown index to the wire role string.
pub(crate) fn role_index_to_wire(idx: u32) -> &'static str {
    match idx {
        1 => "editor",
        2 => "admin",
        _ => "viewer",
    }
}

/// Map a wire role string to its dropdown index.
pub(crate) fn role_wire_to_index(role: &str) -> u32 {
    match role {
        "editor" => 1,
        "admin" => 2,
        _ => 0,
    }
}

/// The mutable state behind an open Share dialog, so the invite/role/link
/// handlers can rebuild the people and link sections after each change without
/// tearing the whole dialog down.
pub(crate) struct ShareDialog {
    pub(crate) ui: Rc<Ui>,
    /// The node's mountpoint-relative path — how every request addresses it.
    pub(crate) rel: String,
    pub(crate) people: adw::PreferencesGroup,
    pub(crate) link_group: adw::PreferencesGroup,
    pub(crate) people_rows: RefCell<Vec<gtk4::Widget>>,
    pub(crate) link_rows: RefCell<Vec<gtk4::Widget>>,
}

/// Open the per-node Share dialog: invite Proton/external users, manage who has
/// access and their roles, and create/copy/remove a public link.
pub(crate) fn open_share_dialog(ui: &Rc<Ui>, entry: &DirEntry) {
    if !*ui.mounted.borrow() {
        toast_error(ui, "Can't share", "Proton Drive isn't connected.");
        return;
    }
    let rel = entry_rel(ui, entry);

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Share", &entry.name)));
    toolbar.add_top_bar(&header);

    // Invite section: emails + role + optional message.
    let invite_group = adw::PreferencesGroup::builder()
        .title("Invite people")
        .description("Proton and non-Proton email addresses, separated by spaces or commas.")
        .build();
    let email_row = adw::EntryRow::builder().title("Email addresses").build();
    let role_model = gtk4::StringList::new(&SHARE_ROLES);
    let role_drop = gtk4::DropDown::builder()
        .model(&role_model)
        .selected(0)
        .valign(gtk4::Align::Center)
        .build();
    let role_wrap = adw::ActionRow::builder().title("Role").build();
    role_wrap.add_suffix(&role_drop);
    let message_row = adw::EntryRow::builder().title("Message (optional)").build();
    let invite_btn = gtk4::Button::builder()
        .label("Send Invitations")
        .halign(gtk4::Align::End)
        .margin_top(6)
        .build();
    invite_btn.add_css_class("suggested-action");
    let invite_wrap = adw::PreferencesRow::builder()
        .activatable(false)
        .child(&invite_btn)
        .build();
    invite_group.add(&email_row);
    invite_group.add(&role_wrap);
    invite_group.add(&message_row);
    invite_group.add(&invite_wrap);

    let people = adw::PreferencesGroup::builder()
        .title("People with access")
        .build();
    let link_group = adw::PreferencesGroup::builder()
        .title("Public link")
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&invite_group);
    content.append(&people);
    content.append(&link_group);
    let clamp = adw::Clamp::builder().child(&content).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&clamp)
        .build();
    toolbar.set_content(Some(&scroll));

    let dialog = adw::Dialog::builder()
        .title("Share")
        .content_width(480)
        .content_height(560)
        .child(&toolbar)
        .build();

    let state = Rc::new(ShareDialog {
        ui: ui.clone(),
        rel,
        people,
        link_group,
        people_rows: RefCell::new(Vec::new()),
        link_rows: RefCell::new(Vec::new()),
    });

    // Enter in either free-text field sends the invitations, no mouse needed.
    let btn = invite_btn.clone();
    email_row.connect_entry_activated(move |_| btn.emit_clicked());
    let btn = invite_btn.clone();
    message_row.connect_entry_activated(move |_| btn.emit_clicked());

    // Invite button.
    let state_inv = state.clone();
    invite_btn.connect_clicked(move |_| {
        let raw = email_row.text();
        let emails: Vec<String> = raw
            .split([',', ' ', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if emails.is_empty() {
            toast_error(&state_inv.ui, "Couldn't share", "Enter at least one email.");
            return;
        }
        let role = role_index_to_wire(role_drop.selected()).to_string();
        let msg = message_row.text().trim().to_string();
        let message = if msg.is_empty() { None } else { Some(msg) };
        let email_clear = email_row.clone();
        let msg_clear = message_row.clone();
        share_dialog_op(
            &state_inv,
            Request::ShareNode {
                path: state_inv.rel.clone(),
                emails,
                role,
                message,
            },
            "Invitations sent",
            "Couldn't send invitations",
            Some(Box::new(move || {
                email_clear.set_text("");
                msg_clear.set_text("");
            })),
        );
    });

    share_dialog_reload(&state);
    dialog.present(ui_window(ui).as_ref());
}

/// Re-fetch the node's share and rebuild the people + public-link sections.
pub(crate) fn share_dialog_reload(state: &Rc<ShareDialog>) {
    let rx = spawn_request(
        state.ui.dirs.control_socket(),
        Request::ListShare {
            path: state.rel.clone(),
        },
    );
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Share { entries, link })) => {
                repaint_share_people(&state, &entries);
                repaint_share_link(&state, link.as_ref());
            }
            Ok(Ok(Response::Error { message, .. })) => {
                toast_error(&state.ui, "Couldn't load sharing", &message)
            }
            _ => toast_error(
                &state.ui,
                "Couldn't load sharing",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Rebuild the "People with access" rows from a fresh share listing.
pub(crate) fn repaint_share_people(state: &Rc<ShareDialog>, entries: &[ShareEntry]) {
    for row in state.people_rows.borrow_mut().drain(..) {
        state.people.remove(&row);
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    if entries.is_empty() {
        let row = dim_row("No one else has access yet.");
        state.people.add(&row);
        rows.push(row.upcast());
        *state.people_rows.borrow_mut() = rows;
        return;
    }
    for entry in entries {
        let subtitle = match entry.kind {
            ShareEntryKind::Member => "Member".to_string(),
            ShareEntryKind::ProtonInvite => "Invited (pending)".to_string(),
            ShareEntryKind::ExternalInvite => "Invited (external, pending)".to_string(),
        };
        let row = adw::ActionRow::builder()
            .title(&entry.email)
            .subtitle(&subtitle)
            .build();

        // External invites can't have their role changed; show it read-only.
        // Members and Proton invites get a role dropdown.
        if matches!(
            entry.kind,
            ShareEntryKind::Member | ShareEntryKind::ProtonInvite
        ) {
            let model = gtk4::StringList::new(&SHARE_ROLES);
            let drop = gtk4::DropDown::builder()
                .model(&model)
                .selected(role_wire_to_index(&entry.role))
                .valign(gtk4::Align::Center)
                .build();
            let state_role = state.clone();
            let id = entry.id.clone();
            let kind = entry.kind;
            drop.connect_selected_notify(move |d| {
                share_dialog_op(
                    &state_role,
                    Request::UpdateShareRole {
                        path: state_role.rel.clone(),
                        id: id.clone(),
                        kind,
                        role: role_index_to_wire(d.selected()).to_string(),
                    },
                    "Role updated",
                    "Couldn't update the role",
                    None,
                );
            });
            row.add_suffix(&drop);
        } else {
            let label = gtk4::Label::builder()
                .label(capitalize(&entry.role))
                .valign(gtk4::Align::Center)
                .build();
            label.add_css_class("dim-label");
            row.add_suffix(&label);
        }

        let remove = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove access")
            .valign(gtk4::Align::Center)
            .build();
        remove.add_css_class("flat");
        let state_rm = state.clone();
        let id = entry.id.clone();
        let kind = entry.kind;
        remove.connect_clicked(move |_| {
            share_dialog_op(
                &state_rm,
                Request::RemoveShareEntry {
                    path: state_rm.rel.clone(),
                    id: id.clone(),
                    kind,
                },
                "Access removed",
                "Couldn't remove access",
                None,
            );
        });
        row.add_suffix(&remove);

        state.people.add(&row);
        rows.push(row.upcast());
    }
    *state.people_rows.borrow_mut() = rows;
}

/// Rebuild the public-link section: an existing link (copy / remove) or a
/// create control.
pub(crate) fn repaint_share_link(state: &Rc<ShareDialog>, link: Option<&PublicLinkInfo>) {
    for row in state.link_rows.borrow_mut().drain(..) {
        state.link_group.remove(&row);
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();

    match link {
        Some(link) => {
            let url = link.url.clone().unwrap_or_default();
            let subtitle = if link.has_password {
                format!("Anyone with the link ({}) · password-protected", link.role)
            } else {
                format!("Anyone with the link ({})", link.role)
            };
            let row = adw::ActionRow::builder()
                .title(if url.is_empty() { "Public link" } else { &url })
                .subtitle(&subtitle)
                .build();
            row.add_css_class("property");

            if !url.is_empty() {
                let copy = gtk4::Button::builder()
                    .icon_name("edit-copy-symbolic")
                    .tooltip_text("Copy link")
                    .valign(gtk4::Align::Center)
                    .build();
                copy.add_css_class("flat");
                let state_copy = state.clone();
                let url_copy = url.clone();
                copy.connect_clicked(move |btn| {
                    btn.clipboard().set_text(&url_copy);
                    toast(&state_copy.ui, "Link copied");
                });
                row.add_suffix(&copy);
            }

            let remove = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove link")
                .valign(gtk4::Align::Center)
                .build();
            remove.add_css_class("flat");
            let state_rm = state.clone();
            let id = link.id.clone();
            remove.connect_clicked(move |_| {
                share_dialog_op(
                    &state_rm,
                    Request::RemovePublicLink {
                        path: state_rm.rel.clone(),
                        id: id.clone(),
                    },
                    "Public link removed",
                    "Couldn't remove the link",
                    None,
                );
            });
            row.add_suffix(&remove);

            state.link_group.add(&row);
            rows.push(row.upcast());
        }
        None => {
            let role_model = gtk4::StringList::new(&["Viewer", "Editor"]);
            let role_drop = gtk4::DropDown::builder()
                .model(&role_model)
                .selected(0)
                .valign(gtk4::Align::Center)
                .build();
            let role_row = adw::ActionRow::builder().title("Link role").build();
            role_row.add_suffix(&role_drop);
            let pw_row = adw::PasswordEntryRow::builder()
                .title("Password (optional)")
                .build();
            let create = gtk4::Button::builder()
                .label("Create Public Link")
                .halign(gtk4::Align::End)
                .margin_top(6)
                .build();
            create.add_css_class("suggested-action");
            let create_wrap = adw::PreferencesRow::builder()
                .activatable(false)
                .child(&create)
                .build();

            // Enter in the password field creates the link.
            let btn = create.clone();
            pw_row.connect_entry_activated(move |_| btn.emit_clicked());

            let state_c = state.clone();
            let pw_for = pw_row.clone();
            create.connect_clicked(move |_| {
                let role = if role_drop.selected() == 1 {
                    "editor"
                } else {
                    "viewer"
                }
                .to_string();
                let pw = pw_for.text().to_string();
                let password = if pw.is_empty() { None } else { Some(pw) };
                share_dialog_create_link(&state_c, role, password);
            });

            state.link_group.add(&role_row);
            state.link_group.add(&pw_row);
            state.link_group.add(&create_wrap);
            rows.push(role_row.upcast());
            rows.push(pw_row.upcast());
            rows.push(create_wrap.upcast());
        }
    }
    *state.link_rows.borrow_mut() = rows;
}

/// Create a public link, then reload the dialog so the copy/remove controls
/// replace the create form (and the freshly minted URL is shown).
pub(crate) fn share_dialog_create_link(
    state: &Rc<ShareDialog>,
    role: String,
    password: Option<String>,
) {
    let rx = spawn_request(
        state.ui.dirs.control_socket(),
        Request::CreatePublicLink {
            path: state.rel.clone(),
            role,
            password,
            expires: None,
        },
    );
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::PublicLink { .. })) => {
                toast(&state.ui, "Public link created");
                share_dialog_reload(&state);
            }
            Ok(Ok(Response::Error { message, .. })) => {
                toast_error(&state.ui, "Couldn't create the link", &message)
            }
            _ => toast_error(
                &state.ui,
                "Couldn't create the link",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Run a Share-dialog mutation, then reload the dialog on success. `on_success`
/// runs an extra UI tweak (e.g. clearing the invite fields) before the reload.
pub(crate) fn share_dialog_op(
    state: &Rc<ShareDialog>,
    req: Request,
    done: &'static str,
    failed: &'static str,
    on_success: Option<Box<dyn Fn()>>,
) {
    let rx = spawn_request(state.ui.dirs.control_socket(), req);
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => {
                if let Some(cb) = on_success {
                    cb();
                }
                toast(&state.ui, done);
                share_dialog_reload(&state);
            }
            Ok(Ok(Response::Error { message, .. })) => toast_error(&state.ui, failed, &message),
            _ => {
                // A role dropdown that failed is now out of sync with the server;
                // reload to snap it back.
                toast_error(&state.ui, failed, "The mount service didn't respond.");
                share_dialog_reload(&state);
            }
        }
    });
}
