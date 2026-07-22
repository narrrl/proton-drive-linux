//! The human-verification (CAPTCHA) dialog.
//!
//! Proton gates logins it does not recognise — a new IP, a VPN exit — behind a
//! CAPTCHA. The API answers the login with a challenge instead of a session, and
//! expects the client to present Proton's hosted verification page, collect the
//! token the user earns by solving it, and retry the login carrying that token.
//!
//! The page is hosted in a `WebKitWebView` rather than handed to the system
//! browser because it reports completion by posting a message to its host, not
//! by redirecting: a browser has nowhere to post it back to.

use crate::*;
use webkit6::prelude::*;

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

/// Present the challenge and hand the solved token back through `token_tx`.
///
/// Closing the dialog without solving drops the sender, which the waiting login
/// worker reads as a cancelled sign-in — the same contract as the 2FA prompt.
pub(crate) fn prompt_human_verification(
    ui: &Rc<Ui>,
    url: &str,
    token_tx: std::sync::mpsc::Sender<String>,
) {
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

    let dialog = adw::Dialog::builder()
        .title("Verification")
        .content_width(420)
        .content_height(560)
        .build();

    let header = adw::HeaderBar::new();
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    body.append(&header);
    body.append(&webview);
    dialog.set_child(Some(&body));

    // Taken on the first success *or* on close, so the token is sent at most
    // once and a cancelled dialog always drops the sender.
    let token_tx = Rc::new(RefCell::new(Some(token_tx)));

    let tx = token_tx.clone();
    let dlg_weak = dialog.downgrade();
    content.connect_script_message_received(Some("hv"), move |_, value| {
        let Some(token) = extract_token(&value.to_str()) else {
            // Every `postMessage` on the page reaches here, most of them the
            // verification app's own chatter. Anything that is not a completion
            // is simply not ours.
            return;
        };
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(token);
        }
        if let Some(dlg) = dlg_weak.upgrade() {
            dlg.close();
        }
    });

    // Covers the close button, Escape, and the user giving up.
    dialog.connect_closed(move |_| {
        token_tx.borrow_mut().take();
    });

    webview.load_uri(url);

    let parent = ui.login.login_button.root().and_downcast::<gtk4::Window>();
    // Returns immediately; the login worker stays blocked on the channel until
    // the handler above sends, or the dialog closes and drops the sender.
    dialog.present(parent.as_ref());
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
    use super::extract_token;

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
}
