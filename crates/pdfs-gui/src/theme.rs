//! The bundled resources and the optional Proton theme, shared by `pdfs-app`
//! and `pdfs-prompt` so both paint the same way from the same preference.

use std::cell::RefCell;

use adw::prelude::*;
use gtk4::{gio, glib};

/// Register the bundled GResources (custom icons, the stylesheets) and load
/// `stylesheet`, the front end's own sheet. The Proton theme is a separate
/// provider, applied by [`set_proton_theme`] from the saved preference.
pub(crate) fn load_resources(stylesheet: &str) {
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/pdfs.gresource"));
    let resource_data = glib::Bytes::from_static(bytes);
    if let Ok(resource) = gio::Resource::from_data(&resource_data) {
        gio::resources_register(&resource);
    } else {
        tracing::error!("failed to load gresource bundle");
    }

    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_resource(stylesheet);
    // Above the user's own gtk.css, which often imports a whole theme: its
    // `button { padding: 6px 10px; min-width: 16px }` would otherwise pad every
    // photo tile and push the grid past the window. Every rule in the sheet is
    // scoped to the app's own classes, so nothing else the user styled changes.
    // One above the Proton theme too, so the app's rules refine it.
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_USER + 2,
    );
    gtk4::IconTheme::for_display(&display).add_resource_path("/de/nils/protondrivelinux/icons");
}

thread_local! {
    /// The Proton theme provider while it is installed. Kept so turning the
    /// preference off can remove exactly what turning it on added.
    static PROTON_THEME: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

/// Whether the Proton theme is on.
pub(crate) fn proton_theme_active() -> bool {
    PROTON_THEME.with(|slot| slot.borrow().is_some())
}

/// libadwaita's own stylesheet, bundled in the library.
const ADWAITA_STYLESHEET: &str = "/org/gnome/Adwaita/styles/gtk.css";

/// The Proton palette laid over it, from our own bundle.
const PROTON_PALETTE: &str = "/de/nils/protondrivelinux/proton-theme.css";

/// Make the theme provider's `@media (prefers-color-scheme)` and
/// `(prefers-contrast)` rules match what libadwaita shows, now and whenever it
/// changes. A provider evaluates those itself, and a new one assumes light.
///
/// The properties are set by name because they only exist since GTK 4.20, and
/// the app still runs on older GTK, whose libadwaita has no such rules anyway.
fn follow_style_manager(provider: &gtk4::CssProvider) {
    fn set_enum(provider: &gtk4::CssProvider, property: &str, nick: &str) {
        let Some(pspec) = provider.find_property(property) else {
            return;
        };
        if let Some(value) = glib::EnumClass::with_type(pspec.value_type())
            .and_then(|class| class.to_value_by_nick(nick))
        {
            provider.set_property_from_value(property, &value);
        }
    }
    fn apply(provider: &gtk4::CssProvider, manager: &adw::StyleManager) {
        let scheme = if manager.is_dark() { "dark" } else { "light" };
        set_enum(provider, "prefers-color-scheme", scheme);
        let contrast = if manager.is_high_contrast() {
            "more"
        } else {
            "no-preference"
        };
        set_enum(provider, "prefers-contrast", contrast);
    }

    let manager = adw::StyleManager::default();
    apply(provider, &manager);
    // Weak, so a provider the preference removed isn't kept alive; its
    // handlers then do nothing.
    let weak = provider.downgrade();
    let weak_dark = weak.clone();
    manager.connect_dark_notify(move |manager| {
        if let Some(provider) = weak_dark.upgrade() {
            apply(&provider, manager);
        }
    });
    manager.connect_high_contrast_notify(move |manager| {
        if let Some(provider) = weak.upgrade() {
            apply(&provider, manager);
        }
    });
}

/// Paint the window in the Proton web apps' colours (`on`) or follow the
/// system theme and accent colour.
///
/// Setting libadwaita's colour variables alone isn't enough: a GTK theme the
/// user's `gtk.css` imports (Catppuccin, Fluent, ...) sits above the app and
/// hard-codes its own accent into hundreds of rules. So the theme provider,
/// one above the user's CSS, first resets every property, then re-applies
/// libadwaita's whole stylesheet and finally the Proton palette, which leaves
/// nothing of the user's theme inside the app.
pub(crate) fn set_proton_theme(on: bool) {
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    PROTON_THEME.with(|slot| {
        let mut slot = slot.borrow_mut();
        match (on, slot.as_ref()) {
            (true, None) => {
                let stylesheet = |path| {
                    gio::resources_lookup_data(path, gio::ResourceLookupFlags::NONE)
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                };
                let palette = stylesheet(PROTON_PALETTE).unwrap_or_default();
                let css = match stylesheet(ADWAITA_STYLESHEET) {
                    // Its images are named relative to the sheet, which a
                    // string loaded here doesn't have.
                    Ok(adwaita) => format!(
                        "* {{ all: unset; }}\n{}\n{palette}",
                        adwaita.replace(
                            "url(\"assets/",
                            "url(\"resource:///org/gnome/Adwaita/styles/assets/"
                        )
                    ),
                    // Still recolours everything that follows libadwaita's
                    // variables; only a custom theme's own rules show through.
                    Err(e) => {
                        tracing::warn!("libadwaita stylesheet not found, only recolouring: {e}");
                        palette
                    }
                };
                let provider = gtk4::CssProvider::new();
                follow_style_manager(&provider);
                provider.load_from_string(&css);
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_USER + 1,
                );
                *slot = Some(provider);
            }
            (false, Some(provider)) => {
                gtk4::style_context_remove_provider_for_display(&display, provider);
                *slot = None;
            }
            _ => {}
        }
    });
}
