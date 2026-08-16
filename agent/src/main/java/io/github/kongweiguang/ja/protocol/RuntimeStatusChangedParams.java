// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.domain.EventId;
import io.github.kongweiguang.ja.domain.ServerInstanceId;

import java.time.Instant;
import java.time.format.DateTimeParseException;
import java.util.Objects;
import java.util.Set;

/**
 * Schema-shaped runtime status event. Only a ready status may carry a token;
 * the handshake state machine adds the equality and generation checks.
 */
public final class RuntimeStatusChangedParams {
    private static final Set<String> KNOWN_FIELDS = Set.of(
            "serverInstanceId", "eventId", "occurredAt", "status", "readyToken", "reason", "health");

    private final ServerInstanceId serverInstanceId;
    private final EventId eventId;
    private final Instant occurredAt;
    private final RuntimeStatus status;
    private final ReadyToken readyToken;
    private final String reason;
    private final ObjectNode health;
    private final ObjectNode extensions;

    /**
     * Validates the conditional token field before an event can be published;
     * this keeps non-ready lifecycle states from becoming token carriers.
     */
    public RuntimeStatusChangedParams(ServerInstanceId serverInstanceId, EventId eventId,
                                      Instant occurredAt, RuntimeStatus status,
                                      ReadyToken readyToken, String reason,
                                      ObjectNode health, ObjectNode extensions) {
        this.serverInstanceId = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        this.eventId = Objects.requireNonNull(eventId, "eventId");
        this.occurredAt = Objects.requireNonNull(occurredAt, "occurredAt");
        this.status = Objects.requireNonNull(status, "status");
        if ((status == RuntimeStatus.READY) != (readyToken != null)) {
            throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
        }
        if (reason != null && reason.length() > 1024) {
            throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
        }
        if (reason != null) {
            UnicodeChecks.wellFormed(reason, "runtime status reason");
        }
        this.readyToken = readyToken;
        this.reason = reason;
        this.health = copyObject(health, "health");
        this.extensions = copyExtensions(extensions);
    }

    /** Creates a status event without optional reason, health, or extension fields. */
    public RuntimeStatusChangedParams(ServerInstanceId serverInstanceId, EventId eventId,
                                      Instant occurredAt, RuntimeStatus status,
                                      ReadyToken readyToken) {
        this(serverInstanceId, eventId, occurredAt, status, readyToken,
                null, null, JsonNodes.object());
    }

    /** Creates a ready event with no optional diagnostic fields. */
    public static RuntimeStatusChangedParams ready(ServerInstanceId serverInstanceId, EventId eventId,
                                                   Instant occurredAt, ReadyToken readyToken) {
        return new RuntimeStatusChangedParams(serverInstanceId, eventId, occurredAt,
                RuntimeStatus.READY, Objects.requireNonNull(readyToken, "readyToken"),
                null, null, JsonNodes.object());
    }

    /**
     * Parses the status event fields and maps every malformed handshake shape
     * to the stable non-secret handshake error.
     */
    public static RuntimeStatusChangedParams fromJson(JsonNode node) {
        try {
            if (node == null || !node.isObject()) {
                throw handshakeFailure();
            }
            ObjectNode object = (ObjectNode) node;
            ServerInstanceId server = new ServerInstanceId(requiredText(object, "serverInstanceId"));
            EventId event = new EventId(requiredText(object, "eventId"));
            Instant occurredAt = parseInstant(requiredText(object, "occurredAt"));
            RuntimeStatus status = WireEnums.decode(requiredText(object, "status"), RuntimeStatus.class);
            ReadyToken token = object.has("readyToken") ? ReadyToken.fromJson(object.get("readyToken")) : null;
            String reason = optionalText(object, "reason");
            JsonNode healthNode = object.get("health");
            ObjectNode health = healthNode == null ? null : asObject(healthNode, "health");
            ObjectNode extensions = JsonNodes.object();
            for (java.util.Map.Entry<String, JsonNode> field : object.properties()) {
                if (!KNOWN_FIELDS.contains(field.getKey())) {
                    extensions.set(field.getKey(), field.getValue().deepCopy());
                }
            }
            return new RuntimeStatusChangedParams(server, event, occurredAt, status, token,
                    reason, health, extensions);
        } catch (ProtocolException exception) {
            throw exception;
        } catch (RuntimeException exception) {
            throw handshakeFailure();
        }
    }

    /** Returns the instance identity carried by the event. */
    public ServerInstanceId serverInstanceId() {
        return serverInstanceId;
    }

    /** Returns the event id used by status timeline deduplication. */
    public EventId eventId() {
        return eventId;
    }

