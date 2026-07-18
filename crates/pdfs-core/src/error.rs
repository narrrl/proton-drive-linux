//! Crate-wide error type.

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
            Error::NotLoggedIn | Error::ReloginRequired => ErrorKind::Denied,
            Error::Proton(_) => ErrorKind::Remote,
            _ => ErrorKind::Internal,
        };
        Self::new(kind, e.to_string())
    }
}

/// The result every request-serving method in the daemon returns.
pub type CoreResult<T> = std::result::Result<T, CoreError>;
