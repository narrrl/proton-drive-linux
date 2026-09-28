use crate::*;

pub(crate) struct SharedState {
    // Shared with me page. Two views under a switcher in the header: what other
    // people share with me, browsed like My files, and the public links I saved.
    // Neither lives in the mount, so both are addressed by uid or token and
    // re-listed from the daemon.
    pub(crate) files: FileList,
    /// "files" and "links".
    pub(crate) views: adw::ViewStack,
    /// "list", or "status" while the page loads or can't be read.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    /// The files view: "list", or "empty" with [`SharedState::files_empty`].
    pub(crate) files_content: gtk4::Stack,
    pub(crate) files_empty: adw::StatusPage,
    /// Leaves the current shared folder.
    pub(crate) back: gtk4::Button,
    /// The breadcrumb trail from "Shared with me" down to the current folder.
    pub(crate) crumb: gtk4::Box,
    /// "{n} invitations waiting", with Review opening the invitations dialog.
    pub(crate) invitations_banner: adw::Banner,
    /// The invitations from the last top-level load.
    pub(crate) invitations: RefCell<Vec<InvitationInfo>>,
    /// The links view: "list", or "empty".
    pub(crate) links_content: gtk4::Stack,
    pub(crate) links_group: adw::PreferencesGroup,
    /// Link rows added last time, removed before the next paint (adw groups
    /// have no clear-all).
    pub(crate) link_rows: RefCell<Vec<gtk4::Widget>>,
    /// Where in a shared folder the page currently is, as `(uid, name)` from the
    /// top level down. Empty = the top level. Shared subtrees have no path in the
    /// mount, so descending is uid-addressed and the stack *is* the breadcrumb.
    pub(crate) nav: RefCell<Vec<(String, String)>>,
    /// Runs the loads. A newer one, such as a navigation made while a folder
    /// was loading, supersedes the one before it.
    pub(crate) loader: Rc<Loader>,
    /// The [`SharedState::nav`] the files on screen were painted for, or `None`
    /// before the first paint.
    pub(crate) listed: RefCell<Option<Vec<(String, String)>>>,
    /// When the Shared page last painted good data. `None` = never / invalidated,
    /// forcing a fetch on next visit. See [`PAGE_TTL`].
    pub(crate) loaded_at: Cell<Option<Instant>>,
}

impl SharedState {
    pub(crate) fn new(widgets: &SharedWidgets) -> Self {
        SharedState {
            files: widgets.files.clone(),
            views: widgets.views.clone(),
            content: widgets.content.clone(),
            status: widgets.status.clone(),
            retry: widgets.retry.clone(),
            files_content: widgets.files_content.clone(),
            files_empty: widgets.files_empty.clone(),
            back: widgets.back.clone(),
            crumb: widgets.crumb.clone(),
            invitations_banner: widgets.invitations_banner.clone(),
            invitations: RefCell::new(Vec::new()),
            links_content: widgets.links_content.clone(),
            links_group: widgets.links_group.clone(),
            link_rows: RefCell::new(Vec::new()),
            nav: RefCell::new(Vec::new()),
            loader: Loader::new(&widgets.content),
            listed: RefCell::new(None),
            loaded_at: Cell::new(None),
        }
    }
}

/// Widgets the Shared page's load/repaint touch.
pub(crate) struct SharedWidgets {
    pub(crate) files: FileList,
    pub(crate) views: adw::ViewStack,
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) files_content: gtk4::Stack,
    pub(crate) files_empty: adw::StatusPage,
    pub(crate) back: gtk4::Button,
    pub(crate) crumb: gtk4::Box,
    pub(crate) invitations_banner: adw::Banner,
    pub(crate) links_content: gtk4::Stack,
    pub(crate) links_group: adw::PreferencesGroup,
    pub(crate) refresh: gtk4::Button,
    pub(crate) layout: gtk4::Button,
    pub(crate) add_link: gtk4::Button,
    pub(crate) add_link_empty: gtk4::Button,
}

