// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.event.AgentEvent;
import io.agentscope.core.event.AgentEventType;
import io.agentscope.core.event.AgentEndEvent;
import io.agentscope.core.event.AgentStartEvent;
import io.agentscope.core.event.ConfirmResult;
import io.agentscope.core.event.RequireUserConfirmEvent;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.MsgRole;
import io.agentscope.core.message.ToolUseBlock;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.ThreadId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import io.github.kongweiguang.ja.runtime.TurnHandle;
import io.github.kongweiguang.ja.runtime.TurnRuntime;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Consumer;
import reactor.core.Disposable;
import reactor.core.publisher.Flux;

/**
 * AgentScope-backed JA turn runtime. Requests sharing one session enter a
 * serial FIFO lane; independent sessions are submitted to a bounded virtual
 * thread pool. Terminal publication is guarded separately from provider
 * cancellation so races cannot produce duplicate turn/completed events.
 */
public final class AgentScopeTurnRuntime implements TurnRuntime {
    private final AgentScopeEngine engine;
    private final EventNormalizer normalizer;
    private final RuntimeContextFactory contextFactory;
    private final Config config;
    private final ExecutorService workers;
    private final ScheduledExecutorService deadlines;
    private final ConcurrentMap<SessionKey, SessionLane> lanes = new ConcurrentHashMap<>();
    private final ConcurrentMap<TurnId, Run> runs = new ConcurrentHashMap<>();
    private final ConcurrentMap<String, Run> approvals = new ConcurrentHashMap<>();
    private final AtomicLong turnSequence = new AtomicLong();
    private final AtomicLong approvalSequence = new AtomicLong();
    private final AtomicInteger acceptedCount = new AtomicInteger();
    private final AtomicInteger acceptedInputBytes = new AtomicInteger();
    private final Object admissionMonitor = new Object();
    private final Object quiescenceMonitor = new Object();
    private final AtomicBoolean accepting = new AtomicBoolean(true);
    private final AtomicBoolean closed = new AtomicBoolean();
    private volatile TurnRuntime.ApprovalSink approvalSink = (prompt, resolver) -> {
        // Direct runtime callers may omit a transport; the run remains waiting until cancelled or
        // its deadline expires, which is safer than silently executing an ASK tool.
    };

    /** Builds a runtime with the bounded production defaults. */
    public AgentScopeTurnRuntime(AgentScopeEngine engine, ServerInstanceId serverInstanceId) {
        this(engine, new EventNormalizer(serverInstanceId), new RuntimeContextFactory(),
                Config.defaults());
    }

    /** Injects all boundaries needed for deterministic scheduler and provider tests. */
    public AgentScopeTurnRuntime(AgentScopeEngine engine, EventNormalizer normalizer,
                                 RuntimeContextFactory contextFactory, Config config) {
        this.engine = Objects.requireNonNull(engine, "engine");
        this.normalizer = Objects.requireNonNull(normalizer, "normalizer");
        this.contextFactory = Objects.requireNonNull(contextFactory, "contextFactory");
        this.config = Objects.requireNonNull(config, "config");
        ThreadFactory threads = Thread.ofVirtual().name("ja-agentscope-turn", 0).factory();
        this.workers = new ThreadPoolExecutor(
                config.maxConcurrentSessions(), config.maxConcurrentSessions(), 0L,
                TimeUnit.MILLISECONDS, new ArrayBlockingQueue<>(config.maxConcurrentSessions()),
                threads, new ThreadPoolExecutor.AbortPolicy());
        ThreadFactory deadlineThreads = Thread.ofPlatform().daemon(true)
                .name("ja-agentscope-deadline", 0).factory();
        this.deadlines = Executors.newSingleThreadScheduledExecutor(deadlineThreads);
    }

    /** Installs the one host approval sink; response handling remains asynchronous and one-shot. */
    @Override
    public void setApprovalSink(TurnRuntime.ApprovalSink sink) {
        approvalSink = sink == null ? (prompt, resolver) -> { } : sink;
    }

