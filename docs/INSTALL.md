# Installation

This page covers installing Proton Drive for Linux from a package or from source, the first
sign-in, running without a desktop, and removing the client again.

- [Requirements](#requirements)
- [Install a package](#install-a-package)
- [Build from source](#build-from-source)
- [First run](#first-run)
- [Headless and server use](#headless-and-server-use)
- [Updating](#updating)
- [Uninstalling](#uninstalling)

## Requirements

| | Minimum |
|---|---|
| Architecture | x86_64 (release packages); other architectures build from source |
| Kernel | FUSE 3 (`fuse3` package, `/dev/fuse` available to your user) |
| GTK / libadwaita | GTK 4.12, libadwaita 1.5 (1.8 or newer gets the newest widgets) |
| WebKitGTK | 6.0 — used only for the CAPTCHA page during sign-in |
| Secret Service | GNOME Keyring, KWallet, KeePassXC, or another provider of `org.freedesktop.secrets` |
| Rust (source builds) | 1.96 |

The session is stored only in the Secret Service. Without a running provider, sign-in succeeds but
cannot be saved, and the service cannot start unattended.

Optional runtime extras:

| Package | Adds |
|---|---|
| `exiftool` (`perl-image-exiftool`, `libimage-exiftool-perl`) | Thumbnails for camera RAW files |
| `ffmpeg` | Video thumbnails |
| `iso-codes` | Country names in your language on the Places map |
| `xdg-utils` | "Open Folder" in the tray, and the default file opener |
| `fzf` plus a terminal (foot, kitty, alacritty, …) | `pdfs-prompt --fzf`, the search-as-you-type launcher |
| AppIndicator extension (GNOME only) | The tray icon. KDE Plasma and most other desktops show it natively |

## Install a package

Every [release](https://github.com/narrrl/proton-drive-linux/releases/latest) ships a `.deb`, a
signed `.rpm` and a `.tar.gz` of the raw binaries. All packages install the same files:

| Path | Purpose |
|---|---|
| `/usr/bin/pdfs` | Command-line client and background service |
| `/usr/bin/pdfs-app` | Desktop app |
| `/usr/bin/pdfs-tray` | Tray icon |
| `/usr/bin/pdfs-prompt` | Search launcher |
| `/usr/lib/systemd/user/proton-drive.service` | User service that keeps the drive mounted |
| `/etc/xdg/autostart/io.narl.proton-drive-linux-tray.desktop` | Starts the tray at login |
| `/usr/share/locale/*/LC_MESSAGES/pdfs.mo` | Translations |

### Debian and Ubuntu (24.04 or newer)

```bash
sudo apt install ./proton-drive-linux_*_amd64.deb
```

### Fedora (44 or newer)

```bash
sudo dnf install ./proton-drive-linux-*.x86_64.rpm
```

For a Fedora package built from your own checkout, see
[Build an RPM locally](#build-an-rpm-locally).

### Arch Linux (AUR)

Three packages cover the usual choices. They conflict with each other, so install one:

| Package | Contents |
|---|---|
| [`proton-drive-for-linux`](https://aur.archlinux.org/packages/proton-drive-for-linux) | Latest tagged release, built from source |
| [`proton-drive-for-linux-bin`](https://aur.archlinux.org/packages/proton-drive-for-linux-bin) | Latest tagged release, prebuilt binaries |
| [`proton-drive-for-linux-git`](https://aur.archlinux.org/packages/proton-drive-for-linux-git) | `main`, built from git |

```bash
paru -S proton-drive-for-linux     # or yay, or any other AUR helper
```

The recipes live in [`packaging/aur/`](../packaging/aur/README.md).

### Other distributions

Unpack the `.tar.gz` into a directory on your `PATH`, then install the service unit as shown in
[Using a binary outside `/usr/bin`](#using-a-binary-outside-usrbin).

## Build from source

### Build dependencies

**Debian / Ubuntu 24.04+**

```bash
sudo apt-get install -y pkg-config gettext libfuse3-dev libgtk-4-dev libadwaita-1-dev \
  libwebkitgtk-6.0-dev libsecret-1-dev libdbus-1-dev
```

**Fedora 44+**

```bash
sudo dnf install -y cargo rust gettext pkgconf-pkg-config fuse3-devel gtk4-devel \
  libadwaita-devel webkitgtk6.0-devel libsecret-devel dbus-devel glib2-devel
```

**Arch Linux**

```bash
sudo pacman -S --needed rust gettext pkgconf fuse3 gtk4 libadwaita webkitgtk-6.0 libsecret dbus
```

Install Rust 1.96 or newer with [rustup](https://rustup.rs) if your distribution ships an older
toolchain.

### Compile

```bash
git clone https://github.com/narrrl/proton-drive-linux.git
cd proton-drive-linux
cargo build --release --locked
po/build.sh target/locale        # optional: compile the translations
```

The four binaries are in `target/release/`: `pdfs`, `pdfs-app`, `pdfs-tray` and `pdfs-prompt`.

### Build a package from the working tree

**Arch Linux:**

```bash
cd packaging && makepkg -fi
```

<a id="build-an-rpm-locally"></a>**Fedora:**

```bash
sudo dnf install -y rpm-build
rpmbuild -bb packaging/proton-drive-linux.spec \
  --define "git_dir $PWD" \
  --define "_rpmdir $PWD/packaging/out" \
  --define "_builddir $PWD/packaging/build" \
  --define "_sourcedir $PWD" \
  --define "_specdir $PWD/packaging" \
  --define "_srcrpmdir $PWD/packaging/out"
sudo dnf install packaging/out/x86_64/proton-drive-linux-*.rpm
```

### Using a binary outside `/usr/bin`

The service unit starts `/usr/bin/pdfs daemon`. If you installed the binaries somewhere else, for
example `~/.local/bin`, install the unit for your user and point it at your binary:

```bash
mkdir -p ~/.config/systemd/user
cp packaging/proton-drive.service ~/.config/systemd/user/
sed -i "s|^ExecStart=.*|ExecStart=$HOME/.local/bin/pdfs daemon|" \
  ~/.config/systemd/user/proton-drive.service
systemctl --user daemon-reload
```

To start the tray at login, copy `packaging/io.narl.proton-drive-linux-tray.desktop` to
`~/.config/autostart/`. For the app launcher entry, copy `packaging/io.narl.proton-drive-linux.desktop`
to `~/.local/share/applications/` and the icon `packaging/io.narl.proton-drive-linux.svg` to
`~/.local/share/icons/hicolor/scalable/apps/`.

## First run

### With the desktop app

1. Open **Proton Drive** from your application menu, or run `pdfs-app`.
2. Sign in with your Proton email and password, and your two-factor code if the account has one.
   If Proton asks for a CAPTCHA (common on a VPN or a new network), the app shows the challenge in
   a window and continues after you solve it.
3. The app enables and starts `proton-drive.service`. Your Drive appears at `~/ProtonDrive`.

The service is tied to your graphical session. It starts after you log in and stops when you log
out. Change the folder location under **Preferences → General → Location**.

### With the command line

`pdfs login` stores the session but does not enable the service, so a setup that runs the daemon
another way stays untouched. Enable the service yourself:

```bash
pdfs login                                        # email, password, 2FA code
systemctl --user enable --now proton-drive.service
pdfs status                                       # account, mount and queue state
```

The CLI cannot show a CAPTCHA. If sign-in reports one, sign in once with `pdfs-app` instead; the
session it stores is used by the CLI and the service as well.

## Headless and server use

The service unit is bound to `graphical-session.target`. On a machine without a desktop session,
run the daemon directly or from your own unit:

```bash
pdfs daemon                      # waits for a stored session, mounts, stays mounted
pdfs mount /srv/proton           # mounts once at a given path, blocks until unmounted
```

Both still need a Secret Service provider. On a server, run
`gnome-keyring-daemon --unlock` or KeePassXC's Secret Service integration inside the user's D-Bus
session.

## Updating

Install the new package over the old one. The service picks up the new binary the next time it
starts; to switch at once:

```bash
systemctl --user restart proton-drive.service
```

Database migrations run automatically and only move forward. A database written by a newer
release refuses to open in an older one, so downgrading means clearing the local database (see
[Troubleshooting](TROUBLESHOOTING.md#start-over-with-a-clean-local-state)).

## Uninstalling

> [!WARNING]
> Check that nothing is waiting to upload before you remove local state. `pdfs sync queue` must be
> empty. Queued writes exist only on this computer until they reach Proton Drive.

```bash
pdfs sync queue                                   # must list nothing
systemctl --user disable --now proton-drive.service
pdfs logout                                       # removes the session from the keyring
```

Then remove the package (`sudo apt remove proton-drive-linux`, `sudo dnf remove
proton-drive-linux`, or `sudo pacman -R proton-drive-for-linux`), and, if you want no trace left,
the local data:

```bash
rm -rf ~/.config/proton-drive-linux ~/.local/state/proton-drive-linux ~/.cache/proton-drive-linux
```

Sign out of the device in your Proton account settings as well, and remove the computer from
**Computers** in the Proton Drive web app if you no longer want its backups.
