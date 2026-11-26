//! Photo albums: the album listing and each album's contents, persisted like
//! the timeline so the Albums view opens from disk instead of re-enumerating the
//! photos volume on every launch.
//!
//! An album's photos are stored per album rather than as a join against
//! `photos`: an album shared with us lives on the sharer's volume, so its photos
//! never appear in our own timeline and have nowhere else to record what a
//! thumbnail attempt learned about them.

use rusqlite::params;

use std::collections::HashMap;

use super::{Db, StoredPhoto, THUMB_UNKNOWN};
use crate::Result;

/// One album of the persisted listing, ordered newest-activity-first in `seq`.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredAlbum {
    pub uid: String,
    pub name: String,
    pub photo_count: usize,
    /// The photo shown as the album's cover, when the server named one.
    pub cover_uid: Option<String>,
    /// Epoch seconds of the last change to the album's contents.
    pub last_activity: Option<i64>,
    /// True when the album lives on someone else's photos volume.
    pub shared: bool,
}

impl Db {
    /// Replace the album listing wholesale, in the given order.
    pub fn albums_replace(&self, albums: &[StoredAlbum]) -> Result<()> {
        self.albums_replace_impl(albums, None).map(|_| ())
    }

    pub fn albums_replace_at(
        &self,
        albums: &[StoredAlbum],
        generation: i64,
        now: i64,
    ) -> Result<bool> {
        self.albums_replace_impl(albums, Some((generation, now)))
    }

