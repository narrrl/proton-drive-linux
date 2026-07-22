//! Bulk local-to-Drive upload workflow.

use super::*;

/// How many files the bulk uploader ships at once. Overlaps the per-file network
/// round-trips without letting an unbounded number of block buffers pile up.
const UPLOAD_CONCURRENCY: usize = 4;

/// One file queued for bulk upload, resolved during the directory walk so the
/// concurrent phase carries everything it needs (no shared state, no `block_on`).
struct UploadTask {
    /// Inode of the (already-created) remote parent folder, for interning the
    /// uploaded node afterwards.
    parent_ino: u64,
    parent_uid: NodeUid,
    name: String,
    /// Local filesystem path to stream from.
    path: PathBuf,
    size: u64,
}

/// Tally of a completed [`Core::upload_paths`] batch, for the daemon log.
#[derive(Default)]
pub(super) struct UploadStats {
    pub(super) uploaded: usize,
    pub(super) failed: usize,
    /// Total plaintext bytes of the files that uploaded successfully.
    pub(super) bytes: u64,
    /// Folders created (or reused) to mirror the local tree.
    pub(super) folders: usize,
}

/// Upload every [`UploadTask`] with at most `limit` in flight at once, each
/// streamed straight from disk and ticking its own transfer-registry guard.
/// Returns, per file, either `(parent_ino, new_uid)` for the caller to intern or
/// `(name, error)` to log — one failure never sinks the batch.
///
/// `job` counts files finished (either way: a failure is still one file the batch
/// no longer waits on), so a front-end can show "12 of 40" over the per-file bars.
async fn run_uploads(
    core: Core,
    tasks: Vec<UploadTask>,
    limit: usize,
    job: Arc<JobGuard>,
) -> Vec<Result<(u64, NodeUid, u64), (String, String)>> {
    let sem = Arc::new(tokio::sync::Semaphore::new(limit));
    let mut set = tokio::task::JoinSet::new();
    for t in tasks {
        let core = core.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore not closed");
            let file = match std::fs::File::open(&t.path) {
                Ok(f) => f,
                Err(e) => return Err((t.name, format!("open {}: {e}", t.path.display()))),
            };
            let mtime = std::fs::metadata(&t.path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mt| mt.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let guard = core
                .transfers
                .begin(&t.name, "", TransferDirection::Upload, t.size);
            let reader = OwnedCountingReader::new(file, guard);
            match core
                .client
                .upload_file_from(
                    &t.parent_uid,
                    &t.name,
                    media_type_for(&t.name),
                    reader,
                    t.size as i64,
                    Vec::new(),
                    mtime,
                    false,
                )
                .await
            {
                Ok(uid) => Ok((t.parent_ino, uid, t.size)),
                Err(e) => Err((t.name, format!("upload: {e}"))),
            }
        });
    }
    let mut out = Vec::new();
    while let Some(joined) = set.join_next().await {
        job.step();
        match joined {
            Ok(result) => out.push(result),
            Err(e) => warn!(error = %e, "upload task panicked"),
        }
    }
    out
}

