use crate::*;

pub(crate) struct DevicesState {
    // Devices page: two sections (synced folders + other devices), rebuilt
    // wholesale on each load.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) rows: RefCell<Vec<gtk4::Widget>>,
    pub(crate) sync_group: adw::PreferencesGroup,
    /// The single "This computer" summary row. The per-folder rows moved to the
    /// Locations page, which owns every local path.
    pub(crate) sync_rows: RefCell<Vec<gtk4::Widget>>,
    /// "Rename" action in the "This computer" header. Insensitive until the
    /// device list identifies this machine's own device.
    pub(crate) rename_this: gtk4::Button,
    /// This machine's own device `(uid, name)`, from the last device list. The
    /// current device is filtered out of the "Other computers" rows, so this is
    /// where its rename target lives. `None` until identified.
    pub(crate) this_device: RefCell<Option<(String, String)>>,
    pub(crate) inflight: Cell<bool>,
    pub(crate) loaded_at: Cell<Option<Instant>>,
}

/// Widgets the Devices page's load/repaint touch.
pub(crate) struct DevicesWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    /// "This computer" — this machine's device identity and a pointer to its
    /// folders on the Locations page.
    pub(crate) sync_group: adw::PreferencesGroup,
    /// "Rename" in the "This computer" header — renames this machine's device.
    pub(crate) rename_this: gtk4::Button,
    /// "Other computers" — the account's *other* registered devices. This
    /// machine's own device is deliberately not among them; see
    /// [`repaint_devices`].
    pub(crate) group: adw::PreferencesGroup,
    /// "Restore Folders" — re-attach this device's remote folders to local
    /// directories after adopting it on a new machine.
    pub(crate) restore: gtk4::Button,
    pub(crate) retry: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
}

/// The Computers page: a "This computer" section naming the device this machine
/// backs up to (renamable from its header), plus an "Other computers" section
/// listing the account's other registered devices. The synced folders
/// themselves live on the Sync page.
pub(crate) fn build_devices_page() -> (gtk4::Widget, DevicesWidgets) {
    let restore = gtk4::Button::builder()
        .label("Restore Folders…")
        .tooltip_text("Sync this computer's Drive folders back to local directories")
        .build();
    let refresh = refresh_button();

    // This page is about device *identity* — which computer this is, which other
    // computers back up to the account, and how to adopt one. The folders
    // themselves are local paths, so they live on the Sync page; keeping a
    // second copy of that list here would be two places to change the same mode.
    let sync_group = adw::PreferencesGroup::builder()
        .title("This computer")
        .description("The computer this machine backs up as.")
        .build();
    // Rename control for *this* machine's device, in the section header. The
    // current device is filtered out of "Other computers", so this is the only
    // place it can be renamed; insensitive until the device list identifies it.
    let rename_this = gtk4::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Rename this computer")
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    rename_this.add_css_class("flat");
    sync_group.set_header_suffix(Some(&rename_this));
    let group = adw::PreferencesGroup::builder()
        .title("Other computers")
        .description("Other computers backing up to this account.")
        .build();

    let groups = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    groups.append(&sync_group);
    groups.append(&group);
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
        .icon_name("computer-symbolic")
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
    let (frame, header, _) = page_frame("Computers", &inner);
    header.pack_start(&restore);
    header.pack_end(&refresh);

    (
        frame.upcast(),
        DevicesWidgets {
            content,
            status,
            sync_group,
            rename_this,
            group,
            restore,
            retry,
            refresh,
        },
    )
}

/// Install the Devices page's retry button and the "Restore Folders" action.
pub(crate) fn wire_devices(ui: &Rc<Ui>, retry: &gtk4::Button, restore: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_devices(&ui_retry);
    });
    let ui_restore = ui.clone();
    restore.connect_clicked(move |_| prompt_restore_folders(&ui_restore));
    let ui_ren = ui.clone();
    ui.devices.rename_this.connect_clicked(move |_| {
        if let Some((uid, name)) = ui_ren.devices.this_device.borrow().clone() {
            prompt_rename_device(&ui_ren, &uid, &name);
        }
    });
}

/// Show a status page in place of the Devices list.
pub(crate) fn devices_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.devices.status.set_icon_name(Some(icon));
    ui.devices.status.set_title(title);
    ui.devices.status.set_description(Some(description));
    ui.devices.retry.set_visible(retry);
    ui.devices.content.set_visible_child_name("status");
}

