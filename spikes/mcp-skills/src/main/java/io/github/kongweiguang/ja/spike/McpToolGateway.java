/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.spike;

import io.agentscope.core.Version;
import io.agentscope.core.tool.mcp.McpClientBuilder;
import io.agentscope.core.tool.mcp.McpClientWrapper;
import io.agentscope.core.tool.mcp.McpTool;
import io.agentscope.core.tool.mcp.McpSyncClientWrapper;
import io.modelcontextprotocol.client.McpClient;
import io.modelcontextprotocol.client.McpSyncClient;
import io.modelcontextprotocol.spec.McpSchema;
import java.net.URI;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import reactor.core.publisher.Mono;

/**
 * Product adapter around AgentScope's real MCP wrapper.
 *
 * <p>The adapter intentionally does not reimplement JSON-RPC or transports. It applies JA's
 * security policy at the point where a server becomes a tool provider, so a server's read-only
 * hint cannot silently bypass an approval request and a duplicate call cannot replay a side effect.
 */
public final class McpToolGateway implements AutoCloseable {
    private static final Pattern SECRET_REF = Pattern.compile("secret-ref://([A-Za-z0-9._-]+)");
    private static final Duration MAX_OPERATION_TIMEOUT = Duration.ofMinutes(1);
    private static final Duration MAX_RETRY_DELAY = Duration.ofSeconds(5);
    private static final int MAX_RECORDED_CALLS = 1024;
    private static final Set<String> SENSITIVE_HTTP_HEADERS =
            Set.of("authorization", "proxy-authorization", "cookie");

    /** Only the two transports that are in the first JA settings scope. */
    public enum Transport {
        STDIO,
        STREAMABLE_HTTP
    }

    /** Transport and policy configuration; secret values are represented only by references. */
    public record ServerConfig(
            String name,
            Transport transport,
            String command,
            List<String> args,
            Map<String, String> env,
            String url,
            Map<String, String> headers,
            List<String> protocolVersions,
            Duration requestTimeout,
            Duration initializationTimeout,
            Duration retryDelay,
            int maxConnectAttempts,
            String authMode,
            Set<String> requestedCapabilities) {
        public ServerConfig {
            if (name == null || name.isBlank()) {
                throw new IllegalArgumentException("mcp_server_name_required");
            }
            Objects.requireNonNull(transport, "transport");
            args = args == null ? List.of() : List.copyOf(args);
            env = env == null ? Map.of() : Map.copyOf(env);
            headers = headers == null ? Map.of() : Map.copyOf(headers);
            protocolVersions = protocolVersions == null || protocolVersions.isEmpty()
                    ? List.of("2024-11-05", "2025-03-26")
                    : List.copyOf(protocolVersions);
            requestTimeout = requestTimeout == null ? Duration.ofSeconds(5) : requestTimeout;
            initializationTimeout = initializationTimeout == null ? Duration.ofSeconds(5) : initializationTimeout;
            retryDelay = retryDelay == null ? Duration.ZERO : retryDelay;
            if (requestTimeout.isZero() || requestTimeout.isNegative()
                    || requestTimeout.compareTo(MAX_OPERATION_TIMEOUT) > 0
                    || initializationTimeout.isZero()
                    || initializationTimeout.isNegative()
                    || initializationTimeout.compareTo(MAX_OPERATION_TIMEOUT) > 0) {
                throw new IllegalArgumentException("mcp_timeout_limit");
            }
            if (retryDelay.isNegative() || retryDelay.compareTo(MAX_RETRY_DELAY) > 0) {
                throw new IllegalArgumentException("mcp_retry_delay_limit");
            }
            if (maxConnectAttempts < 1 || maxConnectAttempts > 3) {
                throw new IllegalArgumentException("mcp_connect_attempts_limit");
            }
            authMode = authMode == null ? "none" : authMode;
            requestedCapabilities = requestedCapabilities == null
                    ? Set.of()
                    : Set.copyOf(requestedCapabilities);
        }

        /** Creates a local server configuration without putting secrets in process arguments. */
        public static ServerConfig stdio(String name, String command, List<String> args) {
            return new ServerConfig(
                    name,
                    Transport.STDIO,
                    command,
                    args,
                    Map.of(),
                    null,
                    Map.of(),
                    null,
                    null,
                    null,
                    null,
                    1,
                    "none",
                    Set.of());
        }

        /** Creates a Streamable HTTP configuration whose URL is kept secret-free by validation. */
        public static ServerConfig streamableHttp(String name, String url) {
            return new ServerConfig(
                    name,
                    Transport.STREAMABLE_HTTP,
                    null,
                    List.of(),
                    Map.of(),
                    url,
                    Map.of(),
                    null,
                    null,
                    null,
                    null,
                    1,
                    "none",
                    Set.of());
        }
    }

