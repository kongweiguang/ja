// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/**
 * Durable compatibility boundary for event idempotency and per-thread sequence
 * allocation. Implementations must preserve {@link SequenceEventKind} so
 * ordinary and approval events share one cursor and one event-id tombstone.
 * They must atomically persist a new allocation before its event is emitted; a
 * runtime must rotate to a new server instance when this ledger reports
 * exhaustion instead of silently reusing an id.
 */
public interface EventSequenceLedger extends SequencePort {
    /** Requires every adapter to preserve event-family identity in the shared transaction. */
    @Override
    EventSequenceAllocation allocate(SequenceTransaction transaction);

    /** Adapts the legacy ordinary-event call without weakening the shared transaction contract. */
    default EventSequenceAllocation allocate(ServerInstanceId serverInstanceId, ThreadId threadId,
                                             EventId eventId) {
        return allocate(new SequenceTransaction(serverInstanceId, threadId, eventId,
                SequenceEventKind.ORDINARY));
    }

    /** Retires payload retention while permanently reserving the event id. */
    boolean retire(ServerInstanceId serverInstanceId, EventId eventId);

    /** Returns the durable last sequence for one server instance and thread. */
    long lastSeq(ServerInstanceId serverInstanceId, ThreadId threadId);

    /** Returns the number of active plus retired event ids retained by the ledger. */
    int trackedEventCount();

    /** Returns the number of thread sequence cursors retained by the ledger. */
    int trackedThreadCount();

    /** Reports per-thread exhaustion for the named durable server and cursor. */
    @Override
    boolean rotationRequired(ServerInstanceId serverInstanceId, ThreadId threadId);

    /** Reports instance-wide exhaustion for the named durable server before new ids are admitted. */
    @Override
    boolean rotationRequired(ServerInstanceId serverInstanceId);

    /** Signals that the caller must rotate the server instance before new ids can be allocated. */
    boolean rotationRequired();
}
