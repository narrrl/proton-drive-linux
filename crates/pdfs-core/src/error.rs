//! Crate-wide error type.

use proton_sdk::api::HumanVerification;
use thiserror::Error;

use crate::control::ErrorKind;

#[derive(Debug, Error)]
pub enum Error {
    #[error("proton api: {0}")]
    Proton(#[from] proton_sdk::ProtonError),

    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),

    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),

    /// No persisted session found — caller must run an interactive login.
    #[error("not logged in (no saved session)")]
    NotLoggedIn,

    /// The stored session can no longer unlock the account key chain: reading
    /// the key salts needs the `locked` scope, which a refreshed access token
    /// does not have. Only a fresh password login restores it.
    #[error("saved session can no longer unlock your keys — run `pdfs login` again")]
    ReloginRequired,

    /// The login was gated behind human verification. Not a failure the user can
    /// fix by retrying: the challenge has to be solved and the login restarted
    /// with the resulting token (see [`auth::login_verified`](crate::auth::login_verified)).
    ///
    /// Carries the challenge rather than just a message so a front-end can
    /// present it; a front-end that cannot (no webview) still has prose to show.
    #[error("sign-in needs human verification — complete the CAPTCHA to continue")]
    HumanVerificationRequired(Box<HumanVerification>),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A failure on its way to a front-end: prose for the user, plus the
/// [`ErrorKind`] the front-end actually branches on.
///
/// This is what the daemon's request-serving methods return, as opposed to
/// [`Error`] above, which is the plumbing error of the `pdfs-core` layer. The
/// two meet at the `From` impls below: a `rusqlite` or `io` failure carries no
/// information a user can act on, so it arrives here as
/// [`ErrorKind::Internal`] and keeps its text only for the log.
///
/// Classification happens where the failure is *understood* — at the API
/// boundary that knows a 404 from a 403, at the path resolver that knows an
/// `ENOENT` — never at the call site, which by then has only a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreError {
    pub kind: ErrorKind,
    pub message: String,
}

impl CoreError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The API is unreachable. Carries fixed wording because there is only ever
    /// one thing to say about it.
    pub fn offline() -> Self {
        Self::new(ErrorKind::Offline, "the Proton Drive API is unreachable")
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, what)
    }

    pub fn denied(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::Denied, what)
    }

    pub fn conflict(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, what)
    }

    pub fn invalid(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, what)
    }

    pub fn remote(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::Remote, what)
    }

    pub fn internal(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, what)
    }

    /// Classify a failure from an SDK call, prefixing `what` for the log.
    ///
    /// Takes `&dyn Error` rather than `&ProtonError` so it reads the same
    /// through the boxes the drain deals in, where the concrete type survives
    /// but the static one does not.
    ///
    /// A failure that is not an API response at all — a transport error, a
    /// timeout, a TLS failure — is what being offline looks like from here, so
    /// it classifies as [`ErrorKind::Offline`] rather than
    /// [`ErrorKind::Remote`]: the request never reached Proton, and telling the
    /// user their connection is down beats telling them the server refused.
    pub fn from_api(e: &(dyn std::error::Error + 'static), what: &str) -> Self {
        let kind = match e.downcast_ref::<proton_sdk::ProtonError>() {
            Some(proton_sdk::ProtonError::Api(api)) => api_kind(api.code),
            // A `ProtonError` that is not an `Api` variant never reached the
            // API; so does anything that is not a `ProtonError` at all.
            _ => ErrorKind::Offline,
        };
        Self::new(kind, format!("{what}: {e}"))
    }

    /// Prefix the message while keeping the classification.
    ///
    /// For adding the caller's context to an already-classified failure — the
    /// kind was decided by whoever understood the error and must survive being
    /// passed up.
    pub fn context(mut self, what: &str) -> Self {
        self.message = format!("{what}: {}", self.message);
        self
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CoreError {}

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::internal(format!("database: {e}"))
    }
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        Self::internal(format!("io: {e}"))
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::internal(format!("serialization: {e}"))
    }
}

impl From<Error> for CoreError {
    fn from(e: Error) -> Self {
        let kind = match &e {
            Error::NotLoggedIn | Error::ReloginRequired | Error::HumanVerificationRequired(_) => {
                ErrorKind::Denied
            }
            Error::Proton(proton_sdk::ProtonError::Api(api)) => api_kind(api.code),
            // A `ProtonError` that never reached the API is a transport failure.
            Error::Proton(_) => ErrorKind::Offline,
            _ => ErrorKind::Internal,
        };
        Self::new(kind, e.to_string())
    }
}

