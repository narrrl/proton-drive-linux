//! The photos timeline: a flat, date-ordered projection of the photo share,
//! persisted so the gallery opens instantly instead of re-enumerating on launch.

use rusqlite::params;

use std::collections::HashMap;

use super::Db;
use crate::Result;

/// A photo whose thumbnail state is not known yet: it has never been asked for.
pub const THUMB_UNKNOWN: i64 = 0;
/// A thumbnail exists for this photo — served by the server, or generated locally
/// from the full file when the server had none.
pub const THUMB_HAVE: i64 = 1;
/// This photo can never be given a thumbnail: the server has none and the bytes
/// could not be decoded locally either. Never retried.
pub const THUMB_NONE: i64 = 2;

/// One photo of the persisted timeline. The timeline itself is server-ordered
/// (newest first) and stored with that order in `seq`; `ratio` and `thumb_state`
/// are locally learned and survive a refresh of the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredPhoto {
    pub uid: String,
    pub capture_time: i64,
    pub name: Option<String>,
    /// Aspect ratio (w/h), known once a thumbnail has been decoded.
    pub ratio: Option<f64>,
    /// One of [`THUMB_UNKNOWN`] / [`THUMB_HAVE`] / [`THUMB_NONE`].
    pub thumb_state: i64,
    /// Which Photos-page tab this entry belongs to, derived from its name and
    /// media type when the timeline was last replaced.
    pub kind: crate::control::PhotoKind,
    /// Whether the photo carries Proton's `Favorite` tag.
    pub favorite: bool,
    /// How many photos this photo's group holds — one shot taken as RAW+JPEG is
    /// two files and one group. `1` for a photo that stands on its own.
    pub group_size: usize,
    /// Whether any member of the group is a camera raw file. The grid badges
    /// that, because the JPEG it shows is not the whole of what is stored.
    pub has_raw: bool,
}

/// One row of a timeline replacement. Everything but the uid and the capture
/// time is "what this refresh learned": a `None` keeps whatever is already
/// stored rather than clearing it, because the timeline DTO carries only the uid
/// and the capture time and resolving the node can fail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineRow {
    pub uid: String,
    pub capture_time: i64,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub favorite: Option<bool>,
    /// The server's duplicate-detection hash for the plaintext.
    pub content_hash: Option<String>,
    /// The main photo this one is *related* to (a live-photo video, a burst
    /// sibling), when the server says it belongs to one.
    pub main_uid: Option<String>,
    /// When this refresh resolved the photo's node, if it did at all. `Some`
    /// makes every other field authoritative — including a `None` that means
    /// "the server no longer reports one" — while `None` means the refresh
    /// skipped this photo and whatever is stored must be kept.
    pub resolved_at: Option<i64>,
}

impl TimelineRow {
    /// A row carrying only what the timeline listing itself gives.
    pub fn new(uid: impl Into<String>, capture_time: i64) -> Self {
        Self {
            uid: uid.into(),
            capture_time,
            ..Self::default()
        }
    }
}

/// What a previous refresh learned about a photo and a new one must not lose.
#[derive(Clone, Debug, Default)]
struct Learned {
    name: Option<String>,
    ratio: Option<f64>,
    thumb_state: i64,
    media_type: Option<String>,
    favorite: bool,
    content_hash: Option<String>,
    main_uid: Option<String>,
    /// When this photo's node was last resolved, kept so a refresh that skips it
    /// does not make it look unresolved again.
    resolved_at: Option<i64>,
}

