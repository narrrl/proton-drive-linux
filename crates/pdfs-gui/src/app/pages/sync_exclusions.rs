//! The Excluded Files group on the Sync page's Folders view: what is left out
//! of every synced folder and of each one on its own, with how many patterns
//! each has, and an Add Pattern dialog that asks where a pattern goes.
//!
//! The rows open the editors that already exist: Preferences → Sync for the
//! patterns of all folders, the Excluded Files dialog for a folder's own
//! `.pdfsignore`. Only folders kept on this computer are listed; an
//! online-only folder has no local files for patterns to leave out. With none
//! of those, the group still shows, so the patterns can be found and set up
//! before a folder is switched to Synced, and says why they do nothing yet.

use crate::*;
use pdfs_core::syncignore::{FolderRules, add_folder_pattern, check_pattern};

pub(crate) struct ExclusionsState {
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) add: gtk4::Button,
    pub(crate) rows: RefCell<Vec<adw::ActionRow>>,
    /// The synced folders kept on this computer the rows were built for, as
    /// id and root.
    pub(crate) folders: RefCell<Vec<(i64, PathBuf)>>,
    /// Counts painted, so a slow read from an older paint does not land on
    /// newer rows.
    pub(crate) generation: Cell<u64>,
}

pub(crate) fn build_exclusions_group() -> (adw::PreferencesGroup, gtk4::Button) {
    let add = gtk4::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .label(gettext("Add Pattern"))
                .icon_name("list-add-symbolic")
                .build(),
        )
        .tooltip_text(gettext(
            "Leave files matching a pattern out of all synced folders or one of them",
        ))
        .valign(gtk4::Align::Center)
        .build();
    add.add_css_class("flat");
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Excluded Files"))
        .description(gettext(
            "Files matching these patterns stay on this computer and are not synced. Excluding never deletes anything from Proton Drive.",
        ))
        .header_suffix(&add)
        .visible(false)
        .build();
    (group, add)
}

pub(crate) fn wire_exclusions(ui: &Rc<Ui>) {
    let ui_add = ui.clone();
    ui.locations
        .exclusions
        .add
        .connect_clicked(move |_| present_add_pattern_dialog(&ui_add, None));
    // Preferences change the patterns of all folders; show the new count.
    let ui_prefs = ui.clone();
    ui.status
        .prefs
        .connect_closed(move |_| refresh_exclusions(&ui_prefs));
}

/// Rebuild the rows for the folders in `items` when the set of synced folders
/// kept on this computer changed, or on the first paint. The tick repaints the folder list every few
/// seconds; the rows and their counts stay as they are in between.
pub(crate) fn paint_exclusions(ui: &Rc<Ui>, items: &[MountSpec]) {
    let folders: Vec<(i64, PathBuf)> = items
        .iter()
        .filter(|spec| spec.mode == MountMode::Mirror)
        .filter_map(|spec| match spec.kind {
            MountKind::Device { sync_folder_id } | MountKind::Attached { sync_folder_id } => {
                Some((sync_folder_id, PathBuf::from(&spec.local_path)))
            }
            _ => None,
        })
        .collect();
    let state = &ui.locations.exclusions;
    if *state.folders.borrow() == folders && !state.rows.borrow().is_empty() {
        return;
    }
    *state.folders.borrow_mut() = folders;
    refresh_exclusions(ui);
}

