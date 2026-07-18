use crate::*;

pub(crate) struct GalleryState {
    // Photos (gallery) page.
    /// Every photo loaded so far, newest first — the order the lightbox's
    /// prev/next walks. The visible day sections are derived from this.
    pub(crate) model: gio::ListStore,
    /// The day sections rendered by the Photos ListView, rebuilt from
    /// [`Self::model`] by [`repaint_gallery`].
    pub(crate) groups: gio::ListStore,
    /// Target row height in px, retuned by Ctrl+scroll / Ctrl+± (see
    /// [`zoom_gallery`]). Each tile keeps its own aspect ratio; rows are
    /// justified to the content width around this height.
    pub(crate) tile: Cell<i32>,
    /// Swaps the Photos content area between the timeline and its status page.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) more: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    /// "1,204 photos" under the page title.
    pub(crate) subtitle: gtk4::Label,
    /// Which tab (Photos / Videos / Raw) the timeline is filtered to, or `None`
    /// for All. Read by [`load_gallery`] and set by the filter toggles.
    pub(crate) kind: Cell<Option<PhotoKind>>,
    /// The filter toggles, index-aligned with [`kind_for_tab`], kept so their
    /// labels can carry live per-kind counts.
    pub(crate) tabs: [gtk4::ToggleButton; 4],
    /// The date-jump dropdown ("All dates" then a month per timeline entry), and
    /// the `[from, to)` window each of its rows selects (index-aligned; `None` is
    /// "All dates"). Selecting a row loads that month via [`load_gallery`].
    pub(crate) dates: gtk4::DropDown,
    pub(crate) date_ranges: RefCell<Vec<Option<(i64, i64)>>>,
    /// The capture-time window the timeline is currently filtered to, or `None`
    /// for the whole span. Read by [`load_gallery`], set by the date dropdown.
    pub(crate) range: Cell<Option<(i64, i64)>>,
    /// Set while the date dropdown is being repopulated, so resetting its model
    /// doesn't fire the selection handler and kick off a spurious reload.
    pub(crate) date_suppress: Cell<bool>,
    /// True while a timeline page is in flight, so the scroll-to-the-end paging
    /// can't fire a second request for the page already coming.
    pub(crate) loading: Cell<bool>,
    /// Content width the sections are currently justified to. Updated when the
    /// ListView is resized, which re-justifies the visible sections.
    pub(crate) width: Cell<i32>,
    /// Decoded thumbnails by photo uid, with the insertion order that evicts the
    /// oldest past [`TEXTURE_CACHE_MAX`]. Scrolling back over a day therefore
    /// repaints from memory instead of re-decoding from disk.
    pub(crate) photo_tex: RefCell<HashMap<String, gtk4::gdk::Texture>>,
    pub(crate) photo_tex_order: RefCell<VecDeque<String>>,
    /// Aspect ratio (w/h) per uid, learned when a thumbnail is decoded and
    /// persisted to disk, so a relaunch justifies its rows correctly on the first
    /// frame instead of reflowing as thumbnails arrive.
    pub(crate) photo_ratio: RefCell<HashMap<String, f64>>,
    /// Ratios learned since the last save, so the ratio file is only rewritten
    /// when it actually changed.
    pub(crate) photo_ratio_dirty: Cell<bool>,
    /// Photos the daemon reported as having no thumbnail at all, so a tile that
    /// can never be filled isn't requested again on every scroll past it.
    pub(crate) photo_nothumb: RefCell<HashSet<String>>,
    /// Tiles on screen still waiting for their thumbnail, by uid. Populated as
    /// sections are bound, drained as batches land, cleared on unbind — so a
    /// batch only ever paints a widget that is still showing that photo.
    pub(crate) thumb_wanted: RefCell<HashMap<String, gtk4::Picture>>,
    /// Uids queued for the next [`Request::PhotoThumbs`] batch, and whether a
    /// batch is already in flight (only one at a time, so a long scroll can't
    /// stack requests on the daemon).
    pub(crate) thumb_queue: RefCell<VecDeque<String>>,
    pub(crate) thumb_inflight: Cell<bool>,
    /// Thumbnails on disk waiting to be turned into textures, as `(uid, path)`.
    /// Decoding happens on the GTK thread (textures are not `Send`), so it is fed
    /// a few at a time from an idle callback rather than in one blocking burst
    /// that would stutter the scroll. [`Self::decode_idle`] is the "callback
    /// already scheduled" guard.
    pub(crate) decode_queue: RefCell<VecDeque<(String, String)>>,
    pub(crate) decode_idle: Cell<bool>,
    /// Pending debounce timers for the thumbnail queue flush and the section
    /// re-justify, replaced on each new trigger so only the last one fires.
    pub(crate) thumb_source: RefCell<Option<glib::SourceId>>,
    pub(crate) relayout_source: RefCell<Option<glib::SourceId>>,
    /// The day sections currently realised by the ListView, by their index in
    /// [`Self::groups`]. A learned ratio or a resize re-justifies these
    /// in place — rebuilding the ListStore instead would reset the scroll
    /// position out from under the user.
    pub(crate) bound: RefCell<HashMap<u32, gtk4::Box>>,
}

/// How many photos to pull per [`Request::PhotosTimeline`] page.
pub(crate) const PHOTOS_PAGE: usize = 60;

/// Gallery row height in px: the zoom range, its default, and the step one
/// Ctrl+scroll notch (or Ctrl+±) moves it by. Rows are justified to the full
/// content width, so this is the *target* height a row lands near, not an exact
/// tile size (see [`justify_rows`]).
pub(crate) const TILE_MIN: i32 = 90;

pub(crate) const TILE_MAX: i32 = 340;

pub(crate) const TILE_DEFAULT: i32 = 220;

pub(crate) const TILE_STEP: i32 = 30;

/// Gap between tiles, horizontally and vertically.
pub(crate) const TILE_GAP: i32 = 6;

/// Aspect ratio (w/h) assumed for a photo whose thumbnail hasn't been decoded
/// yet, so a tile can be laid out before its image exists. Landscape 3:2, the
/// commonest camera/phone ratio; once the thumbnail lands the real ratio
/// replaces it and the section re-justifies in place.
pub(crate) const RATIO_UNKNOWN: f64 = 1.5;

/// Aspect ratios are clamped to this range before a row is packed: one absurd
/// panorama, or a sliver of a portrait, must not be able to squash the row it
/// lands in down to nothing. The tile still shows the whole image.
pub(crate) const RATIO_MIN: f64 = 0.4;

