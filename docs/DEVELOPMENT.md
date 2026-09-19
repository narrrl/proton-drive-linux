# Development and Verification Status for 1.0

This document summarizes the work still pending after the 1.0.0 release branch and records the most important recently completed changes. [`BUGS.md`](BUGS.md) is the authoritative issue ledger; [`TESTING.md`](TESTING.md) defines the acceptance commands.

---

## 1. Core Feature Roadmap (Horizon Tasks)
Consolidated from the historical feature roadmap; current bug status lives in [`BUGS.md`](BUGS.md).

* **Local Cache & Metadata Encryption at Rest (Priority 5 / Horizon 1)**: Encrypt the SQLite database `cache.db` on disk using SQLCipher (`rusqlite` feature `bundled-sqlcipher`), and encrypt raw content cache blocks (`content/blocks/` and `content/scratch/`) using a fast symmetric scheme like AES-GCM or ChaCha20-Poly1305. The encryption key should be derived from the OS Keyring. (The SDK entity cache in `sdk_cache.db` is already encrypted at rest — see *Persistent SDK Entity Cache* below — but `cache.db` and the content cache are not.)
* **Active Bandwidth Throttling & Traffic Shaping (Horizon 1)**: Add speed governors for daemon uploads and downloads. Support configuration variables `max_upload_rate_kbps` and `max_download_rate_kbps` in `config.json` that can be dynamically adjusted over the Unix control socket.
* **Interactive & Policy-Based Conflict Resolution (Horizon 1)**: Implement configurable conflict resolution policies (`rename-local`, `rename-remote`, `prefer-local`, `prefer-remote`, `interactive`). Add a "Conflicts" tab to the GUI to notify the user, block sync, and launch external visual diff tools (like Meld or KDiff3).
* **Multi-Account / Multi-Profile Support (Horizon 3)**: Enable running multiple profiles concurrently. Support separate configuration namespaces (e.g. `profiles/<profile_name>/`), independent SQLite databases, distinct mount paths (e.g. `~/ProtonDrive/Personal` and `~/ProtonDrive/Work`), and multiple daemon processes.
* **Custom FUSE Mount Options (Horizon 1)**: Expose custom mounting parameters in configuration (e.g. `ro` for read-only, `direct_io` to bypass kernel caching, `allow_other`). Auto-generate systemd user mount units upon sync folder configuration.
* **Symbolic Link & Hard Link Virtualization (Horizon 2)**: Add symbolic link virtualization inside the metadata store (`NodeType::Symlink`) by intercepting `readlink(2)` and `symlink(2)`. Targets should sync remotely as small encrypted metadata placeholder files on Drive (`proton-vfs-symlink: <target>`).
* **Desktop File Manager Integration (Horizon 2)**: Develop shell extension plugins for popular Linux file managers (Nautilus Python plugin, Dolphin Service Menus, Thunar custom actions) to offer context-menu shortcuts ("Pin", "Unpin", "Copy Share Link", "View Version History") and file overlay sync badges.
* **Integrated Photo Gallery Enhancements (Horizon 2)**: Album *editing* — rename, cover photo, delete, and removing photos from an album — is still unported in the SDK (TS `updateAlbum` / `deleteAlbum` / `removePhotos`), as is adding a photo that lives on someone else's volume (TS `copyPhoto`). Album **creation** and adding own-volume photos landed with the Google Photos import. Still open: an auto-upload Pictures folder pipeline, and a GUI for creating albums by hand. Exif display, the date-based timeline scrubber and favourites are implemented.
* **Google Photos import follow-ups**: the importer uploads photos one at a time — a large export would finish sooner with a bounded concurrent upload pool. Trash folders are recognized by a hardcoded list of localized names ([`takeout.rs`](../crates/pdfs-core/src/takeout.rs) `TRASH_FOLDERS`); a locale not on it imports its trash as ordinary photos. Motion-photo `.MP`/`.MV` sidecar videos are ignored rather than uploaded as related photos. Live validation against a real export is pending.
* **Random-Access Media Streaming seek support (Horizon 2)**: Intercept out-of-order sparse reads (player seeks) in `pdfs-fuse` and prioritize downloading blocks around the seek offset, aborting/postponing sequential pre-fetches.
* **Dynamic Sync Dashboard & Queue Visualization (Horizon 2)**: GUI real-time transfer list (progress bars, speed, ETA), sync history feed, storage usage breakdown, and global pause/resume sync controls.
* **Interactive Terminal UI (TUI) Mode (Horizon 3)**: Implement `pdfs tui` using the `ratatui` crate to show transfer speed, sync status, active transfer queue with progress bars, and scrolling daemon logs.
* **Shutdown Safety & Write Queue Visibility (Horizon 1 / P4)**: Register systemd inhibitor locks in `pdfs-fuse` when there are outstanding staging writes to prevent shutdown or sleep, pop up warnings when exiting the GUI, and build a blocking `pdfs sync flush` command.
* **Block-Level Deduplication (Horizon 3)**: Transition to a Content-Addressable block cache where cached blocks are stored on disk by their SHA-256 hash. Map logical ranges in the database: `(node_uid, block_idx) -> block_hash` to avoid storing or downloading identical blocks.
* **Pre-emptive Sync Debouncing & File System Events (Horizon 3)**: Group filesystem write events on paths and delay sync queue insertion until a quiet period has elapsed (e.g. 5 seconds of inactivity) to prevent thrashing the Proton API.

