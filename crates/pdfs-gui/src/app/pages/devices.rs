use crate::*;

pub(crate) struct DevicesState {
    // Computers page: one list of the account's computers, this one first, and
    // a read-only browser over another computer's backup.
    /// "list", "browse", or "status" while the page loads or can't be read.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) group: adw::PreferencesGroup,
    /// Rows added last time, removed before the next paint (adw groups have no
    /// clear-all).
    pub(crate) rows: RefCell<Vec<gtk4::Widget>>,
    /// The browse view's files.
    pub(crate) files: FileList,
    /// The browse view: "list", or "empty".
    pub(crate) files_content: gtk4::Stack,
    /// The breadcrumb trail from "Computers" down to the current folder.
    pub(crate) crumb: gtk4::Box,
    /// Grid or list for the browse view; hidden on the device list.
    pub(crate) layout: gtk4::Button,
    /// Where the browse view is, as `(uid, name)`: the computer first, then each
    /// folder below it. Empty = the device list. A backup has no path in the
    /// mount, so descending is uid-addressed and the stack is the breadcrumb.
    pub(crate) nav: RefCell<Vec<(String, String)>>,
    /// The [`DevicesState::nav`] the page on screen was painted for, or `None`
    /// before the first paint.
    pub(crate) listed: RefCell<Option<Vec<(String, String)>>>,
    /// Runs the loads; the rows stay up while one runs.
    pub(crate) loader: Rc<Loader>,
}

impl DevicesState {
    pub(crate) fn new(widgets: &DevicesWidgets) -> Self {
        DevicesState {
            content: widgets.content.clone(),
            status: widgets.status.clone(),
            retry: widgets.retry.clone(),
            group: widgets.group.clone(),
            rows: RefCell::new(Vec::new()),
            files: widgets.files.clone(),
            files_content: widgets.files_content.clone(),
            crumb: widgets.crumb.clone(),
            layout: widgets.layout.clone(),
            nav: RefCell::new(Vec::new()),
            listed: RefCell::new(None),
            loader: Loader::new(&widgets.content),
        }
    }
}

/// Widgets the Devices page's load/repaint touch.
pub(crate) struct DevicesWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    /// Every computer on the account, this one first.
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) files: FileList,
    pub(crate) files_content: gtk4::Stack,
    /// Goes up one folder, and from a computer's top level back to the list.
    pub(crate) back: gtk4::Button,
    pub(crate) crumb: gtk4::Box,
    pub(crate) layout: gtk4::Button,
    pub(crate) retry: gtk4::Button,
}

/// The Computers page: every computer backing up to the account, with this one
/// first. This computer's row leads to its folders on the Sync page; another
/// computer's row opens its backup read-only, in the same list as My files.
pub(crate) fn build_devices_page() -> (gtk4::Widget, DevicesWidgets) {
    let group = adw::PreferencesGroup::new();
    let clamp = adw::Clamp::builder().child(&group).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&clamp)
        .build();

    // Browse: a path bar over the list, as on Shared with me.
    let files = FileList::new();
    files.show_list(true);
    let layout = layout_button(&files);
    layout.set_visible(false);
    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text(gettext("Back"))
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
        .icon_name("folder-open-symbolic")
        .title(gettext("This folder is empty"))
        .description(gettext("Nothing in this folder has been backed up."))
        .vexpand(true)
        .build();
    files_empty.add_css_class("compact");
    let files_content = gtk4::Stack::new();
    files_content.add_named(&files.views, Some("list"));
    files_content.add_named(&files_empty, Some("empty"));
    let browse = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    browse.append(&path_bar);
    browse.append(&files_content);

    let retry = gtk4::Button::builder()
        .label(gettext("Retry"))
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("computer-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&scroll, Some("list"));
    content.add_named(&browse, Some("browse"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(18);
    inner.set_margin_bottom(18);
    inner.set_margin_start(18);
    inner.set_margin_end(18);
    inner.append(&content);
    let (frame, header, _) = page_frame(&gettext("Computers"), &inner);
    header.pack_end(&layout);

    (
        frame.upcast(),
        DevicesWidgets {
            content,
            status,
            group,
            files,
            files_content,
            back,
            crumb,
            layout,
            retry,
        },
    )
}

/// Install the browse view's file list and the page's buttons.
pub(crate) fn wire_devices(ui: &Rc<Ui>, widgets: &DevicesWidgets) {
    let files = &ui.devices.files;
    files.wire(
        ui,
        FileListBehavior {
            activate: open_device_entry,
            entry_menu: device_entry_menu,
            bulk_menu: device_bulk_menu,
            background_menu: device_background_menu,
            badges: false,
            drag_and_drop: false,
        },
    );
    files
        .column_view
        .append_column(&text_column(&pgettext("column", "Modified"), |e| {
            if e.modified > 0 {
                dates::short_date(e.modified)
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
        ui_back.devices.nav.borrow_mut().pop();
        load_devices(&ui_back);
    });
    let ui_retry = ui.clone();
    widgets
        .retry
        .connect_clicked(move |_| restart_service_then(&ui_retry, load_devices));
}

/// Show a status page in place of the Devices list.
pub(crate) fn devices_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.devices.status.set_icon_name(Some(icon));
    ui.devices.status.set_title(title);
    ui.devices.status.set_description(Some(description));
    ui.devices.retry.set_visible(retry);
    ui.devices.content.set_visible_child_name("status");
}

/// Load the view [`DevicesState::nav`] points at: the device list, or a folder
/// of another computer's backup.
pub(crate) fn load_devices(ui: &Rc<Ui>) {
    cancel_file_thumbnails(ui);
    let nav = ui.devices.nav.borrow().clone();
    ui.devices.layout.set_visible(!nav.is_empty());
    if !nav.is_empty() {
        load_device_folder(ui, &nav);
        return;
    }
    let ticket = begin_devices_load(ui, "computer-symbolic", gettext("Reading your computers."));
    ui.busy_begin();
    // The two lists are independent and the daemon serves requests concurrently,
    // so fire both up front and collect them, rather than paying two round trips
    // back to back.
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSyncFolders);
    let rx2 = spawn_request(ui.dirs.control_socket(), Request::ListDevices);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let sync = rx.recv().await;
        let devices_reply = rx2.recv().await;
        ui.busy_end();
        if !ticket.is_current() {
            return;
        }
        let folders = match sync {
            Ok(Ok(Response::SyncFolders { items })) => items,
            // The daemon answered but not as expected — treat as empty and carry
            // on to the devices list rather than failing the whole page.
            Ok(Ok(_)) => Vec::new(),
            Ok(Err(_)) | Err(_) => {
                devices_unreachable(&ui);
                return;
            }
        };
        let devices = match devices_reply {
            Ok(Ok(Response::Devices { items })) => items,
            _ => Vec::new(),
        };
        ui.devices.content.set_visible_child_name("list");
        repaint_devices(&ui, &devices, &folders);
        repaint_device_crumb(&ui, &[]);
        *ui.devices.listed.borrow_mut() = Some(Vec::new());
    });
}

