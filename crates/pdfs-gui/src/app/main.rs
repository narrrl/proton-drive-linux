#[path = "../activation.rs"]
pub(crate) mod activation;
pub(crate) mod pages;
pub(crate) mod widgets;

use pages::activity::*;
use pages::albums::*;
use pages::browser::*;
use pages::devices::*;
use pages::locations::*;
use pages::login::*;
use pages::photo_viewer::*;
use pages::photos::*;
use pages::shared::*;
use pages::shared_by_me::*;
use pages::status::*;
use pages::takeout::*;
use pages::trash::*;
use pages::verify::*;
use widgets::details::*;
use widgets::share_dialog::*;
use widgets::thumbnails::*;
use widgets::versions_dialog::*;

use std::cell::{Cell, RefCell};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use std::path::{Path, PathBuf};

use std::process::Command;

use std::rc::Rc;

use std::time::{Duration, Instant};

use adw::prelude::*;

use gtk4::gio;

use gtk4::glib;

use gtk4::glib::BoxedAnyObject;

use pdfs_core::auth;

use pdfs_core::config::{AppDirs, FileSort, FilesView};

use pdfs_core::control::{
    ActivityEntry, ActivityKind, AlbumInfo, BookmarkInfo, ConflictInfo, ConflictKeep, DeviceInfo,
    DirEntry, ErrorKind, ImportSummary, InvitationInfo, JobItem, PendingOpInfo, PhotoItem,
    PhotoKind, PublicLinkInfo, RefreshScope, Request, Response, RestorableFolder, RestoreItem,
    SearchHit, ShareEntry, ShareEntryKind, SharedItem, SyncFolderInfo, SyncPhase, SyncProgress,
    ThumbnailBuildStatus, TransferDirection, TransferItem, pending_summary, send,
};

use pdfs_core::mounts::{MountAccess, MountKind, MountMode, MountSpec};

use pdfs_core::service;

const APP_ID: &str = "io.narl.proton-drive-linux";

/// Proton brand purple, applied as the libadwaita accent when the user opts in
/// (Preferences → Appearance), so switches, buttons and links all pick it up.
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

/// How often the gallery asks whether the timeline refresh it started has
/// finished.
const PHOTOS_REFRESH_POLL: Duration = Duration::from_secs(2);

/// How long the gallery keeps following one timeline refresh. A library of tens
/// of thousands of photos is re-read in batches over several minutes; past this
/// the daemon carries on and the page catches up on its next load.
const PHOTOS_REFRESH_FOLLOW_LIMIT: Duration = Duration::from_secs(15 * 60);

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
    /// How many open/load round-trips are in flight. While any is, every page
    /// header shows its spinner (see [`page_frame`]); ref-counted so concurrent
    /// operations don't hide it early.
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
    /// Sidebar destination list (My files, Photos, …, Sync). Selecting a row swaps
    /// the page stack; [`sync_sidebar`] mirrors navigation that starts elsewhere.
    sidebar: gtk4::ListBox,
    /// The sidebar/content split. Collapsed while signed out, so the login page
    /// owns the whole window and no destination is reachable without a session.
    nav: adw::NavigationSplitView,
    /// Shared thumbnails for ordinary image files outside the Photos gallery.
    /// One cache and request queue serves Files, search, Shared and Trash, so
    /// the same image is downloaded and decoded only once.
    pub(crate) file_thumbs: FileThumbnailState,

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
    pub(crate) locations: LocationsState,
    pub(crate) activity: ActivityState,
    pub(crate) takeout: TakeoutState,
}

impl Ui {
    /// Begin a unit of background work: show + spin the header spinner.
    fn busy_begin(&self) {
        self.busy.set(self.busy.get() + 1);
        set_busy_spinners(true);
    }

    /// End a unit of background work: stop the spinner once the last one is done.
    fn busy_end(&self) {
        let remaining = self.busy.get().saturating_sub(1);
        self.busy.set(remaining);
        if remaining == 0 {
            set_busy_spinners(false);
        }
    }

    /// Remember a decoded thumbnail, evicting the oldest once the cache is full.
    /// Scrolling back over a day then repaints from memory rather than decoding
    /// the same JPEGs off disk again.
    fn store_texture(&self, uid: &str, texture: gtk4::gdk::Texture) {
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
    }

