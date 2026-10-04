//! The simulation runs of `docs/MILESTONE-3.0.0.md` §8.2.
//!
//! A run starts one or more daemons on a shared [`FakeDrive`], each owning a
//! folder `c<n>` that only it writes, so there is one writer per file. A seed
//! draws a sequence of steps: syscalls through a client's mount, the link of a
//! client going down or up, a client restarting, a pause, a settle (every
//! queue must drain within a budget), or a file deleted while the drain holds
//! a write to it. Every syscall is checked against the
//! client's [`Model`] as it happens. After the sequence every link comes up
//! and the run checks:
//!
//! 1. **No loss** and **POSIX**: Drive's copy of each client's folder is what
//!    its model says.
//! 2. **No false conflicts**: no conflict copy anywhere, and the drain never
//!    takes a client's own delete for another device's.
//! 3. **Convergence**: each mount shows Drive's tree.
//! 4. **Liveness**: no syscall took longer than its budget, settles drained in
//!    theirs, and every queue drained once the links were up. A call that
//!    never returns is caught by the [`Watchdog`], which keeps the stacks.
//!
//! A restart here is a stop and a start: the next run finds what the last
//! one wrote down, but in-flight work is let finish rather than cut. A kill
//! at an arbitrary point needs the daemon in a process of its own.
//!
//! A call that answers otherwise than the model says fails the run, unless it
//! is one of the [`KNOWN`] open bugs. Those are counted instead and the model
//! takes the call back, or the seed ends there when the bug leaves the mount
//! where the model cannot follow, so the runs stay a regression net until
//! they are fixed. A file gone, or a conflict copy the drain made of one of
//! the client's own, is counted too when a known bug explains it
//! (`Run::known_damage`), and so is a hang the stacks show is one of the
//! [`KNOWN_HANGS`]. `PDFS_SIM_KNOWN=fail` fails on all of them.
//!
//! The runs are ignored by default because they mount FUSE; run them with
//! `cargo test -p pdfs-fuse --lib sim::run -- --ignored --test-threads=1`.
//! `PDFS_SIM_SEEDS=n` sets how many seeds each runs, `PDFS_SIM_SEED=s` replays
//! one, and `RUST_LOG=pdfs_fuse=debug` with `--nocapture` shows the daemons'
//! logs.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use proton_drive_rs::proton_sdk::ids::NodeUid;

use super::daemon::{Daemon, scratch, take_logged, wait_until};
use super::fake_drive::{Entry, FakeDrive, Faults};
use super::model::{FsOp, Model, Tree, data, join, name_of, parent_of};
use super::rng::Rng;
use super::watchdog::{Hung, Watchdog};

/// An open bug in `docs/BUGS.md` the runs tolerate.
struct Known {
    bug: &'static str,
    /// Whether a call that answered `errno` against the model's word, with
    /// lines `logged` at `INFO` and above meanwhile and `before` it, is this
    /// bug. `model` is from before the call.
    is: fn(model: &Model, op: &FsOp, errno: i32, logged: &[String], before: &[String]) -> bool,
    /// Whether it also left the mount where the model cannot follow, so the
    /// seed ends there.
    ends_seed: fn(model: &Model, op: &FsOp) -> bool,
}

const KNOWN: &[Known] = &[
    // A replacing rename trashes what it replaces first, so Drive refuses the
    // name only when it holds a node the mount has forgotten: the create of
    // the replaced file landed after its op was dropped.
    Known {
        bug: "B129",
        is: |model, op, errno, logged, _| {
            let FsOp::Rename { to, .. } = op else {
                return false;
            };
            errno == libc::EIO
                && model.tree().contains_key(to)
                && logged
                    .iter()
                    .any(|line| line.starts_with("rename failed") && line.contains("AlreadyExists"))
        },
        // The model made the rename, and Drive holds a node the mount does
        // not know.
        ends_seed: |_, _| true,
    },
    // A folder made offline is listed on Drive once its listing has been
    // invalidated, and Drive does not know it, or does not list it yet. Only
    // the client writes its folder, so a folder of its own is never really
    // gone.
    Known {
        bug: "B125",
        is: |_, _, errno, logged, _| {
            matches!(errno, libc::EIO | libc::ENOENT)
                && logged.iter().any(|line| {
                    line.starts_with("enumerate folder children failed")
                        && line.contains("DoesNotExist")
                })
        },
        // The folder keeps failing to list, and what is in it can come back
        // as gone, so the mount no longer follows the model.
        ends_seed: |_, _| true,
    },
    // A restart forgets the listings served offline, so the first lookup
    // lists a folder on Drive, which does not hold what is still queued for
    // it.
    Known {
        bug: "B132",
        is: |_, _, errno, _, before| {
            errno == libc::ENOENT
                && before
                    .iter()
                    .any(|line| line.starts_with("lost the connection to Proton"))
                && before
                    .iter()
                    .any(|line| line.starts_with("restored pending ops"))
        },
        // The listing stays without it until something relists the folder.
        ends_seed: |_, _| true,
    },
];