/// What an API response code means to a front-end.
///
/// The grouping is by *what the user can do about it*, which is what
/// [`ErrorKind`] exists to express — not by HTTP-ish similarity. So a 403 and a
/// `NotEnoughPermissions` land together despite being different numbers, while
/// `AlreadyExists` and `DoesNotExist` stay apart despite both being 25xx.
fn api_kind(code: proton_sdk::api::ResponseCode) -> ErrorKind {
    use proton_sdk::api::ResponseCode as C;
    match code {
        // The account may not do this, and repeating it will not help.
        C::Unauthorized
        | C::Forbidden
        | C::NotEnoughPermissions
        | C::NotEnoughPermissionsToGrantPermissions
        | C::InsufficientScope
        | C::IncorrectLoginCredentials
        | C::InvalidRefreshToken
        | C::AccountDeleted
        | C::AccountDisabled
        | C::NoActiveSubscription => ErrorKind::Denied,

        C::DoesNotExist => ErrorKind::NotFound,

        // The remote moved underneath the request; the caller has to decide.
        C::AlreadyExists | C::IncompatibleState => ErrorKind::Conflict,

        // The request was malformed or asks for something structurally
        // impossible. A bug in the caller, not a condition to retry.
        C::InvalidRequirements
        | C::InvalidValue
        | C::InvalidEncryptedIdFormat
        | C::InvalidApp
        | C::TooManyChildren
        | C::NestingTooDeep => ErrorKind::Invalid,

        C::InsufficientQuota | C::InsufficientSpace | C::InsufficientVolumeQuota => {
            ErrorKind::Quota
        }

        // Reached Proton, but it could not answer *now*. Retrying is exactly
        // the right response, so these are Offline rather than Remote — the
        // front-end already words that as "not right now".
        C::RequestTimeout
        | C::Timeout
        | C::TooManyRequests
        | C::ServiceUnavailable
        | C::Offline => ErrorKind::Offline,

        // Reached Proton and it refused for a reason we have no specific
        // handling for. Retryable, because an unchanged retry can legitimately
        // succeed against a transient server-side condition.
        _ => ErrorKind::Remote,
    }
}

/// The result every request-serving method in the daemon returns.
pub type CoreResult<T> = std::result::Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::*;
    use proton_sdk::api::ResponseCode as C;
    use proton_sdk::{ProtonApiError, ProtonError};

    fn api(code: C) -> ProtonError {
        ProtonError::Api(ProtonApiError {
            code,
            http_status: 422,
            message: "nope".into(),
            details: None,
        })
    }

    #[test]
    fn api_codes_classify_by_what_the_user_can_do() {
        assert_eq!(api_kind(C::Forbidden), ErrorKind::Denied);
        assert_eq!(api_kind(C::NotEnoughPermissions), ErrorKind::Denied);
        assert_eq!(api_kind(C::DoesNotExist), ErrorKind::NotFound);
        assert_eq!(api_kind(C::AlreadyExists), ErrorKind::Conflict);
        assert_eq!(api_kind(C::InvalidValue), ErrorKind::Invalid);
        assert_eq!(api_kind(C::InsufficientSpace), ErrorKind::Quota);
        assert_eq!(api_kind(C::TooManyRequests), ErrorKind::Offline);
        // Nothing specific known about it, but Proton did answer.
        assert_eq!(api_kind(C::ProtonDriveUnknown), ErrorKind::Remote);
    }

    /// The regression this whole pass exists to prevent: before it, every SDK
    /// failure was hardcoded to `Remote`, so a 403 asked the user to retry a
    /// request that could never succeed.
    #[test]
    fn a_forbidden_response_is_not_offered_as_retryable() {
        let e = CoreError::from_api(&api(C::Forbidden), "rename");
        assert_eq!(e.kind, ErrorKind::Denied);
        assert!(!e.kind.retryable());
        assert!(e.message.starts_with("rename: "));
    }

    /// Being full is not a transient condition, so retrying an upload that did
    /// not fit must not be offered as a fix.
    #[test]
    fn running_out_of_space_is_not_retryable() {
        assert!(
            !CoreError::from_api(&api(C::InsufficientQuota), "upload")
                .kind
                .retryable()
        );
    }

    /// A failure that never reached the API is the user's connection, not
    /// Proton refusing — the front-end words those differently.
    #[test]
    fn a_transport_failure_reads_as_offline() {
        let io = std::io::Error::other("connection reset");
        assert_eq!(CoreError::from_api(&io, "list").kind, ErrorKind::Offline);
    }

    /// `Error` is the plumbing type; converting it must classify as sharply as
    /// the direct path, or the two disagree about the same failure.
    #[test]
    fn converting_from_the_plumbing_error_classifies_the_api_code() {
        let e: CoreError = Error::Proton(api(C::DoesNotExist)).into();
        assert_eq!(e.kind, ErrorKind::NotFound);
    }
}