    /// Mark a cached thumbnail as freshly used, so eviction takes the tiles
    /// nobody has looked at rather than the oldest ones.
    ///
    /// Insertion order alone evicts the top of the timeline first — exactly what
    /// a scroll back up then asks for again.
    fn touch_texture(&self, uid: &str) {
        let mut order = self.gallery.photo_tex_order.borrow_mut();
        if order.back().is_some_and(|back| back == uid) {
            return;
        }
        if let Some(at) = order.iter().position(|held| held == uid) {
            order.remove(at);
            order.push_back(uid.to_string());
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
        if let Ok(dirs) = AppDirs::new() {
            set_proton_accent(dirs.load_config().proton_accent.unwrap_or(false));
        }
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

/// Register the bundled GResources (custom icons, the stylesheet) and load the
/// app stylesheet. The Proton accent is a separate provider, applied by
/// [`set_proton_accent`] from the saved preference.
fn load_proton_theme() {
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/pdfs.gresource"));
    let resource_data = glib::Bytes::from_static(bytes);
    if let Ok(resource) = gio::Resource::from_data(&resource_data) {
        gio::resources_register(&resource);
    } else {
        tracing::error!("failed to load gresource bundle");
    }

    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_resource("/de/nils/protondrivelinux/style.css");
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    gtk4::IconTheme::for_display(&display).add_resource_path("/de/nils/protondrivelinux/icons");
}

thread_local! {
    /// The Proton purple accent provider while it is installed. Kept so turning
    /// the preference off can remove exactly what turning it on added.
    static PROTON_ACCENT: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

/// Paint the app in Proton purple (`on`) or follow the system accent colour.
/// Every accent-coloured widget reads `--accent-bg-color`, so overriding that
/// one variable recolours the whole app.
fn set_proton_accent(on: bool) {
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    PROTON_ACCENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        match (on, slot.as_ref()) {
            (true, None) => {
                let provider = gtk4::CssProvider::new();
                provider
                    .load_from_string(&format!(":root {{ --accent-bg-color: {PROTON_PURPLE}; }}"));
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
                );
                *slot = Some(provider);
            }
            (false, Some(provider)) => {
                gtk4::style_context_remove_provider_for_display(&display, provider);
                *slot = None;
            }
            _ => {}
        }
    });
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
    let main_widgets = build_main_page();
    let (browser_page, browser_widgets) = build_browser_page();
    let (gallery_page, gallery_widgets) = build_gallery_page();
    let (shared_page, shared_widgets) = build_shared_page();
    let (shared_by_me_page, shared_by_me_widgets) = build_shared_by_me_page();
    let (devices_page, devices_widgets) = build_devices_page();
    let (locations_page, locations_widgets) = build_locations_page();
    let (activity_page, activity_widgets) = build_activity_page();
    let (trash_page, trash_widgets) = build_trash_page();
    let (takeout_page, takeout_widgets) = build_takeout_page();
    // The login page has no title and no actions, but it still needs a header
    // bar: it is the only thing carrying the window controls.
    let login_frame = adw::ToolbarView::new();
    login_frame.add_top_bar(&{
        let header = adw::HeaderBar::new();
        header.set_show_title(false);
        header
    });
    login_frame.set_content(Some(&login_page));
    stack.add_named(&login_frame, Some("login"));
    stack.add_named(&browser_page, Some("browser"));
    stack.add_named(&gallery_page, Some("gallery"));
    stack.add_named(&shared_by_me_page, Some("sharedbyme"));
    stack.add_named(&shared_page, Some("shared"));
    stack.add_named(&devices_page, Some("devices"));
    stack.add_named(&locations_page, Some("locations"));
    stack.add_named(&activity_page, Some("activity"));
    stack.add_named(&trash_page, Some("trash"));
    stack.add_named(&takeout_page, Some("takeout"));

    // Sidebar: the signed-in destinations. Selecting a row swaps the page stack;
    // `sync_sidebar` pushes the other way when navigation happens elsewhere (e.g.
    // login lands on Files).
    let (sidebar_page, sidebar_list) = build_sidebar(&main_widgets.footer);

    // Every page brings its own header bar (see `page_frame`), so the content
    // side is the bare stack.
    let content_page = adw::NavigationPage::builder()
        .title("Proton Drive")
        .child(&stack)
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
        busy: Cell::new(0),
        opening: RefCell::new(HashSet::new()),
        session: RefCell::new(auth::load().ok()),
        mounted: RefCell::new(false),
        sidebar: sidebar_list.clone(),
        nav: split.clone(),
        file_thumbs: FileThumbnailState::new(),
        login: login_widgets,
        status: StatusState {
            status_inflight: Cell::new(false),
            transfers_group: locations_widgets.transfers_group.clone(),
            prefs: main_widgets.prefs.clone(),
            account_name: main_widgets.account_name.clone(),
            avatar: main_widgets.avatar.clone(),
            status_icon: main_widgets.status_icon.clone(),
            status_title: main_widgets.status_title.clone(),
            status_detail: main_widgets.status_detail.clone(),
            transfer_rows: RefCell::new(Vec::new()),
            transfers_inflight: Cell::new(false),
            cache_bar: main_widgets.cache_bar.clone(),
            cache_label: main_widgets.cache_label.clone(),
            quota_box: main_widgets.quota_box.clone(),
            quota_bar: main_widgets.quota_bar.clone(),
            quota_label: main_widgets.quota_label.clone(),
            quota_inflight: Cell::new(false),
            quota_checked_at: Cell::new(None),
            autostart_row: main_widgets.autostart_row.clone(),
            budget_row: main_widgets.budget_row.clone(),
            mountpoint_row: main_widgets.mountpoint_row.clone(),
            accent_row: main_widgets.accent_row.clone(),
            settings_suppress: Cell::new(false),
            budget_source: RefCell::new(None),
            pins_expanded: Cell::new(false),
            pins_group: main_widgets.pins_group.clone(),
            pin_rows: RefCell::new(Vec::new()),
            pins_state: RefCell::new(None),
            notified_mounted: Cell::new(None),
            active_transfers: Cell::new(0),
        },
        browser: BrowserState {
            model: browser_widgets.model.clone(),
            history: RefCell::new(Vec::new()),
            future: RefCell::new(Vec::new()),
            crumb: browser_widgets.crumb.clone(),
            content: browser_widgets.content.clone(),
            status: browser_widgets.status.clone(),
            retry: browser_widgets.retry.clone(),
            split: browser_widgets.split.clone(),
            path: RefCell::new(String::new()),
            search: browser_widgets.search.clone(),
            actions: browser_widgets.actions.clone(),
            view_button: browser_widgets.view_button.clone(),
            view: Cell::new(FilesView::default()),
            listing: RefCell::new(Vec::new()),
            build_thumbnails: browser_widgets.build_thumbnails.clone(),
            thumbnail_build_row: browser_widgets.thumbnail_build_row.clone(),
            thumbnail_progress: browser_widgets.thumbnail_progress.clone(),
            thumbnail_status: browser_widgets.thumbnail_status.clone(),
            thumbnail_poll: RefCell::new(None),
            thumbnail_build_running: Cell::new(false),
            thumbnail_cancel_pending: Cell::new(false),
            search_source: RefCell::new(None),
            load_generation: Cell::new(0),
            views: browser_widgets.views.clone(),
            bulk: browser_widgets.bulk.clone(),
            bulk_label: browser_widgets.bulk_label.clone(),
            bulk_trash: browser_widgets.bulk_trash.clone(),
            bulk_pin: browser_widgets.bulk_pin.clone(),
            bulk_unpin: browser_widgets.bulk_unpin.clone(),
            bulk_move: browser_widgets.bulk_move.clone(),
            empty_actions: browser_widgets.empty_actions.clone(),
            summary: browser_widgets.summary.clone(),
            zoom: browser_widgets.zoom.clone(),
            grid_thumbnail_size: Cell::new(GRID_THUMB_DEFAULT),
            grid_tiles: RefCell::new(Vec::new()),
            quota_box: browser_widgets.quota_box.clone(),
            quota: browser_widgets.quota.clone(),
            quota_text: browser_widgets.quota_text.clone(),
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
            selection: trash_widgets.selection.clone(),
            selection_bar: trash_widgets.selection_bar.clone(),
            selection_label: trash_widgets.selection_label.clone(),
        },
        gallery: GalleryState {
            model: gallery_widgets.model.clone(),
            groups: gallery_widgets.groups.clone(),
            row_height: Cell::new(ROW_DEFAULT),
            learned_ratios: RefCell::new(HashMap::new()),
            scrolling_down: Cell::new(true),
            scroll_offset: Cell::new(0.0),
            assumed_ratios: RefCell::new(HashSet::new()),
            content: gallery_widgets.content.clone(),
            status: gallery_widgets.status.clone(),
            retry: gallery_widgets.retry.clone(),
            more: gallery_widgets.more.clone(),
            import_banner: gallery_widgets.import_banner.clone(),
            upload: gallery_widgets.upload.clone(),
            import: gallery_widgets.import.clone(),
            empty_actions: gallery_widgets.empty_actions.clone(),
            title: gallery_widgets.title.clone(),
            albums: gallery_widgets.albums.clone(),
            albums_stack: gallery_widgets.albums_stack.clone(),
            albums_status: gallery_widgets.albums_status.clone(),
            photos_btn: gallery_widgets.photos_btn.clone(),
            albums_btn: gallery_widgets.albums_btn.clone(),
            view_switch: gallery_widgets.view_switch.clone(),
            albums_loading: Cell::new(false),
            album: RefCell::new(None),
            back: gallery_widgets.back.clone(),
            filters: gallery_widgets.filters.clone(),
            kind: Cell::new(None),
            tabs: gallery_widgets.tabs.clone(),
            counts: Cell::new(None),
            dates: gallery_widgets.dates.clone(),
            date_ranges: RefCell::new(vec![None]),
            range: Cell::new(None),
            favorites_btn: gallery_widgets.favorites_btn.clone(),
            favorites: Cell::new(false),
            date_suppress: Cell::new(false),
            loading: Cell::new(false),
            width: Cell::new(0),
            photo_tex: RefCell::new(HashMap::new()),
            photo_tex_order: RefCell::new(VecDeque::new()),
            photo_nothumb: RefCell::new(HashSet::new()),
            thumb_wanted: RefCell::new(HashMap::new()),
            thumb_queue: RefCell::new(VecDeque::new()),
            thumb_inflight: Cell::new(false),
            decode_queue: RefCell::new(VecDeque::new()),
            decode_idle: Cell::new(false),
            thumb_source: RefCell::new(None),
            relayout_source: RefCell::new(None),
            bound: RefCell::new(BTreeMap::new()),
            list: gallery_widgets.list.clone(),
            selecting: Cell::new(false),
            selected: RefCell::new(HashSet::new()),
            select_btn: gallery_widgets.select_btn.clone(),
            select_bar: gallery_widgets.select_bar.clone(),
            select_label: gallery_widgets.select_label.clone(),
            select_trash: gallery_widgets.select_trash.clone(),
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
        locations: LocationsState {
            content: locations_widgets.content.clone(),
            status: locations_widgets.status.clone(),
            retry: locations_widgets.retry.clone(),
            group: locations_widgets.group.clone(),
            rows: RefCell::new(Vec::new()),
            inflight: Cell::new(false),
            loaded_at: Cell::new(None),
            card: SyncCard {
                icon: locations_widgets.card_icon.clone(),
                row: locations_widgets.card_row.clone(),
                pause: locations_widgets.pause.clone(),
                paused: Cell::new(false),
            },
            queue: QueueState {
                group: locations_widgets.queue_group.clone(),
                retry_all: locations_widgets.retry_all.clone(),
                rows: RefCell::new(Vec::new()),
                painted: RefCell::new(Vec::new()),
                inflight: Cell::new(false),
            },
            conflicts: ConflictsState {
                group: locations_widgets.conflicts_group.clone(),
                rows: RefCell::new(Vec::new()),
                painted: RefCell::new(Vec::new()),
                inflight: Cell::new(false),
                fetched_at: Cell::new(None),
            },
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
        takeout: TakeoutState {
            archives: RefCell::new(Vec::new()),
            list_group: takeout_widgets.list_group.clone(),
            rows: RefCell::new(Vec::new()),
            dropzone: takeout_widgets.dropzone.clone(),
            scan_button: takeout_widgets.scan_button.clone(),
            import_button: takeout_widgets.import_button.clone(),
            cancel_button: takeout_widgets.cancel_button.clone(),
            clear_button: takeout_widgets.clear_button.clone(),
            progress_group: takeout_widgets.progress_group.clone(),
            progress_label: takeout_widgets.progress_label.clone(),
            progress_bar: takeout_widgets.progress_bar.clone(),
            summary_group: takeout_widgets.summary_group.clone(),
            summary_rows: RefCell::new(Vec::new()),
            inflight: Cell::new(false),
            running: Cell::new(false),
            dry_run: Cell::new(false),
        },
    });
    wire_login(&ui);
    let ui_status = ui.clone();
    main_widgets
        .status_button
        .connect_clicked(move |_| ui_status.stack.set_visible_child_name("locations"));
    wire_settings(
        &ui,
        &main_widgets.purge_button,
        &main_widgets.mountpoint_button,
    );
    wire_sidebar(&ui);
    wire_browser(&ui, &browser_widgets.grid, &browser_widgets.column_view);
    wire_bulk(
        &ui,
        &browser_widgets.bulk_clear,
        &browser_widgets.empty_upload,
        &browser_widgets.empty_new_folder,
    );
    wire_browser_actions(&ui, &browser_widgets.build_thumbnails);
    wire_details(&ui);
    wire_search(&ui);
    wire_gallery(
        &ui,
        &gallery_widgets.list,
        &gallery_widgets.scroll,
        &gallery_widgets.select_done,
    );
    wire_gallery_empty(
        &ui,
        &gallery_widgets.empty_upload,
        &gallery_widgets.empty_import,
    );
    wire_albums(&ui);
    wire_trash(&ui, &trash_widgets);
    wire_shared(&ui, &shared_widgets.retry, &shared_widgets.add_bookmark);
    wire_shared_by_me(&ui, &shared_by_me_widgets.retry);
    wire_devices(&ui, &devices_widgets.retry, &devices_widgets.restore);
    wire_locations(&ui, &locations_widgets.retry, &locations_widgets.add_folder);
    wire_activity(&ui, &activity_widgets.retry);
    wire_takeout(&ui, &takeout_widgets);
    wire_refresh(
        &ui,
        &[
            &browser_widgets.refresh,
            &gallery_widgets.refresh,
            &trash_widgets.refresh,
            &shared_widgets.refresh,
            &shared_by_me_widgets.refresh,
            &devices_widgets.refresh,
            &locations_widgets.refresh,
            &activity_widgets.refresh,
        ],
    );
    wire_retry(&ui);

    // Lazily load the Files / Photos pages the first time they're shown, so the
    // network round-trip only happens on demand rather than on every refresh.
    let ui_nav = ui.clone();
    stack.connect_visible_child_name_notify(move |st| {
        // Rows from the page being left must not keep full-size image downloads
        // alive in the daemon. The page being entered establishes a fresh
        // thumbnail generation as it paints.
        cancel_file_thumbnails(&ui_nav);
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
            Some("locations") => {
                refresh_conflicts(&ui_nav, true);
                if !page_fresh(&ui_nav.locations.loaded_at) {
                    load_locations(&ui_nav);
                }
            }
            // Activity is intentionally not TTL-cached: it changes out from under
            // the page as background uploads and edits complete, so it reloads on
            // every visit to stay live.
            Some("activity") => load_activity(&ui_nav),
            Some("trash") => load_trash(&ui_nav),
            // The import runs in the daemon and outlives this page, so arriving
            // here asks straight away whether one is in flight.
            Some("takeout") => refresh_takeout(&ui_nav),
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
    install_window_actions(&ui, &window);
    app.set_accels_for_action("win.preferences", &["<Control>comma"]);
    app.set_accels_for_action("win.shortcuts", &["<Control>question"]);

    refresh(&ui);
    // Periodic refresh while the window lives. The closure holds a strong `Rc`;
    // it is dropped when the source is removed on window close.
    let ui_tick = ui.clone();
    let source = glib::timeout_add_local(REFRESH_INTERVAL, move || {
        refresh(&ui_tick);
        glib::ControlFlow::Continue
    });
    let cell = RefCell::new(Some(source));
    window.connect_close_request(move |_| {
        if let Some(id) = cell.borrow_mut().take() {
            id.remove();
        }
        glib::Propagation::Proceed
    });

    window.present();
}

/// The sidebar destinations, in order: the row index is the index into this table,
/// and each entry is `(stack page name, label, icon)`. The places in the account
/// come first; the rows from [`SIDEBAR_SYNC_SECTION`] on are about this computer's
/// sync, set off by a separator.
const DESTINATIONS: [(&str, &str, &str); 8] = [
    ("browser", "My files", "folder-symbolic"),
    ("gallery", "Photos", "image-x-generic-symbolic"),
    ("shared", "Shared with me", "system-users-symbolic"),
    ("sharedbyme", "Shared by me", "emblem-shared-symbolic"),
    ("devices", "Computers", "computer-symbolic"),
    ("trash", "Trash", "user-trash-symbolic"),
    ("locations", "Sync", "emblem-synchronizing-symbolic"),
    ("activity", "Activity", "document-open-recent-symbolic"),
];

/// Index of the first row in the sidebar's sync section.
const SIDEBAR_SYNC_SECTION: i32 = 6;

/// The navigation sidebar: the destinations over a footer carrying the sync
/// status, the account quota and the account menu. Returns the page (for the
/// split view) and the list (to drive + reflect the current page).
fn build_sidebar(footer: &gtk4::Box) -> (adw::NavigationPage, gtk4::ListBox) {
    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    for (_, label, icon) in DESTINATIONS {
        let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        row_box.append(&gtk4::Image::from_icon_name(icon));
        row_box.append(&gtk4::Label::new(Some(label)));
        let row = gtk4::ListBoxRow::builder().child(&row_box).build();
        list.append(&row);
    }
    list.set_header_func(|row, _| {
        if row.index() == SIDEBAR_SYNC_SECTION {
            let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
            separator.set_margin_top(6);
            separator.set_margin_bottom(6);
            row.set_header(Some(&separator));
        } else {
            row.set_header(gtk4::Widget::NONE);
        }
    });

    let brand = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let icon = gtk4::Image::from_icon_name("folder-remote-symbolic");
    icon.add_css_class("brand-icon");
    brand.append(&icon);
    let name = gtk4::Label::new(Some("Proton Drive"));
    name.add_css_class("heading");
    brand.append(&name);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&brand));
    header.pack_end(&build_primary_menu());

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));
    toolbar.add_bottom_bar(footer);

    let page = adw::NavigationPage::builder()
        .title("Proton Drive")
        .child(&toolbar)
        .build();
    (page, list)
}

