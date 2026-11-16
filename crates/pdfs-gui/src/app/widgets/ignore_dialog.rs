//! The Excluded Files dialog of one synced folder: its own `.pdfsignore`,
//! edited as text, with a check of what the rules do to a path.
//!
//! The file sits in the synced folder and syncs like any other file, so it can
//! change on another computer while it is open here. Saving checks for that
//! and asks, rather than writing over the other edit.

use crate::*;
use pdfs_core::syncignore::{FolderRules, IgnoreRules, RuleSource, Verdict, WriteRulesError};

/// Open the dialog for the synced folder `id` at `root`.
pub(crate) fn present_ignore_dialog(ui: &Rc<Ui>, id: i64, root: PathBuf) {
    let root_display = tilde_path(&root.display().to_string());
    let intro = gtk4::Label::builder()
        // Translators: {folder} is a local folder path such as ~/Projects.
        .label(gettext_f(
            "Patterns for {folder} only, one per line, on top of the ones for all synced folders. Lines starting with # are comments.",
            &[("folder", &root_display)],
        ))
        .wrap(true)
        .xalign(0.0)
        .build();
    let text = gtk4::TextView::builder()
        .monospace(true)
        .wrap_mode(gtk4::WrapMode::WordChar)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .sensitive(false)
        .build();
    let scroller = gtk4::ScrolledWindow::builder()
        .child(&text)
        .min_content_height(180)
        .vexpand(true)
        .build();
    scroller.add_css_class("card");
    let problems = gtk4::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .build();
    problems.add_css_class("error");

    let check_row = adw::EntryRow::builder()
        .title(gettext("Check a path in the folder"))
        .build();
    let verdict = gtk4::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .margin_top(6)
        .build();
    verdict.add_css_class("dim-label");
    let check_group = adw::PreferencesGroup::new();
    check_group.add(&check_row);

    let all_folders = gtk4::Button::builder()
        .label(gettext("Patterns for All Folders…"))
        .halign(gtk4::Align::Start)
        .build();
    all_folders.add_css_class("flat");

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.append(&intro);
    body.append(&scroller);
    body.append(&problems);
    body.append(&check_group);
    body.append(&verdict);
    body.append(&all_folders);

    let cancel = gtk4::Button::with_label(&gettext("Cancel"));
    let save = gtk4::Button::with_label(&gettext("Save"));
    save.add_css_class("suggested-action");
    save.set_sensitive(false);
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&body));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));
    let dialog = adw::Dialog::builder()
        .title(gettext("Excluded Files"))
        .content_width(560)
        .content_height(560)
        .child(&view)
        .build();

    // What was read, so the save can tell whether the file changed since.
    let loaded: Rc<RefCell<Option<FolderRules>>> = Rc::default();
    let globals = Rc::new(ui.dirs.load_config().resolved_ignore_patterns());

    let show_verdict = {
        let (text, check_row, verdict, root, globals) = (
            text.clone(),
            check_row.clone(),
            verdict.clone(),
            root.clone(),
            globals.clone(),
        );
        move || {
            let typed = check_row.text();
            let typed = typed.trim();
            if typed.is_empty() {
                verdict.set_text("");
                return;
            }
            let path = Path::new(typed);
            let rel = path.strip_prefix(&root).unwrap_or(path);
            let rel = rel.to_string_lossy();
            let rel = rel.trim_start_matches('/');
            let is_dir = typed.ends_with('/') || root.join(rel).is_dir();
            let rules = IgnoreRules::with_folder_text(&root, &globals, &buffer_text(&text));
            verdict.set_text(&verdict_text(&rules.explain(rel, is_dir)));
        }
    };
    let on_text = {
        let (problems, save, show_verdict) = (problems.clone(), save.clone(), show_verdict.clone());
        move |text: &str| {
            let bad = pdfs_core::syncignore::check_rules(text);
            problems.set_visible(!bad.is_empty());
            if let Some((line, _)) = bad.first() {
                problems.set_text(&ngettext_f(
                    "Line {line} isn't a valid pattern.",
                    "Line {line} and {more} more aren't valid patterns.",
                    bad.len() as u64,
                    &[
                        ("line", &line.to_string()),
                        ("more", &(bad.len() - 1).to_string()),
                    ],
                ));
            }
            save.set_sensitive(bad.is_empty());
            show_verdict();
        }
    };
    let on_text_changed = on_text.clone();
    text.buffer()
        .connect_changed(move |buffer| on_text_changed(&buffer_text_of(buffer)));
    check_row.connect_changed(move |_| show_verdict());

    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });
    let (ui_all, d) = (ui.clone(), dialog.clone());
    all_folders.connect_clicked(move |_| {
        d.close();
        let prefs = &ui_all.status.prefs;
        prefs.set_visible_page_name("sync");
        prefs.present(ui_window(&ui_all).as_ref());
    });

    // Read off the main thread: the folder may be on a slow disk.
    let reader = {
        let (text, loaded, root, toasts) =
            (text.clone(), loaded.clone(), root.clone(), toasts.clone());
        move || {
            let (text, loaded, root, toasts) =
                (text.clone(), loaded.clone(), root.clone(), toasts.clone());
            glib::spawn_future_local(async move {
                let read = gio::spawn_blocking(move || FolderRules::read(&root))
                    .await
                    .unwrap_or_else(|_| Err(std::io::Error::other("the read did not finish")));
                match read {
                    Ok(rules) => {
                        text.buffer().set_text(rules.text.as_deref().unwrap_or(""));
                        text.set_sensitive(true);
                        *loaded.borrow_mut() = Some(rules);
                    }
                    Err(e) => dialog_toast(
                        &toasts,
                        &gettext_f(
                            "{what}: {detail}",
                            &[
                                ("what", &gettext("Couldn't read the patterns")),
                                ("detail", &e.to_string()),
                            ],
                        ),
                    ),
                }
            });
        }
    };
    reader();

    let (ui_save, d) = (ui.clone(), dialog.clone());
    save.connect_clicked(move |_| {
        let Some(rules) = loaded.borrow().clone() else {
            return;
        };
        write_rules(
            &ui_save,
            &d,
            &toasts,
            id,
            rules,
            buffer_text(&text),
            reader.clone(),
        );
    });

    dialog.present(ui_window(ui).as_ref());
}

