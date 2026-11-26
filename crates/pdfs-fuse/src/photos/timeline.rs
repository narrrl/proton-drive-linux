//! The photos timeline and what is built on it: favorites, duplicates and
//! similar shots, places, on this day, groups, and trashing photos.

use super::*;

impl Core {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn photos_timeline(
        &self,
        offset: usize,
        limit: usize,
        kind: Option<PhotoKind>,
        range: Option<(i64, i64)>,
        favorites: bool,
        not_in_album: bool,
        excluded: &str,
    ) -> CoreResult<Option<Vec<PhotoItem>>> {
        let count = self.db.photos_count().map_err(CoreError::from)?;
        let listed = self
            .db
            .state_i64(PHOTOS_SYNCED_MS)
            .map_err(CoreError::from)?
            .is_some();
        if !listed {
            if count == 0 {
                if !self.refresh_timeline_joined()? {
                    return Ok(None);
                }
            } else {
                self.spawn_timeline_refresh();
            }
        } else if self
            .db
            .state_i64(PHOTOS_AVAILABLE)
            .map_err(CoreError::from)?
            == Some(0)
        {
            return Ok(None);
        }
        // The filter is only as good as the album contents on disk, and those
        // are fetched lazily, per album opened. Fill in the missing ones before
        // the first page; later pages reuse what that fetch stored.
        if not_in_album && offset == 0 {
            self.ensure_album_contents()?;
        }

        let page = self
            .db
            .photos_page_visible(
                offset,
                limit,
                kind,
                range,
                favorites,
                not_in_album,
                excluded,
            )
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
            favorite: photo.favorite,
            group_size: photo.group_size as u32,
            has_raw: photo.has_raw,
            place_id: photo.place_id,
        }
    }

    /// Add or remove Proton's `Favorite` tag on a photo, then record the change
    /// locally so the gallery reflects it without waiting for a timeline refresh.
    ///
    /// The SDK reports one outcome per photo rather than failing the call, so the
    /// single outcome is unwrapped back into this request's result.
    pub(crate) fn set_photo_favorite(&self, uid: &NodeUid, favorite: bool) -> CoreResult<()> {
        let photos = self.photos();
        let update = PhotoTagsUpdate {
            node_uid: uid.clone(),
            tags_to_add: if favorite {
                vec![PhotoTag::Favorite]
            } else {
                Vec::new()
            },
            tags_to_remove: if favorite {
                Vec::new()
            } else {
                vec![PhotoTag::Favorite]
            },
        };
        let outcomes = self
            .rt
            .block_on(photos.update_photos(std::slice::from_ref(&update)))
            .map_err(|e| CoreError::from_api(&e, "update photo tags"))?;
        pdfs_core::batch::into_unit(outcomes)
            .map_err(|e| CoreError::from_api(&e, "update photo tags"))?;
        // The photo event feed replays this write back at us as a bare
        // `NodeUpdated`, which is indistinguishable from the same photo being
        // favorited on a phone. Claim the echo, or the gallery pays for a
        // resolve of a node whose one changed field is already stored below.
        self.note_self_change(uid);
        self.db
            .photos_set_favorite(&uid.to_string(), favorite)
            .map_err(CoreError::from)?;
        Ok(())
    }

    /// The timeline's duplicate sets, as wire items (see
    /// [`db::Db::photo_duplicates`]).
    pub(crate) fn photo_duplicates(&self) -> CoreResult<Vec<Vec<PhotoItem>>> {
        let sets = self.db.photo_duplicates().map_err(CoreError::from)?;
        Ok(sets
            .into_iter()
            .map(|set| set.into_iter().map(|p| self.photo_item(p)).collect())
            .collect())
    }

    /// The sets of shots that look alike, as wire items (see
    /// [`db::Db::photo_similar`]), how far hashing has got while it runs, and
    /// how many shots couldn't be compared once it's done. Starts a hashing
    /// pass for the shots not hashed yet unless one is running.
    pub(crate) fn photo_similar(
        &self,
    ) -> CoreResult<(Vec<Vec<PhotoItem>>, Option<MappingProgress>, usize)> {
        self.spawn_similar_hashing();
        let sets = self.db.photo_similar().map_err(CoreError::from)?;
        let (done, total) = self.db.similar_progress().map_err(CoreError::from)?;
        let hashing = self
            .photo_jobs
            .similar_hashing
            .load(Ordering::SeqCst)
            .then_some(MappingProgress { done, total });
        let uncompared = if hashing.is_some() { 0 } else { total - done };
        let sets = sets
            .into_iter()
            .map(|set| set.into_iter().map(|p| self.photo_item(p)).collect())
            .collect();
        Ok((sets, hashing, uncompared))
    }

    pub(crate) fn spawn_similar_hashing(&self) {
        if self.photo_jobs.similar_hashing.swap(true, Ordering::SeqCst) {
            return;
        }
        let core = self.clone();
        let spawned = self.services.spawn_job("pdfs-similar", move || {
            if let Err(e) = core.hash_similar() {
                warn!(error = %e, "similar-photo hashing failed");
            }
            core.photo_jobs
                .similar_hashing
                .store(false, Ordering::SeqCst);
        });
        if let Err(e) = spawned {
            warn!(error = %e, "could not start similar-photo hashing");
            self.photo_jobs
                .similar_hashing
                .store(false, Ordering::SeqCst);
        }
    }

    /// Hash the look of every shot not hashed yet, a chunk at a time. Only
    /// thumbnails the server has are used: a photo without one is skipped
    /// rather than downloaded whole, and tried again on the next pass.
    pub(crate) fn hash_similar(&self) -> CoreResult<()> {
        let mut tried: HashSet<String> = HashSet::new();
        loop {
            if self.shutdown.is_stopping() {
                return Ok(());
            }
            // Shots tried this pass without a thumbnail are still unhashed and
            // come back first, so ask past them.
            let chunk: Vec<NodeUid> = self
                .db
                .photos_unhashed(tried.len() + SIMILAR_CHUNK)
                .map_err(CoreError::from)?
                .into_iter()
                .filter(|photo| tried.insert(photo.uid.clone()))
                .take(SIMILAR_CHUNK)
                .filter_map(|photo| parse_uid(&photo.uid))
                .collect();
            if chunk.is_empty() {
                return Ok(());
            }
            let paths: Vec<(String, PathBuf)> = self
                .photo_thumbs_with(&chunk, false)
                .into_iter()
                .filter_map(|thumb| Some((thumb.uid, PathBuf::from(thumb.path?))))
                .collect();
            for (uid, hash) in similar_hashes(paths) {
                self.db
                    .photo_set_similar_hash(&uid, hash)
                    .map_err(CoreError::from)?;
            }
        }
    }

    /// The towns the photos were taken in, as wire items. A town the bundled
    /// table no longer knows (the stored id came from an older table) is left
    /// out rather than shown without a name.
    pub(crate) fn photo_places(&self, excluded: &str) -> CoreResult<Vec<PlaceInfo>> {
        let places = self
            .db
            .photo_places_visible(excluded)
            .map_err(CoreError::from)?;
        Ok(places
            .into_iter()
            .filter_map(|place| {
                let city = pdfs_core::places::city(place.id)?;
                Some(PlaceInfo {
                    id: place.id,
                    name: city.name.to_string(),
                    country: city.country.to_string(),
                    latitude: city.latitude,
                    longitude: city.longitude,
                    photo_count: place.count,
                    cover: self.photo_item(place.cover),
                })
            })
            .collect())
    }

    /// A page of the photos taken in town `id`, as wire items.
    pub(crate) fn place_photos(
        &self,
        id: u32,
        offset: usize,
        limit: usize,
        excluded: &str,
    ) -> CoreResult<Vec<PhotoItem>> {
        let photos = self
            .db
            .photos_at_place_visible(id, offset, limit, excluded)
            .map_err(CoreError::from)?;
        Ok(photos.into_iter().map(|p| self.photo_item(p)).collect())
    }

    /// Every photo taken in town `id` with its own location, as wire items.
    pub(crate) fn place_locations(&self, id: u32) -> CoreResult<Vec<PhotoLocation>> {
        let photos = self
            .db
            .photo_locations_at_place(id)
            .map_err(CoreError::from)?;
        Ok(photos
            .into_iter()
            .map(|(photo, (latitude, longitude))| PhotoLocation {
                photo: self.photo_item(photo),
                latitude,
                longitude,
            })
            .collect())
    }

    /// Photos taken on today's date in earlier years, as wire items. Served
    /// from the stored timeline only: the strip is a nicety, not worth a wait
    /// on the network, and the timeline's own load keeps that store fresh.
    pub(crate) fn photos_on_this_day(
        &self,
        limit: usize,
        excluded: &str,
    ) -> CoreResult<Vec<PhotoItem>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let photos = self
            .db
            .photos_on_this_day_visible(now, limit, excluded)
            .map_err(CoreError::from)?;
        Ok(photos.into_iter().map(|p| self.photo_item(p)).collect())
    }

    /// Every file of one photo's group, in server order, as wire items. A photo
    /// the timeline does not hold comes back empty, which the lightbox reads as
    /// "nothing to switch between".
    pub(crate) fn photo_group(&self, uid: &str) -> CoreResult<Vec<PhotoItem>> {
        let members = self.db.photos_group(uid).map_err(CoreError::from)?;
        Ok(members
            .into_iter()
            .map(|photo| self.photo_item(photo))
            .collect())
    }

    /// Every uid asked for, plus the other files of any group they belong to,
    /// each once. A uid the timeline does not hold (an album photo on someone
    /// else's volume) is passed through unchanged.
    pub(crate) fn expand_photo_groups(&self, uids: &[NodeUid]) -> Vec<NodeUid> {
        let mut out: Vec<NodeUid> = Vec::with_capacity(uids.len());
        let mut seen: HashSet<String> = HashSet::new();
        for uid in uids {
            let members = self.db.photos_group(&uid.to_string()).unwrap_or_default();
            let mut group: Vec<NodeUid> = members
                .iter()
                .filter_map(|photo| parse_uid(&photo.uid))
                .collect();
            if group.is_empty() {
                group.push(uid.clone());
            }
            for member in group {
                if seen.insert(member.to_string()) {
                    out.push(member);
                }
            }
        }
        out
    }

    /// Mutation admission for a photo, which [`Core::require_uid_writable`]
    /// cannot answer on its own.
    ///
    /// Photos live on the photo volume, and that volume is not part of the FUSE
    /// tree: a photo has no `nodes` row, so every authority comes back
    /// [`WriteAuthority::Unknown`] and the plain check refuses the whole
    /// library. Membership of our own timeline is the authority that does exist
    /// here — the timeline is enumerated from our photo share, so a uid in it is
    /// ours to trash. Anything else (an album photo on someone else's volume)
    /// still fails closed.
    pub(crate) fn require_photo_writable(&self, uid: &NodeUid) -> Result<(), fuser::Errno> {
        match self.uid_write_authority(uid) {
            WriteAuthority::Writable => Ok(()),
            WriteAuthority::Denied => Err(fuser::Errno::EACCES),
            WriteAuthority::Unknown => {
                let key = uid.to_string();
                let known = self
                    .db
                    .photos_by_uid(std::slice::from_ref(&key))
                    .map(|rows| !rows.is_empty())
                    .unwrap_or(false);
                if known {
                    Ok(())
                } else {
                    Err(fuser::Errno::EACCES)
                }
            }
        }
    }

    /// Trash photos by uid, and forget them locally.
    ///
    /// The gallery is not part of the FUSE mount, so this is the photos-side
    /// counterpart of [`Core::delete`]: same destination (Proton trash, so the
    /// Trash page can restore them), addressed by uid because that is all a tile
    /// has.
    ///
    /// The server answers per node, and so does this: a batch where one photo
    /// fails still removes the ones that went, and the caller is told which is
    /// which. Only what actually left the server is forgotten locally.
    ///
    /// A photo that is one file of a group takes its whole group with it. The
    /// gallery shows a RAW+JPEG shot as one tile, so deleting that tile has to
    /// delete the shot — leaving the RAW behind would put the photo back on the
    /// Raw tab and nowhere else. `files_only` turns that off, for removing
    /// one copy of a shot that is stored twice.
    pub(crate) fn trash_photos(
        &self,
        uids: &[NodeUid],
        files_only: bool,
    ) -> CoreResult<TrashOutcome> {
        if uids.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let expanded;
        let uids = if files_only {
            uids
        } else {
            expanded = self.expand_photo_groups(uids);
            &expanded
        };
        for uid in uids {
            self.require_photo_writable(uid)
                .map_err(|errno| self.errno_error(errno, "trash access"))?;
        }
        let outcomes = self
            .rt
            .block_on(self.drive.trash_nodes(uids))
            .map_err(|e| CoreError::from_api(&e, "trash"))?;
        let (trashed, failed) = pdfs_core::batch::split(outcomes);

        let trashed: Vec<String> = trashed.iter().map(|uid| uid.to_string()).collect();
        for uid in &trashed {
            if let Some(parsed) = parse_uid(uid) {
                self.cache.evict(&parsed);
                self.evict_reader(&parsed);
            }
        }
        // The timeline is served from the database, so removing the rows is what
        // makes the photos disappear now rather than at the next refresh.
        if let Err(error) = self.db.photos_delete(&trashed) {
            warn!(%error, "trashed photos could not be removed from the timeline");
        }
        self.invalidate_photos();
        self.invalidate_albums();
        self.invalidate_trash();

        let failed = failed
            .into_iter()
            .map(|(uid, error)| (uid.to_string(), error.to_string()))
            .collect();
        Ok((trashed, failed))
    }

    /// Commit the lightweight listing before resolving pending nodes. The
    /// feed generation fences both commits against events received in flight.
    pub(crate) async fn refresh_timeline(&self) -> CoreResult<bool> {
        let started = Instant::now();
        let generation = self
            .db
            .photo_projection_generation()
            .map_err(CoreError::from)?;
        let photos = self.photos();
        if self
            .db
            .state_i64(PHOTOS_SYNCED_MS)
            .map_err(CoreError::from)?
            .is_none()
        {
            let available = link::bounded(photos.get_photos_root())
                .await
                .map_err(|e| CoreError::from_api(&e, "photos root"))?
                .is_some();
            let items = if available {
                link::bounded_by(crate::BATCH_CALL_TIMEOUT, photos.enumerate_timeline())
                    .await
                    .map_err(|e| CoreError::from_api(&e, "timeline"))?
                    .into_iter()
                    .map(|item| db::TimelineRow::new(item.uid.to_string(), item.capture_time))
                    .collect()
            } else {
                Vec::new()
            };
            if !self
                .db
                .photos_replace_at(&items, generation, available, now_ms())
                .map_err(CoreError::from)?
            {
                return Err(CoreError::remote(
                    "Photos changed during the refresh. Try again shortly.",
                ));
            }
            if !available {
                return Ok(false);
            }
        } else if self
            .db
            .state_i64(PHOTOS_AVAILABLE)
            .map_err(CoreError::from)?
            == Some(0)
        {
            return Ok(false);
        }

        let pending = self.db.photos_pending().map_err(CoreError::from)?;
        let total = self.db.photos_count().map_err(CoreError::from)?;
        let skipped = total.saturating_sub(pending.len());
        let mut resolved = 0;
        let progress = ProgressGuard(&self.photo_jobs.timeline_progress);
        for (index, chunk) in pending.chunks(TIMELINE_ENRICH_CHUNK).enumerate() {
            if self.shutdown.is_stopping() {
                break;
            }
            *progress.0.lock() = Some((skipped + index * TIMELINE_ENRICH_CHUNK, total));
            let uids: Vec<_> = chunk.iter().filter_map(|uid| parse_uid(uid)).collect();
            let nodes = link::bounded(photos.enumerate_nodes(&uids))
                .await
                .map_err(|e| CoreError::from_api(&e, "read photo metadata"))?;
            let rows: Vec<_> = nodes
                .into_iter()
                .filter_map(|node| {
                    let NodeKind::File { media_type, .. } = node.kind else {
                        return None;
                    };
                    if node.trashed {
                        return None;
                    }
                    let photo = node.photo.as_ref();
                    Some(db::TimelineRow {
                        name: Some(node.name),
                        media_type: Some(media_type),
                        favorite: Some(photo.is_some_and(|p| p.tags.contains(&PhotoTag::Favorite))),
                        content_hash: photo.and_then(|p| p.content_hash.clone()),
                        location: photo
                            .and_then(|p| p.location)
                            .map(|l| (l.latitude, l.longitude)),
                        main_uid: photo
                            .and_then(|p| p.main_photo_uid.as_ref())
                            .map(ToString::to_string),
                        resolved_at: Some(now_ms()),
                        ..db::TimelineRow::new(node.uid.to_string(), 0)
                    })
                })
                .collect();
            if !self
                .db
                .photos_resolve_at(generation, &rows)
                .map_err(CoreError::from)?
            {
                return Err(CoreError::remote(
                    "Photos changed during the refresh. Try again shortly.",
                ));
            }
            resolved += rows.len();
        }
        info!(
            photos = total,
            resolved,
            wanted = pending.len(),
            skipped,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "photos timeline refreshed"
        );
        Ok(true)
    }

    /// Refresh the timeline off the request path, so a stale page is still served
    /// at DB speed. At most one refresh runs at a time.
    /// Refresh the timeline and wait for it, sharing a refresh already under
    /// way instead of starting a second one beside it.
    ///
    /// The first request after an empty cache has nothing to serve until the
    /// fetch lands, so it waits. Two such requests (the gallery and its retry,
    /// or a background refresh started by an import) used to each fetch the
    /// whole library at once, sharing the session's rate limit and taking
    /// minutes. Returns whether the account has a photos library.
    pub(crate) fn refresh_timeline_joined(&self) -> CoreResult<bool> {
        if self
            .photo_jobs
            .timeline_refreshing
            .swap(true, Ordering::SeqCst)
        {
            while self.photo_jobs.timeline_refreshing.load(Ordering::SeqCst)
                && !self.shutdown.is_stopping()
            {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            // A refresh that landed stamped the timeline fresh. One that failed
            // did not, and the caller gets this refresh's own error instead of
            // an empty timeline.
            if self
                .db
                .state_i64(PHOTOS_SYNCED_MS)
                .map_err(CoreError::from)?
                .is_some()
                || self
                    .photo_jobs
                    .timeline_refreshing
                    .swap(true, Ordering::SeqCst)
            {
                return Ok(self.db.state_i64(PHOTOS_AVAILABLE).ok().flatten() != Some(0));
            }
        }
        let refreshed = self.rt.block_on(self.refresh_timeline());
        self.photo_jobs
            .timeline_refreshing
            .store(false, Ordering::SeqCst);
        if refreshed.is_ok() {
            self.events.publish(&[Topic::Photos]);
        }
        refreshed
    }

    pub(crate) fn spawn_timeline_refresh(&self) {
        if self
            .photo_jobs
            .timeline_refreshing
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let core = self.clone();
        self.services
            .spawn_task_job(&self.rt, "pdfs-timeline-refresh", async move {
                let refreshed = core.refresh_timeline().await;
                core.photo_jobs
                    .timeline_refreshing
                    .store(false, Ordering::SeqCst);
                // Only a refresh that landed: a front-end re-reading after a failed
                // one would find the timeline still stale and start another.
                match refreshed {
                    Ok(_) => core.events.publish(&[Topic::Photos]),
                    Err(e) => warn!(error = %e, "background timeline refresh failed"),
                }
            });
    }

    /// How far the running timeline refresh has got reading photo metadata, or
    /// `None` when no refresh is reading any.
    pub(crate) fn timeline_progress(&self) -> Option<MappingProgress> {
        self.photo_jobs
            .timeline_progress
            .lock()
            .map(|(done, total)| MappingProgress { done, total })
    }

    /// Whether a timeline refresh is running. A front-end that asked for one
    /// watches this to know when the new timeline is worth re-reading.
    pub(crate) fn timeline_refreshing(&self) -> bool {
        self.photo_jobs.timeline_refreshing.load(Ordering::SeqCst)
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
