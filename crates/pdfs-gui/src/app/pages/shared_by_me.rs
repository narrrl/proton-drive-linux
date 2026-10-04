use crate::*;

pub(crate) struct SharedByMeState {
    // Shared by me page: the items I have shared, as a file list whose columns
    // say where each lives, who has it and what its public link is doing.
    pub(crate) files: FileList,
    /// "list", or "status" while the page loads, is empty or can't be read.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    /// "12 items" under the page title.
    pub(crate) subtitle: adw::WindowTitle,
    /// Which items to show, indexed like [`SHARE_FILTERS`].
    pub(crate) filter: gtk4::DropDown,
    /// The last listing, unfiltered. The list holds [`DirEntry`]s; the columns
    /// and menus look a row's people and link up here by uid.
    pub(crate) items: Rc<RefCell<Vec<SharedItem>>>,
    /// Runs the loads; the rows stay up while one runs.
    pub(crate) loader: Rc<Loader>,
}

impl SharedByMeState {
    pub(crate) fn new(widgets: &SharedByMeWidgets) -> Self {
        SharedByMeState {
            files: widgets.files.clone(),
            content: widgets.content.clone(),
            status: widgets.status.clone(),
            retry: widgets.retry.clone(),
            subtitle: widgets.subtitle.clone(),
            filter: widgets.filter.clone(),
            items: Rc::new(RefCell::new(Vec::new())),
            loader: Loader::new(&widgets.content),
        }
    }
}

/// Widgets the Shared (by-me) page's load/repaint touch.
pub(crate) struct SharedByMeWidgets {
    pub(crate) files: FileList,
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) subtitle: adw::WindowTitle,
    pub(crate) filter: gtk4::DropDown,
}

/// Which shared items the page shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ShareFilter {
    All,
    /// Items with a public link.
    Links,
    /// Items shared with people, invited or already in.
    People,
    /// Items whose public link has expired.
    Expired,
}

/// The filters in the order the header's drop-down lists them.
const SHARE_FILTERS: [ShareFilter; 4] = [
    ShareFilter::All,
    ShareFilter::Links,
    ShareFilter::People,
    ShareFilter::Expired,
];

fn share_filter_label(filter: ShareFilter) -> String {
    match filter {
        ShareFilter::All => gettext("All Shared Items"),
        ShareFilter::Links => gettext("Links Only"),
        ShareFilter::People => gettext("People Only"),
        ShareFilter::Expired => gettext("Expired Links"),
    }
}

