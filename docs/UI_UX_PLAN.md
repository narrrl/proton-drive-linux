# UI/UX audit and improvement plan

Audit date: 2026-09-23. Scope: `pdfs-gui` (all pages, dialogs, photo viewer), `pdfs-tray`, and the
backend capabilities the UI would need. File references are to `crates/pdfs-gui/src/app/` unless
stated otherwise.

## 1. Summary

The client is functionally rich but the UI is built page by page, and it shows:

- **No sync control surface.** There is no way to pause or resume sync, no list of what is stuck or
  failing, and no conflict overview. The backend has no pause at all, no per-op listing, and no
  per-file conflict record (conflicts exist only as `(sync-conflict <ts>)` files plus activity rows).
  The GUI reads `online`, `pending_uploads` and `pending_changes` from `Status` and ignores
  `failing_ops`, `failing_error`, `parked_uploads` and `staged_*`.
- **Information architecture is scattered.** Live transfers are a group on *Settings* while the
  *Activity* page is history only. *Locations* and *Computers* point at each other. Import lives under
  Settings but is launched from Gallery and returns to Settings. Sign-out, quota, mount status,
  pinned files, import and developer info all share the Settings page.
- **Chrome is non-standard.** Every page draws its own title and toolbar row under an almost empty
  `AdwHeaderBar` whose title is always "Proton Drive". Two stacked bars, wasted height, no current
  folder in the title.
- **Icons and wording drift.** Upload uses `document-send-symbolic` (reads as "send"), offline uses
  stars (reads as "favourite"), trash icons mean "stop syncing", "remove device" and "remove
  bookmark". "Keep offline" / "Unpin" / "Remove offline copy" / "Available offline" are one concept.
  "Computers" vs "device", "Gallery" vs "Photos", "mount service" vs "daemon" vs "Connect".
- **Theming is hardcoded.** `PROTON_PURPLE` overrides the system accent; badge, viewer and tile
  colours are hex/rgba literals and ignore light, dark and high-contrast modes.
- **Several visible bugs** (section 9).

## 2. Design principles

1. **Follow GNOME HIG and current libadwaita.** The system has libadwaita 1.9; the crate is pinned to
   feature `v1_5`. Raise it to at least `v1_7` to get `adw::Spinner`, `ToggleGroup`,
   `InlineViewSwitcher`, `ButtonRow`, `WrapBox`, `BottomSheet`, and (1.8) `ShortcutsDialog`.
2. **Sync state is first class.** The user must always see: up to date / syncing / paused / offline /
   needs attention, and be one click from pausing or from the list of problems.
3. **One term, one icon, one place** per concept (glossary in section 8).
4. **Secondary and destructive actions go into menus.** Rows get at most one visible suffix action
   plus a `view-more-symbolic` menu. Destructive actions are red inside the menu and in the confirm
   dialog, not as filled buttons in headers.
5. **Never block on the network visibly.** Keep old content on screen while reloading, show a header
   spinner, never flash the whole page to a "Loading…" status page.
6. **Respect the system.** Use the system accent by default (brand purple stays on login, app icon
   and an opt-in toggle). Use named colours (`@success_color`, `@warning_color`, `@error_color`).

## 3. New information architecture

### Sidebar

```
Proton Drive                     (header: app name)
  My files
  Photos
  Shared with me        (badge: pending invitations)
  Shared by me
  Computers
  Trash
─────────────
  Sync                  (badge: issue count, icon reflects state)
─────────────  footer
  [●] Up to date · Pause          ← sync status strip, click opens Sync
  ▓▓▓░░░ 269 GiB of 1.6 TiB       ← quota
  (avatar) user@proton.me   ⋮     ← account menu: Preferences, Sign out…
```

- **Remove** sidebar rows: *Locations* (merged into Sync), *Activity* (becomes the Sync "History"
  view), *Settings* (becomes an `AdwPreferencesDialog`, `Ctrl+,`).
