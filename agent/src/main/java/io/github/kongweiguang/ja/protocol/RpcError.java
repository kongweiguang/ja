// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.Objects;

/** JSON-RPC error object with a stable JA code and redacted diagnostic metadata. */
public record RpcError(int code, String message, RpcErrorData data) {
    /** Rejects malformed or metadata-inconsistent errors before wire serialization. */
    public RpcError {
        if (code < -32768 || code > -32000) {
            throw new IllegalArgumentException("JSON-RPC error code is outside the reserved range");
        }
        if (message == null || message.isBlank() || message.length() > 512
                || message.contains("\n") || message.contains("\r")) {
            throw new IllegalArgumentException("message must be bounded and single-line");
        }
        UnicodeChecks.wellFormed(message, "error message");
        Objects.requireNonNull(data, "data");
        JaErrorCode knownByCode = JaErrorCode.fromWireCode(code);
        JaErrorCode knownByName = JaErrorCode.fromName(data.jaCode());
        if (knownByCode == null || knownByName == null || knownByCode != knownByName
                || knownByCode.retryable() != data.retryable()) {
            throw new IllegalArgumentException("error code and metadata disagree");
        }
    }

    /** Creates an error using only the public message associated with the stable code. */
    public static RpcError of(JaErrorCode code, String diagnosticId) {
        Objects.requireNonNull(code, "code");
        return new RpcError(code.wireCode(), code.publicMessage(),
                new RpcErrorData(code.name(), code.retryable(), diagnosticId, null, null));
    }

    /** Serializes this error without allowing arbitrary provider data onto the wire. */
    public ObjectNode toJson() {
        ObjectNode node = JsonSupport.objectNode();
        node.put("code", code);
        node.put("message", message);
        ObjectNode errorData = node.putObject("data");
        errorData.put("jaCode", data.jaCode());
        errorData.put("retryable", data.retryable());
        if (data.diagnosticId() != null) {
            errorData.put("diagnosticId", data.diagnosticId());
        }
        if (data.field() != null) {
            errorData.put("field", data.field());
        }
        if (data.retryAfterMs() != null) {
            errorData.put("retryAfterMs", data.retryAfterMs());
        }
        return node;
    }

    /** Omits provider text and diagnostic payloads so exception logging cannot leak secrets. */
    @Override
    public String toString() {
        return "RpcError[code=" + code + ", jaCode=" + data.jaCode()
                + ", retryable=" + data.retryable() + ", messageBytes="
                + UnicodeChecks.utf8Bytes(message, "error message") + "]";
    }
}
