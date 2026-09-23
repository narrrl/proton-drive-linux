use crate::*;

pub(crate) struct ActivityState {
    // Activity page: a "Needs attention" section for live conflicts, then the
    // feed grouped by day, rebuilt wholesale on each change.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) attention: adw::PreferencesGroup,
    pub(crate) attention_rows: RefCell<Vec<adw::ActionRow>>,
    pub(crate) days: gtk4::Box,
    pub(crate) inflight: Cell<bool>,
    /// Fingerprint of what is on screen (see [`activity_key`]). The page polls
    /// every couple of seconds and usually gets back exactly what it is already
    /// showing; rebuilding every row for that would throw away the user's
    /// scroll position several times a minute.
    pub(crate) key: RefCell<Option<String>>,
    /// The last feed the daemon sent, so a filter change repaints without a
    /// round-trip.
    pub(crate) items: RefCell<Vec<ActivityEntry>>,
    pub(crate) filter: Cell<ActivityFilter>,
    /// Conflict copies still waiting for a decision, or `None` before the first
    /// listing. Lets a logged conflict read as resolved once it is gone.
    pub(crate) conflicts: RefCell<Option<Vec<ConflictInfo>>>,
    pub(crate) conflicts_inflight: Cell<bool>,
    pub(crate) conflicts_at: Cell<Option<Instant>>,
}

/// Widgets the Activity page's load/repaint touch.
pub(crate) struct ActivityWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) attention: adw::PreferencesGroup,
    pub(crate) days: gtk4::Box,
    pub(crate) filters: Vec<(ActivityFilter, gtk4::ToggleButton)>,
    pub(crate) retry: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
}

/// Which slice of the feed the filter bar shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivityFilter {
    All,
    Transfers,
    Changes,
    Sharing,
    Problems,
}

impl ActivityFilter {
    const ALL: [ActivityFilter; 5] = [
        ActivityFilter::All,
        ActivityFilter::Transfers,
        ActivityFilter::Changes,
        ActivityFilter::Sharing,
        ActivityFilter::Problems,
    ];

    fn label(self) -> &'static str {
        match self {
            ActivityFilter::All => "All",
            ActivityFilter::Transfers => "Transfers",
            ActivityFilter::Changes => "Changes",
            ActivityFilter::Sharing => "Sharing",
            ActivityFilter::Problems => "Problems",
        }
    }

    /// Whether an entry belongs in this slice. A failed action is a problem
    /// whatever it was, and also stays under its own kind.
    pub(crate) fn matches(self, entry: &ActivityEntry) -> bool {
        use ActivityKind::*;
        match self {
            ActivityFilter::All => true,
            ActivityFilter::Transfers => matches!(entry.kind, Upload | Download | Sync),
            ActivityFilter::Changes => matches!(
                entry.kind,
                Rename | Move | CreateFolder | Trash | Restore | DeleteForever | EmptyTrash
            ),
            ActivityFilter::Sharing => matches!(entry.kind, Share | PublicLink | Unshare),
            ActivityFilter::Problems => !entry.ok || entry.kind == Conflict,
        }
    }
}

/// The Activity page: a newest-first feed of the mutations and transfers the
/// daemon performed (uploads, deletes, shares, …), grouped by day, with the
/// conflicts that still need a decision on top.
pub(crate) fn build_activity_page() -> (gtk4::Widget, ActivityWidgets) {
    let refresh = refresh_button();

    // Filter chips: a linked row of radio toggles.
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("linked");
    bar.set_halign(gtk4::Align::Start);
    let mut filters: Vec<(ActivityFilter, gtk4::ToggleButton)> = Vec::new();
    for filter in ActivityFilter::ALL {
        let button = gtk4::ToggleButton::with_label(filter.label());
        if let Some((_, first)) = filters.first() {
            button.set_group(Some(first));
        } else {
            button.set_active(true);
        }
        bar.append(&button);
        filters.push((filter, button));
    }

    let attention = adw::PreferencesGroup::builder()
        .title("Needs attention")
        .visible(false)
        .build();
    let days = gtk4::Box::new(gtk4::Orientation::Vertical, 24);

    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 24);
    column.set_margin_top(18);
    column.set_margin_bottom(18);
    column.set_margin_start(18);
    column.set_margin_end(18);
    column.append(&bar);
    column.append(&attention);
    column.append(&days);
    let clamp = adw::Clamp::builder()
        .maximum_size(900)
        .tightening_threshold(600)
        .child(&column)
        .build();
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
        .icon_name("document-open-recent-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&scroll, Some("list"));
    content.add_named(&status, Some("status"));

    let (frame, header, _) = page_frame("Activity", &content);
    header.pack_end(&refresh);

    (
        frame.upcast(),
        ActivityWidgets {
            content,
            status,
            attention,
            days,
            filters,
            retry,
            refresh,
        },
    )
}

