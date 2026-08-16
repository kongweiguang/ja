// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.application.HandshakeStateMachine;
import io.github.kongweiguang.ja.domain.EventId;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import org.junit.jupiter.api.Test;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Instant;
import java.util.Optional;

import static org.junit.jupiter.api.Assertions.*;

/** Frozen handshake DTO, state, and whole-frame redaction regressions. */
class HandshakeProtocolTest {
    private static final ProtocolLimits LIMITS = ProtocolLimits.defaults();
    private static final String TOKEN = "0123456789abcdef0123456789abcdef";
    private static final String OTHER_TOKEN = "abcdef0123456789abcdef0123456789";

    /** Verifies the challenge is strict lowercase hexadecimal and never appears in diagnostics. */
    @Test
    void readyTokenDtoIsStrictAndRedacted() {
        ReadyToken token = new ReadyToken(TOKEN);
        assertEquals(TOKEN, token.value());
        assertEquals("ReadyToken[redacted]", token.toString());
        assertThrows(ProtocolException.class, () -> new ReadyToken(TOKEN.toUpperCase()));
        assertThrows(ProtocolException.class, () -> new ReadyToken("0".repeat(31)));
        assertThrows(ProtocolException.class, () -> InitializedParams.fromJson(JsonNodes.object()));
        assertThrows(ProtocolException.class, () -> InitializedParams.fromJson(
                JsonNodes.object().put("readyToken", TOKEN).put("future", true)));
        InitializedParams params = InitializedParams.fromJson(JsonNodes.object().put("readyToken", TOKEN));
        assertFalse(params.toString().contains(TOKEN));
        assertEquals(TOKEN, params.toJson().path("readyToken").textValue());
    }

    /** Verifies one generation accepts initialized once and emits an exact ready echo. */
    @Test
    void handshakeStatePublishesOnlyMatchingReady() {
        ServerInstanceId server = new ServerInstanceId("srv_handshake_test");
        HandshakeStateMachine state = new HandshakeStateMachine(server, 1L);
        RpcNotification initialized = initialized(TOKEN);
        state.acceptInitialized(initialized);
        assertEquals(HandshakeStateMachine.State.INITIALIZED, state.state());
        assertThrows(ProtocolException.class, () -> state.requireReady());

        RpcNotification ready = state.publishReady(new EventId("evt_handshake_ready_test"),
                Instant.parse("2026-08-16T00:00:01Z"));
        assertEquals(HandshakeStateMachine.State.READY, state.state());
        assertEquals(TOKEN, ready.params().path("readyToken").textValue());
        assertEquals("ready", ready.params().path("status").textValue());
        assertFalse(state.toString().contains(TOKEN));
        assertFalse(ready.toString().contains(TOKEN));
        assertTrue(new HandshakeJsonlCodec(state).encode(ready, LIMITS).length > 0);
        assertThrows(ProtocolException.class, () -> state.publishReady(
                RuntimeStatusChangedParams.ready(server, new EventId("evt_handshake_ready_duplicate"),
                        Instant.parse("2026-08-16T00:00:02Z"), new ReadyToken(TOKEN))));
    }

