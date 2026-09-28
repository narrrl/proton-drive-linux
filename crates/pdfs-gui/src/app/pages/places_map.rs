//! The map of the Places view: the towns the photos were taken in, marked by
//! their newest photo, and once the map is zoomed in close, each photo where
//! it was taken.
//!
//! Underneath is one of two maps. The built-in one is drawn here from Natural
//! Earth's 1:50m land polygons and country borders (public domain), trimmed
//! into `data/land.txt` and `data/borders.txt` by
//! `scripts/natural-earth-land.py`; it needs no network and tells nobody
//! anything. The street map from OpenFreeMap ([`super::map_tiles`]) is
//! opt-in, in Preferences, and draws over the built-in one wherever its tiles
//! have arrived.
//!
//! The projection is Web Mercator, the one every other map uses, so the
//! world looks the way people expect. Markers that would overlap on screen
//! merge into one showing the newest photo and the count of all of them;
//! clicking it zooms in until they part. Clicking a town opens it, clicking
//! a single photo opens it in its town, and photos taken on one spot, which
//! never part, are listed to pick from.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gtk4::cairo;
use pdfs_core::control::PhotoLocation;

use super::map_tiles::{self, Done, Label, LabelClass, Palette, TILE_EDGE, TileKey, Tiles};
use crate::*;

const LAND: &str = include_str!("../../../data/land.txt");
const BORDERS: &str = include_str!("../../../data/borders.txt");

/// Latitude past which Mercator is cut off, as on every web map.
const MAX_LATITUDE: f64 = 85.0;

/// How far in the map zooms, as a multiple of the whole world fitting the
/// widget: far enough for photos in one town to part on the built-in map,
/// and down to single streets on the street map.
const MAX_ZOOM: f64 = 8192.0;
const ONLINE_MAX_ZOOM: f64 = 65536.0;

/// How close "show all" zooms onto a single town, the same way.
const FIT_MAX_ZOOM: f64 = 64.0;

/// From this many px per world width on — about a town filling the widget —
/// the map shows each photo where it was taken rather than its town.
const PHOTO_SCALE: f64 = 131_072.0;

/// How much closer than [`PHOTO_SCALE`] a single photo's spot is shown.
const SPOT_ZOOM: f64 = 4.0;

/// The zoom step of one scroll notch and of the zoom buttons.
const ZOOM_STEP: f64 = 1.25;
const BUTTON_ZOOM_STEP: f64 = 2.0;

/// A press that moves less than this many px is a click, not a drag.
const CLICK_SLOP: f64 = 4.0;

/// Outline points closer than this many px to the last one drawn are
/// skipped: at world zoom most of the outline would fall onto one pixel.
const MIN_SEGMENT: f64 = 0.75;

/// Edge in px of the square a marker's photo is scaled down to once.
const COVER_PX: i32 = 128;

/// How many street-map tiles stay in memory.
const TILE_CACHE: usize = 160;

/// How long a tile that couldn't be had is left alone before it is tried
/// again.
const TILE_RETRY: Duration = Duration::from_secs(60);

/// At most this many names are set on the street map at once.
const MAX_LABELS: usize = 80;

/// A point in projected world space: `x` and `y` both run 0..1, from the
/// antimeridian eastwards and from the north edge southwards.
#[derive(Clone, Copy, Debug, PartialEq)]
struct World {
    x: f64,
    y: f64,
}

fn project(latitude: f64, longitude: f64) -> World {
    let lat = latitude.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    World {
        x: (longitude + 180.0) / 360.0,
        y: (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0,
    }
}

/// One land outline or border line, projected, with its bounding box for
/// culling.
struct Ring {
    points: Vec<World>,
    min: World,
    max: World,
}

fn land() -> &'static [Ring] {
    static LAND_RINGS: OnceLock<Vec<Ring>> = OnceLock::new();
    LAND_RINGS.get_or_init(|| LAND.lines().filter_map(parse_ring).collect())
}

fn borders() -> &'static [Ring] {
    static BORDER_LINES: OnceLock<Vec<Ring>> = OnceLock::new();
    BORDER_LINES.get_or_init(|| BORDERS.lines().filter_map(parse_ring).collect())
}

fn parse_ring(line: &str) -> Option<Ring> {
    let points: Vec<World> = line
        .split(' ')
        .filter_map(|pair| {
            let (lon, lat) = pair.split_once(',')?;
            Some(project(lat.parse().ok()?, lon.parse().ok()?))
        })
        .collect();
    let first = *points.first()?;
    let (min, max) = points.iter().fold((first, first), |(min, max), p| {
        (
            World {
                x: min.x.min(p.x),
                y: min.y.min(p.y),
            },
            World {
                x: max.x.max(p.x),
                y: max.y.max(p.y),
            },
        )
    });
    Some(Ring { points, min, max })
}

/// Radius in px of a marker holding `photos` photos: big enough to make the
/// photo out, and growing with the count, but slowly, so a home town with
/// thousands doesn't cover a continent.
fn bubble_radius(photos: usize) -> f64 {
    18.0 + 2.0 * (photos.max(1) as f64).ln()
}

/// What the map shows: which part of the world, at what size.
#[derive(Clone, Copy, Debug)]
struct View {
    /// The world point at the middle of the widget.
    center: World,
    /// px per world unit: the whole world is this wide on screen.
    scale: f64,
}

impl View {
    fn to_screen(self, p: World, (width, height): (f64, f64)) -> (f64, f64) {
        (
            (p.x - self.center.x) * self.scale + width / 2.0,
            (p.y - self.center.y) * self.scale + height / 2.0,
        )
    }

    /// The world's corners at the widget's top left and bottom right.
    fn bounds(&self, (width, height): (f64, f64)) -> (World, World) {
        (
            World {
                x: self.center.x - width / 2.0 / self.scale,
                y: self.center.y - height / 2.0 / self.scale,
            },
            World {
                x: self.center.x + width / 2.0 / self.scale,
                y: self.center.y + height / 2.0 / self.scale,
            },
        )
    }
}

/// What a marker stands for.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    /// A town: an index into [`MapState::places`].
    Place(usize),
    /// One photo of a town: the town's index, and the photo's index in the
    /// town's [`MapState::locations`].
    Photo { place: usize, index: usize },
}

/// One thing the map marks: a town, or one photo when zoomed in close.
struct Marker {
    at: World,
    photos: usize,
    /// The photo it shows.
    cover: String,
    target: Target,
}

/// One marker as last drawn, standing for one or more [`Marker`]s: where, how
/// big, and which (indices into [`MapState::markers`], the one with the most
/// photos first).
struct Bubble {
    x: f64,
    y: f64,
    radius: f64,
    photos: usize,
    members: Vec<usize>,
}

/// A street-map tile ready to draw, and the names on it.
struct CachedTile {
    surface: cairo::ImageSurface,
    labels: Rc<Vec<Label>>,
    /// The frame it was last drawn in, for dropping the stalest first.
    used: u64,
}

/// The street map, while it is turned on.
struct Online {
    tiles: Tiles,
    cache: HashMap<TileKey, CachedTile>,
    pending: HashSet<TileKey>,
    failed: HashMap<TileKey, Instant>,
    frame: u64,
    /// Whether the cached tiles were drawn for a dark window.
    dark: bool,
}