/// An open bug in `docs/BUGS.md` that hangs a call, which the runs tolerate.
/// The seed ends there, since the watchdog aborted the mounts.
struct KnownHang {
    bug: &'static str,
    /// Whether the stacks the watchdog kept show this bug.
    is: fn(stacks: &str) -> bool,
}

const KNOWN_HANGS: &[KnownHang] = &[
    // A runtime worker invalidating a file's pages waits for the page lock
    // of a read, whose fetch waits for a timer that no worker drives.
    KnownHang {
        bug: "B130",
        is: |stacks| {
            stacks.split("\nthread ").any(|thread| {
                thread.contains("\"tokio-rt-worker\", state D")
                    && thread.contains("NotifyBatch>::flush")
            })
        },
    },
];

/// What a run looks like.
pub(crate) struct Profile {
    pub(crate) name: &'static str,
    pub(crate) clients: usize,
    pub(crate) steps: usize,
    pub(crate) faults: fn() -> Faults,
    /// Whether links go down and come back during the sequence.
    pub(crate) link_flaps: bool,
    pub(crate) restarts: bool,
    /// Whether files are deleted while the drain holds a write to them.
    pub(crate) deletes_mid_upload: bool,
    /// How long a settle may take; `None` leaves settles out.
    pub(crate) settle_budget: Option<Duration>,
    /// How long one syscall may take.
    pub(crate) syscall_budget: Duration,
    /// How long the queues may take to drain at the end.
    pub(crate) drain_budget: Duration,
}

/// How long the mounts may take to show Drive's tree at the end: one event
/// poll (`POLL_INTERVAL`) and change.
const CONVERGE_BUDGET: Duration = Duration::from_secs(30);

/// How long the drain may take to pick up a write: its debounce, and then
/// some.
const PICKUP_BUDGET: Duration = Duration::from_secs(8);

/// Steps shown before a failure. All of them are written to `steps.log` in
/// the state the failure keeps.
const LOG_TAIL: usize = 40;

/// How long past its own budget a step, or the checks after the last one, may
/// run before the watchdog takes it for hung. Longer than a restart may wait
/// for the last run and the mount.
const STALL: Duration = Duration::from_secs(120);

/// What a known bug left on Drive in place of a file, and how the model
/// takes it in.
struct KnownDamage {
    bug: &'static str,
    path: String,
    /// The conflict copy it made, if it made one.
    copy: Option<String>,
    change: FsOp,
}

struct Client {
    daemon: Option<Daemon>,
    model: Model,
    online: bool,
}

impl Client {
    fn daemon(&self) -> &Daemon {
        self.daemon
            .as_ref()
            .expect("a client always has a daemon between steps")
    }

    fn root(&self, index: usize) -> PathBuf {
        self.daemon().mountpoint.join(owned_folder(index))
    }
}

struct Run {
    seed: u64,
    profile: &'static Profile,
    drive: std::sync::Arc<FakeDrive>,
    clients: Vec<Client>,
    log: Vec<String>,
    started: Instant,
    /// How often each [`KNOWN`] bug turned up.
    known: BTreeMap<&'static str, usize>,
    /// The known bug that ended the seed early, if one did.
    ended_by: Option<&'static str>,
    /// What the daemons logged at `INFO` and above so far.
    logged: Vec<String>,
    /// Files deleted while the drain held a write to them.
    held: Vec<NodeUid>,
    /// Those of them made again before it went on.
    remade: Vec<NodeUid>,
    watchdog: Watchdog,
}

/// Run `profile` on `seed`. It answers how often each known bug turned up,
/// or a report of what went wrong.
pub(crate) fn run(
    seed: u64,
    profile: &'static Profile,
) -> Result<BTreeMap<&'static str, usize>, String> {
    let dir = scratch(&format!("{}-{seed}", profile.name));
    let mut rng = Rng::new(seed);
    let drive = FakeDrive::new();
    let mut run = Run {
        seed,
        profile,
        drive: drive.clone(),
        clients: Vec::new(),
        log: Vec::new(),
        started: Instant::now(),
        known: BTreeMap::new(),
        ended_by: None,
        logged: Vec::new(),
        held: Vec::new(),
        remade: Vec::new(),
        watchdog: Watchdog::new(&dir),
    };
    let outcome = run.execute(&dir, &mut rng);
    run.watchdog.stop();
    // A hang is what failed, whatever the aborted call answered.
    let outcome = match run.watchdog.fired() {
        Some(hung) => run.known_hang(&hung),
        None => outcome,
    };
    for client in &mut run.clients {
        if let Some(daemon) = client.daemon.take() {
            let _ = daemon.stop();
        }
    }
    match outcome {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&dir);
            Ok(run.known)
        }
        Err(failure) => Err(run.report(&failure, &dir)),
    }
}

