//! Photo albums: the album listing, and one album's contents.
//!
//! Both are persisted projections of what the photos volume reports, refreshed
//! on the same stale-while-serving rule as the timeline
//! ([`crate::photos`]): a listing already on disk is served immediately and
//! refreshed behind the caller, and only an empty one waits on the network.
//!
//! An album is a folder node carrying album properties, so it has a name like
//! any other node; the properties supply the photo count and the cover. Albums
//! other people share with us come from a second listing and live on *their*
//! photos volume — which is why an album's photos are persisted per album rather
//! than looked up in our own timeline, where a shared album's photos never
//! appear.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use pdfs_core::control::{AlbumInfo, PhotoItem, Topic};
use pdfs_core::{CoreError, CoreResult};
use pdfs_store::db::StoredAlbum;

use proton_drive_rs::NodeKind;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::warn;

use super::{
    ALBUM_SYNCED_PREFIX, ALBUMS_SYNCED_MS, Core, PHOTOS_AVAILABLE, TIMELINE_ENRICH_CHUNK,
    TIMELINE_TTL, link, now_ms, parse_uid,
};

impl Core {
    /// The album listing, served from disk and refreshed behind the caller when
    /// stale. `None` means the account has no photos volume — the same signal
    /// [`Core::photos_timeline`] gives.
    pub(crate) fn albums(&self) -> CoreResult<Option<Vec<AlbumInfo>>> {
        let count = self.db.albums_count().map_err(CoreError::from)?;
        if count == 0
            && self
                .db
                .state_i64(ALBUMS_SYNCED_MS)
                .map_err(CoreError::from)?
                .is_none()
        {
            // Nothing on disk, so this one request waits for the fetch — unless
            // the account is already known to have no photos volume.
            let known_empty = self.db.state_i64(PHOTOS_AVAILABLE).ok().flatten() == Some(0);
            if known_empty && !self.listing_stale(ALBUMS_SYNCED_MS, TIMELINE_TTL) {
                return Ok(None);
            }
            if !self.rt.block_on(self.refresh_albums())? {
                return Ok(None);
            }
        } else if self.listing_stale(ALBUMS_SYNCED_MS, TIMELINE_TTL) {
            self.spawn_albums_refresh();
        }

        let albums = self.db.albums_list().map_err(CoreError::from)?;
        Ok(Some(albums.into_iter().map(album_info).collect()))
    }

    /// Re-enumerate the albums — ours and the ones shared with us — and replace
    /// the persisted listing. Returns false when the account has no photos
    /// volume.
    pub(crate) async fn refresh_albums(&self) -> CoreResult<bool> {
        let generation = self
            .db
            .photo_projection_generation()
            .map_err(CoreError::from)?;
        let photos = self.photos();
        if link::bounded(photos.get_photos_root())
            .await
            .map_err(|e| CoreError::from_api(&e, "photos root"))?
            .is_none()
        {
            self.db
                .albums_replace_at(&[], generation, now_ms())
                .map_err(CoreError::from)?;
            return Ok(false);
        }
        let mut uids = link::bounded(photos.enumerate_album_node_uids())
            .await
            .map_err(|e| CoreError::from_api(&e, "albums"))?;
        let shared = link::bounded(photos.enumerate_shared_with_me_album_uids())
            .await
            .map_err(|e| CoreError::from_api(&e, "shared albums"))?;
        let shared_keys: HashSet<_> = shared.iter().map(ToString::to_string).collect();
        uids.extend(shared);
        let mut unique = HashSet::new();
        uids.retain(|uid| unique.insert(uid.clone()));

        let mut albums = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(TIMELINE_ENRICH_CHUNK) {
            let nodes = link::bounded(photos.enumerate_nodes(chunk))
                .await
                .map_err(|e| CoreError::from_api(&e, "album metadata"))?;
            if nodes.len() != chunk.len() {
                return Err(CoreError::remote(
                    "Some album details are still unavailable.",
                ));
            }
            for node in nodes.into_iter().filter(|node| !node.trashed) {
                let uid = node.uid.to_string();
                let props = node.album.as_ref();
                albums.push(StoredAlbum {
                    shared: shared_keys.contains(&uid),
                    uid,
                    name: node.name,
                    photo_count: props.map_or(0, |a| a.photo_count.max(0) as usize),
                    cover_uid: props
                        .and_then(|a| a.cover_photo_uid.as_ref())
                        .map(ToString::to_string),
                    last_activity: props.and_then(|a| a.last_activity_time),
                });
            }
        }
        albums.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        if !self
            .db
            .albums_replace_at(&albums, generation, now_ms())
            .map_err(CoreError::from)?
        {
            return Err(CoreError::remote(
                "Photos changed during the album refresh. Try again shortly.",
            ));
        }
        Ok(true)
    }

