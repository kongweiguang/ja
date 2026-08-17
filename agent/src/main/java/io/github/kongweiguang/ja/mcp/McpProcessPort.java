/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.Objects;

/**
 * Narrow composition boundary for local MCP process policy.
 * The official MCP stdio transport still owns pipes and shutdown; this port only
 * supplies a validated builder so MCP code does not grow another process runner.
 */
@FunctionalInterface
public interface McpProcessPort {
    /** Creates the builder that the official SDK will start and close. */
    ProcessBuilder prepare(List<String> command, Map<String, String> environment);

    /** Uses an explicit workspace and a clean environment for the normal desktop path. */
    static McpProcessPort restricted(Path workingDirectory) {
        Path cwd = Objects.requireNonNull(workingDirectory, "workingDirectory")
                .toAbsolutePath().normalize();
        if (!Files.isDirectory(cwd)) {
            throw new IllegalArgumentException("mcp_process_cwd_invalid");
        }
        return (command, environment) -> {
            if (command == null || command.isEmpty()) {
                throw new IllegalArgumentException("mcp_process_command_required");
            }
            ProcessBuilder builder = new ProcessBuilder(List.copyOf(command));
            builder.directory(cwd.toFile());
            // Clearing inherited variables prevents the agent JVM from leaking credentials.
            builder.environment().clear();
            if (environment != null) {
                builder.environment().putAll(Map.copyOf(environment));
            }
            return builder;
        };
    }

    /** Uses the current application directory when no workspace-specific port is supplied. */
    static McpProcessPort restricted() {
        return restricted(Path.of(System.getProperty("user.dir", ".")));
    }
}
