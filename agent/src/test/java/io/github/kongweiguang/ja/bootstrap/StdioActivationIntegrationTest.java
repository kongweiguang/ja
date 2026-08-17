// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.model.ChatResponse;
import io.agentscope.core.model.GenerateOptions;
import io.agentscope.core.model.Model;
import io.agentscope.core.model.ToolSchema;
import io.github.kongweiguang.ja.model.ModelHandle;
import io.github.kongweiguang.ja.profiles.CapabilitySet;
import io.github.kongweiguang.ja.runtime.StdioRuntime;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import reactor.core.publisher.Flux;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Clock;
import java.time.Duration;
import java.util.Base64;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.assertTimeout;

/** Proves production configuration, asynchronous secret resolution, and real Harness execution. */
final class StdioActivationIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";

    /** Verifies that the ASCII transport preserves Unicode paths and accepts both argv shapes. */
    @Test
    void decodesUnicodeDataDirectoryFromBase64Argv() {
        String value = "数据 目录 🚀x";
        String encoded = Base64.getUrlEncoder().withoutPadding()
                .encodeToString(value.getBytes(StandardCharsets.UTF_8));
        Path expected = Path.of(value).toAbsolutePath().normalize();

        assertFalse(encoded.endsWith("="));
        assertEquals(expected, SidecarConfiguration.fromArgs(
                new String[]{"--data-dir-base64=" + encoded}).dataDirectory());
        assertEquals(expected, SidecarConfiguration.fromArgs(
                new String[]{"--data-dir-base64", encoded}).dataDirectory());
    }

    /** Verifies malformed transport and byte sequences fail before Path construction. */
    @Test
    void rejectsMalformedBase64AndUtf8DataDirectories() {
        assertThrows(IllegalArgumentException.class, () -> SidecarConfiguration.fromArgs(
                new String[]{"--data-dir-base64=not?base64"}));

        String invalidUtf8 = Base64.getUrlEncoder().withoutPadding()
                .encodeToString(new byte[]{(byte) 0xc3, 0x28});
        assertThrows(IllegalArgumentException.class, () -> SidecarConfiguration.fromArgs(
                new String[]{"--data-dir-base64=" + invalidUtf8}));

        assertThrows(IllegalArgumentException.class, () -> SidecarConfiguration.fromArgs(
                new String[]{"--data-dir-base64="}));
        assertThrows(IllegalArgumentException.class, () -> SidecarConfiguration.fromArgs(
                new String[]{"--data-dir-base64", "   "}));
    }

    /** Verifies the legacy plain-path flag remains source-compatible for launchers. */
    @Test
    void keepsLegacyDataDirectoryArg() {
        String value = "legacy 数据 目录";
        Path expected = Path.of(value).toAbsolutePath().normalize();

        assertEquals(expected, SidecarConfiguration.fromArgs(
                new String[]{"--data-dir=" + value}).dataDirectory());
        assertEquals(expected, SidecarConfiguration.fromArgs(
                new String[]{"--data-dir", value}).dataDirectory());
    }

    /**
     * Uses a fake provider model only at the model seam; workspace/profile/activation and the turn
     * itself still traverse the real JSONL runtime, AgentScope Harness, SQLite graph, and shutdown.
     */
    @Test
    void activatesThroughSecretRequestAndRunsHarnessTurn(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.PRODUCTION, data),
                Clock.systemUTC(), null, (profile, secret) -> {
                    assertEquals("secret-from-rust", secret);
                    return new ModelHandle(new FinalModel(), profile.fingerprint(),
                            CapabilitySet.defaults(profile.provider(), profile.api()));
                });
        CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            JsonNode initialize = read(output);
            assertEquals("c:init", initialize.path("id").textValue());
            assertTrue(initialize.path("result").path("capabilities").path("methods")
                    .toString().contains("turn/cancel"));
            send(input, initializedFrame());
            assertEquals("ready", read(output).path("params").path("status").textValue());

            send(input, capabilitiesReadFrame("c:caps-before"));
            JsonNode capabilitiesBefore = readUntil(output, "c:caps-before", null);

            send(input, turnStartFrame());
            JsonNode unavailable = readUntil(output, "c:turn", null);
            assertEquals("MODEL_UNAVAILABLE", unavailable.path("error").path("data")
                    .path("jaCode").textValue());

            send(input, workspaceOpenFrame(workspace));
            assertEquals("c:workspace", read(output).path("id").textValue());
            send(input, profileSaveFrame("profile_activation"));
            assertEquals("c:profile", read(output).path("id").textValue());
            send(input, profileActivateFrame());
            JsonNode secret = read(output);
            assertEquals("secret/resolve", secret.path("method").textValue());
            assertTrue(secret.path("id").textValue().startsWith("s:secret_"));

            // The activation request is still pending on the Rust secret. A replay must be
            // ignored so the eventual activation result remains the only response for c:activate.
            send(input, profileActivateFrame());
            send(input, secretResponse(secret.path("id").textValue()));
            JsonNode activation = readUntil(output, "c:activate", null);
            assertFalse(activation.has("error"), activation.toString());
            assertEquals("profile_activation", activation.path("result")
                    .path("activeProfileRevision").textValue());

            send(input, capabilitiesReadFrame("c:caps-after"));
            JsonNode capabilitiesAfter = readUntil(output, "c:caps-after", null);
            assertEquals(capabilitiesBefore.path("result").path("capabilities"),
                    capabilitiesAfter.path("result").path("capabilities"),
                    "activation must not mutate the negotiated method set");

            // After the original response has been queued, replaying the same id is a completed
            // request and may receive the stable duplicate error without starting activation.
            send(input, profileActivateFrame());
            JsonNode replay = readUntil(output, "c:activate", null);
            assertEquals("DUPLICATE_REQUEST", replay.path("error").path("data")
                    .path("jaCode").textValue());

            send(input, turnStartFrame("c:turn-after-activation"));
            JsonNode accepted = readUntil(output, "c:turn-after-activation", null);
            assertFalse(accepted.has("error"), accepted.toString());
            boolean completed = false;
            while (!completed) {
                JsonNode event = read(output);
                completed = "turn/completed".equals(event.path("method").textValue());
                if (completed) {
                    assertEquals("completed", event.path("params").path("turn")
                            .path("terminalStatus").textValue());
                }
            }
            send(input, profileSaveFrame("c:profile2", "profile_second"));
            assertEquals("c:profile2", read(output).path("id").textValue());
            send(input, profileActivateFrame("c:activate-second", "profile_second"));
            JsonNode secondActivation = readUntil(output, "c:activate-second", null);
            assertEquals("CONFLICT", secondActivation.path("error").path("data")
                    .path("jaCode").textValue());
            send(input, shutdownFrame());
            assertEquals("c:stop", read(output).path("id").textValue());
        } finally {
            clientInput.close();
            clientOutput.close();
            runtime.close();
        }
        assertEquals(0, exit.get(10, TimeUnit.SECONDS));
            assertTrue(Files.isRegularFile(data.resolve("ja.sqlite")));
    }

    /** A secret-store failure is returned as a stable error and never opens a partial graph. */
    @Test
    void secretErrorDoesNotActivateGraph(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.PRODUCTION, data),
                Clock.systemUTC(), null, (profile, secret) -> {
                    throw new AssertionError("provider model must not be built after secret error");
                });
        CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            read(output);
            send(input, initializedFrame());
            read(output);
            send(input, workspaceOpenFrame(workspace));
            read(output);
            send(input, profileSaveFrame("profile_error"));
            read(output);
            send(input, profileActivateFrame("profile_error"));
            JsonNode secret = read(output);
            send(input, secretErrorResponse(secret.path("id").textValue()));
            JsonNode activation = readUntil(output, "c:activate", null);
            assertEquals("SECRET_NOT_FOUND", activation.path("error").path("data")
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

    /** Proves a missing Rust secret response cannot strand activation and that retry is safe. */
    @Test
    void secretTimeoutRetiresPendingRequestAndAllowsRetry(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.PRODUCTION, data),
                Clock.systemUTC(), null, (profile, secret) -> {
                    assertEquals("secret-from-rust", secret);
                    return new ModelHandle(new FinalModel(), profile.fingerprint(),
                            CapabilitySet.defaults(profile.provider(), profile.api()));
                });
        CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            read(output);
            send(input, initializedFrame());
            read(output);
            send(input, workspaceOpenFrame(workspace));
            read(output);
            send(input, profileSaveFrame("profile_timeout"));
            read(output);

            long startedAt = System.nanoTime();
            send(input, profileActivateFrame("c:activate-timeout", "profile_timeout"));
            JsonNode firstSecret = readUntil(output, null, "secret/resolve");
            JsonNode timeout = readUntil(output, "c:activate-timeout", null);
            long elapsedMillis = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - startedAt);
            assertEquals("MODEL_UNAVAILABLE", timeout.path("error").path("data")
                    .path("jaCode").textValue());
            assertTrue(elapsedMillis >= 4_000 && elapsedMillis < 10_000,
                    "secret deadline was not bounded: " + elapsedMillis + "ms");

            // This response belongs to the retired request and must not revive activation or emit
            // a second client response. The next activation below proves the runtime remains live.
            send(input, secretResponse(firstSecret.path("id").textValue()));
            send(input, profileActivateFrame("c:activate-retry", "profile_timeout"));
            JsonNode secondSecret = readUntil(output, null, "secret/resolve");
            assertFalse(firstSecret.path("id").textValue().equals(secondSecret.path("id")
                    .textValue()));
            send(input, secretResponse(secondSecret.path("id").textValue()));
            JsonNode retry = readUntil(output, "c:activate-retry", null);
            assertFalse(retry.has("error"), retry.toString());
            send(input, shutdownFrame());
            assertEquals("c:stop", read(output).path("id").textValue());
        } finally {
            clientInput.close();
            clientOutput.close();
            runtime.close();
        }
        assertEquals(0, exit.get(10, TimeUnit.SECONDS));
    }

    /** Builds the minimal client capabilities required to retain the activation methods. */
    private static String initializeFrame() throws Exception {
        var params = JsonNodes.object();
        params.put("protocolMajor", 1);
        params.put("protocolMinor", 0);
        params.put("minimumCompatibleMinor", 0);
        params.put("clientVersion", "activation-test");
        params.set("capabilities", JSON.readTree("{\"methods\":[\"initialize\",\"workspace/open\",\"profile/save\",\"profile/activate\",\"turn/start\",\"turn/cancel\",\"shutdown\"],\"events\":[\"runtime/statusChanged\",\"turn/started\",\"item/started\",\"item/delta\",\"item/completed\",\"turn/completed\"],\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"],\"itemKinds\":[\"agent_message\"],\"mcp\":{\"protocolVersions\":[],\"transports\":[],\"features\":[]}}"));
        params.set("limits", JSON.readTree("{\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024,\"maxInFlightRequests\":64,\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536,\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576,\"defaultRequestDeadlineMs\":120000,\"defaultApprovalDeadlineMs\":300000}"));
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:init",
                "method", "initialize", "params", params));
    }

    /** Builds the handshake completion notification with the fixed test challenge. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\""
                + READY_TOKEN + "\"}}";
    }

    /** Reads the stable capability projection without changing activation state. */
    private static String capabilitiesReadFrame(String id) throws Exception {
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", "capabilities/read", "params", Map.of()));
    }

    /** Builds one workspace binding request without creating the directory in Java. */
    private static String workspaceOpenFrame(Path workspace) throws Exception {
        var params = JsonNodes.object();
        params.put("workspaceId", "ws_activation");
        params.put("rootPath", workspace.toString());
        params.put("trust", "trusted");
        params.put("displayName", "activation workspace");
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:workspace",
                "method", "workspace/open", "params", params));
    }

    /** Builds a secret-free profile save using the frozen Anthropic/OpenAI-compatible fields. */
    private static String profileSaveFrame(String revision) throws Exception {
        return profileSaveFrame("c:profile", revision);
    }

    /** Builds a profile save with a caller-owned id for conflict/expected-revision cases. */
    private static String profileSaveFrame(String id, String revision) throws Exception {
        var model = JsonNodes.object();
        model.put("provider", "openai");
        model.put("protocol", "openai_chat_completions");
        model.put("model", "fixture-model");
        model.put("credentialRef", "cred_activation");
        var profile = JsonNodes.object();
        profile.put("profileRevision", revision);
        profile.put("name", "Activation fixture");
        profile.set("model", model);
        profile.put("accessMode", "workspace");
        var params = JsonNodes.object();
        params.set("profile", profile);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", "profile/save", "params", params));
    }

    /** Starts one asynchronous activation whose response must follow the secret request. */
    private static String profileActivateFrame() throws Exception {
        return profileActivateFrame("profile_activation");
    }

    /** Builds activation for an explicit revision so stale and second-switch cases stay visible. */
    private static String profileActivateFrame(String revision) throws Exception {
        return profileActivateFrame("c:activate", revision);
    }

    /** Builds activation with an explicit request id so timeout/retry ids cannot collide. */
    private static String profileActivateFrame(String id, String revision) throws Exception {
        var params = JsonNodes.object();
        params.put("profileRevision", revision);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", "profile/activate", "params", params));
    }

    /** Replies only with the opaque secret field; the test never puts it into an event or log. */
    private static String secretResponse(String id) {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"" + id
                + "\",\"result\":{\"secretValue\":\"secret-from-rust\"}}";
    }

    /** Builds a valid stable secret-store failure without provider details. */
    private static String secretErrorResponse(String id) {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"" + id
                + "\",\"error\":{\"code\":-32050,\"message\":\"The secret was not found.\","
                + "\"data\":{\"jaCode\":\"SECRET_NOT_FOUND\",\"retryable\":false}}}";
    }

    /** Builds one turn after the active profile is installed. */
    private static String turnStartFrame() throws Exception {
        return turnStartFrame("c:turn");
    }

    /** Builds a turn with an explicit request id so each generation id remains single-use. */
    private static String turnStartFrame(String id) throws Exception {
        var params = JsonNodes.object();
        params.put("threadId", "thr_activation");
        params.put("userId", "activation-user");
        params.put("sessionId", "activation-session");
        params.put("accessMode", "workspace");
        params.put("profileRevision", "profile_activation");
        var input = JsonNodes.array();
        input.add(JsonNodes.object().put("type", "text").put("text", "say hello"));
        params.set("input", input);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", "turn/start", "params", params));
    }

    /** Builds the normal graceful shutdown request. */
    private static String shutdownFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}";
    }

    /** Writes one complete JSONL frame. */
    private static void send(BufferedWriter writer, String frame) throws Exception {
        writer.write(frame);
        writer.write('\n');
        writer.flush();
    }

    /** Reads one bounded frame while keeping the test deterministic on a pipe. */
    private static JsonNode read(BufferedReader reader) throws Exception {
        return assertTimeout(Duration.ofSeconds(10), () -> {
            String line = reader.readLine();
            assertNotNull(line);
            assertFalse(line.isBlank());
            return JSON.readTree(line);
        });
    }

    /** Reads until the correlated response arrives, ignoring asynchronous notifications. */
    private static JsonNode readUntil(BufferedReader reader, String id, String method)
            throws Exception {
        for (int index = 0; index < 128; index++) {
            JsonNode frame = read(reader);
            if ((id == null || id.equals(frame.path("id").textValue()))
                    && (method == null || method.equals(frame.path("method").textValue()))) {
                return frame;
            }
        }
        throw new AssertionError("frame did not arrive: " + id + "/" + method);
    }

    /** Returns deterministic final text while AgentScope still owns the Harness turn lifecycle. */
    private static final class FinalModel implements Model {
        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            return Flux.just(new ChatResponse("activation-final",
                    List.of(TextBlock.builder().text("activation complete").build()), null,
                    Map.of(), "stop"));
        }

        /** Gives AgentScope a stable model identity for the persisted session state. */
        @Override
        public String getModelName() {
            return "ja-activation-fixture";
        }
    }
}
