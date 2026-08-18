// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.MsgRole;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.model.ChatResponse;
import io.agentscope.core.model.GenerateOptions;
import io.agentscope.core.model.Model;
import io.agentscope.core.model.ToolSchema;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.runtime.StdioRuntime;
import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Clock;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import reactor.core.publisher.Flux;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Proves real stdio admission preserves session FIFO and independent-session parallelism. */
final class AgentScopeConcurrencyStdioIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";
    private static final Duration FRAME_TIMEOUT = Duration.ofSeconds(10);
    private static final int MAX_FRAMES_PER_PHASE = 128;

    /**
     * Runs both scheduler contracts through one real AgentScope Harness and JSONL transport.
     * The first model stream is held by a latch, so the assertions observe ordering rather than
     * relying on timing or a sleep-based race.
     */
    @Test
    void sameSessionIsFifoAndIndependentSessionsRunInParallel(@TempDir Path temp)
            throws Exception {
        runScenario(temp.resolve("same-session"), false);
        runScenario(temp.resolve("independent-sessions"), true);
    }

    /**
     * Exercises one isolated sidecar so the model barrier and AgentScope session state cannot
     * leak between the serial and parallel assertions.
     */
    private static void runScenario(Path temp, boolean independentSessions) throws Exception {
        Path workspace = Files.createDirectories(temp.resolve("workspace"));
        Path data = Files.createDirectories(temp.resolve("data"));
        BarrierModel model = new BarrierModel(2);
        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        AgentScopeRuntimeGraph graph = AgentScopeRuntimeGraph.open(workspace, data, model);
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.PRODUCTION),
                Clock.systemUTC(), graph);
        CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
        String firstRequest = "c:turn-1";
        String secondRequest = "c:turn-2";
        String firstThread = independentSessions ? "thr_parallel_1" : "thr_fifo";
        String secondThread = independentSessions ? "thr_parallel_2" : "thr_fifo";
        String firstSession = independentSessions ? "session_parallel_1" : "session_fifo";
        String secondSession = independentSessions ? "session_parallel_2" : "session_fifo";
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8));
             FrameReader frames = new FrameReader(output, clientOutput)) {
            send(input, initializeFrame());
            JsonNode initialized = readFrame(frames);
            assertEquals("c:init", initialized.path("id").textValue());
            send(input, initializedFrame());
            assertEquals("runtime/statusChanged",
                    readFrame(frames).path("method").textValue());

            send(input, turnStartFrame(firstRequest, firstThread, firstSession, "fifo-first"));
            send(input, turnStartFrame(secondRequest, secondThread, secondSession, "fifo-second"));
            Map<String, String> accepted = readAccepted(frames,
                    firstRequest, secondRequest);
            assertEquals(2, accepted.size(), "both pipelined turns must be admitted");

            if (independentSessions) {
                assertTrue(model.awaitAllEntries(),
                        "independent sessions must both enter Model before release");
                assertTrue(model.hasEntryText("fifo-first"),
                        "the first independent turn must enter the fake Model");
                assertTrue(model.hasEntryText("fifo-second"),
                        "the second independent turn must enter the fake Model");
            } else {
                assertTrue(model.awaitFirstEntry(), "the FIFO head must enter Model");
                assertEquals(1, model.callCount(),
                        "the queued same-session turn must not enter Model while its predecessor runs");
                assertEquals(List.of(1), model.entryOrder(),
                        "only the first same-session turn may be executing before release");
                assertFalse(model.hasEntryText("fifo-second"),
                        "the queued same-session turn must not enter Model before release");
            }

            model.release();
            List<String> terminalOrder = new ArrayList<>();
            Map<String, Integer> terminalCounts = readTerminals(frames,
                    accepted.values().stream().toList(), terminalOrder);
            assertEquals(2, terminalOrder.size(), "each accepted turn must reach a terminal event");
            assertTrue(terminalCounts.values().stream().allMatch(count -> count == 1),
                    "each turn must have exactly one terminal event");
            assertEquals(1, model.entryCount("fifo-first"),
                    "the scenario must observe one provider entry for the first user turn");
            assertEquals(1, model.entryCount("fifo-second"),
                    "the scenario must observe one provider entry for the second user turn");
            if (!independentSessions) {
                assertEquals(List.of(accepted.get(firstRequest), accepted.get(secondRequest)),
                        terminalOrder, "same-session terminal order must follow admission order");
                assertTrue(model.hasEntryText("fifo-first"),
                        "the first same-session turn must enter the fake Model");
                assertTrue(model.hasEntryText("fifo-second"),
                        "the second same-session turn must enter Model after the first terminal path");
                assertTrue(model.firstEntryIndex("fifo-first") < model.firstEntryIndex("fifo-second"),
                        "same-session Model entries must preserve FIFO order");
            }

            send(input, shutdownFrame());
            readUntilShutdown(frames, terminalCounts,
                    accepted.values().stream().toList());
            assertTrue(terminalCounts.values().stream().allMatch(count -> count == 1),
                    "shutdown must not reveal a duplicate terminal event");
        } finally {
            // A failed assertion must still unblock the fake provider before runtime cleanup;
            // otherwise a test failure could leave a virtual worker waiting on the barrier.
            model.release();
            clientInput.close();
            clientOutput.close();
            runtime.close();
        }
        assertEquals(0, exit.get(10, TimeUnit.SECONDS),
                "normal JSONL shutdown must return exit code zero");
    }

    /**
     * Reads both start responses while preserving the single-writer JSONL boundary. A terminal
     * before either acceptance would violate StdioRuntime's event-admission gate and is rejected.
     */
    private static Map<String, String> readAccepted(FrameReader frames, String firstId,
                                                      String secondId) throws Exception {
        Set<String> expected = Set.of(firstId, secondId);
        Map<String, String> accepted = new LinkedHashMap<>();
        for (int index = 0; index < MAX_FRAMES_PER_PHASE && accepted.size() < expected.size(); index++) {
            JsonNode frame = readFrame(frames);
            String id = frame.path("id").textValue();
            if (!expected.contains(id)) {
                assertFalse("turn/completed".equals(frame.path("method").textValue()),
                        "a turn cannot complete before its start response: " + frame);
                continue;
            }
            assertFalse(frame.has("error"), frame.toString());
            assertTrue(frame.path("result").path("accepted").asBoolean(false), frame.toString());
            String turnId = frame.path("result").path("turnId").textValue();
            assertNotNull(turnId, frame.toString());
            accepted.put(id, turnId);
        }
        assertEquals(expected.size(), accepted.size(), "both turn/start responses must arrive");
        assertEquals((long) expected.size(), accepted.values().stream().distinct().count(),
                "accepted turns must receive unique turn ids");
        return accepted;
    }

    /**
     * Reads terminal notifications until every accepted turn is observed and records their exact
     * order. Duplicate terminal notifications fail immediately instead of being silently folded.
     */
    private static Map<String, Integer> readTerminals(FrameReader frames, List<String> turnIds,
                                                        List<String> terminalOrder) throws Exception {
        Set<String> expected = Set.copyOf(turnIds);
        Map<String, Integer> counts = new LinkedHashMap<>();
        while (terminalOrder.size() < expected.size()) {
            JsonNode frame = readFrame(frames);
            if (!"turn/completed".equals(frame.path("method").textValue())) {
                continue;
            }
            JsonNode turn = frame.path("params").path("turn");
            String turnId = turn.path("turnId").textValue();
            assertTrue(expected.contains(turnId), "unknown terminal turn: " + frame);
            int count = counts.merge(turnId, 1, Integer::sum);
            assertEquals(1, count, "one turn must have exactly one terminal event");
            assertEquals("completed", turn.path("terminalStatus").textValue(), frame.toString());
            terminalOrder.add(turnId);
        }
        return counts;
    }

    /**
     * Drains the shutdown acknowledgement and any frames already queued before it, keeping the
     * terminal-count assertion effective through the transport's final writer boundary.
     */
    private static void readUntilShutdown(FrameReader frames, Map<String, Integer> terminalCounts,
                                           List<String> turnIds) throws Exception {
        Set<String> expected = Set.copyOf(turnIds);
        for (int index = 0; index < MAX_FRAMES_PER_PHASE; index++) {
            JsonNode frame = readFrame(frames);
            if ("turn/completed".equals(frame.path("method").textValue())) {
                String turnId = frame.path("params").path("turn").path("turnId").textValue();
                assertTrue(expected.contains(turnId), "unknown late terminal turn: " + frame);
                terminalCounts.merge(turnId, 1, Integer::sum);
            }
            if ("c:stop".equals(frame.path("id").textValue())) {
                assertFalse(frame.has("error"), frame.toString());
                return;
            }
        }
        throw new AssertionError("shutdown acknowledgement did not arrive");
    }

    /** Creates the complete handshake offer used by the real StdioRuntime codec. */
    private static String initializeFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:init\",\"method\":\"initialize\",\"params\":{" +
                "\"protocolMajor\":1,\"protocolMinor\":0,\"minimumCompatibleMinor\":0," +
                "\"clientVersion\":\"concurrency-test\",\"capabilities\":{" +
                "\"methods\":[\"initialize\",\"turn/start\",\"shutdown\"]," +
                "\"events\":[\"runtime/statusChanged\",\"turn/started\",\"item/started\",\"item/delta\",\"item/completed\",\"turn/completed\"]," +
                "\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"]," +
                "\"itemKinds\":[\"agent_message\"],\"mcp\":{" +
                "\"protocolVersions\":[],\"transports\":[],\"features\":[]}}," +
                "\"limits\":{" +
                "\"maxFrameBytes\":4194304,\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024," +
                "\"maxInFlightRequests\":64,\"maxPendingRequests\":64,\"maxItemDeltaBytes\":65536," +
                "\"maxInlineToolOutputBytes\":1048576,\"maxLogBytes\":1048576,\"defaultRequestDeadlineMs\":120000," +
                "\"defaultApprovalDeadlineMs\":300000}}}";
    }

    /** Creates the post-initialize notification with the fixed challenge token. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\""
                + READY_TOKEN + "\"}}";
    }

    /** Creates one bounded coding turn frame for either the FIFO or parallel scenario. */
    private static String turnStartFrame(String id, String threadId, String sessionId,
                                         String text) throws Exception {
        var params = JsonNodes.object();
        params.put("threadId", threadId);
        params.put("userId", "concurrency-user");
        params.put("sessionId", sessionId);
        params.put("accessMode", "workspace");
        params.put("profileRevision", "profile_concurrency");
        var input = JsonNodes.array();
        var part = JsonNodes.object();
        part.put("type", "text");
        part.put("text", text);
        input.add(part);
        params.set("input", input);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", "turn/start", "params", params));
    }

    /** Creates the normal graceful shutdown request. */
    private static String shutdownFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}";
    }

    /** Writes one complete JSONL frame and flushes the client side of the pipe. */
    private static void send(BufferedWriter input, String frame) throws Exception {
        input.write(frame);
        input.write('\n');
        input.flush();
    }

    /** Parses every received line at the same boundary used by the desktop transport. */
    private static JsonNode readFrame(FrameReader frames) throws Exception {
        String line = frames.nextLine();
        assertNotNull(line);
        assertFalse(line.isBlank());
        JsonNode frame = JSON.readTree(line);
        assertNotNull(frame);
        assertTrue(frame.isObject(), "stdout frame must be a JSON object");
        assertEquals("2.0", frame.path("jsonrpc").textValue(), frame.toString());
        return frame;
    }

    /**
     * Owns one reader for the complete PipedInputStream session; changing read-side threads per
     * frame makes Java's pipe report a dead writer. The bounded queue decouples frame parsing from
     * the pipe while close() wakes and joins the sole reader on timeout or normal cleanup.
     */
    private static final class FrameReader implements AutoCloseable {
        private static final int QUEUE_CAPACITY = 256;

        private final BufferedReader reader;
        private final PipedInputStream clientOutput;
        private final BlockingQueue<ReadResult> results = new ArrayBlockingQueue<>(QUEUE_CAPACITY);
        private final AtomicBoolean closed = new AtomicBoolean();
        private final Thread worker;

        /** Starts the one stable read-side thread before any frame is requested. */
        private FrameReader(BufferedReader reader, PipedInputStream clientOutput) {
            this.reader = reader;
            this.clientOutput = clientOutput;
            this.worker = Thread.ofVirtual().name("ja-test-stdio-reader").start(this::readLoop);
        }

        /** Reads one queued line with a finite deadline and reclaims the reader on timeout. */
        private String nextLine() throws Exception {
            ReadResult result = results.poll(FRAME_TIMEOUT.toMillis(), TimeUnit.MILLISECONDS);
            if (result == null) {
                close();
                throw new AssertionError("stdout frame read timed out");
            }
            if (result.failure() != null) {
                Throwable failure = result.failure();
                if (failure instanceof IOException ioFailure) {
                    throw ioFailure;
                }
                if (failure instanceof RuntimeException runtimeFailure) {
                    throw runtimeFailure;
                }
                throw new AssertionError("stdout frame reader failed", failure);
            }
            return result.line();
        }

        /** Keeps one PipedInputStream read-side identity while applying bounded result buffering. */
        private void readLoop() {
            try {
                while (!closed.get()) {
                    String line = reader.readLine();
                    if (line == null) {
                        enqueue(new ReadResult(null, null));
                        return;
                    }
                    enqueue(new ReadResult(line, null));
                }
            } catch (Throwable failure) {
                if (!closed.get()) {
                    enqueue(new ReadResult(null, failure));
                }
            }
        }

        /** Offers a result without allowing a stalled test consumer to grow memory unbounded. */
        private void enqueue(ReadResult result) {
            while (!closed.get()) {
                try {
                    if (results.offer(result, 100, TimeUnit.MILLISECONDS)) {
                        return;
                    }
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                    return;
                }
            }
        }

        /** Closes the pipe before BufferedReader and joins the sole reader deterministically. */
        @Override
        public void close() throws Exception {
            if (closed.compareAndSet(false, true)) {
                try {
                    clientOutput.close();
                } catch (IOException ignored) {
                    // The test's primary result must not be replaced by cleanup noise.
                }
                try {
                    reader.close();
                } catch (IOException ignored) {
                    // Closing the underlying pipe is sufficient to wake the reader.
                }
                worker.interrupt();
            }
            worker.join(2_000L);
            if (worker.isAlive()) {
                throw new AssertionError("stdio reader thread must not outlive the session");
            }
        }

        /** Carries one line, EOF, or one terminal reader failure through the bounded queue. */
        private record ReadResult(String line, Throwable failure) {
        }
    }

    /**
     * A fake provider that records Model entry before waiting for one explicit release. Returning
     * a gated Flux keeps the real Harness and AgentScope scheduler in the path while avoiding
     * wall-clock sleeps and network calls.
     */
    private static final class BarrierModel implements Model {
        private final CountDownLatch firstEntry = new CountDownLatch(1);
        private final CountDownLatch allEntries;
        private final CountDownLatch release = new CountDownLatch(1);
        private final AtomicInteger calls = new AtomicInteger();
        private final CopyOnWriteArrayList<Integer> entryOrder = new CopyOnWriteArrayList<>();
        private final CopyOnWriteArrayList<String> entryTexts = new CopyOnWriteArrayList<>();

        /** Fixes the expected entry barrier so parallelism is asserted without polling. */
        private BarrierModel(int expectedEntries) {
            allEntries = new CountDownLatch(expectedEntries);
        }

        /** Records provider entry before the gated response can publish a terminal event. */
        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            int call = calls.incrementAndGet();
            entryOrder.add(call);
            entryTexts.add(messages.stream()
                    .filter(message -> message.getRole() == MsgRole.USER)
                    .map(Msg::getTextContent)
                    .filter(text -> text != null && !text.isBlank())
                    .reduce((ignored, text) -> text)
                    .orElse("<missing-user-text>"));
            firstEntry.countDown();
            allEntries.countDown();
            return Flux.defer(() -> {
                try {
                    if (!release.await(10, TimeUnit.SECONDS)) {
                        return Flux.error(new AssertionError("model barrier was not released"));
                    }
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                    return Flux.error(exception);
                }
                return Flux.just(new ChatResponse("concurrency-response-" + call,
                        List.of(TextBlock.builder().text("turn complete " + call).build()),
                        null, Map.of(), "stop"));
            });
        }

        /** Gives AgentScope a stable model identity without reaching a provider. */
        @Override
        public String getModelName() {
            return "ja-concurrency-integration-model";
        }

        /** Waits for the first real Model entry without introducing scheduling sleeps. */
        private boolean awaitFirstEntry() throws InterruptedException {
            return firstEntry.await(10, TimeUnit.SECONDS);
        }

        /** Waits until both independent session workers have entered Model. */
        private boolean awaitAllEntries() throws InterruptedException {
            return allEntries.await(10, TimeUnit.SECONDS);
        }

        /** Returns the number of provider entries observed so far. */
        private int callCount() {
            return calls.get();
        }

        /** Returns a stable snapshot of provider entry order for FIFO assertions. */
        private List<Integer> entryOrder() {
            return List.copyOf(entryOrder);
        }

        /** Returns whether one exact user turn text reached the provider seam. */
        private boolean hasEntryText(String expected) {
            return entryTexts.contains(expected);
        }

        /** Counts only exact user-turn entries, excluding AgentScope's internal memory prompts. */
        private int entryCount(String expected) {
            return (int) entryTexts.stream().filter(expected::equals).count();
        }

        /** Returns the first provider-entry position for one exact user turn text. */
        private int firstEntryIndex(String expected) {
            return entryTexts.indexOf(expected);
        }

        /** Releases all currently gated model streams so terminal events can drain. */
        private void release() {
            release.countDown();
        }
    }
}
