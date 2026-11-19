#!/usr/bin/env bash
# Lay out the release tarball: built binaries, desktop files, icon, user
# service, translations, shell completions and install.sh, under one top-level
# directory with a /usr-like tree, then pack it.
#
# usage: packaging/tarball/build.sh <binary dir> <version> <output dir>
#   e.g. packaging/tarball/build.sh target/release 3.8.0 dist
# Writes <output dir>/proton-drive-linux-<version>-x86_64.tar.gz and prints
# its path.
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "usage: $0 <binary dir> <version> <output dir>" >&2
  exit 2
fi
bin_dir="$1"
version="$2"
out="$3"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

name="proton-drive-linux-$version-x86_64"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
root="$stage/$name"

for exe in pdfs pdfs-app pdfs-tray pdfs-prompt; do
  install -Dm755 "$bin_dir/$exe" "$root/bin/$exe"
done
install -Dm644 "$repo/packaging/io.narl.proton-drive-linux.desktop" \
  "$root/share/applications/io.narl.proton-drive-linux.desktop"
install -Dm644 "$repo/packaging/io.narl.proton-drive-linux-tray.desktop" \
  "$root/etc/xdg/autostart/io.narl.proton-drive-linux-tray.desktop"
install -Dm644 "$repo/packaging/io.narl.proton-drive-linux.svg" \
  "$root/share/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg"
install -Dm644 "$repo/packaging/io.narl.proton-drive-linux.policy" \
  "$root/share/polkit-1/actions/io.narl.proton-drive-linux.policy"
install -Dm644 "$repo/packaging/proton-drive.service" \
  "$root/lib/systemd/user/proton-drive.service"
install -Dm644 "$repo/LICENSE" "$root/share/licenses/proton-drive-linux/LICENSE"
"$repo/po/build.sh" "$root/share/locale"
"$repo/packaging/completions.sh" "$root/bin/pdfs" "$root/share"
install -Dm755 "$repo/packaging/tarball/install.sh" "$root/install.sh"

mkdir -p "$out"
tarball="$(cd "$out" && pwd)/$name.tar.gz"
tar -czf "$tarball" -C "$stage" --owner=0 --group=0 "$name"
echo "$tarball"
