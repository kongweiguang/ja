// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.domain.ItemKind;
import io.github.kongweiguang.ja.domain.ItemStatus;
import io.github.kongweiguang.ja.domain.PermissionMode;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.ThreadId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.domain.TurnMode;
import io.github.kongweiguang.ja.domain.TurnState;
import io.github.kongweiguang.ja.domain.TurnStatus;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.protocol.WireEnums;

import java.time.Clock;
import java.time.Instant;
import java.util.Objects;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Consumer;

/**
 * Small deterministic adapter used only by the explicit {@code --runtime=fake}
 * mode. It models the product event contract while leaving AgentScope wiring
 * to the later coding-engine phase.
 */
public final class DeterministicFakeTurnRuntime implements TurnRuntime {
    private static final int MAX_INPUT_CHARS = 1_048_576;

    private final ServerInstanceId serverInstanceId;
    private final Clock clock;
    private final ProtocolLimits limits;
    private final CountDownLatch executionGate;
    private final ExecutorService workers;
    private final Object activeMonitor = new Object();
    private final AtomicLong turnSequence = new AtomicLong();
    private final AtomicLong itemSequence = new AtomicLong();
    private final AtomicLong eventSequence = new AtomicLong();
    private final ConcurrentHashMap<ThreadId, TurnId> activeTurns = new ConcurrentHashMap<>();
    private final ConcurrentHashMap<TurnId, AtomicBoolean> cancellationFlags = new ConcurrentHashMap<>();
    private final ConcurrentHashMap<TurnId, Thread> workerThreads = new ConcurrentHashMap<>();
    private final ConcurrentHashMap<ThreadId, AtomicLong> threadSequences = new ConcurrentHashMap<>();
    private final AtomicBoolean accepting = new AtomicBoolean(true);
    private final AtomicBoolean closed = new AtomicBoolean(false);

    /** Uses the system clock for the production fixture while keeping IDs local to one sidecar. */
    public DeterministicFakeTurnRuntime(ServerInstanceId serverInstanceId) {
        this(serverInstanceId, Clock.systemUTC());
    }

    /** Injects a clock so event ordering tests never depend on sleeping or wall-clock timing. */
    public DeterministicFakeTurnRuntime(ServerInstanceId serverInstanceId, Clock clock) {
        this(serverInstanceId, clock, new CountDownLatch(0));
    }

    /** Allows tests to hold a worker at its deterministic boundary without sleeping. */
    public DeterministicFakeTurnRuntime(ServerInstanceId serverInstanceId, Clock clock,
                                        CountDownLatch executionGate) {
        this(serverInstanceId, clock, executionGate, ProtocolLimits.defaults());
    }

    /**
     * Injects the negotiated budgets so fixture admission has the same finite
     * concurrency contract as the production adapter.
     */
    public DeterministicFakeTurnRuntime(ServerInstanceId serverInstanceId, Clock clock,
                                        CountDownLatch executionGate, ProtocolLimits limits) {
        this.serverInstanceId = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        this.clock = Objects.requireNonNull(clock, "clock");
        this.executionGate = Objects.requireNonNull(executionGate, "executionGate");
        this.limits = Objects.requireNonNull(limits, "limits");
        ThreadFactory factory = Thread.ofVirtual().name("ja-fake-turn", 0).factory();
        this.workers = new ThreadPoolExecutor(
                limits.maxInFlightRequests(), limits.maxInFlightRequests(), 0L, TimeUnit.MILLISECONDS,
                new ArrayBlockingQueue<>(limits.maxPendingRequests()), factory,
                new ThreadPoolExecutor.AbortPolicy());
    }

    /**
     * Validates and reserves a thread synchronously before scheduling work;
     * this makes same-thread admission serial even when requests arrive together.
     */
    @Override
    public TurnHandle start(RpcRequest request, Consumer<TurnEvent> eventPublisher) {
        Objects.requireNonNull(request, "request");
        Objects.requireNonNull(eventPublisher, "eventPublisher");
        if (!accepting.get() || closed.get()) {
            throw new ProtocolException(JaErrorCode.SHUTTING_DOWN);
        }
        TurnInput input = TurnInput.from(request.params());
        if (activeTurns.containsKey(input.threadId())) {
            throw new ProtocolException(JaErrorCode.THREAD_BUSY);
        }
        TurnId turnId = new TurnId("turn_fake_" + turnSequence.incrementAndGet());
        TurnId previous = activeTurns.putIfAbsent(input.threadId(), turnId);
        if (previous != null) {
            throw new ProtocolException(JaErrorCode.THREAD_BUSY);
        }
        cancellationFlags.put(turnId, new AtomicBoolean());
        try {
            workers.submit(() -> runTurn(input, turnId, eventPublisher));
        } catch (RejectedExecutionException exception) {
            activeTurns.remove(input.threadId(), turnId);
            cancellationFlags.remove(turnId);
            synchronized (activeMonitor) {
                activeMonitor.notifyAll();
            }
            throw new ProtocolException(JaErrorCode.QUEUE_FULL, null, exception);
        }
        return new TurnHandle(turnId);
    }

