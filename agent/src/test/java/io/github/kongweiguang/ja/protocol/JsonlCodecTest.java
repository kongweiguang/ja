// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import io.github.kongweiguang.ja.application.HandshakeStateMachine;
import io.github.kongweiguang.ja.domain.EventId;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import org.junit.jupiter.api.Test;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Instant;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.Executors;

import static org.junit.jupiter.api.Assertions.*;

/** Golden and boundary tests keep the JSONL wire contract stricter than Jackson defaults. */
class JsonlCodecTest {
    private static final ProtocolLimits LIMITS = ProtocolLimits.defaults();
    private static final String TOKEN = "0123456789abcdef0123456789abcdef";

    /** Verifies both full-duplex request/response id-prefix directions. */
    @Test
    void acceptsOnlyTheCorrectRequestAndResponseDirectionMatrix() {
        String clientRequest = "{\"jsonrpc\":\"2.0\",\"id\":\"c:req\",\"method\":\"version\",\"params\":{}}\n";
        String clientResponse = "{\"jsonrpc\":\"2.0\",\"id\":\"s:resp\",\"result\":{}}\n";
        String serverRequest = "{\"jsonrpc\":\"2.0\",\"id\":\"s:req\",\"method\":\"approval/request\",\"params\":{}}\n";
        String serverResponse = "{\"jsonrpc\":\"2.0\",\"id\":\"c:resp\",\"result\":{}}\n";
        assertInstanceOf(RpcRequest.class, decode(clientRequest, RpcDirection.CLIENT_TO_SERVER));
        assertInstanceOf(RpcResponse.class, decode(clientResponse, RpcDirection.CLIENT_TO_SERVER));
        assertInstanceOf(RpcRequest.class, decode(serverRequest, RpcDirection.SERVER_TO_CLIENT));
        assertInstanceOf(RpcResponse.class, decode(serverResponse, RpcDirection.SERVER_TO_CLIENT));
        assertThrows(ProtocolException.class, () -> decode(serverRequest, RpcDirection.CLIENT_TO_SERVER));
        assertThrows(ProtocolException.class, () -> decode(clientResponse, RpcDirection.SERVER_TO_CLIENT));
        assertThrows(ProtocolException.class, () -> decode(clientRequest, RpcDirection.SERVER_TO_CLIENT));
        assertThrows(ProtocolException.class, () -> decode(serverResponse, RpcDirection.CLIENT_TO_SERVER));
    }

    /** Verifies unknown minor fields remain available to a forward-compatible caller. */
    @Test
    void decodesFrozenGoldenFramesAndPreservesUnknownMinorFields() throws Exception {
        Path fixture = fixture("version", "minor-compatible.json");
        String frame = Files.readString(fixture, StandardCharsets.UTF_8).trim() + "\n";
        RpcRequest request = assertInstanceOf(RpcRequest.class,
                decode(frame, RpcDirection.CLIENT_TO_SERVER));
        assertEquals("initialize", request.method());
        assertEquals("old-client-ignores-this", request.params().path("futureOptionalField").textValue());
    }

    /** Verifies only LF-delimited frames reach JSON parsing. */
    @Test
    void readsOnlyLfTerminatedFrames() throws Exception {
        byte[] valid = "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\"0123456789abcdef0123456789abcdef\"}}\n"
                .getBytes(StandardCharsets.UTF_8);
        HandshakeStateMachine initializedState = new HandshakeStateMachine(new ServerInstanceId("srv_lf_initialized"));
        assertTrue(new HandshakeJsonlCodec(initializedState).readFrame(
                new ByteArrayInputStream(valid), RpcDirection.CLIENT_TO_SERVER, LIMITS).isPresent());
        assertThrows(ProtocolException.class, () -> new HandshakeJsonlCodec(
                new HandshakeStateMachine(new ServerInstanceId("srv_lf_unterminated"))).readFrame(
                new ByteArrayInputStream(java.util.Arrays.copyOf(valid, valid.length - 1)),
                RpcDirection.CLIENT_TO_SERVER, LIMITS));
        assertThrows(ProtocolException.class, () -> new HandshakeJsonlCodec(
                new HandshakeStateMachine(new ServerInstanceId("srv_lf_cr"))).decode(
                "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\"0123456789abcdef0123456789abcdef\"}}\r\n"
                        .getBytes(StandardCharsets.UTF_8), RpcDirection.CLIENT_TO_SERVER, LIMITS));
    }

