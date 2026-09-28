#!/usr/bin/env python3
"""Account-free filesystem API contract plus opt-in live FUSE acceptance.

Every case runs first against a temporary local directory and then, when asked,
against the mount. The local run is not just a smoke test of the runner: its
observations become the reference that the mount is diffed against, so a case
fails when the mount *differs* from an ordinary filesystem, not only when it
raises. Differences that are legitimate (device ids, block counts, capabilities
a network filesystem cannot have) are recorded as notes instead of comparisons,
which makes the set of accepted divergences explicit and reviewable.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import contextlib
import ctypes
import errno
import faulthandler
import fcntl
import hashlib
import json
import mmap
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
import xml.etree.ElementTree as ElementTree

# A wedged FUSE operation is unkillable from Python, so every case runs under a
# soft alarm and a hard backstop. The soft alarm raises inside the main thread
# and lets cleanup and reporting run; the backstop dumps every thread's stack
# and exits, which is the only way to learn where a mount actually hung.
DEFAULT_TIMEOUT = int(os.environ.get("PDFS_ACCEPTANCE_TIMEOUT", "180"))
HARD_TIMEOUT_GRACE = 20

REFERENCE = "reference"
LIVE = "live"

# Failing an unsupported operation is fine; failing it *dirtily* is not. These
# are the errno values that mean "this filesystem does not offer that", as
# opposed to EIO or a hang, which mean something is broken.
CLEAN_UNSUPPORTED = {
    errno.EPERM,
    errno.ENOSYS,
    errno.EOPNOTSUPP,
    errno.EACCES,
    errno.EINVAL,
    errno.ENOTTY,
}

STALE_ROOT = re.compile(r"^pdfs-acceptance-[0-9a-f]{32}$")
ANSI = re.compile(r"\x1b\[[0-9;]*m")
ERROR_LEVEL = re.compile(r"\bERROR\b")

FALLOC_FL_KEEP_SIZE = 0x01
FALLOC_FL_PUNCH_HOLE = 0x02
RENAME_NOREPLACE = 1 << 0
RENAME_EXCHANGE = 1 << 1
AT_FDCWD = -100

_libc = ctypes.CDLL(None, use_errno=True)


def _libc_call(name: str, argtypes: list, *args) -> None:
    entry = getattr(_libc, name, None)
    if entry is None:
        raise OSError(errno.ENOSYS, f"{name} is unavailable in libc")
    entry.argtypes = argtypes
    entry.restype = ctypes.c_int
    ctypes.set_errno(0)
    if entry(*args) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))


def fallocate(fd: int, mode: int, offset: int, length: int) -> None:
    _libc_call(
        "fallocate",
        [ctypes.c_int, ctypes.c_int, ctypes.c_longlong, ctypes.c_longlong],
        ctypes.c_int(fd),
        ctypes.c_int(mode),
        ctypes.c_longlong(offset),
        ctypes.c_longlong(length),
    )


def renameat2(old: Path, new: Path, flags: int) -> None:
    _libc_call(
        "renameat2",
        [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint],
        ctypes.c_int(AT_FDCWD),
        os.fsencode(old),
        ctypes.c_int(AT_FDCWD),
        os.fsencode(new),
        ctypes.c_uint(flags),
    )


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def expect_errno(expected: set[int], operation, description: str) -> None:
    try:
        operation()
    except OSError as error:
        check(error.errno in expected, f"{description}: errno {error.errno}, expected {expected}")
    else:
        raise AssertionError(f"{description}: unexpectedly succeeded")


def check_bytes(actual: bytes, expected: bytes, message: str) -> None:
    """Assert byte equality with a report that is actually diagnosable.

    "wrong bytes" is useless in a log: a truncated upload, a zero-filled gap and
    a wholly different revision all produce it. Say which one it was.
    """
    if actual == expected:
        return
    detail = f"{message}: got {len(actual)} bytes, expected {len(expected)}"
    if len(actual) == len(expected):
        offset = next(i for i, (a, b) in enumerate(zip(actual, expected)) if a != b)
        run_end = offset
        while run_end < len(actual) and actual[run_end] != expected[run_end]:
            run_end += 1
        zeros = actual[offset:run_end] == bytes(run_end - offset)
        detail += (
            f"; first difference at {offset} ({run_end - offset} bytes"
            f"{', all zero' if zeros else ''})"
            f"; got {actual[offset:offset + 16]!r} expected {expected[offset:offset + 16]!r}"
        )
    elif expected.startswith(actual):
        detail += "; got a truncated prefix of the expected content"
    elif actual.startswith(expected):
        detail += "; got the expected content plus trailing data"
    raise AssertionError(detail)


def read(path: Path) -> bytes:
    with path.open("rb", buffering=0) as file:
        return file.read()


def write_durable(path: Path, data: bytes) -> None:
    fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
    try:
        view = memoryview(data)
        while view:
            written = os.write(fd, view)
            check(written > 0, "write made no progress")
            view = view[written:]
        os.fsync(fd)
    finally:
        os.close(fd)


class Observations:
    """Per-case facts, split into what must match the reference and what may not.

    `record` is a claim about filesystem semantics: the mount has to agree with
    an ordinary filesystem or the case fails. `note` is context — inode numbers,
    block counts, which optional syscalls exist — that is reported in the diff
    but never fails a run. Choosing between them at the call site is what keeps
    the accepted-divergence list honest; there is no separate allowlist to drift.
    """

    def __init__(self) -> None:
        self.compared: dict[str, object] = {}
        self.noted: dict[str, object] = {}
        self._scope = ""

    def scope(self, name: str) -> None:
        self._scope = name

    def _key(self, key: str) -> str:
        return f"{self._scope}.{key}" if self._scope else key

    def record(self, key: str, value) -> None:
        self.compared[self._key(key)] = value

    def note(self, key: str, value) -> None:
        self.noted[self._key(key)] = value

    def stat(self, key: str, path: Path) -> os.stat_result:
        """Record the portable half of a stat and note the rest."""
        info = os.lstat(path)
        self.record(f"{key}.type", stat.S_IFMT(info.st_mode))
        self.record(f"{key}.size", info.st_size)
        self.record(f"{key}.is_dir", stat.S_ISDIR(info.st_mode))
        self.note(f"{key}.mode", stat.S_IMODE(info.st_mode))
        self.note(f"{key}.nlink", info.st_nlink)
        self.note(f"{key}.blocks", info.st_blocks)
        return info

    def probe(self, key: str, operation, allow: set[int] = CLEAN_UNSUPPORTED) -> Outcome:
        """Run an optional operation; demand that failure at least be clean.

        The reference filesystem supports more than a network filesystem can, so
        the *outcome* is a note. What is asserted is the shape of a refusal: a
        recognised errno, never EIO and never a hang.
        """
        try:
            value = operation()
        except OSError as error:
            check(
                error.errno in allow,
                f"{key}: refused with errno {error.errno} ({errno.errorcode.get(error.errno)}); "
                f"expected success or one of {sorted(allow)}",
            )
            self.note(key, f"errno:{errno.errorcode.get(error.errno, error.errno)}")
            return Outcome(False, None, error.errno)
        self.note(key, "ok")
        return Outcome(True, value, None)


class Outcome:
    """The result of an optional operation: did it work, and if not, why."""

    def __init__(self, ok: bool, value, code: int | None) -> None:
        self.ok = ok
        self.value = value
        self.errno = code

    def __bool__(self) -> bool:
        return self.ok


class Context:
    """Everything a case may touch: its sandbox, its recorder, and the daemon."""

    def __init__(self, root: Path, obs: Observations, kind: str, daemon=None) -> None:
        self.root = root
        self.obs = obs
        self.kind = kind
        self.daemon = daemon

    @property
    def is_live(self) -> bool:
        return self.kind == LIVE

    def record(self, key: str, value) -> None:
        self.obs.record(key, value)

    def note(self, key: str, value) -> None:
        self.obs.note(key, value)

    def probe(self, key: str, operation, allow: set[int] = CLEAN_UNSUPPORTED):
        return self.obs.probe(key, operation, allow)

    def stat(self, key: str, path: Path) -> os.stat_result:
        return self.obs.stat(key, path)

    def require_daemon(self) -> Daemon:
        if self.daemon is None:
            raise Skip("no reachable pdfs daemon; set PDFS_ACCEPTANCE_PDFS")
        return self.daemon


class Skip(Exception):
    """A case cannot run here, and that is not a failure."""


class TestTimeout(Exception):
    pass


@contextlib.contextmanager
def time_limit(seconds: int, label: str):
    if seconds <= 0:
        yield
        return

    def on_alarm(_signum, _frame):
        raise TestTimeout(f"{label} exceeded {seconds}s")

    previous = signal.signal(signal.SIGALRM, on_alarm)
    faulthandler.dump_traceback_later(seconds + HARD_TIMEOUT_GRACE, exit=True)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        faulthandler.cancel_dump_traceback_later()
        signal.signal(signal.SIGALRM, previous)


# --------------------------------------------------------------------------
# Filesystem API contract
# --------------------------------------------------------------------------


def test_create_flags(ctx: Context) -> None:
    root = ctx.root
    path = root / "flags"
    fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
    os.write(fd, b"abcdef")
    os.close(fd)
    expect_errno({errno.EEXIST}, lambda: os.open(path, os.O_CREAT | os.O_EXCL), "O_EXCL")
    fd = os.open(path, os.O_WRONLY | os.O_APPEND)
    os.lseek(fd, 0, os.SEEK_SET)
    os.write(fd, b"++")
    os.close(fd)
    check(read(path) == b"abcdef++", "O_APPEND ignored")
    ctx.record("append", read(path))
    fd = os.open(path, os.O_WRONLY | os.O_TRUNC)
    os.close(fd)
    check(path.stat().st_size == 0, "O_TRUNC did not truncate")
    ctx.stat("truncated", path)


def test_positioned_and_vectored_io(ctx: Context) -> None:
    root = ctx.root
    path = root / "positioned.bin"
    base = bytes(range(256)) * 32768
    write_durable(path, base)
    fd = os.open(path, os.O_RDWR)
    try:
        check(os.pread(fd, 19, 4093) == base[4093:4112], "pread returned wrong range")
        check(os.pwrite(fd, b"boundary-write", 4093) == 14, "short pwrite")
        if hasattr(os, "pwritev"):
            check(os.pwritev(fd, [b"vector-", b"write"], 65531) == 12, "short pwritev")
        if hasattr(os, "preadv"):
            chunks = [bytearray(7), bytearray(5)]
            check(os.preadv(fd, chunks, 65531) == 12, "short preadv")
            check(b"".join(chunks) == b"vector-write", "preadv data mismatch")
        os.fdatasync(fd)
    finally:
        os.close(fd)
    expected = bytearray(base)
    expected[4093:4107] = b"boundary-write"
    expected[65531:65543] = b"vector-write"
    got = read(path)
    check(got == expected, "positioned write damaged surrounding bytes")
    digest = hashlib.sha256(got).hexdigest()
    ctx.record("digest", digest)
    ctx.stat("file", path)
    print(f"    sha256 {digest}")


def test_resize_and_sparse_io(ctx: Context) -> None:
    root = ctx.root
    path = root / "resize"
    write_durable(path, b"0123456789")
    os.truncate(path, 4)
    check(read(path) == b"0123", "shrink did not preserve prefix")
    os.truncate(path, 8193)
    data = read(path)
    check(data[:4] == b"0123" and data[4:] == bytes(8189), "grown range is not zero-filled")
    ctx.record("grown.digest", hashlib.sha256(data).hexdigest())

    sparse = root / "sparse"
    fd = os.open(sparse, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        os.pwrite(fd, b"head", 0)
        os.pwrite(fd, b"tail", 8 * 1024 * 1024 + 17)
        os.fsync(fd)
    finally:
        os.close(fd)
    check(sparse.stat().st_size == 8 * 1024 * 1024 + 21, "sparse size mismatch")
    fd = os.open(sparse, os.O_RDONLY)
    try:
        check(os.pread(fd, 8, 4) == bytes(8), "sparse hole is not zero-filled")
        check(os.pread(fd, 4, 8 * 1024 * 1024 + 17) == b"tail", "sparse tail missing")
        check(os.pread(fd, 1, sparse.stat().st_size + 4096) == b"", "read beyond EOF not empty")
    finally:
        os.close(fd)
    ctx.stat("sparse", sparse)


def test_mmap_and_copy_paths(ctx: Context) -> None:
    root = ctx.root
    source = root / "mapped"
    data = bytearray((b"mmap-and-sendfile\0" * 65536)[:1024 * 1024])
    write_durable(source, data)
    fd = os.open(source, os.O_RDWR)
    try:
        with mmap.mmap(fd, len(data), access=mmap.ACCESS_WRITE) as mapping:
            mapping[4091:4107] = b"mapped-boundary!"
            mapping.flush()
        os.fsync(fd)
    finally:
        os.close(fd)
    data[4091:4107] = b"mapped-boundary!"
    check(read(source) == data, "shared mmap write/read mismatch")

    target = root / "sendfile-copy"
    src = os.open(source, os.O_RDONLY)
    dst = os.open(target, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
    try:
        offset = 0
        while offset < len(data):
            count = os.sendfile(dst, src, offset, len(data) - offset)
            check(count > 0, "sendfile made no progress")
            offset += count
        os.fsync(dst)
    finally:
        os.close(src)
        os.close(dst)
    check(read(target) == data, "sendfile copy mismatch")
    ctx.record("digest", hashlib.sha256(read(target)).hexdigest())

    # coreutils reaches for copy_file_range before read/write, so a filesystem
    # that neither implements it nor refuses it cleanly breaks plain `cp`.
    ranged = root / "copy-file-range"
    src = os.open(source, os.O_RDONLY)
    dst = os.open(ranged, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
    try:
        copied = ctx.probe(
            "copy_file_range",
            lambda: os.copy_file_range(src, dst, len(data)),
            allow=CLEAN_UNSUPPORTED | {errno.EXDEV},
        )
    finally:
        os.close(src)
        os.close(dst)
    if copied.ok and copied.value:
        check(
            read(ranged)[: copied.value] == data[: copied.value],
            "copy_file_range produced wrong bytes",
        )


def test_namespace_and_errors(ctx: Context) -> None:
    root = ctx.root
    a, b = root / "dir-a", root / "dir-b"
    a.mkdir()
    b.mkdir()
    (a / "nested").mkdir()
    write_durable(a / "nested" / "child", b"child")
    write_durable(root / "source", b"source")
    write_durable(root / "victim", b"victim")
    os.replace(root / "source", root / "victim")
    check(read(root / "victim") == b"source" and not (root / "source").exists(), "replace failed")
    os.rename(root / "victim", b / "moved")
    check(read(b / "moved") == b"source", "cross-directory move failed")
    check("moved" in os.listdir(b), "cross-directory move missing from readdir")
    os.rename(b / "moved", b / "moved")
    check(read(b / "moved") == b"source", "same-name rename changed data")
    ctx.record("moved.bytes", read(b / "moved"))

    expect_errno({errno.ENOTEMPTY, errno.EEXIST}, lambda: os.rmdir(a), "rmdir(non-empty)")
    expect_errno({errno.ENOTDIR}, lambda: os.rmdir(b / "moved"), "rmdir(file)")
    expect_errno({errno.EISDIR, errno.EPERM}, lambda: os.unlink(a), "unlink(directory)")
    expect_errno({errno.EINVAL}, lambda: os.rename(a, a / "nested" / "cycle"), "directory cycle")
    expect_errno({errno.ENOENT}, lambda: os.unlink(root / "absent"), "unlink(absent)")
    expect_errno({errno.ENOENT}, lambda: os.rename(root / "absent", root / "new"), "rename(absent)")

    empty_src, empty_dst = root / "empty-src", root / "empty-dst"
    empty_src.mkdir()
    empty_dst.mkdir()
    os.replace(empty_src, empty_dst)
    check(empty_dst.is_dir() and not empty_src.exists(), "empty directory replacement failed")
    nonempty = root / "nonempty-dst"
    nonempty.mkdir()
    write_durable(nonempty / "keep", b"keep")
    replacement = root / "replacement-dir"
    replacement.mkdir()
    expect_errno(
        {errno.ENOTEMPTY, errno.EEXIST},
        lambda: os.replace(replacement, nonempty),
        "replace non-empty dir",
    )
    check(read(nonempty / "keep") == b"keep", "failed replacement damaged destination")

    # renameat2 flags: git and dpkg use NOREPLACE, and a filesystem that ignores
    # the flag instead of refusing it turns a guarded rename into a clobber.
    guard_src, guard_dst = root / "guard-src", root / "guard-dst"
    write_durable(guard_src, b"guard-src")
    write_durable(guard_dst, b"guard-dst")
    guarded = ctx.probe(
        "renameat2.noreplace",
        lambda: renameat2(guard_src, guard_dst, RENAME_NOREPLACE),
        allow=CLEAN_UNSUPPORTED | {errno.EEXIST},
    )
    check(not guarded.ok, "RENAME_NOREPLACE overwrote an existing destination")
    check(read(guard_dst) == b"guard-dst", "RENAME_NOREPLACE damaged the destination")
    ctx.probe("renameat2.exchange", lambda: renameat2(guard_src, guard_dst, RENAME_EXCHANGE))


def test_open_lifetime(ctx: Context) -> None:
    root = ctx.root
    path = root / "open-unlink"
    data = b"held-open\0" * 4096
    write_durable(path, data)
    fd = os.open(path, os.O_RDWR)
    os.unlink(path)
    check(not path.exists(), "unlinked name remains visible")
    check(os.pread(fd, len(data), 0) == data, "open file lost data after unlink")
    os.pwrite(fd, b"still-open", 0)
    check(os.pread(fd, 10, 0) == b"still-open", "unlinked open file is not writable")
    os.close(fd)

    old, new = root / "open-rename-old", root / "open-rename-new"
    write_durable(old, b"before")
    fd = os.open(old, os.O_RDWR)
    os.rename(old, new)
    os.pwrite(fd, b"after!", 0)
    os.fsync(fd)
    os.close(fd)
    check(not old.exists() and read(new) == b"after!", "open handle did not follow rename")
    ctx.record("renamed.bytes", read(new))


def test_names_and_enumeration(ctx: Context) -> None:
    root = ctx.root
    names = ["space name.txt", "unicodé-文件", ".hidden", "trailing.dot.", "case", "CASE"]
    for index, name in enumerate(names):
        write_durable(root / name, f"name-{index}".encode())
    listed = set(os.listdir(root))
    check(set(names) <= listed, "directory enumeration omitted valid names")
    for name in listed:
        os.lstat(root / name)
    ctx.record("case_sensitive", read(root / "case") != read(root / "CASE"))
    ctx.record("names", sorted(name for name in names))
    expect_errno({errno.ENAMETOOLONG}, lambda: os.open(root / ("x" * 256), os.O_CREAT), "overlong name")


def test_readdir_stability(ctx: Context) -> None:
    """Enumeration must not duplicate or lose entries when the directory moves.

    readdir is served off the dispatch loop, so a pass that re-reads the child
    list between two kernel-visible offsets can double-report or skip an entry.
    A lister that sees one name twice deletes it twice.
    """
    root = ctx.root
    directory = root / "readdir-churn"
    directory.mkdir()
    stable = [f"stable-{index:03d}" for index in range(64)]
    for name in stable:
        write_durable(directory / name, b"x")

    seen: list[str] = []
    created: list[str] = []
    with os.scandir(directory) as entries:
        for position, entry in enumerate(entries):
            seen.append(entry.name)
            # Mutate mid-enumeration: churn is the point.
            if position == 8:
                for index in range(4):
                    name = f"late-{index}"
                    write_durable(directory / name, b"y")
                    created.append(name)
            if position == 24:
                os.unlink(directory / stable[-1])

    check(len(seen) == len(set(seen)), f"readdir reported duplicates: {sorted(seen)}")
    survivors = set(os.listdir(directory))
    check(set(created) <= survivors, "entries created during enumeration were lost")
    check(stable[-1] not in survivors, "entry unlinked during enumeration reappeared")
    # An entry that existed unchanged for the whole pass must be reported once.
    missing = set(stable[:-1]) - set(seen)
    check(not missing, f"stable entries vanished from readdir: {sorted(missing)[:10]}")
    ctx.record("no_duplicates", True)
    ctx.record("survivors", len(survivors))

    # rewinddir: a second full pass must agree with the settled directory.
    check(set(os.listdir(directory)) == survivors, "second enumeration disagreed with the first")


def test_metadata_updates(ctx: Context) -> None:
    """chmod/utimes must succeed even where they cannot be stored.

    Drive has no POSIX mode or owner, and `setattr` deliberately accepts and
    ignores them. That is only correct if it reports success: `cp -p`, `tar -x`
    and `rsync -a` all treat a refused chmod as a fatal error.
    """
    root = ctx.root
    path = root / "metadata"
    write_durable(path, b"metadata")

    os.chmod(path, 0o640)
    ctx.note("mode.after_chmod", stat.S_IMODE(os.lstat(path).st_mode))
    os.utime(path, (1_600_000_000, 1_600_000_123))
    ctx.note("mtime.after_utime", int(os.lstat(path).st_mtime))
    ctx.record("chmod_and_utime_succeeded", True)

    directory = root / "metadata-dir"
    directory.mkdir()
    os.chmod(directory, 0o750)
    os.utime(directory, None)

    # A path-based truncate with no open handle is a separate write path (a
    # shell's `> file`), and it is the one that has to work offline.
    write_durable(path, b"0123456789")
    os.truncate(path, 0)
    check(read(path) == b"", "closed-file truncate to zero did not empty the file")
    ctx.record("closed_truncate.size", os.lstat(path).st_size)

    with path.open("wb", buffering=0) as handle:
        handle.write(b"redirect")
    check(read(path) == b"redirect", "reopen-and-write after truncate lost bytes")
    ctx.record("redirect.bytes", read(path))


def test_extended_attributes(ctx: Context) -> None:
    """The thumbnail xattr interface, including its two-call size protocol."""
    root = ctx.root
    path = root / "attributes.txt"
    write_durable(path, b"attributes")

    expect_errno(
        {errno.ENODATA, errno.ENOATTR} if hasattr(errno, "ENOATTR") else {errno.ENODATA},
        lambda: os.getxattr(path, "user.proton.definitely-absent"),
        "getxattr(unknown name)",
    )
    ctx.probe("setxattr", lambda: os.setxattr(path, "user.pdfs.probe", b"v"))

    listed = os.listxattr(path)
    ctx.note("listxattr", sorted(listed))
    # Thumbnails exist only for images and video; advertising them on a text
    # file is what made `ls -l` issue a network round trip per file (B5).
    check(
        "user.proton.thumbnail" not in listed and "user.proton.preview" not in listed,
        f"a text file advertised thumbnail attributes: {listed}",
    )
    ctx.record("no_thumbnail_on_text", True)

    if not ctx.is_live:
        return
    # Opportunistic: if the mount holds a real image, exercise the size protocol
    # against it. Nothing here creates one, because only the server can.
    for candidate in _find_thumbnailable(ctx.root.parent):
        names = os.listxattr(candidate)
        if "user.proton.thumbnail" not in names:
            continue
        size = _xattr_size(candidate, "user.proton.thumbnail")
        if size == 0:
            continue
        expect_errno(
            {errno.ERANGE},
            lambda: _xattr_into(candidate, "user.proton.thumbnail", size - 1),
            "getxattr(undersized buffer)",
        )
        payload = os.getxattr(candidate, "user.proton.thumbnail")
        check(len(payload) == size, "thumbnail length disagreed with its size probe")
        ctx.note("thumbnail.bytes", size)
        return
    ctx.note("thumbnail.bytes", "no thumbnailable file found")


def _find_thumbnailable(directory: Path, limit: int = 200):
    suffixes = {".jpg", ".jpeg", ".png", ".heic", ".mp4", ".mov", ".webp", ".gif"}
    seen = 0
    try:
        entries = sorted(directory.iterdir())
    except OSError:
        return
    for entry in entries:
        if seen >= limit:
            return
        seen += 1
        if entry.is_file() and entry.suffix.lower() in suffixes:
            yield entry


def _xattr_size(path: Path, name: str) -> int:
    fd = os.open(path, os.O_RDONLY)
    try:
        return len(os.getxattr(fd, name))
    except OSError:
        return 0
    finally:
        os.close(fd)


def _xattr_into(path: Path, name: str, size: int) -> bytes:
    """Force the undersized-buffer branch that Python's helper hides."""
    buffer = ctypes.create_string_buffer(max(size, 1))
    _libc.getxattr.argtypes = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_void_p,
        ctypes.c_size_t,
    ]
    _libc.getxattr.restype = ctypes.c_long
    ctypes.set_errno(0)
    written = _libc.getxattr(os.fsencode(path), name.encode(), buffer, ctypes.c_size_t(size))
    if written < 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    return buffer.raw[:written]


