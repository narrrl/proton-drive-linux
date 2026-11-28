//! Preferences → Apps: which program opens which file (`open_with`) and how
//! the search launcher shows itself (`prompt`).
//!
//! Neither involves the service. The app and the search launcher read both
//! again each time they open something, so a change applies to the next file
//! opened or the next search.

use crate::*;
use pdfs_core::menu::{PromptConfig, PromptMode};
use pdfs_core::opener::{
    OpenRule, OpenWith, command_available, format_command, parse_command, resolve_terminal,
};

/// The widgets of the Apps page, built with the rest of the dialog and wired
/// once the app's state exists.
pub(crate) struct AppsPrefs {
    pub(crate) page: adw::PreferencesPage,
    rules: adw::PreferencesGroup,
    add_rule: gtk4::Button,
    other_row: adw::EntryRow,
    terminal_row: adw::EntryRow,
    mode_row: adw::ComboRow,
    menu_row: adw::EntryRow,
    prompt_terminal_row: adw::EntryRow,
    limit_row: adw::SpinRow,
    icons_row: adw::SwitchRow,
    remember_row: adw::SwitchRow,
    clear_row: adw::ActionRow,
    clear_button: gtk4::Button,
}

/// The launcher modes in the order the combo row lists them.
const MODES: [PromptMode; 3] = [PromptMode::Gtk, PromptMode::Dmenu, PromptMode::Fzf];

