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
    /// Target row height in px, retuned by Ctrl+scroll / Ctrl+± (see
    /// [`zoom_gallery`]). Each row is filled with photos at their own aspect
    /// ratios and then scaled to the content width, so this is the height a row
    /// lands near rather than the height it gets (see [`justify_rows`]).
    pub(crate) row_height: Cell<i32>,
    /// Aspect ratios learned by decoding a thumbnail, for photos the daemon had
    /// not recorded one for. Consulted by the layout ahead of
    /// [`PhotoItem::ratio`]: a row laid out at the assumed 1.0 re-flows once the
    /// real shape is known.
    pub(crate) learned_ratios: RefCell<HashMap<String, f64>>,
    /// Photos the layout had to guess a ratio for. Decoding one of these is what
    /// makes a re-flow worth doing; decoding a photo whose ratio was already
    /// known would re-flow the timeline for no visible change.
    pub(crate) assumed_ratios: RefCell<HashSet<String>>,
    /// Swaps the Photos content area between the timeline, its status page, and
    /// the Albums grid.
    pub(crate) content: gtk4::Stack,
    pub(crate) status: adw::StatusPage,
    pub(crate) retry: gtk4::Button,
    /// Spins under the timeline while the next page is loading.
    pub(crate) pager: gtk4::Spinner,
    /// The month scrubber on the timeline's right edge, the months it spans
    /// (newest first), and whether the pointer is on it — while it is, the
    /// scroll position must not move the knob out from under the drag.
    pub(crate) scrubber: gtk4::Scale,
    pub(crate) months: RefCell<Vec<PhotoMonth>>,
    pub(crate) scrubbing: Cell<bool>,
    /// The pending jump the scrubber debounces to.
    pub(crate) scrub_source: RefCell<Option<glib::SourceId>>,
    /// A scrubber jump in progress: the end of the month it is headed for.
    /// Pages keep loading until a photo older than this is in the model.
    pub(crate) jump: Cell<Option<i64>>,
    /// Makes the next page [`JUMP_PAGE`] long, for a jump that has far to go.
    pub(crate) burst: Cell<bool>,
    /// Says a Google Photos import is running, with a way to its page.
    pub(crate) import_banner: adw::Banner,
    pub(crate) upload: gtk4::Button,
    /// Opens the Google Photos Takeout import chooser.
    pub(crate) import: gtk4::Button,
    /// Upload / Import offered on the *empty timeline* status page, so a fresh
    /// account is a place to start rather than a dead end. Hidden on every other
    /// status: a load error is not the moment to offer an upload.
    pub(crate) empty_actions: gtk4::Box,
    /// "Photos", or the album's name while one is open.
    /// "1,204 photos" as its subtitle.
    pub(crate) title: adw::WindowTitle,
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
    /// Set when an album replaced the timeline in [`Self::model`], so going
    /// back to the timeline has to reload it; otherwise the timeline is shown
    /// as it was left, scroll position included.
    pub(crate) timeline_stale: Cell<bool>,
    /// The albums the grid shows, as last listed.
    pub(crate) album_list: RefCell<Vec<AlbumInfo>>,
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
    /// The favorites toggle, and whether it is on. When on, the timeline is
    /// restricted to photos carrying Proton's `Favorite` tag.
    pub(crate) favorites_btn: gtk4::ToggleButton,
    pub(crate) favorites: Cell<bool>,
    /// Set while the date dropdown is being repopulated, so resetting its model
    /// doesn't fire the selection handler and kick off a spurious reload.
    pub(crate) date_suppress: Cell<bool>,
    /// True while a timeline page is in flight, so the scroll-to-the-end paging
    /// can't fire a second request for the page already coming.
    pub(crate) loading: Cell<bool>,
    /// Whether the last page came back full, so there may be more to load.
    pub(crate) has_more: Cell<bool>,
    /// Run once the page in flight has landed in [`Self::model`] — how the
    /// lightbox steps past the last loaded photo.
    pub(crate) page_waiters: RefCell<Vec<Box<dyn FnOnce()>>>,
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
    /// Which way the timeline was last scrolled: `true` for downwards, the
    /// direction [`prefetch_thumbs`] warms tiles in. Photos are read newest
    /// first, so downwards is the common case and the default.
    pub(crate) scrolling_down: Cell<bool>,
    /// The scroll offset the direction was last decided at.
    pub(crate) scroll_offset: Cell<f64>,
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
    /// True while the grid is picking photos rather than opening them. A tile
    /// then toggles instead of activating, and shows a checkbox.
    pub(crate) selecting: Cell<bool>,
    /// The photos picked so far, by uid.
    pub(crate) selected: RefCell<HashSet<String>>,
    /// The Select toggle, the bar it reveals, and the bar's own widgets.
    pub(crate) select_btn: gtk4::ToggleButton,
    pub(crate) select_bar: gtk4::Revealer,
    pub(crate) select_label: gtk4::Label,
    pub(crate) select_trash: gtk4::Button,
    pub(crate) select_album: gtk4::Button,
}

/// How many photos to pull per [`Request::PhotosTimeline`] page.
pub(crate) const PHOTOS_PAGE: usize = 200;

/// Page length while a scrubber jump is loading its way to a month — the
/// daemon's cap on one reply.
pub(crate) const JUMP_PAGE: usize = 1000;

/// How long the scrubber has to rest on a month before the timeline jumps.
const SCRUB_DEBOUNCE: Duration = Duration::from_millis(150);

/// Gallery row height in px: the zoom range, its default, and the step one
/// Ctrl+scroll notch (or Ctrl+±) moves it by. A justified row is scaled to the
/// content width once it is full, so this is the *target* a row lands near
/// rather than the height it ends up with (see [`justify_rows`]).
pub(crate) const ROW_MIN: i32 = 90;

pub(crate) const ROW_MAX: i32 = 340;

pub(crate) const ROW_DEFAULT: i32 = 180;

pub(crate) const ROW_STEP: i32 = 30;

/// Gap between tiles, horizontally and vertically. Tight on purpose: the grid
/// should read as a sheet of photographs, not as a deck of cards.
pub(crate) const TILE_GAP: i32 = 2;

/// The aspect ratios the layout will lay out. A 12:1 panorama laid out honestly
/// is a row of one photo two hundred px tall, and a scan of a strip of film is
/// worse; clamping keeps one odd frame from deciding what a whole row looks
/// like, at the cost of cropping that frame's extremes in its tile.
pub(crate) const RATIO_MIN: f64 = 0.4;

pub(crate) const RATIO_MAX: f64 = 3.0;

/// The ratio a photo gets before anything has seen its pixels. Square is the
/// least wrong guess: it is between the two orientations, so the re-flow when
/// the real ratio lands moves the row as little as possible.
pub(crate) const RATIO_UNKNOWN: f64 = 1.0;

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

/// Decoded thumbnails held in memory, evicted least-recently-*used* first. Each
/// is a few hundred KiB of GPU texture, and at the justified row density 600 was
/// under two screens' worth — a scroll down and back up then had to re-fetch and
/// re-decode everything it had just shown.
pub(crate) const TEXTURE_CACHE_MAX: usize = 1500;

