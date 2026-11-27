//! Rules about the nodes a mount lists that need neither the kernel nor Drive:
//! which of them a folder shows, how queued changes lie over a listing, and
//! what a node made here looks like before Drive has seen it.

use std::collections::{HashMap, HashSet};

use pdfs_core::timestamp::now_secs;
use pdfs_drive::{media_type_for, remote_name_is_safe};
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use tracing::warn;

/// Fabricate the node the server would have returned, for a `create`/`mkdir`
/// that could not reach it. Everything the kernel asks about a fresh node —
/// name, kind, size, times — is knowable locally; the uid is the only invention,
/// and the drain replaces it with the real one.
pub fn local_node(uid: NodeUid, parent_uid: NodeUid, name: String, is_dir: bool) -> Node {
    let now = now_secs();
    Node {
        uid,
        parent_uid: Some(parent_uid),
        kind: if is_dir {
            NodeKind::Folder
        } else {
            NodeKind::File {
                media_type: media_type_for(&name).to_string(),
                total_size_on_storage: 0,
                // No revision has been sealed: nothing has been uploaded yet.
                active_revision_state: None,
                active_revision_id: None,
                content_sha1: None,
                claimed_size: Some(0),
                claimed_modification_time: None,
            }
        },
        name,
        creation_time: now,
        modification_time: now,
        trashed: false,
        is_shared: false,
        is_shared_publicly: false,
        signature_email: None,
        membership: None,
        // A locally fabricated node is on the main volume, never a photo/album.
        photo: None,
        album: None,
        // Nothing signed it: it has never been near the crypto layer.
        verification: Default::default(),
        direct_role: None,
        share_id: None,
    }
}

/// Whether `node` is a file Drive lists without an active revision, as it does
/// one whose create has not been committed.
pub fn is_unrevised(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::File {
            active_revision_id: None,
            ..
        }
    )
}

/// The plaintext size, in bytes, that a node reports.
/// Overwrite each file node's `claimed_size` with the optimistic size of its
/// queued write, where it has one. Folders and nodes with nothing queued are
/// left alone. See [`Core::stamp_pending_sizes`] for why this exists.
pub fn apply_pending_sizes(nodes: &mut [Node], sizes: &HashMap<NodeUid, u64>) {
    for node in nodes {
        if let Some(&len) = sizes.get(&node.uid)
            && let NodeKind::File { claimed_size, .. } = &mut node.kind
        {
            *claimed_size = Some(len as i64);
        }
    }
}

pub fn node_visible(node: &Node, folder_uid: &NodeUid, hidden: &HashSet<NodeUid>) -> bool {
    if !remote_name_is_safe(&node.name) {
        warn!(uid = %node.uid, "a remote name cannot be a local file name; leaving it out");
        return false;
    }
    !node.trashed && node.uid != *folder_uid && !hidden.contains(&node.uid)
}

/// Lay what is queued for a folder over Drive's listing of it, from
/// `Db::queued_children`: a node a queued rename moved away is dropped, and a
/// node a queued create, mkdir or rename put there is listed as the DB has it.
///
/// That node holds its name here: a create Drive already has but whose answer
/// is still on its way, or which the drain is still reading back, is listed by
/// Drive under its new uid and by the queue under its stand-in. Both were
/// listed, and a 128-file folder listed 129 (`docs/BUGS.md` B164).
pub fn overlay_queued(nodes: &mut Vec<Node>, queued: Vec<Node>, gone: &HashSet<String>) {
    let here: HashSet<String> = queued.iter().map(|node| node.uid.to_string()).collect();
    let names: HashSet<&str> = queued.iter().map(|node| node.name.as_str()).collect();
    nodes.retain(|node| {
        let uid = node.uid.to_string();
        !gone.contains(&uid) && !here.contains(&uid) && !names.contains(node.name.as_str())
    });
    nodes.extend(queued);
}

#[cfg(test)]
mod pending_size_tests {
    use super::{Node, NodeKind, NodeUid, apply_pending_sizes};
    use pdfs_drive::node_size;
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};
    use std::collections::HashMap;

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
    }

    fn file(link: &str, claimed: i64) -> Node {
        Node {
            uid: uid(link),
            parent_uid: Some(uid("parent")),
            kind: NodeKind::File {
                media_type: "text/plain".into(),
                total_size_on_storage: 0,
                active_revision_state: None,
                active_revision_id: None,
                content_sha1: None,
                claimed_size: Some(claimed),
                claimed_modification_time: None,
            },
            name: link.into(),
            creation_time: 100,
            modification_time: 100,
            trashed: false,
            is_shared: false,
            is_shared_publicly: false,
            signature_email: None,
            membership: None,
            photo: None,
            album: None,
            verification: Default::default(),
            direct_role: None,
            share_id: None,
        }
    }

    fn folder(link: &str) -> Node {
        Node {
            kind: NodeKind::Folder,
            ..file(link, 0)
        }
    }

    /// B11: a re-enumeration mid-write must not revert the size to the
    /// server's. A file that stats as 0 gets no `read` from the kernel at all,
    /// so the staged blob is never served and the file reads as empty.
    #[test]
    fn a_queued_write_keeps_its_optimistic_size_through_a_re_enumeration() {
        let mut nodes = vec![file("queued", 0), file("settled", 4096)];
        let sizes = HashMap::from([(uid("queued"), 3)]);

        apply_pending_sizes(&mut nodes, &sizes);

        assert_eq!(
            node_size(&nodes[0]),
            3,
            "the remote's pre-write size must not win over the queued write's"
        );
        assert_eq!(
            node_size(&nodes[1]),
            4096,
            "a file with nothing queued keeps the size the server reported"
        );
    }

    /// The map is keyed by uid and says nothing about kind; a folder that
    /// somehow collides must not grow a `claimed_size`.
    #[test]
    fn folders_are_left_alone() {
        let mut nodes = vec![folder("dir")];
        apply_pending_sizes(&mut nodes, &HashMap::from([(uid("dir"), 999)]));
        assert!(matches!(nodes[0].kind, NodeKind::Folder));
        assert_eq!(node_size(&nodes[0]), 0);
    }

    /// The common case: nothing queued, nothing touched.
    #[test]
    fn an_empty_pending_map_changes_nothing() {
        let mut nodes = vec![file("a", 10), file("b", 20)];
        apply_pending_sizes(&mut nodes, &HashMap::new());
        assert_eq!(node_size(&nodes[0]), 10);
        assert_eq!(node_size(&nodes[1]), 20);
    }
}
