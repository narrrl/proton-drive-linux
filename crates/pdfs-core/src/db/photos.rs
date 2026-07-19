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
}

impl Db {
    pub fn photos_replace(
        &self,
        items: &[(String, i64, Option<String>, Option<String>)],
    ) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        // `media_type` is learned-and-kept like the ratio and thumb verdict: the
        // timeline DTO carries only the uid and capture time, so the daemon may
        // not know a photo's media type yet when it replaces the timeline. Keep
        // any previously learned value so the Photos/Videos/Raw split survives a
        // refresh instead of collapsing back to name-extension guesses.
        let learned: HashMap<String, (Option<f64>, i64, Option<String>)> = {
            let mut stmt = tx.prepare("SELECT uid, ratio, thumb_state, media_type FROM photos")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        tx.execute("DELETE FROM photos", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO photos
                   (uid, capture_time, name, ratio, thumb_state, seq, media_type, kind)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (seq, (uid, capture_time, name, media_type)) in items.iter().enumerate() {
                let (ratio, thumb_state, learned_media) =
                    learned
                        .get(uid)
                        .cloned()
                        .unwrap_or((None, THUMB_UNKNOWN, None));
                let media_type = media_type.clone().or(learned_media);
                // The tab this photo lands in is derived here, once, so a page or
                // count query is a plain indexed `WHERE kind = ?` rather than a
                // reclassification of every row.
                let kind =
                    crate::control::PhotoKind::classify(name.as_deref(), media_type.as_deref());
                stmt.execute(params![
                    uid,
                    capture_time,
                    name,
                    ratio,
                    thumb_state,
                    seq as i64,
                    media_type,
                    kind.as_i64(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// One page of the persisted timeline, newest first. `kind`, when set,
    /// restricts the page to one tab (Photos / Videos / Raw); `range`, when set,
    /// restricts it to a `[from, to)` capture-time window (epoch seconds) — the
    /// date scrubber's jump. `offset` is relative to whatever the filters leave.
    pub fn photos_page(
        &self,
        offset: usize,
        limit: usize,
        kind: Option<crate::control::PhotoKind>,
        range: Option<(i64, i64)>,
    ) -> Result<Vec<StoredPhoto>> {
        let conn = self.conn.lock();
        // Built up so any combination of the optional filters is one indexed
        // query rather than a statement per case.
        let mut sql =
            String::from("SELECT uid, capture_time, name, ratio, thumb_state, kind FROM photos");
        let mut binds: Vec<i64> = Vec::new();
        let mut conds: Vec<String> = Vec::new();
        if let Some(k) = kind {
            binds.push(k.as_i64());
            conds.push(format!("kind = ?{}", binds.len()));
        }
        if let Some((from, to)) = range {
            binds.push(from);
            conds.push(format!("capture_time >= ?{}", binds.len()));
            binds.push(to);
            conds.push(format!("capture_time < ?{}", binds.len()));
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
            " ORDER BY seq LIMIT ?{limit_pos} OFFSET ?{offset_pos}"
        ));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), |r| {
                Ok(StoredPhoto {
                    uid: r.get(0)?,
                    capture_time: r.get(1)?,
                    name: r.get(2)?,
                    ratio: r.get(3)?,
                    thumb_state: r.get(4)?,
                    kind: crate::control::PhotoKind::from_i64(r.get(5)?),
                })
            })?
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
        let conn = self.conn.lock();
        let (filter, binds): (&str, Vec<i64>) = match kind {
            Some(k) => (" WHERE kind = ?1", vec![k.as_i64()]),
            None => ("", Vec::new()),
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT kind, COUNT(*) FROM photos GROUP BY kind")?;
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
        let conn = self.conn.lock();
        let placeholders = vec!["?"; uids.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT uid, capture_time, name, ratio, thumb_state, kind FROM photos
             WHERE uid IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(uids), |r| {
            Ok(StoredPhoto {
                uid: r.get(0)?,
                capture_time: r.get(1)?,
                name: r.get(2)?,
                ratio: r.get(3)?,
                thumb_state: r.get(4)?,
                kind: crate::control::PhotoKind::from_i64(r.get(5)?),
            })
        })?;
        let mut photos = Vec::new();
        for row in rows {
            photos.push(row?);
        }
        Ok(photos)
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
        let conn = self.conn.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }
}
