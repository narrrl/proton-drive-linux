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

When you are signed in but the service does not answer, it also asks Proton which of your keys
open, under **Keys**: each account key, each address key, and the address key your My Files
share is encrypted to. A key that does not open is named with the key it depends on. The section
shows key IDs, which are not secret, so you can post it in an issue.

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
[Configuration](CONFIGURATION.md#the-systemd-service)) and restart the service.

### Service state

```bash
systemctl --user status proton-drive.service
pdfs status
```

## Signing in

### Proton is asking for a CAPTCHA

Proton wants to check that you are human, usually on a VPN or an unfamiliar network. The app
shows the page in its own window. `pdfs login` prints the page's address instead: open it in your
browser, complete it, then press Enter.

### "Verify in Your Browser"

WebKit, which shows the CAPTCHA page in the app, could not start its sandbox. Hardened systems
such as secureblue keep its bubblewrap away from the journal socket. The log then shows
`bwrap: Can't get type of source /run/systemd/journal/socket: Permission denied` and
`Failed to fully launch dbus-proxy`.

1. Choose **Open Verification Page** and complete the page in your browser.
2. Choose **Continue**.

If that sign-in is refused too, check that `bwrap` and `xdg-dbus-proxy` are installed. As a last
resort, quit the app and sign in once with the sandbox switched off for this one run:

```bash
WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 pdfs-app
```

The page then runs without WebKit's isolation, so use this only for the sign-in. The stored
session keeps working without it. Signing in from your usual network, without a VPN, often avoids
the CAPTCHA in the first place.

### "This version of the app is no longer supported" or "Proton is temporarily refusing sign-in from this device"

Proton blocks sign-in for a while after too many attempts, or from a network it distrusts. It sends
the first message even when your version is current. Do not retry for several hours, up to a day,
then sign in once.

If it still fails, run `RUST_LOG=debug pdfs login` and look for `code=` in the
`proton api request failed` line. 2028 is this block; 5003 is a release Proton no longer accepts.

### Sign-in works but is forgotten after a restart

No Secret Service provider is running, or its keyring is locked. Install and unlock GNOME
Keyring, KWallet or KeePassXC (with Secret Service turned on). `pdfs diagnose` then shows
`keyring session` as failing.

When the app finds a saved sign-in it cannot read, it says so above the sign-in form, and opens
by itself once the keyring is unlocked. You do not have to sign in again.

### Asked for the mailbox password after every reboot

The account has a separate mailbox password (two-password mode). Such accounts are locked by
default: the unlock lasts until you log out or restart. To stay unlocked, turn on **Keep unlocked
on this computer** in the unlock dialog, or run `pdfs unlock --remember`. Either one stores the
unlock in the system keyring. The [security model](SECURITY_MODEL.md) explains what that trades
away.

## The Proton Drive folder

> [!TIP]
> When the service hangs, any program that touches the Proton Drive folder can hang with it,
> including `ls` and your shell's prompt. Put `timeout 5` in front of commands you try.

### `Transport endpoint is not connected`

The service stopped without unmounting. Unmount and restart:

```bash
fusermount3 -uz ~/ProtonDrive
systemctl --user restart proton-drive.service
```

### My files does not open: the folder is not empty

The app says **Your Proton Drive Folder Isn't Empty**, and `pdfs diagnose` reports
`mount: failing … is not empty`. The folder chosen for My files has files in it, and the service
does not open My files over them: they would be hidden, and not uploaded, for as long as the
service runs.

Move the files out of the folder, or choose another folder in **Preferences → General →
Location**. The service tries again on its own and opens once the folder is empty. To put the
files on Proton Drive, copy them into My files once it is open.

Files saved into `~/ProtonDrive` while the service was stopped are in the same place: in the
local folder, not on Proton Drive.

### The folder is empty or missing

Check that the service runs and is signed in, with `pdfs status`. The service waits for a stored
session before it shows your files.

### `fetch My Files root: … failed to decrypt: missing key` in the log

Your account keys cannot open your Drive, which happens after a password reset: Proton locks the
files that were encrypted with the old keys. Since 3.4.1 the client then starts a new, empty My
files. Your old files are still there, locked: open [drive.proton.me](https://drive.proton.me) and
restore them with your previous password. If your files are not locked and the error stays, run
`pdfs diagnose` and post its **Keys** section in an issue.

### Everything in the folder hangs

Find out what the service is doing, then restart it:

```bash
timeout 10 pdfs diagnostics
systemctl --user restart proton-drive.service
```

The service restarts by itself when it stops answering for two minutes. Attach the
`pdfs diagnostics` output to a bug report; it shows which part is stuck.

### A change made on another device does not show up

Changes from other devices normally arrive within seconds. To fetch a folder again at once:

```bash
pdfs refresh Documents
```

### "Permission denied" when writing

The item is in a share where you are a viewer, or directly under `Shared with me/`, which is
read-only. Check your role with `pdfs shared-with-me`.

### `ln` or another program fails with "Operation not permitted"

Proton Drive cannot store symbolic links, hard links, device files or FIFOs, so creating one fails
at once. This is on purpose. For projects that need them, use a
[synced folder](SYNCED_FOLDERS.md) and exclude those paths.

## Uploads and sync

### A change shows here but not on other devices

Changes are recorded on this computer first and sent from a queue. List what is still waiting,
and what needs you:

```bash
pdfs sync queue
pdfs sync issues
```

The app shows the same on **Sync → Overview**. A change waits while the service is offline or
sync is paused (`pdfs status`), and waits longer after each failure. To retry at once:

```bash
pdfs sync retry           # every failed change
pdfs sync retry 42        # one change
```

A change Proton Drive refuses (storage full, access taken away, folder gone) is listed by
`pdfs sync issues` with what to do. It stays queued until you fix the cause or drop it. Dropping
it undoes it here, so save a file's content first if you want to keep it:

```bash
pdfs sync export 42 ~/Desktop/
pdfs sync discard 42
```

[Changes waiting for Proton Drive](SYNCED_FOLDERS.md#changes-waiting-for-proton-drive) explains
the choices.

If you suspect that the instant, queued changes introduced in 3.0.0 are the problem,
`"local_first": false` in `config.json` makes the Proton Drive folder wait for Proton Drive again
while online (see [Configuration](CONFIGURATION.md#local_first)). Please report the problem too.

### A file in a synced folder does not upload

Check that:

- it is not excluded by `.pdfsignore` or `ignore_patterns`;
- no program still holds it open for writing (such files wait until closed);
- sync is not paused (`pdfs status`, or the tray);
- the folder shows no error on **Sync → Folders**.

`pdfs sync now` starts a sync pass at once.

### A synced folder stays in error and nothing downloads

The log says `M of the N synced paths are missing locally; refusing to trash them on Drive`.
The folder synced before, and now all or nearly all of its files are gone from this computer:
everything, or nine in ten of a folder with 20 or more files and folders. The sync stops so that
it does not delete them on Proton Drive too. Check that the folder is the right
one and that its disk is mounted. To get the files back, choose **Sync From Scratch…** in the
folder's menu on **Sync → Folders**, or run:

```bash
pdfs sync list            # find the folder's ID
pdfs sync reset 1
```

This downloads what Proton Drive has and uploads what only this computer has. Nothing is deleted.
If you deleted the files on purpose, delete them on Proton Drive too, or stop syncing the folder.

### Files named `(sync-conflict …)` appeared

The file changed in two places at once. Nothing was lost. See
[Conflicts](SYNCED_FOLDERS.md#conflicts) to pick a version.

### Photos show the wrong date after a Google Takeout import

Run `pdfs redate-photos --dry-run`. See
[Fixing photos filed under the wrong date](PHOTOS.md#fixing-photos-filed-under-the-wrong-date).

## Disk space

| What | Check | Reduce |
|---|---|---|
| Cache of opened files | `pdfs cache inspect` | Lower **Preferences → Storage → Cache size limit**, or `pdfs cache clear` |
| Available-offline files | `pdfs pins` | `pdfs unpin PATH` |
| Metadata database | `pdfs cache inspect` (reclaimable) | `pdfs cache vacuum` |
| Synced folders | `pdfs sync list` | Switch large folders to online only: `pdfs sync mode ID ondemand` |

`pdfs cache vacuum` locks the database while it runs, and needs free space for a second copy of
it.

## Desktop integration

### No tray icon on GNOME

Install and turn on the
[AppIndicator and KStatusNotifierItem Support](https://extensions.gnome.org/extension/615/appindicator-support/)
extension. Other desktops show the icon without help. If you hid it from its own menu, turn it
back on under **Preferences → General → Appearance**.

### The search launcher does nothing

`pdfs-prompt` needs the service. Run it from a terminal to see its error. For `--fzf`, install
`fzf` and a supported terminal.

### The app shows "Not responding"

The service runs but does not answer within five seconds. See
[Everything in the folder hangs](#everything-in-the-folder-hangs).

## Start over with a clean local state

This removes the local database and cache, so they are built again from Proton Drive. It fixes a
damaged database, and it is the only way to go back to an older release.

> [!WARNING]
> Content in `staging/` and `recovery/` has not reached Proton Drive yet. Deleting it loses those
> changes for good. Wait until `pdfs sync queue` is empty.

```bash
pdfs sync queue                                   # must list nothing
systemctl --user stop proton-drive.service
mv ~/.local/state/proton-drive-linux ~/.local/state/proton-drive-linux.bak
mv ~/.cache/proton-drive-linux ~/.cache/proton-drive-linux.bak
systemctl --user start proton-drive.service
```

Your session is in the keyring and your settings in `~/.config`, so you stay signed in. Synced
folders are not attached again by themselves. Restore them with `pdfs sync restore`, as described
in [Recovery](RECOVERY.md#2-restore-your-synced-folders). Delete the `.bak` directories once
everything is back.

## Reporting a problem

Include:

- `pdfs --version`, your distribution and desktop;
- the output of `pdfs diagnose`;
- relevant log lines from `journalctl --user -u proton-drive.service`;
- for a hang, the output of `timeout 10 pdfs diagnostics`;
- the steps that make it happen.

Logs contain file names. Remove anything private before posting. Report security problems
privately, as described in [SECURITY.md](../SECURITY.md).
