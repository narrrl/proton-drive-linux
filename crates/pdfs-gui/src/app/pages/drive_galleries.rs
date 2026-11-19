use crate::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use pdfs_core::control::{GalleryAction, GalleryAlbum, GalleryFolder, GalleryPlace};
use std::collections::VecDeque;

const PAGE: usize = 150;
const THUMB_CACHE: usize = 256;
type ThumbnailKey = (u64, bool, String);
type ThumbnailTarget = (glib::WeakRef<gtk4::Picture>, glib::WeakRef<gtk4::Image>);

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
    root: gtk4::FlowBox,
    root_status: gtk4::Label,
    root_loading: Cell<bool>,
    root_loaded: Cell<bool>,
    root_generation: Cell<u64>,
    root_reload: Cell<bool>,
    preview: glib::WeakRef<adw::Dialog>,
    unlock_pending: Cell<bool>,
    rows: gio::ListStore,
    scroll: gtk4::ScrolledWindow,
    title: adw::WindowTitle,
    status: gtk4::Label,
    choose: gtk4::Button,
    unlock: gtk4::Button,
    more: gtk4::Button,
    trail: RefCell<Vec<Place>>,
    chooser: RefCell<Option<Vec<(GalleryFolder, f64)>>>,
    cover: RefCell<Option<GalleryPlace>>,
    albums: RefCell<Vec<GalleryAlbum>>,
    items: RefCell<Vec<PhotoItem>>,
    width: Cell<i32>,
    generation: Cell<u64>,
    total: Cell<usize>,
    loading: Cell<bool>,
    unlocked: Cell<bool>,
    thumbnail_slots: tokio::sync::Semaphore,
    thumbnails: RefCell<VecDeque<(bool, String, gtk4::gdk::Texture)>>,
    thumb_pending: RefCell<HashMap<ThumbnailKey, Vec<ThumbnailTarget>>>,
}

impl Browser {
    pub(crate) fn reset(&self) {
        self.close();
        self.root_generation
            .set(self.root_generation.get().wrapping_add(1));
        self.root_loading.set(false);
        self.root_loaded.set(false);
        self.root_reload.set(false);
        self.root.remove_all();
        self.root_status.set_label("");
        self.next_request();
        self.thumbnails.borrow_mut().clear();
        self.thumb_pending.borrow_mut().clear();
    }

    pub(crate) fn close(&self) {
        if !self.active() {
            return;
        }
        self.next_request();
        self.loading.set(false);
        self.total.set(0);
        if self.unlocked.replace(false) || self.unlock_pending.get() {
            self.lock();
        }
        if let Some(preview) = self.preview.upgrade() {
            preview.force_close();
        }
        self.thumbnails
            .borrow_mut()
            .retain(|(private, _, _)| !private);
        self.thumb_pending.borrow_mut().clear();
        self.trail.borrow_mut().clear();
        self.chooser.borrow_mut().take();
        self.cover.borrow_mut().take();
        self.albums.borrow_mut().clear();
        self.items.borrow_mut().clear();
        self.rows.remove_all();
        self.ui.gallery.title.set_subtitle("");
        self.ui
            .gallery
            .content
            .set_visible_child_full("albums", gtk4::StackTransitionType::None);
        sync_header(&self.ui);
    }

    fn active(&self) -> bool {
        self.ui.gallery.content.visible_child_name().as_deref() == Some("drive-galleries")
    }

    fn enter(&self) {
        self.title.set_subtitle("");
        self.ui.gallery.filters.set_visible(false);
        self.ui
            .gallery
            .content
            .set_visible_child_full("drive-galleries", gtk4::StackTransitionType::None);
        sync_header(&self.ui);
    }

    pub(crate) fn refresh_visibility(self: &Rc<Self>) {
        self.thumbnails.borrow_mut().clear();
        self.root.remove_all();
        self.load_root();
        if !self.active() || self.chooser.borrow().is_some() || self.unlock_pending.get() {
            return;
        }
        if let Some(preview) = self.preview.upgrade() {
            preview.force_close();
        }
        if self.unlocked.get() {
            self.close();
        } else {
            self.load(0, self.scroll.vadjustment().value());
        }
    }