pub(crate) const RATIO_MAX: f64 = 3.5;

/// How many thumbnails one on-demand [`Request::PhotoThumbs`] batch asks for.
/// Small, so the first tiles on screen fill in quickly rather than the whole
/// page landing at once.
pub(crate) const THUMB_BATCH: usize = 16;

/// Idle pause before a thumbnail batch is sent, so a fast scroll coalesces into
/// one request per settle instead of one per row that flickers past.
pub(crate) const THUMB_DEBOUNCE: Duration = Duration::from_millis(60);

/// How long to wait before asking again for a thumbnail the daemon is generating
/// itself. That means downloading the photo's full file, so the wait is measured
/// in seconds, not milliseconds.
pub(crate) const THUMB_RETRY: Duration = Duration::from_secs(4);

/// Decoded thumbnails held in memory. Each is a few hundred KiB of GPU texture;
/// this caps the gallery's footprint while covering several screens of scroll.
pub(crate) const TEXTURE_CACHE_MAX: usize = 600;

/// Pause after a resize/zoom before the visible sections are re-justified.
pub(crate) const RELAYOUT_DEBOUNCE: Duration = Duration::from_millis(80);

/// One day-section of the photos timeline: a heading plus the photos captured
/// that day, in timeline order. Built from the flat [`Ui::gallery_model`] by
/// [`group_photos`] and rendered as one [`gtk4::ListView`] row.
pub(crate) struct PhotoGroup {
    /// "Today", "Yesterday", or e.g. "3 June 2026".
    pub(crate) heading: String,
    pub(crate) photos: Vec<PhotoItem>,
}

/// The widgets [`build_gallery_page`] hands back to [`build_window`].
pub(crate) struct GalleryWidgets {
    /// Flat, newest-first list of every loaded photo. Backs the lightbox's
    /// prev/next navigation; the visible sections are derived from it.
    pub(crate) model: gio::ListStore,
    /// Day sections rendered by the ListView, derived from `model`.
    pub(crate) groups: gio::ListStore,
    /// Swaps between the timeline and the empty/loading/error status page.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) subtitle: gtk4::Label,
    pub(crate) more: gtk4::Button,
    pub(crate) list: gtk4::ListView,
    pub(crate) scroll: gtk4::ScrolledWindow,
    pub(crate) retry: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
    /// The All / Photos / Videos / Raw filter toggles, in that order (index maps
    /// to [`kind_for_tab`]).
    pub(crate) tabs: [gtk4::ToggleButton; 4],
    /// The date-jump dropdown, populated with the timeline's months.
    pub(crate) dates: gtk4::DropDown,
}

/// The Photos page: a [`gtk4::ListView`] of day sections, each a heading over
/// that day's photos laid out as justified rows (see [`justify_rows`]) — every
/// photo at its own aspect ratio, every row filled edge to edge.
///
/// A ListView of sections rather than one flat GridView because GTK's grid has no
/// row headers and forces square cells: the justified rows and the date headings
/// both need per-row structure, and the ListView only realises the sections on
/// screen, which is what keeps a 10,000-photo timeline cheap. The factory is
/// installed by [`wire_gallery`], which has the [`Ui`] the tiles need (zoom
/// level, thumbnail cache, click-to-open).
pub(crate) fn build_gallery_page() -> (gtk4::Widget, GalleryWidgets) {
    let model = gio::ListStore::new::<BoxedAnyObject>();
    let groups = gio::ListStore::new::<BoxedAnyObject>();

    let selection = gtk4::NoSelection::new(Some(groups.clone()));
    let list = gtk4::ListView::builder()
        .model(&selection)
        .single_click_activate(false)
        .build();
    list.add_css_class("gallery-sections");

    // Shown only when a load failed because the mount is down; restarts it.
    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);

    let status = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    // Kept as an explicit fallback: the timeline also pages itself in as the
    // scroll nears the bottom (see [`wire_gallery`]), so reaching this button at
    // all is unusual.
    let more = gtk4::Button::builder()
        .label("Load more")
        .halign(gtk4::Align::Center)
        .build();
    more.add_css_class("pill");
    more.set_visible(false);

    // Horizontal scrolling is never wanted: rows are justified to the viewport
    // width, and a stray hscrollbar would fight the layout.
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&list)
        .build();

    let title_label = gtk4::Label::builder()
        .label("Photos")
        .halign(gtk4::Align::Start)
        .build();
    title_label.add_css_class("title-2");

    let subtitle = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .visible(false)
        .build();
    subtitle.add_css_class("dim-label");
    subtitle.add_css_class("caption");

    let titles = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(&title_label);
    titles.append(&subtitle);

    let upload = gtk4::Button::builder()
        .label("Upload")
        .icon_name("list-add-symbolic")
        .valign(gtk4::Align::Center)
        .build();
    upload.add_css_class("pill");
    upload.add_css_class("suggested-action");
    let refresh = refresh_button();

    let header_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header_box.append(&titles);
    header_box.append(&refresh);
    header_box.append(&upload);

    // All / Photos / Videos / Raw filter. Linked toggles acting as one segmented
    // control: exactly one is active, and flipping it reloads the timeline
    // filtered to that kind (wired in [`wire_gallery`]). Labels gain live counts
    // once a page lands.
    let tab_labels = ["All", "Photos", "Videos", "Raw"];
    let tabs: [gtk4::ToggleButton; 4] = std::array::from_fn(|i| {
        gtk4::ToggleButton::builder()
            .label(tab_labels[i])
            .active(i == 0)
            .build()
    });
    // Group the toggles so they behave as a radio set: chaining each to the first
    // is what GTK turns into mutual exclusion.
    for btn in &tabs[1..] {
        btn.set_group(Some(&tabs[0]));
    }
    let tab_group = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    tab_group.add_css_class("linked");
    for btn in &tabs {
        btn.add_css_class("pill");
        tab_group.append(btn);
    }

    // Date jump: "All dates" plus a row per month, filled in once the timeline's
    // months are known (see [`refresh_photo_months`]). Pushed to the far end of
    // the filter row, opposite the kind toggles.
    let dates = gtk4::DropDown::from_strings(&["All dates"]);
    dates.add_css_class("pill");
    dates.set_tooltip_text(Some("Jump to a month"));

    let filter_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    filter_bar.append(&tab_group);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    filter_bar.append(&spacer);
    filter_bar.append(&dates);

    // The timeline (plus its pager) or the status page, never both.
    let timeline = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    timeline.append(&scroll);
    timeline.append(&more);

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&timeline, Some("timeline"));
    content.add_named(&status, Some("status"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header_box);
    inner.append(&filter_bar);
    inner.append(&content);

    (
        inner.upcast(),
        GalleryWidgets {
            model,
            groups,
            content,
            status,
            subtitle,
            more,
            list,
            scroll,
            retry,
            upload,
            refresh,
            tabs,
            dates,
        },
    )
}

