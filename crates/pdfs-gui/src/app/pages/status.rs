use crate::*;

pub(crate) struct StatusState {
    /// Whether a [`Request::Status`] round-trip is already in flight, so the 2s
    /// refresh tick doesn't pile worker threads up on a slow/wedged daemon.
    pub(crate) status_inflight: Cell<bool>,
    /// Guards the [`Request::GetQueueStatus`] poll the same way: at most one
    /// in-flight at a time so a wedged daemon can't stack worker threads.
    pub(crate) transfers_inflight: Cell<bool>,
    /// Activity group + its current rows, hidden when no transfer is in flight.
    pub(crate) transfers_group: adw::PreferencesGroup,
    pub(crate) transfer_rows: RefCell<Vec<TransferRow>>,
    // Main page.
    pub(crate) account_row: adw::ActionRow,
    /// Read-only mount status line. The mount is driven by the systemd user
    /// service (enabled on login), not by the user — so this only reports.
    pub(crate) mount_row: adw::ActionRow,
    pub(crate) cache_bar: gtk4::ProgressBar,
    pub(crate) cache_label: gtk4::Label,
    /// "Start on login" toggle. [`Self::settings_suppress`] guards programmatic
    /// sets so reflecting the systemd state doesn't fire the toggle handler.
    pub(crate) autostart_row: adw::SwitchRow,
    /// Cache-budget editor (GiB). Populated once from config; user edits drive a
    /// `SetCacheBudget` round-trip. Guarded by [`Self::settings_suppress`].
    pub(crate) budget_row: adw::SpinRow,
    /// Shows the active mountpoint in its subtitle; updated when the user picks a
    /// new folder.
    pub(crate) mountpoint_row: adw::ActionRow,
    /// Set while a settings widget is being populated programmatically, so its
    /// change handler skips the IPC/systemd side effect.
    pub(crate) settings_suppress: Cell<bool>,
    pub(crate) pins_group: adw::PreferencesGroup,
    /// Rows currently shown under [`Self::pins_group`], retained so a refresh can
    /// diff against them and only rebuild when the pin set actually changes.
    pub(crate) pin_rows: RefCell<Vec<PinRow>>,
    /// The pin paths last rendered, the diff baseline for [`repaint_pins`].
    /// `None` = nothing built yet; `Some(empty)` = the placeholder is shown.
    pub(crate) pins_state: RefCell<Option<Vec<String>>>,
    /// The mount state the *last* desktop notification reported, so a flap only
    /// notifies on the edge. `None` until the first status reply, so a cold start
    /// doesn't announce "disconnected" before the service has had a chance to come
    /// up.
    pub(crate) notified_mounted: Cell<Option<bool>>,
    /// How many transfers were in flight on the previous poll. A drop to zero is
    /// what "sync complete" means; there's no completion event on the wire.
    pub(crate) active_transfers: Cell<usize>,
}

/// One rendered pin row, retained so [`repaint_pins`] can flip the unpin button's
/// `sensitive` in place (when the pin set is unchanged) instead of rebuilding.
pub(crate) struct PinRow {
    pub(crate) row: adw::ActionRow,
    /// The unpin button, absent on the placeholder row.
    pub(crate) unpin: Option<gtk4::Button>,
}

/// One rendered row in the Activity group: a description over a progress bar.
/// Retained so [`repaint_transfers`] can update the bar and label in place each
/// tick when the active set is unchanged, instead of rebuilding.
pub(crate) struct TransferRow {
    pub(crate) row: adw::PreferencesRow,
    pub(crate) label: gtk4::Label,
    pub(crate) bar: gtk4::ProgressBar,
}

/// What one Activity row should say this tick, and how far along it is —
/// `None` meaning "no total known", which the bar shows by pulsing. Jobs and
/// transfers both render to this, so the group is one list in the order the
/// daemon reports: the jobs that frame the work, then the files moving under it.
pub(crate) struct ActivityLine {
    pub(crate) text: String,
    pub(crate) fraction: Option<f64>,
}

