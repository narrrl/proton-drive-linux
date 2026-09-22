//! The Locations page: every local place Proton Drive occupies on this machine.
//!
//! One row per [`MountSpec`] from [`Request::ListLocations`] — the primary
//! `~/ProtonDrive` mount, plus each folder this computer backs up, whether it is
//! a mirrored local directory or an on-demand FUSE session. The page is called
//! *Locations* rather than *Mounts* because a mirror folder is a plain directory
//! with no FUSE session behind it (mount-architecture.md §4).
//!
//! The device rows drive the same control requests the Computers page used to:
//! mode switch, sync now, remove. What is new here is that the primary mount is
//! listed alongside them, with the mountpoint chooser that used to live in
//! Settings.

use crate::*;

pub(crate) struct LocationsState {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) rows: RefCell<Vec<gtk4::Widget>>,
    pub(crate) inflight: Cell<bool>,
    pub(crate) loaded_at: Cell<Option<Instant>>,
}

/// Widgets the Locations page's load/repaint touch.
pub(crate) struct LocationsWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) retry: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
    pub(crate) add_folder: gtk4::Button,
}

pub(crate) fn build_locations_page() -> (gtk4::Widget, LocationsWidgets) {
    let title = gtk4::Label::builder()
        .label("Locations")
        .halign(gtk4::Align::Start)
        .build();
    title.add_css_class("title-2");

    let add_folder = gtk4::Button::builder()
        .label("Add Folder")
        .tooltip_text("Back a local folder up to this computer's Proton Drive device")
        .valign(gtk4::Align::Center)
        .build();
    add_folder.add_css_class("flat");
    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title);
    header.append(&titles);
    header.append(&refresh);
    header.append(&add_folder);
    // Clamped to the same width as the rows below, or the page title floats off
    // to the left of the list it names.
    let header_clamp = adw::Clamp::builder().child(&header).build();

    // Same warning the Computers page carried, for the same reason: the
    // on-demand switch removes the local copy, which is not a thing to discover
    // after the fact.
    let group = adw::PreferencesGroup::builder()
        .title("On this computer")
        .description(
            "Where Proton Drive lives on this machine. Synced folders keep a full copy on \
             this disk; on-demand folders keep the files in Proton Drive only and fetch \
             them as you open them.",
        )
        .build();

    let groups = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    groups.append(&group);
    let clamp = adw::Clamp::builder().child(&groups).build();
    // Never scroll sideways: a location's title is a full path, and letting the
    // row grow to fit one pushes the controls at its end off screen. Constrained,
    // the path ellipsizes and the switch/remove/open stay reachable.
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
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
        .icon_name("drive-harddisk-symbolic")
        .title("Loading…")
        .child(&retry)
        .build();
    // A bare `StatusPage` asks for more width than a half-screen window has, and
    // a `Stack` is as wide as its widest child — so an unwrapped status page
    // silently widens the *list* page too, pushing each row's controls off the
    // right edge. Scrolling it (never horizontally) caps that demand.
    let status_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&status)
        .build();

    let content = gtk4::Stack::new();
    content.add_named(&scroll, Some("list"));
    content.add_named(&status_scroll, Some("status"));

    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    page.set_margin_top(18);
    page.set_margin_bottom(18);
    page.set_margin_start(18);
    page.set_margin_end(18);
    page.append(&header_clamp);
    page.append(&content);

    let widgets = LocationsWidgets {
        content: content.clone(),
        status: status.clone(),
        group: group.clone(),
        retry: retry.clone(),
        refresh,
        add_folder,
    };
    (page.upcast(), widgets)
}

pub(crate) fn wire_locations(ui: &Rc<Ui>, retry: &gtk4::Button, add_folder: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        ui_retry.locations.loaded_at.set(None);
        load_locations(&ui_retry);
    });
    let ui_add = ui.clone();
    add_folder.connect_clicked(move |_| prompt_add_sync_folder(&ui_add));
}

/// Show a status page in place of the locations list.
pub(crate) fn locations_status(
    ui: &Rc<Ui>,
    icon: &str,
    title: &str,
    description: &str,
    retry: bool,
) {
    ui.locations.status.set_icon_name(Some(icon));
    ui.locations.status.set_title(title);
    ui.locations.status.set_description(Some(description));
    ui.locations.retry.set_visible(retry);
    ui.locations.content.set_visible_child_name("status");
}

