use super::activity::ACTIVITY_KEEP;
use super::migrations::SCHEMA_VERSION;
use super::*;
use crate::control::{ActivityEntry, ActivityKind};
use crate::localindex::LocalEntry;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};
use serde_json::json;

/// `Db::open` applies the pragmas that `open_in_memory` skips, so the WAL
/// settings can only be checked against a real file.
#[test]
fn open_bounds_the_wal_size() {
    let path = std::env::temp_dir().join(format!("pdfs-db-wal-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path).unwrap();
    let conn = db.conn.lock();

    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    // Without this the WAL is reused in place but never shrinks, so one
    // oversized transaction strands its high-water mark on disk forever.
    let limit: i64 = conn
        .query_row("PRAGMA journal_size_limit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(limit, WAL_SIZE_LIMIT);

    // Without a busy timeout the default is 0: a lock held by anyone else fails
    // the statement instantly instead of waiting for it to clear.
    let busy: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(busy, BUSY_TIMEOUT.as_millis() as i64);

    drop(conn);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A second connection to the same file — a hand-started `pdfs mount` alongside
/// the systemd service, a backup tool, a stray `sqlite3` session — holds a write
/// lock briefly. Our write must wait for it rather than failing on the spot.
///
/// **This passed before the busy timeout was set explicitly**, because rusqlite
/// already applies one. It is kept as a characterisation test, not as evidence
/// of a fix: it states the behaviour the daemon relies on, so that losing it
/// (dropping the pragma, or a dependency changing its default) fails here rather
/// than in front of a user.
#[test]
fn a_write_waits_for_a_competing_writer() {
    use std::time::Duration;

    let path = std::env::temp_dir().join(format!("pdfs-db-busy-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path).unwrap();

    // A second process's connection takes an exclusive write lock and holds it
    // for a moment, as any real transaction would.
    let holder = {
        let path = path.clone();
        std::thread::spawn(move || {
            let other = rusqlite::Connection::open(&path).unwrap();
            other.busy_timeout(Duration::from_secs(5)).unwrap();
            other.execute_batch("BEGIN IMMEDIATE").unwrap();
            std::thread::sleep(Duration::from_millis(300));
            other.execute_batch("COMMIT").unwrap();
        })
    };
    // Let the holder actually acquire the lock before we contend for it.
    std::thread::sleep(Duration::from_millis(50));

    // This is the assertion: it blocks until the holder commits, rather than
    // returning SQLITE_BUSY.
    db.set_state_i64("busy_probe", 7).unwrap();

    holder.join().unwrap();
    assert_eq!(db.state_i64("busy_probe").unwrap(), Some(7));

    drop(db);
    let _ = std::fs::remove_file(&path);
}
use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};

fn uid(link: &str) -> NodeUid {
    NodeUid::new(VolumeId::from("vol"), LinkId::from(link))
}

fn now_test_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// `NodeVerification` is not re-exported, so build test nodes by
// deserializing JSON (the field has a serde default and can be omitted).
fn node_from(parent: serde_json::Value, link: &str, name: &str, kind: serde_json::Value) -> Node {
    let v = serde_json::json!({
        "uid": {"volume_id": "vol", "link_id": link},
        "parent_uid": parent,
        "kind": kind,
        "name": name,
        "creation_time": 100,
        "modification_time": 200,
        "trashed": false,
        "signature_email": null,
    });
    serde_json::from_value(v).unwrap()
}

#[test]
fn photos_replace_keeps_what_was_learned_and_drops_what_left() {
    let db = Db::open_in_memory().unwrap();
    db.photos_replace(&[
        TimelineRow {
            media_type: Some("video/mp4".into()),
            favorite: Some(true),
            ..TimelineRow::new("p1", 300)
        },
        TimelineRow::new("p2", 200),
        TimelineRow::new("p3", 100),
    ])
    .unwrap();

    // A thumbnail attempt teaches us p1's ratio and that p2 can never have one.
    db.photo_set_thumb("p1", THUMB_HAVE, Some(1.5)).unwrap();
    db.photo_set_thumb("p2", THUMB_NONE, None).unwrap();

    // The next refresh brings a new photo, keeps p1 and p2, and loses p3.
    // p1's media type arrives as `None` this time and must not be forgotten.
    db.photos_replace(&[
        TimelineRow {
            name: Some("new.jpg".into()),
            favorite: Some(false),
            ..TimelineRow::new("p0", 400)
        },
        TimelineRow::new("p1", 300),
        TimelineRow::new("p2", 200),
    ])
    .unwrap();

    let page = db.photos_page(0, 10, None, None, false).unwrap();
    assert_eq!(
        page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p0", "p1", "p2"],
        "server order is preserved, and the dropped photo is gone"
    );
    assert_eq!(page[0].name.as_deref(), Some("new.jpg"));
    // Ratios and verdicts cost a download to rediscover: they survive a refresh.
    assert_eq!(page[1].ratio, Some(1.5));
    assert_eq!(page[1].thumb_state, THUMB_HAVE);
    assert_eq!(page[2].thumb_state, THUMB_NONE);
    // A photo we know nothing about yet starts blank.
    assert_eq!(page[0].ratio, None);
    assert_eq!(page[0].thumb_state, THUMB_UNKNOWN);
    // Media type is learned-and-kept like the ratio: p1 keeps the video type
    // it was first seen with even though the later refresh carried `None`, so
    // it stays classified as a video.
    assert_eq!(page[1].kind, crate::control::PhotoKind::Video);

    assert_eq!(db.photos_count().unwrap(), 3);
    // The counts break down by tab, and a filtered page returns only its tab.
    assert_eq!(db.photos_counts().unwrap(), (2, 1, 0));
    let videos = db
        .photos_page(0, 10, Some(crate::control::PhotoKind::Video), None, false)
        .unwrap();
    assert_eq!(
        videos.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p1"]
    );
    let by_uid = db.photos_by_uid(&["p2".into()]).unwrap();
    assert_eq!(by_uid.len(), 1);
    assert_eq!(by_uid[0].capture_time, 200);

    // A date-range page keeps only the window's photos: [150, 350) is p1+p2,
    // not p0 at 400. Combined with a kind filter both conditions apply.
    let ranged = db
        .photos_page(0, 10, None, Some((150, 350)), false)
        .unwrap();
    assert_eq!(
        ranged.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p1", "p2"]
    );
    let ranged_video = db
        .photos_page(
            0,
            10,
            Some(crate::control::PhotoKind::Video),
            Some((150, 350)),
            false,
        )
        .unwrap();
    assert_eq!(
        ranged_video
            .iter()
            .map(|p| p.uid.as_str())
            .collect::<Vec<_>>(),
        ["p1"]
    );
    // All three surviving photos sit in the same (1970-01) local month.
    let months = db.photos_months(None).unwrap();
    assert_eq!(months.len(), 1);
    assert_eq!(months[0].2, 3);
}

#[test]
fn favorites_are_remembered_across_refreshes_and_filter_a_page() {
    let db = Db::open_in_memory().unwrap();
    db.photos_replace(&[
        TimelineRow {
            favorite: Some(true),
            ..TimelineRow::new("p1", 300)
        },
        TimelineRow {
            favorite: Some(false),
            ..TimelineRow::new("p2", 200)
        },
    ])
    .unwrap();
    let favorites = db.photos_page(0, 10, None, None, true).unwrap();
    assert_eq!(
        favorites.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p1"],
        "only the favourited photo is in a favourites page"
    );

    // A local toggle survives a refresh that could not resolve the photo's node
    // (`None`), and loses to one that could.
    db.photos_set_favorite("p2", true).unwrap();
    db.photos_replace(&[
        TimelineRow {
            favorite: Some(false),
            ..TimelineRow::new("p1", 300)
        },
        TimelineRow::new("p2", 200),
    ])
    .unwrap();
    let favorites = db.photos_page(0, 10, None, None, true).unwrap();
    assert_eq!(
        favorites.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p2"],
        "the server's answer wins where it has one; the local flag holds where it doesn't"
    );
    assert!(db.photos_by_uid(&["p2".into()]).unwrap()[0].favorite);
}

#[test]
fn photos_page_slices_the_timeline_in_order() {
    let db = Db::open_in_memory().unwrap();
    let items: Vec<_> = (0..5)
        .map(|i| TimelineRow::new(format!("p{i}"), 500 - i as i64))
        .collect();
    db.photos_replace(&items).unwrap();

    let page = db.photos_page(2, 2, None, None, false).unwrap();
    assert_eq!(
        page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p2", "p3"]
    );
    assert!(db.photos_page(9, 2, None, None, false).unwrap().is_empty());
}

#[test]
fn trash_replace_lists_folders_first() {
    let db = Db::open_in_memory().unwrap();
    db.trash_replace(&[
        StoredTrash {
            uid: "t1".into(),
            name: "zeta.txt".into(),
            is_dir: false,
            size: 10,
            mtime: 1,
            parent_uid: Some("t2".into()),
        },
        StoredTrash {
            uid: "t2".into(),
            name: "Alpha".into(),
            is_dir: true,
            size: 0,
            mtime: 2,
            parent_uid: None,
        },
    ])
    .unwrap();

    let items = db.trash_list().unwrap();
    assert_eq!(
        items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        ["Alpha", "zeta.txt"]
    );

    // The parent links come back with the rows: a restore reads the tree shape
    // from them.
    let mut parents = db.trash_parents().unwrap();
    parents.sort();
    assert_eq!(
        parents,
        [
            ("t1".to_string(), Some("t2".to_string())),
            ("t2".to_string(), None),
        ]
    );

    // A replace is a replace: emptying the trash on the server empties it here.
    db.trash_replace(&[]).unwrap();
    assert!(db.trash_list().unwrap().is_empty());
}

#[test]
fn state_stamps_round_trip_and_clear() {
    let db = Db::open_in_memory().unwrap();
    assert_eq!(db.state_i64("photos_synced_ms").unwrap(), None);
    db.set_state_i64("photos_synced_ms", 1234).unwrap();
    assert_eq!(db.state_i64("photos_synced_ms").unwrap(), Some(1234));
    db.clear_state("photos_synced_ms").unwrap();
    assert_eq!(
        db.state_i64("photos_synced_ms").unwrap(),
        None,
        "a cleared stamp reads as never fetched, so the next request blocks on a refresh"
    );
}

fn folder(link: &str, parent: Option<&str>, name: &str) -> Node {
    let parent = match parent {
        Some(p) => serde_json::json!({"volume_id": "vol", "link_id": p}),
        None => serde_json::Value::Null,
    };
    node_from(parent, link, name, serde_json::json!("Folder"))
}

fn file(link: &str, parent: &str, name: &str, size: i64) -> Node {
    let kind = serde_json::json!({
        "File": {
            "media_type": "text/plain",
            "total_size_on_storage": size + 10,
            "claimed_size": size,
            "claimed_modification_time": null,
        }
    });
    node_from(
        serde_json::json!({"volume_id": "vol", "link_id": parent}),
        link,
        name,
        kind,
    )
}

fn foreign_folder(volume: &str, link: &str, parent: NodeUid, name: &str) -> Node {
    let mut node = folder(link, None, name);
    node.uid = NodeUid::new(VolumeId::from(volume), LinkId::from(link));
    node.parent_uid = Some(parent);
    node
}

/// Recovering the root by uid is what lets the daemon mount offline
/// (offline.md Phase 1): the uid is remembered in `sync_state`, the node
/// itself comes back out of `nodes`.
#[test]
fn node_by_uid_recovers_a_stored_node() {
    let db = Db::open_in_memory().unwrap();
    let root = folder("root", None, "My Files");
    db.upsert_node(&root).unwrap();
    db.set_state_str("root_uid", &root.uid.to_string()).unwrap();

    let key = db.state_str("root_uid").unwrap().unwrap();
    let got = db.node_by_uid(&key).unwrap().expect("root recovered");
    assert_eq!(got.uid, root.uid);
    assert_eq!(got.name, "My Files");
    assert!(got.is_folder());

    assert!(db.node_by_uid("vol~nope").unwrap().is_none());
    assert!(db.state_str("never_written").unwrap().is_none());
}

#[test]
fn upsert_and_load_all_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    let root = folder("root", None, "My Files");
    let child = file("f1", "root", "hello.txt", 42);
    db.upsert_node(&root).unwrap();
    db.upsert_node(&child).unwrap();

    let loaded = db.load_all().unwrap();
    assert_eq!(loaded.len(), 2);
    let f = loaded.iter().find(|s| s.node.uid == uid("f1")).unwrap();
    assert_eq!(f.node.name, "hello.txt");
    assert!(!f.listed);
    match &f.node.kind {
        NodeKind::File { claimed_size, .. } => assert_eq!(*claimed_size, Some(42)),
        _ => panic!("expected file"),
    }
}

#[test]
fn upsert_nodes_and_load_all_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    let root = folder("root", None, "My Files");
    let child1 = file("f1", "root", "hello.txt", 42);
    let child2 = file("f2", "root", "world.txt", 100);
    db.upsert_nodes(&[root, child1, child2]).unwrap();

    let loaded = db.load_all().unwrap();
    assert_eq!(loaded.len(), 3);
    let f1 = loaded.iter().find(|s| s.node.uid == uid("f1")).unwrap();
    assert_eq!(f1.node.name, "hello.txt");
    let f2 = loaded.iter().find(|s| s.node.uid == uid("f2")).unwrap();
    assert_eq!(f2.node.name, "world.txt");
}

#[test]
fn upsert_is_idempotent_update() {
    let db = Db::open_in_memory().unwrap();
    let mut n = folder("root", None, "My Files");
    db.upsert_node(&n).unwrap();
    n.name = "Renamed".into();
    db.upsert_node(&n).unwrap();
    let loaded = db.load_all().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].node.name, "Renamed");
}

#[test]
fn delete_node_removes_row() {
    let db = Db::open_in_memory().unwrap();
    let n = folder("root", None, "My Files");
    db.upsert_node(&n).unwrap();
    db.delete_node(&uid("root")).unwrap();
    assert!(db.load_all().unwrap().is_empty());
}

#[test]
fn completing_trash_atomically_removes_the_op_and_retained_node() {
    let db = Db::open_in_memory().unwrap();
    let node = file("trash-me", "parent", "trash-me.txt", 4);
    let uid = node.uid.clone();
    db.upsert_node(&node).unwrap();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_TRASH.to_string(),
        uid: uid.to_string(),
        parent_uid: None,
        name: Some(node.name.clone()),
        blob_path: None,
        meta_json: None,
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();
    let op = db.pending_ops().unwrap().remove(0);

    db.complete_trash_op(op.id, &uid).unwrap();

    assert!(db.node_by_uid(&uid.to_string()).unwrap().is_none());
    assert!(db.pending_ops().unwrap().is_empty());
}

#[test]
fn children_if_listed_gated_on_flag() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&file("f1", "root", "a.txt", 1)).unwrap();
    db.upsert_node(&file("f2", "root", "b.txt", 2)).unwrap();

    // Not listed yet → unknown, force a re-fetch.
    assert!(db.children_if_listed(&uid("root")).unwrap().is_none());

    db.set_listed(&uid("root"), true).unwrap();
    let kids = db.children_if_listed(&uid("root")).unwrap().unwrap();
    assert_eq!(kids.len(), 2);
}

/// The shape of the `mv`-loses-the-file bug (bugs.md B1): a rename deletes the
/// moved node's row, so a destination left marked `listed` is rebuilt from the DB
/// without it — the file is gone from the source and absent from the destination,
/// with `rename(2)` having reported success. Clearing the flag is what forces the
/// re-enumeration that finds it again.
#[test]
fn a_deleted_child_leaves_a_listed_parent_serving_a_stale_listing() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("dst", Some("root"), "dest"))
        .unwrap();
    db.upsert_node(&file("f1", "root", "a.txt", 1)).unwrap();
    db.set_listed(&uid("dst"), true).unwrap();

    // The rename moved a.txt into `dst` server-side, then forgot the node.
    db.delete_node(&uid("f1")).unwrap();

    // `dst` is still flagged listed, so the DB fast path answers — and the file
    // it was just moved into is nowhere in the result.
    assert!(
        db.children_if_listed(&uid("dst"))
            .unwrap()
            .unwrap()
            .is_empty()
    );

    // Clearing the flag is the only thing that sends the next read to the server.
    db.set_listed(&uid("dst"), false).unwrap();
    assert!(db.children_if_listed(&uid("dst")).unwrap().is_none());
}

