# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

"""Run the production Native Image sidecar through a bounded stdio handshake.

This is a CI-only smoke gate. It deliberately uses the frozen protocol fixture shape instead of
implementing a second protocol client: the gate only proves that the built executable starts,
publishes the native runtime identity, accepts the ready challenge, and shuts down cleanly.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
from pathlib import Path
from queue import Empty, Queue
import re
import signal
import subprocess
import sys
import threading
import time
from typing import Any


MAX_OUTPUT_BYTES = 4 * 1024 * 1024
READY_TOKEN = "0123456789abcdef0123456789abcdef"
SECRET_NAME_PARTS = (
    "API_KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "BEARER",
    "CREDENTIAL",
)
LEAK_PATTERN = re.compile(r"api[_ -]?key|bearer|sk-[a-z0-9]|github_token", re.IGNORECASE)


def initialize_frame() -> dict[str, Any]:
    """Build the smallest client capability document that exercises the real handshake.

    The production runtime must remain startable before a model profile is activated, so the
    smoke gate advertises only handshake and shutdown methods and does not provide credentials or
    a fake provider. This keeps the build gate independent of paid external APIs.
    """

    return {
        "jsonrpc": "2.0",
        "id": "c:init",
        "method": "initialize",
        "params": {
            "protocolMajor": 1,
            "protocolMinor": 0,
            "minimumCompatibleMinor": 0,
            "clientVersion": "ja-native-ci",
            "capabilities": {
                "methods": ["initialize", "shutdown"],
                "events": ["runtime/statusChanged"],
                "accessModes": ["read_only", "workspace", "full_access"],
                "itemKinds": ["agent_message"],
                "mcp": {"protocolVersions": [], "transports": [], "features": []},
            },
            "limits": {
                "maxFrameBytes": 4194304,
                "maxInboundQueueFrames": 256,
                "maxOutboundQueueFrames": 1024,
                "maxInFlightRequests": 64,
                "maxPendingRequests": 64,
                "maxItemDeltaBytes": 65536,
                "maxInlineToolOutputBytes": 1048576,
                "maxLogBytes": 1048576,
                "defaultRequestDeadlineMs": 120000,
                "defaultApprovalDeadlineMs": 300000,
            },
        },
    }


def initialized_frame() -> dict[str, Any]:
    """Return the challenge response required before the runtime can publish ready."""

    return {
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {"readyToken": READY_TOKEN},
    }


def shutdown_frame() -> dict[str, Any]:
    """Return the normal close request so the smoke observes the graceful path."""

    return {"jsonrpc": "2.0", "id": "c:shutdown", "method": "shutdown", "params": {}}


def sanitized_environment() -> dict[str, str]:
    """Remove credential-like variables and Java discovery variables from the child environment.

    A native executable must not depend on a developer JDK or inherit model credentials. Retaining
    ordinary platform variables preserves Windows loader and macOS process behavior while the
    name-based filter prevents CI secrets from entering the smoke process.
    """

    environment = dict(os.environ)
    for key in list(environment):
        upper = key.upper()
        if key == "JAVA_HOME" or any(part in upper for part in SECRET_NAME_PARTS):
            environment.pop(key, None)
    environment["JA_NATIVE_SMOKE"] = "1"
    return environment


class BinaryStreamCollector:
    """Collect one child pipe without allowing a noisy sidecar to exceed the smoke bound."""

    def __init__(self, stream: Any) -> None:
        """Keep one binary pipe's bounded capture and queue separate from the handshake state."""

        self._stream = stream
        self.lines = Queue()
        self.chunks: list[bytes] = []
        self.total_bytes = 0
        self.overflow = False
        self.error: BaseException | None = None
        self._thread = threading.Thread(target=self._consume, daemon=True)

    def start(self) -> None:
        """Start a dedicated reader because Windows does not support select() on anonymous pipes."""

        self._thread.start()

    def _consume(self) -> None:
        """Read bounded binary lines and publish them to the handshake reader in arrival order."""

        try:
            while True:
                line = self._stream.readline(MAX_OUTPUT_BYTES + 1)
                if not line:
                    break
                self.total_bytes += len(line)
                if self.total_bytes > MAX_OUTPUT_BYTES:
                    self.overflow = True
                    break
                self.chunks.append(line)
                self.lines.put(line)
        except BaseException as failure:  # pragma: no cover - platform pipe failures are external
            self.error = failure
        finally:
            self.lines.put(None)

    def join(self, timeout: float) -> None:
        """Join the reader only for the remaining smoke deadline so pipe cleanup cannot hang CI."""

        self._thread.join(max(0.0, timeout))

    def bytes(self) -> bytes:
        """Return the captured bytes after the reader has reached EOF or the hard cap."""

        return b"".join(self.chunks)


