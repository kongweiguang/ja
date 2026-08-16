// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Objects;
import java.util.Set;

/** Test-only ledger proving the production sequencer requires an injected durable boundary. */
public final class InMemoryEventSequenceLedger implements EventSequenceLedger {
    private static final int MAX_THREADS = 4_096;
    private static final int MAX_EVENTS = 1_048_576;

    private final int maxThreads;
    private final int maxEvents;
    private final Map<ThreadId, Long> lastByThread = new HashMap<>();
    private final Map<EventId, Entry> events = new HashMap<>();
    private final Set<EventId> retired = new HashSet<>();
    private ServerInstanceId instance;
    private boolean rotationRequired;

    /** Creates a bounded fake whose cap exposes the same rotation contract as a durable ledger. */
    public InMemoryEventSequenceLedger(int maxThreads, int maxEvents) {
        if (maxThreads < 1 || maxThreads > MAX_THREADS || maxEvents < 1 || maxEvents > MAX_EVENTS) {
            throw new IllegalArgumentException("test ledger caps are outside absolute bounds");
        }
        this.maxThreads = maxThreads;
        this.maxEvents = maxEvents;
    }

    /** Allocates atomically under the test ledger monitor and never evicts old ids. */
    @Override
    public synchronized EventSequenceAllocation allocate(ServerInstanceId serverInstanceId,
                                                          ThreadId threadId, EventId eventId) {
        return allocate(new SequenceTransaction(serverInstanceId, threadId, eventId,
                SequenceEventKind.ORDINARY));
    }

    /** Shares the same per-thread cursor and global event-id tombstone across event families. */
    @Override
    public synchronized EventSequenceAllocation allocate(SequenceTransaction transaction) {
        bind(transaction.serverInstanceId());
        ThreadId threadId = transaction.threadId();
        EventId eventId = transaction.eventId();
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(eventId, "eventId");
        Entry previous = events.get(eventId);
        if (previous != null) {
            if (!previous.transaction.equals(transaction)) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
            return new EventSequenceAllocation(previous.seq, true);
        }
        if (retired.contains(eventId)) {
            throw new ProtocolException(JaErrorCode.DUPLICATE_REQUEST);
        }
        if (!lastByThread.containsKey(threadId) && lastByThread.size() >= maxThreads) {
            rotationRequired = true;
            throw new ProtocolException(JaErrorCode.RESYNC_REQUIRED);
        }
        if (events.size() + retired.size() >= maxEvents) {
            rotationRequired = true;
            throw new ProtocolException(JaErrorCode.RESYNC_REQUIRED);
        }
        long seq = lastByThread.getOrDefault(threadId, 0L) + 1;
        EventSequenceAllocation allocation = new EventSequenceAllocation(seq, false);
        Entry entry = new Entry(transaction, allocation.seq());
        // Construct every validated value before changing either map so a
        // failed value check cannot leave a cursor ahead of its event row.
        lastByThread.put(threadId, seq);
        events.put(eventId, entry);
        if (events.size() + retired.size() >= maxEvents) {
            rotationRequired = true;
        }
        return allocation;
    }

    /** Retires only active payload state while preserving a permanent id tombstone. */
    @Override
    public synchronized boolean retire(ServerInstanceId serverInstanceId, EventId eventId) {
        bind(serverInstanceId);
        EventId id = Objects.requireNonNull(eventId, "eventId");
        if (events.remove(id) == null) {
            return false;
        }
        retired.add(id);
        return true;
    }

    /** Returns the monotonic sequence cursor for the bound test instance. */
    @Override
    public synchronized long lastSeq(ServerInstanceId serverInstanceId, ThreadId threadId) {
        bind(serverInstanceId);
        return lastByThread.getOrDefault(Objects.requireNonNull(threadId, "threadId"), 0L);
    }

    /** Returns active plus retired ids to make lifetime exhaustion observable. */
    @Override
    public synchronized int trackedEventCount() {
        return events.size() + retired.size();
    }

    /** Returns retained thread cursors. */
    @Override
    public synchronized int trackedThreadCount() {
        return lastByThread.size();
    }

    /**
     * Reports the bounded ledger state for one thread while validating the instance namespace,
     * because callers must not make a rotation decision for a different server identity.
     */
    @Override
    public synchronized boolean rotationRequired(ServerInstanceId serverInstanceId, ThreadId threadId) {
        bind(serverInstanceId);
        Objects.requireNonNull(threadId, "threadId");
        return rotationRequired;
    }

    /**
     * Reports the shared instance-wide lifetime boundary so every event family sees exhaustion
     * before attempting to allocate a new event id.
     */
    @Override
    public synchronized boolean rotationRequired(ServerInstanceId serverInstanceId) {
        bind(serverInstanceId);
        return rotationRequired;
    }

    /** Reports that the host must start a new instance before allocating new ids. */
    @Override
    public synchronized boolean rotationRequired() {
        return rotationRequired;
    }

    /** Binds one fake ledger to one server namespace so tests cannot hide cross-instance reuse. */
    private void bind(ServerInstanceId serverInstanceId) {
        if (instance == null) {
            instance = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        } else if (!instance.equals(serverInstanceId)) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
    }

    /** Stores the full transaction identity so cross-kind reuse cannot look idempotent. */
    private record Entry(SequenceTransaction transaction, long seq) {
    }
}
