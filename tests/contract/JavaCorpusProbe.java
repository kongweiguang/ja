// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.application.HandshakeStateMachine;
import io.github.kongweiguang.ja.application.InitializeWireMapper;
import io.github.kongweiguang.ja.application.NegotiatedInitialization;
import io.github.kongweiguang.ja.application.ProtocolVersion;
import io.github.kongweiguang.ja.domain.EventId;
import io.github.kongweiguang.ja.domain.ServerInstanceId;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashMap;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.Objects;

/**
 * Temporary Java-side consumer used by the cross-language contract gate.
 * It deliberately lives under tests/contract so production API boundaries are
 * not widened merely to make a test adapter convenient.
 */
public final class JavaCorpusProbe {
    private static final ProtocolLimits LIMITS = ProtocolLimits.defaults();
    private static final String FIRST_TOKEN = "0123456789abcdef0123456789abcdef";
    private static final String EXPECTED_DIGEST_ENV = "JA_CONTRACT_DIGEST";
    private static final String PROPERTY_PATH_ENV = "JA_PROPERTY_PATH";
    private static final String PROPERTY_DIGEST_ENV = "JA_PROPERTY_DIGEST";

    private JavaCorpusProbe() {
    }

    /** Runs the same fixture files through the guarded codec and bounded property corpus. */
    public static void main(String[] arguments) {
        try {
            Path golden = Path.of(arguments[0]).toAbsolutePath();
            String digest = digest(golden);
            assertCondition(digest.equals(System.getenv(EXPECTED_DIGEST_ENV)), "corpus digest mismatch");
            int[] valid = consumeValidCorpus(golden);
            int parseFrames = consumeParseCorpus(golden);
            PropertyResult property = consumePropertyCorpus(Path.of(Objects.requireNonNull(
                    System.getenv(PROPERTY_PATH_ENV), "property path")));
            assertCondition(valid[0] == 54, "valid frame count mismatch");
            assertCondition(valid[1] == 16, "method result count mismatch");
            assertCondition(parseFrames == 47, "parse frame count mismatch");
            assertCondition(property.valid() == 100 && property.invalid() == 100, "property count mismatch");
            assertCondition(property.digest().equals(System.getenv(PROPERTY_DIGEST_ENV)), "property digest mismatch");
            System.out.println("JAVA_CONTRACT_OK digest=" + digest
                    + " validFrames=" + valid[0]
                    + " methodResults=" + valid[1]
                    + " parseFrames=" + parseFrames
                    + " propertyValid=" + property.valid()
                    + " propertyInvalid=" + property.invalid()
                    + " propertyDigest=" + property.digest());
        } catch (Throwable failure) {
            // Keep the adapter itself safe: raw exception text could contain fixture tokens or paths.
            String classification = null;
            for (Throwable current = failure; current != null && classification == null; current = current.getCause()) {
                String message = current.getMessage();
                if (message != null && message.matches("(?:valid_frame|valid_handshake|invalid_case|parse_case|property)_\\d+|valid frame count mismatch|method result count mismatch|parse frame count mismatch|property count mismatch|invalid handshake count mismatch|corpus digest mismatch")) {
                    classification = message;
                }
            }
            if (classification == null) {
                classification = failure.getClass().getSimpleName();
            }
            System.err.println("JAVA_CONTRACT_FAIL classification=" + classification);
            System.exit(1);
        }
    }

