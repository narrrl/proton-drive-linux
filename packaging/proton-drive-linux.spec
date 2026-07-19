Name:           proton-drive-linux
Version:        %{version}
Release:        1%{?dist}
Summary:        Proton Drive client for Linux (unofficial)

License:        MIT
URL:            https://github.com/narl/proton-drive-linux

# Disable debuginfo package generation since we package pre-built release binaries
%global debug_package %{nil}
# Disable binary strip/post-processing checks to avoid issues with pre-compiled binaries
%global __os_install_post %{nil}

Requires:       fuse3
Requires:       gtk4 >= 4.12.0
Requires:       libadwaita >= 1.5.0
Requires:       libsecret
Requires:       dbus

%description
A fast, unofficial Proton Drive client for Linux. This client features an advanced files-on-demand FUSE virtual mount with block-level caching, a command-line interface (CLI), and a fully non-blocking GTK4 desktop application with system tray integration.

%install
mkdir -p %{buildroot}%{_bindir}
mkdir -p %{buildroot}%{_datadir}/applications
mkdir -p %{buildroot}%{_sysconfdir}/xdg/autostart
mkdir -p %{buildroot}%{_datadir}/icons/hicolor/scalable/apps
mkdir -p %{buildroot}%{_usr}/lib/systemd/user
mkdir -p %{buildroot}%{_datadir}/licenses/proton-drive-linux

cp %{_sourcedir}/target/release/pdfs %{buildroot}%{_bindir}/
cp %{_sourcedir}/target/release/pdfs-tray %{buildroot}%{_bindir}/
cp %{_sourcedir}/target/release/pdfs-app %{buildroot}%{_bindir}/
cp %{_sourcedir}/target/release/pdfs-prompt %{buildroot}%{_bindir}/

cp %{_sourcedir}/packaging/io.narl.proton-drive-linux.desktop %{buildroot}%{_datadir}/applications/
cp %{_sourcedir}/packaging/io.narl.proton-drive-linux-tray.desktop %{buildroot}%{_sysconfdir}/xdg/autostart/
cp %{_sourcedir}/packaging/io.narl.proton-drive-linux.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/
cp %{_sourcedir}/packaging/proton-drive.service %{buildroot}%{_usr}/lib/systemd/user/
cp %{_sourcedir}/LICENSE %{buildroot}%{_datadir}/licenses/proton-drive-linux/

%files
%{_bindir}/pdfs
%{_bindir}/pdfs-tray
%{_bindir}/pdfs-app
%{_bindir}/pdfs-prompt
%{_datadir}/applications/io.narl.proton-drive-linux.desktop
%{_sysconfdir}/xdg/autostart/io.narl.proton-drive-linux-tray.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg
%{_usr}/lib/systemd/user/proton-drive.service
%{_datadir}/licenses/proton-drive-linux/LICENSE
