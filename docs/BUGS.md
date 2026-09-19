# Bugs & side findings

Running tracker for bugs and loose ends spotted while working on something else —
so they don't get lost when the session that found them ends.

Conventions:

- **Open** / **Fixed (unverified)** / **Fixed** / **Won't fix**
- "Fixed (unverified)" means the code change is in but nobody has driven the real
  flow against it yet. It stays that way until someone does.
- Record how it was *found*, not just what it is. The repro is the expensive part.

---

## B91 — The daemon hung overnight, and nothing could say what it was waiting on

**Status:** Open — the hang itself is undiagnosed. What is fixed (unverified) is the
*undiagnosability*: the daemon now reports what it is doing, warns when it stops making progress,
and is restarted by systemd when it stops answering.
**Found:** 2026-09-18, from a user report — the mount stopped responding and only came back after
`systemctl --user restart`.
**Where:** `crates/pdfs-fuse/src/{workers.rs,diagnostics.rs,supervisor.rs,systemd.rs,mount.rs}`,
`packaging/proton-drive.service`.

**Symptom.** The mount stops answering. The process is alive and is not spinning; there is no
panic, no error, and no log line after the moment it stopped. The accounting for the hung run:

```
Consumed 43min 39.045s CPU time over 20h 8min 15.733s wall clock time,
5.2G memory peak, 65.7M memory swap peak
```

It went silent at 17:10 and stayed that way until the manual stop at 13:18:54 the next day. A
restart picked everything back up, which rules out corrupt local state.

**What made it undiagnosable.** Nothing in the daemon could be asked what it was doing:

- the worker pool published no state at all — not which threads were busy, not with what, not for
  how long, not how deep the queues were;
- control requests were not tracked, so a handler that never returned looked exactly like a
  handler that was never asked for;
- there was no watchdog, so a daemon that stopped answering stayed "active (running)" for twenty
  hours;
- resident size was never sampled, so the 5.2 G peak was only visible in systemd's post-mortem.

**Related, and a real bug on its own.** At 2026-09-18 09:58:35 a meta worker panicked during
shutdown:

```
thread 'pdfs-fuse-meta-0' (3044) panicked at ...:
A Tokio 1.x context was found, but it is being shutdown.
```

The pool was flagged closed at teardown but never joined, so the process could drop the tokio
runtime while a worker was still inside a job. `catch_unwind` caught it, which is why it was only
ever a log line — but it is teardown ordering, not an error.

**What changed.**

- `Workers` publishes per-thread state (busy, job label, age) plus queue depths and per-lane
  completion counts, all outside the queue lock, and every FUSE handler labels the job it hands
  over.
- `Request::Diagnostics` / `pdfs diagnostics` reports that, the in-flight control requests with
  their ages, the handler count against its limit, resident size and the pending-op count. It is
  built to answer while the daemon is wedged: atomics and `try_lock` only, with the one database
  read on a throwaway thread behind a 500 ms deadline.
- A supervisor thread WARNs when a worker has held one job for 120 s, when a control request has
  run that long, or when a lane has a queue but completed nothing since the last tick; it samples
  RSS every 5 minutes.
- The supervisor answers the systemd watchdog, but only after a real round trip over the control
  socket — so the ping means "a client can still talk to this daemon" rather than "a thread is
  still scheduled". `WatchdogSec=120` and `Restart=always` turn the next hang into a restart
  instead of a morning of downtime, and `MemoryHigh=2G`/`MemoryMax=6G` bound the growth.
- Teardown joins the worker pool, with a 10 s deadline, before the caller drops the runtime.

**Still open.** None of this says *why* it hung. The candidates the evidence does not yet separate:
the 5.2 G working set (a leak, or a legitimately huge listing retained), a lock held across an
await somewhere in the control plane, and the ~150 `rt.block_on` sites on the control path that
have no op-level deadline of their own — each individual HTTP call is bounded by the SDK's 30 s
API / 300 s storage timeouts, but a sequence of them is not. The next occurrence should be a
one-line answer from `pdfs diagnostics` or from the stall WARN.

---

## B90 — The local-file indexer walks into foreign FUSE mounts, and a dead one makes the daemon unkillable

**Status:** Fixed (unverified) — the walker no longer stats a nested mountpoint. The stuck-daemon
half cannot be fixed in userspace at all; see "Why there is no second line of defence".
**Found:** 2026-09-16, from a user report of `systemctl --user restart` failing in a loop.
**Where:** `crates/pdfs-core/src/localindex.rs` (`default_excludes`, `scan`),
`crates/pdfs-fuse/src/background.rs` (`scan_local_once`).

**Symptom.** Stopping the service times out, SIGKILL does not help, and every subsequent start
fails with `another Proton Drive daemon is already using .../cache.db`:

```
systemd[2196]: proton-drive.service: State 'final-sigterm' timed out. Killing.
systemd[2196]: proton-drive.service: Killing process 2221 (pdfs) with signal SIGKILL.
systemd[2196]: proton-drive.service: Processes still around after final SIGKILL. Entering failed mode.
systemd[2196]: proton-drive.service: Unit process 2221 (pdfs) remains running after unit stopped.
pdfs[73921]: ERROR mount failed; retrying in 5s error="open cache db: another Proton Drive daemon
             is already using /home/narl/.local/state/proton-drive-linux/cache.db"
```

**Diagnosis.** The surviving process is a zombie that still has a live thread:

```
$ cat /proc/2221/status | grep -E 'State|Threads'
State:  Z (zombie)
Threads: 2
$ cat /proc/2221/task/73231/comm; cat /proc/2221/task/73231/stack
pdfs-localindex
request_wait_answer
```

`pdfs-localindex` is parked in a FUSE request nobody will answer, in uninterruptible `D` state, so
it ignores SIGKILL. The thread group leader exits, the thread does not, the process cannot be
reaped — and it keeps the `cache.db` lock, which is what turns one stuck scan into a permanent
restart loop. The same dead connection also blocked an unrelated `duf` in `fuse_statfs`
(`/sys/fs/fuse/connections/69/waiting` was 2).

**Cause.** `scan_local_once` walks `$HOME`, and `default_excludes` only excluded the Drive
mountpoint plus our own state and cache dirs. The reporter's home also held
`~/remote/narl.io` (sshfs) and `~/remote/gdrive` (rclone) — third-party FUSE mounts the walk
descends into.

`scan` does set `same_file_system(true)`, which looks like it should already cover this. It does
not: in `ignore` 0.4.33 the check is `is_same_file_system` → `device_num(path)` →
`path.metadata()` (`walk.rs:2164`), i.e. **a stat of the mountpoint**, performed in `run_one`
*after* the directory has been queued. To discover that a mount is foreign, the walker first has
to touch it. Against a network FUSE mount whose server is gone, that stat never returns.

**Fix.** `localindex::nested_mount_points` reads `/proc/self/mounts` and returns every mountpoint
strictly below the scan root; `scan_local_once` appends those to the excludes. `filter_entry` runs
before the device stat (`walk.rs:1926`, before `send`), so a path-only rejection keeps the walker
from ever touching the mount. Mountpoint fields are octal-unescaped, because a `\040` left in
place would silently fail to match and put the mount back in the walk. Re-read per scan, not
cached at startup: a mount that appeared since the daemon started is the likeliest to be a
half-alive network filesystem.

This changes nothing about what gets indexed. A nested mount is a different device, so
`same_file_system(true)` was always going to skip it — the exclude only removes the stat that had
to happen first.

**Why there is no second line of defence.** Once a thread is in uninterruptible FUSE wait, no
userspace change rescues it: not a join timeout, not `std::process::exit`, not SIGKILL. The kernel
will not reap the task until the syscall returns, which needs someone to answer the request or
abort the connection (`echo 1 > /sys/fs/fuse/connections/<id>/abort`, root-only). Avoiding entry is
the entire fix. That makes any *new* code path that stats a user-supplied or mount-crossing path
from a daemon thread a repeat of this bug — mirror-folder scanning is the obvious next candidate.

**Recovery for an already-wedged daemon.** As root, abort the connection with waiters
(`grep . /sys/fs/fuse/connections/*/waiting` finds it), which releases the thread, lets the zombie
be reaped and frees the `cache.db` lock. A reboot also clears it.

---

## B89 — The recorded SDK dependency is three minor versions out of date in the working memory

**Status:** Open — documentation only, but it misdirects every reader of the SDK code.
**Found:** 2026-08-17, while checking B85's evidence against the reader implementation.
**Where:** `proton-dev/CLAUDE.md` (the container-level file, not in this repo).

`CLAUDE.md` states that `proton-drive-linux/Cargo.toml` declares `proton-sdk = "0.1"` /
`proton-drive-rs = "0.1"` and that `Cargo.lock` pins **0.1.11** from the registry, against a local
`proton-sdk-rs/` at 0.1.12. Neither half is true any more: the lock pins **0.6.0**, bumped by
1.7.1 (see its changelog entry, "Workspace deps bumped to `proton-sdk` / `proton-drive-rs` 0.6.0")
and never reflected in the working memory.

**Why it matters beyond tidiness.** The whole point of that section is to tell you which source
you are reasoning about when you read SDK code. Verifying B85 meant establishing what `read_at`
does in the version the client actually builds against; following `CLAUDE.md` sends you to
`proton-drive-rs-0.1.11` in the registry cache, and to a local checkout described as one patch
ahead of it when it is in fact five minor versions behind. The conclusion happened to be the same
in every version — but that was luck, and the next question of this shape may not be.

**Fix:** correct the versions, and prefer describing where to *look* (`grep -A2 'name =
"proton-drive-rs"' Cargo.lock`) over restating a number that goes stale on every dependency bump.

---

## B88 — `pdfs pin`, `rename`, `move`, `delete` and sharing still cannot name a path under a secondary mount

**Status:** Open — B86's fix covers `ls` and `refresh` only.
**Found:** 2026-08-17, while fixing B86.
**Where:** `crates/pdfs-fuse/src/control.rs`, every handler still calling `rel_to_mount`.

B86's second half added `Core::rooted_at` and `control.rs::route_to_mount`, which resolve a path
against whichever mount the daemon owns it under and answer the request in that mount's inode
space. Only `ListDir` and `RefreshScope::Dir` were routed through it, because those two were what
the B86 repro needed: an escape hatch for a folder serving a stale listing.

Every other path-addressed control request — `Pin`, `Unpin`, `Rename`, `Move`, `Delete`,
`CreateFolder`, `OpenFile`, the sharing and public-link requests, `ListRevisions` — still uses the
primary-only `rel_to_mount`, so an absolute path inside a secondary on-demand mount is rejected
with "is not under the mountpoint". The user can now *see* such a folder's contents and cannot act
on them by path.

**Why it was left.** Routing a mutating request is not the same change as routing a read. Each of
those handlers resolves an inode, checks access, performs a remote operation and then invalidates
local state, and several of them invalidate `self.state()` — the primary — explicitly. Routing
them means auditing each for which mount's state it is really talking about, which is a larger and
riskier change than the one B86 needed. Doing it half-way would be worse than not doing it.

**Required fix/test:** move the remaining handlers onto `route_to_mount`, auditing each for
per-mount state assumptions as it goes; the `for_each_state`/uid-keyed helpers are the pattern for
the ones that touch more than their own inode space. Test: a `pdfs pin` and a `pdfs rename` by
absolute path inside a secondary on-demand mount, plus a path under no mount still erroring.

---

## B87 — A revision's block geometry is learned a read too late

**Status:** Open — a bounded, self-correcting cost, accepted deliberately in 1.8.2.
**Found:** 2026-08-17, while fixing B85. This is the residue that fix leaves.
**Where:** `crates/pdfs-fuse/src/reads.rs` (`Core::block_geometry`, `read_block`).

B85's fix takes block boundaries from the revision instead of assuming 4 MiB. The geometry comes
from `RevisionReader::block_sizes()`, and a reader is expensive to open — link details, ancestor
keys, an S2K node-key unlock and the block table, which is exactly the per-read work the block
cache exists to avoid (B12). So the geometry is *learned* as a side effect of the first read,
which has to open a reader anyway, and written to a `<key>.geom` sidecar for later reads.

The first read of a file therefore still plans on `BlockGeometry::uniform`. On a file whose blocks
are not 4 MiB that plan is wrong, and it costs exactly one straddling fetch: the reader fetches
two of the revision's blocks to answer for one client block, and the block written to the cache is
at an offset the *next* read — now planning correctly — will not ask for, so it ages out unused.
Every read after the first is right.

**Why it is not being fixed now.** The alternatives are all worse than the cost. Resolving a
reader before planning reintroduces B12's per-read key derivation on every file. Persisting the
geometry at upload only covers files this client wrote, which are the ones the fallback is already
correct for. Fetching the block table alone still costs the node-key unlock.

**Worth doing if:** non-4-MiB revisions turn out to be common on real accounts rather than a
handful of files on one, or if the block table becomes reachable without unlocking the node key.
Measure before deciding — one wasted fetch per file per revision is not obviously worth paying
anything to remove.

---

## B86 — An on-demand mount serves a listing that predates the mirror engine's own uploads

**Status:** Fixed in code (2026-08-17); live re-run of the repro pending.
**Found:** 2026-08-16, while trying to drive B42's remote-delete path from one machine.
**Where:** `crates/pdfs-fuse/src/sync.rs` upload path (no listing invalidation) meets the cached
children snapshot the on-demand mount enumerates from.

**Repro.** With folder 6 (`~/pdfs-live-mirror`) in mirror mode: create `c1.txt` and `c2.txt`,
`pdfs sync now 6` — the activity feed reports `c1.txt new file`, `c2.txt new file` and
`sync_entry` gains rows carrying real `remote_uid`s. Switch the folder to on-demand and list it:

```
$ ls ~/pdfs-live-mirror
b25.txt
b40.bin          # c1.txt, c2.txt and the conflict copy are missing
```

`ls ~/pdfs-live-mirror/c1.txt` returns `ENOENT`. Restarting the daemon does not help — the stale
children snapshot is in the database, not in memory. Switching the folder back to mirror proves
the files were on the server the whole time: the pass downloads all five, and `b40.bin` comes back
md5-identical.

**Effect.** No data loss — the mirror side is correct and the files are intact remotely — but a
user who switches a synced folder to on-demand sees an incomplete folder, and anything that walks
it (a backup, an indexer, `rm -r`) sees the same incomplete folder. It also blocks B42's live
verification, because `pdfs rm` cannot name a file the mount will not admit exists.

**Second, smaller gap in the same repro:** `pdfs refresh` and `pdfs ls` reject any path that is
not under the *primary* mountpoint (`Error: … is not under the mountpoint`), so a secondary
on-demand mount has no manual escape hatch either.

**Required fix/test:** invalidate (or update) the parent's cached children when the sync engine
creates a remote node, the same way the FUSE write path does; and let `refresh`/`ls` resolve a
path under any mount the daemon owns, not just the primary. Test: the repro above, asserting the
new files appear immediately after the mode switch.

### Fix (2026-08-17)

Two halves, matching the two halves of the report.

**The stale listing.** `Core::invalidate_children_of` (`pdfs-fuse/src/lib.rs`) drops a folder's
cached children *by uid*. The existing `invalidate_parent_listing` could not serve here for two
reasons: it names the folder by inode, and a mirror folder has no inode space at all at the moment
the engine writes to it; and the listing that outlived the pass was not a hot-cache map but the
`listed` flag on the folder's DB row — which is why restarting the daemon did not help. The new
helper clears that flag unconditionally and then invalidates whichever mounts happen to hold the
folder resident. The sync engine calls it after every remote mutation it makes: folder creation
and file upload (`apply_one`, `upload_new`), and the three remote-trash sites, whose parent uid
comes from the pass's folder-uid map or the baseline (`parent_uid_of`).

**The missing escape hatch.** `StateRegistry` now carries each mount's `size_upgrades` map as well
as its state, notifier and liveness flag — that set is exactly the per-mount half of a `Core`
(everything `fork_state` replaces) — and `StateRegistry::covering_parts` returns it for the mount
that most specifically covers a path. `Core::rooted_at` swaps those fields into a clone of the
`Core`, giving a view rooted at the owning mount plus the path relative to *its* root. `pdfs ls`
and `pdfs refresh` route through it (`control.rs::route_to_mount`), so a secondary on-demand mount
can be named at last; a path under nothing this daemon has mounted still gets the familiar "is not
under the mountpoint" error. The other path-addressed control requests deliberately still use the
primary-only `rel_to_mount` — routing the mutating ones is a larger change and was not what
blocked the repro.

Test: `covering_parts_route_a_path_to_the_mount_that_owns_it` covers the routing rule (nested
mount wins, suffix relative to it, its own inode space and upgrade batches, a path under no mount
routes nowhere). The listing-invalidation half is the live repro, which is what is still pending.

---

## B84 — A block read that comes back short truncates the file, silently

**Status:** Fixed and **live-verified 2026-08-16**. **Data correctness** — an application reading a large
file through the mount could get a truncated copy that reported success. The truncation is now
impossible: a disagreement is repaired from the whole-file download, or the read fails `EIO`.
Re-verified on 1.8.1 after the release build: `118454731_p0.png`, `121677947_p0.png`,
`123349685_p0.png` and `128451683_p0.png` each delivered exactly their `stat` size
(19471492, 11811519, 22055175, 11393178 bytes) with no `Invalid argument` reply in the journal.
**Found:** 2026-08-16, chasing what looked like log noise. GNOME's file indexer
(`localsearch-3`) was filling the journal with `libpng error: Read Error`,
`libpng error: IDAT: CRC error` and `File too small to be a PNG` on files under
`~/ProtonDrive/Pictures`, interleaved with 49 `ERROR fuser::reply: Failed to send FUSE reply:
Invalid argument (os error 22)` from the daemon in one afternoon. The indexer was right and we
were wrong.

**Where:** `Core::read_block` (`crates/pdfs-fuse/src/reads.rs:566`), the assembly loop in
`Core::read_range_remote` (`reads.rs:759-769`); upstream, `RevisionReader::read_at` /
`splice_block` and `rank_block_sizes` (`proton-drive-rs` 0.6.0, `revision.rs`,
`transport.rs:256`).

**Repro.** Deterministic per file, no account state needed beyond the files themselves:

```bash
stat -c %s FILE            # what the mount reports
cat FILE | wc -c           # what the mount can actually deliver
```

| file | `nodes.size` / `stat` | bytes readable |
|---|---|---|
| `Pictures/old/92433593_p0.png` | 4317497 | 4194304 (tail block yields **0**) |
| `Pictures/old/79779657_p0.png` | 11574982 | 10567052 |
| `Pictures/old/2022-07-09_04-53.png` | 6388959 | 4389310 |

`pdfs pin` on the third file, then re-read: **6388959 bytes, a fully valid PNG** — every chunk
CRC passes, IEND is reached, the IDAT stream inflates to 44.8 MB. So the remote data is intact,
`nodes.size` is right, and the whole-file (manifest-verified) download path is right. Only the
block/range path truncates. Pinning is the workaround.

**Root cause.** `read_block` asks the SDK for `blen = BLOCK_SIZE.min(fsize - bstart)` bytes and
never checks how many it got:

```rust
let bytes = reader.read_at(bstart, blen).await.map_err(...)?;   // reads.rs:577
```

`RevisionReader::read_at` clamps the range to its own `file_size` — summed from the *believed*
per-block sizes `rank_block_sizes` resolved — and `splice_block` clamps again to
`plaintext.len()`. Both are silent. A short block then vanishes in the assembly loop: with
`block.len()` short, `e = (end.min(bstart + block.len()) - bstart)` collapses, and for an empty
block `s < e` is false, so nothing is appended at all. The FUSE read returns fewer bytes than
asked for, the kernel hands userspace a short read, and every tool reads a short read as EOF.
The `EINVAL` reply errors are the same mismatch seen from the other side.

Nothing on this path validates a block against the length the file's own size implies
(`block_len(fsize, bidx)` — which the *cache* does check, in `ContentCache::cached_block`, and
which is why a short blob on disk is rejected and refetched while a short blob from the network
is served).

**Not yet known:** which rung of `rank_block_sizes` these revisions land on. The three
reader-vs-node deltas fit no constant ratio, so this is per-revision metadata rather than block
padding — the terminal `block_count <= 1` fallback (`vec![4 MiB; block_count]`) would explain
the file that stops at exactly 4 MiB but not the other two. Needs a live probe printing
`RevisionReader::block_sizes()` against `nodes.size` for these three uids. That answer decides
whether the real fix belongs in this repo or in `proton-sdk-rs`.

**Fix (2026-08-16).** `Core::read_block` no longer trusts the range path blindly.

1. **The disagreement is caught before any bytes are read.** If `reader.size() != fsize`, the
   revision's block table does not describe this file and *no* block of it can be trusted — not
   just the short ones. Sizes that overstate a block shift every block after it and return
   full-length reads of the wrong bytes, which no length check would ever catch (the SDK's
   `rank_block_sizes` documents exactly this failure). So the whole file leaves the range path.
2. **A short block is caught as a second net,** compared against `block_len(fsize, bidx)` — now
   `pub` in `pdfs-core::cache`, because it is the contract the read path shares with the cache
   rather than a detail of the cache. The assembly loop in `read_range_remote` re-checks the same
   thing and fails `EIO` rather than contributing fewer bytes than the range it covers, which is
   the line that actually turned a bad block into a truncated file.
3. **A file that fails either check is repaired, not just refused.** `Core::repair_block` pulls
   the whole revision through `download_file_to` — the manifest-verified path, which does no
   boundary arithmetic and is why pinning worked — into a scratch file, checks it against
   `nodes.size`, and adopts it as the cached blob via `store_file`. Every later read of the file
   is then answered by `ContentCache::read_range` before the block path is consulted at all. One
   download per file, not per block: `content_repairs` holds a lock per file so the concurrent
   block reads that all discover the same bad table share it. Bounded by `REPAIR_MAX` (512 MiB) —
   past that one unlucky read would pull down gigabytes, so it fails `EIO` and says to pin.

Nothing short is cached or promoted: `read_block` only reaches `store_block` and the `block_ring`
insert with bytes that passed. `store_block` itself deliberately does *not* check — its fixtures
store 4 KiB "blocks" of notional 1 GiB files to keep the budget tests cheap, and the read side
(`cached_block`, `has_block`) already rejects a wrong-length block as a miss. Validation is the
reader's job, at the point of fetch.

`getattr` needs no change after all: `pdfs pin` proved `nodes.size` is the correct one and the
reader's belief is the wrong one, so the fix is to stop serving the reader's belief — not to
publish it through `stat`.

**Verified live (2026-08-16).** Four multi-block PNGs under `Pictures/wallpaper/` were read whole
through the mount on the running 1.8.1 daemon; every one delivered exactly its `stat` size
(5558846, 1098729, 2015463, 2291122 bytes), and no `fuser::reply … Invalid argument` appeared. The
journal shows the second net firing and the repair path serving each of them:

```
WARN block came back shorter than the file's size implies bidx=0 blen=4194304 got=4939506 …
INFO served a short block from a repaired whole-file download bidx=0 …
```

**Answered, and not as expected.** The open question was which rung of `rank_block_sizes` these
revisions take. The evidence says the reader is not the confused party: `reader.size()` equals
`fsize` in every case — the gate in step 1 never fires — and the blocks it returns are *larger*
than 4 MiB, not smaller (4939506 for a whole 4.9 MB file; 5239804 and 5670228 as first blocks).
This client's fixed 4 MiB block geometry is the thing that is wrong. Filed as **B85**; the repair
path above is what keeps those files readable in the meantime, at the cost of a whole-file
download on first read.

## B85 — The read path assumes 4 MiB blocks; some revisions do not have them

**Status:** Fixed in code (2026-08-17) — the geometry now comes from the revision. See
"Correction to the diagnosis" below: the premise that this was *losing* data does not survive
reading the SDK, and the fix is a cost fix, not a correctness one.
**Found:** 2026-08-16, while live-verifying B84 (see its journal evidence)
**Where:** `crates/pdfs-fuse/src/reads.rs`, `crates/pdfs-core/src/cache.rs` (`BLOCK_SIZE`,
`block_len`)

**Cause:** The whole range path is built on one assumption — that a revision is a series of
`BLOCK_SIZE` (4 MiB) blocks with a remainder at the end. `block_len(size, idx)` encodes it, the
on-disk block cache is keyed by that index, and `read_block` asks the SDK's `RevisionReader` for
`[idx * BLOCK_SIZE, block_len)`.

Some revisions on this account are not laid out that way, and B84's diagnostic caught it in the
act. The warning it logs reads "shorter", but the numbers say the opposite:

```
bidx=0 blen=4194304 got=4939506 fsize=4939506     # one block holding the whole file
bidx=1 blen=745202  got=0       fsize=4939506     # …so there is no block 1
bidx=0 blen=4194304 got=5239804 fsize=11537412    # first block is ~5.0 MiB, not 4
bidx=1 blen=4194304 got=5670228 fsize=11106988    # …and they are not even uniform
bidx=2 blen=4194304 got=5233034 fsize=19932790
```

`reader.size()` agrees with `fsize` in every one of these, so B84's primary gate — the one that
catches a block table that does not add up — correctly does *not* fire. The file is intact and
the reader knows its real boundaries; it is this client that is asking for the wrong ones.

**Why it is not currently losing data:** B84's length check catches every one of these and routes
the file through the whole-file manifest-verified download, so reads are correct. The cost is
real, though: any file with non-4-MiB blocks is downloaded whole on first read, however few bytes
were asked for, and one above `REPAIR_MAX` (512 MiB) fails `EIO` and can only be read by pinning
it.

**Fix direction:** take the geometry from the revision instead of assuming it.
`RevisionReader::block_sizes()` already exposes it and B84 already logs it. That means block
offsets become a prefix sum over the real sizes rather than `idx * BLOCK_SIZE`, and — the part
that makes this its own change rather than a patch — the on-disk block cache is keyed and
length-validated on the 4 MiB assumption too (`block_len`), so its key scheme has to follow the
file's own geometry or the cache has to hold ranges rather than blocks.