/// How many tiles beyond the realised rows [`prefetch_thumbs`] warms, so a tile
/// is decoded before it is scrolled onto rather than after.
pub(crate) const THUMB_PREFETCH: usize = 32;

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
    /// One row of a day's grid, already justified to the current width and zoom.
    Tiles {
        tiles: Vec<Tile>,
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
            GalleryRow::Tiles { tiles, .. } => tiles.first().map(|t| t.photo.uid.clone()),
        }
    }

    /// Whether re-rendering `self` would produce exactly what `other` shows —
    /// the test [`repaint_gallery`] diffs on, so appending a page only touches
    /// the rows that actually changed.
    fn same_as(&self, other: &GalleryRow) -> bool {
        match (self, other) {
            (GalleryRow::Heading(a), GalleryRow::Heading(b)) => a == b,
            (
                GalleryRow::Tiles { tiles: a, last: al },
                GalleryRow::Tiles { tiles: b, last: bl },
            ) => {
                al == bl
                    && a.len() == b.len()
                    && a.iter().zip(b).all(|(x, y)| {
                        x.photo.uid == y.photo.uid
                            && x.width == y.width
                            && x.height == y.height
                            && x.selecting == y.selecting
                            && x.selected == y.selected
                    })
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
    pub(crate) title: adw::WindowTitle,
    pub(crate) pager: gtk4::Spinner,
    pub(crate) scrubber: gtk4::Scale,
    /// Says a Google Photos import is running, with a way to its page.
    pub(crate) import_banner: adw::Banner,
    pub(crate) list: gtk4::ListView,
    pub(crate) scroll: gtk4::ScrolledWindow,
    pub(crate) retry: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    pub(crate) import: gtk4::Button,
    pub(crate) empty_actions: gtk4::Box,
    pub(crate) empty_upload: gtk4::Button,
    pub(crate) empty_import: gtk4::Button,
    pub(crate) refresh: gtk4::Button,
    /// The Select toggle and the bar it reveals.
    pub(crate) select_btn: gtk4::ToggleButton,
    pub(crate) select_bar: gtk4::Revealer,
    pub(crate) select_label: gtk4::Label,
    pub(crate) select_trash: gtk4::Button,
    pub(crate) select_album: gtk4::Button,
    pub(crate) select_done: gtk4::Button,
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
        .label(gettext("Retry"))
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);

    // The empty-timeline state's two ways forward. `StatusPage` takes one child,
    // so Retry and these share a box and each is shown only for its own state.
    let empty_upload = gtk4::Button::builder()
        .label(gettext("Upload photos"))
        .build();
    empty_upload.add_css_class("pill");
    empty_upload.add_css_class("suggested-action");
    let empty_import = gtk4::Button::builder()
        .label(gettext("Import from Google Photos"))
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

    // The timeline pages itself in as the scroll nears the bottom (see
    // [`wire_gallery`]); this only says a page is on its way.
    let pager = gtk4::Spinner::builder()
        .halign(gtk4::Align::Center)
        .margin_bottom(6)
        .visible(false)
        .build();

    // Month scrubber: newest at the top, a mark per year. Dragging it jumps
    // the timeline to the month under the knob (see [`wire_gallery`]).
    let scrubber = gtk4::Scale::builder()
        .orientation(gtk4::Orientation::Vertical)
        .adjustment(&gtk4::Adjustment::new(0.0, 0.0, 1.0, 1.0, 1.0, 0.0))
        .draw_value(false)
        .value_pos(gtk4::PositionType::Left)
        .round_digits(0)
        .halign(gtk4::Align::End)
        .margin_top(12)
        .margin_bottom(12)
        .margin_end(4)
        .tooltip_text(gettext("Jump to a month"))
        .visible(false)
        .build();
    scrubber.add_css_class("photo-scrubber");

    // Horizontal scrolling is never wanted: the grid is sized to the viewport
    // width, and a stray hscrollbar would fight the layout.
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&list)
        .build();

    // Leaves an open album for the grid it came from. Only an open album shows
    // it — the Albums toggle is what leaves the grid itself.
    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text(gettext("Back to albums"))
        .valign(gtk4::Align::Center)
        .visible(false)
        .build();
    back.add_css_class("flat");
    back.add_css_class("circular");

    // `label` and `icon_name` both replace a button's child, so the pair needs
    // an `adw::ButtonContent` to show together.
    let upload = gtk4::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .label(pgettext("verb", "Upload"))
                .icon_name("list-add-symbolic")
                .build(),
        )
        .tooltip_text(gettext("Upload photos"))
        .valign(gtk4::Align::Center)
        .build();
    upload.add_css_class("suggested-action");

    // Importing an export is a rarer, heavier action than adding one photo, so
    // it sits beside Upload as a plain button rather than a second accent one.
    let import = gtk4::Button::builder()
        .icon_name("folder-download-symbolic")
        .tooltip_text(gettext("Import a Google Photos Takeout export"))
        .valign(gtk4::Align::Center)
        .build();
    import.add_css_class("flat");
    import.add_css_class("circular");
    let refresh = refresh_button();

    // Picking photos is a mode, so its control is a toggle rather than a button.
    let select_btn = gtk4::ToggleButton::builder()
        .icon_name("selection-mode-symbolic")
        .tooltip_text(gettext("Select photos"))
        .valign(gtk4::Align::Center)
        .build();
    select_btn.add_css_class("flat");
    select_btn.add_css_class("circular");

    // What the selection can do, revealed with the mode. A revealer rather than
    // a hidden box so the grid slides down instead of jumping.
    let select_label = gtk4::Label::builder()
        .label(gettext("Select photos"))
        .hexpand(true)
        .xalign(0.0)
        .build();
    let select_trash = gtk4::Button::builder()
        .label(gettext("Move to Trash"))
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    select_trash.add_css_class("destructive-action");
    let select_album = gtk4::Button::builder()
        .label(gettext("Add to Album…"))
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    let select_done = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text(gettext("Leave selection (Esc)"))
        .valign(gtk4::Align::Center)
        .build();
    select_done.add_css_class("flat");
    select_done.add_css_class("circular");
    let select_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    select_box.add_css_class("toolbar");
    select_box.add_css_class("bulk-bar");
    select_box.append(&select_label);
    select_box.append(&select_album);
    select_box.append(&select_trash);
    select_box.append(&select_done);
    let select_bar = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideDown)
        .child(&select_box)
        .build();

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

    // Favorites: a filter, not a tab — it cuts across Photos / Videos / Raw, so
    // it stays outside the segmented control rather than becoming a fifth option
    // that would silently drop the kind the user picked.
    let favorites_btn = gtk4::ToggleButton::builder()
        .icon_name("starred-symbolic")
        .tooltip_text(gettext("Show only favorites"))
        .build();
    favorites_btn.add_css_class("pill");

    // Date jump: "All dates" plus a row per month, filled in once the timeline's
    // months are known (see [`refresh_photo_months`]). Pushed to the far end of
    // the filter row, opposite the kind toggles.
    let dates = gtk4::DropDown::from_strings(&[gettext("All dates").as_str()]);
    dates.add_css_class("pill");
    dates.set_tooltip_text(Some(&gettext("Jump to a month")));

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
    // "Timeline", not "Photos": the kind filter below has a Photos tab too.
    let photos_btn = gtk4::ToggleButton::builder()
        .label(gettext("Timeline"))
        .active(true)
        .build();
    let albums_btn = gtk4::ToggleButton::builder()
        .label(gettext("Albums"))
        .build();
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
    timeline.append(&select_bar);
    let timeline_overlay = gtk4::Overlay::builder().child(&scroll).build();
    timeline_overlay.add_overlay(&scrubber);
    timeline.append(&timeline_overlay);
    timeline.append(&pager);

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
    let new_album = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .label(gettext("New Album…"))
        .halign(gtk4::Align::Start)
        .action_name("win.new-album")
        .build();
    new_album.add_css_class("pill");
    let albums_page = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    albums_page.append(&new_album);
    albums_page.append(&albums_stack);

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&timeline, Some("timeline"));
    content.add_named(&status, Some("status"));
    content.add_named(&albums_page, Some("albums"));

    let import_banner = adw::Banner::builder()
        .title(gettext("Importing from Google Photos…"))
        .button_label(pgettext("verb", "View"))
        .action_name("win.show-import")
        .build();

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&view_switch);
    inner.append(&filter_bar);
    inner.append(&content);

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    body.append(&import_banner);
    body.append(&inner);
    let (frame, header, title) = page_frame(&gettext("Photos"), &body);
    header.pack_start(&back);
    header.pack_start(&upload);
    header.pack_end(&refresh);
    header.pack_end(&select_btn);
    header.pack_end(&import);

    (
        frame.upcast(),
        GalleryWidgets {
            model,
            groups,
            content,
            status,
            title,
            pager,
            scrubber,
            import_banner,
            list,
            scroll,
            retry,
            upload,
            import,
            empty_actions,
            empty_upload,
            empty_import,
            refresh,
            select_btn,
            select_bar,
            select_label,
            select_trash,
            select_album,
            select_done,
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

/// "June 2024": a calendar month in the user's language, for the date jump
/// and the scrubber. The month name comes from glib, so it follows the locale.
pub(crate) fn month_label(year: i32, month: i32) -> String {
    // Translators: strftime format for a month on its own, such as "June 2024". %OB is the month name in its standalone (nominative) form; use %B if your language has no separate form.
    let format = gettext("%OB %Y");
    glib::DateTime::from_local(year, month, 1, 0, 0, 0.0)
        .and_then(|date| date.format(&format))
        .map(|label| label.to_string())
        .unwrap_or_else(|_| format!("{year}-{month:02}"))
}

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
        let mut labels = vec![gettext("All dates")];
        let mut ranges: Vec<Option<(i64, i64)>> = vec![None];
        fill_scrubber(&ui, &months);
        for m in months {
            let count = m.count.to_string();
            let month = month_label(m.year, m.month);
            // Translators: a row of the date jump; {month} is a month such as "June 2024", {count} how many photos it holds.
            let label = gettext_f("{month} ({count})", &[("month", &month), ("count", &count)]);
            labels.push(label);
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

pub(crate) fn wire_gallery(
    ui: &Rc<Ui>,
    list: &gtk4::ListView,
    scroll: &gtk4::ScrolledWindow,
    select_done: &gtk4::Button,
) {
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
                row_box.set_margin_top(10);
                row_box.set_margin_bottom(4);
                let label = gtk4::Label::builder()
                    .label(heading)
                    .halign(gtk4::Align::Start)
                    .build();
                label.add_css_class("heading");
                label.add_css_class("gallery-day");
                row_box.append(&label);
            }
            GalleryRow::Tiles { tiles, last } => {
                row_box.set_margin_top(0);
                // The last row of a day carries the gap to the next heading.
                row_box.set_margin_bottom(if *last { 4 } else { TILE_GAP });
                for tile in tiles {
                    row_box.append(&photo_tile(&ui_bind, tile.clone()));
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
            && let GalleryRow::Tiles { tiles, .. } = &*obj.borrow::<GalleryRow>()
        {
            let mut wanted = ui_unbind.gallery.thumb_wanted.borrow_mut();
            for tile in tiles {
                wanted.remove(&tile.photo.uid);
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

    // Page the timeline in while the user is still a screen and a half from the
    // end, so scrolling at a normal pace never reaches it.
    let ui_scroll = ui.clone();
    scroll.vadjustment().connect_value_changed(move |adj| {
        let last = ui_scroll.gallery.scroll_offset.replace(adj.value());
        if (adj.value() - last).abs() > 1.0 {
            ui_scroll.gallery.scrolling_down.set(adj.value() > last);
        }
        let near_end = adj.value() + adj.page_size() >= adj.upper() - adj.page_size() * 1.5;
        if near_end && ui_scroll.gallery.has_more.get() {
            load_gallery(&ui_scroll, true);
        }
        sync_scrubber_position(&ui_scroll);
    });
    wire_scrubber(ui);

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
        zoom_gallery(&ui_zoom, if dy < 0.0 { ROW_STEP } else { -ROW_STEP });
        glib::Propagation::Stop
    });
    scroll.add_controller(zoom_scroll);

    // Ctrl+plus / Ctrl+minus / Ctrl+0, the keyboard equivalents, and the two
    // keys selection mode owns.
    let zoom_keys = gtk4::EventControllerKey::new();
    let ui_keys = ui.clone();
    zoom_keys.connect_key_pressed(move |_, key, _code, state| {
        if ui_keys.gallery.selecting.get() {
            match key.name().as_deref() {
                Some("Escape") => {
                    set_selection_mode(&ui_keys, false);
                    return glib::Propagation::Stop;
                }
                Some("Delete" | "KP_Delete") => {
                    delete_selected(&ui_keys);
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
        }
        if !state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
            return glib::Propagation::Proceed;
        }
        match key.name().as_deref() {
            Some("plus" | "equal" | "KP_Add") => zoom_gallery(&ui_keys, ROW_STEP),
            Some("minus" | "KP_Subtract") => zoom_gallery(&ui_keys, -ROW_STEP),
            Some("0" | "KP_0") => set_gallery_tile(&ui_keys, ROW_DEFAULT),
            Some("a" | "A") => select_all_photos(&ui_keys),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    list.add_controller(zoom_keys);

    let ui_select = ui.clone();
    ui.gallery.select_btn.clone().connect_toggled(move |btn| {
        set_selection_mode(&ui_select, btn.is_active());
    });
    let ui_trash = ui.clone();
    let ui_album = ui.clone();
    ui.gallery.select_album.clone().connect_clicked(move |_| {
        let uids: Vec<String> = ui_album.gallery.selected.borrow().iter().cloned().collect();
        prompt_add_to_album(&ui_album, uids);
    });
    ui.gallery.select_trash.clone().connect_clicked(move |_| {
        delete_selected(&ui_trash);
    });
    let ui_done = ui.clone();
    select_done
        .clone()
        .connect_clicked(move |_| set_selection_mode(&ui_done, false));

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

    // Favorites: reload the timeline restricted to favorites (or back to all).
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
            .title(gettext("Select Photo to Upload"))
            .build();

        let filter = gtk4::FileFilter::new();
        filter.set_name(Some(&gettext("Images")));
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
                            toast(&ui_clone, &gettext("Photo uploaded"));
                        }
                        Ok(Ok(Response::Error { message, kind })) => {
                            toast_failure(
                                &ui_clone,
                                &gettext("Couldn't upload photo"),
                                &message,
                                kind,
                            );
                        }
                        _ => {
                            toast_error(
                                &ui_clone,
                                &gettext("Couldn't upload photo"),
                                &gettext("The mount service didn't respond."),
                            );
                        }
                    }
                });
            }
        });
    });
}

