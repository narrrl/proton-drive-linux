//! The mirror engine's decisions, apart from the I/O that feeds and applies them.
//!
//! [`model`] is what a pass knows about each path, [`planner`] decides what to
//! do with it and refuses what looks like a wipe, [`trace`] records where a pass
//! spent its time, and [`naming`] names conflict copies. None of it touches the
//! disk, the database or Drive, so all of it is tested without a daemon.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use pdfs_core::control::ActivityKind;
use pdfs_core::syncignore::IgnoreRules;
use pdfs_store::db::StoredSyncEntry;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};

pub mod model;
pub mod naming;
pub mod planner;
pub mod trace;

use model::{LocalItem, LocalSig, RemoteItem};
use trace::Call;