---

## 2. Performance, Correctness & Robustness Items
Consolidated from the performance and media-streaming audit notes.

* **B6. Debounce the LRU access touch**: Keep last-touch times in memory and flush them in batches (debounce window of 30-60 seconds) to avoid performing a SQLite `UPDATE` to the `cache_entries` table on every cache hit.
* **E12. Client in-flight cap shape**: Address the lock discrepancy between `MAX_CONCURRENT_BLOCK_DOWNLOADS = 10` (per-file) and `DEFAULT_MAX_INFLIGHT_BLOCKS = 12` (global client-wide). Either raise the global cap, lower the per-file window, or dynamically calculate the window to prevent single files from starving concurrent transfers.
* **Pipeline `enumerate_nodes_detail` (P2.8 / 8)**: Introduce pipelined node enumeration to speed up directory listings.
* **Local ancestor-chain walk (P3.4 / 4)**: Optimize recursive path lookups where full batching is impossible.
* **Name Search for Photos/Videos**: Add search by photo/video name (deferred from Phase 3 gallery implementation).
* **Sequential-Read Prefetch for Media Streaming**: Implement sequential read prefetch for media files to avoid stalls when buffering.
* **Minor sweep items (Phase F)**:
  * **F1. `StreamRing.tags` Leak**: Clean up tag entries in `StreamRing.tags` when blocks are evicted. Currently, it only shrinks on `drop_node` (revision mismatch), leading to tag accumulation.
  * **F2. Non-saturating bytes subtraction**: Use `saturating_sub` in `lib.rs:382` (`self.bytes -= dropped.len()`) to prevent release mode panics/wrapping if accounting drifts.
  * **F3. Short block yields a short read / EOF**: Address the kernel EOF interpretation on short block reads. Either pad the block or fail loudly rather than silently serving short data.
  * **F4. `stream_readahead` check-then-spawn race**: Prevent duplicate concurrent readahead tasks for the same block. Check-then-spawn is currently racey.
  * **F5. `refresh_blocks` permit lock (SDK)**: Ensure URL refreshing (`refresh_blocks`) does not hold an in-flight block permit, preventing expired URLs from pinning client permits.
  * **F7. Document temporary cache file safety**: Add explanatory comment to `cache.rs:362` explaining why `with_extension("tmp")` is safe there but not in `store_thumbnail`.

---

## 3. Open & Unverified Bugs
Derived from: [`BUGS.md`](BUGS.md)

