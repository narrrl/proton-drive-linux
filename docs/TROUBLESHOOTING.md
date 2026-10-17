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
[CONFIGURATION.md](CONFIGURATION.md#the-systemd-service)) and restart the service.

### Service state

```bash
systemctl --user status proton-drive.service
pdfs status
```

## Signing in

**"Proton is asking for a CAPTCHA"** — Proton wants human verification, usually on a VPN or an
unfamiliar network. `pdfs login` prints the verification page's address: open it in your browser,
complete it, then press Enter. The app shows the page in its own window.

**"Verify in Your Browser"** — WebKit, which shows the CAPTCHA page in the app, could not start
its sandbox. Hardened systems such as secureblue keep its bubblewrap from the journal socket, and
the log then shows `bwrap: Can't get type of source /run/systemd/journal/socket: Permission
denied` and `Failed to fully launch dbus-proxy`. Choose **Open Verification Page**, complete the
page in your browser, then choose **Continue**. If that sign-in is refused too, check that `bwrap`
and `xdg-dbus-proxy` are installed. As a last resort, sign in once with the sandbox switched off
for this one run:

```bash
WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 pdfs-app
```

Quit the app first, so this starts a new instance. The page then runs without WebKit's isolation,
so use this only for the sign-in. The stored session keeps working without it. Signing in from
your usual network, without a VPN, often avoids the CAPTCHA in the first place.

**"This version of the app is no longer supported"** or **"Proton is temporarily refusing
sign-in from this device"** — Proton blocks sign-in for a while after too many attempts or from a
network it distrusts, and sends the first message even when the release is current. Do not retry
for several hours, up to a day, then sign in once. If it still fails, run
`RUST_LOG=debug pdfs login` and look for `code=` in the `proton api request failed` line: 2028 is
this block, 5003 is a release Proton no longer accepts.

**Sign-in works but is forgotten after a restart** — no Secret Service provider is running, or its
keyring is locked. Install and unlock GNOME Keyring, KWallet or KeePassXC (with Secret Service
enabled). `pdfs diagnose` shows `keyring session` as failing in that case. When the app finds
a saved sign-in it cannot read, it says so above the sign-in form and opens on its own once the
keyring is unlocked. You don't have to sign in again.

**Asked for the mailbox password after every reboot** — the account has a separate mailbox
password (two-password mode). Such accounts are locked by default: the unlock lasts until you log
out or restart. To stay unlocked, turn on **Keep unlocked on this computer** in the unlock dialog,
or run `pdfs unlock --remember`. Either one stores the unlock in the system keyring.
[SECURITY_MODEL.md](SECURITY_MODEL.md) explains what that trades away.

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

**`fetch My Files root: … failed to decrypt: missing key`** in the service log — the account
keys cannot open your Drive volume, which happens after a password reset. Proton locks the volume
that was encrypted to the old keys. From 3.4.1 the client starts a new, empty My Files volume
in that case. Your old files stay in the locked volume: open [drive.proton.me](https://drive.proton.me)
and restore them there with your previous password. If the volume is not locked and the error
stays, run `pdfs diagnose` and post its **Keys** section in an issue.

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