- **Rename** *Gallery* to *Photos* and *Shared* to *Shared by me*.
- Primary menu: Preferences, Keyboard Shortcuts, About. The About dialog's `debug_info` gets app
  version, user agent, daemon status and config path (replacing the "Developer" group).

### Sync page (new, replaces Locations + Activity + Settings→Activity)

Top: an `AdwBanner`-style status card with state icon, one-line summary ("Syncing 12 files · 3.4
MB/s", "Paused until you resume", "2 files need attention") and one primary button: **Pause** /
**Resume** (split button with "Pause for 1 hour / until tomorrow / until resumed").

Below, an `InlineViewSwitcher` with four views:

1. **Issues** (default when non-empty): conflicts, failing uploads (`last_error`, attempts, next
   retry), folders in `error`/`conflict` state, parked uploads older than N minutes. Each row has a
   primary fix action (Resolve…, Retry now) and a menu (Show in folder, Discard change…).
2. **Transfers**: in-flight uploads and downloads with progress, speed, cancel; background jobs
   (thumbnail build, Takeout import, re-date) with their own cancel.
3. **Folders**: the old Locations list. Row: path, mode as a dropdown suffix (Synced / On-demand, with
   confirm when going on-demand deletes local copies), state subtitle, `Sync now` and a menu (Open,
   Change location…, Pause this folder, Stop syncing…). "Add folder…" as a `ButtonRow` at the end.
   The primary mount appears first and is not removable.
4. **History**: the old Activity feed, with a kind filter, failures in `@error_color`, rows
   activatable (open or reveal the target), and "Clear history".

### Preferences dialog (slimmed Settings)

| Page | Contents |
|---|---|
| General | Start on login; Mount location (path + Change…); Notifications (on sync issues, on import done) |
| Storage | Cache usage bar; Cache budget (spin row, in GB, "Unlimited" switch instead of magic 0); Clear cache…; Offline files (count + "Manage" → Sync › Folders filter or a sub-page) |
| Sync | Ignore patterns (editable list); Conflict clean-up: Off / Report / Remove identical copies (`conflict_sweep`); later: bandwidth limits |
| Advanced | Open-with rules, search prompt config (only if kept in GUI) |

Moved out: account row and Sign out (sidebar account menu, with confirm), quota (sidebar footer),
mount status (sidebar status strip), live transfers (Sync), Google Photos import (Photos page menu),
version/user agent (About → Debug info).

## 4. Backend work required

Everything below is new; nothing like it exists today.

### B-1 Pause and resume (global, then per folder)

- `Request::SetSyncPaused { paused: bool, until: Option<i64> }`, persisted (config key
  `sync_paused_until` or a DB setting) so a pause survives restarts; `Response::Status` gains
  `paused` and `paused_until`.
- Drain: `run_pending_drain` (`pdfs-fuse/src/drain.rs:265`) already releases claimed ops while
  offline; gate on `paused` at the same point and reuse the wake path.
- Sync engine: add `SyncMsg::Pause/Resume` or gate `reconcile_all` / `reconcile_folder`; the 120 s
  poll and watcher keep recording, they just do not apply.
- FUSE reads must keep working while paused (on-demand hydration is a read, not a sync); document that
  pause means "no uploads, no mirror reconcile".
- Per folder later: `sync_folder.paused` column, skipped in `reconcile_all`.
- Bandwidth limits later: token bucket in `CountingReader` / `CountingWriter`
  (`pdfs-fuse/src/transfers.rs`).

### B-2 Pending ops and transfers

- `Request::ListPendingOps` → id, kind, path/name, attempts, `last_error` (classified as
  `ErrorKind`), `next_attempt_at`, parked flag, staged bytes.
- `Request::RetryOp { id }` (set `next_attempt_at = now`, wake drain) and `RetryAllFailing`.
- `Request::DiscardOp { id }` for a create/revision whose bytes should be dropped. Must save the
  staged blob to a local recovery folder first; never silent data loss (see `docs/RECOVERY.md`).
- `TransferItem` gains a stable id and `Request::CancelTransfer { id }`.
- Per-folder last error message on `SyncFolderInfo` (today the text only exists in the activity feed).

### B-3 Conflicts

- `Request::ListConflicts`: walk nodes whose name parses with `sweep.rs` `conflict_base_name`, for
  the mount and every mirror folder. Return copy path, original path (if it exists), both sizes and
  mtimes, whether content is identical (size + `content_sha1`, as the sweep already proves), and
  which side is local vs remote when known.
- Optionally a `conflict` table written by `keep_as_conflict_copy` / `preserve_conflict_copy` so the
  origin (drain vs mirror, device) is recorded instead of inferred from names.
- `Request::ResolveConflict { copy, keep: Original | Copy | Both }`:
  - Original: trash the copy (re-verify like the enforcing sweep does).
  - Copy: upload the copy's content as a new revision of the original, then trash the copy (the old
    content stays in version history, so this is reversible).
  - Both: rename the copy to a user-chosen name, drop the `(sync-conflict …)` stamp.
