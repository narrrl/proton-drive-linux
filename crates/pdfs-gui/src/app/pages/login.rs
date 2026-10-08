use crate::*;
use pdfs_core::proton_sdk::api::HumanVerificationCredential;

pub(crate) struct LoginState {
    // Login page.
    pub(crate) email: adw::EntryRow,
    pub(crate) password: adw::PasswordEntryRow,
    pub(crate) login_button: gtk4::Button,
    /// Spins inside the Sign in button while a sign-in is running.
    pub(crate) login_spinner: Spinner,
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

    let login_spinner = spinner();
    login_spinner.set_visible(false);
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
    let logo = gtk4::Image::from_icon_name(APP_ID);
    logo.set_pixel_size(96);
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
        let LoginChannels {
            result: rx,
            totp: totp_req_rx,
            hv: hv_req_rx,
            mailbox: mailbox_req_rx,
        } = spawn_login(username, password);

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
        // sign-in, and the worker is blocked on the token until it does. A
        // window that never came up is told apart from one the user closed, so
        // the result below can say why the sign-in stopped.
        let ui_hv = ui.clone();
        let verification_failed = Rc::new(Cell::new(false));
        let hv_failed = verification_failed.clone();
        glib::spawn_future_local(async move {
            if let Ok((url, token_tx)) = hv_req_rx.recv().await {
                ui_hv
                    .login
                    .login_status
                    .set_text(&gettext("Complete the verification to continue…"));
                // Anything but a token drops the sender, which the worker reads
                // as a cancelled sign-in.
                match verify_human(url).await {
                    Verification::Solved(token) => {
                        let _ = token_tx.send(token);
                    }
                    Verification::Cancelled => {}
                    Verification::Failed => hv_failed.set(true),
                }
            }
        });

