#!/usr/bin/env python3
"""Exercise remote-target failure paths against observable local traps.

The Docker client normally has an empty BitRouter home.  That proves the
network boundary, but it cannot prove that a remote failure did not silently
fall back to a tempting local router.  This script supplies such a router and
uses Linux inotify plus Unix-socket traps to make any local config, metering,
or control-socket access observable.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import os
from pathlib import Path
import select
import shutil
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time


CONFIG_SENTINEL = "TARGET_ISOLATION_LOCAL_CONFIG_SENTINEL"
METERING_SENTINEL = "TARGET_ISOLATION_LOCAL_METERING_SENTINEL"
PROVIDER_SENTINEL = "TARGET_ISOLATION_LOCAL_PROVIDER_SECRET"
WRONG_TOKEN = "target-isolation-wrong-token-000000000000000000000000"
COMMAND_TIMEOUT_SECONDS = 20.0

IN_ACCESS = 0x00000001
IN_MODIFY = 0x00000002
IN_ATTRIB = 0x00000004
IN_CLOSE_WRITE = 0x00000008
IN_CLOSE_NOWRITE = 0x00000010
IN_OPEN = 0x00000020
IN_MOVED_FROM = 0x00000040
IN_MOVED_TO = 0x00000080
IN_CREATE = 0x00000100
IN_DELETE = 0x00000200
IN_DELETE_SELF = 0x00000400
IN_MOVE_SELF = 0x00000800
IN_IGNORED = 0x00008000

FILE_WATCH_MASK = (
    IN_ACCESS
    | IN_MODIFY
    | IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_CLOSE_NOWRITE
    | IN_OPEN
)
DIRECTORY_WATCH_MASK = (
    IN_MODIFY
    | IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF
)
EVENT_HEADER = struct.Struct("iIII")


class IsolationFailure(RuntimeError):
    """A remote command observed or disclosed a local trap."""


class InotifyMonitor:
    """Nonblocking Linux inotify watcher for only the poisoned local inputs."""

    def __init__(self, paths: list[tuple[str, Path, int]]) -> None:
        if sys.platform != "linux":
            raise IsolationFailure("target-isolation requires Linux inotify")
        libc = ctypes.CDLL(None, use_errno=True)
        try:
            init = libc.inotify_init1
            add_watch = libc.inotify_add_watch
        except AttributeError as error:
            raise IsolationFailure("target-isolation requires inotify_init1") from error
        init.argtypes = [ctypes.c_int]
        init.restype = ctypes.c_int
        add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
        add_watch.restype = ctypes.c_int

        fd = init(os.O_NONBLOCK | os.O_CLOEXEC)
        if fd < 0:
            raise OSError(ctypes.get_errno(), "inotify_init1")
        self._fd = fd
        self._labels: dict[int, str] = {}
        self._add_watch = add_watch
        try:
            for label, path, mask in paths:
                watch_descriptor = self._add_watch(
                    self._fd,
                    os.fsencode(path),
                    mask,
                )
                if watch_descriptor < 0:
                    raise OSError(
                        ctypes.get_errno(),
                        f"inotify_add_watch {label}",
                    )
                self._labels[watch_descriptor] = label
        except BaseException:
            os.close(self._fd)
            raise

    @property
    def fd(self) -> int:
        return self._fd

    def drain(self) -> list[str]:
        events: list[str] = []
        while True:
            try:
                data = os.read(self._fd, 64 * 1024)
            except BlockingIOError:
                break
            if not data:
                break
            offset = 0
            while offset < len(data):
                watch_descriptor, mask, _, name_size = EVENT_HEADER.unpack_from(data, offset)
                offset += EVENT_HEADER.size
                name_end = offset + name_size
                name = data[offset:name_end].rstrip(b"\0").decode("utf-8", "replace")
                offset = name_end
                if mask & IN_IGNORED:
                    continue
                label = self._labels.get(watch_descriptor, "unknown")
                suffix = f"/{name}" if name else ""
                events.append(f"{label}{suffix}:{event_names(mask)}")
        return events

    def close(self) -> None:
        if self._fd >= 0:
            os.close(self._fd)
            self._fd = -1


def event_names(mask: int) -> str:
    names = [
        (IN_ACCESS, "access"),
        (IN_OPEN, "open"),
        (IN_MODIFY, "modify"),
        (IN_ATTRIB, "attrib"),
        (IN_CLOSE_WRITE, "close_write"),
        (IN_CLOSE_NOWRITE, "close_nowrite"),
        (IN_CREATE, "create"),
        (IN_DELETE, "delete"),
        (IN_MOVED_FROM, "moved_from"),
        (IN_MOVED_TO, "moved_to"),
    ]
    rendered = [name for flag, name in names if mask & flag]
    return ",".join(rendered) if rendered else f"0x{mask:x}"


class UnixSocketTrap:
    """Records a local daemon control connection without polling or sleeping."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(os.fspath(path))
        self._listener.listen()
        self._listener.setblocking(False)
        self._wake_reader, self._wake_writer = socket.socketpair()
        self._stopped = threading.Event()
        self._lock = threading.Lock()
        self._hits = 0
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    @property
    def hits(self) -> int:
        with self._lock:
            return self._hits

    def _serve(self) -> None:
        while not self._stopped.is_set():
            readable, _, _ = select.select([self._listener, self._wake_reader], [], [])
            if self._wake_reader in readable:
                self._wake_reader.recv(1)
                return
            if self._listener not in readable:
                continue
            while True:
                try:
                    connection, _ = self._listener.accept()
                except BlockingIOError:
                    break
                except OSError:
                    return
                with self._lock:
                    self._hits += 1
                connection.close()

    def close(self) -> None:
        if self._stopped.is_set():
            return
        self._stopped.set()
        try:
            self._wake_writer.send(b"x")
        except OSError:
            pass
        self._thread.join(timeout=2)
        self._listener.close()
        self._wake_reader.close()
        self._wake_writer.close()
        try:
            self.path.unlink()
        except FileNotFoundError:
            pass


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="bitrouter")
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--ca", type=Path)
    return parser.parse_args()