/// Widgets the settings page hands back for the refresh loop and action wiring.
pub(crate) struct MainWidgets {
    pub(crate) account_row: adw::ActionRow,
    pub(crate) mount_row: adw::ActionRow,
    /// Live upload/download progress, populated by the refresh loop.
    pub(crate) transfers_group: adw::PreferencesGroup,
    pub(crate) cache_bar: gtk4::ProgressBar,
    pub(crate) cache_label: gtk4::Label,
    pub(crate) pins_group: adw::PreferencesGroup,
    pub(crate) logout_button: gtk4::Button,
    /// "Start on login" toggle, reflecting the systemd unit's enabled state.
    pub(crate) autostart_row: adw::SwitchRow,
    /// Cache soft-cap editor, in GiB; `0` = unlimited.
    pub(crate) budget_row: adw::SpinRow,
    /// Purges all unpinned cached content.
    pub(crate) purge_button: gtk4::Button,
    /// Shows the active mountpoint; its suffix button opens a folder chooser.
    pub(crate) mountpoint_row: adw::ActionRow,
    pub(crate) mountpoint_button: gtk4::Button,
}

/// The main (logged-in) page: a libadwaita settings surface — account header,
/// mount status, storage controls (cache budget + purge), system integration
/// (start-on-login, mountpoint), the pin list, and developer overrides. Returns
/// the widgets the refresh loop updates plus the controls to wire.
pub(crate) fn build_main_page() -> (gtk4::Widget, MainWidgets) {
    // Account group: identity + sign-out.
    let account_group = adw::PreferencesGroup::new();
    let account_row = adw::ActionRow::builder().title("Not signed in").build();
    let avatar = adw::Avatar::new(40, None, true);
    account_row.add_prefix(&avatar);
    let logout_button = gtk4::Button::builder()
        .label("Sign out")
        .valign(gtk4::Align::Center)
        .build();
    logout_button.add_css_class("flat");
    account_row.add_suffix(&logout_button);
    account_group.add(&account_row);

    // Mount group: a read-only status line. The mount is managed automatically
    // by the systemd user service; there is no toggle to fiddle with.
    let mount_group = adw::PreferencesGroup::builder().title("Drive").build();
    let mount_row = adw::ActionRow::builder()
        .title("Proton Drive")
        .subtitle("Not mounted")
        .build();
    mount_group.add(&mount_row);

    // Activity group: live upload/download progress. Hidden until the refresh
    // loop sees an in-flight transfer from `Request::GetQueueStatus`.
    let transfers_group = adw::PreferencesGroup::builder()
        .title("Activity")
        .description("Files moving to and from Proton Drive.")
        .visible(false)
        .build();

    // Storage group: a progress bar + "X of Y used" label, plus the cache-budget
    // editor and a purge button. `budget_row`/`purge_button` are wired in
    // `wire_settings`; the bar + label are repainted by the refresh loop.
    let storage_group = adw::PreferencesGroup::builder()
        .title("Storage")
        .description("Local cache for pinned and recently opened files.")
        .build();
    let storage_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    storage_box.set_margin_top(6);
    storage_box.set_margin_bottom(6);
    let cache_bar = gtk4::ProgressBar::new();
    let cache_label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
    cache_label.add_css_class("dim-label");
    storage_box.append(&cache_bar);
    storage_box.append(&cache_label);
    let usage_row = adw::PreferencesRow::builder()
        .activatable(false)
        .child(&storage_box)
        .build();
    storage_group.add(&usage_row);

    // Cache budget, expressed in GiB. 0 = unlimited; the daemon applies a 0 cap
    // as "no eviction". Step in 0.5 GiB; the upper bound is generous.
    let budget_adj = gtk4::Adjustment::new(0.0, 0.0, 1024.0, 0.5, 1.0, 0.0);
    let budget_row = adw::SpinRow::builder()
        .title("Cache budget (GiB)")
        .subtitle("Soft cap for cached content; 0 = unlimited.")
        .adjustment(&budget_adj)
        .digits(1)
        .build();
    storage_group.add(&budget_row);
    let purge_row = adw::ActionRow::builder()
        .title("Purge cache")
        .subtitle("Delete cached content. Pinned files are kept.")
        .build();
    let purge_button = gtk4::Button::builder()
        .label("Purge")
        .valign(gtk4::Align::Center)
        .build();
    purge_button.add_css_class("destructive-action");
    purge_row.add_suffix(&purge_button);
    storage_group.add(&purge_row);

    // System integration: start-on-login + mountpoint chooser.
    let system_group = adw::PreferencesGroup::builder()
        .title("System integration")
        .build();
    let autostart_row = adw::SwitchRow::builder()
        .title("Start on login")
        .subtitle("Mount Proton Drive automatically when you log in.")
        .build();
    system_group.add(&autostart_row);
    let mountpoint_row = adw::ActionRow::builder()
        .title("Mountpoint")
        .subtitle("—")
        .build();
    let mountpoint_button = gtk4::Button::builder()
        .label("Change")
        .valign(gtk4::Align::Center)
        .build();
    mountpoint_button.add_css_class("flat");
    mountpoint_row.add_suffix(&mountpoint_button);
    system_group.add(&mountpoint_row);

    // Pins group: filled in by refresh.
    let pins_group = adw::PreferencesGroup::builder()
        .title("Pinned files")
        .description("Kept available offline on this device.")
        .build();

    // Developer overrides: read-only client identity, for support/debugging.
    let dev_group = adw::PreferencesGroup::builder().title("Developer").build();
    let version_row = adw::ActionRow::builder()
        .title("App version")
        .subtitle(pdfs_core::config::APP_VERSION)
        .build();
    version_row.add_css_class("property");
    let agent_row = adw::ActionRow::builder()
        .title("User agent")
        .subtitle(pdfs_core::config::USER_AGENT)
        .build();
    agent_row.add_css_class("property");
    dev_group.add(&version_row);
    dev_group.add(&agent_row);

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    inner.set_margin_top(18);
    inner.set_margin_bottom(18);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&account_group);
    inner.append(&mount_group);
    inner.append(&transfers_group);
    inner.append(&storage_group);
    inner.append(&system_group);
    inner.append(&pins_group);
    inner.append(&dev_group);

    let clamp = adw::Clamp::builder()
        .maximum_size(560)
        .child(&inner)
        .build();
    let scroll = gtk4::ScrolledWindow::builder().child(&clamp).build();

    (
        scroll.upcast(),
        MainWidgets {
            account_row,
            mount_row,
            transfers_group,
            cache_bar,
            cache_label,
            pins_group,
            logout_button,
            autostart_row,
            budget_row,
            purge_button,
            mountpoint_row,
            mountpoint_button,
        },
    )
}

