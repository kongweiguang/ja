/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.spike;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import io.agentscope.core.skill.repository.AgentSkillRepository;
import io.agentscope.harness.agent.HarnessAgent;
import io.modelcontextprotocol.spec.McpSchema;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.TimeoutException;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Integration evidence for the real AgentScope Skill/Harness/MCP paths used by JA. */
class McpSkillsProbeTest {
    private static final ObjectMapper MAPPER = new ObjectMapper();

    @TempDir Path tempDir;

    /**
     * Demonstrates source layering, prompt-safe indexing, strict reload and HarnessAgent wiring
     * without ever executing a skill resource.
     */
    @Test
    void skillsUseAgentScopeRepositoriesAndRetainLastGoodSnapshot() throws Exception {
        Path user = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspace = Files.createDirectories(tempDir.resolve("workspace-skills"));
        writeSkill(user, "user-demo", "User skill body must not enter the index.");
        writeSkill(workspace, "workspace-demo", "Workspace skill body loaded on demand.");

        try (SkillCatalog catalog = new SkillCatalog()) {
            catalog.addClasspath("builtin-skills");
            catalog.addFilesystem(SkillCatalog.Source.USER, user, false);
            catalog.addFilesystem(SkillCatalog.Source.WORKSPACE, workspace, true);
            SkillCatalog.ReloadReport initial = catalog.reload();

            assertEquals(3, initial.active().size());
            assertTrue(catalog.indexPrompt().contains("workspace-demo"));
            assertFalse(catalog.indexPrompt().contains("Workspace skill body"));
            SkillCatalog.SkillId id = new SkillCatalog.SkillId(SkillCatalog.Source.WORKSPACE, "workspace-demo");
            SkillCatalog.SkillIndex index = catalog.index().stream()
                    .filter(entry -> entry.id().equals(id))
                    .findFirst()
                    .orElseThrow();
            assertEquals("Workspace skill body loaded on demand.", catalog.loadBody(id, index.revision()));

            catalog.disable(id);
            assertFalse(catalog.indexPrompt().contains("workspace-demo"));
            catalog.enable(id);

            Files.writeString(
                    workspace.resolve("workspace-demo").resolve("SKILL.md"),
                    "---\nname: workspace-demo\ndescription: broken\n---\n",
                    StandardCharsets.UTF_8);
            SkillCatalog.ReloadReport broken = catalog.reload();
            assertTrue(broken.rejected().stream().anyMatch(path -> path.contains("workspace-demo")));
            assertEquals(
                    "Workspace skill body loaded on demand.",
                    catalog.loadBody(id, index.revision()),
                    "an invalid edit must not replace the last-good snapshot");
            writeSkill(workspace, "workspace-demo", "Workspace skill body loaded on demand.");
            Files.createDirectories(tempDir.resolve("harness-workspace"));
            catalog.reload();

            List<AgentSkillRepository> repositories = catalog.repositories();
            assertEquals(3, repositories.size());
            try (HarnessAgent harness = HarnessAgent.builder()
                    .name("ja-skill-probe")
                    .workspace(tempDir.resolve("harness-workspace"))
                    .skillRepositories(repositories)
                    .disableDefaultWorkspaceSkills()
                    .disableDynamicSkills()
                    .disableFilesystemTools()
                    .disableShellTool()
                    .disableMemoryTools()
                    .disableMemoryHooks()
                    .disableWorkspaceContext()
                    .disableAtPathExpansion()
                    .disableSubagents()
                    .disableSessionPersistence()
                    .disableToolsConfig()
                    .build()) {
                assertEquals(3, harness.getSkillRepositories().size());
            }
        }
    }

