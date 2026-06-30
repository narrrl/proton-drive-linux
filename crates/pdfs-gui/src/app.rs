//! `pdfs-app` — GTK4 / libadwaita desktop window for the Proton Drive Linux
//! client.
//!
//! Slice 2 of the GUI phase: the settings/management window that complements the
//! [`pdfs-tray`](crate) SNI icon. It presents four things in a Proton-purple,
//! Google-Drive-style layout:
//!
//! * a **login form** (email / password / optional 2FA) driving
//!   [`pdfs_core::auth::login`]; logging in enables the systemd mount service;
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
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::gio;
use gtk4::glib;
use gtk4::glib::BoxedAnyObject;

use pdfs_core::auth;
use pdfs_core::config::AppDirs;
use pdfs_core::control::{DirEntry, PhotoItem, Request, Response, send};
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

/// One rendered pin row, retained so [`repaint_pins`] can flip the unpin button's
/// `sensitive` in place (when the pin set is unchanged) instead of rebuilding.
struct PinRow {
    row: adw::ActionRow,
    /// The unpin button, absent on the placeholder row.
    unpin: Option<gtk4::Button>,
}

/// All widgets the periodic refresh and the action handlers mutate, plus the
/// resolved paths they act on. Wrapped in an [`Rc`] so handlers and the timeout
/// closure share one instance.
struct Ui {
    dirs: AppDirs,
    stack: adw::ViewStack,
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
    // Login page.
    email: adw::EntryRow,
    password: adw::PasswordEntryRow,
    totp: adw::EntryRow,
    login_button: gtk4::Button,
    login_status: gtk4::Label,
    // Main page.
    account_row: adw::ActionRow,
    /// Read-only mount status line. The mount is driven by the systemd user
    /// service (enabled on login), not by the user — so this only reports.
    mount_row: adw::ActionRow,
    cache_bar: gtk4::ProgressBar,
    cache_label: gtk4::Label,
    pins_group: adw::PreferencesGroup,
    /// Rows currently shown under [`Self::pins_group`], retained so a refresh can
    /// diff against them and only rebuild when the pin set actually changes.
    pin_rows: RefCell<Vec<PinRow>>,
    /// The pin paths last rendered, the diff baseline for [`repaint_pins`].
    /// `None` = nothing built yet; `Some(empty)` = the placeholder is shown.
    pins_state: RefCell<Option<Vec<String>>>,
    /// Whether the last refresh saw a live mount daemon. Gates the unpin
    /// buttons, which need the daemon to evict + re-hydrate.
    mounted: RefCell<bool>,
    /// Bottom switcher between the Account / Files / Photos pages; hidden on the
    /// login page so the user can't jump to pages that need a session.
    switcher: adw::ViewSwitcherBar,
    // Files (browser) page.
    /// Shared model behind the grid and column views; repopulated per directory.
    browser_model: gio::ListStore,
    browser_back: gtk4::Button,
    browser_crumb: gtk4::Label,
    browser_status: gtk4::Label,
    /// Shown beside [`Self::browser_status`] when a load failed because the mount
    /// service is down (not merely starting); restarts the service and reloads.
    browser_retry: gtk4::Button,
    /// Mountpoint-relative path the browser is showing (empty = root).
    browser_path: RefCell<String>,
    // Photos (gallery) page.
    gallery_model: gio::ListStore,
    gallery_status: gtk4::Label,
    gallery_retry: gtk4::Button,
    gallery_more: gtk4::Button,
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
    let css = format!(
        "@define-color accent_bg_color {PROTON_PURPLE};\n\
         @define-color accent_color {PROTON_PURPLE};\n\
         .brand-title {{ font-size: 1.6rem; font-weight: 800; }}\n\
         .brand-icon {{ color: {PROTON_PURPLE}; }}\n\
         .file-grid {{ padding: 6px; }}\n\
         .file-tile {{ padding: 8px; border-radius: 10px; }}\n\
         .file-tile:hover {{ background: alpha({PROTON_PURPLE}, 0.10); }}\n"
    );
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
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
    // Login has no title, so it never appears in the switcher; the three signed-in
    // pages are titled + iconed so the bottom switcher lists them.
    stack.add_named(&login_page, Some("login"));
    stack.add_titled_with_icon(
        &main_page,
        Some("main"),
        "Account",
        "avatar-default-symbolic",
    );
    stack.add_titled_with_icon(&browser_page, Some("browser"), "Files", "folder-symbolic");
    stack.add_titled_with_icon(
        &gallery_page,
        Some("gallery"),
        "Photos",
        "image-x-generic-symbolic",
    );

