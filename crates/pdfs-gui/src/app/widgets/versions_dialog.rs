//! The per-file **Versions** dialog: what Proton Drive still holds of a file's
//! earlier content, and the three things a user can do with one.
//!
//! Every row is one revision the server kept. Restoring re-points the file at an
//! old revision (server-side, so nothing is uploaded and the mount may take a
//! moment to catch up); saving writes an old revision out to a local file
//! without touching the live one; deleting is permanent, and the active revision
//! cannot be deleted at all — that row simply has no delete button.

use crate::*;

use pdfs_core::control::RevisionInfo;

/// How an open Versions dialog addresses its file — the same split as
/// [`ShareTarget`](super::share_dialog::ShareTarget), for the same reason: a
/// node reached from Shared or a device folder may have no path in the primary
/// mount's namespace, and an older daemon must reject the request rather than
/// resolve an empty path to the mount root.
pub(crate) enum VersionTarget {
    Path(String),
    Uid(String),
}

impl VersionTarget {
    fn list(&self) -> Request {
        match self {
            VersionTarget::Path(path) => Request::ListRevisions { path: path.clone() },
            VersionTarget::Uid(uid) => Request::ListRevisionsByUid { uid: uid.clone() },
        }
    }

    fn restore(&self, revision_id: String) -> Request {
        match self {
            VersionTarget::Path(path) => Request::RestoreRevision {
                path: path.clone(),
                revision_id,
            },
            VersionTarget::Uid(uid) => Request::RestoreRevisionByUid {
                uid: uid.clone(),
                revision_id,
            },
        }
    }

    fn delete(&self, revision_id: String) -> Request {
        match self {
            VersionTarget::Path(path) => Request::DeleteRevision {
                path: path.clone(),
                revision_id,
            },
            VersionTarget::Uid(uid) => Request::DeleteRevisionByUid {
                uid: uid.clone(),
                revision_id,
            },
        }
    }

    fn save_as(&self, revision_id: String, dest: String) -> Request {
        match self {
            VersionTarget::Path(path) => Request::SaveRevisionAs {
                path: path.clone(),
                revision_id,
                dest,
            },
            VersionTarget::Uid(uid) => Request::SaveRevisionAsByUid {
                uid: uid.clone(),
                revision_id,
                dest,
            },
        }
    }
}

/// The state behind an open Versions dialog, so an action can rebuild the list
/// in place instead of tearing the dialog down.
pub(crate) struct VersionsDialog {
    pub(crate) ui: Rc<Ui>,
    pub(crate) target: VersionTarget,
    /// The file's name, for the save dialog's suggested filename and for toasts.
    pub(crate) name: String,
    pub(crate) group: adw::PreferencesGroup,
    pub(crate) rows: RefCell<Vec<gtk4::Widget>>,
}

/// Open the Versions dialog for a file.
pub(crate) fn open_versions_dialog(ui: &Rc<Ui>, entry: &DirEntry) {
    if !*ui.mounted.borrow() {
        toast_error(
            ui,
            &gettext("Can't load versions"),
            &gettext("Proton Drive isn't connected."),
        );
        return;
    }
    if entry.is_dir {
        toast_error(
            ui,
            &gettext("No versions"),
            &gettext("Folders don't have versions."),
        );
        return;
    }
    let target = if entry.path.is_empty() {
        VersionTarget::Uid(entry.uid.clone())
    } else {
        VersionTarget::Path(entry_rel(ui, entry))
    };

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &gettext("Versions"),
        &entry.name,
    )));
    toolbar.add_top_bar(&header);

    let group = adw::PreferencesGroup::builder()
        .title(gettext("Version history"))
        .description(gettext(
            "Proton Drive keeps earlier versions of a file until you delete them.",
        ))
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&group);
    let clamp = adw::Clamp::builder().child(&content).build();
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&clamp)
        .build();
    toolbar.set_content(Some(&scroll));

    let dialog = adw::Dialog::builder()
        .title(gettext("Versions"))
        .content_width(520)
        .content_height(520)
        .child(&toolbar)
        .build();

    let state = Rc::new(VersionsDialog {
        ui: ui.clone(),
        target,
        name: entry.name.clone(),
        group,
        rows: RefCell::new(Vec::new()),
    });

    versions_dialog_reload(&state);
    dialog.present(ui_window(ui).as_ref());
}

/// Re-fetch the file's revisions and rebuild the rows.
pub(crate) fn versions_dialog_reload(state: &Rc<VersionsDialog>) {
    let rx = spawn_request(state.ui.dirs.control_socket(), state.target.list());
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Revisions { items })) => repaint_versions(&state, &items),
            Ok(Ok(Response::Error { message, .. })) => {
                toast_error(&state.ui, &gettext("Couldn't load versions"), &message)
            }
            _ => toast_error(
                &state.ui,
                &gettext("Couldn't load versions"),
                &gettext("The mount service didn't respond."),
            ),
        }
    });
}

