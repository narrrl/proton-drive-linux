//! `pdfs-app` — GTK4 / libadwaita desktop window for the Proton Drive Linux
//! client.
//!
//! Slice 2 of the GUI phase: the settings/management window that complements the
//! [`pdfs-tray`](crate) SNI icon. It presents four things in a Proton-purple,
//! Google-Drive-style layout:
//!
//! * a **login form** (email / password, with a lazy 2FA dialog shown only when
//!   the account requires it) driving [`pdfs_core::auth::login`]; logging in
//!   enables the systemd mount service;
//! * a **read-only mount status** line (the mount is managed automatically);
//! * a **cache usage** read-out; and
//! * the **pin list** with per-file unpin.
//!
//! Mount status, cache usage and the pin list all ride along on one
//! [`Request::Status`] round-trip to the daemon (which owns the cache), fetched on
//! a worker thread so the periodic refresh never blocks the GTK main loop. Login
//! identity is cached in [`Ui`] and refreshed only on login/logout, so the 2s tick
//! never touches the keyring.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk4::gio;
use gtk4::glib;
use gtk4::glib::BoxedAnyObject;

use pdfs_core::auth;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{
    ActivityEntry, ActivityKind, BookmarkInfo, DeviceInfo, DirEntry, InvitationInfo, JobItem,
    PhotoItem, PhotoKind, PublicLinkInfo, RefreshScope, Request, Response, SearchHit, ShareEntry,
    ShareEntryKind, SharedItem, SyncFolderInfo, SyncPhase, SyncProgress, TransferDirection,
    TransferItem, pending_summary, send,
};
use pdfs_core::service;

/// How many photos to pull per [`Request::PhotosTimeline`] page.
const PHOTOS_PAGE: usize = 60;

const APP_ID: &str = "io.narl.proton-drive-linux";
/// Proton brand purple, applied as the libadwaita accent so switches, buttons,
/// links and the storage bar all pick it up.
const PROTON_PURPLE: &str = "#6d4aff";
/// How often the window re-reads mount status, cache usage and the pin list.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Backoff between auto-retries of a Files/Photos load while the mount service
/// is still coming up (see [`load_browser`] / [`load_gallery`]).
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// How long a network-backed page (Shared, Shared-by-me, Devices, Activity) is
/// considered fresh. Re-navigating to it within this window reuses the rows
/// already on screen instead of re-fetching and flashing the "Loading…"
/// placeholder. The Retry button and every mutation still force an immediate
/// reload by clearing the page's timestamp.
const PAGE_TTL: Duration = Duration::from_secs(30);
/// Idle pause after the last keystroke before a search query is sent, so typing
/// doesn't fire a request per character.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// Cap on search hits requested from the daemon.
const SEARCH_LIMIT: usize = 200;

/// Gallery row height in px: the zoom range, its default, and the step one
/// Ctrl+scroll notch (or Ctrl+±) moves it by. Rows are justified to the full
/// content width, so this is the *target* height a row lands near, not an exact
/// tile size (see [`justify_rows`]).
const TILE_MIN: i32 = 90;
const TILE_MAX: i32 = 340;
const TILE_DEFAULT: i32 = 220;
const TILE_STEP: i32 = 30;
/// Gap between tiles, horizontally and vertically.
const TILE_GAP: i32 = 6;
/// Aspect ratio (w/h) assumed for a photo whose thumbnail hasn't been decoded
/// yet, so a tile can be laid out before its image exists. Landscape 3:2, the
/// commonest camera/phone ratio; once the thumbnail lands the real ratio
/// replaces it and the section re-justifies in place.
const RATIO_UNKNOWN: f64 = 1.5;
/// Aspect ratios are clamped to this range before a row is packed: one absurd
/// panorama, or a sliver of a portrait, must not be able to squash the row it
/// lands in down to nothing. The tile still shows the whole image.
const RATIO_MIN: f64 = 0.4;
const RATIO_MAX: f64 = 3.5;
/// How many thumbnails one on-demand [`Request::PhotoThumbs`] batch asks for.
/// Small, so the first tiles on screen fill in quickly rather than the whole
/// page landing at once.
const THUMB_BATCH: usize = 16;
/// Idle pause before a thumbnail batch is sent, so a fast scroll coalesces into
/// one request per settle instead of one per row that flickers past.
const THUMB_DEBOUNCE: Duration = Duration::from_millis(60);
/// How long to wait before asking again for a thumbnail the daemon is generating
/// itself. That means downloading the photo's full file, so the wait is measured
/// in seconds, not milliseconds.
const THUMB_RETRY: Duration = Duration::from_secs(4);
/// Decoded thumbnails held in memory. Each is a few hundred KiB of GPU texture;
/// this caps the gallery's footprint while covering several screens of scroll.
const TEXTURE_CACHE_MAX: usize = 600;
/// Pause after a resize/zoom before the visible sections are re-justified.
const RELAYOUT_DEBOUNCE: Duration = Duration::from_millis(80);

/// One rendered pin row, retained so [`repaint_pins`] can flip the unpin button's
/// `sensitive` in place (when the pin set is unchanged) instead of rebuilding.
struct PinRow {
    row: adw::ActionRow,
    /// The unpin button, absent on the placeholder row.
    unpin: Option<gtk4::Button>,
}

/// One rendered row in the Activity group: a description over a progress bar.
/// Retained so [`repaint_transfers`] can update the bar and label in place each
/// tick when the active set is unchanged, instead of rebuilding.
struct TransferRow {
    row: adw::PreferencesRow,
    label: gtk4::Label,
    bar: gtk4::ProgressBar,
}

/// What one Activity row should say this tick, and how far along it is —
/// `None` meaning "no total known", which the bar shows by pulsing. Jobs and
/// transfers both render to this, so the group is one list in the order the
/// daemon reports: the jobs that frame the work, then the files moving under it.
struct ActivityLine {
    text: String,
    fraction: Option<f64>,
}

/// All widgets the periodic refresh and the action handlers mutate, plus the
/// resolved paths they act on. Wrapped in an [`Rc`] so handlers and the timeout
/// closure share one instance.
struct Ui {
    dirs: AppDirs,
    stack: adw::ViewStack,
    /// Wraps the whole window content; every non-blocking outcome (a completed
    /// rename, a failed upload, a purge) is reported here rather than in a modal,
    /// so an action never interrupts what the user is doing next.
    toasts: adw::ToastOverlay,
    /// Header spinner shown while any open/load round-trip is in flight; ref-
    /// counted via [`Self::busy`] so concurrent operations don't stop it early.
    spinner: gtk4::Spinner,
    busy: Cell<u32>,
    /// Keys (relative path / photo uid) of open requests currently in flight, so
    /// a double-click on the same entry is a no-op instead of a second download.
    opening: RefCell<HashSet<String>>,
    /// Resolved login identity, cached so the periodic [`refresh`] never hits the
    /// keyring (a DBus round-trip). Populated at startup and updated only on
    /// login / logout. `None` = signed out.
    session: RefCell<Option<auth::StoredSession>>,
    /// Whether a [`Request::Status`] round-trip is already in flight, so the 2s
    /// refresh tick doesn't pile worker threads up on a slow/wedged daemon.
    status_inflight: Cell<bool>,
    /// Guards the [`Request::GetQueueStatus`] poll the same way: at most one
    /// in-flight at a time so a wedged daemon can't stack worker threads.
    transfers_inflight: Cell<bool>,
    /// Activity group + its current rows, hidden when no transfer is in flight.
    transfers_group: adw::PreferencesGroup,
    transfer_rows: RefCell<Vec<TransferRow>>,
    // Login page.
    email: adw::EntryRow,
    password: adw::PasswordEntryRow,
    login_button: gtk4::Button,
    login_status: gtk4::Label,
    // Main page.
    account_row: adw::ActionRow,
    /// Read-only mount status line. The mount is driven by the systemd user
    /// service (enabled on login), not by the user — so this only reports.
    mount_row: adw::ActionRow,
    cache_bar: gtk4::ProgressBar,
    cache_label: gtk4::Label,
    /// "Start on login" toggle. [`Self::settings_suppress`] guards programmatic
    /// sets so reflecting the systemd state doesn't fire the toggle handler.
    autostart_row: adw::SwitchRow,
    /// Cache-budget editor (GiB). Populated once from config; user edits drive a
    /// `SetCacheBudget` round-trip. Guarded by [`Self::settings_suppress`].
    budget_row: adw::SpinRow,
    /// Shows the active mountpoint in its subtitle; updated when the user picks a
    /// new folder.
    mountpoint_row: adw::ActionRow,
    /// Set while a settings widget is being populated programmatically, so its
    /// change handler skips the IPC/systemd side effect.
    settings_suppress: Cell<bool>,
    pins_group: adw::PreferencesGroup,
    /// Rows currently shown under [`Self::pins_group`], retained so a refresh can
    /// diff against them and only rebuild when the pin set actually changes.
    pin_rows: RefCell<Vec<PinRow>>,
    /// The pin paths last rendered, the diff baseline for [`repaint_pins`].
    /// `None` = nothing built yet; `Some(empty)` = the placeholder is shown.
    pins_state: RefCell<Option<Vec<String>>>,
    /// Whether the last refresh saw a live mount daemon. Gates the unpin buttons
    /// (which need the daemon to evict + re-hydrate) and every mutating action.
    mounted: RefCell<bool>,
    /// The mount state the *last* desktop notification reported, so a flap only
    /// notifies on the edge. `None` until the first status reply, so a cold start
    /// doesn't announce "disconnected" before the service has had a chance to come
    /// up.
    notified_mounted: Cell<Option<bool>>,
    /// How many transfers were in flight on the previous poll. A drop to zero is
    /// what "sync complete" means; there's no completion event on the wire.
    active_transfers: Cell<usize>,
    /// Sidebar destination list (Files / Photos / Settings). Selecting a row swaps
    /// the page stack; [`sync_sidebar`] mirrors navigation that starts elsewhere.
    sidebar: gtk4::ListBox,
    /// The sidebar/content split. Collapsed while signed out, so the login page
    /// owns the whole window and no destination is reachable without a session.
    nav: adw::NavigationSplitView,
    // Files (browser) page.
    /// Shared model behind the grid and column views; repopulated per directory.
    browser_model: gio::ListStore,
    browser_back: gtk4::Button,
    /// Clickable breadcrumb trail (a button per path segment); rebuilt per load
    /// by [`repaint_crumb`] so each ancestor folder navigates on click.
    browser_crumb: gtk4::Box,
    /// Swaps the Files content area between the grid/list views and the status
    /// page below; see [`browser_status`].
    browser_content: gtk4::Stack,
    /// The Files empty/loading/error surface, shown in place of the views.
    browser_status: adw::StatusPage,
    /// Sits in [`Self::browser_status`]; shown when a load failed because the mount
    /// service is down (not merely starting); restarts the service and reloads.
    browser_retry: gtk4::Button,
    /// The details pane's host; `show_sidebar` is what reveals/hides the pane.
    browser_split: adw::OverlaySplitView,
    /// Widgets in the details pane, repainted from the selected entry.
    details: DetailsWidgets,
    /// The entry the details pane is currently showing, so its buttons act on the
    /// same one the user is looking at even after the model is repopulated.
    details_entry: RefCell<Option<DirEntry>>,
    /// Set while the details pane is being populated, so setting the offline
    /// switch programmatically doesn't fire a pin/unpin round-trip.
    details_suppress: Cell<bool>,
    /// The grid's and list's selection models. Both wrap the one browser model, so
    /// a selection in either drives the details pane.
    grid_selection: gtk4::SingleSelection,
    list_selection: gtk4::SingleSelection,
    /// Mountpoint-relative path the browser is showing (empty = root).
    browser_path: RefCell<String>,
    /// Debounced full-text search box in the browser header.
    browser_search: gtk4::SearchEntry,
    /// The folder-level actions, insensitive while the mount is down: without a
    /// daemon they can only fail, and a greyed button says so before the click.
    browser_new_folder: gtk4::Button,
    browser_upload: gtk4::Button,
    browser_upload_folder: gtk4::Button,
    /// Pending debounce timer for the search box; replaced on every keystroke so
    /// only the last pause actually fires a [`Request::Search`].
    search_source: RefCell<Option<glib::SourceId>>,
    // Trash page. Trashed nodes are addressed by uid, not by path, so this page
    // keeps no current-directory state — it re-lists from the daemon on show.
    trash_model: gio::ListStore,
    trash_content: gtk4::Stack,
    trash_status: adw::StatusPage,
    trash_retry: gtk4::Button,
    /// Empties the trash; insensitive while it is empty (or unread).
    trash_empty: gtk4::Button,
    /// "12 items" under the page title.
    trash_subtitle: gtk4::Label,
    // Photos (gallery) page.
    /// Every photo loaded so far, newest first — the order the lightbox's
    /// prev/next walks. The visible day sections are derived from this.
    gallery_model: gio::ListStore,
    /// The day sections rendered by the Photos ListView, rebuilt from
    /// [`Self::gallery_model`] by [`repaint_gallery`].
    gallery_groups: gio::ListStore,
    /// Target row height in px, retuned by Ctrl+scroll / Ctrl+± (see
    /// [`zoom_gallery`]). Each tile keeps its own aspect ratio; rows are
    /// justified to the content width around this height.
    gallery_tile: Cell<i32>,
    /// Swaps the Photos content area between the timeline and its status page.
    gallery_content: gtk4::Stack,
    gallery_status: adw::StatusPage,
    gallery_retry: gtk4::Button,
    gallery_more: gtk4::Button,
    gallery_upload: gtk4::Button,
    /// "1,204 photos" under the page title.
    gallery_subtitle: gtk4::Label,
    /// Which tab (Photos / Videos / Raw) the timeline is filtered to, or `None`
    /// for All. Read by [`load_gallery`] and set by the filter toggles.
    gallery_kind: Cell<Option<PhotoKind>>,
    /// The filter toggles, index-aligned with [`kind_for_tab`], kept so their
    /// labels can carry live per-kind counts.
    gallery_tabs: [gtk4::ToggleButton; 4],
    /// The date-jump dropdown ("All dates" then a month per timeline entry), and
    /// the `[from, to)` window each of its rows selects (index-aligned; `None` is
    /// "All dates"). Selecting a row loads that month via [`load_gallery`].
    gallery_dates: gtk4::DropDown,
    gallery_date_ranges: RefCell<Vec<Option<(i64, i64)>>>,
    /// The capture-time window the timeline is currently filtered to, or `None`
    /// for the whole span. Read by [`load_gallery`], set by the date dropdown.
    gallery_range: Cell<Option<(i64, i64)>>,
    /// Set while the date dropdown is being repopulated, so resetting its model
    /// doesn't fire the selection handler and kick off a spurious reload.
    gallery_date_suppress: Cell<bool>,
    /// True while a timeline page is in flight, so the scroll-to-the-end paging
    /// can't fire a second request for the page already coming.
    gallery_loading: Cell<bool>,
    /// Content width the sections are currently justified to. Updated when the
    /// ListView is resized, which re-justifies the visible sections.
    gallery_width: Cell<i32>,
    /// Decoded thumbnails by photo uid, with the insertion order that evicts the
    /// oldest past [`TEXTURE_CACHE_MAX`]. Scrolling back over a day therefore
    /// repaints from memory instead of re-decoding from disk.
    photo_tex: RefCell<HashMap<String, gtk4::gdk::Texture>>,
    photo_tex_order: RefCell<VecDeque<String>>,
    /// Aspect ratio (w/h) per uid, learned when a thumbnail is decoded and
    /// persisted to disk, so a relaunch justifies its rows correctly on the first
    /// frame instead of reflowing as thumbnails arrive.
    photo_ratio: RefCell<HashMap<String, f64>>,
    /// Ratios learned since the last save, so the ratio file is only rewritten
    /// when it actually changed.
    photo_ratio_dirty: Cell<bool>,
    /// Photos the daemon reported as having no thumbnail at all, so a tile that
    /// can never be filled isn't requested again on every scroll past it.
    photo_nothumb: RefCell<HashSet<String>>,
    /// Tiles on screen still waiting for their thumbnail, by uid. Populated as
    /// sections are bound, drained as batches land, cleared on unbind — so a
    /// batch only ever paints a widget that is still showing that photo.
    thumb_wanted: RefCell<HashMap<String, gtk4::Picture>>,
    /// Uids queued for the next [`Request::PhotoThumbs`] batch, and whether a
    /// batch is already in flight (only one at a time, so a long scroll can't
    /// stack requests on the daemon).
    thumb_queue: RefCell<VecDeque<String>>,
    thumb_inflight: Cell<bool>,
    /// Thumbnails on disk waiting to be turned into textures, as `(uid, path)`.
    /// Decoding happens on the GTK thread (textures are not `Send`), so it is fed
    /// a few at a time from an idle callback rather than in one blocking burst
    /// that would stutter the scroll. [`Self::decode_idle`] is the "callback
    /// already scheduled" guard.
    decode_queue: RefCell<VecDeque<(String, String)>>,
    decode_idle: Cell<bool>,
    /// Pending debounce timers for the thumbnail queue flush and the section
    /// re-justify, replaced on each new trigger so only the last one fires.
    thumb_source: RefCell<Option<glib::SourceId>>,
    relayout_source: RefCell<Option<glib::SourceId>>,
    /// The day sections currently realised by the ListView, by their index in
    /// [`Self::gallery_groups`]. A learned ratio or a resize re-justifies these
    /// in place — rebuilding the ListStore instead would reset the scroll
    /// position out from under the user.
    gallery_bound: RefCell<HashMap<u32, gtk4::Box>>,
    // Shared page. Three sections rebuilt wholesale on each load: shared-with-me,
    // incoming invitations, and bookmarks. Rows are tracked so a reload can remove
    // the old ones before appending the new (adw groups have no clear-all).
    shared_content: gtk4::Stack,
    shared_status: adw::StatusPage,
    shared_retry: gtk4::Button,
    shared_with_me_group: adw::PreferencesGroup,
    invitations_group: adw::PreferencesGroup,
    bookmarks_group: adw::PreferencesGroup,
    shared_rows: RefCell<Vec<(adw::PreferencesGroup, gtk4::Widget)>>,
    /// Guards the Shared page's load so overlapping navigations don't stack.
    shared_inflight: Cell<bool>,
    /// When the Shared page last painted good data. `None` = never / invalidated,
    /// forcing a fetch on next visit. See [`PAGE_TTL`].
    shared_loaded_at: Cell<Option<Instant>>,
    // Devices page: two sections (synced folders + other devices), rebuilt
    // wholesale on each load.
    devices_content: gtk4::Stack,
    devices_status: adw::StatusPage,
    devices_retry: gtk4::Button,
    devices_group: adw::PreferencesGroup,
    devices_rows: RefCell<Vec<gtk4::Widget>>,
    devices_sync_group: adw::PreferencesGroup,
    devices_sync_rows: RefCell<Vec<gtk4::Widget>>,
    devices_inflight: Cell<bool>,
    devices_loaded_at: Cell<Option<Instant>>,
    // Shared (by me) page: one section listing the items I've shared, each with a
    // copy-link / manage affordance. Rebuilt wholesale on each load.
    shared_by_me_content: gtk4::Stack,
    shared_by_me_status: adw::StatusPage,
    shared_by_me_retry: gtk4::Button,
    shared_by_me_group: adw::PreferencesGroup,
    shared_by_me_rows: RefCell<Vec<gtk4::Widget>>,
    shared_by_me_inflight: Cell<bool>,
    shared_by_me_loaded_at: Cell<Option<Instant>>,
    // Activity page: one section, newest-first, rebuilt wholesale on each load.
    activity_content: gtk4::Stack,
    activity_status: adw::StatusPage,
    activity_retry: gtk4::Button,
    activity_group: adw::PreferencesGroup,
    activity_rows: RefCell<Vec<gtk4::Widget>>,
    activity_inflight: Cell<bool>,
    /// Fingerprint of the feed currently on screen (see [`activity_key`]). The
    /// page polls every couple of seconds and usually gets back exactly what it
    /// is already showing; rebuilding all 200 rows for that would throw away the
    /// user's scroll position several times a minute.
    activity_key: RefCell<Option<String>>,
}

impl Ui {
    /// Begin a unit of background work: show + spin the header spinner.
    fn busy_begin(&self) {
        self.busy.set(self.busy.get() + 1);
        self.spinner.set_visible(true);
        self.spinner.start();
    }

    /// End a unit of background work: stop the spinner once the last one is done.
    fn busy_end(&self) {
        let remaining = self.busy.get().saturating_sub(1);
        self.busy.set(remaining);
        if remaining == 0 {
            self.spinner.stop();
            self.spinner.set_visible(false);
        }
    }

    /// The aspect ratio (w/h) to lay `uid`'s tile out at: the real one once its
    /// thumbnail has been seen, otherwise [`RATIO_UNKNOWN`].
    fn ratio(&self, uid: &str) -> f64 {
        self.photo_ratio
            .borrow()
            .get(uid)
            .copied()
            .unwrap_or(RATIO_UNKNOWN)
    }

    /// Remember a decoded thumbnail and the aspect ratio it revealed. Returns
    /// true when the ratio is *new information* — the caller re-justifies the
    /// affected rows, since they were laid out against a guess.
    fn store_texture(&self, uid: &str, texture: gtk4::gdk::Texture) -> bool {
        let (w, h) = (texture.width(), texture.height());

        let mut cache = self.photo_tex.borrow_mut();
        let mut order = self.photo_tex_order.borrow_mut();
        if cache.insert(uid.to_string(), texture).is_none() {
            order.push_back(uid.to_string());
        }
        while order.len() > TEXTURE_CACHE_MAX {
            if let Some(old) = order.pop_front() {
                cache.remove(&old);
            }
        }
        drop((cache, order));

        if h <= 0 {
            return false;
        }
        let ratio = f64::from(w) / f64::from(h);
        let mut ratios = self.photo_ratio.borrow_mut();
        match ratios.insert(uid.to_string(), ratio) {
            // Only a *changed* ratio invalidates a layout; re-decoding a photo we
            // already sized correctly must not trigger another pass.
            Some(prev) if (prev - ratio).abs() < f64::EPSILON => false,
            _ => {
                self.photo_ratio_dirty.set(true);
                true
            }
        }
    }

    /// Where the learned aspect ratios are persisted between runs.
    fn ratio_path(&self) -> PathBuf {
        self.dirs.cache_dir().join("photo-ratios.json")
    }

    /// Load the persisted aspect ratios. A missing or corrupt file just means the
    /// first paint uses [`RATIO_UNKNOWN`] and re-justifies as thumbnails land.
    fn load_ratios(&self) {
        let path = self.ratio_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        match serde_json::from_str::<HashMap<String, f64>>(&text) {
            Ok(map) => *self.photo_ratio.borrow_mut() = map,
            Err(e) => tracing::debug!("ignoring {}: {e}", path.display()),
        }
    }

    /// Persist the learned aspect ratios, if any were learned since the last save.
    fn save_ratios(&self) {
        if !self.photo_ratio_dirty.replace(false) {
            return;
        }
        let path = self.ratio_path();
        let ratios = self.photo_ratio.borrow();
        let write = serde_json::to_vec(&*ratios)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
                std::fs::write(&path, bytes)
            });
        if let Err(e) = write {
            tracing::debug!("cannot save {}: {e}", path.display());
        }
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        load_proton_theme();
        // Refresh the file manager's right-click pin/unpin scripts, so they always
        // match the installed `pdfs`.
        pdfs_core::shell::install_file_manager_scripts();
        spawn_tray();
    });
    app.connect_activate(build_window);
    app.run()
}

/// Spawn the tray icon process in the background.
fn spawn_tray() {
    match Command::new("pdfs-tray").spawn() {
        Ok(_) => tracing::info!("spawned `pdfs-tray`"),
        Err(e) => tracing::error!("failed to spawn `pdfs-tray`: {e}"),
    }
}

/// Install a CSS provider that overrides libadwaita's accent colour with Proton
/// purple, app-wide. Named-colour overrides recolour the stock widgets (switch,
/// buttons, progress fill) without per-widget styling.
fn load_proton_theme() {
    // Compile-in and register our custom GResources (e.g. custom icons)
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/pdfs.gresource"));
    let resource_data = glib::Bytes::from_static(bytes);
    if let Ok(resource) = gio::Resource::from_data(&resource_data) {
        gio::resources_register(&resource);
    } else {
        tracing::error!("failed to load gresource bundle");
    }

    let css = format!(
        "@define-color accent_bg_color {PROTON_PURPLE};\n\
         @define-color accent_color {PROTON_PURPLE};\n\
         .brand-title {{ font-size: 1.6rem; font-weight: 800; }}\n\
         .brand-icon {{ color: {PROTON_PURPLE}; }}\n\
         .file-grid {{ padding: 6px; }}\n\
         .file-tile {{ padding: 8px; border-radius: 10px; }}\n\
         .file-tile:hover {{ background: alpha({PROTON_PURPLE}, 0.10); }}\n\
         .file-badge {{ -gtk-icon-shadow: 0 1px 2px rgba(0, 0, 0, 0.5); }}\n\
         .badge-pinned {{ color: #f5c211; }}\n\
         .badge-cached {{ color: #2ec27e; }}\n\
         .badge-cloud {{ color: #9aa0a6; }}\n\
         .photo-viewer-window {{ background-color: #111014; }}\n\
         .viewer-top-bar {{ background: linear-gradient(to bottom, rgba(0, 0, 0, 0.75), rgba(0, 0, 0, 0)); padding: 10px 16px 28px 20px; color: white; }}\n\
         .viewer-title {{ font-weight: 700; font-size: 1.05rem; text-shadow: 0 1px 3px rgba(0, 0, 0, 0.9); color: white; }}\n\
         .viewer-counter {{ font-size: 0.8rem; color: rgba(255, 255, 255, 0.72); text-shadow: 0 1px 3px rgba(0, 0, 0, 0.9); }}\n\
         .viewer-action-btn {{ color: white; background-color: rgba(255, 255, 255, 0.12); border-radius: 50%; min-width: 34px; min-height: 34px; margin-left: 4px; }}\n\
         .viewer-action-btn:hover {{ background-color: rgba(255, 255, 255, 0.28); color: white; }}\n\
         .viewer-action-btn:checked {{ background-color: {PROTON_PURPLE}; color: white; }}\n\
         .viewer-close-btn {{ background-color: rgba(255, 255, 255, 0.2); margin-left: 10px; }}\n\
         .viewer-close-btn:hover {{ background-color: #e01b24; color: white; }}\n\
         .viewer-nav-btn {{ background-color: rgba(0, 0, 0, 0.45); color: white; margin: 20px; min-width: 44px; min-height: 44px; border-radius: 50%; opacity: 0.7; transition: opacity 150ms ease, background-color 150ms ease; }}\n\
         .viewer-nav-btn:hover {{ background-color: rgba(0, 0, 0, 0.85); color: white; opacity: 1; }}\n\
         .viewer-nav-btn:disabled {{ opacity: 0; }}\n\
         .viewer-spinner {{ color: white; }}\n\
         .viewer-status {{ color: white; font-size: 1rem; background-color: rgba(0, 0, 0, 0.75); padding: 12px 24px; border-radius: 12px; }}\n\
         .viewer-info-panel {{ background-color: @window_bg_color; border-left: 1px solid alpha(currentColor, 0.12); }}\n\
         .gallery-day {{ font-size: 1.05rem; font-weight: 700; padding: 2px 2px 6px 2px; }}\n\
         .photo-tile {{ padding: 0; margin: 0; min-height: 0; min-width: 0; border-radius: 10px; background: alpha(currentColor, 0.06); box-shadow: none; transition: filter 160ms ease, box-shadow 160ms ease; }}\n\
         .photo-tile:hover {{ filter: brightness(1.06); box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35); }}\n\
         .photo-tile:focus {{ outline: 2px solid {PROTON_PURPLE}; outline-offset: -2px; }}\n\
         .photo-placeholder {{ color: alpha(currentColor, 0.35); background: alpha(currentColor, 0.07); }}\n\
         .photo-caption {{ font-size: 0.78rem; color: white; text-shadow: 0 1px 3px rgba(0, 0, 0, 0.9); padding: 22px 10px 6px 10px; opacity: 0; transition: opacity 160ms ease; }}\n\
         .photo-video-badge {{ color: white; background: rgba(0, 0, 0, 0.45); border-radius: 999px; padding: 8px; min-width: 20px; min-height: 20px; box-shadow: 0 2px 8px rgba(0, 0, 0, 0.5); transition: background 160ms ease; }}\n\
         .photo-tile:hover .photo-video-badge {{ background: alpha({PROTON_PURPLE}, 0.85); }}\n\
         .photo-tile:hover .photo-caption {{ opacity: 1; background: linear-gradient(to top, rgba(0, 0, 0, 0.55), rgba(0, 0, 0, 0)); }}\n\
         .card {{ border-radius: 8px; transition: transform 0.2s ease, filter 0.2s ease; margin: 4px; }}\n\
         .card:hover {{ transform: scale(1.02); filter: brightness(0.9); }}\n\
         .navigation-sidebar row {{ border-radius: 8px; margin: 2px 6px; }}\n\
         .navigation-sidebar row:selected {{ background: alpha({PROTON_PURPLE}, 0.16); color: {PROTON_PURPLE}; font-weight: 600; }}\n\
         .navigation-sidebar row:selected image {{ color: {PROTON_PURPLE}; }}\n\
         .file-tile:selected, .file-tile:hover:selected {{ background: alpha({PROTON_PURPLE}, 0.20); }}\n"
    );
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        // Register custom icons directory from our GResource with the icon theme
        let icon_theme = gtk4::IconTheme::for_display(&display);
        icon_theme.add_resource_path("/de/nils/protondrivelinux/icons");
    }
}