impl Run {
    /// Count `hung` if it is one of the [`KNOWN_HANGS`], or fail on it.
    fn known_hang(&mut self, hung: &Hung) -> Result<(), String> {
        let known = KNOWN_HANGS.iter().find(|known| (known.is)(&hung.stacks));
        match known {
            Some(known) if tolerate_known() => {
                *self.known.entry(known.bug).or_default() += 1;
                self.note(format!("known bug {}; the seed ends here", known.bug));
                self.ended_by = Some(known.bug);
                Ok(())
            }
            _ => Err(format!("liveness: {}", hung.what)),
        }
    }

    fn execute(&mut self, dir: &Path, rng: &mut Rng) -> Result<(), String> {
        for index in 0..self.profile.clients {
            let client = self.drive.client(rng.next_u64(), (self.profile.faults)());
            let daemon = Daemon::start(&dir.join(format!("client{index}")), client)
                .map_err(|error| format!("client {index} did not mount: {error}"))?;
            let folder = daemon.mountpoint.join(owned_folder(index));
            std::fs::create_dir(&folder)
                .map_err(|error| format!("client {index}: mkdir {}: {error}", folder.display()))?;
            self.clients.push(Client {
                daemon: Some(daemon),
                model: Model::default(),
                online: true,
            });
        }
        for step in 0..self.profile.steps {
            let _watching = self.watchdog.arm(self.step_limit(), format!("step {step}"));
            self.step(rng)?;
            if self.ended_by.is_some() {
                return Ok(());
            }
        }
        let _watching = self.watchdog.arm(
            self.profile.drain_budget + STALL,
            "the checks after the last step",
        );
        self.finish()
    }

    /// How long a step may run before the watchdog takes it for hung.
    fn step_limit(&self) -> Duration {
        let budget = self.profile.settle_budget.unwrap_or_default();
        budget.max(self.profile.syscall_budget) + STALL
    }

    fn step(&mut self, rng: &mut Rng) -> Result<(), String> {
        let index = rng.below(self.clients.len() as u64) as usize;
        let roll = rng.below(100);
        match roll {
            0..4 if self.profile.link_flaps => {
                let client = &mut self.clients[index];
                client.online = !client.online;
                client.daemon().client.set_online(client.online);
                let state = if client.online { "up" } else { "down" };
                self.note(format!("client {index}: link {state}"));
            }
            4..6 if self.profile.restarts => {
                self.note(format!("client {index}: restart"));
                let daemon = self.clients[index].daemon.take().expect("daemon");
                let daemon = daemon
                    .restart()
                    .map_err(|error| format!("client {index} did not come back: {error}"))?;
                self.clients[index].daemon = Some(daemon);
            }
            6..12 => {
                let ms = rng.below(400);
                self.note(format!("pause {ms} ms"));
                std::thread::sleep(Duration::from_millis(ms));
            }
            12..15 => {
                if let Some(budget) = self.profile.settle_budget
                    && self.clients.iter().all(|client| client.online)
                {
                    self.settle(budget)?;
                }
            }
            15..25 if self.profile.deletes_mid_upload => self.delete_mid_upload(index, rng)?,
            _ => {
                let op = self.clients[index].model.next_op(rng);
                self.syscall(index, op)?;
            }
        }
        Ok(())
    }

    /// Wait until every queue is empty and Drive holds what the models say.
    fn settle(&mut self, budget: Duration) -> Result<(), String> {
        self.note("settle".into());
        let start = Instant::now();
        if !wait_until(budget, || self.landed()) {
            return Err(format!(
                "liveness: the queues did not drain within {budget:?}\n{}",
                self.difference()
            ));
        }
        self.note(format!("settled in {:?}", start.elapsed()));
        Ok(())
    }