    /** Verifies duplicate keys, one-of collisions, wrong directions, and null params fail closed. */
    @Test
    void rejectsDuplicateKeysInvalidOneOfWrongDirectionAndNullParams() {
        assertThrows(ProtocolException.class, () -> decode("{\"jsonrpc\":\"2.0\",\"id\":\"c:x\","
                + "\"method\":\"version\",\"params\":{},\"params\":{}}\n"));
        assertThrows(ProtocolException.class, () -> decode("{\"jsonrpc\":\"2.0\",\"id\":\"c:x\","
                + "\"result\":{},\"error\":{}}\n"));
        assertThrows(ProtocolException.class, () -> decode("{\"jsonrpc\":\"2.0\",\"id\":\"s:x\","
                + "\"method\":\"version\",\"params\":{}}\n"));
        assertThrows(ProtocolException.class, () -> decode("{\"jsonrpc\":\"2.0\",\"id\":\"c:x\","
                + "\"method\":\"turn/start\",\"params\":null}\n"));
    }

    /** Verifies JSON null remains a present result distinct from an absent result. */
    @Test
    void distinguishesPresentNullResultFromMissingResult() {
        RpcResponse response = assertInstanceOf(RpcResponse.class, decode(
                "{\"jsonrpc\":\"2.0\",\"id\":\"s:null\",\"result\":null}\n"));
        assertTrue(response.resultPresent());
        assertNull(response.result());
        assertThrows(ProtocolException.class, () -> decode("{\"jsonrpc\":\"2.0\",\"id\":\"s:missing\"}\n"));
    }

    /** Verifies strict UTF-8 decoding and negotiated byte limits. */
    @Test
    void enforcesUtf8FrameAndConfiguredByteLimit() {
        byte[] invalidUtf8 = new byte[] {'{', '}', (byte) 0xc3, '\n'};
        assertThrows(ProtocolException.class, () -> new HandshakeJsonlCodec(readyState()).decode(
                invalidUtf8, RpcDirection.CLIENT_TO_SERVER, LIMITS));
        String large = "x".repeat(LIMITS.maxFrameBytes());
        RpcNotification notification = new RpcNotification("runtime/notice",
                JsonNodeFactory.instance.objectNode().put("message", large), RpcDirection.SERVER_TO_CLIENT);
        assertThrows(ProtocolException.class, () -> encode(notification));
        ProtocolLimits tooSmall = new ProtocolLimits(1024, 1, 1, 1, 1, 256, 1024,
                4096, 1000, 1000);
        assertThrows(ProtocolException.class, () -> new HandshakeJsonlCodec(readyState()).encode(notification, tooSmall));
        assertThrows(IllegalArgumentException.class, () -> new ProtocolLimits(1024, 1, 1, 1, 1,
                ProtocolLimits.MAX_ITEM_DELTA_BYTES + 1, 1024, 4096, 1000, 1000));
    }