/// Install the Activity page's retry button.
pub(crate) fn wire_activity(
    ui: &Rc<Ui>,
    retry: &gtk4::Button,
    filters: &[(ActivityFilter, gtk4::ToggleButton)],
) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_activity(&ui_retry);
    });
    for (filter, button) in filters {
        let ui = ui.clone();
        let filter = *filter;
        button.connect_toggled(move |button| {
            if button.is_active() {
                ui.activity.filter.set(filter);
                paint_activity(&ui);
            }
        });
    }
}

/// Show a status page in place of the Activity list.
pub(crate) fn activity_status(
    ui: &Rc<Ui>,
    icon: &str,
    title: &str,
    description: &str,
    retry: bool,
) {
    ui.activity.status.set_icon_name(Some(icon));
    ui.activity.status.set_title(title);
    ui.activity.status.set_description(Some(description));
    ui.activity.retry.set_visible(retry);
    ui.activity.content.set_visible_child_name("status");
    // The list is no longer what is on screen, so the next repaint must not skip
    // itself as a no-op and leave this status view up.
    *ui.activity.key.borrow_mut() = None;
}

/// Refresh the Activity feed in place, with no status flash and no spinner.
/// Driven by the periodic tick while the page is on screen, so a running sync
/// pass fills the feed as it works rather than only once it is done. Anything
/// other than a good answer leaves the rows alone until the next tick.
pub(crate) fn refresh_activity(ui: &Rc<Ui>) {
    refresh_activity_conflicts(ui, false);
    if ui.activity.inflight.get() {
        return;
    }
    ui.activity.inflight.set(true);
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListActivity { limit: 200 },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.activity.inflight.set(false);
        // The page may have been navigated away from while the request was in
        // flight.
        if let Ok(Ok(Response::Activity { items })) = result
            && ui.stack.visible_child_name().as_deref() == Some("activity")
        {
            repaint_activity(&ui, &items);
        }
    });
}

pub(crate) fn load_activity(ui: &Rc<Ui>) {
    refresh_activity_conflicts(ui, true);
    if ui.activity.inflight.get() {
        return;
    }
    ui.activity.inflight.set(true);
    activity_status(
        ui,
        "document-open-recent-symbolic",
        "Loading…",
        "Reading recent activity.",
        false,
    );
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListActivity { limit: 200 },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.activity.inflight.set(false);
        match result {
            Ok(Ok(Response::Activity { items })) => repaint_activity(&ui, &items),
            Ok(Ok(Response::Error { message, .. })) => activity_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read activity",
                &message,
                false,
            ),
            Ok(Ok(_)) => activity_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read activity",
                "Unexpected reply from the mount service.",
                false,
            ),
            Ok(Err(_)) | Err(_) => activity_unreachable(&ui),
        }
    });
}

/// The daemon didn't answer the Activity page.
pub(crate) fn activity_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        activity_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    activity_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("activity") {
            load_activity(&ui);
        }
    });
}

/// Poll the conflicts still waiting for a decision while the Activity page is
/// on screen. `force` skips the TTL, for navigation and right after a
/// resolution. Listing walks every node the daemon knows, so the tick asks far
/// less often than it does for the feed.
pub(crate) fn refresh_activity_conflicts(ui: &Rc<Ui>, force: bool) {
    let state = &ui.activity;
    if state.conflicts_inflight.get()
        || (!force
            && state
                .conflicts_at
                .get()
                .is_some_and(|at| at.elapsed() < CONFLICTS_TTL))
    {
        return;
    }
    state.conflicts_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListConflicts);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        let state = &ui.activity;
        state.conflicts_inflight.set(false);
        state.conflicts_at.set(Some(Instant::now()));
        // An older daemon, or none, has no list: leave the logged conflicts as
        // they are rather than calling them resolved.
        let items = match result {
            Ok(Ok(Response::Conflicts { items })) => Some(items),
            _ => None,
        };
        *state.conflicts.borrow_mut() = items;
        if ui.stack.visible_child_name().as_deref() == Some("activity") {
            paint_activity(&ui);
        }
    });
}