thread_local! {
    /// Every page header's busy spinner. There is one per page because each
    /// page has its own header bar; [`Ui::busy_begin`] shows them all, so the one
    /// on screen is always the right one.
    static BUSY_SPINNERS: RefCell<Vec<gtk4::Spinner>> = const { RefCell::new(Vec::new()) };
}

fn set_busy_spinners(visible: bool) {
    BUSY_SPINNERS.with(|spinners| {
        for spinner in spinners.borrow().iter() {
            spinner.set_visible(visible);
            spinner.set_spinning(visible);
        }
    });
}

/// A destination page's frame: its own header bar over the page content, the
/// way GNOME apps lay a split view out. The title sits in the middle with the
/// busy spinner beside it; the page packs its actions into the returned header
/// bar and may give the returned title a subtitle.
fn page_frame(
    title: &str,
    content: &impl IsA<gtk4::Widget>,
) -> (adw::ToolbarView, adw::HeaderBar, adw::WindowTitle) {
    let window_title = adw::WindowTitle::new(title, "");
    let spinner = gtk4::Spinner::new();
    spinner.set_visible(false);
    BUSY_SPINNERS.with(|spinners| spinners.borrow_mut().push(spinner.clone()));
    let title_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    title_box.append(&window_title);
    title_box.append(&spinner);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title_box));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(content));
    (toolbar, header, window_title)
}