/// The [`PhotoKind`] filter a gallery tab index selects: index 0 is All (no
/// filter), then Photos / Videos / Raw. Index-aligned with the toggle array.
pub(crate) fn kind_for_tab(index: usize) -> Option<PhotoKind> {
    match index {
        1 => Some(PhotoKind::Photo),
        2 => Some(PhotoKind::Video),
        3 => Some(PhotoKind::Raw),
        _ => None,
    }
}

/// The `[from, to)` epoch-second window of a local calendar month, or `None` if
/// the date is somehow unrepresentable. Computed with glib so month rollover and
/// the local timezone (matching the daemon's month buckets) are handled for us.
pub(crate) fn month_range(year: i32, month: i32) -> Option<(i64, i64)> {
    let start = glib::DateTime::from_local(year, month, 1, 0, 0, 0.0).ok()?;
    let end = start.add_months(1).ok()?;
    Some((start.to_unix(), end.to_unix()))
}

/// English month names, indexed 1..=12.
pub(crate) const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];

/// Rebuild the date-jump dropdown for the active kind: ask the daemon which
/// months the timeline spans and turn them into "Month YYYY (count)" rows, each
/// remembering the window it jumps to. Resets the selection to "All dates" — the
/// caller pairs this with a fresh timeline load. Off the UI thread; a failure
/// just leaves the dropdown as it was.
pub(crate) fn refresh_photo_months(ui: &Rc<Ui>) {
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::PhotoMonths {
            kind: ui.gallery.kind.get(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let Ok(Ok(Response::PhotoMonths { months })) = rx.recv().await else {
            return;
        };
        let mut labels = vec!["All dates".to_string()];
        let mut ranges: Vec<Option<(i64, i64)>> = vec![None];
        for m in months {
            let name = MONTH_NAMES.get((m.month - 1) as usize).copied().unwrap_or("?");
            labels.push(format!("{name} {} ({})", m.year, m.count));
            ranges.push(month_range(m.year, m.month));
        }
        let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

        // Repopulating resets the selection to 0, which would otherwise fire the
        // handler and reload; suppress that — the caller is already reloading.
        ui.gallery.date_suppress.set(true);
        ui.gallery.dates
            .set_model(Some(&gtk4::StringList::new(&label_refs)));
        ui.gallery.dates.set_selected(0);
        *ui.gallery.date_ranges.borrow_mut() = ranges;
        ui.gallery.date_suppress.set(false);
    });
}

/// Label the filter toggles with the whole-timeline `(photos, videos, raw)`
/// counts, so a glance shows how much sits behind each tab. A tab with nothing
/// behind it is disabled — you can't filter to an empty set — but the currently
/// selected one stays clickable so you can always switch back off it.
pub(crate) fn update_gallery_tabs(ui: &Rc<Ui>, counts: (usize, usize, usize)) {
    let (photos, videos, raw) = counts;
    let totals = [photos + videos + raw, photos, videos, raw];
    for (index, tab) in ui.gallery.tabs.iter().enumerate() {
        let name = ["All", "Photos", "Videos", "Raw"][index];
        let n = totals[index];
        tab.set_label(&format!("{name} {n}"));
        tab.set_sensitive(n > 0 || tab.is_active());
    }
}