    let switcher = adw::ViewSwitcherBar::builder().stack(&stack).build();

    // Header spinner, hidden until a background open/load is in flight.
    let spinner = gtk4::Spinner::new();
    spinner.set_visible(false);

    let ui = Rc::new(Ui {
        dirs,
        stack: stack.clone(),
        spinner: spinner.clone(),
        busy: Cell::new(0),
        opening: RefCell::new(HashSet::new()),
        session: RefCell::new(auth::load().ok()),
        status_inflight: Cell::new(false),
        email: login_widgets.0,
        password: login_widgets.1,
        totp: login_widgets.2,
        login_button: login_widgets.3,
        login_status: login_widgets.4,
        account_row: main_widgets.0,
        mount_row: main_widgets.1,
        cache_bar: main_widgets.2,
        cache_label: main_widgets.3,
        pins_group: main_widgets.4,
        pin_rows: RefCell::new(Vec::new()),
        pins_state: RefCell::new(None),
        mounted: RefCell::new(false),
        switcher: switcher.clone(),
        browser_model: browser_widgets.0,
        browser_back: browser_widgets.1,
        browser_crumb: browser_widgets.2,
        browser_status: browser_widgets.3,
        browser_retry: browser_widgets.6,
        browser_path: RefCell::new(String::new()),
        gallery_model: gallery_widgets.0,
        gallery_status: gallery_widgets.1,
        gallery_retry: gallery_widgets.4,
        gallery_more: gallery_widgets.2,
    });

    wire_login(&ui);
    wire_logout(&ui, &main_widgets.5);
    wire_browser(&ui, &browser_widgets.4, &browser_widgets.5);
    wire_gallery(&ui, &gallery_widgets.3);
    wire_retry(&ui);

    // Lazily load the Files / Photos pages the first time they're shown, so the
    // network round-trip only happens on demand rather than on every refresh.
    let ui_nav = ui.clone();
    stack.connect_visible_child_name_notify(move |st| match st.visible_child_name().as_deref() {
        Some("browser") => load_browser(&ui_nav),
        Some("gallery") => load_gallery(&ui_nav, false),
        _ => {}
    });

    // Header with a Proton-branded title; the content is the page stack inside a
    // clamp so it stays comfortably narrow on a wide window, Google-Drive style.
    let header = adw::HeaderBar::new();
    let brand = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let icon = gtk4::Image::from_icon_name("folder-remote-symbolic");
    icon.add_css_class("brand-icon");
    brand.append(&icon);
    brand.append(&gtk4::Label::new(Some("Proton Drive")));
    header.set_title_widget(Some(&brand));
    header.pack_end(&spinner);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    toolbar.add_bottom_bar(&switcher);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Proton Drive")
        .default_width(760)
        .default_height(620)
        .content(&toolbar)
        .build();

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

/// The login page: an email row, password row, optional 2FA row, a primary
/// "Sign in" button and a status label, centred in a clamp.
#[allow(clippy::type_complexity)]
fn build_login_page() -> (
    gtk4::Widget,
    (
        adw::EntryRow,
        adw::PasswordEntryRow,
        adw::EntryRow,
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
    let totp = adw::EntryRow::builder()
        .title("Two-factor code (if enabled)")
        .build();
    group.add(&email);
    group.add(&password);
    group.add(&totp);

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
        (email, password, totp, login_button, login_status),
    )
}

/// The main (logged-in) page: account header, mount toggle, storage usage and
/// the pin list. Returns the widgets the refresh loop updates plus the logout
/// button to wire.
#[allow(clippy::type_complexity)]
fn build_main_page() -> (
    gtk4::Widget,
    (
        adw::ActionRow,
        adw::ActionRow,
        gtk4::ProgressBar,
        gtk4::Label,
        adw::PreferencesGroup,
        gtk4::Button,
    ),
) {
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

    // Storage group: a progress bar + "X of Y used" label.
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
    storage_group.add(&storage_box);

    // Pins group: filled in by refresh.
    let pins_group = adw::PreferencesGroup::builder()
        .title("Pinned files")
        .description("Kept available offline on this device.")
        .build();

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    inner.set_margin_top(18);
    inner.set_margin_bottom(18);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&account_group);
    inner.append(&mount_group);
    inner.append(&storage_group);
    inner.append(&pins_group);