- Activity rows for each resolution.

## 5. Shell and visual polish (all pages)

- **Per-page header bars.** Give each stack page its own `adw::ToolbarView` + `adw::HeaderBar` (or
  inject page widgets into the shared bar on page switch). Title = page name, or the current folder
  in My files. Page actions move into the header; the extra toolbar row disappears.
- **One page skeleton.** Same margins everywhere (today 12 vs 18), same `Clamp` rule (lists that are
  settings-like clamp at 720, file/photo grids are full width), same status pages (`compact`,
  `vexpand`, crossfade), same Retry semantics (Retry reloads; "Start Proton Drive" is a separate,
  explicitly labelled button when the service is down).
- **Loading.** `adw::Spinner` in the header; keep the old model until the new one lands; status page
  only on first load.
- **Theme.** Drop the global `accent_*` override; offer "Use Proton purple accent" in Preferences.
  Replace hex colours with named colours; test light, dark and high contrast.
- **Move the 70-line CSS string** in `main.rs:232` to `resources/style.css` in the gresource
  (`style-dark.css` if needed), so it can be edited and linted.
- **Shortcuts.** Replace the hand-rolled cheatsheet with `adw::ShortcutsDialog` (or
  `GtkShortcutsWindow` before 1.8) listing all bindings, including F5/Ctrl+R and the viewer keys.
- **Toasts vs dialogs.** Dialogs report their own errors inline (versions, share, add bookmark); a
  toast behind a modal is invisible. Pluralise properly (`ngettext`-ready helpers, no "item(s)").
- **i18n groundwork.** Wrap user strings in a `gettext!`-style macro now, even without translations,
  so later localisation is mechanical.

## 6. Page-by-page plan

### My files

Header: `[‹ ›]  Path bar (breadcrumbs, scroll to end, Ctrl+L to edit)  … [search] [view ▾] [+ New ▾]`

- **New ▾** (`list-add-symbolic`, suggested): New folder (Ctrl+Shift+N), Upload files (Ctrl+U),
  Upload folder. Replaces three separate icons, fixes the "send" icon.
- **View ▾** split button: Grid/List (Ctrl+1/2), Sort by Name/Size/Modified, ascending/descending,
  Folders first, zoom slider. Persist choice (config or GSettings).
- **Folder ⋮ menu** (in header or background context menu): Refresh, Build thumbnails, Make folder
  available offline, Open in file manager, Share folder…. Build thumbnails leaves the main toolbar.
- **Navigation.** Back/forward history (Alt+←/→), Up (Alt+↑); searching then pressing Back clears
  the search consistently.
