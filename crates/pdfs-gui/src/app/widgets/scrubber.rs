//! The Photos timeline's month scrubber, laid over the grid's right edge as in
//! Google Photos: a thin tick where each year starts, a knob at the month on
//! screen, and a bubble naming the month under the pointer. It stays out of
//! the way until the pointer nears the edge or the timeline scrolls.

use crate::*;
use gtk4::cairo;

/// Width of the strip the scrubber draws in, in px.
const STRIP_WIDTH: i32 = 72;
/// How close to the right edge the pointer has to come to show the scrubber.
const EDGE: f64 = 96.0;
/// Space kept free above the first and below the last month, in px.
const PAD: f64 = 16.0;
/// How long the scrubber stays up after the timeline stops scrolling.
const LINGER: Duration = Duration::from_millis(1500);

#[derive(Default)]
struct State {
    /// How many months the strip spans, newest at the top.
    months: usize,
    /// Where each year starts, as `(month index, label)`.
    years: Vec<(usize, String)>,
    /// The month the knob marks.
    value: usize,
    /// Whether the timeline has months to move between at all.
    available: bool,
    /// The pointer is near the right edge.
    near: bool,
    /// The month under the pointer while it is on the strip.
    hover: Option<usize>,
    /// Where the running drag started, in the strip's coordinates.
    drag_from: Option<f64>,
    /// Keeps the scrubber up for a moment after a scroll.
    linger: Option<glib::SourceId>,
    label: Option<Rc<dyn Fn(usize) -> String>>,
    on_pick: Option<Rc<dyn Fn(usize)>>,
}

/// The scrubber's widgets and state. Cheap to clone: every clone is the same
/// scrubber.
#[derive(Clone)]
pub(crate) struct Scrubber {
    revealer: gtk4::Revealer,
    area: gtk4::DrawingArea,
    knob: gtk4::Box,
    bubble: gtk4::Label,
    /// The timeline the scrubber lies over.
    scroll: Option<gtk4::ScrolledWindow>,
    state: Rc<RefCell<State>>,
}

