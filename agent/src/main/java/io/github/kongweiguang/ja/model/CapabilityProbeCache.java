// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import io.github.kongweiguang.ja.profiles.CapabilitySet;
import io.github.kongweiguang.ja.profiles.ModelCapability;
import io.github.kongweiguang.ja.profiles.ModelProfile;
import io.github.kongweiguang.ja.profiles.ModelProfileValidator;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.Future;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Thread-safe capability cache with bounded single-flight cooperative probe execution.
 *
 * <p>The transport port is intentionally cooperative. Java cannot safely kill a thread that ignores
 * interruption, so a stalled task becomes a permanent fail-closed cache state after a bounded grace
 * period; no new provider work is admitted after that point.
 */
public final class CapabilityProbeCache implements AutoCloseable {
    private static final Duration DEFAULT_TIMEOUT = Duration.ofSeconds(5);
    private static final Duration CLOSE_WAIT = Duration.ofSeconds(1);
    // A cooperative provider may need a few scheduler turns to observe cancellation on a busy
    // Windows runner; keep the grace finite but wide enough to avoid false permanent-faults.
    private static final Duration STOP_GRACE = Duration.ofMillis(250);
    private static final Duration IDLE_WORKER_TIMEOUT = Duration.ofMillis(100);
    private static final int DEFAULT_CONCURRENCY = 4;
    private static final int DEFAULT_MAX_ENTRIES = 256;
    private static final long WAIT_SLICE_NANOS = TimeUnit.MILLISECONDS.toNanos(25);

    private final Map<CacheKey, CapabilityProbeResult> entries = new HashMap<>();
    private final Map<CacheKey, ProbeWork> inFlight = new HashMap<>();
    private final ThreadPoolExecutor executor;
    private final long timeoutNanos;
    private final int maxEntries;
    private final Object lifecycleLock = new Object();
    private final Map<String, Long> profileEpochs = new HashMap<>();
    private long generation;
    private boolean closed;
    private boolean permanentlyFaulted;

    /** Uses bounded defaults so an unconfigured provider cannot stall the sidecar indefinitely. */
    public CapabilityProbeCache() {
        this(DEFAULT_TIMEOUT, DEFAULT_CONCURRENCY, DEFAULT_MAX_ENTRIES);
    }

    /** Configures an explicit deadline while retaining bounded worker and cache limits. */
    public CapabilityProbeCache(Duration timeout) {
        this(timeout, DEFAULT_CONCURRENCY, DEFAULT_MAX_ENTRIES);
    }

    /** Creates a zero-queue executor; a second unique key is rejected instead of waiting unboundedly. */
    public CapabilityProbeCache(Duration timeout, int maxConcurrency) {
        this(timeout, maxConcurrency, DEFAULT_MAX_ENTRIES);
    }

    /** Creates a bounded cache and cooperative executor for deterministic lifecycle behavior. */
    public CapabilityProbeCache(Duration timeout, int maxConcurrency, int maxEntries) {
        Objects.requireNonNull(timeout, "timeout");
        if (timeout.isZero() || timeout.isNegative() || maxConcurrency <= 0 || maxEntries <= 0) {
            throw new IllegalArgumentException("probe limits must be positive");
        }
        try {
            timeoutNanos = timeout.toNanos();
        } catch (ArithmeticException exception) {
            throw new IllegalArgumentException("probe timeout is too large");
        }
        if (timeoutNanos <= 0) {
            throw new IllegalArgumentException("probe timeout is too small");
        }
        this.maxEntries = maxEntries;
        ThreadFactory factory = new ProbeThreadFactory();
        // SynchronousQueue is an intentional zero-capacity queue: deadline accounting never hides queue wait.
        executor = new ThreadPoolExecutor(maxConcurrency, maxConcurrency,
                IDLE_WORKER_TIMEOUT.toNanos(), TimeUnit.NANOSECONDS,
                new SynchronousQueue<>(), factory, new ThreadPoolExecutor.AbortPolicy());
        executor.allowCoreThreadTimeOut(true);
    }

