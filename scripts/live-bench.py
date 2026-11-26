#!/usr/bin/env python3
"""Time one edit in a synced folder, from the write to the new revision on Drive.

Runs against a real account on a separate instance (`PDFS_INSTANCE`), so the
user's own daemon and session are never touched. For each tree size it:

1. makes a disposable folder in My files and a local tree of that many files,
2. attaches the local tree to it and waits for the first sync to finish,
3. edits one file and measures:
   - debounce: the write until the sync pass starts,
   - the pass, split into its stages and calls to Drive (`pdfs diagnostics`),
   - verify: the pass end until `pdfs versions list` shows the new revision,
4. detaches the folder and trashes the remote copy.

The account comes from an env file with `username=`, `password=` and, for an
account with two-factor sign-in, `PROTON_TOTP_SECRET=`. It is read here and
typed into `pdfs login` over a pseudo-terminal; it is never printed or written
anywhere. The session stays in the instance's keyring entry for the next run,
so repeated runs do not sign in again (Proton limits sign-ins); pass
`--logout` to forget it.

    scripts/live-bench.py --account-env ../proton-sdk-rs/.env --pdfs target/debug/pdfs

The daemon mounts at `~/ProtonDrive-<instance>` and logs to the work directory.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import hashlib
import hmac
import json
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

POLL = 0.1


def read_account(path: Path) -> dict[str, str]:
    values = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.strip()
    for key in ("username", "password"):
        if not values.get(key):
            raise SystemExit(f"{path} has no {key}=")
    return values


def totp(secret: str, at: float | None = None) -> str:
    """RFC 6238: SHA-1, 30-second steps, six digits."""
    key = base64.b32decode(secret.upper() + "=" * (-len(secret) % 8))
    counter = int((at if at is not None else time.time()) // 30)
    digest = hmac.new(key, struct.pack(">Q", counter), hashlib.sha1).digest()
    offset = digest[-1] & 0x0F
    code = struct.unpack(">I", digest[offset : offset + 4])[0] & 0x7FFFFFFF
    return f"{code % 1_000_000:06d}"


class Pdfs:
    def __init__(self, binary: str, instance: str) -> None:
        self.binary = binary
        self.env = dict(os.environ, PDFS_INSTANCE=instance)

    def run(self, *args: str, check: bool = True, timeout: float = 120) -> str:
        result = subprocess.run(
            [self.binary, *args],
            env=self.env,
            text=True,
            capture_output=True,
            stdin=subprocess.DEVNULL,
            timeout=timeout,
        )
        if check and result.returncode:
            detail = result.stderr.strip() or result.stdout.strip()
            raise RuntimeError(f"pdfs {' '.join(args)} failed ({result.returncode}): {detail}")
        return result.stdout

    def json(self, *args: str) -> object:
        return json.loads(self.run("--json", *args))

    def logged_in(self) -> bool:
        return "Not logged in" not in self.run("status", check=False)

    def login(self, account: dict[str, str]) -> None:
        """Answer `pdfs login`'s prompts over a pseudo-terminal."""
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(self.binary, [self.binary, "login", "--username", account["username"]], self.env)
        transcript = b""
        answered: set[bytes] = set()
        deadline = time.monotonic() + 180
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([fd], [], [], 1)
                if not ready:
                    continue
                try:
                    chunk = os.read(fd, 4096)
                except OSError:
                    break
                if not chunk:
                    break
                transcript += chunk
                tail = transcript[-200:].lower()
                if b"password:" in tail and b"password" not in answered and b"mailbox" not in tail:
                    answered.add(b"password")
                    os.write(fd, account["password"].encode() + b"\n")
                elif b"2fa code:" in tail and b"totp" not in answered:
                    secret = account.get("PROTON_TOTP_SECRET")
                    if not secret:
                        raise RuntimeError("the account asks for a 2FA code; add PROTON_TOTP_SECRET")
                    answered.add(b"totp")
                    os.write(fd, totp(secret).encode() + b"\n")
                elif b"mailbox" in tail:
                    raise RuntimeError("two-password accounts are not supported by this benchmark")
                elif b"captcha" in tail:
                    raise RuntimeError("Proton asks for a CAPTCHA; sign in once by hand with PDFS_INSTANCE set")
        finally:
            with contextlib.suppress(OSError):
                os.close(fd)
            _, status = os.waitpid(pid, 0)
        if os.waitstatus_to_exitcode(status) != 0:
            # The transcript holds only prompts and pdfs's own messages: the
            # password and the code are typed with echo off.
            last = transcript.decode(errors="replace").strip().splitlines()[-1:]
            raise RuntimeError(f"pdfs login failed: {' '.join(last)}")


