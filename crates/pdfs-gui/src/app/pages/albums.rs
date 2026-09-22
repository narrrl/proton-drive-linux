//! The Albums view of the Photos page: a grid of album covers, and the album
//! that opens when one is clicked.
//!
//! An open album reuses the gallery wholesale — same square grid, same
//! on-demand thumbnails, same lightbox — by parking the album in
//! [`GalleryState::album`] and letting [`load_gallery`] page
//! [`Request::AlbumPhotos`] instead of the timeline. Only the header and the
//! filter bar differ, because an album is served whole rather than filtered.

use crate::*;

/// Edge length in px of an album cover in the grid. Bigger than a timeline tile:
/// there are far fewer albums than photos, and the cover is the only thing
/// identifying one at a glance.
const COVER_EDGE: i32 = 200;

/// Show the Albums grid and (re)load it. Called when the Albums toggle goes on.
pub(crate) fn show_albums(ui: &Rc<Ui>) {
    close_album(ui);
    ui.gallery.content.set_visible_child_name("albums");
    load_albums(ui);
}

/// Ask the daemon for the album listing and rebuild the grid.
pub(crate) fn load_albums(ui: &Rc<Ui>) {
    if ui.gallery.albums_loading.get() {
        return;
    }
    ui.gallery.albums_loading.set(true);
    albums_status(
        ui,
        "view-grid-symbolic",
        "Loading albums…",
        "Reading your Proton Drive albums.",
    );

    let rx = spawn_request(ui.dirs.control_socket(), Request::PhotoAlbums);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.gallery.albums_loading.set(false);
        match result {
            Ok(Ok(Response::Albums {
                available: false, ..
            })) => albums_status(
                &ui,
                "image-missing-symbolic",
                "No photo library",
                "This Proton account doesn't have Photos enabled.",
            ),
            Ok(Ok(Response::Albums { items, .. })) if items.is_empty() => albums_status(
                &ui,
                "view-grid-symbolic",
                "No albums",
                "Albums you create in Proton Photos appear here.",
            ),
            Ok(Ok(Response::Albums { items, .. })) => {
                fill_albums(&ui, &items);
                ui.gallery.albums_stack.set_visible_child_name("grid");
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't load albums", &message, kind);
                albums_status(
                    &ui,
                    "dialog-warning-symbolic",
                    "Couldn't load albums",
                    &message,
                );
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => albums_status(
                &ui,
                "network-offline-symbolic",
                "Not connected",
                "The Proton Drive mount service didn't respond.",
            ),
        }
    });
}

/// Replace the grid's cards with `albums`, in the order the daemon gave them
/// (newest activity first).
fn fill_albums(ui: &Rc<Ui>, albums: &[AlbumInfo]) {
    while let Some(child) = ui.gallery.albums.first_child() {
        ui.gallery.albums.remove(&child);
    }
    for album in albums {
        ui.gallery.albums.append(&album_card(ui, album));
    }
    // The covers are ordinary photos as far as the daemon is concerned, so the
    // batch that fills them is the gallery's own.
    schedule_thumbs(ui);
}

/// One album as a clickable card: its cover, its name, and how many photos it
/// holds. An album shared with us says so — it lives on someone else's volume,
/// and that is worth knowing before opening it.
fn album_card(ui: &Rc<Ui>, album: &AlbumInfo) -> gtk4::Button {
    let picture = gtk4::Picture::builder()
        // Fills and crops the cover square, like a timeline tile.
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    picture.add_css_class("photo-thumb");

    let placeholder = gtk4::Image::builder()
        .icon_name("view-grid-symbolic")
        .pixel_size(32)
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .build();
    placeholder.add_css_class("photo-placeholder");

    let cover = gtk4::Overlay::new();
    cover.set_child(Some(&placeholder));
    cover.add_overlay(&picture);
    cover.set_size_request(COVER_EDGE, COVER_EDGE);
    cover.set_overflow(gtk4::Overflow::Hidden);
    cover.add_css_class("album-cover");

    let name = gtk4::Label::builder()
        .label(&album.name)
        .halign(gtk4::Align::Start)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    name.add_css_class("heading");

    let count = gtk4::Label::builder()
        .label(album_subtitle(album))
        .halign(gtk4::Align::Start)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    count.add_css_class("dim-label");
    count.add_css_class("caption");

    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    card.append(&cover);
    card.append(&name);
    card.append(&count);

    let button = gtk4::Button::builder().child(&card).build();
    button.add_css_class("flat");
    button.add_css_class("album-card");
    button.set_tooltip_text(Some(&album.name));

    if let Some(uid) = album.cover_uid.clone() {
        want_cover(ui, uid, &picture);
    }

    let ui_open = ui.clone();
    let album = album.clone();
    button.connect_clicked(move |_| open_album(&ui_open, album.clone()));
    button
}

