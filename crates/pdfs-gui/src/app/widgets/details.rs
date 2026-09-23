use crate::*;

pub(crate) struct DetailsState {
    /// Widgets in the details pane, repainted from the selected entry.
    pub(crate) details: DetailsWidgets,
    /// The entry the details pane is currently showing, so its buttons act on the
    /// same one the user is looking at even after the model is repopulated.
    pub(crate) details_entry: RefCell<Option<DirEntry>>,
    /// Set while the details pane is being populated, so setting the offline
    /// switch programmatically doesn't fire a pin/unpin round-trip.
    pub(crate) details_suppress: Cell<bool>,
    /// The one selection model behind both the grid and the list, so switching
    /// views keeps what is selected; it drives the details pane.
    pub(crate) selection: gtk4::MultiSelection,
}

/// The widgets in the browser's details pane that a selection repaints.
pub(crate) struct DetailsWidgets {
    pub(crate) icon: gtk4::Image,
    pub(crate) name: gtk4::Label,
    pub(crate) kind: gtk4::Label,
    pub(crate) size_row: adw::ActionRow,
    pub(crate) modified_row: adw::ActionRow,
    pub(crate) path_row: adw::ActionRow,
    pub(crate) pin_row: adw::SwitchRow,
    pub(crate) open_button: gtk4::Button,
    pub(crate) rename_button: gtk4::Button,
    /// Who has access and whether there is a public link; filled on selection.
    pub(crate) sharing_row: adw::ActionRow,
    pub(crate) share_button: gtk4::Button,
    pub(crate) copy_link_button: gtk4::Button,
    /// Files only — folders have no revision history.
    pub(crate) versions_group: adw::PreferencesGroup,
    pub(crate) versions_row: adw::ActionRow,
    pub(crate) versions_button: gtk4::Button,
    pub(crate) trash_button: gtk4::Button,
    pub(crate) close_button: gtk4::Button,
    /// The header toggle that asks for the pane. Selecting an entry only
    /// fills the pane; it is shown while this is on, so a click never moves
    /// the files under the pointer halfway through a double-click.
    pub(crate) toggle: gtk4::ToggleButton,
    /// "details" while an entry is shown, "empty" with nothing selected.
    pub(crate) pages: gtk4::Stack,
}

