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
    pub(crate) sync_rows: RefCell<Vec<gtk4::Widget>>,
    pub(crate) inflight: Cell<bool>,
    pub(crate) loaded_at: Cell<Option<Instant>>,
}

/// Widgets the Devices page's load/repaint touch.
pub(crate) struct DevicesWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    /// "This computer" — the local folders synced to this machine's device.
    pub(crate) sync_group: adw::PreferencesGroup,
    /// "Other computers" — the account's *other* registered devices. This
    /// machine's own device is deliberately not among them; see
    /// [`repaint_devices`].
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) add_folder: gtk4::Button,
    /// "Restore Folders" — re-attach this device's remote folders to local
    /// directories after adopting it on a new machine.
    pub(crate) restore: gtk4::Button,
    pub(crate) retry: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
}

/// The Computers page: a "This computer" section listing the local folders synced
/// to this machine's device (each row offering a mode toggle and Remove), plus an
/// "Other devices" section listing the account's other registered devices.
pub(crate) fn build_devices_page() -> (gtk4::Widget, DevicesWidgets) {
    let title = gtk4::Label::builder()
        .label("Computers")
        .halign(gtk4::Align::Start)
        .build();
    title.add_css_class("title-2");
    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title);

    let add_folder = gtk4::Button::builder()
        .label("Add Folder")
        .valign(gtk4::Align::Center)
        .build();
    add_folder.add_css_class("flat");
    let restore = gtk4::Button::builder()
        .label("Restore Folders")
        .tooltip_text("Sync this computer's Drive folders back to local directories")
        .valign(gtk4::Align::Center)
        .build();
    restore.add_css_class("flat");
    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&titles);
    header.append(&refresh);
    header.append(&restore);
    header.append(&add_folder);

    // The description spells out what the per-row On-demand switch does to the
    // local copy, because the switch itself can't: turning it on *deletes* the
    // files from this disk, which is not something to discover afterwards.
    let sync_group = adw::PreferencesGroup::builder()
        .title("This computer")
        .description(
            "Local folders backed up to this device. Synced folders keep a full copy on \
             this disk; on-demand folders keep the files in Proton Drive only and fetch \
             them as you open them.",
        )
        .build();
    let group = adw::PreferencesGroup::builder()
        .title("Other computers")
        .description("Other devices backing up to this account.")
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
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&content);

    (
        inner.upcast(),
        DevicesWidgets {
            content,
            status,
            sync_group,
            group,
            add_folder,
            restore,
            retry,
            refresh,
        },
    )
}

/// Install the Devices page's retry button and the "Add Folder" action.
pub(crate) fn wire_devices(
    ui: &Rc<Ui>,
    retry: &gtk4::Button,
    add_folder: &gtk4::Button,
    restore: &gtk4::Button,
) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_devices(&ui_retry);
    });
    let ui_add = ui.clone();
    add_folder.connect_clicked(move |_| prompt_add_sync_folder(&ui_add));
    let ui_restore = ui.clone();
    restore.connect_clicked(move |_| prompt_restore_folders(&ui_restore));
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
        "Reading your devices.",
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
        // Daemon reachable: show the list and paint the synced-folders section.
        ui.devices.content.set_visible_child_name("list");
        repaint_sync_folders(&ui, &folders);

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