    /** Verifies pre-ready business traffic and wrong-generation challenges fail closed. */
    @Test
    void handshakeRejectsPreReadyDuplicateAndStaleGenerationUse() {
        HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_handshake_reject"));
        RpcNotification notice = new RpcNotification("runtime/notice", JsonNodes.object()
                .put("code", "NOT_INITIALIZED").put("message", "wait"), RpcDirection.SERVER_TO_CLIENT);
        assertCode(JaErrorCode.NOT_INITIALIZED, () -> state.acceptInbound(notice));
        state.acceptInitialized(initialized(TOKEN));
        assertThrows(ProtocolException.class, () -> state.acceptInitialized(initialized(OTHER_TOKEN)));

        HandshakeStateMachine restarted = new HandshakeStateMachine(new ServerInstanceId("srv_handshake_restart"));
        restarted.acceptInitialized(initialized(TOKEN));
        restarted.publishReady(new EventId("evt_handshake_restart_ready"), Instant.parse("2026-08-16T00:00:01Z"));
        restarted.shutdown();
        restarted.restart(2L);
        assertEquals(HandshakeStateMachine.State.NEW, restarted.state());
        assertThrows(ProtocolException.class, () -> restarted.acceptInitialized(initialized(TOKEN)));
        assertEquals(1, restarted.rememberedGenerationCount());

        HandshakeStateMachine history = new HandshakeStateMachine(new ServerInstanceId("srv_handshake_history"));
        history.acceptInitialized(initialized(TOKEN));
        history.publishReady(new EventId("evt_handshake_history_ready1"), Instant.parse("2026-08-16T00:00:01Z"));
        history.shutdown();
        ServerInstanceId nextServer = new ServerInstanceId("srv_handshake_history2");
        history.restart(2L, nextServer);
        history.acceptInitialized(initialized(OTHER_TOKEN));
        assertEquals(2, history.rememberedGenerationCount());
        assertEquals(nextServer.value(), history.publishReady(new EventId("evt_handshake_history_ready2"),
                Instant.parse("2026-08-16T00:00:02Z")).params().path("serverInstanceId").asText());
        RpcResponse oldValue = RpcResponse.success("c:old-generation", JsonNodes.object().put("value", TOKEN));
        assertCode(JaErrorCode.HANDSHAKE_FAILED, () -> new HandshakeJsonlCodec(history).encode(oldValue, LIMITS));
    }

    /**
     * Verifies only the client initialize request can cross the NEW-state
     * admission boundary through both production JSONL entry points.
     */
    @Test
    void admitsClientInitializeRequestBeforeReadyThroughDecodeAndReadFrame() throws Exception {
        byte[] frame = appendLf(jsonBytes(RpcRequest.client("c:initialize", "initialize",
                JsonNodes.object().put("protocolMajor", 1)).toJson()));

        HandshakeStateMachine decodeState = new HandshakeStateMachine(
                new ServerInstanceId("srv_initialize_decode"));
        RpcRequest decoded = assertInstanceOf(RpcRequest.class,
                new HandshakeJsonlCodec(decodeState).decode(frame, RpcDirection.CLIENT_TO_SERVER, LIMITS));
        assertEquals("initialize", decoded.method());
        assertEquals(HandshakeStateMachine.State.NEW, decodeState.state());

        HandshakeStateMachine readState = new HandshakeStateMachine(
                new ServerInstanceId("srv_initialize_read"));
        RpcRequest read = assertInstanceOf(RpcRequest.class,
                new HandshakeJsonlCodec(readState).readFrame(
                        new ByteArrayInputStream(frame), RpcDirection.CLIENT_TO_SERVER, LIMITS)
                        .orElseThrow());
        assertEquals("initialize", read.method());
        assertEquals(HandshakeStateMachine.State.NEW, readState.state());
    }