/// Bytes per GiB, for the cache-budget editor's unit conversion.
pub(crate) const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Wire the Settings-page controls: the cache-budget editor, the purge button,
/// the start-on-login switch and the mountpoint chooser. Initial widget state is
/// read once from config / systemd here (the refresh loop owns only the live
/// mount + cache-usage read-out), with [`Ui::settings_suppress`] set around the
/// programmatic populate so the change handlers don't fire on it.
pub(crate) fn wire_settings(
    ui: &Rc<Ui>,
    purge_button: &gtk4::Button,
    mountpoint_button: &gtk4::Button,
) {
    let config = ui.dirs.load_config();

    // Populate from persisted config + the systemd unit state, suppressed.
    ui.status.settings_suppress.set(true);
    ui.status
        .budget_row
        .set_value(config.resolved_cache_budget() as f64 / GIB);
    ui.status
        .mountpoint_row
        .set_subtitle(&ui.dirs.resolved_mountpoint(&config).display().to_string());
    ui.status.autostart_row.set_active(service::is_enabled());
    ui.status.settings_suppress.set(false);

    // Cache budget: a user edit applies the new soft cap on the daemon (which
    // also persists it to config). 0 GiB = unlimited.
    let ui_budget = ui.clone();
    ui.status.budget_row.connect_value_notify(move |row| {
        if ui_budget.status.settings_suppress.get() {
            return;
        }
        let bytes = (row.value() * GIB).round() as u64;
        settings_request(
            &ui_budget,
            Request::SetCacheBudget { bytes },
            "Cache budget updated",
            "Couldn't set cache budget",
        );
    });

    // Purge: confirm, then drop all unpinned cached content via the daemon.
    let ui_purge = ui.clone();
    purge_button.connect_clicked(move |_| {
        let ui = ui_purge.clone();
        let dialog = adw::AlertDialog::builder()
            .heading("Purge cache")
            .body("Delete all cached content that isn't pinned? Pinned files stay offline.")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("purge", "Purge");
        dialog.set_response_appearance("purge", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |_, resp| {
            if resp == "purge" {
                settings_request(
                    &ui,
                    Request::PurgeCache,
                    "Cache purged",
                    "Couldn't purge cache",
                );
            }
        });
        dialog.present(ui_window(&ui_purge).as_ref());
    });

    // Start on login: enable/disable the systemd unit without stopping a live
    // mount (the user can disconnect separately).
    let ui_auto = ui.clone();
    ui.status.autostart_row.connect_active_notify(move |row| {
        if ui_auto.status.settings_suppress.get() {
            return;
        }
        if row.is_active() {
            service::enable();
        } else {
            service::disable();
        }
    });

    // Mountpoint: pick a folder, persist it, and offer to restart the mount so
    // the change takes effect (the daemon reads the path on mount).
    let ui_mp = ui.clone();
    mountpoint_button.connect_clicked(move |_| prompt_mountpoint(&ui_mp));
}

