//! The map of the Places view: the towns the photos were taken in, drawn as
//! bubbles over an outline of the world's land.
//!
//! The map is drawn here rather than loaded from a tile server: no tile source
//! with terms that allow an app like this one to use it is free, and a town
//! bubble needs no more detail than coastlines. The land is Natural Earth's
//! 1:50m land polygons (public domain), trimmed into `data/land.txt` by
//! `scripts/natural-earth-land.py`.
//!
//! The projection is Web Mercator, the one every other map uses, so the
//! world looks the way people expect. Towns that would overlap on screen merge
//! into one bubble counting all their photos; clicking it zooms in until they
//! part, clicking a single town opens it.

use std::sync::OnceLock;

use crate::*;

const LAND: &str = include_str!("../../../data/land.txt");

/// Latitude past which Mercator is cut off, as on every web map.
const MAX_LATITUDE: f64 = 85.0;

/// How far in the map zooms, as a multiple of the whole world fitting the
/// widget's width.
const MAX_ZOOM: f64 = 2048.0;

/// How close "show all" zooms onto a single town, the same way.
const FIT_MAX_ZOOM: f64 = 64.0;

/// The zoom step of one scroll notch and of the zoom buttons.
const ZOOM_STEP: f64 = 1.25;
const BUTTON_ZOOM_STEP: f64 = 2.0;

/// A press that moves less than this many px is a click, not a drag.
const CLICK_SLOP: f64 = 4.0;

/// Coastline points closer than this many px to the last one drawn are
/// skipped: at world zoom most of the outline would fall onto one pixel.
const MIN_SEGMENT: f64 = 0.75;

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

/// One land outline, projected, with its bounding box for culling.
struct Ring {
    points: Vec<World>,
    min: World,
    max: World,
}

fn land() -> &'static [Ring] {
    static LAND_RINGS: OnceLock<Vec<Ring>> = OnceLock::new();
    LAND_RINGS.get_or_init(|| LAND.lines().filter_map(parse_ring).collect())
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

/// Radius in px of a bubble holding `photos` photos: grows with the count,
/// but slowly, so a home town with thousands doesn't cover a continent.
fn bubble_radius(photos: usize) -> f64 {
    7.0 + 3.0 * (photos.max(1) as f64).ln()
}

/// What the map shows: which part of the world, at what size.
#[derive(Clone, Copy, Debug)]
struct View {
    /// The world point at the middle of the widget.
    center: World,
    /// px per world unit: the whole world is this wide on screen.
    scale: f64,
}

/// One bubble as last drawn: where, how big, and the places it stands for
/// (indices into [`MapState::places`], the one with the most photos first).
struct Bubble {
    x: f64,
    y: f64,
    radius: f64,
    photos: usize,
    members: Vec<usize>,
}

struct MapState {
    places: Vec<(PlaceInfo, World)>,
    view: Cell<View>,
    /// False until the view has been fitted to the places at a real size, and
    /// again whenever new places arrive before the user moved the map.
    fitted: Cell<bool>,
    moved: Cell<bool>,
    bubbles: RefCell<Vec<Bubble>>,
    pointer: Cell<(f64, f64)>,
    /// The view a drag started from.
    drag_origin: Cell<Option<View>>,
    /// The scale a pinch started from.
    pinch_origin: Cell<Option<View>>,
}

/// What clicking a town on the map does.
type OpenPlace = Box<dyn Fn(PlaceInfo)>;

/// The Places map widget.
pub(crate) struct PlacesMap {
    root: gtk4::Overlay,
    area: gtk4::DrawingArea,
    state: RefCell<MapState>,
    on_open: RefCell<Option<OpenPlace>>,
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

        let root = gtk4::Overlay::new();
        root.set_child(Some(&area));
        root.add_overlay(&buttons);
        root.set_overflow(gtk4::Overflow::Hidden);
        root.add_css_class("places-map-frame");