#[test]
fn children_if_listed_excludes_trashed() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    let mut trashed = file("f1", "root", "a.txt", 1);
    trashed.trashed = true;
    db.upsert_node(&trashed).unwrap();
    db.set_listed(&uid("root"), true).unwrap();
    assert_eq!(
        db.children_if_listed(&uid("root")).unwrap().unwrap().len(),
        0
    );
}

#[test]
fn shared_root_replacement_tombstones_without_losing_work_or_authority() {
    let db = Db::open_in_memory().unwrap();
    let own_root = folder("root", None, "My Files");
    let virtual_uid = NodeUid::new(VolumeId::from("virtual"), LinkId::from("sharedwithme"));
    let virtual_root = foreign_folder(
        "virtual",
        "sharedwithme",
        own_root.uid.clone(),
        "Shared with me",
    );
    let shared = foreign_folder("foreign", "share", virtual_uid.clone(), "Project");
    let child = foreign_folder("foreign", "child", shared.uid.clone(), "Secret");
    db.upsert_nodes(&[own_root, virtual_root, shared.clone(), child.clone()])
        .unwrap();
    db.set_share_access(&shared.uid, crate::Access::Editor)
        .unwrap();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: child.uid.to_string(),
        parent_uid: Some(shared.uid.to_string()),
        name: Some(child.name.clone()),
        blob_path: Some("/staging/shared-edit".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();
    assert_eq!(db.search("Secret", 10).unwrap().len(), 1);

    let removed = db.publish_shared_roots(&virtual_uid, &[], &[]).unwrap();
    assert_eq!(removed, vec![shared.uid.clone()]);
    assert!(db.visible_children(&virtual_uid).unwrap().is_empty());
    assert!(
        db.node_by_uid(&shared.uid.to_string())
            .unwrap()
            .unwrap()
            .trashed
    );
    assert!(
        db.node_by_uid(&child.uid.to_string())
            .unwrap()
            .unwrap()
            .trashed
    );
    assert!(db.search("Secret", 10).unwrap().is_empty());
    assert_eq!(db.pending_ops().unwrap().len(), 1);
    assert_eq!(
        db.share_access(&shared.uid).unwrap(),
        Some(crate::Access::Viewer)
    );

    let mut reappeared = shared.clone();
    reappeared.trashed = false;
    assert!(
        db.publish_shared_roots(
            &virtual_uid,
            std::slice::from_ref(&shared.uid),
            &[PublishedSharedRoot {
                node: reappeared,
                access: crate::Access::Viewer,
            }],
        )
        .unwrap()
        .is_empty()
    );
    let restored = db.visible_children(&virtual_uid).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].parent_uid.as_ref(), Some(&virtual_uid));
    assert!(!restored[0].trashed);
    assert!(
        db.node_by_uid(&child.uid.to_string())
            .unwrap()
            .unwrap()
            .trashed,
        "descendants remain hidden until their folder is re-enumerated"
    );
    assert_eq!(db.pending_ops().unwrap().len(), 1);
    assert_eq!(
        db.share_access(&shared.uid).unwrap(),
        Some(crate::Access::Viewer),
        "reappearance stays fail-closed until an observed role refresh"
    );
}

#[test]
fn shared_root_node_and_authority_publication_roll_back_together() {
    let db = Db::open_in_memory().unwrap();
    let own_root = folder("root", None, "My Files");
    let virtual_uid = NodeUid::new(VolumeId::from("virtual"), LinkId::from("sharedwithme"));
    let virtual_root = foreign_folder(
        "virtual",
        "sharedwithme",
        own_root.uid.clone(),
        "Shared with me",
    );
    let shared = foreign_folder("foreign", "share", virtual_uid.clone(), "Before");
    db.upsert_nodes(&[own_root, virtual_root, shared.clone()])
        .unwrap();
    db.set_share_access(&shared.uid, crate::Access::Viewer)
        .unwrap();
    db.set_listed(&virtual_uid, false).unwrap();
    db.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER reject_shared_authority
             BEFORE UPDATE ON share_access
             WHEN NEW.root_uid = 'foreign~share'
             BEGIN SELECT RAISE(ABORT, 'injected authority failure'); END;",
        )?;
        Ok(())
    })
    .unwrap();

    let mut changed = shared.clone();
    changed.name = "After".into();
    let result = db.publish_shared_roots(
        &virtual_uid,
        std::slice::from_ref(&shared.uid),
        &[PublishedSharedRoot {
            node: changed,
            access: crate::Access::Editor,
        }],
    );
    assert!(result.is_err());
    assert_eq!(
        db.node_by_uid(&shared.uid.to_string())
            .unwrap()
            .unwrap()
            .name,
        "Before"
    );
    assert_eq!(
        db.share_access(&shared.uid).unwrap(),
        Some(crate::Access::Viewer)
    );
    assert!(db.children_if_listed(&virtual_uid).unwrap().is_none());
}

#[test]
fn accepted_but_unmaterialized_shared_root_keeps_its_snapshot() {
    let db = Db::open_in_memory().unwrap();
    let own_root = folder("root", None, "My Files");
    let virtual_uid = NodeUid::new(VolumeId::from("virtual"), LinkId::from("sharedwithme"));
    let virtual_root = foreign_folder(
        "virtual",
        "sharedwithme",
        own_root.uid.clone(),
        "Shared with me",
    );
    let shared = foreign_folder("foreign", "share", virtual_uid.clone(), "Retained");
    db.upsert_nodes(&[own_root, virtual_root, shared.clone()])
        .unwrap();
    db.set_share_access(&shared.uid, crate::Access::Editor)
        .unwrap();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_RENAME.to_string(),
        uid: shared.uid.to_string(),
        parent_uid: Some(virtual_uid.to_string()),
        name: Some("Retained locally".to_string()),
        blob_path: None,
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();

    let removed = db
        .publish_shared_roots(&virtual_uid, std::slice::from_ref(&shared.uid), &[])
        .unwrap();
    assert!(removed.is_empty());
    assert_eq!(db.visible_children(&virtual_uid).unwrap().len(), 1);
    assert_eq!(db.pending_ops().unwrap().len(), 1);
    assert_eq!(
        db.share_access(&shared.uid).unwrap(),
        Some(crate::Access::Viewer),
        "an accepted root without verified materialized membership fails closed"
    );
}

#[test]
fn foreign_listing_reconciles_authoritative_uids_and_survives_restart_shape() {
    let db = Db::open_in_memory().unwrap();
    let own_root = folder("root", None, "My Files");
    let virtual_uid = NodeUid::new(VolumeId::from("virtual"), LinkId::from("sharedwithme"));
    let virtual_root = foreign_folder(
        "virtual",
        "sharedwithme",
        own_root.uid.clone(),
        "Shared with me",
    );
    let shared = foreign_folder("foreign", "share", virtual_uid, "Shared");
    let kept = foreign_folder("foreign", "kept", shared.uid.clone(), "Kept");
    let removed = foreign_folder("foreign", "removed", shared.uid.clone(), "Removed");
    let deep = foreign_folder("foreign", "deep", removed.uid.clone(), "Deep secret");
    db.upsert_nodes(&[
        own_root,
        virtual_root,
        shared.clone(),
        kept.clone(),
        removed.clone(),
        deep.clone(),
    ])
    .unwrap();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: deep.uid.to_string(),
        parent_uid: Some(removed.uid.to_string()),
        name: Some(deep.name.clone()),
        blob_path: Some("/staging/foreign".into()),
        meta_json: Some("{}".into()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();

    let removed_uids = db
        .publish_foreign_children(&shared.uid, std::slice::from_ref(&kept.uid), &[])
        .unwrap();
    assert_eq!(removed_uids, vec![removed.uid.clone()]);
    let snapshot = db.visible_children(&shared.uid).unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].uid, kept.uid);
    assert!(
        db.node_by_uid(&deep.uid.to_string())
            .unwrap()
            .unwrap()
            .trashed
    );
    assert!(db.search("secret", 10).unwrap().is_empty());
    assert_eq!(db.pending_ops().unwrap().len(), 1);
    assert_eq!(
        db.children_if_listed(&shared.uid).unwrap().unwrap().len(),
        1,
        "restart hydration sees the completed authoritative snapshot"
    );
}

#[test]
fn foreign_delete_tombstones_the_subtree_without_losing_pending_work() {
    let db = Db::open_in_memory().unwrap();
    let own_root = folder("root", None, "My Files");
    let virtual_uid = NodeUid::new(VolumeId::from("virtual"), LinkId::from("sharedwithme"));
    let virtual_root = foreign_folder(
        "virtual",
        "sharedwithme",
        own_root.uid.clone(),
        "Shared with me",
    );
    let shared = foreign_folder("foreign", "share", virtual_uid, "Shared");
    let deleted = foreign_folder("foreign", "deleted", shared.uid.clone(), "Deleted");
    let deep = foreign_folder("foreign", "deep", deleted.uid.clone(), "Searchable deep");
    db.upsert_nodes(&[
        own_root,
        virtual_root,
        shared,
        deleted.clone(),
        deep.clone(),
    ])
    .unwrap();
    db.set_share_access(&deleted.uid, crate::Access::Editor)
        .unwrap();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: deep.uid.to_string(),
        parent_uid: Some(deleted.uid.to_string()),
        name: Some(deep.name.clone()),
        blob_path: Some("/staging/deleted-foreign".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();
    assert_eq!(db.search("Searchable", 10).unwrap().len(), 1);

    db.tombstone_foreign_subtree(&deleted.uid).unwrap();

    for uid in [&deleted.uid, &deep.uid] {
        assert!(db.node_by_uid(&uid.to_string()).unwrap().unwrap().trashed);
    }
    assert!(db.search("Searchable", 10).unwrap().is_empty());
    assert_eq!(db.pending_ops().unwrap().len(), 1);
    assert_eq!(
        db.share_access(&deleted.uid).unwrap(),
        Some(crate::Access::Viewer)
    );
}

#[test]
fn hidden_virtual_root_removes_descendants_from_fts_and_restores_them() {
    let db = Db::open_in_memory().unwrap();
    let own_root = folder("root", None, "My Files");
    let virtual_uid = NodeUid::new(VolumeId::from("virtual"), LinkId::from("sharedwithme"));
    let mut virtual_root = foreign_folder(
        "virtual",
        "sharedwithme",
        own_root.uid.clone(),
        "Shared with me",
    );
    let shared = foreign_folder("foreign", "share", virtual_uid, "Shared");
    let child = foreign_folder(
        "foreign",
        "child",
        shared.uid.clone(),
        "Findable descendant",
    );
    db.upsert_node(&own_root).unwrap();
    assert!(
        db.publish_virtual_root("shared_with_me_name", &virtual_root)
            .unwrap()
    );
    db.upsert_nodes(&[shared, child.clone()]).unwrap();
    assert_eq!(db.search("Findable", 10).unwrap().len(), 1);

    virtual_root.trashed = true;
    assert!(
        db.publish_virtual_root("shared_with_me_name", &virtual_root)
            .unwrap()
    );
    assert!(db.search("Findable", 10).unwrap().is_empty());
    assert!(
        !db.node_by_uid(&child.uid.to_string())
            .unwrap()
            .unwrap()
            .trashed,
        "valid descendants are hidden by ancestry, not revoked"
    );

    virtual_root.trashed = false;
    assert!(
        db.publish_virtual_root("shared_with_me_name", &virtual_root)
            .unwrap()
    );
    assert_eq!(db.search("Findable", 10).unwrap().len(), 1);
    let before = db.with_conn(|conn| Ok(conn.total_changes())).unwrap();
    assert!(
        !db.publish_virtual_root("shared_with_me_name", &virtual_root)
            .unwrap()
    );
    let after = db.with_conn(|conn| Ok(conn.total_changes())).unwrap();
    assert_eq!(
        before, after,
        "unchanged root lookup performs no DB/FTS writes"
    );
}

#[test]
fn shared_root_pinned_name_survives_database_reopen() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-shared-name-{}-{}.db",
        std::process::id(),
        now_test_id()
    ));
    {
        let db = Db::open(&path).unwrap();
        db.set_state_str("shared_with_me_name", "Shared with me (Proton 4)")
            .unwrap();
    }
    let reopened = Db::open(&path).unwrap();
    assert_eq!(
        reopened
            .state_str("shared_with_me_name")
            .unwrap()
            .as_deref(),
        Some("Shared with me (Proton 4)")
    );
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

/// Two writers on one database is the failure mode the single-instance lock
/// exists to prevent — a hand-run `pdfs mount` next to the systemd unit gives
/// two daemons the same inode space, content cache, and drain queue. The second
/// open has to fail, and it has to stop failing once the first handle is gone.
#[test]
fn a_second_writer_is_refused_while_the_first_holds_the_database() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-single-writer-{}-{}.db",
        std::process::id(),
        now_test_id()
    ));
    let first = Db::open(&path).unwrap();
    let second = Db::open(&path);
    assert!(second.is_err(), "a second writer was allowed in");

    drop(first);
    // The lock lives in the kernel and dies with the descriptor, so nothing has
    // to be cleaned up for the next daemon to start.
    Db::open(&path).expect("reopen after the first handle is dropped");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db.lock"));
}

/// `cache.db` is derived state — every row can be re-fetched or re-scanned — so
/// a file that is not a database at all must not stop the daemon starting. It
/// is moved aside (not deleted: it is the only evidence) and rebuilt.
#[test]
fn a_corrupt_database_is_moved_aside_and_rebuilt() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-corrupt-{}-{}.db",
        std::process::id(),
        now_test_id()
    ));
    std::fs::write(&path, b"this is not an SQLite database").unwrap();

    let db = Db::open(&path).unwrap();
    db.set_state_str("shared_with_me_name", "rebuilt").unwrap();
    assert_eq!(
        db.state_str("shared_with_me_name").unwrap().as_deref(),
        Some("rebuilt")
    );
    drop(db);

    let dir = path.parent().unwrap();
    let stem = path.file_name().unwrap().to_string_lossy().to_string();
    let quarantined: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(&format!("{stem}.corrupt-")))
        .collect();
    assert_eq!(quarantined.len(), 1, "the damaged file was not kept");

    for name in quarantined {
        let _ = std::fs::remove_file(dir.join(name));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db.lock"));
}

fn local(path: &str, name: &str, is_dir: bool) -> LocalEntry {
    LocalEntry {
        path: path.into(),
        name: name.into(),
        is_dir,
        size: 10,
        mtime: 5,
    }
}

/// A local scan is searchable by substring, and a *later* scan prunes the
/// paths it no longer sees — including out of the FTS index, so a deleted
/// file cannot keep surfacing in the prompt.
#[test]
fn local_scan_indexes_then_prunes_stale_paths() {
    let db = Db::open_in_memory().unwrap();

    let gen1 = db.local_begin_scan().unwrap();
    db.local_upsert_batch(
        gen1,
        &[
            local("/home/u/docs/report.pdf", "report.pdf", false),
            local("/home/u/docs/notes.md", "notes.md", false),
        ],
    )
    .unwrap();
    assert_eq!(db.local_finish_scan(gen1, 1_000).unwrap(), 2);

    // Trigram index gives substring (not just prefix) matches.
    let hits = db.search_local("port", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "/home/u/docs/report.pdf");
    assert!(!hits[0].is_dir);
    assert_eq!(db.local_indexed_at().unwrap(), Some(1_000));

    // Second scan sees only notes.md → report.pdf is gone from disk.
    let gen2 = db.local_begin_scan().unwrap();
    db.local_upsert_batch(gen2, &[local("/home/u/docs/notes.md", "notes.md", false)])
        .unwrap();
    assert_eq!(db.local_finish_scan(gen2, 2_000).unwrap(), 1);
    assert!(db.search_local("report", 10).unwrap().is_empty());
    assert_eq!(db.search_local("notes", 10).unwrap().len(), 1);
}

