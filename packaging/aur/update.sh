#!/usr/bin/env bash
# Point the AUR packages at a new upstream release: rewrite pkgver, reset
# pkgrel, refresh the checksums that depend on the version, and regenerate
# every .SRCINFO.
#
# Usage: ./update.sh 1.12.0
set -euo pipefail

version="${1:-}"
if [[ -z $version ]]; then
  echo "usage: ${0##*/} <version>" >&2
  exit 2
fi

cd "$(dirname "$(readlink -f "$0")")"

# The -git package derives its pkgver from git describe, so only the two
# release-based packages are versioned here.
for pkg in proton-drive-for-linux proton-drive-for-linux-bin; do
  sed -i -e "s/^pkgver=.*/pkgver=$version/" -e "s/^pkgrel=.*/pkgrel=1/" "$pkg/PKGBUILD"
  (cd "$pkg" && updpkgsums)
done

# --printsrcinfo does not run pkgver(), so let makepkg clone the source and
# write the current git describe version back into the -git PKGBUILD first.
# --noprepare skips the cargo fetch in prepare().
(cd proton-drive-for-linux-git && makepkg -od --noprepare --noconfirm && rm -rf src)

for pkg in proton-drive-for-linux proton-drive-for-linux-git proton-drive-for-linux-bin; do
  (cd "$pkg" && makepkg --printsrcinfo > .SRCINFO)
  echo "updated $pkg"
done
