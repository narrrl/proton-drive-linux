use crate::*;

pub(crate) struct StatusState {
    /// Whether a [`Request::Status`] round-trip is already in flight, so the 2s
    /// refresh tick doesn't pile worker threads up on a slow/wedged daemon.
    pub(crate) status_inflight: Cell<bool>,
    /// Guards the [`Request::GetQueueStatus`] poll the same way: at most one
    /// in-flight at a time so a wedged daemon can't stack worker threads.
    pub(crate) transfers_inflight: Cell<bool>,
    /// Transfers group (on the Sync page) + its current rows, hidden when no
    /// transfer is in flight.
    pub(crate) transfers_group: adw::PreferencesGroup,
    pub(crate) transfer_rows: RefCell<Vec<TransferRow>>,
    /// The Preferences dialog, built once and presented on demand.
    pub(crate) prefs: adw::PreferencesDialog,
    /// Sidebar footer: signed-in identity.
    pub(crate) account_name: gtk4::Label,
    pub(crate) avatar: adw::Avatar,
    /// Sidebar footer: the one-line sync state. The mount is driven by the
    /// systemd user service, not by the user — so this only reports.
    pub(crate) status_icon: gtk4::Image,
    pub(crate) status_title: gtk4::Label,
    pub(crate) status_detail: gtk4::Label,
    pub(crate) cache_bar: gtk4::ProgressBar,
    pub(crate) cache_label: gtk4::Label,
    /// Sidebar footer: account quota bar/label. Hidden until the first reading.
    pub(crate) quota_box: gtk4::Box,
    pub(crate) quota_bar: gtk4::ProgressBar,
    pub(crate) quota_label: gtk4::Label,
    /// One `AccountQuota` in flight at a time, and when it last succeeded — quota
    /// barely moves, so [`refresh_quota`] refetches on a long TTL, not every tick.
    pub(crate) quota_inflight: Cell<bool>,
    pub(crate) quota_checked_at: Cell<Option<Instant>>,
    /// "Start on login" toggle. [`Self::settings_suppress`] guards programmatic
    /// sets so reflecting the systemd state doesn't fire the toggle handler.
    pub(crate) autostart_row: adw::SwitchRow,
    /// Cache-budget editor (GiB). Populated once from config; user edits drive a
    /// `SetCacheBudget` round-trip. Guarded by [`Self::settings_suppress`].
    pub(crate) budget_row: adw::SpinRow,
    /// Bandwidth caps (MiB/s, `0` = none). Populated once from config; an edit
    /// to either sends both in one `SetBandwidthLimits`. Guarded by
    /// [`Self::settings_suppress`].
    pub(crate) upload_limit_row: adw::SpinRow,
    pub(crate) download_limit_row: adw::SpinRow,
    /// Shows where the primary mount lives; its Change button picks a new one.
    pub(crate) mountpoint_row: adw::ActionRow,
    /// "Proton purple accent" toggle. Guarded by [`Self::settings_suppress`].
    pub(crate) accent_row: adw::SwitchRow,
    /// "Show tray icon" toggle. Guarded by [`Self::settings_suppress`].
    pub(crate) tray_row: adw::SwitchRow,
    /// Set while a settings widget is being populated programmatically, so its
    /// change handler skips the IPC/systemd side effect.
    pub(crate) settings_suppress: Cell<bool>,
    /// Pending debounce for the cache-budget editor. Each `+` click is a value
    /// change, and firing a `SetCacheBudget` (and a toast) per click means a
    /// stack of toasts and a series of caps the user never asked to apply — so
    /// only the value they settle on is sent.
    pub(crate) budget_source: RefCell<Option<glib::SourceId>>,
    /// Pending debounce for the bandwidth editors, as for the cache budget.
    pub(crate) limit_source: RefCell<Option<glib::SourceId>>,
    pub(crate) pins_group: adw::PreferencesGroup,
    /// Whether the pin list is showing every pin or only the first
    /// [`PINS_COLLAPSED`]. A long pin list would otherwise push everything below
    /// it off the end of the Storage page.
    pub(crate) pins_expanded: Cell<bool>,
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

/// Widgets the Preferences dialog and the sidebar footer hand back for the
/// refresh loop and action wiring.
pub(crate) struct MainWidgets {
    pub(crate) prefs: adw::PreferencesDialog,
    /// The sidebar footer: sync status, quota, account.
    pub(crate) footer: gtk4::Box,
    /// Opens the Sync page; the footer's status strip is this button.
    pub(crate) status_button: gtk4::Button,
    pub(crate) account_name: gtk4::Label,
    pub(crate) avatar: adw::Avatar,
    pub(crate) status_icon: gtk4::Image,
    pub(crate) status_title: gtk4::Label,
    pub(crate) status_detail: gtk4::Label,
    pub(crate) quota_box: gtk4::Box,
    pub(crate) quota_bar: gtk4::ProgressBar,
    pub(crate) quota_label: gtk4::Label,
    pub(crate) cache_bar: gtk4::ProgressBar,
    pub(crate) cache_label: gtk4::Label,
    pub(crate) pins_group: adw::PreferencesGroup,
    /// "Start on login" toggle, reflecting the systemd unit's enabled state.
    pub(crate) autostart_row: adw::SwitchRow,
    /// Cache soft-cap editor, in GiB; `0` = unlimited.
    pub(crate) budget_row: adw::SpinRow,
    /// Upload and download caps, in MiB/s; `0` = unlimited.
    pub(crate) upload_limit_row: adw::SpinRow,
    pub(crate) download_limit_row: adw::SpinRow,
    /// Purges all unpinned cached content.
    pub(crate) purge_button: gtk4::Button,
    /// Shows the active mountpoint; its suffix button picks a new one.
    pub(crate) mountpoint_row: adw::ActionRow,
    pub(crate) mountpoint_button: gtk4::Button,
    pub(crate) accent_row: adw::SwitchRow,
    pub(crate) tray_row: adw::SwitchRow,
}

/// Build the two surfaces that replaced the old Settings page:
///
/// - the Preferences dialog (General: start on login, mount location,
///   appearance; Storage: cache usage, budget, clear, offline files), and
/// - the sidebar footer (sync status strip, account quota, account menu).
///
/// Live state (transfers, the folder list) lives on the Sync page instead, and
/// the app version and user agent in About → Troubleshooting.
pub(crate) fn build_main_page() -> MainWidgets {
    // ---- Preferences: General
    let startup_group = adw::PreferencesGroup::builder().title("Startup").build();
    let autostart_row = adw::SwitchRow::builder()
        .title("Start on login")
        .subtitle("Connect Proton Drive automatically when you log in")
        .build();
    startup_group.add(&autostart_row);

    let location_group = adw::PreferencesGroup::builder().title("Location").build();
    let mountpoint_row = adw::ActionRow::builder()
        .title("Proton Drive folder")
        .subtitle("—")
        .build();
    mountpoint_row.add_css_class("property");
    let mountpoint_button = gtk4::Button::builder()
        .label("Change…")
        .tooltip_text("Choose a different folder for the Proton Drive mount")
        .valign(gtk4::Align::Center)
        .build();
    mountpoint_button.add_css_class("flat");
    mountpoint_row.add_suffix(&mountpoint_button);
    location_group.add(&mountpoint_row);

    // Bandwidth caps in MiB/s. 0 = unlimited; the smallest cap is one 0.5 step,
    // since anything slower would stall a single block for most of a minute.
    let network_group = adw::PreferencesGroup::builder()
        .title("Network")
        .description("Limit how fast files move, shared by every transfer. 0 means no limit.")
        .build();
    let limit_row = |title: &str| {
        adw::SpinRow::builder()
            .title(title)
            .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 1000.0, 0.5, 5.0, 0.0))
            .digits(1)
            .build()
    };
    let upload_limit_row = limit_row("Upload limit (MiB/s)");
    let download_limit_row = limit_row("Download limit (MiB/s)");
    network_group.add(&upload_limit_row);
    network_group.add(&download_limit_row);

    let appearance_group = adw::PreferencesGroup::builder().title("Appearance").build();
    let accent_row = adw::SwitchRow::builder()
        .title("Proton purple accent")
        .subtitle("Use the Proton brand color instead of the system accent color")
        .build();
    appearance_group.add(&accent_row);
    let tray_row = adw::SwitchRow::builder()
        .title("Show tray icon")
        .subtitle("Sync status and quick actions in the panel")
        .build();
    appearance_group.add(&tray_row);

    let general = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("preferences-system-symbolic")
        .build();
    general.add(&startup_group);
    general.add(&location_group);
    general.add(&network_group);
    general.add(&appearance_group);

    // ---- Preferences: Storage
    let storage_group = adw::PreferencesGroup::builder()
        .title("Cache")
        .description("Pinned and recently opened files, kept on this computer.")
        .build();
    let storage_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    storage_box.set_margin_top(12);
    storage_box.set_margin_bottom(12);
    storage_box.set_margin_start(12);
    storage_box.set_margin_end(12);
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
        .title("Cache size limit (GiB)")
        .subtitle("Older files are removed past this size; 0 means no limit")
        .adjustment(&budget_adj)
        .digits(1)
        .build();
    storage_group.add(&budget_row);
    let purge_row = adw::ActionRow::builder()
        .title("Clear cache")
        .subtitle("Remove cached copies. Files kept offline stay.")
        .build();
    let purge_button = gtk4::Button::builder()
        .label("Clear…")
        .valign(gtk4::Align::Center)
        .build();
    purge_button.add_css_class("flat");
    purge_row.add_suffix(&purge_button);
    storage_group.add(&purge_row);

    // Pins group: filled in by refresh.
    let pins_group = adw::PreferencesGroup::builder()
        .title("Available offline")
        .description("Files kept on this computer, even without a connection.")
        .build();

    let storage = adw::PreferencesPage::builder()
        .title("Storage")
        .icon_name("drive-harddisk-symbolic")
        .build();
    storage.add(&storage_group);
    storage.add(&pins_group);

    let prefs = adw::PreferencesDialog::builder()
        .search_enabled(false)
        .build();
    prefs.add(&general);
    prefs.add(&storage);

    // ---- Sidebar footer
    let status_icon = gtk4::Image::from_icon_name("content-loading-symbolic");
    let status_title = gtk4::Label::builder()
        .label("Connecting…")
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    status_title.add_css_class("heading");
    let status_detail = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .visible(false)
        .build();
    status_detail.add_css_class("caption");
    status_detail.add_css_class("dim-label");
    let status_text = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    status_text.set_hexpand(true);
    status_text.append(&status_title);
    status_text.append(&status_detail);
    let status_content = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    status_content.append(&status_icon);
    status_content.append(&status_text);
    let status_button = gtk4::Button::builder()
        .child(&status_content)
        .tooltip_text("Show sync status")
        .build();
    status_button.add_css_class("flat");
    status_button.add_css_class("sidebar-status");

    let quota_bar = gtk4::ProgressBar::new();
    let quota_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    quota_label.add_css_class("caption");
    quota_label.add_css_class("dim-label");
    let quota_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    quota_box.add_css_class("sidebar-quota");
    quota_box.set_margin_start(8);
    quota_box.set_margin_end(8);
    quota_box.set_visible(false);
    quota_box.append(&quota_bar);
    quota_box.append(&quota_label);

    let avatar = adw::Avatar::new(28, None, true);
    let account_name = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .build();
    let account_menu = gio::Menu::new();
    account_menu.append(Some("Preferences"), Some("win.preferences"));
    let sign_out = gio::Menu::new();
    sign_out.append(Some("Sign Out…"), Some("win.sign-out"));
    account_menu.append_section(None, &sign_out);
    let account_button = gtk4::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Account")
        .menu_model(&account_menu)
        .valign(gtk4::Align::Center)
        .build();
    account_button.add_css_class("flat");
    let account = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    account.add_css_class("sidebar-account");
    account.append(&avatar);
    account.append(&account_name);
    account.append(&account_button);

    let footer = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    footer.add_css_class("sidebar-footer");
    footer.append(&status_button);
    footer.append(&quota_box);
    footer.append(&account);

    MainWidgets {
        prefs,
        footer,
        status_button,
        account_name,
        avatar,
        status_icon,
        status_title,
        status_detail,
        quota_box,
        quota_bar,
        quota_label,
        cache_bar,
        cache_label,
        pins_group,
        autostart_row,
        budget_row,
        upload_limit_row,
        download_limit_row,
        purge_button,
        mountpoint_row,
        mountpoint_button,
        accent_row,
        tray_row,
    }
}

