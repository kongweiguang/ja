/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import io.agentscope.core.message.ToolResultBlock;
import io.agentscope.core.message.ToolUseBlock;
import io.agentscope.core.tool.ToolCallParam;
import io.agentscope.core.tool.Toolkit;
import io.agentscope.harness.agent.tools.McpServerConfig;
import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;

/** Proves JA only composes official AgentScope MCP clients and bounds final results. */
class McpRuntimeTest {
    /** The upstream Toolkit performs discovery and invokes the local HTTP fixture. */
    @Test
    void httpUsesOfficialToolkitAndResolvesSecretAtBoundary() throws InterruptedException {
        try (HttpFixture fixture = HttpFixture.start();
                McpRuntime runtime = runtime(McpLimits.DEFAULT, request -> "resolved-secret")) {
            McpServerConfig config = httpConfig(fixture.url(), Duration.ofSeconds(3));
            McpRuntime.ServerStatus status = runtime.connect("local", config);
            assertEquals(McpRuntime.State.READY, status.state());
            assertEquals("Bearer resolved-secret", fixture.authorization.get());
            assertFalse(config.getHeaders().get("Authorization").contains("resolved-secret"));

            String toolName = runtime.toolkit().getToolNames().stream()
                    .filter(name -> name.contains("echo"))
                    .findFirst()
                    .orElseThrow();
            ToolResultBlock result = runtime.toolkit().callTool(ToolCallParam.builder()
                    .toolUseBlock(ToolUseBlock.builder()
                            .id("call-1")
                            .name(toolName)
                            .input(Map.of("value", "x"))
                            .content("{\"value\":\"x\"}")
                            .build())
                    .input(Map.of("value", "x"))
                    .build()).block(Duration.ofSeconds(3));
            assertNotNull(result);
            // A real AgentScope MCP tool is a normal Toolkit call: it returns a ToolResultBlock
            // directly and must not enter the unsupported external-execution pause path.
            assertFalse(result.isSuspended());
            assertTrue(fixture.called.await(1, TimeUnit.SECONDS));
        }
    }

    /** The official builder timeout completes a slow tool without a JA network implementation. */
    @Test
    void slowHttpCallIsBoundedByOfficialClientTimeout() throws InterruptedException {
        try (HttpFixture fixture = HttpFixture.start(true);
                McpRuntime runtime = runtime(McpLimits.DEFAULT, reference -> "unused")) {
            McpServerConfig config = httpConfig(fixture.url(), Duration.ofMillis(150));
            assertEquals(McpRuntime.State.READY, runtime.connect("slow", config).state());
            String toolName = runtime.toolkit().getToolNames().stream()
                    .filter(name -> name.contains("echo"))
                    .findFirst()
                    .orElseThrow();
            ToolResultBlock result = runtime.toolkit().callTool(ToolCallParam.builder()
                    .toolUseBlock(ToolUseBlock.builder()
                            .id("slow-call")
                            .name(toolName)
                            .input(Map.of())
                            .content("{}")
                            .build())
                    .input(Map.of())
                    .build()).block(Duration.ofSeconds(2));
            assertNotNull(result);
            assertTrue(fixture.called.await(1, TimeUnit.SECONDS));
        }
    }

    /** A stdio process uses the narrow cwd/env port while SDK close reaps the child. */
    @Test
    void stdioUsesControlledProcessPortAndOfficialCleanup() throws Exception {
        Path directory = Files.createTempDirectory("ja-mcp-stdio");
        Path pidFile = directory.resolve("pid");
        AtomicReference<Map<String, String>> environment = new AtomicReference<>();
        McpProcessPort delegate = McpProcessPort.restricted(directory);
        McpProcessPort port = (command, values) -> {
            environment.set(Map.copyOf(values));
            return delegate.prepare(command, values);
        };
        long pid;
        try (McpRuntime runtime = new McpRuntime(
                reference -> "resolved-secret", port, new Toolkit(), McpLimits.DEFAULT)) {
            McpServerConfig config = new McpServerConfig();
            config.setTransport("stdio");
            config.setCommand(javaExecutable());
            config.setArgs(List.of("-cp", absoluteClasspath(), StdioFixture.class.getName(), pidFile.toString()));
            config.setEnv(Map.of("MCP_FIXTURE_SECRET", "secret-ref://token"));
            config.setEnableTools(List.of("echo"));
            assertEquals(McpRuntime.State.READY, runtime.connect("stdio", config).state());
            String toolName = runtime.toolkit().getToolNames().stream()
                    .filter(name -> name.contains("echo"))
                    .findFirst()
                    .orElseThrow();
            assertNotNull(runtime.toolkit().callTool(ToolCallParam.builder()
                    .toolUseBlock(ToolUseBlock.builder()
                            .id("call-stdio")
                            .name(toolName)
                            .input(Map.of())
                            .content("{}")
                            .build())
                    .input(Map.of())
                    .build()).block(Duration.ofSeconds(3)));
            assertEquals("resolved-secret", environment.get().get("MCP_FIXTURE_SECRET"));
            assertTrue(awaitFile(pidFile));
            pid = Long.parseLong(Files.readString(pidFile));
        }
        assertTrue(awaitProcessExit(pid));
        deleteTree(directory);
    }

