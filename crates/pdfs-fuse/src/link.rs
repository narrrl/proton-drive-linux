//! Noticing that the network went away mid-session, and that it came back.
//!
//! The mount used to learn it was offline only at startup: a daemon that had
//! connected once kept `online` true for the rest of its life, so a Wi-Fi drop
//! turned every create, mkdir, delete and rename into a two-minute wait for an
//! `EIO`, while the queue that exists for exactly this sat unused. Now any
//! remote call a handler waits on reports a failure to reach Proton through
//! [`Core::lost_link`], which flips the mount offline; the handler then takes its
//! queued path, and the probe thread here flips it back when Proton answers.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use pdfs_core::error::session_revoked;
use proton_drive_rs::proton_sdk::api::ResponseCode;
use proton_drive_rs::proton_sdk::error::{ProtonApiError, ProtonError};
use tracing::{debug, info, warn};

use crate::{Core, ONLINE_PROBE_MAX, ONLINE_PROBE_MIN};

/// How long a remote call a FUSE handler is waiting on may take before the link
/// is treated as down.
///
/// The SDK's own budget for one request is four attempts of up to 30 seconds
/// each, so a dead link held the caller for about two minutes. A metadata call
/// that has had no answer in 20 seconds is not going to be one the user wants to
/// keep waiting for; queueing the change is the better answer.
pub(crate) const INTERACTIVE_CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one probe may wait for an answer. A probe is a single small request,
/// and a probe stuck in the SDK's retry budget would delay noticing the link is
/// back by as long as it waits.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// First delay between probes after a drop seen mid-session, and the ceiling it
/// doubles to while the outage is young. A blip should end in seconds, not in
/// the minutes [`ONLINE_PROBE_MAX`] allows.
const PROBE_AFTER_DROP: Duration = Duration::from_secs(2);
const PROBE_YOUNG_MAX: Duration = Duration::from_secs(30);
/// How long an outage counts as young. Past it the probe backs off to
/// [`ONLINE_PROBE_MAX`], because a laptop can sit offline for days and each
/// probe is a real API round trip.
const YOUNG_OUTAGE: Duration = Duration::from_secs(10 * 60);

/// How often the idle probe thread checks for teardown while the link is up.
const IDLE_CHECK: Duration = Duration::from_secs(1);

/// The shared half of the online flag: when the link went down, and a condvar
/// that fires on every change so waiters need not poll.
#[derive(Default)]
pub(crate) struct Link {
    lost_at: Mutex<Option<Instant>>,
    changed: Condvar,
    /// Folders listed from the DB alone while offline, to relist once back.
    pub(crate) stale_listings: Mutex<std::collections::HashSet<u64>>,
    /// Proton refused the session itself ([`session_revoked`]). The mount is
    /// offline then, but no probe brings it back: only a new login does, and
    /// that restarts the daemon. Read by front-ends through `Response::Status`,
    /// so they can ask the user to sign in again rather than say "offline".
    pub(crate) session_expired: AtomicBool,
    /// Proton refused the session, but only because another process rotated
    /// it (`pdfs unlock`): the keyring holds the live tokens. The mount loop
    /// ends the mount for the daemon to mount again from the keyring.
    pub(crate) resume_stored: AtomicBool,
}

impl Link {
    /// The link of a mount that comes up with Proton refusing its session, or
    /// not.
    pub(crate) fn starting(session_expired: bool) -> Self {
        Self {
            session_expired: AtomicBool::new(session_expired),
            ..Default::default()
        }
    }
}

/// Whether a failed call failed because Proton could not be reached, rather
/// than because it answered and refused.
///
/// A transport error is the network. So is a request timeout: the SDK reports
/// Proton's own "try again" codes that way, and [`no_answer`] reports our
/// deadline that way. Anything else Proton actually said, and says again on a
/// retry, so it must reach the caller rather than be queued.
pub(crate) fn is_network_error(e: &ProtonError) -> bool {
    match e {
        ProtonError::Transport(_) => true,
        ProtonError::Api(api) => {
            matches!(
                api.code,
                ResponseCode::RequestTimeout | ResponseCode::Timeout
            )
        }
        _ => false,
    }
}

