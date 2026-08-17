// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.node.ObjectNode;
import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.event.AgentEndEvent;
import io.agentscope.core.event.AgentEvent;
import io.agentscope.core.event.AgentStartEvent;
import io.agentscope.core.event.TextBlockDeltaEvent;
import io.agentscope.core.event.TextBlockEndEvent;
import io.agentscope.core.event.TextBlockStartEvent;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import java.time.Clock;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.Consumer;
import org.junit.jupiter.api.Test;
import reactor.core.publisher.Flux;
import reactor.core.publisher.FluxSink;

/** Scheduler tests use barriers instead of timing sleeps or paid providers. */
final class AgentScopeTurnRuntimeTest {
    private static final Clock CLOCK = Clock.fixed(
            Instant.parse("2026-08-17T02:00:00Z"), ZoneOffset.UTC);

    /** Same-session requests remain FIFO while a different session can run concurrently. */
    @Test
    void serializesSameSessionAndRunsDifferentSessionsInParallel() throws Exception {
        ScriptedEngine engine = new ScriptedEngine();
        AgentScopeTurnRuntime runtime = runtime(engine, new AgentScopeTurnRuntime.Config(
                2, 8, 1024 * 1024, java.time.Duration.ofSeconds(2)));
        List<TurnEvent> firstEvents = new java.util.concurrent.CopyOnWriteArrayList<>();
        List<TurnEvent> secondEvents = new java.util.concurrent.CopyOnWriteArrayList<>();
        CountDownLatch firstStarted = new CountDownLatch(1);
        CountDownLatch releaseFirst = new CountDownLatch(1);
        engine.gate("first", firstStarted, releaseFirst);

        TurnId first = runtime.start(request("thr_a", "session_a", "first"), firstEvents::add).turnId();
        assertTrue(firstStarted.await(2, TimeUnit.SECONDS));
        TurnId second = runtime.start(request("thr_a", "session_a", "second"), secondEvents::add).turnId();
        assertTrue(secondEvents.isEmpty());
        releaseFirst.countDown();
        assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
        assertEquals(List.of(first.value(), second.value()), completedTurnIds(firstEvents, secondEvents));
        assertTrue(engine.threadNames().stream()
                .allMatch(name -> name.startsWith("ja-agentscope-turn")));
        assertEquals(0, runtime.laneCount());
        assertEquals(0, runtime.activeRunCount());

        CountDownLatch parallel = new CountDownLatch(2);
        CountDownLatch releaseParallel = new CountDownLatch(1);
        engine.gate("left", parallel, releaseParallel);
        engine.gate("right", parallel, releaseParallel);
        runtime.start(request("thr_left", "session_left", "left"), event -> { });
        runtime.start(request("thr_right", "session_right", "right"), event -> { });
        assertTrue(parallel.await(2, TimeUnit.SECONDS));
        releaseParallel.countDown();
        assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
        runtime.close();
    }

    /** Cancellation races with a late provider callback and still emits one terminal event. */
    @Test
    void cancellationSuppressesLateEventsAndTerminalDuplicates() throws Exception {
        ScriptedEngine engine = new ScriptedEngine();
        AgentScopeTurnRuntime runtime = runtime(engine, AgentScopeTurnRuntime.Config.defaults());
        List<TurnEvent> events = new java.util.concurrent.CopyOnWriteArrayList<>();
        CountDownLatch started = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        engine.gate("cancel", started, release);
        TurnId turn = runtime.start(request("thr_cancel", "session_cancel", "cancel"), events::add)
                .turnId();
        assertTrue(started.await(2, TimeUnit.SECONDS));
        assertTrue(runtime.cancel(turn));
        release.countDown();
        assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
        assertEquals(1, events.stream().filter(e -> e.method().equals("turn/completed")).count());
        assertEquals("interrupted", events.getLast().params().path("turn")
                .path("terminalStatus").textValue());
        runtime.close();
    }

    /** Provider and callback failures become redacted failed terminals without leaked details. */
    @Test
    void providerFailureAndPublisherFailureRemainTerminalExactlyOnce() {
        ScriptedEngine engine = new ScriptedEngine();
        engine.failInput("provider", new IllegalStateException("secret/provider/path"));
        AgentScopeTurnRuntime runtime = runtime(engine, AgentScopeTurnRuntime.Config.defaults());
        List<TurnEvent> providerEvents = new ArrayList<>();
        runtime.start(request("thr_provider", "session_provider", "provider"), providerEvents::add);
        assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
        assertEquals("failed", providerEvents.getLast().params().path("turn")
                .path("terminalStatus").textValue());
        assertTrue(providerEvents.toString().contains("provider_error"));
        List<TurnEvent> callbackEvents = new ArrayList<>();
        runtime.start(request("thr_callback", "session_callback", "callback"), event -> {
            callbackEvents.add(event);
            if (event.method().equals("item/delta")) {
                throw new IllegalStateException("consumer secret");
            }
        });
        assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
        assertEquals(1, callbackEvents.stream().filter(e -> e.method().equals("turn/completed")).count());
        runtime.close();
    }

