use crate::*;
use pdfs_core::syncignore::is_scratch_name;

use super::activity_text::{Described, describe};

pub(crate) struct ActivityState {
    // Activity page: a "Needs attention" section for live conflicts, then the
    // feed grouped by day, in one list that a repaint updates in place.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    /// The rows, as [`FeedItem`]s. A repaint swaps only the ones that
    /// changed, so the page polling every couple of seconds keeps the user's
    /// scroll position.
    pub(crate) model: gio::ListStore,
    /// The list, or the "No matching activity" status when the filter leaves
    /// nothing.
    pub(crate) feed: gtk4::Stack,
    pub(crate) inflight: Cell<bool>,
    /// Shows the loading state if a full load is slow; the rows stay up
    /// until then.
    pub(crate) loader: Rc<Loader>,
    /// The last feed the daemon sent, so a filter change repaints without a
    /// round-trip.
    pub(crate) items: RefCell<Vec<ActivityEntry>>,
    pub(crate) filter: Cell<ActivityFilter>,
    /// Conflict copies still waiting for a decision, or `None` before the first
    /// listing. Lets a logged conflict read as resolved once it is gone.
    pub(crate) conflicts: RefCell<Option<Vec<ConflictInfo>>>,
    pub(crate) conflicts_inflight: Cell<bool>,
    pub(crate) conflicts_at: Cell<Option<Instant>>,
    /// Whether entries about editors' scratch files (`.goutputstream-*`,
    /// `~$report.docx`, …) show. Off by default: one save can log several.
    pub(crate) show_scratch: Cell<bool>,
    /// How many entries to ask the daemon for; "Show Older" raises it.
    pub(crate) limit: Cell<usize>,
}

/// How many entries the feed asks for at first, and how many more each
/// "Show Older" adds.
pub(crate) const ACTIVITY_PAGE: usize = 200;

/// The most the daemon keeps.
const ACTIVITY_KEPT: usize = 2000;

/// Entries of one kind this close together read as one burst.
const BURST_GAP: i64 = 5 * 60;

/// The fewest entries that make a burst.
const BURST_MIN: usize = 3;

