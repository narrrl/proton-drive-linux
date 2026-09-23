use crate::*;

/// How long the pointer has to rest before the lightbox's controls fade out.
const CHROME_TIMEOUT: Duration = Duration::from_secs(2);

/// Factor one zoom step (a `+`/`-` key, a wheel notch) multiplies by.
const ZOOM_STEP: f64 = 1.25;

/// The furthest the lightbox zooms in, as a multiple of the image's own pixels.
const MAX_ZOOM: f64 = 8.0;

/// How close to the end of the loaded photos stepping forward starts paging the
/// next batch in, so reaching the end rarely means waiting for it.
const PAGE_AHEAD: u32 = 10;

/// The lightbox's mutable parts, shared by [`load_photo`], [`navigate_photo`] and
/// the button/key handlers so each one takes a single handle instead of a dozen
/// widget arguments.
pub(crate) struct Viewer {
    /// The lightbox itself, so a page that lands after it closed can tell.
    pub(crate) window: gtk4::Window,
    /// Swaps the still (`"photo"`, inside [`Self::scroller`]) for the player
    /// (`"video"`).
    pub(crate) media: gtk4::Stack,
    pub(crate) scroller: gtk4::ScrolledWindow,
    pub(crate) picture: gtk4::Picture,
    pub(crate) video: gtk4::Video,
    pub(crate) spinner: gtk4::Spinner,
    pub(crate) status: gtk4::Label,
    pub(crate) title: gtk4::Label,
    pub(crate) counter: gtk4::Label,
    pub(crate) prev: gtk4::Button,
    pub(crate) next: gtk4::Button,
    /// Details panel: the toggle that reveals it, the rows it fills in, and the
    /// "Show on map" button (hidden when the photo carries no GPS tags).
    pub(crate) info_toggle: gtk4::ToggleButton,
    pub(crate) info_revealer: gtk4::Revealer,
    pub(crate) info_rows: gtk4::Box,
    pub(crate) info_map: gtk4::Button,
    /// Coordinates behind `info_map`, once a photo with GPS tags is shown.
    pub(crate) coords: RefCell<Option<(f64, f64)>>,
    /// The favorite toggle in the top bar, and whether it is being set from
    /// the model rather than by the user — the same suppression the details pane
    /// uses for its pin switch, so painting a photo doesn't fire a round-trip.
    pub(crate) favorite: gtk4::ToggleButton,
    pub(crate) favorite_suppress: Cell<bool>,
    /// The button that switches between the files of one shot, and the files it
    /// switches between (the JPEG and the RAW of a photo, a live photo and its
    /// clip). Empty, and the button hidden, for a photo stored as one file.
    pub(crate) group_btn: gtk4::Button,
    pub(crate) group: RefCell<Vec<PhotoItem>>,
    /// uid of the photo currently on screen.
    pub(crate) uid: RefCell<String>,
    /// Drive name of the photo on screen, for Save a Copy and Open With.
    pub(crate) name: RefCell<Option<String>>,
    /// On-disk path of the full-size photo, once it has been downloaded.
    pub(crate) path: RefCell<Option<String>>,
    /// True while the full-size photo is still downloading and what's on screen is
    /// the upscaled thumbnail — a click-outside-to-close hit test has to size the
    /// image from the *thumbnail's* ratio in that window, but more importantly a
    /// late reply for a photo the user has already navigated away from must not
    /// overwrite the new one. Guarded by comparing against `uid`.
    pub(crate) loading: Cell<bool>,
    /// Zoom as a multiple of the image's own pixels, or `None` while the photo
    /// is fitted to the window.
    pub(crate) zoom: Cell<Option<f64>>,
    /// The zoom a pinch started from, and the scroll offsets a pan started from.
    pub(crate) pinch_start: Cell<f64>,
    pub(crate) drag_start: Cell<(f64, f64)>,
    /// Last pointer position over the image, which Ctrl+scroll zooms around.
    pub(crate) pointer: Cell<(f64, f64)>,
    /// The top bar and the prev/next buttons, which fade out once the pointer
    /// rests; whether the pointer is over one of them (which keeps them up);
    /// and the pending fade-out.
    pub(crate) chrome: Vec<gtk4::Revealer>,
    pub(crate) chrome_hover: Cell<bool>,
    pub(crate) chrome_source: RefCell<Option<glib::SourceId>>,
}

/// Camera/exposure/location facts pulled from a photo's own EXIF tags, as
/// label/value pairs for the details panel.
#[derive(Default)]
pub(crate) struct ExifInfo {
    /// `("Camera", "Apple iPhone 15")` and friends, the label already
    /// translated; empty when the file has no EXIF at all, which is normal for
    /// screenshots and re-encoded images.
    pub(crate) fields: Vec<(String, String)>,
    /// Decimal degrees, if the photo is geotagged.
    pub(crate) coords: Option<(f64, f64)>,
}

/// Paint the favorite toggle without firing its handler.
pub(crate) fn show_favorite(viewer: &Rc<Viewer>, favorite: bool) {
    viewer.favorite_suppress.set(true);
    viewer.favorite.set_active(favorite);
    set_favorite_icon(&viewer.favorite, favorite);
    viewer.favorite_suppress.set(false);
}

/// Filled star for a favorite, outline for the rest.
fn set_favorite_icon(button: &gtk4::ToggleButton, favorite: bool) {
    button.set_icon_name(if favorite {
        "starred-symbolic"
    } else {
        "non-starred-symbolic"
    });
}

/// Write a favorite change back into the loaded gallery page, so returning to
/// the grid (or reopening the photo) shows what was just set without a reload.
pub(crate) fn set_gallery_favorite(ui: &Rc<Ui>, uid: &str, favorite: bool) {
    let Some(idx) = find_photo_index(&ui.gallery.model, uid) else {
        return;
    };
    let Some(boxed) = ui.gallery.model.item(idx).and_downcast::<BoxedAnyObject>() else {
        return;
    };
    let mut item = boxed.borrow::<PhotoItem>().clone();
    item.favorite = favorite;
    ui.gallery.model.remove(idx);
    ui.gallery.model.insert(idx, &BoxedAnyObject::new(item));
}

