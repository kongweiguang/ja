/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.bootstrap;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.message.ToolResultBlock;
import io.agentscope.core.message.ToolUseBlock;
import io.agentscope.core.model.ChatResponse;
import io.agentscope.core.model.GenerateOptions;
import io.agentscope.core.model.Model;
import io.agentscope.core.model.ToolSchema;
import io.github.kongweiguang.ja.mcp.McpServerDefinition;
import io.github.kongweiguang.ja.model.ModelHandle;
import io.github.kongweiguang.ja.profiles.CapabilitySet;
import io.github.kongweiguang.ja.profiles.ModelProfile;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.runtime.StdioRuntime;
import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Clock;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;
import reactor.core.publisher.Flux;

/** Proves profile activation and MCP calls traverse the real Harness/stdio boundary. */
@Timeout(60)
final class StdioMcpActivationIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";
    private static final String FIXTURE = "io.github.kongweiguang.ja.mcp.McpRuntimeTest$StdioFixture";
    private static final String MODEL_SECRET = "model-secret-b2";
    private static final String MCP_A_SECRET = "mcp-a-secret-b2";
    private static final String MCP_B_SECRET = "mcp-b-secret-b2";

    /** Keeps short-lived MCP activation diagnostics useful without exposing resolved credentials. */
    @Test
    void mcpActivationToStringRedactsResolvedSecret() {
        String marker = "mcp-secret-marker-b2";
        McpServerDefinition definition = new McpServerDefinition(
                "mcp_redaction", "redaction", "stdio", "fixture", "2024-11-05",
                List.of(), Map.of(), Map.of(), Map.of(),
                new McpServerDefinition.Auth("env", "MCP_SECRET", "cred_redaction"), true);
        String diagnostic = new AgentScopeRuntimeGraph.McpActivation(definition, marker).toString();
        assertFalse(diagnostic.contains(marker));
        assertTrue(diagnostic.contains("<redacted>"));
    }

    /**
     * Checks the exact ordered secret contract before any provider or MCP child is started.
     * The model builder is still fake, but profile activation and secret routing are real.
     */
    @Test
    void activationResolvesModelAndMcpSecretsInProfileOrder(@TempDir Path temp) throws Exception {
        Session session = Session.open(temp, (profile, secret) -> {
            assertEquals(MODEL_SECRET, secret);
            return modelHandle(new FinalModel("activated"), profile);
        }, MODEL_SECRET, MCP_A_SECRET, MCP_B_SECRET);
        List<Long> pids = new ArrayList<>();
        try {
            session.initialize();
            session.workspace();
            Path firstPid = temp.resolve("mcp-a.pid");
            Path secondPid = temp.resolve("mcp-b.pid");
            JsonNode firstSaved = session.saveMcp("mcp_a", firstPid, "server-a", null, "cred_mcp_a");
            assertFalse(firstSaved.has("error"), firstSaved.toString());
            assertFalse(session.saveMcp("mcp_b", secondPid, "server-b", null, "cred_mcp_b")
                    .has("error"));
            assertFalse(session.saveProfile("profile_secret_order", true,
                    List.of("mcp_a", "mcp_b")).has("error"));

            session.activate("c:activate", "profile_secret_order");
            JsonNode modelRequest = session.readUntilMethod("secret/resolve");
            assertSecretRequest(modelRequest, "cred_model", "model", "profile_secret_order", null);
            session.respondSecret(modelRequest, MODEL_SECRET);

            JsonNode firstMcp = session.readUntilMethod("secret/resolve");
            assertSecretRequest(firstMcp, "cred_mcp_a", "mcp", "profile_secret_order", "mcp_a");
            session.respondSecret(firstMcp, MCP_A_SECRET);

            JsonNode secondMcp = session.readUntilMethod("secret/resolve");
            assertSecretRequest(secondMcp, "cred_mcp_b", "mcp", "profile_secret_order", "mcp_b");
            session.respondSecret(secondMcp, MCP_B_SECRET);

            JsonNode activated = session.readUntilId("c:activate");
            assertFalse(activated.has("error"), activated.toString());
            assertEquals("profile_secret_order", activated.path("result")
                    .path("activeProfileRevision").textValue());
            assertTrue(session.readOptionalId("c:activate", Duration.ofMillis(150)).isEmpty(),
                    "activation must have exactly one terminal response");
            pids.add(awaitPid(firstPid));
            pids.add(awaitPid(secondPid));
            session.shutdown();
        } finally {
            session.close();
        }
        for (long pid : pids) {
            assertTrue(awaitExit(pid), "MCP child remained after activation shutdown: " + pid);
        }
    }

    /**
     * Selects aliases from the model's actual ToolSchema list, then verifies two real approvals,
     * official MCP tools/call routing, ToolResultBlock feedback, and the final model response.
     */
    @Test
    void harnessCallsTwoAliasedMcpToolsThroughApprovalAndFinal(@TempDir Path temp) throws Exception {
        AliasedMcpModel model = new AliasedMcpModel();
        Session session = Session.open(temp, (profile, secret) -> {
            assertNull(secret);
            return modelHandle(model, profile);
        });
        List<Long> pids = new ArrayList<>();
        try {
            session.initialize();
            session.workspace();
            Path firstPid = temp.resolve("alias-a.pid");
            Path secondPid = temp.resolve("alias-b.pid");
            assertFalse(session.saveMcp("mcp_a", firstPid, "server-a", null, null).has("error"));
            assertFalse(session.saveMcp("mcp_b", secondPid, "server-b", null, null).has("error"));
            assertFalse(session.saveProfile("profile_alias_turn", false,
                    List.of("mcp_a", "mcp_b")).has("error"));
            session.activate("c:activate", "profile_alias_turn");
            JsonNode activated = session.readUntilId("c:activate");
            assertFalse(activated.has("error"), activated.toString());

            session.turn("c:turn", "profile_alias_turn");
            JsonNode firstApproval = session.readApprovalBeforeTerminal();
            assertEquals("mcp_tool", firstApproval.path("params").path("action")
                    .path("kind").textValue());
            session.allowOnce(firstApproval);
            JsonNode secondApproval = session.readApprovalBeforeTerminal();
            assertEquals("mcp_tool", secondApproval.path("params").path("action")
                    .path("kind").textValue());
            session.allowOnce(secondApproval);
            JsonNode completed = session.readUntilTurnCompleted(Duration.ofSeconds(10));
            assertEquals("completed", completed.path("params").path("turn")
                    .path("terminalStatus").textValue());
            assertEquals(List.of("server-a", "server-b"), model.toolResults());
            assertTrue(model.schemasValid(), "model must select aliases from provider-safe schemas");
            assertNull(model.failure(), () -> "fake model failed: " + model.failure());
            pids.add(awaitPid(firstPid));
            pids.add(awaitPid(secondPid));
            session.shutdown();
        } finally {
            session.close();
        }
        for (long pid : pids) {
            assertTrue(awaitExit(pid), "MCP child remained after Harness shutdown: " + pid);
        }
    }

    /**
     * Makes the second real server fail initialization once. The first child must be closed, the
     * graph must remain unavailable, and the same profile activation must succeed on retry.
     */
    @Test
    void secondMcpFailureIsAtomicAndRetryable(@TempDir Path temp) throws Exception {
        Session session = Session.open(temp, (profile, secret) ->
                modelHandle(new FinalModel("retry"), profile));
        List<Long> firstAttemptPids = new ArrayList<>();
        List<Long> retryPids = new ArrayList<>();
        try {
            session.initialize();
            session.workspace();
            Path firstPid = temp.resolve("atomic-a.pid");
            Path secondPid = temp.resolve("atomic-b.pid");
            Path failOnce = temp.resolve("atomic-b.fail-once");
            assertFalse(session.saveMcp("mcp_a", firstPid, "server-a", null, null).has("error"));
            assertFalse(session.saveMcp("mcp_b", secondPid, "server-b", failOnce, null)
                    .has("error"));
            assertFalse(session.saveProfile("profile_atomic", false,
                    List.of("mcp_a", "mcp_b")).has("error"));

            session.activate("c:activate-failed", "profile_atomic");
            JsonNode failed = session.readUntilId("c:activate-failed");
            assertEquals("MCP_SERVER_UNAVAILABLE", failed.path("error").path("data")
                    .path("jaCode").textValue());
            assertTrue(session.readOptionalId("c:activate-failed", Duration.ofMillis(150)).isEmpty(),
                    "failed activation must emit exactly one terminal response");
            JsonNode turnUnavailable = session.turn("c:turn-before-retry", "profile_atomic");
            assertEquals("MODEL_UNAVAILABLE", turnUnavailable.path("error").path("data")
                    .path("jaCode").textValue());
            firstAttemptPids.add(awaitPid(firstPid));
            firstAttemptPids.add(awaitPid(secondPid));

            session.activate("c:activate-retry", "profile_atomic");
            JsonNode retry = session.readUntilId("c:activate-retry");
            assertFalse(retry.has("error"), retry.toString());
            retryPids.add(awaitPid(firstPid));
            retryPids.add(awaitPid(secondPid));
            session.shutdown();
        } finally {
            session.close();
        }
        for (long pid : firstAttemptPids) {
            assertTrue(awaitExit(pid), "failed graph leaked child: " + pid);
        }
        for (long pid : retryPids) {
            assertTrue(awaitExit(pid), "retry graph leaked child: " + pid);
        }
    }

    /** A timed-out MCP secret becomes a tombstone; its late response cannot revive activation. */
    @Test
    void mcpSecretTimeoutIgnoresLateResponseAndAllowsRetry(@TempDir Path temp) throws Exception {
        Session session = Session.open(temp, (profile, secret) ->
                modelHandle(new FinalModel("secret-retry"), profile), MCP_A_SECRET);
        try {
            session.initialize();
            session.workspace();
            Path pid = temp.resolve("secret-timeout.pid");
            assertFalse(session.saveMcp("mcp_a", pid, "server-a", null, "cred_mcp_a")
                    .has("error"));
            assertFalse(session.saveProfile("profile_mcp_timeout", false,
                    List.of("mcp_a")).has("error"));
            session.activate("c:activate-timeout", "profile_mcp_timeout");
            JsonNode secret = session.readUntilMethod("secret/resolve");
            assertSecretRequest(secret, "cred_mcp_a", "mcp", "profile_mcp_timeout", "mcp_a");
            Thread.sleep(5_300);
            JsonNode timeout = session.readUntilId("c:activate-timeout");
            assertEquals("MCP_SERVER_UNAVAILABLE", timeout.path("error").path("data")
                    .path("jaCode").textValue());
            session.respondSecret(secret, MCP_A_SECRET);
            assertTrue(session.readOptionalId("c:activate-timeout", Duration.ofMillis(150)).isEmpty(),
                    "late response must not produce a second activation result");

            session.activate("c:activate-retry", "profile_mcp_timeout");
            JsonNode retrySecret = session.readUntilMethod("secret/resolve");
            assertFalse(secret.path("id").textValue().equals(retrySecret.path("id").textValue()));
            session.respondSecret(retrySecret, MCP_A_SECRET);
            JsonNode activated = session.readUntilId("c:activate-retry");
            assertFalse(activated.has("error"), activated.toString());
            session.shutdown();
        } finally {
            session.close();
        }
        assertTrue(awaitExit(awaitPid(temp.resolve("secret-timeout.pid"))));
    }

    /** A fresh StdioRuntime generation rebinds MCP and completes another approved real turn. */
    @Test
    void restartReactivatesAndCompletesMcpTurn(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectories(temp.resolve("workspace"));
        Path data = Files.createDirectories(temp.resolve("data"));
        Path pid = temp.resolve("restart.pid");
        runOneGeneration(workspace, data, pid, "profile_restart_one", "c:first", "first");
        long firstPid = awaitPid(pid);
        assertTrue(awaitExit(firstPid));
        runOneGeneration(workspace, data, pid, "profile_restart_two", "c:second", "second");
        long secondPid = awaitPid(pid);
        assertTrue(awaitExit(secondPid));
    }

    /** Runs one complete generation to make the restart assertion use the production graph. */
    private static void runOneGeneration(Path workspace, Path data, Path pid,
                                         String profileRevision, String requestId,
                                         String identity) throws Exception {
        RestartMcpModel model = new RestartMcpModel(identity);
        try (Session session = Session.open(workspace.getParent(), workspace, data,
                (profile, secret) -> modelHandle(model, profile))) {
            session.initialize();
            session.workspace();
            assertFalse(session.saveMcp("mcp_restart", pid, identity, null, null).has("error"));
            assertFalse(session.saveProfile(profileRevision, false,
                    List.of("mcp_restart")).has("error"));
            session.activate(requestId + "-activate", profileRevision);
            assertFalse(session.readUntilId(requestId + "-activate").has("error"));
            JsonNode turn = session.turn(requestId + "-turn", profileRevision);
            assertFalse(turn.has("error"), turn.toString());
            JsonNode approval = session.readApprovalBeforeTerminal();
            session.allowOnce(approval);
            JsonNode completed = session.readUntilTurnCompleted(Duration.ofSeconds(10));
            assertEquals("completed", completed.path("params").path("turn")
                    .path("terminalStatus").textValue());
            assertEquals(List.of(identity), model.toolResults());
            session.shutdown();
        }
    }

    /** Asserts only the purpose-bound fields permitted on a model/MCP secret request. */
    private static void assertSecretRequest(JsonNode request, String credentialRef, String purpose,
                                            String profileRevision, String mcpRevision) {
        JsonNode params = request.path("params");
        assertEquals("secret/resolve", request.path("method").textValue());
        assertEquals(mcpRevision == null ? 3 : 4, params.size());
        assertEquals(credentialRef, params.path("credentialRef").textValue());
        assertEquals(purpose, params.path("purpose").textValue());
        assertEquals(profileRevision, params.path("profileRevision").textValue());
        if (mcpRevision == null) {
            assertFalse(params.has("mcpRevision"));
        } else {
            assertEquals(mcpRevision, params.path("mcpRevision").textValue());
        }
    }

    /** Creates the model handle while retaining the real AgentScope Harness graph. */
    private static ModelHandle modelHandle(Model model, ModelProfile profile) {
        return new ModelHandle(model, profile.fingerprint(),
                CapabilitySet.defaults(profile.provider(), profile.api()));
    }

    /** Reads the upstream message accessor so the fake model advances only after a real tool result. */
    private static List<String> toolResultTexts(List<Msg> messages) {
        return toolResultTexts(messages, null);
    }

    /** Filters resumed-generation results by ToolUse id so persisted history cannot skip a call. */
    private static List<String> toolResultTexts(List<Msg> messages, String toolCallId) {
        List<String> texts = new ArrayList<>();
        for (Msg message : messages) {
            for (ToolResultBlock result : message.getContentBlocks(ToolResultBlock.class)) {
                if (toolCallId != null && !toolCallId.equals(result.getId())) {
                    continue;
                }
                assertNotNull(result.getOutput(), "AgentScope returned a null MCP result output");
                String text = result.getOutput().stream()
                        .filter(TextBlock.class::isInstance)
                        .map(TextBlock.class::cast)
                        .map(TextBlock::getText)
                        .reduce("", String::concat);
                assertFalse(text.isBlank(), "MCP tool result must contain fixture identity");
                texts.add(text);
            }
        }
        return texts;
    }

    /** Filters both unique call id and fixture identity so persisted prior-generation results cannot advance. */
    private static List<String> toolResultTextsForGeneration(List<Msg> messages,
                                                               String toolCallId,
                                                               String identity) {
        List<String> exact = toolResultTexts(messages, toolCallId).stream()
                .filter(identity::equals).toList();
        if (!exact.isEmpty()) {
            return exact;
        }
        // AgentScope may replace a provider id with an operation id while rebuilding a persisted
        // turn; the fixture identity is the stable generation marker in that upstream path.
        return toolResultTexts(messages).stream().filter(identity::equals).toList();
    }

    /** Returns the Java executable explicitly so stdio fixtures do not depend on PATH. */
    private static String javaExecutable() {
        return Path.of(System.getProperty("java.home"), "bin",
                System.getProperty("os.name", "").toLowerCase().contains("win")
                        ? "java.exe" : "java").toString();
    }

    /** Builds a short absolute child classpath because the MCP process receives a restricted cwd. */
    private static String absoluteClasspath() {
        Path base = Path.of(System.getProperty("user.dir", ".")).toAbsolutePath().normalize();
        String separator = System.getProperty("path.separator");
        return base.resolve("target/test-classes").toString()
                + separator + base.resolve("target/classes");
    }

    /** Waits for a fixture to publish its process identity before the graph is closed. */
    private static long awaitPid(Path pidFile) throws Exception {
        for (int attempt = 0; attempt < 200; attempt++) {
            if (Files.exists(pidFile) && Files.size(pidFile) > 0) {
                return Long.parseLong(Files.readString(pidFile));
            }
            Thread.sleep(20);
        }
        throw new AssertionError("MCP fixture did not publish pid: " + pidFile);
    }

    /** Confirms official MCP transport cleanup without introducing a JA process manager. */
    private static boolean awaitExit(long pid) throws InterruptedException {
        for (int attempt = 0; attempt < 200; attempt++) {
            if (ProcessHandle.of(pid).map(handle -> !handle.isAlive()).orElse(true)) {
                return true;
            }
            Thread.sleep(20);
        }
        return false;
    }

    /** Builds the base initialize frame used by every session in this class. */
    private static String initializeFrame() throws Exception {
        ObjectNode params = JsonNodes.object();
        params.put("protocolMajor", 1);
        params.put("protocolMinor", 0);
        params.put("minimumCompatibleMinor", 0);
        params.put("clientVersion", "b2-mcp-activation-test");
        params.set("capabilities", JSON.readTree(
                "{\"methods\":[\"initialize\",\"health/read\",\"workspace/open\","
                        + "\"profile/save\",\"profile/activate\",\"turn/start\",\"shutdown\"],"
                        + "\"events\":[\"runtime/statusChanged\",\"turn/started\","
                        + "\"item/started\",\"item/delta\",\"item/completed\",\"turn/completed\"],"
                        + "\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"],"
                        + "\"itemKinds\":[\"agent_message\",\"approval\"],\"mcp\":{"
                        + "\"protocolVersions\":[\"2024-11-05\"],\"transports\":[\"stdio\"],"
                        + "\"features\":[\"tools_list\",\"tools_call\"]}}"));
        params.set("limits", JSON.readTree(
                "{\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,"
                        + "\"maxOutboundQueueFrames\":1024,\"maxInFlightRequests\":64,"
                        + "\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536,"
                        + "\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576,"
                        + "\"defaultRequestDeadlineMs\":120000,\"defaultApprovalDeadlineMs\":300000}"));
        return request("c:init", "initialize", params);
    }

    /** Completes the handshake challenge before profile and turn requests are admitted. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\""
                + READY_TOKEN + "\"}}";
    }

    /** Serializes one JSON-RPC request without allowing secret values into a frame. */
    private static String request(String id, String method, JsonNode params) throws Exception {
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", method, "params", params));
    }

    /** Emits the graceful shutdown request through the same client writer as all other frames. */
    private static String shutdownFrame() throws Exception {
        return request("c:stop", "shutdown", JsonNodes.object());
    }

    /** Emits one approval result accepted by the existing StdioRuntime parser. */
    private static String approvalResponse(String id) throws Exception {
        ObjectNode result = JsonNodes.object();
        result.put("decision", "allow_once");
        result.put("resolvedAt", "2026-08-18T00:00:00Z");
        return response(id, result);
    }

    /** Emits one opaque secret response and never logs or embeds the value elsewhere. */
    private static String secretResponse(String id, String value) throws Exception {
        return response(id, JsonNodes.object().put("secretValue", value));
    }

    /** Serializes a JSON-RPC response without a method so PendingRequestRegistry can consume it. */
    private static String response(String id, JsonNode result) throws Exception {
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id, "result", result));
    }

    /** Provides a minimal deterministic final model for activation-only tests. */
    private static final class FinalModel implements Model {
        private final String text;

        private FinalModel(String text) {
            this.text = text;
        }

        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            return Flux.just(new ChatResponse("final-" + text,
                    List.of(TextBlock.builder().text(text).build()), null, Map.of(), "stop"));
        }

        /** Returns a stable identity for persisted AgentScope state. */
        @Override
        public String getModelName() {
            return "ja-b2-final-model";
        }
    }

    /** Selects aliases from ToolSchema and waits for real ToolResultBlock feedback between calls. */
    private static final class AliasedMcpModel implements Model {
        private final List<String> results = new CopyOnWriteArrayList<>();
        private final AtomicReference<Throwable> failure = new AtomicReference<>();
        private volatile boolean schemasValid;

        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            try {
                // AgentScope's optional memory-maintenance call has no tool list; only the real
                // Harness turn is allowed to select an MCP alias from its provider schema.
                if (tools == null) {
                    return Flux.empty();
                }
                List<ToolSchema> aliases = tools.stream()
                        .filter(schema -> schema.getName().startsWith("ja_mcp_"))
                        .toList();
                if (aliases.size() != 2) {
                    throw new AssertionError("expected two MCP aliases, got " + aliases);
                }
                schemasValid = aliases.get(0).getName().matches("[A-Za-z0-9_-]{1,64}")
                        && aliases.get(1).getName().matches("[A-Za-z0-9_-]{1,64}")
                        && !aliases.get(0).getName().equals(aliases.get(1).getName());
                for (String text : toolResultTexts(messages)) {
                    if (!results.contains(text)) {
                        results.add(text);
                    }
                }
                assertTrue(results.stream().allMatch(value -> value.equals("server-a")
                        || value.equals("server-b")), "unexpected MCP result: " + results);
                if (!results.contains("server-a")) {
                    String alias = aliases.stream().map(ToolSchema::getName)
                            .filter(name -> name.startsWith("ja_mcp_a_"))
                            .findFirst().orElseThrow();
                    return Flux.just(toolCall("mcp-a-call", alias));
                }
                if (!results.contains("server-b")) {
                    assertTrue(results.contains("server-a"), "first MCP result was not routed");
                    String alias = aliases.stream().map(ToolSchema::getName)
                            .filter(name -> name.startsWith("ja_mcp_b_"))
                            .findFirst().orElseThrow();
                    return Flux.just(toolCall("mcp-b-call", alias));
                }
                assertTrue(results.contains("server-a"));
                assertTrue(results.contains("server-b"));
                return Flux.just(new ChatResponse("mcp-final",
                        List.of(TextBlock.builder().text("MCP tools completed").build()),
                        null, Map.of(), "stop"));
            } catch (Throwable error) {
                failure.set(error);
                return Flux.error(error);
            }
        }

        /** Emits one provider-style ToolUseBlock chosen from the current schema list. */
        private static ChatResponse toolCall(String id, String alias) {
            return new ChatResponse(id, List.of(new ToolUseBlock(id, alias, Map.of(), "{}", Map.of())),
                    null, Map.of(), "tool_calls");
        }

        /** Gives AgentScope a stable provider identity for the test session. */
        @Override
        public String getModelName() {
            return "ja-b2-aliased-mcp-model";
        }

        /** Returns the identities observed in real ToolResultBlock messages. */
        List<String> toolResults() {
            return List.copyOf(results);
        }

        /** Indicates whether both names came from the current provider-safe ToolSchema list. */
        boolean schemasValid() {
            return schemasValid;
        }

        /** Returns the first model-side failure for a deterministic test assertion. */
        Throwable failure() {
            return failure.get();
        }
    }

    /** Selects one MCP alias and records the returned identity for restart assertions. */
    private static final class RestartMcpModel implements Model {
        private final String expectedIdentity;
        private final String expectedCallId;
        private final List<String> results = new CopyOnWriteArrayList<>();

        private RestartMcpModel(String expectedIdentity) {
            this.expectedIdentity = expectedIdentity;
            this.expectedCallId = "restart-call-" + expectedIdentity;
        }

        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            // Compaction may invoke a model without tools; it must not masquerade as a resumed
            // coding turn or pollute the restart result assertion.
            if (tools == null) {
                return Flux.empty();
            }
            List<String> currentResults = toolResultTextsForGeneration(messages, expectedCallId,
                    expectedIdentity);
            for (String text : currentResults) {
                if (!results.contains(text)) {
                    results.add(text);
                }
            }
            if (currentResults.isEmpty()) {
                String alias = tools.stream().map(ToolSchema::getName)
                        .filter(name -> name.startsWith("ja_mcp_restart_"))
                        .findFirst().orElseThrow();
                return Flux.just(new ChatResponse(expectedCallId,
                        List.of(new ToolUseBlock(expectedCallId, alias, Map.of(), "{}", Map.of())),
                        null, Map.of(), "tool_calls"));
            }
            assertEquals(List.of(expectedIdentity), currentResults,
                    "restart turn must consume the selected MCP result before final");
            return Flux.just(new ChatResponse("restart-final",
                    List.of(TextBlock.builder().text("restart complete").build()),
                    null, Map.of(), "stop"));
        }

        /** Gives AgentScope a stable provider identity across graph generations. */
        @Override
        public String getModelName() {
            return "ja-b2-restart-mcp-model";
        }

        /** Returns the real MCP identity consumed by the restarted Harness turn. */
        List<String> toolResults() {
            return List.copyOf(results);
        }
    }

    /** Full-duplex client/session wrapper with stdout secret-leak and response-correlation guards. */
    private static final class Session implements AutoCloseable {
        private final PipedOutputStream clientInput;
        private final PipedInputStream clientOutput;
        private final BufferedWriter writer;
        private final BufferedReader reader;
        private final StdioRuntime runtime;
        private final CompletableFuture<Integer> exit;
        private final Map<String, JsonNode> stash = new HashMap<>();
        private final List<String> forbidden;
        private final Path workspace;
        private boolean shutdown;

        private Session(PipedOutputStream clientInput, PipedInputStream clientOutput,
                        BufferedWriter writer, BufferedReader reader, StdioRuntime runtime,
                        CompletableFuture<Integer> exit, List<String> forbidden, Path workspace) {
            this.clientInput = clientInput;
            this.clientOutput = clientOutput;
            this.writer = writer;
            this.reader = reader;
            this.runtime = runtime;
            this.exit = exit;
            this.forbidden = List.copyOf(forbidden);
            this.workspace = workspace;
        }

        /** Opens one production StdioRuntime whose model still enters the real AgentScope graph. */
        static Session open(Path temp, StdioRuntime.ModelBuilder modelBuilder,
                            String... forbidden) throws Exception {
            Path workspace = Files.createDirectories(temp.resolve("workspace"));
            Path data = Files.createDirectories(temp.resolve("data"));
            return open(temp, workspace, data, modelBuilder, forbidden);
        }

        /** Opens a generation against caller-owned paths so restart tests reuse SQLite state. */
        static Session open(Path temp, Path workspace, Path data,
                            StdioRuntime.ModelBuilder modelBuilder, String... forbidden)
                throws Exception {
            PipedOutputStream clientInput = new PipedOutputStream();
            PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
            PipedOutputStream serverOutput = new PipedOutputStream();
            PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
            StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                    new SidecarConfiguration(SidecarConfiguration.RuntimeMode.PRODUCTION, data),
                    Clock.systemUTC(), null, modelBuilder);
            CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
            return new Session(clientInput, clientOutput,
                    new BufferedWriter(new OutputStreamWriter(clientInput, StandardCharsets.UTF_8)),
                    new BufferedReader(new InputStreamReader(clientOutput, StandardCharsets.UTF_8)),
                    runtime, exit, List.of(forbidden), workspace);
        }

        /** Performs the handshake and asserts the sidecar reaches ready. */
        void initialize() throws Exception {
            send(initializeFrame());
            assertEquals("c:init", readUntilId("c:init").path("id").textValue());
            send(initializedFrame());
            assertEquals("ready", readUntilMethod("runtime/statusChanged").path("params")
                    .path("status").textValue());
        }

        /** Opens the same workspace that the activation graph will bind and clean up. */
        void workspace() throws Exception {
            ObjectNode params = JsonNodes.object();
            params.put("workspaceId", "ws_b2_mcp");
            params.put("rootPath", workspace.toString());
            params.put("trust", "trusted");
            params.put("displayName", "B2 MCP workspace");
            send(request("c:workspace", "workspace/open", params));
            assertFalse(readUntilId("c:workspace").has("error"));
        }

        /** Saves a stdio fixture with optional auth and fail-once behavior. */
        JsonNode saveMcp(String revision, Path pid, String identity, Path failOnce,
                         String credentialRef) throws Exception {
            ObjectNode server = JsonNodes.object();
            server.put("mcpRevision", revision);
            server.put("name", identity);
            server.put("transport", "stdio");
            server.put("endpoint", javaExecutable());
            server.put("protocolVersion", "2024-11-05");
            var args = JsonNodes.array().add("-cp").add(absoluteClasspath())
                    .add(FIXTURE).add(pid.toString()).add(identity);
            if (failOnce != null) {
                args.add("2024-11-05").add("fail-once").add(failOnce.toString());
            }
            server.set("args", args);
            server.set("env", JsonNodes.object());
            if (credentialRef != null) {
                server.set("auth", JsonNodes.object().put("kind", "env")
                        .put("name", "MCP_SECRET").put("credentialRef", credentialRef));
            }
            server.put("enabled", true);
            send(request("c:save-" + revision, "mcp/save",
                    JsonNodes.object().set("server", server)));
            return readUntilId("c:save-" + revision);
        }

        /** Saves a profile with either model credential resolution or a credential-free fake model. */
        JsonNode saveProfile(String revision, boolean modelSecret, List<String> mcpRevisions)
                throws Exception {
            ObjectNode model = JsonNodes.object();
            model.put("provider", "openai");
            model.put("protocol", "openai_chat_completions");
            model.put("model", "b2-fixture-model");
            if (modelSecret) {
                model.put("credentialRef", "cred_model");
            }
            ObjectNode profile = JsonNodes.object();
            profile.put("profileRevision", revision);
            profile.put("name", "B2 MCP profile");
            profile.set("model", model);
            profile.put("accessMode", "full_access");
            profile.set("skillRevisions", JsonNodes.array());
            var revisions = JsonNodes.array();
            mcpRevisions.forEach(revisions::add);
            profile.set("mcpRevisions", revisions);
            send(request("c:profile-" + revision, "profile/save",
                    JsonNodes.object().set("profile", profile)));
            return readUntilId("c:profile-" + revision);
        }

        /** Starts activation for a profile revision independent of the request id. */
        void activate(String id, String profileRevision) throws Exception {
            send(request(id, "profile/activate", JsonNodes.object()
                    .put("profileRevision", profileRevision)));
        }

        /** Starts one turn and returns its immediate admission result or stable failure. */
        JsonNode turn(String id, String profileRevision) throws Exception {
            ObjectNode params = JsonNodes.object();
            params.put("threadId", "thr_b2_mcp");
            params.put("userId", "user_b2_mcp");
            params.put("sessionId", "session_b2_mcp");
            params.put("accessMode", "full_access");
            params.put("profileRevision", profileRevision);
            params.set("input", JsonNodes.array().add(JsonNodes.object()
                    .put("type", "text").put("text", "use the configured MCP tools")));
            send(request(id, "turn/start", params));
            return readUntilId(id);
        }

        /** Resolves one real approval request using the wire's allow-once decision. */
        void allowOnce(JsonNode approval) throws Exception {
            send(approvalResponse(approval.path("id").textValue()));
        }

        /** Resolves one exact secret request while keeping the value out of stdout. */
        void respondSecret(JsonNode secret, String value) throws Exception {
            send(secretResponse(secret.path("id").textValue(), value));
        }

        /** Requests normal shutdown and waits for its response before closing pipes. */
        void shutdown() throws Exception {
            if (shutdown) {
                return;
            }
            send(shutdownFrame());
            assertEquals("shutting_down", readUntilId("c:stop").path("result")
                    .path("status").textValue());
            shutdown = true;
        }

        /** Reads one optional response without blocking so duplicate terminal frames are visible. */
        java.util.Optional<JsonNode> readOptionalId(String id, Duration wait) throws Exception {
            JsonNode cached = stash.remove(id);
            if (cached != null) {
                return java.util.Optional.of(cached);
            }
            long deadline = System.nanoTime() + wait.toNanos();
            while (System.nanoTime() < deadline) {
                if (reader.ready()) {
                    JsonNode value = read();
                    if (id.equals(value.path("id").textValue())) {
                        return java.util.Optional.of(value);
                    }
                    String otherId = value.path("id").textValue();
                    if (otherId != null && !otherId.isBlank()) {
                        stash.put(otherId, value);
                    }
                } else {
                    Thread.sleep(10);
                }
            }
            return java.util.Optional.empty();
        }

        /** Reads until a response id arrives while preserving other correlated server frames. */
        JsonNode readUntilId(String id) throws Exception {
            JsonNode cached = stash.remove(id);
            if (cached != null) {
                return cached;
            }
            while (true) {
                JsonNode value = read();
                if (id.equals(value.path("id").textValue())) {
                    return value;
                }
                String otherId = value.path("id").textValue();
                if (otherId != null && !otherId.isBlank()) {
                    stash.put(otherId, value);
                }
            }
        }

        /** Reads until a server request/event method arrives, preserving correlated responses. */
        JsonNode readUntilMethod(String method) throws Exception {
            while (true) {
                JsonNode value = read();
                if (method.equals(value.path("method").textValue())) {
                    return value;
                }
                String otherId = value.path("id").textValue();
                if (otherId != null && !otherId.isBlank()) {
                    stash.put(otherId, value);
                }
            }
        }

        /** Fails immediately if a real turn terminates before the next approval request arrives. */
        JsonNode readApprovalBeforeTerminal() throws Exception {
            long deadline = System.nanoTime() + Duration.ofSeconds(10).toNanos();
            while (true) {
                if (System.nanoTime() >= deadline) {
                    throw new AssertionError("timed out waiting for approval/request");
                }
                if (!reader.ready()) {
                    LockSupport.parkNanos(TimeUnit.MILLISECONDS.toNanos(1));
                    continue;
                }
                JsonNode value = read();
                String method = value.path("method").textValue();
                if ("turn/completed".equals(method)) {
                    throw new AssertionError("turn completed before approval: " + value);
                }
                if ("approval/request".equals(method)) {
                    return value;
                }
                String otherId = value.path("id").textValue();
                if (otherId != null && !otherId.isBlank()) {
                    stash.put(otherId, value);
                }
            }
        }

        /** Waits for the unique terminal while failing on a hidden second approval request. */
        JsonNode readUntilTurnCompleted(Duration timeout) throws Exception {
            long deadline = System.nanoTime() + timeout.toNanos();
            while (true) {
                if (System.nanoTime() >= deadline) {
                    throw new AssertionError("timed out waiting for turn/completed");
                }
                if (!reader.ready()) {
                    LockSupport.parkNanos(TimeUnit.MILLISECONDS.toNanos(1));
                    continue;
                }
                JsonNode value = read();
                String method = value.path("method").textValue();
                if ("approval/request".equals(method)) {
                    throw new AssertionError("second approval before terminal: " + value);
                }
                if ("turn/completed".equals(method)) {
                    return value;
                }
                String otherId = value.path("id").textValue();
                if (otherId != null && !otherId.isBlank()) {
                    stash.put(otherId, value);
                }
            }
        }

        /** Writes exactly one JSONL request through the single client writer. */
        private void send(String frame) throws Exception {
            writer.write(frame);
            writer.write('\n');
            writer.flush();
        }

        /** Reads stdout and rejects every known secret before JSON parsing. */
        private JsonNode read() throws Exception {
            String line = reader.readLine();
            assertNotNull(line, "sidecar closed stdout unexpectedly");
            assertFalse(line.isBlank());
            for (String value : forbidden) {
                assertFalse(line.contains(value), "secret leaked to stdout: " + value);
            }
            return JSON.readTree(line);
        }

        /** Closes the test client and waits for the sidecar's bounded lifecycle drain. */
        @Override
        public void close() throws Exception {
            try {
                if (!shutdown) {
                    runtime.close();
                }
                assertEquals(0, exit.get(10, TimeUnit.SECONDS));
            } finally {
                writer.close();
                clientOutput.close();
                runtime.close();
            }
        }
    }
}
