//! Synthetic nodes and naming rules for the mounted shared tree.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use proton_drive_rs::proton_sdk::ids::{LinkId, NodeUid, VolumeId};
use proton_drive_rs::{Node, NodeKind};

pub(crate) const VIRTUAL_VOLUME: &str = "virtual";
pub(crate) const SHARED_WITH_ME_LINK: &str = "sharedwithme";
pub(crate) const SHARED_WITH_ME_BASE: &str = "Shared with me";
const MAX_COMPONENT_BYTES: usize = 255;

pub(crate) fn shared_with_me_uid() -> NodeUid {
    NodeUid::new(
        VolumeId::from(VIRTUAL_VOLUME),
        LinkId::from(SHARED_WITH_ME_LINK),
    )
}

pub(crate) fn is_virtual_uid(uid: &NodeUid) -> bool {
    uid.volume_id.as_str() == VIRTUAL_VOLUME
}

pub(crate) fn is_own_or_virtual_uid(uid: &NodeUid, own_volume: &VolumeId) -> bool {
    uid.volume_id == *own_volume || is_virtual_uid(uid)
}

pub(crate) fn is_primary_root_listing(
    primary: bool,
    folder_uid: &NodeUid,
    primary_root_uid: &NodeUid,
) -> bool {
    primary && folder_uid == primary_root_uid
}

pub(crate) fn virtual_node(parent_uid: NodeUid, name: String, timestamp: i64) -> Node {
    Node {
        uid: shared_with_me_uid(),
        parent_uid: Some(parent_uid),
        name,
        kind: NodeKind::Folder,
        creation_time: timestamp,
        modification_time: timestamp,
        trashed: false,
        is_shared: false,
        is_shared_publicly: false,
        signature_email: None,
        membership: None,
        // A synthetic folder is neither a photo nor an album.
        photo: None,
        album: None,
        verification: Default::default(),
        direct_role: None,
        share_id: None,
    }
}

/// Choose the initial synthetic-root name. Once persisted, `pinned` always wins;
/// a later real collision only controls whether the synthetic dentry is exposed.
pub(crate) fn virtual_root_name(
    real_names: &HashSet<String>,
    pinned: Option<&str>,
) -> (String, bool) {
    if let Some(pinned) = pinned {
        return (pinned.to_string(), !real_names.contains(pinned));
    }
    for n in 0usize.. {
        let candidate = match n {
            0 => SHARED_WITH_ME_BASE.to_string(),
            1 => format!("{SHARED_WITH_ME_BASE} (Proton)"),
            _ => format!("{SHARED_WITH_ME_BASE} (Proton {n})"),
        };
        if !real_names.contains(&candidate) {
            return (candidate, true);
        }
    }
    unreachable!("the numbered virtual-root namespace is unbounded")
}

/// Give every shared root a unique Linux component name, independent of API
/// ordering. Literal suffix-shaped names reserve their basename before duplicate
/// names are numbered, so `Photos (2)` is never stolen by a second `Photos`.
pub(crate) fn disambiguate_shared_names(nodes: &mut [Node]) {
    nodes.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.uid.to_string().cmp(&b.uid.to_string()))
    });
    let reserved: HashSet<String> = nodes
        .iter()
        .map(|node| component_with_suffix(&node.name, ""))
        .collect();
    let mut used = HashSet::with_capacity(nodes.len());

    for node in nodes {
        let original = node.name.clone();
        let basename = component_with_suffix(&original, "");
        let chosen = if used.insert(basename.clone()) {
            basename
        } else {
            let mut n = 2usize;
            loop {
                let suffix = format!(" ({n})");
                let candidate = component_with_suffix(&original, &suffix);
                if !reserved.contains(&candidate) && used.insert(candidate.clone()) {
                    break candidate;
                }
                n += 1;
            }
        };
        node.name = chosen;
    }
}