    /// Refresh the album listing off the request path. At most one runs at a time.
    pub(crate) fn spawn_albums_refresh(&self) {
        if self
            .photo_jobs
            .albums_refreshing
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let core = self.clone();
        self.services
            .spawn_task_job(&self.rt, "pdfs-albums-refresh", async move {
                let refreshed = core.refresh_albums().await;
                core.photo_jobs
                    .albums_refreshing
                    .store(false, Ordering::SeqCst);
                match refreshed {
                    Ok(_) => core.events.publish(&[Topic::Photos]),
                    Err(e) => warn!(error = %e, "background album refresh failed"),
                }
            });
    }

    /// A page of one album's photos, newest capture first. The album's contents
    /// are fetched on first open and refreshed behind the caller afterwards, so
    /// paging never waits on the network twice.
    pub(crate) fn album_photos(
        &self,
        album: &NodeUid,
        offset: usize,
        limit: usize,
        excluded: &str,
    ) -> CoreResult<Vec<PhotoItem>> {
        let key = album.to_string();
        if self.db.photo_removed(&key).map_err(CoreError::from)? {
            return Ok(Vec::new());
        }
        let stamp = format!("{ALBUM_SYNCED_PREFIX}{key}");
        let count = self.db.album_photos_count(&key).map_err(CoreError::from)?;
        if count == 0
            && self
                .db
                .state_i64(&stamp)
                .map_err(CoreError::from)?
                .is_none()
        {
            self.rt.block_on(self.refresh_album(album))?;
        } else if self.listing_stale(&stamp, TIMELINE_TTL) {
            let core = self.clone();
            let album = album.clone();
            self.services
                .spawn_task_job(&self.rt, "pdfs-album-refresh", async move {
                    if let Err(e) = core.refresh_album(&album).await {
                        warn!(%album, error = %e, "background album refresh failed");
                    }
                });
        }

        let page = self
            .db
            .album_photos_page_visible(&key, offset, limit, excluded)
            .map_err(CoreError::from)?;
        Ok(page.into_iter().map(|p| self.photo_item(p)).collect())
    }

    /// Make sure every album of ours has its contents on disk, for a filter
    /// that asks which photos no album holds. Contents are otherwise fetched
    /// the first time an album is opened, so an album never opened would read
    /// as empty and its photos as unfiled.
    ///
    /// Shared-with-me albums are skipped: their photos live on the sharer's
    /// volume and are never in our timeline. The freshness stamps are no guide
    /// here — any album change clears all of them — so only an album whose
    /// server count disagrees with an empty local copy is waited for.
    pub(crate) fn ensure_album_contents(&self) -> CoreResult<()> {
        if self.db.albums_count().map_err(CoreError::from)? == 0
            && !self.rt.block_on(self.refresh_albums())?
        {
            return Ok(());
        }
        let albums = self.db.albums_list().map_err(CoreError::from)?;
        for album in albums.iter().filter(|a| !a.shared && a.photo_count > 0) {
            let stored = self
                .db
                .album_photos_count(&album.uid)
                .map_err(CoreError::from)?;
            if stored > 0 {
                continue;
            }
            let Some(uid) = parse_uid(&album.uid) else {
                continue;
            };
            if let Err(error) = self.rt.block_on(self.refresh_album(&uid)) {
                warn!(album = %album.uid, %error, "fetching an album's contents failed");
            }
        }
        Ok(())
    }

    /// Re-enumerate one album's photos and replace its persisted contents.
    pub(crate) async fn refresh_album(&self, album: &NodeUid) -> CoreResult<()> {
        let generation = self
            .db
            .photo_projection_generation()
            .map_err(CoreError::from)?;
        let photos = self.photos();
        let items = link::bounded(photos.enumerate_album(album))
            .await
            .map_err(|e| CoreError::from_api(&e, "album"))?;
        let key = album.to_string();
        let mut meta = self
            .db
            .album_photo_metadata(&key)
            .map_err(CoreError::from)?;
        let uids: Vec<NodeUid> = items
            .iter()
            .filter(|item| !meta.contains_key(&item.uid.to_string()))
            .map(|item| item.uid.clone())
            .collect();
        for chunk in uids.chunks(TIMELINE_ENRICH_CHUNK) {
            let nodes = link::bounded(photos.enumerate_nodes(chunk))
                .await
                .map_err(|e| CoreError::from_api(&e, "album photo metadata"))?;
            for node in nodes.into_iter().filter(|node| !node.trashed) {
                let media_type = match node.kind {
                    NodeKind::File { media_type, .. } => Some(media_type),
                    NodeKind::Folder => None,
                };
                meta.insert(node.uid.to_string(), (node.name, media_type));
            }
        }
        let rows: Vec<_> = items
            .into_iter()
            .map(|item| {
                let uid = item.uid.to_string();
                let (name, media_type) = meta
                    .get(&uid)
                    .map(|(name, mime)| (Some(name.clone()), mime.clone()))
                    .unwrap_or_default();
                (uid, item.capture_time, name, media_type)
            })
            .collect();
        if !self
            .db
            .album_photos_replace_at(&key, &rows, generation, now_ms())
            .map_err(CoreError::from)?
        {
            return Err(CoreError::remote(
                "Photos changed during the album refresh. Try again shortly.",
            ));
        }
        Ok(())
    }

