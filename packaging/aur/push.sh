#!/usr/bin/env bash
# Publish the packages in this directory to their AUR repositories.
#
# Usage: ./push.sh [pkgbase ...]        (default: all three)
#        MESSAGE="..." ./push.sh        (custom commit message)
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

packages=("$@")
if [[ ${#packages[@]} -eq 0 ]]; then
  packages=(proton-drive-for-linux proton-drive-for-linux-git proton-drive-for-linux-bin)
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

for pkg in "${packages[@]}"; do
  test -f "$pkg/PKGBUILD" || { echo "no such package: $pkg" >&2; exit 1; }

  # A fresh clone every time: the AUR repository, not this checkout, is what the
  # commit history has to build on.
  git -c init.defaultBranch=master clone -q "ssh://aur@aur.archlinux.org/$pkg.git" "$workdir/$pkg"
  install -m644 "$pkg/PKGBUILD" "$pkg/.SRCINFO" "$pkg/.gitignore" "$pkg/LICENSE" "$workdir/$pkg/"

  git -C "$workdir/$pkg" add -f PKGBUILD .SRCINFO .gitignore LICENSE
  if git -C "$workdir/$pkg" diff --cached --quiet; then
    echo "$pkg: no changes"
    continue
  fi

  version="$(sed -n 's/^\tpkgver = //p' "$pkg/.SRCINFO" | head -n1)"
  git -C "$workdir/$pkg" commit -q -m "${MESSAGE:-Update to $version}"
  git -C "$workdir/$pkg" push -q origin HEAD:master
  echo "$pkg: pushed ${MESSAGE:-$version}"
done
