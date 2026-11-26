//! The mirror engine: two-way sync between a local folder and a folder on Drive.
//!
//! [`mirror`] runs the passes. It walks both sides, applies the plan and runs
//! the engine thread; it owns its state and reaches the rest of the daemon only
//! through [`mirror::SyncHost`]. The rest is the engine's decisions, apart from
//! the I/O that feeds and applies them: [`model`] is what a pass knows about
//! each path, [`planner`] decides what to do with it and refuses what looks
//! like a wipe, [`trace`] records where a pass spent its time, and [`naming`]
//! names conflict copies. None of those touch the disk, the database or Drive,
//! so they are tested without a daemon.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use pdfs_core::control::ActivityKind;
use pdfs_core::syncignore::IgnoreRules;
use pdfs_store::db::StoredSyncEntry;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};

pub mod mirror;
pub mod model;
pub mod naming;
pub mod planner;
pub mod trace;

use model::{LocalItem, LocalSig, RemoteItem};
use trace::Call;

/// How often the daemon reads the Drive event feed. A pass trusts the feed's
/// list of changed folders only while reads keep coming at about this pace.
pub const EVENT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