/// List one folder of another computer's backup. The computer's own top level
/// is its restorable folders; below that, any folder lists by uid.
fn load_device_folder(ui: &Rc<Ui>, nav: &[(String, String)]) {
    let ticket = begin_devices_load(ui, "folder-symbolic", gettext("Reading this folder."));
    ui.busy_begin();
    let (uid, _) = nav.last().cloned().unwrap_or_default();
    let request = if nav.len() == 1 {
        Request::ListDeviceRestorableFolders { device: uid }
    } else {
        Request::ListSharedFolder { uid }
    };
    let rx = spawn_request(ui.dirs.control_socket(), request);
    let ui = ui.clone();
    let nav = nav.to_vec();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        if !ticket.is_current() {
            return;
        }
        let entries = match result {
            Ok(Ok(Response::Entries { entries })) => entries,
            Ok(Ok(Response::RestorableFolders { items })) => {
                items.into_iter().map(restorable_entry).collect()
            }
            Ok(Ok(Response::Error { message, kind })) => {
                // The folder or the computer is gone: fall back a level rather
                // than stranding the page on a dead uid.
                ui.devices.nav.borrow_mut().pop();
                toast_failure(&ui, &gettext("Couldn't open folder"), &message, kind);
                load_devices(&ui);
                return;
            }
            _ => {
                devices_unreachable(&ui);
                return;
            }
        };
        ui.devices.content.set_visible_child_name("browse");
        repaint_device_crumb(&ui, &nav);
        *ui.devices.listed.borrow_mut() = Some(nav);
        if entries.is_empty() {
            ui.devices.files.model.remove_all();
            ui.devices.files_content.set_visible_child_name("empty");
        } else {
            ui.devices.files_content.set_visible_child_name("list");
            replace_items(&ui.devices.files.model, &entries);
        }
    });
}

/// A computer's top-level folder as a browsable entry.
fn restorable_entry(folder: RestorableFolder) -> DirEntry {
    DirEntry {
        name: folder.name,
        is_dir: true,
        uid: folder.remote_uid,
        ..DirEntry::default()
    }
}

/// Start a load of the view [`DevicesState::nav`] points at. A reload of the
/// view on screen keeps it up and usable; moving elsewhere greys it out until
/// the new view arrives.
fn begin_devices_load(ui: &Rc<Ui>, icon: &'static str, description: String) -> LoadTicket {
    let ui_p = ui.clone();
    let placeholder = move || {
        devices_status(&ui_p, icon, &gettext("Loading…"), &description, false);
    };
    if *ui.devices.listed.borrow() == Some(ui.devices.nav.borrow().clone()) {
        ui.devices.loader.refresh(placeholder)
    } else {
        ui.devices.listed.borrow_mut().take();
        ui.devices.loader.replace(placeholder)
    }
}

