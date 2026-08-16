// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.Objects;

/** Explicit JSON-RPC request; id direction is checked before dispatch. */
public final class RpcRequest implements RpcEnvelope {
    private final String id;
    private final String method;
    private final ObjectNode params;
    private final RpcDirection direction;
    private final ObjectNode extensions;

    /** Creates a request with no extension fields. */
    public RpcRequest(String id, String method, ObjectNode params, RpcDirection direction) {
        this(id, method, params, direction, JsonSupport.objectNode());
    }

    /** Creates a request while preserving minor-version extension fields. */
    public RpcRequest(String id, String method, ObjectNode params, RpcDirection direction,
                      ObjectNode extensions) {
        this.id = ProtocolChecks.requestId(id, direction);
        this.method = ProtocolChecks.method(method);
        this.params = Objects.requireNonNull(params, "params").deepCopy();
        UnicodeChecks.tree(this.params);
        this.direction = Objects.requireNonNull(direction, "direction");
        this.extensions = ProtocolChecks.extensions(extensions);
    }

    /** Creates a request originating from the Rust/Tauri client. */
    public static RpcRequest client(String id, String method, ObjectNode params) {
        return new RpcRequest(id, method, params, RpcDirection.CLIENT_TO_SERVER);
    }

    /** Creates a request originating from the Java sidecar. */
    public static RpcRequest server(String id, String method, ObjectNode params) {
        return new RpcRequest(id, method, params, RpcDirection.SERVER_TO_CLIENT);
    }

    /** Returns the bounded request id. */
    public String id() { return id; }
    /** Returns the method selected by dispatch. */
    public String method() { return method; }
    /** Returns a copy so caller mutation cannot alter the pending request. */
    public ObjectNode params() { return params.deepCopy(); }
    /** Returns the origin direction used for id ownership. */
    public RpcDirection direction() { return direction; }
    /** Returns a copy of fields unknown to this minor protocol. */
    public ObjectNode extensions() { return extensions.deepCopy(); }

    /** Returns the exact request id prefix required by this originator. */
    public boolean isClientRequest() { return direction == RpcDirection.CLIENT_TO_SERVER; }

    @Override
    /** Builds a fresh JSON object for the single writer queue. */
    public ObjectNode toJson() {
        ObjectNode node = extensions.deepCopy();
        node.put("jsonrpc", "2.0");
        node.put("id", id);
        node.put("method", method);
        node.set("params", params.deepCopy());
        return node;
    }
}