/// A cheap fingerprint of what the page shows: the feed's length and newest
/// entry (the log is append-only and newest-first, so that pins the feed), the
/// filter, and which conflicts are still open.
pub(crate) fn activity_key(
    items: &[ActivityEntry],
    filter: ActivityFilter,
    conflicts: Option<&[ConflictInfo]>,
) -> String {
    let newest = match items.first() {
        Some(a) => format!("{}:{}:{}:{}", items.len(), a.time, a.target, a.detail),
        None => String::new(),
    };
    let open = conflicts.map(|c| {
        c.iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    });
    format!("{newest}|{filter:?}|{open:?}")
}

/// Keep a fresh log from the daemon and repaint with it.
pub(crate) fn repaint_activity(ui: &Rc<Ui>, items: &[ActivityEntry]) {
    *ui.activity.items.borrow_mut() = items.to_vec();
    paint_activity(ui);
}

/// One feed row: an entry and how many times it was logged in a row (or, for
/// a conflict, at all).
#[derive(Debug)]
pub(crate) struct FeedRow<'a> {
    pub(crate) entry: &'a ActivityEntry,
    pub(crate) count: usize,
    /// When the oldest collapsed repeat happened.
    pub(crate) first: i64,
}

/// Collapse repeats so the feed reads as news. The daemon flags a divergent
/// conflict once per run, so the same copy comes back every day it stays
/// unresolved: every conflict entry folds into its newest mention. Any other
/// entry folds only into an identical one right before it (a retry loop
/// failing the same way).
pub(crate) fn collapse_feed(items: &[ActivityEntry]) -> Vec<FeedRow<'_>> {
    let same = |a: &ActivityEntry, b: &ActivityEntry| {
        a.kind == b.kind && a.ok == b.ok && a.target == b.target && a.detail == b.detail
    };
    let mut rows: Vec<FeedRow> = Vec::new();
    for entry in items {
        let prior = if entry.kind == ActivityKind::Conflict {
            rows.iter_mut().find(|row| same(row.entry, entry))
        } else {
            rows.last_mut().filter(|row| same(row.entry, entry))
        };
        match prior {
            Some(row) => {
                row.count += 1;
                row.first = row.first.min(entry.time);
            }
            None => rows.push(FeedRow {
                entry,
                count: 1,
                first: entry.time,
            }),
        }
    }
    rows
}

/// A section title for the day `at` falls on, seen from `now`: "Today",
/// "Yesterday", a weekday within the week, then a date.
pub(crate) fn day_label(at: &glib::DateTime, now: &glib::DateTime) -> String {
    let today = now.ymd();
    if at.ymd() == today {
        return "Today".to_string();
    }
    if now.add_days(-1).is_ok_and(|y| y.ymd() == at.ymd()) {
        return "Yesterday".to_string();
    }
    let format = if now.difference(at).as_seconds() < 6 * 86_400 {
        "%A"
    } else if at.year() == now.year() {
        "%A, %B %-d"
    } else {
        "%B %-d, %Y"
    };
    at.format(format).map(|s| s.to_string()).unwrap_or_default()
}

/// Rebuild the page from the kept feed and conflict list, unless it already
/// shows exactly this.
pub(crate) fn paint_activity(ui: &Rc<Ui>) {
    let state = &ui.activity;
    let items = state.items.borrow();
    let conflicts = state.conflicts.borrow();
    let filter = state.filter.get();
    let key = activity_key(&items, filter, conflicts.as_deref());
    if state.key.borrow().as_deref() == Some(key.as_str()) {
        return;
    }
    let open: &[ConflictInfo] = conflicts.as_deref().unwrap_or_default();
    if items.is_empty() && open.is_empty() {
        activity_status(
            ui,
            "document-open-recent-symbolic",
            "Nothing yet",
            "Uploads, moves, shares and other changes appear here as they happen.",
            false,
        );
        return;
    }
    *state.key.borrow_mut() = Some(key);
    state.content.set_visible_child_name("list");

    // Needs attention: the live conflicts, each opening the resolve dialog.
    for row in state.attention_rows.borrow_mut().drain(..) {
        state.attention.remove(&row);
    }
    state.attention.set_visible(!open.is_empty());
    state.attention.set_description(Some(&match open.len() {
        1 => "A file was changed in two places at once. Choose which version to keep.".to_string(),
        n => {
            format!("{n} files were changed in two places at once. Choose which versions to keep.")
        }
    }));
    let mut attention_rows = state.attention_rows.borrow_mut();
    for conflict in open {
        let row = conflict_row(ui, conflict);
        state.attention.add(&row);
        attention_rows.push(row);
    }
    drop(attention_rows);

    while let Some(child) = state.days.first_child() {
        state.days.remove(&child);
    }
    let rows: Vec<FeedRow> = collapse_feed(&items)
        .into_iter()
        .filter(|row| filter.matches(row.entry))
        .collect();
    if rows.is_empty() {
        let empty = adw::StatusPage::builder()
            .icon_name("edit-find-symbolic")
            .title("No matching activity")
            .description("Nothing in the recent log fits this filter.")
            .build();
        empty.add_css_class("compact");
        state.days.append(&empty);
        return;
    }
    let now = glib::DateTime::now_local().ok();
    let mut group: Option<(String, adw::PreferencesGroup)> = None;
    for row in rows {
        let at = glib::DateTime::from_unix_local(row.entry.time).ok();
        let label = match (&at, &now) {
            (Some(at), Some(now)) => day_label(at, now),
            _ => String::new(),
        };
        if group.as_ref().is_none_or(|(day, _)| *day != label) {
            let section = adw::PreferencesGroup::builder().title(&label).build();
            state.days.append(&section);
            group = Some((label, section));
        }
        if let Some((_, section)) = &group {
            section.add(&feed_row(ui, &row, at.as_ref(), open));
        }
    }
}

