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

/// The expiry choices offered when creating a public link, in dropdown order,
/// with their lifetime in days (`None` never expires).
pub(crate) const LINK_EXPIRY: [(&str, Option<i64>); 4] = [
    ("Never", None),
    ("1 day", Some(1)),
    ("7 days", Some(7)),
    ("30 days", Some(30)),
];

/// The Unix expiry for a [`LINK_EXPIRY`] choice, counted from `now`.
pub(crate) fn link_expiry_at(idx: u32, now: i64) -> Option<i64> {
    LINK_EXPIRY
        .get(idx as usize)
        .and_then(|(_, days)| *days)
        .map(|days| now + days * 86_400)
}

/// A loose shape check for an email address: one `@` with something before
/// it and a dotted domain after it. The server does the real validation; this
/// only catches typos before the Invite button is pressed.
pub(crate) fn looks_like_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && !s.chars().any(char::is_whitespace)
        && domain
            .split_once('.')
            .is_some_and(|(host, rest)| !host.is_empty() && !rest.is_empty())
        && !domain.ends_with('.')
}

/// Split the invite field into addresses. Returns `Err` with the first entry
/// that doesn't look like an email, and `Ok` with an empty list when the field
/// is blank.
pub(crate) fn parse_emails(raw: &str) -> Result<Vec<String>, String> {
    raw.split([',', ' ', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if looks_like_email(s) {
                Ok(s.to_string())
            } else {
                Err(s.to_string())
            }
        })
        .collect()
}

/// A button showing a spinner while its request runs. It goes insensitive
/// on [`Busy::start`] and gets its label or icon back when dropped, so moving
/// the guard into the request's future ends the busy state on every path.
pub(crate) struct Busy {
    button: gtk4::Button,
    label: Option<glib::GString>,
    icon: Option<glib::GString>,
}

impl Busy {
    pub(crate) fn start(button: &gtk4::Button) -> Self {
        let label = button.label();
        let icon = button.icon_name();
        let spinner = gtk4::Spinner::builder().spinning(true).build();
        match &label {
            Some(text) => {
                let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
                content.append(&spinner);
                content.append(&gtk4::Label::new(Some(text)));
                button.set_child(Some(&content));
            }
            None => button.set_child(Some(&spinner)),
        }
        button.set_sensitive(false);
        Self {
            button: button.clone(),
            label,
            icon,
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(label) = &self.label {
            self.button.set_label(label);
        } else if let Some(icon) = &self.icon {
            self.button.set_icon_name(icon);
        }
        self.button.set_sensitive(true);
    }
}

/// How an open Share dialog addresses its node.
///
/// A node the browser is showing has a mountpoint-relative path. A node reached
/// from Shared or Shared-with-me may not: it can live under a device folder's
/// own inode space, or on someone else's volume, neither of which the primary
/// mount's path space can name. Those carry a uid instead, and the daemon
/// resolves it with `Core::resolve_anywhere` (mount-architecture.md P4).
///
/// The two are separate request variants rather than one request with an
/// optional field on purpose: an older daemon that ignored `uid` would resolve
/// the empty path to the *mount root* and act on the wrong node.
pub(crate) enum ShareTarget {
    Path(String),
    Uid(String),
}

impl ShareTarget {
    fn list(&self) -> Request {
        match self {
            ShareTarget::Path(path) => Request::ListShare { path: path.clone() },
            ShareTarget::Uid(uid) => Request::ListShareByUid { uid: uid.clone() },
        }
    }

    fn invite(&self, emails: Vec<String>, role: String, message: Option<String>) -> Request {
        match self {
            ShareTarget::Path(path) => Request::ShareNode {
                path: path.clone(),
                emails,
                role,
                message,
            },
            ShareTarget::Uid(uid) => Request::ShareNodeByUid {
                uid: uid.clone(),
                emails,
                role,
                message,
            },
        }
    }

    fn update_role(&self, id: String, kind: ShareEntryKind, role: String) -> Request {
        match self {
            ShareTarget::Path(path) => Request::UpdateShareRole {
                path: path.clone(),
                id,
                kind,
                role,
            },
            ShareTarget::Uid(uid) => Request::UpdateShareRoleByUid {
                uid: uid.clone(),
                id,
                kind,
                role,
            },
        }
    }

    fn remove_entry(&self, id: String, kind: ShareEntryKind) -> Request {
        match self {
            ShareTarget::Path(path) => Request::RemoveShareEntry {
                path: path.clone(),
                id,
                kind,
            },
            ShareTarget::Uid(uid) => Request::RemoveShareEntryByUid {
                uid: uid.clone(),
                id,
                kind,
            },
        }
    }