/// Wire the gallery: install the section factory, the zoom gestures, the pager
/// and the upload button. Activating a thumbnail downloads the photo and opens it
/// in the in-app lightbox.
pub(crate) fn wire_gallery(ui: &Rc<Ui>, list: &gtk4::ListView, scroll: &gtk4::ScrolledWindow) {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let section = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        section.set_margin_bottom(16);
        let heading = gtk4::Label::builder().halign(gtk4::Align::Start).build();
        heading.add_css_class("heading");
        heading.add_css_class("gallery-day");
        section.append(&heading);
        item.set_child(Some(&section));
        item.set_activatable(false);
    });

    let ui_bind = ui.clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let section = item.child().and_downcast::<gtk4::Box>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let group = obj.borrow::<PhotoGroup>();

        let heading = section.first_child().and_downcast::<gtk4::Label>().unwrap();
        heading.set_label(&group.heading);

        fill_section(&ui_bind, &section, &group.photos);
        // Remember the realised section so a learned aspect ratio or a resize can
        // re-justify it in place, without rebuilding the ListStore (which would
        // yank the scroll position back to the top).
        ui_bind
            .gallery.bound
            .borrow_mut()
            .insert(item.position(), section);
    });

    // ListView recycles section widgets, so a scrolled-away day must give up its
    // claim on the widgets — otherwise a thumbnail landing late would paint into
    // a tile that now shows a different day.
    let ui_unbind = ui.clone();
    factory.connect_unbind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        ui_unbind
            .gallery.bound
            .borrow_mut()
            .remove(&item.position());
        if let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() {
            let group = obj.borrow::<PhotoGroup>();
            let mut wanted = ui_unbind.gallery.thumb_wanted.borrow_mut();
            for photo in &group.photos {
                wanted.remove(&photo.uid);
            }
        }
    });
    list.set_factory(Some(&factory));

    // Rows are justified to the content width, so a resize re-justifies whatever
    // is on screen (offscreen sections pick the new width up when they bind).
    let ui_width = ui.clone();
    list.connect_notify_local(Some("width"), move |list, _| {
        let width = list.width();
        if width > 0 && width != ui_width.gallery.width.get() {
            ui_width.gallery.width.set(width);
            schedule_relayout(&ui_width);
        }
    });

    // Page the timeline in as the scroll nears the end, so "load more" is a
    // fallback button rather than something the user has to hunt for.
    let ui_scroll = ui.clone();
    scroll.vadjustment().connect_value_changed(move |adj| {
        let near_end = adj.value() + adj.page_size() >= adj.upper() - adj.page_size() * 0.5;
        if near_end && ui_scroll.gallery.more.is_visible() && ui_scroll.gallery.more.is_sensitive()
        {
            load_gallery(&ui_scroll, true);
        }
    });

    // Ctrl+scroll zoom. Capture phase so the ScrolledWindow doesn't eat the event
    // and scroll the page out from under the gesture.
    let zoom_scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
    zoom_scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let ui_zoom = ui.clone();
    zoom_scroll.connect_scroll(move |controller, _dx, dy| {
        if !controller
            .current_event_state()
            .contains(gtk4::gdk::ModifierType::CONTROL_MASK)
            || dy == 0.0
        {
            return glib::Propagation::Proceed;
        }
        // Scroll up (negative dy) zooms in, i.e. bigger tiles.
        zoom_gallery(&ui_zoom, if dy < 0.0 { TILE_STEP } else { -TILE_STEP });
        glib::Propagation::Stop
    });
    scroll.add_controller(zoom_scroll);

    // Ctrl+plus / Ctrl+minus / Ctrl+0, the keyboard equivalents.
    let zoom_keys = gtk4::EventControllerKey::new();
    let ui_keys = ui.clone();
    zoom_keys.connect_key_pressed(move |_, key, _code, state| {
        if !state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
            return glib::Propagation::Proceed;
        }
        match key.name().as_deref() {
            Some("plus" | "equal" | "KP_Add") => zoom_gallery(&ui_keys, TILE_STEP),
            Some("minus" | "KP_Subtract") => zoom_gallery(&ui_keys, -TILE_STEP),
            Some("0" | "KP_0") => set_gallery_tile(&ui_keys, TILE_DEFAULT),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    list.add_controller(zoom_keys);

    let ui_more = ui.clone();
    ui.gallery.more.clone().connect_clicked(move |_| {
        load_gallery(&ui_more, true);
    });

    // Filter toggles: flipping to a tab reloads the timeline filtered to that
    // kind. Only the button being switched *on* acts — the group also fires a
    // `toggled` for the one switching off, which this skips — and a redundant
    // toggle to the already-current kind is a no-op.
    for (index, tab) in ui.gallery.tabs.iter().enumerate() {
        let ui_tab = ui.clone();
        tab.connect_toggled(move |btn| {
            if !btn.is_active() {
                return;
            }
            let kind = kind_for_tab(index);
            if ui_tab.gallery.kind.get() == kind {
                return;
            }
            ui_tab.gallery.kind.set(kind);
            // A different kind has a different set of months; clearing the active
            // window makes the reload rebuild the date jump for the new kind.
            ui_tab.gallery.range.set(None);
            load_gallery(&ui_tab, false);
        });
    }

    // Date jump: selecting a month loads that window; "All dates" (row 0) clears
    // it. Skipped while the model is being repopulated (see `gallery_date_suppress`).
    let ui_dates = ui.clone();
    ui.gallery.dates.connect_selected_notify(move |dd| {
        if ui_dates.gallery.date_suppress.get() {
            return;
        }
        let range = ui_dates
            .gallery.date_ranges
            .borrow()
            .get(dd.selected() as usize)
            .copied()
            .flatten();
        if ui_dates.gallery.range.get() == range {
            return;
        }
        ui_dates.gallery.range.set(range);
        load_gallery(&ui_dates, false);
    });

    let ui_upload = ui.clone();
    ui.gallery.upload.connect_clicked(move |_| {
        let dialog = gtk4::FileDialog::builder()
            .title("Select Photo to Upload")
            .build();

        let filter = gtk4::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_mime_type("image/*");
        let filters = gio::ListStore::new::<gtk4::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));

        let ui = ui_upload.clone();
        let parent_win = ui.stack.root().and_downcast::<gtk4::Window>();
        dialog.open(parent_win.as_ref(), gio::Cancellable::NONE, move |res| {
            if let Ok(file) = res
                && let Some(path) = file.path()
            {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("photo.jpg")
                    .to_string();
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!("Failed to read photo: {e}");
                        return;
                    }
                };
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("jpg")
                    .to_lowercase();
                let media_type = match ext.as_str() {
                    "png" => "image/png",
                    "gif" => "image/gif",
                    "webp" => "image/webp",
                    "tiff" | "tif" => "image/tiff",
                    _ => "image/jpeg",
                };
                let capture_time = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);

                ui.busy_begin();
                let rx = spawn_request(
                    ui.dirs.control_socket(),
                    Request::UploadPhoto {
                        name,
                        media_type: media_type.to_string(),
                        bytes,
                        capture_time,
                    },
                );
                let ui_clone = ui.clone();
                glib::spawn_future_local(async move {
                    let res = rx.recv().await;
                    ui_clone.busy_end();
                    match res {
                        Ok(Ok(Response::Ok { message })) => {
                            tracing::info!("Photo uploaded: {message}");
                            load_gallery(&ui_clone, false);
                            toast(&ui_clone, "Photo uploaded");
                        }
                        Ok(Ok(Response::Error { message, kind })) => {
                            toast_failure(&ui_clone, "Couldn't upload photo", &message, kind);
                        }
                        _ => {
                            toast_error(
                                &ui_clone,
                                "Couldn't upload photo",
                                "The mount service didn't respond.",
                            );
                        }
                    }
                });
            }
        });
    });
}