/// The error a call reports when it runs past `after` without an answer.
pub(crate) fn no_answer(after: Duration) -> ProtonError {
    ProtonError::Api(ProtonApiError {
        code: ResponseCode::RequestTimeout,
        http_status: 408,
        message: format!("no answer within {}s", after.as_secs()),
        details: None,
    })
}

/// The slowest upload rate an upload's deadline allows for, past the
/// metadata budget it starts with. Far below any link that works, so only a
/// transfer that has stopped moving runs into it.
const SLOWEST_UPLOAD: u64 = 32 * 1024;

/// How long an upload of `len` bytes may take before the link is treated as
/// down: the budget of a metadata call, plus the bytes at [`SLOWEST_UPLOAD`].
pub(crate) fn upload_deadline(len: u64) -> Duration {
    INTERACTIVE_CALL_TIMEOUT + Duration::from_secs(len / SLOWEST_UPLOAD)
}

/// Run a remote call, giving up after [`INTERACTIVE_CALL_TIMEOUT`].
pub(crate) async fn bounded<T>(
    call: impl Future<Output = Result<T, ProtonError>>,
) -> Result<T, ProtonError> {
    bounded_by(INTERACTIVE_CALL_TIMEOUT, call).await
}

/// Run a remote call, giving up after `limit`.
pub(crate) async fn bounded_by<T>(
    limit: Duration,
    call: impl Future<Output = Result<T, ProtonError>>,
) -> Result<T, ProtonError> {
    match tokio::time::timeout(limit, call).await {
        Ok(result) => result,
        Err(_) => Err(no_answer(limit)),
    }
}

/// The next delay between probes, given the current one and how long the
/// outage has lasted.
fn next_probe_delay(delay: Duration, outage: Duration) -> Duration {
    let cap = if outage < YOUNG_OUTAGE {
        PROBE_YOUNG_MAX
    } else {
        ONLINE_PROBE_MAX
    };
    (delay * 2).min(cap)
}

impl Core {
    /// Whether the mount currently believes Proton is reachable.
    pub(crate) fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Whether a namespace change through the mount goes to Drive inside the
    /// syscall. Only with [`AppConfig::local_first`] off, and only online;
    /// otherwise it is queued and the drain sends it.
    ///
    /// [`AppConfig::local_first`]: pdfs_core::config::AppConfig::local_first
    pub(crate) fn sends_inline(&self) -> bool {
        !self.local_first && self.is_online()
    }

    /// Run a remote call a caller is waiting on, giving up after
    /// [`INTERACTIVE_CALL_TIMEOUT`]. The timer is built inside the runtime: a
    /// FUSE worker has no reactor of its own.
    pub(crate) fn block_on_bounded<T>(
        &self,
        call: impl Future<Output = Result<T, ProtonError>>,
    ) -> Result<T, ProtonError> {
        self.rt.block_on(bounded(call))
    }

    /// Run a remote call, giving up after `limit`: an upload, whose budget
    /// grows with its size ([`upload_deadline`]).
    pub(crate) fn block_on_within<T>(
        &self,
        limit: Duration,
        call: impl Future<Output = Result<T, ProtonError>>,
    ) -> Result<T, ProtonError> {
        self.rt.block_on(bounded_by(limit, call))
    }

    /// Whether Proton refused the session itself, so nothing reaches it until
    /// the user signs in again.
    pub(crate) fn session_expired(&self) -> bool {
        self.link.session_expired.load(Ordering::Relaxed)
    }

    /// Report a failed remote call. Returns whether it failed for want of a
    /// network, or of a session, in which case the mount is now offline and the
    /// caller should take its queued path instead of failing.
    pub(crate) fn lost_link(&self, e: &ProtonError, what: &str) -> bool {
        if session_revoked(e) {
            self.mark_session_expired(what, e);
            return true;
        }
        if !is_network_error(e) {
            return false;
        }
        self.mark_offline(what, e);
        true
    }

