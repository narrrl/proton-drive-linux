# Proton Drive client for Linux (unofficial)

A fast, unofficial Proton Drive client for Linux. This client provides a FUSE files-on-demand virtual mount, a command-line interface (CLI), and a GTK4 desktop application with system tray integration.

## Features

- **Files-on-demand (FUSE)**: Mount your Proton Drive as a virtual filesystem where files are downloaded only when opened.
- **Command-Line Interface (CLI)**: Manage your drive, authenticate, and monitor sync status directly from the terminal.
- **GTK4 Desktop App**: Log in, browse files, and configure sync options through a modern, native Linux GUI.
- **System Tray Integration**: Background indicator for status monitoring, quick actions, and fast sync controls.
- **Secure Credential Storage**: Integrates with the system Secret Service (GNOME Keyring, KWallet, etc.) for safe token storage.

---

## Prerequisites

To compile the application from source or run the built binaries, ensure you have the following system libraries installed on your distribution:

### Ubuntu / Debian (24.04+)
```bash
sudo apt-get update
sudo apt-get install -y \
  pkg-config \
  libfuse3-dev \
  libgtk-4-dev \
  libadwaita-1-dev \
  libsecret-1-dev \
  libdbus-1-dev
```

### Arch Linux
```bash
sudo pacman -S --needed pkgconf fuse3 gtk4 libadwaita libsecret dbus
```

---

## Building from Source

Ensure you have Rust and Cargo installed (minimum supported Rust version is 1.96).

1. Clone the repository and navigate into the project directory:
   ```bash
   git clone https://github.com/narl/proton-drive-linux.git
   cd proton-drive-linux
   ```
2. Build the workspace in release mode:
   ```bash
   cargo build --release --locked
   ```

The compiled binaries will be available under `target/release/`:
- `pdfs`: The CLI utility.
- `pdfs-app`: The GTK4 application.
- `pdfs-tray`: The tray status notifier.

---

## Installation & Packages

### 1. Debian / Ubuntu (.deb)
Install the debian package via `dpkg` or `apt`:
```bash
sudo apt install ./proton-drive-linux_*.deb
```

### 2. AppImage (.AppImage)
The AppImage is a portable build that packages all binaries. Make it executable and run it:
```bash
chmod +x proton-drive-linux-*.AppImage
./proton-drive-linux-*.AppImage
```

> [!TIP]
> The AppImage is a multi-call binary. By default, running it launches the GUI. You can run the CLI or the Tray app by passing their name as the first argument, or by renaming/symlinking the AppImage file:
> ```bash
> # Run CLI
> ./proton-drive-linux-*.AppImage pdfs --help
> 
> # Run Tray
> ./proton-drive-linux-*.AppImage pdfs-tray
> ```

### 3. Arch Linux
A local `PKGBUILD` is available under the `packaging/` directory. You can build and install it using:
```bash
cd packaging && makepkg -fi
```

---

## Automated Releases (CI/CD)

This project has a GitHub Actions CI workflow configured under `.github/workflows/release.yml`.

### How it works:
1. **Triggers**: 
   - Pushing a git tag matching `v*` (e.g. `git tag v0.1.0 && git push origin v0.1.0`).
   - Manual runs via the **Actions** tab in GitHub (**workflow_dispatch**).
2. **Build Process**:
   - Spawns an Ubuntu runner and installs GTK4, Libadwaita, FUSE3, and Secret Service packages.
   - Sets up the Rust compiler and caches build targets to speed up runs.
   - Compiles the workspace members in release mode.
3. **Artifact Packaging**:
   - Generates a `.tar.gz` containing the raw binaries (`pdfs`, `pdfs-app`, `pdfs-tray`).
   - Packs them into a Debian package (`.deb`).
   - Builds a portable AppImage using `appimagetool` without requiring FUSE inside CI.
4. **Publishing**:
   - Creates a GitHub Release matching the pushed tag and uploads the `.deb`, `.AppImage`, and `.tar.gz` packages as release assets.
   - For manual runs, compiles and exposes the packages as workflow run artifacts for testing.

---

## License

This project is licensed under the [MIT License](LICENSE).
