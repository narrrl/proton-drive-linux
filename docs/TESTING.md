# Testing

The normal Rust suite exercises the metadata database, sync planner, offline
queue, write staging, cache, and FUSE handler state without requiring a Proton
account:

```console
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Filesystem API and live FUSE acceptance suite

The account-free suite exercises the filesystem syscall contract against a
temporary local directory. This validates the runner and covers creation/open
flags, positioned and vectored I/O, truncation and sparse files, `mmap`,
`sendfile`, `copy_file_range`, durability barriers, namespace/error semantics,
`renameat2` flags, open-file lifetime, names and enumeration, readdir stability
under concurrent mutation, metadata updates, extended attributes, allocation and
punched holes, refusal semantics for unimplemented operations, concurrent I/O,
and real application workloads. It never reads application state or contacts
Proton:

```console
scripts/fuse-acceptance.sh --offline-only
```

Kernel/FUSE behavior and remote convergence need a real mount. With no
arguments, the script tests the account the running service is logged in to:

```console
scripts/fuse-acceptance.sh           # every case, every mount kind
scripts/fuse-acceptance.sh --quick   # one on-demand/mirror pair instead of four
```

An account run does this, in order:

1. Runs the account-free contract as the reference.
2. Runs the contract in a fresh `pdfs-acceptance-<id>` folder in My files and
   diffs it against the reference. After the last case it reads back a file
   the run wrote and records its digest. A read that fails there, as one did
   with the link down (B119), is a failed result named "a file of this run
   reads back at the end", and the cleanup still runs.
3. Registers two new sync folders under `~/.cache/pdfs-acceptance/run-<id>/`
   and runs the contract and the move cases in every on-demand/mirror pairing.
4. Removes everything it created, locally and remotely, and reports a
   `cleanup` result. The run fails if anything is left behind.

Cleanup identifies remote folders by uid, never by name alone, so nothing of
yours is touched. It removes each sync folder with `sync rm --delete-remote`,
trashes the My files folder, and then deletes both permanently from the trash.
It only counts a folder as gone once a fresh trash listing no longer shows it.
It then reads `pdfs sync queue` and counts any queued op that still names one
of its folders as left behind too, since such an op keeps its staged bytes and
retries forever (B102).

Ops that were queued before a target starts are not the run's. The run prints
one `NOTE` line naming them, and the queue waits ignore them. They are told
apart by id and queue time together, since SQLite can give a new op the id of
one that has drained.

A write is queued when the kernel releases the file, and `close(2)` returns
without waiting for that. A case that reads `pdfs sync queue` right after
writing therefore waits for the ops it expects (`Daemon.queued`) instead of
reading once. "regression B102" saw one revision of two before it did.

Cleanup also runs on a failure, Ctrl-C, `SIGTERM` or `SIGHUP`. While it runs,
two more interrupts are ignored; a third abandons it.

Each run records what it is about to create in
`~/.cache/pdfs-acceptance/run-<id>/manifest.json` before it creates it, and
holds a lock on that directory. If a run dies without cleaning up (`kill -9`, a
hard timeout, a power cut), the next run finds the unlocked manifest and
finishes that cleanup first. It then removes stray `pdfs-acceptance-<32 hex>`
folders that no live run claims: in My files (older than
`PDFS_ACCEPTANCE_REAP_AGE`, default one hour), under the acceptance home, and
in the trash. A run does not start while that reap leaves anything behind.

The run creates and destroys real remote folders, so a dedicated test account
without irreplaceable data is still the safest target.

`--live PATH...` tests mounts you set up yourself and leaves the sync folder
configuration alone. It runs destructive POSIX operations only below a fresh
`pdfs-acceptance-*` directory, refuses paths that are not FUSE mounts, and
removes its directory on success, failure, or interruption:

```console
scripts/fuse-acceptance.sh --live /mnt/on-demand-testmount
```

### Reading the output

A run opens with what it is about to test: the mode, the `pdfs` binary and its
version, Python and the kernel, the per-case limit, the options and the start
time. Each target then names its root, the storage behind it, the daemon it
talks to and how many cases it runs, followed by one line per case:

```
==> live FUSE /mnt/testmount
  -> root      /mnt/testmount/pdfs-acceptance-…
  -> storage   FUSE mount /mnt/testmount (fuse.protondrive)
  -> daemon    pdfs 2.8.3, user@proton.me, online, 0 queued ops
  -> cases     45 of 45
  [ 1/45] creation and open flags .................................. ok       0.41s
  …
  [21/45] throughput floors ........................................ ok      48.20s
          write 31 MiB/s, read 54 MiB/s, 4 KiB writes 12.0 MiB/s, metadata 9 ops/s
  …
  [43/45] regression B115: pdfs ls reports a file's real size ...... ok       6.12s
  -> reference matches the ordinary filesystem in 412 compared observations
  -> result    44 passed, 1 skipped in 11m02s