/// One tile of the grid: the photo, and the box [`justify_rows`] sized it to.
#[derive(Clone)]
pub(crate) struct Tile {
    pub(crate) photo: PhotoItem,
    pub(crate) width: i32,
    pub(crate) height: i32,
    /// Whether the grid is in selection mode, and whether this photo is picked.
    /// Carried on the tile rather than read at bind time so the row diff in
    /// [`repaint_gallery`] sees a selection change and rebuilds that row.
    pub(crate) selecting: bool,
    pub(crate) selected: bool,
}

/// The aspect ratio to lay `photo` out at: what a decode has learned, else what
/// the daemon remembered, else square.
fn tile_ratio(ui: &Rc<Ui>, photo: &PhotoItem) -> f64 {
    let known = ui
        .gallery
        .learned_ratios
        .borrow()
        .get(&photo.uid)
        .copied()
        .or(photo.ratio)
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0);
    match known {
        Some(ratio) => ratio.clamp(RATIO_MIN, RATIO_MAX),
        None => {
            ui.gallery
                .assumed_ratios
                .borrow_mut()
                .insert(photo.uid.clone());
            RATIO_UNKNOWN
        }
    }
}

/// Break one day's photos into justified rows spanning `width` — each photo at
/// its own aspect ratio, each row scaled so it ends exactly on the right margin.
///
/// This is what removes the black bars: a square tile had to either crop the
/// photo or letterbox it, and with a box of the right shape there is nothing to
/// do either way.
pub(crate) fn justify_rows(ui: &Rc<Ui>, photos: &[PhotoItem], width: i32) -> Vec<Vec<Tile>> {
    let ratios: Vec<f64> = photos.iter().map(|photo| tile_ratio(ui, photo)).collect();
    let selecting = ui.gallery.selecting.get();
    let selected = ui.gallery.selected.borrow();
    let mut photos = photos.iter();
    plan_rows(&ratios, width, ui.gallery.row_height.get())
        .into_iter()
        .map(|(height, widths)| {
            widths
                .into_iter()
                .filter_map(|width| {
                    photos.next().map(|photo| Tile {
                        selecting,
                        selected: selecting && selected.contains(&photo.uid),
                        photo: photo.clone(),
                        width,
                        height,
                    })
                })
                .collect()
        })
        .collect()
}

