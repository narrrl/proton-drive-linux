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
import platform
import random
import re
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import traceback
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
MIB = 1024 * 1024
# Drive stores content in blocks of this size; the edges of one are where an
# off-by-one in the upload or the read path shows.
BLOCK = 4 * MIB
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


def failure_detail(error: BaseException) -> str:
    """The error, and for anything but a failed check the script line that hit it.

    A bare `OSError: [Errno 5]` from a case with forty filesystem calls says
    nothing about which one the mount refused.
    """
    detail = f"{type(error).__name__}: {error}"
    if isinstance(error, AssertionError):
        return detail
    frames = [
        frame
        for frame in traceback.extract_tb(error.__traceback__)
        if frame.filename == __file__ and frame.name not in {"check", "run"}
    ]
    if frames:
        frame = frames[-1]
        detail += f" (at {frame.name}:{frame.lineno}: {frame.line})"
    return detail


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


def pattern(size: int, seed: str) -> bytes:
    """Deterministic bytes that differ at every offset, so a shift cannot hide."""
    return random.Random(seed).randbytes(size)


def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        check(written > 0, "write made no progress")
        view = view[written:]


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
    """Everything a case may touch: its sandbox, its recorder, and the daemon.

    `location` is the folder the sandbox was made in. Only when the run created
    that folder itself does `owns_location` allow a case to aim a destructive
    command at it, on the chance that the command is not refused.
    """

    def __init__(
        self,
        root: Path,
        obs: Observations,
        kind: str,
        daemon=None,
        location: Path | None = None,
        owns_location: bool = False,
    ) -> None:
        self.root = root
        self.obs = obs
        self.kind = kind
        self.daemon = daemon
        self.location = location
        self.owns_location = owns_location
        self.infos: list[str] = []

    @property
    def is_live(self) -> bool:
        return self.kind == LIVE

    def info(self, line: str) -> None:
        """Something worth showing under the case's result, like a measured rate."""
        self.infos.append(line)

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


class KnownIssue(Exception):
    """An open bug from docs/BUGS.md reproduced. Reported, but not a failure.

    A case for an open bug raises this when the bug shows and passes when it
    does not, so the run says when the entry can be closed.
    """


class TestTimeout(Exception):
    pass


class Interrupted(KeyboardInterrupt):
    """SIGTERM or SIGHUP, raised like Ctrl-C so that every `finally` runs."""


def _raise_interrupted(signum, _frame):
    raise Interrupted(f"received {signal.Signals(signum).name}")


def install_interrupt_handlers() -> None:
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, _raise_interrupted)


@contextlib.contextmanager
def shielded():
    """Hold interrupts off while cleanup runs.

    A Ctrl-C that lands in the middle of cleanup would strand exactly the
    remote state cleanup exists to remove. Two are acknowledged and ignored;
    a third abandons cleanup, and the next run reaps whatever it left.
    """
    received = 0

    def on_signal(signum, _frame):
        nonlocal received
        received += 1
        if received >= 3:
            raise Interrupted("cleanup abandoned; the next run removes what is left")
        print(
            f"[cleanup] {signal.Signals(signum).name} ignored while cleaning up; "
            f"send it {3 - received} more time(s) to abandon cleanup",
            file=sys.stderr,
        )

    watched = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    previous = {signum: signal.signal(signum, on_signal) for signum in watched}
    try:
        yield
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)


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


def answers(root: Path, seconds: float = 15) -> bool:
    """Whether a stat and a listing of `root` come back, and in time.

    The probe runs on a thread the run can leave behind, since a wedged FUSE
    call cannot be interrupted.
    """
    outcome: list[bool] = []

    def probe() -> None:
        try:
            os.stat(root)
            os.listdir(root)
            outcome.append(True)
        except OSError:
            outcome.append(False)

    thread = threading.Thread(target=probe, daemon=True)
    thread.start()
    thread.join(seconds)
    return outcome == [True]


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
    ctx.info(f"sha256 {digest}")


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


def _sqlite_create(connection: sqlite3.Connection) -> None:
    connection.execute("CREATE TABLE rows (id INTEGER PRIMARY KEY, value TEXT)")


def _sqlite_insert(connection: sqlite3.Connection) -> None:
    with connection:
        connection.executemany(
            "INSERT INTO rows (value) VALUES (?)", [(f"row-{index}",) for index in range(500)]
        )


def _sqlite_diagnosis(database: Path) -> str:
    """What a malformed database on the mount looks like next to a local replay.

    The same statements from the same library write the same bytes, so the
    pages that differ, and whether a stray journal sat next to the file, say
    whether the mount lost a write, served a stale read or let a rollback in.
    """
    notes = []
    try:
        entries = sorted(
            f"{entry.name}={entry.stat(follow_symlinks=False).st_size}"
            for entry in os.scandir(database.parent)
            if entry.name.startswith(database.name)
        )
        notes.append(f"files [{', '.join(entries)}]")
    except OSError as error:
        notes.append(f"listing failed: {error}")
    try:
        actual = database.read_bytes()
    except OSError as error:
        return "; ".join(notes + [f"reading the database failed: {error}"])
    with tempfile.TemporaryDirectory(prefix="pdfs-sqlite-reference-") as scratch:
        reference_path = Path(scratch) / database.name
        reference = sqlite3.connect(reference_path)
        try:
            _sqlite_create(reference)
            _sqlite_insert(reference)
        finally:
            reference.close()
        expected = reference_path.read_bytes()
    page = int.from_bytes(expected[16:18], "big") or 65536
    notes.append(f"size {len(actual)} vs reference {len(expected)}")
    differing = []
    for offset in range(0, max(len(actual), len(expected)), page):
        ours, theirs = actual[offset : offset + page], expected[offset : offset + page]
        if ours != theirs:
            kind = "zeros" if ours.strip(b"\0") == b"" else "differs"
            differing.append(f"{offset // page + 1}:{kind}")
    notes.append(f"pages differing [{', '.join(differing[:16]) or 'none'}]")
    return "; ".join(notes)


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
    step = "open"
    try:
        step = "create table"
        _sqlite_create(connection)
        step = "insert"
        _sqlite_insert(connection)
        step = "checkpoint"
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        step = "count"
        count = connection.execute("SELECT COUNT(*) FROM rows").fetchone()[0]
    except sqlite3.DatabaseError as error:
        raise AssertionError(
            f"sqlite failed at {step}: {error}; {_sqlite_diagnosis(database)}"
        ) from error
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


def test_block_boundaries_and_overwrites(ctx: Context) -> None:
    """Sizes on either side of every boundary the stack has.

    The kernel moves 4 KiB pages, FUSE 128 KiB requests, and Drive 4 MiB
    blocks. An off-by-one at any of them truncates, pads or shifts a file by one
    byte, and only a file of exactly that size shows it.
    """
    root = ctx.root / "boundaries"
    root.mkdir()
    sizes = [0, 1, 4095, 4096, 4097, 131071, 131072, 131073, BLOCK - 1, BLOCK, BLOCK + 1, 2 * BLOCK + 3]
    for size in sizes:
        write_durable(root / f"size-{size}", pattern(size, f"size-{size}"))
    for size in sizes:
        path = root / f"size-{size}"
        reported = os.lstat(path).st_size
        check(reported == size, f"a {size}-byte file reports {reported} bytes")
        check_bytes(read(path), pattern(size, f"size-{size}"), f"{size}-byte file")
    ctx.record("sizes", [os.lstat(root / f"size-{size}").st_size for size in sizes])

    # Overwrites that change how many blocks a file has, in both directions.
    path = root / "reshaped"
    write_durable(path, pattern(2 * BLOCK + 3, "reshaped"))
    model = bytearray(pattern(BLOCK + 7, "shorter"))
    write_durable(path, bytes(model))
    check_bytes(read(path), bytes(model), "an overwrite with fewer blocks")
    fd = os.open(path, os.O_RDWR)
    try:
        straddle = pattern(8192, "straddle")
        os.pwrite(fd, straddle, BLOCK - 4096)
        model[BLOCK - 4096 : BLOCK + 4096] = straddle
        tail = pattern(BLOCK, "tail")
        os.pwrite(fd, tail, len(model))
        model.extend(tail)
        # Back to exactly one block, then past the end, leaving a hole.
        os.ftruncate(fd, BLOCK)
        del model[BLOCK:]
        os.pwrite(fd, b"after-hole", BLOCK + 10)
        model.extend(bytes(10) + b"after-hole")
        os.fsync(fd)
    finally:
        os.close(fd)
    check(os.lstat(path).st_size == len(model), "block-crossing edits left the wrong size")
    check_bytes(read(path), bytes(model), "block-crossing edits after reopening")
    ctx.record("reshaped.digest", hashlib.sha256(model).hexdigest())

    # Until the queue drains, reads come from the staged copy. What reached
    # Drive is only visible after it has, so read everything again from there.
    if ctx.is_live and ctx.daemon is not None:
        ctx.daemon.wait_for_queue()
        for size in sizes:
            path = root / f"size-{size}"
            check(os.lstat(path).st_size == size, f"a {size}-byte file changed size on upload")
            check_bytes(read(path), pattern(size, f"size-{size}"), f"uploaded {size}-byte file")
        check_bytes(read(root / "reshaped"), bytes(model), "uploaded block-crossing edits")


UNUSUAL_NAMES = [
    "a" * 255,
    "é" * 127,
    " leading space",
    "trailing space ",
    "trailing dot.",
    "...",
    "-starts-with-dash",
    "#hash & ampersand",
    "percent%20encoded",
    "semi;colon,comma",
    "quote's \"double\"",
    "back\\slash",
    "colon:star*question?",
    "<angle>|pipe",
    "tab\there",
    "new\nline",
    "emoji 📁🗂️",
    "zero​width",
    "rtl‮override",
    "cjk 文件夹",
    "latin ñ ü ß",
    "nfc-é",
    "nfd-é",
]


def test_unusual_names(ctx: Context) -> None:
    """Every byte sequence Linux allows in a name must survive a round trip.

    Drive's own clients refuse some of these (Windows forbids `:*?"<>|`, macOS
    normalizes Unicode), but a Linux program may create any of them, and one
    that is silently renamed or dropped loses the file for that program.
    """
    root = ctx.root / "names"
    root.mkdir()
    for name in UNUSUAL_NAMES:
        write_durable(root / name, name.encode())
    (root / " spaced dir ").mkdir()
    write_durable(root / " spaced dir " / "child", b"child")
    listed = set(os.listdir(root))
    missing = [name for name in UNUSUAL_NAMES if name not in listed]
    check(not missing, f"names missing from readdir: {missing!r}")
    for name in UNUSUAL_NAMES:
        check_bytes(read(root / name), name.encode(), f"contents of {name!r}")
    check_bytes(read(root / " spaced dir " / "child"), b"child", "child of a spaced directory")
    for index, name in enumerate(UNUSUAL_NAMES):
        os.rename(root / name, root / f"renamed-{index}")
        os.rename(root / f"renamed-{index}", root / name)
    check(set(os.listdir(root)) == listed, "renaming names away and back changed the listing")

    write_durable(root / "Case-Only.txt", b"case")
    os.rename(root / "Case-Only.txt", root / "case-only.txt")
    names = set(os.listdir(root))
    check("case-only.txt" in names and "Case-Only.txt" not in names, "a case-only rename did not stick")
    check_bytes(read(root / "case-only.txt"), b"case", "a case-only rename")
    ctx.record("names", sorted(os.listdir(root)))


def test_deep_tree(ctx: Context) -> None:
    root = ctx.root / "deep-tree"
    deep = root / "deep"
    leaf = deep.joinpath(*(f"level-{level:02d}" for level in range(24)))
    leaf.mkdir(parents=True)
    write_durable(leaf / "leaf.txt", b"deep leaf")
    os.rename(deep, root / "deep-renamed")
    moved = root / "deep-renamed" / leaf.relative_to(deep)
    check_bytes(read(moved / "leaf.txt"), b"deep leaf", "a leaf 24 levels down after renaming the top")

    shutil.rmtree(root)
    check(not root.exists(), "rm -rf left the tree behind")
    check("deep-tree" not in os.listdir(ctx.root), "rm -rf left the tree in readdir")


def test_wide_directory(ctx: Context) -> None:
    """More entries than one readdir reply holds, created and removed in parallel.

    The kernel serializes creates and unlinks within one directory, so the
    threads only overlap the parts outside that lock. The per-create time is
    noted: online, each create is a round trip to the server.
    """
    root = ctx.root / "wide"
    root.mkdir()
    names = [f"entry-{index:04d}.txt" for index in range(128)]
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(lambda name: write_durable(root / name, name.encode()), names))
    ctx.note("wide.create_ms", round((time.monotonic() - started) * 1000 / len(names)))
    listed = sorted(os.listdir(root))
    check(listed == names, f"a {len(names)}-entry directory lists {len(listed)} entries")
    with os.scandir(root) as entries:
        sizes = {entry.name: entry.stat().st_size for entry in entries}
    wrong = [name for name in names if sizes.get(name) != len(name)]
    check(not wrong, f"wrong sizes in a wide directory: {wrong[:10]}")
    for name in names[::17]:
        check_bytes(read(root / name), name.encode(), f"wide entry {name}")
    half = len(names) // 2
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(lambda name: os.unlink(root / name), names[:half]))
    check(sorted(os.listdir(root)) == names[half:], "concurrent unlinks left the wrong entries")
    ctx.record("wide.count", len(listed))

    shutil.rmtree(root)
    check(not root.exists(), "rm -rf left the directory behind")
    check("wide" not in os.listdir(ctx.root), "rm -rf left the directory in readdir")