/// The Shared by me page: every item I have shared with people or by link.
/// Activating one opens its Share dialog, since managing access is what this
/// page is for; opening the item itself is in the menu.
pub(crate) fn build_shared_by_me_page() -> (gtk4::Widget, SharedByMeWidgets) {
    let files = FileList::new();
    files.show_list(true);
    let layout = layout_button(&files);
    let labels: Vec<String> = SHARE_FILTERS
        .iter()
        .copied()
        .map(share_filter_label)
        .collect();
    let filter =
        gtk4::DropDown::from_strings(&labels.iter().map(String::as_str).collect::<Vec<_>>());
    filter.set_valign(gtk4::Align::Center);
    filter.set_tooltip_text(Some(&gettext("Show only some shared items")));

    let retry = gtk4::Button::builder()
        .label(gettext("Retry"))
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("pdfs-share-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&files.views, Some("list"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&content);

    let (frame, header, subtitle) = page_frame(&gettext("Shared by me"), &inner);
    header.pack_start(&filter);
    header.pack_end(&layout);

    (
        frame.upcast(),
        SharedByMeWidgets {
            files,
            content,
            status,
            retry,
            subtitle,
            filter,
        },
    )
}

/// Install the file list, its columns, the filter and the retry button.
pub(crate) fn wire_shared_by_me(ui: &Rc<Ui>, widgets: &SharedByMeWidgets) {
    let files = &ui.shared_by_me.files;
    files.wire(
        ui,
        FileListBehavior {
            activate: open_share_dialog,
            entry_menu: shared_by_me_menu,
            bulk_menu: shared_by_me_bulk_menu,
            background_menu: shared_by_me_background_menu,
            badges: false,
            drag_and_drop: false,
        },
    );
    let items = &ui.shared_by_me.items;
    files
        .column_view
        .append_column(&text_column(&gettext("Location"), |e| {
            location_label(&e.path)
        }));
    let people = items.clone();
    files
        .column_view
        .append_column(&text_column(&pgettext("column", "People"), move |e| {
            find_item(&people, &e.uid)
                .map(|item| people_label(&item))
                .unwrap_or_else(|| "—".to_string())
        }));
    files.column_view.append_column(&link_column(ui));

    let ui_retry = ui.clone();
    widgets
        .retry
        .connect_clicked(move |_| restart_service_then(&ui_retry, load_shared_by_me));
    let ui_filter = ui.clone();
    widgets.filter.connect_selected_notify(move |_| {
        let items = ui_filter.shared_by_me.items.borrow().clone();
        repaint_shared_by_me(&ui_filter, &items);
    });
}

/// The shared item a row stands for.
fn find_item(items: &RefCell<Vec<SharedItem>>, uid: &str) -> Option<SharedItem> {
    items.borrow().iter().find(|item| item.uid == uid).cloned()
}

/// The Link column: what the public link is doing, and a button that copies
/// it while it works.
fn link_column(ui: &Rc<Ui>) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    let ui_setup = ui.clone();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = gtk4::Label::builder()
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        label.add_css_class("dim-label");
        let copy = gtk4::Button::builder()
            .icon_name("pdfs-link-symbolic")
            .tooltip_text(gettext("Copy link"))
            .valign(gtk4::Align::Center)
            .build();
        copy.add_css_class("flat");
        // Read at click time: the view recycles this cell for other rows.
        let (ui, item_c) = (ui_setup.clone(), item.clone());
        copy.connect_clicked(move |btn| {
            let Some(obj) = item_c.item().and_downcast::<BoxedAnyObject>() else {
                return;
            };
            let uid = obj.borrow::<DirEntry>().uid.clone();
            if let Some(url) = find_item(&ui.shared_by_me.items, &uid)
                .and_then(|item| item.link)
                .and_then(|link| link.url)
            {
                btn.clipboard().set_text(&url);
                flash_copied(btn, "pdfs-link-symbolic");
                toast(&ui, &gettext("Link copied"));
            }
        });
        let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        cell.append(&label);
        cell.append(&copy);
        item.set_child(Some(&cell));
    });
    let items = ui.shared_by_me.items.clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let cell = item.child().and_downcast::<gtk4::Box>().unwrap();
        let label = cell.first_child().and_downcast::<gtk4::Label>().unwrap();
        let copy = cell.last_child().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let uid = obj.borrow::<DirEntry>().uid.clone();
        let link = find_item(&items, &uid).and_then(|item| item.link);
        let now = glib::real_time() / 1_000_000;
        let text = link_label(link.as_ref(), now);
        label.set_label(&text);
        label.set_tooltip_text(Some(&text));
        copy.set_visible(
            link.as_ref()
                .is_some_and(|l| l.url.is_some() && !link_expired(l, now)),
        );
    });
    let column = gtk4::ColumnViewColumn::new(Some(&pgettext("column", "Link")), Some(factory));
    column.set_resizable(true);
    column
}

/// Where a shared item lives in My files: its folder, "My files" at the top,
/// or a dash when the daemon can't place it.
pub(crate) fn location_label(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None if path.is_empty() => "—".to_string(),
        None => gettext("My files"),
    }
}

/// Who can reach a shared item through an invitation.
pub(crate) fn people_label(item: &SharedItem) -> String {
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
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    }
}

fn link_expired(link: &PublicLinkInfo, now: i64) -> bool {
    link.expires.is_some_and(|at| at <= now)
}