/// Widgets the Activity page's load/repaint touch.
pub(crate) struct ActivityWidgets {
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) model: gio::ListStore,
    pub(crate) list: gtk4::ListView,
    pub(crate) feed: gtk4::Stack,
    pub(crate) filters: Vec<(ActivityFilter, gtk4::ToggleButton)>,
    /// Holds the ⋮ menu, which needs the [`Ui`] and so is built when wiring.
    pub(crate) options: gtk4::Box,
    pub(crate) retry: gtk4::Button,
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

    fn label(self) -> String {
        match self {
            ActivityFilter::All => pgettext("activity filter", "All"),
            ActivityFilter::Transfers => pgettext("activity filter", "Transfers"),
            ActivityFilter::Changes => pgettext("activity filter", "Changes"),
            ActivityFilter::Sharing => pgettext("activity filter", "Sharing"),
            ActivityFilter::Problems => pgettext("activity filter", "Problems"),
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

/// The activity feed, shown as the Sync page's History view: a newest-first
/// feed of the mutations and transfers the daemon performed (uploads,
/// deletes, shares, …), grouped by day, with the conflicts that still need a
/// decision on top.
pub(crate) fn build_activity_page() -> (gtk4::Widget, ActivityWidgets) {
    // Filter chips: a linked row of radio toggles. They sit above the list,
    // not in it, so they stay in reach however far the feed is scrolled.
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("linked");
    bar.set_halign(gtk4::Align::Start);
    let mut filters: Vec<(ActivityFilter, gtk4::ToggleButton)> = Vec::new();
    for filter in ActivityFilter::ALL {
        let button = gtk4::ToggleButton::with_label(&filter.label());
        if let Some((_, first)) = filters.first() {
            button.set_group(Some(first));
        } else {
            button.set_active(true);
        }
        bar.append(&button);
        filters.push((filter, button));
    }
    bar.set_hexpand(true);
    let options = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    top.append(&bar);
    top.append(&options);
    top.set_margin_top(12);
    top.set_margin_bottom(6);
    top.set_margin_start(12);
    top.set_margin_end(12);
    let bar_clamp = adw::Clamp::builder()
        .maximum_size(900)
        .tightening_threshold(600)
        .child(&top)
        .build();

    // Rows are sectioned by the key each item carries: the conflicts first,
    // then one section per day. The items already arrive in that order, and
    // the sort is stable, so this only marks where each section starts.
    let model = gio::ListStore::new::<BoxedAnyObject>();
    let sections = gtk4::SortListModel::new(Some(model.clone()), None::<gtk4::Sorter>);
    sections.set_section_sorter(Some(&gtk4::CustomSorter::new(|a, b| {
        let key = |object: &glib::Object| {
            object
                .downcast_ref::<BoxedAnyObject>()
                .map_or(i64::MIN, |item| item.borrow::<FeedItem>().section())
        };
        // Newest section first.
        key(b).cmp(&key(a)).into()
    })));
    let list = gtk4::ListView::builder()
        .model(&gtk4::NoSelection::new(Some(sections)))
        .single_click_activate(true)
        .build();
    list.add_css_class("activity-feed");
    list.set_header_factory(Some(&feed_header_factory()));
    let clamp = adw::ClampScrollable::builder()
        .maximum_size(900)
        .tightening_threshold(600)
        .child(&list)
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&clamp)
        .build();

    let no_match = adw::StatusPage::builder()
        .icon_name("edit-find-symbolic")
        .title(gettext("No matching activity"))
        .description(gettext("Nothing in the recent log fits this filter."))
        .vexpand(true)
        .build();
    no_match.add_css_class("compact");
    let feed = gtk4::Stack::new();
    feed.add_named(&scroll, Some("rows"));
    feed.add_named(&no_match, Some("empty"));

    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    column.append(&bar_clamp);
    column.append(&feed);

    let retry = gtk4::Button::builder()
        .label(gettext("Retry"))
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
    content.add_named(&column, Some("list"));
    content.add_named(&status, Some("status"));

    (
        content.clone().upcast(),
        ActivityWidgets {
            content,
            status,
            model,
            list,
            feed,
            filters,
            options,
            retry,
        },
    )
}

/// Section headings: "Needs attention" with a line on what to do, or the day.
/// Each is read from the first item of its section.
fn feed_header_factory() -> gtk4::SignalListItemFactory {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, header| {
        let header = header.downcast_ref::<gtk4::ListHeader>().unwrap();
        let title = gtk4::Label::builder().xalign(0.0).build();
        title.add_css_class("heading");
        let description = gtk4::Label::builder().xalign(0.0).wrap(true).build();
        description.add_css_class("dim-label");
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        column.add_css_class("activity-heading");
        column.append(&title);
        column.append(&description);
        header.set_child(Some(&column));
    });
    factory.connect_bind(|_, header| {
        let header = header.downcast_ref::<gtk4::ListHeader>().unwrap();
        let Some(column) = header.child() else {
            return;
        };
        let Some(title) = column.first_child().and_downcast::<gtk4::Label>() else {
            return;
        };
        let Some(description) = title.next_sibling().and_downcast::<gtk4::Label>() else {
            return;
        };
        let Some(item) = header.item().and_downcast::<BoxedAnyObject>() else {
            return;
        };
        let (heading, line) = match &*item.borrow::<FeedItem>() {
            FeedItem::Attention { open, .. } => (
                gettext("Needs attention"),
                Some(ngettext_f(
                    "A file was changed in two places at once. Choose which version to keep.",
                    "{n} files were changed in two places at once. Choose which versions to keep.",
                    *open as u64,
                    &[],
                )),
            ),
            FeedItem::Logged { day_label, .. }
            | FeedItem::Burst { day_label, .. }
            | FeedItem::More { day_label, .. } => (day_label.clone(), None),
        };
        title.set_label(&heading);
        description.set_visible(line.is_some());
        description.set_label(line.as_deref().unwrap_or_default());
    });
    factory
}

