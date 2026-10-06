# Testing

Three layers check the client, from fast and offline to slow and against a real account:

| Layer | Needs | Runs in CI |
|---|---|---|
| [Unit tests](#unit-tests) | Nothing | Every push and pull request |
| [Simulation runs](#simulation-runs) | `/dev/fuse` | Every push to `main` |
| [Acceptance suite](#acceptance-suite) | Nothing for the reference run; a signed-in account for the rest | The reference run only |

## Unit tests

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Tests sit in `#[cfg(test)]` modules next to the code. Database tests are in
`crates/pdfs-core/src/db/tests.rs`; a schema change adds one that migrates from the previous
version.

To check the migrations against a real database, point `PDFS_MIGRATE_DB` at it and run the
ignored test. It migrates a copy taken with `VACUUM INTO` and never writes the database named:

```bash
PDFS_MIGRATE_DB=~/.local/state/proton-drive-linux/cache.db \
  cargo test -p pdfs-core a_copy_of_a_real_database -- --ignored
```

## Simulation runs

The simulation runs (`crates/pdfs-fuse/src/sim/`) start real daemons with real FUSE mounts against
an in-memory Drive, `FakeDrive`. The daemon runs as in production (drain workers, sync engine,
event feed, online probe, control socket); only its Drive is fake. `FakeDrive` has the faults the
real one has: listings that lag a create, renames refused for a stale name hash, no atomic
replace, delayed and reordered events, requests that stall or lose their answer, per-request
latency, and blocks of uneven size.

A run draws a sequence of steps from a seed: syscalls through a client's mount, the link going
down and up, restarts, settles (every queue must drain within a budget), and files deleted while
the drain holds a write to them. Every syscall is checked against an in-memory model of the
filesystem as it happens. At the end every link comes up and the run checks:

1. **No loss and POSIX.** Drive's copy of each client's folder is what its model says.
2. **No false conflicts.** No conflict copy anywhere; each file has one writer.
3. **Convergence.** Each mount shows Drive's tree.
4. **Liveness.** No syscall took longer than its budget, and every queue drained once the links
   were up. A call that never returns is caught by a watchdog that writes every thread's stack to
   `stacks.txt` in the run's state directory.

| Test | Seeds |
|---|---|
| `one_client_on_a_good_link` | 12 |
| `one_client_deleting_files_as_they_upload` | 3 |
| `one_client_with_stale_hashes_and_reordered_echoes` | 6 |
| `one_client_on_a_flaky_link` | 4 |
| `one_client_on_a_slow_link` | 4 |
| `three_clients_one_writer_each` | 3 |

`sim/daemon.rs` holds scenario tests for single bugs, run the same way.

The tests mount FUSE, so they are ignored by default:

```bash
cargo test -p pdfs-fuse --lib sim:: -- --ignored --test-threads=1   # what CI runs
PDFS_SIM_SEEDS=50 cargo test -p pdfs-fuse --lib sim::run -- --ignored --test-threads=1
PDFS_SIM_SEED=7 RUST_LOG=pdfs_fuse=debug cargo test -p pdfs-fuse --lib \
  sim::run::tests::one_client_on_a_flaky_link -- --ignored --exact --nocapture
```

| Variable | Effect |
|---|---|
| `PDFS_SIM_SEEDS` | How many seeds each test runs, from 1 |
| `PDFS_SIM_SEED` | Replay one seed. A failing run prints the seed, the last steps, and where it kept its state |
| `PDFS_SIM_MEASURE` | Run `a_thousand_files_drain_as_fast_on_wifi_as_on_lan`, which times 1,000 files in 100 folders on a LAN and a Wi-Fi profile and prints the times |

A failure is a seed that can be replayed, but the daemon uses real threads, so a race may need
several replays. In CI, the `simulation` job uploads the stacks of a hung run as an artifact.

## Acceptance suite

`scripts/fuse-acceptance.sh` checks the filesystem contract a program sees: creation and open
flags, positioned and vectored I/O, truncation and sparse files, `mmap`, `sendfile`,
`copy_file_range`, durability barriers, renames and their `renameat2` flags, open-file lifetime,
names, `readdir` under concurrent change, metadata, extended attributes, holes, refusals of
unsupported operations, concurrent I/O, and real application workloads.

> [!CAUTION]
> Every mode except `--offline-only` creates and deletes real files in Proton Drive. Use a test
> account with nothing you cannot lose.

### Modes

```bash
scripts/fuse-acceptance.sh --offline-only            # the reference run; no account
scripts/fuse-acceptance.sh                           # the signed-in account, every mode pairing
scripts/fuse-acceptance.sh --quick                   # one on-demand/mirror pairing instead of four
scripts/fuse-acceptance.sh --live /mnt/testmount     # mounts you set up yourself
scripts/fuse-acceptance.sh --managed-live /mnt/a /mnt/b
```

**The reference run** always runs first, against a temporary local directory. It records the
facts each case establishes, and every mount is then **compared** against those facts. A case
fails when the mount differs from an ordinary filesystem, not only when it raises. A fact that may
legitimately differ (inode numbers, block counts, which optional syscalls exist) is **noted**
instead, and listed under the target's `accepted` line. The choice is made in the test body, so
the accepted differences are visible where they are accepted. Prefer `record`; use `note` only
for something a network filesystem cannot do.

**An account run** (no mode):

1. Runs the reference.
2. Runs the contract in a new `pdfs-acceptance-<id>` folder in My files, and at the end reads back
   a file the run wrote.
3. Registers two sync folders under `~/.cache/pdfs-acceptance/run-<id>/` and runs the contract and
   the move cases in every pairing of on-demand and mirror. Before a folder switches to on-demand,
   a new file is written into it; once mounted, the file must be listed with its bytes. In every
   pairing each folder must still hold the file written into it at setup.
4. Removes everything it made, locally and on Drive, and reports `cleanup`. A run that leaves
   anything behind fails.

Cleanup finds remote folders by uid, never by name, so nothing of yours is touched. It removes
each sync folder with `sync rm --delete-remote`, trashes the My files folder, deletes both from
the trash for good, and checks that a fresh trash listing no longer shows them. Queued ops that
still name one of its folders count as left behind. Cleanup also runs on a failure, Ctrl-C,
`SIGTERM` or `SIGHUP`; while it runs, two more interrupts are ignored and a third abandons it.

Each run writes what it is about to create to `~/.cache/pdfs-acceptance/run-<id>/manifest.json`
first and holds a lock on that directory. A later run finishes the cleanup of a run that died, and
removes stray `pdfs-acceptance-<32 hex>` folders no live run claims: in My files (older than
`PDFS_ACCEPTANCE_REAP_AGE`, default one hour), under the acceptance home, and in the trash. It
does not start while anything is left.

Ops that were queued before the run started are not the run's: it prints one `NOTE` line naming
them, and its queue waits ignore them.

**`--live PATH...`** runs the contract on mounts you set up, and leaves the sync configuration
alone. It works only below a new `pdfs-acceptance-*` directory, refuses paths that are not FUSE
mounts, and removes its directory afterwards. The first path must be a FUSE mount; later paths may
be another mount or a mirrored folder. If all paths show the **same remote folder**, set
`PDFS_ACCEPTANCE_CONVERGENCE=1` to also check that bytes written through the first appear through
the others.

**`--managed-live A B`** takes two existing, empty, unmounted directories, registers them as sync
folders, and runs every pairing as an account run does, then deletes the remote folders. It
refuses non-empty paths, mounts, and paths already in `pdfs sync list`.

### Reading the output

A run opens with the mode, the `pdfs` binary and version, Python, the kernel, the per-case limit
and the options. Each target then names its root, the storage behind it, the daemon and the
number of cases, followed by one line per case:

```
==> live FUSE /mnt/testmount
  -> root      /mnt/testmount/pdfs-acceptance-…
  -> storage   FUSE mount /mnt/testmount (fuse.protondrive)
  -> daemon    pdfs 3.0.0, user@proton.me, online, 0 queued ops
  -> cases     45 of 45
  [ 1/45] creation and open flags .................................. ok       0.41s
  …
  [21/45] throughput floors ........................................ ok      48.20s
          write 31 MiB/s, read 54 MiB/s, 4 KiB writes 12.0 MiB/s, metadata 9 ops/s
  …
  -> reference matches the ordinary filesystem in 412 compared observations
  -> result    44 passed, 1 skipped in 11m02s
```

A case ends as `ok`, `FAIL`, `TIMEOUT`, `skip` (it does not apply to the target), or `known` (an
`open B<n>` case reproduced a bug that is still open). Dimmed lines under a case hold its details.
A failed `regression B<n>` case points to its entry in [BUGS.md](BUGS.md).

The run ends with a table of every target, the report paths, the slowest cases, cases over
`--budget`, known issues that still reproduce, errors the daemon logged, and each failure. The
last line is `PASS:` or `FAIL:` with the totals. Colour is used only on a terminal and never with
`NO_COLOR` set.

### What the cases cover

Besides the syscall contract:

- **Block boundaries.** Files from 0 bytes to two 4 MiB blocks plus 3 bytes, every size next to a
  boundary, and overwrites that grow, shrink and reshape them.
- **Unusual names.** 255 bytes, NFC and NFD forms of the same text, newline, tab, emoji,
  characters Windows forbids, leading and trailing spaces, a case-only rename.
- **A deep tree and a wide folder.** 24 levels renamed at the top; 128 entries created
  concurrently, unlinked concurrently, then `rm -rf`.
- **Rename patterns.** Swaps, chains, log rotation, replacing a file under an open reader, the
  `EISDIR`, `ENOTDIR` and `ENOENT` refusals.
- **Handle coherency.** Unsaved writes seen by a second handle, two `O_APPEND` writers, 30 rewrite
  generations, a truncate under a reader.
- **Throughput.** 64 MiB written and read, 1 MiB in 4 KiB writes, metadata on 100 files. The case
  fails below `PDFS_ACCEPTANCE_MIN_MIBPS` (default 10) or `PDFS_ACCEPTANCE_MIN_OPS` (default 20
  operations per second, 3 on a FUSE mount). `0` turns a floor off.
- **Operations the filesystem does not implement.** `git`, `rsync`, `cp -a`, `tar` and installers
  probe for symlinks, hard links, FIFOs, device nodes, locks, `O_DIRECT` and `SEEK_HOLE`. Each must
  work or be refused cleanly: a known errno, never `EIO`, never a hang.
- **Moves between locations** (`managed` in `--list`). Trees moved between two sync folders in
  both directions, several sources in one `pdfs move`, and a tree through My files and out again.
  The refusals must move nothing and lose nothing: a name already taken, a folder into itself, a
  mirrored source holding something Drive lacks, a file still open for writing.
- **The CLI.** `pdfs` into a closed pipe exits 0 with nothing on stderr; `pdfs mkdir`, `rename`
  and `rm` by local path, and their refusals.

### Regressions and open bugs

A case named `regression B<n>` reproduces an entry of [BUGS.md](BUGS.md) so it stays fixed. Most
need a live mount and a reachable daemon, and are skipped without one. `--list` prints every case
and the targets it runs on.

A regression case checks the outcome **on Drive**, so it waits for the queue first. Reading back
through the mount proves nothing: the local caches return the bytes just written whether or not
they reached Drive. A write is queued when the kernel releases the file, after `close(2)` has
returned, so a case that reads `pdfs sync queue` right after writing waits for the ops it expects.

A case named `open B<n>` checks a bug that is still open. While it reproduces, the case ends as
`known`: listed at the end, but not a failure. When it passes, the bug is fixed: rename the case to
`regression B<n>` and make it fail with `check`. No case is open at the moment.

Some cases need what a test account cannot provide alone and skip without it: B34 needs a folder
shared read-only by a **second account**, B34b one shared with editor rights, and B79 an on-demand
synced folder.

### Timeouts, reports and hung mounts

A hung FUSE call cannot be interrupted from Python. Each case runs under a soft alarm (`--timeout`,
default 180 s, or `PDFS_ACCEPTANCE_TIMEOUT`) that reports the case and lets cleanup run, backed by
a hard stop that dumps every thread's stack and exits. After a timeout the run checks whether the
mount still answers a `stat` and a listing within 15 s. If it does, the target goes on; if not, its
remaining cases are skipped. Cases that make many round trips by design get twice the limit.

| Option | Effect |
|---|---|
| `--report-json PATH` | Every result and recorded observation. Attach it to a bug report |
| `--report-junit PATH` | JUnit XML. `known` is reported as skipped |
| `--budget SECONDS` | Flags cases that pass but take longer (`PDFS_ACCEPTANCE_BUDGET`) |
| `--journal-check` | Fails the run if `proton-drive.service` logged an error during it, or the warnings "queued write conflicts; keeping a conflict copy" and "a fuse worker has held the same job for a long time" |
| `--durability` | Restarts the service mid-suite and checks the bytes written before it. Proves the queue survives a restart. `PDFS_ACCEPTANCE_UNIT` names another unit |
| `--fail-fast` | Stops at the first failure |
| `--list` | Prints every case and its targets |

### Selecting cases

`PDFS_ACCEPTANCE_ONLY` runs the cases whose name contains the text, ignoring case. Separate several
with commas (`PDFS_ACCEPTANCE_ONLY=B116,B117`). A filter that matches no case at all fails, as the
typo it is. A release run leaves it unset.

| Variable | Effect |
|---|---|
| `PDFS_ACCEPTANCE_PDFS` | The `pdfs` binary to use, for example `target/debug/pdfs` |
| `PDFS_ACCEPTANCE_SYNC_TIMEOUT` | Seconds a sync folder may take to settle after a change of mode |
| `PDFS_ACCEPTANCE_REAP_AGE` | Age in seconds after which a stray `pdfs-acceptance-*` folder is removed (default 3600) |
| `PDFS_ACCEPTANCE_CONVERGENCE` | `1` checks that several `--live` paths show the same bytes |
| `PDFS_ACCEPTANCE_MIN_MIBPS`, `PDFS_ACCEPTANCE_MIN_OPS` | Throughput floors |
| `PDFS_ACCEPTANCE_UNIT` | The systemd unit `--durability` restarts |

## Before a release

1. The quality gates and the simulation runs pass in CI.
2. A full account run with `--journal-check` passes on a test account:
   `scripts/fuse-acceptance.sh --journal-check`.
3. Fixes marked *unverified* in [BUGS.md](BUGS.md) that a run now covers are marked verified.

What no automated run covers yet is listed under
[Verification still owed](ROADMAP.md#verification-still-owed).