/// Build the application window, wire the two pages, kick off the refresh loop,
/// and present it.
fn build_window(app: &adw::Application) {
    let dirs = match AppDirs::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("cannot resolve app dirs: {e}");
            return;
        }
    };

    let stack = adw::ViewStack::new();
    let (login_page, login_widgets) = build_login_page();
    let (main_page, main_widgets) = build_main_page();
    let (browser_page, browser_widgets) = build_browser_page();
    let (gallery_page, gallery_widgets) = build_gallery_page();
    let (shared_page, shared_widgets) = build_shared_page();
    let (shared_by_me_page, shared_by_me_widgets) = build_shared_by_me_page();
    let (devices_page, devices_widgets) = build_devices_page();
    let (activity_page, activity_widgets) = build_activity_page();
    let (trash_page, trash_widgets) = build_trash_page();
    stack.add_named(&login_page, Some("login"));
    stack.add_named(&main_page, Some("main"));
    stack.add_named(&browser_page, Some("browser"));
    stack.add_named(&gallery_page, Some("gallery"));
    stack.add_named(&shared_by_me_page, Some("sharedbyme"));
    stack.add_named(&shared_page, Some("shared"));
    stack.add_named(&devices_page, Some("devices"));
    stack.add_named(&activity_page, Some("activity"));
    stack.add_named(&trash_page, Some("trash"));

    // Sidebar: the signed-in destinations. Selecting a row swaps the page stack;
    // `sync_sidebar` pushes the other way when navigation happens elsewhere (e.g.
    // login lands on Files).
    let (sidebar_page, sidebar_list) = build_sidebar();

    // Header spinner, hidden until a background open/load is in flight.
    let spinner = gtk4::Spinner::new();
    spinner.set_visible(false);
    let header = adw::HeaderBar::new();
    header.pack_end(&build_primary_menu());
    header.pack_end(&spinner);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let content_page = adw::NavigationPage::builder()
        .title("Proton Drive")
        .child(&toolbar)
        .build();
    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar_page)
        .content(&content_page)
        .min_sidebar_width(200.0)
        .max_sidebar_width(240.0)
        .build();

    // Toasts float over everything, so a report from a background action reaches
    // the user whichever page they're on.
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));

    let ui = Rc::new(Ui {
        dirs,
        stack: stack.clone(),
        toasts: toasts.clone(),
        spinner: spinner.clone(),
        busy: Cell::new(0),
        opening: RefCell::new(HashSet::new()),
        session: RefCell::new(auth::load().ok()),
        status_inflight: Cell::new(false),
        email: login_widgets.0,
        password: login_widgets.1,
        login_button: login_widgets.2,
        login_status: login_widgets.3,
        account_row: main_widgets.account_row.clone(),
        mount_row: main_widgets.mount_row.clone(),
        transfers_group: main_widgets.transfers_group.clone(),
        transfer_rows: RefCell::new(Vec::new()),
        transfers_inflight: Cell::new(false),
        cache_bar: main_widgets.cache_bar.clone(),
        cache_label: main_widgets.cache_label.clone(),
        autostart_row: main_widgets.autostart_row.clone(),
        budget_row: main_widgets.budget_row.clone(),
        mountpoint_row: main_widgets.mountpoint_row.clone(),
        settings_suppress: Cell::new(false),
        pins_group: main_widgets.pins_group.clone(),
        pin_rows: RefCell::new(Vec::new()),
        pins_state: RefCell::new(None),
        mounted: RefCell::new(false),
        notified_mounted: Cell::new(None),
        active_transfers: Cell::new(0),
        sidebar: sidebar_list.clone(),
        nav: split.clone(),
        browser_model: browser_widgets.model.clone(),
        browser_back: browser_widgets.back.clone(),
        browser_crumb: browser_widgets.crumb.clone(),
        browser_content: browser_widgets.content.clone(),
        browser_status: browser_widgets.status.clone(),
        browser_retry: browser_widgets.retry.clone(),
        browser_split: browser_widgets.split.clone(),
        details: browser_widgets.details,
        details_entry: RefCell::new(None),
        details_suppress: Cell::new(false),
        grid_selection: browser_widgets.grid_selection.clone(),
        list_selection: browser_widgets.list_selection.clone(),
        browser_path: RefCell::new(String::new()),
        browser_search: browser_widgets.search.clone(),
        browser_new_folder: browser_widgets.new_folder.clone(),
        browser_upload: browser_widgets.upload.clone(),
        browser_upload_folder: browser_widgets.upload_folder.clone(),
        search_source: RefCell::new(None),
        trash_model: trash_widgets.model.clone(),
        trash_content: trash_widgets.content.clone(),
        trash_status: trash_widgets.status.clone(),
        trash_retry: trash_widgets.retry.clone(),
        trash_empty: trash_widgets.empty.clone(),
        trash_subtitle: trash_widgets.subtitle.clone(),
        gallery_model: gallery_widgets.model.clone(),
        gallery_groups: gallery_widgets.groups.clone(),
        gallery_tile: Cell::new(TILE_DEFAULT),
        gallery_content: gallery_widgets.content.clone(),
        gallery_status: gallery_widgets.status.clone(),
        gallery_retry: gallery_widgets.retry.clone(),
        gallery_more: gallery_widgets.more.clone(),
        gallery_upload: gallery_widgets.upload.clone(),
        gallery_subtitle: gallery_widgets.subtitle.clone(),
        gallery_kind: Cell::new(None),
        gallery_tabs: gallery_widgets.tabs.clone(),
        gallery_dates: gallery_widgets.dates.clone(),
        gallery_date_ranges: RefCell::new(vec![None]),
        gallery_range: Cell::new(None),
        gallery_date_suppress: Cell::new(false),
        gallery_loading: Cell::new(false),
        gallery_width: Cell::new(0),
        photo_tex: RefCell::new(HashMap::new()),
        photo_tex_order: RefCell::new(VecDeque::new()),
        photo_ratio: RefCell::new(HashMap::new()),
        photo_ratio_dirty: Cell::new(false),
        photo_nothumb: RefCell::new(HashSet::new()),
        thumb_wanted: RefCell::new(HashMap::new()),
        thumb_queue: RefCell::new(VecDeque::new()),
        thumb_inflight: Cell::new(false),
        decode_queue: RefCell::new(VecDeque::new()),
        decode_idle: Cell::new(false),
        thumb_source: RefCell::new(None),
        relayout_source: RefCell::new(None),
        gallery_bound: RefCell::new(HashMap::new()),
        shared_content: shared_widgets.content.clone(),
        shared_status: shared_widgets.status.clone(),
        shared_retry: shared_widgets.retry.clone(),
        shared_with_me_group: shared_widgets.shared_with_me.clone(),
        invitations_group: shared_widgets.invitations.clone(),
        bookmarks_group: shared_widgets.bookmarks.clone(),
        shared_rows: RefCell::new(Vec::new()),
        shared_inflight: Cell::new(false),
        shared_loaded_at: Cell::new(None),
        devices_content: devices_widgets.content.clone(),
        devices_status: devices_widgets.status.clone(),
        devices_retry: devices_widgets.retry.clone(),
        devices_group: devices_widgets.group.clone(),
        devices_rows: RefCell::new(Vec::new()),
        devices_sync_group: devices_widgets.sync_group.clone(),
        devices_sync_rows: RefCell::new(Vec::new()),
        devices_inflight: Cell::new(false),
        devices_loaded_at: Cell::new(None),
        shared_by_me_content: shared_by_me_widgets.content.clone(),
        shared_by_me_status: shared_by_me_widgets.status.clone(),
        shared_by_me_retry: shared_by_me_widgets.retry.clone(),
        shared_by_me_group: shared_by_me_widgets.group.clone(),
        shared_by_me_rows: RefCell::new(Vec::new()),
        shared_by_me_inflight: Cell::new(false),
        shared_by_me_loaded_at: Cell::new(None),
        activity_content: activity_widgets.content.clone(),
        activity_status: activity_widgets.status.clone(),
        activity_retry: activity_widgets.retry.clone(),
        activity_group: activity_widgets.group.clone(),
        activity_rows: RefCell::new(Vec::new()),
        activity_inflight: Cell::new(false),
        activity_key: RefCell::new(None),
    });
    ui.load_ratios();

    wire_login(&ui);
    wire_logout(&ui, &main_widgets.logout_button);
    wire_settings(
        &ui,
        &main_widgets.purge_button,
        &main_widgets.mountpoint_button,
    );
    wire_sidebar(&ui);
    wire_browser(&ui, &browser_widgets.grid, &browser_widgets.column_view);
    wire_browser_actions(
        &ui,
        &browser_widgets.new_folder,
        &browser_widgets.upload,
        &browser_widgets.upload_folder,
    );
    wire_details(&ui);
    wire_search(&ui);
    wire_gallery(&ui, &gallery_widgets.list, &gallery_widgets.scroll);
    wire_trash(&ui, &trash_widgets.list, &trash_widgets.empty);
    wire_shared(&ui, &shared_widgets.retry, &shared_widgets.add_bookmark);
    wire_shared_by_me(&ui, &shared_by_me_widgets.retry);
    wire_devices(&ui, &devices_widgets.retry, &devices_widgets.add_folder);
    wire_activity(&ui, &activity_widgets.retry);
    wire_refresh(
        &ui,
        &[
            &browser_widgets.refresh,
            &gallery_widgets.refresh,
            &trash_widgets.refresh,
            &shared_widgets.refresh,
            &shared_by_me_widgets.refresh,
            &devices_widgets.refresh,
            &activity_widgets.refresh,
        ],
    );
    wire_retry(&ui);

    // Lazily load the Files / Photos pages the first time they're shown, so the
    // network round-trip only happens on demand rather than on every refresh.
    let ui_nav = ui.clone();
    stack.connect_visible_child_name_notify(move |st| {
        sync_sidebar(&ui_nav);
        match st.visible_child_name().as_deref() {
            Some("browser") => load_browser(&ui_nav),
            Some("gallery") => load_gallery(&ui_nav, false),
            // Network-backed pages skip the fetch (and the "Loading…" flash) when
            // the rows on screen are still fresh; the Retry button and mutations
            // invalidate the timestamp to force a reload.
            Some("sharedbyme") if page_fresh(&ui_nav.shared_by_me_loaded_at) => {}
            Some("sharedbyme") => load_shared_by_me(&ui_nav),
            Some("shared") if page_fresh(&ui_nav.shared_loaded_at) => {}
            Some("shared") => load_shared(&ui_nav),
            Some("devices") if page_fresh(&ui_nav.devices_loaded_at) => {}
            Some("devices") => load_devices(&ui_nav),
            // Activity is intentionally not TTL-cached: it changes out from under
            // the page as background uploads and edits complete, so it reloads on
            // every visit to stay live.
            Some("activity") => load_activity(&ui_nav),
            Some("trash") => load_trash(&ui_nav),
            _ => {}
        }
    });

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Proton Drive")
        .default_width(980)
        .default_height(680)
        .content(&toasts)
        .build();
    install_shortcuts(&ui, &window);
    install_window_actions(&window);

    refresh(&ui);
    // Periodic refresh while the window lives. The closure holds a strong `Rc`;
    // it is dropped when the source is removed on window close.
    let ui_tick = ui.clone();
    let source = glib::timeout_add_local(REFRESH_INTERVAL, move || {
        refresh(&ui_tick);
        glib::ControlFlow::Continue
    });
    let cell = RefCell::new(Some(source));
    let ui_close = ui.clone();
    window.connect_close_request(move |_| {
        if let Some(id) = cell.borrow_mut().take() {
            id.remove();
        }
        ui_close.save_ratios();
        glib::Propagation::Proceed
    });

    window.present();
}

/// The sidebar destinations, in order: the row index is the index into this table,
/// and each entry is `(stack page name, label, icon)`.
const DESTINATIONS: [(&str, &str, &str); 8] = [
    ("browser", "My files", "folder-symbolic"),
    ("sharedbyme", "Shared", "emblem-shared-symbolic"),
    ("shared", "Shared with me", "system-users-symbolic"),
    ("devices", "Computers", "computer-symbolic"),
    ("gallery", "Photos", "image-x-generic-symbolic"),
    ("activity", "Activity", "document-open-recent-symbolic"),
    ("trash", "Trash", "user-trash-symbolic"),
    ("main", "Settings", "emblem-system-symbolic"),
];

/// The navigation sidebar: a Proton-branded header over one row per destination.
/// Returns the page (for the split view) and the list (to drive + reflect the
/// current page).
fn build_sidebar() -> (adw::NavigationPage, gtk4::ListBox) {
    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    for (_, label, icon) in DESTINATIONS {
        let row = adw::ActionRow::builder().title(label).build();
        row.add_prefix(&gtk4::Image::from_icon_name(icon));
        list.append(&row);
    }

    let brand = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let icon = gtk4::Image::from_icon_name("folder-remote-symbolic");
    icon.add_css_class("brand-icon");
    brand.append(&icon);
    brand.append(&gtk4::Label::new(Some("Proton Drive")));

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&brand));

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));

    let page = adw::NavigationPage::builder()
        .title("Proton Drive")
        .child(&toolbar)
        .build();
    (page, list)
}

/// Selecting a sidebar row navigates the page stack. The reverse direction (stack
/// → sidebar highlight) is [`sync_sidebar`], so the two can't fight: this handler
/// only ever writes the stack.
fn wire_sidebar(ui: &Rc<Ui>) {
    let ui_row = ui.clone();
    ui.sidebar.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        let Some((page, _, _)) = DESTINATIONS.get(row.index() as usize) else {
            return;
        };
        if ui_row.stack.visible_child_name().as_deref() != Some(*page) {
            ui_row.stack.set_visible_child_name(page);
        }
    });
}

/// Whether a network-backed page painted good data within [`PAGE_TTL`] and so
/// can be reused without re-fetching. A `None` timestamp (never loaded, or
/// invalidated by a mutation) is always stale.
fn page_fresh(loaded_at: &Cell<Option<Instant>>) -> bool {
    loaded_at.get().is_some_and(|t| t.elapsed() < PAGE_TTL)
}

/// A page header's Refresh button. Every page that can show stale rows carries
/// one, so the user never has to guess whether what they're looking at is
/// current or wait out a TTL they can't see.
fn refresh_button() -> gtk4::Button {
    let button = gtk4::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh (F5)")
        .valign(gtk4::Align::Center)
        .build();
    button.add_css_class("flat");
    button
}

/// Point every page's Refresh button at the current page. One handler for all of
/// them: the button acts on whatever is on screen, so it can't refresh a page the
/// user has since navigated away from.
fn wire_refresh(ui: &Rc<Ui>, buttons: &[&gtk4::Button]) {
    for button in buttons {
        let ui = ui.clone();
        button.connect_clicked(move |_| reload_current_page(&ui));
    }
}

/// Re-fetch the visible page from the server, bypassing every layer of cache
/// between it and the account.
///
/// The two layers are separate: the daemon's own persisted listings (folders,
/// trash, photos) are dropped with [`Request::Refresh`] before re-asking, while
/// the pages the daemon always fetches live (sharing, devices, activity) only
/// need this front-end's [`PAGE_TTL`] stamp cleared.
fn reload_current_page(ui: &Rc<Ui>) {
    match ui.stack.visible_child_name().as_deref() {
        Some("browser") => {
            let path = ui.browser_path.borrow().clone();
            refresh_then(ui, RefreshScope::Dir { path }, load_browser);
        }
        Some("gallery") => refresh_then(ui, RefreshScope::Photos, |ui| load_gallery(ui, false)),
        Some("trash") => refresh_then(ui, RefreshScope::Trash, load_trash),
        Some("shared") => {
            ui.shared_loaded_at.set(None);
            load_shared(ui);
        }
        Some("sharedbyme") => {
            ui.shared_by_me_loaded_at.set(None);
            load_shared_by_me(ui);
        }
        Some("devices") => {
            ui.devices_loaded_at.set(None);
            load_devices(ui);
        }
        Some("activity") => load_activity(ui),
        _ => {}
    }
}

/// Drop a daemon-side cached listing, then run the page's loader to re-fetch it.
///
/// The loader runs even when the invalidation failed: it is the loader that knows
/// how to report an unreachable daemon on its own page, and a refresh that fails
/// silently would read as a dead button.
fn refresh_then(ui: &Rc<Ui>, scope: RefreshScope, load: fn(&Rc<Ui>)) {
    let rx = spawn_request(ui.dirs.control_socket(), Request::Refresh { scope });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let _ = rx.recv().await;
        load(&ui);
    });
}

/// Highlight the sidebar row for whichever page the stack is showing, so
/// navigation that doesn't start in the sidebar (login landing on Files, the tray
/// raising the window) still moves the selection.
fn sync_sidebar(ui: &Rc<Ui>) {
    let Some(current) = ui.stack.visible_child_name() else {
        return;
    };
    let index = DESTINATIONS
        .iter()
        .position(|(page, _, _)| *page == current);
    match index.and_then(|i| ui.sidebar.row_at_index(i as i32)) {
        Some(row) => {
            if ui.sidebar.selected_row().as_ref() != Some(&row) {
                ui.sidebar.select_row(Some(&row));
            }
        }
        // The login page has no destination row.
        None => ui.sidebar.unselect_all(),
    }
}

/// Show a transient toast. Non-blocking by design: an action's outcome is
/// reported without stealing focus or forcing a click, so the user can keep
/// working while a slow upload lands.
fn toast(ui: &Rc<Ui>, message: &str) {
    ui.toasts.add_toast(adw::Toast::new(message));
}

/// Show a toast for a failure. Same surface as [`toast`], but the message is
/// prefixed with what was being attempted, since a bare daemon error ("no such
/// file") reads as noise without it.
fn toast_error(ui: &Rc<Ui>, what: &str, detail: &str) {
    let detail = detail.trim();
    let message = if detail.is_empty() {
        what.to_string()
    } else {
        format!("{what}: {detail}")
    };
    tracing::warn!("{message}");
    let toast = adw::Toast::builder().title(&message).timeout(6).build();
    ui.toasts.add_toast(toast);
}

/// The header's primary (hamburger) menu: the app-level entries that don't belong
/// on any one page.
fn build_primary_menu() -> gtk4::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About Proton Drive"), Some("win.about"));
    gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main menu")
        .menu_model(&menu)
        .build()
}

/// Back the primary menu's entries with window actions.
fn install_window_actions(window: &adw::ApplicationWindow) {
    let shortcuts = gio::SimpleAction::new("shortcuts", None);
    let win = window.clone();
    shortcuts.connect_activate(move |_, _| show_shortcuts(&win));
    window.add_action(&shortcuts);

    let about = gio::SimpleAction::new("about", None);
    let win = window.clone();
    about.connect_activate(move |_, _| {
        let dialog = adw::AboutDialog::builder()
            .application_name("Proton Drive")
            .application_icon("folder-remote-symbolic")
            .version(pdfs_core::config::APP_VERSION)
            .developer_name("proton-drive-linux")
            .comments("On-demand Proton Drive sync for Linux.")
            .build();
        dialog.present(Some(&win));
    });
    window.add_action(&about);
}

/// The keyboard-shortcut cheatsheet behind the menu entry, listing exactly what
/// [`install_shortcuts`] binds.
fn show_shortcuts(window: &adw::ApplicationWindow) {
    const KEYS: [(&str, &str); 6] = [
        ("Ctrl+F", "Search Drive"),
        ("Ctrl+N", "New folder"),
        ("Ctrl+U", "Upload file"),
        ("F2", "Rename selection"),
        ("Delete", "Move selection to Trash"),
        ("Escape", "Close the details pane"),
    ];
    let group = adw::PreferencesGroup::builder().title("Files").build();
    for (keys, action) in KEYS {
        let row = adw::ActionRow::builder().title(action).build();
        let label = gtk4::Label::builder()
            .label(keys)
            .valign(gtk4::Align::Center)
            .build();
        label.add_css_class("dim-label");
        label.add_css_class("monospace");
        row.add_suffix(&label);
        group.add(&row);
    }
    let page = adw::PreferencesPage::new();
    page.add(&group);

    let dialog = adw::Dialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(420)
        .child(&{
            let toolbar = adw::ToolbarView::new();
            toolbar.add_top_bar(&adw::HeaderBar::new());
            toolbar.set_content(Some(&page));
            toolbar
        })
        .build();
    dialog.present(Some(window));
}

/// Window-level keyboard shortcuts, so the browser is usable without the mouse:
/// Ctrl+F focuses search, Ctrl+N makes a folder, Ctrl+U uploads, F2 renames and
/// Delete trashes the selected entry, Escape closes the details pane.
fn install_shortcuts(ui: &Rc<Ui>, window: &adw::ApplicationWindow) {
    let controller = gtk4::EventControllerKey::new();
    let ui = ui.clone();
    controller.connect_key_pressed(move |_, key, _, state| {
        let ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
        let on_browser = ui.stack.visible_child_name().as_deref() == Some("browser");
        match key {
            // Refresh works on every page, so it is matched before the
            // browser-only bindings.
            gtk4::gdk::Key::F5 => reload_current_page(&ui),
            gtk4::gdk::Key::r | gtk4::gdk::Key::R if ctrl => reload_current_page(&ui),
            gtk4::gdk::Key::f | gtk4::gdk::Key::F if ctrl && on_browser => {
                ui.browser_search.grab_focus();
            }
            gtk4::gdk::Key::n | gtk4::gdk::Key::N if ctrl && on_browser => prompt_new_folder(&ui),
            gtk4::gdk::Key::u | gtk4::gdk::Key::U if ctrl && on_browser => prompt_upload(&ui),
            gtk4::gdk::Key::F2 if on_browser => {
                if let Some(entry) = selected_entry(&ui) {
                    prompt_rename(&ui, &entry);
                }
            }
            gtk4::gdk::Key::Delete if on_browser => {
                if let Some(entry) = selected_entry(&ui) {
                    prompt_delete(&ui, &entry);
                }
            }
            gtk4::gdk::Key::Escape if on_browser && ui.browser_split.shows_sidebar() => {
                hide_details(&ui);
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(controller);
}

/// The login page: an email row, password row, a primary "Sign in" button and a
/// status label, centred in a clamp. The 2FA code is prompted lazily in a
/// dialog (see [`prompt_2fa`]) only when the account actually requires it.
#[allow(clippy::type_complexity)]
fn build_login_page() -> (
    gtk4::Widget,
    (
        adw::EntryRow,
        adw::PasswordEntryRow,
        gtk4::Button,
        gtk4::Label,
    ),
) {
    let group = adw::PreferencesGroup::builder()
        .title("Sign in to Proton")
        .description("Use your Proton account to connect Drive.")
        .build();

    let email = adw::EntryRow::builder().title("Email or username").build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    group.add(&email);
    group.add(&password);

    let login_button = gtk4::Button::builder()
        .label("Sign in")
        .halign(gtk4::Align::Center)
        .build();
    login_button.add_css_class("suggested-action");
    login_button.add_css_class("pill");

    let login_status = gtk4::Label::builder()
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .build();
    login_status.add_css_class("dim-label");

    let header = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let logo = gtk4::Image::from_icon_name("folder-remote-symbolic");
    logo.set_pixel_size(64);
    logo.add_css_class("brand-icon");
    let title = gtk4::Label::new(Some("Proton Drive"));
    title.add_css_class("brand-title");
    header.append(&logo);
    header.append(&title);
    header.set_margin_bottom(12);

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    inner.set_margin_top(32);
    inner.set_margin_bottom(32);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&group);
    inner.append(&login_button);
    inner.append(&login_status);

    let clamp = adw::Clamp::builder()
        .maximum_size(420)
        .child(&inner)
        .build();
    let scroll = gtk4::ScrolledWindow::builder().child(&clamp).build();

    (
        scroll.upcast(),
        (email, password, login_button, login_status),
    )
}

/// Widgets the settings page hands back for the refresh loop and action wiring.
struct MainWidgets {
    account_row: adw::ActionRow,
    mount_row: adw::ActionRow,
    /// Live upload/download progress, populated by the refresh loop.
    transfers_group: adw::PreferencesGroup,
    cache_bar: gtk4::ProgressBar,
    cache_label: gtk4::Label,
    pins_group: adw::PreferencesGroup,
    logout_button: gtk4::Button,
    /// "Start on login" toggle, reflecting the systemd unit's enabled state.
    autostart_row: adw::SwitchRow,
    /// Cache soft-cap editor, in GiB; `0` = unlimited.
    budget_row: adw::SpinRow,
    /// Purges all unpinned cached content.
    purge_button: gtk4::Button,
    /// Shows the active mountpoint; its suffix button opens a folder chooser.
    mountpoint_row: adw::ActionRow,
    mountpoint_button: gtk4::Button,
}

/// The main (logged-in) page: a libadwaita settings surface — account header,
/// mount status, storage controls (cache budget + purge), system integration
/// (start-on-login, mountpoint), the pin list, and developer overrides. Returns
/// the widgets the refresh loop updates plus the controls to wire.
fn build_main_page() -> (gtk4::Widget, MainWidgets) {
    // Account group: identity + sign-out.
    let account_group = adw::PreferencesGroup::new();
    let account_row = adw::ActionRow::builder().title("Not signed in").build();
    let avatar = adw::Avatar::new(40, None, true);
    account_row.add_prefix(&avatar);
    let logout_button = gtk4::Button::builder()
        .label("Sign out")
        .valign(gtk4::Align::Center)
        .build();
    logout_button.add_css_class("flat");
    account_row.add_suffix(&logout_button);
    account_group.add(&account_row);

    // Mount group: a read-only status line. The mount is managed automatically
    // by the systemd user service; there is no toggle to fiddle with.
    let mount_group = adw::PreferencesGroup::builder().title("Drive").build();
    let mount_row = adw::ActionRow::builder()
        .title("Proton Drive")
        .subtitle("Not mounted")
        .build();
    mount_group.add(&mount_row);

    // Activity group: live upload/download progress. Hidden until the refresh
    // loop sees an in-flight transfer from `Request::GetQueueStatus`.
    let transfers_group = adw::PreferencesGroup::builder()
        .title("Activity")
        .description("Files moving to and from Proton Drive.")
        .visible(false)
        .build();

    // Storage group: a progress bar + "X of Y used" label, plus the cache-budget
    // editor and a purge button. `budget_row`/`purge_button` are wired in
    // `wire_settings`; the bar + label are repainted by the refresh loop.
    let storage_group = adw::PreferencesGroup::builder()
        .title("Storage")
        .description("Local cache for pinned and recently opened files.")
        .build();
    let storage_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    storage_box.set_margin_top(6);
    storage_box.set_margin_bottom(6);
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
        .title("Cache budget (GiB)")
        .subtitle("Soft cap for cached content; 0 = unlimited.")
        .adjustment(&budget_adj)
        .digits(1)
        .build();
    storage_group.add(&budget_row);
    let purge_row = adw::ActionRow::builder()
        .title("Purge cache")
        .subtitle("Delete cached content. Pinned files are kept.")
        .build();
    let purge_button = gtk4::Button::builder()
        .label("Purge")
        .valign(gtk4::Align::Center)
        .build();
    purge_button.add_css_class("destructive-action");
    purge_row.add_suffix(&purge_button);
    storage_group.add(&purge_row);

    // System integration: start-on-login + mountpoint chooser.
    let system_group = adw::PreferencesGroup::builder()
        .title("System integration")
        .build();
    let autostart_row = adw::SwitchRow::builder()
        .title("Start on login")
        .subtitle("Mount Proton Drive automatically when you log in.")
        .build();
    system_group.add(&autostart_row);
    let mountpoint_row = adw::ActionRow::builder()
        .title("Mountpoint")
        .subtitle("—")
        .build();
    let mountpoint_button = gtk4::Button::builder()
        .label("Change")
        .valign(gtk4::Align::Center)
        .build();
    mountpoint_button.add_css_class("flat");
    mountpoint_row.add_suffix(&mountpoint_button);
    system_group.add(&mountpoint_row);

    // Pins group: filled in by refresh.
    let pins_group = adw::PreferencesGroup::builder()
        .title("Pinned files")
        .description("Kept available offline on this device.")
        .build();

    // Developer overrides: read-only client identity, for support/debugging.
    let dev_group = adw::PreferencesGroup::builder().title("Developer").build();
    let version_row = adw::ActionRow::builder()
        .title("App version")
        .subtitle(pdfs_core::config::APP_VERSION)
        .build();
    version_row.add_css_class("property");
    let agent_row = adw::ActionRow::builder()
        .title("User agent")
        .subtitle(pdfs_core::config::USER_AGENT)
        .build();
    agent_row.add_css_class("property");
    dev_group.add(&version_row);
    dev_group.add(&agent_row);

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    inner.set_margin_top(18);
    inner.set_margin_bottom(18);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&account_group);
    inner.append(&mount_group);
    inner.append(&transfers_group);
    inner.append(&storage_group);
    inner.append(&system_group);
    inner.append(&pins_group);
    inner.append(&dev_group);

    let clamp = adw::Clamp::builder()
        .maximum_size(560)
        .child(&inner)
        .build();
    let scroll = gtk4::ScrolledWindow::builder().child(&clamp).build();

    (
        scroll.upcast(),
        MainWidgets {
            account_row,
            mount_row,
            transfers_group,
            cache_bar,
            cache_label,
            pins_group,
            logout_button,
            autostart_row,
            budget_row,
            purge_button,
            mountpoint_row,
            mountpoint_button,
        },
    )
}

/// Connect the sign-in button: read the fields, run [`auth::login`] on a worker
/// thread, and report the outcome back on the main loop.
fn wire_login(ui: &Rc<Ui>) {
    // Pressing Enter in either field submits the form, so signing in never needs
    // a reach for the mouse. Both fields route to the same button.
    let btn = ui.login_button.clone();
    ui.email
        .connect_entry_activated(move |_| btn.emit_clicked());
    let btn = ui.login_button.clone();
    ui.password
        .connect_entry_activated(move |_| btn.emit_clicked());

    let ui = ui.clone();
    let button = ui.login_button.clone();
    button.connect_clicked(move |_| {
        let username = ui.email.text().to_string();
        let password = ui.password.text().to_string();
        if username.is_empty() || password.is_empty() {
            ui.login_status.set_text("Enter your email and password.");
            return;
        }

        ui.login_button.set_sensitive(false);
        ui.login_status.set_text("Signing in…");
        let (rx, totp_req_rx) = spawn_login(username, password);

        // Surface the 2FA dialog only if the SDK actually asks for a code (i.e.
        // the account has 2FA enabled). The worker blocks until the dialog feeds
        // back a code (or is cancelled, dropping the sender).
        let ui_2fa = ui.clone();
        glib::spawn_future_local(async move {
            if let Ok(code_tx) = totp_req_rx.recv().await {
                prompt_2fa(&ui_2fa, code_tx);
            }
        });

        let ui = ui.clone();
        glib::spawn_future_local(async move {
            let result = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err("login cancelled".into()));
            ui.login_button.set_sensitive(true);
            match result {
                Ok(()) => {
                    ui.login_status.set_text("");
                    ui.password.set_text("");
                    // Cache the new identity so `refresh` never hits the keyring.
                    *ui.session.borrow_mut() = auth::load().ok();
                    // Enable+start the mount service now that we have a session.
                    service::enable_start();
                    refresh(&ui);
                }
                Err(e) => ui.login_status.set_text(&format!("Sign-in failed: {e}")),
            }
        });
    });
}

/// Run the async SRP + optional 2FA login on a dedicated current-thread Tokio
/// runtime. Returns two channels: the first yields the final login result once;
/// the second fires *only if* the account needs a 2FA code, carrying a
/// [`std::sync::mpsc::Sender`] the UI uses to feed the code back. The login
/// closure blocks the worker on that sender until the dialog answers, so the
/// code is requested lazily and can't expire before the password proof.
#[allow(clippy::type_complexity)]
fn spawn_login(
    username: String,
    password: String,
) -> (
    async_channel::Receiver<Result<(), String>>,
    async_channel::Receiver<std::sync::mpsc::Sender<String>>,
) {
    let (tx, rx) = async_channel::bounded(1);
    let (totp_req_tx, totp_req_rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send_blocking(Err(e.to_string()));
                return;
            }
        };
        let result = rt.block_on(async move {
            auth::login(&username, &password, || {
                // 2FA required: hand a one-shot sender to the UI and block until
                // the dialog supplies the code. A dropped sender (cancelled
                // dialog) surfaces as a cancelled login.
                let (code_tx, code_rx) = std::sync::mpsc::channel::<String>();
                totp_req_tx
                    .send_blocking(code_tx)
                    .map_err(|_| pdfs_core::Error::Other("two-factor prompt closed".into()))?;
                code_rx
                    .recv()
                    .map_err(|_| pdfs_core::Error::Other("two-factor entry cancelled".into()))
            })
            .await
            .map_err(|e| e.to_string())
        });
        let _ = tx.send_blocking(result);
    });
    (rx, totp_req_rx)
}

/// Show the lazy two-factor dialog and feed the entered code back to the waiting
/// login worker via `code_tx`. Cancelling (or closing) drops the sender, which
/// the worker reads as a cancelled login.
fn prompt_2fa(ui: &Rc<Ui>, code_tx: std::sync::mpsc::Sender<String>) {
    let dialog = adw::AlertDialog::builder()
        .heading("Two-factor authentication")
        .body("Enter the code from your authenticator app.")
        .build();

    let group = adw::PreferencesGroup::new();
    let entry = adw::EntryRow::builder()
        .title("Authentication code")
        .activates_default(true)
        .build();
    group.add(&entry);
    dialog.set_extra_child(Some(&group));

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Confirm");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let code_tx = RefCell::new(Some(code_tx));
    dialog.connect_response(None, move |_, resp| {
        // On cancel/close we take + drop `tx` without sending, so the worker's
        // recv errors out and the login is reported as cancelled.
        if let Some(tx) = code_tx.borrow_mut().take()
            && resp == "confirm"
        {
            let _ = tx.send(entry.text().trim().to_string());
        }
    });

    let parent = ui.login_button.root().and_downcast::<gtk4::Window>();
    dialog.present(parent.as_ref());
}

/// Connect the sign-out button: disable+stop the mount service (so the daemon
/// isn't left running without credentials), forget the stored session, and drop
/// back to the login page.
fn wire_logout(ui: &Rc<Ui>, button: &gtk4::Button) {
    let ui = ui.clone();
    button.connect_clicked(move |_| {
        service::disable_stop();
        if let Err(e) = auth::logout() {
            tracing::error!("logout failed: {e}");
        }
        *ui.session.borrow_mut() = None;
        refresh(&ui);
    });
}

