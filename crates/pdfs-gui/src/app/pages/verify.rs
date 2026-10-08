//! The human-verification (CAPTCHA) window.
//!
//! Proton gates logins it does not recognise — a new IP, a VPN exit — behind a
//! CAPTCHA. The API answers the login with a challenge instead of a session, and
//! expects the client to present Proton's hosted verification page, collect the
//! token the user earns by solving it, and retry the login carrying that token.
//!
//! The page is hosted in a `WebKitWebView` rather than handed to the system
//! browser because it reports completion by posting a message to its host, not
//! by redirecting: a browser has nowhere to post it back to.
//!
//! The webview runs in a child `pdfs-app` started with [`VERIFY_ARG`], not in the
//! app itself. WebKit aborts its whole process when its sandbox cannot start, as
//! on hardened systems that keep bubblewrap from the journal socket
//! (`docs/BUGS.md` B195); in a child that costs the verification, not the app.
//! The URL goes to the child on stdin, so it never shows in a process list, and
//! the token comes back on stdout.

use crate::*;
use std::io::{BufRead, BufReader, Write as _};
use std::process::{ExitStatus, Stdio};
use webkit6::prelude::*;

/// The argument that makes `pdfs-app` the verification window instead of the app.
pub(crate) const VERIFY_ARG: &str = "--human-verification";

/// Marks the token among whatever else lands on the child's stdout: GLib and
/// WebKit write debug output there.
const TOKEN_PREFIX: &str = "hv-token ";

/// Bridge the verification page's `postMessage` to a handler this side can read.
///
/// The page targets its embedding host (`window.parent`), which in a webview is
/// the page itself — so nothing arrives unless the message is forwarded
/// explicitly. Injecting at document start guarantees the listener is installed
/// before the page can post anything.
const BRIDGE: &str = r#"
window.addEventListener('message', function (event) {
    try {
        window.webkit.messageHandlers.hv.postMessage(JSON.stringify(event.data));
    } catch (e) {}
});
"#;

/// How the verification window ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verification {
    Solved(String),
    /// Closed without solving.
    Cancelled,
    /// Never came up, or died: most likely WebKit could not start its sandbox.
    Failed,
}

/// Show the challenge in the verification window and wait until it closes.
pub(crate) async fn verify_human(url: String) -> Verification {
    match gio::spawn_blocking(move || run_verification_child(&url)).await {
        Ok(Ok(verification)) => verification,
        Ok(Err(e)) => {
            tracing::error!("cannot run the verification window: {e}");
            Verification::Failed
        }
        Err(_) => Verification::Failed,
    }
}

fn run_verification_child(url: &str) -> std::io::Result<Verification> {
    let mut child = Command::new(own_exe())
        .arg(VERIFY_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A failed write means the child is already gone; its exit status says
        // how. Dropping `stdin` closes it.
        let _ = writeln!(stdin, "{url}");
    }
    // Stops reading at the token rather than at EOF: WebKit's helper processes
    // inherit the pipe and may hold it open a moment after the child quits.
    let token = child
        .stdout
        .take()
        .and_then(|out| find_token(BufReader::new(out)));
    let status = child.wait()?;
    Ok(outcome(token, status))
}

/// This very binary, so a development build opens its own window. After an
/// upgrade replaced the file, `current_exe` names a deleted path, and the
/// installed `pdfs-app` is the next best.
fn own_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("pdfs-app"))
}

fn find_token(out: impl BufRead) -> Option<String> {
    out.lines()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix(TOKEN_PREFIX).map(str::to_owned))
}

/// A token wins over the exit status: the user solved the page, even if
/// WebKit's teardown crashed after it.
fn outcome(token: Option<String>, status: ExitStatus) -> Verification {
    match token {
        Some(token) => Verification::Solved(token),
        None if status.success() => Verification::Cancelled,
        None => {
            tracing::error!(%status, "the verification window failed");
            Verification::Failed
        }
    }
}

/// Run as the verification window: read the page's URL from stdin, show it, and
/// print the token once the user has solved it. Closing the window exits
/// without a token, which the app reads as a cancelled sign-in.
pub(crate) fn run_verification_window() -> glib::ExitCode {
    let mut url = String::new();
    if std::io::stdin().lock().read_line(&mut url).is_err() || url.trim().is_empty() {
        tracing::error!("{VERIFY_ARG} expects the verification URL on stdin");
        return glib::ExitCode::FAILURE;
    }
    let url = url.trim().to_owned();

    // Not unique: the app that started it already holds APP_ID.
    let app = adw::Application::builder()
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| build_verification_window(app, &url));
    // GApplication would refuse VERIFY_ARG as an unknown option.
    let argv0 = std::env::args().next().unwrap_or_default();
    app.run_with_args(&[argv0])
}