/// The index is maintained incrementally rather than rebuilt at the end of a
/// scan, so the case that used to be covered for free — a file the scan sees
/// again — needs its own test: a re-scan must neither lose the entry nor
/// duplicate it.
#[test]
fn rescanning_an_unchanged_file_keeps_exactly_one_index_entry() {
    let db = Db::open_in_memory().unwrap();
    for (generation, mtime) in [(1_000, 5), (2_000, 9)] {
        let g = db.local_begin_scan().unwrap();
        let mut e = local("/home/u/docs/report.pdf", "report.pdf", false);
        e.mtime = mtime;
        db.local_upsert_batch(g, &[e]).unwrap();
        assert_eq!(db.local_finish_scan(g, generation).unwrap(), 1);
        let hits = db.search_local("report", 10).unwrap();
        assert_eq!(hits.len(), 1, "generation {generation}");
        assert_eq!(hits[0].mtime, mtime);
    }
}

/// Queries below the trigram minimum still match, via the `LIKE` fallback.
#[test]
fn local_search_short_query_like_fallback() {
    let db = Db::open_in_memory().unwrap();
    let g = db.local_begin_scan().unwrap();
    db.local_upsert_batch(g, &[local("/home/u/a.txt", "a.txt", false)])
        .unwrap();
    db.local_finish_scan(g, 1).unwrap();
    assert_eq!(db.search_local("a", 10).unwrap().len(), 1);
    assert!(db.search_local("", 10).unwrap().is_empty());
}

#[test]
fn event_cursor_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    // Absent before first write → seed from server head on first mount.
    assert!(db.get_event_cursor().unwrap().is_none());
    db.set_event_cursor("evt-1").unwrap();
    assert_eq!(db.get_event_cursor().unwrap().as_deref(), Some("evt-1"));
    // Overwrites, not appends.
    db.set_event_cursor("evt-2").unwrap();
    assert_eq!(db.get_event_cursor().unwrap().as_deref(), Some("evt-2"));
}

#[test]
fn search_trigram_substring_and_path() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("docs", Some("root"), "Documents"))
        .unwrap();
    db.upsert_node(&file("f1", "docs", "report.pdf", 1))
        .unwrap();
    db.upsert_node(&file("f2", "root", "notes.txt", 2)).unwrap();
    db.upsert_node(&file(
        "f3",
        "root",
        "Rampage Open Air 2026 - order 166765244.pdf",
        3,
    ))
    .unwrap();

    // Substring match (trigram), not just prefix.
    let hits = db.search("port", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.name, "report.pdf");
    // Path is mountpoint-relative, root excluded.
    assert_eq!(hits[0].path, "Documents/report.pdf");

    // Top-level file → bare name.
    let hits = db.search("notes", 10).unwrap();
    assert_eq!(hits[0].path, "notes.txt");

    // Multi-term FTS5 query matching (out of order, separated terms)
    let hits = db.search("rampage 2026", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].node.name,
        "Rampage Open Air 2026 - order 166765244.pdf"
    );
}

#[test]
fn search_excludes_trashed_and_respects_limit() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&file("f1", "root", "alpha.txt", 1)).unwrap();
    let mut gone = file("f2", "root", "alphb.txt", 1);
    gone.trashed = true;
    db.upsert_node(&gone).unwrap();

    let hits = db.search("alph", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.name, "alpha.txt");

    db.upsert_node(&file("f3", "root", "alphc.txt", 1)).unwrap();
    assert_eq!(db.search("alph", 1).unwrap().len(), 1);
}

/// A device folder's root is stored in `device`/`sync_folder`, never in
/// `nodes`, so its children's `parent_uid` points at a row that does not exist.
/// Requiring the ancestor walk to reach a `parent_uid IS NULL` row therefore
/// excluded every device-folder subtree from search: 2,705 of 9,252 nodes on
/// the account this was found on, including everything under `~/Downloads`.
#[test]
fn search_finds_nodes_below_an_uncached_parent() {
    let db = Db::open_in_memory().unwrap();
    // "device-root" is deliberately never upserted.
    db.upsert_node(&folder("downloads", Some("device-root"), "Downloads"))
        .unwrap();
    db.upsert_node(&file("t", "downloads", "tickets.pdf", 1))
        .unwrap();

    let hits = db.search("tickets", 10).unwrap();
    assert_eq!(hits.len(), 1, "a device folder's files must be searchable");
    assert_eq!(hits[0].path, "Downloads/tickets.pdf");
}

/// The relaxed rule must not also start indexing trash: a node whose *parent*
/// is trashed stays out even though the walk now terminates happily.
#[test]
fn search_excludes_a_node_under_a_trashed_ancestor() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    let mut folder_gone = folder("boxed", Some("root"), "Boxed");
    folder_gone.trashed = true;
    db.upsert_node(&folder_gone).unwrap();
    db.upsert_node(&file("f1", "boxed", "buried.txt", 1))
        .unwrap();

    assert!(db.search("buried", 10).unwrap().is_empty());
}

/// A parent cycle is corrupt data the API can still hand us. It must stay out
/// of the index, and — because the short-query `LIKE` lane reads `nodes`
/// directly and never consults the index — resolving one's path must terminate
/// rather than spin inside the daemon's only SQLite connection.
#[test]
fn a_parent_cycle_is_unindexed_and_never_hangs_a_search() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("a", Some("b"), "Alphaville"))
        .unwrap();
    db.upsert_node(&folder("b", Some("a"), "Betamax")).unwrap();

    // Index lane: the cycle is not indexed, so it cannot be found.
    assert!(db.search("alphaville", 10).unwrap().is_empty());
    assert!(db.search("betamax", 10).unwrap().is_empty());

    // LIKE lane (below the trigram minimum): the rows are reachable, and what
    // matters is that resolving their paths returns at all.
    let hits = db.search("al", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].path.ends_with("Alphaville"));
}

#[test]
fn search_short_query_like_fallback() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&file("f1", "root", "ab.txt", 1)).unwrap();
    // Under trigram min length → LIKE path still finds it.
    let hits = db.search("ab", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.name, "ab.txt");
}

#[test]
fn fuzzy_candidates_include_typo_and_parent_path_matches() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("projects", Some("root"), "Projects"))
        .unwrap();
    db.upsert_node(&file("report", "projects", "quarterly-report.pdf", 1))
        .unwrap();
    db.upsert_node(&file("video", "projects", "video.mp4", 1))
        .unwrap();

    let typo = db.search_candidates("quartrly", 20).unwrap();
    assert!(
        typo.iter()
            .any(|hit| hit.node.name == "quarterly-report.pdf")
    );
    assert!(
        db.search_candidates("vedio", 20)
            .unwrap()
            .iter()
            .any(|hit| hit.node.name == "video.mp4")
    );

    let path = db.search_candidates("projects", 20).unwrap();
    assert!(
        path.iter()
            .any(|hit| hit.node.name == "quarterly-report.pdf")
    );
}

#[test]
fn renaming_parent_refreshes_descendant_search_paths() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("folder", Some("root"), "zzbeforezz"))
        .unwrap();
    db.upsert_node(&file("child", "folder", "plain.txt", 1))
        .unwrap();
    assert!(
        db.search_candidates("zzbeforezz", 20)
            .unwrap()
            .iter()
            .any(|h| h.node.name == "plain.txt")
    );

    db.upsert_node(&folder("folder", Some("root"), "yyafteryy"))
        .unwrap();
    assert!(
        !db.search_candidates("zzbeforezz", 20)
            .unwrap()
            .iter()
            .any(|h| h.node.name == "plain.txt")
    );
    assert!(
        db.search_candidates("yyafteryy", 20)
            .unwrap()
            .iter()
            .any(|h| h.node.name == "plain.txt")
    );
}

#[test]
fn local_fuzzy_candidates_include_typo_and_parent_path() {
    let db = Db::open_in_memory().unwrap();
    let generation = db.local_begin_scan().unwrap();
    db.local_upsert_batch(
        generation,
        &[
            local(
                "/home/u/Projects/quarterly-report.pdf",
                "quarterly-report.pdf",
                false,
            ),
            local("/home/u/Videos/video.mp4", "video.mp4", false),
        ],
    )
    .unwrap();
    db.local_finish_scan(generation, 1).unwrap();

    assert_eq!(db.search_local_candidates("quartrly", 20).unwrap().len(), 1);
    assert_eq!(db.search_local_candidates("projects", 20).unwrap().len(), 1);
    assert!(
        db.search_local_candidates("vedio", 20)
            .unwrap()
            .iter()
            .any(|hit| hit.name == "video.mp4")
    );
}

#[test]
fn search_drops_fts_row_on_delete_and_trash() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&file("f1", "root", "unique.txt", 1))
        .unwrap();
    assert_eq!(db.search("unique", 10).unwrap().len(), 1);

    // Re-upsert as trashed → leaves the index.
    let mut t = file("f1", "root", "unique.txt", 1);
    t.trashed = true;
    db.upsert_node(&t).unwrap();
    assert_eq!(db.search("unique", 10).unwrap().len(), 0);

    // Resurrect, then hard-delete.
    db.upsert_node(&file("f1", "root", "unique.txt", 1))
        .unwrap();
    assert_eq!(db.search("unique", 10).unwrap().len(), 1);
    db.delete_node(&uid("f1")).unwrap();
    assert_eq!(db.search("unique", 10).unwrap().len(), 0);
}

/// Renaming re-indexes the node, which is the case delete-by-rowid has to get
/// right: the *old* name must leave the index. A stale row here would be
/// invisible in every other test — the new name is findable either way.
#[test]
fn renaming_a_node_replaces_the_name_in_the_search_index() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&file("f1", "root", "beforename.txt", 1))
        .unwrap();
    assert_eq!(db.search("beforename", 10).unwrap().len(), 1);

    db.upsert_node(&file("f1", "root", "aftername.txt", 1))
        .unwrap();

    assert_eq!(
        db.search("aftername", 10).unwrap().len(),
        1,
        "the new name must be searchable"
    );
    assert_eq!(
        db.search("beforename", 10).unwrap().len(),
        0,
        "the old name must have left the index, not merely been added alongside"
    );
}

/// B12: the cost of writing a listing must not scale with how much is already
/// indexed. The old `DELETE FROM nodes_fts WHERE uid = ?` scanned the whole
/// index once per node written, so a large account paid more to write the *same*
/// folder — measured at 6.5 ms per node against a 17k-node index on the live DB.
///
/// Written as a ratio between two index sizes rather than an absolute time, so
/// it fails on the scaling regression and not on a slow machine.
#[test]
fn writing_nodes_does_not_scale_with_the_size_of_the_index() {
    use std::time::Instant;

    /// Fill `db` with `n` indexed nodes, then time re-writing a fixed batch.
    fn cost_against_index_of(n: usize) -> std::time::Duration {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        let filler: Vec<_> = (0..n)
            .map(|i| file(&format!("bg{i}"), "root", &format!("background{i}.txt"), 1))
            .collect();
        db.upsert_nodes(&filler).unwrap();

        // The batch under test is identical in both runs; only the amount of
        // already-indexed data around it differs.
        let batch: Vec<_> = (0..200)
            .map(|i| file(&format!("m{i}"), "root", &format!("measured{i}.txt"), 1))
            .collect();
        db.upsert_nodes(&batch).unwrap();

        let t = Instant::now();
        db.upsert_nodes(&batch).unwrap();
        t.elapsed()
    }

    let small = cost_against_index_of(200);
    let large = cost_against_index_of(5000);

    // Calibration, measured while writing this: over the same 25× growth in
    // index size, the old full-scan path cost 6.4× more (46 ms → 294 ms) while
    // the rowid path costs 1.04× (39 ms → 41 ms). 3× sits well clear of both.
    println!("B12: 200-node write — against 200 indexed {small:?}, against 5000 {large:?}");
    assert!(
        large < small * 3,
        "writing a listing got dramatically more expensive as the index grew, \
         which is the B12 full-scan regression; small={small:?} large={large:?}"
    );
}

fn activity(target: &str, kind: ActivityKind, ok: bool) -> ActivityEntry {
    ActivityEntry {
        time: 1700,
        kind,
        target: target.into(),
        detail: "detail".into(),
        ok,
    }
}

#[test]
fn activity_reads_back_newest_first() {
    let db = Db::open_in_memory().unwrap();
    db.activity_add(&activity("a.txt", ActivityKind::Upload, true))
        .unwrap();
    db.activity_add(&activity("b.txt", ActivityKind::Download, false))
        .unwrap();

    let items = db.activity_list(10).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].target, "b.txt");
    assert_eq!(items[0].kind, ActivityKind::Download);
    assert!(!items[0].ok);
    assert_eq!(items[0].detail, "detail");
    assert_eq!(items[0].time, 1700);
    assert_eq!(items[1].target, "a.txt");

    assert_eq!(db.activity_list(1).unwrap().len(), 1);
}

#[test]
fn activity_prunes_to_the_keep_limit() {
    let db = Db::open_in_memory().unwrap();
    for i in 0..(ACTIVITY_KEEP + 10) {
        db.activity_add(&activity(&format!("f{i}"), ActivityKind::Upload, true))
            .unwrap();
    }
    let items = db.activity_list(ACTIVITY_KEEP as usize * 2).unwrap();
    assert_eq!(items.len(), ACTIVITY_KEEP as usize);
    // The newest survive; the oldest are the ones dropped.
    assert_eq!(items[0].target, format!("f{}", ACTIVITY_KEEP + 9));
}

#[test]
fn opens_and_migrates() {
    let db = Db::open_in_memory().unwrap();
    let version: String = db
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT value FROM sync_state WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION.to_string());
}

#[test]
fn share_access_round_trips_updates_and_deletes() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("shared-root");

    assert_eq!(db.share_access(&root).unwrap(), None);
    for access in [
        crate::Access::Owner,
        crate::Access::Editor,
        crate::Access::Viewer,
        crate::Access::Unknown,
    ] {
        db.set_share_access(&root, access).unwrap();
        assert_eq!(db.share_access(&root).unwrap(), Some(access));
        assert_eq!(db.all_share_access().unwrap().get(&root), Some(&access));
    }
    db.delete_share_access(&root).unwrap();
    assert_eq!(db.share_access(&root).unwrap(), None);
    assert!(db.all_share_access().unwrap().is_empty());
}

#[test]
fn effective_node_access_inherits_from_the_persisted_share_root() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("shared", Some("root"), "Shared"))
        .unwrap();
    db.upsert_node(&folder("nested", Some("shared"), "Nested"))
        .unwrap();
    db.upsert_node(&file("child", "nested", "child.txt", 1))
        .unwrap();
    db.set_share_access(&uid("shared"), crate::Access::Viewer)
        .unwrap();

    assert_eq!(
        db.effective_node_access(&uid("child")).unwrap(),
        Some(crate::Access::Viewer)
    );
    assert_eq!(
        db.effective_node_access(&uid("root")).unwrap(),
        Some(crate::Access::Owner)
    );
    assert_eq!(db.effective_node_access(&uid("missing")).unwrap(), None);
}

#[test]
fn effective_node_access_uses_the_nearest_nested_share_root() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("outer", Some("root"), "Outer"))
        .unwrap();
    db.upsert_node(&folder("inner", Some("outer"), "Inner"))
        .unwrap();
    db.upsert_node(&file("child", "inner", "child.txt", 1))
        .unwrap();
    db.set_share_access(&uid("outer"), crate::Access::Editor)
        .unwrap();
    db.set_share_access(&uid("inner"), crate::Access::Viewer)
        .unwrap();

    assert_eq!(
        db.effective_node_access(&uid("child")).unwrap(),
        Some(crate::Access::Viewer)
    );
}

#[test]
fn effective_node_access_keeps_a_missing_share_root_tombstone_authoritative() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("shared", Some("root"), "Shared"))
        .unwrap();
    db.upsert_node(&folder("nested", Some("shared"), "Nested"))
        .unwrap();
    db.upsert_node(&file("child", "nested", "child.txt", 1))
        .unwrap();
    db.set_share_access(&uid("shared"), crate::Access::Viewer)
        .unwrap();
    db.delete_node(&uid("shared")).unwrap();

    assert_eq!(
        db.effective_node_access(&uid("child")).unwrap(),
        Some(crate::Access::Viewer)
    );
}