/// Refresh just the synced-folders section, with no status flash, no spinner and
/// no devices round-trip. Driven by the periodic tick while the Devices page is
/// on screen: a folder's sync progress is only live if something re-reads it.
///
/// A hiccup leaves the page as it is — the next tick tries again, and a real
/// outage is reported by [`load_devices`] on the next visit.
pub(crate) fn refresh_sync_folders(ui: &Rc<Ui>) {
    if ui.devices.inflight.get() {
        return;
    }
    ui.devices.inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSyncFolders);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.devices.inflight.set(false);
        let Ok(Ok(Response::SyncFolders { items })) = result else {
            return;
        };
        // The page may have been navigated away from, or collapsed to a status
        // view, while the request was in flight.
        if ui.stack.visible_child_name().as_deref() == Some("devices")
            && ui.devices.content.visible_child_name().as_deref() == Some("list")
        {
            repaint_sync_folders(&ui, &items);
        }
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
        let remove = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove this computer and everything it backed up")
            .valign(gtk4::Align::Center)
            .build();
        remove.add_css_class("flat");
        let rename = gtk4::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Rename computer")
            .valign(gtk4::Align::Center)
            .build();
        rename.add_css_class("flat");
        // Adoption is how a reinstalled or renamed machine re-attaches to the
        // device it used to be, instead of registering a duplicate. It only
        // makes sense on *another* device's row, which is the only place this
        // loop paints.
        let adopt = gtk4::Button::builder()
            .icon_name("insert-object-symbolic")
            .tooltip_text("Use this computer's identity for this machine")
            .valign(gtk4::Align::Center)
            .build();
        adopt.add_css_class("flat");
        let ui_ad = ui.clone();
        let uid_ad = dev.uid.clone();
        let name_ad = dev.name.clone();
        adopt.connect_clicked(move |_| prompt_adopt_device(&ui_ad, &uid_ad, &name_ad));
        row.add_suffix(&adopt);
        let ui_ren = ui.clone();
        let uid_ren = dev.uid.clone();
        let name_ren = dev.name.clone();
        rename.connect_clicked(move |_| prompt_rename_device(&ui_ren, &uid_ren, &name_ren));
        let ui_rm = ui.clone();
        let uid_rm = dev.uid.clone();
        let name_rm = dev.name.clone();
        remove.connect_clicked(move |_| prompt_remove_device(&ui_rm, &uid_rm, &name_rm));
        row.add_suffix(&rename);
        row.add_suffix(&remove);
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