def test_rename_patterns(ctx: Context) -> None:
    """The rename sequences programs actually use, not just single renames."""
    root = ctx.root / "renames"
    root.mkdir()
    first, second, spare = root / "a.txt", root / "b.txt", root / "swap.tmp"
    write_durable(first, b"alpha")
    write_durable(second, b"beta")
    os.rename(first, spare)
    os.rename(second, first)
    os.rename(spare, second)
    check(read(first) == b"beta" and read(second) == b"alpha", "a three-way swap mixed up contents")

    write_durable(root / "chain-0", b"chain")
    for step in range(1, 6):
        os.rename(root / f"chain-{step - 1}", root / f"chain-{step}")
    chain = sorted(name for name in os.listdir(root) if name.startswith("chain"))
    check(chain == ["chain-5"], f"a rename chain left {chain}")
    check_bytes(read(root / "chain-5"), b"chain", "the end of a rename chain")

    # Log rotation: rename away and recreate the name straight after.
    log = root / "app.log"
    write_durable(log, b"first generation\n")
    os.rename(log, root / "app.log.1")
    write_durable(log, b"second generation\n")
    check_bytes(read(root / "app.log.1"), b"first generation\n", "the rotated log")
    check_bytes(read(log), b"second generation\n", "the recreated log")

    # Replacing a file must not change what an already open reader sees.
    target = root / "replaced.txt"
    write_durable(target, b"old contents")
    reader = os.open(target, os.O_RDONLY)
    try:
        write_durable(root / "replacement.tmp", b"new contents!")
        os.replace(root / "replacement.tmp", target)
        check_bytes(os.pread(reader, 64, 0), b"old contents", "an open reader after its file was replaced")
    finally:
        os.close(reader)
    check_bytes(read(target), b"new contents!", "the replacement")

    (root / "a-dir").mkdir()
    expect_errno({errno.EISDIR}, lambda: os.rename(first, root / "a-dir"), "rename(file over directory)")
    expect_errno({errno.ENOTDIR}, lambda: os.rename(root / "a-dir", first), "rename(directory over file)")
    expect_errno({errno.ENOENT}, lambda: os.rename(first, root / "missing" / "a.txt"), "rename(into a missing directory)")

    (root / "left").mkdir()
    (root / "right").mkdir()
    write_durable(root / "left" / "payload", b"travels")
    os.rename(root / "left", root / "right" / "left")
    os.rename(root / "right" / "left", root / "left")
    check_bytes(read(root / "left" / "payload"), b"travels", "a directory moved out and back")
    ctx.record("listing", sorted(os.listdir(root)))


def test_handle_coherency(ctx: Context) -> None:
    """What one handle writes, every other handle and every later open sees."""
    root = ctx.root
    path = root / "coherent"
    write_durable(path, b"")
    writer = os.open(path, os.O_WRONLY)
    reader = os.open(path, os.O_RDONLY)
    try:
        write_all(writer, b"visible without fsync")
        check_bytes(os.pread(reader, 64, 0), b"visible without fsync", "a second handle before fsync")
    finally:
        os.close(writer)
        os.close(reader)

    log = root / "appenders"
    write_durable(log, b"")
    handles = [os.open(log, os.O_WRONLY | os.O_APPEND) for _ in range(2)]
    try:
        for index in range(50):
            write_all(handles[index % 2], f"record {index:02d}\n".encode())
    finally:
        for handle in handles:
            os.close(handle)
    lines = read(log).decode().splitlines()
    check(lines == [f"record {index:02d}" for index in range(50)], "two O_APPEND handles overwrote each other")
    ctx.record("appenders.lines", len(lines))

    # Rewrite and reread in a tight loop: a stale page cache or attribute
    # cache serves the previous generation, or the previous generation's size.
    churn = root / "churn"
    for generation in range(30):
        payload = pattern(1000 + generation * 37, f"generation-{generation}")
        write_durable(churn, payload)
        check(os.lstat(churn).st_size == len(payload), f"generation {generation}: stale size")
        check_bytes(read(churn), payload, f"generation {generation} after close and reopen")

    shrink = root / "shrink-under-reader"
    write_durable(shrink, b"x" * 8192)
    reader = os.open(shrink, os.O_RDONLY)
    try:
        os.truncate(shrink, 100)
        check(os.fstat(reader).st_size == 100, "an open handle missed a truncate by path")
        check_bytes(os.pread(reader, 8192, 0), b"x" * 100, "an open handle read past a truncate")
    finally:
        os.close(reader)


