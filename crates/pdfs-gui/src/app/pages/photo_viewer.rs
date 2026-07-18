use crate::*;

/// The lightbox's mutable parts, shared by [`load_photo`], [`navigate_photo`] and
/// the button/key handlers so each one takes a single handle instead of a dozen
/// widget arguments.
pub(crate) struct Viewer {
    pub(crate) picture: gtk4::Picture,
    pub(crate) spinner: gtk4::Spinner,
    pub(crate) status: gtk4::Label,
    pub(crate) title: gtk4::Label,
    pub(crate) counter: gtk4::Label,
    pub(crate) prev: gtk4::Button,
    pub(crate) next: gtk4::Button,
    /// Details drawer: the toggle that reveals it, the rows it fills in, and the
    /// "Show on map" button (hidden when the photo carries no GPS tags).
    pub(crate) info_toggle: gtk4::ToggleButton,
    pub(crate) info_revealer: gtk4::Revealer,
    pub(crate) info_rows: gtk4::Box,
    pub(crate) info_map: gtk4::Button,
    /// Coordinates behind `info_map`, once a photo with GPS tags is shown.
    pub(crate) coords: RefCell<Option<(f64, f64)>>,
    /// uid of the photo currently on screen.
    pub(crate) uid: RefCell<String>,
    /// On-disk path of the full-size photo, once it has been downloaded.
    pub(crate) path: RefCell<Option<String>>,
    /// True while the full-size photo is still downloading and what's on screen is
    /// the upscaled thumbnail — a click-outside-to-close hit test has to size the
    /// image from the *thumbnail's* ratio in that window, but more importantly a
    /// late reply for a photo the user has already navigated away from must not
    /// overwrite the new one. Guarded by comparing against `uid`.
    pub(crate) loading: Cell<bool>,
}

/// Camera/exposure/location facts pulled from a photo's own EXIF tags, as
/// label/value pairs for the details drawer.
pub(crate) struct ExifInfo {
    /// `("Camera", "Apple iPhone 15")` and friends; empty when the file has no
    /// EXIF at all, which is normal for screenshots and re-encoded images.
    pub(crate) fields: Vec<(&'static str, String)>,
    /// Decimal degrees, if the photo is geotagged.
    pub(crate) coords: Option<(f64, f64)>,
}

/// Show the photo behind `uid`: paint its (already cached) thumbnail immediately
/// so the lightbox never opens on a blank screen, ask the daemon for the
/// full-size file, and swap it in — plus its EXIF — when it lands.
pub(crate) fn load_photo(ui: &Rc<Ui>, viewer: &Rc<Viewer>, uid: String) {
    viewer.spinner.set_visible(true);
    viewer.spinner.start();
    viewer.status.set_visible(false);
    viewer.loading.set(true);
    *viewer.path.borrow_mut() = None;
    clear_info(viewer);

    // The thumbnail the gallery already decoded stands in for the full photo
    // while it downloads: blurry for a moment beats black for a second.
    match ui.gallery.photo_tex.borrow().get(&uid) {
        Some(texture) => viewer.picture.set_paintable(Some(texture)),
        None => viewer.picture.set_paintable(gtk4::gdk::Paintable::NONE),
    }

    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenPhoto { uid: uid.clone() },
    );
    let viewer = viewer.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        // The user may have moved on while this was in flight; that photo's own
        // request owns the viewer now.
        if *viewer.uid.borrow() != uid {
            return;
        }
        viewer.spinner.stop();
        viewer.spinner.set_visible(false);
        viewer.loading.set(false);

        let fail = |message: &str| {
            viewer.status.set_label(message);
            viewer.status.set_visible(true);
        };
        match result {
            Ok(Ok(Response::FilePath { path })) => match gtk4::gdk::Texture::from_filename(&path) {
                Ok(texture) => {
                    viewer.picture.set_paintable(Some(&texture));
                    *viewer.path.borrow_mut() = Some(path.clone());
                    show_info(&viewer, &path, read_exif(&path));
                }
                Err(e) => {
                    tracing::error!("Failed to load texture for {path}: {e}");
                    fail("Couldn't render this photo.");
                }
            },
            Ok(Ok(Response::Error { message, .. })) => fail(&message),
            Ok(Ok(_)) => fail("Unexpected reply from the mount service."),
            Ok(Err(_)) | Err(_) => fail("Couldn't reach Proton Drive."),
        }
    });
}