/// The file classes a rule can name, with what the dialog calls them.
fn classes() -> [(&'static str, String); 6] {
    [
        ("@text", gettext("Text")),
        ("@document", gettext("Documents")),
        ("@image", gettext("Images")),
        ("@media", gettext("Audio and Video")),
        ("@dir", gettext("Folders")),
        ("@any", gettext("Everything")),
    ]
}

pub(crate) fn build_apps_prefs() -> AppsPrefs {
    let add_rule = gtk4::Button::builder()
        .label(gettext("Add Rule…"))
        .valign(gtk4::Align::Center)
        .build();
    add_rule.add_css_class("flat");
    let rules = adw::PreferencesGroup::builder()
        .title(gettext("Open Files With"))
        .description(gettext(
            "For files opened from Proton Drive and the search launcher. The first rule that matches a file decides; other files open in their default app.",
        ))
        .header_suffix(&add_rule)
        .build();

    let other_row = adw::EntryRow::builder()
        .title(gettext("Other files (empty: default app)"))
        .show_apply_button(true)
        .build();
    other_row.add_css_class("monospace");
    let terminal_row = adw::EntryRow::builder()
        .title(terminal_title())
        .show_apply_button(true)
        .build();
    terminal_row.add_css_class("monospace");
    let other = adw::PreferencesGroup::new();
    other.add(&other_row);
    other.add(&terminal_row);

    let mode_row = adw::ComboRow::builder()
        .title(gettext("Opens as"))
        .model(&gtk4::StringList::new(&[
            &gettext("Window"),
            &gettext("Your Launcher"),
            &gettext("fzf in a Terminal"),
        ]))
        .build();
    let menu_row = adw::EntryRow::builder()
        .title(gettext("Launcher command (empty: find one)"))
        .show_apply_button(true)
        .build();
    menu_row.add_css_class("monospace");
    let prompt_terminal_row = adw::EntryRow::builder()
        .title(terminal_title())
        .show_apply_button(true)
        .build();
    prompt_terminal_row.add_css_class("monospace");
    let limit_row = adw::SpinRow::builder()
        .title(gettext("Results shown"))
        .adjustment(&gtk4::Adjustment::new(
            pdfs_core::menu::DEFAULT_MENU_LIMIT as f64,
            5.0,
            500.0,
            5.0,
            25.0,
            0.0,
        ))
        .build();
    let icons_row = adw::SwitchRow::builder()
        .title(gettext("Show icons"))
        .subtitle(gettext("When the launcher can show them"))
        .build();
    let remember_row = adw::SwitchRow::builder()
        .title(gettext("Remember recent files"))
        .subtitle(gettext(
            "List what you opened before you type, and rank it higher in results. Off, the list is cleared.",
        ))
        .build();
    let clear_button = gtk4::Button::builder()
        .label(gettext("Clear"))
        .valign(gtk4::Align::Center)
        .build();
    let clear_row = adw::ActionRow::builder()
        .title(gettext("Clear recent files"))
        .subtitle(gettext(
            "The search window forgets what it opened. Your files are not touched.",
        ))
        .build();
    clear_row.add_suffix(&clear_button);
    let launcher = adw::PreferencesGroup::builder()
        .title(gettext("Search Launcher"))
        .description(gettext(
            "How “pdfs-prompt” shows search results, for example from a keyboard shortcut. Your launcher can be any program that reads choices like dmenu, such as rofi, wofi, fuzzel or bemenu.",
        ))
        .build();
    launcher.add(&mode_row);
    launcher.add(&menu_row);
    launcher.add(&prompt_terminal_row);
    launcher.add(&limit_row);
    launcher.add(&icons_row);
    launcher.add(&remember_row);
    launcher.add(&clear_row);

    let page = adw::PreferencesPage::builder()
        .title(gettext("Apps"))
        .name("apps")
        .icon_name("application-x-executable-symbolic")
        .build();
    page.add(&rules);
    page.add(&other);
    page.add(&launcher);
    AppsPrefs {
        page,
        rules,
        add_rule,
        other_row,
        terminal_row,
        mode_row,
        menu_row,
        prompt_terminal_row,
        limit_row,
        icons_row,
        remember_row,
        clear_row,
        clear_button,
    }
}

/// "Terminal", naming the one used when the row is left empty.
fn terminal_title() -> String {
    match resolve_terminal() {
        // Translators: {terminal} is a terminal command line such as "foot -e".
        Some(found) => gettext_f(
            "Terminal (empty: {terminal})",
            &[("terminal", &format_command(&found))],
        ),
        None => gettext("Terminal"),
    }
}

/// Shared by every handler of the page.
struct Apps {
    ui: Rc<Ui>,
    widgets: AppsPrefs,
    rule_rows: RefCell<Vec<adw::ActionRow>>,
}

/// Fill the page from the config and connect its controls.
pub(crate) fn wire_apps_prefs(ui: &Rc<Ui>, widgets: AppsPrefs) {
    let apps = Rc::new(Apps {
        ui: ui.clone(),
        widgets,
        rule_rows: RefCell::default(),
    });
    let config = ui.dirs.load_config();
    let open_with = config.resolved_open_with();
    let prompt = config.resolved_prompt();
    let w = &apps.widgets;

    let suppress = ui.status.settings_suppress.replace(true);
    paint_rules(&apps, &open_with);
    w.other_row
        .set_text(&format_command(open_with.default.as_deref().unwrap_or(&[])));
    w.terminal_row.set_text(&format_command(
        open_with.terminal.as_deref().unwrap_or(&[]),
    ));
    let mode = prompt.resolved_mode();
    w.mode_row
        .set_selected(MODES.iter().position(|m| *m == mode).unwrap_or_default() as u32);
    w.menu_row
        .set_text(&format_command(prompt.menu.as_deref().unwrap_or(&[])));
    w.prompt_terminal_row
        .set_text(&format_command(prompt.terminal.as_deref().unwrap_or(&[])));
    w.limit_row.set_value(prompt.resolved_menu_limit() as f64);
    w.icons_row.set_active(prompt.resolved_menu_icons());
    w.remember_row.set_active(prompt.resolved_remember_recent());
    show_mode_rows(w, mode);
    ui.status.settings_suppress.set(suppress);

    let a = apps.clone();
    w.add_rule
        .connect_clicked(move |_| edit_rule(&a, None, OpenRule::default()));

    // Command lines: checked as typed, saved on apply. Empty means "unset".
    connect_command_row(&apps, &w.other_row, |config, argv| {
        open_with_of(config).default = argv;
    });
    connect_command_row(&apps, &w.terminal_row, |config, argv| {
        open_with_of(config).terminal = argv;
    });
    connect_command_row(&apps, &w.menu_row, |config, argv| {
        prompt_of(config).menu = argv;
    });
    connect_command_row(&apps, &w.prompt_terminal_row, |config, argv| {
        prompt_of(config).terminal = argv;
    });

    let a = apps.clone();
    w.mode_row.connect_selected_notify(move |row| {
        if a.ui.status.settings_suppress.get() {
            return;
        }
        let mode = MODES
            .get(row.selected() as usize)
            .copied()
            .unwrap_or_default();
        show_mode_rows(&a.widgets, mode);
        save(&a, move |config| {
            prompt_of(config).mode = (mode != PromptMode::default()).then_some(mode);
        });
    });
    let a = apps.clone();
    w.limit_row.connect_value_notify(move |row| {
        if a.ui.status.settings_suppress.get() {
            return;
        }
        let limit = row.value().round() as usize;
        save(&a, move |config| {
            prompt_of(config).menu_limit =
                (limit != pdfs_core::menu::DEFAULT_MENU_LIMIT).then_some(limit);
        });
    });
    let a = apps.clone();
    w.icons_row.connect_active_notify(move |row| {
        if a.ui.status.settings_suppress.get() {
            return;
        }
        let on = row.is_active();
        save(&a, move |config| {
            prompt_of(config).menu_icons = (!on).then_some(false)
        });
    });
    let a = apps.clone();
    w.remember_row.connect_active_notify(move |row| {
        if a.ui.status.settings_suppress.get() {
            return;
        }
        let on = row.is_active();
        // The window clears the list itself the next time it opens; clearing
        // here as well means nothing waits on disk until then.
        if !on {
            clear_history(&a);
        }
        save(&a, move |config| {
            prompt_of(config).remember_recent = (!on).then_some(false)
        });
    });
    let a = apps.clone();
    w.clear_button.connect_clicked(move |_| {
        if clear_history(&a) {
            prefs_toast(&a.ui, &gettext("Recent files cleared"));
        }
    });
}

/// Delete the search window's history. It reads the file again each time it
/// opens. Says whether that worked.
fn clear_history(apps: &Rc<Apps>) -> bool {
    match std::fs::remove_file(history_path(&apps.ui)) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            prefs_toast(
                &apps.ui,
                &gettext_f(
                    "{what}: {detail}",
                    &[
                        ("what", &gettext("Couldn't clear recent files")),
                        ("detail", &e.to_string()),
                    ],
                ),
            );
            false
        }
    }
}