    /** Probes with a fresh cooperative cancellation token. */
    public CapabilityProbeResult probe(ModelProfile profile, CapabilityProbeTransport transport) {
        return probe(profile, "probe-v1", transport, new CapabilityProbeCancellation());
    }

    /**
     * Shares one in-flight future per cache key and establishes the caller deadline before profile
     * revision lookup or executor admission, so every wait consumes the same bounded time budget.
     */
    public CapabilityProbeResult probe(ModelProfile profile, CapabilityProbeTransport transport,
                                       CapabilityProbeCancellation cancellation) {
        return probe(profile, "probe-v1", transport, cancellation);
    }

    /** Uses an explicit bounded in-memory revision so key formation cannot perform provider I/O. */
    public CapabilityProbeResult probe(ModelProfile profile, String transportRevision,
                                       CapabilityProbeTransport transport) {
        return probe(profile, transportRevision, transport, new CapabilityProbeCancellation());
    }

    /**
     * Shares one in-flight future with an explicit revision supplied by the provider registration,
     * not computed by an arbitrary transport callback.
     */
    public CapabilityProbeResult probe(ModelProfile profile, String transportRevision,
                                       CapabilityProbeTransport transport,
                                       CapabilityProbeCancellation cancellation) {
        long callerDeadline = deadline(timeoutNanos);
        Objects.requireNonNull(profile, "profile");
        Objects.requireNonNull(transport, "transport");
        Objects.requireNonNull(cancellation, "cancellation");
        if (cancellation.isCancelled()) {
            return failure("unknown", "unknown", CapabilityProbeStatus.CANCELLED,
                    CapabilityProbeFailureCode.CANCELLED);
        }
        synchronized (lifecycleLock) {
            if (closed) {
                return failure("unknown", "unknown", CapabilityProbeStatus.FAILED,
                        CapabilityProbeFailureCode.CLOSED);
            }
            if (permanentlyFaulted) {
                return failure("unknown", "unknown", CapabilityProbeStatus.FAILED,
                        CapabilityProbeFailureCode.PERMANENT_FAULT);
            }
        }
        ModelProfileValidator.requireValid(profile);
        String profileRevision = profile.fingerprint();
        transportRevision = boundedRevision(transportRevision);
        CacheKey key = new CacheKey(profile.id(), profileRevision, transportRevision);
        ProbeWork work;
        boolean owner = false;
        synchronized (lifecycleLock) {
            if (closed) {
                return failure(profileRevision, transportRevision, CapabilityProbeStatus.FAILED,
                        CapabilityProbeFailureCode.CLOSED);
            }
            if (permanentlyFaulted) {
                return failure(profileRevision, transportRevision, CapabilityProbeStatus.FAILED,
                        CapabilityProbeFailureCode.PERMANENT_FAULT);
            }
            if (cancellation.isCancelled()) {
                return failure(profileRevision, transportRevision, CapabilityProbeStatus.CANCELLED,
                        CapabilityProbeFailureCode.CANCELLED);
            }
            CapabilityProbeResult cached = entries.get(key);
            if (cached != null) {
                return cached;
            }
            work = inFlight.get(key);
            if (work == null) {
                if (entries.size() >= maxEntries) {
                    return failure(profileRevision, transportRevision, CapabilityProbeStatus.FAILED,
                            CapabilityProbeFailureCode.CACHE_FULL);
                }
                CapabilityProbeContext context = new CapabilityProbeContext(callerDeadline,
                        new CapabilityProbeCancellation(), ModelCapability.values().length, 128);
                work = new ProbeWork(key, profile, transport, context, generation,
                        profileEpochs.getOrDefault(key.profileId(), 0L));
                inFlight.put(key, work);
                owner = true;
                try {
                    ProbeWork admitted = work;
                    work.task = executor.submit(() -> run(admitted));
                } catch (RejectedExecutionException exception) {
                    inFlight.remove(key, work);
                    completeAdmissionFailure(work, CapabilityProbeFailureCode.OVERLOADED);
                }
            }
        }
        return await(work, cancellation, callerDeadline, owner);
    }

