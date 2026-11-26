//! Thumbnails: the ones the server lacks, generated here and uploaded; the
//! file-manager thumbnails for ordinary files and folder covers; and the
//! recursive build started from the Files toolbar.

use super::*;

impl Core {
    /// [`upload_thumbnails`] for a file about to upload, staging any RAW copy
    /// under this cache. Blocking.
    pub(crate) fn upload_thumbnails(&self, path: &Path, name: &str) -> Vec<Thumbnail> {
        upload_thumbnails(path, name, self.cache.raw_thumbnail_staging_dir())
    }

    /// [`Core::upload_thumbnails`] on the blocking pool, for async callers.
    pub(crate) async fn upload_thumbnails_async(
        &self,
        path: PathBuf,
        name: String,
    ) -> Vec<Thumbnail> {
        let staging_dir = self.cache.raw_thumbnail_staging_dir().to_path_buf();
        tokio::task::spawn_blocking(move || upload_thumbnails(&path, &name, &staging_dir))
            .await
            .unwrap_or_default()
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
    ///
    /// A camera raw is served by the JPEG of the same shot when there is one:
    /// the two files show one picture, and the JPEG is already decodable in
    /// milliseconds where the raw costs a full download and a raw decode. The
    /// tile still answers under the uid that was asked for.
    pub(crate) fn photo_thumbs(&self, requested: &[NodeUid]) -> Vec<PhotoThumb> {
        self.photo_thumbs_with(requested, true)
    }

    /// [`Core::photo_thumbs`], with local generation only when `generate` is
    /// set. Without it, a photo the server has no thumbnail for is left
    /// without one rather than downloaded whole.
    pub(crate) fn photo_thumbs_with(
        &self,
        requested: &[NodeUid],
        generate: bool,
    ) -> Vec<PhotoThumb> {
        let ttype = ThumbnailType::Thumbnail.as_i32();
        // Only a raw can be stood in for, and that is one batched read; the
        // per-shot group lookup is then paid for raws alone, not for a page of
        // ordinary photos.
        let asked: Vec<String> = requested.iter().map(|u| u.to_string()).collect();
        let raws: HashSet<String> = self
            .db
            .photos_by_uid(&asked)
            .unwrap_or_default()
            .into_iter()
            .filter(|photo| photo.kind == PhotoKind::Raw)
            .map(|photo| photo.uid)
            .collect();
        let sources: Vec<NodeUid> = requested
            .iter()
            .map(|uid| {
                if raws.contains(&uid.to_string()) {
                    self.thumb_source(uid)
                } else {
                    uid.clone()
                }
            })
            .collect();
        let uids = &sources;
        let keys: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
        let mut stored = self.db.photos_by_uid(&keys).unwrap_or_default();
        // Album covers and the contents of an album shared with us are not in our
        // own timeline, so their capture time — the cache's validity tag — only
        // exists on the album rows. Fill in whatever the timeline didn't cover.
        {
            let have: HashSet<String> = stored.iter().map(|p| p.uid.clone()).collect();
            let missing: Vec<String> = keys
                .iter()
                .filter(|k| !have.contains(*k))
                .cloned()
                .collect();
            stored.extend(self.db.album_photos_by_uid(&missing).unwrap_or_default());
        }
        let stored = stored;
        let tags: HashMap<String, i64> = stored
            .iter()
            .map(|p| (p.uid.clone(), p.capture_time))
            .collect();
        let names: HashMap<String, String> = stored
            .iter()
            .filter_map(|photo| photo.name.clone().map(|name| (photo.uid.clone(), name)))
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
            if generate && !missing.is_empty() {
                self.spawn_generate_thumbs(missing, &tags, &names, ThumbJob::Photos);
            }
        }

        let pending = self.photo_jobs.thumb_gen.lock();
        requested
            .iter()
            .zip(uids)
            .map(|(asked, source)| PhotoThumb {
                uid: asked.to_string(),
                path: tags.get(&source.to_string()).and_then(|&tag| {
                    self.cache
                        .cached_thumbnail_path(source, ttype, tag)
                        .map(|p| p.display().to_string())
                }),
                pending: pending.contains(source),
            })
            .collect()
    }