/// The Sync page's live-transfers group, hidden until the refresh loop sees an
/// in-flight transfer from [`Request::GetQueueStatus`].
pub(crate) fn build_transfers_group() -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder()
        .title("Transfers")
        .description("Files moving to and from Proton Drive.")
        .visible(false)
        .build()
}

/// How many pins the Storage preferences list before collapsing the rest behind a
/// "Show all" row. Enough to recognise the list at a glance; short enough that
/// the groups below it stay reachable.
pub(crate) const PINS_COLLAPSED: usize = 6;

/// How long the cache-budget editor waits after the last change before applying
/// it, so a run of `+` clicks is one request rather than one per click.
pub(crate) const BUDGET_DEBOUNCE: Duration = Duration::from_millis(600);

/// Bytes per GiB, for the cache-budget editor's unit conversion.
pub(crate) const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Bytes per MiB, for the bandwidth editors' unit conversion.
pub(crate) const MIB: f64 = 1024.0 * 1024.0;

/// Wire the Preferences controls: the cache-budget editor, the purge button, the
/// start-on-login switch, the mountpoint chooser and the accent toggle. Initial widget state is
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
        .upload_limit_row
        .set_value(config.upload_limit.unwrap_or(0) as f64 / MIB);
    ui.status
        .download_limit_row
        .set_value(config.download_limit.unwrap_or(0) as f64 / MIB);
    ui.status
        .mountpoint_row
        .set_subtitle(&ui.dirs.resolved_mountpoint(&config).display().to_string());
    ui.status.autostart_row.set_active(service::is_enabled());
    ui.status
        .accent_row
        .set_active(config.proton_accent.unwrap_or(false));
    ui.status.tray_row.set_active(!config.tray_hidden);
    ui.status.settings_suppress.set(false);

    // Cache budget: a user edit applies the new soft cap on the daemon (which
    // also persists it to config). 0 GiB = unlimited.
    let ui_budget = ui.clone();
    ui.status.budget_row.connect_value_notify(move |row| {
        if ui_budget.status.settings_suppress.get() {
            return;
        }
        // Replace any pending apply, so only the value the user stops on is sent.
        if let Some(src) = ui_budget.status.budget_source.borrow_mut().take() {
            src.remove();
        }
        let bytes = (row.value() * GIB).round() as u64;
        let ui_fire = ui_budget.clone();
        let src = glib::timeout_add_local_once(BUDGET_DEBOUNCE, move || {
            ui_fire.status.budget_source.borrow_mut().take();
            settings_request(
                &ui_fire,
                Request::SetCacheBudget { bytes },
                "Cache budget updated",
                "Couldn't set cache budget",
            );
        });
        *ui_budget.status.budget_source.borrow_mut() = Some(src);
    });

    // Bandwidth: either editor sends both caps, since the daemon sets them as a
    // pair. Debounced like the budget.
    for row in [&ui.status.upload_limit_row, &ui.status.download_limit_row] {
        let ui_limit = ui.clone();
        row.connect_value_notify(move |_| {
            if ui_limit.status.settings_suppress.get() {
                return;
            }
            if let Some(src) = ui_limit.status.limit_source.borrow_mut().take() {
                src.remove();
            }
            let ui_fire = ui_limit.clone();
            let src = glib::timeout_add_local_once(BUDGET_DEBOUNCE, move || {
                ui_fire.status.limit_source.borrow_mut().take();
                let upload = (ui_fire.status.upload_limit_row.value() * MIB).round() as u64;
                let download = (ui_fire.status.download_limit_row.value() * MIB).round() as u64;
                settings_request(
                    &ui_fire,
                    Request::SetBandwidthLimits { upload, download },
                    "Bandwidth limits updated",
                    "Couldn't set bandwidth limits",
                );
            });
            *ui_limit.status.limit_source.borrow_mut() = Some(src);
        });
    }

    // Purge: confirm, then drop all unpinned cached content via the daemon.
    let ui_purge = ui.clone();
    purge_button.connect_clicked(move |_| {
        let ui = ui_purge.clone();
        let dialog = adw::AlertDialog::builder()
            .heading("Clear Cache?")
            .body(
                "Cached copies are removed from this computer and download again when \
                 you open them. Files kept available offline stay.",
            )
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("purge", "Clear Cache");
        dialog.set_response_appearance("purge", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |_, resp| {
            if resp == "purge" {
                settings_request(
                    &ui,
                    Request::PurgeCache,
                    "Cache cleared",
                    "Couldn't clear the cache",
                );
            }
        });
        dialog.present(Some(&ui_purge.status.prefs));
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

    let ui_mp = ui.clone();
    mountpoint_button.connect_clicked(move |_| prompt_mountpoint(&ui_mp));

    // Accent: saved with the rest of the config, applied right away.
    let ui_accent = ui.clone();
    ui.status.accent_row.connect_active_notify(move |row| {
        if ui_accent.status.settings_suppress.get() {
            return;
        }
        let on = row.is_active();
        set_proton_accent(on);
        let mut config = ui_accent.dirs.load_config();
        config.proton_accent = Some(on);
        if let Err(e) = ui_accent.dirs.save_config(&config) {
            toast_error(&ui_accent, "Couldn't save the accent color", &e.to_string());
        }
    });

    // Tray icon: the tray reads the same flag at start, so hiding it here also
    // keeps the login autostart from bringing it back.
    let ui_tray = ui.clone();
    ui.status.tray_row.connect_active_notify(move |row| {
        if ui_tray.status.settings_suppress.get() {
            return;
        }
        let show = row.is_active();
        let mut config = ui_tray.dirs.load_config();
        config.tray_hidden = !show;
        if let Err(e) = ui_tray.dirs.save_config(&config) {
            toast_error(&ui_tray, "Couldn't save the tray setting", &e.to_string());
            return;
        }
        if show {
            spawn_tray();
        } else {
            pdfs_core::tray::quit(&ui_tray.dirs);
        }
    });
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
        // The Locations page shows the same path as a row title; its cached rows
        // are now stale whether or not it is the visible page.
        ui.locations.loaded_at.set(None);
        if ui.stack.visible_child_name().as_deref() == Some("locations") {
            load_locations(&ui);
        }

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
                if ui.status.account_name.label() != s.username {
                    ui.status.account_name.set_label(&s.username);
                    ui.status.account_name.set_tooltip_text(Some(&s.username));
                    ui.status.avatar.set_text(Some(&s.username));
                }
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
    // A Takeout import outlives the page that started it, so it is polled
    // wherever the user has navigated to — otherwise leaving the page means
    // never being told it finished.
    ui.gallery
        .import_banner
        .set_revealed(ui.takeout.running.get());
    if ui.stack.visible_child_name().as_deref() == Some("takeout") || ui.takeout.running.get() {
        refresh_takeout(ui);
    }
    // The quota sits in the sidebar footer, visible on every page; its TTL
    // keeps this from asking more than once a minute.
    refresh_quota(ui);
    match ui.stack.visible_child_name().as_deref() {
        Some("locations") => {
            refresh_locations(ui);
            refresh_queue(ui);
            refresh_conflicts(ui, false);
        }
        Some("activity") => refresh_activity(ui),
        _ => {}
    }
}

/// How long a quota reading stays fresh. Account storage barely moves, so the
/// active-page tick refetches it only this often rather than every 2s.
const QUOTA_TTL: Duration = Duration::from_secs(60);

/// Fetch the account quota (if the last reading is stale) and paint both the
/// sidebar footer and Files status bar. A failed fetch leaves the last
/// good reading in place.
pub(crate) fn refresh_quota(ui: &Rc<Ui>) {
    if ui.status.quota_inflight.get() {
        return;
    }
    if let Some(at) = ui.status.quota_checked_at.get()
        && at.elapsed() < QUOTA_TTL
    {
        return;
    }
    ui.status.quota_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::AccountQuota);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.status.quota_inflight.set(false);
        if let Ok(Ok(Response::AccountQuota {
            max_space,
            used_space,
        })) = result
        {
            ui.status.quota_checked_at.set(Some(Instant::now()));
            paint_account_quota(&ui, max_space, used_space);
            ui.status.quota_box.set_visible(true);
        } else if ui.status.quota_checked_at.get().is_none() {
            // Match Dolphin: capacity information does not occupy the bar until
            // the backing observer has real figures.
            ui.browser.quota_box.set_visible(false);
        }
    });
}

