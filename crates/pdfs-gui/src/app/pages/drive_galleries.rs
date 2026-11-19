use crate::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use pdfs_core::control::{GalleryAction, GalleryAlbum, GalleryFolder, GalleryPlace};

const PAGE: usize = 150;

#[derive(Clone)]
enum Row {
    Albums(Vec<GalleryAlbum>, i32),
    Heading(String),
    Media(Vec<(PhotoItem, (i32, i32))>),
    Folder(GalleryFolder),
}

struct Place {
    album: GalleryAlbum,
    parent_offset: f64,
}

pub(crate) struct Browser {
    ui: Rc<Ui>,
    window: glib::WeakRef<gtk4::Window>,
    rows: gio::ListStore,
    scroll: gtk4::ScrolledWindow,
    title: adw::WindowTitle,
    status: gtk4::Label,
    choose: gtk4::Button,
    trail: RefCell<Vec<Place>>,
    chooser: RefCell<Option<Vec<GalleryFolder>>>,
    cover: RefCell<Option<GalleryPlace>>,
    albums: RefCell<Vec<GalleryAlbum>>,
    items: RefCell<Vec<PhotoItem>>,
    width: Cell<i32>,
    generation: Cell<u64>,
    total: Cell<usize>,
    loading: Cell<bool>,
    unlocked: Cell<bool>,
    thumbnail_slots: tokio::sync::Semaphore,
}

impl Browser {
    pub(crate) fn close(&self) {
        if let Some(window) = self.window.upgrade() {
            window.close();
        }
    }

    pub(crate) fn refresh_visibility(self: &Rc<Self>) {
        if self.chooser.borrow().is_none() {
            self.trail.borrow_mut().clear();
            self.cover.borrow_mut().take();
            self.load(0, 0.0);
        }
    }

    fn current(&self) -> Option<GalleryAlbum> {
        self.trail.borrow().last().map(|place| place.album.clone())
    }

    fn next_request(&self) -> u64 {
        let next = self.generation.get().wrapping_add(1);
        self.generation.set(next);
        next
    }