    pub(crate) fn load_root(self: &Rc<Self>) {
        if self.root_loading.replace(true) {
            self.root_reload.set(true);
            return;
        }
        let generation = self.root_generation.get();
        if !self.root_loaded.get() {
            self.root_status.set_label(&gettext("Loading galleries…"));
        }
        let rx = spawn_request(
            self.ui.dirs.control_socket(),
            Request::DriveGallery {
                action: GalleryAction::List {
                    album: None,
                    offset: 0,
                    limit: PAGE,
                },
            },
        );
        let browser = self.clone();
        glib::spawn_future_local(async move {
            let response = rx.recv().await;
            if generation != browser.root_generation.get() {
                return;
            }
            browser.root_loading.set(false);
            if browser.root_reload.replace(false) {
                browser.load_root();
            }
            let Ok(Ok(Response::GalleryPage { albums, .. })) = response else {
                browser.root_status.set_label(&match response {
                    Ok(Ok(Response::Error { message, .. })) => message,
                    _ => gettext("Couldn’t load Drive galleries."),
                });
                return;
            };
            browser.root_loaded.set(true);
            browser.root_status.set_label(&if albums.is_empty() {
                gettext("No Drive galleries yet.")
            } else {
                String::new()
            });
            browser.root.remove_all();
            for album in albums {
                let card = browser.album_tile(&album, 185, false);
                let weak = Rc::downgrade(&browser);
                card.connect_clicked(move |_| {
                    if let Some(browser) = weak.upgrade() {
                        browser.enter();
                        browser.trail.borrow_mut().push(Place {
                            album: album.clone(),
                            parent_offset: 0.0,
                        });
                        browser.load(0, 0.0);
                    }
                });
                browser.root.append(&card);
            }
        });
    }

    fn album_tile(
        self: &Rc<Self>,
        album: &GalleryAlbum,
        width: i32,
        private: bool,
    ) -> gtk4::Button {
        let items = ngettext_f("{n} item", "{n} items", album.items, &[]);
        let count = if album.folders > 0 {
            let folders = ngettext_f("{n} folder", "{n} folders", album.folders, &[]);
            format!("{items} · {folders}")
        } else {
            items
        };
        let (button, picture) = super::albums::collection_card(
            &album.folder.name,
            &count,
            if album.proton {
                "pdfs-album-symbolic"
            } else {
                "folder-symbolic"
            },
            (width, width.min(200)),
        );
        button.set_halign(gtk4::Align::Start);
        wire_thumbnail(self, &picture, album.cover.as_deref(), private, None);
        button
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
        self.load_page(offset, restore, false);
    }

