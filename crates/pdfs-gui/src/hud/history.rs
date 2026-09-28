//! Recent files and frecency.
//!
//! The launcher remembers what it opened in `prompt-history.json` under the
//! state directory. With an empty query those files come first, and while
//! searching a file opened often and lately gets a bonus on top of its match
//! score (see [`frecency_bonus`]).
//!
//! One writer thread owns the file: it loads it, applies every recorded visit
//! in order and writes the result back atomically. A visit recorded before the
//! first load finished simply waits in the channel, so it is merged into the
//! loaded list rather than overwriting it. The main thread only ever sees
//! snapshots.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::glib;

use crate::Hit;

/// How many files the history keeps. More than the launcher ever shows, so
/// frecency still knows a file that dropped off the recent list.
const MAX_VISITS: usize = 50;

/// The frecency bonus of one visit made just now.
const VISIT_BONUS: f64 = 1500.0;

/// The bonus halves every this many days without a visit.
const HALF_LIFE_DAYS: f64 = 14.0;

/// The largest frecency bonus. A name match is worth up to 20000 points, so
/// history breaks ties between similar matches but cannot beat a much better
/// one.
const MAX_BONUS: f64 = 6000.0;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Visit {
    pub(crate) hit: Hit,
    /// When the file was last opened, in epoch seconds.
    pub(crate) last: i64,
    pub(crate) count: u32,
}

/// The history bonus for a file opened `count` times, the last time
/// `age_secs` ago.
pub(crate) fn frecency_bonus(count: u32, age_secs: i64) -> i64 {
    let age_days = age_secs.max(0) as f64 / 86_400.0;
    let bonus = f64::from(count) * VISIT_BONUS * 0.5f64.powf(age_days / HALF_LIFE_DAYS);
    bonus.min(MAX_BONUS) as i64
}

/// Count one more visit of `hit` at `now`, newest first, and keep at most
/// [`MAX_VISITS`]. The stored hit is replaced, so its size, date and sync
/// state are the ones from the latest search.
fn record(visits: &mut Vec<Visit>, hit: Hit, now: i64) {
    let key = hit.key();
    let count = match visits.iter().position(|visit| visit.hit.key() == key) {
        Some(at) => visits.remove(at).count.saturating_add(1),
        None => 1,
    };
    visits.insert(
        0,
        Visit {
            hit,
            last: now,
            count,
        },
    );
    visits.truncate(MAX_VISITS);
}

/// Drop local files that no longer exist. Drive files stay: checking them
/// would touch the mount, and a stale one only costs a row.
fn prune(visits: &mut Vec<Visit>) -> bool {
    let before = visits.len();
    visits.retain(|visit| match &visit.hit {
        Hit::Local(hit) => Path::new(&hit.path).symlink_metadata().is_ok(),
        Hit::Drive(_) => true,
    });
    visits.len() != before
}