    let clamp = adw::Clamp::builder()
        .maximum_size(560)
        .child(&inner)
        .build();
    let scroll = gtk4::ScrolledWindow::builder().child(&clamp).build();

    (
        scroll.upcast(),
        (
            account_row,
            mount_row,
            cache_bar,
            cache_label,
            pins_group,
            logout_button,
        ),
    )
}

/// Connect the sign-in button: read the fields, run [`auth::login`] on a worker
/// thread, and report the outcome back on the main loop.
fn wire_login(ui: &Rc<Ui>) {
    let ui = ui.clone();
    let button = ui.login_button.clone();
    button.connect_clicked(move |_| {
        let username = ui.email.text().to_string();
        let password = ui.password.text().to_string();
        let totp = ui.totp.text().to_string();
        if username.is_empty() || password.is_empty() {
            ui.login_status.set_text("Enter your email and password.");
            return;
        }

        ui.login_button.set_sensitive(false);
        ui.login_status.set_text("Signing in…");
        let rx = spawn_login(username, password, totp);

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
                    ui.totp.set_text("");
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
/// runtime, returning a channel that yields the result once. The TOTP closure
/// supplies whatever was typed in the 2FA field, erroring if the account needs a
/// code but none was entered.
fn spawn_login(
    username: String,
    password: String,
    totp: String,
) -> async_channel::Receiver<Result<(), String>> {
    let (tx, rx) = async_channel::bounded(1);
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
                let code = totp.trim();
                if code.is_empty() {
                    Err(pdfs_core::Error::Other("two-factor code required".into()))
                } else {
                    Ok(code.to_string())
                }
            })
            .await
            .map_err(|e| e.to_string())
        });
        let _ = tx.send_blocking(result);
    });
    rx
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
                // Only pull the user onto "main" when they're sitting on the login
                // page; otherwise leave whichever signed-in page they navigated to.
                if ui.stack.visible_child_name().as_deref() == Some("login") {
                    ui.stack.set_visible_child_name("main");
                }
                ui.switcher.set_reveal(true);
                ui.account_row.set_title(&s.username);
                ui.account_row.set_subtitle("Proton account");
            }
            None => {
                ui.stack.set_visible_child_name("login");
                ui.switcher.set_reveal(false);
                return;
            }
        }
    }

    refresh_status(ui);
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
                ..
            })) => {
                *ui.mounted.borrow_mut() = true;
                ui.mount_row.set_subtitle(&format!("Mounted at {mountpoint}"));
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
                *ui.mounted.borrow_mut() = false;
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
#[allow(clippy::type_complexity)]
fn build_browser_page() -> (
    gtk4::Widget,
    (
        gio::ListStore,
        gtk4::Button,
        gtk4::Label,
        gtk4::Label,
        gtk4::GridView,
        gtk4::ColumnView,
        gtk4::Button,
    ),
) {
    let model = gio::ListStore::new::<BoxedAnyObject>();

    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Up one folder")
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    back.add_css_class("flat");
    let crumb = gtk4::Label::builder()
        .label("Proton Drive")
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::Start)
        .build();
    crumb.add_css_class("heading");

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

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&back);
    header.append(&crumb);
    header.append(&toggles);

    let status = gtk4::Label::builder().wrap(true).build();
    status.add_css_class("dim-label");
    status.set_visible(false);

    // Shown only when a load failed because the mount is down; restarts it.
    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Start)
        .build();
    retry.add_css_class("pill");
    retry.set_visible(false);

    let status_box = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    status_box.append(&status);
    status_box.append(&retry);

    // Icon grid.
    let grid = gtk4::GridView::builder()
        .model(&gtk4::SingleSelection::new(Some(model.clone())))
        .min_columns(2)
        .max_columns(10)
        .build();
    grid.add_css_class("file-grid");
    let grid_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&grid)
        .build();

    // Column list.
    let column_view = gtk4::ColumnView::builder()
        .model(&gtk4::SingleSelection::new(Some(model.clone())))
        .build();
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

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&status_box);
    inner.append(&view_stack);

    (
        inner.upcast(),
        (model, back, crumb, status, grid, column_view, retry),
    )
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
            tile.append(&icon);
            tile.append(&label);
            attach_context_menu(&ui, item, &tile);
            item.set_child(Some(&tile));
        }
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let tile = item.child().and_downcast::<gtk4::Box>().unwrap();
        let icon = tile.first_child().and_downcast::<gtk4::Image>().unwrap();
        let label = tile.last_child().and_downcast::<gtk4::Label>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        icon.set_icon_name(Some(icon_base_for(&entry)));
        label.set_label(&display_name(&entry));
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
            let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            cell.append(&icon);
            cell.append(&label);
            attach_context_menu(&ui, item, &cell);
            item.set_child(Some(&cell));
        }
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let cell = item.child().and_downcast::<gtk4::Box>().unwrap();
        let icon = cell.first_child().and_downcast::<gtk4::Image>().unwrap();
        let label = cell.last_child().and_downcast::<gtk4::Label>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        icon.set_icon_name(Some(&format!("{}-symbolic", icon_base_for(&entry))));
        label.set_label(&display_name(&entry));
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

    let open = menu_item("Open", "document-open-symbolic");
    let ui_open = ui.clone();
    let entry_open = entry.clone();
    let pop = popover.clone();
    open.connect_clicked(move |_| {
        pop.popdown();
        activate_entry(&ui_open, &entry_open);
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

/// Open an entry the Nautilus way: folders descend, files download-and-open.
fn activate_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    if entry.is_dir {
        *ui.browser_path.borrow_mut() = rel;
        load_browser(ui);
    } else {
        // Ignore a repeat activation of a file already downloading, so an
        // impatient double-click doesn't kick off a second round-trip.
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
                    tracing::error!("open file failed: {message}")
                }
                _ => tracing::error!("open file request failed"),
            }
        });
    }
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
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Error { message })) => tracing::error!("pin toggle failed: {message}"),
            Ok(Ok(_)) => load_browser(&ui),
            _ => tracing::error!("pin toggle request failed"),
        }
    });
}

