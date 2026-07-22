# Testing

The normal Rust suite exercises the metadata database, sync planner, offline
queue, write staging, cache, and FUSE handler state without requiring a Proton
account:

```console
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Live FUSE acceptance suite

Kernel behavior and remote convergence need a real mount. The live suite runs
destructive POSIX operations only below a fresh `pdfs-acceptance-*` directory.
It refuses paths that are not FUSE mounts and removes its directory on success,
failure, or interruption.

Use a dedicated test account without irreplaceable data:

```console
scripts/fuse-acceptance.sh /mnt/on-demand-testmount
```

Passing more paths runs the same filesystem suite independently against each.
The first path must be the FUSE mount under test; later paths may be another
FUSE mount or a normal local mirror filesystem:

```console
PDFS_ACCEPTANCE_SYNC_TIMEOUT=180 \
  scripts/fuse-acceptance.sh /mnt/on-demand-testmount /mnt/testmount
```

If two paths really are views of the **same remote folder**, enable an
additional byte-for-byte convergence check explicitly:

```console
PDFS_ACCEPTANCE_CONVERGENCE=1 \
  scripts/fuse-acceptance.sh /mnt/first-view /mnt/second-view
```

Do not enable that flag merely because two sync folders belong to the same
account. Their configured remote UIDs must be identical.

The suite covers create/read/rewrite/truncate, multi-megabyte binary content,
unaligned random writes, rename replacement, cross-directory moves, important
POSIX error cases, open-then-unlink behavior, concurrent writers, directory
enumeration, and optional cross-mount byte-for-byte convergence. A pass is
strong evidence for those paths, not a blanket data-loss guarantee. Before a
release, also run the recovery drills in `docs/RECOVERY.md` and inspect
`docs/BUGS.md` for live-verification items.

The runner accepts already-mounted paths instead of starting a daemon. This
keeps authentication and keyring setup explicit and prevents it from silently
reusing or replacing the normal application state directory.
