// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.Objects;

/** Explicit JSON-RPC notification; notifications never create pending entries. */
public final class RpcNotification implements RpcEnvelope {
    private final String method;
    private final ObjectNode params;
    private final RpcDirection direction;
    private final ObjectNode extensions;

    /** Creates a notification with no extension fields. */
    public RpcNotification(String method, ObjectNode params, RpcDirection direction) {
        this(method, params, direction, JsonSupport.objectNode());
    }

    /** Creates a notification while preserving minor-version extension fields. */
    public RpcNotification(String method, ObjectNode params, RpcDirection direction,
                           ObjectNode extensions) {
        this.method = ProtocolChecks.method(method);
        this.params = Objects.requireNonNull(params, "params").deepCopy();
        UnicodeChecks.tree(this.params);
        this.direction = Objects.requireNonNull(direction, "direction");
        this.extensions = ProtocolChecks.extensions(extensions);
        validateHandshakeShape();
    }

    /** Returns the notification method. */
    public String method() { return method; }
    /** Returns a defensive copy of notification parameters. */
    public ObjectNode params() { return params.deepCopy(); }
    /** Returns the originating side. */
    public RpcDirection direction() { return direction; }
    /** Returns a defensive copy of unknown extension fields. */
    public ObjectNode extensions() { return extensions.deepCopy(); }

    /**
     * Applies the schema's closed handshake DTOs at construction time so a
     * malformed challenge cannot be hidden inside a generic notification.
     */
    private void validateHandshakeShape() {
        if ("initialized".equals(method)) {
            if (direction != RpcDirection.CLIENT_TO_SERVER) {
                throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
            }
            InitializedParams.fromJson(params);
        } else if ("runtime/statusChanged".equals(method)) {
            RuntimeStatusChangedParams.fromJson(params);
            if (direction != RpcDirection.SERVER_TO_CLIENT) {
                throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
            }
        }
    }

    @Override
    /** Builds a fresh object so the writer owns serialized state. */
    public ObjectNode toJson() {
        ObjectNode node = extensions.deepCopy();
        node.put("jsonrpc", "2.0");
        node.put("method", method);
        node.set("params", params.deepCopy());
        return node;
    }
}
