#!/usr/bin/env bash
# Regenerate po/pdfs.pot from the sources listed in po/POTFILES.in, then merge
# the new template into every catalog in po/LINGUAS. Run from anywhere.
set -euo pipefail

po_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$po_dir/.."

version="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"

xgettext \
  --language=Rust \
  --from-code=UTF-8 \
  --files-from=po/POTFILES.in \
  --output=po/pdfs.pot \
  --package-name=proton-drive-linux \
  --package-version="$version" \
  --msgid-bugs-address=https://github.com/narrrl/proton-drive-linux/issues \
  --add-comments=Translators: \
  --sort-by-file \
  --keyword= \
  --keyword=gettext \
  --keyword=gettext_f \
  --keyword=gettext_noop \
  --keyword=pgettext:1c,2 \
  --keyword=ngettext:1,2 \
  --keyword=ngettext_f:1,2

while read -r lang; do
  [ -n "$lang" ] || continue
  if [ -f "po/$lang.po" ]; then
    msgmerge --quiet --update --backup=none --previous "po/$lang.po" po/pdfs.pot
  else
    msginit --no-translator --locale="$lang.UTF-8" --input=po/pdfs.pot --output-file="po/$lang.po"
  fi
done < po/LINGUAS