/// Ask the daemon which files this shot was stored as, and offer a switch when
/// there is more than one. The grid shows such a shot as a single tile, so the
/// lightbox is the only place the RAW is reachable.
fn load_photo_group(ui: &Rc<Ui>, viewer: &Rc<Viewer>, uid: &str) {
    viewer.group.borrow_mut().clear();
    viewer.group_btn.set_visible(false);
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::PhotoGroup {
            uid: uid.to_string(),
        },
    );
    let viewer = viewer.clone();
    let uid = uid.to_string();
    glib::spawn_future_local(async move {
        let Ok(Ok(Response::Photos { items, .. })) = rx.recv().await else {
            return;
        };
        // A late reply for a photo the user has already left must not relabel
        // the button under the one now on screen.
        if *viewer.uid.borrow() != uid || items.len() < 2 {
            return;
        }
        if let Some(next) = next_group_member(&items, &uid) {
            viewer
                .group_btn
                .set_tooltip_text(Some(&member_tooltip(next)));
        }
        *viewer.group.borrow_mut() = items;
        viewer.group_btn.set_visible(true);
    });
}

/// The member after `uid`, wrapping round — the button steps through a shot's
/// files rather than toggling two of them, so a burst of three works too.
pub(crate) fn next_group_member<'a>(members: &'a [PhotoItem], uid: &str) -> Option<&'a PhotoItem> {
    let at = members.iter().position(|item| item.uid == uid)?;
    members.get((at + 1) % members.len())
}

/// What to call one file of a shot: its name, or its kind when the daemon has
/// not resolved a name for it.
pub(crate) fn member_label(item: &PhotoItem) -> String {
    match item.name.as_deref() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => match item.kind {
            PhotoKind::Raw => "the raw file".to_string(),
            PhotoKind::Video => "the video".to_string(),
            PhotoKind::Photo => "the photo".to_string(),
        },
    }
}

/// The group switch's tooltip, naming the file it will show next.
fn member_tooltip(item: &PhotoItem) -> String {
    match item.name.as_deref() {
        Some(name) if !name.is_empty() => {
            // Translators: tooltip of the button that switches to another file of the same photo; {name} is a file name.
            gettext_f("Show {name}", &[("name", &member_label(item))])
        }
        _ => match item.kind {
            PhotoKind::Raw => gettext("Show the raw file"),
            PhotoKind::Video => gettext("Show the video"),
            PhotoKind::Photo => gettext("Show the photo"),
        },
    }
}

/// The gallery's entry for `uid`, or the shot member it names when the group
/// switch has left the timeline's own photo (a RAW is not in the timeline).
fn viewer_item(ui: &Rc<Ui>, viewer: &Rc<Viewer>, uid: &str) -> Option<PhotoItem> {
    find_photo_index(&ui.gallery.model, uid)
        .and_then(|idx| ui.gallery.model.item(idx))
        .and_downcast::<BoxedAnyObject>()
        .map(|boxed| boxed.borrow::<PhotoItem>().clone())
        .or_else(|| {
            viewer
                .group
                .borrow()
                .iter()
                .find(|item| item.uid == uid)
                .cloned()
        })
}

/// Show the photo behind `uid`: paint its (already cached) thumbnail immediately
/// so the lightbox never opens on a blank screen, ask the daemon for the
/// full-size file, and swap it in — plus its EXIF — when it lands. A video
/// plays in place once its file is down.
pub(crate) fn load_photo(ui: &Rc<Ui>, viewer: &Rc<Viewer>, uid: String) {
    viewer.spinner.set_visible(true);
    viewer.spinner.start();
    viewer.status.set_visible(false);
    viewer.loading.set(true);
    *viewer.path.borrow_mut() = None;
    clear_info(viewer);
    stop_video(viewer);
    viewer.media.set_visible_child_name("photo");
    reset_zoom(viewer);

    // Looked up before the group is refetched, which empties it.
    let item = viewer_item(ui, viewer, &uid);
    let is_video = item
        .as_ref()
        .is_some_and(|item| item.kind == PhotoKind::Video);
    // The favorite state comes from the timeline page the gallery already has,
    // so the button is right on the first frame instead of after a round-trip.
    show_favorite(viewer, item.as_ref().is_some_and(|item| item.favorite));
    *viewer.name.borrow_mut() = item.and_then(|item| item.name);
    load_photo_group(ui, viewer, &uid);

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
            Ok(Ok(Response::FilePath { path })) if is_video => {
                viewer.video.set_filename(Some(&path));
                viewer.media.set_visible_child_name("video");
                *viewer.path.borrow_mut() = Some(path.clone());
                show_info(&viewer, &path, ExifInfo::default());
            }
            Ok(Ok(Response::FilePath { path })) => match gtk4::gdk::Texture::from_filename(&path) {
                Ok(texture) => {
                    // Zoomed in on the thumbnail: keep the photo the size it is
                    // on screen, now drawn from the full-size pixels.
                    let rescale = viewer
                        .zoom
                        .get()
                        .zip(intrinsic_size(&viewer.picture))
                        .map(|(zoom, (width, _))| zoom * width / f64::from(texture.width()));
                    viewer.picture.set_paintable(Some(&texture));
                    if let Some(zoom) = rescale {
                        viewer.zoom.set(Some(zoom));
                        zoom_to(&viewer, Some(zoom), None);
                    }
                    *viewer.path.borrow_mut() = Some(path.clone());
                    show_info(&viewer, &path, read_exif(&path));
                }
                Err(e) => {
                    tracing::error!("Failed to load texture for {path}: {e}");
                    fail(&gettext("Couldn't render this photo."));
                }
            },
            Ok(Ok(Response::Error { message, .. })) => fail(&message),
            Ok(Ok(_)) => fail(&gettext("Unexpected reply from the mount service.")),
            Ok(Err(_)) | Err(_) => fail(&gettext("Couldn't reach Proton Drive.")),
        }
    });
}