/// The layout math, over aspect ratios alone: each row's height, and the width
/// of every tile in it.
///
/// Photos are taken in order until their summed ratio no longer leaves them
/// `target` px tall, and that row is then scaled to land on `width` exactly. The
/// *last* row of a day is left at the target height instead of being stretched:
/// a day holding two photos would otherwise be two enormous tiles, which reads
/// as a layout bug rather than as a short day.
pub(crate) fn plan_rows(ratios: &[f64], width: i32, target: i32) -> Vec<(i32, Vec<i32>)> {
    let width = width.max(ROW_MIN);
    let target = target.clamp(ROW_MIN, ROW_MAX);
    let mut rows = Vec::new();
    let mut start = 0;
    let mut sum = 0.0;

    for (index, ratio) in ratios.iter().enumerate() {
        sum += ratio;
        let count = (index + 1 - start) as i32;
        // The gaps are fixed, so only the pixels left over from them scale.
        let usable = (width - TILE_GAP * (count - 1)).max(1) as f64;
        let height = usable / sum;
        if height <= target as f64 {
            let height = (height.round() as i32).max(1);
            rows.push((height, fit_row(&ratios[start..=index], width, height)));
            start = index + 1;
            sum = 0.0;
        }
    }
    if start < ratios.len() {
        let tail = &ratios[start..];
        let widths = tail
            .iter()
            .map(|ratio| ((ratio * target as f64).round() as i32).max(1))
            .collect();
        rows.push((target, widths));
    }
    rows
}

/// The tile widths of one full row at `height`, adjusted so the row spans
/// `width` to the pixel.
///
/// Rounding each tile independently leaves a few px of slack, which at a tight
/// gap is visible as a ragged right margin; the last tile absorbs it.
pub(crate) fn fit_row(ratios: &[f64], width: i32, height: i32) -> Vec<i32> {
    let mut widths: Vec<i32> = ratios
        .iter()
        .map(|ratio| ((ratio * height as f64).round() as i32).max(1))
        .collect();
    let gaps = TILE_GAP * (widths.len() as i32 - 1);
    let used: i32 = widths.iter().sum::<i32>() + gaps;
    if let Some(last) = widths.last_mut() {
        *last = (*last + (width - used)).max(1);
    }
    widths
}

/// The width the grid is laid out to: the ListView's own width, less a couple of
/// px so a rounding error can't push a row into a horizontal overflow.
/// Falls back to a sane guess before the first allocation.
pub(crate) fn gallery_width(ui: &Rc<Ui>) -> i32 {
    match ui.gallery.width.get() {
        0 => 900,
        w => (w - 2).max(ROW_MIN),
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
        // The tile is already the photo's shape, so Contain shows the whole
        // frame with nothing cropped and nothing letterboxed. Cover would still
        // crop here: a thumbnail's ratio is not exactly the ratio the row was
        // laid out at, and the rounding is enough to shave an edge. The expands
        // are what make the picture take the whole overlay.
        .content_fit(gtk4::ContentFit::Contain)
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

    // A shot stored as more than one file says so, in the corner the caption
    // does not use. "RAW" is the useful word when one of the members is a raw
    // file — that is what the person wants to find — and a plain count covers
    // the rest (a live photo, a burst).
    if tile.photo.has_raw || tile.photo.group_size > 1 {
        let badge = gtk4::Label::builder()
            .label(if tile.photo.has_raw {
                // Translators: badge on a photo tile whose shot includes a raw camera file.
                gettext("RAW")
            } else {
                format!("{}", tile.photo.group_size)
            })
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Start)
            .build();
        badge.add_css_class("photo-group-badge");
        overlay.add_overlay(&badge);
    }

    // Selection mode marks every tile, picked or not: a check that only appears
    // once a photo is chosen leaves the user guessing what else is clickable.
    if tile.selecting {
        let check = gtk4::Image::builder()
            .icon_name(if tile.selected {
                "checkbox-checked-symbolic"
            } else {
                "checkbox-symbolic"
            })
            .pixel_size(16)
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .margin_start(6)
            .margin_top(6)
            .build();
        check.add_css_class("photo-check");
        if tile.selected {
            check.add_css_class("photo-check-on");
        }
        overlay.add_overlay(&check);
    }

    let button = gtk4::Button::builder()
        .child(&overlay)
        .width_request(tile.width)
        .height_request(tile.height)
        .tooltip_text(format_capture_time(tile.photo.capture_time))
        .build();
    button.add_css_class("photo-tile");
    button.add_css_class("flat");
    if tile.selected {
        button.add_css_class("photo-tile-selected");
    }
    // Clip the thumbnail to the tile's rounded corners.
    button.set_overflow(gtk4::Overflow::Hidden);

    want_thumb(ui, &tile.photo, &picture);

    // Ctrl or Shift on a plain click starts a selection — the gesture people
    // already use for picking things, without having to find the Select button
    // first. Claimed in the capture phase so the click never also opens the
    // photo it was picking.
    let modifier = gtk4::GestureClick::new();
    modifier.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let ui_modifier = ui.clone();
    let modifier_uid = tile.photo.uid.clone();
    modifier.connect_pressed(move |gesture, _, _, _| {
        let state = gesture.current_event_state().intersects(
            gtk4::gdk::ModifierType::CONTROL_MASK | gtk4::gdk::ModifierType::SHIFT_MASK,
        );
        if !state || ui_modifier.gallery.selecting.get() {
            return;
        }
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        set_selection_mode(&ui_modifier, true);
        toggle_selected(&ui_modifier, &modifier_uid);
    });
    button.add_controller(modifier);

    let context = gtk4::GestureClick::builder().button(3).build();
    let ui_context = ui.clone();
    let photo = tile.photo.clone();
    context.connect_pressed(move |gesture, _, x, y| {
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        if let Some(anchor) = gesture.widget().and_downcast::<gtk4::Button>() {
            show_photo_menu(&ui_context, &photo, &anchor, x, y);
        }
    });
    button.add_controller(context);

    // A tile opens in the in-app lightbox, which plays a video in place. In
    // selection mode a tile picks instead of opening: the lightbox is one
    // Escape away.
    let ui_open = ui.clone();
    let uid = tile.photo.uid.clone();
    button.connect_clicked(move |_| {
        if ui_open.gallery.selecting.get() {
            toggle_selected(&ui_open, &uid);
        } else {
            open_photo_viewer(&ui_open, uid.clone());
        }
    });
    button
}

/// Enter or leave selection mode, repainting the tiles so their checkboxes
/// appear or go. Leaving drops the selection: a hidden selection that a later
/// Delete would act on is a trap.
pub(crate) fn set_selection_mode(ui: &Rc<Ui>, selecting: bool) {
    if ui.gallery.selecting.get() == selecting {
        return;
    }
    ui.gallery.selecting.set(selecting);
    if !selecting {
        ui.gallery.selected.borrow_mut().clear();
    }
    if ui.gallery.select_btn.is_active() != selecting {
        ui.gallery.select_btn.set_active(selecting);
    }
    sync_selection_bar(ui);
    repaint_gallery(ui);
}

/// Pick or unpick one photo.
fn toggle_selected(ui: &Rc<Ui>, uid: &str) {
    {
        let mut selected = ui.gallery.selected.borrow_mut();
        if !selected.remove(uid) {
            selected.insert(uid.to_string());
        }
    }
    sync_selection_bar(ui);
    repaint_gallery(ui);
}

/// Reflect the selection in the bar above the grid.
fn sync_selection_bar(ui: &Rc<Ui>) {
    let count = ui.gallery.selected.borrow().len();
    ui.gallery.select_label.set_label(&match count {
        0 => gettext("Select photos"),
        // Translators: {n} is how many items are selected: files and folders in My Files, photos here.
        n => ngettext_f("{n} selected", "{n} selected", n as u64, &[]),
    });
    ui.gallery.select_trash.set_sensitive(count > 0);
    ui.gallery.select_album.set_sensitive(count > 0);
    ui.gallery
        .select_bar
        .set_reveal_child(ui.gallery.selecting.get());
}