    /** Verifies wire errors use redacted bounded fields and fixed schema ranges. */
    @Test
    void serializesRedactedErrorsAndRejectsSchemaDrift() {
        RpcError safe = RpcError.of(JaErrorCode.INVALID_FRAME, "diag_fixture");
        byte[] bytes = encode(RpcResponse.failure("c:error", safe));
        String json = new String(bytes, StandardCharsets.UTF_8);
        assertTrue(json.contains("diag_fixture"));
        assertFalse(json.contains("C:\\"));
        assertEquals(-32001, JaErrorCode.INVALID_FRAME.wireCode());
        assertEquals(-32008, JaErrorCode.QUEUE_FULL.wireCode());
        assertEquals(-32033, JaErrorCode.TURN_NOT_ACTIVE.wireCode());
        assertEquals(-32070, JaErrorCode.CAPABILITY_UNSUPPORTED.wireCode());
        assertEquals(-32080, JaErrorCode.INTERNAL_ERROR.wireCode());
        assertEquals(JaErrorCode.values().length,
                java.util.Arrays.stream(JaErrorCode.values()).map(JaErrorCode::wireCode).distinct().count());
        assertThrows(IllegalArgumentException.class,
                () -> new RpcError(-31999, "bad", new RpcErrorData("BAD_CODE", false, null, null, null)));
        assertThrows(IllegalArgumentException.class,
                () -> new RpcError(-32000, "x".repeat(513), new RpcErrorData("X_CODE", false, null, null, null)));
        assertThrows(IllegalArgumentException.class,
                () -> new RpcError(-32001, "bad", new RpcErrorData("QUEUE_FULL", true, null, null, null)));
        assertThrows(IllegalArgumentException.class,
                () -> new RpcErrorData("X_CODE", false, "diagnostic", null, null));
        assertThrows(IllegalArgumentException.class,
                () -> new RpcErrorData("PEER_RETRYABLE", true, null, null, null));
        assertThrows(ProtocolException.class, () -> decode(
                "{\"jsonrpc\":\"2.0\",\"id\":\"s:overflow\",\"error\":{"
                        + "\"code\":-32001,\"message\":\"bad\",\"data\":{"
                        + "\"jaCode\":\"INVALID_FRAME\",\"retryable\":false,"
                        + "\"retryAfterMs\":9223372036854775808}}}\n"));
        RpcResponse unknown = assertInstanceOf(RpcResponse.class, decode(
                "{\"jsonrpc\":\"2.0\",\"id\":\"s:unknown_code\",\"error\":{"
                        + "\"code\":-32001,\"message\":\"peer secret\",\"data\":{"
                        + "\"jaCode\":\"PEER_RETRYABLE\",\"retryable\":true,"
                        + "\"diagnosticId\":\"diag_secret\",\"field\":\"secret\"}}}\n"));
        assertEquals(JaErrorCode.INTERNAL_ERROR.wireCode(), unknown.error().code());
        assertEquals("INTERNAL_ERROR", unknown.error().data().jaCode());
        assertFalse(unknown.error().data().retryable());
        assertNull(unknown.error().data().diagnosticId());
        assertThrows(ProtocolException.class, () -> decode(
                "{\"jsonrpc\":\"2.0\",\"id\":\"s:mismatch\",\"error\":{"
                        + "\"code\":-32001,\"message\":\"bad\",\"data\":{"
                        + "\"jaCode\":\"INVALID_FRAME\",\"retryable\":true}}}\n"));
        String marker = "SECRET_ERROR_MARKER";
        RpcError marked = RpcError.of(JaErrorCode.INTERNAL_ERROR, "diag_" + marker);
        assertFalse(marked.toString().contains(marker));
        assertFalse(marked.data().toString().contains(marker));
        assertFalse(new ProtocolException(JaErrorCode.INTERNAL_ERROR, "diag_" + marker,
                new IllegalStateException(marker)).toString().contains(marker));
    }

    /** Verifies every Java error mapping remains synchronized with the frozen markdown table. */
    @Test
    void errorCodesMatchFrozenContractTable() throws Exception {
        Pattern row = Pattern.compile("^\\|\\s*(-\\d+)\\s*\\|\\s*`([A-Z0-9_]+)`\\s*\\|\\s*(是|否)\\s*\\|");
        Map<Integer, ErrorContractRow> contract = new HashMap<>();
        for (String line : Files.readAllLines(projectFile("contracts", "ja-rpc", "v1", "errors.md"))) {
            Matcher match = row.matcher(line);
            if (match.find()) {
                int code = Integer.parseInt(match.group(1));
                ErrorContractRow previous = contract.put(code,
                        new ErrorContractRow(match.group(2), "是".equals(match.group(3))));
                assertNull(previous, "duplicate contract error code " + code);
            }
        }
        assertEquals(JaErrorCode.values().length, contract.size());
        for (JaErrorCode value : JaErrorCode.values()) {
            ErrorContractRow expected = contract.get(value.wireCode());
            assertNotNull(expected, "missing contract code " + value.wireCode());
            assertEquals(value.name(), expected.jaCode());
            assertEquals(value.retryable(), expected.retryable());
        }
    }

    /** Verifies responses are removed from pending state exactly once. */
    @Test
    void pendingResponsesAreConsumedExactlyOnce() {
        PendingRequestRegistry registry = new PendingRequestRegistry(2);
        RpcRequest request = RpcRequest.client("c:pending", "version", JsonNodeFactory.instance.objectNode());
        registry.register(request);
        registry.accept(RpcResponse.success(request, JsonNodeFactory.instance.objectNode()));
        assertThrows(ProtocolException.class,
                () -> registry.accept(RpcResponse.success("c:pending", JsonNodeFactory.instance.objectNode())));
        assertThrows(ProtocolException.class,
                () -> registry.accept(RpcResponse.success("c:unknown", JsonNodeFactory.instance.objectNode())));
    }

