//! The Import page: bringing a Google Photos **Takeout** export into Proton
//! Photos.
//!
//! Reached from the Photos page's import button (and its empty timeline),
//! because it is a one-off migration rather than something the
//! photo timeline does every day. It gets a page of its own instead of a dialog
//! for two reasons: an export is a *set* of archives that has to be staged and
//! checked before anything is sent, and the import that follows runs for hours,
//! so its progress needs somewhere to live that the user can navigate away from
//! and come back to.
//!
//! Archives arrive either by drag-and-drop onto the page or through the file
//! chooser; both funnel into [`add_archives`], which is where the `.zip` filter
//! and de-duplication live. Nothing is sent until the user presses Scan or
//! Import — staging is local, so a mis-drop costs nothing.
//!
//! The daemon owns the run itself ([`Request::ImportTakeout`]): this page only
//! stages paths, launches, polls [`Request::ImportStatus`] for the final report,
//! and reads live progress off the import's job in the transfer snapshot the
//! status tick already fetches (see [`takeout_progress`]).

use crate::*;

/// The title the daemon gives the import's job in [`Request::GetQueueStatus`].
/// Matching on it is how this page borrows the progress the status tick is
/// already polling instead of adding a second poll of its own.
const IMPORT_JOB: &str = "Importing Google Photos";

/// What the archive list says before anything is staged. `repaint_archives`
/// swaps in the count and total once there is something to report, and swaps
/// this back when the list is emptied again.
/// Where the staged set is remembered between runs, under the app's state
/// directory. Staging an export means picking dozens of files out of a file
/// chooser; losing that to a window close (or a crash mid-import) and having to
/// redo it is the kind of small cruelty that makes a migration not get finished.
fn staged_path(ui: &Rc<Ui>) -> PathBuf {
    ui.dirs.state_dir().join("takeout-staged.json")
}

/// Read the remembered set, dropping any archive that has since been moved or
/// deleted — a stale path would only fail at launch, and silently is worse.
fn load_staged(ui: &Rc<Ui>) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(staged_path(ui)) else {
        return Vec::new();
    };
    let paths: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
    paths
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .collect()
}

/// Remember the staged set. Best-effort: failing to write it costs the user a
/// re-pick next time, which is not worth interrupting them over now.
fn save_staged(ui: &Rc<Ui>) {
    let paths: Vec<String> = ui
        .takeout
        .archives
        .borrow()
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let path = staged_path(ui);
    let written = serde_json::to_string(&paths)
        .map_err(|e| e.to_string())
        .and_then(|text| std::fs::write(&path, text).map_err(|e| e.to_string()));
    if let Err(e) = written {
        tracing::warn!("cannot remember staged Takeout archives: {e}");
    }
}

const EMPTY_LIST_DESCRIPTION: &str = "Photos already in Proton Photos are matched by name and content and skipped, so \
     re-running an interrupted import is safe.";

pub(crate) struct TakeoutState {
    /// Archives staged for this import, in the order they were added. Sent as
    /// one set: Google splits an export across numbered zips and routinely puts
    /// a photo in one part and its metadata sidecar in another, so a per-file
    /// import would lose album titles and capture times.
    pub(crate) archives: RefCell<Vec<PathBuf>>,
    /// The staged-archive list and the rows currently rendered in it.
    pub(crate) list_group: adw::PreferencesGroup,
    pub(crate) rows: RefCell<Vec<adw::PreferencesRow>>,
    /// The drop zone, whose `dropzone-active` class follows the drag.
    pub(crate) dropzone: gtk4::Box,
    pub(crate) scan_button: gtk4::Button,
    pub(crate) import_button: gtk4::Button,
    pub(crate) cancel_button: gtk4::Button,
    pub(crate) clear_button: gtk4::Button,
    /// Progress group, hidden unless an import is running.
    pub(crate) progress_group: adw::PreferencesGroup,
    pub(crate) progress_label: gtk4::Label,
    pub(crate) progress_bar: gtk4::ProgressBar,
    /// Report group for the last finished run, hidden until there is one.
    pub(crate) summary_group: adw::PreferencesGroup,
    pub(crate) summary_rows: RefCell<Vec<adw::ActionRow>>,
    /// One [`Request::ImportStatus`] in flight at a time, so a wedged daemon
    /// can't stack worker threads on the 2s tick.
    pub(crate) inflight: Cell<bool>,
    /// Whether the daemon reported an import running on the last poll.
    pub(crate) running: Cell<bool>,
    /// Whether the run in flight was launched as a dry run, so the report can
    /// say "would upload" rather than claiming photos were sent. The daemon's
    /// summary carries no such flag — it is the same shape either way.
    pub(crate) dry_run: Cell<bool>,
}

