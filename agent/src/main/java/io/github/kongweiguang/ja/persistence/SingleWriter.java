// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.time.Duration;
import java.util.List;
import java.util.Objects;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.Callable;
import java.util.concurrent.CancellationException;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.FutureTask;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

/** Serializes JDBC operations behind bounded count/byte admission and one writer thread. */
final class SingleWriter implements AutoCloseable {
    private static final long DEFAULT_OPERATION_TIMEOUT_NANOS = Duration.ofSeconds(30).toNanos();
    private final int maxPendingCount;
    private final long maxPendingBytes;
    private final long operationTimeoutNanos;
    private final AtomicBoolean closed = new AtomicBoolean();
    private final AtomicBoolean closeConfirmed = new AtomicBoolean();
    private final AtomicBoolean closeInProgress = new AtomicBoolean();
    private final AtomicReference<Thread> writerThread = new AtomicReference<>();
    private final AtomicInteger pendingCount = new AtomicInteger();
    private final AtomicLong pendingBytes = new AtomicLong();
    private final AtomicReference<TrackedTask<?>> activeTask = new AtomicReference<>();
    private final Object admissionMonitor = new Object();
    private final ThreadPoolExecutor executor;

    /** Keeps the original constructor useful for package-level tests while applying safe byte/deadline bounds. */
    SingleWriter(int queueCapacity) {
        this(queueCapacity, Math.max(1L, queueCapacity) * 64L * 1024,
                Duration.ofNanos(DEFAULT_OPERATION_TIMEOUT_NANOS));
    }