    fn load_page(self: &Rc<Self>, offset: usize, restore: f64, keep: bool) {
        self.choose.set_visible(false);
        self.unlock.set_visible(false);
        self.more.set_visible(true);
        self.loading.set(true);
        let generation = if offset == 0 {
            if !keep {
                self.total.set(0);
                self.albums.borrow_mut().clear();
                self.items.borrow_mut().clear();
                self.rows.remove_all();
            }
            self.next_request()
        } else {
            self.generation.get()
        };
        let album = self.current();
        let private = self.unlocked.get();
        self.title.set_title(&album.as_ref().map_or_else(
            || {
                if private {
                    gettext("Hidden albums")
                } else {
                    gettext("Drive galleries")
                }
            },
            |a| a.folder.name.clone(),
        ));
        self.status.set_label(&gettext("Loading…"));
        let proton = album.as_ref().is_some_and(|a| a.proton);
        let place = album.map(|a| a.place);
        let action = if private {
            GalleryAction::PrivateList {
                album: place,
                proton,
                offset,
                limit: PAGE,
            }
        } else {
            GalleryAction::List {
                album: place,
                offset,
                limit: PAGE,
            }
        };
        let rx = spawn_request(
            self.ui.dirs.control_socket(),
            Request::DriveGallery { action },
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
                        browser.items.borrow_mut().clear();
                    }
                    browser.items.borrow_mut().extend(items);
                    browser.total.set(total);
                    let empty = total == 0 && browser.albums.borrow().is_empty();
                    let message = if browser.cover.borrow().is_some() {
                        gettext("Choose a photo or video as the album cover.")
                    } else if empty {
                        if private && browser.current().is_none() {
                            gettext("No hidden albums.")
                        } else {
                            gettext("This gallery is empty.")
                        }
                    } else {
                        String::new()
                    };
                    browser.status.set_label(&message);
                    browser.rebuild();
                    browser.restore_offset(generation, restore);
                }
                Ok(Ok(Response::Error { message, .. })) => browser.status.set_label(&message),
                _ => browser
                    .status
                    .set_label(&gettext("Couldn't reach Proton Drive.")),
            }
        });
    }

    fn browse_folders(self: &Rc<Self>, restore: f64) {
        let Some(trail) = self.chooser.borrow().clone() else {
            return;
        };
        let generation = self.next_request();
        let parent = trail.last().map(|(folder, _)| folder.uid.clone());
        self.more.set_visible(false);
        self.unlock.set_visible(false);
        self.choose.set_visible(parent.is_some());
        self.title.set_title(&trail.last().map_or_else(
            || gettext("Choose a Drive folder"),
            |(folder, _)| folder.name.clone(),
        ));
        self.status.set_label(&gettext("Loading folders…"));
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
                    let rows: Vec<_> = folders
                        .into_iter()
                        .map(|folder| BoxedAnyObject::new(Row::Folder(folder)))
                        .collect();
                    browser.rows.splice(0, browser.rows.n_items(), &rows);
                    browser.restore_offset(generation, restore);
                }
                Ok(Ok(Response::Error { message, .. })) => browser.status.set_label(&message),
                _ => browser
                    .status
                    .set_label(&gettext("Couldn't reach Proton Drive.")),
            }
        });
    }

    fn restore_offset(self: &Rc<Self>, generation: u64, restore: f64) {
        let browser = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(browser) = browser.upgrade()
                && generation == browser.generation.get()
            {
                browser.scroll.vadjustment().set_value(restore);
            }
        });
    }

    fn change(self: &Rc<Self>, action: GalleryAction) {
        let unlock = matches!(action, GalleryAction::Unlock { .. });
        let update_root = matches!(
            action,
            GalleryAction::Add { .. }
                | GalleryAction::Remove { .. }
                | GalleryAction::Refresh
                | GalleryAction::Cover { .. }
        );
        if unlock && self.unlock_pending.replace(true) {
            return;
        }
        self.unlock.set_visible(false);
        let generation = self.next_request();
        self.status.set_label(&gettext("Updating gallery…"));
        let rx = spawn_request(
            self.ui.dirs.control_socket(),
            Request::DriveGallery { action },
        );
        let browser = self.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            if unlock {
                browser.unlock_pending.set(false);
                browser.unlock.set_visible(
                    browser.active()
                        && browser.generation.get() == generation
                        && !matches!(result, Ok(Ok(Response::Ok { .. }))),
                );
            }
            if browser.generation.get() != generation {
                if unlock && matches!(result, Ok(Ok(Response::Ok { .. }))) {
                    browser.lock();
                }
                return;
            }
            match result {
                Ok(Ok(Response::Ok { .. })) => {
                    if unlock {
                        browser.unlocked.set(true);
                    }
                    let added = browser.chooser.borrow().is_some();
                    if update_root {
                        browser.load_root();
                    }
                    if added || (!browser.unlocked.get() && browser.current().is_none()) {
                        browser.close();
                        return;
                    }
                    browser.chooser.borrow_mut().take();
                    let restore = if update_root {
                        browser.scroll.vadjustment().value()
                    } else {
                        0.0
                    };
                    browser.load_page(0, restore, update_root);
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
        if self.unlock_pending.get() {
            return;
        }
        let Some(parent) = ui_window(&self.ui) else {
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
                browser.enter();
                browser.more.set_visible(false);
                browser.title.set_title(&gettext("Hidden albums"));
                browser.change(GalleryAction::Unlock { passphrase });
            }
        });
        dialog.present(Some(&parent));
    }

    pub(crate) fn back(self: &Rc<Self>) {
        let choosing = self.chooser.borrow().is_some();
        if choosing {
            let previous = self.chooser.borrow_mut().as_mut().and_then(Vec::pop);
            if let Some((_, offset)) = previous {
                self.browse_folders(offset);
            } else {
                self.chooser.borrow_mut().take();
                self.close();
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
        if let Some(previous) = previous
            && (self.unlocked.get() || self.current().is_some())
        {
            self.load(0, previous.parent_offset);
        } else {
            self.close();
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
        let private = self.unlocked.get();
        let request = if video {
            Request::OpenGalleryVideo {
                uid: item.uid,
                private,
            }
        } else {
            Request::OpenGalleryImage {
                uid: item.uid,
                private,
            }
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
            let parent = ui_window(&browser.ui)
                .filter(|_| browser.active() && generation == browser.generation.get());
            let Some(parent) = parent else {
                if let Ok(Ok(Response::GalleryVideo { token, .. })) = result {
                    close(token);
                }
                return;
            };
            let preview = adw::Dialog::builder()
                .title(item.name.unwrap_or_else(|| gettext("Preview")))
                .content_width(850)
                .content_height(600)
                .build();
            let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            content.append(&adw::HeaderBar::new());
            preview.set_child(Some(&content));
            match result {
                Ok(Ok(Response::GalleryVideo { url, token, .. })) => {
                    let player = gtk4::Video::builder().autoplay(true).build();
                    player.set_file(Some(&gio::File::for_uri(&url)));
                    player.set_vexpand(true);
                    content.append(&player);
                    let socket = browser.ui.dirs.control_socket();
                    preview.connect_closed(move |_| {
                        player.set_media_stream(gtk4::MediaStream::NONE);
                        let _ = spawn_request(
                            socket.clone(),
                            Request::CloseGalleryVideo {
                                token: token.clone(),
                            },
                        );
                    });
                }
                Ok(Ok(Response::FilePath { path })) => {
                    let picture = gtk4::Picture::for_filename(path);
                    picture.set_vexpand(true);
                    content.append(&picture);
                }
                Ok(Ok(Response::GalleryThumbnail { data: Some(data) })) if private => {
                    let Some(texture) = decode_thumbnail(&data) else {
                        browser
                            .status
                            .set_label(&gettext("Couldn't open the photo."));
                        return;
                    };
                    let picture = gtk4::Picture::for_paintable(&texture);
                    picture.set_vexpand(true);
                    content.append(&picture);
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
            browser.preview.set(Some(&preview));
            preview.present(Some(&parent));
        });
    }
}

pub(crate) fn wire(
    ui: &Rc<Ui>,
    window: &adw::ApplicationWindow,
    root: &gtk4::FlowBox,
    root_status: &gtk4::Label,
    add: &gtk4::Button,
    hidden: &gtk4::Button,
) {
    let title = ui.gallery.title.clone();
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.set_halign(gtk4::Align::End);
    let choose = gtk4::Button::with_label(&gettext("Add This Folder"));
    choose.set_visible(false);
    header.append(&choose);
    let more = gtk4::Button::from_icon_name("view-more-symbolic");
    more.set_tooltip_text(Some(&gettext("Gallery Options")));
    let unlock = gtk4::Button::with_label(&gettext("Unlock"));
    unlock.set_visible(false);
    header.append(&unlock);
    header.append(&more);
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
    ui.gallery
        .content
        .add_named(&content, Some("drive-galleries"));
    for label in [&status, root_status] {
        label.set_visible(false);
        label.connect_label_notify(|label| label.set_visible(!label.label().is_empty()));
    }
    let browser = Rc::new(Browser {
        ui: ui.clone(),
        root: root.clone(),
        root_status: root_status.clone(),
        root_loaded: Cell::new(false),
        root_generation: Cell::new(0),
        root_reload: Cell::new(false),
        root_loading: Cell::new(false),
        preview: glib::WeakRef::new(),
        unlock_pending: Cell::new(false),
        rows,
        scroll: scroll.clone(),
        title,
        status,
        choose: choose.clone(),
        unlock: unlock.clone(),
        more: more.clone(),
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
        thumbnails: RefCell::default(),
        thumb_pending: RefCell::default(),
    });
    *ui.drive_galleries.borrow_mut() = Rc::downgrade(&browser);
    let weak = Rc::downgrade(&browser);
    window.connect_is_active_notify(move |window| {
        if !window.is_active()
            && let Some(browser) = weak.upgrade()
            && (browser.unlocked.get() || browser.unlock_pending.get())
        {
            browser.close();
        }
    });
    let weak = Rc::downgrade(&browser);
    window.connect_close_request(move |_| {
        if let Some(browser) = weak.upgrade() {
            browser.close();
        }
        glib::Propagation::Proceed
    });
    unlock.connect_clicked({
        let weak = Rc::downgrade(&browser);
        move |_| {
            if let Some(browser) = weak.upgrade() {
                browser.prompt_unlock();
            }
        }
    });
    hidden.connect_clicked({
        let browser = browser.clone();
        move |_| browser.prompt_unlock()
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
                                trail.push((folder.clone(), browser.scroll.vadjustment().value()));
                            }
                            browser.browse_folders(0.0);
                        }
                    }
                });
                line.append(&button);
            }
            Row::Albums(albums, width) => {
                for album in albums {
                    let button = browser.album_tile(album, *width, browser.unlocked.get());
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
                        (*width, *height),
                        item.kind == PhotoKind::Video,
                        browser.unlocked.get(),
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
    add.connect_clicked({
        let browser = browser.clone();
        move |_| {
            browser.enter();
            *browser.chooser.borrow_mut() = Some(Vec::new());
            browser.browse_folders(0.0);
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
                    .map(|(folder, _)| folder.uid.clone());
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
                        move || browser.close()
                    });
                } else {
                    menu.item(&gettext("Refresh galleries"), {
                        let browser = browser.clone();
                        move || browser.change(GalleryAction::Refresh)
                    });
                }
                if let Some(album) = browser.current().filter(|album| !album.proton) {
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
                    if !browser.unlocked.get() {
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
                                    ui_window(&browser.ui),
                                )
                            }
                        });
                        if album.place.gallery == album.place.album {
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
                } else if !browser.unlocked.get() && browser.chooser.borrow().is_none() {
                    menu.item(&gettext("Unlock Hidden Albums"), {
                        let browser = browser.clone();
                        move || browser.prompt_unlock()
                    });
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
}