def test_allocation_and_holes(ctx: Context) -> None:
    """fallocate, including the punched hole that must survive the upload.

    A punched range reads as zero. The commit path gap-fills unauthored ranges
    from the remote baseline, so if the hole is not claimed as authored the
    original bytes come back and the hole silently disappears after close.
    """
    root = ctx.root
    path = root / "allocated"
    write_durable(path, b"A" * (256 * 1024))

    fd = os.open(path, os.O_RDWR)
    try:
        ctx.probe("posix_fallocate", lambda: os.posix_fallocate(fd, 0, 512 * 1024))
        os.fsync(fd)
    finally:
        os.close(fd)
    ctx.note("allocated.size", os.lstat(path).st_size)

    holed = root / "punched"
    write_durable(holed, b"B" * (192 * 1024))
    fd = os.open(holed, os.O_RDWR)
    punched = False
    try:
        try:
            fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 64 * 1024, 64 * 1024)
            punched = True
        except OSError as error:
            check(
                error.errno in CLEAN_UNSUPPORTED,
                f"punch hole refused with errno {error.errno}",
            )
            ctx.note("punch_hole", f"errno:{errno.errorcode.get(error.errno)}")
        if punched:
            ctx.note("punch_hole", "ok")
            check(
                os.pread(fd, 64 * 1024, 64 * 1024) == bytes(64 * 1024),
                "punched range did not read as zeros",
            )
        os.fsync(fd)
    finally:
        os.close(fd)

    if punched:
        check(os.lstat(holed).st_size == 192 * 1024, "punch hole with KEEP_SIZE changed the size")
        expected = b"B" * (64 * 1024) + bytes(64 * 1024) + b"B" * (64 * 1024)
        # Reopening is the assertion that matters: it is where a gap-fill from
        # the remote baseline would undo the hole.
        check(read(holed) == expected, "punched hole did not survive close and reopen")
        # A note, not a comparison: whether the filesystem can punch at all is a
        # capability. That it produced the right bytes when it did is asserted
        # above, where the answer does not depend on the reference having agreed
        # to punch too.
        ctx.note("punched.digest", hashlib.sha256(read(holed)).hexdigest())

    # tar and `cp --sparse` navigate with SEEK_HOLE/SEEK_DATA.
    fd = os.open(holed, os.O_RDONLY)
    try:
        ctx.probe("seek_data", lambda: os.lseek(fd, 0, os.SEEK_DATA), allow=CLEAN_UNSUPPORTED | {errno.ENXIO})
        ctx.probe("seek_hole", lambda: os.lseek(fd, 0, os.SEEK_HOLE), allow=CLEAN_UNSUPPORTED | {errno.ENXIO})
    finally:
        os.close(fd)


