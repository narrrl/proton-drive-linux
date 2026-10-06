# Configuration

Most settings are in the app under **Preferences**. This page documents the configuration file
behind them, the settings that exist only there, the environment variables the programs read,
and where the client keeps its data.

- [Files and directories](#files-and-directories)
- [`config.json`](#configjson)
- [Environment variables](#environment-variables)
- [The systemd service](#the-systemd-service)

## Files and directories

| Path | Contents |
|---|---|
| `~/.config/proton-drive-linux/config.json` | Settings (this page) |
| `~/.local/state/proton-drive-linux/cache.db` | Metadata database: names, folder tree, sync state, upload queue, photo index |
| `~/.local/state/proton-drive-linux/sdk_cache.db` | Encrypted cache of Proton Drive metadata. Safe to delete while the service is stopped |
| `~/.local/state/proton-drive-linux/control.sock` | Socket the app, tray, prompt and CLI use to reach the service |
| `~/.cache/proton-drive-linux/content/` | File content cache, see below |
| `~/ProtonDrive` | The Proton Drive folder (default mountpoint) |
| System keyring | The session. Never written to disk by the client |

The directories honour `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME`. They must be real
directories owned by you with mode `0700`; the service refuses to start otherwise.

Inside `content/`:

| Directory | Contents | Safe to delete? |
|---|---|---|
| top level, `blocks/`, `thumbs/` | Cached file content and thumbnails | Yes — use `pdfs cache clear` |
| `scratch/` | Files open for writing right now | **No** |
| `staging/` | Closed files waiting to upload | **No** — may be the only copy |
| `recovery/` | Writes rescued after a crash, waiting to be queued | **No** — may be the only copy |

> [!CAUTION]
> Never delete `staging/` or `recovery/` by hand. Check `pdfs sync queue` first. Content there has
> not reached Proton Drive and exists nowhere else.

Everything in these directories except the keyring is **decrypted**. See
[ARCHITECTURE.md §10](ARCHITECTURE.md#10-threat-model-what-this-client-writes-to-disk-in-plaintext).

## `config.json`

The file is created with defaults on first start. Every key is optional; a missing key takes its
default. Edit it while the app is closed, then restart the service
(`systemctl --user restart proton-drive.service`) for service settings to apply.

An unreadable file is never overwritten: the programs log an error and run with defaults until you
fix it.

```json
{
  "mountpoint": "/home/you/ProtonDrive",
  "cache_budget": 10737418240,
  "upload_limit": null,
  "download_limit": 5242880,
  "ignore_patterns": ["node_modules/", "target/", "*.iso"],
  "language": "de",
  "proton_theme": true,
  "online_map": false,
  "tray_hidden": false,
  "files_view": { "list": true, "sort": "modified", "descending": true, "folders_first": true },
  "open_with": {
    "terminal": ["alacritty", "-e"],
    "rules": [{ "match": ["@text"], "command": ["nvim"], "terminal": true }]
  },
  "prompt": { "mode": "fzf", "menu_limit": 50 }
}
```

### Reference

| Key | Type | Default | In the app | Meaning |
|---|---|---|---|---|
| `mountpoint` | path | `~/ProtonDrive` | General → Location | Where My files is mounted |
| `cache_budget` | bytes | 5 GiB | Storage → Cache | Soft limit for cached content. `0` means unlimited. Available-offline files do not count against it |
| `upload_limit` | bytes/s | none | General → Network | Shared cap for all uploads. `0` or `null` means none |
| `download_limit` | bytes/s | none | General → Network | Shared cap for all downloads, reads from the mount included |
| `ignore_patterns` | list | see below | — | Global ignore rules for synced folders |
| `device_uid` | string | none | Computers → Continue This Backup Here… | The computer this machine continues. Unset means "match by hostname" |
| `conflict_sweep` | `off` / `report` / `enforce` | `report` | — | See [`conflict_sweep`](#conflict_sweep) |
| `local_first` | bool | `true` | — | See [`local_first`](#local_first) |
| `language` | gettext code | system | General → Appearance | Language of the app, tray and prompt, such as `de` or `pt_BR` |
| `proton_theme` | bool | follow system | General → Appearance | Proton's colours instead of the system theme |
| `online_map` | bool | `false` | General → Appearance | Street map for Places from OpenFreeMap |
| `tray_hidden` | bool | `false` | General → Appearance | Hide the tray icon |
| `files_view` | object | grid, by name | My files view menu | `list`, `sort` (`name`, `size`, `modified`), `descending`, `folders_first` |
| `open_with` | object | `xdg-open` | — | See [`open_with`](#open_with) |
| `prompt` | object | built-in window | — | See [`prompt`](#prompt) |

`app_version` and `user_agent` identify the client to Proton. Leave them alone.

### `ignore_patterns`

Gitignore-style rules applied to every synced folder, in addition to each folder's own
`.pdfsignore`. When the key is absent, these defaults apply:

```json
[".git/", ".hg/", ".svn/", "node_modules/", "target/", ".venv/", "__pycache__/",
 "*~", "*.swp", "*.tmp", ".DS_Store", "Thumbs.db"]
```

Set it to `[]` to turn the defaults off. See
[USER_GUIDE.md](USER_GUIDE.md#excluding-files-with-pdfsignore) for how ignoring behaves.

### `conflict_sweep`

A background task looks for `(sync-conflict …)` copies that are byte-identical to the file they
were copied from.

| Value | Behaviour |
|---|---|
| `off` | Do not run. |
| `report` | Log and show what would be removed; remove nothing. **Default.** |
| `enforce` | Move copies proven identical (same size and SHA-1) to the Trash. |

`PDFS_CONFLICT_SWEEP` overrides the setting for one run of the service.

### `local_first`

A new folder, a new file, a rename or a delete made through the mount is recorded locally and the
call returns at once. The change goes to Drive from the queue, the same way it does offline, and
`pdfs status` counts it until it lands.

Set it to `false` to have these calls wait for Drive again, as before 3.0.0. The switch is there
for one release, in case the queued path misbehaves for you; please report it if it does.

### `open_with`

Decides how files picked in the search launcher or the app are opened. Rules are tried in order;
the first match wins.

```json
{
  "open_with": {
    "terminal": ["alacritty", "-e"],
    "default": ["xdg-open"],
    "rules": [
      { "match": ["@text", "*.rs", "*.toml"], "command": ["nvim"], "terminal": true },
      { "match": ["*.png", "*.jpg"], "command": ["imv"] },
      { "match": ["@document"], "command": ["$EDITOR", "{}"], "terminal": true }
    ]
  }
}
```

- `match` takes file-name globs (`*.md`, `notes-*.txt`) or the classes `@dir`, `@text`,
  `@document`, `@image`, `@media` and `@any`. Patterns are matched against the name in Drive, not
  against the cached file, which is stored under a content hash.
- `command` is an argument list, not a shell line. `{}` is replaced by the path; without it the
  path is appended. `$VAR` expands from the environment.
- `"terminal": true` runs the command inside `terminal`. When `terminal` is unset, the client uses
  `$PDFS_TERMINAL`, then `$TERMINAL`, then the first known terminal on `PATH`, and adds that
  terminal's "run this command" flag for you.
- `default` replaces `xdg-open` for files no rule matches.

### `prompt`

Settings for `pdfs-prompt`.

| Key | Default | Meaning |
|---|---|---|
| `mode` | `gtk` | Front end used without a flag: `gtk`, `dmenu` or `fzf` |
| `menu` | first installed of fuzzel, rofi, wofi, tofi, bemenu, dmenu | Launcher for `dmenu` mode, as an argument list. `{prompt}` is replaced by the prompt text; without it, the launcher's own prompt flag is added |
| `terminal` | first installed of foot, ghostty, kitty, alacritty, wezterm, xterm | Terminal for `fzf` mode. `{cmd}` is replaced by the command; without it, the command is appended |
| `menu_limit` | `50` | Results shown in `dmenu` and `fzf` mode |
| `menu_icons` | `true` | File-type icons in launchers that support them |

```json
{
  "prompt": {
    "mode": "dmenu",
    "menu": ["fuzzel", "--dmenu", "--width", "60"],
    "terminal": ["foot", "--app-id=pdfs-prompt", "--"]
  }
}
```

Command-line flags win over the file: `--gtk`, `--dmenu`, `--fzf` and `--menu`.

## Environment variables

| Variable | Read by | Effect |
|---|---|---|
| `RUST_LOG` | all | Log filter, for example `RUST_LOG=debug` or `RUST_LOG=pdfs_fuse=debug,info`. Default `info` |
| `PDFS_CONFLICT_SWEEP` | service | Overrides `conflict_sweep` (`off`, `report`, `enforce`) |
| `PDFS_TERMINAL`, `TERMINAL` | app, prompt | Terminal for `open_with` rules with `"terminal": true` |
| `LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG` | app, tray, prompt | Interface language when `language` is unset |
| `PDFS_LOCALEDIR` | app, tray, prompt | Load translations from another directory (development) |
| `PDFS_STAND_IN_WIDGETS` | app | `1` uses the libadwaita 1.5 fallback widgets on newer systems (development) |

The tests have their own variables (`PDFS_ACCEPTANCE_*`, `PDFS_SIM_*`, `PDFS_MIGRATE_DB`),
listed in [TESTING.md](TESTING.md).

## The systemd service

`proton-drive.service` is a user unit started with your graphical session
(`WantedBy=graphical-session.target`). It restarts the service whenever it exits, restarts it
when it stops answering for two minutes (`WatchdogSec=120`), throttles it above 2 GiB of memory
(`MemoryHigh`) and stops it at 6 GiB (`MemoryMax`). To change the unit without editing the
packaged file, use a drop-in:

```bash
systemctl --user edit proton-drive.service
```

```ini
[Service]
Environment=RUST_LOG=pdfs_fuse=debug,info
Environment=PDFS_CONFLICT_SWEEP=off
```

Logs go to the journal: `journalctl --user -u proton-drive.service -f`.