/// One tile in a justified row: the photo, and the pixel size it was justified to.
pub(crate) struct Tile {
    pub(crate) photo: PhotoItem,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// Break one day's photos into rows that each fill `width` exactly, at heights
/// near [`Ui::gallery_tile`] — the layout every modern photo gallery uses, and
/// the reason nothing here has to be cropped: each tile keeps its own aspect
/// ratio, and it is the row *height* that flexes to make the widths add up.
///
/// Greedy, one pass: keep adding photos to the row: the more photos share it, the
/// shorter it has to be to fit. The moment that height drops to the target, the
/// row is as full as it should be and is emitted. The trailing row keeps the
/// target height instead of being stretched, so a day with three photos doesn't
/// blow them up to fill a screen-wide row.
///
/// Photos whose thumbnail hasn't been decoded yet are laid out at
/// [`RATIO_UNKNOWN`]; [`Ui::store_texture`] reports the real ratio when it lands
/// and the section is re-justified in place.
pub(crate) fn justify_rows(ui: &Rc<Ui>, photos: &[PhotoItem], width: i32) -> Vec<Vec<Tile>> {
    let ratios: Vec<f64> = photos.iter().map(|photo| ui.ratio(&photo.uid)).collect();
    let target = f64::from(ui.gallery.tile.get());
    let plan = plan_rows(&ratios, target, f64::from(width));

    let mut photos = photos.iter();
    plan.into_iter()
        .map(|row| {
            row.into_iter()
                .filter_map(|(width, height)| {
                    photos.next().map(|photo| Tile {
                        photo: photo.clone(),
                        width,
                        height,
                    })
                })
                .collect()
        })
        .collect()
}

/// The row-packing math behind [`justify_rows`], over nothing but aspect ratios:
/// takes each photo's ratio (w/h) in order and returns the pixel `(width,
/// height)` of every tile, grouped into rows.
pub(crate) fn plan_rows(ratios: &[f64], target: f64, width: f64) -> Vec<Vec<(i32, i32)>> {
    let avail = width.max(f64::from(TILE_MIN));
    let gap = f64::from(TILE_GAP);

    // The height a row of `n` photos with `sum_ratio` total ratio must take for
    // its widths (plus gaps) to add up to exactly `avail`.
    let row_height = |sum_ratio: f64, n: usize| {
        let gaps = gap * (n.saturating_sub(1)) as f64;
        (avail - gaps) / sum_ratio
    };

    let mut rows: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut row: Vec<f64> = Vec::new();
    let mut sum_ratio = 0.0;

    for ratio in ratios {
        let ratio = ratio.clamp(RATIO_MIN, RATIO_MAX);
        row.push(ratio);
        sum_ratio += ratio;

        let height = row_height(sum_ratio, row.len());
        if height <= target {
            rows.push(size_row(&row, height, avail, true));
            row.clear();
            sum_ratio = 0.0;
        }
    }
    if !row.is_empty() {
        let height = row_height(sum_ratio, row.len()).min(target);
        rows.push(size_row(&row, height, avail, false));
    }
    rows
}

/// Size one row's tiles at `height`. A `justified` row is nudged so its widths
/// plus gaps hit `avail` exactly — rounding each width independently leaves a
/// few px of ragged right edge, so the last tile absorbs the remainder.
pub(crate) fn size_row(ratios: &[f64], height: f64, avail: f64, justified: bool) -> Vec<(i32, i32)> {
    let height = height.round().max(1.0);
    let mut sizes: Vec<(i32, i32)> = ratios
        .iter()
        .map(|ratio| ((ratio * height).round().max(1.0) as i32, height as i32))
        .collect();

    if justified {
        let gaps = TILE_GAP * (sizes.len().saturating_sub(1)) as i32;
        let used: i32 = sizes.iter().rev().skip(1).map(|(width, _)| *width).sum();
        if let Some(last) = sizes.last_mut() {
            last.0 = (avail as i32 - gaps - used).max(1);
        }
    }
    sizes
}

/// (Re)build a bound day-section's tiles: justify this day's photos to the
/// current content width and hand each tile whatever thumbnail is already in
/// memory, queueing the rest. Replaces the section's rows in place, leaving the
/// heading — so a re-justify never touches the ListView's model or scroll.
pub(crate) fn fill_section(ui: &Rc<Ui>, section: &gtk4::Box, photos: &[PhotoItem]) {
    let Some(heading) = section.first_child() else {
        return;
    };
    while let Some(old) = heading.next_sibling() {
        section.remove(&old);
    }

    let width = gallery_width(ui);
    let rows = gtk4::Box::new(gtk4::Orientation::Vertical, TILE_GAP);
    for row in justify_rows(ui, photos, width) {
        let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, TILE_GAP);
        for tile in row {
            row_box.append(&photo_tile(ui, tile));
        }
        rows.append(&row_box);
    }
    section.append(&rows);
    schedule_thumbs(ui);
}

/// The width justified rows are laid out to: the ListView's own width, less a
/// couple of px so a rounding error can't push a row into a horizontal overflow.
/// Falls back to a sane guess before the first allocation.
pub(crate) fn gallery_width(ui: &Rc<Ui>) -> i32 {
    match ui.gallery.width.get() {
        0 => 900,
        w => (w - 2).max(TILE_MIN),
    }
}

/// One photo tile: a fixed-size button wrapping the thumbnail, with the capture
/// time revealed on hover over a bottom scrim. A button (rather than a bare
/// picture) so the tile is focusable, keyboard-activatable and gets hover feedback
/// for free.
///
/// The picture sits in an overlay over a placeholder, so a tile is never a hole:
/// until the thumbnail lands it shows a dim card, and a photo that can never have
/// one keeps an image glyph instead of an empty rectangle.
pub(crate) fn photo_tile(ui: &Rc<Ui>, tile: Tile) -> gtk4::Button {
    let picture = gtk4::Picture::builder()
        // The tile is exactly the thumbnail's own aspect ratio, so Cover scales
        // it and crops nothing; it only bites during the brief window where the
        // ratio is still a guess, and the re-justify then fixes the tile.
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .build();

    let placeholder = gtk4::Image::builder()
        .icon_name("image-x-generic-symbolic")
        .pixel_size(24)
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .build();
    placeholder.add_css_class("photo-placeholder");

    // The capture time, on a gradient that only exists while the pointer is over
    // the tile — legible over any photo, invisible the rest of the time.
    let caption = gtk4::Label::builder()
        // Fill horizontally so the scrim spans the tile; the text itself stays
        // left-aligned inside it.
        .halign(gtk4::Align::Fill)
        .valign(gtk4::Align::End)
        .xalign(0.0)
        .label(short_capture_time(tile.photo.capture_time))
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    caption.add_css_class("photo-caption");

    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&placeholder));
    overlay.add_overlay(&picture);
    overlay.add_overlay(&caption);

    // A video reads as a video at a glance: a play glyph centred over the poster
    // thumbnail. Kept above the caption scrim so it stays legible on hover.
    let is_video = tile.photo.kind == PhotoKind::Video;
    if is_video {
        let badge = gtk4::Image::builder()
            .icon_name("media-playback-start-symbolic")
            .pixel_size(28)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        badge.add_css_class("photo-video-badge");
        overlay.add_overlay(&badge);
    }

    let button = gtk4::Button::builder()
        .child(&overlay)
        .width_request(tile.width)
        .height_request(tile.height)
        .tooltip_text(format_capture_time(tile.photo.capture_time))
        .build();
    button.add_css_class("photo-tile");
    button.add_css_class("flat");
    // Clip the thumbnail to the tile's rounded corners.
    button.set_overflow(gtk4::Overflow::Hidden);

    want_thumb(ui, &tile.photo, &picture);

    // A still opens in the in-app lightbox; a video can't render there, so it
    // downloads and hands off to an external player instead.
    let ui_open = ui.clone();
    let uid = tile.photo.uid.clone();
    button.connect_clicked(move |_| {
        if is_video {
            play_video(&ui_open, uid.clone());
        } else {
            open_photo_viewer(&ui_open, uid.clone());
        }
    });
    button
}