- **Context menus** as `gtk::PopoverMenu` from `gio::Menu` + actions: keyboard navigable, Menu /
  Shift+F10, same action set as details pane. Add a background menu. Order: Open, Open with…, Play
  (media) · Available offline (check item) · Share…, Copy link · Rename…, Move to…, Versions… ·
  Move to Trash (destructive).
- **Offline state.** One concept "Available offline", toggle semantics. Badges: cloud-only = no
  badge or subtle `cloud-outline`; downloaded = check; available offline = pin/`folder-download`
  style filled icon. Use named colours. Folders can be made available offline too, or the toggle is
  hidden for them (fix `details.rs:259`).
- **List view.** Sortable, resizable columns: Name, Status, Size, Modified (date + time), and
  Location for search results. Rubberband selection. One selection model shared between views.
- **Details pane.** On wide windows a real side pane (breakpoint, not overlay); header bar with
  close; real thumbnail; sections: Info, Offline, Sharing (people count, link), Versions (latest 3 +
  "All versions…"). Same actions as the context menu.
- **Selection bar** (≥1 selected, not ≥2): count + size, Available offline, Move to…, Share (single),
  Move to Trash; Select all (Ctrl+A). Drop the confirm dialog for trash since Undo exists (trash is
  reversible), or keep confirm and drop Undo — not both.
