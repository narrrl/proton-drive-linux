use crate::*;

pub(crate) struct GalleryState {
    // Photos (gallery) page.
    /// Every photo loaded so far, newest first — the order the lightbox's
    /// prev/next walks. The visible day sections are derived from this.
    pub(crate) model: gio::ListStore,
    /// The Photos ListView's own model: one item per *rendered row* — a day
    /// heading, or one row of that day's tile grid — rebuilt from
    /// [`Self::model`] by [`repaint_gallery`]. Rows rather than whole days
    /// because the ListView only realises what is on screen, and a day holding
    /// 1,600 photos (a Takeout import lands one) would otherwise be 1,600
    /// widgets built in a single bind.
    pub(crate) groups: gio::ListStore,
    /// Target tile edge in px, retuned by Ctrl+scroll / Ctrl+± (see
    /// [`zoom_gallery`]). The grid fits as many square tiles of about this size
    /// as the content width holds.
    pub(crate) tile: Cell<i32>,
    /// Swaps the Photos content area between the timeline, its status page, and
    /// the Albums grid.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    pub(crate) more: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    /// Opens the Google Photos Takeout import chooser.
    pub(crate) import: gtk4::Button,
    /// Upload / Import offered on the *empty timeline* status page, so a fresh
    /// account is a place to start rather than a dead end. Hidden on every other
    /// status: a load error is not the moment to offer an upload.
    pub(crate) empty_actions: gtk4::Box,
    /// "Photos", or the album's name while one is open.
    pub(crate) title: gtk4::Label,
    /// "1,204 photos" under the page title.
    pub(crate) subtitle: gtk4::Label,
    /// The album grid, and the stack that swaps it for its own status page.
    pub(crate) albums: gtk4::FlowBox,
    pub(crate) albums_stack: gtk4::Stack,
    pub(crate) albums_status: adw::StatusPage,
    /// The Photos/Albums switcher, and the box holding it — hidden while an
    /// album is open, where the back button leads instead.
    pub(crate) photos_btn: gtk4::ToggleButton,
    pub(crate) albums_btn: gtk4::ToggleButton,
    pub(crate) view_switch: gtk4::Box,
    /// True while the album listing is in flight, so a re-entry into the Albums
    /// view can't stack requests.
    pub(crate) albums_loading: Cell<bool>,
    /// The album currently open, or `None` when the timeline is showing. Set by
    /// [`open_album`]; read by [`load_gallery`], which pages that album instead
    /// of the timeline while it is set.
    pub(crate) album: RefCell<Option<AlbumInfo>>,
    /// Leaves an open album for the grid it was opened from. Visible only while
    /// an album is open.
    pub(crate) back: gtk4::Button,
    /// The kind toggles and the date jump, hidden while an album is open — an
    /// album page is served whole, not filtered.
    pub(crate) filters: gtk4::Box,
    /// Which tab (Photos / Videos / Raw) the timeline is filtered to, or `None`
    /// for All. Read by [`load_gallery`] and set by the filter toggles.
    pub(crate) kind: Cell<Option<PhotoKind>>,
    /// The filter toggles, index-aligned with [`kind_for_tab`], kept so their
    /// labels can carry live per-kind counts.
    pub(crate) tabs: [gtk4::ToggleButton; 4],
    /// Whole-timeline `(photos, videos, raw)` counts from the last page that
    /// carried them, so the subtitle can say how big the library *is* rather
    /// than how much of it has been paged in.
    pub(crate) counts: Cell<Option<(usize, usize, usize)>>,
    /// The date-jump dropdown ("All dates" then a month per timeline entry), and
    /// the `[from, to)` window each of its rows selects (index-aligned; `None` is
    /// "All dates"). Selecting a row loads that month via [`load_gallery`].
    pub(crate) dates: gtk4::DropDown,
    pub(crate) date_ranges: RefCell<Vec<Option<(i64, i64)>>>,
    /// The capture-time window the timeline is currently filtered to, or `None`
    /// for the whole span. Read by [`load_gallery`], set by the date dropdown.
    pub(crate) range: Cell<Option<(i64, i64)>>,
    /// The favourites toggle, and whether it is on. When on, the timeline is
    /// restricted to photos carrying Proton's `Favorite` tag.
    pub(crate) favorites_btn: gtk4::ToggleButton,
    pub(crate) favorites: Cell<bool>,
    /// Set while the date dropdown is being repopulated, so resetting its model
    /// doesn't fire the selection handler and kick off a spurious reload.
    pub(crate) date_suppress: Cell<bool>,
    /// True while a timeline page is in flight, so the scroll-to-the-end paging
    /// can't fire a second request for the page already coming.
    pub(crate) loading: Cell<bool>,
    /// Content width the grid is currently laid out to. Updated when the
    /// ListView is resized, which re-flows the visible sections.
    pub(crate) width: Cell<i32>,
    /// Decoded thumbnails by photo uid, with the insertion order that evicts the
    /// oldest past [`TEXTURE_CACHE_MAX`]. Scrolling back over a day therefore
    /// repaints from memory instead of re-decoding from disk.
    pub(crate) photo_tex: RefCell<HashMap<String, gtk4::gdk::Texture>>,
    pub(crate) photo_tex_order: RefCell<VecDeque<String>>,
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
    /// re-flow, replaced on each new trigger so only the last one fires.
    pub(crate) thumb_source: RefCell<Option<glib::SourceId>>,
    pub(crate) relayout_source: RefCell<Option<glib::SourceId>>,
    /// The rows currently realised by the ListView, as row index -> the uid of
    /// its first photo (absent for a heading row). A resize or a zoom step
    /// changes how many tiles fit per row, so the row model has to be rebuilt —
    /// this is what lets the rebuild put the user back where they were.
    pub(crate) bound: RefCell<BTreeMap<u32, Option<String>>>,
    /// The ListView itself, so a rebuild can scroll back to the row the user was
    /// looking at.
    pub(crate) list: gtk4::ListView,
}

