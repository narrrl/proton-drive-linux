#!/usr/bin/env bash
# Compile every catalog in po/LINGUAS into <localedir>/<lang>/LC_MESSAGES/pdfs.mo.
# Packages pass their install tree, e.g. "$pkgdir/usr/share/locale"; for a run
# from the source tree use target/locale with PDFS_LOCALEDIR=target/locale.
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <localedir>" >&2
  exit 2
fi
localedir="$1"
po_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

while read -r lang; do
  [ -n "$lang" ] || continue
  mkdir -p "$localedir/$lang/LC_MESSAGES"
  msgfmt --check --output-file="$localedir/$lang/LC_MESSAGES/pdfs.mo" "$po_dir/$lang.po"
done < "$po_dir/LINGUAS"