/// Fetch this machine's synced folders and the account's other devices, then
/// repaint both sections. The two requests are chained so a single unreachable
/// daemon collapses the whole page to a status view.
pub(crate) fn load_devices(ui: &Rc<Ui>) {
    if ui.devices.inflight.get() {
        return;
    }
    ui.devices.inflight.set(true);
    devices_status(
        ui,
        "computer-symbolic",
        "Loading…",
        "Reading your computers.",
        false,
    );
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
        let folders = match sync {
            Ok(Ok(Response::SyncFolders { items })) => items,
            // The daemon answered but not as expected — treat as empty and carry
            // on to the devices list rather than failing the whole page.
            Ok(Ok(_)) => Vec::new(),
            Ok(Err(_)) | Err(_) => {
                ui.busy_end();
                ui.devices.inflight.set(false);
                ui.devices.loaded_at.set(None);
                devices_unreachable(&ui);
                return;
            }
        };
        // Daemon reachable: show the list and paint the "This computer" section.
        ui.devices.content.set_visible_child_name("list");
        repaint_this_computer(&ui, &folders);

        let devices = match devices_reply {
            Ok(Ok(Response::Devices { items })) => items,
            _ => Vec::new(),
        };
        ui.busy_end();
        ui.devices.inflight.set(false);
        repaint_devices(&ui, &devices);
        ui.devices.loaded_at.set(Some(Instant::now()));
    });
}

/// The daemon didn't answer the Devices page.
pub(crate) fn devices_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        devices_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    devices_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("devices") {
            load_devices(&ui);
        }
    });
}

/// Rebuild the "Other computers" section from a fresh listing. Empty is a normal
/// state (this machine may be the only device), so it shows a placeholder row
/// rather than collapsing the page.
///
/// This machine's own device is filtered out. Deleting a device deletes its root
/// folder — every file backed up from it — and for *this* machine that would also
/// pull the ground out from under the synced folders listed directly above, so it
/// is not offered as a peer of "remove some other laptop". Removing this
/// computer's backup means removing its folders, each of which asks about its
/// cloud copy on its own terms.
pub(crate) fn repaint_devices(ui: &Rc<Ui>, devices: &[DeviceInfo]) {
    for row in ui.devices.rows.borrow_mut().drain(..) {
        ui.devices.group.remove(&row);
    }
    // Identify this machine's own device so the "This computer" header's Rename
    // action has a target. It never appears in the rows below, so this is the
    // only place its name/uid is captured.
    match devices.iter().find(|d| d.this_device) {
        Some(me) => {
            *ui.devices.this_device.borrow_mut() = Some((me.uid.clone(), me.name.clone()));
            ui.devices
                .sync_group
                .set_description(Some(&format!("Backing up as “{}”.", me.name)));
            ui.devices.rename_this.set_sensitive(true);
            ui.devices
                .rename_this
                .set_tooltip_text(Some(&format!("Rename this computer ({})", me.name)));
        }
        None => {
            *ui.devices.this_device.borrow_mut() = None;
            ui.devices.rename_this.set_sensitive(false);
        }
    }
    let others: Vec<&DeviceInfo> = devices.iter().filter(|d| !d.this_device).collect();
    if others.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No other computers")
            .subtitle("Desktop apps syncing to this account appear here.")
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("computer-symbolic"));
        ui.devices.group.add(&row);
        *ui.devices.rows.borrow_mut() = vec![row.upcast()];
        return;
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for dev in others {
        let row = adw::ActionRow::builder()
            .title(&dev.name)
            .subtitle(device_subtitle(dev))
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("computer-symbolic"));
        // Adoption is how a reinstalled or renamed machine re-attaches to the
        // device it used to be, instead of registering a duplicate. It only
        // makes sense on *another* computer's row, which is the only place this
        // loop paints.
        let (ui_ren, uid_ren, name_ren) = (ui.clone(), dev.uid.clone(), dev.name.clone());
        let (ui_ad, uid_ad, name_ad) = (ui.clone(), dev.uid.clone(), dev.name.clone());
        let (ui_rm, uid_rm, name_rm) = (ui.clone(), dev.uid.clone(), dev.name.clone());
        row.add_suffix(&more_menu_button(vec![
            (
                "Rename…",
                "document-edit-symbolic",
                Box::new(move || prompt_rename_device(&ui_ren, &uid_ren, &name_ren)),
            ),
            (
                "Use This Computer's Identity…",
                "insert-object-symbolic",
                Box::new(move || prompt_adopt_device(&ui_ad, &uid_ad, &name_ad)),
            ),
            (
                "Remove Computer…",
                "user-trash-symbolic",
                Box::new(move || prompt_remove_device(&ui_rm, &uid_rm, &name_rm)),
            ),
        ]));
        ui.devices.group.add(&row);
        rows.push(row.upcast());
    }
    *ui.devices.rows.borrow_mut() = rows;
}

