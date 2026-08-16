// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

/** Redacted, bounded error metadata that is safe to expose to the desktop host. */
public record RpcErrorData(
        String jaCode,
        boolean retryable,
        String diagnosticId,
        String field,
        Long retryAfterMs) {

    /** Validates only bounded diagnostic metadata so provider details cannot leak to the peer. */
    public RpcErrorData {
        if (jaCode == null || !jaCode.matches("[A-Z][A-Z0-9_]{2,63}")) {
            throw new IllegalArgumentException("jaCode must be a stable upper-case code");
        }
        UnicodeChecks.wellFormed(jaCode, "jaCode");
        if (JaErrorCode.fromName(jaCode) == null) {
            throw new IllegalArgumentException("jaCode is not frozen");
        }
        if (diagnosticId != null && !diagnosticId.matches("^diag_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$")) {
            throw new IllegalArgumentException("diagnosticId is not safe");
        }
        if (diagnosticId != null) {
            UnicodeChecks.wellFormed(diagnosticId, "diagnosticId");
        }
        if (field != null && !field.matches("[A-Za-z][A-Za-z0-9_.-]{0,255}")) {
            throw new IllegalArgumentException("field is not safe");
        }
        if (field != null) {
            UnicodeChecks.wellFormed(field, "error field");
        }
        if (retryAfterMs != null && (retryAfterMs < 1 || retryAfterMs > 3_600_000)) {
            throw new IllegalArgumentException("retryAfterMs is out of bounds");
        }
    }

    /** Hides diagnostic handles and field names from debug logs while retaining safe metadata. */
    @Override
    public String toString() {
        return "RpcErrorData[jaCode=" + jaCode + ", retryable=" + retryable
                + ", hasDiagnosticId=" + (diagnosticId != null)
                + ", hasField=" + (field != null) + ", hasRetryAfterMs="
                + (retryAfterMs != null) + "]";
    }
}
