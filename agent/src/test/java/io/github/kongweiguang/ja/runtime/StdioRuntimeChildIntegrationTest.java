// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.github.kongweiguang.ja.App;
import org.junit.jupiter.api.Test;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.OutputStream;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.time.Clock;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.FutureTask;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Exercises the real App child so JVM wiring and stdout/stderr ownership stay observable. */
class StdioRuntimeChildIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";

    /** Verifies initialize/ready/fake Turn/shutdown through an actual Java child process. */
    @Test
    void childCompletesHandshakeFakeTurnAndCleanShutdown() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Process process = startChild(java);
        CompletableFuture<String> stderr = CompletableFuture.supplyAsync(() -> readAll(process));
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     process.getInputStream(), StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            // Pipeline initialized to prove the reader never relies on handler timing.
            send(input, "{" +
                    "\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{" +
                    "\"readyToken\":\"" + READY_TOKEN + "\"}}");
            JsonNode initialize = readJson(output);
            assertEquals("c:init", initialize.path("id").textValue());
            assertEquals(1, initialize.path("result").path("protocolMajor").intValue());
            assertEquals("native-image", initialize.path("result").path("runtime").path("kind").textValue());

            JsonNode ready = readJson(output);
            assertEquals("runtime/statusChanged", ready.path("method").textValue());
            assertEquals("ready", ready.path("params").path("status").textValue());
            assertEquals(READY_TOKEN, ready.path("params").path("readyToken").textValue());

            send(input, turnStartFrame());
            JsonNode turnResponse = readJson(output);
            assertEquals("c:turn", turnResponse.path("id").textValue());
            String turnId = turnResponse.path("result").path("turnId").textValue();
            assertTrue(turnId.startsWith("turn_"));
            List<String> methods = new ArrayList<>();
            for (int index = 0; index < 5; index++) {
                JsonNode event = readJson(output);
                methods.add(event.path("method").textValue());
                if ("turn/completed".equals(event.path("method").textValue())) {
                    assertEquals(turnId, event.path("params").path("turn").path("turnId").textValue());
                    assertEquals("completed", event.path("params").path("terminalStatus").textValue());
                }
            }
            assertEquals(List.of("turn/started", "item/started", "item/delta",
                    "item/completed", "turn/completed"), methods);

            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");
            JsonNode shutdown = readJson(output);
            assertEquals("c:stop", shutdown.path("id").textValue());
            assertEquals("shutting_down", shutdown.path("result").path("status").textValue());
        }
        assertTrue(process.waitFor(5, TimeUnit.SECONDS));
        assertEquals(0, process.exitValue());
        String diagnostic = stderr.get(5, TimeUnit.SECONDS);
        assertFalse(diagnostic.contains("Fake response"));
    }

    /** Verifies explicit fake data roots persist the history surface across two real App children. */
    @Test
    void childFakeHistoryPersistsOnlyWithExplicitDataDirectory() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Path root = Files.createTempDirectory("ja-fake-history-child-");
        Path workspace = Files.createDirectory(root.resolve("workspace"));
        Path data = root.resolve("data");
        exerciseHistoryChild(java, workspace, data, false);
        exerciseHistoryChild(java, workspace, data, true);
        assertTrue(Files.isDirectory(data));

        // A fake process without an explicit data root remains ephemeral and reports the frozen
        // Java-side INVALID_STATE instead of silently selecting a repository/temp database.
        Process process = startChild(java);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     process.getInputStream(), StandardCharsets.UTF_8))) {
            send(input, historyInitializeFrame());
            send(input, initializedFrame());
            assertEquals("c:init", readJson(output).path("id").textValue());
            assertEquals("ready", readJson(output).path("params").path("status").textValue());
            send(input, historyRequest("c:ephemeral-list", "workspace/list", Map.of()));
            JsonNode unavailable = readJson(output);
            assertEquals("INVALID_STATE", unavailable.path("error").path("data")
                    .path("jaCode").textValue());
            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\","
                    + "\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:stop", readJson(output).path("id").textValue());
        } finally {
            if (process.isAlive() && !process.waitFor(5, TimeUnit.SECONDS)) {
                process.destroyForcibly();
            }
        }
        assertTrue(process.waitFor(5, TimeUnit.SECONDS));
        assertEquals(0, process.exitValue());
    }

    /** Runs one real child against the same history database, optionally asserting restart data. */
    private static void exerciseHistoryChild(String java, Path workspace, Path data,
                                              boolean expectExisting) throws Exception {
        Process process = startChild(java, data);
        BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
        BufferedReader output = new BufferedReader(new InputStreamReader(
                process.getInputStream(), StandardCharsets.UTF_8));
        try {
            send(input, historyInitializeFrame());
            send(input, initializedFrame());
            assertEquals("c:init", readJson(output).path("id").textValue());
            assertEquals("ready", readJson(output).path("params").path("status").textValue());

            send(input, workspaceOpenFrame(workspace));
            assertEquals("c:workspace", readJson(output).path("id").textValue());
            send(input, historyRequest("c:workspace-list", "workspace/list", Map.of()));
            JsonNode workspaces = readJson(output);
            assertEquals(1, workspaces.path("result").path("workspaces").size());

            if (!expectExisting) {
                send(input, historyRequest("c:create", "thread/create", Map.of(
                        "workspaceId", "ws_fake_activation", "title", "Persisted history")));
                JsonNode created = readJson(output);
                assertEquals("c:create", created.path("id").textValue());
                assertTrue(created.path("result").path("thread").path("threadId")
                        .textValue().startsWith("thr_"));
            }
            send(input, historyRequest("c:thread-list", "thread/list", Map.of(
                    "workspaceId", "ws_fake_activation")));
            JsonNode threads = readJson(output);
            assertEquals(1, threads.path("result").path("threads").size());
            String threadId = threads.path("result").path("threads").get(0)
                    .path("threadId").textValue();
            send(input, historyRequest("c:thread-read", "thread/read", Map.of(
                    "threadId", threadId)));
            JsonNode read = readJson(output);
            assertEquals(threadId, read.path("result").path("thread").path("threadId")
                    .textValue());
            assertEquals(0, read.path("result").path("items").size());

            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\","
                    + "\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:stop", readJson(output).path("id").textValue());
        } finally {
            input.close();
            output.close();
            // The shutdown ACK is queued before the bounded drain; allow the real child to
            // finish its SQLite close before treating it as stuck and forcing termination.
            if (process.isAlive() && !process.waitFor(5, TimeUnit.SECONDS)) {
                process.destroyForcibly();
            }
        }
        assertTrue(process.waitFor(5, TimeUnit.SECONDS));
        assertEquals(0, process.exitValue());
    }

    /** Verifies an identical client request id cannot admit a second fake turn. */
    @Test
    void duplicateTurnRequestIdIsRejectedWithoutSecondTurn() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Process process = startChild(java);
        BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
        BufferedReader output = new BufferedReader(new InputStreamReader(
                process.getInputStream(), StandardCharsets.UTF_8));
        try {
            send(input, initializeFrame());
            send(input, initializedFrame());
            assertEquals("c:init", readJson(output).path("id").textValue());
            assertEquals("ready", readJson(output).path("params").path("status").textValue());
            String duplicate = turnStartFrame("c:duplicate", "thr_duplicate", "duplicate request");
            send(input, duplicate);
            send(input, duplicate);

            int accepted = 0;
            int terminals = 0;
            for (int index = 0; index < 16 && (accepted == 0 || terminals == 0); index++) {
                JsonNode frame = readJson(output);
                if ("c:duplicate".equals(frame.path("id").textValue())) {
                    if (frame.has("result")) {
                        accepted++;
                    }
                }
                if ("turn/completed".equals(frame.path("method").textValue())) {
                    terminals++;
                }
            }
            assertEquals(1, accepted);
            assertEquals(1, terminals);

            // Once the original result is queued, the same id is a replay and receives one
            // stable duplicate error; it must not start a second fake turn.
            send(input, duplicate);
            JsonNode replay = readJson(output);
            assertEquals("c:duplicate", replay.path("id").textValue());
            assertEquals("DUPLICATE_REQUEST", replay.path("error").path("data")
                    .path("jaCode").textValue());
            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:stop", readJson(output).path("id").textValue());
            assertTrue(process.waitFor(5, TimeUnit.SECONDS));
            assertEquals(0, process.exitValue());
        } finally {
            input.close();
            output.close();
            if (process.isAlive()) {
                process.destroyForcibly();
            }
        }
    }

    /** Verifies business requests are rejected before ready and EOF still exits normally. */
    @Test
    void childRejectsTurnBeforeHandshakeAndExitsOnEof() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Process process = startChild(java);
        CompletableFuture<String> stderr = CompletableFuture.supplyAsync(() -> readAll(process));
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     process.getInputStream(), StandardCharsets.UTF_8))) {
            send(input, turnStartFrame());
            JsonNode error = readJson(output);
            assertEquals("c:turn", error.path("id").textValue());
            assertEquals("NOT_INITIALIZED", error.path("error").path("data").path("jaCode").textValue());
        }
        assertTrue(process.waitFor(5, TimeUnit.SECONDS));
        assertEquals(0, process.exitValue());
        assertFalse(stderr.get(5, TimeUnit.SECONDS).contains("stdout"));
    }

    /**
     * Proves a worker that is already interrupted before publishing its terminal event cannot
     * lose that event while waiting for the start ACK. The gate makes cancellation deterministic
     * without sleeping, and the injected runtime keeps this test on the real stdio writer path.
     */
    @Test
    void interruptedWorkerPublishesExactlyOneTerminalAfterStartAck() throws Exception {
        InterruptedTerminalRuntime turnRuntime = new InterruptedTerminalRuntime();
        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        QueuedOutputStream serverOutput = new QueuedOutputStream();
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new io.github.kongweiguang.ja.bootstrap.SidecarConfiguration(
                        io.github.kongweiguang.ja.bootstrap.SidecarConfiguration.RuntimeMode.FAKE),
                Clock.systemUTC(), turnRuntime);
        CompletableFuture<Integer> result = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8))) {
            send(input, activationInitializeFrame());
            send(input, initializedFrame());
            assertEquals("c:init", readQueuedJson(serverOutput).path("id").textValue());
            assertEquals("ready", readQueuedJson(serverOutput).path("params").path("status").textValue());

            send(input, turnStartFrame("c:cancel-order", "thr_cancel_order", "cancel me"));
            JsonNode startResponse = readQueuedJson(serverOutput);
            assertEquals("c:cancel-order", startResponse.path("id").textValue());
            String turnId = startResponse.path("result").path("turnId").textValue();
            assertEquals("turn_interrupt_fixture", turnId);

            send(input, turnCancelFrame("c:cancel-order-ack", "thr_cancel_order", turnId));
            int terminalEvents = 0;
            JsonNode cancelResponse = null;
            JsonNode terminal = null;
            for (int index = 0; index < 4 && (cancelResponse == null || terminal == null); index++) {
                JsonNode frame = readQueuedJson(serverOutput);
                if ("c:cancel-order-ack".equals(frame.path("id").textValue())) {
                    cancelResponse = frame;
                }
                if ("turn/completed".equals(frame.path("method").textValue())) {
                    terminalEvents++;
                    terminal = frame;
                }
            }
            assertNotNull(cancelResponse, "cancel ACK was not observed");
            assertEquals(turnId, cancelResponse.path("result").path("turnId").textValue());
            assertEquals("interrupting", cancelResponse.path("result").path("status").textValue());
            assertNotNull(terminal, "interrupted terminal event was not observed");
            assertEquals(1, terminalEvents, "accepted turn must publish one terminal event");
            assertEquals(turnId, terminal.path("params").path("turn").path("turnId").textValue());
            assertEquals("interrupted", terminal.path("params").path("terminalStatus").textValue());

            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:stop", readQueuedJson(serverOutput).path("id").textValue());
        } finally {
            clientInput.close();
            runtime.close();
        }
        assertEquals(0, result.get(5, TimeUnit.SECONDS));
    }

    /**
     * Replays the desktop's fake-sidecar activation sequence in a real child.  The fixture must
     * validate the same workspace/profile/data prerequisites as production while avoiding any
     * provider or MCP claim; this keeps Rust/Tauri contract tests deterministic and credential-free.
     */
    @Test
    void childActivatesFakeProfileThenCancelsTurnAndShutsDown() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Path root = Files.createTempDirectory("ja-fake-child-");
        Path workspace = Files.createDirectory(root.resolve("workspace"));
        Path data = root.resolve("data");
        assertFalse(Files.exists(data));
        Process process = startChild(java, data);
        CompletableFuture<String> stderr = CompletableFuture.supplyAsync(() -> readAll(process));
        BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
        BufferedReader output = new BufferedReader(new InputStreamReader(
                process.getInputStream(), StandardCharsets.UTF_8));
        try {
            send(input, activationInitializeFrame());
            send(input, initializedFrame());
            assertEquals("c:init", readJson(output).path("id").textValue());
            assertEquals("ready", readJson(output).path("params").path("status").textValue());

            send(input, workspaceOpenFrame(workspace));
            assertEquals("c:workspace", readJson(output).path("id").textValue());
            send(input, profileSaveFrame("profile_fake_activation"));
            assertEquals("c:profile", readJson(output).path("id").textValue());

            send(input, profileActivateFrame("c:wrong", "profile_wrong"));
            assertEquals("CONFLICT", readJson(output).path("error").path("data")
                    .path("jaCode").textValue());
            send(input, profileActivateFrame("c:activate", "profile_fake_activation"));
            JsonNode activated = readJson(output);
            assertEquals("c:activate", activated.path("id").textValue());
            assertEquals("profile_fake_activation", activated.path("result")
                    .path("activeProfileRevision").textValue());
            send(input, profileActivateFrame("c:activate-retry", "profile_fake_activation"));
            JsonNode replay = readJson(output);
            assertEquals("c:activate-retry", replay.path("id").textValue());
            assertEquals("profile_fake_activation", replay.path("result")
                    .path("activeProfileRevision").textValue());
            assertTrue(Files.isDirectory(data, LinkOption.NOFOLLOW_LINKS));

            send(input, fakeActivatedTurnStartFrame("c:turn", "thr_fake_activation", "hello activation"));
            JsonNode accepted = readJson(output);
            assertEquals("c:turn", accepted.path("id").textValue());
            String turnId = accepted.path("result").path("turnId").textValue();
            assertTrue(turnId.startsWith("turn_fake_"));
            JsonNode started = readJson(output);
            assertEquals("turn/started", started.path("method").textValue());
            send(input, turnCancelFrame(turnId));

            boolean cancelSeen = false;
            boolean terminalSeen = false;
            for (int index = 0; index < 64 && !(cancelSeen && terminalSeen); index++) {
                JsonNode event = readJson(output);
                if ("c:cancel".equals(event.path("id").textValue())) {
                    cancelSeen = true;
                    if (event.has("error")) {
                        String code = event.path("error").path("data").path("jaCode").textValue();
                        assertTrue("TURN_NOT_FOUND".equals(code) || "TURN_NOT_ACTIVE".equals(code),
                                event.toString());
                    } else {
                        assertEquals(turnId, event.path("result").path("turnId").textValue());
                    }
                }
                if ("turn/completed".equals(event.path("method").textValue())) {
                    terminalSeen = true;
                    assertEquals(turnId, event.path("params").path("turn").path("turnId")
                            .textValue());
                }
            }
            assertTrue(cancelSeen, "cancel response was not observed");
            assertTrue(terminalSeen, "turn terminal event was not observed");
            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:stop", readJson(output).path("id").textValue());
        } finally {
            if (process.isAlive() && !process.waitFor(5, TimeUnit.SECONDS)) {
                process.destroyForcibly();
            }
            process.waitFor(5, TimeUnit.SECONDS);
            input.close();
            output.close();
        }
        assertTrue(process.waitFor(5, TimeUnit.SECONDS));
        assertEquals(0, process.exitValue());
        assertFalse(stderr.get(5, TimeUnit.SECONDS).contains("stdout"));
    }

    /** Verifies fake activation rejects a data file and a final symlink with the stable state error. */
    @Test
    void childRejectsInvalidFakeDataDirectories() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Path root = Files.createTempDirectory("ja-fake-invalid-data-");
        Path workspace = Files.createDirectory(root.resolve("workspace"));
        Path dataFile = Files.createFile(root.resolve("data-file"));
        assertEquals("INVALID_STATE", activateFakeProfile(java, workspace, dataFile));

        Path target = Files.createDirectory(root.resolve("data-target"));
        Path symlink = root.resolve("data-link");
        try {
            Files.createSymbolicLink(symlink, target);
        } catch (UnsupportedOperationException | java.io.IOException | SecurityException unsupported) {
            // Windows environments without symlink privilege still exercise the file boundary.
            return;
        }
        assertEquals("INVALID_STATE", activateFakeProfile(java, workspace, symlink));
    }

    /** Replays activation up to the data-root decision and then shuts down the isolated child. */
    private static String activateFakeProfile(String java, Path workspace, Path dataDirectory)
            throws Exception {
        Process process = startChild(java, dataDirectory);
        BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
        BufferedReader output = new BufferedReader(new InputStreamReader(
                process.getInputStream(), StandardCharsets.UTF_8));
        try {
            send(input, activationInitializeFrame());
            send(input, initializedFrame());
            assertEquals("c:init", readJson(output).path("id").textValue());
            assertEquals("ready", readJson(output).path("params").path("status").textValue());
            send(input, workspaceOpenFrame(workspace));
            assertEquals("c:workspace", readJson(output).path("id").textValue());
            send(input, profileSaveFrame("profile_fake_invalid_data"));
            assertEquals("c:profile", readJson(output).path("id").textValue());
            send(input, profileActivateFrame("c:activate", "profile_fake_invalid_data"));
            JsonNode activation = readJson(output);
            assertEquals("c:activate", activation.path("id").textValue());
            assertTrue(activation.has("error"), activation.toString());
            String code = activation.path("error").path("data").path("jaCode").textValue();
            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:stop", readJson(output).path("id").textValue());
            assertTrue(process.waitFor(5, TimeUnit.SECONDS));
            assertEquals(0, process.exitValue());
            return code;
        } finally {
            if (process.isAlive() && !process.waitFor(5, TimeUnit.SECONDS)) {
                process.destroyForcibly();
            }
            process.waitFor(5, TimeUnit.SECONDS);
            input.close();
            output.close();
        }
    }

    /** Verifies shutdown exits with stdin still open and drains an accepted turn terminal. */
    @Test
    void childShutdownExitsWithOpenStdinAndDrainsAcceptedTurn() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Process process = startChild(java);
        CompletableFuture<String> stderr = CompletableFuture.supplyAsync(() -> readAll(process));
        BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
        BufferedReader output = new BufferedReader(new InputStreamReader(
                process.getInputStream(), StandardCharsets.UTF_8));
        try {
            send(input, initializeFrame());
            send(input, initializedFrame());
            assertEquals("c:init", readJson(output).path("id").textValue());
            assertEquals("ready", readJson(output).path("params").path("status").textValue());

            send(input, turnStartFrame("thr_open", "accepted before shutdown"));
            JsonNode accepted = readJson(output);
            assertEquals("c:turn", accepted.path("id").textValue());
            String turnId = accepted.path("result").path("turnId").textValue();
            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");

            boolean shutdownSeen = false;
            boolean terminalSeen = false;
            for (int index = 0; index < 6; index++) {
                JsonNode event = readJson(output);
                if ("c:stop".equals(event.path("id").textValue())) {
                    shutdownSeen = true;
                }
                if ("turn/completed".equals(event.path("method").textValue())) {
                    terminalSeen = true;
                    assertEquals(turnId, event.path("params").path("turn").path("turnId").textValue());
                }
            }
            assertTrue(shutdownSeen);
            assertTrue(terminalSeen);

            // Deliberately leave input open while waiting: the sidecar must
            // use its lifecycle latch, not close(System.in), to terminate.
            assertTrue(process.waitFor(5, TimeUnit.SECONDS));
            assertEquals(0, process.exitValue());
            assertFalse(stderr.get(5, TimeUnit.SECONDS).contains("stdout"));
        } finally {
            input.close();
            output.close();
            if (process.isAlive()) {
                process.destroyForcibly();
            }
        }
    }

    /** Verifies large Unicode output is split by UTF-8 bytes and preserves trailing whitespace. */
    @Test
    void childStreamsLargeUnicodeWithCompleteFinalItem() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Process process = startChild(java);
        BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
        BufferedReader output = new BufferedReader(new InputStreamReader(
                process.getInputStream(), StandardCharsets.UTF_8));
        try {
            send(input, initializeFrame());
            send(input, initializedFrame());
            readJson(output);
            readJson(output);
            String text = "界".repeat(80_000) + "尾部空白  ";
            send(input, turnStartFrame("thr_unicode", text));
            JsonNode accepted = readJson(output);
            assertEquals("c:turn", accepted.path("id").textValue());
            StringBuilder deltas = new StringBuilder();
            int deltaCount = 0;
            boolean completed = false;
            while (!completed) {
                JsonNode event = readJson(output);
                if ("item/delta".equals(event.path("method").textValue())) {
                    String delta = event.path("params").path("delta").textValue();
                    assertTrue(delta.getBytes(StandardCharsets.UTF_8).length <= 65_536);
                    deltas.append(delta);
                    deltaCount++;
                }
                if ("item/completed".equals(event.path("method").textValue())) {
                    assertEquals(deltas.toString(), event.path("params").path("item").path("text").textValue());
                }
                if ("turn/completed".equals(event.path("method").textValue())) {
                    completed = true;
                }
            }
            assertTrue(deltaCount > 1);
            assertTrue(deltas.toString().endsWith("尾部空白  "));
            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:stop", readJson(output).path("id").textValue());
            assertTrue(process.waitFor(5, TimeUnit.SECONDS));
            assertEquals(0, process.exitValue());
        } finally {
            input.close();
            output.close();
            if (process.isAlive()) {
                process.destroyForcibly();
            }
        }
    }

    /** Verifies a pipelined shutdown cannot overtake the ready notification. */
    @Test
    void pipelinedShutdownWaitsForReadyOnControlLane() throws Exception {
        String java = Path.of(System.getProperty("java.home"), "bin", "java.exe").toString();
        Process process = startChild(java);
        BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                process.getOutputStream(), StandardCharsets.UTF_8));
        BufferedReader output = new BufferedReader(new InputStreamReader(
                process.getInputStream(), StandardCharsets.UTF_8));
        try {
            send(input, initializeFrame());
            send(input, initializedFrame());
            send(input, "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}");
            assertEquals("c:init", readJson(output).path("id").textValue());
            assertEquals("ready", readJson(output).path("params").path("status").textValue());
            assertEquals("c:stop", readJson(output).path("id").textValue());
            assertTrue(process.waitFor(5, TimeUnit.SECONDS));
            assertEquals(0, process.exitValue());
        } finally {
            input.close();
            output.close();
            if (process.isAlive()) {
                process.destroyForcibly();
            }
        }
    }

    /** Builds the exact initialize shape accepted by the frozen protocol. */
    private static String initializeFrame() {
        return "{" +
                "\"jsonrpc\":\"2.0\",\"id\":\"c:init\",\"method\":\"initialize\",\"params\":{" +
                "\"protocolMajor\":1,\"protocolMinor\":0,\"minimumCompatibleMinor\":0," +
                "\"clientVersion\":\"child-test\",\"capabilities\":{" +
                "\"methods\":[\"initialize\",\"turn/start\",\"shutdown\"]," +
                "\"events\":[\"runtime/statusChanged\",\"turn/started\",\"item/started\",\"item/delta\",\"item/completed\",\"turn/completed\"]," +
                "\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"]," +
                "\"itemKinds\":[\"agent_message\"],\"mcp\":{" +
                "\"protocolVersions\":[],\"transports\":[],\"features\":[]}}," +
                "\"limits\":{" +
                "\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024," +
                "\"maxInFlightRequests\":64,\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536," +
                "\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576," +
                "\"defaultRequestDeadlineMs\":120000,\"defaultApprovalDeadlineMs\":300000}}}";
    }

    /** Extends the child handshake with the minimal persisted-history methods under test. */
    private static String historyInitializeFrame() {
        return initializeFrame().replace("\"methods\":[\"initialize\",\"turn/start\",\"shutdown\"]",
                "\"methods\":[\"initialize\",\"workspace/open\",\"workspace/list\","
                        + "\"thread/create\",\"thread/list\",\"thread/read\",\"shutdown\"]");
    }

    /** Builds one history request with a typed JSON params object and no second protocol client. */
    private static String historyRequest(String id, String method,
                                         Map<String, ?> params) throws Exception {
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", method, "params", params));
    }

    /** Extends the child handshake only for the activation replay without changing the baseline fixture. */
    private static String activationInitializeFrame() {
        return initializeFrame().replace("\"methods\":[\"initialize\",\"turn/start\",\"shutdown\"]",
                "\"methods\":[\"initialize\",\"workspace/open\",\"profile/save\","
                        + "\"profile/activate\",\"turn/start\",\"turn/cancel\",\"shutdown\"]");
    }

    /** Builds a workspace binding frame so fake activation cannot bypass the canonical root check. */
    private static String workspaceOpenFrame(Path workspace) throws Exception {
        return JSON.writeValueAsString(java.util.Map.of("jsonrpc", "2.0", "id", "c:workspace",
                "method", "workspace/open", "params", java.util.Map.of(
                        "workspaceId", "ws_fake_activation", "rootPath", workspace.toString(),
                        "trust", "trusted", "displayName", "fake activation workspace")));
    }

    /** Builds a credential-free profile because fake mode must reject credentials instead of faking them. */
    private static String profileSaveFrame(String revision) throws Exception {
        return JSON.writeValueAsString(java.util.Map.of("jsonrpc", "2.0", "id", "c:profile",
                "method", "profile/save", "params", java.util.Map.of("profile", java.util.Map.of(
                        "profileRevision", revision, "name", "Fake activation profile",
                        "accessMode", "workspace", "model", java.util.Map.of(
                                "provider", "openai", "protocol", "openai_chat_completions",
                                "model", "fixture-model")))));
    }

    /** Builds activation requests whose revision semantics are checked by both the first and replay calls. */
    private static String profileActivateFrame(String id, String revision) throws Exception {
        return JSON.writeValueAsString(java.util.Map.of("jsonrpc", "2.0", "id", id,
                "method", "profile/activate", "params", java.util.Map.of(
                        "profileRevision", revision)));
    }

    /** Builds the same turn input Rust uses after activation so cancel and terminal ordering are observable. */
    private static String fakeActivatedTurnStartFrame(String id, String threadId, String text)
            throws Exception {
        return JSON.writeValueAsString(java.util.Map.of("jsonrpc", "2.0", "id", id,
                "method", "turn/start", "params", java.util.Map.of(
                        "threadId", threadId, "accessMode", "workspace",
                        "profileRevision", "profile_fake_activation", "input", List.of(
                                java.util.Map.of("type", "text", "text", text)))));
    }

    /** Builds an exact thread/turn cancel request so the terminal event remains the authority. */
    private static String turnCancelFrame(String turnId) throws Exception {
        return turnCancelFrame("c:cancel", "thr_fake_activation", turnId);
    }

    /** Builds a cancel request with explicit identity so the ACK can be checked independently. */
    private static String turnCancelFrame(String id, String threadId, String turnId)
            throws Exception {
        return JSON.writeValueAsString(java.util.Map.of("jsonrpc", "2.0", "id", id,
                "method", "turn/cancel", "params", java.util.Map.of(
                        "threadId", threadId, "turnId", turnId)));
    }

    /** Builds the handshake notification used after initialize is acknowledged. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{" +
                "\"readyToken\":\"" + READY_TOKEN + "\"}}";
    }

    /** Uses the packaged executable jar when the caller supplies one, otherwise test classes. */
    private static Process startChild(String java) throws Exception {
        return startChild(java, null);
    }

    /** Adds an explicit data directory only when a child replay exercises production-shaped activation. */
    private static Process startChild(String java, Path dataDirectory) throws Exception {
        String jar = System.getProperty("ja.test.jar");
        ProcessBuilder builder = jar == null
                ? new ProcessBuilder(java, "-cp", System.getProperty("java.class.path"),
                App.class.getName(), "--runtime=fake")
                : new ProcessBuilder(java, "-jar", jar, "--runtime=fake");
        if (dataDirectory != null) {
            builder.command().add("--data-dir=" + dataDirectory);
        }
        return builder.redirectErrorStream(false).start();
    }

    /** Builds one valid turn/start request whose text must appear in the fake item. */
    private static String turnStartFrame() throws Exception {
        return turnStartFrame("c:turn", "thr_child", "hello child");
    }

    /** Builds a valid turn request with caller-controlled text for boundary tests. */
    private static String turnStartFrame(String threadId, String text) throws Exception {
        return turnStartFrame("c:turn", threadId, text);
    }

    /** Builds one request with a caller-controlled id for replay protection tests. */
    private static String turnStartFrame(String requestId, String threadId, String text)
            throws Exception {
        return "{\"jsonrpc\":\"2.0\",\"id\":"
                + JSON.writeValueAsString(requestId)
                + ",\"method\":\"turn/start\",\"params\":{" +
                "\"threadId\":" + JSON.writeValueAsString(threadId) +
                ",\"input\":[{\"type\":\"text\",\"text\":" + JSON.writeValueAsString(text) + "}]," +
                "\"accessMode\":\"workspace\",\"profileRevision\":\"profile_child\"}}";
    }

    /** Writes one complete LF frame and flushes so the child reader can progress immediately. */
    private static void send(BufferedWriter input, String frame) throws Exception {
        input.write(frame);
        input.write('\n');
        input.flush();
    }

    /** Reads and parses one stdout line, making stdout purity part of the child contract. */
    private static JsonNode readJson(BufferedReader output) throws Exception {
        FutureTask<String> read = new FutureTask<>(output::readLine);
        Thread.ofVirtual().start(read);
        String line = read.get(3, TimeUnit.SECONDS);
        assertNotNull(line);
        assertFalse(line.isBlank());
        return JSON.readTree(line);
    }

    /** Reads one captured JSONL frame with a bounded poll, avoiding PipedInputStream reader-thread races. */
    private static JsonNode readQueuedJson(QueuedOutputStream output) throws Exception {
        byte[] frame = output.frames.poll(3, TimeUnit.SECONDS);
        assertNotNull(frame, "stdio frame was not published before the deadline");
        String line = new String(frame, StandardCharsets.UTF_8).stripTrailing();
        assertFalse(line.isBlank());
        return JSON.readTree(line);
    }

    /** Captures complete writer frames so ordering can be asserted without adding a second protocol writer. */
    private static final class QueuedOutputStream extends OutputStream {
        private final BlockingQueue<byte[]> frames = new ArrayBlockingQueue<>(32);

        /** Copies each complete frame because StdioWriter owns the producer buffer after enqueue. */
        @Override
        public void write(byte[] bytes, int offset, int length) {
            frames.add(java.util.Arrays.copyOfRange(bytes, offset, offset + length));
        }

        /** Retains OutputStream's required single-byte contract for incidental test writes. */
        @Override
        public void write(int value) {
            write(new byte[]{(byte) value}, 0, 1);
        }

        /** Keeps the test output sink side-effect free when the writer flushes each accepted frame. */
        @Override
        public void flush() {
            // Frames are already visible at write time.
        }
    }

    /**
     * Provides a deterministic turn worker that sets its interrupt flag before invoking the
     * terminal callback. It is intentionally test-only: production cancellation remains owned by
     * the existing TurnRuntime implementations, while this fixture isolates the stdio boundary.
     */
    private static final class InterruptedTerminalRuntime implements TurnRuntime {
        private final CountDownLatch workerReady = new CountDownLatch(1);
        private final CountDownLatch terminalGate = new CountDownLatch(1);
        private final CountDownLatch finished = new CountDownLatch(1);
        private final io.github.kongweiguang.ja.domain.TurnId turnId =
                new io.github.kongweiguang.ja.domain.TurnId("turn_interrupt_fixture");
        private volatile String threadId;
        private volatile Thread worker;
        private volatile boolean accepting = true;

        /** Starts one worker and waits only for its registration so cancellation cannot race setup. */
        @Override
        public TurnHandle start(io.github.kongweiguang.ja.protocol.RpcRequest request,
                                java.util.function.Consumer<TurnEvent> eventPublisher) {
            if (!accepting) {
                throw new io.github.kongweiguang.ja.protocol.ProtocolException(
                        io.github.kongweiguang.ja.protocol.JaErrorCode.SHUTTING_DOWN);
            }
            threadId = request.params().path("threadId").textValue();
            Thread candidate = Thread.ofVirtual().name("ja-test-interrupt-worker").start(() -> {
                workerReady.countDown();
                try {
                    terminalGate.await();
                } catch (InterruptedException exception) {
                    // The callback must observe the same interrupt state that caused cancellation.
                    Thread.currentThread().interrupt();
                    eventPublisher.accept(interruptedTerminal());
                } finally {
                    finished.countDown();
                }
            });
            worker = candidate;
            try {
                if (!workerReady.await(2, TimeUnit.SECONDS)) {
                    throw new AssertionError("test worker did not start");
                }
            } catch (InterruptedException exception) {
                Thread.currentThread().interrupt();
                throw new AssertionError("test worker setup was interrupted", exception);
            }
            return new TurnHandle(turnId);
        }

        /** Interrupts the registered worker after identity validation so the test cannot cancel a different turn. */
        @Override
        public TurnRuntime.CancelResult cancel(String requestedThreadId,
                                               io.github.kongweiguang.ja.domain.TurnId requestedTurnId,
                                               String reason) {
            if (!java.util.Objects.equals(threadId, requestedThreadId)
                    || !turnId.equals(requestedTurnId)) {
                throw new io.github.kongweiguang.ja.protocol.ProtocolException(
                        io.github.kongweiguang.ja.protocol.JaErrorCode.TURN_NOT_FOUND);
            }
            worker.interrupt();
            return new TurnRuntime.CancelResult(true, turnId, "interrupting");
        }

        /** Enables the stdio cancel handler for this focused fixture. */
        @Override
        public boolean supportsCancellation() {
            return true;
        }

        /** Stops further fixture admissions before the stdio shutdown drain. */
        @Override
        public void stopAccepting() {
            accepting = false;
        }

        /** Waits for the one worker with the same bounded semantics as the production port. */
        @Override
        public boolean awaitQuiescence(java.time.Duration timeout) {
            try {
                return finished.await(timeout.toMillis(), TimeUnit.MILLISECONDS);
            } catch (InterruptedException exception) {
                Thread.currentThread().interrupt();
                return false;
            }
        }

        /** Interrupts a leftover fixture worker so a failed assertion cannot leave a test thread behind. */
        @Override
        public void close() {
            accepting = false;
            Thread current = worker;
            if (current != null) {
                current.interrupt();
            }
            try {
                finished.await(2, TimeUnit.SECONDS);
            } catch (InterruptedException exception) {
                Thread.currentThread().interrupt();
            }
        }

        /** Builds the smallest terminal payload accepted by the stdio event envelope. */
        private TurnEvent interruptedTerminal() {
            var params = io.github.kongweiguang.ja.protocol.JsonNodes.object();
            var turn = params.putObject("turn");
            turn.put("turnId", turnId.value());
            turn.put("threadId", threadId);
            turn.put("status", "interrupted");
            params.put("terminalStatus", "interrupted");
            return new TurnEvent("turn/completed", params);
        }
    }

    /** Drains stderr independently so diagnostics never block the child writer. */
    private static String readAll(Process process) {
        try (BufferedReader reader = new BufferedReader(new InputStreamReader(
                process.getErrorStream(), StandardCharsets.UTF_8))) {
            return reader.lines().reduce("", (left, right) -> left + right + "\n");
        } catch (Exception exception) {
            return "stderr-read-failed";
        }
    }
}
