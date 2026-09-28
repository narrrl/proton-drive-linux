//! The Sync page (formerly Locations), in three views: Overview (what sync
//! is doing, what needs a decision, what is moving), Folders (every local
//! place Proton Drive occupies on this machine) and History (the activity
//! feed, built in `activity.rs`).
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
    pub(crate) rows: RefCell<Vec<LocationRow>>,
    pub(crate) inflight: Cell<bool>,
    /// Shows the loading state if a full load is slow; the rows stay up
    /// until then.
    pub(crate) loader: Rc<Loader>,
    pub(crate) loaded_at: Cell<Option<Instant>>,
    pub(crate) card: SyncCard,
    pub(crate) queue: QueueState,
    pub(crate) conflicts: ConflictsState,
    /// Overview, Folders and History.
    pub(crate) views: adw::ViewStack,
    /// Points at the Overview while conflicts or failures wait there.
    pub(crate) banner: adw::Banner,
    /// Changes the daemon failed to upload, from the last status.
    pub(crate) failing: Cell<u64>,
}

/// The status card heading the Sync page's Overview, and Pause/Resume in the
/// page header.
pub(crate) struct SyncCard {
    pub(crate) icon: gtk4::Image,
    pub(crate) title: gtk4::Label,
    pub(crate) detail: gtk4::Label,
    pub(crate) pause: adw::SplitButton,
    /// Whether the last status said paused, so the button knows which way to go.
    pub(crate) paused: Cell<bool>,
}

/// What a folder row's controls are built from. The subtitle and progress bar
/// change on every tick of a running pass and are updated in place; anything
/// in here changes which controls the row has, so a new key rebuilds it.
pub(crate) type LocationKey = (
    i64,
    MountKind,
    String,
    MountMode,
    Option<MountMode>,
    bool,
    MountAccess,
);

/// One row of the folder list, kept so the refresh tick can update it in place
/// instead of replacing it under the pointer every two seconds.
pub(crate) struct LocationRow {
    /// `None` for the "No locations" placeholder.
    key: Option<LocationKey>,
    row: adw::ActionRow,
    progress: gtk4::ProgressBar,
}

/// What a queue row shows that can change: id, attempts, next attempt, parked.
pub(crate) type QueueKey = (i64, i64, Option<i64>, bool);

/// The "Waiting to Upload" list: the daemon's pending-op queue.
pub(crate) struct QueueState {
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) retry_all: gtk4::Button,
    pub(crate) rows: RefCell<Vec<adw::ActionRow>>,
    /// What the rows were built from, so an unchanged queue is not rebuilt on
    /// every tick.
    pub(crate) painted: RefCell<Vec<QueueKey>>,
    pub(crate) inflight: Cell<bool>,
}

/// What a conflict row shows that can change: path, both sizes, both times.
pub(crate) type ConflictKey = (String, u64, i64, Option<u64>, Option<i64>);

/// The "Conflicts" list: `(sync-conflict …)` copies waiting for a decision.
pub(crate) struct ConflictsState {
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) rows: RefCell<Vec<adw::ActionRow>>,
    pub(crate) painted: RefCell<Vec<ConflictKey>>,
    pub(crate) inflight: Cell<bool>,
    /// When the list was last fetched. Listing walks every node the daemon
    /// knows, so the tick asks far less often than it does for the queue.
    pub(crate) fetched_at: Cell<Option<Instant>>,
}

/// Widgets the Locations page's load/repaint touch.
pub(crate) struct LocationsWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) retry: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
    pub(crate) add_folder: gtk4::Button,
    /// Live transfers, on the Overview; painted by the refresh loop.
    pub(crate) transfers_group: adw::PreferencesGroup,
    pub(crate) card_icon: gtk4::Image,
    pub(crate) card_title: gtk4::Label,
    pub(crate) card_detail: gtk4::Label,
    pub(crate) pause: adw::SplitButton,
    pub(crate) queue_group: adw::PreferencesGroup,
    pub(crate) retry_all: gtk4::Button,
    pub(crate) conflicts_group: adw::PreferencesGroup,
    pub(crate) views: adw::ViewStack,
    pub(crate) banner: adw::Banner,
}

