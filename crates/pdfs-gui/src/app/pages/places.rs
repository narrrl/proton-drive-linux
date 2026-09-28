//! The Places view of the Photos page: a grid of the towns the photos were
//! taken in, and the place that opens when one is clicked.
//!
//! An open place works like an open album: it is parked in
//! [`GalleryState::place`], and [`load_gallery`] pages
//! [`Request::PlacePhotos`] instead of the timeline.

use crate::*;

/// Edge length in px of a place's cover, the same as an album's.
const COVER_EDGE: i32 = 200;

/// Edge length in px of a photo in the list of photos taken on one spot.
const SPOT_EDGE: i32 = 96;

/// How often the listing is asked for again while photo locations are read.
const MAPPING_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Show the Places grid and (re)load it. Called when the Places toggle goes on.
pub(crate) fn show_places(ui: &Rc<Ui>) {
    show_place_grid(ui);
    load_places(ui);
}

/// Switch to the place grid as it was last filled, keeping its scroll
/// position. The timeline's filters do not apply to places, so they go.
pub(crate) fn show_place_grid(ui: &Rc<Ui>) {
    close_place(ui);
    close_album(ui);
    ui.gallery.filters.set_visible(false);
    ui.gallery.content.set_visible_child_name("places");
    ui.gallery
        .title
        .set_subtitle(&places_subtitle(ui.gallery.place_count.get()));
}

/// Ask the daemon for the place listing and rebuild the grid.
pub(crate) fn load_places(ui: &Rc<Ui>) {
    if ui.gallery.places_loading.get() {
        return;
    }
    ui.gallery.places_loading.set(true);
    // A grid already on screen stays there while it refreshes.
    if ui.gallery.places_stack.visible_child_name().as_deref() != Some("grid") {
        places_status(
            ui,
            "mark-location-symbolic",
            &gettext("Loading places…"),
            &gettext("Finding where your photos were taken."),
        );
    }

    let rx = spawn_request(ui.dirs.control_socket(), Request::PhotoPlaces);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.gallery.places_loading.set(false);
        let mapping = match &result {
            Ok(Ok(Response::Places { mapping, .. })) => *mapping,
            _ => None,
        };
        sync_mapping(&ui, mapping);
        match result {
            Ok(Ok(Response::Places { items, .. })) if items.is_empty() && mapping.is_some() => {
                ui.gallery.place_count.set(0);
                places_status(
                    &ui,
                    "mark-location-symbolic",
                    &gettext("Mapping your photos…"),
                    &gettext("Places show up here once the locations of your photos are read."),
                )
            }
            Ok(Ok(Response::Places { items, .. })) if items.is_empty() => {
                ui.gallery.place_count.set(0);
                if ui.gallery.place.borrow().is_none() {
                    ui.gallery.title.set_subtitle("");
                }
                places_status(
                    &ui,
                    "mark-location-symbolic",
                    &gettext("No places"),
                    &gettext(
                        "Photos that carry the location they were taken at show up here, grouped by town.",
                    ),
                )
            }
            Ok(Ok(Response::Places { items, .. })) => {
                fill_places(&ui, &items);
                ui.gallery.places_map.set_places(&items);
                show_places_layout(&ui);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &gettext("Couldn't load places"), &message, kind);
                places_status(
                    &ui,
                    "dialog-warning-symbolic",
                    &gettext("Couldn't load places"),
                    &message,
                );
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => places_status(
                &ui,
                "network-offline-symbolic",
                &gettext("Not connected"),
                &gettext("The Proton Drive service didn't respond."),
            ),
        }
    });
}