/// What a public link is doing at `now` (epoch seconds): working, working
/// until a date, or expired, and whether a password guards it.
pub(crate) fn link_label(link: Option<&PublicLinkInfo>, now: i64) -> String {
    let Some(link) = link else {
        return "—".to_string();
    };
    let state = match link.expires {
        Some(at) if at <= now => pgettext("link", "Expired"),
        // Translators: {date} is when a public link stops working.
        Some(at) => gettext_f("Expires {date}", &[("date", &dates::short_date(at))]),
        None if link.created > 0 => gettext_f(
            // Translators: {date} is when the public link was created.
            "Active since {date}",
            &[("date", &dates::short_date(link.created))],
        ),
        None => pgettext("link", "Active"),
    };
    if link.has_password {
        [state, gettext("Password protected")].join(" · ")
    } else {
        state
    }
}

/// Whether `item` passes `filter` at `now` (epoch seconds).
pub(crate) fn matches_filter(item: &SharedItem, filter: ShareFilter, now: i64) -> bool {
    match filter {
        ShareFilter::All => true,
        ShareFilter::Links => item.link.is_some(),
        ShareFilter::People => item.member_count + item.invite_count > 0,
        ShareFilter::Expired => item.link.as_ref().is_some_and(|l| link_expired(l, now)),
    }
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
    ui.shared_by_me.subtitle.set_subtitle("");
}

/// Fetch the shared-by-me listing and repaint the page.
pub(crate) fn load_shared_by_me(ui: &Rc<Ui>) {
    cancel_file_thumbnails(ui);
    let ui_p = ui.clone();
    let ticket = ui.shared_by_me.loader.refresh(move || {
        ui_p.shared_by_me.files.model.remove_all();
        shared_by_me_status(
            &ui_p,
            "pdfs-share-symbolic",
            &gettext("Loading…"),
            &gettext("Reading what you've shared."),
            false,
        );
    });
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSharedByMe);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        if !ticket.is_current() {
            return;
        }
        if !matches!(result, Ok(Ok(Response::SharedByMe { .. }))) {
            ui.shared_by_me.files.model.remove_all();
        }
        match result {
            Ok(Ok(Response::SharedByMe { items })) => {
                let changed = changed_items(&ui.shared_by_me.items.borrow(), &items);
                *ui.shared_by_me.items.borrow_mut() = items.clone();
                repaint_shared_by_me(&ui, &items);
                rebind_rows(&ui, &changed);
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
                &gettext("Unexpected reply from the Proton Drive service."),
                false,
            ),
            Ok(Err(_)) | Err(_) => {
                shared_by_me_unreachable(&ui);
            }
        }
    });
}

/// The daemon didn't answer the Shared (by-me) page.
pub(crate) fn shared_by_me_unreachable(ui: &Rc<Ui>) {
    service_unreachable(ui, "sharedbyme", shared_by_me_status, load_shared_by_me);
}

/// Show the items that pass the header's filter.
pub(crate) fn repaint_shared_by_me(ui: &Rc<Ui>, items: &[SharedItem]) {
    if items.is_empty() {
        shared_by_me_status(
            ui,
            "pdfs-share-symbolic",
            &gettext("Nothing shared yet"),
            &gettext("Items you share with people or by link show up here."),
            false,
        );
        ui.shared_by_me.files.model.remove_all();
        return;
    }
    let filter = SHARE_FILTERS
        .get(ui.shared_by_me.filter.selected() as usize)
        .copied()
        .unwrap_or(ShareFilter::All);
    let now = glib::real_time() / 1_000_000;
    let shown: Vec<DirEntry> = items
        .iter()
        .filter(|item| matches_filter(item, filter, now))
        .map(shared_item_as_entry)
        .collect();
    if shown.is_empty() {
        shared_by_me_status(
            ui,
            "pdfs-share-symbolic",
            &gettext("No Matching Items"),
            &gettext("None of your shared items match this filter."),
            false,
        );
        ui.shared_by_me.files.model.remove_all();
        return;
    }
    ui.shared_by_me.content.set_visible_child_name("list");
    ui.shared_by_me.subtitle.set_subtitle(&ngettext_f(
        "{n} item",
        "{n} items",
        shown.len() as u64,
        &[],
    ));
    // Only the rows that changed are swapped, so a refresh keeps the selection
    // and the scroll position.
    replace_items(&ui.shared_by_me.files.model, &shown);
}