    fn load(self: &Rc<Self>, offset: usize, restore: f64) {
        self.choose.set_visible(false);
        self.loading.set(true);
        let generation = if offset == 0 {
            self.total.set(0);
            self.albums.borrow_mut().clear();
            self.items.borrow_mut().clear();
            self.rows.remove_all();
            self.next_request()
        } else {
            self.generation.get()
        };
        let album = self.current();
        self.title.set_title(
            album
                .as_ref()
                .map_or_else(|| gettext("Drive Galleries"), |a| a.folder.name.clone())
                .as_str(),
        );
        self.status.set_label(&gettext("Loading…"));
        let rx = spawn_request(
            self.ui.dirs.control_socket(),
            Request::DriveGallery {
                action: GalleryAction::List {
                    album: album.map(|a| a.place),
                    offset,
                    limit: PAGE,
                },
            },
        );
        let browser = self.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            if browser.generation.get() != generation {
                return;
            }
            browser.loading.set(false);
            match result {
                Ok(Ok(Response::GalleryPage {
                    albums,
                    items,
                    total,
                })) => {
                    if offset == 0 {
                        *browser.albums.borrow_mut() = albums;
                    }
                    browser.items.borrow_mut().extend(items);
                    browser.total.set(total);
                    let empty = total == 0 && browser.albums.borrow().is_empty();
                    let message = if browser.cover.borrow().is_some() {
                        gettext("Choose a photo or video as the album cover.")
                    } else if empty {
                        gettext("This gallery is empty.")
                    } else {
                        String::new()
                    };
                    browser.status.set_label(&message);
                    browser.rebuild();
                    if restore > 0.0 {
                        let scroll = browser.scroll.clone();
                        glib::idle_add_local_once(move || scroll.vadjustment().set_value(restore));
                    }
                }
                Ok(Ok(Response::Error { message, .. })) => browser.status.set_label(&message),
                _ => browser
                    .status
                    .set_label(&gettext("Couldn't reach Proton Drive.")),
            }
        });
    }

    fn browse_folders(self: &Rc<Self>) {
        let Some(trail) = self.chooser.borrow().clone() else {
            return;
        };
        let generation = self.next_request();
        let parent = trail.last().map(|folder| folder.uid.clone());
        self.choose.set_visible(parent.is_some());
        self.title.set_title(&gettext("Choose a Drive Folder"));
        self.rows.remove_all();
        let rx = spawn_request(
            self.ui.dirs.control_socket(),
            Request::DriveGallery {
                action: GalleryAction::Folders { parent },
            },
        );
        let browser = self.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            if browser.generation.get() != generation {
                return;
            }
            match result {
                Ok(Ok(Response::GalleryFolders { folders })) => {
                    browser
                        .status
                        .set_label(&gettext("Open a folder, then choose Add This Folder."));
                    for folder in folders {
                        browser
                            .rows
                            .append(&BoxedAnyObject::new(Row::Folder(folder)));
                    }
                }
                Ok(Ok(Response::Error { message, .. })) => browser.status.set_label(&message),
                _ => browser
                    .status
                    .set_label(&gettext("Couldn't reach Proton Drive.")),
            }
        });
    }

    fn change(self: &Rc<Self>, action: GalleryAction) {
        let unlock = matches!(action, GalleryAction::Unlock { .. });
        let lock = matches!(action, GalleryAction::Lock);
        if lock {
            self.trail.borrow_mut().clear();
            self.cover.borrow_mut().take();
            self.items.borrow_mut().clear();
            self.rows.remove_all();
        }
        let generation = self.next_request();
        self.status.set_label(&gettext("Updating gallery…"));
        let rx = spawn_request(
            self.ui.dirs.control_socket(),
            Request::DriveGallery { action },
        );
        let browser = self.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            if browser.generation.get() != generation {
                if unlock && matches!(result, Ok(Ok(Response::Ok { .. }))) {
                    browser.lock();
                }
                return;
            }
            match result {
                Ok(Ok(Response::Ok { .. })) => {
                    if unlock || lock {
                        browser.unlocked.set(unlock);
                    }
                    browser.chooser.borrow_mut().take();
                    browser.load(0, 0.0);
                }
                Ok(Ok(Response::Error { message, .. })) => browser.status.set_label(&message),
                _ => browser
                    .status
                    .set_label(&gettext("Couldn't reach Proton Drive.")),
            }
        });
    }

    fn lock(&self) {
        let _ = spawn_request(
            self.ui.dirs.control_socket(),
            Request::DriveGallery {
                action: GalleryAction::Lock,
            },
        );
    }

    fn prompt_unlock(self: &Rc<Self>) {
        let Some(parent) = self.window.upgrade() else {
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Unlock Hidden Albums"))
            .body(gettext("Enter your Gallery passphrase. On first use, choose one with at least 12 characters."))
            .build();
        let password = gtk4::PasswordEntry::builder().show_peek_icon(true).build();
        dialog.set_extra_child(Some(&password));
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("unlock", &gettext("Unlock"));
        dialog.set_response_appearance("unlock", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("unlock"));
        let browser = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let passphrase = password.text().to_string();
            password.set_text("");
            if response == "unlock"
                && let Some(browser) = browser.upgrade()
            {
                browser.change(GalleryAction::Unlock { passphrase });
            }
        });
        dialog.present(Some(&parent));
    }

    fn back(self: &Rc<Self>) {
        let choosing = self.chooser.borrow().is_some();
        if choosing {
            let went_up = self
                .chooser
                .borrow_mut()
                .as_mut()
                .and_then(Vec::pop)
                .is_some();
            if went_up {
                self.browse_folders();
            } else {
                self.chooser.borrow_mut().take();
                self.load(0, 0.0);
            }
            return;
        }
        let at_target = self
            .cover
            .borrow()
            .as_ref()
            .zip(self.current())
            .is_some_and(|(target, current)| target.album == current.place.album);
        if at_target {
            self.cover.borrow_mut().take();
            self.status.set_label("");
            return;
        }
        let previous = self.trail.borrow_mut().pop();
        if let Some(previous) = previous {
            self.load(0, previous.parent_offset);
        } else if let Some(window) = self.window.upgrade() {
            window.close();
        }
    }

    fn rebuild(&self) {
        if self.chooser.borrow().is_some() {
            return;
        }
        let width = (self.width.get() - 24).max(240);
        let columns = ((width + 8) / 216).max(1);
        let card = (width - 8 * (columns - 1)) / columns;
        let mut rows = Vec::new();
        for albums in self.albums.borrow().chunks(columns as usize) {
            rows.push(BoxedAnyObject::new(Row::Albums(albums.to_vec(), card)));
        }
        let items = self.items.borrow();
        let period = |item: &PhotoItem| {
            glib::DateTime::from_unix_local(item.capture_time)
                .ok()
                .and_then(|date| date.format("%B %Y").ok())
                .map(|value| value.to_string())
                .unwrap_or_else(|| gettext("Unknown date"))
        };
        for section in items.chunk_by(|a, b| period(a) == period(b)) {
            rows.push(BoxedAnyObject::new(Row::Heading(period(&section[0]))));
            let ratios: Vec<_> = section
                .iter()
                .map(|item| item.ratio.unwrap_or(1.0))
                .collect();
            let mut items = section.iter();
            for sizes in super::photos::plan_justified(&ratios, width, 180) {
                rows.push(BoxedAnyObject::new(Row::Media(
                    items.by_ref().cloned().zip(sizes).collect(),
                )));
            }
        }
        self.rows.splice(0, self.rows.n_items(), &rows);
    }

    fn preview(self: &Rc<Self>, item: PhotoItem) {
        let video = item.kind == PhotoKind::Video;
        let request = if video {
            Request::OpenGalleryVideo { uid: item.uid }
        } else {
            Request::OpenGalleryImage { uid: item.uid }
        };
        let reply = spawn_request(self.ui.dirs.control_socket(), request);
        let browser = self.clone();
        let generation = self.generation.get();
        glib::spawn_future_local(async move {
            let result = reply.recv().await;
            let close = |token| {
                let _ = spawn_request(
                    browser.ui.dirs.control_socket(),
                    Request::CloseGalleryVideo { token },
                );
            };
            let parent = browser
                .window
                .upgrade()
                .filter(|_| generation == browser.generation.get());
            let Some(parent) = parent else {
                if let Ok(Ok(Response::GalleryVideo { token, .. })) = result {
                    close(token);
                }
                return;
            };
            let preview = gtk4::Window::builder()
                .transient_for(&parent)
                .destroy_with_parent(true)
                .title(item.name.unwrap_or_else(|| gettext("Preview")))
                .default_width(850)
                .default_height(600)
                .build();
            preview.add_css_class("photo-viewer-window");
            match result {
                Ok(Ok(Response::GalleryVideo { url, token, .. })) => {
                    let player = gtk4::Video::builder().autoplay(true).build();
                    player.set_file(Some(&gio::File::for_uri(&url)));
                    preview.set_child(Some(&player));
                    let socket = browser.ui.dirs.control_socket();
                    preview.connect_close_request(move |_| {
                        player.set_media_stream(gtk4::MediaStream::NONE);
                        let _ = spawn_request(
                            socket.clone(),
                            Request::CloseGalleryVideo {
                                token: token.clone(),
                            },
                        );
                        glib::Propagation::Proceed
                    });
                }
                Ok(Ok(Response::FilePath { path })) => {
                    let picture = gtk4::Picture::for_filename(path);
                    preview.set_child(Some(&picture));
                }
                Ok(Ok(Response::Error { message, .. })) => {
                    browser.status.set_label(&message);
                    return;
                }
                _ => {
                    browser
                        .status
                        .set_label(&gettext("Couldn't open the file."));
                    return;
                }
            }
            preview.present();
        });
    }
}

