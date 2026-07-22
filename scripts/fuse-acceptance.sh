#!/usr/bin/env bash
# Filesystem syscall contract, with optional tests against mounted Drive views.
set -Eeuo pipefail

usage() {
    cat >&2 <<'EOF'
usage: scripts/fuse-acceptance.sh [--offline-only]
       scripts/fuse-acceptance.sh --live MOUNTPOINT [MOUNTPOINT ...]
       scripts/fuse-acceptance.sh --managed-live EMPTY_DIR EMPTY_DIR
       scripts/fuse-acceptance.sh MOUNTPOINT [MOUNTPOINT ...]  # compatibility

The local reference suite always runs first and requires no account. --live then
runs the same contract on each mount. Set PDFS_ACCEPTANCE_CONVERGENCE=1 only
when all live mountpoints show the same remote folder.
EOF
    exit 2
}

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

case "${1:-}" in
    -h|--help) usage ;;
    --offline-only|"")
        (( $# <= 1 )) || usage
        exec python3 -u "$script_dir/fuse-acceptance.py"
        ;;
    --live) shift ; (( $# > 0 )) || usage ;;
    --managed-live)
        shift
        (( $# == 2 )) || usage
        exec python3 -u "$script_dir/fuse-acceptance.py" --managed-live "$@"
        ;;
    --*) usage ;;
esac

command -v findmnt >/dev/null || { echo "findmnt is required for --live" >&2; exit 2; }
for mountpoint in "$@"; do
    [[ -d "$mountpoint" ]] || { echo "not a directory: $mountpoint" >&2; exit 2; }
done
fstype="$(findmnt -T "$1" -n -o FSTYPE | head -n 1)"
[[ "$fstype" == fuse* ]] || {
    echo "refusing primary non-FUSE path $1 (type: ${fstype:-unknown})" >&2
    exit 2
}

exec python3 -u "$script_dir/fuse-acceptance.py" --live "$@"
