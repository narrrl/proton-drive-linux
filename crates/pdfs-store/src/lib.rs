//! The daemon's local state: the SQLite metadata database and the content
//! cache of file blocks on disk.
//!
//! Split out of `pdfs-core`, which keeps configuration, the control protocol and
//! the types every front end shares. Front ends that only talk to the daemon do
//! not depend on this crate.

pub mod cache;
pub mod db;
