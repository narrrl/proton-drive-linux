use crate::*;

/// Side of a copy's thumbnail in the duplicates dialog, in px.
const COPY_SIZE: i32 = 150;

/// Each set's copies with their Keep toggles.
type SetToggles = Vec<Vec<(PhotoItem, gtk4::ToggleButton)>>;

/// How often the dialog asks again while the daemon hashes thumbnails.
const HASHING_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// What the finder looks for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Finder {
    /// Byte-identical copies, going by the server's content hash.
    Exact,
    /// Shots that look alike, going by their thumbnails.
    Similar,
}

/// The dialog's parts that a search fills in.
struct FinderView {
    ui: Rc<Ui>,
    status: adw::StatusPage,
    progress: gtk4::ProgressBar,
    note: gtk4::Label,
    stack: gtk4::Stack,
    trash: gtk4::Button,
    toolbar: adw::ToolbarView,
    /// Bumped by every new search and when the dialog closes, so a reply or a
    /// poll for an older one is dropped.
    generation: Cell<u32>,
    /// The Trash button's handler for the review on show.
    clicked: RefCell<Option<glib::SignalHandlerId>>,
}

/// Open the duplicate finder: every set of photos stored more than once, side
/// by side, with the copy worth keeping ticked. The copies left unticked go to
/// Trash — one file each, never the rest of their shot. The Similar switch
/// looks for shots that look alike instead.
pub(crate) fn show_duplicates(ui: &Rc<Ui>) {
    let progress = gtk4::ProgressBar::builder()
        .show_text(true)
        .width_request(280)
        .halign(gtk4::Align::Center)
        .visible(false)
        .build();
    let note = gtk4::Label::builder()
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .visible(false)
        .build();
    note.add_css_class("dim-label");
    let status_extra = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    status_extra.append(&progress);
    status_extra.append(&note);
    let status = adw::StatusPage::builder()
        .icon_name("edit-copy-symbolic")
        .vexpand(true)
        .child(&status_extra)
        .build();
    status.add_css_class("compact");

    let stack = gtk4::Stack::new();
    stack.add_named(&status, Some("status"));

    let trash = gtk4::Button::builder()
        .halign(gtk4::Align::End)
        .sensitive(false)
        .build();
    trash.add_css_class("destructive-action");
    trash.add_css_class("pill");
    let bottom = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bottom.add_css_class("toolbar");
    bottom.set_halign(gtk4::Align::End);
    bottom.append(&trash);

    let exact = gtk4::ToggleButton::builder()
        .label(pgettext("duplicates", "Exact"))
        .tooltip_text(gettext("Photos stored more than once"))
        .active(true)
        .build();
    let similar = gtk4::ToggleButton::builder()
        .label(pgettext("duplicates", "Similar"))
        .tooltip_text(gettext("Photos that look alike"))
        .group(&exact)
        .build();
    let switch = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    switch.add_css_class("linked");
    switch.append(&exact);
    switch.append(&similar);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switch));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    toolbar.add_bottom_bar(&bottom);
    toolbar.set_reveal_bottom_bars(false);

    let dialog = adw::Dialog::builder()
        .title(gettext("Duplicates"))
        .content_width(760)
        .content_height(640)
        .child(&toolbar)
        .build();
    dialog.present(ui_window(ui).as_ref());

    let view = Rc::new(FinderView {
        ui: ui.clone(),
        status,
        progress,
        note,
        stack,
        trash,
        toolbar,
        generation: Cell::new(0),
        clicked: RefCell::new(None),
    });
    let view_closed = view.clone();
    dialog.connect_closed(move |_| {
        view_closed
            .generation
            .set(view_closed.generation.get().wrapping_add(1));
    });
    let view_switch = view.clone();
    similar.connect_toggled(move |similar| {
        let finder = if similar.is_active() {
            Finder::Similar
        } else {
            Finder::Exact
        };
        search(&view_switch, finder);
    });
    search(&view, Finder::Exact);
}

/// Drop what the dialog shows and look again, for `finder`.
fn search(view: &Rc<FinderView>, finder: Finder) {
    let generation = view.generation.get().wrapping_add(1);
    view.generation.set(generation);
    if let Some(review) = view.stack.child_by_name("review") {
        view.stack.remove(&review);
    }
    if let Some(handler) = view.clicked.take() {
        view.trash.disconnect(handler);
    }
    view.toolbar.set_reveal_bottom_bars(false);
    view.status.set_icon_name(Some("edit-copy-symbolic"));
    view.status.set_title(&match finder {
        Finder::Exact => gettext("Looking for duplicates…"),
        Finder::Similar => gettext("Looking for similar photos…"),
    });
    view.status.set_description(None);
    view.progress.set_visible(false);
    view.note.set_visible(false);
    view.stack.set_visible_child_name("status");
    ask(view, finder, generation);
}