/// Stop whatever the player was playing, so stepping off a video (or closing
/// the lightbox) doesn't leave its sound running.
fn stop_video(viewer: &Rc<Viewer>) {
    viewer.video.set_media_stream(gtk4::MediaStream::NONE);
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
        .label(gettext("Reading photo details…"))
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

    let mut fields: Vec<(String, String)> = Vec::new();
    if let Ok(meta) = std::fs::metadata(path) {
        // Translators: label of a photo's file size in the details panel.
        fields.push((pgettext("property", "Size"), human_bytes(meta.len())));
    }
    fields.extend(info.fields);

    if fields.is_empty() {
        let label = gtk4::Label::builder()
            .label(gettext("This photo carries no metadata."))
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
                .title(label.as_str())
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
    let mut fields: Vec<(String, String)> = Vec::new();
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
    // Text tags, without the quotes `display_value` wraps each string in and
    // the NUL padding some cameras leave behind.
    let ascii = |tag: exif::Tag| {
        let field = reader.get_field(tag, exif::In::PRIMARY)?;
        let exif::Value::Ascii(parts) = &field.value else {
            return None;
        };
        let text = parts
            .iter()
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        (!text.is_empty()).then(|| text.to_string())
    };

    if let Some(size) = field(exif::Tag::PixelXDimension)
        .zip(field(exif::Tag::PixelYDimension))
        // Translators: a photo's pixel dimensions, such as "4032 × 3024".
        .map(|(w, h)| gettext_f("{width} × {height}", &[("width", &w), ("height", &h)]))
    {
        // Translators: label of a photo's pixel dimensions in the details panel.
        fields.push((gettext("Dimensions"), size));
    }
    if let Some(taken) = ascii(exif::Tag::DateTimeOriginal) {
        // Translators: label of when a photo was taken, in the details panel.
        fields.push((pgettext("photo property", "Taken"), taken));
    }

    if let Some(camera) = camera_name(ascii(exif::Tag::Make), ascii(exif::Tag::Model)) {
        // Translators: label of the camera model in a photo's details panel.
        fields.push((gettext("Camera"), camera));
    }
    if let Some(lens) = ascii(exif::Tag::LensModel) {
        // Translators: label of the lens model in a photo's details panel.
        fields.push((gettext("Lens"), lens));
    }

    let shutter = reader
        .get_field(exif::Tag::ExposureTime, exif::In::PRIMARY)
        .and_then(|f| match &f.value {
            exif::Value::Rational(r) => r.first().map(|r| r.to_f64()),
            _ => None,
        })
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(format_shutter);
    let iso = reader
        .get_field(exif::Tag::PhotographicSensitivity, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        // Translators: a photo's ISO sensitivity, such as "ISO 400".
        .map(|n| gettext_f("ISO {value}", &[("value", &n.to_string())]));
    let exposure: Vec<String> = [
        field(exif::Tag::FNumber),
        shutter,
        iso,
        field(exif::Tag::FocalLength),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !exposure.is_empty() {
        // Translators: label of aperture, shutter speed, ISO and focal length in a photo's details panel.
        fields.push((gettext("Exposure"), exposure.join(" · ")));
    }

    if let Some(lat) = gps_degrees(&reader, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef)
        && let Some(lon) = gps_degrees(&reader, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef)
    {
        let (lat_text, lon_text) = (format!("{lat:.5}"), format!("{lon:.5}"));
        // Translators: label of where a photo was taken, in the details panel.
        let label = pgettext("photo property", "Location");
        // Translators: GPS coordinates in decimal degrees, such as "52.52000, 13.40500".
        let value = gettext_f(
            "{latitude}, {longitude}",
            &[("latitude", &lat_text), ("longitude", &lon_text)],
        );
        fields.push((label, value));
        coords = Some((lat, lon));
    }

    ExifInfo { fields, coords }
}

/// "Google Pixel 6" from Make "Google" and Model "Pixel 6", but just "Canon EOS
/// R5" where the model already names the maker.
fn camera_name(make: Option<String>, model: Option<String>) -> Option<String> {
    match (make, model) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model)
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (make, model) => make.or(model),
    }
}

/// A shutter speed the way cameras print it: "1/250 s" below a second, "2.5 s"
/// from one up.
fn format_shutter(seconds: f64) -> String {
    if seconds >= 0.95 {
        let text = format!("{seconds:.1}");
        format!("{} s", text.strip_suffix(".0").unwrap_or(&text))
    } else {
        format!("1/{} s", (1.0 / seconds).round())
    }
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

/// The image's own size in pixels, once something is painted.
fn intrinsic_size(picture: &gtk4::Picture) -> Option<(f64, f64)> {
    let paintable = picture.paintable()?;
    let (width, height) = (
        f64::from(paintable.intrinsic_width()),
        f64::from(paintable.intrinsic_height()),
    );
    (width > 0.0 && height > 0.0).then_some((width, height))
}

/// The scale the photo is drawn at: its zoom, or the one that fits it to the
/// window.
fn current_scale(viewer: &Rc<Viewer>) -> Option<f64> {
    if let Some(zoom) = viewer.zoom.get() {
        return Some(zoom);
    }
    let (width, height) = intrinsic_size(&viewer.picture)?;
    let (view_w, view_h) = (
        f64::from(viewer.scroller.width()),
        f64::from(viewer.scroller.height()),
    );
    (view_w > 0.0 && view_h > 0.0).then(|| (view_w / width).min(view_h / height))
}

/// Fit the photo to the window again.
fn reset_zoom(viewer: &Rc<Viewer>) {
    viewer.zoom.set(None);
    viewer.picture.set_size_request(-1, -1);
    viewer.picture.set_cursor(None);
    viewer
        .scroller
        .set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Never);
}

/// Zoom to `scale` × the image's own pixels, keeping the point under `anchor`
/// (in the image area's coordinates; its centre when `None`) where it is.
/// `None`, or anything at or below the fitted size, fits the photo again.
fn zoom_to(viewer: &Rc<Viewer>, scale: Option<f64>, anchor: Option<(f64, f64)>) {
    let Some((width, height)) = intrinsic_size(&viewer.picture) else {
        return;
    };
    let (view_w, view_h) = (
        f64::from(viewer.scroller.width()),
        f64::from(viewer.scroller.height()),
    );
    if view_w <= 0.0 || view_h <= 0.0 {
        return;
    }
    let fit = (view_w / width).min(view_h / height);
    let Some(target) = scale
        .filter(|scale| *scale > fit * 1.01)
        .map(|scale| scale.min(MAX_ZOOM.max(fit)))
    else {
        reset_zoom(viewer);
        return;
    };

    let (anchor_x, anchor_y) = anchor.unwrap_or((view_w / 2.0, view_h / 2.0));
    let current = viewer.zoom.get().unwrap_or(fit);
    let (h, v) = (viewer.scroller.hadjustment(), viewer.scroller.vadjustment());
    // Where the anchor sits on the image, as a fraction of it — the same spot
    // has to be under the pointer after the resize. An image smaller than the
    // window on an axis is centred on it, hence the offset.
    let fraction = |value: f64, anchor: f64, drawn: f64, view: f64| {
        let offset = ((view - drawn) / 2.0).max(0.0);
        ((value + anchor - offset) / drawn).clamp(0.0, 1.0)
    };
    let fx = fraction(h.value(), anchor_x, width * current, view_w);
    let fy = fraction(v.value(), anchor_y, height * current, view_h);

    let (drawn_w, drawn_h) = (width * target, height * target);
    viewer.zoom.set(Some(target));
    viewer
        .scroller
        .set_policy(gtk4::PolicyType::External, gtk4::PolicyType::External);
    viewer
        .picture
        .set_size_request(drawn_w.round() as i32, drawn_h.round() as i32);
    viewer.picture.set_cursor_from_name(Some("grab"));

    // The scrolled window only learns the new size at the next layout, so the
    // adjustments are sized here to let the offset land now.
    let place = |adj: &gtk4::Adjustment, fraction: f64, anchor: f64, drawn: f64, view: f64| {
        let offset = ((view - drawn) / 2.0).max(0.0);
        let upper = drawn.max(view);
        let value = (fraction * drawn + offset - anchor).clamp(0.0, upper - view);
        adj.configure(value, 0.0, upper, view * 0.1, view * 0.9, view);
    };
    place(&h, fx, anchor_x, drawn_w, view_w);
    place(&v, fy, anchor_y, drawn_h, view_h);
}

/// Zoom in or out by `factor` around the centre.
fn zoom_by(viewer: &Rc<Viewer>, factor: f64) {
    if let Some(scale) = current_scale(viewer) {
        zoom_to(viewer, Some(scale * factor), None);
    }
}

/// Bring the top bar and the prev/next buttons back, and fade them out again
/// once the pointer has rested for [`CHROME_TIMEOUT`].
fn show_chrome(viewer: &Rc<Viewer>) {
    for revealer in &viewer.chrome {
        revealer.set_reveal_child(true);
        revealer.set_can_target(true);
    }
    viewer.media.set_cursor(None);
    if let Some(source) = viewer.chrome_source.borrow_mut().take() {
        source.remove();
    }
    let weak = Rc::downgrade(viewer);
    let source = glib::timeout_add_local_once(CHROME_TIMEOUT, move || {
        let Some(viewer) = weak.upgrade() else {
            return;
        };
        // This timer has fired, so its id must not be removed again.
        viewer.chrome_source.borrow_mut().take();
        if !viewer.chrome_hover.get() {
            hide_chrome(&viewer);
        }
    });
    *viewer.chrome_source.borrow_mut() = Some(source);
}

/// Fade the controls out. A hidden revealer still covers its corner of the
/// photo, so it stops taking clicks too.
fn hide_chrome(viewer: &Rc<Viewer>) {
    for revealer in &viewer.chrome {
        revealer.set_reveal_child(false);
        revealer.set_can_target(false);
    }
    viewer.media.set_cursor_from_name(Some("none"));
}

/// Copy the photo on screen to where the user picks, under its Drive name.
pub(crate) fn save_photo_to_disk(ui: &Rc<Ui>, window: &gtk4::Window, source: &str, name: &str) {
    let dialog = gtk4::FileDialog::builder()
        .title(gettext("Save a Copy"))
        .initial_name(name)
        .build();
    let source = source.to_string();
    let ui = ui.clone();
    dialog.save(Some(window), gio::Cancellable::NONE, move |res| {
        // An error here is the dialog being dismissed.
        let Some(dest) = res.ok().and_then(|file| file.path()) else {
            return;
        };
        // A video can run to gigabytes; copy it off the GTK thread.
        glib::spawn_future_local(async move {
            let copied =
                gio::spawn_blocking(move || std::fs::copy(&source, &dest).map(|_| dest)).await;
            match copied {
                Ok(Ok(dest)) => {
                    let saved = dest.file_name().map_or_else(
                        || dest.display().to_string(),
                        |name| name.to_string_lossy().into_owned(),
                    );
                    // Translators: {name} is the saved file's name.
                    toast(&ui, &gettext_f("Saved “{name}”", &[("name", &saved)]));
                }
                Ok(Err(e)) => toast_error(&ui, &gettext("Couldn't save the copy"), &e.to_string()),
                Err(_) => toast_error(
                    &ui,
                    &gettext("Couldn't save the copy"),
                    &gettext("The copy was interrupted."),
                ),
            }
        });
    });
}

/// A symlink to the cached file `path` under its Drive `name`, in a directory
/// of its own below `base`. The cache names files by content hash, with no
/// extension; the app chooser and the app it launches both need the real name
/// to know what they are looking at.
pub(crate) fn named_link(base: &Path, path: &str, name: &str) -> Option<PathBuf> {
    let name = Path::new(name).file_name()?;
    let dir = base.join(Path::new(path).file_name()?);
    std::fs::create_dir_all(&dir).ok()?;
    let link = dir.join(name);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(path, &link).ok()?;
    Some(link)
}

/// Offer every app that can open the photo on screen, rather than launching
/// the default one.
fn open_with(window: &gtk4::Window, path: &str, name: Option<&str>) {
    let base = glib::user_runtime_dir().join("pdfs-open");
    let target = name
        .and_then(|name| named_link(&base, path, name))
        .unwrap_or_else(|| PathBuf::from(path));
    let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(target)));
    launcher.set_always_ask(true);
    launcher.launch(Some(window), gio::Cancellable::NONE, |res| {
        if let Err(e) = res {
            tracing::warn!("open with failed: {e}");
        }
    });
}

