use crate::*;

/// An action run by an [`ActionMenu`] item.
pub(crate) type MenuAction = Box<dyn Fn()>;

/// A `gio::Menu` whose items run closures, for menus whose contents depend on
/// what they were opened for (a row, a selection, a photo). Each item gets its
/// own action in a private `menu` group, installed on whatever hosts the
/// popover, so the result is a real [`gtk4::PopoverMenu`]: arrow keys, Enter,
/// Escape and mnemonics work, and it reads as a menu to assistive technology.
pub(crate) struct ActionMenu {
    menu: gio::Menu,
    section: gio::Menu,
    group: gio::SimpleActionGroup,
    count: usize,
}

impl ActionMenu {
    pub(crate) fn new() -> Self {
        Self {
            menu: gio::Menu::new(),
            section: gio::Menu::new(),
            group: gio::SimpleActionGroup::new(),
            count: 0,
        }
    }

    fn next_action(&mut self) -> String {
        self.count += 1;
        format!("a{}", self.count)
    }

    /// A plain item.
    pub(crate) fn item(&mut self, label: &str, run: impl Fn() + 'static) -> &mut Self {
        let name = self.next_action();
        let action = gio::SimpleAction::new(&name, None);
        action.connect_activate(move |_, _| run());
        self.group.add_action(&action);
        self.section
            .append(Some(label), Some(&format!("menu.{name}")));
        self
    }

    /// A check item showing `active`; `run` gets the state the user asked for.
    pub(crate) fn toggle(
        &mut self,
        label: &str,
        active: bool,
        run: impl Fn(bool) + 'static,
    ) -> &mut Self {
        let name = self.next_action();
        let action = gio::SimpleAction::new_stateful(&name, None, &active.to_variant());
        action.connect_activate(move |action, _| {
            let next = !action
                .state()
                .and_then(|s| s.get::<bool>())
                .unwrap_or(false);
            action.set_state(&next.to_variant());
            run(next);
        });
        self.group.add_action(&action);
        self.section
            .append(Some(label), Some(&format!("menu.{name}")));
        self
    }

    /// Close the current section; the next item starts a new one below a
    /// separator. An empty section is skipped, so callers can end a group of
    /// conditional items unconditionally.
    pub(crate) fn section(&mut self) -> &mut Self {
        self.flush(None);
        self
    }

    /// Close the current section under a heading.
    pub(crate) fn labelled_section(&mut self, heading: &str) -> &mut Self {
        self.flush(Some(heading));
        self
    }

    fn flush(&mut self, heading: Option<&str>) {
        if self.section.n_items() > 0 {
            self.menu.append_section(heading, &self.section);
            self.section = gio::Menu::new();
        }
    }

    fn finish(mut self) -> (gio::Menu, gio::SimpleActionGroup) {
        self.flush(None);
        (self.menu, self.group)
    }

    /// Pop the menu up over `anchor` at `(x, y)` in its coordinates. The
    /// popover removes itself once dismissed; it waits for an idle turn, so the
    /// item that closed it still finds its action.
    pub(crate) fn popup_at(self, anchor: &impl IsA<gtk4::Widget>, x: f64, y: f64) {
        let (menu, group) = self.finish();
        let popover = gtk4::PopoverMenu::from_model(Some(&menu));
        popover.set_has_arrow(false);
        popover.set_position(gtk4::PositionType::Bottom);
        popover.set_halign(gtk4::Align::Start);
        popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.insert_action_group("menu", Some(&group));
        popover.set_parent(anchor);
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    /// A flat ⋮ button that opens this menu.
    pub(crate) fn button(self) -> gtk4::MenuButton {
        let (menu, group) = self.finish();
        let button = gtk4::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("More")
            .valign(gtk4::Align::Center)
            .menu_model(&menu)
            .build();
        button.insert_action_group("menu", Some(&group));
        button.add_css_class("flat");
        button
    }
}

/// A flat ⋮ button whose menu lists `items` as (label, action).
pub(crate) fn more_menu_button(items: Vec<(&str, MenuAction)>) -> gtk4::MenuButton {
    let mut menu = ActionMenu::new();
    for (label, run) in items {
        menu.item(label, run);
    }
    menu.button()
}
