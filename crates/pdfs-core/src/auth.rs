//! Login, session persistence, and Drive-client construction.
//!
//! A successful login is persisted to the OS keyring as a single JSON blob
//! holding the resumable session tokens, the account's key salts and — unless
//! the user chose otherwise — the per-key *passphrases* derived from the
//! mailbox password. The password itself is never stored: the passphrases are
//! what it exists to produce, and they are enough to rebuild the key chain on
//! resume. The daemon resumes from this blob with no interactive step; the
//! refresh token auto-renews via the HTTP client's 401 path, so no fresh 2FA is
//! required until the refresh token itself expires.
//!
//! # Two-password accounts
//!
//! An account with a separate mailbox password exists precisely so that no one
//! login holds the means to read the data. Storing that password in the keyring
//! would defeat it, so such an account is **locked by default**: its
//! passphrases are kept only in the kernel keyring ([`crate::kernelkey`]), for
//! the length of the user's session, and `pdfs unlock` (or the app) supplies
//! them again after each login. Storing them in the Secret Service as well is
//! an explicit opt-in ([`Mailbox::remember`]). Single-password accounts have no
//! such separation to protect and are stored, as they always were.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use keyring_core::Entry;
use proton_drive_rs::{KeySalt, ProtonDriveClient};
use proton_sdk::account::KeyPassphrases;
use proton_sdk::api::{HumanVerification, HumanVerificationCredential};
use proton_sdk::cache::EncryptedCacheRepository;
use proton_sdk::config::ProtonClientConfiguration;
use proton_sdk::error::ProtonError;
use proton_sdk::http::Tokens;
use proton_sdk::session::{PasswordMode, ProtonApiSession, ResumeParameters};
use proton_sdk::telemetry::TracingTelemetry;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::{APP_VERSION, AppDirs, KEYRING_SERVICE, USER_AGENT};
use crate::error::{Error, Result};
use crate::kernelkey;

/// How often a wrong mailbox password is asked for again before the login gives up.
const MAILBOX_ATTEMPTS: u32 = 3;

/// Domain-separation label for the key that encrypts the persistent entity cache.
const ENTITY_CACHE_PURPOSE: &[u8] = b"pdfs entity cache v1";

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
    /// The mailbox password of a blob written by 3.0.x and earlier.
    ///
    /// Read only, to migrate: [`resume_client`] derives the passphrases from it
    /// once and writes the blob back without it. Never serialized again.
    #[serde(default, skip_serializing)]
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
    /// Per-key passphrases (key id → base64), when the user keeps them in the
    /// keyring: always for a single-password account, by opt-in for a
    /// two-password one. `None` means they are not stored here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_passphrases: Option<BTreeMap<String, String>>,
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
            (
                config.resolved_app_version().to_string(),
                config.resolved_user_agent().to_string(),
            )
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

/// What the user answered when asked for the mailbox password.
pub struct Mailbox {
    pub password: Zeroizing<String>,
    /// Keep the derived passphrases in the keyring, so the daemon unlocks by
    /// itself after every login. Off by default: it trades the protection a
    /// separate mailbox password gives for convenience.
    pub remember: bool,
}

/// Where the passphrases that unlock this account's keys currently live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// In the keyring, available after every login.
    Stored,
    /// In kernel memory for this login session only.
    Session,
    /// Nowhere: the mailbox password has to be entered (`pdfs unlock`).
    Locked,
}

/// Run an interactive SRP + (optional) 2FA login and persist the session.
///
/// `get_totp` is only invoked when the account requires a second factor, so
/// callers can defer prompting until it is actually needed. It may be called
/// more than once — [`login_interactive`] re-runs the whole login after human
/// verification, and a 2FA account needs a fresh code on that second attempt.
///
/// `get_mailbox` is only invoked for a two-password account. It receives the
/// number of earlier wrong answers (so a prompt can say "try again") and is
/// asked at most [`MAILBOX_ATTEMPTS`] times; an `Err` from it aborts the login.
///
/// A gated login fails with [`Error::HumanVerificationRequired`]; a front-end
/// that can present the challenge should use [`login_interactive`] instead.
pub async fn login(
    username: &str,
    password: &str,
    get_totp: impl Fn() -> Result<String>,
    get_mailbox: impl Fn(u32) -> Result<Mailbox>,
) -> Result<()> {
    login_verified(username, password, None, get_totp, get_mailbox).await
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
    get_mailbox: impl Fn(u32) -> Result<Mailbox>,
    get_hv: impl FnOnce(HumanVerification) -> Result<HumanVerificationCredential>,
) -> Result<()> {
    match login_verified(username, password, None, &get_totp, &get_mailbox).await {
        Err(Error::HumanVerificationRequired(hv)) => {
            let credential = get_hv(*hv)?;
            login_verified(
                username,
                password,
                Some(&credential),
                &get_totp,
                &get_mailbox,
            )
            .await
        }
        other => other,
    }
}

