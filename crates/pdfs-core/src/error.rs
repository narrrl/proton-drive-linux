//! Crate-wide error type.

use thiserror::Error;

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
