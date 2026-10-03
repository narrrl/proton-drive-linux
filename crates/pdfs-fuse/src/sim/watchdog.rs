//! Turns a call through a mount that never returns into a failed seed.
//!
//! A deadlock in a daemon leaves the run's own thread blocked in the kernel,
//! where no budget check is reached, and a CI job then waits for its timeout.
//! The watchdog runs beside the run. When what it was armed for outlives its
//! limit, it aborts the FUSE connections of the run's mounts, so the blocked
//! call fails and the run reports the hang, and writes where every thread was
//! to `stacks.txt`.
//!
//! The stacks are taken in the process: no tracer can stop a thread that
//! waits on a FUSE request, so `gdb` and `eu-stack` wait as long as the hang.
//! Each thread is sent [`stack_signal`] and captures its own backtrace in the
//! handler. A thread in the kernel takes the signal only once it leaves, so
//! the mounts are aborted after the others have answered, and its backtrace
//! is the call it was blocked in. Its kernel wait channel is recorded first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::daemon::wait_until;

/// How long the threads that are not blocked in the kernel have to answer,
/// and the blocked ones once the mounts are aborted.
const ANSWER: Duration = Duration::from_secs(5);

pub(crate) struct Watchdog {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Shared {
    /// The run's directory: its mounts are below it, and the stacks go in it.
    dir: PathBuf,
    armed: Mutex<Option<Armed>>,
    /// What hung, once something has.
    fired: Mutex<Option<Hung>>,
    stop: AtomicBool,
}

struct Armed {
    what: String,
    limit: Duration,
    since: Instant,
}

/// What hung, and where every thread was then.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Hung {
    pub(crate) what: String,
    pub(crate) stacks: String,
}

/// Disarms the watchdog when dropped.
pub(crate) struct Watching(Arc<Shared>);