/// The daemon didn't answer the Devices page.
pub(crate) fn devices_unreachable(ui: &Rc<Ui>) {
    service_unreachable(ui, "devices", devices_status, load_devices);
}

/// Rebuild the device list: this computer first, then the others.
///
/// This computer's row offers no Remove. Deleting a device deletes its root
/// folder — every file backed up from it — and for this machine that would also
/// pull the ground out from under its synced folders, so it is not offered as a
/// peer of "remove some other laptop". Removing this computer's backup means
/// removing its folders, each of which asks about its cloud copy on its own
/// terms.
pub(crate) fn repaint_devices(ui: &Rc<Ui>, devices: &[DeviceInfo], folders: &[SyncFolderInfo]) {
    for row in ui.devices.rows.borrow_mut().drain(..) {
        ui.devices.group.remove(&row);
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    let me = devices.iter().find(|d| d.this_device);
    rows.push(this_computer_row(ui, me, folders).upcast());
    let others: Vec<&DeviceInfo> = devices.iter().filter(|d| !d.this_device).collect();
    for dev in &others {
        rows.push(other_computer_row(ui, dev).upcast());
    }
    if others.is_empty() {
        let row = adw::ActionRow::builder()
            .title(gettext("No other computers"))
            .subtitle(gettext(
                "Install Proton Drive on another computer to see it here.",
            ))
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("computer-symbolic"));
        let get = gtk4::LinkButton::with_label(PROTON_DRIVE_DOWNLOAD, &gettext("Get Proton Drive"));
        get.set_valign(gtk4::Align::Center);
        row.add_suffix(&get);
        rows.push(row.upcast());
    }
    for row in &rows {
        ui.devices.group.add(row);
    }
    *ui.devices.rows.borrow_mut() = rows;
}

/// Where the Proton Drive apps for other systems are.
const PROTON_DRIVE_DOWNLOAD: &str = "https://proton.me/drive/download";

/// This computer's row: its device name with a "This computer" pill, and how
/// many folders it backs up. It leads to those folders on the Sync page, which
/// owns every local path. `me` is `None` until the daemon has registered this
/// machine as a device.
fn this_computer_row(
    ui: &Rc<Ui>,
    me: Option<&DeviceInfo>,
    folders: &[SyncFolderInfo],
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(me.map_or_else(|| gettext("This computer"), |d| d.name.clone()))
        .subtitle(this_computer_subtitle(folders))
        .activatable(true)
        .build();
    row.add_prefix(&gtk4::Image::from_icon_name("computer-symbolic"));
    if me.is_some() {
        let pill = gtk4::Label::new(Some(&gettext("This computer")));
        pill.add_css_class("role-pill");
        pill.add_css_class("caption");
        pill.set_valign(gtk4::Align::Center);
        row.add_suffix(&pill);
    }
    let mut menu = ActionMenu::new();
    if let Some(me) = me {
        let (ui_c, uid, name) = (ui.clone(), me.uid.clone(), me.name.clone());
        menu.item(&gettext("Rename…"), move || {
            prompt_rename_device(&ui_c, &uid, &name)
        });
    }
    let ui_c = ui.clone();
    menu.item(&gettext("Restore Folders…"), move || {
        prompt_restore_folders(&ui_c, None)
    });
    row.add_suffix(&menu.button());
    row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    let ui_c = ui.clone();
    row.connect_activated(move |_| {
        ui_c.locations.views.set_visible_child_name("folders");
        ui_c.stack.set_visible_child_name("locations");
    });
    row
}

/// Another computer's row. Opening it browses that computer's backup.
fn other_computer_row(ui: &Rc<Ui>, dev: &DeviceInfo) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&dev.name)
        .subtitle(device_subtitle(dev))
        .activatable(true)
        .build();
    row.add_prefix(&gtk4::Image::from_icon_name("computer-symbolic"));
    let (uid, name) = (dev.uid.clone(), dev.name.clone());
    let mut menu = ActionMenu::new();
    let (ui_c, uid_c, name_c) = (ui.clone(), uid.clone(), name.clone());
    menu.item(&gettext("Browse Files"), move || {
        browse_device(&ui_c, &uid_c, &name_c)
    });
    let (ui_c, uid_c, name_c) = (ui.clone(), uid.clone(), name.clone());
    menu.item(&gettext("Restore to This Computer…"), move || {
        prompt_restore_folders(&ui_c, Some((uid_c.clone(), name_c.clone())))
    });
    let (ui_c, uid_c, name_c) = (ui.clone(), uid.clone(), name.clone());
    menu.item(&gettext("Move to This Computer…"), move || {
        prompt_migrate_device(&ui_c, &uid_c, &name_c)
    });
    menu.section();
    let (ui_c, uid_c, name_c) = (ui.clone(), uid.clone(), name.clone());
    menu.item(&gettext("Rename…"), move || {
        prompt_rename_device(&ui_c, &uid_c, &name_c)
    });
    // Adoption is how a reinstalled or renamed machine re-attaches to the device
    // it used to be, instead of registering a duplicate. It only makes sense on
    // another computer's row.
    let (ui_c, uid_c, name_c) = (ui.clone(), uid.clone(), name.clone());
    menu.item(&gettext("Continue This Backup Here…"), move || {
        prompt_adopt_device(&ui_c, &uid_c, &name_c)
    });
    menu.section();
    let (ui_c, uid_c, name_c) = (ui.clone(), uid.clone(), name.clone());
    menu.item(&gettext("Remove…"), move || {
        prompt_remove_device(&ui_c, &uid_c, &name_c)
    });
    row.add_suffix(&menu.button());
    row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    let ui_c = ui.clone();
    row.connect_activated(move |_| browse_device(&ui_c, &uid, &name));
    row
}