    /// Write a file Drive holds and delete it once the drain has picked the
    /// write up. The drain's read of the node is held until the delete has
    /// landed and a read shows it, so the drain finds the file trashed while
    /// it holds a write that is no longer queued (B105). Half the time the
    /// file is made again meanwhile, which queues a write under the op id the
    /// delete freed (B133).
    fn delete_mid_upload(&mut self, index: usize, rng: &mut Rng) -> Result<(), String> {
        let file = rng
            .pick(&self.clients[index].model.files())
            .map(|path| (*path).clone());
        let (Some(path), Some(budget), true) = (
            file,
            self.profile.settle_budget,
            self.clients.iter().all(|client| client.online),
        ) else {
            let op = self.clients[index].model.next_op(rng);
            return self.syscall(index, op);
        };
        self.settle(budget)?;
        let Some(uid) = self.drive.lookup(&join(&owned_folder(index), &path)) else {
            return Err(format!(
                "no loss: client {index}'s {path} is not on Drive after a settle"
            ));
        };
        let held = self.clients[index].daemon().client.hold_next_read(&uid);
        self.note(format!("client {index}: hold the next read of {path}"));
        let mut bytes = data(rng);
        bytes.push(b'.');
        self.syscall(
            index,
            FsOp::Write {
                path: path.clone(),
                offset: 0,
                data: bytes,
            },
        )?;
        if self.ended_by.is_some() {
            return Ok(());
        }
        if !wait_until(PICKUP_BUDGET, || held.reached()) {
            self.note(format!("client {index}: nothing read {path} back"));
            return Ok(());
        }
        self.syscall(index, FsOp::Unlink { path: path.clone() })?;
        if self.ended_by.is_some() {
            return Ok(());
        }
        self.held.push(uid.clone());
        if rng.chance(0.5) {
            self.syscall(
                index,
                FsOp::Create {
                    path: path.clone(),
                    data: data(rng),
                },
            )?;
            self.remade.push(uid);
        }
        let faults = (self.profile.faults)();
        let latency = Duration::from_millis(faults.latency_ms.1);
        std::thread::sleep(faults.listing_lag + latency);
        drop(held);
        self.note(format!("client {index}: the read of {path} goes on"));
        // Nothing else is queued before the drain has answered.
        std::thread::sleep(2 * latency + Duration::from_millis(50));
        Ok(())
    }