/// The Shared with me page: the files and folders other people share with me,
/// in the same list and grid as My files, and a second view of the public
/// links I saved. Pending invitations wait in a banner above both.
pub(crate) fn build_shared_page() -> (gtk4::Widget, SharedWidgets) {
    let refresh = refresh_button();
    let add_link = gtk4::Button::builder()
        .label(gettext("Save Link…"))
        .tooltip_text(gettext("Save a public link to open it from here"))
        .visible(false)
        .build();
    add_link.add_css_class("flat");

    // Files: a path bar over the list, as in My files.
    let files = FileList::new();
    files.show_list(true);
    let layout = layout_button(&files);
    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text(gettext("Back"))
        .sensitive(false)
        .build();
    back.add_css_class("flat");
    let crumb = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    crumb.set_valign(gtk4::Align::Center);
    let crumb_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::External)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .hexpand(true)
        .child(&crumb)
        .build();
    let path_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    path_bar.append(&back);
    path_bar.append(&crumb_scroll);

    let files_empty = adw::StatusPage::builder()
        .icon_name("pdfs-people-symbolic")
        .vexpand(true)
        .build();
    files_empty.add_css_class("compact");
    let files_content = gtk4::Stack::new();
    files_content.add_named(&files.views, Some("list"));
    files_content.add_named(&files_empty, Some("empty"));
    let files_page = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    files_page.append(&path_bar);
    files_page.append(&files_content);

    // Saved links: few enough, and plain enough, for a boxed list.
    let links_group = adw::PreferencesGroup::new();
    let links_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&adw::Clamp::builder().child(&links_group).build())
        .build();
    let add_link_empty = gtk4::Button::builder()
        .label(gettext("Save Link…"))
        .halign(gtk4::Align::Center)
        .build();
    add_link_empty.add_css_class("pill");
    add_link_empty.add_css_class("suggested-action");
    let links_empty = adw::StatusPage::builder()
        .icon_name("pdfs-link-symbolic")
        .title(gettext("No Saved Links"))
        .description(gettext(
            "Save a Proton Drive public link someone sent you to open it from here later.",
        ))
        .vexpand(true)
        .child(&add_link_empty)
        .build();
    links_empty.add_css_class("compact");
    let links_content = gtk4::Stack::new();
    links_content.add_named(&links_scroll, Some("list"));
    links_content.add_named(&links_empty, Some("empty"));

    let views = adw::ViewStack::new();
    views.set_vexpand(true);
    views.add_titled_with_icon(
        &files_page,
        Some("files"),
        &gettext("Shared with me"),
        "pdfs-people-symbolic",
    );
    views.add_titled_with_icon(
        &links_content,
        Some("links"),
        &gettext("Saved Links"),
        "pdfs-link-symbolic",
    );
    let switcher = adw::ViewSwitcher::builder()
        .stack(&views)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();

    let retry = gtk4::Button::builder()
        .label(gettext("Retry"))
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("pdfs-people-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&views, Some("list"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&content);

    let invitations_banner = adw::Banner::builder()
        .button_label(gettext("Review"))
        .build();

    let (frame, header) = page_frame_with(&switcher, &inner);
    // Under the header, above both views: an invitation is waiting for a
    // decision whichever view is open.
    frame.add_top_bar(&invitations_banner);
    header.pack_end(&refresh);
    header.pack_end(&layout);
    header.pack_end(&add_link);

    (
        frame.upcast(),
        SharedWidgets {
            files,
            views,
            content,
            status,
            retry,
            files_content,
            files_empty,
            back,
            crumb,
            invitations_banner,
            links_content,
            links_group,
            refresh,
            layout,
            add_link,
            add_link_empty,
        },
    )
}