/// How many photos to pull per [`Request::PhotosTimeline`] page.
pub(crate) const PHOTOS_PAGE: usize = 60;

/// Gallery tile edge in px: the zoom range, its default, and the step one
/// Ctrl+scroll notch (or Ctrl+±) moves it by. The grid divides the content width
/// evenly, so this is the *target* a tile lands near rather than its exact size
/// (see [`plan_grid`]).
pub(crate) const TILE_MIN: i32 = 90;

pub(crate) const TILE_MAX: i32 = 340;

pub(crate) const TILE_DEFAULT: i32 = 180;

pub(crate) const TILE_STEP: i32 = 30;

/// Gap between tiles, horizontally and vertically.
pub(crate) const TILE_GAP: i32 = 8;

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

/// Pause after a resize/zoom before the visible sections are re-flowed.
pub(crate) const RELAYOUT_DEBOUNCE: Duration = Duration::from_millis(80);

/// One day-section of the photos timeline: a heading plus the photos captured
/// that day, in timeline order. Built from the flat [`Ui::gallery_model`] by
/// [`group_photos`], then flattened into [`GalleryRow`]s for rendering.
pub(crate) struct PhotoGroup {
    /// "Today", "Yesterday", or e.g. "3 June 2026".
    pub(crate) heading: String,
    pub(crate) photos: Vec<PhotoItem>,
}

/// One item of the ListView's model — the unit the gallery virtualises at.
///
/// A whole day is *not* one item: a Google Photos Takeout drops thousands of
/// photos onto a single date, and one ListView item per day means GTK builds
/// every one of those tiles in one bind, on the main thread, before the row can
/// be shown. Splitting the day into its grid rows keeps a bind to a handful of
/// widgets no matter how big the day is.
pub(crate) enum GalleryRow {
    /// A day heading: "Today", "3 June 2026".
    Heading(String),
    /// One row of a day's grid, already laid out to the current width and zoom.
    Tiles {
        photos: Vec<PhotoItem>,
        edge: i32,
        /// True for the last row of its day, which carries the section's bottom
        /// margin so days stay visually separated.
        last: bool,
    },
}

impl GalleryRow {
    /// The uid of the row's first photo, or `None` for a heading — the anchor a
    /// relayout scrolls back to.
    fn anchor(&self) -> Option<String> {
        match self {
            GalleryRow::Heading(_) => None,
            GalleryRow::Tiles { photos, .. } => photos.first().map(|p| p.uid.clone()),
        }
    }

