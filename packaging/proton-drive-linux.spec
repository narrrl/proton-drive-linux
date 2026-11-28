# Two ways to build this spec:
#
# - From a checkout (local builds and the release workflow). From the repository root:
#     rpmbuild -bb packaging/proton-drive-linux.spec --define "git_dir $PWD" ...
#   %build needs network so Cargo can fetch crates.io and Git dependencies.
# - From the release tarballs (COPR, .github/workflows/copr.yml), when git_dir is not
#   defined. Source1 holds every crate, so the build runs offline.

%global debug_package %{nil}
# Match Arch PKGBUILD `!lto` — LTO has broken some GTK/Rust links in practice.
%global _lto_cflags %{nil}

%{!?version: %global version 4.0.0}
%global forge https://github.com/narrrl/proton-drive-linux
# Files come from the checkout, or from the unpacked tag archive.
%global src %{?git_dir}%{!?git_dir:.}

Name:           proton-drive-linux
Version:        %{version}
Release:        1%{?dist}
Summary:        Unofficial Proton Drive client for Linux (FUSE, CLI, GTK4 app + tray)
License:        MIT
URL:            https://proton-drive.narl.io
ExclusiveArch:  x86_64
%if 0%{!?git_dir:1}
Source0:        %{forge}/archive/refs/tags/v%{version}.tar.gz#/%{name}-%{version}.tar.gz
Source1:        %{forge}/releases/download/v%{version}/%{name}-vendor-%{version}.tar.xz
# The offline Places basemap, checked against its pinned SHA-256 in %%prep.
Source2:        https://github.com/narrrl/proton-rs/releases/download/basemap-v1/basemap-v1.pmtiles
%endif
%global basemap_sha256 ce9acab1b7ed7f15b23e2365b1b6a580db79f4bc4421851ee673eff4c920d280

BuildRequires:  cargo
BuildRequires:  rust >= 1.96
BuildRequires:  pkgconf-pkg-config
BuildRequires:  fuse3-devel
BuildRequires:  gtk4-devel
BuildRequires:  libadwaita-devel
BuildRequires:  libsecret-devel
BuildRequires:  dbus-devel
BuildRequires:  glib2-devel
BuildRequires:  webkitgtk6.0-devel
BuildRequires:  gettext

Requires:       polkit
Requires:       fuse3
Requires:       gtk4
Requires:       libadwaita
Requires:       libsecret
Requires:       webkitgtk6.0
Requires:       perl-Image-ExifTool
Requires:       xdg-utils

# DE-specific; do not Require a single desktop environment. dnf installs weak
# dependencies by default, so each is conditional on its own desktop: a plain
# `Recommends: kwallet` pulled KDE Frameworks onto GNOME installs.
Recommends:     (gnome-keyring or plasma-workspace)
Recommends:     iso-codes
Recommends:     (gnome-shell-extension-appindicator if gnome-shell)
Recommends:     (kwallet if plasma-workspace)

# Only for `pdfs-prompt --fzf`; every other front end works without them.
Suggests:       fzf
Suggests:       foot

Provides:       pdfs = %{version}-%{release}

%description
Files-on-demand Proton Drive for the Linux desktop: a FUSE virtual mount with
block-level caching, a scriptable CLI, and a GTK4/Libadwaita desktop app with
system tray and search launcher. Unofficial and not affiliated with Proton AG.

%prep
%if 0%{?git_dir:1}
# In-tree build: no Source tarball.
test -f %{git_dir}/Cargo.toml
cp -a %{git_dir}/LICENSE .
%else
# -a 1 unpacks the vendored crates and their source mappings.
%autosetup -n %{name}-%{version} -a 1
echo "%{basemap_sha256}  %{SOURCE2}" | sha256sum --check --quiet
# Older vendor tarballs contained only crates.io dependencies.
if [ ! -f .cargo/config.toml ]; then
  mkdir -p .cargo
  cat > .cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF
fi
%endif

%build
# The release workflow has already built target/release and passes --define "prebuilt 1".
# rpmbuild exports its own compiler flags, which would make cargo rebuild everything.
%if 0%{?prebuilt}
test -x %{git_dir}/target/release/pdfs
%else
cd %{src}
cargo build --release %{?git_dir:--locked}%{!?git_dir:--frozen} \
  --bin pdfs \
  --bin pdfs-tray \
  --bin pdfs-app \
  --bin pdfs-prompt
