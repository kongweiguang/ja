# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

"""Stage one already-built Native Image executable for the Tauri resource bundle.

The script is intentionally a file-and-manifest adapter only.  Native Image compilation,
protocol validation, and Tauri bundle configuration remain owned by their existing tools; this
step gives CI and local packaging one deterministic ``sidecars/ja-agent-<target-triple>`` layout.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
from typing import Any


TARGET_TRIPLE_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
COMMIT_PATTERN = re.compile(r"^[0-9a-fA-F]{7,64}$")


def sha256(path: Path) -> str:
    """Hash the source and staged file in chunks so large native binaries stay bounded in memory."""

    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_entry(path: Path) -> dict[str, Any]:
    """Return portable artifact facts without serializing a host-specific absolute path."""

    if not path.is_file():
        raise RuntimeError(f"native artifact is missing: {path}")
    return {
        "fileName": path.name,
        "sizeBytes": path.stat().st_size,
        "sha256": sha256(path),
    }


def validate_identity(target_triple: str, source_commit: str) -> None:
    """Reject path-like target identities before they can escape the requested staging root."""

    if not TARGET_TRIPLE_PATTERN.fullmatch(target_triple):
        raise RuntimeError("target triple contains an unsafe character")
    if not COMMIT_PATTERN.fullmatch(source_commit):
        raise RuntimeError("source commit must be a short or full hexadecimal commit id")


def sidecar_file_name(target_triple: str) -> str:
    """Use the same target-triple naming contract consumed by the Rust resource resolver."""

    suffix = ".exe" if target_triple.endswith("-windows-msvc") else ""
    return f"ja-agent-{target_triple}{suffix}"


def contained_path(root: Path, child: Path) -> Path:
    """Resolve a destination and prove it remains below the caller-owned staging directory."""

    root_resolved = root.resolve()
    child_resolved = child.resolve()
    try:
        child_resolved.relative_to(root_resolved)
    except ValueError as failure:
        raise RuntimeError("staging destination escaped the requested output directory") from failure
    return child_resolved


def validate_build_report(report_path: Path, source: dict[str, Any], source_commit: str) -> None:
    """Cross-check optional build evidence so staging cannot relabel another commit or binary."""

    try:
        report = json.loads(report_path.read_text(encoding="utf-8"))
        native = report["artifacts"]["nativeExecutable"]
        toolchain = report["toolchain"]
    except (OSError, ValueError, KeyError, TypeError) as failure:
        raise RuntimeError(f"invalid native build report: {report_path.name}") from failure
    if report.get("sourceCommit") != source_commit:
        raise RuntimeError("build report source commit differs from staging commit")
    if report.get("product") != "JA" or toolchain.get("nativeImageOnly") is not True:
        raise RuntimeError("build report is not a JA Native Image report")
    if toolchain.get("noFallback") is not True:
        raise RuntimeError("build report does not prove --no-fallback")
    if native.get("fileName") != source["fileName"] or native.get("sha256") != source["sha256"]:
        raise RuntimeError("build report does not match the native artifact")


def manifest_for(
    source: dict[str, Any],
    staged: dict[str, Any],
    platform: str,
    arch: str,
    target_triple: str,
    source_commit: str,
    mode: str,
) -> dict[str, Any]:
    """Build one portable staging record shared by copy and offline dry-run paths."""

    return {
        "schemaVersion": 1,
        "product": "JA",
        "sourceCommit": source_commit,
        "target": {
            "platform": platform,
            "arch": arch,
            "targetTriple": target_triple,
        },
        "nativeImageOnly": True,
        "noFallback": True,
        "stagingMode": mode,
        "sidecar": {
            "relativePath": f"sidecars/{staged['fileName']}",
            "sourceArtifact": source,
            "stagedArtifact": staged,
        },
    }


def write_manifest(path: Path, manifest: dict[str, Any]) -> None:
    """Atomically publish the manifest so consumers never observe a partially-written JSON file."""

    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")
    temporary.replace(path)


def parse_args() -> argparse.Namespace:
    """Expose only explicit staging inputs; no build or protocol options are duplicated here."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--target-triple", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--build-report", type=Path)
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="validate identity/report and print the expected Tauri sidecar without copying files",
    )
    return parser.parse_args()


def main() -> int:
    """Copy one executable, verify optional evidence, and emit a deterministic sidecar manifest."""

    args = parse_args()
    try:
        validate_identity(args.target_triple, args.source_commit)
        source = artifact_entry(args.artifact)
        if args.build_report:
            validate_build_report(args.build_report, source, args.source_commit)

        output_root = args.output_dir.resolve()
        sidecars_root = contained_path(output_root, output_root / "sidecars")
        staged_name = sidecar_file_name(args.target_triple)
        staged_path = contained_path(sidecars_root, sidecars_root / staged_name)
        if args.dry_run:
            expected = {
                "fileName": staged_name,
                "sizeBytes": source["sizeBytes"],
                "sha256": source["sha256"],
            }
            manifest = manifest_for(
                source,
                expected,
                args.platform,
                args.arch,
                args.target_triple,
                args.source_commit,
                "dry-run",
            )
            manifest["tauriBundle"] = {
                "status": "skipped",
                "skippedReason": "offline staging check does not invoke tauri build or platform signing",
            }
            print(json.dumps(manifest, ensure_ascii=False, indent=2))
            return 0

        sidecars_root.mkdir(parents=True, exist_ok=True)
        shutil.copy2(args.artifact, staged_path)
        staged = artifact_entry(staged_path)
        if staged["sha256"] != source["sha256"] or staged["sizeBytes"] != source["sizeBytes"]:
            raise RuntimeError("staged sidecar differs from the native artifact")

        manifest = manifest_for(
            source,
            staged,
            args.platform,
            args.arch,
            args.target_triple,
            args.source_commit,
            "copy",
        )
        write_manifest(output_root / "sidecar-manifest.json", manifest)
    except (OSError, RuntimeError) as failure:
        raise SystemExit(f"cannot stage native sidecar: {failure}") from failure

    print(json.dumps(manifest, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