pub(crate) fn open(ui: &Rc<Ui>) {
    if let Some(browser) = ui.drive_galleries.borrow().upgrade()
        && let Some(window) = browser.window.upgrade()
    {
        window.present();
        return;
    }
    let Some(parent) = ui_window(ui) else { return };
    let window = gtk4::Window::builder()
        .transient_for(&parent)
        .default_width(1020)
        .default_height(760)
        .build();
    let title = adw::WindowTitle::new(&gettext("Drive Galleries"), "");
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    let back = gtk4::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some(&gettext("Back")));
    header.pack_start(&back);
    let add = gtk4::Button::with_label(&gettext("Add Gallery"));
    let choose = gtk4::Button::with_label(&gettext("Add This Folder"));
    choose.set_visible(false);
    header.pack_end(&add);
    header.pack_end(&choose);
    let more = gtk4::Button::from_icon_name("view-more-symbolic");
    more.set_tooltip_text(Some(&gettext("Gallery Options")));
    header.pack_end(&more);
    let rows = gio::ListStore::new::<BoxedAnyObject>();
    let factory = gtk4::SignalListItemFactory::new();
    let list = gtk4::ListView::builder()
        .model(&gtk4::NoSelection::new(Some(rows.clone())))
        .factory(&factory)
        .build();
    list.add_css_class("drive-gallery-rows");
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::External)
        .child(&list)
        .build();
    let status = gtk4::Label::new(None);
    status.set_wrap(true);
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&status);
    content.append(&scroll);
    window.set_child(Some(&content));
    let browser = Rc::new(Browser {
        ui: ui.clone(),
        window: window.downgrade(),
        rows,
        scroll: scroll.clone(),
        title,
        status,
        choose: choose.clone(),
        trail: RefCell::new(Vec::new()),
        chooser: RefCell::new(None),
        cover: RefCell::new(None),
        albums: RefCell::new(Vec::new()),
        items: RefCell::new(Vec::new()),
        width: Cell::new(0),
        generation: Cell::new(0),
        total: Cell::new(0),
        loading: Cell::new(false),
        unlocked: Cell::new(false),
        thumbnail_slots: tokio::sync::Semaphore::new(8),
    });
    *ui.drive_galleries.borrow_mut() = Rc::downgrade(&browser);
    window.connect_close_request({
        let browser = browser.clone();
        move |_| {
            *browser.ui.drive_galleries.borrow_mut() = std::rc::Weak::new();
            browser.next_request();
            if browser.unlocked.get() {
                browser.lock();
            }
            glib::Propagation::Proceed
        }
    });
    let weak = Rc::downgrade(&browser);
    factory.connect_bind(move |_, item| {
        let Some(browser) = weak.upgrade() else {
            return;
        };
        let Some(list_item) = item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let Some(data) = list_item.item().and_downcast::<BoxedAnyObject>() else {
            return;
        };
        let row = data.borrow::<Row>();
        let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        line.set_margin_start(12);
        line.set_margin_end(12);
        line.set_margin_bottom(8);
        match &*row {
            Row::Heading(label) => {
                let label = gtk4::Label::new(Some(label));
                label.add_css_class("heading");
                line.append(&label);
            }
            Row::Folder(folder) => {
                let button = gtk4::Button::with_label(&folder.name);
                button.set_hexpand(true);
                button.connect_clicked({
                    let weak = Rc::downgrade(&browser);
                    let folder = folder.clone();
                    move |_| {
                        if let Some(browser) = weak.upgrade() {
                            if let Some(trail) = browser.chooser.borrow_mut().as_mut() {
                                trail.push(folder.clone());
                            }
                            browser.browse_folders();
                        }
                    }
                });
                line.append(&button);
            }
            Row::Albums(albums, width) => {
                for album in albums {
                    let items = ngettext_f("{n} item", "{n} items", album.items, &[]);
                    let mut detail = if album.folders > 0 {
                        let folders = ngettext_f("{n} folder", "{n} folders", album.folders, &[]);
                        format!("{items} · {folders}")
                    } else {
                        items
                    };
                    if album.hidden {
                        detail = format!("{detail} · {}", gettext("Hidden"));
                    }
                    let button = tile(
                        &browser,
                        &album.folder.name,
                        album.cover.as_deref(),
                        *width,
                        130,
                        Some(&detail),
                        false,
                    );
                    button.connect_clicked({
                        let weak = Rc::downgrade(&browser);
                        let album = album.clone();
                        move |_| {
                            if let Some(browser) = weak.upgrade() {
                                browser.trail.borrow_mut().push(Place {
                                    album: album.clone(),
                                    parent_offset: browser.scroll.vadjustment().value(),
                                });
                                browser.load(0, 0.0);
                            }
                        }
                    });
                    line.append(&button);
                }
            }
            Row::Media(items) => {
                line.set_spacing(super::photos::TILE_GAP);
                for (item, (width, height)) in items {
                    let button = tile(
                        &browser,
                        item.name.as_deref().unwrap_or(""),
                        Some(&item.uid),
                        *width,
                        *height,
                        None,
                        item.kind == PhotoKind::Video,
                    );
                    button.connect_clicked({
                        let weak = Rc::downgrade(&browser);
                        let item = item.clone();
                        move |_| {
                            if let Some(browser) = weak.upgrade() {
                                let cover = browser.cover.borrow_mut().take();
                                if let Some(album) = cover {
                                    browser.change(GalleryAction::Cover {
                                        album,
                                        uid: item.uid.clone(),
                                    });
                                } else {
                                    browser.preview(item.clone());
                                }
                            }
                        }
                    });
                    line.append(&button);
                }
            }
        }
        list_item.set_child(Some(&line));
    });
    factory.connect_unbind(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk4::ListItem>() {
            item.set_child(gtk4::Widget::NONE);
        }
    });
    back.connect_clicked({
        let weak = Rc::downgrade(&browser);
        move |_| {
            if let Some(browser) = weak.upgrade() {
                browser.back();
            }
        }
    });
    add.connect_clicked({
        let weak = Rc::downgrade(&browser);
        let choose = choose.clone();
        move |_| {
            if let Some(browser) = weak.upgrade() {
                choose.set_visible(true);
                *browser.chooser.borrow_mut() = Some(Vec::new());
                browser.browse_folders();
            }
        }
    });
    choose.connect_clicked({
        let weak = Rc::downgrade(&browser);
        move |button| {
            if let Some(browser) = weak.upgrade() {
                let uid = browser
                    .chooser
                    .borrow()
                    .as_ref()
                    .and_then(|trail| trail.last())
                    .map(|f| f.uid.clone());
                if let Some(uid) = uid {
                    button.set_visible(false);
                    browser.change(GalleryAction::Add { uid });
                }
            }
        }
    });
    more.connect_clicked({
        let weak = Rc::downgrade(&browser);
        move |button| {
            if let Some(browser) = weak.upgrade() {
                let mut menu = ActionMenu::new();
                menu.item(&gettext("Refresh"), {
                    let browser = browser.clone();
                    move || browser.change(GalleryAction::Refresh)
                });
                if browser.unlocked.get() {
                    menu.item(&gettext("Lock Hidden Albums"), {
                        let browser = browser.clone();
                        move || browser.change(GalleryAction::Lock)
                    });
                } else {
                    menu.item(&gettext("Unlock Hidden Albums"), {
                        let browser = browser.clone();
                        move || browser.prompt_unlock()
                    });
                }
                if let Some(album) = browser.current() {
                    menu.item(&gettext("Choose Cover"), {
                        let browser = browser.clone();
                        let place = album.place.clone();
                        move || {
                            *browser.cover.borrow_mut() = Some(place.clone());
                            browser.status.set_label(&gettext(
                                "Choose a photo or video from this album or a subfolder.",
                            ));
                        }
                    });
                    menu.item(&gettext("Public Link"), {
                        let browser = browser.clone();
                        let folder = album.folder.clone();
                        move || {
                            open_share_dialog_over(
                                &browser.ui,
                                &DirEntry {
                                    uid: folder.uid.clone(),
                                    name: folder.name.clone(),
                                    is_dir: true,
                                    ..DirEntry::default()
                                },
                                browser.window.upgrade(),
                            )
                        }
                    });
                    if browser.trail.borrow().len() == 1 {
                        menu.item(&gettext("Remove Gallery"), {
                            let browser = browser.clone();
                            move || {
                                browser.trail.borrow_mut().clear();
                                browser.change(GalleryAction::Remove {
                                    gallery: album.place.gallery.clone(),
                                });
                            }
                        });
                    }
                }
                menu.popup_at(
                    button,
                    f64::from(button.width()) / 2.0,
                    f64::from(button.height()),
                );
            }
        }
    });
    scroll.vadjustment().connect_value_changed({
        let weak = Rc::downgrade(&browser);
        move |adj| {
            if let Some(browser) = weak.upgrade() {
                let offset = browser.items.borrow().len();
                if browser.chooser.borrow().is_none()
                    && !browser.loading.get()
                    && offset < browser.total.get()
                    && adj.value() + adj.page_size() + 800.0 >= adj.upper()
                {
                    browser.load(offset, 0.0);
                }
            }
        }
    });
    scroll.add_tick_callback({
        let weak = Rc::downgrade(&browser);
        move |scroll, _| {
            let Some(browser) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let width = scroll.width();
            if width > 0 && browser.width.replace(width) != width {
                browser.rebuild();
            }
            glib::ControlFlow::Continue
        }
    });
    window.present();
    browser.load(0, 0.0);
}

