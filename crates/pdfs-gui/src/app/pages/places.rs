//! The Places view of the Photos page: a grid of the towns the photos were
//! taken in, and the place that opens when one is clicked.
//!
//! An open place works like an open album: it is parked in
//! [`GalleryState::place`], and [`load_gallery`] pages
//! [`Request::PlacePhotos`] instead of the timeline.

use crate::*;

/// Edge length in px of a place's cover, the same as an album's.
const COVER_EDGE: i32 = 200;

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
        match result {
            Ok(Ok(Response::Places { items })) if items.is_empty() => {
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
            Ok(Ok(Response::Places { items })) => {
                fill_places(&ui, &items);
                ui.gallery.places_stack.set_visible_child_name("grid");
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
                &gettext("The Proton Drive mount service didn't respond."),
            ),
        }
    });
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

fn places_subtitle(count: usize) -> String {
    ngettext_f("{n} place", "{n} places", count as u64, &[])
}

/// "Germany · 12 photos": where the town is, and how much was taken there.
fn place_subtitle(place: &PlaceInfo) -> String {
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

    let ui_open = ui.clone();
    let place = place.clone();
    button.connect_clicked(move |_| open_place(&ui_open, place.clone()));
    button
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
}