    /** Invalidates one profile epoch so its late in-flight result cannot repopulate the cache. */
    public void invalidate(String profileId) {
        Objects.requireNonNull(profileId, "profileId");
        List<ProbeWork> stale;
        synchronized (lifecycleLock) {
            boolean global = profileEpochs.size() >= maxEntries && !profileEpochs.containsKey(profileId);
            if (global) {
                generation++;
                profileEpochs.clear();
                // The bounded epoch table has fallen back to a generation barrier, so every
                // previously cached result belongs to the superseded generation as well.
                entries.clear();
            } else {
                profileEpochs.put(profileId, profileEpochs.getOrDefault(profileId, 0L) + 1);
            }
            if (!global) {
                entries.entrySet().removeIf(entry -> entry.getKey().profileId().equals(profileId));
            }
            stale = new ArrayList<>(global ? inFlight.values() : List.of());
            inFlight.entrySet().removeIf(entry -> {
                if (global || entry.getKey().profileId().equals(profileId)) {
                    if (!global) {
                        stale.add(entry.getValue());
                    }
                    return true;
                }
                return false;
            });
        }
        stopAll(stale, CapabilityProbeFailureCode.CANCELLED, true);
    }

    /** Clears cache and in-flight work under a new generation, preventing old tasks from repopulating it. */
    public void clear() {
        List<ProbeWork> stale;
        synchronized (lifecycleLock) {
            generation++;
            profileEpochs.clear();
            entries.clear();
            stale = new ArrayList<>(inFlight.values());
            inFlight.clear();
        }
        stopAll(stale, CapabilityProbeFailureCode.CANCELLED, true);
    }

    /** Exposes only the bounded cache entry count for diagnostics. */
    public int size() {
        synchronized (lifecycleLock) {
            return entries.size();
        }
    }

    /** Exposes the bounded single-flight count so overload tests can assert no slot leak. */
    public int inFlightSize() {
        synchronized (lifecycleLock) {
            return inFlight.size();
        }
    }

    /** Reports the permanent fail-closed state caused by a transport that ignored cancellation. */
    public boolean isPermanentlyFaulted() {
        synchronized (lifecycleLock) {
            return permanentlyFaulted;
        }
    }

    /** Idempotently invalidates all work and waits only within a bounded close deadline. */
    @Override
    public void close() {
        List<ProbeWork> active;
        synchronized (lifecycleLock) {
            if (closed) {
                return;
            }
            closed = true;
            generation++;
            profileEpochs.clear();
            entries.clear();
            active = new ArrayList<>(inFlight.values());
            inFlight.clear();
            executor.shutdownNow();
        }
        stopAll(active, CapabilityProbeFailureCode.CLOSED, false);
        try {
            executor.awaitTermination(CLOSE_WAIT.toNanos(), TimeUnit.NANOSECONDS);
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
        }
    }

