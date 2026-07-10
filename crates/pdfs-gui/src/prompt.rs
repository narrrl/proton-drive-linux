//! `pdfs-prompt` — a fast, Google Drive-style launcher prompt for searching
//! and opening files or folders in Proton Drive.
//!
//! Designed to be bound to a system shortcut (e.g. in Hyprland) for quick HUD search.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::glib;

use pdfs_core::config::AppDirs;
use pdfs_core::control::{Request, Response, SearchHit, send};

const APP_ID: &str = "io.narl.proton-drive-linux-prompt";
const PROTON_PURPLE: &str = "#6d4aff";
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileFilter {
    All,
    Folders,
    PDFs,
    Documents,
    Images,
}

/// Run one blocking control-socket round-trip on a worker thread, returning a
/// channel that yields the [`Response`] once.
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

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        load_prompt_theme();
    });

    app.connect_activate(build_window);

    app.run()
}

/// Override accent colors and style the launcher for a premium HUD look.
fn load_prompt_theme() {
    let css = format!(
        "@define-color accent_bg_color {PROTON_PURPLE};\n\
         @define-color accent_color {PROTON_PURPLE};\n\
         \n\
         window.launcher-window {{\n\
             background-color: transparent;\n\
         }}\n\
         \n\
         .launcher-container {{\n\
             background: linear-gradient(135deg, #1e1b2e 0%, #14121e 100%);\n\
             border: 1px solid rgba(109, 74, 255, 0.3);\n\
             border-radius: 16px;\n\
             padding: 16px;\n\
             box-shadow: 0 20px 50px rgba(0, 0, 0, 0.6), 0 0 30px rgba(109, 74, 255, 0.15);\n\
         }}\n\
         \n\
         .launcher-header-box {{\n\
             padding: 10px 16px;\n\
             background-color: rgba(255, 255, 255, 0.04);\n\
             border-radius: 12px;\n\
             border: 1px solid rgba(109, 74, 255, 0.2);\n\
         }}\n\
         \n\
         .launcher-entry {{\n\
             font-size: 1.25rem;\n\
             font-weight: 500;\n\
             background: transparent;\n\
             border: none;\n\
             box-shadow: none;\n\
             color: white;\n\
         }}\n\
         \n\
         .filter-bar {{\n\
             margin: 8px 0px 4px 0px;\n\
         }}\n\
         \n\
         .filter-chip {{\n\
             background-color: rgba(255, 255, 255, 0.03);\n\
             border: 1px solid rgba(255, 255, 255, 0.08);\n\
             border-radius: 20px;\n\
             padding: 6px 14px;\n\
             color: rgba(255, 255, 255, 0.7);\n\
             font-size: 0.85rem;\n\
             font-weight: 500;\n\
             transition: all 0.15s ease-in-out;\n\
         }}\n\
         \n\
         .filter-chip:hover {{\n\
             background-color: rgba(109, 74, 255, 0.15);\n\
             border-color: rgba(109, 74, 255, 0.4);\n\
             color: white;\n\
         }}\n\
         \n\
         .filter-chip.active {{\n\
             background-color: {PROTON_PURPLE};\n\
             border-color: {PROTON_PURPLE};\n\
             color: white;\n\
             box-shadow: 0 2px 8px rgba(109, 74, 255, 0.4);\n\
         }}\n\
         \n\
         .launcher-listbox {{\n\
             background: transparent;\n\
         }}\n\
         \n\
         .launcher-row {{\n\
             padding: 12px 16px;\n\
             border-radius: 10px;\n\
             margin: 3px 0px;\n\
             transition: background-color 0.15s ease, color 0.15s ease;\n\
         }}\n\
         \n\
         .launcher-row:hover {{\n\
             background-color: rgba(109, 74, 255, 0.08);\n\
         }}\n\
         \n\
         .launcher-row:selected {{\n\
             background-color: {PROTON_PURPLE};\n\
             color: white;\n\
             box-shadow: 0 4px 12px rgba(109, 74, 255, 0.3);\n\
         }}\n\
         \n\
         .launcher-row:selected .dim-label {{\n\
             color: rgba(255, 255, 255, 0.7);\n\
             opacity: 0.9;\n\
         }}\n\
         \n\
         .launcher-row:selected .icon-folder {{\n\
             color: white;\n\
         }}\n\
         \n\
         .launcher-row:selected .icon-pdf {{\n\
             color: white;\n\
         }}\n\
         \n\
         .launcher-row:selected .icon-doc {{\n\
             color: white;\n\
         }}\n\
         \n\
         .launcher-row:selected .icon-image {{\n\
             color: white;\n\
         }}\n\
         \n\
         .launcher-row:selected .icon-pin {{\n\
             color: white;\n\
         }}\n\
         \n\
         .launcher-footer {{\n\
             padding: 10px 12px 0px 12px;\n\
             font-size: 0.82rem;\n\
             border-top: 1px solid rgba(255, 255, 255, 0.08);\n\
         }}\n\
         \n\
         .dim-label {{\n\
             color: rgba(255, 255, 255, 0.45);\n\
             font-size: 0.85rem;\n\
         }}\n\
         \n\
         .icon-folder {{\n\
             color: #35b3ff;\n\
         }}\n\
         \n\
         .icon-pdf {{\n\
             color: #ff4a4a;\n\
         }}\n\
         \n\
         .icon-doc {{\n\
             color: #2ecc71;\n\
         }}\n\
         \n\
         .icon-image {{\n\
             color: #e67e22;\n\
         }}\n\
         \n\
         .icon-generic {{\n\
             color: #a0a0b0;\n\
         }}\n\
         \n\
         .icon-pin {{\n\
             color: #f1c40f;\n\
         }}\n\
         \n\
         .error-box {{\n\
             padding: 24px;\n\
         }}\n\
         \n\
         .error-title {{\n\
             font-size: 1.25rem;\n\
             font-weight: bold;\n\
             margin-bottom: 8px;\n\
             color: white;\n\
         }}\n\
         \n\
         scrolledwindow viewport {{\n\
             background: transparent;\n\
         }}\n\
         \n\
         scrollbar {{\n\
             background: transparent;\n\
         }}\n\
         \n\
         scrollbar slider {{\n\
             background: rgba(255, 255, 255, 0.1);\n\
             border-radius: 4px;\n\
             min-width: 6px;\n\
             min-height: 6px;\n\
         }}\n\
         \n\
         scrollbar slider:hover {{\n\
             background: rgba(109, 74, 255, 0.4);\n\
         }}\n"
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

fn build_window(app: &adw::Application) {
    let dirs = match AppDirs::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("cannot resolve app dirs: {e}");
            return;
        }
    };
    let socket = dirs.control_socket();
    let default_mountpoint = dirs.default_mountpoint();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Proton Drive Launcher")
        .default_width(650)
        .default_height(450)
        .decorated(false)
        .build();

    window.add_css_class("launcher-window");

    // Prefer dark mode for high-tech HUD look
    let style_manager = adw::StyleManager::default();
    style_manager.set_color_scheme(adw::ColorScheme::PreferDark);

    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    container.add_css_class("launcher-container");
    window.set_content(Some(&container));

    // Header box: search icon, entry field, and loading spinner
    let header_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header_box.add_css_class("launcher-header-box");

    let search_icon = gtk4::Image::from_icon_name("system-search-symbolic");
    header_box.append(&search_icon);

    let entry = gtk4::Entry::builder()
        .placeholder_text("Search Proton Drive...")
        .hexpand(true)
        .build();
    entry.add_css_class("launcher-entry");
    header_box.append(&entry);

    let spinner = gtk4::Spinner::new();
    spinner.set_visible(false);
    header_box.append(&spinner);

    container.append(&header_box);

    // Horizontal box for Google Drive style filter chips
    let filter_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    filter_box.add_css_class("filter-bar");
    container.append(&filter_box);

    let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    container.append(&separator);

    // View stack to transition between results list and warning page
    let stack = adw::ViewStack::new();
    container.append(&stack);

    // Page 1: Scrolled window and listbox for results
    let list_box = gtk4::ListBox::new();
    list_box.add_css_class("launcher-listbox");

    let scrolled_window = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .child(&list_box)
        .build();
    stack.add_named(&scrolled_window, Some("search"));

    // Page 2: Error screen if daemon is offline
    let error_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    error_box.add_css_class("error-box");
    error_box.set_valign(gtk4::Align::Center);
    error_box.set_halign(gtk4::Align::Center);

    let error_icon = gtk4::Image::from_icon_name("dialog-warning-symbolic");
    error_icon.set_pixel_size(48);
    error_icon.add_css_class("brand-icon");
    error_box.append(&error_icon);

    let error_title = gtk4::Label::builder()
        .label("Daemon Offline")
        .justify(gtk4::Justification::Center)
        .build();
    error_title.add_css_class("error-title");
    error_box.append(&error_title);

    let error_desc = gtk4::Label::builder()
        .label("Proton Drive mount daemon is not running.\nStart the daemon or run `pdfs mount` to connect.")
        .justify(gtk4::Justification::Center)
        .build();
    error_desc.add_css_class("dim-label");
    error_box.append(&error_desc);

    let retry_button = gtk4::Button::builder()
        .label("Retry Connection")
        .halign(gtk4::Align::Center)
        .build();
    retry_button.add_css_class("suggested-action");
    retry_button.add_css_class("pill");
    error_box.append(&retry_button);

    stack.add_named(&error_box, Some("error"));

    // Footer box: status label and shortcut helper
    let footer_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    footer_box.add_css_class("launcher-footer");

    let status_label = gtk4::Label::builder()
        .label("Initialising...")
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .hexpand(true)
        .build();
    status_label.add_css_class("dim-label");
    footer_box.append(&status_label);

    let shortcut_guide = gtk4::Label::builder()
        .label("Tab: cycle filters  •  Enter: open  •  Esc: close")
        .halign(gtk4::Align::End)
        .build();
    shortcut_guide.add_css_class("dim-label");
    footer_box.append(&shortcut_guide);

    container.append(&footer_box);

    // Shared state variables
    let hits = Rc::new(RefCell::new(Vec::<SearchHit>::new()));
    let raw_hits = Rc::new(RefCell::new(Vec::<SearchHit>::new()));
    let current_filter = Rc::new(Cell::new(FileFilter::All));
    let mountpoint = Rc::new(RefCell::new(default_mountpoint.display().to_string()));
    let is_opening = Rc::new(Cell::new(false));

    // Define filter button mapping and setup select closure
    let filter_buttons = Rc::new(RefCell::new(Vec::<(FileFilter, gtk4::Button)>::new()));

    let select_filter_rc = {
        let current_filter = current_filter.clone();
        let filter_buttons = filter_buttons.clone();
        let list_box = list_box.clone();
        let hits = hits.clone();
        let raw_hits = raw_hits.clone();
        let status_label = status_label.clone();
        
        Rc::new(move |filter: FileFilter| {
            current_filter.set(filter);
            
            // Update active CSS class on buttons
            for (f, btn) in filter_buttons.borrow().iter() {
                if *f == filter {
                    btn.add_css_class("active");
                } else {
                    btn.remove_css_class("active");
                }
            }
            
            // Re-apply filter
            apply_filter(&list_box, &hits, &raw_hits.borrow(), filter);
            
            // Update status text based on filter matches
            let filtered_count = hits.borrow().len();
            let total_count = raw_hits.borrow().len();
            if total_count == 0 {
                status_label.set_text("No files loaded.");
            } else if filter == FileFilter::All {
                status_label.set_text(&format!("Showing all {total_count} files."));
            } else {
                status_label.set_text(&format!(
                    "Found {filtered_count} matches (filtered from {total_count} total)."
                ));
            }
        })
    };

    // Populate filter bar
    let filter_names = [
        (FileFilter::All, "All", "system-search-symbolic"),
        (FileFilter::Folders, "Folders", "folder-symbolic"),
        (FileFilter::PDFs, "PDFs", "document-symbolic"),
        (FileFilter::Documents, "Documents", "document-symbolic"),
        (FileFilter::Images, "Images", "image-symbolic"),
    ];

    for (filter_type, label, icon) in filter_names {
        let btn = gtk4::Button::builder()
            .label(label)
            .icon_name(icon)
            .build();
        btn.add_css_class("filter-chip");
        if filter_type == FileFilter::All {
            btn.add_css_class("active");
        }
        
        let select_filter_clone = select_filter_rc.clone();
        btn.connect_clicked(move |_| {
            select_filter_clone(filter_type);
        });
        
        filter_box.append(&btn);
        filter_buttons.borrow_mut().push((filter_type, btn));
    }

    // Handle retry on connection failure
    let stack_clone = stack.clone();
    let socket_clone = socket.clone();
    let mountpoint_clone = mountpoint.clone();
    let status_label_clone = status_label.clone();
    let spinner_clone = spinner.clone();
    let raw_hits_clone = raw_hits.clone();
    let current_filter_clone = current_filter.clone();
    let select_filter_conn = select_filter_rc.clone();
    let entry_clone = entry.clone();

    let check_connection = {
        let select_filter_conn = select_filter_conn.clone();
        move || {
            status_label_clone.set_text("Connecting to daemon...");
            match send(&socket_clone, &Request::Status) {
                Ok(Response::Status { mountpoint: mp, .. }) => {
                    *mountpoint_clone.borrow_mut() = mp;
                    stack_clone.set_visible_child_name("search");
                    entry_clone.set_sensitive(true);
                    load_initial_pins(
                        &raw_hits_clone,
                        &current_filter_clone,
                        select_filter_conn.clone(),
                        &socket_clone,
                        &status_label_clone,
                        &spinner_clone,
                    );
                    true
                }
                _ => {
                    stack_clone.set_visible_child_name("error");
                    entry_clone.set_sensitive(false);
                    status_label_clone.set_text("Daemon offline.");
                    false
                }
            }
        }
    };

    let check_conn = check_connection.clone();
    retry_button.connect_clicked(move |_| {
        check_conn();
    });

    window.connect_is_active_notify(move |w| {
        if !w.is_active() {
            w.close();
        }
    });

    // Auto-focus search entry on window show
    let entry_focus = entry.clone();
    window.connect_map(move |_| {
        entry_focus.grab_focus();
    });

    // Event key controller on entry field for keyboard shortcuts
    let key_controller = gtk4::EventControllerKey::new();
    let list_box_keys = list_box.clone();
    let window_keys = window.clone();
    let entry_keys = entry.clone();
    let hits_keys = hits.clone();
    let is_opening_keys = is_opening.clone();
    let current_filter_keys = current_filter.clone();
    let select_filter_keys = select_filter_rc.clone();

    key_controller.connect_key_pressed(move |_, key, _keycode, _state| {
        if is_opening_keys.get() {
            return glib::Propagation::Stop;
        }

        match key {
            gtk4::gdk::Key::Escape => {
                window_keys.close();
                glib::Propagation::Stop
            }
            gtk4::gdk::Key::Tab => {
                let is_shift = _state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
                let next_filter = if is_shift {
                    match current_filter_keys.get() {
                        FileFilter::All => FileFilter::Images,
                        FileFilter::Folders => FileFilter::All,
                        FileFilter::PDFs => FileFilter::Folders,
                        FileFilter::Documents => FileFilter::PDFs,
                        FileFilter::Images => FileFilter::Documents,
                    }
                } else {
                    match current_filter_keys.get() {
                        FileFilter::All => FileFilter::Folders,
                        FileFilter::Folders => FileFilter::PDFs,
                        FileFilter::PDFs => FileFilter::Documents,
                        FileFilter::Documents => FileFilter::Images,
                        FileFilter::Images => FileFilter::All,
                    }
                };
                select_filter_keys(next_filter);
                glib::Propagation::Stop
            }
            gtk4::gdk::Key::Down => {
                let count = hits_keys.borrow().len();
                if count > 0 {
                    let selected = list_box_keys.selected_row();
                    let next_idx = if let Some(row) = selected {
                        (row.index() + 1) % count as i32
                    } else {
                        0
                    };
                    if let Some(row) = list_box_keys.row_at_index(next_idx) {
                        list_box_keys.select_row(Some(&row));
                        row.grab_focus();
                        entry_keys.grab_focus();
                    }
                }
                glib::Propagation::Stop
            }
            gtk4::gdk::Key::Up => {
                let count = hits_keys.borrow().len();
                if count > 0 {
                    let selected = list_box_keys.selected_row();
                    let next_idx = if let Some(row) = selected {
                        (row.index() - 1 + count as i32) % count as i32
                    } else {
                        (count - 1) as i32
                    };
                    if let Some(row) = list_box_keys.row_at_index(next_idx) {
                        list_box_keys.select_row(Some(&row));
                        row.grab_focus();
                        entry_keys.grab_focus();
                    }
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    entry.add_controller(key_controller);

    // Entry activation (pressing Return/Enter)
    let list_box_activate = list_box.clone();
    let hits_activate = hits.clone();
    let window_activate = window.clone();
    let socket_activate = socket.clone();
    let mountpoint_activate = mountpoint.clone();
    let status_label_activate = status_label.clone();
    let spinner_activate = spinner.clone();
    let is_opening_activate = is_opening.clone();

    entry.connect_activate(move |_| {
        if is_opening_activate.get() {
            return;
        }
        if let Some(row) = list_box_activate.selected_row() {
            let idx = row.index() as usize;
            let hits_borrow = hits_activate.borrow();
            if idx < hits_borrow.len() {
                let hit = &hits_borrow[idx];
                open_hit(
                    &window_activate,
                    hit,
                    &socket_activate,
                    &mountpoint_activate,
                    &status_label_activate,
                    &spinner_activate,
                    is_opening_activate.clone(),
                );
            }
        }
    });

    // List box row activation (mouse clicks / touch taps)
    let hits_act = hits.clone();
    let window_act = window.clone();
    let socket_act = socket.clone();
    let mountpoint_act = mountpoint.clone();
    let status_label_act = status_label.clone();
    let spinner_act = spinner.clone();
    let is_opening_act = is_opening.clone();

    list_box.connect_row_activated(move |_, row| {
        if is_opening_act.get() {
            return;
        }
        let idx = row.index() as usize;
        let hits_borrow = hits_act.borrow();
        if idx < hits_borrow.len() {
            let hit = &hits_borrow[idx];
            open_hit(
                &window_act,
                hit,
                &socket_act,
                &mountpoint_act,
                &status_label_act,
                &spinner_act,
                is_opening_act.clone(),
            );
        }
    });

    // Debounced search text entry listener
    let search_source = Rc::new(RefCell::new(None::<glib::SourceId>));
    let search_source_changed = search_source.clone();
    let raw_hits_search = raw_hits.clone();
    let current_filter_search = current_filter.clone();
    let select_filter_search = select_filter_rc.clone();
    let socket_search = socket.clone();
    let status_label_search = status_label.clone();
    let spinner_search = spinner.clone();

    entry.connect_changed(move |ent| {
        if let Some(src) = search_source_changed.borrow_mut().take() {
            src.remove();
        }

        let ent_clone = ent.clone();
        let raw_hits_clone = raw_hits_search.clone();
        let current_filter_clone = current_filter_search.clone();
        let select_filter_clone = select_filter_search.clone();
        let socket_clone = socket_search.clone();
        let status_label_clone = status_label_search.clone();
        let spinner_clone = spinner_search.clone();
        let search_source_cb = search_source_changed.clone();

        let src = glib::timeout_add_local_once(SEARCH_DEBOUNCE, move || {
            search_source_cb.borrow_mut().take();
            let query = ent_clone.text().trim().to_string();
            if query.is_empty() {
                load_initial_pins(
                    &raw_hits_clone,
                    &current_filter_clone,
                    select_filter_clone,
                    &socket_clone,
                    &status_label_clone,
                    &spinner_clone,
                );
            } else {
                run_prompt_search(
                    &raw_hits_clone,
                    &current_filter_clone,
                    select_filter_clone,
                    &socket_clone,
                    &query,
                    &status_label_clone,
                    &spinner_clone,
                );
            }
        });
        *search_source_changed.borrow_mut() = Some(src);
    });

    // Perform the initial connection and load check
    check_connection();

    window.present();
}

/// Retrieve pinned files from the daemon and render them in the results list.
fn load_initial_pins(
    raw_hits: &Rc<RefCell<Vec<SearchHit>>>,
    current_filter: &Rc<Cell<FileFilter>>,
    select_filter: Rc<dyn Fn(FileFilter)>,
    socket: &Path,
    status_label: &gtk4::Label,
    spinner: &gtk4::Spinner,
) {
    status_label.set_text("Loading pinned files...");
    spinner.set_visible(true);
    spinner.start();

    let rx = spawn_request(socket.to_path_buf(), Request::ListPins);
    let raw_hits = raw_hits.clone();
    let current_filter = current_filter.clone();
    let select_filter = select_filter.clone();
    let status_label = status_label.clone();
    let spinner = spinner.clone();

    glib::spawn_future_local(async move {
        let res = rx.recv().await;
        spinner.stop();
        spinner.set_visible(false);

        match res {
            Ok(Ok(Response::Pins { pins })) => {
                let mapped_hits: Vec<SearchHit> = pins
                    .into_iter()
                    .map(|p| {
                        let path_obj = Path::new(&p.path);
                        let name = path_obj
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(&p.path)
                            .to_string();
                        SearchHit {
                            name,
                            path: p.path,
                            is_dir: p.recursive,
                            size: 0,
                            modified: 0,
                            pinned: true,
                            uid: p.uid,
                        }
                    })
                    .collect();

                *raw_hits.borrow_mut() = mapped_hits;
                select_filter(current_filter.get());
            }
            _ => {
                status_label.set_text("Failed to load pinned files. Type to search.");
            }
        }
    });
}

/// Run search on the daemon and update the results list.
fn run_prompt_search(
    raw_hits: &Rc<RefCell<Vec<SearchHit>>>,
    current_filter: &Rc<Cell<FileFilter>>,
    select_filter: Rc<dyn Fn(FileFilter)>,
    socket: &Path,
    query: &str,
    status_label: &gtk4::Label,
    spinner: &gtk4::Spinner,
) {
    status_label.set_text("Searching...");
    spinner.set_visible(true);
    spinner.start();

    let rx = spawn_request(
        socket.to_path_buf(),
        Request::Search {
            query: query.to_string(),
            limit: 30,
        },
    );
    let raw_hits = raw_hits.clone();
    let current_filter = current_filter.clone();
    let select_filter = select_filter.clone();
    let status_label = status_label.clone();
    let spinner = spinner.clone();

    glib::spawn_future_local(async move {
        let res = rx.recv().await;
        spinner.stop();
        spinner.set_visible(false);

        match res {
            Ok(Ok(Response::SearchResults { hits: search_hits })) => {
                *raw_hits.borrow_mut() = search_hits;
                select_filter(current_filter.get());
            }
            Ok(Ok(Response::Error { message })) => {
                status_label.set_text(&format!("Search error: {message}"));
            }
            _ => {
                status_label.set_text("Search request failed.");
            }
        }
    });
}

/// Apply local filter criteria to raw hits and repaint list view.
fn apply_filter(
    list_box: &gtk4::ListBox,
    hits: &Rc<RefCell<Vec<SearchHit>>>,
    raw_hits: &[SearchHit],
    filter: FileFilter,
) {
    let filtered: Vec<SearchHit> = raw_hits
        .iter()
        .filter(|hit| match filter {
            FileFilter::All => true,
            FileFilter::Folders => hit.is_dir,
            FileFilter::PDFs => !hit.is_dir && hit.name.to_lowercase().ends_with(".pdf"),
            FileFilter::Documents => {
                !hit.is_dir
                    && (hit.name.to_lowercase().ends_with(".docx")
                        || hit.name.to_lowercase().ends_with(".doc")
                        || hit.name.to_lowercase().ends_with(".txt")
                        || hit.name.to_lowercase().ends_with(".md")
                        || hit.name.to_lowercase().ends_with(".odt")
                        || hit.name.to_lowercase().ends_with(".pdf")
                        || hit.name.to_lowercase().ends_with(".xlsx")
                        || hit.name.to_lowercase().ends_with(".xls")
                        || hit.name.to_lowercase().ends_with(".csv")
                        || hit.name.to_lowercase().ends_with(".pptx")
                        || hit.name.to_lowercase().ends_with(".ppt"))
            }
            FileFilter::Images => {
                !hit.is_dir
                    && (hit.name.to_lowercase().ends_with(".png")
                        || hit.name.to_lowercase().ends_with(".jpg")
                        || hit.name.to_lowercase().ends_with(".jpeg")
                        || hit.name.to_lowercase().ends_with(".webp")
                        || hit.name.to_lowercase().ends_with(".gif")
                        || hit.name.to_lowercase().ends_with(".bmp")
                        || hit.name.to_lowercase().ends_with(".svg"))
            }
        })
        .cloned()
        .collect();

    repaint_prompt_results(list_box, hits, &filtered);
}

/// Render the provided search hits into the ListBox.
fn repaint_prompt_results(
    list_box: &gtk4::ListBox,
    hits: &Rc<RefCell<Vec<SearchHit>>>,
    new_hits: &[SearchHit],
) {
    // Clear all rows
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    *hits.borrow_mut() = new_hits.to_vec();

    for hit in new_hits {
        let row = gtk4::ListBoxRow::new();
        row.add_css_class("launcher-row");

        let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);

        // Select icon and add appropriate class for modern custom styling colors
        let (icon_name, icon_class) = if hit.is_dir {
            ("folder-symbolic", "icon-folder")
        } else {
            let ext = hit.name.split('.').last().unwrap_or("").to_lowercase();
            if ext == "pdf" {
                ("document-symbolic", "icon-pdf")
            } else if ["docx", "doc", "txt", "md", "odt", "xlsx", "xls", "csv", "pptx", "ppt"].contains(&ext.as_str()) {
                ("document-symbolic", "icon-doc")
            } else if ["png", "jpg", "jpeg", "webp", "gif", "bmp", "svg"].contains(&ext.as_str()) {
                ("image-symbolic", "icon-image")
            } else {
                ("document-symbolic", "icon-generic")
            }
        };
        let icon = gtk4::Image::from_icon_name(icon_name);
        icon.add_css_class(icon_class);
        row_box.append(&icon);

        let text_box = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        text_box.set_hexpand(true);

        let name_label = gtk4::Label::builder()
            .label(&hit.name)
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();

        let path_label = gtk4::Label::builder()
            .label(&hit.path)
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        path_label.add_css_class("dim-label");

        text_box.append(&name_label);
        text_box.append(&path_label);

        row_box.append(&text_box);

        // Right details box
        let right_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        right_box.set_valign(gtk4::Align::Center);

        if hit.pinned {
            let pin_icon = gtk4::Image::from_icon_name("emblem-favorite-symbolic");
            pin_icon.add_css_class("icon-pin");
            right_box.append(&pin_icon);
        }

        if !hit.is_dir && hit.size > 0 {
            let size_str = format_size(hit.size);
            let size_label = gtk4::Label::builder()
                .label(&size_str)
                .build();
            size_label.add_css_class("dim-label");
            right_box.append(&size_label);
        }

        if hit.modified > 0 {
            let time_str = format_relative_time(hit.modified);
            let time_label = gtk4::Label::builder()
                .label(&time_str)
                .build();
            time_label.add_css_class("dim-label");
            right_box.append(&time_label);
        }

        row_box.append(&right_box);

        row.set_child(Some(&row_box));
        list_box.append(&row);
    }

    // Automatically select the first item
    if !new_hits.is_empty() {
        if let Some(row) = list_box.row_at_index(0) {
            list_box.select_row(Some(&row));
        }
    }
}

/// Open the selected search hit. Directories open FUSE directly, files trigger cache download.
fn open_hit(
    window: &adw::ApplicationWindow,
    hit: &SearchHit,
    socket: &Path,
    mountpoint: &Rc<RefCell<String>>,
    status_label: &gtk4::Label,
    spinner: &gtk4::Spinner,
    is_opening: Rc<Cell<bool>>,
) {
    if hit.is_dir {
        let mp = mountpoint.borrow();
        let full_path = Path::new(&*mp).join(&hit.path);
        tracing::info!("Opening directory: {}", full_path.display());
        if let Err(e) = Command::new("xdg-open").arg(&full_path).spawn() {
            tracing::error!("Failed to xdg-open directory: {e}");
        }
        window.close();
    } else {
        is_opening.set(true);
        status_label.set_text(&format!("Downloading {}...", hit.name));
        spinner.set_visible(true);
        spinner.start();

        let rx = spawn_request(
            socket.to_path_buf(),
            Request::OpenFile {
                path: hit.path.clone(),
            },
        );
        let window_clone = window.clone();
        let status_label_clone = status_label.clone();
        let spinner_clone = spinner.clone();
        let is_opening_clone = is_opening.clone();

        glib::spawn_future_local(async move {
            let res = rx.recv().await;
            spinner_clone.stop();
            spinner_clone.set_visible(false);
            is_opening_clone.set(false);

            match res {
                Ok(Ok(Response::FilePath { path })) => {
                    tracing::info!("Opening file: {}", path);
                    if let Err(e) = Command::new("xdg-open").arg(&path).spawn() {
                        tracing::error!("Failed to xdg-open file: {e}");
                    }
                    window_clone.close();
                }
                Ok(Ok(Response::Error { message })) => {
                    status_label_clone.set_text(&format!("Error: {message}"));
                }
                _ => {
                    status_label_clone.set_text("Error: Failed to communicate with daemon.");
                }
            }
        });
    }
}

/// Helper function to format sizes in human-readable terms.
fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Helper function to format relative timestamps.
fn format_relative_time(epoch_secs: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    
    let diff = now - epoch_secs;
    if diff <= 0 {
        return "Just now".to_string();
    }
    if diff < 60 {
        return "Just now".to_string();
    }
    let mins = diff / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    
    if days < 30 {
        return format!("{days}d ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo ago");
    }
    let years = months / 12;
    format!("{years}y ago")
}
