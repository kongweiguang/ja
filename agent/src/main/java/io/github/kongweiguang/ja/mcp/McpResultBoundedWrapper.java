/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

import io.agentscope.core.tool.mcp.McpClientWrapper;
import io.modelcontextprotocol.spec.McpSchema;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import reactor.core.publisher.Mono;

/**
 * Thin final-boundary adapter. AgentScope still owns MCP lifecycle and tool
 * invocation; JA only bounds metadata and the result entering Harness state.
 */
final class McpResultBoundedWrapper extends McpClientWrapper {
    private static final int ALIAS_HASH_LENGTH = 16;
    private static final int ALIAS_MAX_LENGTH = 64;
    private final McpClientWrapper delegate;
    private final McpLimits limits;
    private final String namespace;
    private final boolean aliasTools;
    private volatile Map<String, String> rawByAlias = Map.of();
    private volatile List<McpSchema.Tool> frozenTools;

    /**
     * Wraps one official client without reimplementing registration or transport.
     * The default constructor keeps the probe path raw; only Toolkit registration
     * uses the provider-safe alias mode below.
     */
    McpResultBoundedWrapper(McpClientWrapper delegate, McpLimits limits) {
        this(delegate, limits, delegate.getName(), false);
    }

    /**
     * Selects the one missing upstream boundary: aliases are needed only because
     * AgentScope 2.0.2 keys MCP tools globally by raw name.  Probe callers keep
     * raw names so the wire display id remains {@code mcp:<revision>/<raw>}.
     */
    McpResultBoundedWrapper(McpClientWrapper delegate, McpLimits limits,
                            String namespace, boolean aliasTools) {
        super(Objects.requireNonNull(delegate, "delegate").getName());
        this.delegate = delegate;
        this.limits = Objects.requireNonNull(limits, "limits");
        this.namespace = requireNamespace(namespace);
        this.aliasTools = aliasTools;
    }

    /** Delegates initialization so SDK protocol negotiation remains authoritative. */
    @Override
    public Mono<Void> initialize() {
        return delegate.initialize();
    }

    /**
     * Validates the provider list once and freezes the alias mapping before
     * Toolkit creates AgentScope MCP tools; a later provider rename cannot
     * silently route a model call to a different raw tool.
     */
    @Override
    public Mono<List<McpSchema.Tool>> listTools() {
        List<McpSchema.Tool> cached = frozenTools;
        if (aliasTools && cached != null) {
            return Mono.just(cached);
        }
        return Mono.defer(() -> delegate.listTools()
                .map(tools -> McpConfigSupport.validateTools(tools, limits))
                .map(this::freezeOrKeepRaw));
    }

    /**
     * Returns the frozen alias set for collision checks against the same
     * AgentScope Toolkit; no second JA registry is introduced.
     */
    List<String> aliases() {
        return List.copyOf(rawByAlias.keySet());
    }

    /** Maps the wire allowlist's raw name to the frozen provider alias. */
    String providerName(String rawName) {
        if (!aliasTools) {
            return rawName;
        }
        return rawByAlias.entrySet().stream()
                .filter(entry -> entry.getValue().equals(rawName))
                .map(Map.Entry::getKey)
                .findFirst()
                .orElseThrow(() -> new IllegalArgumentException("mcp_tool_name_invalid"));
    }

    /**
     * Converts a validated raw list to provider-safe names or preserves it for
     * read-only probes, while rejecting duplicate aliases before registration.
     */
    private List<McpSchema.Tool> freezeOrKeepRaw(List<McpSchema.Tool> tools) {
        List<McpSchema.Tool> forced = forceAsk(tools);
        if (!aliasTools) {
            return forced;
        }
        LinkedHashMap<String, String> nextRawByAlias = new LinkedHashMap<>();
        List<McpSchema.Tool> projected = new java.util.ArrayList<>(forced.size());
        for (McpSchema.Tool tool : forced) {
            String alias = alias(namespace, tool.name());
            String previous = nextRawByAlias.put(alias, tool.name());
            if (previous != null) {
                throw new IllegalArgumentException("mcp_tool_alias_collision");
            }
            projected.add(rename(tool, alias));
        }
        Map<String, String> immutable = Map.copyOf(nextRawByAlias);
        Map<String, String> previous = rawByAlias;
        if (!previous.isEmpty() && !previous.equals(immutable)) {
            throw new IllegalArgumentException("mcp_tool_alias_set_changed");
        }
        rawByAlias = immutable;
        List<McpSchema.Tool> immutableTools = List.copyOf(projected);
        frozenTools = immutableTools;
        return immutableTools;
    }