    /** Executes one cooperative transport and funnels every result through epoch-aware publication. */
    private void run(ProbeWork work) {
        work.started.set(true);
        CapabilityProbeResult result;
        try {
            work.context.checkActive();
            CapabilitySet output = work.transport.probe(work.profile, work.context);
            work.context.checkActive();
            if (output == null || output.supported().size() > work.context.maxCapabilities()) {
                result = failure(work.key.profileRevision(), work.key.transportRevision(),
                        CapabilityProbeStatus.FAILED, CapabilityProbeFailureCode.INVALID_OUTPUT);
            } else {
                result = CapabilityProbeResult.success(work.key.profileRevision(),
                        work.key.transportRevision(), output.apply(work.profile.capabilityOverrides()));
            }
        } catch (CapabilityProbeTimeoutException exception) {
            result = failure(work.key.profileRevision(), work.key.transportRevision(),
                    CapabilityProbeStatus.TIMEOUT, CapabilityProbeFailureCode.TIMEOUT);
        } catch (CancellationException exception) {
            result = failure(work.key.profileRevision(), work.key.transportRevision(),
                    CapabilityProbeStatus.CANCELLED, CapabilityProbeFailureCode.CANCELLED);
        } catch (Throwable ignored) {
            // Provider exception text is deliberately discarded at this boundary.
            result = failure(work.key.profileRevision(), work.key.transportRevision(),
                    CapabilityProbeStatus.FAILED, CapabilityProbeFailureCode.FAILED);
        }
        try {
            finish(work, result);
        } finally {
            work.terminated.countDown();
        }
    }

    /** Publishes exactly once and only when the work still belongs to the current generation. */
    private void finish(ProbeWork work, CapabilityProbeResult result) {
        synchronized (lifecycleLock) {
            if (!work.finished.compareAndSet(false, true)) {
                return;
            }
            CapabilityProbeResult forced = work.forced;
            boolean current = !closed && !permanentlyFaulted && work.generation == generation
                    && work.profileEpoch == profileEpochs.getOrDefault(work.key.profileId(), 0L);
            CapabilityProbeResult published = forced != null ? forced : result;
            if (!current && forced == null) {
                published = failure(work.key.profileRevision(), work.key.transportRevision(),
                        CapabilityProbeStatus.CANCELLED, CapabilityProbeFailureCode.CANCELLED);
            }
            inFlight.remove(work.key, work);
            if (current && cacheable(published) && entries.size() < maxEntries) {
                entries.putIfAbsent(work.key, published);
            }
            work.result.complete(published);
        }
    }

    /** Handles executor rejection without leaving a phantom in-flight key behind. */
    private void completeAdmissionFailure(ProbeWork work, CapabilityProbeFailureCode code) {
        work.forced = failure(work.key.profileRevision(), work.key.transportRevision(),
                CapabilityProbeStatus.FAILED, code);
        work.finished.set(true);
        work.result.complete(work.forced);
        work.terminated.countDown();
    }

    /** Waits on the shared future without allowing a joining caller to cancel another caller's work. */
    private CapabilityProbeResult await(ProbeWork work, CapabilityProbeCancellation cancellation,
                                        long callerDeadline, boolean owner) {
        while (true) {
            try {
                long remaining = remaining(callerDeadline);
                if (cancellation.isCancelled()) {
                    return owner ? stop(work, CapabilityProbeFailureCode.CANCELLED, true)
                            : failure(work.key.profileRevision(), work.key.transportRevision(),
                            CapabilityProbeStatus.CANCELLED, CapabilityProbeFailureCode.CANCELLED);
                }
                if (remaining == 0) {
                    return owner ? stop(work, CapabilityProbeFailureCode.TIMEOUT, true)
                            : failure(work.key.profileRevision(), work.key.transportRevision(),
                            CapabilityProbeStatus.TIMEOUT, CapabilityProbeFailureCode.TIMEOUT);
                }
                return work.result.get(Math.min(remaining, WAIT_SLICE_NANOS), TimeUnit.NANOSECONDS);
            } catch (TimeoutException ignored) {
                // Re-check cancellation and the same absolute deadline instead of extending the wait.
            } catch (CancellationException exception) {
                return failure(work.key.profileRevision(), work.key.transportRevision(),
                        CapabilityProbeStatus.CANCELLED, CapabilityProbeFailureCode.CANCELLED);
            } catch (InterruptedException exception) {
                Thread.currentThread().interrupt();
                return owner ? stop(work, CapabilityProbeFailureCode.CANCELLED, true)
                        : failure(work.key.profileRevision(), work.key.transportRevision(),
                        CapabilityProbeStatus.CANCELLED, CapabilityProbeFailureCode.CANCELLED);
            } catch (ExecutionException exception) {
                return failure(work.key.profileRevision(), work.key.transportRevision(),
                        CapabilityProbeStatus.FAILED, CapabilityProbeFailureCode.FAILED);
            }
        }
    }