impl Watchdog {
    pub(crate) fn new(dir: &Path) -> Self {
        let shared = Arc::new(Shared {
            dir: dir.to_path_buf(),
            armed: Mutex::new(None),
            fired: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("sim-watchdog".into())
                .spawn(move || watch(&shared))
                .expect("spawn the simulation watchdog")
        };
        Self {
            shared,
            thread: Some(thread),
        }
    }

    /// Watch `what` until the guard is dropped, and abort the mounts if it is
    /// still going after `limit`.
    pub(crate) fn arm(&self, limit: Duration, what: impl Into<String>) -> Watching {
        *self.shared.armed.lock() = Some(Armed {
            what: what.into(),
            limit,
            since: Instant::now(),
        });
        Watching(self.shared.clone())
    }

    /// What hung, if something did. Call [`Watchdog::stop`] first, or the
    /// stacks may still be on their way.
    pub(crate) fn fired(&self) -> Option<Hung> {
        self.shared.fired.lock().clone()
    }

    /// Stop watching, once the stacks of a hang that was caught are taken.
    pub(crate) fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Watching {
    fn drop(&mut self) {
        *self.0.armed.lock() = None;
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

fn watch(shared: &Shared) {
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        let hung = shared
            .armed
            .lock()
            .as_ref()
            .filter(|armed| armed.since.elapsed() > armed.limit)
            .map(|armed| format!("{} has not returned after {:?}", armed.what, armed.limit));
        let Some(hung) = hung else { continue };
        *shared.armed.lock() = None;
        let path = shared.dir.join("stacks.txt");
        // Before anything is let go: the run reports the hang, not whatever
        // the aborted call answers.
        *shared.fired.lock() = Some(Hung {
            what: format!("{hung}; the stacks are in {}", path.display()),
            stacks: String::new(),
        });
        let stacks = take_stacks(|| abort_mounts_below(&shared.dir));
        let what = match std::fs::write(&path, &stacks) {
            Ok(()) => format!("{hung}; the stacks are in {}", path.display()),
            Err(error) => format!("{hung}; the stacks could not be kept: {error}"),
        };
        *shared.fired.lock() = Some(Hung { what, stacks });
    }
}

/// One thread of this process as the kernel sees it.
struct Task {
    tid: i32,
    name: String,
    /// `D` while it waits in the kernel uninterruptibly.
    state: String,
    wchan: String,
}

/// Where every other thread of this process is, with `release` called once
/// those not blocked in the kernel have answered.
fn take_stacks(release: impl FnOnce()) -> String {
    let own = gettid();
    let tasks = tasks();
    STACKS.lock().clear();
    install_stack_handler();
    for task in &tasks {
        if task.tid != own {
            // SAFETY: tgkill only sends a signal, whose handler is installed.
            unsafe {
                libc::syscall(
                    libc::SYS_tgkill,
                    std::process::id() as i32,
                    task.tid,
                    stack_signal(),
                )
            };
        }
    }
    let answered = |blocked: bool| {
        let stacks = STACKS.lock();
        tasks
            .iter()
            .filter(|task| task.tid != own && (blocked || task.state != "D"))
            .all(|task| stacks.contains_key(&task.tid))
    };
    wait_until(ANSWER, || answered(false));
    release();
    wait_until(ANSWER, || answered(true));
    let stacks = std::mem::take(&mut *STACKS.lock());
    let mut out = String::new();
    for task in tasks.iter().filter(|task| task.tid != own) {
        out.push_str(&format!(
            "thread {} {:?}, state {}, waiting in {}\n",
            task.tid, task.name, task.state, task.wchan
        ));
        match stacks.get(&task.tid) {
            Some(stack) => out.push_str(&format!("{stack}\n")),
            None => out.push_str("  (no answer)\n\n"),
        }
    }
    out
}

fn tasks() -> Vec<Task> {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let read = |dir: &Path, file: &str| {
        std::fs::read_to_string(dir.join(file))
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let tid = entry.file_name().to_str()?.parse().ok()?;
            let dir = entry.path();
            // The state follows the name in parentheses, which may hold spaces.
            let stat = read(&dir, "stat");
            let state = stat.rsplit_once(") ")?.1.split(' ').next()?.to_owned();
            Some(Task {
                tid,
                name: read(&dir, "comm"),
                state,
                wchan: read(&dir, "wchan"),
            })
        })
        .collect()
}

/// The backtraces the threads captured, by thread id.
static STACKS: Mutex<BTreeMap<i32, std::backtrace::Backtrace>> = Mutex::new(BTreeMap::new());

/// The signal that asks a thread for its stack: one of the real-time signals
/// that nothing in the daemon uses.
fn stack_signal() -> i32 {
    libc::SIGRTMIN() + 7
}

fn install_stack_handler() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    extern "C" fn handler(_: i32) {
        // Not async-signal-safe: the capture allocates. The process is being
        // taken down for a hang anyway, and a thread interrupted inside the
        // allocator only fails to answer.
        let stack = std::backtrace::Backtrace::force_capture();
        STACKS.lock().insert(gettid(), stack);
    }
    INSTALLED.call_once(|| {
        // SAFETY: a zeroed sigaction with a handler and SA_RESTART is valid,
        // and the handler only touches `STACKS`.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as extern "C" fn(i32) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(stack_signal(), &action, std::ptr::null_mut());
        }
    });
}

fn gettid() -> i32 {
    // SAFETY: gettid has no arguments and cannot fail.
    unsafe { libc::gettid() }
}

/// Abort the FUSE connection of every mount below `dir`. Its connection is
/// named by the mount's device minor, and the mount's owner may abort it.
fn abort_mounts_below(dir: &Path) {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let (Some(device), Some(mountpoint)) = (fields.get(2), fields.get(4)) else {
            continue;
        };
        if !Path::new(mountpoint).starts_with(dir) {
            continue;
        }
        if let Some((_, minor)) = device.split_once(':') {
            let _ = std::fs::write(format!("/sys/fs/fuse/connections/{minor}/abort"), "1");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::daemon::scratch;

    #[test]
    #[ignore = "signals every thread of the test process: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn a_call_past_its_limit_is_reported_with_the_stacks() {
        let dir = scratch("watchdog");
        let mut watchdog = Watchdog::new(&dir);
        {
            let _watching = watchdog.arm(Duration::from_millis(100), "a nap");
            std::thread::sleep(Duration::from_secs(1));
        }
        watchdog.stop();
        let hung = watchdog.fired().expect("the watchdog fired");
        assert!(
            hung.what.starts_with("a nap has not returned after"),
            "{hung:?}"
        );
        assert!(hung.stacks.contains("a_call_past_its_limit"), "{hung:?}");
        let kept = std::fs::read_to_string(dir.join("stacks.txt")).unwrap_or_default();
        assert_eq!(kept, hung.stacks);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_call_within_its_limit_is_not() {
        let dir = scratch("watchdog-quiet");
        let watchdog = Watchdog::new(&dir);
        drop(watchdog.arm(Duration::from_millis(100), "a quick call"));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(watchdog.fired(), None);
        drop(watchdog);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
