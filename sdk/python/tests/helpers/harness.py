from __future__ import annotations

import os
import queue
import re
import shutil
import subprocess
import tempfile
import threading
import time
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(os.environ.get("OPENYAK_REPO_ROOT", Path(__file__).resolve().parents[4]))
RUST_ROOT = REPO_ROOT / "rust"


@dataclass(slots=True)
class ManagedProcess:
    process: subprocess.Popen[str]
    _stderr_chunks: list[str]

    def close(self) -> None:
        if self.process.poll() is not None:
            return
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)

    @property
    def stderr_text(self) -> str:
        return "".join(self._stderr_chunks)


@dataclass(slots=True)
class MockAnthropicHarness:
    base_url: str
    managed: ManagedProcess

    def close(self) -> None:
        self.managed.close()


@dataclass(slots=True)
class OpenyakServerHarness:
    base_url: str
    workspace: Path
    managed: ManagedProcess
    cleanup_workspace: bool = True

    def close(self) -> None:
        try:
            self.managed.close()
        finally:
            if self.cleanup_workspace:
                shutil.rmtree(self.workspace, ignore_errors=True)


@dataclass(slots=True)
class ServerHarness:
    mock: MockAnthropicHarness
    server: OpenyakServerHarness

    def close(self) -> None:
        try:
            self.server.close()
        finally:
            self.mock.close()


@contextmanager
def server_harness() -> Iterator[ServerHarness]:
    mock = start_mock_anthropic_service()
    server = start_openyak_server(
        {
            **os.environ,
            "ANTHROPIC_API_KEY": "test-sdk-key",
            "ANTHROPIC_BASE_URL": mock.base_url,
        }
    )
    harness = ServerHarness(mock=mock, server=server)
    try:
        yield harness
    finally:
        harness.close()


def start_mock_anthropic_service() -> MockAnthropicHarness:
    managed, match = _start_process(
        _resolve_standalone_rust_binary(
            "mock-anthropic-service",
            "MOCK_ANTHROPIC_SERVICE_BIN",
        ),
        {},
        re.compile(r"^MOCK_ANTHROPIC_BASE_URL=(.+)$"),
    )
    return MockAnthropicHarness(base_url=match.group(1), managed=managed)


def start_openyak_server(env: dict[str, str]) -> OpenyakServerHarness:
    workspace = Path(tempfile.mkdtemp(prefix="openyak-python-sdk-alpha-"))
    return start_openyak_server_in(workspace, env, cleanup_workspace=True)


def start_openyak_server_in(
    workspace: Path,
    env: dict[str, str],
    *,
    bind: str = "127.0.0.1:0",
    cleanup_workspace: bool = False,
) -> OpenyakServerHarness:
    managed, match = _start_process(
        _resolve_openyak_server_command(),
        {
            "cwd": workspace,
            "env": env,
            "bind": bind,
        },
        re.compile(r"^Local thread server listening on (http://.+)$"),
    )
    return OpenyakServerHarness(
        base_url=match.group(1),
        workspace=workspace,
        managed=managed,
        cleanup_workspace=cleanup_workspace,
    )


def _resolve_standalone_rust_binary(name: str, env_var: str) -> list[str]:
    override = os.environ.get(env_var)
    if override:
        return [override]

    binary_name = f"{name}.exe" if os.name == "nt" else name
    built_binary = RUST_ROOT / "target" / "debug" / binary_name
    if built_binary.exists():
        return [str(built_binary)]

    cargo = os.environ.get("CARGO", "cargo")
    return [
        cargo,
        "run",
        "--manifest-path",
        str(RUST_ROOT / "Cargo.toml"),
        "--quiet",
        "--bin",
        name,
        "--",
    ]


def _resolve_openyak_server_command() -> list[str]:
    override = os.environ.get("OPENYAK_SERVER_BIN")
    if override:
        return [override, "server"]

    binary_name = "openyak.exe" if os.name == "nt" else "openyak"
    built_binary = RUST_ROOT / "target" / "debug" / binary_name
    if built_binary.exists():
        return [str(built_binary), "server"]

    cargo = os.environ.get("CARGO", "cargo")
    return [
        cargo,
        "run",
        "--manifest-path",
        str(RUST_ROOT / "Cargo.toml"),
        "--quiet",
        "--bin",
        "openyak",
        "--",
        "server",
    ]


def _start_process(
    command: list[str],
    options: dict[str, object],
    matcher: re.Pattern[str],
) -> tuple[ManagedProcess, re.Match[str]]:
    process = subprocess.Popen(
        command + ["--bind", str(options.get("bind", "127.0.0.1:0"))],
        cwd=str(options.get("cwd", REPO_ROOT)),
        env=options.get("env", os.environ),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    stdout_queue: queue.Queue[str] = queue.Queue()
    stderr_chunks: list[str] = []
    threading.Thread(
        target=_pump_stdout,
        args=(process, stdout_queue),
        daemon=True,
    ).start()
    threading.Thread(
        target=_pump_stderr,
        args=(process, stderr_chunks),
        daemon=True,
    ).start()

    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                "process exited before startup line "
                f"(code={process.returncode}): {''.join(stderr_chunks)}"
            )
        try:
            line = stdout_queue.get(timeout=0.1)
        except queue.Empty:
            continue
        match = matcher.match(line)
        if match:
            return ManagedProcess(process=process, _stderr_chunks=stderr_chunks), match

    process.terminate()
    raise RuntimeError(
        f"process did not emit startup line within 90s: {''.join(stderr_chunks)}"
    )


def _pump_stdout(process: subprocess.Popen[str], sink: queue.Queue[str]) -> None:
    assert process.stdout is not None
    for line in process.stdout:
        sink.put(line.rstrip("\r\n"))


def _pump_stderr(process: subprocess.Popen[str], sink: list[str]) -> None:
    assert process.stderr is not None
    for line in process.stderr:
        sink.append(line)
