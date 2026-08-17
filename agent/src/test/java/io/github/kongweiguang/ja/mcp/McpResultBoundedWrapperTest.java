/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

import io.agentscope.core.tool.mcp.McpClientWrapper;
import io.modelcontextprotocol.spec.McpSchema;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import reactor.core.publisher.Mono;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Proves the one thin alias boundary around AgentScope's raw-name MCP manager. */
final class McpResultBoundedWrapperTest {
    /** Same raw tool on two clients receives distinct provider names and routes back exactly. */
    @Test
    void aliasesAreProviderSafeStableAndRouteToRawTool() {
        AtomicReference<String> firstRaw = new AtomicReference<>();
        AtomicReference<String> secondRaw = new AtomicReference<>();
        McpResultBoundedWrapper first = new McpResultBoundedWrapper(
                new StubClient("mcp_first", firstRaw), McpLimits.DEFAULT, "mcp_first", true);
        McpResultBoundedWrapper second = new McpResultBoundedWrapper(
                new StubClient("mcp_second", secondRaw), McpLimits.DEFAULT, "mcp_second", true);

        String firstAlias = first.listTools().block().getFirst().name();
        String secondAlias = second.listTools().block().getFirst().name();
        assertNotEquals(firstAlias, secondAlias);
        assertTrue(firstAlias.matches("[A-Za-z0-9_-]{1,64}"));
        assertTrue(secondAlias.matches("[A-Za-z0-9_-]{1,64}"));

        first.callTool(firstAlias, Map.of()).block();
        second.callTool(secondAlias, Map.of()).block();
        assertEquals("echo", firstRaw.get());
        assertEquals("echo", secondRaw.get());
        assertThrows(IllegalArgumentException.class,
                () -> first.callTool("echo", Map.of()).block());
    }

    /** Raw probe wrappers preserve the wire display name and bypass provider aliasing. */
    @Test
    void probeWrapperKeepsRawToolNames() {
        McpResultBoundedWrapper probe = new McpResultBoundedWrapper(
                new StubClient("probe", new AtomicReference<>()), McpLimits.DEFAULT);
        assertEquals("echo", probe.listTools().block().getFirst().name());
    }

    /** Minimal official-wrapper double; the transport remains outside this alias unit test. */
    private static final class StubClient extends McpClientWrapper {
        private final AtomicReference<String> called;

        private StubClient(String name, AtomicReference<String> called) {
            super(name);
            this.called = called;
        }

        @Override
        public Mono<Void> initialize() {
            initialized = true;
            return Mono.empty();
        }

        @Override
        public Mono<List<McpSchema.Tool>> listTools() {
            return Mono.just(List.of(tool("echo")));
        }

        @Override
        public Mono<McpSchema.CallToolResult> callTool(String toolName,
                                                        Map<String, Object> arguments) {
            called.set(toolName);
            return Mono.just(new McpSchema.CallToolResult("ok", false));
        }

        @Override
        public Mono<McpSchema.CallToolResult> callTool(String toolName,
                                                        Map<String, Object> arguments,
                                                        Map<String, Object> meta) {
            return callTool(toolName, arguments);
        }

        @Override
        public void close() {
        }

        /** Builds the smallest valid MCP schema accepted by JA's boundary checks. */
        private static McpSchema.Tool tool(String name) {
            McpSchema.JsonSchema schema = new McpSchema.JsonSchema(
                    "object", Map.of(), List.of(), false, Map.of(), Map.of());
            return new McpSchema.Tool(name, null, "echo", schema, Map.of(),
                    new McpSchema.ToolAnnotations(null, true, false, true, true, false), Map.of());
        }
    }
}
