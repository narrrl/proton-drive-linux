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
    /// The Places grid and its own status page, like the album grid's, and the
    /// switcher's Places toggle.
    pub(crate) places: gtk4::FlowBox,
    pub(crate) places_stack: gtk4::Stack,
    pub(crate) places_status: adw::StatusPage,
    pub(crate) places_btn: gtk4::ToggleButton,
    /// True while the place listing is in flight.
    pub(crate) places_loading: Cell<bool>,
    /// How many places the grid shows, for the subtitle when back returns there.
    pub(crate) place_count: Cell<usize>,
    /// The place currently open, paged by [`load_gallery`] like an album.
    pub(crate) place: RefCell<Option<PlaceInfo>>,
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
    /// The Filters button, the count badge on it, and the popover's "Clear
    /// filters" button.
    pub(crate) filter_btn: gtk4::MenuButton,
    pub(crate) filter_count: gtk4::Label,
    pub(crate) clear_filters: gtk4::Button,
    /// The month row of the Filters popover ("All dates" then a month per
    /// timeline entry), and the `[from, to)` window each of its rows selects
    /// (index-aligned; `None` is "All dates"). Selecting a row loads that month
    /// via [`load_gallery`].
    pub(crate) dates: adw::ComboRow,
    pub(crate) date_ranges: RefCell<Vec<Option<(i64, i64)>>>,
    /// The capture-time window the timeline is currently filtered to, or `None`
    /// for the whole span. Read by [`load_gallery`], set by the date dropdown.
    pub(crate) range: Cell<Option<(i64, i64)>>,
    /// The favorites switch, and whether it is on. When on, the timeline is
    /// restricted to photos carrying Proton's `Favorite` tag.
    pub(crate) favorites_row: adw::SwitchRow,
    pub(crate) favorites: Cell<bool>,
    /// The album filter switch, and whether it is on. When on, the timeline is
    /// restricted to photos no album of ours holds yet.
    pub(crate) not_in_album_row: adw::SwitchRow,
    pub(crate) not_in_album: Cell<bool>,
    /// The "On this day" strip, the box holding its cards, and whether there
    /// is anything to show in it. The strip is the timeline's first row (see
    /// [`GalleryRow::Memories`]), so it scrolls away with the grid.
    pub(crate) memories: gtk4::Box,
    pub(crate) memory_cards: gtk4::Box,
    pub(crate) has_memories: Cell<bool>,
    /// Set while the date dropdown is being repopulated, so resetting its model
    /// doesn't fire the selection handler and kick off a spurious reload.
    pub(crate) date_suppress: Cell<bool>,
    /// A fresh load was asked for while a page was still loading.
    pub(crate) reload_pending: Cell<bool>,
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
    /// its first photo (absent for a heading row) and the row's widget. A
    /// resize or a zoom step changes how many tiles fit per row, so the row
    /// model has to be rebuilt — this is what lets the rebuild put the user
    /// back where they were. The ListView realises rows beyond both edges of
    /// the viewport, so the widget is what tells which of them is on screen.
    pub(crate) bound: RefCell<BTreeMap<u32, (Option<String>, gtk4::Box)>>,
    /// The ListView itself, so a rebuild can scroll back to the row the user was
    /// looking at.
    pub(crate) list: gtk4::ListView,
    /// True while the grid is picking photos rather than opening them. A tile
    /// then toggles instead of activating, and shows a checkbox.
    pub(crate) selecting: Cell<bool>,
    /// The photos picked so far, by uid.
    pub(crate) selected: RefCell<HashSet<String>>,
    /// The photo last picked or unpicked by a click, which a Shift+click
    /// selects a range from.
    pub(crate) select_anchor: RefCell<Option<String>>,
    /// The Select toggle, the bar it reveals, and the bar's own widgets.
    pub(crate) select_btn: gtk4::ToggleButton,
    pub(crate) select_bar: gtk4::Revealer,
    pub(crate) select_label: gtk4::Label,
    pub(crate) select_trash: gtk4::Button,
    pub(crate) select_album: gtk4::Button,
}

impl GalleryState {
    /// Whether the gallery shows an album or a place rather than the timeline.
    /// Neither is filtered or dated, so the timeline's extras stay away.
    pub(crate) fn in_collection(&self) -> bool {
        self.album.borrow().is_some() || self.place.borrow().is_some()
    }
}

/// How many photos to pull per [`Request::PhotosTimeline`] page.
pub(crate) const PHOTOS_PAGE: usize = 200;

/// Page length while a scrubber jump is loading its way to a month — the
/// daemon's cap on one reply.
pub(crate) const JUMP_PAGE: usize = 1000;

/// How long the scrubber has to rest on a month before the timeline jumps.
const SCRUB_DEBOUNCE: Duration = Duration::from_millis(150);

/// Gallery tile size in px: the zoom range, its default, and the step one
/// Ctrl+scroll notch (or Ctrl+±) moves it by. The columns are stretched to
/// span the content width, so this is the *target* a tile lands near rather
/// than the size it ends up with (see [`plan_grid`]).
pub(crate) const ROW_MIN: i32 = 60;

pub(crate) const ROW_MAX: i32 = 340;

pub(crate) const ROW_DEFAULT: i32 = 180;

pub(crate) const ROW_STEP: i32 = 30;

/// Tile sizes at which the timeline stops heading each day and heads each
/// month instead, then each year. Only big tiles get a heading per day: a day
/// rarely holds a row of photos, and a heading plus a short row per day would
/// leave most of a wide window empty.
pub(crate) const GROUP_BY_MONTH_BELOW: i32 = 240;

pub(crate) const GROUP_BY_YEAR_BELOW: i32 = 90;

/// How the timeline sections its photos, set by the zoom level (see
/// [`grouping_for`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Grouping {
    Day,
    Month,
    Year,
}

/// The grouping that suits a target tile size.
pub(crate) fn grouping_for(row_height: i32) -> Grouping {
    if row_height < GROUP_BY_YEAR_BELOW {
        Grouping::Year
    } else if row_height < GROUP_BY_MONTH_BELOW {
        Grouping::Month
    } else {
        Grouping::Day
    }
}

/// Gap between tiles, horizontally and vertically. Tight on purpose: the grid
/// should read as a sheet of photographs, not as a deck of cards.
pub(crate) const TILE_GAP: i32 = 2;

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

/// How close to the top or bottom edge a drag-select has to come before the
/// timeline scrolls under it, how far one tick scrolls at the very edge, and
/// how often it ticks.
const DRAG_EDGE: f64 = 48.0;

const DRAG_SCROLL_STEP: f64 = 24.0;

const DRAG_SCROLL_TICK: Duration = Duration::from_millis(16);

/// Pause after a resize/zoom before the visible sections are re-flowed.
pub(crate) const RELAYOUT_DEBOUNCE: Duration = Duration::from_millis(80);