/// Run a settings control-socket round-trip (budget / purge) on a worker thread,
/// confirming with `done` or reporting the daemon's error under `failed`. Unlike
/// [`run_mutation`] there's no browser reload; the next refresh tick repaints the
/// cache read-out.
pub(crate) fn settings_request(
    ui: &Rc<Ui>,
    req: Request,
    done: &'static str,
    failed: &'static str,
) {
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => toast(&ui, done),
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, failed, &message, kind),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}

/// Prompt for a new mountpoint folder, persist it to config, and offer to restart
/// the mount service so the daemon picks it up.
pub(crate) fn prompt_mountpoint(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = gtk4::FileDialog::builder()
        .title("Choose mountpoint folder")
        .build();
    let ui = ui.clone();
    dialog.select_folder(win.as_ref(), gio::Cancellable::NONE, move |res| {
        let Ok(folder) = res else { return };
        let Some(path) = folder.path() else { return };
        let path_str = path.display().to_string();

        // Persist the choice to config so the next mount uses it.
        let mut config = ui.dirs.load_config();
        config.mountpoint = Some(path_str.clone());
        if let Err(e) = ui.dirs.save_config(&config) {
            toast_error(&ui, "Couldn't save mountpoint", &e.to_string());
            return;
        }
        ui.status.mountpoint_row.set_subtitle(&path_str);

        // The daemon only reads the mountpoint at mount time, so offer a restart.
        let confirm = adw::AlertDialog::builder()
            .heading("Restart to apply")
            .body(format!(
                "The mountpoint is now “{path_str}”. Restart the Drive mount to use it?"
            ))
            .build();
        confirm.add_response("later", "Later");
        confirm.add_response("restart", "Restart now");
        confirm.set_response_appearance("restart", adw::ResponseAppearance::Suggested);
        confirm.set_default_response(Some("restart"));
        confirm.set_close_response("later");
        confirm.connect_response(None, |_, resp| {
            if resp == "restart" {
                service::restart();
            }
        });
        confirm.present(ui_window(&ui).as_ref());
    });
}

/// Connect the Files/Photos "Retry" buttons (shown by [`browser_unreachable`] /
/// [`gallery_unreachable`] when the mount is down): restart the systemd unit and
/// reload the page.
pub(crate) fn wire_retry(ui: &Rc<Ui>) {
    let ui_browser = ui.clone();
    ui.browser.retry.clone().connect_clicked(move |_| {
        service::restart();
        load_browser(&ui_browser);
    });
    let ui_gallery = ui.clone();
    ui.gallery.retry.clone().connect_clicked(move |_| {
        service::restart();
        load_gallery(&ui_gallery, false);
    });
    let ui_trash = ui.clone();
    ui.trash.retry.clone().connect_clicked(move |_| {
        service::restart();
        load_trash(&ui_trash);
    });
}