/// The uids in `new` whose people or link differ from `old`. Their rows show
/// the same [`DirEntry`] as before, so the list would not repaint them.
fn changed_items(old: &[SharedItem], new: &[SharedItem]) -> HashSet<String> {
    new.iter()
        .filter(|item| old.iter().find(|o| o.uid == item.uid) != Some(*item))
        .map(|item| item.uid.clone())
        .collect()
}

/// Have the list bind the rows of `uids` again, to repaint their columns.
fn rebind_rows(ui: &Rc<Ui>, uids: &HashSet<String>) {
    let model = &ui.shared_by_me.files.model;
    for pos in 0..model.n_items() {
        if entry_at(Some(model), pos).is_some_and(|e| uids.contains(&e.uid)) {
            model.items_changed(pos, 1, 1);
        }
    }
}

/// A shared item's right-click menu: manage it, open it, its link, and Stop
/// Sharing. Manage Access opens the Share dialog, which addresses a pathless
/// node by uid, so every shared item can be managed from here.
fn shared_by_me_menu(ui: &Rc<Ui>, entry: &DirEntry) -> ActionMenu {
    let mut menu = ActionMenu::new();
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    menu.item(&gettext("Manage Access…"), move || {
        open_share_dialog(&ui_c, &entry_c)
    });
    // A node the daemon can place in my tree opens like it would in My Files.
    if !entry.path.is_empty() {
        let (ui_c, entry_c) = (ui.clone(), entry.clone());
        menu.item(&pgettext("verb", "Open"), move || {
            open_shared_by_me(&ui_c, &entry_c)
        });
        let (ui_c, entry_c) = (ui.clone(), entry.clone());
        menu.item(&gettext("Show in My Files"), move || {
            show_in_my_files(&ui_c, &entry_c)
        });
    }
    let now = glib::real_time() / 1_000_000;
    let url = find_item(&ui.shared_by_me.items, &entry.uid)
        .and_then(|item| item.link)
        .filter(|link| !link_expired(link, now))
        .and_then(|link| link.url);
    if let Some(url) = url {
        menu.section();
        let (ui_c, url_c) = (ui.clone(), url.clone());
        menu.item(&gettext("Copy Link"), move || {
            ui_c.stack.clipboard().set_text(&url_c);
            toast(&ui_c, &gettext("Link copied"));
        });
        menu.item(&gettext("Open Link"), move || open_uri(&url));
    }
    menu.section();
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    menu.item(&gettext("Stop Sharing…"), move || {
        prompt_stop_sharing(&ui_c, std::slice::from_ref(&entry_c))
    });
    menu
}

fn shared_by_me_bulk_menu(ui: &Rc<Ui>, entries: Vec<DirEntry>) -> ActionMenu {
    let mut menu = ActionMenu::new();
    menu.labelled_section(&ngettext_f(
        "{n} selected",
        "{n} selected",
        entries.len() as u64,
        &[],
    ));
    let ui_c = ui.clone();
    menu.item(&gettext("Stop Sharing…"), move || {
        prompt_stop_sharing(&ui_c, &entries)
    });
    menu
}

fn shared_by_me_background_menu(ui: &Rc<Ui>) -> ActionMenu {
    let mut menu = ActionMenu::new();
    let ui_c = ui.clone();
    menu.item(&gettext("Select All"), move || {
        ui_c.shared_by_me.files.selection.select_all();
    });
    let ui_c = ui.clone();
    menu.item(&gettext("Refresh"), move || reload_current_page(&ui_c));
    menu
}