/// Something the map needs from the app, asked for once drawing is done.
enum Want {
    Locations(u32),
    Thumb(PhotoItem),
}

struct MapState {
    places: Vec<(PlaceInfo, World)>,
    /// Each photo of a town and where it was taken, by town id, once asked
    /// for from close up.
    locations: HashMap<u32, Vec<(PhotoItem, World)>>,
    asked_locations: HashSet<u32>,
    /// Marker photos, scaled and cropped to a square, by photo uid.
    covers: HashMap<String, cairo::ImageSurface>,
    asked_thumbs: HashSet<String>,
    view: View,
    /// False until the view has been fitted to the places at a real size, and
    /// again whenever new places arrive before the user moved the map.
    fitted: bool,
    moved: bool,
    markers: Vec<Marker>,
    bubbles: Vec<Bubble>,
    pointer: (f64, f64),
    /// The view a drag started from.
    drag_origin: Option<View>,
    /// The scale a pinch started from.
    pinch_origin: Option<View>,
    online: Option<Online>,
    cache_dir: Option<PathBuf>,
    /// The spot [`PlacesMap::show_spot`] went to, ringed until the map is
    /// fitted to all places again.
    pin: Option<World>,
}

type OpenPlace = Box<dyn Fn(PlaceInfo)>;
type OpenPhoto = Box<dyn Fn(PlaceInfo, String)>;
type WantLocations = Box<dyn Fn(u32)>;
type WantThumb = Box<dyn Fn(PhotoItem)>;
type OpenSpot = Box<dyn Fn(Vec<(PlaceInfo, PhotoItem)>, (f64, f64))>;

/// What the map asks of the app around it.
#[derive(Default)]
struct Hooks {
    open_place: Option<OpenPlace>,
    open_photo: Option<OpenPhoto>,
    want_locations: Option<WantLocations>,
    want_thumb: Option<WantThumb>,
    open_spot: Option<OpenSpot>,
}

/// The Places map widget.
pub(crate) struct PlacesMap {
    root: gtk4::Overlay,
    area: gtk4::DrawingArea,
    credit: gtk4::Label,
    state: RefCell<MapState>,
    hooks: RefCell<Hooks>,
}

impl PlacesMap {
    pub(crate) fn new() -> Rc<Self> {
        let area = gtk4::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .has_tooltip(true)
            .build();
        area.add_css_class("places-map");

        let zoom_in = map_button("zoom-in-symbolic", &gettext("Zoom In"));
        let zoom_out = map_button("zoom-out-symbolic", &gettext("Zoom Out"));
        let fit = map_button("zoom-fit-best-symbolic", &gettext("Show All Places"));
        let zoom = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        zoom.add_css_class("linked");
        zoom.append(&zoom_in);
        zoom.append(&zoom_out);
        let buttons = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        buttons.set_halign(gtk4::Align::End);
        buttons.set_valign(gtk4::Align::Start);
        buttons.set_margin_top(12);
        buttons.set_margin_end(12);
        buttons.append(&zoom);
        buttons.append(&fit);

        let link = |url: &str, name: &str| format!("<a href=\"{url}\">{name}</a>");
        let credit = gtk4::Label::builder()
            .use_markup(true)
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::End)
            .visible(false)
            .label(gettext_f(
                // Translators: the credit on the street map. {openfreemap},
                // {openmaptiles} and {openstreetmap} are the linked names of
                // the projects, which stay as they are.
                "{openfreemap} © {openmaptiles} Data from {openstreetmap}",
                &[
                    (
                        "openfreemap",
                        &link("https://openfreemap.org", "OpenFreeMap"),
                    ),
                    (
                        "openmaptiles",
                        &link("https://www.openmaptiles.org/", "OpenMapTiles"),
                    ),
                    (
                        "openstreetmap",
                        &link("https://www.openstreetmap.org/copyright", "OpenStreetMap"),
                    ),
                ],
            ))
            .build();
        credit.add_css_class("places-map-credit");
        credit.add_css_class("caption");

        let root = gtk4::Overlay::new();
        root.set_child(Some(&area));
        root.add_overlay(&buttons);
        root.add_overlay(&credit);
        root.set_overflow(gtk4::Overflow::Hidden);
        root.add_css_class("places-map-frame");

        let map = Rc::new(Self {
            root,
            area,
            credit,
            state: RefCell::new(MapState {
                places: Vec::new(),
                locations: HashMap::new(),
                asked_locations: HashSet::new(),
                covers: HashMap::new(),
                asked_thumbs: HashSet::new(),
                view: View {
                    center: World { x: 0.5, y: 0.5 },
                    scale: 1.0,
                },
                fitted: false,
                moved: false,
                markers: Vec::new(),
                bubbles: Vec::new(),
                pointer: (0.0, 0.0),
                drag_origin: None,
                pinch_origin: None,
                online: None,
                cache_dir: None,
                pin: None,
            }),
            hooks: RefCell::new(Hooks::default()),
        });

