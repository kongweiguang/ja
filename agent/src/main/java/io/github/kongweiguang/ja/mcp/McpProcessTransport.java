/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

import io.modelcontextprotocol.client.transport.ServerParameters;
import io.modelcontextprotocol.client.transport.StdioClientTransport;
import io.modelcontextprotocol.json.McpJsonMapper;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;

/**
 * Thin official-SDK stdio adapter. Only ProcessBuilder creation is customized so
 * the configured environment and cwd remain controlled while framing stays upstream.
 */
final class McpProcessTransport extends StdioClientTransport {
    private final List<String> command;
    private final Map<String, String> environment;
    private final McpProcessPort processPort;

    /** Delegates framing, correlation, timeout plumbing, and close to the MCP SDK. */
    McpProcessTransport(
            String command,
            List<String> args,
            Map<String, String> environment,
            McpProcessPort processPort) {
        super(parameters(command, args), McpJsonMapper.getDefault());
        this.command = new ArrayList<>();
        this.command.add(command);
        if (args != null) {
            this.command.addAll(List.copyOf(args));
        }
        this.environment = Map.copyOf(environment == null ? Map.of() : environment);
        // The port owns process policy and cleanup; allowing a null port would silently bypass that boundary.
        this.processPort = Objects.requireNonNull(processPort, "processPort");
    }

    /** Lets the composition root apply cwd and environment policy before SDK start. */
    @Override
    protected ProcessBuilder getProcessBuilder() {
        return processPort.prepare(command, environment);
    }

    /** Keeps the superclass parameter object free of inherited environment surprises. */
    private static ServerParameters parameters(String command, List<String> args) {
        return ServerParameters.builder(command).args(args == null ? List.of() : args).build();
    }
}