/// Warm the cache with the photo `delta` steps away, so stepping there is
/// instant. Fire-and-forget: the daemon serves control connections concurrently,
/// so this rides alongside whatever the user does next, and a failure here is
/// simply a photo that downloads on arrival like it used to.
pub(crate) fn prefetch_photo(ui: &Rc<Ui>, viewer: &Rc<Viewer>, delta: i32) {
    let model = &ui.gallery.model;
    let current = find_photo_index(model, &viewer.uid.borrow()).unwrap_or(0) as i32;
    let index = current + delta;
    if index < 0 || index >= model.n_items() as i32 {
        return;
    }
    let Some(photo) = model
        .item(index as u32)
        .and_downcast::<BoxedAnyObject>()
        .map(|boxed| boxed.borrow::<PhotoItem>().clone())
    else {
        return;
    };
    // Never prefetch a video: the lightbox skips over it, and warming it would
    // mean downloading a whole clip the user won't watch from here.
    if photo.kind == PhotoKind::Video {
        return;
    }
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenPhoto { uid: photo.uid },
    );
    glib::spawn_future_local(async move {
        let _ = rx.recv().await;
    });
}

/// Reset the details drawer while the next photo is in flight, so it never shows
/// the previous photo's camera against the new image.
pub(crate) fn clear_info(viewer: &Rc<Viewer>) {
    *viewer.coords.borrow_mut() = None;
    viewer.info_map.set_visible(false);
    while let Some(row) = viewer.info_rows.first_child() {
        viewer.info_rows.remove(&row);
    }
    let label = gtk4::Label::builder()
        .label("Reading photo details…")
        .halign(gtk4::Align::Start)
        .build();
    label.add_css_class("dim-label");
    viewer.info_rows.append(&label);
}

/// Fill the details drawer for the photo now on screen: its own file facts
/// (size, dimensions) plus whatever EXIF it carries.
pub(crate) fn show_info(viewer: &Rc<Viewer>, path: &str, info: ExifInfo) {
    while let Some(row) = viewer.info_rows.first_child() {
        viewer.info_rows.remove(&row);
    }

    let mut fields: Vec<(&str, String)> = Vec::new();
    if let Ok(meta) = std::fs::metadata(path) {
        fields.push(("Size", human_bytes(meta.len())));
    }
    fields.extend(info.fields.iter().map(|(k, v)| (*k, v.clone())));

    if fields.is_empty() {
        let label = gtk4::Label::builder()
            .label("This photo carries no metadata.")
            .halign(gtk4::Align::Start)
            .wrap(true)
            .build();
        label.add_css_class("dim-label");
        viewer.info_rows.append(&label);
    } else {
        let group = gtk4::ListBox::new();
        group.set_selection_mode(gtk4::SelectionMode::None);
        group.add_css_class("boxed-list");
        for (label, value) in fields {
            let row = adw::ActionRow::builder()
                .title(label)
                .subtitle(value)
                .subtitle_selectable(true)
                .build();
            row.add_css_class("property");
            group.append(&row);
        }
        viewer.info_rows.append(&group);
    }

    viewer.info_map.set_visible(info.coords.is_some());
    *viewer.coords.borrow_mut() = info.coords;
}

