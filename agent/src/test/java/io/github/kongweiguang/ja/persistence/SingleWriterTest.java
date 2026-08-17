// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import org.junit.jupiter.api.Test;

import java.time.Duration;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Verifies bounded writer admission and honest shutdown without relying on
 * process termination to hide a live non-daemon database owner. */
class SingleWriterTest {
    /** Byte admission and timeout release keep the writer reusable. */
    @Test
    void boundedAdmissionReleasesAfterTimeout() {
        SingleWriter writer = new SingleWriter(1, 4, Duration.ofMillis(100));
        try {
            PersistenceException oversized = assertThrows(PersistenceException.class,
                    () -> writer.execute(5, () -> null));
            assertEquals(PersistenceException.Code.QUEUE_FULL, oversized.code());
            PersistenceException timeout = assertThrows(PersistenceException.class,
                    () -> writer.execute(1, () -> {
                        Thread.sleep(500);
                        return null;
                    }));
            assertEquals(PersistenceException.Code.QUEUE_TIMEOUT, timeout.code());
            assertDoesNotThrow(() -> writer.execute(1, () -> null));
        } finally {
            writer.close();
        }
    }

    /** Concurrent callers must observe one JDBC owner at a time, otherwise a
     * single connection could interleave transactions and corrupt turn order. */
    @Test
    void concurrentCallsAreSerialized() throws Exception {
        SingleWriter writer = new SingleWriter(16, 16, Duration.ofSeconds(2));
        ExecutorService callers = Executors.newFixedThreadPool(4);
        AtomicInteger active = new AtomicInteger();
        AtomicInteger maximumActive = new AtomicInteger();
        try {
            var futures = new java.util.ArrayList<Future<Integer>>();
            for (int index = 0; index < 12; index++) {
                futures.add(callers.submit(() -> writer.execute(1, () -> {
                    int current = active.incrementAndGet();
                    maximumActive.accumulateAndGet(current, Math::max);
                    try {
                        Thread.sleep(5);
                        return current;
                    } finally {
                        active.decrementAndGet();
                    }
                })));
            }
            for (Future<Integer> future : futures) {
                assertEquals(1, future.get(2, TimeUnit.SECONDS));
            }
            assertEquals(1, maximumActive.get());
        } finally {
            callers.shutdownNow();
            writer.close();
        }
    }

    /** An interrupt-ignoring task makes close fail visibly; once released, a
     * second close proves the owner debt is recoverable. */
    @Test
    void blockedTaskCloseFailsThenRetriesWithoutLiveWriter() throws Exception {
        SingleWriter writer = new SingleWriter(1, 4, Duration.ofMillis(80));
        CountDownLatch entered = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        var caller = Executors.newSingleThreadExecutor();
        try {
            CompletableFuture<Void> operation = CompletableFuture.runAsync(() -> {
                assertThrows(PersistenceException.class, () -> writer.execute(1, () -> {
                    entered.countDown();
                    while (release.getCount() != 0) {
                        try {
                            release.await(10, TimeUnit.MILLISECONDS);
                        } catch (InterruptedException ignored) {
                            // Close must not claim success while this task ignores interrupt.
                        }
                    }
                    return null;
                }));
            }, caller);
            assertTrue(entered.await(1, TimeUnit.SECONDS));
            PersistenceException failure = assertThrows(PersistenceException.class, writer::close);
            assertEquals(PersistenceException.Code.WRITER_CLOSE_UNCONFIRMED, failure.code());
            assertTrue(Thread.getAllStackTraces().keySet().stream()
                    .anyMatch(thread -> thread.getName().equals("ja-sqlite-writer")
                            && thread.isAlive() && !thread.isDaemon()));
            release.countDown();
            operation.join();
            assertDoesNotThrow(writer::close);
            assertTrue(Thread.getAllStackTraces().keySet().stream()
                    .noneMatch(thread -> thread.getName().equals("ja-sqlite-writer")
                            && thread.isAlive()));
        } finally {
            release.countDown();
            caller.shutdownNow();
            try {
                writer.close();
            } catch (PersistenceException ignored) {
                // The first close assertion intentionally reports the debt.
            }
        }
    }
}
