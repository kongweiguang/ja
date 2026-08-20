/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.model;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.MsgRole;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.model.ChatResponse;
import io.github.kongweiguang.ja.profiles.ModelApi;
import io.github.kongweiguang.ja.profiles.ModelProfile;
import io.github.kongweiguang.ja.profiles.ModelProvider;
import io.github.kongweiguang.ja.profiles.SecretRef;
import io.github.kongweiguang.ja.profiles.SecretValue;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;

/** Verifies the JA factory reaches AgentScope's real Anthropic Messages transport. */
class AgentScopeAnthropicLoopbackTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String RESPONSE = """
            {"id":"msg_loopback","type":"message","role":"assistant",
             "model":"claude-loopback-model","content":[{"type":"text","text":"pong"}],
             "stop_reason":"end_turn","stop_sequence":null,
             "usage":{"input_tokens":1,"output_tokens":1}}
            """.replaceAll("\\s+", "");

    /** Starts a local endpoint and proves the native AgentScope model sends and parses one turn. */
    @Test
    void factoryRoundTripsAnthropicMessagesWithResolvedSecret() throws Exception {
        AtomicReference<String> requestBody = new AtomicReference<>();
        AtomicReference<String> requestPath = new AtomicReference<>();
        AtomicReference<String> requestMethod = new AtomicReference<>();
        AtomicReference<String> apiKey = new AtomicReference<>();
        AtomicReference<String> apiVersion = new AtomicReference<>();
        ExecutorService executor = Executors.newSingleThreadExecutor();
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.setExecutor(executor);
        server.createContext("/v1/messages", exchange -> handleRequest(
                exchange, requestBody, requestPath, requestMethod, apiKey, apiVersion));
        server.start();
        try {
            ModelProfile profile = ModelProfile.builder()
                    .id("anthropic-loopback")
                    .displayName("Anthropic loopback")
                    .provider(ModelProvider.ANTHROPIC)
                    .api(ModelApi.ANTHROPIC_MESSAGES)
                    .model("claude-loopback-model")
                    .baseUrl("http://127.0.0.1:" + server.getAddress().getPort())
                    .secretRef(new SecretRef("anthropic-loopback-key"))
                    .stream(false)
                    .build();

            List<ChatResponse> responses = new AgentScopeModelFactory()
                    .create(profile, ignored -> SecretValue.of("anthropic-loopback-secret"))
                    .model()
                    .stream(List.of(Msg.builder().role(MsgRole.USER).textContent("ping").build()),
                            List.of(), null)
                    .collectList()
                    .block(Duration.ofSeconds(10));

            assertNotNull(responses);
            assertEquals(1, responses.size());
            assertEquals("pong", ((TextBlock) responses.getFirst().getContent().getFirst()).getText());

            JsonNode request = JSON.readTree(requestBody.get());
            assertEquals("POST", requestMethod.get());
            assertEquals("/v1/messages", requestPath.get());
            assertEquals("anthropic-loopback-secret", apiKey.get());
            assertEquals("2023-06-01", apiVersion.get());
            assertEquals("claude-loopback-model", request.path("model").textValue());
            assertEquals(4096, request.path("max_tokens").intValue());
            assertEquals("user", request.path("messages").get(0).path("role").textValue());
            assertEquals("ping", request.path("messages").get(0).path("content")
                    .get(0).path("text").textValue());
        } finally {
            server.stop(0);
            executor.shutdownNow();
        }
    }

    /** Responds with a minimal Anthropic Messages envelope while retaining request evidence. */
    private static void handleRequest(HttpExchange exchange, AtomicReference<String> requestBody,
                                      AtomicReference<String> requestPath,
                                      AtomicReference<String> requestMethod,
                                      AtomicReference<String> apiKey,
                                      AtomicReference<String> apiVersion) throws IOException {
        try (exchange) {
            requestBody.set(new String(exchange.getRequestBody().readAllBytes(), StandardCharsets.UTF_8));
            requestPath.set(exchange.getRequestURI().getPath());
            requestMethod.set(exchange.getRequestMethod());
            apiKey.set(exchange.getRequestHeaders().getFirst("X-Api-Key"));
            apiVersion.set(exchange.getRequestHeaders().getFirst("anthropic-version"));
            byte[] payload = RESPONSE.getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("Content-Type", "application/json");
            exchange.sendResponseHeaders(200, payload.length);
            exchange.getResponseBody().write(payload);
        }
    }
}