**Open question:** what produced these revisions. Uniform-but-not-4-MiB would suggest another
client's chunk size; these are *not* uniform (5239804, 5670228, 5233034), which looks more like a
size recorded per block after some transformation. Worth answering before choosing the fix, since
"blocks are whatever the revision says" and "blocks are a different constant" lead to different
designs.

### Correction to the diagnosis (2026-08-17)

**The logged numbers cannot have come from `read_at`, and the "this client asks for the wrong
range" reading is wrong.** `RevisionReader::read_at(offset, length)` does not assume any geometry:
it computes `end = min(offset + length, file_size)`, plans over the *real* block sizes
(`plan_blocks`), and splices each block clamped to `[offset, end)` (`splice_block`). Both are
present and identical in the version the client actually builds against — `Cargo.lock` pins
`proton-drive-rs` **0.6.0**, not the 0.1.11 the top-level `CLAUDE.md` still describes — and in
0.1.11 as well. So a `read_at(bstart, blen)` can return at most `blen` bytes, and every quoted
line has `got > blen`:

```
bidx=0 blen=4194304 got=4939506     # got exceeds the length requested
bidx=1 blen=4194304 got=5670228     # …as does this one
```

The only site in the client that logs this shape (`reads.rs`, the `blen`/`got`/`fsize` warning)
reaches it solely on the `bytes.len() != blen` branch of a `read_at(bstart, blen)`. Those two
facts cannot both hold. Either the numbers were transcribed with `blen` and `got` swapped, or they
came from a build that is not in the history. **This wants re-capturing from the journal before
anything is concluded from it** — and in particular, the entry's claim that B84's repair path is
"currently masking data loss" here is not supported.

What *is* true, and is what got fixed: the client assumed the geometry. `idx * BLOCK_SIZE` and
`block_len(size, idx)` encoded "4 MiB blocks with a remainder" in the range planner, the in-memory
ring, the in-flight fetch key and the on-disk block cache. When a revision is not shaped that way,
nothing breaks — but every client block straddles two server blocks, so the reader fetches and
decrypts two to answer for one, and the block written to disk lines up with nothing the next read
asks for. That is a real and unbounded cost on exactly the files the entry found, and it defeats
the property the block cache was built around.

### Fix (2026-08-17)

`BlockGeometry` (`pdfs-core/src/cache.rs`) is a prefix sum over a revision's block sizes, with
`BlockGeometry::uniform(size)` reproducing the old constant-block arithmetic exactly. Blocks are
now addressed by `BlockSpan { idx, start, len }` rather than by index, everywhere: the range
planner (`spans(offset, end)`), the in-memory ring, the in-flight-fetch key, and the on-disk block
cache. The span is the identity because the index alone stopped naming one byte range — block 1
under the fallback and block 1 under the real geometry are different bytes — so the block
sidecar records the block's plaintext `start` and a re-planned read misses rather than being
served the wrong offset. Sidecars written before that field existed read as "wherever the 4 MiB
assumption put it", which keeps the existing cache intact for every file this client uploaded.

The geometry itself is learned as a side effect of the first read (which has to open a
`RevisionReader` anyway) and written to a `<key>.geom` sidecar under the block cache;
`Core::block_geometry` reads it back and falls back to `uniform` when it is absent. Resolving a
reader just to *plan* would reintroduce the per-read key derivation the block cache exists to
avoid (B12), so the first read of a file still plans on the fallback and self-corrects. A recorded
table whose sizes do not sum to the node's size is refused outright, and eviction takes the
geometry with the blocks it describes.

The open question above is still open, and no longer blocks anything: "blocks are whatever the
revision says" is now what the code does, so a different constant is just another table.

Tests: `the_uniform_geometry_is_the_old_block_arithmetic`,
`a_non_uniform_geometry_places_blocks_by_prefix_sum` (built on the exact sizes from the log
above), `a_cached_block_is_not_served_to_a_different_geometry`,
`a_recorded_geometry_is_returned_only_for_the_revision_it_describes`.

---

## B83 — An access-deferred op retries forever and is reported as an ordinary queued upload

**Status:** Fixed (partly verified) 2026-08-16 — both halves are in and
unit-tested. The reporting half was confirmed against the live 28-op backlog on
1.8.0 (schema migrated to v27, all 28 rows window-stamped, `WARN` lines per op);
the resolution half has not yet been observed draining that backlog.
**Found:** 2026-08-16, running `scripts/fuse-acceptance.sh --live` on a
throwaway CLI-made mount. Four regression cases (B69, B70, B74, B79) failed with
`TimeoutError: daemon mutation queue did not drain` — the suite's
`wait_for_queue()` polls the *global* `status.mount.pending_uploads`, which had
been sitting at 28 for 30 days on the development machine. `pdfs status` called
them 28 queued uploads holding 18.4 GiB; `pdfs transfers` showed nothing;
`failing_ops` and `parked_uploads` were both 0.
**Where:** `run_pending_drain` / `run_authorized_drain`
(`crates/pdfs-fuse/src/drain.rs`), `require_uid_writable`
(`crates/pdfs-fuse/src/lib.rs`), `Db::effective_node_access`
(`crates/pdfs-core/src/db/share_access.rs`),
`Db::defer_op_without_attempt` (`crates/pdfs-core/src/db/ops.rs`).

**Root cause.** `effective_node_access` returns `Ok(None)` when the uid has no
row in `nodes`, and `require_uid_access` maps that to `EACCES` — "I have never
heard of this node" and "you may not write this node" are the same answer. The
drain reads `EACCES` as `DrainDisposition::AccessDeferred` and calls
`defer_op_without_attempt`, which by design touches neither `attempts` nor
`last_error`. So the op was re-deferred every `DRAIN_ACCESS_RECHECK` (5s)
indefinitely, and every channel that could have surfaced it stayed empty:
`attempts` 0, `last_error` NULL, `parked` counts only `PARK_UNTIL` rows,
`failing` counts only `attempts >= FAILING_ATTEMPTS`, and the one log line was
`debug!`. On the machine that found it, all 28 uids were absent from `nodes`:

```sql
select (select count(*) from pending_op p
          where exists(select 1 from nodes n where n.uid = p.uid)) as in_nodes,
       (select count(*) from pending_op) as total;
-- 0 | 28
```

**Fix, part 1 — stop guessing.** `WriteAuthority { Writable, Denied, Unknown }`
replaces the `Result<(), Errno>` the drain's access check used to return
(`Core::uid_write_authority`). `require_uid_writable` still collapses both
refusals to `EACCES`, which is right for a syscall: a stale handle must not be
admitted because the tree forgot the node. The drain does not collapse them —
`Unknown` becomes `DrainDisposition::AuthorityUnknown(uid)`, and
`Core::resolve_unknown_authority` asks the remote:

- node still there → `upsert_node` re-interns it and the op is deferred for one
  recheck, so the next pass gets a real access answer;
- node gone (`Ok(None)` / `is_gone`) → `record_op_failure` **immediately**, with
  a message naming the vanished uid, rather than waiting out the 5-minute window
  to say something vaguer;
- could not ask → ordinary deferral.

The authority asked about is the op's own (`pending_op_authorities`), so a
`create`/`mkdir` refetches its *parent*, not the node it has not made yet. The
row and its staged blob survive every branch.

**Fix, part 2 — say something.** Schema v27 adds `pending_op.access_deferred_since`.
`Db::defer_op_for_access` stamps it on the first deferral of a run and returns
the stamp on every one; `Core::defer_for_access` warns on the first deferral and,
once the run passes `DRAIN_ACCESS_DEFER_LIMIT` (5 min), reports it through
`record_op_failure` so it enters the ordinary backoff and shows up in
`failing_ops` and the status error text. `record_op_failure` clears the window so
each escalation re-arms it rather than firing every recheck, and an attempt the
access check admits clears it too (`clear_op_access_deferral`). The row and its
staged blob are untouched on every path — this changes only what the user can
see.

The deferral window stays as the backstop for everything that does not resolve,
including a refetch that keeps not helping, and `AccessDeferral` now carries the
reason into both the log line and the recorded `last_error`.

**Still open:** the acceptance runner waits on the *global* queue
(`ManagedSyncPair.wait_for_queue`), so any unrelated stuck op fails those four
cases on a machine that has one — that is what turned this backlog into four
apparent FUSE regressions.

## B82 — A `parent_uid` cycle hangs the daemon on any short search

**Status:** Fixed (unverified) 2026-08-12 — unit-tested, not driven against a
real corrupted database (nobody has one).
**Found:** 2026-08-12, writing a test for B80. The test built a two-node cycle
and called `search`; `cargo test -p pdfs-core` then sat at
`search_excludes_a_parent_cycle` for 29 minutes with **zero CPU** before it was
killed. Two earlier runs had already been abandoned to the same stall without
the cause being understood — it looks exactly like a cargo lock contention
problem, which is what it was first blamed on.
**Where:** `path_of` (`crates/pdfs-core/src/db/utils.rs`), `upsert_node_tx`'s
`descendants` CTE (`db/nodes.rs`).

**Root cause.** Both walks are recursive CTEs using `UNION ALL`, which does not
deduplicate, so `a.parent = b, b.parent = a` never terminates. The index side
was never exposed — `node_is_indexable_tx` rejects a cycle — but a query shorter
than `TRIGRAM_MIN` (3 chars) takes the `LIKE` lane, and **that lane reads
`nodes` directly and never consults the index**. Every row it returns is then
handed to `path_of`. So typing two characters into the prompt was enough, and
the spin happens while holding the daemon's only SQLite connection: the mount,
the drain, and every control request stop with it.

The API is what would produce such a cycle (a rename/move race, a malformed
event), so this is not a "corrupt your own DB" scenario only.

**Fix.** `path_of` is depth-capped at 256 and returns the truncated path rather
than hanging; `path_relative_to` already carried a 1024 cap, which is the
precedent. The `descendants` CTE in `upsert_node_tx` got the same guard for the
write side. Cycles also stay out of the index (see B80).

**Still open:** `pin_is_pinned`'s ancestor walk (`db/pins.rs`) has the same
shape and no guard. Not reached from search, so it was left alone.

## B81 — Prompt labels every Drive hit "My files", including device folders

**Status:** Fixed (unverified) 2026-08-12 — needs a GUI/dmenu run.
**Found:** 2026-08-12, reading the prompt while fixing B80: with B80 fixed, a
device-folder hit would render as `My files / Downloads`, a folder that does not
exist under the primary mount, while activating the same row opens
`~/Downloads/…`. Label and action disagreed.
**Where:** `Hit::location()` (`crates/pdfs-gui/src/prompt.rs`), shared by the
GTK prompt and the dmenu front-end.

The label was built from `SearchHit::path` with a hardcoded `My files /` prefix.
The daemon already resolves a device/sync-folder hit to a real local path in
`mounted_path` (`Core::mounted_search_path`), which is also what activation
uses; the label now derives from that when it is set, and falls back to
`My files` only for the primary mount.

**Second defect, same area.** Pin rows set `is_dir` from `Pin::recursive`
(`prompt.rs`, `dmenu.rs`). That flag is pin *policy* — whether the subtree is
kept on disk — not node kind, so a folder pinned non-recursively drew a file
icon and sent an invalid `OpenFile`. `Pin` now carries an `is_dir: Option<bool>`
resolved from the `nodes` row by `pin_list`, serde-defaulted so an older daemon
(which omits it) still parses; front-ends fall back to `recursive` only when the
node is not cached.

## B80 — 29% of the account is absent from the search index

**Status:** Fixed (unverified) 2026-08-12 — migration replayed against a
read-only copy of the live DB; not yet run by the daemon itself.
**Found:** 2026-08-12, auditing `cache.db` after a user report that search felt
unreliable. `pdfs search tickets` returned only the My Files folder
`Documents/Tickets`, never the `tickets.pdf` sitting in the device folder
`Downloads`. Counting the tables explained it:

```
nodes                     9252
nodes_fts                 6547     ← 2705 rows never indexed
nodes in orphan subtrees  2705     ← exactly the missing set
```

**Where:** `node_is_indexable_tx` (`crates/pdfs-core/src/db/nodes.rs`).

**Root cause — the same one as B79.** A node was indexed only if walking
`parent_uid` reached a row with `parent_uid IS NULL`. Exactly one row is stored
that way (the My Files root). A device folder's root lives in `device` /
`sync_folder` and is **never persisted as a `nodes` row**, so `Downloads`,
`Pictures`, `Documents`, `Music` and `Videos` walk up to a parent that does not
exist and were judged unindexable, along with everything beneath them. Stale
`pdfs-acceptance-*` roots hit the same path.

The v16 backfill did *not* share the bug — it rooted its walk at any node whose
parent is null **or absent**. So the index was correct immediately after
migrating and then lost each subtree the first time one of its nodes was
rewritten. That divergence between backfill and write path is why this survived:
a fresh install looks fine.

**Fix.** Indexability now requires only that the walk terminates with nothing
trashed on the way; a cycle is still rejected explicitly (B82). `MIGRATION_V21`
(`SCHEMA_VERSION` → 21) rebuilds `nodes_fts` so existing installs recover
without waiting for each node to be rewritten, and fixes two gaps in the v16
walk it replays: descendants of a trashed folder stay out, and the walk is
depth-capped.

Replayed against a `.backup` of the live DB: **9276 non-trashed nodes → 9276
indexed** (from 6547), 300 rows under `Downloads`, `tickets.pdf` at
`Downloads/tickets.pdf`.

**Watch for.** That path is the second thing this uncovered: the backfill first
seeded an uncached-parent root with an empty path, so the migration indexed
`tickets.pdf` while a later upsert indexed `Downloads/tickets.pdf` — the same
node with two paths depending on which code wrote it. Any future rebuild of this
index has to match `path_of`, not invent its own root rule.

**Related, not a defect.** The local half of the prompt indexes 110,187 files,
63,399 of them (58%) the Go module cache. Candidates are capped at 1000 and
scored *after* SQLite, so for a common term the pool filled with vendored files
before a real one was considered — for "test", 367 of the 500 best-ranked
candidates came from `go/pkg/mod`. `SKIP_DIRS` gained `dist`/`build`, a new
`SKIP_PATH_SUFFIXES` covers `go/pkg/mod`, and both candidate functions gained a
whole-query prefix lane ahead of the existing single-character one.

## B79 — Every write in an on-demand device-folder mount fails EACCES

**Status:** Fixed (verified live 2026-07-31, running in a locally rebuilt 1.2.1
package since)
**Found:** 2026-07-31, user on packaged 1.2.1: `touch ~/Documents/test` →
`Permission denied`, while the same operation under `~/ProtonDrive` succeeds.
**Where:** `crates/pdfs-fuse/src/state.rs`, `State::hydrate_access`

Not a mode-bit problem — the mount root reads `drwxr-xr-x` and the kernel's
`default_permissions` check passes. The denial comes from the queue guard:
`serve_create` → `Core::require_uid_writable`, which **intersects the access
every live inode space reports for that uid** (`lib.rs:721`) before anything
reaches `pending_op`.

The on-demand fork itself says `Owner`. The **primary** state says `Unknown`:

- `Core::hydrate` (`lib.rs:1012`) loads *every* `nodes` row into the primary
  state, including the subtrees that belong to on-demand device folders — they
  are on the same volume, in the same table.
- A device folder's `parent_uid` is the **device root**, which is never
  persisted as a node. In this account's DB all six device-folder roots
  (`Downloads`, `Pictures`, `Music`, `Documents`, `Videos`, `narl`) have
  `parent_uid = <device root>` with no matching row, so pass 2 parks them at
  `ORPHAN_INO`.
- `hydrate_access` then hit its "parent is not resident" branch, which had no
  answer and fell back to `Access::Unknown` — fail closed.
- `Unknown` is not writable, so the intersection denies every create, mkdir,
  write, rename, setattr and trash in every on-demand mount, permanently: those
  entries are never re-interned in the primary state, so nothing recomputes them.

`~/ProtonDrive` was unaffected because its nodes descend from `ROOT_INO`, which
`ProtonFs::new` hardcodes to `Owner`.

Two facts make this a *classification* bug rather than a policy one: the
persisted authority `Db::effective_node_access` already answers `Owner` for
exactly these uids (no `share_access` row covers them — the table holds only
`virtual~sharedwithme` and one accepted share), and the design table in
`mount-architecture.md` §2.2 says *not under a share, no role → Owner, fail
open*. The in-memory path disagreed with both.

**Fix:** the non-resident-parent branch now mirrors the persisted authority — a
recorded `share_access` row above the node still wins, otherwise fail **open**
to `Owner` on the mount's own volume and fail **closed** to `Unknown` on a
foreign one (`State::is_own_volume`). Foreign-volume shared content keeps its
fail-closed behavior, which is what B34 needs.

**Second-order effect, also fixed by the same change:** `downgrade_known_shared_access`
selects roots with `entry.access != Access::Owner`, so a `ScopeAccessLost` /
`SharedWithMeUpdated` event would additionally have force-downgraded every
device-folder tree to `Viewer` in memory.

**Tests:** `state::tests::own_volume_nodes_without_a_resident_parent_stay_owned`,
`foreign_volume_nodes_without_a_resident_parent_fail_closed`,
`persisted_authority_outranks_the_own_volume_fail_open`. The first fails on the
pre-fix branch with `left: Unknown, right: Owner`. The standing live guard is the
`regression B79` acceptance case, which finds a mounted on-demand device folder
through `pdfs locations --json` and writes into it (skipping when this machine
has none).

**Verified live (2026-07-31):** the packaged 1.2.1 daemon denies
`touch ~/Documents/pdfs-b79-test`, `mkdir`, and `>` redirect while
`~/ProtonDrive` accepts them; a local `--release` build of this fix accepts all
three on the same account and DB, the write drains (`pending_op` back to 0, the
new nodes carry real `G88km…` remote uids rather than local ones), and the
daemon log has no `403`/EACCES. `~/ProtonDrive/Shared with me/` stayed
`dr-xr-xr-x`, and the accepted editor-role share under it stayed writable — the
foreign-volume fail-closed path is unchanged.

**Side note for whoever runs this next:** stopping the daemon and touching a
file in an on-demand *mountpoint* while it is unmounted leaves that file in the
local directory, and `restore_ondemand_mounts` then refuses to mount over the
non-empty dir (`WARN … local dir is not empty; refusing to mount over it`).
Empty the directory and restart the unit.

---

## B78 — Profile backup fails: Cannot create file at the root of a device

**Status:** Fixed (verified live 2026-07-31). Same defect as **B68**, which was
found first on the managed-live matrix; B68's second half — making backup health
visible — is still open.
**Found:** 2026-07-29, user reported `upload profile: proton api error NotEnoughPermissions (http 422): Cannot create file at the root of a device`

The background task that backs up the machine's profile (sync folder mappings, pins, cache budget) currently attempts to write `profile.json` directly to the device root node (`device.root_uid`) on the Drive backend. The Proton Drive API has started enforcing a restriction that files cannot be created directly at the root of a device; they must be placed inside a folder.

**Consequence:** The upload is rejected with HTTP 422. The daemon retries when changes happen but the profile is never successfully backed up. Local changes (pinning, adding a sync folder) still take effect locally, but they will not be restorable on a new machine.

**Fix applied (matches the original `RECOVERY.md` spec):** the profile now lives
in a `.proton-drive-linux` folder inside the device root.

- `pdfs_core::profile::PROFILE_DIR_NAME` — the new constant, next to
  `PROFILE_FILE_NAME`.
- `profile.rs:ensure_profile_dir` — reuses the folder by name via the existing
  `find_device_child_folder`, creating it on the first save. Reuse-by-identity
  matters: a second folder of the same name would split the record in two with
  nothing to say which is current.
- `profile.rs:save_profile` — both upload branches (new file, new revision) now
  address the folder, never the device root.
- `profile.rs:load_profile` — reads the folder first, then falls back to a
  device-root `profile.json` written by an older client. The fallback is
  read-only and the legacy document is **not** trashed: another machine may
  still be running a pre-fix client that reads it.
- `profile.rs:list_restorable_folders` — filters `.proton-drive-linux` out, or
  the restore picker would offer to sync the bookkeeping back onto the machine
  it describes.
- `devices.rs:add_sync_folder` — refuses a local folder named
  `.proton-drive-linux`, which would otherwise be reused as the remote profile
  folder by name and then reconciled (i.e. overwritten) by the sync engine.

**Verified live (2026-07-31)** against the real account: first save logged
`created device profile folder name=".proton-drive-linux"` then
`profile backed up folders=6` with no 422; a second trigger reused the folder
(no second create) and uploaded a revision; `pdfs sync restore --json` lists
exactly the six device folders and not the profile folder.

**Tests:** `profile::tests::the_profile_is_never_uploaded_to_the_device_root`,
`loading_falls_back_to_a_legacy_device_root_profile`,
`the_profile_folder_is_not_offered_as_user_data` — source-level guards in the
style of `lib.rs`'s queue-guard ordering tests, because both upload branches are
pure network calls with nothing to assert offline.

**Not covered by this fix:** B68 also asks for backup *health* to be visible
(currently a `WARN` in the log and nothing in the UI), and for the
replacement-machine / fresh-state restore to be re-run end to end.

---

## B1 — FUSE rename loses the file (data loss)

**Status:** Fixed (verified 2026-07-20)
**Found:** 2026-07-19, user reported `mv file.mkv dir/` on the mount deleted the file
**Verified:** the exact repro below on the new daemon — `mv` returns 0, the file is
present in the destination listing and reads back its content. Previously it was in
neither directory.
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `Filesystem::rename`

`rename` ended with:

```rust
st.forget(&uid);
st.children.remove(&newparent);   // memory only
```

`forget()` deletes the node's **DB row** (`db.delete_node`). `children.remove()`
only drops the **in-memory** listing, leaving `nodes.listed = 1` on the
destination folder. The next `ls` goes through `ensure_children`, which takes the
DB fast path (`children_if_listed`, lib.rs:722) and rebuilds the destination from
the database — where the node no longer has a row.

Net effect: file gone from the source, absent from the destination, and
`rename(2)` returned **0**. Silent loss with a success code.

**Severity is worse than "stale cache":** `listed` is a **DB** column, so the bad
state survives a daemon restart. Nothing clears it on its own — the affected
folder keeps serving the listing that omits the file indefinitely, until an event
invalidation or a manual refresh happens to hit it. Confirmed on the repro
folders, which still read `listed=1` with no child rows minutes later.

The data itself is believed intact server-side (the move succeeded; only the
local view is wrong), but that was **not** verifiable without a daemon restart —
see the status line.

Also hits plain in-place renames (`mv a.txt b.txt`), same mechanism — the parent
is both source and destination there.

The control-socket `Core::move_to` path was always correct; it used
`invalidate_listing`, which clears the DB flag as well. The FUSE path didn't.

**Fix:** use the same helper.

```rust
st.forget(&uid);
st.invalidate_listing(newparent);
```

**Repro:**

```
cd <mount> && mkdir d && echo hi > f.txt && mv f.txt d/
# renameat2(...) = 0, but f.txt is in neither directory,
# not in trash, and absent from the `nodes` table
```

**Note on `invalidate_listing`:** it early-returns when the folder isn't in the
in-memory `children` map, so it won't clear a stale DB `listed` flag in that
case. Safe on this path (both `lookup_child` and the explicit
`ensure_children(newparent)` guarantee the listing is resident), but it's a sharp
edge for other callers. See B4.

---

## B2 — The reported .mkv went to trash, the repro vanished entirely

**Status:** Partly resolved — `mv` exonerated; trash origin still unattributed
**Found:** 2026-07-19, while chasing B1

### What was settled (2026-07-19)

**`mv` never trashes.** Three repros on the mount, all with the old (pre-B1-fix)
daemon:

| repro | node state | destination | result |
|---|---|---|---|
| `zz-claude-repro-src.txt` | create still queued | fresh folder | vanished, **not** trashed |
| `zz-b2-settled.txt` | settled, real uid, landed | listed folder | vanished, **not** trashed |
| `zz-b3 Movie Title.mkv` | settled | folder named as the file's stem (the user's exact shape) | vanished, **not** trashed |

So B1 fully explains the *disappearance*, and nothing in the rename path
trashes. The `.mkv` reaching the trash has a different cause.

**The listing the user acted on was almost certainly stale.** B1/B4 make stale
listings *persistent* — `listed = 1` lives in the DB and survives restarts — so
`ls` showing the file at 15:57 is not evidence it still existed remotely. This
is the most likely reading: it had been trashed earlier and the mount kept
showing it.

**Ruled out:**

- *Sync engine.* `reconcile_folder` gates on `mode == "mirror"`; `~/Videos` was
  `ondemand` from 15:39:27. All three sync trash sites log, and no such rows exist.
- *Log pruning hiding the evidence.* `ACTIVITY_KEEP = 2000`; the table is at the
  cap but its oldest row is Jul 17 18:03, so the whole window is covered. The
  `.mkv`'s uid appears exactly once — an `Upload` at 06:07:56 — and never again.
- *Conflict machinery.* `keep_as_conflict_copy` uploads a new file under an
  alternate name; it never trashes the original.
- *Eviction through the mount.* Was the best theory: `mirror→ondemand` calls
  `evict_dir_contents`, which would issue one `unlink(2)` per file, and FUSE
  `unlink` trashes remotely. But `apply_sync_folder_mode` evicts **before**
  `spawn_ondemand_mount`, so the deletes hit the plain local directory. Dead.
- *Control-socket delete.* `CtlRequest::Delete` logs on both success and failure.

**What remains:** `trash_child` (backing FUSE `unlink`/`rmdir`) was the only
trash path that wrote no activity row — so an ordinary `rm` on the mount, at any
point after 15:39:27 when `~/Videos` became a FUSE mount, would produce exactly
what we see and leave no trace. Note there is also a *trashed folder* of the same
name (`Evangelion 1.11 - You Are (Not) Alone`, `is_dir=1`, content mtime
04:57:50), which suggests this mkdir-and-move dance had been attempted earlier —
plausibly followed by a manual cleanup while B1 was making files appear to vanish.