/// Show how far the daemon has got reading photo locations, and ask again in a
/// moment while it is still reading — the new places land once it is done.
fn sync_mapping(ui: &Rc<Ui>, mapping: Option<MappingProgress>) {
    if let Some(source) = ui.gallery.places_poll.take() {
        source.remove();
    }
    ui.gallery.places_mapping.set_visible(mapping.is_some());
    let Some(MappingProgress { done, total }) = mapping else {
        return;
    };
    ui.gallery.places_mapping_label.set_label(&gettext_f(
        // Translators: {done} and {total} are numbers of photos.
        "Mapping photos: {done} of {total}",
        &[("done", &thousands(done)), ("total", &thousands(total))],
    ));
    ui.gallery.places_mapping_bar.set_fraction(if total == 0 {
        0.0
    } else {
        done as f64 / total as f64
    });
    let ui_poll = ui.clone();
    let source = glib::timeout_add_local_once(MAPPING_POLL, move || {
        ui_poll.gallery.places_poll.take();
        let on_places = ui_poll.gallery.content.visible_child_name().as_deref() == Some("places");
        if on_places && ui_poll.gallery.place.borrow().is_none() {
            load_places(&ui_poll);
        }
    });
    ui.gallery.places_poll.replace(Some(source));
}

/// Replace the grid's cards with `places`, the one with the most photos first.
fn fill_places(ui: &Rc<Ui>, places: &[PlaceInfo]) {
    let scroll = ui
        .gallery
        .places
        .ancestor(gtk4::ScrolledWindow::static_type())
        .and_downcast::<gtk4::ScrolledWindow>();
    let position = scroll.as_ref().map(|s| s.vadjustment().value());
    while let Some(child) = ui.gallery.places.first_child() {
        ui.gallery.places.remove(&child);
    }
    ui.gallery.place_count.set(places.len());
    if ui.gallery.place.borrow().is_none() {
        ui.gallery
            .title
            .set_subtitle(&places_subtitle(places.len()));
    }
    for place in places {
        ui.gallery.places.append(&place_card(ui, place));
    }
    schedule_thumbs(ui);
    if let (Some(scroll), Some(position)) = (scroll, position) {
        glib::idle_add_local_once(move || scroll.vadjustment().set_value(position));
    }
}

pub(crate) fn places_subtitle(count: usize) -> String {
    ngettext_f("{n} place", "{n} places", count as u64, &[])
}

/// "Germany · 12 photos": where the town is, and how much was taken there.
pub(crate) fn place_subtitle(place: &PlaceInfo) -> String {
    ngettext_f(
        // Translators: {country} is a country name, {n} a number of photos.
        "{country} · {n} photo",
        "{country} · {n} photos",
        place.photo_count as u64,
        &[("country", &country_name(&place.country))],
    )
}

/// One place as a clickable card: its newest photo, the town, and the country
/// with the number of photos taken there.
fn place_card(ui: &Rc<Ui>, place: &PlaceInfo) -> gtk4::Button {
    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    picture.add_css_class("photo-thumb");

    let placeholder = gtk4::Image::builder()
        .icon_name("mark-location-symbolic")
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
        .label(&place.name)
        .halign(gtk4::Align::Start)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    name.add_css_class("heading");

    let subtitle = place_subtitle(place);
    let count = gtk4::Label::builder()
        .label(&subtitle)
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
    button.set_tooltip_text(Some(&format!("{}\n{subtitle}", place.name)));

    want_cover(ui, place.cover.uid.clone(), &picture);
    // The map marks the town with the same photo.
    cover_map_marker(ui, &place.cover.uid, &picture);

    let ui_open = ui.clone();
    let place = place.clone();
    button.connect_clicked(move |_| open_place(&ui_open, place.clone()));
    button
}

/// Hand the map the photo `picture` shows once it is there.
fn cover_map_marker(ui: &Rc<Ui>, uid: &str, picture: &gtk4::Picture) {
    let map = Rc::downgrade(&ui.gallery.places_map);
    let set = move |picture: &gtk4::Picture, uid: &str| {
        let texture = picture.paintable().and_downcast::<gtk4::gdk::Texture>();
        if let (Some(map), Some(texture)) = (map.upgrade(), texture) {
            map.set_cover(uid, &texture);
        }
    };
    set(picture, uid);
    let uid = uid.to_owned();
    picture.connect_paintable_notify(move |picture| set(picture, &uid));
}