    /**
     * Verifies the NEW exception is limited to one request shape and that
     * invalid frames cannot use it to bypass raw or envelope validation.
     */
    @Test
    void preReadyAdmissionRejectsOtherDirectionsKindsAndMalformedInitialize() throws Exception {
        assertCode(JaErrorCode.NOT_INITIALIZED, () -> decodeNew(
                new RpcNotification("initialize", JsonNodes.object(), RpcDirection.CLIENT_TO_SERVER),
                RpcDirection.CLIENT_TO_SERVER));
        assertCode(JaErrorCode.NOT_INITIALIZED, () -> decodeNew(
                RpcRequest.server("s:initialize", "initialize", JsonNodes.object()),
                RpcDirection.SERVER_TO_CLIENT));
        assertCode(JaErrorCode.NOT_INITIALIZED, () -> decodeNew(
                RpcResponse.success("s:initialize-response", JsonNodes.object()),
                RpcDirection.CLIENT_TO_SERVER));
        assertCode(JaErrorCode.NOT_INITIALIZED, () -> decodeNew(
                RpcRequest.client("c:version", "version", JsonNodes.object()),
                RpcDirection.CLIENT_TO_SERVER));
        assertCode(JaErrorCode.NOT_INITIALIZED, () -> decodeNew(
                RpcRequest.client("c:shutdown", "shutdown", JsonNodes.object()),
                RpcDirection.CLIENT_TO_SERVER));
        assertCode(JaErrorCode.NOT_INITIALIZED, () -> decodeNew(
                RpcRequest.client("c:approx", "initialize/extra", JsonNodes.object()),
                RpcDirection.CLIENT_TO_SERVER));

        HandshakeStateMachine initializedState = new HandshakeStateMachine(
                new ServerInstanceId("srv_initialize_not_ready"));
        initializedState.acceptInitialized(initialized(TOKEN));
        byte[] initializeFrame = appendLf(jsonBytes(RpcRequest.client("c:after-challenge", "initialize",
                JsonNodes.object()).toJson()));
        assertCode(JaErrorCode.NOT_INITIALIZED, () -> new HandshakeJsonlCodec(initializedState).decode(
                initializeFrame, RpcDirection.CLIENT_TO_SERVER, LIMITS));

        String tokenBypass = "{\"jsonrpc\":\"2.0\",\"id\":\"c:token-bypass\","
                + "\"method\":\"initialize\",\"params\":{\"readyToken\":\""
                + TOKEN + "\"}}\n";
        HandshakeStateMachine malformedState = new HandshakeStateMachine(
                new ServerInstanceId("srv_initialize_token_bypass"));
        assertCode(JaErrorCode.HANDSHAKE_FAILED, () -> new HandshakeJsonlCodec(malformedState).decode(
                tokenBypass.getBytes(StandardCharsets.UTF_8), RpcDirection.CLIENT_TO_SERVER, LIMITS));
        assertEquals(HandshakeStateMachine.State.FAILED, malformedState.state());

        String invalidParams = "{\"jsonrpc\":\"2.0\",\"id\":\"c:invalid-params\","
                + "\"method\":\"initialize\",\"params\":null}\n";
        assertCode(JaErrorCode.INVALID_FRAME, () -> new HandshakeJsonlCodec(
                new HandshakeStateMachine(new ServerInstanceId("srv_initialize_invalid_params"))).decode(
                invalidParams.getBytes(StandardCharsets.UTF_8), RpcDirection.CLIENT_TO_SERVER, LIMITS));
    }

    /**
     * Verifies a ready connection forwards duplicate initialize to the
     * application layer, where ALREADY_INITIALIZED remains a use-case result.
     */
    @Test
    void readyConnectionDoesNotFabricateDuplicateInitializeError() throws Exception {
        HandshakeStateMachine state = readyState("srv_initialize_duplicate");
        RpcRequest request = assertInstanceOf(RpcRequest.class,
                new HandshakeJsonlCodec(state).decode(
                        appendLf(jsonBytes(RpcRequest.client("c:duplicate-initialize", "initialize",
                                JsonNodes.object()).toJson())),
                        RpcDirection.CLIENT_TO_SERVER, LIMITS));
        assertEquals("initialize", request.method());
        assertEquals(HandshakeStateMachine.State.READY, state.state());
    }

    /** Verifies arbitrary nested result data and readyToken keys cannot cross the guarded writer. */
    @Test
    void wholeFrameRedactionRejectsNestedTokenKeysAndValues() {
        HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_handshake_redact"));
        state.acceptInitialized(initialized(TOKEN));
        RpcNotification ready = state.publishReady(new EventId("evt_handshake_redact_ready"),
                Instant.parse("2026-08-16T00:00:01Z"));
        ObjectNode nested = JsonNodes.object().set("provider", JsonNodes.object()
                .set("diagnostic", JsonNodes.object().put("observed", TOKEN)));
        RpcResponse response = RpcResponse.success("c:redact", nested);
        assertCode(JaErrorCode.HANDSHAKE_FAILED, () -> new HandshakeJsonlCodec(state).encode(response, LIMITS));
        assertFalse(ready.toJson().toString().contains("provider"));

        String rawKey = "{\"jsonrpc\":\"2.0\",\"id\":\"c:token-key\","
                + "\"error\":{\"code\":-32017,\"message\":\"bad\",\"data\":{"
                + "\"jaCode\":\"HANDSHAKE_FAILED\",\"retryable\":false,"
                + "\"details\":{\"readyToken\":\"" + TOKEN + "\"}}}}\n";
        HandshakeStateMachine rawState = new HandshakeStateMachine(new ServerInstanceId("srv_handshake_raw"));
        rawState.acceptInitialized(initialized(TOKEN));
        rawState.publishReady(new EventId("evt_handshake_raw_ready"),
                Instant.parse("2026-08-16T00:00:03Z"));
        assertCode(JaErrorCode.HANDSHAKE_FAILED, () -> new HandshakeJsonlCodec(rawState).decode(
                rawKey.getBytes(StandardCharsets.UTF_8), RpcDirection.SERVER_TO_CLIENT, LIMITS));
    }

