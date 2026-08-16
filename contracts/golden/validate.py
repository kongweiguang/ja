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

HANDSHAKE_METHODS = {"initialized", "runtime/statusChanged"}
NON_READY_STATUSES = {"starting", "degraded", "shutting_down", "stopped", "crashed"}
HANDSHAKE_VALID_FIXTURE = BASE / "valid" / "handshake.jsonl"
HANDSHAKE_INVALID_FIXTURE = BASE / "invalid" / "handshake-challenge.jsonl"
ERRORS_DOC = BASE.parent / "ja-rpc" / "v1" / "errors.md"
ERROR_ROW = re.compile(r"^\|\s*(-?\d+)\s+\|\s+`([A-Z][A-Z0-9_]*)`\s+\|\s+(是|否)\s+\|")


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


def load_error_catalog() -> dict[int, tuple[str, bool]]:
    """从稳定错误表建立唯一 code/jaCode/retryable 映射，防止握手错误被错误码复用。"""
    catalog: dict[int, tuple[str, bool]] = {}
    names: set[str] = set()
    for line_number, line in enumerate(ERRORS_DOC.read_text(encoding="utf-8").splitlines(), start=1):
        match = ERROR_ROW.match(line)
        if match is None:
            continue
        code = int(match.group(1))
        ja_code = match.group(2)
        retryable = match.group(3) == "是"
        if code in catalog:
            raise ValueError(f"{ERRORS_DOC}:{line_number}: duplicate error code {code}")
        if ja_code in names:
            raise ValueError(f"{ERRORS_DOC}:{line_number}: duplicate jaCode {ja_code}")
        catalog[code] = (ja_code, retryable)
        names.add(ja_code)

    if catalog.get(-32017) != ("HANDSHAKE_FAILED", False):
        raise ValueError("HANDSHAKE_FAILED must map to -32017 and retryable=false")
    if catalog.get(-32080) != ("INTERNAL_ERROR", False):
        raise ValueError("INTERNAL_ERROR must map to -32080 and retryable=false")
    return catalog


def validate_error_mapping(document: Any, catalog: dict[int, tuple[str, bool]], label: str) -> None:
    """核对每个 error response 的 code、jaCode 和 retryable，避免错误码语义漂移。"""
    if not isinstance(document, dict) or not isinstance(document.get("error"), dict):
        return
    error = document["error"]
    code = error.get("code")
    data = error.get("data")
    if code not in catalog or not isinstance(data, dict):
        raise ValueError(f"{label}: error is absent from the stable catalog")
    expected_ja_code, expected_retryable = catalog[code]
    actual = (data.get("jaCode"), data.get("retryable"))
    if actual != (expected_ja_code, expected_retryable):
        raise ValueError(
            f"{label}: error mapping mismatch for {code}: expected "
            f"{expected_ja_code}/{expected_retryable}, got {actual[0]}/{actual[1]}"
        )


def validate_frame_redaction(
    document: Any,
    forbidden_tokens: set[str],
    label: str,
    allow_ready_token_path: bool = False,
) -> None:
    """扫描整帧而非仅 error 子树，确保 challenge 不能经 meta、notice 或扩展字段泄漏。"""
    def visit(node: Any, path: tuple[str, ...]) -> None:
        """递归遍历所有 object key/value；仅两个握手字段位置允许 challenge 原值。"""
        if isinstance(node, dict):
            for key, value in node.items():
                key_text = str(key)
                child_path = path + (key_text,)
                legal_ready_path = allow_ready_token_path and child_path == ("params", "readyToken")
                if legal_ready_path:
                    continue
                if key_text == "readyToken" or key_text in forbidden_tokens:
                    raise ValueError(f"{label}: handshake token key leaked at {'.'.join(child_path)}")
                if isinstance(value, str) and value in forbidden_tokens:
                    raise ValueError(f"{label}: challenge value leaked at {'.'.join(child_path)}")
                visit(value, child_path)
            return
        if isinstance(node, list):
            for index, value in enumerate(node):
                visit(value, path + (f"[{index}]",))
            return
        if isinstance(node, str) and node in forbidden_tokens:
            raise ValueError(f"{label}: challenge value leaked at {'.'.join(path)}")

    visit(document, ())


