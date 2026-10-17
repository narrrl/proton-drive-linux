//! Tells the user when a newer release is out, if they turned on Preferences →
//! Updates. The decisions are [`pdfs_core::update`]'s; this asks GitHub and
//! shows the banner.

use crate::*;

use std::time::{SystemTime, UNIX_EPOCH};

use pdfs_core::update::{
    RELEASES_LATEST_API, is_newer, latest_version, next_check_in, release_url, updates_managed,
};

/// How long one request to GitHub may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait after a failed check, such as one made offline, before
/// trying again. Not saved, so a restart tries again straight away.
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

pub(crate) struct UpdateState {
    /// Over the sidebar, so it shows whichever page is open.
    pub(crate) banner: adw::Banner,
    /// The check loop is running. Turning the setting off and on again while it
    /// sleeps must not start a second one.
    running: Cell<bool>,
    /// The release page the banner's button opens.
    url: RefCell<String>,
}

impl UpdateState {
    pub(crate) fn new() -> Self {
        Self {
            banner: adw::Banner::builder()
                .button_label(gettext("Download"))
                .build(),
            running: Cell::new(false),
            url: RefCell::new(String::new()),
        }
    }
}

/// Whether a package manager brings new releases to this install. The app then
/// never asks, and Preferences does not offer to.
pub(crate) fn updates_managed_here() -> bool {
    std::env::current_exe().is_ok_and(|exe| updates_managed(&exe))
}

/// Wire the banner, show a release an earlier check found, and start checking
/// if the user asked for it.
pub(crate) fn wire_update(ui: &Rc<Ui>) {
    let ui_open = ui.clone();
    ui.update
        .banner
        .connect_button_clicked(move |_| open_uri(&ui_open.update.url.borrow()));
    show_latest(ui);
    start_update_checks(ui);
}

/// Start the daily check, unless it is off, managed elsewhere, or running.
pub(crate) fn start_update_checks(ui: &Rc<Ui>) {
    if updates_managed_here()
        || !ui.dirs.load_config().update_check
        || ui.update.running.replace(true)
    {
        return;
    }
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        // Also kept here, so a config that cannot be saved does not make every
        // pass ask GitHub again.
        let mut checked = None;
        loop {
            let config = ui.dirs.load_config();
            if !config.update_check {
                break;
            }
            let wait = next_check_in(config.update_last_checked.max(checked), unix_now());
            if !wait.is_zero() {
                glib::timeout_future(wait).await;
                continue;
            }
            match fetch_latest().await {
                Ok(latest) => {
                    checked = Some(unix_now());
                    let mut config = ui.dirs.load_config();
                    config.update_last_checked = checked;
                    config.update_latest = latest;
                    if let Err(e) = ui.dirs.save_config(&config) {
                        tracing::warn!("update check: cannot save the result: {e}");
                    }
                    show_latest(&ui);
                }
                Err(e) => {
                    tracing::warn!("update check: {e}");
                    glib::timeout_future(RETRY_AFTER).await;
                }
            }
        }
        ui.update.running.set(false);
    });
}

/// Reveal the banner if the last check found a release newer than this one,
/// and hide it otherwise.
pub(crate) fn show_latest(ui: &Ui) {
    let config = ui.dirs.load_config();
    let newer = config
        .update_latest
        .filter(|latest| config.update_check && is_newer(latest, env!("CARGO_PKG_VERSION")));
    match newer {
        Some(version) => {
            ui.update.banner.set_title(&gettext_f(
                "Proton Drive {version} is available",
                &[("version", &version)],
            ));
            *ui.update.url.borrow_mut() = release_url(&version);
            ui.update.banner.set_revealed(!updates_managed_here());
        }
        None => ui.update.banner.set_revealed(false),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The latest release's version, asked on a worker thread so the window never
/// waits on GitHub. `Ok(None)` when the answer names no `X.Y.Z` release.
async fn fetch_latest() -> Result<Option<String>, String> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(fetch_latest_blocking());
    });
    rx.recv().await.map_err(|e| e.to_string())?
}

fn fetch_latest_blocking() -> Result<Option<String>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("no runtime: {e}"))?;
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "proton-drive-linux/",
            env!("CARGO_PKG_VERSION"),
            " (",
            env!("CARGO_PKG_REPOSITORY"),
            ")"
        ))
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| format!("no HTTP client: {e}"))?;
    runtime.block_on(async {
        let reply = client
            .get(RELEASES_LATEST_API)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !reply.status().is_success() {
            return Err(format!("GitHub answered {}", reply.status()));
        }
        let body = reply.bytes().await.map_err(|e| e.to_string())?;
        Ok(latest_version(&body))
    })
}