#[test]
fn downgrade_all_share_access_leaves_unrecorded_owned_nodes_owned() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("shared", Some("root"), "Shared"))
        .unwrap();
    db.set_share_access(&uid("shared"), crate::Access::Editor)
        .unwrap();

    assert_eq!(db.downgrade_all_share_access().unwrap(), 1);
    assert_eq!(
        db.effective_node_access(&uid("shared")).unwrap(),
        Some(crate::Access::Viewer)
    );
    assert_eq!(
        db.effective_node_access(&uid("root")).unwrap(),
        Some(crate::Access::Owner)
    );
}

#[test]
fn migration_v17_adds_share_access_to_a_v16_fixture() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-db-v16-fixture-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        // Remove the later tables too, so restoring the version stamp produces
        // the exact schema a released V16 database had.
        let db = Db::open(&path).unwrap();
        db.set_state_str("fixture_value", "survives").unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TRIGGER mount_sync_folder_insert;
             DROP TABLE mount;
             DROP TABLE share_access;
             DROP TABLE album_photos;
             DROP TABLE albums;
             DROP INDEX idx_photos_favorite;
             ALTER TABLE photos DROP COLUMN favorite;
             ALTER TABLE nodes DROP COLUMN path;
             UPDATE sync_state SET value = '16' WHERE key = 'schema_version';",
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.state_str("fixture_value").unwrap().as_deref(),
        Some("survives")
    );
    let root = uid("offline-share");
    db.set_share_access(&root, crate::Access::Viewer).unwrap();
    assert_eq!(db.share_access(&root).unwrap(), Some(crate::Access::Viewer));
    // Migrating an old fixture always lands on head, whatever head is today.
    assert_eq!(
        db.state_str("schema_version").unwrap().as_deref(),
        Some(SCHEMA_VERSION.to_string().as_str())
    );

    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn migration_v18_projects_v17_sync_folders_and_cascades_deletes() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-db-v17-fixture-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        // Pin the released V17 shape that V18 reads instead of deriving a
        // fixture from today's head schema and dropping newer objects.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sync_state (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO sync_state VALUES ('schema_version', '17');
             INSERT INTO sync_state VALUES ('fixture_value', 'survives');
             CREATE TABLE sync_folder (
               id              INTEGER PRIMARY KEY,
               local_path      TEXT NOT NULL UNIQUE,
               remote_uid      TEXT NOT NULL,
               remote_share_id TEXT NOT NULL,
               mode            TEXT NOT NULL DEFAULT 'mirror',
               state           TEXT NOT NULL DEFAULT 'idle',
               last_sync       INTEGER NOT NULL DEFAULT 0,
               pending_mode    TEXT
             );
             CREATE TABLE sync_entry (
               folder_id   INTEGER NOT NULL,
               rel_path    TEXT NOT NULL,
               remote_uid  TEXT,
               local_mtime INTEGER NOT NULL DEFAULT 0,
               local_size  INTEGER NOT NULL DEFAULT 0,
               remote_hash TEXT,
               remote_rev  TEXT,
               PRIMARY KEY (folder_id, rel_path)
             );
             INSERT INTO sync_folder
               (id, local_path, remote_uid, remote_share_id, mode, state, last_sync)
             VALUES
               (41, '/home/me/Existing', 'vol~existing', 'share-existing',
                'ondemand', 'syncing', 123);
             -- The timeline as V17 held it: later migrations add columns to it,
             -- so a fixture without it isn't a database any release produced.
             CREATE TABLE photos (
               uid          TEXT PRIMARY KEY,
               capture_time INTEGER NOT NULL,
               name         TEXT,
               ratio        REAL,
               thumb_state  INTEGER NOT NULL DEFAULT 0,
               seq          INTEGER NOT NULL,
               media_type   TEXT,
               kind         INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE share_access (
               root_uid TEXT PRIMARY KEY,
               access   TEXT NOT NULL
                        CHECK (access IN ('owner', 'editor', 'viewer', 'unknown'))
             );",
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    let existing = 41;
    let locations = db.mount_list().unwrap();
    assert_eq!(locations.len(), 1);
    let projected = &locations[0];
    assert!(matches!(
        projected.kind,
        crate::mounts::MountKind::Device {
            sync_folder_id
        } if sync_folder_id == existing
    ));
    assert_eq!(projected.local_path, "/home/me/Existing");
    assert_eq!(projected.mode, crate::mounts::MountMode::OnDemand);
    assert_eq!(projected.state, "syncing");
    assert_eq!(projected.last_sync, 123);
    assert_eq!(
        db.state_str("fixture_value").unwrap().as_deref(),
        Some("survives")
    );

    let added = db
        .sync_folder_add("/home/me/New", "vol~new", "share-new")
        .unwrap();
    assert_eq!(db.mount_list().unwrap().len(), 2);
    assert!(db.sync_folder_remove(added).unwrap());
    assert_eq!(
        db.mount_list().unwrap().len(),
        1,
        "the V18 foreign key must remove the device projection"
    );
    // Migrating an old fixture always lands on head, whatever head is today.
    assert_eq!(
        db.state_str("schema_version").unwrap().as_deref(),
        Some(SCHEMA_VERSION.to_string().as_str())
    );

    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn my_files_projection_retains_share_id_only_for_the_same_root() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .mount_upsert_my_files("/home/me/ProtonDrive", "vol~root", Some("share-main"))
        .unwrap();
    let same_id = db
        .mount_upsert_my_files("/mnt/proton", "vol~root", None)
        .unwrap();
    assert_eq!(same_id, id);
    assert_eq!(db.mount_list().unwrap()[0].root_share_id, "share-main");

    db.mount_upsert_my_files("/mnt/proton", "vol~root-new", None)
        .unwrap();

    let locations = db.mount_list().unwrap();
    assert_eq!(locations.len(), 1);
    assert!(matches!(
        locations[0].kind,
        crate::mounts::MountKind::MyFiles
    ));
    assert_eq!(locations[0].local_path, "/mnt/proton");
    assert_eq!(locations[0].root_uid, "vol~root-new");
    assert_eq!(
        locations[0].root_share_id, "",
        "a share id must never be paired with a different root uid"
    );
}

#[test]
fn my_files_share_id_repair_is_conditional_on_root_uid() {
    let db = Db::open_in_memory().unwrap();
    db.mount_upsert_my_files("/mnt/proton", "vol~root", None)
        .unwrap();

    assert!(
        !db.mount_repair_my_files_share_id("vol~stale", "share-stale")
            .unwrap()
    );
    assert_eq!(db.mount_list().unwrap()[0].root_share_id, "");

    assert!(
        db.mount_repair_my_files_share_id("vol~root", "share-current")
            .unwrap()
    );
    assert_eq!(db.mount_list().unwrap()[0].root_share_id, "share-current");
}

#[test]
fn mount_constraints_reject_malformed_kinds_and_missing_sync_folders() {
    let db = Db::open_in_memory().unwrap();
    let conn = db.conn.lock();

    assert!(
        conn.execute(
            "INSERT INTO mount (kind, local_path, root_uid)
             VALUES ('device', '/tmp/device', 'vol~device')",
            [],
        )
        .is_err(),
        "a device projection without sync_folder_id must fail its CHECK"
    );
    assert!(
        conn.execute(
            "INSERT INTO mount (kind, sync_folder_id)
             VALUES ('device', 999999)",
            [],
        )
        .is_err(),
        "a device projection cannot reference a missing sync folder"
    );
    assert!(
        conn.execute(
            "INSERT INTO mount (kind, share_root_uid, local_path, root_uid)
             VALUES ('myfiles', 'vol~shared', '/tmp/root', 'vol~root')",
            [],
        )
        .is_err(),
        "kind-specific columns must not be mixed"
    );
}

#[test]
fn deleting_a_device_projection_does_not_delete_its_sync_folder() {
    let db = Db::open_in_memory().unwrap();
    let folder_id = db
        .sync_folder_add("/home/me/Keep", "vol~keep", "share-keep")
        .unwrap();
    {
        let conn = db.conn.lock();
        assert_eq!(
            conn.execute(
                "DELETE FROM mount WHERE kind = 'device' AND sync_folder_id = ?1",
                [folder_id],
            )
            .unwrap(),
            1
        );
    }

    assert!(db.sync_folder_get(folder_id).unwrap().is_some());
    assert!(
        db.mount_list().unwrap().is_empty(),
        "the foreign key direction must not recreate or delete sync authority"
    );
}

#[test]
fn refuses_a_schema_newer_than_the_running_build() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-db-future-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE sync_state (key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO sync_state VALUES ('schema_version', '9999');",
    )
    .unwrap();
    drop(conn);

    let err = Db::open(&path)
        .err()
        .expect("future schema must fail closed");
    assert!(err.to_string().contains("newer than this build supports"));
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: String = conn
        .query_row(
            "SELECT value FROM sync_state WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "9999", "failed open must not rewrite the database");
    drop(conn);
    let _ = std::fs::remove_file(path);
}

/// A queued write whose baseline is restamped must keep everything else.
///
/// The restamp happens after our *own* upload seals a new revision under a
/// still-queued write. If it took the blob or the retry state with it, the fix
/// for a spurious conflict copy would cost the bytes that conflict copy was
/// there to protect.
#[test]
fn restamping_a_baseline_leaves_the_blob_and_retry_state_alone() {
    let db = Db::open_in_memory().unwrap();
    let op = PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid("a").to_string(),
        parent_uid: None,
        name: None,
        blob_path: Some("/staging/blob".to_string()),
        meta_json: Some(r#"{"based_on":"old"}"#.to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };
    db.enqueue_op(&op).unwrap();
    db.record_op_failure(db.pending_ops().unwrap()[0].id, "offline", 999)
        .unwrap();

    let updated = db
        .update_op_meta(&uid("a").to_string(), OP_REVISION, r#"{"based_on":"new"}"#)
        .unwrap();
    assert!(updated, "the queued write is there to restamp");

    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].meta_json.as_deref(), Some(r#"{"based_on":"new"}"#));
    assert_eq!(
        ops[0].blob_path.as_deref(),
        Some("/staging/blob"),
        "the only copy of the user's bytes"
    );
    assert_eq!(ops[0].attempts, 1, "backoff survives a restamp");
    assert_eq!(ops[0].next_attempt_at, 999);

    // No queued write for that node is the ordinary case, not an error: most
    // uploads are the last one for their file.
    assert!(
        !db.update_op_meta(&uid("b").to_string(), OP_REVISION, "{}")
            .unwrap()
    );
}

#[test]
fn a_second_write_supersedes_the_first_pending_op() {
    let db = Db::open_in_memory().unwrap();
    let op = |blob: &str| PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid("a").to_string(),
        parent_uid: None,
        name: None,
        blob_path: Some(blob.to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };

    let (_, superseded) = db.enqueue_op(&op("/staging/first")).unwrap();
    assert_eq!(superseded, None, "nothing to supersede on the first write");

    // The newer blob already contains everything the older one did, so the
    // older op must go — and its blob must be reported so it can be deleted
    // rather than leaked.
    let (id2, superseded) = db.enqueue_op(&op("/staging/second")).unwrap();
    assert_eq!(superseded.as_deref(), Some("/staging/first"));

    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1, "one queued upload per node");
    assert_eq!(ops[0].id, id2);
    assert_eq!(ops[0].blob_path.as_deref(), Some("/staging/second"));
    assert_eq!(db.pending_op_counts().unwrap().uploads, 1);
}

#[test]
fn failed_superseding_insert_keeps_the_old_pending_op() {
    let db = Db::open_in_memory().unwrap();
    let op = |blob: &str| PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid("atomic").to_string(),
        parent_uid: None,
        name: None,
        blob_path: Some(blob.to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };
    db.enqueue_op(&op("/staging/original")).unwrap();
    db.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER reject_replacement BEFORE INSERT ON pending_op
             WHEN NEW.blob_path = '/staging/replacement'
             BEGIN SELECT RAISE(ABORT, 'injected insert failure'); END;",
        )?;
        Ok(())
    })
    .unwrap();

    assert!(db.enqueue_op(&op("/staging/replacement")).is_err());
    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].blob_path.as_deref(), Some("/staging/original"));
}

/// B53's remaining half. The insert-failure regression above proves the *row*
/// rolls back; this proves the two things that make the rollback usable — the
/// old blob is not reported as superseded (so the caller does not delete the
/// bytes the surviving row still points at), and the rollback survives a
/// restart rather than living only in the connection that failed.
#[test]
fn a_rolled_back_supersede_keeps_its_blob_and_survives_reopen() {
    let dir = std::env::temp_dir().join(format!("pdfs-db-b53-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("queue.sqlite");
    let op = |blob: &str| PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid("durable").to_string(),
        parent_uid: None,
        name: None,
        blob_path: Some(blob.to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };

    {
        let db = Db::open(&path).unwrap();
        db.enqueue_op(&op("/staging/original")).unwrap();
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER reject_replacement BEFORE INSERT ON pending_op
                 WHEN NEW.blob_path = '/staging/replacement'
                 BEGIN SELECT RAISE(ABORT, 'injected insert failure'); END;",
            )?;
            Ok(())
        })
        .unwrap();

        // The failure must not hand back the old blob path. That value is the
        // caller's instruction to delete those bytes, and the row that owns
        // them is still queued.
        assert!(db.enqueue_op(&op("/staging/replacement")).is_err());
        db.with_conn(|conn| {
            conn.execute_batch("DROP TRIGGER reject_replacement")?;
            Ok(())
        })
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1, "old or new — never neither");
    assert_eq!(ops[0].blob_path.as_deref(), Some("/staging/original"));

    // And once the injected failure is gone, superseding works and *does*
    // report the blob it retired.
    let (_, superseded) = db.enqueue_op(&op("/staging/replacement")).unwrap();
    assert_eq!(superseded.as_deref(), Some("/staging/original"));
    assert_eq!(db.pending_ops().unwrap().len(), 1);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A full disk, as SQLite reports it. `max_page_count` makes the database
/// refuse to grow, which is the same `SQLITE_FULL` an out-of-space filesystem
/// produces — and the queue must come out of it with the acknowledged work
/// still in it (B53).
#[test]
fn a_full_database_rolls_the_supersede_back_rather_than_erasing_it() {
    let db = Db::open_in_memory().unwrap();
    let op = |blob: &str| PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid("full").to_string(),
        parent_uid: None,
        name: None,
        blob_path: Some(blob.to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };
    db.enqueue_op(&op("/staging/original")).unwrap();

    // Cap the file at its current size, so any page the insert needs fails.
    let pages: i64 = db
        .with_conn(|conn| Ok(conn.query_row("PRAGMA page_count", [], |r| r.get(0))?))
        .unwrap();
    db.with_conn(|conn| {
        conn.execute_batch(&format!("PRAGMA max_page_count = {pages}"))?;
        Ok(())
    })
    .unwrap();

    // A payload far larger than one page, so growth is unavoidable.
    let mut fat = op("/staging/replacement");
    fat.meta_json = Some("x".repeat(512 * 1024));
    let result = db.enqueue_op(&fat);

    db.with_conn(|conn| {
        conn.execute_batch("PRAGMA max_page_count = 1073741823")?;
        Ok(())
    })
    .unwrap();

    assert!(result.is_err(), "a database that cannot grow must refuse");
    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(
        ops[0].blob_path.as_deref(),
        Some("/staging/original"),
        "the acknowledged upload is still queued and still owns its bytes"
    );
}

#[test]
fn atomic_trash_replacement_returns_owned_blobs_after_commit() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("trash-root").to_string();
    let child = uid("trash-child").to_string();
    let revision = |uid: &str, parent_uid: Option<&str>, blob: &str| PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid.to_string(),
        parent_uid: parent_uid.map(str::to_string),
        name: None,
        blob_path: Some(blob.to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };
    db.enqueue_op(&revision(&root, None, "/staging/root"))
        .unwrap();
    db.enqueue_op(&revision(&child, Some(&root), "/staging/child"))
        .unwrap();

    let (trash_id, mut blobs) = db.replace_ops_with_trash(&root, "removed.txt", 2).unwrap();
    blobs.sort();
    assert_eq!(blobs, vec!["/staging/child", "/staging/root"]);

    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].id, trash_id);
    assert_eq!(ops[0].kind, OP_TRASH);
    assert_eq!(ops[0].uid, root);
    assert_eq!(ops[0].name.as_deref(), Some("removed.txt"));
    assert!(ops[0].blob_path.is_none());
}