        let weak = Rc::downgrade(&map);
        map.area.set_draw_func(move |area, cr, width, height| {
            if let Some(map) = weak.upgrade() {
                map.draw(area, cr, f64::from(width), f64::from(height));
            }
        });
        let weak = Rc::downgrade(&map);
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(map) = weak.upgrade() {
                map.area.queue_draw();
            }
        });
        map.wire(&zoom_in, &zoom_out, &fit);
        map
    }

    pub(crate) fn widget(&self) -> &gtk4::Overlay {
        &self.root
    }

    /// What clicking a town does.
    pub(crate) fn connect_open(&self, f: impl Fn(PlaceInfo) + 'static) {
        self.hooks.borrow_mut().open_place = Some(Box::new(f));
    }

    /// What clicking a single photo does: it gets the photo's town and uid.
    pub(crate) fn connect_open_photo(&self, f: impl Fn(PlaceInfo, String) + 'static) {
        self.hooks.borrow_mut().open_photo = Some(Box::new(f));
    }

    /// How the map gets the photos of a town with their locations, once it is
    /// zoomed in on it: the app answers with [`Self::set_locations`].
    pub(crate) fn connect_want_locations(&self, f: impl Fn(u32) + 'static) {
        self.hooks.borrow_mut().want_locations = Some(Box::new(f));
    }

    /// How the map gets a photo's thumbnail: the app answers with
    /// [`Self::set_cover`].
    pub(crate) fn connect_want_thumb(&self, f: impl Fn(PhotoItem) + 'static) {
        self.hooks.borrow_mut().want_thumb = Some(Box::new(f));
    }

    /// `f` gets the photos of a marker that can't part any further, newest
    /// first, and the point in the map widget they were clicked at.
    pub(crate) fn connect_open_spot(
        &self,
        f: impl Fn(Vec<(PlaceInfo, PhotoItem)>, (f64, f64)) + 'static,
    ) {
        self.hooks.borrow_mut().open_spot = Some(Box::new(f));
    }

    /// Show `places`. The view is fitted to them unless the user has already
    /// moved the map, so a refresh while they look around leaves it be.
    pub(crate) fn set_places(&self, places: &[PlaceInfo]) {
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        // A town whose count changed has new photos: ask for them again.
        let counts: HashMap<u32, usize> = places.iter().map(|p| (p.id, p.photo_count)).collect();
        let before: HashMap<u32, usize> = state
            .places
            .iter()
            .map(|(p, _)| (p.id, p.photo_count))
            .collect();
        state
            .locations
            .retain(|id, _| counts.get(id) == before.get(id));
        state
            .asked_locations
            .retain(|id| counts.get(id) == before.get(id));
        state.places = places
            .iter()
            .filter(|p| p.latitude != 0.0 || p.longitude != 0.0)
            .map(|p| (p.clone(), project(p.latitude, p.longitude)))
            .collect();
        if !state.moved {
            state.fitted = false;
        }
        self.area.queue_draw();
    }

    /// The photos of town `id` and where each was taken.
    pub(crate) fn set_locations(&self, id: u32, photos: Vec<PhotoLocation>) {
        let located = photos
            .into_iter()
            .filter(|p| p.latitude != 0.0 || p.longitude != 0.0)
            .map(|p| {
                let at = project(p.latitude, p.longitude);
                (p.photo, at)
            })
            .collect();
        self.state.borrow_mut().locations.insert(id, located);
        self.area.queue_draw();
    }

    /// Zoom in on where a photo was taken, close enough for it to have its own
    /// marker, and ring the spot.
    pub(crate) fn show_spot(&self, latitude: f64, longitude: f64) {
        let at = project(latitude, longitude);
        let scale = (PHOTO_SCALE * SPOT_ZOOM).clamp(self.min_scale(), self.max_scale());
        {
            let mut state = self.state.borrow_mut();
            state.view = View { center: at, scale };
            state.pin = Some(at);
            state.fitted = true;
            state.moved = true;
        }
        self.clamp_view();
        self.area.queue_draw();
    }

    /// Show `texture` in the markers of photo `uid`.
    pub(crate) fn set_cover(&self, uid: &str, texture: &gtk4::gdk::Texture) {
        if self.state.borrow().covers.contains_key(uid) {
            return;
        }
        let Some(cover) = square_cover(texture) else {
            return;
        };
        self.state.borrow_mut().covers.insert(uid.to_owned(), cover);
        self.area.queue_draw();
    }

    /// Draw OpenFreeMap's street map under the markers, or stop, keeping its
    /// tiles in `cache_dir`.
    pub(crate) fn set_online(self: &Rc<Self>, on: bool, cache_dir: PathBuf) {
        {
            let mut state = self.state.borrow_mut();
            state.cache_dir = Some(cache_dir);
            if on == state.online.is_some() {
                return;
            }
            state.online = None;
        }
        self.credit.set_visible(on);
        if on {
            self.start_tiles();
        }
        self.clamp_zoom();
        self.area.queue_draw();
    }

    /// Start a tile worker for the widget's current pixel density.
    fn start_tiles(self: &Rc<Self>) {
        let Some(dir) = self.state.borrow().cache_dir.clone() else {
            return;
        };
        let language = gtk4::pango::Language::default().to_string();
        let language = language.split(['-', '_']).next().unwrap_or("en").to_owned();
        let (tiles, done) = Tiles::start(
            dir.join("map-tiles"),
            language,
            self.area.scale_factor().max(1),
        );
        self.state.borrow_mut().online = Some(Online {
            tiles,
            cache: HashMap::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            frame: 0,
            dark: adw::StyleManager::default().is_dark(),
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(done) = done.recv().await {
                let Some(map) = weak.upgrade() else {
                    break;
                };
                map.tile_done(done);
            }
        });
    }

    fn tile_done(&self, done: Done) {
        let mut state = self.state.borrow_mut();
        let Some(online) = state.online.as_mut() else {
            return;
        };
        match done {
            Done::Tile(mut raster) => {
                let key = raster.key;
                online.pending.remove(&key);
                if raster.dark != online.dark {
                    return;
                }
                let labels = Rc::new(std::mem::take(&mut raster.labels));
                let Some(surface) = raster.surface() else {
                    return;
                };
                online.cache.insert(
                    key,
                    CachedTile {
                        surface,
                        labels,
                        used: online.frame,
                    },
                );
                evict(&mut online.cache, online.frame);
            }
            Done::Failed(key) => {
                online.pending.remove(&key);
                online.failed.insert(key, Instant::now());
            }
            Done::Dropped(key) => {
                online.pending.remove(&key);
            }
        }
        drop(state);
        self.area.queue_draw();
    }

    fn wire(self: &Rc<Self>, zoom_in: &gtk4::Button, zoom_out: &gtk4::Button, fit: &gtk4::Button) {
        let weak = Rc::downgrade(self);
        zoom_in.connect_clicked(move |_| {
            if let Some(map) = weak.upgrade() {
                map.zoom_at_middle(BUTTON_ZOOM_STEP);
            }
        });
        let weak = Rc::downgrade(self);
        zoom_out.connect_clicked(move |_| {
            if let Some(map) = weak.upgrade() {
                map.zoom_at_middle(1.0 / BUTTON_ZOOM_STEP);
            }
        });
        let weak = Rc::downgrade(self);
        fit.connect_clicked(move |_| {
            if let Some(map) = weak.upgrade() {
                let mut state = map.state.borrow_mut();
                state.moved = false;
                state.fitted = false;
                drop(state);
                map.area.queue_draw();
            }
        });

        let motion = gtk4::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            if let Some(map) = weak.upgrade() {
                map.state.borrow_mut().pointer = (x, y);
                let over = map.bubble_at(x, y).is_some();
                map.area
                    .set_cursor_from_name(Some(if over { "pointer" } else { "default" }));
            }
        });
        self.area.add_controller(motion);

        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _, dy| {
            let Some(map) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let (x, y) = map.state.borrow().pointer;
            map.zoom_at(x, y, ZOOM_STEP.powf(-dy));
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);

        // One drag gesture for both panning and clicking: a press that barely
        // moves is a click on whatever marker it started on.
        let drag = gtk4::GestureDrag::new();
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(map) = weak.upgrade() {
                let mut state = map.state.borrow_mut();
                state.drag_origin = Some(state.view);
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(map) = weak.upgrade() else {
                return;
            };
            if dx.hypot(dy) < CLICK_SLOP {
                return;
            }
            let mut state = map.state.borrow_mut();
            let Some(origin) = state.drag_origin else {
                return;
            };
            state.moved = true;
            state.view = View {
                center: World {
                    x: origin.center.x - dx / origin.scale,
                    y: origin.center.y - dy / origin.scale,
                },
                ..origin
            };
            drop(state);
            map.clamp_view();
            map.area.queue_draw();
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |gesture, dx, dy| {
            let Some(map) = weak.upgrade() else {
                return;
            };
            map.state.borrow_mut().drag_origin = None;
            if dx.hypot(dy) >= CLICK_SLOP {
                return;
            }
            if let Some((x, y)) = gesture.start_point() {
                map.click(x, y);
            }
        });
        self.area.add_controller(drag);

        let pinch = gtk4::GestureZoom::new();
        let weak = Rc::downgrade(self);
        pinch.connect_begin(move |_, _| {
            if let Some(map) = weak.upgrade() {
                let mut state = map.state.borrow_mut();
                state.pinch_origin = Some(state.view);
            }
        });
        let weak = Rc::downgrade(self);
        pinch.connect_scale_changed(move |gesture, scale| {
            let Some(map) = weak.upgrade() else {
                return;
            };
            let Some(origin) = map.state.borrow().pinch_origin else {
                return;
            };
            let (x, y) = gesture
                .bounding_box_center()
                .unwrap_or_else(|| map.middle());
            let now = map.state.borrow().view.scale;
            map.zoom_at(x, y, origin.scale * scale / now);
        });
        self.area.add_controller(pinch);

        let weak = Rc::downgrade(self);
        self.area
            .connect_query_tooltip(move |_, x, y, _keyboard, tooltip| {
                let Some(map) = weak.upgrade() else {
                    return false;
                };
                let Some(text) = map.tooltip_at(f64::from(x), f64::from(y)) else {
                    return false;
                };
                tooltip.set_text(Some(&text));
                true
            });
    }

    fn size(&self) -> (f64, f64) {
        (f64::from(self.area.width()), f64::from(self.area.height()))
    }

    fn middle(&self) -> (f64, f64) {
        let (width, height) = self.size();
        (width / 2.0, height / 2.0)
    }

    /// The whole world's width at the least zoom: the widget's own width.
    fn min_scale(&self) -> f64 {
        f64::from(self.area.width().max(self.area.height()).max(1))
    }

    fn max_scale(&self) -> f64 {
        let zoom = if self.state.borrow().online.is_some() {
            ONLINE_MAX_ZOOM
        } else {
            MAX_ZOOM
        };
        self.min_scale() * zoom
    }

    fn zoom_at_middle(&self, factor: f64) {
        let (x, y) = self.middle();
        self.zoom_at(x, y, factor);
    }

    /// Zoom by `factor`, keeping the world point under `(x, y)` where it is.
    fn zoom_at(&self, x: f64, y: f64, factor: f64) {
        let (min, max) = (self.min_scale(), self.max_scale());
        let (width, height) = self.size();
        {
            let mut state = self.state.borrow_mut();
            let view = state.view;
            let scale = (view.scale * factor).clamp(min, max);
            let under = World {
                x: view.center.x + (x - width / 2.0) / view.scale,
                y: view.center.y + (y - height / 2.0) / view.scale,
            };
            state.moved = true;
            state.view = View {
                center: World {
                    x: under.x - (x - width / 2.0) / scale,
                    y: under.y - (y - height / 2.0) / scale,
                },
                scale,
            };
        }
        self.clamp_view();
        self.area.queue_draw();
    }

    /// Back out to the built-in map's deepest zoom when the street map goes.
    fn clamp_zoom(&self) {
        let max = self.max_scale();
        let mut state = self.state.borrow_mut();
        state.view.scale = state.view.scale.min(max);
        drop(state);
        self.clamp_view();
    }

    /// Keep the world on screen: the map can't be dragged off into nothing.
    fn clamp_view(&self) {
        let (width, height) = self.size();
        let mut state = self.state.borrow_mut();
        let mut view = state.view;
        view.center.x = clamp_axis(view.center.x, width / 2.0 / view.scale);
        view.center.y = clamp_axis(view.center.y, height / 2.0 / view.scale);
        state.view = view;
    }

    /// Frame every place, or the whole world when there are none.
    fn fit(&self, width: f64, height: f64) {
        let min = self.min_scale();
        let mut state = self.state.borrow_mut();
        let view = match fit_view(&state.places, width, height) {
            Some(view) => View {
                scale: view.scale.clamp(min, min * FIT_MAX_ZOOM),
                ..view
            },
            None => View {
                center: World { x: 0.5, y: 0.5 },
                scale: min,
            },
        };
        state.view = view;
        state.fitted = true;
        state.pin = None;
        drop(state);
        self.clamp_view();
    }

    fn draw(
        self: &Rc<Self>,
        area: &gtk4::DrawingArea,
        cr: &cairo::Context,
        width: f64,
        height: f64,
    ) {
        if width < 1.0 || height < 1.0 {
            return;
        }
        if !self.state.borrow().fitted {
            self.fit(width, height);
        }
        // Tiles made for another pixel density would come out blurred.
        let restart = self
            .state
            .borrow()
            .online
            .as_ref()
            .is_some_and(|o| o.tiles.scale != area.scale_factor().max(1));
        if restart {
            self.state.borrow_mut().online = None;
            self.start_tiles();
        }

        let dark = adw::StyleManager::default().is_dark();
        let colors = map_tiles::palette(dark);
        let accent = area.color();
        let size = (width, height);
        let mut wants = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            let view = state.view;
            paint_outline(cr, view, size, colors);
            let labels = match state.online.as_mut() {
                Some(online) => paint_tiles(cr, online, view, size, dark, area.scale_factor()),
                None => Vec::new(),
            };
            if !labels.is_empty() {
                paint_labels(cr, area, &labels, view, size, colors);
            }

            state.markers = markers(state, &mut wants, size);
            let screen: Vec<(f64, f64)> = state
                .markers
                .iter()
                .map(|m| view.to_screen(m.at, size))
                .collect();
            let counts: Vec<usize> = state.markers.iter().map(|m| m.photos).collect();
            state.bubbles = cluster(&screen, &counts, width, height);
            if let Some(pin) = state.pin {
                let (x, y) = view.to_screen(pin, size);
                paint_pin(cr, x, y, &accent);
            }
            for bubble in &state.bubbles {
                let marker = &state.markers[bubble.members[0]];
                let cover = state.covers.get(&marker.cover);
                if cover.is_none()
                    && let Target::Photo { place, index } = marker.target
                    && state.asked_thumbs.insert(marker.cover.clone())
                {
                    let id = state.places[place].0.id;
                    if let Some((photo, _)) = state.locations.get(&id).and_then(|p| p.get(index)) {
                        wants.push(Want::Thumb(photo.clone()));
                    }
                }
                paint_bubble(cr, bubble, cover, &accent);
            }
        }
        if !wants.is_empty() {
            // Not from inside the draw: an answer may come back at once.
            let weak = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(map) = weak.upgrade() {
                    map.ask(wants);
                }
            });
        }
    }

    fn ask(&self, wants: Vec<Want>) {
        let hooks = self.hooks.borrow();
        for want in wants {
            match want {
                Want::Locations(id) => {
                    if let Some(f) = hooks.want_locations.as_ref() {
                        f(id);
                    }
                }
                Want::Thumb(photo) => {
                    if let Some(f) = hooks.want_thumb.as_ref() {
                        f(photo);
                    }
                }
            }
        }
    }
}

