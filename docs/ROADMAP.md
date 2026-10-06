# Roadmap

Planned work and open verification after 3.0.0. Individual defects and their status are in
[BUGS.md](BUGS.md); this page groups what is left by theme. Nothing here has a date.

- [Sync engine](#sync-engine)
- [Data safety and recovery](#data-safety-and-recovery)
- [Features](#features)
- [Performance and correctness](#performance-and-correctness)
- [Verification still owed](#verification-still-owed)
- [Release assurance](#release-assurance)

## Sync engine

3.0.0 records changes made through the mount locally and sends them from the queue
([ARCHITECTURE.md §4](ARCHITECTURE.md#4-write-path)). What comes next:

| Item | State | Notes |
|---|---|---|
| Remove the online path | Not started | The old way of sending a change inside the syscall, and the `local_first` switch that brings it back, go in the next release |
| One applier for remote changes | Not started | Everything that comes from Drive (events, listings) changes the tree in one place, which is also the only place that tells the kernel. Replaces today's invalidation helpers and in-memory overlays |
| Synced folders on the queue | Not started | Mirrored folders feed the local tree and use the same queue as the mount; `sync_entry` becomes the record of the last agreed state. Switching a folder's mode then moves no data except to evict or fetch content |
| Batch trashes and moves | Deferred | See below |
| Provisional values as types | Not started | A size or block geometry that is not known yet as its own type, so it cannot reach the kernel as if it were known |
| Deadlines on every remote call | Partly done | The drain's calls and calls a syscall waits on have one; background calls outside the drain do not yet |

**Batching trashes and moves.** `trash_nodes` and `move_nodes_streaming` take many nodes per call,
but the queue sends one node per call. It was left out of 3.0.0 on purpose:

- The 3.0.0 speed target is met without it. Making 1,000 files in 100 folders through the mount
  takes 5.2 s on a LAN profile and 5.0 s on a Wi-Fi profile in the simulation
  (`PDFS_SIM_MEASURE`, see [TESTING.md](TESTING.md#simulation-runs)).
- The drain spends its time on creates. Each waits for Drive to list the new node before it
  counts as landed, and batching does not shorten that.
- One call answering for many ops has to be split back into results per op. Races in exactly that
  step were the largest class of bugs found for 3.0.0 (B15x to B17x in [BUGS.md](BUGS.md)).

Before building it, measure `rm -r` of a folder with 1,000 files on Wi-Fi. If that is slow, the
cheaper fix is to drop the queued trashes of a trashed folder's contents: trashing the folder takes
them along.

## Data safety and recovery

| Item | State | Notes |
|---|---|---|
| Protect queued changes at shutdown | Not started | A systemd inhibitor lock while `staging/` holds unsent data, a warning when signing out, and a `pdfs sync flush` that waits for the queue. Today the queue is visible in `pdfs sync queue`, the tray and **Sync → Overview**, but nothing stops a shutdown (B66) |
| Restore pins from the machine profile | Not started | `profile.json` records pins; `pdfs sync restore` does not apply them yet |
| Profile backup health | Not started | A failed profile upload is a `WARN` in the log and nothing in the app (B68) |
| Rotated credentials survive a keyring failure | Open | Report and retry keyring write failures, so a rotated single-use refresh token is not lost (B61) |
| Restore never picks a new device silently | Open | Ask when the machine's device is ambiguous after a loss of local state (B67) |
| Encrypt local state at rest | Not started | SQLCipher for `cache.db` and an AEAD for the content cache, keyed from the keyring. `sdk_cache.db` is already encrypted. See [ARCHITECTURE.md §10](ARCHITECTURE.md#10-threat-model-what-this-client-writes-to-disk-in-plaintext) |

## Features

| Item | State | Notes |
|---|---|---|
| File-manager integration | Not started | Context-menu actions (available offline, share link, versions) and sync badges for Nautilus, Dolphin and Thunar |
| Terminal UI | Not started | `pdfs tui` with transfers, sync state and logs |
| Multiple accounts | Not started | Separate config, state and mountpoint per account, one daemon each |
| Mount options | Not started | Read-only mount, `allow_other` |
| Symbolic links | Not started | Proton Drive has no link type; links would need a placeholder format. Until then, `symlink` fails with `EPERM` |
| `fsync` that waits for Drive | Idea | `fsync` means "durable locally". A mount option or an xattr could offer "on Drive" without slowing the default |
| Add someone else's photo to your album | Blocked | Needs `copyPhoto` in the SDK |
| Photo name search | Not started | Photos search by date and filters only |
| Automatic photo upload from a folder | Not started | |
| Faster Google Photos import | Not started | Uploads run one at a time. Trash folders of locales missing from `TRASH_FOLDERS` import as photos; motion-photo `.MP` parts are skipped |
| Content-addressed block cache | Idea | Store blocks by hash to deduplicate identical content |

## Performance and correctness

| Item | Notes |
|---|---|
| Saturating stream-ring accounting | `self.bytes -= …` in `crates/pdfs-fuse/src/reads.rs` should saturate, so an accounting drift cannot wrap in release builds |
| Block geometry learned one read late | The first read of a file plans on 4 MiB blocks; accepted as a bounded cost (B87) |
| Path-based commands under online-only folders | `pin`, `unpin`, opening a file, uploads, sharing and versions cannot name a path inside an online-only synced folder yet. `rm`, `rename`, `mkdir` and `move` can (B88) |
| Unicode normalization | NFC and NFD names are distinct, on purpose (B36) |
| Pipelined node enumeration | Faster first listings of large folders |

## Verification still owed

Fixes marked *unverified* in [BUGS.md](BUGS.md) have unit or simulation coverage but have not
been driven against a live account. The larger runs still owed:

- **A clean account run on 3.0.0.** The last account run before the release (2026-10-04, on Wi-Fi)
  failed only its cleanup, on a script bug fixed since. No full run with `--journal-check` has
  passed on the released code yet.
- **Disaster-recovery drill.** Register a device, sync a folder, delete all local state, restore
  with `pdfs sync restore`, and compare the tree byte for byte. See [RECOVERY.md](RECOVERY.md).
- **Second-account sharing.** Shared folders on a foreign volume, accepting invitations, changing
  roles, the viewer cases of `regression B34`, and an editor writing into someone else's share.
- **Fault injection on a real account.** Network loss mid-upload, a full disk during staging, and
  crashes at each step of publishing a write. The simulation covers link loss and restarts against
  the fake Drive, not a kill at an arbitrary point.
- **Acceptance runs across a network drop.** A case that spans an outage times out while the queue
  replays, instead of being reported as interrupted.
- **CAPTCHA sign-in** against a real verification request (B8).
- **The overnight hang** (B91) is not diagnosed. The supervisor and `pdfs diagnostics` exist to
  catch the next one.

## Release assurance

| Item | State |
|---|---|
| Tag, workspace and PKGBUILD version agree | Enforced by the release workflow |
| Clean-install test of the `.deb` and `.rpm` in a VM | Pending (B62) |
| Protocol version identity between front ends and daemon | Open (B63) |
| SBOM, provenance and checksums for release artifacts | Open (B65). The RPM is GPG-signed |