/// Join the entry name onto the current browser directory to get its
/// mountpoint-relative path.
fn entry_rel(ui: &Rc<Ui>, entry: &DirEntry) -> String {
    let base = ui.browser_path.borrow();
    if base.is_empty() {
        entry.name.clone()
    } else {
        format!("{base}/{}", entry.name)
    }
}

/// The name shown under a tile / in the Name column, marked with a star when the
/// file is kept offline.
fn display_name(entry: &DirEntry) -> String {
    if entry.pinned {
        format!("★ {}", entry.name)
    } else {
        entry.name.clone()
    }
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
    ui.browser_crumb.set_label(if path.is_empty() {
        "Proton Drive"
    } else {
        &path
    });
    ui.browser_back.set_sensitive(!path.is_empty());

    // Drop the previous folder's rows up front: a slow reply must not leave stale
    // entries visible, where clicking one would open with a wrong relative path.
    ui.browser_model.remove_all();
    ui.browser_retry.set_visible(false);
    ui.browser_status.set_label("Loading…");
    ui.browser_status.set_visible(true);

    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::ListDir { path });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Entries { entries })) => repaint_browser(&ui, &entries),
            Ok(Ok(Response::Error { message })) => browser_message(&ui, &message),
            Ok(Ok(_)) => browser_message(&ui, "Unexpected reply from daemon."),
            Ok(Err(_)) | Err(_) => browser_unreachable(&ui),
        }
    });
}

