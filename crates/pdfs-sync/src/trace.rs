//! Where a reconcile pass spends its time.
//!
//! A pass over a large folder used to log that it started and, hours later,
//! that a stop cut it short. Nothing said whether the hours went into the local
//! walk, listing Drive, unlocking node keys or applying (`docs/BUGS.md` B294).
//! A [`PassTrace`] times each stage and each kind of call to Drive, counts what
//! each stage handled, and ends up in the journal and in `pdfs diagnostics`.

use std::fmt;

use pdfs_core::control::{SyncPassTrace, SyncTraceCount, SyncTraceStat};

use super::*;

/// A stage of a pass. A pass runs them in this order, except that planning
/// and applying alternate, one folder depth at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Waiting for the folder's lock, held by a mode switch or a relocation.
    LockWait,
    /// Reading the stored baseline from the database.
    Baseline,
    LocalWalk,
    RemoteWalk,
    /// Deciding what to do with each path, and the inline work that needs no
    /// network (local folders, baseline rows, local deletions).
    Plan,
    /// Running the queued uploads, downloads and folder creations.
    Apply,
    /// Removing the folders deleted on either side, deepest first.
    Cleanup,
}

impl Phase {
    const ALL: [Phase; 7] = [
        Phase::LockWait,
        Phase::Baseline,
        Phase::LocalWalk,
        Phase::RemoteWalk,
        Phase::Plan,
        Phase::Apply,
        Phase::Cleanup,
    ];

    fn name(self) -> &'static str {
        match self {
            Phase::LockWait => "lock wait",
            Phase::Baseline => "baseline",
            Phase::LocalWalk => "local walk",
            Phase::RemoteWalk => "remote walk",
            Phase::Plan => "plan",
            Phase::Apply => "apply",
            Phase::Cleanup => "cleanup",
        }
    }
}

/// A kind of call to Drive, or of work applied for one path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Call {
    /// Waiting for the daemon's admission before an upload, download or
    /// folder creation could start. Time here is time in line, not on the
    /// network.
    Admission,
    /// The child uids of one folder.
    ListChildren,
    /// Names and kinds of one folder's children, without unlocking file keys.
    ReadNodes,
    /// Full reads of the files whose size had to be read for real: one node
    /// key unlock each.
    ReadNodesFull,
    CreateFolder,
    Upload,
    UploadRevision,
    Download,
    /// Hashing a local file to match it up with Drive's copy on a first sync.
    Adopt,
    Conflict,
    Trash,
}

impl Call {
    const ALL: [Call; 11] = [
        Call::Admission,
        Call::ListChildren,
        Call::ReadNodes,
        Call::ReadNodesFull,
        Call::CreateFolder,
        Call::Upload,
        Call::UploadRevision,
        Call::Download,
        Call::Adopt,
        Call::Conflict,
        Call::Trash,
    ];

    fn name(self) -> &'static str {
        match self {
            Call::Admission => "admission wait",
            Call::ListChildren => "list children",
            Call::ReadNodes => "read nodes",
            Call::ReadNodesFull => "read nodes full",
            Call::CreateFolder => "create folder",
            Call::Upload => "upload",
            Call::UploadRevision => "upload revision",
            Call::Download => "download",
            Call::Adopt => "adopt",
            Call::Conflict => "conflict",
            Call::Trash => "trash",
        }
    }
}

/// Something a pass counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Count {
    /// Files and folders the local walk found.
    LocalEntries,
    /// Folders on Drive the remote walk listed.
    RemoteFolders,
    /// Nodes read without unlocking their key.
    Nodes,
    /// Nodes read in full, one key unlock each.
    NodesFull,
    /// Baseline rows given the revision id they were missing.
    Revisions,
    /// Paths queued for the apply stage.
    Queued,
    /// Paths on Drive read back from a walk an earlier pass did not finish.
    Resumed,
    /// Paths on Drive read back from the last complete walk, which the event
    /// stream keeps up to date.
    Reused,
    /// Folders on Drive marked as changed since the last pass listed them.
    Dirty,
    /// Deletions the pass planned that the evidence does not back, and so
    /// did not make: the path was not proven gone on the side the deletion is
    /// based on. Should stay zero; see `evidence`.
    UnprovenDeletions,
}

impl Count {
    const ALL: [Count; 10] = [
        Count::LocalEntries,
        Count::RemoteFolders,
        Count::Nodes,
        Count::NodesFull,
        Count::Revisions,
        Count::Queued,
        Count::Resumed,
        Count::Reused,
        Count::Dirty,
        Count::UnprovenDeletions,
    ];

    fn name(self) -> &'static str {
        match self {
            Count::LocalEntries => "local entries",
            Count::RemoteFolders => "remote folders",
            Count::Nodes => "nodes",
            Count::NodesFull => "nodes full",
            Count::Revisions => "revisions recorded",
            Count::Queued => "queued",
            Count::Resumed => "resumed",
            Count::Reused => "reused",
            Count::Dirty => "dirty folders",
            Count::UnprovenDeletions => "unproven deletions",
        }
    }
}

/// How often one thing happened, and how long it took.
#[derive(Clone, Copy, Default, Debug)]
struct Stat {
    count: u64,
    total: Duration,
    max: Duration,
}

