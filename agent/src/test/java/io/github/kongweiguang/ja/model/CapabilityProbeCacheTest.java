// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.kongweiguang.ja.profiles.CapabilitySet;
import io.github.kongweiguang.ja.profiles.ModelApi;
import io.github.kongweiguang.ja.profiles.ModelCapability;
import io.github.kongweiguang.ja.profiles.ModelProfile;
import io.github.kongweiguang.ja.profiles.ModelProvider;
import java.time.Duration;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;

/** Verifies bounded probe lifecycle, redacted diagnostics, and fail-closed outputs. */
class CapabilityProbeCacheTest {
    /** Uses a key-free loopback profile so lifecycle tests never access an external provider. */
    private static ModelProfile profile() {
        return profile("probe");
    }

    /** Creates a distinct local profile so zero-queue overload tests do not join the same flight. */
    private static ModelProfile profile(String id) {
        return ModelProfile.builder().id(id).displayName("Probe")
                .provider(ModelProvider.OPENAI_COMPATIBLE).api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("local").baseUrl("http://127.0.0.1:9").build();
    }

    /** Times out a transport that cooperatively checks the absolute deadline and leaves no cache entry. */
    @Test
    void blockingTransportTimesOutAndClosesWorkers() throws Exception {
        CapabilityProbeTransport blocking = new CapabilityProbeTransport() {
            @Override public CapabilitySet probe(ModelProfile ignored, CapabilityProbeContext context) {
                try {
                    while (true) {
                        context.checkActive();
                        Thread.sleep(10);
                    }
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                    throw new CancellationException("cancelled");
                }
            }
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofMillis(80), 1)) {
            CapabilityProbeResult result = cache.probe(profile(), blocking);
            assertEquals(CapabilityProbeStatus.TIMEOUT, result.status());
            assertEquals(CapabilityProbeFailureCode.TIMEOUT, result.failureCode());
            assertTrue(result.capabilities().supported().isEmpty());
            // A Future may be cancelled before its worker reaches Thread.sleep; the contract is
            // bounded cancellation and cleanup, not a promise that the provider observes interrupt.
            assertEquals(0, cache.inFlightSize());
            assertEquals(0, cache.size());
        }
        assertNoProbeThreads();
    }

    /** Cancels an in-flight probe and verifies the future is interrupted rather than retained. */
    @Test
    void cancellationCompletesWithoutUnboundedWait() throws Exception {
        CapabilityProbeTransport blocking = new CapabilityProbeTransport() {
            @Override public CapabilitySet probe(ModelProfile ignored, CapabilityProbeContext context) {
                while (true) {
                    context.checkActive();
                    Thread.onSpinWait();
                }
            }
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(5), 1)) {
            CapabilityProbeCancellation cancellation = new CapabilityProbeCancellation();
            CompletableFuture<CapabilityProbeResult> future = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile(), blocking, cancellation));
            Thread.sleep(50);
            cancellation.cancel();
            CapabilityProbeResult result = future.get(1, TimeUnit.SECONDS);
            assertEquals(CapabilityProbeStatus.CANCELLED, result.status());
            assertEquals(CapabilityProbeFailureCode.CANCELLED, result.failureCode());
            assertTrue(result.capabilities().supported().isEmpty());
        }
        assertNoProbeThreads();
    }

    /** Verifies repeated close and post-close calls are safe and cannot create new executor threads. */
    @Test
    void closeIsIdempotentAndFailClosed() {
        CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(1), 1);
        cache.close();
        cache.close();
        CapabilityProbeResult result = cache.probe(profile(), (ignored, context) -> {
            context.checkActive();
            return CapabilitySet.empty();
        });
        assertEquals(CapabilityProbeFailureCode.CLOSED, result.failureCode());
        assertTrue(result.capabilities().supported().isEmpty());
    }

    /** Replaces provider exception text with a stable summary that cannot contain URL or secret data. */
    @Test
    void exceptionDiagnosticsAreRedactedAndBounded() {
        String secret = "sk-live-should-not-escape";
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(1), 1)) {
            CapabilityProbeResult result = cache.probe(profile(), (ignored, context) -> {
                context.checkActive();
                throw new IllegalStateException("https://provider.test?apiKey=" + secret);
            });
            assertEquals(CapabilityProbeFailureCode.FAILED, result.failureCode());
            assertFalse(result.failureSummary().contains(secret));
            assertFalse(result.failureSummary().contains("provider.test"));
            assertTrue(result.capabilities().supported().isEmpty());
        }
    }

    /** Rejects oversized diagnostic fields even when a caller bypasses the cache result constructors. */
    @Test
    void oversizedDiagnosticsAreRejected() {
        assertThrows(IllegalArgumentException.class, () -> new CapabilityProbeResult(
                profile().fingerprint(), "test", CapabilityProbeStatus.FAILED,
                CapabilitySet.empty(), CapabilityProbeFailureCode.FAILED, "x".repeat(129)));
        assertThrows(IllegalArgumentException.class, () -> new CapabilityProbeResult(
                profile().fingerprint(), "test", CapabilityProbeStatus.FAILED,
                new CapabilitySet(Set.of(ModelCapability.TEXT)), CapabilityProbeFailureCode.FAILED,
                "provider capability probe failed"));
        assertThrows(IllegalArgumentException.class, () -> new CapabilityProbeResult(
                profile().fingerprint(), "test", CapabilityProbeStatus.FAILED,
                CapabilitySet.empty(), CapabilityProbeFailureCode.FAILED,
                "provider failed at https://provider.test?apiKey=secret"));
        assertThrows(IllegalArgumentException.class, () -> CapabilityProbeResult.failure(
                "https://provider.test?apiKey=secret", "test", CapabilityProbeStatus.FAILED,
                CapabilityProbeFailureCode.FAILED));
        assertThrows(IllegalArgumentException.class, () -> CapabilityProbeResult.failure(
                "profile", "é", CapabilityProbeStatus.FAILED, CapabilityProbeFailureCode.FAILED));
    }

    /** Ensures concurrent callers share one transport invocation and one bounded in-flight slot. */
    @Test
    void singleFlightSharesOneTransportCall() throws Exception {
        AtomicInteger calls = new AtomicInteger();
        CountDownLatch entered = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        CapabilityProbeTransport transport = (ignored, context) -> {
            calls.incrementAndGet();
            entered.countDown();
            try {
                while (!release.await(5, TimeUnit.MILLISECONDS)) {
                    context.checkActive();
                }
            } catch (InterruptedException exception) {
                Thread.currentThread().interrupt();
                throw new CancellationException("cancelled");
            }
            context.checkActive();
            return CapabilitySet.empty();
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(2), 1)) {
            CompletableFuture<CapabilityProbeResult> first = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile(), transport));
            assertTrue(entered.await(1, TimeUnit.SECONDS));
            CompletableFuture<CapabilityProbeResult> second = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile(), transport));
            assertEquals(1, cache.inFlightSize());
            release.countDown();
            assertEquals(CapabilityProbeStatus.SUCCESS, first.get(1, TimeUnit.SECONDS).status());
            assertEquals(CapabilityProbeStatus.SUCCESS, second.get(1, TimeUnit.SECONDS).status());
            assertEquals(1, calls.get());
            assertEquals(0, cache.inFlightSize());
        }
    }

    /** Invalidates an active generation and proves its late result cannot repopulate the cache. */
    @Test
    void invalidateCancelsFlightAndPreventsStalePublish() throws Exception {
        AtomicInteger calls = new AtomicInteger();
        CountDownLatch entered = new CountDownLatch(1);
        CapabilityProbeTransport transport = (ignored, context) -> {
            int invocation = calls.incrementAndGet();
            entered.countDown();
            if (invocation == 1) {
                while (!context.cancellation().isCancelled()) {
                    Thread.onSpinWait();
                }
            }
            // Deliberately return output after invalidation; the epoch must suppress publication.
            return CapabilitySet.empty();
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(2), 1)) {
            CompletableFuture<CapabilityProbeResult> first = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile(), transport));
            assertTrue(entered.await(1, TimeUnit.SECONDS));
            cache.invalidate(profile().id());
            assertEquals(0, cache.size());
            assertEquals(CapabilityProbeFailureCode.CANCELLED, first.get(1, TimeUnit.SECONDS).failureCode());
            assertEquals(CapabilityProbeStatus.SUCCESS, cache.probe(profile(), transport).status());
            assertEquals(2, calls.get());
            assertEquals(1, cache.size());
        }
    }

    /** Clears the global generation and prevents an old successful task from reappearing afterward. */
    @Test
    void clearCancelsFlightAndDropsEntries() throws Exception {
        AtomicInteger calls = new AtomicInteger();
        CountDownLatch entered = new CountDownLatch(1);
        CapabilityProbeTransport transport = (ignored, context) -> {
            if (calls.incrementAndGet() == 1) {
                entered.countDown();
                while (!context.cancellation().isCancelled()) {
                    Thread.onSpinWait();
                }
            }
            return CapabilitySet.empty();
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(2), 1)) {
            CompletableFuture<CapabilityProbeResult> first = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile(), transport));
            assertTrue(entered.await(1, TimeUnit.SECONDS));
            cache.clear();
            assertEquals(CapabilityProbeFailureCode.CANCELLED, first.get(1, TimeUnit.SECONDS).failureCode());
            assertEquals(0, cache.size());
            assertEquals(0, cache.inFlightSize());
            assertEquals(CapabilityProbeStatus.SUCCESS, cache.probe(profile(), transport).status());
            assertEquals(2, calls.get());
        }
    }

    /** Makes the explicit zero-capacity queue contract observable for distinct keys. */
    @Test
    void zeroQueueRejectsDistinctKeyWithoutLeakingSlot() throws Exception {
        CountDownLatch entered = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        CapabilityProbeTransport transport = (ignored, context) -> {
            entered.countDown();
            try {
                while (!release.await(5, TimeUnit.MILLISECONDS)) {
                    context.checkActive();
                }
            } catch (InterruptedException exception) {
                Thread.currentThread().interrupt();
                throw new CancellationException("cancelled");
            }
            return CapabilitySet.empty();
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(2), 1)) {
            CompletableFuture<CapabilityProbeResult> first = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile("one"), transport));
            assertTrue(entered.await(1, TimeUnit.SECONDS));
            CapabilityProbeResult overloaded = cache.probe(profile("two"), transport);
            assertEquals(CapabilityProbeFailureCode.OVERLOADED, overloaded.failureCode());
            assertEquals(1, cache.inFlightSize());
            release.countDown();
            assertEquals(CapabilityProbeStatus.SUCCESS, first.get(1, TimeUnit.SECONDS).status());
        }
    }

    /** Converts an interrupt-ignoring transport into a permanent fault and stops accepting new work. */
    @Test
    void nonCooperativeTransportTriggersPermanentFault() throws Exception {
        AtomicInteger calls = new AtomicInteger();
        CountDownLatch entered = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        CapabilityProbeTransport malicious = (ignored, context) -> {
            calls.incrementAndGet();
            entered.countDown();
            while (true) {
                try {
                    if (release.await(5, TimeUnit.MILLISECONDS)) {
                        return CapabilitySet.empty();
                    }
                } catch (InterruptedException ignoredInterrupt) {
                    // Deliberately violate the cooperative port to exercise permanent-fault isolation.
                }
            }
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofMillis(40), 1)) {
            CompletableFuture<CapabilityProbeResult> first = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile(), malicious));
            assertTrue(entered.await(1, TimeUnit.SECONDS));
            CapabilityProbeResult result = first.get(1, TimeUnit.SECONDS);
            assertEquals(CapabilityProbeFailureCode.PERMANENT_FAULT, result.failureCode());
            assertTrue(cache.isPermanentlyFaulted());
            assertEquals(0, cache.inFlightSize());
            assertEquals(CapabilityProbeFailureCode.PERMANENT_FAULT,
                    cache.probe(profile("later"), malicious).failureCode());
            assertEquals(1, calls.get());
            release.countDown();
        }
        assertNoProbeThreads();
    }

    /** Isolates every active flight when one worker violates cancellation so no global slot remains usable. */
    @Test
    void permanentFaultClosesAllActiveFlights() throws Exception {
        CountDownLatch entered = new CountDownLatch(2);
        CountDownLatch release = new CountDownLatch(1);
        CapabilityProbeTransport malicious = (ignored, context) -> {
            entered.countDown();
            while (true) {
                try {
                    if (release.await(5, TimeUnit.MILLISECONDS)) {
                        return CapabilitySet.empty();
                    }
                } catch (InterruptedException ignoredInterrupt) {
                    // Deliberately ignore interruption; the cache must isolate rather than kill this code.
                }
            }
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofMillis(40), 2)) {
            CompletableFuture<CapabilityProbeResult> first = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile("first"), malicious));
            CompletableFuture<CapabilityProbeResult> second = CompletableFuture.supplyAsync(
                    () -> cache.probe(profile("second"), malicious));
            assertTrue(entered.await(1, TimeUnit.SECONDS));
            CapabilityProbeResult firstResult = first.get(1, TimeUnit.SECONDS);
            CapabilityProbeResult secondResult = second.get(1, TimeUnit.SECONDS);
            assertEquals(CapabilityProbeFailureCode.PERMANENT_FAULT, firstResult.failureCode());
            assertEquals(CapabilityProbeFailureCode.PERMANENT_FAULT, secondResult.failureCode());
            assertEquals(0, cache.inFlightSize());
            assertEquals(CapabilityProbeFailureCode.PERMANENT_FAULT,
                    cache.probe(profile("third"), malicious).failureCode());
            release.countDown();
        }
        assertNoProbeThreads();
    }

    /** Keeps successful entries bounded and returns a stable full-cache result for a new key. */
    @Test
    void cacheEntryLimitIsFailClosed() {
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(1), 1, 1)) {
            CapabilityProbeTransport transport = (ignored, context) -> {
                context.checkActive();
                return CapabilitySet.empty();
            };
            assertEquals(CapabilityProbeStatus.SUCCESS, cache.probe(profile("one"), transport).status());
            assertEquals(CapabilityProbeFailureCode.CACHE_FULL,
                    cache.probe(profile("two"), transport).failureCode());
            assertEquals(1, cache.size());
        }
    }

    /** Clears all cached generations when the bounded per-profile epoch table must use a global barrier. */
    @Test
    void epochTableOverflowUsesGlobalGeneration() {
        CapabilityProbeTransport transport = (ignored, context) -> {
            context.checkActive();
            return CapabilitySet.empty();
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(1), 1, 1)) {
            assertEquals(CapabilityProbeStatus.SUCCESS, cache.probe(profile("one"), transport).status());
            cache.invalidate("one");
            assertEquals(0, cache.size());
            assertEquals(CapabilityProbeStatus.SUCCESS, cache.probe(profile("one"), transport).status());
            cache.invalidate("two");
            assertEquals(0, cache.size());
        }
    }

    /** Normalizes oversized and malformed revisions before keying so revision processing stays bounded. */
    @Test
    void revisionInputIsBoundedAndPureMemory() {
        AtomicInteger calls = new AtomicInteger();
        CapabilityProbeTransport transport = (ignored, context) -> {
            calls.incrementAndGet();
            context.checkActive();
            return CapabilitySet.empty();
        };
        String oversized = "x".repeat(65);
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(1), 1)) {
            assertEquals(CapabilityProbeStatus.SUCCESS,
                    cache.probe(profile(), oversized, transport).status());
            assertEquals(CapabilityProbeStatus.SUCCESS,
                    cache.probe(profile(), "https://secret.invalid", transport).status());
            assertEquals(1, calls.get());
        }
    }

    /** Rejects Unicode revision confusables so cache keys remain bounded ASCII registration data. */
    @Test
    void unicodeRevisionIsNormalized() {
        AtomicInteger calls = new AtomicInteger();
        CapabilityProbeTransport transport = (ignored, context) -> {
            calls.incrementAndGet();
            context.checkActive();
            return CapabilitySet.empty();
        };
        try (CapabilityProbeCache cache = new CapabilityProbeCache(Duration.ofSeconds(1), 1)) {
            assertEquals(CapabilityProbeStatus.SUCCESS,
                    cache.probe(profile(), "é", transport).status());
            assertEquals(CapabilityProbeStatus.SUCCESS,
                    cache.probe(profile(), "invalid-revision", transport).status());
            assertEquals(1, calls.get());
        }
    }

    /** Waits briefly for probe workers to disappear, making lifecycle leaks observable without a fixed sleep. */
    private static void assertNoProbeThreads() throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(1);
        while (System.nanoTime() < deadline) {
            long count = Thread.getAllStackTraces().keySet().stream()
                    .filter(thread -> thread.getName().startsWith("ja-model-probe-")).count();
            if (count == 0) return;
            Thread.sleep(10);
        }
        long count = Thread.getAllStackTraces().keySet().stream()
                .filter(thread -> thread.getName().startsWith("ja-model-probe-")).count();
        assertEquals(0, count);
    }
}
