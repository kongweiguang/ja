// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.time.Instant;
import java.util.Objects;

/** Minimal durable turn snapshot; prompt, source, and secret material stay outside this row. */
public record TurnSnapshot(String turnId, String threadId, TurnPhase phase,
                           Instant startedAt, Instant completedAt, long revision) {
    /** Validates lifecycle timestamps so a crash cannot be represented as a false completion. */
    public TurnSnapshot {
        requireId(turnId, "turnId");
        requireId(threadId, "threadId");
        Objects.requireNonNull(phase, "phase");
        startedAt = PersistenceTime.canonical(startedAt, "startedAt");
        completedAt = completedAt == null ? null
                : PersistenceTime.canonical(completedAt, "completedAt");
        if (revision < 1) {
            throw new IllegalArgumentException("revision must be positive");
        }
        if (phase.terminal() != (completedAt != null)) {
            throw new IllegalArgumentException("terminal phase/completion timestamp mismatch");
        }
        if (completedAt != null && completedAt.isBefore(startedAt)) {
            throw new IllegalArgumentException("completedAt precedes startedAt");
        }
    }

    /** Creates a revision-one queued snapshot for a new turn. */
    public static TurnSnapshot queued(String turnId, String threadId, Instant startedAt) {
        return new TurnSnapshot(turnId, threadId, TurnPhase.QUEUED, startedAt, null, 1);
    }

    /** Returns the next durable revision while retaining the caller's chosen lifecycle fields. */
    public TurnSnapshot nextRevision() {
        if (revision == Long.MAX_VALUE) {
            throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                    "turn revision exhausted");
        }
        return new TurnSnapshot(turnId, threadId, phase, startedAt, completedAt, revision + 1);
    }

    private static void requireId(String value, String name) {
        if (value == null || value.isBlank() || value.length() > 256
                || value.indexOf('\0') >= 0 || value.indexOf('\n') >= 0) {
            throw new IllegalArgumentException("invalid " + name);
        }
    }
}