/// Bytes per GiB, for the cache-budget editor's unit conversion.
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Wire the Settings-page controls: the cache-budget editor, the purge button,
/// the start-on-login switch and the mountpoint chooser. Initial widget state is
/// read once from config / systemd here (the refresh loop owns only the live
/// mount + cache-usage read-out), with [`Ui::settings_suppress`] set around the
/// programmatic populate so the change handlers don't fire on it.
fn wire_settings(ui: &Rc<Ui>, purge_button: &gtk4::Button, mountpoint_button: &gtk4::Button) {
    let config = ui.dirs.load_config();

    // Populate from persisted config + the systemd unit state, suppressed.
    ui.settings_suppress.set(true);
    ui.budget_row
        .set_value(config.resolved_cache_budget() as f64 / GIB);
    ui.mountpoint_row
        .set_subtitle(&ui.dirs.resolved_mountpoint(&config).display().to_string());
    ui.autostart_row.set_active(service::is_enabled());
    ui.settings_suppress.set(false);

    // Cache budget: a user edit applies the new soft cap on the daemon (which
    // also persists it to config). 0 GiB = unlimited.
    let ui_budget = ui.clone();
    ui.budget_row.connect_value_notify(move |row| {
        if ui_budget.settings_suppress.get() {
            return;
        }
        let bytes = (row.value() * GIB).round() as u64;
        settings_request(
            &ui_budget,
            Request::SetCacheBudget { bytes },
            "Cache budget updated",
            "Couldn't set cache budget",
        );
    });

    // Purge: confirm, then drop all unpinned cached content via the daemon.
    let ui_purge = ui.clone();
    purge_button.connect_clicked(move |_| {
        let ui = ui_purge.clone();
        let dialog = adw::AlertDialog::builder()
            .heading("Purge cache")
            .body("Delete all cached content that isn't pinned? Pinned files stay offline.")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("purge", "Purge");
        dialog.set_response_appearance("purge", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |_, resp| {
            if resp == "purge" {
                settings_request(
                    &ui,
                    Request::PurgeCache,
                    "Cache purged",
                    "Couldn't purge cache",
                );
            }
        });
        dialog.present(ui_window(&ui_purge).as_ref());
    });

    // Start on login: enable/disable the systemd unit without stopping a live
    // mount (the user can disconnect separately).
    let ui_auto = ui.clone();
    ui.autostart_row.connect_active_notify(move |row| {
        if ui_auto.settings_suppress.get() {
            return;
        }
        if row.is_active() {
            service::enable();
        } else {
            service::disable();
        }
    });

    // Mountpoint: pick a folder, persist it, and offer to restart the mount so
    // the change takes effect (the daemon reads the path on mount).
    let ui_mp = ui.clone();
    mountpoint_button.connect_clicked(move |_| prompt_mountpoint(&ui_mp));
}

/// Run a settings control-socket round-trip (budget / purge) on a worker thread,
/// confirming with `done` or reporting the daemon's error under `failed`. Unlike
/// [`run_mutation`] there's no browser reload; the next refresh tick repaints the
/// cache read-out.
fn settings_request(ui: &Rc<Ui>, req: Request, done: &'static str, failed: &'static str) {
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => toast(&ui, done),
            Ok(Ok(Response::Error { message })) => toast_error(&ui, failed, &message),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}

/// Prompt for a new mountpoint folder, persist it to config, and offer to restart
/// the mount service so the daemon picks it up.
fn prompt_mountpoint(ui: &Rc<Ui>) {
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
        ui.mountpoint_row.set_subtitle(&path_str);

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
fn wire_retry(ui: &Rc<Ui>) {
    let ui_browser = ui.clone();
    ui.browser_retry.clone().connect_clicked(move |_| {
        service::restart();
        load_browser(&ui_browser);
    });
    let ui_gallery = ui.clone();
    ui.gallery_retry.clone().connect_clicked(move |_| {
        service::restart();
        load_gallery(&ui_gallery, false);
    });
    let ui_trash = ui.clone();
    ui.trash_retry.clone().connect_clicked(move |_| {
        service::restart();
        load_trash(&ui_trash);
    });
}

/// Repaint the window from the cached login identity, then kick an async mount-
/// status fetch. Runs on the 2s tick: the identity check is instant (no keyring),
/// and the status round-trip is offloaded to a worker so the main loop never
/// blocks on a slow or wedged daemon.
fn refresh(ui: &Rc<Ui>) {
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
                ui.account_row.set_title(&s.username);
                ui.account_row.set_subtitle("Proton account");
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
    match ui.stack.visible_child_name().as_deref() {
        Some("devices") => refresh_sync_folders(ui),
        Some("activity") => refresh_activity(ui),
        _ => {}
    }
}

/// Record the mount state seen by the last status poll: gate every control that
/// needs a live daemon, and notify the desktop when the state actually flips.
///
/// The gating is the point — without it, New Folder / Upload / the details pane's
/// actions stay clickable while the mount is down and each click buys a round-trip
/// that can only fail. A greyed control says so up front.
fn set_mounted(ui: &Rc<Ui>, mounted: bool) {
    *ui.mounted.borrow_mut() = mounted;
    ui.browser_new_folder.set_sensitive(mounted);
    ui.browser_upload.set_sensitive(mounted);
    ui.browser_upload_folder.set_sensitive(mounted);
    ui.gallery_upload.set_sensitive(mounted);
    ui.details.pin_row.set_sensitive(mounted);
    ui.details.rename_button.set_sensitive(mounted);
    ui.details.trash_button.set_sensitive(mounted);
    ui.details.open_button.set_sensitive(mounted);

    // Only notify on a real edge, and never for the first reading: at startup the
    // service is usually still coming up, and "disconnected" would be a lie.
    if ui.notified_mounted.get() == Some(mounted) {
        return;
    }
    let first = ui.notified_mounted.replace(Some(mounted)).is_none();
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

/// Send a desktop notification through the app's GIO channel. `id` replaces any
/// earlier notification with the same id, so a flapping mount updates one
/// notification instead of stacking a column of them.
fn notify(id: &str, title: &str, body: &str) {
    let Some(app) = gio::Application::default() else {
        return;
    };
    let notification = gio::Notification::new(title);
    notification.set_body(Some(body));
    notification.set_priority(gio::NotificationPriority::Low);
    app.send_notification(Some(id), &notification);
}

/// Poll the daemon's in-flight transfers on a worker thread and repaint the
/// Activity group. Independently inflight-guarded from [`refresh_status`] so the
/// two cheap polls on the 2s tick don't gate each other. The group hides itself
/// when nothing is moving, so an idle account shows no Activity section.
fn refresh_transfers(ui: &Rc<Ui>) {
    if ui.transfers_inflight.get() {
        return;
    }
    ui.transfers_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::GetQueueStatus);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.transfers_inflight.set(false);
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
fn repaint_transfers(ui: &Rc<Ui>, items: &[TransferItem], jobs: &[JobItem]) {
    let lines: Vec<ActivityLine> = jobs
        .iter()
        .map(job_line)
        .chain(items.iter().map(transfer_line))
        .collect();

    // The wire carries in-flight transfers only, with no completion event: the
    // count falling to zero is what "the sync finished" looks like from here.
    // Jobs are deliberately not counted — a bulk upload retires its scan job and
    // starts its upload job mid-flight, which is not a thing finishing.
    let previous = ui.active_transfers.replace(items.len());
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
        if !ui.transfer_rows.borrow().is_empty() {
            for tr in ui.transfer_rows.borrow_mut().drain(..) {
                ui.transfers_group.remove(&tr.row);
            }
        }
        ui.transfers_group.set_visible(false);
        return;
    }

    ui.transfers_group.set_visible(true);

    // Rebuild rows only when the count changes; otherwise reuse them in place.
    if ui.transfer_rows.borrow().len() != lines.len() {
        for tr in ui.transfer_rows.borrow_mut().drain(..) {
            ui.transfers_group.remove(&tr.row);
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
            ui.transfers_group.add(&row);
            ui.transfer_rows
                .borrow_mut()
                .push(TransferRow { row, label, bar });
        }
    }

    for (line, tr) in lines.iter().zip(ui.transfer_rows.borrow().iter()) {
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
fn job_line(j: &JobItem) -> ActivityLine {
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
fn transfer_line(t: &TransferItem) -> ActivityLine {
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
fn refresh_status(ui: &Rc<Ui>) {
    if ui.status_inflight.get() {
        return;
    }
    ui.status_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::Status);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.status_inflight.set(false);
        match result {
            Ok(Ok(Response::Status {
                mountpoint,
                used,
                budget,
                pins,
                online,
                pending_uploads,
                pending_changes,
                ..
            })) => {
                set_mounted(&ui, true);
                // The queue is the more useful thing to say when it has anything
                // in it: it is why a file that looks saved is not on the remote
                // yet, and offline is usually the reason it is still queued.
                let queued = pending_summary(pending_uploads, pending_changes);
                ui.mount_row.set_subtitle(&match (online, queued) {
                    (true, None) => format!("Mounted at {mountpoint}"),
                    (true, Some(q)) => format!("Mounted at {mountpoint} — {q}"),
                    (false, None) => {
                        format!("Mounted at {mountpoint} — offline, cached files only")
                    }
                    (false, Some(q)) => format!("Mounted at {mountpoint} — offline, {q}"),
                });
                let fraction = if budget == 0 {
                    0.0
                } else {
                    (used as f64 / budget as f64).min(1.0)
                };
                ui.cache_bar.set_fraction(fraction);
                ui.cache_label.set_text(&format!(
                    "{} of {} used",
                    human_bytes(used),
                    human_bytes(budget)
                ));
                repaint_pins(&ui, &pins, true);
            }
            // Daemon unreachable (still starting, or down): report not-mounted and
            // grey out the unpin buttons in place, but leave the last-known pin
            // rows and cache read-out so the page doesn't flicker on a blip.
            _ => {
                set_mounted(&ui, false);
                ui.mount_row.set_subtitle("Not mounted");
                for r in ui.pin_rows.borrow().iter() {
                    if let Some(b) = &r.unpin {
                        b.set_sensitive(false);
                    }
                }
            }
        }
    });
}

/// Render the pins group from `pins`, with the unpin buttons enabled only while a
/// mount daemon is running (`mounted`). Diffs against the last batch by path: when
/// the set is unchanged (the common case on the 2s tick) it only flips the unpin
/// buttons' `sensitive` flag, avoiding the rebuild that used to flicker the list
/// and drop scroll/selection every tick.
fn repaint_pins(ui: &Rc<Ui>, pins: &[pdfs_core::cache::Pin], mounted: bool) {
    let desired: Vec<String> = pins.iter().map(|p| p.path.clone()).collect();
    if ui.pins_state.borrow().as_ref() == Some(&desired) {
        for r in ui.pin_rows.borrow().iter() {
            if let Some(b) = &r.unpin {
                b.set_sensitive(mounted);
            }
        }
        return;
    }

    for pr in ui.pin_rows.borrow_mut().drain(..) {
        ui.pins_group.remove(&pr.row);
    }
    *ui.pins_state.borrow_mut() = Some(desired);

    if pins.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No pinned files")
            .subtitle("Right-click a file in the mount to keep it offline.")
            .build();
        ui.pins_group.add(&row);
        ui.pin_rows.borrow_mut().push(PinRow { row, unpin: None });
        return;
    }

    for pin in pins {
        let name = Path::new(&pin.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&pin.path)
            .to_string();
        let row = adw::ActionRow::builder()
            .title(&name)
            .subtitle(&pin.path)
            .build();
        let icon = gtk4::Image::from_icon_name("emblem-documents-symbolic");
        row.add_prefix(&icon);

        let unpin = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .valign(gtk4::Align::Center)
            .tooltip_text("Unpin (remove offline copy)")
            .sensitive(mounted)
            .build();
        unpin.add_css_class("flat");
        let ui_btn = ui.clone();
        let path = pin.path.clone();
        unpin.connect_clicked(move |_| {
            let socket = ui_btn.dirs.control_socket();
            match send(&socket, &Request::Unpin { path: path.clone() }) {
                Ok(Response::Error { message }) => tracing::error!("unpin failed: {message}"),
                Ok(_) => refresh(&ui_btn),
                Err(e) => tracing::error!("unpin request failed: {e}"),
            }
        });
        row.add_suffix(&unpin);

        ui.pins_group.add(&row);
        ui.pin_rows.borrow_mut().push(PinRow {
            row,
            unpin: Some(unpin),
        });
    }
}

/// Run one blocking control-socket round-trip on a worker thread, returning a
/// channel that yields the [`Response`] once. Browser/gallery requests reach the
/// network through the daemon, so they must not block the GTK main loop the way
/// the cheap [`Request::Status`] poll in [`refresh`] can.
fn spawn_request(
    socket: PathBuf,
    req: Request,
) -> async_channel::Receiver<Result<Response, String>> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = send(&socket, &req).map_err(|e| e.to_string());
        let _ = tx.send_blocking(result);
    });
    rx
}

/// The Files page: a Nautilus-style file manager. A back/breadcrumb header with
/// a grid/list view toggle sits over a [`gtk4::Stack`] that swaps between an
/// **icon grid** ([`gtk4::GridView`]) and a **column list** ([`gtk4::ColumnView`]
/// with Name / Size / Modified columns). Both views are driven by one shared
/// [`gio::ListStore`] of [`BoxedAnyObject`]-wrapped [`DirEntry`]s, so a directory
/// load repopulates the model once and both views update.
///
/// The factories that render entries — and the columns — need the [`Ui`] handle
/// for activation and the right-click menu, so they're installed later in
/// [`wire_browser`]; this builder only assembles the empty widgets.
///
/// Empty / loading / error outcomes aren't a label under the header: the whole
/// content area swaps to a centred [`adw::StatusPage`] (see [`browser_status`]),
/// so "this folder is empty" and "the mount is down" read as first-class states
/// rather than a stray line above a blank grid.
struct BrowserWidgets {
    model: gio::ListStore,
    back: gtk4::Button,
    crumb: gtk4::Box,
    grid: gtk4::GridView,
    column_view: gtk4::ColumnView,
    /// Swaps the content area between the grid/list views and the status page.
    content: gtk4::Stack,
    /// The empty/loading/error surface shown in place of the views.
    status: adw::StatusPage,
    /// Sits in the status page; shown only when the mount service is down.
    retry: gtk4::Button,
    search: gtk4::SearchEntry,
    new_folder: gtk4::Button,
    upload: gtk4::Button,
    upload_folder: gtk4::Button,
    refresh: gtk4::Button,
    /// Wraps the views + the details pane; the pane slides in on selection.
    split: adw::OverlaySplitView,
    details: DetailsWidgets,
    /// The two selection models, so a selection change can drive the details pane
    /// and so an action can re-read the entry the user has highlighted.
    grid_selection: gtk4::SingleSelection,
    list_selection: gtk4::SingleSelection,
}

fn build_browser_page() -> (gtk4::Widget, BrowserWidgets) {
    let model = gio::ListStore::new::<BoxedAnyObject>();

    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Up one folder")
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    back.add_css_class("flat");
    // Clickable breadcrumb trail; `repaint_crumb` fills it per load. Wrapped in a
    // horizontally-scrolling viewport so a deep path can't shove the search box
    // and view toggles off the right edge.
    let crumb = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    crumb.set_valign(gtk4::Align::Center);
    let crumb_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::External)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .hexpand(true)
        .child(&crumb)
        .build();

    // Folder-level actions: create a subfolder / upload a file into the current
    // directory. Wired in `wire_browser_actions`.
    let new_folder = gtk4::Button::builder()
        .icon_name("folder-new-symbolic")
        .tooltip_text("New folder")
        .valign(gtk4::Align::Center)
        .build();
    new_folder.add_css_class("flat");
    let upload = gtk4::Button::builder()
        .icon_name("document-send-symbolic")
        .tooltip_text("Upload files")
        .valign(gtk4::Align::Center)
        .build();
    upload.add_css_class("flat");
    let upload_folder = gtk4::Button::builder()
        .icon_name("pdfs-folder-upload-symbolic")
        .tooltip_text("Upload folder")
        .valign(gtk4::Align::Center)
        .build();
    upload_folder.add_css_class("flat");

    // Linked grid/list toggle, top-right, Nautilus-style.
    let grid_toggle = gtk4::ToggleButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("Grid view")
        .active(true)
        .build();
    let list_toggle = gtk4::ToggleButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("List view")
        .build();
    list_toggle.set_group(Some(&grid_toggle));
    let toggles = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    toggles.add_css_class("linked");
    toggles.append(&grid_toggle);
    toggles.append(&list_toggle);

    let search = gtk4::SearchEntry::builder()
        .placeholder_text("Search Drive")
        .valign(gtk4::Align::Center)
        .build();
    search.set_width_chars(18);

    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&back);
    header.append(&crumb_scroll);
    header.append(&refresh);
    header.append(&new_folder);
    header.append(&upload);
    header.append(&upload_folder);
    header.append(&search);
    header.append(&toggles);

    // Empty / loading / error surface, shown in place of the views.
    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("folder-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    // Icon grid.
    let grid_selection = gtk4::SingleSelection::builder()
        .model(&model)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let grid = gtk4::GridView::builder()
        .model(&grid_selection)
        .min_columns(2)
        .max_columns(10)
        .build();
    grid.add_css_class("file-grid");
    let grid_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&grid)
        .build();

    // Column list.
    let list_selection = gtk4::SingleSelection::builder()
        .model(&model)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let column_view = gtk4::ColumnView::builder().model(&list_selection).build();
    column_view.add_css_class("data-table");
    let column_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&column_view)
        .build();

    // Stack swapped by the toggle buttons.
    let view_stack = gtk4::Stack::new();
    view_stack.set_vexpand(true);
    view_stack.add_named(&grid_scroll, Some("grid"));
    view_stack.add_named(&column_scroll, Some("list"));
    let vs = view_stack.clone();
    grid_toggle.connect_toggled(move |b| {
        if b.is_active() {
            vs.set_visible_child_name("grid");
        }
    });
    let vs = view_stack.clone();
    list_toggle.connect_toggled(move |b| {
        if b.is_active() {
            vs.set_visible_child_name("list");
        }
    });

    // Outer stack: the views, or the status page when there's nothing to show.
    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&view_stack, Some("views"));
    content.add_named(&status, Some("status"));

    // The details pane slides in from the right when an entry is selected.
    let (details_pane, details) = build_details_pane();
    let split = adw::OverlaySplitView::builder()
        .sidebar_position(gtk4::PackType::End)
        .collapsed(true)
        .show_sidebar(false)
        .max_sidebar_width(300.0)
        .content(&content)
        .sidebar(&details_pane)
        .build();

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&split);

    (
        inner.upcast(),
        BrowserWidgets {
            model,
            back,
            crumb,
            grid,
            column_view,
            content,
            status,
            retry,
            search,
            new_folder,
            upload,
            upload_folder,
            refresh,
            split,
            details,
            grid_selection,
            list_selection,
        },
    )
}

/// The widgets in the browser's details pane that a selection repaints.
struct DetailsWidgets {
    icon: gtk4::Image,
    name: gtk4::Label,
    kind: gtk4::Label,
    size_row: adw::ActionRow,
    modified_row: adw::ActionRow,
    path_row: adw::ActionRow,
    pin_row: adw::SwitchRow,
    open_button: gtk4::Button,
    rename_button: gtk4::Button,
    trash_button: gtk4::Button,
    close_button: gtk4::Button,
}

/// The details pane shown beside the file views: a big type icon over the entry's
/// name, its properties, an offline (pin) toggle and the primary actions. Built
/// empty; [`repaint_details`] fills it from the selected [`DirEntry`] and
/// [`wire_details`] connects the buttons.
fn build_details_pane() -> (gtk4::Widget, DetailsWidgets) {
    let close_button = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Close details")
        .halign(gtk4::Align::End)
        .build();
    close_button.add_css_class("flat");
    close_button.add_css_class("circular");

    let icon = gtk4::Image::builder()
        .icon_name("text-x-generic-symbolic")
        .pixel_size(64)
        .build();
    let name = gtk4::Label::builder()
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .justify(gtk4::Justification::Center)
        .build();
    name.add_css_class("title-4");
    let kind = gtk4::Label::new(None);
    kind.add_css_class("dim-label");
    kind.add_css_class("caption");

    let head = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    head.set_margin_bottom(6);
    head.append(&icon);
    head.append(&name);
    head.append(&kind);

    let props = adw::PreferencesGroup::new();
    let size_row = adw::ActionRow::builder()
        .title("Size")
        .subtitle("—")
        .build();
    size_row.add_css_class("property");
    let modified_row = adw::ActionRow::builder()
        .title("Modified")
        .subtitle("—")
        .build();
    modified_row.add_css_class("property");
    let path_row = adw::ActionRow::builder()
        .title("Location")
        .subtitle("—")
        .subtitle_lines(3)
        .build();
    path_row.add_css_class("property");
    props.add(&size_row);
    props.add(&modified_row);
    props.add(&path_row);

    let offline = adw::PreferencesGroup::new();
    let pin_row = adw::SwitchRow::builder()
        .title("Available offline")
        .subtitle("Keep a local copy on this device.")
        .build();
    offline.add(&pin_row);

    let open_button = gtk4::Button::builder().label("Open").build();
    open_button.add_css_class("suggested-action");
    open_button.add_css_class("pill");
    let rename_button = gtk4::Button::builder().label("Rename").build();
    rename_button.add_css_class("pill");
    let trash_button = gtk4::Button::builder().label("Move to Trash").build();
    trash_button.add_css_class("destructive-action");
    trash_button.add_css_class("pill");

    let actions = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    actions.set_margin_top(6);
    actions.append(&open_button);
    actions.append(&rename_button);
    actions.append(&trash_button);

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&close_button);
    inner.append(&head);
    inner.append(&props);
    inner.append(&offline);
    inner.append(&actions);

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&inner)
        .build();
    let pane = adw::ToolbarView::new();
    pane.set_content(Some(&scroll));
    pane.add_css_class("background");

    (
        pane.upcast(),
        DetailsWidgets {
            icon,
            name,
            kind,
            size_row,
            modified_row,
            path_row,
            pin_row,
            open_button,
            rename_button,
            trash_button,
            close_button,
        },
    )
}

/// Connect the details pane: mirror the grid's and list's selection into it, and
/// wire its buttons back onto the entry it's showing.
fn wire_details(ui: &Rc<Ui>) {
    // Both views share one model but have their own selection, so watch both.
    for selection in [ui.grid_selection.clone(), ui.list_selection.clone()] {
        let ui_sel = ui.clone();
        selection.connect_selection_changed(move |sel, _, _| {
            match entry_at(sel.model().as_ref(), sel.selected()) {
                Some(entry) => show_details(&ui_sel, &entry),
                None => hide_details(&ui_sel),
            }
        });
    }

    let ui_close = ui.clone();
    ui.details.close_button.connect_clicked(move |_| {
        ui_close
            .grid_selection
            .set_selected(gtk4::INVALID_LIST_POSITION);
        ui_close
            .list_selection
            .set_selected(gtk4::INVALID_LIST_POSITION);
        hide_details(&ui_close);
    });

    let ui_open = ui.clone();
    ui.details.open_button.connect_clicked(move |_| {
        // Bind the clone to a local so the `Ref` from `borrow()` is dropped before
        // `activate_entry` runs; it navigates and repaints the details pane, which
        // takes `details_entry.borrow_mut()` and would panic against a live borrow.
        let entry = ui_open.details_entry.borrow().clone();
        if let Some(entry) = entry {
            activate_entry(&ui_open, &entry);
        }
    });
    let ui_rename = ui.clone();
    ui.details.rename_button.connect_clicked(move |_| {
        let entry = ui_rename.details_entry.borrow().clone();
        if let Some(entry) = entry {
            prompt_rename(&ui_rename, &entry);
        }
    });
    let ui_trash = ui.clone();
    ui.details.trash_button.connect_clicked(move |_| {
        let entry = ui_trash.details_entry.borrow().clone();
        if let Some(entry) = entry {
            prompt_delete(&ui_trash, &entry);
        }
    });
    let ui_pin = ui.clone();
    ui.details.pin_row.connect_active_notify(move |row| {
        if ui_pin.details_suppress.get() {
            return;
        }
        let Some(entry) = ui_pin.details_entry.borrow().clone() else {
            return;
        };
        // The switch reads the *desired* state; `toggle_pin` derives the request
        // from the entry's current one, so only act when they actually differ.
        if row.is_active() != entry.pinned {
            toggle_pin(&ui_pin, &entry);
        }
    });
}

/// Reveal the details pane and paint it from `entry`.
fn show_details(ui: &Rc<Ui>, entry: &DirEntry) {
    ui.details_suppress.set(true);
    let d = &ui.details;
    d.icon.set_icon_name(Some(icon_base_for(entry)));
    d.name.set_label(&entry.name);
    d.kind
        .set_label(if entry.is_dir { "Folder" } else { "File" });
    d.size_row.set_subtitle(&if entry.is_dir {
        "—".to_string()
    } else {
        human_bytes(entry.size)
    });
    d.size_row.set_visible(!entry.is_dir);
    d.modified_row
        .set_subtitle(&format_modified(entry.modified));
    let rel = entry_rel(ui, entry);
    let parent = match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    };
    d.path_row.set_subtitle(if parent.is_empty() {
        "Proton Drive"
    } else {
        parent
    });
    d.pin_row.set_active(entry.pinned);
    d.pin_row.set_sensitive(*ui.mounted.borrow());
    d.open_button
        .set_label(if entry.is_dir { "Open folder" } else { "Open" });
    ui.details_suppress.set(false);

    *ui.details_entry.borrow_mut() = Some(entry.clone());
    // Reveal the pane on idle, not inline. This runs from `selection_changed`,
    // which fires on the *first* press of a double-click; mutating the widget
    // tree here cancels GtkGridView's multi-press tracking, so the second press
    // restarts the count and `activate` never fires — the folder needs two
    // double-clicks. Deferring lets the click gesture finish first. Guard on the
    // entry still being present so a navigation that clears it wins the race.
    let ui = ui.clone();
    glib::idle_add_local_once(move || {
        if ui.details_entry.borrow().is_some() {
            ui.browser_split.set_show_sidebar(true);
        }
    });
}

/// Hide the details pane and forget the entry it was showing, so a stale entry
/// can't be acted on after the listing moves on.
fn hide_details(ui: &Rc<Ui>) {
    ui.browser_split.set_show_sidebar(false);
    *ui.details_entry.borrow_mut() = None;
}

/// The entry highlighted in whichever browser view is on screen, if any. Backs the
/// F2 / Delete shortcuts.
fn selected_entry(ui: &Rc<Ui>) -> Option<DirEntry> {
    ui.details_entry.borrow().clone()
}

/// Swap the Files content area to the status page, with a Retry button only when
/// the failure is one the user can act on.
fn browser_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.browser_status.set_icon_name(Some(icon));
    ui.browser_status.set_title(title);
    ui.browser_status.set_description(Some(description));
    ui.browser_retry.set_visible(retry);
    ui.browser_content.set_visible_child_name("status");
    hide_details(ui);
}

/// Swap the Files content area back to the grid/list views.
fn browser_views(ui: &Rc<Ui>) {
    ui.browser_content.set_visible_child_name("views");
}

/// Install the entry factories, columns, activation handlers and the back
/// button. Split out from [`build_browser_page`] because every renderer needs
/// the [`Ui`] handle to open entries and raise the context menu.
fn wire_browser(ui: &Rc<Ui>, grid: &gtk4::GridView, column_view: &gtk4::ColumnView) {
    // Back: pop one path segment and reload.
    let ui_back = ui.clone();
    ui.browser_back.clone().connect_clicked(move |_| {
        {
            let mut path = ui_back.browser_path.borrow_mut();
            *path = match path.rfind('/') {
                Some(i) => path[..i].to_string(),
                None => String::new(),
            };
        }
        load_browser(&ui_back);
    });

    // Grid tiles: a big icon over an ellipsized name, with a right-click menu.
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let icon = gtk4::Image::builder().pixel_size(64).build();
            // Corner sync-state badge, overlaid on the big icon.
            let badge = gtk4::Image::builder()
                .pixel_size(18)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::End)
                .build();
            badge.add_css_class("file-badge");
            let overlay = gtk4::Overlay::new();
            overlay.set_child(Some(&icon));
            overlay.add_overlay(&badge);
            let label = gtk4::Label::builder()
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .justify(gtk4::Justification::Center)
                .max_width_chars(13)
                .width_chars(13)
                .wrap(true)
                .lines(2)
                .build();
            let tile = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            tile.add_css_class("file-tile");
            tile.append(&overlay);
            tile.append(&label);
            attach_context_menu(&ui, item, &tile);
            attach_drag(&ui, item, &tile);
            attach_drop(&ui, item, &tile);
            item.set_child(Some(&tile));
        }
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let tile = item.child().and_downcast::<gtk4::Box>().unwrap();
        let overlay = tile.first_child().and_downcast::<gtk4::Overlay>().unwrap();
        let icon = overlay.first_child().and_downcast::<gtk4::Image>().unwrap();
        let badge = overlay.last_child().and_downcast::<gtk4::Image>().unwrap();
        let label = tile.last_child().and_downcast::<gtk4::Label>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        icon.set_icon_name(Some(icon_base_for(&entry)));
        label.set_label(&entry.name);
        apply_badge(&badge, &entry);
    });
    grid.set_factory(Some(&factory));

    let ui_grid = ui.clone();
    grid.connect_activate(move |grid, pos| {
        if let Some(entry) = entry_at(grid.model().as_ref(), pos) {
            activate_entry(&ui_grid, &entry);
        }
    });

    // Column list: Name (icon + label, right-clickable), Size, Modified.
    column_view.append_column(&name_column(ui));
    column_view.append_column(&text_column("Size", |e| {
        if e.is_dir {
            "—".to_string()
        } else {
            human_bytes(e.size)
        }
    }));
    column_view.append_column(&text_column("Modified", |e| format_modified(e.modified)));

    let ui_col = ui.clone();
    column_view.connect_activate(move |view, pos| {
        if let Some(entry) = entry_at(view.model().as_ref(), pos) {
            activate_entry(&ui_col, &entry);
        }
    });
}

/// Build the Name column: a small icon plus the (star-prefixed when pinned) name,
/// with the same right-click menu the grid tiles carry.
fn name_column(ui: &Rc<Ui>) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let icon = gtk4::Image::builder().pixel_size(16).build();
            let label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
            let badge = gtk4::Image::builder().pixel_size(14).build();
            badge.add_css_class("file-badge");
            let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            cell.append(&icon);
            cell.append(&label);
            cell.append(&badge);
            attach_context_menu(&ui, item, &cell);
            attach_drag(&ui, item, &cell);
            attach_drop(&ui, item, &cell);
            item.set_child(Some(&cell));
        }
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let cell = item.child().and_downcast::<gtk4::Box>().unwrap();
        let icon = cell.first_child().and_downcast::<gtk4::Image>().unwrap();
        let badge = cell.last_child().and_downcast::<gtk4::Image>().unwrap();
        let label = icon.next_sibling().and_downcast::<gtk4::Label>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        icon.set_icon_name(Some(&format!("{}-symbolic", icon_base_for(&entry))));
        label.set_label(&entry.name);
        apply_badge(&badge, &entry);
    });
    let column = gtk4::ColumnViewColumn::new(Some("Name"), Some(factory));
    column.set_expand(true);
    column
}

/// Build a trailing text column whose cell text is derived from each [`DirEntry`]
/// by `render`.
fn text_column(
    title: &str,
    render: impl Fn(&DirEntry) -> String + 'static,
) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
        label.add_css_class("dim-label");
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = item.child().and_downcast::<gtk4::Label>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        label.set_label(&render(&entry));
    });
    gtk4::ColumnViewColumn::new(Some(title), Some(factory))
}

/// Attach a secondary-button [`gtk4::GestureClick`] to a cell that pops a context
/// menu for whatever entry the owning `item` is currently bound to. Capturing the
/// [`gtk4::ListItem`] (rather than a snapshot of the entry) keeps the menu correct
/// as the view recycles cells while scrolling.
fn attach_context_menu(ui: &Rc<Ui>, item: &gtk4::ListItem, anchor: &gtk4::Box) {
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
    let ui = ui.clone();
    let item = item.clone();
    let target = anchor.clone();
    gesture.connect_pressed(move |_, _, x, y| {
        if let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() {
            let entry = obj.borrow::<DirEntry>().clone();
            show_context_menu(&ui, &entry, &target, x, y);
        }
    });
    anchor.add_controller(gesture);
}