impl Stat {
    fn add(&mut self, took: Duration) {
        self.count += 1;
        self.total += took;
        self.max = self.max.max(took);
    }

    fn summary(self, name: &str) -> SyncTraceStat {
        SyncTraceStat {
            name: name.to_string(),
            count: self.count,
            total_ms: millis(self.total),
            max_ms: millis(self.max),
        }
    }
}

/// The timings and counts of one pass.
#[derive(Clone, Debug)]
pub struct PassTrace {
    folder_id: i64,
    started: Instant,
    /// How the pass ended and how long it ran; `None` while it runs.
    ended: Option<(&'static str, Duration)>,
    phases: [Stat; Phase::ALL.len()],
    calls: [Stat; Call::ALL.len()],
    counts: [u64; Count::ALL.len()],
    slowest_listing: Option<(String, Duration)>,
}

impl PassTrace {
    pub fn new(folder_id: i64) -> Self {
        Self {
            folder_id,
            started: Instant::now(),
            ended: None,
            phases: [Stat::default(); Phase::ALL.len()],
            calls: [Stat::default(); Call::ALL.len()],
            counts: [0; Count::ALL.len()],
            slowest_listing: None,
        }
    }

    pub fn folder_id(&self) -> i64 {
        self.folder_id
    }

    pub fn is_ended(&self) -> bool {
        self.ended.is_some()
    }

    pub fn phase(&mut self, phase: Phase, took: Duration) {
        self.phases[phase as usize].add(took);
    }

    pub fn call(&mut self, call: Call, took: Duration) {
        self.calls[call as usize].add(took);
    }

    pub fn count(&mut self, count: Count, n: usize) {
        self.counts[count as usize] += n as u64;
    }

    /// Note how long listing the Drive folder at `rel` took, keeping the slowest.
    pub fn listing(&mut self, rel: &str, took: Duration) {
        if self
            .slowest_listing
            .as_ref()
            .is_none_or(|(_, max)| took > *max)
        {
            self.slowest_listing = Some((rel.to_string(), took));
        }
    }

    pub fn end(&mut self, how: &'static str) {
        if self.ended.is_none() {
            self.ended = Some((how, self.started.elapsed()));
        }
    }

    fn elapsed(&self) -> Duration {
        self.ended
            .map_or_else(|| self.started.elapsed(), |(_, took)| took)
    }

    /// The trace as `pdfs diagnostics` reports it. Stages and calls that
    /// never happened are left out.
    pub fn summary(&self) -> SyncPassTrace {
        SyncPassTrace {
            folder_id: self.folder_id,
            ended: self
                .ended
                .map(|(how, _)| how.to_string())
                .unwrap_or_default(),
            elapsed_ms: millis(self.elapsed()),
            phases: Phase::ALL
                .iter()
                .filter(|p| self.phases[**p as usize].count > 0)
                .map(|p| self.phases[*p as usize].summary(p.name()))
                .collect(),
            calls: Call::ALL
                .iter()
                .filter(|c| self.calls[**c as usize].count > 0)
                .map(|c| self.calls[*c as usize].summary(c.name()))
                .collect(),
            counts: Count::ALL
                .iter()
                .filter(|c| self.counts[**c as usize] > 0)
                .map(|c| SyncTraceCount {
                    name: c.name().to_string(),
                    value: self.counts[*c as usize],
                })
                .collect(),
            slowest_listing: self
                .slowest_listing
                .as_ref()
                .map(|(rel, took)| (rel.clone(), millis(*took))),
        }
    }
}

/// One line for the journal; see [`SyncPassTrace`]'s `Display`.
impl fmt::Display for PassTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.summary().fmt(f)
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_reports_only_what_happened() {
        let mut trace = PassTrace::new(3);
        trace.phase(Phase::RemoteWalk, Duration::from_secs(7800));
        trace.call(Call::ReadNodesFull, Duration::from_secs(2));
        trace.call(Call::ReadNodesFull, Duration::from_secs(3));
        trace.count(Count::NodesFull, 480);
        trace.listing("a", Duration::from_secs(1));
        trace.listing("a/b", Duration::from_secs(9));
        trace.listing("c", Duration::from_secs(4));
        trace.end("cut short");
        trace.end("finished");

        let summary = trace.summary();
        assert_eq!(summary.folder_id, 3);
        assert_eq!(summary.ended, "cut short");
        assert_eq!(summary.phases.len(), 1);
        assert_eq!(summary.phases[0].name, "remote walk");
        assert_eq!(
            summary.calls,
            vec![SyncTraceStat {
                name: "read nodes full".into(),
                count: 2,
                total_ms: 5000,
                max_ms: 3000,
            }]
        );
        assert_eq!(summary.counts.len(), 1);
        assert_eq!(summary.counts[0].value, 480);
        assert_eq!(summary.slowest_listing, Some(("a/b".into(), 9000)));
        assert_eq!(
            summary.to_string(),
            "remote walk 2h10m; read nodes full 2x/5.0s (max 3.0s); nodes full 480; \
             slowest listing \"a/b\" 9.0s"
        );
    }
}