/// The Sync page: Overview (the status, conflicts, transfers and queue),
/// Folders (this computer's synced folders) and History (the activity feed,
/// `history`), switched from the header.
pub(crate) fn build_locations_page(history: &gtk4::Widget) -> (gtk4::Widget, LocationsWidgets) {
    let add_folder = gtk4::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .label(gettext("Add Folder"))
                .icon_name("list-add-symbolic")
                .build(),
        )
        .tooltip_text(gettext(
            "Back a local folder up to this computer's Proton Drive device",
        ))
        .valign(gtk4::Align::Center)
        .build();
    add_folder.add_css_class("flat");
    let refresh = refresh_button();
    let transfers_group = build_transfers_group();
    let (card, card_icon, card_title, card_detail) = build_sync_card();
    let pause = build_pause_button();
    let (queue_group, retry_all) = build_queue_group();
    let conflicts_group = adw::PreferencesGroup::builder()
        .title(gettext("Conflicts"))
        .description(gettext(
            "Files changed in two places at once. Both versions were kept; choose which one stays.",
        ))
        .visible(false)
        .build();

    // Overview: what sync is doing, then what needs a decision, then what is
    // moving.
    let overview = gtk4::Box::new(gtk4::Orientation::Vertical, 24);
    overview.append(&card);
    overview.append(&conflicts_group);
    overview.append(&transfers_group);
    overview.append(&queue_group);

    // Folders: the explanation of the two modes waits behind a "?" rather
    // than sitting over the list as a paragraph.
    let help = gtk4::Label::builder()
        .label(gettext("Synced folders keep a full copy on this computer. Online-only folders fetch each file when you open it."))
        .wrap(true)
        .max_width_chars(40)
        .xalign(0.0)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let help_button = gtk4::MenuButton::builder()
        .icon_name("info-outline-symbolic")
        .tooltip_text(gettext("About Sync Modes"))
        .valign(gtk4::Align::Center)
        .popover(&gtk4::Popover::builder().child(&help).build())
        .build();
    help_button.add_css_class("flat");
    let suffix = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    suffix.append(&help_button);
    suffix.append(&add_folder);
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Folders"))
        .header_suffix(&suffix)
        .build();

    let retry = gtk4::Button::builder()
        .label(gettext("Retry"))
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("drive-harddisk-symbolic")
        .title(gettext("Loading…"))
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
    content.add_named(&scrolled_column(&group), Some("list"));
    content.add_named(&status_scroll, Some("status"));

    let views = adw::ViewStack::new();
    views.set_vexpand(true);
    views.add_titled_with_icon(
        &scrolled_column(&overview),
        Some("overview"),
        &pgettext("sync view", "Overview"),
        "pdfs-sync-symbolic",
    );
    views.add_titled_with_icon(
        &content,
        Some("folders"),
        &pgettext("sync view", "Folders"),
        "folder-symbolic",
    );
    views.add_titled_with_icon(
        history,
        Some("history"),
        &pgettext("sync view", "History"),
        "document-open-recent-symbolic",
    );

    // Conflicts and failures are on the Overview; away from it, a banner
    // says they are there.
    let banner = adw::Banner::builder()
        .button_label(gettext("Review"))
        .build();
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    column.append(&banner);
    column.append(&views);

    let switcher = adw::ViewSwitcher::builder()
        .stack(&views)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let (frame, header) = page_frame_with(&switcher, &column);
    header.pack_start(&pause);
    header.pack_end(&refresh);

    let widgets = LocationsWidgets {
        content: content.clone(),
        status: status.clone(),
        group: group.clone(),
        retry: retry.clone(),
        refresh,
        add_folder,
        transfers_group,
        card_icon,
        card_title,
        card_detail,
        pause,
        queue_group,
        retry_all,
        conflicts_group,
        views,
        banner,
    };
    (frame.upcast(), widgets)
}

/// `child` in a clamp that scrolls vertically only, with page margins.
fn scrolled_column(child: &impl IsA<gtk4::Widget>) -> gtk4::ScrolledWindow {
    let clamp = adw::Clamp::builder().child(child).build();
    clamp.set_margin_top(18);
    clamp.set_margin_bottom(18);
    clamp.set_margin_start(18);
    clamp.set_margin_end(18);
    // Never scroll sideways: a location's subtitle holds a full path, and letting
    // the row grow to fit one pushes the controls at its end off screen.
    // Constrained, the path ellipsizes and the controls stay reachable.
    gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&clamp)
        .build()
}

pub(crate) fn wire_locations(ui: &Rc<Ui>, retry: &gtk4::Button, add_folder: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        ui_retry.locations.loaded_at.set(None);
        load_locations(&ui_retry);
    });
    let ui_add = ui.clone();
    add_folder.connect_clicked(move |_| prompt_add_sync_folder(&ui_add));
    wire_pause(ui);
    let ui_retry = ui.clone();
    ui.locations
        .queue
        .retry_all
        .connect_clicked(move |_| retry_queued(&ui_retry, None));
    let ui_review = ui.clone();
    ui.locations.banner.connect_button_clicked(move |_| {
        ui_review.locations.views.set_visible_child_name("overview");
    });
    let ui_views = ui.clone();
    ui.locations
        .views
        .connect_visible_child_name_notify(move |_| {
            paint_sync_banner(&ui_views);
            if ui_views.stack.visible_child_name().as_deref() == Some("locations") {
                load_sync_view(&ui_views);
            }
        });
}

/// Load what the Sync page's current view shows, on arrival.
pub(crate) fn load_sync_view(ui: &Rc<Ui>) {
    match ui.locations.views.visible_child_name().as_deref() {
        // Activity changes out from under the page as background uploads and
        // edits complete, so it reloads on every visit.
        Some("history") => load_activity(ui),
        _ => {
            refresh_conflicts(ui, true);
            if !page_fresh(&ui.locations.loaded_at) {
                load_locations(ui);
            }
        }
    }
}

/// Reload the Sync page's current view, for Refresh.
pub(crate) fn reload_sync_view(ui: &Rc<Ui>) {
    if ui.locations.views.visible_child_name().as_deref() == Some("history") {
        load_activity(ui);
    } else {
        refresh_conflicts(ui, true);
        load_locations(ui);
    }
}

/// Follow the refresh tick on the Sync page's current view.
pub(crate) fn tick_sync_view(ui: &Rc<Ui>) {
    if ui.locations.views.visible_child_name().as_deref() == Some("history") {
        refresh_activity(ui);
    } else {
        refresh_locations(ui);
        refresh_queue(ui);
        refresh_conflicts(ui, false);
    }
}

/// Show the banner over Folders and History while conflicts or failed
/// changes wait on the Overview, and badge the sidebar's Sync row with them.
pub(crate) fn paint_sync_banner(ui: &Rc<Ui>) {
    let state = &ui.locations;
    let conflicts = state.conflicts.painted.borrow().len() as u64;
    let failing = state.failing.get();
    set_sidebar_badge(ui, "locations", conflicts + failing);
    let title = match (conflicts, failing) {
        (0, 0) => None,
        (0, n) => Some(ngettext_f(
            "{n} change couldn't be uploaded",
            "{n} changes couldn't be uploaded",
            n,
            &[],
        )),
        (n, 0) => Some(ngettext_f(
            "{n} file was changed in two places at once",
            "{n} files were changed in two places at once",
            n,
            &[],
        )),
        _ => Some(gettext("Some files need your attention")),
    };
    let away = state.views.visible_child_name().as_deref() != Some("overview");
    state.banner.set_title(title.as_deref().unwrap_or_default());
    state.banner.set_revealed(away && title.is_some());
}