/// Install the file list, its columns, and the page's buttons.
pub(crate) fn wire_shared(ui: &Rc<Ui>, widgets: &SharedWidgets) {
    let files = &ui.shared.files;
    files.wire(
        ui,
        FileListBehavior {
            activate: open_shared_entry,
            entry_menu: shared_entry_menu,
            bulk_menu: shared_bulk_menu,
            background_menu: shared_background_menu,
            badges: false,
            drag_and_drop: false,
        },
    );
    files.column_view.append_column(&shared_by_column());
    files
        .column_view
        .append_column(&text_column(&pgettext("column", "Access"), |e| {
            role_access(&e.role).unwrap_or_else(|| "—".to_string())
        }));
    files
        .column_view
        .append_column(&text_column(&pgettext("column", "Date Shared"), |e| {
            if e.shared_at > 0 {
                dates::short_date(e.shared_at)
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

    let ui_back = ui.clone();
    widgets.back.connect_clicked(move |_| {
        ui_back.shared.nav.borrow_mut().pop();
        load_shared(&ui_back);
    });
    let ui_retry = ui.clone();
    widgets
        .retry
        .connect_clicked(move |_| restart_service_then(&ui_retry, load_shared));
    for button in [&widgets.add_link, &widgets.add_link_empty] {
        let ui_add = ui.clone();
        button.connect_clicked(move |_| prompt_add_bookmark(&ui_add));
    }
    let ui_review = ui.clone();
    widgets
        .invitations_banner
        .connect_button_clicked(move |_| open_invitations_dialog(&ui_review));

    // The header offers what the view on screen can use.
    let (layout, add_link) = (widgets.layout.clone(), widgets.add_link.clone());
    let sync = move |views: &adw::ViewStack| {
        let links = views.visible_child_name().as_deref() == Some("links");
        layout.set_visible(!links);
        add_link.set_visible(links);
    };
    sync(&widgets.views);
    widgets.views.connect_visible_child_name_notify(sync);
}

/// Show a status page in place of both views.
pub(crate) fn shared_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.shared.status.set_icon_name(Some(icon));
    ui.shared.status.set_title(title);
    ui.shared.status.set_description(Some(description));
    ui.shared.retry.set_visible(retry);
    ui.shared.content.set_visible_child_name("status");
}

/// Fetch the top level (shared-with-me, invitations, saved links) in parallel
/// and repaint the page once all three land.
///
/// Inside a shared folder ([`SharedState::nav`] non-empty) only that folder's
/// children are fetched: invitations and saved links belong to the top level.
pub(crate) fn load_shared(ui: &Rc<Ui>) {
    cancel_file_thumbnails(ui);
    let current = ui.shared.nav.borrow().last().cloned();
    if let Some((uid, _)) = current {
        load_shared_folder(ui, uid);
        return;
    }
    let ticket = begin_shared_load(
        ui,
        "pdfs-people-symbolic",
        gettext("Reading your shared items."),
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
        if !ticket.is_current() {
            return;
        }

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
        repaint_invitations(&ui, invitations);
        repaint_links(&ui, &bookmark_items);
        repaint_shared_files(&ui, &shared_items);
        ui.shared.loaded_at.set(Some(Instant::now()));
    });
}

/// List one shared folder's children and repaint the files view as that
/// folder. The uid comes from the entry that was opened (or from the nav stack
/// on a reload) — a shared subtree is reachable no other way.
fn load_shared_folder(ui: &Rc<Ui>, uid: String) {
    let ticket = begin_shared_load(
        ui,
        "folder-symbolic",
        gettext("Reading this shared folder."),
    );
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSharedFolder { uid });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        if !ticket.is_current() {
            return;
        }
        match result {
            Ok(Ok(Response::Entries { entries })) => {
                repaint_shared_files(&ui, &entries);
                ui.shared.loaded_at.set(Some(Instant::now()));
            }
            Ok(Ok(Response::Error { message, kind })) => {
                // The folder is gone or access was revoked: fall back to the top
                // level rather than stranding the page on a dead uid.
                ui.shared.nav.borrow_mut().pop();
                toast_failure(&ui, &gettext("Couldn't open shared folder"), &message, kind);
                load_shared(&ui);
            }
            _ => {
                ui.shared.loaded_at.set(None);
                shared_unreachable(&ui);
            }
        }
    });
}

/// Start a load of the view [`SharedState::nav`] points at. A reload of the
/// folder on screen keeps its entries up and usable. Moving to another folder
/// greys them out until the new entries arrive, since they belong elsewhere.
fn begin_shared_load(ui: &Rc<Ui>, icon: &'static str, description: String) -> LoadTicket {
    let ui_p = ui.clone();
    let placeholder = move || {
        ui_p.shared.files.model.remove_all();
        shared_status(&ui_p, icon, &gettext("Loading…"), &description, false);
    };
    if *ui.shared.listed.borrow() == Some(ui.shared.nav.borrow().clone()) {
        ui.shared.loader.refresh(placeholder)
    } else {
        ui.shared.listed.borrow_mut().take();
        ui.shared.loader.replace(placeholder)
    }
}