/// Step `delta` photos through the flat timeline model and load what lands
/// there. Stepping past the last loaded photo pages the next batch in first.
pub(crate) fn navigate_photo(ui: &Rc<Ui>, viewer: &Rc<Viewer>, delta: i32) {
    let model = &ui.gallery.model;
    let n = model.n_items();
    if n == 0 {
        return;
    }
    let current = find_photo_index(model, &viewer.uid.borrow()).unwrap_or(0);
    if delta == 1 && current + 1 >= n {
        if ui.gallery.has_more.get() {
            wait_for_page(ui, viewer, n);
        }
        return;
    }
    // Prev/next are ±1; Home/End are huge deltas that clamp to the ends.
    let index = (i64::from(current) + i64::from(delta)).clamp(0, i64::from(n) - 1) as u32;
    if index == current {
        return;
    }
    let Some(photo) = model
        .item(index)
        .and_downcast::<BoxedAnyObject>()
        .map(|boxed| boxed.borrow::<PhotoItem>().clone())
    else {
        return;
    };

    *viewer.uid.borrow_mut() = photo.uid.clone();
    show_photo_position(ui, viewer, index, photo.capture_time);
    load_photo(ui, viewer, photo.uid);
    // Keep walking in the same direction: the next one is likely where they're
    // headed, so have it in the cache before they ask.
    prefetch_photo(ui, viewer, delta.signum());
    if delta > 0 && index + PAGE_AHEAD >= n && ui.gallery.has_more.get() {
        load_gallery(ui, true);
    }
}