/// Pop a context menu next to `anchor` at the click point, offering Open and a
/// Pin/Unpin toggle (files only). Built fresh per click because the items are
/// entry-specific; it unparents itself once dismissed.
fn show_context_menu(ui: &Rc<Ui>, entry: &DirEntry, anchor: &gtk4::Box, x: f64, y: f64) {
    let menu = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let popover = gtk4::Popover::builder()
        .has_arrow(false)
        .position(gtk4::PositionType::Bottom)
        .pointing_to(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1))
        .child(&menu)
        .build();
    popover.set_parent(anchor);
    popover.connect_closed(|p| p.unparent());

    // Videos lead with Play (stream from the mount, no download); the plain Open
    // below still downloads a local copy for anyone who wants one.
    if is_video_entry(entry) {
        let play = menu_item("Play (stream)", "media-playback-start-symbolic");
        let ui_play = ui.clone();
        let entry_play = entry.clone();
        let pop = popover.clone();
        play.connect_clicked(move |_| {
            pop.popdown();
            stream_entry(&ui_play, &entry_play);
        });
        menu.append(&play);
    }

    let open = menu_item("Open", "document-open-symbolic");
    let ui_open = ui.clone();
    let entry_open = entry.clone();
    let pop = popover.clone();
    open.connect_clicked(move |_| {
        pop.popdown();
        // Open always means "download a local copy and hand off", even for a
        // video — `activate_entry` would otherwise stream it.
        if is_video_entry(&entry_open) {
            download_and_open(&ui_open, &entry_open);
        } else {
            activate_entry(&ui_open, &entry_open);
        }
    });
    menu.append(&open);

    if !entry.is_dir {
        let (label, icon) = if entry.pinned {
            ("Unpin", "non-starred-symbolic")
        } else {
            ("Keep offline", "starred-symbolic")
        };
        let pin = menu_item(label, icon);
        let ui_pin = ui.clone();
        let entry_pin = entry.clone();
        let pop = popover.clone();
        pin.connect_clicked(move |_| {
            pop.popdown();
            toggle_pin(&ui_pin, &entry_pin);
        });
        menu.append(&pin);
    }

    menu.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let rename = menu_item("Rename…", "document-edit-symbolic");
    let ui_rn = ui.clone();
    let entry_rn = entry.clone();
    let pop = popover.clone();
    rename.connect_clicked(move |_| {
        pop.popdown();
        prompt_rename(&ui_rn, &entry_rn);
    });
    menu.append(&rename);

    let move_it = menu_item("Move…", "folder-move-symbolic");
    let ui_mv = ui.clone();
    let entry_mv = entry.clone();
    let pop = popover.clone();
    move_it.connect_clicked(move |_| {
        pop.popdown();
        prompt_move(&ui_mv, &entry_mv);
    });
    menu.append(&move_it);

    menu.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let share = menu_item("Share…", "emblem-shared-symbolic");
    let ui_sh = ui.clone();
    let entry_sh = entry.clone();
    let pop = popover.clone();
    share.connect_clicked(move |_| {
        pop.popdown();
        open_share_dialog(&ui_sh, &entry_sh);
    });
    menu.append(&share);

    let trash = menu_item("Move to Trash", "user-trash-symbolic");
    let ui_tr = ui.clone();
    let entry_tr = entry.clone();
    let pop = popover.clone();
    trash.connect_clicked(move |_| {
        pop.popdown();
        prompt_delete(&ui_tr, &entry_tr);
    });
    menu.append(&trash);

    popover.popup();
}

/// A left-aligned, flat icon+label button for the context menu.
fn menu_item(label: &str, icon: &str) -> gtk4::Button {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.append(&gtk4::Image::from_icon_name(icon));
    row.append(
        &gtk4::Label::builder()
            .label(label)
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .build(),
    );
    let button = gtk4::Button::builder().child(&row).build();
    button.add_css_class("flat");
    button
}

/// Fetch the [`DirEntry`] backing the model item at `pos`, if any.
fn entry_at(model: Option<&impl IsA<gio::ListModel>>, pos: u32) -> Option<DirEntry> {
    let obj = model?.item(pos).and_downcast::<BoxedAnyObject>()?;
    let entry = obj.borrow::<DirEntry>().clone();
    Some(entry)
}

/// Whether an entry's name marks it as a video, reusing the same classifier the
/// Photos page splits on — so the file browser and the gallery agree on what a
/// video is.
fn is_video_entry(entry: &DirEntry) -> bool {
    !entry.is_dir && PhotoKind::classify(Some(&entry.name), None) == PhotoKind::Video
}

/// Stream a video straight from the mount, no download. Drive folders *are* part
/// of the FUSE mount, so a player pointed at `<mountpoint>/<rel>` reads the file
/// through [`Core::read_range_remote`] — 4 MB blocks fetched on demand as it
/// seeks and buffers — instead of waiting for the whole file to land like
/// [`Request::OpenFile`] does. This is the point of the feature: a 2 GB HEVC
/// `.mkv` starts playing in seconds.
fn stream_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let mountpoint = ui.dirs.resolved_mountpoint(&ui.dirs.load_config());
    let abs = mountpoint.join(&rel);
    let Some(path) = abs.to_str() else {
        toast_error(ui, "Couldn't play video", "The file path isn't valid UTF-8.");
        return;
    };
    toast(ui, &format!("Streaming “{}”…", entry.name));
    play_external(path);
}

/// Open an entry the Nautilus way: folders descend, videos stream from the mount,
/// other files download-and-open.
fn activate_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    if entry.is_dir {
        // Descending into a search hit: clear the query so the folder listing
        // isn't immediately re-masked by a stale search.
        if !entry.path.is_empty() {
            ui.browser_search.set_text("");
        }
        *ui.browser_path.borrow_mut() = rel;
        load_browser(ui);
    } else if is_video_entry(entry) {
        // A video streams rather than downloads: that is exactly the "play it,
        // don't fetch the whole thing" behaviour this is for.
        stream_entry(ui, entry);
    } else {
        download_and_open(ui, entry);
    }
}

/// Download a file's full content into the cache and hand it to the user's
/// default application. The download-and-open path behind both a plain
/// double-click and the context menu's "Open" (including for a video, when the
/// user explicitly wants a local copy rather than to stream it).
fn download_and_open(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    // Ignore a repeat activation of a file already downloading, so an impatient
    // double-click doesn't kick off a second round-trip.
    if !ui.opening.borrow_mut().insert(rel.clone()) {
        return;
    }
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenFile { path: rel.clone() },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.opening.borrow_mut().remove(&rel);
        match result {
            Ok(Ok(Response::FilePath { path })) => open_path(&path),
            Ok(Ok(Response::Error { message })) => {
                toast_error(&ui, "Couldn't open file", &message)
            }
            _ => toast_error(
                &ui,
                "Couldn't open file",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Pin or unpin an entry through the daemon, then reload to reflect the new
/// state.
fn toggle_pin(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let req = if entry.pinned {
        Request::Unpin { path: rel }
    } else {
        Request::Pin { path: rel }
    };
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let name = entry.name.clone();
    let pinned = entry.pinned;
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Error { message })) => {
                toast_error(&ui, "Couldn't change offline state", &message);
                // The details switch may be showing the state we failed to reach.
                load_browser(&ui);
            }
            Ok(Ok(_)) => {
                load_browser(&ui);
                toast(
                    &ui,
                    &if pinned {
                        format!("“{name}” is no longer kept offline")
                    } else {
                        format!("“{name}” is now available offline")
                    },
                );
            }
            _ => {
                toast_error(
                    &ui,
                    "Couldn't change offline state",
                    "The mount service didn't respond.",
                );
                load_browser(&ui);
            }
        }
    });
}

/// Join the entry name onto the current browser directory to get its
/// mountpoint-relative path.
fn entry_rel(ui: &Rc<Ui>, entry: &DirEntry) -> String {
    // Search hits carry an absolute (mountpoint-relative) path since they can
    // live anywhere; plain listing entries derive it from the current folder.
    if !entry.path.is_empty() {
        return entry.path.clone();
    }
    let base = ui.browser_path.borrow();
    if base.is_empty() {
        entry.name.clone()
    } else {
        format!("{base}/{}", entry.name)
    }
}

/// Rebuild the clickable breadcrumb trail for the mountpoint-relative `path`. The
/// root is always present ("Proton Drive"); each segment becomes a flat button
/// that navigates to that ancestor, except the last (the current folder), shown
/// as a plain heading label.
fn repaint_crumb(ui: &Rc<Ui>, path: &str) {
    while let Some(child) = ui.browser_crumb.first_child() {
        ui.browser_crumb.remove(&child);
    }
    let segments: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    };
    ui.browser_crumb
        .append(&crumb_node(ui, "Proton Drive", "", segments.is_empty()));
    let mut acc = String::new();
    for (i, seg) in segments.iter().enumerate() {
        let sep = gtk4::Label::new(Some("›"));
        sep.add_css_class("dim-label");
        ui.browser_crumb.append(&sep);
        acc = if acc.is_empty() {
            seg.to_string()
        } else {
            format!("{acc}/{seg}")
        };
        let current = i == segments.len() - 1;
        ui.browser_crumb.append(&crumb_node(ui, seg, &acc, current));
    }
}

/// One breadcrumb segment: a plain heading label for the current folder, or a
/// flat button that navigates to `target` (clearing any active search first).
fn crumb_node(ui: &Rc<Ui>, label: &str, target: &str, current: bool) -> gtk4::Widget {
    if current {
        let l = gtk4::Label::builder()
            .label(label)
            .ellipsize(gtk4::pango::EllipsizeMode::Start)
            .build();
        l.add_css_class("heading");
        return l.upcast();
    }
    let button = gtk4::Button::builder().label(label).build();
    button.add_css_class("flat");
    let ui = ui.clone();
    let target = target.to_string();
    button.connect_clicked(move |_| {
        ui.browser_search.set_text("");
        *ui.browser_path.borrow_mut() = target.clone();
        load_browser(&ui);
    });
    button.upcast()
}

/// Wire the browser header's New-folder, Upload-files and Upload-folder buttons.
fn wire_browser_actions(
    ui: &Rc<Ui>,
    new_folder: &gtk4::Button,
    upload: &gtk4::Button,
    upload_folder: &gtk4::Button,
) {
    let ui_nf = ui.clone();
    new_folder.connect_clicked(move |_| prompt_new_folder(&ui_nf));
    let ui_up = ui.clone();
    upload.connect_clicked(move |_| prompt_upload(&ui_up));
    let ui_uf = ui.clone();
    upload_folder.connect_clicked(move |_| prompt_upload_folder(&ui_uf));
}

/// Send a mutating request (rename / move / delete / mkdir / upload, or a trash
/// restore / purge) on a worker thread, then reload the listing it changed and
/// confirm with a toast, or report the daemon's error in one. `done` is the
/// past-tense confirmation ("Renamed to “x”"); `failed` names the attempt
/// ("Couldn't rename").
fn run_mutation(ui: &Rc<Ui>, req: Request, done: String, failed: &'static str) {
    if !*ui.mounted.borrow() {
        toast_error(ui, failed, "Proton Drive isn't connected.");
        return;
    }
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => {
                // The listing the mutation changed is stale now; reload it, then
                // confirm, so the toast lands over the updated view.
                reload_listing(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message })) => toast_error(&ui, failed, &message),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}

/// Reload the listing a completed mutation invalidated: whichever of the two
/// listing pages is on screen, since that is the one the action was raised from.
fn reload_listing(ui: &Rc<Ui>) {
    match ui.stack.visible_child_name().as_deref() {
        Some("browser") => load_browser(ui),
        Some("trash") => load_trash(ui),
        _ => {}
    }
}

/// Prompt for a new name and rename the entry through the daemon.
fn prompt_rename(ui: &Rc<Ui>, entry: &DirEntry) {
    let parent = ui_window(ui);
    let rel = entry_rel(ui, entry);
    let original = entry.name.clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Rename")
        .body(format!("Rename “{original}”."))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("New name")
        .activates_default(true)
        .build();
    row.set_text(&original);
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Rename");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let new_name = row.text().trim().to_string();
        if new_name.is_empty() || new_name == original {
            return;
        }
        let done = format!("Renamed to “{new_name}”");
        run_mutation(
            &ui,
            Request::Rename {
                path: rel.clone(),
                new_name,
            },
            done,
            "Couldn't rename",
        );
    });
    dialog.present(parent.as_ref());
}

/// Prompt for a destination folder (mountpoint-relative, empty = Drive root) and
/// move the entry there through the daemon.
fn prompt_move(ui: &Rc<Ui>, entry: &DirEntry) {
    let parent = ui_window(ui);
    let rel = entry_rel(ui, entry);
    let name = entry.name.clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Move")
        .body(format!(
            "Move “{}” into another folder. Enter its path from the Drive root \
             (leave blank for the root).",
            entry.name
        ))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("Destination folder")
        .activates_default(true)
        .build();
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Move");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let new_parent = row.text().trim().trim_matches('/').to_string();
        let done = match new_parent.as_str() {
            "" => format!("Moved “{name}” to Proton Drive"),
            dest => format!("Moved “{name}” to “{dest}”"),
        };
        run_mutation(
            &ui,
            Request::Move {
                path: rel.clone(),
                new_parent,
            },
            done,
            "Couldn't move",
        );
    });
    dialog.present(parent.as_ref());
}

/// Confirm and move the entry to Trash through the daemon.
fn prompt_delete(ui: &Rc<Ui>, entry: &DirEntry) {
    let win = ui_window(ui);
    let rel = entry_rel(ui, entry);
    let name = entry.name.clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Move to Trash")
        .body(format!("Move “{}” to Trash?", entry.name))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("trash", "Move to Trash");
    dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "trash" {
            run_mutation(
                &ui,
                Request::Delete { path: rel.clone() },
                format!("Moved “{name}” to Trash"),
                "Couldn't move to Trash",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Prompt for a folder name and create it under the current browser directory.
fn prompt_new_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let parent = ui.browser_path.borrow().clone();
    let dialog = adw::AlertDialog::builder()
        .heading("New folder")
        .body("Create a folder in the current directory.")
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("Folder name")
        .activates_default(true)
        .build();
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Create");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let name = row.text().trim().to_string();
        if name.is_empty() {
            return;
        }
        let done = format!("Created “{name}”");
        run_mutation(
            &ui,
            Request::CreateFolder {
                parent: parent.clone(),
                name,
            },
            done,
            "Couldn't create folder",
        );
    });
    dialog.present(win.as_ref());
}

/// Pick one or more local files and upload them into the current browser
/// directory. The daemon streams them from disk itself, so nothing is read into
/// the GUI — even a large multi-file selection.
fn prompt_upload(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = gtk4::FileDialog::builder().title("Upload Files").build();
    let ui = ui.clone();
    dialog.open_multiple(win.as_ref(), gio::Cancellable::NONE, move |res| {
        let Ok(files) = res else { return };
        let sources: Vec<String> = files
            .into_iter()
            .filter_map(|f| f.ok())
            .filter_map(|obj| obj.downcast::<gio::File>().ok())
            .filter_map(|f| f.path())
            .filter_map(|p| p.to_str().map(str::to_string))
            .collect();
        start_upload(&ui, sources);
    });
}

/// Pick a local folder and upload it — with its whole subtree — into the current
/// browser directory. The daemon recreates the directory structure remotely.
fn prompt_upload_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = gtk4::FileDialog::builder().title("Upload Folder").build();
    let ui = ui.clone();
    dialog.select_folder(win.as_ref(), gio::Cancellable::NONE, move |res| {
        let Ok(folder) = res else { return };
        let Some(path) = folder.path().and_then(|p| p.to_str().map(str::to_string)) else {
            return;
        };
        start_upload(&ui, vec![path]);
    });
}

/// Hand a set of local source paths to the daemon for background bulk upload.
/// The daemon acks at once and does the work off-socket, so we confirm the
/// hand-off with a toast; the Activity group then shows live progress and the
/// listing refreshes itself when the transfers finish (see [`repaint_transfers`]).
fn start_upload(ui: &Rc<Ui>, sources: Vec<String>) {
    if sources.is_empty() {
        return;
    }
    if !*ui.mounted.borrow() {
        toast_error(ui, "Couldn't upload", "Proton Drive isn't connected.");
        return;
    }
    let parent = ui.browser_path.borrow().clone();
    let n = sources.len();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::UploadPaths { parent, sources },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => {
                let what = if n == 1 {
                    "Uploading…".to_string()
                } else {
                    format!("Uploading {n} items…")
                };
                toast(&ui, &what);
            }
            Ok(Ok(Response::Error { message })) => toast_error(&ui, "Couldn't upload", &message),
            _ => toast_error(&ui, "Couldn't upload", "The mount service didn't respond."),
        }
    });
}

/// The top-level window, for parenting dialogs.
fn ui_window(ui: &Rc<Ui>) -> Option<gtk4::Window> {
    ui.stack.root().and_downcast::<gtk4::Window>()
}

/// The sync-state badge for an entry: `(icon, css-class)`, or `None` for folders
/// (which carry no per-file cache state). Pinned (kept offline) ranks above merely
/// cached (downloaded, evictable); everything else is online-only.
fn badge_for(entry: &DirEntry) -> Option<(&'static str, &'static str)> {
    if entry.is_dir {
        return None;
    }
    Some(if entry.pinned {
        ("starred-symbolic", "badge-pinned")
    } else if entry.cached {
        ("emblem-ok-symbolic", "badge-cached")
    } else {
        ("weather-overcast-symbolic", "badge-cloud")
    })
}

/// Paint `badge` to reflect the entry's sync state (see [`badge_for`]). Clears any
/// prior colour class first, since list factories recycle cells.
fn apply_badge(badge: &gtk4::Image, entry: &DirEntry) {
    for class in ["badge-pinned", "badge-cached", "badge-cloud"] {
        badge.remove_css_class(class);
    }
    match badge_for(entry) {
        Some((icon, class)) => {
            badge.set_icon_name(Some(icon));
            badge.add_css_class(class);
            badge.set_visible(true);
        }
        None => badge.set_visible(false),
    }
}

/// Make a browser cell draggable, carrying the bound entry's mountpoint-relative
/// path as the drag payload. Reads the entry live at drag time (via the captured
/// [`gtk4::ListItem`]) so a recycled cell drags whatever it currently shows.
fn attach_drag(ui: &Rc<Ui>, item: &gtk4::ListItem, anchor: &gtk4::Box) {
    let source = gtk4::DragSource::new();
    source.set_actions(gtk4::gdk::DragAction::MOVE);
    let ui = ui.clone();
    let item = item.clone();
    source.connect_prepare(move |_, _, _| {
        let obj = item.item().and_downcast::<BoxedAnyObject>()?;
        let rel = entry_rel(&ui, &obj.borrow::<DirEntry>());
        Some(gtk4::gdk::ContentProvider::for_value(&glib::Value::from(
            rel.as_str(),
        )))
    });
    anchor.add_controller(source);
}

/// Make a browser cell a drop target: dropping a dragged path onto a *folder* cell
/// moves the source into it through the daemon. Drops onto files, onto the item
/// itself, or that would move a folder into its own subtree are rejected.
fn attach_drop(ui: &Rc<Ui>, item: &gtk4::ListItem, anchor: &gtk4::Box) {
    let target = gtk4::DropTarget::new(glib::types::Type::STRING, gtk4::gdk::DragAction::MOVE);
    let ui = ui.clone();
    let item = item.clone();
    target.connect_drop(move |_, value, _, _| {
        let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() else {
            return false;
        };
        let dest = obj.borrow::<DirEntry>();
        if !dest.is_dir {
            return false;
        }
        let Ok(src) = value.get::<String>() else {
            return false;
        };
        let dest_path = entry_rel(&ui, &dest);
        // No-op onto self, and never move a folder into itself or a descendant.
        if src == dest_path || dest_path.starts_with(&format!("{src}/")) {
            return false;
        }
        let done = format!("Moved into “{}”", dest.name);
        run_mutation(
            &ui,
            Request::Move {
                path: src,
                new_parent: dest_path,
            },
            done,
            "Couldn't move",
        );
        true
    });
    anchor.add_controller(target);
}

/// Pick a freedesktop icon base name for an entry from its kind / extension.
/// Callers append `-symbolic` for the column view's small icons.
fn icon_base_for(entry: &DirEntry) -> &'static str {
    if entry.is_dir {
        return "folder";
    }
    let ext = entry
        .name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "heic" | "tiff" => {
            "image-x-generic"
        }
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => "video-x-generic",
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" => "audio-x-generic",
        "pdf" | "doc" | "docx" | "odt" => "x-office-document",
        "xls" | "xlsx" | "ods" | "csv" => "x-office-spreadsheet",
        "ppt" | "pptx" | "odp" => "x-office-presentation",
        "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" => "package-x-generic",
        _ => "text-x-generic",
    }
}

/// Format an epoch-seconds modification time as a short local date.
fn format_modified(secs: i64) -> String {
    match glib::DateTime::from_unix_local(secs) {
        Ok(dt) => dt
            .format("%-d %b %Y")
            .map(|s| s.to_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// Request the current browser directory from the daemon and repaint both views.
fn load_browser(ui: &Rc<Ui>) {
    let path = ui.browser_path.borrow().clone();
    repaint_crumb(ui, &path);
    ui.browser_back.set_sensitive(!path.is_empty());

    // Drop the previous folder's rows up front: a slow reply must not leave stale
    // entries visible, where clicking one would open with a wrong relative path.
    ui.browser_model.remove_all();
    browser_status(
        ui,
        "folder-symbolic",
        "Loading…",
        "Reading this folder.",
        false,
    );

    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListDir { path: path.clone() },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        // The user may have navigated on while this folder was loading. A stale
        // out-of-order reply must not repaint rows for a folder we've left, or
        // the breadcrumb and the grid would disagree.
        if *ui.browser_path.borrow() != path {
            return;
        }
        match result {
            Ok(Ok(Response::Entries { entries })) => repaint_browser(&ui, &entries),
            Ok(Ok(Response::Error { message })) => browser_failed(&ui, &message),
            Ok(Ok(_)) => browser_failed(&ui, "Unexpected reply from the mount service."),
            Ok(Err(_)) | Err(_) => browser_unreachable(&ui),
        }
    });
}

/// Clear the model and show the daemon's error on the status page. Used for
/// in-band failures (a bad path, a permission error) — the mount is up, so Retry
/// (which restarts the service) wouldn't help and isn't offered.
fn browser_failed(ui: &Rc<Ui>, message: &str) {
    ui.browser_model.remove_all();
    browser_status(
        ui,
        "dialog-warning-symbolic",
        "Couldn't open this folder",
        message,
        false,
    );
}

/// The daemon didn't answer. Distinguish *still starting* (auto-retry, no
/// button) from *down* (actionable error + Retry), so a cold start self-heals
/// once the systemd mount comes up but a real failure stays visible.
fn browser_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        browser_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    browser_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        // Only keep polling while the Files page is the one on screen.
        if ui.stack.visible_child_name().as_deref() == Some("browser") {
            load_browser(&ui);
        }
    });
}

/// Repopulate the shared model — folders first, then case-insensitive by name —
/// which refreshes both the grid and the column list.
fn repaint_browser(ui: &Rc<Ui>, entries: &[DirEntry]) {
    ui.browser_model.remove_all();
    if entries.is_empty() {
        browser_status(
            ui,
            "folder-open-symbolic",
            "This folder is empty",
            "Upload a file or create a folder to get started.",
            false,
        );
        return;
    }
    browser_views(ui);

    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for entry in sorted {
        ui.browser_model.append(&BoxedAnyObject::new(entry));
    }
}

/// Wire the browser header's search box: debounce keystrokes, then either run a
/// search or — when cleared — restore the current directory listing.
fn wire_search(ui: &Rc<Ui>) {
    let ui_s = ui.clone();
    ui.browser_search.connect_search_changed(move |_| {
        // Replace any pending debounce so only the last keystroke's pause fires.
        if let Some(src) = ui_s.search_source.borrow_mut().take() {
            src.remove();
        }
        let ui_t = ui_s.clone();
        let src = glib::timeout_add_local_once(SEARCH_DEBOUNCE, move || {
            ui_t.search_source.borrow_mut().take();
            let query = ui_t.browser_search.text().trim().to_string();
            if query.is_empty() {
                load_browser(&ui_t);
            } else {
                run_search(&ui_t, &query);
            }
        });
        *ui_s.search_source.borrow_mut() = Some(src);
    });
}

/// Send a [`Request::Search`] to the daemon and render the hits in the browser
/// views, reusing the same row model so click-to-open and pin work unchanged
/// (each hit carries its full path; see [`entry_rel`]).
fn run_search(ui: &Rc<Ui>, query: &str) {
    ui.browser_model.remove_all();
    browser_status(
        ui,
        "system-search-symbolic",
        "Searching…",
        &format!("Looking for “{query}”."),
        false,
    );

    ui.busy_begin();
    let query = query.to_string();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::Search {
            query: query.clone(),
            limit: SEARCH_LIMIT,
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        // The box may have been cleared or typed past while the reply was in
        // flight; if the query no longer matches, a fresher load/search already
        // owns the model — drop this stale, possibly out-of-order result.
        if ui.browser_search.text().trim() != query {
            return;
        }
        match result {
            Ok(Ok(Response::SearchResults { hits })) => repaint_search(&ui, &hits),
            Ok(Ok(Response::Error { message })) => browser_failed(&ui, &message),
            Ok(Ok(_)) => browser_failed(&ui, "Unexpected reply from the mount service."),
            Ok(Err(_)) | Err(_) => browser_unreachable(&ui),
        }
    });
}

/// Repopulate the model with search hits — folders first, then by name — mapping
/// each [`SearchHit`] to a path-carrying [`DirEntry`] the existing renderers and
/// handlers already understand.
fn repaint_search(ui: &Rc<Ui>, hits: &[SearchHit]) {
    ui.browser_model.remove_all();
    if hits.is_empty() {
        browser_status(
            ui,
            "system-search-symbolic",
            "No matches",
            "No files or folders match that search.",
            false,
        );
        return;
    }
    browser_views(ui);

    let mut entries: Vec<DirEntry> = hits
        .iter()
        .map(|h| DirEntry {
            name: h.name.clone(),
            is_dir: h.is_dir,
            size: h.size,
            modified: h.modified,
            pinned: h.pinned,
            // Search hits don't carry cache state; the badge shows in listings.
            cached: false,
            uid: h.uid.clone(),
            path: h.path.clone(),
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for entry in entries {
        ui.browser_model.append(&BoxedAnyObject::new(entry));
    }
}

/// The widgets of the Trash page that a load repaints.
struct TrashWidgets {
    model: gio::ListStore,
    list: gtk4::ListView,
    content: gtk4::Stack,
    status: adw::StatusPage,
    retry: gtk4::Button,
    empty: gtk4::Button,
    refresh: gtk4::Button,
    subtitle: gtk4::Label,
}

/// Widgets the Shared page's load/repaint touch.
struct SharedWidgets {
    content: gtk4::Stack,
    status: adw::StatusPage,
    shared_with_me: adw::PreferencesGroup,
    invitations: adw::PreferencesGroup,
    bookmarks: adw::PreferencesGroup,
    retry: gtk4::Button,
    add_bookmark: gtk4::Button,
    refresh: gtk4::Button,
}

/// Widgets the Devices page's load/repaint touch.
struct DevicesWidgets {
    content: gtk4::Stack,
    status: adw::StatusPage,
    /// "This computer" — the local folders synced to this machine's device.
    sync_group: adw::PreferencesGroup,
    /// "Other computers" — the account's *other* registered devices. This
    /// machine's own device is deliberately not among them; see
    /// [`repaint_devices`].
    group: adw::PreferencesGroup,
    add_folder: gtk4::Button,
    retry: gtk4::Button,
    refresh: gtk4::Button,
}

/// The Shared page: three stacked sections — items shared *with* me (each with a
/// Leave action), invitations addressed to me pending accept/reject, and public
/// links I've bookmarked (open / remove). All three live outside the mount tree,
/// so the page addresses them by uid/id/token and always re-lists from the daemon.
fn build_shared_page() -> (gtk4::Widget, SharedWidgets) {
    let title = gtk4::Label::builder()
        .label("Shared with me")
        .halign(gtk4::Align::Start)
        .build();
    title.add_css_class("title-2");
    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title);

    let add_bookmark = gtk4::Button::builder()
        .label("Add Bookmark")
        .valign(gtk4::Align::Center)
        .build();
    add_bookmark.add_css_class("flat");
    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&titles);
    header.append(&refresh);
    header.append(&add_bookmark);

    let shared_with_me = adw::PreferencesGroup::builder()
        .title("Shared with me")
        .build();
    let invitations = adw::PreferencesGroup::builder()
        .title("Invitations")
        .build();
    let bookmarks = adw::PreferencesGroup::builder().title("Bookmarks").build();

    let groups = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    groups.append(&shared_with_me);
    groups.append(&invitations);
    groups.append(&bookmarks);
    let clamp = adw::Clamp::builder().child(&groups).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
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
        .icon_name("emblem-shared-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&scroll, Some("list"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&content);

    (
        inner.upcast(),
        SharedWidgets {
            content,
            status,
            shared_with_me,
            invitations,
            bookmarks,
            retry,
            add_bookmark,
            refresh,
        },
    )
}

/// The Computers page: a "This computer" section listing the local folders synced
/// to this machine's device (each row offering a mode toggle and Remove), plus an
/// "Other devices" section listing the account's other registered devices.
fn build_devices_page() -> (gtk4::Widget, DevicesWidgets) {
    let title = gtk4::Label::builder()
        .label("Computers")
        .halign(gtk4::Align::Start)
        .build();
    title.add_css_class("title-2");
    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title);

    let add_folder = gtk4::Button::builder()
        .label("Add Folder")
        .valign(gtk4::Align::Center)
        .build();
    add_folder.add_css_class("flat");
    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&titles);
    header.append(&refresh);
    header.append(&add_folder);

    // The description spells out what the per-row On-demand switch does to the
    // local copy, because the switch itself can't: turning it on *deletes* the
    // files from this disk, which is not something to discover afterwards.
    let sync_group = adw::PreferencesGroup::builder()
        .title("This computer")
        .description(
            "Local folders backed up to this device. Synced folders keep a full copy on \
             this disk; on-demand folders keep the files in Proton Drive only and fetch \
             them as you open them.",
        )
        .build();
    let group = adw::PreferencesGroup::builder()
        .title("Other computers")
        .description("Other devices backing up to this account.")
        .build();

    let groups = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    groups.append(&sync_group);
    groups.append(&group);
    let clamp = adw::Clamp::builder().child(&groups).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
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
        .icon_name("computer-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&scroll, Some("list"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&content);

    (
        inner.upcast(),
        DevicesWidgets {
            content,
            status,
            sync_group,
            group,
            add_folder,
            retry,
            refresh,
        },
    )
}

/// The Trash page: a flat list of everything Drive is holding in the trash, each
/// row offering Restore and Delete Forever, with Empty Trash in the header.
///
/// A trashed node has no path inside the mount — the daemon forgets it when it is
/// trashed — so unlike the Files page this one addresses entries by uid and always
/// re-lists from the server rather than from a cached listing. Row rendering needs
/// the [`Ui`] handle for its buttons, so the factory is installed in [`wire_trash`].
fn build_trash_page() -> (gtk4::Widget, TrashWidgets) {
    let model = gio::ListStore::new::<BoxedAnyObject>();

    let title = gtk4::Label::builder()
        .label("Trash")
        .halign(gtk4::Align::Start)
        .build();
    title.add_css_class("title-2");
    let subtitle = gtk4::Label::builder().halign(gtk4::Align::Start).build();
    subtitle.add_css_class("dim-label");
    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title);
    titles.append(&subtitle);

    let empty = gtk4::Button::builder()
        .label("Empty Trash")
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    empty.add_css_class("destructive-action");
    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&titles);
    header.append(&refresh);
    header.append(&empty);

    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("user-trash-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    // No selection model: every action lives on the row it acts on, so a
    // mis-aimed Delete Forever isn't one click away from a stale selection.
    let list = gtk4::ListView::builder()
        .model(&gtk4::NoSelection::new(Some(model.clone())))
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&list)
        .build();

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&scroll, Some("list"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&content);

    (
        inner.upcast(),
        TrashWidgets {
            model,
            list,
            content,
            status,
            retry,
            empty,
            refresh,
            subtitle,
        },
    )
}