    /** Verifies inbound request ids share the generation tombstone with outbound requests. */
    @Test
    void inboundRequestIdsCannotBeReplayed() {
        PendingRequestRegistry registry = new PendingRequestRegistry(2);
        RpcRequest request = RpcRequest.client("c:inbound", "turn/start",
                JsonNodeFactory.instance.objectNode());
        assertEquals(PendingRequestRegistry.InboundAdmission.ACCEPTED,
                registry.registerInbound(request));
        assertEquals(PendingRequestRegistry.InboundAdmission.PENDING_DUPLICATE,
                registry.registerInbound(request));
        assertTrue(registry.completeInbound(request.id()));
        assertEquals(PendingRequestRegistry.InboundAdmission.REPLAY,
                registry.registerInbound(request));
        assertEquals(0, registry.pendingCount());
    }

    /** Verifies a late response can never collide with a reused id or an evicted tombstone. */
    @Test
    void pendingIdsArePermanentUntilConnectionRotation() {
        assertThrows(IllegalArgumentException.class,
                () -> new PendingRequestRegistry(1, PendingRequestRegistry.MAX_LIFETIME_IDS + 1));
        PendingRequestRegistry registry = new PendingRequestRegistry(1, 2);
        RpcRequest first = RpcRequest.client("c:reuse", "version", JsonNodeFactory.instance.objectNode());
        registry.register(first);
        assertTrue(registry.cancel(first.id()));
        assertThrows(ProtocolException.class, () -> registry.register(first));

        RpcRequest second = RpcRequest.client("c:second", "version", JsonNodeFactory.instance.objectNode());
        registry.register(second);
        assertTrue(registry.cancel(second.id()));
        assertTrue(registry.rotationRequired());
        assertThrows(ProtocolException.class, () -> registry.register(
                RpcRequest.client("c:third", "version", JsonNodeFactory.instance.objectNode())));
        assertThrows(ProtocolException.class, () -> registry.accept(
                RpcResponse.success(first.id(), JsonNodeFactory.instance.objectNode())));
        registry.closeForRotation();
        assertTrue(registry.rotationRequired());
    }

    /** Verifies each terminal pending reason maps to late rather than duplicate response. */
    @Test
    void pendingTerminalReasonsRemainDistinguishable() {
        PendingRequestRegistry registry = new PendingRequestRegistry(4);
        RpcRequest cancelled = RpcRequest.client("c:cancelled", "version", JsonNodeFactory.instance.objectNode());
        RpcRequest deadline = RpcRequest.client("c:deadline", "version", JsonNodeFactory.instance.objectNode());
        RpcRequest disconnected = RpcRequest.client("c:disconnected", "version", JsonNodeFactory.instance.objectNode());
        RpcRequest responded = RpcRequest.client("c:responded", "version", JsonNodeFactory.instance.objectNode());
        registry.register(cancelled);
        registry.register(deadline);
        registry.register(disconnected);
        registry.register(responded);
        assertTrue(registry.cancel(cancelled.id()));
        assertTrue(registry.deadline(deadline.id()));
        assertTrue(registry.disconnect(disconnected.id()));
        registry.accept(RpcResponse.success(responded, JsonNodeFactory.instance.objectNode()));
        assertCode(JaErrorCode.LATE_RESPONSE, () -> registry.accept(
                RpcResponse.success(cancelled.id(), JsonNodeFactory.instance.objectNode())));
        assertCode(JaErrorCode.LATE_RESPONSE, () -> registry.accept(
                RpcResponse.success(deadline.id(), JsonNodeFactory.instance.objectNode())));
        assertCode(JaErrorCode.LATE_RESPONSE, () -> registry.accept(
                RpcResponse.success(disconnected.id(), JsonNodeFactory.instance.objectNode())));
        assertCode(JaErrorCode.DUPLICATE_RESPONSE, () -> registry.accept(
                RpcResponse.success(responded.id(), JsonNodeFactory.instance.objectNode())));
    }

