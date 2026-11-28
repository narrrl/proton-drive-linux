use crate::*;

/// The role labels offered in the Share dialog's dropdowns, in index order.
pub(crate) fn share_roles() -> [String; 3] {
    [
        pgettext("role", "Viewer"),
        pgettext("role", "Editor"),
        pgettext("role", "Admin"),
    ]
}

/// A dropdown model from translated labels.
fn string_list(labels: &[String]) -> gtk4::StringList {
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    gtk4::StringList::new(&labels)
}

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
    // Translators: a public link that never expires.
    (gettext_noop("Never"), None),
    (gettext_noop("1 day"), Some(1)),
    (gettext_noop("7 days"), Some(7)),
    (gettext_noop("30 days"), Some(30)),
];

/// The Unix expiry for a [`LINK_EXPIRY`] choice, counted from `now`.
pub(crate) fn link_expiry_at(idx: u32, now: i64) -> Option<i64> {
    LINK_EXPIRY
        .get(idx as usize)
        .and_then(|(_, days)| *days)
        .map(|days| now + days * 86_400)
}

/// The labels of the Expires dropdown on an existing link. When the link has
/// an expiry, the first entry is that expiry, so the dropdown shows what is
/// set and choosing it again changes nothing.
pub(crate) fn link_expiry_choices(current: Option<i64>, now: i64) -> Vec<String> {
    let current = current.map(|at| capitalize(&link_expiry_label(at, now)));
    current
        .into_iter()
        .chain(LINK_EXPIRY.iter().map(|(name, _)| gettext(name)))
        .collect()
}

/// What choosing entry `idx` of [`link_expiry_choices`] does to the expiry.
pub(crate) fn link_expiry_change(idx: u32, current: Option<i64>, now: i64) -> LinkChange<i64> {
    let idx = match (current, idx) {
        (Some(_), 0) => return LinkChange::Keep,
        (Some(_), idx) => idx - 1,
        (None, idx) => idx,
    };
    match link_expiry_at(idx, now) {
        Some(at) => LinkChange::Set(at),
        None if current.is_some() => LinkChange::Remove,
        None => LinkChange::Keep,
    }
}

/// A row with a title and a dropdown of `labels` as its suffix.
fn dropdown_row(title: &str, labels: &[String], selected: u32) -> (adw::ActionRow, gtk4::DropDown) {
    let drop = gtk4::DropDown::builder()
        .model(&string_list(labels))
        .selected(selected)
        .valign(gtk4::Align::Center)
        .build();
    let row = adw::ActionRow::builder().title(title).build();
    row.add_suffix(&drop);
    (row, drop)
}

/// The roles a public link can grant, in dropdown order.
fn link_roles() -> [String; 2] {
    [pgettext("role", "Viewer"), pgettext("role", "Editor")]
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
        let spinner = spinner();
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

/// A section's action, such as Send Invitations. It goes under the section's
/// rows, at their right edge, rather than in a row of its own: a row in a boxed
/// list reads as a setting, and a button alone in one looks misplaced.
fn group_button(label: &str) -> gtk4::Button {
    let button = gtk4::Button::builder()
        .label(label)
        .halign(gtk4::Align::End)
        .margin_top(12)
        .build();
    button.add_css_class("suggested-action");
    button
}

/// A section's only row while the share is being read.
fn loading_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(gettext("Loading…")).build();
    row.add_prefix(&spinner());
    row
}

