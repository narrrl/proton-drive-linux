# Recovery: Getting a Replacement Machine Back onto Your Device

You lose the laptop. You install `pdfs` on a new one. What does it take to get the
synced folders back?

This document is two things: **§2 is the runbook that works with the client as it
stands today**, gotchas included, and **§3 is the plan to reduce that runbook to
one command.** §1 is the part neither can fix — what actually died with the
machine.

---

## 1. What survives, and what does not

### Survives (it is on Drive, or derivable)

| Thing | Where it lives |
|---|---|
| Every file that finished uploading | Proton Drive |
| The Device registration itself | Proton Drive (`enumerate_devices`) |
| The device's folder tree — one folder per synced folder | Proton Drive, under the device root |
| Your account, keys, sharing | Proton |

### Does not survive

| Thing | Why | Recoverable? |
|---|---|---|
| Undrained `staging/` + `recovery/` blobs | Writes the kernel accepted but that never reached Drive. They existed only on that disk. | **No.** |
| Local-only files in a mirror folder that had not uploaded yet | Same. | **No.** |
| `sync_folder` rows — which local path maps to which remote folder, and in which mode | Local SQLite (`cache.db`) | Rebuildable by hand (§2), not automatically (§3) |
| `sync_entry` baseline | Local SQLite | Not needed. An empty baseline reconciles correctly — see §2.4. |
| Pins | Local SQLite | Rebuildable by hand |
| `config.json` — mountpoint, cache budget | Local config dir | Rebuildable by hand |
| `cache.db` node/FTS/photo caches | Local SQLite | Pure cache; rebuilds itself |

The first two rows are the only genuinely unrecoverable ones, and today nothing
warns you they are non-empty before you walk away from a machine. That is tracked
separately from this document as a pending-writes visibility gap.

### Before anything else: revoke

The lost machine holds a decrypted content cache, a plaintext index of every node
name in your Drive (`cache.db`), and a live session in its keyring. Whether that
matters depends on whether its disk was encrypted — but the session does not.

**Revoke the session from your Proton account settings before restoring.** Nothing
in this client can do that for you; the daemon on the lost machine holds
credentials it will happily keep refreshing.

If you have also lost your password, Proton's recovery phrase is what determines
whether the data is reachable at all. That is upstream of this client entirely: if
zero-knowledge encryption worked as advertised, nobody at Proton can help.

---

## 2. The runbook that works today

### 2.1 Install, log in

```bash
pdfs login          # stores the session in the keyring
systemctl --user start proton-drive     # or let autostart do it
```

### 2.2 Re-adopt the device — check this before anything else

`Core::ensure_device` recovers an existing device by **matching a Linux device
whose name equals this machine's hostname**, read from
`/proc/sys/kernel/hostname`. So:

```bash
hostnamectl hostname            # what this machine will register as
pdfs devices list               # what the account already has
```

- **Hostnames match** → the new machine silently re-adopts the old device and its
  root folder. This is the happy path, and it is why the rest of the runbook works.
- **Hostnames differ** → `pdfs` registers a **second device**. Nothing errors.
  Your old device and all its folders are still there, but this machine is not
  attached to them, and every step below will quietly create empty folders under
  the new device instead.

> **The single most important step in this document:** if the hostnames differ,
> set the new machine's hostname to match the old device's name *before* adding
> any sync folders.
> ```bash
> sudo hostnamectl set-hostname OLD-DEVICE-NAME
> ```
> There is no supported way to adopt a device by uid yet — see §3.1.

### 2.3 Find out which folders the device had

```bash
pdfs devices list       # confirm the device and note its name
```

The device's synced folders are the folders directly under its root in Drive.
Browse them in the GUI's Computers page, or in the Proton Drive web app under
that device. **Write down their exact names** — you need them in the next step.

### 2.4 Re-attach each folder

For each folder name you noted, create the local directory and add it:

```bash
mkdir -p ~/Documents
pdfs sync add ~/Documents
```

`add_sync_folder` looks for a folder under the device root named after the local
directory's **basename**, and reuses it if found rather than creating a second
one. So `~/Documents` re-attaches to the existing `Documents`. With an empty
baseline and an empty local directory, the first reconcile classifies every remote
path as `(None, Some(remote))` with no baseline → **download**, and the folder
refills from Drive.

Three things to be careful about:

- **The basename must match exactly.** Restoring to `~/docs` when the device
  folder is `Documents` does not re-attach — it creates a *new*, empty `docs`
  folder in Drive and syncs nothing. The failure is silent; you just see an empty
  directory. Check with `pdfs sync list` and the Computers page.
- **Add the folder empty, or accept conflict copies.** If the local directory
  already has files (restored from a backup, say), there is no baseline to say
  which side is newer, so both sides read as changed and the reconcile takes the
  conflict arm: your local file is renamed to a `(sync-conflict <ts>)` copy and
  the remote version is downloaded. Nothing is lost, but it is noisy. Restoring
  into an empty directory avoids it.
