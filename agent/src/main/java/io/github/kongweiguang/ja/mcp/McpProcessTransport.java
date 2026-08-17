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
    private final List<String> protocolVersions;

    /** Delegates framing, correlation, timeout plumbing, and close to the MCP SDK. */
    McpProcessTransport(
            String command,
            List<String> args,
            Map<String, String> environment,
            McpProcessPort processPort,
            String protocolVersion) {
        super(parameters(command, args), McpJsonMapper.getDefault());
        this.command = new ArrayList<>();
        this.command.add(command);
        if (args != null) {
            this.command.addAll(List.copyOf(args));
        }
        this.environment = Map.copyOf(environment == null ? Map.of() : environment);
        // The port owns process policy and cleanup; allowing a null port would silently bypass that boundary.
        this.processPort = Objects.requireNonNull(processPort, "processPort");
        // The SDK's stdio default is one older version; carry the already-selected wire version
        // here so initialize negotiation has the same single-version contract as HTTP.
        this.protocolVersions = List.of(McpConfigSupport.validateProtocolVersion(protocolVersion));
    }

    /** Lets the composition root apply cwd and environment policy before SDK start. */
    @Override
    protected ProcessBuilder getProcessBuilder() {
        return processPort.prepare(command, environment);
    }

    /** Restricts stdio negotiation to the protocol version selected by the JA wire request. */
    @Override
    public List<String> protocolVersions() {
        return protocolVersions;
    }

    /** Keeps the superclass parameter object free of inherited environment surprises. */
    private static ServerParameters parameters(String command, List<String> args) {
        return ServerParameters.builder(command).args(args == null ? List.of() : args).build();
    }
}
