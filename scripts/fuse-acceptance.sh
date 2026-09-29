#!/usr/bin/env bash
# Filesystem syscall contract, with optional tests against mounted Drive views.
set -Eeuo pipefail

usage() {
    cat >&2 <<'EOF'
usage: scripts/fuse-acceptance.sh [OPTION ...]
       scripts/fuse-acceptance.sh --offline-only [OPTION ...]
       scripts/fuse-acceptance.sh --live MOUNTPOINT [MOUNTPOINT ...] [OPTION ...]
       scripts/fuse-acceptance.sh --managed-live EMPTY_DIR EMPTY_DIR [OPTION ...]
       scripts/fuse-acceptance.sh MOUNTPOINT [MOUNTPOINT ...]  # compatibility

With no mode, everything runs on the signed-in account: My files, then two
sync folders it registers itself, through every pairing of on-demand and
mirror, with moves and copies between them. Everything it creates is removed
afterwards, on failure and on Ctrl-C too, and permanently deleted from the
trash; a run that was killed is cleaned up by the next one.

--offline-only runs only the local reference suite, which needs no account.
It always runs first; every mount is diffed against its recorded behaviour.
--live runs the same contract on each given mount. Set PDFS_ACCEPTANCE_CONVERGENCE=1 only when all live mountpoints
show the same remote folder.

Options are forwarded to the Python runner. The useful ones:
  --list                  print every case and the targets it runs against
  --timeout SECONDS       per-case limit (default 180); a hung case dumps stacks
  --fail-fast             stop at the first failure instead of continuing
  --quick                 one on-demand/mirror pairing instead of all four
  --report-json PATH      machine-readable results and recorded observations
  --report-junit PATH     JUnit XML for CI
  --budget SECONDS        report any case slower than this
  --journal-check         fail if the daemon logs errors during the run
  --durability            restart the daemon mid-suite and re-verify bytes
EOF
    exit 2
}

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
runner=(python3 -u "$script_dir/fuse-acceptance.py")

mountpoints=()
passthrough=()
# Paths come first and options after them, so the first dash-prefixed argument
# ends the path list and everything from there is forwarded untouched.
collect_paths() {
    while (( $# > 0 )) && [[ "$1" != -* ]]; do
        mountpoints+=("$1")
        shift
    done
    passthrough=("$@")
}

case "${1:-}" in
    -h|--help) usage ;;
    "")
        exec "${runner[@]}" --account
        ;;
    --offline-only)
        shift
        exec "${runner[@]}" "$@"
        ;;
    --live)
        shift
        collect_paths "$@"
        (( ${#mountpoints[@]} > 0 )) || usage
        ;;
    --managed-live)
        shift
        (( $# >= 2 )) || usage
        first="$1"; second="$2"; shift 2
        exec "${runner[@]}" --managed-live "$first" "$second" "$@"
        ;;
    --account)
        exec "${runner[@]}" "$@"
        ;;
    --*)
        # Bare options with no mode: an account run with those options.
        exec "${runner[@]}" --account "$@"
        ;;
    *)
        collect_paths "$@"
        ;;
esac

command -v findmnt >/dev/null || { echo "findmnt is required for --live" >&2; exit 2; }
for mountpoint in "${mountpoints[@]}"; do
    [[ -d "$mountpoint" ]] || { echo "not a directory: $mountpoint" >&2; exit 2; }
done
fstype="$(findmnt -T "${mountpoints[0]}" -n -o FSTYPE | head -n 1)"
[[ "$fstype" == fuse* ]] || {
    echo "refusing primary non-FUSE path ${mountpoints[0]} (type: ${fstype:-unknown})" >&2
    exit 2
}

exec "${runner[@]}" --live "${mountpoints[@]}" "${passthrough[@]}"
