# Recovering on a new computer

Your laptop is lost, stolen or reinstalled. This page explains what survives, and how to get
your synced folders back on the replacement machine.

- [1. What survives](#1-what-survives)
- [2. Restore your synced folders](#2-restore-your-synced-folders)
- [3. How the restore works](#3-how-the-restore-works)
- [4. Known gaps](#4-known-gaps)

## 1. What survives

### On Proton Drive

| What | Where |
|---|---|
| Every file that finished uploading | Proton Drive |
| The computer's registration and its backup | **Computers** in the app and the web |
| One folder per synced folder, under that computer | Proton Drive |
| The machine profile: which folders were synced, to which local path, in which mode; pins; ignore patterns | `.proton-drive-linux/profile.json` under the computer's backup, end-to-end encrypted like any other file |
| Your account, keys and shares | Proton |

The service rewrites the profile a short while after you add, remove or switch a synced folder,
or change a pin.

### Lost with the machine

| What | Why | Recoverable? |
|---|---|---|
| Changes still in `staging/` or `recovery/` | Writes that were accepted locally but never reached Proton Drive | **No** |
| New files in a synced folder that had not uploaded yet | Same | **No** |
| `config.json`: mountpoint, cache limit, bandwidth limits, appearance | Local config directory | Set again in **Preferences** |
| The local database and content cache | Local disk | Not needed; rebuilt from Proton Drive |

Only the first two rows are real losses. Before you retire a working machine, check that
`pdfs sync queue` prints "Nothing queued" (or that **Sync → Overview** shows nothing waiting).

### Before anything else: revoke the old session

The lost machine holds a decrypted content cache, a plain-text index of your file names and a
live session in its keyring. Whether its disk was encrypted decides how much of that is exposed,
but the session is valid either way.

**Sign the lost machine out from your Proton account's security settings before you restore.**
The client cannot do that for you.

If you have also lost your password, account recovery is Proton's matter, not this client's.
Proton Drive is end-to-end encrypted: without the password or a recovery method, the data cannot
be decrypted.

## 2. Restore your synced folders

### Step 1: Install and sign in

Install the client as described in [INSTALL.md](INSTALL.md) and sign in with `pdfs-app`. Signing
in from the app also starts the service.

### Step 2: Continue the old computer's backup

Tell this machine which computer it replaces, so new changes go into the existing backup instead
of a second, empty one.

- **In the app:** open **Computers**, open the menu of the old computer and choose
  **Continue This Backup Here…**.
- **From a terminal:**

  ```bash
  pdfs devices list                 # find the old computer's UID
  pdfs devices adopt UID
  ```

Adoption takes effect at once and is stored in `config.json` as `device_uid`. It survives a
change of hostname. `pdfs devices adopt --clear` returns to the default, which is to match a
computer whose name equals this machine's hostname.

> [!NOTE]
> Without adoption, the client only finds the old computer if this machine has the same
> hostname. Otherwise it registers a new computer, and nothing points at the old backup.

### Step 3: Restore the folders

- **In the app:** open **Computers**, open the menu of **This computer** and choose
  **Restore Folders…**. Confirm or change the local path of each folder.
- **From a terminal:**

  ```console
  $ pdfs sync restore
  Documents [mirror] -> /home/you/Documents (Enter to accept, 'n' to skip, or a path)
  Projects [ondemand] -> /home/you/Projects (Enter to accept, 'n' to skip, or a path)
  ```

  Press Enter to accept a path, type `n` to skip the folder, or type another path. `--yes`
  accepts every proposal without asking.

For each folder you accept, the service creates the local directory, attaches it to the folder in
the backup, restores its mode (**Mirror** or **Online only**) and downloads it. Follow the
progress on **Sync → Overview** or with `pdfs sync list`.

> [!TIP]
> Restore into empty or missing directories. If a directory already holds files, for example
> from another backup, the client cannot tell which side is newer. It keeps both: your local file
> is renamed to a `(sync-conflict …)` copy and the version from Proton Drive is downloaded. See
> [Conflicts](USER_GUIDE.md#conflicts).

### Step 4: Restore the rest by hand

- **Available offline files.** The profile records them, but the restore does not apply them yet.
  Mark them again in the app, or with `pdfs pin PATH`.
- **Preferences.** Set the cache limit, bandwidth limits, mountpoint and appearance again.

### Step 5: Clean up

If this machine registered a second computer before you adopted the old one, remove it:

```bash
pdfs devices list
pdfs devices rm UID
```

> [!WARNING]
> Removing a computer deletes its backup from Proton Drive. Check what is under it in
> **Computers → Browse Files** first.

### Restoring another computer's folders

To bring folders from a computer that still exists, for example a desktop whose documents you want
on a laptop, use **Restore to This Computer…** on that computer's row, or:

```bash
pdfs sync restore --device UID
```

The folders stay part of the other computer's backup and sync in both directions. This machine
does not take over that computer's backup.

### Moving another computer's backup here

To switch computers for good, for example from a Windows PC to this Linux machine, move the old
computer's folders into this computer's backup instead. Use **Move to This Computer…** on that
computer's row, or:

```bash
pdfs devices list                 # find the old computer's UID
pdfs devices migrate UID
```

Each folder moves on Proton Drive itself, so nothing is uploaded again and no storage is used
twice. A folder whose name this computer already uses gets the old computer's name added, for
example `Documents (DESKTOP-1)`. The restore picker then opens, as in
[Step 3](#step-3-restore-the-folders), so you choose where each folder goes here.

The old computer stays in your account, without those folders. Unlike
[adoption](#step-2-continue-the-old-computers-backup), this machine keeps its own name and backup.

> [!WARNING]
> Stop Proton Drive on the old computer first. A client still running there sees its folders
> disappear, and may upload them again as new.

## 3. How the restore works

This section is for the curious and for anyone debugging a restore.

- **Which computer the machine is.** At start, the service uses `device_uid` from `config.json`
  when it is set. When it is not, it looks for a Linux computer in the account whose name equals
  the hostname, and registers a new one when there is none. An adopted UID that no longer exists
  in the account is reported in the log instead of being replaced silently.
- **What can be restored.** `pdfs sync restore` lists the folders directly under the computer's
  backup, except `.proton-drive-linux`. Folders already synced on this machine are skipped.
- **Proposed paths.** When the profile names a local path whose parent directory exists on this
  machine, that path is proposed. Otherwise the proposal is `~/<folder name>`. The service never
  writes to a path you have not confirmed.
- **Attaching by UID.** The restore binds each local path to the remote folder's UID, not to its
  name. Restoring `Documents` into `~/docs` works.
- **Downloading.** A restored folder starts with an empty sync baseline. Against an empty local
  directory, every remote file counts as new and is downloaded. Files present on both sides with
  no baseline become conflict copies, as described above.
- **The wipe guard.** A sync pass in which every previously synced file has vanished locally is
  refused instead of being mirrored to Proton Drive as a mass deletion. A folder that was emptied
  by accident therefore does not empty the backup.
- **The profile format.** `profile.json` carries a `version` field. A client refuses a profile
  written by a newer, incompatible client instead of applying part of it.

## 4. Known gaps

| Gap | Effect |
|---|---|
| Pins are backed up but not restored | Mark files available offline again by hand |
| No warning at shutdown or sign-out while changes are queued | Check `pdfs sync queue` before you retire a machine |
| The full disaster drill (delete all local state, restore, compare byte for byte) has not been run against a live account | The individual steps are tested; the whole sequence is not |

Planned work is tracked in [ROADMAP.md](ROADMAP.md).
