// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import java.util.Objects;

/**
 * A deliberately small exception carrying only a stable public failure code.
 * The sidecar must not turn parser/provider exceptions into wire-visible secrets.
 */
public final class ProtocolException extends RuntimeException {
    private final JaErrorCode code;
    private final String diagnosticId;

    /** Creates a redacted protocol failure without exposing parser details. */
    public ProtocolException(JaErrorCode code) {
        this(code, null, null);
    }

    /** Creates a failure with an optional opaque diagnostic handle. */
    public ProtocolException(JaErrorCode code, String diagnosticId) {
        this(code, diagnosticId, null);
    }

    /** Retains the cause for local logs while constraining the public message. */
    public ProtocolException(JaErrorCode code, String diagnosticId, Throwable cause) {
        super(Objects.requireNonNull(code, "code").publicMessage(), cause);
        this.code = code;
        this.diagnosticId = safeDiagnosticId(diagnosticId);
    }

    /** Returns the stable error code intended for the wire. */
    public JaErrorCode code() {
        return code;
    }

    /** Returns an optional opaque diagnostic handle, never a raw exception message. */
    public String diagnosticId() {
        return diagnosticId;
    }

    /** Converts the failure into a redacted JSON-RPC error object. */
    public RpcError toRpcError() {
        return RpcError.of(code, diagnosticId);
    }

    /** Keeps exception debug output independent of diagnostic handles and parser/provider causes. */
    @Override
    public String toString() {
        return "ProtocolException[code=" + code + "]";
    }

    /** Drops untrusted diagnostics rather than allowing paths or provider text onto the wire. */
    private static String safeDiagnosticId(String value) {
        if (value == null || value.isBlank() || value.length() > 101
                || !value.matches("^diag_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$")) {
            return null;
        }
        return value;
    }
}
