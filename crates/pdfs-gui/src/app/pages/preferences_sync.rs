//! Preferences → Sync: the patterns every synced folder leaves out
//! (`ignore_patterns`) and what the conflict sweep may do (`conflict_sweep`).
//!
//! Both live in `config.json`. The daemon reads the patterns again on every
//! sync pass, so a change here is followed by a Sync Now and applies at once;
//! the sweep mode is read when the service starts, so a change offers a
//! restart, as the mountpoint does.

use crate::*;
use pdfs_core::config::SweepMode;
use pdfs_core::syncignore::{DEFAULT_IGNORE_PATTERNS, check_pattern};

/// The widgets of the Sync page, built with the rest of the dialog and wired
/// once the app's state exists.
pub(crate) struct SyncPrefs {
    pub(crate) page: adw::PreferencesPage,
    patterns: adw::PreferencesGroup,
    add_row: adw::EntryRow,
    reset: gtk4::Button,
    sweep_row: adw::ComboRow,
    /// Whether the built-in patterns are shown, kept across repaints.
    builtin_expanded: Cell<bool>,
}

/// The sweep modes in the order the combo row lists them.
const SWEEP_MODES: [SweepMode; 3] = [SweepMode::Report, SweepMode::Enforce, SweepMode::Off];

pub(crate) fn build_sync_prefs() -> SyncPrefs {
    let reset = gtk4::Button::builder()
        .label(gettext("Reset"))
        .tooltip_text(gettext("Use the built-in patterns again"))
        .valign(gtk4::Align::Center)
        .build();
    reset.add_css_class("flat");
    let patterns = adw::PreferencesGroup::builder()
        .title(gettext("Excluded Files"))
        .description(gettext(
            "Files and folders matching these patterns are not synced in any synced folder. Use * for any characters, end a pattern with / to match folders only, and start it with ! to include something again. Excluding never deletes anything from Proton Drive.",
        ))
        .header_suffix(&reset)
        .build();
    let add_row = adw::EntryRow::builder()
        .title(gettext("Add Pattern"))
        .show_apply_button(true)
        .build();
    add_row.add_css_class("monospace");
    patterns.add(&add_row);

    let sweep_row = adw::ComboRow::builder()
        .title(gettext("Identical conflict copies"))
        .subtitle(gettext(
            "A copy made when a file changed in two places, found to be the same as the file",
        ))
        .model(&gtk4::StringList::new(&[
            &gettext("Report Only"),
            &gettext("Move to Trash"),
            &gettext("Off"),
        ]))
        .build();
    let conflicts = adw::PreferencesGroup::builder()
        .title(gettext("Conflict Copies"))
        .build();
    conflicts.add(&sweep_row);

    let page = adw::PreferencesPage::builder()
        .title(gettext("Sync"))
        .name("sync")
        .icon_name("pdfs-sync-symbolic")
        .build();
    page.add(&patterns);
    page.add(&conflicts);
    SyncPrefs {
        page,
        patterns,
        add_row,
        reset,
        sweep_row,
        builtin_expanded: Cell::new(false),
    }
}