/// Install the row factory and the Empty Trash button. The row's two buttons read
/// the entry off the [`gtk4::ListItem`] they were clicked on rather than a
/// captured copy, so a recycled row always acts on the item it currently shows.
fn wire_trash(ui: &Rc<Ui>, list: &gtk4::ListView, empty: &gtk4::Button) {
    let factory = gtk4::SignalListItemFactory::new();
    let ui_setup = ui.clone();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();

        let icon = gtk4::Image::builder().pixel_size(32).build();
        let name = gtk4::Label::builder()
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .build();
        let meta = gtk4::Label::builder().halign(gtk4::Align::Start).build();
        meta.add_css_class("dim-label");
        meta.add_css_class("caption");
        let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.set_valign(gtk4::Align::Center);
        text.append(&name);
        text.append(&meta);

        let restore = gtk4::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text("Restore to its original folder")
            .valign(gtk4::Align::Center)
            .build();
        restore.add_css_class("flat");
        let purge = gtk4::Button::builder()
            .icon_name("edit-delete-symbolic")
            .tooltip_text("Delete permanently")
            .valign(gtk4::Align::Center)
            .build();
        purge.add_css_class("flat");

        let ui_restore = ui_setup.clone();
        let item_restore = item.clone();
        restore.connect_clicked(move |_| {
            if let Some(entry) = bound_entry(&item_restore) {
                restore_entry(&ui_restore, &entry);
            }
        });
        let ui_purge = ui_setup.clone();
        let item_purge = item.clone();
        purge.connect_clicked(move |_| {
            if let Some(entry) = bound_entry(&item_purge) {
                prompt_delete_forever(&ui_purge, &entry);
            }
        });

        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(6);
        row.set_margin_end(6);
        row.append(&icon);
        row.append(&text);
        row.append(&restore);
        row.append(&purge);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let Some(entry) = bound_entry(item) else {
            return;
        };
        let row = item.child().and_downcast::<gtk4::Box>().unwrap();
        let Some(icon) = row.first_child().and_downcast::<gtk4::Image>() else {
            return;
        };
        icon.set_icon_name(Some(&format!("{}-symbolic", icon_base_for(&entry))));
        let Some(text) = icon.next_sibling().and_downcast::<gtk4::Box>() else {
            return;
        };
        if let Some(name) = text.first_child().and_downcast::<gtk4::Label>() {
            name.set_label(&entry.name);
        }
        if let Some(meta) = text.last_child().and_downcast::<gtk4::Label>() {
            let kind = if entry.is_dir {
                "Folder".to_string()
            } else {
                human_bytes(entry.size)
            };
            meta.set_label(&format!("{kind} · {}", format_modified(entry.modified)));
        }
    });
    list.set_factory(Some(&factory));

    let ui_empty = ui.clone();
    empty.connect_clicked(move |_| prompt_empty_trash(&ui_empty));
}

/// The [`DirEntry`] a list item is currently bound to, or `None` for an unbound
/// (recycled) row.
fn bound_entry(item: &gtk4::ListItem) -> Option<DirEntry> {
    let obj = item.item().and_downcast::<BoxedAnyObject>()?;
    let entry = obj.borrow::<DirEntry>().clone();
    Some(entry)
}

/// Fetch the trash listing and repaint the page.
fn load_trash(ui: &Rc<Ui>) {
    // Drop the old rows first: a stale row here would offer Restore on something
    // that may already be gone.
    ui.trash_model.remove_all();
    ui.trash_empty.set_sensitive(false);
    trash_status(
        ui,
        "user-trash-symbolic",
        "Loading…",
        "Reading the trash.",
        false,
    );

    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListTrash);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Entries { entries })) => repaint_trash(&ui, &entries),
            Ok(Ok(Response::Error { message })) => trash_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read the trash",
                &message,
                false,
            ),
            Ok(Ok(_)) => trash_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read the trash",
                "Unexpected reply from the mount service.",
                false,
            ),
            Ok(Err(_)) | Err(_) => trash_unreachable(&ui),
        }
    });
}

/// The daemon didn't answer. Same split as the Files page: still starting (poll
/// again, no button) versus actually down (Retry, which restarts the service).
fn trash_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        trash_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    trash_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("trash") {
            load_trash(&ui);
        }
    });
}

/// Show a status page in place of the trash list.
fn trash_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.trash_status.set_icon_name(Some(icon));
    ui.trash_status.set_title(title);
    ui.trash_status.set_description(Some(description));
    ui.trash_retry.set_visible(retry);
    ui.trash_content.set_visible_child_name("status");
    ui.trash_subtitle.set_label("");
}

/// Repopulate the trash list, most recently modified first — the order in which a
/// user looks for what they just deleted.
fn repaint_trash(ui: &Rc<Ui>, entries: &[DirEntry]) {
    ui.trash_model.remove_all();
    ui.trash_empty.set_sensitive(!entries.is_empty());
    if entries.is_empty() {
        trash_status(
            ui,
            "user-trash-symbolic",
            "Trash is empty",
            "Items you delete from Proton Drive show up here.",
            false,
        );
        return;
    }
    ui.trash_content.set_visible_child_name("list");
    ui.trash_subtitle.set_label(&match entries.len() {
        1 => "1 item".to_string(),
        n => format!("{n} items"),
    });

    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.modified));
    for entry in sorted {
        ui.trash_model.append(&BoxedAnyObject::new(entry));
    }
}

/// Restore one trashed entry to the folder it was trashed from.
fn restore_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let name = entry.name.clone();
    run_mutation(
        ui,
        Request::Restore {
            uids: vec![entry.uid.clone()],
        },
        format!("Restored “{name}”"),
        "Couldn't restore",
    );
}

/// Confirm, then permanently delete one trashed entry. Irreversible, so it asks.
fn prompt_delete_forever(ui: &Rc<Ui>, entry: &DirEntry) {
    let win = ui_window(ui);
    let uid = entry.uid.clone();
    let name = entry.name.clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Delete permanently")
        .body(format!(
            "Permanently delete “{name}”? This cannot be undone."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete Permanently");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "delete" {
            run_mutation(
                &ui,
                Request::DeleteForever {
                    uids: vec![uid.clone()],
                },
                format!("Deleted “{name}” permanently"),
                "Couldn't delete",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Confirm, then permanently delete everything in the trash.
fn prompt_empty_trash(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let count = ui.trash_model.n_items();
    let dialog = adw::AlertDialog::builder()
        .heading("Empty Trash")
        .body(format!(
            "Permanently delete all {count} item(s) in the trash? This cannot be undone."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("empty", "Empty Trash");
    dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "empty" {
            run_mutation(
                &ui,
                Request::EmptyTrash,
                "Trash emptied".to_string(),
                "Couldn't empty the trash",
            );
        }
    });
    dialog.present(win.as_ref());
}

// ---- Shared page ----------------------------------------------------------

/// Install the Shared page's retry and Add-Bookmark buttons.
fn wire_shared(ui: &Rc<Ui>, retry: &gtk4::Button, add_bookmark: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_shared(&ui_retry);
    });
    let ui_add = ui.clone();
    add_bookmark.connect_clicked(move |_| prompt_add_bookmark(&ui_add));
}

/// Show a status page in place of the Shared sections.
fn shared_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.shared_status.set_icon_name(Some(icon));
    ui.shared_status.set_title(title);
    ui.shared_status.set_description(Some(description));
    ui.shared_retry.set_visible(retry);
    ui.shared_content.set_visible_child_name("status");
}

/// Fetch the three Shared sections (shared-with-me, invitations, bookmarks) in
/// parallel and repaint the page once all three land.
fn load_shared(ui: &Rc<Ui>) {
    if ui.shared_inflight.get() {
        return;
    }
    ui.shared_inflight.set(true);
    shared_status(
        ui,
        "emblem-shared-symbolic",
        "Loading…",
        "Reading your shared items.",
        false,
    );

    ui.busy_begin();
    let socket = ui.dirs.control_socket();
    let shared_rx = spawn_request(socket.clone(), Request::ListSharedWithMe);
    let invites_rx = spawn_request(socket.clone(), Request::ListInvitations);
    let bookmarks_rx = spawn_request(socket, Request::ListBookmarks);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let shared = shared_rx.recv().await;
        let invites = invites_rx.recv().await;
        let bookmarks = bookmarks_rx.recv().await;
        ui.busy_end();
        ui.shared_inflight.set(false);

        // A transport failure on any of the three means the daemon isn't up.
        if matches!(shared, Ok(Err(_)) | Err(_))
            || matches!(invites, Ok(Err(_)) | Err(_))
            || matches!(bookmarks, Ok(Err(_)) | Err(_))
        {
            ui.shared_loaded_at.set(None);
            shared_unreachable(&ui);
            return;
        }

        let shared_items = match shared {
            Ok(Ok(Response::Entries { entries })) => entries,
            _ => Vec::new(),
        };
        let invitations = match invites {
            Ok(Ok(Response::Invitations { items })) => items,
            _ => Vec::new(),
        };
        let bookmark_items = match bookmarks {
            Ok(Ok(Response::Bookmarks { items })) => items,
            _ => Vec::new(),
        };
        repaint_shared(&ui, &shared_items, &invitations, &bookmark_items);
        ui.shared_loaded_at.set(Some(Instant::now()));
    });
}

/// The daemon didn't answer the Shared page. Same still-starting vs. down split
/// as the other pages.
fn shared_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        shared_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    shared_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("shared") {
            load_shared(&ui);
        }
    });
}

/// Rebuild the three Shared sections. Rows added last time are removed first —
/// adw groups have no clear-all — then each section is repopulated, showing a
/// dim placeholder row when empty.
fn repaint_shared(
    ui: &Rc<Ui>,
    shared: &[DirEntry],
    invitations: &[InvitationInfo],
    bookmarks: &[BookmarkInfo],
) {
    for (group, row) in ui.shared_rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    ui.shared_content.set_visible_child_name("list");
    let mut rows: Vec<(adw::PreferencesGroup, gtk4::Widget)> = Vec::new();

    // Shared with me: name + Leave.
    if shared.is_empty() {
        let row = dim_row("Nothing is shared with you.");
        ui.shared_with_me_group.add(&row);
        rows.push((ui.shared_with_me_group.clone(), row.upcast()));
    } else {
        for entry in shared {
            let row = adw::ActionRow::builder().title(&entry.name).build();
            row.add_prefix(&gtk4::Image::from_icon_name(if entry.is_dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));
            let leave = gtk4::Button::builder()
                .label("Leave")
                .valign(gtk4::Align::Center)
                .build();
            leave.add_css_class("flat");
            let ui_leave = ui.clone();
            let uid = entry.uid.clone();
            let name = entry.name.clone();
            leave.connect_clicked(move |_| prompt_leave_shared(&ui_leave, &uid, &name));
            row.add_suffix(&leave);
            ui.shared_with_me_group.add(&row);
            rows.push((ui.shared_with_me_group.clone(), row.upcast()));
        }
    }

    // Invitations: inviter + item, Accept / Reject.
    if invitations.is_empty() {
        let row = dim_row("No pending invitations.");
        ui.invitations_group.add(&row);
        rows.push((ui.invitations_group.clone(), row.upcast()));
    } else {
        for inv in invitations {
            let item = inv
                .name
                .clone()
                .unwrap_or_else(|| "a shared item".to_string());
            let row = adw::ActionRow::builder()
                .title(&item)
                .subtitle(format!("from {}", inv.inviter_email))
                .build();
            row.add_prefix(&gtk4::Image::from_icon_name(if inv.is_dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));
            let reject = gtk4::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text("Reject")
                .valign(gtk4::Align::Center)
                .build();
            reject.add_css_class("flat");
            let accept = gtk4::Button::builder()
                .label("Accept")
                .valign(gtk4::Align::Center)
                .build();
            accept.add_css_class("suggested-action");
            let ui_acc = ui.clone();
            let id_acc = inv.id.clone();
            accept.connect_clicked(move |_| {
                respond_invitation(&ui_acc, &id_acc, true);
            });
            let ui_rej = ui.clone();
            let id_rej = inv.id.clone();
            reject.connect_clicked(move |_| {
                respond_invitation(&ui_rej, &id_rej, false);
            });
            row.add_suffix(&accept);
            row.add_suffix(&reject);
            ui.invitations_group.add(&row);
            rows.push((ui.invitations_group.clone(), row.upcast()));
        }
    }

    // Bookmarks: name/URL, Open / Remove.
    if bookmarks.is_empty() {
        let row = dim_row("No saved bookmarks.");
        ui.bookmarks_group.add(&row);
        rows.push((ui.bookmarks_group.clone(), row.upcast()));
    } else {
        for bm in bookmarks {
            let title = bm.name.clone().unwrap_or_else(|| "Shared link".to_string());
            let row = adw::ActionRow::builder()
                .title(&title)
                .subtitle(&bm.url)
                .build();
            row.add_prefix(&gtk4::Image::from_icon_name("emblem-symbolic-link"));
            let remove = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove bookmark")
                .valign(gtk4::Align::Center)
                .build();
            remove.add_css_class("flat");
            let open = gtk4::Button::builder()
                .icon_name("external-link-symbolic")
                .tooltip_text("Open in browser")
                .valign(gtk4::Align::Center)
                .build();
            open.add_css_class("flat");
            let url_open = bm.url.clone();
            open.connect_clicked(move |_| open_uri(&url_open));
            let ui_rm = ui.clone();
            let token = bm.token.clone();
            let name_rm = title.clone();
            remove.connect_clicked(move |_| prompt_remove_bookmark(&ui_rm, &token, &name_rm));
            row.add_suffix(&open);
            row.add_suffix(&remove);
            ui.bookmarks_group.add(&row);
            rows.push((ui.bookmarks_group.clone(), row.upcast()));
        }
    }

    *ui.shared_rows.borrow_mut() = rows;
}

/// Accept or reject an invitation, then reload the Shared page.
fn respond_invitation(ui: &Rc<Ui>, id: &str, accept: bool) {
    let req = if accept {
        Request::AcceptInvitation { id: id.to_string() }
    } else {
        Request::RejectInvitation { id: id.to_string() }
    };
    let (done, failed) = if accept {
        ("Invitation accepted", "Couldn't accept the invitation")
    } else {
        ("Invitation rejected", "Couldn't reject the invitation")
    };
    run_shared_mutation(ui, req, done, failed);
}

/// Confirm, then leave a node shared with me.
fn prompt_leave_shared(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Leave shared item")
        .body(format!(
            "Leave “{name}”? You'll lose access until you're invited again."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("leave", "Leave");
    dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp == "leave" {
            run_shared_mutation(
                &ui,
                Request::LeaveShared { uid: uid.clone() },
                "Left shared item",
                "Couldn't leave the shared item",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Confirm, then remove a saved bookmark.
fn prompt_remove_bookmark(ui: &Rc<Ui>, token: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Remove bookmark")
        .body(format!("Remove the bookmark for “{name}”?"))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Remove");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let token = token.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp == "remove" {
            run_shared_mutation(
                &ui,
                Request::DeleteBookmark {
                    token: token.clone(),
                },
                "Bookmark removed",
                "Couldn't remove the bookmark",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Prompt for a public-link URL (and optional password) and save it as a bookmark.
fn prompt_add_bookmark(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Add bookmark")
        .body("Paste a Proton Drive public link to save it here.")
        .build();
    let group = adw::PreferencesGroup::new();
    let url_row = adw::EntryRow::builder()
        .title("Public link URL")
        .activates_default(true)
        .build();
    let pw_row = adw::PasswordEntryRow::builder()
        .title("Password (if the link has one)")
        .activates_default(true)
        .build();
    group.add(&url_row);
    group.add(&pw_row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Add");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "add" {
            return;
        }
        let url = url_row.text().trim().to_string();
        if url.is_empty() {
            toast_error(&ui, "Couldn't add bookmark", "A URL is required.");
            return;
        }
        let pw = pw_row.text().to_string();
        let password = if pw.is_empty() { None } else { Some(pw) };
        run_shared_mutation(
            &ui,
            Request::CreateBookmark { url, password },
            "Bookmark saved",
            "Couldn't add the bookmark",
        );
    });
    dialog.present(win.as_ref());
}

/// Run a mutation raised from the Shared page and reload the page on success.
fn run_shared_mutation(ui: &Rc<Ui>, req: Request, done: &'static str, failed: &'static str) {
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    let done = done.to_string();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => {
                load_shared(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message })) => toast_error(&ui, failed, &message),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}

// ---- Devices page ---------------------------------------------------------

/// Install the Devices page's retry button and the "Add Folder" action.
fn wire_devices(ui: &Rc<Ui>, retry: &gtk4::Button, add_folder: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_devices(&ui_retry);
    });
    let ui_add = ui.clone();
    add_folder.connect_clicked(move |_| prompt_add_sync_folder(&ui_add));
}

/// Show a status page in place of the Devices list.
fn devices_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.devices_status.set_icon_name(Some(icon));
    ui.devices_status.set_title(title);
    ui.devices_status.set_description(Some(description));
    ui.devices_retry.set_visible(retry);
    ui.devices_content.set_visible_child_name("status");
}

/// Fetch this machine's synced folders and the account's other devices, then
/// repaint both sections. The two requests are chained so a single unreachable
/// daemon collapses the whole page to a status view.
fn load_devices(ui: &Rc<Ui>) {
    if ui.devices_inflight.get() {
        return;
    }
    ui.devices_inflight.set(true);
    devices_status(
        ui,
        "computer-symbolic",
        "Loading…",
        "Reading your devices.",
        false,
    );
    ui.busy_begin();
    // The two lists are independent and the daemon serves requests concurrently,
    // so fire both up front and collect them, rather than paying two round trips
    // back to back.
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSyncFolders);
    let rx2 = spawn_request(ui.dirs.control_socket(), Request::ListDevices);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let sync = rx.recv().await;
        let devices_reply = rx2.recv().await;
        let folders = match sync {
            Ok(Ok(Response::SyncFolders { items })) => items,
            // The daemon answered but not as expected — treat as empty and carry
            // on to the devices list rather than failing the whole page.
            Ok(Ok(_)) => Vec::new(),
            Ok(Err(_)) | Err(_) => {
                ui.busy_end();
                ui.devices_inflight.set(false);
                ui.devices_loaded_at.set(None);
                devices_unreachable(&ui);
                return;
            }
        };
        // Daemon reachable: show the list and paint the synced-folders section.
        ui.devices_content.set_visible_child_name("list");
        repaint_sync_folders(&ui, &folders);

        let devices = match devices_reply {
            Ok(Ok(Response::Devices { items })) => items,
            _ => Vec::new(),
        };
        ui.busy_end();
        ui.devices_inflight.set(false);
        repaint_devices(&ui, &devices);
        ui.devices_loaded_at.set(Some(Instant::now()));
    });
}

/// Refresh just the synced-folders section, with no status flash, no spinner and
/// no devices round-trip. Driven by the periodic tick while the Devices page is
/// on screen: a folder's sync progress is only live if something re-reads it.
///
/// A hiccup leaves the page as it is — the next tick tries again, and a real
/// outage is reported by [`load_devices`] on the next visit.
fn refresh_sync_folders(ui: &Rc<Ui>) {
    if ui.devices_inflight.get() {
        return;
    }
    ui.devices_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSyncFolders);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.devices_inflight.set(false);
        let Ok(Ok(Response::SyncFolders { items })) = result else {
            return;
        };
        // The page may have been navigated away from, or collapsed to a status
        // view, while the request was in flight.
        if ui.stack.visible_child_name().as_deref() == Some("devices")
            && ui.devices_content.visible_child_name().as_deref() == Some("list")
        {
            repaint_sync_folders(&ui, &items);
        }
    });
}

/// The daemon didn't answer the Devices page.
fn devices_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        devices_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    devices_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("devices") {
            load_devices(&ui);
        }
    });
}

/// Rebuild the "Other computers" section from a fresh listing. Empty is a normal
/// state (this machine may be the only device), so it shows a placeholder row
/// rather than collapsing the page.
///
/// This machine's own device is filtered out. Deleting a device deletes its root
/// folder — every file backed up from it — and for *this* machine that would also
/// pull the ground out from under the synced folders listed directly above, so it
/// is not offered as a peer of "remove some other laptop". Removing this
/// computer's backup means removing its folders, each of which asks about its
/// cloud copy on its own terms.
fn repaint_devices(ui: &Rc<Ui>, devices: &[DeviceInfo]) {
    for row in ui.devices_rows.borrow_mut().drain(..) {
        ui.devices_group.remove(&row);
    }
    let others: Vec<&DeviceInfo> = devices.iter().filter(|d| !d.this_device).collect();
    if others.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No other computers")
            .subtitle("Desktop apps syncing to this account appear here.")
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("computer-symbolic"));
        ui.devices_group.add(&row);
        *ui.devices_rows.borrow_mut() = vec![row.upcast()];
        return;
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for dev in others {
        let row = adw::ActionRow::builder()
            .title(&dev.name)
            .subtitle(device_subtitle(dev))
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("computer-symbolic"));
        let remove = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove this computer and everything it backed up")
            .valign(gtk4::Align::Center)
            .build();
        remove.add_css_class("flat");
        let rename = gtk4::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Rename computer")
            .valign(gtk4::Align::Center)
            .build();
        rename.add_css_class("flat");
        let ui_ren = ui.clone();
        let uid_ren = dev.uid.clone();
        let name_ren = dev.name.clone();
        rename.connect_clicked(move |_| prompt_rename_device(&ui_ren, &uid_ren, &name_ren));
        let ui_rm = ui.clone();
        let uid_rm = dev.uid.clone();
        let name_rm = dev.name.clone();
        remove.connect_clicked(move |_| prompt_remove_device(&ui_rm, &uid_rm, &name_rm));
        row.add_suffix(&rename);
        row.add_suffix(&remove);
        ui.devices_group.add(&row);
        rows.push(row.upcast());
    }
    *ui.devices_rows.borrow_mut() = rows;
}

/// A device row's subtitle: its platform, and when it last synced. A device that
/// has never synced says so — an unexplained missing date reads as a bug, and
/// "never" is the fact that tells the user this computer isn't backing anything up.
fn device_subtitle(dev: &DeviceInfo) -> String {
    match dev.last_sync {
        Some(secs) if secs > 0 => {
            format!("{} · last synced {}", dev.device_type, activity_time(secs))
        }
        _ => format!("{} · never synced", dev.device_type),
    }
}

/// Rebuild the "This computer" section from a fresh synced-folders listing.
fn repaint_sync_folders(ui: &Rc<Ui>, folders: &[SyncFolderInfo]) {
    for row in ui.devices_sync_rows.borrow_mut().drain(..) {
        ui.devices_sync_group.remove(&row);
    }
    if folders.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No synced folders")
            .subtitle("Add a folder to back it up to this device.")
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("folder-symbolic"));
        ui.devices_sync_group.add(&row);
        *ui.devices_sync_rows.borrow_mut() = vec![row.upcast()];
        return;
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for f in folders {
        // A running pass describes itself ("Uploading photo.jpg — 12 of 40");
        // otherwise fall back to the folder's resting state.
        let status = match &f.progress {
            Some(p) => sync_progress_label(p),
            None => sync_state_label(&f.state).to_string(),
        };
        // A queued switch is where the folder is *going*, which is what the user
        // just asked for and is waiting to see happen — so it leads the subtitle
        // instead of the mode the folder is still technically in.
        let subtitle = match (f.pending_mode.as_deref(), f.mode.as_str()) {
            (Some("ondemand"), _) => format!("Going on-demand · {status}"),
            (Some(_), _) => format!("Switching to synced · {status}"),
            (None, "ondemand") => format!("On-demand · {status}"),
            (None, _) => format!("Synced · {status}"),
        };
        let row = adw::ActionRow::builder()
            .title(&f.local_path)
            .subtitle(&subtitle)
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("folder-symbolic"));
        let id = f.id;

        // A pass that knows how far it has got shows it: the subtitle alone
        // ("syncing photo.jpg — 12 of 40") never conveys that the end is near. This
        // holds for scanning as much as applying — a big folder spends minutes there
        // — but a folder syncing for the first time has no estimate to draw against
        // (`total == 0`), and this row is repainted on the 2s tick, too slow for a
        // pulse to read as motion. So it stays text-only until real counts exist.
        if let Some(p) = &f.progress
            && p.total > 0
        {
            let bar = gtk4::ProgressBar::builder()
                .fraction((p.done as f64 / p.total.max(p.done) as f64).min(1.0))
                .valign(gtk4::Align::Center)
                .width_request(120)
                .build();
            row.add_suffix(&bar);
        }

        // Sync now — only meaningful for mirror folders (on-demand has no local
        // copy to reconcile). The engine also syncs on file changes and on a
        // 120s poll; this is the "don't wait" button.
        if f.mode != "ondemand" {
            let sync_now = gtk4::Button::builder()
                .icon_name("view-refresh-symbolic")
                .tooltip_text("Sync this folder now")
                .valign(gtk4::Align::Center)
                .build();
            sync_now.add_css_class("flat");
            let ui_sync = ui.clone();
            sync_now.connect_clicked(move |_| {
                let rx = spawn_request(
                    ui_sync.dirs.control_socket(),
                    Request::SyncNow { id: Some(id) },
                );
                let ui_sync = ui_sync.clone();
                glib::spawn_future_local(async move {
                    match rx.recv().await {
                        Ok(Ok(Response::Ok { .. })) => toast(&ui_sync, "Syncing folder…"),
                        Ok(Ok(Response::Error { message })) => {
                            toast_error(&ui_sync, "Couldn't sync", &message)
                        }
                        _ => toast_error(
                            &ui_sync,
                            "Couldn't sync",
                            "The mount service didn't respond.",
                        ),
                    }
                });
            });
            row.add_suffix(&sync_now);
        }

        // On-demand toggle: off = full local copy (mirror), on = FUSE mount that
        // frees the disk. Set the state before wiring the handler so painting the
        // current mode doesn't fire a spurious request.
        //
        // A queued switch paints as already flipped: the daemon accepted the
        // request and will act on it, so snapping the switch back to the current
        // mode would read as "it didn't take" and invite the user to toggle again.
        let target = f.pending_mode.as_deref().unwrap_or(&f.mode);
        let ondemand = gtk4::Switch::builder()
            .tooltip_text(
                "On-demand: free this disk by keeping the files in Proton Drive only, \
                 fetching each as you open it. Turn off to download them back and keep a \
                 full local copy.",
            )
            .valign(gtk4::Align::Center)
            .active(target == "ondemand")
            .build();
        let ui_mode = ui.clone();
        ondemand.connect_state_set(move |_, on| {
            set_sync_folder_mode(&ui_mode, id, if on { "ondemand" } else { "mirror" });
            glib::Propagation::Proceed
        });
        row.add_suffix(&ondemand);

        let remove = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Stop syncing this folder")
            .valign(gtk4::Align::Center)
            .build();
        remove.add_css_class("flat");
        let ui_rm = ui.clone();
        let path = f.local_path.clone();
        // The folder's *current* mode, not a queued one: a switch that hasn't
        // landed yet has not moved the files anywhere.
        let ondemand = f.mode == "ondemand";
        remove.connect_clicked(move |_| prompt_remove_sync_folder(&ui_rm, id, &path, ondemand));
        row.add_suffix(&remove);
        ui.devices_sync_group.add(&row);
        rows.push(row.upcast());
    }
    *ui.devices_sync_rows.borrow_mut() = rows;
}

/// Human label for a synced folder's `state` column.
fn sync_state_label(state: &str) -> &str {
    match state {
        "syncing" => "syncing…",
        "error" => "sync error",
        "conflict" => "needs attention",
        _ => "up to date",
    }
}

/// Human label for a sync pass in flight: what it is doing, to which file, and
/// how far along it is. Neither phase's total is exact — the scan's is an estimate
/// from the last pass, and the applying total grows as deeper paths are classified
/// — so both read "12 of 40" rather than a percentage that could go backwards.
fn sync_progress_label(p: &SyncProgress) -> String {
    match p.phase {
        // Before the first pass finishes there is no estimate, so the count would
        // be "checked 12 of 12" — worse than saying nothing.
        SyncPhase::Scanning if p.total == 0 => "checking for changes…".to_string(),
        SyncPhase::Scanning => format!(
            "checking for changes — {} of {}",
            p.done,
            p.total.max(p.done)
        ),
        SyncPhase::Applying => {
            let count = format!("{} of {}", p.done + 1, p.total.max(p.done + 1));
            if p.current.is_empty() {
                format!("syncing {count}")
            } else {
                format!("syncing {} — {count}", p.current)
            }
        }
    }
}

/// Pick a local folder and hand it to the daemon to sync to this device.
fn prompt_add_sync_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let dialog = gtk4::FileDialog::builder()
        .title("Add Folder to Sync")
        .build();
    let ui = ui.clone();
    dialog.select_folder(win.as_ref(), gio::Cancellable::NONE, move |res| {
        let Ok(folder) = res else { return };
        let Some(local_path) = folder.path().and_then(|p| p.to_str().map(str::to_string)) else {
            return;
        };
        let rx = spawn_request(
            ui.dirs.control_socket(),
            Request::AddSyncFolder {
                local_path: local_path.clone(),
            },
        );
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(Response::Ok { .. })) => {
                    // The daemon acks before the row exists — registering the
                    // device and creating the remote folder is off-socket network
                    // work. A fixed delay here either fires too early (empty list)
                    // or too late (row flashes in). Instead let the periodic
                    // `refresh_sync_folders` tick pick the row up whenever it
                    // actually lands, which is what keeps a running pass live too.
                    toast(&ui, "Syncing folder…");
                }
                Ok(Ok(Response::Error { message })) => {
                    toast_error(&ui, "Couldn't add folder", &message)
                }
                _ => toast_error(
                    &ui,
                    "Couldn't add folder",
                    "The mount service didn't respond.",
                ),
            }
        });
    });
}

/// Flip a synced folder between `mirror` and `ondemand`. Reloads after so the
/// row's subtitle and switch reflect the daemon's real state (the request may be
/// rejected, e.g. switching to on-demand while a folder is mid-sync).
fn set_sync_folder_mode(ui: &Rc<Ui>, id: i64, mode: &str) {
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::SetSyncFolderMode {
            id,
            mode: mode.to_string(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { message })) => toast(&ui, &message),
            Ok(Ok(Response::Error { message })) => {
                toast_error(&ui, "Couldn't change mode", &message)
            }
            _ => toast_error(
                &ui,
                "Couldn't change mode",
                "The mount service didn't respond.",
            ),
        }
        if ui.stack.visible_child_name().as_deref() == Some("devices") {
            load_devices(&ui);
        }
    });
}