/// Widgets the Import page hands back for wiring.
pub(crate) struct TakeoutWidgets {
    pub(crate) list_group: adw::PreferencesGroup,
    pub(crate) dropzone: gtk4::Box,
    /// The whole page, which is the drop target: a drag released anywhere on it
    /// counts, not only inside the dashed rectangle.
    pub(crate) page: gtk4::Widget,
    pub(crate) choose_button: gtk4::Button,
    pub(crate) scan_button: gtk4::Button,
    pub(crate) import_button: gtk4::Button,
    pub(crate) cancel_button: gtk4::Button,
    pub(crate) clear_button: gtk4::Button,
    pub(crate) back_button: gtk4::Button,
    pub(crate) progress_group: adw::PreferencesGroup,
    pub(crate) progress_label: gtk4::Label,
    pub(crate) progress_bar: gtk4::ProgressBar,
    pub(crate) summary_group: adw::PreferencesGroup,
}

pub(crate) fn build_takeout_page() -> (gtk4::Widget, TakeoutWidgets) {
    // Header: a back button out to Photos, since this page is reached from
    // there and has no sidebar row of its own to navigate back with.
    let back_button = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back to Photos")
        .valign(gtk4::Align::Center)
        .build();
    back_button.add_css_class("flat");

    let subtitle = gtk4::Label::builder()
        .label("Add a Google Takeout export to your Proton Photos timeline.")
        .halign(gtk4::Align::Start)
        .wrap(true)
        .xalign(0.0)
        .build();
    subtitle.add_css_class("dim-label");

    let subtitle_clamp = adw::Clamp::builder().child(&subtitle).build();

    // Drop zone. It is a hint, not the only way in — the button under it does
    // the same thing for anyone not dragging, and the drop target covers the
    // whole page so a drag released just outside the rectangle still lands.
    let drop_icon = gtk4::Image::from_icon_name("folder-download-symbolic");
    drop_icon.set_pixel_size(48);
    drop_icon.add_css_class("dim-label");
    let drop_title = gtk4::Label::builder()
        .label("Drop your Takeout .zip files here")
        .build();
    drop_title.add_css_class("title-4");
    let drop_hint = gtk4::Label::builder()
        .label("Add every part of the export at once — a photo and its metadata often sit in different parts.")
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .max_width_chars(48)
        .build();
    drop_hint.add_css_class("dim-label");
    let choose_button = gtk4::Button::builder()
        .label("Choose files…")
        .halign(gtk4::Align::Center)
        .build();
    choose_button.add_css_class("pill");

    let dropzone = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    dropzone.set_halign(gtk4::Align::Fill);
    dropzone.add_css_class("dropzone");
    dropzone.append(&drop_icon);
    dropzone.append(&drop_title);
    dropzone.append(&drop_hint);
    dropzone.append(&choose_button);

    // Staged archives. Filled by `repaint_archives`, which owns the description
    // too (it becomes the count + total once there is something to count); the
    // group carries a placeholder row while empty so it never renders as a bare
    // title.
    let list_group = adw::PreferencesGroup::builder()
        .title("Archives to import")
        .description(EMPTY_LIST_DESCRIPTION)
        .build();

    let clear_button = gtk4::Button::builder()
        .label("Clear")
        .tooltip_text("Remove every staged archive")
        .build();
    clear_button.add_css_class("flat");

    // Actions. Scan is a dry run: it reads the archives' directories and reports
    // what an import *would* do, without uploading a byte. It is offered first
    // because the import behind it is hours long and hard to take back.
    let scan_button = gtk4::Button::builder()
        .label("Scan")
        .tooltip_text("Check the archives and report what would be imported, without uploading")
        .build();
    scan_button.add_css_class("pill");
    let import_button = gtk4::Button::builder().label("Import").build();
    import_button.add_css_class("pill");
    import_button.add_css_class("suggested-action");
    let cancel_button = gtk4::Button::builder()
        .label("Stop import")
        .tooltip_text("Finish the photo on the wire, file what was uploaded, then stop")
        .build();
    cancel_button.add_css_class("pill");
    cancel_button.add_css_class("destructive-action");
    cancel_button.set_visible(false);

    // Clear sits apart from the launch buttons, on the other end of the row: it
    // undoes staging, and putting it next to Import invites the wrong click.
    let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&clear_button);
    actions.append(&spacer);
    actions.append(&cancel_button);
    actions.append(&scan_button);
    actions.append(&import_button);

    // Progress: the import's job from the transfer snapshot, so this page shows
    // the same numbers the Sync page's Transfers list does.
    let progress_group = adw::PreferencesGroup::builder()
        .title("Import in progress")
        .visible(false)
        .build();
    let progress_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    progress_box.set_margin_top(8);
    progress_box.set_margin_bottom(8);
    let progress_label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
    progress_label.add_css_class("dim-label");
    let progress_bar = gtk4::ProgressBar::new();
    progress_box.append(&progress_label);
    progress_box.append(&progress_bar);
    progress_group.add(
        &adw::PreferencesRow::builder()
            .activatable(false)
            .child(&progress_box)
            .build(),
    );

    // Report for the last finished run, kept on screen after it ends: an import
    // outlives the window being on this page, and "what happened" is the whole
    // reason to come back to it.
    let summary_group = adw::PreferencesGroup::builder()
        .title("Last import")
        .visible(false)
        .build();

    let groups = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    groups.append(&dropzone);
    groups.append(&list_group);
    groups.append(&actions);
    groups.append(&progress_group);
    groups.append(&summary_group);
    let clamp = adw::Clamp::builder()
        .maximum_size(640)
        .child(&groups)
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&clamp)
        .build();

    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    page.set_margin_top(18);
    page.set_margin_bottom(18);
    page.set_margin_start(18);
    page.set_margin_end(18);
    page.append(&subtitle_clamp);
    page.append(&scroll);
    let (frame, header, _) = page_frame("Import from Google Photos", &page);
    header.pack_start(&back_button);

    let widgets = TakeoutWidgets {
        list_group: list_group.clone(),
        dropzone: dropzone.clone(),
        page: page.clone().upcast(),
        choose_button,
        scan_button: scan_button.clone(),
        import_button: import_button.clone(),
        cancel_button: cancel_button.clone(),
        clear_button,
        back_button,
        progress_group: progress_group.clone(),
        progress_label: progress_label.clone(),
        progress_bar: progress_bar.clone(),
        summary_group: summary_group.clone(),
    };
    (frame.upcast(), widgets)
}