/// The daemon didn't answer the Shared page. Same still-starting vs. down split
/// as the other pages.
pub(crate) fn shared_unreachable(ui: &Rc<Ui>) {
    service_unreachable(ui, "shared", shared_status, load_shared);
}

/// Paint the folder [`SharedState::nav`] points at (the top level when empty).
fn repaint_shared_files(ui: &Rc<Ui>, entries: &[DirEntry]) {
    ui.shared.content.set_visible_child_name("list");
    let nav = ui.shared.nav.borrow().clone();
    repaint_shared_crumb(ui, &nav);
    let top = nav.is_empty();
    *ui.shared.listed.borrow_mut() = Some(nav);
    if entries.is_empty() {
        ui.shared.files.model.remove_all();
        let (icon, title, description) = if top {
            (
                "pdfs-people-symbolic",
                gettext("Nothing Shared with You"),
                gettext("Files and folders other people share with you appear here."),
            )
        } else {
            (
                "folder-open-symbolic",
                gettext("This folder is empty"),
                gettext("Nothing has been added to this shared folder yet."),
            )
        };
        ui.shared.files_empty.set_icon_name(Some(icon));
        ui.shared.files_empty.set_title(&title);
        ui.shared.files_empty.set_description(Some(&description));
        ui.shared.files_content.set_visible_child_name("empty");
        return;
    }
    ui.shared.files_content.set_visible_child_name("list");
    // Only the rows that changed are swapped, so a refresh keeps the selection
    // and the scroll position.
    replace_items(&ui.shared.files.model, entries);
}

/// Rebuild the breadcrumb trail for `nav`: "Shared with me", then each folder
/// down to the current one, which is a plain heading.
fn repaint_shared_crumb(ui: &Rc<Ui>, nav: &[(String, String)]) {
    let crumb = &ui.shared.crumb;
    while let Some(child) = crumb.first_child() {
        crumb.remove(&child);
    }
    ui.shared.back.set_sensitive(!nav.is_empty());
    crumb.append(&shared_crumb_node(
        ui,
        &gettext("Shared with me"),
        0,
        nav.is_empty(),
    ));
    for (i, (_, name)) in nav.iter().enumerate() {
        let sep = gtk4::Label::new(Some("›"));
        sep.add_css_class("dim-label");
        crumb.append(&sep);
        crumb.append(&shared_crumb_node(ui, name, i + 1, i + 1 == nav.len()));
    }
}

/// One breadcrumb segment: a heading for the current folder, or a flat button
/// that goes back up to `depth` folders below the top level.
fn shared_crumb_node(ui: &Rc<Ui>, label: &str, depth: usize, current: bool) -> gtk4::Widget {
    if current {
        let l = gtk4::Label::builder()
            .label(label)
            .ellipsize(gtk4::pango::EllipsizeMode::Start)
            .build();
        l.add_css_class("heading");
        return l.upcast();
    }
    let button = gtk4::Button::builder().label(label).build();
    button.add_css_class("flat");
    let ui = ui.clone();
    button.connect_clicked(move |_| {
        ui.shared.nav.borrow_mut().truncate(depth);
        load_shared(&ui);
    });
    button.upcast()
}

/// Whether the files on screen are the top level, where each entry is a share
/// root of its own and can be left.
fn at_share_roots(ui: &Rc<Ui>) -> bool {
    ui.shared
        .listed
        .borrow()
        .as_ref()
        .is_some_and(|nav| nav.is_empty())
}

/// Folders open in place; files download and open with the default app.
fn open_shared_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    if entry.is_dir {
        ui.shared
            .nav
            .borrow_mut()
            .push((entry.uid.clone(), entry.name.clone()));
        load_shared(ui);
    } else {
        open_shared_file(ui, &entry.uid, &entry.name);
    }
}

fn shared_entry_menu(ui: &Rc<Ui>, entry: &DirEntry) -> ActionMenu {
    let mut menu = ActionMenu::new();
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    menu.item(&pgettext("verb", "Open"), move || {
        open_shared_entry(&ui_c, &entry_c)
    });
    // Only once the mount has interned the share does it have a place there.
    if !entry.path.is_empty() {
        let (ui_c, entry_c) = (ui.clone(), entry.clone());
        menu.item(&gettext("Show in My Files"), move || {
            show_in_my_files(&ui_c, &entry_c)
        });
    }
    // Leaving a folder's child on its own is not a thing the API offers.
    if at_share_roots(ui) {
        menu.section();
        let (ui_c, entry_c) = (ui.clone(), entry.clone());
        menu.item(&gettext("Leave…"), move || {
            prompt_leave_shared(&ui_c, std::slice::from_ref(&entry_c))
        });
    }
    menu
}