/// Confirm, then move every selected photo to Proton trash.
/// How many files the selected tiles stand for. A tile whose photo the model no
/// longer holds counts as one, which is what it looks like on screen.
fn selected_file_count(ui: &Rc<Ui>, uids: &[String]) -> usize {
    let wanted: HashSet<&str> = uids.iter().map(String::as_str).collect();
    let mut files = 0;
    for idx in 0..ui.gallery.model.n_items() {
        let Some(boxed) = ui.gallery.model.item(idx).and_downcast::<BoxedAnyObject>() else {
            continue;
        };
        let photo = boxed.borrow::<PhotoItem>();
        if wanted.contains(photo.uid.as_str()) {
            files += photo.group_size.max(1) as usize;
        }
    }
    files.max(uids.len())
}

pub(crate) fn delete_selected(ui: &Rc<Ui>) {
    let uids: Vec<String> = ui.gallery.selected.borrow().iter().cloned().collect();
    confirm_trash_photos(ui, uids);
}

/// Confirm, then move the photos behind `uids` to Proton trash.
pub(crate) fn confirm_trash_photos(ui: &Rc<Ui>, uids: Vec<String>) {
    if uids.is_empty() {
        return;
    }
    // A selected tile can stand for more than one file — a shot kept as RAW and
    // JPEG — and all of them go. The dialog says so rather than letting the user
    // find out from the trash.
    let files = selected_file_count(ui, &uids);
    let body = match (uids.len(), files) {
        (1, 1) => gettext("Move this photo to Trash?"),
        (1, files) => ngettext_f(
            "Move this photo to Trash? It is stored as {n} file.",
            "Move this photo to Trash? It is stored as {n} files.",
            files as u64,
            &[],
        ),
        (n, files) if files == n => ngettext_f(
            "Move {n} photo to Trash?",
            "Move {n} photos to Trash?",
            n as u64,
            &[],
        ),
        // Translators: {n} is how many photos are selected, {files} how many files they are stored as (always more than {n}).
        (n, files) => ngettext_f(
            "Move {n} photo to Trash? They are stored as {files} files.",
            "Move {n} photos to Trash? They are stored as {files} files.",
            n as u64,
            &[("files", &files.to_string())],
        ),
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Move to Trash"))
        .body(body)
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("trash", &gettext("Move to Trash"));
    dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let win = ui_window(ui);
    let ui = ui.clone();
    dialog.connect_response(None, move |_, response| {
        if response == "trash" {
            trash_photos(&ui, uids.clone());
        }
    });
    dialog.present(win.as_ref());
}

/// Move photos to Proton trash, taking them out of the grid straight away.
///
/// The removal is optimistic because the alternative — a grid that keeps showing
/// photos the user just deleted until a refresh lands — reads as a failure. What
/// the server refuses comes back, so the grid still ends up telling the truth,
/// and what succeeded is one Undo away for as long as the toast is up.
pub(crate) fn trash_photos(ui: &Rc<Ui>, uids: Vec<String>) {
    let removed = remove_photos(ui, &uids);
    set_selection_mode(ui, false);
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::TrashNodes { uids: uids.clone() },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let reply = rx.recv().await;
        ui.busy_end();
        match reply {
            Ok(Ok(Response::Trashed { trashed, failed })) => {
                // Put back whatever stayed on the server, in its timeline
                // position rather than at the end.
                if !failed.is_empty() {
                    let kept: Vec<PhotoItem> = removed
                        .iter()
                        .filter(|photo| failed.iter().any(|f| f.uid == photo.uid))
                        .cloned()
                        .collect();
                    restore_photos(&ui, kept);
                    let message = failed
                        .first()
                        .map(|f| f.message.clone())
                        .unwrap_or_else(|| gettext("The server refused."));
                    toast_error(
                        &ui,
                        &gettext("Some photos couldn't be moved to Trash"),
                        &message,
                    );
                }
                if trashed.is_empty() {
                    return;
                }
                let count = trashed.len();
                let message = ngettext_f(
                    "Moved {n} photo to Trash",
                    "Moved {n} photos to Trash",
                    count as u64,
                    &[],
                );
                toast_action(&ui, &message, &gettext("Undo"), move |ui| {
                    restore_uids(ui, trashed.clone(), count);
                    load_gallery(ui, false);
                });
            }
            Ok(Ok(Response::Error { message, kind })) => {
                restore_photos(&ui, removed.clone());
                toast_failure(&ui, &gettext("Couldn't move to Trash"), &message, kind);
            }
            _ => {
                restore_photos(&ui, removed.clone());
                toast_error(
                    &ui,
                    &gettext("Couldn't move to Trash"),
                    &gettext("The mount service didn't respond."),
                );
            }
        }
    });
}

/// Take photos out of the loaded model, returning them so a failure can put them
/// back.
pub(crate) fn remove_photos(ui: &Rc<Ui>, uids: &[String]) -> Vec<PhotoItem> {
    let model = &ui.gallery.model;
    let mut removed = Vec::new();
    let mut index = 0;
    while index < model.n_items() {
        let item = model
            .item(index)
            .and_downcast::<BoxedAnyObject>()
            .map(|obj| obj.borrow::<PhotoItem>().clone());
        match item {
            Some(photo) if uids.contains(&photo.uid) => {
                model.remove(index);
                removed.push(photo);
            }
            _ => index += 1,
        }
    }
    repaint_gallery(ui);
    removed
}

/// Put photos back into the loaded model, at their place in the timeline.
pub(crate) fn restore_photos(ui: &Rc<Ui>, photos: Vec<PhotoItem>) {
    let model = &ui.gallery.model;
    for photo in photos {
        // The timeline is newest first, so a photo belongs before the first
        // entry older than it.
        let at = (0..model.n_items())
            .find(|index| {
                model
                    .item(*index)
                    .and_downcast::<BoxedAnyObject>()
                    .is_some_and(|obj| obj.borrow::<PhotoItem>().capture_time < photo.capture_time)
            })
            .unwrap_or(model.n_items());
        model.insert(at, &BoxedAnyObject::new(photo));
    }
    repaint_gallery(ui);
}