/// How an open Share dialog addresses its node.
///
/// A node the browser is showing has a mountpoint-relative path. A node reached
/// from Shared or Shared-with-me may not: it can live under a device folder's
/// own inode space, or on someone else's volume, neither of which the primary
/// mount's path space can name. Those carry a uid instead, and the daemon
/// resolves it with `Core::resolve_anywhere`.
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

    fn update_link(
        &self,
        id: String,
        role: Option<String>,
        password: LinkChange<String>,
        expires: LinkChange<i64>,
    ) -> Request {
        match self {
            ShareTarget::Path(path) => Request::UpdatePublicLink {
                path: path.clone(),
                id,
                role,
                password,
                expires,
            },
            ShareTarget::Uid(uid) => Request::UpdatePublicLinkByUid {
                uid: uid.clone(),
                id,
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
    open_share_dialog_over(ui, entry, None);
}

/// [`open_share_dialog`] over `parent` rather than the main window. The photo
/// viewer is a window of its own, and a dialog on the main window would open
/// behind it.
pub(crate) fn open_share_dialog_over(ui: &Rc<Ui>, entry: &DirEntry, parent: Option<gtk4::Window>) {
    if !*ui.mounted.borrow() {
        toast_error(
            ui,
            &gettext("Can't share"),
            &gettext("Proton Drive isn't connected."),
        );
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
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &pgettext("verb", "Share"),
        &entry.name,
    )));
    toolbar.add_top_bar(&header);

    // Invite section: emails + role + optional message.
    let invite_group = adw::PreferencesGroup::builder()
        .title(gettext("Invite people"))
        .description(gettext(
            "Proton and non-Proton email addresses, separated by spaces or commas.",
        ))
        .build();
    let email_row = adw::EntryRow::builder()
        .title(gettext("Email addresses"))
        .build();
    let role_model = string_list(&share_roles());
    let role_drop = gtk4::DropDown::builder()
        .model(&role_model)
        .selected(0)
        .valign(gtk4::Align::Center)
        .build();
    // The role goes with the addresses it applies to, at the end of their row.
    role_drop.set_tooltip_text(Some(&gettext("Role")));
    email_row.add_suffix(&role_drop);
    let message_row = adw::EntryRow::builder()
        .title(gettext("Message (optional)"))
        .build();
    let invite_btn = group_button(&gettext("Send Invitations"));
    invite_btn.set_sensitive(false);
    invite_group.add(&email_row);
    invite_group.add(&message_row);
    invite_group.add(&invite_btn);

    let people = adw::PreferencesGroup::builder()
        .title(gettext("People with access"))
        .build();
    let link_group = adw::PreferencesGroup::builder()
        .title(gettext("Public link"))
        .build();

    // Until the share is read, each section says so rather than standing
    // empty under its title.
    let people_loading = loading_row();
    people.add(&people_loading);
    let link_loading = loading_row();
    link_group.add(&link_loading);

    let content = adw::PreferencesPage::new();
    content.add(&invite_group);
    content.add(&people);
    content.add(&link_group);
    toolbar.set_content(Some(&content));

    let dialog = adw::Dialog::builder()
        .title(pgettext("verb", "Share"))
        .content_width(480)
        .content_height(560)
        .child(&toolbar)
        .build();

    let state = Rc::new(ShareDialog {
        ui: ui.clone(),
        target,
        people,
        link_group,
        people_rows: RefCell::new(vec![people_loading.upcast()]),
        link_rows: RefCell::new(vec![link_loading.upcast()]),
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
                // Translators: {text} is what the user typed in place of an email address.
                row.set_tooltip_text(Some(&gettext_f(
                    "“{text}” isn't an email address",
                    &[("text", bad)],
                )));
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
            &gettext("Invitations sent"),
            &gettext("Couldn't send invitations"),
            Some(Busy::start(btn)),
            Some(Box::new(move || {
                email_clear.set_text("");
                msg_clear.set_text("");
            })),
        );
    });

    // Shared by me lists what this dialog changes: reload it when it is the
    // page underneath. Any other page reads it again on the next visit.
    let ui_closed = ui.clone();
    dialog.connect_closed(move |_| {
        if ui_closed.stack.visible_child_name().as_deref() == Some("sharedbyme") {
            load_shared_by_me(&ui_closed);
        }
    });
    share_dialog_reload(&state);
    dialog.present(parent.or_else(|| ui_window(ui)).as_ref());
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
                share_dialog_unread(&state);
                toast_error(&state.ui, &gettext("Couldn't load sharing"), &message)
            }
            _ => {
                share_dialog_unread(&state);
                toast_error(
                    &state.ui,
                    &gettext("Couldn't load sharing"),
                    &gettext("The Proton Drive service didn't respond."),
                )
            }
        }
    });
}

/// The share couldn't be read: what the sections showed (rows still loading,
/// or ones that may now be stale) gives way to one line that says so.
fn share_dialog_unread(state: &Rc<ShareDialog>) {
    for row in state.people_rows.borrow_mut().drain(..) {
        state.people.remove(&row);
    }
    for row in state.link_rows.borrow_mut().drain(..) {
        state.link_group.remove(&row);
    }
    let row = dim_row(&gettext("Couldn't load sharing"));
    state.people.add(&row);
    state.people_rows.borrow_mut().push(row.upcast());
}

