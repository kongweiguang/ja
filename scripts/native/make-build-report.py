# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

"""Create a reproducible Native Image build report from real CI outputs."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
from typing import Any


VERSION_PATTERN = re.compile(r"(?:\d+\.){2}\d+(?:[+.-][A-Za-z0-9.-]+)?")


def sha256(path: Path) -> str:
    """Hash an actual artifact in bounded chunks so report generation stays memory stable."""

    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def command_version(command: str, *arguments: str) -> str:
    """Capture only version lines, excluding installation paths and arbitrary build output."""

    candidates = [command]
    if command in {"native-image", "mvn"} and not command.endswith(".cmd"):
        candidates.append(f"{command}.cmd")
    executable = next((shutil.which(candidate) for candidate in candidates if shutil.which(candidate)), None)
    if executable is None:
        return "unavailable"
    try:
        completed = subprocess.run(
            [executable, *arguments],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unavailable"
    lines = [line.strip() for line in completed.stdout.splitlines() if line.strip()]
    for line in lines:
        if VERSION_PATTERN.search(line):
            return line[:240]
    return lines[0][:240] if lines else "unavailable"


def file_entry(path: Path) -> dict[str, Any]:
    """Describe a generated file using only its portable name, size, and content checksum."""

    if not path.is_file():
        raise RuntimeError(f"required report input is missing: {path.name}")
    return {"fileName": path.name, "sizeBytes": path.stat().st_size, "sha256": sha256(path)}


def parse_args() -> argparse.Namespace:
    """Parse report inputs without allowing arbitrary host paths into the serialized report."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", required=True, type=Path)
    parser.add_argument("--sbom", required=True, type=Path)
    parser.add_argument("--smoke", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--checksum-output", required=True, type=Path)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--runner", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--nik-version", required=True)
    parser.add_argument("--nik-java-version", required=True)
    parser.add_argument("--nik-sha256", required=True)
    parser.add_argument("--no-fallback", action="store_true")
    return parser.parse_args()


def main() -> int:
    """Validate the three evidence inputs and emit one machine-readable build record."""

    args = parse_args()
    if not args.no_fallback:
        raise SystemExit("--no-fallback is required")
    try:
        smoke = json.loads(args.smoke.read_text(encoding="utf-8"))
        if smoke.get("status") != "passed":
            raise RuntimeError("smoke evidence is not marked passed")
        artifact = file_entry(args.artifact)
        sbom = file_entry(args.sbom)
    except (OSError, ValueError, RuntimeError) as failure:
        raise SystemExit(f"cannot create native build report: {failure}") from failure

    report = {
        "schemaVersion": 1,
        "product": "JA",
        "sourceCommit": args.source_commit,
        "target": {"platform": args.platform, "arch": args.arch, "runner": args.runner},
        "toolchain": {
            "distribution": "BellSoft Liberica Native Image Kit",
            "nikVersion": args.nik_version,
            "javaVersion": args.nik_java_version,
            "java": command_version("java", "-version"),
            "nativeImage": command_version("native-image", "--version"),
            "maven": command_version("mvn", "--version"),
            "nikArchiveSha256": args.nik_sha256.lower(),
            "nativeImageOnly": True,
            "noFallback": True,
        },
        "artifacts": {"nativeExecutable": artifact, "sbom": sbom},
        "smoke": smoke,
        "security": {
            "credentialsRedacted": True,
            "jvmEnvironmentRemoved": True,
            "stdoutProtocolOnly": True,
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")
    args.checksum_output.write_text(
        f"{artifact['sha256']}  {artifact['fileName']}\n", encoding="ascii", newline="\n"
    )
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
