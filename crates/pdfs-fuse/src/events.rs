//! Push notifications for front-ends: the daemon's side of
//! [`Request::Subscribe`](pdfs_core::control::Request::Subscribe).
//!
//! Front-ends used to poll. The app asked for the status every two seconds, the
//! tray every three, and pages re-read themselves on a timer, whether or not
//! anything had changed. Now each subscription owns a [`Mailbox`] in the
//! [`EventHub`]. Code that changes something publishes a [`Topic`], and
//! [`run_event_sampler`] watches the few things that change without passing
//! through one place (the status, transfers, sync progress) and publishes them
//! when they differ.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex, MutexGuard};
use pdfs_core::control::{Event, JobItem, Response as CtlResponse, Topic, TransferItem};
use tracing::warn;

use super::Core;

/// Most subscriptions served at once. Each holds a control handler for as long
/// as it lasts; the app and the tray need one apiece, and the cap keeps a
/// misbehaving client from spending the handlers that requests need.
const MAX_SUBSCRIBERS: usize = 8;

/// How long a subscription waits after a change before it sends, so a burst
/// (a folder of files trashed at once) goes out as one event.
const COALESCE: Duration = Duration::from_millis(150);

/// How often [`run_event_sampler`] looks at transfers. Status and sync
/// progress are looked at every other time.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Every open subscription's mailbox. Shared by every clone of [`Core`],
/// on-demand forks included, so a change in any mount reaches every client.
#[derive(Default)]
pub(crate) struct EventHub {
    mailboxes: Mutex<Vec<Arc<Mailbox>>>,
}

/// What one subscription has yet to send.
#[derive(Default)]
pub(crate) struct Mailbox {
    inbox: Mutex<Inbox>,
    ready: Condvar,
}

#[derive(Default)]
struct Inbox {
    topics: BTreeSet<Topic>,
    /// The newest [`Event::Status`] line. A newer one replaces it unsent: a
    /// client wants the current status, not every status in between.
    status: Option<Arc<str>>,
    /// The newest [`Event::Transfers`] line, replaced the same way.
    transfers: Option<Arc<str>>,
}

impl Inbox {
    fn is_empty(&self) -> bool {
        self.topics.is_empty() && self.status.is_none() && self.transfers.is_none()
    }
}

/// `event` as one line of the wire format, newline included.
pub(crate) fn event_line(event: &Event) -> Option<Arc<str>> {
    match serde_json::to_string(event) {
        Ok(mut line) => {
            line.push('\n');
            Some(line.into())
        }
        Err(e) => {
            warn!(error = %e, "events: serialize event failed");
            None
        }
    }
}

impl EventHub {
    /// Tell every subscriber that `topics` changed.
    pub(crate) fn publish(&self, topics: &[Topic]) {
        self.deliver(|inbox| inbox.topics.extend(topics.iter().copied()));
    }

    /// Hand every subscriber a fresh status.
    fn publish_status(&self, line: Arc<str>) {
        self.deliver(|inbox| inbox.status = Some(line.clone()));
    }

    /// Hand every subscriber a fresh transfers snapshot.
    fn publish_transfers(&self, line: Arc<str>) {
        self.deliver(|inbox| inbox.transfers = Some(line.clone()));
    }

    fn deliver(&self, put: impl Fn(&mut Inbox)) {
        for mailbox in self.mailboxes.lock().iter() {
            put(&mut mailbox.inbox.lock());
            mailbox.ready.notify_one();
        }
    }

    /// Open a mailbox, or `None` at [`MAX_SUBSCRIBERS`].
    pub(crate) fn subscribe(&self) -> Option<Arc<Mailbox>> {
        let mut mailboxes = self.mailboxes.lock();
        if mailboxes.len() >= MAX_SUBSCRIBERS {
            return None;
        }
        let mailbox = Arc::new(Mailbox::default());
        mailboxes.push(mailbox.clone());
        Some(mailbox)
    }

    pub(crate) fn unsubscribe(&self, mailbox: &Arc<Mailbox>) {
        self.mailboxes
            .lock()
            .retain(|other| !Arc::ptr_eq(other, mailbox));
    }

    fn has_subscribers(&self) -> bool {
        !self.mailboxes.lock().is_empty()
    }
}

impl Mailbox {
    /// Wait up to `timeout` for something to send, then take it as wire lines:
    /// the status first, then transfers, then the changed topics. Empty when
    /// nothing arrived in time.
    pub(crate) fn take(&self, timeout: Duration) -> Vec<Arc<str>> {
        let mut inbox = self.inbox.lock();
        if inbox.is_empty() {
            self.ready.wait_for(&mut inbox, timeout);
            if inbox.is_empty() {
                return Vec::new();
            }
            // Let the rest of a burst arrive before sending the first of it.
            MutexGuard::unlocked(&mut inbox, || std::thread::sleep(COALESCE));
        }
        let mut lines = Vec::new();
        lines.extend(inbox.status.take());
        lines.extend(inbox.transfers.take());
        if !inbox.topics.is_empty() {
            let topics = std::mem::take(&mut inbox.topics).into_iter().collect();
            lines.extend(event_line(&Event::Changed { topics }));
        }
        lines
    }
}

/// The [`Event::Transfers`] line for a snapshot.
pub(crate) fn transfers_line(items: Vec<TransferItem>, jobs: Vec<JobItem>) -> Option<Arc<str>> {
    event_line(&Event::Transfers { items, jobs })
}