/// Get the map the thumbnail of a photo it marks on its own, through the
/// gallery's thumbnail pipeline.
fn want_map_thumb(ui: &Rc<Ui>, photo: &PhotoItem) {
    if let Some(texture) = ui.gallery.photo_tex.borrow().get(&photo.uid) {
        ui.gallery.places_map.set_cover(&photo.uid, texture);
        return;
    }
    // A town's card may be waiting for the same photo: share its picture
    // rather than take its place in the queue.
    let waiting = ui.gallery.thumb_wanted.borrow().get(&photo.uid).cloned();
    let picture = waiting.unwrap_or_else(|| {
        let picture = gtk4::Picture::new();
        want_thumb(ui, photo, &picture);
        picture
    });
    cover_map_marker(ui, &photo.uid, &picture);
    schedule_thumbs(ui);
}

/// List the photos taken on one spot of the map in a popover at `(x, y)`;
/// clicking one opens it in its town.
fn show_spot_photos(ui: &Rc<Ui>, photos: Vec<(PlaceInfo, PhotoItem)>, (x, y): (f64, f64)) {
    let Some((place, _)) = photos.first() else {
        return;
    };
    let popover = gtk4::Popover::new();
    popover.set_parent(ui.gallery.places_map.widget());
    popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);
    let title = gtk4::Label::builder()
        .label(&place.name)
        .halign(gtk4::Align::Start)
        .build();
    title.add_css_class("heading");
    let count = gtk4::Label::builder()
        .label(ngettext_f(
            "{n} photo",
            "{n} photos",
            photos.len() as u64,
            &[],
        ))
        .halign(gtk4::Align::Start)
        .build();
    count.add_css_class("dim-label");
    count.add_css_class("caption");
    content.append(&title);
    content.append(&count);

    let grid = gtk4::FlowBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(1)
        .max_children_per_line(4)
        .row_spacing(4)
        .column_spacing(4)
        .build();
    for (place, photo) in photos {
        let picture = gtk4::Picture::builder()
            .content_fit(gtk4::ContentFit::Cover)
            .can_shrink(true)
            .width_request(SPOT_EDGE)
            .height_request(SPOT_EDGE)
            .build();
        spot_thumb(ui, &photo, &picture);
        let button = gtk4::Button::builder()
            .child(&picture)
            .tooltip_text(format_capture_time(photo.capture_time))
            .build();
        button.add_css_class("flat");
        button.add_css_class("places-spot-photo");
        let ui_open = ui.clone();
        let popover_open = popover.clone();
        button.connect_clicked(move |_| {
            popover_open.popdown();
            open_place_photo(&ui_open, place.clone(), photo.uid.clone());
        });
        grid.append(&button);
    }
    schedule_thumbs(ui);
    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(360)
        .child(&grid)
        .build();
    content.append(&scroller);
    popover.set_child(Some(&content));
    // Gone with its last use; its parent is the map, which stays.
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
}

/// Show photo's thumbnail in `picture`, sharing one another widget is
/// already waiting for.
fn spot_thumb(ui: &Rc<Ui>, photo: &PhotoItem, picture: &gtk4::Picture) {
    let waiting = ui.gallery.thumb_wanted.borrow().get(&photo.uid).cloned();
    match waiting {
        Some(waiting) => {
            waiting
                .bind_property("paintable", picture, "paintable")
                .sync_create()
                .build();
        }
        None => want_thumb(ui, photo, picture),
    }
}

/// Get the map the photos of town `id` with where each was taken.
fn load_place_locations(ui: &Rc<Ui>, id: u32) {
    let rx = spawn_request(ui.dirs.control_socket(), Request::PlaceLocations { id });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::PhotoLocations { items })) => {
                ui.gallery.places_map.set_locations(id, items)
            }
            // The map keeps showing the town as one marker.
            Ok(Ok(Response::Error { message, .. })) => {
                tracing::debug!("place locations failed: {message}")
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => tracing::debug!("place locations: no reply"),
        }
    });
}

/// Open `place` and then photo `uid` in it, paging through the town until
/// the photo has loaded.
fn open_place_photo(ui: &Rc<Ui>, place: PlaceInfo, uid: String) {
    open_place(ui, place);
    view_when_loaded(ui, uid);
}

fn view_when_loaded(ui: &Rc<Ui>, uid: String) {
    let ui_page = ui.clone();
    ui.gallery.page_waiters.borrow_mut().push(Box::new(move || {
        let ui = ui_page;
        if find_photo_index(&ui.gallery.model, &uid).is_some() {
            open_photo_viewer(&ui, uid);
        } else if ui.gallery.has_more.get() && ui.gallery.place.borrow().is_some() {
            view_when_loaded(&ui, uid);
            ui.gallery.burst.set(true);
            load_gallery(&ui, true);
        }
    }));
}

