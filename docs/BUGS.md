# Bugs & side findings

Running tracker for bugs and loose ends spotted while working on something else —
so they don't get lost when the session that found them ends.

Conventions:

- **Open** / **Fixed (unverified)** / **Fixed** / **Won't fix**
- "Fixed (unverified)" means the code change is in but nobody has driven the real
  flow against it yet. It stays that way until someone does.
- Record how it was *found*, not just what it is. The repro is the expensive part.

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

**Status:** Fixed in code (unverified on a live mount, 2026-07-22)
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

**Verified:** `cargo clippy -p pdfs-fuse --all-targets -- -D warnings` and the
`pdfs-fuse` unit suite pass. A fault-injected/live mode-switch test is still needed.

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

**Status:** Open / In Progress  
**Found:** 2026-07-21, multi-agent concurrency audit (`audit_bugs.md` HIGH-02)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `sync.rs`, `devices.rs`

**Cause:** Invoking `rt.block_on(...)` while holding `state.lock()` deadlocks worker threads and the main FUSE loop under high concurrency. Most major handlers have been refactored to drop `state.lock()` before invoking async runtime methods, but a full systematic audit across all background tasks and FUSE callbacks is required to eliminate all instances.

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

**Status:** Open  
**Found:** 2026-07-21, multi-agent sync engine audit (`audit_bugs.md` HIGH-05)  
**Where:** `crates/pdfs-core/src/cache.rs:L157`, `Baseline`

**Cause:** Conflict resolution baseline detection relies strictly on `(mtime, size)` tuple comparisons without content hashing or revision IDs. Concurrent edits landing within the same second that produce identical file sizes match baselines, causing offline edits to overwrite remote changes silently without creating `(sync-conflict <ts>)` copies.

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

**Status:** Open / In Progress  
**Found:** 2026-07-21, multi-agent database audit (`audit_bugs.md` MED-03)  
**Where:** `crates/pdfs-core/src/db/ops.rs`, `crates/pdfs-core/src/db/mod.rs`

**Cause:** Long-running upload drain transactions hold SQLite write locks while worker threads wait for `state.lock()`, causing periodic FUSE response latency spikes during heavy sync operations.

---

## B31 — Unimplemented Special Files & Attribute Modifications (MED-04)

**Status:** Open  
**Found:** 2026-07-21, multi-agent POSIX audit (`audit_bugs.md` MED-04)  
**Where:** `crates/pdfs-fuse/src/lib.rs`

**Cause:** Symlinks (`symlink`/`readlink`), hardlinks (`link`), FIFOs/sockets (`mknod`), and `chmod`/`chown` attribute modifications return `ENOSYS` or are silently ignored.

---

## B32 — Race Condition in Concurrent Handle Release and Open (MED-05)

**Status:** Open  
**Found:** 2026-07-21, multi-agent concurrency audit (`audit_bugs.md` MED-05)  
**Where:** `crates/pdfs-fuse/src/lib.rs:L4042, L4574`

**Cause:** Opening a file for writing while a previous handle release is draining creates overlapping scratch write handles and out-of-order remote revision uploads.

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