    /**
     * Verifies raw parser methods and the connection guard are not callable
     * from application/bootstrap packages, leaving one guarded frame facade.
     */
    @Test
    void productionFrameApiCannotBypassHandshake() throws Exception {
        assertThrows(ClassNotFoundException.class,
                () -> Class.forName("io.github.kongweiguang.ja.protocol.JsonlCodec"));
        assertThrows(ClassNotFoundException.class,
                () -> Class.forName("io.github.kongweiguang.ja.application.HandshakeFrameGuard"));
        for (var method : HandshakeJsonlCodec.class.getDeclaredMethods()) {
            assertFalse(method.getName().toLowerCase(java.util.Locale.ROOT).contains("raw"), method.getName());
        }
        assertThrows(NullPointerException.class, () -> new HandshakeJsonlCodec(null));
        assertEquals(HandshakeStateMachine.class,
                HandshakeJsonlCodec.class.getConstructor(HandshakeStateMachine.class)
                        .getParameterTypes()[0]);
        for (var constructor : HandshakeJsonlCodec.class.getDeclaredConstructors()) {
            assertTrue(constructor.trySetAccessible());
            assertArrayEquals(new Class<?>[]{HandshakeStateMachine.class}, constructor.getParameterTypes());
        }
    }

    /** Verifies unnamed-module reflection can invoke only the guarded production operations. */
    @Test
    void reflectiveProductionCallsStillRequireHandshakeGuard() throws Exception {
        String raw = "{\"jsonrpc\":\"2.0\",\"id\":\"s:reflective\",\"result\":{"
                + "\"nested\":{\"readyToken\":\"" + TOKEN + "\"}}}\n";
        RpcResponse nested = RpcResponse.success("c:reflective", JsonNodes.object()
                .set("nested", JsonNodes.object().put("readyToken", TOKEN)));

        var encode = HandshakeJsonlCodec.class.getDeclaredMethod("encode", RpcEnvelope.class,
                ProtocolLimits.class);
        var encodeCodec = HandshakeJsonlCodec.class.getConstructor(HandshakeStateMachine.class)
                .newInstance(readyState("srv_reflective_encode"));
        var encodeFailure = assertThrows(java.lang.reflect.InvocationTargetException.class,
                () -> encode.invoke(encodeCodec, nested, LIMITS));
        assertEquals(JaErrorCode.HANDSHAKE_FAILED,
                ((ProtocolException) encodeFailure.getCause()).code());

        var decode = HandshakeJsonlCodec.class.getDeclaredMethod("decode", byte[].class,
                RpcDirection.class, ProtocolLimits.class);
        var decodeCodec = HandshakeJsonlCodec.class.getConstructor(HandshakeStateMachine.class)
                .newInstance(readyState("srv_reflective_decode"));
        var decodeFailure = assertThrows(java.lang.reflect.InvocationTargetException.class,
                () -> decode.invoke(decodeCodec, raw.getBytes(StandardCharsets.UTF_8),
                        RpcDirection.CLIENT_TO_SERVER, LIMITS));
        assertEquals(JaErrorCode.HANDSHAKE_FAILED,
                ((ProtocolException) decodeFailure.getCause()).code());

        var readFrame = HandshakeJsonlCodec.class.getDeclaredMethod("readFrame", java.io.InputStream.class,
                RpcDirection.class, ProtocolLimits.class);
        var readCodec = HandshakeJsonlCodec.class.getConstructor(HandshakeStateMachine.class)
                .newInstance(readyState("srv_reflective_read"));
        var readFailure = assertThrows(java.lang.reflect.InvocationTargetException.class,
                () -> readFrame.invoke(readCodec, new ByteArrayInputStream(raw.getBytes(StandardCharsets.UTF_8)),
                        RpcDirection.CLIENT_TO_SERVER, LIMITS));
        assertEquals(JaErrorCode.HANDSHAKE_FAILED,
                ((ProtocolException) readFailure.getCause()).code());
    }

