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

    /// No persisted session found — caller must run an interactive login.
    #[error("not logged in (no saved session)")]
    NotLoggedIn,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
