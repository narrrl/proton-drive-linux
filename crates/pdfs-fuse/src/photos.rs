//! The photos timeline and its thumbnails.
//!
//! The timeline is a flat, newest-first projection of the photo share, persisted
//! by [`pdfs_core::db`] so the gallery opens instantly and refreshes behind the
//! user. Thumbnails the server has none for (anything a camera wrote rather than
//! a phone) are generated locally and stored as if the server had served them.

use std::collections::HashMap;
use std::path::PathBuf;

use pdfs_core::control::{PhotoItem, PhotoKind, PhotoThumb};
use pdfs_core::db::{self, StoredPhoto};
use pdfs_core::{CoreError, CoreResult};
use std::sync::atomic::Ordering;

use proton_drive_rs::proton_sdk::ids::NodeUid;
use proton_drive_rs::{NodeKind, ThumbnailType};
use tracing::{info, warn};

use super::{
    Core, PHOTOS_AVAILABLE, PHOTOS_SYNCED_MS, THUMB_GEN_CONCURRENCY, TIMELINE_ENRICH_CHUNK,
    TIMELINE_TTL, ThumbAttempt, node_size, now_ms, parse_uid, ratio_of, scale_thumbnail,
};

impl Core {
    pub(crate) fn photos_timeline(
        &self,
        offset: usize,
        limit: usize,
        kind: Option<PhotoKind>,
        range: Option<(i64, i64)>,
    ) -> CoreResult<Option<Vec<PhotoItem>>> {
        let count = self.db.photos_count().map_err(CoreError::from)?;
        if count == 0 {
            // Nothing to serve, so this one request has to wait for the fetch —
            // unless we already know the account has no photos volume and the
            // answer is a fresh "no".
            let known_empty = self.db.state_i64(PHOTOS_AVAILABLE).ok().flatten() == Some(0);
            if known_empty && !self.listing_stale(PHOTOS_SYNCED_MS, TIMELINE_TTL) {
                return Ok(None);
            }
            if !self.rt.block_on(self.refresh_timeline())? {
                return Ok(None);
            }
        } else if self.listing_stale(PHOTOS_SYNCED_MS, TIMELINE_TTL) {
            self.spawn_timeline_refresh();
        }

        let page = self
            .db
            .photos_page(offset, limit, kind, range)
            .map_err(CoreError::from)?;
        Ok(Some(page.into_iter().map(|p| self.photo_item(p)).collect()))
    }

    /// Project a persisted photo into the wire item the front-end paints: its
    /// learned aspect ratio, its thumbnail verdict, and the on-disk path of its
    /// thumbnail when one is cached (tagged with the capture time, which is the
    /// only revision handle a photo carries).
    pub(crate) fn photo_item(&self, photo: StoredPhoto) -> PhotoItem {
        let thumb_path = parse_uid(&photo.uid).and_then(|uid| {
            self.cache
                .cached_thumbnail_path(&uid, ThumbnailType::Thumbnail.as_i32(), photo.capture_time)
                .map(|p| p.display().to_string())
        });
        PhotoItem {
            uid: photo.uid,
            capture_time: photo.capture_time,
            thumb_path,
            name: photo.name,
            ratio: photo.ratio,
            no_thumb: photo.thumb_state == db::THUMB_NONE,
            kind: photo.kind,
        }
    }

