# Installation

Install Proton Drive for Linux from your distribution's package, sign in, and keep it up to date.
This page also covers building from source, running without a desktop, and removing the client
again.

- [Install a package](#install-a-package)
- [Optional extras](#optional-extras)
- [First run](#first-run)
- [Updating](#updating)
- [Uninstalling](#uninstalling)
- [Headless and server use](#headless-and-server-use)
- [Build from source](#build-from-source)
- [What the packages install](#what-the-packages-install)

## Install a package

| Distribution | Package | Updates |
|---|---|---|
| [Fedora 44 or newer](#fedora-44-or-newer) | COPR repository `narl-rs/proton-drive-linux` | With `dnf upgrade` |
| [Arch Linux](#arch-linux-aur) | `proton-drive-for-linux` in the AUR | With your AUR helper |
| [Debian, Ubuntu 24.04 or newer](#debian-and-ubuntu-2404-or-newer) | `.deb` from the release page | Download each release |
| [NixOS](#nixos-flake) | The flake in this repository | With `nix flake update` |
| [Anything else](#other-distributions) | `.tar.gz` from the release page, or [build from source](#build-from-source) | By hand |

Release packages are for x86_64. You also need a Secret Service provider, such as GNOME
Keyring, KWallet or KeePassXC, to keep your session. Most desktops have one already.

### Fedora (44 or newer)

Enable the [COPR repository](https://copr.fedorainfracloud.org/coprs/narl-rs/proton-drive-linux/)
and install the package:

```bash
sudo dnf copr enable narl-rs/proton-drive-linux
sudo dnf install proton-drive-linux
```

COPR builds every release for Fedora 44, 45 and Rawhide, so `sudo dnf upgrade` keeps the client
up to date along with the rest of your system.

You can also download the signed `.rpm` from the
[latest release](https://github.com/narrrl/proton-drive-linux/releases/latest). It installs the
same files, but you have to download each new version yourself:

```bash
sudo dnf install ./proton-drive-linux-*.x86_64.rpm
```

To build an RPM from your own checkout, see [Build an RPM locally](#build-an-rpm-locally).

### Arch Linux (AUR)

Three packages cover the usual choices. They conflict with each other, so install one:

| Package | Contents |
|---|---|
| [`proton-drive-for-linux`](https://aur.archlinux.org/packages/proton-drive-for-linux) | Latest release, built from source |
| [`proton-drive-for-linux-bin`](https://aur.archlinux.org/packages/proton-drive-for-linux-bin) | Latest release, prebuilt binaries |
| [`proton-drive-for-linux-git`](https://aur.archlinux.org/packages/proton-drive-for-linux-git) | The newest code on `main`, built from git |

```bash
paru -S proton-drive-for-linux     # or yay, or any other AUR helper
```

The recipes are in [`packaging/aur/`](../packaging/aur/README.md).

### Debian and Ubuntu (24.04 or newer)

Download the `.deb` from the [latest release](https://github.com/narrrl/proton-drive-linux/releases/latest)
and install it:

```bash
sudo apt install ./proton-drive-linux_*_amd64.deb
```

There is no APT repository yet, so repeat this for each new release. Turn on **Check for
updates** in **Preferences** to be told when one is out.

### NixOS (flake)

The repository is a flake. Add it as an input and enable the module. The module installs the
package, starts the user service in every graphical session, and turns on the `fusermount3`
wrapper and GNOME Keyring for you:

```nix
{
  inputs.proton-drive-linux.url = "github:narrrl/proton-drive-linux";

  outputs = { nixpkgs, proton-drive-linux, ... }: {
    nixosConfigurations.<host> = nixpkgs.lib.nixosSystem {
      modules = [
        proton-drive-linux.nixosModules.default
        { services.proton-drive-linux.enable = true; }
      ];
    };
  };
}
```

That URL follows `main`. To stay on releases, pin the input to a release tag instead, and change
the tag to update:

```nix
inputs.proton-drive-linux.url = "github:narrrl/proton-drive-linux/v3.5.0";
```

Each release tag is built with Nix before the release is published.

#### Binary cache

CI uploads every build of `main` and of each release tag to a Cachix binary cache, so Nix can
download the client instead of compiling it. A system build does not read a flake input's cache
settings, so add the cache to your NixOS configuration once:

```nix
nix.settings = {
  substituters = [ "https://proton-drive-linux.cachix.org" ];
  trusted-public-keys = [
    "proton-drive-linux.cachix.org-1:w1+3pXVXyTlqkFGn3dWpUhlaJtyGhSm3X0mFwfa++AA="
  ];
};
```

The cache holds the build against the `nixpkgs` this flake locks. Setting
`inputs.proton-drive-linux.inputs.nixpkgs.follows = "nixpkgs"` builds against yours instead,
which the cache does not have, so Nix compiles it. `nix run` and `nix shell` ask whether to use
the cache the first time; answer yes, or run `cachix use proton-drive-linux`.

To try it without changing your system, run it straight from the flake:

```bash
nix run github:narrrl/proton-drive-linux#proton-drive-linux -- --help   # the CLI
nix shell github:narrrl/proton-drive-linux -c pdfs-app                   # the desktop app
```

The mount needs the setuid `fusermount3` from NixOS (`programs.fuse.enable`, on by default with
the module) and a Secret Service such as GNOME Keyring or KWallet. To leave out ffmpeg, and with
it video thumbnails, override the package with `withFfmpeg = false`. The recipe is in
[`packaging/nix/`](../packaging/nix/).

### Other distributions

Unpack the `.tar.gz` from the [latest release](https://github.com/narrrl/proton-drive-linux/releases/latest)
into a directory on your `PATH`, then install the service as shown in
[Using a binary outside `/usr/bin`](#using-a-binary-outside-usrbin). The binaries need GTK 4.12,
libadwaita 1.5, WebKitGTK 6.0 and FUSE 3; see [Requirements](#requirements).

## Optional extras

The packages pull in what the client needs. These add features if you install them:

| Package | Adds |
|---|---|
| `exiftool` (`perl-image-exiftool`, `libimage-exiftool-perl`) | Thumbnails for camera RAW files |
| `ffmpeg` | Video thumbnails |
| `iso-codes` | Country names in your language on the Places map |
| `xdg-utils` | "Open Folder" in the tray, and the default file opener |
| `fzf` and a terminal (foot, kitty, alacritty, …) | `pdfs-prompt --fzf`, the search-as-you-type launcher |
| [AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/) (GNOME only) | The tray icon. KDE Plasma and most other desktops show it without help |

## First run

### With the desktop app

1. Open **Proton Drive** from your applications menu, or run `pdfs-app`.
2. Sign in with your Proton email and password, and your two-factor code if the account has one.
   If Proton asks for a CAPTCHA (common on a VPN or a new network), the app shows the challenge in
   a window and continues after you solve it.
3. The app enables and starts `proton-drive.service`. Your Drive appears at `~/ProtonDrive`.

The service belongs to your desktop session. It starts after you log in and stops when you log
out. To put the folder somewhere else, change **Preferences → General → Location**.

[Getting started](GETTING_STARTED.md) explains what to do next.

### With the command line

`pdfs login` stores the session but does not enable the service, so a setup that runs the service
another way stays untouched. Enable it yourself:

```bash
pdfs login                                        # email, password, 2FA code
systemctl --user enable --now proton-drive.service
pdfs status                                       # account, mount and queue state
```

`pdfs login` prints the address of the CAPTCHA page if Proton asks for one: open it in your
browser, solve it, then press Enter.

## Updating

| Installed from | Update with |
|---|---|
| COPR (Fedora) | `sudo dnf upgrade` |
| AUR | `paru -Syu`, or your AUR helper's equivalent |
| Nix flake | `nix flake update proton-drive-linux`, or change the pinned tag, then rebuild |
| `.deb`, release `.rpm`, `.tar.gz` | Download the new release and install it over the old one |

The service switches to the new version the next time it starts. To switch at once:

```bash
systemctl --user restart proton-drive.service
```

With the `.deb`, the release `.rpm` or the `.tar.gz`, nothing tells you about a new release.
Turn on **Preferences → General → Check for updates**: the app asks GitHub once a day and shows a
banner over the sidebar when a newer version is out. The app never installs updates itself.
Packages that a package manager updates do not offer the setting. A package marks itself that way
by installing the empty file `share/proton-drive-linux/managed-updates` under the prefix of its
`bin/` directory, for example `/usr/share/proton-drive-linux/managed-updates`.

Coming from 2.8.0 or older, re-enable the service once. Those releases started it before the
desktop session was ready, and on KDE that crashes `ksecretd` at every login. Signing in again or
toggling **Start on login** does this as well:

```bash
systemctl --user reenable proton-drive.service
```

Database upgrades run by themselves and only move forward. A newer release's database does not
open in an older release, so going back to an older version means clearing the local database
(see [Troubleshooting](TROUBLESHOOTING.md#start-over-with-a-clean-local-state)).

## Uninstalling

> [!WARNING]
> Check that nothing is waiting to upload before you remove anything. `pdfs sync queue` must be
> empty. Changes that have not reached Proton Drive exist only on this computer.

1. Stop the service and sign out:

   ```bash
   pdfs sync queue                                   # must list nothing
   systemctl --user disable --now proton-drive.service
   pdfs logout                                       # removes the session from the keyring
   ```

2. Remove the package:

   | Installed from | Remove with |
   |---|---|
   | COPR (Fedora) | `sudo dnf remove proton-drive-linux`, then `sudo dnf copr remove narl-rs/proton-drive-linux` |
   | Release `.rpm` | `sudo dnf remove proton-drive-linux` |
   | `.deb` | `sudo apt remove proton-drive-linux` |
   | AUR | `sudo pacman -R proton-drive-for-linux` (or the `-bin` or `-git` package) |
   | NixOS | Remove the module and the flake input, then rebuild |

   Removing the package also stops the service of every logged-in user, in case step 1 was
   skipped.

3. If you want no trace left, remove the local data:

   ```bash
   rm -rf ~/.config/proton-drive-linux ~/.local/state/proton-drive-linux ~/.cache/proton-drive-linux
   ```

4. Sign the device out in your Proton account settings. If you no longer want this computer's
   backups, remove it from **Computers** in the Proton Drive web app.

> [!CAUTION]
> Do not delete `~/ProtonDrive` while it is mounted. It is your Proton Drive, not a copy: every
> file deleted there is moved to the Proton Drive trash. Once the service is stopped, the folder
> is empty, and removing it is safe.

## Headless and server use

The packaged service starts with a desktop session. On a machine without one, run the service
directly or from your own systemd unit:

```bash
pdfs daemon                      # waits for a stored session, mounts, stays mounted
pdfs mount /srv/proton           # mounts once at a given path, blocks until unmounted
```

Both still need a Secret Service provider for the session. On a server, run
`gnome-keyring-daemon --unlock` or KeePassXC's Secret Service integration inside the user's D-Bus
session.

## Build from source

### Requirements

| | Minimum |
|---|---|
| Architecture | Any that Rust and GTK support. Release packages are x86_64 only |
| Kernel | FUSE 3 (`fuse3` package, `/dev/fuse` available to your user) |
| GTK / libadwaita | GTK 4.12, libadwaita 1.5 (1.8 or newer gets the newest widgets) |
| WebKitGTK | 6.0, used only for the CAPTCHA page during sign-in |
| Secret Service | GNOME Keyring, KWallet, KeePassXC, or another provider of `org.freedesktop.secrets` |
| Rust | 1.96 |

The session is stored only in the Secret Service. Without a running provider, sign-in succeeds
but cannot be saved, and the service cannot start unattended.

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

If your distribution ships a Rust older than 1.96, install a newer one with
[rustup](https://rustup.rs).

### Compile

```bash
git clone https://github.com/narrrl/proton-drive-linux.git
cd proton-drive-linux
cargo build --release --locked
po/build.sh target/locale        # optional: compile the translations
```

The four programs are in `target/release/`: `pdfs`, `pdfs-app`, `pdfs-tray` and `pdfs-prompt`.

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

The service unit starts `/usr/bin/pdfs daemon`. If you installed the programs somewhere else, for
example `~/.local/bin`, install the unit for your user and point it at your binary:

```bash
mkdir -p ~/.config/systemd/user
cp packaging/proton-drive.service ~/.config/systemd/user/
sed -i "s|^ExecStart=.*|ExecStart=$HOME/.local/bin/pdfs daemon|" \
  ~/.config/systemd/user/proton-drive.service
systemctl --user daemon-reload
```

To start the tray at login, copy `packaging/io.narl.proton-drive-linux-tray.desktop` to
`~/.config/autostart/`. For the entry in your applications menu, copy
`packaging/io.narl.proton-drive-linux.desktop` to `~/.local/share/applications/` and the icon
`packaging/io.narl.proton-drive-linux.svg` to `~/.local/share/icons/hicolor/scalable/apps/`.

## What the packages install

Every package installs the same files:

| Path | Purpose |
|---|---|
| `/usr/bin/pdfs` | Command-line client and background service |
| `/usr/bin/pdfs-app` | Desktop app |
| `/usr/bin/pdfs-tray` | Tray icon |
| `/usr/bin/pdfs-prompt` | Search launcher |
| `/usr/lib/systemd/user/proton-drive.service` | User service that keeps the drive mounted |
| `/etc/xdg/autostart/io.narl.proton-drive-linux-tray.desktop` | Starts the tray at login |
| `/usr/share/locale/*/LC_MESSAGES/pdfs.mo` | Translations |
