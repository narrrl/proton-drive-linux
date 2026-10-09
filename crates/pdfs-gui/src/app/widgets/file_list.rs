//! The file list every file page shows: an icon grid and a column list over one
//! model and one selection, with thumbnails, context menus, rubberband selection
//! and click-to-deselect. My files, Trash, Shared with me and Shared by me build
//! one each and differ only in their columns, menus and what activating an entry
//! does (see [`FileListBehavior`]).

use crate::*;

/// Where a page's entries go when the user acts on them. Plain function pointers:
/// each page has one of these, fixed for the life of the window.
#[derive(Clone, Copy)]
pub(crate) struct FileListBehavior {
    /// Double-click or Enter on an entry.
    pub(crate) activate: fn(&Rc<Ui>, &DirEntry),
    /// The right-click menu for one entry.
    pub(crate) entry_menu: fn(&Rc<Ui>, &DirEntry) -> ActionMenu,
    /// The right-click menu for a multi-selection the clicked entry is part of.
    pub(crate) bulk_menu: fn(&Rc<Ui>, Vec<DirEntry>) -> ActionMenu,
    /// The right-click menu on empty space.
    pub(crate) background_menu: fn(&Rc<Ui>) -> ActionMenu,
    /// Paint the offline/cached/online-only badge. Only entries in the mounted
    /// tree have a local state worth showing.
    pub(crate) badges: bool,
    /// Entries can be dragged onto folders to move them.
    pub(crate) drag_and_drop: bool,
}

/// A realised grid cell's thumbnail and name label.
type GridTile = (glib::WeakRef<gtk4::Overlay>, glib::WeakRef<gtk4::Label>);

/// One page's grid and list. Cheap to clone: every field is a GObject or an
/// `Rc`, so a clone is the same list.
#[derive(Clone)]
pub(crate) struct FileList {
    /// The entries, as [`BoxedAnyObject`]-wrapped [`DirEntry`]s.
    pub(crate) model: gio::ListStore,
    /// One selection for both views: switching layout keeps what is selected.
    pub(crate) selection: gtk4::MultiSelection,
    pub(crate) grid: gtk4::GridView,
    pub(crate) column_view: gtk4::ColumnView,
    /// The two views, named "grid" and "list".
    pub(crate) views: gtk4::Stack,
    thumbnail_size: Rc<Cell<i32>>,
    /// Weak references to realised grid cells. A new thumbnail size resizes only
    /// these visible, recycled surfaces instead of invalidating the whole model.
    tiles: Rc<RefCell<Vec<GridTile>>>,
    behavior: Rc<Cell<Option<FileListBehavior>>>,
}

impl FileList {
    /// Assemble the empty views. The factories need the [`Ui`] handle, so they
    /// are installed later by [`FileList::wire`].
    pub(crate) fn new() -> Self {
        let model = gio::ListStore::new::<BoxedAnyObject>();
        // Multi-select: acting on a batch is the common case, and doing it one
        // confirmation dialog at a time is not a workflow. Rubberband drags a
        // selection box from empty space, as a file manager does.
        let selection = gtk4::MultiSelection::new(Some(model.clone()));
        let grid = gtk4::GridView::builder()
            .model(&selection)
            .min_columns(2)
            .max_columns(32)
            .enable_rubberband(true)
            .build();
        grid.add_css_class("file-grid");
        let grid_scroll = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .child(&grid)
            .build();

        let column_view = gtk4::ColumnView::builder()
            .model(&selection)
            .enable_rubberband(true)
            .build();
        column_view.add_css_class("data-table");
        let column_scroll = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .child(&column_view)
            .build();

        let views = gtk4::Stack::new();
        views.set_vexpand(true);
        views.add_named(&grid_scroll, Some("grid"));
        views.add_named(&column_scroll, Some("list"));

        FileList {
            model,
            selection,
            grid,
            column_view,
            views,
            thumbnail_size: Rc::new(Cell::new(GRID_THUMB_DEFAULT)),
            tiles: Rc::new(RefCell::new(Vec::new())),
            behavior: Rc::new(Cell::new(None)),
        }
    }