    /**
     * Proves that imports are delegated to AgentScope's SkillUtil after JA's zip/path limits, and
     * that traversal packages never reach the repository write path.
     */
    @Test
    void skillImportRejectsZipSlipAndAcceptsBoundedPackage() throws Exception {
        Path workspace = Files.createDirectories(tempDir.resolve("workspace-skills"));
        try (SkillCatalog catalog = new SkillCatalog()) {
            catalog.addFilesystem(SkillCatalog.Source.WORKSPACE, workspace, true);
            SkillCatalog.SkillIndex imported = catalog.importZip(
                    SkillCatalog.Source.WORKSPACE,
                    zip(Map.of(
                            "package/SKILL.md",
                            "---\nname: imported-demo\ndescription: imported\n---\nImported body.",
                            "package/scripts/run.sh",
                            "echo never executed")));
            assertEquals("imported-demo", imported.id().name());
            assertTrue(Files.exists(workspace.resolve("imported-demo").resolve("SKILL.md")));

            byte[] zipSlip = zip(Map.of(
                    "package/SKILL.md",
                    "---\nname: bad\ndescription: bad\n---\nbody",
                    "package/../outside.txt",
                    "must reject"));
            assertThrows(IOException.class, () -> catalog.importZip(SkillCatalog.Source.WORKSPACE, zipSlip));
            assertFalse(Files.exists(workspace.resolve("outside.txt")));
        }
    }

    /** Confirms that a filesystem catalog reports malicious directory names instead of loading them. */
    @Test
    void skillFilesystemRejectsMaliciousDirectoryNames() throws Exception {
        Path workspace = Files.createDirectories(tempDir.resolve("workspace-skills"));
        writeSkill(workspace, "safe-demo", "safe");
        Path malicious = Files.createDirectories(workspace.resolve("bad name"));
        Files.writeString(
                malicious.resolve("SKILL.md"),
                "---\nname: bad-name\ndescription: bad\n---\nbody",
                StandardCharsets.UTF_8);
        try (SkillCatalog catalog = new SkillCatalog()) {
            catalog.addFilesystem(SkillCatalog.Source.WORKSPACE, workspace, false);
            SkillCatalog.ReloadReport report = catalog.reload();
            assertEquals(1, report.active().size());
            assertTrue(report.rejected().stream().anyMatch(path -> path.contains("bad name")));
        }
    }

