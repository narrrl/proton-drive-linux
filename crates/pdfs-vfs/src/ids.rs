//! Inode numbers the tree reserves, and the volume of nodes that only exist
//! here.

use proton_drive_rs::proton_sdk::ids::NodeUid;

/// The root inode of every session. Each session calls its own root 1.
pub const ROOT_INO: u64 = 1;

/// Parent inode for a persisted node whose parent row is missing from the
/// database. No folder carries this inode, so such a node is listed by nobody
/// and stays inert until a live enumeration re-parents it.
pub const ORPHAN_INO: u64 = 0;

/// The volume id of the folders this client makes up, such as "Shared with
/// me". No such volume exists on Drive.
pub const VIRTUAL_VOLUME: &str = "virtual";

/// Whether `uid` names a folder this client made up rather than a Drive node.
pub fn is_virtual_uid(uid: &NodeUid) -> bool {
    uid.volume_id.as_str() == VIRTUAL_VOLUME
}