/// The local calendar day a capture time falls on, as a `YYYY-MM-DD` string.
/// Grouping by day rather than by exact second is deliberate: a camera writes
/// the RAW and the JPEG of one shot a moment apart, and two shots of the same
/// subject on different days are not one photo.
fn capture_day(conn: &rusqlite::Connection, capture_time: i64) -> Option<String> {
    conn.query_row(
        "SELECT strftime('%Y-%m-%d', ?1, 'unixepoch', 'localtime')",
        params![capture_time],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// The file name without its extension, lowercased — `IMG_1234.CR2` and
/// `IMG_1234.JPG` share `img_1234` — with the camera's own part-of-a-shot
/// marker removed, so the two files a raw capture writes share a stem too.
fn name_stem(name: Option<&str>) -> Option<String> {
    let name = name?;
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    };
    let stem = stem.to_ascii_lowercase();
    Some(strip_shot_marker(&stem).to_string())
}

/// Drop a `.RAW-NN.ROLE` marker from a stem.
///
/// Pixel stores one raw capture as `PXL_20260919_000625670.RAW-01.COVER.jpg`
/// and `PXL_20260919_000625670.RAW-02.ORIGINAL.dng`: one shot, two files, two
/// stems that differ in exactly the part naming which file of the shot this is.
/// Cutting the marker off is what lets rule 2 see the pair. The check is narrow
/// on purpose — digits, then a single role word — so an ordinary name that
/// happens to contain `.raw-` keeps its stem.
fn strip_shot_marker(stem: &str) -> &str {
    let Some(at) = stem.rfind(".raw-") else {
        return stem;
    };
    let rest = &stem[at + ".raw-".len()..];
    let Some((index, role)) = rest.split_once('.') else {
        return stem;
    };
    let marked = !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
        && !role.is_empty()
        && role.bytes().all(|b| b.is_ascii_alphabetic());
    if marked { &stem[..at] } else { stem }
}

/// One entry of the grouping pass: what rule 2 needs, plus what choosing a
/// representative needs.
struct Grouped {
    uid: String,
    kind: crate::control::PhotoKind,
    main_uid: Option<String>,
    day: Option<String>,
    stem: Option<String>,
}

/// Which group each photo belongs to, and which member of it the gallery shows.
///
/// Precedence, as agreed:
///
/// 1. The server's own relation — a photo with a `main_photo_uid` joins that
///    photo's group. The server knows about live photos and bursts; we do not
///    have to guess at them.
/// 2. Otherwise, same capture day **and** same file-name stem **and** one of the
///    two is a camera raw while the other is not. That is the RAW+JPEG pair a
///    camera writes, and the differing-kind clause keeps two JPEGs that happen
///    to share a name apart.
/// 3. Otherwise the photo is its own group.
///
/// The representative is the non-raw member when the group has one — a JPEG
/// decodes in milliseconds and is what the person expects to see — and the
/// earliest member in server order otherwise.
fn group_photos(entries: &[Grouped]) -> Vec<String> {
    let mut parent: Vec<usize> = (0..entries.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let union = |parent: &mut [usize], a: usize, b: usize| {
        let (a, b) = (find(parent, a), find(parent, b));
        if a != b {
            parent[b] = a;
        }
    };

    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.uid.as_str(), i))
        .collect();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(main) = entry.main_uid.as_deref()
            && let Some(&j) = index.get(main)
        {
            union(&mut parent, j, i);
        }
    }

    // Rule 2 needs only one pass: everything sharing a day and a stem lands in
    // the same bucket, and the raw / non-raw split is checked pairwise there.
    let mut by_name: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if let (Some(day), Some(stem)) = (entry.day.as_deref(), entry.stem.as_deref()) {
            by_name.entry((day, stem)).or_default().push(i);
        }
    }
    let is_raw = |i: usize| entries[i].kind == crate::control::PhotoKind::Raw;
    for bucket in by_name.values() {
        for (n, &i) in bucket.iter().enumerate() {
            for &j in &bucket[n + 1..] {
                if is_raw(i) != is_raw(j) {
                    union(&mut parent, i, j);
                }
            }
        }
    }

    // Representative per root, then the key every member carries.
    let mut representative: HashMap<usize, usize> = HashMap::new();
    for i in 0..entries.len() {
        let root = find(&mut parent, i);
        match representative.get(&root).copied() {
            Some(current) if !is_raw(current) || is_raw(i) => {}
            _ => {
                representative.insert(root, i);
            }
        }
    }
    (0..entries.len())
        .map(|i| {
            let root = find(&mut parent, i);
            entries[representative[&root]].uid.clone()
        })
        .collect()
}

/// The columns every photo read selects, with the group aggregates joined in.
/// `g.n` is how many photos the group holds and `g.raw` whether any of them is a
/// camera raw file — one query rather than a second lookup per tile.
///
/// `COALESCE(group_key, uid)` is what makes a row written before schema v30 its
/// own group: the column is `NULL` until the next refresh computes it, and a
/// `NULL` would otherwise match nothing and drop the photo off the page.
const PHOTO_SELECT: &str = "SELECT p.uid, p.capture_time, p.name, p.ratio, p.thumb_state, \
     p.kind, p.favorite, g.n, g.raw \
     FROM photos p JOIN (SELECT COALESCE(group_key, uid) AS k, COUNT(*) AS n, \
     MAX(kind = 2) AS raw FROM photos GROUP BY k) g ON g.k = COALESCE(p.group_key, p.uid)";

