#!/usr/bin/env bash
# Download the offline Places basemap built by proton-rs's `basemap` tool,
# checked against its pinned SHA-256. Without it the map shows coarse
# outlines only.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
name=basemap-v1.pmtiles
url=https://github.com/narrrl/proton-rs/releases/download/basemap-v1/$name
sha256=ce9acab1b7ed7f15b23e2365b1b6a580db79f4bc4421851ee673eff4c920d280
out=${1:-$root/data/$name}
if [[ -f $out ]] && echo "$sha256  $out" | sha256sum --check --status; then
  exit 0
fi
mkdir -p "$(dirname -- "$out")"
part=$(mktemp "$out.XXXXXX")
trap 'rm -f -- "$part"' EXIT
curl --fail --location --silent --show-error --retry 3 --output "$part" "$url"
echo "$sha256  $part" | sha256sum --check --status || {
  echo "The downloaded $name doesn't match its pinned checksum." >&2
  exit 1
}
chmod 644 "$part"
mv -- "$part" "$out"
