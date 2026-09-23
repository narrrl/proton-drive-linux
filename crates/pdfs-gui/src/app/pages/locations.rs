//! The Sync page (formerly Locations): what is moving right now, and every local
//! place Proton Drive occupies on this machine.
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
    pub(crate) card: SyncCard,
    pub(crate) queue: QueueState,
    pub(crate) conflicts: ConflictsState,
}

/// The status card heading the Sync page: what sync is doing, and Pause/Resume.
pub(crate) struct SyncCard {
    pub(crate) icon: gtk4::Image,
    pub(crate) row: adw::ActionRow,
    pub(crate) pause: adw::SplitButton,
    /// Whether the last status said paused, so the button knows which way to go.
    pub(crate) paused: Cell<bool>,
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
    /// Live transfers, above the folder list; painted by the refresh loop.
    pub(crate) transfers_group: adw::PreferencesGroup,
    pub(crate) card_icon: gtk4::Image,
    pub(crate) card_row: adw::ActionRow,
    pub(crate) pause: adw::SplitButton,
    pub(crate) queue_group: adw::PreferencesGroup,
    pub(crate) retry_all: gtk4::Button,
    pub(crate) conflicts_group: adw::PreferencesGroup,
}

pub(crate) fn build_locations_page() -> (gtk4::Widget, LocationsWidgets) {
    let add_folder = gtk4::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .label("Add Folder")
                .icon_name("list-add-symbolic")
                .build(),
        )
        .tooltip_text("Back a local folder up to this computer's Proton Drive device")
        .build();
    let refresh = refresh_button();
    let transfers_group = build_transfers_group();
    let (card, card_icon, card_row, pause) = build_sync_card();
    let (queue_group, retry_all) = build_queue_group();
    let conflicts_group = adw::PreferencesGroup::builder()
        .title("Conflicts")
        .description(
            "Files changed in two places at once. Both versions were kept; choose which \
             one stays.",
        )
        .visible(false)
        .build();

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
    groups.append(&card);
    groups.append(&conflicts_group);
    groups.append(&queue_group);
    groups.append(&transfers_group);
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
    page.append(&content);
    let (frame, header, _) = page_frame("Sync", &page);
    header.pack_start(&add_folder);
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
        card_row,
        pause,
        queue_group,
        retry_all,
        conflicts_group,
    };
    (frame.upcast(), widgets)
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
}

/// The status card: a large state icon, the state in words, and Pause/Resume
/// with timed pauses in its menu.
fn build_sync_card() -> (
    adw::PreferencesGroup,
    gtk4::Image,
    adw::ActionRow,
    adw::SplitButton,
) {
    let icon = gtk4::Image::builder()
        .icon_name("emblem-synchronizing-symbolic")
        .pixel_size(32)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    icon.add_css_class("sidebar-status");
    let menu = gio::Menu::new();
    menu.append(Some("Pause for 1 Hour"), Some("sync.pause-for(int64 3600)"));
    menu.append(
        Some("Pause for 8 Hours"),
        Some("sync.pause-for(int64 28800)"),
    );
    menu.append(
        Some("Pause for 24 Hours"),
        Some("sync.pause-for(int64 86400)"),
    );
    let pause = adw::SplitButton::builder()
        .label("Pause")
        .menu_model(&menu)
        .valign(gtk4::Align::Center)
        .tooltip_text("Stop uploading until you resume. Files still open as usual.")
        .dropdown_tooltip("Pause for a while")
        .build();
    let row = adw::ActionRow::builder()
        .title("Checking…")
        .title_lines(1)
        .subtitle_lines(2)
        .build();
    row.add_css_class("property");
    row.add_prefix(&icon);
    row.add_suffix(&pause);
    let group = adw::PreferencesGroup::new();
    group.add(&row);
    (group, icon, row, pause)
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
    card.row.set_title(title);
    card.row.set_subtitle(detail.unwrap_or_default());
    card.paused.set(paused);
    card.pause
        .set_label(if paused { "Resume" } else { "Pause" });
    card.pause.set_tooltip_text(Some(if paused {
        "Upload everything that waited while sync was paused"
    } else {
        "Stop uploading until you resume. Files still open as usual."
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
            "Couldn't pause sync"
        } else {
            "Couldn't resume sync"
        };
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => toast(
                &ui,
                match (paused, until) {
                    (false, _) => "Sync resumed",
                    (true, None) => "Sync paused until you resume",
                    (true, Some(_)) => "Sync paused",
                },
            ),
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, what, &message, kind),
            _ => toast_error(&ui, what, "The mount service didn't respond."),
        }
        ui.locations.card.pause.set_sensitive(true);
        refresh_status(&ui);
    });
}