def test_unsupported_operations(ctx: Context) -> None:
    """Operations the filesystem does not implement must fail cleanly, not hang.

    Every one of these is probed by ordinary tools — git, rsync, `cp -a`, `df`,
    installers. An unimplemented FUSE handler that returns EIO, or blocks, turns
    a graceful fallback into a hard failure in software that never asked for the
    feature in the first place.
    """
    root = ctx.root
    target = root / "link-target"
    write_durable(target, b"link-target")

    ctx.probe("symlink", lambda: os.symlink("link-target", root / "symlink"))
    if (root / "symlink").is_symlink():
        check(os.readlink(root / "symlink") == "link-target", "readlink returned the wrong target")
    ctx.probe("hardlink", lambda: os.link(target, root / "hardlink"))
    ctx.probe("mkfifo", lambda: os.mkfifo(root / "fifo"))
    ctx.probe("mknod", lambda: os.mknod(root / "node", 0o600 | stat.S_IFCHR, os.makedev(1, 3)))

    # df and any installer that checks free space call statfs. It must answer.
    info = os.statvfs(root)
    check(info.f_bsize > 0, "statfs reported a zero block size")
    ctx.note("statfs.bsize", info.f_bsize)
    ctx.note("statfs.blocks", info.f_blocks)
    ctx.record("statfs_answers", True)

    check(os.access(root, os.R_OK | os.X_OK), "access(2) denied read/execute on the test root")
    check(os.access(target, os.R_OK), "access(2) denied read on a readable file")

    fd = os.open(target, os.O_RDWR)
    try:
        ctx.probe("flock", lambda: fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB))
        with contextlib.suppress(OSError):
            fcntl.flock(fd, fcntl.LOCK_UN)
        ctx.probe("posix_lock", lambda: fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 0, 1))
        with contextlib.suppress(OSError):
            fcntl.lockf(fd, fcntl.LOCK_UN, 0, 1)
    finally:
        os.close(fd)

    direct = ctx.probe("o_direct", lambda: os.open(target, os.O_RDONLY | os.O_DIRECT))
    if direct.ok:
        with contextlib.suppress(OSError):
            # O_DIRECT demands an aligned buffer; mmap gives a page-aligned one.
            with mmap.mmap(-1, 4096) as buffer:
                os.preadv(direct.value, [buffer], 0)
        os.close(direct.value)

    # TCGETS against a filesystem must answer ENOTTY, which is what stops fuser
    # from logging a warning every time a program probes whether an fd is a tty.
    fd = os.open(root, os.O_RDONLY)
    try:
        ctx.probe("ioctl_tcgets", lambda: fcntl.ioctl(fd, 0x5401, b"\0" * 64))
    finally:
        os.close(fd)


def test_concurrency(ctx: Context) -> None:
    root = ctx.root

    def independent(index: int) -> None:
        data = bytes([index]) * (131072 + index)
        path = root / f"concurrent-{index}"
        write_durable(path, data)
        check(read(path) == data, f"concurrent file {index} mismatch")

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(independent, range(1, 17)))

    shared = root / "concurrent-ranges"
    extent = 64 * 1024
    write_durable(shared, bytes(extent * 8))
    fd = os.open(shared, os.O_RDWR)
    try:

        def positioned(index: int) -> None:
            payload = bytes([index + 1]) * extent
            check(os.pwrite(fd, payload, index * extent) == extent, f"short concurrent pwrite {index}")

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(positioned, range(8)))
        os.fsync(fd)
    finally:
        os.close(fd)
    expected = b"".join(bytes([i + 1]) * extent for i in range(8))
    check(read(shared) == expected, "concurrent disjoint writes overlapped or vanished")
    ctx.record("shared.digest", hashlib.sha256(read(shared)).hexdigest())


def test_application_workloads(ctx: Context) -> None:
    """Real tools, because they combine syscalls in ways a matrix will not.

    Each of these has broken on a network filesystem before: the editor save
    dance (write temp, fsync, rename over), git's lock files and many small
    objects, tar's metadata restore, sqlite's journal and locking.
    """
    root = ctx.root
    workloads = root / "workloads"
    workloads.mkdir()

    # The atomic-save cycle every editor performs, repeated so a stale inode or
    # a mishandled replace shows up rather than passing once by luck.
    document = workloads / "document.txt"
    for revision in range(5):
        temporary = workloads / f".document.txt.{revision}.tmp"
        payload = f"revision {revision}\n".encode() * 128
        write_durable(temporary, payload)
        os.replace(temporary, document)
        check(read(document) == payload, f"atomic save lost revision {revision}")
    ctx.record("atomic_save.bytes", read(document))

    # tar restores mode and mtime on extract; a filesystem that refuses either
    # makes every extraction fail loudly.
    tree = workloads / "tree"
    (tree / "nested").mkdir(parents=True)
    write_durable(tree / "nested" / "leaf", b"leaf" * 512)
    write_durable(tree / "top", b"top")
    archive = workloads / "tree.tar"
    with tarfile.open(archive, "w") as bundle:
        bundle.add(tree, arcname="tree")
    extracted = workloads / "extracted"
    extracted.mkdir()
    with tarfile.open(archive) as bundle:
        try:
            bundle.extractall(extracted, filter="data")
        except TypeError:  # the extraction filter predates Python 3.12
            bundle.extractall(extracted)
    check(
        read(extracted / "tree" / "nested" / "leaf") == b"leaf" * 512,
        "tar extraction produced wrong bytes",
    )
    ctx.record("tar.leaf_size", os.lstat(extracted / "tree" / "nested" / "leaf").st_size)

    # sqlite exercises locking, journal creation and unlink, and fsync ordering.
    database = workloads / "workload.db"
    connection = sqlite3.connect(database)
    try:
        connection.execute("CREATE TABLE rows (id INTEGER PRIMARY KEY, value TEXT)")
        with connection:
            connection.executemany(
                "INSERT INTO rows (value) VALUES (?)", [(f"row-{index}",) for index in range(500)]
            )
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        count = connection.execute("SELECT COUNT(*) FROM rows").fetchone()[0]
    finally:
        connection.close()
    check(count == 500, f"sqlite lost rows: {count}")
    ctx.record("sqlite.rows", count)
    connection = sqlite3.connect(database)
    try:
        check(
            connection.execute("PRAGMA integrity_check").fetchone()[0] == "ok",
            "sqlite integrity check failed after reopen",
        )
    finally:
        connection.close()
    ctx.record("sqlite.integrity", "ok")

    if shutil.which("git"):
        repository = workloads / "repository"
        repository.mkdir()
        _run_tool(["git", "init", "--quiet", "."], repository)
        _run_tool(["git", "config", "user.email", "acceptance@example.invalid"], repository)
        _run_tool(["git", "config", "user.name", "pdfs acceptance"], repository)
        for index in range(3):
            write_durable(repository / f"file-{index}", f"content {index}\n".encode() * 64)
            _run_tool(["git", "add", "-A"], repository)
            _run_tool(["git", "commit", "--quiet", "-m", f"commit {index}"], repository)
        status = _run_tool(["git", "status", "--porcelain"], repository)
        check(status.strip() == "", f"git saw a dirty tree after committing: {status!r}")
        _run_tool(["git", "fsck", "--no-progress"], repository)
        ctx.record("git.clean_status", True)
    else:
        ctx.note("git", "not installed")

    if shutil.which("rsync"):
        mirror = workloads / "rsync-mirror"
        mirror.mkdir()
        _run_tool(["rsync", "-a", f"{tree}/", f"{mirror}/"], workloads)
        check(
            read(mirror / "nested" / "leaf") == b"leaf" * 512,
            "rsync mirrored the wrong bytes",
        )
        ctx.record("rsync.leaf_size", os.lstat(mirror / "nested" / "leaf").st_size)
    else:
        ctx.note("rsync", "not installed")


