// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.json.JsonMapper;
import io.github.kongweiguang.ja.bootstrap.SidecarConfiguration;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.persistence.SqlitePersistence;
import io.github.kongweiguang.ja.persistence.SqlitePersistenceConfig;
import io.github.kongweiguang.ja.protocol.RpcRequest;
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
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Exercises the real stdio fake turn against the same durable history used after restart. */
final class StdioHistoryIntegrationTest {
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";
    private static final JsonMapper JSON = JsonMapper.builder().build();

    /** Creates, streams, reads, closes, and reopens one workspace/thread timeline. */
    @Test
    void fakeTurnIsVisibleThroughThreadReadAfterRestart(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        SqlitePersistenceConfig config = SqlitePersistenceConfig.of(
                data.resolve("ja.sqlite"), data.resolve("ja.sqlite.bak"));
        String threadId;
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            Session session = new Session(persistence, workspace);
            session.send(initializeFrame());
            assertEquals("c:init", session.read().path("id").textValue());
            session.send(initializedFrame());
            assertEquals("ready", session.read().path("params").path("status").textValue());
            session.send(request("c:workspace", "workspace/open", "{\"workspaceId\":\"ws_history\","
                    + "\"rootPath\":\"" + escape(workspace) + "\",\"trust\":\"trusted\"}"));
            assertEquals("c:workspace", session.read().path("id").textValue());
            session.send(request("c:create", "thread/create", "{\"workspaceId\":\"ws_history\","
                    + "\"title\":\"History\"}"));
            JsonNode created = session.read();
            assertEquals("c:create", created.path("id").textValue());
            threadId = created.path("result").path("thread").path("threadId").textValue();
            assertTrue(threadId != null && threadId.startsWith("thr_"));
            session.send(request("c:list-limit", "thread/list", "{\"workspaceId\":\""
                    + "ws_history\",\"limit\":0}"));
            assertInvalidParams(session.read());
            session.send(request("c:list-cursor", "thread/list", "{\"workspaceId\":\""
                    + "ws_history\",\"cursor\":\"next\"}"));
            assertInvalidParams(session.read());
            session.send(request("c:read-after", "thread/read", "{\"threadId\":\""
                    + threadId + "\",\"afterSeq\":1}"));
            assertInvalidParams(session.read());
            session.send(request("c:read-view", "thread/read", "{\"threadId\":\""
                    + threadId + "\",\"view\":\"events\"}"));
            assertInvalidParams(session.read());
            session.send(turnStart(threadId));
            JsonNode accepted = session.read();
            assertFalse(accepted.has("error"), accepted.toString());
            boolean completed = false;
            while (!completed) {
                JsonNode frame = session.read();
                if ("item/completed".equals(frame.path("method").textValue())) {
                    assertTrue(frame.toString().contains("agent_message"));
                }
                completed = "turn/completed".equals(frame.path("method").textValue());
            }
            session.send(request("c:read", "thread/read", "{\"threadId\":\""
                    + threadId + "\"}"));
            JsonNode read = session.read();
            assertEquals(2, read.path("result").path("items").size());
            assertEquals("user_message", read.path("result").path("items").get(0)
                    .path("kind").textValue());
            assertEquals("agent_message", read.path("result").path("items").get(1)
                    .path("kind").textValue());
            session.shutdown();
        }