    /** Resolves a secret reference without exposing the resolved value to this adapter's logs. */
    @FunctionalInterface
    public interface SecretResolver {
        String resolve(String reference);
    }

    /** Normalized tool descriptor used by the UI and approval layer. */
    public record ToolDescriptor(
            String server,
            String name,
            String description,
            Map<String, Object> inputSchema) {}

    /** The decision/result shape keeps approval separate from transport invocation. */
    public record CallOutcome(Status status, McpSchema.CallToolResult result, boolean invoked, String error) {}

    /** Call outcomes distinguish a policy pause from a transport failure. */
    public enum Status {
        ASK,
        COMPLETED,
        DUPLICATE,
        FAILED
    }

    private final SecretResolver secretResolver;
    private final ConcurrentMap<String, Session> sessions = new ConcurrentHashMap<>();
    private final ConcurrentMap<String, CompletableFuture<McpSchema.CallToolResult>> calls =
            new ConcurrentHashMap<>();
    private final java.util.concurrent.ConcurrentLinkedDeque<String> callOrder =
            new java.util.concurrent.ConcurrentLinkedDeque<>();

    /** Uses a resolver that rejects every secret reference by default. */
    public McpToolGateway() {
        this(reference -> {
            throw new IllegalArgumentException("secret_ref_unresolved");
        });
    }

    /** Injects the product secret store while keeping values out of the configuration model. */
    public McpToolGateway(SecretResolver secretResolver) {
        this.secretResolver = Objects.requireNonNull(secretResolver, "secretResolver");
    }

    /**
     * Connects with bounded initialization retries; a failed attempt is closed before the next
     * attempt so a crashed stdio child or HTTP session cannot leak a process or connection.
     */
    public synchronized List<ToolDescriptor> connect(ServerConfig config) {
        validateUnsupported(config);
        validateSecretPlacement(config);
        validateHttpTransport(config);
        Session previous = sessions.remove(config.name());
        if (previous != null) {
            previous.wrapper().close();
        }

        Throwable last = null;
        for (int attempt = 1; attempt <= config.maxConnectAttempts(); attempt++) {
            McpClientWrapper wrapper = null;
            try {
                wrapper = buildClient(config);
                wrapper.initialize().block(config.initializationTimeout().plusSeconds(1));
                List<McpSchema.Tool> tools = wrapper.listTools().block(config.requestTimeout());
                List<ToolDescriptor> descriptors = validateTools(config.name(), tools);
                sessions.put(config.name(), new Session(wrapper, descriptors, config.requestTimeout()));
                return descriptors;
            } catch (Throwable failure) {
                last = failure;
                if (wrapper != null) {
                    wrapper.close();
                }
                if (attempt < config.maxConnectAttempts() && !config.retryDelay().isZero()) {
                    Mono.delay(config.retryDelay()).block(config.retryDelay().plusSeconds(1));
                }
            }
        }
        throw new IllegalStateException("mcp_connect_failed: " + config.name(), last);
    }