fn tile(
    browser: &Rc<Browser>,
    title: &str,
    uid: Option<&str>,
    (width, height): (i32, i32),
    video: bool,
    private: bool,
) -> gtk4::Button {
    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .build();
    let label = gtk4::Label::new(Some(title));
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_max_width_chars((width / 9).max(1));
    label.add_css_class("photo-caption");
    label.set_valign(gtk4::Align::End);
    let icon = gtk4::Image::builder()
        .icon_name(if video {
            "video-x-generic-symbolic"
        } else {
            "image-x-generic-symbolic"
        })
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .pixel_size(32)
        .build();
    icon.add_css_class("dim-label");
    let cover = gtk4::Overlay::new();
    cover.set_child(Some(&icon));
    cover.set_size_request(width, height);
    cover.add_overlay(&picture);
    cover.add_overlay(&label);
    cover.add_css_class("photo-placeholder");
    cover.set_overflow(gtk4::Overflow::Hidden);
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
    let button = gtk4::Button::builder()
        .tooltip_text(title)
        .child(&cover)
        .build();
    button.add_css_class("photo-tile");
    wire_thumbnail(browser, &picture, uid, private, Some(&icon));
    button
}

fn wire_thumbnail(
    browser: &Rc<Browser>,
    picture: &gtk4::Picture,
    uid: Option<&str>,
    private: bool,
    icon: Option<&gtk4::Image>,
) {
    let Some(uid) = uid else {
        return;
    };
    let browser = Rc::downgrade(browser);
    let uid = uid.to_owned();
    let icon = icon.map_or_else(glib::WeakRef::new, ObjectExt::downgrade);
    picture.connect_map(move |picture| {
        if picture.paintable().is_none()
            && let Some(browser) = browser.upgrade()
        {
            browser.want_thumbnail(&uid, private, picture, &icon);
        }
    });
}

