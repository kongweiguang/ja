// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.event.AgentEvent;
import io.agentscope.core.event.AgentStartEvent;
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
import java.util.ArrayDeque;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
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
    private final AtomicLong turnSequence = new AtomicLong();
    private final AtomicInteger acceptedCount = new AtomicInteger();
    private final AtomicInteger acceptedInputBytes = new AtomicInteger();
    private final Object admissionMonitor = new Object();
    private final Object quiescenceMonitor = new Object();
    private final AtomicBoolean accepting = new AtomicBoolean(true);
    private final AtomicBoolean closed = new AtomicBoolean();

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

    /** Applies one cancellation path to queued and running turns, preserving the reason. */
    private boolean cancelInternal(TurnId turnId, String reason) {
        Run run = runs.get(Objects.requireNonNull(turnId, "turnId"));
        if (run == null || !run.cancelled.compareAndSet(false, true)) {
            return false;
        }
        SessionLane lane = lanes.get(run.sessionKey);
        boolean queued = false;
        if (lane != null) {
            synchronized (lane) {
                if (lane.queue.remove(run)) {
                    queued = true;
                }
            }
        }
        if (queued) {
            finish(run, "interrupted", reason);
            return true;
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
        finish(run, "interrupted", reason);
        return true;
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
                    run.input.mode, run.input.permissionMode);
            run.context = contextFactory.create(run.sessionKey, Map.of(
                    "ja.threadId", run.input.threadId.value(),
                    "ja.turnId", run.turnId.value(),
                    "ja.deadline", Long.toString(run.deadlineNanos)));
        try {
            // Emit a product-owned start boundary even if a provider stream
            // omits AgentStart; the normalizer suppresses a later duplicate.
            publishNormalized(run, new AgentStartEvent(run.sessionKey.sessionId(),
                    run.turnId.value(), "ja"));
            if (run.finished.get() || run.cancelled.get()) {
                return;
            }
            Flux<AgentEvent> stream = engine.stream(run.input.text, run.context);
            Disposable subscription = stream.subscribe(
                    event -> publishNormalized(run, event),
                    error -> streamFailed(run, error),
                    () -> streamCompleted(run));
            synchronized (run) {
                run.subscription = subscription;
                if (run.cancelled.get()) {
                    subscription.dispose();
                }
            }
        } catch (RuntimeException exception) {
            streamFailed(run, exception);
        }
    }

    /** Publishes normalized events while suppressing callbacks after cancellation. */
    private void publishNormalized(Run run, AgentEvent event) {
        if (run.finished.get() || run.cancelled.get()) {
            return;
        }
        if (deadlineExpired(run)) {
            cancelInternal(run.turnId, "deadline_exceeded");
            return;
        }
        try {
            List<TurnEvent> normalized = normalizer.normalize(event, run.normalized);
            boolean terminalBatch = normalizer.isTerminal(run.normalized);
            for (TurnEvent output : normalized) {
                if (!run.finished.get() && !run.cancelled.get()) {
                    if (!terminalBatch && !admitOutput(run, output)) {
                        run.resourceOverflow.set(true);
                        streamFailed(run, new IllegalStateException("event budget exceeded"));
                        return;
                    }
                    run.publisher.accept(output);
                }
            }
        } catch (RuntimeException exception) {
            streamFailed(run, exception);
        }
    }

    /** Completes an event stream with a successful terminal status when needed. */
    private void streamCompleted(Run run) {
        finish(run, run.cancelled.get() ? "interrupted" : "completed",
                run.cancelled.get() ? "cancelled" : null);
    }

    /** Reduces provider/callback exceptions to a stable, non-secret terminal reason. */
    private void streamFailed(Run run, Throwable error) {
        String reason = run.resourceOverflow.get()
                || (run.normalized != null && run.normalized.isOverflowed())
                ? "event_budget_exceeded"
                : deadlineExpired(run) ? "deadline_exceeded" : "provider_error";
        finish(run, run.cancelled.get() ? "interrupted" : "failed",
                run.cancelled.get() ? (deadlineExpired(run) ? "deadline_exceeded" : "cancelled") : reason);
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
        if (!run.finished.compareAndSet(false, true)) {
            return;
        }
        ScheduledFuture<?> deadlineTask = run.deadlineTask;
        if (deadlineTask != null) {
            deadlineTask.cancel(false);
        }
        try {
            EventNormalizer.Context context = run.normalized;
            if (context == null) {
                context = normalizer.open(run.input.threadId, run.turnId,
                        run.input.mode, run.input.permissionMode);
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

        private Run(TurnId turnId, SessionKey sessionKey, Input input,
                    Consumer<TurnEvent> publisher) {
            this.turnId = turnId;
            this.sessionKey = sessionKey;
            this.input = input;
            this.publisher = publisher;
        }
    }

    private record Input(ThreadId threadId, String userId, String sessionId, String text,
                         String requestedTurnId, String mode, String permissionMode,
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
                String permission = optional(params, "permissionMode", "workspace");
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
                    } else if ("attachment".equals(type)) {
                        text.append("[attachment:")
                                .append(required((ObjectNode) part, "attachmentId"))
                                .append("]");
                    } else {
                        throw new IllegalArgumentException("input part type");
                    }
                }
                String value = text.toString();
                if (value.isBlank()) {
                    throw new IllegalArgumentException("input size");
                }
                return new Input(threadId, userId, sessionId, value, requestedTurnId,
                        mode, permission, text.bytes());
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