    /**
     * Returns ASK before any network/process call; even a server-declared read-only tool remains
     * behind this explicit product decision boundary.
     */
    public CallOutcome call(
            String server,
            String callId,
            String tool,
            Map<String, Object> arguments,
            boolean approved) {
        if (!approved) {
            return new CallOutcome(Status.ASK, null, false, "approval_required");
        }
        if (callId == null || callId.isBlank()) {
            return new CallOutcome(Status.FAILED, null, false, "call_id_required");
        }
        Session session = sessions.get(server);
        if (session == null) {
            return new CallOutcome(Status.FAILED, null, false, "mcp_server_not_connected");
        }
        if (session.tools().stream().noneMatch(item -> item.name().equals(tool))) {
            return new CallOutcome(Status.FAILED, null, false, "mcp_tool_not_found");
        }

        String key = server + "\u0000" + callId;
        CompletableFuture<McpSchema.CallToolResult> created = new CompletableFuture<>();
        CompletableFuture<McpSchema.CallToolResult> existing = calls.putIfAbsent(key, created);
        if (existing != null) {
            try {
                return new CallOutcome(Status.DUPLICATE, existing.get(6, TimeUnit.SECONDS), false, null);
            } catch (Exception exception) {
                return new CallOutcome(Status.DUPLICATE, null, false, "duplicate_failed");
            }
        }

        try {
            McpSchema.CallToolResult result =
                    session.wrapper().callTool(tool, arguments == null ? Map.of() : arguments)
                            .block(session.requestTimeout());
            validateResult(result);
            created.complete(result);
            recordCall(key);
            return new CallOutcome(Status.COMPLETED, result, true, null);
        } catch (Throwable failure) {
            created.completeExceptionally(failure);
            recordCall(key);
            return new CallOutcome(Status.FAILED, null, true, stableFailure(failure));
        }
    }

    /** Converts the validated descriptor to AgentScope's real McpTool for Toolkit registration. */
    public McpTool asAgentScopeTool(String server, String toolName) {
        Session session = requireSession(server);
        McpSchema.Tool tool = session.wrapper().getCachedTool(toolName);
        if (tool == null) {
            throw new IllegalArgumentException("mcp_tool_not_found: " + toolName);
        }
        Map<String, Object> schema = McpTool.convertMcpSchemaToParameters(tool.inputSchema(), Set.of());
        return new McpTool(
                tool.name(),
                tool.description(),
                schema,
                null,
                session.wrapper(),
                null,
                server,
                false);
    }

    /** Returns a snapshot of descriptors without exposing the SDK wrapper to callers. */
    public List<ToolDescriptor> tools(String server) {
        return List.copyOf(requireSession(server).tools());
    }

    /** Closes every AgentScope wrapper; duplicate-call records are local state only. */
    @Override
    public synchronized void close() {
        for (Session session : sessions.values()) {
            session.wrapper().close();
        }
        sessions.clear();
        calls.clear();
        callOrder.clear();
    }

    private McpClientWrapper buildClient(ServerConfig config) {
        if (config.transport() == Transport.STDIO) {
            List<String> args = config.args().stream().map(this::rejectSecretMarker).toList();
            Map<String, String> env = resolveSecrets(config.env());
            SafeStdioClientTransport transport = new SafeStdioClientTransport(
                    rejectSecretMarker(config.command()), args, env, config.protocolVersions());
            McpSyncClient client = McpClient.sync(transport)
                    .requestTimeout(config.requestTimeout())
                    .initializationTimeout(config.initializationTimeout())
                    .clientInfo(new McpSchema.Implementation("agentscope-java", "AgentScope Java Framework", Version.VERSION))
                    .capabilities(McpSchema.ClientCapabilities.builder().build())
                    .build();
            // Keep AgentScope's wrapper and tool conversion while replacing only the unsafe SDK
            // stdio process-builder default at the product security boundary.
            return new McpSyncClientWrapper(config.name(), client);
        }
        // Keep HTTP construction in AgentScope so JA inherits its protocol and reconnect behavior.
        McpClientBuilder builder = McpClientBuilder.create(config.name());
        builder.protocolVersions(config.protocolVersions().toArray(String[]::new));
        builder.timeout(config.requestTimeout());
        builder.initializationTimeout(config.initializationTimeout());
        builder.streamableHttpTransport(rejectSecretMarker(config.url()));
        for (Map.Entry<String, String> header : config.headers().entrySet()) {
            builder.header(header.getKey(), resolveValue(header.getValue()));
        }
        return builder.buildSync();
    }