fn shared_bulk_menu(ui: &Rc<Ui>, entries: Vec<DirEntry>) -> ActionMenu {
    let mut menu = ActionMenu::new();
    menu.labelled_section(&ngettext_f(
        "{n} selected",
        "{n} selected",
        entries.len() as u64,
        &[],
    ));
    if at_share_roots(ui) {
        let ui_c = ui.clone();
        menu.item(&gettext("Leave…"), move || {
            prompt_leave_shared(&ui_c, &entries)
        });
    }
    menu
}

fn shared_background_menu(ui: &Rc<Ui>) -> ActionMenu {
    let mut menu = ActionMenu::new();
    let ui_c = ui.clone();
    menu.item(&gettext("Select All"), move || {
        ui_c.shared.files.selection.select_all();
    });
    let ui_c = ui.clone();
    menu.item(&gettext("Refresh"), move || reload_current_page(&ui_c));
    menu.section();
    let ui_c = ui.clone();
    menu.item(&gettext("Save Link…"), move || prompt_add_bookmark(&ui_c));
    menu
}

/// The Shared by column: who shared the item, and a warning pill when their
/// invitation did not verify.
fn shared_by_column() -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = gtk4::Label::builder()
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        label.add_css_class("dim-label");
        // The name beside it is what the invitation claims. When its signature
        // does not check out against that person's keys, the claim must not
        // read as fact — someone may be impersonating a contact to get a file
        // opened.
        let pill = gtk4::Label::new(Some(&gettext("Unverified sender")));
        pill.add_css_class("role-pill");
        pill.add_css_class("warning-pill");
        pill.add_css_class("caption");
        pill.set_valign(gtk4::Align::Center);
        pill.set_tooltip_text(Some(&gettext(UNVERIFIED_INVITER_TOOLTIP)));
        let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        cell.append(&label);
        cell.append(&pill);
        item.set_child(Some(&cell));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let cell = item.child().and_downcast::<gtk4::Box>().unwrap();
        let label = cell.first_child().and_downcast::<gtk4::Label>().unwrap();
        let pill = cell.last_child().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        let text = if entry.shared_by.is_empty() {
            "—"
        } else {
            entry.shared_by.as_str()
        };
        label.set_label(text);
        label.set_tooltip_text(Some(text));
        pill.set_visible(entry.shared_by_unverified);
    });
    let column = gtk4::ColumnViewColumn::new(Some(&pgettext("column", "Shared by")), Some(factory));
    column.set_resizable(true);
    column
}

/// Tooltip on the warning shown next to a share whose invitation did not verify.
const UNVERIFIED_INVITER_TOOLTIP: &str = gettext_noop(
    "The invitation's signature doesn't match the sender's keys. It may not really be from them.",
);

/// The display name for a wire role, or `None` when there is nothing to show:
/// owned content carries no role, and a role the API did not report is not
/// guessed at (the SDK's `from_permissions_exact` returns `None` for an
/// unrecognised mask rather than degrading it to viewer).
pub(crate) fn role_label(role: &str) -> Option<String> {
    match role {
        "viewer" => Some(pgettext("role", "Viewer")),
        "editor" => Some(pgettext("role", "Editor")),
        "admin" => Some(pgettext("role", "Admin")),
        _ => None,
    }
}

/// What a role lets me do, as a short phrase. A bare "Editor" read like a
/// job title rather than a permission.
pub(crate) fn role_access(role: &str) -> Option<String> {
    match role {
        "viewer" => Some(gettext("Can view")),
        "editor" => Some(gettext("Can edit")),
        "admin" => Some(gettext("Can manage")),
        _ => None,
    }
}