/// The status hero: a large state icon, the state in words, and what it
/// means.
fn build_sync_card() -> (gtk4::Box, gtk4::Image, gtk4::Label, gtk4::Label) {
    let icon = gtk4::Image::builder()
        .icon_name("pdfs-sync-symbolic")
        .pixel_size(48)
        .valign(gtk4::Align::Center)
        .build();
    icon.add_css_class("sync-card-icon");
    let title = gtk4::Label::builder()
        .label(gettext("Checking…"))
        .xalign(0.0)
        .wrap(true)
        .build();
    title.add_css_class("title-2");
    let detail = gtk4::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .build();
    detail.add_css_class("dim-label");
    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    text.set_valign(gtk4::Align::Center);
    text.set_hexpand(true);
    text.append(&title);
    text.append(&detail);
    let card = gtk4::Box::new(gtk4::Orientation::Horizontal, 18);
    card.add_css_class("card");
    card.add_css_class("sync-card");
    card.append(&icon);
    card.append(&text);
    (card, icon, title, detail)
}

/// Pause/Resume, with timed pauses in its menu.
fn build_pause_button() -> adw::SplitButton {
    let menu = gio::Menu::new();
    menu.append(
        Some(&gettext("Pause for 1 Hour")),
        Some("sync.pause-for(int64 3600)"),
    );
    menu.append(
        Some(&gettext("Pause for 8 Hours")),
        Some("sync.pause-for(int64 28800)"),
    );
    menu.append(
        Some(&gettext("Pause for 24 Hours")),
        Some("sync.pause-for(int64 86400)"),
    );
    adw::SplitButton::builder()
        .label(pgettext("verb", "Pause"))
        .menu_model(&menu)
        .valign(gtk4::Align::Center)
        .tooltip_text(gettext(
            "Stop uploading until you resume. Files still open as usual.",
        ))
        .dropdown_tooltip(gettext("Pause for a while"))
        .build()
}

/// Paint the Sync page's status card from what the sidebar strip shows.
pub(crate) fn paint_sync_card(
    ui: &Rc<Ui>,
    icon: &str,
    class: Option<&str>,
    title: &str,
    detail: Option<&str>,
    paused: bool,
    connected: bool,
) {
    let card = &ui.locations.card;
    card.icon.set_icon_name(Some(icon));
    for c in ["success", "warning", "error"] {
        card.icon.remove_css_class(c);
    }
    if let Some(class) = class {
        card.icon.add_css_class(class);
    }
    card.title.set_label(title);
    card.detail
        .set_visible(detail.is_some_and(|d| !d.is_empty()));
    card.detail.set_label(detail.unwrap_or_default());
    card.paused.set(paused);
    card.pause.set_label(&if paused {
        pgettext("verb", "Resume")
    } else {
        pgettext("verb", "Pause")
    });
    card.pause.set_tooltip_text(Some(&if paused {
        gettext("Upload everything that waited while sync was paused")
    } else {
        gettext("Stop uploading until you resume. Files still open as usual.")
    }));
    if paused {
        card.pause.add_css_class("suggested-action");
    } else {
        card.pause.remove_css_class("suggested-action");
    }
    card.pause.set_sensitive(connected);
}

/// Pause/Resume on the card: the button flips, the menu pauses for a while.
fn wire_pause(ui: &Rc<Ui>) {
    let ui_click = ui.clone();
    ui.locations.card.pause.connect_clicked(move |_| {
        let resume = ui_click.locations.card.paused.get();
        set_sync_paused(&ui_click, !resume, None);
    });
    let actions = gio::SimpleActionGroup::new();
    let pause_for = gio::SimpleAction::new("pause-for", Some(glib::VariantTy::INT64));
    let ui_for = ui.clone();
    pause_for.connect_activate(move |_, param| {
        let Some(secs) = param.and_then(|p| p.get::<i64>()) else {
            return;
        };
        let until = glib::DateTime::now_utc()
            .map(|now| now.to_unix())
            .unwrap_or_default()
            + secs;
        set_sync_paused(&ui_for, true, Some(until));
    });
    actions.add_action(&pause_for);
    ui.locations
        .card
        .pause
        .insert_action_group("sync", Some(&actions));
}

fn set_sync_paused(ui: &Rc<Ui>, paused: bool, until: Option<i64>) {
    ui.locations.card.pause.set_sensitive(false);
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::SetSyncPaused { paused, until },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let what = if paused {
            gettext("Couldn't pause sync")
        } else {
            gettext("Couldn't resume sync")
        };
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => toast(
                &ui,
                &match (paused, until) {
                    (false, _) => gettext("Sync resumed"),
                    (true, None) => gettext("Sync paused until you resume"),
                    (true, Some(_)) => gettext("Sync paused"),
                },
            ),
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, &what, &message, kind),
            _ => toast_error(
                &ui,
                &what,
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
        ui.locations.card.pause.set_sensitive(true);
        refresh_status(&ui);
    });
}

/// The queue list, hidden while nothing waits.
fn build_queue_group() -> (adw::PreferencesGroup, gtk4::Button) {
    let retry_all = gtk4::Button::builder()
        .label(gettext("Retry All"))
        .valign(gtk4::Align::Center)
        .tooltip_text(gettext("Try every failed change again now"))
        .visible(false)
        .build();
    retry_all.add_css_class("flat");
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Waiting to Upload"))
        .description(gettext(
            "Changes made on this computer that are not on Proton Drive yet.",
        ))
        .header_suffix(&retry_all)
        .visible(false)
        .build();
    (group, retry_all)
}

/// Most queue rows shown at once. The queue can hold thousands after a big copy;
/// the first screenful says what is going on, and the count says the rest.
const QUEUE_ROWS_SHOWN: usize = 50;

