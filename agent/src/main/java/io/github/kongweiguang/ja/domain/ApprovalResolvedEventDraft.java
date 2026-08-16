// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.time.Instant;
import java.util.Objects;

/**
 * Caller-supplied identity for the approval/resolved outbox record.
 * Sequence and outbox key are deliberately absent: only the ledger can allocate
 * them atomically with state, preventing caller-selected sequence collisions.
 */
public record ApprovalResolvedEventDraft(ServerInstanceId serverInstanceId, ThreadId threadId,
                                         TurnId turnId, ApprovalId approvalId, EventId eventId,
                                         Instant occurredAt) {
    /** Enforces one instance-scoped event identity before the ledger allocates its sequence. */
    public ApprovalResolvedEventDraft {
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(turnId, "turnId");
        Objects.requireNonNull(approvalId, "approvalId");
        Objects.requireNonNull(eventId, "eventId");
        Objects.requireNonNull(occurredAt, "occurredAt");
    }

    /** Omits event payload details because the draft is an internal correlation value. */
    @Override
    public String toString() {
        return "ApprovalResolvedEventDraft[approvalId=" + approvalId + ", eventId=" + eventId + "]";
    }
}
