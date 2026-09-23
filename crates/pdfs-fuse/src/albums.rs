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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use pdfs_core::control::{AlbumInfo, PhotoItem};
use pdfs_core::db::StoredAlbum;
use pdfs_core::{CoreError, CoreResult};

use proton_drive_rs::NodeKind;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::warn;

use super::{
    ALBUM_SYNCED_PREFIX, ALBUMS_SYNCED_MS, Core, PHOTOS_AVAILABLE, PHOTOS_SYNCED_MS,
    TIMELINE_ENRICH_CHUNK, TIMELINE_TTL, now_ms, parse_uid,
};

impl Core {
    /// The album listing, served from disk and refreshed behind the caller when
    /// stale. `None` means the account has no photos volume — the same signal
    /// [`Core::photos_timeline`] gives.
    pub(crate) fn albums(&self) -> CoreResult<Option<Vec<AlbumInfo>>> {
        let count = self.db.albums_count().map_err(CoreError::from)?;
        if count == 0 {
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
        let photos = self.photos();
        if photos
            .get_photos_root()
            .await
            .map_err(|e| CoreError::from_api(&e, "photos root"))?
            .is_none()
        {
            let _ = self.db.set_state_i64(PHOTOS_AVAILABLE, 0);
            let _ = self.db.set_state_i64(PHOTOS_SYNCED_MS, now_ms());
            let _ = self.db.set_state_i64(ALBUMS_SYNCED_MS, now_ms());
            return Ok(false);
        }

        let mut uids = photos
            .enumerate_album_node_uids()
            .await
            .map_err(|e| CoreError::from_api(&e, "albums"))?;

        // Shared-with-me albums are a separate listing on the sharer's volume.
        // Best-effort: an account whose sharing listing fails still gets its own
        // albums rather than an error page.
        let shared: HashSet<String> = match photos.enumerate_shared_with_me_album_uids().await {
            Ok(shared) => {
                let keys = shared.iter().map(|u| u.to_string()).collect();
                uids.extend(shared);
                keys
            }
            Err(e) => {
                warn!(error = %e, "enumerating shared-with-me albums failed");
                HashSet::new()
            }
        };

        // The listings carry uids only; the name, photo count and cover come from
        // the album nodes themselves. A chunk that fails to resolve drops those
        // albums from this refresh rather than failing the whole listing.
        let mut albums: Vec<StoredAlbum> = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(TIMELINE_ENRICH_CHUNK) {
            match photos.enumerate_nodes(chunk).await {
                Ok(nodes) => {
                    for node in nodes {
                        let key = node.uid.to_string();
                        let props = node.album.as_ref();
                        albums.push(StoredAlbum {
                            shared: shared.contains(&key),
                            uid: key,
                            name: node.name,
                            photo_count: props.map_or(0, |a| a.photo_count.max(0) as usize),
                            cover_uid: props
                                .and_then(|a| a.cover_photo_uid.as_ref())
                                .map(|u| u.to_string()),
                            last_activity: props.and_then(|a| a.last_activity_time),
                        });
                    }
                }
                Err(e) => warn!(error = %e, "resolving an album chunk failed"),
            }
        }

        // Newest activity first, and albums the server gave no activity time for
        // last — an album nobody has touched belongs below the ones in use. Ties
        // fall back to the name so the order is stable between refreshes.
        albums.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        self.db.albums_replace(&albums).map_err(CoreError::from)?;
        let _ = self.db.set_state_i64(PHOTOS_AVAILABLE, 1);
        let _ = self.db.set_state_i64(ALBUMS_SYNCED_MS, now_ms());
        Ok(true)
    }

    /// Refresh the album listing off the request path. At most one runs at a time.
    pub(crate) fn spawn_albums_refresh(&self) {
        if self.albums_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let core = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = core.refresh_albums().await {
                warn!(error = %e, "background album refresh failed");
            }
            core.albums_refreshing.store(false, Ordering::SeqCst);
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
    ) -> CoreResult<Vec<PhotoItem>> {
        let key = album.to_string();
        let stamp = format!("{ALBUM_SYNCED_PREFIX}{key}");
        let count = self.db.album_photos_count(&key).map_err(CoreError::from)?;
        if count == 0 {
            self.rt.block_on(self.refresh_album(album))?;
        } else if self.listing_stale(&stamp, TIMELINE_TTL) {
            let core = self.clone();
            let album = album.clone();
            self.rt.spawn(async move {
                if let Err(e) = core.refresh_album(&album).await {
                    warn!(%album, error = %e, "background album refresh failed");
                }
            });
        }

        let page = self
            .db
            .album_photos_page(&key, offset, limit)
            .map_err(CoreError::from)?;
        Ok(page.into_iter().map(|p| self.photo_item(p)).collect())
    }

    /// Re-enumerate one album's photos and replace its persisted contents.
    pub(crate) async fn refresh_album(&self, album: &NodeUid) -> CoreResult<()> {
        let photos = self.photos();
        let items = photos
            .enumerate_album(album)
            .await
            .map_err(|e| CoreError::from_api(&e, "album"))?;

        // Same enrichment as the timeline: the listing carries a uid and a
        // capture time, and the gallery needs a name and media type to classify
        // each entry. Best-effort per chunk.
        let uids: Vec<NodeUid> = items.iter().map(|it| it.uid.clone()).collect();
        let mut meta: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
        for chunk in uids.chunks(TIMELINE_ENRICH_CHUNK) {
            match photos.enumerate_nodes(chunk).await {
                Ok(nodes) => {
                    for node in nodes {
                        let media_type = match &node.kind {
                            NodeKind::File { media_type, .. } => Some(media_type.clone()),
                            NodeKind::Folder => None,
                        };
                        meta.insert(node.uid.to_string(), (Some(node.name), media_type));
                    }
                }
                Err(e) => warn!(error = %e, "resolving album photo metadata failed"),
            }
        }

        let rows: Vec<(String, i64, Option<String>, Option<String>)> = items
            .iter()
            .map(|it| {
                let key = it.uid.to_string();
                let (name, media_type) = meta.get(&key).cloned().unwrap_or((None, None));
                (key, it.capture_time, name, media_type)
            })
            .collect();
        let key = album.to_string();
        self.db
            .album_photos_replace(&key, &rows)
            .map_err(CoreError::from)?;
        let _ = self
            .db
            .set_state_i64(&format!("{ALBUM_SYNCED_PREFIX}{key}"), now_ms());
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
        if let Err(error) = self.db.album_photos_replace(&key, &[]) {
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
