/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.spike;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.ByteArrayOutputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Loopback-only Streamable HTTP fixture used by the native executable.
 *
 * <p>The fixture speaks the same request/response JSON-RPC subset as the JVM test and captures the
 * resolved Authorization header only in memory. It intentionally does not expose a network or
 * secret-bearing persistence path; its purpose is to prove the native image reaches the real
 * AgentScope HTTP transport and tool wrapper.
 */
final class NativeHttpMcpFixture implements AutoCloseable {
    private static final ObjectMapper JSON = new ObjectMapper();
    private final HttpServer server;
    private final ExecutorService executor;
    private volatile String authorization;
    private volatile String lastPathAndQuery = "";

    private NativeHttpMcpFixture(HttpServer server, ExecutorService executor) {
        this.server = server;
        this.executor = executor;
    }

    /** Starts a loopback server with the MCP endpoint before returning its URL. */
    static NativeHttpMcpFixture start() throws IOException {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        ExecutorService executor = Executors.newCachedThreadPool(runnable -> {
            Thread thread = new Thread(runnable, "ja-native-http-fixture");
            thread.setDaemon(true);
            return thread;
        });
        NativeHttpMcpFixture fixture = new NativeHttpMcpFixture(server, executor);
        server.createContext("/mcp", fixture::handle);
        server.setExecutor(executor);
        server.start();
        return fixture;
    }

    /** Returns the URL used by AgentScope's Streamable HTTP builder. */
    String url() {
        return "http://127.0.0.1:" + server.getAddress().getPort() + "/mcp";
    }

    /** Returns the in-memory header observation without writing the resolved secret anywhere. */
    String authorization() {
        return authorization;
    }

    /** Returns only the path and query so the caller can prove no secret entered the URL. */
    String lastPathAndQuery() {
        return lastPathAndQuery;
    }

    /**
     * Handles POST JSON-RPC messages using the SDK-supported finite JSON response variant; keeping
     * the fixture on the protocol path avoids proving a separate custom HTTP client.
     */
    private void handle(HttpExchange exchange) throws IOException {
        lastPathAndQuery = exchange.getRequestURI().toString();
        authorization = exchange.getRequestHeaders().getFirst("Authorization");
        if (!"POST".equalsIgnoreCase(exchange.getRequestMethod())) {
            exchange.sendResponseHeaders(405, -1);
            exchange.close();
            return;
        }
        JsonNode request = JSON.readTree(readBounded(exchange));
        if (request == null || request.path("id").isMissingNode()) {
            exchange.sendResponseHeaders(202, -1);
            exchange.close();
            return;
        }
        ObjectNode response = JSON.createObjectNode();
        response.put("jsonrpc", "2.0");
        response.set("id", request.get("id"));
        ObjectNode result = JSON.createObjectNode();
        switch (request.path("method").asText()) {
            case "initialize" -> {
                result.put("protocolVersion", "2025-03-26");
                result.set("capabilities", JSON.createObjectNode().set("tools", JSON.createObjectNode()));
                result.set("serverInfo", JSON.createObjectNode().put("name", "ja-native-http").put("version", "1"));
            }
            case "tools/list" -> result.set("tools", tools());
            case "tools/call" -> {
                ArrayNode content = JSON.createArrayNode();
                content.add(JSON.createObjectNode().put("type", "text").put("text", "native-http-ok"));
                result.set("content", content);
                result.put("isError", false);
            }
            default -> response.set("error", JSON.createObjectNode().put("code", -32601).put("message", "unknown"));
        }
        if (!response.has("error")) {
            response.set("result", result);
        }
        byte[] json = JSON.writeValueAsBytes(response);
        byte[] bytes = new byte[json.length + 1];
        System.arraycopy(json, 0, bytes, 0, json.length);
        bytes[json.length] = '\n';
        exchange.getResponseHeaders().set("Content-Type", "application/json");
        exchange.getResponseHeaders().set("Mcp-Session-Id", "ja-native-http-session");
        exchange.sendResponseHeaders(200, bytes.length);
        try (OutputStream output = exchange.getResponseBody()) {
            output.write(bytes);
        }
    }

    /**
     * Keeps the fixture body finite even though formal HTTP response limits remain a JAVA-MCP
     * production gate; this local server is not a substitute for that future bounded transport.
     */
    private static byte[] readBounded(HttpExchange exchange) throws IOException {
        String lengthHeader = exchange.getRequestHeaders().getFirst("Content-Length");
        if (lengthHeader == null) {
            throw new IOException("native_http_fixture_content_length_required");
        }
        int expected;
        try {
            expected = Integer.parseInt(lengthHeader);
        } catch (NumberFormatException exception) {
            throw new IOException("native_http_fixture_content_length_invalid", exception);
        }
        if (expected < 0 || expected > 1024 * 1024) {
            throw new IOException("native_http_fixture_body_limit");
        }
        ByteArrayOutputStream bytes = new ByteArrayOutputStream(expected);
        byte[] buffer = new byte[8192];
        int total = 0;
        int read;
        while (total < expected && (read = exchange.getRequestBody().read(buffer, 0, Math.min(buffer.length, expected - total))) >= 0) {
            total += read;
            bytes.write(buffer, 0, read);
        }
        if (total != expected) {
            throw new IOException("native_http_fixture_body_truncated");
        }
        return bytes.toByteArray();
    }

    /** Emits the one valid object-schema tool used by the gateway approval/call assertion. */
    private static ArrayNode tools() {
        ArrayNode tools = JSON.createArrayNode();
        ObjectNode tool = JSON.createObjectNode();
        tool.put("name", "echo");
        tool.put("description", "Native HTTP echo");
        ObjectNode schema = JSON.createObjectNode();
        schema.put("type", "object");
        schema.set("properties", JSON.createObjectNode());
        tool.set("inputSchema", schema);
        tools.add(tool);
        return tools;
    }

    /** Stops only the loopback fixture and releases its daemon executor. */
    @Override
    public void close() {
        server.stop(0);
        executor.shutdownNow();
    }
}