/// One row of the feed.
fn feed_row(
    ui: &Rc<Ui>,
    row: &FeedRow,
    at: Option<&glib::DateTime>,
    open: &[ConflictInfo],
) -> adw::ActionRow {
    let a = row.entry;
    let title = match a.kind {
        // The target of an empty is a count, not a name to lead with a verb.
        ActivityKind::EmptyTrash => format!("Emptied Trash ({})", a.target),
        kind => format!("{} {}", activity_verb(kind), a.target),
    };
    // A logged conflict names the copy; it is still open while the daemon
    // still lists a copy by that name.
    let pending = (a.kind == ActivityKind::Conflict)
        .then(|| open.iter().find(|c| file_name(&c.path) == a.target))
        .flatten();
    let resolved = a.kind == ActivityKind::Conflict
        && pending.is_none()
        && ui.activity.conflicts.borrow().is_some();

    let mut subtitle: Vec<String> = Vec::new();
    if !a.detail.is_empty() {
        subtitle.push(capitalize(&a.detail));
    }
    if row.count > 1 {
        subtitle.push(format!(
            "{} times since {}",
            row.count,
            activity_time(row.first)
        ));
    }
    if resolved {
        subtitle.push("Resolved".to_string());
    }
    let widget = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle.join(" · "))
        .use_markup(false)
        .title_lines(1)
        .subtitle_lines(2)
        .build();

    // A failure and a conflict are different news: a conflict kept both
    // copies, a failure lost the action. Each gets its own icon and colour.
    let icon = gtk4::Image::from_icon_name(if resolved {
        "object-select-symbolic"
    } else if a.ok || a.kind == ActivityKind::Conflict {
        activity_icon(a.kind)
    } else {
        "dialog-error-symbolic"
    });
    if resolved {
        icon.add_css_class("success");
    } else if a.kind == ActivityKind::Conflict {
        icon.add_css_class("warning");
    } else if !a.ok {
        icon.add_css_class("error");
    }
    widget.add_prefix(&icon);

    let time = gtk4::Label::new(Some(
        &at.and_then(|at| at.format("%H:%M").ok())
            .map(|s| s.to_string())
            .unwrap_or_default(),
    ));
    time.add_css_class("dim-label");
    time.add_css_class("numeric");
    if let Some(at) = at.and_then(|at| at.format("%c").ok()) {
        time.set_tooltip_text(Some(&at));
    }
    widget.add_suffix(&time);

    if let Some(conflict) = pending {
        let resolve = gtk4::Button::builder()
            .label("Resolve…")
            .valign(gtk4::Align::Center)
            .build();
        let ui = ui.clone();
        let conflict = conflict.clone();
        resolve.connect_clicked(move |_| prompt_resolve_conflict(&ui, &conflict));
        widget.add_suffix(&resolve);
    }
    widget
}

/// A human verb for an activity kind, used as the row title's lead word.
pub(crate) fn activity_verb(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Upload => "Uploaded",
        ActivityKind::Download => "Downloaded",
        ActivityKind::Sync => "Synced",
        ActivityKind::Rename => "Renamed",
        ActivityKind::Move => "Moved",
        ActivityKind::CreateFolder => "Created folder",
        ActivityKind::Trash => "Trashed",
        ActivityKind::Restore => "Restored",
        ActivityKind::DeleteForever => "Deleted",
        ActivityKind::EmptyTrash => "Emptied Trash",
        ActivityKind::Share => "Shared",
        ActivityKind::PublicLink => "Created a link to",
        ActivityKind::Unshare => "Unshared",
        ActivityKind::Conflict => "Conflict",
    }
}