- **A wholly empty local tree is now safe.** Before the audit fixes, adding a
  folder whose local side was empty against a populated remote could, on a later
  pass, be read as "the user deleted everything" and trash the lot. `sync.rs`'s
  `guard_local_wipe` now refuses any pass where every baseline path has vanished
  locally, which is exactly the restore shape.

### 2.5 Restore modes, pins, and settings by hand

```bash
pdfs sync list                       # note the ids
pdfs sync mode <id> ondemand         # for folders that were on-demand
```

Nothing records which folders were `ondemand` before, so this is from memory. Pins
and the cache budget/mountpoint likewise: set them again from the Settings page.

### 2.6 Clean up

If a duplicate device got created before you noticed the hostname mismatch:

```bash
pdfs devices list
pdfs devices rm <uid-of-the-stray-device>
```

Check what is under it in the web app first. Deleting a device you actually
attached folders to will take its folders with it.

---

## 3. The plan: make this one command

The pieces all exist — device enumeration, folder-name reuse, an empty-baseline
reconcile that downloads correctly. What is missing is that the restore is
**implicit, name-based, and undiscoverable**. Each phase below removes one of
those properties. They are independent and land in this order by value.

### P1 — Adopt a device explicitly, by uid

**Problem:** device identity is inferred from the hostname. A new laptop with a
new name silently forks a second device, and the failure looks like "my files
didn't come back" rather than an error.

- Add `Request::AdoptDevice { uid }`: validate the uid against
  `enumerate_devices`, write the `StoredDevice` row, and refuse if any
  `sync_folder` rows already reference a different device.
- `pdfs devices adopt <uid>`, and an "Adopt this device" action on rows in the
  GUI's Computers page that are not the current machine.
- Make the fork *visible* rather than silent: when `ensure_device` is about to
  create a device and the account already has other Linux devices, log a warning
  naming them, and surface it in `pdfs status`.

Keep hostname matching as the default. It is right for the common case; it just
must not be the *only* way.

### P2 — Discover and re-attach the device's folders

**Problem:** §2.3 and §2.4 are "remember what you had, then retype it exactly."

- Add `Request::ListDeviceFolders { device_uid }` — enumerate the device root's
  child folders and report name, node uid, child count, and total size. This is
  the "what did this machine sync?" answer that today requires the web app.
- Add `Request::AttachSyncFolder { remote_uid, local_path, mode }` — the
  re-attach counterpart to `AddSyncFolder`. It binds a local path to a **specific
  remote uid** rather than resolving by basename, which removes the silent
  `~/docs` vs `Documents` failure entirely.
- CLI: `pdfs sync restore [--device <uid>] [--into <dir>]` — list the device's
  folders, propose `<dir>/<name>` for each (default `$HOME`), and attach the ones
  the user confirms.
- GUI: a "Restore folders from this device" flow on the Computers page, with a
  per-folder local-path picker defaulting to `~/<name>`.

Refuse to attach into a non-empty directory without an explicit
`--allow-nonempty`, so the conflict-copy noise of §2.4 is opt-in rather than a
surprise.

### P3 — Back the profile up to Drive

**Problem:** modes, pins, mountpoint, and cache budget exist only on the dead
machine, so even a perfect P2 restores folders in the wrong mode with no pins.

Write a small JSON document into a hidden folder in the user's own Drive — say
`.proton-drive-linux/profile.json` — containing, per device:

- sync folders: remote uid, the local path last used, and mode
- pinned node uids
- mountpoint and cache budget

It rides on Drive's own encryption, so this introduces no new crypto and no new
place for plaintext to sit. Rewrite it whenever any of those change, debounced.

On a fresh install, after login: *"Found settings from `<device>`, last updated
`<date>`. Restore?"* Node uids are stable across machines, so pins restore
exactly. Local paths need remapping when the username or home differs — show the
old path, default the new one to `$HOME/<basename>`.

Store the last-used local path only as a *hint*. Restoring a profile must never
write to a path the user has not confirmed in this session.

### P4 — Make the unrecoverable visible

Nothing above helps with §1's undrained writes, because by then it is too late.
The mitigation is to make the pending queue impossible to miss *before* the
machine is lost:

- pending count and bytes in `pdfs status`, using the existing `pending` map and
  `TransferRegistry`
- a tray icon state and a GUI banner while the queue is non-empty
- a warning on logout or shutdown while it is non-empty

### P5 — Verification

None of the above is trustworthy without exercising the actual shape of the
disaster. The end-to-end test is: register a device, sync a folder, upload
content, **delete the local state directory and cache entirely** (simulating the
lost machine), restart, restore, and assert the tree comes back byte-identical.

That is a two-account or at least two-state-dir integration test against a live
account, not a unit test. Until the mock-backend problem described in
`audit-plan.md` is solved, it stays manual — but it should be run, and its result
recorded here, before P1–P3 are called done.

---

## 4. Current status

| Phase | State |
|---|---|
| Manual runbook (§2) | Works, with the hostname and basename caveats |
| P1 explicit adoption | Not started |
| P2 discover + attach | Not started |
| P3 profile on Drive | Not started |
| P4 pending visibility | Not started |
| P5 restore drill | Never run |