    /**
     * Uses AgentScope's wrapper over JA's safe stdio transport and verifies environment isolation,
     * approval, duplicate protection, close cleanup and crash semantics against real child JVMs.
     */
    @Test
    void mcpStdioUsesRealToolsListCallAndNoReplayAfterCrash() throws Exception {
        Path java = Path.of(
                System.getProperty("java.home"),
                "bin",
                System.getProperty("os.name").toLowerCase().contains("win") ? "java.exe" : "java");
        String classPath = System.getProperty("surefire.real.class.path", System.getProperty("java.class.path"));
        String stderrMarker = "stderr-" + UUID.randomUUID();
        Path report = tempDir.resolve("stdio-environment.json");
        String resolvedSecret = "opaque-" + UUID.randomUUID();
        McpToolGateway.SecretResolver resolver = reference -> {
            assertEquals("token", reference);
            return resolvedSecret;
        };
        try (McpToolGateway gateway = new McpToolGateway(resolver)) {
            assertThrows(IllegalArgumentException.class, () -> gateway.connect(new McpToolGateway.ServerConfig(
                    "unsafe-argv",
                    McpToolGateway.Transport.STDIO,
                    java.toString(),
                    List.of("secret-ref://token"),
                    Map.of(),
                    null,
                    Map.of(),
                    List.of("2024-11-05"),
                    Duration.ofSeconds(2),
                    Duration.ofSeconds(2),
                    Duration.ZERO,
                    1,
                    "none",
                    Set.of())));
            McpToolGateway.ServerConfig config = stdioConfig(
                    "stdio-fixture", java, classPath, report, stderrMarker, false);
            assertEquals("echo", gateway.connect(config).getFirst().name());
            JsonNode environment = MAPPER.readTree(Files.readString(report));
            assertFalse(environment.path("parentSecretVisible").asBoolean());
            assertTrue(environment.path("allowlistVisible").asBoolean());
            assertTrue(environment.path("secretEnvResolved").asBoolean());
            assertTrue(environment.path("stderrMarkerEmitted").asBoolean());
            String parentMarker = System.getenv("JA_MCP_PARENT_SECRET");
            assertFalse(parentMarker == null || parentMarker.isBlank(),
                    "JVM regression requires an explicit parent marker outside the SDK baseline allowlist");
            assertFalse(environment.path("parentSecretVisible").asBoolean(),
                    "parent marker must not cross the stdio environment boundary");
            assertTrue(environment.path("allowlistVisible").asBoolean(),
                    "explicit non-sensitive environment must cross the stdio boundary");
            assertTrue(environment.path("secretEnvResolved").asBoolean(),
                    "secret-ref environment must be resolved only at the transport boundary");
            assertEquals(McpToolGateway.Status.ASK, gateway.call("stdio-fixture", "call-1", "echo", Map.of("value", "x"), false).status());
            McpToolGateway.CallOutcome completed = gateway.call(
                    "stdio-fixture", "call-1", "echo", Map.of("value", "x"), true);
            assertEquals(McpToolGateway.Status.COMPLETED, completed.status());
            assertTrue(completed.invoked());
            McpToolGateway.CallOutcome duplicate = gateway.call(
                    "stdio-fixture", "call-1", "echo", Map.of("value", "different"), true);
            assertEquals(McpToolGateway.Status.DUPLICATE, duplicate.status());
            assertFalse(duplicate.invoked());
            assertFalse(gateway.asAgentScopeTool("stdio-fixture", "echo").isReadOnly());

            gateway.close();
            assertFixtureProcessGone();
            assertNoTextInReports(stderrMarker);

            // Closing must forget replay records so a newly connected session can reuse a call id.
            gateway.connect(config);
            assertEquals(McpToolGateway.Status.COMPLETED,
                    gateway.call("stdio-fixture", "call-1", "echo", Map.of("value", "reconnected"), true).status());
            gateway.close();
            assertFixtureProcessGone();
        }

        Path crashReport = tempDir.resolve("stdio-crash-environment.json");
        String crashStderrMarker = "stderr-crash-" + UUID.randomUUID();
        try (McpToolGateway crashGateway = new McpToolGateway(resolver)) {
            McpToolGateway.ServerConfig config = stdioConfig(
                    "stdio-crash-fixture", java, classPath, crashReport, crashStderrMarker, true);
            assertEquals("echo", crashGateway.connect(config).getFirst().name());
            assertEquals(McpToolGateway.Status.COMPLETED,
                    crashGateway.call("stdio-crash-fixture", "call-1", "echo", Map.of("value", "x"), true).status());
            McpToolGateway.CallOutcome crashed = crashGateway.call(
                    "stdio-crash-fixture", "call-2", "echo", Map.of("value", "after-crash"), true);
            assertEquals(McpToolGateway.Status.FAILED, crashed.status());
            assertTrue(crashed.invoked());
            assertTrue(Set.of("mcp_transport_failure", "mcp_timeout").contains(crashed.error()));
            crashGateway.close();
            assertFixtureProcessGone();
            assertNoTextInReports(crashStderrMarker);
        }
    }

