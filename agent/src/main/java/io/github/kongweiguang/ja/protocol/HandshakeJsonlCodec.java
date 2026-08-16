// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.core.JsonParseException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.application.HandshakeStateMachine;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;

/**
 * The sole production JSONL entry point. Each public frame operation owns
 * framing, Jackson parsing/serialization, raw challenge inspection, and
 * lifecycle admission in one path. There is deliberately no separate raw
 * codec that reflection or bootstrap code could call to bypass the session.
 */
public final class HandshakeJsonlCodec {
    private final HandshakeStateMachine stateMachine;

    /**
     * Binds the codec to one concrete connection authority; requiring the
     * state machine prevents callers from injecting an independent token
     * policy or reusing a guard from another generation.
     */
    public HandshakeJsonlCodec(HandshakeStateMachine stateMachine) {
        this.stateMachine = Objects.requireNonNull(stateMachine, "stateMachine");
    }

    /**
     * Decodes one complete frame, validates its raw tree before lossy mapping,
     * and admits the resulting envelope only after lifecycle checks succeed.
     */
    public RpcEnvelope decode(byte[] frame, RpcDirection inboundDirection,
                              ProtocolLimits limits) {
        Objects.requireNonNull(inboundDirection, "inboundDirection");
        Objects.requireNonNull(limits, "limits");
        if (frame == null || frame.length == 0 || frame[frame.length - 1] != '\n') {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        if (frame.length > limits.maxFrameBytes() + 1) {
            throw new ProtocolException(JaErrorCode.FRAME_TOO_LARGE);
        }
        for (int index = 0; index < frame.length - 1; index++) {
            if (frame[index] == '\n' || frame[index] == '\r') {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }
        }
        byte[] json = java.util.Arrays.copyOf(frame, frame.length - 1);
        String text = decodeUtf8(json);
        if (text.isEmpty() || text.charAt(0) == '\ufeff') {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        try {
            JsonNode parsed = JsonSupport.readTree(json);
            if (parsed == null || !parsed.isObject()) {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }
            ObjectNode node = (ObjectNode) parsed;
            UnicodeChecks.tree(node);
            stateMachine.validateRawFrame(node, inboundDirection);

            JsonNode jsonrpc = node.get("jsonrpc");
            if (jsonrpc == null || !jsonrpc.isTextual() || !"2.0".equals(jsonrpc.textValue())) {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }
            boolean hasId = node.has("id");
            boolean hasMethod = node.has("method");
            boolean hasResult = node.has("result");
            boolean hasError = node.has("error");
            if (hasResult && hasError) {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }
            if (hasMethod && (hasResult || hasError)) {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }

            RpcEnvelope envelope;
            if (hasId && hasMethod) {
                JsonNode idNode = node.get("id");
                if (idNode == null || !idNode.isTextual()) {
                    throw new ProtocolException(JaErrorCode.INVALID_FRAME);
                }
                JsonNode paramsNode = node.get("params");
                if (paramsNode == null || !paramsNode.isObject()) {
                    throw new ProtocolException(JaErrorCode.INVALID_FRAME);
                }
                JsonNode methodNode = node.get("method");
                if (methodNode == null || !methodNode.isTextual()) {
                    throw new ProtocolException(JaErrorCode.INVALID_FRAME);
                }
                envelope = new RpcRequest(ProtocolChecks.requestId(idNode.textValue(), inboundDirection),
                        ProtocolChecks.method(methodNode.textValue()), (ObjectNode) paramsNode,
                        inboundDirection, extensions(node, "jsonrpc", "id", "method", "params"));
            } else if (!hasId && hasMethod) {
                JsonNode methodNode = node.get("method");
                if (methodNode == null || !methodNode.isTextual()) {
                    throw new ProtocolException(JaErrorCode.INVALID_FRAME);
                }
                JsonNode paramsNode = node.get("params");
                if (paramsNode == null || !paramsNode.isObject()) {
                    throw new ProtocolException(JaErrorCode.INVALID_FRAME);
                }
                envelope = new RpcNotification(ProtocolChecks.method(methodNode.textValue()),
                        (ObjectNode) paramsNode, inboundDirection,
                        extensions(node, "jsonrpc", "method", "params"));
            } else if (hasId && !hasMethod && (hasResult || hasError)) {
                JsonNode idNode = node.get("id");
                if (idNode == null || !idNode.isTextual()) {
                    throw new ProtocolException(JaErrorCode.INVALID_FRAME);
                }
                String id = ProtocolChecks.requestId(idNode.textValue(), inboundDirection.opposite());
                if (hasError) {
                    JsonNode errorNode = node.get("error");
                    if (errorNode == null || !errorNode.isObject()) {
                        throw new ProtocolException(JaErrorCode.INVALID_FRAME);
                    }
                    envelope = new RpcResponse(id, null, false, parseError(errorNode),
                            extensions(node, "jsonrpc", "id", "error"));
                } else {
                    envelope = new RpcResponse(id, node.get("result"), true, null,
                            extensions(node, "jsonrpc", "id", "result"));
                }
            } else {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }
            return stateMachine.acceptInbound(envelope);
        } catch (ProtocolException exception) {
            throw exception;
        } catch (JsonParseException exception) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME, null, exception);
        } catch (IOException | RuntimeException exception) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME, null, exception);
        }
    }

    /**
     * Reads one LF-delimited frame and routes it through {@link #decode}; EOF
     * is empty, while an unterminated or over-limit frame fails closed.
     */
    public Optional<RpcEnvelope> readFrame(InputStream input, RpcDirection inboundDirection,
                                           ProtocolLimits limits) throws IOException {
        Objects.requireNonNull(input, "input");
        Objects.requireNonNull(inboundDirection, "inboundDirection");
        Objects.requireNonNull(limits, "limits");
        ByteArrayOutputStream buffer = new ByteArrayOutputStream(Math.min(limits.maxFrameBytes() + 1, 8192));
        int value;
        while ((value = input.read()) != -1) {
            if (value == '\n') {
                buffer.write(value);
                return Optional.of(decode(buffer.toByteArray(), inboundDirection, limits));
            }
            if (value == '\r') {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }
            if (buffer.size() >= limits.maxFrameBytes()) {
                throw new ProtocolException(JaErrorCode.FRAME_TOO_LARGE);
            }
            buffer.write(value);
        }
        if (buffer.size() != 0) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        return Optional.empty();
    }

    /**
     * Validates the outbound envelope before direct Jackson serialization;
     * no raw serializer exists outside this guarded production method.
     */
    public byte[] encode(RpcEnvelope envelope, ProtocolLimits limits) {
        Objects.requireNonNull(envelope, "envelope");
        Objects.requireNonNull(limits, "limits");
        stateMachine.validateOutboundFrame(envelope);
        try {
            JsonNode document = envelope.toJson();
            if (document == null || !document.isObject()) {
                throw new ProtocolException(JaErrorCode.INVALID_FRAME);
            }
            UnicodeChecks.tree(document);
            byte[] json = JsonSupport.write(document);
            if (json.length > limits.maxFrameBytes()) {
                throw new ProtocolException(JaErrorCode.FRAME_TOO_LARGE);
            }
            byte[] frame = java.util.Arrays.copyOf(json, json.length + 1);
            frame[frame.length - 1] = '\n';
            return frame;
        } catch (ProtocolException exception) {
            throw exception;
        } catch (IOException | IllegalArgumentException exception) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME, null, exception);
        }
    }

    /** Decodes UTF-8 with REPORT actions so malformed bytes cannot be replaced. */
    private static String decodeUtf8(byte[] bytes) {
        try {
            CharBuffer chars = StandardCharsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT)
                    .decode(ByteBuffer.wrap(bytes));
            return chars.toString();
        } catch (CharacterCodingException exception) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME, null, exception);
        }
    }

    /** Parses only the bounded redacted error shape allowed on the wire. */
    private static RpcError parseError(JsonNode node) {
        JsonNode code = node.get("code");
        JsonNode message = node.get("message");
        JsonNode data = node.get("data");
        if (code == null || !code.isInt() || message == null || !message.isTextual()
                || data == null || !data.isObject()) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        ObjectNode errorData = (ObjectNode) data;
        String jaCode = text(errorData, "jaCode");
        JsonNode retryable = errorData.get("retryable");
        if (jaCode == null || retryable == null || !retryable.isBoolean()) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        String diagnosticId = text(errorData, "diagnosticId");
        String field = text(errorData, "field");
        Long retryAfter = errorData.has("retryAfterMs") ? longValue(errorData.get("retryAfterMs")) : null;
        JaErrorCode knownByName = JaErrorCode.fromName(jaCode);
        JaErrorCode knownByCode = JaErrorCode.fromWireCode(code.intValue());
        if (knownByName == null || knownByCode == null) {
            return RpcError.of(JaErrorCode.INTERNAL_ERROR, null);
        }
        if (knownByName != knownByCode || knownByName.retryable() != retryable.booleanValue()) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        try {
            return new RpcError(code.intValue(), message.textValue(),
                    new RpcErrorData(jaCode, retryable.booleanValue(), diagnosticId, field, retryAfter));
        } catch (IllegalArgumentException exception) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME, null, exception);
        }
    }

    /** Rejects non-integral and overflowing retry hints before narrowing to long. */
    private static Long longValue(JsonNode node) {
        if (node == null || !node.isIntegralNumber() || !node.canConvertToLong()) {
            throw new ProtocolException(JaErrorCode.INVALID_FRAME);
        }
        return node.longValue();
    }

    /** Reads optional text metadata without accepting arbitrary JSON values. */
    private static String text(ObjectNode node, String name) {
        JsonNode value = node.get(name);
        return value == null ? null : value.isTextual() ? value.textValue() : invalidText();
    }

    /** Centralizes malformed optional text handling to preserve fail-closed parsing. */
    private static String invalidText() {
        throw new ProtocolException(JaErrorCode.INVALID_FRAME);
    }

    /** Copies unknown minor fields while preserving the closed envelope members. */
    private static ObjectNode extensions(ObjectNode node, String... known) {
        ObjectNode result = JsonSupport.objectNode();
        java.util.Set<String> excluded = java.util.Set.of(known);
        for (Map.Entry<String, JsonNode> field : node.properties()) {
            if (!excluded.contains(field.getKey())) {
                result.set(field.getKey(), field.getValue().deepCopy());
            }
        }
        return result;
    }
}
