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
    show_album_grid(ui);
    load_albums(ui);
}

/// Switch to the album grid as it was last filled, keeping its scroll
/// position. The timeline's filters do not apply to albums, so they go.
fn show_album_grid(ui: &Rc<Ui>) {
    close_album(ui);
    ui.gallery.filters.set_visible(false);
    ui.gallery.content.set_visible_child_name("albums");
}

/// Ask the daemon for the album listing and rebuild the grid.
pub(crate) fn load_albums(ui: &Rc<Ui>) {
    if ui.gallery.albums_loading.get() {
        return;
    }
    ui.gallery.albums_loading.set(true);
    // A grid already on screen stays there while it refreshes, rather than
    // blinking to a loading page and losing its scroll position.
    if ui.gallery.albums_stack.visible_child_name().as_deref() != Some("grid") {
        albums_status(
            ui,
            "view-grid-symbolic",
            &gettext("Loading albums…"),
            &gettext("Reading your Proton Drive albums."),
        );
    }

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
                &gettext("No photo library"),
                &gettext("This Proton account doesn't have Photos enabled."),
            ),
            Ok(Ok(Response::Albums { items, .. })) if items.is_empty() => albums_status(
                &ui,
                "view-grid-symbolic",
                &gettext("No albums"),
                &gettext("Create one with New Album, or in Proton Photos."),
            ),
            Ok(Ok(Response::Albums { items, .. })) => {
                fill_albums(&ui, &items);
                ui.gallery.albums_stack.set_visible_child_name("grid");
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &gettext("Couldn't load albums"), &message, kind);
                albums_status(
                    &ui,
                    "dialog-warning-symbolic",
                    &gettext("Couldn't load albums"),
                    &message,
                );
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => albums_status(
                &ui,
                "network-offline-symbolic",
                &gettext("Not connected"),
                &gettext("The Proton Drive mount service didn't respond."),
            ),
        }
    });
}

/// Replace the grid's cards with `albums`, in the order the daemon gave them
/// (newest activity first).
fn fill_albums(ui: &Rc<Ui>, albums: &[AlbumInfo]) {
    let scroll = ui
        .gallery
        .albums
        .ancestor(gtk4::ScrolledWindow::static_type())
        .and_downcast::<gtk4::ScrolledWindow>();
    let position = scroll.as_ref().map(|s| s.vadjustment().value());
    while let Some(child) = ui.gallery.albums.first_child() {
        ui.gallery.albums.remove(&child);
    }
    *ui.gallery.album_list.borrow_mut() = albums.to_vec();
    if ui.gallery.album.borrow().is_none() {
        ui.gallery.title.set_subtitle(&ngettext_f(
            "{n} album",
            "{n} albums",
            albums.len() as u64,
            &[],
        ));
    }
    for album in albums {
        ui.gallery.albums.append(&album_card(ui, album));
    }
    // The covers are ordinary photos as far as the daemon is concerned, so the
    // batch that fills them is the gallery's own.
    schedule_thumbs(ui);
    // A refresh rebuilds the cards; put the view back where the user was.
    if let (Some(scroll), Some(position)) = (scroll, position) {
        glib::idle_add_local_once(move || scroll.vadjustment().set_value(position));
    }
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

    let context = gtk4::GestureClick::builder().button(3).build();
    let ui_context = ui.clone();
    let menu_album = album.clone();
    context.connect_pressed(move |gesture, _, x, y| {
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        if let Some(anchor) = gesture.widget() {
            show_album_menu(&ui_context, &menu_album, &anchor, x, y);
        }
    });
    button.add_controller(context);

    let ui_open = ui.clone();
    let album = album.clone();
    button.connect_clicked(move |_| open_album(&ui_open, album.clone()));
    button
}

