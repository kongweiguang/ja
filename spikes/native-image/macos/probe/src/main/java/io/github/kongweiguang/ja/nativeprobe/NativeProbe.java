/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.nativeprobe;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.model.ChatResponse;
import io.agentscope.core.model.Model;
import io.agentscope.core.model.ModelCreationContext;
import io.agentscope.core.model.ModelRegistry;
import io.agentscope.core.model.ToolSchema;
import io.agentscope.core.model.GenerateOptions;
import io.agentscope.core.state.InMemoryAgentStateStore;
import io.agentscope.extensions.model.anthropic.AnthropicChatModel;
import io.agentscope.extensions.model.openai.OpenAIChatModel;
import io.agentscope.harness.agent.HarnessAgent;
import io.agentscope.harness.agent.filesystem.local.LocalFilesystem;
import io.agentscope.harness.agent.skill.WorkspaceSkillRepository;
import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.skill.AgentSkill;
import io.agentscope.core.tool.mcp.McpClientBuilder;
import io.agentscope.core.tool.mcp.McpClientWrapper;
import io.modelcontextprotocol.spec.McpSchema;
import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.security.SecureRandom;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.Statement;
import java.time.Duration;
import java.util.Base64;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManagerFactory;
import org.noear.solon.Solon;
import reactor.core.publisher.Flux;

/**
 * Runs the deterministic macOS Native Image closure probe.
 *
 * <p>The same main class is exercised on the JVM and in the generated executable so that a green
 * Maven test cannot hide a runtime-only reachability failure. It intentionally avoids real model
 * calls and network endpoints; the only external process is the local MCP fixture supplied by the
 * caller.
 */
public final class NativeProbe {

    private static final ObjectMapper JSON = new ObjectMapper();
    private static final char[] PROBE_KEY_STORE_PASSWORD = "changeit".toCharArray();
    /*
     * This is a loopback-only, test-generated PKCS#12 identity. It is embedded so the probe is
     * self-contained and does not depend on keytool, a user certificate store, or the network.
     */
    private static final String PROBE_KEY_STORE_BASE64 =
            "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";

    private NativeProbe() {}

    /**
     * Executes every required boundary in a fixed order and emits machine-readable smoke markers.
     * Keeping the order stable makes JVM/native output comparable and prevents a later failure from
     * being mistaken for a successful partial closure.
     */
    public static void main(String[] args) throws Exception {
        Path mcpScript = requiredFileArgument(args, "--mcp-script");
        Path mcpInterpreter = requiredExecutableArgument(args, "--mcp-python");
        Path workspace = Files.createTempDirectory("ja-native-probe-");
        try {
            probeSolonLifecycle();
            probeJsonRpcFixture();
            probeSqlite();
            probeSkillRepository(workspace);
            probeHarness(workspace);
            probeProviderSpi();
            probeTlsHandshake();
            probeMcpStdio(mcpScript, mcpInterpreter);
            probeSubprocess();
            System.out.println("JA_NATIVE_PROBE=OK");
        } finally {
            deleteTree(workspace);
        }
    }

    /**
     * Starts Solon with HTTP disabled because the sidecar is a stdio process, not an HTTP server;
     * this proves the lifecycle kernel can be composed without binding a port.
     */
    private static void probeSolonLifecycle() {
        Solon.start(NativeProbe.class, new String[0], app -> app.enableHttp(false));
        try {
            if (Solon.app() == null || Solon.app().enableHttp()) {
                throw new IllegalStateException("Solon HTTP-only lifecycle unexpectedly enabled");
            }
            System.out.println("solon=ok");
        } finally {
            // Explicitly close Solon so a sidecar can release lifecycle resources before the next
            // request or probe phase starts; relying on process exit hides shutdown regressions.
            Solon.stopBlock(false, 0);
        }
    }

    /**
     * Decodes and re-encodes the versioned JSON-RPC shape through Jackson so native resource
     * registration is exercised by the actual protocol fixture rather than a string comparison.
     */
    private static void probeJsonRpcFixture() throws IOException {
        JsonNode request = JSON.readTree("{\"jsonrpc\":\"2.0\",\"id\":\"c:native-probe\",\"method\":\"turn/start\",\"params\":{\"threadId\":\"t-1\"}}");
        String encoded = JSON.writeValueAsString(request);
        JsonNode roundTrip = JSON.readTree(encoded);
        if (!"c:native-probe".equals(roundTrip.path("id").asText())
                || !"turn/start".equals(roundTrip.path("method").asText())
                || !"t-1".equals(roundTrip.path("params").path("threadId").asText())) {
            throw new IllegalStateException("JSON-RPC fixture round-trip failed");
        }
        System.out.println("jsonrpc=ok");
    }

