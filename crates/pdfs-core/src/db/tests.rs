use super::activity::ACTIVITY_KEEP;
use super::migrations::SCHEMA_VERSION;
use super::*;
use crate::control::{ActivityEntry, ActivityKind};
use crate::localindex::LocalEntry;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{Node, NodeKind};

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
        ("p1".into(), 300, None, Some("video/mp4".into())),
        ("p2".into(), 200, None, None),
        ("p3".into(), 100, None, None),
    ])
    .unwrap();

    // A thumbnail attempt teaches us p1's ratio and that p2 can never have one.
    db.photo_set_thumb("p1", THUMB_HAVE, Some(1.5)).unwrap();
    db.photo_set_thumb("p2", THUMB_NONE, None).unwrap();

    // The next refresh brings a new photo, keeps p1 and p2, and loses p3.
    // p1's media type arrives as `None` this time and must not be forgotten.
    db.photos_replace(&[
        ("p0".into(), 400, Some("new.jpg".into()), None),
        ("p1".into(), 300, None, None),
        ("p2".into(), 200, None, None),
    ])
    .unwrap();

    let page = db.photos_page(0, 10, None, None).unwrap();
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
        .photos_page(0, 10, Some(crate::control::PhotoKind::Video), None)
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
    let ranged = db.photos_page(0, 10, None, Some((150, 350))).unwrap();
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
fn photos_page_slices_the_timeline_in_order() {
    let db = Db::open_in_memory().unwrap();
    let items: Vec<_> = (0..5)
        .map(|i| (format!("p{i}"), 500 - i as i64, None, None::<String>))
        .collect();
    db.photos_replace(&items).unwrap();

    let page = db.photos_page(2, 2, None, None).unwrap();
    assert_eq!(
        page.iter().map(|p| p.uid.as_str()).collect::<Vec<_>>(),
        ["p2", "p3"]
    );
    assert!(db.photos_page(9, 2, None, None).unwrap().is_empty());
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
        },
        StoredTrash {
            uid: "t2".into(),
            name: "Alpha".into(),
            is_dir: true,
            size: 0,
            mtime: 2,
        },
    ])
    .unwrap();

    let items = db.trash_list().unwrap();
    assert_eq!(
        items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        ["Alpha", "zeta.txt"]
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
    db.upsert_node(&folder("dst", Some("root"), "dest")).unwrap();
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
    assert_eq!(list[0], ("vol~a".into(), "docs/a.txt".into(), false));
    assert_eq!(list[1], ("vol~d".into(), "docs".into(), true));

    // Re-pin refreshes path/flag, not a duplicate row.
    db.pin_add("vol~a", "moved/a.txt", false).unwrap();
    let list = db.pin_list().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].1, "moved/a.txt");

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

/// Backoff still gates: nothing is returned before an op is due.
#[test]
fn next_due_op_respects_backoff() {
    let db = Db::open_in_memory().unwrap();
    let root = uid("root").to_string();
    let id = db.enqueue_op(&bulk_op(1, &root)).unwrap().0;
    db.record_op_failure(id, "boom", 5_000).unwrap();

    assert!(db.next_due_op(4_999).unwrap().is_none(), "still backing off");
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
    println!("B4: {rounds} budget checks over {N} entries — scan {scan:?}, aggregate {aggregate:?}");
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