        try (SqlitePersistence reopened = SqlitePersistence.open(config)) {
            var restored = reopened.history().readThread(threadId).orElseThrow();
            assertEquals(2, restored.items().size());
            assertTrue(restored.thread().lastSeq() >= 2);
        }
    }

    /** Verifies the frozen workspace text bounds before path resolution or SQLite persistence. */
    @Test
    void workspaceOpenRejectsEmptyAndOverlongWireText(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        SqlitePersistenceConfig config = SqlitePersistenceConfig.of(
                data.resolve("ja.sqlite"), data.resolve("ja.sqlite.bak"));
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            Session session = new Session(persistence, workspace);
            session.send(initializeFrame());
            assertEquals("c:init", session.read().path("id").textValue());
            session.send(initializedFrame());
            assertEquals("ready", session.read().path("params").path("status").textValue());

            session.send(request("c:empty", "workspace/open", "{"
                    + "\"workspaceId\":\"ws_bounds\",\"rootPath\":\"\","
                    + "\"trust\":\"trusted\"}"));
            assertInvalidParams(session.read());
            session.send(request("c:root-long", "workspace/open", "{"
                    + "\"workspaceId\":\"ws_bounds\",\"rootPath\":\""
                    + "x".repeat(4_097) + "\",\"trust\":\"trusted\"}"));
            assertInvalidParams(session.read());
            session.send(request("c:name-long", "workspace/open", "{"
                    + "\"workspaceId\":\"ws_bounds\",\"rootPath\":\""
                    + escape(workspace) + "\",\"displayName\":\""
                    + "x".repeat(257) + "\",\"trust\":\"trusted\"}"));
            assertInvalidParams(session.read());
            session.shutdown();
        }
    }

    /** Drops producer events when the durable user-item admission fails after runtime.start. */
    @Test
    void failedHistoryAdmissionDoesNotLeakTurnEvents(@TempDir Path temp) throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        SqlitePersistenceConfig config = SqlitePersistenceConfig.of(
                data.resolve("ja.sqlite"), data.resolve("ja.sqlite.bak"));
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            AdmissionFailureRuntime failingRuntime = new AdmissionFailureRuntime(persistence);
            Session session = new Session(persistence, workspace, failingRuntime);
            session.send(initializeFrame());
            assertEquals("c:init", session.read().path("id").textValue());
            session.send(initializedFrame());
            assertEquals("ready", session.read().path("params").path("status").textValue());
            session.send(request("c:workspace", "workspace/open", "{"
                    + "\"workspaceId\":\"ws_failure\",\"rootPath\":\""
                    + escape(workspace) + "\",\"trust\":\"trusted\"}"));
            assertEquals("c:workspace", session.read().path("id").textValue());
            session.send(request("c:create", "thread/create", "{\"workspaceId\":\"ws_failure\","
                    + "\"title\":\"Failure\"}"));
            String threadId = session.read().path("result").path("thread").path("threadId")
                    .textValue();
            session.send(turnStart(threadId));
            JsonNode failure = session.read();
            assertTrue(failure.has("error"), failure.toString());
            assertTrue(failingRuntime.cancelled.get());
            session.shutdown();
        }
    }

    /** Keeps protocol-boundary assertions independent from the server's internal exception text. */
    private static void assertInvalidParams(JsonNode frame) {
        assertEquals("INVALID_PARAMS", frame.path("error").path("data").path("jaCode").textValue(),
                frame.toString());
    }

    /** Keeps one pipe session isolated so stdout remains the only parsed protocol surface. */
    private static final class Session implements AutoCloseable {
        private final PipedOutputStream clientInput;
        private final PipedInputStream serverInput;
        private final PipedOutputStream serverOutput;
        private final PipedInputStream clientOutput;
        private final BufferedWriter input;
        private final BufferedReader output;
        private final StdioRuntime runtime;
        private final CompletableFuture<Integer> exit;

        /** Opens the injected history adapter without letting the runtime create a second owner. */
        private Session(SqlitePersistence persistence, Path workspace) throws Exception {
            this(persistence, workspace, null);
        }

        /** Injects a controlled runtime for admission-failure ordering tests. */
        private Session(SqlitePersistence persistence, Path workspace,
                        TurnRuntime injectedTurnRuntime) throws Exception {
            clientInput = new PipedOutputStream();
            serverInput = new PipedInputStream(clientInput, 64 * 1024);
            serverOutput = new PipedOutputStream();
            clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
            input = new BufferedWriter(new OutputStreamWriter(clientInput, StandardCharsets.UTF_8));
            output = new BufferedReader(new InputStreamReader(clientOutput, StandardCharsets.UTF_8));
            runtime = new StdioRuntime(serverInput, serverOutput,
                    new SidecarConfiguration(SidecarConfiguration.RuntimeMode.FAKE),
                    Clock.systemUTC(), injectedTurnRuntime, null, persistence.history());
            exit = CompletableFuture.supplyAsync(runtime::run);
        }

        /** Writes one JSONL request and flushes it before waiting for a response. */
        private void send(String frame) throws Exception {
            input.write(frame);
            input.write('\n');
            input.flush();
        }

        /** Reads one response/event and fails if the sidecar closes stdout unexpectedly. */
        private JsonNode read() throws Exception {
            String line = output.readLine();
            if (line == null) {
                throw new AssertionError("JA closed stdout before a JSONL frame");
            }
            return JSON.readTree(line);
        }

        /** Sends shutdown and waits for the same clean zero exit used by the desktop host. */
        private void shutdown() throws Exception {
            send(request("c:shutdown", "shutdown", "{}"));
            assertEquals("c:shutdown", read().path("id").textValue());
            input.close();
            exit.get();
            output.close();
        }

        /** Closes pipes and runtime if an assertion interrupts the normal shutdown path. */
        @Override
        public void close() {
            runtime.close();
            try {
                input.close();
                output.close();
            } catch (Exception ignored) {
                // The test's primary assertion is more useful than pipe cleanup noise.
            }
        }
    }

    /** Builds the protocol handshake accepted by the existing strict codec. */
    private static String initializeFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:init\",\"method\":\"initialize\","
                + "\"params\":{\"protocolMajor\":1,\"protocolMinor\":0,"
                + "\"minimumCompatibleMinor\":0,\"clientVersion\":\"history-test\","
                + "\"capabilities\":{\"methods\":[\"initialize\",\"workspace/open\","
                + "\"thread/create\",\"thread/read\",\"turn/start\",\"shutdown\"],"
                + "\"events\":[\"runtime/statusChanged\",\"turn/started\",\"item/started\","
                + "\"item/delta\",\"item/completed\",\"turn/completed\"],"
                + "\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"],"
                + "\"itemKinds\":[\"user_message\",\"agent_message\"],\"mcp\":{"
                + "\"protocolVersions\":[],\"transports\":[],\"features\":[]}},"
                + "\"limits\":{\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,"
                + "\"maxOutboundQueueFrames\":1024,\"maxInFlightRequests\":64,"
                + "\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536,"
                + "\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576,"
                + "\"defaultRequestDeadlineMs\":120000,\"defaultApprovalDeadlineMs\":300000}}}";
    }

    /** Uses the frozen ready token so this test exercises the real handshake state machine. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{"
                + "\"readyToken\":\"" + READY_TOKEN + "\"}}";
    }

    /** Builds the minimal request envelope while keeping params JSON explicit. */
    private static String request(String id, String method, String params) {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"" + id + "\",\"method\":\""
                + method + "\",\"params\":" + params + "}";
    }

    /** Sends a plain text turn so the fake runtime emits user and agent snapshots. */
    private static String turnStart(String threadId) {
        return request("c:turn", "turn/start", "{\"threadId\":\"" + threadId + "\","
                + "\"input\":[{\"type\":\"text\",\"text\":\"inspect source\"}],"
                + "\"accessMode\":\"workspace\",\"profileRevision\":\"profile_history\"}");
    }

    /** Escapes only the path characters that can occur in a Windows JSON string. */
    private static String escape(Path path) {
        return path.toString().replace("\\", "\\\\").replace("\"", "\\\"");
    }

    /** Closes history immediately after start so the gate is exercised at its narrow failure edge. */
    private static final class AdmissionFailureRuntime implements TurnRuntime {
        private final SqlitePersistence persistence;
        private final AtomicBoolean cancelled = new AtomicBoolean();

        /** Keeps the test runtime tied to the exact owner that turn/start is about to write. */
        private AdmissionFailureRuntime(SqlitePersistence persistence) {
            this.persistence = persistence;
        }

        /** Emits one event concurrently, then closes the owner to force user-item admission fail. */
        @Override
        public TurnHandle start(RpcRequest request, Consumer<TurnEvent> eventPublisher) {
            TurnId turnId = new TurnId("turn_failure");
            Thread.startVirtualThread(() -> {
                var params = io.github.kongweiguang.ja.protocol.JsonNodes.object();
                params.put("threadId", request.params().path("threadId").textValue());
                var turn = io.github.kongweiguang.ja.protocol.JsonNodes.object();
                turn.put("turnId", turnId.value());
                params.set("turn", turn);
                eventPublisher.accept(new TurnEvent("turn/started", params));
            });
            persistence.close();
            return new TurnHandle(turnId);
        }

        /** Records the best-effort cancellation requested after the durable admission failure. */
        @Override
        public TurnRuntime.CancelResult cancel(String threadId, TurnId turnId, String reason) {
            cancelled.set(true);
            return new TurnRuntime.CancelResult(true, turnId, "interrupted");
        }

        /** Enables the transport to exercise its cleanup path without changing TurnRuntime. */
        @Override
        public boolean supportsCancellation() {
            return true;
        }

        /** Rejects no additional work because this fixture owns no worker pool. */
        @Override
        public void stopAccepting() {
        }

        /** The fixture has no asynchronous work that must be drained. */
        @Override
        public boolean awaitQuiescence(Duration timeout) {
            return true;
        }

        /** The persistence owner is closed by start; there are no other resources here. */
        @Override
        public void close() {
        }
    }
}