    /** Verifies null-result presence and reserved extension keys cannot be misrepresented. */
    @Test
    void responsePresenceAndExtensionsAreClosedOverWireMembers() {
        assertTrue(RpcResponse.success("c:null_result", null).resultPresent());
        assertThrows(IllegalArgumentException.class, () -> new RpcResponse("c:bad_result",
                JsonNodeFactory.instance.objectNode(), false, RpcError.of(JaErrorCode.INVALID_FRAME, null),
                JsonNodeFactory.instance.objectNode()));
        assertThrows(ProtocolException.class, () -> new RpcRequest("c:reserved", "version",
                JsonNodeFactory.instance.objectNode(), RpcDirection.CLIENT_TO_SERVER,
                JsonNodeFactory.instance.objectNode().put("id", "c:shadow")));
        assertThrows(ProtocolException.class, () -> new RpcResponse("c:reserved_response",
                JsonNodeFactory.instance.objectNode(), true, null,
                JsonNodeFactory.instance.objectNode().put("result", "shadow")));
        assertThrows(ProtocolException.class, () -> new RpcNotification("notice",
                JsonNodeFactory.instance.objectNode(), RpcDirection.SERVER_TO_CLIENT,
                JsonNodeFactory.instance.objectNode().put("id", "s:shadow")));
        assertThrows(ProtocolException.class, () -> new RpcRequest("c:reserved_request", "version",
                JsonNodeFactory.instance.objectNode(), RpcDirection.CLIENT_TO_SERVER,
                JsonNodeFactory.instance.objectNode().put("error", "shadow")));
        assertThrows(ProtocolException.class, () -> new RpcResponse("c:reserved_method",
                JsonNodeFactory.instance.objectNode(), true, null,
                JsonNodeFactory.instance.objectNode().put("method", "shadow")));
        assertThrows(ProtocolException.class, () -> decode(
                "{\"jsonrpc\":\"2.0\",\"id\":\"c:shadow\",\"method\":\"version\","
                        + "\"params\":{},\"result\":{}}\n"));
        assertThrows(ProtocolException.class, () -> decode(
                "{\"jsonrpc\":\"2.0\",\"id\":\"s:shadow\",\"result\":{},\"method\":\"version\"}\n"));

        RpcRequest request = new RpcRequest("c:extension_roundtrip", "version",
                JsonNodeFactory.instance.objectNode(), RpcDirection.CLIENT_TO_SERVER,
                JsonNodeFactory.instance.objectNode().put("futureField", "kept"));
        RpcRequest decoded = assertInstanceOf(RpcRequest.class,
                decode(new String(encode(request), StandardCharsets.UTF_8), RpcDirection.CLIENT_TO_SERVER));
        assertEquals("kept", decoded.extensions().path("futureField").textValue());
        RpcNotification notification = new RpcNotification("notice", JsonNodeFactory.instance.objectNode(),
                RpcDirection.SERVER_TO_CLIENT,
                JsonNodeFactory.instance.objectNode().put("futureNotice", "kept"));
        RpcNotification decodedNotice = assertInstanceOf(RpcNotification.class,
                decode(new String(encode(notification), StandardCharsets.UTF_8), RpcDirection.SERVER_TO_CLIENT));
        assertEquals("kept", decodedNotice.extensions().path("futureNotice").textValue());
        RpcResponse response = RpcResponse.success("s:extension_response",
                JsonNodeFactory.instance.objectNode().put("ok", true));
        RpcResponse decodedResponse = assertInstanceOf(RpcResponse.class,
                decode(new String(encode(response), StandardCharsets.UTF_8), RpcDirection.CLIENT_TO_SERVER));
        assertTrue(decodedResponse.result().path("ok").booleanValue());
        RpcResponse failure = RpcResponse.failure("s:extension_error",
                RpcError.of(JaErrorCode.INVALID_FRAME, "diag_roundtrip"));
        RpcResponse decodedFailure = assertInstanceOf(RpcResponse.class,
                decode(new String(encode(failure), StandardCharsets.UTF_8), RpcDirection.CLIENT_TO_SERVER));
        assertEquals(JaErrorCode.INVALID_FRAME.wireCode(), decodedFailure.error().code());
    }

    /** Verifies malformed escaped UTF-16 cannot enter a domain or wire tree as replacement text. */
    @Test
    void rejectsUnpairedSurrogatesAtWireBoundary() {
        assertThrows(ProtocolException.class, () -> decode(
                "{\"jsonrpc\":\"2.0\",\"id\":\"c:unicode\",\"method\":\"version\","
                        + "\"params\":{\"text\":\"\\uD800\"}}\n"));
        assertThrows(IllegalArgumentException.class, () -> new RpcNotification("notice",
                JsonNodeFactory.instance.objectNode().put("text", "\uD800"),
                RpcDirection.SERVER_TO_CLIENT));
    }