def test_throughput(ctx: Context) -> None:
    """Measured, printed, and held to floors a working mount clears easily.

    The floors catch a pathological slowdown (a server round trip per write, a
    re-download per read), not a slow link. Writes land in local staging and
    reads of just-written data come from the cache, so neither should depend
    on the network. PDFS_ACCEPTANCE_MIN_MIBPS and PDFS_ACCEPTANCE_MIN_OPS move
    the floors; 0 turns one off.

    Metadata has a lower default floor on a FUSE target: 3 operations a
    second. A local-first mount records each create and unlink locally and
    clears it easily, but with `local_first` off each one waits for Drive
    before it returns (about 350 ms for a create), and 20 a second is out of
    reach. The floor still catches a second round trip per operation.
    """
    min_rate = float(os.environ.get("PDFS_ACCEPTANCE_MIN_MIBPS", "10"))
    default_ops = "3" if is_fuse(ctx.root) else "20"
    min_ops = float(os.environ.get("PDFS_ACCEPTANCE_MIN_OPS", default_ops))
    root = ctx.root / "throughput"
    root.mkdir()
    chunk = pattern(MIB, "throughput")
    total = 64

    path = root / "sequential.bin"
    started = time.monotonic()
    fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
    try:
        for _ in range(total):
            write_all(fd, chunk)
        os.fsync(fd)
    finally:
        os.close(fd)
    write_rate = total / max(time.monotonic() - started, 1e-6)

    started = time.monotonic()
    digest = hashlib.sha256()
    with path.open("rb", buffering=0) as file:
        while block := file.read(MIB):
            digest.update(block)
    read_rate = total / max(time.monotonic() - started, 1e-6)
    check(digest.hexdigest() == hashlib.sha256(chunk * total).hexdigest(), "the 64 MiB file read back wrong")

    # Many small writes: one upload per write, or a lock held across each,
    # shows up here long before it shows up in a large copy.
    small = root / "small-writes.bin"
    started = time.monotonic()
    fd = os.open(small, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
    try:
        for offset in range(0, MIB, 4096):
            write_all(fd, chunk[offset : offset + 4096])
    finally:
        os.close(fd)
    small_rate = 1 / max(time.monotonic() - started, 1e-6)
    check_bytes(read(small), chunk, "1 MiB written 4 KiB at a time")

    started = time.monotonic()
    names = [root / f"meta-{index:03d}" for index in range(100)]
    for name in names:
        write_durable(name, b"m")
    for name in names:
        os.lstat(name)
    os.listdir(root)
    for name in names:
        os.unlink(name)
    ops_rate = (3 * len(names) + 1) / max(time.monotonic() - started, 1e-6)

    ctx.info(
        f"write {write_rate:.0f} MiB/s, read {read_rate:.0f} MiB/s, "
        f"4 KiB writes {small_rate:.1f} MiB/s, metadata {ops_rate:.0f} ops/s"
    )
    ctx.note("write_mibps", round(write_rate, 1))
    ctx.note("read_mibps", round(read_rate, 1))
    ctx.note("small_write_mibps", round(small_rate, 2))
    ctx.note("metadata_ops", round(ops_rate, 1))
    for label, rate, floor in (
        ("sequential write", write_rate, min_rate),
        ("sequential read", read_rate, min_rate),
        ("4 KiB writes", small_rate, min_rate / 10),
    ):
        check(not floor or rate >= floor, f"{label} ran at {rate:.1f} MiB/s, under the {floor:g} MiB/s floor")
    check(not min_ops or ops_rate >= min_ops, f"metadata ran at {ops_rate:.1f} ops/s, under the {min_ops:g} floor")
    ctx.record("within_floors", True)


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


def test_regression_b101_write_during_create_upload(ctx: Context) -> None:
    """A write that closes while its file's create uploads must reach Drive (B101).

    The create of a new file drains on its own, often before the first write
    closes. The write attached its bytes to the create's queue row while the
    upload was on the wire, and the landed create deleted that row, so Drive
    kept an empty file. A handle that stayed open across the landing still
    carried the placeholder uid and failed its close.
    """
    daemon = ctx.require_daemon()
    root = ctx.root / "b101"
    root.mkdir()
    payloads = {}
    for index, pause in enumerate((0.0, 0.5, 2.0, 5.0)):
        path = root / f"file-{index}"
        payloads[path] = pattern(BLOCK + 4096 * index + 1, f"b101-{index}")
        fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
        try:
            # Held open with nothing written, so the empty create can drain
            # (and land) before the bytes arrive.
            time.sleep(pause)
            write_all(fd, payloads[path])
            os.fsync(fd)
        finally:
            os.close(fd)
    daemon.wait_for_queue()
    time.sleep(3)
    daemon.wait_for_queue()
    for path, payload in payloads.items():
        check(os.lstat(path).st_size == len(payload), f"{path.name} has the wrong size after upload")
        check_bytes(read(path), payload, f"{path.name} after upload")
    check(not _conflict_copies(root), "writes during a create produced conflict copies")


def test_regression_b113_move_before_upload(ctx: Context) -> None:
    """A new file renamed or moved before its upload drains is no conflict copy (B113).

    A create mints an empty file on Drive at once and queues the bytes. The
    handle carried no revision id, so the drain fell back to comparing mtimes,
    and Drive advances a file's mtime when it is renamed or moved. Downloads
    moved on before they drained came back as `(sync-conflict …)` copies next
    to an empty original. Pausing sync holds the bytes back while the files
    move, which is the order the bug needs. The names must not be transient
    ones like `*.part`: those mint nothing and queue their create instead (B70).

    Since changes through the mount are queued (local-first), the create is
    queued as well and the moves rewrite it before anything reaches Drive. The
    files must land where they were moved, without conflict copies, either way.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder uploads in its own passes, not through a minted create")
    if (daemon.status().get("mount") or {}).get("paused"):
        raise Skip("sync is paused already; this case has to pause and resume it itself")
    root = ctx.root / "b113"
    moved = root / "moved here"
    moved.mkdir(parents=True)
    daemon.wait_for_queue()
    payloads = {
        name: pattern(4096 * index + 13, f"b113-{name}")
        for index, name in enumerate(("renamed", "moved", "both"), 1)
    }
    # Timed, so a run that dies here leaves sync paused for minutes, not for good.
    daemon.command("sync", "pause", "--for", "10m")
    try:
        for name, payload in payloads.items():
            write_durable(root / f"{name}.bin", payload)
        # Drive keeps mtimes in whole seconds: a move in the same second as the
        # create leaves the mtime as it was and hides the bug.
        time.sleep(2)
        os.rename(root / "renamed.bin", root / "renamed later.bin")
        os.rename(root / "moved.bin", moved / "moved.bin")
        os.rename(root / "both.bin", moved / "both moved.bin")
        held = [item for item in daemon.queue() if not preexisting(item)]
        check(bool(held), "nothing was queued while sync was paused, so the moves raced nothing")
        ctx.note("held_ops", len(held))
        ctx.note("held_creates", sum(item["kind"] == "create" for item in held))
    finally:
        daemon.command("sync", "resume")
    daemon.wait_for_queue()
    # The queue reports empty as soon as the last op lands; give the revision
    # behind it time to show up, as B74 does.
    time.sleep(3)
    daemon.wait_for_queue()

    expected = {
        root / "renamed later.bin": payloads["renamed"],
        moved / "moved.bin": payloads["moved"],
        moved / "both moved.bin": payloads["both"],
    }
    for path, payload in expected.items():
        size = os.lstat(path).st_size
        check(size == len(payload), f"{path.name} is {size} bytes after the drain, expected {len(payload)}")
        check_bytes(read(path), payload, f"{path.name} after the drain")
    conflicts = _conflict_copies(root) + _conflict_copies(moved)
    check(not conflicts, f"moving files before their upload made conflict copies: {conflicts}")
    check(
        sorted(os.listdir(root)) == ["moved here", "renamed later.bin"],
        f"old names are still listed: {sorted(os.listdir(root))}",
    )


def test_regression_b102_removed_folder_takes_its_uploads(ctx: Context) -> None:
    """Removing a folder drops the uploads queued for the files inside it (B102).

    Only creates and mkdirs carry a parent in the queue, so removing a folder
    kept the revisions of files that were already on Drive. They held their
    staged bytes and retried against the trash for good, and `pdfs rm` dropped
    nothing at all. Sync is paused so the revisions are still queued when the
    folders go, one with `pdfs rm` and one through the mount.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder uploads in its own passes, not through the queue")
    if (daemon.status().get("mount") or {}).get("paused"):
        raise Skip("sync is paused already; this case has to pause and resume it itself")
    files = {}
    for how in ("by pdfs rm", "through the mount"):
        inner = ctx.root / f"b102 {how}" / "inner"
        inner.mkdir(parents=True)
        files[how] = inner / "queued.bin"
        write_durable(files[how], b"b102 first version\n")
    daemon.wait_for_queue()
    daemon.command("sync", "pause", "--for", "10m")
    try:
        for how, path in files.items():
            write_durable(path, pattern(BLOCK + 102, f"b102-{how}"))
        held = {
            item["id"]
            for item in daemon.queued(
                lambda item: item["kind"] == "revision" and not preexisting(item), len(files)
            )
        }
        check(
            len(held) >= len(files),
            f"the rewrites queued {plural(len(held), 'revision')}, not {len(files)}, so B102 was not replayed",
        )
        daemon.command("rm", str(files["by pdfs rm"].parent.parent))
        shutil.rmtree(files["through the mount"].parent.parent)
        left = [f"{item['kind']} {item['path']}" for item in daemon.queue() if item["id"] in held]
        check(not left, f"uploads queued in a removed folder are still queued: {', '.join(left)}")
    finally:
        daemon.command("sync", "resume")
    daemon.wait_for_queue()
    listed = os.listdir(ctx.root)
    back = [path.parent.parent.name for path in files.values() if path.parent.parent.name in listed]
    check(not back, f"a removed folder is listed again: {back}")


def test_regression_b94_deleted_transient_file_leaves_nothing_queued(ctx: Context) -> None:
    """A `*.part` file deleted before its rename leaves nothing queued (B94).

    A transient name parks its create until a rename gives the file its final
    name (B70). That rename was the park's only exit, so a temp file that was
    deleted instead stayed queued for good, with its bytes. The sweep that lets
    an hour-old park through is not driven here.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder parks nothing; it only puts off a transient name")
    path = ctx.root / "b94 abandoned.part"
    write_durable(path, pattern(4096 + 94, "b94"))
    parked = {item["id"] for item in daemon.queue() if item["parked"] and item["path"].endswith(path.name)}
    check(bool(parked), "the .part file's create was not parked, so B94 was not replayed")
    os.unlink(path)
    deadline = time.monotonic() + 30
    while left := [f"{item['kind']} {item['path']}" for item in daemon.queue() if item["id"] in parked]:
        check(time.monotonic() < deadline, f"the deleted temp file is still queued: {', '.join(left)}")
        time.sleep(1)
    check(path.name not in os.listdir(ctx.root), "the deleted temp file is listed again")


def test_regression_b95_tmp_name_uploads(ctx: Context) -> None:
    """A file whose final name ends in `.tmp` or `.temp` uploads at once (B95).

    Both suffixes counted as transient, so such a file waited for a rename that
    never came. A Takeout import left dozens of `index.tmp` files parked.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder uploads in its own passes, so the queue cannot show a park")
    folder = ctx.root / "b95"
    folder.mkdir()
    payloads = {
        name: pattern(4096 + index, f"b95-{name}") for index, name in enumerate(("index.tmp", "preview.temp"))
    }
    for name, payload in payloads.items():
        write_durable(folder / name, payload)
    # Fails at once on a parked op, naming it.
    daemon.wait_for_queue()
    listed = {entry["name"] for entry in daemon.listing(folder)}
    check(set(payloads) <= listed, f"pdfs ls lists {sorted(listed)}")
    for name, payload in payloads.items():
        check_bytes(read(folder / name), payload, f"{name} after the upload")


def test_regression_b111_rename_back_at_once(ctx: Context) -> None:
    """A file renamed and straight back keeps working, every time (B111).

    Right after a rename, the SDK still sent the old name hash, and Drive
    refused the next rename as out of date. The daemon answered with `EIO`,
    about once a run. On a mount, `pdfs rename` is driven too.
    """
    folder = ctx.root / "b111"
    folder.mkdir()
    original = folder / "b111 original.txt"
    other = folder / "b111 other.txt"
    payload = b"b111 rename back\n" * 64
    write_durable(original, payload)
    live = ctx.is_live and ctx.daemon is not None and is_fuse(ctx.root)
    if live:
        # The renames should race nothing but Drive.
        ctx.daemon.wait_for_queue()
    for _ in range(8):
        os.rename(original, other)
        os.rename(other, original)
    names = sorted(os.listdir(folder))
    check(names == [original.name], f"after renaming there and back: {names}")
    check_bytes(read(original), payload, "the file after renaming it there and back")
    ctx.record("names", names)
    if not live:
        return
    for _ in range(3):
        ctx.daemon.command("rename", str(original), other.name)
        ctx.daemon.command("rename", str(other), original.name)
    listed = sorted(entry["name"] for entry in ctx.daemon.listing(folder))
    check(listed == [original.name], f"pdfs ls lists {listed} after pdfs rename there and back")
    check_bytes(read(original), payload, "the file after pdfs rename there and back")


def test_regression_b121_move_right_after_rename(ctx: Context) -> None:
    """A file renamed and moved at once lands where it was sent (B121).

    The SDK kept a node's old name hash until the next event poll, up to ten
    seconds after the daemon's own rename. A move in that window was refused
    as out of date: `EIO` through the mount, an error from `pdfs move`. A
    rename across folders landed only after seconds of retries. On a mount,
    `pdfs rename` and `pdfs move` are driven too.
    """
    folder = ctx.root / "b121"
    inner = folder / "inner"
    inner.mkdir(parents=True)
    path = folder / "b121 first.txt"
    payload = b"b121 move right after rename\n" * 64
    write_durable(path, payload)
    live = ctx.is_live and ctx.daemon is not None and is_fuse(ctx.root)
    if live:
        # The changes should race nothing but Drive.
        ctx.daemon.wait_for_queue()
    started = time.monotonic()
    renamed = folder / "b121 renamed.txt"
    os.rename(path, renamed)
    os.rename(renamed, inner / renamed.name)
    # A rename across folders is a rename and then a move of the same node.
    back = folder / "b121 back.txt"
    os.rename(inner / renamed.name, back)
    ctx.info(f"renamed, moved and renamed back through the mount in {time.monotonic() - started:.1f}s")
    names = sorted(os.listdir(folder))
    check(names == sorted([back.name, inner.name]), f"after the rename and moves: {names}")
    check(not os.listdir(inner), f"inner still lists {sorted(os.listdir(inner))}")
    check_bytes(read(back), payload, "the file after the rename and moves")
    ctx.record("names", names)
    if not live:
        return
    started = time.monotonic()
    ctx.daemon.command("rename", str(back), path.name)
    ctx.daemon.command("move", str(path), str(inner))
    ctx.info(f"pdfs rename and pdfs move took {time.monotonic() - started:.1f}s")
    listed = sorted(entry["name"] for entry in ctx.daemon.listing(inner))
    check(listed == [path.name], f"pdfs ls lists {listed} in inner after pdfs rename and move")
    check_bytes(read(inner / path.name), payload, "the file after pdfs rename and move")


def _uploads_named(daemon: Daemon, name: str) -> list[dict]:
    transfers = json.loads(daemon.command("transfers", json_output=True))
    return [
        item for item in transfers.get("items", []) if item["direction"] == "Upload" and item["name"] == name
    ]


def _describe_uploads(items: list[dict]) -> str:
    return ", ".join(f"{item['bytes_completed']} of {item['bytes_total']} bytes" for item in items)


def test_regression_b98_superseded_upload_ends(ctx: Context) -> None:
    """An upload that a new write supersedes ends, and the new bytes land (B98).

    A write while a revision is on the wire cancels that upload. The cancel was
    reported as a read to retry, and the SDK retried it forever: the drain
    thread spun at full CPU and the transfer stayed listed at 0 bytes. A
    browser download, which keeps growing, triggers it.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder uploads in its own passes, not through the drain")
    path = ctx.root / "b98 growing.bin"
    first = pattern(64 * MIB, "b98-first")
    write_durable(path, first)
    deadline = time.monotonic() + 60
    while not _uploads_named(daemon, path.name):
        if time.monotonic() >= deadline:
            raise Skip("the upload was never seen on the wire, so nothing could supersede it")
        time.sleep(0.2)
    tail = pattern(MIB + 3, "b98-tail")
    with open(path, "ab") as handle:
        handle.write(tail)
        handle.flush()
        os.fsync(handle.fileno())
    try:
        daemon.wait_for_queue()
    except TimeoutError as error:
        raise AssertionError(f"{error}; still uploading: {_describe_uploads(_uploads_named(daemon, path.name))}") from error
    deadline = time.monotonic() + 15
    while stuck := _uploads_named(daemon, path.name):
        check(
            time.monotonic() < deadline,
            f"the superseded upload is still listed after the queue drained: {_describe_uploads(stuck)}",
        )
        time.sleep(1)
    expected = first + tail
    size = os.lstat(path).st_size
    check(size == len(expected), f"{path.name} is {size} bytes after the drain, expected {len(expected)}")
    check_bytes(read(path), expected, f"{path.name} after the drain")
    check(not _conflict_copies(ctx.root), "superseding an upload made conflict copies")


def test_regression_b80_new_file_is_searchable(ctx: Context) -> None:
    """A new file is found by `pdfs search`, also after it is rewritten (B80).

    A node was indexed only when its parents led up to the My files root. A
    device folder's root is no node row, so everything in a synced folder fell
    out of the index the first time it was rewritten. The name carries a unique
    token, so the search cannot find anything else.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("the case checks what a mount indexes as it writes; a mirror folder writes in its own passes")
    token = f"b80x{uuid.uuid4().hex[:12]}"
    folder = ctx.root / "b80"
    folder.mkdir()
    path = folder / f"findable {token}.txt"
    write_durable(path, b"b80 first version\n")
    daemon.wait_for_queue()
    write_durable(path, b"b80 rewritten\n")
    daemon.wait_for_queue()
    deadline = time.monotonic() + 30
    while path.name not in (found := daemon.command("search", token)):
        check(time.monotonic() < deadline, f"pdfs search {token} does not find the file: {found.strip()}")
        time.sleep(2)


def test_regression_b48_emptied_folder_removes_at_once(ctx: Context) -> None:
    """A folder whose children were all just unlinked can be removed at once (B48).

    A listing right after the unlinks could bring the trashed children back from
    Drive, and a child moved into a folder the kernel had listed was missing
    from that cached listing. `rmdir` then failed with `ENOTEMPTY` for as long
    as anyone retried. On a mount, `pdfs refresh` asks Drive for the listing.
    """
    folder = ctx.root / "b48"
    elsewhere = ctx.root / "b48 elsewhere"
    folder.mkdir()
    elsewhere.mkdir()
    for index in range(12):
        write_durable(folder / f"child-{index:02}.txt", f"b48 child {index}\n".encode())
    live = ctx.is_live and ctx.daemon is not None and is_fuse(ctx.root)
    if live:
        ctx.daemon.wait_for_queue()
    # Listed, so the kernel caches the folder before the move into it.
    os.listdir(folder)
    write_durable(elsewhere / "moved in.txt", b"b48 moved in\n")
    os.rename(elsewhere / "moved in.txt", folder / "moved in.txt")
    names = sorted(os.listdir(folder))
    ctx.record("names", names)
    for name in names:
        os.unlink(folder / name)
    if live:
        ctx.daemon.command("refresh", str(folder))
    left = sorted(os.listdir(folder))
    check(not left, f"unlinked children are listed again: {left}")
    os.rmdir(folder)
    check(folder.name not in os.listdir(ctx.root), "the removed folder is still listed")


def _refused(daemon: Daemon, *args: str) -> str:
    """Run a pdfs command that has to fail, and return what it said."""
    try:
        output = daemon.command(*args)
    except RuntimeError as error:
        return str(error)
    raise AssertionError(f"pdfs {' '.join(args)} should have been refused: {output.strip()}")


def test_regression_b88_cli_changes_by_local_path(ctx: Context) -> None:
    """`pdfs mkdir`, `rename` and `rm` take a local path in every location (B88).

    They resolved paths against My files only, so in an on-demand synced folder
    they answered "is not under the mountpoint". Routing them to the folder's
    own mount made its root a path they could name, and an on-demand folder's
    root has a parent on Drive: `pdfs rm ~/Downloads` would have trashed the
    whole folder. So a mount root has to be refused. `rm` is aimed at one only
    when this run created the folder itself. A mirror folder has no mount to
    change, so there all three have to refuse and leave the local files alone.
    """
    daemon = ctx.require_daemon()
    root = ctx.root
    payload = b"b88-by-path\n" * 512
    if not is_fuse(root):
        mirrors = [
            Path(folder["local_path"]).resolve()
            for folder in daemon.sync_folders()
            if folder.get("mode") == "mirror"
        ]
        if not any(root.resolve().is_relative_to(mirror) for mirror in mirrors):
            raise Skip(f"{root.parent} is neither a mount nor a mirror folder of this daemon")
        local = root / "b88 local.txt"
        write_durable(local, payload)
        for args in (
            ("mkdir", str(root), "b88 folder"),
            ("rename", str(local), "b88 renamed.txt"),
            ("rm", str(local)),
        ):
            message = _refused(daemon, *args)
            check("mirrored folder" in message, f"pdfs {args[0]} in a mirror folder: {message}")
        check(not (root / "b88 folder").exists(), "pdfs mkdir changed a mirror folder")
        check_bytes(read(local), payload, "a refused command changed the local file")
        return

    folder = root / "b88 folder"
    daemon.command("mkdir", str(root), folder.name)
    check(folder.is_dir(), "pdfs mkdir by local path made no folder")
    write_durable(folder / "note.txt", payload)
    daemon.wait_for_queue()

    daemon.command("rename", str(folder / "note.txt"), "renamed.txt")
    check(os.listdir(folder) == ["renamed.txt"], f"after pdfs rename: {os.listdir(folder)}")
    check_bytes(read(folder / "renamed.txt"), payload, "the renamed file")
    renamed = root / "b88 renamed folder"
    daemon.command("rename", str(folder), renamed.name)
    check(renamed.name in os.listdir(root), "pdfs rename by local path did not rename the folder")
    check_bytes(read(renamed / "renamed.txt"), payload, "the file in the renamed folder")

    daemon.command("rm", str(renamed / "renamed.txt"))
    check(os.listdir(renamed) == [], f"after pdfs rm of the file: {os.listdir(renamed)}")
    daemon.command("rm", str(renamed))
    check(renamed.name not in os.listdir(root), "pdfs rm by local path left the folder listed")

    # Renaming a root to its own name changes nothing even if it gets through.
    mount = daemon.enclosing_mount(root)
    message = _refused(daemon, "rename", str(mount), mount.name)
    check("root of a synced location" in message, f"pdfs rename of {mount}: {message}")
    if ctx.owns_location and ctx.location is not None and mount == ctx.location.resolve():
        message = _refused(daemon, "rm", str(mount))
        check("root of a synced location" in message, f"pdfs rm of {mount}: {message}")
        ctx.note("rm_root", "refused")
    check(os.path.ismount(mount) and root.name in os.listdir(mount), f"{mount} changed under a refusal")

    with tempfile.TemporaryDirectory(prefix="pdfs-b88-outside-") as directory:
        outside = Path(directory) / "outside.txt"
        outside.write_bytes(payload)
        message = _refused(daemon, "rm", str(outside))
        check("not under the mountpoint" in message, f"pdfs rm outside every mount: {message}")
        check(outside.read_bytes() == payload, "pdfs rm changed a file outside every mount")


def test_cli_output_into_a_closed_pipe(ctx: Context) -> None:
    """`pdfs ls | head` ends quietly when the reader stops early.

    Rust's `println!` panics when stdout is a closed pipe, so a listing longer
    than `head` read ended with a panic message and exit status 101. The
    reading end is closed before the command starts, so its first line fails.
    """
    daemon = ctx.require_daemon()
    mountpoint = (daemon.status().get("mount") or {}).get("mountpoint")
    if not mountpoint:
        raise Skip("the daemon reports no My files mount to list")
    for args in (["ls", mountpoint], ["--json", "ls", mountpoint], ["status"]):
        reader, writer = os.pipe()
        os.close(reader)
        try:
            result = subprocess.run(
                [daemon.pdfs, *args],
                stdin=subprocess.DEVNULL,
                stdout=writer,
                stderr=subprocess.PIPE,
                text=True,
                timeout=daemon.timeout,
            )
        finally:
            os.close(writer)
        command = f"pdfs {' '.join(args)}"
        check(
            result.returncode == 0,
            f"{command} into a closed pipe exited with {result.returncode}: {result.stderr.strip()}",
        )
        check(not result.stderr.strip(), f"{command} into a closed pipe wrote: {result.stderr.strip()}")


def test_regression_b114_removed_name_stops_resolving(ctx: Context) -> None:
    """A name `pdfs rm` or `pdfs rename` took away stops resolving at once (B114).

    The FUSE handlers tell the kernel when a name goes, and the control requests
    did not. The kernel then kept answering for the old name from its entry
    cache for up to 30 seconds (`TTL` in lib.rs), while the listing no longer
    had it.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder has no kernel entry cache to go stale")
    removed = ctx.root / "b114 removed.txt"
    renamed = ctx.root / "b114 renamed.txt"
    for path in (removed, renamed):
        write_durable(path, b"b114\n")
    daemon.wait_for_queue()
    # Looked up, so the kernel holds both names.
    os.lstat(removed)
    os.lstat(renamed)
    daemon.command("rm", str(removed))
    daemon.command("rename", str(renamed), "b114 new name.txt")
    listed = os.listdir(ctx.root)
    check(removed.name not in listed and renamed.name not in listed, f"old names still listed: {listed}")
    stale = [
        f"{path.name} ({command})"
        for path, command in ((removed, "pdfs rm"), (renamed, "pdfs rename"))
        if os.path.lexists(path)
    ]
    verb = "resolves" if len(stale) == 1 else "resolve"
    check(not stale, f"{' and '.join(stale)} still {verb}, though no longer listed")


def test_regression_b115_ls_reports_the_real_size(ctx: Context) -> None:
    """`pdfs ls` reports a file's real size, not the size Drive stores (B115).

    The listing fell back to the encrypted size on storage when a node had no
    claimed size, which showed a 6-byte file as 57 bytes. `pdfs refresh` brings
    back such a listing on purpose (see B100), so both are checked.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("pdfs ls lists a mount; a mirror folder is plain local storage")
    folder = ctx.root / "b115"
    folder.mkdir()
    sizes = {"six.txt": 6, "page.bin": 4097, "block.bin": BLOCK + 7}
    for name, size in sizes.items():
        write_durable(folder / name, pattern(size, f"b115-{name}"))
    daemon.wait_for_queue()
    listings: dict[str, dict[str, int]] = {}
    for when in ("after the upload", "after pdfs refresh"):
        if when == "after pdfs refresh":
            daemon.command("refresh", str(folder))
        listed = {entry["name"]: entry["size"] for entry in daemon.listing(folder)}
        check(set(listed) == set(sizes), f"pdfs ls {when} lists {sorted(listed)}")
        listings[when] = listed
    for name, size in sizes.items():
        check(os.lstat(folder / name).st_size == size, f"{name} has the wrong size through the mount")
    wrong = []
    for name, size in sizes.items():
        shown = {when: listed[name] for when, listed in listings.items() if listed[name] != size}
        if len(set(shown.values())) == 1:
            wrong.append(f"{name}: {next(iter(shown.values()))} bytes, not {size}, {' and '.join(shown)}")
        elif shown:
            wrong.append(f"{name}: not {size} bytes but " + ", ".join(f"{n} {when}" for when, n in shown.items()))
    check(not wrong, "pdfs ls shows the wrong size\n" + "\n".join(wrong))


def test_regression_b116_trashed_folder_drops_deeper_creates(ctx: Context) -> None:
    """Trashing a folder drops the creates queued in its subfolders (B116).

    Only the ops chained to the folder through the queue, and the revisions of
    the files below it, went with the folder. A queued create in a subfolder
    that was already on Drive stayed, and once due the drain put the file in
    the root. A `*.part` file renamed while sync is paused makes such a create.
    The folder is restored before sync resumes, so a create that wrongly stays
    lands where it was written rather than in the root.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder uploads in its own passes, not through the queue")
    if (daemon.status().get("mount") or {}).get("paused"):
        raise Skip("sync is paused already; this case has to pause and resume it itself")
    folder = ctx.root / "b116"
    deeper = folder / "sub" / "deeper"
    deeper.mkdir(parents=True)
    daemon.wait_for_queue()
    uid = daemon.child_uid(ctx.root, folder.name)
    check(uid is not None, f"pdfs ls does not list {folder.name}")
    path = deeper / "b116 held.bin"
    payload = pattern(4096 + 116, "b116")
    stray = daemon.enclosing_mount(ctx.root) / path.name
    survived: list[str] = []
    in_root = False
    try:
        daemon.command("sync", "pause", "--for", "10m")
        try:
            partial = path.with_name(path.name + ".part")
            write_durable(partial, payload)
            # The rename to a final name lets the parked create go, but sync is paused.
            os.rename(partial, path)
            held = {
                item["id"]
                for item in daemon.queue()
                if item["kind"] == "create" and not item["parked"] and item["path"].endswith(path.name)
            }
            check(bool(held), "the renamed file's create was not queued, so B116 was not replayed")
            daemon.command("rm", str(folder))
            survived = [f"{item['kind']} #{item['id']}" for item in daemon.queue() if item["id"] in held]
            daemon.command("restore", uid)
        finally:
            daemon.command("sync", "resume")
        daemon.wait_for_queue()
    finally:
        # Only a create that drained while its folder was trashed lands there.
        if stray.is_file() and read(stray) == payload:
            os.unlink(stray)
            in_root = True
            ctx.info(f"the drain put {path.name} in {stray.parent}; it is in the trash now")
    check(
        not survived,
        f"the create of {path.relative_to(ctx.root)} ({', '.join(survived)}) "
        f"stayed queued after pdfs rm {folder.name}",
    )
    check(not in_root, f"the drain put {path.name} in the root, although its folder was restored first")


def test_regression_b117_moved_upload_keeps_its_path(ctx: Context) -> None:
    """A queued upload of a file `pdfs move` moved keeps its path and goes at once (B117).

    `pdfs move` and `pdfs rename` forgot the node, and that deleted its row.
    Until a listing brought the row back, `pdfs sync queue` showed the upload by
    its raw id, and the drain found no node, re-read it and put the upload off
    by one recheck. Sync is paused so the upload is queued during the move.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder uploads in its own passes, not through the queue")
    if (daemon.status().get("mount") or {}).get("paused"):
        raise Skip("sync is paused already; this case has to pause and resume it itself")
    folder = ctx.root / "b117"
    target = folder / "moved here"
    target.mkdir(parents=True)
    path = folder / "b117 moved.bin"
    write_durable(path, b"b117 first version\n")
    daemon.wait_for_queue()
    uid = daemon.child_uid(folder, path.name)
    check(uid is not None, f"pdfs ls does not list {path.name}")
    payload = pattern(BLOCK + 117, "b117")
    started = time.time()
    daemon.command("sync", "pause", "--for", "10m")
    try:
        write_durable(path, payload)
        # Waited for before the move: an upload queued after it would race nothing.
        held = {
            item["id"]
            for item in daemon.queued(
                lambda item: item["kind"] == "revision"
                and not preexisting(item)
                and item["path"].endswith(path.name),
                1,
            )
        }
        check(bool(held), "the rewrite queued no revision, so B117 was not replayed")
        daemon.command("move", str(path), str(target))
        shown = [item["path"] for item in daemon.queue() if item["id"] in held]
        check(bool(shown), "the queued revision left the queue while sync was paused")
    finally:
        daemon.command("sync", "resume")
    daemon.wait_for_queue()
    time.sleep(3)
    daemon.wait_for_queue()

    moved = target / path.name
    check_bytes(read(moved), payload, f"{path.name} after the move and the drain")
    check(path.name not in os.listdir(folder), f"{path.name} is still listed where it was")
    conflicts = _conflict_copies(folder) + _conflict_copies(target)
    check(not conflicts, f"moving a file with a queued upload made conflict copies: {conflicts}")
    symptoms = [
        f"pdfs sync queue listed its upload as {shown_path}"
        for shown_path in shown
        if not shown_path.endswith(f"{target.name}/{path.name}")
    ]
    lines = read_journal(daemon.unit, started)
    deferred = [line for line in lines or [] if uid in line and "pending operation deferred" in line]
    if deferred:
        symptoms.append(f"its upload was put off: {deferred[0]}")
    check(not symptoms, "after pdfs move\n" + "\n".join(symptoms))
    if lines is None:
        ctx.info("journalctl is unavailable, so a put-off upload could not be seen")


def test_regression_b119_uploaded_file_stays_cached(ctx: Context) -> None:
    """A file written through the mount stays cached once its upload lands (B119).

    The drain threw the staged bytes away after the upload, so the first read
    of a file just written downloaded it again. With the link down that read
    failed with EIO, which is how the end-of-run read-back crashed.
    """
    daemon = ctx.require_daemon()
    if not is_fuse(ctx.root):
        raise Skip("a mirror folder keeps its files on local storage")
    folder = ctx.root / "b119"
    folder.mkdir()
    sizes = {"small.txt": 119, "block.bin": BLOCK + 119}
    for name, size in sizes.items():
        write_durable(folder / name, pattern(size, f"b119-{name}"))
    daemon.wait_for_queue()
    listed = {entry["name"]: entry for entry in daemon.listing(folder)}
    check(set(listed) == set(sizes), f"pdfs ls lists {sorted(listed)}")
    uncached = [name for name in sizes if not listed[name].get("cached")]
    check(not uncached, f"{', '.join(uncached)} not cached after the upload, so reading it needs the network")
    for name, size in sizes.items():
        check_bytes(read(folder / name), pattern(size, f"b119-{name}"), f"{name} after the upload")


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
    runs will not have.
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


def test_regression_b100_copied_tree_exact_sizes(ctx: Context) -> None:
    """A tree copied in from another volume reads back at its exact sizes (B100).

    A listing that has not fetched a file's real size reports the encrypted
    size, 53 bytes over, and reading at that size failed with EIO. It was found
    by copying a tree and reading it straight away. `pdfs refresh` drops the
    cached listing, which brings that size-less listing back on purpose.
    """
    sizes = [0, 1, 53, 4095, 4096, 20_000, 131_073, 1_000_003]
    files: dict[Path, bytes] = {}
    copy = ctx.root / "b100-copy"
    with tempfile.TemporaryDirectory(prefix="pdfs-b100-source-") as directory:
        source = Path(directory) / "tree"
        for index in range(48):
            relative = Path(f"dir-{index % 6}") / f"sub-{index % 3}" / f"file-{index:02d}.bin"
            files[relative] = pattern(sizes[index % len(sizes)], f"b100-{index}")
            (source / relative).parent.mkdir(parents=True, exist_ok=True)
            write_durable(source / relative, files[relative])
        shutil.copytree(source, copy)

    def verify(when: str) -> None:
        for relative, data in files.items():
            size = os.lstat(copy / relative).st_size
            check(size == len(data), f"{relative} {when}: {size} bytes, expected {len(data)}")
            check_bytes(read(copy / relative), data, f"{relative} {when}")

    verify("right after the copy")
    ctx.record("sizes", sorted(len(data) for data in files.values()))
    if not ctx.is_live or ctx.daemon is None:
        return
    ctx.daemon.wait_for_queue()
    verify("after the upload")
    folders = sorted({copy / relative.parent for relative in files} | {copy})
    try:
        for folder in folders:
            ctx.daemon.command("refresh", str(folder))
    except RuntimeError as error:
        # A mirror folder is plain local storage with no listing to drop.
        ctx.note("refresh", str(error))
        return
    verify("after dropping the cached listing")


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
    def __init__(
        self,
        name: str,
        run,
        kinds: tuple[str, ...] = (REFERENCE, LIVE),
        budget_scale: int = 1,
    ) -> None:
        self.name = name
        self.run = run
        self.kinds = kinds
        # A case whose work is bound by per-operation server round trips gets a
        # multiple of the per-case timeout rather than a smaller workload.
        self.budget_scale = budget_scale


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
    # Each change and each first read is a network round trip on a mount, and
    # these two make many.
    Case("application workloads (editor, tar, sqlite, git, rsync)", test_application_workloads, budget_scale=2),
    Case("block boundaries and overwrites that change the block count", test_block_boundaries_and_overwrites),
    Case("unusual but legal names", test_unusual_names, budget_scale=2),
    Case("a deep tree", test_deep_tree),
    Case("a wide directory", test_wide_directory, budget_scale=2),
    Case("rename patterns: swap, chain, rotation, replace under a reader", test_rename_patterns),
    Case("coherency between handles and across reopen", test_handle_coherency),
    Case("throughput floors", test_throughput),
    Case("regression B7: renamed directory stays traversable", test_regression_b7_renamed_directory),
    Case("regression B69: identical rewrite makes no conflict", test_regression_b69_identical_rewrite, (LIVE,)),
    Case("regression B70: transient download name is not sealed", test_regression_b70_transient_download_name, (LIVE,)),
    Case("regression B74: rename after close keeps its content", test_regression_b74_rename_after_close, (LIVE,)),
    Case("regression B34: a viewer share is read-only and queues nothing", test_regression_b34_viewer_share_is_read_only, (LIVE,)),
    Case("regression B34b: an editor share still writes", test_regression_b34b_editor_share_still_writes, (LIVE,)),
    Case("synthetic Shared-with-me directory contract", test_shared_directory_contract, (LIVE,)),
    Case("regression B79: an on-demand device folder accepts writes", test_regression_b79_ondemand_location_accepts_writes, (LIVE,)),
    Case("regression B100: a copied tree reads back at its exact sizes", test_regression_b100_copied_tree_exact_sizes),
    Case("regression B101: a write during its create's upload reaches Drive", test_regression_b101_write_during_create_upload, (LIVE,)),
    Case("regression B113: moving a file before its upload makes no conflict", test_regression_b113_move_before_upload, (LIVE,)),
    Case("regression B102: removing a folder drops the uploads queued inside it", test_regression_b102_removed_folder_takes_its_uploads, (LIVE,)),
    Case("regression B94: a deleted .part file leaves nothing queued", test_regression_b94_deleted_transient_file_leaves_nothing_queued, (LIVE,)),
    Case("regression B95: a .tmp file uploads at once", test_regression_b95_tmp_name_uploads, (LIVE,)),
    Case("regression B111: renaming a file straight back works", test_regression_b111_rename_back_at_once),
    Case("regression B121: a file renamed and moved at once lands", test_regression_b121_move_right_after_rename),
    Case("regression B98: an upload superseded by a write ends", test_regression_b98_superseded_upload_ends, (LIVE,), budget_scale=2),
    Case("regression B80: pdfs search finds a new file after a rewrite", test_regression_b80_new_file_is_searchable, (LIVE,)),
    Case("regression B48: an emptied folder can be removed at once", test_regression_b48_emptied_folder_removes_at_once),
    Case("regression B88: pdfs mkdir, rename and rm by local path", test_regression_b88_cli_changes_by_local_path, (LIVE,)),
    Case("pdfs output into a closed pipe ends quietly", test_cli_output_into_a_closed_pipe, (LIVE,)),
    Case("regression B114: a name pdfs rm or rename took away stops resolving", test_regression_b114_removed_name_stops_resolving, (LIVE,)),
    Case("regression B115: pdfs ls reports a file's real size", test_regression_b115_ls_reports_the_real_size, (LIVE,)),
    Case("regression B116: trashing a folder drops the creates queued in its subfolders", test_regression_b116_trashed_folder_drops_deeper_creates, (LIVE,)),
    Case("regression B117: a moved file's queued upload keeps its path", test_regression_b117_moved_upload_keeps_its_path, (LIVE,)),
    Case("regression B119: an uploaded file stays cached", test_regression_b119_uploaded_file_stays_cached, (LIVE,)),
    Case("durability across a daemon restart", test_durability_across_restart, (LIVE,)),
]


