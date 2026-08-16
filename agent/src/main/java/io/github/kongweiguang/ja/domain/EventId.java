// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Idempotency key for one persisted thread event. */
public record EventId(String value) {
    /** Validates the event prefix because idempotency depends on one bounded canonical key. */
    public EventId {
        IdChecks.require(value, "evt_");
    }

    @Override public String toString() { return value; }
}
