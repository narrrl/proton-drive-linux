//! Login, session persistence, and Drive-client construction.
//!
//! A successful login is persisted to the OS keyring as a single JSON blob
//! holding the resumable session tokens plus the mailbox password (needed to
//! rebuild the key chain on resume). The daemon resumes from this blob with no
//! interactive step; the refresh token auto-renews via the HTTP client's 401
//! path, so no fresh 2FA is required until the refresh token itself expires.

use keyring_core::Entry;
use proton_drive_rs::{KeySalt, ProtonDriveClient};
use proton_sdk::api::{HumanVerification, HumanVerificationCredential};
use proton_sdk::cache::EncryptedCacheRepository;
use proton_sdk::config::ProtonClientConfiguration;
use proton_sdk::session::{PasswordMode, ProtonApiSession, ResumeParameters};
use proton_sdk::telemetry::TracingTelemetry;
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
    /// The account's key salts, captured at login.
    ///
    /// `core/v4/keys/salts` requires the `locked` scope, which only the access
    /// token minted by a password login carries: once that token has been
    /// rotated through `auth/v4/refresh` (as it is on every daemon start after
    /// the first), the endpoint answers 403 and the key chain can no longer be
    /// unlocked. Salts only change when the password changes, so store them
    /// once and seed the Drive client with them on resume.
    ///
    /// Empty for blobs written before this field existed; [`resume_client`]
    /// then falls back to fetching (and, if the token is still scoped for it,
    /// backfilling) them.
    #[serde(default)]
    pub key_salts: Vec<KeySalt>,
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
    // Installed lazily rather than once at startup so a session bus that was not
    // up yet on the first attempt is retried on the next one.
    if keyring_core::get_default_store().is_none() {
        keyring_core::set_default_store(dbus_secret_service_keyring_store::Store::new()?);
    }
    Ok(Entry::new(KEYRING_SERVICE, KEYRING_USER)?)
}

/// Run an interactive SRP + (optional) 2FA login and persist the session.
///
/// `get_totp` is only invoked when the account requires a second factor, so
/// callers can defer prompting until it is actually needed. It may be called
/// more than once — [`login_interactive`] re-runs the whole login after human
/// verification, and a 2FA account needs a fresh code on that second attempt.
///
/// A gated login fails with [`Error::HumanVerificationRequired`]; a front-end
/// that can present the challenge should use [`login_interactive`] instead.
pub async fn login(
    username: &str,
    password: &str,
    get_totp: impl Fn() -> Result<String>,
) -> Result<()> {
    login_verified(username, password, None, get_totp).await
}

/// [`login`], able to answer a human-verification gate rather than fail on it.
///
/// `get_hv` is invoked only when the API actually gates the login, mirroring how
/// `get_totp` is only invoked for accounts with a second factor: neither costs a
/// prompt the user did not need. It receives the challenge and returns the token
/// the user earned by solving it.
///
/// The retry lives here rather than in the front-ends because the recovery is
/// not a UI concern: the gated attempt burned its SRP handshake, so the login
/// has to start over with the credential attached, and every front-end would
/// otherwise have to know that. Gating the *retry* is not handled — a second
/// challenge in a row means the token was rejected, and looping on it would trap
/// the user in a CAPTCHA that never clears.
pub async fn login_interactive(
    username: &str,
    password: &str,
    get_totp: impl Fn() -> Result<String>,
    get_hv: impl FnOnce(HumanVerification) -> Result<HumanVerificationCredential>,
) -> Result<()> {
    match login_verified(username, password, None, &get_totp).await {
        Err(Error::HumanVerificationRequired(hv)) => {
            let credential = get_hv(*hv)?;
            login_verified(username, password, Some(&credential), &get_totp).await
        }
        other => other,
    }
}

/// [`login`], replaying a human-verification token the user has already earned.
///
/// A gated login fails with [`Error::HumanVerificationRequired`] carrying the
/// challenge. The front-end presents it, collects the solved token, and calls
/// this. The login starts over from the SRP handshake rather than resuming: the
/// gated attempt never got a session, and its `srp_session` is spent.
pub async fn login_verified(
    username: &str,
    password: &str,
    verification: Option<&HumanVerificationCredential>,
    get_totp: impl Fn() -> Result<String>,
) -> Result<()> {
    let mut session = ProtonApiSession::begin_verified(
        client_config(),
        username,
        password.as_bytes(),
        verification,
    )
    .await
    .map_err(classify_verification_gate)?;

    if session.is_waiting_for_second_factor() {
        let code = get_totp()?;
        session.apply_second_factor_code(code.trim()).await?;
    }

    // Grab the key salts while this access token still has the `locked` scope:
    // after its first refresh it never will again, and without them no later
    // resume can unlock the key chain. See `StoredSession::key_salts`.
    let client = ProtonDriveClient::new(&session, password.as_bytes().to_vec());
    let key_salts = client.account().key_salts().await?;

    register_refresh_handler(&session, password.to_owned(), key_salts.clone());

    save(&session, password, key_salts).await?;
    Ok(())
}

