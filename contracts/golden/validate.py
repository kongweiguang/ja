# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later
"""Validate JA protocol golden frames and method-specific result payloads.

The response envelope deliberately has no method field.  This helper therefore keeps the
same pending-id association that Rust and Java must keep at runtime before validating a
successful result against its method-specific schema.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Iterable

from jsonschema import Draft202012Validator, FormatChecker


BASE = Path(__file__).resolve().parent
SCHEMA_PATH = BASE.parent / "ja-rpc" / "v1" / "schema" / "ja-rpc-v1.schema.json"

RESULT_DEFS = {
    "initialize": "initializeResult",
    "version": "versionResult",
    "capabilities/read": "capabilitiesResult",
    "health/read": "healthResult",
    "diagnostics/read": "diagnosticsResult",
    "shutdown": "shutdownResult",
    "workspace/open": "workspaceOpenResult",
    "workspace/list": "workspaceListResult",
    "workspace/trust/set": "workspaceTrustResult",
    "workspace/unregister": "workspaceUnregisterResult",
    "thread/create": "threadCreateResult",
    "thread/list": "threadListResult",
    "thread/read": "threadReadResult",
    "thread/subscribe": "threadSubscribeResult",
    "thread/unsubscribe": "threadUnsubscribeResult",
    "thread/archive": "threadMutationResult",
    "thread/delete": "threadMutationResult",
    "thread/purge": "threadMutationResult",
    "turn/start": "turnStartResult",
    "turn/cancel": "turnCancelResult",
    "turn/steer": "turnSteerResult",
    "turn/followUp": "turnFollowUpResult",
    "profile/list": "profileListResult",
    "profile/read": "profileReadResult",
    "profile/save": "profileSaveResult",
    "profile/activate": "profileActivateResult",
    "model/probe": "modelProbeResult",
    "model/capabilities/read": "modelCapabilitiesResult",
    "skill/list": "skillListResult",
    "skill/import": "skillImportResult",
    "skill/enable": "skillEnableResult",
    "skill/reload": "skillReloadResult",
    "skill/health/read": "skillHealthResult",
    "mcp/list": "mcpListResult",
    "mcp/save": "mcpSaveResult",
    "mcp/delete": "mcpDeleteResult",
    "mcp/test": "mcpTestResult",
    "mcp/reload": "mcpReloadResult",
    "mcp/tools/read": "mcpToolsReadResult",
    "mcp/toolPolicy/set": "mcpToolPolicyResult",
    "attachment/import": "attachmentImportResult",
    "attachment/read": "attachmentReadResult",
    "attachment/delete": "attachmentDeleteResult",
    "approval/request": "approvalResponseResult",
    "secret/resolve": "secretResolveResult",
    "externalTool/request": "externalToolResponseResult",
}


def load_documents(path: Path) -> list[dict[str, Any]]:
    """按 JSONL frame 逐行读取，确保 newline framing 破坏时不会被宽松 JSON 解析掩盖。"""
    if path.suffix == ".jsonl":
        return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    return [json.loads(path.read_text(encoding="utf-8"))]


def validate_local_refs(schema: dict[str, Any]) -> int:
    """提前解析本地 JSON Pointer，避免跨语言实现遇到运行时才发现的断引用。"""
    refs: list[str] = []

    def visit(node: Any) -> None:
        """递归收集 ref 字段，以便在 schema 尚未装载 resolver 时也能检查闭包。"""
        if isinstance(node, dict):
            if "$ref" in node:
                refs.append(node["$ref"])
            for value in node.values():
                visit(value)
        elif isinstance(node, list):
            for value in node:
                visit(value)

    visit(schema)
    for reference in refs:
        if not reference.startswith("#/"):
            raise ValueError(f"non-local schema reference: {reference}")
        node: Any = schema
        for part in reference[2:].split("/"):
            part = part.replace("~1", "/").replace("~0", "~")
            if not isinstance(node, dict) or part not in node:
                raise ValueError(f"missing schema reference: {reference}")
            node = node[part]
    return len(refs)


def validate_result(validator: Draft202012Validator, result: Any, definition: str) -> None:
    """使用根 resolver 校验 method-specific result，避免独立 validator 丢失 `$defs`。"""
    errors = list(validator.descend(result, {"$ref": f"#/$defs/{definition}"}, path="result"))
    if errors:
        raise ValueError(f"result {definition} invalid: {errors[0].message}")


def validate_valid_fixtures(validator: Draft202012Validator) -> tuple[int, int]:
    """同时校验 envelope 和 request-id 关联 result，证明各语言不会各猜一套 response DTO。"""
    frame_count = 0
    result_count = 0
    for path in sorted(BASE.rglob("*.json*")):
        if path.name == "validate.py" or "invalid" in path.parts or path.name == "major-incompatible.json":
            continue
        pending: dict[str, str] = {}
        for index, document in enumerate(load_documents(path), start=1):
            root_errors = list(validator.iter_errors(document))
            if root_errors:
                raise ValueError(f"{path}:{index}: {root_errors[0].message}")
            frame_count += 1
            if "method" in document and "id" in document:
                pending[document["id"]] = document["method"]
                continue
            if "result" not in document or "id" not in document:
                continue
            method = pending.pop(document["id"], None)
            if method is None:
                raise ValueError(f"{path}:{index}: response id has no pending request")
            definition = RESULT_DEFS.get(method)
            if definition is None:
                raise ValueError(f"{path}:{index}: no result mapping for {method}")
            validate_result(validator, document["result"], definition)
            result_count += 1
    return frame_count, result_count


def validate_parse_only_fixtures() -> int:
    """解析 invalid/major 样例但不把故意非法的语义误报成通过。"""
    count = 0
    for path in sorted(BASE.rglob("*.json*")):
        if "invalid" not in path.parts and path.name != "major-incompatible.json":
            continue
        count += len(load_documents(path))
    return count


def validate_markdown_headers() -> int:
    """检查契约说明具备作者和许可证，避免可审查的协议规则变成无归属文本。"""
    count = 0
    for path in Path(__file__).resolve().parents[1].rglob("*.md"):
        text = path.read_text(encoding="utf-8")
        if "@author kongweiguang" not in text or "SPDX-License-Identifier: GPL-3.0-or-later" not in text:
            raise ValueError(f"missing author/SPDX header: {path}")
        count += 1
    return count


def validate_no_path_or_secret_leaks() -> None:
    """扫描 fixture 和说明，防止 golden 将用户路径或可复用凭据带入仓库。"""
    text = "\n".join(
        path.read_text(encoding="utf-8")
        for path in Path(__file__).resolve().parents[1].rglob("*")
        if path.is_file() and path.name != "validate.py" and path.suffix in {".md", ".json", ".jsonl", ".py"}
    )
    patterns = [
        r"(?i)(?:[A-Z]:\\|/Users/|/home/)",
        r"(?i)sk-[A-Za-z0-9]{12,}",
        r"(?i)Bearer\s+[A-Za-z0-9._-]{12,}",
        r"(?i)(?:api[_-]?key|access[_-]?token)\s*[:=]\s*[\"'][^\"']{12,}",
    ]
    for pattern in patterns:
        if re.search(pattern, text):
            raise ValueError(f"possible path/secret leak: {pattern}")


def main() -> int:
    """按协议交付门槛一次性运行 schema、result、parse 和泄漏检查。"""
    schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
    Draft202012Validator.check_schema(schema)
    reference_count = validate_local_refs(schema)
    validator = Draft202012Validator(schema, format_checker=FormatChecker())
    frame_count, result_count = validate_valid_fixtures(validator)
    parse_count = validate_parse_only_fixtures()
    markdown_count = validate_markdown_headers()
    validate_no_path_or_secret_leaks()
    print(f"SCHEMA_OK refs={reference_count} validFrames={frame_count} methodResults={result_count}")
    print(f"PARSE_ONLY_OK invalidOrMajorFrames={parse_count}")
    print(f"HEADERS_OK markdown={markdown_count}")
    print("PATH_SECRET_SCAN_OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
