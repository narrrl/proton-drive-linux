# Local in-tree RPM (mirrors packaging/PKGBUILD).
# From the repository root:
#   rpmbuild -bb packaging/proton-drive-linux.spec --define "git_dir $PWD" ...
# %build needs network so cargo can fetch crates.io (same trade-off as the Arch PKGBUILD).

%global debug_package %{nil}
# Match Arch PKGBUILD `!lto` — LTO has broken some GTK/Rust links in practice.
%global _lto_cflags %{nil}

Name:           proton-drive-linux
Version:        0.4.0
Release:        1%{?dist}
Summary:        Proton Drive client for Linux (FUSE, CLI, GTK4 app + tray)
License:        MIT
URL:            https://github.com/narl/proton-drive-linux
ExclusiveArch:  x86_64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  pkgconf-pkg-config
BuildRequires:  fuse3-devel
BuildRequires:  gtk4-devel
BuildRequires:  libadwaita-devel
BuildRequires:  libsecret-devel
BuildRequires:  dbus-devel
BuildRequires:  glib2-devel

Requires:       fuse3
Requires:       gtk4
Requires:       libadwaita
Requires:       libsecret
Requires:       xdg-utils

# DE-specific; do not Require a single desktop environment.
Recommends:     gnome-keyring
Recommends:     gnome-shell-extension-appindicator
Recommends:     kwallet

Provides:       pdfs = %{version}-%{release}

%description
Unofficial Proton Drive client featuring a FUSE files-on-demand mount,
CLI, GTK4/Libadwaita GUI, system tray, and search launcher.

%prep
# In-tree build: no Source tarball. Pass --define "git_dir /path/to/checkout".
test -n "%{?git_dir}" || (echo 'Pass --define "git_dir $PWD" from the repo root' >&2; exit 1)
test -f %{git_dir}/Cargo.toml
cp -a %{git_dir}/LICENSE .
%build
cd %{git_dir}
cargo build --release --locked \
  --bin pdfs \
  --bin pdfs-tray \
  --bin pdfs-app \
  --bin pdfs-prompt

%install
rel=%{git_dir}/target/release
install -D -m0755 "$rel/pdfs"        %{buildroot}%{_bindir}/pdfs
install -D -m0755 "$rel/pdfs-tray"   %{buildroot}%{_bindir}/pdfs-tray
install -D -m0755 "$rel/pdfs-app"    %{buildroot}%{_bindir}/pdfs-app
install -D -m0755 "$rel/pdfs-prompt" %{buildroot}%{_bindir}/pdfs-prompt

install -D -m0644 %{git_dir}/packaging/io.narl.proton-drive-linux.desktop \
  %{buildroot}%{_datadir}/applications/io.narl.proton-drive-linux.desktop
install -D -m0644 %{git_dir}/packaging/io.narl.proton-drive-linux-tray.desktop \
  %{buildroot}%{_sysconfdir}/xdg/autostart/io.narl.proton-drive-linux-tray.desktop
install -D -m0644 %{git_dir}/packaging/io.narl.proton-drive-linux.svg \
  %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg
install -D -m0644 %{git_dir}/packaging/proton-drive.service \
  %{buildroot}/usr/lib/systemd/user/proton-drive.service

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

%changelog
* Sun Jul 19 2026 Local Packager - 0.4.0-1
- Initial Fedora local package (in-tree cargo build).
