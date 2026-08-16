// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.Objects;
import java.util.Set;

/** Central bounded checks used by all manually constructed wire envelopes. */
final class ProtocolChecks {
    private static final String ID_TAIL = "[A-Za-z0-9][A-Za-z0-9._-]{0,95}";
    private static final Set<String> RESERVED_ENVELOPE_FIELDS = Set.of(
            "jsonrpc", "id", "method", "params", "result", "error");

    /** Prevents construction because these checks are deliberately stateless. */
    private ProtocolChecks() {
    }

    /** Validates the prefix owner so a response cannot be routed to the wrong pipe. */
    static String requestId(String id, RpcDirection direction) {
        Objects.requireNonNull(direction, "direction");
        String value = genericRequestId(id);
        if (!value.startsWith(direction.idPrefix())) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        return value;
    }

    /** Applies the shared bounded id grammar before any direction-specific check. */
    static String genericRequestId(String id) {
        if (id == null || id.length() > 98 || !id.matches("^(c|s):" + ID_TAIL)) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        UnicodeChecks.wellFormed(id, "request id");
        return id;
    }

    /** Bounds method names so dispatcher lookups cannot retain arbitrary input. */
    static String method(String method) {
        if (method == null || method.isBlank() || method.length() > 128
                || method.indexOf('\n') >= 0 || method.indexOf('\r') >= 0) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        UnicodeChecks.wellFormed(method, "method");
        return method;
    }

    /** Rejects extensions that could overwrite envelope semantics during serialization. */
    static ObjectNode extensions(ObjectNode extensions) {
        Objects.requireNonNull(extensions, "extensions");
        for (String name : RESERVED_ENVELOPE_FIELDS) {
            if (extensions.has(name)) {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }
        }
        UnicodeChecks.tree(extensions);
        return extensions.deepCopy();
    }
}