That is a hypothesis, not a finding. It is not provable from the evidence that
survives.

### Fix applied

`trash_child` now logs to the activity feed on both success (`"trashed from the
mount"`) and failure, matching every other trash site. The next occurrence will
be attributable — which is the part that actually mattered here.

### If it recurs

The activity log is now the first place to look. `trash` rows carry no
trashed-at timestamp (the `mtime` column is the node's *content* mtime — for the
`.mkv` it read 06:07:56, matching its upload, not its deletion), so the activity
feed is the only ordering evidence there is.

Two different endings for what looked like the same operation, so B1 may not be
the whole story:

- User's `Evangelion 1.11 - You Are (Not) Alone.mkv` → **trash**, full size
  (1768670449) intact, recoverable via `pdfs restore`.
- Clean repro (`zz-claude-repro-src.txt`) → **gone entirely**. Not in trash, no
  `nodes` row, nothing in the search index.

B1 explains the second. It does not explain a node reaching the trash — nothing
in the FUSE `rename` path calls `trash_nodes`.

Leading theory: the trash came from the sync-conflict machinery, which *is*
logged (`ActivityKind::Trash` rows exist with `(sync-conflict <ts>)` details).
The `~/Videos` folder is registered as a sync folder in `ondemand` mode, and had
just been switched to `ondemand` shortly before. `reconcile_folder` gates on
`mode == "mirror"`, so it should have been inert — worth confirming that gate
actually held, and that no pass was already in flight across the switch.

Note the file was a **conflict-copy sibling** of two files already carrying
`-003` / `-004` suffixes, so the conflict path had definitely been active in that
directory.

**Next step:** find the trash event's origin. The activity log had no row at the
15:58 mv, which points away from a logged (control-socket / sync) path — but the
timestamp bug in B3 made that table hard to read, so re-check with correct
scaling before trusting the absence.

---

## B3 — ~~Activity log timestamps written in seconds, read as milliseconds~~

**Status:** Not a bug — investigator error, kept as a record
**Found / retracted:** 2026-07-19

Originally filed because every activity row rendered as `1970-01-21 …`. That was
an artifact of the ad-hoc debugging query, which divided by 1000; the code never
does. Seconds are consistent end to end:

- writer — `log_activity` uses `now_secs()` (`pdfs-fuse/src/lib.rs`)
- schema / `activity_list` — pass the value through untouched
- reader — `activity_time` calls `glib::DateTime::from_unix_local(secs)`

The correct manual query, for next time:

```sql
select datetime(time,'unixepoch','localtime'), kind, target, detail, ok
from activity order by time desc limit 40;
```

Worth noting because the bad query is what made the activity log look empty
around the reported `mv` — which is evidence B2 leans on. That absence has since
been re-checked with correct scaling and it does hold: no row at 15:58.

---

## B4 — `invalidate_listing` silently skipped non-resident folders

**Status:** Fixed (verified 2026-07-20, via B1's repro — the FUSE rename path that
depends on this helper now behaves correctly end to end)
**Found:** 2026-07-19, reviewing the B1 fix
**Where:** `crates/pdfs-fuse/src/state.rs:257`

```rust
if self.children.remove(&ino).is_none() {
    return;              // never clears the DB `listed` flag
}
```

The early return assumed "not in the hot cache ⇒ nothing to invalidate", which
stopped being true once listings became DB-backed. A folder trimmed from the
in-memory map but still `listed = 1` in the DB could not be invalidated at all —
callers thought they had dropped the listing, and `ensure_children` would happily
rebuild the stale one.

**Fix:** drop the early return; always clear the flag. Costs one redundant
`UPDATE` when nothing was cached.