/// The album card's menu. Albums shared with us belong to someone else, so
/// they only open.
fn show_album_menu(ui: &Rc<Ui>, album: &AlbumInfo, anchor: &gtk4::Widget, x: f64, y: f64) {
    let mut menu = ActionMenu::new();
    let (ui_c, album_c) = (ui.clone(), album.clone());
    menu.item(&pgettext("verb", "Open"), move || {
        open_album(&ui_c, album_c.clone())
    });
    if !album.shared {
        menu.section();
        let (ui_c, album_c) = (ui.clone(), album.clone());
        menu.item(&gettext("Rename…"), move || {
            prompt_rename_album(&ui_c, &album_c)
        });
        menu.section();
        let (ui_c, album_c) = (ui.clone(), album.clone());
        menu.item(&gettext("Delete Album…"), move || {
            confirm_delete_album(&ui_c, &album_c)
        });
    }
    menu.popup_at(anchor, x, y);
}

/// Ask for an album name. `on_name` gets the trimmed name; Create stays off
/// while the entry is blank.
fn prompt_album_name(
    ui: &Rc<Ui>,
    heading: &str,
    action: &str,
    current: &str,
    on_name: impl Fn(String) + 'static,
) {
    let win = ui_window(ui);
    let dialog = adw::AlertDialog::builder().heading(heading).build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title(gettext("Album name"))
        .activates_default(true)
        .build();
    row.set_text(current);
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("ok", action);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("ok", !current.trim().is_empty());
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let dialog_typed = dialog.clone();
    row.connect_changed(move |row| {
        dialog_typed.set_response_enabled("ok", !row.text().trim().is_empty());
    });
    dialog.connect_response(None, move |_, resp| {
        let name = row.text().trim().to_string();
        if resp == "ok" && !name.is_empty() {
            on_name(name);
        }
    });
    dialog.present(win.as_ref());
}

/// Ask for a name and create an album, then add `photos` to it, if any.
pub(crate) fn prompt_new_album(ui: &Rc<Ui>, photos: Vec<String>) {
    let ui_c = ui.clone();
    let (heading, action) = (gettext("New Album"), gettext("Create"));
    prompt_album_name(ui, &heading, &action, "", move |name| {
        let rx = spawn_request(ui_c.dirs.control_socket(), Request::CreateAlbum { name });
        let ui = ui_c.clone();
        let photos = photos.clone();
        ui.busy_begin();
        glib::spawn_future_local(async move {
            let reply = rx.recv().await;
            ui.busy_end();
            match reply {
                Ok(Ok(Response::AlbumCreated { uid })) => {
                    if photos.is_empty() {
                        toast(&ui, &gettext("Album created"));
                        reload_album_grid(&ui);
                    } else {
                        add_to_album(&ui, uid, photos);
                    }
                }
                Ok(Ok(Response::Error { message, kind })) => {
                    toast_failure(&ui, &gettext("Couldn't create the album"), &message, kind)
                }
                _ => toast_error(
                    &ui,
                    &gettext("Couldn't create the album"),
                    &gettext("The mount service didn't respond."),
                ),
            }
        });
    });
}

fn prompt_rename_album(ui: &Rc<Ui>, album: &AlbumInfo) {
    let (ui_c, uid) = (ui.clone(), album.uid.clone());
    let (heading, action) = (gettext("Rename Album"), gettext("Rename"));
    prompt_album_name(ui, &heading, &action, &album.name, move |name| {
        run_album_request(
            &ui_c,
            Request::RenameAlbum {
                uid: uid.clone(),
                name,
            },
            gettext("Album renamed"),
            gettext("Couldn't rename the album"),
        );
    });
}

fn confirm_delete_album(ui: &Rc<Ui>, album: &AlbumInfo) {
    let Some(win) = ui_window(ui) else { return };
    let (ui_c, uid) = (ui.clone(), album.uid.clone());
    confirm_destructive(
        &win,
        &gettext("Delete Album?"),
        // Translators: {name} is the album's name.
        &gettext_f(
            "“{name}” is deleted. Its photos stay in your timeline.",
            &[("name", &album.name)],
        ),
        &gettext("Delete"),
        move || {
            run_album_request(
                &ui_c,
                Request::DeleteAlbum {
                    uid: uid.clone(),
                    delete_photos: false,
                },
                gettext("Album deleted"),
                gettext("Couldn't delete the album"),
            )
        },
    );
}