    /** Verifies fixed Jackson nesting/string budgets fail before dispatcher logic can allocate them. */
    @Test
    void fixedJsonParserConstraintsRejectDeepOrOversizedStrings() {
        String nested = "{\"jsonrpc\":\"2.0\",\"id\":\"c:deep\",\"method\":\"version\",\"params\":"
                + "[".repeat(130) + "{}" + "]".repeat(130) + "}\n";
        assertThrows(ProtocolException.class, () -> decode(nested));
        String oversized = "{\"jsonrpc\":\"2.0\",\"id\":\"c:long\",\"method\":\"version\","
                + "\"params\":{\"text\":\"" + "x".repeat(1_048_577) + "\"}}\n";
        assertThrows(ProtocolException.class, () -> decode(oversized));
    }

    /** Verifies concurrent accept/cancel cannot reopen a consumed request id. */
    @Test
    void pendingAcceptAndCancelShareOneLinearizableStateBoundary() {
        try (var pool = Executors.newFixedThreadPool(4)) {
            for (int index = 0; index < 64; index++) {
                PendingRequestRegistry registry = new PendingRequestRegistry(2);
                String id = "c:race_" + index;
                RpcRequest request = RpcRequest.client(id, "version", JsonNodeFactory.instance.objectNode());
                registry.register(request);
                CyclicBarrier start = new CyclicBarrier(3);
                CompletableFuture<Boolean> accepted = CompletableFuture.supplyAsync(() -> {
                    await(start);
                    try {
                        registry.accept(RpcResponse.success(id, JsonNodeFactory.instance.objectNode()));
                        return true;
                    } catch (ProtocolException ignored) {
                        return false;
                    }
                }, pool);
                CompletableFuture<Boolean> cancelled = CompletableFuture.supplyAsync(() -> {
                    await(start);
                    return registry.cancel(id);
                }, pool);
                await(start);
                assertEquals(1, (accepted.join() ? 1 : 0) + (cancelled.join() ? 1 : 0));
                assertEquals(0, registry.pendingCount());
                assertThrows(ProtocolException.class, () -> registry.register(request));
            }
        }
    }

    /** Uses the client-to-server direction because most fixtures model client requests. */
    private static RpcEnvelope decode(String frame) {
        return decode(frame, RpcDirection.CLIENT_TO_SERVER);
    }

    /** Decodes a frame with an explicitly selected inbound pipe direction. */
    private static RpcEnvelope decode(String frame, RpcDirection direction) {
        return new HandshakeJsonlCodec(readyState()).decode(
                frame.getBytes(StandardCharsets.UTF_8), direction, LIMITS);
    }

    /** Encodes through a fresh ready connection so the test cannot bypass the production guard. */
    private static byte[] encode(RpcEnvelope envelope) {
        return new HandshakeJsonlCodec(readyState()).encode(envelope, LIMITS);
    }

    /** Creates the minimum initialized/ready lifecycle needed for ordinary wire fixtures. */
    private static HandshakeStateMachine readyState() {
        HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_codec_test"));
        state.acceptInitialized(new RpcNotification("initialized",
                JsonNodeFactory.instance.objectNode().put("readyToken", TOKEN),
                RpcDirection.CLIENT_TO_SERVER));
        state.publishReady(new EventId("evt_codec_ready"), Instant.parse("2026-08-16T00:00:01Z"));
        return state;
    }

    /** Locates a frozen golden fixture without copying its contract semantics into the test. */
    private static Path fixture(String... parts) {
        Path current = Path.of(System.getProperty("user.dir")).toAbsolutePath();
        for (int depth = 0; depth < 4 && current != null; depth++, current = current.getParent()) {
            Path candidate = current.resolve(Path.of("contracts", "golden"));
            for (String part : parts) {
                candidate = candidate.resolve(part);
            }
            if (Files.isRegularFile(candidate)) {
                return candidate;
            }
        }
        throw new IllegalStateException("golden fixture not found");
    }

    /** Locates a repository contract so Java tests fail when the frozen error table drifts. */
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
        throw new IllegalStateException("contract file not found");
    }

    /** Holds only the stable fields needed for the frozen error mapping assertion. */
    private record ErrorContractRow(String jaCode, boolean retryable) {
    }

    /** Waits at a deterministic test gate so concurrency assertions do not depend on timing. */
    private static void await(CyclicBarrier barrier) {
        try {
            barrier.await();
        } catch (Exception exception) {
            throw new AssertionError("concurrency gate failed", exception);
        }
    }

    /** Asserts a stable protocol code without coupling the test to public exception text. */
    private static void assertCode(JaErrorCode expected, org.junit.jupiter.api.function.Executable action) {
        ProtocolException failure = assertThrows(ProtocolException.class, action);
        assertEquals(expected, failure.code());
    }
}