/// The details pane shown beside the file views: a big type icon over the entry's
/// name, then Info, Offline, Sharing and Versions sections and the primary
/// actions. Built empty; [`show_details`] fills it from the selected
/// [`DirEntry`] and [`wire_details`] connects the buttons.
pub(crate) fn build_details_pane() -> (gtk4::Widget, DetailsWidgets) {
    let toggle = gtk4::ToggleButton::builder()
        .icon_name("sidebar-show-right-symbolic")
        .tooltip_text(gettext("Details (Alt+Enter)"))
        .build();

    let close_button = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text(gettext("Close details"))
        .halign(gtk4::Align::End)
        .build();
    close_button.add_css_class("flat");
    close_button.add_css_class("circular");

    let icon = gtk4::Image::builder()
        .icon_name("text-x-generic-symbolic")
        .pixel_size(64)
        .build();
    let name = gtk4::Label::builder()
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .justify(gtk4::Justification::Center)
        .build();
    name.add_css_class("title-4");
    let kind = gtk4::Label::new(None);
    kind.add_css_class("dim-label");
    kind.add_css_class("caption");

    let head = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    head.set_margin_bottom(6);
    head.append(&icon);
    head.append(&name);
    head.append(&kind);

    let props = adw::PreferencesGroup::builder()
        .title(gettext("Info"))
        .build();
    let size_row = adw::ActionRow::builder()
        .title(pgettext("column", "Size"))
        .subtitle("—")
        .build();
    size_row.add_css_class("property");
    let modified_row = adw::ActionRow::builder()
        .title(pgettext("column", "Modified"))
        .subtitle("—")
        .build();
    modified_row.add_css_class("property");
    let path_row = adw::ActionRow::builder()
        .title(gettext("Location"))
        .subtitle("—")
        .subtitle_lines(3)
        .build();
    path_row.add_css_class("property");
    props.add(&size_row);
    props.add(&modified_row);
    props.add(&path_row);

    let offline = adw::PreferencesGroup::builder()
        .title(pgettext("state", "Offline"))
        .build();
    let pin_row = adw::SwitchRow::builder()
        .title(gettext("Available offline"))
        .subtitle(gettext(
            "Keep a copy on this computer, even without a connection.",
        ))
        .build();
    offline.add(&pin_row);

    let sharing = adw::PreferencesGroup::builder()
        .title(gettext("Sharing"))
        .build();
    let sharing_row = adw::ActionRow::builder()
        .title(gettext("Access"))
        .subtitle("—")
        .build();
    sharing_row.add_css_class("property");
    sharing.add(&sharing_row);
    let share_button = gtk4::Button::builder().label(gettext("Share…")).build();
    let copy_link_button = gtk4::Button::builder().label(gettext("Copy Link")).build();
    let share_actions = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .margin_top(6)
        .build();
    share_actions.append(&share_button);
    share_actions.append(&copy_link_button);
    sharing.add(&share_actions);

    let versions_group = adw::PreferencesGroup::builder()
        .title(gettext("Versions"))
        .build();
    let versions_row = adw::ActionRow::builder()
        .title(gettext("History"))
        .subtitle("—")
        .build();
    versions_row.add_css_class("property");
    versions_group.add(&versions_row);
    let versions_button = gtk4::Button::builder()
        .label(gettext("Versions…"))
        .margin_top(6)
        .build();
    versions_group.add(&versions_button);

    let open_button = gtk4::Button::builder()
        .label(pgettext("verb", "Open"))
        .build();
    open_button.add_css_class("suggested-action");
    open_button.add_css_class("pill");
    let rename_button = gtk4::Button::builder().label(gettext("Rename")).build();
    rename_button.add_css_class("pill");
    let trash_button = gtk4::Button::builder()
        .label(gettext("Move to Trash"))
        .build();
    trash_button.add_css_class("destructive-action");
    trash_button.add_css_class("pill");

    let actions = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    actions.set_margin_top(6);
    actions.append(&open_button);
    actions.append(&rename_button);
    actions.append(&trash_button);

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&close_button);
    inner.append(&head);
    inner.append(&props);
    inner.append(&offline);
    inner.append(&sharing);
    inner.append(&versions_group);
    inner.append(&actions);

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&inner)
        .build();
    // With the pane open and nothing selected (a fresh folder, a cleared
    // selection) it says so rather than closing: opening and closing it by
    // itself would move the files under the pointer.
    let empty_close = gtk4::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text(gettext("Close details"))
        .halign(gtk4::Align::End)
        .margin_top(12)
        .margin_end(12)
        .build();
    empty_close.add_css_class("flat");
    empty_close.add_css_class("circular");
    let toggle_off = toggle.clone();
    empty_close.connect_clicked(move |_| toggle_off.set_active(false));
    let empty_status = adw::StatusPage::builder()
        .icon_name("sidebar-show-right-symbolic")
        .title(gettext("No item selected"))
        .description(gettext("Select a file or folder to see its details."))
        .vexpand(true)
        .build();
    empty_status.add_css_class("compact");
    let empty = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    empty.append(&empty_close);
    empty.append(&empty_status);

    let pages = gtk4::Stack::new();
    pages.add_named(&scroll, Some("details"));
    pages.add_named(&empty, Some("empty"));
    pages.set_visible_child_name("empty");
    let pane = adw::ToolbarView::new();
    pane.set_content(Some(&pages));
    pane.add_css_class("background");

    (
        pane.upcast(),
        DetailsWidgets {
            icon,
            name,
            kind,
            size_row,
            modified_row,
            path_row,
            pin_row,
            sharing_row,
            share_button,
            copy_link_button,
            versions_group,
            versions_row,
            open_button,
            rename_button,
            versions_button,
            trash_button,
            close_button,
            toggle,
            pages,
        },
    )
}

