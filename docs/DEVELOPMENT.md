# Development guide

How to build, run, debug and release the client from source. Read
[CONTRIBUTING.md](../CONTRIBUTING.md) for the review rules and [ARCHITECTURE.md](ARCHITECTURE.md)
for how the pieces fit together.

- [Setup](#setup)
- [Repository layout](#repository-layout)
- [Running from source](#running-from-source)
- [Quality gates](#quality-gates)
- [Debugging](#debugging)
- [Common changes](#common-changes)
- [Releasing](#releasing)

## Setup

You need Rust 1.96 or newer and the native development packages listed under
[Build dependencies](INSTALL.md#build-dependencies) for your distribution. Then:

```bash
git clone https://github.com/narrrl/proton-drive-linux.git
cd proton-drive-linux
cargo build --workspace
```

The GUI builds against libadwaita 1.5, the version in Ubuntu 24.04, and uses newer widgets at run
time when the installed library has them (`crates/pdfs-gui/src/compat.rs`).

## Repository layout

| Path | Contents |
|---|---|
| `crates/pdfs-core` | Shared library: authentication and keyring (`auth`), configuration (`config`), SQLite schema and queries (`db/`), content cache (`cache`), control protocol (`control`), search scoring, sync ignore rules, Google Takeout parsing, machine profile |
| `crates/pdfs-fuse` | The daemon: FUSE handlers (`filesystem`), the tree (`state`), reads (`reads`), the queue and the drain (`queue`, `drain`, `upload`), link state (`link`), the sync engine (`sync`, `sync/`), moves between locations (`relocate`), remote events (`background`), push events (`events`), control handlers (`control`), photos, devices, the supervisor, and the simulation tests (`sim/`) |
| `crates/pdfs-cli` | The `pdfs` binary |
| `crates/pdfs-gui` | `pdfs-app`, `pdfs-tray` and `pdfs-prompt`. Pages in `src/app/pages/`, shared widgets in `src/app/widgets/`, CSS and icons in `resources/` |
| `po/` | Translation template, catalogs and the scripts that maintain them |
| `packaging/` | systemd unit, desktop files, icon, PKGBUILD, RPM spec, AUR recipes |
| `scripts/` | FUSE acceptance suite, map-data generators, the wiki publisher |
| `docs/` | This documentation |

The GUI talks to the daemon only through the control socket. Anything a front end needs that the
daemon does not offer yet is a new request in `pdfs_core::control`, not a database read.

## Running from source

The packaged service and a daemon run from source cannot share the same state directory. Stop the
service before you run your own build:

```bash
systemctl --user stop proton-drive.service
RUST_LOG=pdfs_fuse=debug,info cargo run -p pdfs-cli -- daemon
```

In another terminal:

```bash
cargo run -p pdfs-cli -- status
po/build.sh target/locale                               # compile translations once
PDFS_LOCALEDIR=target/locale cargo run -p pdfs-gui --bin pdfs-app
cargo run -p pdfs-gui --bin pdfs-tray
cargo run -p pdfs-gui --bin pdfs-prompt -- --fzf
```

To keep the systemd unit but point it at your build, see
[INSTALL.md](INSTALL.md#using-a-binary-outside-usrbin).

> [!CAUTION]
> Develop against a test account. The acceptance suite and a buggy daemon can delete or rewrite
> files in Proton Drive.

> [!TIP]
> A daemon stopped in a debugger or deadlocked freezes every process that touches the mount,
> including your shell if its prompt reads the current directory. Keep your shell outside
> `~/ProtonDrive`, prefix commands that touch it with `timeout`, and unmount a dead mount with
> `fusermount3 -uz ~/ProtonDrive`.

## Quality gates

CI (`.github/workflows/ci.yml`) runs these on every push to `main` and every pull request. Run them before you
push:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
scripts/fuse-acceptance.sh --offline-only
```

For changes to the FUSE layer, the queue or the sync engine, also run the
[simulation runs](TESTING.md#simulation-runs), which CI runs only after a push to `main`, and the
[acceptance suite](TESTING.md#acceptance-suite) on a test account.

For changes to GUI strings, check that every catalog is complete:

```bash
for l in $(cat po/LINGUAS); do msgfmt --check --statistics -o /dev/null po/$l.po; done
```

## Debugging

| Tool | Use |
|---|---|
| `RUST_LOG` | Log filter for every binary, for example `RUST_LOG=pdfs_fuse::sync=trace,info` |
| `pdfs diagnose` | Static checks: directories, FUSE, keyring, socket, database integrity. Works without the daemon |
| `pdfs diagnostics` | Live state of a running daemon: each worker's current job and its age, queue depth, control requests in flight, memory |
| `pdfs cache inspect --deep` | Database size, row counts and SQLite `integrity_check` |
| `pdfs --json …` | Raw protocol payloads for query commands |
| `GTK_DEBUG=interactive` | The GTK inspector for `pdfs-app` |
| `PDFS_STAND_IN_WIDGETS=1` | Force the libadwaita 1.5 fallback widgets on a newer system |
| `G_DEBUG=fatal-criticals` | Turn GLib criticals into crashes to get a backtrace |

The databases are ordinary SQLite files. Open a copy, not the live file:

```bash
cp ~/.local/state/proton-drive-linux/cache.db /tmp/cache.db
sqlite3 /tmp/cache.db '.tables'
```

## Common changes

**A new control request.** Add the variant to `Request` (and a `Response` if it returns data) in
`crates/pdfs-core/src/control.rs`, handle it in `crates/pdfs-fuse/src/control.rs`, and list the
`Topic`s it changes in the same file so subscribed front ends refresh. A daemon from the previous
release may still be running when a new front end starts, so treat an error reply to a new
request as "not supported".

**A schema change.** Migrations are forward-only. Add a new step to
`crates/pdfs-core/src/db/migrations.rs` and bump `SCHEMA_VERSION`; never edit a shipped step. Add
a test in `crates/pdfs-core/src/db/tests.rs` that migrates from the previous version.

**A GUI string.** Wrap it in the helpers from `crates/pdfs-gui/src/i18n.rs`, add any new source
file to `po/POTFILES.in`, run `po/update.sh`, and translate the new entries in every catalog. See
[TRANSLATING.md](TRANSLATING.md). The CLI and daemon messages stay in English.

**A setting.** Add the field to `AppConfig` in `crates/pdfs-core/src/config.rs` with
`#[serde(default)]`, so older files still load, and document it in
[CONFIGURATION.md](CONFIGURATION.md).

**A bug fix.** Add or update the entry in [BUGS.md](BUGS.md#adding-an-entry), cover the fix with a
unit test, a simulation scenario or a `regression B<n>` acceptance case, and cite the entry in the
code as `docs/BUGS.md B<n>`. Do not cite planning notes that are not in the repository.

**A user-visible change.** Update the matching page (user guide, CLI reference, configuration)
and the `[Unreleased]` section of [CHANGELOG.md](CHANGELOG.md). `scripts/publish-wiki.sh` copies
the six user pages (`INSTALL`, `USER_GUIDE`, `CLI`, `CONFIGURATION`, `TROUBLESHOOTING`, `RECOVERY`)
to the GitHub wiki, so keep their file names.

## Releasing

Releases are cut from `main` by pushing a `v*` tag. `.github/workflows/release.yml` then:

1. checks that the tag, the workspace version in `Cargo.toml` and `pkgver` in
   `packaging/PKGBUILD` agree;
2. runs the quality gates;
3. builds the release binaries and packages a `.tar.gz`, a `.deb` and a GPG-signed `.rpm`;
4. publishes them as a GitHub release;
5. calls `.github/workflows/aur.yml`, which updates and pushes the AUR recipes and commits them
   back to `main`.

To prepare a release:

```bash
# 1. Bump the version in Cargo.toml and packaging/PKGBUILD, then refresh the lock file.
cargo update --workspace
# 2. Move the [Unreleased] notes in docs/CHANGELOG.md under the new version.
# 3. Commit, tag and push.
git commit -am "chore(release): X.Y.Z"
git tag vX.Y.Z
git push origin main vX.Y.Z
```

Before a release, work through [Before a release](TESTING.md#before-a-release). Open
release-assurance items are listed in [ROADMAP.md](ROADMAP.md#release-assurance).
