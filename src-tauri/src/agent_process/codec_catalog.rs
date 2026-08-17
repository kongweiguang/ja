// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Frozen v1 error catalog kept separate from JSON framing mechanics.

/// 返回唯一 code/jaCode/retryable/message 映射，阻止任意诊断文本进入 wire。
pub(super) fn catalog_entry(
    code: i64,
    ja_code: &str,
    retryable: bool,
) -> Option<(&'static str, &'static str)> {
    const CATALOG: &[(i64, &str, bool, &str)] = &[
        (-32_001, "INVALID_FRAME", false, "invalid frame"),
        (-32_002, "FRAME_TOO_LARGE", false, "frame too large"),
        (
            -32_003,
            "PROTOCOL_VERSION_UNSUPPORTED",
            false,
            "protocol version unsupported",
        ),
        (-32_004, "NOT_INITIALIZED", false, "not initialized"),
        (-32_005, "ALREADY_INITIALIZED", false, "already initialized"),
        (-32_006, "METHOD_NOT_FOUND", false, "method not found"),
        (-32_007, "INVALID_PARAMS", false, "invalid params"),
        (-32_008, "QUEUE_FULL", true, "queue full"),
        (-32_009, "PENDING_LIMIT", true, "pending limit"),
        (-32_010, "DUPLICATE_REQUEST", false, "duplicate request"),
        (-32_011, "UNKNOWN_REQUEST_ID", false, "unknown request id"),
        (-32_012, "DUPLICATE_RESPONSE", false, "duplicate response"),
        (-32_013, "LATE_RESPONSE", false, "late response"),
        (
            -32_014,
            "REQUEST_DEADLINE_EXCEEDED",
            true,
            "request deadline exceeded",
        ),
        (-32_015, "PAYLOAD_TOO_LARGE", false, "payload too large"),
        (-32_016, "RESYNC_REQUIRED", true, "resync required"),
        (-32_017, "HANDSHAKE_FAILED", false, "handshake failed"),
        (-32_020, "SHUTTING_DOWN", true, "shutting down"),
        (-32_021, "DATA_DIR_IN_USE", false, "data directory in use"),
        (-32_023, "MIGRATION_FAILED", false, "migration failed"),
        (-32_024, "SCHEMA_TOO_NEW", false, "schema too new"),
        (-32_025, "WORKSPACE_NOT_FOUND", false, "workspace not found"),
        (-32_026, "WORKSPACE_UNTRUSTED", false, "workspace untrusted"),
        (-32_028, "CONFLICT", true, "conflict"),
        (-32_029, "THREAD_NOT_FOUND", false, "thread not found"),
        (-32_030, "THREAD_BUSY", true, "thread busy"),
        (-32_031, "THREAD_READ_ONLY", false, "thread read only"),
        (-32_032, "TURN_NOT_FOUND", false, "turn not found"),
        (-32_033, "TURN_NOT_ACTIVE", false, "turn not active"),
        (-32_034, "INVALID_STATE", false, "invalid state"),
        (-32_035, "CANCELLED", false, "cancelled"),
        (-32_036, "BUDGET_EXCEEDED", false, "budget exceeded"),
        (-32_040, "APPROVAL_NOT_FOUND", false, "approval not found"),
        (-32_041, "APPROVAL_EXPIRED", false, "approval expired"),
        (
            -32_042,
            "APPROVAL_ALREADY_RESOLVED",
            false,
            "approval already resolved",
        ),
        (-32_043, "TOOL_DENIED", false, "tool denied"),
        (-32_044, "TOOL_FAILED", false, "tool failed"),
        (-32_046, "PROCESS_TIMEOUT", true, "process timeout"),
        (
            -32_047,
            "PROCESS_OUTPUT_LIMIT",
            false,
            "process output limit",
        ),
        (
            -32_048,
            "EXTERNAL_TOOL_UNSUPPORTED",
            false,
            "external tool unsupported",
        ),
        (-32_050, "SECRET_NOT_FOUND", false, "secret not found"),
        (
            -32_051,
            "SECRET_ACCESS_DENIED",
            false,
            "secret access denied",
        ),
        (-32_052, "MODEL_UNSUPPORTED", false, "model unsupported"),
        (-32_053, "MODEL_UNAVAILABLE", true, "model unavailable"),
        (-32_054, "SKILL_INVALID", false, "skill invalid"),
        (-32_055, "SKILL_UNAVAILABLE", true, "skill unavailable"),
        (
            -32_056,
            "MCP_UNSUPPORTED_AUTH",
            false,
            "mcp unsupported auth",
        ),
        (
            -32_057,
            "MCP_SERVER_UNAVAILABLE",
            true,
            "mcp server unavailable",
        ),
        (
            -32_058,
            "MCP_PROTOCOL_UNSUPPORTED",
            false,
            "mcp protocol unsupported",
        ),
        (-32_059, "MCP_TOOL_NOT_FOUND", false, "mcp tool not found"),
        (-32_060, "MCP_TOOL_FAILED", false, "mcp tool failed"),
        (
            -32_070,
            "CAPABILITY_UNSUPPORTED",
            false,
            "capability unsupported",
        ),
        (-32_071, "AUTH_UNSUPPORTED", false, "auth unsupported"),
        (-32_080, "INTERNAL_ERROR", false, "internal error"),
        (-32_081, "SIDE_CAR_CRASHED", false, "sidecar crashed"),
        (-32_082, "SHUTDOWN_TIMEOUT", false, "shutdown timeout"),
    ];
    CATALOG
        .iter()
        .find(|(entry_code, entry_ja, entry_retryable, _)| {
            *entry_code == code && *entry_ja == ja_code && *entry_retryable == retryable
        })
        .map(|(_, entry_ja, _, message)| (*entry_ja, *message))
}