    /** Verifies the guarded reader applies lifecycle admission after parsing one frame. */
    @Test
    void guardedReaderUsesTheBoundStateMachine() throws Exception {
        HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_guarded_reader"));
        state.acceptInitialized(initialized(TOKEN));
        state.publishReady(new EventId("evt_guarded_reader_ready"),
                Instant.parse("2026-08-16T00:00:01Z"));
        RpcRequest request = RpcRequest.client("c:guarded-read", "turn/list", JsonNodes.object());
        byte[] frame = appendLf(jsonBytes(request.toJson()));
        Optional<RpcEnvelope> decoded = new HandshakeJsonlCodec(state).readFrame(
                new ByteArrayInputStream(frame), RpcDirection.CLIENT_TO_SERVER, LIMITS);
        assertTrue(decoded.isPresent());
        assertEquals("turn/list", assertInstanceOf(RpcRequest.class, decoded.orElseThrow()).method());
    }

    /** Verifies every production frame entry rejects duplicate and nested challenge bypasses. */
    @Test
    void everyProductionFrameEntryRejectsChallengeBypasses() throws Exception {
        HandshakeStateMachine encodeState = readyState("srv_guarded_encode");
        RpcResponse nested = RpcResponse.success("c:nested", JsonNodes.object()
                .set("tool", JsonNodes.object().put("readyToken", TOKEN)));
        assertCode(JaErrorCode.HANDSHAKE_FAILED,
                () -> new HandshakeJsonlCodec(encodeState).encode(nested, LIMITS));

        String raw = "{\"jsonrpc\":\"2.0\",\"id\":\"s:nested\",\"result\":{"
                + "\"provider\":{\"readyToken\":\"" + TOKEN + "\"}}}\n";
        assertCode(JaErrorCode.HANDSHAKE_FAILED,
                () -> new HandshakeJsonlCodec(readyState("srv_guarded_decode")).decode(
                        raw.getBytes(StandardCharsets.UTF_8), RpcDirection.CLIENT_TO_SERVER, LIMITS));
        assertCode(JaErrorCode.HANDSHAKE_FAILED,
                () -> new HandshakeJsonlCodec(readyState("srv_guarded_read")).readFrame(
                        new ByteArrayInputStream(raw.getBytes(StandardCharsets.UTF_8)),
                        RpcDirection.CLIENT_TO_SERVER, LIMITS));

        HandshakeStateMachine duplicate = new HandshakeStateMachine(new ServerInstanceId("srv_duplicate_codec"));
        byte[] initializedFrame = appendLf(jsonBytes(initialized(TOKEN).toJson()));
        HandshakeJsonlCodec duplicateCodec = new HandshakeJsonlCodec(duplicate);
        duplicateCodec.decode(initializedFrame, RpcDirection.CLIENT_TO_SERVER, LIMITS);
        assertCode(JaErrorCode.HANDSHAKE_FAILED,
                () -> duplicateCodec.decode(initializedFrame, RpcDirection.CLIENT_TO_SERVER, LIMITS));
    }

    /** Verifies all 23 frozen negative cases are rejected by Java's shape/state/redaction path. */
    @Test
    void frozenInvalidHandshakeCasesAreRejected() throws Exception {
        Path fixture = projectFile("contracts", "golden", "invalid", "handshake-challenge.jsonl");
        int cases = 0;
        for (String line : Files.readAllLines(fixture, StandardCharsets.UTF_8)) {
            if (line.isBlank()) {
                continue;
            }
            JsonNode caseNode = JsonSupport.readTree(line.getBytes(StandardCharsets.UTF_8));
            HandshakeStateMachine state = new HandshakeStateMachine(
                    new ServerInstanceId("srv_invalid_case_" + cases));
            boolean rejected = false;
            for (JsonNode frame : caseNode.path("frames")) {
                try {
                    processFrame(state, (ObjectNode) frame);
                } catch (ProtocolException expected) {
                    rejected = true;
                    break;
                }
            }
            if (!rejected && state.state() != HandshakeStateMachine.State.READY) {
                rejected = true;
            }
            assertTrue(rejected, caseNode.path("case").asText());
            cases++;
        }
        assertEquals(23, cases);
    }

