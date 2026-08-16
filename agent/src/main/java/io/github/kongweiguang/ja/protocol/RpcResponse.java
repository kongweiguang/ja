// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.Objects;

/**
 * Explicit JSON-RPC response. The resultPresent bit is intentional: JSON null is
 * a valid present result, while an omitted result is an invalid response.
 */
public final class RpcResponse implements RpcEnvelope {
    private final String id;
    private final JsonNode result;
    private final boolean resultPresent;
    private final RpcError error;
    private final ObjectNode extensions;

    /** Creates a response with a present result or an error, never both. */
    public RpcResponse(String id, JsonNode result, RpcError error) {
        this(id, result, true, error, JsonSupport.objectNode());
    }

    /** Creates a response while retaining the distinction between null and absent result. */
    public RpcResponse(String id, JsonNode result, boolean resultPresent, RpcError error,
                       ObjectNode extensions) {
        this.id = ProtocolChecks.genericRequestId(id);
        this.result = result == null ? null : result.deepCopy();
        UnicodeChecks.tree(this.result);
        this.resultPresent = resultPresent;
        this.error = error;
        this.extensions = ProtocolChecks.extensions(extensions);
        if (resultPresent == (error != null) || (!resultPresent && result != null)) {
            throw new IllegalArgumentException("response must contain exactly one of result or error");
        }
    }

    /** Builds a successful response, including a deliberate JSON null result when needed. */
    public static RpcResponse success(String id, JsonNode result) {
        return new RpcResponse(id, result, true, null, JsonSupport.objectNode());
    }

    /** Builds a failure response while retaining no provider-specific diagnostic text. */
    public static RpcResponse failure(String id, RpcError error) {
        return new RpcResponse(id, null, false, Objects.requireNonNull(error, "error"),
                JsonSupport.objectNode());
    }

    /** Creates success using the id of an existing request. */
    public static RpcResponse success(RpcRequest request, JsonNode result) {
        return success(request.id(), result);
    }

    /** Creates a failure using the id of an existing request. */
    public static RpcResponse failure(RpcRequest request, RpcError error) {
        return failure(request.id(), error);
    }

    /** Returns the response id for pending lookup. */
    public String id() { return id; }
    /** Returns a defensive non-null result, or null for JSON null/absent error responses. */
    public JsonNode result() { return result == null || result.isNull() ? null : result.deepCopy(); }
    /** Indicates whether the JSON object contained a result member. */
    public boolean resultPresent() { return resultPresent; }
    /** Returns the redacted error, if this is a failure response. */
    public RpcError error() { return error; }
    /** Returns a defensive copy of unknown extension fields. */
    public ObjectNode extensions() { return extensions.deepCopy(); }

    @Override
    /** Builds a fresh object for strict JSONL serialization. */
    public ObjectNode toJson() {
        ObjectNode node = extensions.deepCopy();
        node.put("jsonrpc", "2.0");
        node.put("id", id);
        if (resultPresent) {
            node.set("result", result == null ? JsonSupport.nullNode() : result.deepCopy());
        } else {
            node.set("error", error.toJson());
        }
        return node;
    }
}