    fn albums_replace_impl(
        &self,
        albums: &[StoredAlbum],
        stamp: Option<(i64, i64)>,
    ) -> Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        if let Some((generation, _)) = stamp
            && super::photos::photos_generation(&tx)? != generation
        {
            return Ok(false);
        }
        // Contents of an album that no longer exists (unshared, deleted) go with
        // it — otherwise they would linger forever, keyed by an album uid no
        // listing mentions again.
        {
            let kept: std::collections::HashSet<&str> =
                albums.iter().map(|a| a.uid.as_str()).collect();
            let stale: Vec<String> = {
                let mut stmt = tx.prepare("SELECT DISTINCT album_uid FROM album_photos")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.filter_map(|r| r.ok())
                    .filter(|uid| !kept.contains(uid.as_str()))
                    .collect()
            };
            let mut stmt = tx.prepare("DELETE FROM album_photos WHERE album_uid = ?1")?;
            for uid in stale {
                stmt.execute([uid])?;
            }
        }
        tx.execute("DELETE FROM albums", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO albums
                   (uid, name, photo_count, cover_uid, last_activity, shared, seq)
                 SELECT ?1,?2,?3,
                    CASE WHEN EXISTS(SELECT 1 FROM photo_tombstones WHERE uid=?4)
                    THEN (SELECT uid FROM album_photos WHERE album_uid=?1 ORDER BY seq LIMIT 1) ELSE ?4 END,
                    ?5,?6,?7
                 WHERE NOT EXISTS(SELECT 1 FROM photo_tombstones WHERE uid=?1)",
            )?;
            for (seq, album) in albums.iter().enumerate() {
                stmt.execute(params![
                    album.uid,
                    album.name,
                    album.photo_count as i64,
                    album.cover_uid,
                    album.last_activity,
                    album.shared as i64,
                    seq as i64,
                ])?;
            }
        }
        if let Some((_, now)) = stamp {
            super::photos::gallery_state(&tx, "albums_synced_ms", now)?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// The persisted album listing, in stored order.
    pub fn albums_list(&self) -> Result<Vec<StoredAlbum>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT uid, name, photo_count, cover_uid, last_activity, shared
             FROM albums ORDER BY seq",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StoredAlbum {
                    uid: r.get(0)?,
                    name: r.get(1)?,
                    photo_count: r.get::<_, i64>(2)?.max(0) as usize,
                    cover_uid: r.get(3)?,
                    last_activity: r.get(4)?,
                    shared: r.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Number of albums in the persisted listing.
    pub fn albums_count(&self) -> Result<usize> {
        let conn = self.read()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }

    /// Replace one album's contents, in the given (newest-capture-first) order.
    /// Learned aspect ratios and thumbnail verdicts survive the replacement, the
    /// same way [`Db::photos_replace`] keeps them for the timeline.
    pub fn album_photos_replace(
        &self,
        album_uid: &str,
        items: &[(String, i64, Option<String>, Option<String>)],
    ) -> Result<()> {
        self.album_photos_replace_impl(album_uid, items, None)
            .map(|_| ())
    }

    pub fn album_photos_replace_at(
        &self,
        album_uid: &str,
        items: &[(String, i64, Option<String>, Option<String>)],
        generation: i64,
        now: i64,
    ) -> Result<bool> {
        self.album_photos_replace_impl(album_uid, items, Some((generation, now)))
    }

    fn album_photos_replace_impl(
        &self,
        album_uid: &str,
        items: &[(String, i64, Option<String>, Option<String>)],
        stamp: Option<(i64, i64)>,
    ) -> Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        if let Some((generation, _)) = stamp
            && super::photos::photos_generation(&tx)? != generation
        {
            return Ok(false);
        }
        type StoredPhotoMetadata = (Option<f64>, i64, Option<String>, Option<String>);
        let learned: HashMap<String, StoredPhotoMetadata> = {
            let mut stmt = tx.prepare(
                "SELECT uid, ratio, thumb_state, media_type, name FROM album_photos
                 WHERE album_uid = ?1",
            )?;
            let rows = stmt.query_map([album_uid], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?),
                ))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        tx.execute("DELETE FROM album_photos WHERE album_uid = ?1", [album_uid])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO album_photos
                   (album_uid, uid, capture_time, name, media_type, kind, ratio, thumb_state, seq, resolved)
                 SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10
                 WHERE NOT EXISTS(SELECT 1 FROM photo_tombstones WHERE uid IN (?1,?2))",
            )?;
            for (seq, (uid, capture_time, name, media_type)) in items.iter().enumerate() {
                let (ratio, thumb_state, learned_media, learned_name) = learned
                    .get(uid)
                    .cloned()
                    .unwrap_or((None, THUMB_UNKNOWN, None, None));
                let resolved = name.is_some();
                let name = name.clone().or(learned_name);
                let media_type = media_type.clone().or(learned_media);
                let kind =
                    crate::control::PhotoKind::classify(name.as_deref(), media_type.as_deref());
                stmt.execute(params![
                    album_uid,
                    uid,
                    capture_time,
                    name,
                    media_type,
                    kind.as_i64(),
                    ratio,
                    thumb_state,
                    seq as i64,
                    resolved,
                ])?;
            }
        }
        if let Some((_, now)) = stamp {
            super::photos::gallery_state(&tx, &format!("album_synced_ms:{album_uid}"), now)?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Reuse current timeline metadata and resolved album-only metadata.
    pub fn album_photo_metadata(
        &self,
        album: &str,
    ) -> Result<HashMap<String, (String, Option<String>)>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT uid,name,media_type FROM album_photos
               WHERE album_uid=?1 AND resolved=1 AND name IS NOT NULL
             UNION ALL SELECT uid,name,media_type FROM photos
               WHERE resolved_at IS NOT NULL AND name IS NOT NULL",
        )?;
        Ok(stmt
            .query_map([album], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// One page of an album's photos, in stored order.
    pub fn album_photos_page(
        &self,
        album_uid: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<StoredPhoto>> {
        self.album_photos_page_visible(album_uid, offset, limit, "[]")
    }

    pub fn album_photos_page_visible(
        &self,
        album_uid: &str,
        offset: usize,
        limit: usize,
        excluded: &str,
    ) -> Result<Vec<StoredPhoto>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT uid, capture_time, name, ratio, thumb_state, kind FROM album_photos
             WHERE album_uid = ?1 AND uid NOT IN (SELECT value FROM json_each(?4))
             ORDER BY seq LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt
            .query_map(
                params![album_uid, limit as i64, offset as i64, excluded],
                |r| {
                    Ok(StoredPhoto {
                        uid: r.get(0)?,
                        capture_time: r.get(1)?,
                        name: r.get(2)?,
                        ratio: r.get(3)?,
                        thumb_state: r.get(4)?,
                        kind: crate::control::PhotoKind::from_i64(r.get(5)?),
                        // Album rows have no favorite state of their own: an album
                        // photo on someone else's volume is not in our timeline, and
                        // favouriting one is not supported yet anyway.
                        favorite: false,
                        // Nor are album rows grouped: an album is a list the user
                        // made, and it shows exactly what they put in it.
                        group_size: 1,
                        has_raw: false,
                        // Album rows carry no location.
                        place_id: None,
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Which albums hold `uid`, by album node uid. Read by the photo re-date,
    /// which has to put the re-uploaded copy back into the albums the original
    /// was in.
    pub fn albums_of_photo(&self, uid: &str) -> Result<Vec<String>> {
        let conn = self.read()?;
        let mut stmt =
            conn.prepare("SELECT DISTINCT album_uid FROM album_photos WHERE uid = ?1")?;
        let rows = stmt
            .query_map([uid], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// How many photos of `album_uid` are persisted.
    pub fn album_photos_count(&self, album_uid: &str) -> Result<usize> {
        let conn = self.read()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM album_photos WHERE album_uid = ?1",
            [album_uid],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    /// The album-held photos among `uids`, one row per uid (a photo in two albums
    /// is returned once). Backs the thumbnail path for photos that are in an
    /// album but not in our own timeline — everything in a shared album, and an
    /// album cover whose photo the current timeline page hasn't reached.
    pub fn album_photos_by_uid(&self, uids: &[String]) -> Result<Vec<StoredPhoto>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read()?;
        let placeholders = vec!["?"; uids.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT uid, capture_time, name, ratio, thumb_state, kind FROM album_photos
             WHERE uid IN ({placeholders}) GROUP BY uid"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(uids), |r| {
                Ok(StoredPhoto {
                    uid: r.get(0)?,
                    capture_time: r.get(1)?,
                    name: r.get(2)?,
                    ratio: r.get(3)?,
                    thumb_state: r.get(4)?,
                    kind: crate::control::PhotoKind::from_i64(r.get(5)?),
                    // Album rows have no favorite state of their own: an album
                    // photo on someone else's volume is not in our timeline, and
                    // favouriting one is not supported yet anyway.
                    favorite: false,
                    // Nor are album rows grouped: an album is a list the user
                    // made, and it shows exactly what they put in it.
                    group_size: 1,
                    has_raw: false,
                    // Album rows carry no location.
                    place_id: None,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Record a thumbnail verdict against every album row for `uid`, so what one
    /// attempt learned is not re-learned per album the photo sits in. The
    /// timeline's own copy is updated separately by [`Db::photo_set_thumb`].
    pub fn album_photo_set_thumb(&self, uid: &str, state: i64, ratio: Option<f64>) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE album_photos SET thumb_state = ?2, ratio = COALESCE(?3, ratio) WHERE uid = ?1",
            params![uid, state, ratio],
        )?;
        Ok(())
    }
}
