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
import io.agentscope.core.tool.ToolCallParam;
import io.github.kongweiguang.ja.mcp.McpServerDefinition;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.runtime.StdioRuntime;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import reactor.core.publisher.Flux;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.IOException;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.time.Clock;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.HexFormat;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Proves the real Harness composition is reachable through the stdio wire. */
final class AgentScopeRuntimeGraphIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";
    private static final String MODE_FIXTURE = "JA mode fixture\n";

    /**
     * Uses pipes instead of a provider process so the complete protocol,
     * Harness stream, SQLite state and clean shutdown remain deterministic.
     */
    @Test
    void realHarnessRunsThroughFullDuplexStdioAndPersistsState(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        Files.writeString(workspace.resolve("README.md"), "JA integration fixture\n",
                StandardCharsets.UTF_8);
        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data, new ToolModel());
        assertSame(graph.toolkit(), graph.mcpRuntime().toolkit(),
                "Harness and MCP must share AgentScope's Toolkit instance");
        assertTrue(graph.supportsCancellation(),
                "the production graph must expose the existing AgentScope cancel port");
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.PRODUCTION),
                Clock.systemUTC(), graph);
        CompletableFuture<Integer> result = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            send(input, initializedFrame());
            JsonNode initialized = readJson(output);
            assertEquals("c:init", initialized.path("id").textValue());
            String serverInstanceId = initialized.path("result").path("serverInstanceId").textValue();
            assertTrue(serverInstanceId.startsWith("srv_"));
            assertFalse("srv_ja_runtime".equals(serverInstanceId),
                    "the identity must be generated per sidecar, not hard-coded");
            assertTrue(initialized.path("result").path("capabilities").path("methods")
                    .toString().contains("turn/start"));
            assertTrue(graph.hasUpstreamSkills(),
                    "the same Harness must retain AgentScope skill repositories");
            assertEquals("ready", readJson(output).path("params").path("status").textValue());

            send(input, turnStartFrame());
            JsonNode accepted = readJson(output);
            assertEquals("c:turn", accepted.path("id").textValue());
            assertFalse(accepted.has("error"), accepted.toString());
            String turnId = accepted.path("result").path("turnId").textValue();
            assertTrue(turnId.startsWith("turn_"));
            boolean completed = false;
            boolean visibleDelta = false;
            boolean readObserved = false;
            boolean patchObserved = false;
            while (!completed) {
                JsonNode event = readJson(output);
                visibleDelta |= "item/delta".equals(event.path("method").textValue());
                readObserved |= event.toString().contains("read_file");
                patchObserved |= event.toString().contains("apply_patch");
                assertEquals(serverInstanceId,
                        event.path("params").path("serverInstanceId").textValue(),
                        "AgentScope events must use the handshake identity");
                completed = "turn/completed".equals(event.path("method").textValue());
                if (completed) {
                    assertEquals(turnId, event.path("params").path("turn").path("turnId").textValue());
                    assertEquals("completed", event.path("params").path("turn").path("terminalStatus").textValue(), event.toString());
                }
            }
            assertTrue(visibleDelta);
            assertTrue(readObserved, "the real Harness must call AgentScope read_file");
            assertTrue(patchObserved, "the real Harness must call the expected-hash patch tool");
            assertEquals("JA integration patched\n", Files.readString(
                    workspace.resolve("README.md"), StandardCharsets.UTF_8));

            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:stop", readJson(output).path("id").textValue());
        } finally {
            clientInput.close();
            clientOutput.close();
            runtime.close();
        }
        assertEquals(0, result.get(10, TimeUnit.SECONDS));
        assertTrue(Files.isRegularFile(data.resolve("ja.sqlite")));

        // The next owner can read the same upstream AgentScope state contract after restart.
        try (var reopened = io.github.kongweiguang.ja.persistence.SqlitePersistence.open(
                io.github.kongweiguang.ja.persistence.SqlitePersistenceConfig.of(
                        data.resolve("ja.sqlite"), data.resolve("ja.sqlite.bak")))) {
            assertTrue(reopened.agentState().exists("integration-user", "integration-session"));
            var restored = reopened.agentState().get("integration-user", "integration-session",
                    "agent_state", io.agentscope.core.state.AgentState.class).orElseThrow();
            assertEquals("integration-user", restored.getUserId());
            assertEquals("integration-session", restored.getSessionId());
            assertFalse(restored.getContext().isEmpty(), "turn context must survive reopening SQLite");
            assertFalse(reopened.agentState().listSessionIds("integration-user").isEmpty());
        }
    }

    /**
     * Proves each public access mode reaches AgentScope's permission engine instead of merely
     * changing a JA-side label: read-only rejects a patch, workspace permits it, and full access
     * still produces the upstream shell confirmation event.
     */
    @Test
    void wireAccessModesMapToAgentScopePermissions(@TempDir Path temp) throws Exception {
        Path readOnly = runPermissionProbe(temp, "trusted", "read_only", "full_access",
                ProbeKind.PATCH);
        assertEquals(MODE_FIXTURE, Files.readString(readOnly, StandardCharsets.UTF_8));

        Path workspace = runPermissionProbe(temp, "trusted", "workspace", "full_access",
                ProbeKind.PATCH);
        assertEquals("JA mode patched\n", Files.readString(workspace, StandardCharsets.UTF_8));

        Path turnWorkspace = runPermissionProbe(temp, "trusted", "full_access", "workspace",
                ProbeKind.PATCH);
        assertEquals("JA mode patched\n", Files.readString(turnWorkspace,
                StandardCharsets.UTF_8));

        Path turnReadOnly = runPermissionProbe(temp, "trusted", "full_access", "read_only",
                ProbeKind.PATCH);
        assertEquals(MODE_FIXTURE, Files.readString(turnReadOnly, StandardCharsets.UTF_8));

        Path untrusted = runPermissionProbe(temp, "untrusted", "full_access", "full_access",
                ProbeKind.PATCH);
        assertEquals(MODE_FIXTURE, Files.readString(untrusted, StandardCharsets.UTF_8));

        ProbeOutcome profileReadOnlyShell = runShellPermissionProbe(temp, "trusted", "read_only",
                "full_access");
        assertFalse(profileReadOnlyShell.approvalObserved(),
                "read_only profile must not allow a turn to request shell approval");
        assertTrue(profileReadOnlyShell.events().stream().anyMatch(event ->
                "turn/completed".equals(event.method())));

        ProbeOutcome untrustedShell = runShellPermissionProbe(temp, "untrusted", "full_access",
                "full_access");
        assertFalse(untrustedShell.approvalObserved(),
                "untrusted workspace must not allow a turn to request shell approval");
        assertTrue(untrustedShell.events().stream().anyMatch(event ->
                "turn/completed".equals(event.method())));

        ProbeOutcome fullAccess = runShellPermissionProbe(temp, "trusted", "full_access",
                "full_access");
        assertTrue(fullAccess.approvalObserved(), "full_access must retain AgentScope shell ASK");
        assertTrue(fullAccess.events().stream().anyMatch(event ->
                "turn/completed".equals(event.method())));
    }

    /** Prevents a real Harness turn from running against an omitted settings snapshot. */
    @Test
    void realGraphRequiresAccessModeAndProfileRevision(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace-required"));
        Path data = Files.createDirectory(temp.resolve("data-required"));
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data, new ToolModel());
        try {
            var missingProfileParams = permissionRequest("workspace", "thr-required-profile",
                    "session-required-profile").params();
            missingProfileParams.remove("profileRevision");
            RpcRequest missingProfile = RpcRequest.client("c:missing-profile", "turn/start",
                    missingProfileParams);
            assertThrows(io.github.kongweiguang.ja.protocol.ProtocolException.class,
                    () -> graph.start(missingProfile, ignored -> { }));

            var missingModeParams = permissionRequest("workspace", "thr-required-mode",
                    "session-required-mode").params();
            missingModeParams.remove("accessMode");
            RpcRequest missingMode = RpcRequest.client("c:missing-mode", "turn/start",
                    missingModeParams);
            assertThrows(io.github.kongweiguang.ja.protocol.ProtocolException.class,
                    () -> graph.start(missingMode, ignored -> { }));
        } finally {
            graph.close();
        }
    }

    /** Exact activation identity rejects a stale wire revision before AgentScope admission. */
    @Test
    void graphBindsServerIdentityAndWireProfileRevision(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace-bound"));
        Path data = Files.createDirectory(temp.resolve("data-bound"));
        ServerInstanceId server = new ServerInstanceId("srv_bound_graph");
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data, server,
                "profile_bound", new ToolModel());
        try {
            assertSame(server, graph.serverInstanceId());
            assertEquals("profile_bound", graph.profileRevision());
            assertThrows(io.github.kongweiguang.ja.protocol.ProtocolException.class,
                    () -> graph.start(permissionRequest("workspace", "thr-bound", "session-bound",
                            "profile_stale"), ignored -> { }));
        } finally {
            graph.close();
        }
    }

    /** Connects a real stdio MCP fixture before Harness creation and calls its aliased tool. */
    @Test
    void graphActivatesMcpOnSharedToolkitAndReapsChild(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace-mcp"));
        Path data = Files.createDirectory(temp.resolve("data-mcp"));
        Path pidFile = temp.resolve("mcp.pid");
        McpServerDefinition definition = new McpServerDefinition(
                "mcp_graph_fixture", "graph fixture", "stdio", javaExecutable(),
                "2024-11-05", List.of("-cp", absoluteClasspath(),
                        "io.github.kongweiguang.ja.mcp.McpRuntimeTest$StdioFixture",
                        pidFile.toString()), Map.of(), Map.of(), Map.of(),
                McpServerDefinition.Auth.none(), true);
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data,
                new ServerInstanceId("srv_mcp_graph"), "profile_mcp_graph", "trusted",
                "full_access", new ToolModel(), List.of(),
                List.of(new AgentScopeRuntimeGraph.McpActivation(definition, null)));
        long pid;
        try {
            String alias = graph.toolkit().getToolNames().stream()
                    .filter(name -> name.startsWith("ja_mcp_graph_fixture_echo_"))
                    .findFirst().orElseThrow();
            ToolResultBlock result = graph.toolkit().callTool(ToolCallParam.builder()
                    .toolUseBlock(ToolUseBlock.builder().id("mcp-call").name(alias)
                            .input(Map.of()).content("{}").build())
                    .input(Map.of()).build()).block(Duration.ofSeconds(3));
            assertNotNull(result);
            assertFalse(result.isSuspended());
            assertTrue(Files.exists(pidFile));
            pid = Long.parseLong(Files.readString(pidFile));
        } finally {
            graph.close();
        }
        assertTrue(awaitProcessExit(pid), "MCP stdio child remained after graph close");
    }

    /**
     * Rejects a profile/tools.json server-name overlap before profile transport startup, proving
     * the atomic guard prevents either source from creating a conflicting child process.
     */
    @Test
    void graphRejectsProfileToolsConfigNameConflictBeforeStartingChild(@TempDir Path temp)
            throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace-mcp-conflict"));
        Path data = Files.createDirectory(temp.resolve("data-mcp-conflict"));
        Path pidFile = temp.resolve("mcp-conflict.pid");
        Files.writeString(workspace.resolve("tools.json"),
                "{\"mcpServers\":{\"mcp_graph_conflict\":{}}}", StandardCharsets.UTF_8);
        McpServerDefinition definition = new McpServerDefinition(
                "mcp_graph_conflict", "graph conflict", "stdio", javaExecutable(),
                "2024-11-05", List.of("-cp", absoluteClasspath(),
                        "io.github.kongweiguang.ja.mcp.McpRuntimeTest$StdioFixture",
                        pidFile.toString()), Map.of(), Map.of(), Map.of(),
                McpServerDefinition.Auth.none(), true);

        IllegalStateException failure = assertThrows(IllegalStateException.class,
                () -> AgentScopeRuntimeGraph.open(workspace, data,
                        new ServerInstanceId("srv_mcp_conflict"), "profile_mcp_conflict",
                        "trusted", "full_access", new ToolModel(), List.of(),
                        List.of(new AgentScopeRuntimeGraph.McpActivation(definition, null))));
        assertEquals("mcp_server_unavailable", failure.getMessage());
        assertFalse(Files.exists(pidFile), "conflicting profile MCP must not start a child");
    }

    /**
     * Proves distinct profile and tools.json servers both use AgentScope registration and are
     * reaped by the normal graph close path, including the JaSandboxFilesystem config read.
     */
    @Test
    void graphRunsDistinctProfileAndToolsConfigServersAndReapsBoth(@TempDir Path temp)
            throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace-mcp-distinct"));
        Path data = Files.createDirectory(temp.resolve("data-mcp-distinct"));
        Path profilePid = temp.resolve("mcp-profile.pid");
        Path toolsPid = temp.resolve("mcp-tools.pid");
        String java = javaExecutable();
        String classpath = absoluteClasspath();
        Files.writeString(workspace.resolve("tools.json"), JSON.writeValueAsString(Map.of(
                "mcpServers", Map.of("mcp_tools_fixture", Map.of(
                        "transport", "stdio",
                        "command", java,
                        "args", List.of("-cp", classpath,
                                "io.github.kongweiguang.ja.mcp.McpRuntimeTest$StdioFixture",
                                toolsPid.toString(), "tools-config"))))),
                StandardCharsets.UTF_8);
        McpServerDefinition definition = new McpServerDefinition(
                "mcp_profile_fixture", "profile fixture", "stdio", java,
                "2024-11-05", List.of("-cp", classpath,
                        "io.github.kongweiguang.ja.mcp.McpRuntimeTest$StdioFixture",
                        profilePid.toString(), "profile"), Map.of(), Map.of(), Map.of(),
                McpServerDefinition.Auth.none(), true);

        long profileProcess;
        long toolsProcess;
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data,
                new ServerInstanceId("srv_mcp_distinct"), "profile_mcp_distinct", "trusted",
                "full_access", new ToolModel(), List.of(),
                List.of(new AgentScopeRuntimeGraph.McpActivation(definition, null)));
        try {
            assertTrue(awaitFile(profilePid), "profile MCP did not start");
            assertTrue(awaitFile(toolsPid), "tools.json MCP was not loaded through the sandbox");
            profileProcess = Long.parseLong(Files.readString(profilePid));
            toolsProcess = Long.parseLong(Files.readString(toolsPid));
            assertTrue(ProcessHandle.of(profileProcess).map(ProcessHandle::isAlive).orElse(false),
                    "profile MCP was not retained by AgentScope");
            assertTrue(ProcessHandle.of(toolsProcess).map(ProcessHandle::isAlive).orElse(false),
                    "tools.json MCP was not retained by AgentScope");
        } finally {
            graph.close();
        }
        assertTrue(awaitProcessExit(profileProcess), "profile MCP child remained after graph close");
        assertTrue(awaitProcessExit(toolsProcess), "tools.json MCP child remained after graph close");
    }

    /** Runs a bounded patch turn and returns the fixture for a mode-specific file assertion. */
    private static Path runPermissionProbe(Path temp, String accessMode, ProbeKind kind)
            throws Exception {
        return runPermissionProbe(temp, "trusted", "full_access", accessMode, kind);
    }

    /** Runs a patch turn through the strict trust/profile/request access intersection. */
    private static Path runPermissionProbe(Path temp, String trust, String profileAccessMode,
                                           String requestedAccessMode, ProbeKind kind)
            throws Exception {
        String suffix = trust + "-" + profileAccessMode + "-" + requestedAccessMode;
        Path workspace = Files.createDirectory(temp.resolve("workspace-" + suffix));
        Path data = Files.createDirectory(temp.resolve("data-" + suffix));
        Path fixture = workspace.resolve("README.md");
        Files.writeString(fixture, MODE_FIXTURE, StandardCharsets.UTF_8);
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data,
                new ServerInstanceId("srv_" + suffix.replace('-', '_')),
                "profile_permission_probe", trust, profileAccessMode,
                new PermissionProbeModel(kind, sha256(Files.readAllBytes(fixture))));
        List<TurnEvent> events = new CopyOnWriteArrayList<>();
        CountDownLatch completed = new CountDownLatch(1);
        try {
            graph.start(permissionRequest(requestedAccessMode, "thr_" + suffix,
                            "session_" + suffix, "profile_permission_probe"),
                    event -> {
                        events.add(event);
                        if ("turn/completed".equals(event.method())) {
                            completed.countDown();
                        }
                    });
            assertTrue(completed.await(10, TimeUnit.SECONDS),
                    () -> "permission probe did not complete: " + events);
            assertTrue(graph.awaitQuiescence(Duration.ofSeconds(5)));
        } finally {
            graph.close();
        }
        return fixture;
    }

    /** Runs a shell turn until AgentScope emits its real human-confirmation event. */
    private static ProbeOutcome runShellPermissionProbe(Path temp) throws Exception {
        return runShellPermissionProbe(temp, "trusted", "full_access", "full_access");
    }

    /** Runs a shell turn under one trust/profile/request intersection. */
    private static ProbeOutcome runShellPermissionProbe(Path temp, String trust,
                                                        String profileAccessMode,
                                                        String requestedAccessMode)
            throws Exception {
        String suffix = "shell-" + trust + "-" + profileAccessMode + "-" + requestedAccessMode;
        Path workspace = Files.createDirectory(temp.resolve("workspace-" + suffix));
        Path data = Files.createDirectory(temp.resolve("data-" + suffix));
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data,
                new ServerInstanceId("srv_" + suffix.replace('-', '_')),
                "profile_permission_probe", trust, profileAccessMode,
                new PermissionProbeModel(ProbeKind.SHELL, null));
        List<TurnEvent> events = new CopyOnWriteArrayList<>();
        CountDownLatch approval = new CountDownLatch(1);
        CountDownLatch completed = new CountDownLatch(1);
        try {
            graph.start(permissionRequest(requestedAccessMode, "thr_" + suffix,
                            "session_" + suffix, "profile_permission_probe"),
                    event -> {
                        events.add(event);
                        if ("item/started".equals(event.method())
                                && "approval".equals(event.params().path("item")
                                .path("kind").textValue())
                                && event.params().path("item").path("metadata")
                                .path("requiresUserAction").asBoolean(false)) {
                            approval.countDown();
                        }
                        if ("turn/completed".equals(event.method())) {
                            completed.countDown();
                        }
                    });
            if ("trusted".equals(trust)
                    && "full_access".equals(profileAccessMode)
                    && "full_access".equals(requestedAccessMode)) {
                assertTrue(approval.await(10, TimeUnit.SECONDS),
                        () -> "shell confirmation was not emitted: " + events);
            } else {
                assertTrue(completed.await(10, TimeUnit.SECONDS),
                        () -> "restricted shell turn did not complete: " + events);
            }
        } finally {
            // No approval is supplied in this deterministic probe; close exercises the same
            // cancellation path used when a user leaves a pending shell request unresolved.
            graph.close();
        }
        return new ProbeOutcome(List.copyOf(events), approval.getCount() == 0);
    }

    /** Builds a direct runtime request with the same required fields as the stdio contract. */
    private static RpcRequest permissionRequest(String accessMode, String threadId,
                                                String sessionId) {
        return permissionRequest(accessMode, threadId, sessionId, "profile_permission_probe");
    }

    /** Builds a request with an explicit wire revision for stale-graph checks. */
    private static RpcRequest permissionRequest(String accessMode, String threadId,
                                                String sessionId, String profileRevision) {
        var params = JsonNodes.object();
        params.put("threadId", threadId);
        params.put("userId", "integration-user");
        params.put("sessionId", sessionId);
        params.put("accessMode", accessMode);
        params.put("profileRevision", profileRevision);
        var input = JsonNodes.array();
        var part = JsonNodes.object();
        part.put("type", "text");
        part.put("text", "exercise the configured permission mode");
        input.add(part);
        params.set("input", input);
        return new RpcRequest("c:permission_" + accessMode, "turn/start", params,
                RpcDirection.CLIENT_TO_SERVER);
    }

    /** Computes the expected-hash value used by the JA apply_patch adapter. */
    private static String sha256(byte[] content) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(content));
    }

    /** Resolves the Java executable used by the isolated stdio fixture child. */
    private static String javaExecutable() {
        return Path.of(System.getProperty("java.home"), "bin",
                System.getProperty("os.name", "").toLowerCase().contains("win")
                        ? "java.exe" : "java").toString();
    }

    /** Keeps the fixture command short enough for the MCP settings bound. */
    private static String absoluteClasspath() {
        Path base = Path.of(System.getProperty("user.dir", "."))
                .toAbsolutePath().normalize();
        String separator = System.getProperty("path.separator");
        return base.resolve("target/test-classes") + separator + base.resolve("target/classes");
    }

    /** Confirms official MCP transport cleanup without inventing a process manager. */
    private static boolean awaitProcessExit(long pid) throws InterruptedException {
        for (int attempt = 0; attempt < 100; attempt++) {
            if (!ProcessHandle.of(pid).map(ProcessHandle::isAlive).orElse(false)) {
                return true;
            }
            Thread.sleep(20);
        }
        return false;
    }

    /** Waits for an MCP fixture to publish its PID before assertions inspect child lifecycle. */
    private static boolean awaitFile(Path path) throws IOException, InterruptedException {
        for (int attempt = 0; attempt < 150; attempt++) {
            if (Files.isRegularFile(path) && Files.size(path) > 0) {
                return true;
            }
            Thread.sleep(20);
        }
        return false;
    }

    /** Builds the protocol envelope accepted by the production handshake. */
    private static String initializeFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:init\",\"method\":\"initialize\",\"params\":{" +
                "\"protocolMajor\":1,\"protocolMinor\":0,\"minimumCompatibleMinor\":0," +
                "\"clientVersion\":\"integration-test\",\"capabilities\":{" +
                "\"methods\":[\"initialize\",\"turn/start\",\"shutdown\"]," +
                "\"events\":[\"runtime/statusChanged\",\"turn/started\",\"item/started\",\"item/delta\",\"item/completed\",\"turn/completed\"]," +
                "\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"]," +
                "\"itemKinds\":[\"agent_message\"],\"mcp\":{\"protocolVersions\":[],\"transports\":[],\"features\":[]}}," +
                "\"limits\":{\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024," +
                "\"maxInFlightRequests\":64,\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536," +
                "\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576,\"defaultRequestDeadlineMs\":120000," +
                "\"defaultApprovalDeadlineMs\":300000}}}";
    }

    /** Builds the post-initialize notification with the fixed handshake token. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{" +
                "\"readyToken\":\"" + READY_TOKEN + "\"}}";
    }

    /** Builds one coding turn whose state is associated with a stable user/session. */
    private static String turnStartFrame() throws Exception {
        var params = JsonNodes.object();
        params.put("threadId", "thr_integration");
        params.put("userId", "integration-user");
        params.put("sessionId", "integration-session");
        params.put("accessMode", "workspace");
        params.put("profileRevision", "profile_integration");
        var input = JsonNodes.array();
        var part = JsonNodes.object();
        part.put("type", "text");
        part.put("text", "inspect this workspace and report the result");
        input.add(part);
        params.set("input", input);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:turn",
                "method", "turn/start", "params", params));
    }

    /** Writes a complete JSONL frame and flushes to unblock the sidecar reader. */
    private static void send(BufferedWriter writer, String frame) throws Exception {
        writer.write(frame);
        writer.write('\n');
        writer.flush();
    }

    /** Parses one line so stdout purity is checked at the same boundary as Tauri. */
    private static JsonNode readJson(BufferedReader reader) throws Exception {
        String line = reader.readLine();
        assertNotNull(line);
        assertFalse(line.isBlank());
        return JSON.readTree(line);
    }

    /** Emits one real upstream read_file call and then a deterministic final response. */
    private static final class ToolModel implements Model {
        private final AtomicInteger calls = new AtomicInteger();

        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            boolean hasToolResult = messages.stream()
                    .flatMap(message -> message.getContent().stream())
                    .anyMatch(block -> block instanceof ToolResultBlock);
            if (!hasToolResult && calls.getAndIncrement() == 0) {
                return Flux.just(new ChatResponse("integration-tool-call",
                        List.of(new ToolUseBlock("read-1", "read_file",
                                Map.of("path", "README.md", "offset", 0, "limit", 0),
                                "{\"path\":\"README.md\",\"offset\":0,\"limit\":0}", Map.of())), null,
                        Map.of(), "tool_calls"));
            }
            if (hasToolResult && calls.getAndIncrement() == 1) {
                return Flux.just(new ChatResponse("integration-patch-call",
                        List.of(new ToolUseBlock("patch-1", "apply_patch", Map.of(
                                "path", "README.md",
                                "expected_sha256", "df3231ffc68591ffe775e94e178a8ca1f7cefa52fc4e1f5876ab74dffa34989c",
                                "old_string", "JA integration fixture",
                                "new_string", "JA integration patched",
                                "replace_all", false),
                                "{\"path\":\"README.md\",\"expected_sha256\":\"df3231ffc68591ffe775e94e178a8ca1f7cefa52fc4e1f5876ab74dffa34989c\",\"old_string\":\"JA integration fixture\",\"new_string\":\"JA integration patched\",\"replace_all\":false}", Map.of())), null, Map.of(), "tool_calls"));
            }
            return Flux.just(new ChatResponse("integration-response",
                    List.of(TextBlock.builder().text("workspace inspected").build()), null,
                    Map.of(), "stop"));
        }

        /** Gives AgentScope a stable model name for state and diagnostics. */
        @Override
        public String getModelName() {
            return "ja-integration-model";
        }
    }

    /** Selects one real Harness tool call for a permission behavior probe. */
    private enum ProbeKind {
        PATCH,
        SHELL
    }

    /** Keeps the shell assertion independent from the event list implementation. */
    private record ProbeOutcome(List<TurnEvent> events, boolean approvalObserved) {
    }

    /**
     * Emits one deterministic tool request and then a final text response.  The actual decision
     * remains in AgentScope's PermissionEngine, so this model cannot make a denied tool run.
     */
    private static final class PermissionProbeModel implements Model {
        private final ProbeKind kind;
        private final String expectedHash;
        private final AtomicInteger calls = new AtomicInteger();

        private PermissionProbeModel(ProbeKind kind, String expectedHash) {
            this.kind = kind;
            this.expectedHash = expectedHash;
        }

        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            if (calls.getAndIncrement() == 0) {
                if (kind == ProbeKind.PATCH) {
                    Map<String, Object> input = Map.of(
                            "path", "README.md",
                            "expected_sha256", expectedHash,
                            "old_string", "JA mode fixture",
                            "new_string", "JA mode patched",
                            "replace_all", false);
                    return Flux.just(new ChatResponse("mode-patch", List.of(new ToolUseBlock(
                            "mode-patch-1", "apply_patch", input,
                            "{\"path\":\"README.md\",\"expected_sha256\":\""
                                    + expectedHash + "\",\"old_string\":\"JA mode fixture\","
                                    + "\"new_string\":\"JA mode patched\",\"replace_all\":false}",
                            Map.of())), null, Map.of(), "tool_calls"));
                }
                Map<String, Object> input = Map.of("command", "echo JA-FULL-SHELL");
                return Flux.just(new ChatResponse("mode-shell", List.of(new ToolUseBlock(
                        "mode-shell-1", "execute", input,
                        "{\"command\":\"echo JA-FULL-SHELL\"}", Map.of())), null,
                        Map.of(), "tool_calls"));
            }
            return Flux.just(new ChatResponse("mode-final",
                    List.of(TextBlock.builder().text("permission probe finished").build()), null,
                    Map.of(), "stop"));
        }

        /** Gives the upstream Harness a stable model identity for state restoration. */
        @Override
        public String getModelName() {
            return "ja-permission-probe-" + kind.name().toLowerCase();
        }
    }
}
