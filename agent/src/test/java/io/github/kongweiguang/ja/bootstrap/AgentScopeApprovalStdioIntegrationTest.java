// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.message.ToolResultBlock;
import io.agentscope.core.message.ToolUseBlock;
import io.agentscope.core.model.ChatResponse;
import io.agentscope.core.model.GenerateOptions;
import io.agentscope.core.model.Model;
import io.agentscope.core.model.ToolSchema;
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
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import reactor.core.publisher.Flux;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.assertTimeout;

/** Proves the real AgentScope ASK pause crosses the frozen stdio request/response boundary. */
final class AgentScopeApprovalStdioIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";

    /**
     * Exercises allow-once and deny through the same JSONL stream, while health/read proves the
     * control lane does not wait for the provider resume Flux.
     */
    @Test
    void approvalRequestResumesOrDeniesAndHealthDoesNotDeadlock(@TempDir Path temp) throws Exception {
        for (String decision : List.of("allow_once", "allow_session", "deny")) {
            runApprovalRound(temp.resolve(decision), decision);
        }
    }

    /** Runs one isolated full-duplex round so an approval id cannot leak between sidecars. */
    private static void runApprovalRound(Path temp, String decision) throws Exception {
        Path workspace = Files.createDirectories(temp.resolve("workspace"));
        Path data = Files.createDirectories(temp.resolve("data"));
        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data,
                new ApprovalModel());
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.PRODUCTION),
                Clock.systemUTC(), graph);
        CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            assertEquals("c:init", read(output).path("id").textValue());
            send(input, initializedFrame());
            assertEquals("runtime/statusChanged", read(output).path("method").textValue());

            send(input, turnStartFrame("c:turn", "thr_approval_" + decision,
                    "session_approval_" + decision));
            assertEquals("c:turn", read(output).path("id").textValue());
            JsonNode approval = readUntil(output, "approval/request", null);
            assertTrue(approval.path("id").textValue().startsWith("s:approval_"));
            assertEquals("full_access", approval.path("params").path("accessMode").textValue());
            assertEquals("shell", approval.path("params").path("action").path("kind").textValue());
            String approvalId = approval.path("id").textValue();

            // This frame is intentionally written immediately after the decision. The server must
            // acknowledge it from the control lane even while the same session resumes AgentScope.
            send(input, approvalResponse(approvalId, decision));
            send(input, healthFrame());
            boolean healthSeen = false;
            int terminals = 0;
            boolean completed = false;
            String terminalStatus = null;
            for (int index = 0; index < 64 && (!healthSeen || !completed); index++) {
                JsonNode frame = read(output);
                if ("c:health".equals(frame.path("id").textValue())) {
                    healthSeen = true;
                    assertFalse(frame.has("error"), frame.toString());
                }
                if ("turn/completed".equals(frame.path("method").textValue())) {
                    terminals++;
                    completed = true;
                    terminalStatus = frame.path("params").path("turn")
                            .path("terminalStatus").textValue();
                }
            }
            assertTrue(healthSeen, "health/read must not wait for AgentScope resume");
            assertTrue(completed, "approval decision must produce a terminal turn");
            assertEquals(1, terminals, "one turn must have exactly one terminal event");
            assertEquals("completed", terminalStatus,
                    "the decision must resume the paused AgentScope turn");

            send(input, shutdownFrame());
            assertEquals("c:stop", read(output).path("id").textValue());
        } finally {
            clientInput.close();
            clientOutput.close();
            runtime.close();
        }
        assertEquals(0, exit.get(10, TimeUnit.SECONDS));
    }

    /** Creates the minimal initialized frame accepted by the production handshake. */
    private static String initializeFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:init\",\"method\":\"initialize\",\"params\":{" +
                "\"protocolMajor\":1,\"protocolMinor\":0,\"minimumCompatibleMinor\":0," +
                "\"clientVersion\":\"approval-test\",\"capabilities\":{" +
                "\"methods\":[\"initialize\",\"turn/start\",\"health/read\",\"shutdown\"]," +
                "\"events\":[\"runtime/statusChanged\",\"turn/started\",\"item/started\",\"item/delta\",\"item/completed\",\"turn/completed\"]," +
                "\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"]," +
                "\"itemKinds\":[\"agent_message\",\"approval\"],\"mcp\":{" +
                "\"protocolVersions\":[],\"transports\":[],\"features\":[]}}," +
                "\"limits\":{" +
                "\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024," +
                "\"maxInFlightRequests\":64,\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536," +
                "\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576,\"defaultRequestDeadlineMs\":120000," +
                "\"defaultApprovalDeadlineMs\":300000}}}";
    }

    /** Creates the post-initialize notification with the fixed test challenge. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\""
                + READY_TOKEN + "\"}}";
    }

    /** Creates a full-access shell turn with a real settings profile revision. */
    private static String turnStartFrame(String id, String threadId, String sessionId)
            throws Exception {
        var params = JsonNodes.object();
        params.put("threadId", threadId);
        params.put("userId", "approval-user");
        params.put("sessionId", sessionId);
        params.put("accessMode", "full_access");
        params.put("profileRevision", "profile_approval");
        var input = JsonNodes.array();
        var part = JsonNodes.object();
        part.put("type", "text");
        part.put("text", "run the configured shell probe");
        input.add(part);
        params.set("input", input);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", "turn/start", "params", params));
    }

    /** Builds the frozen approval/request standard result; no approval/respond method is used. */
    private static String approvalResponse(String id, String decision) {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"" + id
                + "\",\"result\":{\"decision\":\"" + decision
                + "\",\"resolvedAt\":\"2026-08-17T10:00:00Z\"}}";
    }

    /** Builds an independent control-lane request sent while the turn is being resumed. */
    private static String healthFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:health\",\"method\":\"health/read\",\"params\":{}}";
    }

    /** Builds the normal graceful shutdown request. */
    private static String shutdownFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}";
    }

    /** Writes a complete JSONL frame and flushes the client side of the pipe. */
    private static void send(BufferedWriter writer, String frame) throws Exception {
        writer.write(frame);
        writer.write('\n');
        writer.flush();
    }

    /** Reads one non-empty JSON frame from the sidecar output. */
    private static JsonNode read(BufferedReader reader) throws Exception {
        // PipedInputStream records the reader thread and reports a broken pipe when each
        // preemptive timeout creates a short-lived replacement thread.  A same-thread timeout
        // keeps the full-duplex test's reader identity stable while still bounding each frame.
        return assertTimeout(Duration.ofSeconds(10), () -> {
            String line = reader.readLine();
            System.err.println("approval-test frame: " + line);
            assertNotNull(line);
            assertFalse(line.isBlank());
            return JSON.readTree(line);
        });
    }

    /** Reads until a server request/notification of the expected method arrives. */
    private static JsonNode readUntil(BufferedReader reader, String method, String id)
            throws Exception {
        for (int index = 0; index < 64; index++) {
            JsonNode candidate = read(reader);
            if (method.equals(candidate.path("method").textValue())
                    && (id == null || id.equals(candidate.path("id").textValue()))) {
                return candidate;
            }
        }
        throw new AssertionError("method did not arrive: " + method);
    }

    /** Emits one real AgentScope shell tool call and then a deterministic final response. */
    private static final class ApprovalModel implements Model {
        private final AtomicInteger calls = new AtomicInteger();

        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            boolean hasToolResult = messages.stream().flatMap(message -> message.getContent().stream())
                    .anyMatch(block -> block instanceof ToolResultBlock);
            if (!hasToolResult && calls.getAndIncrement() == 0) {
                return Flux.just(new ChatResponse("approval-shell",
                        List.of(new ToolUseBlock("approval-shell-1", "execute",
                                Map.of("command", "echo JA-APPROVAL"),
                                "{\"command\":\"echo JA-APPROVAL\"}", Map.of())),
                        null, Map.of(), "tool_calls"));
            }
            return Flux.just(new ChatResponse("approval-final",
                    List.of(TextBlock.builder().text("approval handled").build()), null,
                    Map.of(), "stop"));
        }

        /** Gives AgentScope a stable model identity for the persisted test session. */
        @Override
        public String getModelName() {
            return "ja-approval-integration-model";
        }
    }
}