fn paint_account_quota(ui: &Rc<Ui>, max_space: i64, used_space: i64) {
    let (fraction, text) = quota_display(max_space, used_space);
    ui.status.quota_bar.set_fraction(fraction);
    ui.status.quota_label.set_text(&text);
    if let Some((fraction, free_text, tooltip)) = quota_status_display(max_space, used_space) {
        ui.browser.quota.set_fraction(fraction);
        ui.browser.quota.set_tooltip_text(Some(&tooltip));
        ui.browser.quota_text.set_label(&free_text);
        ui.browser.quota_text.set_tooltip_text(Some(&tooltip));
        ui.browser.quota_box.set_visible(true);
    } else {
        ui.browser.quota_box.set_visible(false);
    }
}

fn quota_display(max_space: i64, used_space: i64) -> (f64, String) {
    let used = used_space.max(0) as u64;
    if max_space <= 0 {
        return (0.0, format!("{} used", human_bytes(used)));
    }
    let total = max_space as u64;
    let fraction = (used as f64 / total as f64).clamp(0.0, 1.0);
    let pct = (fraction * 100.0).round() as u64;
    (
        fraction,
        format!(
            "{} of {} used ({pct}%)",
            human_bytes(used),
            human_bytes(total)
        ),
    )
}

/// Dolphin's status bar shows a bare capacity bar followed by “X free”; the
/// full free/total/percentage sentence is a tooltip rather than inline bar text.
fn quota_status_display(max_space: i64, used_space: i64) -> Option<(f64, String, String)> {
    if max_space <= 0 {
        return None;
    }
    let total = max_space as u64;
    let used = (used_space.max(0) as u64).min(total);
    let free = total.saturating_sub(used);
    let fraction = used as f64 / total as f64;
    let pct = (fraction * 100.0).round() as u64;
    Some((
        fraction,
        format!("{} free", human_bytes(free)),
        format!(
            "{} free out of {} ({pct}% used)",
            human_bytes(free),
            human_bytes(total)
        ),
    ))
}