* **B2. Unattributed Trash Origin**: Investigate why deleted/moved files are occasionally sent to trash instead of vanishing on older rename operations.
* **B10. GLib Critical Warning on HUD Close**: Assertion failure `g_list_store_remove: assertion '!g_sequence_iter_is_end (it)' failed` when prompt window closes during active launch (`xdg-open`). Needs investigation under `G_DEBUG=fatal-criticals`.
* **B15. Empty-but-listed folders and duplicate UIDs**: Investigate folder-identity issues (e.g. duplicate remote folders like `Music` with different UIDs) and how folders obtain `listed = 1` in the database without matching child rows.
* **B8 (CAPTCHA sign-in bridge)**: Verify a real gated sign-in using Webkit CAPTCHA completion.
* **Draft Revision Upload loop**: Verify SDK-side deletion and retry logic when a conflicting draft revision exists under the same client UID.
* **Optimistic Size Loss on Restart**: Verify that pending uploads retain their optimistic sizes across a daemon restart.
* **B48 — stale listing after unlink**: Session tombstones and authoritative empty listings are implemented; run focused live verification against eventually consistent remote enumeration.
* **B49–B55 — data-safety fixes**: Conflict preservation, orphan/staging durability, transactional supersession, one-entry wipe protection, and incomplete-scan handling have regression coverage but still require the fault, power-loss, and live mode-matrix cases recorded in `BUGS.md`.
* **B56–B60 — fail-closed state and IPC fixes**: Future schemas, event cursor advancement, socket limits, private-directory validation, and atomic config saves are implemented. Malformed-schema, database-full, connection-flood, wrong-owner, and concurrent/faulted-save verification remains.
* **B61 — credential rotation persistence**: Surface and retry keyring persistence failures so a rotated single-use refresh token cannot be lost on restart.
* **B62–B65 — release assurance**: Clean-install package verification, complete version/protocol identity, recovery sign-off, advisory/license policy, SBOM, provenance, checksums, and signatures remain.
* **B66–B67 — lifecycle and recovery**: Add queue flush/shutdown protection and explicit device adoption during fresh-state restore.
* **B68/B78 — profile backup**: the destination is fixed and verified live — `profile.json` now lives in `.proton-drive-linux/` under the device root, since the API refuses file creation at the root itself. What remains from B68 is making backup *health* visible (today a failure is a `WARN` in the log and nothing in the UI) and re-running the replacement-machine restore end to end.
* **B79 — cross-mount permission classification**: fixed and verified live; the standing guard is the `regression B79` acceptance case. The second-account cases it neighbours (`regression B34`) still cannot run — see below.

---

## 4. Testing, Verification & Disaster Recovery Drills
Derived from: [`TESTING.md`](TESTING.md) and [`RECOVERY.md`](RECOVERY.md)

* **IPC Memory and Performance Stress Tests (Phase 3)**: Verify memory allocations do not balloon when sending large `UploadPhoto` JSON payloads. Test control socket timeout behaviors under long-running block operations.
* **Fault Injection and Resiliency Tests (Phase 4)**:
  * **Network degradation**: Verify that interrupted uploads leave readable staging buffers and result in clean backoff retries.
  * **Disk full (ENOSPC)**: Verify that write staging and gap-filling report correct errors to FUSE and preserve staged work.
  * **Concurrent reader starvation**: Test that parallel sequential reads do not freeze directory lookups or metadata handlers.
* **Disaster Recovery Drill (P5 / Phase 5)**: Execute a complete manual disaster recovery test:
  1. Register a device.
  2. Sync a folder and upload data.
  3. Wipe the local state directory and SQLite cache.
  4. Restart the daemon.
  5. Run `pdfs sync restore` and assert the recovered filesystem matches the source byte-for-byte.