    /**
     * Exercises Streamable HTTP, static secret-ref headers, malformed schemas/results, protocol
     * negotiation and stable unsupported capability errors with an in-process local server.
     */
    @Test
    void mcpHttpUsesStaticSecretRefAndRejectsUnsupportedOrMalformedSurfaces() {
        String resolvedSecret = "opaque-" + UUID.randomUUID();
        try (HttpMcpFixture fixture = HttpMcpFixture.start(HttpMcpFixture.Mode.NORMAL);
                McpToolGateway gateway = new McpToolGateway(reference -> {
                    assertEquals("token", reference);
                    return resolvedSecret;
                })) {
            McpToolGateway.ServerConfig initial = McpToolGateway.ServerConfig.streamableHttp(
                    "http-fixture", "http://127.0.0.1:" + fixture.port() + "/mcp");
            McpToolGateway.ServerConfig base = new McpToolGateway.ServerConfig(
                    initial.name(), initial.transport(), initial.command(), initial.args(), initial.env(), initial.url(),
                    Map.of("Authorization", "Bearer secret-ref://token", "X-Test", "static"),
                    initial.protocolVersions(), initial.requestTimeout(), initial.initializationTimeout(),
                    initial.retryDelay(), initial.maxConnectAttempts(), initial.authMode(), initial.requestedCapabilities());
            assertEquals("echo", gateway.connect(base).getFirst().name());
            assertEquals("Bearer " + resolvedSecret, fixture.authorization());
            assertEquals(McpToolGateway.Status.COMPLETED,
                    gateway.call("http-fixture", "http-call", "echo", Map.of("value", "ok"), true).status());
            assertFalse(fixture.lastPathAndQuery().contains(resolvedSecret));
            assertFalse(fixture.lastPathAndQuery().contains("secret-ref"));

            assertThrows(UnsupportedCapabilityException.class, () -> gateway.connect(new McpToolGateway.ServerConfig(
                    "oauth", McpToolGateway.Transport.STREAMABLE_HTTP, null, List.of(), Map.of(),
                    base.url(), Map.of(), null, null, null, null, 1, "oauth", Set.of())));
            assertThrows(UnsupportedCapabilityException.class, () -> gateway.connect(new McpToolGateway.ServerConfig(
                    "resources", McpToolGateway.Transport.STREAMABLE_HTTP, null, List.of(), Map.of(),
                    base.url(), Map.of(), null, null, null, null, 1, "none", Set.of("resources"))));
            assertThrows(IllegalArgumentException.class,
                    () -> gateway.connect(httpConfig("bad-scheme", "ftp://127.0.0.1/mcp", Map.of())));
            assertThrows(IllegalArgumentException.class,
                    () -> gateway.connect(httpConfig("userinfo", "http://user:pass@127.0.0.1/mcp", Map.of())));
            assertThrows(IllegalArgumentException.class,
                    () -> gateway.connect(httpConfig("fragment", "http://127.0.0.1/mcp#secret", Map.of())));
            assertThrows(IllegalArgumentException.class,
                    () -> gateway.connect(httpConfig("url-crlf", "http://127.0.0.1/mcp\r\nX", Map.of())));
            assertThrows(IllegalArgumentException.class,
                    () -> gateway.connect(httpConfig(
                            "literal-auth", base.url(), Map.of("Authorization", "Bearer literal"))));
            assertThrows(IllegalArgumentException.class,
                    () -> gateway.connect(httpConfig(
                            "header-crlf", base.url(), Map.of("X-Test", "line\nvalue"))));
        }

        try (HttpMcpFixture malformedSchema = HttpMcpFixture.start(HttpMcpFixture.Mode.MALFORMED_SCHEMA);
                McpToolGateway gateway = new McpToolGateway()) {
            McpToolGateway.ServerConfig config = McpToolGateway.ServerConfig.streamableHttp(
                    "bad-schema", "http://127.0.0.1:" + malformedSchema.port() + "/mcp");
            assertThrows(IllegalStateException.class, () -> gateway.connect(config));
        }

        try (HttpMcpFixture malformedResult = HttpMcpFixture.start(HttpMcpFixture.Mode.MALFORMED_RESULT);
                McpToolGateway gateway = new McpToolGateway()) {
            McpToolGateway.ServerConfig config = McpToolGateway.ServerConfig.streamableHttp(
                    "bad-result", "http://127.0.0.1:" + malformedResult.port() + "/mcp");
            gateway.connect(config);
            assertEquals(McpToolGateway.Status.FAILED,
                    gateway.call("bad-result", "bad-result-call", "echo", Map.of("value", "bad"), true).status());
        }

        try (HttpMcpFixture badVersion = HttpMcpFixture.start(HttpMcpFixture.Mode.BAD_VERSION);
                McpToolGateway gateway = new McpToolGateway()) {
            McpToolGateway.ServerConfig config = new McpToolGateway.ServerConfig(
                    "bad-version", McpToolGateway.Transport.STREAMABLE_HTTP, null, List.of(), Map.of(),
                    "http://127.0.0.1:" + badVersion.port() + "/mcp", Map.of(), List.of("2024-11-05"),
                    Duration.ofSeconds(2), Duration.ofSeconds(2), Duration.ZERO, 1, "none", Set.of());
            assertThrows(IllegalStateException.class, () -> gateway.connect(config));
        }
    }