    private List<ToolDescriptor> validateTools(String server, List<McpSchema.Tool> tools) {
        // Validate the model-facing schema before exposing a remote tool to approval or execution.
        if (tools == null) {
            throw new IllegalArgumentException("mcp_tools_result_missing");
        }
        List<ToolDescriptor> descriptors = new ArrayList<>();
        Set<String> names = new HashSet<>();
        for (McpSchema.Tool tool : tools) {
            if (tool == null || tool.name() == null || tool.name().isBlank() || !names.add(tool.name())) {
                throw new IllegalArgumentException("mcp_tool_schema_invalid");
            }
            if (tool.inputSchema() == null || !"object".equals(tool.inputSchema().type())) {
                throw new IllegalArgumentException("mcp_tool_schema_invalid: " + tool.name());
            }
            descriptors.add(new ToolDescriptor(
                    server,
                    tool.name(),
                    tool.description() == null ? "" : tool.description(),
                    Map.copyOf(tool.inputSchema().properties() == null ? Map.of() : tool.inputSchema().properties())));
        }
        return List.copyOf(descriptors);
    }

    private static void validateResult(McpSchema.CallToolResult result) {
        // Treat missing content as a protocol fault instead of presenting an ambiguous empty result.
        if (result == null || result.content() == null) {
            throw new IllegalArgumentException("mcp_result_invalid");
        }
        if (result.content().stream().anyMatch(Objects::isNull)) {
            throw new IllegalArgumentException("mcp_result_invalid");
        }
    }

    private void validateUnsupported(ServerConfig config) {
        // Fail closed for capabilities that the first JA UI cannot represent or approve safely.
        if (!"none".equalsIgnoreCase(config.authMode())) {
            throw new UnsupportedCapabilityException("unsupported_auth", "OAuth and remote auth are not enabled");
        }
        for (String capability : config.requestedCapabilities()) {
            String normalized = capability.toLowerCase(Locale.ROOT);
            if (Set.of("resources", "prompts", "sampling", "apps").contains(normalized)) {
                throw new UnsupportedCapabilityException(
                        "unsupported_capability", "MCP capability is not enabled: " + normalized);
            }
        }
    }

    private void validateSecretPlacement(ServerConfig config) {
        // URL and argv are durable/process-visible channels, so secret references are forbidden there.
        rejectSecretMarker(config.command());
        rejectSecretMarker(config.url());
        config.args().forEach(this::rejectSecretMarker);
    }