    fn syscall(&mut self, index: usize, op: FsOp) -> Result<(), String> {
        let client = &mut self.clients[index];
        let root = client.root(index);
        let model = client.model.clone();
        let before = client.model.content(target(&op)).cloned();
        let expected = client.model.apply(&op);
        self.logged.extend(take_logged());
        let logged_before = self.logged.len();
        let start = Instant::now();
        let actual = perform(&root, &op, before.as_deref());
        let took = start.elapsed();
        self.note(format!(
            "client {index}: {} = {} in {took:?}",
            op.describe(),
            outcome(&actual)
        ));
        let actual = actual?;
        let logged = take_logged();
        self.logged.extend(logged.iter().cloned());
        if actual != expected
            && let Err(errno) = actual
            && tolerate_known()
            && let Some(known) = KNOWN.iter().find(|known| {
                (known.is)(&model, &op, errno, &logged, &self.logged[..logged_before])
            })
        {
            *self.known.entry(known.bug).or_default() += 1;
            if (known.ends_seed)(&model, &op) {
                self.note(format!("known bug {}; the seed ends here", known.bug));
                self.ended_by = Some(known.bug);
            } else {
                self.note(format!("known bug {}; taken back", known.bug));
            }
            self.clients[index].model = model;
            return Ok(());
        }
        if actual != expected {
            return Err(format!(
                "posix: client {index}: {} answered {}, expected {}",
                op.describe(),
                errno(actual),
                errno(expected)
            ));
        }
        if took > self.profile.syscall_budget {
            return Err(format!(
                "liveness: client {index}: {} took {took:?}, over {:?}",
                op.describe(),
                self.profile.syscall_budget
            ));
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), String> {
        for client in &mut self.clients {
            client.online = true;
            client.daemon().client.set_online(true);
        }
        self.note("every link up; waiting for the queues".into());
        if !wait_until(self.profile.drain_budget, || {
            self.logged.extend(take_logged());
            self.landed()
        }) {
            return Err(format!(
                "no loss: Drive does not hold what the clients wrote, {:?} after the links came up\n{}",
                self.profile.drain_budget,
                self.difference()
            ));
        }
        // Each client's folder is its own, so a file of it gone from Drive was
        // deleted by the client.
        let own_deletes: Vec<String> = self
            .logged
            .iter()
            .filter(|line| is_own_delete_kept(line))
            .cloned()
            .collect();
        for line in own_deletes {
            let bug = if self
                .remade
                .iter()
                .any(|uid| line.contains(&format!(" uid={uid} ")))
            {
                // Made again while the drain held its write: the new file's
                // write took the freed op id.
                "B133"
            } else if !self
                .held
                .iter()
                .any(|uid| line.contains(&format!(" uid={uid} ")))
                && !line.contains(" name=\"recovered-")
            {
                // The tree still placed the file, so the drain looked before
                // the unlink had dropped its write.
                "B134"
            } else {
                return Err(format!(
                    "no false conflicts: the drain took a client's own delete for another device's: {line}"
                ));
            };
            if !tolerate_known() {
                return Err(format!(
                    "no false conflicts: the drain took a client's own delete for another device's: {line}"
                ));
            }
            *self.known.entry(bug).or_default() += 1;
            self.note(format!("known bug {bug}: {line}"));
        }
        let drive = self.drive_tree();
        let mut explained = Vec::new();
        for index in 0..self.clients.len() {
            for damage in self.known_damage(index, &drive) {
                *self.known.entry(damage.bug).or_default() += 1;
                self.note(format!(
                    "known bug {}: client {index}'s {} landed as {:?}",
                    damage.bug, damage.path, damage.copy
                ));
                let _ = self.clients[index].model.apply(&damage.change);
                if let Some(copy) = damage.copy {
                    explained.push(join(&owned_folder(index), &copy));
                }
            }
        }
        let conflicts: Vec<String> = drive
            .into_keys()
            .filter(|path| is_conflict_copy(path) && !explained.contains(path))
            .collect();
        if !conflicts.is_empty() {
            return Err(format!("no false conflicts: {conflicts:?}"));
        }
        for index in 0..self.clients.len() {
            let mut last = Tree::new();
            let converged = wait_until(CONVERGE_BUDGET, || {
                last = read_tree(&self.clients[index].daemon().mountpoint);
                last == self.drive_tree()
            });
            if !converged {
                return Err(format!(
                    "convergence: client {index}'s mount differs from Drive\n{}",
                    diff(&last, &self.drive_tree(), "mount", "Drive")
                ));
            }
        }
        Ok(())
    }

    /// Whether every client's queue is empty and Drive holds what its model
    /// says, but for what known bugs explain.
    fn landed(&self) -> bool {
        let drive = self.drive_tree();
        self.clients.iter().enumerate().all(|(index, client)| {
            let mut model = client.model.clone();
            for damage in self.known_damage(index, &drive) {
                let _ = model.apply(&damage.change);
            }
            client
                .daemon()
                .pending()
                .is_ok_and(|items| items.is_empty())
                && subtree(&drive, &owned_folder(index)) == *model.tree()
        })
    }

    /// The files of client `index` that a known bug left otherwise on Drive
    /// than the model says. A conflict copy takes Drive holding it with the
    /// model's bytes, and the drain saying it made that copy for that name:
    ///
    /// - B126: the file's queued create ran into its name while a queued
    ///   rename was about to free it, so it is there only as the copy.
    /// - B127: the file's queued create landed but its answer was lost, so
    ///   the retry found it and made the copy next to it.
    ///
    /// A lost file takes the drain saying it adopted a node for that name
    /// whose queued trash then landed:
    ///
    /// - B128: the file's queued create ran into the name of the file it
    ///   replaced and took that over, trash and all.
    ///
    /// A conflict copy of a file the client deleted takes the drain saying it
    /// kept that file's write as the copy, because Drive had the file in the
    /// trash while the tree still placed it:
    ///
    /// - B134: the unlink trashed the file on Drive before it dropped the
    ///   write.
    fn known_damage(&self, index: usize, drive: &Tree) -> Vec<KnownDamage> {
        if !tolerate_known() {
            return Vec::new();
        }
        let drive = subtree(drive, &owned_folder(index));
        let model = self.clients[index].model.tree();
        let made_copy = |path: &str, copy: &str| {
            let wanted = format!("wanted={:?}", name_of(path));
            let name = format!("name={:?}", name_of(copy));
            self.logged.iter().any(|line| {
                line.starts_with("name is taken remotely; creating under a conflict name")
                    && line.contains(&wanted)
                    && line.contains(&name)
            })
        };
        let adopted_trashed = |path: &str| {
            let wanted = format!("wanted={:?}", name_of(path));
            self.logged.iter().any(|line| {
                line.starts_with("the name is held by our own unanswered create; adopting it")
                    && line.contains(&wanted)
                    && line.split(' ').any(|field| {
                        field.strip_prefix("twin=").is_some_and(|twin| {
                            let uid = format!("uid={twin}");
                            self.logged.iter().any(|line| {
                                line.starts_with("pending trash landed")
                                    && line.split(' ').any(|field| field == uid)
                            })
                        })
                    })
            })
        };
        let kept_deleted = |copy: &str| {
            let alt = format!(" alt={:?}", name_of(copy));
            self.logged.iter().any(|line| {
                line.starts_with("queued write landed as a conflict copy")
                    && line.ends_with(&alt)
                    && line.split(' ').any(|field| {
                        field.starts_with("uid=")
                            && self.logged.iter().any(|kept| {
                                is_own_delete_kept(kept)
                                    && !kept.contains(" name=\"recovered-")
                                    && kept.split(' ').any(|other| other == field)
                            })
                    })
            })
        };
        let mut damage = Vec::new();
        for (copy, content) in &drive {
            let Some(content) = content else { continue };
            if !is_conflict_copy(copy) || model.contains_key(copy) {
                continue;
            }
            if !kept_deleted(copy) {
                continue;
            }
            damage.push(KnownDamage {
                bug: "B134",
                path: copy.clone(),
                copy: Some(copy.clone()),
                change: FsOp::Create {
                    path: copy.clone(),
                    data: content.clone(),
                },
            });
        }
        for (path, content) in model {
            let Some(content) = content else { continue };
            let copy = drive.iter().find(|(copy, other)| {
                is_conflict_copy(copy)
                    && !model.contains_key(*copy)
                    && parent_of(copy) == parent_of(path)
                    && other.as_ref() == Some(content)
                    && made_copy(path, copy)
            });
            let Some((copy, _)) = copy else {
                if !drive.contains_key(path) && adopted_trashed(path) {
                    damage.push(KnownDamage {
                        bug: "B128",
                        path: path.clone(),
                        copy: None,
                        change: FsOp::Unlink { path: path.clone() },
                    });
                }
                continue;
            };
            let (bug, change) = match drive.get(path) {
                None => (
                    "B126",
                    FsOp::Rename {
                        from: path.clone(),
                        to: copy.clone(),
                    },
                ),
                Some(Some(landed)) if landed == content => (
                    "B127",
                    FsOp::Create {
                        path: copy.clone(),
                        data: content.clone(),
                    },
                ),
                Some(_) => continue,
            };
            damage.push(KnownDamage {
                bug,
                path: path.clone(),
                copy: Some(copy.clone()),
                change,
            });
        }
        damage
    }

    fn difference(&self) -> String {
        let drive = self.drive_tree();
        let mut out = String::new();
        for (index, client) in self.clients.iter().enumerate() {
            let pending = client.daemon().pending().unwrap_or_default();
            for item in pending {
                out.push_str(&format!(
                    "  client {index} still owes: {} {} (attempts {}, last error {:?})\n",
                    item.kind, item.path, item.attempts, item.last_error
                ));
            }
            out.push_str(&diff(
                client.model.tree(),
                &subtree(&drive, &owned_folder(index)),
                &format!("client {index}'s model"),
                "Drive",
            ));
        }
        out
    }

    fn drive_tree(&self) -> Tree {
        self.drive
            .tree()
            .into_iter()
            .map(|(path, entry)| {
                let content = match entry {
                    Entry::Folder => None,
                    Entry::File(content) => Some(content.as_ref().clone()),
                };
                (path, content)
            })
            .collect()
    }

    fn note(&mut self, line: String) {
        let line = format!("{:>8.3}s {line}", self.started.elapsed().as_secs_f64());
        self.log.push(line);
    }

    fn report(&self, failure: &str, dir: &Path) -> String {
        let _ = std::fs::write(dir.join("steps.log"), self.log.join("\n") + "\n");
        let tail = &self.log[self.log.len().saturating_sub(LOG_TAIL)..];
        format!(
            "{} seed {} failed: {failure}\nreplay with PDFS_SIM_SEED={}; state kept in {}\nlast steps:\n{}",
            self.profile.name,
            self.seed,
            self.seed,
            dir.display(),
            tail.join("\n")
        )
    }
}

/// Make `op` under `root`. The outer error is a check that failed on the
/// way (the content an open file reads back), the inner one the errno.
fn perform(root: &Path, op: &FsOp, before: Option<&[u8]>) -> Result<Result<(), i32>, String> {
    let at = |path: &str| root.join(path);
    let result: io::Result<()> = match op {
        FsOp::Create { path, data } => std::fs::write(at(path), data),
        FsOp::Write { path, offset, data } => OpenOptions::new()
            .write(true)
            .open(at(path))
            .and_then(|mut file| {
                file.seek(SeekFrom::Start(*offset))?;
                file.write_all(data)
            }),
        FsOp::Truncate { path, len } => OpenOptions::new()
            .write(true)
            .open(at(path))
            .and_then(|file| file.set_len(*len)),
        FsOp::Mkdir { path } => std::fs::create_dir(at(path)),
        FsOp::Rename { from, to } => std::fs::rename(at(from), at(to)),
        FsOp::Unlink { path } => std::fs::remove_file(at(path)),
        FsOp::Rmdir { path } => std::fs::remove_dir(at(path)),
        FsOp::UnlinkOpen { path, data } => {
            let mut read = Vec::new();
            let result = OpenOptions::new()
                .read(true)
                .write(true)
                .open(at(path))
                .and_then(|mut file| {
                    std::fs::remove_file(at(path))?;
                    file.seek(SeekFrom::End(0))?;
                    file.write_all(data)?;
                    file.seek(SeekFrom::Start(0))?;
                    file.read_to_end(&mut read).map(|_| ())
                });
            if result.is_ok() {
                let expected = [before.unwrap_or_default(), data].concat();
                if read != expected {
                    return Err(format!(
                        "posix: {} read back {:?}, expected {:?}",
                        op.describe(),
                        String::from_utf8_lossy(&read),
                        String::from_utf8_lossy(&expected)
                    ));
                }
            }
            result
        }
    };
    Ok(result.map_err(|error| error.raw_os_error().unwrap_or(libc::EIO)))
}

fn target(op: &FsOp) -> &str {
    match op {
        FsOp::Create { path, .. }
        | FsOp::Write { path, .. }
        | FsOp::Truncate { path, .. }
        | FsOp::Mkdir { path }
        | FsOp::Unlink { path }
        | FsOp::UnlinkOpen { path, .. }
        | FsOp::Rmdir { path } => path,
        FsOp::Rename { from, .. } => from,
    }
}

/// The tree a mount shows, read through it, without the folders it makes up
/// that are not on Drive.
fn read_tree(mountpoint: &Path) -> Tree {
    fn walk(dir: &Path, prefix: &str, tree: &mut Tree) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if prefix.is_empty() && name == crate::r#virtual::SHARED_WITH_ME_BASE {
                continue;
            }
            let path = super::model::join(prefix, &name);
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                tree.insert(path.clone(), None);
                walk(&entry.path(), &path, tree);
            } else {
                tree.insert(path, std::fs::read(entry.path()).ok());
            }
        }
    }
    let mut tree = Tree::new();
    walk(mountpoint, "", &mut tree);
    tree
}