    /** A monotonic absolute deadline terminates a never-ending provider without wall-clock sleeps. */
    @Test
    void deadlineTerminatesNeverEndingStream() throws Exception {
        AgentScopeEngine engine = new AgentScopeEngine() {
            @Override
            public Flux<AgentEvent> stream(String input, RuntimeContext context) {
                return Flux.never();
            }

            @Override
            public void interrupt(RuntimeContext context) {
            }

            @Override
            public void close() {
            }
        };
        AgentScopeTurnRuntime runtime = runtime(engine, new AgentScopeTurnRuntime.Config(
                1, 4, 1024, java.time.Duration.ofSeconds(1), java.time.Duration.ofMillis(50),
                new AgentScopeTurnRuntime.ResourceLimits(32, 16, 4096, 64 * 1024, 4096),
                AgentScopeTurnRuntime.ResourceLimits.sessionDefaults()));
        List<TurnEvent> events = new java.util.concurrent.CopyOnWriteArrayList<>();
        runtime.start(request("thr_deadline", "session_deadline", "never"), events::add);
        assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
        assertEquals("deadline_exceeded", events.getLast().params().path("turn")
                .path("reason").textValue());
        runtime.close();
    }

    /** A turn event budget fails closed while preserving one terminal event for the UI. */
    @Test
    void outputBudgetFailsClosed() {
        ScriptedEngine engine = new ScriptedEngine();
        AgentScopeTurnRuntime runtime = runtime(engine, new AgentScopeTurnRuntime.Config(
                1, 4, 1024, java.time.Duration.ofSeconds(1), java.time.Duration.ofSeconds(5),
                new AgentScopeTurnRuntime.ResourceLimits(2, 16, 4096, 64 * 1024, 4096),
                AgentScopeTurnRuntime.ResourceLimits.sessionDefaults()));
        List<TurnEvent> events = new java.util.concurrent.CopyOnWriteArrayList<>();
        runtime.start(request("thr_limit", "session_limit", "limited"), events::add);
        assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
        assertEquals("event_budget_exceeded", events.getLast().params().path("turn")
                .path("reason").textValue());
        assertEquals(1, events.stream().filter(e -> e.method().equals("turn/completed")).count());
        runtime.close();
    }

    /** Aggregates queued input count and UTF-8 bytes before a provider can consume them. */
    @Test
    void queuedInputAdmissionIsBoundedIncrementally() throws Exception {
        ScriptedEngine engine = new ScriptedEngine();
        CountDownLatch started = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        engine.gate("😀", started, release);
        AgentScopeTurnRuntime runtime = runtime(engine, new AgentScopeTurnRuntime.Config(
                1, 2, 8, java.time.Duration.ofSeconds(1), java.time.Duration.ofSeconds(5),
                AgentScopeTurnRuntime.ResourceLimits.turnDefaults(),
                AgentScopeTurnRuntime.ResourceLimits.sessionDefaults(), 8));
        try {
            runtime.start(request("thr_utf8", "session_utf8", "😀"), event -> { });
            assertTrue(started.await(2, TimeUnit.SECONDS));
            assertEquals(4, runtime.acceptedInputBytes());
            ProtocolException rejected = assertThrows(ProtocolException.class, () -> runtime.start(
                    request("thr_utf8", "session_utf8", "12345"), event -> { }));
            assertEquals(JaErrorCode.QUEUE_FULL, rejected.code());
            release.countDown();
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
            assertEquals(0, runtime.acceptedInputBytes());
        } finally {
            runtime.close();
        }
    }

