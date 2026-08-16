// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;

import java.util.ArrayList;
import java.util.List;
import java.util.Objects;

/** Maps the frozen initialize wire shape without exposing a mutable global mapper. */
public final class InitializeWireMapper {
    /** Prevents construction because mapping has no mutable state. */
    private InitializeWireMapper() {
    }

    /** Parses raw initialize fields before version compatibility is evaluated. */
    public static InitializeWireParams readParams(ObjectNode node) {
        Objects.requireNonNull(node, "node");
        return new InitializeWireParams(requiredInt(node, "protocolMajor"),
                requiredInt(node, "protocolMinor"),
                requiredInt(node, "minimumCompatibleMinor"),
                requiredText(node, "clientVersion"), readCapabilities(node.get("capabilities")),
                readLimits(node.get("limits")));
    }

    /** Writes exact lower-camel initialize fields so Java and Rust share one DTO shape. */
    public static ObjectNode writeParams(InitializeWireParams params) {
        Objects.requireNonNull(params, "params");
        ObjectNode node = io.github.kongweiguang.ja.protocol.JsonNodes.object();
        node.put("protocolMajor", params.protocolMajor());
        node.put("protocolMinor", params.protocolMinor());
        node.put("minimumCompatibleMinor", params.minimumCompatibleMinor());
        node.put("clientVersion", params.clientVersion());
        node.set("capabilities", writeCapabilities(params.capabilities()));
        node.set("limits", writeLimits(params.limits()));
        return node;
    }

    /** Writes the mandatory initialize result fields used by the desktop host. */
    public static ObjectNode writeResult(NegotiatedInitialization result) {
        Objects.requireNonNull(result, "result");
        ObjectNode node = io.github.kongweiguang.ja.protocol.JsonNodes.object();
        node.put("protocolMajor", result.version().major());
        node.put("protocolMinor", result.version().minor());
        node.put("serverVersion", result.serverVersion());
        node.put("serverInstanceId", result.serverInstanceId().value());
        node.set("capabilities", writeCapabilities(result.capabilities()));
        node.set("limits", writeLimits(result.limits()));
        return node;
    }

    /** Reads a mandatory object so missing/null capability data cannot become an empty offer. */
    private static Capabilities readCapabilities(JsonNode node) {
        if (node == null || !node.isObject()) {
            throw invalidParams();
        }
        return new Capabilities(textArray(node, "methods"), textArray(node, "events"),
                textArray(node, "permissionModes"), textArray(node, "itemKinds"),
                readMcp(node.get("mcp")));
    }

    /** Reads the mandatory nested MCP capability object. */
    private static McpCapabilities readMcp(JsonNode node) {
        if (node == null || !node.isObject()) {
            throw invalidParams();
        }
        return new McpCapabilities(textArray(node, "protocolVersions"),
                textArray(node, "transports"), textArray(node, "features"));
    }

    /** Reads all bounded limits through the same constructor used by negotiation. */
    private static ProtocolLimits readLimits(JsonNode node) {
        if (node == null || !node.isObject()) {
            throw invalidParams();
        }
        return new ProtocolLimits(requiredInt(node, "maxFrameBytes"),
                requiredInt(node, "maxInboundQueueFrames"), requiredInt(node, "maxOutboundQueueFrames"),
                requiredInt(node, "maxInFlightRequests"), requiredInt(node, "maxPendingRequests"),
                requiredInt(node, "maxItemDeltaBytes"), requiredInt(node, "maxInlineToolOutputBytes"),
                requiredInt(node, "maxArtifactBytes"), requiredInt(node, "maxLogBytes"),
                requiredInt(node, "defaultRequestDeadlineMs"), requiredInt(node, "defaultApprovalDeadlineMs"));
    }

    /** Serializes the schema-complete capability arrays in stable field order. */
    private static ObjectNode writeCapabilities(Capabilities capabilities) {
        ObjectNode node = io.github.kongweiguang.ja.protocol.JsonNodes.object();
        node.set("methods", textArray(capabilities.methods()));
        node.set("events", textArray(capabilities.events()));
        node.set("permissionModes", textArray(capabilities.permissionModes()));
        node.set("itemKinds", textArray(capabilities.itemKinds()));
        ObjectNode mcp = io.github.kongweiguang.ja.protocol.JsonNodes.object();
        mcp.set("protocolVersions", textArray(capabilities.mcp().protocolVersions()));
        mcp.set("transports", textArray(capabilities.mcp().transports()));
        mcp.set("features", textArray(capabilities.mcp().features()));
        node.set("mcp", mcp);
        return node;
    }

    /** Serializes all negotiated resource limits without dropping a field. */
    private static ObjectNode writeLimits(ProtocolLimits limits) {
        ObjectNode node = io.github.kongweiguang.ja.protocol.JsonNodes.object();
        node.put("maxFrameBytes", limits.maxFrameBytes());
        node.put("maxInboundQueueFrames", limits.maxInboundQueueFrames());
        node.put("maxOutboundQueueFrames", limits.maxOutboundQueueFrames());
        node.put("maxInFlightRequests", limits.maxInFlightRequests());
        node.put("maxPendingRequests", limits.maxPendingRequests());
        node.put("maxItemDeltaBytes", limits.maxItemDeltaBytes());
        node.put("maxInlineToolOutputBytes", limits.maxInlineToolOutputBytes());
        node.put("maxArtifactBytes", limits.maxArtifactBytes());
        node.put("maxLogBytes", limits.maxLogBytes());
        node.put("defaultRequestDeadlineMs", limits.defaultRequestDeadlineMs());
        node.put("defaultApprovalDeadlineMs", limits.defaultApprovalDeadlineMs());
        return node;
    }

    /** Converts a JSON array to a bounded immutable string list before domain validation. */
    private static List<String> textArray(JsonNode parent, String field) {
        JsonNode node = parent.get(field);
        if (node == null || !node.isArray()) {
            throw invalidParams();
        }
        List<String> values = new ArrayList<>();
        for (JsonNode value : node) {
            if (!value.isTextual()) {
                throw invalidParams();
            }
            values.add(value.textValue());
        }
        return List.copyOf(values);
    }

    /** Creates a JSON array without exposing an ObjectMapper. */
    private static ArrayNode textArray(List<String> values) {
        ArrayNode node = io.github.kongweiguang.ja.protocol.JsonNodes.array();
        values.forEach(node::add);
        return node;
    }

    /** Reads a required bounded integer and rejects floating, null, or overflow values. */
    private static int requiredInt(JsonNode node, String field) {
        JsonNode value = node.get(field);
        if (value == null || !value.isIntegralNumber() || !value.canConvertToInt()) {
            throw invalidParams();
        }
        return value.intValue();
    }

    /** Reads a required non-null text field before the domain constructor applies its length bound. */
    private static String requiredText(JsonNode node, String field) {
        JsonNode value = node.get(field);
        if (value == null || !value.isTextual()) {
            throw invalidParams();
        }
        return value.textValue();
    }

    /** Creates the stable parameter error without exposing parser internals. */
    private static ProtocolException invalidParams() {
        return new ProtocolException(JaErrorCode.INVALID_PARAMS);
    }
}