def require_file(path: Path | None) -> None:
    if path is not None and not path.is_file():
        raise IsolationFailure("the supplied control CA is unavailable")


def require_token() -> str:
    token = os.environ.get("CLIENT_READ_TOKEN")
    if token is None or len(token) < 32:
        raise IsolationFailure("CLIENT_READ_TOKEN must provide a reader credential")
    return token


def write_metering_database(path: Path) -> None:
    connection = sqlite3.connect(path)
    try:
        connection.execute("CREATE TABLE local_trap (marker TEXT NOT NULL)")
        connection.execute("INSERT INTO local_trap (marker) VALUES (?)", (METERING_SENTINEL,))
        connection.commit()
    finally:
        connection.close()


def write_local_config(config: Path, socket_path: Path, database_path: Path) -> None:
    config.write_text(
        "\n".join(
            [
                f"# {CONFIG_SENTINEL}",
                "server:",
                '  listen: "127.0.0.1:4356"',
                f'  control_socket: "{socket_path}"',
                "  skip_auth: true",
                "database:",
                f'  url: "sqlite://{database_path}?mode=rwc"',
                "inherit_defaults: false",
                "registry:",
                "  enabled: false",
                "providers:",
                "  local-trap:",
                '    api_base: "http://127.0.0.1:9/v1"',
                '    api_key: "${OPENAI_API_KEY}"',
                "    api_protocol:",
                '      - "*": chat_completions',
                "    models:",
                "      - id: local-trap-model",
                "policy:",
                '  path: "./local-trap-policy.yaml"',
                "  mode: frozen",
                "presets:",
                "  local-trap:",
                "    model: local-trap:local-trap-model",
            ]
        )
        + "\n",
        encoding="utf-8",
    )


def file_signature(path: Path) -> tuple[int, int, int]:
    stat = path.stat()
    return stat.st_ino, stat.st_size, stat.st_mtime_ns