/// The markers to draw: every town, or its photos one by one once the map is
/// close enough and they have come. Towns on screen whose photos are still
/// to be asked for go into `wants`.
fn markers(state: &mut MapState, wants: &mut Vec<Want>, size: (f64, f64)) -> Vec<Marker> {
    let close = state.view.scale >= PHOTO_SCALE;
    let (min, max) = state.view.bounds(size);
    let mut markers = Vec::new();
    for (i, (place, at)) in state.places.iter().enumerate() {
        if close && let Some(photos) = state.locations.get(&place.id) {
            markers.extend(
                photos
                    .iter()
                    .enumerate()
                    .map(|(index, (photo, at))| Marker {
                        at: *at,
                        photos: 1,
                        cover: photo.uid.clone(),
                        target: Target::Photo { place: i, index },
                    }),
            );
            continue;
        }
        // A town reaches a little past its centre: its photos may be on
        // screen when it isn't.
        let margin = 0.2 * (max.x - min.x);
        let near = at.x > min.x - margin
            && at.x < max.x + margin
            && at.y > min.y - margin
            && at.y < max.y + margin;
        if close && near && state.asked_locations.insert(place.id) {
            wants.push(Want::Locations(place.id));
        }
        markers.push(Marker {
            at: *at,
            photos: place.photo_count,
            cover: place.cover.uid.clone(),
            target: Target::Place(i),
        });
    }
    markers
}