/// Poll the pending-op queue while the Sync page is on screen.
pub(crate) fn refresh_queue(ui: &Rc<Ui>) {
    if ui.locations.queue.inflight.get() {
        return;
    }
    ui.locations.queue.inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListPendingOps);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.locations.queue.inflight.set(false);
        match result {
            Ok(Ok(Response::PendingOps { items })) => repaint_queue(&ui, &items),
            // An older daemon without the request, or none at all: say nothing
            // rather than show a list that cannot be kept current.
            _ => repaint_queue(&ui, &[]),
        }
    });
}

fn repaint_queue(ui: &Rc<Ui>, items: &[PendingOpInfo]) {
    let queue = &ui.locations.queue;
    // Stuck first — they are why the user is here — then oldest first.
    let mut items: Vec<&PendingOpInfo> = items.iter().collect();
    items.sort_by_key(|op| (!op.failing, op.attempts == 0, op.id));
    let key: Vec<QueueKey> = items
        .iter()
        .map(|op| (op.id, op.attempts, op.next_attempt_at, op.parked))
        .collect();
    if *queue.painted.borrow() == key {
        return;
    }
    *queue.painted.borrow_mut() = key;

    for row in queue.rows.borrow_mut().drain(..) {
        queue.group.remove(&row);
    }
    queue.group.set_visible(!items.is_empty());
    queue
        .retry_all
        .set_visible(items.iter().any(|op| op.attempts > 0 && !op.parked));
    let hidden = items.len().saturating_sub(QUEUE_ROWS_SHOWN);
    queue.group.set_description(Some(&if hidden > 0 {
        let shown = QUEUE_ROWS_SHOWN.to_string();
        // Translators: {shown} is how many rows are listed (50), {n} the total number of queued changes.
        ngettext_f("Changes made on this computer that are not on Proton Drive yet. Showing the first {shown} of {n}.", "Changes made on this computer that are not on Proton Drive yet. Showing the first {shown} of {n}.", items.len() as u64, &[("shown", &shown)])
    } else {
        gettext("Changes made on this computer that are not on Proton Drive yet.")
    }));
    let now = glib::DateTime::now_utc()
        .map(|now| now.to_unix())
        .unwrap_or_default();
    let mut rows = queue.rows.borrow_mut();
    for op in items.into_iter().take(QUEUE_ROWS_SHOWN) {
        let row = queue_row(ui, op, now);
        queue.group.add(&row);
        rows.push(row);
    }
}

fn queue_row(ui: &Rc<Ui>, op: &PendingOpInfo, now: i64) -> adw::ActionRow {
    let (icon, action) = match op.kind.as_str() {
        "revision" => ("pdfs-upload-symbolic", gettext("Upload changes")),
        "create" => ("pdfs-upload-symbolic", gettext("Upload new file")),
        "mkdir" => ("folder-new-symbolic", gettext("Create folder")),
        "rename" => ("document-edit-symbolic", gettext("Rename or move")),
        "trash" => ("user-trash-symbolic", gettext("Move to trash")),
        // Translators: a queued change of an unknown kind.
        _ => ("pdfs-sync-symbolic", pgettext("noun", "Change")),
    };
    let state = match (op.parked, op.next_attempt_at) {
        (true, _) => gettext("waiting for the app writing it to finish"),
        (false, Some(at)) if at > now && op.attempts > 0 => {
            // Translators: {time} is when the next attempt happens, such as "14:30".
            gettext_f("retrying {time}", &[("time", &clock_time(at))])
        }
        (false, _) if ui.locations.card.paused.get() => gettext("waiting for sync to resume"),
        (false, _) => gettext("up next"),
    };
    // Translators: a queued change's kind and its state, such as "Upload changes · up next".
    let mut subtitle = gettext_f(
        "{action} · {state}",
        &[("action", &action), ("state", &state)],
    );
    if let Some(error) = &op.last_error {
        subtitle.push('\n');
        // Translators: {n} is how many attempts failed, {error} the last error message.
        subtitle.push_str(&ngettext_f(
            "Failed {n} time: {error}",
            "Failed {n} times: {error}",
            op.attempts.max(0) as u64,
            &[("error", error)],
        ));
    }
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&op.path).as_str())
        .subtitle(glib::markup_escape_text(&subtitle).as_str())
        .title_lines(1)
        .subtitle_lines(3)
        .tooltip_text(&op.path)
        .build();
    let image = gtk4::Image::from_icon_name(if op.failing {
        "dialog-warning-symbolic"
    } else {
        icon
    });
    if op.failing {
        image.add_css_class("error");
    }
    row.add_prefix(&image);
    let waiting = !op.parked && op.attempts > 0 && op.next_attempt_at.is_some_and(|at| at > now);
    if waiting {
        let retry = gtk4::Button::builder()
            .label(gettext("Retry"))
            .tooltip_text(gettext("Retry now"))
            .valign(gtk4::Align::Center)
            .build();
        retry.add_css_class("flat");
        let ui = ui.clone();
        let id = op.id;
        retry.connect_clicked(move |_| retry_queued(&ui, Some(id)));
        row.add_suffix(&retry);
    }
    row
}

/// Ask the daemon to stop waiting out one op's backoff, or every failed op's.
fn retry_queued(ui: &Rc<Ui>, id: Option<i64>) {
    let rx = spawn_request(ui.dirs.control_socket(), Request::RetryPendingOp { id });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => {
                toast(&ui, &gettext("Retrying now…"));
                ui.locations.queue.painted.borrow_mut().clear();
                refresh_queue(&ui);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &gettext("Couldn't retry"), &message, kind)
            }
            _ => toast_error(
                &ui,
                &gettext("Couldn't retry"),
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
    });
}

/// How long a conflict listing stays fresh on the refresh tick.
pub(crate) const CONFLICTS_TTL: Duration = Duration::from_secs(30);

/// How long a conflict listing stays fresh for the sidebar badge while the
/// Sync page is not on screen.
const CONFLICTS_BADGE_TTL: Duration = Duration::from_secs(300);

