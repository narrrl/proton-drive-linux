# Changelog

All notable changes to the Linux client since the 1.0.0 release. Format follows
[Keep a Changelog](https://keepachangelog.com/1.1.0/); versions follow semver, and every
release is tagged `vX.Y.Z` with `Cargo.toml` and `packaging/PKGBUILD` in agreement.

`Schema:` lines record the SQLite `SCHEMA_VERSION` (`pdfs-core/src/db/migrations.rs`) a
release ships. Migrations are forward-only — a database written by a newer client is a hard
refuse-to-open, not a downgrade, so rolling back a release means restoring the cache from
scratch (user data in `staging/` and `recovery/` is never touched by this).

## [1.9.1] — 2026-08-21

Thumbnails everywhere the GUI lists files, a status bar under the Files browser, and the
repository's contribution/branding files. No schema change. Schema: **28**; SDK unchanged at
`proton-sdk` / `proton-drive-rs` **0.6.1**.

`exiftool` (`libimage-exiftool-perl` / `perl-image-exiftool` / `perl-Image-ExifTool`) is a new
runtime dependency, declared in the `.deb`, `.rpm` and `PKGBUILD` packaging. It is only used to
pull the embedded preview out of a camera RAW; without it every other image format still gets a
thumbnail.

### Added
- **Thumbnails for ordinary Drive files**, not just the Photos timeline — painted in Files (both
  the grid and the Name column of the list), Shared, Shared by me, and Trash. Standard raster
  formats (`bmp`, `gif`, `jpeg`, `png`, `tiff`, `webp`) are decoded in the daemon; camera RAWs
  (`arw`, `cr2`, `cr3`, `dng`, `nef`, `orf`, `raf`, `rw2`, and the rest of the concrete list) are
  served from their embedded preview via `exiftool`. Previews are cached beside the existing photo
  thumbnails, outside the cache-budget scan, and tagged with the node's modification time — so a
  new revision of an image can never be shown with its predecessor's thumbnail.
- Thumbnails for visible rows are requested opportunistically as tiles scroll in, and the daemon
  drops work for listings the user has already navigated away from. The listing generation is
  issued by the daemon (`ReserveFileThumbGeneration`), not seeded from the front-end's clock.
- **Build a whole folder tree ahead of time** from the Files toolbar: one recursive local-thumbnail
  job per daemon, with live scan/found/completed/failed progress and a Cancel that appears only
  once the daemon has acknowledged the start. The job outlives navigation — it is deliberate work,
  separate from the opportunistic per-tile requests.
- **A Dolphin-style status bar under the Files browser**: the current listing's counts, a Zoom
  slider for the icon grid, and account storage usage. Zoom resizes the realised grid cells rather
  than rebuilding the model, so dragging the slider does not tear down the selection; the default
  size reproduces the previous fixed 72 px tiles. Capacity stays out of the bar until a real
  reading lands, and the quota fetch (60 s TTL) now also runs while Files is on screen instead of
  only in Settings.
- Repository housekeeping: `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, issue and pull
  request templates, a project logo, and workspace crate metadata (`repository`, `homepage`,
  `readme`, `keywords`, `categories`).

### Fixed
- A remote thumbnail miss no longer suppresses local generation. The two negative results are now
  held in separate bounded caches: "Proton has no thumbnail for this node" and "this exact revision
  was decoded locally and could not be read" mean different things, and only the second is a
  permanent verdict for that revision.
- A RAW whose preview could not be extracted because of the *environment* — `exiftool` missing, a
  failed download — is retried instead of being remembered as undecodable. Only bytes conclusively
  outside the supported formats are cached as a negative.
- A recursive build that finds a uid an opportunistic tile job already owns waits briefly for it
  rather than either duplicating the work or blocking on it forever, and a cancellation racing a
  waiter's registration is caught by a backstop instead of leaving the waiter parked.
- Starting a build for a root that is already building attaches to that job; starting one for a
  *different* root is refused explicitly rather than silently reporting the running job's progress
  as the caller's.
- Decrypted camera RAWs are staged for `exiftool` in an owner-only (`0700`) directory of their own,
  removed as soon as the preview is out and swept at cache open after an unclean shutdown.
- The RAW extension list had `rw` (not a format) and was missing `raw`; both classification helpers
  now read from one shared table instead of two hand-maintained `matches!` arms.
- The tray and desktop-entry icons were wrong/missing; `.desktop` files and the packaged SVG were
  corrected.

## [1.9.0] — 2026-08-18

Google Photos import, and a pass over the desktop app's interaction gaps. No schema change.
Schema: **28**. Builds against the published `proton-sdk` / `proton-drive-rs` **0.6.1** — the album
writes this release needs are in the registry crates, so the local `[patch.crates-io]` shim the
feature was developed against is gone and no local path dependency remains.

### Added
- **Import a Google Photos Takeout export into Proton Photos** — `pdfs import-google-photos
  takeout-*.zip` (add `--dry-run` to see what it would do, `--wait` to follow it), or the import
  button in the Photos header, which multi-selects the export's zips. Albums in the export are
  recreated as Proton albums and their photos filed into them; year folders and `Archive/` import
  into the timeline; `Trash/` is skipped. Capture times and favourites come from Google's per-photo
  JSON sidecars, including the truncated and `(N)`-swapped sidecar names Google produces.
- Photos already on the account are **not re-uploaded**: the import asks the photos volume which of
  the export's names exist (batched, 150 per request), reads and SHA-1s only those files, and skips
  the ones whose content digest matches too — Proton's own duplicate rule. A library already partly
  synced by Proton Photos on a phone therefore imports only what is missing, and an interrupted
  import resumes by being run again. Google keeps a full copy of a photo in every album folder it
  appears in; those fold into one upload that joins several albums.
- The import streams each photo straight out of its zip, so an export needs no scratch space to
  unpack into, and reports as a job in `pdfs transfers` and the GUI's activity view. `pdfs
  import-status` shows the counts, `pdfs cancel-import` stops it — a cancelled run still files what
  it uploaded into its albums.
- **An Import page in the GUI** (Settings → *Import from Google Photos*, or the import button in
  the Photos header). Archives are staged by drag-and-drop onto the page or through the file
  chooser, listed with their sizes so the set can be checked before anything is sent, and launched
  as either a Scan (dry run) or a confirmed Import. The page shows live progress off the import's
  job, keeps the finished run's report on screen, and can stop a run in flight; because the import
  outlives the page, it is polled wherever the user navigates and its completion is reported as a
  desktop notification.

- SDK (`proton-sdk-rs` **0.6.1**): album **writes** — `ProtonPhotosClient::create_album` and
  `add_photos_to_album`, ported from the TypeScript SDK (upstream C# has no album write API), plus
  batch duplicate detection (`find_duplicates_many`, `name_collisions`). The workspace deps moved
  `"0.6.0"` → `"0.6.1"` and `Cargo.lock` re-resolved to the registry crates. Adding a photo from an
  album *shared with us* (a different volume) is still unported and fails that photo's outcome.
- **Multi-select in Files.** Both views select a batch (click-drag, Ctrl-click, Shift-click); a bar
  slides in reporting the count and offering Move to Trash and offline on/off for the whole
  selection. Delete acts on everything selected, Esc clears it, and right-clicking inside a
  selection offers the batch actions rather than the one row under the pointer. Actions that need a
  single subject — rename, move, share, revisions — stay single-entry, and the details pane steps
  aside for a batch instead of speaking for it.
- **Undo on trash.** "Moved *X* to Trash" now carries an Undo that restores the batch, single files
  included.
- **The empty states are a place to act**: an empty folder offers Upload files / New folder, and an
  empty photo timeline offers Upload photos / Import from Google Photos, instead of describing what
  the user could go and do elsewhere.
- **Ctrl+F works from any page**, navigating to Files and focusing the search box, rather than being
  silently inert everywhere but Files.
- Staged Takeout archives survive closing the window; archives that have since been moved or deleted
  are dropped from the restored set.

### Fixed
- Settings showed the local cache as "X of 0 B used" when no cache budget was set. An unset budget
  means *unlimited*, so it now reads "X cached — no limit set" rather than implying a zero cap.
- The cache-budget editor fired a `SetCacheBudget` round-trip — and a toast — on every step, so a
  run of `+` clicks applied a series of caps the user never asked for and stacked a column of
  toasts. It now waits for the value to settle.
- A long pin list pushed everything below it off the end of Settings. It shows the first six with a
  "Show all N" row for the rest.

## [1.8.2] — 2026-08-17

Closes the two findings 1.8.1's live verification produced (B85, B86) and finishes the data-safety
audit entries that had been sitting as "fixed in code, verification pending" since 1.0 (B49–B55).
No schema change. Schema: **28**.

The read path no longer assumes a revision is a series of 4 MiB blocks; it takes the geometry from
the revision. The B85 investigation also **contradicted its own bug report** — see Notes, because
the conclusion recorded there was that reads of such files were losing data, and they were not.

### Fixed
- A folder switched from mirror to on-demand now shows the files the mirror pass itself uploaded.
  The sync engine created nodes on the remote without invalidating the parent's cached child
  listing, and that listing is a database flag rather than an in-memory cache — so it survived a
  daemon restart, and the files stayed invisible until the folder was switched back. Every remote
  mutation the engine makes (folder create, file upload, and the three trash paths) now drops the
  parent's cached listing by uid, which reaches a folder that no mount has resident — the case a
  mirror folder always is (`docs/BUGS.md` B86).
- `pdfs ls` and `pdfs refresh` accept a path under any mount the daemon owns, not only the primary
  one. Both previously answered "is not under the mountpoint" for anything inside a secondary
  on-demand mount, which left a folder serving a stale listing with no manual way to re-enumerate
  it. A path under nothing this daemon has mounted still gets the same error as before.
- A file whose blocks are not 4 MiB is no longer read twice over. Block offsets came from
  `index * 4 MiB`, so on a revision laid out differently every block the client asked for
  straddled two of the revision's own, and each was fetched and decrypted to answer for one — then
  cached at boundaries the next read would not ask about. Offsets now come from the revision's
  recorded block sizes, and the on-disk cache, the in-memory ring and the in-flight fetch key
  address a block by the byte range it covers rather than by its index (`docs/BUGS.md` B85).

### Changed
- The block cache records each block's plaintext start alongside its `(mtime, size)` tag, and each
  revision's block table in a small sidecar beside its blocks. Blocks cached by an earlier release
  carry no start; they are honoured wherever the old 4 MiB assumption and the new plan agree on
  the offset, which is every file this client uploaded — so the existing cache survives the
  upgrade intact and is quietly refetched only for the files it was placing wrongly.

### Notes
- **B85's diagnosis was wrong, and the entry has been corrected rather than closed.** It read a
  set of journal lines as proof that this client was requesting wrong byte ranges and being saved
  only by B84's repair path. `RevisionReader::read_at` does not work that way: it plans over the
  revision's real block sizes and clamps every block to the range asked for, identically in the
  0.6.0 the client builds against and in every earlier version. A read can therefore never return
  more bytes than it requested — yet every quoted line shows exactly that (`blen=4194304
  got=4939506`), and the one place in the client that logs that shape can only reach it on the
  opposite condition. The numbers were either transcribed with two fields swapped or captured from
  a build not in the history; they want re-capturing from the journal before anything is concluded
  from them. What was real is the assumption itself, and that is what this release removes — a
  cost fix, not a correctness one. Nothing here changes what a read returns.
- The first read of a file still plans on the 4 MiB assumption, and learns the real geometry as a
  side effect of opening the reader it needs anyway. Resolving a reader merely to *plan* would
  reintroduce the per-read key derivation the block cache exists to avoid, so the first read of a
  non-4-MiB file pays one straddling fetch and then self-corrects.
- Seven data-safety audit entries (B49–B55) moved from "fixed in code, verification pending" to
  verified. The fixes were all genuinely present and needed no change; what they lacked was the
  injected-failure testing each entry asked for, and that is what was written — unwritable
  directories and `chmod 000` subtrees for permission failures, a real `/dev/shm`↔`/tmp` device
  boundary for the cross-filesystem fallbacks, SQLite's `max_page_count` for a full disk, an abort
  trigger for a failed insert, and truncated and unpublished sidecars for the crash boundaries.
  Two seams were added to make failure arrangeable at all: `stage_write_at`, mirroring the
  `preserve_write_at` that already existed, and `walk_local_tree` lifted out of the sync engine so
  the local scan can be driven against a real tree without a daemon.
- **Two of those entries are not fully closed, and say so.** B51 and B52 concern crash consistency,
  and nothing short of a machine actually losing power tests that; the crash *boundaries* are
  covered by reconstructing on disk exactly what a crash at each point leaves. B54 also diverges
  from its required fix on purpose: it has no "deliberate delete" confirmation path, so a total
  local wipe is refused outright rather than offered for confirmation. That is stricter than asked
  for, and the cost is that a user who really did delete everything must remove and re-add the
  sync folder.
- B86's fix routes only `ls` and `refresh` to the owning mount. The other path-addressed control
  requests — pin, rename, move, delete, sharing — still resolve against the primary mount only.
  That is a larger change and was not what blocked the repro; it is recorded as an open follow-up
  (`docs/BUGS.md` B88).
- Three findings were opened rather than fixed: the geometry above is learned one read late, so a
  non-4-MiB file pays a single wasted fetch on its first read (B87); the remaining path-addressed
  control requests are still primary-only (B88); and the container-level `CLAUDE.md` still
  describes the SDK dependency as 0.1.11 when 1.7.1 bumped it to 0.6.0, which is what sent the
  B85 investigation to the wrong source tree to begin with (B89).

## [1.8.1] — 2026-08-16

Some files read short through the mount and reported success — an application got a truncated
copy with nothing anywhere saying so (`docs/BUGS.md` B84). Found because GNOME's file indexer
kept complaining that PNGs under `~/ProtonDrive/Pictures` were corrupt; it was right. The rest
of this release is the long-standing architectural backlog from the July concurrency audits
(B25, B32, B39, B40, B43, B44), each verified against the current code before being fixed.
Schema: **28** — migration V28 adds `sync_entry.local_mtime_ns`.

### Fixed
- A read no longer trusts a revision whose recorded block sizes disagree with the file's size.
  Such a file leaves the block/range path entirely: it is fetched through the whole-file,
  manifest-verified download — the same path `pdfs pin` uses, and the reason pinning was the
  workaround — and served from the resulting cached blob. One download per file however many
  reads discover the problem at once, and files above 512 MiB fail `EIO` with a note to pin
  them rather than pulling down gigabytes for one read.
- A block that comes back shorter than the file's size implies is never served, cached or kept
  in memory. Assembling a read out of short blocks is what quietly produced the truncation:
  the missing bytes were simply not appended, the kernel handed userspace a short read, and
  every tool reads a short read as end-of-file.
- A synced folder no longer reports the same deletion as an error on every pass. Deleting a
  local file that something else had already deleted counted as a failure, which kept its
  baseline and made the next pass try again — forever. A path that is already gone is the
  state the sync pass wanted (`docs/BUGS.md` B42).
- A mirror folder no longer misses an edit made in the same second as its last sync. Change
  detection compared whole seconds, so rewriting a file in place without changing its length
  read as "unchanged" and never uploaded — the remote silently stayed behind until something
  else about the file moved. Baselines now record the modification time to nanoseconds
  (`docs/BUGS.md` B25).
- A sync download no longer overwrites an edit that finished while it was running. The
  destination is re-checked in the last moment before the download is published, and a local
  file that has moved on since the pass planned for it is kept as a conflict copy — the same
  resolution a both-sides-changed conflict gets (`docs/BUGS.md` B39).
- An upload interrupted by a writer no longer reports itself as synchronized. The baseline now
  records the content that actually went up rather than whatever is on disk when the transfer
  ends, so a revision torn by a concurrent write is replaced by a clean one on the next pass
  instead of standing as the file's content on Drive (`docs/BUGS.md` B40).
- Listing a directory that changes while it is being read no longer skips or repeats entries.
  `readdir` resumed by index into a listing rebuilt on every call, so a create or a trash
  between two pages shifted everything after it. The listing is now frozen for the life of the
  directory handle (`docs/BUGS.md` B43).
- Opening a file for writing while a previous close is still publishing no longer discards that
  close's revision. The base is re-checked at the moment the write handle is installed, rather
  than trusted across a copy that can be the size of the whole file (`docs/BUGS.md` B32).
- Unmounting now stops and joins the daemon's background workers instead of leaving them
  running. The drain, sync engine, conflict sweep, online probe, local indexer and control
  listener share a stop signal and interruptible waits, so an in-process remount no longer
  starts a second set on top of the first (`docs/BUGS.md` B44).

### Notes
- The size `stat` reports was always the correct one; it is the reader's belief about where
  blocks end that was wrong, so nothing about reported sizes changes.
- Verified live on the affected account: four multi-block PNGs that had been reading short now
  deliver their full size, and the journal shows each one repaired. The diagnostics also
  answered the open question, and not as expected — the revision metadata is fine; those files
  simply do not have 4 MiB blocks, and this client assumes they do. Filed as `docs/BUGS.md`
  B85. Until that is fixed, a file with non-4-MiB blocks is downloaded whole on its first read.
- Migration V28's new column is nullable rather than converted from the existing seconds. A
  baseline row written before this release has no sub-second time to recover, and inventing
  zero for it would make every already-synced file compare as changed and re-upload an entire
  mirror on first start; `NULL` keeps those rows comparing exactly as they did, and each gains
  a real value the next time its path syncs.
- Three audit entries closed without a code change, having been fixed already or being correct
  as they stand: sync-folder removal is serialized against reconciliation (B41), the drain's
  database contention was removed by the read-only connection pool and the claim column (B30),
  and the refusals for symlinks, hard links, device nodes and `chmod` are deliberate — Drive
  cannot represent them, and a local-only representation no other Proton client can read would
  be worse than a clean refusal (B31).
- Six of this release's fixes were then driven live against the installed 1.8.1 on a real
  account: the same-second edit uploads (B25), three rapid rewrites of a 64 MiB file publish the
  third one byte for byte (B32), a write racing a download survives as a conflict copy (B39), an
  upload torn by a concurrent write heals itself on the following pass (B40), a `getdents64`
  walk with a 160-byte buffer against a directory mutated between pages returns every entry
  exactly once (B43), and a restart of a daemon with six mounts and a busy drain stops in 110 ms
  and comes back at the same thread count (B44). B42 could not be driven from a single machine —
  see B86 below.
- One new bug came out of the verification: an on-demand mount serves a folder listing that
  predates the sync engine's own uploads, so files the mirror pass created are invisible until
  the folder is switched back (`docs/BUGS.md` B86). Nothing is lost — they are on the server and
  come back on the next mirror pass — but they cannot be seen or named while the folder is
  on-demand, which is also what blocks B42's live verification.

## [1.8.0] — 2026-08-16

A queued write whose node had left the local tree was retried every five seconds forever and
counted as an ordinary pending upload — 28 of them, holding 18.4 GiB, had been doing that for
30 days on the machine that found it (`docs/BUGS.md` B83). Schema: 27 — migration V27 adds
`pending_op.access_deferred_since`.

### Fixed
- The drain no longer reads "I have never heard of this node" as "you may not write this
  node". A missing authority is now resolved against the remote: the node is re-interned if
  it is still there, and reported as a real failure if it is gone. Ops that had been stuck
  invisibly for weeks now say what is wrong with them on the first pass.
- A create or mkdir resolves its *parent* as the authority, so the refetch asks about the
  folder rather than the node it has not made yet.

### Added
- Access deferrals are bounded. A deferral still costs no attempt and records no error for
  the first five minutes — the case it exists for is a share downgraded mid-queue and undone
  a moment later — but past that window each recheck records a failure, so the op enters the
  ordinary backoff and appears in `failing_ops` and the `pdfs status` error text like any
  other stuck operation. The first deferral of a run is logged at `warn` rather than `debug`.

### Notes
- No staged data is touched by any of this. A reported failure keeps its `pending_op` row and
  its staged blob exactly as before; the change is in what the user is told, and in the queue
  no longer waiting on a permission change that was never coming.

## [1.7.1] — 2026-08-16

Write-back throughput follow-up to 1.7.0, same audit
(`docs/plans/audit-2026-08-15-perf-locks.md`). Schema: 26 — migration V26 adds
`pending_op.claimed_at`.

### Added
- Parallel write-back drain: three workers share the queue through a `pending_op.claimed_at`
  claim column (migration V26), so a large upload no longer holds every queued rename, trash
  and small write behind it. Ordering is preserved per node — the claim query never offers
  two workers ops for the same uid — and claims left by a crashed run are cleared at open.
- An upload already on the wire is cancelled when a newer write, a trash or a discard
  supersedes it, instead of spending the uplink on a revision the next operation replaces.

### Changed
- The revision debounce is adaptive: it widens toward how long that node's last upload
  actually took (bounded to 2–60 s), so a file saved faster than it can be sent supersedes
  in the queue rather than mid-upload.
- Workspace deps bumped to `proton-sdk` / `proton-drive-rs` 0.6.0.

## [1.7.0] — 2026-08-16

Performance and robustness pass over the mount, from the architecture audit in
`docs/plans/audit-2026-08-15-perf-locks.md`. Schema: 25.

### Added
- Sequential-read prefetch (`Core::prefetch_ahead`): per-file sequential detection, depth
  doubling 2→8, reset to 0 after a seek, skipping blocks already cached, and bounded by its
  own permit budget so it can never queue ahead of a demand read.
- Single-flight block fetching (`BlockFlight`) keyed on `(uid, mtime, size, block)`, so a
  demand read, kernel read-ahead and prefetch of the same block share one download+decrypt.
- In-memory block LRU (`BlockRing`) in front of *both* block paths; a block read off disk is
  promoted into it, so the following ~31 kernel reads of that 4 MiB block cost nothing.
- Read-only SQLite connection pool (`ReadPool` / `Db::read`) with every `SELECT`-only method
  routed at it, so a read no longer waits on the write connection.
- `nodes.path` persisted and backfilled, replacing the recursive `path_of` walk.
- Single-writer `flock` on the database; a `SQLITE_CORRUPT`/`NOTADB` file is quarantined as
  `cache.db.corrupt-<ts>` (WAL sidecars too) and rebuilt rather than failing the mount.
- `VACUUM` and the deep `integrity_check` now ack immediately and run as background jobs;
  `pdfs doctor` polls for the verdict.
- Staging reconcile at mount: staged blobs no queued operation refers to are re-queued, or
  named in the log when they cannot be addressed.
- Queue/staging visibility — `Status` gained `parked_uploads`, `failing_ops`,
  `failing_error`, `staged_bytes`, `staged_oldest_secs`; `pdfs status` prints them when
  non-zero.
- `statfs` implemented against the account quota; FUSE `init` negotiates `max_readahead`,
  `max_background`, `PARALLEL_DIROPS` and `CACHE_SYMLINKS`; `blksize` raised to 1 MiB.

### Changed
- `State` no longer writes to SQLite under the inode lock — mutations queue into an outbox
  applied after the lock is released, so interning a 5000-child listing no longer commits a
  transaction with the whole mount frozen.
- Re-listing an unchanged folder no longer touches its subtree; when a subtree really moved,
  one recursive walk carries both the new path and the inherited trashed flag.
- `release(2)` no longer gap-fills from the network: it stages an incomplete blob from local
  caches only and the drain thread finishes it off the dispatch loop.
- A partial write over an undrained one is merged instead of refused.
- Size-upgrade waiters park on a queue instead of occupying a worker thread, capped at 8
  folders in flight; past the cap `stat` answers provisionally.
- Block-cache disk writes moved off the read's critical path; no more fsync per cached block.
- Cache LRU touches buffered in memory and flushed in one transaction.
- Search: sync-folder list and pin lookups hoisted out of the per-hit loop.
- SQLite tuned (`cache_size`, `mmap_size`, `temp_store=MEMORY`, `wal_autocheckpoint`) and
  three new indexes added.
- Mount registry lock released before FUSE teardown/join; kernel notifications batched under
  the state lock and flushed after it.

### Fixed
- **Data loss:** `ENOSPC` is now propagated from `write`/`fsync`/staging instead of being
  swallowed, with emergency eviction wired up and scratch marked durable before any staging
  error; a failed `record_pending_write` fails `release` rather than dropping the write.
- **Data loss:** the conflict sweep no longer discards queued operations after the trash
  round trip.
- **Data loss:** `rescue_scratch` moves a blob with an unreadable sidecar to `recovery/`
  unaccompanied instead of fabricating a sidecar for it.
- Own-sealed revisions are persisted (7-day TTL), so a restart no longer reopens the
  transient-download fork window fixed in 1.1.0.
- Depth-capped `is_pinned` (cycle-safe), daemon write timeout, and every client-supplied
  `limit` clamped.
- `recover_fsynced_writes` no longer scans the recovery directory on every idle pass.

## [1.6.0] — 2026-08-12

Search-index correctness and ranking (`docs/plans/search-index.md`). Schema: 21 —
migration V21 rebuilds `nodes_fts`.

### Fixed
- **B80** — 29% of the account was missing from the search index. `node_is_indexable_tx`
  required a null-parent ancestor, which only the My Files root has, so every node under a
  device folder was dropped from `nodes_fts` on its first upsert after the v16 backfill
  (2,705 of 9,252 nodes on the account this was diagnosed against). The rule now matches the
  backfill, migration V21 replays it, nodes under a trashed folder stay out, and the walk is
  depth-capped.
- **B81** — the prompt labelled every Drive hit "My files", including device folders; pin
  rows also took `is_dir` from `Pin::recursive`.
- **B82** — a `parent_uid` cycle hung the daemon on any short search: the recursive CTEs in
  `path_of` and the pin walk use `UNION ALL`, which does not deduplicate. `path_of` is now
  depth-capped at 256 and returns the truncated path.

### Changed
- Local search ranking: whole-query prefix matches get their own lane (newest first) ahead
  of trigram rank, which on a home directory full of dependency trees was burying the
  obvious answer — 73% of the 500 best-ranked candidates for "test" came from one Go module
  cache. The single-character lane, which recovers a typo that destroys every trigram, stays
  behind it.

## [1.5.0] — 2026-08-11

Schema: 20.

### Added
- **`pdfs-prompt --dmenu`** — the same search through an external dmenu-style launcher
  (fuzzel, rofi, wofi, tofi, bemenu, dmenu) instead of the built-in GTK HUD, so the prompt
  matches a tiling-WM desktop. A launcher filters a fixed list rather than re-querying per
  keystroke, so searching is a two-step loop; `--query` skips the first step, fuzzel and rofi
  get file-type icons. Configurable as `"prompt": { "mode": "dmenu", "menu": [...] }`, with
  `--gtk` overriding for one invocation.
- **Configurable open-with rules** (`pdfs-core/src/opener.rs`): an ordered list of name/class
  patterns in `config.json` under `open_with`, each mapped to argv and optionally wrapped in
  a terminal, first match wins, falling back to `xdg-open`. Lets "open a text file" mean
  `nvim` in a terminal without breaking every graphical caller. Absent config reproduces the
  old behaviour exactly.

### Fixed
- The prompt and dmenu openers hand applications the cached file while matching rules
  against the *original* Drive filename, so e.g. `.md` files stop being routed by their
  cache-path extension.

## [1.4.0] — 2026-08-10

Schema: 20.

### Added
- **File version history** (`pdfs-fuse/src/revisions.rs` + a GTK versions dialog): list a
  file's revisions, restore one, delete one, or write an old one out to a local path. A
  restore re-points the file server-side, so no content crosses the wire and nothing enters
  the drain queue; the daemon drops its cached content and repopulates on the next read.
- **Persistent SDK entity cache** (`pdfs-core/src/sdkcache.rs`): the SDK's decrypted node
  metadata now survives a daemon restart instead of re-walking the tree. It lives in its own
  `sdk_cache.db`, deliberately off the daemon's hot `cache.db` mutex, holds no key material,
  needs no migrations, and is cleared on logout.
- Per-node outcomes for batch operations (`pdfs-core/src/batch.rs`): `trash`/`restore`/
  `delete`/`move` report one result per node, so a node the server rejects no longer fails
  the whole batch.
- Photo viewer page in the GUI and richer photo metadata in the details pane.

### Changed
- SDK telemetry routed into `tracing`, so its transfer/block-storage/request spans land in
  the daemon log instead of a no-op sink.
- A file that fits in one block uploads as a single atomic request rather than the
  draft/block/commit sequence.

## [1.3.0] — 2026-07-31

Schema: 19.

### Added
- **Photo albums** — `PhotoAlbums` / `AlbumPhotos` control requests, daemon-side `albums.rs`
  and a GUI albums page (the former gallery view, renamed). Albums other people share with
  you are included; their photos are persisted per album because a shared album's photos
  never appear in your own timeline. Served stale-while-refreshing like the timeline.
- **GUI "Locations" page** — one row per mount spec: the primary `~/ProtonDrive` mount
  alongside every folder this computer backs up, mirrored or on-demand. Called *Locations*
  rather than *Mounts* because a mirror folder is a plain directory with no FUSE session.
  It absorbs the old Computers page's actions and the mountpoint chooser from Settings.
- `role` on shared-node metadata (`viewer`/`editor`/`admin`), defaulted for wire-compat with
  older clients and daemons.
- FUSE acceptance coverage for the permission-based mounts.

## [1.2.1] — 2026-07-31

Schema: 18.

### Fixed
- Migration bug that could leave the `ProtonDrive` mount unwritable after upgrade.
- Permission error on on-demand mounts.
- B78.

### Changed
- Release pipeline: `.deb` signing added and `.rpm` signing fixed.

## [1.2.0] — 2026-07-29

Sharing and multi-mount release. Schema: 18.

### Added
- **Shared with me**: shares others sent you are listed, verified and mountable, including on
  the primary mount.
  They appear under a synthetic `Shared with me` directory on a virtual volume
  (`pdfs-fuse/src/virtual.rs`), with name collisions and over-long components resolved by
  its own naming rules.
- Per-share access model (`db/share_access.rs`): the effective role of each shared root is
  persisted and kept resident in mount state, so interning an inode never costs a query. The
  database stays the restart/offline authority — every role change is written through before
  the cache updates. A share you cannot write is presented POSIX read-only.
- Unified location model (`pdfs-core/src/mounts.rs`, `db/mounts.rs`): one presentation shape
  for every local Proton Drive location, joining device sync state with a new `mount` table.
  `MountMode` is `Mirror` (full local copy, reconciled both ways) or `OnDemand` (FUSE session
  fetching on access), with an `Unknown` fallback so a mode written by a newer client does
  not break an older one.

## [1.1.1] — 2026-07-27

Schema: 16.

### Fixed
- Trash refresh did not pull updates.
- Packaging: `PKGBUILD` was left at 1.1.0 by the 1.1.1 version bump, failing the release
  workflow's tag/manifest/PKGBUILD agreement check.

## [1.1.0] — 2026-07-26

Conflict-handling and concurrency release. Schema: 16.

### Added
- Conflict sweep (`pdfs-fuse/src/sweep.rs`, 5-minute cadence): removes conflict copies proven
  identical by size + `content_sha1`, surfaces divergent ones once as an activity entry, and
  never removes what it cannot prove.
- Sweep mode and `.pdfsignore`-driven conflict handling; ignore rules are rebuilt per sync
  pass so edits take effect without a restart.
- Substantially extended FUSE acceptance suite.

### Fixed
- **B69** — spurious `(sync-conflict)` copies. Revision identity now uses the server's
  `revision_id` rather than `(mtime, size)`, which the server re-stamps on seal; the planner
  gained `AdoptBaseline` so an equal-size no-baseline pair is recorded instead of forked on
  first reconcile.
- **B70** — browser downloads into an on-demand mount had their in-flight temporaries sealed
  as canonical revisions. Transient names are recognised and held until the name is finalized.
- Assorted race conditions and concurrency bugs on the write/drain paths.

### Changed
- Workspace deps bumped to `proton-sdk` / `proton-drive-rs` 0.2.2.

## [1.0.0] — 2026-07-22

First stable release: FUSE files-on-demand mount, sync daemon under `proton-drive.service`,
`pdfs` CLI and the GTK4 app, on top of the pure-Rust Proton Drive SDK. Schema: 16.

### Fixed
- The outstanding FUSE defects tracked in `docs/BUGS.md`, plus a truncate defect, validated
  by a new POSIX compliance suite for the filesystem.

[1.9.1]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.9.1
[1.9.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.9.0
[1.8.2]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.8.2
[1.8.1]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.8.1
[1.8.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.8.0
[1.7.1]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.7.1
[1.7.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.7.0
[1.6.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.6.0
[1.5.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.5.0
[1.4.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.4.0
[1.3.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.3.0
[1.2.1]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.2.1
[1.2.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.2.0
[1.1.1]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.1.1
[1.1.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.1.0
[1.0.0]: https://github.com/narrrl/proton-drive-linux/releases/tag/v1.0.0