    /**
     * Claims cancellation for one exact thread/turn pair and interrupts only
     * that fixture worker, matching the production runtime's identity boundary.
     */
    @Override
    public CancelResult cancel(String threadId, TurnId turnId, String reason) {
        final ThreadId expectedThread;
        try {
            expectedThread = new ThreadId(Objects.requireNonNull(threadId, "threadId"));
        } catch (RuntimeException exception) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS, null, exception);
        }
        TurnId active = activeTurns.get(expectedThread);
        if (active == null || !active.equals(turnId)) {
            throw new ProtocolException(JaErrorCode.TURN_NOT_FOUND);
        }
        AtomicBoolean cancelled = cancellationFlags.get(turnId);
        if (cancelled == null) {
            throw new ProtocolException(JaErrorCode.TURN_NOT_ACTIVE);
        }
        if (!cancelled.compareAndSet(false, true)) {
            return new CancelResult(true, turnId, "interrupted");
        }
        Thread worker = workerThreads.get(turnId);
        if (worker != null) {
            worker.interrupt();
        }
        // The worker owns the terminal event; returning interrupting keeps the
        // ACK independent from its event-publisher scheduling.
        return new CancelResult(true, turnId, "interrupting");
    }

    /** Confirms that the fixture worker can be interrupted for stdio cancellation tests. */
    @Override
    public boolean supportsCancellation() {
        return true;
    }

    /** Stops admission so accepted turns can drain without being discarded. */
    @Override
    public void stopAccepting() {
        accepting.set(false);
    }

    /** Waits for every accepted turn to publish its terminal event. */
    @Override
    public boolean awaitQuiescence(java.time.Duration timeout) {
        Objects.requireNonNull(timeout, "timeout");
        long deadline = System.nanoTime() + timeout.toNanos();
        synchronized (activeMonitor) {
            while (!activeTurns.isEmpty()) {
                long remaining = deadline - System.nanoTime();
                if (remaining <= 0L) {
                    return false;
                }
                try {
                    long millis = TimeUnit.NANOSECONDS.toMillis(remaining);
                    int nanos = (int) (remaining - TimeUnit.MILLISECONDS.toNanos(millis));
                    activeMonitor.wait(Math.max(1L, millis), Math.max(0, nanos));
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                    return false;
                }
            }
            return true;
        }
    }

    /** Gracefully closes workers after accepted work has had a terminal chance. */
    @Override
    public void close() {
        if (closed.compareAndSet(false, true)) {
            stopAccepting();
            workers.shutdown();
            try {
                // The runtime is closed only after quiescence in the sidecar
                // lifecycle.  Waiting without shutdownNow preserves the
                // exactly-once terminal opportunity for every accepted turn.
                workers.awaitTermination(5, TimeUnit.SECONDS);
            } catch (InterruptedException exception) {
                Thread.currentThread().interrupt();
            }
        }
    }

    /** Emits one complete, ordered fixture timeline and releases the thread admission. */
    private void runTurn(TurnInput input, TurnId turnId, Consumer<TurnEvent> eventPublisher) {
        workerThreads.put(turnId, Thread.currentThread());
        Instant startedAt = clock.instant();
        TurnState queued = TurnState.queued(turnId, input.threadId(), input.accessMode(),
                PermissionMode.ASK, startedAt);
        ItemIdParts item = new ItemIdParts("item_fake_" + itemSequence.incrementAndGet(), input.threadId());
        boolean terminalPublished = false;
        TurnState latest = queued;
        try {
            AtomicBoolean cancelled = cancellationFlags.get(turnId);
            if (cancelled != null && cancelled.get()) {
                publishInterrupted(latest, eventPublisher);
                terminalPublished = true;
                return;
            }
            executionGate.await();
            if (cancelled != null && cancelled.get()) {
                publishInterrupted(latest, eventPublisher);
                terminalPublished = true;
                return;
            }
            latest = queued.transition(TurnStatus.RUNNING, clock.instant());
            eventPublisher.accept(new TurnEvent("turn/started", turnParams(latest)));
            eventPublisher.accept(new TurnEvent("item/started", itemParams(item, turnId,
                    ItemStatus.STARTED, input.outputText(), input.inputText())));
            for (String delta : utf8Chunks(input.outputText(), limits.maxItemDeltaBytes())) {
                if (cancelled != null && cancelled.get()) {
                    publishInterrupted(latest, eventPublisher);
                    terminalPublished = true;
                    return;
                }
                eventPublisher.accept(new TurnEvent("item/delta", deltaParams(item, delta)));
            }
            if (cancelled != null && cancelled.get()) {
                publishInterrupted(latest, eventPublisher);
                terminalPublished = true;
                return;
            }
            TurnState completed = latest.transition(TurnStatus.COMPLETED, clock.instant());
            eventPublisher.accept(new TurnEvent("item/completed", itemParams(item, turnId,
                    ItemStatus.COMPLETED, input.outputText(), input.inputText())));
            ObjectNode completedParams = turnParams(completed);
            completedParams.put("terminalStatus", "completed");
            eventPublisher.accept(new TurnEvent("turn/completed", completedParams));
            terminalPublished = true;
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            AtomicBoolean cancelled = cancellationFlags.get(turnId);
            if (cancelled != null && cancelled.get()) {
                publishInterrupted(latest, eventPublisher);
            } else {
                publishAborted(latest, eventPublisher);
            }
            terminalPublished = true;
        } catch (RuntimeException exception) {
            if (!terminalPublished) {
                publishAborted(latest, eventPublisher);
            }
        } finally {
            workerThreads.remove(turnId, Thread.currentThread());
            cancellationFlags.remove(turnId);
            activeTurns.remove(input.threadId(), turnId);
            synchronized (activeMonitor) {
                activeMonitor.notifyAll();
            }
        }
    }

    /** Publishes the only fallback terminal status when a worker is interrupted. */
    private void publishAborted(TurnState latest, Consumer<TurnEvent> eventPublisher) {
        if (latest.status().terminal()) {
            return;
        }
        try {
            TurnState aborted = latest.transition(TurnStatus.ABORTED_BY_RUNTIME, clock.instant());
            ObjectNode params = turnParams(aborted);
            params.put("terminalStatus", "aborted_by_runtime");
            eventPublisher.accept(new TurnEvent("turn/completed", params));
        } catch (RuntimeException ignored) {
            // A broken writer owns the primary failure; no second diagnostic may reach stdout.
        }
    }

    /** Publishes the frozen interrupted terminal through the normal state transitions. */
    private void publishInterrupted(TurnState latest, Consumer<TurnEvent> eventPublisher) {
        if (latest.status().terminal()) {
            return;
        }
        try {
            TurnState interrupting = latest.status() == TurnStatus.INTERRUPTING
                    ? latest : latest.transition(TurnStatus.INTERRUPTING, clock.instant());
            TurnState interrupted = interrupting.transition(TurnStatus.INTERRUPTED, clock.instant());
            ObjectNode params = turnParams(interrupted);
            params.put("terminalStatus", "interrupted");
            eventPublisher.accept(new TurnEvent("turn/completed", params));
        } catch (RuntimeException ignored) {
            // A broken writer owns the primary failure; no second diagnostic may reach stdout.
        }
    }

    /** Adds the common thread event envelope and full turn snapshot. */
    private ObjectNode turnParams(TurnState state) {
        ObjectNode params = eventBase(state.threadId());
        ObjectNode turn = JsonNodes.object();
        turn.put("turnId", state.turnId().value());
        turn.put("threadId", state.threadId().value());
        turn.put("status", WireEnums.encode(state.status()));
        turn.put("accessMode", WireEnums.encode(state.accessMode()));
        turn.put("startedAt", state.startedAt().toString());
        if (state.completedAt() != null) {
            turn.put("completedAt", state.completedAt().toString());
        }
        params.set("turn", turn);
        return params;
    }

    /** Adds a complete item snapshot so the UI never has to infer final state from deltas. */
    private ObjectNode itemParams(ItemIdParts item, TurnId turnId, ItemStatus status,
                                  String text, String inputText) {
        ObjectNode params = eventBase(item.threadId);
        ObjectNode itemNode = JsonNodes.object();
        itemNode.put("itemId", item.itemId);
        itemNode.put("turnId", turnId.value());
        itemNode.put("kind", WireEnums.encode(ItemKind.AGENT_MESSAGE));
        itemNode.put("status", WireEnums.encode(status));
        itemNode.put("title", "JA fake agent message");
        itemNode.put("text", text);
        ObjectNode metadata = JsonNodes.object();
        metadata.put("runtime", "fake");
        metadata.put("input", inputText);
        itemNode.set("metadata", metadata);
        params.set("item", itemNode);
        return params;
    }

    /** Emits a bounded delta that is always recoverable from item/completed. */
    private ObjectNode deltaParams(ItemIdParts item, String text) {
        ObjectNode params = eventBase(item.threadId);
        params.put("itemId", item.itemId);
        params.put("delta", text);
        params.put("deltaBytes", text.getBytes(java.nio.charset.StandardCharsets.UTF_8).length);
        return params;
    }

    /** Splits by complete Unicode code points so byte limits cannot corrupt text. */
    private static java.util.List<String> utf8Chunks(String text, int maxBytes) {
        java.util.ArrayList<String> chunks = new java.util.ArrayList<>();
        StringBuilder current = new StringBuilder();
        int bytes = 0;
        for (int offset = 0; offset < text.length();) {
            int codePoint = text.codePointAt(offset);
            String piece = new String(Character.toChars(codePoint));
            int pieceBytes = piece.getBytes(java.nio.charset.StandardCharsets.UTF_8).length;
            if (bytes > 0 && bytes + pieceBytes > maxBytes) {
                chunks.add(current.toString());
                current.setLength(0);
                bytes = 0;
            }
            current.append(piece);
            bytes += pieceBytes;
            offset += Character.charCount(codePoint);
        }
        if (current.length() > 0) {
            chunks.add(current.toString());
        }
        return chunks;
    }

    /** Creates one shared sequence envelope for every event in a thread. */
    private ObjectNode eventBase(ThreadId threadId) {
        if (threadId == null) {
            throw new IllegalStateException("fake event has no thread");
        }
        long seq = threadSequences.computeIfAbsent(threadId, ignored -> new AtomicLong())
                .incrementAndGet();
        ObjectNode params = JsonNodes.object();
        params.put("serverInstanceId", serverInstanceId.value());
        params.put("threadId", threadId.value());
        params.put("seq", seq);
        params.put("eventId", "evt_fake_" + eventSequence.incrementAndGet());
        params.put("occurredAt", clock.instant().toString());
        return params;
    }

    /** Validates turn/start params and maps all input parts into visible fixture text. */
    private record TurnInput(ThreadId threadId, TurnMode accessMode,
                             String inputText, String outputText) {
        private static TurnInput from(ObjectNode params) {
            try {
                ThreadId threadId = new ThreadId(requiredText(params, "threadId"));
                TurnMode accessMode = WireEnums.decode(requiredText(params, "accessMode"), TurnMode.class);
                String profile = requiredText(params, "profileRevision");
                if (!profile.startsWith("profile_")) {
                    throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
                }
                JsonNode inputNode = params.get("input");
                if (inputNode == null || !inputNode.isArray() || inputNode.isEmpty() || inputNode.size() > 128) {
                    throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
                }
                StringBuilder input = new StringBuilder();
                int partIndex = 0;
                for (JsonNode part : inputNode) {
                    if (part == null || !part.isObject()) {
                        throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
                    }
                    if (partIndex++ > 0) {
                        input.append('\n');
                    }
                    String type = requiredText((ObjectNode) part, "type");
                    if ("text".equals(type)) {
                        input.append(requiredText((ObjectNode) part, "text"));
                    } else {
                        throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
                    }
                }
                String inputText = input.toString();
                if (inputText.isBlank() || inputText.length() > MAX_INPUT_CHARS) {
                    throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
                }
                String output = "Fake response: " + inputText;
                return new TurnInput(threadId, accessMode, inputText, output);
            } catch (ProtocolException exception) {
                throw exception;
            } catch (RuntimeException exception) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
        }

        /** Rejects missing or blank fields before their value can affect fixture output. */
        private static String requiredText(ObjectNode node, String field) {
            JsonNode value = node.get(field);
            if (value == null || !value.isTextual() || value.textValue().isBlank()) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
            return value.textValue();
        }
    }

    /** Keeps item identity and its thread routing together during one worker run. */
    private static final class ItemIdParts {
        private final String itemId;
        private final ThreadId threadId;

        /** Keeps the item route attached to its originating thread for every event. */
        private ItemIdParts(String itemId, ThreadId threadId) {
            this.itemId = itemId;
            this.threadId = threadId;
        }
    }
}