/// A device row's subtitle: its platform, and when it last synced. A device that
/// has never synced says so — an unexplained missing date reads as a bug, and
/// "never" is the fact that tells the user this computer isn't backing anything up.
pub(crate) fn device_subtitle(dev: &DeviceInfo) -> String {
    match dev.last_sync {
        Some(secs) if secs > 0 => {
            format!("{} · last synced {}", dev.device_type, activity_time(secs))
        }
        _ => format!("{} · never synced", dev.device_type),
    }
}

/// Rebuild the "This computer" section: one row naming the device this machine
/// backs up to, and how many folders it carries.
///
/// The per-folder rows (mode switch, sync now, remove) live on the Locations
/// page. They are local paths, and Locations is the one page that owns those;
/// two lists of the same folders would be two places to flip the same switch.
pub(crate) fn repaint_this_computer(ui: &Rc<Ui>, folders: &[SyncFolderInfo]) {
    for row in ui.devices.sync_rows.borrow_mut().drain(..) {
        ui.devices.sync_group.remove(&row);
    }
    let row = adw::ActionRow::builder()
        .title("This computer")
        .subtitle(this_computer_subtitle(folders))
        .build();
    row.add_prefix(&gtk4::Image::from_icon_name("computer-symbolic"));
    let manage = gtk4::Button::builder()
        .label("Open Sync")
        .tooltip_text("Manage this computer's folders and mountpoint")
        .valign(gtk4::Align::Center)
        .build();
    manage.add_css_class("flat");
    let ui_go = ui.clone();
    manage.connect_clicked(move |_| ui_go.stack.set_visible_child_name("locations"));
    row.add_suffix(&manage);
    ui.devices.sync_group.add(&row);
    *ui.devices.sync_rows.borrow_mut() = vec![row.upcast()];
}

/// How many folders this machine backs up, and whether any of them needs
/// attention — the one fact worth surfacing away from the folder list itself.
pub(crate) fn this_computer_subtitle(folders: &[SyncFolderInfo]) -> String {
    if folders.is_empty() {
        return "No folders backed up yet — add one on the Sync page.".to_string();
    }
    let count = match folders.len() {
        1 => "1 folder backed up".to_string(),
        n => format!("{n} folders backed up"),
    };
    let attention = folders
        .iter()
        .filter(|f| f.state == "error" || f.state == "conflict")
        .count();
    match attention {
        0 => format!("{count} · manage them on the Sync page"),
        1 => format!("{count} · 1 needs attention"),
        n => format!("{count} · {n} need attention"),
    }
}

/// Human label for a synced folder's `state` column.
pub(crate) fn sync_state_label(state: &str) -> &str {
    match state {
        "syncing" => "syncing…",
        "error" => "sync error",
        "conflict" => "needs attention",
        _ => "up to date",
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
        SyncPhase::Scanning if p.total == 0 => "checking for changes…".to_string(),
        SyncPhase::Scanning => format!(
            "checking for changes — {} of {}",
            p.done,
            p.total.max(p.done)
        ),
        SyncPhase::Applying => {
            let count = format!("{} of {}", p.done + 1, p.total.max(p.done + 1));
            if p.current.is_empty() {
                format!("syncing {count}")
            } else {
                format!("syncing {} — {count}", p.current)
            }
        }
    }
}

/// Pick a local folder and hand it to the daemon to sync to this device.
pub(crate) fn prompt_add_sync_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = gtk4::FileDialog::builder()
        .title("Add Folder to Sync")
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
                    toast(&ui, "Syncing folder…");
                }
                Ok(Ok(Response::Error { message, kind })) => {
                    toast_failure(&ui, "Couldn't add folder", &message, kind)
                }
                _ => toast_error(
                    &ui,
                    "Couldn't add folder",
                    "The mount service didn't respond.",
                ),
            }
        });
    });
}