def terminate_process(process: subprocess.Popen[Any]) -> None:
    """Terminate the whole smoke process group after a deadline without leaving a child behind."""

    if process.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    else:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        if os.name != "nt":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        process.kill()
        process.wait(timeout=5)


def send_frame(process: subprocess.Popen[Any], frame: dict[str, Any]) -> None:
    """Write one UTF-8 JSONL frame as bytes so Windows cannot translate LF into CRLF."""

    if process.stdin is None:
        raise RuntimeError("native sidecar stdin is unavailable")
    payload = json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n"
    try:
        process.stdin.write(payload)
        process.stdin.flush()
    except (BrokenPipeError, OSError) as failure:
        raise RuntimeError("native sidecar stdin closed during handshake") from failure


def next_line(collector: BinaryStreamCollector, deadline: float) -> bytes | None:
    """Read one binary JSONL line with the shared process deadline and bounded queue."""

    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise RuntimeError("native sidecar smoke exceeded its deadline")
    try:
        line = collector.lines.get(timeout=remaining)
    except Empty as failure:
        raise RuntimeError("native sidecar smoke exceeded its deadline") from failure
    if line is None and collector.error is not None:
        raise RuntimeError("native sidecar stdout reader failed") from collector.error
    if line is None and collector.overflow:
        raise RuntimeError("native sidecar smoke output exceeded the hard limit")
    return line


def decode_frame(line: bytes) -> dict[str, Any]:
    """Decode exactly one UTF-8 JSON object while rejecting non-protocol stdout bytes."""

    try:
        document = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as failure:
        raise RuntimeError("native sidecar emitted a non-JSON stdout line") from failure
    if not isinstance(document, dict):
        raise RuntimeError("native sidecar emitted a non-object JSON frame")
    return document


def read_until(
    collector: BinaryStreamCollector,
    documents: list[dict[str, Any]],
    deadline: float,
    *,
    frame_id: str | None = None,
    method: str | None = None,
    predicate: Any = None,
) -> dict[str, Any]:
    """Read frames until one correlated response or predicate match arrives, retaining events."""

    for _ in range(128):
        line = next_line(collector, deadline)
        if line is None:
            raise RuntimeError("native sidecar closed stdout before handshake completed")
        document = decode_frame(line)
        documents.append(document)
        matches_id = frame_id is None or document.get("id") == frame_id
        matches_method = method is None or document.get("method") == method
        matches_predicate = predicate is None or predicate(document)
        if matches_id and matches_method and matches_predicate:
            return document
    raise RuntimeError("native sidecar emitted too many frames before the expected handshake frame")


def drain_stdout(
    collector: BinaryStreamCollector, documents: list[dict[str, Any]], deadline: float
) -> None:
    """Drain trailing JSONL events after shutdown so the reader thread and child pipe fully close."""

    while True:
        line = next_line(collector, deadline)
        if line is None:
            return
        documents.append(decode_frame(line))


def ready_frame(document: dict[str, Any]) -> bool:
    """Identify only the ready event carrying the exact challenge issued by the smoke client."""

    params = document.get("params")
    return (
        document.get("method") == "runtime/statusChanged"
        and isinstance(params, dict)
        and params.get("status") == "ready"
        and params.get("readyToken") == READY_TOKEN
    )