/// Load the next timeline page and step onto it once it lands — unless the
/// lightbox closed or moved on in the meantime.
fn wait_for_page(ui: &Rc<Ui>, viewer: &Rc<Viewer>, loaded: u32) {
    viewer.next.set_sensitive(false);
    let (ui_page, viewer_page) = (ui.clone(), viewer.clone());
    ui.gallery.page_waiters.borrow_mut().push(Box::new(move || {
        if !viewer_page.window.is_visible() {
            return;
        }
        let index = find_photo_index(&ui_page.gallery.model, &viewer_page.uid.borrow());
        if index == Some(loaded - 1) && ui_page.gallery.model.n_items() > loaded {
            navigate_photo(&ui_page, &viewer_page, 1);
        } else if let Some(index) = index {
            let capture_time = viewer_item(&ui_page, &viewer_page, &viewer_page.uid.borrow())
                .map_or(0, |item| item.capture_time);
            show_photo_position(&ui_page, &viewer_page, index, capture_time);
        }
    }));
    load_gallery(ui, true);
}

/// How many photos the lightbox is stepping through: an album's own count, or
/// the library's for the active kind. `None` when a date or favorites filter
/// narrows the timeline to a count the daemon does not report.
fn gallery_total(ui: &Rc<Ui>) -> Option<usize> {
    if let Some(album) = ui.gallery.album.borrow().as_ref() {
        return Some(album.photo_count);
    }
    if ui.gallery.range.get().is_some() || ui.gallery.favorites.get() {
        return None;
    }
    let (photos, videos, raw) = ui.gallery.counts.get()?;
    Some(match ui.gallery.kind.get() {
        Some(PhotoKind::Photo) => photos,
        Some(PhotoKind::Video) => videos,
        Some(PhotoKind::Raw) => raw,
        None => photos + videos + raw,
    })
}

/// "12 of 340" for the photo at `index`. While more pages are left the total
/// is the library's (`total`), or the loaded count with a "+" when that is
/// unknown; once everything is loaded the loaded count is exact.
pub(crate) fn position_label(index: u32, loaded: u32, more: bool, total: Option<usize>) -> String {
    let (at, loaded) = (index as usize + 1, loaded as usize);
    match total {
        // Translators: the lightbox counter, such as "12 of 340"; {position} is the photo's place, {total} how many there are.
        _ if !more => gettext_f(
            "{position} of {total}",
            &[("position", &thousands(at)), ("total", &thousands(loaded))],
        ),
        // Translators: the lightbox counter, such as "12 of 340"; {position} is the photo's place, {total} how many there are.
        Some(total) if total > loaded => gettext_f(
            "{position} of {total}",
            &[("position", &thousands(at)), ("total", &thousands(total))],
        ),
        // Translators: the lightbox counter while more photos are still loading, such as "12 of 200+"; {loaded} is how many are loaded so far.
        _ => gettext_f(
            "{position} of {loaded}+",
            &[("position", &thousands(at)), ("loaded", &thousands(loaded))],
        ),
    }
}

/// Set the lightbox's title (the photo's date) and its "12 of 340" counter.
fn show_photo_position(ui: &Rc<Ui>, viewer: &Rc<Viewer>, index: u32, capture_time: i64) {
    let loaded = ui.gallery.model.n_items();
    let more = ui.gallery.has_more.get();
    viewer.prev.set_sensitive(index > 0);
    viewer.next.set_sensitive(index + 1 < loaded || more);
    viewer.title.set_label(&format_capture_time(capture_time));
    viewer
        .counter
        .set_label(&position_label(index, loaded, more, gallery_total(ui)));
}