/// Record the mount state seen by the last status poll: gate every control that
/// needs a live daemon, and notify the desktop when the state actually flips.
///
/// The gating is the point — without it, New Folder / Upload / the details pane's
/// actions stay clickable while the mount is down and each click buys a round-trip
/// that can only fail. A greyed control says so up front.
pub(crate) fn set_mounted(ui: &Rc<Ui>, mounted: bool) {
    *ui.mounted.borrow_mut() = mounted;
    sync_mounted_actions(ui, mounted);
    ui.gallery.upload.set_sensitive(mounted);
    ui.details.details.pin_row.set_sensitive(mounted);
    ui.details.details.rename_button.set_sensitive(mounted);
    ui.details.details.trash_button.set_sensitive(mounted);
    ui.details.details.open_button.set_sensitive(mounted);
    ui.details.details.versions_button.set_sensitive(mounted);

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
    // The Import page reads its progress off the same snapshot rather than
    // polling the daemon a second time for numbers already on the wire.
    takeout_progress(ui, jobs);

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
                failing_ops,
                failing_error,
                paused,
                paused_until,
                ..
            })) => {
                set_mounted(&ui, true);
                // The queue is the more useful thing to say when it has anything
                // in it: it is why a file that looks saved is not on the remote
                // yet, and offline is usually the reason it is still queued.
                let queued = pending_summary(pending_uploads, pending_changes);
                let state = if paused {
                    SyncState::Paused {
                        until: paused_until,
                        queued,
                    }
                } else if failing_ops > 0 {
                    SyncState::Attention {
                        count: failing_ops,
                        error: failing_error,
                    }
                } else if !online {
                    SyncState::Offline { queued }
                } else if let Some(queued) = queued {
                    SyncState::Syncing { queued }
                } else {
                    SyncState::UpToDate { mountpoint }
                };
                paint_sync_status(&ui, state);
                let fraction = if budget == 0 {
                    0.0
                } else {
                    (used as f64 / budget as f64).min(1.0)
                };
                ui.status.cache_bar.set_fraction(fraction);
                // A 0 budget means *unlimited*, not a zero-byte cap — "of 0 B
                // used" reads as a broken read-out, and there is no fraction to
                // draw against no limit.
                ui.status.cache_label.set_text(&if budget == 0 {
                    format!("{} cached — no limit set", human_bytes(used))
                } else {
                    format!("{} of {} used", human_bytes(used), human_bytes(budget))
                });
                repaint_pins(&ui, &pins, true);
            }
            // Daemon unreachable (still starting, or down): report not-mounted and
            // grey out the unpin buttons in place, but leave the last-known pin
            // rows and cache read-out so the page doesn't flicker on a blip.
            _ => {
                set_mounted(&ui, false);
                paint_sync_status(&ui, SyncState::Disconnected);
                for r in ui.status.pin_rows.borrow().iter() {
                    if let Some(b) = &r.unpin {
                        b.set_sensitive(false);
                    }
                }
            }
        }
    });
}

