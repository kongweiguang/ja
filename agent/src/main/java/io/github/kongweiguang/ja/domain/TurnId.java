// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Stable identity for one admitted agent turn. */
public record TurnId(String value) {
    /** Validates the turn prefix so cancellation and admission keys remain bounded and unambiguous. */
    public TurnId {
        IdChecks.require(value, "turn_");
    }

    @Override public String toString() { return value; }
}