    fn create_link(&self, role: String, password: Option<String>, expires: Option<i64>) -> Request {
        match self {
            ShareTarget::Path(path) => Request::CreatePublicLink {
                path: path.clone(),
                role,
                password,
                expires,
            },
            ShareTarget::Uid(uid) => Request::CreatePublicLinkByUid {
                uid: uid.clone(),
                role,
                password,
                expires,
            },
        }
    }

    fn remove_link(&self, id: String) -> Request {
        match self {
            ShareTarget::Path(path) => Request::RemovePublicLink {
                path: path.clone(),
                id,
            },
            ShareTarget::Uid(uid) => Request::RemovePublicLinkByUid {
                uid: uid.clone(),
                id,
            },
        }
    }
}

/// The mutable state behind an open Share dialog, so the invite/role/link
/// handlers can rebuild the people and link sections after each change without
/// tearing the whole dialog down.
pub(crate) struct ShareDialog {
    pub(crate) ui: Rc<Ui>,
    /// How every request from this dialog addresses the node.
    pub(crate) target: ShareTarget,
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
    // A node with no path is not a bug here: Shared and Shared-with-me list
    // nodes that the primary mount never interned. Address those by uid.
    let target = if entry.path.is_empty() {
        ShareTarget::Uid(entry.uid.clone())
    } else {
        ShareTarget::Path(entry_rel(ui, entry))
    };

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
    invite_btn.set_sensitive(false);
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
        target,
        people,
        link_group,
        people_rows: RefCell::new(Vec::new()),
        link_rows: RefCell::new(Vec::new()),
    });

    // Check the addresses as they are typed: a malformed one marks the row as
    // an error and keeps the Invite button off, so a typo never reaches the
    // server as a failed invitation.
    let btn = invite_btn.clone();
    email_row.connect_changed(move |row| {
        let parsed = parse_emails(&row.text());
        match &parsed {
            Err(bad) => {
                row.add_css_class("error");
                row.set_tooltip_text(Some(&format!("“{bad}” isn't an email address")));
            }
            Ok(_) => {
                row.remove_css_class("error");
                row.set_tooltip_text(None);
            }
        }
        btn.set_sensitive(parsed.is_ok_and(|emails| !emails.is_empty()));
    });

    // Enter in either free-text field sends the invitations, no mouse needed.
    let btn = invite_btn.clone();
    email_row.connect_entry_activated(move |_| btn.emit_clicked());
    let btn = invite_btn.clone();
    message_row.connect_entry_activated(move |_| btn.emit_clicked());

    // Invite button.
    let state_inv = state.clone();
    invite_btn.connect_clicked(move |btn| {
        // Enter in a field reaches here even while the button is off.
        if !btn.is_sensitive() {
            return;
        }
        let Ok(emails) = parse_emails(&email_row.text()) else {
            return;
        };
        if emails.is_empty() {
            return;
        }
        let role = role_index_to_wire(role_drop.selected()).to_string();
        let msg = message_row.text().trim().to_string();
        let message = if msg.is_empty() { None } else { Some(msg) };
        let email_clear = email_row.clone();
        let msg_clear = message_row.clone();
        share_dialog_op(
            &state_inv,
            state_inv.target.invite(emails, role, message),
            "Invitations sent",
            "Couldn't send invitations",
            Some(Busy::start(btn)),
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
    let rx = spawn_request(state.ui.dirs.control_socket(), state.target.list());
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
                    state_role.target.update_role(
                        id.clone(),
                        kind,
                        role_index_to_wire(d.selected()).to_string(),
                    ),
                    "Role updated",
                    "Couldn't update the role",
                    None,
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
        let who = entry.email.clone();
        remove.connect_clicked(move |btn| {
            let state = state_rm.clone();
            let id = id.clone();
            let button = btn.clone();
            confirm_destructive(
                btn,
                "Remove Access?",
                &format!("{who} will no longer be able to open this item."),
                "Remove",
                move || {
                    share_dialog_op(
                        &state,
                        state.target.remove_entry(id.clone(), kind),
                        "Access removed",
                        "Couldn't remove access",
                        Some(Busy::start(&button)),
                        None,
                    );
                },
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
            let mut subtitle = format!("Anyone with the link ({})", capitalize(&link.role));
            if link.has_password {
                subtitle.push_str(" · password-protected");
            }
            if let Some(expires) = link.expires {
                subtitle.push_str(" · ");
                subtitle.push_str(&link_expiry_label(expires, glib::real_time() / 1_000_000));
            }
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
            remove.connect_clicked(move |btn| {
                let state = state_rm.clone();
                let id = id.clone();
                let button = btn.clone();
                confirm_destructive(
                    btn,
                    "Remove Public Link?",
                    "The link stops working for everyone who has it. A new link will have \
                     a different address.",
                    "Remove Link",
                    move || {
                        share_dialog_op(
                            &state,
                            state.target.remove_link(id.clone()),
                            "Public link removed",
                            "Couldn't remove the link",
                            Some(Busy::start(&button)),
                            None,
                        );
                    },
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
            let expiry_names: Vec<&str> = LINK_EXPIRY.iter().map(|(name, _)| *name).collect();
            let expiry_drop = gtk4::DropDown::builder()
                .model(&gtk4::StringList::new(&expiry_names))
                .selected(0)
                .valign(gtk4::Align::Center)
                .build();
            let expiry_row = adw::ActionRow::builder().title("Expires").build();
            expiry_row.add_suffix(&expiry_drop);
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
            create.connect_clicked(move |btn| {
                if !btn.is_sensitive() {
                    return;
                }
                let role = if role_drop.selected() == 1 {
                    "editor"
                } else {
                    "viewer"
                }
                .to_string();
                let pw = pw_for.text().to_string();
                let password = if pw.is_empty() { None } else { Some(pw) };
                let expires = link_expiry_at(expiry_drop.selected(), glib::real_time() / 1_000_000);
                share_dialog_create_link(&state_c, role, password, expires, Busy::start(btn));
            });

            state.link_group.add(&role_row);
            state.link_group.add(&pw_row);
            state.link_group.add(&expiry_row);
            state.link_group.add(&create_wrap);
            rows.push(role_row.upcast());
            rows.push(pw_row.upcast());
            rows.push(expiry_row.upcast());
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
    expires: Option<i64>,
    busy: Busy,
) {
    let rx = spawn_request(
        state.ui.dirs.control_socket(),
        state.target.create_link(role, password, expires),
    );
    let state = state.clone();
    glib::spawn_future_local(async move {
        let reply = rx.recv().await;
        drop(busy);
        match reply {
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
/// runs an extra UI tweak (e.g. clearing the invite fields) before the reload;
/// `busy` keeps the button that started it spinning until the reply arrives.
pub(crate) fn share_dialog_op(
    state: &Rc<ShareDialog>,
    req: Request,
    done: &'static str,
    failed: &'static str,
    busy: Option<Busy>,
    on_success: Option<Box<dyn Fn()>>,
) {
    let rx = spawn_request(state.ui.dirs.control_socket(), req);
    let state = state.clone();
    glib::spawn_future_local(async move {
        let reply = rx.recv().await;
        drop(busy);
        match reply {
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

/// How an existing link's expiry reads in its subtitle: the date it stops
/// working, or that it already has.
pub(crate) fn link_expiry_label(expires: i64, now: i64) -> String {
    if expires <= now {
        return "expired".to_string();
    }
    let date = glib::DateTime::from_unix_local(expires)
        .ok()
        .and_then(|at| at.format("%x").ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    format!("expires {date}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_check_accepts_plain_addresses() {
        assert!(looks_like_email("a@b.co"));
        assert!(looks_like_email("first.last+tag@mail.example.org"));
    }

    #[test]
    fn email_check_rejects_typos() {
        for bad in [
            "a", "a@", "@b.co", "a@b", "a@b.", "a@.co", "a@@b.co", "a@b@c.co",
        ] {
            assert!(!looks_like_email(bad), "{bad}");
        }
    }

    #[test]
    fn parse_emails_splits_and_reports_first_bad_entry() {
        assert_eq!(
            parse_emails("a@b.co, c@d.org;e@f.net"),
            Ok(vec!["a@b.co".into(), "c@d.org".into(), "e@f.net".into()])
        );
        assert_eq!(parse_emails("  ,  "), Ok(vec![]));
        assert_eq!(parse_emails("a@b.co oops"), Err("oops".into()));
    }

    #[test]
    fn link_expiry_counts_days_from_now() {
        assert_eq!(link_expiry_at(0, 1_000), None);
        assert_eq!(link_expiry_at(1, 1_000), Some(1_000 + 86_400));
        assert_eq!(link_expiry_at(3, 0), Some(30 * 86_400));
        assert_eq!(link_expiry_at(99, 0), None);
    }

    #[test]
    fn past_expiry_reads_expired() {
        assert_eq!(link_expiry_label(10, 20), "expired");
        assert!(link_expiry_label(2_000_000_000, 0).starts_with("expires "));
    }
}
