#!/usr/bin/env bash
# Destructive only inside a uniquely named directory created by this script.
set -Eeuo pipefail

usage() {
    echo "usage: $0 MOUNTPOINT [MOUNTPOINT ...]" >&2
    echo "example: $0 /mnt/on-demand-testmount /mnt/testmount" >&2
    exit 2
}

(( $# > 0 )) || usage
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
command -v findmnt >/dev/null || { echo "findmnt is required" >&2; exit 2; }

mounts=("$@")
run_id="pdfs-acceptance-$(date +%Y%m%dT%H%M%S)-$$"
roots=()

cleanup() {
    local root
    for root in "${roots[@]}"; do
        [[ -n "$root" && "$root" == */pdfs-acceptance-* ]] || continue
        rm -rf -- "$root" 2>/dev/null || true
    done
}
trap cleanup EXIT INT TERM

for index in "${!mounts[@]}"; do
    mountpoint="${mounts[$index]}"
    [[ -d "$mountpoint" ]] || { echo "not a directory: $mountpoint" >&2; exit 2; }
    fs_type="$(findmnt -T "$mountpoint" -n -o FSTYPE | head -n 1)"
    if (( index == 0 )) && [[ "$fs_type" != fuse* ]]; then
        echo "refusing primary non-FUSE path $mountpoint (type: ${fs_type:-unknown})" >&2
        exit 2
    fi
    roots+=("${mountpoint%/}/$run_id")
done

step() { printf '[test] %s\n' "$1"; }
assert_text() {
    python3 - "$1" "$2" <<'PY'
import sys
p, expected = sys.argv[1], sys.argv[2].encode()
with open(p, "rb") as f: got = f.read()
if got != expected:
    raise AssertionError(
        f"{p}: expected {expected!r} ({len(expected)} bytes), "
        f"got {got!r} ({len(got)} bytes)"
    )
PY
}

run_suite() {
local primary="$1"
printf '[mount] testing %s\n' "${primary%/*}"
mkdir -- "$primary"

step "create, read, overwrite, truncate"
printf 'alpha\n' > "$primary/plain.txt"
assert_text "$primary/plain.txt" $'alpha\n'
printf 'a much longer replacement\n' > "$primary/plain.txt"
truncate -s 4 "$primary/plain.txt"
assert_text "$primary/plain.txt" "a mu"

step "binary round-trip and random-access writes"
python3 - "$primary/binary.dat" <<'PY'
import hashlib, sys
p = sys.argv[1]
data = bytes(range(256)) * 16384
with open(p, "wb") as f: f.write(data)
with open(p, "r+b", buffering=0) as f:
    f.seek(4093); f.write(b"boundary-write")
with open(p, "rb") as f: got = f.read()
expected = data[:4093] + b"boundary-write" + data[4107:]
assert got == expected
print(hashlib.sha256(got).hexdigest())
PY

step "media-style header and distant seek reads"
python3 - "$primary/seek-sample.mkv" <<'PY'
import os, sys

p = sys.argv[1]
size = 12 * 1024 * 1024 + 137
header = b"\x1aE\xdf\xa3pdfs-media-header"
middle = b"pdfs-middle-cue"
trailer = b"pdfs-media-trailer"
middle_at = 7 * 1024 * 1024 + 19
trailer_at = size - len(trailer)

# A player probes the container header, jumps to cues/index data, then commonly
# seeks near EOF.  Write a sparse fixture so the acceptance test stays cheap,
# and issue separate unbuffered reads so FUSE receives genuinely distant ranges
# rather than one accidental sequential read.
with open(p, "wb", buffering=0) as f:
    f.truncate(size)
    f.seek(0); f.write(header)
    f.seek(middle_at); f.write(middle)
    f.seek(trailer_at); f.write(trailer)

with open(p, "rb", buffering=0) as f:
    assert f.read(len(header)) == header
    f.seek(middle_at); assert f.read(len(middle)) == middle
    f.seek(trailer_at); assert f.read(len(trailer)) == trailer
    assert f.read(1) == b""
    f.seek(size + 4096); assert f.read(16) == b""
assert os.stat(p).st_size == size
PY

step "rename, move, and replacement semantics"
mkdir -- "$primary/dir-a" "$primary/dir-b"
printf 'source' > "$primary/source"
printf 'victim' > "$primary/victim"
mv -- "$primary/source" "$primary/victim"
assert_text "$primary/victim" "source"
[[ ! -e "$primary/source" ]]
mv -- "$primary/victim" "$primary/dir-a/moved"
assert_text "$primary/dir-a/moved" "source"

step "POSIX error behavior"
if rmdir -- "$primary/dir-a/moved" 2>/dev/null; then echo "rmdir removed a file" >&2; exit 1; fi
if rm -- "$primary/dir-a" 2>/dev/null; then echo "rm removed a directory" >&2; exit 1; fi
if mv -- "$primary/dir-a" "$primary/dir-a/child" 2>/dev/null; then
    echo "moving a directory below itself succeeded" >&2; exit 1
fi

step "open file remains readable after unlink"
python3 - "$primary/open-unlink" <<'PY'
import os, sys
p = sys.argv[1]; data = b"held-open\0" * 4096
with open(p, "wb") as f: f.write(data)
f = open(p, "rb", buffering=0); os.unlink(p)
assert not os.path.exists(p) and f.read() == data
f.close()
PY

step "concurrent independent writers"
for i in $(seq 1 12); do
    (python3 - "$primary/concurrent-$i" "$i" <<'PY'
import sys
p, seed = sys.argv[1], int(sys.argv[2]); data = bytes([seed]) * (131072 + seed)
with open(p, "wb") as f: f.write(data)
with open(p, "rb") as f: assert f.read() == data
PY
    ) &
done
wait

step "directory enumeration agrees with stat"
python3 - "$primary" <<'PY'
import os, sys
for name in os.listdir(sys.argv[1]): os.lstat(os.path.join(sys.argv[1], name))
PY
}

for root in "${roots[@]}"; do
    run_suite "$root"
done

if [[ "${PDFS_ACCEPTANCE_CONVERGENCE:-0}" == 1 ]] && (( ${#roots[@]} > 1 )); then
    step "cross-mount convergence"
    primary="${roots[0]}"
    expected="$(sha256sum "$primary/binary.dat" | cut -d' ' -f1)"
    for secondary in "${roots[@]:1}"; do
        deadline=$((SECONDS + ${PDFS_ACCEPTANCE_SYNC_TIMEOUT:-120}))
        while (( SECONDS < deadline )); do
            if [[ -f "$secondary/binary.dat" ]] &&
               [[ "$(sha256sum "$secondary/binary.dat" 2>/dev/null | cut -d' ' -f1)" == "$expected" ]]; then break; fi
            sleep 2
        done
        [[ -f "$secondary/binary.dat" ]] || { echo "did not appear on $secondary" >&2; exit 1; }
        [[ "$(sha256sum "$secondary/binary.dat" | cut -d' ' -f1)" == "$expected" ]] || {
            echo "content mismatch on $secondary" >&2; exit 1
        }

        # The media fixture may drain after binary.dat.  Wait independently,
        # then probe only its sparse ranges on the other mount.  Unlike the
        # same-mount check above, this exercises remote FUSE hydration and seek
        # reads instead of serving the still-local staged file.
        deadline=$((SECONDS + ${PDFS_ACCEPTANCE_SYNC_TIMEOUT:-120}))
        while (( SECONDS < deadline )); do
            [[ -f "$secondary/seek-sample.mkv" ]] &&
                [[ "$(stat -c %s "$secondary/seek-sample.mkv" 2>/dev/null)" == 12583049 ]] && break
            sleep 2
        done
        [[ -f "$secondary/seek-sample.mkv" ]] || {
            echo "media fixture did not appear on $secondary" >&2; exit 1
        }
        python3 - "$secondary/seek-sample.mkv" <<'PY'
import os, sys
p = sys.argv[1]
size = 12 * 1024 * 1024 + 137
checks = [
    (0, b"\x1aE\xdf\xa3pdfs-media-header"),
    (7 * 1024 * 1024 + 19, b"pdfs-middle-cue"),
    (size - len(b"pdfs-media-trailer"), b"pdfs-media-trailer"),
]
assert os.stat(p).st_size == size
with open(p, "rb", buffering=0) as f:
    for offset, expected in checks:
        f.seek(offset)
        assert f.read(len(expected)) == expected
PY
    done
fi

step "cleanup"
cleanup
roots=()
echo "PASS: FUSE acceptance suite completed ($run_id)"
