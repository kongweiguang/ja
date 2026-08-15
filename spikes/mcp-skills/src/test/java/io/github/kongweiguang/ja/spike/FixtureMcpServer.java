/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.spike;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * Tiny local MCP stdio fixture. It intentionally speaks the wire protocol directly so this test
 * proves AgentScope's transport rather than another MCP client library.
 */
public final class FixtureMcpServer {
    private FixtureMcpServer() {}

    /**
     * Reads JSON-RPC lines until the client closes the pipe; no diagnostics are written to stdout
     * because stdout is the protocol channel and stderr is reserved for non-secret diagnostics.
     */
    public static void main(String[] args) throws Exception {
        ObjectMapper mapper = new ObjectMapper();
        boolean crashAfterCall = Arrays.asList(args).contains("--crash-after-call");
        Path environmentReport = optionPath(args, "--env-report-file");
        writeEnvironmentReport(mapper, environmentReport);
        emitConfiguredStderr();
        try (BufferedReader reader = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
                BufferedWriter writer = new BufferedWriter(new OutputStreamWriter(System.out, StandardCharsets.UTF_8))) {
            String line;
            while ((line = reader.readLine()) != null) {
                JsonNode request = mapper.readTree(line);
                if (request == null || request.path("method").isMissingNode()) {
                    continue;
                }
                String method = request.path("method").asText();
                JsonNode id = request.get("id");
                if (id == null || id.isNull()) {
                    continue;
                }
                ObjectNode response = mapper.createObjectNode();
                response.put("jsonrpc", "2.0");
                response.set("id", id);
                ObjectNode result = mapper.createObjectNode();
                switch (method) {
                    case "initialize" -> {
                        result.put("protocolVersion", "2024-11-05");
                        result.set("capabilities", mapper.createObjectNode().set("tools", mapper.createObjectNode()));
                        result.set("serverInfo", mapper.createObjectNode().put("name", "ja-fixture").put("version", "1.0"));
                    }
                    case "tools/list" -> result.set("tools", tools(mapper));
                    case "tools/call" -> {
                        String name = request.path("params").path("name").asText();
                        if ("echo".equals(name)) {
                            ArrayNode content = mapper.createArrayNode();
                            ObjectNode text = mapper.createObjectNode();
                            text.put("type", "text");
                            text.put("text", request.path("params").path("arguments").path("value").asText());
                            content.add(text);
                            result.set("content", content);
                            result.put("isError", false);
                            if (crashAfterCall) {
                                response.set("result", result);
                                write(mapper, writer, response);
                                System.exit(17);
                            }
                        } else {
                            response.set("error", mapper.createObjectNode().put("code", -32601).put("message", "unknown tool"));
                            write(mapper, writer, response);
                            continue;
                        }
                    }
                    default -> {
                        response.set("error", mapper.createObjectNode().put("code", -32601).put("message", "unknown method"));
                        write(mapper, writer, response);
                        continue;
                    }
                }
                response.set("result", result);
                write(mapper, writer, response);
            }
        }
    }

    /**
     * Writes only boolean observations so the test can prove process isolation without persisting
     * a parent marker, resolved secret or server diagnostic into the workspace.
     */
    private static void writeEnvironmentReport(ObjectMapper mapper, Path report) throws Exception {
        if (report == null) {
            return;
        }
        ObjectNode result = mapper.createObjectNode();
        result.put("parentSecretVisible", hasText(System.getenv("JA_MCP_PARENT_SECRET")));
        result.put("allowlistVisible", "visible".equals(System.getenv("JA_MCP_ALLOWLIST_ENV")));
        String secret = System.getenv("JA_MCP_SECRET_ENV");
        result.put("secretEnvResolved", hasText(secret) && !secret.contains("secret-ref://"));
        result.put("stderrMarkerEmitted", hasText(System.getenv("JA_MCP_STDERR_MARKER")));
        Files.writeString(report, mapper.writeValueAsString(result), StandardCharsets.UTF_8);
    }

    /**
     * Emits a caller-provided marker to stderr to prove the safe transport consumes diagnostics
     * without allowing the SDK's default INFO handler to copy them into test or desktop logs.
     */
    private static void emitConfiguredStderr() {
        String marker = System.getenv("JA_MCP_STDERR_MARKER");
        if (hasText(marker)) {
            System.err.println(marker);
            System.err.flush();
        }
    }

    /**
     * Parses one optional fixture path while keeping command-line handling deterministic for the
     * child-process integration test.
     */
    private static Path optionPath(String[] args, String option) {
        for (int index = 0; index + 1 < args.length; index++) {
            if (option.equals(args[index])) {
                return Path.of(args[index + 1]);
            }
        }
        return null;
    }

    /** Returns whether an environment observation contains a usable value without exposing it. */
    private static boolean hasText(String value) {
        return value != null && !value.isBlank();
    }

    /** Returns the one deterministic tool used to count side effects in integration tests. */
    private static ArrayNode tools(ObjectMapper mapper) {
        ArrayNode tools = mapper.createArrayNode();
        ObjectNode tool = mapper.createObjectNode();
        tool.put("name", "echo");
        tool.put("description", "Returns the supplied value");
        ObjectNode schema = mapper.createObjectNode();
        schema.put("type", "object");
        ObjectNode properties = mapper.createObjectNode();
        properties.set("value", mapper.createObjectNode().put("type", "string"));
        schema.set("properties", properties);
        schema.set("required", mapper.createArrayNode().add("value"));
        tool.set("inputSchema", schema);
        tools.add(tool);
        return tools;
    }

    /** Flushes one response before reading again so the client never waits on buffered output. */
    private static void write(ObjectMapper mapper, BufferedWriter writer, ObjectNode response) throws Exception {
        writer.write(mapper.writeValueAsString(response));
        writer.newLine();
        writer.flush();
    }
}