#[test]
fn failed_atomic_trash_insert_keeps_prior_revision_and_blob_ownership() {
    let db = Db::open_in_memory().unwrap();
    let node = uid("atomic-trash").to_string();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: node.clone(),
        parent_uid: None,
        name: None,
        blob_path: Some("/staging/still-owned".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();
    db.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER reject_trash_insert BEFORE INSERT ON pending_op
             WHEN NEW.kind = 'trash'
             BEGIN SELECT RAISE(ABORT, 'injected trash insert failure'); END;",
        )?;
        Ok(())
    })
    .unwrap();

    assert!(db.replace_ops_with_trash(&node, "removed.txt", 2).is_err());
    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].kind, OP_REVISION);
    assert_eq!(ops[0].uid, node);
    assert_eq!(
        ops[0].blob_path.as_deref(),
        Some("/staging/still-owned"),
        "the rolled-back revision still owns its staged bytes"
    );
}

/// Deleting a folder that was created offline must take the ops queued
/// underneath it with it: they name a placeholder parent that will now never
/// become real, so nothing could ever drain them and nothing is left to
/// rewrite them.
#[test]
fn deleting_a_queued_folder_takes_its_queued_children_with_it() {
    let db = Db::open_in_memory().unwrap();
    let op = |kind: &str, uid: &str, parent: &str, blob: Option<&str>| PendingOp {
        id: 0,
        kind: kind.to_string(),
        uid: uid.to_string(),
        parent_uid: Some(parent.to_string()),
        name: Some("n".to_string()),
        blob_path: blob.map(str::to_string),
        meta_json: None,
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };
    let root = uid("root").to_string();
    db.enqueue_op(&op(OP_MKDIR, "local~dir", &root, None))
        .unwrap();
    db.enqueue_op(&op(OP_MKDIR, "local~sub", "local~dir", None))
        .unwrap();
    db.enqueue_op(&op(
        OP_CREATE,
        "local~deep",
        "local~sub",
        Some("/staging/deep"),
    ))
    .unwrap();
    // A sibling outside the doomed subtree must survive.
    db.enqueue_op(&op(OP_CREATE, "local~other", &root, Some("/staging/other")))
        .unwrap();

    let blobs = db.delete_ops_for_uid("local~dir").unwrap();
    assert_eq!(
        blobs,
        vec!["/staging/deep"],
        "the subtree's bytes come back"
    );

    let left: Vec<String> = db
        .pending_ops()
        .unwrap()
        .into_iter()
        .map(|o| o.uid)
        .collect();
    assert_eq!(left, vec!["local~other"]);
}

/// A rename is the node's desired end state, so the newest one is the only
/// one worth performing — but it must not disturb the queued *upload* of the
/// same node, which is unrelated work.
#[test]
fn a_second_rename_supersedes_the_first_but_leaves_the_upload_alone() {
    let db = Db::open_in_memory().unwrap();
    let rename = |name: &str| PendingOp {
        id: 0,
        kind: OP_RENAME.to_string(),
        uid: uid("a").to_string(),
        parent_uid: Some(uid("parent").to_string()),
        name: Some(name.to_string()),
        blob_path: None,
        meta_json: None,
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid("a").to_string(),
        parent_uid: None,
        name: None,
        blob_path: Some("/staging/blob".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();

    db.enqueue_op(&rename("first")).unwrap();
    let (_, superseded) = db.enqueue_op(&rename("second")).unwrap();
    assert_eq!(superseded, None, "a rename owns no blob to clean up");

    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 2, "the queued upload survives the rename");
    let renames: Vec<_> = ops.iter().filter(|o| o.kind == OP_RENAME).collect();
    assert_eq!(renames.len(), 1, "one rename per node");
    assert_eq!(renames[0].name.as_deref(), Some("second"));

    let counts = db.pending_op_counts().unwrap();
    assert_eq!(counts.uploads, 1, "the revision is the only upload");
    assert_eq!(counts.changes, 1, "the rename carries no bytes");
}

/// Renaming a node whose create has not drained rewrites the intent rather
/// than queueing a rename against a uid the server has never issued.
#[test]
fn renaming_a_queued_create_rewrites_its_target() {
    let db = Db::open_in_memory().unwrap();
    let local = "local~abc";
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_CREATE.to_string(),
        uid: local.to_string(),
        parent_uid: Some(uid("old").to_string()),
        name: Some("draft.txt".to_string()),
        blob_path: Some("/staging/blob".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();

    let rewritten = db
        .rewrite_op_target(local, &uid("new").to_string(), "final.txt")
        .unwrap();
    assert!(rewritten);

    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1, "a rewrite is not a second op");
    assert_eq!(ops[0].name.as_deref(), Some("final.txt"));
    assert_eq!(
        ops[0].parent_uid.as_deref(),
        Some(uid("new").to_string()).as_deref()
    );
    assert_eq!(
        ops[0].blob_path.as_deref(),
        Some("/staging/blob"),
        "the bytes riding on the create are untouched"
    );

    // Once the create has drained there is no intent left to rewrite, and the
    // caller has to rename the real node instead.
    db.delete_op(ops[0].id).unwrap();
    assert!(
        !db.rewrite_op_target(local, &uid("new").to_string(), "final.txt")
            .unwrap()
    );
}

#[test]
fn a_write_folds_into_a_queued_create_instead_of_superseding_it() {
    let db = Db::open_in_memory().unwrap();
    let local = "local~abc";
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_CREATE.to_string(),
        uid: local.to_string(),
        parent_uid: Some(uid("parent").to_string()),
        name: Some("new.txt".to_string()),
        blob_path: None,
        meta_json: None,
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();

    let first = db
        .attach_blob_to_create(local, "/staging/first", "{}")
        .unwrap()
        .expect("create is still queued");
    assert_eq!(first.superseded, None);

    // Rewriting the file before it drains replaces the bytes but must leave
    // the create itself alone: it is the only thing that will ever bring this
    // file into existence remotely.
    let second = db
        .attach_blob_to_create(local, "/staging/second", "{}")
        .unwrap()
        .expect("create is still queued");
    assert_eq!(second.superseded.as_deref(), Some("/staging/first"));

    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1, "still exactly one create");
    assert_eq!(ops[0].kind, OP_CREATE);
    assert_eq!(ops[0].blob_path.as_deref(), Some("/staging/second"));
    assert_eq!(ops[0].name.as_deref(), Some("new.txt"));
}

#[test]
fn attaching_to_an_already_drained_create_reports_it_is_gone() {
    let db = Db::open_in_memory().unwrap();
    // No create queued: the caller must fall back to a revision op rather
    // than silently dropping the bytes.
    let out = db
        .attach_blob_to_create("local~gone", "/staging/x", "{}")
        .unwrap();
    assert!(out.is_none());
}

#[test]
fn draining_a_folder_repoints_its_queued_children() {
    let db = Db::open_in_memory().unwrap();
    let local_dir = "local~dir";
    let real_dir = uid("realdir").to_string();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_CREATE.to_string(),
        uid: "local~child".to_string(),
        parent_uid: Some(local_dir.to_string()),
        name: Some("inside.txt".to_string()),
        blob_path: Some("/staging/child".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 2,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();

    db.remap_local_uid(local_dir, &real_dir).unwrap();

    // The child was queued against a folder that did not exist yet. Once the
    // folder is real, the child must target the server's uid — otherwise the
    // upload would address `local~dir` and 404.
    let ops = db.pending_ops().unwrap();
    assert_eq!(ops[0].parent_uid.as_deref(), Some(real_dir.as_str()));
}

#[test]
fn a_failed_op_stays_queued_with_backoff() {
    let db = Db::open_in_memory().unwrap();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid("a").to_string(),
        parent_uid: None,
        name: None,
        blob_path: Some("/staging/blob".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    })
    .unwrap();
    let id = db.pending_ops().unwrap()[0].id;

    db.record_op_failure(id, "network unreachable", 5_000)
        .unwrap();

    // The staged blob is the only copy of the user's bytes: a failure must
    // never drop the row, only defer it.
    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].attempts, 1);
    assert_eq!(ops[0].last_error.as_deref(), Some("network unreachable"));
    assert_eq!(ops[0].next_attempt_at, 5_000);

    db.record_op_failure(id, "still down", 9_000).unwrap();
    assert_eq!(db.pending_ops().unwrap()[0].attempts, 2);

    db.delete_op(id).unwrap();
    assert_eq!(db.pending_op_counts().unwrap().uploads, 0);
}

#[test]
fn an_access_blocked_op_is_deferred_without_counting_an_attempt() {
    let db = Db::open_in_memory().unwrap();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: uid("blocked").to_string(),
        parent_uid: None,
        name: None,
        blob_path: Some("/staging/blocked".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 3,
        last_error: Some("earlier network failure".to_string()),
        next_attempt_at: 0,
    })
    .unwrap();
    let before = db.pending_ops().unwrap();
    let id = before[0].id;
    let count = db.pending_op_counts().unwrap();

    db.defer_op_without_attempt(id, 5_000).unwrap();

    let after = db.pending_ops().unwrap();
    assert_eq!(after.len(), 1);
    let after_count = db.pending_op_counts().unwrap();
    assert_eq!(after_count.uploads, count.uploads);
    assert_eq!(after_count.changes, count.changes);
    assert_eq!(after[0].attempts, before[0].attempts);
    assert_eq!(after[0].last_error, before[0].last_error);
    assert_eq!(after[0].next_attempt_at, 5_000);
}

#[test]
fn access_deferral_does_not_unpark_a_transient_create() {
    let db = Db::open_in_memory().unwrap();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_CREATE.to_string(),
        uid: uid("transient").to_string(),
        parent_uid: Some(uid("parent").to_string()),
        name: Some(".part".to_string()),
        blob_path: Some("/staging/transient".to_string()),
        meta_json: Some("{}".to_string()),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: PARK_UNTIL,
    })
    .unwrap();
    let op = db.pending_ops().unwrap().remove(0);

    db.defer_op_without_attempt(op.id, 5_000).unwrap();

    assert_eq!(db.pending_ops().unwrap()[0].next_attempt_at, PARK_UNTIL);
}

#[test]
fn migrate_is_idempotent() {
    let db = Db::open_in_memory().unwrap();
    // Second migrate is a no-op (already at head) and must not error.
    db.migrate().unwrap();
}

/// A queued mode switch is a promise the daemon has to keep across a restart,
/// so it lives in the row, and reaching the mode is what retires it.
#[test]
fn pending_mode_is_queued_until_the_mode_is_reached() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .sync_folder_add("/home/me/Downloads", "v~l", "s")
        .unwrap();
    assert_eq!(db.sync_folder_get(id).unwrap().unwrap().pending_mode, None);

    db.sync_folder_set_pending_mode(id, Some("ondemand"))
        .unwrap();
    assert_eq!(
        db.sync_folder_get(id)
            .unwrap()
            .unwrap()
            .pending_mode
            .as_deref(),
        Some("ondemand")
    );
    // The listing carries it too — it is what the front-ends paint from.
    assert_eq!(
        db.sync_folder_list().unwrap()[0].pending_mode.as_deref(),
        Some("ondemand")
    );

    // Landing the switch satisfies the request: a `pending_mode` outliving it
    // would have the engine try to apply the same switch on every later pass.
    db.sync_folder_set_mode(id, "ondemand").unwrap();
    let folder = db.sync_folder_get(id).unwrap().unwrap();
    assert_eq!(folder.mode, "ondemand");
    assert_eq!(folder.pending_mode, None);

    // And the user can withdraw a request that hasn't landed yet.
    db.sync_folder_set_pending_mode(id, Some("mirror")).unwrap();
    db.sync_folder_set_pending_mode(id, None).unwrap();
    let folder = db.sync_folder_get(id).unwrap().unwrap();
    assert_eq!(folder.mode, "ondemand");
    assert_eq!(folder.pending_mode, None);
}

#[test]
fn mode_commit_clears_only_the_intent_it_satisfies() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .sync_folder_add("/tmp/pdfs-mode-intent", "vol~folder", "share")
        .unwrap();

    db.sync_folder_set_pending_mode(id, Some("ondemand"))
        .unwrap();
    db.sync_folder_set_mode(id, "ondemand").unwrap();
    assert_eq!(db.sync_folder_get(id).unwrap().unwrap().pending_mode, None);

    db.sync_folder_set_pending_mode(id, Some("mirror")).unwrap();
    db.sync_folder_set_mode(id, "ondemand").unwrap();
    assert_eq!(
        db.sync_folder_get(id)
            .unwrap()
            .unwrap()
            .pending_mode
            .as_deref(),
        Some("mirror")
    );
}

#[test]
fn clearing_a_stale_pending_mode_preserves_the_newer_intent() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .sync_folder_add("/tmp/pdfs-pending", "vol~folder", "share")
        .unwrap();
    db.sync_folder_set_pending_mode(id, Some("ondemand"))
        .unwrap();
    db.sync_folder_set_pending_mode(id, Some("mirror")).unwrap();

    assert!(
        !db.sync_folder_clear_pending_mode_if(id, "ondemand")
            .unwrap()
    );
    assert_eq!(
        db.sync_folder_get(id)
            .unwrap()
            .unwrap()
            .pending_mode
            .as_deref(),
        Some("mirror")
    );
    assert!(db.sync_folder_clear_pending_mode_if(id, "mirror").unwrap());
    assert_eq!(db.sync_folder_get(id).unwrap().unwrap().pending_mode, None);
}

#[test]
fn mirror_uid_lookup_covers_the_root_and_synced_descendants_only() {
    let db = Db::open_in_memory().unwrap();
    let mirror = db
        .sync_folder_add("/home/me/Mirror", "vol~mirror-root", "share")
        .unwrap();
    db.sync_entry_upsert(
        mirror,
        &StoredSyncEntry {
            rel_path: "nested/file.txt".into(),
            remote_uid: Some("vol~mirror-file".into()),
            local_mtime: 1,
            local_size: 2,
            remote_rev: Some("3".into()),
            remote_hash: Some("2".into()),
            local_mtime_ns: None,
        },
    )
    .unwrap();
    let ondemand = db
        .sync_folder_add("/home/me/OnDemand", "vol~ondemand-root", "share")
        .unwrap();
    db.sync_folder_set_mode(ondemand, "ondemand").unwrap();
    db.sync_entry_upsert(
        ondemand,
        &StoredSyncEntry {
            rel_path: "stale.txt".into(),
            remote_uid: Some("vol~ondemand-stale".into()),
            local_mtime: 1,
            local_size: 2,
            remote_rev: Some("3".into()),
            remote_hash: Some("2".into()),
            local_mtime_ns: None,
        },
    )
    .unwrap();

    assert!(db.mirror_contains_uid("vol~mirror-root").unwrap());
    assert!(db.mirror_contains_uid("vol~mirror-file").unwrap());
    assert!(!db.mirror_contains_uid("vol~ondemand-root").unwrap());
    assert!(!db.mirror_contains_uid("vol~ondemand-stale").unwrap());
    assert!(!db.mirror_contains_uid("vol~missing").unwrap());
}

#[test]
fn node_path_resolves_relative_to_a_sync_root_uid() {
    let db = Db::open_in_memory().unwrap();
    let root = node_from(serde_json::Value::Null, "root", "My Files", json!("Folder"));
    let videos = node_from(json!(uid("root")), "videos", "Videos", json!("Folder"));
    let anime = node_from(json!(uid("videos")), "anime", "anime", json!("Folder"));
    let show = node_from(json!(uid("anime")), "show", "Oshi no Ko", json!("Folder"));
    let episode = node_from(
        json!(uid("show")),
        "episode",
        "episode 01.mkv",
        json!({"File": {
            "media_type": "video/x-matroska",
            "total_size_on_storage": 42,
            "claimed_size": 42,
            "claimed_modification_time": null
        }}),
    );
    db.upsert_nodes(&[root, videos, anime, show, episode])
        .unwrap();

    assert_eq!(
        db.path_relative_to(&uid("videos").to_string(), &uid("episode").to_string())
            .unwrap()
            .as_deref(),
        Some("anime/Oshi no Ko/episode 01.mkv")
    );
    assert_eq!(
        db.path_relative_to(&uid("videos").to_string(), &uid("videos").to_string())
            .unwrap()
            .as_deref(),
        Some("")
    );
    assert_eq!(
        db.path_relative_to(&uid("anime").to_string(), &uid("episode").to_string())
            .unwrap()
            .as_deref(),
        Some("Oshi no Ko/episode 01.mkv")
    );
}

