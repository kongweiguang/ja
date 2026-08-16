// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.time.Instant;
import java.util.Objects;

/** Persisted requested event returned after the ledger allocates sequence and outbox identity. */
public record ApprovalRequestedEventRecord(ServerInstanceId serverInstanceId, ThreadId threadId,
                                           TurnId turnId, ApprovalId approvalId, EventId eventId,
                                           Instant occurredAt, long seq, String outboxKey) {
    /** Validates the ledger-owned monotonic sequence and bounded internal outbox key. */
    public ApprovalRequestedEventRecord {
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(turnId, "turnId");
        Objects.requireNonNull(approvalId, "approvalId");
        Objects.requireNonNull(eventId, "eventId");
        Objects.requireNonNull(occurredAt, "occurredAt");
        if (seq < 1 || seq > 9_007_199_254_740_991L) {
            throw new IllegalArgumentException("invalid requested event sequence");
        }
        if (outboxKey == null || outboxKey.isBlank() || outboxKey.length() > 256) {
            throw new IllegalArgumentException("invalid requested event outbox key");
        }
    }

    /** Omits the internal outbox key while keeping durable identity visible in diagnostics. */
    @Override
    public String toString() {
        return "ApprovalRequestedEventRecord[approvalId=" + approvalId + ", eventId=" + eventId
                + ", seq=" + seq + "]";
    }
}