/// The sidebar destination a page belongs under. Most pages are their own
/// destination; a sub-page reached from one (Import, off Photos) has no row of
/// its own and answers with its parent, so the sidebar highlights where the user
/// came from rather than clearing — which would read as "nowhere".
fn destination_of(page: &str) -> &str {
    match page {
        "takeout" => "gallery",
        other => other,
    }
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
        // Compared by *destination*, not by page: while a sub-page is open its
        // parent's row is the selected one, and re-asserting it here would kick
        // the user off the sub-page the moment they arrived.
        let current = ui_row.stack.visible_child_name();
        if current.as_deref().map(destination_of) != Some(*page) {
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
        // One scope covers the whole photos view, so Refresh reloads whichever of
        // the two — album grid or timeline/album — is actually on screen.
        Some("gallery") => refresh_then(ui, RefreshScope::Photos { full: false }, |ui| {
            if ui.gallery.content.visible_child_name().as_deref() == Some("albums") {
                load_albums(ui);
            } else {
                load_gallery(ui, false);
            }
        }),
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
        Some("locations") => {
            ui.locations.loaded_at.set(None);
            load_locations(ui);
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
    // A photos refresh answers as soon as it has started, because re-reading a
    // large library takes minutes. The page is therefore loaded twice: once from
    // what the daemon already holds, and again when the refresh has landed.
    let follow = matches!(scope, RefreshScope::Photos { .. });
    let rx = spawn_request(ui.dirs.control_socket(), Request::Refresh { scope });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let _ = rx.recv().await;
        load(&ui);
        if follow {
            follow_photos_refresh(&ui, load);
        }
    });
}

