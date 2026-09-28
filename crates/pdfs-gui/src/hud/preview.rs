//! The preview pane beside the results: a thumbnail of the selected hit, what
//! it is, where it is and whether it is on this computer, and the actions for
//! it with their shortcuts.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::{gio, glib};

use super::row::{fill_slot, thumb_slot};
use super::thumbs::Thumbs;
use crate::{Hit, dates, gettext, human_bytes, pgettext};

/// Width of the pane.
pub(crate) const PREVIEW_WIDTH: i32 = 300;

/// The thumbnail area, inside the pane's padding.
const THUMB_WIDTH: i32 = 268;
const THUMB_HEIGHT: i32 = 180;

/// How long "Path copied" stays up.
const COPIED_FEEDBACK: Duration = Duration::from_millis(1500);

/// One label/value line of the details grid.
struct Detail {
    title: gtk4::Label,
    value: gtk4::Label,
}

impl Detail {
    fn new(grid: &gtk4::Grid, row: i32, title: &str) -> Self {
        let title = gtk4::Label::builder()
            .label(title)
            .xalign(0.0)
            .valign(gtk4::Align::Start)
            .build();
        title.add_css_class("preview-key");
        let value = gtk4::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .build();
        value.add_css_class("preview-value");
        grid.attach(&title, 0, row, 1, 1);
        grid.attach(&value, 1, row, 1, 1);
        Self { title, value }
    }

    /// Show `value`, or hide the line when there is nothing to say.
    fn set(&self, value: Option<&str>) {
        self.title.set_visible(value.is_some());
        self.value.set_visible(value.is_some());
        self.value.set_label(value.unwrap_or_default());
    }
}

pub(crate) struct Preview {
    pub(crate) root: gtk4::Box,
    stack: gtk4::Stack,
    slot: gtk4::Overlay,
    name: gtk4::Label,
    kind: gtk4::Label,
    size: Detail,
    modified: Detail,
    location: Detail,
    availability: Detail,
    availability_icon: gtk4::Image,
    pub(crate) open: gtk4::Button,
    open_label: gtk4::Label,
    pub(crate) reveal: gtk4::Button,
    pub(crate) copy: gtk4::Button,
    copied: gtk4::Revealer,
    copied_timer: Rc<RefCell<Option<glib::SourceId>>>,
}

/// A flat action button: the label, and its shortcut at the far end.
fn action(icon: &str, label: &str, accelerator: &str) -> (gtk4::Button, gtk4::Label) {
    let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    content.append(&gtk4::Image::from_icon_name(icon));
    let text = gtk4::Label::builder()
        .label(label)
        .xalign(0.0)
        .hexpand(true)
        .build();
    content.append(&text);
    let shortcut = gtk4::ShortcutLabel::new(accelerator);
    shortcut.add_css_class("preview-shortcut");
    content.append(&shortcut);
    let button = gtk4::Button::builder()
        .child(&content)
        .focus_on_click(false)
        .focusable(false)
        .build();
    button.add_css_class("flat");
    button.add_css_class("preview-action");
    (button, text)
}

impl Preview {
    pub(crate) fn new() -> Self {
        let details = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

        let slot = thumb_slot(THUMB_WIDTH, THUMB_HEIGHT, 96, gtk4::ContentFit::Contain);
        slot.add_css_class("preview-thumb");
        details.append(&slot);

        let name = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .lines(3)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        name.add_css_class("preview-name");
        details.append(&name);

        let kind = gtk4::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        kind.add_css_class("preview-kind");
        details.append(&kind);

        let grid = gtk4::Grid::builder()
            .row_spacing(6)
            .column_spacing(12)
            .build();
        grid.add_css_class("preview-details");
        let size = Detail::new(&grid, 0, &pgettext("column", "Size"));
        let modified = Detail::new(&grid, 1, &pgettext("column", "Modified"));
        let location = Detail::new(&grid, 2, &gettext("Location"));
        // Translators: a heading in the launcher's preview pane, followed by
        // "Available offline", "Downloaded", "Online only" or "On this computer".
        let availability = Detail::new(&grid, 3, &gettext("Availability"));
        // The availability value is an icon and a text.
        let availability_icon = gtk4::Image::new();
        availability_icon.add_css_class("preview-badge");
        grid.remove(&availability.value);
        let availability_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        availability_box.append(&availability_icon);
        availability_box.append(&availability.value);
        grid.attach(&availability_box, 1, 3, 1, 1);
        details.append(&grid);

        let spacer = gtk4::Box::builder().vexpand(true).build();
        details.append(&spacer);

        let actions = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        actions.add_css_class("preview-actions");
        let (open, open_label) = action("document-open-symbolic", "", "Return");
        let (reveal, _) = action(
            "folder-open-symbolic",
            &gettext("Show in Folder"),
            "<Control>Return",
        );
        let (copy, _) = action("edit-copy-symbolic", &gettext("Copy Path"), "<Control>c");
        actions.append(&open);
        actions.append(&reveal);
        actions.append(&copy);
        details.append(&actions);

        let copied_label = gtk4::Label::new(Some(&gettext("Path copied")));
        copied_label.add_css_class("preview-feedback");
        let copied = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::Crossfade)
            .child(&copied_label)
            .build();
        details.append(&copied);

