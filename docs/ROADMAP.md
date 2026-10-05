# Roadmap

Planned work and open verification, as of 3.0.0. Individual defects and their status live in
[BUGS.md](BUGS.md), which is the authoritative ledger; this page groups what is left by theme.
Nothing here is a promise of a date.

- [Sync engine](#sync-engine)
- [Data safety and recovery](#data-safety-and-recovery)
- [Features](#features)
- [Performance and correctness](#performance-and-correctness)
- [Verification still owed](#verification-still-owed)
- [Release assurance](#release-assurance)

## Sync engine

3.0.0 records changes made through the mount locally and sends them from the queue
([MILESTONE-3.0.0.md](MILESTONE-3.0.0.md)). What comes after it:

| Item | State | Notes |
|---|---|---|
| Remove the online path | Not started | The old way of sending a change before the syscall returns, and the `local_first` switch that brings it back, go in the release after 3.0.0 |
| One applier for remote changes | Not started | Phase 4 of the milestone: a remote tree, and remote changes applied through the same planner as local ones |
| Synced folders on the queue | Not started | Phase 5: synced folders sent by the same planner and executor as the mount |
| Batch trashes and moves | Deferred | See below |

**Batching trashes and moves.** `trash_nodes` and `move_nodes_streaming` take up to 150 nodes per
call, but the queue still sends one node per call. It was left out of 3.0.0 on purpose:

- The speed target of the milestone (§7) is met without it. Making 1,000 files in 100 folders
  takes 5.2 s on a LAN and 5.0 s on Wi-Fi.
- What the queue spends its time on is the creates. Each one waits for Drive to list the new node
  before it counts as landed (250 ms, then 1 s), and batching does not help with that.
- Sending many ops in one call means splitting one answer back into many results. Races in
  exactly that step were the largest class of bugs found for 3.0.0 (B15x to B17x).

Before building it, measure `rm -r` of a folder with 1,000 files on Wi-Fi. If that is slow, the
cheaper fix is to drop the queued trashes of the files in a folder when the folder itself is
trashed, since trashing the folder takes its contents with it.

## Data safety and recovery

| Item | State | Notes |
|---|---|---|
| Protect queued changes at shutdown | Not started | A systemd inhibitor lock while `staging/` holds unsent data, a warning when signing out, and a blocking `pdfs sync flush`. Today the queue is visible through `pdfs sync queue`, the tray and **Sync → Overview**, but nothing stops a shutdown (B66) |
| Restore pins from the machine profile | Not started | `profile.json` records pins; `sync restore` does not apply them yet |
| Profile backup health | Not started | A failed profile upload is a `WARN` in the log and nothing in the UI (B68) |
| Rotated credentials survive a keyring failure | Open | Surface and retry keyring write failures so a rotated single-use refresh token is not lost (B61) |
| Encrypt local state at rest | Not started | SQLCipher for `cache.db` and an AEAD for the content cache, keyed from the keyring. `sdk_cache.db` is already encrypted. See [ARCHITECTURE.md §8](ARCHITECTURE.md#8-threat-model-what-this-client-writes-to-disk-in-plaintext) |

## Features

| Item | State | Notes |
|---|---|---|
| File-manager integration | Not started | Context-menu actions (available offline, share link, versions) and sync badges for Nautilus, Dolphin and Thunar |
| Terminal UI | Not started | `pdfs tui` with transfers, sync state and logs |
| Multiple accounts | Not started | Separate config, state and mountpoint per profile, one daemon each |
| Mount options | Not started | Read-only mount, `allow_other` |
| Symbolic links | Not started | Proton Drive has no link type; links would need a placeholder format. Until then, creating one fails with `EOPNOTSUPP` |
| Add someone else's photo to your album | Blocked | Needs `copyPhoto` in the SDK |
| Photo name search | Not started | Photos search by date and filters only |
| Automatic photo upload from a folder | Not started | |
| Faster Google Photos import | Not started | Uploads run one at a time. Trash folders of locales missing from `TRASH_FOLDERS` import as photos; motion-photo `.MP` parts are skipped |
| Content-addressed block cache | Idea | Store blocks by hash to deduplicate identical content |

## Performance and correctness

| Item | Notes |
|---|---|
| Saturating stream-ring accounting | `self.bytes -= …` in `crates/pdfs-fuse/src/reads.rs` should saturate so an accounting drift cannot wrap in release builds |
| Block geometry learned one read late | The first read of a file plans on 4 MiB blocks; accepted as a bounded cost (B87) |
| Path-based commands under secondary mounts | `pin`, `rename`, `move`, `rm` and sharing cannot name a path inside an on-demand synced folder yet (B88) |
| Unicode normalization | NFC and NFD names are distinct, deliberately (B36) |
| Pipelined node enumeration | Faster cold listings of large folders |

## Verification still owed

Fixes marked *unverified* in [BUGS.md](BUGS.md) have unit or offline coverage but have not been
driven against a live account. The larger outstanding runs:

- **Disaster-recovery drill.** Register a device, sync a folder, delete all local state, restore
  with `pdfs sync restore` and compare the tree byte for byte. See [RECOVERY.md](RECOVERY.md).
- **Second-account sharing.** Shared trees on a foreign volume, accepting invitations, changing
  roles, the viewer cases of `regression B34`, and an editor writing into someone else's share.
- **Fault injection.** Network loss mid-upload, a full disk during staging, and crash points
  across the write publication path (after each fsync, rename and queue commit).
- **CAPTCHA sign-in** against a real verification request (B8).
- **Managed live mode matrix** before each release:
  `scripts/fuse-acceptance.sh --managed-live <empty-a> <empty-b>`. See [TESTING.md](TESTING.md).
- **The overnight hang** (B91) is not diagnosed yet. The supervisor and `pdfs diagnostics` exist to
  catch the next occurrence.

## Release assurance

| Item | State |
|---|---|
| Tag, workspace and PKGBUILD version agreement | Enforced by the release workflow |
| Clean-install test of the `.deb` and `.rpm` in a VM | Pending (B62) |
| Protocol version identity between front ends and daemon | Open (B63) |
| SBOM, provenance and checksums for release artifacts | Open (B65). The RPM is GPG-signed |
