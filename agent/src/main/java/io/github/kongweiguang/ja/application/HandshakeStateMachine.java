// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.domain.EventId;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.protocol.InitializedParams;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.ReadyToken;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.protocol.RpcEnvelope;
import io.github.kongweiguang.ja.protocol.RpcNotification;
import io.github.kongweiguang.ja.protocol.RuntimeStatus;
import io.github.kongweiguang.ja.protocol.RuntimeStatusChangedParams;
import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.time.Instant;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.Map;
import java.util.Objects;
import java.util.Set;

/**
 * Owns one stdio generation's challenge/ready transition. The token is only
 * accepted from Rust and is never generated, logged, or returned as a Java
 * state accessor.
 */
public final class HandshakeStateMachine {
    /** States are intentionally terminal per generation after a protocol failure. */
    public enum State {
        NEW,
        INITIALIZED,
        READY,
        FAILED,
        STOPPED
    }

    private static final int MAX_GENERATIONS = 1_024;

    private ServerInstanceId serverInstanceId;
    private final Set<String> knownTokenValues = new LinkedHashSet<>();
    private long generation;
    private State state = State.NEW;
    private ReadyToken currentToken;

    /** Starts generation one with a bounded whole-frame redaction guard. */
    public HandshakeStateMachine(ServerInstanceId serverInstanceId) {
        this(serverInstanceId, 1L);
    }

    /** Starts an explicitly identified generation without inventing its challenge. */
    public HandshakeStateMachine(ServerInstanceId serverInstanceId, long generation) {
        this.serverInstanceId = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        if (generation < 1) {
            throw new IllegalArgumentException("generation must be positive");
        }
        this.generation = generation;
    }

    /**
     * Accepts exactly one inbound initialized notification and records its
     * challenge only after the full frame passes redaction validation.
     */
    public synchronized void acceptInitialized(RpcNotification notification) {
        try {
            requireState(State.NEW);
            if (notification == null || notification.direction() != RpcDirection.CLIENT_TO_SERVER
                    || !"initialized".equals(notification.method())) {
                throw handshakeFailure();
            }
            validateEnvelope(notification);
            InitializedParams params = InitializedParams.fromJson(notification.params());
            acceptChallenge(params.readyToken());
            currentToken = params.readyToken();
            state = State.INITIALIZED;
        } catch (ProtocolException exception) {
            failGeneration();
            throw handshakeFailure();
        } catch (RuntimeException exception) {
            failGeneration();
            throw handshakeFailure();
        }
    }

    /** Accepts raw initialized parameters only through the same strict notification path. */
    public synchronized void acceptInitialized(ObjectNode params) {
        try {
            acceptInitialized(new RpcNotification("initialized", params, RpcDirection.CLIENT_TO_SERVER));
        } catch (RuntimeException exception) {
            failGeneration();
            throw handshakeFailure();
        }
    }

    /** Builds the ready event using the already accepted challenge, preventing caller substitution. */
    public synchronized RpcNotification publishReady(EventId eventId, Instant occurredAt) {
        try {
            requireState(State.INITIALIZED);
            if (currentToken == null) {
                throw handshakeFailure();
            }
            return publishReady(RuntimeStatusChangedParams.ready(serverInstanceId,
                    Objects.requireNonNull(eventId, "eventId"), Objects.requireNonNull(occurredAt, "occurredAt"),
                    currentToken));
        } catch (RuntimeException exception) {
            failGeneration();
            throw handshakeFailure();
        }
    }

    /**
     * Publishes exactly one ready event after proving instance, state, status,
     * and challenge equality; malformed or stale values terminate the generation.
     */
    public synchronized RpcNotification publishReady(RuntimeStatusChangedParams params) {
        try {
            requireState(State.INITIALIZED);
            if (params == null || params.status() != RuntimeStatus.READY
                    || !serverInstanceId.equals(params.serverInstanceId())
                    || currentToken == null || !matchesCurrent(params.readyToken())) {
                throw handshakeFailure();
            }
            RpcNotification ready = new RpcNotification("runtime/statusChanged", params.toJson(),
                    RpcDirection.SERVER_TO_CLIENT);
            validateEnvelope(ready);
            state = State.READY;
            return ready;
        } catch (ProtocolException exception) {
            failGeneration();
            throw handshakeFailure();
        } catch (RuntimeException exception) {
            failGeneration();
            throw handshakeFailure();
        }
    }