/// The part of `tree` below `folder`, relative to it.
fn subtree(tree: &Tree, folder: &str) -> Tree {
    let prefix = format!("{folder}/");
    tree.iter()
        .filter_map(|(path, entry)| Some((path.strip_prefix(&prefix)?.to_owned(), entry.clone())))
        .collect()
}

/// Whether a known bug is counted rather than failing the run.
fn tolerate_known() -> bool {
    std::env::var("PDFS_SIM_KNOWN").as_deref() != Ok("fail")
}

fn owned_folder(index: usize) -> String {
    format!("c{index}")
}

/// Whether `line` is the drain keeping a queued write as a conflict copy
/// because Drive no longer holds the file, or holds it in the trash.
fn is_own_delete_kept(line: &str) -> bool {
    line.starts_with("queued write conflicts; keeping a conflict copy")
        && (line.contains(" reason=\"the file was trashed remotely\"")
            || line.contains(" reason=\"the file no longer exists remotely\""))
}

fn is_conflict_copy(path: &str) -> bool {
    path.split('/')
        .any(|name| name.contains("(sync-conflict") || name.starts_with("recovered-"))
}

fn diff(left: &Tree, right: &Tree, left_name: &str, right_name: &str) -> String {
    let show = |entry: &Option<Vec<u8>>| match entry {
        None => "folder".to_owned(),
        Some(content) => format!("{:?}", String::from_utf8_lossy(content)),
    };
    let mut out = String::new();
    for (path, entry) in left {
        match right.get(path) {
            None => out.push_str(&format!(
                "  {path}: only in {left_name} ({})\n",
                show(entry)
            )),
            Some(other) if other != entry => out.push_str(&format!(
                "  {path}: {left_name} has {}, {right_name} has {}\n",
                show(entry),
                show(other)
            )),
            Some(_) => {}
        }
    }
    for (path, entry) in right {
        if !left.contains_key(path) {
            out.push_str(&format!(
                "  {path}: only in {right_name} ({})\n",
                show(entry)
            ));
        }
    }
    out
}