/// Where the search window keeps what it opened (`hud/history.rs`).
fn history_path(ui: &Ui) -> std::path::PathBuf {
    ui.dirs
        .state_dir()
        .join(pdfs_core::menu::PROMPT_HISTORY_FILE)
}

/// Show only the rows the chosen launcher mode uses.
fn show_mode_rows(w: &AppsPrefs, mode: PromptMode) {
    w.menu_row.set_visible(mode == PromptMode::Dmenu);
    w.icons_row.set_visible(mode == PromptMode::Dmenu);
    w.prompt_terminal_row.set_visible(mode == PromptMode::Fzf);
    w.limit_row.set_visible(mode != PromptMode::Gtk);
    // Only the search window keeps a history.
    w.remember_row.set_visible(mode == PromptMode::Gtk);
    w.clear_row.set_visible(mode == PromptMode::Gtk);
}

/// The `open_with` section, created when absent.
fn open_with_of(config: &mut pdfs_core::config::AppConfig) -> &mut OpenWith {
    config.open_with.get_or_insert_with(OpenWith::default)
}

/// The `prompt` section, created when absent.
fn prompt_of(config: &mut pdfs_core::config::AppConfig) -> &mut PromptConfig {
    config.prompt.get_or_insert_with(PromptConfig::default)
}

/// Save a change, then drop sections left at their defaults so the file only
/// holds what the user chose. Says whether it saved.
fn save(apps: &Rc<Apps>, change: impl FnOnce(&mut pdfs_core::config::AppConfig)) -> bool {
    let saved = apps.ui.dirs.update_config(|config| {
        change(config);
        if config.open_with.as_ref() == Some(&OpenWith::default()) {
            config.open_with = None;
        }
        if config.prompt.as_ref() == Some(&PromptConfig::default()) {
            config.prompt = None;
        }
        config.resolved_open_with()
    });
    match saved {
        Ok(open_with) => {
            paint_rules(apps, &open_with);
            true
        }
        Err(e) => {
            prefs_toast(
                &apps.ui,
                &gettext_f(
                    "{what}: {detail}",
                    &[
                        ("what", &gettext("Couldn't save the setting")),
                        ("detail", &e.to_string()),
                    ],
                ),
            );
            false
        }
    }
}