/// Clear the model and show a single status line instead. Hides the Retry button
/// (used for "in-band" outcomes: empty folder, or an error the daemon returned).
fn browser_message(ui: &Rc<Ui>, message: &str) {
    ui.browser_model.remove_all();
    ui.browser_retry.set_visible(false);
    ui.browser_status.set_label(message);
    ui.browser_status.set_visible(true);
}

/// The daemon didn't answer. Distinguish *still starting* (auto-retry, no
/// button) from *down* (actionable error + Retry), so a cold start self-heals
/// once the systemd mount comes up but a real failure stays visible.
fn browser_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        ui.browser_status
            .set_label("Couldn't reach Proton Drive. The mount service isn't running.");
        ui.browser_status.set_visible(true);
        ui.browser_retry.set_visible(true);
        return;
    }
    ui.browser_status.set_label("Connecting to Proton Drive…");
    ui.browser_status.set_visible(true);
    ui.browser_retry.set_visible(false);
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
        browser_message(ui, "This folder is empty.");
        return;
    }
    ui.browser_status.set_visible(false);

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

/// The Photos page: a [`gtk4::GridView`] of thumbnails backed by a
/// [`gio::ListStore`] of [`BoxedAnyObject`]-wrapped [`PhotoItem`]s, plus a
/// "Load more" button and a status label.
fn build_gallery_page() -> (
    gtk4::Widget,
    (
        gio::ListStore,
        gtk4::Label,
        gtk4::Button,
        gtk4::GridView,
        gtk4::Button,
    ),
) {
    let model = gio::ListStore::new::<BoxedAnyObject>();

    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let picture = gtk4::Picture::builder()
            .content_fit(gtk4::ContentFit::Cover)
            .width_request(150)
            .height_request(150)
            .build();
        picture.add_css_class("card");
        item.set_child(Some(&picture));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let picture = item.child().and_downcast::<gtk4::Picture>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let photo = obj.borrow::<PhotoItem>();
        match photo.thumb_path.as_deref() {
            Some(p) => match gtk4::gdk::Texture::from_filename(p) {
                Ok(texture) => picture.set_paintable(Some(&texture)),
                Err(_) => picture.set_paintable(gtk4::gdk::Paintable::NONE),
            },
            None => picture.set_paintable(gtk4::gdk::Paintable::NONE),
        }
    });

    let selection = gtk4::NoSelection::new(Some(model.clone()));
    let grid = gtk4::GridView::builder()
        .model(&selection)
        .factory(&factory)
        .min_columns(2)
        .max_columns(6)
        .build();

    let status = gtk4::Label::builder().wrap(true).build();
    status.add_css_class("dim-label");
    status.set_visible(false);

    // Shown only when a load failed because the mount is down; restarts it.
    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.set_visible(false);

    let more = gtk4::Button::builder()
        .label("Load more")
        .halign(gtk4::Align::Center)
        .build();
    more.add_css_class("pill");
    more.set_visible(false);

    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&grid)
        .build();

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&status);
    inner.append(&retry);
    inner.append(&scroll);
    inner.append(&more);

    (inner.upcast(), (model, status, more, grid, retry))
}