/// Repaint the window from the cached login identity, then kick an async mount-
/// status fetch. Runs on the 2s tick: the identity check is instant (no keyring),
/// and the status round-trip is offloaded to a worker so the main loop never
/// blocks on a slow or wedged daemon.
pub(crate) fn refresh(ui: &Rc<Ui>) {
    // Login identity decides which page is shown. Read the cached session — set
    // at startup and on login/logout — never the keyring.
    {
        let session = ui.session.borrow();
        match session.as_ref() {
            Some(s) => {
                // Only pull the user onto a destination when they're sitting on the
                // login page; otherwise leave whichever page they navigated to.
                if ui.stack.visible_child_name().as_deref() == Some("login") {
                    ui.stack.set_visible_child_name("browser");
                }
                ui.nav.set_collapsed(false);
                ui.status.account_row.set_title(&s.username);
                ui.status.account_row.set_subtitle("Proton account");
            }
            None => {
                ui.stack.set_visible_child_name("login");
                // Collapsed + showing content = the login page owns the window and
                // no destination is reachable without a session.
                ui.nav.set_collapsed(true);
                ui.nav.set_show_content(true);
                return;
            }
        }
    }

    refresh_status(ui);
    refresh_transfers(ui);
    // Both of these pages show work as it happens, so they follow the tick while
    // they are on screen. Every other page loads on navigation only.
    match ui.stack.visible_child_name().as_deref() {
        Some("devices") => refresh_sync_folders(ui),
        Some("activity") => refresh_activity(ui),
        _ => {}
    }
}

/// Record the mount state seen by the last status poll: gate every control that
/// needs a live daemon, and notify the desktop when the state actually flips.
///
/// The gating is the point — without it, New Folder / Upload / the details pane's
/// actions stay clickable while the mount is down and each click buys a round-trip
/// that can only fail. A greyed control says so up front.
pub(crate) fn set_mounted(ui: &Rc<Ui>, mounted: bool) {
    *ui.mounted.borrow_mut() = mounted;
    ui.browser.new_folder.set_sensitive(mounted);
    ui.browser.upload.set_sensitive(mounted);
    ui.browser.upload_folder.set_sensitive(mounted);
    ui.gallery.upload.set_sensitive(mounted);
    ui.details.details.pin_row.set_sensitive(mounted);
    ui.details.details.rename_button.set_sensitive(mounted);
    ui.details.details.trash_button.set_sensitive(mounted);
    ui.details.details.open_button.set_sensitive(mounted);

    // Only notify on a real edge, and never for the first reading: at startup the
    // service is usually still coming up, and "disconnected" would be a lie.
    if ui.status.notified_mounted.get() == Some(mounted) {
        return;
    }
    let first = ui.status.notified_mounted.replace(Some(mounted)).is_none();
    if first {
        return;
    }
    if mounted {
        notify(
            "mount-state",
            "Proton Drive connected",
            "Your Drive is mounted and available.",
        );
    } else {
        notify(
            "mount-state",
            "Proton Drive disconnected",
            "The mount service stopped. Files aren't available until it restarts.",
        );
    }
}

/// Poll the daemon's in-flight transfers on a worker thread and repaint the
/// Activity group. Independently inflight-guarded from [`refresh_status`] so the
/// two cheap polls on the 2s tick don't gate each other. The group hides itself
/// when nothing is moving, so an idle account shows no Activity section.
pub(crate) fn refresh_transfers(ui: &Rc<Ui>) {
    if ui.status.transfers_inflight.get() {
        return;
    }
    ui.status.transfers_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::GetQueueStatus);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.status.transfers_inflight.set(false);
        match result {
            Ok(Ok(Response::Transfers { items, jobs })) => repaint_transfers(&ui, &items, &jobs),
            // Daemon unreachable or odd reply: clear the section rather than
            // leave stale progress bars frozen on screen.
            _ => repaint_transfers(&ui, &[], &[]),
        }
    });
}