* **Second-account sharing run (mount-architecture.md P7)**: The only outstanding verification for the mount/sharing work. One account covers `context_share_id`, the shared-with-me listing, sharing from device folders and the Locations page — all confirmed. A **second account** is required for the shared-tree rendering under a foreign volume, `accept_invitation`, `update_member_role`, the `regression B34` viewer cases (which skip cleanly today), and above all an **editor-role write into a foreign share**, the only thing that validates `membership_address_for`.
* **Managed live mode matrix**: Before release, run `scripts/fuse-acceptance.sh --managed-live <empty-a> <empty-b>` with `PDFS_ACCEPTANCE_ONLY` unset. It validates on-demand/on-demand, on-demand/mirror, mirror/on-demand, and mirror/mirror transitions.
* **Crash-consistency matrix**: Inject failure after scratch/sidecar sync, each staging rename, directory sync, pending-op insert/commit, and source deletion; prove that a complete discoverable copy and either the old or new queue record always survives.

---

## 5. Fixed & Resolved Bugs
Derived from: [`BUGS.md`](BUGS.md)

### Completely Fixed & Verified
* **B1 — FUSE rename loses the file (data loss)**: In `rename`, the node's database row was deleted while only updating the in-memory state, causing the file to vanish on next listing sync. Fixed by correctly calling `st.invalidate_listing(newparent)` to refresh SQLite cache states.
* **B4 — `invalidate_listing` silently skipped non-resident folders**: Folders not loaded in memory were being skipped during listing invalidation, leaving stale cache flags in the DB. Fixed by removing the residency check guard.
* **B5 — `ls -l` costs a network round trip per file (thumbnail xattr probes)**: File listings triggered blocking network checks for thumbnails on unsupported files. Fixed by caching negative thumbnail results and restricting xattr advertisement to supported image/media files.
* **B6 — Daemon sets no file modes: `control.sock` is an unguarded authority**: The Unix control socket was created without restricting file permissions, exposing daemon controls to other local users. Fixed by applying proper creation permission masks.
* **B7 — renamed directory reads as missing until the entry TTL expires**: Kernel cache maps remained stale after a FUSE directory re-anchor. Fixed by proactively sending entry invalidation notifications to the kernel.
* **B9 — Enter did not open the selected result in the launcher**: Hooked up keyboard execution (Enter key) in `pdfs-prompt` search HUD to launch the selected item using `xdg-open`.
* **B11 — a file moved while its create is still queued reads as empty**: Reconciling a local rename on a file that had not finished uploading yet uploaded empty blocks. Fixed by carrying over and staging the correct logical file size.
* **B12 — cold enumeration is slow per entry, and goes superlinear past ~500**: S2K decryption and thumbnail resolution loops were serial. Fixed by parallelizing key derivations and caching metadata.
* **B13 — `rename` over an existing destination fails instead of replacing it**: Fixed POSIX rename target overwrite behavior to replace existing destinations.
* **B14 — provisional (ciphertext) sizes make rsync read short and abort the file**: Adjusted size reporting to align logical and ciphertext sizing, preventing premature EOF.
* **B45 — truncate over a queued rewrite**: Path-based and handle-based truncate now compose over the complete pending blob; shrink, growth, and sparse I/O are live-verified.
* **B46 — combined cross-directory move and rename**: The desired end state is durably queued and reconciles partially completed remote operations; replacement and combined cases are live-verified.
* **B47 — overlong/invalid names**: All name-taking callbacks share a validator for `NAME_MAX`, reserved components, and UTF-8; managed-live verification passed.

### Implemented in 1.0 (Further Fault Verification Pending)

* **Unified search and resident prompt**: `SearchV2` combines Drive and local lookup with shared relevance scoring and content/source filters. The prompt reuses its window and opens streamable media through FUSE.
* **Durable write publication**: Scratch sidecars and staged blobs use synced temporary files, atomic renames, and directory synchronization; orphan rescue retains its source on failure.
* **Transactional pending operations**: Superseding a queued operation cannot delete the old durable record unless the replacement commits.
* **Safer mirror reconciliation**: Conflict-copy failure aborts publication, incomplete scans are non-destructive, and total-wipe protection includes one-entry baselines.
* **Bounded IPC**: Control requests are limited to 1 MiB and 10 seconds, with at most 64 active handlers.
* **Private local state**: Sensitive directories require correct ownership, real-directory identity, and `0700`; sockets use `0600`; config saves are atomic and malformed input is preserved.
* **Release gates**: Tag/workspace/package versions are checked and formatting, lint, tests, and offline FUSE acceptance run before packaging.