fn load(path: &Path) -> Vec<Visit> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable {}: {e}", path.display());
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// Write the history through a temporary file and a rename, so a crash
/// mid-write leaves the old file rather than half a new one.
fn save(path: &Path, visits: &[Visit]) -> std::io::Result<()> {
    let json = serde_json::to_vec(visits).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

enum Command {
    Record(Hit, i64),
    Prune,
}

/// The writer thread's loop: load, then apply commands until the launcher
/// exits. `publish` receives the list after the load and after every change.
fn serve(path: &Path, commands: &async_channel::Receiver<Command>, publish: impl Fn(Vec<Visit>)) {
    let mut visits = load(path);
    let pruned = prune(&mut visits);
    publish(visits.clone());
    if pruned && let Err(e) = save(path, &visits) {
        tracing::warn!("cannot write {}: {e}", path.display());
    }
    while let Ok(command) = commands.recv_blocking() {
        let changed = match command {
            Command::Record(hit, now) => {
                record(&mut visits, hit, now);
                true
            }
            Command::Prune => prune(&mut visits),
        };
        if changed {
            if let Err(e) = save(path, &visits) {
                tracing::warn!("cannot write {}: {e}", path.display());
            }
            publish(visits.clone());
        }
    }
}

/// The main thread's view of the history.
pub(crate) struct History {
    visits: Rc<RefCell<Vec<Visit>>>,
    commands: async_channel::Sender<Command>,
}

impl History {
    /// Start the writer thread. `changed` runs on the main thread whenever a
    /// new snapshot arrives.
    pub(crate) fn start(path: PathBuf, changed: impl Fn() + 'static) -> Self {
        let (commands, inbox) = async_channel::unbounded();
        let (snapshots, updates) = async_channel::unbounded::<Vec<Visit>>();
        let spawned = std::thread::Builder::new()
            .name("prompt-history".into())
            .spawn(move || {
                serve(&path, &inbox, |visits| {
                    let _ = snapshots.send_blocking(visits);
                })
            });
        if let Err(e) = spawned {
            tracing::warn!("cannot start the history writer: {e}");
        }

        let visits = Rc::new(RefCell::new(Vec::new()));
        let shared = visits.clone();
        glib::spawn_future_local(async move {
            while let Ok(snapshot) = updates.recv().await {
                *shared.borrow_mut() = snapshot;
                changed();
            }
        });
        Self { visits, commands }
    }

    /// Remember that `hit` was opened just now.
    pub(crate) fn record(&self, hit: Hit) {
        let _ = self.commands.try_send(Command::Record(hit, now()));
    }

    /// Forget local files that were deleted since they were opened.
    pub(crate) fn prune(&self) {
        let _ = self.commands.try_send(Command::Prune);
    }

    /// The most recently opened files, newest first.
    pub(crate) fn recent(&self, limit: usize) -> Vec<Hit> {
        self.visits
            .borrow()
            .iter()
            .take(limit)
            .map(|visit| visit.hit.clone())
            .collect()
    }

    /// The frecency bonus of every remembered file, by [`Hit::key`].
    pub(crate) fn bonuses(&self) -> HashMap<(bool, String), i64> {
        let now = now();
        self.visits
            .borrow()
            .iter()
            .map(|visit| {
                (
                    visit.hit.key(),
                    frecency_bonus(visit.count, now - visit.last),
                )
            })
            .collect()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfs_core::control::{LocalHit, SearchHit};

    const DAY: i64 = 86_400;

    fn drive(name: &str) -> Hit {
        Hit::Drive(SearchHit {
            name: name.into(),
            path: format!("Docs/{name}"),
            is_dir: false,
            size: 1,
            modified: 1,
            pinned: false,
            cached: false,
            uid: format!("v~{name}"),
            mounted_path: None,
            score: 0,
        })
    }

    fn scratch_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pdfs-prompt-history-{test}-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_bonus_halves_every_two_weeks() {
        assert_eq!(frecency_bonus(1, 0), 1500);
        assert_eq!(frecency_bonus(1, 14 * DAY), 750);
        assert_eq!(frecency_bonus(1, 28 * DAY), 375);
        assert_eq!(frecency_bonus(2, 14 * DAY), 1500);
        // A clock that went backwards counts as "just now", not as a boost.
        assert_eq!(frecency_bonus(1, -DAY), 1500);
    }

    #[test]
    fn the_bonus_is_capped() {
        assert_eq!(frecency_bonus(4, 0), 6000);
        assert_eq!(frecency_bonus(1000, 0), 6000);
        assert!(frecency_bonus(1000, 365 * DAY) < 6000);
    }

    #[test]
    fn a_repeat_visit_moves_the_file_to_the_top_and_counts_it() {
        let mut visits = Vec::new();
        record(&mut visits, drive("a.pdf"), 10);
        record(&mut visits, drive("b.pdf"), 20);
        record(&mut visits, drive("a.pdf"), 30);
        assert_eq!(visits.len(), 2);
        assert_eq!(visits[0].hit.name(), "a.pdf");
        assert_eq!((visits[0].count, visits[0].last), (2, 30));
        assert_eq!(visits[1].count, 1);
    }

    #[test]
    fn the_history_keeps_the_newest_fifty_files() {
        let mut visits = Vec::new();
        for n in 0..60 {
            record(&mut visits, drive(&format!("{n}.pdf")), n);
        }
        assert_eq!(visits.len(), MAX_VISITS);
        assert_eq!(visits[0].hit.name(), "59.pdf");
        assert_eq!(visits[MAX_VISITS - 1].hit.name(), "10.pdf");
    }

    #[test]
    fn the_history_file_round_trips() {
        let dir = scratch_dir("round-trip");
        let path = dir.join("prompt-history.json");
        let mut visits = Vec::new();
        record(&mut visits, drive("a.pdf"), 10);
        record(&mut visits, drive("a.pdf"), 20);
        save(&path, &visits).unwrap();

        let loaded = load(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].hit.key(), drive("a.pdf").key());
        assert_eq!((loaded[0].count, loaded[0].last), (2, 20));
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn visits_recorded_before_the_load_are_merged_into_it() {
        let dir = scratch_dir("merge");
        let path = dir.join("prompt-history.json");
        let mut stored = Vec::new();
        record(&mut stored, drive("old.pdf"), 10);
        record(&mut stored, drive("old.pdf"), 20);
        save(&path, &stored).unwrap();

        // Both visits are queued before the writer has read the file.
        let (commands, inbox) = async_channel::unbounded();
        commands
            .send_blocking(Command::Record(drive("old.pdf"), 30))
            .unwrap();
        commands
            .send_blocking(Command::Record(drive("new.pdf"), 40))
            .unwrap();
        drop(commands);

        let published = RefCell::new(Vec::new());
        serve(&path, &inbox, |visits| published.borrow_mut().push(visits));

        let last = published.borrow().last().cloned().unwrap();
        assert_eq!(last.len(), 2);
        assert_eq!(last[0].hit.name(), "new.pdf");
        assert_eq!((last[1].hit.name(), last[1].count), ("old.pdf", 3));
        let on_disk = load(&path);
        assert_eq!(on_disk.len(), 2);
        assert_eq!(on_disk[1].count, 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pruning_drops_deleted_local_files_only() {
        let dir = scratch_dir("prune");
        let kept = dir.join("kept.txt");
        std::fs::write(&kept, "x").unwrap();
        let local = |path: &Path| {
            Hit::Local(LocalHit {
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                path: path.display().to_string(),
                is_dir: false,
                size: 1,
                modified: 1,
                score: 0,
            })
        };
        let mut visits = Vec::new();
        record(&mut visits, local(&kept), 1);
        record(&mut visits, local(&dir.join("gone.txt")), 2);
        record(&mut visits, drive("remote.pdf"), 3);

        assert!(prune(&mut visits));
        let names: Vec<&str> = visits.iter().map(|visit| visit.hit.name()).collect();
        assert_eq!(names, ["remote.pdf", "kept.txt"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