/// Confirm, then stop syncing a folder. Offers to also delete the cloud copy.
fn prompt_remove_sync_folder(ui: &Rc<Ui>, id: i64, path: &str, ondemand: bool) {
    let win = ui_window(ui);
    // The two modes leave the user in opposite places, so they can't share a
    // sentence. A mirror folder's files are already on this disk and stay there.
    // An on-demand folder's are not: the path is a mount over content that lives
    // in Proton Drive, so unmounting it leaves an empty directory — which is a
    // nasty surprise if the dialog claimed the local files were safe, and is
    // recoverable only by turning On-demand off *first* and letting it download.
    let body = if ondemand {
        format!(
            "Stop syncing “{path}”?\n\nThis folder is on-demand: its files live in Proton \
             Drive, not on this disk, so the folder will be empty once it is unmounted. To \
             keep a local copy, cancel, turn off On-demand, and wait for the download to \
             finish before removing it."
        )
    } else {
        format!(
            "Stop syncing “{path}”?\n\nThe local files stay on this disk and simply stop \
             being synced. Choose whether to also delete the copy in Proton Drive."
        )
    };
    let dialog = adw::AlertDialog::builder()
        .heading("Stop syncing folder")
        .body(body)
        .build();
    let group = adw::PreferencesGroup::new();
    let delete_remote = adw::SwitchRow::builder()
        .title("Also delete from Proton Drive")
        .subtitle(if ondemand {
            "Deletes the only copy of these files."
        } else {
            "The local copy is unaffected."
        })
        .active(false)
        .build();
    group.add(&delete_remote);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Stop Syncing");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "remove" {
            run_devices_mutation(
                &ui,
                Request::RemoveSyncFolder {
                    id,
                    delete_remote: delete_remote.is_active(),
                },
                "Stopped syncing folder",
                "Couldn't stop syncing the folder",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Prompt for a new device name and rename it.
fn prompt_rename_device(ui: &Rc<Ui>, uid: &str, current: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Rename device")
        .body(format!("Rename “{current}”."))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("New name")
        .activates_default(true)
        .build();
    row.set_text(current);
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp != "rename" {
            return;
        }
        let name = row.text().trim().to_string();
        if name.is_empty() {
            toast_error(&ui, "Couldn't rename device", "A name is required.");
            return;
        }
        run_devices_mutation(
            &ui,
            Request::RenameDevice {
                uid: uid.clone(),
                name,
            },
            "Device renamed",
            "Couldn't rename the device",
        );
    });
    dialog.present(win.as_ref());
}

/// Confirm, then remove (deregister) a device.
fn prompt_remove_device(ui: &Rc<Ui>, uid: &str, name: &str) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder()
        .heading("Remove computer")
        .body(format!(
            "Remove “{name}” from this account?\n\nEverything it backed up to Proton Drive is \
             deleted along with it. The files on that computer itself are not touched — but \
             this cannot be undone from here."
        ))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Remove");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ui = ui.clone();
    let uid = uid.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp == "remove" {
            run_devices_mutation(
                &ui,
                Request::DeleteDevice { uid: uid.clone() },
                "Device removed",
                "Couldn't remove the device",
            );
        }
    });
    dialog.present(win.as_ref());
}

/// Run a mutation raised from the Devices page and reload the page on success.
fn run_devices_mutation(ui: &Rc<Ui>, req: Request, done: &'static str, failed: &'static str) {
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    let done = done.to_string();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => {
                load_devices(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message })) => toast_error(&ui, failed, &message),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}

// ---- Shared (by me) page --------------------------------------------------

/// Widgets the Shared (by-me) page's load/repaint touch.
struct SharedByMeWidgets {
    content: gtk4::Stack,
    status: adw::StatusPage,
    group: adw::PreferencesGroup,
    retry: gtk4::Button,
    refresh: gtk4::Button,
}

/// The Shared page: one section listing the items I have shared with others —
/// each row summarizing who has access and carrying its public link (copy/open)
/// and a Manage shortcut into the per-node Share dialog.
fn build_shared_by_me_page() -> (gtk4::Widget, SharedByMeWidgets) {
    let title = gtk4::Label::builder()
        .label("Shared")
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .build();
    title.add_css_class("title-2");
    let refresh = refresh_button();
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.set_hexpand(true);
    header.append(&title);
    header.append(&refresh);

    let group = adw::PreferencesGroup::new();
    let clamp = adw::Clamp::builder().child(&group).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
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
        .icon_name("emblem-shared-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&scroll, Some("list"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&content);

    (
        inner.upcast(),
        SharedByMeWidgets {
            content,
            status,
            group,
            retry,
            refresh,
        },
    )
}

/// Install the Shared (by-me) page's retry button.
fn wire_shared_by_me(ui: &Rc<Ui>, retry: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_shared_by_me(&ui_retry);
    });
}

/// Show a status page in place of the Shared (by-me) list.
fn shared_by_me_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.shared_by_me_status.set_icon_name(Some(icon));
    ui.shared_by_me_status.set_title(title);
    ui.shared_by_me_status.set_description(Some(description));
    ui.shared_by_me_retry.set_visible(retry);
    ui.shared_by_me_content.set_visible_child_name("status");
}

/// Fetch the shared-by-me listing and repaint the page.
fn load_shared_by_me(ui: &Rc<Ui>) {
    if ui.shared_by_me_inflight.get() {
        return;
    }
    ui.shared_by_me_inflight.set(true);
    shared_by_me_status(
        ui,
        "emblem-shared-symbolic",
        "Loading…",
        "Reading what you've shared.",
        false,
    );
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListSharedByMe);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.shared_by_me_inflight.set(false);
        match result {
            Ok(Ok(Response::SharedByMe { items })) => {
                repaint_shared_by_me(&ui, &items);
                ui.shared_by_me_loaded_at.set(Some(Instant::now()));
            }
            Ok(Ok(Response::Error { message })) => shared_by_me_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read your shares",
                &message,
                false,
            ),
            Ok(Ok(_)) => shared_by_me_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't read your shares",
                "Unexpected reply from the mount service.",
                false,
            ),
            Ok(Err(_)) | Err(_) => {
                ui.shared_by_me_loaded_at.set(None);
                shared_by_me_unreachable(&ui);
            }
        }
    });
}

/// The daemon didn't answer the Shared (by-me) page.
fn shared_by_me_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        shared_by_me_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    shared_by_me_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("sharedbyme") {
            load_shared_by_me(&ui);
        }
    });
}

/// Rebuild the Shared (by-me) section from a fresh listing.
fn repaint_shared_by_me(ui: &Rc<Ui>, items: &[SharedItem]) {
    for row in ui.shared_by_me_rows.borrow_mut().drain(..) {
        ui.shared_by_me_group.remove(&row);
    }
    if items.is_empty() {
        shared_by_me_status(
            ui,
            "emblem-shared-symbolic",
            "Nothing shared yet",
            "Items you share with people or by link show up here.",
            false,
        );
        return;
    }
    ui.shared_by_me_content.set_visible_child_name("list");
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for item in items {
        let row = adw::ActionRow::builder()
            .title(&item.name)
            .subtitle(shared_item_summary(item))
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name(if item.is_dir {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        }));

        // Copy / open the public link, when there is one with a recovered URL.
        if let Some(url) = item.link.as_ref().and_then(|l| l.url.clone()) {
            let copy = gtk4::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy link")
                .valign(gtk4::Align::Center)
                .build();
            copy.add_css_class("flat");
            let ui_copy = ui.clone();
            let url_copy = url.clone();
            copy.connect_clicked(move |btn| {
                btn.clipboard().set_text(&url_copy);
                toast(&ui_copy, "Link copied");
            });
            let open = gtk4::Button::builder()
                .icon_name("external-link-symbolic")
                .tooltip_text("Open link")
                .valign(gtk4::Align::Center)
                .build();
            open.add_css_class("flat");
            let url_open = url.clone();
            open.connect_clicked(move |_| open_uri(&url_open));
            row.add_suffix(&copy);
            row.add_suffix(&open);
        }

        // Manage opens the per-node Share dialog — only when the node's path is
        // known (it has been browsed to this session); the dialog is path-keyed.
        if !item.path.is_empty() {
            let manage = gtk4::Button::builder()
                .label("Manage")
                .valign(gtk4::Align::Center)
                .build();
            manage.add_css_class("flat");
            let ui_manage = ui.clone();
            let entry = shared_item_as_entry(item);
            manage.connect_clicked(move |_| open_share_dialog(&ui_manage, &entry));
            row.add_suffix(&manage);
        }

        ui.shared_by_me_group.add(&row);
        rows.push(row.upcast());
    }
    *ui.shared_by_me_rows.borrow_mut() = rows;
}

/// A one-line summary of who can reach a shared item, for its row subtitle.
fn shared_item_summary(item: &SharedItem) -> String {
    let mut parts = Vec::new();
    if item.member_count > 0 {
        parts.push(format!(
            "{} {}",
            item.member_count,
            if item.member_count == 1 {
                "person"
            } else {
                "people"
            }
        ));
    }
    if item.invite_count > 0 {
        parts.push(format!("{} pending", item.invite_count));
    }
    if item.link.is_some() {
        parts.push("Public link".to_string());
    }
    if parts.is_empty() {
        "Shared".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Build a [`DirEntry`] from a [`SharedItem`] so the path-keyed Share dialog can
/// open on it. Only used when the item's path is known.
fn shared_item_as_entry(item: &SharedItem) -> DirEntry {
    DirEntry {
        name: item.name.clone(),
        is_dir: item.is_dir,
        size: 0,
        modified: 0,
        pinned: false,
        cached: false,
        uid: item.uid.clone(),
        path: item.path.clone(),
    }
}

// ---- Activity page --------------------------------------------------------

/// Widgets the Activity page's load/repaint touch.
struct ActivityWidgets {
    content: gtk4::Stack,
    status: adw::StatusPage,
    group: adw::PreferencesGroup,
    retry: gtk4::Button,
    refresh: gtk4::Button,
}

/// The Activity page: a newest-first feed of the mutations and transfers the
/// daemon performed this session (uploads, deletes, shares, …).
fn build_activity_page() -> (gtk4::Widget, ActivityWidgets) {
    let title = gtk4::Label::builder()
        .label("Activity")
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .build();
    title.add_css_class("title-2");
    let refresh = refresh_button();
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.set_hexpand(true);
    header.append(&title);
    header.append(&refresh);

    let group = adw::PreferencesGroup::new();
    let clamp = adw::Clamp::builder().child(&group).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
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

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&content);

    (
        inner.upcast(),
        ActivityWidgets {
            content,
            status,
            group,
            retry,
            refresh,
        },
    )
}

/// Install the Activity page's retry button.
fn wire_activity(ui: &Rc<Ui>, retry: &gtk4::Button) {
    let ui_retry = ui.clone();
    retry.connect_clicked(move |_| {
        service::restart();
        load_activity(&ui_retry);
    });
}

/// Show a status page in place of the Activity list.
fn activity_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.activity_status.set_icon_name(Some(icon));
    ui.activity_status.set_title(title);
    ui.activity_status.set_description(Some(description));
    ui.activity_retry.set_visible(retry);
    ui.activity_content.set_visible_child_name("status");
    // The list is no longer what is on screen, so the next repaint must not skip
    // itself as a no-op and leave this status view up.
    *ui.activity_key.borrow_mut() = None;
}

/// Refresh the Activity feed in place, with no status flash and no spinner.
/// Driven by the periodic tick while the page is on screen, so a running sync
/// pass fills the feed as it works rather than only once it is done. Anything
/// other than a good answer leaves the rows alone until the next tick.
fn refresh_activity(ui: &Rc<Ui>) {
    if ui.activity_inflight.get() {
        return;
    }
    ui.activity_inflight.set(true);
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListActivity { limit: 200 },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.activity_inflight.set(false);
        // The page may have been navigated away from while the request was in
        // flight.
        if let Ok(Ok(Response::Activity { items })) = result
            && ui.stack.visible_child_name().as_deref() == Some("activity")
        {
            repaint_activity(&ui, &items);
        }
    });
}

fn load_activity(ui: &Rc<Ui>) {
    if ui.activity_inflight.get() {
        return;
    }
    ui.activity_inflight.set(true);
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
        ui.activity_inflight.set(false);
        match result {
            Ok(Ok(Response::Activity { items })) => repaint_activity(&ui, &items),
            Ok(Ok(Response::Error { message })) => activity_status(
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
fn activity_unreachable(ui: &Rc<Ui>) {
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

/// A cheap fingerprint of a feed: how many entries, and what the newest one is.
/// The log is append-only and newest-first, so a feed with the same length and
/// the same newest entry is the same feed.
fn activity_key(items: &[ActivityEntry]) -> String {
    match items.first() {
        Some(a) => format!("{}:{}:{}:{}", items.len(), a.time, a.target, a.detail),
        None => String::new(),
    }
}

/// Rebuild the Activity section from a fresh log, unless it is already showing
/// exactly this feed.
fn repaint_activity(ui: &Rc<Ui>, items: &[ActivityEntry]) {
    let key = activity_key(items);
    if ui.activity_key.borrow().as_deref() == Some(key.as_str()) {
        return;
    }
    *ui.activity_key.borrow_mut() = Some(key);
    for row in ui.activity_rows.borrow_mut().drain(..) {
        ui.activity_group.remove(&row);
    }
    if items.is_empty() {
        activity_status(
            ui,
            "document-open-recent-symbolic",
            "Nothing yet",
            "Uploads, moves, shares and other changes appear here as they happen.",
            false,
        );
        return;
    }
    ui.activity_content.set_visible_child_name("list");
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    for a in items {
        let mut subtitle = activity_time(a.time);
        if !a.detail.is_empty() {
            subtitle = format!("{subtitle} · {}", a.detail);
        }
        let row = adw::ActionRow::builder()
            .title(format!("{} {}", activity_verb(a.kind), a.target))
            .subtitle(subtitle)
            .build();
        let icon = if a.ok {
            activity_icon(a.kind)
        } else {
            "dialog-warning-symbolic"
        };
        row.add_prefix(&gtk4::Image::from_icon_name(icon));
        row.set_activatable(false);
        ui.activity_group.add(&row);
        rows.push(row.upcast());
    }
    *ui.activity_rows.borrow_mut() = rows;
}

/// A human verb for an activity kind, used as the row title's lead word.
fn activity_verb(kind: ActivityKind) -> &'static str {
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
        ActivityKind::EmptyTrash => "Emptied trash —",
        ActivityKind::Share => "Shared",
        ActivityKind::PublicLink => "Linked",
        ActivityKind::Unshare => "Unshared",
    }
}

/// A themed icon for an activity kind.
fn activity_icon(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Upload => "document-send-symbolic",
        ActivityKind::Download => "document-save-symbolic",
        ActivityKind::Sync => "emblem-synchronizing-symbolic",
        ActivityKind::Rename => "document-edit-symbolic",
        ActivityKind::Move => "go-jump-symbolic",
        ActivityKind::CreateFolder => "folder-new-symbolic",
        ActivityKind::Trash | ActivityKind::DeleteForever | ActivityKind::EmptyTrash => {
            "user-trash-symbolic"
        }
        ActivityKind::Restore => "edit-undo-symbolic",
        ActivityKind::Share | ActivityKind::PublicLink => "emblem-shared-symbolic",
        ActivityKind::Unshare => "action-unavailable-symbolic",
    }
}

/// Format an epoch-seconds timestamp for the Activity feed, in local time.
fn activity_time(secs: i64) -> String {
    glib::DateTime::from_unix_local(secs)
        .and_then(|dt| dt.format("%b %-d, %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// A dim, non-interactive placeholder row for an empty section.
fn dim_row(text: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(text).build();
    row.add_css_class("dim-label");
    row.set_activatable(false);
    row
}

/// Open a URL in the user's default browser.
fn open_uri(url: &str) {
    if let Err(e) = gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>) {
        tracing::warn!("open uri {url}: {e}");
    }
}

// ---- per-node Share dialog ------------------------------------------------

/// The roles offered in the Share dialog's dropdowns, in index order.
const SHARE_ROLES: [&str; 3] = ["Viewer", "Editor", "Admin"];

/// Map a role dropdown index to the wire role string.
fn role_index_to_wire(idx: u32) -> &'static str {
    match idx {
        1 => "editor",
        2 => "admin",
        _ => "viewer",
    }
}

/// Map a wire role string to its dropdown index.
fn role_wire_to_index(role: &str) -> u32 {
    match role {
        "editor" => 1,
        "admin" => 2,
        _ => 0,
    }
}

/// The mutable state behind an open Share dialog, so the invite/role/link
/// handlers can rebuild the people and link sections after each change without
/// tearing the whole dialog down.
struct ShareDialog {
    ui: Rc<Ui>,
    /// The node's mountpoint-relative path — how every request addresses it.
    rel: String,
    people: adw::PreferencesGroup,
    link_group: adw::PreferencesGroup,
    people_rows: RefCell<Vec<gtk4::Widget>>,
    link_rows: RefCell<Vec<gtk4::Widget>>,
}

/// Open the per-node Share dialog: invite Proton/external users, manage who has
/// access and their roles, and create/copy/remove a public link.
fn open_share_dialog(ui: &Rc<Ui>, entry: &DirEntry) {
    if !*ui.mounted.borrow() {
        toast_error(ui, "Can't share", "Proton Drive isn't connected.");
        return;
    }
    let rel = entry_rel(ui, entry);

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Share", &entry.name)));
    toolbar.add_top_bar(&header);

    // Invite section: emails + role + optional message.
    let invite_group = adw::PreferencesGroup::builder()
        .title("Invite people")
        .description("Proton and non-Proton email addresses, separated by spaces or commas.")
        .build();
    let email_row = adw::EntryRow::builder().title("Email addresses").build();
    let role_model = gtk4::StringList::new(&SHARE_ROLES);
    let role_drop = gtk4::DropDown::builder()
        .model(&role_model)
        .selected(0)
        .valign(gtk4::Align::Center)
        .build();
    let role_wrap = adw::ActionRow::builder().title("Role").build();
    role_wrap.add_suffix(&role_drop);
    let message_row = adw::EntryRow::builder().title("Message (optional)").build();
    let invite_btn = gtk4::Button::builder()
        .label("Send Invitations")
        .halign(gtk4::Align::End)
        .margin_top(6)
        .build();
    invite_btn.add_css_class("suggested-action");
    let invite_wrap = adw::PreferencesRow::builder()
        .activatable(false)
        .child(&invite_btn)
        .build();
    invite_group.add(&email_row);
    invite_group.add(&role_wrap);
    invite_group.add(&message_row);
    invite_group.add(&invite_wrap);

    let people = adw::PreferencesGroup::builder()
        .title("People with access")
        .build();
    let link_group = adw::PreferencesGroup::builder()
        .title("Public link")
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&invite_group);
    content.append(&people);
    content.append(&link_group);
    let clamp = adw::Clamp::builder().child(&content).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&clamp)
        .build();
    toolbar.set_content(Some(&scroll));

    let dialog = adw::Dialog::builder()
        .title("Share")
        .content_width(480)
        .content_height(560)
        .child(&toolbar)
        .build();

    let state = Rc::new(ShareDialog {
        ui: ui.clone(),
        rel,
        people,
        link_group,
        people_rows: RefCell::new(Vec::new()),
        link_rows: RefCell::new(Vec::new()),
    });

    // Enter in either free-text field sends the invitations, no mouse needed.
    let btn = invite_btn.clone();
    email_row.connect_entry_activated(move |_| btn.emit_clicked());
    let btn = invite_btn.clone();
    message_row.connect_entry_activated(move |_| btn.emit_clicked());

    // Invite button.
    let state_inv = state.clone();
    invite_btn.connect_clicked(move |_| {
        let raw = email_row.text();
        let emails: Vec<String> = raw
            .split([',', ' ', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if emails.is_empty() {
            toast_error(&state_inv.ui, "Couldn't share", "Enter at least one email.");
            return;
        }
        let role = role_index_to_wire(role_drop.selected()).to_string();
        let msg = message_row.text().trim().to_string();
        let message = if msg.is_empty() { None } else { Some(msg) };
        let email_clear = email_row.clone();
        let msg_clear = message_row.clone();
        share_dialog_op(
            &state_inv,
            Request::ShareNode {
                path: state_inv.rel.clone(),
                emails,
                role,
                message,
            },
            "Invitations sent",
            "Couldn't send invitations",
            Some(Box::new(move || {
                email_clear.set_text("");
                msg_clear.set_text("");
            })),
        );
    });

    share_dialog_reload(&state);
    dialog.present(ui_window(ui).as_ref());
}

/// Re-fetch the node's share and rebuild the people + public-link sections.
fn share_dialog_reload(state: &Rc<ShareDialog>) {
    let rx = spawn_request(
        state.ui.dirs.control_socket(),
        Request::ListShare {
            path: state.rel.clone(),
        },
    );
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Share { entries, link })) => {
                repaint_share_people(&state, &entries);
                repaint_share_link(&state, link.as_ref());
            }
            Ok(Ok(Response::Error { message })) => {
                toast_error(&state.ui, "Couldn't load sharing", &message)
            }
            _ => toast_error(
                &state.ui,
                "Couldn't load sharing",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Rebuild the "People with access" rows from a fresh share listing.
fn repaint_share_people(state: &Rc<ShareDialog>, entries: &[ShareEntry]) {
    for row in state.people_rows.borrow_mut().drain(..) {
        state.people.remove(&row);
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    if entries.is_empty() {
        let row = dim_row("No one else has access yet.");
        state.people.add(&row);
        rows.push(row.upcast());
        *state.people_rows.borrow_mut() = rows;
        return;
    }
    for entry in entries {
        let subtitle = match entry.kind {
            ShareEntryKind::Member => "Member".to_string(),
            ShareEntryKind::ProtonInvite => "Invited (pending)".to_string(),
            ShareEntryKind::ExternalInvite => "Invited (external, pending)".to_string(),
        };
        let row = adw::ActionRow::builder()
            .title(&entry.email)
            .subtitle(&subtitle)
            .build();

        // External invites can't have their role changed; show it read-only.
        // Members and Proton invites get a role dropdown.
        if matches!(
            entry.kind,
            ShareEntryKind::Member | ShareEntryKind::ProtonInvite
        ) {
            let model = gtk4::StringList::new(&SHARE_ROLES);
            let drop = gtk4::DropDown::builder()
                .model(&model)
                .selected(role_wire_to_index(&entry.role))
                .valign(gtk4::Align::Center)
                .build();
            let state_role = state.clone();
            let id = entry.id.clone();
            let kind = entry.kind;
            drop.connect_selected_notify(move |d| {
                share_dialog_op(
                    &state_role,
                    Request::UpdateShareRole {
                        path: state_role.rel.clone(),
                        id: id.clone(),
                        kind,
                        role: role_index_to_wire(d.selected()).to_string(),
                    },
                    "Role updated",
                    "Couldn't update the role",
                    None,
                );
            });
            row.add_suffix(&drop);
        } else {
            let label = gtk4::Label::builder()
                .label(capitalize(&entry.role))
                .valign(gtk4::Align::Center)
                .build();
            label.add_css_class("dim-label");
            row.add_suffix(&label);
        }

        let remove = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove access")
            .valign(gtk4::Align::Center)
            .build();
        remove.add_css_class("flat");
        let state_rm = state.clone();
        let id = entry.id.clone();
        let kind = entry.kind;
        remove.connect_clicked(move |_| {
            share_dialog_op(
                &state_rm,
                Request::RemoveShareEntry {
                    path: state_rm.rel.clone(),
                    id: id.clone(),
                    kind,
                },
                "Access removed",
                "Couldn't remove access",
                None,
            );
        });
        row.add_suffix(&remove);

        state.people.add(&row);
        rows.push(row.upcast());
    }
    *state.people_rows.borrow_mut() = rows;
}

/// Rebuild the public-link section: an existing link (copy / remove) or a
/// create control.
fn repaint_share_link(state: &Rc<ShareDialog>, link: Option<&PublicLinkInfo>) {
    for row in state.link_rows.borrow_mut().drain(..) {
        state.link_group.remove(&row);
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();

    match link {
        Some(link) => {
            let url = link.url.clone().unwrap_or_default();
            let subtitle = if link.has_password {
                format!("Anyone with the link ({}) · password-protected", link.role)
            } else {
                format!("Anyone with the link ({})", link.role)
            };
            let row = adw::ActionRow::builder()
                .title(if url.is_empty() { "Public link" } else { &url })
                .subtitle(&subtitle)
                .build();
            row.add_css_class("property");

            if !url.is_empty() {
                let copy = gtk4::Button::builder()
                    .icon_name("edit-copy-symbolic")
                    .tooltip_text("Copy link")
                    .valign(gtk4::Align::Center)
                    .build();
                copy.add_css_class("flat");
                let state_copy = state.clone();
                let url_copy = url.clone();
                copy.connect_clicked(move |btn| {
                    btn.clipboard().set_text(&url_copy);
                    toast(&state_copy.ui, "Link copied");
                });
                row.add_suffix(&copy);
            }

            let remove = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove link")
                .valign(gtk4::Align::Center)
                .build();
            remove.add_css_class("flat");
            let state_rm = state.clone();
            let id = link.id.clone();
            remove.connect_clicked(move |_| {
                share_dialog_op(
                    &state_rm,
                    Request::RemovePublicLink {
                        path: state_rm.rel.clone(),
                        id: id.clone(),
                    },
                    "Public link removed",
                    "Couldn't remove the link",
                    None,
                );
            });
            row.add_suffix(&remove);

            state.link_group.add(&row);
            rows.push(row.upcast());
        }
        None => {
            let role_model = gtk4::StringList::new(&["Viewer", "Editor"]);
            let role_drop = gtk4::DropDown::builder()
                .model(&role_model)
                .selected(0)
                .valign(gtk4::Align::Center)
                .build();
            let role_row = adw::ActionRow::builder().title("Link role").build();
            role_row.add_suffix(&role_drop);
            let pw_row = adw::PasswordEntryRow::builder()
                .title("Password (optional)")
                .build();
            let create = gtk4::Button::builder()
                .label("Create Public Link")
                .halign(gtk4::Align::End)
                .margin_top(6)
                .build();
            create.add_css_class("suggested-action");
            let create_wrap = adw::PreferencesRow::builder()
                .activatable(false)
                .child(&create)
                .build();

            // Enter in the password field creates the link.
            let btn = create.clone();
            pw_row.connect_entry_activated(move |_| btn.emit_clicked());

            let state_c = state.clone();
            let pw_for = pw_row.clone();
            create.connect_clicked(move |_| {
                let role = if role_drop.selected() == 1 {
                    "editor"
                } else {
                    "viewer"
                }
                .to_string();
                let pw = pw_for.text().to_string();
                let password = if pw.is_empty() { None } else { Some(pw) };
                share_dialog_create_link(&state_c, role, password);
            });

            state.link_group.add(&role_row);
            state.link_group.add(&pw_row);
            state.link_group.add(&create_wrap);
            rows.push(role_row.upcast());
            rows.push(pw_row.upcast());
            rows.push(create_wrap.upcast());
        }
    }
    *state.link_rows.borrow_mut() = rows;
}

/// Create a public link, then reload the dialog so the copy/remove controls
/// replace the create form (and the freshly minted URL is shown).
fn share_dialog_create_link(state: &Rc<ShareDialog>, role: String, password: Option<String>) {
    let rx = spawn_request(
        state.ui.dirs.control_socket(),
        Request::CreatePublicLink {
            path: state.rel.clone(),
            role,
            password,
            expires: None,
        },
    );
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::PublicLink { .. })) => {
                toast(&state.ui, "Public link created");
                share_dialog_reload(&state);
            }
            Ok(Ok(Response::Error { message })) => {
                toast_error(&state.ui, "Couldn't create the link", &message)
            }
            _ => toast_error(
                &state.ui,
                "Couldn't create the link",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Run a Share-dialog mutation, then reload the dialog on success. `on_success`
/// runs an extra UI tweak (e.g. clearing the invite fields) before the reload.
fn share_dialog_op(
    state: &Rc<ShareDialog>,
    req: Request,
    done: &'static str,
    failed: &'static str,
    on_success: Option<Box<dyn Fn()>>,
) {
    let rx = spawn_request(state.ui.dirs.control_socket(), req);
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => {
                if let Some(cb) = on_success {
                    cb();
                }
                toast(&state.ui, done);
                share_dialog_reload(&state);
            }
            Ok(Ok(Response::Error { message })) => toast_error(&state.ui, failed, &message),
            _ => {
                // A role dropdown that failed is now out of sync with the server;
                // reload to snap it back.
                toast_error(&state.ui, failed, "The mount service didn't respond.");
                share_dialog_reload(&state);
            }
        }
    });
}

/// Uppercase the first character of a role word for read-only display.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// One day-section of the photos timeline: a heading plus the photos captured
/// that day, in timeline order. Built from the flat [`Ui::gallery_model`] by
/// [`group_photos`] and rendered as one [`gtk4::ListView`] row.
struct PhotoGroup {
    /// "Today", "Yesterday", or e.g. "3 June 2026".
    heading: String,
    photos: Vec<PhotoItem>,
}

/// The widgets [`build_gallery_page`] hands back to [`build_window`].
struct GalleryWidgets {
    /// Flat, newest-first list of every loaded photo. Backs the lightbox's
    /// prev/next navigation; the visible sections are derived from it.
    model: gio::ListStore,
    /// Day sections rendered by the ListView, derived from `model`.
    groups: gio::ListStore,
    /// Swaps between the timeline and the empty/loading/error status page.
    content: gtk4::Stack,
    status: adw::StatusPage,
    subtitle: gtk4::Label,
    more: gtk4::Button,
    list: gtk4::ListView,
    scroll: gtk4::ScrolledWindow,
    retry: gtk4::Button,
    upload: gtk4::Button,
    refresh: gtk4::Button,
    /// The All / Photos / Videos / Raw filter toggles, in that order (index maps
    /// to [`kind_for_tab`]).
    tabs: [gtk4::ToggleButton; 4],
    /// The date-jump dropdown, populated with the timeline's months.
    dates: gtk4::DropDown,
}

/// The Photos page: a [`gtk4::ListView`] of day sections, each a heading over
/// that day's photos laid out as justified rows (see [`justify_rows`]) — every
/// photo at its own aspect ratio, every row filled edge to edge.
///
/// A ListView of sections rather than one flat GridView because GTK's grid has no
/// row headers and forces square cells: the justified rows and the date headings
/// both need per-row structure, and the ListView only realises the sections on
/// screen, which is what keeps a 10,000-photo timeline cheap. The factory is
/// installed by [`wire_gallery`], which has the [`Ui`] the tiles need (zoom
/// level, thumbnail cache, click-to-open).
fn build_gallery_page() -> (gtk4::Widget, GalleryWidgets) {
    let model = gio::ListStore::new::<BoxedAnyObject>();
    let groups = gio::ListStore::new::<BoxedAnyObject>();

    let selection = gtk4::NoSelection::new(Some(groups.clone()));
    let list = gtk4::ListView::builder()
        .model(&selection)
        .single_click_activate(false)
        .build();
    list.add_css_class("gallery-sections");

    // Shown only when a load failed because the mount is down; restarts it.
    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);

    let status = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    // Kept as an explicit fallback: the timeline also pages itself in as the
    // scroll nears the bottom (see [`wire_gallery`]), so reaching this button at
    // all is unusual.
    let more = gtk4::Button::builder()
        .label("Load more")
        .halign(gtk4::Align::Center)
        .build();
    more.add_css_class("pill");
    more.set_visible(false);

    // Horizontal scrolling is never wanted: rows are justified to the viewport
    // width, and a stray hscrollbar would fight the layout.
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&list)
        .build();

    let title_label = gtk4::Label::builder()
        .label("Photos")
        .halign(gtk4::Align::Start)
        .build();
    title_label.add_css_class("title-2");

    let subtitle = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    subtitle.add_css_class("dim-label");
    subtitle.add_css_class("caption");

    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title_label);
    titles.append(&subtitle);

    let upload = gtk4::Button::builder()
        .label("Upload")
        .icon_name("list-add-symbolic")
        .valign(gtk4::Align::Center)
        .build();
    upload.add_css_class("pill");
    upload.add_css_class("suggested-action");
    let refresh = refresh_button();

    let header_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header_box.append(&titles);
    header_box.append(&refresh);
    header_box.append(&upload);

    // All / Photos / Videos / Raw filter. Linked toggles acting as one segmented
    // control: exactly one is active, and flipping it reloads the timeline
    // filtered to that kind (wired in [`wire_gallery`]). Labels gain live counts
    // once a page lands.
    let tab_labels = ["All", "Photos", "Videos", "Raw"];
    let tabs: [gtk4::ToggleButton; 4] = std::array::from_fn(|i| {
        gtk4::ToggleButton::builder()
            .label(tab_labels[i])
            .active(i == 0)
            .build()
    });
    // Group the toggles so they behave as a radio set: chaining each to the first
    // is what GTK turns into mutual exclusion.
    for btn in &tabs[1..] {
        btn.set_group(Some(&tabs[0]));
    }
    let tab_group = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    tab_group.add_css_class("linked");
    for btn in &tabs {
        btn.add_css_class("pill");
        tab_group.append(btn);
    }

    // Date jump: "All dates" plus a row per month, filled in once the timeline's
    // months are known (see [`refresh_photo_months`]). Pushed to the far end of
    // the filter row, opposite the kind toggles.
    let dates = gtk4::DropDown::from_strings(&["All dates"]);
    dates.add_css_class("pill");
    dates.set_tooltip_text(Some("Jump to a month"));

    let filter_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    filter_bar.append(&tab_group);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    filter_bar.append(&spacer);
    filter_bar.append(&dates);

    // The timeline (plus its pager) or the status page, never both.
    let timeline = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    timeline.append(&scroll);
    timeline.append(&more);

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&timeline, Some("timeline"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header_box);
    inner.append(&filter_bar);
    inner.append(&content);

    (
        inner.upcast(),
        GalleryWidgets {
            model,
            groups,
            content,
            status,
            subtitle,
            more,
            list,
            scroll,
            retry,
            upload,
            refresh,
            tabs,
            dates,
        },
    )
}