    /// Which file's thumbnail stands for `uid` on a tile.
    ///
    /// Itself, for all but a camera raw that was written next to a JPEG of the
    /// same shot. For that one, the JPEG: same picture, already compressed,
    /// often carrying a server thumbnail the raw does not have — so the Raw tab
    /// costs no raw download and no raw decode. A raw that stands alone keeps
    /// its own local generation.
    pub(crate) fn thumb_source(&self, uid: &NodeUid) -> NodeUid {
        let members = match self.db.photos_group(&uid.to_string()) {
            Ok(members) if members.len() > 1 => members,
            _ => return uid.clone(),
        };
        let key = uid.to_string();
        if !members
            .iter()
            .any(|photo| photo.uid == key && photo.kind == PhotoKind::Raw)
        {
            return uid.clone();
        }
        members
            .iter()
            .find(|photo| photo.kind != PhotoKind::Raw)
            .and_then(|photo| parse_uid(&photo.uid))
            .unwrap_or_else(|| uid.clone())
    }

    /// Thumbnails for image files shown by the regular file browser, Shared and
    /// Trash pages. Unlike [`Core::photo_thumbs`], these nodes need not be in the
    /// Photos timeline: the request supplies each file's modification time as
    /// the cache validity tag.
    pub(crate) fn file_thumbs(
        &self,
        items: &[FileThumbRequest],
        generation: u64,
    ) -> Option<Vec<PhotoThumb>> {
        let current = generation != 0
            && self.photo_jobs.file_thumb_generation.load(Ordering::SeqCst) == generation;
        if !current {
            return None;
        }
        let ttype = ThumbnailType::Thumbnail.as_i32();
        let parsed: Vec<(String, NodeUid, i64)> = items
            .iter()
            .filter_map(|item| {
                parse_uid(&item.uid).map(|uid| (item.uid.clone(), uid, item.modified))
            })
            .collect();
        let tags: HashMap<String, i64> = parsed
            .iter()
            .map(|(raw, _, modified)| (raw.clone(), *modified))
            .collect();
        let names: HashMap<String, String> = items
            .iter()
            .map(|item| (item.uid.clone(), item.name.clone()))
            .collect();

        // Drive makes no thumbnails itself; a file has one only when its
        // uploader attached it, as this client and Proton's apps do. The
        // generation step asks for that first, and only makes one from the
        // file's content without it.
        let mut wanted = Vec::new();
        let mut seen = HashSet::new();
        for (raw, uid, modified) in &parsed {
            if self
                .cache
                .cached_thumbnail_path(uid, ttype, *modified)
                .is_some()
                || self
                    .photo_jobs
                    .thumbnail_misses
                    .lock()
                    .local_contains(&(uid.clone(), ttype), *modified)
                || !seen.insert(raw.clone())
            {
                continue;
            }
            wanted.push(uid.clone());
        }

        if !wanted.is_empty() {
            self.spawn_generate_thumbs(wanted, &tags, &names, ThumbJob::Files(generation));
        }

        let pending = self.photo_jobs.thumb_gen.lock();
        Some(
            items
                .iter()
                .map(|item| {
                    let uid = parse_uid(&item.uid);
                    PhotoThumb {
                        uid: item.uid.clone(),
                        path: uid.as_ref().and_then(|uid| {
                            self.cache
                                .cached_thumbnail_path(uid, ttype, item.modified)
                                .map(|path| path.display().to_string())
                        }),
                        pending: uid.as_ref().is_some_and(|uid| pending.contains(uid)),
                    }
                })
                .collect(),
        )
    }

