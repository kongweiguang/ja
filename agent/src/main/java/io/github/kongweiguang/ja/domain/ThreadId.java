// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Stable thread identity; a prefix prevents cross-entity id confusion. */
public record ThreadId(String value) {
    /** Validates the thread prefix because sequence and approval scope depend on canonical IDs. */
    public ThreadId {
        IdChecks.require(value, "thr_");
    }

    @Override public String toString() { return value; }
}
