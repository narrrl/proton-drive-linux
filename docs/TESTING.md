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

Kernel/FUSE behavior and remote convergence need a real mount. Live testing is
explicitly opt-in and always runs the account-free contract first. It runs
destructive POSIX operations only below a fresh `pdfs-acceptance-*` directory.
It refuses paths that are not FUSE mounts and removes its directory on success,
failure, or interruption.

Use a dedicated test account without irreplaceable data:

```console
scripts/fuse-acceptance.sh --live /mnt/on-demand-testmount
```

### The mount is diffed against an ordinary filesystem

The local reference run is not only a self-test of the runner. Every case
records the facts it established, and a live run is compared against those
recordings, so a case fails when the mount *differs* from an ordinary
filesystem — not only when it raises. The report names the exact key:

```
  [DIVERGENCE] live FUSE /mnt/testmount does not match the reference filesystem:
    ! creation and open flags.truncated.size: live=6 reference=0
```

Facts are recorded in one of two ways, chosen in the test body:

- **compared** — a claim about filesystem semantics. The mount must agree.
- **noted** — context that legitimately differs: inode numbers, block counts,
  reported mode bits, and which optional syscalls exist at all. Reported under
  `[capabilities]`, never a failure.

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

### Timeouts, reports, and hung mounts

A wedged FUSE operation cannot be interrupted from Python, so each case runs
under a soft alarm (`--timeout`, default 180s) that reports the case and lets
cleanup run, backed by a hard stop that dumps every thread's stack and exits.
That backstop is the only way to learn *where* a mount hung. A timeout ends the
run: later cases against a wedged mount produce noise, not information.

```console
scripts/fuse-acceptance.sh --live /mnt/testmount \
  --timeout 300 --report-junit results.xml --budget 30
```

`--report-json` also writes every recorded observation, which is what to attach
to a bug report. `--budget SECONDS` flags cases that pass but got slower —
B5 was a 186 ms-per-call regression that a pass/fail suite could not see.
`--list` prints every case and which targets it runs against, and `--fail-fast`
stops at the first failure.

`--journal-check` fails the run if `proton-drive.service` logged any error while
it ran. A suite can pass every assertion while the daemon logs failures behind
it; that has happened, and it was caught only by reading the journal by hand.

Abandoned `pdfs-acceptance-*` roots from a crashed run are removed at startup,
but only if they match the exact generated name and are over an hour old
(`PDFS_ACCEPTANCE_REAP_AGE`), so a concurrent run is never disturbed.

### Automated mode matrix

The managed live runner accepts two existing, empty, unmounted directories. It
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

### Durability drill

`--durability` restarts `proton-drive.service` in the middle of the suite and
re-verifies the bytes written before it. `staging/` and `recovery/` can hold the
only copy of a file whose upload has not finished, so this is the case that
proves the queue is persistent rather than in-memory. It is opt-in because it
interrupts a running daemon; `PDFS_ACCEPTANCE_UNIT` selects a different unit.

### Selecting cases

During development, `PDFS_ACCEPTANCE_ONLY` runs tests whose descriptive name
contains the supplied text (for example `PDFS_ACCEPTANCE_ONLY=namespace`). A
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