/// The built-in map: sea, land, coasts and country borders.
fn paint_outline(cr: &cairo::Context, view: View, size: (f64, f64), colors: &Palette) {
    map_tiles::set_rgb(cr, colors.water, 1.0);
    let _ = cr.paint();
    let (min, max) = view.bounds(size);
    let visible = |ring: &Ring| {
        ring.max.x >= min.x && ring.min.x <= max.x && ring.max.y >= min.y && ring.min.y <= max.y
    };
    let trace = |ring: &Ring, close: bool| {
        let mut last: Option<(f64, f64)> = None;
        for &point in &ring.points {
            let (x, y) = view.to_screen(point, size);
            match last {
                None => cr.move_to(x, y),
                Some((lx, ly)) if (x - lx).hypot(y - ly) < MIN_SEGMENT => continue,
                Some(_) => cr.line_to(x, y),
            }
            last = Some((x, y));
        }
        if close {
            cr.close_path();
        }
    };
    cr.new_path();
    for ring in land().iter().filter(|r| visible(r)) {
        trace(ring, true);
    }
    map_tiles::set_rgb(cr, colors.land, 1.0);
    let _ = cr.fill_preserve();
    map_tiles::set_rgb(cr, colors.coast, 1.0);
    cr.set_line_width(0.75);
    let _ = cr.stroke();

    cr.new_path();
    for line in borders().iter().filter(|r| visible(r)) {
        trace(line, false);
    }
    map_tiles::set_rgb(cr, colors.border, 0.7);
    cr.set_line_width(0.75);
    cr.set_line_join(cairo::LineJoin::Round);
    let _ = cr.stroke();
}

/// The street-map zoom whose tiles come closest to their own size on screen.
fn tile_zoom(scale: f64) -> u8 {
    (scale / TILE_EDGE).log2().round().clamp(0.0, 18.0) as u8
}

/// Draw the street-map tiles on screen, asking for the missing ones and
/// standing in their nearest ancestor meanwhile. Returns the names on them.
fn paint_tiles(
    cr: &cairo::Context,
    online: &mut Online,
    view: View,
    size: (f64, f64),
    dark: bool,
    device_scale: i32,
) -> Vec<Rc<Vec<Label>>> {
    if online.dark != dark {
        online.dark = dark;
        online.cache.clear();
        online.pending.clear();
    }
    online.frame += 1;
    let z = tile_zoom(view.scale);
    let n = 1u32 << z;
    let (min, max) = view.bounds(size);
    let range = |lo: f64, hi: f64| {
        let first = (lo * f64::from(n)).floor().clamp(0.0, f64::from(n - 1)) as u32;
        let last = (hi * f64::from(n)).floor().clamp(0.0, f64::from(n - 1)) as u32;
        first..=last
    };
    let mut keys: Vec<TileKey> = range(min.y, max.y)
        .flat_map(|y| range(min.x, max.x).map(move |x| TileKey { z, x, y }))
        .collect();
    // The middle first: that is where the eye is.
    let middle = (view.center.x * f64::from(n), view.center.y * f64::from(n));
    keys.sort_by(|a, b| {
        let d =
            |k: &TileKey| (f64::from(k.x) + 0.5 - middle.0).hypot(f64::from(k.y) + 0.5 - middle.1);
        d(a).total_cmp(&d(b))
    });
    online.tiles.set_wanted(keys.iter().copied().collect());

    let snap = |v: f64| {
        let s = f64::from(device_scale.max(1));
        (v * s).round() / s
    };
    let screen_rect = |key: TileKey| {
        let n = f64::from(1u32 << key.z);
        let (x0, y0) = view.to_screen(
            World {
                x: f64::from(key.x) / n,
                y: f64::from(key.y) / n,
            },
            size,
        );
        let (x1, y1) = view.to_screen(
            World {
                x: f64::from(key.x + 1) / n,
                y: f64::from(key.y + 1) / n,
            },
            size,
        );
        (snap(x0), snap(y0), snap(x1), snap(y1))
    };

    let now = Instant::now();
    online
        .failed
        .retain(|_, at| now.duration_since(*at) < TILE_RETRY);
    let mut labels = Vec::new();
    for key in keys {
        let mut source = Some(key);
        if !online.cache.contains_key(&key)
            && !online.pending.contains(&key)
            && !online.failed.contains_key(&key)
        {
            online.pending.insert(key);
            online.tiles.request(key, dark);
        }
        while let Some(candidate) = source {
            if online.cache.contains_key(&candidate) {
                break;
            }
            source = candidate.parent();
        }
        let Some(tile) = source.and_then(|k| online.cache.get_mut(&k).map(|t| (k, t))) else {
            continue;
        };
        let (from, tile) = tile;
        tile.used = online.frame;
        if !labels.iter().any(|l| Rc::ptr_eq(l, &tile.labels)) {
            labels.push(tile.labels.clone());
        }

        let (cx0, cy0, cx1, cy1) = screen_rect(key);
        let (x0, y0, x1, y1) = screen_rect(from);
        let edge = f64::from(tile.surface.width());
        cr.save().ok();
        cr.rectangle(cx0, cy0, cx1 - cx0, cy1 - cy0);
        cr.clip();
        cr.translate(x0, y0);
        cr.scale((x1 - x0) / edge, (y1 - y0) / edge);
        if cr.set_source_surface(&tile.surface, 0.0, 0.0).is_ok() {
            let pattern = cr.source();
            pattern.set_extend(cairo::Extend::Pad);
            pattern.set_filter(cairo::Filter::Good);
            let _ = cr.paint();
        }
        cr.restore().ok();
    }
    labels
}

/// Drop the tiles drawn longest ago, down to [`TILE_CACHE`], never one drawn
/// in the current frame.
fn evict(cache: &mut HashMap<TileKey, CachedTile>, frame: u64) {
    if cache.len() <= TILE_CACHE {
        return;
    }
    let mut ages: Vec<(u64, TileKey)> = cache
        .iter()
        .filter(|(_, t)| t.used < frame)
        .map(|(k, t)| (t.used, *k))
        .collect();
    ages.sort_unstable_by_key(|(used, _)| *used);
    let excess = cache.len() - TILE_CACHE;
    for (_, key) in ages.into_iter().take(excess) {
        cache.remove(&key);
    }
}