#[test]
fn node_path_rejects_unrelated_missing_and_broken_ancestor_chains() {
    let db = Db::open_in_memory().unwrap();
    let root = node_from(serde_json::Value::Null, "root", "My Files", json!("Folder"));
    let videos = node_from(json!(uid("root")), "videos", "Videos", json!("Folder"));
    let episode = node_from(
        json!(uid("videos")),
        "episode",
        "episode.mkv",
        json!({"File": {
            "media_type": "video/x-matroska",
            "total_size_on_storage": 42,
            "claimed_size": 42,
            "claimed_modification_time": null
        }}),
    );
    let unrelated = node_from(
        json!(uid("root")),
        "documents",
        "Documents",
        json!("Folder"),
    );
    let orphan = node_from(
        json!(uid("missing-parent")),
        "orphan",
        "orphan.mkv",
        json!({"File": {
            "media_type": "video/x-matroska",
            "total_size_on_storage": 42,
            "claimed_size": 42,
            "claimed_modification_time": null
        }}),
    );
    db.upsert_nodes(&[root, videos, episode, unrelated, orphan])
        .unwrap();

    for (ancestor, descendant) in [
        ("documents", "episode"),
        ("videos", "missing-node"),
        ("videos", "orphan"),
    ] {
        assert_eq!(
            db.path_relative_to(&uid(ancestor).to_string(), &uid(descendant).to_string())
                .unwrap(),
            None
        );
    }
}

#[test]
fn cache_index_touch_access_and_lru_order() {
    let db = Db::open_in_memory().unwrap();
    db.cache_touch("k1", "blob", 100, 10).unwrap();
    db.cache_touch("k2", "blob", 200, 20).unwrap();
    // LRU-first: k1 (older access) before k2.
    let rows = db.cache_entries_by_kind("blob").unwrap();
    assert_eq!(rows, vec![("k1".into(), 100), ("k2".into(), 200)]);

    // Accessing k1 moves it to the back (most recent).
    db.cache_accessed("k1", 30).unwrap();
    let rows = db.cache_entries_by_kind("blob").unwrap();
    assert_eq!(rows[0].0, "k2");
    assert_eq!(rows[1].0, "k1");

    // Re-touch updates size, not just time.
    db.cache_touch("k1", "blob", 150, 40).unwrap();
    let rows = db.cache_entries_by_kind("blob").unwrap();
    assert_eq!(rows.iter().find(|(k, _)| k == "k1").unwrap().1, 150);
}

#[test]
fn cache_index_kinds_are_separate() {
    let db = Db::open_in_memory().unwrap();
    db.cache_touch("blob1", "blob", 100, 1).unwrap();
    db.cache_touch("blk1.b0", "block", 50, 1).unwrap();
    assert_eq!(db.cache_entries_by_kind("blob").unwrap().len(), 1);
    assert_eq!(db.cache_entries_by_kind("block").unwrap().len(), 1);
}

#[test]
fn cache_index_remove_and_remove_all() {
    let db = Db::open_in_memory().unwrap();
    // A blob plus two of its blocks (key prefix shared).
    db.cache_touch("abc", "blob", 1, 1).unwrap();
    db.cache_touch("abc.b0", "block", 1, 1).unwrap();
    db.cache_touch("abc.b1", "block", 1, 1).unwrap();
    // An unrelated entry that must survive.
    db.cache_touch("xyz", "blob", 1, 1).unwrap();

    db.cache_remove("abc.b0").unwrap();
    assert_eq!(db.cache_entries_by_kind("block").unwrap().len(), 1);

    // remove_all drops the blob row and every remaining block of that uid.
    db.cache_remove_all("abc").unwrap();
    assert!(db.cache_entries_by_kind("block").unwrap().is_empty());
    let blobs = db.cache_entries_by_kind("blob").unwrap();
    assert_eq!(blobs, vec![("xyz".into(), 1)]);
}

/// A rebuild is a replacement, not a merge: whatever the index said before
/// is what a stale or externally-deleted cache file would leave behind.
#[test]
fn cache_index_rebuild_replaces_every_row() {
    let db = Db::open_in_memory().unwrap();
    db.cache_touch("gone", "blob", 1, 1).unwrap();
    db.cache_touch("also-gone", "block", 1, 1).unwrap();

    db.cache_rebuild(&[CacheEntryInput {
        key: "kept",
        kind: "blob",
        size: 7,
        last_accessed: 42,
    }])
    .unwrap();

    assert_eq!(
        db.cache_entries_by_kind("blob").unwrap(),
        vec![("kept".to_string(), 7)]
    );
    assert!(db.cache_entries_by_kind("block").unwrap().is_empty());

    // An empty rebuild is how a cache directory that vanished reports itself.
    db.cache_rebuild(&[]).unwrap();
    assert!(db.cache_entries_by_kind("blob").unwrap().is_empty());
}

#[test]
fn pin_add_list_remove_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    db.pin_add("vol~a", "docs/a.txt", false).unwrap();
    db.pin_add("vol~d", "docs", true).unwrap();
    let list = db.pin_list().unwrap();
    assert_eq!(list.len(), 2);
    // No `nodes` row for either pin, so the kind is unknown rather than guessed
    // from the recursive flag.
    assert_eq!(
        list[0],
        PinRow {
            uid: "vol~a".into(),
            path: "docs/a.txt".into(),
            recursive: false,
            is_dir: None,
        }
    );
    assert_eq!(
        list[1],
        PinRow {
            uid: "vol~d".into(),
            path: "docs".into(),
            recursive: true,
            is_dir: None,
        }
    );

    // Once the node is cached, the pin reports the real kind — a folder pinned
    // non-recursively must not look like a file.
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("d", Some("root"), "docs")).unwrap();
    db.pin_add("vol~d", "docs", false).unwrap();
    let list = db.pin_list().unwrap();
    assert!(!list[1].recursive, "pin policy stays non-recursive");
    assert_eq!(list[1].is_dir, Some(true), "but the node is still a folder");

    // Re-pin refreshes path/flag, not a duplicate row.
    db.pin_add("vol~a", "moved/a.txt", false).unwrap();
    let list = db.pin_list().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].path, "moved/a.txt");

    assert!(db.pin_remove("vol~a").unwrap());
    assert!(!db.pin_remove("vol~a").unwrap());
    assert_eq!(db.pin_list().unwrap().len(), 1);
}

#[test]
fn is_pinned_direct_without_node_row() {
    // A direct pin counts even when the node was never hydrated into `nodes`.
    let db = Db::open_in_memory().unwrap();
    db.pin_add("vol~a", "a.txt", false).unwrap();
    assert!(db.is_pinned("vol~a").unwrap());
    assert!(!db.is_pinned("vol~b").unwrap());
}

#[test]
fn recursive_folder_pin_covers_subtree() {
    let db = Db::open_in_memory().unwrap();
    // root/docs/{report.pdf, sub/deep.txt}, root/loose.txt
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("docs", Some("root"), "Documents"))
        .unwrap();
    db.upsert_node(&file("rep", "docs", "report.pdf", 1))
        .unwrap();
    db.upsert_node(&folder("sub", Some("docs"), "Sub")).unwrap();
    db.upsert_node(&file("deep", "sub", "deep.txt", 1)).unwrap();
    db.upsert_node(&file("loose", "root", "loose.txt", 1))
        .unwrap();

    // Pin the Documents folder recursively (uids are `vol~link` display form).
    let du = |l: &str| uid(l).to_string();
    db.pin_add(&du("docs"), "Documents", true).unwrap();

    // Everything under docs (any depth) is pinned; loose.txt is not.
    assert!(db.is_pinned(&du("docs")).unwrap());
    assert!(db.is_pinned(&du("rep")).unwrap());
    assert!(db.is_pinned(&du("deep")).unwrap());
    assert!(!db.is_pinned(&du("loose")).unwrap());

    // pinned_uids expands the subtree (folder + descendants), no loose.txt.
    let mut got = db.pinned_uids().unwrap();
    got.sort();
    assert_eq!(got, vec![du("deep"), du("docs"), du("rep"), du("sub")]);

    // descendants() lists the subtree of a folder (excludes the folder).
    let mut desc = db.descendants(&du("docs")).unwrap();
    desc.sort();
    assert_eq!(desc, vec![du("deep"), du("rep"), du("sub")]);
}

#[test]
fn non_recursive_folder_pin_does_not_cover_children() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("docs", Some("root"), "Documents"))
        .unwrap();
    db.upsert_node(&file("rep", "docs", "report.pdf", 1))
        .unwrap();
    // A non-recursive pin on the folder covers only the folder itself.
    let du = |l: &str| uid(l).to_string();
    db.pin_add(&du("docs"), "Documents", false).unwrap();
    assert!(db.is_pinned(&du("docs")).unwrap());
    assert!(!db.is_pinned(&du("rep")).unwrap());
}

/// A `parent_uid` cycle is corrupt data the API can hand us, and `is_pinned`'s
/// ancestor walk is `UNION ALL` — uncapped it never terminates, while holding
/// the daemon's only SQLite connection, on a path that runs per cached read.
/// The answer here matters less than the fact that there is one.
#[test]
fn is_pinned_terminates_on_a_parent_cycle() {
    let db = Db::open_in_memory().unwrap();
    let du = |l: &str| uid(l).to_string();
    // a -> b -> a, with nothing pinned above either.
    db.upsert_node(&folder("a", Some("b"), "A")).unwrap();
    db.upsert_node(&folder("b", Some("a"), "B")).unwrap();
    db.upsert_node(&file("leaf", "a", "leaf.txt", 1)).unwrap();

    assert!(!db.is_pinned(&du("leaf")).unwrap());

    // And a pin *inside* the cycle is still found rather than walked past.
    db.pin_add(&du("a"), "A", true).unwrap();
    assert!(db.is_pinned(&du("leaf")).unwrap());
}

#[test]
fn schema_objects_exist() {
    let db = Db::open_in_memory().unwrap();
    let count: i64 = db
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM sqlite_master
                     WHERE name IN ('nodes', 'nodes_fts', 'cache_entries')",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(count, 3);
}

/// A queued op carrying a realistically-sized `meta_json`, which is what made
/// the old scan expensive: the drain read and parsed every op's sidecar to pick
/// one.
fn bulk_op(i: usize, parent: &str) -> PendingOp {
    PendingOp {
        id: 0,
        kind: OP_REVISION.to_string(),
        uid: format!("vol~link{i}"),
        parent_uid: Some(parent.to_string()),
        name: Some(format!("file-{i}.bin")),
        blob_path: Some(format!("/staging/vol~link{i}-{i}")),
        // A StagedWrite sidecar, roughly the shape and size of a real one.
        meta_json: Some(format!(
            r#"{{"uid":"vol~link{i}","len":8388608,"base_size":8388608,
                 "base_mtime":1700000000,"authored":[[0,8388608]],"complete":true,
                 "based_on":{{"mtime":1700000000,"size":8388608}}}}"#
        )),
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    }
}

/// **The B3 measurement.** The drain picks one op per iteration. Doing that by
/// reading the whole queue is quadratic in queue length; doing it in SQL with
/// `LIMIT 1` is flat. Asserts the shape of the difference, not a wall-clock
/// number — the point is that one grows with the queue and the other does not.
#[test]
fn next_due_op_does_not_scale_with_queue_length() {
    use std::time::Instant;

    let db = Db::open_in_memory().unwrap();
    let root = uid("root").to_string();
    const N: usize = 2000;
    for i in 0..N {
        db.enqueue_op(&bulk_op(i, &root)).unwrap();
    }

    // Both must agree on which op is next.
    let scanned = db
        .pending_ops()
        .unwrap()
        .into_iter()
        .find(|o| o.next_attempt_at <= 10);
    let queried = db.next_due_op(10).unwrap();
    assert_eq!(
        scanned.as_ref().map(|o| o.id),
        queried.as_ref().map(|o| o.id),
        "the new query must pick the op the old scan picked"
    );
    assert_eq!(scanned.map(|o| o.uid), queried.map(|o| o.uid));

    // Simulate a drain pass: pick the next op, retire it, repeat.
    let rounds = 200;

    let t0 = Instant::now();
    for _ in 0..rounds {
        let _ = db
            .pending_ops()
            .unwrap()
            .into_iter()
            .find(|o| o.next_attempt_at <= 10);
    }
    let scan = t0.elapsed();

    let t1 = Instant::now();
    for _ in 0..rounds {
        let _ = db.next_due_op(10).unwrap();
    }
    let query = t1.elapsed();

    println!("B3: {rounds} picks over a {N}-op queue — scan {scan:?}, query {query:?}");
    assert!(
        query * 20 < scan,
        "expected the LIMIT 1 query to be far cheaper than a full scan; \
         scan={scan:?} query={query:?}"
    );
}

/// The readiness filter moved into SQL, so it needs its own coverage there: an
/// op whose parent was itself created offline is not yet sendable and must be
/// skipped in favour of a later one that is.
#[test]
fn next_due_op_skips_ops_blocked_on_a_local_parent() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("root").to_string();

    db.enqueue_op(&bulk_op(1, "local~dir")).unwrap();
    db.enqueue_op(&bulk_op(2, &root)).unwrap();

    let next = db.next_due_op(10).unwrap().expect("an op is due");
    assert_eq!(next.uid, "vol~link2", "skipped the local-parent op");

    // A NULL parent is not blocked.
    let mut orphan = bulk_op(3, &root);
    orphan.parent_uid = None;
    db.enqueue_op(&orphan).unwrap();
    db.delete_op(next.id).unwrap();
    assert_eq!(db.next_due_op(10).unwrap().unwrap().uid, "vol~link3");
}

/// B70: a transient file's create is parked so its bytes never upload while it
/// wears that name, the park survives a write attaching its blob, and the
/// finalize rename (un-park) is what finally makes the completed file due.
#[test]
fn a_parked_transient_create_stays_off_the_drain_until_finalized() {
    let db = Db::open_in_memory().unwrap();
    let node = uid("dl").to_string();
    let parent = uid("root").to_string();
    db.enqueue_op(&PendingOp {
        id: 0,
        kind: OP_CREATE.to_string(),
        uid: node.clone(),
        parent_uid: Some(parent),
        name: Some("movie.mkv.part".to_string()),
        blob_path: None,
        meta_json: None,
        created_at: 1,
        attempts: 0,
        last_error: None,
        next_attempt_at: PARK_UNTIL,
    })
    .unwrap();

    // Parked: never selected, however far the clock advances.
    assert!(
        db.next_due_op(PARK_UNTIL - 1).unwrap().is_none(),
        "a parked create is never due"
    );

    // A write attaches its blob to the create but must not wake it.
    let attached = db
        .attach_blob_to_create(&node, "/staging/blob1", "{}")
        .unwrap();
    assert!(attached.is_some(), "the blob attached to the create");
    assert!(
        db.next_due_op(PARK_UNTIL - 1).unwrap().is_none(),
        "still parked after a write attaches"
    );

    // Finalize: un-park, and the completed file is due to upload with its bytes.
    assert!(
        db.set_create_hold(&node, false).unwrap(),
        "a create row was un-parked"
    );
    let due = db
        .next_due_op(1_000)
        .unwrap()
        .expect("the un-parked create is due");
    assert_eq!(due.uid, node);
    assert_eq!(due.blob_path.as_deref(), Some("/staging/blob1"));
}

/// Backoff still gates: nothing is returned before an op is due.
#[test]
fn next_due_op_respects_backoff() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("root").to_string();
    let id = db.enqueue_op(&bulk_op(1, &root)).unwrap().0;
    db.record_op_failure(id, "boom", 5_000).unwrap();

    assert!(
        db.next_due_op(4_999).unwrap().is_none(),
        "still backing off"
    );
    assert!(db.next_due_op(5_000).unwrap().is_some(), "due now");
}