/// Ask the daemon for `finder`'s sets and show them, or, while it is still
/// hashing thumbnails, how far it has got — and ask again in a moment.
fn ask(view: &Rc<FinderView>, finder: Finder, generation: u32) {
    let request = match finder {
        Finder::Exact => Request::PhotoDuplicates,
        Finder::Similar => Request::PhotoSimilar,
    };
    let rx = spawn_request(view.ui.dirs.control_socket(), request);
    let view = view.clone();
    glib::spawn_future_local(async move {
        let reply = rx.recv().await;
        if view.generation.get() != generation {
            return;
        }
        let failed = match finder {
            Finder::Exact => gettext("Couldn't look for duplicates"),
            Finder::Similar => gettext("Couldn't look for similar photos"),
        };
        let (sets, uncompared) = match reply {
            Ok(Ok(Response::PhotoDuplicates { sets })) => (sets, 0),
            Ok(Ok(Response::PhotoSimilar {
                hashing: Some(MappingProgress { done, total }),
                ..
            })) => {
                view.status.set_title(&gettext("Comparing photos…"));
                view.status.set_description(Some(&gettext(
                    "Photos are compared by their thumbnails. The first time takes a while.",
                )));
                view.progress.set_fraction(if total == 0 {
                    0.0
                } else {
                    done as f64 / total as f64
                });
                view.progress.set_text(Some(&gettext_f(
                    // Translators: {done} and {total} are numbers of photos.
                    "{done} of {total}",
                    &[("done", &thousands(done)), ("total", &thousands(total))],
                )));
                view.progress.set_visible(true);
                glib::timeout_add_local_once(HASHING_POLL, move || {
                    if view.generation.get() == generation {
                        ask(&view, finder, generation);
                    }
                });
                return;
            }
            Ok(Ok(Response::PhotoSimilar {
                sets,
                hashing: None,
                uncompared,
            })) => (sets, uncompared),
            Ok(Ok(Response::Error { message, .. })) => {
                view.status.set_icon_name(Some("dialog-warning-symbolic"));
                view.status.set_title(&failed);
                view.status.set_description(Some(&message));
                view.progress.set_visible(false);
                return;
            }
            _ => {
                view.status.set_icon_name(Some("dialog-warning-symbolic"));
                view.status.set_title(&failed);
                view.status
                    .set_description(Some(&gettext("The mount service didn't respond.")));
                view.progress.set_visible(false);
                return;
            }
        };
        view.progress.set_visible(false);
        if sets.is_empty() {
            view.status.set_icon_name(Some("emblem-ok-symbolic"));
            match finder {
                Finder::Exact => {
                    view.status.set_title(&gettext("No duplicates"));
                    view.status.set_description(Some(&gettext(
                        "Every photo in your library is stored once.",
                    )));
                }
                Finder::Similar => {
                    view.status.set_title(&gettext("No similar photos"));
                    view.status.set_description(Some(&gettext(
                        "No two photos in your library look alike.",
                    )));
                }
            }
            if uncompared > 0 {
                view.note.set_label(&uncompared_note(uncompared));
                view.note.set_visible(true);
            }
            return;
        }
        let (review, handler) = duplicate_review(&view.ui, sets, &view.trash, finder, uncompared);
        view.clicked.replace(Some(handler));
        view.stack.add_named(&review, Some("review"));
        view.stack.set_visible_child_name("review");
        view.toolbar.set_reveal_bottom_bars(true);
        schedule_thumbs(&view.ui);
    });
}

/// Says how many photos the similar finder left out for want of a thumbnail.
fn uncompared_note(n: usize) -> String {
    ngettext_f(
        "{n} photo couldn't be compared because it has no thumbnail yet.",
        "{n} photos couldn't be compared because they have no thumbnail yet.",
        n as u64,
        &[],
    )
}

