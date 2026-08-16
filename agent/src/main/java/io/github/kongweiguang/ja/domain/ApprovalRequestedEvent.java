// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.time.Instant;
import java.util.Objects;

/** Draft identity for the event emitted with an approval registration. */
public record ApprovalRequestedEvent(ServerInstanceId serverInstanceId, ThreadId threadId,
                                     TurnId turnId, ApprovalId approvalId, EventId eventId,
                                     Instant occurredAt) {
    /** Rejects incomplete event identity before the ledger allocates its sequence. */
    public ApprovalRequestedEvent {
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(turnId, "turnId");
        Objects.requireNonNull(approvalId, "approvalId");
        Objects.requireNonNull(eventId, "eventId");
        Objects.requireNonNull(occurredAt, "occurredAt");
    }
}