/// Send an album rename or delete and refresh the grid once it lands.
fn run_album_request(ui: &Rc<Ui>, request: Request, done: String, failed: String) {
    let rx = spawn_request(ui.dirs.control_socket(), request);
    let ui = ui.clone();
    ui.busy_begin();
    glib::spawn_future_local(async move {
        let reply = rx.recv().await;
        ui.busy_end();
        match reply {
            Ok(Ok(Response::Ok { .. })) => {
                toast(&ui, &done);
                reload_album_grid(&ui);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &failed, &message, kind)
            }
            _ => toast_error(&ui, &failed, &gettext("The mount service didn't respond.")),
        }
    });
}

/// Refill the album grid if it is on screen; otherwise it reloads when shown.
fn reload_album_grid(ui: &Rc<Ui>) {
    if ui.gallery.content.visible_child_name().as_deref() == Some("albums") {
        load_albums(ui);
    }
}

/// Offer our own albums to add `photos` to, plus a new one.
pub(crate) fn prompt_add_to_album(ui: &Rc<Ui>, photos: Vec<String>) {
    let rx = spawn_request(ui.dirs.control_socket(), Request::PhotoAlbums);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let albums: Vec<AlbumInfo> = match rx.recv().await {
            Ok(Ok(Response::Albums { items, .. })) => {
                items.into_iter().filter(|album| !album.shared).collect()
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &gettext("Couldn't load albums"), &message, kind);
                return;
            }
            _ => {
                toast_error(
                    &ui,
                    &gettext("Couldn't load albums"),
                    &gettext("The mount service didn't respond."),
                );
                return;
            }
        };
        if albums.is_empty() {
            prompt_new_album(&ui, photos);
            return;
        }

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        for album in &albums {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&album.name))
                .subtitle(album_subtitle(album))
                .activatable(true)
                .build();
            list.append(&row);
        }
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(360)
            .child(&list)
            .build();
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Add to Album"))
            .body(match photos.len() {
                1 => gettext("Choose an album for this photo."),
                n => ngettext_f(
                    "Choose an album for {n} photo.",
                    "Choose an album for {n} photos.",
                    n as u64,
                    &[],
                ),
            })
            .extra_child(&scroll)
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("new", &gettext("New Album…"));
        dialog.set_close_response("cancel");

        let (ui_c, dialog_c, photos_c) = (ui.clone(), dialog.clone(), photos.clone());
        list.connect_row_activated(move |_, row| {
            if let Some(album) = albums.get(row.index().max(0) as usize) {
                dialog_c.close();
                add_to_album(&ui_c, album.uid.clone(), photos_c.clone());
            }
        });
        let ui_c = ui.clone();
        dialog.connect_response(Some("new"), move |_, _| {
            prompt_new_album(&ui_c, photos.clone())
        });
        dialog.present(ui_window(&ui).as_ref());
    });
}

fn add_to_album(ui: &Rc<Ui>, album: String, photos: Vec<String>) {
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::AddToAlbum { uid: album, photos },
    );
    let ui = ui.clone();
    ui.busy_begin();
    glib::spawn_future_local(async move {
        let reply = rx.recv().await;
        ui.busy_end();
        match reply {
            Ok(Ok(Response::AlbumChanged { changed, failed })) => {
                if let Some(failure) = failed.first() {
                    toast_error(
                        &ui,
                        &gettext("Some photos couldn't be added"),
                        &failure.message,
                    );
                } else {
                    toast(
                        &ui,
                        &ngettext_f(
                            "Added {n} photo to the album",
                            "Added {n} photos to the album",
                            changed.len() as u64,
                            &[],
                        ),
                    );
                }
                reload_album_grid(&ui);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, &gettext("Couldn't add to the album"), &message, kind)
            }
            _ => toast_error(
                &ui,
                &gettext("Couldn't add to the album"),
                &gettext("The mount service didn't respond."),
            ),
        }
    });
}

