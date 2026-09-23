use crate::*;
use pdfs_core::proton_sdk::api::HumanVerificationCredential;

pub(crate) struct LoginState {
    // Login page.
    pub(crate) email: adw::EntryRow,
    pub(crate) password: adw::PasswordEntryRow,
    pub(crate) login_button: gtk4::Button,
    /// Spins inside the Sign in button while a sign-in is running.
    pub(crate) login_spinner: gtk4::Spinner,
    pub(crate) login_status: gtk4::Label,
}

/// Where the login page's account links go.
const SIGNUP_URL: &str = "https://account.proton.me/drive/signup";
const RESET_PASSWORD_URL: &str = "https://account.proton.me/reset-password";

/// The login page: an email row, password row, a primary "Sign in" button, a
/// status label and links to create an account or reset the password, centred
/// in a clamp. The 2FA code is prompted lazily in a dialog (see [`prompt_2fa`])
/// only when the account actually requires it.
pub(crate) fn build_login_page() -> (gtk4::Widget, LoginState) {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Sign in to Proton"))
        .description(gettext("Use your Proton account to connect Drive."))
        .build();

    let email = adw::EntryRow::builder()
        .title(gettext("Email or username"))
        .build();
    let password = adw::PasswordEntryRow::builder()
        .title(gettext("Password"))
        .build();
    group.add(&email);
    group.add(&password);

    let login_spinner = gtk4::Spinner::builder().visible(false).build();
    let button_content = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    button_content.append(&login_spinner);
    button_content.append(&gtk4::Label::new(Some(&gettext("Sign in"))));
    let login_button = gtk4::Button::builder()
        .child(&button_content)
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
    let links = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    links.set_halign(gtk4::Align::Center);
    links.append(&gtk4::LinkButton::with_label(
        SIGNUP_URL,
        &gettext("Create account"),
    ));
    links.append(&gtk4::LinkButton::with_label(
        RESET_PASSWORD_URL,
        &gettext("Forgot password?"),
    ));
    inner.append(&links);

    let clamp = adw::Clamp::builder()
        .maximum_size(420)
        .child(&inner)
        .build();
    let scroll = gtk4::ScrolledWindow::builder().child(&clamp).build();

    (
        scroll.upcast(),
        LoginState {
            email,
            password,
            login_button,
            login_spinner,
            login_status,
        },
    )
}

/// Connect the sign-in button: read the fields, run [`auth::login`] on a worker
/// thread, and report the outcome back on the main loop.
pub(crate) fn wire_login(ui: &Rc<Ui>) {
    // Pressing Enter in either field submits the form, so signing in never needs
    // a reach for the mouse. Both fields route to the same button.
    let btn = ui.login.login_button.clone();
    ui.login
        .email
        .connect_entry_activated(move |_| btn.emit_clicked());
    let btn = ui.login.login_button.clone();
    ui.login
        .password
        .connect_entry_activated(move |_| btn.emit_clicked());

    let ui = ui.clone();
    let button = ui.login.login_button.clone();
    button.connect_clicked(move |_| {
        let username = ui.login.email.text().to_string();
        let password = ui.login.password.text().to_string();
        if username.is_empty() || password.is_empty() {
            ui.login
                .login_status
                .set_text(&gettext("Enter your email and password."));
            return;
        }

        set_signing_in(&ui, true);
        ui.login.login_status.set_text(&gettext("Signing in…"));
        let (rx, totp_req_rx, hv_req_rx) = spawn_login(username, password);

        // Surface the 2FA dialog only if the SDK actually asks for a code (i.e.
        // the account has 2FA enabled). The worker blocks until the dialog feeds
        // back a code (or is cancelled, dropping the sender).
        //
        // Loops rather than awaiting once: a login gated behind a CAPTCHA is
        // restarted after verification, so a 2FA account is asked for a second,
        // unexpired code on the retry.
        let ui_2fa = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(code_tx) = totp_req_rx.recv().await {
                prompt_2fa(&ui_2fa, code_tx);
            }
        });

        // Same shape for human verification: only fires when the API gates the
        // sign-in, and the worker is blocked on the token until it does.
        let ui_hv = ui.clone();
        glib::spawn_future_local(async move {
            if let Ok((url, token_tx)) = hv_req_rx.recv().await {
                ui_hv
                    .login
                    .login_status
                    .set_text(&gettext("Complete the verification to continue…"));
                prompt_human_verification(&ui_hv, &url, token_tx);
            }
        });

        let ui = ui.clone();
        glib::spawn_future_local(async move {
            let result = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err(gettext("login cancelled")));
            set_signing_in(&ui, false);
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
                Err(e) => ui.login.login_status.set_text(&e),
            }
        });
    });
}

/// Lock the form and spin the button while a sign-in runs.
fn set_signing_in(ui: &Rc<Ui>, running: bool) {
    ui.login.login_button.set_sensitive(!running);
    ui.login.email.set_sensitive(!running);
    ui.login.password.set_sensitive(!running);
    ui.login.login_spinner.set_visible(running);
    ui.login.login_spinner.set_spinning(running);
}

