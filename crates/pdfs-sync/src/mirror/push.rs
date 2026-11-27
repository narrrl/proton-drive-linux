//! The push pass: sending local changes to Drive between full passes.

use super::*;

impl Syncer {
    /// Get every local change up to Drive, and nothing else — the pass run for a
    /// folder waiting to go on-demand, whose local copy is about to be evicted.
    ///
    /// This exists because a full [`do_reconcile`](Self::do_reconcile) is both too
    /// slow and too wasteful to stand between the user and the switch they asked
    /// for. It walks the whole remote tree (the minutes-long part of a pass over a
    /// large folder) to answer a question the switch makes irrelevant — what the
    /// remote side has that we don't — and then downloads those files onto a disk
    /// we are about to clear. A push pass answers only "is anything here not on
    /// Drive yet?", which the local tree and the baseline already know, so it needs
    /// no remote walk at all and finishes in the time the local walk takes.
    ///
    /// The remote is consulted for exactly one thing: the files whose local copy
    /// changed. Those, and only those, could be a two-sided edit, so their nodes are
    /// fetched (cheaply, in one batch) to check whether the remote moved too. If it
    /// did, the local copy goes up under a conflict name rather than overwriting the
    /// other side. Everything else — remote edits, remote deletions, remote-only
    /// files — is left for the FUSE mount to show live once the switch lands.
    ///
    /// Per-path failures are counted, never fatal, exactly as in `do_reconcile`; a
    /// non-zero [`Outcome::errors`] leaves the folder out of `idle`, which keeps the
    /// queued switch waiting rather than evicting a file that never made it up.
    pub(super) fn push_pass(
        &self,
        folder_id: i64,
        local_root: &Path,
        remote_root: &NodeUid,
        rules: &IgnoreRules,
    ) -> Result<Outcome, PassAbort> {
        // This pass uploads without walking Drive, so a walk saved by an
        // earlier pass would not know what it sent; see `do_reconcile`.
        self.store
            .sync_walk_clear(folder_id)
            .map_err(|e| format!("drop the saved walk: {e:?}"))?;
        let step = Instant::now();
        let baseline = self
            .store
            .sync_entries(folder_id)
            .map_err(|e| format!("load baseline: {e:?}"))?;
        self.trace_phase(Phase::Baseline, step.elapsed());
        // Only the local side is walked here, so each baseline path is checked once.
        self.progress_scan_total(folder_id, baseline.len());

        let mut local: HashMap<String, LocalItem> = HashMap::new();
        let mut unscanned = Unscanned::default();
        let step = Instant::now();
        let writing = open_for_write_set(local_root);
        self.walk_local(
            folder_id,
            local_root,
            rules,
            &writing,
            &mut local,
            &mut unscanned,
        )?;
        self.trace_phase(Phase::LocalWalk, step.elapsed());
        self.trace_count(Count::LocalEntries, local.len());
        // The guard compares against the paths this pass could still see: an
        // ignored or unreadable path is absent from `local` by rule or by
        // failure, not by loss, and counting those as missing would trip the
        // guard on every pass — wedging the folder's sync permanently — the
        // moment a rule covers the whole tree.
        let tracked = tracked_baseline(&baseline, rules, &unscanned);
        // A local copy that is gone in its entirety is not trashed on Drive,
        // for the reason `guard_local_wipe` gives. A full pass stops there, but
        // this one exists for a switch to on-demand, which needs nothing local
        // and leaves Drive as it is: refusing the pass would only keep the
        // folder out of `idle`, wedging the switch for good (#46). So the pass
        // forgets those paths instead of trashing them, and the mount then
        // shows everything Drive still has.
        let local_gone = match guard_local_wipe(&tracked, &local) {
            Ok(()) => false,
            Err(reason) => {
                warn!(id = folder_id, %reason, "sync: going on-demand without trashing anything on Drive");
                true
            }
        };

        // The remote folder uids come from the baseline instead of a walk: every
        // directory this device has synced recorded its uid there when it was
        // created or first matched up.
        let mut remote_dirs: HashMap<String, NodeUid> = HashMap::new();
        remote_dirs.insert(String::new(), remote_root.clone());
        for (rel, item) in &local {
            if !item.is_dir {
                continue;
            }
            if let Some(uid) = baseline
                .get(rel)
                .and_then(|e| e.remote_uid.as_deref())
                .and_then(parse_uid)
            {
                remote_dirs.insert(rel.clone(), uid);
            }
        }
        // A local directory the baseline has no uid for is either brand new or one
        // whose first pass never finished — and those look identical from here.
        // Creating it blindly would duplicate a remote folder we simply haven't
        // looked at, so when any exist, the remote folder tree is walked for real.
        // It costs one light listing per folder and no file keys, and a folder whose
        // baseline is complete — the case this pass is written for — skips it
        // entirely and touches the network only for the files it uploads.
        if local
            .iter()
            .any(|(rel, item)| item.is_dir && !remote_dirs.contains_key(rel))
        {
            self.walk_remote_dirs(folder_id, remote_root, "", &mut remote_dirs)?;
        }

        // Files whose local copy has moved since the baseline: the only ones with
        // anything to push, and the only ones that could be a two-sided edit.
        let changed: Vec<&String> = local
            .iter()
            .filter(|(_, item)| !item.is_dir && !item.open_for_write)
            .filter(|(rel, item)| {
                baseline
                    .get(*rel)
                    .is_none_or(|b| b.local_mtime != item.mtime || b.local_size != item.size)
            })
            .map(|(rel, _)| rel)
            .collect();

        // One light batch tells us which of those also changed remotely. A light
        // node carries the modification time and the revision id without
        // unlocking the node key, and those are what the baseline compares
        // against.
        let check: Vec<NodeUid> = changed
            .iter()
            .filter_map(|rel| baseline.get(*rel))
            .filter_map(|e| e.remote_uid.as_deref())
            .filter_map(parse_uid)
            .collect();
        let remote_now: HashMap<String, (i64, Option<String>)> = if check.is_empty() {
            HashMap::new()
        } else {
            self.rt
                .block_on(self.drive.enumerate_nodes_light(&check))
                .map_err(|e| format!("resolve changed nodes: {e}"))?
                .into_iter()
                .filter(|n| !n.trashed)
                .map(|n| {
                    let revision = pdfs_drive::node_revision_id(&n);
                    (n.uid.to_string(), (n.modification_time, revision))
                })
                .collect()
        };
        let changed: HashSet<&String> = changed.into_iter().collect();

        let mut outcome = Outcome {
            deferred: local.values().filter(|item| item.open_for_write).count(),
            unscanned: unscanned.report(),
            ..Default::default()
        };
        let mut order: Vec<&String> = local.keys().collect();
        order.sort_by_key(|p| p.matches('/').count());

        // Same depth-ascending batching as `do_reconcile`: a folder is created
        // remotely before the paths inside it are classified, so their parent uid is
        // in `remote_dirs` by the time they need it.
        let mut batch: Vec<Pending> = Vec::new();
        let mut batch_depth = 0usize;
        for rel in order {
            let depth = rel.matches('/').count();
            if depth > batch_depth {
                self.flush_batch(
                    folder_id,
                    local_root,
                    &mut remote_dirs,
                    std::mem::take(&mut batch),
                    &mut outcome,
                )?;
                batch_depth = depth;
            }
            let item = &local[rel];
            if item.is_dir {
                // The folder is already on Drive, so only its children carry work —
                // but if the walk above is what found it, record the uid now so the
                // next pass over this folder needs no walk at all.
                if let Some(uid) = remote_dirs.get(rel) {
                    if baseline
                        .get(rel)
                        .and_then(|e| e.remote_uid.as_deref())
                        .is_none()
                        && let Err(e) = self.baseline_dir(folder_id, rel, uid)
                    {
                        warn!(rel, error = %e, "sync: folder step failed; continuing");
                        outcome.failed(&format!("{rel}: {e}"));
                    }
                    continue;
                }
                match remote_dirs.get(parent_rel(rel)) {
                    Some(parent) => batch.push(Pending::CreateDir {
                        rel: rel.clone(),
                        parent: parent.clone(),
                    }),
                    None => {
                        warn!(rel, "sync: no remote parent for folder; continuing");
                        outcome.failed(&format!("{rel}: no remote parent for folder"));
                    }
                }
                continue;
            }
            if !changed.contains(rel) {
                // Local copy matches the baseline, so it is already on Drive.
                continue;
            }

            let base = baseline.get(rel);
            let uid = base
                .and_then(|e| e.remote_uid.as_deref())
                .and_then(parse_uid);
            match uid {
                // Never uploaded (or the remote node is gone): it goes up as a new file.
                None => match remote_dirs.get(parent_rel(rel)) {
                    Some(parent) => batch.push(Pending::UploadNew {
                        rel: rel.clone(),
                        parent: parent.clone(),
                    }),
                    None => {
                        warn!(rel, "sync: no remote parent for file; continuing");
                        outcome.failed(&format!("{rel}: no remote parent for file"));
                    }
                },
                Some(uid) => {
                    let (remote_moved, seen) = match remote_now.get(&uid.to_string()) {
                        Some((mtime, revision)) => {
                            let recorded = base.and_then(remote_sig);
                            let moved = recorded.is_none_or(|(m, _)| m != *mtime)
                                || revision_moved(base, revision.as_deref());
                            let seen = RemoteSeen {
                                mtime: *mtime,
                                size: recorded.map_or(0, |(_, size)| size),
                                revision: revision.clone(),
                            };
                            (moved, seen)
                        }
                        // Trashed or unreadable remotely — treat as gone and re-upload
                        // the local copy as a new file rather than losing it.
                        None => {
                            match remote_dirs.get(parent_rel(rel)) {
                                Some(parent) => batch.push(Pending::UploadNew {
                                    rel: rel.clone(),
                                    parent: parent.clone(),
                                }),
                                None => {
                                    warn!(rel, "sync: no remote parent for file; continuing");
                                    outcome.failed(&format!("{rel}: no remote parent for file"));
                                }
                            }
                            continue;
                        }
                    };
                    if remote_moved {
                        match remote_dirs.get(parent_rel(rel)) {
                            Some(parent) => batch.push(Pending::PushConflict {
                                rel: rel.clone(),
                                parent: parent.clone(),
                            }),
                            None => {
                                warn!(rel, "sync: no remote parent for file; continuing");
                                outcome.failed(&format!("{rel}: no remote parent for file"));
                            }
                        }
                    } else {
                        batch.push(Pending::UploadRevision {
                            rel: rel.clone(),
                            uid,
                            seen,
                        });
                    }
                }
            }
        }
        self.flush_batch(
            folder_id,
            local_root,
            &mut remote_dirs,
            std::mem::take(&mut batch),
            &mut outcome,
        )?;

        // Paths the baseline knows that are no longer on disk: the user deleted them
        // locally, and the deletion has to reach Drive before the mount starts
        // showing the folder's remote contents — otherwise everything they deleted
        // comes back the moment the switch lands. Shallowest first, skipping anything
        // under a folder already trashed (its children went with it).
        let deletions = local_deletions(&tracked, &local);
        if local_gone {
            // Forgotten instead of trashed. A row left behind would read to
            // the switch as a deletion not on Drive yet and withdraw it, and
            // to a later full pass as a reason to stop again. Forgotten, a full
            // pass downloads the paths again.
            let mut forgotten: HashSet<&str> = HashSet::new();
            for rel in &deletions {
                let mut parent = parent_rel(rel);
                while !parent.is_empty() && !forgotten.contains(parent) {
                    parent = parent_rel(parent);
                }
                if parent.is_empty() {
                    let _ = self.store.sync_entries_remove_subtree(folder_id, rel);
                    forgotten.insert(rel);
                }
            }
            return Ok(outcome);
        }
        let evidence: Evidence<'_, LocalItem, RemoteItem> = Evidence {
            rules,
            local: &local,
            unscanned: &unscanned,
            remote: None,
        };
        let deletions: Vec<String> = deletions
            .into_iter()
            .filter(|rel| !self.refuses_deletion(&evidence, rel, Side::Remote, &mut outcome))
            .collect();
        let mut trashed: Vec<String> = Vec::new();
        for rel in &deletions {
            if trashed
                .iter()
                .any(|dir| rel.starts_with(&format!("{dir}/")))
            {
                let _ = self.store.sync_entry_remove(folder_id, rel);
                continue;
            }
            let Some(uid) = baseline[rel].remote_uid.as_deref().and_then(parse_uid) else {
                let _ = self.store.sync_entry_remove(folder_id, rel);
                continue;
            };
            if let Err(e) = self
                .rt
                .block_on(self.drive.trash_nodes(&[uid]))
                .and_then(batch::into_unit)
            {
                warn!(rel, error = %e, "sync: trash remote failed");
                self.log_activity(ActivityKind::Trash, base_name(rel), e.to_string(), false);
                outcome.failed(&format!("{rel}: {e}"));
                continue;
            }
            trashed.push(rel.clone());
            if let Some(parent) = parent_uid_of(rel, &remote_dirs, &baseline) {
                self.invalidate_children_of(&parent);
            }
            let _ = self.store.sync_entry_remove(folder_id, rel);
            self.log_activity(
                ActivityKind::Trash,
                base_name(rel),
                "removed on Drive",
                true,
            );
            outcome.deleted += 1;
        }

        Ok(outcome)
    }
}