/// Connect the details pane: mirror the grid's and list's selection into it, and
/// wire its buttons back onto the entry it's showing.
pub(crate) fn wire_details(ui: &Rc<Ui>) {
    {
        let ui_sel = ui.clone();
        ui.details
            .selection
            .connect_selection_changed(move |_, _, _| {
                // The details pane describes *one* entry. A batch is the bulk bar's
                // business, so the pane steps aside rather than picking a member of
                // the selection to speak for the rest.
                let entries = selected_entries(&ui_sel);
                match entries.as_slice() {
                    [entry] => show_details(&ui_sel, entry),
                    _ => clear_details(&ui_sel),
                }
                sync_bulk_bar(&ui_sel);
            });
    }

    let ui_close = ui.clone();
    ui.details.details.close_button.connect_clicked(move |_| {
        ui_close.details.details.toggle.set_active(false);
    });

    let ui_toggle = ui.clone();
    ui.details.details.toggle.connect_toggled(move |toggle| {
        ui_toggle.browser.split.set_show_sidebar(toggle.is_active());
    });
    // On a narrow window the pane overlays the files, and a click outside it
    // closes it; the toggle follows so the next click on it opens it again.
    let ui_split = ui.clone();
    ui.browser.split.connect_show_sidebar_notify(move |split| {
        let toggle = &ui_split.details.details.toggle;
        if toggle.is_active() != split.shows_sidebar() {
            toggle.set_active(split.shows_sidebar());
        }
    });

    let ui_open = ui.clone();
    ui.details.details.open_button.connect_clicked(move |_| {
        // Bind the clone to a local so the `Ref` from `borrow()` is dropped before
        // `activate_entry` runs; it navigates and repaints the details pane, which
        // takes `details_entry.borrow_mut()` and would panic against a live borrow.
        let entry = ui_open.details.details_entry.borrow().clone();
        if let Some(entry) = entry {
            activate_entry(&ui_open, &entry);
        }
    });
    let ui_rename = ui.clone();
    ui.details.details.rename_button.connect_clicked(move |_| {
        let entry = ui_rename.details.details_entry.borrow().clone();
        if let Some(entry) = entry {
            prompt_rename(&ui_rename, &entry);
        }
    });
    let ui_versions = ui.clone();
    ui.details
        .details
        .versions_button
        .connect_clicked(move |_| {
            let entry = ui_versions.details.details_entry.borrow().clone();
            if let Some(entry) = entry {
                open_versions_dialog(&ui_versions, &entry);
            }
        });
    let ui_share = ui.clone();
    ui.details.details.share_button.connect_clicked(move |_| {
        let entry = ui_share.details.details_entry.borrow().clone();
        if let Some(entry) = entry {
            open_share_dialog(&ui_share, &entry);
        }
    });
    let ui_link = ui.clone();
    ui.details
        .details
        .copy_link_button
        .connect_clicked(move |_| {
            let entry = ui_link.details.details_entry.borrow().clone();
            if let Some(entry) = entry {
                copy_entry_link(&ui_link, &entry);
            }
        });
    let ui_trash = ui.clone();
    ui.details.details.trash_button.connect_clicked(move |_| {
        let entry = ui_trash.details.details_entry.borrow().clone();
        if let Some(entry) = entry {
            trash_entry(&ui_trash, &entry);
        }
    });
    let ui_pin = ui.clone();
    ui.details
        .details
        .pin_row
        .connect_active_notify(move |row| {
            if ui_pin.details.details_suppress.get() {
                return;
            }
            let Some(entry) = ui_pin.details.details_entry.borrow().clone() else {
                return;
            };
            // The switch reads the *desired* state; `toggle_pin` derives the request
            // from the entry's current one, so only act when they actually differ.
            if row.is_active() != entry.pinned {
                toggle_pin(&ui_pin, &entry);
            }
        });
}

/// Paint the details pane from `entry`. It shows while the header toggle is on.
pub(crate) fn show_details(ui: &Rc<Ui>, entry: &DirEntry) {
    ui.details.details_suppress.set(true);
    let d = &ui.details.details;
    d.icon.set_icon_name(Some(icon_base_for(entry)));
    d.name.set_label(&entry.name);
    d.kind.set_label(&if entry.is_dir {
        gettext("Folder")
    } else {
        gettext("File")
    });
    d.size_row.set_subtitle(&if entry.is_dir {
        "—".to_string()
    } else {
        human_bytes(entry.size)
    });
    d.size_row.set_visible(!entry.is_dir);
    d.modified_row
        .set_subtitle(&format_modified(entry.modified));
    let rel = entry_rel(ui, entry);
    let parent = match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    };
    d.path_row.set_subtitle(&if parent.is_empty() {
        gettext("Proton Drive")
    } else {
        parent.to_string()
    });
    // Pinning is per file: the context menu and the selection bar offer it for
    // files only, so the pane must not offer it for folders either.
    d.pin_row.set_active(entry.pinned);
    d.pin_row.set_visible(!entry.is_dir);
    d.pin_row.set_sensitive(*ui.mounted.borrow());
    d.open_button.set_label(&if entry.is_dir {
        gettext("Open folder")
    } else {
        pgettext("verb", "Open")
    });
    // Only files have revisions, and only a connected daemon can fetch them.
    let mounted = *ui.mounted.borrow();
    d.versions_group.set_visible(!entry.is_dir);
    d.versions_button.set_sensitive(mounted);
    d.share_button.set_sensitive(mounted);
    d.copy_link_button.set_sensitive(mounted);
    d.sharing_row.set_subtitle("—");
    d.versions_row.set_subtitle("—");
    ui.details.details_suppress.set(false);

    *ui.details.details_entry.borrow_mut() = Some(entry.clone());
    if mounted {
        load_details_extras(ui, entry);
    }
    // Only the content changes. Whether the pane shows is the toggle's call:
    // this runs from `selection_changed`, on the *first* press of a
    // double-click, and sliding the pane in there would move the files so the
    // second press lands elsewhere and the folder never opens.
    d.pages.set_visible_child_name("details");
}

