# User guide

How Proton Drive for Linux works and how to use it day to day. To install it, see
[Installation](INSTALL.md). If you are new, [Getting started](GETTING_STARTED.md) is the quicker
read.

## Topics

| Page | What it covers |
|---|---|
| [Files and offline access](FILES.md) | The `~/ProtonDrive` folder, keeping files available offline, the Trash, earlier versions |
| [Synced folders](SYNCED_FOLDERS.md) | Backing up local folders, excluding files, changes waiting to upload, conflicts, pausing |
| [Photos](PHOTOS.md) | Timeline, albums, places, duplicates, importing from Google Photos |
| [Sharing](SHARING.md) | Invitations, public links, items shared with you |
| [Computers](COMPUTERS.md) | Other computers in your account, syncing several computers |
| [Search launcher and tray](LAUNCHER_AND_TRAY.md) | Finding files from a keyboard shortcut, the tray icon |

Every command mentioned in these pages is listed in the
[command-line reference](CLI.md).

## How it works

A background service, `proton-drive.service`, runs `pdfs daemon`. It holds your session, talks to
Proton Drive, and shows your Drive as a folder. Everything else asks the service to do things:

| Program | What it is |
|---|---|
| `pdfs-app` | The desktop app: files, photos, sharing, sync, preferences |
| `pdfs-tray` | Tray icon with status, pause and quick actions |
| `pdfs-prompt` | A search launcher for a keyboard shortcut |
| `pdfs` | The command-line client, for everything above and for scripts |

Your files, file names and photos are end-to-end encrypted by Proton. To show them to you, the
service decrypts them on this computer and keeps a cache, which is not encrypted. Use full-disk or
home-folder encryption. The [security model](SECURITY_MODEL.md#what-is-kept-on-this-computer)
lists exactly what is stored.

## Words used in the app

The app uses these words, and so do these pages:

| Term | Meaning |
|---|---|
| **My files** | Everything in your Proton Drive, apart from Photos and computer backups |
| **Proton Drive folder** | Where My files appears on this computer, `~/ProtonDrive` by default |
| **Online only** | A file or folder that downloads when you open it |
| **Available offline** | A file, or a folder with everything in it, kept on this computer |
| **Synced folder** | A local folder with a full copy on this computer, backed up to Proton Drive |
| **Proton Drive service** | The background service (`proton-drive.service`) |

## Preferences

| Page | Settings |
|---|---|
| **General** | Start on login · Proton Drive folder location · upload and download limits · Proton theme, street map for Places, tray icon · language · check for updates |
| **Storage** | Cache size limit · clear the cache · files kept available offline |

The account menu shows the storage you use and holds **Sign Out…**. Settings that are not in the
app are described in [Configuration](CONFIGURATION.md).
