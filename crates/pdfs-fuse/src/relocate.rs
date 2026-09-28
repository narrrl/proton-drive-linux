//! Moving a file or folder between two locations without moving its content.
//!
//! `~/ProtonDrive` and every on-demand synced folder are separate FUSE
//! sessions, and a mirror folder is a plain local directory. The kernel refuses
//! `rename(2)` between two mounts with `EXDEV` before the daemon hears of it, so
//! `mv` falls back to copy and delete: every file is downloaded, uploaded again,
//! and the original is trashed.
//!
//! On Proton Drive the same move is one metadata change, because My files and
//! this computer's device folders all live on the main volume. So a move
//! request that names two absolute paths is resolved here, against every
//! location this daemon knows, and done as a server-side move. The local side
//! is then brought level: a mount forgets the node and re-lists, and a mirror
//! folder either renames its copy along or drops it once Drive has it.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use pdfs_core::db::{StoredSyncEntry, StoredSyncFolder};
use pdfs_core::{CoreError, CoreResult};
use proton_drive_rs::NodeKind;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{info, warn};

use super::background::forget_and_notify;
use super::sync::mirror_subtree_unsynced;
use super::{Core, is_local_uid, parse_uid};

/// How long a move waits for a mirror folder's sync pass to finish before it
/// reports the folder busy. A pass can run for hours on a first sync, so the
/// move does not wait it out.
const MIRROR_LOCK_WAIT: Duration = Duration::from_secs(5);

/// Where one side of a move lives.
enum Place {
    /// My files or an on-demand folder: a live inode space. `core` is rooted at
    /// that mount and `rel` is relative to its mountpoint.
    Mounted { core: Box<Core>, rel: PathBuf },
    /// A mirror folder: a local directory kept in step by the sync engine.
    /// `rel` is `/`-separated, relative to the folder, as the baseline keys it.
    Mirror {
        folder: StoredSyncFolder,
        rel: String,
    },
}

impl Place {
    fn mirror_id(&self) -> Option<i64> {
        match self {
            Place::Mirror { folder, .. } => Some(folder.id),
            Place::Mounted { .. } => None,
        }
    }

    fn is_root(&self) -> bool {
        match self {
            Place::Mounted { rel, .. } => rel.as_os_str().is_empty(),
            Place::Mirror { rel, .. } => rel.is_empty(),
        }
    }
}