    /** Invalid transport is rejected before any upstream client or network is created. */
    @Test
    void unsupportedTransportIsStable() {
        try (McpRuntime runtime = runtime(McpLimits.DEFAULT, reference -> "unused")) {
            McpServerConfig config = new McpServerConfig();
            config.setTransport("sse");
            McpRuntime.ServerStatus status = runtime.connect("legacy", config);
            assertEquals(McpRuntime.State.FAILED, status.state());
            assertEquals("mcp_transport_unsupported", status.error());
        }
    }

    /** Runtime close uses upstream removal and exposes a closed status without provider text. */
    @Test
    void closeIsIdempotentAndStatusIsStable() {
        McpRuntime runtime = runtime(McpLimits.DEFAULT, reference -> "unused");
        runtime.close();
        runtime.close();
        assertEquals(McpRuntime.State.DISCONNECTED, runtime.status("local").state());
    }

    /** Builds the runtime with explicit process and Toolkit ownership. */
    private static McpRuntime runtime(McpLimits limits, McpRuntime.SecretResolver resolver) {
        return new McpRuntime(resolver, McpProcessPort.restricted(), new Toolkit(), limits);
    }

    /** Builds the upstream Harness configuration shape without introducing a JA duplicate. */
    private static McpServerConfig httpConfig(String url, Duration timeout) {
        McpServerConfig config = new McpServerConfig();
        config.setTransport("http");
        config.setUrl(url);
        config.setHeaders(Map.of("Authorization", "Bearer secret-ref://token"));
        config.setTimeout(timeout);
        config.setInitializationTimeout(timeout);
        return config;
    }

    /** Resolves the Java executable without relying on an inherited PATH. */
    private static String javaExecutable() {
        Path candidate = Path.of(System.getProperty("java.home"), "bin",
                System.getProperty("os.name", "").toLowerCase().contains("win") ? "java.exe" : "java");
        return candidate.toString();
    }

    /** Resolves test classes because the child process starts in its restricted cwd. */
    private static String absoluteClasspath() {
        String separator = System.getProperty("path.separator");
        return Arrays.stream(System.getProperty("java.class.path").split(java.util.regex.Pattern.quote(separator)))
                .map(entry -> Path.of(entry).toAbsolutePath().normalize().toString())
                .collect(java.util.stream.Collectors.joining(separator));
    }

    /** Waits for the child fixture to publish its PID. */
    private static boolean awaitFile(Path path) throws IOException, InterruptedException {
        for (int attempt = 0; attempt < 100; attempt++) {
            if (Files.exists(path) && Files.size(path) > 0) {
                return true;
            }
            Thread.sleep(20);
        }
        return false;
    }

    /** Confirms the official transport closed the child process. */
    private static boolean awaitProcessExit(long pid) throws InterruptedException {
        for (int attempt = 0; attempt < 150; attempt++) {
            if (ProcessHandle.of(pid).isEmpty() || !ProcessHandle.of(pid).orElseThrow().isAlive()) {
                return true;
            }
            Thread.sleep(20);
        }
        return false;
    }

    /** Removes only the temporary fixture directory. */
    private static void deleteTree(Path directory) throws IOException {
        if (Files.exists(directory)) {
            try (var paths = Files.walk(directory)) {
                paths.sorted(java.util.Comparator.reverseOrder()).forEach(path -> {
                    try {
                        Files.deleteIfExists(path);
                    } catch (IOException failure) {
                        throw new IllegalStateException("fixture_cleanup_failed");
                    }
                });
            }
        }
    }

    /** In-process Streamable HTTP fixture; no external service or network is used. */
    private static final class HttpFixture implements AutoCloseable {
        private final HttpServer server;
        private final ExecutorService executor;
        private final boolean delayed;
        private final AtomicReference<String> authorization = new AtomicReference<>();
        private final CountDownLatch called = new CountDownLatch(1);