    /** Rebuilds only the name field so schemas and provider annotations stay intact. */
    private static McpSchema.Tool rename(McpSchema.Tool tool, String alias) {
        return new McpSchema.Tool(alias, tool.title(), tool.description(), tool.inputSchema(),
                tool.outputSchema(), tool.annotations(), tool.meta());
    }

    /**
     * Clears only the untrusted read-only hint so AgentScope's native
     * McpTool.checkPermissions path remains the single ASK/HITL authority.
     */
    private static List<McpSchema.Tool> forceAsk(List<McpSchema.Tool> tools) {
        return tools.stream().map(tool -> {
            McpSchema.ToolAnnotations annotations = tool.annotations();
            if (annotations == null || !Boolean.TRUE.equals(annotations.readOnlyHint())) {
                return tool;
            }
            McpSchema.ToolAnnotations forced = new McpSchema.ToolAnnotations(
                    annotations.title(), false, annotations.destructiveHint(),
                    annotations.idempotentHint(), annotations.openWorldHint(),
                    annotations.returnDirect());
            return new McpSchema.Tool(tool.name(), tool.title(), tool.description(),
                    tool.inputSchema(), tool.outputSchema(), forced, tool.meta());
        }).toList();
    }

    /** Bounds provider output while preserving Reactor cancellation semantics. */
    @Override
    public Mono<McpSchema.CallToolResult> callTool(
            String toolName, Map<String, Object> arguments) {
        return Mono.defer(() -> delegate.callTool(resolveRaw(toolName), arguments)
                .map(result -> McpConfigSupport.validateResult(result, limits)));
    }

    /** Preserves MCP metadata calls without creating a second protocol path. */
    @Override
    public Mono<McpSchema.CallToolResult> callTool(
            String toolName, Map<String, Object> arguments, Map<String, Object> meta) {
        return Mono.defer(() -> delegate.callTool(resolveRaw(toolName), arguments, meta)
                .map(result -> McpConfigSupport.validateResult(result, limits)));
    }

    /** Rejects an unknown model-visible alias instead of forwarding an arbitrary raw name. */
    private String resolveRaw(String providerName) {
        if (!aliasTools) {
            return providerName;
        }
        String raw = rawByAlias.get(providerName);
        if (raw == null) {
            throw new IllegalArgumentException("mcp_tool_alias_unknown");
        }
        return raw;
    }

    /** Builds a deterministic readable prefix plus hash within provider limits. */
    private static String alias(String namespace, String rawName) {
        String readable = "ja_" + safe(namespace, 18) + "_" + safe(rawName, 20);
        String hash = sha256(namespace + "\u0000" + rawName).substring(0, ALIAS_HASH_LENGTH);
        String candidate = readable + "_" + hash;
        if (candidate.length() <= ALIAS_MAX_LENGTH) {
            return candidate;
        }
        int readableLength = ALIAS_MAX_LENGTH - 1 - ALIAS_HASH_LENGTH;
        return candidate.substring(0, readableLength) + "_" + hash;
    }

    /** Keeps the human-readable part legal without allowing provider punctuation. */
    private static String safe(String value, int maxLength) {
        String normalized = value.replaceAll("[^A-Za-z0-9_-]", "_");
        if (normalized.isEmpty() || !Character.isLetterOrDigit(normalized.charAt(0))) {
            normalized = "x_" + normalized;
        }
        return normalized.substring(0, Math.min(maxLength, normalized.length()));
    }

    /** Computes a stable namespace-qualified suffix without retaining provider text in errors. */
    private static String sha256(String value) {
        try {
            byte[] digest = MessageDigest.getInstance("SHA-256")
                    .digest(value.getBytes(StandardCharsets.UTF_8));
            return java.util.HexFormat.of().formatHex(digest);
        } catch (NoSuchAlgorithmException impossible) {
            throw new IllegalStateException("sha256_unavailable", impossible);
        }
    }

    /** Validates the namespace once because it participates in every provider alias. */
    private static String requireNamespace(String value) {
        if (value == null || value.isBlank() || value.indexOf('\0') >= 0) {
            throw new IllegalArgumentException("mcp_namespace_invalid");
        }
        return value;
    }

    /** Closes the official wrapper so its HTTP stream or process ownership is retained upstream. */
    @Override
    public void close() {
        delegate.close();
    }
}