    /**
     * Builds a fixture command with explicit environment values so the child process cannot rely
     * on the parent environment or carry a secret reference in argv.
     */
    private static McpToolGateway.ServerConfig stdioConfig(
            String name,
            Path java,
            String classPath,
            Path report,
            String stderrMarker,
            boolean crashAfterCall) {
        List<String> args = new ArrayList<>(List.of("-cp", classPath, FixtureMcpServer.class.getName()));
        if (crashAfterCall) {
            args.add("--crash-after-call");
        }
        args.add("--env-report-file");
        args.add(report.toString());
        return new McpToolGateway.ServerConfig(
                name,
                McpToolGateway.Transport.STDIO,
                java.toString(),
                args,
                Map.of(
                        "JA_MCP_ALLOWLIST_ENV", "visible",
                        "JA_MCP_SECRET_ENV", "secret-ref://token",
                        "JA_MCP_STDERR_MARKER", stderrMarker),
                null,
                Map.of(),
                List.of("2024-11-05"),
                Duration.ofSeconds(5),
                Duration.ofSeconds(5),
                Duration.ZERO,
                1,
                "none",
                Set.of());
    }

    /** Builds a policy-only HTTP configuration so malformed endpoints fail before any socket opens. */
    private static McpToolGateway.ServerConfig httpConfig(
            String name, String url, Map<String, String> headers) {
        return new McpToolGateway.ServerConfig(
                name,
                McpToolGateway.Transport.STREAMABLE_HTTP,
                null,
                List.of(),
                Map.of(),
                url,
                headers,
                List.of("2024-11-05"),
                Duration.ofSeconds(2),
                Duration.ofSeconds(2),
                Duration.ZERO,
                1,
                "none",
                Set.of());
    }

    /**
     * Waits on an explicit process-liveness deadline rather than an arbitrary sleep before
     * asserting that SDK close destroyed the immediate stdio child.
     */
    private static void assertFixtureProcessGone() {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(3);
        while (true) {
            Optional<ProcessHandle> process = findFixtureProcess();
            if (process.isEmpty()) {
                return;
            }
            long remaining = deadline - System.nanoTime();
            if (remaining <= 0) {
                assertFalse(process.get().isAlive(), "stdio fixture process remains after close");
                return;
            }
            try {
                process.get().onExit().get(remaining, TimeUnit.NANOSECONDS);
            } catch (TimeoutException exception) {
                assertFalse(process.get().isAlive(), "stdio fixture process remains after close");
                return;
            } catch (InterruptedException exception) {
                Thread.currentThread().interrupt();
                throw new AssertionError("interrupted_while_waiting_for_stdio_fixture", exception);
            } catch (ExecutionException exception) {
                throw new AssertionError("stdio_fixture_exit_observation_failed", exception);
            }
        }
    }

    /** Finds the immediate fixture process so cleanup can await its own exit future. */
    private static Optional<ProcessHandle> findFixtureProcess() {
        return ProcessHandle.allProcesses()
                .filter(handle -> handle.info().commandLine()
                        .map(commandLine -> commandLine.contains(FixtureMcpServer.class.getName()))
                        .orElse(false))
                .findFirst();
    }

    /** Scans generated reports for a dynamic stderr marker that must never cross the transport. */
    private static void assertNoTextInReports(String marker) throws IOException {
        Path reports = Path.of(
                System.getProperty("user.dir"), "spikes", "mcp-skills", "target", "surefire-reports");
        if (!Files.isDirectory(reports)) {
            return;
        }
        try (var paths = Files.walk(reports)) {
            assertTrue(paths.filter(Files::isRegularFile)
                    .noneMatch(path -> contains(path, marker)), "stderr marker leaked into test reports");
        }
    }

    /** Reads one generated report as text without logging its contents or any dynamic marker. */
    private static boolean contains(Path path, String marker) {
        try {
            return Files.readString(path).contains(marker);
        } catch (IOException exception) {
            throw new IllegalStateException("report_scan_failed", exception);
        }
    }

    private static void writeSkill(Path root, String name, String body) throws IOException {
        // Keep fixture resources on disk so the real AgentScope filesystem repository sees production-like input.
        Path directory = Files.createDirectories(root.resolve(name));
        Files.writeString(
                directory.resolve("SKILL.md"),
                "---\nname: " + name + "\ndescription: " + name + " description\n---\n" + body,
                StandardCharsets.UTF_8);
        Files.createDirectories(directory.resolve("references"));
        Files.writeString(directory.resolve("references/guide.md"), "reference", StandardCharsets.UTF_8);
    }