    /** Handles initialized and ready as one deterministic operation for a stdio reader loop. */
    public synchronized RpcNotification acceptInitializedAndPublishReady(RpcNotification initialized,
                                                                          EventId eventId,
                                                                          Instant occurredAt) {
        acceptInitialized(initialized);
        return publishReady(eventId, occurredAt);
    }

    /**
     * Validates inbound frames recursively and rejects business traffic until
     * the current generation has published ready.
     */
    public synchronized RpcEnvelope acceptInbound(RpcEnvelope envelope) {
        try {
            validateEnvelope(Objects.requireNonNull(envelope, "envelope"));
            if (envelope instanceof RpcNotification notification
                    && "initialized".equals(notification.method())) {
                acceptInitialized(notification);
                return notification;
            }
            if (envelope instanceof RpcNotification notification
                    && "runtime/statusChanged".equals(notification.method())) {
                RuntimeStatusChangedParams status = RuntimeStatusChangedParams.fromJson(notification.params());
                if (status.status() == RuntimeStatus.READY) {
                    throw handshakeFailure();
                }
                if (status.status() == RuntimeStatus.STOPPED
                        || status.status() == RuntimeStatus.CRASHED) {
                    shutdown();
                }
                return notification;
            }
            requireReady();
            return envelope;
        } catch (ProtocolException exception) {
            if (exception.code() == JaErrorCode.HANDSHAKE_FAILED) {
                failGeneration();
                throw handshakeFailure();
            }
            throw exception;
        }
    }

    /**
     * Applies the connection guard to the raw object before envelope parsing;
     * this bridge is intentionally only for {@link io.github.kongweiguang.ja.protocol.HandshakeJsonlCodec}.
     */
    public synchronized void validateRawFrame(ObjectNode document, RpcDirection inboundDirection) {
        try {
            validateRaw(Objects.requireNonNull(document, "document"),
                    Objects.requireNonNull(inboundDirection, "inboundDirection"));
        } catch (ProtocolException exception) {
            if (exception.code() == JaErrorCode.HANDSHAKE_FAILED) {
                failGeneration();
                throw handshakeFailure();
            }
            throw exception;
        } catch (RuntimeException exception) {
            failGeneration();
            throw handshakeFailure();
        }
    }

    /**
     * Applies the connection guard to an outbound object while keeping raw
     * JSON encoding private to the guarded protocol facade.
     */
    public synchronized void validateOutboundFrame(RpcEnvelope envelope) {
        Objects.requireNonNull(envelope, "envelope");
        if (envelope instanceof RpcNotification notification
                && "initialized".equals(notification.method())) {
            failGeneration();
            throw handshakeFailure();
        }
        try {
            validateEnvelope(envelope);
        } catch (ProtocolException exception) {
            if (exception.code() == JaErrorCode.HANDSHAKE_FAILED) {
                failGeneration();
                throw handshakeFailure();
            }
            throw exception;
        }
    }

    /** Requires ready before ordinary business dispatch can touch models, tools, or workspaces. */
    public synchronized void requireReady() {
        if (state == State.READY) {
            return;
        }
        if (state == State.FAILED || state == State.STOPPED) {
            throw handshakeFailure();
        }
        throw new ProtocolException(JaErrorCode.NOT_INITIALIZED);
    }

    /** Ends the generation and clears active challenge state before process shutdown. */
    public synchronized void shutdown() {
        state = State.STOPPED;
        currentToken = null;
        clearCurrent();
    }

    /**
     * Starts a strictly newer generation while retaining only redaction/reuse
     * tombstones; the old active challenge cannot make the new generation ready.
     */
    public synchronized void restart(long nextGeneration) {
        restart(nextGeneration, serverInstanceId);
    }

    /**
     * Starts a newer generation with an optionally rotated server identity;
     * ready events must use this identity so old sidecar timelines cannot mix.
     */
    public synchronized void restart(long nextGeneration, ServerInstanceId nextServerInstanceId) {
        if (nextGeneration <= generation) {
            throw handshakeFailure();
        }
        serverInstanceId = Objects.requireNonNull(nextServerInstanceId, "nextServerInstanceId");
        generation = nextGeneration;
        state = State.NEW;
        currentToken = null;
        clearCurrent();
    }