```

A case ends as `ok`, `FAIL`, `TIMEOUT`, `skip` when it does not apply to the
target (no daemon, no share at that role), or `known` when an `open B<n>` case
reproduced a bug that is still open (see below). The dimmed lines under a case
are its details: the reason for a skip, measured rates, a hash. A failed
`regression B<n>` case also points to its entry in `docs/BUGS.md`. When a case
prints something itself, such as a service restart, the case line is printed
again once the case ends.

The run ends with a table of every target's counts and time, the report paths,
the slowest cases, the cases over `--budget`, the known issues that still
reproduce, any errors the daemon logged, and each failure with its target. The
last line is `PASS:` or `FAIL:` and the totals. Colour is used only on a
terminal and never with `NO_COLOR` set.

### Edge cases and performance

Besides the syscall contract, the suite covers:

- **Block boundaries.** Files of 0 bytes up to two 4 MiB blocks plus 3 bytes,
  including every size next to a boundary, then overwrites that grow, shrink
  and reshape them.
- **Unusual names.** A 255-byte name, NFC and NFD forms of the same text,
  newline, tab, emoji, characters Windows forbids, leading and trailing
  spaces, and a case-only rename.
- **A deep tree and a wide directory.** 24 levels of nesting renamed at the
  top; 128 entries created concurrently, concurrent unlinks, then `rm -rf`.
  Online, each create is a round trip to the server, and the kernel runs
  creates in one directory one at a time. So the wide case gets twice the
  per-case timeout and notes the time per create as `wide.create_ms`.
- **Rename patterns.** Swaps, chains, log rotation, replacing a file under an
  open reader, the `EISDIR`/`ENOTDIR`/`ENOENT` refusals, and a folder moved out
  and back.
- **Handle coherency.** Unsynced writes seen by a second handle, two `O_APPEND`
  writers, 30 rewrite generations, and a truncate under a reader.
- **Throughput.** 64 MiB written and read sequentially, 1 MiB in 4 KiB writes,
  and metadata operations on 100 files. The rates are printed. The case fails
  below `PDFS_ACCEPTANCE_MIN_MIBPS` (default 10) or `PDFS_ACCEPTANCE_MIN_OPS`
  (default 20 operations per second, or 3 on a FUSE mount, where each create
  and unlink waits for Drive before it returns).

### The mount is diffed against an ordinary filesystem

The local reference run is not only a self-test of the runner. Every case
records the facts it established, and a live run is compared against those
recordings, so a case fails when the mount *differs* from an ordinary
filesystem — not only when it raises. The target's `reference` line names the
exact key:

```
  -> reference 1 observation of 412 differs from the ordinary filesystem:
               ! creation and open flags.truncated.size: live FUSE /mnt/testmount=6 reference=0