fn tile(
    browser: &Rc<Browser>,
    title: &str,
    uid: Option<&str>,
    width: i32,
    height: i32,
    detail: Option<&str>,
    video: bool,
) -> gtk4::Button {
    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .width_request(if detail.is_some() {
            (width - 24).max(1)
        } else {
            width
        })
        .height_request(height)
        .build();
    let label = gtk4::Label::new(Some(title));
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_max_width_chars((width / 9).max(1));
    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let cover = gtk4::Overlay::new();
    cover.set_child(Some(&picture));
    let icon = gtk4::Image::from_icon_name(if detail.is_some() {
        "folder-symbolic"
    } else {
        "image-x-generic-symbolic"
    });
    icon.set_halign(gtk4::Align::Center);
    icon.set_valign(gtk4::Align::Center);
    icon.set_pixel_size(32);
    icon.add_css_class("dim-label");
    cover.add_overlay(&icon);
    cover.set_overflow(gtk4::Overflow::Hidden);
    let button = gtk4::Button::builder().tooltip_text(title).build();
    if let Some(detail) = detail {
        box_.append(&cover);
        box_.append(&label);
        let caption = gtk4::Label::new(Some(detail));
        caption.add_css_class("dim-label");
        caption.add_css_class("caption");
        caption.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        box_.append(&caption);
        button.set_width_request(width);
        button.set_child(Some(&box_));
    } else {
        button.add_css_class("photo-tile");
        cover.add_css_class("photo-placeholder");
        label.add_css_class("photo-caption");
        label.set_valign(gtk4::Align::End);
        cover.add_overlay(&label);
        if video {
            let play = gtk4::Image::from_icon_name("media-playback-start-symbolic");
            play.add_css_class("photo-video-badge");
            play.add_css_class("photo-video-badge-small");
            play.set_halign(gtk4::Align::End);
            play.set_valign(gtk4::Align::Start);
            play.set_margin_end(8);
            play.set_margin_top(8);
            cover.add_overlay(&play);
        }
        button.set_child(Some(&cover));
    }
    if let Some(uid) = uid {
        let browser = browser.clone();
        let generation = browser.generation.get();
        let uid = uid.to_owned();
        let picture = picture.downgrade();
        let icon = icon.downgrade();
        glib::spawn_future_local(async move {
            let Ok(_permit) = browser.thumbnail_slots.acquire().await else {
                return;
            };
            if picture.upgrade().is_none() || generation != browser.generation.get() {
                return;
            }
            let request = spawn_request(
                browser.ui.dirs.control_socket(),
                Request::GalleryThumbnail { uid },
            );
            let response = request.recv().await;
            if generation == browser.generation.get()
                && let (Some(picture), Ok(Ok(Response::GalleryThumbnail { data: Some(data) }))) =
                    (picture.upgrade(), response)
                && let Ok(bytes) = STANDARD.decode(data)
                && let Ok(texture) = gtk4::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
            {
                picture.set_paintable(Some(&texture));
                if let Some(icon) = icon.upgrade() {
                    icon.set_visible(false);
                }
            }
        });
    }
    button
}
