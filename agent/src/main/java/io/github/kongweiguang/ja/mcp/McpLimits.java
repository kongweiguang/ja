/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

/**
 * Small immutable budget shared by the gateway and AgentScope wrapper.
 * Keeping only model-facing retention limits here avoids rebuilding MCP framing
 * and network policy that the official SDK already owns.
 */
public record McpLimits(
        int maxToolCount,
        int maxResultBytes,
        int maxDepth,
        int maxStringBytes,
        int maxCollectionEntries) {

    /** Preview defaults keep a single misbehaving server from filling the agent context. */
    public static final McpLimits DEFAULT = new McpLimits(
            128, 1 * 1024 * 1024, 32, 256 * 1024, 1_024);

    /** Rejects invalid budgets before a client or child process is created. */
    public McpLimits {
        requireRange(maxToolCount, 1, 1_024, "mcp_tool_count_limit");
        requireRange(maxResultBytes, 1_024, 16 * 1024 * 1024, "mcp_result_limit");
        requireRange(maxDepth, 1, 128, "mcp_depth_limit");
        requireRange(maxStringBytes, 256, 4 * 1024 * 1024, "mcp_string_limit");
        requireRange(maxCollectionEntries, 1, 16_384, "mcp_collection_limit");
    }

    /** Uses one validation branch so every limit has the same fail-closed semantics. */
    private static void requireRange(int value, int min, int max, String code) {
        if (value < min || value > max) {
            throw new IllegalArgumentException(code);
        }
    }
}
