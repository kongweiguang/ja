// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.util.Objects;

/**
 * Immutable input to the shared sequence transaction. Event identity is global
 * within the server instance, while the sequence cursor is scoped by thread.
 */
public record SequenceTransaction(ServerInstanceId serverInstanceId, ThreadId threadId,
                                   EventId eventId, SequenceEventKind eventKind) {
    /** Validates the transaction key before a durable sequence cursor can move. */
    public SequenceTransaction {
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(eventId, "eventId");
        Objects.requireNonNull(eventKind, "eventKind");
    }
}
