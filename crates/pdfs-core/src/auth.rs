//! Login, session persistence, and Drive-client construction.
//!
//! A successful login is persisted to the OS keyring as a single JSON blob
//! holding the resumable session tokens plus the mailbox password (needed to
//! rebuild the key chain on resume). The daemon resumes from this blob with no
//! interactive step; the refresh token auto-renews via the HTTP client's 401
//! path, so no fresh 2FA is required until the refresh token itself expires.

use keyring::Entry;
use proton_drive_rs::ProtonDriveClient;
use proton_sdk::config::ProtonClientConfiguration;
use proton_sdk::session::{PasswordMode, ProtonApiSession, ResumeParameters};
use serde::{Deserialize, Serialize};

use crate::config::{APP_VERSION, AppDirs, KEYRING_SERVICE, USER_AGENT};
use crate::error::{Error, Result};

/// Fixed keyring account name for the single stored session blob.
const KEYRING_USER: &str = "session";

/// Everything needed to resume a session unattended, persisted to the keyring.
#[derive(Serialize, Deserialize, Clone)]
pub struct StoredSession {
    pub session_id: String,
    pub username: String,
    pub user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub scopes: Vec<String>,
    /// `1` = single password, `2` = dual (Proton wire value).
    pub password_mode: u8,
    /// Mailbox (data) password — required by `ProtonDriveClient::new` to derive
    /// the key chain. Lives only in the OS keyring, never on disk in cleartext.
    pub mailbox_password: String,
}

impl StoredSession {
    fn to_params(&self) -> ResumeParameters {
        ResumeParameters {
            session_id: self.session_id.clone().into(),
            username: self.username.clone(),
            user_id: self.user_id.clone().into(),
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            scopes: self.scopes.clone(),
            is_waiting_for_second_factor_code: false,
            password_mode: match self.password_mode {
                1 => PasswordMode::Single,
                _ => PasswordMode::Dual,
            },
        }
    }
}

fn client_config() -> ProtonClientConfiguration {
    let (app_version, user_agent) = match AppDirs::new() {
        Ok(dirs) => {
            let config = dirs.load_config();
            (config.app_version, config.user_agent)
        }
        Err(_) => (APP_VERSION.to_string(), USER_AGENT.to_string()),
    };
    ProtonClientConfiguration::new(app_version).with_user_agent(user_agent)
}

fn keyring_entry() -> Result<Entry> {
    Ok(Entry::new(KEYRING_SERVICE, KEYRING_USER)?)
}

/// Run an interactive SRP + (optional) 2FA login and persist the session.
///
/// `get_totp` is only invoked when the account requires a second factor, so
/// callers can defer prompting until it is actually needed.
pub async fn login(
    username: &str,
    password: &str,
    get_totp: impl FnOnce() -> Result<String>,
) -> Result<()> {
    let mut session =
        ProtonApiSession::begin(client_config(), username, password.as_bytes()).await?;

    if session.is_waiting_for_second_factor() {
        let code = get_totp()?;
        session.apply_second_factor_code(code.trim()).await?;
    }

    register_refresh_handler(&session, password.to_owned());

    save(&session, password).await?;
    Ok(())
}

/// Persist the session's current tokens + mailbox password to the keyring.
pub async fn save(session: &ProtonApiSession, mailbox_password: &str) -> Result<()> {
    let tokens = session.current_tokens().await;
    let stored = StoredSession {
        session_id: session.session_id().as_str().to_owned(),
        username: session.username().to_owned(),
        user_id: session.user_id().as_str().to_owned(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        scopes: session.scopes().to_vec(),
        password_mode: match session.password_mode() {
            PasswordMode::Single => 1,
            PasswordMode::Dual => 2,
        },
        mailbox_password: mailbox_password.to_owned(),
    };
    let json = serde_json::to_string(&stored)?;
    keyring_entry()?.set_password(&json)?;
    Ok(())
}

/// Load the persisted session blob, or `Error::NotLoggedIn` if absent.
pub fn load() -> Result<StoredSession> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(json) => Ok(serde_json::from_str(&json)?),
        Err(keyring::Error::NoEntry) => Err(Error::NotLoggedIn),
        Err(e) => Err(e.into()),
    }
}

/// Forget the persisted session (best-effort; absent entry is not an error).
pub fn logout() -> Result<()> {
    match keyring_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Resume a persisted session and build an authenticated Drive client.
///
/// Returns `Error::NotLoggedIn` when no session has been saved. The caller is
/// responsible for persisting rotated tokens over the session's lifetime via
/// [`persist`] — Proton refresh tokens are single-use, so a refresh that is not
/// written back leaves the keyring holding a stale (now-invalid) refresh token,
/// and the next resume fails with `InvalidRefreshToken`.
pub async fn resume_client() -> Result<(ProtonDriveClient, ProtonApiSession)> {
    let stored = load()?;
    let session = ProtonApiSession::resume(client_config(), stored.to_params())?;

    register_refresh_handler(&session, stored.mailbox_password.clone());

    let client = ProtonDriveClient::new(&session, stored.mailbox_password.clone().into_bytes());
    Ok((client, session))
}

/// Write the session's current tokens back to the keyring.
///
/// The session does not carry the mailbox password (it is only needed to rebuild
/// the key chain on resume), so it is re-read from the existing stored blob and
/// preserved. Call whenever the session may have rotated its tokens through the
/// 401-refresh path so a later [`resume_client`] presents a live refresh token.
pub async fn persist(session: &ProtonApiSession) -> Result<()> {
    let mailbox_password = load()?.mailbox_password;
    save(session, &mailbox_password).await
}

fn register_refresh_handler(session: &ProtonApiSession, mailbox_password: String) {
    let session_id = session.session_id().as_str().to_owned();
    let username = session.username().to_owned();
    let user_id = session.user_id().as_str().to_owned();
    let scopes = session.scopes().to_vec();
    let password_mode = match session.password_mode() {
        PasswordMode::Single => 1,
        PasswordMode::Dual => 2,
    };

    session.http().set_on_tokens_refreshed(move |tokens| {
        let stored = StoredSession {
            session_id: session_id.clone(),
            username: username.clone(),
            user_id: user_id.clone(),
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            scopes: scopes.clone(),
            password_mode,
            mailbox_password: mailbox_password.clone(),
        };
        if let Ok(json) = serde_json::to_string(&stored)
            && let Ok(entry) = keyring_entry() {
                if let Err(e) = entry.set_password(&json) {
                    tracing::warn!(error = %e, "failed to auto-persist refreshed tokens in keyring");
                } else {
                    tracing::info!("successfully auto-persisted refreshed tokens in keyring");
                }
            }
    });
}