/// Read the EXIF tags a gallery viewer cares about out of a decrypted photo on
/// disk. Anything missing is simply left out — phone screenshots and re-encoded
/// images legitimately carry no EXIF at all.
pub(crate) fn read_exif(path: &str) -> ExifInfo {
    let mut fields: Vec<(&'static str, String)> = Vec::new();
    let mut coords = None;

    let reader = match std::fs::File::open(path) {
        Ok(file) => {
            match exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)) {
                Ok(reader) => reader,
                Err(e) => {
                    tracing::debug!("no exif in {path}: {e}");
                    return ExifInfo { fields, coords };
                }
            }
        }
        Err(e) => {
            tracing::warn!("cannot open {path} for exif: {e}");
            return ExifInfo { fields, coords };
        }
    };

    let field = |tag: exif::Tag| {
        reader
            .get_field(tag, exif::In::PRIMARY)
            .map(|f| f.display_value().with_unit(&reader).to_string())
    };

    if let Some(size) = field(exif::Tag::PixelXDimension)
        .zip(field(exif::Tag::PixelYDimension))
        .map(|(w, h)| format!("{w} × {h}"))
    {
        fields.push(("Dimensions", size));
    }
    if let Some(taken) = field(exif::Tag::DateTimeOriginal) {
        fields.push(("Taken", taken));
    }

    let camera = [exif::Tag::Make, exif::Tag::Model]
        .iter()
        .filter_map(|tag| field(*tag))
        .collect::<Vec<_>>()
        .join(" ");
    if !camera.is_empty() {
        fields.push(("Camera", camera));
    }
    if let Some(lens) = field(exif::Tag::LensModel) {
        fields.push(("Lens", lens));
    }

    let exposure: Vec<String> = [
        exif::Tag::FNumber,
        exif::Tag::ExposureTime,
        exif::Tag::PhotographicSensitivity,
        exif::Tag::FocalLength,
    ]
    .iter()
    .filter_map(|tag| field(*tag))
    .collect();
    if !exposure.is_empty() {
        fields.push(("Exposure", exposure.join(" · ")));
    }

    if let Some(lat) = gps_degrees(&reader, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef)
        && let Some(lon) = gps_degrees(&reader, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef)
    {
        fields.push(("Location", format!("{lat:.5}, {lon:.5}")));
        coords = Some((lat, lon));
    }

    ExifInfo { fields, coords }
}