/// Check a command line as it is typed, and save it when applied.
fn connect_command_row(
    apps: &Rc<Apps>,
    row: &adw::EntryRow,
    store: impl Fn(&mut pdfs_core::config::AppConfig, Option<Vec<String>>) + 'static,
) {
    row.connect_changed(|row| mark_command(row, false));
    let a = apps.clone();
    row.connect_apply(move |row| {
        let Some(argv) = parse_command(&row.text()) else {
            prefs_toast(&a.ui, &gettext("A quote is not closed in the command"));
            return;
        };
        if !argv.is_empty() && !command_available(&argv) {
            prefs_toast(
                &a.ui,
                // Translators: {program} is a program name such as "nvim".
                &gettext_f(
                    "“{program}” isn't installed; saved anyway",
                    &[("program", &argv[0])],
                ),
            );
        }
        let argv = (!argv.is_empty()).then_some(argv);
        save(&a, |config| store(config, argv));
        mark_command(row, true);
    });
}

/// Mark a command line that doesn't parse as an error, and, once `applied`,
/// one whose program isn't found as a warning.
fn mark_command(row: &adw::EntryRow, applied: bool) {
    row.remove_css_class("error");
    row.remove_css_class("warning");
    match parse_command(&row.text()) {
        None => row.add_css_class("error"),
        Some(argv) if applied && !argv.is_empty() && !command_available(&argv) => {
            row.add_css_class("warning")
        }
        Some(_) => {}
    }
}

/// Show the rules, one row each, in the order they are tried.
fn paint_rules(apps: &Rc<Apps>, open_with: &OpenWith) {
    let group = &apps.widgets.rules;
    for row in apps.rule_rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    let count = open_with.rules.len();
    if count == 0 {
        let row = adw::ActionRow::builder()
            .title(gettext("No rules: every file opens in its default app"))
            .build();
        row.add_css_class("dim-label");
        group.add(&row);
        apps.rule_rows.borrow_mut().push(row);
        return;
    }
    let names = classes();
    for (index, rule) in open_with.rules.iter().enumerate() {
        let patterns: Vec<String> = rule
            .patterns
            .iter()
            .map(|p| {
                names
                    .iter()
                    .find(|(class, _)| class == p)
                    .map_or_else(|| p.clone(), |(_, name)| name.clone())
            })
            .collect();
        let command = format_command(&rule.command);
        let subtitle = if rule.terminal {
            // Translators: {command} is a command line such as "nvim".
            gettext_f("{command}, in a terminal", &[("command", &command)])
        } else {
            command
        };
        let row = adw::ActionRow::builder()
            .title(patterns.join(", "))
            .subtitle(subtitle)
            .use_markup(false)
            .activatable(true)
            .build();
        if !command_available(&rule.command) {
            let warning = gtk4::Image::builder()
                .icon_name("dialog-warning-symbolic")
                // Translators: {program} is a program name such as "nvim".
                .tooltip_text(gettext_f(
                    "“{program}” isn't installed",
                    &[("program", rule.command.first().map_or("", String::as_str))],
                ))
                .build();
            warning.add_css_class("warning");
            row.add_prefix(&warning);
        }
        let up = rule_button("go-up-symbolic", &gettext("Move Up"), index > 0);
        let down = rule_button("go-down-symbolic", &gettext("Move Down"), index + 1 < count);
        let remove = rule_button("user-trash-symbolic", &gettext("Remove"), true);
        let a = apps.clone();
        up.connect_clicked(move |_| {
            save(&a, |config| {
                open_with_of(config).rules.swap(index - 1, index)
            });
        });
        let a = apps.clone();
        down.connect_clicked(move |_| {
            save(&a, |config| {
                open_with_of(config).rules.swap(index, index + 1)
            });
        });
        let a = apps.clone();
        remove.connect_clicked(move |_| {
            save(&a, |config| {
                let rules = &mut open_with_of(config).rules;
                if index < rules.len() {
                    rules.remove(index);
                }
            });
        });
        row.add_suffix(&up);
        row.add_suffix(&down);
        row.add_suffix(&remove);
        let a = apps.clone();
        let rule = rule.clone();
        row.connect_activated(move |_| edit_rule(&a, Some(index), rule.clone()));
        group.add(&row);
        apps.rule_rows.borrow_mut().push(row);
    }
}

fn rule_button(icon: &str, tooltip: &str, sensitive: bool) -> gtk4::Button {
    let button = gtk4::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk4::Align::Center)
        .sensitive(sensitive)
        .build();
    button.add_css_class("flat");
    button
}

