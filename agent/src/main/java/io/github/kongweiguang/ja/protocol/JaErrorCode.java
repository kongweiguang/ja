// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import java.util.HashSet;
import java.util.Set;

/** Stable machine-readable failures frozen by the ja-rpc/v1 error contract. */
public enum JaErrorCode {
    INVALID_FRAME(false, -32001, "The protocol frame is invalid."),
    FRAME_TOO_LARGE(false, -32002, "The protocol frame exceeds the negotiated limit."),
    PROTOCOL_VERSION_UNSUPPORTED(false, -32003, "The protocol version is not supported."),
    NOT_INITIALIZED(false, -32004, "The runtime has not been initialized."),
    ALREADY_INITIALIZED(false, -32005, "The runtime has already been initialized."),
    METHOD_NOT_FOUND(false, -32006, "The requested method is not available."),
    INVALID_PARAMS(false, -32007, "The request parameters are invalid."),
    QUEUE_FULL(true, -32008, "The runtime queue is full."),
    PENDING_LIMIT(true, -32009, "The pending request limit was reached."),
    DUPLICATE_REQUEST(false, -32010, "The request id or idempotency key is already in use."),
    UNKNOWN_REQUEST_ID(false, -32011, "The response id is not pending."),
    DUPLICATE_RESPONSE(false, -32012, "The response was already consumed."),
    LATE_RESPONSE(false, -32013, "The response arrived after its deadline or cancellation."),
    REQUEST_DEADLINE_EXCEEDED(true, -32014, "The request deadline was exceeded."),
    PAYLOAD_TOO_LARGE(false, -32015, "The payload exceeds its limit."),
    RESYNC_REQUIRED(true, -32016, "The event stream requires resynchronization."),
    HANDSHAKE_FAILED(false, -32017, "The runtime handshake failed."),

    SHUTTING_DOWN(true, -32020, "The runtime is shutting down."),
    DATA_DIR_IN_USE(false, -32021, "The data directory is already in use."),
    STATE_RECOVERY_REQUIRED(false, -32022, "Runtime state recovery is required."),
    MIGRATION_FAILED(false, -32023, "The data migration failed."),
    SCHEMA_TOO_NEW(false, -32024, "The stored schema is newer than this runtime."),
    WORKSPACE_NOT_FOUND(false, -32025, "The workspace was not found."),
    WORKSPACE_UNTRUSTED(false, -32026, "The workspace is not trusted."),
    WORKSPACE_MUTATION_BUSY(true, -32027, "The workspace mutation lease is busy."),
    CONFLICT(true, -32028, "The requested revision conflicts with current state."),
    THREAD_NOT_FOUND(false, -32029, "The thread was not found."),
    THREAD_BUSY(true, -32030, "The thread already has an active turn."),
    THREAD_READ_ONLY(false, -32031, "The thread is read-only."),
    TURN_NOT_FOUND(false, -32032, "The turn was not found."),
    TURN_NOT_ACTIVE(false, -32033, "The turn is not active."),
    INVALID_STATE(false, -32034, "The state transition is not allowed."),
    CANCELLED(false, -32035, "The operation was cancelled."),
    BUDGET_EXCEEDED(false, -32036, "The operation budget was exceeded."),

    APPROVAL_NOT_FOUND(false, -32040, "The approval was not found."),
    APPROVAL_EXPIRED(false, -32041, "The approval is expired."),
    APPROVAL_ALREADY_RESOLVED(false, -32042, "The approval was already resolved."),
    TOOL_DENIED(false, -32043, "The tool call was denied."),
    TOOL_FAILED(false, -32044, "The tool call failed."),
    SANDBOX_POLICY_UNAVAILABLE(false, -32045, "The sandbox policy is unavailable."),
    PROCESS_TIMEOUT(true, -32046, "The process timed out."),
    PROCESS_OUTPUT_LIMIT(false, -32047, "The process output limit was reached."),
    EXTERNAL_TOOL_UNSUPPORTED(false, -32048, "The external tool is unsupported."),

    SECRET_NOT_FOUND(false, -32050, "The secret was not found."),
    SECRET_ACCESS_DENIED(false, -32051, "Secret access was denied."),
    MODEL_UNSUPPORTED(false, -32052, "The model is unsupported."),
    MODEL_UNAVAILABLE(true, -32053, "The model provider is unavailable."),
    SKILL_INVALID(false, -32054, "The skill is invalid."),
    SKILL_UNAVAILABLE(true, -32055, "The skill is unavailable."),
    MCP_UNSUPPORTED_AUTH(false, -32056, "The MCP authentication mode is unsupported."),
    MCP_SERVER_UNAVAILABLE(true, -32057, "The MCP server is unavailable."),
    MCP_PROTOCOL_UNSUPPORTED(false, -32058, "The MCP protocol is unsupported."),
    MCP_TOOL_NOT_FOUND(false, -32059, "The MCP tool was not found."),
    MCP_TOOL_FAILED(false, -32060, "The MCP tool call failed."),
    ATTACHMENT_NOT_FOUND(false, -32061, "The attachment was not found."),
    ATTACHMENT_TOO_LARGE(false, -32062, "The attachment is too large."),
    ATTACHMENT_TYPE_UNSUPPORTED(false, -32063, "The attachment type is unsupported."),
    ARTIFACT_NOT_FOUND(false, -32064, "The artifact was not found."),

    CAPABILITY_UNSUPPORTED(false, -32070, "The capability is not available."),
    AUTH_UNSUPPORTED(false, -32071, "The authentication mode is unsupported."),
    INTERNAL_ERROR(false, -32080, "The operation could not be completed."),
    SIDE_CAR_CRASHED(false, -32081, "The Java sidecar exited unexpectedly."),
    SHUTDOWN_TIMEOUT(false, -32082, "The runtime shutdown deadline was exceeded.");

    private final boolean retryable;
    private final int wireCode;
    private final String publicMessage;

    /** Validates each explicit code at class initialization so a later edit cannot leave the reserved range. */
    JaErrorCode(boolean retryable, int wireCode, String publicMessage) {
        this.retryable = retryable;
        if (wireCode < -32768 || wireCode > -32000) {
            throw new IllegalArgumentException("wire code is outside JSON-RPC reserved range");
        }
        this.wireCode = wireCode;
        this.publicMessage = publicMessage;
    }

    static {
        Set<Integer> codes = new HashSet<>();
        for (JaErrorCode value : values()) {
            if (!codes.add(value.wireCode)) {
                throw new ExceptionInInitializerError("duplicate stable JA wire code");
            }
        }
    }

    /** Indicates whether the contract permits a safe retry after this failure. */
    public boolean retryable() {
        return retryable;
    }

    /** Returns the explicit frozen JSON-RPC integer, independent of enum ordering. */
    public int wireCode() {
        return wireCode;
    }

    /** Returns a stable message that cannot expose paths, secrets, or stack traces. */
    public String publicMessage() {
        return publicMessage;
    }

    /** Resolves a known frozen wire code so error metadata can be checked for consistency. */
    static JaErrorCode fromWireCode(int wireCode) {
        for (JaErrorCode value : values()) {
            if (value.wireCode == wireCode) {
                return value;
            }
        }
        return null;
    }

    /** Resolves only frozen symbolic names so peer-provided retry metadata cannot invent policy. */
    static JaErrorCode fromName(String name) {
        for (JaErrorCode value : values()) {
            if (value.name().equals(name)) {
                return value;
            }
        }
        return null;
    }
}