/// Open another computer's backup at its top level.
fn browse_device(ui: &Rc<Ui>, uid: &str, name: &str) {
    *ui.devices.nav.borrow_mut() = vec![(uid.to_string(), name.to_string())];
    load_devices(ui);
}

/// Rebuild the breadcrumb trail for `nav`: "Computers", then the computer and
/// each folder down to the current one, which is a plain heading.
fn repaint_device_crumb(ui: &Rc<Ui>, nav: &[(String, String)]) {
    let crumb = &ui.devices.crumb;
    while let Some(child) = crumb.first_child() {
        crumb.remove(&child);
    }
    crumb.append(&device_crumb_node(
        ui,
        &gettext("Computers"),
        0,
        nav.is_empty(),
    ));
    for (i, (_, name)) in nav.iter().enumerate() {
        crumb.append(&crumb_separator());
        crumb.append(&device_crumb_node(ui, name, i + 1, i + 1 == nav.len()));
    }
}

/// One breadcrumb segment: a heading for the current folder, or a flat button
/// that goes back up to `depth` levels below the device list.
fn device_crumb_node(ui: &Rc<Ui>, label: &str, depth: usize, current: bool) -> gtk4::Widget {
    if current {
        return crumb_current(label);
    }
    let button = gtk4::Button::builder().label(label).build();
    button.add_css_class("flat");
    let ui = ui.clone();
    button.connect_clicked(move |_| {
        ui.devices.nav.borrow_mut().truncate(depth);
        load_devices(&ui);
    });
    button.upcast()
}

/// Folders open in place; files download and open with the default app.
fn open_device_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    if entry.is_dir {
        ui.devices
            .nav
            .borrow_mut()
            .push((entry.uid.clone(), entry.name.clone()));
        load_devices(ui);
    } else {
        open_shared_file(ui, &entry.uid, &entry.name);
    }
}

fn device_entry_menu(ui: &Rc<Ui>, entry: &DirEntry) -> ActionMenu {
    let mut menu = ActionMenu::new();
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    menu.item(&pgettext("verb", "Open"), move || {
        open_device_entry(&ui_c, &entry_c)
    });
    menu
}

fn device_bulk_menu(_ui: &Rc<Ui>, entries: Vec<DirEntry>) -> ActionMenu {
    let mut menu = ActionMenu::new();
    menu.labelled_section(&ngettext_f(
        "{n} selected",
        "{n} selected",
        entries.len() as u64,
        &[],
    ));
    menu
}

fn device_background_menu(ui: &Rc<Ui>) -> ActionMenu {
    let mut menu = ActionMenu::new();
    let ui_c = ui.clone();
    menu.item(&gettext("Select All"), move || {
        ui_c.devices.files.selection.select_all();
    });
    let ui_c = ui.clone();
    menu.item(&gettext("Refresh"), move || reload_current_page(&ui_c));
    menu
}

/// A device row's subtitle: its platform, and when it last backed up. A device
/// that never did says so — an unexplained missing date reads as a bug, and
/// "no backups yet" is the fact that tells the user this computer isn't backing
/// anything up.
pub(crate) fn device_subtitle(dev: &DeviceInfo) -> String {
    let platform = platform_label(&dev.device_type);
    match dev.last_sync {
        // Translators: {platform} is the device's platform (such as "Linux"), {time} a relative time such as "5 minutes ago".
        Some(secs) if secs > 0 => gettext_f(
            "{platform} · last backup {time}",
            &[("platform", platform), ("time", &dates::relative(secs))],
        ),
        // Translators: {platform} is the device's platform, such as "Linux".
        _ => gettext_f("{platform} · no backups yet", &[("platform", platform)]),
    }
}

/// The platform as its maker spells it. The API's names are enum variants.
fn platform_label(device_type: &str) -> &str {
    match device_type {
        "MacOs" => "macOS",
        other => other,
    }
}