fn errno(outcome: Result<(), i32>) -> String {
    match outcome {
        Ok(()) => "ok".into(),
        Err(code) => io::Error::from_raw_os_error(code).to_string(),
    }
}

fn outcome(actual: &Result<Result<(), i32>, String>) -> String {
    match actual {
        Ok(result) => errno(*result),
        Err(check) => check.clone(),
    }
}

/// Run `profile` over the seeds the environment asks for: `PDFS_SIM_SEED`
/// alone, or `PDFS_SIM_SEEDS` of them (default `default_seeds`) from 1.
pub(crate) fn run_seeds(profile: &'static Profile, default_seeds: u64) {
    let seeds: Vec<u64> = match std::env::var("PDFS_SIM_SEED") {
        Ok(seed) => vec![seed.parse().expect("PDFS_SIM_SEED is a number")],
        Err(_) => {
            let count = std::env::var("PDFS_SIM_SEEDS")
                .ok()
                .and_then(|count| count.parse().ok())
                .unwrap_or(default_seeds);
            (1..=count).collect()
        }
    };
    let mut failures = Vec::new();
    let mut known = BTreeMap::<&str, usize>::new();
    for seed in seeds {
        match run(seed, profile) {
            Ok(hits) => {
                for (bug, count) in hits {
                    *known.entry(bug).or_default() += count;
                }
            }
            Err(failure) => failures.push(failure),
        }
    }
    if !known.is_empty() {
        eprintln!("{}: known bugs turned up: {known:?}", profile.name);
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    static ONE_CLIENT_LAN: Profile = Profile {
        name: "one-client-lan",
        clients: 1,
        steps: 80,
        faults: Faults::lan,
        link_flaps: false,
        restarts: false,
        deletes_mid_upload: false,
        settle_budget: Some(Duration::from_secs(4)),
        syscall_budget: Duration::from_secs(2),
        drain_budget: Duration::from_secs(20),
    };

    static ONE_CLIENT_DELETES: Profile = Profile {
        name: "one-client-deletes",
        clients: 1,
        steps: 30,
        faults: Faults::lan,
        link_flaps: false,
        restarts: false,
        deletes_mid_upload: true,
        settle_budget: Some(Duration::from_secs(4)),
        syscall_budget: Duration::from_secs(2),
        drain_budget: Duration::from_secs(20),
    };

    static ONE_CLIENT_ECHOES: Profile = Profile {
        name: "one-client-echoes",
        clients: 1,
        steps: 60,
        faults: Faults::echoes,
        link_flaps: false,
        restarts: false,
        deletes_mid_upload: false,
        settle_budget: Some(Duration::from_secs(6)),
        syscall_budget: Duration::from_secs(2),
        drain_budget: Duration::from_secs(20),
    };

    static ONE_CLIENT_FLAKY: Profile = Profile {
        name: "one-client-flaky",
        clients: 1,
        steps: 80,
        faults: Faults::wifi,
        link_flaps: true,
        restarts: true,
        deletes_mid_upload: false,
        settle_budget: None,
        syscall_budget: Duration::from_secs(10),
        drain_budget: Duration::from_secs(120),
    };

    /// Wi-Fi's latency, lag and lost replies without the outages: the queue
    /// has to drain within a settle, not merely by the end.
    static ONE_CLIENT_WIFI: Profile = Profile {
        name: "one-client-wifi",
        clients: 1,
        steps: 80,
        faults: Faults::wifi,
        link_flaps: false,
        restarts: false,
        deletes_mid_upload: false,
        settle_budget: Some(Duration::from_secs(30)),
        syscall_budget: Duration::from_secs(10),
        drain_budget: Duration::from_secs(120),
    };

    static THREE_CLIENTS: Profile = Profile {
        name: "three-clients",
        clients: 3,
        steps: 90,
        faults: Faults::lan,
        link_flaps: true,
        restarts: true,
        deletes_mid_upload: false,
        settle_budget: Some(Duration::from_secs(4)),
        syscall_budget: Duration::from_secs(2),
        drain_budget: Duration::from_secs(60),
    };

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse --lib sim::run -- --ignored`"]
    fn one_client_on_a_good_link() {
        run_seeds(&ONE_CLIENT_LAN, 12);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse --lib sim::run -- --ignored`"]
    fn one_client_deleting_files_as_they_upload() {
        run_seeds(&ONE_CLIENT_DELETES, 3);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse --lib sim::run -- --ignored`"]
    fn one_client_with_stale_hashes_and_reordered_echoes() {
        run_seeds(&ONE_CLIENT_ECHOES, 6);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse --lib sim::run -- --ignored`"]
    fn one_client_on_a_flaky_link() {
        run_seeds(&ONE_CLIENT_FLAKY, 4);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse --lib sim::run -- --ignored`"]
    fn one_client_on_a_slow_link() {
        run_seeds(&ONE_CLIENT_WIFI, 4);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse --lib sim::run -- --ignored`"]
    fn three_clients_one_writer_each() {
        run_seeds(&THREE_CLIENTS, 3);
    }

    #[test]
    fn conflict_copies_are_recognised_by_name() {
        assert!(is_conflict_copy("c0/x/a (sync-conflict 1790000000).txt"));
        assert!(is_conflict_copy("recovered-XrPUSW5W"));
        assert!(!is_conflict_copy("c0/x/a.txt"));
    }

    #[test]
    fn a_write_kept_because_its_file_is_gone_is_recognised() {
        let kept = "queued write conflicts; keeping a conflict copy uid=simvol~L10 \
                    name=\"f.txt\" reason=\"the file was trashed remotely\"";
        assert!(is_own_delete_kept(kept));
        assert!(is_own_delete_kept(
            &kept.replace("was trashed", "no longer exists")
        ));
        assert!(!is_own_delete_kept(
            &kept.replace("the file was trashed remotely", "revision changed")
        ));
    }

    #[test]
    fn an_invalidation_stuck_on_a_page_lock_is_b130() {
        let b130 = |stacks: &str| KNOWN_HANGS.iter().any(|known| (known.is)(stacks));
        let stuck = "thread 1 \"fuser-0\", state S, waiting in fuse_dev_do_read\n\
                     \nthread 2 \"tokio-rt-worker\", state D, waiting in __folio_lock\n\
                     \x20 11: <pdfs_fuse::NotifyBatch>::flush\n";
        assert!(b130(stuck));
        assert!(!b130(&stuck.replace("state D", "state S")));
        assert!(!b130(&stuck.replace("NotifyBatch", "Other")));
    }
}
