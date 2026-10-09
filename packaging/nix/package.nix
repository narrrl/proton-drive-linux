# Nix package for the workspace, built from the checkout this file lives in.
# The flake at the repository root exposes it; `nix build` builds it.
{
  lib,
  rustPlatform,
  pkg-config,
  gettext,
  glib,
  wrapGAppsHook4,
  fuse3,
  gtk4,
  libadwaita,
  webkitgtk_6_0,
  libsecret,
  dbus,
  exiftool,
  ffmpeg-headless,
  withFfmpeg ? true,
}:

let
  cargoToml = lib.importTOML ../../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "proton-drive-linux";
  inherit (cargoToml.workspace.package) version;

  # Only what the build reads, so editing docs or CI does not rebuild.
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../crates
      ../../po
      ../../LICENSE
      ../proton-drive.service
      ../io.narl.proton-drive-linux.desktop
      ../io.narl.proton-drive-linux-tray.desktop
      ../io.narl.proton-drive-linux.svg
    ];
  };

  cargoLock.lockFile = ../../Cargo.lock;

  cargoBuildFlags = [
    "--bin"
    "pdfs"
    "--bin"
    "pdfs-tray"
    "--bin"
    "pdfs-app"
    "--bin"
    "pdfs-prompt"
  ];

  nativeBuildInputs = [
    pkg-config
    gettext # msgfmt, for po/build.sh
    glib # glib-compile-resources, for the GUI's build.rs
    wrapGAppsHook4
  ];

  buildInputs = [
    fuse3
    gtk4
    libadwaita
    webkitgtk_6_0
    libsecret
    dbus
  ];

  # The app looks for its catalogs here instead of /usr/share/locale.
  env.PDFS_LOCALEDIR = "${placeholder "out"}/share/locale";

  # The suite mounts FUSE, talks to D-Bus and the kernel keyring, and checks
  # permissions a build sandbox does not have; CI runs it on every push.
  doCheck = false;

  postInstall = ''
    install -Dm644 packaging/io.narl.proton-drive-linux.desktop \
      $out/share/applications/io.narl.proton-drive-linux.desktop
    install -Dm644 packaging/io.narl.proton-drive-linux-tray.desktop \
      $out/etc/xdg/autostart/io.narl.proton-drive-linux-tray.desktop
    install -Dm644 packaging/io.narl.proton-drive-linux.svg \
      $out/share/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg
    substituteInPlace $out/share/applications/io.narl.proton-drive-linux.desktop \
      --replace-fail "Exec=pdfs-app" "Exec=$out/bin/pdfs-app"
    substituteInPlace $out/etc/xdg/autostart/io.narl.proton-drive-linux-tray.desktop \
      --replace-fail "Exec=pdfs-tray" "Exec=$out/bin/pdfs-tray"

    install -Dm644 packaging/proton-drive.service \
      $out/lib/systemd/user/proton-drive.service
    substituteInPlace $out/lib/systemd/user/proton-drive.service \
      --replace-fail "ExecStart=/usr/bin/pdfs" "ExecStart=$out/bin/pdfs"

    patchShebangs po/build.sh
    po/build.sh $out/share/locale
  '';

  # Thumbnails shell out to exiftool, and to ffmpeg for video. Suffixed, so a
  # user's own builds on PATH win. fusermount3 comes from the system: on NixOS
  # it is the setuid wrapper in /run/wrappers/bin.
  preFixup = ''
    gappsWrapperArgs+=(--suffix PATH : ${
      lib.makeBinPath ([ exiftool ] ++ lib.optional withFfmpeg ffmpeg-headless)
    })
  '';

  meta = {
    description = "Unofficial Proton Drive client for Linux: files-on-demand FUSE mount, CLI, GTK4 app and tray";
    homepage = "https://proton-drive.narl.io";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "pdfs";
  };
}