fn build_verification_window(app: &adw::Application, url: &str) {
    let content = webkit6::UserContentManager::new();
    content.add_script(&webkit6::UserScript::new(
        BRIDGE,
        webkit6::UserContentInjectedFrames::AllFrames,
        webkit6::UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    // Registration is what makes `messageHandlers.hv` exist in the page; without
    // it the bridge above throws on every message.
    content.register_script_message_handler("hv", None);

    let webview = webkit6::WebView::builder()
        .user_content_manager(&content)
        .vexpand(true)
        .hexpand(true)
        .build();

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    body.append(&adw::HeaderBar::new());
    body.append(&webview);
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(gettext("Verification"))
        .icon_name(APP_ID)
        .default_width(420)
        .default_height(560)
        .content(&body)
        .build();

    let app = app.downgrade();
    content.connect_script_message_received(Some("hv"), move |_, value| {
        let Some(token) = extract_token(&value.to_str()) else {
            // Every `postMessage` on the page reaches here, most of them the
            // verification app's own chatter. Anything that is not a completion
            // is simply not ours.
            return;
        };
        let mut stdout = std::io::stdout().lock();
        if let Err(e) = writeln!(stdout, "{TOKEN_PREFIX}{token}").and_then(|()| stdout.flush()) {
            tracing::error!("cannot hand the verification token back: {e}");
        }
        if let Some(app) = app.upgrade() {
            app.quit();
        }
    });

    webview.load_uri(url);
    window.present();
}

/// Pull the verification token out of a message posted by the verification page.
///
/// Returns `None` for anything that is not a completion message. The page posts
/// a good deal besides — resize requests, readiness pings — and treating an
/// unrecognised shape as success would hand the API an empty token and fail the
/// login with a confusing error.
fn extract_token(raw: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    // If the message was stringified twice (e.g. raw is a double-quoted JSON string),
    // parse the inner string.
    if let Some(inner) = value.as_str()
        && let Ok(parsed) = serde_json::from_str(inner)
    {
        value = parsed;
    }
    // The payload is nested under `payload` and the message names itself in
    // `type`; both spellings of the success type have shipped.
    let kind = value.get("type")?.as_str()?;
    if !matches!(
        kind,
        "HUMAN_VERIFICATION_SUCCESS" | "human_verification_success"
    ) {
        return None;
    }
    let payload = value.get("payload")?;
    let token = payload.get("token")?.as_str()?;
    if token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Verification, extract_token, find_token, outcome};
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;

    /// The wait status of a process killed by SIGABRT.
    const ABORTED: i32 = 6;

    #[test]
    fn a_completion_message_yields_its_token() {
        let raw =
            r#"{"type":"HUMAN_VERIFICATION_SUCCESS","payload":{"token":"tok-1","type":"captcha"}}"#;
        assert_eq!(extract_token(raw).as_deref(), Some("tok-1"));
    }

    #[test]
    fn a_double_serialized_completion_message_yields_its_token() {
        let raw = r#""{\"type\":\"HUMAN_VERIFICATION_SUCCESS\",\"payload\":{\"token\":\"tok-1\",\"type\":\"captcha\"}}""#;
        assert_eq!(extract_token(raw).as_deref(), Some("tok-1"));
    }

    /// The page posts plenty that is not a completion; none of it may be
    /// mistaken for one, or the login retries with a garbage token.
    #[test]
    fn unrelated_messages_are_ignored() {
        for raw in [
            r#"{"type":"resize","payload":{"height":400}}"#,
            r#"{"type":"HUMAN_VERIFICATION_SUCCESS"}"#,
            r#"{"payload":{"token":"tok"}}"#,
            r#"{"type":"HUMAN_VERIFICATION_SUCCESS","payload":{"token":""}}"#,
            "not json at all",
            "",
        ] {
            assert!(extract_token(raw).is_none(), "accepted: {raw}");
        }
    }

    #[test]
    fn the_token_is_found_among_other_output() {
        let out = "Gtk-Message: something\nhv-token tok-1\nmore\n";
        assert_eq!(find_token(out.as_bytes()).as_deref(), Some("tok-1"));
        assert_eq!(find_token("only chatter\n".as_bytes()), None);
    }

    #[test]
    fn a_window_closed_without_a_token_cancels() {
        assert_eq!(
            outcome(None, ExitStatus::from_raw(0)),
            Verification::Cancelled
        );
    }

    /// WebKit aborts when its sandbox cannot start (B195). That must read as a
    /// failure the app explains, not as the user giving up.
    #[test]
    fn a_window_that_died_fails() {
        assert_eq!(
            outcome(None, ExitStatus::from_raw(ABORTED)),
            Verification::Failed
        );
        let exited_1 = ExitStatus::from_raw(1 << 8);
        assert_eq!(outcome(None, exited_1), Verification::Failed);
    }

    #[test]
    fn a_token_counts_even_if_the_window_died_after_it() {
        assert_eq!(
            outcome(Some("tok-1".into()), ExitStatus::from_raw(ABORTED)),
            Verification::Solved("tok-1".into())
        );
    }
}
