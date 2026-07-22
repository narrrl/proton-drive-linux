#[path = "../activation.rs"]
pub(crate) mod activation;
pub(crate) mod pages;
pub(crate) mod widgets;

use pages::activity::*;
use pages::browser::*;
use pages::devices::*;
use pages::login::*;
use pages::photo_viewer::*;
use pages::photos::*;
use pages::shared::*;
use pages::shared_by_me::*;
use pages::status::*;
use pages::trash::*;
use pages::verify::*;
use widgets::details::*;
use widgets::share_dialog::*;

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
    ActivityEntry, ActivityKind, BookmarkInfo, DeviceInfo, DirEntry, ErrorKind, InvitationInfo,
    JobItem, PhotoItem, PhotoKind, PublicLinkInfo, RefreshScope, Request, Response,
    RestorableFolder, RestoreItem, SearchHit, ShareEntry, ShareEntryKind, SharedItem,
    SyncFolderInfo, SyncPhase, SyncProgress, TransferDirection, TransferItem, pending_summary,
    send,
};

use pdfs_core::service;

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
    /// Whether the last refresh saw a live mount daemon. Gates the unpin buttons
    /// (which need the daemon to evict + re-hydrate) and every mutating action.
    mounted: RefCell<bool>,
    /// Sidebar destination list (Files / Photos / Settings). Selecting a row swaps
    /// the page stack; [`sync_sidebar`] mirrors navigation that starts elsewhere.
    sidebar: gtk4::ListBox,
    /// The sidebar/content split. Collapsed while signed out, so the login page
    /// owns the whole window and no destination is reachable without a session.
    nav: adw::NavigationSplitView,

    // Per-page state. Each page module owns its own struct; `Ui` keeps only
    // what more than one page genuinely shares.
    pub(crate) login: LoginState,
    pub(crate) status: StatusState,
    pub(crate) browser: BrowserState,
    pub(crate) details: DetailsState,
    pub(crate) trash: TrashState,
    pub(crate) gallery: GalleryState,
    pub(crate) shared: SharedState,
    pub(crate) shared_by_me: SharedByMeState,
    pub(crate) devices: DevicesState,
    pub(crate) activity: ActivityState,
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
        self.gallery
            .photo_ratio
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

        let mut cache = self.gallery.photo_tex.borrow_mut();
        let mut order = self.gallery.photo_tex_order.borrow_mut();
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
        let mut ratios = self.gallery.photo_ratio.borrow_mut();
        match ratios.insert(uid.to_string(), ratio) {
            // Only a *changed* ratio invalidates a layout; re-decoding a photo we
            // already sized correctly must not trigger another pass.
            Some(prev) if (prev - ratio).abs() < f64::EPSILON => false,
            _ => {
                self.gallery.photo_ratio_dirty.set(true);
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
            Ok(map) => *self.gallery.photo_ratio.borrow_mut() = map,
            Err(e) => tracing::debug!("ignoring {}: {e}", path.display()),
        }
    }

    /// Persist the learned aspect ratios, if any were learned since the last save.
    fn save_ratios(&self) {
        if !self.gallery.photo_ratio_dirty.replace(false) {
            return;
        }
        let path = self.ratio_path();
        let ratios = self.gallery.photo_ratio.borrow();
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
        mounted: RefCell::new(false),
        sidebar: sidebar_list.clone(),
        nav: split.clone(),
        login: LoginState {
            email: login_widgets.0,
            password: login_widgets.1,
            login_button: login_widgets.2,
            login_status: login_widgets.3,
        },
        status: StatusState {
            status_inflight: Cell::new(false),
            account_row: main_widgets.account_row.clone(),
            mount_row: main_widgets.mount_row.clone(),
            transfers_group: main_widgets.transfers_group.clone(),
            transfer_rows: RefCell::new(Vec::new()),
            transfers_inflight: Cell::new(false),
            cache_bar: main_widgets.cache_bar.clone(),
            cache_label: main_widgets.cache_label.clone(),
            quota_group: main_widgets.quota_group.clone(),
            quota_bar: main_widgets.quota_bar.clone(),
            quota_label: main_widgets.quota_label.clone(),
            quota_inflight: Cell::new(false),
            quota_checked_at: Cell::new(None),
            autostart_row: main_widgets.autostart_row.clone(),
            budget_row: main_widgets.budget_row.clone(),
            mountpoint_row: main_widgets.mountpoint_row.clone(),
            settings_suppress: Cell::new(false),
            pins_group: main_widgets.pins_group.clone(),
            pin_rows: RefCell::new(Vec::new()),
            pins_state: RefCell::new(None),
            notified_mounted: Cell::new(None),
            active_transfers: Cell::new(0),
        },
        browser: BrowserState {
            model: browser_widgets.model.clone(),
            back: browser_widgets.back.clone(),
            crumb: browser_widgets.crumb.clone(),
            content: browser_widgets.content.clone(),
            status: browser_widgets.status.clone(),
            retry: browser_widgets.retry.clone(),
            split: browser_widgets.split.clone(),
            path: RefCell::new(String::new()),
            search: browser_widgets.search.clone(),
            new_folder: browser_widgets.new_folder.clone(),
            upload: browser_widgets.upload.clone(),
            upload_folder: browser_widgets.upload_folder.clone(),
            search_source: RefCell::new(None),
        },
        details: DetailsState {
            details: browser_widgets.details,
            details_entry: RefCell::new(None),
            details_suppress: Cell::new(false),
            grid_selection: browser_widgets.grid_selection.clone(),
            list_selection: browser_widgets.list_selection.clone(),
        },
        trash: TrashState {
            model: trash_widgets.model.clone(),
            content: trash_widgets.content.clone(),
            status: trash_widgets.status.clone(),
            retry: trash_widgets.retry.clone(),
            empty: trash_widgets.empty.clone(),
            subtitle: trash_widgets.subtitle.clone(),
        },
        gallery: GalleryState {
            model: gallery_widgets.model.clone(),
            groups: gallery_widgets.groups.clone(),
            tile: Cell::new(TILE_DEFAULT),
            content: gallery_widgets.content.clone(),
            status: gallery_widgets.status.clone(),
            retry: gallery_widgets.retry.clone(),
            more: gallery_widgets.more.clone(),
            upload: gallery_widgets.upload.clone(),
            subtitle: gallery_widgets.subtitle.clone(),
            kind: Cell::new(None),
            tabs: gallery_widgets.tabs.clone(),
            dates: gallery_widgets.dates.clone(),
            date_ranges: RefCell::new(vec![None]),
            range: Cell::new(None),
            date_suppress: Cell::new(false),
            loading: Cell::new(false),
            width: Cell::new(0),
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
            bound: RefCell::new(HashMap::new()),
        },
        shared: SharedState {
            content: shared_widgets.content.clone(),
            status: shared_widgets.status.clone(),
            retry: shared_widgets.retry.clone(),
            with_me_group: shared_widgets.shared_with_me.clone(),
            invitations_group: shared_widgets.invitations.clone(),
            bookmarks_group: shared_widgets.bookmarks.clone(),
            nav: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
            inflight: Cell::new(false),
            loaded_at: Cell::new(None),
        },
        shared_by_me: SharedByMeState {
            content: shared_by_me_widgets.content.clone(),
            status: shared_by_me_widgets.status.clone(),
            retry: shared_by_me_widgets.retry.clone(),
            group: shared_by_me_widgets.group.clone(),
            rows: RefCell::new(Vec::new()),
            inflight: Cell::new(false),
            loaded_at: Cell::new(None),
        },
        devices: DevicesState {
            content: devices_widgets.content.clone(),
            status: devices_widgets.status.clone(),
            retry: devices_widgets.retry.clone(),
            group: devices_widgets.group.clone(),
            rows: RefCell::new(Vec::new()),
            sync_group: devices_widgets.sync_group.clone(),
            sync_rows: RefCell::new(Vec::new()),
            rename_this: devices_widgets.rename_this.clone(),
            this_device: RefCell::new(None),
            inflight: Cell::new(false),
            loaded_at: Cell::new(None),
        },
        activity: ActivityState {
            content: activity_widgets.content.clone(),
            status: activity_widgets.status.clone(),
            retry: activity_widgets.retry.clone(),
            group: activity_widgets.group.clone(),
            rows: RefCell::new(Vec::new()),
            inflight: Cell::new(false),
            key: RefCell::new(None),
        },
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
    wire_devices(
        &ui,
        &devices_widgets.retry,
        &devices_widgets.add_folder,
        &devices_widgets.restore,
    );
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
            Some("sharedbyme") if page_fresh(&ui_nav.shared_by_me.loaded_at) => {}
            Some("sharedbyme") => load_shared_by_me(&ui_nav),
            Some("shared") if page_fresh(&ui_nav.shared.loaded_at) => {}
            Some("shared") => load_shared(&ui_nav),
            Some("devices") if page_fresh(&ui_nav.devices.loaded_at) => {}
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
            let path = ui.browser.path.borrow().clone();
            refresh_then(ui, RefreshScope::Dir { path }, load_browser);
        }
        Some("gallery") => refresh_then(ui, RefreshScope::Photos, |ui| load_gallery(ui, false)),
        Some("trash") => refresh_then(ui, RefreshScope::Trash, load_trash),
        Some("shared") => {
            ui.shared.loaded_at.set(None);
            load_shared(ui);
        }
        Some("sharedbyme") => {
            ui.shared_by_me.loaded_at.set(None);
            load_shared_by_me(ui);
        }
        Some("devices") => {
            ui.devices.loaded_at.set(None);
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

/// Headline for a failed request, chosen from its [`ErrorKind`] rather than from
/// the text the daemon happened to assemble.
///
/// The daemon's prose names the layer that failed (`"enumerate nodes: …"`), which
/// is right for a log and wrong for a user. `kind` is the part that says what the
/// person in front of the screen should understand, so the headline comes from it
/// and the prose is demoted to the detail line.
///
/// `fallback` is the caller's own description of the action ("Couldn't rename"),
/// used where the class carries no better wording than the caller already has.
fn error_headline(kind: ErrorKind, fallback: &str) -> &str {
    match kind {
        ErrorKind::Offline => "You're offline",
        ErrorKind::NotFound => "That's not there any more",
        ErrorKind::Denied => "You don't have access to that",
        ErrorKind::Conflict => "Something changed this first",
        ErrorKind::Quota => "Your Proton Drive is full",
        ErrorKind::Invalid | ErrorKind::Remote | ErrorKind::Internal => fallback,
    }
}

/// Report a failed request, letting its class pick the wording.
///
/// Prefer this to [`toast_error`] anywhere a [`Response::Error`] is being shown:
/// being offline is the common case and deserves to read as a state of the
/// network rather than as a fault in whatever the user just did.
fn toast_failure(ui: &Rc<Ui>, what: &str, message: &str, kind: ErrorKind) {
    match kind {
        // The detail here is always some inner layer's EIO. Nothing in it helps.
        ErrorKind::Offline => toast_error(ui, error_headline(kind, what), ""),
        _ => toast_error(ui, error_headline(kind, what), message),
    }
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
                ui.browser.search.grab_focus();
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
            gtk4::gdk::Key::Escape if on_browser && ui.browser.split.shows_sidebar() => {
                hide_details(&ui);
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(controller);
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

/// The top-level window, for parenting dialogs.
fn ui_window(ui: &Rc<Ui>) -> Option<gtk4::Window> {
    ui.stack.root().and_downcast::<gtk4::Window>()
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

/// Uppercase the first character of a role word for read-only display.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Open a local path with the user's default handler.
fn open_path(path: &str) {
    if let Err(e) = Command::new("xdg-open").arg(path).spawn() {
        tracing::error!("xdg-open {path} failed: {e}");
    }
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
