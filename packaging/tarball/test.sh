#!/usr/bin/env bash
# Check a release tarball: install it into a throwaway home, check that every
# file landed with its paths rewritten and that the programs run, update it in
# place, then uninstall and check nothing is left. Touches no real home and no
# systemd (--no-service).
#
# usage: packaging/tarball/test.sh <tarball>
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <tarball>" >&2
  exit 2
fi
tarball="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

tar -xzf "$tarball" -C "$work"
src="$(find "$work" -mindepth 1 -maxdepth 1 -type d -name 'proton-drive-linux-*')"
[ -x "$src/install.sh" ] || fail "no install.sh in the tarball"

export HOME="$work/home" XDG_CONFIG_HOME="$work/home/.config"
mkdir -p "$HOME"
prefix="$HOME/.local"
# Exercise the failure path independently of this host's own packages.
mkdir "$work/tools"
cat >"$work/tools/ldd" <<'EOF'
#!/bin/sh
echo 'libmissing.so => not found'
EOF
chmod +x "$work/tools/ldd"
if PATH="$work/tools:$PATH" "$src/install.sh" --check >"$work/check.log" 2>&1; then
  fail "compatibility check accepted a missing library"
fi
grep -q 'libmissing.so' "$work/check.log" || fail "check did not name the missing library"
[ -z "$(find "$HOME" -mindepth 1 -print -quit)" ] || fail "compatibility check wrote into HOME"
# ldd refusing the binary outright must fail the check too, not pass it.
cat >"$work/tools/ldd" <<'EOF'
#!/bin/sh
echo 'not a dynamic executable' >&2
exit 1
EOF
if PATH="$work/tools:$PATH" "$src/install.sh" --check >"$work/check.log" 2>&1; then
  fail "compatibility check accepted a binary ldd could not load"
fi
grep -q 'not a dynamic executable' "$work/check.log" || fail "check did not say why ldd failed"
[ -z "$(find "$HOME" -mindepth 1 -print -quit)" ] || fail "compatibility check wrote into HOME"

"$src/install.sh" --no-service --force >"$work/install.log" 2>&1 ||
  { cat "$work/install.log" >&2; fail "install.sh failed"; }

for exe in pdfs pdfs-app pdfs-tray pdfs-prompt; do
  [ -x "$prefix/bin/$exe" ] || fail "$exe was not installed"
done
"$prefix/bin/pdfs" --version >/dev/null || fail "pdfs does not run"
app="$prefix/share/applications/io.narl.proton-drive-linux.desktop"
tray="$XDG_CONFIG_HOME/autostart/io.narl.proton-drive-linux-tray.desktop"
unit="$XDG_CONFIG_HOME/systemd/user/proton-drive.service"
grep -qx "Exec=\"$prefix/bin/pdfs-app\"" "$app" || fail "the app's Exec= was not rewritten: $(grep ^Exec= "$app")"
grep -qx "Exec=\"$prefix/bin/pdfs-tray\"" "$tray" || fail "the tray's Exec= was not rewritten: $(grep ^Exec= "$tray")"
grep -qx "ExecStart=\"$prefix/bin/pdfs\" daemon" "$unit" || fail "ExecStart= was not rewritten: $(grep ^ExecStart= "$unit")"
[ -f "$prefix/share/locale/de/LC_MESSAGES/pdfs.mo" ] || fail "no translations"
[ -f "$prefix/share/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg" ] || fail "no icon"
[ -f "$prefix/share/bash-completion/completions/pdfs" ] || fail "no bash completion"
[ -f "$prefix/share/proton-drive-linux/basemap-v1.pmtiles" ] || fail "no Places basemap"
[ ! -e "$prefix/share/proton-drive-linux/managed-updates" ] || fail "the update check must stay on offer"
uninstaller="$prefix/share/proton-drive-linux/install.sh"
[ -x "$uninstaller" ] || fail "install.sh was not kept for --uninstall"

# An update from the same tarball leaves the same files.
before="$(cd "$work/home" && find . -type f | sort)"
"$src/install.sh" --no-service --force >"$work/update.log" 2>&1 ||
  { cat "$work/update.log" >&2; fail "updating failed"; }
after="$(cd "$work/home" && find . -type f | sort)"
[ "$before" = "$after" ] || fail "an update changed the set of files: $(diff <(echo "$before") <(echo "$after"))"

# A file an older version installed and this one does not is removed.
stale="$prefix/share/locale/xx/LC_MESSAGES/pdfs.mo"
mkdir -p "$(dirname "$stale")" && : >"$stale"
echo "$stale" >>"$prefix/share/proton-drive-linux/installed-files"
"$src/install.sh" --no-service --force >/dev/null 2>&1
[ ! -e "$stale" ] || fail "a file the new version does not ship was left behind"

# Settings are not the installer's: uninstalling keeps them.
mkdir -p "$XDG_CONFIG_HOME/proton-drive-linux"
echo '{}' >"$XDG_CONFIG_HOME/proton-drive-linux/config.json"
"$uninstaller" --uninstall --no-service >"$work/uninstall.log" 2>&1 ||
  { cat "$work/uninstall.log" >&2; fail "uninstall failed"; }
left="$(cd "$work/home" && find . -type f | sort)"
[ "$left" = "./.config/proton-drive-linux/config.json" ] || fail "uninstall left files behind: $left"

echo "tarball OK: $(basename "$tarball")"
