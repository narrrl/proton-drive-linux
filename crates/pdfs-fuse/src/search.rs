//! Search over the tree, the database and the home index.

use super::*;

impl Core {
    /// The local locations a search result can be opened through, resolved once
    /// per search rather than once per hit.
    ///
    /// An on-demand folder only answers while its mount is live — without the
    /// mount its `local_path` is an ordinary empty directory. A mirror folder
    /// needs no mount: its files are real local copies the sync loop pushes
    /// back, so it is a valid open target whenever it is configured.
    ///
    /// This used to be a per-hit call that re-read the sync-folder list and ran
    /// an ancestor walk per folder, against a candidate pool of up to a
    /// thousand: the whole cost of a keystroke was in here.
    pub(super) fn search_roots(&self) -> SearchRoots {
        let live_mounts = self.mounts.lock();
        let folders = self.db.sync_folder_list().unwrap_or_default();
        SearchRoots {
            roots: folders
                .into_iter()
                .filter(|folder| match folder.mode.as_str() {
                    "ondemand" => live_mounts.contains_key(&folder.id),
                    "mirror" => true,
                    _ => false,
                })
                .filter_map(|folder| {
                    let root = self.db.node_path(&folder.remote_uid).ok().flatten()?;
                    Some((root, PathBuf::from(folder.local_path)))
                })
                .collect(),
        }
    }

    pub(super) fn search(
        &self,
        query: &str,
        limit: usize,
        scope: Option<&str>,
    ) -> CoreResult<Vec<SearchHit>> {
        let hits = self
            .db
            .search_in(query, limit, scope)
            .map_err(|e| CoreError::from_api(&e, "search"))?;
        let roots = self.search_roots();
        let pinned = self.pinned_set();
        Ok(hits
            .into_iter()
            .map(|h| SearchHit {
                name: h.node.name.clone(),
                is_dir: h.node.is_folder(),
                size: node_size(&h.node),
                modified: h.node.modification_time,
                pinned: pinned.contains(&h.node.uid.to_string()),
                cached: !h.node.is_folder()
                    && self.cache.is_cached(
                        &h.node.uid,
                        h.node.modification_time,
                        node_size(&h.node),
                    ),
                uid: h.node.uid.to_string(),
                mounted_path: roots.resolve(&h.path),
                path: h.path,
                score: 0,
            })
            .collect())
    }

    /// Search the index of files on this machine (outside Drive), built by the
    /// background scanner in [`run_local_index`]. Pure local lookup, never hits
    /// the network — and never touches the FUSE mount, which the scanner excludes.
    pub(super) fn search_local(&self, query: &str, limit: usize) -> CoreResult<Vec<LocalHit>> {
        let hits = self
            .db
            .search_local(query, limit)
            .map_err(|e| CoreError::from_api(&e, "local search"))?;
        Ok(hits
            .into_iter()
            .map(|h| LocalHit {
                name: h.name,
                path: h.path,
                is_dir: h.is_dir,
                size: h.size.max(0) as u64,
                modified: h.mtime,
                score: 0,
            })
            .collect())
    }

    /// Search both prompt sources with identical query and per-source limits.
    /// Keeping the composition in the daemon core gives every control surface
    /// the same semantics while preserving the legacy source-specific methods.
    pub(super) fn search_v2(
        &self,
        query: &str,
        limit: usize,
        filters: &SearchFilters,
    ) -> CoreResult<(Vec<SearchHit>, Vec<LocalHit>)> {
        if query.trim().is_empty() || limit == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        // Candidate generation is intentionally broader than the final result
        // cap: filtering and fuzzy rejection happen below, so limiting at the
        // SQLite boundary to `limit` would recreate the old empty-filter bug.
        let candidate_limit = limit.saturating_mul(10).clamp(100, 1_000);
        let mut drive_hits = if filters.sources.contains(&SearchSource::Drive) {
            let roots = self.search_roots();
            let pinned = self.pinned_set();
            self.db
                .search_candidates(query, candidate_limit)
                .map_err(|e| CoreError::from_api(&e, "search candidates"))?
                .into_iter()
                .filter(|hit| filters.kind.accepts(&hit.node.name, hit.node.is_folder()))
                .filter_map(|hit| {
                    let parent = Path::new(&hit.path)
                        .parent()
                        .map_or_else(String::new, |path| path.display().to_string());
                    let uid = hit.node.uid.to_string();
                    let is_pinned = pinned.contains(&uid);
                    let score = relevance_score(query, &hit.node.name, &parent)?
                        + if is_pinned { 250 } else { 0 };
                    Some(SearchHit {
                        name: hit.node.name.clone(),
                        is_dir: hit.node.is_folder(),
                        size: node_size(&hit.node),
                        modified: hit.node.modification_time,
                        pinned: is_pinned,
                        cached: !hit.node.is_folder()
                            && self.cache.is_cached(
                                &hit.node.uid,
                                hit.node.modification_time,
                                node_size(&hit.node),
                            ),
                        uid,
                        mounted_path: roots.resolve(&hit.path),
                        path: hit.path,
                        score,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut local_hits = if filters.sources.contains(&SearchSource::Local) {
            self.db
                .search_local_candidates(query, candidate_limit)
                .map_err(|e| CoreError::from_api(&e, "local search candidates"))?
                .into_iter()
                .filter(|hit| filters.kind.accepts(&hit.name, hit.is_dir))
                .filter_map(|hit| {
                    let parent = Path::new(&hit.path)
                        .parent()
                        .map_or_else(String::new, |path| path.display().to_string());
                    Some(LocalHit {
                        score: relevance_score(query, &hit.name, &parent)?,
                        name: hit.name,
                        path: hit.path,
                        is_dir: hit.is_dir,
                        size: hit.size.max(0) as u64,
                        modified: hit.mtime,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        drive_hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.path.cmp(&b.path))
        });
        local_hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.path.cmp(&b.path))
        });
        drive_hits.truncate(limit);
        local_hits.truncate(limit);
        Ok((drive_hits, local_hits))
    }
}