/// Read one row of [`PHOTO_SELECT`].
fn stored_photo(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPhoto> {
    Ok(StoredPhoto {
        uid: r.get(0)?,
        capture_time: r.get(1)?,
        name: r.get(2)?,
        ratio: r.get(3)?,
        thumb_state: r.get(4)?,
        kind: crate::control::PhotoKind::from_i64(r.get(5)?),
        favorite: r.get::<_, i64>(6)? != 0,
        group_size: r.get::<_, i64>(7)?.max(1) as usize,
        has_raw: r.get::<_, i64>(8)? != 0,
    })
}

impl Db {
    /// Replace the timeline wholesale. A row whose `resolved_at` is `Some` was
    /// read from the server in this pass, so its fields are taken as they are —
    /// a `None` there means the server no longer reports that value. A row whose
    /// `resolved_at` is `None` was skipped or failed to resolve, and keeps
    /// whatever is already stored rather than having it silently cleared.
    pub fn photos_replace(&self, items: &[TimelineRow]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        // `name` and `media_type` are learned-and-kept like the ratio and thumb
        // verdict: the timeline DTO carries only the uid and capture time, so the
        // daemon may not know them when it replaces the timeline — because the
        // refresh skipped an already-resolved photo, or because resolving it
        // failed. Keep any previously learned value so the tab split and the
        // RAW+JPEG grouping survive a refresh instead of collapsing to nothing:
        // both are derived from the name. The photo relation is kept for the same
        // reason — losing it would break a group up until the next resolve.
        let learned: HashMap<String, Learned> = {
            let mut stmt = tx.prepare(
                "SELECT uid, name, ratio, thumb_state, media_type, favorite, content_hash, \
                 main_uid, resolved_at FROM photos",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Learned {
                        name: r.get(1)?,
                        ratio: r.get(2)?,
                        thumb_state: r.get(3)?,
                        media_type: r.get(4)?,
                        favorite: r.get::<_, i64>(5)? != 0,
                        content_hash: r.get(6)?,
                        main_uid: r.get(7)?,
                        resolved_at: r.get(8)?,
                    },
                ))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        // Everything the insert and the grouping need, resolved once per photo.
        let resolved: Vec<(&TimelineRow, Learned, crate::control::PhotoKind)> = items
            .iter()
            .map(|row| {
                let mut carried = learned.get(&row.uid).cloned().unwrap_or(Learned {
                    thumb_state: THUMB_UNKNOWN,
                    ..Learned::default()
                });
                if row.resolved_at.is_some() {
                    // This pass read the node itself, so what it says is the
                    // truth — including what it no longer says. A photo unlinked
                    // from its main on another device loses its `main_uid` here,
                    // which a carry could never express.
                    carried.name = row.name.clone();
                    carried.media_type = row.media_type.clone();
                    carried.favorite = row.favorite.unwrap_or(false);
                    carried.content_hash = row.content_hash.clone();
                    carried.main_uid = row.main_uid.clone();
                } else {
                    carried.name = row.name.clone().or(carried.name);
                    carried.media_type = row.media_type.clone().or(carried.media_type);
                    carried.favorite = row.favorite.unwrap_or(carried.favorite);
                    carried.content_hash = row.content_hash.clone().or(carried.content_hash);
                    carried.main_uid = row.main_uid.clone().or(carried.main_uid);
                }
                carried.resolved_at = row.resolved_at.or(carried.resolved_at);
                // The tab this photo lands in is derived here, once, so a page or
                // count query is a plain indexed `WHERE kind = ?` rather than a
                // reclassification of every row.
                let kind = crate::control::PhotoKind::classify(
                    carried.name.as_deref(),
                    carried.media_type.as_deref(),
                );
                (row, carried, kind)
            })
            .collect();

        let entries: Vec<Grouped> = resolved
            .iter()
            .map(|(row, carried, kind)| Grouped {
                uid: row.uid.clone(),
                kind: *kind,
                main_uid: carried.main_uid.clone(),
                day: capture_day(&tx, row.capture_time),
                stem: name_stem(carried.name.as_deref()),
            })
            .collect();
        let group_keys = group_photos(&entries);

        tx.execute("DELETE FROM photos", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO photos
                   (uid, capture_time, name, ratio, thumb_state, seq, media_type, kind, favorite,
                    content_hash, main_uid, group_key, resolved_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for (seq, ((row, carried, kind), group_key)) in
                resolved.iter().zip(group_keys.iter()).enumerate()
            {
                stmt.execute(params![
                    row.uid,
                    row.capture_time,
                    carried.name,
                    carried.ratio,
                    carried.thumb_state,
                    seq as i64,
                    carried.media_type,
                    kind.as_i64(),
                    carried.favorite as i64,
                    carried.content_hash,
                    carried.main_uid,
                    group_key,
                    carried.resolved_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The uids whose node metadata is already resolved, so a refresh can skip
    /// reading their nodes again. Everything else — a photo never seen before, or
    /// one a remote event marked stale — is resolved. That is what keeps a
    /// 12k-photo library off 63 round-trips of `enumerate_nodes` every refresh.
    pub fn photos_resolved(&self) -> Result<Vec<String>> {
        let conn = self.read();
        let mut stmt = conn.prepare("SELECT uid FROM photos WHERE resolved_at IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut uids = Vec::new();
        for row in rows {
            uids.push(row?);
        }
        Ok(uids)
    }

    /// Mark photos as needing a fresh metadata resolve, and report how many rows
    /// that matched. This is how a remote change is recorded: the event feed says
    /// *which* node changed but never *what* changed about it, so the uid is
    /// written down and the next refresh reads the node.
    pub fn photos_mark_unresolved(&self, uids: &[String]) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut marked = 0;
        {
            let mut stmt = tx.prepare("UPDATE photos SET resolved_at = NULL WHERE uid = ?1")?;
            for uid in uids {
                marked += stmt.execute(params![uid])?;
            }
        }
        tx.commit()?;
        Ok(marked)
    }

    /// The same for the whole timeline: every photo is resolved again on the next
    /// refresh. For a continuity loss, where the daemon cannot know what it
    /// missed, and for an explicit `refresh photos --full`.
    pub fn photos_mark_all_unresolved(&self) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE photos SET resolved_at = NULL", [])?)
    }

    /// Forget photos that have been trashed, so the gallery does not show them
    /// until the next timeline refresh would have caught up.
    ///
    /// Album membership goes with them: a trashed photo is gone from every album
    /// that held it, and leaving the rows behind would keep it on the album
    /// pages and in the album covers.
    pub fn photos_delete(&self, uids: &[String]) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut removed = 0;
        {
            let mut photos = tx.prepare("DELETE FROM photos WHERE uid = ?1")?;
            let mut albums = tx.prepare("DELETE FROM album_photos WHERE uid = ?1")?;
            for uid in uids {
                removed += photos.execute([uid])?;
                albums.execute([uid])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// One page of the persisted timeline, newest first. `kind`, when set,
    /// restricts the page to one tab (Photos / Videos / Raw); `range`, when set,
    /// restricts it to a `[from, to)` capture-time window (epoch seconds) — the
    /// date scrubber's jump. `offset` is relative to whatever the filters leave.
    /// `favorites` restricts the page to favorited photos.
    ///
    /// The page holds one row per *group*: the RAW and the JPEG of one shot are
    /// one tile. The Raw tab is the exception and lists every raw file, because
    /// that tab exists to answer "which raws do I have".
    pub fn photos_page(
        &self,
        offset: usize,
        limit: usize,
        kind: Option<crate::control::PhotoKind>,
        range: Option<(i64, i64)>,
        favorites: bool,
    ) -> Result<Vec<StoredPhoto>> {
        let conn = self.read();
        // Built up so any combination of the optional filters is one indexed
        // query rather than a statement per case.
        let mut sql = String::from(PHOTO_SELECT);
        let mut binds: Vec<i64> = Vec::new();
        let mut conds: Vec<String> = Vec::new();
        if kind != Some(crate::control::PhotoKind::Raw) {
            conds.push("p.uid = COALESCE(p.group_key, p.uid)".to_string());
        }
        if let Some(k) = kind {
            binds.push(k.as_i64());
            conds.push(format!("p.kind = ?{}", binds.len()));
        }
        if let Some((from, to)) = range {
            binds.push(from);
            conds.push(format!("p.capture_time >= ?{}", binds.len()));
            binds.push(to);
            conds.push(format!("p.capture_time < ?{}", binds.len()));
        }
        if favorites {
            conds.push("p.favorite = 1".to_string());
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        binds.push(limit as i64);
        let limit_pos = binds.len();
        binds.push(offset as i64);
        let offset_pos = binds.len();
        sql.push_str(&format!(
            " ORDER BY p.seq LIMIT ?{limit_pos} OFFSET ?{offset_pos}"
        ));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), stored_photo)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Every photo of one photo's group, in server order — the RAW and the JPEG
    /// of a shot, or just the photo itself when it stands alone. The lightbox
    /// switches between these, and deleting a tile trashes all of them.
    pub fn photos_group(&self, uid: &str) -> Result<Vec<StoredPhoto>> {
        let conn = self.read();
        let sql = format!(
            "{PHOTO_SELECT} WHERE COALESCE(p.group_key, p.uid) = \
             (SELECT COALESCE(group_key, uid) FROM photos WHERE uid = ?1) ORDER BY p.seq"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![uid], stored_photo)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// The whole persisted timeline, in stored order — every file, not one row
    /// per group. Only for whole-library passes (the photo re-date); the gallery
    /// pages with [`Db::photos_page`].
    pub fn photos_all(&self) -> Result<Vec<StoredPhoto>> {
        let conn = self.read();
        let sql = format!("{PHOTO_SELECT} ORDER BY p.seq");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], stored_photo)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// The months the timeline spans, newest first, each with how many photos it
    /// holds — the data behind the date scrubber. Buckets are local-time
    /// `(year, month)` so they line up with the day headings the gallery draws
    /// and with the boundaries a front-end computes when it jumps to one. `kind`
    /// scopes the counts to one tab when set.
    pub fn photos_months(
        &self,
        kind: Option<crate::control::PhotoKind>,
    ) -> Result<Vec<(i32, i32, usize)>> {
        let conn = self.read();
        // The scrubber has to agree with the page it scrubs, so it counts what
        // the page shows: groups everywhere except on the Raw tab.
        let (filter, binds): (&str, Vec<i64>) = match kind {
            Some(crate::control::PhotoKind::Raw) => (" WHERE kind = 2", Vec::new()),
            Some(k) => (
                " WHERE uid = COALESCE(group_key, uid) AND kind = ?1",
                vec![k.as_i64()],
            ),
            None => (" WHERE uid = COALESCE(group_key, uid)", Vec::new()),
        };
        let sql = format!(
            "SELECT CAST(strftime('%Y', capture_time, 'unixepoch', 'localtime') AS INTEGER) AS y, \
                    CAST(strftime('%m', capture_time, 'unixepoch', 'localtime') AS INTEGER) AS m, \
                    COUNT(*) \
             FROM photos{filter} GROUP BY y, m ORDER BY y DESC, m DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), |r| {
                Ok((
                    r.get::<_, i32>(0)?,
                    r.get::<_, i32>(1)?,
                    r.get::<_, i64>(2)? as usize,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Per-tab counts for the Photos page subtitle: `(photos, videos, raw)`.
    pub fn photos_counts(&self) -> Result<(usize, usize, usize)> {
        use crate::control::PhotoKind;
        let conn = self.read();
        // Photos and Videos count groups — one shot is one entry, however many
        // files it was stored as. Raw counts files, because the Raw tab lists
        // them individually.
        let mut stmt = conn.prepare(
            "SELECT kind, COUNT(*) FROM photos \
             WHERE uid = COALESCE(group_key, uid) OR kind = 2 GROUP BY kind",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let (mut photos, mut videos, mut raw) = (0usize, 0usize, 0usize);
        for row in rows {
            let (kind, n) = row?;
            match PhotoKind::from_i64(kind) {
                PhotoKind::Photo => photos = n as usize,
                PhotoKind::Video => videos = n as usize,
                PhotoKind::Raw => raw = n as usize,
            }
        }
        Ok((photos, videos, raw))
    }

    /// The stored photos for `uids`, in no particular order. Used by the thumbnail
    /// path, which needs each photo's capture time (the cache validity tag) and
    /// its thumbnail verdict.
    pub fn photos_by_uid(&self, uids: &[String]) -> Result<Vec<StoredPhoto>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read();
        let placeholders = vec!["?"; uids.len()].join(",");
        let sql = format!("{PHOTO_SELECT} WHERE p.uid IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(uids), stored_photo)?;
        let mut photos = Vec::new();
        for row in rows {
            photos.push(row?);
        }
        Ok(photos)
    }

    /// Record a photo's favorite flag locally, after the server accepted the
    /// change. A uid the timeline does not hold is a no-op — an album photo on
    /// someone else's volume is never in our own `photos` table.
    pub fn photos_set_favorite(&self, uid: &str, favorite: bool) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE photos SET favorite = ?2 WHERE uid = ?1",
            params![uid, favorite as i64],
        )?;
        Ok(())
    }

    /// Record what a thumbnail attempt learned: whether the photo now has one
    /// ([`THUMB_HAVE`] / [`THUMB_NONE`]), and its aspect ratio if the pixels were
    /// seen. A `None` ratio leaves any previously learned one alone.
    pub fn photo_set_thumb(&self, uid: &str, state: i64, ratio: Option<f64>) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE photos SET thumb_state = ?2, ratio = COALESCE(?3, ratio) WHERE uid = ?1",
            params![uid, state, ratio],
        )?;
        Ok(())
    }

    /// Number of photos in the persisted timeline.
    pub fn photos_count(&self) -> Result<usize> {
        let conn = self.read();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }
}