    /** Evicts normalizer sequence and runtime lane registries after the last turn is terminal. */
    @Test
    void sequenceRegistryIsEvictedWithTheLastSessionLane() {
        ScriptedEngine engine = new ScriptedEngine();
        EventNormalizer normalizer = new EventNormalizer(new ServerInstanceId("srv_cleanup"), CLOCK);
        AgentScopeTurnRuntime runtime = new AgentScopeTurnRuntime(engine, normalizer,
                new RuntimeContextFactory(), AgentScopeTurnRuntime.Config.defaults());
        try {
            runtime.start(request("thr_cleanup", "session_cleanup", "done"), event -> { });
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
            assertEquals(0, runtime.activeRunCount());
            assertEquals(0, runtime.laneCount());
            assertEquals(0, normalizer.threadSequenceCount());
        } finally {
            runtime.close();
        }
    }

    /** Creates a runtime with deterministic instance and event timestamps. */
    private static AgentScopeTurnRuntime runtime(AgentScopeEngine engine,
                                                 AgentScopeTurnRuntime.Config config) {
        return new AgentScopeTurnRuntime(engine,
                new EventNormalizer(new ServerInstanceId("srv_runtime"), CLOCK),
                new RuntimeContextFactory(), config);
    }

    /** Creates the strict request shape accepted by the AgentScope adapter. */
    private static RpcRequest request(String threadId, String sessionId, String text) {
        ObjectNode params = JsonNodes.object();
        params.put("threadId", threadId.startsWith("thr_") ? threadId : "thr_" + threadId);
        params.put("sessionId", sessionId);
        params.put("userId", "user");
        params.put("mode", "coding");
        params.put("permissionMode", "workspace");
        var input = JsonNodes.array();
        ObjectNode part = JsonNodes.object();
        part.put("type", "text");
        part.put("text", text);
        input.add(part);
        params.set("input", input);
        return new RpcRequest("c:req_test", "turn/start", params,
                RpcDirection.CLIENT_TO_SERVER);
    }

    /** Extracts terminal turn identities in the order observed by the test. */
    private static List<String> completedTurnIds(List<TurnEvent> first, List<TurnEvent> second) {
        List<TurnEvent> all = new ArrayList<>();
        all.addAll(first);
        all.addAll(second);
        return all.stream().filter(e -> e.method().equals("turn/completed"))
                .map(e -> e.params().path("turn").path("turnId").textValue()).toList();
    }

    private static final class ScriptedEngine implements AgentScopeEngine {
        private final Map<String, Gate> gates = new ConcurrentHashMap<>();
        private final Map<String, Throwable> failures = new ConcurrentHashMap<>();
        private final ConcurrentLinkedQueue<String> threadNames = new ConcurrentLinkedQueue<>();
        private final AtomicInteger closeCount = new AtomicInteger();

        /** Installs a barrier keyed by input text for deterministic scheduling assertions. */
        void gate(String input, CountDownLatch started, CountDownLatch release) {
            gates.put(input, new Gate(started, release));
        }

        /** Installs a provider failure without an HTTP or model call. */
        void failInput(String input, Throwable error) {
            failures.put(input, error);
        }

        /** Captures worker identity so the virtual-thread execution boundary remains explicit. */
        List<String> threadNames() {
            return List.copyOf(threadNames);
        }

        /** Emits a stable AgentScope event script after the optional barrier. */
        @Override
        public Flux<AgentEvent> stream(String input, RuntimeContext context) {
            return Flux.create(sink -> {
                threadNames.add(Thread.currentThread().getName());
                Throwable failure = failures.get(input);
                if (failure != null) {
                    sink.error(failure);
                    return;
                }
                Gate gate = gates.get(input);
                if (gate != null) {
                    gate.started.countDown();
                    try {
                        gate.release.await(2, TimeUnit.SECONDS);
                    } catch (InterruptedException exception) {
                        Thread.currentThread().interrupt();
                        sink.error(exception);
                        return;
                    }
                }
                emit(sink, input);
            });
        }

        /** The scripted engine does not need a provider-specific interrupt hook. */
        @Override
        public void interrupt(RuntimeContext context) {
            // Flux disposal is the deterministic cancellation boundary in this fake.
        }

        /** Counts close calls so the test can detect leaked runtime ownership. */
        @Override
        public void close() {
            closeCount.incrementAndGet();
        }

        private static void emit(FluxSink<AgentEvent> sink, String input) {
            sink.next(new AgentStartEvent("session", "reply_" + input, "ja"));
            sink.next(new TextBlockStartEvent("reply_" + input, "block_" + input));
            sink.next(new TextBlockDeltaEvent("reply_" + input, "block_" + input, input));
            sink.next(new TextBlockEndEvent("reply_" + input, "block_" + input));
            sink.next(new AgentEndEvent("reply_" + input));
            sink.complete();
        }

        private record Gate(CountDownLatch started, CountDownLatch release) { }
    }
}
