// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.Iterator;
import java.util.Map;
import java.util.Objects;

/** Strict DTO for the one challenge-bearing {@code initialized} notification. */
public final class InitializedParams {
    private final ReadyToken readyToken;

    /** Requires a challenge because an empty initialized notification cannot prove a generation. */
    public InitializedParams(ReadyToken readyToken) {
        this.readyToken = Objects.requireNonNull(readyToken, "readyToken");
    }

    /** Converts an externally supplied string through the same strict token validator. */
    public InitializedParams(String readyToken) {
        this(new ReadyToken(readyToken));
    }

    /**
     * Reads the frozen object shape and rejects all extension fields, because
     * an unknown initialized field could become an unaudited handshake input.
     */
    public static InitializedParams fromJson(JsonNode node) {
        if (node == null || !node.isObject()) {
            throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
        }
        ObjectNode object = (ObjectNode) node;
        if (object.size() != 1 || !object.has("readyToken")) {
            throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
        }
        Iterator<Map.Entry<String, JsonNode>> fields = object.properties().iterator();
        if (!fields.hasNext() || !"readyToken".equals(fields.next().getKey())) {
            throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
        }
        return new InitializedParams(ReadyToken.fromJson(object.get("readyToken")));
    }

    /** Returns the challenge for generation comparison without changing it. */
    public ReadyToken readyToken() {
        return readyToken;
    }

    /** Serializes the only permitted initialized parameter without adding metadata. */
    public ObjectNode toJson() {
        return JsonNodes.object().put("readyToken", readyToken.value());
    }

    /** Hides the challenge value from diagnostic output. */
    @Override
    public String toString() {
        return "InitializedParams[readyToken=redacted]";
    }

    /** Compares immutable DTO values for exact retry handling. */
    @Override
    public boolean equals(Object other) {
        return other instanceof InitializedParams params && readyToken.equals(params.readyToken);
    }

    /** Keeps immutable DTO values usable in deterministic state assertions. */
    @Override
    public int hashCode() {
        return readyToken.hashCode();
    }
}