# --------------------------------------------------------------------------
# Runner
# --------------------------------------------------------------------------


FAILED = {"fail", "timeout"}


class Result:
    def __init__(
        self,
        name: str,
        target: str,
        status: str,
        seconds: float,
        message: str = "",
        info: list[str] | None = None,
    ) -> None:
        self.name = name
        self.target = target
        self.status = status
        self.seconds = seconds
        self.message = message
        self.info = list(info or [])

    def as_dict(self) -> dict:
        return {
            "name": self.name,
            "target": self.target,
            "status": self.status,
            "seconds": round(self.seconds, 3),
            "message": self.message,
            "info": self.info,
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
        # What the header said about the target, for the JSON report too.
        self.facts: dict[str, str] = {}
        self.wall: float | None = None

    @property
    def failed(self) -> bool:
        return any(result.status in FAILED for result in self.results)

    @property
    def seconds(self) -> float:
        return self.wall if self.wall is not None else sum(r.seconds for r in self.results)

    def counts(self) -> dict[str, int]:
        counts = dict.fromkeys(("pass", "fail", "skip", "known"), 0)
        for result in self.results:
            counts["fail" if result.status in FAILED else result.status] += 1
        return counts


REPORT: list[Run] = []


# --------------------------------------------------------------------------
# Console output
# --------------------------------------------------------------------------

STATUS_WORDS = {"pass": "ok", "skip": "skip", "known": "known", "fail": "FAIL", "timeout": "TIMEOUT"}
BOLD, DIM, RED, GREEN, YELLOW, MAGENTA = "1", "2", "1;31", "32", "33", "35"
STATUS_STYLES = {"pass": GREEN, "skip": YELLOW, "known": MAGENTA, "fail": RED, "timeout": RED}
BUG_REFERENCE = re.compile(r"\bB(\d+)")


class Console:
    """One line per case, coloured on a terminal.

    A case's line is written when the case starts and finished with its
    result, so the case that is running, or hanging, is the last line on
    screen. Anything else written meanwhile, by the case or by cleanup, first
    ends that line instead of running into it, and the result then gets a whole
    line of its own.
    """

    def __init__(self) -> None:
        self.out = sys.stdout
        self.color = (
            self.out.isatty() and "NO_COLOR" not in os.environ and os.environ.get("TERM") != "dumb"
        )
        self.columns = shutil.get_terminal_size((100, 24)).columns
        self.pending = False

    def install(self) -> None:
        sys.stdout = _LineBreaker(self, sys.stdout)
        sys.stderr = _LineBreaker(self, sys.stderr)

    def paint(self, text: str, style: str) -> str:
        return f"\x1b[{style}m{text}\x1b[0m" if self.color else text

    def heading(self, text: str) -> None:
        print()
        print(self.paint(f"==> {text}", BOLD))

    def fact(self, key: str, value: str) -> None:
        print(f"  -> {key:<10}{value}")

    def begin(self, head: str) -> None:
        self.out.write(head)
        self.out.flush()
        self.pending = True

    def finish(self, head: str, tail: str) -> None:
        """End the line `begin` opened, or write it whole if something broke it."""
        self.out.write(f"{tail}\n" if self.pending else f"{head}{tail}\n")
        self.out.flush()
        self.pending = False

    def interrupt(self) -> None:
        if self.pending:
            self.pending = False
            self.out.write("\n")
            self.out.flush()


class _LineBreaker:
    """A stream that ends the console's open case line before it writes."""

    def __init__(self, console: Console, stream) -> None:
        self._console = console
        self._stream = stream

    def write(self, text: str) -> int:
        if text:
            self._console.interrupt()
        return self._stream.write(text)

    def __getattr__(self, name: str):
        return getattr(self._stream, name)


CONSOLE = Console()


def duration(seconds: float) -> str:
    if seconds < 60:
        return f"{seconds:.2f}s"
    minutes, rest = divmod(round(seconds), 60)
    if minutes < 60:
        return f"{minutes}m{rest:02d}s"
    hours, minutes = divmod(minutes, 60)
    return f"{hours}h{minutes:02d}m"


def plural(count: int, noun: str, many: str | None = None) -> str:
    return f"{count} {noun if count == 1 else many or noun + 's'}"


def indented(text: str, width: int) -> str:
    return "\n".join(" " * width + line for line in text.splitlines())


def tally(counts: dict[str, int], seconds: float | None = None) -> str:
    """`23 passed, 2 skipped, 1 known issue in 4m12s`, leaving out the zeros."""
    parts = [
        f"{counts['pass']} passed",
        *([f"{counts['fail']} failed"] if counts["fail"] else []),
        *([f"{counts['skip']} skipped"] if counts["skip"] else []),
        *([plural(counts["known"], "known issue")] if counts["known"] else []),
    ]
    return ", ".join(parts) + (f" in {duration(seconds)}" if seconds is not None else "")


def mount_of(path: Path) -> tuple[Path, str]:
    """The mount `path` lives on and its type, read from /proc/self/mountinfo."""
    path = path.resolve()
    best, fstype = Path("/"), ""
    with open("/proc/self/mountinfo", encoding="utf-8") as mounts:
        for line in mounts:
            fields = line.split()
            # Octal escapes (\040 for a space) are how mountinfo writes odd names.
            point = Path(fields[4].encode().decode("unicode_escape"))
            if (path == point or point in path.parents) and len(point.parts) >= len(best.parts):
                best, fstype = point, fields[fields.index("-") + 1]
    return best, fstype


def describe_storage(path: Path) -> str:
    mountpoint, fstype = mount_of(path)
    if fstype.startswith("fuse"):
        return f"FUSE mount {mountpoint} ({fstype})"
    return f"local directory on {fstype or 'an unknown filesystem'}"


def pdfs_version(pdfs: str) -> str:
    try:
        result = subprocess.run(
            [pdfs, "--version"], text=True, capture_output=True, timeout=10, stdin=subprocess.DEVNULL
        )
    except (OSError, subprocess.SubprocessError) as error:
        return f"not runnable ({error})"
    return (result.stdout.strip() or result.stderr.strip()) if result.returncode == 0 else "no version"


def describe_daemon(daemon: Daemon | None, kind: str) -> str:
    if daemon is None:
        return "not used" if kind == REFERENCE else "none reachable; cases that need it skip"
    try:
        status = daemon.status()
    except Exception as error:  # noqa: BLE001 - only describes the target
        return f"{daemon.pdfs}: status failed: {error}"
    mount = status.get("mount") or {}
    queued = sum(mount.get(key, 0) for key in ("pending_uploads", "pending_changes"))
    state = "online" if mount.get("online") else "offline"
    if mount.get("paused"):
        state += ", sync paused"
    return f"{status.get('username', 'not signed in')}, {state}, {plural(queued, 'queued op')}"


def describe_cases(kind: str, cases: list[Case], pool: list[Case]) -> str:
    of_kind = [case for case in pool if kind in case.kinds]
    text = f"{len(cases)} of {len(pool)}"
    if len(pool) > len(of_kind):
        need = "a live mount" if kind == REFERENCE else "the reference only"
        text += f", {len(pool) - len(of_kind)} need {need}"
    if len(of_kind) > len(cases):
        only = os.environ.get("PDFS_ACCEPTANCE_ONLY", "")
        text += f", {len(of_kind) - len(cases)} left out by PDFS_ACCEPTANCE_ONLY={only!r}"
    return text


def announce(run: Run, facts: dict[str, str]) -> None:
    run.facts = facts
    CONSOLE.heading(run.label)
    for key, value in facts.items():
        CONSOLE.fact(key, value)


def print_result(head: str, indent: int, result: Result) -> None:
    status = CONSOLE.paint(STATUS_WORDS[result.status].ljust(7), STATUS_STYLES[result.status])
    CONSOLE.finish(head, f"{status} {duration(result.seconds):>6}")
    pad = " " * indent
    quiet = result.status in {"pass", "skip"}
    for line in result.message.splitlines():
        print(pad + (CONSOLE.paint(line, DIM) if quiet else line))
    for line in result.info:
        print(pad + CONSOLE.paint(line, DIM))
    reference = BUG_REFERENCE.search(result.name)
    if result.status in FAILED and reference:
        print(pad + CONSOLE.paint(f"see docs/BUGS.md B{reference[1]}", DIM))


def select_cases(kind: str, pool: list[Case] | None = None) -> list[Case]:
    """The cases of `kind` whose name contains any comma-separated part of the filter."""
    selected = os.environ.get("PDFS_ACCEPTANCE_ONLY", "")
    cases = [case for case in (TESTS if pool is None else pool) if kind in case.kinds]
    needles = [part.strip().lower() for part in selected.split(",") if part.strip()]
    if not needles:
        return cases
    # A filter that names a live-only case (every regression case is one) matches
    # nothing in the reference run, which is not an error — the reference target
    # simply has nothing to do. Only a part that matches *no case at all* is a
    # typo worth failing on.
    for needle in needles:
        check(
            any(needle in case.name.lower() for case in TESTS + MOVE_CASES),
            f"PDFS_ACCEPTANCE_ONLY: {needle!r} matched no tests",
        )
    return [case for case in cases if any(needle in case.name.lower() for needle in needles)]


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


def run_cases(run: Run, cases: list[Case], context_for, timeout: int, fail_fast: bool) -> None:
    """Run `cases` into `run`, one console line each.

    A timeout ends the loop when the mount no longer answers: later cases would
    only produce noise, and cleanup already has to fight for it. A mount that
    still answers was only slow, as on a poor link, and the run goes on.
    """
    width = min(max((len(case.name) for case in cases), default=0), max(CONSOLE.columns - 32, 40))
    for index, case in enumerate(cases, 1):
        context = context_for(case)
        counter = f"[{index:>{len(str(len(cases)))}}/{len(cases)}]"
        dots = CONSOLE.paint("." * max(width - len(case.name) + 2, 2), DIM)
        head = f"  {counter} {case.name} {dots} "
        CONSOLE.begin(head)
        started = time.monotonic()
        failure = None
        try:
            with time_limit(timeout * case.budget_scale, case.name):
                case.run(context)
            status, message = "pass", ""
        except Skip as reason:
            status, message = "skip", str(reason)
        except KnownIssue as issue:
            status, message = "known", str(issue)
        except TestTimeout as reason:
            status, message = "timeout", str(reason)
        except KeyboardInterrupt:
            raise
        except BaseException as error:  # noqa: BLE001 - reported, then continued
            status, message, failure = "fail", failure_detail(error), error
        result = Result(
            case.name,
            run.label,
            status,
            time.monotonic() - started,
            message,
            getattr(context, "infos", None),
        )
        run.results.append(result)
        print_result(head, len(counter) + 3, result)
        if status == "pass":
            run.ran.add(case.name)
        if status == "timeout" and (left := len(cases) - index):
            if answers(context.root):
                print(CONSOLE.paint("  the mount still answers, so the run goes on", DIM))
                continue
            print(CONSOLE.paint(f"  {plural(left, 'later case')} not run: the mount stopped answering", DIM))
            break
        if failure is not None and fail_fast:
            raise failure


def run_contract(
    parent: Path,
    label: str,
    kind: str = REFERENCE,
    daemon=None,
    timeout: int = DEFAULT_TIMEOUT,
    fail_fast: bool = False,
    janitor: Janitor | None = None,
    owns_location: bool = False,
    reference: Run | None = None,
) -> Run:
    """Run the contract in a fresh root under `parent`.

    With a janitor the root is registered before it exists and removed by the
    janitor, through the daemon; without one it is this function's to delete.
    `owns_location` says this run created `parent` itself. A `reference` run is
    diffed against before the result line, so that line counts the comparison.
    """
    if janitor is None:
        reap_stale_roots(parent, int(os.environ.get("PDFS_ACCEPTANCE_REAP_AGE", "3600")))
        root = parent / f"pdfs-acceptance-{uuid.uuid4().hex}"
    else:
        root = janitor.new_root(parent)
    root.mkdir()
    run = Run(label, kind, root)
    REPORT.append(run)
    cases = select_cases(kind)
    announce(
        run,
        {
            "root": str(root),
            "storage": describe_storage(root),
            "daemon": describe_daemon(daemon, kind),
            "cases": describe_cases(kind, cases, TESTS),
        },
    )
    obs = Observations()

    def context_for(case: Case) -> Context:
        obs.scope(case.name)
        return Context(root, obs, kind, daemon, parent, owns_location)

    started = time.monotonic()
    try:
        run_cases(run, cases, context_for, timeout, fail_fast)
        run.wall = time.monotonic() - started
        run.compared = dict(obs.compared)
        run.noted = dict(obs.noted)
        digest_path = root / "positioned.bin"
        if digest_path.exists():
            try:
                run.digest = hashlib.sha256(read(digest_path)).hexdigest()
            except OSError as error:
                # Reported, not raised: the results and the cleanup are owed.
                run.results.append(
                    Result("a file of this run reads back at the end", run.label, "fail", 0.0, failure_detail(error))
                )
        if reference is not None:
            summarize_divergence(reference, run)
        CONSOLE.fact("result", tally(run.counts(), run.wall))
        return run
    except BaseException:
        if janitor is None:
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
                    "facts": run.facts,
                    "seconds": round(run.seconds, 3),
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
        CONSOLE.fact("report", str(json_path))
    if junit_path:
        suites = ElementTree.Element("testsuites")
        for run in REPORT:
            suite = ElementTree.SubElement(
                suites,
                "testsuite",
                name=run.label,
                tests=str(len(run.results)),
                failures=str(sum(1 for r in run.results if r.status in FAILED)),
                skipped=str(sum(1 for r in run.results if r.status in {"skip", "known"})),
                time=f"{run.seconds:.3f}",
            )
            for result in run.results:
                case = ElementTree.SubElement(
                    suite,
                    "testcase",
                    name=result.name,
                    classname=run.label,
                    time=f"{result.seconds:.3f}",
                )
                if result.status in FAILED:
                    failure = ElementTree.SubElement(case, "failure", type=result.status)
                    failure.text = result.message
                elif result.status == "skip":
                    ElementTree.SubElement(case, "skipped", message=result.message)
                elif result.status == "known":
                    # JUnit has no expected failure; a skip is what CI shows as neither.
                    ElementTree.SubElement(case, "skipped", message=f"known issue: {result.message}")
        ElementTree.ElementTree(suites).write(junit_path, encoding="unicode", xml_declaration=True)
        CONSOLE.fact("report", str(junit_path))


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


# Warnings that mean the daemon got something wrong, though it carried on. A
# conflict copy of a run's own write is B69, B105 or B113 come back; a FUSE job
# held for minutes is the shape of B107 and B110. A queued op whose authority
# had to be re-read lost its row to a control move (B117) or to a folder that
# had just landed (B120); nothing in a run takes a row away on purpose.
JOURNAL_WARNINGS = (
    "queued write conflicts; keeping a conflict copy",
    "a fuse worker has held the same job for a long time",
    "re-interned an authority missing from the local tree",
)


def read_journal(unit: str, since: float) -> list[str] | None:
    """The unit's journal lines since `since`, or None without journalctl.

    Tracing colours its output when it thinks it has a terminal, so the ANSI
    codes are stripped. Raises RuntimeError when journalctl fails.
    """
    if shutil.which("journalctl") is None:
        return None
    result = subprocess.run(
        [
            "journalctl",
            "--user",
            "-u",
            unit,
            "--since",
            f"@{int(since)}",
            "--no-pager",
            "-o",
            "cat",
        ],
        text=True,
        capture_output=True,
    )
    if result.returncode:
        raise RuntimeError(f"journalctl failed: {result.stderr.strip()}")
    return [ANSI.sub("", line).strip() for line in result.stdout.splitlines()]


class JournalWatch:
    """Fail a run that leaves errors in the daemon's journal.

    A suite can pass every assertion while the daemon logs a stream of failures
    behind it; that has happened here before, and it was only noticed by reading
    the journal by hand. The warnings in `JOURNAL_WARNINGS` count as errors.
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
        try:
            lines = read_journal(self.unit, self.since) or []
        except RuntimeError as error:
            return [str(error)]
        # Filtering on syslog priority (`-p err`) finds nothing: the daemon writes
        # tracing levels as text on stdout, which journald files at the unit's
        # default priority. Match the level token instead.
        return [
            line
            for line in lines
            if ERROR_LEVEL.search(line) or any(warning in line for warning in JOURNAL_WARNINGS)
        ]


# --------------------------------------------------------------------------
# Daemon control
# --------------------------------------------------------------------------


# Queue ops that were already there when a target started. They belong
# to the user, to an earlier run that died, or to an earlier target of this run
# that timed out, and may never drain: a target that waited for an empty queue
# would time out on every sync case. Waits skip them. The id alone does not
# name an op: the daemon numbers them afresh once its queue has emptied.
PREEXISTING_OPS: set[tuple[int, int]] = set()


def preexisting(item: dict) -> bool:
    return (item["id"], item["queued_at"]) in PREEXISTING_OPS


def remember_preexisting_ops(queue: list[dict]) -> None:
    new = [item for item in queue if not preexisting(item)]
    if not new:
        return
    PREEXISTING_OPS.update((item["id"], item["queued_at"]) for item in new)
    paths = sorted({item["path"] for item in new})
    shown = ", ".join(paths[:3])
    if len(paths) > 3:
        shown += f" and {len(paths) - 3} more"
    print(f"NOTE: {plural(len(new), 'queued op')} predate this target, for {shown}; queue waits ignore them")


def queue_settled(mount: dict | None, queue) -> bool:
    """Whether everything this run queued has drained. `queue` lists the ops.

    `mount` is `pdfs --json status`'s "mount", None while no daemon answers:
    just after a restart, before the new one listens. That is not a drained
    queue, only an unread one.
    """
    if mount is None:
        return False
    if mount.get("pending_uploads", 0) == 0 and mount.get("pending_changes", 0) == 0:
        return True
    if not PREEXISTING_OPS:
        return False
    return all(preexisting(item) for item in queue())


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
        with contextlib.suppress(Exception):
            remember_preexisting_ops(daemon.queue())
        return daemon

    def command(self, *args: str, json_output: bool = False, stdin: str | None = None) -> str:
        command = [self.pdfs]
        if json_output:
            command.append("--json")
        command.extend(args)
        result = subprocess.run(
            command,
            text=True,
            capture_output=True,
            timeout=self.timeout,
            input=stdin,
            stdin=None if stdin is not None else subprocess.DEVNULL,
        )
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
            last = self.status().get("mount")
            if queue_settled(last, self.queue):
                return
            if last and last.get("parked_uploads"):
                # A transient name's create waits for a rename, not for the
                # drain, so waiting out the timeout would only hide which file.
                parked = [
                    f"{item['kind']} {item['path']}"
                    for item in self.queue()
                    if item["parked"] and not preexisting(item)
                ]
                check(not parked, f"the queue holds parked ops that never drain: {', '.join(parked)}")
            time.sleep(1)
        raise TimeoutError(f"daemon mutation queue did not drain: {last}")

    def queue(self) -> list[dict]:
        """The daemon's queued uploads and changes, each with its path."""
        return json.loads(self.command("sync", "queue", json_output=True)).get("items", [])

    def queued(self, wanted, count: int, seconds: float = 10) -> list[dict]:
        """The queued ops `wanted` picks, once there are `count` of them.

        A write is queued when the kernel releases the file, and `close(2)` does
        not wait for the release. A queue read over the control socket right
        after a close can therefore miss the last write: "regression B102" saw
        one revision of two that way. Gives up after `seconds` and returns what
        there is.
        """
        deadline = time.monotonic() + seconds
        while True:
            items = [item for item in self.queue() if wanted(item)]
            if len(items) >= count or time.monotonic() >= deadline:
                return items
            time.sleep(0.2)

    def sync_folders(self) -> list[dict]:
        return json.loads(self.command("sync", "list", json_output=True))["items"]

    def listing(self, directory: Path) -> list[dict]:
        """A directory's entries as the daemon knows them, without touching FUSE."""
        return json.loads(self.command("ls", str(directory), json_output=True)).get("entries", [])

    def child_uid(self, directory: Path, name: str) -> str | None:
        return next((entry["uid"] for entry in self.listing(directory) if entry["name"] == name), None)

    def trashed(self, uid: str) -> bool:
        """Whether `uid` is in the trash, asking the server rather than the cache."""
        self.command("refresh", "trash")
        return any(entry["uid"] == uid for entry in self.trash())

    def trash(self) -> list[dict]:
        output = self.command("trash", json_output=True)
        try:
            return json.loads(output).get("entries", [])
        except ValueError:
            # A CLI from before `trash --json`: read its table instead.
            entries = []
            for line in output.splitlines():
                match = TRASH_LINE.match(line)
                if match:
                    entries.append({"is_dir": match[1] == "d", "name": match[3], "uid": match[4]})
            return entries

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


TRASH_LINE = re.compile(r"^([d-])\s+(\d+)  (.*)  \[([^\]]+)\]$")


def acceptance_home() -> Path:
    """Where account runs keep their manifests and their local sync folders."""
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "pdfs-acceptance"


class Janitor:
    """What one account run creates, written down before it exists.

    Every remote root and every sync folder goes into a manifest *before* it
    is created, and the manifest sits next to a lock this process holds for its
    whole life. Cleanup never has to guess: on a pass, a failure or a signal it
    removes exactly what the manifest lists. A run killed outright (SIGKILL,
    the hard timeout, a power cut) leaves a manifest whose lock nobody holds,
    and the next run finishes that cleanup before it starts its own.

    Removal goes through the daemon, not `rm -rf`: one server-side trash of the
    root, then a permanent delete by uid. Deleting a trashed folder for good
    also takes every entry that was trashed out of it one at a time, so the
    account's trash ends up exactly as it was.
    """

    def __init__(self, daemon: Daemon, directory: Path, lock_fd: int) -> None:
        self.daemon = daemon
        self.directory = directory
        self.lock_fd = lock_fd
        self.roots: list[str] = []
        self.sync_folders: list[str] = []

    @classmethod
    def start(cls, daemon: Daemon) -> Janitor:
        home = acceptance_home()
        home.mkdir(parents=True, exist_ok=True)
        # Locked under a hidden name and only then renamed into view, so a
        # concurrent reaper never sees a run directory without a held lock.
        token = uuid.uuid4().hex
        pending = home / f".run-{token}"
        pending.mkdir()
        lock_fd = os.open(pending / "lock", os.O_CREAT | os.O_RDWR, 0o600)
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        directory = home / f"run-{token}"
        os.rename(pending, directory)
        janitor = cls(daemon, directory, lock_fd)
        janitor.save()
        return janitor

    @property
    def manifest(self) -> Path:
        return self.directory / "manifest.json"

    def save(self) -> None:
        payload = {
            "pid": os.getpid(),
            "started": int(time.time()),
            "roots": self.roots,
            "sync_folders": self.sync_folders,
        }
        temporary = self.directory / "manifest.json.tmp"
        temporary.write_text(json.dumps(payload, indent=2))
        os.replace(temporary, self.manifest)

    def new_root(self, parent: Path) -> Path:
        """A fresh root in a mount, recorded before the caller creates it."""
        root = parent / f"pdfs-acceptance-{uuid.uuid4().hex}"
        self.roots.append(str(root))
        self.save()
        return root

    def new_sync_folder(self) -> Path:
        """A fresh local directory to register as a sync folder.

        The daemon names the remote folder after the local basename, so the
        basename carries the same unique marker as every other root.
        """
        path = self.directory / f"pdfs-acceptance-{uuid.uuid4().hex}"
        self.sync_folders.append(str(path))
        self.save()
        path.mkdir()
        return path

    # -- cleanup ------------------------------------------------------------

    def cleanup(self) -> list[str]:
        """Remove everything recorded; return what could not be removed."""
        leftovers: list[str] = []
        with shielded():
            print(f"[cleanup] removing what this run created ({self.directory.name})")
            with contextlib.suppress(Exception):
                self.daemon.wait_for_queue()
            for local in reversed(self.sync_folders):
                leftovers.extend(self._remove_sync_folder(Path(local)))
            for root in reversed(self.roots):
                leftovers.extend(self._remove_root(Path(root)))
            leftovers.extend(self._queued_leftovers())
            if leftovers:
                for line in leftovers:
                    print(f"[cleanup] LEFT OVER: {line}")
                print(f"[cleanup] the manifest stays at {self.manifest}; the next run retries")
            else:
                self._forget()
                print("[cleanup] nothing left behind")
        return leftovers

    def _queued_leftovers(self) -> list[str]:
        """Queued ops still naming something this run created.

        Removing a root has to take its queued uploads along. One that
        survives keeps its staged bytes on disk and retries against the trash
        forever, or lands later as a conflict copy, so it is a leftover like
        any file (`docs/BUGS.md` B102). The daemon drops them as it trashes,
        so this only waits out a drain worker that was mid-attempt.
        """
        names = {Path(path).name for path in self.roots + self.sync_folders}
        deadline = time.monotonic() + min(self.daemon.timeout, 30)
        while True:
            try:
                queued = [
                    item
                    for item in self.daemon.queue()
                    if names.intersection(Path(item.get("path", "")).parts)
                ]
            except Exception as error:  # noqa: BLE001 - reported as a leftover
                return [f"could not read the daemon queue: {error}"]
            if not queued or time.monotonic() >= deadline:
                break
            time.sleep(1)
        return [
            f"queued {item['kind']} #{item['id']} for {item['path']}"
            + (f" ({item['last_error']})" if item.get("last_error") else "")
            for item in queued
        ]

    def _forget(self) -> None:
        with contextlib.suppress(OSError):
            os.close(self.lock_fd)
        shutil.rmtree(self.directory, ignore_errors=True)

    def _remove_sync_folder(self, local: Path) -> list[str]:
        leftovers: list[str] = []
        registered = None
        try:
            registered = next(
                (
                    item
                    for item in self.daemon.sync_folders()
                    if Path(item["local_path"]) == local
                ),
                None,
            )
        except Exception as error:  # noqa: BLE001 - reported as a leftover
            return [f"sync folder {local}: could not list sync folders: {error}"]
        if registered is not None:
            print(f"[cleanup] unregistering sync folder {local}")
            try:
                self.daemon.command("sync", "rm", str(registered["id"]), "--delete-remote")
            except Exception as error:  # noqa: BLE001 - reported as a leftover
                leftovers.append(f"sync folder {local}: sync rm failed: {error}")
            uid = registered.get("remote_uid")
            if uid and not self.purge(uid, local.name):
                leftovers.append(f"remote folder {local.name} ({uid}) is still in the trash")
        else:
            # Unregistered by an earlier attempt that died before the purge.
            leftovers.extend(self._purge_by_name(local.name))
        if is_mountpoint(local):
            leftovers.append(f"{local} is still mounted")
        elif local.exists() or local.is_symlink():
            shutil.rmtree(local, ignore_errors=True)
            if local.exists():
                leftovers.append(f"local directory {local} could not be removed")
        return leftovers

    def _remove_root(self, root: Path) -> list[str]:
        uid = None
        try:
            uid = self.daemon.child_uid(root.parent, root.name)
        except Exception as error:  # noqa: BLE001 - reported as a leftover
            return [f"{root}: could not list its parent through the daemon: {error}"]
        if uid is None:
            # Never created, or trashed by an attempt that died before the purge.
            return self._purge_by_name(root.name)
        print(f"[cleanup] removing {root}")
        try:
            self.daemon.command("rm", str(root))
        except Exception as error:  # noqa: BLE001 - reported as a leftover
            return [f"{root}: pdfs rm failed: {error}"]
        if not self.purge(uid, root.name):
            return [f"{root.name} ({uid}) is still in the trash"]
        return []

    def _purge_by_name(self, name: str) -> list[str]:
        try:
            self.daemon.command("refresh", "trash")
            matches = [entry for entry in self.daemon.trash() if entry["name"] == name]
        except Exception as error:  # noqa: BLE001 - reported as a leftover
            return [f"{name}: could not read the trash: {error}"]
        return [
            f"{name} ({entry['uid']}) is still in the trash"
            for entry in matches
            if not self.purge(entry["uid"], name)
        ]

    def purge(self, uid: str, name: str) -> bool:
        """Permanently delete one trashed node the run owns, by uid.

        By uid and never by name: a name can match something of the user's, a
        uid cannot. The node has to show up in the trash first, which is also
        what proves the trash step before it worked, and it has to be gone from
        a fresh listing afterwards.

        The server's own answer comes first, though: it only deletes a node
        that is in the trash, and it answers per uid. A listing is a whole-trash
        enumeration, which on a large trash can time out on every attempt and
        would otherwise keep a folder the server would delete on request.
        """
        with contextlib.suppress(Exception):
            reply = self.daemon.command("delete-forever", uid, stdin="yes\n")
            if re.search(r"\bpermanently deleted 1 item", reply):
                print(f"[cleanup] permanently deleted {name}")
                return True
        deadline = time.monotonic() + self.daemon.timeout
        attempted = False
        last_error = ""
        while True:
            try:
                if self.daemon.trashed(uid):
                    attempted = True
                    self.daemon.command("delete-forever", uid, stdin="yes\n")
                # Gone from the trash after a delete was sent, even one that
                # reported an error, means the delete landed.
                if attempted and not self.daemon.trashed(uid):
                    print(f"[cleanup] permanently deleted {name}")
                    return True
            except Exception as error:  # noqa: BLE001 - retried, then reported
                last_error = f": {error}"
            if time.monotonic() >= deadline:
                stage = "it is still in the trash" if attempted else "it never reached the trash"
                print(f"WARNING: could not permanently delete {name} ({uid}): {stage}{last_error}")
                return False
            time.sleep(3)

    # -- reaping ------------------------------------------------------------

    @classmethod
    def reap(cls, daemon: Daemon, my_files: Path | None) -> list[str]:
        """Finish the cleanup of every earlier run that died before its own.

        A run directory whose lock can be taken belongs to a dead process. Its
        manifest says exactly what to remove. Afterwards, sync folders under
        the acceptance directory and trash entries named like a root that no
        live run claims are removed too: they are what a run leaves when it
        dies between creating something and writing it down, which the
        manifest ordering makes rare but not impossible.
        """
        home = acceptance_home()
        home.mkdir(parents=True, exist_ok=True)
        leftovers: list[str] = []
        live_names: set[str] = set()
        for directory in sorted(home.iterdir()):
            if not directory.is_dir():
                continue
            lock_path = directory / "lock"
            try:
                lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
            except OSError:
                continue
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                os.close(lock_fd)
                live_names.update(cls._claimed_names(directory))
                continue
            print(f"[reap] finishing the cleanup of an earlier run: {directory.name}")
            janitor = cls(daemon, directory, lock_fd)
            with contextlib.suppress(OSError, ValueError):
                recorded = json.loads((directory / "manifest.json").read_text())
                janitor.roots = list(recorded.get("roots", []))
                janitor.sync_folders = list(recorded.get("sync_folders", []))
            # Folders that were created but not yet recorded still sit here.
            for child in directory.iterdir():
                if STALE_ROOT.match(child.name) and str(child) not in janitor.sync_folders:
                    janitor.sync_folders.append(str(child))
            leftovers.extend(janitor.cleanup())
        leftovers.extend(cls._reap_orphans(daemon, home, live_names, my_files))
        return leftovers

    @staticmethod
    def _claimed_names(directory: Path) -> set[str]:
        try:
            recorded = json.loads((directory / "manifest.json").read_text())
        except (OSError, ValueError):
            return set()
        return {Path(path).name for path in recorded.get("roots", []) + recorded.get("sync_folders", [])}

    @classmethod
    def _reap_orphans(
        cls, daemon: Daemon, home: Path, live_names: set[str], my_files: Path | None
    ) -> list[str]:
        leftovers: list[str] = []
        orphan = cls(daemon, home, -1)
        if my_files is not None:
            # Roots from `--live` runs, or from a run on another machine, have
            # no manifest here. Age is the only safe signal for those.
            cutoff = time.time() - int(os.environ.get("PDFS_ACCEPTANCE_REAP_AGE", "3600"))
            try:
                entries = daemon.listing(my_files)
            except Exception as error:  # noqa: BLE001 - reported as a leftover
                entries = []
                leftovers.append(f"could not list {my_files}: {error}")
            for entry in entries:
                if (
                    STALE_ROOT.match(entry["name"])
                    and entry["name"] not in live_names
                    and entry.get("modified", 0) < cutoff
                ):
                    print(f"[reap] removing an abandoned root: {my_files / entry['name']}")
                    leftovers.extend(orphan._remove_root(my_files / entry["name"]))
        try:
            folders = daemon.sync_folders()
        except Exception as error:  # noqa: BLE001 - reported as a leftover
            return [f"could not list sync folders: {error}"]
        for item in folders:
            local = Path(item["local_path"])
            if home in local.parents and local.name not in live_names:
                print(f"[reap] removing orphaned sync folder {local}")
                leftovers.extend(orphan._remove_sync_folder(local))
        try:
            daemon.command("refresh", "trash")
            trashed = daemon.trash()
        except Exception as error:  # noqa: BLE001 - reported as a leftover
            return leftovers + [f"could not read the trash: {error}"]
        for entry in trashed:
            if STALE_ROOT.match(entry["name"]) and entry["name"] not in live_names:
                print(f"[reap] permanently deleting an abandoned root from the trash: {entry['name']}")
                if not orphan.purge(entry["uid"], entry["name"]):
                    leftovers.append(f"{entry['name']} ({entry['uid']}) is still in the trash")
        return leftovers


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


MODE_SWITCH_FILE = "pdfs-mode-switch.bin"


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
        # What the mirror engine uploaded right before a switch to on-demand,
        # by folder, and one result per such switch (B86).
        self.switch_payloads: dict[Path, bytes] = {}
        self.switches = Run("mode switches", LIVE, self.paths[0].parent)

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
            last = value.get("mount")
            if queue_settled(
                last,
                lambda: json.loads(self.command("sync", "queue", json_output=True))["items"],
            ):
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
        switched: set[Path] = set()
        for path, mode in zip(self.paths, modes, strict=True):
            current = self.wait_for(path)
            if current["mode"] != mode:
                if mode == "ondemand":
                    # Uploaded after the mount's last listing of the folder, if
                    # an earlier pairing had it on-demand: that listing is stale.
                    payload = pattern(4096 + 86, f"b86-{path}-{uuid.uuid4()}")
                    write_durable(path / MODE_SWITCH_FILE, payload)
                    self.switch_payloads[path] = payload
                    self.force_sync(path)
                print(f"[setup] switching {path} to {mode}")
                self.command("sync", "mode", str(self.ids[path]), mode)
                switched.add(path)
        for path, mode in zip(self.paths, modes, strict=True):
            self.wait_for(path, mode=mode)
            # The mode row flips before the asynchronous restore pass starts,
            # and its prior idle/last_sync values remain visible meanwhile.
            # Demand a completed pass before inspecting restored local bytes.
            if path in switched and mode == "mirror":
                self.force_sync(path)
            mounted = is_mountpoint(path)
            check(mounted == (mode == "ondemand"), f"{path}: mode is {mode}, mounted={mounted}")
            self.check_sentinel(path, mode, path in switched)
            if mode == "ondemand" and path in self.switch_payloads:
                self.check_switch_file(path)

    def online(self) -> bool:
        try:
            status = json.loads(self.command("status", json_output=True))
        except (RuntimeError, ValueError):
            return False
        return bool((status.get("mount") or {}).get("online"))

    def through_outage(self, action, what: str, info: list[str]):
        """`action()`, and once more if it failed with EIO.

        The switch to on-demand frees the mirror's local copy, so the folder is
        read from Drive. When the link drops meanwhile, the daemon fails the
        read with EIO once its read budget runs out. That is right, and says
        nothing about the switch, so wait for the daemon to be back online and
        let the second try decide.
        """
        try:
            return action()
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            first = failure_detail(error)
        deadline = time.monotonic() + self.timeout
        waited = False
        while not self.online():
            if time.monotonic() >= deadline:
                raise TimeoutError(f"{what}: {first}; the daemon stayed offline for {self.timeout}s")
            waited = True
            time.sleep(2)
        info.append(
            f"{what} failed with EIO while the daemon was offline; tried again once it was back"
            if waited
            else f"{what} failed with EIO; tried once more"
        )
        return action()

    def record_switch(self, name: str, started: float, message: str, info: list[str]) -> None:
        result = Result(
            name,
            self.switches.label,
            "fail" if message else "pass",
            time.monotonic() - started,
            message,
            info,
        )
        self.switches.results.append(result)
        print_result(f"  {result.name} ", 4, result)

    def check_sentinel(self, path: Path, mode: str, switched: bool) -> None:
        """Record whether the file written at setup is still there, byte for byte."""
        started = time.monotonic()
        sentinel = path / "pdfs-mode-preservation.bin"
        info: list[str] = []
        message = ""
        try:
            check_bytes(
                self.through_outage(lambda: read(sentinel), f"reading {sentinel.name}", info),
                self.sentinels[path],
                f"{sentinel.name} in {mode}",
            )
        except (AssertionError, OSError) as error:
            message = failure_detail(error)
        which = ("first", "second")[self.paths.index(path)]
        shown = "on-demand" if mode == "ondemand" else mode
        self.record_switch(
            f"the {which} folder keeps its file across the switch to {shown}"
            if switched
            else f"the {which} folder keeps its file while it stays {shown}",
            started,
            message,
            info,
        )

    def check_switch_file(self, path: Path) -> None:
        """Record whether the mount lists what the mirror uploaded just before (B86).

        The mount used to serve the folder's listing from before the mirror
        engine's uploads, so the files were missing until the next listing.
        """
        started = time.monotonic()
        switched = path / MODE_SWITCH_FILE
        info: list[str] = []
        message = ""
        try:
            listed = self.through_outage(lambda: os.listdir(path), "listing the folder", info)
            check(switched.name in listed, f"{switched.name} is not listed after the switch: {sorted(listed)}")
            check_bytes(
                self.through_outage(lambda: read(switched), f"reading {switched.name}", info),
                self.switch_payloads[path],
                f"{switched.name} after the switch",
            )
        except (AssertionError, OSError) as error:
            message = failure_detail(error)
        which = ("first", "second")[self.paths.index(path)]
        self.record_switch(
            f"regression B86: the {which} folder lists the mirror's last upload once on-demand",
            started,
            message,
            info,
        )
        with contextlib.suppress(OSError):
            switched.unlink()
            del self.switch_payloads[path]

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
            owned = {"pdfs-mode-preservation.bin": self.sentinels[path]}
            if path in self.switch_payloads:
                owned[MODE_SWITCH_FILE] = self.switch_payloads[path]
            for name, payload in owned.items():
                sentinel = path / name
                try:
                    if sentinel.exists() and read(sentinel) == payload:
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
        """`pdfs move`, bounded: a wedged mount must not hang the harness.

        A mirror's sync pass holds the folder, and the daemon refuses a move
        that waited 5 s for it. A periodic pass over a folder earlier runs
        filled can take longer, so the move is tried again until it ends.
        With several sources, the ones an attempt already moved are gone, and
        only the rest are tried again."""
        *sources, dest = paths
        deadline = time.monotonic() + self.pair.timeout
        while True:
            command = [self.pair.pdfs, "move", *(str(path) for path in (*sources, dest))]
            result = subprocess.run(
                command, text=True, capture_output=True, timeout=self.pair.timeout
            )
            detail = result.stderr.strip() or result.stdout.strip()
            if not (result.returncode and "busy syncing" in detail) or time.monotonic() > deadline:
                break
            left = [source for source in sources if os.path.lexists(source)]
            if not left:
                break
            sources = left
            time.sleep(2)
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


def _copy_between(source: Location, dest: Location) -> None:
    name = f"copied-{uuid.uuid4().hex[:8]}"
    files = _move_tree(source.root, name)
    source.settle()
    shutil.copytree(source.root / name, dest.root / name)
    for relative, data in files.items():
        size = os.lstat(dest.root / relative).st_size
        check(size == len(data), f"{dest.name}: {relative} is {size} bytes right after the copy, expected {len(data)}")
        check_bytes(read(dest.root / relative), data, f"{dest.name}: {relative} right after the copy")
    source.settle()
    dest.settle()
    for location in (source, dest):
        _unchanged(location, files, "after the copy settled")
        check((location.root / name / "empty-dir").is_dir(), f"{location.name}: the empty folder is missing")
        copies = location.conflict_copies()
        check(not copies, f"{location.name}: the copy left conflict copies: {copies}")


def test_copy_tree_between_locations(mc: MoveContext) -> None:
    _copy_between(mc.first, mc.second)


def test_copy_tree_out_of_my_files(mc: MoveContext) -> None:
    if mc.myfiles is None:
        raise Skip("the daemon reports no My files mount")
    _copy_between(mc.myfiles, mc.first)


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
    if dest.mode == "mirror" and os.stat(source.folder).st_dev == os.stat(dest.folder).st_dev:
        # Between two mirrors on one filesystem the local copy is renamed
        # along, so the move loses nothing and goes ahead.
        mc.move_ok(source.root / name, dest.root)
        check((dest.root / name / "link").is_symlink(), "the symlink did not move along")
        dest.settle()
        check_bytes(
            read(dest.root / name / "data.txt"),
            files[Path(name) / "data.txt"],
            f"{dest.name}: data.txt after the move",
        )
        check(not (source.root / name).exists(), f"{source.name}: {name} is still there")
        return
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
    Case("copy a tree between the two locations", test_copy_tree_between_locations, (LIVE,)),
    Case("copy a tree out of My files", test_copy_tree_out_of_my_files, (LIVE,)),
    Case("move refuses a name taken at the destination", test_move_refuses_a_name_taken_at_the_destination, (LIVE,)),
    Case("move refuses a folder into itself", test_move_refuses_a_folder_into_itself, (LIVE,)),
    Case("move never loses a mirror copy Drive lacks", test_move_refuses_a_mirror_copy_drive_lacks, (LIVE,)),
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
    pair: ManagedSyncPair,
    modes: tuple[str, str],
    timeout: int,
    fail_fast: bool,
    janitor: Janitor | None = None,
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
    if myfiles is not None and janitor is not None:
        myfiles.root = janitor.new_root(my_files)
    locations = [first, second] + ([myfiles] if myfiles else [])
    label = f"managed move {modes[0]}/{modes[1]}"
    run = Run(label, LIVE, first.root)
    REPORT.append(run)
    cases = select_cases(LIVE, MOVE_CASES)
    announce(
        run,
        {
            "first": f"{first.mode}, {first.folder}",
            "second": f"{second.mode}, {second.folder}",
            "my files": str(my_files) if my_files else "not mounted; its cases skip",
            "cases": describe_cases(LIVE, cases, MOVE_CASES),
        },
    )
    started = time.monotonic()
    try:
        for location in locations:
            if janitor is None:
                reap_stale_roots(location.folder, int(os.environ.get("PDFS_ACCEPTANCE_REAP_AGE", "3600")))
            location.root.mkdir()
        for location in locations:
            location.settle()
        context = MoveContext(first, second, myfiles)
        run_cases(run, cases, lambda _case: context, timeout, fail_fast)
        run.wall = time.monotonic() - started
        CONSOLE.fact("result", tally(run.counts(), run.wall))
    finally:
        for location in locations:
            # The janitor removes its My files root in one server-side step,
            # and each sync folder goes away whole with its registration.
            if janitor is not None:
                continue
            try:
                pair.remove_tree(location.root)
                location.settle()
            except Exception as error:  # noqa: BLE001 - cleanup must reach every root
                print(f"WARNING: could not remove {location.root}: {error}")
    return run


def is_mountpoint(path: Path) -> bool:
    """True for a distinct mount, including FUSE mounts over an existing dir."""
    return os.path.ismount(path)


def is_fuse(path: Path) -> bool:
    """True when `path` lives on a FUSE mount."""
    return mount_of(path)[1].startswith("fuse")


def run_managed_matrix(
    paths: list[Path], reference: Run, args, janitor: Janitor | None = None
) -> None:
    timeout = int(os.environ.get("PDFS_ACCEPTANCE_SYNC_TIMEOUT", "180"))
    pair = ManagedSyncPair(paths, timeout)
    daemon = Daemon.discover(timeout, may_restart=args.durability)
    roots: list[Path] = []
    try:
        pair.create()
        # --quick still puts each mode on each side once; the full matrix adds
        # the same-mode pairs, where a move never changes kind of storage.
        matrices = [("ondemand", "mirror")] if args.quick else [
            ("ondemand", "ondemand"),
            ("ondemand", "mirror"),
            ("mirror", "ondemand"),
            ("mirror", "mirror"),
        ]
        for index, modes in enumerate(matrices, 1):
            CONSOLE.heading(
                f"Mode pairing {index}/{len(matrices)}: first folder {modes[0]}, second {modes[1]}"
            )
            pair.set_modes(modes)
            for path, mode in zip(pair.paths, modes, strict=True):
                run = run_contract(
                    path,
                    f"managed {mode} {path.name}",
                    kind=LIVE,
                    daemon=daemon,
                    timeout=args.timeout,
                    fail_fast=args.fail_fast,
                    owns_location=True,
                    reference=reference,
                )
                roots.append(run.root)
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
            run_move_contract(pair, modes, max(args.timeout, 8 * timeout), args.fail_fast, janitor)
            for path, mode in zip(pair.paths, modes, strict=True):
                if mode == "mirror":
                    pair.force_sync(path)
    finally:
        if pair.switches.results:
            REPORT.append(pair.switches)
        if janitor is None:
            with shielded():
                for root in roots:
                    shutil.rmtree(root, ignore_errors=True)
                pair.cleanup()


def summarize_divergence(reference: Run, target: Run) -> None:
    divergences = compare_runs(reference, target)
    capabilities = note_capability_differences(reference, target)
    shared = reference.ran & target.ran
    compared = sum(1 for key in reference.compared if key.split(".", 1)[0] in shared)
    if divergences:
        CONSOLE.fact(
            "reference",
            CONSOLE.paint(
                f"{plural(len(divergences), 'observation')} of {compared}"
                f" {'differs' if len(divergences) == 1 else 'differ'}",
                RED,
            )
            + " from the ordinary filesystem:",
        )
        for line in divergences:
            print(f"               ! {line}")
    elif compared:
        CONSOLE.fact(
            "reference",
            f"matches the ordinary filesystem in {plural(compared, 'compared observation')}",
        )
    else:
        CONSOLE.fact("reference", "nothing to compare: no case that records one passed on both")
    if capabilities:
        CONSOLE.fact("accepted", f"{plural(len(capabilities), 'capability', 'capabilities')} differ:")
        for line in capabilities:
            print(CONSOLE.paint(f"               - {line}", DIM))
    if not divergences and not compared:
        return
    target.results.append(
        Result(
            "differential comparison against the reference filesystem",
            target.label,
            "fail" if divergences else "pass",
            0.0,
            "; ".join(divergences),
        )
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    live_mode = parser.add_mutually_exclusive_group()
    live_mode.add_argument(
        "--account",
        action="store_true",
        help="run everything on the signed-in account in folders of its own, then remove them",
    )
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
    parser.add_argument(
        "--quick",
        action="store_true",
        help="with --account or --managed-live: one mode pairing instead of all four",
    )
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

    install_interrupt_handlers()
    CONSOLE.install()
    started = time.monotonic()
    describe_run(args)
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
        CONSOLE.fact(
            "warning",
            CONSOLE.paint("this failed on an ordinary filesystem: suspect the runner or this machine", RED),
        )

    try:
        if args.account:
            run_account(reference, args)
        elif args.managed_live:
            run_managed_matrix(args.managed_live, reference, args)
        elif args.live:
            run_live(args.live, reference, args)
    finally:
        outcome = finish(args, journal, time.monotonic() - started)
    return outcome


def describe_run(args) -> None:
    if args.account:
        mode = "the signed-in account"
    elif args.managed_live:
        mode = "managed sync folders " + ", ".join(map(str, args.managed_live))
    elif args.live:
        mode = "live mounts " + ", ".join(map(str, args.live))
    else:
        mode = "the account-free reference only"
    CONSOLE.heading(f"pdfs acceptance suite on {mode}")
    if args.account or args.managed_live or args.live:
        pdfs = os.environ.get("PDFS_ACCEPTANCE_PDFS", "pdfs")
        CONSOLE.fact("pdfs", f"{shutil.which(pdfs) or pdfs}, {pdfs_version(pdfs)}")
    CONSOLE.fact("python", f"{platform.python_version()} on {platform.system()} {platform.release()}")
    limits = f"{args.timeout}s per case"
    if args.budget:
        limits += f", {args.budget:g}s budget"
    CONSOLE.fact("limits", limits)
    options = [
        flag
        for flag, on in (
            ("--quick", args.quick),
            ("--fail-fast", args.fail_fast),
            ("--durability", args.durability),
            ("--journal-check", args.journal_check),
        )
        if on
    ]
    if os.environ.get("PDFS_ACCEPTANCE_ONLY"):
        options.append(f"PDFS_ACCEPTANCE_ONLY={os.environ['PDFS_ACCEPTANCE_ONLY']!r}")
    if options:
        CONSOLE.fact("options", ", ".join(options))
    CONSOLE.fact("started", time.strftime("%Y-%m-%d %H:%M:%S"))


def run_account(reference: Run, args) -> None:
    """Everything, on the signed-in account, with nothing left behind.

    The contract runs in My files, then in two sync folders this run creates
    and registers itself, through every pairing of on-demand and mirror, with
    the moves between them and My files. The janitor owns all of it.
    """
    timeout = int(os.environ.get("PDFS_ACCEPTANCE_SYNC_TIMEOUT", "180"))
    daemon = Daemon.discover(timeout, may_restart=args.durability)
    if daemon is None:
        raise RuntimeError(
            "no running pdfs daemon: start proton-drive.service and sign in, "
            "or pass --offline-only for the account-free contract"
        )
    # A daemon that was just (re)started answers before it has mounted My
    # files; give it the sync timeout to get there rather than failing at once.
    deadline = time.monotonic() + timeout
    while True:
        status = daemon.status()
        mountpoint = (status.get("mount") or {}).get("mountpoint")
        ready = bool(mountpoint) and is_mountpoint(Path(mountpoint))
        if ready or not status.get("logged_in") or time.monotonic() >= deadline:
            break
        time.sleep(1)
    check(bool(status.get("logged_in")), "the daemon is not signed in; run `pdfs login` first")
    check(
        bool(mountpoint) and is_mountpoint(Path(mountpoint)),
        f"My files is not mounted (daemon reports {mountpoint!r})",
    )
    my_files = Path(mountpoint)
    print(f"[account] {status.get('username', '?')}, My files at {my_files}")

    stale = Janitor.reap(daemon, my_files)
    check(not stale, f"leftovers from an earlier run could not be removed: {stale}")
    janitor = Janitor.start(daemon)
    try:
        run = run_contract(
            my_files,
            f"My files {my_files}",
            kind=LIVE,
            daemon=daemon,
            timeout=args.timeout,
            fail_fast=args.fail_fast,
            janitor=janitor,
            reference=reference,
        )
        paths = [janitor.new_sync_folder(), janitor.new_sync_folder()]
        run_managed_matrix(paths, reference, args, janitor)
    finally:
        started = time.monotonic()
        leftovers = janitor.cleanup()
        cleanup = Run("cleanup", LIVE, janitor.directory)
        cleanup.results.append(
            Result(
                "the run left nothing behind",
                "cleanup",
                "fail" if leftovers else "pass",
                time.monotonic() - started,
                "; ".join(leftovers),
            )
        )
        REPORT.append(cleanup)


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
                reference=reference,
            )
            completed.append((mountpoint.resolve(), run))
        if convergence and len(mountpoints) > 1:
            CONSOLE.fact("converge", "waiting for every secondary view to show the primary's bytes")
            source = completed[0][1]
            relative = source.root.relative_to(completed[0][0]) / "positioned.bin"
            for mountpoint in mountpoints[1:]:
                wait_for_copy(mountpoint.resolve(), relative, source.digest, timeout)
    finally:
        for _, run in completed:
            shutil.rmtree(run.root, ignore_errors=True)


def finish(args, journal: JournalWatch, seconds: float) -> int:
    results = [(run, result) for run in REPORT for result in run.results]
    failures = [(run, result) for run, result in results if result.status in FAILED]
    known = [(run, result) for run, result in results if result.status == "known"]
    slow = report_timings(args.budget)
    journal_errors = journal.errors() if args.journal_check else []

    CONSOLE.heading("Summary")
    print_summary_table()
    write_reports(args.report_json, args.report_junit)

    slowest = sorted(
        ((run, result) for run, result in results if result.seconds >= 1.0),
        key=lambda item: item[1].seconds,
        reverse=True,
    )[:5]
    if slowest:
        CONSOLE.heading("Slowest cases")
        for run, result in slowest:
            print(f"  {duration(result.seconds):>7}  {result.name}  {CONSOLE.paint(run.label, DIM)}")
    if slow:
        CONSOLE.heading(f"Over the {args.budget:g}s budget ({len(slow)})")
        for line in slow:
            print(f"  - {line}")
    if known:
        CONSOLE.heading(f"Known issues that still reproduce ({len(known)})")
        for run, result in known:
            print(f"  - {result.name}  {CONSOLE.paint(run.label, DIM)}")
            print(indented(result.message, 6))
    if journal_errors:
        CONSOLE.heading(f"The daemon logged {plural(len(journal_errors), 'problem')} during the run")
        for line in journal_errors[:20]:
            print(f"  ! {line}")
        if len(journal_errors) > 20:
            print(f"  … and {len(journal_errors) - 20} more")
    if failures:
        CONSOLE.heading(f"Failures ({len(failures)})")
        for number, (run, result) in enumerate(failures, 1):
            word = CONSOLE.paint(STATUS_WORDS[result.status].ljust(8), RED)
            print(f"  {number}) {result.name}")
            print(f"     target   {run.label}")
            print(f"     {word} {indented(result.message, 14).lstrip()}")
            reference = BUG_REFERENCE.search(result.name)
            if reference:
                print(f"     see      docs/BUGS.md B{reference[1]}")

    totals = dict.fromkeys(("pass", "fail", "skip", "known"), 0)
    for run in REPORT:
        for status, count in run.counts().items():
            totals[status] += count
    print()
    if failures or journal_errors:
        verdict = CONSOLE.paint("FAIL:", RED)
        if journal_errors and not failures:
            verdict += f" the daemon logged {plural(len(journal_errors), 'problem')};"
        print(f"{verdict} {tally(totals, seconds)}")
        return 1
    print(f"{CONSOLE.paint('PASS:', GREEN)} {tally(totals, seconds)}")
    return 0


def print_summary_table() -> None:
    columns = ("pass", "fail", "skip", "known")
    styles = {"fail": RED, "skip": YELLOW, "known": MAGENTA}
    width = max([len(run.label) for run in REPORT] + [len("total")])
    print(f"  {'target':<{width}}  " + "".join(f"{name:>7}" for name in columns) + f"{'time':>9}")
    totals = dict.fromkeys(columns, 0)
    for run in REPORT:
        counts = run.counts()
        cells = ""
        for name in columns:
            totals[name] += counts[name]
            cell = f"{counts[name]:>7}"
            cells += CONSOLE.paint(cell, styles[name]) if counts[name] and name in styles else cell
        print(f"  {run.label:<{width}}  {cells}{duration(run.seconds):>9}")
    if len(REPORT) > 1:
        cells = "".join(f"{totals[name]:>7}" for name in columns)
        total_time = sum(run.seconds for run in REPORT)
        print(CONSOLE.paint(f"  {'total':<{width}}  {cells}{duration(total_time):>9}", BOLD))


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
