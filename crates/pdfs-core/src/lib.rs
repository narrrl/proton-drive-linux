//! Core building blocks for the Proton Drive Linux client: static client
//! identity, XDG paths, and the login/session/keyring layer that every
//! front-end (CLI today, daemon + GUI later) shares.

pub mod access;
pub mod auth;
pub mod batch;
pub mod config;
pub mod control;
pub mod error;
pub mod kernelkey;
pub mod keycheck;
pub mod localindex;
pub mod lockrank;
pub mod menu;
pub mod mountfail;
pub mod mountpoint;
pub mod mounts;
pub mod opener;
pub mod profile;
pub mod sdkcache;
pub mod search;
pub mod service;
pub mod shell;
pub mod syncignore;
pub mod syncroot;
pub mod timestamp;
pub mod tray;
pub mod update;

// Photo logic shared with Proton Gallery through the proton-rs crates.
pub use proton_gallery_rs::{exiftime, places, similar, takeout};

pub use access::{Access, access_for, perm_bits};
pub use error::{CoreError, CoreResult, Error, Result};

// Re-export the SDK surface downstream crates need, so they depend on
// `pdfs-core` rather than pinning the SDK directly.
pub use proton_drive_rs::{self, Node, NodeKind, ProtonDriveClient};
pub use proton_sdk;