    /// Install the grid tiles, the Name column, activation, the context menus
    /// and click-to-deselect. The page appends its other columns afterwards.
    pub(crate) fn wire(&self, ui: &Rc<Ui>, behavior: FileListBehavior) {
        self.behavior.set(Some(behavior));
        for view in [
            self.grid.clone().upcast::<gtk4::Widget>(),
            self.column_view.clone().upcast(),
        ] {
            self.attach_background_menu(ui, &view, behavior);
            self.attach_background_deselect(&view);
        }

        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup({
            let (ui, list) = (ui.clone(), self.clone());
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                let size = list.thumbnail_size.get();
                let thumbnail = file_thumbnail_widget(size, grid_fallback_size(size));
                // Keep the sync-state badge inside the thumbnail surface.
                let badge = gtk4::Image::builder()
                    .pixel_size(18)
                    .halign(gtk4::Align::End)
                    .valign(gtk4::Align::Start)
                    .margin_top(2)
                    .margin_end(2)
                    .visible(false)
                    .build();
                badge.add_css_class("file-badge");
                thumbnail.add_overlay(&badge);
                // `WordChar` rather than the default `Word`: a name with no
                // spaces offers no word-break opportunity, so word wrapping
                // cannot break it at all and the label asks for its full natural
                // width instead — one tile stretches to the width of the window
                // and the grid collapses to a single column. Allowing a mid-word
                // break is what keeps the two-line-then-ellipsis budget below
                // enforceable for *every* name rather than only the ones that
                // happen to have spaces. Pango marks such a break with a hyphen
                // that is not in the name ("SteamSetup.e-xe"); a file name is
                // not prose, so break it bare, as Nautilus does.
                let no_hyphens = gtk4::pango::AttrList::new();
                no_hyphens.insert(gtk4::pango::AttrInt::new_insert_hyphens(false));
                let label = gtk4::Label::builder()
                    .attributes(&no_hyphens)
                    .ellipsize(gtk4::pango::EllipsizeMode::End)
                    .justify(gtk4::Justification::Center)
                    .max_width_chars(13)
                    .width_chars(13)
                    .wrap(true)
                    .wrap_mode(gtk4::pango::WrapMode::WordChar)
                    .lines(2)
                    .build();
                list.tiles
                    .borrow_mut()
                    .push((thumbnail.downgrade(), label.downgrade()));
                let tile = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
                tile.add_css_class("file-tile");
                tile.append(&thumbnail);
                tile.append(&label);
                list.attach_context_menu(&ui, item, &tile, behavior);
                if behavior.drag_and_drop {
                    attach_drag(&ui, item, &tile);
                    attach_drop(&ui, item, &tile);
                }
                item.set_child(Some(&tile));
            }
        });
        factory.connect_bind({
            let (ui, list) = (ui.clone(), self.clone());
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                let tile = item.child().and_downcast::<gtk4::Box>().unwrap();
                let thumbnail = tile.first_child().and_downcast::<gtk4::Overlay>().unwrap();
                let badge = thumbnail
                    .last_child()
                    .and_downcast::<gtk4::Image>()
                    .unwrap();
                let label = thumbnail
                    .next_sibling()
                    .and_downcast::<gtk4::Label>()
                    .unwrap();
                let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
                let entry = obj.borrow::<DirEntry>();
                resize_grid_tile(&thumbnail, &label, list.thumbnail_size.get());
                bind_file_thumbnail(&ui, &thumbnail, &entry, false);
                label.set_label(&entry.name);
                label.set_tooltip_text(Some(&entry.name));
                if behavior.badges {
                    apply_badge(&badge, &entry);
                }
            }
        });
        self.grid.set_factory(Some(&factory));

        let ui_grid = ui.clone();
        self.grid.connect_activate(move |grid, pos| {
            if let Some(entry) = entry_at(grid.model().as_ref(), pos) {
                (behavior.activate)(&ui_grid, &entry);
            }
        });

        self.column_view
            .append_column(&self.name_column(ui, behavior));
        let ui_col = ui.clone();
        self.column_view.connect_activate(move |view, pos| {
            if let Some(entry) = entry_at(view.model().as_ref(), pos) {
                (behavior.activate)(&ui_col, &entry);
            }
        });
    }

    /// Every highlighted entry, in model order.
    ///
    /// Walks the model rather than the selection bitset: a listing is at most a
    /// few thousand rows, and asking each position whether it is selected keeps
    /// this free of bitset-iterator lifetimes for no measurable cost.
    pub(crate) fn selected(&self) -> Vec<DirEntry> {
        (0..self.selection.n_items())
            .filter(|i| self.selection.is_selected(*i))
            .filter_map(|i| entry_at(Some(&self.selection), i))
            .collect()
    }

    /// Show the grid, or the list.
    pub(crate) fn show_list(&self, list: bool) {
        self.views
            .set_visible_child_name(if list { "list" } else { "grid" });
    }

    /// Whether the list, rather than the grid, is on screen.
    pub(crate) fn shows_list(&self) -> bool {
        self.views.visible_child_name().as_deref() == Some("list")
    }

    /// Resize the grid's thumbnails. Only realised cells are touched; the rest
    /// pick the size up when they are bound.
    pub(crate) fn set_thumbnail_size(&self, size: i32) {
        if self.thumbnail_size.replace(size) == size {
            return;
        }
        self.tiles
            .borrow_mut()
            .retain(|(thumbnail_ref, label_ref)| {
                let (Some(thumbnail), Some(label)) = (thumbnail_ref.upgrade(), label_ref.upgrade())
                else {
                    return false;
                };
                resize_grid_tile(&thumbnail, &label, size);
                true
            });
        self.grid.queue_resize();
    }

    /// The Menu key or Shift+F10: the menu for the selection, or the background
    /// menu when nothing is selected, opened at the focused item.
    pub(crate) fn popup_keyboard_menu(&self, ui: &Rc<Ui>) {
        let Some(behavior) = self.behavior.get() else {
            return;
        };
        let view = self
            .views
            .visible_child()
            .unwrap_or_else(|| self.views.clone().upcast());
        // The focused cell is where the eye is; without one, the middle of the view.
        let (x, y) = view
            .root()
            .and_then(|root| root.focus())
            .filter(|focus| focus.is_ancestor(&view))
            .and_then(|focus| focus.compute_bounds(&view))
            .map(|b| {
                (
                    (b.x() + b.width() / 2.0) as f64,
                    (b.y() + b.height() / 2.0) as f64,
                )
            })
            .unwrap_or((view.width() as f64 / 2.0, view.height() as f64 / 2.0));
        let selected = self.selected();
        let menu = match selected.len() {
            0 => (behavior.background_menu)(ui),
            1 => (behavior.entry_menu)(ui, &selected[0]),
            _ => (behavior.bulk_menu)(ui, selected),
        };
        menu.popup_at(&view, x, y);
    }

    /// The Name column: a small thumbnail with its local-state badge overlaid,
    /// followed by the name and the same right-click menu the grid tiles carry.
    fn name_column(&self, ui: &Rc<Ui>, behavior: FileListBehavior) -> gtk4::ColumnViewColumn {
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup({
            let (ui, list) = (ui.clone(), self.clone());
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                let thumbnail = file_thumbnail_widget(28, 16);
                let badge = gtk4::Image::builder()
                    .pixel_size(14)
                    .halign(gtk4::Align::End)
                    .valign(gtk4::Align::Start)
                    .visible(false)
                    .build();
                badge.add_css_class("file-badge");
                thumbnail.add_overlay(&badge);
                // Ellipsized so the Name column can be *narrower* than its
                // longest name. Without it the label's minimum width is the whole
                // string, the column inherits that minimum, and one long name
                // pushes the other columns off the right edge of the window for
                // every row.
                let label = gtk4::Label::builder()
                    .halign(gtk4::Align::Start)
                    .ellipsize(gtk4::pango::EllipsizeMode::End)
                    .build();
                let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
                cell.append(&thumbnail);
                cell.append(&label);
                list.attach_context_menu(&ui, item, &cell, behavior);
                if behavior.drag_and_drop {
                    attach_drag(&ui, item, &cell);
                    attach_drop(&ui, item, &cell);
                }
                item.set_child(Some(&cell));
            }
        });
        factory.connect_bind({
            let ui = ui.clone();
            move |_, item| {
                let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
                let cell = item.child().and_downcast::<gtk4::Box>().unwrap();
                let thumbnail = cell.first_child().and_downcast::<gtk4::Overlay>().unwrap();
                let badge = thumbnail
                    .last_child()
                    .and_downcast::<gtk4::Image>()
                    .unwrap();
                let label = thumbnail
                    .next_sibling()
                    .and_downcast::<gtk4::Label>()
                    .unwrap();
                let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
                let entry = obj.borrow::<DirEntry>();
                bind_file_thumbnail(&ui, &thumbnail, &entry, true);
                label.set_label(&entry.name);
                label.set_tooltip_text(Some(&entry.name));
                if behavior.badges {
                    apply_badge(&badge, &entry);
                }
            }
        });
        let column = gtk4::ColumnViewColumn::new(Some(&pgettext("column", "Name")), Some(factory));
        column.set_expand(true);
        column
    }

    /// Pop the menu for whatever entry `item` is bound to when a cell is
    /// right-clicked. Capturing the [`gtk4::ListItem`] rather than a snapshot of
    /// the entry keeps the menu right as the view recycles cells while
    /// scrolling. The click is claimed, so the background menu does not open on
    /// top of it.
    fn attach_context_menu(
        &self,
        ui: &Rc<Ui>,
        item: &gtk4::ListItem,
        anchor: &gtk4::Box,
        behavior: FileListBehavior,
    ) {
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
        let (ui, list, item, target) = (ui.clone(), self.clone(), item.clone(), anchor.clone());
        gesture.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            if let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() {
                let entry = obj.borrow::<DirEntry>().clone();
                // Right-clicking a row that is part of a multi-selection acts on
                // the batch; right-clicking outside one acts on the row.
                let selected = list.selected();
                if selected.len() > 1 && selected.iter().any(|e| e.uid == entry.uid) {
                    (behavior.bulk_menu)(&ui, selected).popup_at(&target, x, y);
                } else {
                    (behavior.entry_menu)(&ui, &entry).popup_at(&target, x, y);
                }
            }
        });
        anchor.add_controller(gesture);
    }

    /// Right-clicking the empty space of a view offers the page's own actions.
    fn attach_background_menu(&self, ui: &Rc<Ui>, view: &gtk4::Widget, behavior: FileListBehavior) {
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
        let (ui, target) = (ui.clone(), view.clone());
        gesture.connect_pressed(move |_, _, x, y| {
            (behavior.background_menu)(&ui).popup_at(&target, x, y);
        });
        view.add_controller(gesture);
    }

    /// Clicking the empty space of a view drops the selection, as in a file
    /// manager. GTK's list views leave it alone, so a stray highlight otherwise
    /// sticks around until another item is clicked.
    fn attach_background_deselect(&self, view: &gtk4::Widget) {
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gtk4::gdk::BUTTON_PRIMARY);
        // Capture sees the press before an item's own gesture claims it; this
        // only looks, so the item still gets its click.
        gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let (selection, target) = (self.selection.clone(), view.clone());
        gesture.connect_pressed(move |gesture, _, x, y| {
            let modifiers = gesture.current_event_state();
            if modifiers.intersects(
                gtk4::gdk::ModifierType::CONTROL_MASK | gtk4::gdk::ModifierType::SHIFT_MASK,
            ) {
                return;
            }
            if !on_item(&target, x, y) {
                selection.unselect_all();
            }
        });
        view.add_controller(gesture);
    }
}