def _run_tool(command: list[str], cwd: Path) -> str:
    result = subprocess.run(command, cwd=cwd, text=True, capture_output=True)
    if result.returncode:
        detail = (result.stderr or result.stdout).strip()
        raise AssertionError(f"{' '.join(command)} failed in {cwd}: {detail}")
    return result.stdout


# --------------------------------------------------------------------------
# Regressions from docs/BUGS.md
# --------------------------------------------------------------------------


def test_regression_b7_renamed_directory(ctx: Context) -> None:
    """A renamed directory must stay traversable (B7).

    The reported failure was a directory that appeared in `ls` but could not be
    entered, because rename replaced the inode the kernel still had cached.
    """
    root = ctx.root
    original = root / "b7 original dir"
    original.mkdir()
    write_durable(original / "child file.txt", b"b7-child")
    (original / "sub dir").mkdir()
    write_durable(original / "sub dir" / "deep", b"b7-deep")

    before = os.lstat(original).st_ino
    renamed = root / "b7 renamed dïr ünï"
    os.rename(original, renamed)

    check(renamed.is_dir(), "renamed directory is not a directory")
    check(set(os.listdir(renamed)) == {"child file.txt", "sub dir"}, "renamed directory lost children")
    check(read(renamed / "child file.txt") == b"b7-child", "child unreadable after directory rename")
    check(read(renamed / "sub dir" / "deep") == b"b7-deep", "grandchild unreadable after rename")
    ctx.record("children", sorted(os.listdir(renamed)))
    ctx.note("inode_stable", os.lstat(renamed).st_ino == before)

    # Writing through the new path proves the entry is not merely readable.
    write_durable(renamed / "child file.txt", b"b7-rewritten")
    check(read(renamed / "child file.txt") == b"b7-rewritten", "write through renamed directory lost")


def test_regression_b69_identical_rewrite(ctx: Context) -> None:
    """Rewriting identical bytes must not fork a `(sync-conflict …)` copy (B69).

    Revision identity used to be `(mtime, size)`, so a rewrite that changed the
    mtime looked like a divergent revision and the sync engine kept both.
    """
    daemon = ctx.require_daemon()
    root = ctx.root
    path = root / "b69-identical.bin"
    payload = b"b69-identical-payload\n" * 4096

    write_durable(path, payload)
    daemon.wait_for_queue()
    time.sleep(1)
    write_durable(path, payload)
    daemon.wait_for_queue()

    conflicts = _conflict_copies(root)
    check(not conflicts, f"identical rewrite produced conflict copies: {conflicts}")
    check_bytes(read(path), payload, "identical rewrite changed the file contents")
    ctx.record("conflicts", [])