/// What to tell the person when sign-in fails. Proton's own API messages are
/// written for people ("Incorrect login credentials…"), so they pass through;
/// everything else is mapped from its kind rather than shown as a debug string.
pub(crate) fn login_error_message(error: &pdfs_core::Error) -> String {
    use pdfs_core::proton_sdk::ProtonError;
    match error {
        pdfs_core::Error::Proton(ProtonError::Api(api)) if api.http_status == 429 => {
            gettext("Too many sign-in attempts. Wait a few minutes and try again.")
        }
        pdfs_core::Error::Proton(ProtonError::Api(api)) if !api.message.is_empty() => {
            api.message.clone()
        }
        pdfs_core::Error::Proton(ProtonError::Transport(_)) => {
            gettext("Couldn't reach Proton. Check your internet connection and try again.")
        }
        pdfs_core::Error::Keyring(_) => gettext(
            "Signed in, but the session couldn't be saved to the system keyring. Make sure a keyring (GNOME Keyring or KWallet) is running and unlocked.",
        ),
        pdfs_core::Error::Other(message) if message.contains("two-factor") => {
            gettext("Sign-in cancelled: no two-factor code was entered.")
        }
        pdfs_core::Error::Other(message) if message.contains("verification") => gettext(
            "Sign-in cancelled: the verification wasn't completed. Sign in again to get a new one.",
        ),
        other => {
            let error = other.to_string();
            // Translators: {error} is an error message, usually in English.
            gettext_f("Sign-in failed: {error}", &[("error", &error)])
        }
    }
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
    async_channel::Receiver<(String, std::sync::mpsc::Sender<String>)>,
) {
    let (tx, rx) = async_channel::bounded(1);
    // Two, not one: a CAPTCHA-gated login restarts, so a 2FA account is asked
    // for a code on each attempt and a single slot would deadlock the worker.
    let (totp_req_tx, totp_req_rx) = async_channel::bounded(2);
    let (hv_req_tx, hv_req_rx) = async_channel::bounded(1);
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
            auth::login_interactive(
                &username,
                &password,
                || {
                    // 2FA required: hand a one-shot sender to the UI and block
                    // until the dialog supplies the code. A dropped sender
                    // (cancelled dialog) surfaces as a cancelled login.
                    let (code_tx, code_rx) = std::sync::mpsc::channel::<String>();
                    totp_req_tx
                        .send_blocking(code_tx)
                        .map_err(|_| pdfs_core::Error::Other("two-factor prompt closed".into()))?;
                    code_rx
                        .recv()
                        .map_err(|_| pdfs_core::Error::Other("two-factor entry cancelled".into()))
                },
                |hv| {
                    // Gated: hand the UI the page to show, block on the token the
                    // user earns by solving it.
                    let (token_tx, token_rx) = std::sync::mpsc::channel::<String>();
                    hv_req_tx
                        .send_blocking((hv.verification_url(), token_tx))
                        .map_err(|_| {
                            pdfs_core::Error::Other("verification prompt closed".into())
                        })?;
                    let token = token_rx
                        .recv()
                        .map_err(|_| pdfs_core::Error::Other("verification cancelled".into()))?;
                    Ok(HumanVerificationCredential::captcha(token))
                },
            )
            .await
            .map_err(|e| login_error_message(&e))
        });
        let _ = tx.send_blocking(result);
    });
    (rx, totp_req_rx, hv_req_rx)
}

/// Show the lazy two-factor dialog and feed the entered code back to the waiting
/// login worker via `code_tx`. Cancelling (or closing) drops the sender, which
/// the worker reads as a cancelled login.
pub(crate) fn prompt_2fa(ui: &Rc<Ui>, code_tx: std::sync::mpsc::Sender<String>) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Two-factor authentication"))
        .body(gettext(
            "Enter the code from your authenticator app, or one of your recovery codes.",
        ))
        .build();

    let group = adw::PreferencesGroup::new();
    let entry = adw::EntryRow::builder()
        .title(gettext("Authentication code"))
        .activates_default(true)
        .build();
    group.add(&entry);
    dialog.set_extra_child(Some(&group));

    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("confirm", &gettext("Confirm"));
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

/// Sign out, after a confirmation: disable+stop the mount service (so the
/// daemon isn't left running without credentials), forget the stored session,
/// and drop back to the login page.
pub(crate) fn sign_out(ui: &Rc<Ui>) {
    let Some(window) = ui_window(ui) else { return };
    let ui = ui.clone();
    confirm_destructive(
        &window,
        &gettext("Sign Out?"),
        &gettext(
            "Proton Drive will disconnect and stop syncing until you sign in again. Files already on this computer stay where they are.",
        ),
        &gettext("Sign Out"),
        move || {
            service::disable_stop();
            if let Err(e) = auth::logout() {
                tracing::error!("logout failed: {e}");
            }
            *ui.session.borrow_mut() = None;
            refresh(&ui);
        },
    );
}

#[cfg(test)]
mod tests {
    use super::login_error_message;

    #[test]
    fn a_cancelled_prompt_says_which_step_was_skipped() {
        let totp = pdfs_core::Error::Other("two-factor entry cancelled".into());
        assert!(login_error_message(&totp).contains("two-factor code"));
        let hv = pdfs_core::Error::Other("verification cancelled".into());
        assert!(login_error_message(&hv).contains("verification wasn't completed"));
    }

    #[test]
    fn an_unmapped_error_keeps_its_text() {
        let error = pdfs_core::Error::Other("something odd".into());
        assert_eq!(login_error_message(&error), "Sign-in failed: something odd");
    }
}