/// Fetch every local location and repaint the list.
pub(crate) fn load_locations(ui: &Rc<Ui>) {
    if ui.locations.inflight.get() {
        return;
    }
    ui.locations.inflight.set(true);
    locations_status(
        ui,
        "drive-harddisk-symbolic",
        "Loading…",
        "Reading this computer's Proton Drive locations.",
        false,
    );
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListLocations);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.locations.inflight.set(false);
        match result {
            Ok(Ok(Response::Locations { items })) => {
                ui.locations.content.set_visible_child_name("list");
                repaint_locations(&ui, &items);
                ui.locations.loaded_at.set(Some(Instant::now()));
            }
            Ok(Ok(Response::Error { message, .. })) => {
                ui.locations.loaded_at.set(None);
                locations_status(
                    &ui,
                    "dialog-warning-symbolic",
                    "Unavailable",
                    &message,
                    true,
                );
            }
            _ => {
                ui.locations.loaded_at.set(None);
                locations_unreachable(&ui);
            }
        }
    });
}

/// Refresh the rows with no status flash and no spinner, from the periodic tick:
/// a sync pass's progress is only live if something re-reads it.
pub(crate) fn refresh_locations(ui: &Rc<Ui>) {
    if ui.locations.inflight.get() {
        return;
    }
    ui.locations.inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListLocations);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.locations.inflight.set(false);
        let Ok(Ok(Response::Locations { items })) = result else {
            return;
        };
        // The page may have been navigated away from, or collapsed to a status
        // view, while the request was in flight.
        if ui.stack.visible_child_name().as_deref() == Some("locations")
            && ui.locations.content.visible_child_name().as_deref() == Some("list")
        {
            repaint_locations(&ui, &items);
        }
    });
}

/// The daemon didn't answer the Locations page.
pub(crate) fn locations_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        locations_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    locations_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("locations") {
            load_locations(&ui);
        }
    });
}

pub(crate) fn repaint_locations(ui: &Rc<Ui>, items: &[MountSpec]) {
    for row in ui.locations.rows.borrow_mut().drain(..) {
        ui.locations.group.remove(&row);
    }
    if items.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No locations")
            .subtitle("The mount service hasn't reported a mountpoint yet.")
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("drive-harddisk-symbolic"));
        ui.locations.group.add(&row);
        *ui.locations.rows.borrow_mut() = vec![row.upcast()];
        return;
    }

    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for spec in items {
        let row = adw::ActionRow::builder()
            .title(&spec.local_path)
            .subtitle(location_subtitle(spec))
            // One line each: a wrapped path would reflow the whole list every
            // time a sync state changed under it.
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name(location_icon(&spec.kind)));

        // A read-only location cannot be written through even where the files are
        // visible; saying so on the row is cheaper than letting the user find out
        // from a save dialog.
        if spec.access == MountAccess::Ro {
            let badge = gtk4::Label::new(Some("Read-only"));
            badge.add_css_class("dim-label");
            badge.add_css_class("caption");
            badge.set_valign(gtk4::Align::Center);
            row.add_suffix(&badge);
        }

        // Same rule as the Computers page: a first pass has no estimate to draw
        // against, so the bar appears only once real counts exist.
        if let Some(p) = &spec.progress
            && p.total > 0
        {
            let bar = gtk4::ProgressBar::builder()
                .fraction((p.done as f64 / p.total.max(p.done) as f64).min(1.0))
                .valign(gtk4::Align::Center)
                .width_request(120)
                .build();
            row.add_suffix(&bar);
        }

        match &spec.kind {
            MountKind::MyFiles => add_my_files_controls(ui, &row),
            MountKind::Device { sync_folder_id } => {
                add_device_controls(ui, &row, spec, *sync_folder_id)
            }
            // A standalone shared mount has no local mode to switch and is not
            // this device's to remove.
            MountKind::Shared { .. } => {}
        }

        let open = gtk4::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Open this folder")
            .valign(gtk4::Align::Center)
            .build();
        open.add_css_class("flat");
        let path = spec.local_path.clone();
        open.connect_clicked(move |_| open_path(&path));
        row.add_suffix(&open);

        ui.locations.group.add(&row);
        rows.push(row.upcast());
    }
    *ui.locations.rows.borrow_mut() = rows;
}