- **Drag and drop.** Drop external files/folders to upload (reuse Takeout's `FileList` target), drag
  the whole selection, highlight drop targets, breadcrumbs accept drops, Undo toast on move.
- **Move to…** a folder-picker dialog (tree/list of Drive folders) instead of a free-text path.
- **Rename** pre-selects the stem, not the extension; drop the redundant "Rename “x”." body.
- **Search.** Scope toggle (This folder / Everywhere), "Showing first 200 results" note, Location
  column, correct cached badges (search hits hardcode `cached: false`).
- **Upload feedback.** Toast "Uploading 3 items" with "View" → Sync › Transfers.

### Photos (was Gallery)

- Header: title "Photos", `InlineViewSwitcher` Timeline | Albums in the header, Select toggle,
  `+ Upload` (fix: use `adw::ButtonContent`, today `.icon_name()` overwrites `.label()` at
  `photos.rs:426-432`), ⋮ menu: Import from Google Photos…, Refresh, Rebuild library.
- Filters: a single filter dropdown/`ToggleGroup` (All, Photos, Videos, RAW, Favourites) instead of
  two segmented rows where "Photos" appears twice. Counts follow the active filter.
- **Month scrubber** on the right edge that *jumps* (not filters); the month dropdown becomes a real
  "Filter by date" if kept.
- Infinite scroll with prefetch (no "Load more" pill; page size ≥ 200; also trigger when the first
  page does not fill the viewport).
- Tiles: `ContentFit::Cover` (crop) in justified rows, known aspect ratio from metadata to prevent
  reflow, placeholder as a tinted card of the final size, subtle hover (no 1.06 scale with heavy
  shadow on a 2 px gap grid), last row justified up to a max stretch.
- Context menu on tiles: Open, Favourite, Add to album, Download/Save copy, Show in My files, Move to
  Trash. Selection: Shift range, Ctrl+A, bulk Favourite, Add to album, Download.
- Empty states per filter ("No favourites yet", "No videos in June 2024") instead of the global
  "No photos yet" with upload buttons.
- Import: runs show a banner on Photos ("Importing from Google Photos — 1,204 of 5,000 · View") and
  Back returns to where the user came from.
- **Albums:** Create, rename, delete, remove from album (not trash). Fix state bugs: filter row
  hidden on the album grid, subtitle count, scroll position kept when switching back.
- **Viewer:** `adw::Window` (or full-window overlay), controls auto-hide after 2 s idle, zoom/pan
  (pinch, Ctrl+scroll, double-click), info panel beside the image instead of over the top bar,
  pages beyond the loaded 60, counter against the real total, videos play inline with
  `gtk::Video`, trash with the same confirm/undo rule as the grid, real filename in "Save a copy",
  success toast, "Open with…" app chooser, favourite tooltip reflects state, readable dates.

### Shared with me / Shared by me

- Shared with me: header title shows the drill-down path, with back; invitations as a banner at
  the top ("2 invitations · Review") opening a list, Reject confirmed; bookmarks become a separate
  view or section via a switcher. Rows: Open, Save to My files, Available offline where supported.
- Shared by me: rows activatable (open), suffix "Manage" becomes a menu with Copy link, Manage
  access…, Stop sharing….
- Share dialog: inline validation, loading spinner, confirm "Remove access" and "Delete link",
  human role names, link expiry.

### Computers

- Show the real device name for "This computer" with rename inline.
- Other computers: one suffix menu (Rename, Use this identity…, Remove…). Remove uses a destructive
  confirm that requires typing the device name since it deletes backups.
- "Restore folders" becomes a per-device action ("Restore to this computer…") with an inline-empty
  state when nothing is restorable, and moves the "Add folder" responsibility fully to Sync.
- Terminology: "computer" everywhere in UI copy.

### Trash

- Same skeleton as other list pages (clamped `boxed-list`), sort by deleted date, multi-select with
  Restore / Delete permanently, "Empty Trash…" moved into a header ⋮ menu or kept as a flat button
  with a destructive confirm, proper plurals, Retry on error.

### Login

- Human error messages mapped from error kinds, spinner in the button, "Create account" and "Forgot
  password" links, recovery-code hint in 2FA, cancelling CAPTCHA returns to the form with a message.

## 7. Tray

- Symbolic icon variants: `pdfs-synced`, `pdfs-syncing`, `pdfs-paused`, `pdfs-offline`,
  `pdfs-attention`; tooltip with the one-line state; `NeedsAttention` status for issues.
- Menu: status line · "N issues — View" (opens Sync › Issues) · Pause sync ▸ (1 h, until tomorrow,
  until resumed) / Resume · Open Proton Drive · Open folder · "Sign in…" when logged out (no
  "Connect") · Stop Proton Drive (confirm) · "Hide tray icon" instead of "Quit" (daemon keeps
  running and the label must not suggest otherwise).

## 8. Glossary (one term, one icon)

| Concept | UI term | Icon |
|---|---|---|
| Root of Drive | My files | `folder-symbolic` |
| Background service | Proton Drive (running / stopped) — never "daemon" or "mount service" in UI | — |
| Cloud-only file | Online only | none / `cloud-outline` |
| Pinned file | Available offline | pin/download-filled custom icon, not stars |
| Upload | Upload files / Upload folder | `document-upload`-style arrow up, custom pair |
| Folder sync mode | Synced / On-demand | — |
| Stop syncing a folder | Stop syncing… | `media-playback-stop-symbolic` in a menu, not trash |
| Remove a computer | Remove computer… | destructive, in menu |
| Conflict copy | Conflict | `dialog-warning-symbolic` + warning colour |
| Failure | Failed / Needs attention | `dialog-error-symbolic` + error colour |
| Pause | Pause sync / Resume sync | `media-playback-pause-symbolic` / `-start-` |
| Photos section | Photos | `image-x-generic-symbolic` |
| Sharing | Shared by me / Shared with me | distinct icons (`emblem-shared` / `folder-publicshare`) |

Spelling: pick one (US "Favorite" or UK "Favourite") and apply it everywhere. Title Case for
dialog headings and buttons in dialogs, Sentence case for menu items and rows (HIG).

## 9. Bugs found during the audit (quick wins)

1. Photos Upload pill loses its label (`photos.rs:426-432`); same for viewer "Show on map"
   (`photo_viewer.rs:594-598`).
2. Takeout Back always goes to Settings, sidebar highlights Settings when opened from Photos
   (`main.rs:779`, `takeout.rs:131-136`).
3. Thumbnail-build completion message computed then hidden (`browser.rs:1632-1667`).
4. Search results show cached files as cloud-only (`browser.rs:2344`).
5. Details "Available offline" switch shown for folders (`details.rs:259`).
6. Grid and list keep separate selections; details pane can describe a hidden-view item.
7. F2 silently does nothing with multiple items selected.
8. Filter changes on the Albums grid swap to the timeline while the Albums toggle stays active
   (`photos.rs:958-997`).
9. Subtitle "60 photos" next to "All 1,416" (loaded count vs total, `photos.rs:2007-2015`); viewer
   counter "12 of 60".
10. Viewer info panel covers Close/Trash/Save (`photo_viewer.rs:588` vs `648`).
11. Locations mode switch and removal reload Computers, not Locations; a rejected mode switch stays
    flipped until the next tick (`devices.rs:612`, `devices.rs:809`, `locations.rs:374`).
12. Going on-demand deletes local copies without confirmation.
13. Retry on Not-connected silently restarts the systemd unit; Locations' Retry does not.
14. Toasts from Versions/Share dialogs land behind the modal.
15. "Emptied trash —" dangling dash, "item(s)", "archive(s)", raw revision id as version title,
    raw lowercase link role.
16. Bulk context menu shows a separator directly under the header when a folder is selected.
17. Sign out, Reject invitation, Remove access, Delete link, tray Disconnect: no confirmation.
18. Stale doc comments (`devices.rs:46-48`, `locations.rs:329-331`, `albums.rs:4`,
    `browser.rs:1155`).

## 10. Phased roadmap

Each phase is shippable on its own. Estimates are rough, for one developer.

| Phase | Content | Crates | Size |
|---|---|---|---|
| 0 Quick wins | Section 9 bugs, glossary wording, icon swaps (upload, offline, stop syncing), confirmations, plurals | gui, tray | S (2–3 days) |
| 1 Shell | libadwaita feature bump, per-page header bars, sidebar restructure + footer (status, quota, account), Preferences dialog, About debug info, theme/colour cleanup, CSS to gresource, shortcuts dialog | gui | M (1 week) |
| 2 Sync hub + pause | B-1 global pause, B-2 list/retry pending ops, Sync page (status card, Issues, Transfers, Folders, History), tray states and pause menu, GUI uses `failing_*` / `parked_*` | core, fuse, gui, tray, cli | L (1.5–2 weeks) |
| 3 Conflicts | B-3 list + resolve, Issues rows with a compare/resolve dialog (both versions side by side: size, date, device, preview for images/text), CLI `pdfs conflicts` | core, fuse, gui, cli | M–L |
| 4 File browser | Header layout, New menu, sort/persisted view, PopoverMenu context menus, details side pane, DnD upload, folder picker for Move, search scope | gui | L |
| 5 Photos | Rename, header switcher, filters, infinite scroll + scrubber, tile layout, context menu, album management, viewer rewrite (auto-hide, zoom, inline video) | gui (+ core for album mutations) | L |
| 6 Rest | Shared pages, Computers, Trash, Login, per-folder pause, bandwidth limits, i18n wrapping | all | M |

Phase 2 is where the user-reported gaps (pause, resume, stuck items) get closed; phase 3 closes the
conflict overview. Phases 0–1 fix most of the "unpolished" impression cheaply and should come first
because later phases build on the new header/page skeleton.

### Verification per phase

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`.
- Backend phases: DB migration tests in `crates/pdfs-core/src/db/tests.rs` (paused state, conflict
  table if added), drain tests proving no op is claimed while paused, resolve-conflict tests proving
  the losing side stays recoverable (trash or version history).
- GUI phases: before/after screenshots in `images/` (light and dark), keyboard-only walkthrough of
  each page, narrow-window check (sidebar collapse breakpoint).
