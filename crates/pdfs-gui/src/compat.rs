//! Widgets that newer libadwaita releases bring, with a stand-in built from
//! what the oldest supported release (1.5, as on Ubuntu 24.04) has.
//!
//! The app builds against 1.5 and picks at run time: when the libadwaita it
//! runs with is 1.8 or newer, the real widgets are made by class name through
//! a `gtk4::Builder`, since the 1.5 bindings have no types for them. Their
//! properties and signals are then reached by name. Call sites never care
//! which one they got.

use std::sync::OnceLock;

use gtk4::prelude::*;

/// Set to anything to use the stand-ins even on a new libadwaita, to see the
/// app as it looks on the oldest supported release.
const STAND_INS_ENV: &str = "PDFS_STAND_IN_WIDGETS";

/// Whether the libadwaita the app runs with has the 1.8 widgets.
pub(crate) fn has_adw_1_8() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        std::env::var_os(STAND_INS_ENV).is_none()
            && (adw::major_version(), adw::minor_version()) >= (1, 8)
    })
}

/// Build `xml`, a GtkBuilder interface naming classes the bindings lack. It
/// is only ever one this module wrote, so a failure is a bug here.
pub(crate) fn build_ui(xml: &str) -> gtk4::Builder {
    gtk4::Builder::from_string(&format!("<interface>{xml}</interface>"))
}

/// The object `id` from a builder [`build_ui`] made.
pub(crate) fn built<T: IsA<gtk4::glib::Object>>(builder: &gtk4::Builder, id: &str) -> T {
    builder
        .object(id)
        .unwrap_or_else(|| panic!("the compat UI has no object {id}"))
}

/// A busy spinner. It spins whenever it is shown: show and hide it with
/// `set_visible` rather than starting and stopping it.
pub(crate) type Spinner = gtk4::Widget;

/// A new [`Spinner`], shown: `AdwSpinner` on libadwaita 1.8, a spinning
/// `gtk4::Spinner` before.
pub(crate) fn spinner() -> Spinner {
    if has_adw_1_8() {
        let builder = build_ui(r#"<object class="AdwSpinner" id="spinner"/>"#);
        built(&builder, "spinner")
    } else {
        // Spinning costs nothing while hidden: an unmapped widget draws no
        // frames.
        gtk4::Spinner::builder().spinning(true).build().upcast()
    }
}
