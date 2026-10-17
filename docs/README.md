# Proton Drive for Linux documentation

Proton Drive for Linux is an unofficial Proton Drive client for the Linux desktop. Your Drive
appears as a folder that downloads files when you open them, local folders are backed up and kept
in sync, and a GTK app with a tray icon and a search launcher covers Photos, sharing and more. A
command-line client does all of it from a terminal.

These pages are also published as the
[wiki](https://github.com/narrrl/proton-drive-linux/wiki), generated from the
[`docs/`](https://github.com/narrrl/proton-drive-linux/tree/main/docs) folder of the repository.

## Where to start

- **New here?** [Getting started](GETTING_STARTED.md) takes you from installing to your first
  synced folder.
- **Installing or updating?** [Installation](INSTALL.md) covers Fedora (COPR), Arch (AUR),
  Debian and Ubuntu, NixOS, and building from source.
- **Something not working?** [Troubleshooting](TROUBLESHOOTING.md) starts with `pdfs diagnose`
  and goes through common problems.
- **New computer, or the old one is gone?** [Recovering on a new computer](RECOVERY.md) gets your
  synced folders back.
- **Quick question?** Try the [FAQ](FAQ.md).

## Using it

| Page | What it covers |
|---|---|
| [User guide](USER_GUIDE.md) | How the pieces fit together, the words the app uses, preferences |
| [Files and offline access](FILES.md) | The `~/ProtonDrive` folder, keeping files available offline, the Trash, earlier versions |
| [Synced folders](SYNCED_FOLDERS.md) | Backing up local folders, excluding files, changes waiting to upload, conflicts, pausing |
| [Photos](PHOTOS.md) | Timeline, albums, places, duplicates, importing from Google Photos |
| [Sharing](SHARING.md) | Invitations, public links, items shared with you |
| [Computers](COMPUTERS.md) | Other computers in your account, syncing several computers |
| [Search launcher and tray](LAUNCHER_AND_TRAY.md) | Finding files from a keyboard shortcut, the tray icon |

## Reference

| Page | What it covers |
|---|---|
| [Command-line reference](CLI.md) | Every `pdfs` command, grouped by task, and scripting with `--json` |
| [Configuration](CONFIGURATION.md) | `config.json`, environment variables, file locations, the systemd service |
| [Security model](SECURITY_MODEL.md) | What protects your files, what is kept on this computer, what the client connects to |
| [Changelog](CHANGELOG.md) | What changed in each release |

## Contributing

| Page | What it covers |
|---|---|
| [Development guide](DEVELOPMENT.md) | Building, running and debugging from source; common changes; releasing |
| [Architecture](ARCHITECTURE.md) | Crates, the local tree, read and write paths, the queue, the sync engine, the control protocol, threads, threat model |
| [Testing](TESTING.md) | Unit tests, simulation runs and the FUSE acceptance suite |
| [Translating](TRANSLATING.md) | Adding and updating translations |
| [Roadmap](ROADMAP.md) | Planned work and verification still owed |
| [Bug ledger](BUGS.md) | Every tracked defect with its analysis and status |

Also see [CONTRIBUTING.md](../CONTRIBUTING.md), and [SECURITY.md](../SECURITY.md) to report a
vulnerability privately.