impl Core {
    /// Bulk-upload local files and directory trees under `sources` into the
    /// mountpoint-relative `parent_rel` folder. Directories are recreated (or
    /// merged into an existing same-named folder) and walked; the resulting flat
    /// set of files is uploaded with bounded concurrency, each ticking the
    /// transfer registry so a front-end sees live progress. Runs on a background
    /// thread — a large tree far outlasts the control socket's read timeout — so
    /// it reports only a summary for the log. Individual failures are counted and
    /// logged rather than aborting the whole batch.
    pub(super) fn upload_paths(
        &self,
        parent_rel: &Path,
        sources: &[PathBuf],
    ) -> CoreResult<UploadStats> {
        let (pino, parent_uid) = self.resolve(parent_rel)?;
        self.ensure_children(pino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;

        // Phase 1 (sequential): build the remote folder skeleton and collect the
        // flat list of files to upload. Folders must exist before their children,
        // so this can't be parallelised. On a deep tree this is a folder-creation
        // round-trip per directory before a single byte moves — long enough that
        // it needs a job of its own, or the daemon looks idle for minutes.
        let mut tasks = Vec::new();
        let mut folders = 0usize;
        {
            let job = self.transfers.begin_job("Preparing upload");
            for src in sources {
                if let Err(e) =
                    self.collect_uploads(pino, &parent_uid, src, &mut tasks, &mut folders, &job)
                {
                    warn!(source = %src.display(), error = %e, "skipping source");
                }
            }
        }

        // Phase 2 (concurrent): upload the files, up to UPLOAD_CONCURRENCY at once.
        // Each file reports its own bytes; this job is the batch's "N of M files",
        // which is the number a user actually waits on.
        let job = Arc::new(self.transfers.begin_job(match tasks.len() {
            1 => "Uploading 1 file".to_string(),
            n => format!("Uploading {n} files"),
        }));
        job.set_total(tasks.len() as u64);
        let outcomes = self.rt.block_on(run_uploads(
            self.clone(),
            tasks,
            UPLOAD_CONCURRENCY,
            job.clone(),
        ));
        drop(job);

        // Phase 3 (sequential): intern each uploaded node so it shows up in the
        // listing without a re-enumeration. fetch_node uses `block_on`, so it must
        // run here rather than inside the async batch — and it is a round-trip per
        // file, so it too gets a job rather than a silent tail.
        let mut stats = UploadStats {
            folders,
            ..UploadStats::default()
        };
        let job = self.transfers.begin_job("Finishing upload");
        job.set_total(outcomes.len() as u64);
        for outcome in outcomes {
            job.step();
            match outcome {
                Ok((parent_ino, uid, size)) => {
                    stats.uploaded += 1;
                    stats.bytes += size;
                    match self.fetch_node(&uid) {
                        Ok(node) => {
                            let mut st = self.state.lock();
                            let ino = st.intern(parent_ino, node);
                            if let Some(kids) = st.children.get_mut(&parent_ino)
                                && !kids.contains(&ino)
                            {
                                kids.push(ino);
                            }
                        }
                        // Uploaded fine but the metadata refresh failed; it will
                        // appear on the next directory enumeration regardless.
                        Err(e) => warn!(%uid, error = ?e, "uploaded node metadata refresh failed"),
                    }
                }
                Err((name, msg)) => {
                    stats.failed += 1;
                    warn!(name, error = %msg, "file upload failed");
                }
            }
        }
        info!(
            uploaded = stats.uploaded,
            failed = stats.failed,
            "bulk upload finished"
        );
        Ok(stats)
    }

    /// Resolve a remote child folder named `name` under `pino`, creating it if it
    /// doesn't exist, and return its `(inode, uid)`. Reusing an existing same-named
    /// folder makes re-uploading a directory merge into it rather than fail on a
    /// duplicate name.
    fn ensure_remote_folder(
        &self,
        pino: u64,
        parent_uid: &NodeUid,
        name: &str,
    ) -> CoreResult<(u64, NodeUid)> {
        if name.is_empty() || name.contains('/') {
            return Err(CoreError::invalid(format!("invalid folder name: {name:?}")));
        }
        self.ensure_children(pino)
            .map_err(|e| self.errno_error(e, "enumerate"))?;
        {
            let st = self.state.lock();
            if let Some(kids) = st.children.get(&pino) {
                for &ino in kids {
                    if let Some(e) = st.entries.get(&ino)
                        && e.node.is_folder()
                        && e.node.name == name
                    {
                        return Ok((ino, e.uid.clone()));
                    }
                }
            }
        }
        let new_uid = self
            .rt
            .block_on(
                self.client
                    .create_folder(parent_uid, name, Some(now_secs())),
            )
            .map_err(|e| CoreError::from_api(&e, &format!("create folder {name}")))?;
        let node = self
            .fetch_node(&new_uid)
            .map_err(|e| self.errno_error(e, "fetch node"))?;
        let mut st = self.state.lock();
        let ino = st.intern(pino, node);
        if let Some(kids) = st.children.get_mut(&pino)
            && !kids.contains(&ino)
        {
            kids.push(ino);
        }
        Ok((ino, new_uid))
    }

    /// Walk one local source path, appending its files to `tasks`. A file becomes
    /// one task; a directory is recreated remotely and recursed into (children
    /// sorted for a stable order). Symlinks and other special files are skipped.
    ///
    /// `job` narrates the walk with the folder currently being mirrored. It stays
    /// indeterminate: nothing knows the size of the tree until the walk has ended.
    fn collect_uploads(
        &self,
        pino: u64,
        parent_uid: &NodeUid,
        src: &Path,
        tasks: &mut Vec<UploadTask>,
        folders: &mut usize,
        job: &JobGuard,
    ) -> CoreResult<()> {
        let meta = std::fs::symlink_metadata(src)
            .map_err(|e| CoreError::internal(format!("stat {}: {e}", src.display())))?;
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| CoreError::invalid(format!("unusable name: {}", src.display())))?
            .to_string();

        if meta.is_dir() {
            job.detail(&name);
            let (child_ino, child_uid) = self.ensure_remote_folder(pino, parent_uid, &name)?;
            *folders += 1;
            let mut entries: Vec<PathBuf> = std::fs::read_dir(src)
                .map_err(|e| CoreError::internal(format!("read dir {}: {e}", src.display())))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            entries.sort();
            for entry in entries {
                if let Err(e) =
                    self.collect_uploads(child_ino, &child_uid, &entry, tasks, folders, job)
                {
                    warn!(source = %entry.display(), error = %e, "skipping entry");
                }
            }
            // Deeper folders have retitled the job by now; put this one back so the
            // line tracks the walk's position rather than its deepest leaf.
            job.detail(&name);
        } else if meta.is_file() {
            if name.contains('/') {
                return Err(CoreError::invalid(format!("invalid file name: {name:?}")));
            }
            tasks.push(UploadTask {
                parent_ino: pino,
                parent_uid: parent_uid.clone(),
                name,
                path: src.to_path_buf(),
                size: meta.len(),
            });
        }
        Ok(())
    }
}