/// Proton's verification page for the user's own browser, for a front-end that
/// cannot host it in a webview.
///
/// Unlike [`HumanVerification::verification_url`] this asks for the full page,
/// not the embedded one: a browser tab has no host to post the token to, so the
/// page has to tell the user they are done instead. Proton Mail Bridge opens the
/// same URL.
pub fn browser_verification_url(hv: &HumanVerification) -> String {
    let token: String = hv
        .token
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!(
        "https://verify.proton.me/?methods={}&token={token}",
        hv.methods.join(","),
    )
}

/// The credential to retry with once the user has solved the challenge in
/// their browser.
///
/// The browser keeps the page's own token, so the retry carries the challenge
/// token instead: solving the page marks that token as verified on Proton's
/// side. The type is every offered method, as Proton Mail Bridge sends it, since
/// the user may have picked any of them on the page.
pub fn browser_verified(hv: &HumanVerification) -> HumanVerificationCredential {
    HumanVerificationCredential {
        token: hv.token.clone(),
        method: hv.methods.join(","),
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
    get_mailbox: impl Fn(u32) -> Result<Mailbox>,
) -> Result<()> {
    let password = Zeroizing::new(password.to_owned());
    let session = ProtonApiSession::begin_verified(
        client_config(),
        username,
        password.as_bytes(),
        verification,
    )
    .await
    .map_err(classify_verification_gate);
    let mut session = session?;

    if session.is_waiting_for_second_factor() {
        let code = get_totp()?;
        session.apply_second_factor_code(code.trim()).await?;
    }

    // Grab the key salts while this access token still has the `locked` scope:
    // after its first refresh it never will again, and without them no later
    // resume can unlock the key chain. See `StoredSession::key_salts`.
    let key_salts = ProtonDriveClient::new(&session, Vec::new())
        .account()
        .key_salts()
        .await?;

    // A single-password account's mailbox password is the login password. A
    // two-password one has a second secret the user must type, and it is checked
    // against the account now — a typo found at login is a retry, found later it
    // is a daemon that never mounts.
    let (passphrases, remember) = match session.password_mode() {
        PasswordMode::Single => (
            derive_verified(&session, password.as_bytes(), key_salts.clone()).await?,
            true,
        ),
        PasswordMode::Dual => {
            let mut attempt = 0;
            loop {
                let mailbox = get_mailbox(attempt)?;
                match derive_verified(&session, mailbox.password.as_bytes(), key_salts.clone())
                    .await
                {
                    Ok(passphrases) => break (passphrases, mailbox.remember),
                    Err(Error::WrongMailboxPassword) if attempt + 1 < MAILBOX_ATTEMPTS => {
                        attempt += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    };

    let tokens = session.current_tokens().await;
    register_refresh_handler(&session, tokens.refresh_token.clone());
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
        mailbox_password: String::new(),
        key_salts,
        key_passphrases: None,
    };
    keep_passphrases(stored, &passphrases, remember)
}

/// Derive the key passphrases from `mailbox` and prove they unlock the account.
///
/// [`Error::WrongMailboxPassword`] means the account answered and the password
/// is wrong; any other error says nothing about the password (no network, an
/// API fault) and must not be reported as a typo.
async fn derive_verified(
    session: &ProtonApiSession,
    mailbox: &[u8],
    key_salts: Vec<KeySalt>,
) -> Result<KeyPassphrases> {
    let client = ProtonDriveClient::with_key_salts(session, mailbox.to_vec(), key_salts);
    client.account().unlock().await.map_err(|e| match e {
        ProtonError::KeysLocked => Error::WrongMailboxPassword,
        other => other.into(),
    })?;
    Ok(client.account().key_passphrases().await?)
}

/// Write `stored` with the passphrases in the place the user chose: the
/// keyring (`remember`) or kernel memory only. Whichever is not chosen is
/// cleared, so a revoked choice never leaves a copy behind.
///
/// The kernel entry is written first. If it cannot be (no keyring support), the
/// secret is *not* quietly stored somewhere weaker: the blob is written locked
/// and the error says so.
fn keep_passphrases(
    mut stored: StoredSession,
    passphrases: &KeyPassphrases,
    remember: bool,
) -> Result<()> {
    let outcome = if remember {
        stored.key_passphrases = Some(encode(passphrases));
        // Best-effort: a system without a kernel keyring has nothing to clear.
        let _ = kernelkey::clear();
        Ok(())
    } else {
        stored.key_passphrases = None;
        kernelkey::store(&encode_secret(passphrases)).map_err(|e| {
            Error::Other(format!(
                "the kernel keyring is unavailable, so the unlock cannot be kept for this session: {e}"
            ))
        })
    };
    write_stored(&stored)?;
    outcome
}

fn encode(passphrases: &KeyPassphrases) -> BTreeMap<String, String> {
    passphrases
        .iter()
        .map(|(id, secret)| (id.to_owned(), BASE64.encode(secret)))
        .collect()
}

fn decode(encoded: &BTreeMap<String, String>) -> Option<KeyPassphrases> {
    let mut passphrases = KeyPassphrases::new();
    for (id, secret) in encoded {
        passphrases.insert(id.clone(), BASE64.decode(secret).ok()?);
    }
    (!passphrases.is_empty()).then_some(passphrases)
}

/// The kernel-keyring payload: the same map as the blob's, as JSON.
fn encode_secret(passphrases: &KeyPassphrases) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(serde_json::to_vec(&encode(passphrases)).unwrap_or_default())
}

fn decode_secret(secret: &[u8]) -> Option<KeyPassphrases> {
    decode(&serde_json::from_slice(secret).ok()?)
}

/// Lift a human-verification gate out of the generic API error into the typed
/// variant a front-end can act on.
///
/// Only a gate that names a solvable method is converted. A `9001` with no
/// `Details`, or one offering only `email`/`sms`, stays a plain API error:
/// promoting it would send the UI to open a verification page it cannot
/// complete, which reads to the user as a hang rather than a refusal.
fn classify_verification_gate(e: ProtonError) -> Error {
    let ProtonError::Api(api) = &e else {
        return e.into();
    };
    match api.human_verification() {
        Some(hv) if hv.supports_captcha() => Error::HumanVerificationRequired(Box::new(hv)),
        _ => e.into(),
    }
}

fn write_stored(stored: &StoredSession) -> Result<()> {
    let json = Zeroizing::new(serde_json::to_string(stored)?);
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

/// Where the passphrases for the stored session live right now.
///
/// Reads the kernel keyring and the stored blob only; never the network.
pub fn key_state() -> Result<KeyState> {
    Ok(key_state_of(&load()?))
}

fn key_state_of(stored: &StoredSession) -> KeyState {
    match find_passphrases(stored) {
        Ok(Some((_, origin))) => origin,
        // A 3.0.x blob still holds its password; `resume_client` migrates it.
        _ if !stored.mailbox_password.is_empty() => KeyState::Stored,
        _ => KeyState::Locked,
    }
}

/// Whether the stored session can be resumed without asking the user:
/// `Ok` when it can, [`Error::NotLoggedIn`] or [`Error::Locked`] when not.
/// What the daemon waits on before it mounts.
pub fn ready() -> Result<()> {
    match key_state_of(&load()?) {
        KeyState::Locked => Err(Error::Locked),
        _ => Ok(()),
    }
}

/// The passphrases the stored session can use without being asked, and which
/// of the two places they came from. Prefers the keyring blob: it is what the
/// user opted into. A corrupt entry is treated as absent, so it reads as
/// locked instead of wedging the daemon.
fn find_passphrases(stored: &StoredSession) -> Result<Option<(KeyPassphrases, KeyState)>> {
    if let Some(found) = stored.key_passphrases.as_ref().and_then(decode) {
        return Ok(Some((found, KeyState::Stored)));
    }
    match kernelkey::load() {
        Ok(Some(secret)) => Ok(decode_secret(&secret).map(|p| (p, KeyState::Session))),
        Ok(None) => Ok(None),
        // No keyring on this system: nothing was ever kept there.
        Err(e) => {
            tracing::debug!(error = %e, "kernel keyring not readable; treating the account as locked");
            Ok(None)
        }
    }
}

/// Supply the mailbox password of a locked account.
///
/// Verifies it against the account, then keeps the derived passphrases in
/// kernel memory for this login session — or, with `remember`, in the keyring.
/// A daemon waiting on [`Error::Locked`] picks them up on its next attempt.
///
/// Fails with [`Error::WrongMailboxPassword`] for a wrong password, which
/// changes nothing, and [`Error::ReloginRequired`] for a session that predates
/// stored key salts.
pub async fn unlock(mailbox: &str, remember: bool) -> Result<()> {
    let stored = load()?;
    if stored.key_salts.is_empty() {
        return Err(Error::ReloginRequired);
    }
    let mailbox = Zeroizing::new(mailbox.to_owned());
    let session = ProtonApiSession::resume(client_config(), stored.to_params())?;
    register_refresh_handler(&session, stored.refresh_token.clone());
    let passphrases =
        derive_verified(&session, mailbox.as_bytes(), stored.key_salts.clone()).await?;
    // Read again: verifying may have rotated the tokens, and the copy read
    // above would put the spent refresh token back.
    keep_passphrases(load()?, &passphrases, remember)
}

/// Drop the passphrases held in kernel memory, so the account is locked again
/// until [`unlock`]. With `forget`, also those in the keyring blob.
pub fn lock(forget: bool) -> Result<()> {
    kernelkey::clear()?;
    if forget {
        let mut stored = load()?;
        if stored.key_passphrases.take().is_some() {
            write_stored(&stored)?;
        }
    }
    Ok(())
}

/// Forget the persisted session (best-effort; absent entry is not an error).
pub fn logout() -> Result<()> {
    // The persisted entity cache describes the account being logged out of. It
    // is encrypted under a key derived from that account's passphrases, but
    // leaving it behind would also mean the next account inherits a store it can
    // only ever read as misses. Best-effort: failing to remove a cache must not
    // block the logout.
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
    if let Err(e) = kernelkey::clear() {
        tracing::warn!(error = %e, "clearing the kernel-keyring unlock on logout failed");
    }
    match keyring_entry()?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Resume a persisted session and build an authenticated Drive client.
///
/// Returns `Error::NotLoggedIn` when no session has been saved and
/// [`Error::Locked`] when the account is waiting for its mailbox password. Tokens
/// the session rotates are written back to the keyring as they happen (see
/// [`register_refresh_handler`]); Proton refresh tokens are single-use, so a
/// refresh that is not written back would leave the keyring holding a stale
/// refresh token, and the next resume would fail with `InvalidRefreshToken`.
///
/// Makes no network call for an account whose passphrases are already known, so
/// a daemon starting offline can still mount from its cache.
pub async fn resume_client() -> Result<(ProtonDriveClient, ProtonApiSession)> {
    let stored = load()?;
    let session = ProtonApiSession::resume(client_config(), stored.to_params())?;
    register_refresh_handler(&session, stored.refresh_token.clone());

    let passphrases = match find_passphrases(&stored)? {
        Some((passphrases, _)) => passphrases,
        None if !stored.mailbox_password.is_empty() => migrate_legacy(&session, &stored).await?,
        None => return Err(Error::Locked),
    };

    let client = tune(
        ProtonDriveClient::with_key_passphrases(&session, passphrases.clone()),
        &passphrases,
    );
    Ok((client, session))
}

/// Turn a blob written by 3.0.x — which holds the mailbox password — into one
/// that holds passphrases, so the password stops being stored.
///
/// The old login saved whatever was typed as the mailbox password, which for a
/// two-password account is the *login* password and unlocks nothing. That is
/// checked here: a password the account rejects leaves the account locked (and
/// the stored copy gone) rather than failing every start forever.
async fn migrate_legacy(
    session: &ProtonApiSession,
    stored: &StoredSession,
) -> Result<KeyPassphrases> {
    let legacy = Zeroizing::new(stored.mailbox_password.clone());

    // Blobs from before key salts were persisted have none: fetch them once
    // more, which only works while the stored access token still carries the
    // `locked` scope. If it doesn't, the account must be logged into again — no
    // refresh can restore that scope.
    let key_salts = if stored.key_salts.is_empty() {
        ProtonDriveClient::new(session, Vec::new())
            .account()
            .key_salts()
            .await
            .map_err(|e| match &e {
                ProtonError::Api(api) if api.is_insufficient_scope() => Error::ReloginRequired,
                _ => e.into(),
            })?
    } else {
        stored.key_salts.clone()
    };

    let verified = derive_verified(session, legacy.as_bytes(), key_salts.clone()).await;
    // Read again: the calls above may have rotated the tokens, and `stored`
    // would put the spent refresh token back.
    let mut migrated = load()?;
    migrated.mailbox_password = String::new();
    migrated.key_salts = key_salts;

    match verified {
        Ok(passphrases) => {
            migrated.key_passphrases = Some(encode(&passphrases));
            write_stored(&migrated)?;
            tracing::info!(
                "migrated the stored session from a mailbox password to key passphrases"
            );
            Ok(passphrases)
        }
        Err(Error::WrongMailboxPassword) => {
            write_stored(&migrated)?;
            tracing::warn!(
                "the stored password does not unlock the account's keys; unlock required"
            );
            Err(Error::Locked)
        }
        Err(e) => Err(e),
    }
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
///   wrapped in the SDK's [`EncryptedCacheRepository`] keyed by a secret derived
///   from the key passphrases, so the decrypted node names it holds are not
///   readable from the file alone — and a password change simply reads as a cold
///   cache, since the SDK treats an undecryptable entry as a miss and clears the
///   store.
///
///   Opening it is best-effort: a store this process cannot open costs cache
///   hits, not the session, so the client falls back to the SDK's in-memory
///   default.
fn tune(client: ProtonDriveClient, passphrases: &KeyPassphrases) -> ProtonDriveClient {
    let client = client
        .with_telemetry(TracingTelemetry::shared())
        .with_small_file_upload(true);
    match AppDirs::new().and_then(|dirs| crate::sdkcache::SdkCache::shared(&dirs)) {
        Ok(cache) => {
            let key = passphrases.derive_secret(ENTITY_CACHE_PURPOSE);
            client.with_entity_repository(EncryptedCacheRepository::shared(cache, key.to_vec()))
        }
        Err(e) => {
            tracing::warn!(error = %e, "persistent SDK entity cache unavailable; using memory");
            client
        }
    }
}

/// Write any rotation of the session's tokens that has not reached the keyring
/// yet. A best-effort last chance before the process exits.
///
/// Rotations are written as they happen (see [`register_refresh_handler`]), so
/// this usually has nothing to do. It never writes the session's current
/// tokens for their own sake: the blob may by now belong to a newer login, or
/// hold a newer rotation, and writing them would put a spent refresh token
/// over a live one.
pub fn persist(session: &ProtonApiSession) -> Result<()> {
    let sink = TOKEN_SINKS
        .lock()
        .get(session.session_id().as_str())
        .cloned();
    match sink {
        Some(sink) => sink.flush(),
        None => Ok(()),
    }
}

/// Where each live session in this process writes its rotated tokens, by
/// session id, for [`persist`] to find.
static TOKEN_SINKS: parking_lot::Mutex<BTreeMap<String, Arc<TokenSink>>> =
    parking_lot::Mutex::new(BTreeMap::new());

/// One session's rotated tokens on their way to the keyring.
///
/// Proton refresh tokens are single-use, so a rotation has to reach the
/// keyring, and it must reach only the blob it belongs to. That is the blob of
/// this session id that still holds the refresh token this session last knew
/// it to hold. Anything else means the blob moved on without this session: a
/// new login, or another session on the same login (`pdfs unlock`) that
/// rotated first. Writing into it would replace a live refresh token with a
/// spent one, and the next start fails with `InvalidRefreshToken` (issue #35).
struct TokenSink {
    session_id: String,
    state: parking_lot::Mutex<SinkState>,
}

struct SinkState {
    /// The refresh token the blob holds for this session, as far as this
    /// session knows: the one it started from, or the last one it wrote.
    stored_refresh: String,
    /// The latest rotation, until the keyring has taken it.
    unsaved: Option<Tokens>,
}

/// What became of a rotation offered to a stored blob.
#[derive(Debug, PartialEq, Eq)]
enum Rotation {
    /// The blob is this session's and took the tokens.
    Taken,
    /// The blob moved on without this session; it is left alone.
    MovedOn,
}

/// Put `tokens` into `stored` if it is still this session's blob: the same
/// session id, holding the refresh token this session last left in it.
fn take_rotation(
    stored: &mut StoredSession,
    session_id: &str,
    stored_refresh: &str,
    tokens: &Tokens,
) -> Rotation {
    if stored.session_id != session_id || stored.refresh_token != stored_refresh {
        return Rotation::MovedOn;
    }
    stored.access_token = tokens.access_token.clone();
    stored.refresh_token = tokens.refresh_token.clone();
    Rotation::Taken
}

impl TokenSink {
    fn new(session_id: &str, stored_refresh: String) -> Self {
        Self {
            session_id: session_id.to_owned(),
            state: parking_lot::Mutex::new(SinkState {
                stored_refresh,
                unsaved: None,
            }),
        }
    }

    /// Take a new rotation and try to write it.
    fn rotated(&self, tokens: Tokens) -> Result<()> {
        self.state.lock().unsaved = Some(tokens);
        self.flush()
    }

    /// Write the unsaved rotation, if there is one. A blob that moved on, or
    /// is gone with a logout, drops it: there is nothing it may still go into.
    fn flush(&self) -> Result<()> {
        let mut state = self.state.lock();
        let Some(tokens) = state.unsaved.clone() else {
            return Ok(());
        };
        let mut stored = match load() {
            Ok(stored) => stored,
            Err(Error::NotLoggedIn) => {
                state.unsaved = None;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match take_rotation(
            &mut stored,
            &self.session_id,
            &state.stored_refresh,
            &tokens,
        ) {
            Rotation::Taken => {
                write_stored(&stored)?;
                state.stored_refresh = tokens.refresh_token;
                tracing::info!("successfully auto-persisted refreshed tokens in keyring");
            }
            Rotation::MovedOn => {
                tracing::info!(
                    "the stored session has moved on; not writing this session's tokens over it"
                );
            }
        }
        state.unsaved = None;
        Ok(())
    }
}

/// Persist rotated tokens the moment the session obtains them.
///
/// `stored_refresh` is the refresh token the keyring holds for this session
/// now. The handler re-reads the stored blob instead of capturing one, so it
/// cannot write back a stale copy over a later unlock or lock, and it writes
/// only into a blob that is still this session's (see [`TokenSink`]), so it
/// cannot resurrect a logged-out session or overwrite a newer login.
fn register_refresh_handler(session: &ProtonApiSession, stored_refresh: String) {
    let session_id = session.session_id().as_str();
    let sink = Arc::new(TokenSink::new(session_id, stored_refresh));
    TOKEN_SINKS
        .lock()
        .insert(session_id.to_owned(), Arc::clone(&sink));
    session.http().set_on_tokens_refreshed(move |tokens| {
        if let Err(e) = sink.rotated(tokens) {
            tracing::warn!(error = %e, "failed to auto-persist refreshed tokens in keyring")
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passphrases() -> KeyPassphrases {
        let mut p = KeyPassphrases::new();
        p.insert("key-a", b"alpha".to_vec());
        p.insert("key-b", vec![0, 255, 7]);
        p
    }

    fn blob(extra: &str) -> String {
        format!(
            r#"{{"session_id":"s","username":"u","user_id":"i","access_token":"a",
            "refresh_token":"r","scopes":[],"password_mode":2{extra}}}"#
        )
    }

    fn challenge() -> HumanVerification {
        serde_json::from_str(
            r#"{"HumanVerificationToken":"ab+c/d=","HumanVerificationMethods":["captcha","email"]}"#,
        )
        .unwrap()
    }

    #[test]
    fn the_browser_gets_the_full_verification_page() {
        assert_eq!(
            browser_verification_url(&challenge()),
            "https://verify.proton.me/?methods=captcha,email&token=ab%2Bc%2Fd%3D"
        );
    }

    /// The browser keeps the page's token, so the retry replays the challenge
    /// itself, typed with every method the page offered.
    #[test]
    fn a_browser_verification_retries_with_the_challenge_token() {
        let credential = browser_verified(&challenge());
        assert_eq!(credential.token, "ab+c/d=");
        assert_eq!(credential.method, "captcha,email");
    }

    #[test]
    fn passphrases_survive_the_blob_encoding() {
        let decoded = decode(&encode(&passphrases())).expect("decodes");
        assert_eq!(decoded, passphrases());
    }

    #[test]
    fn passphrases_survive_the_kernel_payload_encoding() {
        let decoded = decode_secret(&encode_secret(&passphrases())).expect("decodes");
        assert_eq!(decoded, passphrases());
    }

    #[test]
    fn corrupt_or_empty_secrets_read_as_absent() {
        assert!(decode_secret(b"not json").is_none());
        assert!(decode(&BTreeMap::new()).is_none());
        assert!(decode(&BTreeMap::from([("k".into(), "!!!".into())])).is_none());
    }

    #[test]
    fn legacy_blob_is_read_but_its_password_is_never_written_back() {
        let stored: StoredSession =
            serde_json::from_str(&blob(r#","mailbox_password":"hunter2""#)).unwrap();
        assert_eq!(stored.mailbox_password, "hunter2");
        assert!(stored.key_passphrases.is_none());

        let written = serde_json::to_string(&stored).unwrap();
        assert!(!written.contains("hunter2"));
        assert!(!written.contains("mailbox_password"));
    }

    #[test]
    fn locked_blob_has_no_passphrases_field() {
        let stored: StoredSession = serde_json::from_str(&blob("")).unwrap();
        assert!(
            serde_json::to_string(&stored)
                .unwrap()
                .find("key_passphrases")
                .is_none()
        );
    }

    #[test]
    fn stored_passphrases_win_over_the_kernel_keyring() {
        let mut stored: StoredSession = serde_json::from_str(&blob("")).unwrap();
        stored.key_passphrases = Some(encode(&passphrases()));
        let (found, origin) = find_passphrases(&stored).unwrap().unwrap();
        assert_eq!(origin, KeyState::Stored);
        assert_eq!(found, passphrases());
    }

    fn tokens(access: &str, refresh: &str) -> Tokens {
        Tokens {
            access_token: access.into(),
            refresh_token: refresh.into(),
        }
    }

    #[test]
    fn own_session_tokens_are_written() {
        let mut stored: StoredSession = serde_json::from_str(&blob("")).unwrap();
        assert_eq!(
            take_rotation(&mut stored, "s", "r", &tokens("a2", "r2")),
            Rotation::Taken
        );
        assert_eq!(stored.access_token, "a2");
        assert_eq!(stored.refresh_token, "r2");
    }

    /// Issue #35: `pdfs login` stores a new session and restarts the daemon,
    /// and the old daemon's stop wrote its spent tokens into the new blob.
    #[test]
    fn tokens_of_another_session_are_not_written() {
        let mut stored: StoredSession = serde_json::from_str(&blob("")).unwrap();
        assert_eq!(
            take_rotation(&mut stored, "old", "r", &tokens("a2", "r2")),
            Rotation::MovedOn
        );
        assert_eq!(stored.refresh_token, "r");
    }

    /// `pdfs unlock` resumes a second session on the same login. Once it has
    /// rotated, the daemon's copy of the refresh token is spent.
    #[test]
    fn tokens_are_not_written_over_a_newer_rotation_of_the_same_session() {
        let mut stored: StoredSession = serde_json::from_str(&blob("")).unwrap();
        assert_eq!(
            take_rotation(&mut stored, "s", "spent", &tokens("a2", "r2")),
            Rotation::MovedOn
        );
        assert_eq!(stored.refresh_token, "r");
    }

    /// The only test that touches the keyring, through `keyring-core`'s
    /// in-memory store.
    #[test]
    fn a_sink_keeps_writing_its_own_rotations_and_stops_at_a_new_login() {
        keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        let stored = |refresh: &str| {
            serde_json::from_str::<StoredSession>(
                &blob("").replace(r#""r""#, &format!("{refresh:?}")),
            )
            .unwrap()
        };
        write_stored(&stored("r")).unwrap();

        let sink = TokenSink::new("s", "r".into());
        sink.rotated(tokens("a2", "r2")).unwrap();
        sink.rotated(tokens("a3", "r3")).unwrap();
        assert_eq!(load().unwrap().refresh_token, "r3");

        // A new login replaces the blob; the old session's next rotation, and
        // its stop, leave it alone.
        let mut login = stored("fresh");
        login.session_id = "new".into();
        write_stored(&login).unwrap();
        sink.rotated(tokens("a4", "r4")).unwrap();
        sink.flush().unwrap();
        let now = load().unwrap();
        assert_eq!(now.session_id, "new");
        assert_eq!(now.refresh_token, "fresh");
    }
}
