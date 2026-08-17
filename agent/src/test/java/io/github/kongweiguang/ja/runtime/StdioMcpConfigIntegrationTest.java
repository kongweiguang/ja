// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.github.kongweiguang.ja.bootstrap.SidecarConfiguration;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Verifies only the generation-local MCP save/list/delete configuration lane. */
final class StdioMcpConfigIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";

    /**
     * Exercises frozen DTO mapping and generation semantics over the real JSONL
     * runtime, including the profile reference guard that protects deletion.
     */
    @Test
    void synchronizesMcpDefinitionsWithoutSecretMarkers(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.FAKE, data));
        CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            JsonNode initialized = read(output);
            assertTrue(contains(initialized.path("result").path("capabilities").path("methods"),
                    "mcp/save"));
            assertTrue(contains(initialized.path("result").path("capabilities").path("methods"),
                    "mcp/list"));
            assertTrue(contains(initialized.path("result").path("capabilities").path("methods"),
                    "mcp/delete"));
            send(input, initializedFrame());
            assertEquals("ready", read(output).path("params").path("status").textValue());

            send(input, request("c:list-empty", "mcp/list", JsonNodes.object()));
            assertEquals(0, read(output).path("result").path("servers").size());

            send(input, request("c:save-stdio", "mcp/save", stdioSaveParams("mcp_stdio")));
            JsonNode first = read(output);
            assertFalse(first.has("error"), first.toString());
            assertTrue(first.path("result").path("created").booleanValue());
            assertTrue(first.path("result").path("server").path("enabled").booleanValue());
            assertEquals("unavailable", first.path("result").path("server").path("status")
                    .textValue());
            assertSummaryShape(first.path("result").path("server"), "stdio");
            assertNoSecretMarker(first);

            send(input, request("c:save-stdio-idempotent", "mcp/save",
                    stdioSaveParams("mcp_stdio")));
            assertFalse(read(output).path("result").path("created").booleanValue());

            var changed = stdioSaveParams("mcp_stdio");
            changed.with("server").put("endpoint", "other-tool");
            send(input, request("c:save-conflict", "mcp/save", changed));
            assertEquals("CONFLICT", read(output).path("error").path("data").path("jaCode")
                    .textValue());

            send(input, request("c:save-legacy", "mcp/save", legacyHttpSaveParams()));
            JsonNode legacy = read(output);
            assertEquals("bearer", legacy.path("result").path("server").path("auth")
                    .path("kind").textValue());
            assertFalse(legacy.path("result").path("server").has("credentialRef"));
            assertSummaryShape(legacy.path("result").path("server"), "streamable_http");
            assertNoSecretMarker(legacy);

            send(input, request("c:list-populated", "mcp/list", JsonNodes.object()));
            JsonNode listed = read(output);
            assertEquals(2, listed.path("result").path("servers").size());
            for (JsonNode server : listed.path("result").path("servers")) {
                assertSummaryShape(server, server.path("transport").textValue());
            }

            send(input, request("c:workspace", "workspace/open", workspaceParams(workspace)));
            assertFalse(read(output).has("error"));

            for (String field : List.of("args", "env", "headers", "queryParams", "auth",
                    "credentialRef", "enabled")) {
                ObjectNodeLike nullField = stdioSaveParams("mcp_null_" + field);
                nullField.with("server").putNull(field);
                send(input, request("c:null-" + field, "mcp/save", nullField));
                assertEquals("INVALID_PARAMS", read(output).path("error").path("data")
                        .path("jaCode").textValue());
            }

            ObjectNodeLike nullExpected = stdioSaveParams("mcp_null_expected");
            nullExpected.node().putNull("expectedRevision");
            send(input, request("c:null-expected", "mcp/save", nullExpected));
            assertEquals("INVALID_PARAMS", read(output).path("error").path("data")
                    .path("jaCode").textValue());

            for (String field : List.of("baseUrl", "credentialRef", "supportsVision")) {
                ObjectNodeLike nullField = profileParams("mcp_stdio");
                nullField.with("profile").with("model").putNull(field);
                send(input, request("c:null-model-" + field, "profile/save", nullField));
                assertEquals("INVALID_PARAMS", read(output).path("error").path("data")
                        .path("jaCode").textValue());
            }
            for (String field : List.of("skillRevisions", "mcpRevisions")) {
                ObjectNodeLike nullField = profileParams("mcp_stdio");
                nullField.with("profile").putNull(field);
                send(input, request("c:null-profile-" + field, "profile/save", nullField));
                assertEquals("INVALID_PARAMS", read(output).path("error").path("data")
                        .path("jaCode").textValue());
            }
            ObjectNodeLike nullProfileExpected = profileParams("mcp_stdio");
            nullProfileExpected.node().putNull("expectedRevision");
            send(input, request("c:null-profile-expected", "profile/save", nullProfileExpected));
            assertEquals("INVALID_PARAMS", read(output).path("error").path("data")
                    .path("jaCode").textValue());
            ObjectNodeLike nonTextProfileExpected = profileParams("mcp_stdio");
            nonTextProfileExpected.node().put("expectedRevision", 7);
            send(input, request("c:nontext-profile-expected", "profile/save",
                    nonTextProfileExpected));
            assertEquals("INVALID_PARAMS", read(output).path("error").path("data")
                    .path("jaCode").textValue());

            send(input, request("c:profile", "profile/save", profileParams("mcp_stdio")));
            assertFalse(read(output).has("error"));

            send(input, request("c:delete-referenced", "mcp/delete",
                    Map.of("mcpRevision", "mcp_stdio")));
            assertEquals("CONFLICT", read(output).path("error").path("data").path("jaCode")
                    .textValue());

            send(input, request("c:delete-unknown", "mcp/delete",
                    Map.of("mcpRevision", "mcp_missing")));
            assertEquals("MCP_SERVER_UNAVAILABLE", read(output).path("error").path("data")
                    .path("jaCode").textValue());

            send(input, request("c:delete-legacy", "mcp/delete",
                    Map.of("mcpRevision", "mcp_http")));
            assertTrue(read(output).path("result").path("accepted").booleanValue());

            send(input, request("c:bad-protocol", "mcp/save",
                    invalidServerParams("mcp_bad_protocol", "2099-01-01", null)));
            assertEquals("MCP_PROTOCOL_UNSUPPORTED", read(output).path("error").path("data")
                    .path("jaCode").textValue());
            send(input, request("c:bad-auth", "mcp/save",
                    invalidServerParams("mcp_bad_auth", "2025-06-18", "oauth")));
            assertEquals("MCP_UNSUPPORTED_AUTH", read(output).path("error").path("data")
                    .path("jaCode").textValue());
            send(input, request("c:auth-legacy-conflict", "mcp/save",
                    authLegacyConflictParams()));
            assertEquals("INVALID_PARAMS", read(output).path("error").path("data")
                    .path("jaCode").textValue());

            send(input, shutdownFrame());
            assertEquals("c:stop", read(output).path("id").textValue());
        } finally {
            clientInput.close();
            clientOutput.close();
            runtime.close();
        }
        assertEquals(0, exit.get(10, TimeUnit.SECONDS));
    }

    /** Builds client capabilities that retain only the methods under this slice. */
    private static String initializeFrame() throws Exception {
        var params = JsonNodes.object();
        params.put("protocolMajor", 1);
        params.put("protocolMinor", 0);
        params.put("minimumCompatibleMinor", 0);
        params.put("clientVersion", "mcp-config-test");
        params.set("capabilities", JSON.readTree("{\"methods\":[\"mcp/list\",\"mcp/save\",\"mcp/delete\",\"workspace/open\",\"profile/save\"],\"events\":[],\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"],\"itemKinds\":[],\"mcp\":{\"protocolVersions\":[],\"transports\":[],\"features\":[]}}"));
        params.set("limits", JSON.readTree("{\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024,\"maxInFlightRequests\":64,\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536,\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576,\"defaultRequestDeadlineMs\":120000,\"defaultApprovalDeadlineMs\":300000}"));
        return request("c:init", "initialize", params);
    }

    /** Completes the challenge transition before configuration requests are admitted. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\""
                + READY_TOKEN + "\"}}";
    }

    /** Builds a stdio server with explicit non-sensitive process arguments. */
    private static ObjectNodeLike stdioSaveParams(String revision) {
        var server = JsonNodes.object();
        server.put("mcpRevision", revision);
        server.put("name", "stdio fixture");
        server.put("transport", "stdio");
        server.put("endpoint", "fixture-tool");
        server.put("protocolVersion", "2025-06-18");
        server.set("args", JsonNodes.array().add("--mode").add("fixture"));
        server.set("env", JsonNodes.object().put("FIXTURE_MODE", "test"));
        server.put("enabled", true);
        return new ObjectNodeLike(JsonNodes.object().set("server", server));
    }

    /** Builds the old HTTP credentialRef shorthand that must normalize to bearer auth. */
    private static ObjectNodeLike legacyHttpSaveParams() {
        var server = JsonNodes.object();
        server.put("mcpRevision", "mcp_http");
        server.put("name", "http fixture");
        server.put("transport", "streamable_http");
        server.put("endpoint", "https://example.test/mcp");
        server.put("protocolVersion", "2025-06-18");
        server.put("credentialRef", "cred_mcp");
        return new ObjectNodeLike(JsonNodes.object().set("server", server));
    }

    /** Builds a profile reference so deletion must observe the saved profile snapshot. */
    private static ObjectNodeLike profileParams(String mcpRevision) {
        var model = JsonNodes.object();
        model.put("provider", "openai");
        model.put("protocol", "openai_chat_completions");
        model.put("model", "fixture-model");
        var profile = JsonNodes.object();
        profile.put("profileRevision", "profile_mcp_config");
        profile.put("name", "MCP config fixture");
        profile.put("accessMode", "workspace");
        profile.set("model", model);
        profile.set("mcpRevisions", JsonNodes.array().add(mcpRevision));
        return new ObjectNodeLike(JsonNodes.object().set("profile", profile));
    }

    /** Creates invalid protocol/auth cases without putting untrusted text in diagnostics. */
    private static ObjectNodeLike invalidServerParams(String revision, String protocol, String authKind) {
        var server = JsonNodes.object();
        server.put("mcpRevision", revision);
        server.put("name", "invalid fixture");
        server.put("transport", "streamable_http");
        server.put("endpoint", "https://example.test/mcp");
        server.put("protocolVersion", protocol);
        if (authKind != null) {
            server.set("auth", JsonNodes.object().put("kind", authKind));
        }
        return new ObjectNodeLike(JsonNodes.object().set("server", server));
    }

    /** Builds the legacy-plus-auth conflict accepted by neither DTO representation. */
    private static ObjectNodeLike authLegacyConflictParams() {
        var server = JsonNodes.object();
        server.put("mcpRevision", "mcp_auth_conflict");
        server.put("name", "invalid fixture");
        server.put("transport", "streamable_http");
        server.put("endpoint", "https://example.test/mcp");
        server.put("protocolVersion", "2025-06-18");
        server.put("credentialRef", "cred_mcp");
        server.set("auth", JsonNodes.object().put("kind", "none"));
        return new ObjectNodeLike(JsonNodes.object().set("server", server));
    }

    /** Builds a workspace binding needed only because profile/save is intentionally workspace-scoped. */
    private static ObjectNodeLike workspaceParams(Path workspace) {
        var params = JsonNodes.object();
        params.put("workspaceId", "ws_mcp_config");
        params.put("rootPath", workspace.toString());
        params.put("trust", "trusted");
        return new ObjectNodeLike(params);
    }

    /** Wraps object parameters while keeping test request construction concise. */
    private record ObjectNodeLike(com.fasterxml.jackson.databind.node.ObjectNode node) {
        private com.fasterxml.jackson.databind.node.ObjectNode with(String field) {
            return (com.fasterxml.jackson.databind.node.ObjectNode) node.get(field);
        }
    }

    /** Serializes one request using the same JSONL envelope as the desktop client. */
    private static String request(String id, String method, Object params) throws Exception {
        Object value = params instanceof ObjectNodeLike wrapped ? wrapped.node() : params;
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", method, "params", value));
    }

    /** Builds graceful shutdown for the sidecar owner. */
    private static String shutdownFrame() throws Exception {
        return request("c:stop", "shutdown", JsonNodes.object());
    }

    /** Writes one complete JSONL request. */
    private static void send(BufferedWriter writer, String frame) throws Exception {
        writer.write(frame);
        writer.write('\n');
        writer.flush();
    }

    /** Reads one non-empty response and enforces the no-secret-marker stdout contract. */
    private static JsonNode read(BufferedReader reader) throws Exception {
        String line = reader.readLine();
        assertNotNull(line);
        assertFalse(line.isBlank());
        assertFalse(line.contains("secret-ref://"), "secret marker leaked to stdout");
        return JSON.readTree(line);
    }

    /** Checks one capability entry without coupling the test to array ordering. */
    private static boolean contains(JsonNode values, String expected) {
        for (JsonNode value : values) {
            if (expected.equals(value.textValue())) {
                return true;
            }
        }
        return false;
    }

    /** Locks the transport-specific optional field shape consumed by the TS result schemas. */
    private static void assertSummaryShape(JsonNode server, String transport) {
        assertTrue(server.has("auth"));
        assertTrue(server.has("enabled"));
        assertTrue(server.has("status"));
        assertTrue(server.has("toolCount"));
        if ("stdio".equals(transport)) {
            assertTrue(server.has("args"));
            assertTrue(server.has("env"));
            assertFalse(server.has("headers"));
            assertFalse(server.has("queryParams"));
        } else {
            assertFalse(server.has("args"));
            assertFalse(server.has("env"));
            assertTrue(server.has("headers"));
            assertTrue(server.has("queryParams"));
        }
    }

    /** Keeps response assertions readable while retaining a single stdout guard. */
    private static void assertNoSecretMarker(JsonNode response) {
        assertFalse(response.toString().contains("secret-ref://"));
    }
}