pub(crate) fn listing_needs_refresh(online: bool, stale: bool) -> bool {
    online && stale
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedListingPlan {
    Resident,
    Persisted,
    Refresh,
}

pub(crate) fn shared_listing_plan(resident: bool, online: bool, stale: bool) -> SharedListingPlan {
    if listing_needs_refresh(online, stale) {
        SharedListingPlan::Refresh
    } else if resident {
        SharedListingPlan::Resident
    } else {
        SharedListingPlan::Persisted
    }
}

pub(crate) fn refresh_generation_is_current(started: u64, current: u64) -> bool {
    started == current
}

#[derive(Default)]
pub(crate) struct SharedRefreshDeadlines {
    deadlines: HashMap<String, Instant>,
}

impl SharedRefreshDeadlines {
    pub(crate) fn is_fresh(&mut self, key: &str, now: Instant) -> bool {
        self.deadlines.retain(|_, deadline| *deadline > now);
        self.deadlines
            .get(key)
            .is_some_and(|deadline| *deadline > now)
    }

    pub(crate) fn mark(&mut self, key: &str, now: Instant, ttl: Duration) {
        self.deadlines.insert(key.to_string(), now + ttl);
    }

    pub(crate) fn clear(&mut self) {
        self.deadlines.clear();
    }
}

fn component_with_suffix(name: &str, suffix: &str) -> String {
    let keep = MAX_COMPONENT_BYTES.saturating_sub(suffix.len());
    let mut boundary = name.len().min(keep);
    while !name.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut out = String::with_capacity(boundary + suffix.len());
    out.push_str(&name[..boundary]);
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(volume: &str, link: &str, name: &str) -> Node {
        let mut node = virtual_node(
            NodeUid::new(VolumeId::from("virtual"), LinkId::from("parent")),
            name.to_string(),
            1,
        );
        node.uid = NodeUid::new(VolumeId::from(volume), LinkId::from(link));
        node
    }

    #[test]
    fn virtual_uid_and_node_round_trip() {
        let parent = NodeUid::new(VolumeId::from("own"), LinkId::from("root"));
        let node = virtual_node(parent.clone(), SHARED_WITH_ME_BASE.into(), 42);
        assert_eq!(node.uid.to_string(), "virtual~sharedwithme");
        assert!(is_virtual_uid(&node.uid));
        assert_eq!(node.parent_uid.as_ref(), Some(&parent));
        assert!(node.is_folder());
        let json = serde_json::to_string(&node).unwrap();
        let restored: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.uid, node.uid);
        assert_eq!(restored.name, SHARED_WITH_ME_BASE);
    }

    #[test]
    fn root_collision_ladder_and_pinned_collision_policy() {
        let names = HashSet::from([
            SHARED_WITH_ME_BASE.to_string(),
            format!("{SHARED_WITH_ME_BASE} (Proton)"),
            format!("{SHARED_WITH_ME_BASE} (Proton 2)"),
        ]);
        assert_eq!(
            virtual_root_name(&names, None),
            (format!("{SHARED_WITH_ME_BASE} (Proton 3)"), true)
        );

        let (name, visible) = virtual_root_name(&names, Some(SHARED_WITH_ME_BASE));
        assert_eq!(name, SHARED_WITH_ME_BASE);
        assert!(!visible, "a real later collision suppresses the dentry");

        let cleared = HashSet::new();
        assert_eq!(
            virtual_root_name(&cleared, Some(SHARED_WITH_ME_BASE)),
            (SHARED_WITH_ME_BASE.to_string(), true),
            "the pinned name returns when the collision clears"
        );
    }

    #[test]
    fn duplicate_names_are_stable_under_reordering_and_reserve_suffixes() {
        let input = vec![
            folder("b", "2", "Photos"),
            folder("c", "3", "Photos (2)"),
            folder("a", "1", "Photos"),
        ];
        let mut forward = input.clone();
        let mut reverse = input;
        reverse.reverse();
        disambiguate_shared_names(&mut forward);
        disambiguate_shared_names(&mut reverse);
        let summarize = |nodes: &[Node]| {
            nodes
                .iter()
                .map(|node| (node.uid.to_string(), node.name.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(summarize(&forward), summarize(&reverse));
        assert_eq!(
            forward
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["Photos", "Photos (3)", "Photos (2)"]
        );
    }

    #[test]
    fn disambiguation_respects_utf8_component_limit() {
        let long = "é".repeat(200);
        let mut nodes = vec![folder("a", "1", &long), folder("b", "2", &long)];
        disambiguate_shared_names(&mut nodes);
        assert!(nodes.iter().all(|node| node.name.len() <= 255));
        assert!(
            nodes
                .iter()
                .all(|node| node.name.is_char_boundary(node.name.len()))
        );
        assert_ne!(nodes[0].name, nodes[1].name);
        assert!(nodes[1].name.ends_with(" (2)"));
    }

    #[test]
    fn ttl_refresh_only_happens_online() {
        assert!(!listing_needs_refresh(false, true));
        assert!(!listing_needs_refresh(true, false));
        assert!(listing_needs_refresh(true, true));
    }

    #[test]
    fn offline_snapshot_suppresses_the_api_and_memory_deadline_suppresses_retries() {
        assert_eq!(
            shared_listing_plan(false, false, true),
            SharedListingPlan::Persisted
        );
        assert_eq!(
            shared_listing_plan(true, true, false),
            SharedListingPlan::Resident
        );

        let now = Instant::now();
        let mut deadlines = SharedRefreshDeadlines::default();
        deadlines.mark("shared", now, Duration::from_secs(60));
        assert!(deadlines.is_fresh("shared", now + Duration::from_secs(30)));
        deadlines.clear();
        assert!(!deadlines.is_fresh("shared", now + Duration::from_secs(30)));
    }

    #[test]
    fn generation_change_rejects_an_inflight_response() {
        assert!(refresh_generation_is_current(7, 7));
        assert!(!refresh_generation_is_current(7, 8));
    }

    #[test]
    fn only_the_primary_inode_space_installs_the_synthetic_root() {
        let root = NodeUid::new(VolumeId::from("own"), LinkId::from("root"));
        assert!(is_primary_root_listing(true, &root, &root));
        assert!(!is_primary_root_listing(false, &root, &root));
        assert!(!is_primary_root_listing(
            true,
            &NodeUid::new(VolumeId::from("own"), LinkId::from("device")),
            &root
        ));
    }

    #[test]
    fn ownership_predicate_excludes_foreign_share_volumes() {
        let own = VolumeId::from("own");
        let own_uid = NodeUid::new(own.clone(), LinkId::from("file"));
        let foreign = NodeUid::new(VolumeId::from("foreign"), LinkId::from("share"));
        assert!(is_own_or_virtual_uid(&own_uid, &own));
        assert!(is_own_or_virtual_uid(&shared_with_me_uid(), &own));
        assert!(!is_own_or_virtual_uid(&foreign, &own));
    }
}