/// Download a file shared with me into the daemon's cache and hand it to the
/// user's default application — the shared-item twin of the browser's
/// download-and-open, addressed by uid because the file lives outside the mount.
/// The daemon reports the download as a transfer, so it shows with its
/// progress beside every other one.
fn open_shared_file(ui: &Rc<Ui>, uid: &str, name: &str) {
    // Ignore a repeat activation of a file already downloading, so an impatient
    // double-click doesn't kick off a second round-trip.
    if !ui.opening.borrow_mut().insert(uid.to_string()) {
        return;
    }
    ui.busy_begin();
    // Translators: {name} is a file name.
    toast(ui, &gettext_f("Downloading “{name}”…", &[("name", name)]));
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenSharedFile {
            uid: uid.to_string(),
        },
    );
    let ui = ui.clone();
    let uid = uid.to_string();
    let name = name.to_string();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.opening.borrow_mut().remove(&uid);
        match result {
            // A cache blob is named by content hash, so the open rules key off
            // the shared node's name instead.
            Ok(Ok(Response::FilePath { path })) => open_named_path(&path, &name),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &gettext("Couldn't open file"), &message, kind)
            }
            _ => toast_error(
                &ui,
                &gettext("Couldn't open file"),
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
    });
}

/// Keep the top level's invitations and say in the banner how many wait.
fn repaint_invitations(ui: &Rc<Ui>, invitations: Vec<InvitationInfo>) {
    let banner = &ui.shared.invitations_banner;
    banner.set_revealed(!invitations.is_empty());
    if !invitations.is_empty() {
        banner.set_title(&ngettext_f(
            "{n} invitation waiting",
            "{n} invitations waiting",
            invitations.len() as u64,
            &[],
        ));
    }
    *ui.shared.invitations.borrow_mut() = invitations;
}

/// The invitations waiting for a decision, one card each with Decline and
/// Accept. A card leaves the dialog once answered; the dialog closes with the
/// last one.
fn open_invitations_dialog(ui: &Rc<Ui>) {
    let invitations = ui.shared.invitations.borrow().clone();
    if invitations.is_empty() {
        return;
    }
    let group = adw::PreferencesGroup::builder()
        .description(gettext("Accept to add the item to your shared files."))
        .build();
    let dialog = adw::Dialog::builder()
        .title(gettext("Invitations"))
        .content_width(480)
        .build();
    let left = Rc::new(Cell::new(invitations.len()));
    for inv in invitations {
        let row = invitation_row(&inv);
        let answered = {
            let (group, row, dialog, left) =
                (group.clone(), row.clone(), dialog.clone(), left.clone());
            move || {
                group.remove(&row);
                left.set(left.get() - 1);
                if left.get() == 0 {
                    dialog.close();
                }
            }
        };
        let (accept, decline) = invitation_buttons(&row);
        let (ui_c, id, answered_c) = (ui.clone(), inv.id.clone(), answered.clone());
        accept.connect_clicked(move |_| {
            respond_invitation(&ui_c, &id, true);
            answered_c();
        });
        let (ui_c, id) = (ui.clone(), inv.id.clone());
        let name = inv.name.clone().unwrap_or_else(|| gettext("this item"));
        decline.connect_clicked(move |btn| {
            let (ui, id, answered) = (ui_c.clone(), id.clone(), answered.clone());
            confirm_destructive(
                btn,
                &gettext("Decline Invitation?"),
                // Translators: {name} is the shared item's name, or "this item".
                &gettext_f(
                    "You won't have access to {name} unless it is shared again.",
                    &[("name", &name)],
                ),
                &gettext("Decline"),
                move || {
                    respond_invitation(&ui, &id, false);
                    answered();
                },
            );
        });
        group.add(&row);
    }
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    dialog.present(ui_window(ui).as_ref());
}

/// One invitation's card: the item, and who it is from.
fn invitation_row(inv: &InvitationInfo) -> adw::ActionRow {
    let item = inv.name.clone().unwrap_or_else(|| gettext("a shared item"));
    let row = adw::ActionRow::builder()
        .title(&item)
        // Translators: {email} is the address of the person who sent the invitation.
        .subtitle(gettext_f("From {email}", &[("email", &inv.inviter_email)]))
        .build();
    row.add_prefix(&gtk4::Image::from_icon_name(if inv.is_dir {
        "folder-symbolic"
    } else {
        "text-x-generic-symbolic"
    }));
    row
}