/// Render the Activity group from a work snapshot: the daemon's jobs (scans,
/// batch counts, the local index) above the files moving under them. Rebuilds the
/// rows only when the set changes (count differs); on the common steady tick it
/// updates each bar's fraction and the label in place, so progress animates
/// without flicker. Hides the whole group when the daemon is idle.
pub(crate) fn repaint_transfers(ui: &Rc<Ui>, items: &[TransferItem], jobs: &[JobItem]) {
    let lines: Vec<ActivityLine> = jobs
        .iter()
        .map(job_line)
        .chain(items.iter().map(transfer_line))
        .collect();

    // The wire carries in-flight transfers only, with no completion event: the
    // count falling to zero is what "the sync finished" looks like from here.
    // Jobs are deliberately not counted — a bulk upload retires its scan job and
    // starts its upload job mid-flight, which is not a thing finishing.
    let previous = ui.status.active_transfers.replace(items.len());
    if items.is_empty() && previous > 0 {
        let files = if previous == 1 {
            "1 file".to_string()
        } else {
            format!("{previous} files")
        };
        notify(
            "sync-complete",
            "Sync complete",
            &format!("{files} finished transferring."),
        );
        // A just-finished batch may have added files (bulk upload) the current
        // listing doesn't show yet; refresh whichever listing is on screen.
        reload_listing(ui);
    }

    if lines.is_empty() {
        if !ui.status.transfer_rows.borrow().is_empty() {
            for tr in ui.status.transfer_rows.borrow_mut().drain(..) {
                ui.status.transfers_group.remove(&tr.row);
            }
        }
        ui.status.transfers_group.set_visible(false);
        return;
    }

    ui.status.transfers_group.set_visible(true);

    // Rebuild rows only when the count changes; otherwise reuse them in place.
    if ui.status.transfer_rows.borrow().len() != lines.len() {
        for tr in ui.status.transfer_rows.borrow_mut().drain(..) {
            ui.status.transfers_group.remove(&tr.row);
        }
        for _ in &lines {
            let row_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            row_box.set_margin_top(8);
            row_box.set_margin_bottom(8);
            let label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
            label.add_css_class("dim-label");
            let bar = gtk4::ProgressBar::new();
            row_box.append(&label);
            row_box.append(&bar);
            let row = adw::PreferencesRow::builder()
                .activatable(false)
                .child(&row_box)
                .build();
            ui.status.transfers_group.add(&row);
            ui.status
                .transfer_rows
                .borrow_mut()
                .push(TransferRow { row, label, bar });
        }
    }

    for (line, tr) in lines.iter().zip(ui.status.transfer_rows.borrow().iter()) {
        tr.label.set_text(&line.text);
        match line.fraction {
            Some(f) => tr.bar.set_fraction(f),
            // No total to divide by: pulse so the bar still reads as "working".
            None => tr.bar.pulse(),
        }
    }
}

/// One Activity row for a daemon job: its title, plus whatever it can say about
/// where it is — a count when it has one, else what it is currently chewing on.
pub(crate) fn job_line(j: &JobItem) -> ActivityLine {
    let text = match (j.total > 0, j.detail.is_empty()) {
        (true, true) => format!("{} — {} of {}", j.title, j.done, j.total),
        (true, false) => format!("{} — {} ({} of {})", j.title, j.detail, j.done, j.total),
        (false, true) => format!("{}…", j.title),
        (false, false) => format!("{} — {}…", j.title, j.detail),
    };
    ActivityLine {
        text,
        fraction: (j.total > 0).then(|| (j.done as f64 / j.total as f64).min(1.0)),
    }
}

/// One Activity row for a file in flight: which way it's going, how far, how fast.
pub(crate) fn transfer_line(t: &TransferItem) -> ActivityLine {
    let arrow = match t.direction {
        TransferDirection::Download => "↓",
        TransferDirection::Upload => "↑",
    };
    if t.bytes_total == 0 {
        ActivityLine {
            text: format!(
                "{arrow} {} — {} ({}/s)",
                t.name,
                human_bytes(t.bytes_completed),
                human_bytes(t.speed_bytes_sec),
            ),
            fraction: None,
        }
    } else {
        ActivityLine {
            text: format!(
                "{arrow} {} — {} of {} ({}/s)",
                t.name,
                human_bytes(t.bytes_completed),
                human_bytes(t.bytes_total),
                human_bytes(t.speed_bytes_sec),
            ),
            fraction: Some((t.bytes_completed as f64 / t.bytes_total as f64).min(1.0)),
        }
    }
}