    /** Interrupts a cooperative task and escalates to a permanent fault only after bounded grace. */
    private CapabilityProbeResult stop(ProbeWork work, CapabilityProbeFailureCode code, boolean escalate) {
        CapabilityProbeResult requested = failure(work.key.profileRevision(), work.key.transportRevision(),
                code == CapabilityProbeFailureCode.TIMEOUT ? CapabilityProbeStatus.TIMEOUT
                        : CapabilityProbeStatus.CANCELLED, code);
        work.forced = requested;
        work.context.cancellation().cancel();
        Future<?> task = work.task;
        if (task != null && task.cancel(true) && !work.started.get()) {
            work.terminated.countDown();
        }
        if (!awaitTermination(work)) {
            if (escalate) {
                return markPermanentFault(work);
            }
            finish(work, requested);
            return requested;
        }
        finish(work, requested);
        return work.result.join();
    }

    /** Stops a set outside the lifecycle lock so a cooperative task can finish and clean its map entry. */
    private void stopAll(List<ProbeWork> works, CapabilityProbeFailureCode code, boolean escalate) {
        for (ProbeWork work : works) {
            stop(work, code, escalate);
        }
    }

    /** Waits on actual runnable termination; Future cancellation alone cannot prove the provider stopped. */
    private boolean awaitTermination(ProbeWork work) {
        long deadline = deadline(STOP_GRACE.toNanos());
        try {
            while (!work.terminated.await(Math.min(remaining(deadline), WAIT_SLICE_NANOS),
                    TimeUnit.NANOSECONDS)) {
                if (remaining(deadline) == 0) {
                    return false;
                }
            }
            return true;
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            return false;
        }
    }

    /** Permanently fails closed because Java has no safe way to kill a non-cooperative provider thread. */
    private CapabilityProbeResult markPermanentFault(ProbeWork work) {
        CapabilityProbeResult permanent = failure(work.key.profileRevision(), work.key.transportRevision(),
                CapabilityProbeStatus.FAILED, CapabilityProbeFailureCode.PERMANENT_FAULT);
        synchronized (lifecycleLock) {
            if (permanentlyFaulted) {
                if (work.finished.compareAndSet(false, true)) {
                    work.forced = permanent;
                    work.context.cancellation().cancel();
                    Future<?> task = work.task;
                    if (task != null && task.cancel(true) && !work.started.get()) {
                        work.terminated.countDown();
                    }
                    work.result.complete(permanent);
                    return permanent;
                }
                return work.result.join();
            }
            permanentlyFaulted = true;
            generation++;
            profileEpochs.clear();
            entries.clear();
            List<ProbeWork> isolated = new ArrayList<>(inFlight.values());
            if (!isolated.contains(work)) {
                // Invalidation may already have removed this work from the map; its waiter still
                // needs a terminal result before the permanent-fault transition can return.
                isolated.add(work);
            }
            inFlight.clear();
            executor.shutdownNow();
            for (ProbeWork active : isolated) {
                if (active.finished.compareAndSet(false, true)) {
                    CapabilityProbeResult activeFailure = failure(active.key.profileRevision(),
                            active.key.transportRevision(), CapabilityProbeStatus.FAILED,
                            CapabilityProbeFailureCode.PERMANENT_FAULT);
                    active.forced = activeFailure;
                    active.context.cancellation().cancel();
                    Future<?> task = active.task;
                    if (task != null && task.cancel(true) && !active.started.get()) {
                        active.terminated.countDown();
                    }
                    active.result.complete(activeFailure);
                }
            }
            return work.result.join();
        }
    }

