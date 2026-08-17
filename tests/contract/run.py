# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later
"""Run the JA frozen protocol gate against Java, Rust, and TypeScript."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import random
import re
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence


ROOT = Path(__file__).resolve().parents[2]
GOLDEN = ROOT / "contracts" / "golden"
CONTRACT = ROOT / "tests" / "contract"
MAX_OUTPUT_BYTES = 256 * 1024
MAX_COMBINED_OUTPUT_BYTES = MAX_OUTPUT_BYTES * 2
COMMAND_TIMEOUT_SECONDS = 600
PROCESS_CLEANUP_TIMEOUT_SECONDS = 8
MAX_ARTIFACT_SNAPSHOT_ENTRIES = 100_000
MAX_DESCENDANT_PIDS = 1_024
PROPERTY_SEED = 20260816
EXPECTED = {
    "validFrames": 54,
    "methodResults": 16,
    "handshakeValidFrames": 6,
    "handshakeInvalidCases": 23,
    "parseFrames": 47,
}


def expected_rust_contract_tests() -> int:
    """Keep the shared marker aligned after adding one cross-platform Rust regression while retaining four Windows-only skips on Unix."""
    return 58 if os.name == "nt" else 54


def rust_test_artifact_exists(target_root: Path | None = None) -> bool:
    """Require Cargo's platform executable before test execution without exposing its absolute path."""
    if target_root is None:
        configured_target = os.environ.get("CARGO_TARGET_DIR")
        target_root = Path(configured_target) if configured_target else ROOT / "src-tauri" / "target"
        if not target_root.is_absolute():
            target_root = ROOT / target_root
    dependencies = target_root / "debug" / "deps"
    if not dependencies.is_dir():
        return False
    for artifact in dependencies.glob("agent_process_contract-*"):
        if not artifact.is_file():
            continue
        if os.name == "nt" and artifact.suffix.lower() == ".exe":
            return True
        if os.name != "nt" and artifact.suffix == "" and os.access(artifact, os.X_OK):
            return True
    return False


class GateFailure(RuntimeError):
    """Carries only a safe stage classification; raw command output never escapes the gate."""


@dataclass(frozen=True)
class ProcessResult:
    """Store only bounded child output so every caller shares the same leak boundary."""

    args: tuple[str, ...]
    returncode: int | None
    stdout: bytes
    stderr: bytes
    output_capped: bool = False
    orphaned: bool = False
    reader_failed: bool = False


def corpus_files(root: Path) -> list[Path]:
    """Keep corpus membership deterministic so all consumers hash exactly the same inputs."""
    return sorted(
        path
        for path in root.rglob("*")
        if path.is_file() and path.suffix in {".json", ".jsonl"}
    )


def corpus_digest(root: Path) -> str:
    """Hash relative names and bytes, preventing a consumer from silently reading another fixture set."""
    digest = hashlib.sha256()
    for path in corpus_files(root):
        relative = path.relative_to(root).as_posix().encode("utf-8")
        digest.update(relative)
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def bounded_output(data: bytes) -> bytes:
    """Bound captured diagnostics before memory or secret-redaction work can become unbounded."""
    return data[:MAX_OUTPUT_BYTES]


def safe_digest(data: bytes) -> str:
    """Expose only an opaque failure fingerprint so path, prompt, and token material cannot leak."""
    return hashlib.sha256(data).hexdigest()[:16]


def safe_failure(stage: str, result: ProcessResult | None, reason: str) -> GateFailure:
    """Convert any process failure into case-independent evidence without printing raw stdout/stderr."""
    if result is None:
        return GateFailure(f"FAIL stage={stage} classification={reason}")
    combined = bounded_output(result.stdout or b"") + bounded_output(result.stderr or b"")
    marker = re.search(rb"classification=([A-Za-z0-9_]+)", combined)
    case_markers = re.findall(rb"RUST_CASE=([A-Za-z0-9_-]+)", combined)
    rust_error = re.search(rb"error\[E([0-9]+)\]", combined)
    rust_method = re.search(rb"no method named [`']([A-Za-z0-9_]+)[`']", combined)
    rust_value = re.search(rb"cannot find value [`']([A-Za-z0-9_]+)[`']", combined)
    classification = marker.group(1).decode("ascii") if marker else reason
    if rust_error is not None:
        classification = f"rustc_E{rust_error.group(1).decode('ascii')}"
        if rust_method is not None:
            classification += f"_{rust_method.group(1).decode('ascii')}"
        if rust_value is not None:
            classification += f"_{rust_value.group(1).decode('ascii')}"
    if case_markers:
        classification += f"_case_{case_markers[-1].decode('ascii')}"
    return GateFailure(
        f"FAIL stage={stage} exit={result.returncode} outputHash={safe_digest(combined)} classification={classification}"
    )