/// How many folders this machine backs up, and whether any of them needs
/// attention — the one fact worth surfacing away from the folder list itself.
pub(crate) fn this_computer_subtitle(folders: &[SyncFolderInfo]) -> String {
    if folders.is_empty() {
        return gettext("No folders backed up yet");
    }
    let count = ngettext_f(
        "{n} folder backed up",
        "{n} folders backed up",
        folders.len() as u64,
        &[],
    );
    let attention = folders
        .iter()
        .filter(|f| f.state == "error" || f.state == "conflict")
        .count();
    match attention {
        0 => count,
        // Translators: {folders} is "N folders backed up"; {n} counts the folders with a problem.
        n => ngettext_f(
            "{folders} · {n} needs attention",
            "{folders} · {n} need attention",
            n as u64,
            &[("folders", &count)],
        ),
    }
}

/// Human label for a synced folder's `state` column.
pub(crate) fn sync_state_label(state: &str) -> String {
    match state {
        "syncing" => gettext("syncing…"),
        "error" => gettext("sync error"),
        "conflict" => gettext("needs attention"),
        _ => gettext("up to date"),
    }
}

/// Human label for a sync pass in flight: what it is doing, to which file, and
/// how far along it is. Neither phase's total is exact — the scan's is an estimate
/// from the last pass, and the applying total grows as deeper paths are classified
/// — so both read "12 of 40" rather than a percentage that could go backwards.
pub(crate) fn sync_progress_label(p: &SyncProgress) -> String {
    match p.phase {
        // Before the first pass finishes there is no estimate, so the count would
        // be "checked 12 of 12" — worse than saying nothing.
        SyncPhase::Scanning if p.total == 0 => gettext("checking for changes…"),
        // Translators: {done} and {total} count folders and files checked so far.
        SyncPhase::Scanning => gettext_f(
            "checking for changes — {done} of {total}",
            &[
                ("done", &p.done.to_string()),
                ("total", &p.total.max(p.done).to_string()),
            ],
        ),
        SyncPhase::Applying => {
            let done = (p.done + 1).to_string();
            let total = p.total.max(p.done + 1).to_string();
            if p.current.is_empty() {
                // Translators: {done} and {total} count the changes applied so far.
                gettext_f(
                    "syncing {done} of {total}",
                    &[("done", &done), ("total", &total)],
                )
            } else {
                // Translators: {file} is the path being synced; {done} and {total} count the changes applied so far.
                gettext_f(
                    "syncing {file} — {done} of {total}",
                    &[("file", &p.current), ("done", &done), ("total", &total)],
                )
            }
        }
    }
}

/// Pick a local folder and hand it to the daemon to sync to this device.
pub(crate) fn prompt_add_sync_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = gtk4::FileDialog::builder()
        .title(gettext("Add Folder to Sync"))
        .build();
    let ui = ui.clone();
    dialog.select_folder(win.as_ref(), gio::Cancellable::NONE, move |res| {
        let Ok(folder) = res else { return };
        let Some(local_path) = folder.path().and_then(|p| p.to_str().map(str::to_string)) else {
            return;
        };
        let rx = spawn_request(
            ui.dirs.control_socket(),
            Request::AddSyncFolder {
                local_path: local_path.clone(),
            },
        );
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(Response::Ok { .. })) => {
                    // The daemon acks before the row exists — registering the
                    // device and creating the remote folder is off-socket network
                    // work. A fixed delay here either fires too early (empty list)
                    // or too late (row flashes in). Instead let the periodic
                    // `refresh_locations` tick pick the row up whenever it
                    // actually lands, which is what keeps a running pass live too.
                    toast(&ui, &gettext("Syncing folder…"));
                }
                Ok(Ok(Response::Error { message, kind })) => {
                    toast_failure(&ui, &gettext("Couldn't add folder"), &message, kind)
                }
                _ => toast_error(
                    &ui,
                    &gettext("Couldn't add folder"),
                    &gettext("The Proton Drive service didn't respond."),
                ),
            }
        });
    });
}

/// Ask the daemon what a device holds, then show a picker mapping each remote
/// folder onto a local directory. `device` is another computer as `(uid, name)`;
/// `None` is this machine's own device.
///
/// The daemon's paths are proposals — from the device's `profile.json` when it
/// makes sense here, else `~/<name>` — so every one of them is editable and
/// nothing is restored without being ticked.
pub(crate) fn prompt_restore_folders(ui: &Rc<Ui>, device: Option<(String, String)>) {
    ui.busy_begin();
    let request = match &device {
        Some((uid, _)) => Request::ListDeviceRestorableFolders {
            device: uid.clone(),
        },
        None => Request::ListRestorableFolders,
    };
    let rx = spawn_request(ui.dirs.control_socket(), request);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::RestorableFolders { items })) => {
                show_restore_picker(&ui, items, device)
            }
            Ok(Ok(Response::Error { message, kind })) => toast_failure(
                &ui,
                &gettext("Couldn't list folders to restore"),
                &message,
                kind,
            ),
            _ => toast_error(
                &ui,
                &gettext("Couldn't list folders to restore"),
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
    });
}