/// Save `body` over `rules`, then close and sync the folder. When the file
/// changed since it was read, ask whether to load the other version or to
/// replace it; `reload` reads the file into the dialog again.
fn write_rules(
    ui: &Rc<Ui>,
    dialog: &adw::Dialog,
    toasts: &adw::ToastOverlay,
    id: i64,
    rules: FolderRules,
    body: String,
    reload: impl Fn() + Clone + 'static,
) {
    let (ui, dialog, toasts) = (ui.clone(), dialog.clone(), toasts.clone());
    glib::spawn_future_local(async move {
        let to_write = rules.clone();
        let text = body.clone();
        let written = gio::spawn_blocking(move || to_write.write(&text))
            .await
            .unwrap_or_else(|_| {
                Err(WriteRulesError::Io(std::io::Error::other(
                    "the write did not finish",
                )))
            });
        match written {
            Ok(()) => {
                dialog.close();
                toast(&ui, &gettext("Excluded files saved"));
                refresh_exclusions(&ui);
                let _ = spawn_request(ui.dirs.control_socket(), Request::SyncNow { id: Some(id) });
            }
            Err(WriteRulesError::Changed) => {
                let ask = adw::AlertDialog::builder()
                    .heading(gettext("Patterns Changed Elsewhere"))
                    .body(gettext(
                        "The patterns of this folder changed while you edited them, for example on another computer. Load the other version, or replace it with yours?",
                    ))
                    .build();
                ask.add_response("cancel", &gettext("Cancel"));
                ask.add_response("reload", &gettext("Load Other Version"));
                ask.add_response("replace", &gettext("Replace"));
                ask.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
                ask.set_default_response(Some("cancel"));
                ask.set_close_response("cancel");
                let (ui, d, t) = (ui.clone(), dialog.clone(), toasts.clone());
                ask.connect_response(None, move |_, response| match response {
                    "reload" => reload(),
                    "replace" => {
                        // Read the other version only as the base to check
                        // against, then write over it.
                        let (ui, d, t, body, reload) = (
                            ui.clone(),
                            d.clone(),
                            t.clone(),
                            body.clone(),
                            reload.clone(),
                        );
                        let root = rules.path.parent().map(Path::to_path_buf);
                        glib::spawn_future_local(async move {
                            let Some(root) = root else { return };
                            let read = gio::spawn_blocking(move || FolderRules::read(&root))
                                .await
                                .ok()
                                .and_then(Result::ok);
                            if let Some(current) = read {
                                write_rules(&ui, &d, &t, id, current, body, reload);
                            }
                        });
                    }
                    _ => {}
                });
                ask.present(Some(&dialog));
            }
            Err(WriteRulesError::Io(e)) => dialog_toast(
                &toasts,
                &gettext_f(
                    "{what}: {detail}",
                    &[
                        ("what", &gettext("Couldn't save the patterns")),
                        ("detail", &e.to_string()),
                    ],
                ),
            ),
        }
    });
}

/// Say what the rules do to the path being checked.
fn verdict_text(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Synced => gettext("Synced: no pattern matches it."),
        // Translators: {pattern} is an ignore pattern such as *.log.
        Verdict::Excluded {
            pattern,
            source: RuleSource::ThisFolder,
        } => gettext_f(
            "Not synced: excluded by “{pattern}” above.",
            &[("pattern", pattern)],
        ),
        Verdict::Excluded {
            pattern,
            source: RuleSource::AllFolders,
        } => gettext_f(
            "Not synced: excluded by “{pattern}” in the patterns for all folders.",
            &[("pattern", pattern)],
        ),
        Verdict::Included { pattern, .. } => gettext_f(
            "Synced: “{pattern}” includes it again.",
            &[("pattern", pattern)],
        ),
    }
}

fn buffer_text(view: &gtk4::TextView) -> String {
    buffer_text_of(&view.buffer())
}

fn buffer_text_of(buffer: &gtk4::TextBuffer) -> String {
    let (start, end) = buffer.bounds();
    buffer.text(&start, &end, false).to_string()
}

fn dialog_toast(toasts: &adw::ToastOverlay, message: &str) {
    toasts.add_toast(
        adw::Toast::builder()
            .title(message)
            .use_markup(false)
            .timeout(6)
            .build(),
    );
}