**Status:** Open  
**Found:** 2026-07-21, multi-agent permissions audit (`audit_bugs.md` MED-07)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/sharing.rs`

**Cause:** Shared "Viewer" (read-only) folders are exposed via POSIX with write permissions (`0755`). Local writes succeed initially on the mount but fail perpetually during background online drain with `403 Forbidden` errors.

---

## B35 — IPC Unix Socket Creation Permission Race (MED-08)

**Status:** Fixed (verified 2026-07-20 - see B6)  
**Found:** 2026-07-21, multi-agent security audit (`audit_bugs.md` MED-08)  
**Where:** `crates/pdfs-fuse/src/control.rs`, `crates/pdfs-core/src/config.rs`

**Cause:** Binding the domain socket before setting `chmod(0600)` created a race window where local users could connect before permissions were enforced.

**Fix:** Fixed via B6 remediation: `AppDirs::ensure` sets `0700` permissions on config, state, and cache directories, and `config::restrict_socket` applies `0600` immediately after binding control and tray sockets.

---

## B36 — Unicode Normalization Discrepancy (NFC vs NFD) (LOW-01)

**Status:** Open  
**Found:** 2026-07-21, multi-agent sync audit (`audit_bugs.md` LOW-01)  
**Where:** `crates/pdfs-fuse/src/lib.rs`, `crates/pdfs-fuse/src/sync.rs`

**Cause:** macOS/HFS+ NFD UTF-8 path inputs differ from Linux NFC UTF-8 paths, causing duplicate folder creation or lookup misses for accented filenames.

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

**Status:** Open
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

---

## B40 — Sync Upload Can Stream Torn Live Content (HIGH-10)

**Status:** Open
**Found:** 2026-07-22, sync concurrency audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, upload apply and baseline update

**Cause:** Planning stats a path, but upload later opens and streams the live
file. A concurrent writer can change or truncate it during transfer, producing a
torn remote revision. Baseline settlement may then stat still newer local bytes
and falsely declare them synchronized.

**Required fix/test:** Upload an immutable staged snapshot, or validate an open
descriptor before and after streaming and refuse baseline settlement on change.

---

## B41 — Sync Folder Removal Races Active Reconciliation (HIGH-11)

**Status:** Open
**Found:** 2026-07-22, sync lifecycle audit
**Where:** `crates/pdfs-fuse/src/devices.rs`, `remove_sync_folder`

**Cause:** Folder removal does not acquire the per-folder `sync_lock` used by a
reconcile pass. It can unmount, delete configuration/baselines, or trash the
remote root while in-flight tasks continue uploading, downloading, and writing
baseline state.

**Required fix/test:** Serialize removal with reconciliation, cancel/disable the
folder, await active work, then unmount and remove durable state.

---

## B42 — Local Delete Failure Is Recorded as Sync Success (HIGH-12)

**Status:** Open
**Found:** 2026-07-22, sync error-path audit
**Where:** `crates/pdfs-fuse/src/sync.rs`, local file/directory deletion branches

**Cause:** Some local removal errors are ignored while the baseline is removed
and success is logged. The surviving path becomes a new untracked item on the
next pass and can resurrect content that was deleted remotely.

**Required fix/test:** Treat `ENOENT` as success; for every other removal error,
retain the baseline and increment `Outcome.errors`.

---

## B43 — FUSE Directory Cookies Are Not Stable (MED-09)

**Status:** Open
**Found:** 2026-07-22, FUSE/POSIX audit
**Where:** `crates/pdfs-fuse/src/filesystem.rs`, `ProtonFs::serve_readdir`

**Cause:** `readdir` rebuilds a live vector for every call and uses its array
index as the continuation cookie. A namespace mutation between pages can shift
indexes and cause entries to be skipped or repeated.

**Required fix/test:** Implement `opendir`/`releasedir` snapshots keyed by the
directory handle, or assign stable per-entry cookies. Test with deliberately
small reply pages and mutation between calls.

---

## B44 — FUSE Background Workers Have No Coordinated Shutdown (MED-10)

**Status:** Open
**Found:** 2026-07-22, daemon lifecycle audit
**Where:** `crates/pdfs-fuse/src/mount.rs`, mount lifecycle; drain/index/sync loops

**Cause:** Long-lived threads and tasks retain `Core` clones but share no
cancellation token or owned join set. Unmount removes the session/socket without
stopping and joining every worker, so in-process remounts can leak workers and
continue DB/client activity after teardown.

**Required fix/test:** Add shared cancellation, interruptible waits, owned join
handles, and ordered shutdown. Repeated mount/unmount tests should return thread
counts to baseline and observe no post-unmount DB work.

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

---

## B47 — FUSE Accepts Path Components Longer Than NAME_MAX

**Status:** Fixed (unverified)
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