/// Give `picture` its thumbnail: straight from the texture cache when it's there,
/// otherwise register the tile as waiting and get the thumbnail moving — decoding
/// it if the daemon already had it cached on disk, or asking the daemon for it.
///
/// This is what makes the gallery on-demand: only tiles the ListView actually
/// realises ever ask for an image.
pub(crate) fn want_thumb(ui: &Rc<Ui>, photo: &PhotoItem, picture: &gtk4::Picture) {
    let cached = ui.gallery.photo_tex.borrow().get(&photo.uid).cloned();
    if let Some(texture) = cached {
        picture.set_paintable(Some(&texture));
        ui.touch_texture(&photo.uid);
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

/// Send one [`Request::PhotoThumbs`] batch for the tiles on screen, topped up
/// with the tiles just beyond them.
///
/// A queued uid whose tile has scrolled away is dropped while the queue is long,
/// because during a fast scroll what the user is looking at *now* is the only
/// thing worth the round trip. Once the queue is short the scroll has settled,
/// and those near misses are the tiles one flick away — fetching them costs one
/// batch and saves a blank tile, and the reply lands in the texture cache
/// whether or not a widget still wants it.
pub(crate) fn flush_thumbs(ui: &Rc<Ui>) {
    if ui.gallery.thumb_inflight.get() {
        return;
    }
    let uids: Vec<String> = {
        let mut queue = ui.gallery.thumb_queue.borrow_mut();
        let wanted = ui.gallery.thumb_wanted.borrow();
        let settled = queue.len() <= THUMB_BATCH * 2;
        let mut batch = Vec::new();
        let mut skipped = VecDeque::new();
        while batch.len() < THUMB_BATCH {
            let Some(uid) = queue.pop_front() else { break };
            if wanted.contains_key(&uid) || settled {
                batch.push(uid);
            } else {
                skipped.push_back(uid);
            }
        }
        // Anything skipped over stays queued behind what was taken: the scroll
        // may still come back to it.
        for uid in skipped.into_iter().rev() {
            queue.push_front(uid);
        }
        batch
    };
    let uids = prefetch_thumbs(ui, uids);
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

/// Top a batch up with the photos just past the rows on screen, in the direction
/// the timeline is being scrolled.
///
/// A tile that starts its fetch when it is bound is a tile that shows a
/// placeholder for as long as the round trip takes. Warming the next screen is
/// what makes a steady scroll look like it already has the photos.
fn prefetch_thumbs(ui: &Rc<Ui>, mut batch: Vec<String>) -> Vec<String> {
    if batch.is_empty() || batch.len() >= THUMB_BATCH {
        return batch;
    }
    let bound = ui.gallery.bound.borrow();
    let (Some(first), Some(last)) = (bound.keys().next(), bound.keys().next_back()) else {
        return batch;
    };
    let rows = &ui.gallery.groups;
    let ahead: Box<dyn Iterator<Item = u32>> = if ui.gallery.scrolling_down.get() {
        Box::new(last.saturating_add(1)..rows.n_items())
    } else {
        Box::new((0..*first).rev())
    };
    drop(bound);

    let cached = ui.gallery.photo_tex.borrow();
    let nothumb = ui.gallery.photo_nothumb.borrow();
    let mut queue = ui.gallery.thumb_queue.borrow_mut();
    let mut warmed = 0;
    for index in ahead {
        if warmed >= THUMB_PREFETCH || batch.len() >= THUMB_BATCH {
            break;
        }
        let Some(obj) = rows.item(index).and_downcast::<BoxedAnyObject>() else {
            continue;
        };
        let GalleryRow::Tiles { tiles, .. } = &*obj.borrow::<GalleryRow>() else {
            continue;
        };
        for tile in tiles {
            let uid = &tile.photo.uid;
            // A photo the daemon already handed us a path for is decoded, not
            // fetched, and one that can never have a thumbnail is neither.
            if tile.photo.thumb_path.is_some() || cached.contains_key(uid) || nothumb.contains(uid)
            {
                continue;
            }
            if batch.contains(uid) || queue.contains(uid) {
                continue;
            }
            warmed += 1;
            if batch.len() < THUMB_BATCH {
                batch.push(uid.clone());
            } else {
                queue.push_back(uid.clone());
            }
        }
    }
    batch
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
            // A thumbnail is the first look anyone gets at the photo's shape.
            // If the row was laid out without it, remember it and re-flow — the
            // debounce means a batch of decodes costs one re-flow, not one
            // each.
            learn_ratio(&ui, &uid, &texture);
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

/// Record the aspect ratio a decoded thumbnail proves, and re-flow if the layout
/// had been guessing.
///
/// Only a photo the layout had to guess about is worth a re-flow: re-recording
/// what the daemon already told us would re-flow the timeline on every scroll.
fn learn_ratio(ui: &Rc<Ui>, uid: &str, texture: &gtk4::gdk::Texture) {
    let (width, height) = (texture.width(), texture.height());
    if width <= 0 || height <= 0 {
        return;
    }
    ui.gallery
        .learned_ratios
        .borrow_mut()
        .insert(uid.to_string(), width as f64 / height as f64);
    if ui.gallery.assumed_ratios.borrow_mut().remove(uid) {
        schedule_relayout(ui);
    }
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
                GalleryRow::Tiles { tiles, .. } => tiles.iter().any(|t| t.photo.uid == uid),
            })
    })
}

/// Step the target row height by `delta` px and re-flow, clamped to the zoom
/// range.
pub(crate) fn zoom_gallery(ui: &Rc<Ui>, delta: i32) {
    set_gallery_tile(ui, ui.gallery.row_height.get() + delta);
}

/// Set the target row height (clamped) and re-flow the visible sections at it.
pub(crate) fn set_gallery_tile(ui: &Rc<Ui>, tile: i32) {
    let tile = tile.clamp(ROW_MIN, ROW_MAX);
    if tile == ui.gallery.row_height.get() {
        return;
    }
    ui.gallery.row_height.set(tile);
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
        let grid = justify_rows(ui, &group.photos, width);
        let last_index = grid.len().saturating_sub(1);
        for (index, tiles) in grid.into_iter().enumerate() {
            rows.push(GalleryRow::Tiles {
                tiles,
                last: index == last_index,
            });
        }
    }
    rows
}

fn clone_row(row: &GalleryRow) -> GalleryRow {
    match row {
        GalleryRow::Heading(heading) => GalleryRow::Heading(heading.clone()),
        GalleryRow::Tiles { tiles, last } => GalleryRow::Tiles {
            tiles: tiles.clone(),
            last: *last,
        },
    }
}

