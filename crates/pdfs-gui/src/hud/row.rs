//! Result rows: a thumbnail or icon, the name with its matched characters in
//! bold, the location, and on the right the sync state, size and age.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{gdk, gio};

use super::highlight;
use super::thumbs::Thumbs;
use crate::theme::proton_theme_active;
use crate::{Hit, format_age, human_bytes};

/// Edge of a row's thumbnail.
const ROW_THUMB: i32 = 40;

/// A thumbnail surface: an icon, with a picture laid over it that shows once
/// the thumbnail has loaded. [`Thumbs`] paints the picture.
pub(crate) fn thumb_slot(
    width: i32,
    height: i32,
    icon_size: i32,
    fit: gtk4::ContentFit,
) -> gtk4::Overlay {
    let fallback = gtk4::Image::builder().pixel_size(icon_size).build();
    let picture = gtk4::Picture::builder()
        .width_request(width)
        .height_request(height)
        .content_fit(fit)
        .can_shrink(true)
        .visible(false)
        .build();
    picture.set_overflow(gtk4::Overflow::Hidden);
    picture.add_css_class("thumb");

    let overlay = gtk4::Overlay::new();
    overlay.set_size_request(width, height);
    overlay.set_halign(gtk4::Align::Center);
    overlay.set_valign(gtk4::Align::Center);
    overlay.set_child(Some(&fallback));
    overlay.add_overlay(&picture);
    overlay
}

/// Show `hit` in a slot from [`thumb_slot`]: its icon at once, and its
/// thumbnail when there is one.
pub(crate) fn fill_slot(slot: &gtk4::Overlay, hit: &Hit, thumbs: &Rc<Thumbs>, mountpoint: &Path) {
    let Some(fallback) = slot.child().and_downcast::<gtk4::Image>() else {
        return;
    };
    let Some(picture) = fallback.next_sibling().and_downcast::<gtk4::Picture>() else {
        return;
    };
    fallback.set_from_gicon(&icon_for(hit.name(), hit.is_dir()));
    fallback.set_visible(true);
    picture.set_visible(false);
    picture.set_paintable(gdk::Paintable::NONE);
    thumbs.want(hit, mountpoint, &picture);
}

/// The desktop's own full-colour icon for a file name, so results look like
/// the user's file manager. Folders are Proton's purple ones while the Proton
/// theme is on, as in the app's browser.
pub(crate) fn icon_for(name: &str, is_dir: bool) -> gio::Icon {
    if is_dir {
        let folder = if proton_theme_active() {
            "pdfs-proton-folder"
        } else {
            "folder"
        };
        return gio::ThemedIcon::new(folder).upcast();
    }
    let (content_type, _uncertain) = gio::functions::content_type_guess(Some(name), None);
    gio::functions::content_type_get_icon(&content_type)
}

/// The sync badge of a Drive file, as in the app's browser: pinned, cached,
/// or online only. Folders and local files have none.
pub(crate) fn badge_for(hit: &Hit) -> Option<(&'static str, &'static str)> {
    let Hit::Drive(drive) = hit else {
        return None;
    };
    if drive.is_dir {
        return None;
    }
    Some(if drive.pinned {
        ("pdfs-offline-symbolic", "badge-pinned")
    } else if drive.cached {
        ("pdfs-cached-symbolic", "badge-cached")
    } else {
        ("pdfs-online-only-symbolic", "badge-cloud")
    })
}

pub(crate) fn build_row(
    hit: &Hit,
    query: &str,
    thumbs: &Rc<Thumbs>,
    mountpoint: &Path,
) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.add_css_class("result-row");
    // Focus stays in the search entry; the list only shows the cursor.
    row.set_focusable(false);

    let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);

    let slot = thumb_slot(ROW_THUMB, ROW_THUMB, 32, gtk4::ContentFit::Cover);
    fill_slot(&slot, hit, thumbs, mountpoint);
    content.append(&slot);

    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.set_valign(gtk4::Align::Center);

    let name = gtk4::Label::builder()
        .label(highlight::markup(hit.name(), query))
        .use_markup(true)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .build();
    name.add_css_class("result-name");
    text.append(&name);

    let location = gtk4::Label::builder()
        .label(hit.location())
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::Start)
        .build();
    location.add_css_class("result-location");
    text.append(&location);
    content.append(&text);

    let meta = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    meta.set_valign(gtk4::Align::Center);
    if let Some((icon, class)) = badge_for(hit) {
        let badge = gtk4::Image::from_icon_name(icon);
        badge.add_css_class("result-badge");
        badge.add_css_class(class);
        meta.append(&badge);
    }
    if !hit.is_dir() && hit.size() > 0 {
        let size = gtk4::Label::new(Some(&human_bytes(hit.size())));
        size.add_css_class("result-meta");
        meta.append(&size);
    }
    if hit.modified() > 0 {
        let modified = gtk4::Label::new(Some(&format_age(hit.modified())));
        modified.add_css_class("result-meta");
        meta.append(&modified);
    }
    content.append(&meta);

    row.set_child(Some(&content));
    row
}

/// Let a row be dragged into a file manager or an application as a file.
/// `dropped` runs once a drop was accepted; a cancelled drag changes nothing.
pub(crate) fn attach_drag(row: &gtk4::ListBoxRow, path: PathBuf, dropped: impl Fn() + 'static) {
    let source = gtk4::DragSource::new();
    source.set_actions(gdk::DragAction::COPY);
    let files = gdk::FileList::from_array(&[gio::File::for_path(&path)]);
    source.set_content(Some(&gdk::ContentProvider::for_value(&files.to_value())));

    let cancelled = Rc::new(Cell::new(false));
    let begin = cancelled.clone();
    source.connect_drag_begin(move |source, _| {
        begin.set(false);
        if let Some(widget) = source.widget() {
            source.set_icon(Some(&gtk4::WidgetPaintable::new(Some(&widget))), 0, 0);
        }
    });
    let cancel = cancelled.clone();
    source.connect_drag_cancel(move |_, _, _| {
        cancel.set(true);
        false
    });
    source.connect_drag_end(move |_, _, _| {
        if !cancelled.get() {
            dropped();
        }
    });
    row.add_controller(source);
}