    /** Creates a single non-daemon writer with explicit count, byte, and wait budgets. */
    SingleWriter(int queueCapacity, long queueBytes, Duration operationTimeout) {
        if (queueCapacity < 1 || queueBytes < 1 || operationTimeout == null
                || operationTimeout.isNegative() || operationTimeout.isZero()) {
            throw new IllegalArgumentException("invalid writer admission budget");
        }
        maxPendingCount = Math.addExact(queueCapacity, 1);
        maxPendingBytes = queueBytes;
        operationTimeoutNanos = operationTimeout.toNanos();
        ThreadFactory factory = task -> {
            Thread thread = new Thread(task, "ja-sqlite-writer");
            thread.setDaemon(false);
            writerThread.set(thread);
            return thread;
        };
        executor = new ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS,
                new ArrayBlockingQueue<>(queueCapacity), factory,
                new ThreadPoolExecutor.AbortPolicy());
    }

    /** Runs inline for nested work, otherwise waits only until one absolute admission deadline. */
    <T> T execute(Callable<T> work) {
        return execute(1, work);
    }

    /** Accounts one bounded work item so queued payload pressure cannot grow without limit. */
    <T> T execute(long estimatedBytes, Callable<T> work) {
        Objects.requireNonNull(work, "work");
        if (closed.get()) {
            throw new PersistenceException(PersistenceException.Code.CLOSED,
                    "persistence is closed");
        }
        if (Thread.currentThread() == writerThread.get()) {
            return call(work);
        }
        long deadline = deadlineNanos(operationTimeoutNanos);
        Admission admission = reserve(estimatedBytes, deadline);
        FutureTask<T> future = new FutureTask<>(work);
        TrackedTask<T> task = new TrackedTask<>(future, admission);
        try {
            executor.execute(task);
        } catch (RejectedExecutionException exception) {
            admission.release();
            if (closed.get()) {
                throw new PersistenceException(PersistenceException.Code.CLOSED,
                        "persistence is closed");
            }
            throw new PersistenceException(PersistenceException.Code.QUEUE_FULL,
                    "persistence writer queue is full");
        }
        try {
            long remaining = remainingNanos(deadline);
            if (remaining <= 0) {
                future.cancel(true);
                admission.release();
                throw queueTimeout();
            }
            return future.get(remaining, TimeUnit.NANOSECONDS);
        } catch (TimeoutException exception) {
            future.cancel(true);
            admission.release();
            throw queueTimeout();
        } catch (InterruptedException exception) {
            future.cancel(true);
            admission.release();
            Thread.currentThread().interrupt();
            throw new PersistenceException(PersistenceException.Code.TRANSACTION,
                    "persistence operation was interrupted");
        } catch (CancellationException exception) {
            admission.release();
            if (closed.get()) {
                throw new PersistenceException(PersistenceException.Code.CLOSED,
                        "persistence is closed");
            }
            throw new PersistenceException(PersistenceException.Code.QUEUE_TIMEOUT,
                    "persistence operation was cancelled");
        } catch (ExecutionException exception) {
            Throwable cause = exception.getCause();
            if (cause instanceof PersistenceException persistenceException) {
                throw persistenceException;
            }
            if (cause instanceof RuntimeException runtimeException) {
                throw runtimeException;
            }
            throw new PersistenceException(PersistenceException.Code.TRANSACTION,
                    "persistence operation failed");
        }
    }

    /** Reserves both queue dimensions with a finite deadline and no leaked admission on interruption. */
    private Admission reserve(long estimatedBytes, long deadline) {
        if (estimatedBytes < 1 || estimatedBytes > maxPendingBytes) {
            throw new PersistenceException(PersistenceException.Code.QUEUE_FULL,
                    "persistence operation exceeds writer byte budget");
        }
        synchronized (admissionMonitor) {
            while (true) {
                if (closed.get()) {
                    throw new PersistenceException(PersistenceException.Code.CLOSED,
                            "persistence is closed");
                }
                if (pendingCount.get() < maxPendingCount
                        && pendingBytes.get() <= maxPendingBytes - estimatedBytes) {
                    pendingCount.incrementAndGet();
                    pendingBytes.addAndGet(estimatedBytes);
                    return new Admission(estimatedBytes);
                }
                long remaining = remainingNanos(deadline);
                if (remaining <= 0) {
                    throw queueTimeout();
                }
                try {
                    long millis = Math.max(1L, TimeUnit.NANOSECONDS.toMillis(remaining));
                    admissionMonitor.wait(millis);
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                    throw new PersistenceException(PersistenceException.Code.TRANSACTION,
                            "persistence admission was interrupted");
                }
            }
        }
    }

    /** Converts checked task failures to the bounded persistence error surface. */
    private static <T> T call(Callable<T> work) {
        try {
            return work.call();
        } catch (PersistenceException exception) {
            throw exception;
        } catch (Exception exception) {
            throw new PersistenceException(PersistenceException.Code.TRANSACTION,
                    "persistence operation failed");
        }
    }

    /**
     * Stops admission, interrupts the current operation, and waits for the writer
     * to really terminate; returning while a non-daemon JDBC owner is alive would
     * let callers reopen the same database behind an unaccounted transaction.
     */
    @Override
    public void close() {
        if (closeConfirmed.get()) {
            return;
        }
        if (!closeInProgress.compareAndSet(false, true)) {
            throw writerCloseUnconfirmed();
        }
        try {
            closed.set(true);
            synchronized (admissionMonitor) {
                admissionMonitor.notifyAll();
            }
            // shutdownNow both interrupts the active JDBC task and returns every
            // task that never started, so no admission remains hidden in the queue.
            List<Runnable> abandoned = executor.shutdownNow();
            for (Runnable runnable : abandoned) {
                if (runnable instanceof TrackedTask<?> task) {
                    task.cancel(false);
                }
            }
            TrackedTask<?> active = activeTask.get();
            if (active != null) {
                // FutureTask cancellation wakes the caller even when a JDBC/native
                // operation ignores Thread.interrupt; the owner still waits for the
                // actual worker termination below before reporting close success.
                active.cancel(true);
            }
            Thread thread = writerThread.get();
            long deadline = deadlineNanos(operationTimeoutNanos);
            while (!executor.isTerminated() || (thread != null && thread.isAlive())) {
                long remaining = remainingNanos(deadline);
                if (remaining <= 0) {
                    break;
                }
                try {
                    if (thread != null && thread.isAlive()) {
                        long millis = TimeUnit.NANOSECONDS.toMillis(remaining);
                        int nanos = (int) (remaining - TimeUnit.MILLISECONDS.toNanos(millis));
                        thread.join(millis, nanos);
                    } else {
                        executor.awaitTermination(remaining, TimeUnit.NANOSECONDS);
                    }
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                    break;
                }
            }
            if (!executor.isTerminated() || (thread != null && thread.isAlive())) {
                throw writerCloseUnconfirmed();
            }
            closeConfirmed.set(true);
        } finally {
            closeInProgress.set(false);
            synchronized (admissionMonitor) {
                admissionMonitor.notifyAll();
            }
        }
    }

    /** Computes one saturating absolute deadline so admission and Future wait share the same budget. */
    private static long deadlineNanos(long timeoutNanos) {
        long now = System.nanoTime();
        if (timeoutNanos >= Long.MAX_VALUE - now) {
            return Long.MAX_VALUE;
        }
        return now + timeoutNanos;
    }

    /** Returns remaining monotonic time without allowing a negative Future timeout. */
    private static long remainingNanos(long deadline) {
        long remaining = deadline - System.nanoTime();
        return remaining < 0 ? 0 : remaining;
    }

    /** Creates one stable timeout category for admission and execution deadline exhaustion. */
    private static PersistenceException queueTimeout() {
        return new PersistenceException(PersistenceException.Code.QUEUE_TIMEOUT,
                "persistence writer deadline exceeded");
    }

    /** Keeps an unconfirmed owner from being mistaken for a safely closed writer. */
    private static PersistenceException writerCloseUnconfirmed() {
        return new PersistenceException(PersistenceException.Code.WRITER_CLOSE_UNCONFIRMED,
                "persistence writer did not terminate before close deadline");
    }

    /** Tracks a FutureTask separately because ThreadPoolExecutor queue cancellation otherwise leaks admission. */
    private final class TrackedTask<T> implements Runnable {
        private final FutureTask<T> delegate;
        private final Admission admission;

        private TrackedTask(FutureTask<T> delegate, Admission admission) {
            this.delegate = delegate;
            this.admission = admission;
        }

        @Override
        public void run() {
            activeTask.set(this);
            try {
                delegate.run();
            } finally {
                activeTask.compareAndSet(this, null);
                admission.release();
            }
        }

        /** Cancels a task removed by close while making its caller observable instead of hanging forever. */
        private void cancel(boolean mayInterruptIfRunning) {
            delegate.cancel(mayInterruptIfRunning);
            admission.release();
        }
    }

    /** Releases count and bytes exactly once across timeout, cancellation, normal run, and close paths. */
    private final class Admission {
        private final long bytes;
        private final AtomicBoolean released = new AtomicBoolean();

        private Admission(long bytes) {
            this.bytes = bytes;
        }

        private void release() {
            if (!released.compareAndSet(false, true)) {
                return;
            }
            pendingCount.decrementAndGet();
            pendingBytes.addAndGet(-bytes);
            synchronized (admissionMonitor) {
                admissionMonitor.notifyAll();
            }
        }
    }
}
