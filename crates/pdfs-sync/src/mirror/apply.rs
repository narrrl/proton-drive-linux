//! Applying a pass's plan: uploads, downloads and the other effects, in
//! batches, and the baseline each one leaves behind.

use super::*;

impl Syncer {
    /// Store the revision id of each file in one listing whose baseline row has
    /// none, when the size the walk just read shows that file unchanged.
    ///
    /// The classification step does the same, but only a pass that gets all the
    /// way through the walk reaches it. A large folder whose walk is cut short
    /// every time (a restart, a logout) then reads the size of every such file
    /// again, one key unlock each, on every pass, and never finishes (B294).
    pub(super) fn record_revisions(
        &self,
        folder_id: i64,
        prefix: &str,
        nodes: &[Node],
        sized: &HashMap<String, (i64, Option<String>)>,
        baseline: &HashMap<String, StoredSyncEntry>,
    ) {
        let rows = revisions_to_record(prefix, nodes, sized, baseline);
        if rows.is_empty() {
            return;
        }
        match self.store.sync_entries_set_revision(folder_id, &rows) {
            Ok(n) => self.trace_count(Count::Revisions, n),
            Err(e) => debug!(folder = prefix, error = ?e, "sync: cannot record revision ids"),
        }
    }

    /// Record a folder's baseline row (folders carry no content signature).
    pub(super) fn baseline_dir(
        &self,
        folder_id: i64,
        rel: &str,
        uid: &NodeUid,
    ) -> Result<(), String> {
        self.store
            .sync_entry_upsert(folder_id, &dir_row(rel, uid))
            .map_err(|e| format!("baseline dir {rel}: {e:?}"))
    }

