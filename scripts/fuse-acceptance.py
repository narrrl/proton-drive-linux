#!/usr/bin/env python3
"""Account-free filesystem API contract plus opt-in live FUSE acceptance."""

from __future__ import annotations

import argparse
import concurrent.futures
import errno
import hashlib
import mmap
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import time
import uuid


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


def test_create_flags(root: Path) -> None:
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
    fd = os.open(path, os.O_WRONLY | os.O_TRUNC)
    os.close(fd)
    check(path.stat().st_size == 0, "O_TRUNC did not truncate")


def test_positioned_and_vectored_io(root: Path) -> None:
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
    print(f"    sha256 {hashlib.sha256(got).hexdigest()}")


def test_resize_and_sparse_io(root: Path) -> None:
    path = root / "resize"
    write_durable(path, b"0123456789")
    os.truncate(path, 4)
    check(read(path) == b"0123", "shrink did not preserve prefix")
    os.truncate(path, 8193)
    data = read(path)
    check(data[:4] == b"0123" and data[4:] == bytes(8189), "grown range is not zero-filled")

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


def test_mmap_and_copy_paths(root: Path) -> None:
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


def test_namespace_and_errors(root: Path) -> None:
    a, b = root / "dir-a", root / "dir-b"
    a.mkdir(); b.mkdir()
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

    expect_errno({errno.ENOTEMPTY, errno.EEXIST}, lambda: os.rmdir(a), "rmdir(non-empty)")
    expect_errno({errno.ENOTDIR}, lambda: os.rmdir(b / "moved"), "rmdir(file)")
    expect_errno({errno.EISDIR, errno.EPERM}, lambda: os.unlink(a), "unlink(directory)")
    expect_errno({errno.EINVAL}, lambda: os.rename(a, a / "nested" / "cycle"), "directory cycle")
    expect_errno({errno.ENOENT}, lambda: os.unlink(root / "absent"), "unlink(absent)")
    expect_errno({errno.ENOENT}, lambda: os.rename(root / "absent", root / "new"), "rename(absent)")

    empty_src, empty_dst = root / "empty-src", root / "empty-dst"
    empty_src.mkdir(); empty_dst.mkdir()
    os.replace(empty_src, empty_dst)
    check(empty_dst.is_dir() and not empty_src.exists(), "empty directory replacement failed")
    nonempty = root / "nonempty-dst"
    nonempty.mkdir(); write_durable(nonempty / "keep", b"keep")
    replacement = root / "replacement-dir"; replacement.mkdir()
    expect_errno({errno.ENOTEMPTY, errno.EEXIST}, lambda: os.replace(replacement, nonempty), "replace non-empty dir")
    check(read(nonempty / "keep") == b"keep", "failed replacement damaged destination")


def test_open_lifetime(root: Path) -> None:
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
    os.fsync(fd); os.close(fd)
    check(not old.exists() and read(new) == b"after!", "open handle did not follow rename")


def test_names_and_enumeration(root: Path) -> None:
    names = ["space name.txt", "unicodé-文件", ".hidden", "trailing.dot.", "case", "CASE"]
    for index, name in enumerate(names):
        write_durable(root / name, f"name-{index}".encode())
    listed = set(os.listdir(root))
    check(set(names) <= listed, "directory enumeration omitted valid names")
    for name in listed:
        os.lstat(root / name)
    expect_errno({errno.ENAMETOOLONG}, lambda: os.open(root / ("x" * 256), os.O_CREAT), "overlong name")


def test_concurrency(root: Path) -> None:
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


TESTS = [
    ("creation and open flags", test_create_flags),
    ("positioned and vectored I/O", test_positioned_and_vectored_io),
    ("truncate, growth, sparse ranges, and EOF", test_resize_and_sparse_io),
    ("mmap and zero-copy copy paths", test_mmap_and_copy_paths),
    ("namespace operations and POSIX errors", test_namespace_and_errors),
    ("open-handle lifetime across unlink and rename", test_open_lifetime),
    ("names, lookup, stat, and enumeration", test_names_and_enumeration),
    ("independent and shared-file concurrency", test_concurrency),
]