/// **The B4 measurement.** `enforce_block_budget` runs on *every* `store_block`,
/// i.e. once per 4 MiB of every cold read. The old path read and sorted every
/// row of the cache index to answer a question that is almost always "no, we are
/// under budget" — under the shared connection lock, so it also stalled FUSE
/// metadata calls.
///
/// Asserts correctness — the aggregate must agree with summing the rows, and
/// must count the kinds apart — and *reports* the timing without asserting on
/// it. See the note at the measurement for why.
#[test]
fn cache_total_bytes_agrees_with_summing_the_rows() {
    use std::time::Instant;

    let db = Db::open_in_memory().unwrap();
    // A 20 GB block cache at 4 MiB a block is ~5000 rows.
    const N: usize = 5000;
    const BLOCK: u64 = 4 << 20;
    for i in 0..N {
        db.cache_touch(&format!("k{i}.b0"), "block", BLOCK, i as i64)
            .unwrap();
    }

    // The aggregate must agree with summing the rows.
    let summed: u64 = db
        .cache_entries_by_kind("block")
        .unwrap()
        .iter()
        .map(|(_, s)| s)
        .sum();
    assert_eq!(db.cache_total_bytes("block").unwrap(), summed);
    assert_eq!(summed, N as u64 * BLOCK);
    // Kinds are counted apart.
    assert_eq!(db.cache_total_bytes("blob").unwrap(), 0);

    let rounds = 500;

    let t0 = Instant::now();
    for _ in 0..rounds {
        let entries = db.cache_entries_by_kind("block").unwrap();
        let _: u64 = entries.iter().map(|(_, s)| *s).sum();
    }
    let scan = t0.elapsed();

    let t1 = Instant::now();
    for _ in 0..rounds {
        let _ = db.cache_total_bytes("block").unwrap();
    }
    let aggregate = t1.elapsed();

    // Reported, not asserted. The SUM is cheaper than reading every row — no
    // materialization, no allocation, no sort — but it is still O(rows), so this
    // is a constant factor, and an assertion on a constant factor measured
    // alongside the rest of the suite tests the machine's load as much as the
    // query. The claim that matters — that the *common* path does not touch the
    // database at all — is pinned by `budget_check_is_free_when_under_budget`,
    // which compares against itself and so is load-independent.
    println!(
        "B4: {rounds} budget checks over {N} entries — scan {scan:?}, aggregate {aggregate:?}"
    );
}

/// Victims still come out least-recently-accessed first, now in bounded batches
/// rather than one unbounded read of the table.
#[test]
fn cache_eviction_candidates_are_lru_ordered_and_limited() {
    let db = Db::open_in_memory().unwrap();
    for i in 0..10u64 {
        // Insert newest-first so insertion order cannot be mistaken for LRU order.
        db.cache_touch(&format!("k{i}"), "blob", 100, (10 - i) as i64)
            .unwrap();
    }
    db.cache_touch("other", "block", 100, 0).unwrap();

    let batch = db.cache_eviction_candidates("blob", 3).unwrap();
    assert_eq!(batch.len(), 3, "honours the limit");
    let keys: Vec<&str> = batch.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["k9", "k8", "k7"], "least-recently-accessed first");
    assert!(
        batch.iter().all(|(k, _)| k != "other"),
        "a different kind is never a candidate"
    );
}

/// **The B5 checkpoint.** `improvements.md` P2.3 proposes concurrent SQLite
/// reads, on the premise that the single `Mutex<Connection>` serializes the FUSE
/// workers. This measures what the connection is actually asked to do now that
/// B3 and B4 have landed, so the proposal is decided on evidence.
///
/// Note what is *not* here: `lookup`/`getattr`/`readdir` do not read this
/// database in the steady state — they serve from `State::entries` in memory and
/// only write through on a cold fill. The per-read DB operation is
/// `cache_accessed`, one `UPDATE` per cache hit, which is what this drives.
#[test]
fn db_contention_under_fuse_worker_load() {
    use std::sync::Arc;
    use std::time::Instant;

    let db = Arc::new(Db::open_in_memory().unwrap());
    for i in 0..5000u64 {
        db.cache_touch(&format!("k{i}.b0"), "block", 4 << 20, i as i64)
            .unwrap();
    }

    // Eight workers, matching FUSE_WORKERS, each doing what a served block read
    // does to the database: one LRU touch.
    const WORKERS: usize = 8;
    const PER_WORKER: usize = 2000;

    let t = Instant::now();
    let mut handles = Vec::new();
    for w in 0..WORKERS {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_WORKER {
                db.cache_accessed(&format!("k{}.b0", (w * PER_WORKER + i) % 5000), i as i64)
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let concurrent = t.elapsed();

    // The same work on one thread, for the serialization baseline.
    let t = Instant::now();
    for i in 0..(WORKERS * PER_WORKER) {
        db.cache_accessed(&format!("k{}.b0", i % 5000), i as i64)
            .unwrap();
    }
    let serial = t.elapsed();

    let total = WORKERS * PER_WORKER;
    println!(
        "B5: {total} LRU touches — {WORKERS} threads {concurrent:?}, 1 thread {serial:?} \
         (per-op {:?} vs {:?})",
        concurrent / total as u32,
        serial / total as u32
    );
}

#[test]
fn test_db_has_children_and_trashed_filtering() {
    use proton_drive_rs::proton_sdk::ids::{LinkId, VolumeId};
    let db = Db::open_in_memory().unwrap();
    let parent_uid = NodeUid::new(VolumeId::from("vol"), LinkId::from("parent"));
    let child_uid = NodeUid::new(VolumeId::from("vol"), LinkId::from("child"));

    assert!(
        !db.has_children(&parent_uid).unwrap(),
        "no children initially"
    );

    let parent_node = Node {
        uid: parent_uid.clone(),
        parent_uid: None,
        name: "parent".into(),
        kind: NodeKind::Folder,
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
    };
    db.upsert_node(&parent_node).unwrap();

    let mut child_node = Node {
        uid: child_uid.clone(),
        parent_uid: Some(parent_uid.clone()),
        name: "child.txt".into(),
        kind: NodeKind::File {
            media_type: "text/plain".into(),
            total_size_on_storage: 10,
            active_revision_state: None,
            active_revision_id: None,
            content_sha1: None,
            claimed_size: Some(10),
            claimed_modification_time: None,
        },
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
    };
    db.upsert_node(&child_node).unwrap();

    assert!(db.has_children(&parent_uid).unwrap(), "child node present");

    // Trashing child removes it from active children
    child_node.trashed = true;
    db.upsert_node(&child_node).unwrap();

    assert!(
        !db.has_children(&parent_uid).unwrap(),
        "trashed child ignored"
    );
}

#[test]
fn test_db_has_create_op() {
    let db = Db::open_in_memory().unwrap();
    let local_uid = "local~testnode12345678901234567890";

    assert!(!db.has_create_op(local_uid).unwrap(), "no op initially");

    let op = ops::PendingOp {
        id: 0,
        kind: ops::OP_CREATE.to_string(),
        uid: local_uid.to_string(),
        parent_uid: Some("vol~parent".to_string()),
        name: Some("test.txt".to_string()),
        blob_path: None,
        meta_json: None,
        created_at: 1000,
        attempts: 0,
        last_error: None,
        next_attempt_at: 0,
    };
    db.enqueue_op(&op).unwrap();

    assert!(db.has_create_op(local_uid).unwrap(), "create op present");

    db.delete_ops_for_uid(local_uid).unwrap();
    assert!(!db.has_create_op(local_uid).unwrap(), "op deleted");
}

/// The album listing is a wholesale replacement, and an album that leaves the
/// listing must take its contents with it — nothing else ever deletes those rows.
#[test]
fn albums_replace_drops_the_contents_of_an_album_that_left() {
    let db = Db::open_in_memory().unwrap();
    let album = |uid: &str, name: &str, activity: Option<i64>| StoredAlbum {
        uid: uid.into(),
        name: name.into(),
        photo_count: 2,
        cover_uid: Some("cover".into()),
        last_activity: activity,
        shared: false,
    };
    db.albums_replace(&[
        album("a1", "Trip", Some(500)),
        album("a2", "Cats", Some(400)),
    ])
    .unwrap();
    db.album_photos_replace("a1", &[("p1".into(), 300, None, None)])
        .unwrap();
    db.album_photos_replace("a2", &[("p2".into(), 200, None, None)])
        .unwrap();

    // The next refresh no longer has a2 — it was deleted, or unshared.
    db.albums_replace(&[album("a1", "Trip", Some(600))])
        .unwrap();

    assert_eq!(db.albums_count().unwrap(), 1);
    assert_eq!(db.album_photos_count("a1").unwrap(), 1);
    assert_eq!(
        db.album_photos_count("a2").unwrap(),
        0,
        "a departed album's photos go with it"
    );
}

/// An album's contents are replaced like the timeline: server order is kept, and
/// what a thumbnail attempt cost a download to learn survives the refresh.
#[test]
fn album_photos_replace_keeps_what_was_learned() {
    let db = Db::open_in_memory().unwrap();
    db.albums_replace(&[StoredAlbum {
        uid: "a1".into(),
        name: "Trip".into(),
        photo_count: 3,
        cover_uid: None,
        last_activity: None,
        shared: true,
    }])
    .unwrap();
    db.album_photos_replace(
        "a1",
        &[
            ("p1".into(), 300, None, Some("video/mp4".into())),
            ("p2".into(), 200, None, None),
        ],
    )
    .unwrap();

    db.album_photo_set_thumb("p1", THUMB_HAVE, Some(1.5))
        .unwrap();
    db.album_photo_set_thumb("p2", THUMB_NONE, None).unwrap();

    db.album_photos_replace(
        "a1",
        &[
            ("p0".into(), 400, Some("new.jpg".into()), None),
            ("p1".into(), 300, None, None),
            ("p2".into(), 200, None, None),
        ],
    )
    .unwrap();

    let page = db.album_photos_page("a1", 0, 10).unwrap();
    assert_eq!(
        page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p0", "p1", "p2"],
        "the album's own order is preserved"
    );
    assert_eq!(page[1].ratio, Some(1.5));
    assert_eq!(page[1].thumb_state, THUMB_HAVE);
    assert_eq!(page[2].thumb_state, THUMB_NONE);
    // The media type is learned-and-kept, so p1 stays classified as a video even
    // though the second refresh carried none.
    assert_eq!(page[1].kind, crate::control::PhotoKind::Video);
    assert_eq!(page[0].thumb_state, THUMB_UNKNOWN);

    // Paging is relative to the album, and the shared flag round-trips.
    let second = db.album_photos_page("a1", 1, 1).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].uid, "p1");
    assert!(db.albums_list().unwrap()[0].shared);
}

/// A photo held only by an album — everything in an album shared with us — is
/// still resolvable for its capture time, which is what tags its cached
/// thumbnail. One row per uid, even when the photo is in several albums.
#[test]
fn album_photos_by_uid_resolves_a_photo_outside_the_timeline() {
    let db = Db::open_in_memory().unwrap();
    db.album_photos_replace("a1", &[("p1".into(), 300, None, None)])
        .unwrap();
    db.album_photos_replace("a2", &[("p1".into(), 300, None, None)])
        .unwrap();

    let found = db
        .album_photos_by_uid(&["p1".into(), "absent".into()])
        .unwrap();
    assert_eq!(found.len(), 1, "a photo in two albums resolves once");
    assert_eq!(found[0].capture_time, 300);
    assert!(db.album_photos_by_uid(&[]).unwrap().is_empty());
}

/// Per-album freshness stamps are keyed by uid, so invalidating them all is a
/// prefix sweep — and it must not take unrelated keys with it.
#[test]
fn clear_state_prefix_takes_only_the_prefixed_keys() {
    let db = Db::open_in_memory().unwrap();
    db.set_state_i64("album_synced_ms:vol~a1", 1).unwrap();
    db.set_state_i64("album_synced_ms:vol~a2", 2).unwrap();
    db.set_state_i64("albums_synced_ms", 3).unwrap();

    db.clear_state_prefix("album_synced_ms:").unwrap();

    assert_eq!(db.state_i64("album_synced_ms:vol~a1").unwrap(), None);
    assert_eq!(db.state_i64("album_synced_ms:vol~a2").unwrap(), None);
    assert_eq!(
        db.state_i64("albums_synced_ms").unwrap(),
        Some(3),
        "the listing's own stamp is not a per-album one"
    );
}

// --- `nodes.path`, and the subtree work it replaces (DB3) -------------------

/// Read the stored path column directly: the point of these tests is that the
/// column itself is right, not that `path_of` can still fall back to a walk.
fn stored_path(db: &Db, link: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    db.with_conn(|conn| {
        Ok(conn
            .query_row(
                "SELECT path FROM nodes WHERE uid = ?1",
                [uid(link).to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    })
    .unwrap()
}

#[test]
fn a_node_stores_its_path_when_it_is_written() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("docs", Some("root"), "Documents"))
        .unwrap();
    db.upsert_node(&file("f1", "docs", "report.pdf", 1))
        .unwrap();

    // The root's name is the mount, so it contributes nothing.
    assert_eq!(stored_path(&db, "root").as_deref(), Some(""));
    assert_eq!(stored_path(&db, "docs").as_deref(), Some("Documents"));
    assert_eq!(
        stored_path(&db, "f1").as_deref(),
        Some("Documents/report.pdf")
    );
}

#[test]
fn a_node_below_an_uncached_parent_starts_its_own_path() {
    // A device folder's root lives in `device`/`sync_folder` and never gets a
    // `nodes` row, so its children are the top of the cached chain.
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("orphan", Some("never-cached"), "Work"))
        .unwrap();
    db.upsert_node(&file("f1", "orphan", "notes.txt", 1))
        .unwrap();

    assert_eq!(stored_path(&db, "orphan").as_deref(), Some("Work"));
    assert_eq!(stored_path(&db, "f1").as_deref(), Some("Work/notes.txt"));
}

#[test]
fn renaming_a_folder_rewrites_every_descendant_path() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("a", Some("root"), "Before"))
        .unwrap();
    db.upsert_node(&folder("b", Some("a"), "Inner")).unwrap();
    db.upsert_node(&file("f1", "b", "deep.txt", 1)).unwrap();
    assert_eq!(
        stored_path(&db, "f1").as_deref(),
        Some("Before/Inner/deep.txt")
    );

    db.upsert_node(&folder("a", Some("root"), "After")).unwrap();

    assert_eq!(stored_path(&db, "a").as_deref(), Some("After"));
    assert_eq!(stored_path(&db, "b").as_deref(), Some("After/Inner"));
    assert_eq!(
        stored_path(&db, "f1").as_deref(),
        Some("After/Inner/deep.txt")
    );
    let hit = &db.search("deep.txt", 10).unwrap()[0];
    assert_eq!(
        hit.path, "After/Inner/deep.txt",
        "the search index must carry the rewritten path too"
    );
}

#[test]
fn moving_a_folder_rewrites_every_descendant_path() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("old", Some("root"), "Old")).unwrap();
    db.upsert_node(&folder("new", Some("root"), "New")).unwrap();
    db.upsert_node(&folder("moved", Some("old"), "Moved"))
        .unwrap();
    db.upsert_node(&file("f1", "moved", "deep.txt", 1)).unwrap();

    db.upsert_node(&folder("moved", Some("new"), "Moved"))
        .unwrap();

    assert_eq!(stored_path(&db, "moved").as_deref(), Some("New/Moved"));
    assert_eq!(
        stored_path(&db, "f1").as_deref(),
        Some("New/Moved/deep.txt")
    );
}

#[test]
fn trashing_a_folder_drops_its_subtree_from_the_index() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&folder("a", Some("root"), "Holder"))
        .unwrap();
    db.upsert_node(&file("f1", "a", "hidden.txt", 1)).unwrap();
    assert_eq!(db.search("hidden", 10).unwrap().len(), 1);

    let mut trashed = folder("a", Some("root"), "Holder");
    trashed.trashed = true;
    db.upsert_node(&trashed).unwrap();

    assert_eq!(
        db.search("hidden", 10).unwrap().len(),
        0,
        "a file under a trashed folder is not reachable and must not be findable"
    );

    // …and comes back when the folder does.
    db.upsert_node(&folder("a", Some("root"), "Holder"))
        .unwrap();
    assert_eq!(db.search("hidden", 10).unwrap().len(), 1);
}