```

Facts are recorded in one of two ways, chosen in the test body:

- **compared** — a claim about filesystem semantics. The mount must agree.
- **noted** — context that legitimately differs: inode numbers, block counts,
  reported mode bits, and which optional syscalls exist at all. Listed under
  the target's `accepted` line, never a failure.

Because the choice is made at each call site, the set of accepted divergences
is visible in the test that accepts it. There is no separate allowlist to drift
out of date. When adding a case, prefer `record`; reach for `note` only when an
ordinary filesystem can do something a network filesystem genuinely cannot.

### Operations the filesystem does not implement

`git`, `rsync`, `cp -a`, `tar`, `df` and most installers probe for symlinks,
hard links, FIFOs, device nodes, `statfs`, file locks, `copy_file_range`,
`O_DIRECT` and `SEEK_HOLE` whether or not a filesystem offers them. The suite
asserts that each either works or is *refused cleanly* — a recognised errno,
never `EIO` and never a hang. A refusal that is merely unimplemented is fine;
one that is dirty breaks software that never asked for the feature.

### Regressions from the bug ledger

Cases named `regression B<n>` reproduce a bug from `docs/BUGS.md` so it stays
fixed. B69 (identical rewrite must not fork a `(sync-conflict …)` copy), B70 (an
in-flight `.crdownload` must not be sealed as a revision) and B74 (a file
renamed after close must keep its content) need a real mount and a reachable
daemon; they are skipped, not failed, without one.

**B100** (a copied tree reads back at its exact sizes) copies 48 files of
assorted sizes into the mount at once, as `cp -r` does. On a live mount it
waits for the queue, checks every size and byte, runs `pdfs refresh` on each
folder to drop the cached listing, and checks again. The refresh is what
brought back the provisional sizes the bug reported.

**B101** (a write during its create's upload reaches Drive) holds new files open for
different times before writing, so their empty creates land before, during and after the
bytes arrive, then checks every file after the queue drains.

**B113** (moving a file before its upload makes no conflict) pauses sync with
`pdfs sync pause --for 10m` and writes three files. While their uploads are held,
it renames one, moves one into a folder, and renames and moves the third. It then
resumes sync, waits for the queue, and checks the bytes and names and that no
conflict copy appeared. It skips when sync was already paused. If the run dies
before the resume, the pause ends by itself after ten minutes.

**B88** (`pdfs mkdir`, `rename` and `rm` by local path) creates, renames and
removes a folder and a file by their paths in the mount. Then it checks the
refusals: `pdfs rename` of the mount's root to its own name, and `pdfs rm` of a
file outside every mount. `pdfs rm` of the root itself is tried only on a sync
folder the run created. In a mirrored sync folder all three commands must be
refused instead, and nothing may change.

**pdfs output into a closed pipe** runs `pdfs ls`, `pdfs --json ls` and
`pdfs status` with stdout on a pipe whose reader is already gone. Each must exit
0 with nothing on stderr, as `pdfs ls | head -1` needs.

**B102** (removing a folder drops the uploads queued inside it) uploads a file
into each of two nested folders, pauses sync and rewrites both. While the
revisions are held, it removes one folder with `pdfs rm` and the other through
the mount. Neither revision may stay queued, and neither folder may come back
after the resume. It skips when sync was already paused.

**B94** (a deleted `.part` file leaves nothing queued) writes a `.part` file,
checks that its create is parked, and unlinks it. The parked op must leave the
queue within 30 seconds. The sweep that lets an hour-old park through is not
driven.

**B95** (a `.tmp` file uploads at once) writes `index.tmp` and `preview.temp`
and waits for the queue, which fails at once on a parked op. `pdfs ls` must
list both, and their bytes must read back.

**B111** (renaming a file straight back works) renames a file there and back
eight times through the mount. On a mount it also does so three times with
`pdfs rename`, after the queue is empty, so the renames race nothing but Drive.
It runs against the reference too.

**B98** (an upload superseded by a write ends) writes 64 MiB and waits for its
upload to show in `pdfs transfers`, then appends to the file. The queue must
drain, the old upload must leave the transfer list, and the file must hold
every byte with no conflict copy. It skips when the upload is never seen on
the wire, and it gets twice the per-case timeout.

**B80** (`pdfs search` finds a new file after a rewrite) writes a file whose
name carries a unique token, waits for the queue, rewrites it and waits again.
`pdfs search` must find it within 30 seconds.

**B48** (an emptied folder can be removed at once) writes twelve files into a
folder, lists it, and moves one more in from elsewhere. It unlinks every
child, runs `pdfs refresh` on a mount, and checks that nothing is listed and
that `rmdir` works. It runs against the reference too.

**B114** (a name `pdfs rm` or `rename` took away stops resolving) looks up two
files, removes one with `pdfs rm` and renames the other with `pdfs rename`. At
once, neither old name may be listed or resolve.

**B115** (`pdfs ls` reports a file's real size) writes files of 6 bytes, 4097
bytes and one block plus 7, and compares the sizes `pdfs ls` lists with the
written ones, after the upload and again after `pdfs refresh`.

**B116** (trashing a folder drops the creates queued in its subfolders) makes
`b116/sub/deeper` and waits for Drive. With sync paused, it writes a `.part`
file there and renames it to its final name, so its create is due but held.
It then runs `pdfs rm b116`, and the create must be gone from the queue. It
restores the folder with `pdfs restore` before it resumes sync. If the file
ends up in the mount's root anyway, the case fails and moves the file to the
trash.

**B117** (a moved file's queued upload keeps its path) rewrites an uploaded
file with sync paused and moves it with `pdfs move`. `pdfs sync queue` must
list the upload under its new path, and the journal must not show the upload
put off with "pending operation deferred". The bytes must arrive at the new
path with no conflict copy. Without `journalctl`, only the queue is checked.

**B119** (an uploaded file stays cached) writes a small file and one of a block
plus 119 bytes, waits for the queue, and expects `pdfs ls` to list both as
cached, so reading them does not need the network.

B74 was found by exactly this mechanism: the B70 case failed, and narrowing it
produced a smaller reproduction that got its own case. The defect is fixed and
live-validated (2026-07-26); the case now guards against its return.

**B79** (an on-demand device folder accepts writes) discovers its target from
`pdfs locations --json` and skips when this machine has no mounted on-demand
folder. It is the cheap standing guard against a *cross-mount* permission
regression: the primary mount holds device-folder nodes too, and the queue guard
intersects what every live inode space says about a uid, so a fail-closed
classification in one mount denies writes in another.

**B34** (a viewer-role share is read-only) and **B34b** (an editor-role share
still writes) locate their subject through `pdfs shared-with-me --json`, which
now carries `role` and a mount-relative `path`. Both skip when the account has no
accepted share at that role — B34 needs a **second account** to have shared a
folder read-only, which is the one part of the sharing suite that cannot be
self-served. B34b degrades gracefully in the other direction: a shared *file*
gets its mode bits and `W_OK` checked but is never written to, because it is
someone else's document.

B34's fourth assertion is the load-bearing one: `pending_uploads` and
`pending_changes` must be **unchanged** across the refused writes. Mode bits
alone would pass a fix that still admitted the write into `pending_op`, which is
the actual harm — a background drain failing 403 forever.

**Synthetic `Shared with me/` directory contract** asserts the virtual directory
enumerates, reports `0555`, and refuses `mkdir`/`rmdir`/`rename`. It skips when
the directory is absent (an account with no accepted shares).

A regression case asserts against the *remote* outcome, so it must wait for the
queue to drain before reading. Reading straight back through the mount proves
nothing: the local attribute and content caches will happily return the bytes
that were just written even when nothing reached Drive, which is precisely how
B74 stayed invisible.

### Bugs that are still open

Cases named `open B<n>` check a bug that `docs/BUGS.md` still lists as open.
While the bug reproduces, the case ends as `known`: it is listed at the end of
the run, but does not fail it. So a known bug does not hide new failures, and
the run notices when a fix lands, because the case then passes. Rename it to
`regression B<n>` then, and fail it with `check` instead of `KnownIssue`. No
bug is open at the moment. B114 to B117 started as such cases.

### What a run verifies

A full account run with `--journal-check` checks these `docs/BUGS.md` entries,
many of them fixed but not yet verified:

- B48, B80, B94, B95, B98, B100, B101, B102, B111, B114, B115, B116, B117 and
  B119, each with its own `regression B<n>` case.
- B118, with "application workloads" and "block boundaries", which failed on
  it. It needs Drive to be slow to list a new node, so a clean run is weak
  evidence.
- B120, with `--journal-check`, but only in a run that goes offline while it
  writes. A mkdir made online goes to Drive at once, so only then does a
  folder land from the queue.
- B86, with the mode-switch check in the full matrix.
- B104, B106, B108 and B109, with the cases that found them: "rename
  patterns", "application workloads" and the account run's own setup.

B105, B107 and B110 are races. The suite hit each of them once, so a clean run
is weak evidence for them. `--journal-check` reports B110's stall warning, and
every case that writes checks for B105's conflict copies.

The suite does not drive:

- the GUI and the photo gallery (B81, B92, B93, B97);
- states it cannot set up: a `parent_uid` cycle (B82), staged bytes that name
  no node (B96), a stuck trash refresh (B103), conflict copies left from an
  earlier start (B99), a foreign FUSE mount in the local-file index (B90),
  revisions without 4 MiB blocks (B85), and several shares from one owner
  (B112);
- B94's hour-long expiry of a park;
- load and failure injection (B22, B27, B30, B37, B42).

Check those by hand, or with the unit tests their entries name.

### Timeouts, reports, and hung mounts

A wedged FUSE operation cannot be interrupted from Python, so each case runs
under a soft alarm (`--timeout`, default 180s) that reports the case and lets
cleanup run, backed by a hard stop that dumps every thread's stack and exits.
That backstop is the only way to learn *where* a mount hung. After a timeout the
run checks whether the mount still answers a `stat` and a listing within 15
seconds. If it does, the target goes on with its next case, so one slow stretch
of the link does not cost every later case. If it does not, the target's later
cases are not run: against a wedged mount they produce noise, not information.
Cases that make many round trips by design ("application workloads", "unusual
but legal names", "a wide directory" and "regression B98") get twice the limit.

```console
scripts/fuse-acceptance.sh --live /mnt/testmount \
  --timeout 300 --report-junit results.xml --budget 30
