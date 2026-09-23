//! Listing and resolving `(sync-conflict …)` copies for the user.
//!
//! The sweep ([`crate::sweep`]) removes copies it can prove identical and flags
//! the rest once in the activity feed. That leaves divergent copies sitting in
//! folders with nothing but a name to find them by. This module lists them with
//! the file they are a copy of, and resolves one the way the user chooses:
//!
//! - **Keep the original**: trash the copy.
//! - **Keep the copy**: trash the original, then give the copy its name.
//! - **Keep both**: rename the copy to a name of the user's choosing.
//!
//! Every resolution goes through the Proton trash, never a delete, so a wrong
//! choice is undone by restoring from Trash. Resolution refuses to touch a file
//! that is open, being written, or still owed an upload, for the same reason the
//! sweep does: the node is about to change under the decision.
//!
//! Only copies under My Files are handled here. A copy inside a mirror folder is
//! a local file first; the sync engine owns it.

use super::*;
use pdfs_core::control::{ConflictInfo, ConflictKeep};
use sweep::conflict_base_name;

impl Core {
    /// Every conflict copy under My Files, with the file it is a copy of.
    pub(crate) fn list_conflicts(&self) -> CoreResult<Vec<ConflictInfo>> {
        let stored = self
            .db
            .load_all()
            .map_err(|e| CoreError::internal(format!("listing conflicts: {e:?}")))?;
        let nodes: Vec<Node> = stored
            .into_iter()
            .map(|stored| stored.node)
            .filter(|node| !node.trashed && !node.is_folder() && self.is_own_or_virtual(&node.uid))
            .collect();
        let mut by_parent_name: HashMap<(Option<&NodeUid>, &str), &Node> = HashMap::new();
        for node in &nodes {
            by_parent_name.insert((node.parent_uid.as_ref(), node.name.as_str()), node);
        }
        let root = self.primary_root_uid.to_string();
        let mut items = Vec::new();
        for node in &nodes {
            let Some(base) = conflict_base_name(&node.name) else {
                continue;
            };
            let Ok(Some(path)) = self.db.path_relative_to(&root, &node.uid.to_string()) else {
                continue;
            };
            let original = by_parent_name
                .get(&(node.parent_uid.as_ref(), base.as_str()))
                .copied();
            items.push(ConflictInfo {
                original_path: sibling_path(&path, &base),
                original_exists: original.is_some(),
                size: node_size(node),
                modified: node.modification_time,
                original_size: original.map(node_size),
                original_modified: original.map(|o| o.modification_time),
                identical: original.is_some_and(|o| {
                    node_size(o) == node_size(node)
                        && node_content_sha1(node)
                            .is_some_and(|sha| node_content_sha1(o).as_deref() == Some(&sha))
                }),
                path,
            });
        }
        items.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(items)
    }

    /// Resolve the conflict copy at mountpoint-relative `copy`. Returns a line
    /// for the user saying what happened.
    pub(crate) fn resolve_conflict(&self, copy: &Path, keep: &ConflictKeep) -> CoreResult<String> {
        let copy_name = copy
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| CoreError::invalid("the conflict copy has no file name"))?;
        let base = conflict_base_name(copy_name)
            .ok_or_else(|| CoreError::invalid(format!("{copy_name} is not a conflict copy")))?;
        let original = copy.with_file_name(&base);
        self.require_settled(copy)?;
        let message = match keep {
            ConflictKeep::Original => {
                self.delete(copy)?;
                self.log_activity(ActivityKind::Trash, copy_name, format!("kept {base}"), true);
                format!("kept {base}; the copy is in Trash")
            }
            ConflictKeep::Copy => {
                self.require_settled(&original)?;
                self.delete(&original)?;
                // The original is already in Trash. A failed rename leaves the
                // copy under its conflict name, which loses nothing — say so
                // rather than leave the user guessing where their file went.
                self.rename(copy, &base).map_err(|e| {
                    e.context(&format!(
                        "{base} was moved to Trash, but the copy kept its conflict name"
                    ))
                })?;
                self.log_activity(
                    ActivityKind::Rename,
                    &base,
                    format!("kept the conflict copy {copy_name}"),
                    true,
                );
                format!("kept the copy as {base}; the previous version is in Trash")
            }
            ConflictKeep::Both { name } => {
                let name = name.trim();
                if name == base || name == copy_name {
                    return Err(CoreError::invalid("choose a name that no other file has"));
                }
                self.rename(copy, name)?;
                self.log_activity(ActivityKind::Rename, name, format!("was {copy_name}"), true);
                format!("kept both; the copy is now {name}")
            }
        };
        Ok(message)
    }

    /// Refuse to resolve around a file that is open, being written, or still
    /// owed an upload: the decision would be about content that is changing.
    fn require_settled(&self, rel: &Path) -> CoreResult<()> {
        let (_, uid) = self.resolve(rel)?;
        let queued = self.db.has_any_op(&uid.to_string()).unwrap_or(true);
        if queued || self.is_busy(&uid) {
            return Err(CoreError::conflict(format!(
                "{} is still being written or uploaded; try again once it has synced",
                rel.display()
            )));
        }
        self.conflict_notified.lock().remove(&uid);
        Ok(())
    }
}

/// The path of `name` next to the file at `path`.
fn sibling_path(path: &str, name: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_original_sits_next_to_its_copy() {
        assert_eq!(
            sibling_path("a/b/f (sync-conflict 1).txt", "f.txt"),
            "a/b/f.txt"
        );
        assert_eq!(sibling_path("f (sync-conflict 1).txt", "f.txt"), "f.txt");
    }
}