    /** Returns the lifecycle state without exposing the challenge. */
    public synchronized State state() {
        return state;
    }

    /** Returns the monotonic generation identity used for lifecycle assertions. */
    public synchronized long generation() {
        return generation;
    }

    /** Returns the current generation's server identity without exposing handshake state. */
    public synchronized ServerInstanceId serverInstanceId() {
        return serverInstanceId;
    }

    /** Returns the number of challenge tombstones retained for security checks. */
    public synchronized int rememberedGenerationCount() {
        return knownTokenValues.size();
    }

    /** Omits challenge and event payloads from lifecycle diagnostics. */
    @Override
    public synchronized String toString() {
        return "HandshakeStateMachine[serverInstanceId=" + serverInstanceId
                + ", generation=" + generation + ", state=" + state + "]";
    }

    /**
     * Validates an already mapped envelope against the same generation before
     * any caller can serialize it or dispatch its extensions.
     */
    private void validateEnvelope(RpcEnvelope envelope) {
        ObjectNode document = envelope.toJson();
        UnicodeChecks.tree(document);
        ReadyToken legalToken = legalHandshakeToken(envelope, document);
        Set<String> forbidden = new HashSet<>(knownTokenValues);
        if (legalToken != null) {
            forbidden.add(legalToken.value());
        }
        visit(document, "", forbidden, legalToken);
    }

    /**
     * Validates raw JSON while unknown extension fields are still present;
     * this closes the only point where a token could hide before mapping.
     */
    private void validateRaw(ObjectNode document, RpcDirection inboundDirection) {
        validateRawSyntax(document);
        ReadyToken legalToken = null;
        JsonNode method = document.get("method");
        if (method != null && method.isTextual() && "initialized".equals(method.textValue())) {
            if (inboundDirection != RpcDirection.CLIENT_TO_SERVER || document.has("id")) {
                throw handshakeFailure();
            }
            legalToken = InitializedParams.fromJson(document.get("params")).readyToken();
            if (knownTokenValues.contains(legalToken.value()) && !legalToken.equals(currentToken)) {
                throw handshakeFailure();
            }
        } else if (method != null && method.isTextual()
                && "runtime/statusChanged".equals(method.textValue())) {
            if (document.has("id")) {
                throw handshakeFailure();
            }
            RuntimeStatusChangedParams params = RuntimeStatusChangedParams.fromJson(document.get("params"));
            if (params.status() == RuntimeStatus.READY) {
                if (inboundDirection != RpcDirection.SERVER_TO_CLIENT
                        || !matchesCurrent(params.readyToken())) {
                    throw handshakeFailure();
                }
                legalToken = params.readyToken();
            }
        }
        Set<String> forbidden = new HashSet<>(knownTokenValues);
        if (legalToken != null) {
            forbidden.add(legalToken.value());
        }
        visit(document, "", forbidden, legalToken);
    }

    /** Records a challenge only after raw syntax and lifecycle checks succeed. */
    private void acceptChallenge(ReadyToken token) {
        Objects.requireNonNull(token, "token");
        if (currentToken != null || knownTokenValues.contains(token.value())
                || knownTokenValues.size() >= MAX_GENERATIONS) {
            throw handshakeFailure();
        }
        knownTokenValues.add(token.value());
        currentToken = token;
    }

    /** Clears only the active challenge while retaining permanent redaction tombstones. */
    private void clearCurrent() {
        currentToken = null;
    }

    /** Returns whether a ready event carries the exact active challenge. */
    private boolean matchesCurrent(ReadyToken token) {
        return currentToken != null && currentToken.equals(token);
    }

    /** Determines the only legal challenge-bearing envelope path. */
    private ReadyToken legalHandshakeToken(RpcEnvelope envelope, ObjectNode document) {
        if (!(envelope instanceof RpcNotification notification)) {
            if (document.has("readyToken")) {
                throw handshakeFailure();
            }
            return null;
        }
        String method = notification.method();
        if ("initialized".equals(method)) {
            if (notification.direction() != RpcDirection.CLIENT_TO_SERVER) {
                throw handshakeFailure();
            }
            InitializedParams params = InitializedParams.fromJson(document.get("params"));
            if (knownTokenValues.contains(params.readyToken().value())
                    && !params.readyToken().equals(currentToken)) {
                throw handshakeFailure();
            }
            return params.readyToken();
        }
        if ("runtime/statusChanged".equals(method)) {
            RuntimeStatusChangedParams params = RuntimeStatusChangedParams.fromJson(document.get("params"));
            if (params.status() != RuntimeStatus.READY) {
                return null;
            }
            if (notification.direction() != RpcDirection.SERVER_TO_CLIENT
                    || !matchesCurrent(params.readyToken())) {
                throw handshakeFailure();
            }
            return params.readyToken();
        }
        if (document.has("readyToken")) {
            throw handshakeFailure();
        }
        return null;
    }