```

`--report-json` also writes every recorded observation, which is what to attach
to a bug report. A known issue has the status `known` there; JUnit has no such
status, so `--report-junit` reports it as skipped. `--budget SECONDS` flags cases that pass but got slower —
B5 was a 186 ms-per-call regression that a pass/fail suite could not see.
`--list` prints every case and which targets it runs against, and `--fail-fast`
stops at the first failure.

`--journal-check` fails the run if `proton-drive.service` logged any error while
it ran. A suite can pass every assertion while the daemon logs failures behind
it; that has happened, and it was caught only by reading the journal by hand.
Two warnings count as errors too: "queued write conflicts; keeping a conflict
copy" (how B113 showed) and "a fuse worker has held the same job for a long
time" (how B110 showed).

With `--live`, abandoned `pdfs-acceptance-*` roots from a crashed run are
removed at startup, but only if they match the exact generated name and are
over an hour old (`PDFS_ACCEPTANCE_REAP_AGE`), so a concurrent run is never
disturbed. An account run instead uses the manifest and reaper described
above.

### Automated mode matrix

An account run drives this matrix itself, in folders it creates. To use
directories of your choice instead, the managed live runner accepts two
existing, empty, unmounted directories. It
registers both as new sync folders, waits for initial synchronization, then
tests all four pairs in order: on-demand/on-demand, on-demand/mirror,
mirror/on-demand, and mirror/mirror. Every transition must become idle, and an
on-demand folder must actually appear as a mount before its contract runs.

```console
mkdir -p /mnt/pdfs-test-a /mnt/pdfs-test-b
scripts/fuse-acceptance.sh --managed-live /mnt/pdfs-test-a /mnt/pdfs-test-b
```

This creates remote folders and permanently deletes those test remotes during
cleanup. It refuses non-empty paths, existing mounts, and paths already present
in `pdfs sync list`. Use `PDFS_ACCEPTANCE_SYNC_TIMEOUT` to extend transition
timeouts and `PDFS_ACCEPTANCE_PDFS` to select a non-installed CLI binary, for
example `target/debug/pdfs`.

After the contract, each mode pair also runs the move cases (`--list` shows
them as `managed`). They move trees between the two folders in both
directions, move several sources in one `pdfs move`, and pass a tree through
My files and out again. Every case waits for Drive before and after the move.
It then checks that the tree is only at the destination, that every byte
arrived, and that neither side made conflict copies. The refusal cases prove
that nothing moves and nothing is lost when:

- the destination already has the name;
- a folder would move into itself;
- a mirror source holds something Drive lacks (a symlink);
- a file in an on-demand source is still open for writing.

The My files case writes a test folder into your real My files and removes it
afterwards. It skips when the daemon reports no My files mount, or when the
locations are on different volumes.

Before a mirror folder switches to on-demand, the runner writes a new file into
it and syncs. Once the folder is mounted, the file must be listed with its
bytes (B86). Each switch reports a `regression B86` result under a separate
`mode switches` target. The bug only shows in a folder that was mounted in an
earlier pairing, because that mount's listing is stale. That happens in the
third pairing, so `--quick` runs the check but does not replay the bug.

In every pairing, each folder must still hold the file the runner wrote into it
at setup, byte for byte. These checks report under `mode switches` as well. A
switch to on-demand frees the local copy, so these reads go to Drive. If the
link drops, the daemon fails such a read with EIO once its two-minute read
budget runs out. That is correct, so the runner waits until the daemon reports
online again, tries once more and notes the retry under the result. Only the
second try decides the result.

### Durability drill

`--durability` restarts `proton-drive.service` in the middle of the suite and
re-verifies the bytes written before it. `staging/` and `recovery/` can hold the
only copy of a file whose upload has not finished, so this is the case that
proves the queue is persistent rather than in-memory. It is opt-in because it
interrupts a running daemon; `PDFS_ACCEPTANCE_UNIT` selects a different unit.

### Selecting cases

During development, `PDFS_ACCEPTANCE_ONLY` runs tests whose descriptive name
contains the supplied text, ignoring case (for example
`PDFS_ACCEPTANCE_ONLY=namespace`). Separate several texts with commas to run
every case that matches any of them (`PDFS_ACCEPTANCE_ONLY=B116,B117`). A
release acceptance run must leave it unset so the complete contract executes.

A filter naming a live-only case (every `regression B<n>` is one) selects nothing
in the account-free reference run, which is not an error — that target simply has
nothing to do. Only a filter matching *no case at all* fails, as the typo it is.

Passing more paths runs the same filesystem suite independently against each.
The first path must be the FUSE mount under test; later paths may be another
FUSE mount or a normal local mirror filesystem:

```console
PDFS_ACCEPTANCE_SYNC_TIMEOUT=180 \
  scripts/fuse-acceptance.sh --live /mnt/on-demand-testmount /mnt/testmount
```

If two paths really are views of the **same remote folder**, enable an
additional byte-for-byte convergence check explicitly:

```console
PDFS_ACCEPTANCE_CONVERGENCE=1 \
  scripts/fuse-acceptance.sh --live /mnt/first-view /mnt/second-view
```

Do not enable that flag merely because two sync folders belong to the same
account. Their configured remote UIDs must be identical.

Normally the live suite applies the same API contract independently to each
supplied mount. In convergence mode it exercises the primary once and verifies
that the resulting bytes appear through every secondary view. A pass is
strong evidence for those paths, not a blanket data-loss guarantee. Before a
release, also run the recovery drills in `docs/RECOVERY.md` and inspect
`docs/BUGS.md` for live-verification items.

The runner accepts already-mounted paths instead of starting a daemon. This
keeps authentication and keyring setup explicit and prevents it from silently
reusing or replacing the normal application state directory.
