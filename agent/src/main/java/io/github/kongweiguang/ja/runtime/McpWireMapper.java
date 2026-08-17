// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.mcp.McpServerDefinition;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Maps the frozen MCP settings DTO into the existing immutable definition.
 *
 * <p>The mapper is intentionally limited to wire shape and transport/auth
 * normalization. Validation of endpoint, collection bounds, and transport
 * matrix remains in {@link McpServerDefinition}; this prevents the settings
 * path from growing a second MCP configuration model.</p>
 */
final class McpWireMapper {
    private static final Set<String> SERVER_FIELDS = Set.of(
            "mcpRevision", "name", "transport", "endpoint", "protocolVersion",
            "args", "env", "headers", "queryParams", "auth", "credentialRef", "enabled");
    private static final Set<String> AUTH_FIELDS = Set.of("kind", "name", "credentialRef");
    private static final Set<String> AUTH_KINDS = Set.of("none", "bearer", "header", "env");

    private McpWireMapper() {
    }

    /**
     * Reads one save payload while keeping legacy HTTP credentialRef compatible
     * with the canonical bearer auth shape. Unknown fields fail before a
     * definition can enter the generation map, so the map remains a frozen DTO
     * rather than an open-ended JSON bag.
     */
    static McpServerDefinition parse(JsonNode node) {
        if (node == null || !node.isObject()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        ObjectNode server = (ObjectNode) node;
        rejectUnknownFields(server, SERVER_FIELDS);
        String revision = requiredText(server, "mcpRevision");
        String name = requiredText(server, "name");
        String transport = requiredText(server, "transport");
        String endpoint = requiredText(server, "endpoint");
        String protocolVersion = requiredText(server, "protocolVersion");
        List<String> args = stringList(server, "args");
        Map<String, String> env = stringMap(server, "env");
        Map<String, String> headers = stringMap(server, "headers");
        Map<String, String> queryParams = stringMap(server, "queryParams");
        if ("stdio".equals(transport) && (server.has("headers") || server.has("queryParams"))) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if ("streamable_http".equals(transport) && (server.has("args") || server.has("env"))) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        JsonNode legacyCredential = optionalField(server, "credentialRef");
        if (legacyCredential != null && !legacyCredential.isTextual()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        String legacyCredentialRef = legacyCredential == null ? null : legacyCredential.textValue();
        JsonNode authNode = optionalField(server, "auth");
        McpServerDefinition.Auth auth = parseAuth(authNode, legacyCredentialRef, transport);
        boolean enabled = booleanValue(server, "enabled", true);

        try {
            return new McpServerDefinition(revision, name, transport, endpoint, protocolVersion,
                    args, env, headers, queryParams, auth, enabled);
        } catch (IllegalArgumentException failure) {
            throw mapDefinitionFailure(failure);
        }
    }

    /**
     * Produces a non-sensitive summary for list/save responses. The opaque
     * credential reference is retained only as an auth reference; the
     * transport marker used internally by McpConfigSupport is never emitted.
     */
    static ObjectNode summary(McpServerDefinition server) {
        ObjectNode result = JsonNodes.object();
        result.put("mcpRevision", server.revision());
        result.put("name", server.name());
        result.put("transport", server.transport());
        result.put("endpoint", server.endpoint());
        result.put("protocolVersion", server.protocolVersion());
        if ("stdio".equals(server.transport())) {
            result.set("args", stringArray(server.args()));
            result.set("env", stringMapNode(server.env()));
        } else {
            result.set("headers", stringMapNode(server.headers()));
            result.set("queryParams", stringMapNode(server.queryParams()));
        }
        result.set("auth", authNode(server.auth()));
        result.put("enabled", server.enabled());
        result.put("status", server.enabled() ? "unavailable" : "disabled");
        result.put("toolCount", 0);
        return result;
    }

    /** Converts one auth value without ever creating a secret-ref:// marker. */
    private static ObjectNode authNode(McpServerDefinition.Auth auth) {
        ObjectNode node = JsonNodes.object();
        node.put("kind", auth.kind());
        if (auth.name() != null) {
            node.put("name", auth.name());
        }
        if (auth.credentialRef() != null) {
            node.put("credentialRef", auth.credentialRef());
        }
        return node;
    }

    /** Copies a string list before the immutable MCP definition applies bounds. */
    private static List<String> stringList(ObjectNode parent, String field) {
        JsonNode value = parent.get(field);
        if (value == null) {
            return List.of();
        }
        if (value.isNull()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (!value.isArray()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        List<String> result = new ArrayList<>();
        for (JsonNode entry : value) {
            if (!entry.isTextual()) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
            result.add(entry.textValue());
        }
        return List.copyOf(result);
    }

    /** Copies one bounded JSON string map while retaining deterministic key order. */
    private static Map<String, String> stringMap(ObjectNode parent, String field) {
        JsonNode value = parent.get(field);
        if (value == null) {
            return Map.of();
        }
        if (value.isNull()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (!value.isObject()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        Map<String, String> result = new LinkedHashMap<>();
        value.fields().forEachRemaining(entry -> {
            if (!entry.getValue().isTextual()) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
            result.put(entry.getKey(), entry.getValue().textValue());
        });
        return Map.copyOf(result);
    }

    /** Applies the explicit auth/transport matrix before definition validation. */
    private static McpServerDefinition.Auth parseAuth(JsonNode authNode,
                                                      String legacyCredentialRef,
                                                      String transport) {
        if (authNode != null && !authNode.isNull() && legacyCredentialRef != null) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if ((authNode == null || authNode.isNull()) && legacyCredentialRef != null) {
            if (!"streamable_http".equals(transport)) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
            return auth("bearer", null, legacyCredentialRef);
        }
        if (authNode == null || authNode.isNull()) {
            return McpServerDefinition.Auth.none();
        }
        if (!authNode.isObject()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        ObjectNode auth = (ObjectNode) authNode;
        rejectUnknownFields(auth, AUTH_FIELDS);
        String kind = requiredText(auth, "kind");
        if (!AUTH_KINDS.contains(kind)) {
            throw new ProtocolException(JaErrorCode.MCP_UNSUPPORTED_AUTH);
        }
        String name = optionalText(auth, "name");
        String credentialRef = optionalText(auth, "credentialRef");
        if ("none".equals(kind) && (name != null || credentialRef != null)) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (!"none".equals(kind) && credentialRef == null) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return auth(kind, name, credentialRef);
    }

    /** Creates the upstream auth value and translates only stable auth failures. */
    private static McpServerDefinition.Auth auth(String kind, String name, String credentialRef) {
        try {
            return new McpServerDefinition.Auth(kind, name, credentialRef);
        } catch (IllegalArgumentException failure) {
            String code = failure.getMessage();
            if (code != null && code.startsWith("mcp_auth_unsupported")) {
                throw new ProtocolException(JaErrorCode.MCP_UNSUPPORTED_AUTH);
            }
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
    }

    /** Maps definition validation strings to the frozen public error taxonomy. */
    private static ProtocolException mapDefinitionFailure(IllegalArgumentException failure) {
        String code = failure.getMessage();
        if (code != null && code.startsWith("mcp_protocol")) {
            return new ProtocolException(JaErrorCode.MCP_PROTOCOL_UNSUPPORTED);
        }
        if (code != null && (code.startsWith("mcp_transport")
                || code.startsWith("mcp_auth_unsupported"))) {
            return new ProtocolException(JaErrorCode.MCP_PROTOCOL_UNSUPPORTED);
        }
        return new ProtocolException(JaErrorCode.INVALID_PARAMS);
    }

    /** Rejects unknown DTO fields so a later extension cannot silently alter auth semantics. */
    private static void rejectUnknownFields(ObjectNode node, Set<String> allowed) {
        node.fieldNames().forEachRemaining(field -> {
            if (!allowed.contains(field)) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
        });
    }

    /** Reads one required text field without copying user data into errors. */
    private static String requiredText(ObjectNode parent, String field) {
        JsonNode value = parent.get(field);
        if (value == null || !value.isTextual() || value.textValue().isBlank()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value.textValue();
    }

    /** Reads an optional text field while preserving omitted/null equivalence. */
    private static String optionalText(ObjectNode parent, String field) {
        JsonNode value = parent.get(field);
        if (value == null) {
            return null;
        }
        if (value.isNull()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (!value.isTextual() || value.textValue().isBlank()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value.textValue();
    }

    /** Reads a boolean with the frozen enabled=true default. */
    private static boolean booleanValue(ObjectNode parent, String field, boolean defaultValue) {
        if (!parent.has(field)) {
            return defaultValue;
        }
        JsonNode value = parent.get(field);
        if (value == null || value.isNull() || !value.isBoolean()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value.booleanValue();
    }

    /** Distinguishes an omitted optional field from explicit null in the frozen DTO. */
    private static JsonNode optionalField(ObjectNode parent, String field) {
        if (!parent.has(field)) {
            return null;
        }
        JsonNode value = parent.get(field);
        if (value == null || value.isNull()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value;
    }

    /** Creates an ordered JSON array from immutable definition values. */
    private static ArrayNode stringArray(List<String> values) {
        ArrayNode result = JsonNodes.array();
        values.forEach(result::add);
        return result;
    }

    /** Creates an object projection without exposing Java implementation maps. */
    private static ObjectNode stringMapNode(Map<String, String> values) {
        ObjectNode result = JsonNodes.object();
        values.forEach(result::put);
        return result;
    }
}