`Core::refresh_dir` had been hand-rolling a workaround for exactly this (clearing
the DB flag itself, then reaching into `state.children` directly, with a comment
explaining why it couldn't use the helper). It now just calls
`invalidate_listing`. **Behaviour change worth knowing:** `refresh_dir` used to
propagate a DB write failure to the caller of `CtlRequest::Refresh`; it now warns
and reports success, matching every other invalidation site.

**Test:** `a_deleted_child_leaves_a_listed_parent_serving_a_stale_listing` in
`pdfs-core/src/db/tests.rs` pins the B1/B4 mechanism at the DB layer — a deleted
child plus a still-`listed` parent yields a listing that silently omits it.

---

## B5 — `ls -l` costs a network round trip per file (thumbnail xattr probes)

**Status:** Fixed (verified 2026-07-20)
**Found:** 2026-07-19, investigating "`exa -l` is slow in the mounts, `exa` is fast"

### Reproduced (2026-07-19)

All runs cold (35 s wait to clear the 30 s attr TTL). `exa` needs only `readdir`;
`exa -l` stats every entry, so the delta is per-entry metadata cost:

| mount | entries | `exa` | `exa -l` | delta per entry |
|---|---|---|---|---|
| primary `~/ProtonDrive/Installer` | 34 | 10 ms | 11 ms | **0.03 ms** |
| on-demand `~/Documents` | 45 | 27 ms | 54 ms | **0.6 ms** |
| on-demand `~/Videos/[Reaktor] FMA …` | 65 | 73 ms | 193 ms | **1.85 ms** |

### Cause (2026-07-20): a network round trip per file per xattr name

`strace -f -T` on a cold `exa -l` of the 65-file `.mkv` directory:

```
lgetxattr(".../E01 ....mkv", "user.proton.thumbnail", NULL, 0) = -1 ENODATA <0.186435>
```

129 `lgetxattr` and 258 `llistxattr` calls for 65 entries, each `lgetxattr`
~186 ms. Three things compounded:

1. **`listxattr` advertised `user.proton.thumbnail` + `user.proton.preview` for
   every file**, regardless of whether that file could have one. An xattr-aware
   lister then asks for each advertised name — two `getxattr` per entry.
2. **`Core::thumbnail` cached only success.** `download_thumbnail` returning
   `None` — the normal answer for a `.mkv` — was never remembered, so every
   listing re-asked the API and was re-told nothing, forever.
3. **`getxattr` ran inline on fuser's dispatch loop**, not on the `Workers`
   pool. `lookup` and `readdir` had been moved off it (PERF #1.0); this one was
   missed. So each 186 ms miss stalled *every* other op on the mount, which is
   why the concurrency `exa` does have bought nothing.

**The mount-kind correlation was a red herring.** It tracks file *type*, not the
fork: `~/Videos` is `.mkv` (Proton generates no thumbnail — always a miss),
`~/ProtonDrive/Installer` is installers whose probes were already warm. The
`fork_state` theory in the previous write-up is wrong; forked and primary mounts
run the same code and neither is at fault. Worth noting as a lesson — three
measurements on different directories looked like a mount-kind effect because
nobody had varied file type independently.

### Fix

- `listxattr` advertises the names only for `image/*` and `video/*` media types.
  `getxattr` still honours an explicit request for an unadvertised name, so
  nothing becomes unreachable. Everything else — documents, installers, archives
  — now costs zero round trips per listing.
- `Core::thumbnail` remembers misses in `no_thumbnail`, keyed `(uid, type)` and
  validated against the node's mtime exactly as the positive side is. Bounded by
  clearing at `MAX_THUMBNAIL_MISSES` (8192).
- `getxattr` hands off to `Lane::Meta`, so a miss can never block the dispatch
  loop.

A video directory still pays its misses once per revision (image/video is where
a thumbnail plausibly exists, so the probe is legitimate) — but in parallel, and
never again.

### Verified (2026-07-20)

Cold runs (40 s wait each) on the restarted daemon:

| measurement | before | after |
|---|---|---|
| `~/Videos/…FMA…` (65 `.mkv`) cold `exa -l` | 193 ms | **14 ms** |
| `~/Documents` (45 entries) cold `exa -l` | 54 ms | **13 ms** |
| `user.proton.*` probes over the 65 `.mkv` | 130 | **0** |
| `~/Documents` probe latency, 1st listing | 124–395 ms | 124–395 ms |
| `~/Documents` probe latency, 2nd listing | 124–395 ms | **0.26–0.67 ms** |

Each part of the fix is separately visible in the traces:

- **The advertising gate.** The `.mkv` directory now issues *zero* `user.proton.*`
  probes; the only remaining `lgetxattr` is exa's own `security.selinux` at
  ~0.3 ms. Note this means Proton does **not** report those files as `video/*` —
  the gate is excluding them by media type, not because they are known to lack a
  thumbnail. A video whose media type is generic no longer advertises one even if
  it has it; `getxattr` still serves an explicit request, so it stays reachable.
  Worth revisiting if thumbnails ever go missing in a file manager.
- **The negative cache.** `~/Documents` holds 10 `.png` files, correctly still
  advertised, so it probes 20 times on both passes — but the second pass answers
  in 0.26–0.67 ms instead of going to the wire. That is the whole point: the
  probes that are legitimate stop being expensive after the first.
- **The worker handoff.** Pass 1 on `~/Documents` shows interleaved
  `<... lgetxattr resumed>` lines across several tids, i.e. the misses now
  overlap instead of serializing behind the dispatch loop.

### Secondary finding (still open): `lookup` is O(n) per name

**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `serve_lookup`

`serve_lookup` linear-scans the parent's children comparing names, and with no
`readdirplus` that is one `lookup` per child — O(n²) name comparisons under the
global state lock for a listing.

Real, but **not** what made this slow: it costs the same on both mount kinds and
at 65 entries a linear scan is nanoseconds against a 186 ms round trip. Two
candidate optimizations were considered and deliberately *not* applied, and that
judgement still holds now the real cause is known:

- **Per-directory `name -> ino` map.** A second structure that must stay in sync
  with `children` — the two-halves-of-one-cache shape that caused B1 and B4.
- **`readdirplus`** (`fuser` 0.17 supports it; we return `ENOSYS`). The right
  shape long-term, but it is a rewrite of the directory-read path and it would
  have hidden this bug rather than explained it.

### Measurement notes for next time

- Cold requires a 35 s wait per run (30 s attr TTL). Warm runs show ~6 ms
  regardless and prove nothing.
- `strace -c` on `exa` **needs `-f`** — exa is multithreaded, and without it the
  trace captures only main-thread startup (3 `statx` calls, no `getdents`).
- `-T` (per-call durations) is what cracked this; `-c` summaries attribute time
  across threads in a way that hid the 186 ms constant.

---

## B6 — Daemon sets no file modes: `control.sock` is an unguarded authority

**Status:** Fixed (verified 2026-07-20)
**Found:** 2026-07-19, while writing the plaintext-at-rest threat model in
`docs/ARCHITECTURE.md` §8. Not from a report — from checking a claim before
asserting it in a doc. I had written "their default 0700 permissions protect
them", went to verify, and found we set no modes at all.
**Where:** `crates/pdfs-fuse/src/mount.rs` (`UnixListener::bind(control_socket)`),
`crates/pdfs-core/src/config.rs` (`create_dir_all` on state/cache dirs)

`grep -rn "set_permissions\|from_mode" crates/pdfs-core/src crates/pdfs-fuse/src`
returns exactly one hit, and it is `shell.rs` setting `0755` on a generated
script. Nothing else sets a mode. So:

- state and cache directories are created at `0777 & ~umask` — typically `0755`
- `control.sock` is bound at the same, typically `0755`

Anything that can connect to the socket drives the daemon with its authenticated
session: enumerate the tree, read file contents, upload, trash, create public
share links. No credential is required — the keyring is never consulted, because
the daemon already holds the session.

The only thing preventing this today is that `~/.cache` and `~/.local/state` are
conventionally `0700`. That is a property of the user's system, not something we
establish or check. It does not hold on a machine with a permissive umask, a
group-shared home, or a home restored from a backup that flattened modes.

**Severity:** low on a single-user desktop, real on a shared or multi-user host.
It is a privilege boundary rather than a data-at-rest issue, which is what makes
it worth more than the cache-plaintext point it was found next to.

**Fix:** `chmod 0600` on the socket immediately after `bind` (before the listener
thread starts accepting), and `0700` on the state and cache directories at
creation. Both are a few lines. Worth also asserting the socket mode in a test,
since a regression here is silent.

**Fixed 2026-07-19:** `AppDirs::ensure` now sets `0700` on the state, cache, and
config directories on *every* start (not just at creation, so an existing
permissive directory is tightened), and `config::restrict_socket` sets `0600` on
both the control socket and the tray socket immediately after `bind`. A socket
whose mode cannot be set takes the daemon down rather than serving unguarded;
the tray's is best-effort, since it only guards single-instancing. Unit tests in
`config.rs` assert both modes.

**Verified 2026-07-20** on the restarted daemon — all three directories now read
`drwx------` and both sockets `srw-------`, against `drwxr-xr-x` / `srwxr-xr-x`
before.

**Measured exposure at the time of the fix:** the directories really were
`drwxr-xr-x` and the live socket `srwxr-xr-x`, but `~/.cache` and `~/.local`
were both `0700` on this machine, so nothing was reachable in practice. The bug
was a latent dependency on those parents, not a live hole. Removing the
dependency is the point.

---

## B7 — renamed directory reads as missing until the entry TTL expires

**Status:** Fixed (verified 2026-07-20)
**Verified:** `mkdir "zz-b7 Old Name"` with a file inside, `mv` to a new name, then
an immediate `ls` and `cat` through the new name — both succeed with no wait. The
directory used to return ENOENT until the entry TTL expired.
**Found:** 2026-07-19, user renamed a folder on the mount with `mv`. `ls` of the
parent listed the new name, but `ls <newname>/` returned ENOENT — repeatedly, so
not a one-shot race. It self-healed on its own a few minutes later, and the
files were all intact: this is a visibility bug, not data loss.
**Where:** `crates/pdfs-fuse/src/state.rs`, `State::relocate`

**Cause:** the online rename path ended with `relocate`, which called `forget` on
the moved node. `forget` drops the node's `by_uid` mapping, so when the
invalidated parent listing re-enumerated, `intern_mem` allocated it a **fresh**
inode. But the kernel had already carried the renamed dentry over to the *old*
inode number, and it holds that dentry for the entry TTL. Every lookup, getattr
and opendir through it resolved to an inode `entries` no longer held:

    entries.get(old_ino) -> None -> ENOENT

`readdir` of the parent went the other way — it walks `children` and reports the
*new* inode — which is exactly why the directory listed fine but could not be
entered. Once the TTL expired the kernel re-looked-up the name, got the new
inode, and everything worked again.

**Fix:** `relocate` now rewrites the node in place (`rename_in_place`) and
invalidates both parents' listings, instead of forgetting it. The inode is
preserved, so the kernel's dentry stays valid and the re-enumeration reuses the
same `by_uid` slot. Both `relocate` tests now assert inode stability; that
property was untested, which is how this got through the B1 fix.

**Note:** dropping the `forget` also stops the moved node's DB row from being
deleted and re-created on every move — the row is updated instead, which is what
the B1 fix was working around from the other side.

---

## B8 — no way to complete a CAPTCHA, so a gated sign-in is unrecoverable

**Status:** Implemented, partially verified — the bridge is proven, a real gate is not
**Found:** 2026-07-19, user hit `proton api error Unknown (http 422): For
security reasons, please complete CAPTCHA` and had no way to answer it.
**Where:** SDK `api.rs`/`http.rs`/`session.rs`, `pdfs-core/src/auth.rs`,
`pdfs-gui/src/app/pages/verify.rs`

**Cause:** three gaps stacked, none of which is a bug on its own.

1. `ResponseCode` had no `9001`, so the gate deserialized to `Unknown` — which
   is why the error read as an opaque failure rather than a recoverable prompt.
2. Nothing read `Details.HumanVerificationToken`, and the client never sent the
   `x-pm-human-verification-token{,-type}` headers the retry needs.
3. No UI could render Proton's hosted verification page.

**Fix:** `HumanVerification` (challenge) + `HumanVerificationCredential`
(answer) in the SDK, header plumbing on the session-less login calls, and
`ProtonApiSession::begin_verified`. `pdfs-core` promotes a solvable gate to
`Error::HumanVerificationRequired` and `auth::login_interactive` re-runs the
login with the earned token — the retry lives in core because the gated attempt
burns its SRP handshake, which no front-end should have to know. The GUI hosts
the page in a `WebKitWebView` (webkit6 0.4, pairs with the existing gtk4 0.9)
and bridges the page's `postMessage` to a script message handler.

**Deliberately not done:** `email`/`sms` verification methods (the token arrives
out of band, so the webview cannot complete them — such a gate stays a plain API
error rather than opening a page the user cannot finish), HV on *authenticated*
endpoints (only the login path is plumbed), and re-gating of the retry (a second
challenge means the token was rejected; looping would trap the user).

**Verified:** the WebKit bridge end-to-end with a throwaway probe — handler
registers, the page's `postMessage` arrives, non-completion messages are
filtered, the completion token round-trips. Unit tests cover 9001 parsing, the
challenge/`Details` shape, URL escaping, and message filtering.

**Not verified:** a real gated login against verify.proton.me. It cannot be
triggered on demand, so the exact message shape Proton's page posts is taken
from its documented contract, not observed. If verification appears to hang with
the page solved, that is the first thing to check — `extract_token` in
`verify.rs` is the single place that decides what counts as completion.

**Note:** the SDK half shipped as proton-sdk / proton-drive-rs **0.1.11**. The
workspace requirement was widened to `"0.1"` at the same time, so later 0.1.x
releases are picked up by `cargo update` without an edit here.

---

## B9 — Enter did not open the selected result in the launcher

**Status:** Fixed (verified)
**Found:** 2026-07-19, user reported Enter doing nothing in `pdfs-prompt` while
the arrow keys and Escape worked normally.
**Where:** `crates/pdfs-gui/src/prompt.rs`, the window key controller

**Cause:** key-event *phase*, not key handling. `gtk4::Entry` — really its inner
`GtkText` — binds Return to its own `activate` and consumes it. The launcher's
`EventControllerKey` is attached to the **window** in the default **bubble**
phase, so the focused entry saw Return first and the window handler was never
reached. Escape and Up/Down worked precisely because `GtkText` has no bindings
for them and they bubbled up as intended.

That asymmetry is the tell: when some keys reach a window-level controller and
others silently don't, the ones that don't are being claimed by the focused
widget.

**Fix:** Return moved off the window controller and onto
`entry.connect_activate`. Enter while focus is in the results list was already
handled by `row_activated`, so the two together cover both focus positions.

**Rejected alternative:** setting the window controller to
`PropagationPhase::Capture`. It fixes the symptom by putting the handler ahead
of the entry — but ahead of it for *every* keystroke, not just Return, which
puts text input and IME composition behind a handler that has no business
seeing them. The narrow fix has no such blast radius.

**Verified:** live. Pressing Enter logged `opening path=…`, the daemon hydrated
the file, and `xdg_open` ran.

---

## B10 — GLib critical when the launcher closes over an in-flight open

**Status:** Open (side finding)
**Found:** 2026-07-19, seen in `pdfs-prompt` output immediately after a
successful Enter-to-open while verifying B9.

```
GLib-GIO-CRITICAL: g_list_store_remove: assertion '!g_sequence_iter_is_end (it)' failed
```

**What is known:** it fires right after the `opening path=…` log line, i.e.
during `xdg_open` + `window.close()`. It is a warning, not a crash — the open
itself succeeded.

**What it is not:** the launcher's own row handling. `Section::set_rows` walks
`GtkListBox` children (`first_child`/`remove`) and touches no `GListStore`, so
the failing store belongs to GTK/libadwaita internals, most likely something
teardown-ordering related in the app/window bookkeeping as the window is closed
while a launch is still settling.

**Why it was not chased:** it appeared while verifying an unrelated fix and has
no user-visible effect. Worth revisiting if the launcher ever misbehaves on
close (a hang, a lost open, or a stale window), since an assertion firing during
teardown is exactly the shape of bug that later turns into one of those.

**Where to start:** run `pdfs-prompt` under `G_DEBUG=fatal-criticals` to turn
the warning into an abort and get a real backtrace naming the store.

## B11 — a file moved while its create is still queued reads as empty

**Status:** Fixed (verified 2026-07-20)
**Found:** 2026-07-20, while verifying B1 on the restarted daemon.

The B1 repro is `echo hi > f.txt && mv f.txt d/`, i.e. the move lands while the
create is still queued for upload. Immediately after the `mv`, the file is
present in the destination listing — B1 is genuinely fixed — but stats as **0
bytes** and `cat` returns nothing:

```
-rw-r--r-- 1 narl narl 0 Jul 20 00:29 zz-b1-dst/zz-b1-src.txt
```

A few seconds later the same file reads correctly (`3` bytes, `hi`). A control
file created and read *without* an intervening `mv` was correct immediately.

**Cause:** `State::intern_mem` replaces an existing entry's node wholesale
(`e.node = node`). A move invalidates both parents' listings (that is B7's fix,
and it is correct), so the next `ls` re-enumerates — and the node that comes back,
from the remote or from its DB row, carries the size of the revision the *server*
holds. For a file whose write is still queued that is the pre-write size, usually
0. Interning it reverts the optimistic size `record_pending_write` had stamped.

**Why an empty read rather than a short one:** a file that stats as 0 bytes gets
**no `read` from the kernel at all**. `read_range` would have served the staged
blob quite happily; it is never asked. So the file reads as empty for as long as
the stale size stands, and "empty file" is indistinguishable from "file whose
contents were lost" to whatever is reading.

`Core::hydrate` already solved exactly this for the *restart* case — it stamps
each pending write's size onto the node as entries materialize. The protection
was simply missing on every live re-enumeration path.

**Fix:** `Core::stamp_pending_sizes` re-applies the optimistic size to a batch of
nodes, called from both arms of `ensure_children` (the DB fast path and the
network path) before the state lock is taken. It snapshots the pending map first
and returns: no site in the daemon holds `pending` and `state` at once, and this
is not the place to become the first — `hydrate` established that pattern for the
same reason.

Scoped to `ensure_children` deliberately. The other `intern` sites are
single-node and all authoritative by construction (mkdir, create, an upload that
just landed); `drain.rs`'s post-upload adoption is *supposed* to take the
server's node, and already handles the queued case through `rebaseline_pending`.

**Tests:** `pending_size_tests` in `pdfs-fuse/src/lib.rs` covers the queued file
keeping its size, a settled sibling keeping the server's, folders being left
alone, and an empty pending map changing nothing.

**Verified 2026-07-20** on the rebuilt daemon: `echo hi > f.txt && mv f.txt d/`
then an immediate `ls -l` and `cat` gives 3 bytes and `hi`, against 0 bytes and
empty output before.

---

## B12 — cold enumeration is slow per entry, and goes superlinear past ~500

**Status:** Both causes fixed and verified (2026-07-20); attr-invalidation follow-up unverified
**Found:** 2026-07-20, measuring improvements.md P2.8 ("pipeline
`enumerate_nodes_detail`") before implementing it, to decide whether the
bottleneck was network or crypto.

### Measured (cold; `pdfs refresh <dir>` then a timed `ls -1`)

| entries | time | per entry |
|---|---|---|
| 1–9 | 350–550 ms | fixed cost |
| 34 | 629 ms | — |
| 147 | 1.92 s | 12.5 ms |
| 251 | 3.18 s | 12.4 ms |
| 484 | 6.25 s | 12.5 ms |
| **793** | **38.8 s** | **48.9 ms** |

The 793 figure reproduces to ±100 ms across three runs. Marginal cost from 484 to
793 is **106.7 ms/entry**, 8.5× the baseline.

`perf record` against the running daemon shows **~90 % of cycles inside the
`pdfs` binary** in both cases, and sample counts matching wall time (1022 samples
≈ 5.1 s, 7538 ≈ 37.9 s at 199 Hz) — so this is CPU-bound throughout, not waiting
on the network. Symbols are unavailable (the installed binary is stripped), which
is what blocks attribution.

### Cause 1 (found, ~half the baseline): every FTS row is deleted by a full scan

`nodes_fts` declares `uid` as **UNINDEXED**:

```sql
CREATE VIRTUAL TABLE nodes_fts USING fts5(uid UNINDEXED, name, tokenize='trigram')
```

and `Db::upsert_nodes` (`pdfs-core/src/db/nodes.rs`) deletes by exactly that
column on every node, because "FTS5 has no UPSERT":

```sql
DELETE FROM nodes_fts WHERE uid = ?1
```

An UNINDEXED FTS5 column is not searchable, so the predicate cannot use an index.
`EXPLAIN QUERY PLAN` confirms it:

```
`--SCAN nodes_fts VIRTUAL TABLE INDEX 0:
```

One full scan of the FTS index per node written. Measured on a `.backup` copy of
the live 171 MB DB (17 443 indexed nodes):

| deletes | time | per delete |
|---|---|---|
| 484 | 3.09 s | 6.4 ms |
| 793 | 5.26 s | 6.6 ms |

So **~6.5 ms of the 12.5 ms per-entry baseline is this one statement**, and it
gets worse for every user as their node count grows — the scan is over the whole
index, so the cost of writing *any* listing scales with the size of the *account*.
It is also paid by every sync pass and every event-driven refresh, not just `ls`.

**Fix direction:** delete by rowid instead. FTS5 deletes by rowid efficiently, so
map `nodes.rowid` to the FTS rowid and drop the `uid` column from the index
(or keep it UNINDEXED purely for retrieval). Needs a schema migration on a live
171 MB DB, so it wants its own change rather than being smuggled into this one.

### Cause 2 (the dominant one): an S2K key derivation per file, to list a folder

**Symbols came from the local unstripped build, not a new install.** `strip`
preserves both the build id and the text addresses, so when `/usr/bin/pdfs` and
`target/release/pdfs` reported the *same* build id, the stripped binary was just
a copy of the local one and `perf buildid-cache -a target/release/pdfs` was
enough to symbolise a recording of the running daemon. Worth remembering — it
turned a "needs another install + restart" into a five-second step.

Hot functions over a 793-entry cold enumerate:

| % | symbol |
|---|---|
| 64.4 | `sha2::sha256::x86::digest_blocks` |
| 6.0 | `<D as digest::DynDigest>::update` |
| 5.7 | `sqlite3VdbeExec` (cause 1, above) |
| 4.0 | `sha2::sha256::compress256` |
| 1.5 | `pgp::types::s2k::StringToKey::derive_key` |

**~74 % of the listing is SHA-256 inside PGP's S2K**, i.e. the per-file node-key
unlock that `build_node` does under `NodeDetail::Full`. `derive_key` itself shows
1.5 % because the time lands in its SHA leaf. Nothing downloads during an `ls`, so
PGP is the only plausible source of that SHA.

### The "cliff past ~500" framing was wrong

That was an inference from a single folder, not a measurement. An S2K's cost is
set by the iteration count in the key packet, which is chosen by *the client that
uploaded the file* — so it varies per file, not per listing size. Every folder
measured sits at 12–16 ms/entry except `Music/aC_ID.dll` at 49 ms, and
`Music/ELDEN RING SOUNDTRACK` (67 files, same parent tree) is ~16 ms/entry, so it
is not "Music files are expensive" either. A 4× jump for a 1.6× change in n fits
"those files were uploaded by a client using a costlier S2K" far better than a
size threshold. n and folder identity are confounded in the data — there was only
ever one folder above 500 entries.

Left unresolved deliberately: the fix below removes the per-file S2K from the
listing path entirely, so which of the two explanations held stopped mattering.
To settle it anyway, log the S2K iteration count per file across one enumerate of
each folder.

### Fix: enumerate cheap, upgrade sizes in the background

`ensure_children` now calls `enumerate_nodes_light`, which skips the file
node-key unlock (folders are still unlocked — their keys are what the children
decrypt with). The listing is served from that immediately.

`Light` returns no `claimed_size`, and `ls -l` wants sizes, so a naive split would
just move the same S2K onto the first `stat`. Instead `Core::spawn_size_upgrade`
fetches the full nodes for that folder on a worker, batched, after the listing has
been answered, and adopts *only* the size — re-interning wholesale would clobber a
name or parent that a concurrent rename/move had changed. It is single-flight per
folder, because a `stat` of one entry in a fresh listing means a `stat` of all of
them. Queued writes keep their optimistic size through it, via the same
`stamp_pending_sizes` that B11 added.

Three entry points cover every way a provisional listing can appear: the network
enumeration, the DB fast path (rows persisted before an upgrade ran), and
`getattr` itself (a listing restored by `hydrate` on mount, which `ensure_children`
returns early for).

**Known tradeoff — sizes are provisional until the upgrade lands.** `node_size`
falls back to `total_size_on_storage`, the ciphertext size, so a `stat` in that
window reads slightly *too large*. Reads are unaffected: the revision reader
carries its own authoritative size. Deliberately not the B11 shape — that
reported **0**, which made the kernel skip reads entirely; too-large is cosmetic
and self-corrects.

**The window was longer than "a round trip", and that took a correction.** The
daemon has the real sizes quickly, but the *kernel* keeps the provisional attrs
for the full 30 s entry TTL, so `ls -l` kept reporting them long after the DB was
right. This nearly caused a misdiagnosis during verification: two successive
`ls -l` runs agreed with each other and both disagreed with the truth by a
constant +59 bytes, which reads exactly like "the upgrade never ran". Querying the
DB directly is what separated "not computed" from "computed but not visible".

Closed by having `spawn_size_upgrade` call `notifier.inval_inode` for the inodes
it corrected, after the DB write so a provoked re-`getattr` cannot race the
persistence. That needed a `Notifier` on `Core` (a `OnceLock`, since the session
is built *from* the `Core`), and a fresh one per on-demand fork — each fork has
its own inode space, so notifying through the primary mount's channel would name
inodes that session has never heard of.

### Verified (2026-07-20)

| folder | before | after |
|---|---|---|
| `Music/aC_ID.dll` (793) | 38.8 s | **4.48 s** (8.7×) |
| `InstantUpload/Camera` (484) | 6.25 s | **2.91 s** (2.1×) |
| `Pictures/old` (417) | 5.10 s | **2.25 s** (2.3×) |

Schema v15 migrated the live 171 MB DB on start (17 443 rows indexed). All 793
sizes converge to exactly their pre-change values, checked against a `.backup`
copy taken before any of this landed. The +59-byte ciphertext delta is a precise
probe for a provisional size, and is what the attr-invalidation follow-up should
be tested with.

### Original cause-2 ruling-out (kept — all still true)

Backing cause 1 out of the totals leaves per-node non-FTS cost at **6.2 ms at
n=484 but 42 ms at n=793**. Something gets ~7× more expensive per node, sharply,
somewhere around 500–800 entries.

Ruled out so far:

- *Chunking / network.* `MAX_BATCH_COUNT = 150`, so cost would rise in visible
  steps at 150/300/450; it is flat at 12.5 ms/entry straight through 484. The
  marginal cost would imply ~16 s per POST, which is absurd.
- *Composition.* Both the 484 and 793 folders are 100 % files, no subfolders.
- *`FOLDER_KEY_CACHE_CAP` (512).* Suspicious number, but `folder_keys` only holds
  *folder* keys and these children are all files, so nothing is inserted during
  the walk. `resolve_parent_key_ctx` caches each ancestor it derives, and all
  children share one parent, so it is a hit after the first child.
- *The entity cache.* `InMemoryCacheRepository` is `HashMap`-backed, O(1) `set`.
- *FTS (cause 1).* Linear — 3.1 s → 5.3 s where the cliff is 6.1 s → 38.8 s.

**Next step:** symbol-level profile. Needs an unstripped `pdfs` installed and the
daemon restarted, then `perf record -p <pid>` across a 793-entry enumerate. Every
cheaper avenue above has been spent. Note a second daemon is **not** an option
for this: Proton refresh tokens are single-use, so a parallel session would fight
the running daemon for them.

**Do not implement improvements.md P2.8 before this is understood.** Pipelining
would parallelize the cliff rather than remove it, and at 793 entries the cliff is
33 s of the 38.8 s — far more than concurrency could win back.

---

## B13 — `rename` over an existing destination fails instead of replacing it

**Status:** Fixed (unverified — needs a real rsync against the rebuilt daemon)
**Found:** 2026-07-20, user ran `rsync -rauLP ~/ProtonDrive/Music/... ~/Music/`
(both are protondrive mounts) and every single file failed at the end of its
transfer:

```
rsync: [receiver] rename ".../.Buunshin - heimwee (Original Mix).wav.b0akm3"
    -> ".../Buunshin - heimwee (Original Mix).wav": Input/output error (5)
```

**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `Filesystem::rename`

Daemon log, one per failed file:

```
ERROR pdfs_fuse: rename failed uid=… error=proton api error AlreadyExists (http 422):
    A file or folder with that name already exists
WARN  pdfs_fuse::drain: pending upload failed; will retry uid=… attempts=1
    error=proton api error DoesNotExist (http 422): File or folder not found
```

**Cause:** `rename(2)` is specified to *atomically replace* an existing
destination. Our handler just calls `client.rename_node`, and Proton refuses a
name that already exists — so the 422 becomes a blanket `EIO`. Nothing in the
path ever looks up the destination name.

This breaks every write-to-temp-then-rename tool, which is most of them: rsync,
editors doing atomic saves, `mv -f`, package managers. The failure is at the
*end* of the transfer, so the bytes are uploaded and then thrown away — the user
pays full upload cost for nothing.

**Second-order damage:** the failed rename leaves the temp node behind and its
queued upload then fails with `DoesNotExist` and retries forever. So each failed
file also leaves a poisoned entry in the drain queue.

**Fix:** `rename` now looks up `newname` under `newparent` and, if it resolves to
a different node, removes it before the API call.

The order is forced by Proton: the name has to be free before `rename_node` will
take it, so it is trash-then-rename, which means the operation is **not atomic**.
`Core::remove_replaced` does the trashing and `Core::restore_replaced` puts the
node back if the rename then fails — on every failing path, including the queued
ones. If the restore *also* fails the node stays in the trash and says so
loudly, because the alternative is a file the user believes was merely renamed
sitting somewhere they will not look.

Deliberate details:

- **Refusals happen before anything is trashed.** `check_replaceable` is a pure
  function for exactly this reason: every `Err` it returns is a case where the
  destination must survive, and a mistake there turns a refusal into deletion.
  `EISDIR` / `ENOTDIR` when the two ends disagree about being a directory, and
  `ENOTEMPTY` for a non-empty destination directory — Proton trashes a folder
  with its whole subtree, so allowing that would discard every file underneath
  without ever naming them.
- **`RenameFlags` is now read** (it was `_flags`). `RENAME_NOREPLACE` returns
  `EEXIST` instead of replacing; `RENAME_EXCHANGE` returns `EINVAL`, since no
  Proton primitive swaps two names and emulating it would leave a window in
  which one of them does not exist.
- A node whose own create is still queued never reached the server, so replacing
  it just discards its queued ops — no API call, and it works offline.
- Renaming a node onto its own name stays a no-op rather than a self-replace.

**Tests:** `replace_tests` in `pdfs-fuse/src/lib.rs` covers all five decisions.
The trash/restore half needs a live server and is not covered.

**Repro:** `cd <mount> && echo a > x && echo b > y && mv y x` → was EIO, should
now succeed with `x` holding `b`.

---

## B14 — provisional (ciphertext) sizes make rsync read short and abort the file

**Status:** Fixed (unverified — needs a cold folder on the rebuilt daemon)
**Found:** 2026-07-20, same rsync run as B13. Alongside the receiver errors, the
sender failed to read its own source files:

```
rsync: [sender] read errors mapping "/home/narl/ProtonDrive/Music/…/
    Buunshin - i think i feel... (Original Mix).wav": No data available (61)
```

**Where:** `node_size` (`pdfs-fuse/src/lib.rs:3480`) + `Core::spawn_size_upgrade`

**Cause:** this is B12's known tradeoff surfacing as a real transfer failure.
Until the background size upgrade lands, `node_size` falls back to
`total_size_on_storage` — the **ciphertext** size, which is larger than the
plaintext (measured at +59 bytes on this account). B12 called that "cosmetic and
self-corrects" because it only affects `ls -l`.

It does not only affect `ls -l`. `ENODATA` (61) is what **rsync sets on a short
read**: `map_ptr` in `fileio.c` asks for the bytes `stat` promised, gets fewer,
and marks the mapping `ENODATA`. So a reader that trusts `st_size` — rsync,
anything using `mmap`, `sendfile`, or a sized `read` loop — sees a truncated
file and errors out. That is the same class of bug as B11, just from the other
direction: B11 reported **too small** (0) so the kernel skipped reads entirely;
this reports **too large** so reads run off the end.

The 30 s attr TTL makes the window much wider than the upgrade's own latency
(B12 documented this), so a cold `rsync` of a large tree is essentially
guaranteed to hit it.

**Why it looked fine afterwards:** re-listing the same directory now shows every
size correct, because the upgrade has long since landed. The bug is only visible
cold. Reproducing it needs `pdfs refresh <dir>` immediately before the read.

**Fix:** a provisional size is never published. `getattr` on a file whose
`claimed_size` is unknown now resolves it *before* replying, instead of replying
with the ciphertext size and upgrading afterwards.

The cost is one batched round trip per folder, not one per file: `ls -l` is one
`getattr` per entry, and they collapse onto a single upgrade. B12's split is
still doing its job — a plain `ls` never reaches this path at all, which is
where the 8.7× came from.

That required making the upgrade *awaitable*, which is most of the change:

- `size_upgrades` went from `HashSet<u64>` to `HashMap<u64, Arc<SizeUpgrade>>`,
  a condvar per folder. Followers wait on it; there may be hundreds for one
  folder, so it releases all of them.
- **The leader does the fetch on its own thread rather than handing it to a
  worker.** This is the part to not undo: `getattr` waits on `Lane::Meta`, so a
  leader that queued its fetch onto that same lane could have a wide enough
  `ls -l` fill the lane with threads waiting for a job that can never be
  scheduled.
- `SizeUpgrade::WAIT` caps the wait at 10 s, falling back to the provisional
  size. A `stat` that never returns is worse than one that is briefly wrong.
- `upgrade_sizes` owns the single-flight bookkeeping and has one exit path;
  `apply_size_upgrade` holds the body it used to inline, so a failed fetch still
  releases the waiters and leaves the folder retryable.

**Tests:** `size_upgrade_tests` covers the follower being released, a waiter
arriving after `finish` (it checks the flag, not the notification, which it
would miss), and all waiters waking.

**Not covered, worth watching:** `Lane::Meta` can now hold waiting `getattr`s.
There is no deadlock — leaders never queue their own work — but a cold `ls -l`
across many folders at once could occupy the lane. If metadata ops start
feeling sticky under heavy cold listing, this is the first suspect.

### Verified (2026-07-20) — correct, but expensive, and not completely closed

Two further holes turned up during verification; both are fixed, and the second
is the one that mattered.

1. **`upgrade_sizes_for_parent` early-returned** when the parent listing was not
   resident in `children` — and a rename invalidates exactly that, so a freshly
   renamed file always landed in the dead branch. `stat` read 67 bytes for a
   16-byte file, and `cat` returned the 16 real bytes followed by 51 NULs. The
   same shape as B4: an early return that assumed the hot cache was
   authoritative. Now falls back to resolving the single node, keyed by its own
   inode.
2. **`lookup` replies with attrs and a TTL too.** With no `readdirplus`, `ls -l`
   is one `lookup` per entry and *zero* `getattr` calls — so fixing only
   `getattr` fixed a path `ls -l` never takes. This is worth remembering: the
   first fix looked right and did nothing for the reported symptom.
   `serve_lookup` now resolves as well, taking an `off_loop` flag because
   `lookup`'s warm path runs on the dispatch loop where it may not block (B5).

| measurement | before fix | after fix |
|---|---|---|
| cold `ls -l`, 7-file wav folder | ciphertext sizes | **exact settled sizes** |
| renamed file `stat` / `cat` | 67 B, 51 NULs | **16 B, clean** |
| cold plain `ls`, 793 entries | 4.48 s | **4.58 s** (B12 intact) |
| cold `ls -l`, 793 entries | ~4.5 s, wrong sizes | **85.8 s**, 785/793 correct |

### The cost, and the part still open

**A cold `ls -l` of a 793-entry folder went from ~4.5 s to 85.8 s.** That is not
a flaw in the batching — it is B12's cause 2 arriving on schedule. A real
`claimed_size` requires the per-file node-key unlock, i.e. one S2K per file, and
that work is single-threaded. B12 removed it from the *listing* path; asking for
sizes puts it back, because sizes are what it produces. Plain `ls` is untouched,
which is why the split is still worth having.

**8 of 793 entries still came back provisional**, caught by diffing the cold
listing against a settled one. Those are `SizeUpgrade::WAIT` timeouts: the leader
takes ~80 s for the batch and a waiter gives up at 10 s. So the bug is rarer but
not gone — it now needs a folder large enough that the batch outruns the cap.

### Per-chunk wakeup (done, unverified)

The timeout gap is closed by releasing waiters **per chunk** instead of once at
the end. `SizeUpgrade` carries a generation counter; `run_size_upgrade` fetches
in `SIZE_UPGRADE_CHUNK` (150, the SDK's own `MAX_BATCH_COUNT`, so one chunk is
one request), applies each chunk, and bumps the generation. A waiter re-checks
whether *its own* node now has a real size and returns if so — it no longer
waits on the other 792.

Two structural points worth keeping:

- **The batch runs on its own thread, not the `Workers` pool.** Callers wait on
  `Lane::Meta`, so queueing the batch there could let a wide `ls -l` fill the
  lane with threads waiting for a job that can never be scheduled.
  `Lane::Transfer` would swap that deadlock for starvation behind bulk reads.
  One short-lived thread per folder, bounded by the single-flight, avoids both.
- **`wait_for` evaluates its predicate with no `SizeUpgrade` lock held.** The
  predicate reads `state`, and the applying thread holds `state` before it
  signals; taking them in the other order would close the cycle.

This does **not** make a full cold `ls -l` faster — every size still has to be
computed. It makes each individual `stat` return after its own chunk rather than
after the whole folder, and removes the provisional-size fallback that the 10 s
cap was producing.

**Tests:** `size_upgrade_tests` covers the resolving chunk releasing a waiter,
an unrelated chunk *not* releasing one, `finish` releasing an unresolved waiter,
the already-resolved and post-`finish` arrivals, all waiters waking, and the
timeout backstop still firing.

---

## Plan: parallelise the per-file S2K (the 85 s)

This is the only item that reduces the work rather than redistributing it, and
it is the same bottleneck as improvements.md PERF #5 — approached from sizes
rather than from cold navigation. Filed here because B12/B14 are what produced
the measurements it needs.

**The claim to be tested first, before any code.** B12's profile of a 793-entry
cold enumerate attributed **64.4 % of cycles to `sha2::sha256::x86::digest_blocks`
and ~74 % overall to SHA-256 inside PGP's S2K**, i.e. the per-file node-key
unlock in `build_node` under `NodeDetail::Full`. If that still holds for the
*size upgrade* path specifically — it is the same `enumerate_nodes` call, so it
should — then the work is CPU-bound, embarrassingly parallel across files, and
currently serialised on one thread.

**Step 0 — confirm, do not assume.** Re-profile against the size-upgrade path
now that it is the thing on the critical path:

```
perf buildid-cache -a target/release/pdfs      # symbols; see B12's note on strip
perf record -p <daemon pid> -- sleep 90
# meanwhile: pdfs refresh Music/aC_ID.dll && ls -l Music/aC_ID.dll
```

Expect the same SHA-256 leaf. If it is *not* dominant, stop — the rest of this
plan is aimed at the wrong thing.

**Step 1 — establish the ceiling.** The upgrade is `enumerate_nodes` over 793
uids. Measure single-node unlock cost directly (log the S2K iteration count and
elapsed time per file across one folder — this also settles the question B12 left
open, whether the 49 ms/entry folder is expensive because of *n* or because the
uploading client chose a costlier S2K). Ideal speedup is bounded by core count
and by how uneven those per-file costs are.

**Step 2 — move the crypto off the async runtime.** The decryption currently runs
inline in the SDK's `enumerate_nodes` future. Blocking CPU work on a Tokio worker
starves everything else sharing that runtime. Wrap the per-node unlock in
`spawn_blocking`, or hand the whole chunk to a `rayon` pool and await the result.
This is the change PERF #5 describes and is a **`proton-drive-rs` change, not a
`pdfs` one** — which is why it was deferred: it needs an SDK release, and the
workspace pins `"0.1"` so a `cargo update` picks it up.

**Step 3 — parallelise across files within a chunk.** 150 nodes per chunk, each
independent once the parent key is resolved. `resolve_parent_key_ctx` caches the
ancestor key and all children of a folder share one parent, so the parallel
region needs read-only access to it — take the folder key once, then fan out.
Watch `FOLDER_KEY_CACHE_CAP`/`folder_keys` (PERF #9 made it an LRU) for
contention if the fan-out takes that lock per node.

**Step 4 — re-measure the same three folders**, which have before/after numbers
already recorded above and in B12:

| folder | cold plain `ls` | cold `ls -l` (today) |
|---|---|---|
| `Music/aC_ID.dll` (793) | 4.58 s | 85.8 s |
| `InstantUpload/Camera` (484) | 2.91 s | not measured |
| `Pictures/old` (417) | 2.25 s | not measured |

Fill in the middle column's `ls -l` first — there is only one data point for the
regression right now, and B12's "cliff past ~500" framing was wrong precisely
because it generalised from a single folder. **Do not repeat that mistake here.**

**What success looks like:** cold `ls -l` on the 793-entry folder within ~2–3× of
the plain `ls`, rather than 19×. Perfect scaling is not available — the request
round trips are serial per chunk and only the crypto parallelises.

**Explicitly out of scope:** `readdirplus`. It would cut the *number* of calls
(one reply per directory instead of one `lookup` per entry) and is the right
long-term shape for the directory-read path, but it does not remove a single S2K
— the sizes still have to be computed. It is a separate change and should not be
bundled in, for the same reason B5 declined it: it would hide this bug rather
than fix it.

**Do not** attempt to derive the plaintext size from `total_size_on_storage`
instead. The delta is not a constant (+564 B on a 39 MB file, +51 B on a 16-byte
one — it scales with block count), so the arithmetic would have to model Proton's
block framing exactly, and being wrong in the *small* direction reproduces B11,
where a short size made the kernel skip reads entirely.

**Verify with:** the ciphertext delta is the probe (+564 B on a 39 MB file,
+51 B on a 16-byte one — it scales with block count, so it is not a constant).
`pdfs refresh` a folder, then `ls -l` immediately and diff against a settled
listing.

---

## B15 — an empty-but-`listed` folder, and duplicated folder uids

**Status:** Open (side finding, unattributed). Originally filed as "a sync-folder
listing came back empty"; that part was investigated and retracted — see below.
**Found:** 2026-07-20, while confirming B13.

`ls ~/Music/Buunshin/not everythin is your fault (wav)/` returns `total 0` — no
entries — even though the rename failures in B13 prove files existed under those
names minutes earlier (Proton rejected the rename *because* the name was taken).
The same folder read through the primary mount
(`~/ProtonDrive/Music/Buunshin/…`) lists all seven `.wav` files correctly.

**Checked (2026-07-20) — the "stale empty listing" reading was wrong.** The
folder `not everythin is your fault (wav)` that read empty has **`listed = 0`**
in `nodes`, so its `ls` was a real network enumeration, not a cached empty one.
The files genuinely are not there: every rename in B13 failed and rsync
discarded its temp files. B15 as originally filed does not exist.

**What the query did turn up, and is worth keeping:**

1. **A real B1-shaped row.** `not everything is your fault` (no `(wav)`) reads
   `listed = 1, trashed = 0` with **zero child rows** — the exact
   empty-but-listed state B1 was about, still present in the live DB, reached by
   some route the B1 fix does not cover. This one is worth chasing.
2. **Duplicated folders.** `Music` and `Buunshin` each exist under **two
   distinct uids**, and the `(wav)` folder name appears twice as well — one copy
   populated (7 children), one empty. So the remote tree has picked up duplicate
   directories somewhere. Related to the self-conflict duplication already
   recorded in memory, but not yet attributed here.

Both are folder-identity problems rather than the mount disagreement originally
suspected. Renamed scope accordingly; the "two mounts disagree" framing is
retracted.

---

## B16 — aria2c preallocation / rapid sequential writes trigger false self-conflict copies

**Status:** Fixed (verified 2026-07-21)
**Found:** 2026-07-21, user reported sync conflict files created during torrent downloads with aria2c onto the FUSE mount (`[DB]Oshi no Ko 3rd Season_-_05... (sync-conflict 1784644482).mkv`).
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/drain.rs`, `crates/pdfs-core/src/db/ops.rs`

**Cause:** Tools like `aria2c` preallocate target file sizes using `ftruncate`/`setattr` and then write actual content across multiple file opens or handle releases.
1. First close (e.g. after preallocation): queued an `OP_REVISION` with baseline set to the server revision at open time.
2. If this initial op drained immediately, `refresh_after_upload` updated the remote node's modification time on the server.
3. Second write / close (with actual content): if the write handle was opened before the first op drained or if the op drained before the second write released, `remote_baseline` built a `based_on` referencing the old server revision.
4. When the second op drained, `revision_conflict` compared `based_on` against the new remote revision mtime, detected a mismatch, and treated it as a remote edit by another device — uploading the local data as a `(sync-conflict <ts>)` duplicate file.

**Fix:** A two-part defense:
1. **Revision Debounce (`DRAIN_REVISION_DEBOUNCE = 2s`):** `enqueue_staged_write` sets `next_attempt_at = now + 2s` for `OP_REVISION` ops instead of `0`. Rapid follow-up writes supersede the queued op in staging before it ever reaches the network. Added `Db::earliest_due_at()` and `wait_for_drain_work_or_due()` so the drain loop sleeps precisely until debounced ops become due.
2. **Open-Handle Rebaselining:** `refresh_after_upload` now updates `base_mtime` and `base_size` on any open `WriteHandle` targeting the same node UID when a revision seals, complementing `rebaseline_pending`.

**Verified:** Clean build and 200/200 workspace tests passing.

**Residual (2026-07-24):** the two-part defense narrowed the window but did not close it — the baseline was still keyed on `(mtime, size)`, which drifts when the server re-stamps the *same* revision. Reproduced again on a plain application save (`test_export.xml`): base mtime `1784897777` vs sealed `1784897780`, identical size, no other device involved. The root cause is that mtime is not a revision identity. Fixed properly by **B69**.

---

## B17 — FUSE mount lock contention, missing next_attempt_at in DB enqueue, and ioctl ENOTTY fix

**Status:** Fixed (verified 2026-07-21)
**Found:** 2026-07-21, deep code audit of `fix/ioctl-handler` branch.
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-core/src/db/ops.rs`

**Cause:**
1. `Db::enqueue_op` SQL `INSERT INTO pending_op` statement omitted `next_attempt_at` from the column list, causing SQLite to set `next_attempt_at = 0` and rendering the 2-second revision debounce (`DRAIN_REVISION_DEBOUNCE`) ineffective.
2. `ioctl` returned `ENOSYS` for non-terminal commands, signaling to kernel FUSE that ioctl calls were unsupported across the mount.
3. `fallocate` executed `libc::fallocate` while holding `self.core.state.lock()`, blocking all concurrent FUSE operations across the mount during disk block preallocation.
4. `open`/`create` invoked `create_scratch()` prior to checking `st.active_writes`, leaking scratch files on disk during concurrent opens.

**Fix:**
1. Updated `Db::enqueue_op` in `ops.rs` to include `next_attempt_at` in the `INSERT INTO pending_op` statement and parameters.
2. Changed `ioctl` handler in `lib.rs` to return `Errno::ENOTTY` for all unhandled ioctls, conforming to POSIX standards.
3. Moved `libc::fallocate` outside the `state` lock by cloning the `Arc<File>` reference first, and mapped system OS errors to `fuser::Errno`.
4. Refactored `open` and `create` to check `st.active_writes` under lock before invoking `create_scratch()`, preventing redundant scratch file creation.

**Verified:** Workspace unit tests (200/200) and `clippy` passing cleanly.

---

## B18 — Subtree Erasure via POSIX `unlink` / `rmdir` (CRIT-01)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent filesystem safety audit (`audit_bugs.md` CRIT-01)  
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::unlink`, `ProtonFs::rmdir`

**Cause:** `unlink` on directories and `rmdir` on non-empty directories bypassed POSIX checks and called `trash_child` directly, silently deleting remote subtrees.

**Fix:** Added type and non-emptiness checks in `lib.rs`:
- `unlink`: Returns `Errno::EISDIR` if target is a folder.
- `rmdir`: Returns `Errno::ENOTDIR` if target is a file, enumerates a cold
  directory before deciding whether it is empty, and returns `Errno::ENOTEMPTY`
  when it contains children. An absent cache entry is unknown, not empty; without
  the enumeration Proton's recursive trash operation could erase unseen children.

**Verified:**
- Unit tests: `test_posix_unlink_and_rmdir_checks` in `lib.rs` passing cleanly.
- Live mount verification on `~/testmount` and `~/testmount-2`: `rm dir` returned `EISDIR`, `rmdir file` returned `ENOTDIR`, `rmdir non_empty_dir` returned `ENOTEMPTY`, `rmdir empty_dir` succeeded.

---

## B19 — Remote Folder Trashing on Mode Switch Failure (CRIT-02)

**Status:** Fixed; all four managed-live mode pairs verified 2026-07-22,
fault-injected mount failure still pending
**Found:** 2026-07-21, multi-agent sync engine audit (`audit_bugs.md` CRIT-02)  
**Where:** `crates/pdfs-fuse/src/devices.rs`, `apply_sync_folder_mode`

**Cause:** The first fix mounted FUSE before calling `evict_dir_contents`. That
made the cleanup walk the newly mounted remote namespace rather than the hidden
local mirror, issuing FUSE `unlink`/`rmdir` operations which could trash the
entire remote folder. The local files were not reclaimed at all.

**Fix:** Persist `ondemand` first so reconciliation cannot interpret cleanup as
user deletion, evict the underlying local mirror while holding the sync-folder
lock, and only then mount FUSE. A mount failure leaves an inert `ondemand` row;
switching back to `mirror` clears the baseline and restores the local copy.

**Verified:** `cargo clippy -p pdfs-fuse --all-targets -- -D warnings`, the
`pdfs-fuse` unit suite, and the complete managed-live matrix passed against the
1.0.0 release binary. A fault-injected mount-failure test is still needed.

---

## B20 — Un-fsynced Temp File Atomic Rename in Content Cache (CRIT-03)

**Status:** Fixed (verified 2026-07-21)  
**Found:** 2026-07-21, multi-agent storage audit (`audit_bugs.md` CRIT-03)  
**Where:** `crates/pdfs-core/src/cache.rs`, `ContentCache::store`

**Cause:** Cache store renamed temporary files into the cache directory without explicit `fsync`. Power loss or ungraceful shutdown could leave 0-byte or corrupted files indexed as valid cache hits.

**Fix:** Added `f.sync_all()?` calls on open file handles prior to executing `std::fs::rename`.

**Verified:** `cargo test -p pdfs-core` suite passing cleanly.

---

## B21 — Untracked Hole Punching in `fallocate` (HIGH-01)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent FUSE audit (`audit_bugs.md` HIGH-01)  
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::fallocate`

**Cause:** `FALLOC_FL_PUNCH_HOLE` zeroed scratch blocks but removed their
`WriteHandle::written` authored status. Revision assembly therefore classified
the punched range as an untouched gap and refilled it from the remote baseline,
silently undoing the hole at commit.

**Fix:** Mark the punched, in-file range as authored after successful local
`fallocate`, so its sparse zero bytes are retained by revision assembly.

**Verified:**
- Live mount verification: `libc.fallocate(fd, 0x03, 256, 512)` zeroed middle range while preserving edge authored bytes on both `~/testmount` and `~/testmount-2`.

---

## B22 — Synchronous `block_on` inside Mutex Guard (HIGH-02)

**Status:** Fixed (unverified under load) 2026-08-16 — the systematic audit this was waiting on
has been done and found nothing left.
**Found:** 2026-07-21, multi-agent concurrency audit (`audit_bugs.md` HIGH-02)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `sync.rs`, `devices.rs`

**Cause:** Invoking `rt.block_on(...)` while holding `state.lock()` deadlocks worker threads and the main FUSE loop under high concurrency. Most major handlers have been refactored to drop `state.lock()` before invoking async runtime methods, but a full systematic audit across all background tasks and FUSE callbacks is required to eliminate all instances.

**Audit (2026-08-16).** All 131 `block_on` sites in `pdfs-fuse` were scanned mechanically: for
each, every guard binding (`self.state()` or any `.lock()`) still lexically live at that point,
accounting for explicit `drop(..)` and scope exits. Two hits, both false positives —
`devices.rs:570` binds `taken`, the `Option<Mount>` *taken out of* the map (the guard is a
temporary, and `_guard` is explicitly dropped above), and `lib.rs:1917/1927` binds `resident`,
a `bool` read out of a temporary guard. `pdfs-core` contains no `block_on` at all. The reverse
direction — a guard held across an `.await` — is covered by `clippy::await_holding_lock`, which
is on in the CI gate.

The scan is lexical, so it cannot see a guard passed into a callee; it is evidence, not proof,
which is why this is *unverified*. What would settle it is the concurrency half of the acceptance
suite run against a heavily loaded drain.

---

## B23 — Un-fsynced Scratch Wipe on Daemon Restart (HIGH-03)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent recovery audit (`audit_bugs.md` HIGH-03)  
**Where:** `crates/pdfs-core/src/cache.rs:L741`, `ContentCache::rescue_scratch`

**Cause:** On daemon startup, `rescue_scratch` inspected the scratch directory and only preserved files with valid `.json` sidecars created by `fsync`. Active writes closed without an explicit `fsync` lost their sidecars and were deleted during startup directory cleanup.

**Fix:** Updated `rescue_scratch` in `cache.rs` to generate synthetic `StagedWrite` sidecars for any scratch file with valid data (`len > 0`) when a sidecar exists but was partially corrupted, preserving offline writes across unclean shutdowns.

**Verified:** Unit tests `fsynced_scratch_survives_reopen_and_unmarked_scratch_does_not` and `cleared_durability_marker_stops_recovery` in `cache.rs` passing 100%.

---

## B24 — Directory Hierarchy Loop in `rename` (HIGH-04)

**Status:** Fixed (verified 2026-07-21)  
**Found:** 2026-07-21, multi-agent FUSE audit (`audit_bugs.md` HIGH-04)  
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::rename`, `crates/pdfs-fuse/src/state.rs:L237`

**Cause:** Moving a directory into one of its own subdirectories created an infinite recursive loop in inode state memory and DB tree structure.

**Fix:** Added `State::is_ancestor_of` helper in `state.rs` that walks up parent chains. `rename` checks `st.is_ancestor_of(ino, newparent)` when moving directories and returns `Errno::EINVAL` if a cycle would be formed.

**Verified:**
- Unit tests: `test_is_ancestor_of_hierarchy` in `state.rs` passing.
- Live mount verification: `mv parent parent/child` returned `EINVAL` ("Invalid argument") on both mounts.

---

## B25 — Weak Conflict Baseline Detection `(mtime, size)` (HIGH-05)

**Status:** Fixed and **live-verified 2026-08-16** on 1.8.1 — the FUSE-write half was closed by
**B69**; the mirror half is closed here.  

**Live verification (2026-08-16, `~/pdfs-live-mirror`, folder 6).** `b25.txt` was written with
mtime `12:00:10.100000000`, synced (baseline `local_mtime=1786874410`,
`local_mtime_ns=1786874410100000000`), then rewritten with sixteen different bytes and stamped
`12:00:10.900000000` — identical whole second, identical length. The next pass classified it as
locally changed and uploaded `b25.txt new version`; the baseline advanced to
`…410900000000`, and reading the file back through an on-demand mount of the same folder returned
`BBBBBBBBBBBBBBBB`. Under the old whole-second comparison this edit was invisible.  
**Found:** 2026-07-21, multi-agent sync engine audit (`audit_bugs.md` HIGH-05)  
**Where:** `crates/pdfs-core/src/cache.rs:L157`, `Baseline`

**Cause:** Conflict resolution baseline detection relies strictly on `(mtime, size)` tuple comparisons without content hashing or revision IDs. Concurrent edits landing within the same second that produce identical file sizes match baselines, causing offline edits to overwrite remote changes silently without creating `(sync-conflict <ts>)` copies.

**Fix (2026-08-16), part one — the FUSE write path.** Closed by **B69**: `Baseline` gained
`revision_id`, and `revision_changed` keys on the server revision id — the true identity, which
advances if and only if a new revision was sealed — falling back to `(mtime, size)` only for a
sidecar written before the field existed.

**Fix (2026-08-16), part two — the mirror sync path.** B69 did not reach it: a mirror folder's
baseline is `sync_entry`, and its local side was still whole seconds. An edit made in the same
second as the last sync that left the file the same length — a fixed-size record rewritten in
place, an image re-exported, a database page updated — read as *unchanged* and was never
uploaded, leaving the remote silently behind until something else about the file moved.

`sync_entry` gains `local_mtime_ns` (schema **28**), and the comparison moved into
`LocalSig::same_content`, which uses nanoseconds when both sides have them and whole seconds
otherwise. The column is nullable rather than a converted `local_mtime * 1e9` on purpose: the
sub-second part of an existing baseline is genuinely unknown, and inventing zero for it would make
every already-synced file compare as changed and re-upload an entire mirror on first start. `NULL`
means "compare seconds", which is exactly the behaviour that row was written under; each row gains
a real value the next time its path syncs.

Covered by `db::tests::migration_v28_adds_sub_second_local_times_without_disturbing_a_v27_baseline`
and `sync::planner::file_plan_tests::a_same_second_same_size_edit_is_still_a_local_change`.

**Still open (deliberately):** the *remote* side of a mirror baseline is still `(mtime, size)`
(`remote_rev`/`remote_hash`). Re-keying it onto `active_revision_id` the way B69 did for the FUSE
path is the same idea and the same size of change, but the stored column already holds
mtime-shaped strings, and telling an old row from a new one would need a heuristic on the value's
shape. That deserves its own column and its own pass rather than a guess.

**To verify:** on a mirror folder, `printf %s aaaa > f && sync && printf %s bbbb > f` inside one
second, then a reconcile pass — expect an upload, and the remote content to be `bbbb`.

---

## B26 — Broken Open-Unlinked File Semantics (HIGH-06)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent FUSE audit (`audit_bugs.md` HIGH-06)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/state.rs`

**Cause:** Unlinking an open file called `trash_child`, which invalidated the node and dropped it from state immediately. Subsequent `read` calls on open file handles failed with `ENOENT` / `EIO`.

**Fix:** 
1. Added `open_count` and `unlinked` fields to `Entry` in `state.rs`.
2. `open` increments `open_count`, `release` decrements `open_count`.
3. `forget_or_unlink` marks `entry.unlinked = true` and removes `ino` from `st.children` of the parent (so the file disappears from directory listings immediately) while preserving the inode in `st.entries`.
4. `read` calculates optimistic `fsize` falling back to `core.pending` length for unlinked/pending files.
5. Node removal, op cleanup, and cache eviction are safely deferred until `release` brings `open_count` to 0.
6. `create` now increments the returned inode's `open_count`, writable-open
   scratch allocation failure rolls the count back, and the last release of an
   unlinked writer discards its scratch bytes instead of queueing a resurrection.

**Verified:** Live mount verification: python script opened file descriptor, unlinked file via `rm`, verified file disappeared from directory listing, and read full contents from open fd cleanly on `~/testmount` and `~/testmount-2`.

---

## B27 — Orphaned Write Revision Bricking Offline Sync Queue (HIGH-07)

**Status:** Fixed in code (unverified create→unlink live sequence, 2026-07-22)
**Found:** 2026-07-21, multi-agent drain engine audit (`audit_bugs.md` HIGH-07)  
**Where:** `crates/pdfs-fuse/src/lib.rs:L1811`, `crates/pdfs-fuse/src/drain.rs`

**Cause:** Unlinking a newly created offline file discards its `Create` pending op, but closing an open write handle on the file queued a `Write` revision for `local~...`.

**Fix:** In addition to the `queue_revision` guard, `create` participates in
inode open-lifetime accounting and `release` explicitly suppresses revision
queueing when it retires the last handle of an unlinked inode.

**Verified:** Code inspection in `lib.rs:1811` and workspace tests passing.

---

## B28 — Unhandled Disk Exhaustion (`ENOSPC`) in Cache & Scratch Store (MED-01)

**Status:** Fixed in code — previous remediation was unsafe (2026-07-22)
**Found:** 2026-07-21, multi-agent storage audit (`audit_bugs.md` MED-01)  
**Where:** `crates/pdfs-core/src/cache.rs`, `emergency_evict`

**Cause:** Running out of local disk space caused unhandled `EIO` errors without triggering emergency eviction of unpinned cache blobs.

**Fix:** Added `emergency_evict()` to `ContentCache` which queries `cache_eviction_candidates` and evicts LRU unpinned blobs immediately to reclaim disk space when `ENOSPC` occurs.

**Verified:** `cargo test -p pdfs-core` suite passing cleanly.

---

## B29 — Missing FUSE `forget` Method / Memory Leak (MED-02)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent FUSE state audit (`audit_bugs.md` MED-02)  
**Where:** `crates/pdfs-fuse/src/lib.rs:L3983`, `crates/pdfs-fuse/src/state.rs`

**Cause:** Kernel `forget` messages sent by FUSE were ignored, causing `state.entries` and `state.by_uid` maps to grow monotonically in RAM over long daemon uptimes.

**Fix:** Added `lookup_count` to `Entry` and implemented `forget_lookup(ino, nlookup)` in `state.rs`. `ProtonFs::forget` now invokes `st.forget_lookup(ino.0, nlookup)` to prune unreferenced inodes from memory.

**Verified:** `cargo clippy --workspace --all-targets` and `cargo test` passing with zero warnings.

---

## B30 — SQLite Write Lock & Mutex Contention under Heavy Drain (MED-03)

**Status:** Fixed (unverified under load) 2026-08-16 — closed by the work filed under the
performance audit rather than by a change made for this entry.  
**Found:** 2026-07-21, multi-agent database audit (`audit_bugs.md` MED-03)  
**Where:** `crates/pdfs-core/src/db/ops.rs`, `crates/pdfs-core/src/db/mod.rs`

**Cause:** Long-running upload drain transactions hold SQLite write locks while worker threads wait for `state.lock()`, causing periodic FUSE response latency spikes during heavy sync operations.

**Audit (2026-08-16).** Each of the three mechanisms behind that sentence is gone:

1. **No transaction spans a network call.** Every `Db` method opens its transaction, commits and
   returns; nothing holds one open across calls (the invariant is stated on `Db::read`). A drain
   op's upload happens between database calls, not inside one.
2. **Reads no longer queue behind the writer.** The read-only connection pool (`Db::read`) gives
   `SELECT`-only methods their own connections, so a FUSE metadata call does not wait on whatever
   is committing. Asserted by `db::tests::a_read_does_not_wait_for_the_write_connection`, which
   holds the write connection for the whole measurement.
3. **The queue is no longer read whole to pick one op.** `Db::claim_next_due_op` selects and
   claims a single row in the database, which is what made a long queue quadratic to drain while
   holding the shared connection.

The `state.lock()` half is covered from the other side: **B75** moved network work off fuser's
dispatch thread, and **B22**'s audit found no `block_on` under a `State` guard.

Unverified because "periodic latency spikes" is a load property, not a code property: it is
closed by construction and wants the concurrency acceptance case under a loaded drain to confirm
it in the field.

---

## B31 — Unimplemented Special Files & Attribute Modifications (MED-04)

**Status:** Not a bug — by design, verified 2026-08-16  
**Found:** 2026-07-21, multi-agent POSIX audit (`audit_bugs.md` MED-04)  
**Where:** `crates/pdfs-fuse/src/lib.rs`

**Cause:** Symlinks (`symlink`/`readlink`), hardlinks (`link`), FIFOs/sockets (`mknod`), and `chmod`/`chown` attribute modifications return `ENOSYS` or are silently ignored.

**Verdict (2026-08-16).** Each of these is the correct answer, not a gap, and each errno is now a
deliberate choice with a comment saying why:

- `symlink` → **EPERM**. The operation is meaningful; Drive simply cannot represent it. Callers
  treat `EPERM` as a definite "no" instead of retrying against an older interface.
- `link` → **EPERM**. A node has exactly one parent on Drive, so there are no hard links. git
  falls back to `rename()` on `EPERM`, which this filesystem supports.
- `mknod` → **ENOSYS**. Devices, FIFOs and sockets have no representation at all. Regular files
  arrive through `create`, never here.
- `chmod`/`chown`/`utimes` → accepted and ignored, so tools that chmod or restamp after writing
  succeed rather than failing on a filesystem with no per-file mode to set.

Implementing any of them would mean inventing a local-only representation that no other Proton
client can read — a worse outcome than a clean refusal. The errno contract is asserted by the
acceptance suite's clean-refusal case so it cannot drift, and **B73** removed the per-probe WARN
that made these look like incidents in the journal.

---

## B32 — Race Condition in Concurrent Handle Release and Open (MED-05)

**Status:** Fixed and **live-verified 2026-08-16** on 1.8.1  

**Live verification (2026-08-16).** Three 64 MiB random files were written over the same mount
path (`ProtonDrive/pdfs-live-20260816/b32.bin`) back to back, each `cp` closing before the next
began, while the drain was still working. The queue coalesced them into a single op (29 queued =
28 known-stuck + 1). After the drain retired it, `pdfs refresh` dropped the cached listing and a
full re-read from Drive returned `4ad41482f5199a401e425381d7621626` — the md5 of the *third*
write, byte for byte. No open re-used a stale staged base, and the retry warning never had to
fire.  
**Found:** 2026-07-21, multi-agent concurrency audit (`audit_bugs.md` MED-05)  
**Where:** `crates/pdfs-fuse/src/lib.rs:L4042, L4574`

**Cause:** Opening a file for writing while a previous handle release is draining creates overlapping scratch write handles and out-of-order remote revision uploads.

**Fix (2026-08-16).** The concrete hole was in `serve_open`: it sampled the queued revision
(`Core::pending`), then copied that blob into a fresh scratch file, then took the `State` lock to
install the handle. A `release` publishing a newer revision anywhere inside that gap — which spans
a copy the size of the whole file — left the new handle carrying the *older* base, and its own
release then published content that silently discarded the revision closed a moment earlier.

The install is now guarded: the pending blob path (which changes on every publication, so it
identifies the revision) is re-read immediately before the `State` lock, and if it moved, the
scratch file is discarded and the open goes around again — up to `OPEN_BASE_ATTEMPTS` (3), after
which it fails with `EIO` rather than spinning a FUSE worker. Only a handle the open is about to
*create* is checked; if one already exists, that handle is authoritative and the scratch is
discarded regardless.

The re-read is deliberately **not** taken under the `State` lock: no site in the daemon holds
`pending` and `state` at once (the invariant is stated on `stamp_pending_sizes`) and this is not
the place to become the first. What that costs is a residual window of a few instructions instead
of a lock-tight comparison; what it removes is the multi-gigabyte window, which is the one that
actually loses data.

**To verify:** the acceptance suite's concurrency case, with a large file, closing and reopening
it for write from two processes while the drain is loaded.

---

## B33 — Lack of Partial Transfer Resumption & Head-of-Line Queue Blocking (MED-06)

**Status:** Fixed (verified 2026-07-22)  
**Found:** 2026-07-21, multi-agent drain engine audit (`audit_bugs.md` MED-06)  
**Where:** `crates/pdfs-fuse/src/drain.rs:L73`

**Cause:** The attempted remediation classified errors by substring and deleted
the pending row after five failures or strings containing 402/403/413. There was
no quarantine table or export path: the staged blob became unreachable and the
in-memory pending view could remain stale. Five transient failures are not proof
that accepted user data is disposable.

**Fix:** Never delete an accepted write on a retry path. Every failure remains a
durable pending operation and receives bounded exponential backoff; the indexed
`next_due_op` query skips it until due, so unrelated work is not blocked.

**Verified:** `cargo clippy -p pdfs-fuse --all-targets -- -D warnings` and the
`pdfs-fuse` unit suite pass. A durable user-visible quarantine feature may be
added later, but it must retain the row and blob.

---

## B34 — Writable POSIX Exposure of Read-Only Shared Folders (MED-07)

**Status:** Fixed in code by P2/P3 (2026-07-29); P3 now exposes the shared tree, while credentialed
second-account and live FUSE acceptance remain pending P7
**Found:** 2026-07-21, multi-agent permissions audit (`audit_bugs.md` MED-07)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/sharing.rs`

**Cause:** Shared "Viewer" (read-only) folders are exposed via POSIX with write permissions (`0755`). Local writes succeed initially on the mount but fail perpetually during background online drain with `403 Forbidden` errors.

**Fix:** P2 persists share authority in schema V17 `share_access`, inherits it through resident
entries, exposes read-only POSIX modes, and gates mutations at handler, queue, and drain boundaries.
Downgrades fail closed and are not acknowledged past failed durable state updates.

**Verification:** The local workspace passes formatting, locked checks, clippy with warnings denied,
and all 347 workspace tests, including P3 virtual-tree materialization and access inheritance plus
mutation gating, queued-operation authorization, and downgrade failure paths. Credentialed
second-account and live FUSE acceptance have not been run and remain required in P7.

---

## B35 — IPC Unix Socket Creation Permission Race (MED-08)

**Status:** Fixed (verified 2026-07-20 - see B6)  
**Found:** 2026-07-21, multi-agent security audit (`audit_bugs.md` MED-08)  
**Where:** `crates/pdfs-fuse/src/control.rs`, `crates/pdfs-core/src/config.rs`

**Cause:** Binding the domain socket before setting `chmod(0600)` created a race window where local users could connect before permissions were enforced.

**Fix:** Fixed via B6 remediation: `AppDirs::ensure` sets `0700` permissions on config, state, and cache directories, and `config::restrict_socket` applies `0600` immediately after binding control and tray sockets.

---

## B36 — Unicode Normalization Discrepancy (NFC vs NFD) (LOW-01)

**Status:** Open (verified 2026-08-16, deliberately not fixed)  
**Found:** 2026-07-21, multi-agent sync audit (`audit_bugs.md` LOW-01)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/sync.rs`

**Cause:** macOS/HFS+ NFD UTF-8 path inputs differ from Linux NFC UTF-8 paths, causing duplicate folder creation or lookup misses for accented filenames.

**Verified (2026-08-16), not fixed.** The claim is accurate: names are compared as bytes
throughout — the mirror engine's rel paths, `by_uid`/`children` interning, and the planner's
classification union — so `café` written by a Proton client on macOS or iOS (NFD) and `café`
written here (NFC) are two different paths, and a mirror folder holding both would sync both.

Not fixed here on purpose. A normalizing comparison changes what "the same file" *means* across
the whole daemon, and it changes it in the unsafe direction: two names that are genuinely distinct
on the remote would start colliding locally, and a collision in the sync engine is a deletion or
an overwrite, not a warning. Doing it properly means a normalization dependency, a decision about
which form is canonical on the wire, and a migration for baselines keyed on the other form — its
own change with its own acceptance case, not a line in a sweep. Left LOW because it needs
cross-platform authorship of the *same* accented name to bite at all.

---

## B37 — Failed Remote Trash Removed the Local Dentry (CRIT-04)

**Status:** Fixed in code (unverified on a live mount, 2026-07-22)
**Found:** 2026-07-22, deep FUSE/POSIX audit
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::trash_child`

**Cause:** `trash_child` called `forget_or_unlink` before the online
`trash_nodes` request. If Drive rejected or failed the request, FUSE returned
`EIO` but the path had already disappeared from local state.

**Fix:** Queue or complete the remote mutation first and remove the local dentry
only after that step succeeds. Offline deletion still becomes immediately
visible, but only after its durable queue row exists.

---

## B38 — Sync Task Panic Could Authorize Destructive Mode Switch (HIGH-08)

**Status:** Fixed in code (2026-07-22)
**Found:** 2026-07-22, sync concurrency audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, `Core::flush_batch`

**Cause:** A `JoinSet` task panic was logged but not added to `Outcome.errors`.
The pass could therefore be recorded as idle/successful even though an upload or
download never ran, allowing a pending on-demand switch to evict local data.

**Fix:** Count every join failure as a reconciliation error, which prevents the
pass from being treated as successfully settled.

---

## B39 — Sync Download Can Overwrite a Concurrent Local Edit (HIGH-09)

**Status:** Fixed and **live-verified 2026-08-16** on 1.8.1

**Live verification (2026-08-16).** Folder 6 was switched to on-demand (evicting its local
copies) and then back to mirror, which queues a download of the 128 MiB `b40.bin`. Three seconds
into that download, `RACING-LOCAL-EDIT-B39` was written to the destination path. The pass logged

```
WARN pdfs_fuse::sync: sync: local file changed while downloading; keeping it as a conflict copy
  path=/home/narl/pdfs-live-mirror/b40.bin expected=None
  actual=Some(LocalSig { mtime: 1786916720, mtime_ns: Some(1786916720414151922), size: 21 })
```

and finished with the remote copy at `b40.bin` and the racing 21 bytes intact at
`b40 (sync-conflict 1786916794).bin` — which the following pass then uploaded as a file of its
own. Before the fix the rename would have destroyed those bytes with no trace.

**Found:** 2026-07-22, sync concurrency audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, download classification and apply path

**Cause:** Reconciliation classifies a local file from an earlier scan, downloads
to a temporary file, then renames over the destination without revalidating that
the local inode/content stayed unchanged. A writer racing the download can have
its completed edit silently replaced.

**Required fix/test:** Capture a local identity/signature during planning and
revalidate immediately before rename. Preserve a conflict copy on mismatch. A
deterministic test should pause the download, edit the target, resume it, and
verify that neither version is lost.

**Fix (2026-08-16).** `Pending::Download` now carries `local: Option<LocalSig>` — what the
planning walk actually saw at that path, or `None` for nothing there — and `download_file` calls
`keep_racing_local_edit` in the last moment before the rename that publishes the download. A
destination that no longer matches the plan is moved aside by `preserve_conflict_copy` first, so
the racing bytes survive and re-upload as a new file on the next pass: the same resolution a
both-sides-changed conflict already gets. A path that is *empty* when something was expected is
not a conflict — the download is about to recreate it and there is nothing to preserve.

`Pending::Conflict` passes `None` deliberately: it has just moved the local copy aside itself, so
the state its download expects to find is "nothing", and anything that appears while it runs is a
new racing write that gets its own copy.

Covered by `sync::tests::a_download_keeps_a_local_edit_it_did_not_plan_for` (unchanged destination
overwritten silently; changed destination preserved). Unverified against a real timing race —
the deterministic pause-mid-download test the entry asks for is not in place; the unit test drives
the guard directly instead.

---

## B40 — Sync Upload Can Stream Torn Live Content (HIGH-10)

**Status:** Fixed and **live-verified 2026-08-16** on 1.8.1

**Live verification (2026-08-16).** A 128 MiB `b40.bin` was dropped into folder 6 and a pass
forced; six seconds into the upload, sixteen bytes were overwritten at offset 100,000,000. The
pass logged

```
WARN pdfs_fuse::sync: sync: file changed while uploading; the remote revision may be torn and
  will be replaced on the next pass rel="b40.bin"
  streamed=LocalSig { mtime: 1786916599, mtime_ns: Some(1786916599336211782), size: 134217728 }
  now=Some(LocalSig { mtime: 1786916605, mtime_ns: Some(1786916605686365985), size: 134217728 })
```

and — this is the part that matters — recorded the baseline against the *streamed* signature
(`…599`), not the file on disk. The very next pass therefore read the file as locally changed and
uploaded `b40.bin new version`. Round-tripping the folder through on-demand and back later
downloaded the remote copy at md5 `cfb45003003fb2ad50c2c365f8313849` — the post-mutation local
content, exactly. The torn revision healed itself without anyone asking.

**Found:** 2026-07-22, sync concurrency audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, upload apply and baseline update

**Cause:** Planning stats a path, but upload later opens and streams the live
file. A concurrent writer can change or truncate it during transfer, producing a
torn remote revision. Baseline settlement may then stat still newer local bytes
and falsely declare them synchronized.

**Required fix/test:** Upload an immutable staged snapshot, or validate an open
descriptor before and after streaming and refuse baseline settlement on change.

**Fix (2026-08-16).** The second option, and the correction is structural rather than a check.
`record_file_baseline` no longer stats the path when it is done; it is *given* the identity the
transferred content corresponds to, and both upload paths pass the `LocalSig` taken from the
descriptor they streamed. So a file that changed mid-stream records a baseline describing the
bytes that went up, not the bytes on disk now — which makes the very next pass classify the path
as locally changed and upload a whole, clean revision over the torn one. The old behaviour stated
the opposite: it declared the torn revision synchronized and left it as the file's content on
Drive.

`warn_if_torn` compares before and after purely so the journal explains the extra revision; the
correction does not depend on it. Downloads are unaffected — there the file on disk *is* the
transferred content, published by a rename and stamped, so its stat is the identity.

Note the pre-existing mitigation this backs up: the `/proc/*/fd` scan already defers any file held
open for writing (`FilePlan::Deferred`), so this covers the residual — a writer that opens, writes
and closes entirely inside one upload.

**To verify:** upload a large file from a mirror folder while appending to it, then check that the
following pass uploads a new revision and the final remote content matches the local file.

---

## B41 — Sync Folder Removal Races Active Reconciliation (HIGH-11)

**Status:** Fixed (verified by inspection) 2026-08-16 — already correct in the code by the time
this was revisited
**Found:** 2026-07-22, sync lifecycle audit
**Where:** `crates/pdfs-fuse/src/devices.rs`, `remove_sync_folder`

**Cause:** Folder removal does not acquire the per-folder `sync_lock` used by a
reconcile pass. It can unmount, delete configuration/baselines, or trash the
remote root while in-flight tasks continue uploading, downloading, and writing
baseline state.

**Required fix/test:** Serialize removal with reconciliation, cancel/disable the
folder, await active work, then unmount and remove durable state.

**Audit (2026-08-16).** `Core::remove_sync_folder` opens by taking `self.sync_lock(id)` and holds
it across the whole removal, and `reconcile_pass` takes the same lock for the whole pass — so a
removal requested mid-pass blocks until that pass finishes, which is the serialization this asked
for. The ordering inside the lock is also the one specified: unmount the on-demand session first
(before trashing the remote tree it serves, which would otherwise leave it answering for deleted
nodes), then drop the row.

The one step outside the lock is the remote trash, and that is safe by construction rather than by
accident: the row is already gone, and `reconcile_pass` re-reads `sync_folder_get` **under the
lock** and returns when it finds nothing, so no pass can start against a folder being removed. The
mode re-check on that same re-read is what makes this hold for a switch racing a removal too.

No code change. Recorded as verified-by-inspection: a live removal during an active pass has not
been driven.

---

## B42 — Local Delete Failure Is Recorded as Sync Success (HIGH-12)

**Status:** Fixed (unverified) 2026-08-16.
**Found:** 2026-07-22, sync error-path audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, local file/directory deletion branches

**Cause:** Some local removal errors are ignored while the baseline is removed
and success is logged. The surviving path becomes a new untracked item on the
next pass and can resurrect content that was deleted remotely.

**Required fix/test:** Treat `ENOENT` as success; for every other removal error,
retain the baseline and increment `Outcome.errors`.

**Fix (2026-08-16).** The error half was already in place by the time this was revisited: both
`FilePlan::DeleteLocal` and the deferred folder removals log the failure, increment
`outcome.errors` and `continue` without touching `sync_entry_remove`, so the baseline survives
and the survivor is not mistaken for a new local file next pass.

The `ENOENT` half was not, and it failed the opposite way: a path that was *already* gone was
counted as an error and kept its baseline, so the same removal was retried and re-reported on
every pass forever. Both call sites now go through `removed_locally`, which reads `NotFound` as
the state the engine was asking for. Covered by
`sync::tests::an_already_absent_path_counts_as_removed`. Unverified because no live sync pass has
been driven over an externally-deleted file yet.

**Live verification attempted 2026-08-16, blocked.** Driving this needs the *remote* side of a
mirror folder to lose a file, and a mirror folder's remote lives in the device share, which no
mountpoint exposes — `pdfs rm` only accepts paths under a mountpoint, and the obvious workaround
(switch the folder to on-demand, trash the file through its own mount, switch back) runs into
**B86**: the on-demand mount serves a listing that predates the mirror engine's uploads, so the
file to be trashed is not visible to `rm` at all. Verifying B42 live therefore needs either a
second client on the account or B86 fixed first. The ENOENT branch remains covered only by its
unit test.

---

## B43 — FUSE Directory Cookies Are Not Stable (MED-09)

**Status:** Fixed and **live-verified 2026-08-16** on 1.8.1

**Live verification (2026-08-16).** A probe opened
`ProtonDrive/pdfs-live-20260816` (12 files) and called `getdents64` with a 160-byte buffer, so the
kernel had to page through the listing; between every page it created a new file in the directory
and unlinked an older one. Three pages, 14 entries returned (12 + `.` + `..`), zero duplicates,
zero pre-existing entries missed — and none of the files created mid-enumeration appeared, which
is the frozen snapshot behaving as specified. The same probe against the pre-fix index-cookie
readdir is what this entry was written about.

**Found:** 2026-07-22, FUSE/POSIX audit
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::serve_readdir`

**Cause:** `readdir` rebuilds a live vector for every call and uses its array
index as the continuation cookie. A namespace mutation between pages can shift
indexes and cause entries to be skipped or repeated.

**Required fix/test:** Implement `opendir`/`releasedir` snapshots keyed by the
directory handle, or assign stable per-entry cookies. Test with deliberately
small reply pages and mutation between calls.

**Fix (2026-08-16).** The first option. `opendir` is now implemented: it enumerates the folder if
it is cold, freezes the listing, and publishes it under a fresh `fh` in `State::dir_snapshots`;
`readdir` serves from that snapshot; `releasedir` drops it. The kernel pairs each `opendir` with
exactly one `releasedir`, so the snapshot lives exactly as long as the enumeration it belongs to,
and a create or a trash landing between two pages can no longer shift the indexes the continuation
cookie is expressed in.

The `fh` comes from the same `State::next_fh` counter write handles use, so a directory handle can
never collide with a file handle. Serving from the snapshot is a pure in-memory walk with no
chance of a remote call, so `readdir` no longer needs a worker at all; the live path
(`serve_readdir`) stays as a fallback for a handle that has no snapshot, and both build their
listing through the same `build_listing` so they cannot disagree.

Enumeration cost is unchanged — it moved from the first `readdir` to `opendir`, one call earlier —
and a folder that cannot be enumerated now fails at `opendir`, where the caller can still act on
the errno.

**To verify:** the acceptance suite with deliberately small reply pages, creating and trashing
entries between `getdents` calls, asserting no entry is skipped or repeated.

---

## B44 — FUSE Background Workers Have No Coordinated Shutdown (MED-10)

**Status:** Fixed and **live-verified 2026-08-16** on 1.8.1

**Live verification (2026-08-16).** `systemctl --user restart proton-drive.service` on a daemon
with six live mounts, a busy drain and 18.4 GiB staged: stop to `Stopped` took 110 ms, the journal
shows the six unmounts followed by `sync engine stopping` and `daemon stopping`, and systemd never
reached its SIGKILL timeout. The old PID was gone immediately, and the fresh daemon settled at 42
threads — the same count the outgoing one had, so no loop was left behind or spawned twice. A
second restart mid-session behaved identically.

**Found:** 2026-07-22, daemon lifecycle audit
**Where:** `crates/pdfs-fuse/src/mount.rs`, mount lifecycle; drain/index/sync loops

**Cause:** Long-lived threads and tasks retain `Core` clones but share no
cancellation token or owned join set. Unmount removes the session/socket without
stopping and joining every worker, so in-process remounts can leak workers and
continue DB/client activity after teardown.

**Required fix/test:** Add shared cancellation, interruptible waits, owned join
handles, and ordered shutdown. Repeated mount/unmount tests should return thread
counts to baseline and observe no post-unmount DB work.

**Fix (2026-08-16).** All four, in `crates/pdfs-fuse/src/shutdown.rs` plus the loops:

1. **Shared cancellation.** `Shutdown` — a flag and a condvar, set once and never cleared — lives
   on `Core`, so every clone a worker was given carries it.
2. **Interruptible waits.** Every loop that called `thread::sleep` in a `loop {}` with no exit now
   calls `Shutdown::sleep`, which returns `false` the moment teardown starts: the conflict sweep
   (warmup *and* interval), the online probe, the local indexer, and both sync poll threads. The
   drain workers check between ops — never inside one, so an op is always either retired or
   released — and the sync engine gained a `SyncMsg::Stop` that ends its loop and abandons any
   burst it was settling.
3. **Owned join handles.** `run_mount` collects every long-lived thread it starts into one
   `workers` vec.
4. **Ordered shutdown.** `stop_workers` sets the flag, wakes the two waits that need waking — the
   drain's own condvar via `wake_drain`, and the control listener, which blocks inside `accept`
   and is woken by a connection from us before the socket is unlinked — then joins them all. It
   runs after the mounts are down, because nothing a worker can still do is harmful at that point,
   and it also runs on the startup-failure path, which abandons the mount just as completely.

A join that takes a while is expected and correct: a drain worker part-way through an upload
finishes it rather than abandoning the user's bytes mid-flight.

Covered by `shutdown::tests` (a stop cuts an hour-long wait short; an already-stopped signal does
not wait at all). Unverified for the property that motivated the entry — repeated in-process
mount/unmount returning thread counts to baseline — which wants the acceptance suite.

---

## B45 — Truncate After a Queued Rewrite Corrupts or Returns EIO

**Status:** Fixed and live-verified (2026-07-22)
**Found by:** `scripts/fuse-acceptance.sh`
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, write-open scratch setup;
`crates/pdfs-fuse/src/lib.rs`, `Core::queue_truncate`

**Repro:** Write a file, immediately replace its contents, then run
`truncate -s 4 file` before the queued replacement drains. Reading it returned
four NUL bytes instead of the first four replacement bytes.

The expanded managed suite found the path-based variant: create and close a
10-byte file, then immediately call `truncate(path, 4)`. The close's complete
revision was still inside the two-second drain debounce. `queue_truncate`
described the shrink as an incomplete edit over the remote base, noticed the
pending edit, preserved an orphan staging file, and returned `EIO`.

**Cause:** A new write handle always treated the server revision as its base.
When a newer complete revision was still queued locally, the handle's scratch
file began sparse and zero-filled. Shrinking it preserved those scratch zeros,
not the prefix of the locally authoritative queued revision.

**Fix:** A write opened over a complete queued revision seeds its scratch file
from that staged blob and marks the copied range authored. Stacking a write over
an incomplete queued revision is refused rather than guessed. Path-based
truncate now composes the same way: it copies a complete pending blob, applies
the new length, marks the result complete, and inherits the last real remote
baseline. Live acceptance verified shrink, zero-filled growth, and sparse I/O.

---

## B46 — Combined Cross-Directory Move and Rename Can Fail Out of Date

**Status:** Fixed and live-verified (2026-07-22)
**Found by:** `scripts/fuse-acceptance.sh`
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::rename`

**Repro:** `mv source victim`, replacing an existing file, followed immediately
by `mv victim dir/moved`. Proton accepted the move half of the second operation,
then rejected its rename half with `InvalidRequirements` (HTTP 422, "out of
date"), surfaced as `EIO`.

**Cause:** Moving changes the node's encrypted-name requirements. The following
link-details read can briefly observe the pre-move state, so the new name is
signed against stale requirements.

**Fix history:** A bounded retry of `InvalidRequirements` reduced the window but
did not close it; the managed matrix reproduced the same failure after all four
retries. Combined cross-directory move+rename now enters the durable rename
queue as a desired end state. Its drain re-fetches the node on every attempt,
skips either half that already landed, renames before moving, and resolves name
collisions without losing the source. Simple rename-only and move-only calls
remain synchronous. The managed live suite subsequently passed the replacement
and combined move/rename cases.

**Follow-up (2026-09-15, unverified):** With SDK 0.6.3 the drain sends the
move and the rename as one `move-multiple` request carrying the target name,
instead of renaming in the source folder and then moving. That removes the
second call the stale requirements could hit, and the SDK retries a remaining
`InvalidRequirements` once with the server's current name hash. A destination
name collision now moves under a conflict name in one step. Same-folder renames
are unchanged. The synchronous FUSE `rename` path (`serve_rename`) still renames
then moves with its bounded retry and has not been converted. Needs a managed
live run of the combined move/rename cases.

---

## B47 — FUSE Accepts Path Components Longer Than NAME_MAX

**Status:** Fixed and managed-live verified (2026-07-22)
**Found:** 2026-07-22, managed FUSE acceptance suite
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, name-taking callbacks

**Repro:** `open(<mount>/<256 ASCII bytes>, O_CREAT)` succeeded. A conventional
Linux filesystem must reject a pathname component longer than 255 bytes with
`ENAMETOOLONG`.

**Cause:** Every callback converted `OsStr` with `to_string_lossy` and passed it
straight to Drive. This imposed neither Linux's component limit nor Drive's
UTF-8 requirement, and could silently change non-UTF-8 names.

**Fix:** Route lookup, create, mkdir, unlink, rmdir, and both rename components
through one validator. It enforces the 255-byte limit, rejects reserved/empty
components, rejects invalid UTF-8 with `EILSEQ`, and preserves valid Unicode.
The managed live suite is the verification gate.

---

## T1 — Managed Acceptance Raced Daemon Startup

**Status:** Fixed (2026-07-22)
**Found:** repeated install/restart/acceptance loop
**Where:** `scripts/fuse-acceptance.py`, managed setup

**Repro:** Run `systemctl --user restart proton-drive.service` and immediately
start `--managed-live`. The offline reference completed before the daemon had
recreated its control socket, so the first `pdfs --json sync list` failed with
`ENOENT` and setup aborted.

**Fix:** Managed validation polls the supported `sync list` API until the daemon
is ready or `PDFS_ACCEPTANCE_SYNC_TIMEOUT` expires. This is tracked as a test
harness defect rather than an application bug.

---

## T2 — Acceptance Cleanup Raced Queued Namespace Replay

**Status:** Fixed (2026-07-22)
**Found:** managed on-demand/on-demand matrix after every functional test passed
**Where:** `scripts/fuse-acceptance.py`, per-contract cleanup

**Repro:** A combined move+rename was correctly accepted into the durable queue.
Cleanup started recursively deleting its test tree while the rename drain was
still landing. The moved entry appeared in `dir-b` after `rmtree` enumerated it
but before `rmdir(dir-b)`, producing `ENOTEMPTY`.

**Fix:** Before cleanup, poll `pdfs --json status` until both pending counters
are zero. Retry `rmtree` only for `ENOTEMPTY` inside the uniquely owned test
root, then wait for the queued deletions before changing modes. Other errors are
still immediate failures.

---

## T3 — Mode-Matrix Preservation Check Raced Mirror Restoration

**Status:** Fixed (2026-07-22)
**Found:** managed on-demand/on-demand → on-demand/mirror transition
**Where:** `scripts/fuse-acceptance.py`, managed mode switching

**Repro:** Switch a populated managed folder from `ondemand` to `mirror`, wait
until `sync list` reports mirror/idle, and immediately read a preservation file.
The file can still be absent even though the subsequent restore pass downloads
it correctly.

**Cause:** The application commits the new mode before scheduling reconciliation
and leaves the previous idle state and sync timestamp in the row. Those values
describe the completed on-demand state, not a completed mirror restoration.

**Fix:** The harness records every transition to mirror and explicitly requests
and waits for a newer completed sync pass before validating local bytes. This is
tracked as a harness synchronization defect; transient absence before that pass
is expected asynchronous behavior, not data loss.

---

## T4 — Successful Managed Cleanup Left Its Own Sentinel Behind

**Status:** Fixed (2026-07-22)
**Found:** second consecutive managed acceptance run
**Where:** `scripts/fuse-acceptance.py`, managed cleanup

**Repro:** Complete the mode matrix in mirror/mirror, then immediately start a
new managed run. Registration cleanup removed the sync-folder records and remote
test folders, but the next run's empty-directory precheck found the harness's
128 KiB preservation sentinel in each local directory.

**Cause:** Removing a mirror registration intentionally preserves the local
copy. The harness treated unregistering as if it also emptied local storage.

**Fix:** After successful unregister, cleanup deletes only the sentinel whose
contents match the per-run bytes generated by this harness. Unknown files remain
untouched and continue to fail the strict precheck.

---

## B48 — Successful Unlink Can Reappear From a Stale Remote Listing

**Status:** Fixed (unverified)
**Found:** 2026-07-22, managed acceptance cleanup after all on-demand I/O passed
**Where:** `crates/pdfs-fuse/src/lib.rs`, child enumeration and trash paths

**Repro:** Unlink every child of a directory and immediately remove the
directory. `unlink` returned success, but a parent invalidation followed by an
eventually consistent remote enumeration returned the just-trashed child again.
`rmdir` then returned `ENOTEMPTY`; repeated recursive removal could reproduce it
for the entire acceptance timeout.

**Cause:** Three stale-data paths combined. A queued cross-directory rename
updated `State` but did not invalidate the kernel's cached empty destination
listing, so recursive walkers never saw or unlinked the moved child. Removing a node recorded no
authoritative local tombstone, so an eventually consistent Drive listing could
intern it again. Separately, `State::has_children` treated a resident empty
listing as inconclusive and fell back to obsolete SQLite child rows. Thus
`readdir` could report empty while `rmdir` returned `ENOTEMPTY` forever.

**Fix:** Every successful online or queued trash records its uid in a
session-shared hidden set. Both persisted and remote child enumeration filter
that set. Secondary on-demand mounts share it, and an explicit restore removes
the uid so restored content can appear normally. `has_children` now treats any
resident listing—including an empty one—as authoritative and consults SQLite
only when the listing is absent. After replying successfully to a queued rename,
the FUSE handler invalidates both exact dentries and both directory inodes; the
ordering matters because an invalidation sent before the rename reply is
overwritten by the kernel's response processing. Uids are immutable and never
reused, making a session-long tombstone safe.

---

## B49 — Failed Conflict Preservation Still Overwrites the Local File (CRIT-05)

**Status:** Fixed, with the injected-failure suite the entry asked for (2026-08-17). Only the
end-to-end live conflict remains unexercised.
**Found:** 2026-07-22, 1.0 data-safety audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, download/conflict handling

**Cause:** If renaming a concurrently changed local file to a conflict copy
fails, sync only logs the error and continues publishing the remote download
over the local version. Second-resolution conflict names can also collide.

**Required fix/test:** Abort download publication unless preservation succeeds;
use collision-resistant names. Test existing names, permissions, `ENOSPC`,
`EIO`, and cross-filesystem behavior. This is B39's failure path; B25 remains
the separate weak-baseline problem.

**Verification (2026-08-17).** `preserve_conflict_copy` was already correct — it copies to a
`create_new` destination, syncs data and directory, and removes the source last, so every failure
returns `Err` and `apply_one` never reaches the download. What was missing was proof, so:
`conflict_preservation_failures_all_keep_the_local_file` drives an unwritable directory
(`EACCES`), a source that vanished between planning and preservation (`ENOENT`), and a destination
name already taken by a directory — and asserts the local file is still there, byte-identical,
after each. `a_conflict_copy_is_byte_and_mtime_identical` covers the other half: the copy carries
the bytes, mtime and mode, so the next pass uploads the user's version rather than something that
looks freshly written. The existing collision test covers the suffix search.

Not covered: a genuine `ENOSPC` or `EIO` from the storage layer, and the end-to-end live conflict.

---

## B50 — Failed Orphan Staging Deletes the Sole Scratch Copy (CRIT-06)

**Status:** Fixed, with injected filesystem-failure verification (2026-08-17).
**Found:** 2026-07-22, 1.0 data-safety audit
**Where:** `crates/pdfs-fuse/src/lib.rs`, `Core::stage_orphaned_write`

**Cause:** If preserving an orphaned scratch write into staging fails, the
failure path removes the original even though no other durable copy exists.

**Required fix/test:** Never unlink the source until a durable replacement and
metadata are committed. Inject `ENOSPC`, `EIO`, `EROFS`, permission, and
cross-device failures and assert a byte-identical copy remains discoverable.
Related: B23, B27, and B28.

**Verification (2026-08-17).** Four new tests in `pdfs-core/src/cache.rs`.
`orphan_preservation_keeps_the_scratch_copy_when_staging_is_unwritable` makes staging mode `0500`
so both the rename and the copy fallback fail, and asserts the scratch file survives
byte-identical, is the path the error names, and carries its recovery marker.
`a_retained_orphan_write_is_discoverable_after_a_restart` reopens the cache after that failure and
asserts `recovered_writes` finds the bytes *with their metadata* — present is not the same as
discoverable, and discoverable is what the entry asked for.
`orphan_preservation_across_a_device_boundary_publishes_before_removing` and
`a_failed_cross_device_preservation_keeps_the_marked_source` drive the copy fallback across a real
device boundary (`/dev/shm` vs `/tmp`), asserting the ordering — destination and sidecar published
and synced first, source removed last — and that a publication which does not finish leaves the
marked source in place. The cross-device pair skips with a printed note on a machine that has only
one filesystem, rather than passing silently.

Not covered: a genuine `ENOSPC`/`EIO` from the block layer; `EROFS` is approximated by the
unwritable-directory case rather than a read-only mount.

---

## B51 — Scratch Sidecar Does Not Establish a Durable Recovery Record (CRIT-07)

**Status:** Fixed, with crash-boundary and corrupt-sidecar verification (2026-08-17). True
power-loss testing (a machine actually losing power mid-write) is still not covered.
**Found:** 2026-07-22, 1.0 crash-consistency audit
**Where:** `crates/pdfs-core/src/cache.rs`, scratch durability markers

**Cause:** A temporary sidecar is renamed without syncing the sidecar first or
the directory afterward. Power loss after successful user `fsync` can preserve
bytes but lose authored-range metadata; recovery may discard the write or treat
a sparse partial write as complete and replace untouched ranges with zeros.

**Required fix/test:** Sync data, write and sync temporary metadata, rename it,
then sync the directory before acknowledging durability. Crash-test every
boundary and corrupt/partial sidecars. B20 and B23 do not cover this invariant.

**Verification (2026-08-17).** `a_half_written_scratch_sidecar_is_never_read_as_a_record` puts on
disk exactly what a crash inside `mark_scratch_durable` leaves — a `.json.tmp` and no published
sidecar — and asserts the next open recovers nothing from it, so an unpublished temporary can
never be replayed as a write. `a_corrupt_scratch_sidecar_still_rescues_its_bytes` truncates a
published sidecar to a JSON prefix and asserts the blob is still moved to `recovery/`
unaccompanied: metadata loss must not take the user's bytes with it.
`remarking_a_scratch_file_replaces_its_record_atomically` covers the repeated-`fsync` case — each
call leaves one complete parseable record and no debris.

Not covered: real power loss. Nothing short of a crashing VM tests that, and the entry should not
be closed as if it did.

---

## B52 — Staged-Write Publication Is Not Crash-Safe (CRIT-08)

**Status:** Fixed, with cross-filesystem verification against a real device boundary
(2026-08-17). True power-loss testing is still not covered.
**Found:** 2026-07-22, 1.0 crash-consistency audit
**Where:** `crates/pdfs-core/src/cache.rs`, staged-write publication

**Cause:** Rename/copy and direct sidecar writes lack a complete sync protocol.
The cross-filesystem fallback can remove its source after an unsynced copy,
leaving no complete data/metadata pair after a crash.

**Required fix/test:** Publish synced temporary data and metadata via atomic
renames and directory syncs; delete the source only after verification. Inject
failure after every write, sync, rename, DB enqueue, and deletion. Related: B50
and B51.

**Verification (2026-08-17).** `stage_write` gained a `stage_write_at` seam — the public entry
point picks a timestamped destination, which is right for production and impossible to arrange a
failure at, the same reason `preserve_write_at` exists.
`staging_across_a_device_boundary_removes_the_source_last` drives the copy path across a real
device boundary and asserts the destination and sidecar are both on disk before the source goes.
`a_failed_staging_sidecar_keeps_the_cross_device_source` blocks the sidecar's temporary name and
asserts the source survives — a staged blob nothing describes is not a substitute for the bytes.
`an_unaccompanied_staged_blob_survives_a_restart` asserts startup leaves an undescribed staging
blob (and a leftover `.json.tmp`) alone rather than tidying user data away.

Not covered: real power loss, and injected failure at *every* one of the listed points — the
sidecar and source-removal boundaries are covered, the intermediate `fsync` calls are not.

---

## B53 — Superseding a Pending Operation Is Not Transactional (CRIT-09)

**Status:** Fixed, with rollback, restart and full-database verification (2026-08-17).
**Found:** 2026-07-22, 1.0 SQLite durability audit
**Where:** `crates/pdfs-core/src/db/ops.rs`, `Db::enqueue_op`

**Cause:** The old durable row is deleted before its replacement is inserted,
outside a transaction. A crash, constraint error, full disk, or I/O failure can
erase acknowledged upload work and orphan its blob.

**Required fix/test:** Select and replace in one transaction; retain the old
blob until commit and make cleanup recoverable. Inject insert/commit failures
and prove that old or new operation—never neither—remains queued. B27 concerns
a retained failing row; this issue erases the row.

**Verification (2026-08-17).** The existing `failed_superseding_insert_keeps_the_old_pending_op`
proves the row rolls back. Two tests were added for the parts that make the rollback usable.
`a_rolled_back_supersede_keeps_its_blob_and_survives_reopen` asserts the failed call does *not*
report the old blob as superseded — that value is the caller's instruction to delete those bytes,
and the row that owns them is still queued — and that the rollback is still there after closing
and reopening a file-backed database, so it is not an artefact of the connection that failed.
`a_full_database_rolls_the_supersede_back_rather_than_erasing_it` caps `max_page_count` at the
current size, which is the same `SQLITE_FULL` an out-of-space filesystem produces, and asserts the
acknowledged upload is still queued and still owns its bytes.

---

## B54 — Total-Wipe Guard Does Not Protect a One-Entry Baseline (CRIT-10)

**Status:** Fixed, with one-entry, absent-root, unreadable-root and replaced-mountpoint
regressions (2026-08-17). The live mode matrix is still the only thing unexercised.
**Found:** 2026-07-22, 1.0 mirror-sync audit
**Where:** `crates/pdfs-fuse/src/sync/planner.rs`

**Cause:** The safeguard activates only when the baseline has at least two
paths. A one-file local folder that is missing, unreadable, or wrongly mounted
can therefore trash its sole remote file.

**Required fix/test:** Protect every non-empty baseline and require an
independently complete scan plus an explicit deliberate-delete signal for a
total wipe. Test absent roots, replaced mountpoints, permissions, and one-file
folders in every mode combination.

**Verification (2026-08-17).** The guard reads `!baseline.is_empty()`, so every non-empty baseline
is protected; `guard_local_wipe_blocks_only_a_total_disappearance` already carried the one-entry
case. The "independently complete scan" half is B55's fix — an absent, unreadable or partially
readable root now fails the pass before the guard is reached, which those tests cover.

What was missing is the case where nothing fails: a root that is readable but is no longer the
tree it was, an empty directory left where a mount used to be.
`a_replaced_mountpoint_trips_the_guard_at_every_baseline_size` walks a real empty root, confirms
the scan honestly reports nothing, and asserts the guard fires for baselines of one, two and three
paths — then that it steps aside the moment the tree is back, so a remounted folder resumes
instead of staying wedged.

**Deliberate divergence from the required fix:** there is no explicit deliberate-delete signal.
The guard refuses a total wipe outright rather than offering a way to confirm one. That is
stricter than asked for, and it means a user who really did delete everything must remove and
re-add the sync folder. Worth revisiting if anyone hits it; not worth building a confirmation path
nobody has asked for yet.

Not covered: the live mode matrix (`scripts/fuse-acceptance.sh --managed-live`).

---

## B55 — Incomplete Local Scans Are Interpreted as Deletions (CRIT-11)

**Status:** Fixed, with injected iterator/stat failure tests (2026-08-17).
**Found:** 2026-07-22, 1.0 mirror-sync audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, local scan/reconciliation

**Cause:** Directory-entry and metadata errors omit paths instead of marking the
snapshot incomplete. Reconciliation can treat omissions as intentional local
deletions and propagate them remotely.

**Required fix/test:** Carry completeness/errors into planning; an incomplete
subtree must make the pass non-destructive and retain its prior baseline. Test
`readdir`, `stat`, permission, transient-I/O, and disappearing-entry failures.
B42 covers the inverse local-delete failure.

**Verification (2026-08-17).** The scan body was lifted out of `Core::walk_local` into a free
`walk_local_tree`, so it can be driven against a real tree with a real unreadable directory in it
and no `Core` at all. Every failure in it is fatal to the pass by construction — a `read_dir` that
fails, an entry that cannot be stat-ed, a name that is not UTF-8 — because a skipped path is a
path reconciliation reads as a deletion and propagates to Drive.

Tests: `an_unreadable_subdirectory_fails_the_scan_instead_of_shrinking_it` (chmod `000` on a
subdirectory holding a file, asserting the error names the subtree),
`an_unreadable_root_fails_the_scan`, `an_absent_root_fails_the_scan` (the unmounted-device case),
`a_non_utf8_name_fails_the_scan`, and `a_readable_tree_scans_completely` as the complement — the
strictness must not cost a working pass, so a healthy tree still reports every entry and still
skips symlinks by design.

Not covered: transient I/O errors, and an entry that disappears between `read_dir` and `stat`.
Both are races that would need a filesystem shim to force; the code path they take is the same
`symlink_metadata` failure the permission test drives.

---

## B56 — Database Schema Version Parsing Fails Open (HIGH-13)

**Status:** Partly fixed — future schemas are refused; malformed-version handling remains open
**Found:** 2026-07-22, 1.0 database audit
**Where:** `crates/pdfs-core/src/db/migrations.rs`, `Db::migrate`

**Cause:** Missing, malformed, or non-numeric version values become zero, while
all future versions are accepted. Corruption can replay migrations, and an
older binary can open a newer incompatible database.

**Required fix/test:** Distinguish a new DB from corrupt metadata; reject
malformed/future versions without mutation and validate required schema. Test
empty, malformed, negative, future, partial/interrupted migration, and upgrade
fixtures from every released schema.

---

## B57 — Event Cursor Can Advance Past Unapplied State (HIGH-14)

**Status:** Partly fixed — cursor now advances per successfully invalidated event; DB/apply fault tests remain
**Found:** 2026-07-22, 1.0 event-recovery audit
**Where:** `crates/pdfs-fuse/src/background.rs`, `run_event_sync`

**Cause:** Cache invalidation failures are ignored while the batch cursor still
advances. Cursor-persistence failure also leaves the in-memory cursor moving;
first-seed persistence failure can later reseed at a newer head. Applying state
and committing its cursor have no atomic relationship.

**Required fix/test:** Apply idempotently and persist derived DB state plus the
cursor atomically, or advance only through the last durable event. Retry failed
prerequisites. Crash/fault-test every event and cursor boundary, first seed,
duplicate replay, malformed events, and database-full conditions.

---

## B58 — Control Socket Has Unbounded Frames and Connections (HIGH-15)

**Status:** Fixed in code with frame-limit regressions; connection-flood verification pending
**Found:** 2026-07-22, 1.0 IPC audit
**Where:** `crates/pdfs-fuse/src/control.rs`

**Cause:** The server uses unbounded `read_line`, a new thread per connection,
and no concurrency or idle limit. A local process can exhaust memory, file
descriptors, and threads, blocking filesystem control and safe shutdown.

**Required fix/test:** Bound frames, clients, and idle lifetime. Test slow/no-
newline clients, oversized/truncated JSON, floods, and recovery. B6/B35 cover
socket authority and permissions, not resource bounds.

---

## B59 — Private State Permission Enforcement Fails Open (HIGH-16)

**Status:** Fixed in code with symlink regression; wrong-owner integration verification pending
**Found:** 2026-07-22, 1.0 local-privacy audit
**Where:** `crates/pdfs-core/src/config.rs`, `AppDirs::ensure`

**Cause:** chmod/creation failures are warned about or ignored while decrypted
content, plaintext metadata, and the authenticated socket may remain accessible
to other local users.

**Required fix/test:** Before loading credentials or serving data, verify every
sensitive directory is owned by the effective user, is not a symlink, and is
owner-only. Test wrong owner, symlink substitution, read-only filesystems,
permissive restored directories, and chmod failure. This tightens B6/B35.

---

## B60 — Config Writes Are Non-Atomic and Parse Errors Are Overwritten (HIGH-17)

**Status:** Fixed in code; fault/concurrent-save verification pending
**Found:** 2026-07-22, 1.0 configuration audit
**Where:** `crates/pdfs-core/src/config.rs`, `load_config` / `save_config`

**Cause:** Save truncates in place without sync. Load treats read/parse failure
as absence and best-effort overwrites with defaults, silently losing mountpoint,
ignore policy, budget, and explicit device adoption.

**Required fix/test:** Atomically publish a restricted, synced temporary file
and sync its directory. Preserve malformed input and report an actionable error.
Test invalid/truncated JSON, permissions, `ENOSPC`, crash points, and concurrent
saves.

---

## B61 — Rotated Single-Use Credentials Can Be Lost (HIGH-18)

**Status:** Open — 1.0 authentication/recovery blocker
**Found:** 2026-07-22, 1.0 authentication audit
**Where:** `crates/pdfs-core/src/auth.rs`, refresh callback

**Cause:** Failure to serialize or persist rotated credentials is only logged
(and some setup failures are silent). The daemon continues in memory while the
keyring retains an invalid single-use refresh token, so restart loses login.

**Required fix/test:** Expose unhealthy persistence, retry with bounded backoff,
notify the user, and define a safe re-login/shutdown path. Test locked/full/
unavailable keyrings, callback races, repeated rotations, death, and restart.

---

## B62 — Debian Artifact Omits Required Service and Autostart Units (HIGH-19)

**Status:** Fixed in workflow; clean-install VM verification pending
**Found:** 2026-07-22, 1.0 packaging audit
**Where:** `.github/workflows/release.yml`; `packaging/`

**Cause:** The generated package omits `proton-drive.service` and the tray
autostart entry even though normal login/GUI flows assume that lifecycle.

**Required fix/test:** Package all units/resources with correct paths, modes,
and hooks. In clean supported-distribution VMs test install, login, reboot,
daemon/tray/FUSE, upgrade, rollback, and uninstall without deleting user state.

---

## B63 — Release Version Identity Is Inconsistent and Unenforced (HIGH-20)

**Status:** Partly fixed — tag/workspace/PKGBUILD equality is enforced; protocol identity remains
**Found:** 2026-07-22, 1.0 release audit
**Where:** manifests, client identity constants, tags, release workflow

**Cause:** Workspace/binary versions, protocol identity strings, and tags can
disagree; the workflow trusts any `v*` tag without checking package metadata or
installed `--version` output.

**Required fix/test:** Establish one version source and fail CI on every tag,
manifest, package, and binary mismatch. Document any intentionally independent
protocol identity.

---

## B64 — Stable Publishing Lacks Data-Safety and Recovery Gates (CRIT-12)

**Status:** Partly fixed — automated gates and managed-live matrix pass; recovery approval remains open
**Found:** 2026-07-22, 1.0 release audit
**Where:** CI and `.github/workflows/release.yml`

**Cause:** A tag can publish without required format/lint/tests, offline and
dedicated-account live acceptance, migrations, or crash/recovery drills. Fixes
still marked unverified can ship as stable.

**Required fix/test:** Publish immutable RC artifacts only after all workspace
checks, full managed mode matrix, migration/power-loss recovery, clean install/
upgrade tests, and explicit sign-off for every critical/high integrity issue.

---

## B65 — Release Artifacts Lack Supply-Chain Verification (MED-11)

**Status:** Open — 1.0 release/security blocker
**Found:** 2026-07-22, 1.0 supply-chain audit
**Where:** CI and release workflows

**Cause:** Releases do not enforce advisory/license policy or publish signed
provenance, SBOMs, and verifiable checksums for exact distributed artifacts.

**Required fix/test:** Add locked audit/license gates, SBOM and provenance,
checksums and signatures, documented verification, and an independent clean-job
verification/install test.

---

## B66 — Local-Only Pending Data Is Not Protected at Shutdown (HIGH-21)

**Status:** Open — 1.0 data-safety/UX blocker
**Found:** 2026-07-22, 1.0 recovery audit
**Where:** shutdown, CLI/GUI status, logout/unregister workflows

**Cause:** Undrained writes may exist only on one machine, but there is no flush
command with a completion contract, prominent pending byte/count health, or
shutdown/logout warning/inhibition.

**Required fix/test:** Add `pdfs sync flush`, expose pending operations/bytes
and last errors, and protect destructive lifecycle actions. Test offline stop,
logout, unregister, reboot, retry exhaustion, and drain. Credential removal must
never delete staged bytes.

---

## B67 — Fresh-State Restore Can Silently Select a New Device (HIGH-22)

**Status:** Open — 1.0 recovery blocker
**Found:** 2026-07-22, 1.0 disaster-recovery audit
**Where:** device discovery/adoption; `docs/RECOVERY.md`

**Cause:** Restore depends on hostname and folder basename. After local-state
loss, a mismatch can silently create/select another device rather than attach
the intended remote roots, making recovery appear complete while data is absent.

**Required fix/test:** Require explicit adoption/confirmation on ambiguity and
show old/proposed device and root IDs. Test state loss, hostname change,
duplicates, renamed folders, multiple devices, and byte-identical restoration.

---

## B68 — Profile Backup Cannot Be Written to the Device Root

**Status:** Partly fixed — the destination is fixed and verified live
(2026-07-31, see **B78**); backup health is still invisible in the UI and the
replacement-machine restore has not been re-run
**Found:** complete managed-live mode matrix against the 1.0.0 release binary
**Where:** `crates/pdfs-fuse/src/profile.rs`, profile backup destination

**Repro:** Every sync-folder registration or mode transition attempted the
profile backup and received `NotEnoughPermissions` (HTTP 422): `Cannot create
file at the root of a device`. The filesystem matrix itself passed, but profile
backup never became durable remotely.

**Impact:** The recovery feature appears active but does not preserve the device
profile, so a replacement machine still lacks modes, pins, mount settings, and
the explicit remote-folder mapping. Repeated retries also add noisy warnings.

**Required fix/test:** Store the profile below a writable application folder
rather than directly at the device root, reuse that folder by stable identity,
and make backup health visible. Verify upload, replacement, restart restore, and
byte-identical fresh-state recovery on a dedicated account.

## B69 — Spurious `(sync-conflict)` copies from mtime-keyed revision identity (B16/B25 root fix)

**Status:** Fixed — offline-tested 2026-07-25, **live-validated 2026-07-26** (see below)
**Found:** 2026-07-24, user reported `test_export (sync-conflict 1784898786).xml` created after a normal save from an application into the on-demand mount. A full-mount scan turned up ~156 conflict copies; 151 were byte-identical to their live sibling.
**Where:** `crates/pdfs-fuse/src/{drain,filesystem,lib,state,sweep,sync,sync/planner}.rs`, `crates/pdfs-core/src/cache.rs`, `crates/pdfs-core/src/control.rs`; SDK `proton-drive-rs` `node.rs`/`client.rs`/`public_link.rs`.

**Cause:** Two independent false-positive sources, both from `(mtime, size)` being used as a revision identity (the weakness B25 flagged; the recurrence B16 could not fully close):
1. **Re-stamped mtime.** `revision_conflict` compared the queued write's baseline mtime against the remote's. The server stamps a sealed revision with its own commit time (`…780`), a few seconds off the optimistic time the client baselined at (`…777`). Same bytes, same size, drifted mtime → the drain diverted the write into a conflict copy of its own base.
2. **First-sync of pre-existing files.** `plan_file` classified `(local, remote, no-baseline)` as `Conflict`, so the *first* reconcile of a folder cut a conflict copy of every file that already existed identically on both sides — the ~151-file storm, all in one ~40s window.

**Fix:** Make conflict detection key on the server **revision id**, the true identity (it advances iff a new revision was sealed):
1. **SDK** exposes `active_revision_id` and `content_sha1` (plaintext SHA-1 from the revision's decrypted `XAttr`) on `NodeKind::File` — both already on the wire / already decrypted, no extra round trips.
2. **`Baseline`** gains `revision_id`, captured at write-open (`WriteHandle.base_revision_id`) and carried across supersede/rebaseline like the mtime. `revision_changed` (extracted, pure, unit-tested) trusts the id when both sides have it and only falls back to `(mtime, size)` for an old sidecar. A re-stamped mtime on the same id is no longer a conflict; a same-size edit by another device (new id) still is.
3. **Planner** adds `FilePlan::AdoptBaseline`: a no-baseline pair of equal size is recorded into the baseline instead of conflicted. Divergent sizes stay a real conflict.
4. **Auto-sweep** (`sweep.rs`, `pdfs-conflict-sweep` thread, 5-min cadence): removes conflict copies proven identical to their live sibling (equal size **and** equal `content_sha1`, trashing the copy remotely) and surfaces divergent/orphaned ones once as a new `ActivityKind::Conflict` entry. Never removes a copy it cannot prove is a duplicate.

**Verified:** `proton-sdk-rs` 50/61 lib tests + clippy clean; client `cargo fmt`/`clippy -D warnings`/`cargo test --workspace --locked` all green (258 tests, incl. new `revision_changed`, planner `AdoptBaseline`, and `conflict_base_name` cases). The 151 identical copies were removed manually during triage; the 5 divergent ones are left for the user.

**Live validation (2026-07-26):** 1.1.0 installed over the production account (6
FUSE mounts of real data), daemon restarted, sweep observed across its warmup pass
and a full 5-minute interval. Pre-run snapshot: exactly 2 `(sync-conflict)` copies
left on the account, both divergent in size *and* `content_sha1`, so the correct
behaviour was to remove nothing. Result: **nothing removed** — the conflict
manifest was byte-identical before and after, the activity feed gained zero
`auto-removed duplicate` entries, and the copies were flagged `differs from …` as
intended. (The 40 `trash` entries in the feed for that window are
`trashed from the mount` — a previous FUSE-acceptance run's queued cleanup ops
replaying through the drain, unrelated to the sweep.) The sweep's *removal* path
therefore remains unexercised against a real account; it is now report-only by
default (B71) precisely so that stays true until there is field evidence.

**Shipping note (resolved 2026-07-25):** the client's revision-id/sha1 use depends on new SDK surface (`proton-sdk`/`proton-drive-rs` **0.2.2**). That version is now **published to crates.io**; the client's workspace deps were bumped `"0.2"` → `"0.2.2"`, the `Cargo.lock` re-resolved to the registry crates, and the local `[patch.crates-io]` shim removed. No local patch remains. (Live validation of the fix against a real account is still pending.)

## B70 — Browser in-flight temp files uploaded and conflict-forked on the on-demand mount

**Status:** Layers A + B fixed in-tree 2026-07-25 — **live-validated 2026-07-26**.
Validation was blocked until B74 was fixed (its scenario ends in a rename, which
B74 made read back as empty); with that out of the way the acceptance case
`regression B70: transient download name is not sealed` passes live.
**Found:** triaging the divergent conflict copies B69 left behind. In `~/Downloads`
(an on-demand mount) the complete files had been forked into `(sync-conflict)`
copies while their truncated partials kept the canonical name — e.g.
`teamspeak.tar.gz` (30 MB partial live vs a 3.77 GB conflict copy) and `nils.zip`
(17 MB vs 3.18 GB). The conflict copies were byte-exact to the user's independent
gdrive backups, proving the *conflict copy* was the finished download and the
*live* file the stub. The smoking gun: four `Unconfirmed NNNNN (sync-conflict …).crdownload`
copies — Brave's in-flight temp files, forked *mid-download*. User confirmed the
trigger: downloading with Brave directly into the on-demand folder, where a stall
+ "resume" is routine.
**Where:** on-demand write path — `crates/pdfs-fuse/src/filesystem.rs` `release`
(`queue_revision`, ~line 780) and `rename` (~line 933); drain seal in
`crates/pdfs-fuse/src/drain.rs`. The existing ignore system
(`crates/pdfs-core/src/syncignore.rs`, `DEFAULT_IGNORE_PATTERNS`) is wired **only**
into mirror reconcile (`sync.rs`), so it never sees on-demand writes.

**Cause:** A browser downloading into the mount writes a growing temp file
(`*.crdownload`, or `*.part` for Firefox) and, on a stall, closes the fd. `release`
hands every closed write to `queue_revision`, so a *partial* revision gets sealed
as the canonical file. On resume the browser reopens and rewrites the full file;
its baseline revision has moved (the client sealed the partial itself), so the
drain diverts the finished bytes into a `(sync-conflict)` copy. Net: the stub wins
the name, the real file is exiled to a conflict copy, and every abandoned
`.crdownload` is uploaded as its own multi-hundred-MB node. Unlike B69 this is a
*genuine* two-revision divergence, so B69's revision-id detection correctly refuses
to auto-sweep it — the fix has to stop the fork from happening, not reconcile it
after.

**Fix — two layers, both without an SDK release (A stops transient names ever
sealing; B stops a single-writer resume forking against its own seal):**

- **A — transient names never seal a revision until finalized (done 2026-07-25).**
  A new `syncignore::is_transient_name` recognises browser download temps
  (`*.crdownload`/`*.part`/`*.partial`/`*.download`), generic scratch
  (`*.tmp`/`*.temp`), editor swap/backup (`*.swp`/`*.swx`/`*~`), and office/lock
  files (`.~lock.*`, `~$*`). On `create` (filesystem.rs) a transient name is kept
  **purely local** — no empty remote node is minted — and its queued create is
  **parked**: `next_attempt_at` is set to the new `PARK_UNTIL` sentinel so
  `Db::next_due_op` never selects it. Written bytes still ride on that create via
  the existing `attach_blob_to_create`, whose `next_attempt_at` reset now
  *preserves* a park (SQL `CASE … >= PARK_UNTIL`), so a growing download attaches
  revision after revision without ever waking the drain. The finalize `rename`
  (`foo.crdownload → foo`) goes through the existing `is_local_uid` branch
  (`rewrite_op_target` renames the queued create); when the old name was transient
  and the new one is not, it calls `Db::set_create_hold(uid, false)` and wakes the
  drain, so the *completed* file uploads exactly once. Net: no partial and no
  abandoned temp ever reaches Drive, and nothing forks. Reuses the whole
  create/attach/rename/drain pipeline — the only new surface is the predicate, the
  `PARK_UNTIL` sentinel, `set_create_hold`, a `hold` arg on `queue_local_node`,
  and the attach-preserves-park `CASE`. Tests: `syncignore` predicate cases +
  `db::a_parked_transient_create_stays_off_the_drain_until_finalized`. No SDK
  dependency.
  - *Not yet covered by A:* apps that write the final name in place with **no**
    temp suffix (rare), and Firefox multi-connection `.part` files that it may
    rename per-segment — those still rely on B. An abandoned transient file (a
    cancelled download left on disk) now stays local-only forever rather than
    polluting Drive; a later janitor could reclaim its staged blob.
- **B — don't self-conflict a single-writer sequential rewrite (done 2026-07-25,
  no SDK dep).** The safety net for names A does not recognise (an app writing the
  final name in place, Firefox per-segment `.part` renames, or any resume the
  in-memory rebase machinery missed). The daemon now records the server revision
  id it *itself* seals per node (`Core.own_sealed_revs`, written in
  `refresh_after_upload` where the sealed node is already fetched, dropped on
  trash). When a queued write drains and `revision_changed` fires,
  `Core::revision_conflict` checks whether the remote sits at one of *our own*
  sealed revisions (pure `is_own_self_supersede`): if so, no other device touched
  the file — it is a single-writer stall→resume — so it chains (supersedes)
  instead of forking a `(sync-conflict)` copy. Gated on `meta.complete`: an
  incomplete blob's gaps still refer to the stale base, so it keeps the
  non-destructive conflict-copy path. This is a superset guard over the existing
  `refresh_after_upload`/`rebaseline_pending` rebasers, which only fire while a
  handle/op is live; B catches the windows they miss (fresh open during the
  earlier upload's flight, a stale inherited baseline). Tests: `is_own_self_supersede`
  cases in `drain.rs`. *Residual:* the map is in-memory only, so a daemon restart
  between the partial seal and the resume drops the record and that one write can
  still fork — narrow, and B69's revision-id keying already covers the mtime-drift
  half. A persisted own-seal ledger would close it.

**Triage of the copies already made is done (2026-07-25):** all completed files
were promoted back to their canonical names (metadata-only rename, no re-upload)
and the abandoned `.crdownload` stubs trashed.

## B71 — Conflict sweep is not production-ready (auto-trash loop, CRIT-13)

**Status:** Blockers 1–4 fixed in-tree 2026-07-26; items 5–10 and the remaining
cleanup still open. The sweep is safe to leave running (it is report-only by
default and cannot trash anything until explicitly switched to `enforce`).
**Where:** `crates/pdfs-fuse/src/sweep.rs`, spawn site `crates/pdfs-fuse/src/mount.rs`,
config surface `crates/pdfs-core/src/config.rs`.

The B69 fix (see above) shipped a `pdfs-conflict-sweep` thread that **trashes user
files on the real Drive** from a background loop. The identity check itself is
conservative (equal size *and* equal `content_sha1`, missing digest treated as
divergent), but the machinery around it is not yet safe to run unattended. Review
found the following, ordered by severity. Each is a separate defect; they are
grouped because they gate the same feature.

**Blockers (all fixed 2026-07-26 — the sweep may not trash anything without them):**

1. **Data-loss race — trash-then-discard (CRIT).** `remove_conflict_copy`
   (`sweep.rs:137-155`) trashes the node remotely and *then* calls
   `discard_queued_ops(uid)`, which deletes the node's queued ops **and discards
   their staged blobs** (`lib.rs:1544-1553`). The decision was made from the
   `load_all()` snapshot taken at `sweep.rs:77`, with network round trips in
   between. A user edit landing in that window is destroyed — the queued write is
   dropped and its staged bytes, which may be the only copy, are discarded. This
   violates the invariant that `staging/` is never purged. *Fix:* re-read the node
   from the DB and re-verify `active_revision_id` immediately before trashing;
   skip the node outright if `Core.pending` or the op table has any entry for it;
   never let the sweep discard staged bytes.
2. **No kill-switch (CRIT).** `mount.rs:234-239` spawns the loop unconditionally.
   There is no `AppConfig` field and no environment override, so the only way to
   stop a destructive background loop is to uninstall. *Fix:* add
   `AppConfig.conflict_sweep: Option<bool>` (same `Option` idiom as `cache_budget`
   / `ignore_patterns`), a `PDFS_CONFLICT_SWEEP=off` override for support, and a
   Settings toggle.
3. **Open handles ignored (HIGH).** Nothing checks for a live file handle before
   `forget_or_unlink` + `evict_reader`, so the sweep can trash a node a reader
   currently has open. *Fix:* skip nodes with open handles.
4. **Enforcing on day one (HIGH, process).** The sweep exists because B69 proved
   the previous revision-identity check was wrong; betting deletion on the
   *replacement* check in the same release, with no field evidence, is the wrong
   order. *Fix:* ship report-only (log `would-trash` plus the existing
   `ActivityKind::Conflict`), collect a release cycle of real logs, then flip to
   enforcing.

**Fix for 1–4 (landed 2026-07-26):**

- **`SweepMode`** (`pdfs-core/src/config.rs`) — `Off` / `Report` / `Enforce`,
  serialised into `AppConfig.conflict_sweep` (`#[serde(default)]`, so configs
  predating the field still load). `Default` is **`Report`**, which is blocker 4:
  an existing install that has never heard of the setting cannot get the
  enforcing behaviour on upgrade. `AppConfig::resolved_conflict_sweep` layers
  `PDFS_CONFLICT_SWEEP` over the stored value; the precedence is a pure
  `resolve_sweep_mode(env, stored)` so it is testable without mutating process
  state. An unparseable value falls through to the config rather than guessing a
  mode in either direction.
- **Spawn gate** (`mount.rs`) — `SweepMode::Off` skips the thread entirely rather
  than starting an idle one, so the setting is verifiable from outside the
  process (`ls /proc/<pid>/task/*/comm`). Both branches log the resolved mode at
  startup. `mount` grew past clippy's argument limit, so `username` +
  `sweep_mode` moved into a `MountOptions` struct resolved by the caller —
  `mount` still reads neither config nor environment.
- **Interlocks** (`sweep.rs::duplicate_still_removable`) — re-checks, immediately
  before the destructive call, everything the pass decided from its stale
  snapshot: no queued op (new `Db::has_any_op`, a cheap `COUNT` — a queued op
  means bytes are owed an upload and `discard_queued_ops` would throw away the
  staged blob holding them), no open write handle or shared scratch file
  (`Core::is_busy`, checking `active_writes` + `handles` under one `State` lock),
  and the node still at the same revision id, size, digest, name, parent and
  untrashed state. Any doubt — including a failed DB read — means leave it alone:
  a copy that survives to the next pass costs nothing, a wrongly removed one
  costs data. This is blockers 1 and 3.
- **Testability** — the decision moved into a pure
  `decide(node, sibling, base, mode) -> SweepAction`, the same extraction used
  for `revision_changed` and `is_own_self_supersede`; everything that can fail or
  race stays in the caller. Tests now cover removal-when-enforcing,
  report-mode-never-removes, size-differs, digest-differs, missing-digest in both
  directions, and orphaned copies. Gate: `cargo fmt` / `clippy -D warnings` clean,
  `cargo test --workspace` 282 passed (was 270).

**Should fix:**

5. **`load_all()` every pass (MED).** Each 300 s pass pulls the entire node table
   into a `Vec<StoredNode>` and builds a full `(parent, name)` `HashMap`. *Fix:*
   query only conflict-named rows (`name LIKE '% (sync-conflict %'`) and look up
   siblings per parent — O(conflicts), not O(drive).
6. **No batching or per-pass cap (MED).** The B69 incident produced 151 copies;
   that is 151 sequential `trash_nodes` calls in one pass with no cap and no
   backoff. `trash_nodes` takes a slice — batch it and cap per pass.
7. **`online` sampled once per pass (MED).** Read at `sweep.rs:100`, ahead of a
   loop that does network I/O; going offline mid-pass yields a long run of failing
   calls. *Fix:* bail on the first transport error.
8. **Ambiguous sibling index (MED).** `by_parent_name.insert` (`sweep.rs:97`) is
   last-write-wins, so with duplicate `(parent, name)` rows the surviving entry
   depends on `load_all` row order. *Fix:* make the choice deterministic or skip
   ambiguous parents.
9. **Trashed ancestors (LOW).** A copy under a trashed folder finds no live
   sibling and is flagged "orphaned", spamming the activity feed. *Fix:* skip
   nodes with a trashed ancestor.
10. **SHA-1 provenance unverified (MED).** `is_identical` (`sweep.rs:181`)
    compares digests without asserting they belong to the current
    `active_revision_id`. A stale `XAttr` digest from an earlier revision plus a
    size collision would delete real data. (The *missing* digest case is already
    handled correctly.)

**Also required to close:**

- **Tests.** The *policy* is now covered via the pure `decide` (see above). Still
  untested are the *interlocks*, which need a `Core`: pending-op→skip,
  open-handle→skip, revision-moved→skip, offline→skip. Those want a test harness
  that can build a `Core` against a temp DB and a stub client.
- **`conflict_notified` is unbounded.** `HashSet<NodeUid>` that only grows except
  on trash (`sweep.rs:157,170`).
- **Docs.** `docs/ARCHITECTURE.md` thread map does not list `pdfs-conflict-sweep`;
  `docs/RECOVERY.md` needs a "the sweep trashed a file, recover it from Proton
  trash" path.

**Mitigating context from the 2026-07-26 pre-run snapshot:** the account had only
2 conflict copies left, both divergent in size *and* `content_sha1`, so the
correct behaviour for the first live pass is to trash nothing.

## B72 — `pdfs trash` never returns

**Status:** Hardened in-tree 2026-07-26 — the hang is gone by construction; the
underlying slowness of the first refresh is still unmeasured (live-pending).
**Found:** capturing a trash baseline before the B69/B70 live validation. `pdfs trash`
produced no output and was still running when killed at 120 s. Daemon was healthy
and every other CLI call (`pdfs sync list`, `pdfs --version`) returned promptly.
**Where:** `Core::list_trash` (`crates/pdfs-fuse/src/lib.rs`). Not the CLI: the
client's 120 s `READ_TIMEOUT` (`pdfs-core/src/control.rs`) is what ends the call,
and the daemon never writes a response line.

**Root cause.** The trash of this account had never been fetched successfully —
`trash` table empty, no `trash_synced_ms` in `sync_state`. `list_trash` therefore
always took its "never fetched → `rt.block_on(refresh_trash())`" branch, which:

- materialized the *entire* trash in one `enumerate_nodes` call (one S2K unlock
  per node) before persisting anything, so nothing was ever saved and no later
  request could be served from the DB;
- had no single-flight guard on that branch (only `spawn_trash_refresh` did), so
  every attempt started another full refresh;
- was never cancelled when the requester timed out, because the control handler
  sits in `block_on` on its own thread and nothing signals it.

So each look at the trash added a permanent, full-cost refresh to the daemon.
Measured on the live daemon after a handful of GUI/CLI attempts: **11 stale
`pdfs-control` threads**, RSS 2.9 GB. Concurrent with a drain backlog that was
uploading conflict copies continuously (1991 `Conflict` activity rows, multi-GB
uploads), none of the refreshes ever finished.

**Fix (in-tree).** `list_trash` never blocks on the whole refresh: it kicks the
single-flighted background refresh, waits at most `TRASH_FIRST_WAIT` (20 s, well
under the front-end read timeout) on a `Notify`, and answers with whatever has
materialized. `refresh_trash` now materializes in `TRASH_MATERIALIZE_CHUNK` (150)
batches, persisting the cumulative listing after each one, and logs the uid count
plus per-batch and total elapsed at INFO/DEBUG — previously the whole path was
silent, which is why the stall could not be located from the journal.

**Still open:** *why* one refresh is slow enough to matter. The new INFO lines
(`trash refresh: enumerated` / `batch` / `done`) answer that on the next run:
whether `enumerate_trash_node_uids` returns at all, how many nodes the trash
holds, and the per-batch cost. If the count is large, materializing with
`enumerate_nodes_light` (skips the per-file node-key S2K; `size` would fall back
to on-storage size) is the next lever.

**Impact:** the trash listing is the user's recovery path after any destructive
operation, including the B71 sweep. It needs to work before the sweep is allowed
to trash anything.

## B73 — Unimplemented FUSE handlers log a WARN per probe

**Status:** Fixed in-tree 2026-07-26 — live-validated. Found 2026-07-26 by the
expanded acceptance suite.
**Found:** `scripts/fuse-acceptance.sh --live` step "unsupported operations refuse
cleanly". Each probe produced a multi-line `fuser` warning in the daemon journal:

```
WARN fuser: [Not Implemented] symlink(parent: INodeNo(0x2d), link_name: "symlink", target: "link-target")
WARN fuser: [Not Implemented] link(ino: INodeNo(0x94), newparent: INodeNo(0x2d), newname: "hardlink")
WARN fuser: [Not Implemented] mknod(parent: INodeNo(0x2d), name: "fifo", mode: 4516, umask: 0x12, rdev: 0)
WARN fuser: [Not Implemented] setxattr(ino: INodeNo(0x91), name: "user.pdfs.probe", flags: 0x0, position: 0)
WARN fuser: [Not Implemented] lseek(ino: INodeNo(0x93), fh: 0, offset: 0, whence: 3)
```

**Cause:** `ProtonFs` does not implement these callbacks, so `fuser`'s default
trait method answers `ENOSYS` *and logs at WARN*. The refusal is correct — the
acceptance suite accepts every one of them as a clean refusal — but the logging
is not conditional on anything the user did wrong.
**Impact:** cosmetic, but these are not exotic calls: `git` probes symlinks,
`cp -a` probes hard links, `tar` and `cp --sparse` probe `SEEK_HOLE`/`SEEK_DATA`
(`whence: 3` above is `SEEK_DATA`), and any xattr-aware tool probes `setxattr`.
Warnings that look like faults bury real ones and make both `--journal-check`
and manual triage noisier.
**Measured** across two full acceptance runs against one daemon lifetime (PID
887441, mount never remounted):

| op | warnings | behaviour |
|---|---|---|
| `link` | 11 | repeats on every call |
| `symlink` | 4 | repeats on every call |
| `mknod` | 2 | repeats on every call |
| `setxattr` | 1 | kernel cached the `ENOSYS`; never asked again |
| `lseek` | 1 | cached |
| `copy_file_range` | 1 | cached |
| `fsyncdir` | 1 | cached |

So the FUSE kernel module remembers `ENOSYS` for the file-level operations and
stops issuing them for the lifetime of the mount, but not for the namespace
operations, which are re-issued forever.

`link` leads for a sharper reason than "tools probe it": git uses `link()` to
place **every loose object**, falling back to `rename()` when it fails. In one
workloads run, 9 of 10 `link` warnings carried a 38-hex-character name — the
`.git/objects/ab/<38 hex>` layout — from just 3 commits over 3 files. The volume
is therefore proportional to work done, not a fixed startup cost: a `git clone`
of a real repository emits one multi-line warning per object. The lone `symlink`
that is not the acceptance probe is git's `core.symlinks` detection at
`git init` (a random name pointing at `testing`).

That moves this from cosmetic to a genuine operational problem: ordinary use of
git inside a mount can bury every other log line.

**Fix (applied):** all seven callbacks are now implemented explicitly in
`crates/pdfs-fuse/src/filesystem.rs`, each answering with **the same errno
`fuser`'s default would** and nothing else — no WARN. Following `ioctl`
(which already used this pattern and says why in its comment), the errno
contract is unchanged, so the acceptance suite's clean-refusal assertions hold
byte for byte:

| op | errno | why that one |
|---|---|---|
| `symlink`, `link` | `EPERM` | meaningful operation, unrepresentable on Drive; git falls back to `rename()` |
| `mknod` | `ENOSYS` | no devices/FIFOs/sockets; regular files arrive via `create` |
| `setxattr` | `ENOSYS` | surfaces as `EOPNOTSUPP`; reads stay implemented for the thumbnail interface |
| `fsyncdir` | `ENOSYS` | kernel absorbs it and reports success; dir changes are already durable in SQLite |
| `lseek`, `copy_file_range` | `ENOSYS` | **must stay** `ENOSYS` — it is what makes the kernel emulate `SEEK_HOLE`/`SEEK_DATA` and coreutils fall back to read/write instead of failing |

Only the three namespace ops were strictly required (the other four silence
themselves once the kernel caches `ENOSYS`), but implementing all seven keeps the
refusal explicit and documented at the call site rather than inherited.

**Verified:** full live acceptance run after the fix — 19 pass / 1 skip, the
capability diff identical to the pre-fix run, and **0** `Not Implemented` lines
in the journal across the whole run (was 17, 10 of them `link`).
**Regression cover:** the acceptance suite's clean-refusal probes assert the
errno contract; they pass unchanged.

## B74 — A drained create never reaches a sync-folder mount's inode space, so the file reads as empty

**Status:** Fixed in-tree 2026-07-26 — live-validated. Found 2026-07-26 by the
expanded acceptance suite (`regression B70` case), then isolated to a broader
trigger.
**Impact:** on a *sync folder* mount (not `~/ProtonDrive`), a file whose create
was still queued reads back as **0 bytes for as long as the daemon runs**, even
though the bytes are on Drive intact. This is the write-to-temp-then-rename
pattern used by every browser download, most editor saves, `rsync`, and `git`.

**Not data loss.** An earlier revision of this entry called it permanent data
loss; that was wrong and is corrected here. The uploaded revision is complete and
correct: for every "lost" file the node row carried the right `size` and
`active_revision_state: "Active"`, and a daemon restart made all of them read
back with the expected sha256. Nothing had to be recovered. The defect was
entirely local and entirely in the read path.

**Reproduction** (on an on-demand mount, per file: write, close, rename, wait for
`pdfs status` to report the queue drained, then read back):

| scenario | result |
|---|---|
| plain name, never renamed | content intact |
| plain name, renamed after close | **0 bytes until restart** |
| `.crdownload`, renamed after close | **0 bytes until restart** |
| `.crdownload`, never renamed | content intact |
| already-settled file, renamed | content intact |
| any of the above, after a daemon restart | content intact |

The gap between `close()` and `rename()` does not matter — 0 s, 2 s and 20 s all
behave the same. The boundary is not timing but state: the affected node is one
whose **create op was still queued**, on a mount that is not the primary one.

**Root cause — one drain thread, many inode spaces.** The daemon mounts
`~/ProtonDrive` plus one FUSE session per `ondemand` sync folder. Each of those
is a `Core::fork_state` clone with its **own** `State` — its own `entries`,
`by_uid`, `children`, `next_ino` — while sharing `db`, `cache`, `pending` and
`client`. There is exactly **one** `pdfs-drain` thread in the process, owned by
the primary mount, and it serves the whole shared `pending_op` table.

So when a create queued by a sync-folder mount lands, the drain calls
`adopt_real_uid` — which looked only at `self.state`, the *primary* mount's inode
space. The fork's entry keeps its `local~…` placeholder uid forever. And
`Core::read_range` (`crates/pdfs-fuse/src/reads.rs:314`) short-circuits a local
uid to an empty vec:

```rust
if is_local_uid(uid) { return Ok(Vec::new()); }
```

which is why the read is silent, instant, and zero-length. A restart rebuilds
every state from the DB, where `remap_local_uid` had already written the real
uid, so the file reads correctly from then on.

Confirmed live: `read handler uid=local~1785067300752-0 fsize=140000` three
seconds after `pending create landed`, and at the same moment
`adopt_real_uid … hit=None strays=[]` — the primary state had no entry for the
uid at all. `/proc/<pid>/task/*/comm` showed 1 `pdfs-drain` against 8 fuser
sessions.

**Fix:** `Core` gained a `states: Arc<StateRegistry>` — a `Vec` of weakly-held
mounts that every mount publishes itself into (`Core::register_state`, called
from `mount.rs` for the primary and from `fork_state` for each fork), plus
`Core::for_each_state`, which walks every live inode space one lock at a time.
A uid is unique across mounts, so at most one state matches and the rest are
no-ops. Five drain sites were converted: `adopt_real_uid`, `adopt_drained_name`,
the conflict-copy `invalidate_listing`, the `active_writes` baseline rebase in
`refresh_after_upload`, and that function's closing `intern`.

**It was completely silent.** No `ERROR`, no `WARN`, no failed op, no conflict
copy, no retry — `pdfs status` reported the queue drained, because it had. Any
diagnosis that relies on the journal or on queue state will not see this.

**Regression cover:** `scripts/fuse-acceptance.py`, case
`regression B74: rename after close keeps its content` — the narrowest form
(plain names, no overwrite, no transient suffix). It failed in two consecutive
live runs before the fix and passes after it. The pre-existing B70 case failed on
the same cause and now passes too.

**Audit of the rest of the daemon.** Every background-thread `state.lock()` was
reviewed for the same shape. The rule that came out of it: **a uid is unique
across mounts, so uid-keyed work must be broadcast; an inode number is only
meaningful to the mount that minted it, so inode-keyed work must not be.** What
changed:

| file | site | why |
|---|---|---|
| `background.rs` | `apply_event` | the read-side twin of this bug — remote deletes, trashes and renames only ever reached the primary mount's tree |
| `sweep.rs` | conflict-copy `forget_or_unlink` | a conflict copy inside a sync folder lives in that fork's inode space |
| `sweep.rs` | `is_busy` | answered "idle" for every file open on a fork, letting the sweep delete one with a writer attached |
| `sharing.rs` | `rel_path_for_uid` | a share of a node in a sync folder could not be named |
| `drain.rs` | `node_place` | fell through to a DB lookup for fork-resident nodes |
| `lib.rs` | `queue_trash`, `rename`, `remove_replaced` (×2), `drop_local`, `restore` | uid-keyed `forget`/invalidate |

Deliberately left on `self.state`: `upload.rs` (`parent_ino`/`pino` are inode
numbers), the size-upgrade path in `lib.rs`, and `drain.rs`'s `root_uid`, which
is correctly primary-only. The 38 `filesystem.rs` sites are FUSE callbacks —
they already run on the right mount's session thread. `parent_is_gone` never
touches state at all; an earlier note here saying it did was wrong.

The registry is now a `StateRegistry` type with a unit test
(`state_registry_walks_every_live_mount_and_reaps_the_rest`) covering the case
this bug was: a fork that fails to appear in the walk, or one that lingers after
unmount.

**One regression came out of the audit, and is fixed.** Broadcasting
`apply_event`'s parent-listing invalidation meant forks started receiving
invalidations they had never received — and it invalidated a folder once per
event, while a burst of our own uploads produces one event per file all naming
the *same* folder. Every following lookup in that folder then re-enumerated it
from the API: quadratic in the folder's size.

Two mitigations, both in `background.rs`. A batch is collapsed to one
invalidation per folder — `apply_event` records the parent in `DirtyParents` and
`flush_dirty_parents` publishes it once after the batch loop (every `break` path
falls through to it). And a change the daemon made *itself* is recognised as its
own echo rather than treated as foreign: the drain records it
(`Core::note_self_change`, consumed once within `SELF_CHANGE_TTL_MS`), which
stops the feed's report of our own upload from evicting the content blob we just
sent. Neither weakens the result — the state after a batch is what it always was.

**The hang that showed up alongside this was a different bug, and this entry
originally misattributed it.** `readdir stability` at 133.94 s and a concurrency
case that blew past its timeout were blamed here on the invalidation storm. They
were not: the mount was freezing because network work runs on fuser's single
dispatch thread, which is B75. Fixing that took the concurrency case from two
timeouts in three runs to 16.9 / 24.1 / 29.1 s and `readdir stability` to 62.9 s.
The coalescing above is still worth having — it is a real amplification — but it
was never the stall.

**Note on B70:** B70's own fix was never implicated. The `.crdownload` park/un-park
behaves correctly in isolation; the empty read arrived with the rename, on the
same path a plain temp file takes. B70's live validation was blocked on this and
is now unblocked — its acceptance case passes.

## B75 — Network work on fuser's single dispatch thread freezes the whole mount

**Status:** Fixed in-tree 2026-07-26 — live-validated. Found 2026-07-26 while
chasing what was thought to be a B74 regression.
**Impact:** any burst of concurrent writes froze the entire mount — not the
files being written, the *mount*. `ls` on an unrelated directory of it timed out
for minutes. The daemon itself stayed healthy throughout: the primary mount and
the control socket answered normally while a sync-folder mount was unreachable.

**Root cause.** `mount.rs` and `devices.rs` both build their session from
`Config::default()`, which leaves `n_threads` unset, and fuser 0.17 defaults it
to 1 (`session.rs:254`). One dispatch thread per mount reads a request, runs the
handler to completion, and only then reads the next. Six handlers were handed to
the [`Workers`] pool — `lookup`, `getattr`, `readdir`, `read`, `getxattr` and the
slow `serve_lookup` — and everything else ran inline, including handlers that
block on the API:

| handler | blocking work |
|---|---|
| `create` | `upload_file` to mint the node, then `fetch_node` — two round trips |
| `mkdir` | `create_folder`, then `fetch_node` |
| `rename` | `rename_node`, then `move_node`, plus a trash on the replaced-destination path |
| `unlink` / `rmdir` | `trash_nodes`; `rmdir` enumerates the folder first |
| `setattr` (path truncate) | `queue_truncate` → `fill_gaps`, which reads the kept prefix from the remote |

So eight applications each creating a file served one create at a time, and
every `read`, `open` and `lookup` on that mount queued behind them.

**Measured**, acceptance suite's `independent and shared-file concurrency` (8
threads × 16 files), and a 3-second `ls` of the mount root sampled every 4 s
throughout:

| | before | after |
|---|---|---|
| concurrency case | 200 s watchdog ×2, 37 s ×1 | 16.9 / 24.1 / 29.1 s, all pass |
| `ls` of the mount during the run | 33 of 55 probes timed out | 0 of 85 |
| `readdir stability` | 148.8 s | 62.9 s |
| `namespace operations` | 31.3 s | 15.7 s |

**Fix:** each of those handlers now parses its arguments inline — cheap, and it
keeps the error paths prompt — and hands the body to the worker pool as a
`serve_*` method, the pattern `lookup`/`readdir` already used. Metadata work
takes `Lane::Meta`; the truncate path takes `Lane::Transfer` because it can pull
a whole file.

**`release` was deliberately left inline, and this is load-bearing.** Moving it
too made all three concurrency runs fail with `concurrent file 1 mismatch`. The
kernel does not wait for a `release` reply before letting `close(2)` return, so
the only thing sequencing the staging of the written bytes against the `open` +
`read` an application issues immediately afterwards is that both are served by
the same dispatch loop. From a worker, the read overtakes the staging and is
answered from the remote's older revision. Taking `release` off the loop — worth
doing, since `queue_revision` gap-fills a partial write from the network — needs
a per-node "staging in flight" barrier that reads wait on, not the dispatch
loop's accidental ordering.

**Not fixed here.** `n_threads` is still 1; with the blocking handlers moved off,
the loop is short enough that raising it is a separate question. Related findings
from the same audit, still open: cache/reader validity is keyed on
`(mtime, size)` while the revision identity is `active_revision_id`
(`cache.rs:84` — two same-size revisions within one mtime second collide, which
is what a SQLite page rewrite looks like); `local_finish_scan` holds the daemon's
one SQLite connection across a full FTS5 trigram rebuild (`db/local.rs:79`);
`earliest_due_at` has no `<= now` filter where `next_due_op` does, so the drain
busy-spins when offline with a due op (`db/ops.rs:385` vs `:421`); and
`drain_local_node` deletes its op before the fallible `adopt_real_uid`
(`drain.rs:346`).