### Code Fixed / Implemented (Pending Verification)
* **B8 — no way to complete a CAPTCHA, so a gated sign-in is unrecoverable**: Implemented a Webkit-based bridge to prompt the user to resolve interactive CAPTCHAs during gated logins. (Needs verification on a real gate request).

### Closed / Not a Bug
* **B3 — Activity log timestamps written in seconds, read as milliseconds**: Closed as investigator error (timestamps are consistent).

---

## 6. Recently Implemented Features

### File Version History (Implemented)
**Files**: [`revisions.rs`](../crates/pdfs-fuse/src/revisions.rs), [`control.rs`](../crates/pdfs-core/src/control.rs), [`versions_dialog.rs`](../crates/pdfs-gui/src/app/widgets/versions_dialog.rs)

Proton Drive keeps every revision a client committed; the daemon only ever addressed the active one, so a file overwritten by a sync pass could be recovered only from whatever the local `recovery/` directory happened to hold.

The control protocol gained `ListRevisions` / `RestoreRevision` / `DeleteRevision` / `SaveRevisionAs` (each with a `…ByUid` twin for nodes the primary mount cannot name), the CLI gained `pdfs versions list|restore|save|rm`, and the browser's details pane gained a **Versions** button opening a per-file dialog.

Three properties that shape the code:

- **A restore is server-side and asynchronous.** No content crosses the wire and nothing enters the drain queue; the server answers 202 and swaps the active revision in the background, so the daemon evicts the file's cached blocks and open readers rather than describing the new state, and the UI wording promises the request, not the result.
- **The active revision cannot be deleted.** `Core::delete_revision_for_uid` checks that locally so the user gets a sentence rather than an API code, and the dialog gives the current row no delete button.
- **Saving a version never overwrites.** `SaveRevisionAs` refuses an existing destination and removes a partial file if the download fails — a half-written export looks identical to a good one to every tool that opens it afterwards.

### Photo Favourites (Implemented)
**Files**: [`photos.rs`](../crates/pdfs-fuse/src/photos.rs), [`photo_viewer.rs`](../crates/pdfs-gui/src/app/pages/photo_viewer.rs)

The gallery can mark and filter favourites, through the SDK's photo-tag API (`update_photos`). Schema **v20** adds `photos.favorite`; the flag is learned in the timeline enrichment pass that already resolves each photo's name and media type, and is kept across a refresh that could not resolve a photo — the same learned-and-kept rule as `media_type`. The lightbox carries a star toggle, the Photos header a favourites filter, and the CLI has `pdfs favorite <uid> [--remove]` plus `pdfs photos --favorites`.

Favouriting a photo that is not on this account's own photos volume (shared with us, or album-only) needs it re-encrypted for our timeline root, which the SDK does not implement; the daemon surfaces that as an error rather than silently doing nothing.

### Persistent SDK Entity Cache (Implemented)
**Files**: [`sdkcache.rs`](../crates/pdfs-core/src/sdkcache.rs), [`auth.rs`](../crates/pdfs-core/src/auth.rs), [`background.rs`](../crates/pdfs-fuse/src/background.rs)

The SDK's Drive entity cache (decrypted node metadata: name, size, parent, signing share) defaulted to memory, so every daemon restart re-fetched and re-decrypted the tree the previous run had already walked. It is now backed by SQLite.

- **Its own file** (`sdk_cache.db` in the state directory), not `cache.db`: the daemon's `Db` is one `Mutex<Connection>` shared by every FUSE thread and the control socket, and this traffic is frequent, small and entirely reconstructible. A separate file also means no schema migration — the store can be deleted at any time.
- **Encrypted at rest** by wrapping it in the SDK's `EncryptedCacheRepository`, keyed by the mailbox password. A password change reads as a cold cache (the SDK treats an undecryptable entry as a miss and clears the store), not as an error.
- **Staleness** is closed by the event loop, which already replays from a persisted cursor and calls `invalidate_caches_for_event` for every event — including those raised while the daemon was down. The one case with no trail is a *seeded* cursor (first-ever mount, or a lost cursor), where the seed path now clears the store instead of trusting it. `auth::logout` removes the file.