def sync_state(pdfs: Pdfs) -> dict:
    value = pdfs.json("diagnostics")

    def find(node: object) -> dict | None:
        if isinstance(node, dict):
            if "traces" in node and "pass_folder" in node:
                return node
            for child in node.values():
                found = find(child)
                if found is not None:
                    return found
        return None

    return find(value) or {"traces": [], "pass_folder": None}


def trace_for(state: dict, folder_id: int) -> dict | None:
    for trace in state.get("traces", []):
        if trace["folder_id"] == folder_id:
            return trace
    return None


def wait(what: str, predicate, timeout: float):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(POLL)
    raise TimeoutError(f"timed out after {timeout:.0f}s waiting for {what}")


def make_tree(root: Path, files: int, per_dir: int = 50) -> list[Path]:
    paths = []
    for index in range(files):
        directory = root / f"d{index // per_dir:04d}"
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / f"f{index:06d}.txt"
        path.write_bytes(f"file {index}\n".encode() * 64)
        paths.append(path)
    return paths


def revisions(pdfs: Pdfs, path: str) -> int:
    value = pdfs.json("versions", "list", path)
    items = value.get("items", value) if isinstance(value, dict) else value
    return len(items)


def wait_quiet(pdfs: Pdfs, folder_id: int, quiet: float, timeout: float) -> None:
    """Wait until no pass has run over the folder for `quiet` seconds."""
    deadline = time.monotonic() + timeout
    last = trace_for(sync_state(pdfs), folder_id)
    since = time.monotonic()
    while time.monotonic() < deadline:
        state = sync_state(pdfs)
        trace = trace_for(state, folder_id)
        if state.get("pass_folder") == folder_id or trace != last:
            last = trace
            since = time.monotonic()
        elif time.monotonic() - since >= quiet:
            return
        time.sleep(POLL)
    raise TimeoutError(f"the folder did not go quiet within {timeout:.0f}s")