/// Build the rows again from the folders last painted, and read every count
/// anew: after a pattern was added or a list edited.
pub(crate) fn refresh_exclusions(ui: &Rc<Ui>) {
    let state = &ui.locations.exclusions;
    for row in state.rows.borrow_mut().drain(..) {
        state.group.remove(&row);
    }
    let folders = state.folders.borrow().clone();
    state.group.set_visible(true);
    let generation = state.generation.get() + 1;
    state.generation.set(generation);

    let config = ui.dirs.load_config();
    let all = adw::ActionRow::builder()
        .title(gettext("All Synced Folders"))
        .subtitle(glib::markup_escape_text(&all_folders_subtitle(&config)).as_str())
        .activatable(true)
        .build();
    all.add_prefix(&gtk4::Image::from_icon_name("view-list-symbolic"));
    all.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    let ui_all = ui.clone();
    all.connect_activated(move |_| {
        let prefs = &ui_all.status.prefs;
        prefs.set_visible_page_name("sync");
        prefs.present(ui_window(&ui_all).as_ref());
    });
    state.group.add(&all);
    let mut rows = vec![all];
    if folders.is_empty() {
        let none = adw::ActionRow::builder()
            .title(gettext("No folder is set to Synced"))
            .subtitle(gettext(
                "Patterns apply only to folders kept on this computer. Online-only folders upload every file saved in them.",
            ))
            .build();
        none.add_prefix(&gtk4::Image::from_icon_name("info-outline-symbolic"));
        state.group.add(&none);
        rows.push(none);
    }

    let mut counted = Vec::new();
    for (id, root) in folders {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&name).as_str())
            .tooltip_text(root.display().to_string())
            .subtitle(glib::markup_escape_text(&tilde_path(&root.display().to_string())).as_str())
            .subtitle_lines(1)
            .activatable(true)
            .build();
        row.add_prefix(&gtk4::Image::from_icon_name("folder-symbolic"));
        row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
        let (ui_row, root_row) = (ui.clone(), root.clone());
        row.connect_activated(move |_| present_ignore_dialog(&ui_row, id, root_row.clone()));
        state.group.add(&row);
        counted.push((row.clone(), root));
        rows.push(row);
    }
    *state.rows.borrow_mut() = rows;

    // A folder may sit on a slow or sleeping disk: count off the main thread.
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let roots: Vec<PathBuf> = counted.iter().map(|(_, root)| root.clone()).collect();
        let counts = gio::spawn_blocking(move || {
            roots
                .iter()
                .map(|root| {
                    FolderRules::read(root)
                        .ok()
                        .map(|rules| rules.patterns().len())
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        if ui.locations.exclusions.generation.get() != generation {
            return;
        }
        for ((row, root), count) in counted.iter().zip(counts) {
            let state = match count {
                Some(0) => gettext("no own patterns"),
                // Translators: how many patterns a synced folder has of its own.
                Some(n) => ngettext_f("{n} own pattern", "{n} own patterns", n as u64, &[]),
                None => gettext("own patterns unreadable"),
            };
            // Translators: a synced folder's location and how many patterns of its own it has, such as "~/Projects · 2 own patterns".
            let subtitle = gettext_f(
                "{path} · {state}",
                &[
                    ("path", &tilde_path(&root.display().to_string())),
                    ("state", &state),
                ],
            );
            row.set_subtitle(glib::markup_escape_text(&subtitle).as_str());
        }
    });
}

/// How many patterns apply to every synced folder, and whether they are the
/// built-in ones.
fn all_folders_subtitle(config: &pdfs_core::config::AppConfig) -> String {
    let n = config.resolved_ignore_patterns().len() as u64;
    match (&config.ignore_patterns, n) {
        (_, 0) => gettext("No patterns: every file is synced"),
        (None, n) => ngettext_f("{n} built-in pattern", "{n} built-in patterns", n, &[]),
        (Some(_), n) => ngettext_f("{n} pattern", "{n} patterns", n, &[]),
    }
}

/// Ask for a pattern and where it applies: every synced folder, or one of
/// them (`folder`, preselected when given).
pub(crate) fn present_add_pattern_dialog(ui: &Rc<Ui>, folder: Option<i64>) {
    let folders = ui.locations.exclusions.folders.borrow().clone();
    let pattern_row = adw::EntryRow::builder().title(gettext("Pattern")).build();
    pattern_row.add_css_class("monospace");
    let mut places = vec![gettext("All synced folders")];
    places.extend(
        folders
            .iter()
            .map(|(_, root)| tilde_path(&root.display().to_string())),
    );
    let places: Vec<&str> = places.iter().map(String::as_str).collect();
    let scope_row = adw::ComboRow::builder()
        .title(gettext("Apply To"))
        .model(&gtk4::StringList::new(&places))
        .build();
    if let Some(at) = folder.and_then(|id| folders.iter().position(|(f, _)| *f == id)) {
        scope_row.set_selected(at as u32 + 1);
    }
    let rows = adw::PreferencesGroup::new();
    rows.add(&pattern_row);
    rows.add(&scope_row);

    let hint = gtk4::Label::builder()
        .label(gettext(
            "Use * for any characters, such as *.db, and end a pattern with / to match folders only, such as build/.",
        ))
        .wrap(true)
        .xalign(0.0)
        .build();
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    let problem = gtk4::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .build();
    problem.add_css_class("error");

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.append(&rows);
    body.append(&hint);
    body.append(&problem);

    let cancel = gtk4::Button::with_label(&gettext("Cancel"));
    let add = gtk4::Button::with_label(&gettext("Add"));
    add.add_css_class("suggested-action");
    add.set_sensitive(false);
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&add);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&body));
    let dialog = adw::Dialog::builder()
        .title(gettext("Add Pattern"))
        .content_width(460)
        .child(&view)
        .build();

    let (add_check, problem_check) = (add.clone(), problem.clone());
    pattern_row.connect_changed(move |row| {
        let verdict = pattern_problem(&row.text());
        match &verdict {
            Some(message) if !row.text().trim().is_empty() => {
                row.add_css_class("error");
                problem_check.set_text(message);
                problem_check.set_visible(true);
            }
            _ => {
                row.remove_css_class("error");
                problem_check.set_visible(false);
            }
        }
        add_check.set_sensitive(verdict.is_none());
    });
    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });

    let submit = {
        let (ui, dialog, pattern_row, scope_row, add, problem) = (
            ui.clone(),
            dialog.clone(),
            pattern_row.clone(),
            scope_row.clone(),
            add.clone(),
            problem.clone(),
        );
        move || {
            if !add.is_sensitive() {
                return;
            }
            let pattern = pattern_row.text().trim().to_string();
            let target = match scope_row.selected() {
                0 => None,
                at => folders.get(at as usize - 1).cloned(),
            };
            // One click, one save: the button comes back if the save fails.
            add.set_sensitive(false);
            add_pattern(&ui, &dialog, &add, &problem, pattern, target);
        }
    };
    let submit_enter = submit.clone();
    pattern_row.connect_entry_activated(move |_| submit_enter());
    add.connect_clicked(move |_| submit());

    dialog.present(ui_window(ui).as_ref());
    pattern_row.grab_focus();
}