    /// Create an album and return its uid. The listing is refreshed before
    /// answering, so the next [`Core::albums`] already shows it.
    pub(crate) fn create_album(&self, name: &str) -> CoreResult<String> {
        let uid = self
            .rt
            .block_on(self.photos().create_album(name))
            .map_err(|e| CoreError::from_api(&e, "create album"))?;
        self.after_album_change(None);
        Ok(uid.to_string())
    }

    pub(crate) fn rename_album(&self, album: &NodeUid, name: &str) -> CoreResult<()> {
        self.rt
            .block_on(self.photos().rename_album(album, name))
            .map_err(|e| CoreError::from_api(&e, "rename album"))?;
        self.after_album_change(None);
        Ok(())
    }

    /// Delete an album. Its photos stay in the timeline unless `delete_photos`,
    /// which only matters for photos that exist in the album alone.
    pub(crate) fn delete_album(&self, album: &NodeUid, delete_photos: bool) -> CoreResult<()> {
        self.rt
            .block_on(self.photos().delete_album(album, delete_photos))
            .map_err(|e| CoreError::from_api(&e, "delete album"))?;
        let key = album.to_string();
        if let Err(error) = self.db.photos_delete(&[key]) {
            warn!(%album, %error, "a deleted album's photos could not be dropped");
        }
        self.after_album_change(None);
        Ok(())
    }

    /// Add photos to an album, per photo: `(added, failed)`.
    pub(crate) fn add_to_album(
        &self,
        album: &NodeUid,
        photos: &[NodeUid],
    ) -> CoreResult<AlbumOutcome> {
        let outcomes = self
            .rt
            .block_on(self.photos().add_photos_to_album(album, photos))
            .map_err(|e| CoreError::from_api(&e, "add to album"))?;
        self.after_album_change(Some(album));
        Ok(album_outcome(outcomes))
    }

    /// Take photos out of an album, per photo: `(removed, failed)`. The photos
    /// stay in the timeline.
    pub(crate) fn remove_from_album(
        &self,
        album: &NodeUid,
        photos: &[NodeUid],
    ) -> CoreResult<AlbumOutcome> {
        let outcomes = self
            .rt
            .block_on(self.photos().remove_photos_from_album(album, photos))
            .map_err(|e| CoreError::from_api(&e, "remove from album"))?;
        self.after_album_change(Some(album));
        Ok(album_outcome(outcomes))
    }

    /// Bring the persisted listing, and `album`'s contents when given, up to
    /// date after a change made here, so the reply is followed by a listing
    /// that shows it. A failed refresh only leaves them stale: the change
    /// itself went through.
    fn after_album_change(&self, album: Option<&NodeUid>) {
        self.invalidate_albums();
        if let Err(error) = self.rt.block_on(self.refresh_albums()) {
            warn!(%error, "album listing refresh after a change failed");
        }
        if let Some(album) = album
            && let Err(error) = self.rt.block_on(self.refresh_album(album))
        {
            warn!(%album, %error, "album refresh after a change failed");
        }
    }

    /// Drop the album listing's and every album's freshness stamp, so the next
    /// request re-enumerates. Paired with [`Core::invalidate_photos`] — a change
    /// to the timeline can just as easily be a change to an album.
    pub(crate) fn invalidate_albums(&self) {
        let _ = self.db.clear_state(ALBUMS_SYNCED_MS);
        let _ = self.db.clear_state_prefix(ALBUM_SYNCED_PREFIX);
    }
}

/// Photos an album change applied to, and the ones it failed for with why.
pub(crate) type AlbumOutcome = (Vec<String>, Vec<(String, String)>);

fn album_outcome(outcomes: Vec<pdfs_core::batch::Outcome>) -> AlbumOutcome {
    let (done, failed) = pdfs_core::batch::split(outcomes);
    (
        done.iter().map(|uid| uid.to_string()).collect(),
        failed
            .into_iter()
            .map(|(uid, error)| (uid.to_string(), error.to_string()))
            .collect(),
    )
}

/// Project a persisted album into the wire item a front-end paints.
fn album_info(album: StoredAlbum) -> AlbumInfo {
    AlbumInfo {
        uid: album.uid,
        name: album.name,
        photo_count: album.photo_count,
        // A cover uid the daemon cannot parse is no cover: the front-end would
        // only ask for a thumbnail that can never be resolved.
        cover_uid: album.cover_uid.filter(|uid| parse_uid(uid).is_some()),
        last_activity: album.last_activity,
        shared: album.shared,
    }
}
