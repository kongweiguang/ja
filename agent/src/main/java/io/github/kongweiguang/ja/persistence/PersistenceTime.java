// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.time.Instant;
import java.util.Objects;

/**
 * Canonical durable time policy. SQLite stores INTEGER epoch milliseconds, so
 * normalizing at the value boundary prevents a nanosecond-bearing caller
 * snapshot from failing a later full-record CAS after reopen.
 */
final class PersistenceTime {
    private PersistenceTime() {
    }

    /** Rejects pre-epoch/overflow values and returns the exact millisecond value SQLite can restore. */
    static Instant canonical(Instant value, String name) {
        Objects.requireNonNull(value, name);
        final long epochMillis;
        try {
            epochMillis = value.toEpochMilli();
        } catch (ArithmeticException exception) {
            throw new IllegalArgumentException(name + " is outside epoch-millisecond range", exception);
        }
        if (epochMillis < 0) {
            throw new IllegalArgumentException(name + " must not precede the Unix epoch");
        }
        return Instant.ofEpochMilli(epochMillis);
    }

    /** Converts an accepted instant to the integer persisted by SQLite. */
    static long millis(Instant value, String name) {
        return canonical(value, name).toEpochMilli();
    }
}