/// Set the names of the tiles on screen, the most important first, leaving
/// out any that would overlap one already set.
fn paint_labels(
    cr: &cairo::Context,
    area: &gtk4::DrawingArea,
    tiles: &[Rc<Vec<Label>>],
    view: View,
    size: (f64, f64),
    colors: &Palette,
) {
    let z = tile_zoom(view.scale);
    let mut seen = HashSet::new();
    let mut labels: Vec<&Label> = tiles
        .iter()
        .flat_map(|t| t.iter())
        .filter(|l| (l.class.min_zoom()..=l.class.max_zoom()).contains(&z))
        // The same name comes with every tile drawn from one data tile.
        .filter(|l| seen.insert((l.text.as_str(), l.x.to_bits(), l.y.to_bits())))
        .collect();
    labels.sort_by(|a, b| {
        a.class
            .cmp(&b.class)
            .then(a.rank.total_cmp(&b.rank))
            .then(a.text.cmp(&b.text))
    });

    let mut placed: Vec<(f64, f64, f64, f64)> = Vec::new();
    for label in labels {
        if placed.len() >= MAX_LABELS {
            break;
        }
        let (x, y) = view.to_screen(
            World {
                x: label.x,
                y: label.y,
            },
            size,
        );
        if x < 0.0 || y < 0.0 || x > size.0 || y > size.1 {
            continue;
        }
        let layout = area.create_pango_layout(Some(&label.text));
        let mut font = layout
            .context()
            .font_description()
            .unwrap_or_else(gtk4::pango::FontDescription::new);
        let (points, weight, style) = match label.class {
            LabelClass::Country => (11.5, gtk4::pango::Weight::Bold, gtk4::pango::Style::Normal),
            LabelClass::State => (9.5, gtk4::pango::Weight::Normal, gtk4::pango::Style::Normal),
            LabelClass::City => (
                11.0,
                gtk4::pango::Weight::Semibold,
                gtk4::pango::Style::Normal,
            ),
            LabelClass::Town => (
                10.0,
                gtk4::pango::Weight::Semibold,
                gtk4::pango::Style::Normal,
            ),
            LabelClass::Village | LabelClass::Suburb => {
                (9.0, gtk4::pango::Weight::Normal, gtk4::pango::Style::Normal)
            }
            LabelClass::Hamlet => (8.5, gtk4::pango::Weight::Normal, gtk4::pango::Style::Normal),
            LabelClass::Water => (9.5, gtk4::pango::Weight::Normal, gtk4::pango::Style::Italic),
        };
        font.set_size((points * f64::from(gtk4::pango::SCALE)) as i32);
        font.set_weight(weight);
        font.set_style(style);
        layout.set_font_description(Some(&font));
        let (_, logical) = layout.pixel_extents();
        let (w, h) = (f64::from(logical.width()), f64::from(logical.height()));
        let rect = (x - w / 2.0 - 3.0, y - h / 2.0 - 2.0, w + 6.0, h + 4.0);
        let overlaps = placed.iter().any(|p| {
            rect.0 < p.0 + p.2
                && p.0 < rect.0 + rect.2
                && rect.1 < p.1 + p.3
                && p.1 < rect.1 + rect.3
        });
        if overlaps {
            continue;
        }
        placed.push(rect);

        cr.new_path();
        cr.move_to(x - w / 2.0, y - h / 2.0);
        pangocairo::functions::layout_path(cr, &layout);
        map_tiles::set_rgb(cr, colors.halo, 0.85);
        cr.set_line_width(3.0);
        cr.set_line_join(cairo::LineJoin::Round);
        let _ = cr.stroke_preserve();
        let text = if label.class == LabelClass::Water {
            colors.water_text
        } else {
            colors.text
        };
        map_tiles::set_rgb(cr, text, 1.0);
        let _ = cr.fill();
    }
}

/// One marker: its photo in a ringed circle, with a badge counting the
/// photos when it stands for more than one — or an accent disc with the
/// count until the photo is there.
/// A ring in the accent colour around a spot, wide enough to show around a
/// single photo's marker drawn over it.
fn paint_pin(cr: &cairo::Context, x: f64, y: f64, accent: &gtk4::gdk::RGBA) {
    cr.new_path();
    cr.arc(x, y, bubble_radius(1) + 6.0, 0.0, std::f64::consts::TAU);
    set_color(cr, accent, 0.35);
    let _ = cr.fill_preserve();
    set_color(cr, accent, 1.0);
    cr.set_line_width(3.0);
    let _ = cr.stroke();
}