/// The primary mount's only control: where it lives. Changing it rewrites config
/// and offers a service restart, which is why it goes through the shared
/// [`prompt_mountpoint`] dialog.
fn add_my_files_controls(ui: &Rc<Ui>, row: &adw::ActionRow) {
    let change = gtk4::Button::builder()
        .label("Change")
        .tooltip_text("Choose a different folder for the Proton Drive mount")
        .valign(gtk4::Align::Center)
        .build();
    change.add_css_class("flat");
    let ui_mp = ui.clone();
    change.connect_clicked(move |_| prompt_mountpoint(&ui_mp));
    row.add_suffix(&change);
}

/// Sync now (mirror only), the on-demand switch, and Stop syncing for one
/// synced folder.
fn add_device_controls(ui: &Rc<Ui>, row: &adw::ActionRow, spec: &MountSpec, id: i64) {
    if spec.mode != MountMode::OnDemand {
        let sync_now = gtk4::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Sync this folder now")
            .valign(gtk4::Align::Center)
            .build();
        sync_now.add_css_class("flat");
        let ui_sync = ui.clone();
        sync_now.connect_clicked(move |_| sync_folder_now(&ui_sync, id));
        row.add_suffix(&sync_now);
    }

    // A queued switch paints as already flipped: the daemon accepted it and will
    // act on it, so snapping back would read as "it didn't take". The state is
    // set before the handler is wired so painting the current mode doesn't fire
    // a spurious request.
    let target = spec.pending_mode.unwrap_or(spec.mode);
    let ondemand = gtk4::Switch::builder()
        .tooltip_text(
            "On-demand: free this disk by keeping the files in Proton Drive only, \
             fetching each as you open it. Turn off to download them back and keep a \
             full local copy.",
        )
        .valign(gtk4::Align::Center)
        .active(target == MountMode::OnDemand)
        .build();
    let ui_mode = ui.clone();
    let mode_path = spec.local_path.clone();
    ondemand.connect_state_set(move |_, on| {
        if on {
            // Going on-demand deletes the local copies, so it is asked for rather
            // than done on a flick of the switch. Stop leaves the switch's state
            // alone; the next repaint paints whatever the daemon then reports.
            confirm_ondemand(&ui_mode, id, &mode_path);
            glib::Propagation::Stop
        } else {
            set_sync_folder_mode(&ui_mode, id, "mirror");
            glib::Propagation::Proceed
        }
    });
    row.add_suffix(&ondemand);

    let remove = gtk4::Button::builder()
        .icon_name("media-playback-stop-symbolic")
        .tooltip_text("Stop syncing this folder")
        .valign(gtk4::Align::Center)
        .build();
    remove.add_css_class("flat");
    let ui_rm = ui.clone();
    let path = spec.local_path.clone();
    // The folder's *current* mode, not a queued one: a switch that hasn't landed
    // yet has not moved the files anywhere.
    let is_ondemand = spec.mode == MountMode::OnDemand;
    remove.connect_clicked(move |_| prompt_remove_sync_folder(&ui_rm, id, &path, is_ondemand));
    row.add_suffix(&remove);
}

/// Ask before switching a synced folder to on-demand: the switch frees disk
/// space by removing the local copies, which is not something to do by accident.
fn confirm_ondemand(ui: &Rc<Ui>, id: i64, path: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("Make Folder On-Demand?")
        .body(format!(
            "Files in {path} will be removed from this computer and kept in Proton \
             Drive only. Each file downloads again when you open it."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ondemand", "Make On-Demand");
    dialog.set_response_appearance("ondemand", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let win = ui_window(ui);
    let ui = ui.clone();
    dialog.connect_response(None, move |_, response| {
        if response == "ondemand" {
            set_sync_folder_mode(&ui, id, "ondemand");
        } else {
            refresh_locations(&ui);
        }
    });
    dialog.present(win.as_ref());
}

/// Ask the daemon for an immediate pass over one folder.
fn sync_folder_now(ui: &Rc<Ui>, id: i64) {
    let rx = spawn_request(ui.dirs.control_socket(), Request::SyncNow { id: Some(id) });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => toast(&ui, "Syncing folder…"),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't sync", &message, kind)
            }
            _ => toast_error(&ui, "Couldn't sync", "The mount service didn't respond."),
        }
    });
}

