# FAQ

Short answers to common questions, with links to the details.

- [Is this an official Proton app?](#is-this-an-official-proton-app)
- [If I delete a file in `~/ProtonDrive`, is it deleted on Proton Drive?](#if-i-delete-a-file-in-protondrive-is-it-deleted-on-proton-drive)
- [Online only, available offline or synced folder: which do I want?](#online-only-available-offline-or-synced-folder-which-do-i-want)
- [Does it work offline?](#does-it-work-offline)
- [How do I get updates?](#how-do-i-get-updates)
- [Why is there no tray icon?](#why-is-there-no-tray-icon)
- [What is stored unencrypted on my computer?](#what-is-stored-unencrypted-on-my-computer)
- [Does it send telemetry?](#does-it-send-telemetry)
- [Can I use it on several computers?](#can-i-use-it-on-several-computers)
- [Can I continue a backup from Windows or macOS?](#can-i-continue-a-backup-from-windows-or-macos)
- [Can I run it on a server without a desktop?](#can-i-run-it-on-a-server-without-a-desktop)
- [Which architectures and languages are supported?](#which-architectures-and-languages-are-supported)
- [How do I report a bug?](#how-do-i-report-a-bug)

## Is this an official Proton app?

No. It is an independent, open-source client, not affiliated with or endorsed by Proton AG. It
uses a Rust port of Proton's own Drive SDK, and everything stays end-to-end encrypted. It has had
no independent security audit. See the [security model](SECURITY_MODEL.md).

## If I delete a file in `~/ProtonDrive`, is it deleted on Proton Drive?

Yes. `~/ProtonDrive` is your Proton Drive, not a copy. A deleted file moves to the Proton Drive
Trash, and you can restore it from **Trash** in the app or the web. See [Trash](FILES.md#trash).

Stopping the service is not the same: once the service stops, `~/ProtonDrive` is just an empty
folder, and your files stay on Proton Drive.

## Online only, available offline or synced folder: which do I want?

- **Online only** for most of your Drive. Files take no space until you open them.
- **Available offline** for files in `~/ProtonDrive` you need without a connection.
- **Synced folder** for a folder that lives on this computer, such as `~/Documents`, and should be
  backed up.

[Getting started](GETTING_STARTED.md#3-choose-how-files-are-kept) compares them side by side.

## Does it work offline?

Yes, for files that are on this computer: available-offline files, synced folders, and recently
opened files still in the cache. You can also create, rename, move and delete files offline. The
changes are queued and sent when the connection is back. See
[Changes waiting for Proton Drive](SYNCED_FOLDERS.md#changes-waiting-for-proton-drive).

## How do I get updates?

Through your package manager. The Fedora COPR repository, the AUR and the Nix flake get every
release, so `dnf upgrade`, your AUR helper or a flake update brings it. The `.deb` has to be
downloaded again for each release. To be told about new releases, turn on **Check for updates**
in **Preferences**. The app never updates itself. See [Installation → Updating](INSTALL.md#updating).

## Why is there no tray icon?

GNOME needs the
[AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/) to show
tray icons. If you hid the icon from its own menu, turn it back on under
**Preferences → General → Appearance**.

## What is stored unencrypted on my computer?

To show your files, the client decrypts them on this computer. Recently opened files, files kept
offline, changes waiting to upload, and the names of your files and folders are stored without
extra encryption. Use full-disk or home-folder encryption. The
[security model](SECURITY_MODEL.md#what-is-kept-on-this-computer) has the full list.

## Does it send telemetry?

No. The client connects only to Proton, and, if you turn them on, to OpenFreeMap for the street map
in Photos and to GitHub to check for updates. See
[Network connections](SECURITY_MODEL.md#network-connections).

## Can I use it on several computers?

Yes. Each computer has its own backup in Proton Drive, and two or more computers can also sync the
same folders. See [Syncing several computers](COMPUTERS.md#syncing-several-computers).

## Can I continue a backup from Windows or macOS?

Yes. On the **Computers** page, choose **Move to This Computer…** on the old computer. Its folders
move into this computer's backup on Proton Drive, without uploading anything again. See
[Moving another computer's backup here](RECOVERY.md#moving-another-computers-backup-here).

## Can I run it on a server without a desktop?

Yes, with `pdfs daemon` or `pdfs mount`. It still needs a Secret Service provider such as GNOME
Keyring or KeePassXC to hold the session. See
[Headless and server use](INSTALL.md#headless-and-server-use).

## Which architectures and languages are supported?

Release packages are for x86_64. Other architectures can [build from source](INSTALL.md#build-from-source).

The app, tray and launcher are available in English, German, Spanish, French, Italian, Dutch,
Polish and Brazilian Portuguese. [Translating](TRANSLATING.md) explains how to add a language.

## How do I report a bug?

Run `pdfs diagnose`, then [open an issue](https://github.com/narrrl/proton-drive-linux/issues/new/choose)
with what [Reporting a problem](TROUBLESHOOTING.md#reporting-a-problem) lists. Report security
problems privately, as described in [SECURITY.md](../SECURITY.md).