pub(crate) fn wire_takeout(ui: &Rc<Ui>, widgets: &TakeoutWidgets) {
    // Drag-and-drop. The target sits on the whole page rather than the dashed
    // rectangle, so a drag released anywhere counts; the rectangle only lights
    // up to say where the drop is understood.
    let target = gtk4::DropTarget::new(
        gtk4::gdk::FileList::static_type(),
        gtk4::gdk::DragAction::COPY,
    );
    let ui_enter = ui.clone();
    target.connect_enter(move |_, _, _| {
        ui_enter.takeout.dropzone.add_css_class("dropzone-active");
        gtk4::gdk::DragAction::COPY
    });
    let ui_leave = ui.clone();
    target.connect_leave(move |_| {
        ui_leave
            .takeout
            .dropzone
            .remove_css_class("dropzone-active");
    });
    let ui_drop = ui.clone();
    target.connect_drop(move |_, value, _, _| {
        ui_drop.takeout.dropzone.remove_css_class("dropzone-active");
        let Ok(files) = value.get::<gtk4::gdk::FileList>() else {
            return false;
        };
        let paths: Vec<PathBuf> = files.files().iter().filter_map(|f| f.path()).collect();
        add_archives(&ui_drop, paths);
        true
    });
    widgets.page.add_controller(target);

    let ui_choose = ui.clone();
    widgets.choose_button.connect_clicked(move |_| {
        // Multi-select on purpose: the whole export goes in one request.
        let dialog = gtk4::FileDialog::builder()
            .title("Select your Google Takeout archives")
            .build();
        let filter = gtk4::FileFilter::new();
        filter.set_name(Some("Takeout archives"));
        filter.add_pattern("*.zip");
        let filters = gio::ListStore::new::<gtk4::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));

        let ui = ui_choose.clone();
        dialog.open_multiple(
            ui_window(&ui_choose).as_ref(),
            gio::Cancellable::NONE,
            move |res| {
                let Ok(files) = res else { return };
                let paths: Vec<PathBuf> = (0..files.n_items())
                    .filter_map(|index| files.item(index))
                    .filter_map(|object| object.downcast::<gio::File>().ok())
                    .filter_map(|file| file.path())
                    .collect();
                add_archives(&ui, paths);
            },
        );
    });

    let ui_clear = ui.clone();
    widgets.clear_button.connect_clicked(move |_| {
        ui_clear.takeout.archives.borrow_mut().clear();
        repaint_archives(&ui_clear);
    });

    let ui_scan = ui.clone();
    widgets
        .scan_button
        .connect_clicked(move |_| start_import(&ui_scan, true));

    // The import itself is confirmed: it uploads a whole photo library, which is
    // hours of bandwidth and a large change to the account's timeline.
    let ui_import = ui.clone();
    widgets.import_button.connect_clicked(move |_| {
        let count = ui_import.takeout.archives.borrow().len();
        let ui = ui_import.clone();
        let dialog = adw::AlertDialog::builder()
            .heading("Import this export?")
            .body(format!(
                "{} will be uploaded to Proton Photos, creating albums as they appear in \
                 the export. This can take hours; you can stop it at any point and what has \
                 been uploaded stays. Photos you already have are skipped.",
                count_noun(count, "archive", "archives")
            ))
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("import", "Import");
        dialog.set_response_appearance("import", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |_, resp| {
            if resp == "import" {
                start_import(&ui, false);
            }
        });
        dialog.present(ui_window(&ui_import).as_ref());
    });

    let ui_cancel = ui.clone();
    widgets.cancel_button.connect_clicked(move |_| {
        let ui = ui_cancel.clone();
        ui.busy_begin();
        let rx = spawn_request(ui.dirs.control_socket(), Request::CancelImport);
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            ui.busy_end();
            match result {
                Ok(Ok(Response::Ok { .. })) => toast(&ui, "Stopping the import…"),
                Ok(Ok(Response::Error { message, kind })) => {
                    toast_failure(&ui, "Couldn't stop the import", &message, kind);
                }
                _ => toast_error(
                    &ui,
                    "Couldn't stop the import",
                    "The mount service didn't respond.",
                ),
            }
        });
    });

    let ui_back = ui.clone();
    widgets
        .back_button
        .connect_clicked(move |_| ui_back.stack.set_visible_child_name("gallery"));

    // Restore whatever was staged when the window last closed, before the first
    // paint, so the page comes up as the user left it.
    *ui.takeout.archives.borrow_mut() = load_staged(ui);
    repaint_archives(ui);
}