    /** Recursively rejects remembered challenge keys and values in all extensions. */
    private void visit(JsonNode node, String path, Set<String> forbidden, ReadyToken legalToken) {
        if (node == null || node.isNull()) {
            return;
        }
        if (node.isTextual()) {
            if (forbidden.contains(node.textValue())) {
                throw handshakeFailure();
            }
            return;
        }
        if (node.isArray()) {
            int index = 0;
            for (JsonNode child : node) {
                visit(child, path + "[" + index++ + "]", forbidden, legalToken);
            }
            return;
        }
        if (!node.isObject()) {
            return;
        }
        for (Map.Entry<String, JsonNode> field : node.properties()) {
            String childPath = path.isEmpty() ? field.getKey() : path + "." + field.getKey();
            JsonNode value = field.getValue();
            boolean legalPath = "params.readyToken".equals(childPath)
                    && legalToken != null
                    && value != null
                    && value.isTextual()
                    && legalToken.value().equals(value.textValue());
            if ("readyToken".equals(field.getKey())) {
                if (!legalPath) {
                    throw handshakeFailure();
                }
                continue;
            }
            if (forbidden.contains(field.getKey())
                    || (value != null && value.isTextual() && forbidden.contains(value.textValue()))) {
                throw handshakeFailure();
            }
            visit(value, childPath, forbidden, legalToken);
        }
    }

    /** Rejects challenge keys before envelope mapping can discard unknown fields. */
    private static void validateRawSyntax(ObjectNode document) {
        ReadyToken legalToken = null;
        JsonNode method = document.get("method");
        if (method != null && method.isTextual() && "initialized".equals(method.textValue())) {
            if (document.has("id")) {
                throw handshakeFailure();
            }
            legalToken = InitializedParams.fromJson(document.get("params")).readyToken();
        } else if (method != null && method.isTextual()
                && "runtime/statusChanged".equals(method.textValue())) {
            if (document.has("id")) {
                throw handshakeFailure();
            }
            RuntimeStatusChangedParams params = RuntimeStatusChangedParams.fromJson(document.get("params"));
            if (params.status() == RuntimeStatus.READY) {
                legalToken = params.readyToken();
            }
        }
        visitRaw(document, "", legalToken);
    }

    /** Recursively rejects raw readyToken keys outside the exact params path. */
    private static void visitRaw(JsonNode node, String path, ReadyToken legalToken) {
        if (node == null || node.isNull()) {
            return;
        }
        if (node.isArray()) {
            int index = 0;
            for (JsonNode child : node) {
                visitRaw(child, path + "[" + index++ + "]", legalToken);
            }
            return;
        }
        if (!node.isObject()) {
            return;
        }
        for (Map.Entry<String, JsonNode> field : node.properties()) {
            String childPath = path.isEmpty() ? field.getKey() : path + "." + field.getKey();
            JsonNode value = field.getValue();
            boolean legalPath = "params.readyToken".equals(childPath)
                    && legalToken != null
                    && value != null
                    && value.isTextual()
                    && legalToken.value().equals(value.textValue());
            if ("readyToken".equals(field.getKey()) && !legalPath) {
                throw handshakeFailure();
            }
            if (!legalPath) {
                visitRaw(value, childPath, legalToken);
            }
        }
    }

    /** Rejects a transition unless the current generation is at the expected state. */
    private void requireState(State expected) {
        if (state != expected) {
            throw handshakeFailure();
        }
    }

    /** Marks a generation terminal without clearing tombstones needed by redaction. */
    private void failGeneration() {
        state = State.FAILED;
    }

    /** Creates the only public failure used for handshake challenge violations. */
    private static ProtocolException handshakeFailure() {
        return new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
    }
}