/// Wire the gallery: activating a tile downloads the photo and opens it; the
/// "Load more" button appends the next page.
fn wire_gallery(ui: &Rc<Ui>, grid: &gtk4::GridView) {
    let ui_open = ui.clone();
    grid.connect_activate(move |grid, pos| {
        let Some(obj) = grid.model().and_then(|m| m.item(pos)) else {
            return;
        };
        let uid = obj
            .downcast_ref::<BoxedAnyObject>()
            .map(|o| o.borrow::<PhotoItem>().uid.clone());
        let Some(uid) = uid else { return };
        // Ignore a repeat activation of a photo already downloading.
        if !ui_open.opening.borrow_mut().insert(uid.clone()) {
            return;
        }
        ui_open.busy_begin();
        let rx = spawn_request(
            ui_open.dirs.control_socket(),
            Request::OpenPhoto { uid: uid.clone() },
        );
        let ui = ui_open.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            ui.busy_end();
            ui.opening.borrow_mut().remove(&uid);
            match result {
                Ok(Ok(Response::FilePath { path })) => open_path(&path),
                Ok(Ok(Response::Error { message })) => {
                    tracing::error!("open photo failed: {message}")
                }
                _ => tracing::error!("open photo request failed"),
            }
        });
    });

    let ui_more = ui.clone();
    ui.gallery_more.clone().connect_clicked(move |_| {
        load_gallery(&ui_more, true);
    });
}

/// Fetch a timeline page from the daemon. When `append` is false the model is
/// cleared first (fresh load); otherwise the next page is tacked on.
fn load_gallery(ui: &Rc<Ui>, append: bool) {
    if append {
        ui.gallery_status.set_visible(false);
    } else {
        // Fresh load: clear the grid and show Loading until the first page lands.
        ui.gallery_model.remove_all();
        ui.gallery_retry.set_visible(false);
        ui.gallery_status.set_label("Loading…");
        ui.gallery_status.set_visible(true);
    }
    let offset = ui.gallery_model.n_items() as usize;
    ui.gallery_more.set_sensitive(false);

    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::PhotosTimeline {
            offset,
            limit: PHOTOS_PAGE,
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.gallery_more.set_sensitive(true);
        match result {
            Ok(Ok(Response::Photos { available, items })) => {
                if !available {
                    gallery_message(&ui, "This account has no photos.");
                    return;
                }
                for item in &items {
                    ui.gallery_model.append(&BoxedAnyObject::new(item.clone()));
                }
                if ui.gallery_model.n_items() == 0 {
                    gallery_message(&ui, "No photos yet.");
                } else {
                    ui.gallery_status.set_visible(false);
                }
                // Offer "Load more" only when the page came back full.
                ui.gallery_more.set_visible(items.len() == PHOTOS_PAGE);
            }
            Ok(Ok(Response::Error { message })) => gallery_message(&ui, &message),
            Ok(Ok(_)) => gallery_message(&ui, "Unexpected reply from daemon."),
            Ok(Err(_)) | Err(_) => gallery_unreachable(&ui),
        }
    });
}

/// Show a gallery status line and hide the pager + Retry button.
fn gallery_message(ui: &Rc<Ui>, message: &str) {
    ui.gallery_retry.set_visible(false);
    ui.gallery_status.set_label(message);
    ui.gallery_status.set_visible(true);
    ui.gallery_more.set_visible(false);
}

/// Photos counterpart of [`browser_unreachable`]: auto-retry while the mount is
/// still starting, surface an actionable error + Retry once it's actually down.
fn gallery_unreachable(ui: &Rc<Ui>) {
    ui.gallery_more.set_visible(false);
    if service::is_failed() || !service::is_active() {
        ui.gallery_status
            .set_label("Couldn't reach Proton Drive. The mount service isn't running.");
        ui.gallery_status.set_visible(true);
        ui.gallery_retry.set_visible(true);
        return;
    }
    ui.gallery_status.set_label("Connecting to Proton Drive…");
    ui.gallery_status.set_visible(true);
    ui.gallery_retry.set_visible(false);
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