%endif

%install
rel=%{src}/target/release
install -D -m0755 "$rel/pdfs"        %{buildroot}%{_bindir}/pdfs
install -D -m0755 "$rel/pdfs-tray"   %{buildroot}%{_bindir}/pdfs-tray
install -D -m0755 "$rel/pdfs-app"    %{buildroot}%{_bindir}/pdfs-app
install -D -m0755 "$rel/pdfs-prompt" %{buildroot}%{_bindir}/pdfs-prompt

install -D -m0644 %{src}/packaging/io.narl.proton-drive-linux.desktop \
  %{buildroot}%{_datadir}/applications/io.narl.proton-drive-linux.desktop
install -D -m0644 %{src}/packaging/io.narl.proton-drive-linux-tray.desktop \
  %{buildroot}%{_sysconfdir}/xdg/autostart/io.narl.proton-drive-linux-tray.desktop
install -D -m0644 %{src}/packaging/io.narl.proton-drive-linux.svg \
  %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg
install -D -m0644 %{src}/packaging/io.narl.proton-drive-linux.policy \
  %{buildroot}/usr/share/polkit-1/actions/io.narl.proton-drive-linux.policy
install -D -m0644 %{src}/packaging/proton-drive.service \
  %{buildroot}/usr/lib/systemd/user/proton-drive.service
%if 0%{?git_dir:1}
# scripts/fetch-basemap.sh puts it here for in-tree builds.
basemap=%{git_dir}/data/basemap-v1.pmtiles
%else
basemap=%{SOURCE2}
%endif
echo "%{basemap_sha256}  $basemap" | sha256sum --check --quiet
install -D -m0644 "$basemap" %{buildroot}%{_datadir}/proton-drive-linux/basemap-v1.pmtiles
%{src}/po/build.sh %{buildroot}%{_datadir}/locale
%{src}/packaging/completions.sh "$rel/pdfs" %{buildroot}%{_datadir}
%if 0%{!?git_dir:1}
# dnf brings new releases from COPR, so the app does not offer its own check.
install -D -m0644 /dev/null %{buildroot}%{_datadir}/proton-drive-linux/managed-updates
%endif

# Removing the package does not stop a running daemon: it keeps serving the
# mount from the deleted binary, so files deleted under the mount afterwards,
# `rm -rf ~/ProtonDrive` included, are still deleted on Proton Drive. Stop every
# logged-in user's service first; anything still queued stays on disk for the
# next install. $1 is 0 on an erase and 1 on an upgrade. Same loop as
# packaging/proton-drive.install.
%preun
if [ "$1" -eq 0 ]; then
  for dir in /run/user/*; do
    user=$(id -nu "${dir##*/}" 2>/dev/null) || continue
    if runuser -u "$user" -- env XDG_RUNTIME_DIR="$dir" \
      systemctl --user --quiet is-active proton-drive.service 2>/dev/null; then
      echo "Stopping Proton Drive for $user, which unmounts its drive."
      runuser -u "$user" -- env XDG_RUNTIME_DIR="$dir" \
        systemctl --user stop proton-drive.service 2>/dev/null || :
    fi
  done
fi

%files
%license LICENSE
%{_bindir}/pdfs
%{_bindir}/pdfs-tray
%{_bindir}/pdfs-app
%{_bindir}/pdfs-prompt
%{_datadir}/applications/io.narl.proton-drive-linux.desktop
%{_sysconfdir}/xdg/autostart/io.narl.proton-drive-linux-tray.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg
/usr/lib/systemd/user/proton-drive.service
%{_datadir}/polkit-1/actions/io.narl.proton-drive-linux.policy
%{_datadir}/locale/*/LC_MESSAGES/pdfs.mo
%{_datadir}/bash-completion/completions/pdfs
%{_datadir}/zsh/site-functions/_pdfs
%{_datadir}/fish/vendor_completions.d/pdfs.fish
%dir %{_datadir}/proton-drive-linux
%{_datadir}/proton-drive-linux/basemap-v1.pmtiles
%if 0%{!?git_dir:1}
%{_datadir}/proton-drive-linux/managed-updates
%endif

%changelog
* Sun Jul 19 2026 Local Packager - 0.4.0-1
- Initial Fedora local package (in-tree cargo build).