/// Stage `paths`, keeping only `.zip` files and skipping ones already staged.
///
/// Both the chooser and the drop target land here, so the filter is stated once.
/// A dropped folder (an already-extracted export) is rejected rather than walked:
/// the scan reads the archives' central directories, and an extracted tree has
/// none — silently importing half of it would be worse than saying no.
pub(crate) fn add_archives(ui: &Rc<Ui>, paths: Vec<PathBuf>) {
    let mut added = 0usize;
    let mut rejected = 0usize;
    {
        let mut staged = ui.takeout.archives.borrow_mut();
        for path in paths {
            let is_zip = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
            if !is_zip || !path.is_file() {
                rejected += 1;
                continue;
            }
            if staged.contains(&path) {
                continue;
            }
            staged.push(path);
            added += 1;
        }
        // Numbered parts drop in whatever order the file manager hands them
        // over; sorting keeps the list readable and the request stable.
        staged.sort();
    }
    repaint_archives(ui);
    if rejected > 0 {
        toast(
            ui,
            "Only Takeout .zip files can be imported — extracted folders aren't supported.",
        );
    } else if added > 0 {
        toast(
            ui,
            &format!("Added {}", count_noun(added, "archive", "archives")),
        );
    }
}

/// Rebuild the staged-archive list and re-derive which actions are available.
pub(crate) fn repaint_archives(ui: &Rc<Ui>) {
    for row in ui.takeout.rows.borrow_mut().drain(..) {
        ui.takeout.list_group.remove(&row);
    }

    let staged = ui.takeout.archives.borrow().clone();
    let mut rows: Vec<adw::PreferencesRow> = Vec::new();
    if staged.is_empty() {
        ui.takeout
            .list_group
            .set_description(Some(EMPTY_LIST_DESCRIPTION));
        let row = dim_row("No archives added yet.");
        ui.takeout.list_group.add(&row);
        rows.push(row.upcast());
    } else {
        let mut total = 0u64;
        for path in &staged {
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            total += size;
            let row = adw::ActionRow::builder()
                .title(
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string()),
                )
                .subtitle(human_bytes(size))
                .build();
            let remove = gtk4::Button::builder()
                .icon_name("edit-delete-symbolic")
                .tooltip_text("Remove from this import")
                .valign(gtk4::Align::Center)
                .build();
            remove.add_css_class("flat");
            let ui_remove = ui.clone();
            let target = path.clone();
            remove.connect_clicked(move |_| {
                ui_remove
                    .takeout
                    .archives
                    .borrow_mut()
                    .retain(|p| *p != target);
                repaint_archives(&ui_remove);
            });
            row.add_suffix(&remove);
            ui.takeout.list_group.add(&row);
            rows.push(row.upcast());
        }
        ui.takeout.list_group.set_description(Some(&format!(
            "{}, {} in total. {EMPTY_LIST_DESCRIPTION}",
            count_noun(staged.len(), "archive", "archives"),
            human_bytes(total)
        )));
    }
    *ui.takeout.rows.borrow_mut() = rows;

    save_staged(ui);
    sync_takeout_actions(ui);
}