fn decode_thumbnail(data: &str) -> Option<gtk4::gdk::Texture> {
    let bytes = STANDARD.decode(data).ok()?;
    gtk4::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()
}

impl Browser {
    fn want_thumbnail(
        self: &Rc<Self>,
        uid: &str,
        private: bool,
        picture: &gtk4::Picture,
        icon: &glib::WeakRef<gtk4::Image>,
    ) {
        if private && !self.unlocked.get() {
            return;
        }
        {
            let mut cache = self.thumbnails.borrow_mut();
            if let Some(index) = cache
                .iter()
                .position(|(p, id, _)| *p == private && id == uid)
            {
                let cached = cache.remove(index).unwrap();
                picture.set_paintable(Some(&cached.2));
                if let Some(icon) = icon.upgrade() {
                    icon.set_visible(false);
                }
                cache.push_back(cached);
                return;
            }
        }
        let key = (self.generation.get(), private, uid.to_owned());
        {
            let mut pending = self.thumb_pending.borrow_mut();
            let started = pending.contains_key(&key);
            let targets = pending.entry(key.clone()).or_default();
            targets.retain(|(target, _)| target.upgrade().is_some_and(|seen| seen != *picture));
            targets.push((picture.downgrade(), icon.clone()));
            if started {
                return;
            }
        }
        let browser = self.clone();
        glib::spawn_future_local(async move {
            let Ok(_permit) = browser.thumbnail_slots.acquire().await else {
                return;
            };
            let visible = browser
                .thumb_pending
                .borrow()
                .get(&key)
                .is_some_and(|targets| {
                    targets
                        .iter()
                        .any(|(picture, _)| picture.upgrade().is_some_and(|p| p.is_mapped()))
                });
            if !visible || key.0 != browser.generation.get() {
                browser.thumb_pending.borrow_mut().remove(&key);
                return;
            }
            let request = spawn_request(
                browser.ui.dirs.control_socket(),
                Request::GalleryThumbnail {
                    uid: key.2.clone(),
                    private,
                },
            );
            let response = request.recv().await;
            let targets = browser
                .thumb_pending
                .borrow_mut()
                .remove(&key)
                .unwrap_or_default();
            if key.0 != browser.generation.get() || private && !browser.unlocked.get() {
                return;
            }
            if let Ok(Ok(Response::GalleryThumbnail { data: Some(data) })) = response
                && let Some(texture) = decode_thumbnail(&data)
            {
                for (picture, icon) in targets {
                    if let Some(picture) = picture.upgrade() {
                        picture.set_paintable(Some(&texture));
                    }
                    if let Some(icon) = icon.upgrade() {
                        icon.set_visible(false);
                    }
                }
                let mut cache = browser.thumbnails.borrow_mut();
                if cache.len() >= THUMB_CACHE {
                    cache.pop_front();
                }
                cache.push_back((private, key.2, texture));
            }
        });
    }
}
