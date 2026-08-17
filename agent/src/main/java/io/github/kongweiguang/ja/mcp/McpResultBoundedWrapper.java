/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

import io.agentscope.core.tool.mcp.McpClientWrapper;
import io.modelcontextprotocol.spec.McpSchema;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import reactor.core.publisher.Mono;

/**
 * Thin final-boundary adapter. AgentScope still owns MCP lifecycle and tool
 * invocation; JA only bounds metadata and the result entering Harness state.
 */
final class McpResultBoundedWrapper extends McpClientWrapper {
    private final McpClientWrapper delegate;
    private final McpLimits limits;

    /** Wraps one official client without reimplementing registration or transport. */
    McpResultBoundedWrapper(McpClientWrapper delegate, McpLimits limits) {
        super(Objects.requireNonNull(delegate, "delegate").getName());
        this.delegate = delegate;
        this.limits = Objects.requireNonNull(limits, "limits");
    }

    /** Delegates initialization so SDK protocol negotiation remains authoritative. */
    @Override
    public Mono<Void> initialize() {
        return delegate.initialize();
    }

    /** Validates only the tool list size/schema before Toolkit retains it. */
    @Override
    public Mono<List<McpSchema.Tool>> listTools() {
        return Mono.defer(() -> delegate.listTools().map(tools -> McpConfigSupport.validateTools(tools, limits)));
    }

    /** Bounds provider output while preserving Reactor cancellation semantics. */
    @Override
    public Mono<McpSchema.CallToolResult> callTool(
            String toolName, Map<String, Object> arguments) {
        return Mono.defer(() -> delegate.callTool(toolName, arguments)
                .map(result -> McpConfigSupport.validateResult(result, limits)));
    }

    /** Preserves MCP metadata calls without creating a second protocol path. */
    @Override
    public Mono<McpSchema.CallToolResult> callTool(
            String toolName, Map<String, Object> arguments, Map<String, Object> meta) {
        return Mono.defer(() -> delegate.callTool(toolName, arguments, meta)
                .map(result -> McpConfigSupport.validateResult(result, limits)));
    }

    /** Closes the official wrapper so its HTTP stream or process ownership is retained upstream. */
    @Override
    public void close() {
        delegate.close();
    }
}
