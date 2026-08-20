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
import re
import signal
import subprocess
import sys
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


def terminate_process(process: subprocess.Popen[str]) -> None:
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


def run_smoke(executable: Path, data_directory: Path, timeout_seconds: float) -> dict[str, Any]:
    """Execute and validate one native handshake while bounding output and process lifetime."""

    if not executable.is_file():
        raise RuntimeError("native executable is missing")
    data_directory.mkdir(parents=True, exist_ok=True)
    encoded_path = base64.urlsafe_b64encode(str(data_directory.resolve()).encode("utf-8")).decode(
        "ascii"
    ).rstrip("=")
    frames = "".join(
        json.dumps(frame, ensure_ascii=False, separators=(",", ":")) + "\n"
        for frame in (initialize_frame(), initialized_frame(), shutdown_frame())
    )
    creationflags = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    process = subprocess.Popen(
        [str(executable), "--runtime=production", f"--data-dir-base64={encoded_path}"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=sanitized_environment(),
        creationflags=creationflags,
        start_new_session=os.name != "nt",
    )
    try:
        stdout, stderr = process.communicate(frames, timeout=timeout_seconds)
    except subprocess.TimeoutExpired as failure:
        terminate_process(process)
        stdout, stderr = process.communicate()
        raise RuntimeError("native sidecar smoke exceeded its deadline") from failure

    if len(stdout.encode("utf-8")) > MAX_OUTPUT_BYTES or len(stderr.encode("utf-8")) > MAX_OUTPUT_BYTES:
        raise RuntimeError("native sidecar smoke output exceeded the hard limit")
    if process.returncode != 0:
        raise RuntimeError(f"native sidecar exited with status {process.returncode}")
    if stderr:
        raise RuntimeError("native sidecar wrote unexpected stderr")
    if LEAK_PATTERN.search(stdout) or LEAK_PATTERN.search(stderr):
        raise RuntimeError("native sidecar smoke output contains a credential marker")
    if str(data_directory.resolve()) in stdout or str(data_directory.resolve()) in stderr:
        raise RuntimeError("native sidecar smoke output contains the private data path")

    documents: list[dict[str, Any]] = []
    for line in stdout.splitlines():
        if not line.strip():
            continue
        try:
            document = json.loads(line)
        except json.JSONDecodeError as failure:
            raise RuntimeError("native sidecar emitted a non-JSON stdout line") from failure
        if not isinstance(document, dict):
            raise RuntimeError("native sidecar emitted a non-object JSON frame")
        documents.append(document)

    initialize = next((doc for doc in documents if doc.get("id") == "c:init"), None)
    ready = next(
        (
            doc
            for doc in documents
            if doc.get("method") == "runtime/statusChanged"
            and isinstance(doc.get("params"), dict)
            and doc["params"].get("status") == "ready"
        ),
        None,
    )
    shutdown = next((doc for doc in documents if doc.get("id") == "c:shutdown"), None)
    if initialize is None or not isinstance(initialize.get("result"), dict):
        raise RuntimeError("native sidecar did not acknowledge initialize")
    result = initialize["result"]
    runtime = result.get("runtime")
    if not isinstance(runtime, dict) or runtime.get("kind") != "native-image":
        raise RuntimeError("initialize did not report native-image runtime")
    if runtime.get("javaVersion") != "25":
        raise RuntimeError("initialize did not report Java 25")
    if ready is None or ready.get("params", {}).get("readyToken") != READY_TOKEN:
        raise RuntimeError("native sidecar did not publish the ready challenge")
    if shutdown is None or shutdown.get("result", {}).get("accepted") is not True:
        raise RuntimeError("native sidecar did not acknowledge shutdown")

    return {
        "status": "passed",
        "returnCode": process.returncode,
        "frameCount": len(documents),
        "methods": [doc.get("method") for doc in documents if isinstance(doc.get("method"), str)],
        "runtime": runtime,
        "stdoutBytes": len(stdout.encode("utf-8")),
        "stderrBytes": len(stderr.encode("utf-8")),
        "environment": {"javaHomeRemoved": True, "credentialLikeVariablesRemoved": True},
    }


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