/// One section of the photos timeline: a heading plus the photos captured that
/// day (or month, or year — see [`Grouping`]), in timeline order. Built from the flat [`Ui::gallery_model`] by
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
    /// The "On this day" strip, as the first row of the whole, unfiltered
    /// timeline. A row rather than a strip above the list: hiding a strip
    /// outside the list as the user scrolled resized the viewport under them,
    /// and the list shook between the two heights near the top.
    Memories,
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
            GalleryRow::Memories | GalleryRow::Heading(_) => None,
            GalleryRow::Tiles { tiles, .. } => tiles.first().map(|t| t.photo.uid.clone()),
        }
    }

    /// Whether re-rendering `self` would produce exactly what `other` shows —
    /// the test [`repaint_gallery`] diffs on, so appending a page only touches
    /// the rows that actually changed.
    fn same_as(&self, other: &GalleryRow) -> bool {
        match (self, other) {
            (GalleryRow::Memories, GalleryRow::Memories) => true,
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
                            && x.grouping == y.grouping
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
    pub(crate) duplicates: gtk4::Button,
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
    pub(crate) places: gtk4::FlowBox,
    pub(crate) places_stack: gtk4::Stack,
    pub(crate) places_status: adw::StatusPage,
    pub(crate) places_btn: gtk4::ToggleButton,
    pub(crate) back: gtk4::Button,
    /// The kind toggles and date jump, as one box so an album view can hide them.
    pub(crate) filters: gtk4::Box,
    /// The All / Photos / Videos / Raw filter toggles, in that order (index maps
    /// to [`kind_for_tab`]).
    pub(crate) tabs: [gtk4::ToggleButton; 4],
    pub(crate) favorites_row: adw::SwitchRow,
    pub(crate) not_in_album_row: adw::SwitchRow,
    /// The "On this day" strip and the box its cards go into.
    pub(crate) memories: gtk4::Box,
    pub(crate) memory_cards: gtk4::Box,
    /// The month row of the Filters popover, populated with the timeline's months.
    pub(crate) dates: adw::ComboRow,
    pub(crate) filter_btn: gtk4::MenuButton,
    pub(crate) filter_count: gtk4::Label,
    pub(crate) clear_filters: gtk4::Button,
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
        .margin_start(6)
        .tooltip_text(gettext("Jump to a month"))
        .visible(false)
        .build();
    scrubber.add_css_class("photo-scrubber");

    // Horizontal scrolling is never wanted: the grid is sized to the viewport
    // width. External rather than Never, which would make the widest row the
    // window's minimum width, so a row laid out a pixel too wide would push the
    // window wider, and the next layout wider still.
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::External)
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

    // The duplicate finder opens over the page: a review, not a view of it.
    let duplicates = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(gettext("Find duplicates"))
        .valign(gtk4::Align::Center)
        .build();
    duplicates.add_css_class("flat");
    duplicates.add_css_class("circular");

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
    let tabs: [gtk4::ToggleButton; 4] = std::array::from_fn(|i| {
        gtk4::ToggleButton::builder()
            .label(tab_label(i))
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

    // Favorites, "not in an album" and the month: the filters that cut across
    // the kind tabs, together in one popover so the row stays short. The tabs
    // stay out in the open because their counts are worth a glance.
    let favorites_row = adw::SwitchRow::builder()
        .title(gettext("Only favorites"))
        .build();
    let not_in_album_row = adw::SwitchRow::builder()
        .title(gettext("Only photos not in an album"))
        .build();
    // "All dates" plus a row per month, filled in once the timeline's months are
    // known (see [`refresh_photo_months`]).
    let dates = adw::ComboRow::builder()
        .title(gettext("Month"))
        .model(&gtk4::StringList::new(&[gettext("All dates").as_str()]))
        .build();
    let filter_list = gtk4::ListBox::new();
    filter_list.set_selection_mode(gtk4::SelectionMode::None);
    filter_list.add_css_class("boxed-list");
    filter_list.append(&favorites_row);
    filter_list.append(&not_in_album_row);
    filter_list.append(&dates);
    let clear_filters = gtk4::Button::builder()
        .label(gettext("Clear filters"))
        .halign(gtk4::Align::End)
        .sensitive(false)
        .build();
    clear_filters.add_css_class("flat");
    let filter_panel = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    filter_panel.set_width_request(340);
    filter_panel.append(&filter_list);
    filter_panel.append(&clear_filters);
    let filter_popover = gtk4::Popover::builder().child(&filter_panel).build();

    // The Filters button, with a badge counting the active filters, kind
    // included, so a filtered timeline is never mistaken for the whole one.
    let filter_count = gtk4::Label::builder().visible(false).build();
    filter_count.add_css_class("filter-count");
    let filter_face = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    filter_face.append(&gtk4::Image::from_icon_name("pdfs-filter-symbolic"));
    filter_face.append(&gtk4::Label::new(Some(&gettext("Filters"))));
    filter_face.append(&filter_count);
    let filter_btn = gtk4::MenuButton::builder()
        .child(&filter_face)
        .popover(&filter_popover)
        .build();
    filter_btn.add_css_class("pill");

    // The kind toggles and the Filters button travel together: they filter the
    // timeline, and neither applies to the album grid or to an open album.
    let filters = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    filters.set_hexpand(true);
    filters.append(&tab_group);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    filters.append(&spacer);
    filters.append(&filter_btn);

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
    let places_btn = gtk4::ToggleButton::builder()
        .label(gettext("Places"))
        .build();
    places_btn.set_group(Some(&photos_btn));
    let view_switch = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    view_switch.add_css_class("linked");
    view_switch.add_css_class("view-switch");
    view_switch.set_halign(gtk4::Align::Start);
    for btn in [&photos_btn, &albums_btn, &places_btn] {
        btn.add_css_class("pill");
        view_switch.append(btn);
    }

    let filter_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    filter_bar.append(&filters);

    // "On this day": a card per earlier year that has photos from today's
    // date. Shown as the first row of the unfiltered timeline (see
    // [`sync_memories`]), so it scrolls away with the grid.
    let memories_title = gtk4::Label::builder()
        .label(gettext("On this day"))
        .xalign(0.0)
        .build();
    memories_title.add_css_class("heading");
    let memory_cards = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let memory_scroll = gtk4::ScrolledWindow::builder()
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .propagate_natural_height(true)
        .child(&memory_cards)
        .build();
    let memories = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    memories.set_hexpand(true);
    memories.set_margin_bottom(4);
    memories.append(&memories_title);
    memories.append(&memory_scroll);

    // The timeline (plus its pager) or the status page, never both.
    let timeline = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    timeline.append(&select_bar);
    // The scrubber sits beside the list rather than over it, so the grid is
    // laid out to the width it really has and no tile hides under a year mark.
    let timeline_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    scroll.set_hexpand(true);
    timeline_row.append(&scroll);
    timeline_row.append(&scrubber);
    timeline.append(&timeline_row);
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

    // The place grid: the same cards, one per town the photos were taken in.
    let places = gtk4::FlowBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .homogeneous(true)
        .row_spacing(TILE_GAP as u32 * 2)
        .column_spacing(TILE_GAP as u32 * 2)
        .min_children_per_line(2)
        .max_children_per_line(8)
        .valign(gtk4::Align::Start)
        .build();
    let places_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&places)
        .build();
    let places_status = adw::StatusPage::builder()
        .icon_name("mark-location-symbolic")
        .vexpand(true)
        .build();
    places_status.add_css_class("compact");
    let places_stack = gtk4::Stack::new();
    places_stack.set_vexpand(true);
    places_stack.add_named(&places_scroll, Some("grid"));
    places_stack.add_named(&places_status, Some("status"));

    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&timeline, Some("timeline"));
    content.add_named(&status, Some("status"));
    content.add_named(&albums_page, Some("albums"));
    content.add_named(&places_stack, Some("places"));

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
    header.pack_end(&duplicates);

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
            duplicates,
            select_btn,
            select_bar,
            select_label,
            select_trash,
            select_album,
            select_done,
            tabs,
            favorites_row,
            not_in_album_row,
            memories,
            memory_cards,
            dates,
            filter_btn,
            filter_count,
            clear_filters,
            albums,
            albums_stack,
            albums_status,
            photos_btn,
            albums_btn,
            view_switch,
            places,
            places_stack,
            places_status,
            places_btn,
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

/// How many timeline filters are on: the kind, favorites, "not in an album"
/// and the month each count once.
pub(crate) fn active_filters(
    kind: Option<PhotoKind>,
    favorites: bool,
    not_in_album: bool,
    range: Option<(i64, i64)>,
) -> usize {
    [kind.is_some(), favorites, not_in_album, range.is_some()]
        .into_iter()
        .filter(|on| *on)
        .count()
}

/// Show the active filter count on the Filters button, and offer "Clear
/// filters" only when there is something to clear.
fn sync_filter_badge(ui: &Rc<Ui>) {
    let n = active_filters(
        ui.gallery.kind.get(),
        ui.gallery.favorites.get(),
        ui.gallery.not_in_album.get(),
        ui.gallery.range.get(),
    );
    ui.gallery.filter_count.set_label(&n.to_string());
    ui.gallery.filter_count.set_visible(n > 0);
    ui.gallery.clear_filters.set_sensitive(n > 0);
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
        let n = totals[index];
        tab.set_label(&format!("{}  {}", tab_label(index), thousands(n)));
        tab.set_sensitive(n > 0 || tab.is_active());
    }
}