def test_regression_b70_transient_download_name(ctx: Context) -> None:
    """An in-flight browser download must not be sealed as a revision (B70).

    A `.crdownload` is a partial file the browser renames on completion. Sealing
    it produced an upload of the partial bytes and then a conflict fork against
    the finished name.
    """
    daemon = ctx.require_daemon()
    root = ctx.root
    partial = root / "b70-download.zip.crdownload"
    final = root / "b70-download.zip"
    payload = b"b70-final-payload\n" * 8192

    # Write the partial in chunks with a pause, the way a download lands.
    fd = os.open(partial, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
    try:
        half = len(payload) // 2
        os.write(fd, payload[:half])
        os.fsync(fd)
        time.sleep(2)
        os.write(fd, payload[half:])
        os.fsync(fd)
    finally:
        os.close(fd)
    os.rename(partial, final)
    daemon.wait_for_queue()

    listing = set(os.listdir(root))
    check(final.name in listing, "completed download is missing after rename")
    check(partial.name not in listing, "transient download name survived the rename")
    conflicts = _conflict_copies(root)
    check(not conflicts, f"download rename produced conflict copies: {conflicts}")
    check_bytes(read(final), payload, "completed download has the wrong bytes")
    ctx.record("final.digest", hashlib.sha256(read(final)).hexdigest())
    ctx.record("conflicts", [])


def test_regression_b74_rename_after_close(ctx: Context) -> None:
    """Write, close, rename: the content must reach Drive (B74).

    Not a `.crdownload` and not an overwrite — the plain temp-file dance that
    every editor, browser, `rsync` and `git` performs. Renaming a file whose
    create is still queued lost the staged bytes and published an empty node,
    so this is the narrowest reproduction of that loss.
    """
    daemon = ctx.require_daemon()
    root = ctx.root
    payload = b"b74-rename-after-close\n" * 6144

    temporary = root / "b74-staging.tmp"
    final = root / "b74-final.bin"
    write_durable(temporary, payload)
    os.rename(temporary, final)
    daemon.wait_for_queue()
    # The queue reports empty the moment the create lands; the revision that
    # carries the bytes can still be in flight behind it.
    time.sleep(5)
    daemon.wait_for_queue()

    check(final.is_file(), "renamed file is missing after the queue drained")
    check_bytes(read(final), payload, "renamed file lost its content")
    check(not _conflict_copies(root), "rename after close produced conflict copies")
    ctx.record("digest", hashlib.sha256(read(final)).hexdigest())


SHARED_DIR_NAME = "Shared with me"


def _locations(daemon: Daemon) -> list[dict]:
    """Every local Proton Drive location the daemon reports."""
    return json.loads(daemon.command("locations", json_output=True)).get("items", [])


def _shared_with_me(daemon: Daemon) -> list[dict]:
    return json.loads(daemon.command("shared-with-me", json_output=True)).get("entries", [])


def _shared_root(daemon: Daemon, mount: Path, role: str) -> Path:
    """The local path of an accepted share held at `role`.

    Skips rather than fails when the account has no such share: the role cases
    need a *second* account to have shared something at that role, which most
    runs will not have (mount-architecture.md §7).
    """
    entries = [e for e in _shared_with_me(daemon) if e.get("role") == role]
    if not entries:
        raise Skip(f"no accepted {role}-role share on this account")
    for entry in entries:
        # `path` is mount-relative and filled in only once the synthetic
        # directory has interned the node, so list it first.
        with contextlib.suppress(OSError):
            os.listdir(mount / SHARED_DIR_NAME)
        relative = entry.get("path") or ""
        if relative:
            candidate = mount / relative
            if candidate.exists():
                return candidate
    raise Skip(f"{role}-role share is not resident under {SHARED_DIR_NAME}/")


def test_regression_b34_viewer_share_is_read_only(ctx: Context) -> None:
    """A viewer-role share must be read-only *and* queue nothing (B34).

    The harm B34 names is not the mode bits: it is that a local write into a
    read-only share was accepted, entered `pending_op`, and then failed 403 in
    the drain forever. So step 4 — the queue depth being unchanged across the
    attempts — is the assertion that distinguishes the fix from cosmetics.
    """
    daemon = ctx.require_daemon()
    mount = daemon.enclosing_mount(ctx.root)
    root = _shared_root(daemon, mount, "viewer")

    before = daemon.status().get("mount") or {}
    mode = stat.S_IMODE(os.stat(root).st_mode)
    check(mode == 0o555, f"viewer share is {mode:o}, expected 555")
    check(not os.access(root, os.W_OK), "viewer share reports itself writable")

    target = root / "b34-should-not-exist"
    expect_errno({errno.EACCES}, lambda: os.mkdir(target), "mkdir in a viewer share")
    expect_errno(
        {errno.EACCES},
        lambda: os.open(target, os.O_CREAT | os.O_WRONLY, 0o600),
        "create in a viewer share",
    )
    for child in sorted(root.iterdir())[:1]:
        child_mode = stat.S_IMODE(os.stat(child).st_mode)
        expected = 0o555 if child.is_dir() else 0o444
        check(child_mode == expected, f"{child.name} is {child_mode:o}, expected {expected:o}")
        expect_errno(
            {errno.EACCES},
            lambda: os.open(child, os.O_WRONLY),
            "open-for-write in a viewer share",
        )
        expect_errno(
            {errno.EACCES},
            lambda: os.rename(child, child.with_name("b34-renamed")),
            "rename in a viewer share",
        )
        expect_errno(
            {errno.EACCES},
            lambda: (os.rmdir(child) if child.is_dir() else os.unlink(child)),
            "remove from a viewer share",
        )

    after = daemon.status().get("mount") or {}
    for key in ("pending_uploads", "pending_changes"):
        check(
            before.get(key, 0) == after.get(key, 0),
            f"a refused write changed {key}: {before.get(key)} -> {after.get(key)}",
        )
    ctx.record("viewer.mode", f"{mode:o}")
    ctx.record("viewer.queue_unchanged", True)


def test_regression_b34b_editor_share_still_writes(ctx: Context) -> None:
    """The fail-open half of B34: an editor share stays writable.

    A permission model that denies everything would pass the viewer case and
    break the feature, so this is the guard that the enforcement is scoped.
    """
    daemon = ctx.require_daemon()
    mount = daemon.enclosing_mount(ctx.root)
    root = _shared_root(daemon, mount, "editor")

    is_dir = root.is_dir()
    mode = stat.S_IMODE(os.stat(root).st_mode)
    expected = 0o755 if is_dir else 0o644
    check(mode == expected, f"editor share is {mode:o}, expected {expected:o}")
    check(os.access(root, os.W_OK), "editor share reports itself unwritable")
    ctx.record("editor.mode", f"{mode:o}")

    # A shared *file* is someone else's document: proving the mode bits and the
    # kernel's own W_OK answer is as far as this case goes. Only a shared folder
    # gets a real write, and only into a file this suite created.
    if not is_dir:
        ctx.note("editor.write", "skipped: the editor share is a file")
        return

    payload = b"b34b-editor-write\n" * 64
    target = root / "b34b-editor-write.bin"
    write_durable(target, payload)
    daemon.wait_for_queue()
    try:
        check_bytes(read(target), payload, "editor-share write did not read back")
    finally:
        with contextlib.suppress(OSError):
            os.unlink(target)
        daemon.wait_for_queue()


def test_shared_directory_contract(ctx: Context) -> None:
    """The synthetic `Shared with me/` directory behaves like a real directory.

    It is a real `Node` on a reserved volume rather than a FUSE special case, so
    it must enumerate, stat and refuse mutation like any other read-only folder —
    and must not be creatable or removable by the user.
    """
    daemon = ctx.require_daemon()
    mount = daemon.enclosing_mount(ctx.root)
    shared = mount / SHARED_DIR_NAME
    if not shared.is_dir():
        raise Skip(f"{SHARED_DIR_NAME}/ is not present in this mount")

    mode = stat.S_IMODE(os.stat(shared).st_mode)
    check(mode == 0o555, f"{SHARED_DIR_NAME}/ is {mode:o}, expected 555")
    # Enumerating must not raise: it is the operation the whole feature exists for.
    names = sorted(os.listdir(shared))
    expect_errno(
        {errno.EACCES},
        lambda: os.mkdir(shared / "b34-synthetic-child"),
        f"mkdir inside {SHARED_DIR_NAME}/",
    )
    expect_errno(
        {errno.EACCES, errno.ENOTEMPTY, errno.EBUSY},
        lambda: os.rmdir(shared),
        f"rmdir {SHARED_DIR_NAME}/",
    )
    expect_errno(
        {errno.EACCES, errno.EEXIST},
        lambda: os.rename(shared, mount / "b34-renamed-shared"),
        f"rename {SHARED_DIR_NAME}/",
    )
    ctx.record("shared.mode", f"{mode:o}")
    ctx.note("shared.entries", len(names))


def test_regression_b79_ondemand_location_accepts_writes(ctx: Context) -> None:
    """An on-demand device folder must accept writes (B79).

    Every mount holds its own inode space, and the queue guard intersects the
    access each one reports. The primary mount hydrates the device folder's
    nodes parentless (their device root is never a persisted node), so a
    fail-closed classification there denied every write in the *secondary*
    mount while its own state said Owner. The symptom is EACCES on a plain
    `touch` in `~/Documents` while `~/ProtonDrive` accepts one.
    """
    daemon = ctx.require_daemon()
    locations = [
        location
        for location in _locations(daemon)
        if location.get("kind", {}).get("kind") == "device"
        and location.get("mode") == "ondemand"
        and location.get("mounted")
        and location.get("access") == "rw"
    ]
    if not locations:
        raise Skip("no mounted on-demand device folder on this machine")

    root = Path(locations[0]["local_path"])
    payload = b"b79-ondemand-write\n" * 128
    directory = root / "pdfs-b79-acceptance"
    target = directory / "write.bin"
    try:
        os.mkdir(directory)
        write_durable(target, payload)
        daemon.wait_for_queue()
        check_bytes(read(target), payload, "on-demand write did not read back")
    finally:
        with contextlib.suppress(OSError):
            os.unlink(target)
        with contextlib.suppress(OSError):
            os.rmdir(directory)
        with contextlib.suppress(Exception):
            daemon.wait_for_queue()
    ctx.record("b79.location", str(root))


def _conflict_copies(root: Path) -> list[str]:
    return sorted(name for name in os.listdir(root) if "(sync-conflict" in name)


def test_durability_across_restart(ctx: Context) -> None:
    """Written bytes must survive a daemon restart (staging/ and recovery/).

    Those directories can hold the only copy of a file whose upload has not
    finished. This is the one case that proves the queue is persistent rather
    than merely in-memory.
    """
    daemon = ctx.require_daemon()
    if not daemon.may_restart:
        raise Skip("restart drill not requested; pass --durability")
    root = ctx.root
    path = root / "durability.bin"
    payload = hashlib.sha256(b"durability").digest() * 8192
    write_durable(path, payload)
    daemon.wait_for_queue()

    mountpoint = daemon.enclosing_mount(root)
    daemon.restart()
    daemon.wait_for_mount(mountpoint)
    daemon.wait_for_queue()

    check(path.is_file(), "file vanished across a daemon restart")
    check(read(path) == payload, "file contents changed across a daemon restart")
    check(not _conflict_copies(root), "restart produced conflict copies")
    ctx.record("digest", hashlib.sha256(read(path)).hexdigest())


class Case:
    def __init__(self, name: str, run, kinds: tuple[str, ...] = (REFERENCE, LIVE)) -> None:
        self.name = name
        self.run = run
        self.kinds = kinds


TESTS = [
    Case("creation and open flags", test_create_flags),
    Case("positioned and vectored I/O", test_positioned_and_vectored_io),
    Case("truncate, growth, sparse ranges, and EOF", test_resize_and_sparse_io),
    Case("mmap and zero-copy copy paths", test_mmap_and_copy_paths),
    Case("namespace operations and POSIX errors", test_namespace_and_errors),
    Case("open-handle lifetime across unlink and rename", test_open_lifetime),
    Case("names, lookup, stat, and enumeration", test_names_and_enumeration),
    Case("readdir stability under concurrent mutation", test_readdir_stability),
    Case("metadata updates and path-based truncate", test_metadata_updates),
    Case("extended attributes and the thumbnail interface", test_extended_attributes),
    Case("allocation, punched holes, and hole-seeking", test_allocation_and_holes),
    Case("unsupported operations refuse cleanly", test_unsupported_operations),
    Case("independent and shared-file concurrency", test_concurrency),
    Case("application workloads (editor, tar, sqlite, git, rsync)", test_application_workloads),
    Case("regression B7: renamed directory stays traversable", test_regression_b7_renamed_directory),
    Case("regression B69: identical rewrite makes no conflict", test_regression_b69_identical_rewrite, (LIVE,)),
    Case("regression B70: transient download name is not sealed", test_regression_b70_transient_download_name, (LIVE,)),
    Case("regression B74: rename after close keeps its content", test_regression_b74_rename_after_close, (LIVE,)),
    Case("regression B34: a viewer share is read-only and queues nothing", test_regression_b34_viewer_share_is_read_only, (LIVE,)),
    Case("regression B34b: an editor share still writes", test_regression_b34b_editor_share_still_writes, (LIVE,)),
    Case("synthetic Shared-with-me directory contract", test_shared_directory_contract, (LIVE,)),
    Case("regression B79: an on-demand device folder accepts writes", test_regression_b79_ondemand_location_accepts_writes, (LIVE,)),
    Case("durability across a daemon restart", test_durability_across_restart, (LIVE,)),
]


# --------------------------------------------------------------------------
# Runner
# --------------------------------------------------------------------------


class Result:
    def __init__(self, name: str, target: str, status: str, seconds: float, message: str = "") -> None:
        self.name = name
        self.target = target
        self.status = status
        self.seconds = seconds
        self.message = message

    def as_dict(self) -> dict:
        return {
            "name": self.name,
            "target": self.target,
            "status": self.status,
            "seconds": round(self.seconds, 3),
            "message": self.message,
        }


class Run:
    def __init__(self, label: str, kind: str, root: Path) -> None:
        self.label = label
        self.kind = kind
        self.root = root
        self.results: list[Result] = []
        self.compared: dict[str, object] = {}
        self.noted: dict[str, object] = {}
        self.ran: set[str] = set()
        self.digest = ""

    @property
    def failed(self) -> bool:
        return any(result.status in {"fail", "timeout"} for result in self.results)


REPORT: list[Run] = []


def select_cases(kind: str, pool: list[Case] | None = None) -> list[Case]:
    selected = os.environ.get("PDFS_ACCEPTANCE_ONLY")
    cases = [case for case in (TESTS if pool is None else pool) if kind in case.kinds]
    if not selected:
        return cases
    needle = selected.lower()
    # A filter that names a live-only case (every regression case is one) matches
    # nothing in the reference run, which is not an error — the reference target
    # simply has nothing to do. Only a filter that matches *no case at all* is a
    # typo worth failing on.
    check(
        any(needle in case.name.lower() for case in TESTS + MOVE_CASES),
        f"PDFS_ACCEPTANCE_ONLY={selected!r} matched no tests",
    )
    return [case for case in cases if needle in case.name.lower()]


def reap_stale_roots(parent: Path, max_age: int) -> None:
    """Remove abandoned roots from crashed runs so preconditions stay meaningful.

    Only exact `pdfs-acceptance-<32 hex>` names are eligible, and only once they
    are older than the age cutoff, so a concurrent run's tree is never touched.
    """
    if max_age <= 0:
        return
    cutoff = time.time() - max_age
    try:
        entries = list(parent.iterdir())
    except OSError as error:
        print(f"WARNING: could not scan {parent} for stale roots: {error}")
        return
    for entry in entries:
        if not STALE_ROOT.match(entry.name):
            continue
        try:
            if not entry.is_dir() or entry.stat().st_mtime > cutoff:
                continue
        except OSError:
            continue
        print(f"[reap] removing abandoned acceptance root {entry}")
        shutil.rmtree(entry, ignore_errors=True)


def run_contract(
    parent: Path,
    label: str,
    kind: str = REFERENCE,
    daemon=None,
    timeout: int = DEFAULT_TIMEOUT,
    fail_fast: bool = False,
) -> Run:
    reap_stale_roots(parent, int(os.environ.get("PDFS_ACCEPTANCE_REAP_AGE", "3600")))
    root = parent / f"pdfs-acceptance-{uuid.uuid4().hex}"
    root.mkdir()
    run = Run(label, kind, root)
    REPORT.append(run)
    print(f"[target] {label}: {parent}")
    obs = Observations()
    try:
        for case in select_cases(kind):
            obs.scope(case.name)
            context = Context(root, obs, kind, daemon)
            started = time.monotonic()
            print(f"  [test] {case.name}")
            try:
                with time_limit(timeout, case.name):
                    case.run(context)
            except Skip as reason:
                elapsed = time.monotonic() - started
                print(f"    SKIP  {reason}")
                run.results.append(Result(case.name, label, "skip", elapsed, str(reason)))
                continue
            except TestTimeout as reason:
                elapsed = time.monotonic() - started
                print(f"    TIMEOUT  {reason}")
                run.results.append(Result(case.name, label, "timeout", elapsed, str(reason)))
                # A timeout means the mount may be wedged; further cases would
                # only produce noise, and cleanup already has to fight for it.
                break
            except BaseException as error:  # noqa: BLE001 - reported, then continued
                elapsed = time.monotonic() - started
                detail = f"{type(error).__name__}: {error}"
                print(f"    FAIL  {detail}")
                run.results.append(Result(case.name, label, "fail", elapsed, detail))
                if fail_fast:
                    raise
                continue
            elapsed = time.monotonic() - started
            print(f"    ok    {elapsed:.2f}s")
            run.results.append(Result(case.name, label, "pass", elapsed))
            run.ran.add(case.name)
        run.compared = dict(obs.compared)
        run.noted = dict(obs.noted)
        digest_path = root / "positioned.bin"
        if digest_path.exists():
            run.digest = hashlib.sha256(read(digest_path)).hexdigest()
        return run
    except BaseException:
        shutil.rmtree(root, ignore_errors=True)
        raise


def compare_runs(reference: Run, target: Run) -> list[str]:
    """Diff the mount's recorded semantics against the local filesystem's.

    Only keys from cases that passed on both sides are compared: a case that
    failed or was skipped has already been reported, and its partial
    observations would just repeat that failure in a less useful form.
    """
    shared = reference.ran & target.ran
    divergences: list[str] = []
    for key, expected in reference.compared.items():
        case = key.split(".", 1)[0]
        if case not in shared:
            continue
        if key not in target.compared:
            divergences.append(f"{key}: missing on {target.label}")
            continue
        actual = target.compared[key]
        if actual != expected:
            divergences.append(f"{key}: {target.label}={actual!r} reference={expected!r}")
    for key in target.compared:
        case = key.split(".", 1)[0]
        if case in shared and key not in reference.compared:
            divergences.append(f"{key}: present on {target.label}, absent from reference")
    return divergences


def note_capability_differences(reference: Run, target: Run) -> list[str]:
    lines = []
    for key, expected in reference.noted.items():
        if key in target.noted and target.noted[key] != expected:
            lines.append(f"{key}: {target.label}={target.noted[key]!r} reference={expected!r}")
    return lines


def write_reports(json_path: Path | None, junit_path: Path | None) -> None:
    if json_path:
        payload = {
            "runs": [
                {
                    "label": run.label,
                    "kind": run.kind,
                    "results": [result.as_dict() for result in run.results],
                    "observations": {
                        key: _jsonable(value) for key, value in sorted(run.compared.items())
                    },
                    "notes": {key: _jsonable(value) for key, value in sorted(run.noted.items())},
                }
                for run in REPORT
            ]
        }
        json_path.write_text(json.dumps(payload, indent=2, sort_keys=True))
        print(f"[report] wrote {json_path}")
    if junit_path:
        suites = ElementTree.Element("testsuites")
        for run in REPORT:
            suite = ElementTree.SubElement(
                suites,
                "testsuite",
                name=run.label,
                tests=str(len(run.results)),
                failures=str(sum(1 for r in run.results if r.status in {"fail", "timeout"})),
                skipped=str(sum(1 for r in run.results if r.status == "skip")),
                time=f"{sum(r.seconds for r in run.results):.3f}",
            )
            for result in run.results:
                case = ElementTree.SubElement(
                    suite,
                    "testcase",
                    name=result.name,
                    classname=run.label,
                    time=f"{result.seconds:.3f}",
                )
                if result.status in {"fail", "timeout"}:
                    failure = ElementTree.SubElement(case, "failure", type=result.status)
                    failure.text = result.message
                elif result.status == "skip":
                    ElementTree.SubElement(case, "skipped", message=result.message)
        ElementTree.ElementTree(suites).write(junit_path, encoding="unicode", xml_declaration=True)
        print(f"[report] wrote {junit_path}")


def _jsonable(value):
    if isinstance(value, (bytes, bytearray)):
        return value.decode("utf-8", "backslashreplace")
    if isinstance(value, (set, frozenset)):
        return sorted(value)
    return value


def report_timings(budget: float) -> list[str]:
    slow = []
    for run in REPORT:
        for result in run.results:
            if result.status == "pass" and budget and result.seconds > budget:
                slow.append(f"{run.label}: {result.name} took {result.seconds:.1f}s (budget {budget:.0f}s)")
    return slow


class JournalWatch:
    """Fail a run that leaves errors in the daemon's journal.

    A suite can pass every assertion while the daemon logs a stream of failures
    behind it; that has happened here before, and it was only noticed by reading
    the journal by hand.
    """

    def __init__(self, unit: str = "proton-drive.service") -> None:
        self.unit = unit
        self.since = 0.0

    def start(self) -> None:
        if shutil.which("journalctl") is None:
            print("WARNING: journalctl is unavailable; skipping the journal check")
            self.since = 0.0
            return
        self.since = time.time()

    def errors(self) -> list[str]:
        if not self.since:
            return []
        result = subprocess.run(
            [
                "journalctl",
                "--user",
                "-u",
                self.unit,
                "--since",
                f"@{int(self.since)}",
                "--no-pager",
                "-o",
                "cat",
            ],
            text=True,
            capture_output=True,
        )
        if result.returncode:
            return [f"journalctl failed: {result.stderr.strip()}"]
        # Filtering on syslog priority (`-p err`) finds nothing: the daemon writes
        # tracing levels as text on stdout, which journald files at the unit's
        # default priority. Match the level token instead, after stripping the
        # ANSI colouring tracing emits when it thinks it has a terminal.
        errors = []
        for line in result.stdout.splitlines():
            plain = ANSI.sub("", line)
            if ERROR_LEVEL.search(plain):
                errors.append(plain.strip())
        return errors


# --------------------------------------------------------------------------
# Daemon control
# --------------------------------------------------------------------------


class Daemon:
    """The `pdfs` CLI, plus the unit control the durability drill needs."""

    def __init__(self, timeout: int, may_restart: bool = False) -> None:
        self.timeout = timeout
        self.may_restart = may_restart
        self.pdfs = os.environ.get("PDFS_ACCEPTANCE_PDFS", "pdfs")
        self.unit = os.environ.get("PDFS_ACCEPTANCE_UNIT", "proton-drive.service")

    @classmethod
    def discover(cls, timeout: int, may_restart: bool = False) -> Daemon | None:
        daemon = cls(timeout, may_restart)
        if shutil.which(daemon.pdfs) is None and not Path(daemon.pdfs).is_file():
            return None
        try:
            daemon.status()
        except Exception:
            return None
        return daemon

    def command(self, *args: str, json_output: bool = False) -> str:
        command = [self.pdfs]
        if json_output:
            command.append("--json")
        command.extend(args)
        result = subprocess.run(command, text=True, capture_output=True, timeout=self.timeout)
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip()
            raise RuntimeError(f"{' '.join(command)} failed: {detail}")
        return result.stdout

    def status(self) -> dict:
        return json.loads(self.command("status", json_output=True))

    def wait_for_queue(self) -> None:
        deadline = time.monotonic() + self.timeout
        last = None
        while time.monotonic() < deadline:
            last = self.status().get("mount") or {}
            if last.get("pending_uploads", 0) == 0 and last.get("pending_changes", 0) == 0:
                return
            time.sleep(1)
        raise TimeoutError(f"daemon mutation queue did not drain: {last}")

    def enclosing_mount(self, path: Path) -> Path:
        current = path.resolve()
        while current != current.parent:
            if os.path.ismount(current):
                return current
            current = current.parent
        return current

    def restart(self) -> None:
        print(f"[restart] systemctl --user restart {self.unit}")
        result = subprocess.run(
            ["systemctl", "--user", "restart", self.unit],
            text=True,
            capture_output=True,
            timeout=self.timeout,
        )
        if result.returncode:
            raise RuntimeError(f"could not restart {self.unit}: {result.stderr.strip()}")

    def wait_for_mount(self, mountpoint: Path) -> None:
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            if os.path.ismount(mountpoint):
                with contextlib.suppress(OSError):
                    os.listdir(mountpoint)
                    return
            time.sleep(1)
        raise TimeoutError(f"{mountpoint} did not come back within {self.timeout}s")


# --------------------------------------------------------------------------
# Live and managed modes
# --------------------------------------------------------------------------


def wait_for_copy(root: Path, relative: Path, digest: str, timeout: int) -> None:
    deadline = time.monotonic() + timeout
    target = root / relative
    while time.monotonic() < deadline:
        try:
            if target.is_file() and hashlib.sha256(read(target)).hexdigest() == digest:
                return
        except OSError:
            pass
        time.sleep(2)
    raise AssertionError(f"{target} did not converge byte-for-byte within {timeout}s")


class ManagedSyncPair:
    """Two sync registrations created by this run and removed on exit."""

    def __init__(self, paths: list[Path], timeout: int) -> None:
        self.paths = [path.resolve() for path in paths]
        self.timeout = timeout
        self.pdfs = os.environ.get("PDFS_ACCEPTANCE_PDFS", "pdfs")
        self.ids: dict[Path, int] = {}
        self.sentinels = {
            path: hashlib.sha256(f"pdfs-preservation:{path}:{uuid.uuid4()}".encode()).digest()
            * 4096
            for path in self.paths
        }

    def command(self, *args: str, json_output: bool = False) -> str:
        command = [self.pdfs]
        if json_output:
            command.append("--json")
        command.extend(args)
        result = subprocess.run(command, text=True, capture_output=True)
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip()
            raise RuntimeError(f"{' '.join(command)} failed: {detail}")
        return result.stdout

    def folders(self) -> list[dict]:
        value = json.loads(self.command("sync", "list", json_output=True))
        return value["items"]

    def wait_for_queue(self) -> None:
        deadline = time.monotonic() + self.timeout
        last = None
        while time.monotonic() < deadline:
            value = json.loads(self.command("status", json_output=True))
            last = value.get("mount") or {}
            if last.get("pending_uploads", 0) == 0 and last.get("pending_changes", 0) == 0:
                return
            time.sleep(1)
        raise TimeoutError(f"daemon mutation queue did not drain: {last}")

    def remove_tree(self, root: Path) -> None:
        deadline = time.monotonic() + self.timeout
        while True:
            try:
                shutil.rmtree(root)
                return
            except FileNotFoundError:
                return
            except OSError as error:
                if error.errno != errno.ENOTEMPTY or time.monotonic() >= deadline:
                    survivors = []
                    if root.exists():
                        for directory, dirs, files in os.walk(root):
                            survivors.extend(str(Path(directory) / name) for name in dirs + files)
                    raise OSError(
                        error.errno,
                        f"{error.strerror}; surviving test entries: {survivors[:50]}",
                        error.filename,
                    ) from error
                # A queued namespace operation can land between rmtree's
                # enumeration and rmdir. Re-walk the test-owned tree.
                time.sleep(0.25)

    def validate(self) -> None:
        if shutil.which(self.pdfs) is None and not Path(self.pdfs).is_file():
            raise RuntimeError(
                f"cannot find {self.pdfs!r}; set PDFS_ACCEPTANCE_PDFS to the CLI binary"
            )
        if self.paths[0] == self.paths[1]:
            raise ValueError("managed live paths must be different directories")
        deadline = time.monotonic() + self.timeout
        last_error = None
        while True:
            try:
                current = self.folders()
                break
            except Exception as error:
                last_error = error
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"pdfs daemon did not become ready within {self.timeout}s: {last_error}"
                    ) from error
                time.sleep(1)
        registered = {Path(item["local_path"]).resolve() for item in current}
        for path in self.paths:
            reap_stale_roots(path, int(os.environ.get("PDFS_ACCEPTANCE_REAP_AGE", "3600")))
            check(path.is_dir(), f"managed path is not a directory: {path}")
            check(not any(path.iterdir()), f"managed path is not empty: {path}")
            check(path not in registered, f"managed path is already registered: {path}")
            check(not is_mountpoint(path), f"managed path is already a mount: {path}")

    def wait_for(self, path: Path, *, mode: str | None = None) -> dict:
        deadline = time.monotonic() + self.timeout
        last = None
        while time.monotonic() < deadline:
            for item in self.folders():
                if Path(item["local_path"]).resolve() != path:
                    continue
                last = item
                if (
                    item["state"] == "idle"
                    and item.get("pending_mode") is None
                    and (mode is None or item["mode"] == mode)
                ):
                    return item
                if item["state"] in {"error", "conflict"}:
                    raise RuntimeError(f"sync folder {path} entered {item['state']}: {item}")
            time.sleep(2)
        raise TimeoutError(f"sync folder {path} did not become idle in mode {mode}: {last}")

    def create(self) -> None:
        self.validate()
        for path in self.paths:
            print(f"[setup] registering empty sync folder {path}")
            self.command("sync", "add", str(path))
            item = self.wait_for(path, mode="mirror")
            self.ids[path] = int(item["id"])
        for path in self.paths:
            write_durable(path / "pdfs-mode-preservation.bin", self.sentinels[path])
            self.force_sync(path)

    def force_sync(self, path: Path) -> None:
        before = int(self.wait_for(path)["last_sync"])
        # last_sync has one-second resolution. Ensure this requested pass cannot
        # finish with the same timestamp and look indistinguishable from no pass.
        while int(time.time()) <= before:
            time.sleep(0.1)
        self.command("sync", "now", str(self.ids[path]))
        deadline = time.monotonic() + self.timeout
        last = None
        while time.monotonic() < deadline:
            last = next(
                (item for item in self.folders() if Path(item["local_path"]).resolve() == path),
                None,
            )
            if last and last["state"] == "idle" and int(last["last_sync"]) > before:
                return
            if last and last["state"] in {"error", "conflict"}:
                raise RuntimeError(f"forced sync for {path} entered {last['state']}: {last}")
            time.sleep(1)
        raise TimeoutError(f"forced sync for {path} did not complete: {last}")

    def set_modes(self, modes: tuple[str, str]) -> None:
        changed_to_mirror: set[Path] = set()
        for path, mode in zip(self.paths, modes, strict=True):
            current = self.wait_for(path)
            if current["mode"] != mode:
                print(f"[setup] switching {path} to {mode}")
                self.command("sync", "mode", str(self.ids[path]), mode)
                if mode == "mirror":
                    changed_to_mirror.add(path)
        for path, mode in zip(self.paths, modes, strict=True):
            self.wait_for(path, mode=mode)
            # The mode row flips before the asynchronous restore pass starts,
            # and its prior idle/last_sync values remain visible meanwhile.
            # Demand a completed pass before inspecting restored local bytes.
            if path in changed_to_mirror:
                self.force_sync(path)
            mounted = is_mountpoint(path)
            check(mounted == (mode == "ondemand"), f"{path}: mode is {mode}, mounted={mounted}")
            check(
                read(path / "pdfs-mode-preservation.bin") == self.sentinels[path],
                f"{path}: preservation sentinel changed or vanished after switch to {mode}",
            )

    def cleanup(self) -> None:
        # An async add can create its row and then time out before create() has
        # recorded the id. validate() proved these exact paths were unregistered
        # at entry, so rows now at those paths belong to this run.
        try:
            for item in self.folders():
                path = Path(item["local_path"]).resolve()
                if path in self.paths:
                    self.ids[path] = int(item["id"])
        except Exception as error:
            print(f"WARNING: could not rediscover managed sync folders: {error}")
        for path, folder_id in reversed(list(self.ids.items())):
            try:
                print(f"[cleanup] removing test sync folder {path}")
                self.command("sync", "rm", str(folder_id), "--delete-remote")
            except Exception as error:
                print(f"WARNING: cleanup failed for sync folder {folder_id}: {error}")
                continue
            # Unregistering a mirror intentionally preserves its local copy.
            # Remove only the sentinel owned by this harness so the next run's
            # strict empty-directory precondition remains meaningful. Unknown
            # survivors are left untouched and will fail validate() next time.
            sentinel = path / "pdfs-mode-preservation.bin"
            try:
                if sentinel.exists() and read(sentinel) == self.sentinels[path]:
                    sentinel.unlink()
            except OSError as error:
                print(f"WARNING: could not remove managed sentinel {sentinel}: {error}")