/// Lift a human-verification gate out of the generic API error into the typed
/// variant a front-end can act on.
///
/// Only a gate that names a solvable method is converted. A `9001` with no
/// `Details`, or one offering only `email`/`sms`, stays a plain API error:
/// promoting it would send the UI to open a verification page it cannot
/// complete, which reads to the user as a hang rather than a refusal.
fn classify_verification_gate(e: proton_sdk::error::ProtonError) -> Error {
    let proton_sdk::error::ProtonError::Api(api) = &e else {
        return e.into();
    };
    match api.human_verification() {
        Some(hv) if hv.supports_captcha() => Error::HumanVerificationRequired(Box::new(hv)),
        _ => e.into(),
    }
}

/// Persist the session's current tokens, mailbox password and key salts.
pub async fn save(
    session: &ProtonApiSession,
    mailbox_password: &str,
    key_salts: Vec<KeySalt>,
) -> Result<()> {
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
        key_salts,
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
        Err(keyring_core::Error::NoEntry) => Err(Error::NotLoggedIn),
        Err(e) => Err(e.into()),
    }
}

/// Forget the persisted session (best-effort; absent entry is not an error).
pub fn logout() -> Result<()> {
    // The persisted entity cache describes the account being logged out of. It
    // is encrypted under that account's mailbox password, but leaving it behind
    // would also mean the next account inherits a store it can only ever read as
    // misses. Best-effort: failing to remove a cache must not block the logout.
    if let Ok(dirs) = AppDirs::new() {
        let path = dirs.state_dir().join("sdk_cache.db");
        if let Some(cache) = crate::sdkcache::SdkCache::opened()
            && let Err(e) = cache.clear_now()
        {
            tracing::warn!(error = %e, "clearing the SDK entity cache on logout failed");
        }
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
    match keyring_entry()?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
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
    let password = stored.mailbox_password.clone();

    // Blobs written before key salts were persisted have none: fetch them once
    // more and write them back, which only works while the stored access token
    // still carries the `locked` scope. If it doesn't, the account must be
    // logged into again — no refresh can restore that scope.
    let key_salts = if stored.key_salts.is_empty() {
        register_refresh_handler(&session, password.clone(), Vec::new());
        let probe = ProtonDriveClient::new(&session, password.clone().into_bytes());
        let salts = probe.account().key_salts().await.map_err(|e| match &e {
            proton_sdk::error::ProtonError::Api(api) if api.is_insufficient_scope() => {
                Error::ReloginRequired
            }
            _ => e.into(),
        })?;
        save(&session, &password, salts.clone()).await?;
        salts
    } else {
        stored.key_salts
    };

    register_refresh_handler(&session, password.clone(), key_salts.clone());

    let client = tune(
        ProtonDriveClient::with_key_salts(&session, password.clone().into_bytes(), key_salts),
        password.as_bytes(),
    );
    Ok((client, session))
}

/// The client settings every front-end wants, applied wherever a
/// [`ProtonDriveClient`] is built for real work.
///
/// - **Telemetry** into `tracing`, so the SDK's own spans (transfers, block
///   storage, per-request timings) land in the daemon's log next to the
///   daemon's, instead of being dropped by the default no-op sink.
/// - **Small-file uploads**: a file that fits in one block is uploaded as a
///   single atomic request instead of the multi-step draft/block/commit dance.
///   The SDK makes this opt-in because it has no remote feature-flag provider;
///   the drain queue writes plenty of small files, and the shorter path is one
///   failure point per write instead of three.
/// - **A persistent entity cache** ([`crate::sdkcache`]), so a restart does not
///   re-fetch and re-decrypt the tree the previous run already walked. It is
///   wrapped in the SDK's [`EncryptedCacheRepository`] keyed by the mailbox
///   password, so the decrypted node names it holds are not readable from the
///   file alone — and a password change simply reads as a cold cache, since the
///   SDK treats an undecryptable entry as a miss and clears the store.
///
///   Opening it is best-effort: a store this process cannot open costs cache
///   hits, not the session, so the client falls back to the SDK's in-memory
///   default.
fn tune(client: ProtonDriveClient, mailbox_password: &[u8]) -> ProtonDriveClient {
    let client = client
        .with_telemetry(TracingTelemetry::shared())
        .with_small_file_upload(true);
    match AppDirs::new().and_then(|dirs| crate::sdkcache::SdkCache::shared(&dirs)) {
        Ok(cache) => client.with_entity_repository(EncryptedCacheRepository::shared(
            cache,
            mailbox_password.to_vec(),
        )),
        Err(e) => {
            tracing::warn!(error = %e, "persistent SDK entity cache unavailable; using memory");
            client
        }
    }
}

/// Write the session's current tokens back to the keyring.
///
/// The session does not carry the mailbox password (it is only needed to rebuild
/// the key chain on resume), so it is re-read from the existing stored blob and
/// preserved. Call whenever the session may have rotated its tokens through the
/// 401-refresh path so a later [`resume_client`] presents a live refresh token.
pub async fn persist(session: &ProtonApiSession) -> Result<()> {
    let stored = load()?;
    save(session, &stored.mailbox_password, stored.key_salts).await
}

fn register_refresh_handler(
    session: &ProtonApiSession,
    mailbox_password: String,
    key_salts: Vec<KeySalt>,
) {
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
            key_salts: key_salts.clone(),
        };
        if let Ok(json) = serde_json::to_string(&stored)
            && let Ok(entry) = keyring_entry()
        {
            if let Err(e) = entry.set_password(&json) {
                tracing::warn!(error = %e, "failed to auto-persist refreshed tokens in keyring");
            } else {
                tracing::info!("successfully auto-persisted refreshed tokens in keyring");
            }
        }
    });
}
