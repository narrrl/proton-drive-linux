use crate::*;

pub(crate) struct LoginState {
    // Login page.
    pub(crate) email: adw::EntryRow,
    pub(crate) password: adw::PasswordEntryRow,
    pub(crate) login_button: gtk4::Button,
    pub(crate) login_status: gtk4::Label,
}

/// The login page: an email row, password row, a primary "Sign in" button and a
/// status label, centred in a clamp. The 2FA code is prompted lazily in a
/// dialog (see [`prompt_2fa`]) only when the account actually requires it.
#[allow(clippy::type_complexity)]
pub(crate) fn build_login_page() -> (
    gtk4::Widget,
    (
        adw::EntryRow,
        adw::PasswordEntryRow,
        gtk4::Button,
        gtk4::Label,
    ),
) {
    let group = adw::PreferencesGroup::builder()
        .title("Sign in to Proton")
        .description("Use your Proton account to connect Drive.")
        .build();

    let email = adw::EntryRow::builder().title("Email or username").build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    group.add(&email);
    group.add(&password);

    let login_button = gtk4::Button::builder()
        .label("Sign in")
        .halign(gtk4::Align::Center)
        .build();
    login_button.add_css_class("suggested-action");
    login_button.add_css_class("pill");

    let login_status = gtk4::Label::builder()
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .build();
    login_status.add_css_class("dim-label");

    let header = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let logo = gtk4::Image::from_icon_name("folder-remote-symbolic");
    logo.set_pixel_size(64);
    logo.add_css_class("brand-icon");
    let title = gtk4::Label::new(Some("Proton Drive"));
    title.add_css_class("brand-title");
    header.append(&logo);
    header.append(&title);
    header.set_margin_bottom(12);

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    inner.set_margin_top(32);
    inner.set_margin_bottom(32);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.append(&header);
    inner.append(&group);
    inner.append(&login_button);
    inner.append(&login_status);

    let clamp = adw::Clamp::builder()
        .maximum_size(420)
        .child(&inner)
        .build();
    let scroll = gtk4::ScrolledWindow::builder().child(&clamp).build();

    (
        scroll.upcast(),
        (email, password, login_button, login_status),
    )
}

/// Connect the sign-in button: read the fields, run [`auth::login`] on a worker
/// thread, and report the outcome back on the main loop.
pub(crate) fn wire_login(ui: &Rc<Ui>) {
    // Pressing Enter in either field submits the form, so signing in never needs
    // a reach for the mouse. Both fields route to the same button.
    let btn = ui.login.login_button.clone();
    ui.login.email
        .connect_entry_activated(move |_| btn.emit_clicked());
    let btn = ui.login.login_button.clone();
    ui.login.password
        .connect_entry_activated(move |_| btn.emit_clicked());

    let ui = ui.clone();
    let button = ui.login.login_button.clone();
    button.connect_clicked(move |_| {
        let username = ui.login.email.text().to_string();
        let password = ui.login.password.text().to_string();
        if username.is_empty() || password.is_empty() {
            ui.login.login_status.set_text("Enter your email and password.");
            return;
        }

        ui.login.login_button.set_sensitive(false);
        ui.login.login_status.set_text("Signing in…");
        let (rx, totp_req_rx) = spawn_login(username, password);

        // Surface the 2FA dialog only if the SDK actually asks for a code (i.e.
        // the account has 2FA enabled). The worker blocks until the dialog feeds
        // back a code (or is cancelled, dropping the sender).
        let ui_2fa = ui.clone();
        glib::spawn_future_local(async move {
            if let Ok(code_tx) = totp_req_rx.recv().await {
                prompt_2fa(&ui_2fa, code_tx);
            }
        });

        let ui = ui.clone();
        glib::spawn_future_local(async move {
            let result = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err("login cancelled".into()));
            ui.login.login_button.set_sensitive(true);
            match result {
                Ok(()) => {
                    ui.login.login_status.set_text("");
                    ui.login.password.set_text("");
                    // Cache the new identity so `refresh` never hits the keyring.
                    *ui.session.borrow_mut() = auth::load().ok();
                    // Enable+start the mount service now that we have a session.
                    service::enable_start();
                    refresh(&ui);
                }
                Err(e) => ui.login.login_status.set_text(&format!("Sign-in failed: {e}")),
            }
        });
    });
}

/// Run the async SRP + optional 2FA login on a dedicated current-thread Tokio
/// runtime. Returns two channels: the first yields the final login result once;
/// the second fires *only if* the account needs a 2FA code, carrying a
/// [`std::sync::mpsc::Sender`] the UI uses to feed the code back. The login
/// closure blocks the worker on that sender until the dialog answers, so the
/// code is requested lazily and can't expire before the password proof.
#[allow(clippy::type_complexity)]
pub(crate) fn spawn_login(
    username: String,
    password: String,
) -> (
    async_channel::Receiver<Result<(), String>>,
    async_channel::Receiver<std::sync::mpsc::Sender<String>>,
) {
    let (tx, rx) = async_channel::bounded(1);
    let (totp_req_tx, totp_req_rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send_blocking(Err(e.to_string()));
                return;
            }
        };
        let result = rt.block_on(async move {
            auth::login(&username, &password, || {
                // 2FA required: hand a one-shot sender to the UI and block until
                // the dialog supplies the code. A dropped sender (cancelled
                // dialog) surfaces as a cancelled login.
                let (code_tx, code_rx) = std::sync::mpsc::channel::<String>();
                totp_req_tx
                    .send_blocking(code_tx)
                    .map_err(|_| pdfs_core::Error::Other("two-factor prompt closed".into()))?;
                code_rx
                    .recv()
                    .map_err(|_| pdfs_core::Error::Other("two-factor entry cancelled".into()))
            })
            .await
            .map_err(|e| e.to_string())
        });
        let _ = tx.send_blocking(result);
    });
    (rx, totp_req_rx)
}

/// Show the lazy two-factor dialog and feed the entered code back to the waiting
/// login worker via `code_tx`. Cancelling (or closing) drops the sender, which
/// the worker reads as a cancelled login.
pub(crate) fn prompt_2fa(ui: &Rc<Ui>, code_tx: std::sync::mpsc::Sender<String>) {
    let dialog = adw::AlertDialog::builder()
        .heading("Two-factor authentication")
        .body("Enter the code from your authenticator app.")
        .build();

    let group = adw::PreferencesGroup::new();
    let entry = adw::EntryRow::builder()
        .title("Authentication code")
        .activates_default(true)
        .build();
    group.add(&entry);
    dialog.set_extra_child(Some(&group));

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Confirm");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let code_tx = RefCell::new(Some(code_tx));
    dialog.connect_response(None, move |_, resp| {
        // On cancel/close we take + drop `tx` without sending, so the worker's
        // recv errors out and the login is reported as cancelled.
        if let Some(tx) = code_tx.borrow_mut().take()
            && resp == "confirm"
        {
            let _ = tx.send(entry.text().trim().to_string());
        }
    });

    let parent = ui.login.login_button.root().and_downcast::<gtk4::Window>();
    dialog.present(parent.as_ref());
}

/// Connect the sign-out button: disable+stop the mount service (so the daemon
/// isn't left running without credentials), forget the stored session, and drop
/// back to the login page.
pub(crate) fn wire_logout(ui: &Rc<Ui>, button: &gtk4::Button) {
    let ui = ui.clone();
    button.connect_clicked(move |_| {
        service::disable_stop();
        if let Err(e) = auth::logout() {
            tracing::error!("logout failed: {e}");
        }
        *ui.session.borrow_mut() = None;
        refresh(&ui);
    });
}