fn paint_bubble(
    cr: &cairo::Context,
    bubble: &Bubble,
    cover: Option<&cairo::ImageSurface>,
    accent: &gtk4::gdk::RGBA,
) {
    let (x, y, r) = (bubble.x, bubble.y, bubble.radius);
    cr.new_path();
    cr.arc(x, y + 1.0, r + 1.5, 0.0, std::f64::consts::TAU);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
    let _ = cr.fill();

    cr.new_path();
    cr.arc(x, y, r, 0.0, std::f64::consts::TAU);
    match cover {
        Some(cover) => {
            cr.save().ok();
            cr.clip_preserve();
            let edge = f64::from(cover.width());
            cr.translate(x - r, y - r);
            cr.scale(2.0 * r / edge, 2.0 * r / edge);
            if cr.set_source_surface(cover, 0.0, 0.0).is_ok() {
                cr.source().set_filter(cairo::Filter::Good);
                let _ = cr.paint();
            }
            cr.restore().ok();
        }
        None => {
            set_color(cr, accent, 1.0);
            let _ = cr.fill_preserve();
        }
    }
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.set_line_width(2.0);
    let _ = cr.stroke();

    // Cairo's own text is enough for a few digits, and needs no layout.
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    // One photo needs no count; it shows once its thumbnail is there.
    if bubble.photos < 2 {
        return;
    }
    let label = compact_count(bubble.photos);
    if cover.is_none() {
        cr.set_font_size(r.clamp(7.0, 13.0));
        if let Ok(extents) = cr.text_extents(&label)
            && extents.width() <= r * 1.8
        {
            cr.move_to(
                x - extents.width() / 2.0 - extents.x_bearing(),
                y - extents.height() / 2.0 - extents.y_bearing(),
            );
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            let _ = cr.show_text(&label);
        }
        return;
    }
    cr.set_font_size(10.0);
    let Ok(extents) = cr.text_extents(&label) else {
        return;
    };
    let h = 16.0;
    let w = (extents.width() + 10.0).max(h);
    let (bx, by) = (x + r * 0.7 - w / 2.0, y - r * 0.7 - h / 2.0);
    cr.new_path();
    cr.arc(
        bx + h / 2.0,
        by + h / 2.0,
        h / 2.0,
        0.5 * std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    cr.arc(
        bx + w - h / 2.0,
        by + h / 2.0,
        h / 2.0,
        -0.5 * std::f64::consts::PI,
        0.5 * std::f64::consts::PI,
    );
    cr.close_path();
    set_color(cr, accent, 1.0);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.set_line_width(1.5);
    let _ = cr.stroke();
    cr.move_to(
        bx + w / 2.0 - extents.width() / 2.0 - extents.x_bearing(),
        by + h / 2.0 - extents.height() / 2.0 - extents.y_bearing(),
    );
    let _ = cr.show_text(&label);
}

/// `texture` cut to a centred square of [`COVER_PX`], in the form cairo
/// draws from.
fn square_cover(texture: &gtk4::gdk::Texture) -> Option<cairo::ImageSurface> {
    let (width, height) = (texture.width(), texture.height());
    if width < 1 || height < 1 {
        return None;
    }
    // GDK's default download format is cairo's ARGB32: premultiplied, in
    // native byte order.
    let stride = width as usize * 4;
    let mut pixels = vec![0u8; stride * height as usize];
    texture.download(&mut pixels, stride);
    let full = cairo::ImageSurface::create_for_data(
        pixels,
        cairo::Format::ARgb32,
        width,
        height,
        stride as i32,
    )
    .ok()?;
    let cover = cairo::ImageSurface::create(cairo::Format::ARgb32, COVER_PX, COVER_PX).ok()?;
    {
        let cr = cairo::Context::new(&cover).ok()?;
        let side = f64::from(width.min(height));
        let scale = f64::from(COVER_PX) / side;
        cr.scale(scale, scale);
        cr.set_source_surface(
            &full,
            -(f64::from(width) - side) / 2.0,
            -(f64::from(height) - side) / 2.0,
        )
        .ok()?;
        cr.source().set_filter(cairo::Filter::Good);
        cr.paint().ok()?;
    }
    Some(cover)
}

impl PlacesMap {
    /// The members of the marker under `(x, y)`, the topmost (last drawn)
    /// first.
    fn bubble_at(&self, x: f64, y: f64) -> Option<Vec<usize>> {
        self.bubble_hit(x, y).map(|(members, _)| members)
    }

    /// [`Self::bubble_at`], with the middle of the marker.
    fn bubble_hit(&self, x: f64, y: f64) -> Option<(Vec<usize>, (f64, f64))> {
        let state = self.state.borrow();
        state
            .bubbles
            .iter()
            .rev()
            .find(|b| (b.x - x).hypot(b.y - y) <= b.radius)
            .map(|b| (b.members.clone(), (b.x, b.y)))
    }

    /// The photos behind markers `members`, newest first.
    fn photos_of(&self, members: &[usize]) -> Vec<(PlaceInfo, PhotoItem)> {
        let state = self.state.borrow();
        let mut photos: Vec<(PlaceInfo, PhotoItem)> = members
            .iter()
            .filter_map(|&i| {
                let Target::Photo { place, index } = state.markers.get(i)?.target else {
                    return None;
                };
                let place = &state.places[place].0;
                let (photo, _) = state.locations.get(&place.id)?.get(index)?;
                Some((place.clone(), photo.clone()))
            })
            .collect();
        photos.sort_by_key(|(_, photo)| std::cmp::Reverse(photo.capture_time));
        photos
    }

    /// The town and photo uid of marker `index`.
    fn photo_of(&self, index: usize) -> Option<(PlaceInfo, String)> {
        let state = self.state.borrow();
        let Target::Photo { place, index } = state.markers.get(index)?.target else {
            return None;
        };
        let place = state.places[place].0.clone();
        let (photo, _) = state.locations.get(&place.id)?.get(index)?;
        Some((place, photo.uid.clone()))
    }

    fn click(&self, x: f64, y: f64) {
        let Some((members, middle)) = self.bubble_hit(x, y) else {
            return;
        };
        let first = self.state.borrow().markers[members[0]].target.clone();
        let open_photo = |index| {
            if let Some((place, uid)) = self.photo_of(index)
                && let Some(open) = self.hooks.borrow().open_photo.as_ref()
            {
                open(place, uid);
            }
        };
        if let [only] = members[..] {
            match first {
                Target::Place(place) => {
                    let place = self.state.borrow().places[place].0.clone();
                    if let Some(open) = self.hooks.borrow().open_place.as_ref() {
                        open(place);
                    }
                }
                Target::Photo { .. } => open_photo(only),
            }
            return;
        }
        // A merged marker zooms in on what it holds until that parts — and
        // photos taken on one spot, which never part, are listed.
        let (width, height) = self.size();
        let (target, span) = {
            let state = self.state.borrow();
            let points: Vec<World> = members.iter().map(|&i| state.markers[i].at).collect();
            let photos: Vec<usize> = members.iter().map(|&i| state.markers[i].photos).collect();
            (fit_points(&points, &photos, width, height), spread(&points))
        };
        let (min, max) = (self.min_scale(), self.max_scale());
        let at_max = self.state.borrow().view.scale >= max * 0.999;
        let parts = span * max > bubble_radius(1);
        if (at_max || !parts) && matches!(first, Target::Photo { .. }) {
            let photos = self.photos_of(&members);
            if let Some(open) = self.hooks.borrow().open_spot.as_ref() {
                open(photos, middle);
            }
            return;
        }
        let Some(target) = target else {
            return;
        };
        {
            let mut state = self.state.borrow_mut();
            let view = state.view;
            state.moved = true;
            state.view = View {
                center: target.center,
                // At least one real step in, even when the markers sit on top
                // of each other at every zoom the fit would pick.
                scale: target
                    .scale
                    .max(view.scale * BUTTON_ZOOM_STEP)
                    .clamp(min, max),
            };
        }
        self.clamp_view();
        self.area.queue_draw();
    }

    /// "Berlin" over "Germany · 12 photos" for a town, the town and the date
    /// for a photo, or the count over the first few towns for a merged
    /// marker.
    fn tooltip_at(&self, x: f64, y: f64) -> Option<String> {
        let members = self.bubble_at(x, y)?;
        let state = self.state.borrow();
        let place_of = |i: usize| match state.markers[i].target {
            Target::Place(p) | Target::Photo { place: p, .. } => &state.places[p].0,
        };
        if let [only] = members[..] {
            let place = place_of(only);
            return Some(match state.markers[only].target {
                Target::Place(_) => format!("{}\n{}", place.name, place_subtitle(place)),
                Target::Photo { index, .. } => {
                    let photo = &state.locations.get(&place.id)?.get(index)?.0;
                    format!(
                        "{}\n{}",
                        place.name,
                        format_capture_time(photo.capture_time)
                    )
                }
            });
        }
        let towns = members
            .iter()
            .all(|&i| matches!(state.markers[i].target, Target::Place(_)));
        let mut names: Vec<&str> = Vec::new();
        for &i in &members {
            let name = place_of(i).name.as_str();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let head = if towns {
            places_subtitle(members.len())
        } else {
            let photos: usize = members.iter().map(|&i| state.markers[i].photos).sum();
            ngettext_f("{n} photo", "{n} photos", photos as u64, &[])
        };
        let mut lines = vec![head];
        lines.extend(names.iter().take(TOOLTIP_NAMES).map(|n| (*n).to_owned()));
        if names.len() > TOOLTIP_NAMES {
            lines.push("…".to_owned());
        }
        Some(lines.join("\n"))
    }
}

/// How many town names a merged marker's tooltip lists.
const TOOLTIP_NAMES: usize = 5;

fn map_button(icon: &str, tooltip: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("osd");
    button
}

/// Keep `center` so that half a view, `half`, either side stays inside 0..1 —
/// or centred, when the view is wider than the world.
fn clamp_axis(center: f64, half: f64) -> f64 {
    if half * 2.0 >= 1.0 {
        0.5
    } else {
        center.clamp(half, 1.0 - half)
    }
}

/// The view framing `places` in a `width` × `height` widget, with room for
/// the markers at the edges. `None` when there is nothing to frame.
fn fit_view(places: &[(PlaceInfo, World)], width: f64, height: f64) -> Option<View> {
    let points: Vec<World> = places.iter().map(|(_, p)| *p).collect();
    let photos: Vec<usize> = places.iter().map(|(p, _)| p.photo_count).collect();
    fit_points(&points, &photos, width, height)
}

fn fit_points(points: &[World], photos: &[usize], width: f64, height: f64) -> Option<View> {
    let first = *points.first()?;
    let (min, max) = points.iter().fold((first, first), |(min, max), p| {
        (
            World {
                x: min.x.min(p.x),
                y: min.y.min(p.y),
            },
            World {
                x: max.x.max(p.x),
                y: max.y.max(p.y),
            },
        )
    });
    // A margin of the largest marker on every side, plus the zoom buttons'
    // column on the right.
    let margin = 2.0 * bubble_radius(photos.iter().copied().max()?) + 48.0;
    let span_x = (max.x - min.x).max(1e-9);
    let span_y = (max.y - min.y).max(1e-9);
    let scale =
        ((width - 2.0 * margin).max(1.0) / span_x).min((height - 2.0 * margin).max(1.0) / span_y);
    Some(View {
        center: World {
            x: (min.x + max.x) / 2.0,
            y: (min.y + max.y) / 2.0,
        },
        scale,
    })
}

/// The widest distance between any of `points`, in world units, along
/// either axis.
fn spread(points: &[World]) -> f64 {
    let xs = points.iter().map(|p| p.x);
    let ys = points.iter().map(|p| p.y);
    let span = |v: &mut dyn Iterator<Item = f64>| {
        let (lo, hi) = v.fold((f64::MAX, f64::MIN), |(lo, hi), x| (lo.min(x), hi.max(x)));
        (hi - lo).max(0.0)
    };
    span(&mut xs.into_iter()).max(span(&mut ys.into_iter()))
}

/// Merge markers whose circles would overlap on screen, the one with the
/// most photos claiming its neighbours (the first of equals, when counts
/// tie). Markers off screen are left out.
fn cluster(screen: &[(f64, f64)], counts: &[usize], width: f64, height: f64) -> Vec<Bubble> {
    let mut order: Vec<usize> = (0..screen.len()).collect();
    order.sort_by(|a, b| counts[*b].cmp(&counts[*a]));
    let mut bubbles: Vec<Bubble> = Vec::new();
    for index in order {
        let (x, y) = screen[index];
        let reach = bubble_radius(counts[index]);
        if x < -reach || y < -reach || x > width + reach || y > height + reach {
            continue;
        }
        match bubbles
            .iter_mut()
            .find(|b| (b.x - x).hypot(b.y - y) < b.radius + reach * 0.5)
        {
            Some(bubble) => {
                bubble.photos += counts[index];
                bubble.radius = bubble_radius(bubble.photos);
                bubble.members.push(index);
            }
            None => bubbles.push(Bubble {
                x,
                y,
                radius: reach,
                photos: counts[index],
                members: vec![index],
            }),
        }
    }
    // Smallest last, so a small marker next to a big one stays clickable.
    bubbles.sort_by(|a, b| b.radius.total_cmp(&a.radius));
    bubbles
}

/// "7", "840", "1.2k", "35k": short enough to fit a marker. Digits and the
/// "k" read the same in every language the app ships.
fn compact_count(n: usize) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{}k", n / 1_000),
    }
}