    /**
     * Opens a real in-memory SQLite database and runs a transaction so the Xerial driver/native
     * loader is part of the closure; DriverManager alone would not prove the driver was loaded.
     */
    private static void probeSqlite() throws Exception {
        Class.forName("org.sqlite.JDBC");
        try (Connection connection = DriverManager.getConnection("jdbc:sqlite::memory:")) {
            connection.setAutoCommit(false);
            try (Statement statement = connection.createStatement()) {
                statement.execute("create table probe(value text not null)");
                statement.execute("insert into probe(value) values ('sqlite-ok')");
                connection.commit();
                try (ResultSet result = statement.executeQuery("select value from probe")) {
                    if (!result.next() || !"sqlite-ok".equals(result.getString(1))) {
                        throw new IllegalStateException("SQLite query result mismatch");
                    }
                }
            }
        }
        System.out.println("sqlite=ok");
    }

    /**
     * Loads a real SKILL.md through AgentScope's workspace repository; this guards the product's
     * skill contract while keeping the fixture in a temporary directory owned by the probe.
     */
    private static void probeSkillRepository(Path workspace) throws Exception {
        Path skillFile = workspace.resolve("skills/probe-skill/SKILL.md");
        Files.createDirectories(skillFile.getParent());
        Files.writeString(
                skillFile,
                "---\nname: probe-skill\ndescription: Native reachability fixture\n---\nUse this only for the native probe.\n",
                StandardCharsets.UTF_8);
        WorkspaceSkillRepository repository =
                new WorkspaceSkillRepository(new LocalFilesystem(workspace), "skills", RuntimeContext::empty);
        List<AgentSkill> skills = repository.getAllSkills();
        if (skills.size() != 1 || !"probe-skill".equals(skills.get(0).getName())) {
            throw new IllegalStateException("SKILL.md was not discovered");
        }
        System.out.println("skill=ok");
    }

    /**
     * Builds and runs an AgentScope Harness with a deterministic model; a recorded model is used so
     * the complete middleware/tool/state composition is exercised without an API key or network.
     */
    private static void probeHarness(Path workspace) {
        try (HarnessAgent agent =
                HarnessAgent.builder()
                        .name("ja-native-probe")
                        .description("deterministic native closure probe")
                        .sysPrompt("Return the recorded response unchanged.")
                        .model(new RecordedModel())
                        .stateStore(new InMemoryAgentStateStore())
                        .workspace(workspace)
                        .build()) {
            Msg response = agent.call("native-probe", RuntimeContext.builder().sessionId("probe").build()).block();
            if (response == null || !response.getTextContent().contains("harness-ok")) {
                throw new IllegalStateException("Harness response mismatch");
            }
        }
        System.out.println("harness=ok");
    }

    /**
     * Resolves both provider SPIs and constructs their HTTP model adapters with a loopback base URL;
     * no stream method is called, so provider construction is validated without transmitting a key.
     */
    private static void probeProviderSpi() {
        ModelRegistry.reloadProviders();
        ModelCreationContext context = ModelCreationContext.builder().stream(false).build();
        if (!ModelRegistry.canResolve("openai:probe-model", context)
                || !ModelRegistry.canResolve("anthropic:probe-model", context)) {
            throw new IllegalStateException("Provider SPI discovery failed");
        }
        OpenAIChatModel openAi =
                OpenAIChatModel.builder()
                        .modelName("probe-model")
                        .baseUrl("https://127.0.0.1:9")
                        .stream(false)
                        .build();
        AnthropicChatModel anthropic =
                AnthropicChatModel.builder()
                        .modelName("probe-model")
                        .baseUrl("https://127.0.0.1:9")
                        .stream(false)
                        .build();
        if (openAi == null || anthropic == null) {
            throw new IllegalStateException("Provider adapter construction failed");
        }
        System.out.println("providers=ok");
    }