/// Rebuild the "This computer" section from a fresh synced-folders listing.
pub(crate) fn repaint_sync_folders(ui: &Rc<Ui>, folders: &[SyncFolderInfo]) {
    for row in ui.devices.sync_rows.borrow_mut().drain(..) {
        ui.devices.sync_group.remove(&row);
    }
    if folders.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No synced folders")
            .subtitle("Add a folder to back it up to this device.")
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("folder-symbolic"));
        ui.devices.sync_group.add(&row);
        *ui.devices.sync_rows.borrow_mut() = vec![row.upcast()];
        return;
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for f in folders {
        // A running pass describes itself ("Uploading photo.jpg — 12 of 40");
        // otherwise fall back to the folder's resting state.
        let status = match &f.progress {
            Some(p) => sync_progress_label(p),
            None => sync_state_label(&f.state).to_string(),
        };
        // A queued switch is where the folder is *going*, which is what the user
        // just asked for and is waiting to see happen — so it leads the subtitle
        // instead of the mode the folder is still technically in.
        let subtitle = match (f.pending_mode.as_deref(), f.mode.as_str()) {
            (Some("ondemand"), _) => format!("Going on-demand · {status}"),
            (Some(_), _) => format!("Switching to synced · {status}"),
            (None, "ondemand") => format!("On-demand · {status}"),
            (None, _) => format!("Synced · {status}"),
        };
        let row = adw::ActionRow::builder()
            .title(&f.local_path)
            .subtitle(&subtitle)
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("folder-symbolic"));
        let id = f.id;

        // A pass that knows how far it has got shows it: the subtitle alone
        // ("syncing photo.jpg — 12 of 40") never conveys that the end is near. This
        // holds for scanning as much as applying — a big folder spends minutes there
        // — but a folder syncing for the first time has no estimate to draw against
        // (`total == 0`), and this row is repainted on the 2s tick, too slow for a
        // pulse to read as motion. So it stays text-only until real counts exist.
        if let Some(p) = &f.progress
            && p.total > 0
        {
            let bar = gtk4::ProgressBar::builder()
                .fraction((p.done as f64 / p.total.max(p.done) as f64).min(1.0))
                .valign(gtk4::Align::Center)
                .width_request(120)
                .build();
            row.add_suffix(&bar);
        }

        // Sync now — only meaningful for mirror folders (on-demand has no local
        // copy to reconcile). The engine also syncs on file changes and on a
        // 120s poll; this is the "don't wait" button.
        if f.mode != "ondemand" {
            let sync_now = gtk4::Button::builder()
                .icon_name("view-refresh-symbolic")
                .tooltip_text("Sync this folder now")
                .valign(gtk4::Align::Center)
                .build();
            sync_now.add_css_class("flat");
            let ui_sync = ui.clone();
            sync_now.connect_clicked(move |_| {
                let rx = spawn_request(
                    ui_sync.dirs.control_socket(),
                    Request::SyncNow { id: Some(id) },
                );
                let ui_sync = ui_sync.clone();
                glib::spawn_future_local(async move {
                    match rx.recv().await {
                        Ok(Ok(Response::Ok { .. })) => toast(&ui_sync, "Syncing folder…"),
                        Ok(Ok(Response::Error { message, kind })) => {
                            toast_failure(&ui_sync, "Couldn't sync", &message, kind)
                        }
                        _ => toast_error(
                            &ui_sync,
                            "Couldn't sync",
                            "The mount service didn't respond.",
                        ),
                    }
                });
            });
            row.add_suffix(&sync_now);
        }

        // On-demand toggle: off = full local copy (mirror), on = FUSE mount that
        // frees the disk. Set the state before wiring the handler so painting the
        // current mode doesn't fire a spurious request.
        //
        // A queued switch paints as already flipped: the daemon accepted the
        // request and will act on it, so snapping the switch back to the current
        // mode would read as "it didn't take" and invite the user to toggle again.
        let target = f.pending_mode.as_deref().unwrap_or(&f.mode);
        let ondemand = gtk4::Switch::builder()
            .tooltip_text(
                "On-demand: free this disk by keeping the files in Proton Drive only, \
                 fetching each as you open it. Turn off to download them back and keep a \
                 full local copy.",
            )
            .valign(gtk4::Align::Center)
            .active(target == "ondemand")
            .build();
        let ui_mode = ui.clone();
        ondemand.connect_state_set(move |_, on| {
            set_sync_folder_mode(&ui_mode, id, if on { "ondemand" } else { "mirror" });
            glib::Propagation::Proceed
        });
        row.add_suffix(&ondemand);

        let remove = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Stop syncing this folder")
            .valign(gtk4::Align::Center)
            .build();
        remove.add_css_class("flat");
        let ui_rm = ui.clone();
        let path = f.local_path.clone();
        // The folder's *current* mode, not a queued one: a switch that hasn't
        // landed yet has not moved the files anywhere.
        let ondemand = f.mode == "ondemand";
        remove.connect_clicked(move |_| prompt_remove_sync_folder(&ui_rm, id, &path, ondemand));
        row.add_suffix(&remove);
        ui.devices.sync_group.add(&row);
        rows.push(row.upcast());
    }
    *ui.devices.sync_rows.borrow_mut() = rows;
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
                    // `refresh_sync_folders` tick pick the row up whenever it
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
pub(crate) fn set_sync_folder_mode(ui: &Rc<Ui>, id: i64, mode: &str) {
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
            Ok(Ok(Response::Ok { message })) => toast(&ui, &message),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't change mode", &message, kind)
            }
            _ => toast_error(
                &ui,
                "Couldn't change mode",
                "The mount service didn't respond.",
            ),
        }
        if ui.stack.visible_child_name().as_deref() == Some("devices") {
            load_devices(&ui);
        }
    });
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
        .heading("Rename device")
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
            toast_error(&ui, "Couldn't rename device", "A name is required.");
            return;
        }
        run_devices_mutation(
            &ui,
            Request::RenameDevice {
                uid: uid.clone(),
                name,
            },
            "Device renamed",
            "Couldn't rename the device",
        );
    });
    dialog.present(win.as_ref());
}

/// Confirm, then remove (deregister) a device.
pub(crate) fn prompt_remove_device(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Remove computer")
        .body(format!(
            "Remove “{name}” from this account?\n\nEverything it backed up to Proton Drive is \
             deleted along with it. The files on that computer itself are not touched — but \
             this cannot be undone from here."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Remove");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp == "remove" {
            run_devices_mutation(
                &ui,
                Request::DeleteDevice { uid: uid.clone() },
                "Device removed",
                "Couldn't remove the device",
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

/// Run a mutation raised from the Devices page and reload the page on success.
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
                load_devices(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, failed, &message, kind),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}