/// "1,204 photos" under the page title.
pub(crate) fn update_gallery_subtitle(ui: &Rc<Ui>) {
    let loaded = ui.gallery.model.n_items() as usize;
    if loaded == 0 {
        ui.gallery.title.set_subtitle("");
    }
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
    if loaded == 0 {
        return;
    }
    ui.gallery.title.set_subtitle(&match total {
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
        return gettext("Unknown date");
    };
    let same_day = |other: &glib::DateTime| {
        other.year() == date.year()
            && other.month() == date.month()
            && other.day_of_month() == date.day_of_month()
    };
    if let Ok(now) = glib::DateTime::now_local() {
        if same_day(&now) {
            return gettext("Today");
        }
        if let Ok(yesterday) = glib::DateTime::from_unix_local(now.to_unix() - 86_400)
            && same_day(&yesterday)
        {
            return gettext("Yesterday");
        }
    }
    // Translators: strftime format for a day heading in the photo timeline, such as "3 June 2026".
    let format = gettext("%-d %B %Y");
    date.format(&format)
        .map(|s| s.to_string())
        .unwrap_or_else(|_| gettext("Unknown date"))
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
    // Translators: strftime format for a photo's capture date and time, such as "2026-06-03 14:05:09".
    let format = gettext("%Y-%m-%d %H:%M:%S");
    match date {
        Ok(d) => match d.format(&format) {
            Ok(s) => s.to_string(),
            Err(_) => gettext("Unknown Date"),
        },
        Err(_) => gettext("Unknown Date"),
    }
}

/// The capture time as a tile caption: the clock time alone, since the day is
/// already the section heading right above it.
pub(crate) fn short_capture_time(secs: i64) -> String {
    // Translators: strftime format for the time a photo was taken, shown on its tile, such as "14:05".
    let format = gettext("%H:%M");
    glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format(&format))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Fetch a timeline page from the daemon. When `append` is false the model is
/// cleared first (fresh load); otherwise the next page is tacked on.
/// The empty state for a timeline filtered to `kind`, favorites and/or a
/// `month`: what is missing, in the filter's own words.
pub(crate) fn empty_timeline_text(
    kind: Option<PhotoKind>,
    favorites: bool,
    month: Option<&str>,
) -> (String, String) {
    let what = match kind {
        None => "photos",
        Some(PhotoKind::Photo) => "photos",
        Some(PhotoKind::Video) => "videos",
        Some(PhotoKind::Raw) => "raw files",
    };
    let when = month.map(|m| format!(" in {m}")).unwrap_or_default();
    match (favorites, kind, month) {
        (false, None, None) => (
            gettext("No photos yet"),
            gettext("Photos you upload to Proton Drive appear here."),
        ),
        (true, None, None) => (
            gettext("No favorites yet"),
            gettext("Star a photo in the viewer or from its menu to find it here."),
        ),
        (true, _, _) => (
            format!("No favorite {what}{when}"),
            gettext("Turn off the favorites filter to see everything."),
        ),
        (false, _, _) => (
            format!("No {what}{when}"),
            gettext("Try another filter or month."),
        ),
    }
}

/// Point the scrubber at `months` (newest first): one step per month, with a
/// mark and a year label where each year starts. Shown only for the whole,
/// unfiltered timeline with more than one month to move between.
fn fill_scrubber(ui: &Rc<Ui>, months: &[PhotoMonth]) {
    let scrubber = &ui.gallery.scrubber;
    scrubber.clear_marks();
    *ui.gallery.months.borrow_mut() = months.to_vec();
    scrubber.set_range(0.0, months.len().saturating_sub(1).max(1) as f64);
    scrubber.set_value(0.0);
    for (index, pair) in months.windows(2).enumerate() {
        if pair[0].year != pair[1].year {
            scrubber.add_mark(
                (index + 1) as f64,
                gtk4::PositionType::Left,
                Some(&pair[1].year.to_string()),
            );
        }
    }
    sync_scrubber(ui);
}

/// Show the scrubber only where a month jump makes sense: the whole timeline,
/// not an album, a date window or the favorites.
pub(crate) fn sync_scrubber(ui: &Rc<Ui>) {
    let whole = ui.gallery.album.borrow().is_none()
        && ui.gallery.range.get().is_none()
        && !ui.gallery.favorites.get();
    ui.gallery
        .scrubber
        .set_visible(whole && ui.gallery.months.borrow().len() > 1);
}

/// "March 2024" for the scrubber's month `index`.
fn scrubber_label(months: &[PhotoMonth], index: usize) -> String {
    months
        .get(index)
        .map_or_else(String::new, |m| month_label(m.year, m.month))
}

/// Move the knob to the month at the top of the timeline, unless the user is
/// holding it.
fn sync_scrubber_position(ui: &Rc<Ui>) {
    if ui.gallery.scrubbing.get() || !ui.gallery.scrubber.is_visible() {
        return;
    }
    let Some(uid) = ui.gallery.bound.borrow().values().flatten().next().cloned() else {
        return;
    };
    let Some(capture_time) = find_photo_index(&ui.gallery.model, &uid)
        .and_then(|idx| ui.gallery.model.item(idx))
        .and_downcast::<BoxedAnyObject>()
        .map(|boxed| boxed.borrow::<PhotoItem>().capture_time)
    else {
        return;
    };
    let months = ui.gallery.months.borrow();
    if let Some(index) = month_index(&months, capture_time) {
        ui.gallery.scrubber.set_value(index as f64);
    }
}

/// Which of `months` (newest first) `capture_time` falls in; the nearest one
/// when it falls between them.
pub(crate) fn month_index(months: &[PhotoMonth], capture_time: i64) -> Option<usize> {
    let position = months
        .iter()
        .position(|m| month_range(m.year, m.month).is_some_and(|(from, _)| capture_time >= from));
    match position {
        Some(index) => Some(index),
        None => months.len().checked_sub(1),
    }
}

/// Wire the scrubber: its value label while the pointer is on it, and the
/// jump once it rests on a month.
fn wire_scrubber(ui: &Rc<Ui>) {
    let scrubber = ui.gallery.scrubber.clone();
    let ui_format = ui.clone();
    scrubber.set_format_value_func(move |_, value| {
        scrubber_label(&ui_format.gallery.months.borrow(), value.round() as usize)
    });

    let hover = gtk4::EventControllerMotion::new();
    let ui_enter = ui.clone();
    hover.connect_enter(move |_, _, _| {
        ui_enter.gallery.scrubbing.set(true);
        ui_enter.gallery.scrubber.set_draw_value(true);
    });
    let ui_leave = ui.clone();
    hover.connect_leave(move |_| {
        ui_leave.gallery.scrubbing.set(false);
        ui_leave.gallery.scrubber.set_draw_value(false);
    });
    scrubber.add_controller(hover);

    // `change-value` fires for the user's own moves only, not for the knob
    // following the scroll.
    let ui_change = ui.clone();
    scrubber.connect_change_value(move |_, _, value| {
        if let Some(source) = ui_change.gallery.scrub_source.borrow_mut().take() {
            source.remove();
        }
        let index = value.round().max(0.0) as usize;
        let ui_jump = ui_change.clone();
        let source = glib::timeout_add_local_once(SCRUB_DEBOUNCE, move || {
            ui_jump.gallery.scrub_source.borrow_mut().take();
            jump_to_month(&ui_jump, index);
        });
        *ui_change.gallery.scrub_source.borrow_mut() = Some(source);
        glib::Propagation::Proceed
    });
}

/// Scroll the timeline to the scrubber's month `index`, loading pages until
/// its first photo is in.
fn jump_to_month(ui: &Rc<Ui>, index: usize) {
    let end = {
        let months = ui.gallery.months.borrow();
        months
            .get(index)
            .and_then(|m| month_range(m.year, m.month))
            .map(|(_, to)| to)
    };
    ui.gallery.jump.set(end);
    continue_jump(ui);
}

/// One step of a scrubber jump: scroll there when the month is loaded, or load
/// a long page and come back when it lands.
fn continue_jump(ui: &Rc<Ui>) {
    let Some(end) = ui.gallery.jump.get() else {
        return;
    };
    let model = &ui.gallery.model;
    // Newest first, so the month's first photo is the first one older than
    // its end.
    let landing = (0..model.n_items()).find(|idx| {
        model
            .item(*idx)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|boxed| boxed.borrow::<PhotoItem>().capture_time < end)
    });
    if landing.is_none() && ui.gallery.has_more.get() {
        let ui_page = ui.clone();
        ui.gallery
            .page_waiters
            .borrow_mut()
            .push(Box::new(move || continue_jump(&ui_page)));
        ui.gallery.burst.set(true);
        load_gallery(ui, true);
        return;
    }
    ui.gallery.jump.set(None);
    let target = landing.or_else(|| model.n_items().checked_sub(1));
    let Some(uid) = target
        .and_then(|idx| model.item(idx))
        .and_downcast::<BoxedAnyObject>()
        .map(|boxed| boxed.borrow::<PhotoItem>().uid.clone())
    else {
        return;
    };
    let Some(row) = row_of_photo(&ui.gallery.groups, &uid) else {
        return;
    };
    // Land on the day heading above the photo when it opens a day.
    let heading = row
        .checked_sub(1)
        .filter(|above| {
            ui.gallery
                .groups
                .item(*above)
                .and_downcast::<BoxedAnyObject>()
                .is_some_and(|boxed| {
                    matches!(*boxed.borrow::<GalleryRow>(), GalleryRow::Heading(_))
                })
        })
        .unwrap_or(row);
    ui.gallery
        .list
        .scroll_to(heading, gtk4::ListScrollFlags::empty(), None);
}

/// Load the next page when the timeline does not fill the window yet: with no
/// scrollbar there is no scrolling to trigger it.
fn fill_viewport(ui: &Rc<Ui>) {
    let ui = ui.clone();
    glib::idle_add_local_once(move || {
        let Some(adj) = ui.gallery.list.vadjustment() else {
            return;
        };
        if ui.gallery.has_more.get() && adj.upper() <= adj.page_size() + 1.0 {
            load_gallery(&ui, true);
        }
    });
}

/// Select every photo loaded so far (Ctrl+A).
pub(crate) fn select_all_photos(ui: &Rc<Ui>) {
    set_selection_mode(ui, true);
    {
        let mut selected = ui.gallery.selected.borrow_mut();
        for idx in 0..ui.gallery.model.n_items() {
            if let Some(boxed) = ui.gallery.model.item(idx).and_downcast::<BoxedAnyObject>() {
                selected.insert(boxed.borrow::<PhotoItem>().uid.clone());
            }
        }
    }
    sync_selection_bar(ui);
    repaint_gallery(ui);
}