    /// Whether re-rendering `self` would produce exactly what `other` shows —
    /// the test [`repaint_gallery`] diffs on, so appending a page only touches
    /// the rows that actually changed.
    fn same_as(&self, other: &GalleryRow) -> bool {
        match (self, other) {
            (GalleryRow::Heading(a), GalleryRow::Heading(b)) => a == b,
            (
                GalleryRow::Tiles {
                    photos: a,
                    edge: ae,
                    last: al,
                },
                GalleryRow::Tiles {
                    photos: b,
                    edge: be,
                    last: bl,
                },
            ) => {
                ae == be
                    && al == bl
                    && a.len() == b.len()
                    && a.iter().zip(b).all(|(x, y)| x.uid == y.uid)
            }
            _ => false,
        }
    }
}

/// The widgets [`build_gallery_page`] hands back to [`build_window`].
pub(crate) struct GalleryWidgets {
    /// Flat, newest-first list of every loaded photo. Backs the lightbox's
    /// prev/next navigation; the visible sections are derived from it.
    pub(crate) model: gio::ListStore,
    /// Day sections rendered by the ListView, derived from `model`.
    pub(crate) groups: gio::ListStore,
    /// Swaps between the timeline, the empty/loading/error status page, and the
    /// Albums grid.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) title: gtk4::Label,
    pub(crate) subtitle: gtk4::Label,
    pub(crate) more: gtk4::Button,
    pub(crate) list: gtk4::ListView,
    pub(crate) scroll: gtk4::ScrolledWindow,
    pub(crate) retry: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    pub(crate) import: gtk4::Button,
    pub(crate) empty_actions: gtk4::Box,
    pub(crate) empty_upload: gtk4::Button,
    pub(crate) empty_import: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
    /// The Albums grid, its own status page and the stack between them, plus the
    /// Photos/Albums switcher and the back button out of an album.
    pub(crate) albums: gtk4::FlowBox,
    pub(crate) albums_stack: gtk4::Stack,
    pub(crate) albums_status: adw::StatusPage,
    pub(crate) photos_btn: gtk4::ToggleButton,
    pub(crate) albums_btn: gtk4::ToggleButton,
    pub(crate) view_switch: gtk4::Box,
    pub(crate) back: gtk4::Button,
    /// The kind toggles and date jump, as one box so an album view can hide them.
    pub(crate) filters: gtk4::Box,
    /// The All / Photos / Videos / Raw filter toggles, in that order (index maps
    /// to [`kind_for_tab`]).
    pub(crate) tabs: [gtk4::ToggleButton; 4],
    pub(crate) favorites_btn: gtk4::ToggleButton,
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

    // The empty-timeline state's two ways forward. `StatusPage` takes one child,
    // so Retry and these share a box and each is shown only for its own state.
    let empty_upload = gtk4::Button::builder().label("Upload photos").build();
    empty_upload.add_css_class("pill");
    empty_upload.add_css_class("suggested-action");
    let empty_import = gtk4::Button::builder()
        .label("Import from Google Photos")
        .build();
    empty_import.add_css_class("pill");
    let empty_actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    empty_actions.set_halign(gtk4::Align::Center);
    empty_actions.set_visible(false);
    empty_actions.append(&empty_upload);
    empty_actions.append(&empty_import);

    let status_child = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    status_child.append(&retry);
    status_child.append(&empty_actions);

    let status = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .vexpand(true)
        .child(&status_child)
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