/// Fill the page from the config and connect its controls.
pub(crate) fn wire_sync_prefs(ui: &Rc<Ui>, widgets: SyncPrefs) {
    let widgets = Rc::new(widgets);
    let rows: Rc<RefCell<Vec<adw::PreferencesRow>>> = Rc::default();
    let config = ui.dirs.load_config();
    paint_patterns(ui, &widgets, &rows, &config);

    let mode = config.conflict_sweep.unwrap_or_default();
    let suppress = ui.status.settings_suppress.replace(true);
    widgets.sweep_row.set_selected(
        SWEEP_MODES
            .iter()
            .position(|m| *m == mode)
            .unwrap_or_default() as u32,
    );
    ui.status.settings_suppress.set(suppress);

    widgets.add_row.connect_changed(|row| {
        let text = row.text();
        if text.trim().is_empty() || check_pattern(text.trim()).is_ok() {
            row.remove_css_class("error");
        } else {
            row.add_css_class("error");
        }
    });
    let (ui_add, w_add, rows_add) = (ui.clone(), widgets.clone(), rows.clone());
    widgets.add_row.connect_apply(move |row| {
        let pattern = row.text().trim().to_string();
        if pattern.is_empty() {
            return;
        }
        if let Err(e) = check_pattern(&pattern) {
            tracing::debug!(%pattern, error = %e, "not a valid ignore pattern");
            prefs_toast(
                &ui_add,
                // Translators: {pattern} is what the user typed as a pattern.
                &gettext_f(
                    "“{pattern}” isn't a valid pattern",
                    &[("pattern", &pattern)],
                ),
            );
            return;
        }
        let added = change_patterns(&ui_add, &w_add, &rows_add, |list| {
            if !list.contains(&pattern) {
                list.push(pattern.clone());
            }
        });
        if added {
            row.set_text("");
        }
    });

    let (ui_reset, w_reset, rows_reset) = (ui.clone(), widgets.clone(), rows.clone());
    widgets.reset.connect_clicked(move |_| {
        let (ui, w, rows) = (ui_reset.clone(), w_reset.clone(), rows_reset.clone());
        confirm_destructive(
            &ui_reset.status.prefs,
            &gettext("Reset Excluded Files?"),
            &gettext("Your own patterns are removed, and the built-in ones apply again."),
            &gettext("Reset"),
            move || {
                change_patterns(&ui, &w, &rows, |list| {
                    *list = default_patterns();
                });
            },
        );
    });

    let ui_sweep = ui.clone();
    widgets.sweep_row.connect_selected_notify(move |row| {
        if ui_sweep.status.settings_suppress.get() {
            return;
        }
        let Some(mode) = SWEEP_MODES.get(row.selected() as usize).copied() else {
            return;
        };
        let stored = (mode != SweepMode::default()).then_some(mode);
        match ui_sweep
            .dirs
            .update_config(|config| config.conflict_sweep = stored)
        {
            Ok(()) => offer_restart(&ui_sweep),
            Err(e) => prefs_toast(
                &ui_sweep,
                &gettext_f(
                    "{what}: {detail}",
                    &[
                        ("what", &gettext("Couldn't save the setting")),
                        ("detail", &e.to_string()),
                    ],
                ),
            ),
        }
    });
}

fn default_patterns() -> Vec<String> {
    DEFAULT_IGNORE_PATTERNS
        .iter()
        .map(|p| (*p).to_string())
        .collect()
}

/// Apply `edit` to the patterns in effect and save them, then show them and
/// have every synced folder sync with them. A list equal to the built-in one
/// is saved as "unset", so it follows the built-in list in later releases.
/// Says whether it saved.
fn change_patterns(
    ui: &Rc<Ui>,
    widgets: &Rc<SyncPrefs>,
    rows: &Rc<RefCell<Vec<adw::PreferencesRow>>>,
    edit: impl FnOnce(&mut Vec<String>),
) -> bool {
    let saved = ui.dirs.update_config(|config| {
        let mut list = config.resolved_ignore_patterns();
        edit(&mut list);
        config.ignore_patterns = (list != default_patterns()).then_some(list);
        config.clone()
    });
    match saved {
        Ok(config) => {
            paint_patterns(ui, widgets, rows, &config);
            // Best effort: the next pass picks the change up anyway.
            let _ = spawn_request(ui.dirs.control_socket(), Request::SyncNow { id: None });
            true
        }
        Err(e) => {
            prefs_toast(
                ui,
                &gettext_f(
                    "{what}: {detail}",
                    &[
                        ("what", &gettext("Couldn't save the patterns")),
                        ("detail", &e.to_string()),
                    ],
                ),
            );
            false
        }
    }
}