/// Enable exactly the actions that make sense right now: nothing to launch
/// without archives, nothing to launch while a run is in flight, and Stop only
/// while there is something to stop.
fn sync_takeout_actions(ui: &Rc<Ui>) {
    let has_files = !ui.takeout.archives.borrow().is_empty();
    let running = ui.takeout.running.get();
    ui.takeout.scan_button.set_sensitive(has_files && !running);
    ui.takeout
        .import_button
        .set_sensitive(has_files && !running);
    ui.takeout.clear_button.set_sensitive(has_files && !running);
    ui.takeout.cancel_button.set_visible(running);
}

/// Hand the staged set to the daemon. It acks immediately and works in the
/// background — an export is far past any socket timeout — so success here means
/// "started", and the page switches over to reporting progress.
fn start_import(ui: &Rc<Ui>, dry_run: bool) {
    let archives: Vec<String> = ui
        .takeout
        .archives
        .borrow()
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    if archives.is_empty() {
        return;
    }

    ui.takeout.dry_run.set(dry_run);
    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ImportTakeout { archives, dry_run },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => {
                ui.takeout.running.set(true);
                ui.takeout.summary_group.set_visible(false);
                ui.takeout.progress_group.set_visible(true);
                ui.takeout.progress_bar.set_fraction(0.0);
                ui.takeout.progress_label.set_text(if dry_run {
                    "Reading archives…"
                } else {
                    "Starting import…"
                });
                sync_takeout_actions(&ui);
                toast(
                    &ui,
                    if dry_run {
                        "Scanning the export…"
                    } else {
                        "Import started — you can leave this page, it keeps running."
                    },
                );
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't start the import", &message, kind);
            }
            _ => toast_error(
                &ui,
                "Couldn't start the import",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Poll the daemon for the import's state while the page is on screen, and paint
/// the report once a run has finished. Cheap and inflight-guarded, like the other
/// tick polls.
pub(crate) fn refresh_takeout(ui: &Rc<Ui>) {
    if ui.takeout.inflight.get() {
        return;
    }
    ui.takeout.inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::ImportStatus);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.takeout.inflight.set(false);
        let Ok(Ok(Response::ImportStatus { running, summary })) = result else {
            return;
        };
        let was_running = ui.takeout.running.replace(running);
        if running {
            ui.takeout.progress_group.set_visible(true);
        } else {
            ui.takeout.progress_group.set_visible(false);
            // An import runs for hours, so its ending is announced rather than
            // left to be discovered: a toast for whoever is looking at the
            // window, and a desktop notification for whoever is not.
            if was_running {
                let dry = ui.takeout.dry_run.get();
                let headline = if dry {
                    "Scan finished"
                } else {
                    "Import finished"
                };
                toast(&ui, headline);
                if !dry {
                    let body = match &summary {
                        Some(s) if s.failed > 0 => {
                            format!("{} photos added, {} failed.", s.uploaded, s.failed)
                        }
                        Some(s) if s.cancelled => {
                            format!("Stopped after {} photos.", s.uploaded)
                        }
                        Some(s) => format!("{} photos added to Proton Photos.", s.uploaded),
                        None => "The Google Photos import has finished.".to_string(),
                    };
                    notify("takeout-import", headline, &body);
                }
            }
        }
        if was_running != running {
            sync_takeout_actions(&ui);
        }
        match summary {
            Some(summary) => repaint_summary(&ui, &summary),
            None => ui.takeout.summary_group.set_visible(false),
        }
    });
}

