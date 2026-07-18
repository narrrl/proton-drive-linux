//! Core building blocks for the Proton Drive Linux client: static client
//! identity, XDG paths, and the login/session/keyring layer that every
//! front-end (CLI today, daemon + GUI later) shares.

pub mod auth;
pub mod cache;
pub mod config;
pub mod control;
pub mod db;
pub mod error;
pub mod localindex;
pub mod service;
pub mod shell;

pub use error::{CoreError, CoreResult, Error, Result};

// Re-export the SDK surface downstream crates need, so they depend on
// `pdfs-core` rather than pinning the SDK directly.
pub use proton_drive_rs::{self, Node, NodeKind, ProtonDriveClient};
pub use proton_sdk;
