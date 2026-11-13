//! Keeping pinned files on this computer after they were pinned.
//!
//! A pin downloads a file once. Nothing downloaded it again afterwards: a
//! change from another device deleted the cached copy, a file added to a
//! pinned folder elsewhere was never fetched, and a download that failed while
//! the link was down stayed failed. Each of those left a file listed as kept
//! offline that the next read downloaded, and that could not be read at all
//! without a network (`docs/BUGS.md` B282).
//!
//! The keeper is one thread that checks pins again and downloads what is
//! missing or stale: every pin when the daemon starts and when the link comes
//! back, and the pins a batch of remote events touched once the batch is
//! applied. A remote change no longer deletes a pinned file's copy. The copy
//! is stored under the revision's `(mtime, size)`, so a read of a newer
//! revision does not take it, and the keeper replaces it once the new one is
//! downloaded.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use proton_drive_rs::NodeKind;
use proton_drive_rs::proton_sdk::ids::NodeUid;
use tracing::{debug, info, warn};

use super::{Core, is_local_uid, node_size, parse_uid};

/// How often the keeper looks for work.
const KEEPER_TICK: Duration = Duration::from_secs(1);
/// The first wait before a check that could not download everything is tried
/// again. It doubles on every failure, up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(30 * 60);

/// What the keeper owes a check.
#[derive(Debug, Default)]
pub(crate) struct PinWork {
    /// Every pin.
    all: bool,
    /// The pinned files and folders named here. A folder is checked whole.
    nodes: HashSet<NodeUid>,
}

impl PinWork {
    fn is_empty(&self) -> bool {
        !self.all && self.nodes.is_empty()
    }

    fn merge(&mut self, other: PinWork) {
        self.all |= other.all;
        self.nodes.extend(other.nodes);
    }
}

/// A file a pin keeps, as the tree knows it now.
pub(crate) struct KeptFile {
    pub(crate) uid: NodeUid,
    pub(crate) name: String,
    pub(crate) mtime: i64,
    pub(crate) size: u64,
}

impl Core {
    /// Have the keeper check every pin.
    pub(crate) fn recheck_all_pins(&self) {
        self.pin_work.lock().all = true;
    }

    /// Have the keeper check the pinned file or folder `uid`.
    pub(crate) fn recheck_pinned(&self, uid: &NodeUid) {
        self.pin_work.lock().nodes.insert(uid.clone());
    }