    /**
     * Performs a real local JSSE handshake on an ephemeral loopback socket. A deterministic
     * fixture identity keeps TLS reachability in the native closure; the client truststore contains
     * only the extracted probe certificate so the handshake exercises real PKIX verification on a
     * non-routable endpoint without depending on a user or operating-system truststore.
     */
    private static void probeTlsHandshake() throws Exception {
        KeyStore keyStore = KeyStore.getInstance("PKCS12");
        keyStore.load(
                new ByteArrayInputStream(
                        Base64.getDecoder().decode(PROBE_KEY_STORE_BASE64)),
                PROBE_KEY_STORE_PASSWORD);
        KeyManagerFactory keyManagers =
                KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        keyManagers.init(keyStore, PROBE_KEY_STORE_PASSWORD);
        SSLContext serverContext = SSLContext.getInstance("TLS");
        serverContext.init(keyManagers.getKeyManagers(), null, new SecureRandom());

        KeyStore trustStore = KeyStore.getInstance(KeyStore.getDefaultType());
        trustStore.load(null, null);
        trustStore.setCertificateEntry("probe", keyStore.getCertificate("probe"));
        TrustManagerFactory trustManagers =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        trustManagers.init(trustStore);
        SSLContext clientContext = SSLContext.getInstance("TLS");
        clientContext.init(null, trustManagers.getTrustManagers(), new SecureRandom());

        try (SSLServerSocket server =
                        (SSLServerSocket)
                                serverContext
                                        .getServerSocketFactory()
                                        .createServerSocket(
                                                0, 1, InetAddress.getLoopbackAddress());
                ExecutorService executor = Executors.newSingleThreadExecutor()) {
            var serverResult =
                    executor.submit(
                            () -> {
                                try (SSLSocket socket = (SSLSocket) server.accept()) {
                                    socket.setSoTimeout(10_000);
                                    socket.startHandshake();
                                    int firstByte = socket.getInputStream().read();
                                    OutputStream output = socket.getOutputStream();
                                    output.write("tls-ok".getBytes(StandardCharsets.US_ASCII));
                                    output.flush();
                                    return firstByte == 'p';
                                }
                            });
            byte[] response;
            try (SSLSocket client = (SSLSocket) clientContext.getSocketFactory().createSocket()) {
                client.connect(
                        new InetSocketAddress(
                                InetAddress.getLoopbackAddress(), server.getLocalPort()),
                        10_000);
                client.startHandshake();
                client.getOutputStream().write('p');
                client.getOutputStream().flush();
                response = client.getInputStream().readNBytes(6);
            }
            try {
                if (!"tls-ok".equals(new String(response, StandardCharsets.US_ASCII))
                        || !Boolean.TRUE.equals(serverResult.get(10, TimeUnit.SECONDS))) {
                    throw new IllegalStateException("local TLS handshake response mismatch");
                }
            } finally {
                if (!serverResult.isDone()) {
                    serverResult.cancel(true);
                }
            }
        }
        System.out.println("tls=ok");
    }

    /**
     * Performs the real MCP initialize/list/call sequence against the supplied local fixture; the
     * explicit fixture argument keeps remote URLs and secrets out of both tests and native output.
     */
    private static void probeMcpStdio(Path script, Path interpreter) {
        McpClientWrapper client =
                McpClientBuilder.create("ja-native-probe")
                        .stdioTransport(mcpCommand(interpreter), mcpArguments(script), Map.of())
                        .timeout(Duration.ofSeconds(10))
                        .initializationTimeout(Duration.ofSeconds(10))
                        .buildSync();
        try {
            client.initialize().block(Duration.ofSeconds(10));
            List<McpSchema.Tool> tools = client.listTools().block(Duration.ofSeconds(10));
            if (tools == null || tools.stream().noneMatch(tool -> "probe_echo".equals(tool.name()))) {
                throw new IllegalStateException("MCP tools/list did not return probe_echo");
            }
            McpSchema.CallToolResult result =
                    client.callTool("probe_echo", Map.of("text", "ok")).block(Duration.ofSeconds(10));
            boolean returnedProbeText =
                    result != null
                            && result.content().stream()
                                    .anyMatch(
                                            content ->
                                                    content instanceof McpSchema.TextContent text
                                                            && "probe:ok".equals(text.text()));
            if (result == null || result.isError() || !returnedProbeText) {
                throw new IllegalStateException("MCP tools/call failed");
            }
        } finally {
            client.close();
        }
        System.out.println("mcp=ok");
    }