    /**
     * Admits the request into its session FIFO before scheduling it, ensuring a
     * fast response does not depend on provider startup latency.
     */
    @Override
    public TurnHandle start(RpcRequest request, Consumer<TurnEvent> eventPublisher) {
        Objects.requireNonNull(request, "request");
        Objects.requireNonNull(eventPublisher, "eventPublisher");
        Input input = Input.parse(request.params(), config.maxInputBytes());
        final TurnId turnId;
        final SessionKey key;
        try {
            // Validate identity value objects before reserving a queue slot so
            // malformed IDs cannot consume admission capacity permanently.
            turnId = new TurnId(input.requestedTurnId() != null
                    ? input.requestedTurnId() : "turn_as_" + turnSequence.incrementAndGet());
            key = new SessionKey(input.userId(), input.sessionId());
        } catch (IllegalArgumentException exception) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS, null, exception);
        }
        Run run;
        SessionLane lane;
        boolean schedule;
        // Close shares this monitor so it cannot drain and close the engine
        // between the admission check and registration in the run map.
        synchronized (admissionMonitor) {
            if (!accepting.get() || closed.get()) {
                throw new ProtocolException(JaErrorCode.SHUTTING_DOWN);
            }
            reserveAdmission(input.inputBytes());
            run = new Run(turnId, key, input, eventPublisher);
            if (runs.putIfAbsent(turnId, run) != null) {
                acceptedCount.decrementAndGet();
                acceptedInputBytes.addAndGet(-input.inputBytes());
                throw new ProtocolException(JaErrorCode.DUPLICATE_REQUEST);
            }
            lane = lanes.computeIfAbsent(key, ignored -> new SessionLane());
            run.lane = lane;
            run.deadlineNanos = System.nanoTime() + config.turnTimeout().toNanos();
            synchronized (lane) {
                lane.queue.addLast(run);
                schedule = lane.running == null;
            }
            try {
                run.deadlineTask = deadlines.schedule(
                        () -> expire(run), config.turnTimeout().toNanos(), TimeUnit.NANOSECONDS);
            } catch (RuntimeException exception) {
                // A concurrent close may stop the scheduler after admission. Release the
                // already-reserved run before surfacing a stable shutdown error.
                finish(run, "failed", "scheduler_rejected");
                throw new ProtocolException(JaErrorCode.SHUTTING_DOWN, null, exception);
            }
        }
        if (schedule) {
            scheduleNext(lane);
        }
        return new TurnHandle(turnId);
    }

    /** Reserves count and UTF-8 queue budgets together so a burst cannot exhaust heap indirectly. */
    private void reserveAdmission(int inputBytes) {
        int currentCount = acceptedCount.get();
        int currentBytes = acceptedInputBytes.get();
        if (currentCount >= config.maxAcceptedTurns()
                || inputBytes > config.maxQueuedInputBytes()
                || (long) currentBytes + inputBytes > config.maxQueuedInputBytes()) {
            throw new ProtocolException(JaErrorCode.QUEUE_FULL);
        }
        acceptedCount.incrementAndGet();
        acceptedInputBytes.addAndGet(inputBytes);
    }

    /**
     * Cancels a queued or running turn by identity; late provider callbacks are
     * ignored after the run's terminal gate changes state.
     */
    public boolean cancel(TurnId turnId) {
        return cancelInternal(turnId, "cancelled");
    }

    /**
     * Cancels only the turn owned by the supplied thread.  The explicit thread
     * check prevents a stale UI request from stopping a same-id turn that was
     * admitted under a different conversation context.
     */
    @Override
    public TurnRuntime.CancelResult cancel(String threadId, TurnId turnId, String reason) {
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(turnId, "turnId");
        Run run = runs.get(turnId);
        if (run == null || !run.input.threadId.value().equals(threadId)) {
            throw new ProtocolException(JaErrorCode.TURN_NOT_FOUND);
        }
        synchronized (run) {
            if (run.finished.get() && !run.cancelled.get()) {
                throw new ProtocolException(JaErrorCode.TURN_NOT_ACTIVE);
            }
            if (!run.cancelled.compareAndSet(false, true)) {
                // A second cancel that races the first one is an idempotent
                // acknowledgement; no provider interrupt or terminal is repeated.
                return new TurnRuntime.CancelResult(true, turnId, "interrupted");
            }
        }
        cancelMarkedRun(run, "cancelled");
        return new TurnRuntime.CancelResult(true, turnId, "interrupting");
    }

    /** Confirms that this adapter owns the provider interruption boundary. */
    @Override
    public boolean supportsCancellation() {
        return true;
    }

    /** Applies one cancellation path to queued and running turns, preserving the reason. */
    private boolean cancelInternal(TurnId turnId, String reason) {
        Run run = runs.get(Objects.requireNonNull(turnId, "turnId"));
        if (run == null) {
            return false;
        }
        synchronized (run) {
            if (run.finished.get() || !run.cancelled.compareAndSet(false, true)) {
                return false;
            }
        }
        cancelMarkedRun(run, reason);
        return true;
    }

    /**
     * Performs the one provider interruption and terminal transition after the
     * atomic cancellation claim; callers must already own that claim.
     */
    private void cancelMarkedRun(Run run, String reason) {
        SessionLane lane = lanes.get(run.sessionKey);
        boolean queued = false;
        if (lane != null) {
            synchronized (lane) {
                if (lane.queue.remove(run)) {
                    queued = true;
                }
            }
        }
        cancelPendingApproval(clearPendingApproval(run));
        if (queued) {
            scheduleCancellationFinish(run, reason);
            return;
        }
        Disposable subscription = run.subscription;
        if (subscription != null) {
            subscription.dispose();
        }
        RuntimeContext context = run.context;
        if (context != null) {
            try {
                engine.interrupt(context);
            } catch (RuntimeException ignored) {
                // The run's own terminal event is still published below.
            }
        }
        scheduleCancellationFinish(run, reason);
    }

    /**
     * Moves terminal publication off the stdio control lane so cancellation
     * never waits on a provider or a slow event consumer.
     */
    private void scheduleCancellationFinish(Run run, String reason) {
        try {
            workers.submit(() -> finish(run, "interrupted", reason));
        } catch (RejectedExecutionException rejected) {
            // Shutdown may already have closed the bounded executor; the
            // terminal gate still must be closed exactly once.
            finish(run, "interrupted", reason);
        }
    }

    /** Clears the AgentScope HITL resume path before a late approval can race cancellation. */
    private PendingApproval clearPendingApproval(Run run) {
        synchronized (run) {
            PendingApproval pending = run.pendingApproval;
            if (pending == null) {
                return null;
            }
            approvals.remove(pending.prompt.approvalId(), run);
            run.pendingApproval = null;
            run.waitingApproval.set(false);
            return pending;
        }
    }

    /** Retires the transport request without introducing another approval correlation map. */
    private static void cancelPendingApproval(PendingApproval pending) {
        if (pending != null && pending.cancelHandle != null) {
            pending.cancelHandle.cancel();
        }
    }

    /** Cancels a turn when its absolute deadline expires, including queued turns. */
    private void expire(Run run) {
        if (!run.finished.get()) {
            cancelInternal(run.turnId, "deadline_exceeded");
        }
    }

    /** Prevents new turns while accepted FIFO entries continue to drain. */
    @Override
    public void stopAccepting() {
        synchronized (admissionMonitor) {
            accepting.set(false);
        }
    }

    /** Waits for both running streams and queued session entries to finish. */
    @Override
    public boolean awaitQuiescence(Duration timeout) {
        Objects.requireNonNull(timeout, "timeout");
        long deadline = System.nanoTime() + timeout.toNanos();
        synchronized (quiescenceMonitor) {
            while (!runs.isEmpty()) {
                long remaining = deadline - System.nanoTime();
                if (remaining <= 0L) {
                    return false;
                }
                try {
                    long millis = TimeUnit.NANOSECONDS.toMillis(remaining);
                    int nanos = (int) (remaining - TimeUnit.MILLISECONDS.toNanos(millis));
                    quiescenceMonitor.wait(Math.max(1L, millis), Math.max(0, nanos));
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                    return false;
                }
            }
            return true;
        }
    }

    /** Returns active run count for shutdown invariants without exposing mutable registries. */
    int activeRunCount() {
        return runs.size();
    }

    /** Returns live FIFO lane count so tests can prove empty sessions are tombstoned. */
    int laneCount() {
        return lanes.size();
    }

    /** Returns aggregate queued input bytes for admission and shutdown diagnostics. */
    int acceptedInputBytes() {
        return acceptedInputBytes.get();
    }

    /**
     * Stops admission, gives accepted work a bounded cancellation opportunity,
     * and closes the AgentScope engine only after no callback can publish.
     */
    @Override
    public void close() {
        synchronized (admissionMonitor) {
            if (!closed.compareAndSet(false, true)) {
                return;
            }
            accepting.set(false);
        }
        if (!awaitQuiescence(config.closeTimeout())) {
            for (TurnId turnId : List.copyOf(runs.keySet())) {
                cancel(turnId);
            }
            awaitQuiescence(config.closeTimeout());
        }
        workers.shutdown();
        try {
            boolean terminated = workers.awaitTermination(config.closeTimeout().toMillis(),
                    TimeUnit.MILLISECONDS);
            if (!terminated) {
                // A provider that ignores cancellation must not keep the process alive beyond the
                // close budget; interrupt the bounded worker set before releasing the engine.
                workers.shutdownNow();
            }
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            workers.shutdownNow();
        } finally {
            deadlines.shutdownNow();
            engine.close();
        }
    }

    /** Schedules the next FIFO head and converts executor rejection into a terminal failure. */
    private void scheduleNext(SessionLane lane) {
        Run run;
        synchronized (lane) {
            if (lane.running != null || lane.queue.isEmpty()) {
                return;
            }
            run = lane.queue.removeFirst();
            lane.running = run;
        }
        try {
            workers.submit(() -> execute(lane, run));
        } catch (RuntimeException exception) {
            finish(run, "failed", "scheduler_rejected");
        }
    }

    /** Subscribes one AgentScope stream and binds all callbacks to one run gate. */
    private void execute(SessionLane lane, Run run) {
        if (run.cancelled.get()) {
            finish(run, "interrupted", "cancelled_before_execution");
            return;
        }
        if (deadlineExpired(run)) {
            cancelInternal(run.turnId, "deadline_exceeded");
            return;
        }
            run.normalized = normalizer.open(run.input.threadId, run.turnId,
                    run.input.mode, run.input.accessMode);
            run.context = contextFactory.create(run.sessionKey, Map.of(
                    "ja.threadId", run.input.threadId.value(),
                    "ja.turnId", run.turnId.value(),
                    "ja.deadline", Long.toString(run.deadlineNanos)));
        try {
            // Emit a product-owned start boundary even if a provider stream
            // omits AgentStart; the normalizer suppresses a later duplicate.
            publishNormalized(run, run.subscriptionEpoch.get(), new AgentStartEvent(
                    run.sessionKey.sessionId(), run.turnId.value(), "ja"));
            if (run.finished.get() || run.cancelled.get()) {
                return;
            }
            subscribeStream(run, engine.stream(run.input.text, run.context));
        } catch (RuntimeException exception) {
            streamFailed(run, exception);
        }
    }

    /**
     * Reserves one callback generation before subscribing so a synchronous provider cannot
     * publish into a stream that a later approval resume has already replaced.
     */
    private void subscribeStream(Run run, Flux<AgentEvent> stream) {
        Objects.requireNonNull(stream, "stream");
        // Reserve the epoch under the run gate. Otherwise a late callback can pass the epoch check
        // while the approval thread is switching to its resume stream, which is exactly the window
        // that can publish an old terminal event.
        long epoch;
        synchronized (run) {
            epoch = run.subscriptionEpoch.incrementAndGet();
        }
        subscribeStream(run, stream, epoch);
    }

    /**
     * Subscribes one stream without blocking the control lane or the session FIFO. Every callback
     * captures the reserved epoch; this keeps a late asking Flux from completing a resumed stream.
     */
    private void subscribeStream(Run run, Flux<AgentEvent> stream, long epoch) {
        Objects.requireNonNull(stream, "stream");
        synchronized (run) {
            if (run.subscriptionEpoch.get() != epoch || run.cancelled.get() || run.finished.get()) {
                return;
            }
        }
        // Do not hold the run gate while subscribing: a provider may synchronously block while it
        // establishes a child process or network stream. Callback handlers still take the gate,
        // so a synchronous event is serialized with a concurrent epoch switch.
        Disposable subscription = stream.subscribe(
                event -> publishNormalized(run, epoch, event),
                error -> streamFailed(run, epoch, error),
                () -> streamCompleted(run, epoch));
        synchronized (run) {
            if (run.subscriptionEpoch.get() != epoch || run.cancelled.get() || run.finished.get()) {
                subscription.dispose();
                return;
            }
            Disposable previous = run.subscription;
            run.subscription = subscription;
            if (previous != null && previous != subscription) {
                previous.dispose();
            }
            if (run.cancelled.get() || run.finished.get()) {
                subscription.dispose();
            }
        }
    }

    /**
     * Publishes only the current stream generation while suppressing callbacks after cancellation.
     * The run gate covers normalization and publication so an epoch switch cannot occur between
     * accepting an event and applying its terminal effect.
     */
    private void publishNormalized(Run run, long epoch, AgentEvent event) {
        synchronized (run) {
            if (!isCurrentStream(run, epoch) || run.finished.get() || run.cancelled.get()) {
                return;
            }
            if (deadlineExpired(run)) {
                cancelInternal(run.turnId, "deadline_exceeded");
                return;
            }
            if (run.waitingApproval.get() && isPermissionPauseTerminal(event)) {
                // AgentScope closes the asking Flux with REQUEST_STOP after emitting the HITL event;
                // some versions also emit AgentEnd before completion. Neither is a real JA terminal
                // while the host still owns the approval decision.
                return;
            }
            try {
                List<TurnEvent> normalized = normalizer.normalize(event, run.normalized);
                boolean terminalBatch = normalizer.isTerminal(run.normalized);
                if (!publishRunEvents(run, normalized, terminalBatch)) {
                    run.resourceOverflow.set(true);
                    streamFailed(run, epoch, new IllegalStateException("event budget exceeded"));
                    return;
                }
                if (event instanceof RequireUserConfirmEvent confirmation) {
                    registerApproval(run, confirmation);
                }
            } catch (RuntimeException exception) {
                streamFailed(run, epoch, exception);
            }
        }
    }

    /** Publishes one normalized batch through the existing admission gate without adding a queue. */
    private boolean publishRunEvents(Run run, List<TurnEvent> events, boolean terminalBatch) {
        for (TurnEvent output : events) {
            if (!run.finished.get() && !run.cancelled.get()) {
                if (!terminalBatch && !admitOutput(run, output)) {
                    return false;
                }
                run.publisher.accept(output);
            }
        }
        return true;
    }

    /** Keeps AgentScope permission-pause bookkeeping non-terminal until the host decision arrives. */
    private static boolean isPermissionPauseTerminal(AgentEvent event) {
        return event != null && (event.getType() == AgentEventType.REQUEST_STOP
                || event instanceof AgentEndEvent);
    }

    /**
     * Turns AgentScope's upstream ASK event into one resumable JA approval prompt. The provider
     * Flux is allowed to complete after this event; the run stays in its session lane until the
     * transport calls the one-shot resolver.
     */
    private void registerApproval(Run run, RequireUserConfirmEvent confirmation) {
        if (confirmation.getToolCalls().isEmpty() || run.waitingApproval.get()) {
            throw new IllegalStateException("invalid AgentScope approval pause");
        }
        String itemId = normalizer.approvalItemId(run.normalized);
        if (itemId == null) {
            throw new IllegalStateException("approval item was not normalized");
        }
        String approvalId = nextApprovalId(run);
        ActionSummary action = summarizeAction(confirmation.getToolCalls());
        TurnRuntime.ApprovalPrompt prompt = new TurnRuntime.ApprovalPrompt(
                approvalId, run.input.threadId.value(), run.turnId.value(), itemId,
                action.kind(), action.command(), action.cwd(), action.relativePaths(), action.risk(),
                run.input.accessMode, Instant.now().plus(config.turnTimeout()), action.reason());
        PendingApproval pending = new PendingApproval(prompt, confirmation.getToolCalls());
        synchronized (run) {
            if (run.finished.get() || run.cancelled.get() || run.waitingApproval.get()) {
                return;
            }
            run.pendingApproval = pending;
            run.waitingApproval.set(true);
            approvals.put(approvalId, run);
            try {
                // Persist and publish the business fact before exposing the private request. The
                // same normalizer context supplies the thread sequence, while StdioRuntime's
                // publisher performs the SQLite append before the frame reaches Rust.
                if (!publishRunEvents(run, normalizer.approvalRequested(run.normalized, prompt), false)) {
                    throw new IllegalStateException("approval requested event budget exceeded");
                }
                pending.cancelHandle = approvalSink.requestWithHandle(
                        prompt, decision -> resolveApproval(approvalId, decision));
                // A sink may complete or cancel synchronously. Keep a handle that was returned
                // after such a callback aligned with the already terminal run.
                if (run.finished.get() || run.cancelled.get()) {
                    cancelPendingApproval(pending);
                }
            } catch (RuntimeException exception) {
                approvals.remove(approvalId, run);
                run.pendingApproval = null;
                run.waitingApproval.set(false);
                throw exception;
            }
        }
    }

    /** Generates a protocol-shaped approval id while allowing more than one ASK in one turn. */
    private String nextApprovalId(Run run) {
        String suffix = run.turnId.value().startsWith("turn_")
                ? run.turnId.value().substring("turn_".length()) : run.turnId.value();
        long sequence = approvalSequence.incrementAndGet();
        String candidate = "appr_" + suffix + "_" + sequence;
        return candidate.length() <= 101 ? candidate : candidate.substring(0, 101);
    }

    /** Converts an accepted response into an asynchronous AgentScope resume operation. */
    private void resolveApproval(String approvalId, TurnRuntime.ApprovalDecision decision) {
        Objects.requireNonNull(decision, "decision");
        Run run = approvals.get(approvalId);
        if (run == null) {
            return;
        }
        PendingApproval pending;
        boolean resolvedEventBudgetFailure = false;
        synchronized (run) {
            if (run.finished.get() || run.cancelled.get() || !run.waitingApproval.get()) {
                return;
            }
            pending = run.pendingApproval;
            if (pending == null || !pending.prompt.approvalId().equals(approvalId)
                    || pending.resolved.get()) {
                return;
            }
            // The response may race AgentScope's asking-stream completion. Store the decision only
            // after winning the one-shot gate; the resume is submitted by tryScheduleApprovalResume
            // once both the decision and the upstream state-save boundary are complete.
            if (!pending.resolved.compareAndSet(false, true)) {
                return;
            }
            pending.decision = decision;
            // Keep the decision fact ahead of resume/tool callbacks while this same run gate is
            // held; cancellation cannot overtake a winner and create an out-of-order timeline.
            if (!publishRunEvents(run, normalizer.approvalResolved(run.normalized, pending.prompt,
                    decision), false)) {
                resolvedEventBudgetFailure = true;
            }
        }
        if (resolvedEventBudgetFailure) {
            streamFailed(run, run.subscriptionEpoch.get(),
                    new IllegalStateException("approval resolved event budget exceeded"));
            return;
        }
        tryScheduleApprovalResume(run, pending);
    }

    /**
     * Resumes only after both HITL inputs are closed: a decision and completion of the asking
     * Flux. AgentScope persists the pending tool state after that Flux completes; waiting here
     * prevents a fast stdio response from reloading a stale state snapshot and issuing the tool
     * call a second time.
     */
    private void tryScheduleApprovalResume(Run run, PendingApproval pending) {
        TurnRuntime.ApprovalDecision decision;
        synchronized (run) {
            if (run.finished.get() || run.cancelled.get() || run.pendingApproval != pending
                    || !run.waitingApproval.get()) {
                return;
            }
            if (!pending.resolved.get() || !pending.askingCompleted.get()
                    || pending.decision == null
                    || !pending.resumeScheduled.compareAndSet(false, true)) {
                return;
            }
            decision = pending.decision;
        }
        try {
            workers.submit(() -> resumeAfterApproval(run, pending, decision));
        } catch (RuntimeException exception) {
            streamFailed(run, exception);
        }
    }

    /** Applies the decision and resubscribes the same AgentScope session without waiting in stdio. */
    private void resumeAfterApproval(Run run, PendingApproval pending,
                                     TurnRuntime.ApprovalDecision decision) {
        try {
            Flux<AgentEvent> resumed;
            long resumeEpoch;
            synchronized (run) {
                // Cancellation and approval resume share this gate. If cancellation wins the
                // gate, no session rule, AgentScope resume, or tool execution may be admitted.
                if (run.finished.get() || run.cancelled.get() || run.pendingApproval != pending
                        || !run.waitingApproval.get()) {
                    return;
                }
                boolean allowed = "allow_once".equals(decision.decision())
                        || "allow_session".equals(decision.decision());
                if ("allow_session".equals(decision.decision())) {
                    engine.allowSession(run.input.userId, run.input.sessionId, pending.toolCalls);
                }
                List<ConfirmResult> confirmations = pending.toolCalls.stream()
                        .map(tool -> new ConfirmResult(allowed, tool))
                        .toList();
                Map<String, Object> metadata = new HashMap<>();
                metadata.put(Msg.METADATA_CONFIRM_RESULTS, confirmations);
                Msg resume = Msg.builder()
                        .name("user")
                        .role(MsgRole.USER)
                        .textContent(allowed ? "approved" : "denied")
                        .metadata(metadata)
                        .build();
                // Reserve the replacement generation before changing waitingApproval. A late
                // callback from the asking Flux can then never observe a resumable run as a fresh
                // terminal. Holding run's gate also makes cancellation wait for this winner.
                resumeEpoch = run.subscriptionEpoch.incrementAndGet();
                approvals.remove(pending.prompt.approvalId(), run);
                run.pendingApproval = null;
                run.waitingApproval.set(false);
                resumed = engine.resume(resume, run.context);
            }
            // The epoch is already authoritative before the replacement subscription is created;
            // subscribing outside the gate keeps provider startup from blocking cancellation.
            subscribeStream(run, resumed, resumeEpoch);
        } catch (RuntimeException exception) {
            streamFailed(run, exception);
        }
    }

    /** Keeps approval summaries useful without copying arbitrary tool arguments to Rust. */
    private static ActionSummary summarizeAction(List<ToolUseBlock> toolCalls) {
        ToolUseBlock first = toolCalls.getFirst();
        String toolName = boundedText(first.getName(), 256);
        String normalized = toolName.toLowerCase(Locale.ROOT);
        String kind = normalized.contains("shell") || normalized.contains("execute")
                || normalized.contains("command") ? "shell"
                : normalized.contains("read") ? "file_read"
                : normalized.contains("write") || normalized.contains("edit")
                || normalized.contains("patch") ? "file_write"
                : normalized.contains("delete") || normalized.contains("remove") ? "file_delete"
                : normalized.contains("mcp") ? "mcp_tool" : "external_tool";
        Map<String, Object> input = first.getInput();
        String command = boundedNullable(input.get("command"), 4_096);
        String cwd = boundedNullable(input.get("cwd"), 4_096);
        List<String> paths = new ArrayList<>();
        Object path = input.get("path");
        if (path instanceof String value) {
            paths.add(boundedText(value, 4_096));
        }
        Object pathList = input.get("paths");
        if (pathList instanceof List<?> values) {
            values.stream().filter(String.class::isInstance).map(String.class::cast)
                    .map(value -> boundedText(value, 4_096)).limit(128).forEach(paths::add);
        }
        String risk = "shell".equals(kind) || "file_delete".equals(kind) ? "high"
                : "file_read".equals(kind) ? "low" : "medium";
        return new ActionSummary(kind, command, cwd, List.copyOf(paths), risk,
                "AgentScope requested confirmation for " + toolName);
    }

    /** Bounds optional model-provided values before they cross the approval request boundary. */
    private static String boundedNullable(Object value, int maxLength) {
        return value instanceof String text && !text.isBlank()
                ? boundedText(text, maxLength) : null;
    }

    /** Bounds one display string without retaining arbitrary provider payloads. */
    private static String boundedText(String value, int maxLength) {
        if (value == null || value.isEmpty()) {
            return "";
        }
        return value.length() <= maxLength ? value : value.substring(0, maxLength);
    }

    /** Completes the current event stream while ignoring stale asking-stream callbacks. */
    private void streamCompleted(Run run, long epoch) {
        synchronized (run) {
            if (!isCurrentStream(run, epoch)) {
                return;
            }
            if (run.waitingApproval.get()) {
                // AgentScope intentionally completes the asking Flux with PERMISSION_ASKING. Marking
                // that boundary lets a response which arrived first resume only after state persistence
                // has drained; the session lane remains occupied until that resume becomes terminal.
                PendingApproval pending = run.pendingApproval;
                if (pending != null) {
                    pending.askingCompleted.set(true);
                    tryScheduleApprovalResume(run, pending);
                }
                return;
            }
        }
        finish(run, run.cancelled.get() ? "interrupted" : "completed",
                run.cancelled.get() ? "cancelled" : null, epoch);
    }

    /** Reduces current-provider exceptions while ignoring only callbacks from stale epochs. */
    private void streamFailed(Run run, long epoch, Throwable error) {
        if (!isCurrentStream(run, epoch)) {
            return;
        }
        String reason = run.resourceOverflow.get()
                || (run.normalized != null && run.normalized.isOverflowed())
                ? "event_budget_exceeded"
                : deadlineExpired(run) ? "deadline_exceeded" : "provider_error";
        finish(run, run.cancelled.get() ? "interrupted" : "failed",
                run.cancelled.get() ? (deadlineExpired(run) ? "deadline_exceeded" : "cancelled") : reason,
                epoch);
    }

    /** Routes control-lane failures to the currently reserved generation. */
    private void streamFailed(Run run, Throwable error) {
        streamFailed(run, run.subscriptionEpoch.get(), error);
    }

    /** Rejects callbacks from an asking stream after a newer resume stream is reserved. */
    private static boolean isCurrentStream(Run run, long epoch) {
        return run.subscriptionEpoch.get() == epoch;
    }

    /** Tests the monotonic absolute deadline rather than relying on wall-clock changes. */
    private boolean deadlineExpired(Run run) {
        return System.nanoTime() >= run.deadlineNanos;
    }

    /** Enforces turn and session event/item/metadata/byte limits before UI publication. */
    private boolean admitOutput(Run run, TurnEvent event) {
        int eventBytes = utf8Bytes(event.params().toString());
        int itemCount = "item/started".equals(event.method()) ? 1 : 0;
        int metadataBytes = event.params().path("item").has("metadata")
                ? utf8Bytes(event.params().path("item").path("metadata").toString()) : 0;
        synchronized (run.lane) {
            if (eventBytes > config.turnLimits().maxSingleEventBytes()
                    || run.turnEventCount + 1 > config.turnLimits().maxEvents()
                    || run.turnItemCount + itemCount > config.turnLimits().maxItems()
                    || (long) run.turnMetadataBytes + metadataBytes
                    > config.turnLimits().maxMetadataBytes()
                    || run.turnEventBytes + eventBytes > config.turnLimits().maxEventBytes()
                    || run.lane.eventBytes + eventBytes > config.sessionLimits().maxEventBytes()
                    || eventBytes > config.sessionLimits().maxSingleEventBytes()
                    || run.lane.eventCount + 1 > config.sessionLimits().maxEvents()
                    || run.lane.itemCount + itemCount > config.sessionLimits().maxItems()
                    || (long) run.lane.metadataBytes + metadataBytes
                    > config.sessionLimits().maxMetadataBytes()) {
                return false;
            }
            run.turnEventCount++;
            run.turnItemCount += itemCount;
            run.turnMetadataBytes += metadataBytes;
            run.turnEventBytes += eventBytes;
            run.lane.eventCount++;
            run.lane.itemCount += itemCount;
            run.lane.metadataBytes += metadataBytes;
            run.lane.eventBytes += eventBytes;
            return true;
        }
    }

    /** Counts UTF-8 bytes for bounded JSON already produced by the normalizer. */
    private static int utf8Bytes(String value) {
        if (value == null || value.isEmpty()) {
            return 0;
        }
        int bytes = 0;
        for (int index = 0; index < value.length(); index++) {
            char character = value.charAt(index);
            if (Character.isHighSurrogate(character) && index + 1 < value.length()
                    && Character.isLowSurrogate(value.charAt(index + 1))) {
                bytes = Math.addExact(bytes, 4);
                index++;
            } else if (character <= 0x7F) {
                bytes = Math.addExact(bytes, 1);
            } else if (character <= 0x7FF) {
                bytes = Math.addExact(bytes, 2);
            } else {
                bytes = Math.addExact(bytes, 3);
            }
        }
        return bytes;
    }

    /**
     * Emits one terminal event, releases the session lane, and wakes shutdown
     * waiters; all three operations are guarded against duplicate callbacks.
     */
    private void finish(Run run, String status, String reason) {
        finish(run, status, reason, null);
    }

    /**
     * Emits a terminal only when the callback still belongs to the reserved stream epoch.  The
     * epoch check must happen in the same critical section as the terminal claim; a check followed
     * by a separate finish call would still let a resume switch in between and close the new stream.
     */
    private void finish(Run run, String status, String reason, Long expectedEpoch) {
        PendingApproval pending;
        synchronized (run) {
            if (expectedEpoch != null && run.subscriptionEpoch.get() != expectedEpoch) {
                return;
            }
            if (!run.finished.compareAndSet(false, true)) {
                return;
            }
            if (run.cancelled.get() && !"interrupted".equals(status)) {
                status = "interrupted";
                reason = reason == null ? "cancelled" : reason;
            }
            pending = run.pendingApproval;
            if (pending != null) {
                approvals.remove(pending.prompt.approvalId(), run);
                run.pendingApproval = null;
                run.waitingApproval.set(false);
            }
        }
        cancelPendingApproval(pending);
        ScheduledFuture<?> deadlineTask = run.deadlineTask;
        if (deadlineTask != null) {
            deadlineTask.cancel(false);
        }
        try {
            EventNormalizer.Context context = run.normalized;
            if (context == null) {
                context = normalizer.open(run.input.threadId, run.turnId,
                        run.input.mode, run.input.accessMode);
                run.normalized = context;
            }
            for (TurnEvent terminal : normalizer.terminal(context, status, reason)) {
                try {
                    run.publisher.accept(terminal);
                } catch (RuntimeException ignored) {
                    // A broken consumer cannot justify publishing a second terminal event.
                }
            }
        } finally {
            runs.remove(run.turnId, run);
            acceptedCount.decrementAndGet();
            acceptedInputBytes.addAndGet(-run.input.inputBytes());
            SessionLane lane;
            boolean releaseThreadSequence = false;
            // Lane removal and admission use one lock order so a new request
            // cannot retain a removed lane and split one session into two FIFOs.
            synchronized (admissionMonitor) {
                lane = lanes.get(run.sessionKey);
                if (lane != null) {
                    synchronized (lane) {
                        lane.queue.remove(run);
                        if (lane.running == run) {
                            lane.running = null;
                        }
                        if (lane.running == null && lane.queue.isEmpty()) {
                            lanes.remove(run.sessionKey, lane);
                            releaseThreadSequence = runs.values().stream()
                                    .noneMatch(other -> other.input.threadId.equals(run.input.threadId));
                        }
                    }
                }
            }
            if (releaseThreadSequence) {
                normalizer.releaseThread(run.input.threadId);
            }
            if (lane != null) {
                scheduleNext(lane);
            }
            synchronized (quiescenceMonitor) {
                quiescenceMonitor.notifyAll();
            }
        }
    }

    /** Immutable runtime resource bounds; no unbounded request/session map is allowed. */
    public record Config(int maxConcurrentSessions, int maxAcceptedTurns, int maxInputBytes,
                         Duration closeTimeout, Duration turnTimeout,
                         ResourceLimits turnLimits, ResourceLimits sessionLimits,
                         int maxQueuedInputBytes) {
        /** Preserves the initial constructor while giving queued input an explicit aggregate cap. */
        public Config(int maxConcurrentSessions, int maxAcceptedTurns, int maxInputBytes,
                      Duration closeTimeout, Duration turnTimeout,
                      ResourceLimits turnLimits, ResourceLimits sessionLimits) {
            this(maxConcurrentSessions, maxAcceptedTurns, maxInputBytes, closeTimeout, turnTimeout,
                    turnLimits, sessionLimits, Math.max(maxInputBytes, 16 * 1024 * 1024));
        }
        /** Keeps the original compact constructor useful for focused callers and tests. */
        public Config(int maxConcurrentSessions, int maxAcceptedTurns, int maxInputBytes,
                      Duration closeTimeout) {
            this(maxConcurrentSessions, maxAcceptedTurns, maxInputBytes, closeTimeout,
                    Duration.ofMinutes(5), ResourceLimits.turnDefaults(),
                    ResourceLimits.sessionDefaults(), 64 * 1024 * 1024);
        }

        /** Validates limits before worker or queue resources are allocated. */
        public Config {
            if (maxConcurrentSessions < 1 || maxConcurrentSessions > 1_024
                    || maxAcceptedTurns < 1 || maxAcceptedTurns > 10_000
                    || maxInputBytes < 1 || maxInputBytes > 16 * 1024 * 1024) {
                throw new IllegalArgumentException("AgentScope runtime limits are invalid");
            }
            closeTimeout = Objects.requireNonNull(closeTimeout, "closeTimeout");
            if (closeTimeout.isNegative() || closeTimeout.isZero()
                    || closeTimeout.compareTo(Duration.ofMinutes(5)) > 0) {
                throw new IllegalArgumentException("close timeout is invalid");
            }
            turnTimeout = Objects.requireNonNull(turnTimeout, "turnTimeout");
            if (turnTimeout.isNegative() || turnTimeout.isZero()
                    || turnTimeout.compareTo(Duration.ofHours(1)) > 0) {
                throw new IllegalArgumentException("turn timeout is invalid");
            }
            turnLimits = Objects.requireNonNull(turnLimits, "turnLimits");
            sessionLimits = Objects.requireNonNull(sessionLimits, "sessionLimits");
            if (maxQueuedInputBytes < maxInputBytes || maxQueuedInputBytes > 256 * 1024 * 1024) {
                throw new IllegalArgumentException("queued input budget is invalid");
            }
        }

        /** Returns the bounded production scheduler baseline. */
        public static Config defaults() {
            return new Config(16, 128, 4 * 1024 * 1024, Duration.ofSeconds(5),
                    Duration.ofMinutes(5), ResourceLimits.turnDefaults(),
                    ResourceLimits.sessionDefaults(), 64 * 1024 * 1024);
        }
    }

    /** Count and byte ceilings applied independently to turns and session lanes. */
    public record ResourceLimits(int maxEvents, int maxItems, int maxMetadataBytes,
                                 int maxEventBytes, int maxSingleEventBytes) {
        /** Keeps compact test construction while preserving a separate total/event ceiling. */
        public ResourceLimits(int maxEvents, int maxItems, int maxMetadataBytes,
                              int maxEventBytes) {
            this(maxEvents, maxItems, maxMetadataBytes, maxEventBytes, maxEventBytes);
        }

        /** Validates limits before a stream can reserve resources. */
        public ResourceLimits {
            if (maxEvents < 1 || maxEvents > 1_000_000 || maxItems < 1 || maxItems > 100_000
                    || maxMetadataBytes < 1 || maxMetadataBytes > 64 * 1024 * 1024
                    || maxEventBytes < 1_024 || maxEventBytes > 256 * 1024 * 1024
                    || maxSingleEventBytes < 1_024 || maxSingleEventBytes > 16 * 1024 * 1024
                    || maxSingleEventBytes > maxEventBytes) {
                throw new IllegalArgumentException("AgentScope resource limits are invalid");
            }
        }

        /** Gives each turn a generous but finite UI publication envelope. */
        public static ResourceLimits turnDefaults() {
            return new ResourceLimits(20_000, 4_096, 1_048_576,
                    64 * 1024 * 1024, 4 * 1024 * 1024);
        }

        /** Gives one session lane a finite aggregate envelope across queued turns. */
        public static ResourceLimits sessionDefaults() {
            return new ResourceLimits(100_000, 20_000, 16 * 1024 * 1024,
                    256 * 1024 * 1024, 4 * 1024 * 1024);
        }
    }

    private static final class SessionLane {
        private final ArrayDeque<Run> queue = new ArrayDeque<>();
        private Run running;
        private int eventCount;
        private int itemCount;
        private int metadataBytes;
        private int eventBytes;

        private SessionLane() {
        }
    }

    private static final class Run {
        private final TurnId turnId;
        private final SessionKey sessionKey;
        private final Input input;
        private final Consumer<TurnEvent> publisher;
        private final AtomicBoolean cancelled = new AtomicBoolean();
        private final AtomicBoolean finished = new AtomicBoolean();
        private final AtomicBoolean resourceOverflow = new AtomicBoolean();
        private final AtomicBoolean waitingApproval = new AtomicBoolean();
        private final AtomicLong subscriptionEpoch = new AtomicLong();
        private volatile SessionLane lane;
        private volatile long deadlineNanos;
        private volatile ScheduledFuture<?> deadlineTask;
        private int turnEventCount;
        private int turnItemCount;
        private int turnMetadataBytes;
        private int turnEventBytes;
        private volatile RuntimeContext context;
        private volatile EventNormalizer.Context normalized;
        private volatile Disposable subscription;
        private volatile PendingApproval pendingApproval;

        private Run(TurnId turnId, SessionKey sessionKey, Input input,
                    Consumer<TurnEvent> publisher) {
            this.turnId = turnId;
            this.sessionKey = sessionKey;
            this.input = input;
            this.publisher = publisher;
        }
    }

    /** Keeps the exact upstream ASK blocks needed by AgentScope's resume contract. */
    private static final class PendingApproval {
        private final TurnRuntime.ApprovalPrompt prompt;
        private final List<ToolUseBlock> toolCalls;
        private final AtomicBoolean resolved = new AtomicBoolean();
        private final AtomicBoolean askingCompleted = new AtomicBoolean();
        private final AtomicBoolean resumeScheduled = new AtomicBoolean();
        private volatile TurnRuntime.ApprovalHandle cancelHandle;
        private volatile TurnRuntime.ApprovalDecision decision;

        private PendingApproval(TurnRuntime.ApprovalPrompt prompt, List<ToolUseBlock> toolCalls) {
            this.prompt = Objects.requireNonNull(prompt, "prompt");
            this.toolCalls = List.copyOf(toolCalls);
        }
    }

    /** Redacted action details used only to construct the frozen approval/request params. */
    private record ActionSummary(String kind, String command, String cwd, List<String> relativePaths,
                                 String risk, String reason) {
    }

    private record Input(ThreadId threadId, String userId, String sessionId, String text,
                         String requestedTurnId, String mode, String accessMode,
                         int inputBytes) {
        /** Parses and bounds the public turn/start request before AgentScope sees it. */
        private static Input parse(ObjectNode params, int maxInputBytes) {
            try {
                ThreadId threadId = new ThreadId(required(params, "threadId"));
                String userId = optional(params, "userId", "ja-user");
                String sessionId = optional(params, "sessionId", threadId.value());
                String requestedTurnId = params.has("turnId") ? required(params, "turnId") : null;
                if (requestedTurnId != null && !requestedTurnId.startsWith("turn_")) {
                    throw new IllegalArgumentException("turn id prefix");
                }
                String mode = optional(params, "mode", "coding");
                // The public contract calls this value accessMode. Keep the old internal field as
                // a direct-runtime test fallback only; the production graph requires accessMode
                // before this parser is reached and maps it to AgentScope PermissionMode.
                String accessMode = params.has("accessMode")
                        ? required(params, "accessMode")
                        : optional(params, "permissionMode", "workspace");
                if (!java.util.Set.of("read_only", "workspace", "full_access")
                        .contains(accessMode)) {
                    throw new IllegalArgumentException("access mode");
                }
                JsonNode input = params.get("input");
                if (input == null || !input.isArray() || input.isEmpty() || input.size() > 128) {
                    throw new IllegalArgumentException("input");
                }
                BoundedTextBuilder text = new BoundedTextBuilder(maxInputBytes);
                int index = 0;
                for (JsonNode part : input) {
                    if (part == null || !part.isObject()) {
                        throw new IllegalArgumentException("input part");
                    }
                    if (index++ > 0) {
                        text.append("\n");
                    }
                    String type = required((ObjectNode) part, "type");
                    if ("text".equals(type)) {
                        text.append(required((ObjectNode) part, "text"));
                    } else {
                        throw new IllegalArgumentException("input part type");
                    }
                }
                String value = text.toString();
                if (value.isBlank()) {
                    throw new IllegalArgumentException("input size");
                }
                return new Input(threadId, userId, sessionId, value, requestedTurnId,
                        mode, accessMode, text.bytes());
            } catch (RuntimeException exception) {
                if (exception instanceof ProtocolException protocol) {
                    throw protocol;
                }
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS, null, exception);
            }
        }

        /** Reads a required textual parameter without allowing null JSON coercion. */
        private static String required(ObjectNode params, String name) {
            JsonNode node = params.get(name);
            if (node == null || !node.isTextual() || node.textValue().isBlank()
                    || node.textValue().length() > 1024) {
                throw new IllegalArgumentException(name);
            }
            return node.textValue();
        }

        /** Uses a fixed safe default for optional session routing fields. */
        private static String optional(ObjectNode params, String name, String fallback) {
            return params.has(name) ? required(params, name) : fallback;
        }
    }

    /** Builds provider input while enforcing the UTF-8 ceiling before retaining more text. */
    private static final class BoundedTextBuilder {
        private final int maxBytes;
        private final StringBuilder value = new StringBuilder();
        private int bytes;

        private BoundedTextBuilder(int maxBytes) {
            this.maxBytes = maxBytes;
        }

        /** Appends one validated chunk and fails as soon as the aggregate budget is crossed. */
        private BoundedTextBuilder append(String chunk) {
            Objects.requireNonNull(chunk, "chunk");
            for (int index = 0; index < chunk.length(); index++) {
                char character = chunk.charAt(index);
                int increment;
                if (Character.isHighSurrogate(character)) {
                    if (index + 1 >= chunk.length()
                            || !Character.isLowSurrogate(chunk.charAt(index + 1))) {
                        throw new IllegalArgumentException("input contains malformed UTF-16");
                    }
                    increment = 4;
                    index++;
                } else if (Character.isLowSurrogate(character)) {
                    throw new IllegalArgumentException("input contains malformed UTF-16");
                } else if (character <= 0x7F) {
                    increment = 1;
                } else if (character <= 0x7FF) {
                    increment = 2;
                } else {
                    increment = 3;
                }
                if (bytes > maxBytes - increment) {
                    throw new IllegalArgumentException("input size");
                }
                bytes += increment;
            }
            value.append(chunk);
            return this;
        }

        /** Returns the bounded provider input after all chunks have passed admission. */
        @Override
        public String toString() {
            return value.toString();
        }

        /** Returns the already-accounted UTF-8 size without re-encoding the full input. */
        private int bytes() {
            return bytes;
        }
    }
}