/// What the sidebar's status strip reports, most urgent first.
pub(crate) enum SyncState {
    /// The user paused syncing; nothing goes up until it resumes. Outranks
    /// everything else, because it is the reason for everything else.
    Paused {
        /// Unix second the pause ends by itself; `None` until resumed.
        until: Option<i64>,
        queued: Option<String>,
    },
    /// Operations keep failing; the user may have to act.
    Attention {
        count: u64,
        error: Option<String>,
    },
    /// No connection to Proton: cached files only, changes wait.
    Offline {
        queued: Option<String>,
    },
    /// Local changes are on their way up.
    Syncing {
        queued: String,
    },
    UpToDate {
        mountpoint: String,
    },
    /// No mount daemon answered: still starting, or the service is down.
    Disconnected,
}

/// Paint the sidebar status strip and the Sync page's status card: icon,
/// one-line state, and a detail line.
fn paint_sync_status(ui: &Rc<Ui>, state: SyncState) {
    let paused = matches!(state, SyncState::Paused { .. });
    let connected = !matches!(state, SyncState::Disconnected);
    let (icon, class, title, detail) = match state {
        SyncState::Paused { until, queued } => (
            "media-playback-pause-symbolic",
            Some("warning"),
            "Sync paused".to_string(),
            Some(match (until, queued) {
                (Some(until), Some(queued)) => format!("{queued} · resumes {}", clock_time(until)),
                (Some(until), None) => format!("Resumes {}", clock_time(until)),
                (None, Some(queued)) => queued,
                (None, None) => "Until you resume".to_string(),
            }),
        ),
        SyncState::Attention { count, error } => (
            "dialog-warning-symbolic",
            Some("error"),
            format!(
                "{} need{} attention",
                count_noun(count as usize, "change", "changes"),
                if count == 1 { "s" } else { "" }
            ),
            error,
        ),
        SyncState::Offline { queued } => (
            "network-offline-symbolic",
            Some("warning"),
            "Offline".to_string(),
            Some(queued.unwrap_or_else(|| "Cached files only".to_string())),
        ),
        SyncState::Syncing { queued } => (
            "emblem-synchronizing-symbolic",
            None,
            "Syncing".to_string(),
            Some(queued),
        ),
        SyncState::UpToDate { mountpoint } => (
            "emblem-ok-symbolic",
            Some("success"),
            "Up to date".to_string(),
            Some(mountpoint),
        ),
        SyncState::Disconnected => (
            "network-offline-symbolic",
            Some("warning"),
            "Not connected".to_string(),
            Some("Proton Drive isn't running".to_string()),
        ),
    };
    let image = &ui.status.status_icon;
    image.set_icon_name(Some(icon));
    for c in ["success", "warning", "error"] {
        image.remove_css_class(c);
    }
    if let Some(class) = class {
        image.add_css_class(class);
    }
    ui.status.status_title.set_label(&title);
    ui.status.status_detail.set_visible(detail.is_some());
    ui.status
        .status_detail
        .set_label(detail.as_deref().unwrap_or_default());
    ui.status.status_detail.set_tooltip_text(detail.as_deref());
    paint_sync_card(
        ui,
        icon,
        class,
        &title,
        detail.as_deref(),
        paused,
        connected,
    );
}