    // Horizontal scrolling is never wanted: the grid is sized to the viewport
    // width, and a stray hscrollbar would fight the layout.
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&list)
        .build();

    let title_label = gtk4::Label::builder()
        .label("Gallery")
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

    // Leaves an open album for the grid it came from. Only an open album shows
    // it — the Albums toggle is what leaves the grid itself.
    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back to albums")
        .valign(gtk4::Align::Center)
        .visible(false)
        .build();
    back.add_css_class("flat");
    back.add_css_class("circular");

    let upload = gtk4::Button::builder()
        .label("Upload")
        .icon_name("list-add-symbolic")
        .valign(gtk4::Align::Center)
        .build();
    upload.add_css_class("pill");
    upload.add_css_class("suggested-action");

    // Importing an export is a rarer, heavier action than adding one photo, so
    // it sits beside Upload as a plain button rather than a second accent one.
    let import = gtk4::Button::builder()
        .icon_name("folder-download-symbolic")
        .tooltip_text("Import a Google Photos Takeout export")
        .valign(gtk4::Align::Center)
        .build();
    import.add_css_class("flat");
    import.add_css_class("circular");
    let refresh = refresh_button();

    let header_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header_box.append(&back);
    header_box.append(&titles);
    header_box.append(&refresh);
    header_box.append(&import);
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

    // Favourites: a filter, not a tab — it cuts across Photos / Videos / Raw, so
    // it stays outside the segmented control rather than becoming a fifth option
    // that would silently drop the kind the user picked.
    let favorites_btn = gtk4::ToggleButton::builder()
        .icon_name("starred-symbolic")
        .tooltip_text("Show only favourites")
        .build();
    favorites_btn.add_css_class("pill");

    // Date jump: "All dates" plus a row per month, filled in once the timeline's
    // months are known (see [`refresh_photo_months`]). Pushed to the far end of
    // the filter row, opposite the kind toggles.
    let dates = gtk4::DropDown::from_strings(&["All dates"]);
    dates.add_css_class("pill");
    dates.set_tooltip_text(Some("Jump to a month"));

    // The kind toggles and the date jump travel together: they filter the
    // timeline, and neither applies to the album grid or to an open album.
    let filters = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    filters.set_hexpand(true);
    filters.append(&tab_group);
    filters.append(&favorites_btn);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    filters.append(&spacer);
    filters.append(&dates);

    // The page's two views, as one segmented control under the title: the
    // timeline, and the albums. This is navigation, not a filter — which is why
    // it sits above the filter row rather than beside the kind toggles.
    let photos_btn = gtk4::ToggleButton::builder()
        .label("Photos")
        .active(true)
        .build();
    let albums_btn = gtk4::ToggleButton::builder().label("Albums").build();
    albums_btn.set_group(Some(&photos_btn));
    let view_switch = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    view_switch.add_css_class("linked");
    view_switch.add_css_class("view-switch");
    view_switch.set_halign(gtk4::Align::Start);
    for btn in [&photos_btn, &albums_btn] {
        btn.add_css_class("pill");
        view_switch.append(btn);
    }

    let filter_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    filter_bar.append(&filters);

    // The timeline (plus its pager) or the status page, never both.
    let timeline = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    timeline.append(&scroll);
    timeline.append(&more);

    // The album grid: cover-first cards that flow to the width they are given.
    let albums = gtk4::FlowBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .homogeneous(true)
        .row_spacing(TILE_GAP as u32 * 2)
        .column_spacing(TILE_GAP as u32 * 2)
        .min_children_per_line(2)
        .max_children_per_line(8)
        .valign(gtk4::Align::Start)
        .build();
    let albums_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&albums)
        .build();
    let albums_status = adw::StatusPage::builder()
        .icon_name("view-grid-symbolic")
        .vexpand(true)
        .build();
    albums_status.add_css_class("compact");
    let albums_stack = gtk4::Stack::new();
    albums_stack.set_vexpand(true);
    albums_stack.add_named(&albums_scroll, Some("grid"));
    albums_stack.add_named(&albums_status, Some("status"));

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&timeline, Some("timeline"));
    content.add_named(&status, Some("status"));
    content.add_named(&albums_stack, Some("albums"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header_box);
    inner.append(&view_switch);
    inner.append(&filter_bar);
    inner.append(&content);

    (
        inner.upcast(),
        GalleryWidgets {
            model,
            groups,
            content,
            status,
            title: title_label,
            subtitle,
            more,
            list,
            scroll,
            retry,
            upload,
            import,
            empty_actions,
            empty_upload,
            empty_import,
            refresh,
            tabs,
            favorites_btn,
            dates,
            albums,
            albums_stack,
            albums_status,
            photos_btn,
            albums_btn,
            view_switch,
            back,
            filters,
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
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
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
            let name = MONTH_NAMES
                .get((m.month - 1) as usize)
                .copied()
                .unwrap_or("?");
            labels.push(format!("{name} {} ({})", m.year, m.count));
            ranges.push(month_range(m.year, m.month));
        }
        let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

        // Repopulating resets the selection to 0, which would otherwise fire the
        // handler and reload; suppress that — the caller is already reloading.
        ui.gallery.date_suppress.set(true);
        ui.gallery
            .dates
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
    ui.gallery.counts.set(Some(counts));
    let (photos, videos, raw) = counts;
    let totals = [photos + videos + raw, photos, videos, raw];
    for (index, tab) in ui.gallery.tabs.iter().enumerate() {
        let name = ["All", "Photos", "Videos", "Raw"][index];
        let n = totals[index];
        tab.set_label(&format!("{name}  {}", thousands(n)));
        tab.set_sensitive(n > 0 || tab.is_active());
    }
}

/// `1422` as `1,422`. Six-figure libraries are ordinary, and an unseparated run
/// of digits in a tab label is unreadable at a glance.
pub(crate) fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Wire the gallery: install the section factory, the zoom gestures, the pager
/// and the upload button. Activating a thumbnail downloads the photo and opens it
/// in the in-app lightbox.
/// Wire the empty-timeline buttons. Upload re-fires the header's own Upload
/// rather than duplicating its file-chooser, so the two can't drift apart.
pub(crate) fn wire_gallery_empty(ui: &Rc<Ui>, upload: &gtk4::Button, import: &gtk4::Button) {
    let ui_upload = ui.clone();
    upload.connect_clicked(move |_| ui_upload.gallery.upload.emit_clicked());
    let ui_import = ui.clone();
    import.connect_clicked(move |_| ui_import.stack.set_visible_child_name("takeout"));
}

pub(crate) fn wire_gallery(ui: &Rc<Ui>, list: &gtk4::ListView, scroll: &gtk4::ScrolledWindow) {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        // One container for both row kinds: the bind fills it with either a
        // heading label or a strip of tiles. The alternative — two factories, or
        // a Stack per row — buys nothing, since a row's children are rebuilt on
        // bind either way.
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, TILE_GAP);
        row.set_halign(gtk4::Align::Start);
        item.set_child(Some(&row));
        item.set_activatable(false);
    });

    let ui_bind = ui.clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let row_box = item.child().and_downcast::<gtk4::Box>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let row = obj.borrow::<GalleryRow>();

        while let Some(child) = row_box.first_child() {
            row_box.remove(&child);
        }
        match &*row {
            GalleryRow::Heading(heading) => {
                row_box.set_margin_top(16);
                row_box.set_margin_bottom(8);
                let label = gtk4::Label::builder()
                    .label(heading)
                    .halign(gtk4::Align::Start)
                    .build();
                label.add_css_class("heading");
                label.add_css_class("gallery-day");
                row_box.append(&label);
            }
            GalleryRow::Tiles { photos, edge, last } => {
                row_box.set_margin_top(0);
                // The last row of a day carries the gap to the next heading.
                row_box.set_margin_bottom(if *last { 8 } else { TILE_GAP });
                for photo in photos {
                    row_box.append(&photo_tile(
                        &ui_bind,
                        Tile {
                            photo: photo.clone(),
                            edge: *edge,
                        },
                    ));
                }
                schedule_thumbs(&ui_bind);
            }
        }
        ui_bind
            .gallery
            .bound
            .borrow_mut()
            .insert(item.position(), row.anchor());
    });

    // ListView recycles row widgets, so a scrolled-away row must give up its
    // claim on them — otherwise a thumbnail landing late would paint into a tile
    // that now shows a different photo.
    let ui_unbind = ui.clone();
    factory.connect_unbind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        ui_unbind
            .gallery
            .bound
            .borrow_mut()
            .remove(&item.position());
        if let Some(obj) = item.item().and_downcast::<BoxedAnyObject>()
            && let GalleryRow::Tiles { photos, .. } = &*obj.borrow::<GalleryRow>()
        {
            let mut wanted = ui_unbind.gallery.thumb_wanted.borrow_mut();
            for photo in photos {
                wanted.remove(&photo.uid);
            }
        }
    });
    list.set_factory(Some(&factory));

    // The grid divides the content width, so a resize re-flows whatever is on
    // screen (offscreen sections pick the new width up when they bind).
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

    // Favourites: reload the timeline restricted to favourites (or back to all).
    // Independent of the kind tabs and the date jump, both of which keep their
    // current value across the toggle.
    let ui_fav = ui.clone();
    ui.gallery.favorites_btn.connect_toggled(move |btn| {
        let on = btn.is_active();
        if ui_fav.gallery.favorites.get() == on {
            return;
        }
        ui_fav.gallery.favorites.set(on);
        load_gallery(&ui_fav, false);
    });

    // Date jump: selecting a month loads that window; "All dates" (row 0) clears
    // it. Skipped while the model is being repopulated (see `gallery_date_suppress`).
    let ui_dates = ui.clone();
    ui.gallery.dates.connect_selected_notify(move |dd| {
        if ui_dates.gallery.date_suppress.get() {
            return;
        }
        let range = ui_dates
            .gallery
            .date_ranges
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

    // The import is a staged, hours-long migration, so the button hands over to
    // the Import page rather than opening a file chooser here: the archives need
    // reviewing before anything is sent, and the run needs somewhere to report.
    let ui_import = ui.clone();
    ui.gallery
        .import
        .connect_clicked(move |_| ui_import.stack.set_visible_child_name("takeout"));

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
                    // The daemon opens the file itself: it runs on this machine,
                    // and a photo's bytes through line-delimited JSON is an OOM
                    // of both processes (see `Request::UploadPhoto`).
                    Request::UploadPhoto {
                        name,
                        media_type: media_type.to_string(),
                        source_path: path.display().to_string(),
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

/// One tile of the grid: the photo, and the square edge it was sized to.
pub(crate) struct Tile {
    pub(crate) photo: PhotoItem,
    pub(crate) edge: i32,
}

/// Break one day's photos into rows of equal square tiles that span `width` —
/// the layout a phone gallery uses, and the reason a day holding two photos
/// looks like every other day rather than like a mistake.
///
/// Each tile is centre-cropped to its square (the full frame is one click away
/// in the lightbox), so nothing here depends on knowing a photo's aspect ratio
/// and a row never has to be re-flowed when a thumbnail finally lands.
pub(crate) fn grid_rows(ui: &Rc<Ui>, photos: &[PhotoItem], width: i32) -> Vec<Vec<Tile>> {
    let (columns, edge) = plan_grid(ui.gallery.tile.get(), width);
    photos
        .chunks(columns)
        .map(|row| {
            row.iter()
                .map(|photo| Tile {
                    photo: photo.clone(),
                    edge,
                })
                .collect()
        })
        .collect()
}

/// The grid math: how many columns fit in `width` at roughly `target` px per
/// tile, and the exact square edge that divides the width between them.
///
/// The column count is what rounds — the edge then absorbs the remainder, so the
/// grid spans the full width at every window size instead of leaving a ragged
/// margin. Never fewer than one column, however narrow the window gets.
pub(crate) fn plan_grid(target: i32, width: i32) -> (usize, i32) {
    let avail = width.max(TILE_MIN);
    let target = target.clamp(TILE_MIN, TILE_MAX);
    // A row of n tiles occupies n*edge + (n-1)*gap, so n tiles of the target size
    // fit while n*(target + gap) - gap <= avail.
    let columns = ((avail + TILE_GAP) / (target + TILE_GAP)).max(1) as usize;
    let gaps = TILE_GAP * (columns as i32 - 1);
    let edge = ((avail - gaps) / columns as i32).max(1);
    (columns, edge)
}

/// The width the grid is laid out to: the ListView's own width, less a couple of
/// px so a rounding error can't push a row into a horizontal overflow.
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
        // Cover fills the square and crops the overflow, so a portrait photo
        // sits flush in its tile instead of floating in letterbox bars. The
        // expands are what make the picture take the whole overlay: without
        // them it is allocated its natural size and the crop never happens.
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    // Zoomed on hover by the stylesheet; the tile clips the overflow.
    picture.add_css_class("photo-thumb");

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
        .width_request(tile.edge)
        .height_request(tile.edge)
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

    ui.gallery
        .thumb_wanted
        .borrow_mut()
        .insert(photo.uid.clone(), picture.clone());

    match photo.thumb_path.as_deref() {
        Some(path) => {
            ui.gallery
                .decode_queue
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
        for (uid, path) in batch {
            let texture = match gtk4::gdk::Texture::from_filename(&path) {
                Ok(texture) => texture,
                Err(e) => {
                    tracing::debug!("cannot decode thumbnail {path}: {e}");
                    ui.gallery.photo_nothumb.borrow_mut().insert(uid);
                    continue;
                }
            };
            ui.store_texture(&uid, texture.clone());
            if let Some(picture) = ui.gallery.thumb_wanted.borrow_mut().remove(&uid) {
                picture.set_paintable(Some(&texture));
            }
        }

        if ui.gallery.decode_queue.borrow().is_empty() {
            ui.gallery.decode_idle.set(false);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// Re-flow the sections on screen shortly. Debounced, because the triggers (a
/// window resize, a zoom step) arrive in floods and only the final state
/// matters.
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

/// Re-flow the timeline at the current width and zoom.
///
/// A different column count means different rows, so unlike the old
/// section-per-day model there is nothing to patch in place — the row model is
/// rebuilt. What that would cost the user is their scroll position, so the
/// topmost realised row's first photo is remembered and scrolled back to.
pub(crate) fn relayout_gallery(ui: &Rc<Ui>) {
    let anchor: Option<String> = ui.gallery.bound.borrow().values().flatten().next().cloned();
    repaint_gallery(ui);
    let Some(anchor) = anchor else { return };
    let Some(row) = row_of_photo(&ui.gallery.groups, &anchor) else {
        return;
    };
    ui.gallery
        .list
        .scroll_to(row, gtk4::ListScrollFlags::empty(), None);
}

/// Which row of the rendered model holds `uid`, if any.
fn row_of_photo(store: &gio::ListStore, uid: &str) -> Option<u32> {
    (0..store.n_items()).find(|i| {
        store
            .item(*i)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|obj| match &*obj.borrow::<GalleryRow>() {
                GalleryRow::Heading(_) => false,
                GalleryRow::Tiles { photos, .. } => photos.iter().any(|p| p.uid == uid),
            })
    })
}

/// Step the tile size by `delta` px and re-flow, clamped to the zoom range.
pub(crate) fn zoom_gallery(ui: &Rc<Ui>, delta: i32) {
    set_gallery_tile(ui, ui.gallery.tile.get() + delta);
}

/// Set the tile size (clamped) and re-flow the visible sections at it.
pub(crate) fn set_gallery_tile(ui: &Rc<Ui>, tile: i32) {
    let tile = tile.clamp(TILE_MIN, TILE_MAX);
    if tile == ui.gallery.tile.get() {
        return;
    }
    ui.gallery.tile.set(tile);
    schedule_relayout(ui);
}

/// Rebuild the ListView's row model from the flat photo model: each day becomes
/// a heading row followed by its grid rows, laid out to the current width and
/// zoom.
///
/// The rows are diffed into the existing store rather than replacing it: a "load
/// more" only really changes the last day (the one the new page continues) and
/// appends after it, and clearing the store instead would scroll the user back
/// to the top of the timeline at the exact moment they asked for more.
pub(crate) fn repaint_gallery(ui: &Rc<Ui>) {
    let rows = build_rows(ui);
    let store = &ui.gallery.groups;

    for (i, row) in rows.iter().enumerate() {
        let i = i as u32;
        let unchanged = store
            .item(i)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|old| row.same_as(&old.borrow::<GalleryRow>()));
        if unchanged {
            continue;
        }
        let boxed = BoxedAnyObject::new(clone_row(row));
        if i < store.n_items() {
            store.splice(i, 1, &[boxed]);
        } else {
            store.append(&boxed);
        }
    }
    if store.n_items() > rows.len() as u32 {
        let len = rows.len() as u32;
        store.splice(len, store.n_items() - len, &[] as &[BoxedAnyObject]);
    }

    update_gallery_subtitle(ui);
}

/// Flatten the loaded photos into the rows the ListView renders.
fn build_rows(ui: &Rc<Ui>) -> Vec<GalleryRow> {
    let width = gallery_width(ui);
    let mut rows = Vec::new();
    for group in group_photos(&ui.gallery.model) {
        rows.push(GalleryRow::Heading(group.heading));
        let grid = grid_rows(ui, &group.photos, width);
        let last_index = grid.len().saturating_sub(1);
        for (index, tiles) in grid.into_iter().enumerate() {
            let edge = tiles.first().map(|t| t.edge).unwrap_or(TILE_DEFAULT);
            rows.push(GalleryRow::Tiles {
                photos: tiles.into_iter().map(|t| t.photo).collect(),
                edge,
                last: index == last_index,
            });
        }
    }
    rows
}

fn clone_row(row: &GalleryRow) -> GalleryRow {
    match row {
        GalleryRow::Heading(heading) => GalleryRow::Heading(heading.clone()),
        GalleryRow::Tiles { photos, edge, last } => GalleryRow::Tiles {
            photos: photos.clone(),
            edge: *edge,
            last: *last,
        },
    }
}

/// "1,204 photos" under the page title.
fn update_gallery_subtitle(ui: &Rc<Ui>) {
    let loaded = ui.gallery.model.n_items() as usize;
    ui.gallery.subtitle.set_visible(loaded > 0);
    // An album counts what the server says it holds, not how much of it has been
    // paged in — the subtitle would otherwise climb as the user scrolls.
    if ui.gallery.album.borrow().is_some() {
        return;
    }
    // The noun tracks the active filter, so a Videos tab doesn't count "photos".
    let kind = ui.gallery.kind.get();
    let (one, many) = match kind {
        Some(PhotoKind::Video) => ("video", "videos"),
        Some(PhotoKind::Raw) => ("raw photo", "raw photos"),
        _ => ("photo", "photos"),
    };
    // The whole library for this filter, not the page count — the subtitle sits
    // next to tabs carrying the same totals, and the two disagreeing reads as a
    // bug. A date jump is the exception: there the window is the subject.
    let total = match (ui.gallery.range.get(), ui.gallery.counts.get()) {
        (None, Some((photos, videos, raw))) => match kind {
            Some(PhotoKind::Photo) => photos,
            Some(PhotoKind::Video) => videos,
            Some(PhotoKind::Raw) => raw,
            None => photos + videos + raw,
        },
        _ => loaded,
    };
    ui.gallery.subtitle.set_label(&match total {
        1 => format!("1 {one}"),
        n => format!("{} {many}", thousands(n)),
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
    // An open album pages itself instead of the timeline; everything downstream —
    // the model, the sections, the thumbnails, the lightbox — is the same.
    let album = ui.gallery.album.borrow().as_ref().map(|a| a.uid.clone());
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
        // dropdown then would fight the selection the user just made. An album
        // has no date jump at all.
        if album.is_none() && ui.gallery.range.get().is_none() {
            refresh_photo_months(ui);
        }
    }
    let offset = ui.gallery.model.n_items() as usize;
    ui.gallery.loading.set(true);
    ui.gallery.more.set_sensitive(false);

    ui.busy_begin();
    let request = match album {
        Some(uid) => Request::AlbumPhotos {
            uid,
            offset,
            limit: PHOTOS_PAGE,
        },
        None => Request::PhotosTimeline {
            offset,
            limit: PHOTOS_PAGE,
            kind: ui.gallery.kind.get(),
            range: ui.gallery.range.get(),
            favorites: ui.gallery.favorites.get(),
        },
    };
    let rx = spawn_request(ui.dirs.control_socket(), request);
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
                // Take the daemon's word on which photos can never have a
                // thumbnail, so their tiles show a placeholder from the first
                // frame instead of queueing a request that can only fail.
                {
                    let mut nothumb = ui.gallery.photo_nothumb.borrow_mut();
                    for item in items.iter().filter(|item| item.no_thumb) {
                        nothumb.insert(item.uid.clone());
                    }
                }
                for item in &items {
                    ui.gallery.model.append(&BoxedAnyObject::new(item.clone()));
                }
                repaint_gallery(&ui);
                if ui.gallery.model.n_items() == 0 {
                    let (title, description) = if ui.gallery.album.borrow().is_some() {
                        ("Empty album", "This album has no photos in it.")
                    } else {
                        (
                            "No photos yet",
                            "Photos you upload to Proton Drive appear here.",
                        )
                    };
                    gallery_status(&ui, "image-x-generic-symbolic", title, description, false);
                    // Only the whole-timeline empty state: an empty *album* is
                    // filled by adding existing photos to it, not by uploading
                    // into it, so the buttons would point the wrong way.
                    ui.gallery
                        .empty_actions
                        .set_visible(ui.gallery.album.borrow().is_none());
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
    // Only the empty timeline offers a way to fill it; every other status turns
    // those buttons back off.
    ui.gallery.empty_actions.set_visible(false);
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
            Ok(Err(_)) | Err(_) => toast_error(
                &ui,
                "Couldn't open this video",
                "Couldn't reach Proton Drive.",
            ),
        }
    });
}