/// Split a typed pattern list into patterns, dropping empty ones.
fn split_patterns(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Edit one rule, or a new one when `index` is `None`, in a dialog over the
/// Preferences.
fn edit_rule(apps: &Rc<Apps>, index: Option<usize>, rule: OpenRule) {
    let patterns_row = adw::EntryRow::builder()
        .title(gettext("Files, such as *.md, *.txt"))
        .text(rule.patterns.join(", "))
        .build();
    patterns_row.add_css_class("monospace");
    let chips = gtk4::FlowBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .column_spacing(6)
        .row_spacing(6)
        .max_children_per_line(6)
        .margin_top(6)
        .build();
    for (class, name) in classes() {
        let chip = gtk4::Button::builder().label(&name).build();
        chip.add_css_class("pill");
        chip.add_css_class("small");
        let row = patterns_row.clone();
        chip.connect_clicked(move |_| {
            let mut list = split_patterns(&row.text());
            if !list.iter().any(|p| p == class) {
                list.push(class.to_string());
            }
            row.set_text(&list.join(", "));
        });
        chips.insert(&chip, -1);
    }
    let command_row = adw::EntryRow::builder()
        .title(gettext("Command, such as nvim or code --wait"))
        .text(format_command(&rule.command))
        .build();
    command_row.add_css_class("monospace");
    let terminal_row = adw::SwitchRow::builder()
        .title(gettext("Run in a terminal"))
        .subtitle(gettext("For programs that run in a terminal, such as vim"))
        .active(rule.terminal)
        .build();
    let hint = gtk4::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .margin_top(6)
        .build();
    hint.add_css_class("caption");
    hint.add_css_class("dim-label");

    let group = adw::PreferencesGroup::new();
    group.add(&patterns_row);
    let files = adw::PreferencesGroup::builder()
        .description(gettext("Or pick kinds of files:"))
        .build();
    files.add(&chips);
    let program = adw::PreferencesGroup::new();
    program.add(&command_row);
    program.add(&terminal_row);
    program.add(&hint);
    let page = adw::PreferencesPage::new();
    page.add(&group);
    page.add(&files);
    page.add(&program);

    let cancel = gtk4::Button::with_label(&gettext("Cancel"));
    let save_button = gtk4::Button::with_label(&gettext("Save"));
    save_button.add_css_class("suggested-action");
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save_button);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title(if index.is_some() {
            gettext("Edit Rule")
        } else {
            gettext("New Rule")
        })
        .content_width(480)
        .child(&view)
        .build();

    // Save only a rule that matches something and runs something.
    let check = {
        let (patterns_row, command_row, save_button, hint) = (
            patterns_row.clone(),
            command_row.clone(),
            save_button.clone(),
            hint.clone(),
        );
        move || {
            let patterns = split_patterns(&patterns_row.text());
            let argv = parse_command(&command_row.text());
            command_row.remove_css_class("error");
            hint.set_text("");
            match &argv {
                None => {
                    command_row.add_css_class("error");
                    hint.set_text(&gettext("A quote is not closed in the command."));
                }
                Some(argv) if !argv.is_empty() && !command_available(argv) => {
                    hint.set_text(&gettext_f(
                        "“{program}” isn't installed.",
                        &[("program", &argv[0])],
                    ));
                }
                Some(_) => {}
            }
            save_button
                .set_sensitive(!patterns.is_empty() && argv.is_some_and(|argv| !argv.is_empty()));
        }
    };
    check();
    let c = check.clone();
    patterns_row.connect_changed(move |_| c());
    command_row.connect_changed(move |_| check());

    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });
    let (a, d) = (apps.clone(), dialog.clone());
    save_button.connect_clicked(move |_| {
        let Some(command) = parse_command(&command_row.text()) else {
            return;
        };
        let rule = OpenRule {
            patterns: split_patterns(&patterns_row.text()),
            command,
            terminal: terminal_row.is_active(),
        };
        let saved = save(&a, move |config| {
            let rules = &mut open_with_of(config).rules;
            match index {
                Some(i) if i < rules.len() => rules[i] = rule,
                _ => rules.push(rule),
            }
        });
        if saved {
            d.close();
        }
    });
    dialog.present(Some(&apps.ui.status.prefs));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_patterns_are_split_on_commas() {
        assert_eq!(
            split_patterns(" *.md, @text ,, notes-*.txt "),
            ["*.md", "@text", "notes-*.txt"]
        );
        assert!(split_patterns(" , ").is_empty());
    }
}