    /** Reads every valid document because a subset would allow a stale implementation to pass. */
    private static int[] consumeValidCorpus(Path golden) throws IOException {
        int count = 0;
        int methodResults = 0;
        for (Path file : jsonFiles(golden)) {
            if (file.toString().contains("\\invalid\\") || file.toString().contains("/invalid/")
                    || file.getFileName().toString().equals("major-incompatible.json")
                    || file.getFileName().toString().equals("handshake.jsonl")) {
                continue;
            }
            Map<String, String> pendingMethods = new HashMap<>();
            for (ObjectNode document : documents(file)) {
                try {
                    RpcEnvelope envelope = consumeValidFrame(document);
                    if (envelope instanceof RpcRequest request) {
                        pendingMethods.put(request.id(), request.method());
                        if ("initialize".equals(request.method())) {
                            InitializeWireMapper.readParams(request.params()).toDomain();
                        }
                    } else if (envelope instanceof RpcResponse response && response.resultPresent()) {
                        String method = pendingMethods.remove(response.id());
                        assertCondition(method != null, "response without pending method");
                        validateMethodResult(method, response.result());
                        methodResults++;
                    }
                } catch (Throwable failure) {
                    throw new IllegalStateException("valid_frame_" + count, failure);
                }
                count++;
            }
        }
        consumeValidHandshake(golden.resolve("valid").resolve("handshake.jsonl"));
        return new int[]{count + 6, methodResults};
    }

    /** Routes each frame through a production codec operation while keeping handshake state explicit. */
    private static RpcEnvelope consumeValidFrame(ObjectNode document) throws IOException {
        String method = text(document, "method");
        if ("initialized".equals(method)) {
            HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_probe"));
            return new HandshakeJsonlCodec(state).decode(frameBytes(document), RpcDirection.CLIENT_TO_SERVER, LIMITS);
        }
        if (isReady(document)) {
            ObjectNode params = (ObjectNode) document.get("params");
            ServerInstanceId server = new ServerInstanceId(params.path("serverInstanceId").asText());
            HandshakeStateMachine state = new HandshakeStateMachine(server);
            state.acceptInitialized(initializedParams(params.path("readyToken").asText()));
            RpcNotification ready = state.publishReady(RuntimeStatusChangedParams.fromJson(params));
            JsonNode emitted = JsonSupport.readTree(new HandshakeJsonlCodec(state).encode(ready, LIMITS));
            assertCondition(emitted.equals(document), "ready projection mismatch");
            return ready;
        }
        HandshakeStateMachine state = readyState();
        return new HandshakeJsonlCodec(state).decode(frameBytes(document), directionFor(document), LIMITS);
    }

    /** Replays both generations through one state machine so a frame-by-frame reset cannot hide ordering bugs. */
    private static void consumeValidHandshake(Path file) throws IOException {
        List<ObjectNode> frames = documents(file);
        HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_handshake1"));
        HandshakeJsonlCodec codec = new HandshakeJsonlCodec(state);
        int generation = 1;
        int count = 0;
        for (int index = 0; index < frames.size(); index++) {
            ObjectNode frame = frames.get(index);
            try {
                if ("initialized".equals(text(frame, "method")) && state.state() == HandshakeStateMachine.State.STOPPED) {
                    generation++;
                    state.restart(generation, nextServer(frames, index));
                    codec = new HandshakeJsonlCodec(state);
                }
                if (isReady(frame)) {
                    ObjectNode params = (ObjectNode) frame.get("params");
                    RpcNotification ready = state.publishReady(RuntimeStatusChangedParams.fromJson(params));
                    assertCondition(JsonSupport.readTree(codec.encode(ready, LIMITS)).equals(frame),
                            "handshake ready projection mismatch");
                } else {
                    codec.decode(frameBytes(frame), directionFor(frame), LIMITS);
                }
            } catch (Throwable failure) {
                throw new IllegalStateException("valid_handshake_" + count, failure);
            }
            count++;
        }
        assertCondition(count == 6 && state.state() == HandshakeStateMachine.State.READY,
                "valid handshake sequence mismatch");
    }