/// Watch the timeline refresh the daemon just started, and load the page again
/// when it finishes.
///
/// Giving up after [`PHOTOS_REFRESH_FOLLOW_LIMIT`] is not a failure: the refresh
/// keeps running in the daemon and the next visit to the page shows its result.
/// It only stops this front-end from polling a daemon forever.
fn follow_photos_refresh(ui: &Rc<Ui>, load: fn(&Rc<Ui>)) {
    let ui = ui.clone();
    let give_up_at = Instant::now() + PHOTOS_REFRESH_FOLLOW_LIMIT;
    glib::spawn_future_local(async move {
        loop {
            glib::timeout_future(PHOTOS_REFRESH_POLL).await;
            // The person may have left the gallery in the meantime; reloading a
            // page nobody is looking at would only cost the daemon work.
            if ui.stack.visible_child_name().as_deref() != Some("gallery") {
                return;
            }
            let rx = spawn_request(ui.dirs.control_socket(), Request::PhotosRefreshStatus);
            match rx.recv().await {
                Ok(Ok(Response::PhotosRefresh { running: true })) => {
                    if Instant::now() >= give_up_at {
                        return;
                    }
                }
                Ok(Ok(Response::PhotosRefresh { running: false })) => {
                    load(&ui);
                    return;
                }
                // An older daemon does not know the request, and anything else
                // means we cannot tell — either way, stop asking.
                _ => return,
            }
        }
    });
}

