// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.JsonNode;

import java.util.Objects;

/**
 * One generation's externally supplied handshake challenge. The value is
 * intentionally readable only through the wire DTO path; diagnostics use the
 * redacted {@link #toString()} so a challenge cannot enter logs accidentally.
 */
public final class ReadyToken {
    private static final String PATTERN = "^[0-9a-f]{32}$";
    private final String value;

    /**
     * Validates the Rust-supplied lowercase 128-bit spelling before it can be
     * retained or compared by the handshake state machine.
     */
    public ReadyToken(String value) {
        if (value == null || !value.matches(PATTERN)) {
            throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
        }
        UnicodeChecks.wellFormed(value, "readyToken");
        this.value = value;
    }

    /**
     * Parses a JSON string without accepting numeric, null, uppercase, or
     * differently sized challenge values.
     */
    public static ReadyToken fromJson(JsonNode node) {
        if (node == null || !node.isTextual()) {
            throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
        }
        return new ReadyToken(node.textValue());
    }

    /** Returns the exact value only for the handshake serializer/comparator. */
    public String value() {
        return value;
    }

    /** Prevents a challenge from appearing in logs, exception diagnostics, or record output. */
    @Override
    public String toString() {
        return "ReadyToken[redacted]";
    }

    /** Compares challenges by value so a retry can prove it belongs to this generation. */
    @Override
    public boolean equals(Object other) {
        return other instanceof ReadyToken token && value.equals(token.value);
    }

    /** Keeps set membership stable for current and retired-generation checks. */
    @Override
    public int hashCode() {
        return Objects.hash(value);
    }
}