Needed a small additive SDK change (`ProtonDriveClient::with_entity_repository`, 0.5.1): `with_entity_cache` is a constructor and so could not be combined with `with_key_salts`, which the daemon requires.

### Per-node Batch Outcomes (Implemented)
**File**: [`batch.rs`](../crates/pdfs-core/src/batch.rs)

SDK 0.5.0 made `trash_nodes` / `restore_nodes` / `delete_nodes` report one outcome per node instead of failing the whole call (mirroring upstream's streamed `NodeActionResult`). `pdfs_core::batch::into_unit` collapses the single-node calls back to a `Result`; `batch::split` handles a collected batch.

A restore is expanded before it is sent (`expand_restore`, `pdfs-fuse/src/lib.rs`): the persisted trash listing carries each row's `parent_uid` (schema 29), so a restore takes the connected piece of the trashed tree — every trashed descendant of what was asked for, and every trashed ancestor above it — and sends it shallowest wave first, because the server cannot put a node back under a parent that is still trashed. A uid the listing does not know about (it is materialised in chunks, and can be stale) is restored on its own rather than dropped.

The trash view's restore and permanent-delete use the SDK's **streaming** variants (`restore_nodes_streaming` / `delete_nodes_streaming`) and apply local state per node as each batch lands rather than after the last one. For a permanent delete — which is irreversible — that means a daemon interrupted mid-batch has forgotten exactly the nodes the server destroyed, no more and no fewer. Both report per-node failures and count only what actually succeeded; only a batch where nothing succeeded is an error.


### Open-for-Write Deferral for Mirror Sync (Implemented)
**File**: [`sync.rs`](../crates/pdfs-fuse/src/sync.rs)

The mirror folder sync engine now defers uploading any file that is currently held open for writing by another process, matching the guarantee the FUSE mount path already provides (where uploads are deferred until `close(fd)`).

**Problem**: Previously, the mirror sync path relied solely on a 2-second trailing-edge debounce (with a 30-second ceiling) to avoid uploading files mid-write. This was insufficient for slow continuous writers (e.g., database dumps, large exports, or editors that keep files open for extended periods), which could have their incomplete state uploaded as a real revision.

**Solution**: Before each reconcile pass, the sync engine scans `/proc/*/fd` once to build a set of canonical paths within the sync root that any process holds open for writing (`O_WRONLY` or `O_RDWR`). Files in this set are:

- **Kept in the local walk** — so they are not misclassified as deletions
- **Treated as unchanged** — so no upload is queued for them
- **Counted as `deferred`** — so the activity summary reports them (e.g., "3 uploaded, 1 deferred (open for write)")
- **Picked up on the next pass** — after the writer has closed the file

**How it works**:
1. `open_for_write_set(root)` reads `/proc/*/fd` → `readlink` for each fd → prefix-checks against the sync root → reads `/proc/<pid>/fdinfo/<n>` to check the `flags:` line's low two bits (O_ACCMODE)
2. `walk_local` sets `LocalItem.open_for_write = true` for matching files
3. Both `do_reconcile` and `push_pass` skip upload classification for flagged files
4. `Outcome.deferred` tracks the count for the activity summary

**Performance**: The `/proc` scan completes in low single-digit milliseconds on a typical desktop. Only fds whose `readlink` target falls under the sync root incur the `fdinfo` read.

**Tests added**: 8 unit tests covering `is_write_mode` parsing of various fdinfo flag combinations, and `Outcome` summary formatting with deferred counts.

| Sync Path | Open-for-write protection | Mechanism |
|---|---|---|
| FUSE mount | ✅ Perfect | Upload deferred until `close(fd)` |
| Mirror folders | ✅ Perfect | `/proc/*/fd` scan per reconcile pass |