/// Give `picture` its thumbnail: straight from the texture cache when it's there,
/// otherwise register the tile as waiting and get the thumbnail moving — decoding
/// it if the daemon already had it cached on disk, or asking the daemon for it.
///
/// This is what makes the gallery on-demand: only tiles the ListView actually
/// realises ever ask for an image.
pub(crate) fn want_thumb(ui: &Rc<Ui>, photo: &PhotoItem, picture: &gtk4::Picture) {
    if let Some(texture) = ui.gallery.photo_tex.borrow().get(&photo.uid) {
        picture.set_paintable(Some(texture));
        return;
    }
    // No thumbnail will ever come for this one — not from the server, and not
    // from the daemon's own scaling of the file. The tile keeps its placeholder
    // glyph, and stays clickable: the full photo may still open fine.
    if ui.gallery.photo_nothumb.borrow().contains(&photo.uid) {
        return;
    }

    ui.gallery.thumb_wanted
        .borrow_mut()
        .insert(photo.uid.clone(), picture.clone());

    match photo.thumb_path.as_deref() {
        Some(path) => {
            ui.gallery.decode_queue
                .borrow_mut()
                .push_back((photo.uid.clone(), path.to_string()));
            schedule_decode(ui);
        }
        None => {
            let mut queue = ui.gallery.thumb_queue.borrow_mut();
            if !queue.contains(&photo.uid) {
                queue.push_back(photo.uid.clone());
            }
        }
    }
}

/// Come back for thumbnails the daemon is still generating — it is downloading
/// each photo's full file to scale it, which takes far longer than a batch. The
/// tiles keep their placeholder until then, and a tile that has scrolled away is
/// dropped by [`flush_thumbs`] like any other queued uid.
pub(crate) fn retry_pending_thumbs(ui: &Rc<Ui>, uids: Vec<String>) {
    let ui = ui.clone();
    glib::timeout_add_local_once(THUMB_RETRY, move || {
        {
            let mut queue = ui.gallery.thumb_queue.borrow_mut();
            for uid in uids {
                if !queue.contains(&uid) {
                    queue.push_back(uid);
                }
            }
        }
        schedule_thumbs(&ui);
    });
}

/// Ask the daemon for the queued thumbnails after a short pause, so a fast scroll
/// coalesces into one batch per settle rather than one per row it flew past.
pub(crate) fn schedule_thumbs(ui: &Rc<Ui>) {
    if ui.gallery.thumb_queue.borrow().is_empty() || ui.gallery.thumb_inflight.get() {
        return;
    }
    if let Some(id) = ui.gallery.thumb_source.borrow_mut().take() {
        id.remove();
    }
    let ui_flush = ui.clone();
    let source = glib::timeout_add_local_once(THUMB_DEBOUNCE, move || {
        ui_flush.gallery.thumb_source.borrow_mut().take();
        flush_thumbs(&ui_flush);
    });
    *ui.gallery.thumb_source.borrow_mut() = Some(source);
}

/// Send one [`Request::PhotoThumbs`] batch for the tiles still on screen. Queued
/// uids whose tile has scrolled away are dropped rather than fetched: the point
/// of the batch is what the user is looking at *now*.
pub(crate) fn flush_thumbs(ui: &Rc<Ui>) {
    if ui.gallery.thumb_inflight.get() {
        return;
    }
    let uids: Vec<String> = {
        let mut queue = ui.gallery.thumb_queue.borrow_mut();
        let wanted = ui.gallery.thumb_wanted.borrow();
        let mut batch = Vec::new();
        while batch.len() < THUMB_BATCH {
            let Some(uid) = queue.pop_front() else { break };
            if wanted.contains_key(&uid) {
                batch.push(uid);
            }
        }
        batch
    };
    if uids.is_empty() {
        return;
    }

    ui.gallery.thumb_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::PhotoThumbs { uids });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.gallery.thumb_inflight.set(false);
        match result {
            Ok(Ok(Response::Thumbs { items })) => {
                let mut decode = ui.gallery.decode_queue.borrow_mut();
                let mut nothumb = ui.gallery.photo_nothumb.borrow_mut();
                let mut pending = Vec::new();
                for item in items {
                    match item.path {
                        Some(path) => decode.push_back((item.uid, path)),
                        // The daemon is making this one itself, from the photo's
                        // full file — that takes a download, so come back for it.
                        None if item.pending => pending.push(item.uid),
                        // No thumbnail exists and none can be made: remember that,
                        // so scrolling past the tile doesn't re-ask forever.
                        None => {
                            nothumb.insert(item.uid);
                        }
                    }
                }
                drop((decode, nothumb));
                schedule_decode(&ui);
                if !pending.is_empty() {
                    retry_pending_thumbs(&ui, pending);
                }
            }
            // A thumbnail that doesn't arrive is not worth a toast — the tile just
            // stays a placeholder, and the next scroll past it tries again.
            Ok(Ok(Response::Error { message, .. })) => {
                tracing::debug!("photo thumbs failed: {message}")
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => tracing::debug!("photo thumbs: no reply"),
        }
        // Whatever the batch did, more tiles may have queued up behind it.
        schedule_thumbs(&ui);
    });
}