/// The re-listing case, which is most of what the write path actually does: a
/// folder upserted with the same name and parent must not touch its subtree.
///
/// Written as a ratio between two subtree sizes, so it fails on the scaling
/// regression rather than on a slow machine. Before this, every re-listing paid
/// a `path_of` walk plus an ancestor walk per descendant.
#[test]
fn re_listing_a_folder_does_not_scale_with_its_subtree() {
    fn elapsed_for(children: usize) -> std::time::Duration {
        let db = Db::open_in_memory().unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&folder("big", Some("root"), "Big")).unwrap();
        let kids: Vec<Node> = (0..children)
            .map(|i| file(&format!("k{i}"), "big", &format!("child-{i}.txt"), 1))
            .collect();
        db.upsert_nodes(&kids).unwrap();

        let unchanged = folder("big", Some("root"), "Big");
        let start = std::time::Instant::now();
        for _ in 0..20 {
            db.upsert_node(&unchanged).unwrap();
        }
        start.elapsed()
    }

    let small = elapsed_for(100);
    let large = elapsed_for(2_000);
    assert!(
        large < small * 8,
        "re-listing a folder scaled with its subtree: {small:?} for 100 children, \
         {large:?} for 2000 (20x the descendants)"
    );
}

#[test]
fn migration_v25_backfills_paths_for_an_existing_tree() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-db-v24-fixture-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let db = Db::open(&path).unwrap();
        db.upsert_node(&folder("root", None, "My Files")).unwrap();
        db.upsert_node(&folder("docs", Some("root"), "Documents"))
            .unwrap();
        db.upsert_node(&file("f1", "docs", "report.pdf", 1))
            .unwrap();
        db.upsert_node(&folder("orphan", Some("never-cached"), "Work"))
            .unwrap();
        db.upsert_node(&file("f2", "orphan", "notes.txt", 1))
            .unwrap();
    }
    {
        // Put the file back in the state a released V24 database was in.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE nodes DROP COLUMN path;
             UPDATE sync_state SET value = '24' WHERE key = 'schema_version';",
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    assert_eq!(stored_path(&db, "root").as_deref(), Some(""));
    assert_eq!(stored_path(&db, "docs").as_deref(), Some("Documents"));
    assert_eq!(
        stored_path(&db, "f1").as_deref(),
        Some("Documents/report.pdf"),
        "an existing tree must come out of the migration with the same paths \
         the walk used to produce"
    );
    // A subtree whose top has an uncached parent is ordinary data, not an error.
    assert_eq!(stored_path(&db, "orphan").as_deref(), Some("Work"));
    assert_eq!(stored_path(&db, "f2").as_deref(), Some("Work/notes.txt"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.lock", path.display()));
}

/// A queue carried over from V26 keeps its rows, and every one of them starts
/// out not access-deferred — a migration must not invent a deferral window that
/// would report an ordinary queued upload as blocked.
#[test]
fn migration_v27_adds_the_access_deferral_column_to_a_v26_queue() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-db-v26-fixture-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let db = Db::open(&path).unwrap();
        db.enqueue_op(&crate::db::PendingOp {
            id: 0,
            kind: crate::db::OP_REVISION.to_string(),
            uid: "v~l".to_string(),
            parent_uid: None,
            name: None,
            blob_path: Some("/tmp/blob".to_string()),
            meta_json: Some("{}".to_string()),
            created_at: 1,
            attempts: 0,
            last_error: None,
            next_attempt_at: 0,
        })
        .unwrap();
    }
    {
        // Put the file back in the state a released V26 database was in.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE pending_op DROP COLUMN access_deferred_since;
             UPDATE sync_state SET value = '26' WHERE key = 'schema_version';",
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    let ops = db.pending_ops().unwrap();
    assert_eq!(ops.len(), 1, "the migration keeps the queued write");
    let now = 5_000i64;
    assert_eq!(
        db.defer_op_for_access(ops[0].id, now, now + 5_000).unwrap(),
        now,
        "a carried-over op is not already deferred"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.lock", path.display()));
}

/// A mirror baseline carried over from V27 keeps its rows, and each one reads
/// back with no nanosecond time. That `None` is the whole point: it makes the
/// comparison fall back to whole seconds for rows written before the column
/// existed, instead of asserting a sub-second time of zero and re-uploading
/// every already-synced file in the folder (bugs.md B25).
#[test]
fn migration_v28_adds_sub_second_local_times_without_disturbing_a_v27_baseline() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-db-v27-fixture-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let db = Db::open(&path).unwrap();
        let folder = db
            .sync_folder_add("/home/me/Mirror", "vol~root", "share")
            .unwrap();
        db.sync_entry_upsert(
            folder,
            &StoredSyncEntry {
                rel_path: "notes.txt".into(),
                remote_uid: Some("vol~notes".into()),
                local_mtime: 1_700_000_000,
                local_mtime_ns: Some(1_700_000_000_123_456_789),
                local_size: 42,
                remote_rev: Some("1700000000".into()),
                remote_hash: Some("42".into()),
            },
        )
        .unwrap();
    }
    {
        // Put the file back in the state a released V27 database was in.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE sync_entry DROP COLUMN local_mtime_ns;
             UPDATE sync_state SET value = '27' WHERE key = 'schema_version';",
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    let entries = db.sync_entries(1).unwrap();
    let entry = entries.get("notes.txt").expect("the baseline row survives");
    assert_eq!(entry.local_mtime, 1_700_000_000);
    assert_eq!(entry.local_size, 42);
    assert_eq!(
        entry.local_mtime_ns, None,
        "a carried-over row must not claim a sub-second time it never recorded"
    );

    // And the column is live: a refreshed row keeps the precision it is given.
    db.sync_entry_upsert(
        1,
        &StoredSyncEntry {
            local_mtime_ns: Some(1_700_000_001_000_000_500),
            ..entry.clone()
        },
    )
    .unwrap();
    assert_eq!(
        db.sync_entries(1).unwrap()["notes.txt"].local_mtime_ns,
        Some(1_700_000_001_000_000_500)
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.lock", path.display()));
}

// --- read-only connection pool (DB2) ---------------------------------------

/// A read must not queue behind whatever happens to be writing.
///
/// The write connection is held for the whole measurement — which is what a
/// large listing's commit does to it — while a reader asks for a search. Before
/// the pool this was the same mutex, so the read waited for the writer;
/// `busy_timeout` never entered into it, because the contention was this
/// process's own lock rather than SQLite's.
#[test]
fn a_read_does_not_wait_for_the_write_connection() {
    let path = std::env::temp_dir().join(format!(
        "pdfs-db-readpool-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let db = std::sync::Arc::new(Db::open(&path).unwrap());
    db.upsert_node(&folder("root", None, "My Files")).unwrap();
    db.upsert_node(&file("f1", "root", "findme.txt", 1))
        .unwrap();

    let held = std::time::Duration::from_millis(500);
    let (tx, rx) = std::sync::mpsc::channel();
    let writer = {
        let db = db.clone();
        std::thread::spawn(move || {
            db.with_conn(|_conn| {
                tx.send(()).unwrap();
                std::thread::sleep(held);
                Ok(())
            })
            .unwrap();
        })
    };
    rx.recv().unwrap();

    let start = std::time::Instant::now();
    let hits = db.search("findme", 10).unwrap();
    let waited = start.elapsed();
    writer.join().unwrap();

    assert_eq!(hits.len(), 1, "the read must still see committed data");
    assert!(
        waited < held / 2,
        "the read waited {waited:?} for a write connection held {held:?}"
    );

    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.lock", path.display()));
}

/// §5: the drain is several workers over one queue, and the claim column is the
/// only thing keeping them off each other's work. Two properties matter — no
/// row is ever handed to two workers, and no *node* is ever worked by two
/// workers even though its ops are separate rows.
#[test]
fn a_claimed_op_is_invisible_to_every_other_worker() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("root").to_string();
    for i in 0..3 {
        db.enqueue_op(&bulk_op(i, &root)).unwrap();
    }

    let first = db.claim_next_due_op(10).unwrap().expect("an op is due");
    let second = db
        .claim_next_due_op(10)
        .unwrap()
        .expect("another op is due");
    let third = db
        .claim_next_due_op(10)
        .unwrap()
        .expect("a third op is due");
    assert!(
        db.claim_next_due_op(10).unwrap().is_none(),
        "a fourth claim must find nothing left: every queued op is taken"
    );

    let mut ids = [first.id, second.id, third.id];
    ids.sort_unstable();
    assert_eq!(
        ids,
        [1, 2, 3],
        "each worker must get a distinct row, oldest first"
    );

    // Releasing puts one back, and it is the one that comes out next.
    db.release_op_claim(second.id).unwrap();
    let again = db.claim_next_due_op(10).unwrap().expect("the released op");
    assert_eq!(again.id, second.id);
}

#[test]
fn two_workers_never_share_a_node() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("root").to_string();
    // Two ops of different kinds against one node — a queued rename and a
    // queued revision, which is an ordinary `mv` plus a save.
    let mut revision = bulk_op(0, &root);
    revision.parent_uid = None;
    db.enqueue_op(&revision).unwrap();
    let rename = PendingOp {
        kind: OP_RENAME.to_string(),
        parent_uid: Some(root.clone()),
        blob_path: None,
        meta_json: None,
        ..bulk_op(0, &root)
    };
    db.enqueue_op(&rename).unwrap();

    let claimed = db.claim_next_due_op(10).unwrap().expect("an op is due");
    assert_eq!(claimed.kind, OP_REVISION, "the older op goes first");
    assert!(
        db.claim_next_due_op(10).unwrap().is_none(),
        "the second op targets the same node, so it must wait for the first: \
         two workers on one uid would race over its staged blob and land its \
         changes out of order"
    );

    db.release_op_claim(claimed.id).unwrap();
    db.delete_op(claimed.id).unwrap();
    assert_eq!(
        db.claim_next_due_op(10).unwrap().map(|o| o.kind),
        Some(OP_RENAME.to_string()),
        "once the revision retires, the rename is claimable"
    );
}

#[test]
fn claims_do_not_survive_the_process_that_took_them() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("root").to_string();
    for i in 0..2 {
        db.enqueue_op(&bulk_op(i, &root)).unwrap();
    }
    db.claim_next_due_op(10).unwrap().unwrap();
    db.claim_next_due_op(10).unwrap().unwrap();
    assert!(db.claim_next_due_op(10).unwrap().is_none());

    // What `Db::open` does after the single-writer lock says the previous run
    // is gone. Without it those ops are invisible to every worker forever.
    assert_eq!(db.clear_op_claims().unwrap(), 2);
    assert!(
        db.claim_next_due_op(10).unwrap().is_some(),
        "a crashed run's claims must not park its queue"
    );
}

#[test]
fn an_idle_worker_does_not_wait_on_an_op_another_worker_has() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("root").to_string();
    db.enqueue_op(&bulk_op(0, &root)).unwrap();

    assert_eq!(db.earliest_due_at().unwrap(), Some(0));
    db.claim_next_due_op(10).unwrap().unwrap();
    assert_eq!(
        db.earliest_due_at().unwrap(),
        None,
        "an op somebody else is draining is not work this worker is waiting for"
    );
}

/// Trashing a photo from the gallery must not leave it behind on an album page:
/// the row goes, its album membership goes with it, and nothing else moves.
#[test]
fn photos_delete_removes_the_photo_and_its_album_membership() {
    let db = Db::open_in_memory().unwrap();
    db.photos_replace(&[
        TimelineRow::new("p1", 300),
        TimelineRow::new("p2", 200),
        TimelineRow::new("p3", 100),
    ])
    .unwrap();
    db.albums_replace(&[StoredAlbum {
        uid: "a1".into(),
        name: "Trip".into(),
        photo_count: 3,
        cover_uid: Some("p1".into()),
        last_activity: Some(500),
        shared: false,
    }])
    .unwrap();
    db.album_photos_replace(
        "a1",
        &[
            ("p1".into(), 300, None, None),
            ("p2".into(), 200, None, None),
            ("p3".into(), 100, None, None),
        ],
    )
    .unwrap();

    assert_eq!(db.photos_delete(&["p2".into()]).unwrap(), 1);

    let page = db.photos_page(0, 10, None, None, false).unwrap();
    assert_eq!(
        page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p1", "p3"],
        "only the trashed photo leaves the timeline"
    );
    assert_eq!(db.album_photos_count("a1").unwrap(), 2);
    let album = db.album_photos_page("a1", 0, 10).unwrap();
    assert_eq!(
        album.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p1", "p3"]
    );

    // A uid we no longer hold is not an error: the event poller and the trash
    // reply can both report the same deletion.
    assert_eq!(db.photos_delete(&["p2".into()]).unwrap(), 0);
    assert_eq!(db.photos_delete(&[]).unwrap(), 0);
}

/// A shot taken as RAW+JPEG is one photo to the person who took it: the grid
/// shows the JPEG once, and the Raw tab still lists the raw file.
#[test]
fn a_raw_and_its_jpeg_are_one_entry_in_the_grid() {
    let db = Db::open_in_memory().unwrap();
    db.photos_replace(&[
        TimelineRow {
            name: Some("IMG_1234.JPG".into()),
            ..TimelineRow::new("jpeg", 300)
        },
        TimelineRow {
            name: Some("img_1234.cr2".into()),
            ..TimelineRow::new("raw", 300)
        },
        TimelineRow {
            name: Some("IMG_9999.JPG".into()),
            ..TimelineRow::new("alone", 200)
        },
    ])
    .unwrap();

    let page = db.photos_page(0, 10, None, None, false).unwrap();
    assert_eq!(
        page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["jpeg", "alone"],
        "the pair shows once, as the member that can be displayed"
    );
    assert_eq!(page[0].group_size, 2);
    assert!(page[0].has_raw, "the tile can say the raw is there");
    assert_eq!(page[1].group_size, 1);
    assert!(!page[1].has_raw);

    // The Raw tab answers "which raws do I have", so it lists files.
    let raws = db
        .photos_page(0, 10, Some(crate::control::PhotoKind::Raw), None, false)
        .unwrap();
    assert_eq!(
        raws.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["raw"]
    );
    assert_eq!(db.photos_counts().unwrap(), (2, 0, 1));

    // Both members are reachable from either uid: the lightbox switches between
    // them, and deleting the tile has to trash both.
    let group = db.photos_group("raw").unwrap();
    assert_eq!(
        group.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["jpeg", "raw"]
    );
    assert_eq!(db.photos_group("alone").unwrap().len(), 1);
}

/// Two files that merely share a name are not one shot: the same stem on
/// different days, or two files of the same kind, stay apart.
#[test]
fn a_shared_name_alone_does_not_group_photos() {
    let db = Db::open_in_memory().unwrap();
    db.photos_replace(&[
        TimelineRow {
            name: Some("IMG_1234.JPG".into()),
            ..TimelineRow::new("today", 1_700_000_000)
        },
        TimelineRow {
            name: Some("IMG_1234.CR2".into()),
            ..TimelineRow::new("last-year", 1_600_000_000)
        },
        TimelineRow {
            name: Some("IMG_1234.PNG".into()),
            ..TimelineRow::new("same-kind", 1_700_000_000)
        },
    ])
    .unwrap();

    let page = db.photos_page(0, 10, None, None, false).unwrap();
    assert_eq!(page.len(), 3, "three shots, three tiles");
    assert!(page.iter().all(|p| p.group_size == 1));
}

/// The server knows about live photos and bursts, so its own relation groups
/// files this client would never pair by name.
#[test]
fn the_servers_photo_relation_groups_a_live_photo() {
    let db = Db::open_in_memory().unwrap();
    db.photos_replace(&[
        TimelineRow {
            name: Some("IMG_0001.HEIC".into()),
            ..TimelineRow::new("main", 300)
        },
        TimelineRow {
            name: Some("clip.mov".into()),
            main_uid: Some("main".into()),
            ..TimelineRow::new("related", 299)
        },
    ])
    .unwrap();

    let page = db.photos_page(0, 10, None, None, false).unwrap();
    assert_eq!(
        page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["main"]
    );
    assert_eq!(page[0].group_size, 2);
    assert!(!page[0].has_raw, "a live photo's video is not a raw file");

    // The relation is learned-and-kept: a refresh that could not resolve the
    // nodes must not break the group up.
    db.photos_replace(&[
        TimelineRow::new("main", 300),
        TimelineRow::new("related", 299),
    ])
    .unwrap();
    assert_eq!(db.photos_group("related").unwrap().len(), 2);
}
