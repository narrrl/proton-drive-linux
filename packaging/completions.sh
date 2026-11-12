#!/usr/bin/env bash
# Write the shell completion scripts of a built `pdfs` into <datadir>, where
# bash-completion, zsh and fish look for them. Packages pass their install
# tree, e.g. "$pkgdir/usr/share". The scripts call `pdfs` from PATH on every
# Tab, so they are the same for every version and need no rebuild per release.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <pdfs binary> <datadir>" >&2
  exit 2
fi
pdfs="$1"
datadir="$2"

install -d "$datadir/bash-completion/completions" \
  "$datadir/zsh/site-functions" \
  "$datadir/fish/vendor_completions.d"
"$pdfs" completions bash > "$datadir/bash-completion/completions/pdfs"
"$pdfs" completions zsh > "$datadir/zsh/site-functions/_pdfs"
"$pdfs" completions fish > "$datadir/fish/vendor_completions.d/pdfs.fish"
chmod 644 "$datadir/bash-completion/completions/pdfs" \
  "$datadir/zsh/site-functions/_pdfs" \
  "$datadir/fish/vendor_completions.d/pdfs.fish"