/// The in-app lightbox: the photo or video, edge-to-edge on a dark backdrop,
/// with a top bar and prev/next buttons that fade out while the pointer rests,
/// and a details panel docked beside the image.
///
/// Closing it is deliberately hard to get wrong — Escape, `q`, Ctrl+W, the close
/// button, or a click on the backdrop beside the photo all dismiss it.
pub(crate) fn open_photo_viewer(ui: &Rc<Ui>, initial_uid: String) {
    let parent = ui.stack.root().and_downcast::<gtk4::Window>().unwrap();

    let window = gtk4::Window::builder()
        .title(gettext("Photo"))
        .modal(true)
        .transient_for(&parent)
        .default_width(1100)
        .default_height(760)
        .build();
    window.add_css_class("photo-viewer-window");

    let overlay = gtk4::Overlay::new();
    overlay.set_hexpand(true);

    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .build();
    // Fitted, the photo is exactly the window; zoomed, it outgrows it and the
    // scrolled window pans over it (with its scrollbars kept out of sight).
    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .hexpand(true)
        .vexpand(true)
        .child(&picture)
        .build();
    let video = gtk4::Video::builder()
        .autoplay(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    let media = gtk4::Stack::new();
    media.add_named(&scroller, Some("photo"));
    media.add_named(&video, Some("video"));
    overlay.set_child(Some(&media));

    let fade = |child: &gtk4::Widget, halign: gtk4::Align, valign: gtk4::Align| {
        gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::Crossfade)
            .reveal_child(true)
            .halign(halign)
            .valign(valign)
            .child(child)
            .build()
    };

    let prev_btn = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text(gettext("Previous (←)"))
        .build();
    prev_btn.add_css_class("circular");
    prev_btn.add_css_class("flat");
    prev_btn.add_css_class("viewer-nav-btn");
    let prev_reveal = fade(
        prev_btn.upcast_ref(),
        gtk4::Align::Start,
        gtk4::Align::Center,
    );
    overlay.add_overlay(&prev_reveal);

    let next_btn = gtk4::Button::builder()
        .icon_name("go-next-symbolic")
        .tooltip_text(gettext("Next (→)"))
        .build();
    next_btn.add_css_class("circular");
    next_btn.add_css_class("flat");
    next_btn.add_css_class("viewer-nav-btn");
    let next_reveal = fade(next_btn.upcast_ref(), gtk4::Align::End, gtk4::Align::Center);
    overlay.add_overlay(&next_reveal);

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
        .tooltip_text(gettext("Details (i)"))
        .valign(gtk4::Align::Center)
        .build();
    info_toggle.add_css_class("flat");
    info_toggle.add_css_class("viewer-action-btn");

    let favorite_btn = gtk4::ToggleButton::builder()
        .icon_name("non-starred-symbolic")
        // Translators: tooltip of the star button that marks a photo as a favorite.
        .tooltip_text(pgettext("verb", "Favorite"))
        .valign(gtk4::Align::Center)
        .build();
    favorite_btn.add_css_class("flat");
    favorite_btn.add_css_class("viewer-action-btn");

    // Only shown for a shot stored as more than one file; `load_photo_group`
    // decides that per photo.
    let group_btn = action("view-paged-symbolic", &gettext("Other files of this photo"));
    group_btn.set_visible(false);

    let download_btn = action("document-save-symbolic", &gettext("Save a Copy…"));
    let delete_btn = action("user-trash-symbolic", &gettext("Move to Trash (Delete)"));
    let open_ext_btn = action("document-open-symbolic", &gettext("Open With…"));
    let close_btn = action("window-close-symbolic", &gettext("Close (Esc)"));
    close_btn.add_css_class("viewer-close-btn");

    let top_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    top_bar.add_css_class("viewer-top-bar");
    top_bar.append(&titles);
    top_bar.append(&group_btn);
    top_bar.append(&favorite_btn);
    top_bar.append(&info_toggle);
    top_bar.append(&download_btn);
    top_bar.append(&delete_btn);
    top_bar.append(&open_ext_btn);
    top_bar.append(&close_btn);
    let top_reveal = fade(top_bar.upcast_ref(), gtk4::Align::Fill, gtk4::Align::Start);
    overlay.add_overlay(&top_reveal);

    // Details panel: docked beside the image as a real surface, so opening it
    // narrows the photo instead of covering part of it.
    let info_rows = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    let info_map = gtk4::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .label(gettext("Show on map"))
                .icon_name("map-symbolic")
                .build(),
        )
        .halign(gtk4::Align::Start)
        .build();
    info_map.add_css_class("pill");
    info_map.set_visible(false);

    let info_title = gtk4::Label::builder()
        .label(gettext("Details"))
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .build();
    info_title.add_css_class("heading");

    let info_close = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text(gettext("Hide details"))
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
        .vexpand(true)
        .child(&info_body)
        .build();

    let info_panel = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    info_panel.add_css_class("viewer-info-panel");
    info_panel.set_width_request(320);
    info_panel.append(&info_scroll);

    // Set explicitly: the title's hexpand would otherwise propagate up and have
    // the revealer take half the window even while it is closed.
    let info_revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideLeft)
        .hexpand(false)
        .child(&info_panel)
        .build();

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

    let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    body.append(&overlay);
    body.append(&info_revealer);
    window.set_child(Some(&body));

    let viewer = Rc::new(Viewer {
        window: window.clone(),
        media: media.clone(),
        scroller: scroller.clone(),
        picture: picture.clone(),
        video,
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
        favorite: favorite_btn.clone(),
        favorite_suppress: Cell::new(false),
        group_btn: group_btn.clone(),
        group: RefCell::new(Vec::new()),
        coords: RefCell::new(None),
        uid: RefCell::new(initial_uid.clone()),
        name: RefCell::new(None),
        path: RefCell::new(None),
        loading: Cell::new(false),
        zoom: Cell::new(None),
        pinch_start: Cell::new(1.0),
        drag_start: Cell::new((0.0, 0.0)),
        pointer: Cell::new((0.0, 0.0)),
        chrome: vec![top_reveal.clone(), prev_reveal.clone(), next_reveal.clone()],
        chrome_hover: Cell::new(false),
        chrome_source: RefCell::new(None),
    });

    let initial_idx = find_photo_index(&ui.gallery.model, &initial_uid).unwrap_or(0);
    let capture_time = viewer_item(ui, &viewer, &initial_uid).map_or(0, |item| item.capture_time);
    show_photo_position(ui, &viewer, initial_idx, capture_time);

    load_photo(ui, &viewer, initial_uid);
    prefetch_photo(ui, &viewer, 1);
    show_chrome(&viewer);

    // Any real pointer movement brings the controls back; resting over one of
    // them keeps them up.
    let motion = gtk4::EventControllerMotion::new();
    let viewer_motion = viewer.clone();
    motion.connect_motion(move |_, x, y| {
        let (last_x, last_y) = viewer_motion.pointer.get();
        viewer_motion.pointer.set((x, y));
        if (x - last_x).abs() > 1.0 || (y - last_y).abs() > 1.0 {
            show_chrome(&viewer_motion);
        }
    });
    body.add_controller(motion);
    for revealer in [&top_reveal, &prev_reveal, &next_reveal] {
        let hover = gtk4::EventControllerMotion::new();
        let viewer_enter = viewer.clone();
        hover.connect_enter(move |_, _, _| viewer_enter.chrome_hover.set(true));
        let viewer_leave = viewer.clone();
        hover.connect_leave(move |_| {
            viewer_leave.chrome_hover.set(false);
            show_chrome(&viewer_leave);
        });
        revealer.add_controller(hover);
    }

    // Ctrl+scroll zooms around the pointer; a plain scroll pans a zoomed photo.
    let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
    wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let viewer_wheel = viewer.clone();
    wheel.connect_scroll(move |controller, _, dy| {
        if !controller
            .current_event_state()
            .contains(gtk4::gdk::ModifierType::CONTROL_MASK)
        {
            return glib::Propagation::Proceed;
        }
        if let Some(scale) = current_scale(&viewer_wheel) {
            let anchor = viewer_wheel.pointer.get();
            zoom_to(
                &viewer_wheel,
                Some(scale * ZOOM_STEP.powf(-dy)),
                Some(anchor),
            );
        }
        glib::Propagation::Stop
    });
    scroller.add_controller(wheel);

    let pinch = gtk4::GestureZoom::new();
    let viewer_pinch = viewer.clone();
    pinch.connect_begin(move |_, _| {
        viewer_pinch
            .pinch_start
            .set(current_scale(&viewer_pinch).unwrap_or(1.0));
    });
    let viewer_pinch = viewer.clone();
    pinch.connect_scale_changed(move |gesture, delta| {
        let start = viewer_pinch.pinch_start.get();
        zoom_to(
            &viewer_pinch,
            Some(start * delta),
            gesture.bounding_box_center(),
        );
    });
    scroller.add_controller(pinch);

    // Dragging a zoomed photo pans it.
    let drag = gtk4::GestureDrag::new();
    let viewer_drag = viewer.clone();
    drag.connect_drag_begin(move |_, _, _| {
        let scroller = &viewer_drag.scroller;
        viewer_drag.drag_start.set((
            scroller.hadjustment().value(),
            scroller.vadjustment().value(),
        ));
        if viewer_drag.zoom.get().is_some() {
            viewer_drag.picture.set_cursor_from_name(Some("grabbing"));
        }
    });
    let viewer_drag = viewer.clone();
    drag.connect_drag_update(move |_, dx, dy| {
        if viewer_drag.zoom.get().is_none() {
            return;
        }
        let (x, y) = viewer_drag.drag_start.get();
        viewer_drag.scroller.hadjustment().set_value(x - dx);
        viewer_drag.scroller.vadjustment().set_value(y - dy);
    });
    let viewer_drag = viewer.clone();
    drag.connect_drag_end(move |_, _, _| {
        if viewer_drag.zoom.get().is_some() {
            viewer_drag.picture.set_cursor_from_name(Some("grab"));
        }
    });
    scroller.add_controller(drag);

    let w_close = window.clone();
    close_btn.connect_clicked(move |_| {
        w_close.close();
    });

    let viewer_closing = viewer.clone();
    window.connect_close_request(move |_| {
        stop_video(&viewer_closing);
        if let Some(source) = viewer_closing.chrome_source.borrow_mut().take() {
            source.remove();
        }
        glib::Propagation::Proceed
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

    let ui_fav = ui.clone();
    let viewer_fav = viewer.clone();
    favorite_btn.connect_toggled(move |btn| {
        if viewer_fav.favorite_suppress.get() {
            return;
        }
        let favorite = btn.is_active();
        set_favorite_icon(btn, favorite);
        let uid = viewer_fav.uid.borrow().clone();
        let rx = spawn_request(
            ui_fav.dirs.control_socket(),
            Request::SetPhotoFavorite {
                uid: uid.clone(),
                favorite,
            },
        );
        let ui_result = ui_fav.clone();
        let viewer_result = viewer_fav.clone();
        glib::spawn_future_local(async move {
            let failed = match rx.recv().await {
                Ok(Ok(Response::Ok { .. })) => {
                    set_gallery_favorite(&ui_result, &uid, favorite);
                    None
                }
                Ok(Ok(Response::Error { message, .. })) => Some(message),
                _ => Some(gettext("The mount service didn't respond.")),
            };
            if let Some(detail) = failed {
                toast_error(
                    &ui_result,
                    &gettext("Couldn't change the favorite"),
                    &detail,
                );
                // The server refused, so the button must go back to describing
                // what is actually stored — without firing this handler again.
                if *viewer_result.uid.borrow() == uid {
                    show_favorite(&viewer_result, !favorite);
                }
            }
        });
    });

    // Deleting the photo on screen closes the lightbox: what it was showing is
    // gone, and leaving it open on a trashed photo invites a second Delete on
    // something the user cannot see.
    let ui_delete = ui.clone();
    let viewer_delete = viewer.clone();
    let w_delete = window.clone();
    delete_btn.connect_clicked(move |_| {
        let uid = viewer_delete.uid.borrow().clone();
        w_delete.close();
        trash_photos(&ui_delete, vec![uid]);
    });

    let ui_group = ui.clone();
    let viewer_group = viewer.clone();
    group_btn.connect_clicked(move |_| {
        let uid = viewer_group.uid.borrow().clone();
        let next =
            next_group_member(&viewer_group.group.borrow(), &uid).map(|item| item.uid.clone());
        if let Some(next) = next {
            *viewer_group.uid.borrow_mut() = next.clone();
            load_photo(&ui_group, &viewer_group, next);
        }
    });

    let ui_download = ui.clone();
    let w_download = window.clone();
    let viewer_download = viewer.clone();
    download_btn.connect_clicked(move |_| {
        if let Some(path) = viewer_download.path.borrow().as_deref() {
            let name = viewer_download.name.borrow();
            save_photo_to_disk(
                &ui_download,
                &w_download,
                path,
                name.as_deref().unwrap_or("photo"),
            );
        }
    });

    let w_ext = window.clone();
    let viewer_ext = viewer.clone();
    open_ext_btn.connect_clicked(move |_| {
        if let Some(path) = viewer_ext.path.borrow().as_deref() {
            open_with(&w_ext, path, viewer_ext.name.borrow().as_deref());
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
    // reaching for the image doesn't fling the window shut; a zoomed photo has
    // no backdrop to speak of. Double-click toggles between fitted and 100 %.
    let backdrop = gtk4::GestureClick::new();
    let viewer_click = viewer.clone();
    backdrop.connect_pressed(move |_, n_press, x, y| {
        if n_press != 2 {
            return;
        }
        if viewer_click.zoom.get().is_some() {
            reset_zoom(&viewer_click);
        } else if let Some(fit) = current_scale(&viewer_click) {
            let scroller = &viewer_click.scroller;
            let anchor = (
                x - scroller.hadjustment().value(),
                y - scroller.vadjustment().value(),
            );
            // A photo smaller than the window is already past 100 % fitted, so
            // double-clicking it doubles it instead.
            zoom_to(&viewer_click, Some((fit * 2.0).max(1.0)), Some(anchor));
        }
    });
    let viewer_click = viewer.clone();
    let w_click = window.clone();
    backdrop.connect_released(move |_, n_press, x, y| {
        if n_press == 1
            && viewer_click.zoom.get().is_none()
            && !over_photo(&viewer_click.picture, x, y)
        {
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
        let playing_video = viewer_key.media.visible_child_name().as_deref() == Some("video");
        match key.name().as_deref() {
            Some("space") if playing_video => {
                if let Some(stream) = viewer_key.video.media_stream() {
                    stream.set_playing(!stream.is_playing());
                }
            }
            Some("Left" | "Up" | "BackSpace") => navigate_photo(&ui_key, &viewer_key, -1),
            Some("Right" | "Down" | "space") => navigate_photo(&ui_key, &viewer_key, 1),
            Some("Home") => navigate_photo(&ui_key, &viewer_key, i32::MIN / 2),
            Some("End") => navigate_photo(&ui_key, &viewer_key, i32::MAX / 2),
            Some("plus" | "equal" | "KP_Add") => zoom_by(&viewer_key, ZOOM_STEP),
            Some("minus" | "KP_Subtract") => zoom_by(&viewer_key, 1.0 / ZOOM_STEP),
            Some("0" | "KP_0") => reset_zoom(&viewer_key),
            Some("1" | "KP_1") => zoom_to(&viewer_key, Some(1.0), None),
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
            Some("Delete" | "KP_Delete") => {
                let uid = viewer_key.uid.borrow().clone();
                w_key.close();
                trash_photos(&ui_key, vec![uid]);
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

#[cfg(test)]
mod tests {
    use super::{camera_name, format_shutter, named_link, position_label};

    #[test]
    fn shutter_speeds_read_like_a_camera() {
        assert_eq!(format_shutter(1.0 / 18.869_704_689_121_615), "1/19 s");
        assert_eq!(format_shutter(0.001), "1/1000 s");
        assert_eq!(format_shutter(2.5), "2.5 s");
        assert_eq!(format_shutter(1.0), "1 s");
    }

    #[test]
    fn camera_name_does_not_repeat_the_maker() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(camera_name(s("Google"), s("Pixel 6")), s("Google Pixel 6"));
        assert_eq!(
            camera_name(s("Canon"), s("Canon EOS R5")),
            s("Canon EOS R5")
        );
        assert_eq!(camera_name(None, s("iPhone 15")), s("iPhone 15"));
        assert_eq!(camera_name(None, None), None);
    }

    #[test]
    fn the_counter_counts_the_library_while_pages_are_left() {
        assert_eq!(position_label(11, 200, true, Some(1422)), "12 of 1,422");
        assert_eq!(position_label(11, 200, true, None), "12 of 200+");
        assert_eq!(position_label(0, 37, false, Some(1422)), "1 of 37");
    }

    #[test]
    fn open_with_sees_the_drive_name() {
        let base = std::env::temp_dir().join(format!("pdfs-named-link-{}", std::process::id()));
        let cached = base.join("0a1b2c");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&cached, b"jpeg").unwrap();

        let links = base.join("links");
        let link = named_link(&links, cached.to_str().unwrap(), "IMG_0001.JPG").unwrap();
        assert_eq!(link.file_name().unwrap(), "IMG_0001.JPG");
        assert_eq!(std::fs::read(&link).unwrap(), b"jpeg");
        // Opening the same photo again replaces the link rather than failing.
        assert!(named_link(&links, cached.to_str().unwrap(), "IMG_0001.JPG").is_some());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