/// Ask the daemon what this machine's device holds, then show a picker mapping
/// each remote folder onto a local directory.
///
/// The daemon's paths are proposals — from the device's `profile.json` when it
/// makes sense here, else `~/<name>` — so every one of them is editable and
/// nothing is restored without being ticked.
pub(crate) fn prompt_restore_folders(ui: &Rc<Ui>) {
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListRestorableFolders);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::RestorableFolders { items })) => show_restore_picker(&ui, items),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't list folders to restore", &message, kind)
            }
            _ => toast_error(
                &ui,
                "Couldn't list folders to restore",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// The restore picker itself: one editable row per restorable folder.
fn show_restore_picker(ui: &Rc<Ui>, items: Vec<RestorableFolder>) {
    let win = ui_window(ui);
    let candidates: Vec<RestorableFolder> =
        items.into_iter().filter(|f| !f.already_synced).collect();
    if candidates.is_empty() {
        toast(
            ui,
            "Nothing to restore — this computer's folders are all synced here.",
        );
        return;
    }

    let group = adw::PreferencesGroup::builder()
        .description(
            "Tick the folders to sync to this machine, and adjust where each one should live.",
        )
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
        .heading("Restore folders")
        .body("These folders are backed up under this computer in Proton Drive.")
        .extra_child(&scroll)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("restore", "Restore");
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
            Request::RestoreSyncFolders { items },
            "Restoring folders…",
            "Couldn't restore folders",
        );
    });
    dialog.present(win.as_ref());
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
                if mode == "ondemand" {
                    "Folder will switch to on-demand"
                } else {
                    "Folder will download and stay synced"
                },
            ),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't change mode", &message, kind)
            }
            _ => toast_error(
                &ui,
                "Couldn't change mode",
                "The mount service didn't respond.",
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
        format!(
            "Stop syncing “{path}”?\n\nThis folder is on-demand: its files live in Proton \
             Drive, not on this disk, so the folder will be empty once it is unmounted. To \
             keep a local copy, cancel, turn off On-demand, and wait for the download to \
             finish before removing it."
        )
    } else {
        format!(
            "Stop syncing “{path}”?\n\nThe local files stay on this disk and simply stop \
             being synced. Choose whether to also delete the copy in Proton Drive."
        )
    };
    let dialog = adw::AlertDialog::builder()
        .heading("Stop syncing folder")
        .body(body)
        .build();
    let group = adw::PreferencesGroup::new();
    let delete_remote = adw::SwitchRow::builder()
        .title("Also delete from Proton Drive")
        .subtitle(if ondemand {
            "Deletes the only copy of these files."
        } else {
            "The local copy is unaffected."
        })
        .active(false)
        .build();
    group.add(&delete_remote);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Stop Syncing");
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
                "Stopped syncing folder",
                "Couldn't stop syncing the folder",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Prompt for a new device name and rename it.
pub(crate) fn prompt_rename_device(ui: &Rc<Ui>, uid: &str, current: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Rename Computer")
        .body(format!("Rename “{current}”."))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("New name")
        .activates_default(true)
        .build();
    row.set_text(current);
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
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
            toast_error(&ui, "Couldn't rename the computer", "A name is required.");
            return;
        }
        run_devices_mutation(
            &ui,
            Request::RenameDevice {
                uid: uid.clone(),
                name,
            },
            "Computer renamed",
            "Couldn't rename the computer",
        );
    });
    dialog.present(win.as_ref());
}

/// Confirm, then remove (deregister) a computer. Removing deletes everything the
/// computer backed up, so the confirm button stays off until the name is typed.
pub(crate) fn prompt_remove_device(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let confirm = adw::EntryRow::builder()
        .title(format!("Type “{name}” to confirm"))
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&confirm);
    let dialog = adw::AlertDialog::builder()
        .heading("Remove Computer")
        .body(format!(
            "Remove “{name}” from this account?\n\nEverything it backed up to Proton Drive is \
             deleted along with it. The files on that computer itself are not touched — but \
             this cannot be undone from here."
        ))
        .extra_child(&group)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Remove");
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
                "Computer removed",
                "Couldn't remove the computer",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Confirm, then adopt another device as this machine's identity.
///
/// Worth a confirmation rather than a plain click: adoption re-points this
/// machine's syncing at another computer's device folder, and the folders
/// already synced here keep pointing at the old one until they are removed. The
/// dialog says so, because the alternative is a user discovering it by watching
/// their folders diverge.
pub(crate) fn prompt_adopt_device(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Use this computer's identity")
        .body(format!(
            "Treat this machine as “{name}”?\n\nNew synced folders are created under that \
             computer in Proton Drive, and this machine keeps that identity even if its hostname \
             changes. Folders already synced here are not moved.\n\nUse “Restore folders” \
             afterwards to bring back what “{name}” was syncing."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("adopt", "Use identity");
    dialog.set_response_appearance("adopt", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp == "adopt" {
            run_devices_mutation(
                &ui,
                Request::AdoptDevice {
                    uid: Some(uid.clone()),
                },
                "Identity adopted",
                "Couldn't adopt that computer",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Run a mutation raised from Computers or Locations and reload the page on success.
pub(crate) fn run_devices_mutation(
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
                reload_sync_pages(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, failed, &message, kind),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
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
        }
    }

    #[test]
    fn a_machine_with_no_folders_is_told_where_to_add_one() {
        assert_eq!(
            this_computer_subtitle(&[]),
            "No folders backed up yet — add one on the Sync page."
        );
    }

    #[test]
    fn folders_needing_attention_are_counted_not_hidden() {
        // The per-folder rows moved to Locations, so this row is the only place
        // the Computers page can surface that something is wrong.
        assert_eq!(
            this_computer_subtitle(&[folder("idle"), folder("idle")]),
            "2 folders backed up · manage them on the Sync page"
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
}