/// Confirm, then remove every member, invitation and the public link.
pub(crate) fn prompt_stop_sharing(ui: &Rc<Ui>, entries: &[DirEntry]) {
    let body = match entries {
        // Translators: {name} is the shared item's name.
        [one] => gettext_f(
            "Everyone you invited loses access to “{name}”, pending invitations are withdrawn and its public link stops working.",
            &[("name", &one.name)],
        ),
        _ => ngettext_f(
            "Everyone you invited loses access to {n} item, pending invitations are withdrawn and its public link stops working.",
            "Everyone you invited loses access to {n} items, pending invitations are withdrawn and their public links stop working.",
            entries.len() as u64,
            &[],
        ),
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Stop Sharing?"))
        .body(body)
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("stop", &gettext("Stop Sharing"));
    dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui_c = ui.clone();
    let done = match entries {
        // Translators: {name} is the item's name.
        [one] => gettext_f("Stopped sharing {name}", &[("name", &one.name)]),
        _ => ngettext_f(
            "Stopped sharing {n} item",
            "Stopped sharing {n} items",
            entries.len() as u64,
            &[],
        ),
    };
    let uids: Vec<String> = entries.iter().map(|e| e.uid.clone()).collect();
    dialog.connect_response(Some("stop"), move |_, _| {
        let done = done.clone();
        run_requests(
            &ui_c,
            uids.iter()
                .map(|uid| Request::StopSharingByUid { uid: uid.clone() })
                .collect(),
            gettext_noop("Couldn't stop sharing"),
            move |ui| {
                reload_listing(ui);
                toast(ui, &done);
            },
        );
    });
    dialog.present(ui_window(ui).as_ref());
}

/// Open a shared item: a folder in My Files, a file the way My Files would.
fn open_shared_by_me(ui: &Rc<Ui>, entry: &DirEntry) {
    if entry.is_dir {
        open_in_my_files(ui, entry.path.clone());
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
    open_in_my_files(ui, parent);
}

/// Build a [`DirEntry`] from a [`SharedItem`], for the file list and the
/// Share dialog, which addresses a pathless node by uid.
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
        trashed_at: 0,
        trashed_from: None,
        issue: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(members: usize, invites: usize, link: Option<PublicLinkInfo>) -> SharedItem {
        SharedItem {
            uid: "vol~link".into(),
            name: "Budget.ods".into(),
            is_dir: false,
            modified: 0,
            path: "Work/Budget.ods".into(),
            member_count: members,
            invite_count: invites,
            link,
        }
    }

    fn link(expires: Option<i64>, has_password: bool) -> PublicLinkInfo {
        PublicLinkInfo {
            id: "l".into(),
            url: Some("https://drive.proton.me/urls/X#y".into()),
            role: "viewer".into(),
            expires,
            has_password,
            created: 0,
        }
    }

    #[test]
    fn a_link_says_whether_it_still_works() {
        assert_eq!(link_label(None, 100), "—");
        assert_eq!(link_label(Some(&link(None, false)), 100), "Active");
        assert_eq!(link_label(Some(&link(Some(100), false)), 100), "Expired");
        assert!(link_label(Some(&link(Some(200), false)), 100).starts_with("Expires "));
        assert_eq!(
            link_label(Some(&link(None, true)), 100),
            "Active · Password protected"
        );
    }

    #[test]
    fn the_filters_pick_links_people_and_expired_links() {
        let people = item(2, 0, None);
        let invited = item(0, 1, None);
        let live = item(0, 0, Some(link(None, false)));
        let expired = item(0, 0, Some(link(Some(50), false)));
        let pick = |filter| {
            [&people, &invited, &live, &expired]
                .iter()
                .map(|i| matches_filter(i, filter, 100))
                .collect::<Vec<_>>()
        };
        assert_eq!(pick(ShareFilter::All), [true, true, true, true]);
        assert_eq!(pick(ShareFilter::Links), [false, false, true, true]);
        assert_eq!(pick(ShareFilter::People), [true, true, false, false]);
        assert_eq!(pick(ShareFilter::Expired), [false, false, false, true]);
    }

    #[test]
    fn the_location_is_the_folder_in_my_files() {
        assert_eq!(location_label("Work/Budget.ods"), "Work");
        assert_eq!(location_label("Budget.ods"), "My files");
        assert_eq!(location_label(""), "—");
    }

    #[test]
    fn people_counts_members_and_pending_invitations() {
        assert_eq!(people_label(&item(0, 0, None)), "—");
        assert_eq!(people_label(&item(1, 0, None)), "1 person");
        assert_eq!(people_label(&item(2, 3, None)), "2 people · 3 pending");
    }
}
