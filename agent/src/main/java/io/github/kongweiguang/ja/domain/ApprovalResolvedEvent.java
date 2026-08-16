// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.time.Instant;
import java.util.Objects;

/** Persisted resolved event whose sequence and outbox key were allocated in the ledger transaction. */
public record ApprovalResolvedEvent(ServerInstanceId serverInstanceId, ThreadId threadId,
                                    TurnId turnId, ApprovalId approvalId, EventId eventId,
                                    Instant occurredAt, long seq, String outboxKey) {
    /** Validates ledger-owned sequence and bounded internal outbox identity. */
    public ApprovalResolvedEvent {
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(turnId, "turnId");
        Objects.requireNonNull(approvalId, "approvalId");
        Objects.requireNonNull(eventId, "eventId");
        Objects.requireNonNull(occurredAt, "occurredAt");
        if (seq < 1 || seq > 9_007_199_254_740_991L) {
            throw new IllegalArgumentException("invalid resolved event sequence");
        }
        if (outboxKey == null || outboxKey.isBlank() || outboxKey.length() > 256) {
            throw new IllegalArgumentException("invalid resolved event outbox key");
        }
    }

    /** Omits the internal outbox key while retaining stable event identity in logs. */
    @Override
    public String toString() {
        return "ApprovalResolvedEvent[approvalId=" + approvalId + ", eventId=" + eventId
                + ", seq=" + seq + "]";
    }
}
