// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Immutable profile revision selected when a turn is created. */
public record ProfileRevision(String value) {
    /** Validates the revision prefix because profile compare-and-swap keys are wire-visible. */
    public ProfileRevision {
        IdChecks.require(value, "profile_");
    }

    @Override public String toString() { return value; }
}