        // And for the mailbox password, which only a two-password account has.
        // The worker asks again after a wrong answer, so this loops too.
        let ui_mailbox = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok((attempt, answer_tx)) = mailbox_req_rx.recv().await {
                ask_mailbox(&ui_mailbox, attempt > 0, move |answer| {
                    // A cancelled dialog drops the sender, which the worker
                    // reads as a cancelled login.
                    if let Some(answer) = answer {
                        let _ = answer_tx.send(answer);
                    }
                });
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
                    let _ = gio::spawn_blocking(service::enable_start).await;
                    refresh(&ui);
                }
                Err(_) if verification_failed.get() => ui.login.login_status.set_text(&gettext(
                    "The verification page couldn't be opened, so the sign-in was cancelled. Your system may keep the web view's sandbox from starting; the troubleshooting guide explains what to do.",
                )),
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
        pdfs_core::Error::Other(message) if message.contains("mailbox") => {
            gettext("Sign-in cancelled: no mailbox password was entered.")
        }
        pdfs_core::Error::WrongMailboxPassword => {
            gettext("That mailbox password didn't unlock your account.")
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

/// What the login worker asks the UI for, and how it reports back.
pub(crate) struct LoginChannels {
    /// The final outcome, once.
    pub(crate) result: async_channel::Receiver<Result<(), String>>,
    /// Fires only if the account needs a 2FA code; carries the sender for it.
    pub(crate) totp: async_channel::Receiver<std::sync::mpsc::Sender<String>>,
    /// Fires only if the sign-in is gated: the page to show, and the sender for
    /// the token the user earns on it.
    pub(crate) hv: async_channel::Receiver<(String, std::sync::mpsc::Sender<String>)>,
    /// Fires only for a two-password account: how many answers were already
    /// wrong, and the sender for the password and the "remember" choice.
    pub(crate) mailbox: async_channel::Receiver<(u32, std::sync::mpsc::Sender<(String, bool)>)>,
}

/// Run the async SRP + optional 2FA login on a dedicated current-thread Tokio
/// runtime. The returned [`LoginChannels`] carry the final result and the lazy
/// prompts. Each login closure blocks the worker on a sender the UI answers, so
/// a code is requested lazily and can't expire before the password proof.
pub(crate) fn spawn_login(username: String, password: String) -> LoginChannels {
    let (tx, rx) = async_channel::bounded(1);
    // Two, not one: a CAPTCHA-gated login restarts, so a 2FA account is asked
    // for a code on each attempt and a single slot would deadlock the worker.
    let (totp_req_tx, totp_req_rx) = async_channel::bounded(2);
    let (hv_req_tx, hv_req_rx) = async_channel::bounded(1);
    let (mailbox_req_tx, mailbox_req_rx) = async_channel::bounded(1);
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
                |attempt| {
                    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<(String, bool)>();
                    mailbox_req_tx
                        .send_blocking((attempt, answer_tx))
                        .map_err(|_| pdfs_core::Error::Other("mailbox prompt closed".into()))?;
                    let (password, remember) = answer_rx
                        .recv()
                        .map_err(|_| pdfs_core::Error::Other("mailbox entry cancelled".into()))?;
                    Ok(auth::Mailbox {
                        password: password.into(),
                        remember,
                    })
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
    LoginChannels {
        result: rx,
        totp: totp_req_rx,
        hv: hv_req_rx,
        mailbox: mailbox_req_rx,
    }
}

/// Ask for the mailbox password of a two-password account.
///
/// `done` gets the password and the "keep unlocked" choice, or `None` when the
/// dialog was cancelled. `retry` says the last answer was wrong. The choice
/// defaults to off: storing the mailbox unlock in the keyring trades away what
/// a separate mailbox password is for, so it is something to opt into.
pub(crate) fn ask_mailbox(
    ui: &Rc<Ui>,
    retry: bool,
    done: impl FnOnce(Option<(String, bool)>) + 'static,
) {
    let body = if retry {
        gettext("That mailbox password didn't unlock your account. Try again.")
    } else {
        gettext("This account has a separate mailbox password. Enter it to unlock your files.")
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Mailbox Password"))
        .body(body)
        .build();

    let group = adw::PreferencesGroup::new();
    let entry = adw::PasswordEntryRow::builder()
        .title(gettext("Mailbox password"))
        .activates_default(true)
        .build();
    let remember = adw::SwitchRow::builder()
        .title(gettext("Keep unlocked on this computer"))
        .subtitle(gettext(
            "Stores the unlock in the system keyring, so Proton Drive opens without asking after every login. Otherwise you enter the password once per session.",
        ))
        .build();
    group.add(&entry);
    group.add(&remember);
    dialog.set_extra_child(Some(&group));

    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("unlock", &gettext("Unlock"));
    dialog.set_response_appearance("unlock", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("unlock"));
    dialog.set_close_response("cancel");

    let done = RefCell::new(Some(done));
    dialog.connect_response(None, move |_, resp| {
        let Some(done) = done.borrow_mut().take() else {
            return;
        };
        let password = entry.text().to_string();
        if resp == "unlock" && !password.is_empty() {
            done(Some((password, remember.is_active())));
        } else {
            done(None);
        }
    });

    let parent = ui_window(ui).map(|w| w.upcast::<gtk4::Window>());
    dialog.present(parent.as_ref());
}

/// If the signed-in account is locked, ask for its mailbox password — once per
/// run; after a cancel, a toast keeps an Unlock button within reach.
///
/// The key state is a keyring and kernel-keyring read, so it runs on a worker.
pub(crate) fn offer_unlock(ui: &Rc<Ui>) {
    if ui.unlock_offered.get() {
        return;
    }
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let state = gio::spawn_blocking(|| auth::key_state().ok()).await;
        if matches!(state, Ok(Some(auth::KeyState::Locked))) && !ui.unlock_offered.replace(true) {
            prompt_unlock(&ui, false);
        }
    });
}

enum UnlockOutcome {
    Unlocked,
    Wrong,
    Failed(String),
}

/// Ask for the mailbox password and unlock the account with it.
fn prompt_unlock(ui: &Rc<Ui>, retry: bool) {
    let ui_done = ui.clone();
    ask_mailbox(ui, retry, move |answer| {
        let Some((password, remember)) = answer else {
            toast_action(
                &ui_done,
                &gettext("Proton Drive is locked. Enter your mailbox password to unlock it."),
                &gettext("Unlock"),
                |ui| prompt_unlock(ui, false),
            );
            return;
        };
        glib::spawn_future_local(async move {
            // Verifying talks to Proton and the restart talks to systemd, so
            // both run on a worker, never on the main loop.
            let outcome = gio::spawn_blocking(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => return UnlockOutcome::Failed(e.to_string()),
                };
                match rt.block_on(auth::unlock(&password, remember)) {
                    Ok(()) => {
                        service::restart();
                        UnlockOutcome::Unlocked
                    }
                    Err(pdfs_core::Error::WrongMailboxPassword) => UnlockOutcome::Wrong,
                    Err(e) => UnlockOutcome::Failed(login_error_message(&e)),
                }
            })
            .await;
            match outcome {
                Ok(UnlockOutcome::Unlocked) => {
                    toast(&ui_done, &gettext("Unlocked."));
                    refresh(&ui_done);
                }
                Ok(UnlockOutcome::Wrong) => prompt_unlock(&ui_done, true),
                Ok(UnlockOutcome::Failed(message)) => {
                    toast_error(&ui_done, &gettext("Couldn't unlock"), &message)
                }
                Err(_) => {}
            }
        });
    });
}

/// Show the lazy two-factor dialog and feed the entered code back to the waiting
/// login worker via `code_tx`. Cancelling (or closing) drops the sender, which
/// the worker reads as a cancelled login.
pub(crate) fn prompt_2fa(ui: &Rc<Ui>, code_tx: std::sync::mpsc::Sender<String>) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Two-Factor Authentication"))
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
            // systemctl and the keyring both block, so run them on a worker.
            let ui = ui.clone();
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(|| {
                    service::disable_stop();
                    auth::logout().map_err(|e| e.to_string())
                })
                .await;
                if let Ok(Err(e)) = result {
                    tracing::error!("logout failed: {e}");
                }
                *ui.session.borrow_mut() = None;
                refresh(&ui);
            });
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
    fn a_cancelled_mailbox_prompt_is_reported_as_such() {
        let error = pdfs_core::Error::Other("mailbox entry cancelled".into());
        assert!(login_error_message(&error).contains("no mailbox password"));
    }

    #[test]
    fn an_unmapped_error_keeps_its_text() {
        let error = pdfs_core::Error::Other("something odd".into());
        assert_eq!(login_error_message(&error), "Sign-in failed: something odd");
    }
}
