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
import io.agentscope.core.event.RequestStopEvent;
import io.agentscope.core.event.RequireUserConfirmEvent;
import io.agentscope.core.event.TextBlockDeltaEvent;
import io.agentscope.core.event.TextBlockEndEvent;
import io.agentscope.core.event.TextBlockStartEvent;
import io.agentscope.core.event.ToolResultEndEvent;
import io.agentscope.core.event.ToolResultStartEvent;
import io.agentscope.core.event.ToolResultTextDeltaEvent;
import io.agentscope.core.message.ToolUseBlock;
import io.agentscope.core.message.ToolResultState;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import io.github.kongweiguang.ja.runtime.TurnRuntime;
import java.time.Clock;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Consumer;
import org.junit.jupiter.api.Test;
import org.reactivestreams.Publisher;
import org.reactivestreams.Subscriber;
import org.reactivestreams.Subscription;
import reactor.core.publisher.Flux;
import reactor.core.publisher.FluxSink;
import reactor.core.publisher.Sinks;

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
        ProtocolException wrongThread = assertThrows(ProtocolException.class,
                () -> runtime.cancel("thr_other", turn));
        assertEquals(JaErrorCode.TURN_NOT_FOUND, wrongThread.code());
        TurnRuntime.CancelResult cancellation = runtime.cancel("thr_cancel", turn);
        assertTrue(cancellation.accepted());
        assertEquals("interrupting", cancellation.status());
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

    /**
     * A fast approval is held until AgentScope closes its asking stream, then resumes exactly once
     * without allowing the permission pause to publish a premature terminal.
     */
    @Test
    void approvalResumeWaitsForAskingCompletionWithoutTimingSleep() throws Exception {
        ApprovalRaceEngine engine = new ApprovalRaceEngine(false);
        AgentScopeTurnRuntime runtime = runtime(engine, AgentScopeTurnRuntime.Config.defaults());
        List<TurnEvent> events = new java.util.concurrent.CopyOnWriteArrayList<>();
        AtomicReference<java.util.function.Consumer<TurnRuntime.ApprovalDecision>> resolver =
                new AtomicReference<>();
        runtime.setApprovalSink((prompt, callback) -> resolver.set(callback));
        try {
            runtime.start(request("thr_epoch", "session_epoch", "epoch"), events::add);
            assertTrue(engine.askingStarted.await(2, TimeUnit.SECONDS));
            assertTrue(engine.approvalObserved.await(2, TimeUnit.SECONDS));
            resolver.get().accept(new TurnRuntime.ApprovalDecision("allow_once", Instant.now()));
            assertEquals(1, engine.resumeStarted.getCount(),
                    "resume must wait for the asking Flux completion");
            assertTrue(events.stream().noneMatch(event -> "turn/completed".equals(event.method())));
            engine.completeAsking();
            assertTrue(engine.resumeStarted.await(2, TimeUnit.SECONDS));

            engine.releaseResume.countDown();
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
            List<TurnEvent> approvalTimeline = events.stream()
                    .filter(event -> event.method().startsWith("approval/"))
                    .toList();
            assertEquals(List.of("approval/requested", "approval/resolved"),
                    approvalTimeline.stream().map(TurnEvent::method).toList());
            long requestedSeq = approvalTimeline.getFirst().params().path("seq").longValue();
            long resolvedSeq = approvalTimeline.getLast().params().path("seq").longValue();
            assertTrue(requestedSeq > 0 && resolvedSeq == requestedSeq + 1,
                    "approval facts must use one continuous thread sequence");
            int requestedIndex = events.indexOf(approvalTimeline.getFirst());
            int resolvedIndex = events.indexOf(approvalTimeline.getLast());
            assertTrue(requestedIndex < resolvedIndex);
            assertTrue(events.subList(resolvedIndex + 1, events.size()).stream()
                    .allMatch(event -> event.params().path("seq").longValue() > resolvedSeq),
                    "resume/tool events must follow approval/resolved");
            assertTrue(events.stream().noneMatch(event -> event.method().equals("approval/request")),
                    "private approval request must not become a UI event");
            assertEquals(1, events.stream().filter(event -> "turn/completed".equals(event.method()))
                    .count());
            assertEquals("completed", events.getLast().params().path("turn")
                    .path("terminalStatus").textValue());
        } finally {
            runtime.close();
        }
    }

    /** Cancellation wins before the asking barrier opens and cannot persist or resume approval. */
    @Test
    void cancellationWinsApprovalResumeAdmission() throws Exception {
        ApprovalRaceEngine engine = new ApprovalRaceEngine(false);
        AgentScopeTurnRuntime runtime = runtime(engine, AgentScopeTurnRuntime.Config.defaults());
        List<TurnEvent> events = new java.util.concurrent.CopyOnWriteArrayList<>();
        AtomicReference<java.util.function.Consumer<TurnRuntime.ApprovalDecision>> resolver =
                new AtomicReference<>();
        runtime.setApprovalSink((prompt, callback) -> resolver.set(callback));
        try {
            TurnId turn = runtime.start(request("thr_cancel_approval", "session_cancel_approval",
                    "cancel-approval"), events::add).turnId();
            assertTrue(engine.approvalObserved.await(2, TimeUnit.SECONDS));
            resolver.get().accept(new TurnRuntime.ApprovalDecision("allow_session", Instant.now()));

            TurnRuntime.CancelResult cancellation = runtime.cancel("thr_cancel_approval", turn);
            assertEquals("interrupting", cancellation.status());
            engine.completeAsking();
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));

            assertEquals(0, engine.allowSessionCalls.get(),
                    "cancel winner must not write a session allow rule");
            assertEquals(0, engine.resumeCalls.get(),
                    "the cancelled approval must not enter AgentScope resume");
            assertEquals(1, events.stream().filter(event -> "turn/completed".equals(event.method()))
                    .count());
            assertEquals("interrupted", events.getLast().params().path("turn")
                    .path("terminalStatus").textValue());
        } finally {
            runtime.close();
        }
    }

    /** Approval wins the shared gate first; a later cancel only interrupts the replacement turn. */
    @Test
    void approvalWinnerThenCancelOnlyInterruptsCurrentRun() throws Exception {
        ApprovalRaceEngine engine = new ApprovalRaceEngine(false, false);
        AgentScopeTurnRuntime runtime = runtime(engine, AgentScopeTurnRuntime.Config.defaults());
        List<TurnEvent> events = new java.util.concurrent.CopyOnWriteArrayList<>();
        AtomicReference<java.util.function.Consumer<TurnRuntime.ApprovalDecision>> resolver =
                new AtomicReference<>();
        runtime.setApprovalSink((prompt, callback) -> resolver.set(callback));
        try {
            TurnId turn = runtime.start(request("thr_approval_winner", "session_approval_winner",
                    "approval-winner"), events::add).turnId();
            assertTrue(engine.approvalObserved.await(2, TimeUnit.SECONDS));
            resolver.get().accept(new TurnRuntime.ApprovalDecision("allow_session", Instant.now()));
            engine.completeAsking();
            assertTrue(engine.resumeStarted.await(2, TimeUnit.SECONDS));

            TurnRuntime.CancelResult cancellation = runtime.cancel("thr_approval_winner", turn);
            assertEquals("interrupting", cancellation.status());
            engine.releaseResume.countDown();
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));

            assertEquals(1, engine.allowSessionCalls.get());
            assertEquals(1, engine.resumeCalls.get());
            assertEquals(1, events.stream().filter(event -> "turn/completed".equals(event.method()))
                    .count());
            assertEquals("interrupted", events.getLast().params().path("turn")
                    .path("terminalStatus").textValue());
        } finally {
            runtime.close();
        }
    }

    /**
     * Keeps a permission pause open across AgentScope stop/end events and proves that successive
     * resume epochs preserve both MCP-like results before the single terminal event. The separate
     * approvalResumeWaitsForAskingCompletionWithoutTimingSleep test owns the completion barrier;
     * this test composes that contract with stale-event epoch filtering.
     */
    @Test
    void approvalResumePreservesSequentialToolResultsAcrossTwoEpochs() throws Exception {
        TwoApprovalEngine engine = new TwoApprovalEngine();
        AgentScopeTurnRuntime runtime = runtime(engine, AgentScopeTurnRuntime.Config.defaults());
        List<TurnEvent> events = new java.util.concurrent.CopyOnWriteArrayList<>();
        BlockingQueue<Consumer<TurnRuntime.ApprovalDecision>> decisions = new ArrayBlockingQueue<>(2);
        CountDownLatch firstPrompt = new CountDownLatch(1);
        CountDownLatch secondPrompt = new CountDownLatch(1);
        AtomicInteger promptCount = new AtomicInteger();
        AtomicReference<Throwable> approvalOverflow = new AtomicReference<>();
        runtime.setApprovalSink((prompt, callback) -> {
            if (!decisions.offer(callback)) {
                approvalOverflow.compareAndSet(null,
                        new AssertionError("approval callback queue overflow"));
                throw new IllegalStateException("approval callback queue overflow");
            }
            if (promptCount.incrementAndGet() == 1) {
                firstPrompt.countDown();
            } else {
                secondPrompt.countDown();
            }
        });
        try {
            runtime.start(request("thr_two_approvals", "session_two_approvals", "two-approvals"),
                    events::add);
            assertTrue(firstPrompt.await(2, TimeUnit.SECONDS));
            assertTrue(engine.lateReady.await(2, TimeUnit.SECONDS));
            assertTrue(events.stream().noneMatch(event -> "turn/completed".equals(event.method())));

            Consumer<TurnRuntime.ApprovalDecision> firstDecision = decisions.poll();
            assertTrue(firstDecision != null, "first approval callback must be queued");
            firstDecision.accept(new TurnRuntime.ApprovalDecision("allow_once", Instant.now()));
            assertTrue(engine.firstResumeSubscribed.await(2, TimeUnit.SECONDS));
            assertEquals(Sinks.EmitResult.OK,
                    engine.lateAskingEvents.tryEmitNext(new AgentEndEvent("late-old-epoch")));
            assertEquals(Sinks.EmitResult.OK, engine.lateAskingEvents.tryEmitComplete());
            assertTrue(engine.lateForwarded.await(2, TimeUnit.SECONDS));
            engine.emitFirstResumeEvents();
            assertTrue(secondPrompt.await(2, TimeUnit.SECONDS));
            assertTrue(events.stream().noneMatch(event -> "turn/completed".equals(event.method())));
            assertEquals(List.of("server-a"), engine.resultTexts(events));

            Consumer<TurnRuntime.ApprovalDecision> secondDecision = decisions.poll();
            assertTrue(secondDecision != null, "second approval callback must be queued");
            secondDecision.accept(new TurnRuntime.ApprovalDecision("allow_once", Instant.now()));
            assertTrue(engine.secondResume.await(2, TimeUnit.SECONDS));
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));

            assertEquals(List.of("server-a", "server-b"), engine.resultTexts(events));
            assertEquals(2, engine.resumeCalls.get());
            assertEquals(2, promptCount.get());
            assertTrue(decisions.isEmpty(), "all approval callbacks must be consumed");
            assertTrue(approvalOverflow.get() == null, "approval callback queue must not overflow");
            List<TurnEvent> terminals = events.stream()
                    .filter(event -> "turn/completed".equals(event.method())).toList();
            assertEquals(1, terminals.size());
            assertEquals(terminals.getFirst(), events.getLast(),
                    "terminal event must be the final callback");
            assertEquals(1, events.stream().filter(event -> "turn/completed".equals(event.method()))
                    .count());
            assertEquals("completed", events.getLast().params().path("turn")
                    .path("terminalStatus").textValue());
        } finally {
            runtime.close();
        }
    }

    /** A current asking-stream error still fails the turn instead of leaving approval pending. */
    @Test
    void currentAskingStreamErrorFailsApprovalTurn() throws Exception {
        ApprovalRaceEngine engine = new ApprovalRaceEngine(true);
        AgentScopeTurnRuntime runtime = runtime(engine, AgentScopeTurnRuntime.Config.defaults());
        List<TurnEvent> events = new java.util.concurrent.CopyOnWriteArrayList<>();
        runtime.setApprovalSink((prompt, callback) -> { });
        try {
            runtime.start(request("thr_epoch_error", "session_epoch_error", "epoch-error"),
                    events::add);
            assertTrue(engine.askingStarted.await(2, TimeUnit.SECONDS));
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
            assertEquals("failed", events.getLast().params().path("turn")
                    .path("terminalStatus").textValue());
            assertEquals("provider_error", events.getLast().params().path("turn")
                    .path("reason").textValue());
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

    /** Coordinates asking/resume Fluxes so callback order is deterministic without sleeps. */
    private static final class ApprovalRaceEngine implements AgentScopeEngine {
        private final boolean failAsking;
        private final CountDownLatch askingStarted = new CountDownLatch(1);
        private final CountDownLatch approvalObserved = new CountDownLatch(1);
        private final CountDownLatch resumeStarted = new CountDownLatch(1);
        private final CountDownLatch releaseResume = new CountDownLatch(1);
        private final AtomicReference<FluxSink<AgentEvent>> askingSink = new AtomicReference<>();
        private final AtomicInteger allowSessionCalls = new AtomicInteger();
        private final AtomicInteger resumeCalls = new AtomicInteger();
        private final boolean holdResume;

        private ApprovalRaceEngine(boolean failAsking) {
            this(failAsking, true);
        }

        /** Allows the approval-winner test to keep a replacement stream live without blocking. */
        private ApprovalRaceEngine(boolean failAsking, boolean holdResume) {
            this.failAsking = failAsking;
            this.holdResume = holdResume;
        }

        /** Emits one approval and optionally fails the still-current asking stream. */
        @Override
        public Flux<AgentEvent> stream(String input, RuntimeContext context) {
            return Flux.create(sink -> {
                askingSink.set(sink);
                askingStarted.countDown();
                ToolUseBlock tool = new ToolUseBlock("race-tool", "execute",
                        Map.of("command", "echo race"));
                sink.next(new RequireUserConfirmEvent("reply_asking", List.of(tool)));
                sink.next(new AgentEndEvent("reply_asking"));
                approvalObserved.countDown();
                if (failAsking) {
                    sink.error(new IllegalStateException("asking stream failed"));
                }
            });
        }

        /** Holds the resumed stream until the test has delivered a stale asking completion. */
        @Override
        public Flux<AgentEvent> resume(io.agentscope.core.message.Msg confirmation,
                                      RuntimeContext context) {
            resumeCalls.incrementAndGet();
            return Flux.create(sink -> {
                resumeStarted.countDown();
                if (!holdResume) {
                    return;
                }
                try {
                    if (!releaseResume.await(2, TimeUnit.SECONDS)) {
                        sink.error(new IllegalStateException("resume barrier timed out"));
                        return;
                    }
                    sink.next(new AgentEndEvent("reply_resumed"));
                    sink.complete();
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                    sink.error(exception);
                }
            });
        }

        /** Counts session-level permission writes so cancellation admission can be asserted. */
        @Override
        public void allowSession(String userId, String sessionId, List<ToolUseBlock> toolCalls) {
            allowSessionCalls.incrementAndGet();
        }

        /** Completes only the old asking Flux after the replacement epoch is active. */
        private void completeAsking() {
            FluxSink<AgentEvent> sink = askingSink.get();
            if (sink == null) {
                throw new AssertionError("asking Flux was not subscribed");
            }
            sink.complete();
        }

        /** Cancellation is represented by the Flux lifecycle in this deterministic engine. */
        @Override
        public void interrupt(RuntimeContext context) {
        }

        /** Releases no external resources in this in-memory engine. */
        @Override
        public void close() {
        }
    }

    /** Drives two explicit HITL resumes so stale asking terminals cannot finish the replacement. */
    private static final class TwoApprovalEngine implements AgentScopeEngine {
        private final Sinks.Many<AgentEvent> lateAskingEvents =
                Sinks.many().unicast().onBackpressureBuffer(new ArrayBlockingQueue<>(1));
        private final Sinks.Many<AgentEvent> firstResumeEvents =
                Sinks.many().unicast().onBackpressureBuffer(new ArrayBlockingQueue<>(6));
        private final CountDownLatch firstResumeSubscribed = new CountDownLatch(1);
        private final CountDownLatch secondResume = new CountDownLatch(1);
        private final CountDownLatch lateReady = new CountDownLatch(1);
        private final CountDownLatch lateForwarded = new CountDownLatch(1);
        private final AtomicInteger resumeCalls = new AtomicInteger();

        /** Emits the first approval and keeps a bounded late callback source for epoch filtering. */
        @Override
        public Flux<AgentEvent> stream(String input, RuntimeContext context) {
            return Flux.from(new LateAskingPublisher());
        }

        /** Returns controllable first-resume events, then a terminal second-resume sequence. */
        @Override
        public Flux<AgentEvent> resume(io.agentscope.core.message.Msg confirmation,
                                       RuntimeContext context) {
            int call = resumeCalls.incrementAndGet();
            if (call == 1) {
                return firstResumeEvents.asFlux()
                        .doOnSubscribe(subscription -> firstResumeSubscribed.countDown());
            }
            secondResume.countDown();
            List<AgentEvent> events = new ArrayList<>(result("server-b", "reply_b"));
            events.add(new AgentEndEvent("reply_final"));
            return Flux.fromIterable(events);
        }

        /** No session policy is needed for this focused approval-order fixture. */
        @Override
        public void allowSession(String userId, String sessionId, List<ToolUseBlock> toolCalls) {
        }

        /** No provider interruption is required for the bounded synchronous fixture. */
        @Override
        public void interrupt(RuntimeContext context) {
        }

        /** No external resources are allocated by this in-memory fixture. */
        @Override
        public void close() {
        }

        /** Emits the first real result, second approval, and upstream pause markers in order. */
        private void emitFirstResumeEvents() {
            result("server-a", "reply_a").forEach(this::emit);
            emit(new RequireUserConfirmEvent("reply_second", List.of(tool("second"))));
            emit(new RequestStopEvent("asking-stop-second"));
            emit(new AgentEndEvent("asking-end-second"));
            assertEquals(Sinks.EmitResult.OK, firstResumeEvents.tryEmitComplete());
        }

        /** Pushes one event into the controlled first resume stream without timing assumptions. */
        private void emit(AgentEvent event) {
            assertEquals(Sinks.EmitResult.OK, firstResumeEvents.tryEmitNext(event));
        }

        /** Extracts and validates every normalized result item lifecycle, not just its metadata. */
        private List<String> resultTexts(List<TurnEvent> events) {
            Map<String, String> itemNames = new LinkedHashMap<>();
            Map<String, List<String>> lifecycle = new LinkedHashMap<>();
            Map<String, String> deltas = new LinkedHashMap<>();
            List<String> completedNames = new ArrayList<>();
            for (TurnEvent event : events) {
                if ("item/started".equals(event.method())) {
                    var item = event.params().path("item");
                    String name = item.path("metadata").path("toolName").textValue();
                    if (name != null && name.startsWith("server-")) {
                        String itemId = item.path("itemId").textValue();
                        assertTrue(itemId != null && !itemId.isBlank());
                        assertEquals("started", item.path("status").textValue());
                        assertEquals("started", item.path("metadata").path("status").textValue());
                        assertTrue(item.path("metadata").path("toolCallId").isTextual());
                        itemNames.put(itemId, name);
                        lifecycle.put(itemId, new ArrayList<>(List.of("started")));
                    }
                } else if ("item/delta".equals(event.method())) {
                    String itemId = event.params().path("itemId").textValue();
                    if (itemNames.containsKey(itemId)) {
                        assertEquals("[tool output redacted]", event.params().path("delta").textValue());
                        assertTrue(event.params().path("deltaBytes").asInt() > 0);
                        lifecycle.get(itemId).add("delta");
                        deltas.put(itemId, event.params().path("delta").textValue());
                    }
                } else if ("item/completed".equals(event.method())) {
                    var item = event.params().path("item");
                    String itemId = item.path("itemId").textValue();
                    if (itemNames.containsKey(itemId)) {
                        assertEquals("completed", item.path("status").textValue());
                        assertEquals("completed", item.path("metadata").path("status").textValue());
                        assertEquals(itemNames.get(itemId),
                                item.path("metadata").path("toolName").textValue());
                        lifecycle.get(itemId).add("completed");
                        completedNames.add(itemNames.get(itemId));
                    }
                }
            }
            assertEquals(itemNames.keySet(), lifecycle.keySet());
            for (String itemId : itemNames.keySet()) {
                assertEquals(List.of("started", "delta", "completed"), lifecycle.get(itemId));
                assertEquals("[tool output redacted]", deltas.get(itemId));
            }
            List<String> names = List.copyOf(itemNames.values());
            assertEquals(names, completedNames, "tool result completion order must match start order");
            return names;
        }

        /** Creates the smallest tool request that still enters AgentScope's native permission path. */
        private static ToolUseBlock tool(String name) {
            return new ToolUseBlock(name, "mcp_" + name, Map.of("value", name));
        }

        /** Creates the normal AgentScope result event sequence used by MCP tool calls. */
        private static List<AgentEvent> result(String value, String replyId) {
            String callId = "call_" + value;
            return List.of(
                    new ToolResultStartEvent(replyId, callId, value),
                    new ToolResultTextDeltaEvent(replyId, callId, value, value),
                    new ToolResultEndEvent(replyId, callId, value, ToolResultState.SUCCESS));
        }

        /**
         * Forwards one deliberately late old-stream event to exercise the runtime epoch guard.
         * The completion barrier is asserted by the dedicated approval completion test above;
         * this fixture does not claim that a second onComplete is observable through Reactor.
         */
        private final class LateAskingPublisher implements Publisher<AgentEvent> {
            @Override
            public void subscribe(Subscriber<? super AgentEvent> subscriber) {
                subscriber.onSubscribe(new Subscription() {
                    @Override
                    public void request(long demand) {
                        // The fixture emits only its bounded scripted sequence.
                    }

                    @Override
                    public void cancel() {
                        // Keep forwarding the deliberate late callback for epoch coverage.
                    }
                });
                subscriber.onNext(new RequireUserConfirmEvent(
                        "reply_first", List.of(tool("first"))));
                subscriber.onNext(new RequestStopEvent("asking-stop"));
                subscriber.onNext(new AgentEndEvent("asking-end"));
                subscriber.onComplete();
                lateAskingEvents.asFlux().subscribe(
                        event -> {
                            subscriber.onNext(event);
                            lateForwarded.countDown();
                        });
                lateReady.countDown();
            }
        }
    }
}