        let empty = gtk4::Image::from_icon_name("system-search-symbolic");
        empty.set_pixel_size(64);
        empty.add_css_class("preview-empty");

        let stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(100)
            .vexpand(true)
            .build();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&details, Some("details"));

        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .width_request(PREVIEW_WIDTH)
            .build();
        root.add_css_class("preview");
        root.append(&stack);

        Self {
            root,
            stack,
            slot,
            name,
            kind,
            size,
            modified,
            location,
            availability,
            availability_icon,
            open,
            open_label,
            reveal,
            copy,
            copied,
            copied_timer: Rc::new(RefCell::new(None)),
        }
    }

    /// Show `hit`, or the empty pane when nothing is selected.
    pub(crate) fn show(&self, hit: Option<&Hit>, thumbs: &Rc<Thumbs>, mountpoint: &Path) {
        let Some(hit) = hit else {
            self.stack.set_visible_child_name("empty");
            return;
        };
        self.stack.set_visible_child_name("details");
        fill_slot(&self.slot, hit, thumbs, mountpoint);

        self.name.set_label(hit.name());
        self.name.set_tooltip_text(Some(hit.name()));
        let content_type = if hit.is_dir() {
            glib::GString::from("inode/directory")
        } else {
            gio::functions::content_type_guess(Some(hit.name()), None).0
        };
        self.kind
            .set_label(&gio::functions::content_type_get_description(&content_type));

        let size = (!hit.is_dir() && hit.size() > 0).then(|| human_bytes(hit.size()));
        self.size.set(size.as_deref());

        let modified = (hit.modified() > 0).then(|| dates::relative(hit.modified()));
        self.modified.set(modified.as_deref());
        self.modified.value.set_tooltip_text(
            (hit.modified() > 0)
                .then(|| dates::full(hit.modified()))
                .as_deref(),
        );

        let location = hit.location();
        self.location.set(Some(&location));

        match availability(hit) {
            Some((icon, class, text)) => {
                self.availability.set(Some(&text));
                self.availability_icon.set_icon_name(Some(icon));
                for other in ["badge-pinned", "badge-cached", "badge-cloud", "badge-local"] {
                    self.availability_icon.remove_css_class(other);
                }
                self.availability_icon.add_css_class(class);
                self.availability_icon.set_visible(true);
            }
            None => {
                self.availability.set(None);
                self.availability_icon.set_visible(false);
            }
        }

        self.open_label.set_label(&if hit.is_dir() {
            gettext("Open folder")
        } else {
            pgettext("verb", "Open")
        });
    }

    /// Say "Path copied" for a moment.
    pub(crate) fn flash_copied(&self) {
        self.copied.set_reveal_child(true);
        if let Some(source) = self.copied_timer.borrow_mut().take() {
            source.remove();
        }
        let copied = self.copied.clone();
        let slot = self.copied_timer.clone();
        let timer = glib::timeout_add_local_once(COPIED_FEEDBACK, move || {
            slot.borrow_mut().take();
            copied.set_reveal_child(false);
        });
        *self.copied_timer.borrow_mut() = Some(timer);
    }
}

/// Where the hit's content is: its icon, its style class and its wording.
fn availability(hit: &Hit) -> Option<(&'static str, &'static str, String)> {
    match hit {
        Hit::Local(_) => Some((
            "drive-harddisk-symbolic",
            "badge-local",
            gettext("On this computer"),
        )),
        Hit::Drive(drive) if drive.pinned => Some((
            "pdfs-offline-symbolic",
            "badge-pinned",
            gettext("Available offline"),
        )),
        // A folder is never downloaded as a whole; only pinning says anything.
        Hit::Drive(drive) if drive.is_dir => None,
        Hit::Drive(drive) if drive.cached => Some((
            "pdfs-cached-symbolic",
            "badge-cached",
            pgettext("file state", "Downloaded"),
        )),
        Hit::Drive(_) => Some((
            "pdfs-online-only-symbolic",
            "badge-cloud",
            pgettext("file state", "Online only"),
        )),
    }
}