/// Watch what changes without a single place to publish it from, and publish it
/// when it differs from the last look: the status (queue counts, cache use,
/// online, pause), the transfers snapshot, the synced folders with their
/// live progress, and which nodes have a sync issue. Looks only while someone
/// is subscribed.
pub(crate) fn run_event_sampler(core: Core, username: String, mountpoint: std::path::PathBuf) {
    let mut transfers: Option<Arc<str>> = None;
    let mut status: Option<String> = None;
    let mut queue: Option<String> = None;
    let mut locations: Option<String> = None;
    let mut issues: Option<HashMap<i64, String>> = None;
    let mut tick = 0u32;
    while core.shutdown.sleep(SAMPLE_INTERVAL) {
        if !core.events.has_subscribers() {
            // A new subscription starts from a snapshot of its own, so there is
            // nothing to compare against until then.
            transfers = None;
            status = None;
            queue = None;
            locations = None;
            issues = None;
            continue;
        }
        let line = transfers_line(core.transfers.snapshot(), core.jobs_snapshot());
        if line.is_some() && line != transfers {
            transfers = line.clone();
            if let Some(line) = line {
                core.events.publish_transfers(line);
            }
        }

        tick = tick.wrapping_add(1);
        if !tick.is_multiple_of(2) {
            continue;
        }
        let now = super::control::status_response(&core, &username, &mountpoint);
        let (key, queue_key) = status_keys(&now);
        if status.as_ref().is_some_and(|last| *last != key)
            && let Some(line) = event_line(&Event::Status(Box::new(now)))
        {
            core.events.publish_status(line);
        }
        status = Some(key);
        if queue.as_ref().is_some_and(|last| *last != queue_key) {
            core.events.publish(&[Topic::Queue]);
        }
        queue = Some(queue_key);

        // An issue shows on its node in the file browser: raised, cleared,
        // landed or discarded, the listing is out of date.
        if let Ok(now) = core.db.node_issues() {
            if issues.as_ref().is_some_and(|last| *last != now) {
                core.events.publish(&[Topic::Files]);
            }
            issues = Some(now);
        }

        if let Ok(now) = core.list_locations() {
            let key = serde_json::to_string(&now).unwrap_or_default();
            if locations.as_ref().is_some_and(|last| *last != key) {
                core.events.publish(&[Topic::Locations]);
            }
            locations = Some(key);
        }
    }
}

/// What makes two statuses different enough to send, and what makes their
/// queues different. The age of the oldest staged write is left out of both:
/// it grows every second while anything is staged, which is not news.
fn status_keys(status: &CtlResponse) -> (String, String) {
    let CtlResponse::Status {
        parked_uploads,
        failing_ops,
        failing_error,
        staged_bytes,
        paused,
        paused_until,
        username,
        mountpoint,
        pinned,
        used,
        budget,
        online,
        session_expired,
        tokens_unsaved,
        pending_uploads,
        pending_changes,
        ..
    } = status
    else {
        return (String::new(), String::new());
    };
    let queue = format!(
        "{pending_uploads}/{pending_changes}/{parked_uploads}/{failing_ops}/{failing_error:?}"
    );
    let key = format!(
        "{queue}/{staged_bytes}/{paused}/{paused_until:?}/{username}/{mountpoint}/{pinned}/\
         {used}/{budget}/{online}/{session_expired}/{tokens_unsaved}"
    );
    (key, queue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(line: &str) -> Vec<Topic> {
        match serde_json::from_str::<Event>(line.trim()) {
            Ok(Event::Changed { topics }) => topics,
            other => panic!("not a change event: {other:?}"),
        }
    }

    #[test]
    fn a_burst_of_changes_arrives_as_one_event() {
        let hub = EventHub::default();
        let mailbox = hub.subscribe().expect("room for a subscriber");
        hub.publish(&[Topic::Trash, Topic::Files]);
        hub.publish(&[Topic::Files]);
        let lines = mailbox.take(Duration::from_secs(1));
        assert_eq!(lines.len(), 1);
        assert_eq!(changed(&lines[0]), vec![Topic::Files, Topic::Trash]);
        assert!(mailbox.take(Duration::from_millis(10)).is_empty());
    }

    #[test]
    fn a_newer_status_replaces_one_not_yet_sent() {
        let hub = EventHub::default();
        let mailbox = hub.subscribe().expect("room for a subscriber");
        hub.publish_status("first\n".into());
        hub.publish_status("second\n".into());
        assert_eq!(
            mailbox.take(Duration::from_secs(1)),
            vec![Arc::<str>::from("second\n")]
        );
    }

    #[test]
    fn a_closed_subscription_gets_nothing_more() {
        let hub = EventHub::default();
        let mailbox = hub.subscribe().expect("room for a subscriber");
        hub.unsubscribe(&mailbox);
        hub.publish(&[Topic::Files]);
        assert!(mailbox.take(Duration::from_millis(10)).is_empty());
        assert!(!hub.has_subscribers());
    }

    #[test]
    fn subscriptions_are_capped() {
        let hub = EventHub::default();
        let open: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_| hub.subscribe().expect("room for a subscriber"))
            .collect();
        assert!(hub.subscribe().is_none());
        hub.unsubscribe(&open[0]);
        assert!(hub.subscribe().is_some());
    }

    #[test]
    fn the_age_of_a_staged_write_is_not_a_change() {
        let status = |staged_oldest_secs| CtlResponse::Status {
            parked_uploads: 0,
            failing_ops: 0,
            failing_error: None,
            staged_bytes: 10,
            staged_oldest_secs,
            paused: false,
            paused_until: None,
            username: "u".into(),
            mountpoint: "/m".into(),
            pinned: 0,
            used: 0,
            budget: 0,
            pins: Vec::new(),
            online: true,
            session_expired: false,
            tokens_unsaved: false,
            pending_uploads: 1,
            pending_changes: 0,
        };
        assert_eq!(status_keys(&status(5)), status_keys(&status(6)));
    }
}