/// "12 photos", plus where the album came from when it isn't ours.
fn album_subtitle(album: &AlbumInfo) -> String {
    let photos = match album.photo_count {
        1 => "1 photo".to_string(),
        n => format!("{n} photos"),
    };
    if album.shared {
        format!("{photos} · shared with me")
    } else {
        photos
    }
}

/// Queue an album cover for the gallery's own thumbnail machinery: same batching,
/// same texture cache, same decode pacing as a timeline tile.
fn want_cover(ui: &Rc<Ui>, uid: String, picture: &gtk4::Picture) {
    if let Some(texture) = ui.gallery.photo_tex.borrow().get(&uid) {
        picture.set_paintable(Some(texture));
        return;
    }
    if ui.gallery.photo_nothumb.borrow().contains(&uid) {
        return;
    }
    ui.gallery
        .thumb_wanted
        .borrow_mut()
        .insert(uid.clone(), picture.clone());
    let mut queue = ui.gallery.thumb_queue.borrow_mut();
    if !queue.contains(&uid) {
        queue.push_back(uid);
    }
}

/// Open one album in the gallery: the timeline view, paged from the album
/// instead, with the filters that don't apply to it out of the way.
pub(crate) fn open_album(ui: &Rc<Ui>, album: AlbumInfo) {
    ui.gallery.title.set_title(&album.name);
    ui.gallery.title.set_subtitle(&album_subtitle(&album));
    *ui.gallery.album.borrow_mut() = Some(album);

    // An album page carries no kind or date filter, and Upload targets the
    // timeline rather than an album — hide those rather than offer controls that
    // would silently do something else. The Photos/Albums switcher goes too: an
    // album is a level below it, and back is the way out.
    ui.gallery.filters.set_visible(false);
    ui.gallery.view_switch.set_visible(false);
    ui.gallery.upload.set_visible(false);
    ui.gallery.back.set_visible(true);
    // A stale kind/date filter would otherwise be sent with the album request.
    ui.gallery.kind.set(None);
    ui.gallery.range.set(None);

    ui.gallery.content.set_visible_child_name("timeline");
    load_gallery(ui, false);
}

/// Leave an open album, restoring the timeline's header and filters. A no-op
/// when no album is open, so callers can use it as "make sure we're not in one".
pub(crate) fn close_album(ui: &Rc<Ui>) {
    if ui.gallery.album.borrow_mut().take().is_none() {
        return;
    }
    ui.gallery.title.set_title("Photos");
    ui.gallery.filters.set_visible(true);
    ui.gallery.view_switch.set_visible(true);
    ui.gallery.upload.set_visible(true);
    ui.gallery.back.set_visible(false);
}

/// Show the album grid's own status page (loading, empty, or an error).
fn albums_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str) {
    ui.gallery.albums_status.set_icon_name(Some(icon));
    ui.gallery.albums_status.set_title(title);
    ui.gallery.albums_status.set_description(Some(description));
    ui.gallery.albums_stack.set_visible_child_name("status");
}

/// Wire the Photos/Albums switcher and the back button. The switcher swaps the
/// whole content area; back leaves an open album for the grid it came from.
pub(crate) fn wire_albums(ui: &Rc<Ui>) {
    let ui_albums = ui.clone();
    ui.gallery.albums_btn.clone().connect_toggled(move |btn| {
        if btn.is_active() {
            show_albums(&ui_albums);
        }
    });

    // Only the button being switched *on* acts: the group fires `toggled` for
    // the one going off too, and acting on both would load twice.
    let ui_photos = ui.clone();
    ui.gallery.photos_btn.clone().connect_toggled(move |btn| {
        if !btn.is_active() {
            return;
        }
        // The timeline is reloaded rather than restored, because an open album
        // left the model holding its own photos.
        close_album(&ui_photos);
        ui_photos.gallery.content.set_visible_child_name("timeline");
        load_gallery(&ui_photos, false);
    });

    let ui_back = ui.clone();
    ui.gallery.back.clone().connect_clicked(move |_| {
        show_albums(&ui_back);
    });
}