/// Decode queued thumbnails into textures on an idle callback, a few per pass, so
/// a big batch fills in progressively instead of freezing the scroll for the
/// length of the whole decode.
pub(crate) fn schedule_decode(ui: &Rc<Ui>) {
    if ui.gallery.decode_idle.get() || ui.gallery.decode_queue.borrow().is_empty() {
        return;
    }
    ui.gallery.decode_idle.set(true);

    let ui = ui.clone();
    glib::idle_add_local(move || {
        let batch: Vec<(String, String)> = {
            let mut queue = ui.gallery.decode_queue.borrow_mut();
            (0..4).filter_map(|_| queue.pop_front()).collect()
        };
        let mut relayout = false;
        for (uid, path) in batch {
            let texture = match gtk4::gdk::Texture::from_filename(&path) {
                Ok(texture) => texture,
                Err(e) => {
                    tracing::debug!("cannot decode thumbnail {path}: {e}");
                    ui.gallery.photo_nothumb.borrow_mut().insert(uid);
                    continue;
                }
            };
            // A ratio we hadn't seen means the tile was sized against a guess.
            relayout |= ui.store_texture(&uid, texture.clone());
            if let Some(picture) = ui.gallery.thumb_wanted.borrow_mut().remove(&uid) {
                picture.set_paintable(Some(&texture));
            }
        }
        if relayout {
            schedule_relayout(&ui);
        }

        if ui.gallery.decode_queue.borrow().is_empty() {
            ui.gallery.decode_idle.set(false);
            ui.save_ratios();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// Re-justify the sections on screen shortly. Debounced, because the triggers
/// (a window resize, a zoom step, a burst of decoded thumbnails) all arrive in
/// floods and only the final state matters.
pub(crate) fn schedule_relayout(ui: &Rc<Ui>) {
    if let Some(id) = ui.gallery.relayout_source.borrow_mut().take() {
        id.remove();
    }
    let ui_relayout = ui.clone();
    let source = glib::timeout_add_local_once(RELAYOUT_DEBOUNCE, move || {
        ui_relayout.gallery.relayout_source.borrow_mut().take();
        relayout_gallery(&ui_relayout);
    });
    *ui.gallery.relayout_source.borrow_mut() = Some(source);
}

/// Rebuild the tiles of the day sections currently on screen, at the current
/// width, zoom and set of known aspect ratios. Sections that are *not* realised
/// need no work: they justify themselves against the current state when the
/// ListView binds them.
pub(crate) fn relayout_gallery(ui: &Rc<Ui>) {
    let bound: Vec<(u32, gtk4::Box)> = ui
        .gallery.bound
        .borrow()
        .iter()
        .map(|(pos, section)| (*pos, section.clone()))
        .collect();
    for (pos, section) in bound {
        let Some(obj) = ui.gallery.groups.item(pos) else {
            continue;
        };
        let Some(boxed) = obj.downcast_ref::<BoxedAnyObject>() else {
            continue;
        };
        let photos = boxed.borrow::<PhotoGroup>().photos.clone();
        fill_section(ui, &section, &photos);
    }
}

/// Step the row height by `delta` px and re-justify, clamped to the zoom range.
pub(crate) fn zoom_gallery(ui: &Rc<Ui>, delta: i32) {
    set_gallery_tile(ui, ui.gallery.tile.get() + delta);
}

/// Set the row height (clamped) and re-justify the visible sections at it.
pub(crate) fn set_gallery_tile(ui: &Rc<Ui>, tile: i32) {
    let tile = tile.clamp(TILE_MIN, TILE_MAX);
    if tile == ui.gallery.tile.get() {
        return;
    }
    ui.gallery.tile.set(tile);
    schedule_relayout(ui);
}

/// Rebuild the day sections from the flat photo model. The timeline arrives
/// newest-first, so photos of the same day are already contiguous — one pass
/// splits them.
///
/// The groups are diffed into the existing store rather than replacing it: a
/// "load more" only really changes the last day (the one the new page continues)
/// and appends after it, and clearing the store instead would scroll the user
/// back to the top of the timeline at the exact moment they asked for more.
pub(crate) fn repaint_gallery(ui: &Rc<Ui>) {
    let groups = group_photos(&ui.gallery.model);
    let store = &ui.gallery.groups;

    for (i, group) in groups.iter().enumerate() {
        let i = i as u32;
        let unchanged = store
            .item(i)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|old| {
                let old = old.borrow::<PhotoGroup>();
                old.heading == group.heading && old.photos.len() == group.photos.len()
            });
        if unchanged {
            continue;
        }
        let boxed = BoxedAnyObject::new(PhotoGroup {
            heading: group.heading.clone(),
            photos: group.photos.clone(),
        });
        if i < store.n_items() {
            store.splice(i, 1, &[boxed]);
        } else {
            store.append(&boxed);
        }
    }
    // Photos only ever get appended, so a shorter model means a fresh load.
    if store.n_items() > groups.len() as u32 {
        let len = groups.len() as u32;
        store.splice(len, store.n_items() - len, &[] as &[BoxedAnyObject]);
    }

    let count = ui.gallery.model.n_items();
    ui.gallery.subtitle.set_visible(count > 0);
    // The noun tracks the active filter, so a Videos tab doesn't count "photos".
    let (one, many) = match ui.gallery.kind.get() {
        Some(PhotoKind::Video) => ("video", "videos"),
        Some(PhotoKind::Raw) => ("raw photo", "raw photos"),
        _ => ("photo", "photos"),
    };
    ui.gallery.subtitle.set_label(&match count {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    });
}

pub(crate) fn group_photos(model: &gio::ListStore) -> Vec<PhotoGroup> {
    let mut groups: Vec<PhotoGroup> = Vec::new();
    for i in 0..model.n_items() {
        let Some(obj) = model.item(i) else { continue };
        let Some(boxed) = obj.downcast_ref::<BoxedAnyObject>() else {
            continue;
        };
        let photo = boxed.borrow::<PhotoItem>().clone();
        let heading = day_heading(photo.capture_time);
        match groups.last_mut() {
            Some(group) if group.heading == heading => group.photos.push(photo),
            _ => groups.push(PhotoGroup {
                heading,
                photos: vec![photo],
            }),
        }
    }
    groups
}

/// Section heading for a capture time: "Today", "Yesterday", or the local date.
pub(crate) fn day_heading(secs: i64) -> String {
    let Ok(date) = glib::DateTime::from_unix_local(secs) else {
        return "Unknown date".into();
    };
    let same_day = |other: &glib::DateTime| {
        other.year() == date.year()
            && other.month() == date.month()
            && other.day_of_month() == date.day_of_month()
    };
    if let Ok(now) = glib::DateTime::now_local() {
        if same_day(&now) {
            return "Today".into();
        }
        if let Ok(yesterday) = glib::DateTime::from_unix_local(now.to_unix() - 86_400)
            && same_day(&yesterday)
        {
            return "Yesterday".into();
        }
    }
    date.format("%-d %B %Y")
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "Unknown date".into())
}

pub(crate) fn find_photo_index(model: &gio::ListStore, uid: &str) -> Option<u32> {
    for i in 0..model.n_items() {
        if let Some(obj) = model.item(i)
            && let Some(boxed) = obj.downcast_ref::<BoxedAnyObject>()
            && boxed.borrow::<PhotoItem>().uid == uid
        {
            return Some(i);
        }
    }
    None
}

pub(crate) fn format_capture_time(secs: i64) -> String {
    let date = glib::DateTime::from_unix_local(secs);
    match date {
        Ok(d) => match d.format("%Y-%m-%d %H:%M:%S") {
            Ok(s) => s.to_string(),
            Err(_) => "Unknown Date".to_string(),
        },
        Err(_) => "Unknown Date".to_string(),
    }
}

/// The capture time as a tile caption: the clock time alone, since the day is
/// already the section heading right above it.
pub(crate) fn short_capture_time(secs: i64) -> String {
    glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format("%H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Fetch a timeline page from the daemon. When `append` is false the model is
/// cleared first (fresh load); otherwise the next page is tacked on.
pub(crate) fn load_gallery(ui: &Rc<Ui>, append: bool) {
    if ui.gallery.loading.get() {
        return;
    }
    if !append {
        // Fresh load: clear the timeline and show Loading until the first page lands.
        ui.gallery.model.remove_all();
        gallery_status(
            ui,
            "image-x-generic-symbolic",
            "Loading photos…",
            "Reading your Proton Drive timeline.",
            false,
        );
        // Rebuild the date jump for the current kind, but only for a full-span
        // load — a jump *to* a month sets a range and reloads, and refreshing the
        // dropdown then would fight the selection the user just made.
        if ui.gallery.range.get().is_none() {
            refresh_photo_months(ui);
        }
    }
    let offset = ui.gallery.model.n_items() as usize;
    ui.gallery.loading.set(true);
    ui.gallery.more.set_sensitive(false);

    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::PhotosTimeline {
            offset,
            limit: PHOTOS_PAGE,
            kind: ui.gallery.kind.get(),
            range: ui.gallery.range.get(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.gallery.loading.set(false);
        ui.gallery.more.set_sensitive(true);
        match result {
            Ok(Ok(Response::Photos {
                available,
                items,
                counts,
            })) => {
                if !available {
                    gallery_status(
                        &ui,
                        "image-missing-symbolic",
                        "No photo library",
                        "This Proton account doesn't have Photos enabled.",
                        false,
                    );
                    return;
                }
                // Label the filter tabs with live per-kind counts.
                if let Some(counts) = counts {
                    update_gallery_tabs(&ui, counts);
                }
                // Take the daemon's word on ratios and thumbnail verdicts before
                // anything is laid out: it persists both, so the very first frame
                // justifies its rows correctly instead of guessing RATIO_UNKNOWN
                // and reflowing as the images land.
                {
                    let mut ratios = ui.gallery.photo_ratio.borrow_mut();
                    let mut nothumb = ui.gallery.photo_nothumb.borrow_mut();
                    for item in &items {
                        if let Some(ratio) = item.ratio {
                            ratios.insert(item.uid.clone(), ratio);
                        }
                        if item.no_thumb {
                            nothumb.insert(item.uid.clone());
                        }
                    }
                }
                for item in &items {
                    ui.gallery.model.append(&BoxedAnyObject::new(item.clone()));
                }
                repaint_gallery(&ui);
                if ui.gallery.model.n_items() == 0 {
                    gallery_status(
                        &ui,
                        "image-x-generic-symbolic",
                        "No photos yet",
                        "Photos you upload to Proton Drive appear here.",
                        false,
                    );
                    return;
                }
                ui.gallery.content.set_visible_child_name("timeline");
                // Offer "Load more" only when the page came back full.
                ui.gallery.more.set_visible(items.len() == PHOTOS_PAGE);
            }
            // A failed *next* page keeps the photos already on screen — the failure
            // goes to a toast rather than wiping the timeline for a status page.
            Ok(Ok(Response::Error { message, .. })) if append => {
                toast_error(&ui, "Couldn't load more photos", &message)
            }
            Ok(Ok(Response::Error { message, .. })) => gallery_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't load photos",
                &message,
                false,
            ),
            Ok(Ok(_)) => gallery_status(
                &ui,
                "dialog-warning-symbolic",
                "Couldn't load photos",
                "Unexpected reply from the mount service.",
                false,
            ),
            Ok(Err(_)) | Err(_) if append => toast_error(
                &ui,
                "Couldn't load more photos",
                "The mount service didn't respond.",
            ),
            Ok(Err(_)) | Err(_) => gallery_unreachable(&ui),
        }
    });
}

/// Swap the Photos content area to the status page, hiding the pager. Retry is
/// offered only when restarting the mount service could actually fix it.
pub(crate) fn gallery_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.gallery.status.set_icon_name(Some(icon));
    ui.gallery.status.set_title(title);
    ui.gallery.status.set_description(Some(description));
    ui.gallery.retry.set_visible(retry);
    ui.gallery.more.set_visible(false);
    ui.gallery.content.set_visible_child_name("status");
}

/// Photos counterpart of [`browser_unreachable`]: auto-retry while the mount is
/// still starting, surface an actionable error + Retry once it's actually down.
pub(crate) fn gallery_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        gallery_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    gallery_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        if ui.stack.visible_child_name().as_deref() == Some("gallery") {
            load_gallery(&ui, false);
        }
    });
}

