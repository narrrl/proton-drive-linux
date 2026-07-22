# Pending Development & Verification Tasks

This document compiles all pending, deferred, and unstarted tasks from the various roadmap, audit, and bug-tracking markdown files in the repository. Completed items have been filtered out.

---

## 1. Core Feature Roadmap (Horizon Tasks)
Derived from: [features.md](file:///home/narl/dev/private/proton-drive-linux/features.md) & [features-plan.md](file:///home/narl/dev/private/proton-drive-linux/features-plan.md)

* **Local Cache & Metadata Encryption at Rest (Priority 5 / Horizon 1)**: Encrypt the SQLite database `cache.db` on disk using SQLCipher (`rusqlite` feature `bundled-sqlcipher`), and encrypt raw content cache blocks (`content/blocks/` and `content/scratch/`) using a fast symmetric scheme like AES-GCM or ChaCha20-Poly1305. The encryption key should be derived from the OS Keyring.
* **Active Bandwidth Throttling & Traffic Shaping (Horizon 1)**: Add speed governors for daemon uploads and downloads. Support configuration variables `max_upload_rate_kbps` and `max_download_rate_kbps` in `config.json` that can be dynamically adjusted over the Unix control socket.
* **Interactive & Policy-Based Conflict Resolution (Horizon 1)**: Implement configurable conflict resolution policies (`rename-local`, `rename-remote`, `prefer-local`, `prefer-remote`, `interactive`). Add a "Conflicts" tab to the GUI to notify the user, block sync, and launch external visual diff tools (like Meld or KDiff3).
* **Multi-Account / Multi-Profile Support (Horizon 3)**: Enable running multiple profiles concurrently. Support separate configuration namespaces (e.g. `profiles/<profile_name>/`), independent SQLite databases, distinct mount paths (e.g. `~/ProtonDrive/Personal` and `~/ProtonDrive/Work`), and multiple daemon processes.
* **Custom FUSE Mount Options (Horizon 1)**: Expose custom mounting parameters in configuration (e.g. `ro` for read-only, `direct_io` to bypass kernel caching, `allow_other`). Auto-generate systemd user mount units upon sync folder configuration.
* **Symbolic Link & Hard Link Virtualization (Horizon 2)**: Add symbolic link virtualization inside the metadata store (`NodeType::Symlink`) by intercepting `readlink(2)` and `symlink(2)`. Targets should sync remotely as small encrypted metadata placeholder files on Drive (`proton-vfs-symlink: <target>`).
* **Desktop File Manager Integration (Horizon 2)**: Develop shell extension plugins for popular Linux file managers (Nautilus Python plugin, Dolphin Service Menus, Thunar custom actions) to offer context-menu shortcuts ("Pin", "Unpin", "Copy Share Link", "View Version History") and file overlay sync badges.
* **Integrated Photo Gallery Enhancements (Horizon 2)**: Extract and display Exif metadata (camera model, exposure, coordinates) from photos locally, support creating and editing Proton Photo Albums, build a date-based timeline scrollbar, and create an auto-upload Pictures folder pipeline.
* **Random-Access Media Streaming seek support (Horizon 2)**: Intercept out-of-order sparse reads (player seeks) in `pdfs-fuse` and prioritize downloading blocks around the seek offset, aborting/postponing sequential pre-fetches.
* **Dynamic Sync Dashboard & Queue Visualization (Horizon 2)**: GUI real-time transfer list (progress bars, speed, ETA), sync history feed, storage usage breakdown, and global pause/resume sync controls.
* **Interactive Terminal UI (TUI) Mode (Horizon 3)**: Implement `pdfs tui` using the `ratatui` crate to show transfer speed, sync status, active transfer queue with progress bars, and scrolling daemon logs.
* **Shutdown Safety & Write Queue Visibility (Horizon 1 / P4)**: Register systemd inhibitor locks in `pdfs-fuse` when there are outstanding staging writes to prevent shutdown or sleep, pop up warnings when exiting the GUI, and build a blocking `pdfs sync flush` command.
* **Block-Level Deduplication (Horizon 3)**: Transition to a Content-Addressable block cache where cached blocks are stored on disk by their SHA-256 hash. Map logical ranges in the database: `(node_uid, block_idx) -> block_hash` to avoid storing or downloading identical blocks.
* **Pre-emptive Sync Debouncing & File System Events (Horizon 3)**: Group filesystem write events on paths and delay sync queue insertion until a quiet period has elapsed (e.g. 5 seconds of inactivity) to prevent thrashing the Proton API.
* **File Version History (Horizon 2)**: Expose past file revisions and version history inside the GUI browser tabs.

---

## 2. Performance, Correctness & Robustness Items
Derived from: [plan.md](file:///home/narl/dev/private/proton-drive-linux/plan.md), [improvements.md](file:///home/narl/dev/private/proton-drive-linux/improvements.md), & [photos-video-streaming.md](file:///home/narl/dev/private/proton-drive-linux/photos-video-streaming.md)

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
  * **F6. Uncapped Unix control socket thread spawns**: Replace the unbounded `std::thread::spawn` per accepted socket connection in `run_control_socket` with a bounded pool or handler limits to prevent thread exhaustion.
  * **F7. Document temporary cache file safety**: Add explanatory comment to `cache.rs:362` explaining why `with_extension("tmp")` is safe there but not in `store_thumbnail`.

---

## 3. Open & Unverified Bugs
Derived from: [docs/bugs.md](file:///home/narl/dev/private/proton-drive-linux/docs/bugs.md)

* **B2. Unattributed Trash Origin**: Investigate why deleted/moved files are occasionally sent to trash instead of vanishing on older rename operations.
* **B10. GLib Critical Warning on HUD Close**: Assertion failure `g_list_store_remove: assertion '!g_sequence_iter_is_end (it)' failed` when prompt window closes during active launch (`xdg-open`). Needs investigation under `G_DEBUG=fatal-criticals`.
* **B15. Empty-but-listed folders and duplicate UIDs**: Investigate folder-identity issues (e.g. duplicate remote folders like `Music` with different UIDs) and how folders obtain `listed = 1` in the database without matching child rows.
* **B8 (CAPTCHA sign-in bridge)**: Verify a real gated sign-in using Webkit CAPTCHA completion.
* **Draft Revision Upload loop**: Verify SDK-side deletion and retry logic when a conflicting draft revision exists under the same client UID.
* **Optimistic Size Loss on Restart**: Verify that pending uploads retain their optimistic sizes across a daemon restart.

---

## 4. Testing, Verification & Disaster Recovery Drills
Derived from: [testing.md](file:///home/narl/dev/private/proton-drive-linux/testing.md) & [docs/RECOVERY.md](file:///home/narl/dev/private/proton-drive-linux/docs/RECOVERY.md)

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

---

## 5. Fixed & Resolved Bugs
Derived from: [docs/bugs.md](file:///home/narl/dev/private/proton-drive-linux/docs/bugs.md)

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

### Code Fixed / Implemented (Pending Verification)
* **B8 — no way to complete a CAPTCHA, so a gated sign-in is unrecoverable**: Implemented a Webkit-based bridge to prompt the user to resolve interactive CAPTCHAs during gated logins. (Needs verification on a real gate request).

### Closed / Not a Bug
* **B3 — Activity log timestamps written in seconds, read as milliseconds**: Closed as investigator error (timestamps are consistent).

---

## 6. Recently Implemented Features

### Open-for-Write Deferral for Mirror Sync (Implemented)
**File**: [`sync.rs`](file:///home/narl/dev/private/proton-drive-linux/crates/pdfs-fuse/src/sync.rs)

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