/// "at 14:30" today, "Tue 09:00" within the week, a date beyond that.
pub(crate) fn clock_time(unix: i64) -> String {
    let (Ok(at), Ok(now)) = (
        glib::DateTime::from_unix_local(unix),
        glib::DateTime::now_local(),
    ) else {
        return "later".to_string();
    };
    let format = if at.ymd() == now.ymd() {
        "at %H:%M"
    } else if at.difference(&now).as_seconds() < 6 * 86_400 {
        "%a %H:%M"
    } else {
        "%e %b"
    };
    at.format(format)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "later".to_string())
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
            .title("No files kept offline")
            .subtitle("Right-click a file and choose “Make available offline”.")
            .build();
        ui.status.pins_group.add(&row);
        ui.status
            .pin_rows
            .borrow_mut()
            .push(PinRow { row, unpin: None });
        return;
    }

    // Only the first page of pins is rendered; the rest sit behind the row
    // below, so a device with hundreds of pins doesn't bury the groups after it.
    let expanded = ui.status.pins_expanded.get();
    let shown = if expanded {
        pins.len()
    } else {
        pins.len().min(PINS_COLLAPSED)
    };
    for pin in &pins[..shown] {
        let name = Path::new(&pin.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&pin.path)
            .to_string();
        let row = adw::ActionRow::builder()
            .title(&name)
            .subtitle(&pin.path)
            .build();
        let icon = gtk4::Image::from_icon_name("pdfs-offline-symbolic");
        row.add_prefix(&icon);

        let unpin = gtk4::Button::builder()
            .icon_name("pdfs-online-only-symbolic")
            .valign(gtk4::Align::Center)
            .tooltip_text("Make online only")
            .sensitive(mounted)
            .build();
        unpin.add_css_class("flat");
        let ui_btn = ui.clone();
        let path = pin.path.clone();
        unpin.connect_clicked(move |_| {
            let rx = spawn_request(
                ui_btn.dirs.control_socket(),
                Request::Unpin { path: path.clone() },
            );
            let ui = ui_btn.clone();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(Response::Error { message, kind })) => {
                        toast_failure(&ui, "Couldn't make it online only", &message, kind)
                    }
                    Ok(Ok(_)) => refresh(&ui),
                    _ => toast_error(
                        &ui,
                        "Couldn't make it online only",
                        "The mount service didn't respond.",
                    ),
                }
            });
        });
        row.add_suffix(&unpin);

        ui.status.pins_group.add(&row);
        ui.status.pin_rows.borrow_mut().push(PinRow {
            row,
            unpin: Some(unpin),
        });
    }

    if pins.len() > PINS_COLLAPSED {
        let row = adw::ActionRow::builder()
            .title(if expanded {
                "Show fewer".to_string()
            } else {
                format!("Show all {}", count_noun(pins.len(), "file", "files"))
            })
            .activatable(true)
            .build();
        row.add_suffix(&gtk4::Image::from_icon_name(if expanded {
            "go-up-symbolic"
        } else {
            "go-down-symbolic"
        }));
        let ui_more = ui.clone();
        row.connect_activated(move |_| {
            ui_more
                .status
                .pins_expanded
                .set(!ui_more.status.pins_expanded.get());
            // The diff guard compares pin *paths*; the same paths render
            // differently now, so the baseline has to be dropped for the next
            // repaint to actually rebuild.
            *ui_more.status.pins_state.borrow_mut() = None;
            refresh(&ui_more);
        });
        ui.status.pins_group.add(&row);
        ui.status
            .pin_rows
            .borrow_mut()
            .push(PinRow { row, unpin: None });
    }
}

#[cfg(test)]
mod tests {
    use super::{quota_display, quota_status_display};

    #[test]
    fn quota_display_reports_used_total_and_percentage() {
        let gib = 1024_i64.pow(3);
        let (fraction, text) = quota_display(4 * gib, gib);
        assert!((fraction - 0.25).abs() < f64::EPSILON);
        assert_eq!(text, "1.0 GiB of 4.0 GiB used (25%)");
    }

    #[test]
    fn quota_display_clamps_bad_api_values() {
        assert_eq!(quota_display(0, -1), (0.0, "0 B used".to_string()));
        assert_eq!(quota_display(100, 150).0, 1.0);
    }

    #[test]
    fn quota_status_display_matches_dolphin_wording() {
        let gib = 1024_i64.pow(3);
        assert_eq!(
            quota_status_display(4 * gib, gib),
            Some((
                0.25,
                "3.0 GiB free".to_string(),
                "3.0 GiB free out of 4.0 GiB (25% used)".to_string()
            ))
        );
        assert_eq!(quota_status_display(0, 0), None);
    }
}