/// Highlight the sidebar row for whichever page the stack is showing, so
/// navigation that doesn't start in the sidebar (login landing on Files, the tray
/// raising the window) still moves the selection.
fn sync_sidebar(ui: &Rc<Ui>) {
    let Some(current) = ui.stack.visible_child_name() else {
        return;
    };
    let current = destination_of(&current);
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

/// "1 item" / "5 items": a count with the noun form that agrees with it, so
/// no user-facing string has to fall back to "item(s)".
fn count_noun(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Show a transient toast. Non-blocking by design: an action's outcome is
/// reported without stealing focus or forcing a click, so the user can keep
/// working while a slow upload lands.
fn toast(ui: &Rc<Ui>, message: &str) {
    ui.toasts.add_toast(adw::Toast::new(message));
}

/// Show a toast that offers one follow-up action — an Undo, or a way to the page
/// the outcome landed on.
///
/// Given a longer timeout than a plain toast: an action the user has to notice,
/// read and reach is not the same ask as a line of confirmation they can ignore.
fn toast_action(ui: &Rc<Ui>, message: &str, label: &str, action: impl Fn(&Rc<Ui>) + 'static) {
    let toast = adw::Toast::builder()
        .title(message)
        .button_label(label)
        .timeout(8)
        .build();
    let ui_action = ui.clone();
    toast.connect_button_clicked(move |_| action(&ui_action));
    ui.toasts.add_toast(toast);
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

/// The sidebar's primary (hamburger) menu: the app-level entries that don't
/// belong on any one page.
fn build_primary_menu() -> gtk4::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("Preferences"), Some("win.preferences"));
    menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About Proton Drive for Linux"), Some("win.about"));
    gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main menu")
        .primary(true)
        .menu_model(&menu)
        .build()
}

/// Back the primary and account menus' entries with window actions.
fn install_window_actions(ui: &Rc<Ui>, window: &adw::ApplicationWindow) {
    let preferences = gio::SimpleAction::new("preferences", None);
    let ui_prefs = ui.clone();
    let win = window.clone();
    preferences.connect_activate(move |_, _| {
        // Signed out there is nothing to configure: every page needs a session.
        if ui_prefs.session.borrow().is_some() {
            ui_prefs.status.prefs.present(Some(&win));
        }
    });
    window.add_action(&preferences);

    let sign_out_action = gio::SimpleAction::new("sign-out", None);
    let ui_out = ui.clone();
    sign_out_action.connect_activate(move |_, _| sign_out(&ui_out));
    window.add_action(&sign_out_action);

    // The Photos page's import banner leads here.
    let show_import = gio::SimpleAction::new("show-import", None);
    let ui_import = ui.clone();
    show_import.connect_activate(move |_, _| ui_import.stack.set_visible_child_name("takeout"));
    window.add_action(&show_import);

    let shortcuts = gio::SimpleAction::new("shortcuts", None);
    let win = window.clone();
    shortcuts.connect_activate(move |_, _| show_shortcuts(&win));
    window.add_action(&shortcuts);

    let about = gio::SimpleAction::new("about", None);
    let win = window.clone();
    let ui_about = ui.clone();
    about.connect_activate(move |_, _| {
        let dialog = adw::AboutDialog::builder()
            .application_name("Proton Drive for Linux")
            .application_icon("io.narl.proton-drive-linux")
            .version(pdfs_core::config::APP_VERSION)
            .developer_name("Nils Pukropp")
            .website("https://github.com/narrrl/proton-drive-linux")
            .issue_url("https://github.com/narrrl/proton-drive-linux/issues")
            .license_type(gtk4::License::MitX11)
            .comments(
                "Files-on-demand Proton Drive for the Linux desktop.\n\n\
                 Unofficial client — not affiliated with, endorsed by, or supported by Proton AG.",
            )
            .debug_info(debug_info(&ui_about))
            .debug_info_filename("proton-drive-linux-debug.txt")
            .build();
        dialog.present(Some(&win));
    });
    window.add_action(&about);
}

/// The About dialog's Troubleshooting text: what a bug report needs and what
/// used to sit in a "Developer" group on the Settings page.
fn debug_info(ui: &Rc<Ui>) -> String {
    let config = ui.dirs.load_config();
    format!(
        "App version: {}\nUser agent: {}\nlibadwaita: {}.{}.{}\nGTK: {}.{}.{}\n\
         Mountpoint: {}\nControl socket: {}\nMount service running: {}\n",
        pdfs_core::config::APP_VERSION,
        pdfs_core::config::USER_AGENT,
        adw::major_version(),
        adw::minor_version(),
        adw::micro_version(),
        gtk4::major_version(),
        gtk4::minor_version(),
        gtk4::micro_version(),
        ui.dirs.resolved_mountpoint(&config).display(),
        ui.dirs.control_socket().display(),
        if *ui.mounted.borrow() { "yes" } else { "no" },
    )
}

/// The keyboard-shortcut cheatsheet behind the menu entry, listing exactly what
/// [`install_shortcuts`], the Photos page and the photo viewer bind.
fn show_shortcuts(window: &adw::ApplicationWindow) {
    const GROUPS: [(&str, &[(&str, &str)]); 4] = [
        (
            "General",
            &[
                ("<Control>f", "Search Drive"),
                ("F5", "Refresh"),
                ("<Control>comma", "Preferences"),
                ("<Control>question", "Keyboard shortcuts"),
            ],
        ),
        (
            "Files",
            &[
                ("<Control>n", "New folder"),
                ("<Control>u", "Upload files"),
                ("<Alt>Left <Alt>Right", "Back / forward"),
                ("<Alt>Up", "Parent folder"),
                ("<Control>1 <Control>2", "Grid / list"),
                ("F2", "Rename"),
                ("Delete", "Move to Trash"),
                ("Escape", "Clear the selection"),
            ],
        ),
        (
            "Photos",
            &[
                ("<Control>plus", "Larger thumbnails"),
                ("<Control>minus", "Smaller thumbnails"),
                ("<Control>0", "Reset thumbnail size"),
                ("<Control>a", "Select all"),
            ],
        ),
        (
            "Photo Viewer",
            &[
                ("Left Right", "Previous / next photo"),
                ("Home End", "First / last photo"),
                ("i", "Show details"),
                ("f", "Fullscreen"),
                ("Delete", "Move to Trash"),
                ("Escape", "Close"),
            ],
        ),
    ];
    let page = adw::PreferencesPage::new();
    for (title, keys) in GROUPS {
        let group = adw::PreferencesGroup::builder().title(title).build();
        for (accel, action) in keys {
            let row = adw::ActionRow::builder().title(*action).build();
            row.add_suffix(
                &gtk4::ShortcutLabel::builder()
                    .accelerator(*accel)
                    .valign(gtk4::Align::Center)
                    .build(),
            );
            group.add(&row);
        }
        page.add(&group);
    }

    let dialog = adw::Dialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(460)
        .content_height(620)
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
        let alt = state.contains(gtk4::gdk::ModifierType::ALT_MASK);
        let on_browser = ui.stack.visible_child_name().as_deref() == Some("browser");
        match key {
            // Refresh works on every page, so it is matched before the
            // browser-only bindings.
            gtk4::gdk::Key::F5 => reload_current_page(&ui),
            gtk4::gdk::Key::r | gtk4::gdk::Key::R if ctrl => reload_current_page(&ui),
            // Search lives on Files, but the shortcut works from anywhere:
            // wanting to find a file is not a reason to first have to remember
            // which page owns the search box.
            gtk4::gdk::Key::f | gtk4::gdk::Key::F if ctrl => {
                if !on_browser {
                    ui.stack.set_visible_child_name("browser");
                }
                ui.browser.search.grab_focus();
            }
            gtk4::gdk::Key::n | gtk4::gdk::Key::N if ctrl && on_browser => prompt_new_folder(&ui),
            gtk4::gdk::Key::_1 if ctrl && on_browser => set_files_view(
                &ui,
                FilesView {
                    list: false,
                    ..ui.browser.view.get()
                },
            ),
            gtk4::gdk::Key::_2 if ctrl && on_browser => set_files_view(
                &ui,
                FilesView {
                    list: true,
                    ..ui.browser.view.get()
                },
            ),
            gtk4::gdk::Key::Left if alt && on_browser => {
                ui.browser.actions.activate_action("back", None)
            }
            gtk4::gdk::Key::Right if alt && on_browser => {
                ui.browser.actions.activate_action("forward", None)
            }
            gtk4::gdk::Key::Up if alt && on_browser => {
                ui.browser.actions.activate_action("up", None)
            }
            gtk4::gdk::Key::u | gtk4::gdk::Key::U if ctrl && on_browser => prompt_upload(&ui),
            gtk4::gdk::Key::F2 if on_browser => {
                // Renaming is one name at a time; saying so beats a key that
                // silently does nothing with several items highlighted.
                if selected_entries(&ui).len() > 1 {
                    toast(&ui, "Select a single item to rename it");
                } else if let Some(entry) = selected_entry(&ui) {
                    prompt_rename(&ui, &entry);
                }
            }
            // Acts on the whole selection: Delete with five files highlighted
            // means those five, not whichever one the details pane happens to
            // be describing.
            gtk4::gdk::Key::Delete if on_browser => {
                let entries = selected_entries(&ui);
                if !entries.is_empty() {
                    trash_entries(&ui, entries);
                }
            }
            gtk4::gdk::Key::Escape
                if on_browser
                    && (ui.browser.split.shows_sidebar() || ui.browser.bulk.reveals_child()) =>
            {
                clear_selection(&ui);
                sync_bulk_bar(&ui);
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

/// Ask before an action that has no Undo. `on_confirm` runs only when the user
/// picks `action`; Cancel is the default, so Enter never destroys anything.
fn confirm_destructive(
    parent: &impl IsA<gtk4::Widget>,
    heading: &str,
    body: &str,
    action: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", action);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("confirm"), move |_, _| on_confirm());
    dialog.present(Some(parent));
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

/// Open a local path with the handler the user configured — `xdg-open` unless
/// `open_with` in `config.json` overrides it for this kind of file.
fn open_path(path: &str) {
    let path = Path::new(path);
    pdfs_core::opener::open_default(path, path.is_dir());
}

/// [`open_path`] for a file the daemon materialised into the content cache,
/// where the on-disk name is a content hash and carries no extension for the
/// rules (or `xdg-open`) to key off. `name` is the Drive name it was opened as.
pub(crate) fn open_named_path(path: &str, name: &str) {
    pdfs_core::opener::open_default_named(Path::new(path), name, false);
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

    /// The full width a row occupies, gaps included.
    fn row_width(widths: &[i32]) -> i32 {
        widths.iter().sum::<i32>() + TILE_GAP * (widths.len() as i32 - 1)
    }

    /// Ratios of a typical mixed day: landscape phone shots, a portrait, a
    /// square crop.
    fn mixed() -> Vec<f64> {
        vec![
            1.5, 1.5, 0.75, 1.0, 1.33, 1.5, 0.75, 1.78, 1.0, 1.5, 1.5, 1.33,
        ]
    }

    #[test]
    fn every_full_row_spans_the_content_width() {
        // The point of justifying: no ragged right margin at any width, at any
        // zoom, for any mix of shapes.
        for width in [640, 900, 1000, 1440, 1920, 2560] {
            let rows = plan_rows(&mixed(), width, ROW_DEFAULT);
            // The last row is deliberately short, so it is not part of this.
            for (_, widths) in rows.iter().take(rows.len() - 1) {
                assert_eq!(row_width(widths), width, "row does not span {width}px");
            }
        }
    }

    #[test]
    fn a_tile_keeps_its_photos_shape() {
        // Each tile is its photo's ratio at the row's height, which is what
        // makes cropping unnecessary.
        let ratios = mixed();
        let rows = plan_rows(&ratios, 1440, ROW_DEFAULT);
        let (height, widths) = &rows[0];
        for (width, ratio) in widths.iter().zip(&ratios).take(widths.len() - 1) {
            let laid_out = *width as f64 / *height as f64;
            assert!((laid_out - ratio).abs() < 0.02, "{laid_out} is not {ratio}");
        }
    }

    #[test]
    fn rows_land_near_the_target_height() {
        // A row is closed as soon as it no longer fits at the target, so it is
        // never taller than the target and never far below it.
        for target in [ROW_MIN, ROW_DEFAULT, ROW_MAX] {
            let rows = plan_rows(&mixed(), 1600, target);
            for (height, _) in rows.iter().take(rows.len() - 1) {
                assert!(
                    *height <= target,
                    "{height}px exceeds the {target}px target"
                );
                assert!(*height > target / 2, "{height}px is far under {target}px");
            }
        }
    }

    #[test]
    fn a_short_day_is_not_stretched_across_the_window() {
        // Two photos are two photos, not two half-window tiles.
        let rows = plan_rows(&[1.5, 1.5], 1920, ROW_DEFAULT);
        assert_eq!(rows.len(), 1);
        let (height, widths) = &rows[0];
        assert_eq!(*height, ROW_DEFAULT);
        assert!(row_width(widths) < 1920 / 2);
    }

    #[test]
    fn zooming_in_puts_fewer_photos_in_a_row() {
        let small = plan_rows(&mixed(), 1200, ROW_MIN);
        let big = plan_rows(&mixed(), 1200, ROW_MAX);
        assert!(small[0].1.len() > big[0].1.len());
    }

    #[test]
    fn every_photo_is_laid_out_exactly_once() {
        let ratios = mixed();
        let placed: usize = plan_rows(&ratios, 1000, ROW_DEFAULT)
            .iter()
            .map(|(_, widths)| widths.len())
            .sum();
        assert_eq!(placed, ratios.len());
        assert!(plan_rows(&[], 1000, ROW_DEFAULT).is_empty());
    }

    /// One file of a shot, for the group-switch tests.
    fn member(uid: &str, name: Option<&str>, kind: PhotoKind) -> PhotoItem {
        PhotoItem {
            uid: uid.into(),
            capture_time: 0,
            thumb_path: None,
            name: name.map(str::to_string),
            ratio: None,
            no_thumb: false,
            kind,
            favorite: false,
            group_size: 2,
            has_raw: kind == PhotoKind::Raw,
        }
    }

    /// The lightbox steps through a shot's files and comes back round, so a
    /// burst of three is as reachable as a RAW+JPEG pair.
    #[test]
    fn the_group_switch_cycles_through_every_file() {
        let members = [
            member("jpeg", Some("IMG_1.JPG"), PhotoKind::Photo),
            member("raw", Some("IMG_1.CR2"), PhotoKind::Raw),
            member("clip", None, PhotoKind::Video),
        ];
        let next = |uid| next_group_member(&members, uid).map(|item| item.uid.as_str());
        assert_eq!(next("jpeg"), Some("raw"));
        assert_eq!(next("raw"), Some("clip"));
        assert_eq!(next("clip"), Some("jpeg"));
        // A photo that is not in the group leaves the button alone rather than
        // jumping somewhere arbitrary.
        assert_eq!(next("elsewhere"), None);
        assert!(next_group_member(&[], "jpeg").is_none());
    }

    /// The switch names what it will show; a file whose name the daemon has not
    /// resolved still gets a sentence rather than a blank.
    #[test]
    fn a_group_member_without_a_name_is_still_described() {
        assert_eq!(
            member_label(&member("raw", Some("IMG_1.CR2"), PhotoKind::Raw)),
            "IMG_1.CR2"
        );
        assert_eq!(
            member_label(&member("raw", None, PhotoKind::Raw)),
            "the raw file"
        );
        assert_eq!(
            member_label(&member("clip", Some(""), PhotoKind::Video)),
            "the video"
        );
    }

    #[test]
    fn a_window_narrower_than_one_photo_still_lays_it_out() {
        // A tile of at least one px, rather than a zero-width widget or a
        // division by zero.
        for width in [0, 40] {
            let rows = plan_rows(&[1.5, 1.5, 1.5], width, ROW_DEFAULT);
            assert!(rows.iter().all(|(h, w)| *h > 0 && w.iter().all(|w| *w > 0)));
        }
    }
}