# --------------------------------------------------------------------------
# Moves between locations
# --------------------------------------------------------------------------


class Location:
    """One place a move can start or end: a managed folder, or My files."""

    def __init__(self, name: str, folder: Path, mode: str, pair: ManagedSyncPair) -> None:
        self.name = name
        self.folder = folder
        self.mode = mode
        self.pair = pair
        self.root = folder / f"pdfs-acceptance-{uuid.uuid4().hex}"

    def settle(self) -> None:
        """Wait until Drive holds everything written here so far."""
        self.pair.wait_for_queue()
        if self.mode == "mirror":
            self.pair.force_sync(self.folder)

    def conflict_copies(self) -> list[str]:
        found = []
        for directory, dirs, files in os.walk(self.root):
            found.extend(
                str(Path(directory) / name) for name in dirs + files if "(sync-conflict" in name
            )
        return found


class MoveContext:
    def __init__(self, first: Location, second: Location, myfiles: Location | None) -> None:
        self.first = first
        self.second = second
        self.myfiles = myfiles
        self.pair = first.pair

    def move(self, *paths: Path) -> subprocess.CompletedProcess:
        """`pdfs move`, bounded: a wedged mount must not hang the harness."""
        command = [self.pair.pdfs, "move", *(str(path) for path in paths)]
        result = subprocess.run(
            command, text=True, capture_output=True, timeout=self.pair.timeout
        )
        detail = result.stderr.strip() or result.stdout.strip()
        if result.returncode and "different Proton Drive volumes" in detail:
            raise Skip(f"the locations are on different volumes: {detail}")
        return result

    def move_ok(self, *paths: Path) -> None:
        result = self.move(*paths)
        check(
            result.returncode == 0,
            f"pdfs move {' '.join(map(str, paths))} failed: "
            f"{result.stderr.strip() or result.stdout.strip()}",
        )

    def move_refused(self, *paths: Path) -> str:
        result = self.move(*paths)
        detail = result.stderr.strip() or result.stdout.strip()
        check(
            result.returncode != 0,
            f"pdfs move {' '.join(map(str, paths))} should have been refused: {detail}",
        )
        return detail