    /**
     * Starts a harmless process and waits for its exit to prove the sidecar can own child-process
     * lifecycle without leaving a child behind; no shell input is derived from user data.
     */
    private static void probeSubprocess() throws Exception {
        List<String> command =
                isWindows()
                        ? List.of("cmd.exe", "/d", "/c", "exit", "0")
                        : List.of("/bin/sh", "-c", "exit 0");
        Process process = new ProcessBuilder(command).start();
        if (!process.waitFor(10, TimeUnit.SECONDS) || process.exitValue() != 0) {
            process.destroyForcibly();
            throw new IllegalStateException("subprocess did not exit cleanly");
        }
        System.out.println("subprocess=ok");
    }

    /**
     * Returns the caller-validated interpreter as the command so MCP cannot resolve a different
     * executable through PATH after the probe has started.
     */
    private static String mcpCommand(Path interpreter) {
        return interpreter.toString();
    }

    /**
     * Passes only the explicit fixture path so a test cannot accidentally inherit shell switches or
     * a user-configured command; this also keeps the child contract portable across architectures.
     */
    private static List<String> mcpArguments(Path script) {
        return List.of(script.toString());
    }

    /**
     * Requires a regular fixture file because silently accepting a missing script would make an
     * incorrectly wired sidecar look like a successful MCP closure.
     */
    private static Path requiredFileArgument(String[] args, String option) throws IOException {
        String value = argumentValue(args, option);
        if (value == null) {
            throw new IllegalArgumentException(option + " is required");
        }
        Path path = Path.of(value).toAbsolutePath().normalize();
        Path resolved = path.toRealPath();
        if (!Files.isRegularFile(resolved)) {
            throw new IllegalArgumentException(option + " must be a regular file");
        }
        return resolved;
    }

    /**
     * Resolves and validates the exact interpreter before any child is spawned, preventing PATH
     * shims, directories, or non-executable files from changing the MCP process boundary.
     */
    private static Path requiredExecutableArgument(String[] args, String option) throws IOException {
        String value = argumentValue(args, option);
        if (value == null) {
            throw new IllegalArgumentException(option + " is required");
        }
        Path path = Path.of(value);
        if (!path.isAbsolute()) {
            throw new IllegalArgumentException(option + " must be an absolute path");
        }
        Path resolved = path.toRealPath();
        if (!Files.isRegularFile(resolved) || !Files.isExecutable(resolved)) {
            throw new IllegalArgumentException(option + " must point to an executable file");
        }
        return resolved;
    }

    /**
     * Keeps the harmless child-process check valid on both development Windows and native macOS;
     * using an OS-owned shell avoids adding another runtime fixture dependency.
     */
    private static boolean isWindows() {
        return System.getProperty("os.name", "").toLowerCase(Locale.ROOT).contains("win");
    }

    /**
     * Reads the raw option value before path normalization so absolute-path requirements cannot be
     * bypassed by converting a relative interpreter path into an absolute working-directory path.
     */
    private static String argumentValue(String[] args, String option) {
        for (int index = 0; index + 1 < args.length; index++) {
            if (option.equals(args[index])) {
                return args[index + 1];
            }
        }
        return null;
    }

    /**
     * Removes only the probe-owned temporary workspace after all adapters have released resources;
     * this prevents stale sqlite files or skill fixtures from contaminating repeat runs.
     */
    private static void deleteTree(Path root) throws IOException {
        if (root == null || !Files.exists(root)) {
            return;
        }
        try (var paths = Files.walk(root)) {
            paths.sorted((left, right) -> right.compareTo(left)).forEach(path -> {
                try {
                    Files.deleteIfExists(path);
                } catch (IOException error) {
                    throw new RuntimeException(error);
                }
            });
        }
    }

    /**
     * Supplies a deterministic AgentScope Model implementation so Harness middleware is tested
     * without accidentally creating a paid or authenticated provider request.
     */
    private static final class RecordedModel implements Model {
        @Override
        public Flux<ChatResponse> stream(
                List<Msg> messages, List<ToolSchema> tools, GenerateOptions options) {
            return Flux.just(
                    new ChatResponse(
                            "native-probe-response",
                            List.of(TextBlock.builder().text("harness-ok").build()),
                            null,
                            Map.of(),
                            "stop"));
        }

        @Override
        public String getModelName() {
            return "recorded-native-probe";
        }
    }
}
