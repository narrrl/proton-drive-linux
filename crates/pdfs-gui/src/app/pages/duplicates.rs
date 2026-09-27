use crate::*;

/// Side of a copy's thumbnail in the duplicates dialog, in px.
const COPY_SIZE: i32 = 150;

/// Each set's copies with their Keep toggles.
type SetToggles = Vec<Vec<(PhotoItem, gtk4::ToggleButton)>>;

/// Open the duplicate finder: every set of photos stored more than once, side
/// by side, with the copy worth keeping ticked. The copies left unticked go to
/// Trash — one file each, never the rest of their shot.
pub(crate) fn show_duplicates(ui: &Rc<Ui>) {
    let status = adw::StatusPage::builder()
        .icon_name("edit-copy-symbolic")
        .title(gettext("Looking for duplicates…"))
        .vexpand(true)
        .build();
    status.add_css_class("compact");

    let stack = gtk4::Stack::new();
    stack.add_named(&status, Some("status"));

    let trash = gtk4::Button::builder()
        .label(gettext("Move to Trash"))
        .halign(gtk4::Align::End)
        .sensitive(false)
        .build();
    trash.add_css_class("destructive-action");
    trash.add_css_class("pill");
    let bottom = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bottom.add_css_class("toolbar");
    bottom.set_halign(gtk4::Align::End);
    bottom.append(&trash);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
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

    let rx = spawn_request(ui.dirs.control_socket(), Request::PhotoDuplicates);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let sets = match rx.recv().await {
            Ok(Ok(Response::PhotoDuplicates { sets })) => sets,
            Ok(Ok(Response::Error { message, .. })) => {
                status.set_icon_name(Some("dialog-warning-symbolic"));
                status.set_title(&gettext("Couldn't look for duplicates"));
                status.set_description(Some(&message));
                return;
            }
            _ => {
                status.set_icon_name(Some("dialog-warning-symbolic"));
                status.set_title(&gettext("Couldn't look for duplicates"));
                status.set_description(Some(&gettext("The mount service didn't respond.")));
                return;
            }
        };
        if sets.is_empty() {
            status.set_icon_name(Some("emblem-ok-symbolic"));
            status.set_title(&gettext("No duplicates"));
            status.set_description(Some(&gettext(
                "Every photo in your library is stored once.",
            )));
            return;
        }
        let review = duplicate_review(&ui, sets, &trash);
        stack.add_named(&review, Some("review"));
        stack.set_visible_child_name("review");
        toolbar.set_reveal_bottom_bars(true);
        schedule_thumbs(&ui);
    });
}

/// The review list: one section per set, each copy a thumbnail with a Keep
/// toggle. Wires `trash` to move every copy not kept to Trash.
fn duplicate_review(ui: &Rc<Ui>, sets: Vec<Vec<PhotoItem>>, trash: &gtk4::Button) -> gtk4::Widget {
    let intro = gtk4::Label::builder()
        .label(gettext(
            "These photos are stored more than once. The copies you don't keep move to Trash, where you can restore them.",
        ))
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

    // Every copy's toggle, by set, so the button can count what goes.
    let toggles: Rc<RefCell<SetToggles>> = Rc::new(RefCell::new(Vec::new()));
    for set in sets {
        let Some(first) = set.first() else {
            continue;
        };
        let date = section_heading(first.capture_time, Grouping::Day);
        // Translators: heading of one set in the duplicates dialog; {date} is the day the photo was taken.
        let title = ngettext_f(
            "{date}, {n} copy",
            "{date}, {n} copies",
            set.len() as u64,
            &[("date", &date)],
        );
        let heading = gtk4::Label::builder().label(title).xalign(0.0).build();
        heading.add_css_class("heading");
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        let mut set_toggles = Vec::new();
        for (index, photo) in set.into_iter().enumerate() {
            let (card, keep) = copy_card(ui, &photo, index == 0);
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
            trash.set_label(&ngettext_f(
                "Move {n} copy to Trash",
                "Move {n} copies to Trash",
                n as u64,
                &[],
            ));
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
    trash.connect_clicked(move |button| {
        let uids = doomed(&toggles.borrow());
        if uids.is_empty() {
            return;
        }
        trash_photos(&ui_trash, uids, true);
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
    scroll.upcast()
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