/// Add labelled Accept and Decline buttons to an invitation's card.
fn invitation_buttons(row: &adw::ActionRow) -> (gtk4::Button, gtk4::Button) {
    let decline = gtk4::Button::builder()
        .label(gettext("Decline"))
        .valign(gtk4::Align::Center)
        .build();
    decline.add_css_class("flat");
    let accept = gtk4::Button::builder()
        .label(gettext("Accept"))
        .valign(gtk4::Align::Center)
        .build();
    accept.add_css_class("suggested-action");
    row.add_suffix(&decline);
    row.add_suffix(&accept);
    (accept, decline)
}

/// Rebuild the Saved Links view.
fn repaint_links(ui: &Rc<Ui>, bookmarks: &[BookmarkInfo]) {
    for row in ui.shared.link_rows.borrow_mut().drain(..) {
        ui.shared.links_group.remove(&row);
    }
    ui.shared
        .links_content
        .set_visible_child_name(if bookmarks.is_empty() {
            "empty"
        } else {
            "list"
        });
    let mut rows = Vec::new();
    for bm in bookmarks {
        let title = bm.name.clone().unwrap_or_else(|| gettext("Shared link"));
        let row = adw::ActionRow::builder()
            .title(&title)
            .subtitle(&bm.url)
            .subtitle_lines(1)
            .activatable(true)
            .tooltip_text(gettext("Open in browser"))
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name(if bm.is_dir {
            "folder-symbolic"
        } else {
            "pdfs-link-symbolic"
        }));
        let url = bm.url.clone();
        row.connect_activated(move |_| open_uri(&url));
        row.add_suffix(&gtk4::Image::from_icon_name("adw-external-link-symbolic"));

        let mut menu = ActionMenu::new();
        let url = bm.url.clone();
        menu.item(&gettext("Open in Browser"), move || open_uri(&url));
        let (ui_c, row_c, url) = (ui.clone(), row.clone(), bm.url.clone());
        menu.item(&gettext("Copy Link"), move || {
            row_c.clipboard().set_text(&url);
            toast(&ui_c, &gettext("Link copied"));
        });
        menu.section();
        let (ui_c, token, name) = (ui.clone(), bm.token.clone(), title.clone());
        menu.item(&gettext("Remove…"), move || {
            prompt_remove_bookmark(&ui_c, &token, &name)
        });
        row.add_suffix(&menu.button());

        ui.shared.links_group.add(&row);
        rows.push(row.upcast());
    }
    *ui.shared.link_rows.borrow_mut() = rows;
}

/// Accept or reject an invitation, then reload the Shared page.
pub(crate) fn respond_invitation(ui: &Rc<Ui>, id: &str, accept: bool) {
    let req = if accept {
        Request::AcceptInvitation { id: id.to_string() }
    } else {
        Request::RejectInvitation { id: id.to_string() }
    };
    let (done, failed) = if accept {
        (
            gettext("Invitation accepted"),
            gettext("Couldn't accept the invitation"),
        )
    } else {
        (
            gettext("Invitation rejected"),
            gettext("Couldn't reject the invitation"),
        )
    };
    run_shared_mutation(ui, req, &done, &failed);
}