        let map = Rc::new(Self {
            root,
            area,
            state: RefCell::new(MapState {
                places: Vec::new(),
                view: Cell::new(View {
                    center: World { x: 0.5, y: 0.5 },
                    scale: 1.0,
                }),
                fitted: Cell::new(false),
                moved: Cell::new(false),
                bubbles: RefCell::new(Vec::new()),
                pointer: Cell::new((0.0, 0.0)),
                drag_origin: Cell::new(None),
                pinch_origin: Cell::new(None),
            }),
            on_open: RefCell::new(None),
        });

        let weak = Rc::downgrade(&map);
        map.area.set_draw_func(move |area, cr, width, height| {
            if let Some(map) = weak.upgrade() {
                map.draw(area, cr, f64::from(width), f64::from(height));
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
        self.on_open.replace(Some(Box::new(f)));
    }

    /// Show `places`. The view is fitted to them unless the user has already
    /// moved the map, so a refresh while they look around leaves it be.
    pub(crate) fn set_places(&self, places: &[PlaceInfo]) {
        let mut state = self.state.borrow_mut();
        state.places = places
            .iter()
            .filter(|p| p.latitude != 0.0 || p.longitude != 0.0)
            .map(|p| (p.clone(), project(p.latitude, p.longitude)))
            .collect();
        if !state.moved.get() {
            state.fitted.set(false);
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
                let state = map.state.borrow();
                state.moved.set(false);
                state.fitted.set(false);
                drop(state);
                map.area.queue_draw();
            }
        });

        let motion = gtk4::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            if let Some(map) = weak.upgrade() {
                map.state.borrow().pointer.set((x, y));
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
            let (x, y) = map.state.borrow().pointer.get();
            map.zoom_at(x, y, ZOOM_STEP.powf(-dy));
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);

        // One drag gesture for both panning and clicking: a press that barely
        // moves is a click on whatever bubble it started on.
        let drag = gtk4::GestureDrag::new();
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(map) = weak.upgrade() {
                let state = map.state.borrow();
                state.drag_origin.set(Some(state.view.get()));
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
            let state = map.state.borrow();
            let Some(origin) = state.drag_origin.get() else {
                return;
            };
            state.moved.set(true);
            state.view.set(View {
                center: World {
                    x: origin.center.x - dx / origin.scale,
                    y: origin.center.y - dy / origin.scale,
                },
                ..origin
            });
            drop(state);
            map.clamp_view();
            map.area.queue_draw();
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |gesture, dx, dy| {
            let Some(map) = weak.upgrade() else {
                return;
            };
            map.state.borrow().drag_origin.set(None);
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
                let state = map.state.borrow();
                state.pinch_origin.set(Some(state.view.get()));
            }
        });
        let weak = Rc::downgrade(self);
        pinch.connect_scale_changed(move |gesture, scale| {
            let Some(map) = weak.upgrade() else {
                return;
            };
            let Some(origin) = map.state.borrow().pinch_origin.get() else {
                return;
            };
            let (x, y) = gesture
                .bounding_box_center()
                .unwrap_or_else(|| map.middle());
            let now = map.state.borrow().view.get().scale;
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

    fn middle(&self) -> (f64, f64) {
        (
            f64::from(self.area.width()) / 2.0,
            f64::from(self.area.height()) / 2.0,
        )
    }

    /// The whole world's width at the least zoom: the widget's own width.
    fn min_scale(&self) -> f64 {
        f64::from(self.area.width().max(self.area.height()).max(1))
    }

    fn zoom_at_middle(&self, factor: f64) {
        let (x, y) = self.middle();
        self.zoom_at(x, y, factor);
    }

    /// Zoom by `factor`, keeping the world point under `(x, y)` where it is.
    fn zoom_at(&self, x: f64, y: f64, factor: f64) {
        let min = self.min_scale();
        let (width, height) = (f64::from(self.area.width()), f64::from(self.area.height()));
        {
            let state = self.state.borrow();
            let view = state.view.get();
            let scale = (view.scale * factor).clamp(min, min * MAX_ZOOM);
            let under = World {
                x: view.center.x + (x - width / 2.0) / view.scale,
                y: view.center.y + (y - height / 2.0) / view.scale,
            };
            state.moved.set(true);
            state.view.set(View {
                center: World {
                    x: under.x - (x - width / 2.0) / scale,
                    y: under.y - (y - height / 2.0) / scale,
                },
                scale,
            });
        }
        self.clamp_view();
        self.area.queue_draw();
    }

    /// Keep the world on screen: the map can't be dragged off into nothing.
    fn clamp_view(&self) {
        let (width, height) = (f64::from(self.area.width()), f64::from(self.area.height()));
        let state = self.state.borrow();
        let mut view = state.view.get();
        view.center.x = clamp_axis(view.center.x, width / 2.0 / view.scale);
        view.center.y = clamp_axis(view.center.y, height / 2.0 / view.scale);
        state.view.set(view);
    }

    /// Frame every place, or the whole world when there are none.
    fn fit(&self, width: f64, height: f64) {
        let min = self.min_scale();
        let state = self.state.borrow();
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
        state.view.set(view);
        state.fitted.set(true);
        drop(state);
        self.clamp_view();
    }

    fn draw(&self, area: &gtk4::DrawingArea, cr: &gtk4::cairo::Context, width: f64, height: f64) {
        if width < 1.0 || height < 1.0 {
            return;
        }
        if !self.state.borrow().fitted.get() {
            self.fit(width, height);
        }
        let state = self.state.borrow();
        // Land in the text colour at low alpha, so it reads in light and dark
        // alike; bubbles in the accent colour the stylesheet gives the area.
        let text = area.parent().map_or_else(|| area.color(), |p| p.color());
        let accent = area.color();
        let bubbles = paint(
            cr,
            state.view.get(),
            (width, height),
            &state.places,
            (&text, &accent),
        );
        state.bubbles.replace(bubbles);
    }
}

/// Draw the land and the bubbles for `places` as seen through `view`, and
/// hand back the bubbles for hit-testing.
fn paint(
    cr: &gtk4::cairo::Context,
    view: View,
    (width, height): (f64, f64),
    places: &[(PlaceInfo, World)],
    (text, accent): (&gtk4::gdk::RGBA, &gtk4::gdk::RGBA),
) -> Vec<Bubble> {
    let to_screen = |p: World| {
        (
            (p.x - view.center.x) * view.scale + width / 2.0,
            (p.y - view.center.y) * view.scale + height / 2.0,
        )
    };
    let min = World {
        x: view.center.x - width / 2.0 / view.scale,
        y: view.center.y - height / 2.0 / view.scale,
    };
    let max = World {
        x: view.center.x + width / 2.0 / view.scale,
        y: view.center.y + height / 2.0 / view.scale,
    };
    for ring in land() {
        if ring.max.x < min.x || ring.min.x > max.x || ring.max.y < min.y || ring.min.y > max.y {
            continue;
        }
        let mut last: Option<(f64, f64)> = None;
        for &point in &ring.points {
            let (x, y) = to_screen(point);
            match last {
                None => cr.move_to(x, y),
                Some((lx, ly)) if (x - lx).hypot(y - ly) < MIN_SEGMENT => continue,
                Some(_) => cr.line_to(x, y),
            }
            last = Some((x, y));
        }
        cr.close_path();
    }
    set_color(cr, text, 0.12);
    let _ = cr.fill_preserve();
    set_color(cr, text, 0.25);
    cr.set_line_width(0.75);
    let _ = cr.stroke();

    let screen: Vec<(f64, f64)> = places.iter().map(|(_, p)| to_screen(*p)).collect();
    let counts: Vec<usize> = places.iter().map(|(p, _)| p.photo_count).collect();
    let bubbles = cluster(&screen, &counts, width, height);

    // Cairo's own text is enough for a few digits, and needs no layout.
    cr.select_font_face(
        "Sans",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Bold,
    );
    for bubble in &bubbles {
        // The last label left a current point; an arc would line up from it.
        cr.new_path();
        cr.arc(
            bubble.x,
            bubble.y,
            bubble.radius,
            0.0,
            std::f64::consts::TAU,
        );
        set_color(cr, accent, 0.85);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
        cr.set_line_width(1.5);
        let _ = cr.stroke();

        let label = compact_count(bubble.photos);
        cr.set_font_size(bubble.radius.clamp(7.0, 14.0));
        let Ok(extents) = cr.text_extents(&label) else {
            continue;
        };
        if extents.width() > bubble.radius * 1.8 {
            continue;
        }
        cr.move_to(
            bubble.x - extents.width() / 2.0 - extents.x_bearing(),
            bubble.y - extents.height() / 2.0 - extents.y_bearing(),
        );
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        let _ = cr.show_text(&label);
    }
    bubbles
}

impl PlacesMap {
    /// The bubble under `(x, y)`, the topmost (last drawn) first.
    fn bubble_at(&self, x: f64, y: f64) -> Option<Vec<usize>> {
        let state = self.state.borrow();
        let bubbles = state.bubbles.borrow();
        bubbles
            .iter()
            .rev()
            .find(|b| (b.x - x).hypot(b.y - y) <= b.radius)
            .map(|b| b.members.clone())
    }

    fn click(&self, x: f64, y: f64) {
        let Some(members) = self.bubble_at(x, y) else {
            return;
        };
        if let [only] = members[..] {
            let place = self.state.borrow().places[only].0.clone();
            if let Some(open) = self.on_open.borrow().as_ref() {
                open(place);
            }
            return;
        }
        // A merged bubble zooms in on its towns until they part.
        let (width, height) = (f64::from(self.area.width()), f64::from(self.area.height()));
        let target = {
            let state = self.state.borrow();
            let chosen: Vec<(PlaceInfo, World)> =
                members.iter().map(|&i| state.places[i].clone()).collect();
            fit_view(&chosen, width, height)
        };
        let Some(target) = target else {
            return;
        };
        let min = self.min_scale();
        {
            let state = self.state.borrow();
            let view = state.view.get();
            state.moved.set(true);
            state.view.set(View {
                center: target.center,
                // At least one real step in, even when the towns sit on top of
                // each other at every zoom the fit would pick.
                scale: target
                    .scale
                    .max(view.scale * BUTTON_ZOOM_STEP)
                    .clamp(min, min * MAX_ZOOM),
            });
        }
        self.clamp_view();
        self.area.queue_draw();
    }

    /// "Berlin" over "Germany · 12 photos" for a town, or the number of places
    /// over the first few of their names for a merged bubble.
    fn tooltip_at(&self, x: f64, y: f64) -> Option<String> {
        let members = self.bubble_at(x, y)?;
        let state = self.state.borrow();
        if let [only] = members[..] {
            let place = &state.places[only].0;
            return Some(format!("{}\n{}", place.name, place_subtitle(place)));
        }
        let mut lines = vec![places_subtitle(members.len())];
        lines.extend(
            members
                .iter()
                .take(TOOLTIP_NAMES)
                .map(|&i| state.places[i].0.name.clone()),
        );
        if members.len() > TOOLTIP_NAMES {
            lines.push("…".to_owned());
        }
        Some(lines.join("\n"))
    }
}

/// How many town names a merged bubble's tooltip lists.
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
/// the bubbles at the edges. `None` when there is nothing to frame.
fn fit_view(places: &[(PlaceInfo, World)], width: f64, height: f64) -> Option<View> {
    let first = places.first()?.1;
    let (min, max) = places.iter().fold((first, first), |(min, max), (_, p)| {
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
    // A margin of the largest bubble on every side, plus the zoom buttons'
    // column on the right.
    let margin = 2.0 * bubble_radius(places.iter().map(|(p, _)| p.photo_count).max()?) + 48.0;
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

/// Merge places whose bubbles would overlap on screen, the one with the most
/// photos claiming its neighbours. Places off screen are left out.
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
    // Smallest last, so a small bubble next to a big one stays clickable.
    bubbles.sort_by(|a, b| b.radius.total_cmp(&a.radius));
    bubbles
}

/// "7", "840", "1.2k", "35k": short enough to fit inside a bubble. Digits
/// and the "k" read the same in every language the app ships.
fn compact_count(n: usize) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{}k", n / 1_000),
    }
}

fn set_color(cr: &gtk4::cairo::Context, color: &gtk4::gdk::RGBA, alpha: f32) {
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
}