    /** Keeps only successful or deterministic provider failures in the bounded result cache. */
    private static boolean cacheable(CapabilityProbeResult result) {
        return result.status() == CapabilityProbeStatus.SUCCESS
                || (result.status() == CapabilityProbeStatus.FAILED
                && result.failureCode() != CapabilityProbeFailureCode.OVERLOADED
                && result.failureCode() != CapabilityProbeFailureCode.CACHE_FULL
                && result.failureCode() != CapabilityProbeFailureCode.PERMANENT_FAULT);
    }

    /** Reads only a bounded, pure-memory revision supplied by the provider registration boundary. */
    private static String boundedRevision(String revision) {
        if (revision == null || revision.length() == 0 || revision.length() > 64) {
            return "invalid-revision";
        }
        for (int index = 0; index < revision.length(); index++) {
            char value = revision.charAt(index);
            boolean asciiLetter = (value >= 'A' && value <= 'Z') || (value >= 'a' && value <= 'z');
            boolean asciiDigit = value >= '0' && value <= '9';
            if (!(value == '-' || value == '_' || value == '.' || value == ':'
                    || value == '~' || asciiLetter || asciiDigit)) {
                return "invalid-revision";
            }
        }
        return revision;
    }

    /** Adds a positive duration to monotonic time without allowing arithmetic overflow to disable limits. */
    private static long deadline(long durationNanos) {
        try {
            return Math.addExact(System.nanoTime(), durationNanos);
        } catch (ArithmeticException exception) {
            return Long.MAX_VALUE;
        }
    }

    /** Consumes a monotonic absolute deadline without wall-clock extension. */
    private static long remaining(long deadlineNanos) {
        if (deadlineNanos == Long.MAX_VALUE) {
            return Long.MAX_VALUE;
        }
        long remaining = deadlineNanos - System.nanoTime();
        return remaining > 0 ? remaining : 0;
    }

    /** Builds a fixed diagnostic with no provider exception, URL, header, body, or credential text. */
    private static CapabilityProbeResult failure(String profileRevision, String transportRevision,
                                                 CapabilityProbeStatus status,
                                                 CapabilityProbeFailureCode code) {
        return CapabilityProbeResult.failure(profileRevision, transportRevision, status, code);
    }

    private record CacheKey(String profileId, String profileRevision, String transportRevision) {}

    /** Tracks one shared task so cleanup and stale-publication checks are identity-based. */
    private static final class ProbeWork {
        private final CacheKey key;
        private final ModelProfile profile;
        private final CapabilityProbeTransport transport;
        private final CapabilityProbeContext context;
        private final long generation;
        private final long profileEpoch;
        private final CompletableFuture<CapabilityProbeResult> result = new CompletableFuture<>();
        private final AtomicBoolean started = new AtomicBoolean();
        private final AtomicBoolean finished = new AtomicBoolean();
        private final CountDownLatch terminated = new CountDownLatch(1);
        private volatile Future<?> task;
        private volatile CapabilityProbeResult forced;

        /** Captures immutable work identity so an old task cannot publish into a newer generation. */
        private ProbeWork(CacheKey key, ModelProfile profile, CapabilityProbeTransport transport,
                          CapabilityProbeContext context, long generation, long profileEpoch) {
            this.key = key;
            this.profile = profile;
            this.transport = transport;
            this.context = context;
            this.generation = generation;
            this.profileEpoch = profileEpoch;
        }
    }

    /** Uses non-daemon platform workers; non-cooperative transports are a permanent fault, not a daemon leak. */
    private static final class ProbeThreadFactory implements ThreadFactory {
        private final AtomicInteger sequence = new AtomicInteger();

        /** Creates a named non-daemon worker so lifecycle violations remain visible in tests and shutdown. */
        @Override
        public Thread newThread(Runnable task) {
            Thread thread = new Thread(task, "ja-model-probe-" + sequence.incrementAndGet());
            thread.setDaemon(false);
            return thread;
        }
    }
}
