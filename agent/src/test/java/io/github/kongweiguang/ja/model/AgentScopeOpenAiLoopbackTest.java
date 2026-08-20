/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.model;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;

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
import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;

/** Verifies the JA factory reaches AgentScope's real OpenAI Chat Completions transport. */
class AgentScopeOpenAiLoopbackTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String RESPONSE = """
            {"id":"chatcmpl-loopback","object":"chat.completion","created":1,
             "model":"ja-loopback-model","choices":[{"index":0,
             "message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
             "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}
            """.replaceAll("\\s+", "");

    /** Starts a local endpoint and proves the native AgentScope model sends and parses one turn. */
    @Test
    void factoryRoundTripsOpenAiChatCompletionsWithoutProviderSecret() throws Exception {
        AtomicReference<String> requestBody = new AtomicReference<>();
        AtomicReference<String> requestPath = new AtomicReference<>();
        AtomicReference<String> requestMethod = new AtomicReference<>();
        AtomicReference<String> requestAuthorization = new AtomicReference<>();
        ExecutorService executor = Executors.newSingleThreadExecutor();
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.setExecutor(executor);
        server.createContext("/v1/chat/completions", exchange -> handleRequest(
                exchange, requestBody, requestPath, requestMethod, requestAuthorization));
        server.start();
        try {
            ModelProfile profile = ModelProfile.builder()
                    .id("loopback")
                    .displayName("Loopback")
                    .provider(ModelProvider.OPENAI_COMPATIBLE)
                    .api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                    .model("ja-loopback-model")
                    .baseUrl("http://127.0.0.1:" + server.getAddress().getPort() + "/v1")
                    .stream(false)
                    .build();

            List<ChatResponse> responses = new AgentScopeModelFactory()
                    .create(profile, null)
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
            assertEquals("/v1/chat/completions", requestPath.get());
            assertEquals("ja-loopback-model", request.path("model").textValue());
            assertEquals(false, request.path("stream").booleanValue());
            assertEquals("user", request.path("messages").get(0).path("role").textValue());
            assertEquals("ping", request.path("messages").get(0).path("content").textValue());
            assertNull(requestAuthorization.get(), "key-free loopback must not send Authorization");
        } finally {
            server.stop(0);
            executor.shutdownNow();
        }
    }

    /** Responds with a minimal OpenAI-compatible envelope while retaining the exact request evidence. */
    private static void handleRequest(HttpExchange exchange, AtomicReference<String> requestBody,
                                      AtomicReference<String> requestPath,
                                      AtomicReference<String> requestMethod,
                                      AtomicReference<String> requestAuthorization) throws IOException {
        try (exchange) {
            requestBody.set(new String(exchange.getRequestBody().readAllBytes(), StandardCharsets.UTF_8));
            requestPath.set(exchange.getRequestURI().getPath());
            requestMethod.set(exchange.getRequestMethod());
            requestAuthorization.set(exchange.getRequestHeaders().getFirst("Authorization"));
            byte[] payload = RESPONSE.getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("Content-Type", "application/json");
            exchange.sendResponseHeaders(200, payload.length);
            exchange.getResponseBody().write(payload);
        }
    }
}
