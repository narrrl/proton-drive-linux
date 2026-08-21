# Contributing

Thanks for taking the time. This is an unofficial Proton Drive client that people trust with real
files, so the bar for changes that touch the filesystem, the sync engine, or the database is
deliberately high — everything else is ordinary Rust review.

## Before you start

- Read [`AGENTS.md`](AGENTS.md) for repository conventions and
  [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for how the daemon, front ends, and SDK fit
  together.
- Check [`docs/BUGS.md`](docs/BUGS.md) — it is the authoritative issue ledger, and your problem may
  already be tracked there with context.
- For anything larger than a fix, open an issue first so the design can be discussed before you
  spend the time.

## Development setup

```bash
git clone https://github.com/narrrl/proton-drive-linux.git
cd proton-drive-linux
cargo build --workspace
```

GUI and FUSE targets need the GTK4, libadwaita, WebKitGTK 6, libsecret, D-Bus, and FUSE3
development packages listed in the [README](README.md#prerequisites). Minimum supported Rust
version is 1.96.

## Quality gates

CI runs exactly these, and they must pass with zero warnings:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
scripts/fuse-acceptance.sh --offline-only
```

Changes to the FUSE layer, sync, or the pending-operation queue should also be exercised against a
real mount before review — see [`docs/TESTING.md`](docs/TESTING.md). Live runs are destructive:
use a dedicated test account, never one holding data you cannot lose.

## Making changes

- **Keep shared logic in `pdfs-core`.** The CLI and GUI never touch the database or the network;
  they speak the control protocol to the daemon.
- **The SQLite schema is forward-only.** Never edit a shipped migration — add a new one and bump
  `SCHEMA_VERSION` in `crates/pdfs-core/src/db/migrations.rs`.
- **Typed errors and `?` over panics** on runtime paths. Blocking filesystem, keyring, or
  control-socket work stays off GTK's main thread.
- **`staging/` and `recovery/` may hold the only copy of user data.** Nothing may clear or purge
  them opportunistically.

## Commits and pull requests

Commits follow scoped [Conventional Commits](https://www.conventionalcommits.org), e.g.
`fix(fuse,core): keep truncate composable with queued revisions`.

A pull request should state:

- the user-visible effect and which crates it touches,
- the `docs/BUGS.md` entries it fixes or affects,
- the commands you ran to verify it,
- screenshots for GTK changes,
- and, explicitly, any migration, recovery, data-loss, or packaging implications.

## Security

Do not open a public issue for a vulnerability — see [SECURITY.md](SECURITY.md).