/// Poll the conflict list while the Sync page is on screen. `force` skips the
/// TTL, for navigation and right after a resolution.
pub(crate) fn refresh_conflicts(ui: &Rc<Ui>, force: bool) {
    fetch_conflicts(ui, if force { Duration::ZERO } else { CONFLICTS_TTL });
}

/// Poll the conflict list now and then from anywhere, for the sidebar badge.
pub(crate) fn refresh_conflicts_badge(ui: &Rc<Ui>) {
    fetch_conflicts(ui, CONFLICTS_BADGE_TTL);
}

/// Ask for the conflict list unless the last one is younger than `ttl`.
fn fetch_conflicts(ui: &Rc<Ui>, ttl: Duration) {
    let conflicts = &ui.locations.conflicts;
    if conflicts.inflight.get()
        || conflicts
            .fetched_at
            .get()
            .is_some_and(|at| !ttl.is_zero() && at.elapsed() < ttl)
    {
        return;
    }
    conflicts.inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListConflicts);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        let conflicts = &ui.locations.conflicts;
        conflicts.inflight.set(false);
        conflicts.fetched_at.set(Some(Instant::now()));
        match result {
            Ok(Ok(Response::Conflicts { items })) => repaint_conflicts(&ui, &items),
            // Same as the queue: an older daemon, or none, shows nothing.
            _ => repaint_conflicts(&ui, &[]),
        }
    });
}

fn repaint_conflicts(ui: &Rc<Ui>, items: &[ConflictInfo]) {
    let conflicts = &ui.locations.conflicts;
    let key: Vec<ConflictKey> = items
        .iter()
        .map(|c| {
            (
                c.path.clone(),
                c.size,
                c.modified,
                c.original_size,
                c.original_modified,
            )
        })
        .collect();
    if *conflicts.painted.borrow() == key {
        return;
    }
    *conflicts.painted.borrow_mut() = key;
    paint_sync_banner(ui);

    for row in conflicts.rows.borrow_mut().drain(..) {
        conflicts.group.remove(&row);
    }
    conflicts.group.set_visible(!items.is_empty());
    let mut rows = conflicts.rows.borrow_mut();
    for conflict in items {
        let row = conflict_row(ui, conflict);
        conflicts.group.add(&row);
        rows.push(row);
    }
}

/// "12.3 MB, changed at 14:30": one side of a conflict.
fn conflict_side(size: u64, modified: i64) -> String {
    // Translators: one side of a conflict; {size} is a file size such as "12.3 MB", {time} when it was changed, such as "5 min ago" or "Sep 21".
    gettext_f(
        "{size}, changed {time}",
        &[
            ("size", &human_bytes(size)),
            ("time", &dates::relative(modified)),
        ],
    )
}

pub(crate) fn conflict_row(ui: &Rc<Ui>, conflict: &ConflictInfo) -> adw::ActionRow {
    let verdict = if !conflict.original_exists {
        gettext("The original is gone; only this copy is left")
    } else if conflict.identical {
        gettext("Same content as the original")
    } else {
        // Translators: {name} is the original file's name.
        gettext_f(
            "Differs from {name}",
            &[("name", file_name(&conflict.original_path))],
        )
    };
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&conflict.path).as_str())
        .subtitle(glib::markup_escape_text(&verdict).as_str())
        .title_lines(1)
        .tooltip_text(&conflict.path)
        .activatable(true)
        .build();
    let image = gtk4::Image::from_icon_name("dialog-warning-symbolic");
    image.add_css_class("warning");
    row.add_prefix(&image);
    row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    let ui = ui.clone();
    let conflict = conflict.clone();
    row.connect_activated(move |_| prompt_resolve_conflict(&ui, &conflict));
    row
}

/// The last component of a mount-relative path.
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Ask which version of a conflicted file to keep.
pub(crate) fn prompt_resolve_conflict(ui: &Rc<Ui>, conflict: &ConflictInfo) {
    let win = ui_window(ui);
    let copy_name = file_name(&conflict.path).to_string();
    let original_name = file_name(&conflict.original_path).to_string();
    let body = match (conflict.original_size, conflict.original_modified) {
        (Some(size), Some(modified)) if conflict.original_exists => {
            let original = conflict_side(size, modified);
            let copy = conflict_side(conflict.size, conflict.modified);
            // Translators: {name} is the file name; {original} and {copy} each read like "12.3 MB, changed 14:30".
            gettext_f(
                "“{name}” was changed here and elsewhere at the same time.\n\nOriginal: {original}\nCopy: {copy}\n\nWhatever is not kept goes to Trash, where it can be restored.",
                &[
                    ("name", &original_name),
                    ("original", &original),
                    ("copy", &copy),
                ],
            )
        }
        _ => {
            let copy = conflict_side(conflict.size, conflict.modified);
            // Translators: {name} is the file name; {copy} reads like "12.3 MB, changed 14:30".
            gettext_f(
                "The original “{name}” no longer exists; only the copy is left ({copy}).\n\nKeep the copy to give it the original name back.",
                &[("name", &original_name), ("copy", &copy)],
            )
        }
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Resolve Conflict"))
        .body(body)
        .build();
    let group = adw::PreferencesGroup::new();
    let name_row = adw::EntryRow::builder()
        .title(gettext("Name for the copy, if keeping both"))
        .build();
    name_row.set_text(&copy_name);
    group.add(&name_row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", &gettext("Cancel"));
    if conflict.original_exists {
        dialog.add_response("original", &gettext("Keep Original"));
        dialog.add_response("both", &gettext("Keep Both"));
    }
    dialog.add_response("copy", &gettext("Keep Copy"));
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    // Keeping both is a rename, so it needs a name that is neither side's.
    let update_both = {
        let dialog = dialog.clone();
        let copy_name = copy_name.clone();
        move |row: &adw::EntryRow| {
            let name = row.text();
            let name = name.trim();
            dialog.set_response_enabled(
                "both",
                !name.is_empty()
                    && name != copy_name
                    && name != original_name
                    && !name.contains('/'),
            );
        }
    };
    if conflict.original_exists {
        update_both(&name_row);
        name_row.connect_changed(update_both);
    }
    let ui = ui.clone();
    let path = conflict.path.clone();
    dialog.connect_response(None, move |_, resp| {
        let keep = match resp {
            "original" => ConflictKeep::Original,
            "copy" => ConflictKeep::Copy,
            "both" => ConflictKeep::Both {
                name: name_row.text().trim().to_string(),
            },
            _ => return,
        };
        resolve_conflict(&ui, path.clone(), keep);
    });
    dialog.present(win.as_ref());
}

fn resolve_conflict(ui: &Rc<Ui>, path: String, keep: ConflictKeep) {
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ResolveConflict { path, keep },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { message })) => {
                toast(&ui, &capitalize(&message));
                refresh_conflicts(&ui, true);
                refresh_activity_conflicts(&ui, true);
            }
            Ok(Ok(Response::Error { message, kind })) => toast_failure(
                &ui,
                &gettext("Couldn't resolve the conflict"),
                &message,
                kind,
            ),
            _ => toast_error(
                &ui,
                &gettext("Couldn't resolve the conflict"),
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
    });
}