def validate_handshake_redaction_sequence(frames: list[dict[str, Any]], label: str) -> None:
    """校验握手序列整帧脱敏，只豁免 initialized/ready 的精确 challenge 字段位置。"""
    forbidden_tokens = {
        frame["params"]["readyToken"]
        for frame in frames
        if frame.get("method") == "initialized"
        and isinstance(frame.get("params"), dict)
        and isinstance(frame["params"].get("readyToken"), str)
    }
    if not forbidden_tokens:
        raise ValueError(f"{label}: handshake sequence has no challenge token")
    for index, frame in enumerate(frames, start=1):
        params = frame.get("params")
        allow_ready_token_path = frame.get("method") == "initialized" or (
            frame.get("method") == "runtime/statusChanged"
            and isinstance(params, dict)
            and params.get("status") == "ready"
        )
        validate_frame_redaction(
            frame,
            forbidden_tokens,
            f"{label}:{index}",
            allow_ready_token_path=allow_ready_token_path,
        )


def validate_handshake_sequence(frames: list[dict[str, Any]], label: str) -> None:
    """校验 challenge 的相等性、顺序和 generation 一次性，补足 JSON Schema 无法表达的状态约束。"""
    if not frames:
        raise ValueError(f"{label}: handshake sequence must contain initialized")
    expected_token: str | None = None
    ready_seen = False
    generation_ended = False
    used_tokens: set[str] = set()
    initialized_seen = False

    def finish_generation(index: int) -> None:
        """在 generation 终止或序列结束时强制恰好一个 ready，避免静默停在半握手状态。"""
        if expected_token is not None and not ready_seen:
            raise ValueError(f"{label}:{index}: generation ended without exactly one ready")

    for index, frame in enumerate(frames, start=1):
        method = frame.get("method")
        params = frame.get("params", {})
        if method == "initialized":
            initialized_seen = True
            token = params.get("readyToken")
            if expected_token is not None:
                if not generation_ended:
                    raise ValueError(f"{label}:{index}: duplicate initialized before generation ended")
                finish_generation(index)
            if token in used_tokens:
                raise ValueError(f"{label}:{index}: readyToken reused across generations")
            expected_token = token
            used_tokens.add(token)
            ready_seen = False
            generation_ended = False
            continue

        if method != "runtime/statusChanged":
            continue

        status = params.get("status")
        token = params.get("readyToken")
        if status in NON_READY_STATUSES:
            if "readyToken" in params:
                raise ValueError(f"{label}:{index}: non-ready status carries readyToken")
            if status in {"stopped", "crashed"}:
                finish_generation(index)
                generation_ended = True
            continue

        if status != "ready":
            raise ValueError(f"{label}:{index}: unknown handshake status {status!r}")
        if expected_token is None:
            raise ValueError(f"{label}:{index}: ready arrived before initialized")
        if generation_ended:
            raise ValueError(f"{label}:{index}: ready arrived after generation ended")
        if ready_seen:
            raise ValueError(f"{label}:{index}: duplicate ready status")
        if token != expected_token:
            raise ValueError(f"{label}:{index}: readyToken does not echo initialized challenge")
        ready_seen = True

    if not initialized_seen:
        raise ValueError(f"{label}: handshake sequence must contain initialized")
    finish_generation(len(frames) + 1)