/// A themed icon for an activity kind.
pub(crate) fn activity_icon(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Upload => "pdfs-upload-symbolic",
        ActivityKind::Download => "document-save-symbolic",
        ActivityKind::Sync => "emblem-synchronizing-symbolic",
        ActivityKind::Rename => "document-edit-symbolic",
        ActivityKind::Move => "go-jump-symbolic",
        ActivityKind::CreateFolder => "folder-new-symbolic",
        ActivityKind::Trash => "user-trash-symbolic",
        ActivityKind::DeleteForever | ActivityKind::EmptyTrash => "edit-delete-symbolic",
        ActivityKind::Restore => "edit-undo-symbolic",
        ActivityKind::Share | ActivityKind::PublicLink => "emblem-shared-symbolic",
        ActivityKind::Unshare => "action-unavailable-symbolic",
        ActivityKind::Conflict => "dialog-warning-symbolic",
    }
}

/// Format an epoch-seconds timestamp for the Activity feed, in local time.
pub(crate) fn activity_time(secs: i64) -> String {
    glib::DateTime::from_unix_local(secs)
        .and_then(|dt| dt.format("%b %-d, %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(time: i64, kind: ActivityKind, target: &str, detail: &str, ok: bool) -> ActivityEntry {
        ActivityEntry {
            time,
            kind,
            target: target.into(),
            detail: detail.into(),
            ok,
        }
    }

    #[test]
    fn a_conflict_logged_every_day_shows_once_with_a_count() {
        let items = vec![
            entry(
                300,
                ActivityKind::Conflict,
                "a (sync-conflict).txt",
                "differs",
                false,
            ),
            entry(250, ActivityKind::Upload, "b.txt", "", true),
            entry(
                200,
                ActivityKind::Conflict,
                "a (sync-conflict).txt",
                "differs",
                false,
            ),
            entry(
                100,
                ActivityKind::Conflict,
                "a (sync-conflict).txt",
                "differs",
                false,
            ),
        ];
        let rows = collapse_feed(&items);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry.time, 300);
        assert_eq!(rows[0].count, 3);
        assert_eq!(rows[0].first, 100);
        assert_eq!(rows[1].count, 1);
    }

    #[test]
    fn other_entries_collapse_only_when_back_to_back() {
        let items = vec![
            entry(300, ActivityKind::Upload, "b.txt", "timeout", false),
            entry(200, ActivityKind::Upload, "b.txt", "timeout", false),
            entry(150, ActivityKind::Trash, "c.txt", "", true),
            entry(100, ActivityKind::Upload, "b.txt", "timeout", false),
        ];
        let counts: Vec<usize> = collapse_feed(&items).iter().map(|r| r.count).collect();
        assert_eq!(counts, vec![2, 1, 1]);
    }

    #[test]
    fn the_problems_filter_holds_failures_and_conflicts() {
        let failed = entry(1, ActivityKind::Upload, "a", "", false);
        let conflict = entry(1, ActivityKind::Conflict, "a", "", false);
        let fine = entry(1, ActivityKind::Upload, "a", "", true);
        assert!(ActivityFilter::Problems.matches(&failed));
        assert!(ActivityFilter::Problems.matches(&conflict));
        assert!(!ActivityFilter::Problems.matches(&fine));
        assert!(ActivityFilter::Transfers.matches(&failed));
        assert!(!ActivityFilter::Sharing.matches(&fine));
    }

    #[test]
    fn days_read_today_yesterday_weekday_then_date() {
        let tz = glib::TimeZone::utc();
        let at = |y, m, d, h| glib::DateTime::new(&tz, y, m, d, h, 0, 0.0).unwrap();
        let now = at(2026, 9, 23, 12);
        assert_eq!(day_label(&at(2026, 9, 23, 1), &now), "Today");
        assert_eq!(day_label(&at(2026, 9, 22, 23), &now), "Yesterday");
        // Weekday and month names follow the locale, so compare against it.
        let named = |dt: glib::DateTime, format: &str| dt.format(format).unwrap().to_string();
        assert_eq!(
            day_label(&at(2026, 9, 20, 10), &now),
            named(at(2026, 9, 20, 10), "%A")
        );
        assert_eq!(
            day_label(&at(2026, 9, 1, 10), &now),
            named(at(2026, 9, 1, 10), "%A, %B %-d")
        );
        assert_eq!(
            day_label(&at(2025, 12, 31, 10), &now),
            named(at(2025, 12, 31, 10), "%B %-d, %Y")
        );
    }
}