/// First letter upper-cased, for a daemon message used as a sentence.
pub(crate) fn capitalize(message: &str) -> String {
    let mut chars = message.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
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
    let ui_p = ui.clone();
    let ticket = ui.locations.loader.refresh(move || {
        locations_status(
            &ui_p,
            "drive-harddisk-symbolic",
            &gettext("Loading…"),
            &gettext("Reading this computer's Proton Drive locations."),
            false,
        );
    });
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListLocations);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.locations.inflight.set(false);
        drop(ticket);
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
                    &gettext("Unavailable"),
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
    service_unreachable(ui, "locations", locations_status, load_locations);
}

/// Paint the folder list. The rows are rebuilt only when what they offer
/// changes (a mode switch, a pause, a folder added); otherwise the tick updates
/// their subtitle and progress in place, so a menu, tooltip or keyboard focus
/// on a row survives the refresh.
pub(crate) fn repaint_locations(ui: &Rc<Ui>, items: &[MountSpec]) {
    let keys: Vec<Option<LocationKey>> = if items.is_empty() {
        vec![None]
    } else {
        items.iter().map(|spec| Some(location_key(spec))).collect()
    };
    let unchanged = {
        let rows = ui.locations.rows.borrow();
        rows.len() == keys.len() && rows.iter().zip(&keys).all(|(row, key)| row.key == *key)
    };
    if !unchanged {
        rebuild_locations(ui, items, keys);
    }
    for (row, spec) in ui.locations.rows.borrow().iter().zip(items) {
        update_location_row(row, spec);
    }
}

fn location_key(spec: &MountSpec) -> LocationKey {
    (
        spec.id,
        spec.kind.clone(),
        spec.local_path.clone(),
        spec.mode,
        spec.pending_mode,
        spec.paused,
        spec.access,
    )
}

fn rebuild_locations(ui: &Rc<Ui>, items: &[MountSpec], keys: Vec<Option<LocationKey>>) {
    for row in ui.locations.rows.borrow_mut().drain(..) {
        ui.locations.group.remove(&row.row);
    }
    let mut rows = Vec::new();
    if items.is_empty() {
        let row = adw::ActionRow::builder()
            .title(gettext("No folders yet"))
            .subtitle(gettext(
                "The Proton Drive service hasn't reported its folder yet.",
            ))
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("folder-symbolic"));
        ui.locations.group.add(&row);
        rows.push(LocationRow {
            key: None,
            row,
            progress: gtk4::ProgressBar::new(),
        });
    }
    for (spec, key) in items.iter().zip(keys) {
        let row = location_row(ui, spec);
        ui.locations.group.add(&row.row);
        rows.push(LocationRow { key, ..row });
    }
    *ui.locations.rows.borrow_mut() = rows;
}

/// The parts of a folder row that move while a pass runs.
fn update_location_row(row: &LocationRow, spec: &MountSpec) {
    // Translators: a synced folder's location and state, such as "~/Documents · Synced · up to date".
    let subtitle = gettext_f(
        "{path} · {state}",
        &[
            ("path", &tilde_path(&spec.local_path)),
            ("state", &location_subtitle(spec)),
        ],
    );
    let subtitle = glib::markup_escape_text(&subtitle);
    if row.row.subtitle().as_deref() != Some(subtitle.as_str()) {
        row.row.set_subtitle(&subtitle);
    }
    // A first pass has no estimate to draw against, so the bar appears only
    // once real counts exist.
    match &spec.progress {
        Some(p) if p.total > 0 => {
            row.progress
                .set_fraction((p.done as f64 / p.total.max(p.done) as f64).min(1.0));
            row.progress.set_visible(true);
        }
        _ => row.progress.set_visible(false),
    }
}