def run_command(
    stage: str,
    args: Sequence[str],
    cwd: Path,
    *,
    env: dict[str, str] | None = None,
    timeout: int = COMMAND_TIMEOUT_SECONDS,
) -> ProcessResult:
    """Drain both pipes concurrently and kill the complete child tree on any hard bound."""
    command = tuple(str(argument) for argument in args)
    creationflags = 0
    launch_options: dict[str, object] = {}
    if os.name == "nt":
        creationflags = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        launch_options["creationflags"] = creationflags
    else:
        launch_options["start_new_session"] = True
    try:
        process = subprocess.Popen(
            list(command),
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            **launch_options,
        )
    except OSError as error:
        # Hash only the exception class because spawn text can contain an executable path.
        raise GateFailure(
            f"FAIL stage={stage} classification=spawn_failed outputHash={safe_digest(type(error).__name__.encode('ascii'))}"
        ) from error

    stdout_buffer = bytearray()
    stderr_buffer = bytearray()
    capture_lock = threading.Lock()
    stop_event = threading.Event()
    capped = False
    reader_failed = False
    kill_lock = threading.Lock()
    killed = False
    cleanup_budget_exceeded = False
    cleanup_scan_failed = False
    cleanup_deadline: float | None = None

    def arm_cleanup_deadline() -> float:
        """Create one absolute cleanup deadline so descendant scans and reaps share one budget."""
        nonlocal cleanup_deadline
        if cleanup_deadline is None:
            cleanup_deadline = time.monotonic() + PROCESS_CLEANUP_TIMEOUT_SECONDS
        return cleanup_deadline

    def cleanup_remaining(deadline: float) -> float:
        """Convert the shared absolute deadline to the remaining subprocess timeout."""
        return max(0.0, deadline - time.monotonic())

    def windows_process_identity(pid: int, deadline: float) -> str | None:
        """Read a process creation stamp so a reused PID cannot be killed by a later scan."""
        return windows_process_identities((pid,), deadline).get(pid)

    def windows_process_identities(pids: Sequence[int], deadline: float) -> dict[int, str]:
        """Batch creation-stamp checks so PID-reuse protection does not consume one subprocess per child."""
        if os.name != "nt":
            return {}
        numeric_pids = tuple(sorted({pid for pid in pids if pid > 0}))
        if not numeric_pids:
            return {}
        remaining = cleanup_remaining(deadline)
        if remaining <= 0:
            return {}
        pid_array = ",".join(str(pid) for pid in numeric_pids)
        script = (
            "$ErrorActionPreference='Stop'; "
            f"$ids = @({pid_array}); "
            "Get-CimInstance Win32_Process | "
            "Where-Object { $ids -contains [int]$_.ProcessId } | "
            "Select-Object ProcessId,@{Name='CreationDate';Expression={ $_.CreationDate.ToUniversalTime().ToString('o') }} | ConvertTo-Json -Compress"
        )
        try:
            snapshot = subprocess.run(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                timeout=remaining,
                check=False,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
        except (OSError, subprocess.SubprocessError) as error:
            raise RuntimeError("process identity scan") from error
        payload = snapshot.stdout.decode("utf-8", errors="replace").strip().lstrip("\ufeff")
        if not payload:
            return {}
        try:
            records = json.loads(payload)
        except (TypeError, ValueError) as error:
            raise RuntimeError("process identity payload") from error
        if isinstance(records, dict):
            records = [records]
        if not isinstance(records, list):
            raise RuntimeError("process identity shape")
        identities: dict[int, str] = {}
        for record in records:
            if not isinstance(record, dict):
                continue
            try:
                process_id = int(record["ProcessId"])
            except (KeyError, TypeError, ValueError):
                continue
            if process_id in numeric_pids:
                identities[process_id] = str(record.get("CreationDate") or "")
        return identities

    def windows_descendants(root_pid: int, deadline: float) -> list[tuple[int, str, int]]:
        """Find bounded descendants with parent/depth/creation data for leaf-first, PID-safe cleanup."""
        if os.name != "nt":
            return []
        descendants: dict[int, tuple[str, int]] = {}
        parents: list[int] = [root_pid]
        depth = 0
        while parents:
            remaining = cleanup_remaining(deadline)
            if remaining <= 0:
                break
            parent_filter = " OR ".join(f"ParentProcessId = {parent}" for parent in parents)
            script = (
                "$ErrorActionPreference='Stop'; "
                f"Get-CimInstance Win32_Process -Filter '{parent_filter}' | "
                f"Select-Object -First {MAX_DESCENDANT_PIDS + 1} ProcessId,ParentProcessId,@{{Name='CreationDate';Expression={{ $_.CreationDate.ToUniversalTime().ToString('o') }}}} | "
                "ConvertTo-Json -Compress"
            )
            try:
                snapshot = subprocess.run(
                    ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    timeout=remaining,
                    check=False,
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                )
            except (OSError, subprocess.SubprocessError) as error:
                raise RuntimeError("descendant scan") from error
            payload = snapshot.stdout.decode("utf-8", errors="replace").strip().lstrip("\ufeff")
            if not payload:
                parents = []
                continue
            try:
                records = json.loads(payload)
            except (TypeError, ValueError) as error:
                raise RuntimeError("descendant payload") from error
            if isinstance(records, dict):
                records = [records]
            if not isinstance(records, list):
                raise RuntimeError("descendant shape")
            if len(records) > MAX_DESCENDANT_PIDS:
                raise OverflowError("descendant budget")
            parent_set = set(parents)
            next_parents: list[int] = []
            for record in records:
                if not isinstance(record, dict):
                    continue
                try:
                    child = int(record["ProcessId"])
                    actual_parent = int(record["ParentProcessId"])
                except (KeyError, TypeError, ValueError):
                    continue
                if actual_parent not in parent_set or child <= 0 or child == root_pid:
                    continue
                if child not in descendants:
                    creation_date = str(record.get("CreationDate") or "")
                    descendants[child] = (creation_date, depth + 1)
                    if len(descendants) > MAX_DESCENDANT_PIDS:
                        raise OverflowError("descendant budget")
                    next_parents.append(child)
            parents = next_parents
            depth += 1
        return [
            (pid, creation_date, child_depth)
            for pid, (creation_date, child_depth) in sorted(
                descendants.items(), key=lambda item: item[1][1], reverse=True
            )
        ]

    def windows_stop_descendants(
        descendants: Sequence[tuple[int, str, int]], deadline: float
    ) -> None:
        """Recheck creation stamps and stop one depth level in one bounded PowerShell call."""
        if os.name != "nt" or not descendants:
            return
        expected = {
            str(pid): creation_date
            for pid, creation_date, _depth in descendants
            if pid > 0
        }
        if not expected:
            return
        encoded = json.dumps(expected, ensure_ascii=True, separators=(",", ":")).replace("'", "''")
        pid_filter = " OR ".join(f"ProcessId = {pid}" for pid in expected)
        script = (
            "$ErrorActionPreference='Stop'; "
            f"$expected = ConvertFrom-Json -InputObject '{encoded}'; "
            f"Get-CimInstance Win32_Process -Filter '{pid_filter}' | "
            "ForEach-Object { "
            "$key = [string]$_.ProcessId; "
            "$stamp = $expected.PSObject.Properties[$key]; "
            "if ($null -ne $stamp) { "
            "$expectedValue = [string]$stamp.Value; "
            "if ($stamp.Value -is [datetime]) { $expectedValue = $stamp.Value.ToUniversalTime().ToString('o') }; "
            "$current = $_.CreationDate.ToUniversalTime().ToString('o'); "
            "if ($expectedValue -eq [string]$current) { "
            "Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue "
            "} "
            "} "
            "}"
        )
        remaining = cleanup_remaining(deadline)
        if remaining <= 0:
            return
        try:
            result = subprocess.run(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=remaining,
                check=False,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
        except (OSError, subprocess.SubprocessError) as error:
            raise RuntimeError("descendant stop") from error
        if result.returncode != 0:
            raise RuntimeError("descendant stop status")

    def terminate_tree() -> None:
        """Terminate the complete tree within one deadline, failing closed on an oversized scan."""
        nonlocal killed, cleanup_budget_exceeded, cleanup_scan_failed
        deadline = arm_cleanup_deadline()
        with kill_lock:
            if killed:
                return
            killed = True
        try:
            if os.name == "nt":
                root_identity: str | None = None
                try:
                    if process.poll() is None:
                        root_identity = windows_process_identity(process.pid, deadline)
                except RuntimeError:
                    cleanup_scan_failed = True
                empty_confirmations = 0
                while cleanup_remaining(deadline) > 0:
                    # A live root can be removed through the OS tree operation first;
                    # descendants are still rescanned because inherited pipes can outlive it.
                    if process.poll() is None and root_identity is not None:
                        try:
                            current_identity = windows_process_identity(process.pid, deadline)
                        except RuntimeError:
                            cleanup_scan_failed = True
                            current_identity = None
                        if current_identity == root_identity:
                            try:
                                subprocess.run(
                                    ["taskkill", "/PID", str(process.pid), "/T", "/F"],
                                    stdin=subprocess.DEVNULL,
                                    stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL,
                                    timeout=cleanup_remaining(deadline),
                                    check=False,
                                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                                )
                            except (OSError, subprocess.SubprocessError):
                                cleanup_scan_failed = True
                            root_identity = None
                    if process.poll() is None and root_identity is None:
                        # Popen's live handle is safer than a reused PID when WMI
                        # cannot identify the root; descendants are handled below.
                        try:
                            process.kill()
                        except OSError:
                            pass
                    try:
                        descendants = windows_descendants(process.pid, deadline)
                    except OverflowError:
                        cleanup_budget_exceeded = True
                        descendants = []
                    except RuntimeError:
                        cleanup_scan_failed = True
                        descendants = []
                    if descendants:
                        empty_confirmations = 0
                        for depth in sorted({item[2] for item in descendants}, reverse=True):
                            depth_group = [item for item in descendants if item[2] == depth]
                            try:
                                windows_stop_descendants(depth_group, deadline)
                            except RuntimeError:
                                cleanup_scan_failed = True
                        continue
                    if cleanup_budget_exceeded:
                        break
                    # A WMI snapshot can race process exit; require two empty scans
                    # separated by a bounded yield before declaring the tree gone.
                    empty_confirmations += 1
                    if empty_confirmations >= 2:
                        break
                    time.sleep(min(0.02, cleanup_remaining(deadline)))
            else:
                try:
                    process_group = os.getpgid(process.pid)
                except ProcessLookupError:
                    # The leader may already be reaped while its process group
                    # remains alive; CREATE_NEW_SESSION makes pid the group id.
                    process_group = process.pid
                os.killpg(process_group, signal.SIGKILL)
        except (OSError, subprocess.SubprocessError, RuntimeError):
            cleanup_scan_failed = True
            try:
                process.kill()
            except OSError:
                pass

    def drain(pipe: object, target: bytearray) -> None:
        """Read incrementally so a noisy child cannot deadlock while the other pipe fills."""
        nonlocal capped, reader_failed
        try:
            while True:
                chunk = pipe.read(16 * 1024)  # type: ignore[union-attr]
                if not chunk:
                    return
                should_kill = False
                with capture_lock:
                    remaining_stream = MAX_OUTPUT_BYTES - len(target)
                    remaining_total = MAX_COMBINED_OUTPUT_BYTES - len(stdout_buffer) - len(stderr_buffer)
                    allowed = min(len(chunk), max(0, remaining_stream), max(0, remaining_total))
                    if allowed:
                        target.extend(chunk[:allowed])
                    if allowed < len(chunk):
                        capped = True
                        stop_event.set()
                        should_kill = True
                if should_kill:
                    terminate_tree()
        except (OSError, ValueError):
            with capture_lock:
                reader_failed = True
            stop_event.set()
            terminate_tree()

    readers = [
        threading.Thread(target=drain, args=(process.stdout, stdout_buffer), daemon=True),
        threading.Thread(target=drain, args=(process.stderr, stderr_buffer), daemon=True),
    ]
    for reader in readers:
        reader.start()
    deadline = time.monotonic() + timeout
    interrupted = False
    timed_out = False
    try:
        while process.poll() is None:
            if stop_event.is_set():
                terminate_tree()
            if time.monotonic() >= deadline:
                timed_out = True
                terminate_tree()
                break
            try:
                process.wait(timeout=0.1)
            except subprocess.TimeoutExpired:
                continue
    except KeyboardInterrupt as error:
        interrupted = True
        terminate_tree()
        raise GateFailure(f"FAIL stage={stage} classification=interrupted") from error
    finally:
        if timed_out:
            terminate_tree()
        if process.poll() is None:
            cleanup_deadline_for_wait = arm_cleanup_deadline()
            remaining = cleanup_remaining(cleanup_deadline_for_wait)
            if remaining > 0:
                try:
                    process.wait(timeout=remaining)
                except subprocess.TimeoutExpired:
                    terminate_tree()
        # A root can exit while descendants still own the inherited pipes;
        # close that tree before spending the shared deadline joining readers.
        if process.poll() is not None and any(reader.is_alive() for reader in readers):
            terminate_tree()
        for reader in readers:
            if reader.is_alive():
                reader_deadline = arm_cleanup_deadline()
                reader.join(timeout=cleanup_remaining(reader_deadline))
        if any(reader.is_alive() for reader in readers):
            # A root process can exit while an inherited pipe remains open;
            # kill the remembered process group/tree before declaring a leak.
            terminate_tree()
            for reader in readers:
                if reader.is_alive():
                    reader.join(timeout=cleanup_remaining(arm_cleanup_deadline()))

    with capture_lock:
        output_capped = capped
        capture_failed = reader_failed
        stdout = bytes(stdout_buffer)
        stderr = bytes(stderr_buffer)
    orphaned = process.poll() is None or any(reader.is_alive() for reader in readers)
    result = ProcessResult(
        args=command,
        returncode=process.returncode,
        stdout=stdout,
        stderr=stderr,
        output_capped=output_capped,
        orphaned=orphaned,
        reader_failed=capture_failed,
    )
    if interrupted:
        raise GateFailure(f"FAIL stage={stage} classification=interrupted")
    if timed_out:
        raise GateFailure(f"FAIL stage={stage} classification=timeout outputHash={safe_digest(stdout + stderr)}")
    if output_capped:
        raise safe_failure(stage, result, "output_cap")
    if capture_failed:
        raise safe_failure(stage, result, "capture_failed")
    if cleanup_budget_exceeded:
        raise GateFailure(f"FAIL stage={stage} classification=descendant_budget")
    if cleanup_scan_failed:
        raise GateFailure(f"FAIL stage={stage} classification=cleanup_scan_failed")
    if orphaned:
        raise safe_failure(stage, result, "orphan_cleanup")
    if result.returncode != 0:
        raise safe_failure(stage, result, "nonzero_exit")
    return result


def tool(name: str) -> str:
    """Use Windows command shims explicitly because CreateProcess does not resolve .cmd like a shell."""
    return f"{name}.cmd" if os.name == "nt" and name in {"mvn", "pnpm"} else name


def require_marker(stage: str, output: bytes, pattern: str) -> re.Match[str]:
    """Require a machine-readable success marker so a passing unrelated test suite cannot satisfy the gate."""
    text = output.decode("utf-8", errors="replace")
    match = re.search(pattern, text)
    if match is None:
        raise GateFailure(f"FAIL stage={stage} classification=missing_marker outputHash={safe_digest(output)}")
    return match


def make_property_corpus(directory: Path) -> Path:
    """Generate a bounded deterministic corpus shared by all adapters without adding repository fixtures."""
    path = directory / "property.jsonl"
    random_source = random.Random(PROPERTY_SEED)
    lines: list[str] = []
    for index in range(100):
        token = random_source.randbytes(16).hex()
        lines.append(
            json.dumps(
                {
                    "kind": "valid",
                    "frame": {
                        "jsonrpc": "2.0",
                        "method": "initialized",
                        "params": {"readyToken": token},
                    },
                },
                ensure_ascii=False,
                separators=(",", ":"),
            )
        )
    for index in range(100):
        lines.append(
            json.dumps(
                {
                    "kind": "invalid",
                    "frame": {
                        "jsonrpc": "2.0",
                        "id": f"c:property-{random_source.randrange(1_000_000)}",
                        "method": "turn/start",
                        "params": None,
                    },
                },
                ensure_ascii=False,
                separators=(",", ":"),
            )
        )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8", newline="\n")
    return path


def property_digest(path: Path) -> str:
    """Hash ordered expected classifications so every adapter proves its own 200-case decision stream."""
    digest = hashlib.sha256()
    for index, line in enumerate(path.read_text(encoding="utf-8").splitlines()):
        entry = json.loads(line)
        expected = entry.get("kind")
        if expected not in {"valid", "invalid"}:
            raise GateFailure("FAIL stage=property classification=unknown_kind")
        record = {
            "classification": "accepted" if expected == "valid" else "rejected",
            "expected": expected,
            "index": index,
        }
        canonical = json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        digest.update(canonical.encode("utf-8"))
        digest.update(b"\n")
    return digest.hexdigest()


def load_expected_counts() -> None:
    """Verify the frozen reference validator reports the exact corpus counts before adapters run."""
    result = run_command(
        "reference",
        [
            tool("uv"),
            "run",
            "--with",
            "jsonschema[format]",
            "python",
            "contracts/golden/validate.py",
        ],
        ROOT,
    )
    output = result.stdout + result.stderr
    require_marker(
        "reference",
        output,
        rf"SCHEMA_OK refs=\d+ validFrames={EXPECTED['validFrames']} methodResults={EXPECTED['methodResults']}",
    )
    require_marker(
        "reference",
        output,
        rf"HANDSHAKE_OK validFrames={EXPECTED['handshakeValidFrames']} invalidCases={EXPECTED['handshakeInvalidCases']}",
    )
    require_marker("reference", output, rf"PARSE_ONLY_OK invalidOrMajorFrames={EXPECTED['parseFrames']}")
    require_marker("reference", output, r"PATH_SECRET_SCAN_OK")
    print(
        "REFERENCE_OK "
        f"validFrames={EXPECTED['validFrames']} methodResults={EXPECTED['methodResults']} "
        f"handshakeInvalidCases={EXPECTED['handshakeInvalidCases']} parseFrames={EXPECTED['parseFrames']}"
    )


def require_jdk25() -> None:
    """Require the requested Java major before Maven can silently select another installed JDK."""
    result = run_command("jdk25", ["java", "--version"], ROOT)
    output = result.stdout + result.stderr
    if re.search(rb"(?:^|\s)25(?:[.\s-])", output, flags=re.MULTILINE) is None:
        raise GateFailure(f"FAIL stage=jdk25 classification=jdk25_required outputHash={safe_digest(output)}")
    javac = run_command("javac25", ["javac", "--version"], ROOT)
    javac_output = javac.stdout + javac.stderr
    if re.search(rb"(?:^|\s)25(?:[.\s-])", javac_output, flags=re.MULTILINE) is None:
        raise GateFailure(f"FAIL stage=javac25 classification=jdk25_required outputHash={safe_digest(javac_output)}")
    maven = run_command("maven-jdk25", [tool("mvn"), "-version"], ROOT)
    maven_output = maven.stdout + maven.stderr
    if re.search(rb"Java version:\s*25(?:[.\s-])", maven_output) is None:
        raise GateFailure(f"FAIL stage=maven-jdk25 classification=jdk25_required outputHash={safe_digest(maven_output)}")
    print("JDK25_OK major=25 javac=25 mavenJava=25")


def require_native_toolchain() -> None:
    """Gate Rust MSRV and the package-manager engine before any adapter is compiled."""
    rustc = run_command("rustc-version", ["rustc", "--version"], ROOT)
    cargo = run_command("cargo-version", [tool("cargo"), "--version"], ROOT)
    node = run_command("node-version", ["node", "--version"], ROOT)
    pnpm = run_command("pnpm-version", [tool("pnpm"), "--version"], ROOT)
    rust_output = (rustc.stdout + rustc.stderr).decode("ascii", errors="ignore")
    cargo_output = (cargo.stdout + cargo.stderr).decode("ascii", errors="ignore")
    node_output = (node.stdout + node.stderr).decode("ascii", errors="ignore")
    pnpm_output = (pnpm.stdout + pnpm.stderr).decode("ascii", errors="ignore").strip()
    rust_match = re.search(r"rustc\s+(\d+)\.(\d+)", rust_output)
    cargo_match = re.search(r"cargo\s+(\d+)\.(\d+)", cargo_output)
    node_match = re.search(r"v(\d+)(?:\.(\d+))?", node_output)
    if rust_match is None or cargo_match is None:
        raise GateFailure("FAIL stage=toolchain classification=rust_version_unreadable")
    rust_version = (int(rust_match.group(1)), int(rust_match.group(2)))
    cargo_version = (int(cargo_match.group(1)), int(cargo_match.group(2)))
    if rust_version < (1, 88) or cargo_version < (1, 88):
        raise GateFailure("FAIL stage=toolchain classification=rust_msrv")
    if node_match is None or int(node_match.group(1)) != 24:
        raise GateFailure("FAIL stage=toolchain classification=node24_required")
    if pnpm_output != "10.33.0":
        raise GateFailure("FAIL stage=toolchain classification=pnpm_lock_version")
    print(
        "TOOLCHAIN_OK "
        f"rust={rust_match.group(1)}.{rust_match.group(2)} "
        f"cargo={cargo_match.group(1)}.{cargo_match.group(2)} node={node_match.group(1)} pnpm=10.33.0"
    )


def remove_empty_vite_temp() -> None:
    """Remove only the known empty Vite residue so the gate leaves the checkout as it found it."""
    vite_temp = ROOT / "node_modules" / ".vite-temp"
    if not vite_temp.is_dir() or vite_temp.is_symlink():
        return
    try:
        next(vite_temp.iterdir())
    except StopIteration:
        try:
            vite_temp.rmdir()
        except OSError as error:
            raise GateFailure("FAIL stage=cleanup classification=vite_temp_cleanup_failed") from error
    except OSError as error:
        raise GateFailure("FAIL stage=cleanup classification=vite_temp_scan_failed") from error


def repo_artifact_snapshot(snapshot_root: Path = ROOT) -> dict[str, tuple[str, int, int]]:
    """Fingerprint bounded roots while ignoring directory metadata that does not leave an artifact."""
    found: dict[str, tuple[str, int, int]] = {}
    roots = (
        snapshot_root / "agent" / "target",
        snapshot_root / "src-tauri" / "target",
        snapshot_root / "node_modules" / ".vite-temp",
        snapshot_root / "node_modules" / ".vite",
    )
    pending = [root for root in roots if root.exists()]
    while pending:
        path = pending.pop()
        relative = path.relative_to(snapshot_root).as_posix()
        try:
            stat = path.stat()
        except OSError as error:
            raise GateFailure("FAIL stage=cleanup classification=artifact_stat_failed") from error
        found[relative] = ("dir", 0, 0) if path.is_dir() else ("file", stat.st_size, stat.st_mtime_ns)
        if len(found) > MAX_ARTIFACT_SNAPSHOT_ENTRIES:
            raise GateFailure("FAIL stage=cleanup classification=artifact_snapshot_limit")
        if path.is_dir():
            try:
                pending.extend(path.iterdir())
            except OSError as error:
                raise GateFailure("FAIL stage=cleanup classification=artifact_scan_failed") from error

    # Keep the broad scan bounded and avoid traversing source-control metadata or
    # all dependencies; generated bytecode elsewhere is still caught exactly.
    pending = [snapshot_root]
    while pending:
        directory = pending.pop()
        try:
            children = list(directory.iterdir())
        except OSError as error:
            raise GateFailure("FAIL stage=cleanup classification=artifact_scan_failed") from error
        for path in children:
            if path.is_dir():
                if path.name in {".git", ".updeng", "node_modules", "target"}:
                    continue
                if path.name == "__pycache__":
                    relative = path.relative_to(snapshot_root).as_posix()
                    found[relative] = ("dir", 0, 0)
                    pending.append(path)
                else:
                    pending.append(path)
                continue
            if path.suffix.lower() not in {".class", ".exe", ".pyc"} and not path.name.startswith("property."):
                continue
            relative = path.relative_to(snapshot_root).as_posix()
            stat = path.stat()
            found[relative] = ("file", stat.st_size, stat.st_mtime_ns)
            if len(found) > MAX_ARTIFACT_SNAPSHOT_ENTRIES:
                raise GateFailure("FAIL stage=cleanup classification=artifact_snapshot_limit")
    return found


def artifact_root_category(relative: str) -> str:
    """Classify a relative artifact path into a bounded label so failures never reveal checkout paths."""
    if relative == "agent/target" or relative.startswith("agent/target/"):
        return "agent_target"
    if relative == "src-tauri/target" or relative.startswith("src-tauri/target/"):
        return "src_tauri_target"
    if relative == "node_modules/.vite-temp" or relative.startswith("node_modules/.vite-temp/"):
        return "vite_temp"
    if relative == "node_modules/.vite" or relative.startswith("node_modules/.vite/"):
        return "vite_cache"
    if "__pycache__" in relative or relative.endswith(".pyc"):
        return "bytecode"
    return "other"


def artifact_difference_summary(
    before: dict[str, tuple[str, int, int]], after: dict[str, tuple[str, int, int]]
) -> tuple[int, int, int, str]:
    """Summarize only file/dir counts and known root labels so cleanup diagnostics stay bounded and private."""
    changed_file_count = 0
    changed_dir_count = 0
    removed_count = 0
    category_counts: dict[str, int] = {}

    def record(relative: str, kind: str) -> None:
        nonlocal changed_file_count, changed_dir_count, removed_count
        if kind == "removed":
            removed_count += 1
        elif kind == "dir":
            changed_dir_count += 1
        else:
            changed_file_count += 1
        category = artifact_root_category(relative)
        category_counts[category] = category_counts.get(category, 0) + 1

    for relative, stamp in after.items():
        previous = before.get(relative)
        if previous == stamp:
            continue
        record(relative, stamp[0])
    for relative in before:
        if relative not in after:
            record(relative, "removed")
    categories = ",".join(
        f"{category}:{category_counts[category]}" for category in sorted(category_counts)
    ) or "none"
    return changed_file_count, changed_dir_count, removed_count, categories


def verify_artifact_snapshot_semantics() -> None:
    """Prove directory-only metadata churn is ignored while file additions, rewrites, and removals fail closed."""
    with tempfile.TemporaryDirectory(prefix="ja-artifact-check-") as directory:
        snapshot_root = Path(directory)
        cache_root = snapshot_root / "node_modules" / ".vite"
        cache_root.mkdir(parents=True)
        before = repo_artifact_snapshot(snapshot_root)

        marker = cache_root / "directory-marker"
        marker.write_bytes(b"directory-only")
        marker.unlink()
        directory_after = repo_artifact_snapshot(snapshot_root)
        if artifact_difference_summary(before, directory_after)[:3] != (0, 0, 0):
            raise GateFailure("FAIL stage=artifact-self-test classification=directory_metadata")

        probe = cache_root / "probe.class"
        probe.write_bytes(b"v1")
        added = repo_artifact_snapshot(snapshot_root)
        if artifact_difference_summary(directory_after, added)[0] == 0:
            raise GateFailure("FAIL stage=artifact-self-test classification=file_add")

        previous_mtime = probe.stat().st_mtime_ns
        probe.write_bytes(b"v2")
        os.utime(probe, ns=(previous_mtime, previous_mtime + 1_000_000_000))
        rewritten = repo_artifact_snapshot(snapshot_root)
        if artifact_difference_summary(added, rewritten)[0] == 0:
            raise GateFailure("FAIL stage=artifact-self-test classification=file_change")

        probe.unlink()
        removed = repo_artifact_snapshot(snapshot_root)
        if artifact_difference_summary(rewritten, removed)[2] == 0:
            raise GateFailure("FAIL stage=artifact-self-test classification=file_remove")
    print(
        "ARTIFACT_SNAPSHOT_OK directoryMtimeIgnored=true "
        "fileAddDetected=true fileChangeDetected=true fileRemoveDetected=true"
    )


def assert_no_repo_artifacts(before: dict[str, tuple[str, int, int]]) -> None:
    """Fail closed on new or changed files while preserving user targets and ignoring directory mtime churn."""
    after = repo_artifact_snapshot()
    changed_file_count, changed_dir_count, removed_count, categories = artifact_difference_summary(before, after)
    if changed_file_count or changed_dir_count or removed_count:
        raise GateFailure(
            "FAIL stage=cleanup classification=repo_artifact_created "
            f"changedFileCount={changed_file_count} changedDirCount={changed_dir_count} "
            f"removedCount={removed_count} rootCategories={categories}"
        )


def run_java(property_path: Path, digest: str, classification_digest: str, temp: Path) -> None:
    """Compile and execute a temporary Java adapter against target/classes and the real dependency graph."""
    # Maven treats project.build.directory as a model value and may ignore a user property;
    # copying the project into OS temp is the only reliable way to keep clean/verify out of checkout.
    agent_project = temp / "agent-project"
    shutil.copytree(ROOT / "agent", agent_project, ignore=shutil.ignore_patterns("target"))
    # The existing Java tests intentionally locate frozen contracts from user.dir; keep that lookup real.
    shutil.copytree(ROOT / "contracts", agent_project / "contracts")
    pom_file = agent_project / "pom.xml"
    build_directory = agent_project / "target"
    build_result = run_command(
        "java-build",
        [tool("mvn"), "-B", "-ntp", "-f", str(pom_file), "clean", "verify"],
        ROOT,
    )
    require_marker("java-build", build_result.stdout + build_result.stderr, r"Tests run:\s*221, Failures:\s*0, Errors:\s*0")
    classpath_file = temp / "java-classpath.txt"
    run_command(
        "java-classpath",
        [
            tool("mvn"),
            "-B",
            "-ntp",
            "-f",
            str(pom_file),
            "dependency:build-classpath",
            f"-Dmdep.outputFile={classpath_file}",
        ],
        ROOT,
    )
    adapter_source = CONTRACT / "JavaCorpusProbe.java"
    classes = temp / "java-classes"
    classes.mkdir()
    classpath = os.pathsep.join([str(build_directory / "classes"), classpath_file.read_text(encoding="utf-8").strip()])
    run_command(
        "java-compile",
        ["javac", "-encoding", "UTF-8", "-cp", classpath, "-d", str(classes), str(adapter_source)],
        ROOT,
    )
    env = os.environ.copy()
    env.update({
        "JA_CONTRACT_DIGEST": digest,
        "JA_PROPERTY_PATH": str(property_path),
        "JA_PROPERTY_DIGEST": classification_digest,
        "JA_GOLDEN_PATH": str(GOLDEN),
    })
    result = run_command(
        "java-adapter",
        [
            "java",
            "-cp",
            os.pathsep.join([str(classes), classpath]),
            "io.github.kongweiguang.ja.protocol.JavaCorpusProbe",
            str(GOLDEN),
        ],
        ROOT,
        env=env,
    )
    require_marker(
        "java-adapter",
        result.stdout + result.stderr,
        rf"JAVA_CONTRACT_OK digest={digest} validFrames={EXPECTED['validFrames']} methodResults={EXPECTED['methodResults']} parseFrames={EXPECTED['parseFrames']} propertyValid=100 propertyInvalid=100 propertyDigest={classification_digest}",
    )
    java_filters = (
        ("full-duplex", "io.github.kongweiguang.ja.protocol.JsonlCodecTest#acceptsOnlyTheCorrectRequestAndResponseDirectionMatrix"),
        ("pending", "io.github.kongweiguang.ja.protocol.JsonlCodecTest#pendingResponsesAreConsumedExactlyOnce"),
        ("cancel-race", "io.github.kongweiguang.ja.protocol.JsonlCodecTest#pendingAcceptAndCancelShareOneLinearizableStateBoundary"),
        ("ready-terminal", "io.github.kongweiguang.ja.protocol.HandshakeProtocolTest#handshakeRejectsPreReadyDuplicateAndStaleGenerationUse"),
    )
    for filter_id, selector in java_filters:
        run_java_filter(filter_id, selector, pom_file)
    print(f"JAVA_OK digest={digest}")


def run_java_filter(filter_id: str, selector: str, pom_file: Path) -> None:
    """Re-run focused Java concurrency boundaries so a broad green suite cannot hide a skipped test."""
    result = run_command(
        f"java-filter-{filter_id}",
        [
            tool("mvn"),
            "-B",
            "-ntp",
            "-f",
            str(pom_file),
            "-Dtest=" + selector,
            "test",
        ],
        ROOT,
    )
    require_marker(
        f"java-filter-{filter_id}",
        result.stdout + result.stderr,
        r"Tests run:\s*[1-9][0-9]*, Failures:\s*0, Errors:\s*0",
    )
    print(f"JAVA_FILTER_OK id={filter_id}")


def prepare_rust_sidecar_scenarios(temp: Path) -> tuple[Path, Path]:
    """Create OS-temp sidecar cases so the Rust adapter can exercise production Supervisor over real stdio."""
    scenario_dir = temp / "rust-sidecar-scenarios"
    scenario_dir.mkdir()
    handshake = [
        json.loads(line)
        for line in (GOLDEN / "valid" / "handshake.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    (scenario_dir / "valid.json").write_text(
        json.dumps({"mode": "valid", "frames": handshake}, ensure_ascii=False),
        encoding="utf-8",
        newline="\n",
    )
    (scenario_dir / "minor.json").write_text(
        json.dumps({"mode": "minor"}, ensure_ascii=False),
        encoding="utf-8",
        newline="\n",
    )
    invalid_cases = [
        json.loads(line)
        for line in (GOLDEN / "invalid" / "handshake-challenge.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    for index, case in enumerate(invalid_cases):
        (scenario_dir / f"case-{index:02}.json").write_text(
            json.dumps(
                {"mode": "invalid", "case": case["case"], "frames": case["frames"]},
                ensure_ascii=False,
            ),
            encoding="utf-8",
            newline="\n",
        )
    sidecar = temp / "rust-contract-sidecar.py"
    sidecar.write_text(
        '''import copy
import json
import pathlib
import sys


def read_frame():
    """Read strict UTF-8 bytes so Windows text mode cannot rewrite protocol framing."""
    raw = sys.stdin.buffer.readline()
    if not raw:
        return None
    try:
        line = raw.rstrip(b"\\n").decode("utf-8", errors="strict")
        return json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError):
        sys.exit(9)


def write_frame(frame):
    """Write one exact LF-delimited UTF-8 frame so production codec sees the frozen wire format."""
    payload = json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(payload + b"\\n")
    sys.stdout.buffer.flush()


def result_frame(request_id, result):
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def ready_frame(token):
    return {
        "jsonrpc": "2.0",
        "method": "runtime/statusChanged",
        "params": {
            "serverInstanceId": "srv_contract",
            "eventId": "evt_contract_ready",
            "occurredAt": "2026-08-16T00:00:00Z",
            "status": "ready",
            "readyToken": token,
        },
    }


def handshake_result(minor):
    return {
        "protocolMajor": 1,
        "protocolMinor": minor,
        "minimumCompatibleMinor": 0,
        "serverVersion": "ja-agent-fixture",
        "serverInstanceId": "srv_contract",
        "runtime": {"kind": "native-image", "agentScopeVersion": "2.0.2", "javaVersion": "25"},
        "capabilities": {
            "methods": [],
            "events": [],
            "accessModes": ["read_only", "workspace", "full_access"],
            "itemKinds": [],
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
    }


def replace_ready_token(value, token):
    if isinstance(value, dict):
        return {key: replace_ready_token(child, token) for key, child in value.items()}
    if isinstance(value, list):
        return [replace_ready_token(child, token) for child in value]
    if isinstance(value, str) and len(value) == 32 and all(char in "0123456789abcdef" for char in value):
        return token
    return value


scenario = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
initialize = read_frame()
if initialize is None or not isinstance(initialize.get("id"), str):
    sys.exit(2)
request_id = initialize["id"]
mode = scenario.get("mode")

if mode == "valid":
    ready_sent = False
    for fixture in scenario.get("frames", []):
        if fixture.get("method") == "runtime/statusChanged" and fixture.get("params", {}).get("status") == "starting":
            write_frame(fixture)
    write_frame(result_frame(request_id, handshake_result(0)))
    initialized = read_frame()
    token = initialized.get("params", {}).get("readyToken") if initialized else None
    if not isinstance(token, str):
        sys.exit(3)
    for fixture in scenario.get("frames", []):
        status = fixture.get("params", {}).get("status")
        if fixture.get("method") == "runtime/statusChanged" and status == "ready" and not ready_sent:
            ready = replace_ready_token(fixture, token)
            ready["params"]["serverInstanceId"] = "srv_contract"
            write_frame(ready)
            ready_sent = True
        elif fixture.get("method") == "runtime/statusChanged" and status == "stopped":
            write_frame(fixture)
elif mode == "minor":
    params = initialize.get("params", {})
    if params.get("protocolMinor") != 1 or params.get("futureOptionalField") != "old-client-ignores-this":
        sys.exit(4)
    result = handshake_result(1)
    result["futureOptionalField"] = "server-optional-extension"
    write_frame(result_frame(request_id, result))
    initialized = read_frame()
    token = initialized.get("params", {}).get("readyToken") if initialized else None
    if not isinstance(token, str):
        sys.exit(5)
    write_frame(ready_frame(token))
elif mode == "invalid":
    case = scenario.get("case", "")
    frames = scenario.get("frames", [])
    if case.startswith("error_"):
        write_frame(result_frame(request_id, handshake_result(0)))
        initialized = read_frame()
        token = initialized.get("params", {}).get("readyToken") if initialized else None
        if not isinstance(token, str):
            sys.exit(6)
        write_frame(ready_frame(token))
        probe = read_frame()
        if probe is None or not isinstance(probe.get("id"), str) or not frames:
            sys.exit(7)
        error_frame = copy.deepcopy(frames[-1])
        error_frame["id"] = probe["id"]
        write_frame(error_frame)
    elif frames and isinstance(frames[0], dict) and "error" in frames[0]:
        error_frame = copy.deepcopy(frames[0])
        error_frame["id"] = request_id
        write_frame(error_frame)
    else:
        write_frame(result_frame(request_id, handshake_result(0)))
        initialized = read_frame()
        for fixture in frames:
            write_frame(fixture)
else:
    sys.exit(8)

while True:
    request = read_frame()
    if request is None:
        break
    if request.get("method") == "shutdown" and isinstance(request.get("id"), str):
        write_frame(result_frame(request["id"], {"accepted": True, "status": "shutting_down", "deadlineMs": 1000}))
        break
''',
        encoding="utf-8",
        newline="\n",
    )
    return sidecar, scenario_dir


def run_rust(property_path: Path, digest: str, classification_digest: str, temp: Path) -> None:
    """Run locked Rust evidence and compile an explicitly declared temp adapter so Cargo lockfile generation does not rely on auto-discovery."""
    supervisor_replay_only = os.environ.get("JA_RUST_SUPERVISOR_REPLAY_ONLY") == "1"
    cargo_target = temp / "cargo-target"
    cargo_env = os.environ.copy()
    cargo_env["CARGO_TARGET_DIR"] = str(cargo_target)
    run_command(
        "rust-build",
        [
            tool("cargo"),
            "test",
            "--manifest-path",
            "src-tauri/Cargo.toml",
            "--locked",
            "--test",
            "agent_process_contract",
            "--no-run",
        ],
        ROOT,
        env=cargo_env,
    )
    if not rust_test_artifact_exists(cargo_target):
        raise GateFailure("FAIL stage=rust-build classification=missing_artifact")
    print("RUST_BUILD_OK artifact=present")
    result = run_command(
        "rust-tests",
        [
            tool("cargo"),
            "test",
            "--manifest-path",
            "src-tauri/Cargo.toml",
            "--locked",
            "--test",
            "agent_process_contract",
            "--",
            "--nocapture",
        ],
        ROOT,
        env=cargo_env,
    )
    output = result.stdout + result.stderr
    require_marker(
        "rust-tests",
        output,
        rf"test result: ok\. {expected_rust_contract_tests()} passed; 0 failed",
    )
    print(f"RUST_TESTS_OK tests={expected_rust_contract_tests()}")
    rust_filters = (
        ("nested", "session_full_duplex_keeps_reader_alive_for_nested_server_request"),
        ("pending", "pending64_deadline_late_duplicate_and_bounded_tombstones"),
        ("cancel", "session_close_during_wait_wakes_request_without_sleep"),
        ("ready-terminal-race", "ready_promotion_gate_rejects_duplicate_before_lifecycle_mark"),
    )
    for filter_id, selector in rust_filters:
        filtered = run_command(
            f"rust-filter-{filter_id}",
            [
                tool("cargo"),
                "test",
                "--manifest-path",
                "src-tauri/Cargo.toml",
                "--locked",
                "--test",
                "agent_process_contract",
                selector,
                "--",
                "--exact",
                "--nocapture",
            ],
            ROOT,
            env=cargo_env,
        )
        require_marker(
            f"rust-filter-{filter_id}",
            filtered.stdout + filtered.stderr,
            r"test result: ok\. 1 passed; 0 failed",
        )
        print(f"RUST_FILTER_OK id={filter_id}")
    sidecar_script, scenario_dir = prepare_rust_sidecar_scenarios(temp)
    codec_dir = temp / "agent_process"
    codec_dir.mkdir()
    (codec_dir / "mod.rs").write_text(
        "pub mod codec;\n"
        "pub mod error;\n"
        "pub mod handshake;\n"
        "pub mod lifecycle;\n"
        "pub mod pending;\n"
        "pub mod process_tree;\n"
        "pub mod session;\n"
        "pub mod supervisor;\n"
        "pub use lifecycle::LifecycleState;\n"
        "pub use session::{SessionEvent, Session};\n"
        "pub use supervisor::{SidecarConfig, SidecarSupervisor};\n"
        "use serde_json::Value;\n"
        "pub fn validate_initialize_params(value: &Value) -> Result<(), error::AgentProcessError> {\n"
        "    handshake::validate_initialize_params(value, &codec::Limits::default())\n"
        "}\n"
        "pub fn is_ready_notification(frame: &codec::RpcFrame, expected: Option<&str>) -> bool {\n"
        "    handshake::is_ready_notification(frame, expected)\n"
        "}\n",
        encoding="utf-8",
        newline="\n",
    )
    for name in (
        "codec.rs",
        "codec_catalog.rs",
        "codec_json.rs",
        "error.rs",
        "handshake.rs",
        "lifecycle.rs",
        "pending.rs",
        "process_tree.rs",
        "session.rs",
        "wire.rs",
        "events.rs",
        "supervisor.rs",
        "client.rs",
        "config.rs",
        "process.rs",
    ):
        shutil.copy2(ROOT / "src-tauri" / "src" / "agent_process" / name, codec_dir / name)
    source = (CONTRACT / "rust_consumer.rs").read_text(encoding="utf-8").replace(
        "__PROPERTY_PATH__", str(property_path).replace("\\", "\\\\")
    ).replace("__GOLDEN_PATH__", str(GOLDEN).replace("\\", "\\\\"))
    source_root = temp / "src"
    source_root.mkdir()
    source_path = source_root / "main.rs"
    source_path.write_text(source, encoding="utf-8", newline="\n")
    for path in (codec_dir,):
        target = source_root / path.name
        if target != path:
            shutil.copytree(path, target)
    manifest = temp / "Cargo.toml"
    manifest.write_text(
        """[package]\nname = \"ja-contract-consumer\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"ja-contract-consumer\"\npath = \"src/main.rs\"\n\n[dependencies]\nserde = { version = \"=1.0.229\", features = [\"derive\"] }\nserde_json = \"=1.0.151\"\nsha2 = \"=0.11.0\"\ntracing = \"=0.1.44\"\n""",
        encoding="utf-8",
        newline="\n",
    )
    run_command(
        "rust-adapter-lock",
        [tool("cargo"), "generate-lockfile", "--offline", "--manifest-path", str(manifest)],
        ROOT,
        env=cargo_env,
    )
    env = cargo_env.copy()
    env["JA_CONTRACT_DIGEST"] = digest
    env["JA_PROPERTY_PATH"] = str(property_path)
    env["JA_PROPERTY_DIGEST"] = classification_digest
    env["JA_RUST_SIDECAR_EXECUTABLE"] = sys.executable
    env["JA_RUST_SIDECAR_SCRIPT"] = str(sidecar_script)
    env["JA_RUST_SIDECAR_SCENARIOS"] = str(scenario_dir)
    if supervisor_replay_only:
        # Keep the directed diagnostic on the same temporary adapter while isolating
        # production Supervisor replay from Java/TypeScript gate stages.
        env["JA_RUST_SUPERVISOR_REPLAY_ONLY"] = "1"
    result = run_command(
        "rust-adapter",
        [tool("cargo"), "run", "--offline", "--manifest-path", str(manifest), "--locked", "--quiet"],
        ROOT,
        env=env,
    )
    if supervisor_replay_only:
        require_marker(
            "rust-adapter-replay",
            result.stdout + result.stderr,
            r"RUST_SUPERVISOR_REPLAY_OK validFrames=6 invalidCases=23 minorCompatible=1",
        )
        print("RUST_SUPERVISOR_REPLAY_OK validFrames=6 invalidCases=23 minorCompatible=1")
        return
    require_marker(
        "rust-adapter",
        result.stdout + result.stderr,
        rf"RUST_CONTRACT_OK digest={digest} validFrames={EXPECTED['validFrames']} methodResults={EXPECTED['methodResults']} parseFrames={EXPECTED['parseFrames']} propertyValid=100 propertyInvalid=100 propertyDigest={classification_digest}",
    )
    print(f"RUST_OK digest={digest}")


def write_vitest_wrapper(temp: Path) -> Path:
    """Overlay the production Vitest config with an OS-temp cache without mutating repository config."""
    cache_directory = (temp / "vitest-cache").as_posix()
    source_directory = (ROOT / "src").as_posix()
    wrapper = temp / "vitest-root.config.ts"
    wrapper.write_text(
        "import tailwindcss from \"@tailwindcss/vite\";\n"
        "import react from \"@vitejs/plugin-react\";\n"
        "import { defineConfig } from \"vitest/config\";\n"
        "export default defineConfig({\n"
        "  plugins: [react(), tailwindcss()],\n"
        f"  cacheDir: {json.dumps(cache_directory)},\n"
        f"  resolve: {{ alias: {{ \"@\": {json.dumps(source_directory)} }} }},\n"
        "  test: { environment: \"jsdom\", include: [\"src/**/*.test.{ts,tsx}\"], clearMocks: true, restoreMocks: true, unstubGlobals: true, css: true },\n"
        "});\n",
        encoding="utf-8",
        newline="\n",
    )
    return wrapper


def run_typescript(property_path: Path, digest: str, classification_digest: str, temp: Path) -> None:
    """Run the complete TS suite, focused IPC filters, and the shared-corpus adapter through pnpm/Vitest."""
    env = os.environ.copy()
    env.update({
        "JA_CONTRACT_DIGEST": digest,
        "JA_PROPERTY_PATH": str(property_path),
        "JA_PROPERTY_DIGEST": classification_digest,
        "JA_GOLDEN_PATH": str(GOLDEN),
        "JA_VITEST_CACHE_DIR": str(temp / "vitest-cache"),
        "JA_VITEST_FULL_SUITE": "1",
        "NODE_PATH": str(ROOT / "node_modules"),
    })
    # Keep config resolution inside the repository so Vite's runner loader can
    # resolve workspace plugins without generating node_modules/.vite-temp.
    root_config = CONTRACT / "vitest.config.ts"
    suite_env = env.copy()
    suite_env["JA_VITEST_FULL_SUITE"] = "1"
    suite = run_command(
        "typescript-suite",
        [tool("pnpm"), "exec", "vitest", "run", "--config", str(root_config), "--configLoader", "runner", "--reporter=verbose"],
        ROOT,
        env=suite_env,
    )
    require_marker("typescript-suite", suite.stdout + suite.stderr, r"Tests\s+138 passed\s+\(138\)")
    print("TS_SUITE_OK tests=138")
    ts_filters = (
        ("nested-pending", "tracks server request pending IDs and rejects duplicate, unknown, and late responses"),
        ("cancel-race", "linearizes slow disconnect and reconnect so stale listeners cannot win"),
        ("ready-terminal-race", "rejects wrong, duplicate, and stale ready echoes with one stable fault"),
    )
    for filter_id, selector in ts_filters:
        filtered = run_command(
            f"typescript-filter-{filter_id}",
            [
                tool("pnpm"),
                "exec",
                "vitest",
                "run",
                "--config",
                str(root_config),
                "--configLoader",
                "runner",
                "--reporter=verbose",
                "-t",
                selector,
            ],
            ROOT,
            env=suite_env,
        )
        require_marker(
            f"typescript-filter-{filter_id}",
            filtered.stdout + filtered.stderr,
            r"Tests\s+1 passed(?:\s+\|\s+[0-9]+\s+skipped)?\s+\(138\)",
        )
        print(f"TS_FILTER_OK id={filter_id}")
    contract_env = env.copy()
    contract_env["JA_VITEST_FULL_SUITE"] = "0"
    result = run_command(
        "typescript-adapter",
        [
            tool("pnpm"),
            "exec",
            "vitest",
            "run",
            "tests/contract/ts_consumer.test.ts",
            "--config",
            "tests/contract/vitest.config.ts",
            "--configLoader",
            "runner",
            "--reporter=verbose",
        ],
        ROOT,
        env=contract_env,
    )
    require_marker(
        "typescript-adapter",
        result.stdout + result.stderr,
        rf"TS_CONTRACT_OK digest={digest} validFrames={EXPECTED['validFrames']} methodResults={EXPECTED['methodResults']} parseFrames={EXPECTED['parseFrames']} propertyValid=100 propertyInvalid=100 propertyDigest={classification_digest}",
    )
    print(f"TS_OK digest={digest}")


def verify_process_tree_cleanup() -> None:
    """Prove multiple root descendants are reaped under one bounded cleanup deadline."""
    child_script = "import time; time.sleep(30)"
    root_script = (
        "import subprocess,sys,time; "
        f"[subprocess.Popen([sys.executable, '-c', {child_script!r}], stdout=sys.stdout, stderr=sys.stderr) for _ in range(8)]; "
        "time.sleep(0.2)"
    )
    started = time.monotonic()
    run_command(
        "process-tree-self-test",
        [sys.executable, "-c", root_script],
        ROOT,
        timeout=10,
    )
    elapsed = time.monotonic() - started
    if elapsed > PROCESS_CLEANUP_TIMEOUT_SECONDS + 2:
        raise GateFailure("FAIL stage=process-tree-self-test classification=cleanup_deadline")
    print("PROCESS_TREE_OK root_exit_descendant_reaped=true descendants=8")


def main() -> int:
    """Keep stage order explicit so no implementation can pass without the frozen reference and all three consumers."""
    os.environ.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    if platform.python_version_tuple()[0] != "3":
        raise GateFailure("FAIL stage=runner classification=python3_required")
    require_jdk25()
    require_native_toolchain()
    verify_process_tree_cleanup()
    verify_artifact_snapshot_semantics()
    load_expected_counts()
    digest = corpus_digest(GOLDEN)
    remove_empty_vite_temp()
    before_artifacts = repo_artifact_snapshot()
    temporary = tempfile.TemporaryDirectory(prefix="ja-contract-")
    pending_error: BaseException | None = None
    try:
        temp = Path(temporary.name)
        property_path = make_property_corpus(temp)
        classification_digest = property_digest(property_path)
        run_java(property_path, digest, classification_digest, temp)
        run_rust(property_path, digest, classification_digest, temp)
        run_typescript(property_path, digest, classification_digest, temp)
    except BaseException as error:
        pending_error = error
    finally:
        try:
            temporary.cleanup()
        except BaseException as error:
            if pending_error is None:
                pending_error = GateFailure(
                    f"FAIL stage=cleanup classification=cleanup_failed outputHash={safe_digest(type(error).__name__.encode('ascii'))}"
                )
        try:
            remove_empty_vite_temp()
        except BaseException as error:
            if pending_error is None:
                pending_error = error
        try:
            assert_no_repo_artifacts(before_artifacts)
        except BaseException as error:
            if pending_error is None:
                pending_error = error
    if pending_error is not None:
        if isinstance(pending_error, GateFailure):
            raise pending_error
        if isinstance(pending_error, KeyboardInterrupt):
            raise GateFailure("FAIL stage=runner classification=interrupted") from pending_error
        raise GateFailure(
            f"FAIL stage=runner classification={type(pending_error).__name__} "
            f"outputHash={safe_digest(type(pending_error).__name__.encode('ascii', errors='ignore'))}"
        ) from pending_error
    print(f"CONTRACT_GATE_OK digest={digest} seed={PROPERTY_SEED} propertyValid=100 propertyInvalid=100 propertyDigest={classification_digest}")
    return 0


if __name__ == "__main__":
    exit_code = 0
    try:
        exit_code = main()
    except GateFailure as error:
        print(str(error), file=sys.stderr)
        exit_code = 1
    except KeyboardInterrupt:
        print("FAIL stage=runner classification=interrupted", file=sys.stderr)
        exit_code = 130
    except Exception as error:
        # Do not print exception text: it can contain a temp path, token, or compiler payload.
        classification = type(error).__name__.replace(" ", "_")
        print(f"FAIL stage=runner classification={classification}", file=sys.stderr)
        exit_code = 1
    raise SystemExit(exit_code)
