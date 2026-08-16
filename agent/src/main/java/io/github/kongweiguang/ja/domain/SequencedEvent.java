// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.util.Objects;

/** Result of allocating a per-instance/per-thread event sequence number. */
public record SequencedEvent(ServerInstanceId serverInstanceId, ThreadId threadId,
                             EventId eventId, long seq, boolean duplicate) {
    /** Validates identity and JSON-safe sequence bounds before event publication. */
    public SequencedEvent {
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(eventId, "eventId");
        if (seq < 1 || seq > 9_007_199_254_740_991L) {
            throw new IllegalArgumentException("seq out of JSON safe integer range");
        }
    }
}