/// Play a video with an external player. Prefers `mpv` — it sniffs the container
/// from the bytes, so the cache's extensionless blob plays fine, and it is the
/// right tool for the HEVC `.mkv`s this is aimed at — and falls back to the
/// user's default handler when mpv isn't installed.
pub(crate) fn play_external(path: &str) {
    if Command::new("mpv").arg(path).spawn().is_ok() {
        return;
    }
    open_path(path);
}

/// Download a Photos-library video, then hand it to an external player. Unlike a
/// still photo — which the in-app lightbox can render — a video needs a real
/// player, and the photos volume isn't part of the FUSE mount, so there is no
/// path to stream it from: [`Request::OpenPhoto`] fetches the whole file into the
/// cache (served straight from there on a repeat) and we launch the player on it.
///
/// For large videos kept in an on-demand *drive* folder, streaming through the
/// mount is the better route — that is the file-browser "Play" action, not this.
pub(crate) fn play_video(ui: &Rc<Ui>, uid: String) {
    toast(ui, "Preparing video…");
    let rx = spawn_request(ui.dirs.control_socket(), Request::OpenPhoto { uid });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::FilePath { path })) => play_external(&path),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't open this video", &message, kind)
            }
            Ok(Ok(_)) => toast_error(
                &ui,
                "Couldn't open this video",
                "Unexpected reply from the mount service.",
            ),
            Ok(Err(_)) | Err(_) => {
                toast_error(&ui, "Couldn't open this video", "Couldn't reach Proton Drive.")
            }
        }
    });
}