/// What is wrong with `text` as a pattern to add, or `None` when it is one.
fn pattern_problem(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return Some(String::new());
    }
    if text.starts_with('#') {
        return Some(gettext(
            "A line starting with # is a comment, not a pattern.",
        ));
    }
    check_pattern(text).err().map(|e| {
        tracing::debug!(pattern = %text, error = %e, "not a valid ignore pattern");
        // Translators: {pattern} is what the user typed as a pattern.
        gettext_f("“{pattern}” isn't a valid pattern.", &[("pattern", text)])
    })
}

/// Save `pattern` for every synced folder (`target` is `None`) or for the
/// folder `target`, then close `dialog`, say so and sync. A failure shows in
/// `problem`, in the dialog, which stays open.
fn add_pattern(
    ui: &Rc<Ui>,
    dialog: &adw::Dialog,
    add: &gtk4::Button,
    problem: &gtk4::Label,
    pattern: String,
    target: Option<(i64, PathBuf)>,
) {
    let (ui, dialog, add, problem) = (ui.clone(), dialog.clone(), add.clone(), problem.clone());
    glib::spawn_future_local(async move {
        let (id, place) = match &target {
            None => (None, gettext("all synced folders")),
            Some((id, root)) => (Some(*id), tilde_path(&root.display().to_string())),
        };
        let added: Result<bool, String> = match target {
            None => {
                let mut added = false;
                ui.dirs
                    .change_ignore_patterns(|list| {
                        if !list.contains(&pattern) {
                            list.push(pattern.clone());
                            added = true;
                        }
                    })
                    .map(|_| added)
                    .map_err(|e| e.to_string())
            }
            Some((_, root)) => {
                let p = pattern.clone();
                gio::spawn_blocking(move || add_folder_pattern(&root, &p))
                    .await
                    .unwrap_or_else(|_| Err(std::io::Error::other("the save did not finish")))
                    .map_err(|e| e.to_string())
            }
        };
        match added {
            Ok(added) => {
                dialog.close();
                let message = if added {
                    // Translators: {pattern} is an ignore pattern, {place} "all synced folders" or a folder path.
                    gettext_f(
                        "“{pattern}” is now excluded in {place}",
                        &[("pattern", &pattern), ("place", &place)],
                    )
                } else {
                    gettext_f(
                        "“{pattern}” was already excluded in {place}",
                        &[("pattern", &pattern), ("place", &place)],
                    )
                };
                toast(&ui, &message);
                refresh_exclusions(&ui);
                if added {
                    // Best effort: the next pass picks the change up anyway.
                    let _ = spawn_request(ui.dirs.control_socket(), Request::SyncNow { id });
                }
            }
            Err(e) => {
                add.set_sensitive(true);
                problem.set_text(&gettext_f(
                    "{what}: {detail}",
                    &[
                        ("what", &gettext("Couldn't save the patterns")),
                        ("detail", &e),
                    ],
                ));
                problem.set_visible(true);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_patterns_can_be_added() {
        assert!(pattern_problem("*.db").is_none());
        assert!(pattern_problem("  build/ ").is_none());
        assert!(pattern_problem("").is_some());
        assert!(pattern_problem("   ").is_some());
        assert!(pattern_problem("# note").is_some());
        assert!(pattern_problem("*.{jpg,png").is_some());
    }
}