/// The [`PhotoKind`] filter a gallery tab index selects: index 0 is All (no
/// filter), then Photos / Videos / Raw. Index-aligned with the toggle array.
fn kind_for_tab(index: usize) -> Option<PhotoKind> {
    match index {
        1 => Some(PhotoKind::Photo),
        2 => Some(PhotoKind::Video),
        3 => Some(PhotoKind::Raw),
        _ => None,
    }
}

/// The `[from, to)` epoch-second window of a local calendar month, or `None` if
/// the date is somehow unrepresentable. Computed with glib so month rollover and
/// the local timezone (matching the daemon's month buckets) are handled for us.
fn month_range(year: i32, month: i32) -> Option<(i64, i64)> {
    let start = glib::DateTime::from_local(year, month, 1, 0, 0, 0.0).ok()?;
    let end = start.add_months(1).ok()?;
    Some((start.to_unix(), end.to_unix()))
}

/// English month names, indexed 1..=12.
const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];

/// Rebuild the date-jump dropdown for the active kind: ask the daemon which
/// months the timeline spans and turn them into "Month YYYY (count)" rows, each
/// remembering the window it jumps to. Resets the selection to "All dates" — the
/// caller pairs this with a fresh timeline load. Off the UI thread; a failure
/// just leaves the dropdown as it was.
fn refresh_photo_months(ui: &Rc<Ui>) {
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::PhotoMonths {
            kind: ui.gallery_kind.get(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let Ok(Ok(Response::PhotoMonths { months })) = rx.recv().await else {
            return;
        };
        let mut labels = vec!["All dates".to_string()];
        let mut ranges: Vec<Option<(i64, i64)>> = vec![None];
        for m in months {
            let name = MONTH_NAMES.get((m.month - 1) as usize).copied().unwrap_or("?");
            labels.push(format!("{name} {} ({})", m.year, m.count));
            ranges.push(month_range(m.year, m.month));
        }
        let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

        // Repopulating resets the selection to 0, which would otherwise fire the
        // handler and reload; suppress that — the caller is already reloading.
        ui.gallery_date_suppress.set(true);
        ui.gallery_dates
            .set_model(Some(&gtk4::StringList::new(&label_refs)));
        ui.gallery_dates.set_selected(0);
        *ui.gallery_date_ranges.borrow_mut() = ranges;
        ui.gallery_date_suppress.set(false);
    });
}

/// Label the filter toggles with the whole-timeline `(photos, videos, raw)`
/// counts, so a glance shows how much sits behind each tab. A tab with nothing
/// behind it is disabled — you can't filter to an empty set — but the currently
/// selected one stays clickable so you can always switch back off it.
fn update_gallery_tabs(ui: &Rc<Ui>, counts: (usize, usize, usize)) {
    let (photos, videos, raw) = counts;
    let totals = [photos + videos + raw, photos, videos, raw];
    for (index, tab) in ui.gallery_tabs.iter().enumerate() {
        let name = ["All", "Photos", "Videos", "Raw"][index];
        let n = totals[index];
        tab.set_label(&format!("{name} {n}"));
        tab.set_sensitive(n > 0 || tab.is_active());
    }
}

/// Wire the gallery: install the section factory, the zoom gestures, the pager
/// and the upload button. Activating a thumbnail downloads the photo and opens it
/// in the in-app lightbox.
fn wire_gallery(ui: &Rc<Ui>, list: &gtk4::ListView, scroll: &gtk4::ScrolledWindow) {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let section = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        section.set_margin_bottom(16);
        let heading = gtk4::Label::builder().halign(gtk4::Align::Start).build();
        heading.add_css_class("heading");
        heading.add_css_class("gallery-day");
        section.append(&heading);
        item.set_child(Some(&section));
        item.set_activatable(false);
    });

    let ui_bind = ui.clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let section = item.child().and_downcast::<gtk4::Box>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let group = obj.borrow::<PhotoGroup>();

        let heading = section.first_child().and_downcast::<gtk4::Label>().unwrap();
        heading.set_label(&group.heading);

        fill_section(&ui_bind, &section, &group.photos);
        // Remember the realised section so a learned aspect ratio or a resize can
        // re-justify it in place, without rebuilding the ListStore (which would
        // yank the scroll position back to the top).
        ui_bind
            .gallery_bound
            .borrow_mut()
            .insert(item.position(), section);
    });

    // ListView recycles section widgets, so a scrolled-away day must give up its
    // claim on the widgets — otherwise a thumbnail landing late would paint into
    // a tile that now shows a different day.
    let ui_unbind = ui.clone();
    factory.connect_unbind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        ui_unbind
            .gallery_bound
            .borrow_mut()
            .remove(&item.position());
        if let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() {
            let group = obj.borrow::<PhotoGroup>();
            let mut wanted = ui_unbind.thumb_wanted.borrow_mut();
            for photo in &group.photos {
                wanted.remove(&photo.uid);
            }
        }
    });
    list.set_factory(Some(&factory));

    // Rows are justified to the content width, so a resize re-justifies whatever
    // is on screen (offscreen sections pick the new width up when they bind).
    let ui_width = ui.clone();
    list.connect_notify_local(Some("width"), move |list, _| {
        let width = list.width();
        if width > 0 && width != ui_width.gallery_width.get() {
            ui_width.gallery_width.set(width);
            schedule_relayout(&ui_width);
        }
    });

    // Page the timeline in as the scroll nears the end, so "load more" is a
    // fallback button rather than something the user has to hunt for.
    let ui_scroll = ui.clone();
    scroll.vadjustment().connect_value_changed(move |adj| {
        let near_end = adj.value() + adj.page_size() >= adj.upper() - adj.page_size() * 0.5;
        if near_end && ui_scroll.gallery_more.is_visible() && ui_scroll.gallery_more.is_sensitive()
        {
            load_gallery(&ui_scroll, true);
        }
    });

    // Ctrl+scroll zoom. Capture phase so the ScrolledWindow doesn't eat the event
    // and scroll the page out from under the gesture.
    let zoom_scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
    zoom_scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let ui_zoom = ui.clone();
    zoom_scroll.connect_scroll(move |controller, _dx, dy| {
        if !controller
            .current_event_state()
            .contains(gtk4::gdk::ModifierType::CONTROL_MASK)
            || dy == 0.0
        {
            return glib::Propagation::Proceed;
        }
        // Scroll up (negative dy) zooms in, i.e. bigger tiles.
        zoom_gallery(&ui_zoom, if dy < 0.0 { TILE_STEP } else { -TILE_STEP });
        glib::Propagation::Stop
    });
    scroll.add_controller(zoom_scroll);

    // Ctrl+plus / Ctrl+minus / Ctrl+0, the keyboard equivalents.
    let zoom_keys = gtk4::EventControllerKey::new();
    let ui_keys = ui.clone();
    zoom_keys.connect_key_pressed(move |_, key, _code, state| {
        if !state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
            return glib::Propagation::Proceed;
        }
        match key.name().as_deref() {
            Some("plus" | "equal" | "KP_Add") => zoom_gallery(&ui_keys, TILE_STEP),
            Some("minus" | "KP_Subtract") => zoom_gallery(&ui_keys, -TILE_STEP),
            Some("0" | "KP_0") => set_gallery_tile(&ui_keys, TILE_DEFAULT),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    list.add_controller(zoom_keys);

    let ui_more = ui.clone();
    ui.gallery_more.clone().connect_clicked(move |_| {
        load_gallery(&ui_more, true);
    });

    // Filter toggles: flipping to a tab reloads the timeline filtered to that
    // kind. Only the button being switched *on* acts — the group also fires a
    // `toggled` for the one switching off, which this skips — and a redundant
    // toggle to the already-current kind is a no-op.
    for (index, tab) in ui.gallery_tabs.iter().enumerate() {
        let ui_tab = ui.clone();
        tab.connect_toggled(move |btn| {
            if !btn.is_active() {
                return;
            }
            let kind = kind_for_tab(index);
            if ui_tab.gallery_kind.get() == kind {
                return;
            }
            ui_tab.gallery_kind.set(kind);
            // A different kind has a different set of months; clearing the active
            // window makes the reload rebuild the date jump for the new kind.
            ui_tab.gallery_range.set(None);
            load_gallery(&ui_tab, false);
        });
    }

    // Date jump: selecting a month loads that window; "All dates" (row 0) clears
    // it. Skipped while the model is being repopulated (see `gallery_date_suppress`).
    let ui_dates = ui.clone();
    ui.gallery_dates.connect_selected_notify(move |dd| {
        if ui_dates.gallery_date_suppress.get() {
            return;
        }
        let range = ui_dates
            .gallery_date_ranges
            .borrow()
            .get(dd.selected() as usize)
            .copied()
            .flatten();
        if ui_dates.gallery_range.get() == range {
            return;
        }
        ui_dates.gallery_range.set(range);
        load_gallery(&ui_dates, false);
    });

    let ui_upload = ui.clone();
    ui.gallery_upload.connect_clicked(move |_| {
        let dialog = gtk4::FileDialog::builder()
            .title("Select Photo to Upload")
            .build();

        let filter = gtk4::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_mime_type("image/*");
        let filters = gio::ListStore::new::<gtk4::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));

        let ui = ui_upload.clone();
        let parent_win = ui.stack.root().and_downcast::<gtk4::Window>();
        dialog.open(parent_win.as_ref(), gio::Cancellable::NONE, move |res| {
            if let Ok(file) = res
                && let Some(path) = file.path()
            {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("photo.jpg")
                    .to_string();
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!("Failed to read photo: {e}");
                        return;
                    }
                };
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("jpg")
                    .to_lowercase();
                let media_type = match ext.as_str() {
                    "png" => "image/png",
                    "gif" => "image/gif",
                    "webp" => "image/webp",
                    "tiff" | "tif" => "image/tiff",
                    _ => "image/jpeg",
                };
                let capture_time = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);

                ui.busy_begin();
                let rx = spawn_request(
                    ui.dirs.control_socket(),
                    Request::UploadPhoto {
                        name,
                        media_type: media_type.to_string(),
                        bytes,
                        capture_time,
                    },
                );
                let ui_clone = ui.clone();
                glib::spawn_future_local(async move {
                    let res = rx.recv().await;
                    ui_clone.busy_end();
                    match res {
                        Ok(Ok(Response::Ok { message })) => {
                            tracing::info!("Photo uploaded: {message}");
                            load_gallery(&ui_clone, false);
                            toast(&ui_clone, "Photo uploaded");
                        }
                        Ok(Ok(Response::Error { message })) => {
                            toast_error(&ui_clone, "Couldn't upload photo", &message);
                        }
                        _ => {
                            toast_error(
                                &ui_clone,
                                "Couldn't upload photo",
                                "The mount service didn't respond.",
                            );
                        }
                    }
                });
            }
        });
    });
}

/// One tile in a justified row: the photo, and the pixel size it was justified to.
struct Tile {
    photo: PhotoItem,
    width: i32,
    height: i32,
}

/// Break one day's photos into rows that each fill `width` exactly, at heights
/// near [`Ui::gallery_tile`] — the layout every modern photo gallery uses, and
/// the reason nothing here has to be cropped: each tile keeps its own aspect
/// ratio, and it is the row *height* that flexes to make the widths add up.
///
/// Greedy, one pass: keep adding photos to the row: the more photos share it, the
/// shorter it has to be to fit. The moment that height drops to the target, the
/// row is as full as it should be and is emitted. The trailing row keeps the
/// target height instead of being stretched, so a day with three photos doesn't
/// blow them up to fill a screen-wide row.
///
/// Photos whose thumbnail hasn't been decoded yet are laid out at
/// [`RATIO_UNKNOWN`]; [`Ui::store_texture`] reports the real ratio when it lands
/// and the section is re-justified in place.
fn justify_rows(ui: &Rc<Ui>, photos: &[PhotoItem], width: i32) -> Vec<Vec<Tile>> {
    let ratios: Vec<f64> = photos.iter().map(|photo| ui.ratio(&photo.uid)).collect();
    let target = f64::from(ui.gallery_tile.get());
    let plan = plan_rows(&ratios, target, f64::from(width));

    let mut photos = photos.iter();
    plan.into_iter()
        .map(|row| {
            row.into_iter()
                .filter_map(|(width, height)| {
                    photos.next().map(|photo| Tile {
                        photo: photo.clone(),
                        width,
                        height,
                    })
                })
                .collect()
        })
        .collect()
}

/// The row-packing math behind [`justify_rows`], over nothing but aspect ratios:
/// takes each photo's ratio (w/h) in order and returns the pixel `(width,
/// height)` of every tile, grouped into rows.
fn plan_rows(ratios: &[f64], target: f64, width: f64) -> Vec<Vec<(i32, i32)>> {
    let avail = width.max(f64::from(TILE_MIN));
    let gap = f64::from(TILE_GAP);

    // The height a row of `n` photos with `sum_ratio` total ratio must take for
    // its widths (plus gaps) to add up to exactly `avail`.
    let row_height = |sum_ratio: f64, n: usize| {
        let gaps = gap * (n.saturating_sub(1)) as f64;
        (avail - gaps) / sum_ratio
    };

    let mut rows: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut row: Vec<f64> = Vec::new();
    let mut sum_ratio = 0.0;

    for ratio in ratios {
        let ratio = ratio.clamp(RATIO_MIN, RATIO_MAX);
        row.push(ratio);
        sum_ratio += ratio;

        let height = row_height(sum_ratio, row.len());
        if height <= target {
            rows.push(size_row(&row, height, avail, true));
            row.clear();
            sum_ratio = 0.0;
        }
    }
    if !row.is_empty() {
        let height = row_height(sum_ratio, row.len()).min(target);
        rows.push(size_row(&row, height, avail, false));
    }
    rows
}

/// Size one row's tiles at `height`. A `justified` row is nudged so its widths
/// plus gaps hit `avail` exactly — rounding each width independently leaves a
/// few px of ragged right edge, so the last tile absorbs the remainder.
fn size_row(ratios: &[f64], height: f64, avail: f64, justified: bool) -> Vec<(i32, i32)> {
    let height = height.round().max(1.0);
    let mut sizes: Vec<(i32, i32)> = ratios
        .iter()
        .map(|ratio| ((ratio * height).round().max(1.0) as i32, height as i32))
        .collect();

    if justified {
        let gaps = TILE_GAP * (sizes.len().saturating_sub(1)) as i32;
        let used: i32 = sizes.iter().rev().skip(1).map(|(width, _)| *width).sum();
        if let Some(last) = sizes.last_mut() {
            last.0 = (avail as i32 - gaps - used).max(1);
        }
    }
    sizes
}

/// (Re)build a bound day-section's tiles: justify this day's photos to the
/// current content width and hand each tile whatever thumbnail is already in
/// memory, queueing the rest. Replaces the section's rows in place, leaving the
/// heading — so a re-justify never touches the ListView's model or scroll.
fn fill_section(ui: &Rc<Ui>, section: &gtk4::Box, photos: &[PhotoItem]) {
    let Some(heading) = section.first_child() else {
        return;
    };
    while let Some(old) = heading.next_sibling() {
        section.remove(&old);
    }

    let width = gallery_width(ui);
    let rows = gtk4::Box::new(gtk4::Orientation::Vertical, TILE_GAP);
    for row in justify_rows(ui, photos, width) {
        let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, TILE_GAP);
        for tile in row {
            row_box.append(&photo_tile(ui, tile));
        }
        rows.append(&row_box);
    }
    section.append(&rows);
    schedule_thumbs(ui);
}

/// The width justified rows are laid out to: the ListView's own width, less a
/// couple of px so a rounding error can't push a row into a horizontal overflow.
/// Falls back to a sane guess before the first allocation.
fn gallery_width(ui: &Rc<Ui>) -> i32 {
    match ui.gallery_width.get() {
        0 => 900,
        w => (w - 2).max(TILE_MIN),
    }
}

/// One photo tile: a fixed-size button wrapping the thumbnail, with the capture
/// time revealed on hover over a bottom scrim. A button (rather than a bare
/// picture) so the tile is focusable, keyboard-activatable and gets hover feedback
/// for free.
///
/// The picture sits in an overlay over a placeholder, so a tile is never a hole:
/// until the thumbnail lands it shows a dim card, and a photo that can never have
/// one keeps an image glyph instead of an empty rectangle.
fn photo_tile(ui: &Rc<Ui>, tile: Tile) -> gtk4::Button {
    let picture = gtk4::Picture::builder()
        // The tile is exactly the thumbnail's own aspect ratio, so Cover scales
        // it and crops nothing; it only bites during the brief window where the
        // ratio is still a guess, and the re-justify then fixes the tile.
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .build();

    let placeholder = gtk4::Image::builder()
        .icon_name("image-x-generic-symbolic")
        .pixel_size(24)
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .build();
    placeholder.add_css_class("photo-placeholder");

    // The capture time, on a gradient that only exists while the pointer is over
    // the tile — legible over any photo, invisible the rest of the time.
    let caption = gtk4::Label::builder()
        // Fill horizontally so the scrim spans the tile; the text itself stays
        // left-aligned inside it.
        .halign(gtk4::Align::Fill)
        .valign(gtk4::Align::End)
        .xalign(0.0)
        .label(short_capture_time(tile.photo.capture_time))
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    caption.add_css_class("photo-caption");

    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&placeholder));
    overlay.add_overlay(&picture);
    overlay.add_overlay(&caption);

    // A video reads as a video at a glance: a play glyph centred over the poster
    // thumbnail. Kept above the caption scrim so it stays legible on hover.
    let is_video = tile.photo.kind == PhotoKind::Video;
    if is_video {
        let badge = gtk4::Image::builder()
            .icon_name("media-playback-start-symbolic")
            .pixel_size(28)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        badge.add_css_class("photo-video-badge");
        overlay.add_overlay(&badge);
    }

    let button = gtk4::Button::builder()
        .child(&overlay)
        .width_request(tile.width)
        .height_request(tile.height)
        .tooltip_text(format_capture_time(tile.photo.capture_time))
        .build();
    button.add_css_class("photo-tile");
    button.add_css_class("flat");
    // Clip the thumbnail to the tile's rounded corners.
    button.set_overflow(gtk4::Overflow::Hidden);

    want_thumb(ui, &tile.photo, &picture);

    // A still opens in the in-app lightbox; a video can't render there, so it
    // downloads and hands off to an external player instead.
    let ui_open = ui.clone();
    let uid = tile.photo.uid.clone();
    button.connect_clicked(move |_| {
        if is_video {
            play_video(&ui_open, uid.clone());
        } else {
            open_photo_viewer(&ui_open, uid.clone());
        }
    });
    button
}

/// Give `picture` its thumbnail: straight from the texture cache when it's there,
/// otherwise register the tile as waiting and get the thumbnail moving — decoding
/// it if the daemon already had it cached on disk, or asking the daemon for it.
///
/// This is what makes the gallery on-demand: only tiles the ListView actually
/// realises ever ask for an image.
fn want_thumb(ui: &Rc<Ui>, photo: &PhotoItem, picture: &gtk4::Picture) {
    if let Some(texture) = ui.photo_tex.borrow().get(&photo.uid) {
        picture.set_paintable(Some(texture));
        return;
    }
    // No thumbnail will ever come for this one — not from the server, and not
    // from the daemon's own scaling of the file. The tile keeps its placeholder
    // glyph, and stays clickable: the full photo may still open fine.
    if ui.photo_nothumb.borrow().contains(&photo.uid) {
        return;
    }

    ui.thumb_wanted
        .borrow_mut()
        .insert(photo.uid.clone(), picture.clone());

    match photo.thumb_path.as_deref() {
        Some(path) => {
            ui.decode_queue
                .borrow_mut()
                .push_back((photo.uid.clone(), path.to_string()));
            schedule_decode(ui);
        }
        None => {
            let mut queue = ui.thumb_queue.borrow_mut();
            if !queue.contains(&photo.uid) {
                queue.push_back(photo.uid.clone());
            }
        }
    }
}

/// Come back for thumbnails the daemon is still generating — it is downloading
/// each photo's full file to scale it, which takes far longer than a batch. The
/// tiles keep their placeholder until then, and a tile that has scrolled away is
/// dropped by [`flush_thumbs`] like any other queued uid.
fn retry_pending_thumbs(ui: &Rc<Ui>, uids: Vec<String>) {
    let ui = ui.clone();
    glib::timeout_add_local_once(THUMB_RETRY, move || {
        {
            let mut queue = ui.thumb_queue.borrow_mut();
            for uid in uids {
                if !queue.contains(&uid) {
                    queue.push_back(uid);
                }
            }
        }
        schedule_thumbs(&ui);
    });
}

/// Ask the daemon for the queued thumbnails after a short pause, so a fast scroll
/// coalesces into one batch per settle rather than one per row it flew past.
fn schedule_thumbs(ui: &Rc<Ui>) {
    if ui.thumb_queue.borrow().is_empty() || ui.thumb_inflight.get() {
        return;
    }
    if let Some(id) = ui.thumb_source.borrow_mut().take() {
        id.remove();
    }
    let ui_flush = ui.clone();
    let source = glib::timeout_add_local_once(THUMB_DEBOUNCE, move || {
        ui_flush.thumb_source.borrow_mut().take();
        flush_thumbs(&ui_flush);
    });
    *ui.thumb_source.borrow_mut() = Some(source);
}

/// Send one [`Request::PhotoThumbs`] batch for the tiles still on screen. Queued
/// uids whose tile has scrolled away are dropped rather than fetched: the point
/// of the batch is what the user is looking at *now*.
fn flush_thumbs(ui: &Rc<Ui>) {
    if ui.thumb_inflight.get() {
        return;
    }
    let uids: Vec<String> = {
        let mut queue = ui.thumb_queue.borrow_mut();
        let wanted = ui.thumb_wanted.borrow();
        let mut batch = Vec::new();
        while batch.len() < THUMB_BATCH {
            let Some(uid) = queue.pop_front() else { break };
            if wanted.contains_key(&uid) {
                batch.push(uid);
            }
        }
        batch
    };
    if uids.is_empty() {
        return;
    }

    ui.thumb_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::PhotoThumbs { uids });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.thumb_inflight.set(false);
        match result {
            Ok(Ok(Response::Thumbs { items })) => {
                let mut decode = ui.decode_queue.borrow_mut();
                let mut nothumb = ui.photo_nothumb.borrow_mut();
                let mut pending = Vec::new();
                for item in items {
                    match item.path {
                        Some(path) => decode.push_back((item.uid, path)),
                        // The daemon is making this one itself, from the photo's
                        // full file — that takes a download, so come back for it.
                        None if item.pending => pending.push(item.uid),
                        // No thumbnail exists and none can be made: remember that,
                        // so scrolling past the tile doesn't re-ask forever.
                        None => {
                            nothumb.insert(item.uid);
                        }
                    }
                }
                drop((decode, nothumb));
                schedule_decode(&ui);
                if !pending.is_empty() {
                    retry_pending_thumbs(&ui, pending);
                }
            }
            // A thumbnail that doesn't arrive is not worth a toast — the tile just
            // stays a placeholder, and the next scroll past it tries again.
            Ok(Ok(Response::Error { message })) => {
                tracing::debug!("photo thumbs failed: {message}")
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => tracing::debug!("photo thumbs: no reply"),
        }
        // Whatever the batch did, more tiles may have queued up behind it.
        schedule_thumbs(&ui);
    });
}