/// A header button that switches `list` between grid and list. It offers the
/// other layout, the way a toggle reads.
pub(crate) fn layout_button(list: &FileList) -> gtk4::Button {
    let button = gtk4::Button::builder().valign(gtk4::Align::Center).build();
    let sync = |button: &gtk4::Button, list: bool| {
        button.set_icon_name(if list {
            "view-grid-symbolic"
        } else {
            "view-list-symbolic"
        });
        button.set_tooltip_text(Some(&if list {
            gettext("Show as grid (Ctrl+1)")
        } else {
            gettext("Show as list (Ctrl+2)")
        }));
    };
    sync(&button, list.shows_list());
    let weak = button.downgrade();
    list.views.connect_visible_child_name_notify(move |views| {
        if let Some(button) = weak.upgrade() {
            sync(
                &button,
                views.visible_child_name().as_deref() == Some("list"),
            );
        }
    });
    let views = list.views.clone();
    button.connect_clicked(move |_| {
        let to_list = views.visible_child_name().as_deref() != Some("list");
        views.set_visible_child_name(if to_list { "list" } else { "grid" });
    });
    button
}

/// Build a trailing text column whose cell text is derived from each [`DirEntry`]
/// by `render`.
pub(crate) fn text_column(
    title: &str,
    render: impl Fn(&DirEntry) -> String + 'static,
) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = gtk4::Label::builder()
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        label.add_css_class("dim-label");
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = item.child().and_downcast::<gtk4::Label>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        let text = render(&entry);
        label.set_tooltip_text(Some(&text));
        label.set_label(&text);
    });
    gtk4::ColumnViewColumn::new(Some(title), Some(factory))
}