    /// Run a batch of queued [`Pending`] ops concurrently (bounded by
    /// [`SYNC_CONCURRENCY`]) and fold their results back into the reconcile's
    /// shared state on the engine thread: created folders are registered in
    /// `remote_dirs` (so deeper paths resolve their parent), and what each op did
    /// is tallied into `outcome`. Driven by `block_on` from the (non-runtime) sync
    /// engine thread, spawning the tasks onto the shared runtime.
    ///
    /// Fails with [`PassAbort::Stopped`] once the daemon is stopping, after the
    /// ops already sending have finished.
    pub(super) fn flush_batch(
        &self,
        folder_id: i64,
        local_root: &Path,
        remote_dirs: &mut HashMap<String, NodeUid>,
        batch: Vec<Pending>,
        outcome: &mut Outcome,
    ) -> Result<(), PassAbort> {
        if batch.is_empty() {
            return Ok(());
        }
        self.progress_queued(folder_id, batch.len());
        self.trace_count(Count::Queued, batch.len());
        let applying = Instant::now();
        let core = self.clone();
        let root = local_root.to_path_buf();
        let results = self.rt.block_on(async move {
            let sem = Arc::new(tokio::sync::Semaphore::new(SYNC_CONCURRENCY));
            let mut set = tokio::task::JoinSet::new();
            for op in batch {
                let core = core.clone();
                let sem = sem.clone();
                let root = root.clone();
                set.spawn(async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore not closed");
                    // An op still waiting for its turn when the stop comes is
                    // not started; the ones already sending finish.
                    if core.shutdown.is_stopping() {
                        return None;
                    }
                    Some(core.apply_pending(folder_id, &root, op).await)
                });
            }
            let mut out = Vec::new();
            let mut join_errors = 0usize;
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Some(result)) => out.push(result),
                    Ok(None) => {}
                    // The runtime ends with the daemon and cancels what is left.
                    Err(e) if e.is_cancelled() => {}
                    Err(e) => {
                        join_errors += 1;
                        warn!(error = %e, "sync: task panicked");
                    }
                }
            }
            (out, join_errors)
        });
        self.trace_phase(Phase::Apply, applying.elapsed());
        let (results, join_errors) = results;
        outcome.errors += join_errors;
        for result in results {
            match result {
                Ok(applied) => {
                    outcome.record(&applied);
                    if let Applied::Dir(rel, uid) = applied {
                        remote_dirs.insert(rel, uid);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "sync: step failed; continuing");
                    outcome.failed(&e);
                }
            }
        }
        // The pass goes no further: the next depth's paths may need a folder
        // this batch did not get to make, and went on to fail one by one
        // (`docs/BUGS.md` B169).
        if self.shutdown.is_stopping() {
            return Err(PassAbort::Stopped);
        }
        if self.pass_abandoned(folder_id) {
            return Err(PassAbort::Abandoned);
        }
        Ok(())
    }

    /// Whether a removal of `folder_id` is waiting for its running pass.
    pub(super) fn pass_abandoned(&self, folder_id: i64) -> bool {
        self.abandon.lock().contains(&folder_id)
    }

    /// Apply one [`Pending`] op (async, so it can run concurrently in a batch),
    /// reporting it to the folder's live progress and to the activity feed. Each
    /// item logs its own line — the feed is meant to answer "what is it doing",
    /// which a pass-level summary alone cannot.
    pub(super) async fn apply_pending(
        &self,
        folder_id: i64,
        local_root: &Path,
        op: Pending,
    ) -> Result<Applied, String> {
        self.progress_started(folder_id, base_name(op.rel()));
        let step = Instant::now();
        let result = self.apply_one(folder_id, local_root, &op).await;
        self.trace_call(op.call(), step.elapsed());
        self.progress_finished(folder_id);
        let name = base_name(op.rel());
        match &result {
            Ok(Applied::Adopted) => {}
            Ok(_) => self.log_activity(op.kind(), name, op.detail(), true),
            Err(e) => self.log_activity(op.kind(), name, e.clone(), false),
        }
        result
    }

    /// The body of one [`Pending`] op, without the progress/activity bookkeeping.
    pub(super) async fn apply_one(
        &self,
        folder_id: i64,
        local_root: &Path,
        op: &Pending,
    ) -> Result<Applied, String> {
        match op {
            Pending::CreateDir { rel, parent } => {
                let uid = self
                    .drive
                    .create_folder(parent, base_name(rel), Some(now_secs()))
                    .await
                    .map_err(|e| format!("create remote folder {rel}: {e}"))?;
                // The parent has gained a child that no cached listing of it
                // knows about (B86).
                self.invalidate_children_of(parent);
                self.baseline_dir(folder_id, rel, &uid)?;
                Ok(Applied::Dir(rel.clone(), uid))
            }
            Pending::UploadNew { rel, parent } => {
                self.upload_new(folder_id, local_root, rel, parent).await
            }
            Pending::UploadRevision { rel, uid, seen } => {
                self.upload_revision(folder_id, local_root, rel, uid, seen)
                    .await
            }
            Pending::Download {
                rel,
                uid,
                seen,
                local,
            } => {
                self.download_file(folder_id, local_root, rel, uid, seen, *local)
                    .await?;
                Ok(Applied::Downloaded)
            }
            Pending::Adopt {
                rel,
                uid,
                seen: remote,
                sha1,
                local,
            } => {
                let path = local_root.join(rel_to_path(rel));
                let hashed = tokio::task::spawn_blocking(move || local_sha1(&path))
                    .await
                    .map_err(|e| format!("hash {rel}: {e}"))?
                    .map_err(|e| format!("hash {rel}: {e}"))?;
                let Some((seen, digest)) = hashed else {
                    return Ok(Applied::Vanished);
                };
                // Edited since the scan: the next pass plans it again.
                if !seen.same_content(local) {
                    return Ok(Applied::Vanished);
                }
                if !digest.eq_ignore_ascii_case(sha1) {
                    info!(
                        rel,
                        "sync: same size as on Drive but different content; keeping a conflict copy"
                    );
                    return self
                        .take_remote_keeping_local(folder_id, local_root, rel, uid, remote)
                        .await;
                }
                self.store
                    .sync_entry_upsert(
                        folder_id,
                        &StoredSyncEntry {
                            rel_path: rel.clone(),
                            remote_uid: Some(uid.to_string()),
                            local_mtime: seen.mtime,
                            local_mtime_ns: seen.mtime_ns,
                            local_size: seen.size,
                            remote_rev: Some(remote.mtime.to_string()),
                            remote_hash: Some(remote.size.to_string()),
                            remote_revision_id: remote.revision.clone(),
                        },
                    )
                    .map_err(|e| format!("adopt baseline {rel}: {e:?}"))?;
                Ok(Applied::Adopted)
            }
            Pending::Conflict { rel, uid, seen } => {
                self.take_remote_keeping_local(folder_id, local_root, rel, uid, seen)
                    .await
            }
            Pending::PushConflict { rel, parent } => {
                // The ordinary conflict resolution — rename the local copy aside and
                // let the next pass upload it — loses the file here: the folder is
                // going on-demand, so "next pass" comes after the local tree has been
                // evicted. Push the conflict copy now, while it still exists, and
                // leave the remote file holding the original path.
                let path = local_root.join(rel_to_path(rel));
                let conflict = preserve_conflict_copy(&path, now_secs())
                    .map_err(|e| format!("set aside conflict copy for {rel}: {e}"))?;
                let name = conflict
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| format!("conflict copy for {rel} has no name"))?;
                let conflict_rel = join_rel(parent_rel(rel), &name);
                self.upload_new(folder_id, local_root, &conflict_rel, parent)
                    .await?;
                info!(rel, "sync: uploaded local changes as a conflict copy");
                Ok(Applied::Conflict)
            }
        }
    }

    /// Set the local copy aside (it re-uploads as a new file next pass), then
    /// take the remote version as the shared truth.
    pub(super) async fn take_remote_keeping_local(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        uid: &NodeUid,
        seen: &RemoteSeen,
    ) -> Result<Applied, String> {
        let path = local_root.join(rel_to_path(rel));
        if path.exists() {
            preserve_conflict_copy(&path, now_secs())
                .map_err(|e| format!("set aside conflict copy for {rel}: {e}"))?;
            info!(rel, "sync: kept local changes as a conflict copy");
        }
        // The local copy has just been moved aside, so the state this
        // download expects to find at the path is "nothing" — anything
        // that appears while it runs is a *new* racing write, and gets
        // its own conflict copy rather than being overwritten (B39).
        self.download_file(folder_id, local_root, rel, uid, seen, None)
            .await?;
        Ok(Applied::Conflict)
    }

    /// Upload a brand-new local file to its remote parent, then record baseline.
    pub(super) async fn upload_new(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        parent: &NodeUid,
    ) -> Result<Applied, String> {
        let path = local_root.join(rel_to_path(rel));
        let name = base_name(rel);
        let Some(file) = open_to_upload(&path, rel)? else {
            return Ok(Applied::Vanished);
        };
        let meta = file.metadata().map_err(|e| format!("stat {rel}: {e}"))?;
        let mtime = system_mtime(&meta);
        // Taken from the descriptor being streamed, so it names the content that
        // actually goes up rather than whatever is at the path when we finish.
        let streamed = LocalSig::from(&meta);
        let thumbnails = self
            .thumbnails_for_upload(path.clone(), name.to_string())
            .await;
        // Count the bytes as they are read, so this upload shows up in
        // `GetQueueStatus` next to a manual one. The uid isn't known until the
        // draft is sealed, so the transfer registers without one.
        let (reader, sent) = Sha1Tap::new(OwnedCountingReader::new(
            file,
            self.transfers
                .begin(name, "", TransferDirection::Upload, meta.len()),
        ));
        let uid = self
            .drive
            // Mirror push: the local file is authoritative. If a prior attempt was
            // interrupted mid-upload it left an unsealed draft of this name; recover
            // it even across a daemon restart (which rotates our client uid), so the
            // folder can reach idle instead of `AlreadyExists`-looping forever.
            .upload_file_replacing_draft_from(
                parent,
                name,
                pdfs_drive::media_type_for(name),
                reader,
                meta.len() as i64,
                thumbnails,
                Some(mtime),
                false,
            )
            .await
            .map_err(|e| format!("upload {rel}: {e}"))?;
        // The parent has gained a child that no cached listing of it knows
        // about. Without this, a folder later switched to on-demand serves a
        // snapshot that predates this upload (B86).
        self.invalidate_children_of(parent);
        warn_if_torn(rel, &path, streamed);
        let remote = self
            .landed_as_sent(rel, &uid, &sent.digest(), streamed)
            .await?;
        self.record_file_baseline(folder_id, rel, Some(streamed), &uid, &remote)?;
        Ok(Applied::Uploaded)
    }

    /// Upload a changed local file as a new revision of an existing remote node.
    ///
    /// The revision supersedes whatever is current on Drive, and the plan comes
    /// from a walk that may be minutes old. Another computer syncing the same
    /// folder may have committed a revision since, and uploading over it would
    /// leave that edit only in the revision history. So Drive is asked first,
    /// and a file that moved on is left for the next pass, which keeps the
    /// local edit as a conflict copy (B249). A revision committed between this
    /// check and the upload's own draft is not caught here; the read-back after
    /// the upload catches one committed after it.
    pub(super) async fn upload_revision(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        uid: &NodeUid,
        seen: &RemoteSeen,
    ) -> Result<Applied, String> {
        let path = local_root.join(rel_to_path(rel));
        let Some(file) = open_to_upload(&path, rel)? else {
            return Ok(Applied::Vanished);
        };
        let current = self
            .drive
            .enumerate_nodes(std::slice::from_ref(uid))
            .await
            .map_err(|e| format!("check {rel} on Drive: {e}"))?;
        match current.first() {
            Some(node) if !node.trashed && seen.still_current(node) => {}
            Some(node) if !node.trashed => {
                return Err(format!(
                    "{rel} changed on Drive since this pass looked; it is compared again on the next pass"
                ));
            }
            _ => {
                return Err(format!(
                    "{rel} is no longer on Drive where this pass saw it; it is compared again on the next pass"
                ));
            }
        }
        let meta = file.metadata().map_err(|e| format!("stat {rel}: {e}"))?;
        let mtime = system_mtime(&meta);
        let streamed = LocalSig::from(&meta);
        let thumbnails = self
            .thumbnails_for_upload(path.clone(), base_name(rel).to_string())
            .await;
        let (reader, sent) = Sha1Tap::new(OwnedCountingReader::new(
            file,
            self.transfers.begin(
                base_name(rel),
                uid.to_string(),
                TransferDirection::Upload,
                meta.len(),
            ),
        ));
        self.drive
            .upload_new_revision_from(uid, reader, meta.len() as i64, thumbnails, Some(mtime))
            .await
            .map_err(|e| format!("upload revision {rel}: {e}"))?;
        warn_if_torn(rel, &path, streamed);
        let remote = self
            .landed_as_sent(rel, uid, &sent.digest(), streamed)
            .await?;
        self.record_file_baseline(folder_id, rel, Some(streamed), uid, &remote)?;
        Ok(Applied::Uploaded)
    }

    /// Download a remote file to its local path (atomically via a temp file),
    /// stamp the local mtime to match the remote, then record baseline.
    ///
    /// `expected_local` is the identity planning saw at the destination. The
    /// rename below is the destructive step, and the download in between can run
    /// for minutes, so the destination is re-examined right before it: a local
    /// file that no longer matches the plan is set aside as a conflict copy
    /// instead of being replaced (`docs/BUGS.md` B39).
    ///
    /// The baseline records `seen`, the revision the pass planned to download,
    /// and not a read of the node afterwards. Another computer may commit a
    /// revision while the download runs, and a read-back would record that
    /// revision as synced while the disk holds the older one, so its edit would
    /// never arrive (B250). If the download did fetch the newer revision, the
    /// next pass fetches it once more, which costs a transfer and loses nothing.
    pub(super) async fn download_file(
        &self,
        folder_id: i64,
        local_root: &Path,
        rel: &str,
        uid: &NodeUid,
        seen: &RemoteSeen,
        expected_local: Option<LocalSig>,
    ) -> Result<(), String> {
        let (mtime, size) = (seen.mtime, seen.size);
        let path = local_root.join(rel_to_path(rel));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir for {rel}: {e}"))?;
        }
        let tmp = staging_path(&path);
        // A file an on-demand mount already read, say one just moved here from
        // it, is in the content cache under the same revision key.
        let seeded = size > 0 && self.cache.copy_cached_to(uid, mtime, size as u64, &tmp);
        if seeded {
            std::fs::File::open(&tmp)
                .and_then(|f| f.sync_all())
                .map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    format!("sync tmp {rel}: {e}")
                })?;
            debug!(
                rel,
                "sync: placed from the content cache instead of downloading"
            );
        } else {
            let out = std::fs::File::create(&tmp).map_err(|e| format!("create tmp {rel}: {e}"))?;
            // Count the bytes as they land, so the download reports progress like
            // an on-demand hydration does.
            let guard = self.transfers.begin(
                base_name(rel),
                uid.to_string(),
                TransferDirection::Download,
                size.max(0) as u64,
            );
            let mut out = CountingWriter::new(out, &guard);
            self.drive
                .download_file_to(uid, &mut out)
                .await
                .map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    format!("download {rel}: {e}")
                })?;
            let mut out = out.into_inner();
            out.flush().map_err(|e| format!("flush tmp {rel}: {e}"))?;
            out.sync_all().map_err(|e| format!("sync tmp {rel}: {e}"))?;
        }
        place_download(&tmp, &path, expected_local, mtime, rel)?;
        // Here the file on disk *is* the transferred content: it was published
        // by a rename and stamped just above, so its stat is the identity.
        self.record_file_baseline(folder_id, rel, local_sig(&path), uid, seen)
    }

    /// What Drive holds for `uid` after this daemon uploaded content with SHA-1
    /// `sent` and identity `streamed` to it, as the baseline should record it.
    ///
    /// Read back so the stored signature is exactly what a walk reports next
    /// time. A digest on Drive that is not `sent` means another computer
    /// committed a revision right after this upload. Recording that revision
    /// as synced would hide it, and the edit here would be lost to it on the
    /// next download. So the baseline is left as it was, and the next pass sees
    /// both sides changed and keeps a conflict copy (B250).
    pub(super) async fn landed_as_sent(
        &self,
        rel: &str,
        uid: &NodeUid,
        sent: &str,
        streamed: LocalSig,
    ) -> Result<RemoteSeen, String> {
        let node = match self.drive.enumerate_nodes(std::slice::from_ref(uid)).await {
            Ok(nodes) => nodes.into_iter().next(),
            Err(e) => {
                warn!(rel, error = %e, "sync: baseline refetch failed; using local mtime");
                None
            }
        };
        let Some(node) = node else {
            // Drive does not list a node it has just made at once. The upload
            // stamped it with the local modification time, so that and the
            // size sent are what a walk reports.
            return Ok(RemoteSeen {
                mtime: streamed.mtime,
                size: streamed.size,
                revision: None,
            });
        };
        if let Some(digest) = pdfs_drive::node_content_sha1(&node)
            && !digest.eq_ignore_ascii_case(sent)
        {
            return Err(format!(
                "{rel} was changed on Drive by another client right after this upload; it is compared again on the next pass"
            ));
        }
        let (mtime, size) = remote_file_sig(&node);
        Ok(RemoteSeen {
            mtime,
            size,
            revision: pdfs_drive::node_revision_id(&node),
        })
    }

    /// Record a file's baseline: the local identity these bytes came from and
    /// the remote revision they were synced with. Called after every
    /// upload/download so the very next reconcile sees the path as unchanged.
    ///
    /// `local` is the identity the *transferred content* corresponds to, which
    /// is not always the file's current stat. An upload streams the live file,
    /// so it passes the identity it read the first byte at: if a writer changed
    /// the file mid-stream, the remote now holds a torn revision, and recording
    /// the pre-upload identity is what makes the next pass see the path as
    /// locally changed and upload a whole, clean revision over it. Recording the
    /// post-upload stat instead declared the torn revision synchronized and left
    /// it as the file's content on Drive (`docs/BUGS.md` B40).
    pub(super) fn record_file_baseline(
        &self,
        folder_id: i64,
        rel: &str,
        local: Option<LocalSig>,
        uid: &NodeUid,
        remote: &RemoteSeen,
    ) -> Result<(), String> {
        let Some(local) = local else {
            return Err(format!("stat {rel}: no local identity to baseline"));
        };
        self.store
            .sync_entry_upsert(
                folder_id,
                &StoredSyncEntry {
                    rel_path: rel.to_string(),
                    remote_uid: Some(uid.to_string()),
                    local_mtime: local.mtime,
                    local_mtime_ns: local.mtime_ns,
                    local_size: local.size,
                    remote_rev: Some(remote.mtime.to_string()),
                    remote_hash: Some(remote.size.to_string()),
                    remote_revision_id: remote.revision.clone(),
                },
            )
            .map_err(|e| format!("baseline {rel}: {e:?}"))
    }
}