def _move_tree(root: Path, name: str) -> dict[Path, bytes]:
    """A small tree worth moving: nesting, an empty file, and a file spanning
    several content blocks, all with bytes unique to this run."""
    tag = uuid.uuid4().bytes
    files = {
        Path(name) / "note.txt": b"moved note " + tag,
        Path(name) / "nested" / "large.bin": hashlib.sha256(tag).digest() * (330 * 1024 // 32)
        + os.urandom(4099),
        Path(name) / "nested" / "deeper" / "empty.txt": b"",
    }
    for relative, data in files.items():
        (root / relative).parent.mkdir(parents=True, exist_ok=True)
        write_durable(root / relative, data)
    (root / name / "empty-dir").mkdir()
    return files


def _await_tree(dest: Location, name: str, files: dict[Path, bytes]) -> None:
    for relative, data in files.items():
        wait_for_copy(dest.root, relative, hashlib.sha256(data).hexdigest(), dest.pair.timeout)
    deadline = time.monotonic() + dest.pair.timeout
    while not (dest.root / name / "empty-dir").is_dir():
        check(time.monotonic() < deadline, f"{dest.name}: empty folder did not arrive")
        time.sleep(1)


def _check_moved(source: Location, dest: Location, name: str, files: dict[Path, bytes]) -> None:
    """After both sides have synced again: the tree is only at the destination,
    byte-for-byte, and neither side made conflict copies of it."""
    source.settle()
    dest.settle()
    check(
        not (source.root / name).exists(),
        f"{source.name}: {name} came back after moving it to {dest.name}",
    )
    for relative, data in files.items():
        check_bytes(read(dest.root / relative), data, f"{dest.name}: {relative} after the move")
    for location in (source, dest):
        copies = location.conflict_copies()
        check(not copies, f"{location.name}: the move left conflict copies: {copies}")


def _moves_between(mc: MoveContext, source: Location, dest: Location) -> None:
    name = f"tree-{uuid.uuid4().hex[:8]}"
    files = _move_tree(source.root, name)
    source.settle()
    mc.move_ok(source.root / name, dest.root)
    check(not (source.root / name).exists(), f"{source.name}: {name} is still there after the move")
    _await_tree(dest, name, files)
    _check_moved(source, dest, name, files)


def test_move_folder_first_to_second(mc: MoveContext) -> None:
    _moves_between(mc, mc.first, mc.second)


def test_move_folder_second_to_first(mc: MoveContext) -> None:
    _moves_between(mc, mc.second, mc.first)


def test_move_several_sources_at_once(mc: MoveContext) -> None:
    source, dest = mc.first, mc.second
    tag = uuid.uuid4().hex[:8]
    files = {
        Path(f"one-{tag}.txt"): b"first of several " + tag.encode(),
        Path(f"two-{tag}.bin"): os.urandom(70_001),
        Path(f"dir-{tag}") / "inside.txt": b"inside " + tag.encode(),
    }
    for relative, data in files.items():
        (source.root / relative).parent.mkdir(parents=True, exist_ok=True)
        write_durable(source.root / relative, data)
    source.settle()
    tops = sorted({relative.parts[0] for relative in files})
    mc.move_ok(*(source.root / top for top in tops), dest.root)
    for relative, data in files.items():
        wait_for_copy(dest.root, relative, hashlib.sha256(data).hexdigest(), dest.pair.timeout)
    source.settle()
    dest.settle()
    for top in tops:
        check(not (source.root / top).exists(), f"{source.name}: {top} came back after the move")
    for relative, data in files.items():
        check_bytes(read(dest.root / relative), data, f"{dest.name}: {relative} after the move")


def test_move_through_my_files(mc: MoveContext) -> None:
    if mc.myfiles is None:
        raise Skip("the daemon reports no My files mount")
    name = f"tree-{uuid.uuid4().hex[:8]}"
    files = _move_tree(mc.first.root, name)
    mc.first.settle()
    mc.move_ok(mc.first.root / name, mc.myfiles.root)
    _await_tree(mc.myfiles, name, files)
    _check_moved(mc.first, mc.myfiles, name, files)
    mc.move_ok(mc.myfiles.root / name, mc.second.root)
    _await_tree(mc.second, name, files)
    _check_moved(mc.myfiles, mc.second, name, files)


def _unchanged(location: Location, files: dict[Path, bytes], what: str) -> None:
    for relative, data in files.items():
        check_bytes(read(location.root / relative), data, f"{location.name}: {relative} {what}")


def test_move_refuses_a_name_taken_at_the_destination(mc: MoveContext) -> None:
    source, dest = mc.first, mc.second
    name = f"clash-{uuid.uuid4().hex[:8]}.txt"
    ours, theirs = b"source side " + name.encode(), b"destination side " + name.encode()
    write_durable(source.root / name, ours)
    write_durable(dest.root / name, theirs)
    source.settle()
    dest.settle()
    mc.move_refused(source.root / name, dest.root)
    source.settle()
    dest.settle()
    _unchanged(source, {Path(name): ours}, "after a refused move")
    _unchanged(dest, {Path(name): theirs}, "after a refused move")


def test_move_refuses_a_folder_into_itself(mc: MoveContext) -> None:
    source = mc.first
    name = f"self-{uuid.uuid4().hex[:8]}"
    files = {Path(name) / "sub" / "kept.txt": b"kept " + name.encode()}
    (source.root / name / "sub").mkdir(parents=True)
    write_durable(source.root / name / "sub" / "kept.txt", files[Path(name) / "sub" / "kept.txt"])
    source.settle()
    mc.move_refused(source.root / name, source.root / name / "sub")
    source.settle()
    _unchanged(source, files, "after refusing to move it into itself")


def test_move_refuses_a_mirror_copy_drive_lacks(mc: MoveContext) -> None:
    source, dest = next(
        ((a, b) for a, b in ((mc.first, mc.second), (mc.second, mc.first)) if a.mode == "mirror"),
        (None, None),
    )
    if source is None:
        raise Skip("needs a mirror source")
    name = f"unsynced-{uuid.uuid4().hex[:8]}"
    files = {Path(name) / "data.txt": b"synced " + name.encode()}
    (source.root / name).mkdir()
    write_durable(source.root / name / "data.txt", files[Path(name) / "data.txt"])
    # Sync skips symlinks, so Drive never gets this one: removing the local
    # copy after a move would lose it.
    os.symlink("data.txt", source.root / name / "link")
    source.settle()
    detail = mc.move_refused(source.root / name, dest.root)
    check("not fully synced" in detail, f"unexpected refusal: {detail}")
    check((source.root / name / "link").is_symlink(), "the symlink was lost by a refused move")
    _unchanged(source, files, "after a refused move")
    dest.settle()
    check(not (dest.root / name).exists(), f"{dest.name}: {name} appeared after a refused move")


def test_move_refuses_a_file_still_being_written(mc: MoveContext) -> None:
    source, dest = next(
        ((a, b) for a, b in ((mc.first, mc.second), (mc.second, mc.first)) if a.mode == "ondemand"),
        (None, None),
    )
    if source is None:
        raise Skip("needs an on-demand source")
    name = f"open-{uuid.uuid4().hex[:8]}"
    path = source.root / name / "open.txt"
    path.parent.mkdir()
    write_durable(path, b"before")
    source.settle()
    fd = os.open(path, os.O_WRONLY | os.O_APPEND)
    try:
        os.write(fd, b" and after")
        mc.move_refused(source.root / name, dest.root)
    finally:
        os.fsync(fd)
        os.close(fd)
    source.settle()
    _unchanged(source, {Path(name) / "open.txt": b"before and after"}, "after a refused move")
    dest.settle()
    check(not (dest.root / name).exists(), f"{dest.name}: {name} appeared after a refused move")


# Only --managed-live has two locations of its own to move between.
MOVE_CASES = [
    Case("move a folder from the first location to the second", test_move_folder_first_to_second, (LIVE,)),
    Case("move a folder from the second location to the first", test_move_folder_second_to_first, (LIVE,)),
    Case("move several sources in one command", test_move_several_sources_at_once, (LIVE,)),
    Case("move into My files and out again", test_move_through_my_files, (LIVE,)),
    Case("move refuses a name taken at the destination", test_move_refuses_a_name_taken_at_the_destination, (LIVE,)),
    Case("move refuses a folder into itself", test_move_refuses_a_folder_into_itself, (LIVE,)),
    Case("move refuses a mirror copy Drive lacks", test_move_refuses_a_mirror_copy_drive_lacks, (LIVE,)),
    Case("move refuses a file still being written", test_move_refuses_a_file_still_being_written, (LIVE,)),
]


def _my_files(pair: ManagedSyncPair) -> Path | None:
    try:
        status = json.loads(pair.command("status", json_output=True))
    except Exception as error:  # noqa: BLE001 - My files is optional here
        print(f"WARNING: could not ask the daemon for My files: {error}")
        return None
    mountpoint = (status.get("mount") or {}).get("mountpoint")
    if not mountpoint or not is_mountpoint(Path(mountpoint)):
        return None
    path = Path(mountpoint).resolve()
    return None if path in pair.paths else path


def run_move_contract(
    pair: ManagedSyncPair, modes: tuple[str, str], timeout: int, fail_fast: bool
) -> Run:
    """Move trees between the two managed folders, and through My files.

    Every case waits for Drive before and after, so a pass here means the move
    happened on Drive, the content arrived intact, and no side put anything
    back or made conflict copies.
    """
    first = Location("first", pair.paths[0], modes[0], pair)
    second = Location("second", pair.paths[1], modes[1], pair)
    my_files = _my_files(pair)
    myfiles = Location("My files", my_files, "ondemand", pair) if my_files else None
    locations = [first, second] + ([myfiles] if myfiles else [])
    label = f"managed move {modes[0]}/{modes[1]}"
    run = Run(label, LIVE, first.root)
    REPORT.append(run)
    print(f"[target] {label}")
    try:
        for location in locations:
            reap_stale_roots(location.folder, int(os.environ.get("PDFS_ACCEPTANCE_REAP_AGE", "3600")))
            location.root.mkdir()
        for location in locations:
            location.settle()
        context = MoveContext(first, second, myfiles)
        for case in select_cases(LIVE, MOVE_CASES):
            started = time.monotonic()
            print(f"  [test] {case.name}")
            try:
                with time_limit(timeout, case.name):
                    case.run(context)
            except Skip as reason:
                print(f"    SKIP  {reason}")
                run.results.append(
                    Result(case.name, label, "skip", time.monotonic() - started, str(reason))
                )
                continue
            except TestTimeout as reason:
                print(f"    TIMEOUT  {reason}")
                run.results.append(
                    Result(case.name, label, "timeout", time.monotonic() - started, str(reason))
                )
                break
            except BaseException as error:  # noqa: BLE001 - reported, then continued
                detail = f"{type(error).__name__}: {error}"
                print(f"    FAIL  {detail}")
                run.results.append(Result(case.name, label, "fail", time.monotonic() - started, detail))
                if fail_fast:
                    raise
                continue
            elapsed = time.monotonic() - started
            print(f"    ok    {elapsed:.2f}s")
            run.results.append(Result(case.name, label, "pass", elapsed))
            run.ran.add(case.name)
    finally:
        for location in locations:
            try:
                pair.remove_tree(location.root)
                location.settle()
            except Exception as error:  # noqa: BLE001 - cleanup must reach every root
                print(f"WARNING: could not remove {location.root}: {error}")
    return run


def is_mountpoint(path: Path) -> bool:
    """True for a distinct mount, including FUSE mounts over an existing dir."""
    return os.path.ismount(path)


def run_managed_matrix(paths: list[Path], reference: Run, args) -> None:
    timeout = int(os.environ.get("PDFS_ACCEPTANCE_SYNC_TIMEOUT", "180"))
    pair = ManagedSyncPair(paths, timeout)
    daemon = Daemon.discover(timeout, may_restart=args.durability)
    roots: list[Path] = []
    try:
        pair.create()
        matrices = [
            ("ondemand", "ondemand"),
            ("ondemand", "mirror"),
            ("mirror", "ondemand"),
            ("mirror", "mirror"),
        ]
        for modes in matrices:
            print(f"[matrix] first={modes[0]}, second={modes[1]}")
            pair.set_modes(modes)
            for path, mode in zip(pair.paths, modes, strict=True):
                run = run_contract(
                    path,
                    f"managed {mode} {path.name}",
                    kind=LIVE,
                    daemon=daemon,
                    timeout=args.timeout,
                    fail_fast=args.fail_fast,
                )
                roots.append(run.root)
                summarize_divergence(reference, run)
                pair.wait_for_queue()
                pair.remove_tree(run.root)
                roots.remove(run.root)
                pair.wait_for_queue()
                # Mirror changes are asynchronous; force and await a pass before
                # changing its mode so authored bytes/deletions cannot be lost.
                if mode == "mirror":
                    pair.force_sync(path)
            # A pass settles Drive several times over, so it gets the sync
            # timeout per settle rather than the per-case filesystem limit.
            run_move_contract(pair, modes, max(args.timeout, 8 * timeout), args.fail_fast)
            for path, mode in zip(pair.paths, modes, strict=True):
                if mode == "mirror":
                    pair.force_sync(path)
    finally:
        for root in roots:
            shutil.rmtree(root, ignore_errors=True)
        pair.cleanup()


def summarize_divergence(reference: Run, target: Run) -> None:
    divergences = compare_runs(reference, target)
    capabilities = note_capability_differences(reference, target)
    if capabilities:
        print(f"  [capabilities] {target.label} differs from the reference filesystem:")
        for line in capabilities:
            print(f"    - {line}")
    if divergences:
        print(f"  [DIVERGENCE] {target.label} does not match the reference filesystem:")
        for line in divergences:
            print(f"    ! {line}")
        target.results.append(
            Result(
                "differential comparison against the reference filesystem",
                target.label,
                "fail",
                0.0,
                "; ".join(divergences),
            )
        )
    else:
        print(f"  [differential] {target.label} matches the reference filesystem")
        target.results.append(
            Result(
                "differential comparison against the reference filesystem",
                target.label,
                "pass",
                0.0,
            )
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    live_mode = parser.add_mutually_exclusive_group()
    live_mode.add_argument("--live", nargs="+", type=Path, metavar="MOUNTPOINT")
    live_mode.add_argument(
        "--managed-live",
        nargs=2,
        type=Path,
        metavar=("EMPTY_DIR_A", "EMPTY_DIR_B"),
    )
    parser.add_argument("--list", action="store_true", help="print case names and exit")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT, help="per-case seconds")
    parser.add_argument("--fail-fast", action="store_true", help="stop at the first failure")
    parser.add_argument("--report-json", type=Path, metavar="PATH")
    parser.add_argument("--report-junit", type=Path, metavar="PATH")
    parser.add_argument(
        "--budget",
        type=float,
        default=float(os.environ.get("PDFS_ACCEPTANCE_BUDGET", "0")),
        help="report any case slower than this many seconds",
    )
    parser.add_argument(
        "--journal-check",
        action="store_true",
        help="fail if the daemon logs errors during the run",
    )
    parser.add_argument(
        "--durability",
        action="store_true",
        help="restart the daemon mid-suite and verify written bytes survive",
    )
    args = parser.parse_args()

    if args.list:
        for case in TESTS:
            print(f"{','.join(case.kinds):<18} {case.name}")
        for case in MOVE_CASES:
            print(f"{'managed':<18} {case.name}")
        return 0

    journal = JournalWatch(os.environ.get("PDFS_ACCEPTANCE_UNIT", "proton-drive.service"))
    if args.journal_check:
        journal.start()

    with tempfile.TemporaryDirectory(prefix="pdfs-api-reference-") as directory:
        reference = run_contract(
            Path(directory),
            "local filesystem API reference (no account)",
            kind=REFERENCE,
            timeout=args.timeout,
            fail_fast=args.fail_fast,
        )
        shutil.rmtree(reference.root, ignore_errors=True)
    if reference.failed:
        print("FAIL: the account-free contract failed against an ordinary filesystem")
    else:
        print("[pass] account-free filesystem API contract")

    try:
        if args.managed_live:
            run_managed_matrix(args.managed_live, reference, args)
        elif args.live:
            run_live(args.live, reference, args)
    finally:
        outcome = finish(args, journal)
    return outcome


def run_live(mountpoints: list[Path], reference: Run, args) -> None:
    completed: list[tuple[Path, Run]] = []
    convergence = os.environ.get("PDFS_ACCEPTANCE_CONVERGENCE", "0") == "1"
    timeout = int(os.environ.get("PDFS_ACCEPTANCE_SYNC_TIMEOUT", "120"))
    daemon = Daemon.discover(timeout, may_restart=args.durability)
    if daemon is None:
        print("WARNING: no reachable pdfs daemon; sync regressions will be skipped")
    try:
        # Views of one remote folder must not independently create the same
        # fixtures. Exercise the primary, then observe that exact tree through
        # every secondary. Without convergence mode, each mount is independent
        # and receives the full contract.
        targets = mountpoints[:1] if convergence and len(mountpoints) > 1 else mountpoints
        for mountpoint in targets:
            run = run_contract(
                mountpoint.resolve(),
                f"live FUSE {mountpoint}",
                kind=LIVE,
                daemon=daemon,
                timeout=args.timeout,
                fail_fast=args.fail_fast,
            )
            completed.append((mountpoint.resolve(), run))
            summarize_divergence(reference, run)
        if convergence and len(mountpoints) > 1:
            print("  [test] cross-mount remote convergence")
            source = completed[0][1]
            relative = source.root.relative_to(completed[0][0]) / "positioned.bin"
            for mountpoint in mountpoints[1:]:
                wait_for_copy(mountpoint.resolve(), relative, source.digest, timeout)
    finally:
        for _, run in completed:
            shutil.rmtree(run.root, ignore_errors=True)


def finish(args, journal: JournalWatch) -> int:
    write_reports(args.report_json, args.report_junit)

    slow = report_timings(args.budget)
    if slow:
        print("[timing] cases over budget:")
        for line in slow:
            print(f"    - {line}")

    journal_errors = journal.errors() if args.journal_check else []
    if journal_errors:
        print(f"[journal] the daemon logged {len(journal_errors)} error(s) during the run:")
        for line in journal_errors[:20]:
            print(f"    ! {line}")

    failures = [
        (run.label, result)
        for run in REPORT
        for result in run.results
        if result.status in {"fail", "timeout"}
    ]
    print()
    for run in REPORT:
        counts: dict[str, int] = {}
        for result in run.results:
            counts[result.status] = counts.get(result.status, 0) + 1
        detail = ", ".join(f"{count} {status}" for status, count in sorted(counts.items()))
        print(f"[summary] {run.label}: {detail}")

    if failures or journal_errors:
        for label, result in failures:
            print(f"FAIL {label}: {result.name}: {result.message}")
        return 1
    print("PASS: acceptance suite completed")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\ninterrupted", file=sys.stderr)
        sys.exit(130)
    except BrokenPipeError:
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        sys.exit(0)
    except (AssertionError, RuntimeError, TimeoutError, ValueError) as failure:
        # Setup and selection problems are the operator's to fix; a traceback
        # buries the one line that says what to change.
        print(f"error: {failure}", file=sys.stderr)
        sys.exit(2)