/// The name of filter tab `index`: All, Photos, Videos or Raw.
fn tab_label(index: usize) -> String {
    match index {
        // Translators: a Photos filter tab that shows photos, videos and raw files together.
        0 => pgettext("photo filter", "All"),
        1 => pgettext("photo filter", "Photos"),
        2 => pgettext("photo filter", "Videos"),
        // Translators: a Photos filter tab for raw camera files.
        _ => pgettext("photo filter", "Raw"),
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
        row_box.set_halign(gtk4::Align::Start);
        match &*row {
            GalleryRow::Memories => {
                // One strip widget, moved into whichever row box shows it.
                let strip = &ui_bind.gallery.memories;
                if let Some(parent) = strip.parent().and_downcast::<gtk4::Box>() {
                    parent.remove(strip);
                }
                row_box.set_halign(gtk4::Align::Fill);
                row_box.set_margin_top(8);
                row_box.set_margin_bottom(0);
                row_box.append(strip);
            }
            GalleryRow::Heading(heading) => {
                row_box.set_margin_top(8);
                row_box.set_margin_bottom(0);
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
            .insert(item.position(), (row.anchor(), row_box.clone()));
    });

    // ListView recycles row widgets, so a scrolled-away row must give up its
    // claim on them — otherwise a thumbnail landing late would paint into a tile
    // that now shows a different photo. Only claims this row widget still
    // holds are given up: after a re-flow, the same photo (and the same
    // position) is often bound again in another row before this one unbinds.
    let ui_unbind = ui.clone();
    factory.connect_unbind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let Some(row_box) = item.child() else {
            return;
        };
        {
            let mut bound = ui_unbind.gallery.bound.borrow_mut();
            if bound
                .get(&item.position())
                .is_some_and(|(_, row)| row.upcast_ref::<gtk4::Widget>() == &row_box)
            {
                bound.remove(&item.position());
            }
        }
        if let Some(obj) = item.item().and_downcast::<BoxedAnyObject>()
            && let GalleryRow::Tiles { tiles, .. } = &*obj.borrow::<GalleryRow>()
        {
            let mut wanted = ui_unbind.gallery.thumb_wanted.borrow_mut();
            for tile in tiles {
                if wanted
                    .get(&tile.photo.uid)
                    .is_some_and(|picture| picture.is_ancestor(&row_box))
                {
                    wanted.remove(&tile.photo.uid);
                }
            }
        }
    });
    list.set_factory(Some(&factory));

    // The grid divides the content width, so a resize re-flows whatever is on
    // screen (offscreen sections pick the new width up when they bind). The
    // width is the viewport's, read from the horizontal adjustment's page
    // size: the list's own width follows its widest row, so laying rows out to
    // it would feed each layout's rounding into the next, wider one.
    let ui_width = ui.clone();
    scroll
        .hadjustment()
        .connect_page_size_notify(move |adjustment| {
            let width = adjustment.page_size() as i32;
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
    wire_drag_select(ui);

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
    ui.gallery.favorites_row.connect_active_notify(move |row| {
        let on = row.is_active();
        if ui_fav.gallery.favorites.get() == on {
            return;
        }
        ui_fav.gallery.favorites.set(on);
        load_gallery(&ui_fav, false);
    });

    // Not in an album: the same kind of filter, and just as independent.
    let ui_unfiled = ui.clone();
    ui.gallery
        .not_in_album_row
        .connect_active_notify(move |row| {
            let on = row.is_active();
            if ui_unfiled.gallery.not_in_album.get() == on {
                return;
            }
            ui_unfiled.gallery.not_in_album.set(on);
            load_gallery(&ui_unfiled, false);
        });

    // Clear filters: back to the whole timeline in one reload. The state goes
    // first, so each widget's handler finds nothing to change and stays quiet.
    let ui_clear = ui.clone();
    ui.gallery.clear_filters.connect_clicked(move |_| {
        let gallery = &ui_clear.gallery;
        gallery.kind.set(None);
        gallery.favorites.set(false);
        gallery.not_in_album.set(false);
        gallery.range.set(None);
        gallery.tabs[0].set_active(true);
        gallery.favorites_row.set_active(false);
        gallery.not_in_album_row.set_active(false);
        gallery.date_suppress.set(true);
        gallery.dates.set_selected(0);
        gallery.date_suppress.set(false);
        gallery.filter_btn.popdown();
        load_gallery(&ui_clear, false);
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
    /// How the timeline is sectioned, which decides what the caption says.
    pub(crate) grouping: Grouping,
}

/// Break one section's photos into rows of square tiles spanning `width`.
///
/// Squares, because a wide window and a day of a few photos do not mix with
/// rows justified to each photo's shape: a short row stays short, and portrait
/// phone shots come out as thin slivers. Every tile of a square grid is the
/// same size whatever it shows, and the thumbnail is cropped to fill it.
pub(crate) fn justify_rows(ui: &Rc<Ui>, photos: &[PhotoItem], width: i32) -> Vec<Vec<Tile>> {
    let selecting = ui.gallery.selecting.get();
    let selected = ui.gallery.selected.borrow();
    let target = ui.gallery.row_height.get();
    let grouping = grouping_for(target);
    let (columns, size) = plan_grid(width, target);
    photos
        .chunks(columns)
        .map(|row| {
            row.iter()
                .map(|photo| Tile {
                    selecting,
                    selected: selecting && selected.contains(&photo.uid),
                    grouping,
                    photo: photo.clone(),
                    width: size,
                    height: size,
                })
                .collect()
        })
        .collect()
}

/// The layout math: how many square tiles of about `target` px fit across
/// `width`, and the size that makes them span it exactly, gaps included.
///
/// The count is rounded to the nearest whole column, so a tile never ends up
/// more than half a column off the zoom level asked for.
pub(crate) fn plan_grid(width: i32, target: i32) -> (usize, i32) {
    let width = width.max(1);
    let target = target.clamp(ROW_MIN, ROW_MAX);
    let columns = (f64::from(width + TILE_GAP) / f64::from(target + TILE_GAP)).round() as i32;
    let columns = columns.max(1);
    let size = ((width - TILE_GAP * (columns - 1)) / columns).max(1);
    (columns as usize, size)
}

/// The width the grid is laid out to: the viewport's width, less a couple of
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
        // Every tile is square, so Cover crops the photo to fill it. Contain
        // would letterbox it, and the bars would read as gaps in the sheet.
        // The expands are what make the picture take the whole overlay.
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
    // the tile — legible over any photo, invisible the rest of the time. Under a
    // day heading the clock time is enough; under a month or a year the day is
    // what the heading no longer says.
    let caption_text = match tile.grouping {
        Grouping::Day => short_capture_time(tile.photo.capture_time),
        Grouping::Month | Grouping::Year => short_capture_date(tile.photo.capture_time),
    };
    let caption = gtk4::Label::builder()
        // Fill horizontally so the scrim spans the tile; the text itself stays
        // left-aligned inside it.
        .halign(gtk4::Align::Fill)
        .valign(gtk4::Align::End)
        .xalign(0.0)
        .label(caption_text)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    caption.add_css_class("photo-caption");

    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&placeholder));
    overlay.add_overlay(&picture);
    overlay.add_overlay(&caption);

    // A video reads as a video at a glance: a play glyph centred over the poster
    // thumbnail. Kept above the caption scrim so it stays legible on hover.
    // Zoomed out, the glyph shrinks with the tile so it marks the video without
    // covering it.
    let small = tile.grouping != Grouping::Day;
    let is_video = tile.photo.kind == PhotoKind::Video;
    if is_video {
        let badge = gtk4::Image::builder()
            .icon_name("media-playback-start-symbolic")
            .pixel_size(if small { 12 } else { 28 })
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        badge.add_css_class("photo-video-badge");
        if small {
            badge.add_css_class("photo-video-badge-small");
        }
        overlay.add_overlay(&badge);
    }

    // A shot stored as more than one file says so, in the corner the caption
    // does not use. "RAW" is the useful word when one of the members is a raw
    // file — that is what the person wants to find — and a plain count covers
    // the rest (a live photo, a burst). Zoomed out to years the badge would
    // cover most of a tile, so it is left off there.
    if (tile.photo.has_raw || tile.photo.group_size > 1) && tile.grouping != Grouping::Year {
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
    // first. Ctrl picks one photo; Shift picks everything from the last photo
    // clicked to this one, as in a file manager. Claimed in the capture phase so
    // the click never also opens the photo it was picking.
    let modifier = gtk4::GestureClick::new();
    modifier.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let ui_modifier = ui.clone();
    let modifier_uid = tile.photo.uid.clone();
    modifier.connect_pressed(move |gesture, _, _, _| {
        let state = gesture.current_event_state();
        let shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
        if !shift && !state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
            return;
        }
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        set_selection_mode(&ui_modifier, true);
        let anchor = ui_modifier.gallery.select_anchor.borrow().clone();
        match anchor {
            Some(anchor) if shift => {
                let base = ui_modifier.gallery.selected.borrow().clone();
                select_range(&ui_modifier, &anchor, &modifier_uid, &base);
            }
            _ => toggle_selected(&ui_modifier, &modifier_uid),
        }
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
        ui.gallery.select_anchor.borrow_mut().take();
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
    *ui.gallery.select_anchor.borrow_mut() = Some(uid.to_string());
    sync_selection_bar(ui);
    repaint_gallery(ui);
}

/// Make the selection `base` plus every loaded photo from `from` to `to`, in
/// either direction. `base` rather than the live selection, so a drag that
/// sweeps back over photos it picked gives them up again.
fn select_range(ui: &Rc<Ui>, from: &str, to: &str, base: &HashSet<String>) {
    let model = &ui.gallery.model;
    let (Some(a), Some(b)) = (find_photo_index(model, from), find_photo_index(model, to)) else {
        return;
    };
    let mut selected = base.clone();
    for index in a.min(b)..=a.max(b) {
        if let Some(boxed) = model.item(index).and_downcast::<BoxedAnyObject>() {
            selected.insert(boxed.borrow::<PhotoItem>().uid.clone());
        }
    }
    if *ui.gallery.selected.borrow() == selected {
        return;
    }
    *ui.gallery.selected.borrow_mut() = selected;
    sync_selection_bar(ui);
    repaint_gallery(ui);
}

/// A drag-select in progress (see [`wire_drag_select`]).
#[derive(Default)]
struct DragSelect {
    /// Whether the drag has moved far enough to be selecting. Until then it
    /// may still turn out to be a click on a tile.
    active: bool,
    /// The selection as it was when the drag began.
    base: HashSet<String>,
    /// The photo the drag started on, or the first one it crossed.
    anchor: Option<String>,
    /// Where the drag started and where the pointer is now, in the list's
    /// coordinates.
    start: (f64, f64),
    pointer: (f64, f64),
    /// The edge autoscroll, while the pointer is near the top or bottom.
    scroll: Option<glib::SourceId>,
}

/// Dragging across the timeline selects every photo from where the drag
/// started to where the pointer is, scrolling when the pointer nears an edge.
///
/// The gesture sits on the list in the capture phase and only claims the
/// pointer once it has moved past the drag threshold, so a plain click still
/// reaches the tile and opens the photo.
fn wire_drag_select(ui: &Rc<Ui>) {
    let drag = gtk4::GestureDrag::builder()
        .button(gtk4::gdk::BUTTON_PRIMARY)
        .propagation_phase(gtk4::PropagationPhase::Capture)
        .build();
    let state = Rc::new(RefCell::new(DragSelect::default()));

    let state_begin = state.clone();
    drag.connect_drag_begin(move |_, x, y| {
        let mut drag = state_begin.borrow_mut();
        if let Some(source) = drag.scroll.take() {
            source.remove();
        }
        *drag = DragSelect {
            start: (x, y),
            pointer: (x, y),
            ..DragSelect::default()
        };
    });

    let ui_update = ui.clone();
    let state_update = state.clone();
    drag.connect_drag_update(move |gesture, dx, dy| {
        let (sx, sy) = state_update.borrow().start;
        state_update.borrow_mut().pointer = (sx + dx, sy + dy);
        if !state_update.borrow().active {
            let threshold = gtk4::Settings::default()
                .map_or(8, |settings| settings.gtk_dnd_drag_threshold())
                as f64;
            if dx.hypot(dy) < threshold {
                return;
            }
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let anchor = tile_at(&ui_update, sx, sy);
            set_selection_mode(&ui_update, true);
            let mut drag = state_update.borrow_mut();
            drag.active = true;
            drag.base = ui_update.gallery.selected.borrow().clone();
            drag.anchor = anchor;
        }
        sweep_drag(&ui_update, &state_update);
        autoscroll_drag(&ui_update, &state_update);
    });

    let ui_end = ui.clone();
    let state_end = state;
    drag.connect_drag_end(move |_, _, _| {
        let mut drag = state_end.borrow_mut();
        if let Some(source) = drag.scroll.take() {
            source.remove();
        }
        if drag.active
            && let Some(anchor) = drag.anchor.take()
        {
            *ui_end.gallery.select_anchor.borrow_mut() = Some(anchor);
        }
        drag.active = false;
    });
    ui.gallery.list.add_controller(drag);
}

/// Select from the drag's anchor to the photo under the pointer.
fn sweep_drag(ui: &Rc<Ui>, state: &Rc<RefCell<DragSelect>>) {
    let (x, y) = state.borrow().pointer;
    // Past an edge, the pointer is over the row at that edge: that row is
    // what is scrolling into view under it.
    let height = f64::from(ui.gallery.list.height());
    let Some(current) = tile_at(ui, x, y.clamp(0.0, (height - 1.0).max(0.0))) else {
        return;
    };
    let (anchor, base) = {
        let mut drag = state.borrow_mut();
        let anchor = drag.anchor.get_or_insert_with(|| current.clone()).clone();
        (anchor, std::mem::take(&mut drag.base))
    };
    select_range(ui, &anchor, &current, &base);
    state.borrow_mut().base = base;
}

/// Scroll while a drag-select's pointer is near the top or bottom edge, faster
/// the closer it gets, re-sweeping as rows move under it.
fn autoscroll_drag(ui: &Rc<Ui>, state: &Rc<RefCell<DragSelect>>) {
    if state.borrow().scroll.is_some() {
        return;
    }
    if drag_scroll_speed(ui, state.borrow().pointer.1) == 0.0 {
        return;
    }
    let ui_tick = ui.clone();
    let state_tick = state.clone();
    let source = glib::timeout_add_local(DRAG_SCROLL_TICK, move || {
        let speed = drag_scroll_speed(&ui_tick, state_tick.borrow().pointer.1);
        let adjustment = ui_tick.gallery.list.vadjustment();
        let (Some(adjustment), true) = (adjustment, speed != 0.0) else {
            state_tick.borrow_mut().scroll.take();
            return glib::ControlFlow::Break;
        };
        let top = adjustment.upper() - adjustment.page_size();
        adjustment.set_value((adjustment.value() + speed).clamp(adjustment.lower(), top));
        sweep_drag(&ui_tick, &state_tick);
        glib::ControlFlow::Continue
    });
    state.borrow_mut().scroll = Some(source);
}

/// Pixels per tick a drag at height `y` scrolls the timeline by: negative near
/// the top, positive near the bottom, zero in between.
fn drag_scroll_speed(ui: &Rc<Ui>, y: f64) -> f64 {
    let height = f64::from(ui.gallery.list.height());
    let depth = if y < DRAG_EDGE {
        y - DRAG_EDGE
    } else if y > height - DRAG_EDGE {
        y - (height - DRAG_EDGE)
    } else {
        return 0.0;
    };
    (depth / DRAG_EDGE).clamp(-1.0, 1.0) * DRAG_SCROLL_STEP
}

/// The photo under `(x, y)`, in the list's coordinates: the tile of the row at
/// that height whose left edge is the last one at or before `x`. `None` over
/// a heading or a gap between rows.
fn tile_at(ui: &Rc<Ui>, x: f64, y: f64) -> Option<String> {
    let list = &ui.gallery.list;
    let (position, row) =
        ui.gallery
            .bound
            .borrow()
            .iter()
            .find_map(|(position, (anchor, row))| {
                anchor.as_ref()?;
                let rect = row.compute_bounds(list)?;
                let (top, bottom) = (f64::from(rect.y()), f64::from(rect.y() + rect.height()));
                (row.is_mapped() && y >= top && y < bottom).then(|| (*position, row.clone()))
            })?;
    let mut index = 0;
    let mut child = row.first_child();
    let mut i = 0;
    while let Some(tile) = child {
        if tile
            .compute_bounds(list)
            .is_some_and(|rect| x >= f64::from(rect.x()))
        {
            index = i;
        }
        i += 1;
        child = tile.next_sibling();
    }
    let item = ui.gallery.groups.item(position)?;
    let boxed = item.downcast_ref::<BoxedAnyObject>()?;
    match &*boxed.borrow::<GalleryRow>() {
        GalleryRow::Tiles { tiles, .. } => tiles.get(index).map(|tile| tile.photo.uid.clone()),
        GalleryRow::Memories | GalleryRow::Heading(_) => None,
    }
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
            trash_photos(&ui, uids.clone(), false);
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
///
/// `files_only` trashes exactly `uids` and leaves the rest of their groups, for
/// the duplicate finder; otherwise each photo takes its whole group along.
pub(crate) fn trash_photos(ui: &Rc<Ui>, uids: Vec<String>, files_only: bool) {
    let removed = remove_photos(ui, &uids);
    set_selection_mode(ui, false);
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::TrashNodes {
            uids: uids.clone(),
            files_only,
        },
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
/// topmost visible row's first photo is remembered, and the row that holds it
/// afterwards is put back at the same height in the viewport.
pub(crate) fn relayout_gallery(ui: &Rc<Ui>) {
    // At the very top there is nothing to hold on to: the first photo row sits
    // under a heading, and anchoring on it would scroll that heading away.
    let at_top = ui
        .gallery
        .list
        .vadjustment()
        .is_none_or(|adj| adj.value() < 1.0);
    let anchor = if at_top { None } else { top_anchor(ui) };
    repaint_gallery(ui);
    let Some((anchor, offset)) = anchor else {
        return;
    };
    let Some(row) = row_of_photo(&ui.gallery.groups, &anchor) else {
        return;
    };
    // Scrolling to a row only brings it into view, anywhere in it; the rows
    // are laid out over the next frames, so the fine alignment follows them.
    ui.gallery
        .list
        .scroll_to(row, gtk4::ListScrollFlags::empty(), None);
    align_row(ui, row, offset);
}

/// How many frames [`align_row`] keeps correcting for: rows above the anchor
/// that the list only estimated get measured as they are realised.
const ALIGN_FRAMES: u32 = 4;

/// Scroll so the top of row `position` sits `offset` px below the top of the
/// viewport, re-checking for a few frames while the rows around it settle.
fn align_row(ui: &Rc<Ui>, position: u32, offset: f32) {
    let ui = ui.clone();
    let frames = Cell::new(0);
    ui.gallery.list.clone().add_tick_callback(move |list, _| {
        frames.set(frames.get() + 1);
        let top = ui
            .gallery
            .bound
            .borrow()
            .get(&position)
            .filter(|(_, row)| row.is_mapped())
            .and_then(|(_, row)| row.compute_bounds(list))
            .map(|rect| rect.y());
        if let (Some(top), Some(adjustment)) = (top, list.vadjustment()) {
            let delta = f64::from(top - offset);
            if delta.abs() >= 1.0 {
                adjustment.set_value(adjustment.value() + delta);
            }
        }
        if frames.get() >= ALIGN_FRAMES {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// The first photo of the topmost photo row that reaches into the viewport,
/// and how far below the top of the viewport that row starts (negative when it
/// is partly scrolled off).
///
/// Not simply the first bound row: the ListView keeps rows bound well outside
/// the viewport (unmapped, with their last allocation), and scrolling one of
/// those "back" to the top is a jump. Falls back to the first bound photo row
/// while the list isn't on screen.
fn top_anchor(ui: &Rc<Ui>) -> Option<(String, f32)> {
    let list = &ui.gallery.list;
    let bound = ui.gallery.bound.borrow();
    let mut photo_rows = bound
        .values()
        .filter_map(|(uid, row)| uid.as_ref().map(|uid| (uid, row)));
    let first = photo_rows.clone().next().map(|(uid, _)| (uid.clone(), 0.0));
    photo_rows
        .find_map(|(uid, row)| {
            let rect = row.compute_bounds(list)?;
            (row.is_mapped() && rect.y() + rect.height() > 0.0).then(|| (uid.clone(), rect.y()))
        })
        .or(first)
}

/// Which row of the rendered model holds `uid`, if any.
fn row_of_photo(store: &gio::ListStore, uid: &str) -> Option<u32> {
    (0..store.n_items()).find(|i| {
        store
            .item(*i)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|obj| match &*obj.borrow::<GalleryRow>() {
                GalleryRow::Memories | GalleryRow::Heading(_) => false,
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
    let grouping = grouping_for(ui.gallery.row_height.get());
    if shows_memories(ui) {
        rows.push(GalleryRow::Memories);
    }
    for group in group_photos(&ui.gallery.model, grouping) {
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
        GalleryRow::Memories => GalleryRow::Memories,
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
    // An album or a place counts what it holds, not how much of it has been
    // paged in — the subtitle would otherwise climb as the user scrolls.
    if ui.gallery.in_collection() {
        return;
    }
    // The noun tracks the active filter, so a Videos tab doesn't count "photos".
    let kind = ui.gallery.kind.get();
    // The whole library for this filter, not the page count — the subtitle sits
    // next to tabs carrying the same totals, and the two disagreeing reads as a
    // bug. A date jump or a filter is the exception: the counts are the
    // library's, not the filter's, so there the loaded count is the honest one.
    let narrowed = ui.gallery.range.get().is_some()
        || ui.gallery.favorites.get()
        || ui.gallery.not_in_album.get();
    let total = match (narrowed, ui.gallery.counts.get()) {
        (false, Some((photos, videos, raw))) => match kind {
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
    let count = thousands(total);
    let args = [("count", count.as_str())];
    let n = total as u64;
    ui.gallery.title.set_subtitle(&match kind {
        // Translators: the Photos page subtitle, such as "12 videos".
        Some(PhotoKind::Video) => ngettext_f("{count} video", "{count} videos", n, &args),
        // Translators: the Photos page subtitle, such as "12 raw photos".
        Some(PhotoKind::Raw) => ngettext_f("{count} raw photo", "{count} raw photos", n, &args),
        // Translators: the Photos page subtitle, such as "1,204 photos".
        _ => ngettext_f("{count} photo", "{count} photos", n, &args),
    });
}

pub(crate) fn group_photos(model: &gio::ListStore, grouping: Grouping) -> Vec<PhotoGroup> {
    let mut groups: Vec<PhotoGroup> = Vec::new();
    for i in 0..model.n_items() {
        let Some(obj) = model.item(i) else { continue };
        let Some(boxed) = obj.downcast_ref::<BoxedAnyObject>() else {
            continue;
        };
        let photo = boxed.borrow::<PhotoItem>().clone();
        let heading = section_heading(photo.capture_time, grouping);
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

/// Section heading for a capture time: "Today", "Yesterday", or the local date
/// when grouping by day; "June 2026" by month; "2026" by year.
pub(crate) fn section_heading(secs: i64, grouping: Grouping) -> String {
    let Ok(date) = glib::DateTime::from_unix_local(secs) else {
        return gettext("Unknown date");
    };
    match grouping {
        Grouping::Day => {}
        Grouping::Month => return month_label(date.year(), date.month()),
        Grouping::Year => return date.year().to_string(),
    }
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

/// The capture date as a tile caption, for a timeline headed by month or year.
pub(crate) fn short_capture_date(secs: i64) -> String {
    // Translators: strftime format for the day a photo was taken, shown on its tile when the timeline is grouped by month or year, such as "3 Jun".
    let format = gettext("%-d %b");
    glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format(&format))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Fetch a timeline page from the daemon. When `append` is false the model is
/// cleared first (fresh load); otherwise the next page is tacked on.
/// The empty state for a timeline filtered to `kind`, favorites, the photos
/// in no album and/or a `month`: what is missing, in the filter's own words.
pub(crate) fn empty_timeline_text(
    kind: Option<PhotoKind>,
    favorites: bool,
    not_in_album: bool,
    month: Option<&str>,
) -> (String, String) {
    let args = [("month", month.unwrap_or_default())];
    let video = kind == Some(PhotoKind::Video);
    let raw = kind == Some(PhotoKind::Raw);
    if not_in_album && !favorites && kind.is_none() && month.is_none() {
        return (
            gettext("Every photo is in an album"),
            gettext("Turn off the album filter to see everything."),
        );
    }
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
            match (video, raw, month.is_some()) {
                // Translators: {month} is a month and year, such as "June 2024".
                (true, _, true) => gettext_f("No favorite videos in {month}", &args),
                (true, _, false) => gettext("No favorite videos"),
                // Translators: {month} is a month and year, such as "June 2024".
                (_, true, true) => gettext_f("No favorite raw files in {month}", &args),
                (_, true, false) => gettext("No favorite raw files"),
                // Translators: {month} is a month and year, such as "June 2024".
                (_, _, true) => gettext_f("No favorite photos in {month}", &args),
                (_, _, false) => gettext("No favorite photos"),
            },
            gettext("Turn off the favorites filter to see everything."),
        ),
        (false, _, _) => (
            match (video, raw, month.is_some()) {
                // Translators: {month} is a month and year, such as "June 2024".
                (true, _, true) => gettext_f("No videos in {month}", &args),
                (true, _, false) => gettext("No videos"),
                // Translators: {month} is a month and year, such as "June 2024".
                (_, true, true) => gettext_f("No raw files in {month}", &args),
                (_, true, false) => gettext("No raw files"),
                // Translators: {month} is a month and year, such as "June 2024".
                (_, _, true) => gettext_f("No photos in {month}", &args),
                (_, _, false) => gettext("No photos"),
            },
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
/// not an album, a date window, the favorites or the unfiled photos.
pub(crate) fn sync_scrubber(ui: &Rc<Ui>) {
    let whole = !ui.gallery.in_collection()
        && ui.gallery.range.get().is_none()
        && !ui.gallery.favorites.get()
        && !ui.gallery.not_in_album.get();
    ui.gallery
        .scrubber
        .set_visible(whole && ui.gallery.months.borrow().len() > 1);
}

/// How many photos the "On this day" strip asks for. Enough for a card per year
/// of a big library; the cards only need one cover each.
const MEMORIES_LIMIT: usize = 200;

/// Size of an "On this day" card's cover, in px.
const MEMORY_CARD_WIDTH: i32 = 180;
const MEMORY_CARD_HEIGHT: i32 = 120;

/// Ask the daemon for photos taken on today's date in earlier years, and fill
/// the "On this day" strip with a card per year. A failure leaves the strip
/// hidden: it is a nicety, not worth a toast.
fn refresh_memories(ui: &Rc<Ui>) {
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::PhotosOnThisDay {
            limit: MEMORIES_LIMIT,
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let items = match rx.recv().await {
            Ok(Ok(Response::Photos { items, .. })) => items,
            _ => Vec::new(),
        };
        let this_year = glib::DateTime::now_local().map_or(0, |now| now.year());
        let cards = &ui.gallery.memory_cards;
        while let Some(child) = cards.first_child() {
            cards.remove(&child);
        }
        let years = memory_years(&items, this_year);
        ui.gallery.has_memories.set(!years.is_empty());
        for (years_ago, photos) in years {
            cards.append(&memory_card(&ui, years_ago, photos));
        }
        schedule_thumbs(&ui);
        sync_memories(&ui);
    });
}

/// Group "On this day" photos (newest first) by how many years ago they were
/// taken, nearest year first. Each group keeps the newest-first order, so its
/// first photo is the card's cover.
pub(crate) fn memory_years(items: &[PhotoItem], this_year: i32) -> Vec<(u32, Vec<PhotoItem>)> {
    let mut years: Vec<(u32, Vec<PhotoItem>)> = Vec::new();
    for item in items {
        let Ok(date) = glib::DateTime::from_unix_local(item.capture_time) else {
            continue;
        };
        let Ok(years_ago) = u32::try_from(this_year - date.year()) else {
            continue;
        };
        if years_ago == 0 {
            continue;
        }
        match years.iter_mut().find(|(y, _)| *y == years_ago) {
            Some((_, photos)) => photos.push(item.clone()),
            None => years.push((years_ago, vec![item.clone()])),
        }
    }
    years.sort_by_key(|(years_ago, _)| *years_ago);
    years
}

/// One "On this day" card: the year's newest photo as the cover, labelled with
/// how long ago it was. Clicking it scrolls the timeline to that day.
fn memory_card(ui: &Rc<Ui>, years_ago: u32, photos: Vec<PhotoItem>) -> gtk4::Button {
    let cover = photos[0].clone();
    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .build();
    let placeholder = gtk4::Image::builder()
        .icon_name("image-x-generic-symbolic")
        .pixel_size(24)
        .build();
    placeholder.add_css_class("photo-placeholder");
    let label = gtk4::Label::builder()
        .label(ngettext_f(
            "{n} year ago",
            "{n} years ago",
            u64::from(years_ago),
            &[],
        ))
        .halign(gtk4::Align::Fill)
        .valign(gtk4::Align::End)
        .xalign(0.0)
        .build();
    label.add_css_class("memory-label");

    // Sized here rather than on the picture: an overlay takes its size from
    // its main child, which is the placeholder glyph.
    let overlay = gtk4::Overlay::new();
    overlay.set_size_request(MEMORY_CARD_WIDTH, MEMORY_CARD_HEIGHT);
    overlay.set_child(Some(&placeholder));
    overlay.add_overlay(&picture);
    overlay.add_overlay(&label);
    overlay.set_overflow(gtk4::Overflow::Hidden);
    overlay.add_css_class("memory-cover");

    let day = section_heading(cover.capture_time, Grouping::Day);
    let card = gtk4::Button::builder()
        .child(&overlay)
        .tooltip_text(ngettext_f(
            "{date}, {n} photo",
            "{date}, {n} photos",
            photos.len() as u64,
            &[("date", &day)],
        ))
        .build();
    card.add_css_class("flat");
    card.add_css_class("memory-card");

    want_thumb(ui, &cover, &picture);
    let ui_click = ui.clone();
    card.connect_clicked(move |_| {
        // The timeline is newest first, so the day's newest photo is the first
        // one older than a second past it.
        ui_click.gallery.jump.set(Some(cover.capture_time + 1));
        continue_jump(&ui_click);
    });
    card
}

/// Whether the "On this day" strip belongs in the timeline: only atop the
/// whole, unfiltered timeline, and only when it has cards.
fn shows_memories(ui: &Rc<Ui>) -> bool {
    let whole = !ui.gallery.in_collection()
        && ui.gallery.kind.get().is_none()
        && ui.gallery.range.get().is_none()
        && !ui.gallery.favorites.get()
        && !ui.gallery.not_in_album.get();
    whole && ui.gallery.has_memories.get() && ui.gallery.model.n_items() > 0
}

/// Add or drop the "On this day" row at the head of the timeline. Only that
/// one row changes, so the rest of the list keeps its widgets and position.
fn sync_memories(ui: &Rc<Ui>) {
    let store = &ui.gallery.groups;
    let shown = store
        .item(0)
        .and_downcast::<BoxedAnyObject>()
        .is_some_and(|obj| matches!(*obj.borrow::<GalleryRow>(), GalleryRow::Memories));
    match (shown, shows_memories(ui)) {
        (false, true) => store.insert(0, &BoxedAnyObject::new(GalleryRow::Memories)),
        (true, false) => store.remove(0),
        _ => {}
    }
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
    let Some((uid, _)) = top_anchor(ui) else {
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
    sync_filter_badge(ui);
    if ui.gallery.loading.get() {
        // A filter flipped while a page is on its way: that page answers the
        // old question, so fetch again once it lands. More pages can wait.
        if !append {
            ui.gallery.reload_pending.set(true);
        }
        return;
    }
    // An open album or place pages itself instead of the timeline; everything
    // downstream — the model, the sections, the thumbnails, the lightbox — is
    // the same.
    let album = ui.gallery.album.borrow().as_ref().map(|a| a.uid.clone());
    let place = ui.gallery.place.borrow().as_ref().map(|p| p.id);
    let collection = album.is_some() || place.is_some();
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
        if !collection && ui.gallery.range.get().is_none() {
            refresh_photo_months(ui);
        }
        sync_scrubber(ui);
        if !collection {
            refresh_memories(ui);
        }
        sync_memories(ui);
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
    let request = match (album, place) {
        (Some(uid), _) => Request::AlbumPhotos { uid, offset, limit },
        (None, Some(id)) => Request::PlacePhotos { id, offset, limit },
        (None, None) => Request::PhotosTimeline {
            offset,
            limit,
            kind: ui.gallery.kind.get(),
            range: ui.gallery.range.get(),
            favorites: ui.gallery.favorites.get(),
            not_in_album: ui.gallery.not_in_album.get(),
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
        if ui.gallery.reload_pending.take() {
            load_gallery(&ui, false);
            return;
        }
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
                        || ui.gallery.not_in_album.get()
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
                            ui.gallery.not_in_album.get(),
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
                        .set_visible(!ui.gallery.in_collection() && !filtered);
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
    use super::{
        Grouping, ROW_DEFAULT, ROW_MAX, ROW_MIN, ROW_STEP, TILE_GAP, active_filters,
        empty_timeline_text, grouping_for, memory_years, month_index, month_range, plan_grid,
        section_heading,
    };
    use pdfs_core::control::{PhotoItem, PhotoKind, PhotoMonth};

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
    fn square_tiles_span_the_content_width() {
        for width in [0, 40, 640, 900, 1440, 1920, 2560] {
            for target in [ROW_MIN, ROW_DEFAULT, ROW_MAX] {
                let (columns, size) = plan_grid(width, target);
                let columns = columns as i32;
                let row = columns * size + TILE_GAP * (columns - 1);
                assert!(size > 0 && columns > 0);
                assert!(
                    width < target || row <= width,
                    "{row}px overflows {width}px"
                );
                assert!(
                    width < target || width - row < columns,
                    "{row}px leaves a margin in {width}px"
                );
            }
        }
    }

    #[test]
    fn zooming_in_puts_fewer_tiles_in_a_row() {
        assert!(plan_grid(1920, ROW_MIN).0 > plan_grid(1920, ROW_DEFAULT).0);
        assert!(plan_grid(1920, ROW_DEFAULT).0 > plan_grid(1920, ROW_MAX).0);
        let (_, size) = plan_grid(1920, ROW_DEFAULT);
        assert!((size - ROW_DEFAULT).abs() < ROW_DEFAULT / 2);
    }

    #[test]
    fn only_big_tiles_are_headed_by_day() {
        assert_eq!(grouping_for(ROW_MAX), Grouping::Day);
        assert_eq!(grouping_for(ROW_DEFAULT + 2 * ROW_STEP), Grouping::Day);
        assert_eq!(grouping_for(ROW_DEFAULT), Grouping::Month);
        assert_eq!(grouping_for(ROW_DEFAULT - 3 * ROW_STEP), Grouping::Month);
        assert_eq!(grouping_for(ROW_MIN), Grouping::Year);
    }

    #[test]
    fn month_and_year_sections_are_headed_by_the_month_and_the_year() {
        let Some((from, _)) = month_range(2024, 6) else {
            panic!("June 2024 is a month");
        };
        let noon = from + 14 * 86_400 + 12 * 3_600;
        assert_eq!(section_heading(noon, Grouping::Year), "2024");
        assert_eq!(
            section_heading(noon, Grouping::Month),
            section_heading(from, Grouping::Month),
            "every day of a month shares its heading"
        );
        assert_ne!(
            section_heading(noon, Grouping::Day),
            section_heading(from, Grouping::Day)
        );
    }

    #[test]
    fn the_filter_badge_counts_each_active_filter_once() {
        assert_eq!(active_filters(None, false, false, None), 0);
        assert_eq!(active_filters(Some(PhotoKind::Raw), false, false, None), 1);
        assert_eq!(
            active_filters(Some(PhotoKind::Video), true, true, Some((0, 1))),
            4
        );
    }

    /// The strip shows the nearest year first, with each year's newest photo
    /// as its cover, and never this year's own photos.
    #[test]
    fn on_this_day_cards_go_nearest_year_first() {
        let photo = |uid: &str, year: i32, hour: i32| {
            let Some((from, _)) = month_range(year, 6) else {
                panic!("June {year} is a month");
            };
            PhotoItem {
                uid: uid.into(),
                capture_time: from + 14 * 86_400 + i64::from(hour) * 3_600,
                thumb_path: None,
                name: None,
                ratio: None,
                no_thumb: false,
                kind: PhotoKind::Photo,
                favorite: false,
                group_size: 1,
                has_raw: false,
            }
        };
        // Newest first, the way the daemon sends them.
        let items = [
            photo("today", 2026, 9),
            photo("late", 2025, 18),
            photo("early", 2025, 8),
            photo("old", 2021, 12),
        ];
        let years = memory_years(&items, 2026);
        let summary: Vec<(u32, Vec<&str>)> = years
            .iter()
            .map(|(ago, photos)| (*ago, photos.iter().map(|p| p.uid.as_str()).collect()))
            .collect();
        assert_eq!(summary, [(1, vec!["late", "early"]), (5, vec!["old"])]);
    }

    #[test]
    fn an_empty_filter_names_what_it_filtered() {
        assert_eq!(
            empty_timeline_text(None, false, false, None).0,
            "No photos yet"
        );
        assert_eq!(
            empty_timeline_text(None, true, false, None).0,
            "No favorites yet"
        );
        assert_eq!(
            empty_timeline_text(Some(PhotoKind::Video), false, false, Some("June 2024")).0,
            "No videos in June 2024"
        );
        assert_eq!(
            empty_timeline_text(Some(PhotoKind::Raw), true, false, None).0,
            "No favorite raw files"
        );
        assert_eq!(
            empty_timeline_text(None, false, true, None).0,
            "Every photo is in an album"
        );
    }
}