    /// Flip the mount offline for good: Proton refused the session, and only a
    /// new login brings it back. Changes stay queued for the daemon that login
    /// starts. Only the first caller logs it.
    ///
    /// Unless the keyring holds a newer session than the refused one: then
    /// the mount ends, for the daemon to mount again on that one.
    pub(crate) fn mark_session_expired(&self, what: &str, error: &dyn std::fmt::Display) {
        if self.link.resume_stored.load(Ordering::Relaxed) {
            return self.mark_offline(what, error);
        }
        if pdfs_core::auth::stored_session_is_newer() {
            info!(during = what, %error,
                  "Proton refused a session another process has since refreshed; resuming it");
            self.link.resume_stored.store(true, Ordering::Relaxed);
            return self.mark_offline(what, error);
        }
        if !self.link.session_expired.swap(true, Ordering::Relaxed) {
            warn!(during = what, %error,
                  "Proton refused the session; run `pdfs login` to sign in again");
        }
        self.mark_offline(what, error);
    }

    /// Flip the mount offline and wake the probe. Only the first caller of an
    /// outage logs it; the rest are the same outage.
    pub(crate) fn mark_offline(&self, what: &str, error: &dyn std::fmt::Display) {
        let mut lost_at = self.link.lost_at.lock();
        if self.online.swap(false, Ordering::Relaxed) {
            *lost_at = Some(Instant::now());
            warn!(during = what, %error,
                  "lost the connection to Proton; queueing changes until it returns");
            self.link.changed.notify_all();
        }
    }

    /// Flip the mount back online, wake everything waiting for it, and let the
    /// queue drain.
    fn mark_online(&self) {
        {
            let mut lost_at = self.link.lost_at.lock();
            self.online.store(true, Ordering::Relaxed);
            match lost_at.take() {
                Some(since) => {
                    info!(
                        offline_secs = since.elapsed().as_secs(),
                        "connection to Proton is back"
                    )
                }
                None => info!("back online"),
            }
            self.link.changed.notify_all();
        }
        self.relist_offline_listings();
        // Anything written while offline is queued and waiting on exactly this,
        // including ops that failed on the way down and are sitting out a
        // backoff sized for a server that keeps refusing.
        if let Err(e) = self.db.retry_failed_ops_now(crate::now_millis()) {
            warn!(error = %e, "moving backed-off ops up after a reconnect failed");
        }
        self.wake_drain();
    }

    /// Wait up to `budget` for the link to come back. Returns whether it is up.
    pub(crate) fn wait_online(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        let mut lost_at = self.link.lost_at.lock();
        while !self.is_online() {
            if self.shutdown.is_stopping()
                || self
                    .link
                    .changed
                    .wait_until(&mut lost_at, deadline)
                    .timed_out()
            {
                return self.is_online();
            }
        }
        true
    }

    /// Wait out `interval` before a background poll of Proton, and while the
    /// link is down, until it is back. Returns as soon as it comes back, so the
    /// poll catches up on what changed during the outage at once instead of
    /// one interval later. A poll into a dead link would only wait out the
    /// SDK's retries and fail.
    pub(crate) async fn next_poll(&self, interval: Duration) {
        let due = Instant::now() + interval;
        let mut was_offline = false;
        loop {
            if !self.is_online() {
                was_offline = true;
            } else if was_offline || Instant::now() >= due {
                return;
            }
            tokio::time::sleep(IDLE_CHECK).await;
        }
    }

    /// Watch the link for the life of the mount: sleep while it is up, and
    /// probe it back while it is down.
    ///
    /// A mount that started from the cache is down from the first moment; one
    /// that started online goes down when [`Core::mark_offline`] says so.
    pub(crate) fn run_online_probe(&self) {
        // Only an offline start never had a chance to repair its share id.
        let mut repair_share_id = !self.is_online();
        loop {
            {
                let mut lost_at = self.link.lost_at.lock();
                while self.is_online() {
                    if self.shutdown.is_stopping() {
                        return;
                    }
                    self.link.changed.wait_for(&mut lost_at, IDLE_CHECK);
                }
            }
            if !self.probe_until_online() {
                return;
            }
            if std::mem::take(&mut repair_share_id) {
                self.repair_primary_share_id(&self.primary_root_uid);
            }
        }
    }