fn set_color(cr: &cairo::Context, color: &gtk4::gdk::RGBA, alpha: f32) {
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha() * alpha),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_puts_the_equator_and_meridian_in_the_middle() {
        let p = project(0.0, 0.0);
        assert!((p.x - 0.5).abs() < 1e-9 && (p.y - 0.5).abs() < 1e-9);
        // North is up, east is right.
        let berlin = project(52.52, 13.40);
        assert!(berlin.x > 0.5 && berlin.y < 0.5);
    }

    #[test]
    fn every_land_ring_parses() {
        assert_eq!(land().len(), LAND.lines().count());
        assert!(land().iter().all(|ring| ring.points.len() >= 4));
    }

    #[test]
    fn every_border_line_parses() {
        assert_eq!(borders().len(), BORDERS.lines().count());
        assert!(borders().iter().all(|line| line.points.len() >= 2));
    }

    #[test]
    fn overlapping_towns_merge_into_one_bubble() {
        let screen = [(100.0, 100.0), (104.0, 102.0), (400.0, 300.0)];
        let counts = [3, 10, 1];
        let bubbles = cluster(&screen, &counts, 800.0, 600.0);
        assert_eq!(bubbles.len(), 2);
        let merged = bubbles.iter().find(|b| b.members.len() == 2).unwrap();
        // The town with more photos claims the other and keeps its position.
        assert_eq!(merged.members, vec![1, 0]);
        assert_eq!((merged.x, merged.photos), (104.0, 13));
    }

    #[test]
    fn photos_of_equal_weight_merge_under_the_first() {
        let screen = [(200.0, 200.0), (205.0, 200.0), (210.0, 200.0)];
        let bubbles = cluster(&screen, &[1, 1, 1], 800.0, 600.0);
        assert_eq!(bubbles.len(), 1);
        assert_eq!(bubbles[0].members, vec![0, 1, 2]);
    }

    #[test]
    fn towns_off_screen_get_no_bubble() {
        let bubbles = cluster(&[(-500.0, 10.0)], &[1], 800.0, 600.0);
        assert!(bubbles.is_empty());
    }

    #[test]
    fn counts_shorten_to_fit_a_bubble() {
        assert_eq!(compact_count(840), "840");
        assert_eq!(compact_count(1_240), "1.2k");
        assert_eq!(compact_count(35_900), "35k");
    }

    #[test]
    fn tiles_come_closest_to_their_own_size() {
        assert_eq!(tile_zoom(256.0), 0);
        assert_eq!(tile_zoom(1_000.0), 2);
        assert_eq!(tile_zoom(1e12), 18);
        assert_eq!(tile_zoom(1.0), 0);
    }

    #[test]
    fn photos_on_one_spot_have_no_spread() {
        let spot = project(49.0069, 8.4037);
        assert_eq!(spread(&[spot, spot]), 0.0);
        let near = project(49.0100, 8.4100);
        assert!(spread(&[spot, near]) > 0.0);
    }
}
