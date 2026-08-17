/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

import io.agentscope.core.Version;
import io.agentscope.core.tool.Toolkit;
import io.agentscope.core.tool.mcp.McpClientBuilder;
import io.agentscope.core.tool.mcp.McpClientWrapper;
import io.agentscope.core.tool.mcp.McpSyncClientWrapper;
import io.agentscope.harness.agent.tools.McpServerConfig;
import io.modelcontextprotocol.client.McpClient;
import io.modelcontextprotocol.client.McpSyncClient;
import io.modelcontextprotocol.spec.McpSchema;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

/**
 * Owns only MCP client lifecycle and status. AgentScope's Toolkit remains the
 * tool registry and invocation path; this class does not mirror its tools.
 */
public final class McpRuntime implements AutoCloseable {
    private static final Duration CLOSE_TIMEOUT = Duration.ofSeconds(3);

    /** Resolves a static secret reference at configuration composition time. */
    @FunctionalInterface
    public interface SecretResolver {
        String resolve(String reference);
    }

    /** Minimal status set needed by settings and desktop diagnostics. */
    public enum State {
        DISCONNECTED,
        CONNECTING,
        READY,
        FAILED,
        CLOSED
    }

    /** Stable state snapshot with no provider exception text. */
    public record ServerStatus(String server, State state, String error) {
    }

    private final SecretResolver secretResolver;
    private final McpProcessPort processPort;
    private final Toolkit toolkit;
    private final McpLimits limits;
    private final ConcurrentMap<String, McpClientWrapper> clients = new ConcurrentHashMap<>();
    private final ConcurrentMap<String, ServerStatus> statuses = new ConcurrentHashMap<>();
    private volatile boolean closed;

    /** Requires explicit composition dependencies so MCP cannot bypass process policy. */
    public McpRuntime(
            SecretResolver secretResolver,
            McpProcessPort processPort,
            Toolkit toolkit,
            McpLimits limits) {
        this.secretResolver = Objects.requireNonNull(secretResolver, "secretResolver");
        this.processPort = Objects.requireNonNull(processPort, "processPort");
        this.toolkit = Objects.requireNonNull(toolkit, "toolkit");
        this.limits = Objects.requireNonNull(limits, "limits");
    }

    /** Exposes the upstream Toolkit so Harness owns MCP tool discovery and calls. */
    public Toolkit toolkit() {
        return toolkit;
    }

    /**
     * Builds one upstream MCP wrapper and registers it through Toolkit's
     * official registration API; no JA registry or tool descriptor copy exists.
     */
    public ServerStatus connect(String name, McpServerConfig config) {
        if (name == null || name.isBlank()) {
            return fail(name, "mcp_server_name_invalid");
        }
        if (closed) {
            return fail(name, "mcp_runtime_closed");
        }
        ServerStatus connecting = new ServerStatus(name, State.CONNECTING, null);
        statuses.put(name, connecting);
        McpClientWrapper wrapper = null;
        try {
            McpServerConfig resolved = McpConfigSupport.resolve(name, config, secretResolver, limits);
            wrapper = new McpResultBoundedWrapper(buildWrapper(name, resolved), limits);
            removeRegistered(name);
            clients.remove(name);
            Toolkit.ToolRegistration registration = toolkit.registration().mcpClient(wrapper);
            if (resolved.getEnableTools() != null && !resolved.getEnableTools().isEmpty()) {
                registration.enableTools(resolved.getEnableTools());
            }
            registration.apply();
            clients.put(name, wrapper);
            ServerStatus ready = new ServerStatus(name, State.READY, null);
            statuses.put(name, ready);
            return ready;
        } catch (Exception failure) {
            if (wrapper != null) {
                closeWrapper(wrapper);
            }
            return fail(name, McpConfigSupport.stableFailure(failure));
        }
    }

    /** Removes one upstream registration so its official wrapper closes pipes/HTTP streams. */
    public ServerStatus disconnect(String name) {
        if (name == null || name.isBlank()) {
            return new ServerStatus(name, State.DISCONNECTED, "mcp_server_name_invalid");
        }
        try {
            removeRegistered(name);
            clients.remove(name);
            ServerStatus result = new ServerStatus(name, closed ? State.CLOSED : State.DISCONNECTED, null);
            statuses.put(name, result);
            return result;
        } catch (Exception failure) {
            return fail(name, McpConfigSupport.stableFailure(failure));
        }
    }

    /** Returns a stable state snapshot without exposing an MCP exception. */
    public ServerStatus status(String name) {
        if (name == null || name.isBlank()) {
            return new ServerStatus(name, State.DISCONNECTED, "mcp_server_name_invalid");
        }
        return statuses.getOrDefault(name, new ServerStatus(name, State.DISCONNECTED, null));
    }

    /** Closes all registrations through Toolkit so AgentScope remains the owner of cleanup. */
    @Override
    public void close() {
        if (closed) {
            return;
        }
        closed = true;
        List<String> names = new ArrayList<>(clients.keySet());
        for (String name : names) {
            disconnect(name);
        }
        clients.clear();
    }

    /** Selects the official AgentScope builder for HTTP and SDK transport for controlled stdio. */
    private McpClientWrapper buildWrapper(String name, McpServerConfig config) {
        Duration timeout = config.getTimeout();
        Duration initializationTimeout = config.getInitializationTimeout();
        if ("stdio".equalsIgnoreCase(config.getTransport())) {
            McpProcessTransport transport = new McpProcessTransport(
                    config.getCommand(), config.getArgs(), config.getEnv(), processPort);
            McpSyncClient client = McpClient.sync(transport)
                    .requestTimeout(timeout)
                    .initializationTimeout(initializationTimeout)
                    .clientInfo(new McpSchema.Implementation("ja", "JA MCP Tools", Version.VERSION))
                    .capabilities(McpSchema.ClientCapabilities.builder().build())
                    .build();
            return new McpSyncClientWrapper(name, client);
        }
        McpClientBuilder builder = McpClientBuilder.create(name)
                .streamableHttpTransport(config.getUrl())
                .headers(config.getHeaders())
                .queryParams(config.getQueryParams())
                .timeout(timeout)
                .initializationTimeout(initializationTimeout)
                .protocolVersions("2024-11-05", "2025-03-26", "2025-06-18");
        return builder.buildSync();
    }

    /** Removes an existing Toolkit registration before a replacement can collide. */
    private void removeRegistered(String name) {
        toolkit.removeMcpClient(name).block(CLOSE_TIMEOUT);
    }

    /** Provides a defensive close when registration failed before Toolkit retained the wrapper. */
    private void closeWrapper(McpClientWrapper wrapper) {
        try {
            wrapper.close();
        } catch (RuntimeException ignored) {
            // The stable failure is reported by the original operation; provider text is not retained.
        }
    }

    /** Stores one stable failure state without copying a provider message. */
    private ServerStatus fail(String name, String error) {
        ServerStatus result = new ServerStatus(name, State.FAILED, error);
        if (name != null && !name.isBlank()) {
            statuses.put(name, result);
        }
        return result;
    }
}
