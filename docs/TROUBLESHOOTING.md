# Troubleshooting

Start with `pdfs diagnose`, then find your symptom below. If nothing here helps,
[open an issue](https://github.com/narrrl/proton-drive-linux/issues/new/choose) with the
information listed under [Reporting a problem](#reporting-a-problem).

- [First checks](#first-checks)
- [Signing in](#signing-in)
- [The Proton Drive folder](#the-proton-drive-folder)
- [Uploads and sync](#uploads-and-sync)
- [Disk space](#disk-space)
- [Desktop integration](#desktop-integration)
- [Start over with a clean local state](#start-over-with-a-clean-local-state)
- [Reporting a problem](#reporting-a-problem)

## First checks

### `pdfs diagnose`

Checks the installation and prints a report. It does not need the service, because the state
worth diagnosing is often the one where the service will not start.

```console
$ pdfs diagnose
Paths
[ok  ]   state dir: /home/you/.local/state/proton-drive-linux
[ok  ]   cache dir: /home/you/.cache/proton-drive-linux
[ok  ]   state dir writable: write and fsync succeeded
[ok  ]   database: /home/you/.local/state/proton-drive-linux/cache.db (473.1 MiB)

FUSE
[ok  ]   /dev/fuse: /dev/fuse
[ok  ]   unmount helper: fusermount3 or fusermount on PATH

Account
[ok  ]   keyring session: you@proton.me

Daemon
[ok  ]   control socket: /home/you/.local/state/proton-drive-linux/control.sock
[ok  ]   daemon responding
[ok  ]   active mount: /home/you/ProtonDrive (fuse.protondrive)
[ok  ]   network: online
[ok  ]   queued writes: none
[ok  ]   database integrity: SQLite integrity_check passed
[ok  ]   configured mountpoint: /home/you/ProtonDrive

No problems found.
```

It exits non-zero when a check fails, so it also works in a health-check script.

### Logs

```bash
journalctl --user -u proton-drive.service -f           # follow the service
journalctl --user -u proton-drive.service -b -p warning  # warnings since boot
```

For more detail, raise the log level with a drop-in (see
[CONFIGURATION.md](CONFIGURATION.md#the-systemd-service)) and restart the service.

### Service state

```bash
systemctl --user status proton-drive.service
pdfs status
```

## Signing in

**"Proton is asking for a CAPTCHA"** — Proton wants human verification, usually on a VPN or an
unfamiliar network. The CLI cannot show it. Sign in with `pdfs-app`, which displays the challenge;
the stored session then works for the CLI too.

**Sign-in works but is forgotten after a restart** — no Secret Service provider is running, or its
keyring is locked. Install and unlock GNOME Keyring, KWallet or KeePassXC (with Secret Service
enabled). `pdfs diagnose` shows `keyring session` as failing in that case.

## The Proton Drive folder

> [!TIP]
> When the service hangs, any program that touches the Proton Drive folder can hang with it,
> including `ls` and your shell's prompt. Prefix exploratory commands with `timeout 5`.

**`Transport endpoint is not connected`** — the service stopped without unmounting. Unmount and
restart:

```bash
fusermount3 -uz ~/ProtonDrive
systemctl --user restart proton-drive.service
```

**The folder is empty or missing** — check that the service runs and is signed in
(`pdfs status`). The service waits for a stored session before it mounts.

**Everything in the folder hangs** — find out what the service is doing, then restart it:

```bash
timeout 10 pdfs diagnostics
systemctl --user restart proton-drive.service
```

The service watchdog restarts a service that stops answering for two minutes by itself. Attach the
`pdfs diagnostics` output to a bug report; it shows which worker is stuck.

**A change made on another device does not show up** — changes arrive through Proton's event
stream, normally within seconds. To fetch a folder again at once:

```bash
pdfs refresh Documents
```

**"Permission denied" when writing** — the item is in a share where you are a viewer, or under
`Shared with me/`, which is read-only as a folder. Check your role with `pdfs shared-with-me`.

**`ln` or another program fails with "Operation not permitted"** — Proton Drive cannot store
symbolic links, hard links, device files or FIFOs, so creating one fails at once. The error is
intentional. Use a synced folder for projects that need them, or exclude those paths.

## Uploads and sync

**A change shows here but not on other devices** — changes are recorded on this computer first
and sent from a queue. List what is still waiting, and what needs attention:

```bash
pdfs sync queue
pdfs sync issues
```

The app shows the same on **Sync → Overview**. A change waits while the service is offline or
sync is paused (`pdfs status`), and backs off after a failure. To retry at once:

```bash
pdfs sync retry           # every failed operation
pdfs sync retry 42        # one operation
```

A change Proton Drive refuses (storage full, access taken away, folder gone) is listed by
`pdfs sync issues` with what to do. It stays queued until you fix the cause or drop it. Dropping
it undoes it here; save a file's content first if you want to keep it:

```bash
pdfs sync export 42 ~/Desktop/
pdfs sync discard 42
```

If you suspect the local-first behaviour of 3.0.0 itself, `"local_first": false` in `config.json`
makes the Proton Drive folder wait for Proton Drive again while online (see
[CONFIGURATION.md](CONFIGURATION.md#local_first)), and please report the problem.

**A file in a synced folder does not upload** — check that:

- it is not matched by `.pdfsignore` or `ignore_patterns`;
- no program still holds it open for writing (such files wait until closed);
- sync is not paused (`pdfs status`, tray);
- the folder has no error on **Sync → Folders**.

`pdfs sync now` starts a pass immediately.

**Files named `(sync-conflict …)` appeared** — the file changed in two places at once. Nothing was
lost; see [Conflicts](USER_GUIDE.md#conflicts) to pick a version.

**Photos show the wrong date after a Google Takeout import** — run
`pdfs redate-photos --dry-run`, see [USER_GUIDE.md](USER_GUIDE.md#importing-from-google-photos).

## Disk space

| What | Check | Reduce |
|---|---|---|
| Content cache | `pdfs cache inspect` | Lower **Preferences → Storage → Cache size limit**, or `pdfs cache clear` |
| Available-offline files | `pdfs pins` | `pdfs unpin PATH` |
| Metadata database | `pdfs cache inspect` (reclaimable) | `pdfs cache vacuum` |
| Synced folders | `pdfs sync list` | Switch large folders to online only: `pdfs sync mode ID ondemand` |

`pdfs cache vacuum` locks the database while it runs and needs free space for a second copy of it.

## Desktop integration

**No tray icon on GNOME** — install and enable the AppIndicator and KStatusNotifierItem Support
extension. Other desktops show the icon natively. If you hid it from its own menu, turn it back on
under **Preferences → General → Appearance**.

**The search launcher does nothing** — `pdfs-prompt` needs the service. Run it from a terminal to
see its error. For `--fzf`, install `fzf` and a supported terminal.

**The app shows "Not responding"** — the service runs but does not answer within five seconds.
See [Everything in the folder hangs](#the-proton-drive-folder).

## Start over with a clean local state

This removes the local database and cache so they are rebuilt from Proton Drive. It is the fix for
a damaged database and the only way to downgrade to an older release.

> [!WARNING]
> Content in `staging/` and `recovery/` has not reached Proton Drive yet. Deleting it loses those
> changes permanently. Wait until `pdfs sync queue` is empty.

```bash
pdfs sync queue                                   # must list nothing
systemctl --user stop proton-drive.service
mv ~/.local/state/proton-drive-linux ~/.local/state/proton-drive-linux.bak
mv ~/.cache/proton-drive-linux ~/.cache/proton-drive-linux.bak
systemctl --user start proton-drive.service
```

Your session is in the keyring and your settings in `~/.config`, so you stay signed in. Synced
folders are not re-attached automatically; restore them with `pdfs sync restore` as described in
[RECOVERY.md](RECOVERY.md#2-restore-your-synced-folders). Delete the `.bak` directories once
everything is back.

## Reporting a problem

Include:

- `pdfs --version`, your distribution and desktop;
- the output of `pdfs diagnose`;
- relevant log lines from `journalctl --user -u proton-drive.service`;
- for a hang, the output of `timeout 10 pdfs diagnostics`;
- the steps that reproduce it.

Logs contain file names. Remove anything private before posting. Report security problems
privately as described in [SECURITY.md](../SECURITY.md).