    /** Maps every frozen method response through field-level checks so fixture additions cannot bypass Java consumption. */
    private static void validateMethodResult(String method, JsonNode result) {
        assertCondition(result != null, "method result missing");
        assertCondition(result.isObject(), "method result must be object");
        ObjectNode object = (ObjectNode) result;
        switch (method) {
            case "turn/start" -> {
                requiredBoolean(object, "accepted");
                requiredBoolean(object, "queued");
                requiredPattern(object, "turnId", "^turn_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
            }
            case "shutdown" -> {
                requiredBoolean(object, "accepted");
                requiredEnum(object, "status", "accepted", "shutting_down", "stopped");
            }
            case "skill/import" -> {
                requiredPattern(object, "skillRevision", "^skill_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
                requiredEnum(object, "status", "healthy", "degraded", "invalid");
                optionalPattern(object, "contentHash", "^[A-Fa-f0-9]{64}$");
            }
            case "skill/enable" -> {
                requiredPattern(object, "skillRevision", "^skill_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
                requiredBoolean(object, "enabled");
                optionalEnum(object, "scope", "user", "workspace", "thread");
            }
            case "skill/list" -> {
                JsonNode skills = object.get("skills");
                assertCondition(skills != null && skills.isArray(), "skill list must be array");
                for (JsonNode skill : skills) {
                    assertCondition(skill.isObject(), "skill summary must be object");
                    ObjectNode summary = (ObjectNode) skill;
                    requiredPattern(summary, "skillRevision", "^skill_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
                    requiredText(summary, "name");
                    requiredEnum(summary, "scope", "builtin", "user", "workspace", "thread");
                    requiredBoolean(summary, "enabled");
                    requiredEnum(summary, "status", "healthy", "degraded", "invalid", "disabled");
                    optionalText(summary, "description");
                }
            }
            case "mcp/save" -> {
                ObjectNode server = requiredObject(object, "server");
                requiredPattern(server, "mcpRevision", "^mcp_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
                requiredText(server, "name");
                requiredEnum(server, "transport", "stdio", "streamable_http");
                requiredText(server, "endpoint");
                requiredText(server, "protocolVersion");
                requiredEnum(server, "status", "healthy", "degraded", "unavailable", "disabled");
            }
            case "mcp/tools/read" -> {
                requiredPattern(object, "mcpRevision", "^mcp_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
                JsonNode tools = object.get("tools");
                assertCondition(tools != null && tools.isArray(), "mcp tools must be array");
                for (JsonNode tool : tools) {
                    assertCondition(tool.isObject(), "mcp tool must be object");
                    ObjectNode toolObject = (ObjectNode) tool;
                    requiredText(toolObject, "name");
                    requiredObject(toolObject, "inputSchema");
                    requiredEnum(toolObject, "policy", "allow", "ask", "deny");
                }
            }
            case "mcp/test" -> {
                requiredPattern(object, "mcpRevision", "^mcp_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
                requiredEnum(object, "status", "healthy", "degraded", "unavailable");
                optionalEnum(object, "protocolVersion", "2024-11-05", "2025-03-26", "2025-06-18");
                if (object.has("toolCount")) {
                    requiredInteger(object, "toolCount");
                }
            }
            case "secret/resolve" -> {
                requiredText(object, "secretValue");
                optionalText(object, "expiresAt");
            }
            case "approval/request" -> {
                requiredEnum(object, "decision", "allow_once", "allow_session", "deny", "expired", "disconnected");
                requiredText(object, "resolvedAt");
            }
            case "thread/read" -> {
                requiredPattern(object, "serverInstanceId", "^srv_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
                requiredObject(object, "thread");
                JsonNode items = object.get("items");
                assertCondition(items != null && items.isArray(), "thread items must be array");
                requiredInteger(object, "snapshotSeq");
            }
            case "thread/subscribe" -> {
                requiredBoolean(object, "accepted");
                requiredPattern(object, "subscriptionId", "^sub_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$");
                requiredInteger(object, "fromSeq");
            }
            case "initialize" -> validateInitializeResult(object);
            default -> throw new IllegalStateException("unmapped method result");
        }
    }

    /** Runs the production initialize DTO mapper and round-trips all mandatory negotiated fields. */
    private static void validateInitializeResult(ObjectNode object) {
        try {
            NegotiatedInitialization mapped = new NegotiatedInitialization(
                    new ProtocolVersion(object.path("protocolMajor").intValue(),
                            object.path("protocolMinor").intValue(), 0),
                    object.path("serverVersion").textValue(),
                    new ServerInstanceId(object.path("serverInstanceId").textValue()),
                    ProtocolLimits.defaults());
            ObjectNode projected = InitializeWireMapper.writeResult(mapped);
            assertCondition(projected.path("protocolMajor").equals(object.path("protocolMajor")),
                    "initialize result major mismatch");
            assertCondition(projected.path("protocolMinor").equals(object.path("protocolMinor")),
                    "initialize result minor mismatch");
            assertCondition(projected.path("serverVersion").equals(object.path("serverVersion")),
                    "initialize result version mismatch");
            assertCondition(projected.path("serverInstanceId").equals(object.path("serverInstanceId")),
                    "initialize result instance mismatch");
        } catch (RuntimeException failure) {
            throw new IllegalStateException("initialize result mapper rejected", failure);
        }
    }

    /** Requires a JSON object so nested method-specific payloads cannot degrade to scalar placeholders. */
    private static ObjectNode requiredObject(ObjectNode parent, String field) {
        JsonNode value = parent.get(field);
        assertCondition(value != null && value.isObject(), field + " must be object");
        return (ObjectNode) value;
    }

    /** Requires a non-empty text field before method-specific validation continues. */
    private static void requiredText(ObjectNode parent, String field) {
        JsonNode value = parent.get(field);
        assertCondition(value != null && value.isTextual() && !value.textValue().isEmpty(), field + " must be text");
    }

    /** Requires a bounded integral value so numeric fields cannot accept floating or null values. */
    private static void requiredInteger(ObjectNode parent, String field) {
        JsonNode value = parent.get(field);
        assertCondition(value != null && value.isIntegralNumber() && value.canConvertToLong(), field + " must be integer");
    }

    /** Requires a boolean field for accepted/queued style result flags. */
    private static void requiredBoolean(ObjectNode parent, String field) {
        JsonNode value = parent.get(field);
        assertCondition(value != null && value.isBoolean(), field + " must be boolean");
    }

    /** Requires one of the frozen enum values while keeping error details out of the marker. */
    private static void requiredEnum(ObjectNode parent, String field, String... values) {
        JsonNode value = parent.get(field);
        assertCondition(value != null && value.isTextual() && List.of(values).contains(value.textValue()), field + " enum mismatch");
    }

    /** Requires a stable protocol identifier shape for method-specific result references. */
    private static void requiredPattern(ObjectNode parent, String field, String pattern) {
        JsonNode value = parent.get(field);
        assertCondition(value != null && value.isTextual() && value.textValue().matches(pattern), field + " pattern mismatch");
    }

    /** Validates an optional enum only when the field is present, matching additionalProperties semantics. */
    private static void optionalEnum(ObjectNode parent, String field, String... values) {
        if (parent.has(field)) {
            requiredEnum(parent, field, values);
        }
    }

    /** Validates an optional text field without weakening its type when a producer includes it. */
    private static void optionalText(ObjectNode parent, String field) {
        if (parent.has(field)) {
            requiredText(parent, field);
        }
    }

    /** Validates an optional hash/reference shape when a result includes an optional digest. */
    private static void optionalPattern(ObjectNode parent, String field, String pattern) {
        if (parent.has(field)) {
            requiredPattern(parent, field, pattern);
        }
    }

    /** Replays all frozen invalid handshake cases so state/order/redaction failures are tested by Java itself. */
    private static int consumeInvalidCorpus(Path golden) throws IOException {
        Path file = golden.resolve("invalid").resolve("handshake-challenge.jsonl");
        int cases = 0;
        for (ObjectNode caseDocument : documents(file)) {
            try {
                JsonNode frameNodes = caseDocument.get("frames");
                assertCondition(frameNodes != null && frameNodes.isArray(), "invalid case frame list missing");
                HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_invalid_probe"));
                HandshakeJsonlCodec codec = new HandshakeJsonlCodec(state);
                boolean rejected = false;
                int generation = 1;
                for (JsonNode value : frameNodes) {
                    ObjectNode frame = (ObjectNode) value;
                    try {
                        if (state.state() == HandshakeStateMachine.State.STOPPED
                                && "initialized".equals(text(frame, "method"))) {
                            generation++;
                            state.restart(generation, nextServer(frameNodes));
                            codec = new HandshakeJsonlCodec(state);
                        }
                        if (isReady(frame)) {
                            state.publishReady(RuntimeStatusChangedParams.fromJson((ObjectNode) frame.get("params")));
                        } else {
                            codec.decode(frameBytes(frame), directionFor(frame), LIMITS);
                        }
                    } catch (ProtocolException expected) {
                        rejected = true;
                        break;
                    }
                }
                if (!rejected && state.state() != HandshakeStateMachine.State.READY) {
                    rejected = true;
                }
                assertCondition(rejected, "invalid case accepted");
            } catch (Throwable failure) {
                throw new IllegalStateException("invalid_case_" + cases, failure);
            }
            cases++;
        }
        assertCondition(cases == 23, "invalid handshake count mismatch");
        return cases;
    }

    /** Sends each non-handshake invalid/major case through the production codec and version mapper. */
    private static int consumeParseCorpus(Path golden) throws IOException {
        int count = consumeInvalidCorpus(golden);
        Path invalid = golden.resolve("invalid");
        for (Path file : jsonFiles(invalid)) {
            if (file.getFileName().toString().equals("handshake-challenge.jsonl")) {
                continue;
            }
            for (ObjectNode document : documents(file)) {
                try {
                    if (file.getFileName().toString().equals("duplicate-late-limit.jsonl")
                            && document.path("id").asText().startsWith("s:")) {
                        consumeLateResponse(document);
                    } else {
                        expectRejected(document, "invalid_parse");
                    }
                } catch (ProtocolException expected) {
                    // The actual codec/pending registry supplied the stable rejection classification.
                } catch (Throwable failure) {
                    throw new IllegalStateException("parse_case_" + count, failure);
                }
                count++;
            }
        }
        Path major = golden.resolve("version").resolve("major-incompatible.json");
        for (ObjectNode document : documents(major)) {
            try {
                RpcEnvelope envelope = readyCodec().decode(frameBytes(document), RpcDirection.CLIENT_TO_SERVER, LIMITS);
                RpcRequest request = (RpcRequest) envelope;
                InitializeWireMapper.readParams(request.params()).toDomain();
                throw new IllegalStateException("major version accepted");
            } catch (ProtocolException expected) {
                // Major incompatibility must remain distinct from malformed JSON framing.
            }
            count++;
        }
        Path minor = golden.resolve("version").resolve("minor-compatible.json");
        for (ObjectNode document : documents(minor)) {
            RpcRequest request = (RpcRequest) readyCodec().decode(
                    frameBytes(document), RpcDirection.CLIENT_TO_SERVER, LIMITS);
            var params = InitializeWireMapper.readParams(request.params());
            assertCondition(params.protocolMinor() == 1, "minor version was not retained");
            params.toDomain();
        }
        return count;
    }

    /** Uses the actual pending ledger to classify late and duplicate responses rather than ignoring decoded values. */
    private static void consumeLateResponse(ObjectNode document) throws IOException {
        PendingRequestRegistry pending = new PendingRequestRegistry(4);
        RpcRequest request = RpcRequest.server(document.path("id").asText(), "approval/request", JsonNodes.object());
        pending.register(request);
        pending.deadline(request.id());
        RpcResponse response = (RpcResponse) readyCodec().decode(
                frameBytes(document), RpcDirection.CLIENT_TO_SERVER, LIMITS);
        try {
            pending.accept(response);
            throw new IllegalStateException("late response accepted");
        } catch (ProtocolException expected) {
            assertCondition(expected.code() == JaErrorCode.LATE_RESPONSE,
                    "late response classification mismatch");
        }
    }

    /** Requires the production codec to reject one malformed document instead of discarding its decode result. */
    private static void expectRejected(ObjectNode document, String classification) throws IOException {
        try {
            RpcEnvelope envelope = readyCodec().decode(frameBytes(document), directionFor(document), LIMITS);
            if (envelope instanceof RpcRequest request && "initialize".equals(request.method())) {
                InitializeWireMapper.readParams(request.params()).toDomain();
            }
            throw new IllegalStateException(classification + " accepted");
        } catch (ProtocolException expected) {
            assertCondition(expected.code() != null, classification + " missing code");
        } catch (IllegalArgumentException expected) {
            // Initialize DTO construction uses IllegalArgumentException for out-of-range limits.
        }
    }

    /** Creates a production codec in ready state for standalone parse cases that are not handshake sequences. */
    private static HandshakeJsonlCodec readyCodec() {
        return new HandshakeJsonlCodec(readyState());
    }

    /** Consumes deterministic valid/invalid generated frames through the same guarded codec. */
    private record PropertyResult(int valid, int invalid, String digest) {
    }

    /** Records each production classification in order so a count-only result cannot hide a swapped case. */
    private static PropertyResult consumePropertyCorpus(Path property) throws IOException {
        int valid = 0;
        int invalid = 0;
        int index = 0;
        List<String> classifications = new ArrayList<>();
        for (ObjectNode entry : documents(property)) {
            try {
                boolean expectedValid = "valid".equals(text(entry, "kind"));
                ObjectNode frame = (ObjectNode) entry.get("frame");
                if (expectedValid) {
                    HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_property"));
                    new HandshakeJsonlCodec(state).decode(frameBytes(frame), RpcDirection.CLIENT_TO_SERVER, LIMITS);
                    classifications.add(classificationRecord(index, "valid", "accepted"));
                    valid++;
                } else {
                    HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_property"));
                    new HandshakeJsonlCodec(state).decode(frameBytes(frame), RpcDirection.CLIENT_TO_SERVER, LIMITS);
                    throw new IllegalStateException("invalid property accepted");
                }
            } catch (ProtocolException rejected) {
                boolean expectedValid = "valid".equals(text(entry, "kind"));
                assertCondition(!expectedValid, "valid property rejected");
                classifications.add(classificationRecord(index, "invalid", "rejected"));
                invalid++;
            } catch (Throwable failure) {
                throw new IllegalStateException("property_" + index, failure);
            }
            index++;
        }
        return new PropertyResult(valid, invalid, sha256Lines(classifications));
    }

    /** Emits the shared canonical record shape without embedding frame payloads or secrets in the digest input. */
    private static String classificationRecord(int index, String expected, String classification) {
        return "{\"classification\":\"" + classification + "\",\"expected\":\""
                + expected + "\",\"index\":" + index + "}\n";
    }

    /** Hashes ordered classification records with the same UTF-8/LF convention as the Python and TS runners. */
    private static String sha256Lines(List<String> lines) {
        try {
            MessageDigest digest = MessageDigest.getInstance("SHA-256");
            for (String line : lines) {
                digest.update(line.getBytes(StandardCharsets.UTF_8));
            }
            return HexFormat.of().formatHex(digest.digest());
        } catch (java.security.NoSuchAlgorithmException failure) {
            throw new IllegalStateException("sha256 unavailable", failure);
        }
    }

    /** Locates the next generation's server identity without inventing protocol state in the adapter. */
    private static ServerInstanceId nextServer(JsonNode frames) {
        for (JsonNode frame : frames) {
            if (isReady((ObjectNode) frame)) {
                return new ServerInstanceId(frame.path("params").path("serverInstanceId").asText());
            }
        }
        return new ServerInstanceId("srv_invalid_probe");
    }

    /** Selects the next generation identity before publishing its ready event. */
    private static ServerInstanceId nextServer(List<ObjectNode> frames, int initializedIndex) {
        for (int index = initializedIndex + 1; index < frames.size(); index++) {
            ObjectNode frame = frames.get(index);
            if (isReady(frame)) {
                return new ServerInstanceId(frame.path("params").path("serverInstanceId").asText());
            }
        }
        return new ServerInstanceId("srv_handshake_probe");
    }

    /** Creates a ready state only for standalone non-handshake fixture frames. */
    private static HandshakeStateMachine readyState() {
        ServerInstanceId server = new ServerInstanceId("srv_probe");
        HandshakeStateMachine state = new HandshakeStateMachine(server);
        state.acceptInitialized(initializedParams(FIRST_TOKEN));
        state.publishReady(new EventId("evt_probe_ready"), Instant.parse("2026-08-16T00:00:00Z"));
        return state;
    }

    /** Builds the minimal strict initialized DTO needed to activate a standalone codec probe. */
    private static ObjectNode initializedParams(String token) {
        return JsonNodes.object().put("readyToken", token);
    }

    /** Determines the inbound direction from frozen id ownership rules. */
    private static RpcDirection directionFor(ObjectNode document) {
        String method = text(document, "method");
        String id = text(document, "id");
        if ("initialized".equals(method) || (method != null && id != null && id.startsWith("c:"))) {
            return RpcDirection.CLIENT_TO_SERVER;
        }
        if (method != null) {
            return RpcDirection.SERVER_TO_CLIENT;
        }
        return id != null && id.startsWith("s:")
                ? RpcDirection.CLIENT_TO_SERVER : RpcDirection.SERVER_TO_CLIENT;
    }

    /** Identifies the only ready notification shape without trusting arbitrary extension fields. */
    private static boolean isReady(ObjectNode document) {
        return "runtime/statusChanged".equals(text(document, "method"))
                && "ready".equals(document.path("params").path("status").asText());
    }

    /** Serializes through the test-visible strict mapper and appends exactly one LF frame delimiter. */
    private static byte[] frameBytes(ObjectNode document) throws IOException {
        byte[] json = JsonSupport.write(document);
        byte[] frame = java.util.Arrays.copyOf(json, json.length + 1);
        frame[frame.length - 1] = '\n';
        return frame;
    }

    /** Loads JSON and JSONL documents without allowing blank-line framing to hide malformed cases. */
    private static List<ObjectNode> documents(Path file) throws IOException {
        byte[] bytes = Files.readAllBytes(file);
        String text = new String(bytes, StandardCharsets.UTF_8);
        List<ObjectNode> result = new ArrayList<>();
        for (String line : text.split("\\R")) {
            if (!line.isBlank()) {
                JsonNode parsed = JsonSupport.readTree(line.getBytes(StandardCharsets.UTF_8));
                result.add((ObjectNode) parsed);
            }
        }
        return result;
    }

    /** Returns deterministic JSON fixture order shared with the Python and Rust digest algorithms. */
    private static List<Path> jsonFiles(Path root) throws IOException {
        try (var stream = Files.walk(root)) {
            return stream.filter(Files::isRegularFile)
                    .filter(path -> path.toString().endsWith(".json") || path.toString().endsWith(".jsonl"))
                    .sorted(Comparator.comparing(path -> root.relativize(path).toString().replace('\\', '/')))
                    .toList();
        }
    }

    /** Hashes names and bytes exactly like the cross-platform runner to prove corpus identity. */
    private static String digest(Path root) throws Exception {
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        for (Path path : jsonFiles(root)) {
            String name = root.relativize(path).toString().replace('\\', '/');
            digest.update(name.getBytes(StandardCharsets.UTF_8));
            digest.update((byte) 0);
            digest.update(Files.readAllBytes(path));
            digest.update((byte) 0);
        }
        return HexFormat.of().formatHex(digest.digest());
    }

    /** Reads a textual field without emitting malformed input or secret content in failure output. */
    private static String text(ObjectNode node, String field) {
        JsonNode value = node.get(field);
        return value == null || !value.isTextual() ? null : value.textValue();
    }

    /** Keeps adapter assertions deliberately opaque so failures cannot become a diagnostic side channel. */
    private static void assertCondition(boolean condition, String classification) {
        if (!condition) {
            throw new IllegalStateException(classification);
        }
    }
}