    /** Returns the event timestamp used for lifecycle ordering. */
    public Instant occurredAt() {
        return occurredAt;
    }

    /** Returns the lower-case status vocabulary value. */
    public RuntimeStatus status() {
        return status;
    }

    /** Returns the challenge only to the secure handshake comparator/serializer. */
    public ReadyToken readyToken() {
        return readyToken;
    }

    /** Returns the optional bounded lifecycle reason. */
    public String reason() {
        return reason;
    }

    /** Returns a defensive health object copy so callers cannot mutate an event after validation. */
    public ObjectNode health() {
        return health == null ? null : health.deepCopy();
    }

    /** Returns a defensive copy of forward-compatible status extensions. */
    public ObjectNode extensions() {
        return extensions.deepCopy();
    }

    /** Serializes the status shape while preserving only validated optional fields. */
    public ObjectNode toJson() {
        ObjectNode node = extensions.deepCopy();
        node.put("serverInstanceId", serverInstanceId.value());
        node.put("eventId", eventId.value());
        node.put("occurredAt", occurredAt.toString());
        node.put("status", WireEnums.encode(status));
        if (readyToken != null) {
            node.put("readyToken", readyToken.value());
        }
        if (reason != null) {
            node.put("reason", reason);
        }
        if (health != null) {
            node.set("health", health.deepCopy());
        }
        return node;
    }

    /** Omits reason, health, extensions, and the challenge from diagnostics. */
    @Override
    public String toString() {
        return "RuntimeStatusChangedParams[serverInstanceId=" + serverInstanceId
                + ", eventId=" + eventId + ", status=" + status + "]";
    }

    /** Compares full immutable event payloads for deterministic event replay. */
    @Override
    public boolean equals(Object other) {
        if (!(other instanceof RuntimeStatusChangedParams that)) {
            return false;
        }
        return serverInstanceId.equals(that.serverInstanceId) && eventId.equals(that.eventId)
                && occurredAt.equals(that.occurredAt) && status == that.status
                && Objects.equals(readyToken, that.readyToken) && Objects.equals(reason, that.reason)
                && Objects.equals(health, that.health) && extensions.equals(that.extensions);
    }

    /** Keeps full event equality usable in deterministic fixture assertions. */
    @Override
    public int hashCode() {
        return Objects.hash(serverInstanceId, eventId, occurredAt, status, readyToken, reason, health, extensions);
    }

    /** Requires a textual field before typed parsing can produce an event identity. */
    private static String requiredText(ObjectNode object, String name) {
        JsonNode value = object.get(name);
        if (value == null || !value.isTextual() || value.textValue().isBlank()) {
            throw handshakeFailure();
        }
        UnicodeChecks.wellFormed(value.textValue(), name);
        return value.textValue();
    }

    /** Reads an optional bounded text field without allowing arbitrary JSON values. */
    private static String optionalText(ObjectNode object, String name) {
        JsonNode value = object.get(name);
        if (value == null) {
            return null;
        }
        if (!value.isTextual()) {
            throw handshakeFailure();
        }
        UnicodeChecks.wellFormed(value.textValue(), name);
        return value.textValue();
    }

    /** Parses ISO timestamps without retaining parser diagnostics that might contain input. */
    private static Instant parseInstant(String value) {
        try {
            return Instant.parse(value);
        } catch (DateTimeParseException exception) {
            throw handshakeFailure();
        }
    }

    /** Requires object-valued health so nested redaction can inspect it as a JSON tree. */
    private static ObjectNode asObject(JsonNode node, String field) {
        if (!node.isObject()) {
            throw handshakeFailure();
        }
        ObjectNode copy = ((ObjectNode) node).deepCopy();
        UnicodeChecks.tree(copy);
        return copy;
    }

    /** Copies an already typed health object so later caller mutation cannot alter the event. */
    private static ObjectNode copyObject(ObjectNode source, String field) {
        if (source == null) {
            return null;
        }
        ObjectNode copy = source.deepCopy();
        UnicodeChecks.tree(copy);
        return copy;
    }

    /** Copies and validates unknown fields without allowing envelope members to be shadowed. */
    private static ObjectNode copyExtensions(ObjectNode source) {
        ObjectNode copy = source == null ? JsonNodes.object() : source.deepCopy();
        for (String name : KNOWN_FIELDS) {
            if (copy.has(name)) {
                throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
            }
        }
        ProtocolChecks.extensions(copy);
        UnicodeChecks.tree(copy);
        return copy;
    }

    /** Creates a stable handshake failure without exposing malformed input. */
    private static ProtocolException handshakeFailure() {
        return new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
    }
}