/// Install the Activity page's retry button, filters and rows.
pub(crate) fn wire_activity(ui: &Rc<Ui>, widgets: &ActivityWidgets) {
    let ui_retry = ui.clone();
    widgets
        .retry
        .connect_clicked(move |_| restart_service_then(&ui_retry, load_activity));
    for (filter, button) in &widgets.filters {
        let ui = ui.clone();
        let filter = *filter;
        button.connect_toggled(move |button| {
            if button.is_active() {
                ui.activity.filter.set(filter);
                paint_activity(&ui);
            }
        });
    }
    let mut menu = ActionMenu::new();
    menu.toggle(&gettext("Show Temporary Files"), false, {
        let ui = ui.clone();
        move |on| {
            ui.activity.show_scratch.set(on);
            paint_activity(&ui);
        }
    });
    let options = menu.button();
    options.set_tooltip_text(Some(&gettext("View Options")));
    widgets.options.append(&options);

    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_bind({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let Some(object) = item.item().and_downcast::<BoxedAnyObject>() else {
                return;
            };
            let row = match &*object.borrow::<FeedItem>() {
                FeedItem::Attention { conflict, .. } => {
                    item.set_activatable(true);
                    conflict_row(&ui, conflict).upcast::<gtk4::Widget>()
                }
                FeedItem::Logged {
                    entry,
                    count,
                    first,
                    when,
                    pending,
                    resolved,
                    ..
                } => {
                    item.set_activatable(false);
                    feed_row(
                        &ui,
                        entry,
                        *count,
                        *first,
                        when,
                        pending.as_ref(),
                        *resolved,
                    )
                    .upcast()
                }
                FeedItem::Burst {
                    kind,
                    entries,
                    when,
                    ..
                } => {
                    item.set_activatable(false);
                    burst_row(*kind, entries, when).upcast()
                }
                FeedItem::More { .. } => {
                    item.set_activatable(false);
                    item.set_child(Some(&more_row(&ui)));
                    return;
                }
            };
            row.add_css_class("card");
            item.set_child(Some(&row));
        }
    });
    factory.connect_unbind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        item.set_child(None::<&gtk4::Widget>);
    });
    widgets.list.set_factory(Some(&factory));
    // A list row does not emit the action row's own `activated`, so the
    // conflicts open their dialog from here.
    let ui = ui.clone();
    widgets.list.connect_activate(move |list, position| {
        let Some(object) = list
            .model()
            .and_then(|sections| sections.item(position))
            .and_downcast::<BoxedAnyObject>()
        else {
            return;
        };
        if let FeedItem::Attention { conflict, .. } = &*object.borrow::<FeedItem>() {
            prompt_resolve_conflict(&ui, conflict);
        }
    });
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
}

/// Refresh the Activity feed in place, with no status flash and no spinner.
/// Driven by the periodic tick while the page is on screen, so a running sync
/// pass fills the feed as it works rather than only once it is done. Anything
/// other than a good answer leaves the rows alone until the next tick.
pub(crate) fn refresh_activity(ui: &Rc<Ui>) {
    refresh_activity_conflicts(ui, false);
    fetch_activity(ui);
}

/// Ask for the feed and repaint with it if the feed is still on screen.
fn fetch_activity(ui: &Rc<Ui>) {
    if ui.activity.inflight.get() {
        return;
    }
    ui.activity.inflight.set(true);
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListActivity {
            limit: ui.activity.limit.get(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.activity.inflight.set(false);
        // The page may have been navigated away from while the request was in
        // flight.
        if let Ok(Ok(Response::Activity { items })) = result
            && activity_visible(&ui)
        {
            repaint_activity(&ui, &items);
        }
    });
}

/// Whether the activity feed is on screen: the Sync page, on its History
/// view.
pub(crate) fn activity_visible(ui: &Ui) -> bool {
    ui.stack.visible_child_name().as_deref() == Some("locations")
        && ui.locations.views.visible_child_name().as_deref() == Some("history")
}