    /// Thumbnails for `uids`, served from the cache, fetched from the server for
    /// whatever is missing, and — for the photos the server has no thumbnail for
    /// at all — *generated locally* from the full file (see
    /// [`Core::generate_thumbs`]). Requested on demand as tiles scroll into view,
    /// so a cold timeline paints immediately and only the photos actually on
    /// screen cost a round-trip.
    ///
    /// A photo absent from the persisted timeline is skipped: its capture time is
    /// the cache's validity tag, and guessing that would poison the cache.
    pub(crate) fn photo_thumbs(&self, uids: &[NodeUid]) -> Vec<PhotoThumb> {
        let ttype = ThumbnailType::Thumbnail.as_i32();
        let keys: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
        let stored = self.db.photos_by_uid(&keys).unwrap_or_default();
        let tags: HashMap<String, i64> = stored
            .iter()
            .map(|p| (p.uid.clone(), p.capture_time))
            .collect();

        // Ask the server only for photos that are missing a cached thumbnail and
        // haven't already been written off as un-thumbnailable.
        let want: Vec<NodeUid> = uids
            .iter()
            .filter(|uid| {
                let key = uid.to_string();
                stored
                    .iter()
                    .find(|p| p.uid == key)
                    .is_some_and(|p| p.thumb_state != db::THUMB_NONE)
                    && tags.get(&key).is_some_and(|&tag| {
                        self.cache.cached_thumbnail_path(uid, ttype, tag).is_none()
                    })
            })
            .cloned()
            .collect();

        if !want.is_empty() {
            let mut missing = want.clone();
            match self.rt.block_on(
                self.photos()
                    .enumerate_thumbnails(&want, ThumbnailType::Thumbnail),
            ) {
                Ok(thumbs) => {
                    for ft in thumbs {
                        let Some(&tag) = tags.get(&ft.file_uid.to_string()) else {
                            continue;
                        };
                        let Ok(bytes) = ft.result else { continue };
                        if bytes.is_empty() {
                            continue;
                        }
                        if self
                            .cache
                            .store_thumbnail(&ft.file_uid, ttype, tag, &bytes)
                            .is_ok()
                        {
                            missing.retain(|uid| uid != &ft.file_uid);
                            self.record_thumb(&ft.file_uid, db::THUMB_HAVE, ratio_of(&bytes));
                        }
                    }
                }
                // A failed batch is not a verdict: leave every uid in `missing` so
                // the local fallback still gives those tiles an image.
                Err(e) => warn!(error = %e, "batch photo thumbnails failed"),
            }

            // Whatever the server had nothing for gets a thumbnail made from its
            // own bytes — this is what fills in camera photos uploaded by clients
            // that never generated one. Off the request path: a full-size photo
            // takes far longer to fetch than the whole rest of the batch, and the
            // thumbnails that *are* ready must not wait behind it.
            if !missing.is_empty() {
                self.spawn_generate_thumbs(missing, &tags);
            }
        }

        let pending = self.thumb_gen.lock();
        uids.iter()
            .map(|uid| PhotoThumb {
                uid: uid.to_string(),
                path: tags.get(&uid.to_string()).and_then(|&tag| {
                    self.cache
                        .cached_thumbnail_path(uid, ttype, tag)
                        .map(|p| p.display().to_string())
                }),
                pending: pending.contains(uid),
            })
            .collect()
    }

    /// Generate the missing thumbnails on the runtime, skipping any photo already
    /// being generated. The uids are marked in-flight before the task starts, so
    /// the reply this call is about to send already reports them as pending.
    pub(crate) fn spawn_generate_thumbs(&self, uids: Vec<NodeUid>, tags: &HashMap<String, i64>) {
        let fresh: Vec<NodeUid> = {
            let mut inflight = self.thumb_gen.lock();
            uids.into_iter()
                .filter(|uid| inflight.insert(uid.clone()))
                .collect()
        };
        if fresh.is_empty() {
            return;
        }

        let core = self.clone();
        let tags = tags.clone();
        // `generate_thumbs` blocks on the runtime itself, so it belongs on the
        // blocking pool rather than on an async worker.
        self.rt.spawn_blocking(move || {
            core.generate_thumbs(&fresh, &tags);
            let mut inflight = core.thumb_gen.lock();
            for uid in &fresh {
                inflight.remove(uid);
            }
        });
    }

    /// Make thumbnails for photos the server has none for: download each full
    /// file once, scale it to [`THUMB_EDGE`], and store the result in the thumbnail
    /// cache exactly as if the server had served it.
    ///
    /// Bounded by [`THUMB_GEN_CONCURRENCY`] — these are full-size originals, and a
    /// screenful of them at once would saturate the link. A photo whose bytes
    /// can't be decoded (a codec `image` doesn't speak) is marked
    /// [`db::THUMB_NONE`] and never attempted again.
    pub(crate) fn generate_thumbs(&self, uids: &[NodeUid], tags: &HashMap<String, i64>) {
        info!(
            count = uids.len(),
            "generating thumbnails the server has none for"
        );
        let results: Vec<(NodeUid, ThumbAttempt)> = self.rt.block_on(async {
            let mut out = Vec::with_capacity(uids.len());
            for chunk in uids.chunks(THUMB_GEN_CONCURRENCY) {
                let mut set = tokio::task::JoinSet::new();
                for uid in chunk {
                    let client = self.client.clone();
                    let uid = uid.clone();
                    set.spawn(async move {
                        let bytes = match client.download_file(&uid).await {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                warn!(%uid, error = %e, "photo download for thumbnail failed");
                                return (uid, ThumbAttempt::Unavailable);
                            }
                        };
                        // Decoding + scaling a 20 MP JPEG is CPU-bound and would
                        // stall the runtime's worker; hand it to the blocking pool.
                        let made = tokio::task::spawn_blocking(move || scale_thumbnail(&bytes))
                            .await
                            .unwrap_or(None);
                        match made {
                            Some(thumb) => (uid, ThumbAttempt::Made(thumb)),
                            None => (uid, ThumbAttempt::Undecodable),
                        }
                    });
                }
                while let Some(joined) = set.join_next().await {
                    if let Ok(result) = joined {
                        out.push(result);
                    }
                }
            }
            out
        });