    private static byte[] zip(Map<String, String> entries) throws IOException {
        // Build packages in memory so zip-slip tests cannot leave an untrusted archive on disk.
        ByteArrayOutputStream output = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(output, StandardCharsets.UTF_8)) {
            for (Map.Entry<String, String> entry : entries.entrySet()) {
                zip.putNextEntry(new ZipEntry(entry.getKey()));
                zip.write(entry.getValue().getBytes(StandardCharsets.UTF_8));
                zip.closeEntry();
            }
        }
        return output.toByteArray();
    }

    private static final class HttpMcpFixture implements AutoCloseable {
        enum Mode { NORMAL, MALFORMED_SCHEMA, MALFORMED_RESULT, BAD_VERSION }

        private final HttpServer server;
        private final Mode mode;
        private volatile String authorization;
        private volatile String lastPathAndQuery;
        private final AtomicInteger callCount = new AtomicInteger();

        private HttpMcpFixture(HttpServer server, Mode mode) {
            this.server = server;
            this.mode = mode;
        }

        static HttpMcpFixture start(Mode mode) {
            try {
                HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
                HttpMcpFixture fixture = new HttpMcpFixture(server, mode);
                server.createContext("/mcp", fixture::handle);
                server.start();
                return fixture;
            } catch (IOException exception) {
                throw new IllegalStateException("fixture_start_failed", exception);
            }
        }

        int port() {
            return server.getAddress().getPort();
        }

        String authorization() {
            return authorization;
        }

        String lastPathAndQuery() {
            return lastPathAndQuery == null ? "" : lastPathAndQuery;
        }

        /** Responds to the minimum MCP lifecycle and tools methods needed by the SDK client. */
        private void handle(HttpExchange exchange) throws IOException {
            lastPathAndQuery = exchange.getRequestURI().toString();
            authorization = exchange.getRequestHeaders().getFirst("Authorization");
            JsonNode request = MAPPER.readTree(exchange.getRequestBody());
            if (request == null || request.path("id").isMissingNode()) {
                exchange.sendResponseHeaders(202, -1);
                exchange.close();
                return;
            }
            ObjectNode response = MAPPER.createObjectNode();
            response.put("jsonrpc", "2.0");
            response.set("id", request.get("id"));
            ObjectNode result = MAPPER.createObjectNode();
            switch (request.path("method").asText()) {
                case "initialize" -> {
                    result.put("protocolVersion", mode == Mode.BAD_VERSION ? "2099-01-01" : "2024-11-05");
                    result.set("capabilities", MAPPER.createObjectNode().set("tools", MAPPER.createObjectNode()));
                    result.set("serverInfo", MAPPER.createObjectNode().put("name", "ja-http-fixture").put("version", "1.0"));
                }
                case "tools/list" -> result.set("tools", tools(mode == Mode.MALFORMED_SCHEMA));
                case "tools/call" -> {
                    callCount.incrementAndGet();
                    if (mode != Mode.MALFORMED_RESULT) {
                        ArrayNode content = MAPPER.createArrayNode();
                        content.add(MAPPER.createObjectNode().put("type", "text").put("text", "ok"));
                        result.set("content", content);
                        result.put("isError", false);
                    }
                }
                default -> response.set("error", MAPPER.createObjectNode().put("code", -32601).put("message", "unknown"));
            }
            if (!response.has("error")) {
                response.set("result", result);
            }
            byte[] bytes = MAPPER.writeValueAsBytes(response);
            exchange.getResponseHeaders().set("Content-Type", "application/json");
            exchange.getResponseHeaders().set("Mcp-Session-Id", "ja-fixture-session");
            exchange.sendResponseHeaders(200, bytes.length);
            try (OutputStream output = exchange.getResponseBody()) {
                output.write(bytes);
            }
        }

        /** Emits a valid or deliberately malformed tool schema to exercise the JA validation gate. */
        private static ArrayNode tools(boolean malformed) {
            ArrayNode tools = MAPPER.createArrayNode();
            ObjectNode tool = MAPPER.createObjectNode();
            tool.put("name", "echo");
            tool.put("description", "HTTP echo");
            ObjectNode schema = MAPPER.createObjectNode();
            schema.put("type", malformed ? "string" : "object");
            schema.set("properties", MAPPER.createObjectNode());
            tool.set("inputSchema", schema);
            tools.add(tool);
            return tools;
        }

        @Override
        public void close() {
            server.stop(0);
        }
    }
}
