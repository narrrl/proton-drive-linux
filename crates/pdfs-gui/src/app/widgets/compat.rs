//! The app's side of `src/compat.rs`: widgets libadwaita 1.6–1.8 brought, with
//! a stand-in for 1.5. [`has_adw_1_8`] picks the real ones at run time.

use crate::*;

/// A segmented control: a row of options of which exactly one is on, told
/// apart by index in the order they were given. `AdwToggleGroup` on
/// libadwaita 1.8, linked toggle buttons before.
#[derive(Clone)]
pub(crate) struct ToggleGroup {
    widget: gtk4::Widget,
    options: Options,
}

#[derive(Clone)]
enum Options {
    /// The group's `AdwToggle`s, reached by property name.
    Toggles(Rc<[glib::Object]>),
    Buttons(Rc<[gtk4::ToggleButton]>),
}

impl ToggleGroup {
    /// A group of `(label, tooltip)` options, with the first one on.
    pub(crate) fn new(options: &[(&str, Option<&str>)]) -> Self {
        if has_adw_1_8() {
            let children: String = (0..options.len())
                .map(|i| format!(r#"<child><object class="AdwToggle" id="toggle{i}"/></child>"#))
                .collect();
            let builder = build_ui(&format!(
                r#"<object class="AdwToggleGroup" id="group">{children}</object>"#
            ));
            let toggles = options
                .iter()
                .enumerate()
                .map(|(i, (label, tooltip))| {
                    let toggle: glib::Object = built(&builder, &format!("toggle{i}"));
                    toggle.set_property("label", *label);
                    if let Some(tooltip) = tooltip {
                        toggle.set_property("tooltip", *tooltip);
                    }
                    toggle
                })
                .collect();
            let widget: gtk4::Widget = built(&builder, "group");
            // A toggle group starts with nothing on.
            widget.set_property("active", 0u32);
            Self {
                widget,
                options: Options::Toggles(toggles),
            }
        } else {
            let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            bar.add_css_class("linked");
            let buttons: Rc<[gtk4::ToggleButton]> = options
                .iter()
                .map(|(label, tooltip)| {
                    let button = gtk4::ToggleButton::with_label(label);
                    button.set_tooltip_text(*tooltip);
                    bar.append(&button);
                    button
                })
                .collect();
            if let Some((first, rest)) = buttons.split_first() {
                first.set_active(true);
                // Chaining each to the first is what GTK turns into mutual
                // exclusion.
                for button in rest {
                    button.set_group(Some(first));
                }
            }
            Self {
                widget: bar.upcast(),
                options: Options::Buttons(buttons),
            }
        }
    }

    /// The widget to pack.
    pub(crate) fn widget(&self) -> gtk4::Widget {
        self.widget.clone()
    }

    /// Give the options pill-shaped ends.
    pub(crate) fn set_round(&self) {
        match &self.options {
            Options::Toggles(_) => self.widget.add_css_class("round"),
            Options::Buttons(buttons) => {
                for button in buttons.iter() {
                    button.add_css_class("pill");
                }
            }
        }
    }

    /// The option that is on.
    pub(crate) fn active(&self) -> u32 {
        match &self.options {
            Options::Toggles(_) => self.widget.property("active"),
            Options::Buttons(buttons) => {
                buttons.iter().position(|b| b.is_active()).unwrap_or(0) as u32
            }
        }
    }

    /// Turn option `index` on, which turns the one that was on off.
    pub(crate) fn set_active(&self, index: u32) {
        match &self.options {
            Options::Toggles(_) => self.widget.set_property("active", index),
            Options::Buttons(buttons) => {
                if let Some(button) = buttons.get(index as usize) {
                    button.set_active(true);
                }
            }
        }
    }

    pub(crate) fn set_label(&self, index: u32, label: &str) {
        match &self.options {
            Options::Toggles(toggles) => {
                if let Some(toggle) = toggles.get(index as usize) {
                    toggle.set_property("label", label);
                }
            }
            Options::Buttons(buttons) => {
                if let Some(button) = buttons.get(index as usize) {
                    button.set_label(label);
                }
            }
        }
    }

    /// Let option `index` be picked, or not.
    pub(crate) fn set_enabled(&self, index: u32, enabled: bool) {
        match &self.options {
            Options::Toggles(toggles) => {
                if let Some(toggle) = toggles.get(index as usize) {
                    toggle.set_property("enabled", enabled);
                }
            }
            Options::Buttons(buttons) => {
                if let Some(button) = buttons.get(index as usize) {
                    button.set_sensitive(enabled);
                }
            }
        }
    }

    /// Call `f` with the option that has just come on, whether the user or
    /// [`Self::set_active`] turned it on.
    pub(crate) fn connect_changed(&self, f: impl Fn(u32) + 'static) {
        match &self.options {
            Options::Toggles(_) => {
                self.widget
                    .connect_notify_local(Some("active"), move |group, _| {
                        f(group.property("active"))
                    });
            }
            Options::Buttons(buttons) => {
                let f = Rc::new(f);
                for (index, button) in buttons.iter().enumerate() {
                    let f = f.clone();
                    // The group also fires `toggled` for the button going off;
                    // only the one coming on counts.
                    button.connect_toggled(move |button| {
                        if button.is_active() {
                            f(index as u32);
                        }
                    });
                }
            }
        }
    }
}

/// A list row that is a button: a centred title and an icon after it.
/// `AdwButtonRow` on libadwaita 1.8, an activatable `adw::ActionRow` before.
pub(crate) fn button_row(
    title: &str,
    end_icon: &str,
    on_activated: impl Fn() + 'static,
) -> gtk4::ListBoxRow {
    if has_adw_1_8() {
        let builder = build_ui(r#"<object class="AdwButtonRow" id="row"/>"#);
        let row: adw::PreferencesRow = built(&builder, "row");
        row.set_title(title);
        row.set_property("end-icon-name", end_icon);
        row.connect_local("activated", false, move |_| {
            on_activated();
            None
        });
        row.upcast()
    } else {
        let row = adw::ActionRow::builder()
            .title(title)
            .activatable(true)
            .build();
        row.add_suffix(&gtk4::Image::from_icon_name(end_icon));
        row.connect_activated(move |_| on_activated());
        row.upcast()
    }
}

/// Show the keyboard shortcuts window over `window`. See [`shortcuts_dialog`].
pub(crate) fn present_shortcuts(
    window: &impl IsA<gtk4::Widget>,
    groups: &[(String, Vec<(&str, String)>)],
) {
    shortcuts_dialog(groups).present(Some(window));
}

/// The keyboard shortcuts window: `groups` of `(accelerator, what it does)`
/// under a heading each, all already translated. `AdwShortcutsDialog` on
/// libadwaita 1.8, a preferences page of rows before.
fn shortcuts_dialog(groups: &[(String, Vec<(&str, String)>)]) -> adw::Dialog {
    if has_adw_1_8() {
        let sections: String = groups
            .iter()
            .enumerate()
            .map(|(s, (_, keys))| {
                let items: String = (0..keys.len())
                    .map(|k| {
                        format!(r#"<child><object class="AdwShortcutsItem" id="item{s}_{k}"/></child>"#)
                    })
                    .collect();
                format!(
                    r#"<child><object class="AdwShortcutsSection" id="section{s}">{items}</object></child>"#
                )
            })
            .collect();
        let builder = build_ui(&format!(
            r#"<object class="AdwShortcutsDialog" id="dialog">{sections}</object>"#
        ));
        for (s, (title, keys)) in groups.iter().enumerate() {
            let section: glib::Object = built(&builder, &format!("section{s}"));
            section.set_property("title", title.as_str());
            for (k, (accel, action)) in keys.iter().enumerate() {
                let item: glib::Object = built(&builder, &format!("item{s}_{k}"));
                item.set_property("title", action.as_str());
                item.set_property("accelerator", *accel);
            }
        }
        return built(&builder, "dialog");
    }

    let page = adw::PreferencesPage::new();
    for (title, keys) in groups {
        let group = adw::PreferencesGroup::builder().title(title).build();
        for (accel, action) in keys {
            let row = adw::ActionRow::builder().title(action).build();
            row.add_suffix(
                &gtk4::ShortcutLabel::builder()
                    .accelerator(*accel)
                    .valign(gtk4::Align::Center)
                    .build(),
            );
            group.add(&row);
        }
        page.add(&group);
    }

    adw::Dialog::builder()
        .title(gettext("Keyboard Shortcuts"))
        .content_width(460)
        .content_height(620)
        .child(&{
            let toolbar = adw::ToolbarView::new();
            toolbar.add_top_bar(&adw::HeaderBar::new());
            toolbar.set_content(Some(&page));
            toolbar
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The newer widgets are made by class name and set up by property and
    /// signal name, so a typo in any of those only shows when they are built.
    /// Needs a display and libadwaita 1.8; passes trivially without either.
    #[test]
    fn newer_widgets_build_by_class_name() {
        if adw::init().is_err() || !has_adw_1_8() {
            return;
        }

        let group = ToggleGroup::new(&[("One", None), ("Two", Some("The second"))]);
        group.set_round();
        group.set_label(1, "Deux");
        group.set_enabled(1, true);
        assert_eq!(group.widget().type_().name(), "AdwToggleGroup");
        assert_eq!(group.active(), 0);
        let changed = Rc::new(Cell::new(None));
        group.connect_changed({
            let changed = changed.clone();
            move |index| changed.set(Some(index))
        });
        group.set_active(1);
        assert_eq!((group.active(), changed.get()), (1, Some(1)));

        let row = button_row("Row", "go-down-symbolic", || {});
        assert_eq!(row.type_().name(), "AdwButtonRow");
        assert_eq!(spinner().type_().name(), "AdwSpinner");

        let dialog = shortcuts_dialog(&[(
            "General".to_owned(),
            vec![("<Control>q", "Quit".to_owned())],
        )]);
        assert_eq!(dialog.type_().name(), "AdwShortcutsDialog");
    }
}