    /// Reserve the next ordinary-file listing generation. This daemon-owned
    /// monotonic value replaces GUI wall-clock seeds, which can move backwards
    /// across NTP corrections, suspend, or dual boot.
    pub(crate) fn reserve_file_thumb_generation(&self) -> u64 {
        let next = |current| if current == u64::MAX { 1 } else { current + 1 };
        let previous = self
            .photo_jobs
            .file_thumb_generation
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(next(current))
            })
            .expect("generation update always succeeds");
        let generation = next(previous);
        self.photo_jobs.file_thumb_cancel.notify_waiters();
        generation
    }

    /// Advance the ordinary-file thumbnail generation. Every queued/download in
    /// an older listing observes the new value and exits; Photos and explicit
    /// recursive builds do not use this generation and continue normally.
    pub(crate) fn cancel_file_thumbs(&self, generation: u64) {
        let previous = self
            .photo_jobs
            .file_thumb_generation
            .fetch_max(generation, Ordering::SeqCst);
        if generation > previous {
            self.photo_jobs.file_thumb_cancel.notify_waiters();
        }
    }

    /// Generate the missing thumbnails on the runtime, skipping any photo already
    /// being generated. The uids are marked in-flight before the task starts, so
    /// the reply this call is about to send already reports them as pending.
    pub(super) fn spawn_generate_thumbs(
        &self,
        uids: Vec<NodeUid>,
        tags: &HashMap<String, i64>,
        names: &HashMap<String, String>,
        job: ThumbJob,
    ) {
        let fresh: Vec<NodeUid> = {
            let mut inflight = self.photo_jobs.thumb_gen.lock();
            uids.into_iter()
                .filter(|uid| inflight.insert(uid.clone()))
                .collect()
        };
        if fresh.is_empty() {
            return;
        }

        let core = self.clone();
        let tags = tags.clone();
        let names = names.clone();
        // `generate_thumbs` blocks on the runtime itself, so it belongs on the
        // blocking pool rather than on an async worker.
        self.rt.spawn_blocking(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                core.generate_thumbs(&fresh, &tags, &names, job)
            }));
            let mut inflight = core.photo_jobs.thumb_gen.lock();
            for uid in &fresh {
                inflight.remove(uid);
            }
            drop(inflight);
            if result.is_err() {
                warn!("thumbnail generation worker panicked");
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
    pub(super) fn thumb_job_current(&self, job: ThumbJob) -> bool {
        match job {
            ThumbJob::Files(generation) => {
                self.photo_jobs.file_thumb_generation.load(Ordering::SeqCst) == generation
            }
            ThumbJob::Photos => true,
            ThumbJob::Build => !self
                .photo_jobs
                .thumbnail_build_cancelled
                .load(Ordering::SeqCst),
        }
    }

    pub(super) fn generate_thumbs(
        &self,
        uids: &[NodeUid],
        tags: &HashMap<String, i64>,
        names: &HashMap<String, String>,
        job: ThumbJob,
    ) -> ThumbBatchSummary {
        info!(count = uids.len(), "generating local thumbnails");
        let results: Vec<(NodeUid, ThumbAttempt)> = self.rt.block_on(async {
            let mut out = Vec::with_capacity(uids.len());
            for chunk in uids.chunks(THUMB_GEN_CONCURRENCY) {
                let mut set = tokio::task::JoinSet::new();
                for uid in chunk {
                    let core = self.clone();
                    let client = self.client.clone();
                    let uid = uid.clone();
                    let name = names.get(&uid.to_string()).cloned().unwrap_or_default();
                    set.spawn(async move {
                        if !core.thumb_job_current(job) {
                            return (uid, ThumbAttempt::Cancelled);
                        }
                        let video = is_video_name(&name);
                        let audio = is_audio_file_name(&name);
                        // Without ffmpeg a video can still have its uploader's
                        // thumbnail, but Photos has asked the server already.
                        let can_sample = !video || ffmpeg_available();
                        if !can_sample && matches!(job, ThumbJob::Photos) {
                            return (uid, ThumbAttempt::Unavailable);
                        }

                        // One shared permit pool bounds all thumbnail batches,
                        // rather than multiplying the limit by every control
                        // request the GUI happens to have in flight.
                        let acquire = core.photo_jobs.thumb_gen_budget.clone().acquire_owned();
                        tokio::pin!(acquire);
                        let _permit = match job {
                            ThumbJob::Files(_) => loop {
                                tokio::select! {
                                    permit = &mut acquire => break permit.ok(),
                                    _ = core.photo_jobs.file_thumb_cancel.notified() => {
                                        if !core.thumb_job_current(job) {
                                            return (uid, ThumbAttempt::Cancelled);
                                        }
                                    }
                                    _ = tokio::time::sleep(THUMB_CANCEL_POLL) => {
                                        if !core.thumb_job_current(job) {
                                            return (uid, ThumbAttempt::Cancelled);
                                        }
                                    }
                                }
                            },
                            ThumbJob::Build => loop {
                                tokio::select! {
                                    permit = &mut acquire => break permit.ok(),
                                    _ = core.photo_jobs.thumbnail_build_cancel.notified() => {
                                        if !core.thumb_job_current(job) {
                                            return (uid, ThumbAttempt::Cancelled);
                                        }
                                    }
                                    _ = tokio::time::sleep(THUMB_CANCEL_POLL) => {
                                        if !core.thumb_job_current(job) {
                                            return (uid, ThumbAttempt::Cancelled);
                                        }
                                    }
                                }
                            },
                            ThumbJob::Photos => acquire.await.ok(),
                        };
                        let Some(_permit) = _permit else {
                            return (uid, ThumbAttempt::Unavailable);
                        };
                        if !core.thumb_job_current(job) {
                            return (uid, ThumbAttempt::Cancelled);
                        }

                        let download_uid = uid.clone();
                        let drive = core.drive.clone();
                        let audio_name = name.clone();
                        let download = async move {
                            // Photos asked the server in one batch already. A
                            // file view's or a build's file may carry a
                            // thumbnail its uploader made, which costs one
                            // small download instead of its content.
                            if !matches!(job, ThumbJob::Photos)
                                && let Some(bytes) = drive
                                    .download_thumbnail(&download_uid, ThumbnailType::Thumbnail)
                                    .await?
                                    .filter(|bytes| !bytes.is_empty())
                            {
                                return Ok(Fetched::Ready(bytes));
                            }
                            if !can_sample {
                                return Ok(Fetched::Nothing);
                            }
                            if video {
                                let reader = drive.open_revision(&download_uid).await?;
                                sample_video(reader.as_ref()).await.map(Fetched::Video)
                            } else if audio {
                                let reader = drive.open_revision(&download_uid).await?;
                                sample_audio(reader.as_ref(), &audio_name)
                                    .await
                                    .map(Fetched::Audio)
                            } else {
                                client
                                    .download_file(&download_uid)
                                    .await
                                    .map(Fetched::Whole)
                            }
                        };
                        tokio::pin!(download);
                        let downloaded = match job {
                            ThumbJob::Files(_) => loop {
                                tokio::select! {
                                    result = &mut download => break Some(result),
                                    _ = core.photo_jobs.file_thumb_cancel.notified() => {
                                        if !core.thumb_job_current(job) {
                                            break None;
                                        }
                                    }
                                    _ = tokio::time::sleep(THUMB_CANCEL_POLL) => {
                                        if !core.thumb_job_current(job) {
                                            break None;
                                        }
                                    }
                                }
                            },
                            ThumbJob::Build => loop {
                                tokio::select! {
                                    result = &mut download => break Some(result),
                                    _ = core.photo_jobs.thumbnail_build_cancel.notified() => {
                                        if !core.thumb_job_current(job) {
                                            break None;
                                        }
                                    }
                                    _ = tokio::time::sleep(THUMB_CANCEL_POLL) => {
                                        if !core.thumb_job_current(job) {
                                            break None;
                                        }
                                    }
                                }
                            },
                            ThumbJob::Photos => Some(download.await),
                        };
                        let Some(downloaded) = downloaded else {
                            return (uid, ThumbAttempt::Cancelled);
                        };
                        let fetched = match downloaded {
                            Ok(fetched) => fetched,
                            Err(e) => {
                                warn!(%uid, error = %e, "download for a thumbnail failed");
                                return (uid, ThumbAttempt::Unavailable);
                            }
                        };
                        if !core.thumb_job_current(job) {
                            return (uid, ThumbAttempt::Cancelled);
                        }
                        // Decoding + scaling a 20 MP JPEG is CPU-bound and would
                        // stall the runtime's worker; hand it to the blocking pool.
                        let staging_dir = core.cache.raw_thumbnail_staging_dir().to_path_buf();
                        let staging = staging_dir.clone();
                        let made = tokio::task::spawn_blocking(move || match fetched {
                            Fetched::Audio(sample) => {
                                audio_cover_thumbnail(&sample, &name, &staging_dir)
                            }
                            Fetched::Video(sample) => video_sample_thumbnail(
                                &sample,
                                &name,
                                &staging_dir,
                                OsStr::new("ffmpeg"),
                                FFMPEG_TIMEOUT,
                            ),
                            Fetched::Whole(bytes) => scale_thumbnail(&bytes, &name, &staging_dir),
                            Fetched::Nothing => ScaleAttempt::Unavailable,
                            Fetched::Ready(bytes) => match ratio_of(&bytes) {
                                Some(ratio) => ScaleAttempt::Made(GeneratedThumb { bytes, ratio }),
                                None => ScaleAttempt::Undecodable,
                            },
                        })
                        .await
                        .unwrap_or(ScaleAttempt::Unavailable);
                        // A song without a picture of its own shows its
                        // folder's cover.
                        let made = match made {
                            ScaleAttempt::Undecodable if audio && core.thumb_job_current(job) => {
                                core.folder_cover_thumbnail(&uid, staging).await
                            }
                            made => made,
                        };
                        if !core.thumb_job_current(job) {
                            return (uid, ThumbAttempt::Cancelled);
                        }
                        match made {
                            ScaleAttempt::Made(thumb) => (uid, ThumbAttempt::Made(thumb)),
                            ScaleAttempt::Undecodable => (uid, ThumbAttempt::Undecodable),
                            ScaleAttempt::Unavailable => (uid, ThumbAttempt::Unavailable),
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
        let mut summary = ThumbBatchSummary::default();
        for (uid, attempt) in results {
            match attempt {
                ThumbAttempt::Made(thumb) => {
                    let Some(&tag) = tags.get(&uid.to_string()) else {
                        summary.unavailable += 1;
                        continue;
                    };
                    match self.cache.store_thumbnail(&uid, ttype, tag, &thumb.bytes) {
                        Ok(()) => {
                            summary.made += 1;
                            self.photo_jobs
                                .thumbnail_misses
                                .lock()
                                .forget_local(&(uid.clone(), ttype));
                            self.record_thumb(&uid, db::THUMB_HAVE, Some(thumb.ratio));
                        }
                        Err(e) => {
                            summary.unavailable += 1;
                            warn!(%uid, error = %e, "storing generated thumbnail failed");
                        }
                    }
                }
                // The photo's own bytes aren't an image we can read: no thumbnail
                // will ever exist for it, so stop trying.
                ThumbAttempt::Undecodable => {
                    summary.undecodable += 1;
                    if let Some(&tag) = tags.get(&uid.to_string()) {
                        self.photo_jobs
                            .thumbnail_misses
                            .lock()
                            .remember_local((uid.clone(), ttype), tag);
                    }
                    self.record_thumb(&uid, db::THUMB_NONE, None);
                }
                // The download failed — a dropped connection, an expired link. That
                // is not a verdict on the photo: leave its state alone so the next
                // scroll past it tries again.
                ThumbAttempt::Unavailable => summary.unavailable += 1,
                ThumbAttempt::Cancelled => summary.cancelled += 1,
            }
        }
        // A folder on screen whose tiles were given up on, as a recursive
        // build's start does to them, fills in without being opened again.
        if summary.made > 0 {
            self.events.publish(&[Topic::Thumbnails]);
        }
        summary
    }

    /// The cover image in the folder of the song `uid`: the best-named one of
    /// [`FOLDER_COVER_STEMS`] among its siblings the database knows.
    pub(crate) fn folder_cover(&self, uid: &NodeUid) -> Option<proton_drive_rs::Node> {
        let node = self.db.node_by_uid(&uid.to_string()).ok()??;
        let parent = node.parent_uid?;
        let siblings = match self.db.children_if_listed(&parent) {
            Ok(Some(children)) => children,
            _ => self.db.known_children(&parent).ok()?,
        };
        siblings
            .into_iter()
            .filter(|sibling| !sibling.is_folder() && !sibling.trashed)
            .filter_map(|sibling| cover_rank(&sibling.name).map(|rank| (rank, sibling)))
            .min_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.name.cmp(&y.name)))
            .map(|(_, cover)| cover)
    }

    /// A thumbnail of the song `uid` made from its folder's cover image: the
    /// cover's own cached thumbnail when it has one, else the thumbnail its
    /// uploader attached, else the image itself, up to
    /// [`FOLDER_COVER_MAX_BYTES`].
    ///
    /// The result is stored under the song, so a cover added to the folder or
    /// replaced later shows only once the song changes or the daemon restarts
    /// and forgets the songs it found no cover for.
    pub(super) async fn folder_cover_thumbnail(
        &self,
        uid: &NodeUid,
        staging_dir: PathBuf,
    ) -> ScaleAttempt {
        let Some(cover) = self.folder_cover(uid) else {
            return ScaleAttempt::Undecodable;
        };
        let ttype = ThumbnailType::Thumbnail;
        let cached = self
            .cache
            .cached_thumbnail_path(&cover.uid, ttype.as_i32(), cover.modification_time)
            .and_then(|path| std::fs::read(path).ok());
        let ready = match cached {
            Some(bytes) => Some(bytes),
            None => match self.drive.download_thumbnail(&cover.uid, ttype).await {
                Ok(bytes) => bytes.filter(|bytes| !bytes.is_empty()),
                Err(e) => {
                    warn!(cover = %cover.uid, error = %e, "folder cover thumbnail download failed");
                    return ScaleAttempt::Unavailable;
                }
            },
        };
        if let Some(bytes) = ready {
            return match ratio_of(&bytes) {
                Some(ratio) => ScaleAttempt::Made(GeneratedThumb { bytes, ratio }),
                None => ScaleAttempt::Undecodable,
            };
        }
        let read = async {
            let reader = self.drive.open_revision(&cover.uid).await?;
            if reader.size() > FOLDER_COVER_MAX_BYTES {
                return Ok(None);
            }
            reader.read_at(0, reader.size()).await.map(Some)
        };
        let bytes = match read.await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return ScaleAttempt::Undecodable,
            Err(e) => {
                warn!(cover = %cover.uid, error = %e, "folder cover download failed");
                return ScaleAttempt::Unavailable;
            }
        };
        let name = cover.name;
        tokio::task::spawn_blocking(move || scale_thumbnail(&bytes, &name, &staging_dir))
            .await
            .unwrap_or(ScaleAttempt::Unavailable)
    }

    pub(crate) fn thumbnail_build_status(&self) -> ThumbnailBuildStatus {
        self.photo_jobs.thumbnail_build.lock().clone()
    }

    /// Start one recursive local-thumbnail build. A request for the same root
    /// may attach to it; a different root is rejected explicitly instead of
    /// silently pretending the existing job belongs to the caller.
    pub(crate) fn start_thumbnail_build(&self, root: PathBuf) -> CoreResult<ThumbnailBuildStatus> {
        let path = root.to_string_lossy().into_owned();
        let status = {
            let mut status = self.photo_jobs.thumbnail_build.lock();
            if !thumbnail_build_may_start(&status, &path)? {
                return Ok(status.clone());
            }
            self.photo_jobs
                .thumbnail_build_cancelled
                .store(false, Ordering::SeqCst);
            *status = ThumbnailBuildStatus {
                running: true,
                scanning: true,
                path: path.clone(),
                folders_scanned: 0,
                images_found: 0,
                completed: 0,
                failed: 0,
                message: None,
            };
            status.clone()
        };

        let core = self.clone();
        self.rt.spawn_blocking(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                core.run_thumbnail_build(root)
            }))
            .is_err()
            {
                let mut status = core.photo_jobs.thumbnail_build.lock();
                status.running = false;
                status.scanning = false;
                status.message = Some("Thumbnail generation stopped unexpectedly".to_string());
                core.photo_jobs
                    .thumbnail_build_cancelled
                    .store(false, Ordering::SeqCst);
                warn!(path = %status.path, "recursive thumbnail build panicked");
            }
        });
        Ok(status)
    }

    pub(crate) fn cancel_thumbnail_build(&self) -> ThumbnailBuildStatus {
        let (status, cancelled) = {
            let status = self.photo_jobs.thumbnail_build.lock();
            let cancelled = status.running;
            if cancelled {
                self.photo_jobs
                    .thumbnail_build_cancelled
                    .store(true, Ordering::SeqCst);
            }
            (status.clone(), cancelled)
        };
        if cancelled {
            self.photo_jobs.thumbnail_build_cancel.notify_waiters();
        }
        status
    }

    pub(crate) fn finish_thumbnail_build(&self) {
        let mut status = self.photo_jobs.thumbnail_build.lock();
        let cancelled = self
            .photo_jobs
            .thumbnail_build_cancelled
            .swap(false, Ordering::SeqCst);
        status.running = false;
        status.scanning = false;
        if cancelled {
            status.message = Some("Thumbnail build cancelled".to_string());
        }
        info!(
            path = %status.path,
            images = status.images_found,
            failed = status.failed,
            cancelled,
            "recursive thumbnail build finished"
        );
    }

    pub(crate) fn run_thumbnail_build(&self, root: PathBuf) {
        let mut folders = vec![root];
        let mut images: Vec<(NodeUid, i64, String)> = Vec::new();
        let mut seen_folders = HashSet::new();
        let mut seen_images = HashSet::new();

        while let Some(folder) = folders.pop() {
            if self
                .photo_jobs
                .thumbnail_build_cancelled
                .load(Ordering::SeqCst)
            {
                self.finish_thumbnail_build();
                return;
            }
            let listing = self.list_dir(&folder);
            {
                let mut status = self.photo_jobs.thumbnail_build.lock();
                status.folders_scanned += 1;
            }
            let entries = match listing {
                Ok(entries) => entries,
                Err(error) => {
                    let mut status = self.photo_jobs.thumbnail_build.lock();
                    if status.message.is_none() {
                        status.message = Some(format!(
                            "Some folders could not be read ({}: {error})",
                            folder.display()
                        ));
                    }
                    continue;
                }
            };

            let mut found_here = 0_u64;
            let mut invalid_here = 0_u64;
            for entry in entries {
                if entry.is_dir {
                    if seen_folders.insert(entry.uid) {
                        folders.push(folder.join(entry.name));
                    }
                } else if is_thumbnail_media_name(&entry.name, Some(entry.size)) {
                    match parse_uid(&entry.uid) {
                        Some(uid) if seen_images.insert(uid.clone()) => {
                            found_here += 1;
                            images.push((uid, entry.modified, entry.name));
                        }
                        Some(_) => {}
                        None => {
                            found_here += 1;
                            invalid_here += 1;
                        }
                    }
                }
            }
            let mut status = self.photo_jobs.thumbnail_build.lock();
            status.images_found += found_here;
            status.completed += invalid_here;
            status.failed += invalid_here;
        }

        self.photo_jobs.thumbnail_build.lock().scanning = false;
        if self
            .photo_jobs
            .thumbnail_build_cancelled
            .load(Ordering::SeqCst)
        {
            self.finish_thumbnail_build();
            return;
        }
        let ttype = ThumbnailType::Thumbnail.as_i32();
        let mut todo = Vec::new();
        for (uid, modified, name) in images {
            if self
                .cache
                .cached_thumbnail_path(&uid, ttype, modified)
                .is_some()
            {
                self.photo_jobs.thumbnail_build.lock().completed += 1;
            } else if self
                .photo_jobs
                .thumbnail_misses
                .lock()
                .local_contains(&(uid.clone(), ttype), modified)
            {
                let mut status = self.photo_jobs.thumbnail_build.lock();
                status.completed += 1;
                status.failed += 1;
            } else {
                todo.push((uid, modified, name));
            }
        }

        for chunk in todo.chunks(THUMB_BUILD_CHUNK) {
            if self
                .photo_jobs
                .thumbnail_build_cancelled
                .load(Ordering::SeqCst)
            {
                self.finish_thumbnail_build();
                return;
            }
            let mut claimed = Vec::with_capacity(chunk.len());
            let mut tags = HashMap::with_capacity(chunk.len());
            let mut names = HashMap::with_capacity(chunk.len());
            for (uid, modified, name) in chunk {
                // A visible tile may have started this image just before the
                // toolbar action. Wait for that one bounded job to finish rather
                // than downloading the full image twice.
                let waiting_since = Instant::now();
                loop {
                    if self
                        .photo_jobs
                        .thumbnail_build_cancelled
                        .load(Ordering::SeqCst)
                    {
                        self.finish_thumbnail_build();
                        return;
                    }
                    if self
                        .cache
                        .cached_thumbnail_path(uid, ttype, *modified)
                        .is_some()
                    {
                        self.photo_jobs.thumbnail_build.lock().completed += 1;
                        break;
                    }
                    if self
                        .photo_jobs
                        .thumbnail_misses
                        .lock()
                        .local_contains(&(uid.clone(), ttype), *modified)
                    {
                        let mut status = self.photo_jobs.thumbnail_build.lock();
                        status.completed += 1;
                        status.failed += 1;
                        break;
                    }
                    if self.photo_jobs.thumb_gen.lock().insert(uid.clone()) {
                        tags.insert(uid.to_string(), *modified);
                        names.insert(uid.to_string(), name.clone());
                        claimed.push(uid.clone());
                        break;
                    }
                    if waiting_since.elapsed() >= THUMB_BUILD_CLAIM_TIMEOUT {
                        let mut status = self.photo_jobs.thumbnail_build.lock();
                        status.completed += 1;
                        status.failed += 1;
                        if status.message.is_none() {
                            status.message = Some(
                                "Some images stayed busy and were skipped after a timeout".into(),
                            );
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }

            if claimed.is_empty() {
                continue;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.generate_thumbs(&claimed, &tags, &names, ThumbJob::Build)
            }));
            {
                let mut inflight = self.photo_jobs.thumb_gen.lock();
                for uid in &claimed {
                    inflight.remove(uid);
                }
            }
            let mut status = self.photo_jobs.thumbnail_build.lock();
            match result {
                Ok(summary) => {
                    status.completed += summary.processed();
                    status.failed += summary.failed();
                }
                Err(_) => {
                    status.completed += claimed.len() as u64;
                    status.failed += claimed.len() as u64;
                    status.message = Some("Some thumbnail workers stopped unexpectedly".into());
                }
            }
        }
        self.finish_thumbnail_build();
    }

    /// Persist what a thumbnail attempt learned about a photo, against the
    /// timeline and against every album the photo sits in — one attempt, one
    /// verdict, wherever the photo is painted.
    pub(crate) fn record_thumb(&self, uid: &NodeUid, state: i64, ratio: Option<f64>) {
        let key = uid.to_string();
        if let Err(e) = self.db.photo_set_thumb(&key, state, ratio) {
            warn!(%uid, error = %e, "recording thumbnail state failed");
        }
        if let Err(e) = self.db.album_photo_set_thumb(&key, state, ratio) {
            warn!(%uid, error = %e, "recording album thumbnail state failed");
        }
    }
}