def validate_handshake_cases(
    validator: Draft202012Validator, catalog: dict[int, tuple[str, bool]]
) -> tuple[int, int]:
    """以 golden case 区分 schema、握手状态和错误脱敏错误，避免安全回归被漏检。"""
    valid_frames = load_documents(HANDSHAKE_VALID_FIXTURE)
    for index, frame in enumerate(valid_frames, start=1):
        errors = list(validator.iter_errors(frame))
        if errors:
            raise ValueError(f"{HANDSHAKE_VALID_FIXTURE}:{index}: {errors[0].message}")
    validate_handshake_sequence(valid_frames, str(HANDSHAKE_VALID_FIXTURE))
    validate_handshake_redaction_sequence(valid_frames, str(HANDSHAKE_VALID_FIXTURE))
    for index, frame in enumerate(valid_frames, start=1):
        validate_error_mapping(frame, catalog, f"{HANDSHAKE_VALID_FIXTURE}:{index}")

    invalid_cases = load_documents(HANDSHAKE_INVALID_FIXTURE)
    for index, case in enumerate(invalid_cases, start=1):
        frames = case.get("frames")
        if not isinstance(frames, list) or not frames:
            raise ValueError(f"{HANDSHAKE_INVALID_FIXTURE}:{index}: frames must be non-empty")
        expected_schema_valid = case.get("schemaValid")
        if not isinstance(expected_schema_valid, bool) or case.get("runtimeValid") is not False:
            raise ValueError(f"{HANDSHAKE_INVALID_FIXTURE}:{index}: invalid expectation metadata")
        schema_valid = True
        for frame_index, frame in enumerate(frames, start=1):
            errors = list(validator.iter_errors(frame))
            if errors:
                schema_valid = False
                if expected_schema_valid:
                    raise ValueError(
                        f"{HANDSHAKE_INVALID_FIXTURE}:{index}:{frame_index}: "
                        f"expected schema-valid frame: {errors[0].message}"
                    )
        if schema_valid != expected_schema_valid:
            raise ValueError(
                f"{HANDSHAKE_INVALID_FIXTURE}:{index}: schemaValid expectation mismatch"
            )
        if expected_schema_valid:
            runtime_invalid = False
            try:
                validate_handshake_sequence(frames, f"{HANDSHAKE_INVALID_FIXTURE}:{index}")
                validate_handshake_redaction_sequence(frames, f"{HANDSHAKE_INVALID_FIXTURE}:{index}")
            except ValueError:
                runtime_invalid = True
            for frame_index, frame in enumerate(frames, start=1):
                try:
                    validate_error_mapping(
                        frame, catalog, f"{HANDSHAKE_INVALID_FIXTURE}:{index}:{frame_index}"
                    )
                except ValueError:
                    runtime_invalid = True
            if not runtime_invalid:
                raise ValueError(f"{HANDSHAKE_INVALID_FIXTURE}:{index}: invalid runtime sequence accepted")
    return len(valid_frames), len(invalid_cases)


def validate_valid_fixtures(
    validator: Draft202012Validator, catalog: dict[int, tuple[str, bool]]
) -> tuple[int, int]:
    """同时校验 envelope、response 关联、错误映射和 challenge 脱敏，保持 golden 与实现同一安全边界。"""
    frame_count = 0
    result_count = 0
    for path in sorted(BASE.rglob("*.json*")):
        if path.name == "validate.py" or "invalid" in path.parts or path.name == "major-incompatible.json":
            continue
        pending: dict[str, str] = {}
        documents = load_documents(path)
        handshake_frames = [
            document for document in documents if document.get("method") in HANDSHAKE_METHODS
        ]
        challenge_tokens = {
            document["params"]["readyToken"]
            for document in documents
            if document.get("method") == "initialized"
            and isinstance(document.get("params"), dict)
            and isinstance(document["params"].get("readyToken"), str)
        }
        for index, document in enumerate(documents, start=1):
            root_errors = list(validator.iter_errors(document))
            if root_errors:
                raise ValueError(f"{path}:{index}: {root_errors[0].message}")
            validate_error_mapping(document, catalog, f"{path}:{index}")
            if document.get("method") not in HANDSHAKE_METHODS:
                validate_frame_redaction(document, challenge_tokens, f"{path}:{index}")
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
        if handshake_frames:
            validate_handshake_sequence(handshake_frames, str(path))
            validate_handshake_redaction_sequence(handshake_frames, str(path))
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
    catalog = load_error_catalog()
    frame_count, result_count = validate_valid_fixtures(validator, catalog)
    handshake_valid_count, handshake_invalid_count = validate_handshake_cases(validator, catalog)
    parse_count = validate_parse_only_fixtures()
    markdown_count = validate_markdown_headers()
    validate_no_path_or_secret_leaks()
    print(f"SCHEMA_OK refs={reference_count} validFrames={frame_count} methodResults={result_count}")
    print(
        "HANDSHAKE_OK "
        f"validFrames={handshake_valid_count} invalidCases={handshake_invalid_count} "
        "runtimeInvariant=echo-order-generation"
    )
    print(f"PARSE_ONLY_OK invalidOrMajorFrames={parse_count}")
    print(f"HEADERS_OK markdown={markdown_count}")
    print("PATH_SECRET_SCAN_OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