    private void validateHttpTransport(ServerConfig config) {
        // Validate the URL and authentication headers before AgentScope creates a client, because
        // userinfo, fragments, CRLF and literal bearer cookies can bypass the intended secret store.
        if (config.transport() != Transport.STREAMABLE_HTTP) {
            return;
        }
        String url = Objects.requireNonNull(config.url(), "mcp_url_required");
        rejectLineBreak(url, "mcp_url_invalid");
        URI parsed;
        try {
            parsed = URI.create(url);
        } catch (IllegalArgumentException exception) {
            throw new IllegalArgumentException("mcp_url_invalid", exception);
        }
        if (!("http".equalsIgnoreCase(parsed.getScheme())
                || "https".equalsIgnoreCase(parsed.getScheme()))) {
            throw new IllegalArgumentException("mcp_url_scheme_unsupported");
        }
        if (parsed.getHost() == null || parsed.getHost().isBlank()) {
            throw new IllegalArgumentException("mcp_url_host_required");
        }
        if (parsed.getRawUserInfo() != null || parsed.getRawFragment() != null) {
            throw new IllegalArgumentException("mcp_url_userinfo_or_fragment_forbidden");
        }
        for (Map.Entry<String, String> header : config.headers().entrySet()) {
            if (header.getKey() == null || header.getKey().isBlank()) {
                throw new IllegalArgumentException("mcp_header_name_required");
            }
            rejectLineBreak(header.getKey(), "mcp_header_invalid");
            rejectLineBreak(header.getValue(), "mcp_header_invalid");
            if (SENSITIVE_HTTP_HEADERS.contains(header.getKey().toLowerCase(Locale.ROOT))
                    && !SECRET_REF.matcher(header.getValue()).find()) {
                throw new IllegalArgumentException("secret_ref_required_for_sensitive_header");
            }
        }
    }

    private static void rejectLineBreak(String value, String error) {
        // Header and URL parsers differ in how they normalize CRLF, so reject it before either
        // AgentScope or the JDK HTTP client gets a chance to interpret attacker-controlled text.
        if (value != null && (value.indexOf('\r') >= 0 || value.indexOf('\n') >= 0)) {
            throw new IllegalArgumentException(error);
        }
    }

    private Map<String, String> resolveSecrets(Map<String, String> values) {
        // Resolve only at the final transport boundary and keep the configuration object reference-only.
        Map<String, String> result = new HashMap<>();
        for (Map.Entry<String, String> entry : values.entrySet()) {
            result.put(entry.getKey(), resolveValue(entry.getValue()));
        }
        return Map.copyOf(result);
    }

    private String resolveValue(String value) {
        // Substitute references in memory without ever putting the resolved value in diagnostics.
        if (value == null) {
            return null;
        }
        Matcher matcher = SECRET_REF.matcher(value);
        StringBuffer result = new StringBuffer();
        while (matcher.find()) {
            String replacement = Objects.requireNonNull(secretResolver.resolve(matcher.group(1)), "secret_ref_empty");
            matcher.appendReplacement(result, Matcher.quoteReplacement(replacement));
        }
        matcher.appendTail(result);
        return result.toString();
    }

    private String rejectSecretMarker(String value) {
        // Prevent accidental leakage through URLs and child-process command arguments.
        if (value != null && SECRET_REF.matcher(value).find()) {
            throw new IllegalArgumentException("secret_ref_must_not_be_in_url_or_argv");
        }
        return value;
    }

    private Session requireSession(String server) {
        // Centralize the disconnected-server error so callers cannot accidentally create a session.
        Session session = sessions.get(server);
        if (session == null) {
            throw new IllegalArgumentException("mcp_server_not_connected: " + server);
        }
        return session;
    }

    private static String stableFailure(Throwable failure) {
        // Collapse vendor/transport exceptions into UI-safe categories without returning secret text.
        if (failure instanceof TimeoutException
                || failure.getCause() instanceof TimeoutException) {
            return "mcp_timeout";
        }
        return "mcp_transport_failure";
    }

    /** Caps replay-protection memory while retaining completed records long enough for UI retries. */
    private void recordCall(String key) {
        callOrder.addLast(key);
        while (callOrder.size() > MAX_RECORDED_CALLS) {
            String oldest = callOrder.pollFirst();
            if (oldest != null) {
                CompletableFuture<McpSchema.CallToolResult> value = calls.get(oldest);
                if (value != null && value.isDone()) {
                    calls.remove(oldest, value);
                } else if (value != null) {
                    callOrder.addLast(oldest);
                    break;
                }
            }
        }
    }

    private record Session(
            McpClientWrapper wrapper, List<ToolDescriptor> tools, Duration requestTimeout) {}
}