/// Confirm, then leave shared items. Only share roots can be left.
pub(crate) fn prompt_leave_shared(ui: &Rc<Ui>, entries: &[DirEntry]) {
    let body = match entries {
        // Translators: {name} is the shared item's name.
        [one] => gettext_f(
            "Leave “{name}”? You'll lose access until you're invited again.",
            &[("name", &one.name)],
        ),
        _ => ngettext_f(
            "Leave {n} shared item? You'll lose access until you're invited again.",
            "Leave {n} shared items? You'll lose access until you're invited again.",
            entries.len() as u64,
            &[],
        ),
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Leave Shared Item?"))
        .body(body)
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("leave", &pgettext("verb", "Leave"));
    dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui_c = ui.clone();
    let uids: Vec<String> = entries.iter().map(|e| e.uid.clone()).collect();
    dialog.connect_response(Some("leave"), move |_, _| {
        let count = uids.len();
        run_requests(
            &ui_c,
            uids.iter()
                .map(|uid| Request::LeaveShared { uid: uid.clone() })
                .collect(),
            gettext_noop("Couldn't leave the shared item"),
            move |ui| {
                load_shared(ui);
                toast(
                    ui,
                    &ngettext_f(
                        "Left {n} shared item",
                        "Left {n} shared items",
                        count as u64,
                        &[],
                    ),
                );
            },
        );
    });
    dialog.present(ui_window(ui).as_ref());
}

/// Send `requests` one after the other, stopping at the first failure, which
/// is reported under `failed` (an untranslated msgid). `done` runs once all
/// succeeded. After a failure the page on screen is reloaded instead, since
/// the requests before it went through.
pub(crate) fn run_requests(
    ui: &Rc<Ui>,
    requests: Vec<Request>,
    failed: &'static str,
    done: impl FnOnce(&Rc<Ui>) + 'static,
) {
    ui.busy_begin();
    let ui = ui.clone();
    let socket = ui.dirs.control_socket();
    glib::spawn_future_local(async move {
        for req in requests {
            let result = spawn_request(socket.clone(), req).recv().await;
            let error = match result {
                Ok(Ok(Response::Ok { .. })) => continue,
                Ok(Ok(Response::Error { message, kind })) => Some((message, kind)),
                _ => None,
            };
            ui.busy_end();
            reload_current_page(&ui);
            match error {
                Some((message, kind)) => toast_failure(&ui, &gettext(failed), &message, kind),
                None => toast_error(
                    &ui,
                    &gettext(failed),
                    &gettext("The Proton Drive service didn't respond."),
                ),
            }
            return;
        }
        ui.busy_end();
        done(&ui);
    });
}

/// Confirm, then remove a saved link. The shared item itself is untouched.
pub(crate) fn prompt_remove_bookmark(ui: &Rc<Ui>, token: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Remove Saved Link?"))
        // Translators: {name} is the name of the item the saved link points to.
        .body(gettext_f(
            "Remove “{name}” from your saved links? The shared item itself is not affected.",
            &[("name", name)],
        ))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("remove", &gettext("Remove"));
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
                &gettext("Link removed"),
                &gettext("Couldn't remove the link"),
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Prompt for a public-link URL (and optional password) and save it.
pub(crate) fn prompt_add_bookmark(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Save Link"))
        .body(gettext("Paste a Proton Drive public link to save it here."))
        .build();
    let group = adw::PreferencesGroup::new();
    let url_row = adw::EntryRow::builder()
        .title(gettext("Public link URL"))
        .activates_default(true)
        .build();
    let pw_row = adw::PasswordEntryRow::builder()
        .title(gettext("Password (if the link has one)"))
        .activates_default(true)
        .build();
    group.add(&url_row);
    group.add(&pw_row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("save", &gettext("Save"));
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "save" {
            return;
        }
        let url = url_row.text().trim().to_string();
        if url.is_empty() {
            toast_error(
                &ui,
                &gettext("Couldn't save the link"),
                &gettext("A URL is required."),
            );
            return;
        }
        let pw = pw_row.text().to_string();
        let password = if pw.is_empty() { None } else { Some(pw) };
        // The new link lands in the Saved Links view; show it there.
        ui.shared.views.set_visible_child_name("links");
        run_shared_mutation(
            &ui,
            Request::CreateBookmark { url, password },
            &gettext("Link saved"),
            &gettext("Couldn't save the link"),
        );
    });
    dialog.present(win.as_ref());
}

/// Run a mutation raised from the Shared page and reload the page on success.
pub(crate) fn run_shared_mutation(ui: &Rc<Ui>, req: Request, done: &str, failed: &str) {
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    let done = done.to_string();
    let failed = failed.to_string();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => {
                load_shared(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &failed, &message, kind)
            }
            _ => toast_error(
                &ui,
                &failed,
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_says_what_it_allows() {
        assert_eq!(role_access("viewer").as_deref(), Some("Can view"));
        assert_eq!(role_access("editor").as_deref(), Some("Can edit"));
        assert_eq!(role_access("admin").as_deref(), Some("Can manage"));
        assert_eq!(role_access(""), None);
    }

    #[test]
    fn only_a_known_role_becomes_a_badge() {
        // "" is owned content (no role applies) and an unrecognised mask is
        // deliberately not guessed at — `from_permissions_exact` returns None
        // rather than degrading to viewer, and this must not undo that.
        for role in ["viewer", "editor", "admin"] {
            assert!(role_label(role).is_some(), "{role} should have a label");
        }
        assert!(role_label("").is_none());
        assert!(role_label("inherited").is_none());
    }
}
