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
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.FutureTask;
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
                "\"permissionModes\":[\"plan\",\"workspace\",\"full_access\"]," +
                "\"itemKinds\":[\"agent_message\"],\"mcp\":{" +
                "\"protocolVersions\":[],\"transports\":[],\"features\":[]}}," +
                "\"limits\":{" +
                "\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024," +
                "\"maxInFlightRequests\":64,\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536," +
                "\"maxInlineToolOutputBytes\":1048576,\"maxArtifactBytes\":268435456,\"maxLogBytes\":1048576," +
                "\"defaultRequestDeadlineMs\":120000,\"defaultApprovalDeadlineMs\":300000}}}";
    }

    /** Builds the handshake notification used after initialize is acknowledged. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{" +
                "\"readyToken\":\"" + READY_TOKEN + "\"}}";
    }

    /** Uses the packaged executable jar when the caller supplies one, otherwise test classes. */
    private static Process startChild(String java) throws Exception {
        String jar = System.getProperty("ja.test.jar");
        ProcessBuilder builder = jar == null
                ? new ProcessBuilder(java, "-cp", System.getProperty("java.class.path"),
                App.class.getName(), "--runtime=fake")
                : new ProcessBuilder(java, "-jar", jar, "--runtime=fake");
        return builder.redirectErrorStream(false).start();
    }

    /** Builds one valid turn/start request whose text must appear in the fake item. */
    private static String turnStartFrame() throws Exception {
        return turnStartFrame("thr_child", "hello child");
    }

    /** Builds a valid turn request with caller-controlled text for boundary tests. */
    private static String turnStartFrame(String threadId, String text) throws Exception {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:turn\",\"method\":\"turn/start\",\"params\":{" +
                "\"threadId\":" + JSON.writeValueAsString(threadId) +
                ",\"input\":[{\"type\":\"text\",\"text\":" + JSON.writeValueAsString(text) + "}]," +
                "\"mode\":\"workspace\",\"permissionMode\":\"ask\",\"profileRevision\":\"profile_child\"}}";
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