def bench_one(
    pdfs: Pdfs, mount: Path, work: Path, files: int, timeout: float, quiet: float
) -> dict:
    name = f"pdfs-bench-{files}-{uuid.uuid4().hex[:8]}"
    local = work / name
    local.mkdir()
    paths = make_tree(local, files)
    pdfs.run("mkdir", str(mount), name)
    folder_id = None
    try:
        # A removed folder's id is reused, and its last trace with it.
        stale = {json.dumps(t, sort_keys=True) for t in sync_state(pdfs).get("traces", [])}
        started = time.monotonic()
        pdfs.run("sync", "attach", "--to", name, str(local))
        folder = next(
            item
            for item in pdfs.json("sync", "list")["items"]
            if Path(item["local_path"]).resolve() == local.resolve()
        )
        folder_id = folder["id"]
        pdfs.run("sync", "flush", timeout=timeout)
        wait(
            "the first pass",
            lambda: (t := trace_for(sync_state(pdfs), folder_id))
            and t["ended"] == "finished"
            and json.dumps(t, sort_keys=True) not in stale,
            timeout,
        )
        first_pass_s = time.monotonic() - started

        # The first pass's own uploads come back as Drive events and start
        # another pass. An edit made while it runs would be timed against it.
        wait_quiet(pdfs, folder_id, quiet, timeout)

        target = paths[len(paths) // 2]
        remote_path = f"{name}/{target.relative_to(local)}"
        before = revisions(pdfs, remote_path)
        previous = trace_for(sync_state(pdfs), folder_id)

        edited = time.monotonic()
        with target.open("ab") as file:
            file.write(b"edited\n")
            file.flush()
            os.fsync(file.fileno())

        def edit_pass():
            """The ended pass that uploaded the edit. A pass that ended without
            uploading (a watcher event that came early) is skipped."""
            nonlocal previous
            state = sync_state(pdfs)
            trace = trace_for(state, folder_id)
            if state.get("pass_folder") == folder_id or not trace or trace == previous:
                return None
            if not trace["ended"]:
                return None
            previous = trace
            if any(call["name"] == "upload revision" for call in trace.get("calls", [])):
                return trace
            return None

        trace = wait("the pass that uploads the edit", edit_pass, timeout)
        pass_end = time.monotonic()
        pass_start = pass_end - trace["elapsed_ms"] / 1000
        wait("the new revision on Drive", lambda: revisions(pdfs, remote_path) > before, timeout)
        verified = time.monotonic()
        return {
            "files": files,
            "first_pass_s": round(first_pass_s, 2),
            "debounce_ms": round((pass_start - edited) * 1000),
            "pass_ms": round((pass_end - pass_start) * 1000),
            "verify_ms": round((verified - pass_end) * 1000),
            "total_ms": round((verified - edited) * 1000),
            "trace": trace,
        }
    finally:
        if folder_id is not None:
            # A removal waits only briefly for a running pass to reach its next
            # checkpoint, and a pass mid-upload can take longer: retry, or the
            # synced folder outlives its tree and every later run trips on it.
            for _ in range(20):
                try:
                    pdfs.run("sync", "rm", str(folder_id))
                    break
                except Exception as error:  # noqa: BLE001
                    print(f"  sync rm: {error}; retrying", file=sys.stderr)
                    time.sleep(6)
        with contextlib.suppress(Exception):
            pdfs.run("rm", f"{mount}/{name}")
        shutil.rmtree(local, ignore_errors=True)


def refuse_without_instances(binary: str) -> None:
    """Stop unless `binary` honours PDFS_INSTANCE. One built before it existed
    ignores the variable and would sign in, attach and delete on the default
    instance: the user's own account."""
    result = subprocess.run(
        [binary, "status"],
        env=dict(os.environ, PDFS_INSTANCE="Not A Valid Name"),
        text=True,
        capture_output=True,
        stdin=subprocess.DEVNULL,
        timeout=30,
    )
    if result.returncode == 0 or "PDFS_INSTANCE" not in result.stderr:
        raise SystemExit(f"{binary} does not support PDFS_INSTANCE; refusing to run")


def remove_leftovers(pdfs: Pdfs, mount: Path) -> None:
    """Remove what an earlier run cut short left behind: its synced folders,
    and their folders on Drive."""
    for item in pdfs.json("sync", "list")["items"]:
        name = Path(item["local_path"]).name
        if not name.startswith("pdfs-bench-"):
            continue
        print(f"removing {name}, left by an earlier run", file=sys.stderr)
        with contextlib.suppress(Exception):
            pdfs.run("sync", "rm", str(item["id"]))
        with contextlib.suppress(Exception):
            pdfs.run("rm", f"{mount}/{name}")


def print_result(result: dict) -> None:
    print(
        f"{result['files']:>6} files: edit to verified {result['total_ms']} ms "
        f"(debounce {result['debounce_ms']}, pass {result['pass_ms']}, verify {result['verify_ms']}); "
        f"first sync {result['first_pass_s']} s"
    )
    trace = result["trace"]
    for kind in ("phases", "calls"):
        line = ", ".join(f"{s['name']} {s['count']}x/{s['total_ms']}ms" for s in trace.get(kind, []))
        if line:
            print(f"         {kind}: {line}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--account-env", type=Path, required=True, help="env file with the test account")
    parser.add_argument("--pdfs", default="pdfs", help="the pdfs CLI to run")
    parser.add_argument("--instance", default="bench", help="PDFS_INSTANCE to run under")
    parser.add_argument("--sizes", default="20,500", help="tree sizes, comma-separated")
    parser.add_argument("--timeout", type=float, default=3600, help="seconds per wait")
    parser.add_argument(
        "--quiet", type=float, default=15, help="seconds without a pass before the edit"
    )
    parser.add_argument("--work", type=Path, help="where the local trees and the daemon log go")
    parser.add_argument("--json", type=Path, help="also write the results here")
    parser.add_argument("--logout", action="store_true", help="forget the session afterwards")
    args = parser.parse_args()
    if args.instance in ("", "default"):
        raise SystemExit("refusing to run on the default instance")

    pdfs = Pdfs(args.pdfs, args.instance)
    refuse_without_instances(args.pdfs)
    if not pdfs.logged_in():
        pdfs.login(read_account(args.account_env))

    work = args.work or Path(tempfile.mkdtemp(prefix="pdfs-bench-"))
    work.mkdir(parents=True, exist_ok=True)
    log = (work / "daemon.log").open("w")
    daemon = subprocess.Popen(
        [args.pdfs, "daemon"], env=pdfs.env, stdin=subprocess.DEVNULL, stdout=log, stderr=log
    )
    results = []
    try:
        status = wait(
            "the daemon to mount",
            lambda: (s := pdfs.json("status")) and s.get("mount") and s,
            args.timeout,
        )
        mount = Path(status["mount"].get("mountpoint") or Path.home() / f"ProtonDrive-{args.instance}")
        remove_leftovers(pdfs, mount)
        for size in (int(s) for s in args.sizes.split(",")):
            result = bench_one(pdfs, mount, work, size, args.timeout, args.quiet)
            print_result(result)
            results.append(result)
            if args.json:
                args.json.write_text(json.dumps(results, indent=2))
    finally:
        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(timeout=60)
        except subprocess.TimeoutExpired:
            daemon.kill()
        log.close()
        if args.logout:
            pdfs.run("logout", check=False)
    print(f"daemon log: {work / 'daemon.log'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