pub(crate) fn location_icon(kind: &MountKind) -> &'static str {
    match kind {
        MountKind::MyFiles => "folder-remote-symbolic",
        MountKind::Device { .. } => "folder-symbolic",
        MountKind::Shared { .. } => "system-users-symbolic",
    }
}

/// One line describing what a location *is* and what it is doing.
///
/// Ordered by how likely it is to be what the user came to check: a queued mode
/// switch leads (they just asked for it), then the resting mode, then the sync
/// state, then — only when it is surprising — the fact that no session owns the
/// path. A mirror folder is a plain directory with no FUSE session, so "not
/// mounted" is its normal state and saying it would be noise.
pub(crate) fn location_subtitle(spec: &MountSpec) -> String {
    let mut parts: Vec<String> = Vec::new();
    match &spec.kind {
        MountKind::MyFiles => parts.push("My files".to_string()),
        MountKind::Shared { .. } => parts.push("Shared folder".to_string()),
        MountKind::Device { .. } => match (spec.pending_mode, spec.mode) {
            (Some(MountMode::OnDemand), _) => parts.push("Going on-demand".to_string()),
            (Some(MountMode::Mirror), _) => parts.push("Switching to synced".to_string()),
            (Some(MountMode::Unknown) | None, MountMode::OnDemand) => {
                parts.push("On-demand".to_string())
            }
            (Some(MountMode::Unknown) | None, _) => parts.push("Synced".to_string()),
        },
    }
    if matches!(spec.kind, MountKind::Device { .. }) {
        parts.push(match &spec.progress {
            Some(p) => sync_progress_label(p),
            None => sync_state_label(&spec.state).to_string(),
        });
    }
    // Only the locations that are supposed to have a session report its absence:
    // a mirror folder never has one.
    let expects_session = !matches!(spec.kind, MountKind::Device { .. })
        || spec.mode == MountMode::OnDemand
        || spec.pending_mode == Some(MountMode::OnDemand);
    if expects_session && !spec.mounted {
        parts.push("not mounted".to_string());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: MountKind, mode: MountMode, mounted: bool) -> MountSpec {
        MountSpec {
            id: 1,
            kind,
            local_path: "/home/u/ProtonDrive".into(),
            root_uid: "vol~link".into(),
            root_share_id: "share".into(),
            mode,
            access: MountAccess::Rw,
            state: "idle".into(),
            last_sync: 0,
            pending_mode: None,
            mounted,
            progress: None,
        }
    }

    #[test]
    fn a_mirror_folder_is_never_reported_as_unmounted() {
        // It is a plain local directory: no FUSE session is expected, so saying
        // "not mounted" would describe every healthy mirror folder as broken.
        let spec = spec(
            MountKind::Device { sync_folder_id: 7 },
            MountMode::Mirror,
            false,
        );
        assert_eq!(location_subtitle(&spec), "Synced · up to date");
    }

    #[test]
    fn an_on_demand_folder_without_a_session_says_so() {
        let spec = spec(
            MountKind::Device { sync_folder_id: 7 },
            MountMode::OnDemand,
            false,
        );
        assert_eq!(
            location_subtitle(&spec),
            "On-demand · up to date · not mounted"
        );
    }

    #[test]
    fn a_queued_switch_leads_the_subtitle() {
        let mut spec = spec(
            MountKind::Device { sync_folder_id: 7 },
            MountMode::Mirror,
            true,
        );
        spec.pending_mode = Some(MountMode::OnDemand);
        assert!(location_subtitle(&spec).starts_with("Going on-demand"));
        spec.mode = MountMode::OnDemand;
        spec.pending_mode = Some(MountMode::Mirror);
        assert!(location_subtitle(&spec).starts_with("Switching to synced"));
    }

    /// An unrecognised persisted mode must not silently read as "Synced" *and*
    /// must not lose the rest of the line.
    #[test]
    fn the_primary_mount_reports_only_its_own_facts() {
        let mut spec = spec(MountKind::MyFiles, MountMode::OnDemand, true);
        assert_eq!(location_subtitle(&spec), "My files");
        spec.mounted = false;
        assert_eq!(location_subtitle(&spec), "My files · not mounted");
    }

    #[test]
    fn sync_state_reaches_the_subtitle() {
        let mut spec = spec(
            MountKind::Device { sync_folder_id: 2 },
            MountMode::Mirror,
            true,
        );
        spec.state = "conflict".into();
        assert_eq!(location_subtitle(&spec), "Synced · needs attention");
    }
}
