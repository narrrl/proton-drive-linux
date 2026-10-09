# Command-line reference

`pdfs` controls the Proton Drive service from a terminal and from scripts. Almost every command
talks to the running service (`proton-drive.service`); `login`, `logout`, `mount`, `daemon` and
`diagnose` work without it.

`pdfs <command> --help` prints the full help of any command. This page groups them by task.

- [Conventions](#conventions)
- [Account and service](#account-and-service)
- [Files](#files)
- [Available offline](#available-offline)
- [Synced folders](#synced-folders)
- [Conflicts](#conflicts)
- [Computers](#computers)
- [Trash](#trash)
- [Versions](#versions)
- [Sharing](#sharing)
- [Photos](#photos)
- [Activity and transfers](#activity-and-transfers)
- [Maintenance and diagnostics](#maintenance-and-diagnostics)
- [Scripting with `--json`](#scripting-with---json)

## Conventions

- **Paths** name items in My files. They may be absolute paths inside the Proton Drive folder
  (`~/ProtonDrive/Documents/a.pdf`) or relative to it (`Documents/a.pdf`).
- **UIDs** identify items that have no path in the mount: photos, trashed items, shared items and
  computers. The listing commands print them.
- **Exit codes**: `0` on success, non-zero on any failure, including an error reported by the
  service.
- **Output** is English and stable enough for `grep`; use `--json` where it is available.

## Account and service

| Command | Description |
|---|---|
| `pdfs login [-u EMAIL]` | Sign in (password, then a 2FA code if the account has one) and store the session in the system keyring. Restarts the service if it is running. |
| `pdfs logout` | Remove the stored session. |
| `pdfs status` | Account, mount, queue and cache state. |
| `pdfs quota` | Storage used across all Proton products. |
| `pdfs locations` | Every local place the service occupies: the Proton Drive folder and each synced folder, with mode and state. |
| `pdfs daemon [MOUNTPOINT]` | Run the service in the foreground: wait for a session, mount, stay mounted. `proton-drive.service` runs this. |
| `pdfs mount [MOUNTPOINT]` | Mount once and block until unmounted. Defaults to `~/ProtonDrive`. |

`pdfs login` cannot show a CAPTCHA. If Proton asks for one, sign in with `pdfs-app` instead.

## Files

| Command | Description |
|---|---|
| `pdfs ls [PATH]` | List a folder. |
| `pdfs search QUERY [--in FOLDER] [--limit N]` | Search names in the local index. |
| `pdfs mkdir PARENT NAME` | Create a folder. |
| `pdfs rename PATH NEW_NAME` | Rename a file or folder. |
| `pdfs move PATH... NEW_PARENT` | Move one or more files or folders into another folder. The two may be in different locations (My files, an on-demand or a mirrored synced folder); the move happens on Proton Drive, without downloading or uploading anything. |
| `pdfs rm PATH` | Move to the Trash. |
| `pdfs upload SOURCES... [-t FOLDER]` | Upload local files and folders (recursively) in the background. Follow with `pdfs transfers`. |
| `pdfs refresh [TARGET] [--full]` | Forget a cached listing so the next read fetches it again. `TARGET` is a folder, `trash` or `photos`; `--full` with `photos` re-reads every photo's metadata. |

Ordinary file tools (`cp`, `mv`, `rsync`, editors) work on the Proton Drive folder directly. These
commands are for scripts and for cases where the mount is not available.

## Available offline

| Command | Description |
|---|---|
| `pdfs pin PATH` | Download a file completely and keep it on this computer. |
| `pdfs unpin PATH` | Release it back to online only. |
| `pdfs pins` | List files kept available offline. |

## Synced folders

| Command | Description |
|---|---|
| `pdfs sync add PATH` | Back up a local folder under this computer and keep it in sync. |
| `pdfs sync list` | List synced folders with their IDs and state. |
| `pdfs sync rm ID [--delete-remote]` | Stop syncing. Local files stay; `--delete-remote` also removes the copy on Proton Drive. |
| `pdfs sync mode ID mirror\|ondemand` | Switch between a full local copy (`mirror`) and online only (`ondemand`). |
| `pdfs sync now [ID]` | Start a sync pass now, for all folders or one. |
| `pdfs sync pause [--for 1h] [--folder ID]` | Pause uploads and synced folders. Reading files keeps working. `--for` ends the pause by itself (`30m`, `1h`, `2d`). |
| `pdfs sync resume [--folder ID]` | Resume after a pause. |
| `pdfs sync limit [--up RATE] [--down RATE]` | Cap bandwidth, for example `500K` or `2M` per second. `0` removes a cap. Without options, prints the current caps. |
| `pdfs sync queue` | List changes that have not reached Proton Drive yet. |
| `pdfs sync issues` | List queued changes Proton Drive refused or that keep failing, and what to do. |
| `pdfs sync export ID DEST` | Save a copy of a queued upload's content. `DEST` may be a folder; an existing file is never overwritten. The upload stays queued. |
| `pdfs sync discard ID [--yes]` | Drop a queued change and undo it here: a new file or folder goes, anything else goes back to what Proton Drive has. Content not exported first is lost. |
| `pdfs sync retry [ID]` | Retry one queued operation, or every failed one. |
| `pdfs sync restore [--device UID] [--yes]` | Re-attach this computer's backed-up folders to local folders and download them. `--device` restores another computer's folders. See [RECOVERY.md](RECOVERY.md). |

## Conflicts

| Command | Description |
|---|---|
| `pdfs conflicts` | List `(sync-conflict …)` copies. |
| `pdfs conflicts resolve PATH --keep original\|copy\|both [--name NAME]` | `original` trashes the copy; `copy` trashes the original and gives the copy its name; `both` renames the copy to `--name`. |

## Computers

| Command | Description |
|---|---|
| `pdfs devices list` | List computers registered to the account. |
| `pdfs devices rename UID NAME` | Rename a computer. |
| `pdfs devices rm UID` | Remove a computer and its backup from the account. |
| `pdfs devices adopt UID` | Make this machine continue another computer's backup, for example after a reinstall. `--clear` returns to matching by hostname. |
| `pdfs devices migrate UID [--yes]` | Move another computer's folders into this computer's backup, then restore them here like `pdfs sync restore`. The other computer stays registered, without those folders. `--yes` skips the confirmation and accepts every proposed path. See [RECOVERY.md](RECOVERY.md). |

## Trash

| Command | Description |
|---|---|
| `pdfs trash` | List the Trash with the UIDs the next commands take. |
| `pdfs restore UIDS...` | Put items back where they were. |
| `pdfs delete-forever UIDS...` | Delete items permanently. |
| `pdfs empty-trash` | Delete everything in the Trash permanently. |

## Versions

| Command | Description |
|---|---|
| `pdfs versions list PATH` | List a file's versions, newest first. |
| `pdfs versions restore PATH ID` | Make an earlier version current. Runs on the server; nothing is re-uploaded. |
| `pdfs versions save PATH ID DEST` | Write an earlier version to a local file. Refuses to overwrite `DEST`. |
| `pdfs versions rm PATH ID` | Delete an earlier version permanently. The current version cannot be deleted. |

## Sharing

| Command | Description |
|---|---|
| `pdfs share PATH EMAILS... [--role viewer\|editor\|admin] [--message TEXT]` | Invite people to a file or folder. |
| `pdfs members PATH` | List members, pending invitations and the public link. |
| `pdfs share-role PATH ID KIND ROLE` | Change a member's or invitation's role. |
| `pdfs unshare PATH [ID KIND]` | Stop sharing, or remove one member or invitation. |
| `pdfs public-link create PATH [--role] [--password] [--expires EPOCH]` | Create a public link. `--expires` takes Unix seconds. |
| `pdfs public-link remove PATH ID` | Remove a public link. |
| `pdfs shared` | List what you share. |
| `pdfs shared-with-me [UID]` | List what others share with you, or the contents of one shared folder. |
| `pdfs shared-get UID [DEST]` | Download a file shared with you. |
| `pdfs leave UID` | Leave a share. |
| `pdfs invitations list\|accept ID\|reject ID` | Manage invitations addressed to you. |
| `pdfs bookmarks list\|add URL [--password]\|rm TOKEN` | Manage saved public links. |

## Photos

| Command | Description |
|---|---|
| `pdfs photos [--limit N] [--offset N] [--favorites]` | List the timeline, newest first. |
| `pdfs open-photo UID` | Download a photo and print its cached path. |
| `pdfs favorite UID [--remove]` | Mark or unmark a favorite. |
| `pdfs albums` | List albums, including those shared with you. |
| `pdfs album UID` | List an album's photos. |
| `pdfs new-album NAME` | Create an album and print its UID. |
| `pdfs rename-album UID NAME` | Rename an album. |
| `pdfs delete-album UID` | Delete an album. Its photos stay in the timeline. |
| `pdfs add-to-album UID PHOTOS...` | Add photos to an album. |
| `pdfs remove-from-album UID PHOTOS...` | Take photos out of an album. |
| `pdfs import-google-photos ARCHIVES... [--dry-run] [--wait]` | Import a Google Takeout export. Pass every `.zip` of the export. |
| `pdfs import-status` / `pdfs cancel-import` | Follow or stop a running import. |
| `pdfs redate-photos [--dry-run] [--from DATE] [--to DATE] [--limit N] [--wait]` | Fix photos filed under their upload day when the file name carries the real date. |
| `pdfs redate-status` / `pdfs cancel-redate` | Follow or stop a running re-date. |

## Activity and transfers

| Command | Description |
|---|---|
| `pdfs activity [-l N]` | Recent activity, newest first. |
| `pdfs transfers` | Uploads and downloads in progress. |

## Maintenance and diagnostics

| Command | Description |
|---|---|
| `pdfs diagnose` | Check the installation and print a report. Works without the service. Exits non-zero if a check fails. |
| `pdfs diagnostics` | What a running service is doing now: worker threads, queues, requests in flight, memory. For a service that has stopped answering. |
| `pdfs cache inspect [--deep]` | Database size, reclaimable space, row counts, cache use against the limit. `--deep` also runs SQLite's integrity check (slow). |
| `pdfs cache vacuum` | Compact the database. Needs free space for a second copy of it while it runs. |
| `pdfs cache clear` | Delete cached file content. Files kept available offline stay. |

[TROUBLESHOOTING.md](TROUBLESHOOTING.md) explains when to use each of these.

## Scripting with `--json`

The global `--json` flag makes query commands print JSON: `status`, `ls`, `pins`, `sync list`,
`devices list`, `locations`, `transfers`, `activity`, `trash` and `cache inspect`. Commands that change
something keep their human-readable output; check their exit code.

```bash
pdfs --json status | jq -r '.mount.mountpoint'
pdfs --json sync list | jq -r '.items[] | select(.state != "idle") | .local_path'
pdfs --json cache inspect | jq '.db_reclaimable_bytes'
```

- The payload is unwrapped: a list is `{"items": [...]}`, never the service's internal message
  name.
- An error from the service is printed as JSON with a machine-readable `kind` and still exits
  non-zero, so `set -e` and `if pdfs …` behave as expected.