/// Fill the Sharing and Versions rows, which need the network. The answers
/// land only if the pane still shows the same entry.
fn load_details_extras(ui: &Rc<Ui>, entry: &DirEntry) {
    let by_uid = entry.path.is_empty() && !entry.uid.is_empty();
    let rel = entry_rel(ui, entry);
    let share_req = if by_uid {
        Request::ListShareByUid {
            uid: entry.uid.clone(),
        }
    } else {
        Request::ListShare { path: rel.clone() }
    };
    let rx = spawn_request(ui.dirs.control_socket(), share_req);
    let (ui_c, uid) = (ui.clone(), entry.uid.clone());
    glib::spawn_future_local(async move {
        let text = match rx.recv().await {
            Ok(Ok(Response::Share { entries, link })) => {
                sharing_summary(entries.len(), link.is_some())
            }
            _ => gettext("Couldn't load sharing"),
        };
        if details_showing(&ui_c, &uid) {
            ui_c.details.details.sharing_row.set_subtitle(&text);
        }
    });

    if entry.is_dir {
        return;
    }
    let req = if by_uid {
        Request::ListRevisionsByUid {
            uid: entry.uid.clone(),
        }
    } else {
        Request::ListRevisions { path: rel }
    };
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let (ui_c, uid) = (ui.clone(), entry.uid.clone());
    glib::spawn_future_local(async move {
        let text = match rx.recv().await {
            Ok(Ok(Response::Revisions { items })) => versions_summary(&items),
            _ => gettext("Couldn't load versions"),
        };
        if details_showing(&ui_c, &uid) {
            ui_c.details.details.versions_row.set_subtitle(&text);
        }
    });
}

fn details_showing(ui: &Rc<Ui>, uid: &str) -> bool {
    ui.details
        .details_entry
        .borrow()
        .as_ref()
        .is_some_and(|e| e.uid == uid)
}

/// "Only you", "3 people", "Public link" or both.
pub(crate) fn sharing_summary(people: usize, link: bool) -> String {
    let n = people as u64;
    match (people, link) {
        (0, false) => gettext("Only you"),
        (0, true) => gettext("Anyone with the link"),
        (_, false) => ngettext_f("Shared with {n} person", "Shared with {n} people", n, &[]),
        (_, true) => ngettext_f(
            "Shared with {n} person · Public link",
            "Shared with {n} people · Public link",
            n,
            &[],
        ),
    }
}

/// "3 versions · latest 2 Sep 2026".
pub(crate) fn versions_summary(items: &[RevisionInfo]) -> String {
    let latest = items.iter().map(|r| r.created).max();
    match (items.len(), latest) {
        (0, _) | (_, None) => gettext("No earlier versions"),
        // Translators: {date} is the date of the only version.
        (1, Some(t)) => gettext_f("1 version · {date}", &[("date", &format_modified(t))]),
        // Translators: {date} is the date of the newest version.
        (n, Some(t)) => ngettext_f(
            "{n} version · latest {date}",
            "{n} versions · latest {date}",
            n as u64,
            &[("date", &format_modified(t))],
        ),
    }
}

/// Show the pane for `entry`, turning the header toggle on.
pub(crate) fn open_details(ui: &Rc<Ui>, entry: &DirEntry) {
    ui.details.details.toggle.set_active(true);
    show_details(ui, entry);
}

/// Alt+Enter: show or hide the pane for the selected entry.
pub(crate) fn toggle_details(ui: &Rc<Ui>) {
    let toggle = &ui.details.details.toggle;
    toggle.set_active(!toggle.is_active());
}

/// Forget the entry the pane was showing, so a stale entry can't be acted on
/// after the listing moves on. An open pane stays open and says nothing is
/// selected.
pub(crate) fn clear_details(ui: &Rc<Ui>) {
    ui.details.details.pages.set_visible_child_name("empty");
    *ui.details.details_entry.borrow_mut() = None;
}

/// The single entry the details pane is showing, if any. Backs the actions that
/// only make sense on one entry (rename, versions); the batch-capable ones ask
/// [`selected_entries`] instead.
pub(crate) fn selected_entry(ui: &Rc<Ui>) -> Option<DirEntry> {
    ui.details.details_entry.borrow().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharing_summary_names_people_and_link() {
        assert_eq!(sharing_summary(0, false), "Only you");
        assert_eq!(sharing_summary(0, true), "Anyone with the link");
        assert_eq!(sharing_summary(1, false), "Shared with 1 person");
        assert_eq!(
            sharing_summary(3, true),
            "Shared with 3 people · Public link"
        );
    }

    #[test]
    fn versions_summary_counts_revisions() {
        let rev = |created| RevisionInfo {
            id: String::new(),
            is_active: false,
            created,
            size_on_storage: 0,
            claimed_size: None,
            claimed_modified: None,
            signed_by: None,
            has_thumbnails: false,
        };
        assert_eq!(versions_summary(&[]), "No earlier versions");
        assert!(versions_summary(&[rev(10), rev(20)]).starts_with("2 versions · latest "));
    }
}