    /** Processes one fixture frame using the direction and lifecycle role frozen by the contract. */
    private static void processFrame(HandshakeStateMachine state, ObjectNode frame) {
        String method = frame.path("method").asText("");
        RpcDirection direction = "initialized".equals(method)
                ? RpcDirection.CLIENT_TO_SERVER : RpcDirection.SERVER_TO_CLIENT;
        byte[] bytes = appendLf(jsonBytes(frame));
        if ("runtime/statusChanged".equals(method)
                && "ready".equals(frame.path("params").path("status").asText())) {
            ObjectNode ready = readObject(bytes);
            state.publishReady(RuntimeStatusChangedParams.fromJson(ready.path("params")));
            return;
        }
        new HandshakeJsonlCodec(state).decode(bytes, direction, LIMITS);
    }

    /** Creates a strict inbound challenge notification without logging its value. */
    private static RpcNotification initialized(String token) {
        return new RpcNotification("initialized", JsonNodes.object().put("readyToken", token),
                RpcDirection.CLIENT_TO_SERVER);
    }

    /** Decodes a fresh NEW-state frame so one negative admission cannot affect another. */
    private static RpcEnvelope decodeNew(RpcEnvelope envelope, RpcDirection direction) {
        HandshakeStateMachine state = new HandshakeStateMachine(
                new ServerInstanceId("srv_initialize_negative_" + direction + "_"
                        + envelope.getClass().getSimpleName()));
        return new HandshakeJsonlCodec(state).decode(
                appendLf(jsonBytes(envelope.toJson())), direction, LIMITS);
    }

    /** Creates a ready state for one independent production codec assertion. */
    private static HandshakeStateMachine readyState(String serverId) {
        HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId(serverId));
        state.acceptInitialized(initialized(TOKEN));
        state.publishReady(new EventId("evt_" + serverId), Instant.parse("2026-08-16T00:00:04Z"));
        return state;
    }

    /** Adds exactly one LF after fixture JSON so the codec sees one complete stdio frame. */
    private static byte[] appendLf(byte[] json) {
        byte[] frame = java.util.Arrays.copyOf(json, json.length + 1);
        frame[frame.length - 1] = '\n';
        return frame;
    }

    /** Converts a fixture object through the strict mapper without allowing checked parser details to escape. */
    private static byte[] jsonBytes(ObjectNode frame) {
        try {
            return JsonSupport.write(frame);
        } catch (java.io.IOException exception) {
            throw new AssertionError("fixture serialization failed", exception);
        }
    }

    /** Parses a fixture object only for test setup without exposing a raw production codec. */
    private static ObjectNode readObject(byte[] bytes) {
        try {
            return (ObjectNode) JsonSupport.readTree(bytes);
        } catch (java.io.IOException exception) {
            throw new AssertionError("fixture parsing failed", exception);
        }
    }

    /** Locates repository fixtures from Maven's module working directory. */
    private static Path projectFile(String... parts) {
        Path current = Path.of(System.getProperty("user.dir")).toAbsolutePath();
        for (int depth = 0; depth < 4 && current != null; depth++, current = current.getParent()) {
            Path candidate = current;
            for (String part : parts) {
                candidate = candidate.resolve(part);
            }
            if (Files.isRegularFile(candidate)) {
                return candidate;
            }
        }
        throw new IllegalStateException("fixture not found");
    }

    /** Asserts the stable protocol code without coupling tests to public error messages. */
    private static void assertCode(JaErrorCode expected, org.junit.jupiter.api.function.Executable executable) {
        ProtocolException failure = assertThrows(ProtocolException.class, executable);
        assertEquals(expected, failure.code());
    }
}