/// Fetch mount status + cache stats from the daemon on a worker thread and repaint
/// the mount line, cache bar and pin list on the reply. The daemon owns the cache
/// stats now (`used`/`budget`/`pins` ride along on [`Response::Status`]), so the
/// GUI never opens the on-disk cache itself. Skipped while a fetch is in flight so
/// the tick can't stack threads on a stalled daemon.
pub(crate) fn refresh_status(ui: &Rc<Ui>) {
    if ui.status.status_inflight.get() {
        return;
    }
    ui.status.status_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::Status);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.status.status_inflight.set(false);
        match result {
            Ok(Ok(Response::Status {
                mountpoint,
                used,
                budget,
                pins,
                online,
                pending_uploads,
                pending_changes,
                ..
            })) => {
                set_mounted(&ui, true);
                // The queue is the more useful thing to say when it has anything
                // in it: it is why a file that looks saved is not on the remote
                // yet, and offline is usually the reason it is still queued.
                let queued = pending_summary(pending_uploads, pending_changes);
                ui.status.mount_row.set_subtitle(&match (online, queued) {
                    (true, None) => format!("Mounted at {mountpoint}"),
                    (true, Some(q)) => format!("Mounted at {mountpoint} — {q}"),
                    (false, None) => {
                        format!("Mounted at {mountpoint} — offline, cached files only")
                    }
                    (false, Some(q)) => format!("Mounted at {mountpoint} — offline, {q}"),
                });
                let fraction = if budget == 0 {
                    0.0
                } else {
                    (used as f64 / budget as f64).min(1.0)
                };
                ui.status.cache_bar.set_fraction(fraction);
                ui.status.cache_label.set_text(&format!(
                    "{} of {} used",
                    human_bytes(used),
                    human_bytes(budget)
                ));
                repaint_pins(&ui, &pins, true);
            }
            // Daemon unreachable (still starting, or down): report not-mounted and
            // grey out the unpin buttons in place, but leave the last-known pin
            // rows and cache read-out so the page doesn't flicker on a blip.
            _ => {
                set_mounted(&ui, false);
                ui.status.mount_row.set_subtitle("Not mounted");
                for r in ui.status.pin_rows.borrow().iter() {
                    if let Some(b) = &r.unpin {
                        b.set_sensitive(false);
                    }
                }
            }
        }
    });
}

/// Render the pins group from `pins`, with the unpin buttons enabled only while a
/// mount daemon is running (`mounted`). Diffs against the last batch by path: when
/// the set is unchanged (the common case on the 2s tick) it only flips the unpin
/// buttons' `sensitive` flag, avoiding the rebuild that used to flicker the list
/// and drop scroll/selection every tick.
pub(crate) fn repaint_pins(ui: &Rc<Ui>, pins: &[pdfs_core::cache::Pin], mounted: bool) {
    let desired: Vec<String> = pins.iter().map(|p| p.path.clone()).collect();
    if ui.status.pins_state.borrow().as_ref() == Some(&desired) {
        for r in ui.status.pin_rows.borrow().iter() {
            if let Some(b) = &r.unpin {
                b.set_sensitive(mounted);
            }
        }
        return;
    }

    for pr in ui.status.pin_rows.borrow_mut().drain(..) {
        ui.status.pins_group.remove(&pr.row);
    }
    *ui.status.pins_state.borrow_mut() = Some(desired);

    if pins.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No pinned files")
            .subtitle("Right-click a file in the mount to keep it offline.")
            .build();
        ui.status.pins_group.add(&row);
        ui.status
            .pin_rows
            .borrow_mut()
            .push(PinRow { row, unpin: None });
        return;
    }

    for pin in pins {
        let name = Path::new(&pin.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&pin.path)
            .to_string();
        let row = adw::ActionRow::builder()
            .title(&name)
            .subtitle(&pin.path)
            .build();
        let icon = gtk4::Image::from_icon_name("emblem-documents-symbolic");
        row.add_prefix(&icon);

        let unpin = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .valign(gtk4::Align::Center)
            .tooltip_text("Unpin (remove offline copy)")
            .sensitive(mounted)
            .build();
        unpin.add_css_class("flat");
        let ui_btn = ui.clone();
        let path = pin.path.clone();
        unpin.connect_clicked(move |_| {
            let socket = ui_btn.dirs.control_socket();
            match send(&socket, &Request::Unpin { path: path.clone() }) {
                Ok(Response::Error { message, .. }) => tracing::error!("unpin failed: {message}"),
                Ok(_) => refresh(&ui_btn),
                Err(e) => tracing::error!("unpin request failed: {e}"),
            }
        });
        row.add_suffix(&unpin);

        ui.status.pins_group.add(&row);
        ui.status.pin_rows.borrow_mut().push(PinRow {
            row,
            unpin: Some(unpin),
        });
    }
}
