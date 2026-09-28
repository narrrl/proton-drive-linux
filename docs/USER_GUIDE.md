# User guide

How Proton Drive for Linux works and how to use it day to day. For installation see
[INSTALL.md](INSTALL.md); every command mentioned here is described in [CLI.md](CLI.md).

- [How it works](#how-it-works)
- [My files](#my-files)
- [Keeping files on this computer](#keeping-files-on-this-computer)
- [Syncing local folders](#syncing-local-folders)
- [Conflicts](#conflicts)
- [Pausing and limiting sync](#pausing-and-limiting-sync)
- [Photos](#photos)
- [Sharing](#sharing)
- [Trash and version history](#trash-and-version-history)
- [Computers](#computers)
- [Search launcher](#search-launcher)
- [Choosing how files open](#choosing-how-files-open)
- [Tray](#tray)
- [Preferences](#preferences)

## How it works

A background service, `proton-drive.service`, runs `pdfs daemon`. It holds your session, talks to
Proton Drive, and keeps your Drive mounted as a folder. Everything else is a front end that asks
the service to do things over a local socket:

| Program | What it is |
|---|---|
| `pdfs-app` | The desktop app: files, photos, sharing, sync, preferences |
| `pdfs-tray` | Tray icon with status, pause and quick actions |
| `pdfs-prompt` | A search launcher for a keyboard shortcut |
| `pdfs` | The command-line client, for everything above and for scripts |

Your Drive, your file names and your photos are end-to-end encrypted by Proton. On this computer,
the service must decrypt what it shows you, and it keeps a decrypted cache and index on disk. Use
full-disk or home-directory encryption. [ARCHITECTURE.md §8](ARCHITECTURE.md#8-threat-model-what-this-client-writes-to-disk-in-plaintext)
lists exactly what is stored.

The app uses these words consistently, and so does this guide:

| Term | Meaning |
|---|---|
| **My files** | The root of your Drive |
| **Proton Drive folder** | Where My files appears on disk, `~/ProtonDrive` by default |
| **Online only** | A file or folder whose content downloads when opened |
| **Available offline** | A single file kept on this computer |
| **Synced folder** | A local folder with a full copy on this computer, backed up to Proton Drive |
| **Proton Drive service** | The background service (`proton-drive.service`) |

## My files

My files is mounted at `~/ProtonDrive`. Every file and folder appears immediately, but content
downloads only when a program reads it:

- **Reads fetch only what is needed.** A video player that seeks, or a tool that reads a file
  header, downloads only those parts. Media streams while it plays.
- **Writes are safe offline.** When a program closes a file it wrote, the service stores the new
  content on disk and queues the upload. The queue survives network loss, restarts and crashes.
  `pdfs sync queue` lists what has not reached Proton Drive yet.
- **Recently read content is cached** on disk up to the cache size limit (5 GiB by default) and
  evicted oldest first.
- **Items shared with you** appear under `Shared with me/` inside the Proton Drive folder. Shares
  where you are a viewer are read-only.

The **My files** page of the app browses the same tree without going through the mount. It has a
grid and a list view, sorting, thumbnails (including camera RAW previews), multi-select and drag
and drop upload. **Details** (Alt+Enter) shows size, dates, sharing and version history for the
selected item.

Symbolic links, hard links, device files and FIFOs cannot be stored on Proton Drive. Programs that
try to create them get a clean error, not a hang or `EIO`.

## Keeping files on this computer

Turn on **Make available offline** for a file in My files, or run `pdfs pin <path>`. The file is
downloaded completely and never evicted from the cache. `pdfs pins` lists these files, and
**Preferences → Storage → Available offline** shows them in the app.

Turning it off (`pdfs unpin <path>`) frees the space. The file stays on Proton Drive.

## Syncing local folders

A synced folder is an ordinary folder on this computer, for example `~/Documents`, that is
backed up to Proton Drive under this computer's name. It appears on the **Computers** page of the
app and in the web app.

- Add one with **Add Folder to Sync** on the **Computers** page, or `pdfs sync add ~/Documents`.
- Changes flow both ways. Local edits upload a few seconds after the file is closed; remote edits
  download on the next pass.
- A file that another program still holds open for writing is not uploaded until it is closed.
- **Sync → Folders** in the app, and `pdfs sync list`, show every folder and its state.

Each synced folder can also be switched to **Online only**. The local copy is replaced by a mount
of the folder, so the files stay visible but take no disk space until opened. Switch with the
**Synced / Online only** switch on the folder's card, or `pdfs sync mode <id> ondemand` and back
with `pdfs sync mode <id> mirror`. `pdfs locations` lists every local place the service occupies.

Removing a synced folder (**Stop Syncing** or `pdfs sync rm <id>`) keeps the local files and the
copy on Proton Drive. Add `--delete-remote` (or tick **Also delete from Proton Drive**) to delete
the copy on Proton Drive as well.

### Excluding files with `.pdfsignore`

Synced folders skip build output, dependency folders and editor leftovers. Rules come from two
places, and both apply:

1. A `.pdfsignore` file at the root of the synced folder (`.protonignore` also works), using
   gitignore syntax, including `!` to re-include:

   ```gitignore
   build/
   *.log
   !important.log
   ```

2. The global `ignore_patterns` list in `config.json` (see
   [CONFIGURATION.md](CONFIGURATION.md#ignore_patterns)). When unset, it defaults to `.git/`,
   `.hg/`, `.svn/`, `node_modules/`, `target/`, `.venv/`, `__pycache__/`, `*~`, `*.swp`, `*.tmp`,
   `.DS_Store` and `Thumbs.db`.

Rules are re-read on every pass. Ignoring never deletes anything: a file that becomes ignored is
simply no longer tracked, and its copy on Proton Drive stays.

## Conflicts

When the same file changed on this computer and somewhere else since the last sync, neither side
is thrown away. The local version is kept as a copy named like
`report (sync-conflict 1759068180).odt`, where the number is the time of the conflict in seconds
since 1970, and the remote version takes the original name.

Conflicts appear under **Sync → Overview → Needs attention** and in `pdfs conflicts`. For each
one, keep the original, keep the copy, or keep both under a new name:

```bash
pdfs conflicts
pdfs conflicts resolve "Documents/report (sync-conflict 1759068180).odt" --keep copy
```

The version you do not keep goes to the Trash, so a wrong choice can be undone.

## Pausing and limiting sync

- **Pause** from the tray, the Sync page, or `pdfs sync pause`. Uploads and synced folders wait;
  reading files keeps working. `pdfs sync resume` continues.
- **Limit bandwidth** under **Preferences → General → Network**, or with
  `pdfs sync limit --up 2M --down 10M`. `0` removes a limit.
- **Retry** a failed upload from **Sync → Overview**, or with `pdfs sync retry`.

## Photos

The **Photos** page shows your Proton Photos library. Photos are stored separately from My files
and do not appear in the Proton Drive folder.

- **Timeline** groups photos by day, month or year as you zoom. Drag the scrubber on the right edge
  to jump through the years. **On this day** shows photos from the same date in earlier years.
- **Filters** narrow the timeline to images, videos or RAW files, to favorites, or to photos that
  are in no album.
- **RAW + JPEG pairs** of the same shot show as one tile. The viewer switches between them, and
  moving the tile to the Trash removes both.
- **Albums** lists your albums and albums shared with you. Create, rename and delete albums, and
  add or remove photos.
- **Places** groups photos by the town they were taken in, as cards or on a map. Towns come from
  bundled [GeoNames](https://www.geonames.org/) data (CC BY 4.0) and the map from
  [Natural Earth](https://www.naturalearthdata.com/) (public domain), so nothing is fetched online.
  An optional street map from [OpenFreeMap](https://openfreemap.org/) (© OpenStreetMap
  contributors) can be enabled under **Preferences → General → Appearance**. It is off by default
  because the tile server learns which areas you look at.
- **Viewer**: favorites, EXIF details, show on map, slideshow (F5).
- **Find Duplicates…** in the page menu lists byte-identical copies. The **Similar** switch also
  finds resized or re-encoded copies and burst shots by comparing thumbnails.
- **Upload** adds photos. Its menu holds **Import from Google Photos…**.

### Importing from Google Photos

Request a Google Takeout export of Google Photos, then pass every `.zip` of it at once. A photo's
metadata often sits in a different part of the export than the photo itself.

```bash
pdfs import-google-photos --dry-run ~/Downloads/takeout-*.zip   # see what would happen
pdfs import-google-photos --wait ~/Downloads/takeout-*.zip
```

Albums are recreated, capture dates and locations are taken from the metadata, and photos already
on the account are skipped, so an interrupted import can be run again. `pdfs import-status` and
`pdfs cancel-import` follow or stop an import that runs in the background.

If photos from an earlier import are filed under the day they were uploaded,
`pdfs redate-photos --dry-run` finds those whose file name carries a different date, and
`pdfs redate-photos` fixes them. Proton cannot change a capture date in place, so each fix
re-uploads the photo and moves the original to the Trash, keeping favorites and albums.

## Sharing

- **Share…** on a file or folder invites people by email as viewer or editor, or creates a public
  link with an optional password and expiry. `pdfs share`, `pdfs members`, `pdfs share-role`,
  `pdfs unshare` and `pdfs public-link` do the same from a terminal.
- **Shared by me** lists everything you share, filtered by links, people or expired links. Copy a
  live link or stop several shares at once.
- **Shared with me** lists what others share with you, with who shared it and your access.
  Pending invitations wait in a banner; `pdfs invitations` accepts or rejects them. Saved public
  links (`pdfs bookmarks`) have their own view.

## Trash and version history

The **Trash** page lists trashed items with the date they were deleted. Restore puts an item back
where it was, together with trashed folders above it. **Delete Forever** and **Empty Trash** are
permanent. The CLI equivalents are `pdfs trash`, `pdfs restore`, `pdfs delete-forever` and
`pdfs empty-trash`.

Proton Drive keeps earlier versions of every file. **Details → Versions…** in the app, or
`pdfs versions list <path>`, shows them. You can:

- **Restore** a version. This happens on the server; nothing is re-uploaded.
- **Save** a version to a separate local file without touching the current one.
- **Delete** an old version permanently. The current version cannot be deleted.

## Computers

**Computers** lists this computer and every other computer that backs up to your account.

- **This computer** shows its synced folders with their state, and **Restore Folders…** brings
  back folders that are backed up under it but not synced here yet.
- **Another computer** can be browsed read-only in the same list or grid as My files, and
  **Restore to This Computer…** downloads its folders here.
- **Continue This Backup Here…** makes this computer take over another computer's backup, for
  example after reinstalling or replacing a machine. See [RECOVERY.md](RECOVERY.md).

## Search launcher

`pdfs-prompt` searches Proton Drive and your home folder together, in one ranked list. It
tolerates prefixes, abbreviations, typos and swapped letters, and matches on parent folder names
as well. Bind it to a keyboard shortcut in your desktop settings.

The window stays loaded after the first use, so later shortcuts open it instantly. Folders, audio
and video open straight from the Proton Drive folder, so players can start before the whole file
downloads. Other Drive files download first, then open.

### Using your own launcher

`pdfs-prompt --dmenu` shows the results in fuzzel, rofi, wofi, tofi, bemenu or dmenu, whichever is
installed:

```bash
pdfs-prompt --dmenu
pdfs-prompt --dmenu --menu 'fuzzel --dmenu --width 60'
pdfs-prompt --dmenu --query invoice
```

A dmenu-style launcher filters a fixed list and cannot ask for new results per keystroke, so the
search takes two steps. The first list, `Search Drive ›`, shows your available-offline files; type
a query and press Enter to search. The second, `Drive: invoice ›`, shows the results; Enter opens
one, and typing something else searches again.

### Search as you type with fzf

`pdfs-prompt --fzf` re-queries the service on every keystroke, like the built-in window. When
started from a keybinding without a terminal, it opens one itself (foot, ghostty, kitty,
alacritty, wezterm or xterm) with the app ID `pdfs-prompt`, so one window rule covers all of them.
For Hyprland:

```ini
windowrulev2 = float, class:^(pdfs-prompt)$
windowrulev2 = size 900 500, class:^(pdfs-prompt)$
windowrulev2 = center, class:^(pdfs-prompt)$
bind = SUPER, space, exec, pdfs-prompt --fzf
```

To make `--dmenu` or `--fzf` the default without changing your keybinding, set `prompt.mode` in
`config.json` ([CONFIGURATION.md](CONFIGURATION.md#prompt)). `--gtk` forces the built-in window.

## Choosing how files open

Files open with `xdg-open` by default. The `open_with` section of `config.json` overrides that per
file type, for example text in Neovim inside a terminal. The rules apply to the search launcher
and to the app. See [CONFIGURATION.md](CONFIGURATION.md#open_with).

## Tray

`pdfs-tray` starts at login and shows whether files are up to date, syncing, paused or offline.
Its menu opens the app or the Proton Drive folder, pauses or resumes sync, and stops or connects
the service. **Hide Tray Icon** removes it until you turn it back on under
**Preferences → General → Appearance → Show tray icon**.

GNOME needs the AppIndicator extension to show tray icons.

## Preferences

| Page | Settings |
|---|---|
| **General** | Start on login · Proton Drive folder location · upload and download limits · Proton theme, street map for Places, tray icon · language |
| **Storage** | Cache size limit · clear the cache · files kept available offline |

The account menu shows storage used and holds **Sign Out…**. Settings not exposed in the app are
listed in [CONFIGURATION.md](CONFIGURATION.md).