/// Convert one GPS coordinate from EXIF's degrees/minutes/seconds rationals to
/// decimal degrees, negating for the S/W hemispheres.
pub(crate) fn gps_degrees(reader: &exif::Exif, tag: exif::Tag, ref_tag: exif::Tag) -> Option<f64> {
    let field = reader.get_field(tag, exif::In::PRIMARY)?;
    let exif::Value::Rational(dms) = &field.value else {
        return None;
    };
    let [deg, min, sec] = dms.get(..3)? else {
        return None;
    };
    let degrees = deg.to_f64() + min.to_f64() / 60.0 + sec.to_f64() / 3600.0;

    let hemisphere = reader
        .get_field(ref_tag, exif::In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .unwrap_or_default();
    let negative = hemisphere.starts_with('S') || hemisphere.starts_with('W');
    Some(if negative { -degrees } else { degrees })
}

pub(crate) fn save_photo_to_disk(window: &gtk4::Window, source_path: &str, original_name: &str) {
    let dialog = gtk4::FileDialog::builder()
        .title("Save Photo")
        .initial_name(original_name)
        .build();
    let source_path_str = source_path.to_string();
    dialog.save(Some(window), gio::Cancellable::NONE, move |res| {
        if let Ok(file) = res
            && let Some(dest_path) = file.path()
        {
            if let Err(e) = std::fs::copy(&source_path_str, &dest_path) {
                tracing::error!("Failed to copy file to {:?}: {}", dest_path, e);
            } else {
                tracing::info!("Saved photo to {:?}", dest_path);
            }
        }
    });
}

/// Step `delta` photos through the flat timeline model and load what lands there.
pub(crate) fn navigate_photo(ui: &Rc<Ui>, viewer: &Rc<Viewer>, delta: i32) {
    let model = &ui.gallery.model;
    let n = model.n_items();
    if n == 0 {
        return;
    }
    let uid_val = viewer.uid.borrow().clone();
    let current_idx = find_photo_index(model, &uid_val).unwrap_or(0);

    // Where the raw step lands (prev/next are ±1; Home/End are huge deltas that
    // clamp to the ends), and which way it was travelling.
    let start = (current_idx as i32 + delta).clamp(0, n as i32 - 1);
    let dir = if delta < 0 { -1 } else { 1 };

    // The lightbox renders stills only — a video plays in an external player, so
    // it has no frame here. Land on the nearest renderable item, preferring the
    // travel direction and falling back to scanning inward from a boundary (so
    // Home/End still reach the first/last still even when videos sit at the end).
    let renderable = |idx: u32| -> Option<PhotoItem> {
        model
            .item(idx)
            .and_then(|obj| obj.downcast_ref::<BoxedAnyObject>().map(|b| b.borrow::<PhotoItem>().clone()))
            .filter(|photo| photo.kind != PhotoKind::Video)
    };
    let scan = |from: i32, step: i32| -> Option<(u32, PhotoItem)> {
        let mut probe = from;
        while (0..n as i32).contains(&probe) {
            if let Some(photo) = renderable(probe as u32) {
                return Some((probe as u32, photo));
            }
            probe += step;
        }
        None
    };
    // Scan the travel direction first. Only fall back to scanning inward when the
    // step landed on a boundary — that is the Home/End case (jump to first/last,
    // then step inward past any videos there); a mid-list prev/next with only
    // videos ahead should simply stay put rather than reverse.
    let at_boundary = start == 0 || start == n as i32 - 1;
    let landing = scan(start, dir).or_else(|| at_boundary.then(|| scan(start, -dir)).flatten());
    let Some((next_idx, photo)) = landing else {
        return;
    };

    *viewer.uid.borrow_mut() = photo.uid.clone();
    show_photo_position(viewer, next_idx, n, photo.capture_time);
    load_photo(ui, viewer, photo.uid.clone());
    // Keep walking in the same direction: the next one is likely where they're
    // headed, so have it in the cache before they ask.
    prefetch_photo(ui, viewer, delta.signum());
}

/// Set the lightbox's title (the photo's date) and its "12 of 340" counter.
pub(crate) fn show_photo_position(viewer: &Rc<Viewer>, index: u32, total: u32, capture_time: i64) {
    viewer.prev.set_sensitive(index > 0);
    viewer.next.set_sensitive(index + 1 < total);
    viewer.title.set_label(&format_capture_time(capture_time));
    viewer
        .counter
        .set_label(&format!("{} of {total}", index + 1));
}

/// The in-app lightbox: the photo, edge-to-edge on a dark backdrop, with a
/// floating top bar, prev/next affordances and a details drawer.
///
/// Closing it is deliberately hard to get wrong — Escape, `q`, Ctrl+W, the close
/// button, or a click on the backdrop beside the photo all dismiss it.
pub(crate) fn open_photo_viewer(ui: &Rc<Ui>, initial_uid: String) {
    let parent = ui.stack.root().and_downcast::<gtk4::Window>().unwrap();

    let window = gtk4::Window::builder()
        .title("Photo")
        .modal(true)
        .transient_for(&parent)
        .default_width(1100)
        .default_height(760)
        .build();
    window.add_css_class("photo-viewer-window");

    let overlay = gtk4::Overlay::new();

    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .build();
    overlay.set_child(Some(&picture));

    let prev_btn = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Previous (←)")
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Center)
        .build();
    prev_btn.add_css_class("circular");
    prev_btn.add_css_class("flat");
    prev_btn.add_css_class("viewer-nav-btn");
    overlay.add_overlay(&prev_btn);

    let next_btn = gtk4::Button::builder()
        .icon_name("go-next-symbolic")
        .tooltip_text("Next (→)")
        .halign(gtk4::Align::End)
        .valign(gtk4::Align::Center)
        .build();
    next_btn.add_css_class("circular");
    next_btn.add_css_class("flat");
    next_btn.add_css_class("viewer-nav-btn");
    overlay.add_overlay(&next_btn);

    // Top bar: the photo's date and position on the left, actions on the right,
    // over a gradient so white controls stay legible on a bright photo.
    let title_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    title_label.add_css_class("viewer-title");

    let counter_label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
    counter_label.add_css_class("viewer-counter");

    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    titles.set_hexpand(true);
    titles.set_valign(gtk4::Align::Center);
    titles.append(&title_label);
    titles.append(&counter_label);

    let action = |icon: &str, tooltip: &str| {
        let button = gtk4::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .valign(gtk4::Align::Center)
            .build();
        button.add_css_class("flat");
        button.add_css_class("viewer-action-btn");
        button
    };

    let info_toggle = gtk4::ToggleButton::builder()
        .icon_name("info-outline-symbolic")
        .tooltip_text("Details (i)")
        .valign(gtk4::Align::Center)
        .build();
    info_toggle.add_css_class("flat");
    info_toggle.add_css_class("viewer-action-btn");

    let download_btn = action("document-save-symbolic", "Save a copy…");
    let open_ext_btn = action("document-open-symbolic", "Open with another app");
    let close_btn = action("window-close-symbolic", "Close (Esc)");
    close_btn.add_css_class("viewer-close-btn");

    let top_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    top_bar.add_css_class("viewer-top-bar");
    top_bar.set_valign(gtk4::Align::Start);
    top_bar.set_hexpand(true);
    top_bar.append(&titles);
    top_bar.append(&info_toggle);
    top_bar.append(&download_btn);
    top_bar.append(&open_ext_btn);
    top_bar.append(&close_btn);
    overlay.add_overlay(&top_bar);

    // Details drawer: slides in from the right as a real surface (not a wash of
    // black over the photo), with its own header and its own way out.
    let info_rows = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    let info_map = gtk4::Button::builder()
        .label("Show on map")
        .icon_name("map-symbolic")
        .halign(gtk4::Align::Start)
        .build();
    info_map.add_css_class("pill");
    info_map.set_visible(false);

    let info_title = gtk4::Label::builder()
        .label("Details")
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .build();
    info_title.add_css_class("heading");

    let info_close = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Hide details")
        .build();
    info_close.add_css_class("flat");
    info_close.add_css_class("circular");

    let info_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    info_header.append(&info_title);
    info_header.append(&info_close);

    let info_body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    info_body.set_margin_top(16);
    info_body.set_margin_bottom(16);
    info_body.set_margin_start(16);
    info_body.set_margin_end(16);
    info_body.append(&info_header);
    info_body.append(&info_rows);
    info_body.append(&info_map);

    // Scrolled, because a photo with a full EXIF block plus a location can
    // outgrow a short window.
    let info_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&info_body)
        .build();

    let info_panel = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    info_panel.add_css_class("viewer-info-panel");
    info_panel.set_width_request(320);
    info_panel.append(&info_scroll);

    let info_revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideLeft)
        .halign(gtk4::Align::End)
        .valign(gtk4::Align::Fill)
        .child(&info_panel)
        .build();
    overlay.add_overlay(&info_revealer);

    let spinner = gtk4::Spinner::builder()
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .width_request(48)
        .height_request(48)
        .build();
    spinner.add_css_class("viewer-spinner");
    overlay.add_overlay(&spinner);

    let status_label = gtk4::Label::builder()
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .build();
    status_label.add_css_class("viewer-status");
    overlay.add_overlay(&status_label);

    window.set_child(Some(&overlay));

    let viewer = Rc::new(Viewer {
        picture: picture.clone(),
        spinner,
        status: status_label,
        title: title_label,
        counter: counter_label,
        prev: prev_btn.clone(),
        next: next_btn.clone(),
        info_toggle: info_toggle.clone(),
        info_revealer: info_revealer.clone(),
        info_rows,
        info_map: info_map.clone(),
        coords: RefCell::new(None),
        uid: RefCell::new(initial_uid.clone()),
        path: RefCell::new(None),
        loading: Cell::new(false),
    });

    let n = ui.gallery.model.n_items();
    let initial_idx = find_photo_index(&ui.gallery.model, &initial_uid).unwrap_or(0);
    let capture_time = ui
        .gallery.model
        .item(initial_idx)
        .and_downcast::<BoxedAnyObject>()
        .map_or(0, |boxed| boxed.borrow::<PhotoItem>().capture_time);
    show_photo_position(&viewer, initial_idx, n, capture_time);

    load_photo(ui, &viewer, initial_uid);
    prefetch_photo(ui, &viewer, 1);

    let w_close = window.clone();
    close_btn.connect_clicked(move |_| {
        w_close.close();
    });

    let viewer_info = viewer.clone();
    info_toggle.connect_toggled(move |toggle| {
        viewer_info
            .info_revealer
            .set_reveal_child(toggle.is_active());
    });

    let toggle_off = info_toggle.clone();
    info_close.connect_clicked(move |_| toggle_off.set_active(false));

    let viewer_map = viewer.clone();
    info_map.connect_clicked(move |_| {
        if let Some((lat, lon)) = *viewer_map.coords.borrow() {
            open_path(&format!(
                "https://www.openstreetmap.org/?mlat={lat:.6}&mlon={lon:.6}#map=16/{lat:.6}/{lon:.6}"
            ));
        }
    });

    let w_download = window.clone();
    let viewer_download = viewer.clone();
    download_btn.connect_clicked(move |_| {
        if let Some(path) = viewer_download.path.borrow().as_deref() {
            let name = format!("{}.jpg", viewer_download.uid.borrow());
            save_photo_to_disk(&w_download, path, &name);
        }
    });

    let viewer_ext = viewer.clone();
    open_ext_btn.connect_clicked(move |_| {
        if let Some(path) = viewer_ext.path.borrow().as_deref() {
            open_path(path);
        }
    });

    let ui_prev = ui.clone();
    let viewer_prev = viewer.clone();
    prev_btn.connect_clicked(move |_| {
        navigate_photo(&ui_prev, &viewer_prev, -1);
    });

    let ui_next = ui.clone();
    let viewer_next = viewer.clone();
    next_btn.connect_clicked(move |_| {
        navigate_photo(&ui_next, &viewer_next, 1);
    });

    // Click the backdrop — the dark area beside the photo — to dismiss, the way
    // every other lightbox behaves. Clicks on the photo itself are left alone, so
    // reaching for the image doesn't fling the window shut.
    let backdrop = gtk4::GestureClick::new();
    let viewer_click = viewer.clone();
    let w_click = window.clone();
    backdrop.connect_released(move |_, _, x, y| {
        if !over_photo(&viewer_click.picture, x, y) {
            w_click.close();
        }
    });
    picture.add_controller(backdrop);

    let key_controller = gtk4::EventControllerKey::new();
    let ui_key = ui.clone();
    let viewer_key = viewer.clone();
    let w_key = window.clone();
    key_controller.connect_key_pressed(move |_, key, _keycode, state| {
        let ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
        match key.name().as_deref() {
            Some("Left" | "Up" | "BackSpace") => navigate_photo(&ui_key, &viewer_key, -1),
            Some("Right" | "Down" | "space") => navigate_photo(&ui_key, &viewer_key, 1),
            Some("Home") => navigate_photo(&ui_key, &viewer_key, i32::MIN / 2),
            Some("End") => navigate_photo(&ui_key, &viewer_key, i32::MAX / 2),
            Some("i") => viewer_key
                .info_toggle
                .set_active(!viewer_key.info_toggle.is_active()),
            Some("f" | "F11") => {
                if w_key.is_fullscreen() {
                    w_key.unfullscreen();
                } else {
                    w_key.fullscreen();
                }
            }
            Some("Escape" | "q") => w_key.close(),
            Some("w") if ctrl => w_key.close(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(key_controller);

    window.present();
}

/// Whether `(x, y)` — in `picture`'s coordinates — lands on the photo itself
/// rather than the backdrop around it. [`gtk4::ContentFit::Contain`] centres the
/// image and letterboxes the rest, so the drawn rectangle is the widget scaled
/// down by whichever axis binds.
pub(crate) fn over_photo(picture: &gtk4::Picture, x: f64, y: f64) -> bool {
    let (width, height) = (f64::from(picture.width()), f64::from(picture.height()));
    let Some(paintable) = picture.paintable() else {
        return false;
    };
    let (iw, ih) = (
        f64::from(paintable.intrinsic_width()),
        f64::from(paintable.intrinsic_height()),
    );
    if iw <= 0.0 || ih <= 0.0 {
        return false;
    }

    let scale = (width / iw).min(height / ih);
    let (drawn_w, drawn_h) = (iw * scale, ih * scale);
    let (left, top) = ((width - drawn_w) / 2.0, (height - drawn_h) / 2.0);
    x >= left && x <= left + drawn_w && y >= top && y <= top + drawn_h
}