/// Open one place in the gallery: the timeline view, paged from that town.
fn open_place(ui: &Rc<Ui>, place: PlaceInfo) {
    ui.gallery.title.set_title(&place.name);
    ui.gallery.title.set_subtitle(&place_subtitle(&place));
    *ui.gallery.place.borrow_mut() = Some(place);
    ui.gallery.timeline_stale.set(true);

    // As in an album: no filters, no Upload, no view switch — back leads out.
    ui.gallery.filters.set_visible(false);
    ui.gallery.view_switch.set_visible(false);
    ui.gallery.upload.set_visible(false);
    ui.gallery.back.set_visible(true);
    ui.gallery.kind.set(None);
    ui.gallery.range.set(None);

    ui.gallery.content.set_visible_child_name("timeline");
    load_gallery(ui, false);
}

/// Leave an open place, restoring the timeline's header. A no-op when no place
/// is open.
pub(crate) fn close_place(ui: &Rc<Ui>) {
    if ui.gallery.place.borrow_mut().take().is_none() {
        return;
    }
    ui.gallery.title.set_title(&gettext("Photos"));
    ui.gallery.view_switch.set_visible(true);
    ui.gallery.upload.set_visible(true);
    ui.gallery.back.set_visible(false);
}

/// Show the place grid's own status page (loading, empty, or an error).
fn places_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str) {
    ui.gallery.places_status.set_icon_name(Some(icon));
    ui.gallery.places_status.set_title(title);
    ui.gallery.places_status.set_description(Some(description));
    ui.gallery.places_stack.set_visible_child_name("status");
}

/// Wire the Places toggle of the view switcher.
pub(crate) fn wire_places(ui: &Rc<Ui>) {
    let ui_places = ui.clone();
    ui.gallery.places_btn.clone().connect_toggled(move |btn| {
        if btn.is_active() {
            show_places(&ui_places);
        }
    });

    let ui_layout = ui.clone();
    ui.gallery.places_map_btn.connect_toggled(move |_| {
        // A status page (loading, empty, an error) stays until a listing
        // replaces it; only the grid and the map swap.
        if ui_layout
            .gallery
            .places_stack
            .visible_child_name()
            .as_deref()
            != Some("status")
        {
            show_places_layout(&ui_layout);
        }
    });

    let map = &ui.gallery.places_map;
    let ui_open = ui.clone();
    map.connect_open(move |place| open_place(&ui_open, place));
    let ui_photo = ui.clone();
    map.connect_open_photo(move |place, uid| open_place_photo(&ui_photo, place, uid));
    let ui_locations = ui.clone();
    map.connect_want_locations(move |id| load_place_locations(&ui_locations, id));
    let ui_thumb = ui.clone();
    map.connect_want_thumb(move |photo| want_map_thumb(&ui_thumb, &photo));
    let ui_spot = ui.clone();
    map.connect_open_spot(move |photos, at| show_spot_photos(&ui_spot, photos, at));
    map.set_online(ui.dirs.load_config().online_map, ui.dirs.cache_dir());
}

/// Switch to the Places map and zoom in on `(latitude, longitude)`, where a
/// photo in the viewer was taken.
pub(crate) fn show_on_map(ui: &Rc<Ui>, latitude: f64, longitude: f64) {
    ui.gallery.places_map_btn.set_active(true);
    if ui.gallery.places_btn.is_active() {
        // Already in Places, perhaps inside a town: back out to the listing.
        show_places(ui);
    } else {
        ui.gallery.places_btn.set_active(true);
    }
    ui.gallery.places_map.show_spot(latitude, longitude);
}

/// Show the places as cards or on the map, as the Grid/Map toggle says.
fn show_places_layout(ui: &Rc<Ui>) {
    let layout = if ui.gallery.places_map_btn.is_active() {
        "map"
    } else {
        "grid"
    };
    ui.gallery.places_stack.set_visible_child_name(layout);
}