    /// The keeper's loop, until the daemon stops. Starts with a check of every
    /// pin, which catches up on whatever changed while the daemon was down.
    ///
    /// Work that failed waits out a backoff of its own. New work does not wait
    /// for it, so a file that keeps failing does not hold up the next change.
    pub(crate) fn run_pin_keeper(&self) {
        self.recheck_all_pins();
        let mut backlog = PinWork::default();
        let mut backlog_due = Instant::now();
        let mut retry = RETRY_FIRST;
        while self.shutdown.sleep(KEEPER_TICK) {
            if !self.is_online() {
                continue;
            }
            let mut work = std::mem::take(&mut *self.pin_work.lock());
            let retrying = !backlog.is_empty() && Instant::now() >= backlog_due;
            if retrying {
                work.merge(std::mem::take(&mut backlog));
            }
            if work.is_empty() {
                continue;
            }
            let failed = self.keep_pins(&work);
            if failed.is_empty() {
                if retrying {
                    retry = RETRY_FIRST;
                }
                continue;
            }
            backlog.merge(failed);
            backlog_due = Instant::now() + retry;
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    /// Download every file `work` covers whose cached copy is missing or
    /// stale. Returns what could not be checked or downloaded.
    fn keep_pins(&self, work: &PinWork) -> PinWork {
        let mut failed = PinWork::default();
        let roots: Vec<NodeUid> = if work.all {
            self.cache
                .list_pins()
                .iter()
                .filter_map(|pin| parse_uid(&pin.uid))
                .chain(work.nodes.iter().cloned())
                .collect()
        } else {
            work.nodes.iter().cloned().collect()
        };
        let mut seen: HashSet<u64> = HashSet::new();
        let mut files: Vec<u64> = Vec::new();
        for uid in roots {
            if self.shutdown.is_stopping() {
                return failed;
            }
            if !self.cache.is_pinned(&uid) {
                continue;
            }
            let found = {
                let st = self.state();
                st.by_uid.get(&uid).and_then(|&ino| {
                    let e = st.entries.get(&ino)?;
                    Some((ino, e.parent, e.node.is_folder()))
                })
            };
            let Some((ino, parent, is_folder)) = found else {
                debug!(%uid, "pin keeper: pinned node is not in the tree");
                continue;
            };
            if is_folder {
                match self.pin_subtree_files(ino) {
                    Ok(inos) => files.extend(inos.into_iter().filter(|i| seen.insert(*i))),
                    Err(e) => {
                        warn!(%uid, error = %e, "pin keeper: listing a kept folder failed");
                        failed.nodes.insert(uid);
                    }
                }
                continue;
            }
            // An event about this file dropped its parent's listing. Listing
            // it again brings the file's entry up to the revision Drive has
            // now, which is what the cached copy is checked against.
            if !work.all
                && parent != ino
                && let Err(e) = self.ensure_children(parent)
            {
                warn!(%uid, error = ?e, "pin keeper: listing a kept file's folder failed");
                failed.nodes.insert(uid);
                continue;
            }
            if seen.insert(ino) {
                files.push(ino);
            }
        }

        let stale: Vec<KeptFile> = files
            .into_iter()
            .filter_map(|ino| self.kept_file(ino))
            .filter(|f| !self.cache.is_cached(&f.uid, f.mtime, f.size))
            .collect();
        if stale.is_empty() {
            return failed;
        }
        let job = self.transfers.begin_job("Updating files kept offline");
        job.set_total(stale.len() as u64);
        let (mut fetched, mut failures) = (0usize, 0usize);
        for file in stale {
            if self.shutdown.is_stopping() {
                break;
            }
            // Unpinned while the check ran.
            if !self.cache.is_pinned(&file.uid) {
                job.step();
                continue;
            }
            job.detail(file.name.clone());
            match self.fetch_pinned(&file.uid, &file.name, file.mtime, file.size) {
                Ok(()) => fetched += 1,
                Err(e) => {
                    warn!(uid = %file.uid, error = %e, "pin keeper: download failed");
                    failed.nodes.insert(file.uid);
                    failures += 1;
                }
            }
            job.step();
        }
        info!(
            fetched,
            failed = failures,
            "pin keeper: brought files kept offline up to date"
        );
        failed
    }

    /// The file at `ino`, as a pin keeps it. `None` for a folder, a file this
    /// daemon still owes an upload for, or a file whose real size cannot be
    /// read.
    ///
    /// A queued write is ahead of Drive. Downloading Drive's revision would
    /// cache what the write replaces, and the drain keeps the uploaded bytes
    /// of a pinned file once they land.
    ///
    /// A listing carries the ciphertext size until its size upgrade lands
    /// (B12). Stored under that size, the copy never matched the plaintext it
    /// holds, so every read downloaded the file again and every check
    /// downloaded it once more.
    pub(crate) fn kept_file(&self, ino: u64) -> Option<KeptFile> {
        let (uid, name, mtime, size, provisional) = {
            let st = self.state();
            let e = st.entries.get(&ino)?;
            let NodeKind::File { claimed_size, .. } = &e.node.kind else {
                return None;
            };
            (
                e.uid.clone(),
                e.node.name.clone(),
                e.node.modification_time,
                node_size(&e.node),
                claimed_size.is_none(),
            )
        };
        if is_local_uid(&uid) || self.pending.lock().contains_key(&uid) {
            return None;
        }
        let size = match provisional {
            true => self.resolve_provisional_size(ino, &uid)?,
            false => size,
        };
        Some(KeptFile {
            uid,
            name,
            mtime,
            size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_that_failed_keeps_every_node_and_the_full_check() {
        let mut backlog = PinWork::default();
        assert!(backlog.is_empty());
        let uid = parse_uid("vol~a").unwrap();
        backlog.merge(PinWork {
            all: false,
            nodes: HashSet::from([uid.clone()]),
        });
        backlog.merge(PinWork {
            all: true,
            nodes: HashSet::from([uid.clone()]),
        });
        assert!(backlog.all);
        assert_eq!(backlog.nodes.len(), 1);
        assert!(!backlog.is_empty());
    }
}