/// The mirror folder that holds `abs`, and `abs` relative to it. The deepest
/// folder wins, although folders are not expected to nest.
fn mirror_covering(
    folders: Vec<StoredSyncFolder>,
    abs: &Path,
) -> Option<(StoredSyncFolder, String)> {
    let folder = folders
        .into_iter()
        .filter(|f| f.mode == "mirror" && abs.starts_with(&f.local_path))
        .max_by_key(|f| Path::new(&f.local_path).components().count())?;
    let rel = abs.strip_prefix(&folder.local_path).ok()?;
    let parts = rel
        .components()
        .map(|c| match c {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((folder, parts.join("/")))
}

/// `name` inside the folder at `parent`, in baseline form.
fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Whether `uid` is `root` or lies under it, as far as the nodes table knows.
fn under(core: &Core, root: &NodeUid, uid: &str) -> bool {
    let root = root.to_string();
    uid == root || matches!(core.db.path_relative_to(&root, uid), Ok(Some(_)))
}

impl Core {
    /// Move the file or folder at `src` into the folder at `dest_parent`. Both
    /// are absolute local paths and may be in different locations: My files,
    /// an on-demand folder, or a mirror folder. Returns the moved name.
    ///
    /// Within one inode space this is [`Core::move_to`]. Across locations it
    /// is a server-side move followed by local bookkeeping; see the module
    /// documentation. Anything the move could lose is refused up front.
    pub(crate) fn move_between(&self, src: &Path, dest_parent: &Path) -> CoreResult<String> {
        let source = self.locate(src)?;
        let dest = self.locate(dest_parent)?;
        if let (
            Place::Mounted { core, rel },
            Place::Mounted {
                core: dest_core,
                rel: dest_rel,
            },
        ) = (&source, &dest)
            && Arc::ptr_eq(&core.state, &dest_core.state)
        {
            return core.move_to(rel, dest_rel);
        }
        if source.is_root() {
            return Err(CoreError::invalid(format!(
                "{} is the root of a synced location and cannot be moved",
                src.display()
            )));
        }
        if dest_parent.starts_with(src) {
            return Err(CoreError::invalid("cannot move a folder into itself"));
        }
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| CoreError::invalid(format!("{} has no usable name", src.display())))?
            .to_string();

        // Hold every mirror folder involved, lowest id first, so no sync pass
        // sees the move half done and two moves cannot deadlock.
        let mut ids: Vec<i64> = [source.mirror_id(), dest.mirror_id()]
            .into_iter()
            .flatten()
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let locks: Vec<Arc<Mutex<()>>> = ids.iter().map(|&id| self.sync_lock(id)).collect();
        let mut guards = Vec::with_capacity(locks.len());
        for lock in &locks {
            guards.push(lock.try_lock_for(MIRROR_LOCK_WAIT).ok_or_else(|| {
                CoreError::conflict("a synced folder is busy syncing; try again when it is done")
            })?);
        }

        let mut baselines: HashMap<i64, HashMap<String, StoredSyncEntry>> = HashMap::new();
        for &id in &ids {
            let baseline = self
                .db
                .sync_entries(id)
                .map_err(|e| CoreError::internal(format!("db: {e}")))?;
            baselines.insert(id, baseline);
        }

        let src_uid = self.source_uid(&source, &baselines)?;
        let dest_uid = self.dest_uid(&dest, &baselines, &name)?;
        if src_uid.volume_id != dest_uid.volume_id {
            return Err(CoreError::invalid(
                "the two locations are on different Proton Drive volumes; copy it instead",
            ));
        }
        if under(self, &src_uid, &dest_uid.to_string()) {
            return Err(CoreError::invalid("cannot move a folder into itself"));
        }

        // Mirror to mirror renames the local copy along, so it needs nothing
        // proven about it. Another filesystem makes that impossible, and the
        // pair then drops the source copy like any other move out of a mirror.
        let mut renamed = None;
        if let (
            Place::Mirror { folder, rel },
            Place::Mirror {
                folder: dest_folder,
                rel: dest_rel,
            },
        ) = (&source, &dest)
        {
            let from = src.to_path_buf();
            let to = dest_parent.join(&name);
            match std::fs::rename(&from, &to) {
                Ok(()) => renamed = Some((from, to)),
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                    info!(from = %folder.local_path, to = %dest_folder.local_path,
                        "move: mirror folders are on different filesystems");
                }
                Err(e) => {
                    return Err(CoreError::internal(format!(
                        "could not move {} to {}: {e}",
                        rel,
                        join_rel(dest_rel, &name)
                    )));
                }
            }
        }
        if renamed.is_none()
            && let Place::Mirror { folder, rel } = &source
            && let Some(reason) =
                mirror_subtree_unsynced(Path::new(&folder.local_path), rel, &baselines[&folder.id])
        {
            return Err(CoreError::conflict(format!(
                "{name} is not fully synced yet ({reason}); wait for the sync to finish"
            )));
        }

        if let Err(e) = self.rt.block_on(self.client.move_node(&src_uid, &dest_uid)) {
            if let Some((from, to)) = &renamed
                && let Err(back) = std::fs::rename(to, from)
            {
                warn!(from = %to.display(), to = %from.display(), error = %back,
                    "move: could not put the local copy back after the move failed");
            }
            return Err(CoreError::from_api(&e, "move"));
        }

        self.settle_source(&source, &src_uid, renamed.is_some(), &dest, &name, src);
        self.settle_dest(&dest, &dest_uid, &name);
        drop(guards);
        if renamed.is_none()
            && let Place::Mirror { folder, .. } = &dest
        {
            // Nothing local exists yet; the folder's next pass downloads it.
            self.sync_now(Some(folder.id));
        }
        Ok(name)
    }

    /// Which location `abs` is in. A mount is looked for first: a mirror folder
    /// cannot live inside a FUSE mount, and a mount is what the kernel sees.
    fn locate(&self, abs: &Path) -> CoreResult<Place> {
        if let Some((core, rel)) = self.rooted_at(abs) {
            if !rel
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
            {
                return Err(CoreError::invalid(format!(
                    "{} escapes its location",
                    abs.display()
                )));
            }
            return Ok(Place::Mounted {
                core: Box::new(core),
                rel,
            });
        }
        let folders = self
            .db
            .sync_folder_list()
            .map_err(|e| CoreError::internal(format!("db: {e}")))?;
        mirror_covering(folders, abs)
            .map(|(folder, rel)| Place::Mirror { folder, rel })
            .ok_or_else(|| {
                CoreError::invalid(format!(
                    "{} is not in a Proton Drive location",
                    abs.display()
                ))
            })
    }

    /// The node being moved, once everything that must hold before it leaves
    /// its location has been checked.
    fn source_uid(
        &self,
        source: &Place,
        baselines: &HashMap<i64, HashMap<String, StoredSyncEntry>>,
    ) -> CoreResult<NodeUid> {
        match source {
            Place::Mounted { core, rel } => {
                let (ino, uid) = core.resolve(rel)?;
                if is_local_uid(&uid) {
                    return Err(CoreError::conflict(
                        "it is not uploaded yet; wait for the upload to finish",
                    ));
                }
                let old_parent = core.source_parent_uid(ino, rel)?;
                for authority in [&uid, &old_parent] {
                    core.require_uid_writable(authority)
                        .map_err(|e| core.errno_error(e, "move access"))?;
                }
                if self.has_pending_work_under(&uid)? {
                    return Err(CoreError::conflict(
                        "it has changes that are not uploaded yet; wait for the upload to finish",
                    ));
                }
                Ok(uid)
            }
            Place::Mirror { folder, rel } => baselines[&folder.id]
                .get(rel)
                .and_then(|entry| entry.remote_uid.as_deref())
                .and_then(parse_uid)
                .ok_or_else(|| {
                    CoreError::conflict(
                        "it is not on Proton Drive yet; wait for the sync to finish",
                    )
                }),
        }
    }

    /// The folder the node moves into. Refuses one that already holds `name`
    /// on this disk; Drive refuses a clash on its side by itself.
    fn dest_uid(
        &self,
        dest: &Place,
        baselines: &HashMap<i64, HashMap<String, StoredSyncEntry>>,
        name: &str,
    ) -> CoreResult<NodeUid> {
        let uid = match dest {
            Place::Mounted { core, rel } => {
                let (ino, uid) = core.resolve(rel)?;
                let is_folder = core
                    .state()
                    .entries
                    .get(&ino)
                    .is_some_and(|entry| matches!(entry.node.kind, NodeKind::Folder));
                if !is_folder {
                    return Err(CoreError::invalid("the destination is not a folder"));
                }
                core.require_uid_writable(&uid)
                    .map_err(|e| core.errno_error(e, "move access"))?;
                uid
            }
            Place::Mirror { folder, rel } => {
                let local = Path::new(&folder.local_path).join(rel);
                if !local.is_dir() {
                    return Err(CoreError::invalid("the destination is not a folder"));
                }
                if std::fs::symlink_metadata(local.join(name)).is_ok() {
                    return Err(CoreError::conflict(format!(
                        "{name} already exists in the destination"
                    )));
                }
                let uid = if rel.is_empty() {
                    Some(folder.remote_uid.as_str())
                } else {
                    baselines[&folder.id]
                        .get(rel)
                        .and_then(|entry| entry.remote_uid.as_deref())
                };
                uid.and_then(parse_uid).ok_or_else(|| {
                    CoreError::conflict(
                        "the destination is not on Proton Drive yet; wait for the sync to finish",
                    )
                })?
            }
        };
        if is_local_uid(&uid) {
            return Err(CoreError::conflict(
                "the destination is not uploaded yet; wait for the upload to finish",
            ));
        }
        Ok(uid)
    }

    /// Whether the queue owes Drive anything for `uid` or a node under it, or a
    /// file under it is open for writing. Moving such a node away would strand
    /// the pending change against a path that no longer leads to it.
    fn has_pending_work_under(&self, uid: &NodeUid) -> CoreResult<bool> {
        let ops = self
            .db
            .pending_ops()
            .map_err(|e| CoreError::internal(format!("db: {e}")))?;
        if ops.iter().any(|op| {
            under(self, uid, &op.uid)
                || op
                    .parent_uid
                    .as_deref()
                    .is_some_and(|p| under(self, uid, p))
        }) {
            return Ok(true);
        }
        // Collected first so no State lock is held across the nodes-table reads.
        let mut writing = Vec::new();
        self.for_each_state(|st| {
            writing.extend(st.active_writes.values().map(|w| w.uid.to_string()));
        });
        Ok(writing.iter().any(|w| under(self, uid, w)))
    }

    /// Bring the source location level with a move Drive has made.
    fn settle_source(
        &self,
        source: &Place,
        uid: &NodeUid,
        renamed: bool,
        dest: &Place,
        name: &str,
        src: &Path,
    ) {
        match source {
            Place::Mounted { core, rel } => {
                // Forget it everywhere, so no mount keeps serving it under the
                // old path. The content cache is keyed by uid and stays valid.
                self.for_each_mount(|st, notify| {
                    forget_and_notify(st, notify, uid);
                });
                core.invalidate_parent_listing(rel);
            }
            Place::Mirror { folder, rel } if renamed => {
                let Place::Mirror {
                    folder: dest_folder,
                    rel: dest_rel,
                } = dest
                else {
                    unreachable!("a local rename needs a mirror destination");
                };
                if let Err(e) = self.db.sync_entries_move(
                    folder.id,
                    rel,
                    dest_folder.id,
                    &join_rel(dest_rel, name),
                ) {
                    // The next passes see a copy with no baseline on one side
                    // and a baseline with no copy on the other. That uploads
                    // the files again and trashes the moved node, but loses
                    // nothing.
                    warn!(error = ?e, "move: could not carry the sync baseline along");
                }
            }
            Place::Mirror { folder, rel } => {
                // Drive has all of it (checked before the move), so the local
                // copy is only a stale duplicate now.
                if let Err(e) = self.db.sync_entries_remove_subtree(folder.id, rel) {
                    warn!(error = ?e, "move: could not drop the sync baseline");
                }
                let removed = if src.is_dir() {
                    std::fs::remove_dir_all(src)
                } else {
                    std::fs::remove_file(src)
                };
                if let Err(e) = removed {
                    warn!(path = %src.display(), error = %e,
                        "move: could not remove the local copy after the move");
                }
            }
        }
    }

    /// Bring the destination location level with a move Drive has made.
    fn settle_dest(&self, dest: &Place, uid: &NodeUid, name: &str) {
        if let Place::Mounted { .. } = dest {
            self.invalidate_children_of(uid);
            // A lookup that missed before the move may be cached as absent.
            self.for_each_mount(|st, notify| {
                if let Some(&ino) = st.by_uid.get(uid) {
                    notify.inval_entry(ino, name.to_string());
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: i64, local_path: &str, mode: &str) -> StoredSyncFolder {
        StoredSyncFolder {
            id,
            local_path: local_path.to_string(),
            remote_uid: format!("vol~root{id}"),
            remote_share_id: String::new(),
            mode: mode.to_string(),
            pending_mode: None,
            state: "idle".to_string(),
            last_sync: 0,
        }
    }

    #[test]
    fn mirror_covering_finds_the_mirror_folder_and_the_path_inside_it() {
        let folders = vec![folder(1, "/home/u/Docs", "mirror")];
        let (found, rel) = mirror_covering(folders, Path::new("/home/u/Docs/a/b.txt")).unwrap();
        assert_eq!(found.id, 1);
        assert_eq!(rel, "a/b.txt");
    }

    #[test]
    fn mirror_covering_gives_an_empty_path_for_the_folder_itself() {
        let folders = vec![folder(1, "/home/u/Docs", "mirror")];
        let (_, rel) = mirror_covering(folders, Path::new("/home/u/Docs")).unwrap();
        assert_eq!(rel, "");
    }

    #[test]
    fn mirror_covering_prefers_the_deepest_folder() {
        let folders = vec![
            folder(1, "/home/u/Docs", "mirror"),
            folder(2, "/home/u/Docs/Work", "mirror"),
        ];
        let (found, rel) = mirror_covering(folders, Path::new("/home/u/Docs/Work/x")).unwrap();
        assert_eq!(found.id, 2);
        assert_eq!(rel, "x");
    }

    #[test]
    fn mirror_covering_skips_on_demand_folders_and_name_prefixed_siblings() {
        let folders = || {
            vec![
                folder(1, "/home/u/Docs", "mirror"),
                folder(2, "/home/u/Media", "ondemand"),
            ]
        };
        assert!(mirror_covering(folders(), Path::new("/home/u/Media/x")).is_none());
        assert!(mirror_covering(folders(), Path::new("/home/u/Docs2/x")).is_none());
    }

    #[test]
    fn mirror_covering_refuses_a_path_that_climbs_out() {
        let folders = vec![folder(1, "/home/u/Docs", "mirror")];
        assert!(mirror_covering(folders, Path::new("/home/u/Docs/../etc")).is_none());
    }

    #[test]
    fn join_rel_names_a_child_of_the_folder_root_without_a_slash() {
        assert_eq!(join_rel("", "a"), "a");
        assert_eq!(join_rel("x/y", "a"), "x/y/a");
    }
}
