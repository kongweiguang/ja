// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import io.github.kongweiguang.ja.bootstrap.SidecarConfiguration;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.File;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.OutputStreamWriter;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.Arrays;
import java.util.stream.Collectors;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Verifies the B1b MCP probe/read projection over the real bounded JSONL runtime. */
@Timeout(20)
final class StdioMcpProbeIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";

    /**
     * Exercises a real Streamable HTTP initialize/tools-list probe while a slow
     * provider is active; the control lane must still reject a second probe,
     * answer health, and expose only the successful immutable projection.
     */
    @Test
    void probeProjectsToolsAndKeepsControlResponsive(@TempDir Path temp) throws Exception {
        try (HttpFixture fixture = HttpFixture.start(true); Session session = Session.open(temp)) {
            JsonNode initialized = session.initialize();
            assertTrue(contains(initialized.path("result").path("capabilities").path("methods"),
                    "mcp/test"));
            assertTrue(contains(initialized.path("result").path("capabilities").path("methods"),
                    "mcp/tools/read"));
            assertEquals(3, initialized.path("result").path("capabilities").path("mcp")
                    .path("protocolVersions").size());
            assertEquals(2, initialized.path("result").path("capabilities").path("mcp")
                    .path("transports").size());
            assertTrue(contains(initialized.path("result").path("capabilities").path("mcp")
                    .path("features"), "tools_list"));
            assertFalse(contains(initialized.path("result").path("capabilities").path("mcp")
                    .path("features"), "tools_call"));
            session.workspace();
            session.send(request("c:save", "mcp/save", serverParams(
                    "mcp_probe", fixture.url(), false, true)));
            assertFalse(session.read().has("error"));

            session.send(request("c:test-1", "mcp/test", idParams("mcp_probe")));
            session.send(request("c:test-2", "mcp/test", idParams("mcp_probe")));
            JsonNode second = session.readUntilId("c:test-2");
            assertEquals("CONFLICT", second.path("error").path("data").path("jaCode").textValue());

            session.send(request("c:health", "health/read", JsonNodes.object()));
            assertEquals("healthy", session.readUntilId("c:health").path("result").path("status")
                    .textValue());

            JsonNode first = session.readUntilId("c:test-1");
            assertEquals("healthy", first.path("result").path("status").textValue());
            assertEquals("2024-11-05", first.path("result").path("protocolVersion").textValue());
            assertEquals(1, first.path("result").path("toolCount").intValue());

            session.send(request("c:tools", "mcp/tools/read", idParams("mcp_probe")));
            JsonNode tools = session.readUntilId("c:tools");
            JsonNode tool = tools.path("result").path("tools").get(0);
            assertEquals("mcp:mcp_probe/echo", tool.path("name").textValue());
            assertEquals("ask", tool.path("policy").textValue());
            assertTrue(tool.path("inputSchema").isObject());
            assertEquals("string", tool.path("inputSchema").path("properties").path("value")
                    .path("type").textValue());
            assertTrue(tool.path("inputSchema").path("required").toString().contains("value"));
            assertFalse(tool.has("title"));
            assertFalse(tool.has("_meta"));
            assertFalse(tool.has("outputSchema"));

            session.send(request("c:list", "mcp/list", JsonNodes.object()));
            JsonNode listed = session.readUntilId("c:list");
            JsonNode summary = listed.path("result").path("servers").get(0);
            assertEquals("healthy", summary.path("status").textValue());
            assertEquals(1, summary.path("toolCount").intValue());

            fixture.duplicateTools();
            session.send(request("c:retry-fails", "mcp/test", idParams("mcp_probe")));
            JsonNode failedRetry = session.readUntilId("c:retry-fails");
            assertEquals("MCP_SERVER_UNAVAILABLE", failedRetry.path("error").path("data")
                    .path("jaCode").textValue());
            session.send(request("c:stale-tools", "mcp/tools/read", idParams("mcp_probe")));
            assertEquals("MCP_SERVER_UNAVAILABLE", session.readUntilId("c:stale-tools")
                    .path("error").path("data").path("jaCode").textValue());

            session.send(request("c:disabled-save", "mcp/save",
                    serverParams("mcp_disabled", fixture.url(), false, false)));
            assertFalse(session.read().has("error"));
            session.send(request("c:disabled-test", "mcp/test", idParams("mcp_disabled")));
            assertEquals("MCP_SERVER_UNAVAILABLE", session.readUntilId("c:disabled-test")
                    .path("error").path("data").path("jaCode").textValue());

            session.send(request("c:stop", "shutdown", JsonNodes.object()));
            assertEquals("shutting_down", session.readUntilId("c:stop").path("result").path("status")
                    .textValue());
            assertEquals(0, fixture.toolCallCount());
        }
    }

    /**
     * Verifies the purpose-bound MCP secret request and the confused-deputy
     * guard: a stale or wrong profile never reaches the provider.
     */
    @Test
    void credentialProbeUsesPurposeBoundSecret(@TempDir Path temp) throws Exception {
        try (HttpFixture fixture = HttpFixture.start(false); Session session = Session.open(temp)) {
            session.initialize();
            session.workspace();
            session.send(request("c:save", "mcp/save", serverParams(
                    "mcp_secret", fixture.url(), true, true)));
            assertFalse(session.read().has("error"));
            session.send(request("c:profile", "profile/save", profileParams("mcp_secret")));
            assertFalse(session.read().has("error"));

            session.send(request("c:missing-profile", "mcp/test", idParams("mcp_secret")));
            JsonNode missingProfile = session.readUntilId("c:missing-profile");
            assertEquals("INVALID_PARAMS", missingProfile.path("error").path("data")
                    .path("jaCode").textValue());
            assertEquals(0, fixture.requestCount());
            session.assertNoStashedFrames();

            session.send(request("c:test-secret", "mcp/test",
                    mcpTestParams("mcp_secret", "profile_mcp_probe")));
            JsonNode secretRequest = session.readUntilMethod("secret/resolve");
            assertEquals("cred_mcp_probe", secretRequest.path("params").path("credentialRef")
                    .textValue());
            assertEquals("mcp", secretRequest.path("params").path("purpose").textValue());
            assertEquals("profile_mcp_probe", secretRequest.path("params").path("profileRevision")
                    .textValue());
            assertEquals("mcp_secret", secretRequest.path("params").path("mcpRevision").textValue());
            assertFalse(secretRequest.toString().contains("fixture-secret"));
            session.send(response(secretRequest.path("id").textValue(),
                    JsonNodes.object().put("secretValue", "fixture-secret")));
            assertEquals("healthy", session.readUntilId("c:test-secret").path("result").path("status")
                    .textValue());
            assertEquals("Bearer fixture-secret", fixture.authorization.get());

            session.send(request("c:wrong-profile", "mcp/test", mcpTestParams(
                    "mcp_secret", "profile_wrong")));
            JsonNode wrongProfile = session.readUntilId("c:wrong-profile");
            assertEquals("CONFLICT", wrongProfile.path("error").path("data").path("jaCode")
                    .textValue());

            session.send(request("c:unknown", "mcp/tools/read", idParams("mcp_unknown")));
            assertEquals("MCP_SERVER_UNAVAILABLE", session.readUntilId("c:unknown").path("error")
                    .path("data").path("jaCode").textValue());

            session.send(request("c:stop", "shutdown", JsonNodes.object()));
            assertEquals("shutting_down", session.readUntilId("c:stop").path("result").path("status")
                    .textValue());
            assertEquals(0, fixture.toolCallCount());
        }
    }

    /**
     * Ensures the five-second secret deadline is a single terminal response,
     * ignores its late response, and permits a new generation probe to retry.
     */
    @Test
    void secretDeadlineIgnoresLateResponseAndAllowsRetry(@TempDir Path temp) throws Exception {
        try (HttpFixture fixture = HttpFixture.start(false); Session session = Session.open(temp)) {
            session.initialize();
            session.workspace();
            session.send(request("c:save", "mcp/save", serverParams(
                    "mcp_secret_retry", fixture.url(), true, true)));
            assertFalse(session.read().has("error"));
            session.send(request("c:profile", "profile/save", profileParams("mcp_secret_retry")));
            assertFalse(session.read().has("error"));

            session.send(request("c:deadline", "mcp/test",
                    mcpTestParams("mcp_secret_retry", "profile_mcp_probe")));
            JsonNode firstSecret = session.readUntilMethod("secret/resolve");
            Thread.sleep(5_300);
            assertEquals("MCP_SERVER_UNAVAILABLE", session.readUntilId("c:deadline").path("error")
                    .path("data").path("jaCode").textValue());
            session.send(response(firstSecret.path("id").textValue(),
                    JsonNodes.object().put("secretValue", "late-secret")));

            session.send(request("c:retry", "mcp/test",
                    mcpTestParams("mcp_secret_retry", "profile_mcp_probe")));
            JsonNode secondSecret = session.readUntilMethod("secret/resolve");
            assertFalse(firstSecret.path("id").textValue().equals(secondSecret.path("id").textValue()));
            session.send(response(secondSecret.path("id").textValue(),
                    JsonNodes.object().put("secretValue", "retry-secret")));
            assertEquals("healthy", session.readUntilId("c:retry").path("result").path("status")
                    .textValue());

            session.send(request("c:stop", "shutdown", JsonNodes.object()));
            assertEquals("shutting_down", session.readUntilId("c:stop").path("result").path("status")
                    .textValue());
        }
    }

    /** Proves the official MCP stdio client starts/closes a real child and maps an exit failure. */
    @Test
    void stdioProbeClosesChildAndMapsFailure(@TempDir Path temp) throws Exception {
        try (Session session = Session.open(temp)) {
            session.initialize();
            session.workspace();
            Path pidFile = temp.resolve("mcp-child.pid");
            String java = Path.of(System.getProperty("java.home"), "bin", javaExecutableName())
                    .toAbsolutePath().toString();
            String classPath = absoluteClasspath();
            ObjectNode server = JsonNodes.object();
            server.put("mcpRevision", "mcp_stdio_child");
            server.put("name", "stdio child");
            server.put("transport", "stdio");
            server.put("endpoint", java);
            server.put("protocolVersion", "2024-11-05");
            server.set("args", JsonNodes.array().add("-cp").add(classPath)
                    .add("io.github.kongweiguang.ja.mcp.McpRuntimeTest$StdioFixture")
                    .add(pidFile.toString()));
            server.put("enabled", true);
            session.send(request("c:save", "mcp/save", JsonNodes.object().set("server", server)));
            JsonNode saved = session.read();
            assertFalse(saved.has("error"), saved + " server=" + server);
            session.send(request("c:test", "mcp/test", idParams("mcp_stdio_child")));
            assertEquals("healthy", session.readUntilId("c:test").path("result").path("status")
                    .textValue());
            long pid = awaitPid(pidFile);
            assertTrue(awaitExit(pid));

            Path failedPidFile = temp.resolve("mcp-child-failed.pid");
            ObjectNode failed = server.deepCopy().put("mcpRevision", "mcp_stdio_failed")
                    .put("protocolVersion", "2025-06-18");
            // Keep the same real fixture so the second process reaches initialize and
            // fails on the negotiated protocol quickly instead of waiting for a
            // missing-main-class process to hit the SDK initialization timeout.
            failed.withArray("args").set(3, failedPidFile.toString());
            session.send(request("c:save-failed", "mcp/save", JsonNodes.object().set("server", failed)));
            assertFalse(session.read().has("error"));
            session.send(request("c:test-failed", "mcp/test", idParams("mcp_stdio_failed")));
            assertEquals("MCP_SERVER_UNAVAILABLE", session.readUntilId("c:test-failed")
                    .path("error").path("data").path("jaCode").textValue());
            long failedPid = awaitPid(failedPidFile);
            assertTrue(awaitExit(failedPid));

            session.send(request("c:stop", "shutdown", JsonNodes.object()));
            assertEquals("shutting_down", session.readUntilId("c:stop").path("result").path("status")
                    .textValue());
        }
    }

    /** Builds an MCP definition with the same frozen fields used by the desktop settings form. */
    private static ObjectNode serverParams(String revision, String endpoint,
                                           boolean secret, boolean enabled) {
        ObjectNode server = JsonNodes.object();
        server.put("mcpRevision", revision);
        server.put("name", "probe fixture");
        server.put("transport", "streamable_http");
        server.put("endpoint", endpoint);
        server.put("protocolVersion", "2024-11-05");
        server.put("enabled", enabled);
        if (secret) {
            server.set("auth", JsonNodes.object().put("kind", "bearer")
                    .put("credentialRef", "cred_mcp_probe"));
        }
        return JsonNodes.object().set("server", server);
    }

    /** Builds a profile association that authorizes only the selected MCP revision. */
    private static ObjectNode profileParams(String mcpRevision) {
        return profileParamsForTest(mcpRevision, "profile_mcp_probe");
    }

    /** Builds a profile save payload with an explicit revision for confused-deputy tests. */
    private static ObjectNode profileParamsForTest(String mcpRevision, String profileRevision) {
        ObjectNode model = JsonNodes.object();
        model.put("provider", "openai");
        model.put("protocol", "openai_chat_completions");
        model.put("model", "fixture-model");
        ObjectNode profile = JsonNodes.object();
        profile.put("profileRevision", profileRevision);
        profile.put("name", "MCP probe fixture");
        profile.put("accessMode", "workspace");
        profile.set("model", model);
        profile.set("mcpRevisions", JsonNodes.array().add(mcpRevision));
        return JsonNodes.object().set("profile", profile);
    }

    /** Provides the shared MCP identifier parameter shape. */
    private static ObjectNode idParams(String revision) {
        return JsonNodes.object().put("mcpRevision", revision);
    }

    /** Adds the optional profile context directly to an MCP test request. */
    private static ObjectNode mcpTestParams(String revision, String profileRevision) {
        return idParams(revision).put("profileRevision", profileRevision);
    }

    /** Uses the executable suffix of the test host so the child path works on Windows and macOS. */
    private static String javaExecutableName() {
        return System.getProperty("os.name", "").toLowerCase().contains("win")
                ? "java.exe" : "java";
    }

    /** Converts every Maven classpath entry to an absolute child-safe path after cwd restriction. */
    private static String absoluteClasspath() {
        Path base = Path.of(System.getProperty("user.dir", ".")).toAbsolutePath().normalize();
        // The MCP settings schema bounds each argument at 4096 code units.  The
        // Maven test classpath is larger than that on a full AgentScope build, so
        // the JDK-only fixture needs only the two project output roots rather than
        // copying every provider dependency into the child command line.
        return Arrays.stream(new String[]{
                        base.resolve("target/test-classes").toString(),
                        base.resolve("target/classes").toString()})
                .map(Path::of)
                .map(path -> path.toAbsolutePath().normalize().toString())
                .collect(Collectors.joining(File.pathSeparator));
    }

    /** Checks a capability array without coupling the wire test to ordering. */
    private static boolean contains(JsonNode values, String expected) {
        for (JsonNode value : values) {
            if (expected.equals(value.textValue())) {
                return true;
            }
        }
        return false;
    }

    /** Waits for the child fixture to publish its pid without unbounded test polling. */
    private static long awaitPid(Path pidFile) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (System.nanoTime() < deadline) {
            if (Files.exists(pidFile)) {
                return Long.parseLong(Files.readString(pidFile, StandardCharsets.UTF_8));
            }
            Thread.sleep(20);
        }
        throw new AssertionError("stdio fixture did not publish pid");
    }

    /** Waits for the MCP child to exit after the official wrapper closes its transport. */
    private static boolean awaitExit(long pid) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (System.nanoTime() < deadline) {
            if (ProcessHandle.of(pid).isEmpty() || !ProcessHandle.of(pid).orElseThrow().isAlive()) {
                return true;
            }
            Thread.sleep(20);
        }
        return false;
    }

    /** Serializes one request using the same JSONL envelope as the desktop client. */
    private static String request(String id, String method, Object params) throws IOException {
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", method, "params", params));
    }

    /** Serializes one response without placing secret data in test diagnostics or stdout checks. */
    private static String response(String id, ObjectNode result) throws IOException {
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id, "result", result));
    }

    /** In-process MCP fixture that supports initialize, tools/list, and optional delay. */
    private static final class HttpFixture implements AutoCloseable {
        private final HttpServer server;
        private final ExecutorService executor;
        private final boolean delayed;
        private final AtomicReference<String> authorization = new AtomicReference<>();
        private final java.util.concurrent.atomic.AtomicInteger toolCalls = new java.util.concurrent.atomic.AtomicInteger();
        private final java.util.concurrent.atomic.AtomicInteger requests = new java.util.concurrent.atomic.AtomicInteger();
        private volatile boolean duplicateTools;

        private HttpFixture(HttpServer server, ExecutorService executor, boolean delayed) {
            this.server = server;
            this.executor = executor;
            this.delayed = delayed;
        }

        /** Starts one loopback endpoint so probe tests do not require an external service. */
        static HttpFixture start(boolean delayed) throws IOException {
            HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
            ExecutorService executor = Executors.newCachedThreadPool();
            HttpFixture fixture = new HttpFixture(server, executor, delayed);
            server.createContext("/mcp", fixture::handle);
            server.setExecutor(executor);
            server.start();
            return fixture;
        }

        /** Returns the loopback endpoint consumed by the official Streamable HTTP client. */
        String url() {
            return "http://127.0.0.1:" + server.getAddress().getPort() + "/mcp";
        }

        /** Makes the next discovery expose duplicate raw names so projection fails closed. */
        void duplicateTools() {
            duplicateTools = true;
        }

        /** Returns the number of tools/call requests, which must remain zero during probe-only B1b. */
        int toolCallCount() {
            return toolCalls.get();
        }

        /** Returns all HTTP lifecycle requests so a rejected auth probe can prove it did no I/O. */
        int requestCount() {
            return requests.get();
        }

        /** Handles only the MCP lifecycle and discovery calls used by B1b. */
        private void handle(HttpExchange exchange) throws IOException {
            requests.incrementAndGet();
            authorization.set(exchange.getRequestHeaders().getFirst("Authorization"));
            String body = new String(exchange.getRequestBody().readAllBytes(), StandardCharsets.UTF_8);
            if (!body.contains("\"id\"")) {
                exchange.sendResponseHeaders(202, -1);
                exchange.close();
                return;
            }
            String id = body.replaceFirst("(?s).*\"id\"\\s*:\\s*(\"[^\"]+\"|[0-9]+).*", "$1");
            String method = body.replaceFirst("(?s).*\"method\"\\s*:\\s*\"([^\"]+)\".*", "$1");
            if (delayed && ("initialize".equals(method) || "tools/list".equals(method))) {
                try {
                    Thread.sleep(700);
                } catch (InterruptedException interrupted) {
                    Thread.currentThread().interrupt();
                }
            }
            if ("tools/call".equals(method)) {
                toolCalls.incrementAndGet();
            }
            String protocolVersion = "2024-11-05";
            String result = switch (method) {
                case "initialize" -> "{\"protocolVersion\":\"" + protocolVersion + "\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1\"}}";
                case "tools/list" -> duplicateTools
                        ? "{\"tools\":[{\"name\":\"echo\",\"description\":\"echo\",\"inputSchema\":{\"type\":\"object\"}},{\"name\":\"echo\",\"description\":\"duplicate\",\"inputSchema\":{\"type\":\"object\"}}]}"
                        : "{\"tools\":[{\"name\":\"echo\",\"title\":\"hidden title\",\"description\":\"echo\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"value\":{\"type\":\"string\"}},\"required\":[\"value\"]},\"outputSchema\":{\"type\":\"string\"},\"_meta\":{\"provider\":\"hidden\"}}]}";
                default -> "{}";
            };
            byte[] bytes = ("{\"jsonrpc\":\"2.0\",\"id\":" + id + ",\"result\":" + result + "}")
                    .getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("Content-Type", "application/json");
            exchange.getResponseHeaders().set("MCP-Protocol-Version", "2024-11-05");
            exchange.getResponseHeaders().set("Mcp-Session-Id", "probe-fixture");
            exchange.sendResponseHeaders(200, bytes.length);
            try (OutputStream output = exchange.getResponseBody()) {
                output.write(bytes);
            }
        }

        /** Stops endpoint workers so the test cannot mask an MCP child/HTTP leak. */
        @Override
        public void close() {
            server.stop(0);
            executor.shutdownNow();
        }
    }

    /** Owns the client pipes and only exposes ID/method-filtered reads to avoid order assumptions. */
    private static final class Session implements AutoCloseable {
        private final PipedOutputStream input;
        private final BufferedWriter writer;
        private final BufferedReader reader;
        private final StdioRuntime runtime;
        private final CompletableFuture<Integer> exit;
        private final Path workspaceRoot;
        private final Map<String, JsonNode> stash = new HashMap<>();

        private Session(PipedOutputStream input, BufferedWriter writer, BufferedReader reader,
                        StdioRuntime runtime, CompletableFuture<Integer> exit, Path workspaceRoot) {
            this.input = input;
            this.writer = writer;
            this.reader = reader;
            this.runtime = runtime;
            this.exit = exit;
            this.workspaceRoot = workspaceRoot;
        }

        /** Creates a fake sidecar with real protocol lifecycle and an isolated data directory. */
        static Session open(Path temp) throws IOException {
            Path data = Files.createDirectory(temp.resolve("data-" + System.nanoTime()));
            Path workspace = Files.createDirectory(temp.resolve("workspace-" + System.nanoTime()));
            PipedOutputStream clientInput = new PipedOutputStream();
            PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
            PipedOutputStream serverOutput = new PipedOutputStream();
            PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
            StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                    new SidecarConfiguration(SidecarConfiguration.RuntimeMode.FAKE, data));
            CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
            return new Session(clientInput,
                    new BufferedWriter(new OutputStreamWriter(clientInput, StandardCharsets.UTF_8)),
                    new BufferedReader(new InputStreamReader(clientOutput, StandardCharsets.UTF_8)),
                    runtime, exit, workspace);
        }

        /** Completes the Java handshake before any MCP settings method is admitted. */
        JsonNode initialize() throws Exception {
            send(initializeFrame());
            JsonNode result = read();
            assertFalse(result.has("error"));
            send("{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\""
                    + READY_TOKEN + "\"}}\n");
            assertEquals("ready", read().path("params").path("status").textValue());
            return result;
        }

        /** Opens the temporary workspace required by MCP process/cwd policy. */
        void workspace() throws Exception {
            ObjectNode params = JsonNodes.object();
            params.put("workspaceId", "ws_mcp_probe");
            params.put("rootPath", workspaceRoot.toString());
            params.put("trust", "trusted");
            send(request("c:workspace", "workspace/open", params));
            assertFalse(read().has("error"));
        }

        /** Writes one complete JSONL frame. */
        void send(String frame) throws IOException {
            writer.write(frame);
            if (!frame.endsWith("\n")) {
                writer.write('\n');
            }
            writer.flush();
        }

        /** Reads one frame while enforcing that secret/provider text never enters stdout. */
        JsonNode read() throws IOException {
            String line = reader.readLine();
            assertNotNull(line);
            assertFalse(line.isBlank());
            assertFalse(line.contains("fixture-secret"));
            assertFalse(line.contains("secret-ref://"));
            return JSON.readTree(line);
        }

        /** Confirms a rejected request did not leave an asynchronous server frame buffered. */
        void assertNoStashedFrames() {
            assertTrue(stash.isEmpty());
        }

        /** Reads until one request id is found and retains other asynchronous responses. */
        JsonNode readUntilId(String id) throws IOException {
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

        /** Reads until the Java-to-Rust MCP secret request is observed. */
        JsonNode readUntilMethod(String method) throws IOException {
            while (true) {
                JsonNode value = read();
                if (method.equals(value.path("method").textValue())) {
                    return value;
                }
            }
        }

        /** Creates the client capability offer used to negotiate all B1b probe fields. */
        private static String initializeFrame() throws IOException {
            ObjectNode params = JsonNodes.object();
            params.put("protocolMajor", 1);
            params.put("protocolMinor", 0);
            params.put("minimumCompatibleMinor", 0);
            params.put("clientVersion", "mcp-probe-test");
            params.set("capabilities", JSON.readTree("{\"methods\":[\"initialize\",\"version\",\"capabilities/read\",\"health/read\",\"shutdown\",\"workspace/open\",\"profile/save\",\"profile/activate\",\"skill/list\",\"mcp/list\",\"mcp/save\",\"mcp/delete\",\"mcp/test\",\"mcp/tools/read\",\"turn/start\"],\"events\":[],\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"],\"itemKinds\":[],\"mcp\":{\"protocolVersions\":[\"2024-11-05\",\"2025-03-26\",\"2025-06-18\"],\"transports\":[\"stdio\",\"streamable_http\"],\"features\":[\"tools_list\"]}}"));
            params.set("limits", JSON.readTree("{\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024,\"maxInFlightRequests\":64,\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536,\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576,\"defaultRequestDeadlineMs\":120000,\"defaultApprovalDeadlineMs\":300000}"));
            return request("c:init", "initialize", params);
        }

        /** Closes pipes and waits for the Java sidecar to finish its bounded drain. */
        @Override
        public void close() throws Exception {
            writer.close();
            input.close();
            runtime.close();
            reader.close();
            assertEquals(0, exit.get(10, TimeUnit.SECONDS));
        }
    }
}