        let ttype = ThumbnailType::Thumbnail.as_i32();
        for (uid, attempt) in results {
            match attempt {
                ThumbAttempt::Made(thumb) => {
                    let Some(&tag) = tags.get(&uid.to_string()) else {
                        continue;
                    };
                    match self.cache.store_thumbnail(&uid, ttype, tag, &thumb.bytes) {
                        Ok(()) => self.record_thumb(&uid, db::THUMB_HAVE, Some(thumb.ratio)),
                        Err(e) => warn!(%uid, error = %e, "storing generated thumbnail failed"),
                    }
                }
                // The photo's own bytes aren't an image we can read: no thumbnail
                // will ever exist for it, so stop trying.
                ThumbAttempt::Undecodable => self.record_thumb(&uid, db::THUMB_NONE, None),
                // The download failed — a dropped connection, an expired link. That
                // is not a verdict on the photo: leave its state alone so the next
                // scroll past it tries again.
                ThumbAttempt::Unavailable => {}
            }
        }
    }

    /// Persist what a thumbnail attempt learned about a photo.
    pub(crate) fn record_thumb(&self, uid: &NodeUid, state: i64, ratio: Option<f64>) {
        if let Err(e) = self.db.photo_set_thumb(&uid.to_string(), state, ratio) {
            warn!(%uid, error = %e, "recording thumbnail state failed");
        }
    }

    /// Whether the listing stamped under `key` is older than `ttl` (or was never
    /// fetched).
    pub(crate) async fn refresh_timeline(&self) -> CoreResult<bool> {
        let photos = self.photos();
        if photos
            .get_photos_root()
            .await
            .map_err(|e| CoreError::from_api(&e, "photos root"))?
            .is_none()
        {
            let _ = self.db.set_state_i64(PHOTOS_AVAILABLE, 0);
            let _ = self.db.set_state_i64(PHOTOS_SYNCED_MS, now_ms());
            return Ok(false);
        }
        let items = photos
            .enumerate_timeline()
            .await
            .map_err(|e| CoreError::from_api(&e, "timeline"))?;

        // The timeline DTO carries only a uid and capture time, but the Photos
        // page has to split into Photos / Videos / Raw — which needs each photo's
        // name and media type. Resolve those in batches off the request path.
        // Best-effort: a photo whose node we fail to resolve keeps whatever was
        // learned before (or classifies from nothing, i.e. a still photo), so a
        // partial resolve never blanks the timeline.
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
                Err(e) => warn!(error = %e, "resolving photo metadata for a timeline chunk failed"),
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
        self.db.photos_replace(&rows).map_err(CoreError::from)?;
        let _ = self.db.set_state_i64(PHOTOS_AVAILABLE, 1);
        let _ = self.db.set_state_i64(PHOTOS_SYNCED_MS, now_ms());
        Ok(true)
    }

    /// Refresh the timeline off the request path, so a stale page is still served
    /// at DB speed. At most one refresh runs at a time.
    pub(crate) fn spawn_timeline_refresh(&self) {
        if self.timeline_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let core = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = core.refresh_timeline().await {
                warn!(error = %e, "background timeline refresh failed");
            }
            core.timeline_refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// Download a photo's full content into the content cache, returning its
    /// on-disk path (served from cache when a fresh blob already exists).
    pub(crate) fn open_photo(&self, uid: &NodeUid) -> CoreResult<PathBuf> {
        let photos = self.photos();
        let node = self
            .rt
            .block_on(photos.get_node(uid))
            .map_err(|e| CoreError::from_api(&e, "photo node"))?
            .ok_or_else(|| CoreError::not_found("photo not found"))?;
        let (mtime, size) = (node.modification_time, node_size(&node));
        if let Some(p) = self.cache.cached_content_path(uid, mtime, size) {
            return Ok(p);
        }
        let bytes = self
            .download_photo_tracked(&photos, uid, &node.name, size)
            .map_err(|e| CoreError::from_api(&e, "download photo"))?;
        self.cache
            .store(uid, mtime, size, &bytes)
            .map_err(|e| CoreError::internal(format!("cache store: {e}")))?;
        Ok(self.cache.content_path(uid))
    }
}