/// The queue list, hidden while nothing waits.
fn build_queue_group() -> (adw::PreferencesGroup, gtk4::Button) {
    let retry_all = gtk4::Button::builder()
        .label("Retry All")
        .valign(gtk4::Align::Center)
        .tooltip_text("Try every failed change again now")
        .visible(false)
        .build();
    retry_all.add_css_class("flat");
    let group = adw::PreferencesGroup::builder()
        .title("Waiting to Upload")
        .description("Changes made on this computer that are not on Proton Drive yet.")
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
        format!(
            "Changes made on this computer that are not on Proton Drive yet. \
             Showing the first {QUEUE_ROWS_SHOWN} of {}.",
            items.len()
        )
    } else {
        "Changes made on this computer that are not on Proton Drive yet.".to_string()
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
        "revision" => ("pdfs-upload-symbolic", "Upload changes"),
        "create" => ("pdfs-upload-symbolic", "Upload new file"),
        "mkdir" => ("folder-new-symbolic", "Create folder"),
        "rename" => ("document-edit-symbolic", "Rename or move"),
        "trash" => ("user-trash-symbolic", "Move to trash"),
        _ => ("emblem-synchronizing-symbolic", "Change"),
    };
    let state = match (op.parked, op.next_attempt_at) {
        (true, _) => "waiting for the app writing it to finish".to_string(),
        (false, Some(at)) if at > now && op.attempts > 0 => {
            format!("retrying {}", clock_time(at))
        }
        (false, _) if ui.locations.card.paused.get() => "waiting for sync to resume".to_string(),
        (false, _) => "up next".to_string(),
    };
    let mut subtitle = format!("{action} · {state}");
    if let Some(error) = &op.last_error {
        subtitle.push_str(&format!(
            "\nFailed {}: {error}",
            count_noun(op.attempts.max(0) as usize, "time", "times")
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
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Retry now")
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
                toast(&ui, "Retrying now…");
                ui.locations.queue.painted.borrow_mut().clear();
                refresh_queue(&ui);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't retry", &message, kind)
            }
            _ => toast_error(&ui, "Couldn't retry", "The mount service didn't respond."),
        }
    });
}

/// How long a conflict listing stays fresh on the refresh tick.
const CONFLICTS_TTL: Duration = Duration::from_secs(30);

/// Poll the conflict list while the Sync page is on screen. `force` skips the
/// TTL, for navigation and right after a resolution.
pub(crate) fn refresh_conflicts(ui: &Rc<Ui>, force: bool) {
    let conflicts = &ui.locations.conflicts;
    if conflicts.inflight.get()
        || (!force
            && conflicts
                .fetched_at
                .get()
                .is_some_and(|at| at.elapsed() < CONFLICTS_TTL))
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
    format!(
        "{}, changed {}",
        glib::format_size(size),
        clock_time(modified)
    )
}

fn conflict_row(ui: &Rc<Ui>, conflict: &ConflictInfo) -> adw::ActionRow {
    let verdict = if !conflict.original_exists {
        "The original is gone; only this copy is left".to_string()
    } else if conflict.identical {
        "Same content as the original".to_string()
    } else {
        format!("Differs from {}", file_name(&conflict.original_path))
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
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Ask which version of a conflicted file to keep.
fn prompt_resolve_conflict(ui: &Rc<Ui>, conflict: &ConflictInfo) {
    let win = ui_window(ui);
    let copy_name = file_name(&conflict.path).to_string();
    let original_name = file_name(&conflict.original_path).to_string();
    let body = match (conflict.original_size, conflict.original_modified) {
        (Some(size), Some(modified)) if conflict.original_exists => format!(
            "“{original_name}” was changed here and elsewhere at the same time.\n\n\
             Original: {}\nCopy: {}\n\n\
             Whatever is not kept goes to Trash, where it can be restored.",
            conflict_side(size, modified),
            conflict_side(conflict.size, conflict.modified),
        ),
        _ => format!(
            "The original “{original_name}” no longer exists; only the copy is left \
             ({}).\n\nKeep the copy to give it the original name back.",
            conflict_side(conflict.size, conflict.modified),
        ),
    };
    let dialog = adw::AlertDialog::builder()
        .heading("Resolve conflict")
        .body(body)
        .build();
    let group = adw::PreferencesGroup::new();
    let name_row = adw::EntryRow::builder()
        .title("Name for the copy, if keeping both")
        .build();
    name_row.set_text(&copy_name);
    group.add(&name_row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    if conflict.original_exists {
        dialog.add_response("original", "Keep Original");
        dialog.add_response("both", "Keep Both");
    }
    dialog.add_response("copy", "Keep Copy");
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
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't resolve the conflict", &message, kind)
            }
            _ => toast_error(
                &ui,
                "Couldn't resolve the conflict",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// First letter upper-cased, for a daemon message used as a sentence.
fn capitalize(message: &str) -> String {
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