/// A folder row: its name, where it is and what it is doing, the folder's mode
/// as a button that names it, and one menu for everything else. Activating the
/// row opens the folder.
fn location_row(ui: &Rc<Ui>, spec: &MountSpec) -> LocationRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&location_name(spec)).as_str())
        .tooltip_text(&spec.local_path)
        // One line each: a wrapped path would reflow the whole list every time
        // a sync state changed under it.
        .title_lines(1)
        .subtitle_lines(1)
        .activatable(true)
        .build();
    row.add_prefix(&gtk4::Image::from_icon_name(location_icon(&spec.kind)));
    let path = spec.local_path.clone();
    row.connect_activated(move |_| open_path(&path));

    // A read-only location cannot be written through even where the files are
    // visible; saying so on the row is cheaper than letting the user find out
    // from a save dialog.
    if spec.access == MountAccess::Ro {
        let badge = gtk4::Label::new(Some(&gettext("Read-only")));
        badge.add_css_class("dim-label");
        badge.add_css_class("caption");
        badge.set_valign(gtk4::Align::Center);
        row.add_suffix(&badge);
    }

    let progress = gtk4::ProgressBar::builder()
        .valign(gtk4::Align::Center)
        .width_request(120)
        .visible(false)
        .build();
    row.add_suffix(&progress);

    let mut menu = ActionMenu::new();
    let path = spec.local_path.clone();
    menu.item(&gettext("Open in Files"), move || open_path(&path));
    menu.section();
    match &spec.kind {
        MountKind::MyFiles => {
            let ui = ui.clone();
            menu.item(&gettext("Change Location…"), move || {
                prompt_mountpoint(&ui)
            });
        }
        MountKind::Device { sync_folder_id } => {
            row.add_suffix(&mode_button(ui, spec, *sync_folder_id));
            add_device_items(ui, &mut menu, spec, *sync_folder_id);
        }
        // A standalone shared mount has no local mode to switch and is not
        // this device's to remove.
        MountKind::Shared { .. } => {}
    }
    row.add_suffix(&menu.button());

    let row = LocationRow {
        key: None,
        row,
        progress,
    };
    update_location_row(&row, spec);
    row
}

/// What a folder row is called: the folder's own name, which is the part of
/// its path a person recognises.
fn location_name(spec: &MountSpec) -> String {
    Path::new(&spec.local_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&spec.local_path)
        .to_string()
}

/// `path` with the home directory written as `~`.
pub(crate) fn tilde_path(path: &str) -> String {
    let home = glib::home_dir();
    match Path::new(path).strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_string(),
    }
}

/// The button naming a synced folder's mode, whose popover switches it. It
/// shows the mode the folder is heading for: a queued switch was accepted and
/// will happen, so showing the old mode would read as "it didn't take".
fn mode_button(ui: &Rc<Ui>, spec: &MountSpec, id: i64) -> gtk4::MenuButton {
    let target = spec.pending_mode.unwrap_or(spec.mode);
    let ondemand = target == MountMode::OnDemand;
    let button = gtk4::MenuButton::builder()
        .label(folder_mode_label(ondemand))
        .tooltip_text(gettext(
            "Choose whether this folder keeps a full copy on this computer",
        ))
        .valign(gtk4::Align::Center)
        .always_show_arrow(true)
        // The daemon refuses a mode switch on a paused folder.
        .sensitive(!spec.paused)
        .build();
    button.add_css_class("flat");

    let options = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    let popover = gtk4::Popover::builder().child(&options).build();
    popover.add_css_class("menu");
    for (choice, title, description) in [
        (
            false,
            folder_mode_label(false),
            gettext("Keep every file on this computer and in Proton Drive."),
        ),
        (
            true,
            folder_mode_label(true),
            gettext("Keep the files in Proton Drive and download each one when you open it."),
        ),
    ] {
        let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        text.append(
            &gtk4::Label::builder()
                .label(&title)
                .xalign(0.0)
                .css_classes(["heading"])
                .build(),
        );
        text.append(
            &gtk4::Label::builder()
                .label(&description)
                .xalign(0.0)
                .wrap(true)
                .max_width_chars(36)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let check = gtk4::Image::from_icon_name("object-select-symbolic");
        check.set_opacity(if choice == ondemand { 1.0 } else { 0.0 });
        let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        content.append(&text);
        content.append(&check);
        text.set_hexpand(true);
        let option = gtk4::Button::builder().child(&content).build();
        option.add_css_class("flat");
        let ui = ui.clone();
        let path = spec.local_path.clone();
        let popover_ref = popover.downgrade();
        option.connect_clicked(move |_| {
            if let Some(popover) = popover_ref.upgrade() {
                popover.popdown();
            }
            if choice == ondemand {
                return;
            }
            if choice {
                confirm_online_only(&ui, id, &path);
            } else {
                confirm_synced(&ui, id, &path);
            }
        });
        options.append(&option);
    }
    button.set_popover(Some(&popover));
    button
}

/// A synced folder's mode, as the row's mode button names it.
fn folder_mode_label(ondemand: bool) -> String {
    if ondemand {
        // Translators: a folder mode: files are kept in Proton Drive and fetched when opened.
        pgettext("folder mode", "Online only")
    } else {
        // Translators: a folder mode: a full copy is kept on this computer.
        pgettext("folder mode", "Synced")
    }
}

/// Sync now, pause or resume (synced folders only), and Stop syncing, in a
/// synced folder's menu.
fn add_device_items(ui: &Rc<Ui>, menu: &mut ActionMenu, spec: &MountSpec, id: i64) {
    if spec.mode != MountMode::OnDemand {
        // A paused folder skips its passes, so Sync now would do nothing.
        if !spec.paused {
            let ui = ui.clone();
            menu.item(&gettext("Sync Now"), move || sync_folder_now(&ui, id));
        }
        let paused = spec.paused;
        let label = if paused {
            gettext("Resume Syncing")
        } else {
            gettext("Pause Syncing")
        };
        let ui = ui.clone();
        menu.item(&label, move || set_folder_paused(&ui, id, !paused));
        menu.section();
    }
    let ui = ui.clone();
    let path = spec.local_path.clone();
    // The folder's *current* mode, not a queued one: a switch that hasn't landed
    // yet has not moved the files anywhere.
    let is_ondemand = spec.mode == MountMode::OnDemand;
    menu.item(&gettext("Stop Syncing…"), move || {
        prompt_remove_sync_folder(&ui, id, &path, is_ondemand)
    });
}

/// Ask before making a synced folder online only: the switch frees disk space
/// by removing the local copies, which is not something to do by accident.
fn confirm_online_only(ui: &Rc<Ui>, id: i64, path: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Make Folder Online Only?"))
        // Translators: {path} is a local folder path.
        .body(gettext_f("Files in {path} will be removed from this computer and kept in Proton Drive only. Each file downloads again when you open it.", &[("path", path)]))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("ondemand", &gettext("Make Online Only"));
    dialog.set_response_appearance("ondemand", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let win = ui_window(ui);
    let ui = ui.clone();
    dialog.connect_response(None, move |_, response| {
        if response == "ondemand" {
            set_sync_folder_mode(&ui, id, "ondemand");
        }
    });
    dialog.present(win.as_ref());
}

/// Ask before making an online-only folder synced: every file downloads, which
/// can fill the disk. The free space is read from the folder's parent, which is
/// on the local disk; the folder itself is a FUSE mount that may not answer.
fn confirm_synced(ui: &Rc<Ui>, id: i64, path: &str) {
    let ui = ui.clone();
    let path = path.to_string();
    glib::spawn_future_local(async move {
        let parent = Path::new(&path)
            .parent()
            .map(gio::File::for_path)
            .unwrap_or_else(|| gio::File::for_path(&path));
        let free = parent
            .query_filesystem_info_future(
                gio::FILE_ATTRIBUTE_FILESYSTEM_FREE,
                glib::Priority::DEFAULT,
            )
            .await
            .ok()
            .map(|info| info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE));
        let body = match free {
            // Translators: {path} is a local folder path, {free} the free disk space such as "12.3 GiB".
            Some(free) => gettext_f(
                "Every file in {path} will download to this computer and stay synced. {free} is free on this disk.",
                &[("path", &path), ("free", &human_bytes(free))],
            ),
            // Translators: {path} is a local folder path.
            None => gettext_f(
                "Every file in {path} will download to this computer and stay synced.",
                &[("path", &path)],
            ),
        };
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Download Folder?"))
            .body(body)
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("mirror", &gettext("Download"));
        dialog.set_response_appearance("mirror", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("mirror"));
        dialog.set_close_response("cancel");
        let win = ui_window(&ui);
        let ui = ui.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "mirror" {
                set_sync_folder_mode(&ui, id, "mirror");
            }
        });
        dialog.present(win.as_ref());
    });
}

