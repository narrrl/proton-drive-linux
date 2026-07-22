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
`sendfile`, durability barriers, namespace/error semantics, open-file lifetime,
names and enumeration, and concurrent I/O. It never reads application state or
contacts Proton:

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
