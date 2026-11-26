//! The account: login, unlock, lock and logout.

use super::*;

/// Read a line from stdin with a prompt (echoed).
pub(super) fn prompt_line(label: &str) -> Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

pub(super) fn cmd_login(username: Option<String>, remember: bool) -> Result<()> {
    let username = match username {
        Some(u) => u,
        None => prompt_line("Proton email")?,
    };
    let password = rpassword::prompt_password("Password: ").context("read password")?;

    // The 2FA prompt fires lazily — only if the account actually requires it.
    let get_totp = || -> pdfs_core::Result<String> {
        prompt_line("2FA code").map_err(|e| pdfs_core::Error::Other(format!("read 2FA code: {e}")))
    };

    // Only a two-password account is ever asked for this.
    let get_mailbox = |attempt: u32| prompt_mailbox(attempt, remember);

    // Proton may ask for a CAPTCHA first, usually on a VPN. The CLI has no
    // browser engine, so the user solves the page in their own browser and says
    // when they are done; the retry then carries the challenge token.
    let get_hv = |hv: pdfs_core::proton_sdk::api::HumanVerification| {
        println!(
            "Proton is asking for a CAPTCHA before it will accept this sign-in.\n\
             Open this page in your browser and complete it:\n\n  {}\n",
            auth::browser_verification_url(&hv)
        );
        prompt_line("Press Enter once the page says you are verified")
            .map_err(|e| pdfs_core::Error::Other(format!("read verification answer: {e}")))?;
        Ok(auth::browser_verified(&hv))
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(auth::login_interactive(
        &username,
        &password,
        get_totp,
        get_mailbox,
        get_hv,
    ))
    .map_err(login_failure)?;

    // A daemon that is already up is parked in `wait_for_session`, sleeping
    // between keyring checks; restart it so the mount appears now instead of
    // whenever the next poll fires. Only when it is already active — `pdfs
    // login` deliberately does not enable the unit the way the GUI does, so a
    // headless user driving `pdfs daemon` by hand keeps that setup.
    if service::is_active() {
        service::restart();
    }

    println!("Logged in as {username}. Session stored in the system keyring.");
    report_key_state();
    Ok(())
}

pub(super) fn login_failure(error: pdfs_core::Error) -> anyhow::Error {
    if matches!(&error, pdfs_core::Error::Proton(e) if pdfs_core::error::sign_in_blocked(e)) {
        // The SDK logs the response code at debug level. Its server text
        // would give contradictory instructions in the displayed cause chain.
        anyhow::anyhow!(
            "Proton declined this sign-in (code 2028). Wait before trying again. If it persists, try signing in on the web and mention code 2028 when contacting Proton support."
        )
    } else {
        anyhow::Error::new(error).context("login failed")
    }
}

/// Ask for the mailbox password of a two-password account, without echo.
pub(super) fn prompt_mailbox(attempt: u32, remember: bool) -> pdfs_core::Result<auth::Mailbox> {
    if attempt == 0 {
        eprintln!("This account has a separate mailbox password.");
    } else {
        eprintln!("That mailbox password did not unlock the account. Try again.");
    }
    let password = rpassword::prompt_password("Mailbox password: ")
        .map_err(|e| pdfs_core::Error::Other(format!("read mailbox password: {e}")))?;
    Ok(auth::Mailbox {
        password: zeroize::Zeroizing::new(password),
        remember,
    })
}

/// Say where the account's keys are held, and what that means for the next login.
pub(super) fn report_key_state() {
    match auth::key_state() {
        Ok(auth::KeyState::Session) => println!(
            "Unlocked for this login session only. After you log out or reboot, run `pdfs unlock`."
        ),
        Ok(auth::KeyState::Locked) => println!("The account is locked. Run `pdfs unlock`."),
        _ => {}
    }
}

pub(super) fn cmd_unlock(remember: bool) -> Result<()> {
    auth::load().context("not logged in (run `pdfs login` first)")?;
    let rt = tokio::runtime::Runtime::new()?;
    // Bounded like the login prompt: three wrong answers end the command.
    let mut attempt = 0;
    loop {
        let mailbox = prompt_mailbox(attempt, remember)?;
        match rt.block_on(auth::unlock(&mailbox.password, remember)) {
            Ok(()) => break,
            Err(pdfs_core::Error::WrongMailboxPassword) if attempt < 2 => attempt += 1,
            Err(e) => return Err(anyhow::Error::new(e).context("unlock failed")),
        }
    }
    // A daemon parked on the locked account would otherwise notice on its next
    // poll; restarting it makes the mount appear now, as after `pdfs login`.
    if service::is_active() {
        service::restart();
    }
    println!("Unlocked.");
    report_key_state();
    Ok(())
}

pub(super) fn cmd_lock(forget: bool) -> Result<()> {
    auth::lock(forget)?;
    println!(
        "Locked. {}",
        if forget {
            "Unlock removed from memory and keyring."
        } else {
            "Run `pdfs unlock` to unlock again."
        }
    );
    // The running daemon keeps its keys until it restarts; say so, since
    // "locked" would otherwise suggest the mount stopped.
    if service::is_active() {
        println!(
            "The running daemon keeps its keys until it restarts: `systemctl --user restart proton-drive`."
        );
    }
    Ok(())
}

/// `stored`, `session` or `locked`: where the account's keys are held.
pub(super) fn key_state_name() -> &'static str {
    match auth::key_state() {
        Ok(auth::KeyState::Stored) => "stored",
        Ok(auth::KeyState::Session) => "session",
        _ => "locked",
    }
}

pub(super) fn cmd_logout(yes: bool) -> Result<()> {
    // Queued changes stay staged on this computer and are sent after the next
    // sign-in, but until then they exist nowhere else. Without a running
    // daemon there is no count to show.
    if !yes
        && let Ok(CtlResponse::Status {
            pending_uploads,
            pending_changes,
            parked_uploads,
            ..
        }) = control_request(CtlRequest::Status)
    {
        let queued = pending_uploads + pending_changes + parked_uploads;
        if queued > 0 {
            println!(
                "{queued} change(s) are not on Proton Drive yet. They stay on this computer and are \
                 sent after you sign in again. Run `pdfs sync flush` first to send them now."
            );
            confirm("Sign out anyway?")?;
        }
    }
    if auth::logout()? {
        println!("Signed out: the session is ended on Proton and cleared here.");
    } else {
        println!(
            "Stored session cleared. Proton could not be reached to end the session; \
             it stays valid until it expires or is signed out in your account's security settings."
        );
    }
    Ok(())
}