impl Scrubber {
    /// Build the scrubber over `host`, whose child is the scrolled timeline.
    pub(crate) fn new(host: &gtk4::Overlay) -> Self {
        let scroll = host.child().and_downcast::<gtk4::ScrolledWindow>();
        let area = gtk4::DrawingArea::builder()
            .width_request(STRIP_WIDTH)
            .vexpand(true)
            .build();
        area.add_css_class("photo-scrubber");
        let knob = gtk4::Box::builder()
            .width_request(24)
            .height_request(4)
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Start)
            .margin_end(4)
            .can_target(false)
            .build();
        knob.add_css_class("photo-scrubber-knob");
        let strip = gtk4::Overlay::new();
        strip.set_child(Some(&area));
        strip.add_overlay(&knob);
        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::Crossfade)
            .transition_duration(150)
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Fill)
            .can_target(false)
            .child(&strip)
            .build();
        let bubble = gtk4::Label::builder()
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Start)
            .margin_end(STRIP_WIDTH)
            .can_target(false)
            .visible(false)
            .build();
        bubble.add_css_class("photo-scrubber-bubble");
        host.add_overlay(&revealer);
        host.add_overlay(&bubble);

        let scrubber = Self {
            revealer,
            area,
            knob,
            bubble,
            scroll,
            state: Rc::default(),
        };
        scrubber.wire(host);
        scrubber
    }

    fn wire(&self, host: &gtk4::Overlay) {
        let this = self.clone();
        self.area.set_draw_func(move |area, cr, width, height| {
            this.draw(area, cr, width, height);
        });
        let this = self.clone();
        self.area.connect_resize(move |_, _, _| this.place_knob());

        // Near the edge is judged on the whole timeline, not the strip: the
        // strip takes no input while it is hidden.
        let edge = gtk4::EventControllerMotion::new();
        let this = self.clone();
        let host_motion = host.clone();
        edge.connect_motion(move |_, x, _| {
            let near = x >= f64::from(host_motion.width()) - EDGE;
            if this.state.borrow().near != near {
                this.state.borrow_mut().near = near;
                this.sync_reveal();
            }
        });
        let this = self.clone();
        edge.connect_leave(move |_| {
            this.state.borrow_mut().near = false;
            this.sync_reveal();
        });
        host.add_controller(edge);

        let hover = gtk4::EventControllerMotion::new();
        let this = self.clone();
        hover.connect_motion(move |_, _, y| {
            let index = this.index_at(y);
            this.state.borrow_mut().hover = index;
            this.sync_bubble();
        });
        let this = self.clone();
        hover.connect_leave(move |_| {
            this.state.borrow_mut().hover = None;
            this.sync_bubble();
        });
        self.area.add_controller(hover);

        // A click is a drag that goes nowhere, so this covers both.
        let drag = gtk4::GestureDrag::new();
        let this = self.clone();
        drag.connect_drag_begin(move |_, _, y| {
            this.state.borrow_mut().drag_from = Some(y);
            this.pick(y, true);
        });
        let this = self.clone();
        drag.connect_drag_update(move |_, _, dy| {
            let from = this.state.borrow().drag_from;
            if let Some(from) = from {
                this.pick(from + dy, false);
            }
        });
        let this = self.clone();
        drag.connect_drag_end(move |_, _, _| {
            this.state.borrow_mut().drag_from = None;
            this.sync_reveal();
            this.sync_bubble();
        });
        self.area.add_controller(drag);
    }

    /// Span `months` months, newest first, with a tick and a label where each
    /// of `years` starts. Moves the knob back to the top.
    pub(crate) fn set_months(&self, months: usize, years: Vec<(usize, String)>) {
        {
            let mut state = self.state.borrow_mut();
            state.months = months;
            state.years = years;
            state.value = 0;
        }
        self.area.queue_draw();
        self.place_knob();
    }

    /// Whether there is anything to scrub through. The scrubber never shows
    /// while there is not.
    pub(crate) fn set_available(&self, available: bool) {
        self.state.borrow_mut().available = available;
        // The scrubber takes the scrollbar's place at the edge; both at once
        // would fight over the same strip.
        if let Some(scroll) = &self.scroll {
            scroll.set_vscrollbar_policy(if available {
                gtk4::PolicyType::External
            } else {
                gtk4::PolicyType::Automatic
            });
        }
        self.sync_reveal();
    }

    pub(crate) fn is_available(&self) -> bool {
        self.state.borrow().available
    }

    /// Mark month `index`, unless the user has the scrubber in hand: the
    /// timeline's scroll position must not move the knob out from under them.
    pub(crate) fn set_value(&self, index: usize) {
        {
            let mut state = self.state.borrow_mut();
            if state.drag_from.is_some() || state.hover.is_some() || state.value == index {
                return;
            }
            state.value = index;
        }
        self.place_knob();
    }

    /// How a month index reads in the bubble, such as "March 2024".
    pub(crate) fn set_label_func(&self, label: impl Fn(usize) -> String + 'static) {
        self.state.borrow_mut().label = Some(Rc::new(label));
    }

    /// Call `f` with the month the user points the scrubber at.
    pub(crate) fn connect_pick(&self, f: impl Fn(usize) + 'static) {
        self.state.borrow_mut().on_pick = Some(Rc::new(f));
    }

    /// Show the scrubber for a moment, as the timeline scrolls, so it can be
    /// found without hunting for the edge — and on a touch screen at all.
    pub(crate) fn flash(&self) {
        if !self.is_available() {
            return;
        }
        if let Some(source) = self.state.borrow_mut().linger.take() {
            source.remove();
        }
        let this = self.clone();
        let source = glib::timeout_add_local_once(LINGER, move || {
            this.state.borrow_mut().linger = None;
            this.sync_reveal();
        });
        self.state.borrow_mut().linger = Some(source);
        self.sync_reveal();
    }

    fn sync_reveal(&self) {
        let show = {
            let state = self.state.borrow();
            state.available && (state.near || state.drag_from.is_some() || state.linger.is_some())
        };
        self.revealer.set_reveal_child(show);
        // A hidden revealer still claims its strip of the grid; let the clicks
        // through to the tiles underneath.
        self.revealer.set_can_target(show);
        if !show {
            self.state.borrow_mut().hover = None;
            self.bubble.set_visible(false);
        }
    }

    /// The bubble names the month being dragged to, or else the one under
    /// the pointer.
    fn sync_bubble(&self) {
        let (index, label) = {
            let state = self.state.borrow();
            let index = if state.drag_from.is_some() {
                Some(state.value)
            } else {
                state.hover
            };
            (index, state.label.clone())
        };
        let (Some(index), Some(label)) = (index, label) else {
            self.bubble.set_visible(false);
            return;
        };
        self.bubble.set_label(&label(index));
        let (_, height, _, _) = self.bubble.measure(gtk4::Orientation::Vertical, -1);
        let y = self.y_of(index) - f64::from(height) / 2.0;
        self.bubble.set_margin_top(y.max(0.0) as i32);
        self.bubble.set_visible(true);
    }

    /// Move the knob to month `y` points at, and tell the page when that is a
    /// different month — or when a new drag starts, so a click jumps even to
    /// the month already marked.
    fn pick(&self, y: f64, starting: bool) {
        let Some(index) = self.index_at(y) else {
            return;
        };
        let (changed, on_pick) = {
            let mut state = self.state.borrow_mut();
            let changed = state.value != index;
            state.value = index;
            (changed, state.on_pick.clone())
        };
        self.place_knob();
        self.sync_bubble();
        if let (true, Some(on_pick)) = (changed || starting, on_pick) {
            on_pick(index);
        }
    }

    fn place_knob(&self) {
        let value = self.state.borrow().value;
        let y = self.y_of(value) - 2.0;
        self.knob.set_margin_top(y.max(0.0) as i32);
    }

    /// Where month `index` sits on the strip.
    fn y_of(&self, index: usize) -> f64 {
        let months = self.state.borrow().months;
        month_y(index, months, f64::from(self.area.height()))
    }

    /// The month at height `y` on the strip, if it spans any.
    fn index_at(&self, y: f64) -> Option<usize> {
        let months = self.state.borrow().months;
        month_at(y, months, f64::from(self.area.height()))
    }

    fn draw(&self, area: &gtk4::DrawingArea, cr: &cairo::Context, width: i32, height: i32) {
        let state = self.state.borrow();
        let color = area.color();
        let (width, height) = (f64::from(width), f64::from(height));
        let attrs = gtk4::pango::AttrList::new();
        attrs.insert(gtk4::pango::AttrFloat::new_scale(0.8));
        attrs.insert(gtk4::pango::AttrInt::new_weight(gtk4::pango::Weight::Bold));
        // The bottom of the last label drawn: a year crowded against the one
        // above it keeps its tick but not its label.
        let mut clear_below = f64::NEG_INFINITY;
        for (index, year) in &state.years {
            let y = month_y(*index, state.months, height);
            cr.set_source_rgba(
                f64::from(color.red()),
                f64::from(color.green()),
                f64::from(color.blue()),
                0.5,
            );
            cr.rectangle(width - 12.0, y.round() - 0.5, 8.0, 1.0);
            let _ = cr.fill();

            let layout = area.create_pango_layout(Some(year));
            layout.set_attributes(Some(&attrs));
            let (_, logical) = layout.pixel_extents();
            let (w, h) = (f64::from(logical.width()), f64::from(logical.height()));
            let top = y - h / 2.0;
            if top < clear_below {
                continue;
            }
            clear_below = top + h + 2.0;
            cr.set_source_rgba(
                f64::from(color.red()),
                f64::from(color.green()),
                f64::from(color.blue()),
                0.8,
            );
            cr.move_to(width - 16.0 - w, top);
            pangocairo::functions::show_layout(cr, &layout);
        }
    }
}

/// Where month `index` of `months` sits on a strip `height` px tall: evenly
/// spaced, newest at the top.
fn month_y(index: usize, months: usize, height: f64) -> f64 {
    let span = (height - 2.0 * PAD).max(0.0);
    if months < 2 {
        return PAD;
    }
    PAD + span * index as f64 / (months - 1) as f64
}

/// The month nearest height `y` on a strip `height` px tall.
fn month_at(y: f64, months: usize, height: f64) -> Option<usize> {
    let last = months.checked_sub(1)?;
    let span = height - 2.0 * PAD;
    if span <= 0.0 {
        return Some(0);
    }
    let fraction = ((y - PAD) / span).clamp(0.0, 1.0);
    Some((fraction * last as f64).round() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scrubber_maps_heights_back_to_their_months() {
        let height = 432.0;
        for index in 0..12 {
            assert_eq!(
                month_at(month_y(index, 12, height), 12, height),
                Some(index)
            );
        }
    }

    #[test]
    fn the_scrubber_clamps_past_either_end() {
        assert_eq!(month_at(-50.0, 12, 432.0), Some(0));
        assert_eq!(month_at(900.0, 12, 432.0), Some(11));
        assert_eq!(month_at(10.0, 0, 432.0), None);
    }
}
