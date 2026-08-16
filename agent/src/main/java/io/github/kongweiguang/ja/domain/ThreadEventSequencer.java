// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.util.Objects;

/** Adapts the durable event ledger to the immutable event value exposed to callers. */
public final class ThreadEventSequencer {
    private final ServerInstanceId serverInstanceId;
    private final EventSequenceLedger ledger;

    /** Requires an explicit durable ledger so production cannot silently use process memory. */
    public ThreadEventSequencer(ServerInstanceId serverInstanceId, EventSequenceLedger ledger) {
        this.serverInstanceId = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        this.ledger = Objects.requireNonNull(ledger, "ledger");
    }

    /** Allocates the next sequence or returns the ledger's idempotent replay. */
    public SequencedEvent next(ThreadId threadId, EventId eventId) {
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(eventId, "eventId");
        EventSequenceAllocation allocation = ledger.allocate(new SequenceTransaction(serverInstanceId, threadId,
                eventId, SequenceEventKind.ORDINARY));
        return new SequencedEvent(serverInstanceId, threadId, eventId,
                allocation.seq(), allocation.duplicate());
    }

    /** Returns the durable last sequence, or zero for a new thread. */
    public long lastSeq(ThreadId threadId) {
        return ledger.lastSeq(serverInstanceId, Objects.requireNonNull(threadId, "threadId"));
    }

    /** Retires payload retention without permitting the event id to be allocated again. */
    public boolean release(EventId eventId) {
        return ledger.retire(serverInstanceId, Objects.requireNonNull(eventId, "eventId"));
    }

    /** Exposes the ledger's finite-lifetime rotation signal to the host lifecycle. */
    public boolean rotationRequired() {
        return ledger.rotationRequired();
    }

    /** Returns the number of event ids retained by the durable ledger. */
    public int trackedEventCount() {
        return ledger.trackedEventCount();
    }

    /** Returns the number of thread cursors retained by the durable ledger. */
    public int trackedThreadCount() {
        return ledger.trackedThreadCount();
    }

    /** Returns the instance namespace whose ids must never be reused after rotation. */
    public ServerInstanceId serverInstanceId() {
        return serverInstanceId;
    }
}
