// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import io.github.kongweiguang.ja.model.AgentScopeModelFactory;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.runtime.StdioRuntime;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Clock;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.assertTimeout;

/**
 * Proves the production activation path reaches AgentScope's real streaming
 * Chat Completions adapter without replacing the Harness with a test model.
 */
final class StdioOpenAiLoopbackIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";
    private static final String PROFILE_REVISION = "profile_loopback";

    /**
     * Runs one complete profile activation and turn over piped stdio while the
     * model request is served by a local SSE endpoint.  The endpoint is local
     * deliberately: it proves the adapter and Harness composition without
     * requiring a user credential or turning a test into an external call.
     */
    @Test
    void activatesRealOpenAiAdapterAndStreamsHarnessTurn(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        AtomicReference<String> requestBody = new AtomicReference<>();
        AtomicReference<String> requestPath = new AtomicReference<>();
        AtomicInteger requestCount = new AtomicInteger();
        ExecutorService httpExecutor = Executors.newSingleThreadExecutor();
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.setExecutor(httpExecutor);
        server.createContext("/v1/chat/completions", exchange -> handleStreamingRequest(
                exchange, requestBody, requestPath, requestCount));
        server.start();

        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.PRODUCTION, data),
                Clock.systemUTC(), null,
                (profile, secret) -> {
                    assertEquals(null, secret,
                            "loopback compatible profile must not request a credential");
                    return new AgentScopeModelFactory().create(profile, null);
                });
        CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            JsonNode initialize = read(output);
            assertEquals("c:init", initialize.path("id").textValue());
            send(input, initializedFrame());
            assertEquals("ready", read(output).path("params").path("status").textValue());

            send(input, workspaceOpenFrame(workspace));
            assertEquals("c:workspace", read(output).path("id").textValue());
            send(input, profileSaveFrame(server.getAddress().getPort()));
            assertEquals("c:profile", read(output).path("id").textValue());
            send(input, profileActivateFrame());
            JsonNode activation = readUntil(output, "c:activate", null);
            assertFalse(activation.has("error"), activation.toString());
            assertEquals(PROFILE_REVISION, activation.path("result")
                    .path("activeProfileRevision").textValue());

            send(input, turnStartFrame());
            JsonNode accepted = readUntil(output, "c:turn", null);
            assertFalse(accepted.has("error"), accepted.toString());
            List<String> methods = new ArrayList<>();
            boolean completed = false;
            boolean deltaSeen = false;
            boolean finalTextSeen = false;
            while (!completed) {
                JsonNode event = read(output);
                String method = event.path("method").textValue();
                methods.add(method);
                deltaSeen |= "item/delta".equals(method);
                finalTextSeen |= event.toString().contains("loopback final");
                if ("turn/completed".equals(method)) {
                    completed = true;
                    assertEquals("completed", event.path("params").path("turn")
                            .path("terminalStatus").textValue());
                }
            }
            assertTrue(deltaSeen, "the real model stream must reach the JA item delta");
            assertTrue(finalTextSeen, "the final AgentScope text must reach stdio");
            assertTrue(methods.indexOf("turn/started") >= 0, methods.toString());
            assertTrue(methods.indexOf("turn/started") < methods.indexOf("turn/completed"),
                    methods.toString());
            assertEquals(1, requestCount.get());
            assertEquals("/v1/chat/completions", requestPath.get());
            JsonNode request = JSON.readTree(requestBody.get());
            assertEquals("ja-loopback-model", request.path("model").textValue());
            assertTrue(request.path("stream").booleanValue());
            assertTrue(request.path("messages").toString().contains("run loopback"));

            send(input, shutdownFrame());
            assertEquals("c:stop", read(output).path("id").textValue());
        } finally {
            clientInput.close();
            clientOutput.close();
            runtime.close();
            server.stop(0);
            httpExecutor.shutdownNow();
        }
        assertEquals(0, exit.get(10, TimeUnit.SECONDS));
    }

    /** Writes the exact streaming response expected from a Chat Completions endpoint. */
    private static void handleStreamingRequest(HttpExchange exchange,
                                               AtomicReference<String> requestBody,
                                               AtomicReference<String> requestPath,
                                               AtomicInteger requestCount) throws IOException {
        try (exchange) {
            requestCount.incrementAndGet();
            requestBody.set(new String(exchange.getRequestBody().readAllBytes(),
                    StandardCharsets.UTF_8));
            requestPath.set(exchange.getRequestURI().getPath());
            String payload = "data: {\"id\":\"chatcmpl-loopback\",\"object\":\"chat.completion.chunk\","
                    + "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},"
                    + "\"finish_reason\":null}]}\n\n"
                    + "data: {\"id\":\"chatcmpl-loopback\",\"object\":\"chat.completion.chunk\","
                    + "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"loopback final\"},"
                    + "\"finish_reason\":null}]}\n\n"
                    + "data: {\"id\":\"chatcmpl-loopback\",\"object\":\"chat.completion.chunk\","
                    + "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
                    + "data: [DONE]\n\n";
            byte[] bytes = payload.getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("Content-Type", "text/event-stream");
            exchange.sendResponseHeaders(200, bytes.length);
            exchange.getResponseBody().write(bytes);
        }
    }

    /** Builds the initialize request with the frozen protocol limits and capability shape. */
    private static String initializeFrame() throws Exception {
        var params = JsonNodes.object();
        params.put("protocolMajor", 1);
        params.put("protocolMinor", 0);
        params.put("minimumCompatibleMinor", 0);
        params.put("clientVersion", "openai-loopback-test");
        params.set("capabilities", JSON.readTree("{\"methods\":[\"initialize\","
                + "\"workspace/open\",\"profile/save\",\"profile/activate\","
                + "\"turn/start\",\"shutdown\"],\"events\":[\"runtime/statusChanged\","
                + "\"turn/started\",\"item/started\",\"item/delta\",\"item/completed\","
                + "\"turn/completed\"],\"accessModes\":[\"read_only\",\"workspace\","
                + "\"full_access\"],\"itemKinds\":[\"agent_message\"],\"mcp\":{"
                + "\"protocolVersions\":[],\"transports\":[],\"features\":[]}}"));
        params.set("limits", JSON.readTree("{\"maxFrameBytes\":4194304,"
                + "\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024,"
                + "\"maxInFlightRequests\":64,\"maxPendingRequests\":64,"
                + "\"maxItemDeltaBytes\":65536,\"maxInlineToolOutputBytes\":1048576,"
                + "\"maxLogBytes\":1048576,\"defaultRequestDeadlineMs\":120000,"
                + "\"defaultApprovalDeadlineMs\":300000}"));
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:init",
                "method", "initialize", "params", params));
    }

    /** Completes the fixed challenge so the sidecar can publish ready. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\""
                + READY_TOKEN + "\"}}";
    }

    /** Binds the temporary workspace used by the real Harness filesystem. */
    private static String workspaceOpenFrame(Path workspace) throws Exception {
        var params = JsonNodes.object();
        params.put("workspaceId", "ws_loopback");
        params.put("rootPath", workspace.toString());
        params.put("trust", "trusted");
        params.put("displayName", "OpenAI loopback");
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:workspace",
                "method", "workspace/open", "params", params));
    }

    /** Saves a key-free compatible profile pointing only to the local fixture endpoint. */
    private static String profileSaveFrame(int port) throws Exception {
        var model = JsonNodes.object();
        model.put("provider", "loopback");
        model.put("protocol", "openai_chat_completions");
        model.put("model", "ja-loopback-model");
        model.put("baseUrl", "http://127.0.0.1:" + port + "/v1");
        var profile = JsonNodes.object();
        profile.put("profileRevision", PROFILE_REVISION);
        profile.put("name", "OpenAI loopback");
        profile.set("model", model);
        profile.put("accessMode", "workspace");
        var params = JsonNodes.object();
        params.set("profile", profile);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:profile",
                "method", "profile/save", "params", params));
    }

    /** Starts activation without a secret request because the endpoint is local and compatible. */
    private static String profileActivateFrame() throws Exception {
        var params = JsonNodes.object();
        params.put("profileRevision", PROFILE_REVISION);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:activate",
                "method", "profile/activate", "params", params));
    }

    /** Starts one real AgentScope turn whose user message must reach the loopback provider. */
    private static String turnStartFrame() throws Exception {
        var params = JsonNodes.object();
        params.put("threadId", "thr_loopback");
        params.put("userId", "loopback-user");
        params.put("sessionId", "loopback-session");
        params.put("accessMode", "workspace");
        params.put("profileRevision", PROFILE_REVISION);
        var input = JsonNodes.array();
        input.add(JsonNodes.object().put("type", "text").put("text", "run loopback"));
        params.set("input", input);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:turn",
                "method", "turn/start", "params", params));
    }

    /** Requests the normal shutdown path so the runtime can drain and return zero. */
    private static String shutdownFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}";
    }

    /** Writes one complete LF-delimited frame and flushes the client side of the pipe. */
    private static void send(BufferedWriter writer, String frame) throws Exception {
        writer.write(frame);
        writer.write('\n');
        writer.flush();
    }

    /** Reads one bounded frame so a broken sidecar cannot hang the Maven worker forever. */
    private static JsonNode read(BufferedReader reader) throws Exception {
        return assertTimeout(Duration.ofSeconds(15), () -> {
            String line = reader.readLine();
            assertNotNull(line);
            assertFalse(line.isBlank());
            return JSON.readTree(line);
        });
    }

    /** Skips asynchronous events until the correlated response or method arrives. */
    private static JsonNode readUntil(BufferedReader reader, String id, String method)
            throws Exception {
        for (int index = 0; index < 256; index++) {
            JsonNode frame = read(reader);
            if ((id == null || id.equals(frame.path("id").textValue()))
                    && (method == null || method.equals(frame.path("method").textValue()))) {
                return frame;
            }
        }
        throw new AssertionError("frame did not arrive: " + id + "/" + method);
    }
}