/// A tile's right-click menu.
fn show_photo_menu(ui: &Rc<Ui>, photo: &PhotoItem, anchor: &gtk4::Button, x: f64, y: f64) {
    let mut menu = ActionMenu::new();
    let (ui_c, uid) = (ui.clone(), photo.uid.clone());
    let label = if photo.kind == PhotoKind::Video {
        pgettext("verb", "Play")
    } else {
        pgettext("verb", "Open")
    };
    menu.item(&label, move || open_photo_viewer(&ui_c, uid.clone()));
    let (ui_c, uid) = (ui.clone(), photo.uid.clone());
    // Translators: a check item in a photo's menu, on while the photo is a favorite.
    let favorite_label = pgettext("state", "Favorite");
    menu.toggle(&favorite_label, photo.favorite, move |favorite| {
        set_photo_favorite(&ui_c, uid.clone(), favorite)
    });
    let (ui_c, uid) = (ui.clone(), photo.uid.clone());
    menu.item(&pgettext("verb", "Select"), move || {
        set_selection_mode(&ui_c, true);
        toggle_selected(&ui_c, &uid);
    });
    menu.section();
    let (ui_c, uid) = (ui.clone(), photo.uid.clone());
    menu.item(&gettext("Add to Album…"), move || {
        prompt_add_to_album(&ui_c, vec![uid.clone()])
    });
    let open_album = ui.gallery.album.borrow().clone();
    if let Some(album) = open_album.filter(|album| !album.shared) {
        let (ui_c, uid) = (ui.clone(), photo.uid.clone());
        menu.item(&gettext("Remove from Album"), move || {
            remove_from_album(&ui_c, &album, vec![uid.clone()])
        });
    }
    menu.section();
    let (ui_c, uid) = (ui.clone(), photo.uid.clone());
    menu.item(&gettext("Move to Trash…"), move || {
        confirm_trash_photos(&ui_c, vec![uid.clone()])
    });
    menu.popup_at(anchor, x, y);
}

/// Star or unstar a photo from the grid, and show the new state.
fn set_photo_favorite(ui: &Rc<Ui>, uid: String, favorite: bool) {
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::SetPhotoFavorite {
            uid: uid.clone(),
            favorite,
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => {
                set_gallery_favorite(&ui, &uid, favorite);
                repaint_gallery(&ui);
                toast(
                    &ui,
                    &if favorite {
                        gettext("Added to Favorites")
                    } else {
                        gettext("Removed from Favorites")
                    },
                );
            }
            Ok(Ok(Response::Error { message, .. })) => {
                toast_error(&ui, &gettext("Couldn't change the favorite"), &message)
            }
            _ => toast_error(
                &ui,
                &gettext("Couldn't change the favorite"),
                &gettext("The mount service didn't respond."),
            ),
        }
    });
}

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
            &gettext("Loading photos…"),
            &gettext("Reading your Proton Drive timeline."),
            false,
        );
        // Rebuild the date jump for the current kind, but only for a full-span
        // load — a jump *to* a month sets a range and reloads, and refreshing the
        // dropdown then would fight the selection the user just made. An album
        // has no date jump at all.
        if album.is_none() && ui.gallery.range.get().is_none() {
            refresh_photo_months(ui);
        }
        sync_scrubber(ui);
    }
    let offset = ui.gallery.model.n_items() as usize;
    let limit = if ui.gallery.burst.replace(false) {
        JUMP_PAGE
    } else {
        PHOTOS_PAGE
    };
    ui.gallery.loading.set(true);
    ui.gallery.pager.set_visible(append);
    ui.gallery.pager.set_spinning(append);

    ui.busy_begin();
    let request = match album {
        Some(uid) => Request::AlbumPhotos { uid, offset, limit },
        None => Request::PhotosTimeline {
            offset,
            limit,
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
        ui.gallery.pager.set_visible(false);
        ui.gallery.pager.set_spinning(false);
        // Whoever waited on this page runs once the reply below has put it in
        // the model, whichever way that goes.
        let waiters = std::mem::take(&mut *ui.gallery.page_waiters.borrow_mut());
        if !waiters.is_empty() {
            glib::idle_add_local_once(move || waiters.into_iter().for_each(|waiter| waiter()));
        }
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
                        &gettext("No photo library"),
                        &gettext("This Proton account doesn't have Photos enabled."),
                        false,
                    );
                    return;
                }
                ui.gallery.has_more.set(items.len() == limit);
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
                    let filtered = ui.gallery.kind.get().is_some()
                        || ui.gallery.favorites.get()
                        || ui.gallery.range.get().is_some();
                    let (title, description) = if ui.gallery.album.borrow().is_some() {
                        (
                            gettext("Empty album"),
                            gettext("This album has no photos in it."),
                        )
                    } else {
                        let month = ui
                            .gallery
                            .range
                            .get()
                            .and_then(|_| ui.gallery.dates.selected_item())
                            .and_downcast::<gtk4::StringObject>()
                            .map(|month| month.string().to_string());
                        empty_timeline_text(
                            ui.gallery.kind.get(),
                            ui.gallery.favorites.get(),
                            month.as_deref(),
                        )
                    };
                    gallery_status(
                        &ui,
                        if ui.gallery.favorites.get() {
                            "starred-symbolic"
                        } else {
                            "image-x-generic-symbolic"
                        },
                        &title,
                        &description,
                        false,
                    );
                    // Only the whole, unfiltered timeline offers Upload: an
                    // empty *album* is filled by adding existing photos to it,
                    // and an empty filter is not a library that needs photos.
                    ui.gallery
                        .empty_actions
                        .set_visible(ui.gallery.album.borrow().is_none() && !filtered);
                    return;
                }
                ui.gallery.content.set_visible_child_name("timeline");
                fill_viewport(&ui);
            }
            // A failed *next* page keeps the photos already on screen — the failure
            // goes to a toast rather than wiping the timeline for a status page.
            Ok(Ok(Response::Error { message, .. })) if append => {
                toast_error(&ui, &gettext("Couldn't load more photos"), &message)
            }
            Ok(Ok(Response::Error { message, .. })) => gallery_status(
                &ui,
                "dialog-warning-symbolic",
                &gettext("Couldn't load photos"),
                &message,
                false,
            ),
            Ok(Ok(_)) => gallery_status(
                &ui,
                "dialog-warning-symbolic",
                &gettext("Couldn't load photos"),
                &gettext("Unexpected reply from the mount service."),
                false,
            ),
            Ok(Err(_)) | Err(_) if append => toast_error(
                &ui,
                &gettext("Couldn't load more photos"),
                &gettext("The mount service didn't respond."),
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
    ui.gallery.has_more.set(false);
    ui.gallery.content.set_visible_child_name("status");
}

/// Photos counterpart of [`browser_unreachable`]: auto-retry while the mount is
/// still starting, surface an actionable error + Retry once it's actually down.
pub(crate) fn gallery_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        gallery_status(
            ui,
            "network-offline-symbolic",
            &gettext("Not connected"),
            &gettext("The Proton Drive mount service isn't running."),
            true,
        );
        return;
    }
    gallery_status(
        ui,
        "folder-remote-symbolic",
        &gettext("Connecting…"),
        &gettext("Waiting for the Proton Drive mount service to come up."),
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

#[cfg(test)]
mod tests {
    use super::{empty_timeline_text, month_index, month_range};
    use pdfs_core::control::{PhotoKind, PhotoMonth};

    #[test]
    fn the_scrubber_finds_the_month_a_photo_was_taken_in() {
        let months = [(2026, 3), (2025, 12), (2025, 11)].map(|(year, month)| PhotoMonth {
            year,
            month,
            count: 1,
        });
        let mid = |year, month| month_range(year, month).unwrap().0 + 86_400 * 10;
        assert_eq!(month_index(&months, mid(2026, 3)), Some(0));
        assert_eq!(month_index(&months, mid(2025, 12)), Some(1));
        // A month with no photos of its own lands on the next older one.
        assert_eq!(month_index(&months, mid(2026, 1)), Some(1));
        assert_eq!(month_index(&months, mid(2020, 1)), Some(2));
    }

    #[test]
    fn an_empty_filter_names_what_it_filtered() {
        assert_eq!(empty_timeline_text(None, false, None).0, "No photos yet");
        assert_eq!(empty_timeline_text(None, true, None).0, "No favorites yet");
        assert_eq!(
            empty_timeline_text(Some(PhotoKind::Video), false, Some("June 2024")).0,
            "No videos in June 2024"
        );
        assert_eq!(
            empty_timeline_text(Some(PhotoKind::Raw), true, None).0,
            "No favorite raw files"
        );
    }
}