    /// Probe until Proton answers, then flip the mount online. Returns `false`
    /// if the daemon is stopping.
    ///
    /// A refused session is not probed: every probe would be refused the same
    /// way, forever (issue #35). The thread waits for the stop instead.
    fn probe_until_online(&self) -> bool {
        let started = self.link.lost_at.lock().unwrap_or_else(Instant::now);
        let mut delay = if started.elapsed() < YOUNG_OUTAGE {
            PROBE_AFTER_DROP
        } else {
            ONLINE_PROBE_MIN
        };
        loop {
            if !self.shutdown.sleep(delay) {
                return false;
            }
            if self.session_expired() {
                delay = ONLINE_PROBE_MAX;
                continue;
            }
            let probe = self.rt.block_on(async {
                match tokio::time::timeout(PROBE_TIMEOUT, self.drive.get_my_files_folder()).await {
                    Ok(result) => result,
                    Err(_) => Err(no_answer(PROBE_TIMEOUT)),
                }
            });
            match probe {
                Ok(root) => {
                    {
                        let root_ino = self.root_ino();
                        let mut st = self.state();
                        if let Some(e) = st.entries.get_mut(&root_ino) {
                            e.node = root.clone();
                        }
                    }
                    if let Err(e) = self.db.upsert_node(&root) {
                        warn!(error = %e, "refresh root after reconnect failed");
                    }
                    self.mark_online();
                    return true;
                }
                Err(e) if session_revoked(&e) => self.mark_session_expired("online probe", &e),
                Err(e) => {
                    debug!(error = %e, ?delay, "online probe failed; still offline");
                    delay = next_probe_delay(delay, started.elapsed());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(code: ResponseCode) -> ProtonError {
        ProtonError::Api(ProtonApiError {
            code,
            http_status: 422,
            message: String::new(),
            details: None,
        })
    }

    #[test]
    fn only_an_unanswered_call_counts_as_a_lost_link() {
        assert!(is_network_error(&no_answer(INTERACTIVE_CALL_TIMEOUT)));
        assert!(is_network_error(&api(ResponseCode::Timeout)));
        assert!(!is_network_error(&api(ResponseCode::AlreadyExists)));
        assert!(!is_network_error(&api(ResponseCode::InvalidRequirements)));
        assert!(!is_network_error(&ProtonError::invalid_operation("bug")));
    }

    #[test]
    fn an_upload_gets_longer_the_bigger_it_is() {
        assert_eq!(upload_deadline(0), INTERACTIVE_CALL_TIMEOUT);
        assert_eq!(
            upload_deadline(1 << 30),
            INTERACTIVE_CALL_TIMEOUT + Duration::from_secs(32_768)
        );
    }

    #[test]
    fn a_call_past_its_deadline_reads_as_a_lost_link() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let limit = Duration::from_millis(20);
        let result: Result<(), _> = rt.block_on(bounded_by(limit, std::future::pending()));
        assert!(result.is_err_and(|e| is_network_error(&e)));
    }

    #[test]
    fn a_young_outage_is_probed_often_and_an_old_one_rarely() {
        let young = Duration::from_secs(60);
        let old = YOUNG_OUTAGE + Duration::from_secs(1);
        assert_eq!(
            next_probe_delay(PROBE_AFTER_DROP, young),
            2 * PROBE_AFTER_DROP
        );
        assert_eq!(next_probe_delay(PROBE_YOUNG_MAX, young), PROBE_YOUNG_MAX);
        assert_eq!(next_probe_delay(PROBE_YOUNG_MAX, old), 2 * PROBE_YOUNG_MAX);
        assert_eq!(next_probe_delay(ONLINE_PROBE_MAX, old), ONLINE_PROBE_MAX);
    }
}