fn resize_grid_tile(thumbnail: &gtk4::Overlay, label: &gtk4::Label, size: i32) {
    resize_file_thumbnail(thumbnail, size, grid_fallback_size(size));
    let name_width = (size / 6 + 1).clamp(8, 24);
    label.set_width_chars(name_width);
    label.set_max_width_chars(name_width);
}

fn grid_fallback_size(thumbnail_size: i32) -> i32 {
    (thumbnail_size * 8 / 9).clamp(24, thumbnail_size)
}

/// Whether `(x, y)` in `view` lands on an item: a grid tile ("child"), a list
/// row ("row"), or a column header ("header"), which has clicks of its own.
fn on_item(view: &gtk4::Widget, x: f64, y: f64) -> bool {
    let mut widget = view.pick(x, y, gtk4::PickFlags::DEFAULT);
    while let Some(w) = widget {
        if &w == view {
            return false;
        }
        if matches!(w.css_name().as_str(), "child" | "row" | "header") {
            return true;
        }
        widget = w.parent();
    }
    false
}

/// Every page's file list.
pub(crate) fn file_lists(ui: &Ui) -> [&FileList; 4] {
    [
        &ui.browser.files,
        &ui.trash.files,
        &ui.shared.files,
        &ui.shared_by_me.files,
    ]
}

/// The file list of the page on screen, if it has one.
pub(crate) fn visible_file_list(ui: &Ui) -> Option<&FileList> {
    match ui.stack.visible_child_name()?.as_str() {
        "browser" => Some(&ui.browser.files),
        "trash" => Some(&ui.trash.files),
        "sharedbyme" => Some(&ui.shared_by_me.files),
        "shared" if ui.shared.views.visible_child_name().as_deref() == Some("files") => {
            Some(&ui.shared.files)
        }
        _ => None,
    }
}