def run_contract(parent: Path, label: str) -> tuple[Path, str]:
    root = parent / f"pdfs-acceptance-{uuid.uuid4().hex}"
    root.mkdir()
    print(f"[target] {label}: {parent}")
    try:
        selected = os.environ.get("PDFS_ACCEPTANCE_ONLY")
        tests = TESTS if not selected else [test for test in TESTS if selected.lower() in test[0].lower()]
        check(bool(tests), f"PDFS_ACCEPTANCE_ONLY={selected!r} matched no tests")
        for name, operation in tests:
            print(f"  [test] {name}")
            operation(root)
        digest_path = root / "positioned.bin"
        digest = hashlib.sha256(read(digest_path)).hexdigest() if digest_path.exists() else ""
        return root, digest
    except BaseException:
        shutil.rmtree(root, ignore_errors=True)
        raise


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
        import json
        value = json.loads(self.command("sync", "list", json_output=True))
        return value["items"]

    def wait_for_queue(self) -> None:
        import json
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


def is_mountpoint(path: Path) -> bool:
    """True for a distinct mount, including FUSE mounts over an existing dir."""
    return os.path.ismount(path)


def run_managed_matrix(paths: list[Path]) -> None:
    timeout = int(os.environ.get("PDFS_ACCEPTANCE_SYNC_TIMEOUT", "180"))
    pair = ManagedSyncPair(paths, timeout)
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
                root, _ = run_contract(path, f"managed {mode}")
                roots.append(root)
                pair.wait_for_queue()
                pair.remove_tree(root)
                roots.remove(root)
                pair.wait_for_queue()
                # Mirror changes are asynchronous; force and await a pass before
                # changing its mode so authored bytes/deletions cannot be lost.
                if mode == "mirror":
                    pair.force_sync(path)
    finally:
        for root in roots:
            shutil.rmtree(root, ignore_errors=True)
        pair.cleanup()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    live_mode = parser.add_mutually_exclusive_group()
    live_mode.add_argument("--live", nargs="+", type=Path, metavar="MOUNTPOINT")
    live_mode.add_argument(
        "--managed-live",
        nargs=2,
        type=Path,
        metavar=("EMPTY_DIR_A", "EMPTY_DIR_B"),
    )
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="pdfs-api-reference-") as directory:
        reference, _ = run_contract(Path(directory), "local filesystem API reference (no account)")
        shutil.rmtree(reference)
    print("[pass] account-free filesystem API contract")

    if args.managed_live:
        run_managed_matrix(args.managed_live)
        print("PASS: offline API and managed live mode-matrix suites completed")
        return

    if not args.live:
        print("PASS: offline acceptance suite completed; live FUSE test was not requested")
        return

    completed: list[tuple[Path, Path, str]] = []
    convergence = os.environ.get("PDFS_ACCEPTANCE_CONVERGENCE", "0") == "1"
    try:
        # Views of one remote folder must not independently create the same
        # fixtures. Exercise the primary, then observe that exact tree through
        # every secondary. Without convergence mode, each mount is independent
        # and receives the full contract.
        targets = args.live[:1] if convergence and len(args.live) > 1 else args.live
        for mountpoint in targets:
            root, digest = run_contract(mountpoint.resolve(), "live FUSE")
            completed.append((mountpoint.resolve(), root, digest))
        if convergence and len(args.live) > 1:
            print("  [test] cross-mount remote convergence")
            timeout = int(os.environ.get("PDFS_ACCEPTANCE_SYNC_TIMEOUT", "120"))
            source_root, digest = completed[0][1], completed[0][2]
            relative = source_root.relative_to(completed[0][0]) / "positioned.bin"
            for mountpoint in args.live[1:]:
                wait_for_copy(mountpoint.resolve(), relative, digest, timeout)
    finally:
        for _, root, _ in completed:
            shutil.rmtree(root, ignore_errors=True)
    print("PASS: offline API and live FUSE acceptance suites completed")


if __name__ == "__main__":
    main()
