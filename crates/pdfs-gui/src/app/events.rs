//! The window's side of the daemon's event feed ([`Request::Subscribe`]).
//!
//! The window used to ask the daemon for its status every two seconds and let
//! pages re-read themselves on timers. Now one thread follows the feed and hands
//! each event to the main loop: a status or transfers snapshot is painted as it
//! comes, and a change reloads the page on screen if the page shows it. A
//! window nobody can see keeps its changes for later and catches up when it is
//! focused again.

use std::collections::BTreeSet;

use pdfs_core::control::{Event, Feed, follow};

use crate::*;

/// How long the conflict list waits after a change before it is read again.
/// Listing walks every node the daemon knows, and a sync pass can report
/// changes many times a second.
const CONFLICTS_DEBOUNCE: Duration = Duration::from_secs(3);

/// How long the import's state waits after its progress moved before it is
/// read again.
const TAKEOUT_DEBOUNCE: Duration = Duration::from_secs(2);

/// How old the invitations may be when the window is focused. Another person
/// inviting this account sends the daemon no event, so focus is when they are
/// looked for.
const INVITATIONS_ON_FOCUS: Duration = Duration::from_secs(60);

/// What the feed keeps between events.
#[derive(Default)]
struct FeedState {
    /// What changed while the window was out of sight.
    deferred: RefCell<BTreeSet<Topic>>,
    /// Whether a conflict read is already waiting out [`CONFLICTS_DEBOUNCE`].
    conflicts_pending: Rc<Cell<bool>>,
    /// Whether an import read is already waiting out [`TAKEOUT_DEBOUNCE`].
    takeout_pending: Rc<Cell<bool>>,
}

/// Follow the daemon for as long as `window` is open.
pub(crate) fn follow_daemon(ui: &Rc<Ui>, window: &adw::ApplicationWindow) {
    let (tx, rx) = async_channel::bounded(64);
    let socket = ui.dirs.control_socket();
    let spawned = std::thread::Builder::new()
        .name("pdfs-feed".into())
        .spawn(move || follow(&socket, |feed| tx.send_blocking(feed).is_ok()));
    if let Err(e) = spawned {
        tracing::error!("cannot start the event feed: {e}");
        return;
    }

    let state = Rc::new(FeedState::default());
    let ui_feed = ui.clone();
    let state_feed = state.clone();
    let window_feed = window.downgrade();
    let rx_feed = rx.clone();
    glib::spawn_future_local(async move {
        while let Ok(feed) = rx_feed.recv().await {
            let hidden = window_feed.upgrade().is_none_or(|w| window_hidden(&w));
            on_feed(&ui_feed, &state_feed, feed, hidden);
        }
    });

    // Catch up as soon as the user comes back to the window.
    let ui_focus = ui.clone();
    window.connect_is_active_notify(move |window| {
        if !window.is_active() {
            return;
        }
        let topics: Vec<Topic> = state.deferred.take().into_iter().collect();
        if !topics.is_empty() {
            follow_changes(&ui_focus, &state, &topics);
        }
        refresh_quota(&ui_focus);
        refresh_invitations_badge(&ui_focus, INVITATIONS_ON_FOCUS);
    });
    // Closing the channel ends the loop above, and the thread with the next
    // event it tries to hand over.
    window.connect_close_request(move |_| {
        rx.close();
        glib::Propagation::Proceed
    });
}

fn on_feed(ui: &Rc<Ui>, state: &Rc<FeedState>, feed: Feed, hidden: bool) {
    match feed {
        // The daemon follows up with its whole state, so there is nothing to
        // ask for here.
        Feed::Connected | Feed::Event(Event::Heartbeat) => {}
        Feed::Event(Event::Status(status)) => {
            paint_status(ui, *status);
            // Uploads are what move the quota, and they move the status too.
            refresh_quota(ui);
        }
        // Painted even out of sight: the end of a batch is what sends the
        // "Sync complete" notification.
        Feed::Event(Event::Transfers { items, jobs }) => {
            repaint_transfers(ui, &items, &jobs);
            follow_takeout(ui, state, &jobs);
        }
        Feed::Event(Event::Changed { topics }) if hidden => {
            state.deferred.borrow_mut().extend(topics);
        }
        Feed::Event(Event::Changed { topics }) => follow_changes(ui, state, &topics),
        // Ask once, which paints "not connected" or "not responding", and clear
        // progress that can no longer move.
        Feed::Disconnected => {
            refresh_status(ui);
            repaint_transfers(ui, &[], &[]);
        }
    }
}

/// Reload what `topics` says changed: the sidebar badges wherever the user
/// is, and the page on screen if it shows one of them.
fn follow_changes(ui: &Rc<Ui>, state: &FeedState, topics: &[Topic]) {
    let changed = |topic| topics.contains(&topic);
    if changed(Topic::Conflicts) {
        let ui = ui.clone();
        debounce(&state.conflicts_pending, CONFLICTS_DEBOUNCE, move || {
            refresh_conflicts(&ui);
            if activity_visible(&ui) {
                refresh_activity_conflicts(&ui, true);
            }
        });
    }
    if changed(Topic::Shares) {
        refresh_invitations_badge(ui, Duration::ZERO);
    }
    match ui.stack.visible_child_name().as_deref() {
        Some("browser") if changed(Topic::Files) => load_browser(ui),
        // The timeline is left alone: reloading it would throw away the place
        // the user scrolled to. F5 reloads it on request.
        Some("gallery")
            if changed(Topic::Photos)
                && ui.gallery.content.visible_child_name().as_deref() == Some("albums") =>
        {
            load_albums(ui)
        }
        Some("trash") if changed(Topic::Trash) => load_trash(ui),
        Some("shared") if changed(Topic::Shares) => load_shared(ui),
        Some("sharedbyme") if changed(Topic::Shares) || changed(Topic::Files) => {
            load_shared_by_me(ui)
        }
        Some("devices") if changed(Topic::Devices) => load_devices(ui),
        Some("locations") => follow_sync_view(ui, topics),
        _ => {}
    }
}

/// Read the import's state again while its progress moves, and once more after
/// it stops, which is how a finished import is noticed. Nothing is read while
/// no import runs and the page is elsewhere.
fn follow_takeout(ui: &Rc<Ui>, state: &FeedState, jobs: &[JobItem]) {
    let importing = jobs.iter().any(|job| job.title == IMPORT_JOB);
    if importing
        || ui.takeout.running.get()
        || ui.stack.visible_child_name().as_deref() == Some("takeout")
    {
        let ui = ui.clone();
        debounce(&state.takeout_pending, TAKEOUT_DEBOUNCE, move || {
            refresh_takeout(&ui)
        });
    }
}

/// Run `f` once `delay` has passed, unless a run is already waiting. The run
/// sees everything that happened up to it, so nothing is missed by skipping.
fn debounce(pending: &Rc<Cell<bool>>, delay: Duration, f: impl FnOnce() + 'static) {
    if pending.replace(true) {
        return;
    }
    let pending = pending.clone();
    glib::timeout_add_local_once(delay, move || {
        pending.set(false);
        f();
    });
}
