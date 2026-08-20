# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

"""Record hashes and unsigned status for one real Tauri bundle directory.

This is an evidence adapter, not a release publisher.  It enumerates actual bundle files after
Tauri finishes, records the explicit ``--no-sign`` contract, and marks notarization as not run so
an unsigned CI smoke cannot be mistaken for a release acceptance gate.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
from typing import Any


EXTENSION_PATTERN = re.compile(r"^\.[A-Za-z0-9]+$")


def sha256(path: Path) -> str:
    """Hash a real bundle artifact in chunks without loading an installer into memory."""

    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_entry(path: Path) -> dict[str, Any]:
    """Describe only the portable file facts needed to reproduce a bundle checksum."""

    if not path.is_file() or path.stat().st_size <= 0:
        raise RuntimeError(f"bundle artifact is missing or empty: {path.name}")
    return {
        "fileName": path.name,
        "sizeBytes": path.stat().st_size,
        "sha256": sha256(path),
        "signatureFile": None,
        "signingStatus": "unsigned",
    }


def load_sidecar_manifest(path: Path, source_commit: str) -> dict[str, Any]:
    """Verify that bundle evidence came from the copy-mode sidecar staged by this workflow."""

    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
        sidecar = manifest["sidecar"]
        staged = sidecar["stagedArtifact"]
    except (OSError, ValueError, KeyError, TypeError) as failure:
        raise RuntimeError(f"invalid sidecar manifest: {path.name}") from failure
    if manifest.get("sourceCommit") != source_commit:
        raise RuntimeError("sidecar manifest source commit differs from bundle evidence")
    if manifest.get("stagingMode") != "copy":
        raise RuntimeError("bundle smoke requires a copy-mode sidecar manifest")
    relative = sidecar.get("relativePath")
    if not isinstance(relative, str) or not isinstance(staged, dict):
        raise RuntimeError("sidecar manifest has no portable artifact identity")
    relative_path = Path(relative)
    if relative_path.is_absolute() or ".." in relative_path.parts:
        raise RuntimeError("sidecar manifest path is not relative")
    staged_path = (path.parent / relative_path).resolve()
    expected_root = path.parent.resolve()
    try:
        staged_path.relative_to(expected_root)
    except ValueError as failure:
        raise RuntimeError("sidecar manifest path escaped its staging directory") from failure
    actual = artifact_entry(staged_path)
    if any(actual[key] != staged.get(key) for key in ("fileName", "sizeBytes", "sha256")):
        raise RuntimeError("sidecar manifest checksum does not match the staged file")
    return {
        "relativePath": relative,
        "fileName": actual["fileName"],
        "sizeBytes": actual["sizeBytes"],
        "sha256": actual["sha256"],
    }


def parse_args() -> argparse.Namespace:
    """Keep evidence inputs explicit so a report cannot silently scan an unrelated directory."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle-dir", required=True, type=Path)
    parser.add_argument("--extension", required=True)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--bundle", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--sidecar-manifest", required=True, type=Path)
    parser.add_argument("--no-sign", action="store_true")
    return parser.parse_args()


def main() -> int:
    """Enumerate actual bundles, write checksums, and preserve explicit unsigned/notarization facts."""

    args = parse_args()
    if not args.no_sign:
        raise SystemExit("--no-sign is required for this non-release bundle smoke")
    if not EXTENSION_PATTERN.fullmatch(args.extension):
        raise SystemExit("bundle extension is unsafe")
    try:
        if not args.bundle_dir.is_dir():
            raise RuntimeError(f"bundle directory is missing: {args.bundle_dir}")
        artifact_paths = sorted(
            (path for path in args.bundle_dir.glob(f"*{args.extension}") if path.is_file()),
            key=lambda path: path.name,
        )
        artifacts = [artifact_entry(path) for path in artifact_paths]
        if not artifacts:
            raise RuntimeError(f"no {args.extension} bundle was produced")
        sidecar = load_sidecar_manifest(args.sidecar_manifest, args.source_commit)
    except (OSError, RuntimeError) as failure:
        raise SystemExit(f"cannot collect Tauri bundle evidence: {failure}") from failure

    command = f"pnpm exec tauri build --ci --no-sign --bundles {args.bundle}"
    report = {
        "schemaVersion": 1,
        "product": "JA",
        "sourceCommit": args.source_commit,
        "target": {"platform": args.platform, "arch": args.arch, "bundle": args.bundle},
        "build": {
            "command": command,
            "noSign": True,
            "signingStatus": "unsigned",
            "notarizationStatus": "not-run",
            "notarizationSkippedReason": "CI bundle smoke is explicitly unsigned and is not a release gate",
        },
        "sidecar": sidecar,
        "artifacts": artifacts,
    }
    args.output_dir.mkdir(parents=True, exist_ok=True)
    (args.output_dir / "tauri-bundle-manifest.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    for artifact in artifacts:
        (args.output_dir / f"{artifact['fileName']}.sha256").write_text(
            f"{artifact['sha256']}  {artifact['fileName']}\n",
            encoding="ascii",
            newline="\n",
        )
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