/// Rebuild the version rows from a fresh listing.
fn repaint_versions(state: &Rc<VersionsDialog>, items: &[RevisionInfo]) {
    for row in state.rows.borrow_mut().drain(..) {
        state.group.remove(&row);
    }
    let mut rows: Vec<gtk4::Widget> = Vec::new();
    if items.is_empty() {
        let row = dim_row(&gettext("No earlier versions."));
        state.group.add(&row);
        rows.push(row.upcast());
        *state.rows.borrow_mut() = rows;
        return;
    }

    for item in items {
        // The uploader's claimed plaintext size is the one that matches what the
        // user sees in a listing; the storage figure counts ciphertext.
        let size = item
            .claimed_size
            .filter(|size| *size >= 0)
            .unwrap_or(item.size_on_storage)
            .max(0) as u64;
        // A revision id means nothing to a person; name earlier versions by
        // when they were saved instead.
        let (title, mut subtitle) = if item.is_active {
            (
                gettext("Current version"),
                // Translators: {time} is when the version was saved, {size} its file size.
                gettext_f(
                    "{time} · {size}",
                    &[
                        ("time", &activity_time(item.created)),
                        ("size", &human_bytes(size)),
                    ],
                ),
            )
        } else {
            (activity_time(item.created), human_bytes(size))
        };
        if let Some(email) = &item.signed_by {
            subtitle.push_str(" · ");
            subtitle.push_str(email);
        }
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();

        let save = gtk4::Button::builder()
            .icon_name("document-save-symbolic")
            .tooltip_text(gettext("Save a copy…"))
            .valign(gtk4::Align::Center)
            .build();
        save.add_css_class("flat");
        let state_save = state.clone();
        let id = item.id.clone();
        save.connect_clicked(move |_| prompt_save_version(&state_save, &id));
        row.add_suffix(&save);

        if !item.is_active {
            let restore = gtk4::Button::builder()
                .icon_name("edit-undo-symbolic")
                .tooltip_text(gettext("Restore this version"))
                .valign(gtk4::Align::Center)
                .build();
            restore.add_css_class("flat");
            let state_restore = state.clone();
            let id = item.id.clone();
            restore.connect_clicked(move |_| prompt_restore_version(&state_restore, &id));
            row.add_suffix(&restore);

            let delete = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(gettext("Delete this version permanently"))
                .valign(gtk4::Align::Center)
                .build();
            delete.add_css_class("flat");
            let state_delete = state.clone();
            let id = item.id.clone();
            delete.connect_clicked(move |_| prompt_delete_version(&state_delete, &id));
            row.add_suffix(&delete);
        }

        state.group.add(&row);
        rows.push(row.upcast());
    }
    *state.rows.borrow_mut() = rows;
}

/// Confirm, then restore. Confirmed because it replaces the file's content for
/// every device on the account, not only this one.
fn prompt_restore_version(state: &Rc<VersionsDialog>, revision_id: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Restore version"))
        // Translators: {name} is a file name.
        .body(gettext_f(
            "Make this version the current content of “{name}”? The version it replaces stays in the history.",
            &[("name", &state.name)],
        ))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("restore", &pgettext("verb", "Restore"));
    dialog.set_response_appearance("restore", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let window = ui_window(&state.ui);
    let state = state.clone();
    let revision_id = revision_id.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp != "restore" {
            return;
        }
        versions_dialog_op(
            &state,
            state.target.restore(revision_id.clone()),
            // The server applies the swap in the background, so the wording
            // promises the request, not the result.
            &gettext("Restoring that version"),
            &gettext("Couldn't restore that version"),
        );
    });
    dialog.present(window.as_ref());
}

/// Confirm, then permanently delete one revision.
fn prompt_delete_version(state: &Rc<VersionsDialog>, revision_id: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Delete version"))
        .body(gettext(
            "Delete this version permanently? Its content can't be recovered.",
        ))
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("delete", &pgettext("verb", "Delete"));
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let window = ui_window(&state.ui);
    let state = state.clone();
    let revision_id = revision_id.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp != "delete" {
            return;
        }
        versions_dialog_op(
            &state,
            state.target.delete(revision_id.clone()),
            &gettext("Version deleted"),
            &gettext("Couldn't delete that version"),
        );
    });
    dialog.present(window.as_ref());
}

/// Ask where to write an old version, then have the daemon download it there.
///
/// The daemon writes the file, so the chosen location has to be one it can see:
/// a path from the portal-free `FileDialog` is a real filesystem path, which it
/// can.
fn prompt_save_version(state: &Rc<VersionsDialog>, revision_id: &str) {
    let window = ui_window(&state.ui);
    let dialog = gtk4::FileDialog::builder()
        .title(gettext("Save version"))
        .initial_name(&state.name)
        .build();
    let state = state.clone();
    let revision_id = revision_id.to_string();
    dialog.save(window.as_ref(), gio::Cancellable::NONE, move |res| {
        let Ok(file) = res else { return };
        let Some(dest) = file.path().and_then(|p| p.to_str().map(str::to_string)) else {
            toast_error(
                &state.ui,
                &gettext("Couldn't save that version"),
                &gettext("That location isn't a local file."),
            );
            return;
        };
        versions_dialog_op(
            &state,
            state.target.save_as(revision_id.clone(), dest),
            &gettext("Version saved"),
            &gettext("Couldn't save that version"),
        );
    });
}

/// Send one version request, toast the outcome, and refresh the list.
fn versions_dialog_op(state: &Rc<VersionsDialog>, request: Request, done: &str, failed: &str) {
    let rx = spawn_request(state.ui.dirs.control_socket(), request);
    let state = state.clone();
    let done = done.to_string();
    let failed = failed.to_string();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => {
                toast(&state.ui, &done);
                versions_dialog_reload(&state);
            }
            Ok(Ok(Response::Error { message, .. })) => toast_error(&state.ui, &failed, &message),
            _ => toast_error(
                &state.ui,
                &failed,
                &gettext("The mount service didn't respond."),
            ),
        }
    });
}