/// The restore picker itself: one editable row per restorable folder.
fn show_restore_picker(
    ui: &Rc<Ui>,
    items: Vec<RestorableFolder>,
    device: Option<(String, String)>,
) {
    let win = ui_window(ui);
    let candidates: Vec<RestorableFolder> =
        items.into_iter().filter(|f| !f.already_synced).collect();
    if candidates.is_empty() {
        toast(
            ui,
            &nothing_to_restore(device.as_ref().map(|(_, n)| n.as_str())),
        );
        return;
    }

    let group = adw::PreferencesGroup::builder()
        .description(gettext(
            "Tick the folders to sync to this machine, and adjust where each one should live.",
        ))
        .build();
    // Held so the response handler can read back what the user ticked and typed.
    let mut controls: Vec<(String, String, gtk4::CheckButton, adw::EntryRow)> = Vec::new();
    for f in candidates {
        let check = gtk4::CheckButton::builder()
            .active(true)
            .valign(gtk4::Align::Center)
            .build();
        let row = adw::EntryRow::builder().title(&f.name).build();
        row.set_text(&f.local_path);
        row.add_prefix(&check);
        group.add(&row);
        controls.push((f.remote_uid, f.mode, check, row));
    }

    let scroll = gtk4::ScrolledWindow::builder()
        .propagate_natural_height(true)
        .max_content_height(420)
        .child(&group)
        .build();
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Restore Folders"))
        .body(restore_picker_body(
            device.as_ref().map(|(_, n)| n.as_str()),
        ))
        .extra_child(&scroll)
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("restore", &gettext("Restore"));
    dialog.set_response_appearance("restore", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("restore"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "restore" {
            return;
        }
        let items: Vec<RestoreItem> = controls
            .iter()
            .filter(|(_, _, check, _)| check.is_active())
            .map(|(uid, mode, _, row)| RestoreItem {
                remote_uid: uid.clone(),
                local_path: row.text().to_string(),
                mode: mode.clone(),
            })
            .collect();
        if items.is_empty() {
            return;
        }
        // The daemon acks before the downloads finish, like AddSyncFolder — the
        // rows appear as the periodic refresh picks them up.
        run_devices_mutation(
            &ui,
            match &device {
                Some((uid, _)) => Request::RestoreDeviceFolders {
                    device: uid.clone(),
                    items,
                },
                None => Request::RestoreSyncFolders { items },
            },
            &gettext("Restoring folders…"),
            &gettext("Couldn't restore folders"),
        );
    });
    dialog.present(win.as_ref());
}

/// The restore picker's body: whose backup the folders come from.
pub(crate) fn restore_picker_body(device: Option<&str>) -> String {
    match device {
        // Translators: {name} is the name of another computer.
        Some(name) => gettext_f(
            "These folders are backed up from “{name}”. Restored folders keep syncing with that computer's copy.",
            &[("name", name)],
        ),
        None => gettext("These folders are backed up under this computer in Proton Drive."),
    }
}

/// The toast when every folder of a device is already synced here.
pub(crate) fn nothing_to_restore(device: Option<&str>) -> String {
    match device {
        // Translators: {name} is the name of another computer.
        Some(name) => gettext_f(
            "Nothing to restore — “{name}”'s folders are all synced here.",
            &[("name", name)],
        ),
        None => gettext("Nothing to restore — this computer's folders are all synced here."),
    }
}

/// Flip a synced folder between `mirror` and `ondemand`. Reloads after so the
/// row's subtitle and switch reflect the daemon's real state (the request may be
/// rejected, e.g. switching to on-demand while a folder is mid-sync).
pub(crate) fn set_sync_folder_mode(ui: &Rc<Ui>, id: i64, mode: &'static str) {
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::SetSyncFolderMode {
            id,
            mode: mode.to_string(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            // The daemon's own text is written for the log; the toast says what
            // the person will see happen. A switch may be queued behind a running
            // pass, hence "will".
            Ok(Ok(Response::Ok { .. })) => toast(
                &ui,
                &if mode == "ondemand" {
                    gettext("Folder will switch to online only")
                } else {
                    gettext("Folder will download and stay synced")
                },
            ),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &gettext("Couldn't change mode"), &message, kind)
            }
            _ => toast_error(
                &ui,
                &gettext("Couldn't change mode"),
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
        reload_sync_pages(&ui);
    });
}

/// Repaint whichever of the two pages that show sync folders is on screen. Sync
/// folder changes are raised from Locations as well as Computers, and both must
/// show the daemon's answer at once rather than on the next tick — a rejected
/// mode switch would otherwise stay visibly flipped.
pub(crate) fn reload_sync_pages(ui: &Rc<Ui>) {
    match ui.stack.visible_child_name().as_deref() {
        Some("devices") => load_devices(ui),
        Some("locations") => refresh_locations(ui),
        _ => {}
    }
}