pub(crate) fn load_activity(ui: &Rc<Ui>) {
    refresh_activity_conflicts(ui, true);
    if ui.activity.inflight.get() {
        return;
    }
    ui.activity.inflight.set(true);
    let ui_p = ui.clone();
    let ticket = ui.activity.loader.refresh(move || {
        activity_status(
            &ui_p,
            "document-open-recent-symbolic",
            &gettext("Loading…"),
            &gettext("Reading recent activity."),
            false,
        );
    });
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListActivity {
            limit: ui.activity.limit.get(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.activity.inflight.set(false);
        drop(ticket);
        match result {
            Ok(Ok(Response::Activity { items })) => repaint_activity(&ui, &items),
            Ok(Ok(Response::Error { message, .. })) => activity_status(
                &ui,
                "dialog-warning-symbolic",
                &gettext("Couldn't read activity"),
                &message,
                false,
            ),
            Ok(Ok(_)) => activity_status(
                &ui,
                "dialog-warning-symbolic",
                &gettext("Couldn't read activity"),
                &gettext("Unexpected reply from the Proton Drive service."),
                false,
            ),
            Ok(Err(_)) | Err(_) => activity_unreachable(&ui),
        }
    });
}

/// The daemon didn't answer the Activity page.
pub(crate) fn activity_unreachable(ui: &Rc<Ui>) {
    service_unreachable(ui, "locations", activity_status, |ui| {
        if activity_visible(ui) {
            load_activity(ui);
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
        if activity_visible(&ui) {
            paint_activity(&ui);
        }
    });
}

/// Keep a fresh log from the daemon and repaint with it.
pub(crate) fn repaint_activity(ui: &Rc<Ui>, items: &[ActivityEntry]) {
    *ui.activity.items.borrow_mut() = items.to_vec();
    paint_activity(ui);
}

/// One row of the Activity list, with everything its row and its section
/// heading show, so that comparing two says whether the row must be redrawn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FeedItem {
    /// A conflict still waiting for a decision. `open` counts all of them,
    /// for the section heading.
    Attention { conflict: ConflictInfo, open: usize },
    /// A logged activity, folded together with its repeats.
    Logged {
        entry: ActivityEntry,
        /// How many times it was logged in a row (or, for a conflict, at all).
        count: usize,
        /// When the oldest collapsed repeat happened.
        first: i64,
        /// The local day it happened on as `yyyymmdd`, which picks its
        /// section, and that day's heading.
        day: i64,
        day_label: String,
        /// When it happened, as the row shows it.
        when: String,
        /// The open conflict a logged conflict names, which the row offers
        /// to resolve.
        pending: Option<ConflictInfo>,
        /// A logged conflict that is no longer open.
        resolved: bool,
    },
    /// The same action on several items within minutes, such as a folder's
    /// files uploading, shown as one row that expands to list them.
    Burst {
        kind: ActivityKind,
        /// Newest first.
        entries: Vec<ActivityEntry>,
        day: i64,
        day_label: String,
        /// When the newest of them happened, as the row shows it.
        when: String,
    },
    /// The "Show Older" row at the end, while the daemon keeps older
    /// entries than the feed asked for. In the last day's section.
    More { day: i64, day_label: String },
}

impl FeedItem {
    /// The section the item belongs to. Sections run from the highest key
    /// down: the conflicts, then the days, newest first.
    fn section(&self) -> i64 {
        match self {
            FeedItem::Attention { .. } => i64::MAX,
            FeedItem::Logged { day, .. }
            | FeedItem::Burst { day, .. }
            | FeedItem::More { day, .. } => *day,
        }
    }
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
        return gettext("Today");
    }
    if now.add_days(-1).is_ok_and(|y| y.ymd() == at.ymd()) {
        return gettext("Yesterday");
    }
    let format = if now.difference(at).as_seconds() < 6 * 86_400 {
        "%A".to_string()
    } else if at.year() == now.year() {
        // Translators: strftime format for a day heading in this year, such as "Wednesday, September 23".
        gettext("%A, %B %-d")
    } else {
        // Translators: strftime format for a day heading in an earlier year, such as "December 31, 2025".
        gettext("%B %-d, %Y")
    };
    at.format(&format)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// What the feed shows besides the filter: whether scratch files count, and
/// whether a "Show Older" row ends it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FeedOptions {
    pub(crate) show_scratch: bool,
    pub(crate) more: bool,
}