/// Rebuild the "People with access" rows from a fresh share listing.
pub(crate) fn repaint_share_people(state: &Rc<ShareDialog>, entries: &[ShareEntry]) {
    for row in state.people_rows.borrow_mut().drain(..) {
        state.people.remove(&row);
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    if entries.is_empty() {
        let row = dim_row(&gettext("No one else has access yet."));
        state.people.add(&row);
        rows.push(row.upcast());
        *state.people_rows.borrow_mut() = rows;
        return;
    }
    for entry in entries {
        // A member is the plain case and needs no word; an invitation says
        // it is still waiting on the other side.
        let subtitle = match entry.kind {
            ShareEntryKind::Member => String::new(),
            ShareEntryKind::ProtonInvite => gettext("Invitation pending"),
            ShareEntryKind::ExternalInvite => gettext("Invitation pending, no Proton account yet"),
        };
        let row = adw::ActionRow::builder()
            .title(&entry.email)
            .subtitle(&subtitle)
            .use_markup(false)
            .build();
        row.add_prefix(&adw::Avatar::new(32, Some(&entry.email), true));

        // External invites can't have their role changed; show it read-only.
        // Members and Proton invites get a role dropdown.
        if matches!(
            entry.kind,
            ShareEntryKind::Member | ShareEntryKind::ProtonInvite
        ) {
            let model = string_list(&share_roles());
            let drop = gtk4::DropDown::builder()
                .model(&model)
                .selected(role_wire_to_index(&entry.role))
                .valign(gtk4::Align::Center)
                .build();
            // Weak, here and in every row below: the rows belong to groups the
            // dialog state holds, so a strong handle in a row's handler is a
            // cycle that kept each closed dialog in memory.
            let state_role = Rc::downgrade(state);
            let id = entry.id.clone();
            let kind = entry.kind;
            drop.connect_selected_notify(move |d| {
                let Some(state_role) = state_role.upgrade() else {
                    return;
                };
                share_dialog_op(
                    &state_role,
                    state_role.target.update_role(
                        id.clone(),
                        kind,
                        role_index_to_wire(d.selected()).to_string(),
                    ),
                    &gettext("Role updated"),
                    &gettext("Couldn't update the role"),
                    None,
                    None,
                );
            });
            row.add_suffix(&drop);
        } else {
            let label = gtk4::Label::builder()
                .label(role_label(&entry.role).unwrap_or_else(|| capitalize(&entry.role)))
                .valign(gtk4::Align::Center)
                .build();
            label.add_css_class("dim-label");
            row.add_suffix(&label);
        }

        let remove = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(gettext("Remove access"))
            .valign(gtk4::Align::Center)
            .build();
        remove.add_css_class("flat");
        let state_rm = Rc::downgrade(state);
        let id = entry.id.clone();
        let kind = entry.kind;
        let who = entry.email.clone();
        remove.connect_clicked(move |btn| {
            let Some(state) = state_rm.upgrade() else {
                return;
            };
            let id = id.clone();
            let button = btn.clone();
            confirm_destructive(
                btn,
                &gettext("Remove Access?"),
                // Translators: {email} is the address of the person losing access.
                &gettext_f(
                    "{email} will no longer be able to open this item.",
                    &[("email", &who)],
                ),
                &gettext("Remove"),
                move || {
                    share_dialog_op(
                        &state,
                        state.target.remove_entry(id.clone(), kind),
                        &gettext("Access removed"),
                        &gettext("Couldn't remove access"),
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

/// Rebuild the public-link section: an existing link (copy / remove, and its
/// settings) or a create control.
pub(crate) fn repaint_share_link(state: &Rc<ShareDialog>, link: Option<&PublicLinkInfo>) {
    for row in state.link_rows.borrow_mut().drain(..) {
        state.link_group.remove(&row);
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();

    match link {
        Some(link) => {
            let now = glib::real_time() / 1_000_000;
            let url = link.url.clone().unwrap_or_default();
            let expired = link.expires.is_some_and(|at| at <= now);
            // The row says how the link works, not where it is: the address
            // is a key, and someone showing their screen should not hand it
            // out by opening the dialog.
            let row = adw::ActionRow::builder()
                .title(link_row_title(link.role == "editor", expired))
                .subtitle(link_policy(
                    link.role == "editor",
                    link.has_password,
                    link.expires,
                    now,
                ))
                .use_markup(false)
                .build();
            row.add_css_class("property");
            let (settings, expiry_drop) = link_settings_row(state, link);

            // The address, hidden until asked for. Every repaint hides it again.
            let address = adw::ActionRow::builder()
                .title(&url)
                .title_selectable(true)
                .use_markup(false)
                .visible(false)
                .build();
            address.add_css_class("monospace");

            if expired {
                // An expired link gives nothing to copy: whoever gets it sees an
                // error. What the owner can do is give it a new expiry.
                let edit = gtk4::Button::builder()
                    .label(gettext("Edit Expiry"))
                    .valign(gtk4::Align::Center)
                    .build();
                let (settings, expiry_drop) = (settings.clone(), expiry_drop.clone());
                edit.connect_clicked(move |_| {
                    settings.set_expanded(true);
                    expiry_drop.grab_focus();
                });
                row.add_suffix(&edit);
            } else if !url.is_empty() {
                let reveal = gtk4::ToggleButton::builder()
                    .icon_name("view-reveal-symbolic")
                    .tooltip_text(gettext("Show link address"))
                    .valign(gtk4::Align::Center)
                    .build();
                reveal.add_css_class("flat");
                let address_t = address.clone();
                reveal.connect_toggled(move |btn| {
                    address_t.set_visible(btn.is_active());
                    btn.set_icon_name(if btn.is_active() {
                        "view-conceal-symbolic"
                    } else {
                        "view-reveal-symbolic"
                    });
                    btn.set_tooltip_text(Some(&if btn.is_active() {
                        gettext("Hide link address")
                    } else {
                        gettext("Show link address")
                    }));
                });
                row.add_suffix(&reveal);

                let copy = gtk4::Button::builder()
                    .icon_name("edit-copy-symbolic")
                    .tooltip_text(gettext("Copy link"))
                    .valign(gtk4::Align::Center)
                    .build();
                copy.add_css_class("flat");
                let ui_copy = state.ui.clone();
                let url_copy = url.clone();
                copy.connect_clicked(move |btn| {
                    btn.clipboard().set_text(&url_copy);
                    flash_copied(btn, "edit-copy-symbolic");
                    toast(&ui_copy, &gettext("Link copied"));
                });
                row.add_suffix(&copy);
            }

            let remove = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(gettext("Remove link"))
                .valign(gtk4::Align::Center)
                .build();
            remove.add_css_class("flat");
            let state_rm = Rc::downgrade(state);
            let id = link.id.clone();
            remove.connect_clicked(move |btn| {
                let Some(state) = state_rm.upgrade() else {
                    return;
                };
                let id = id.clone();
                let button = btn.clone();
                confirm_destructive(
                    btn,
                    &gettext("Remove Public Link?"),
                    &gettext("The link stops working for everyone who has it. A new link will have a different address."),
                    &gettext("Remove Link"),
                    move || {
                        share_dialog_op(
                            &state,
                            state.target.remove_link(id.clone()),
                            &gettext("Public link removed"),
                            &gettext("Couldn't remove the link"),
                            Some(Busy::start(&button)),
                            None,
                        );
                    },
                );
            });
            row.add_suffix(&remove);

            state.link_group.add(&row);
            rows.push(row.upcast());
            state.link_group.add(&address);
            rows.push(address.upcast());
            state.link_group.add(&settings);
            rows.push(settings.upcast());
        }
        None => {
            let (role_row, role_drop) = dropdown_row(&gettext("Link role"), &link_roles(), 0);
            let pw_row = adw::PasswordEntryRow::builder()
                .title(gettext("Password (optional)"))
                .build();
            let expiry_names: Vec<String> =
                LINK_EXPIRY.iter().map(|(name, _)| gettext(name)).collect();
            let (expiry_row, expiry_drop) = dropdown_row(&gettext("Expires"), &expiry_names, 0);
            // How the link will work, right above the button that makes it.
            let now = glib::real_time() / 1_000_000;
            let summary = adw::ActionRow::builder()
                .title(gettext("The new link"))
                .subtitle(link_policy(false, false, None, now))
                .use_markup(false)
                .build();
            summary.add_css_class("property");
            let create = group_button(&gettext("Create Public Link"));

            let refresh: Rc<dyn Fn()> = {
                let (summary, role_drop, pw_row, expiry_drop) = (
                    summary.clone(),
                    role_drop.clone(),
                    pw_row.clone(),
                    expiry_drop.clone(),
                );
                Rc::new(move || {
                    let now = glib::real_time() / 1_000_000;
                    summary.set_subtitle(&link_policy(
                        role_drop.selected() == 1,
                        !pw_row.text().is_empty(),
                        link_expiry_at(expiry_drop.selected(), now),
                        now,
                    ));
                })
            };
            let r = refresh.clone();
            role_drop.connect_selected_notify(move |_| r());
            let r = refresh.clone();
            pw_row.connect_changed(move |_| r());
            expiry_drop.connect_selected_notify(move |_| refresh());

            // Enter in the password field moves on to the expiry, which comes
            // next: creating the link there would skip it.
            let next = expiry_drop.clone();
            pw_row.connect_entry_activated(move |_| {
                next.grab_focus();
            });

            let state_c = Rc::downgrade(state);
            let pw_for = pw_row.clone();
            create.connect_clicked(move |btn| {
                let Some(state_c) = state_c.upgrade() else {
                    return;
                };
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
            state.link_group.add(&summary);
            state.link_group.add(&create);
            rows.push(role_row.upcast());
            rows.push(pw_row.upcast());
            rows.push(expiry_row.upcast());
            rows.push(summary.upcast());
            rows.push(create.upcast());
        }
    }
    *state.link_rows.borrow_mut() = rows;
}

/// What the owner has chosen in an existing link's settings and not saved yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkDraft {
    /// Anyone with the link can edit.
    pub(crate) editor: bool,
    /// The chosen entry of [`link_expiry_choices`].
    pub(crate) expiry: u32,
    /// The link asks for a password.
    pub(crate) protect: bool,
    /// A new password; empty keeps the one the link has.
    pub(crate) password: String,
}

impl LinkDraft {
    /// The draft that changes nothing about `link`.
    pub(crate) fn of(link: &PublicLinkInfo) -> Self {
        Self {
            editor: link.role == "editor",
            expiry: 0,
            protect: link.has_password,
            password: String::new(),
        }
    }
}

/// The one update that saving a [`LinkDraft`] sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkEdit {
    pub(crate) role: Option<String>,
    pub(crate) password: LinkChange<String>,
    pub(crate) expires: LinkChange<i64>,
}

/// What saving `draft` would ask of `link`: `Ok(None)` when it changes
/// nothing, `Err(())` when it cannot be saved yet. That is a password asked for
/// on a link that has none, with no password typed.
pub(crate) fn link_edit(
    link: &PublicLinkInfo,
    draft: &LinkDraft,
    now: i64,
) -> Result<Option<LinkEdit>, ()> {
    let role = (draft.editor != (link.role == "editor"))
        .then(|| if draft.editor { "editor" } else { "viewer" }.to_string());
    let password = match (draft.protect, draft.password.is_empty(), link.has_password) {
        (false, _, true) => LinkChange::Remove,
        (false, _, false) | (true, true, true) => LinkChange::Keep,
        (true, true, false) => return Err(()),
        (true, false, _) => LinkChange::Set(draft.password.clone()),
    };
    let expires = link_expiry_change(draft.expiry, link.expires, now);
    let edit = LinkEdit {
        role,
        password,
        expires,
    };
    let unchanged = edit.role.is_none()
        && edit.password == LinkChange::Keep
        && edit.expires == LinkChange::Keep;
    Ok((!unchanged).then_some(edit))
}

/// The title of an existing link's row: what kind of link it is, never its
/// address.
pub(crate) fn link_row_title(editor: bool, expired: bool) -> String {
    match (expired, editor) {
        (true, _) => gettext("Expired public link"),
        (false, true) => gettext("Public editor link"),
        (false, false) => gettext("Public viewer link"),
    }
}

/// How a link works, in one line: who can do what, the password, the expiry.
pub(crate) fn link_policy(editor: bool, password: bool, expires: Option<i64>, now: i64) -> String {
    let mut parts = vec![if editor {
        gettext("Anyone with the link can edit")
    } else {
        gettext("Anyone with the link can view")
    }];
    parts.push(if password {
        gettext("password-protected")
    } else {
        gettext("no password")
    });
    parts.push(match expires {
        Some(at) => link_expiry_label(at, now),
        None => gettext("never expires"),
    });
    parts.join(" · ")
}

/// How `link` works once `edit` is saved.
fn link_policy_after(
    link: &PublicLinkInfo,
    draft: &LinkDraft,
    edit: &LinkEdit,
    now: i64,
) -> String {
    let expires = match edit.expires {
        LinkChange::Keep => link.expires,
        LinkChange::Remove => None,
        LinkChange::Set(at) => Some(at),
    };
    link_policy(draft.editor, draft.protect, expires, now)
}

/// The settings of an existing link, collapsed under one row. Nothing reaches
/// Drive until Save: an owner turning a viewer link into an editor link with a
/// password must not hand out editing before the password is set, so every
/// change goes in one update. Closing the dialog drops what was not saved.
/// Returns the row and its Expires dropdown, which an expired link's Edit
/// Expiry button opens the row on.
fn link_settings_row(
    state: &Rc<ShareDialog>,
    link: &PublicLinkInfo,
) -> (adw::ExpanderRow, gtk4::DropDown) {
    let now = glib::real_time() / 1_000_000;
    let expander = adw::ExpanderRow::builder()
        .title(gettext("Link settings"))
        .subtitle(gettext(
            "Change who can edit, the password or the expiry, then save. The link stays the same.",
        ))
        .build();
    let draft = Rc::new(RefCell::new(LinkDraft::of(link)));

    let (role_row, role_drop) = dropdown_row(
        &gettext("Link role"),
        &link_roles(),
        u32::from(draft.borrow().editor),
    );
    expander.add_row(&role_row);
    let (expiry_row, expiry_drop) = dropdown_row(
        &gettext("Expires"),
        &link_expiry_choices(link.expires, now),
        0,
    );
    expander.add_row(&expiry_row);
    let protect = adw::SwitchRow::builder()
        .title(gettext("Require a password"))
        .active(link.has_password)
        .build();
    expander.add_row(&protect);
    let pw_row = adw::PasswordEntryRow::builder()
        .title(if link.has_password {
            gettext("New password (optional)")
        } else {
            gettext("Password")
        })
        .visible(link.has_password)
        .build();
    expander.add_row(&pw_row);

    // What the link will do once saved, right above the one action that sends it.
    let summary = adw::ActionRow::builder()
        .title(gettext("No changes"))
        .use_markup(false)
        .build();
    summary.add_css_class("property");
    let save = gtk4::Button::builder()
        .label(gettext("Save Link Settings"))
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    save.add_css_class("suggested-action");
    summary.add_suffix(&save);
    expander.add_row(&summary);

    let refresh: Rc<dyn Fn()> = {
        let (draft, link, summary, save) =
            (draft.clone(), link.clone(), summary.clone(), save.clone());
        Rc::new(move || {
            let draft = draft.borrow();
            let now = glib::real_time() / 1_000_000;
            match link_edit(&link, &draft, now) {
                Ok(None) => {
                    summary.set_title(&gettext("No changes"));
                    summary.set_subtitle("");
                    save.set_sensitive(false);
                }
                Ok(Some(edit)) => {
                    summary.set_title(&gettext("After saving"));
                    summary.set_subtitle(&link_policy_after(&link, &draft, &edit, now));
                    save.set_sensitive(true);
                }
                Err(()) => {
                    summary.set_title(&gettext("Enter a password to save"));
                    summary.set_subtitle("");
                    save.set_sensitive(false);
                }
            }
        })
    };
    {
        let (draft, refresh) = (draft.clone(), refresh.clone());
        role_drop.connect_selected_notify(move |d| {
            draft.borrow_mut().editor = d.selected() == 1;
            refresh();
        });
    }
    {
        let (draft, refresh) = (draft.clone(), refresh.clone());
        expiry_drop.connect_selected_notify(move |d| {
            draft.borrow_mut().expiry = d.selected();
            refresh();
        });
    }
    {
        let (draft, refresh, pw_row) = (draft.clone(), refresh.clone(), pw_row.clone());
        protect.connect_active_notify(move |s| {
            draft.borrow_mut().protect = s.is_active();
            pw_row.set_visible(s.is_active());
            if s.is_active() {
                pw_row.grab_focus();
            }
            refresh();
        });
    }
    {
        let (draft, refresh) = (draft.clone(), refresh.clone());
        pw_row.connect_changed(move |row| {
            draft.borrow_mut().password = row.text().to_string();
            refresh();
        });
    }
    {
        let save = save.clone();
        pw_row.connect_entry_activated(move |_| {
            if save.is_sensitive() {
                save.emit_clicked();
            }
        });
    }
    let (state_w, expander_w, link) = (Rc::downgrade(state), expander.clone(), link.clone());
    save.connect_clicked(move |_| {
        let Some(state) = state_w.upgrade() else {
            return;
        };
        let Ok(Some(edit)) = link_edit(&link, &draft.borrow(), glib::real_time() / 1_000_000)
        else {
            return;
        };
        let request =
            state
                .target
                .update_link(link.id.clone(), edit.role, edit.password, edit.expires);
        share_dialog_update_link(&state, request, &expander_w);
    });
    (expander, expiry_drop)
}

/// Send a link change, then reload the dialog: on success to show the link as
/// it now is, on failure to put the controls back to what the link still is.
fn share_dialog_update_link(
    state: &Rc<ShareDialog>,
    request: Request,
    settings: &adw::ExpanderRow,
) {
    settings.set_sensitive(false);
    let rx = spawn_request(state.ui.dirs.control_socket(), request);
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::PublicLink { .. })) => toast(&state.ui, &gettext("Link updated")),
            Ok(Ok(Response::Error { message, .. })) => {
                toast_error(&state.ui, &gettext("Couldn't change the link"), &message)
            }
            _ => toast_error(
                &state.ui,
                &gettext("Couldn't change the link"),
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
        share_dialog_reload(&state);
    });
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
                toast(&state.ui, &gettext("Public link created"));
                share_dialog_reload(&state);
            }
            Ok(Ok(Response::Error { message, .. })) => {
                toast_error(&state.ui, &gettext("Couldn't create the link"), &message)
            }
            _ => toast_error(
                &state.ui,
                &gettext("Couldn't create the link"),
                &gettext("The Proton Drive service didn't respond."),
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
    done: &str,
    failed: &str,
    busy: Option<Busy>,
    on_success: Option<Box<dyn Fn()>>,
) {
    let rx = spawn_request(state.ui.dirs.control_socket(), req);
    let state = state.clone();
    let (done, failed) = (done.to_string(), failed.to_string());
    glib::spawn_future_local(async move {
        let reply = rx.recv().await;
        drop(busy);
        match reply {
            Ok(Ok(Response::Ok { .. })) => {
                if let Some(cb) = on_success {
                    cb();
                }
                toast(&state.ui, &done);
                share_dialog_reload(&state);
            }
            Ok(Ok(Response::Error { message, .. })) => toast_error(&state.ui, &failed, &message),
            _ => {
                // A role dropdown that failed is now out of sync with the server;
                // reload to snap it back.
                toast_error(
                    &state.ui,
                    &failed,
                    &gettext("The Proton Drive service didn't respond."),
                );
                share_dialog_reload(&state);
            }
        }
    });
}

/// How an existing link's expiry reads in its subtitle: the date it stops
/// working, or that it already has.
pub(crate) fn link_expiry_label(expires: i64, now: i64) -> String {
    if expires <= now {
        // Translators: a public link past its expiry date.
        return gettext("expired");
    }
    // Translators: strftime format for a link's expiry date; "%x" is the locale's date, such as "09/23/26".
    let format = gettext("%x");
    let date = glib::DateTime::from_unix_local(expires)
        .ok()
        .and_then(|at| at.format(&format).ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    // Translators: {date} is the date a public link stops working.
    gettext_f("expires {date}", &[("date", &date)])
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
    fn choosing_an_expiry_keeps_removes_or_sets_it() {
        let now = 1_000;
        // No expiry yet: the choices are just Never and the lifetimes.
        assert_eq!(link_expiry_choices(None, now).len(), LINK_EXPIRY.len());
        assert_eq!(link_expiry_change(0, None, now), LinkChange::Keep);
        assert_eq!(
            link_expiry_change(2, None, now),
            LinkChange::Set(now + 7 * 86_400)
        );

        // An expiry set: it comes first, then Never and the lifetimes.
        let at = Some(2_000_000_000);
        assert_eq!(link_expiry_choices(at, now).len(), LINK_EXPIRY.len() + 1);
        assert_eq!(link_expiry_change(0, at, now), LinkChange::Keep);
        assert_eq!(link_expiry_change(1, at, now), LinkChange::Remove);
        assert_eq!(
            link_expiry_change(2, at, now),
            LinkChange::Set(now + 86_400)
        );
    }

    fn viewer_link(has_password: bool, expires: Option<i64>) -> PublicLinkInfo {
        PublicLinkInfo {
            id: "link".into(),
            url: Some("https://drive.proton.me/urls/X#y".into()),
            role: "viewer".into(),
            expires,
            has_password,
            created: 0,
        }
    }

    #[test]
    fn an_untouched_draft_sends_nothing() {
        let link = viewer_link(true, Some(2_000_000_000));
        assert_eq!(link_edit(&link, &LinkDraft::of(&link), 1_000), Ok(None));
    }

    /// Editor and a password go in one update, so link holders never get edit
    /// access without the password.
    #[test]
    fn editor_and_a_password_are_saved_together() {
        let link = viewer_link(false, None);
        let draft = LinkDraft {
            editor: true,
            protect: true,
            password: "hunter2".into(),
            ..LinkDraft::of(&link)
        };
        assert_eq!(
            link_edit(&link, &draft, 1_000),
            Ok(Some(LinkEdit {
                role: Some("editor".into()),
                password: LinkChange::Set("hunter2".into()),
                expires: LinkChange::Keep,
            }))
        );
    }

    #[test]
    fn asking_for_a_password_without_one_cannot_be_saved() {
        let link = viewer_link(false, None);
        let draft = LinkDraft {
            editor: true,
            protect: true,
            ..LinkDraft::of(&link)
        };
        assert_eq!(link_edit(&link, &draft, 1_000), Err(()));
    }

    #[test]
    fn a_kept_password_stays_and_a_dropped_one_goes() {
        let link = viewer_link(true, None);
        let keep = LinkDraft {
            expiry: 2,
            ..LinkDraft::of(&link)
        };
        assert_eq!(
            link_edit(&link, &keep, 1_000).unwrap().unwrap().password,
            LinkChange::Keep
        );
        let drop = LinkDraft {
            protect: false,
            password: "typed then switched off".into(),
            ..LinkDraft::of(&link)
        };
        assert_eq!(
            link_edit(&link, &drop, 1_000),
            Ok(Some(LinkEdit {
                role: None,
                password: LinkChange::Remove,
                expires: LinkChange::Keep,
            }))
        );
    }

    #[test]
    fn the_summary_shows_the_policy_after_saving() {
        let link = viewer_link(true, Some(2_000_000_000));
        let draft = LinkDraft {
            editor: true,
            expiry: 1,
            protect: false,
            ..LinkDraft::of(&link)
        };
        let edit = link_edit(&link, &draft, 1_000).unwrap().unwrap();
        assert_eq!(
            link_policy_after(&link, &draft, &edit, 1_000),
            "Anyone with the link can edit · no password · never expires"
        );
    }

    /// The row names the kind of link and never its address.
    #[test]
    fn a_link_row_is_titled_by_what_the_link_does() {
        assert_eq!(link_row_title(false, false), "Public viewer link");
        assert_eq!(link_row_title(true, false), "Public editor link");
        assert_eq!(link_row_title(true, true), "Expired public link");
        let link = viewer_link(false, Some(10));
        assert!(!link_row_title(false, false).contains(link.url.as_deref().unwrap()));
        assert_eq!(
            link_policy(false, false, link.expires, 20),
            "Anyone with the link can view · no password · expired"
        );
    }

    #[test]
    fn past_expiry_reads_expired() {
        assert_eq!(link_expiry_label(10, 20), "expired");
        assert!(link_expiry_label(2_000_000_000, 0).starts_with("expires "));
    }
}