/// Confirm, then stop syncing a folder. Offers to also delete the cloud copy.
pub(crate) fn prompt_remove_sync_folder(ui: &Rc<Ui>, id: i64, path: &str, ondemand: bool) {
    let win = ui_window(ui);
    // The two modes leave the user in opposite places, so they can't share a
    // sentence. A mirror folder's files are already on this disk and stay there.
    // An on-demand folder's are not: the path is a mount over content that lives
    // in Proton Drive, so unmounting it leaves an empty directory — which is a
    // nasty surprise if the dialog claimed the local files were safe, and is
    // recoverable only by turning On-demand off *first* and letting it download.
    let body = if ondemand {
        // Translators: {path} is a local folder path.
        gettext_f(
            "Stop syncing “{path}”?\n\nThis folder is online only: its files live in Proton Drive, not on this disk, so the folder will be empty once it stops syncing. To keep a local copy, cancel, switch the folder to Synced, and wait for the download to finish before removing it.",
            &[("path", path)],
        )
    } else {
        // Translators: {path} is a local folder path.
        gettext_f(
            "Stop syncing “{path}”?\n\nThe local files stay on this disk and simply stop being synced. Choose whether to also delete the copy in Proton Drive.",
            &[("path", path)],
        )
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Stop Syncing Folder?"))
        .body(body)
        .build();
    let group = adw::PreferencesGroup::new();
    let delete_remote = adw::SwitchRow::builder()
        .title(gettext("Also delete from Proton Drive"))
        .subtitle(if ondemand {
            gettext("Deletes the only copy of these files.")
        } else {
            gettext("The local copy is unaffected.")
        })
        .active(false)
        .build();
    group.add(&delete_remote);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("remove", &gettext("Stop Syncing"));
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "remove" {
            run_devices_mutation(
                &ui,
                Request::RemoveSyncFolder {
                    id,
                    delete_remote: delete_remote.is_active(),
                },
                &gettext("Stopped syncing folder"),
                &gettext("Couldn't stop syncing the folder"),
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Prompt for a new device name and rename it.
pub(crate) fn prompt_rename_device(ui: &Rc<Ui>, uid: &str, current: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Rename Computer"))
        // Translators: {name} is the computer's current name.
        .body(gettext_f("Rename “{name}”.", &[("name", current)]))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title(gettext("New name"))
        .activates_default(true)
        .build();
    row.set_text(current);
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("rename", &gettext("Rename"));
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp != "rename" {
            return;
        }
        let name = row.text().trim().to_string();
        if name.is_empty() {
            toast_error(
                &ui,
                &gettext("Couldn't rename the computer"),
                &gettext("A name is required."),
            );
            return;
        }
        run_devices_mutation(
            &ui,
            Request::RenameDevice {
                uid: uid.clone(),
                name,
            },
            &gettext("Computer renamed"),
            &gettext("Couldn't rename the computer"),
        );
    });
    dialog.present(win.as_ref());
}

/// Confirm, then remove (deregister) a computer. Removing deletes everything the
/// computer backed up, so the confirm button stays off until the name is typed.
pub(crate) fn prompt_remove_device(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let confirm = adw::EntryRow::builder()
        // Translators: {name} is the computer's name, which the user must type.
        .title(gettext_f("Type “{name}” to confirm", &[("name", name)]))
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&confirm);
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Remove Computer?"))
        // Translators: {name} is the name of the computer being removed.
        .body(gettext_f(
            "Remove “{name}” from this account?\n\nEverything it backed up to Proton Drive is deleted along with it. The files on that computer itself are not touched — but this cannot be undone from here.",
            &[("name", name)],
        ))
        .extra_child(&group)
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("remove", &gettext("Remove"));
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_response_enabled("remove", false);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let dialog_typed = dialog.clone();
    let expected = name.to_string();
    confirm.connect_changed(move |row| {
        dialog_typed.set_response_enabled("remove", row.text().trim() == expected);
    });
    let ui = ui.clone();
    let uid = uid.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp == "remove" {
            run_devices_mutation(
                &ui,
                Request::DeleteDevice { uid: uid.clone() },
                &gettext("Computer removed"),
                &gettext("Couldn't remove the computer"),
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Confirm, then continue another computer's backup on this machine (adopt its
/// device).
///
/// Worth a confirmation rather than a plain click: adoption re-points this
/// machine's syncing at another computer's device folder, and the folders
/// already synced here keep pointing at the old one until they are removed. The
/// dialog says so, because the alternative is a user discovering it by watching
/// their folders diverge.
pub(crate) fn prompt_adopt_device(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Continue This Backup Here?"))
        // Translators: {name} is the name of the other computer whose backup this one takes over.
        .body(gettext_f(
            "This computer takes over the backup of “{name}”, for example after a reinstall.\n\nNew synced folders go under “{name}” in Proton Drive, even if this computer's name changes. Folders already synced here are not moved. Use “Restore to This Computer” afterwards to bring back what “{name}” was syncing.",
            &[("name", name)],
        ))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("adopt", &gettext("Continue Here"));
    dialog.set_response_appearance("adopt", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    // Translators: {name} is the name of the computer whose backup this one took over.
    let done = gettext_f("Now backing up as “{name}”", &[("name", name)]);
    dialog.connect_response(None, move |_, resp| {
        if resp == "adopt" {
            run_devices_mutation(
                &ui,
                Request::AdoptDevice {
                    uid: Some(uid.clone()),
                },
                &done,
                &gettext("Couldn't continue that backup here"),
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Ask before moving another computer's folders into this computer's backup,
/// then offer them for syncing here.
///
/// Destructive-looking on purpose: the other computer keeps its device but
/// loses its folders, and a copy of Proton Drive still running there would
/// watch them vanish.
pub(crate) fn prompt_migrate_device(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Move Backup to This Computer?"))
        // Translators: {name} is the name of the other computer whose folders move to this one.
        .body(gettext_f(
            "The folders backed up by “{name}” move into this computer's backup in Proton Drive, and you choose where they go on this computer. “{name}” stays in your account, without those folders.\n\nStop Proton Drive on “{name}” first. Otherwise it sees its folders disappear.",
            &[("name", name)],
        ))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("move", &gettext("Move Here"));
    dialog.set_response_appearance("move", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    // Translators: {name} is the name of the computer whose folders moved to this one.
    let done = gettext_f("Moved the backup of “{name}”", &[("name", name)]);
    dialog.connect_response(None, move |_, resp| {
        if resp != "move" {
            return;
        }
        ui.busy_begin();
        let rx = spawn_request(
            ui.dirs.control_socket(),
            Request::MigrateDevice { uid: uid.clone() },
        );
        let (ui, done) = (ui.clone(), done.clone());
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            ui.busy_end();
            let failed = gettext("Couldn't move that backup");
            match result {
                Ok(Ok(Response::Ok { .. })) => {
                    reload_sync_pages(&ui);
                    toast(&ui, &done);
                    // The folders are this computer's now, so its own restore
                    // picker is where the user chooses their local paths.
                    prompt_restore_folders(&ui, None);
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
    });
    dialog.present(win.as_ref());
}

/// Run a mutation raised from Computers or Locations and reload the page on success.
pub(crate) fn run_devices_mutation(ui: &Rc<Ui>, req: Request, done: &str, failed: &str) {
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
                reload_sync_pages(&ui);
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

    fn folder(state: &str) -> SyncFolderInfo {
        SyncFolderInfo {
            id: 1,
            local_path: "/home/u/Documents".into(),
            remote_uid: "vol~link".into(),
            mode: "mirror".into(),
            state: state.into(),
            last_sync: 0,
            pending_mode: None,
            progress: None,
            paused: false,
        }
    }

    #[test]
    fn a_machine_with_no_folders_says_so() {
        assert_eq!(this_computer_subtitle(&[]), "No folders backed up yet");
    }

    #[test]
    fn folders_needing_attention_are_counted_not_hidden() {
        // The per-folder rows live on the Sync page, so this row is the only
        // place the Computers page can surface that something is wrong.
        assert_eq!(
            this_computer_subtitle(&[folder("idle"), folder("idle")]),
            "2 folders backed up"
        );
        assert_eq!(
            this_computer_subtitle(&[folder("idle"), folder("error")]),
            "2 folders backed up · 1 needs attention"
        );
        assert_eq!(
            this_computer_subtitle(&[folder("conflict"), folder("error")]),
            "2 folders backed up · 2 need attention"
        );
    }

    #[test]
    fn the_restore_picker_names_the_computer_it_restores_from() {
        assert!(restore_picker_body(Some("laptop")).contains("“laptop”"));
        assert!(restore_picker_body(None).contains("this computer"));
        assert!(nothing_to_restore(Some("laptop")).contains("“laptop”"));
    }

    fn device(device_type: &str, last_sync: Option<i64>) -> DeviceInfo {
        DeviceInfo {
            uid: "dev".into(),
            name: "laptop".into(),
            device_type: device_type.into(),
            last_sync,
            this_device: false,
            adopted: false,
        }
    }

    #[test]
    fn a_computer_that_never_backed_up_says_no_backups_yet() {
        assert_eq!(
            device_subtitle(&device("Linux", None)),
            "Linux · no backups yet"
        );
        assert_eq!(
            device_subtitle(&device("Linux", Some(0))),
            "Linux · no backups yet"
        );
        assert!(device_subtitle(&device("MacOs", Some(1))).starts_with("macOS · last backup "));
    }
}