/// The rows of the page: the open conflicts, then the feed entries the filter
/// lets through, each with its day, runs of one action folded into bursts.
/// `conflicts` is `None` while the open conflicts are not known yet.
pub(crate) fn feed_items(
    items: &[ActivityEntry],
    filter: ActivityFilter,
    conflicts: Option<&[ConflictInfo]>,
    now: Option<&glib::DateTime>,
    options: FeedOptions,
) -> Vec<FeedItem> {
    let open = conflicts.unwrap_or_default();
    let mut rows: Vec<FeedItem> = open
        .iter()
        .map(|conflict| FeedItem::Attention {
            conflict: conflict.clone(),
            open: open.len(),
        })
        .collect();
    let kept: Vec<ActivityEntry> = items
        .iter()
        .filter(|entry| options.show_scratch || !is_scratch(entry))
        .cloned()
        .collect();
    let mut logged = Vec::new();
    for row in collapse_feed(&kept) {
        if !filter.matches(row.entry) {
            continue;
        }
        let a = row.entry;
        let at = glib::DateTime::from_unix_local(a.time).ok();
        let (day, day_label) = match (&at, now) {
            (Some(at), Some(now)) => {
                let (y, m, d) = at.ymd();
                (
                    i64::from(y) * 10_000 + i64::from(m) * 100 + i64::from(d),
                    day_label(at, now),
                )
            }
            _ => (0, String::new()),
        };
        // A logged conflict names the copy; it is still open while the daemon
        // still lists a copy by that name.
        let pending = (a.kind == ActivityKind::Conflict)
            .then(|| open.iter().find(|c| file_name(&c.path) == a.target))
            .flatten()
            .cloned();
        let resolved = a.kind == ActivityKind::Conflict && pending.is_none() && conflicts.is_some();
        logged.push(FeedItem::Logged {
            entry: a.clone(),
            count: row.count,
            first: row.first,
            day,
            day_label,
            when: now.map(|now| feed_time(a.time, now)).unwrap_or_default(),
            pending,
            resolved,
        });
    }
    let last = logged.last().map(|row| match row {
        FeedItem::Logged { day, day_label, .. } => (*day, day_label.clone()),
        _ => (0, String::new()),
    });
    rows.extend(fold_bursts(logged));
    if options.more
        && let Some((day, day_label)) = last
    {
        rows.push(FeedItem::More { day, day_label });
    }
    rows
}

/// Whether an entry is about an editor's scratch file: the temporary a save
/// writes before renaming it over the real file, a lock file, a partial
/// download. A rename away from one is the save itself.
fn is_scratch(entry: &ActivityEntry) -> bool {
    use ActivityKind::*;
    if !matches!(
        entry.kind,
        Upload | Download | Rename | Move | Trash | Restore | DeleteForever | CreateFolder
    ) {
        return false;
    }
    is_scratch_name(file_name(&entry.target))
        || (entry.kind == Rename
            && entry
                .detail
                .strip_prefix("was ")
                .is_some_and(|old| is_scratch_name(file_name(old))))
}