def run_smoke(executable: Path, data_directory: Path, timeout_seconds: float) -> dict[str, Any]:
    """Execute a sequential binary handshake while bounding output and process lifetime."""

    if not executable.is_file():
        raise RuntimeError("native executable is missing")
    data_directory.mkdir(parents=True, exist_ok=True)
    encoded_path = base64.urlsafe_b64encode(str(data_directory.resolve()).encode("utf-8")).decode(
        "ascii"
    ).rstrip("=")
    creationflags = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    process = subprocess.Popen(
        [str(executable), "--runtime=production", f"--data-dir-base64={encoded_path}"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        env=sanitized_environment(),
        creationflags=creationflags,
        start_new_session=os.name != "nt",
    )
    stdout_collector = BinaryStreamCollector(process.stdout)
    stderr_collector = BinaryStreamCollector(process.stderr)
    stdout_collector.start()
    stderr_collector.start()
    documents: list[dict[str, Any]] = []
    deadline = time.monotonic() + timeout_seconds
    try:
        send_frame(process, initialize_frame())
        initialize = read_until(stdout_collector, documents, deadline, frame_id="c:init")
        if not isinstance(initialize.get("result"), dict):
            raise RuntimeError("native sidecar did not acknowledge initialize")
        result = initialize["result"]
        runtime = result.get("runtime")
        if not isinstance(runtime, dict) or runtime.get("kind") != "native-image":
            raise RuntimeError("initialize did not report native-image runtime")
        if runtime.get("javaVersion") != "25":
            raise RuntimeError("initialize did not report Java 25")

        send_frame(process, initialized_frame())
        read_until(stdout_collector, documents, deadline, predicate=ready_frame)
        send_frame(process, shutdown_frame())
        shutdown = read_until(stdout_collector, documents, deadline, frame_id="c:shutdown")
        if not isinstance(shutdown.get("result"), dict) or shutdown["result"].get("accepted") is not True:
            raise RuntimeError("native sidecar did not acknowledge shutdown")

        if process.stdin is not None:
            process.stdin.close()
        drain_stdout(stdout_collector, documents, deadline)
        remaining = max(0.0, deadline - time.monotonic())
        try:
            process.wait(timeout=remaining)
        except subprocess.TimeoutExpired as failure:
            raise RuntimeError("native sidecar smoke exceeded its deadline") from failure
        stderr_collector.join(max(0.0, deadline - time.monotonic()))
        if stdout_collector.overflow or stderr_collector.overflow:
            raise RuntimeError("native sidecar smoke output exceeded the hard limit")
        if stdout_collector.error is not None or stderr_collector.error is not None:
            raise RuntimeError("native sidecar pipe reader failed")

        stdout = stdout_collector.bytes()
        stderr = stderr_collector.bytes()
        if process.returncode != 0:
            raise RuntimeError(f"native sidecar exited with status {process.returncode}")
        stdout_text = stdout.decode("utf-8", errors="replace")
        stderr_text = stderr.decode("utf-8", errors="replace")
        if stderr:
            raise RuntimeError("native sidecar wrote unexpected stderr")
        if LEAK_PATTERN.search(stdout_text) or LEAK_PATTERN.search(stderr_text):
            raise RuntimeError("native sidecar smoke output contains a credential marker")
        if str(data_directory.resolve()) in stdout_text or str(data_directory.resolve()) in stderr_text:
            raise RuntimeError("native sidecar smoke output contains the private data path")

        return {
            "status": "passed",
            "returnCode": process.returncode,
            "frameCount": len(documents),
            "methods": [doc.get("method") for doc in documents if isinstance(doc.get("method"), str)],
            "runtime": runtime,
            "stdoutBytes": len(stdout),
            "stderrBytes": len(stderr),
            "environment": {"javaHomeRemoved": True, "credentialLikeVariablesRemoved": True},
        }
    except RuntimeError as failure:
        if process.poll() not in (None, 0):
            raise RuntimeError(f"native sidecar exited with status {process.returncode}") from failure
        raise
    finally:
        if process.stdin is not None:
            try:
                process.stdin.close()
            except OSError:
                pass
        if process.poll() is None:
            terminate_process(process)
        stdout_collector.join(5)
        stderr_collector.join(5)


def parse_args() -> argparse.Namespace:
    """Parse the narrow CI interface so callers cannot accidentally select a different runtime."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--executable", required=True, type=Path)
    parser.add_argument("--data-dir", required=True, type=Path)
    parser.add_argument("--timeout-seconds", type=float, default=45.0)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def main() -> int:
    """Run the smoke gate and write only sanitized structured evidence."""

    args = parse_args()
    try:
        report = run_smoke(args.executable, args.data_dir, args.timeout_seconds)
    except (OSError, RuntimeError) as failure:
        print(f"native smoke failed: {failure}", file=sys.stderr)
        return 1
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8", newline="\n")
    print(encoded, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