/// The review list: one section per set, each photo a thumbnail with a Keep
/// toggle. Wires `trash` to move every photo not kept to Trash, and returns
/// that handler so a new search can take it off again.
///
/// Exact copies start with only the first of each set kept. Similar photos
/// all start kept: photos that look alike can still be different shots.
fn duplicate_review(
    ui: &Rc<Ui>,
    sets: Vec<Vec<PhotoItem>>,
    trash: &gtk4::Button,
    finder: Finder,
    uncompared: usize,
) -> (gtk4::Widget, glib::SignalHandlerId) {
    let intro = gtk4::Label::builder()
        .label(match finder {
            Finder::Exact => gettext(
                "These photos are stored more than once. The copies you don't keep move to Trash, where you can restore them.",
            ),
            Finder::Similar => gettext(
                "These photos look alike. They can be copies of one photo or different shots of one moment, so all of them are kept until you untick them. The photos you don't keep move to Trash, where you can restore them.",
            ),
        })
        .wrap(true)
        .xalign(0.0)
        .build();
    intro.add_css_class("dim-label");

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&intro);
    if uncompared > 0 {
        let note = gtk4::Label::builder()
            .label(uncompared_note(uncompared))
            .wrap(true)
            .xalign(0.0)
            .build();
        note.add_css_class("dim-label");
        content.append(&note);
    }

    // Every copy's toggle, by set, so the button can count what goes.
    let toggles: Rc<RefCell<SetToggles>> = Rc::new(RefCell::new(Vec::new()));
    for set in sets {
        let Some(first) = set.first() else {
            continue;
        };
        let date = section_heading(first.capture_time, Grouping::Day);
        let title = match finder {
            // Translators: heading of one set in the duplicates dialog; {date} is the day the photo was taken.
            Finder::Exact => ngettext_f(
                "{date}, {n} copy",
                "{date}, {n} copies",
                set.len() as u64,
                &[("date", &date)],
            ),
            // Translators: heading of one set of photos that look alike; {date} is the day the first was taken.
            Finder::Similar => ngettext_f(
                "{date}, {n} photo",
                "{date}, {n} photos",
                set.len() as u64,
                &[("date", &date)],
            ),
        };
        let heading = gtk4::Label::builder().label(title).xalign(0.0).build();
        heading.add_css_class("heading");
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        let mut set_toggles = Vec::new();
        for (index, photo) in set.into_iter().enumerate() {
            let (card, keep) = copy_card(ui, &photo, index == 0 || finder == Finder::Similar);
            row.append(&card);
            set_toggles.push((photo, keep));
        }
        let row_scroll = gtk4::ScrolledWindow::builder()
            .vscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&row)
            .build();
        let section = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        section.append(&heading);
        section.append(&row_scroll);
        content.append(&section);
        toggles.borrow_mut().push(set_toggles);
    }

    let sync = {
        let toggles = toggles.clone();
        let trash = trash.clone();
        move || {
            let n = doomed(&toggles.borrow()).len();
            trash.set_label(&match finder {
                Finder::Exact => ngettext_f(
                    "Move {n} copy to Trash",
                    "Move {n} copies to Trash",
                    n as u64,
                    &[],
                ),
                Finder::Similar => ngettext_f(
                    "Move {n} photo to Trash",
                    "Move {n} photos to Trash",
                    n as u64,
                    &[],
                ),
            });
            trash.set_sensitive(n > 0);
        }
    };
    sync();
    for set in toggles.borrow().iter() {
        for (_, keep) in set {
            let toggles = toggles.clone();
            let sync = sync.clone();
            keep.connect_toggled(move |keep| {
                // A set always keeps a copy: unticking the last one would trash
                // the photo itself, which is not what removing duplicates means.
                if !keep.is_active()
                    && toggles.borrow().iter().any(|set| {
                        set.iter().any(|(_, t)| t == keep)
                            && set.iter().all(|(_, t)| !t.is_active())
                    })
                {
                    keep.set_active(true);
                    return;
                }
                sync();
            });
        }
    }

    let ui_trash = ui.clone();
    let handler = trash.connect_clicked(move |button| {
        let uids = doomed(&toggles.borrow());
        if uids.is_empty() {
            return;
        }
        // An exact copy is one file of its shot; a similar photo stands for
        // its whole shot, raw and all.
        trash_photos(&ui_trash, uids, finder == Finder::Exact);
        if let Some(dialog) = button
            .ancestor(adw::Dialog::static_type())
            .and_downcast::<adw::Dialog>()
        {
            dialog.close();
        }
    });

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    (scroll.upcast(), handler)
}

/// The uids of every copy not kept.
fn doomed(toggles: &SetToggles) -> Vec<String> {
    toggles
        .iter()
        .flatten()
        .filter(|(_, keep)| !keep.is_active())
        .map(|(photo, _)| photo.uid.clone())
        .collect()
}

/// One copy: its thumbnail, its file name, and whether to keep it.
fn copy_card(ui: &Rc<Ui>, photo: &PhotoItem, keep: bool) -> (gtk4::Box, gtk4::ToggleButton) {
    let picture = gtk4::Picture::builder()
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .build();
    let placeholder = gtk4::Image::builder()
        .icon_name("image-x-generic-symbolic")
        .pixel_size(24)
        .build();
    placeholder.add_css_class("photo-placeholder");
    let overlay = gtk4::Overlay::new();
    overlay.set_size_request(COPY_SIZE, COPY_SIZE);
    overlay.set_child(Some(&placeholder));
    overlay.add_overlay(&picture);
    overlay.set_overflow(gtk4::Overflow::Hidden);
    overlay.add_css_class("memory-cover");
    want_thumb(ui, photo, &picture);

    let name = photo.name.clone().unwrap_or_default();
    let label = gtk4::Label::builder()
        .label(&name)
        .tooltip_text(&name)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .max_width_chars(18)
        .build();
    label.add_css_class("caption");

    let toggle = gtk4::ToggleButton::builder()
        .label(gettext("Keep"))
        .active(keep)
        .build();
    toggle.add_css_class("pill");

    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    card.set_size_request(COPY_SIZE, -1);
    card.append(&overlay);
    card.append(&label);
    card.append(&toggle);
    (card, toggle)
}