def directory_signature(path: Path) -> tuple[tuple[str, int, int, int], ...]:
    rows: list[tuple[str, int, int, int]] = []
    for candidate in sorted(path.rglob("*")):
        if candidate.is_file():
            stat = candidate.stat()
            rows.append((str(candidate.relative_to(path)), stat.st_ino, stat.st_size, stat.st_mtime_ns))
    return tuple(rows)


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(64 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def directory_digest(path: Path) -> tuple[tuple[str, str], ...]:
    return tuple(
        (str(candidate.relative_to(path)), file_digest(candidate))
        for candidate in sorted(path.rglob("*"))
        if candidate.is_file()
    )


def run_setup(binary: str, environment: dict[str, str], args: list[str], cwd: Path) -> None:
    completed = subprocess.run(
        [binary, *args],
        cwd=cwd,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=COMMAND_TIMEOUT_SECONDS,
        check=False,
    )
    if completed.returncode != 0:
        raise IsolationFailure("could not create an isolated remote context")


def run_remote_failure(
    label: str,
    binary: str,
    args: list[str],
    environment: dict[str, str],
    cwd: Path,
    monitor: InotifyMonitor,
    socket_traps: list[UnixSocketTrap],
    config_signature: tuple[int, int, int],
    metering_signature: tuple[tuple[str, int, int, int], ...],
    required_markers: tuple[str, ...],
) -> None:
    monitor.drain()
    initial_hits = [trap.hits for trap in socket_traps]
    process = subprocess.Popen(
        [binary, *args],
        cwd=cwd,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    deadline = time.monotonic() + COMMAND_TIMEOUT_SECONDS
    events: list[str] = []
    while process.poll() is None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            process.kill()
            process.communicate()
            raise IsolationFailure(f"{label} did not complete within its bounded timeout")
        select.select([monitor.fd], [], [], min(remaining, 0.25))
        events.extend(monitor.drain())
    stdout, stderr = process.communicate()
    events.extend(monitor.drain())
    combined = f"{stdout}\n{stderr}"
    folded = combined.lower()

    if process.returncode == 0:
        raise IsolationFailure(f"{label} unexpectedly succeeded")
    for sentinel in (CONFIG_SENTINEL, METERING_SENTINEL, PROVIDER_SENTINEL):
        if sentinel.lower() in folded:
            raise IsolationFailure(f"{label} disclosed a local sentinel")
    if not any(marker in folded for marker in required_markers):
        raise IsolationFailure(f"{label} did not report its expected remote failure")
    if events:
        raise IsolationFailure(f"{label} accessed a local file trap ({'; '.join(events)})")
    if [trap.hits for trap in socket_traps] != initial_hits:
        raise IsolationFailure(f"{label} contacted a local control-socket trap")
    if config_signature != file_signature(_config_path(environment)):
        raise IsolationFailure(f"{label} changed the local config trap")
    if metering_signature != directory_signature(_metering_directory(environment)):
        raise IsolationFailure(f"{label} changed the local metering trap")


def _config_path(environment: dict[str, str]) -> Path:
    return Path(environment["BITROUTER_HOME"]) / "bitrouter.yaml"


def _metering_directory(environment: dict[str, str]) -> Path:
    return Path(environment["TARGET_ISOLATION_METERING_DIR"])


def reserve_unreachable_loopback_port() -> int:
    reservation = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        reservation.bind(("127.0.0.1", 0))
        return int(reservation.getsockname()[1])
    finally:
        reservation.close()


def build_environment(home: Path, router_home: Path, metering_directory: Path, token: str) -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(
        {
            "HOME": str(home),
            "BITROUTER_HOME": str(router_home),
            "CLIENT_READ_TOKEN": token,
            "ISOLATION_READ_TOKEN": token,
            "ISOLATION_WRONG_TOKEN": WRONG_TOKEN,
            "OPENAI_API_KEY": PROVIDER_SENTINEL,
            "TARGET_ISOLATION_METERING_DIR": str(metering_directory),
        }
    )
    return environment


def main() -> int:
    arguments = parse_arguments()
    require_file(arguments.ca)
    token = require_token()
    binary = shutil.which(arguments.binary) or arguments.binary

    with tempfile.TemporaryDirectory(prefix="bitrouter-target-isolation-") as temporary:
        root = Path(temporary)
        home = root / "home"
        router_home = home / ".bitrouter"
        workdir = root / "workdir"
        metering_directory = root / "metering"
        home.mkdir()
        router_home.mkdir()
        workdir.mkdir()
        metering_directory.mkdir()

        configured_socket = root / "configured-control.sock"
        default_socket = router_home / "bitrouter.sock"
        database = metering_directory / "local-metering.sqlite"
        config = router_home / "bitrouter.yaml"
        write_metering_database(database)
        write_local_config(config, configured_socket, database)
        environment = build_environment(home, router_home, metering_directory, token)
        config_before = file_signature(config)
        metering_before = directory_signature(metering_directory)
        config_digest_before = file_digest(config)
        metering_digest_before = directory_digest(metering_directory)

        configured_trap = UnixSocketTrap(configured_socket)
        default_trap = UnixSocketTrap(default_socket)
        monitor: InotifyMonitor | None = None
        try:
            run_setup(
                binary,
                environment,
                [
                    "context",
                    "add",
                    "wrong-auth",
                    "--endpoint",
                    arguments.endpoint,
                    "--token-env",
                    "ISOLATION_WRONG_TOKEN",
                ],
                workdir,
            )
            unreachable_port = reserve_unreachable_loopback_port()
            run_setup(
                binary,
                environment,
                [
                    "context",
                    "add",
                    "unreachable",
                    "--endpoint",
                    f"http://127.0.0.1:{unreachable_port}/control/v1",
                    "--token-env",
                    "ISOLATION_READ_TOKEN",
                ],
                workdir,
            )

            # Context metadata is deliberately local and expected. Start
            # watching only after those client-local writes complete.
            monitor = InotifyMonitor(
                [
                    ("local-config", config, FILE_WATCH_MASK),
                    ("local-metering", database, FILE_WATCH_MASK),
                    ("local-metering-directory", metering_directory, DIRECTORY_WATCH_MASK),
                ]
            )

            wrong_auth_reads = [
                ("wrong-token-status", ["--context", "wrong-auth", "status"]),
                (
                    "wrong-token-models",
                    ["--context", "wrong-auth", "models", "--provider", "local-trap"],
                ),
                (
                    "wrong-token-route",
                    [
                        "--context",
                        "wrong-auth",
                        "route",
                        "local-trap/local-trap-model",
                    ],
                ),
                (
                    "wrong-token-requests",
                    ["--context", "wrong-auth", "requests", "--limit", "1"],
                ),
                (
                    "wrong-token-providers",
                    ["--context", "wrong-auth", "providers", "list"],
                ),
                (
                    "wrong-token-observe",
                    ["--context", "wrong-auth", "observe", "status"],
                ),
                (
                    "wrong-token-policy-status",
                    ["--context", "wrong-auth", "policy", "status", "--view", "active"],
                ),
                (
                    "wrong-token-policy-show",
                    [
                        "--context",
                        "wrong-auth",
                        "policy",
                        "show",
                        "local-trap",
                        "--view",
                        "active",
                    ],
                ),
                (
                    "wrong-token-agents",
                    ["--context", "wrong-auth", "agents", "list"],
                ),
                (
                    "wrong-token-operation-lookup",
                    [
                        "--context",
                        "wrong-auth",
                        "operations",
                        "show",
                        "00000000-0000-0000-0000-000000000000",
                        "--instance",
                        "00000000-0000-0000-0000-000000000000",
                    ],
                ),
                ("wrong-token-reload", ["--context", "wrong-auth", "reload"]),
            ]
            for label, command in wrong_auth_reads:
                run_remote_failure(
                    label,
                    binary,
                    command,
                    environment,
                    workdir,
                    monitor,
                    [configured_trap, default_trap],
                    config_before,
                    metering_before,
                    ("unauthorized", "401"),
                )

            for label, command in [
                ("unreachable-status", ["--context", "unreachable", "status"]),
                ("unreachable-requests", ["--context", "unreachable", "requests", "--limit", "1"]),
                (
                    "unreachable-providers",
                    ["--context", "unreachable", "providers", "list"],
                ),
            ]:
                run_remote_failure(
                    label,
                    binary,
                    command,
                    environment,
                    workdir,
                    monitor,
                    [configured_trap, default_trap],
                    config_before,
                    metering_before,
                    ("remote control endpoint",),
                )

            run_remote_failure(
                "excluded-remote-agent-registry",
                binary,
                ["--context", "wrong-auth", "agents", "list", "--remote"],
                environment,
                workdir,
                monitor,
                [configured_trap, default_trap],
                config_before,
                metering_before,
                ("cannot run against a remote bitrouter context",),
            )
        finally:
            if monitor is not None:
                monitor.close()
            configured_trap.close()
            default_trap.close()

        if file_digest(config) != config_digest_before:
            raise IsolationFailure("a remote failure changed the local config trap")
        if directory_digest(metering_directory) != metering_digest_before:
            raise IsolationFailure("a remote failure changed the local metering trap")

    print('{"target_isolation":"passed","read_actions":9,"failure_modes":2}')
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (IsolationFailure, OSError, subprocess.SubprocessError) as error:
        print(f"remote target-isolation acceptance failed: {error}", file=sys.stderr)
        raise SystemExit(1) from None