/// Show the patterns in effect below the entry row: the user's own one row
/// each, then the built-in ones folded into one row, since they are many and
/// rarely changed.
fn paint_patterns(
    ui: &Rc<Ui>,
    widgets: &Rc<SyncPrefs>,
    rows: &Rc<RefCell<Vec<adw::PreferencesRow>>>,
    config: &pdfs_core::config::AppConfig,
) {
    for row in rows.borrow_mut().drain(..) {
        widgets.patterns.remove(&row);
    }
    widgets
        .reset
        .set_sensitive(config.ignore_patterns.is_some());
    let list = config.resolved_ignore_patterns();
    let (builtin, own): (Vec<String>, Vec<String>) = list
        .into_iter()
        .partition(|p| DEFAULT_IGNORE_PATTERNS.contains(&p.as_str()));
    for pattern in own {
        let row = pattern_row(ui, widgets, rows, pattern);
        widgets.patterns.add(&row);
        rows.borrow_mut().push(row.upcast());
    }
    // Every built-in pattern, on or off, so one turned off can be turned on
    // again without losing the user's own.
    let on = builtin.len();
    let total = DEFAULT_IGNORE_PATTERNS.len();
    let expander = adw::ExpanderRow::builder()
        .title(gettext("Built-in patterns"))
        // Translators: {on} and {total} are numbers of patterns.
        .subtitle(gettext_f(
            "{on} of {total} on: version control, dependency and build folders, and what editors, downloads and systems leave behind",
            &[("on", &on.to_string()), ("total", &total.to_string())],
        ))
        .expanded(widgets.builtin_expanded.get())
        .build();
    let w = widgets.clone();
    expander.connect_expanded_notify(move |row| w.builtin_expanded.set(row.is_expanded()));
    for pattern in DEFAULT_IGNORE_PATTERNS {
        let row = adw::SwitchRow::builder()
            .title(format!("<tt>{}</tt>", glib::markup_escape_text(pattern)))
            .use_markup(true)
            .active(builtin.iter().any(|p| p == pattern))
            .build();
        let (ui, w, r) = (ui.clone(), widgets.clone(), rows.clone());
        row.connect_active_notify(move |row| {
            let active = row.is_active();
            // Repainting replaces this row; let the switch finish first.
            let (ui, w, r) = (ui.clone(), w.clone(), r.clone());
            glib::idle_add_local_once(move || {
                change_patterns(&ui, &w, &r, move |list| {
                    list.retain(|p| p != pattern);
                    if active {
                        list.push((*pattern).to_string());
                    }
                });
            });
        });
        expander.add_row(&row);
    }
    widgets.patterns.add(&expander);
    rows.borrow_mut().push(expander.upcast());
}

/// One pattern, in monospace, with a button that removes it.
fn pattern_row(
    ui: &Rc<Ui>,
    widgets: &Rc<SyncPrefs>,
    rows: &Rc<RefCell<Vec<adw::PreferencesRow>>>,
    pattern: String,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(format!("<tt>{}</tt>", glib::markup_escape_text(&pattern)))
        .use_markup(true)
        .build();
    let remove = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text(gettext("Remove"))
        .valign(gtk4::Align::Center)
        .build();
    remove.add_css_class("flat");
    let (ui, w, r) = (ui.clone(), widgets.clone(), rows.clone());
    remove.connect_clicked(move |_| {
        let pattern = pattern.clone();
        change_patterns(&ui, &w, &r, move |list| list.retain(|p| *p != pattern));
    });
    row.add_suffix(&remove);
    row
}

/// The sweep mode is read when the service starts; offer that restart.
fn offer_restart(ui: &Rc<Ui>) {
    let confirm = adw::AlertDialog::builder()
        .heading(gettext("Restart Proton Drive?"))
        .body(gettext(
            "The new setting applies after Proton Drive restarts.",
        ))
        .build();
    confirm.add_response("later", &gettext("Later"));
    confirm.add_response("restart", &gettext("Restart Now"));
    confirm.set_response_appearance("restart", adw::ResponseAppearance::Suggested);
    confirm.set_default_response(Some("restart"));
    confirm.set_close_response("later");
    let ui_restart = ui.clone();
    confirm.connect_response(None, move |_, resp| {
        if resp == "restart" {
            restart_service_then(&ui_restart, |_| {});
        }
    });
    confirm.present(Some(&ui.status.prefs));
}

/// A toast on the Preferences dialog, which covers the window's own toasts.
pub(crate) fn prefs_toast(ui: &Rc<Ui>, message: &str) {
    ui.status.prefs.add_toast(
        adw::Toast::builder()
            .title(message)
            .use_markup(false)
            .timeout(6)
            .build(),
    );
}