/// Pause or resume one synced folder, then repaint so its row shows the result.
fn set_folder_paused(ui: &Rc<Ui>, id: i64, paused: bool) {
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::SetSyncFolderPaused { id, paused },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let failed = match paused {
            true => gettext("Couldn't pause the folder"),
            false => gettext("Couldn't resume the folder"),
        };
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => toast(
                &ui,
                &match paused {
                    true => gettext("Folder paused"),
                    false => gettext("Folder resumed"),
                },
            ),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &failed, &message, kind)
            }
            _ => toast_error(
                &ui,
                &failed,
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
        refresh_locations(&ui);
    });
}

/// Ask the daemon for an immediate pass over one folder.
fn sync_folder_now(ui: &Rc<Ui>, id: i64) {
    let rx = spawn_request(ui.dirs.control_socket(), Request::SyncNow { id: Some(id) });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => toast(&ui, &gettext("Syncing folder…")),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &gettext("Couldn't sync"), &message, kind)
            }
            _ => toast_error(
                &ui,
                &gettext("Couldn't sync"),
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
    });
}

pub(crate) fn location_icon(kind: &MountKind) -> &'static str {
    match kind {
        MountKind::MyFiles => "folder-remote-symbolic",
        MountKind::Device { .. } => "folder-symbolic",
        MountKind::Shared { .. } => "pdfs-people-symbolic",
    }
}

/// One line describing what a location *is* and what it is doing.
///
/// Ordered by how likely it is to be what the user came to check: a queued mode
/// switch leads (they just asked for it), then the resting mode, then the sync
/// state, then — only when it is surprising — the fact that no session owns the
/// path. A mirror folder is a plain directory with no FUSE session, so having
/// none is its normal state and saying so would be noise.
pub(crate) fn location_subtitle(spec: &MountSpec) -> String {
    let mut parts: Vec<String> = Vec::new();
    match &spec.kind {
        MountKind::MyFiles => parts.push(gettext("My files")),
        MountKind::Shared { .. } => parts.push(gettext("Shared folder")),
        MountKind::Device { .. } => match (spec.pending_mode, spec.mode) {
            (Some(MountMode::OnDemand), _) => parts.push(gettext("Going online only")),
            (Some(MountMode::Mirror), _) => parts.push(gettext("Switching to synced")),
            (Some(MountMode::Unknown) | None, MountMode::OnDemand) => {
                parts.push(folder_mode_label(true))
            }
            (Some(MountMode::Unknown) | None, _) => parts.push(folder_mode_label(false)),
        },
    }
    if matches!(spec.kind, MountKind::Device { .. }) {
        parts.push(match &spec.progress {
            Some(p) => sync_progress_label(p),
            None if spec.paused => pgettext("state", "paused"),
            None => sync_state_label(&spec.state).to_string(),
        });
    }
    // Only the locations that are supposed to have a session report its absence:
    // a mirror folder never has one.
    let expects_session = !matches!(spec.kind, MountKind::Device { .. })
        || spec.mode == MountMode::OnDemand
        || spec.pending_mode == Some(MountMode::OnDemand);
    if expects_session && !spec.mounted {
        // Translators: a folder's files can't be opened right now.
        parts.push(gettext("not available"));
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
            paused: false,
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
    fn a_paused_folder_says_paused_instead_of_its_state() {
        let mut spec = spec(
            MountKind::Device { sync_folder_id: 7 },
            MountMode::Mirror,
            false,
        );
        spec.paused = true;
        assert_eq!(location_subtitle(&spec), "Synced · paused");
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
            "Online only · up to date · not available"
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
        assert!(location_subtitle(&spec).starts_with("Going online only"));
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
        assert_eq!(location_subtitle(&spec), "My files · not available");
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