/// Fold runs of the same successful action on single items, close together
/// on one day, into [`FeedItem::Burst`]s. `rows` are logged rows, newest
/// first.
fn fold_bursts(rows: Vec<FeedItem>) -> Vec<FeedItem> {
    let burstable = |row: &FeedItem| match row {
        FeedItem::Logged { entry, count, .. } => {
            *count == 1
                && entry.ok
                && matches!(
                    entry.kind,
                    ActivityKind::Upload
                        | ActivityKind::Download
                        | ActivityKind::Trash
                        | ActivityKind::Restore
                        | ActivityKind::DeleteForever
                        | ActivityKind::Move
                        | ActivityKind::Rename
                        | ActivityKind::CreateFolder
                )
        }
        _ => false,
    };
    let joins = |run: &[FeedItem], row: &FeedItem| {
        let (
            Some(FeedItem::Logged {
                entry: prev,
                day: prev_day,
                ..
            }),
            FeedItem::Logged { entry, day, .. },
        ) = (run.last(), row)
        else {
            return false;
        };
        prev.kind == entry.kind && prev_day == day && prev.time - entry.time <= BURST_GAP
    };
    let mut out = Vec::new();
    let mut run: Vec<FeedItem> = Vec::new();
    let flush = |run: &mut Vec<FeedItem>, out: &mut Vec<FeedItem>| {
        if run.len() < BURST_MIN {
            out.append(run);
            return;
        }
        let mut entries = Vec::with_capacity(run.len());
        let mut head = None;
        for row in run.drain(..) {
            if let FeedItem::Logged {
                entry,
                day,
                day_label,
                when,
                ..
            } = row
            {
                head.get_or_insert((entry.kind, day, day_label, when));
                entries.push(entry);
            }
        }
        if let Some((kind, day, day_label, when)) = head {
            out.push(FeedItem::Burst {
                kind,
                entries,
                day,
                day_label,
                when,
            });
        }
    };
    for row in rows {
        if burstable(&row) && (run.is_empty() || joins(&run, &row)) {
            run.push(row);
            continue;
        }
        flush(&mut run, &mut out);
        if burstable(&row) {
            run.push(row);
        } else {
            out.push(row);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// When an entry happened, as its row shows it under its day's heading: how
/// long ago within the last hour, else the time of day.
pub(crate) fn feed_time(time: i64, now: &glib::DateTime) -> String {
    if (0..3600).contains(&(now.to_unix() - time)) {
        return dates::relative(time);
    }
    // Translators: strftime format for the time of day in the activity feed, such as "14:05".
    let format = gettext("%H:%M");
    glib::DateTime::from_unix_local(time)
        .ok()
        .and_then(|at| at.format(&format).ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Bring the page up to date with the kept feed and conflict list, changing
/// only the rows that differ.
pub(crate) fn paint_activity(ui: &Rc<Ui>) {
    let state = &ui.activity;
    let items = state.items.borrow();
    let conflicts = state.conflicts.borrow();
    let open = conflicts.as_deref().unwrap_or_default();
    if items.is_empty() && open.is_empty() {
        activity_status(
            ui,
            "document-open-recent-symbolic",
            &gettext("Nothing yet"),
            &gettext("Uploads, moves, shares and other changes appear here as they happen."),
            false,
        );
        state.model.remove_all();
        return;
    }
    let now = glib::DateTime::now_local().ok();
    let limit = state.limit.get();
    let rows = feed_items(
        &items,
        state.filter.get(),
        conflicts.as_deref(),
        now.as_ref(),
        FeedOptions {
            show_scratch: state.show_scratch.get(),
            more: items.len() >= limit && limit < ACTIVITY_KEPT,
        },
    );
    replace_items(&state.model, &rows);
    let shows_entries = rows.iter().any(|row| !matches!(row, FeedItem::More { .. }));
    state
        .feed
        .set_visible_child_name(if shows_entries { "rows" } else { "empty" });
    state.content.set_visible_child_name("list");
}

/// A time label for the end of a row, with the whole moment in its tooltip.
fn time_label(when: &str, time: i64) -> gtk4::Label {
    let label = gtk4::Label::new(Some(when));
    label.add_css_class("dim-label");
    label.add_css_class("numeric");
    label.set_tooltip_text(Some(&dates::full(time)));
    label
}

/// One logged row of the feed, with the fields of [`FeedItem::Logged`].
fn feed_row(
    ui: &Rc<Ui>,
    a: &ActivityEntry,
    count: usize,
    first: i64,
    when: &str,
    pending: Option<&ConflictInfo>,
    resolved: bool,
) -> adw::ActionRow {
    let Described { title, mut details } = describe(a);
    if count > 1 {
        let since = dates::relative(first);
        // Translators: {time} is a date or time such as "Sep 21" or "5 min ago".
        details.push(ngettext_f(
            "{n} time since {time}",
            "{n} times since {time}",
            count as u64,
            &[("time", &since)],
        ));
    }
    if resolved {
        details.push(pgettext("state", "Resolved"));
    }
    let widget = adw::ActionRow::builder()
        .title(title)
        .subtitle(details.join(" · "))
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
    widget.add_suffix(&time_label(when, a.time));

    if let Some(conflict) = pending {
        let resolve = gtk4::Button::builder()
            .label(gettext("Resolve…"))
            .valign(gtk4::Align::Center)
            .build();
        let ui = ui.clone();
        let conflict = conflict.clone();
        resolve.connect_clicked(move |_| prompt_resolve_conflict(&ui, &conflict));
        widget.add_suffix(&resolve);
    }
    widget
}

/// A burst row: one line for the whole run, expanding to one row per item.
fn burst_row(kind: ActivityKind, entries: &[ActivityEntry], when: &str) -> adw::ExpanderRow {
    let n = entries.len() as u64;
    let title = match kind {
        ActivityKind::Upload => ngettext_f("Uploaded {n} item", "Uploaded {n} items", n, &[]),
        ActivityKind::Download => ngettext_f("Downloaded {n} item", "Downloaded {n} items", n, &[]),
        ActivityKind::Trash => ngettext_f(
            "Moved {n} item to the Trash",
            "Moved {n} items to the Trash",
            n,
            &[],
        ),
        ActivityKind::Restore => ngettext_f("Restored {n} item", "Restored {n} items", n, &[]),
        ActivityKind::DeleteForever => ngettext_f(
            "Deleted {n} item forever",
            "Deleted {n} items forever",
            n,
            &[],
        ),
        ActivityKind::Move => ngettext_f("Moved {n} item", "Moved {n} items", n, &[]),
        ActivityKind::Rename => ngettext_f("Renamed {n} item", "Renamed {n} items", n, &[]),
        ActivityKind::CreateFolder => {
            ngettext_f("Created {n} folder", "Created {n} folders", n, &[])
        }
        _ => ngettext_f("{n} change", "{n} changes", n, &[]),
    };
    let names: Vec<&str> = entries.iter().map(|e| file_name(&e.target)).collect();
    let row = adw::ExpanderRow::builder()
        .title(title)
        .subtitle(names.join(", "))
        .use_markup(false)
        .title_lines(1)
        .subtitle_lines(1)
        .build();
    row.add_prefix(&gtk4::Image::from_icon_name(activity_icon(kind)));
    if let Some(newest) = entries.first() {
        row.add_suffix(&time_label(when, newest.time));
    }
    let now = glib::DateTime::now_local().ok();
    for entry in entries {
        let Described { details, .. } = describe(entry);
        let child = adw::ActionRow::builder()
            .title(file_name(&entry.target))
            .subtitle(details.join(" · "))
            .use_markup(false)
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        let when = now
            .as_ref()
            .map(|now| feed_time(entry.time, now))
            .unwrap_or_default();
        child.add_suffix(&time_label(&when, entry.time));
        row.add_row(&child);
    }
    row
}

/// The row at the end of the feed that asks the daemon for older entries.
fn more_row(ui: &Rc<Ui>) -> gtk4::Widget {
    let button = gtk4::Button::builder()
        .label(gettext("Show Older"))
        .halign(gtk4::Align::Center)
        .margin_top(6)
        .margin_bottom(12)
        .build();
    button.add_css_class("pill");
    let ui = ui.clone();
    button.connect_clicked(move |button| {
        button.set_sensitive(false);
        let state = &ui.activity;
        state
            .limit
            .set((state.limit.get() + ACTIVITY_PAGE).min(ACTIVITY_KEPT));
        fetch_activity(&ui);
    });
    button.upcast()
}

/// A themed icon for an activity kind.
pub(crate) fn activity_icon(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Upload => "pdfs-upload-symbolic",
        ActivityKind::Download => "document-save-symbolic",
        ActivityKind::Sync => "pdfs-sync-symbolic",
        ActivityKind::Rename => "document-edit-symbolic",
        ActivityKind::Move => "go-jump-symbolic",
        ActivityKind::CreateFolder => "folder-new-symbolic",
        ActivityKind::Trash => "user-trash-symbolic",
        ActivityKind::DeleteForever | ActivityKind::EmptyTrash => "edit-delete-symbolic",
        ActivityKind::Restore => "edit-undo-symbolic",
        ActivityKind::Share | ActivityKind::PublicLink => "pdfs-share-symbolic",
        ActivityKind::Unshare => "pdfs-unshare-symbolic",
        ActivityKind::Conflict => "dialog-warning-symbolic",
    }
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

    fn conflict(path: &str) -> ConflictInfo {
        ConflictInfo {
            path: path.into(),
            original_path: "docs/a.txt".into(),
            original_exists: true,
            size: 1,
            modified: 0,
            original_size: None,
            original_modified: None,
            identical: false,
        }
    }

    #[test]
    fn open_conflicts_lead_and_logged_ones_know_their_state() {
        let items = vec![
            entry(
                300,
                ActivityKind::Conflict,
                "a (sync-conflict).txt",
                "",
                false,
            ),
            entry(
                200,
                ActivityKind::Conflict,
                "b (sync-conflict).txt",
                "",
                false,
            ),
            entry(100, ActivityKind::Upload, "c.txt", "", true),
        ];
        let open = [conflict("docs/a (sync-conflict).txt")];
        let rows = feed_items(
            &items,
            ActivityFilter::All,
            Some(&open),
            None,
            FeedOptions::default(),
        );
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[0], FeedItem::Attention { open: 1, .. }));
        assert!(rows[0].section() > rows[1].section());
        let states: Vec<(bool, bool)> = rows[1..]
            .iter()
            .map(|row| match row {
                FeedItem::Logged {
                    pending, resolved, ..
                } => (pending.is_some(), *resolved),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(states, vec![(true, false), (false, true), (false, false)]);
    }

    #[test]
    fn unknown_conflicts_resolve_nothing_and_filters_spare_the_attention_rows() {
        let items = vec![
            entry(
                300,
                ActivityKind::Conflict,
                "a (sync-conflict).txt",
                "",
                false,
            ),
            entry(100, ActivityKind::Upload, "c.txt", "", true),
        ];
        let rows = feed_items(
            &items,
            ActivityFilter::All,
            None,
            None,
            FeedOptions::default(),
        );
        assert!(matches!(
            &rows[0],
            FeedItem::Logged {
                resolved: false,
                ..
            }
        ));
        let open = [conflict("docs/a (sync-conflict).txt")];
        let rows = feed_items(
            &items,
            ActivityFilter::Sharing,
            Some(&open),
            None,
            FeedOptions::default(),
        );
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], FeedItem::Attention { .. }));
    }

    #[test]
    fn a_run_of_uploads_folds_into_one_burst() {
        let items: Vec<ActivityEntry> = (0..4)
            .map(|i| {
                entry(
                    1000 - i * 60,
                    ActivityKind::Upload,
                    &format!("{i}.jpg"),
                    "",
                    true,
                )
            })
            .chain([entry(100, ActivityKind::Upload, "late.jpg", "", true)])
            .collect();
        let rows = feed_items(
            &items,
            ActivityFilter::All,
            Some(&[]),
            None,
            FeedOptions::default(),
        );
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], FeedItem::Burst { entries, .. } if entries.len() == 4));
        assert!(matches!(&rows[1], FeedItem::Logged { .. }));
    }

    #[test]
    fn two_in_a_row_stay_separate_and_failures_never_fold() {
        let items = vec![
            entry(300, ActivityKind::Upload, "a", "", true),
            entry(290, ActivityKind::Upload, "b", "", true),
            entry(280, ActivityKind::Upload, "c", "timeout", false),
            entry(270, ActivityKind::Upload, "d", "", true),
        ];
        let rows = feed_items(
            &items,
            ActivityFilter::All,
            Some(&[]),
            None,
            FeedOptions::default(),
        );
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn scratch_files_hide_unless_asked_for() {
        let items = vec![
            entry(300, ActivityKind::Upload, ".goutputstream-AB12", "", true),
            entry(
                290,
                ActivityKind::Rename,
                "notes.txt",
                "was .goutputstream-AB12",
                true,
            ),
            entry(280, ActivityKind::Upload, "notes.txt", "", true),
        ];
        let rows = feed_items(
            &items,
            ActivityFilter::All,
            Some(&[]),
            None,
            FeedOptions::default(),
        );
        assert_eq!(rows.len(), 1);
        let shown = FeedOptions {
            show_scratch: true,
            ..FeedOptions::default()
        };
        let rows = feed_items(&items, ActivityFilter::All, Some(&[]), None, shown);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn show_older_ends_the_last_section() {
        let items = vec![entry(300, ActivityKind::Trash, "a", "", true)];
        let more = FeedOptions {
            more: true,
            ..FeedOptions::default()
        };
        let rows = feed_items(&items, ActivityFilter::All, Some(&[]), None, more);
        assert!(matches!(rows.last(), Some(FeedItem::More { .. })));
        assert_eq!(rows[0].section(), rows[1].section());
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