/// Paint the last run's report.
fn repaint_summary(ui: &Rc<Ui>, summary: &ImportSummary) {
    for row in ui.takeout.summary_rows.borrow_mut().drain(..) {
        ui.takeout.summary_group.remove(&row);
    }
    let dry = ui.takeout.dry_run.get();
    ui.takeout
        .summary_group
        .set_title(if dry { "Scan result" } else { "Last import" });
    ui.takeout.summary_group.set_description(Some(if dry {
        "Nothing was uploaded — this is what an import would do."
    } else if summary.cancelled {
        "Stopped early. Everything uploaded so far is in your timeline; run it again to \
         continue where it left off."
    } else {
        "Finished."
    }));

    // Only lines that carry information: a run with no failures should not have
    // to show a "0 failed" row for the user to read past.
    let mut lines: Vec<(String, String)> = vec![
        ("Photos in the export".into(), summary.found.to_string()),
        (
            if dry { "Would upload" } else { "Uploaded" }.into(),
            summary.uploaded.to_string(),
        ),
    ];
    if summary.duplicates > 0 {
        lines.push((
            "Already in Proton Photos".into(),
            summary.duplicates.to_string(),
        ));
    }
    if summary.albums_created > 0 {
        lines.push((
            if dry {
                "Albums to create"
            } else {
                "Albums created"
            }
            .into(),
            summary.albums_created.to_string(),
        ));
    }
    if summary.album_links > 0 {
        lines.push((
            "Photos filed into albums".into(),
            summary.album_links.to_string(),
        ));
    }
    if summary.skipped_trashed > 0 {
        lines.push((
            "Skipped (in Google's trash)".into(),
            summary.skipped_trashed.to_string(),
        ));
    }
    if summary.bytes > 0 {
        lines.push((
            if dry {
                "Data to upload"
            } else {
                "Data uploaded"
            }
            .into(),
            human_bytes(summary.bytes),
        ));
    }
    if summary.failed > 0 {
        lines.push(("Failed".into(), summary.failed.to_string()));
    }

    let mut rows = Vec::new();
    for (title, value) in lines {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(value)
            .build();
        row.add_css_class("property");
        ui.takeout.summary_group.add(&row);
        rows.push(row);
    }
    *ui.takeout.summary_rows.borrow_mut() = rows;
    ui.takeout.summary_group.set_visible(true);
}

/// Paint the import's progress from the transfer snapshot the status tick
/// already fetched, so this page costs no extra poll. A job with no known total
/// pulses instead of showing a fraction, the same as the Activity list.
pub(crate) fn takeout_progress(ui: &Rc<Ui>, jobs: &[JobItem]) {
    let Some(job) = jobs.iter().find(|job| job.title == IMPORT_JOB) else {
        return;
    };
    ui.takeout.progress_group.set_visible(true);
    if job.total > 0 {
        ui.takeout
            .progress_bar
            .set_fraction((job.done as f64 / job.total as f64).min(1.0));
        ui.takeout
            .progress_label
            .set_text(&format!("{} of {} — {}", job.done, job.total, job.detail));
    } else {
        ui.takeout.progress_bar.pulse();
        ui.takeout
            .progress_label
            .set_text(if job.detail.is_empty() {
                "Working…"
            } else {
                &job.detail
            });
    }
}