/// Take photos out of the open album. They leave the view at once and come
/// back if the server refuses; the timeline keeps them either way.
pub(crate) fn remove_from_album(ui: &Rc<Ui>, album: &AlbumInfo, photos: Vec<String>) {
    let removed = remove_photos(ui, &photos);
    set_selection_mode(ui, false);
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::RemoveFromAlbum {
            uid: album.uid.clone(),
            photos,
        },
    );
    let ui = ui.clone();
    ui.busy_begin();
    glib::spawn_future_local(async move {
        let reply = rx.recv().await;
        ui.busy_end();
        match reply {
            Ok(Ok(Response::AlbumChanged { changed, failed })) => {
                if !failed.is_empty() {
                    let kept: Vec<PhotoItem> = removed
                        .iter()
                        .filter(|photo| failed.iter().any(|f| f.uid == photo.uid))
                        .cloned()
                        .collect();
                    restore_photos(&ui, kept);
                    toast_error(
                        &ui,
                        &gettext("Some photos couldn't be removed from the album"),
                        &failed[0].message,
                    );
                }
                if changed.is_empty() {
                    return;
                }
                // The open album's count, as the header shows it.
                let mut open = ui.gallery.album.borrow_mut();
                if let Some(album) = open.as_mut() {
                    album.photo_count = album.photo_count.saturating_sub(changed.len());
                    ui.gallery.title.set_subtitle(&album_subtitle(album));
                }
                drop(open);
                toast(
                    &ui,
                    &ngettext_f(
                        "Removed {n} photo from the album",
                        "Removed {n} photos from the album",
                        changed.len() as u64,
                        &[],
                    ),
                );
            }
            Ok(Ok(Response::Error { message, kind })) => {
                restore_photos(&ui, removed);
                toast_failure(
                    &ui,
                    &gettext("Couldn't remove from the album"),
                    &message,
                    kind,
                )
            }
            _ => {
                restore_photos(&ui, removed);
                toast_error(
                    &ui,
                    &gettext("Couldn't remove from the album"),
                    &gettext("The mount service didn't respond."),
                )
            }
        }
    });
}

/// "12 photos", plus where the album came from when it isn't ours.
fn album_subtitle(album: &AlbumInfo) -> String {
    let count = album.photo_count as u64;
    if album.shared {
        ngettext_f(
            "{n} photo · shared with me",
            "{n} photos · shared with me",
            count,
            &[],
        )
    } else {
        ngettext_f("{n} photo", "{n} photos", count, &[])
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
    ui.gallery.timeline_stale.set(true);

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
    ui.gallery.title.set_title(&gettext("Photos"));
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
        close_album(&ui_photos);
        ui_photos.gallery.filters.set_visible(true);
        ui_photos.gallery.content.set_visible_child_name("timeline");
        // The timeline comes back as it was left unless an album took over the
        // model since; then it has to be reloaded.
        if ui_photos.gallery.timeline_stale.replace(false) || ui_photos.gallery.model.n_items() == 0
        {
            load_gallery(&ui_photos, false);
        } else {
            update_gallery_subtitle(&ui_photos);
        }
    });

    // Back returns to the grid the album was opened from, scrolled where it
    // was, instead of reloading it.
    let ui_back = ui.clone();
    ui.gallery.back.clone().connect_clicked(move |_| {
        show_album_grid(&ui_back);
        let count = ui_back.gallery.album_list.borrow().len();
        ui_back.gallery.title.set_subtitle(&ngettext_f(
            "{n} album",
            "{n} albums",
            count as u64,
            &[],
        ));
    });
}