        private HttpFixture(HttpServer server, ExecutorService executor, boolean delayed) {
            this.server = server;
            this.executor = executor;
            this.delayed = delayed;
        }

        /** Starts the fixture on loopback with a random port. */
        static HttpFixture start() {
            return start(false);
        }

        /** Starts a fixture whose tool response intentionally exceeds the client timeout. */
        static HttpFixture start(boolean delayed) {
            try {
                HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
                ExecutorService executor = Executors.newCachedThreadPool();
                HttpFixture fixture = new HttpFixture(server, executor, delayed);
                server.createContext("/mcp", fixture::handle);
                server.setExecutor(executor);
                server.start();
                return fixture;
            } catch (IOException failure) {
                throw new IllegalStateException("fixture_start_failed");
            }
        }

        /** Returns the loopback endpoint. */
        String url() {
            return "http://127.0.0.1:" + server.getAddress().getPort() + "/mcp";
        }

        /** Handles only initialize/list/call and initialized notifications. */
        private void handle(HttpExchange exchange) throws IOException {
            authorization.set(exchange.getRequestHeaders().getFirst("Authorization"));
            String request = new String(exchange.getRequestBody().readAllBytes(), StandardCharsets.UTF_8);
            if (!request.contains("\"id\"")) {
                exchange.sendResponseHeaders(202, -1);
                exchange.close();
                return;
            }
            String id = request.replaceFirst("(?s).*\"id\"\\s*:\\s*(\"[^\"]+\"|[0-9]+).*", "$1");
            String method = request.replaceFirst("(?s).*\"method\"\\s*:\\s*\"([^\"]+)\".*", "$1");
            if ("tools/call".equals(method)) {
                called.countDown();
                if (delayed) {
                    try {
                        Thread.sleep(2_000);
                    } catch (InterruptedException interrupted) {
                        Thread.currentThread().interrupt();
                    }
                }
            }
            String result = switch (method) {
                case "initialize" -> "{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1\"}}";
                case "tools/list" -> "{\"tools\":[{\"name\":\"echo\",\"description\":\"echo\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"value\":{\"type\":\"string\"}}}}]}";
                case "tools/call" -> "{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"isError\":false}";
                default -> "{}";
            };
            byte[] body = ("{\"jsonrpc\":\"2.0\",\"id\":" + id + ",\"result\":" + result + "}")
                    .getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("Content-Type", "application/json");
            exchange.getResponseHeaders().set("MCP-Protocol-Version", "2024-11-05");
            exchange.getResponseHeaders().set("Mcp-Session-Id", "fixture-session");
            exchange.sendResponseHeaders(200, body.length);
            try (OutputStream output = exchange.getResponseBody()) {
                output.write(body);
            }
        }

        /** Stops fixture threads so direct JUnit runs terminate deterministically. */
        @Override
        public void close() {
            server.stop(0);
            executor.shutdownNow();
        }
    }

    /** Trusted local JVM fixture used only to verify official process cleanup. */
    public static final class StdioFixture {
        /** Speaks the minimal MCP request set and exits when the parent closes stdin. */
        public static void main(String[] args) throws Exception {
            Path pidFile = Path.of(args[0]);
            Files.writeString(pidFile, Long.toString(ProcessHandle.current().pid()), StandardCharsets.UTF_8);
            try (BufferedReader reader = new BufferedReader(
                    new InputStreamReader(System.in, StandardCharsets.UTF_8))) {
                String line;
                while ((line = reader.readLine()) != null) {
                    if (!line.contains("\"id\"")) {
                        continue;
                    }
                    String id = line.replaceFirst("(?s).*\"id\"\\s*:\\s*(\"[^\"]+\"|[0-9]+).*", "$1");
                    String method = line.replaceFirst("(?s).*\"method\"\\s*:\\s*\"([^\"]+)\".*", "$1");
                    String result = switch (method) {
                        case "initialize" -> "{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"stdio\",\"version\":\"1\"}}";
                        case "tools/list" -> "{\"tools\":[{\"name\":\"echo\",\"description\":\"echo\",\"inputSchema\":{\"type\":\"object\"}}]}";
                        case "tools/call" -> "{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"isError\":false}";
                        default -> "{}";
                    };
                    System.out.println("{\"jsonrpc\":\"2.0\",\"id\":" + id + ",\"result\":" + result + "}");
                    System.out.flush();
                }
            }
        }
    }
}
