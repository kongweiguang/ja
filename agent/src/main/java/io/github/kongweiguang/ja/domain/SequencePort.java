// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/**
 * Durable shared sequence boundary for ordinary and approval events.
 * Implementations must atomically reserve the event id and increment the
 * `(serverInstanceId, threadId)` cursor; a duplicate replays the exact same
 * transaction, while a reused id with any different identity is a conflict.
 */
public interface SequencePort {
    /** Allocates or idempotently replays one shared sequence transaction. */
    EventSequenceAllocation allocate(SequenceTransaction transaction);

    /** Returns a durable cursor without advancing it, scoped to one server and thread. */
    long lastSeq(ServerInstanceId serverInstanceId, ThreadId threadId);

    /**
     * Reports per-thread exhaustion before a caller attempts another allocation, because
     * silently reusing a bounded event namespace would make late events ambiguous.
     */
    boolean rotationRequired(ServerInstanceId serverInstanceId, ThreadId threadId);

    /**
     * Reports instance-wide exhaustion so approval and ordinary-event admission observe the
     * same rotation boundary instead of maintaining independent capacity decisions.
     */
    boolean rotationRequired(ServerInstanceId serverInstanceId);
}