/// Decode queued thumbnails into textures on an idle callback, a few per pass, so
/// a big batch fills in progressively instead of freezing the scroll for the
/// length of the whole decode.
fn schedule_decode(ui: &Rc<Ui>) {
    if ui.decode_idle.get() || ui.decode_queue.borrow().is_empty() {
        return;
    }
    ui.decode_idle.set(true);

    let ui = ui.clone();
    glib::idle_add_local(move || {
        let batch: Vec<(String, String)> = {
            let mut queue = ui.decode_queue.borrow_mut();
            (0..4).filter_map(|_| queue.pop_front()).collect()
        };
        let mut relayout = false;
        for (uid, path) in batch {
            let texture = match gtk4::gdk::Texture::from_filename(&path) {
                Ok(texture) => texture,
                Err(e) => {
                    tracing::debug!("cannot decode thumbnail {path}: {e}");
                    ui.photo_nothumb.borrow_mut().insert(uid);
                    continue;
                }
            };
            // A ratio we hadn't seen means the tile was sized against a guess.
            relayout |= ui.store_texture(&uid, texture.clone());
            if let Some(picture) = ui.thumb_wanted.borrow_mut().remove(&uid) {
                picture.set_paintable(Some(&texture));
            }
        }
        if relayout {
            schedule_relayout(&ui);
        }

        if ui.decode_queue.borrow().is_empty() {
            ui.decode_idle.set(false);
            ui.save_ratios();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// Re-justify the sections on screen shortly. Debounced, because the triggers
/// (a window resize, a zoom step, a burst of decoded thumbnails) all arrive in
/// floods and only the final state matters.
fn schedule_relayout(ui: &Rc<Ui>) {
    if let Some(id) = ui.relayout_source.borrow_mut().take() {
        id.remove();
    }
    let ui_relayout = ui.clone();
    let source = glib::timeout_add_local_once(RELAYOUT_DEBOUNCE, move || {
        ui_relayout.relayout_source.borrow_mut().take();
        relayout_gallery(&ui_relayout);
    });
    *ui.relayout_source.borrow_mut() = Some(source);
}

/// Rebuild the tiles of the day sections currently on screen, at the current
/// width, zoom and set of known aspect ratios. Sections that are *not* realised
/// need no work: they justify themselves against the current state when the
/// ListView binds them.
fn relayout_gallery(ui: &Rc<Ui>) {
    let bound: Vec<(u32, gtk4::Box)> = ui
        .gallery_bound
        .borrow()
        .iter()
        .map(|(pos, section)| (*pos, section.clone()))
        .collect();
    for (pos, section) in bound {
        let Some(obj) = ui.gallery_groups.item(pos) else {
            continue;
        };
        let Some(boxed) = obj.downcast_ref::<BoxedAnyObject>() else {
            continue;
        };
        let photos = boxed.borrow::<PhotoGroup>().photos.clone();
        fill_section(ui, &section, &photos);
    }
}

/// Step the row height by `delta` px and re-justify, clamped to the zoom range.
fn zoom_gallery(ui: &Rc<Ui>, delta: i32) {
    set_gallery_tile(ui, ui.gallery_tile.get() + delta);
}

/// Set the row height (clamped) and re-justify the visible sections at it.
fn set_gallery_tile(ui: &Rc<Ui>, tile: i32) {
    let tile = tile.clamp(TILE_MIN, TILE_MAX);
    if tile == ui.gallery_tile.get() {
        return;
    }
    ui.gallery_tile.set(tile);
    schedule_relayout(ui);
}

/// Rebuild the day sections from the flat photo model. The timeline arrives
/// newest-first, so photos of the same day are already contiguous — one pass
/// splits them.
///
/// The groups are diffed into the existing store rather than replacing it: a
/// "load more" only really changes the last day (the one the new page continues)
/// and appends after it, and clearing the store instead would scroll the user
/// back to the top of the timeline at the exact moment they asked for more.
fn repaint_gallery(ui: &Rc<Ui>) {
    let groups = group_photos(&ui.gallery_model);
    let store = &ui.gallery_groups;

    for (i, group) in groups.iter().enumerate() {
        let i = i as u32;
        let unchanged = store
            .item(i)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|old| {
                let old = old.borrow::<PhotoGroup>();
                old.heading == group.heading && old.photos.len() == group.photos.len()
            });
        if unchanged {
            continue;
        }
        let boxed = BoxedAnyObject::new(PhotoGroup {
            heading: group.heading.clone(),
            photos: group.photos.clone(),
        });
        if i < store.n_items() {
            store.splice(i, 1, &[boxed]);
        } else {
            store.append(&boxed);
        }
    }
    // Photos only ever get appended, so a shorter model means a fresh load.
    if store.n_items() > groups.len() as u32 {
        let len = groups.len() as u32;
        store.splice(len, store.n_items() - len, &[] as &[BoxedAnyObject]);
    }

    let count = ui.gallery_model.n_items();
    ui.gallery_subtitle.set_visible(count > 0);
    // The noun tracks the active filter, so a Videos tab doesn't count "photos".
    let (one, many) = match ui.gallery_kind.get() {
        Some(PhotoKind::Video) => ("video", "videos"),
        Some(PhotoKind::Raw) => ("raw photo", "raw photos"),
        _ => ("photo", "photos"),
    };
    ui.gallery_subtitle.set_label(&match count {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    });
}

fn group_photos(model: &gio::ListStore) -> Vec<PhotoGroup> {
    let mut groups: Vec<PhotoGroup> = Vec::new();
    for i in 0..model.n_items() {
        let Some(obj) = model.item(i) else { continue };
        let Some(boxed) = obj.downcast_ref::<BoxedAnyObject>() else {
            continue;
        };
        let photo = boxed.borrow::<PhotoItem>().clone();
        let heading = day_heading(photo.capture_time);
        match groups.last_mut() {
            Some(group) if group.heading == heading => group.photos.push(photo),
            _ => groups.push(PhotoGroup {
                heading,
                photos: vec![photo],
            }),
        }
    }
    groups
}

/// Section heading for a capture time: "Today", "Yesterday", or the local date.
fn day_heading(secs: i64) -> String {
    let Ok(date) = glib::DateTime::from_unix_local(secs) else {
        return "Unknown date".into();
    };
    let same_day = |other: &glib::DateTime| {
        other.year() == date.year()
            && other.month() == date.month()
            && other.day_of_month() == date.day_of_month()
    };
    if let Ok(now) = glib::DateTime::now_local() {
        if same_day(&now) {
            return "Today".into();
        }
        if let Ok(yesterday) = glib::DateTime::from_unix_local(now.to_unix() - 86_400)
            && same_day(&yesterday)
        {
            return "Yesterday".into();
        }
    }
    date.format("%-d %B %Y")
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "Unknown date".into())
}

fn find_photo_index(model: &gio::ListStore, uid: &str) -> Option<u32> {
    for i in 0..model.n_items() {
        if let Some(obj) = model.item(i)
            && let Some(boxed) = obj.downcast_ref::<BoxedAnyObject>()
            && boxed.borrow::<PhotoItem>().uid == uid
        {
            return Some(i);
        }
    }
    None
}

fn format_capture_time(secs: i64) -> String {
    let date = glib::DateTime::from_unix_local(secs);
    match date {
        Ok(d) => match d.format("%Y-%m-%d %H:%M:%S") {
            Ok(s) => s.to_string(),
            Err(_) => "Unknown Date".to_string(),
        },
        Err(_) => "Unknown Date".to_string(),
    }
}

/// The capture time as a tile caption: the clock time alone, since the day is
/// already the section heading right above it.
fn short_capture_time(secs: i64) -> String {
    glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format("%H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// The lightbox's mutable parts, shared by [`load_photo`], [`navigate_photo`] and
/// the button/key handlers so each one takes a single handle instead of a dozen
/// widget arguments.
struct Viewer {
    picture: gtk4::Picture,
    spinner: gtk4::Spinner,
    status: gtk4::Label,
    title: gtk4::Label,
    counter: gtk4::Label,
    prev: gtk4::Button,
    next: gtk4::Button,
    /// Details drawer: the toggle that reveals it, the rows it fills in, and the
    /// "Show on map" button (hidden when the photo carries no GPS tags).
    info_toggle: gtk4::ToggleButton,
    info_revealer: gtk4::Revealer,
    info_rows: gtk4::Box,
    info_map: gtk4::Button,
    /// Coordinates behind `info_map`, once a photo with GPS tags is shown.
    coords: RefCell<Option<(f64, f64)>>,
    /// uid of the photo currently on screen.
    uid: RefCell<String>,
    /// On-disk path of the full-size photo, once it has been downloaded.
    path: RefCell<Option<String>>,
    /// True while the full-size photo is still downloading and what's on screen is
    /// the upscaled thumbnail — a click-outside-to-close hit test has to size the
    /// image from the *thumbnail's* ratio in that window, but more importantly a
    /// late reply for a photo the user has already navigated away from must not
    /// overwrite the new one. Guarded by comparing against `uid`.
    loading: Cell<bool>,
}

/// Camera/exposure/location facts pulled from a photo's own EXIF tags, as
/// label/value pairs for the details drawer.
struct ExifInfo {
    /// `("Camera", "Apple iPhone 15")` and friends; empty when the file has no
    /// EXIF at all, which is normal for screenshots and re-encoded images.
    fields: Vec<(&'static str, String)>,
    /// Decimal degrees, if the photo is geotagged.
    coords: Option<(f64, f64)>,
}

/// Show the photo behind `uid`: paint its (already cached) thumbnail immediately
/// so the lightbox never opens on a blank screen, ask the daemon for the
/// full-size file, and swap it in — plus its EXIF — when it lands.
fn load_photo(ui: &Rc<Ui>, viewer: &Rc<Viewer>, uid: String) {
    viewer.spinner.set_visible(true);
    viewer.spinner.start();
    viewer.status.set_visible(false);
    viewer.loading.set(true);
    *viewer.path.borrow_mut() = None;
    clear_info(viewer);

    // The thumbnail the gallery already decoded stands in for the full photo
    // while it downloads: blurry for a moment beats black for a second.
    match ui.photo_tex.borrow().get(&uid) {
        Some(texture) => viewer.picture.set_paintable(Some(texture)),
        None => viewer.picture.set_paintable(gtk4::gdk::Paintable::NONE),
    }

    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenPhoto { uid: uid.clone() },
    );
    let viewer = viewer.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        // The user may have moved on while this was in flight; that photo's own
        // request owns the viewer now.
        if *viewer.uid.borrow() != uid {
            return;
        }
        viewer.spinner.stop();
        viewer.spinner.set_visible(false);
        viewer.loading.set(false);

        let fail = |message: &str| {
            viewer.status.set_label(message);
            viewer.status.set_visible(true);
        };
        match result {
            Ok(Ok(Response::FilePath { path })) => match gtk4::gdk::Texture::from_filename(&path) {
                Ok(texture) => {
                    viewer.picture.set_paintable(Some(&texture));
                    *viewer.path.borrow_mut() = Some(path.clone());
                    show_info(&viewer, &path, read_exif(&path));
                }
                Err(e) => {
                    tracing::error!("Failed to load texture for {path}: {e}");
                    fail("Couldn't render this photo.");
                }
            },
            Ok(Ok(Response::Error { message })) => fail(&message),
            Ok(Ok(_)) => fail("Unexpected reply from the mount service."),
            Ok(Err(_)) | Err(_) => fail("Couldn't reach Proton Drive."),
        }
    });
}

/// Warm the cache with the photo `delta` steps away, so stepping there is
/// instant. Fire-and-forget: the daemon serves control connections concurrently,
/// so this rides alongside whatever the user does next, and a failure here is
/// simply a photo that downloads on arrival like it used to.
fn prefetch_photo(ui: &Rc<Ui>, viewer: &Rc<Viewer>, delta: i32) {
    let model = &ui.gallery_model;
    let current = find_photo_index(model, &viewer.uid.borrow()).unwrap_or(0) as i32;
    let index = current + delta;
    if index < 0 || index >= model.n_items() as i32 {
        return;
    }
    let Some(photo) = model
        .item(index as u32)
        .and_downcast::<BoxedAnyObject>()
        .map(|boxed| boxed.borrow::<PhotoItem>().clone())
    else {
        return;
    };
    // Never prefetch a video: the lightbox skips over it, and warming it would
    // mean downloading a whole clip the user won't watch from here.
    if photo.kind == PhotoKind::Video {
        return;
    }
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenPhoto { uid: photo.uid },
    );
    glib::spawn_future_local(async move {
        let _ = rx.recv().await;
    });
}

/// Reset the details drawer while the next photo is in flight, so it never shows
/// the previous photo's camera against the new image.
fn clear_info(viewer: &Rc<Viewer>) {
    *viewer.coords.borrow_mut() = None;
    viewer.info_map.set_visible(false);
    while let Some(row) = viewer.info_rows.first_child() {
        viewer.info_rows.remove(&row);
    }
    let label = gtk4::Label::builder()
        .label("Reading photo details…")
        .halign(gtk4::Align::Start)
        .build();
    label.add_css_class("dim-label");
    viewer.info_rows.append(&label);
}

/// Fill the details drawer for the photo now on screen: its own file facts
/// (size, dimensions) plus whatever EXIF it carries.
fn show_info(viewer: &Rc<Viewer>, path: &str, info: ExifInfo) {
    while let Some(row) = viewer.info_rows.first_child() {
        viewer.info_rows.remove(&row);
    }

    let mut fields: Vec<(&str, String)> = Vec::new();
    if let Ok(meta) = std::fs::metadata(path) {
        fields.push(("Size", human_bytes(meta.len())));
    }
    fields.extend(info.fields.iter().map(|(k, v)| (*k, v.clone())));

    if fields.is_empty() {
        let label = gtk4::Label::builder()
            .label("This photo carries no metadata.")
            .halign(gtk4::Align::Start)
            .wrap(true)
            .build();
        label.add_css_class("dim-label");
        viewer.info_rows.append(&label);
    } else {
        let group = gtk4::ListBox::new();
        group.set_selection_mode(gtk4::SelectionMode::None);
        group.add_css_class("boxed-list");
        for (label, value) in fields {
            let row = adw::ActionRow::builder()
                .title(label)
                .subtitle(value)
                .subtitle_selectable(true)
                .build();
            row.add_css_class("property");
            group.append(&row);
        }
        viewer.info_rows.append(&group);
    }

    viewer.info_map.set_visible(info.coords.is_some());
    *viewer.coords.borrow_mut() = info.coords;
}

/// Read the EXIF tags a gallery viewer cares about out of a decrypted photo on
/// disk. Anything missing is simply left out — phone screenshots and re-encoded
/// images legitimately carry no EXIF at all.
fn read_exif(path: &str) -> ExifInfo {
    let mut fields: Vec<(&'static str, String)> = Vec::new();
    let mut coords = None;

    let reader = match std::fs::File::open(path) {
        Ok(file) => {
            match exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)) {
                Ok(reader) => reader,
                Err(e) => {
                    tracing::debug!("no exif in {path}: {e}");
                    return ExifInfo { fields, coords };
                }
            }
        }
        Err(e) => {
            tracing::warn!("cannot open {path} for exif: {e}");
            return ExifInfo { fields, coords };
        }
    };

    let field = |tag: exif::Tag| {
        reader
            .get_field(tag, exif::In::PRIMARY)
            .map(|f| f.display_value().with_unit(&reader).to_string())
    };

    if let Some(size) = field(exif::Tag::PixelXDimension)
        .zip(field(exif::Tag::PixelYDimension))
        .map(|(w, h)| format!("{w} × {h}"))
    {
        fields.push(("Dimensions", size));
    }
    if let Some(taken) = field(exif::Tag::DateTimeOriginal) {
        fields.push(("Taken", taken));
    }

    let camera = [exif::Tag::Make, exif::Tag::Model]
        .iter()
        .filter_map(|tag| field(*tag))
        .collect::<Vec<_>>()
        .join(" ");
    if !camera.is_empty() {
        fields.push(("Camera", camera));
    }
    if let Some(lens) = field(exif::Tag::LensModel) {
        fields.push(("Lens", lens));
    }

    let exposure: Vec<String> = [
        exif::Tag::FNumber,
        exif::Tag::ExposureTime,
        exif::Tag::PhotographicSensitivity,
        exif::Tag::FocalLength,
    ]
    .iter()
    .filter_map(|tag| field(*tag))
    .collect();
    if !exposure.is_empty() {
        fields.push(("Exposure", exposure.join(" · ")));
    }

    if let Some(lat) = gps_degrees(&reader, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef)
        && let Some(lon) = gps_degrees(&reader, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef)
    {
        fields.push(("Location", format!("{lat:.5}, {lon:.5}")));
        coords = Some((lat, lon));
    }

    ExifInfo { fields, coords }
}

/// Convert one GPS coordinate from EXIF's degrees/minutes/seconds rationals to
/// decimal degrees, negating for the S/W hemispheres.
fn gps_degrees(reader: &exif::Exif, tag: exif::Tag, ref_tag: exif::Tag) -> Option<f64> {
    let field = reader.get_field(tag, exif::In::PRIMARY)?;
    let exif::Value::Rational(dms) = &field.value else {
        return None;
    };
    let [deg, min, sec] = dms.get(..3)? else {
        return None;
    };
    let degrees = deg.to_f64() + min.to_f64() / 60.0 + sec.to_f64() / 3600.0;

    let hemisphere = reader
        .get_field(ref_tag, exif::In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .unwrap_or_default();
    let negative = hemisphere.starts_with('S') || hemisphere.starts_with('W');
    Some(if negative { -degrees } else { degrees })
}

fn save_photo_to_disk(window: &gtk4::Window, source_path: &str, original_name: &str) {
    let dialog = gtk4::FileDialog::builder()
        .title("Save Photo")
        .initial_name(original_name)
        .build();
    let source_path_str = source_path.to_string();
    dialog.save(Some(window), gio::Cancellable::NONE, move |res| {
        if let Ok(file) = res
            && let Some(dest_path) = file.path()
        {
            if let Err(e) = std::fs::copy(&source_path_str, &dest_path) {
                tracing::error!("Failed to copy file to {:?}: {}", dest_path, e);
            } else {
                tracing::info!("Saved photo to {:?}", dest_path);
            }
        }
    });
}

/// Step `delta` photos through the flat timeline model and load what lands there.
fn navigate_photo(ui: &Rc<Ui>, viewer: &Rc<Viewer>, delta: i32) {
    let model = &ui.gallery_model;
    let n = model.n_items();
    if n == 0 {
        return;
    }
    let uid_val = viewer.uid.borrow().clone();
    let current_idx = find_photo_index(model, &uid_val).unwrap_or(0);

    // Where the raw step lands (prev/next are ±1; Home/End are huge deltas that
    // clamp to the ends), and which way it was travelling.
    let start = (current_idx as i32 + delta).clamp(0, n as i32 - 1);
    let dir = if delta < 0 { -1 } else { 1 };

    // The lightbox renders stills only — a video plays in an external player, so
    // it has no frame here. Land on the nearest renderable item, preferring the
    // travel direction and falling back to scanning inward from a boundary (so
    // Home/End still reach the first/last still even when videos sit at the end).
    let renderable = |idx: u32| -> Option<PhotoItem> {
        model
            .item(idx)
            .and_then(|obj| obj.downcast_ref::<BoxedAnyObject>().map(|b| b.borrow::<PhotoItem>().clone()))
            .filter(|photo| photo.kind != PhotoKind::Video)
    };
    let scan = |from: i32, step: i32| -> Option<(u32, PhotoItem)> {
        let mut probe = from;
        while (0..n as i32).contains(&probe) {
            if let Some(photo) = renderable(probe as u32) {
                return Some((probe as u32, photo));
            }
            probe += step;
        }
        None
    };
    // Scan the travel direction first. Only fall back to scanning inward when the
    // step landed on a boundary — that is the Home/End case (jump to first/last,
    // then step inward past any videos there); a mid-list prev/next with only
    // videos ahead should simply stay put rather than reverse.
    let at_boundary = start == 0 || start == n as i32 - 1;
    let landing = scan(start, dir).or_else(|| at_boundary.then(|| scan(start, -dir)).flatten());
    let Some((next_idx, photo)) = landing else {
        return;
    };

    *viewer.uid.borrow_mut() = photo.uid.clone();
    show_photo_position(viewer, next_idx, n, photo.capture_time);
    load_photo(ui, viewer, photo.uid.clone());
    // Keep walking in the same direction: the next one is likely where they're
    // headed, so have it in the cache before they ask.
    prefetch_photo(ui, viewer, delta.signum());
}

/// Set the lightbox's title (the photo's date) and its "12 of 340" counter.
fn show_photo_position(viewer: &Rc<Viewer>, index: u32, total: u32, capture_time: i64) {
    viewer.prev.set_sensitive(index > 0);
    viewer.next.set_sensitive(index + 1 < total);
    viewer.title.set_label(&format_capture_time(capture_time));
    viewer
        .counter
        .set_label(&format!("{} of {total}", index + 1));
}

/// The in-app lightbox: the photo, edge-to-edge on a dark backdrop, with a
/// floating top bar, prev/next affordances and a details drawer.
///
/// Closing it is deliberately hard to get wrong — Escape, `q`, Ctrl+W, the close
/// button, or a click on the backdrop beside the photo all dismiss it.
fn open_photo_viewer(ui: &Rc<Ui>, initial_uid: String) {
    let parent = ui.stack.root().and_downcast::<gtk4::Window>().unwrap();

    let window = gtk4::Window::builder()
        .title("Photo")
        .modal(true)
        .transient_for(&parent)
        .default_width(1100)
        .default_height(760)
        .build();
    window.add_css_class("photo-viewer-window");

    let overlay = gtk4::Overlay::new();

    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .build();
    overlay.set_child(Some(&picture));

    let prev_btn = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Previous (←)")
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Center)
        .build();
    prev_btn.add_css_class("circular");
    prev_btn.add_css_class("flat");
    prev_btn.add_css_class("viewer-nav-btn");
    overlay.add_overlay(&prev_btn);

    let next_btn = gtk4::Button::builder()
        .icon_name("go-next-symbolic")
        .tooltip_text("Next (→)")
        .halign(gtk4::Align::End)
        .valign(gtk4::Align::Center)
        .build();
    next_btn.add_css_class("circular");
    next_btn.add_css_class("flat");
    next_btn.add_css_class("viewer-nav-btn");
    overlay.add_overlay(&next_btn);

    // Top bar: the photo's date and position on the left, actions on the right,
    // over a gradient so white controls stay legible on a bright photo.
    let title_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    title_label.add_css_class("viewer-title");

    let counter_label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
    counter_label.add_css_class("viewer-counter");

    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    titles.set_hexpand(true);
    titles.set_valign(gtk4::Align::Center);
    titles.append(&title_label);
    titles.append(&counter_label);

    let action = |icon: &str, tooltip: &str| {
        let button = gtk4::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .valign(gtk4::Align::Center)
            .build();
        button.add_css_class("flat");
        button.add_css_class("viewer-action-btn");
        button
    };

    let info_toggle = gtk4::ToggleButton::builder()
        .icon_name("info-outline-symbolic")
        .tooltip_text("Details (i)")
        .valign(gtk4::Align::Center)
        .build();
    info_toggle.add_css_class("flat");
    info_toggle.add_css_class("viewer-action-btn");

    let download_btn = action("document-save-symbolic", "Save a copy…");
    let open_ext_btn = action("document-open-symbolic", "Open with another app");
    let close_btn = action("window-close-symbolic", "Close (Esc)");
    close_btn.add_css_class("viewer-close-btn");

    let top_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    top_bar.add_css_class("viewer-top-bar");
    top_bar.set_valign(gtk4::Align::Start);
    top_bar.set_hexpand(true);
    top_bar.append(&titles);
    top_bar.append(&info_toggle);
    top_bar.append(&download_btn);
    top_bar.append(&open_ext_btn);
    top_bar.append(&close_btn);
    overlay.add_overlay(&top_bar);

    // Details drawer: slides in from the right as a real surface (not a wash of
    // black over the photo), with its own header and its own way out.
    let info_rows = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    let info_map = gtk4::Button::builder()
        .label("Show on map")
        .icon_name("map-symbolic")
        .halign(gtk4::Align::Start)
        .build();
    info_map.add_css_class("pill");
    info_map.set_visible(false);

    let info_title = gtk4::Label::builder()
        .label("Details")
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .build();
    info_title.add_css_class("heading");

    let info_close = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Hide details")
        .build();
    info_close.add_css_class("flat");
    info_close.add_css_class("circular");

    let info_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    info_header.append(&info_title);
    info_header.append(&info_close);

    let info_body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    info_body.set_margin_top(16);
    info_body.set_margin_bottom(16);
    info_body.set_margin_start(16);
    info_body.set_margin_end(16);
    info_body.append(&info_header);
    info_body.append(&info_rows);
    info_body.append(&info_map);

    // Scrolled, because a photo with a full EXIF block plus a location can
    // outgrow a short window.
    let info_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&info_body)
        .build();

    let info_panel = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    info_panel.add_css_class("viewer-info-panel");
    info_panel.set_width_request(320);
    info_panel.append(&info_scroll);

    let info_revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideLeft)
        .halign(gtk4::Align::End)
        .valign(gtk4::Align::Fill)
        .child(&info_panel)
        .build();
    overlay.add_overlay(&info_revealer);

    let spinner = gtk4::Spinner::builder()
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .width_request(48)
        .height_request(48)
        .build();
    spinner.add_css_class("viewer-spinner");
    overlay.add_overlay(&spinner);

    let status_label = gtk4::Label::builder()
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .build();
    status_label.add_css_class("viewer-status");
    overlay.add_overlay(&status_label);

    window.set_child(Some(&overlay));

    let viewer = Rc::new(Viewer {
        picture: picture.clone(),
        spinner,
        status: status_label,
        title: title_label,
        counter: counter_label,
        prev: prev_btn.clone(),
        next: next_btn.clone(),
        info_toggle: info_toggle.clone(),
        info_revealer: info_revealer.clone(),
        info_rows,
        info_map: info_map.clone(),
        coords: RefCell::new(None),
        uid: RefCell::new(initial_uid.clone()),
        path: RefCell::new(None),
        loading: Cell::new(false),
    });

    let n = ui.gallery_model.n_items();
    let initial_idx = find_photo_index(&ui.gallery_model, &initial_uid).unwrap_or(0);
    let capture_time = ui
        .gallery_model
        .item(initial_idx)
        .and_downcast::<BoxedAnyObject>()
        .map_or(0, |boxed| boxed.borrow::<PhotoItem>().capture_time);
    show_photo_position(&viewer, initial_idx, n, capture_time);

    load_photo(ui, &viewer, initial_uid);
    prefetch_photo(ui, &viewer, 1);

    let w_close = window.clone();
    close_btn.connect_clicked(move |_| {
        w_close.close();
    });

    let viewer_info = viewer.clone();
    info_toggle.connect_toggled(move |toggle| {
        viewer_info
            .info_revealer
            .set_reveal_child(toggle.is_active());
    });

    let toggle_off = info_toggle.clone();
    info_close.connect_clicked(move |_| toggle_off.set_active(false));

    let viewer_map = viewer.clone();
    info_map.connect_clicked(move |_| {
        if let Some((lat, lon)) = *viewer_map.coords.borrow() {
            open_path(&format!(
                "https://www.openstreetmap.org/?mlat={lat:.6}&mlon={lon:.6}#map=16/{lat:.6}/{lon:.6}"
            ));
        }
    });

    let w_download = window.clone();
    let viewer_download = viewer.clone();
    download_btn.connect_clicked(move |_| {
        if let Some(path) = viewer_download.path.borrow().as_deref() {
            let name = format!("{}.jpg", viewer_download.uid.borrow());
            save_photo_to_disk(&w_download, path, &name);
        }
    });

    let viewer_ext = viewer.clone();
    open_ext_btn.connect_clicked(move |_| {
        if let Some(path) = viewer_ext.path.borrow().as_deref() {
            open_path(path);
        }
    });

    let ui_prev = ui.clone();
    let viewer_prev = viewer.clone();
    prev_btn.connect_clicked(move |_| {
        navigate_photo(&ui_prev, &viewer_prev, -1);
    });

    let ui_next = ui.clone();
    let viewer_next = viewer.clone();
    next_btn.connect_clicked(move |_| {
        navigate_photo(&ui_next, &viewer_next, 1);
    });

    // Click the backdrop — the dark area beside the photo — to dismiss, the way
    // every other lightbox behaves. Clicks on the photo itself are left alone, so
    // reaching for the image doesn't fling the window shut.
    let backdrop = gtk4::GestureClick::new();
    let viewer_click = viewer.clone();
    let w_click = window.clone();
    backdrop.connect_released(move |_, _, x, y| {
        if !over_photo(&viewer_click.picture, x, y) {
            w_click.close();
        }
    });
    picture.add_controller(backdrop);

    let key_controller = gtk4::EventControllerKey::new();
    let ui_key = ui.clone();
    let viewer_key = viewer.clone();
    let w_key = window.clone();
    key_controller.connect_key_pressed(move |_, key, _keycode, state| {
        let ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
        match key.name().as_deref() {
            Some("Left" | "Up" | "BackSpace") => navigate_photo(&ui_key, &viewer_key, -1),
            Some("Right" | "Down" | "space") => navigate_photo(&ui_key, &viewer_key, 1),
            Some("Home") => navigate_photo(&ui_key, &viewer_key, i32::MIN / 2),
            Some("End") => navigate_photo(&ui_key, &viewer_key, i32::MAX / 2),
            Some("i") => viewer_key
                .info_toggle
                .set_active(!viewer_key.info_toggle.is_active()),
            Some("f" | "F11") => {
                if w_key.is_fullscreen() {
                    w_key.unfullscreen();
                } else {
                    w_key.fullscreen();
                }
            }
            Some("Escape" | "q") => w_key.close(),
            Some("w") if ctrl => w_key.close(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(key_controller);

    window.present();
}

/// Whether `(x, y)` — in `picture`'s coordinates — lands on the photo itself
/// rather than the backdrop around it. [`gtk4::ContentFit::Contain`] centres the
/// image and letterboxes the rest, so the drawn rectangle is the widget scaled
/// down by whichever axis binds.
fn over_photo(picture: &gtk4::Picture, x: f64, y: f64) -> bool {
    let (width, height) = (f64::from(picture.width()), f64::from(picture.height()));
    let Some(paintable) = picture.paintable() else {
        return false;
    };
    let (iw, ih) = (
        f64::from(paintable.intrinsic_width()),
        f64::from(paintable.intrinsic_height()),
    );
    if iw <= 0.0 || ih <= 0.0 {
        return false;
    }

    let scale = (width / iw).min(height / ih);
    let (drawn_w, drawn_h) = (iw * scale, ih * scale);
    let (left, top) = ((width - drawn_w) / 2.0, (height - drawn_h) / 2.0);
    x >= left && x <= left + drawn_w && y >= top && y <= top + drawn_h
}

/// Fetch a timeline page from the daemon. When `append` is false the model is
/// cleared first (fresh load); otherwise the next page is tacked on.
fn load_gallery(ui: &Rc<Ui>, append: bool) {
    if ui.gallery_loading.get() {
        return;
    }
    if !append {
        // Fresh load: clear the timeline and show Loading until the first page lands.
        ui.gallery_model.remove_all();
        gallery_status(
            ui,
            "image-x-generic-symbolic",
            "Loading photos…",
            "Reading your Proton Drive timeline.",
            false,
        );
        // Rebuild the date jump for the current kind, but only for a full-span
        // load — a jump *to* a month sets a range and reloads, and refreshing the
        // dropdown then would fight the selection the user just made.
        if ui.gallery_range.get().is_none() {
            refresh_photo_months(ui);
        }
    }
    let offset = ui.gallery_model.n_items() as usize;
    ui.gallery_loading.set(true);
    ui.gallery_more.set_sensitive(false);

    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::PhotosTimeline {
            offset,
            limit: PHOTOS_PAGE,
            kind: ui.gallery_kind.get(),
            range: ui.gallery_range.get(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.gallery_loading.set(false);
        ui.gallery_more.set_sensitive(true);
        match result {
            Ok(Ok(Response::Photos {
                available,
                items,
                counts,
            })) => {
                if !available {
                    gallery_status(
                        &ui,
                        "image-missing-symbolic",
                        "No photo library",
                        "This Proton account doesn't have Photos enabled.",
                        false,
                    );
                    return;
                }
                // Label the filter tabs with live per-kind counts.
                if let Some(counts) = counts {
                    update_gallery_tabs(&ui, counts);
                }
                // Take the daemon's word on ratios and thumbnail verdicts before
                // anything is laid out: it persists both, so the very first frame
                // justifies its rows correctly instead of guessing RATIO_UNKNOWN
                // and reflowing as the images land.
                {
                    let mut ratios = ui.photo_ratio.borrow_mut();
                    let mut nothumb = ui.photo_nothumb.borrow_mut();
                    for item in &items {
                        if let Some(ratio) = item.ratio {
                            ratios.insert(item.uid.clone(), ratio);
                        }
                        if item.no_thumb {
                            nothumb.insert(item.uid.clone());
                        }
                    }
                }
                for item in &items {
                    ui.gallery_model.append(&BoxedAnyObject::new(item.clone()));
                }
                repaint_gallery(&ui);
                if ui.gallery_model.n_items() == 0 {
                    gallery_status(
                        &ui,
                        "image-x-generic-symbolic",
                        "No photos yet",
                        "Photos you upload to Proton Drive appear here.",
                        false,
                    );
                    return;
                }
                ui.gallery_content.set_visible_child_name("timeline");
                // Offer "Load more" only when the page came back full.
                ui.gallery_more.set_visible(items.len() == PHOTOS_PAGE);
            }
            // A failed *next* page keeps the photos already on screen — the failure
            // goes to a toast rather than wiping the timeline for a status page.
            Ok(Ok(Response::Error { message })) if append => {
                toast_error(&ui, "Couldn't load more photos", &message)
            }
            Ok(Ok(Response::Error { message })) => gallery_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't load photos",
                &message,
                false,
            ),
            Ok(Ok(_)) => gallery_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't load photos",
                "Unexpected reply from the mount service.",
                false,
            ),
            Ok(Err(_)) | Err(_) if append => toast_error(
                &ui,
                "Couldn't load more photos",
                "The mount service didn't respond.",
            ),
            Ok(Err(_)) | Err(_) => gallery_unreachable(&ui),
        }
    });
}

/// Swap the Photos content area to the status page, hiding the pager. Retry is
/// offered only when restarting the mount service could actually fix it.
fn gallery_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.gallery_status.set_icon_name(Some(icon));
    ui.gallery_status.set_title(title);
    ui.gallery_status.set_description(Some(description));
    ui.gallery_retry.set_visible(retry);
    ui.gallery_more.set_visible(false);
    ui.gallery_content.set_visible_child_name("status");
}

/// Photos counterpart of [`browser_unreachable`]: auto-retry while the mount is
/// still starting, surface an actionable error + Retry once it's actually down.
fn gallery_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        gallery_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    gallery_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("gallery") {
            load_gallery(&ui, false);
        }
    });
}

/// Open a local path with the user's default handler.
fn open_path(path: &str) {
    if let Err(e) = Command::new("xdg-open").arg(path).spawn() {
        tracing::error!("xdg-open {path} failed: {e}");
    }
}

/// Play a video with an external player. Prefers `mpv` — it sniffs the container
/// from the bytes, so the cache's extensionless blob plays fine, and it is the
/// right tool for the HEVC `.mkv`s this is aimed at — and falls back to the
/// user's default handler when mpv isn't installed.
fn play_external(path: &str) {
    if Command::new("mpv").arg(path).spawn().is_ok() {
        return;
    }
    open_path(path);
}

/// Download a Photos-library video, then hand it to an external player. Unlike a
/// still photo — which the in-app lightbox can render — a video needs a real
/// player, and the photos volume isn't part of the FUSE mount, so there is no
/// path to stream it from: [`Request::OpenPhoto`] fetches the whole file into the
/// cache (served straight from there on a repeat) and we launch the player on it.
///
/// For large videos kept in an on-demand *drive* folder, streaming through the
/// mount is the better route — that is the file-browser "Play" action, not this.
fn play_video(ui: &Rc<Ui>, uid: String) {
    toast(ui, "Preparing video…");
    let rx = spawn_request(ui.dirs.control_socket(), Request::OpenPhoto { uid });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::FilePath { path })) => play_external(&path),
            Ok(Ok(Response::Error { message })) => {
                toast_error(&ui, "Couldn't open this video", &message)
            }
            Ok(Ok(_)) => toast_error(
                &ui,
                "Couldn't open this video",
                "Unexpected reply from the mount service.",
            ),
            Ok(Err(_)) | Err(_) => {
                toast_error(&ui, "Couldn't open this video", "Couldn't reach Proton Drive.")
            }
        }
    });
}

/// Format a byte count as a short binary-unit string (e.g. `1.2 GiB`).
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A justified row must fill the content width exactly: tile widths plus the
    /// gaps between them add up to the width, with nothing left ragged.
    fn row_width(row: &[(i32, i32)]) -> i32 {
        let gaps = TILE_GAP * (row.len().saturating_sub(1)) as i32;
        row.iter().map(|(width, _)| *width).sum::<i32>() + gaps
    }

    #[test]
    fn justified_rows_fill_the_width_exactly() {
        let ratios = [
            1.5, 0.75, 1.5, 1.33, 0.66, 1.5, 1.0, 2.2, 1.5, 0.8, 1.5, 1.6,
        ];
        let rows = plan_rows(&ratios, 180.0, 1000.0);

        assert!(
            rows.len() > 1,
            "12 photos at 180px should need several rows"
        );
        // Every row but the trailing one is justified.
        for row in &rows[..rows.len() - 1] {
            assert_eq!(row_width(row), 1000);
        }
    }

    #[test]
    fn every_photo_lands_in_exactly_one_row() {
        let ratios: Vec<f64> = (0..37).map(|i| 0.5 + f64::from(i % 5) * 0.4).collect();
        let rows = plan_rows(&ratios, 180.0, 1000.0);
        let tiles: usize = rows.iter().map(Vec::len).sum();
        assert_eq!(tiles, ratios.len());
    }

    #[test]
    fn tiles_keep_their_aspect_ratio() {
        let rows = plan_rows(&[1.5, 1.5, 1.5, 1.5, 1.5, 1.5, 1.5, 1.5], 180.0, 1000.0);
        // All but the last tile of a justified row are sized straight from the
        // ratio, so w/h is the photo's own — no cropping needed to place it.
        for row in &rows {
            for (width, height) in &row[..row.len() - 1] {
                let ratio = f64::from(*width) / f64::from(*height);
                assert!((ratio - 1.5).abs() < 0.02, "{width}x{height} is not 3:2");
            }
        }
    }

    #[test]
    fn trailing_row_is_not_stretched() {
        // Two photos can't fill a 1000px row at 180px tall, so they must keep the
        // target height rather than being blown up to a screen-wide row.
        let rows = plan_rows(&[1.5, 1.5], 180.0, 1000.0);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].iter().all(|(_, height)| *height == 180));
        assert!(row_width(&rows[0]) < 1000);
    }

    #[test]
    fn an_extreme_panorama_cannot_squash_its_row() {
        // Unclamped, a 20:1 panorama would drag its whole row down to ~40px tall.
        // Clamped at RATIO_MAX it stays a wide tile in a row of normal height.
        let rows = plan_rows(&[20.0, 1.5, 1.5], 180.0, 1000.0);
        let (width, height) = rows[0][0];
        assert!(height > 120, "row squashed to {height}px");
        assert!(width <= 1000, "tile overflows the row at {width}px");
        assert_eq!(row_width(&rows[0]), 1000);
    }

    #[test]
    fn narrow_widths_and_no_photos_are_survivable() {
        assert!(plan_rows(&[], 180.0, 1000.0).is_empty());
        // Width below the floor is clamped rather than producing zero-px tiles.
        let rows = plan_rows(&[1.5, 1.5], 180.0, 0.0);
        assert!(
            rows.iter()
                .flatten()
                .all(|(width, height)| *width > 0 && *height > 0)
        );
    }
}
