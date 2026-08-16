// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.util.Objects;

/** Admission decision that makes same-thread serialization observable to callers. */
public record TurnAdmission(boolean accepted, boolean queued, TurnId turnId, TurnId activeTurnId) {
    /** Ensures every decision is exactly one of accepted or queued with matching context. */
    public TurnAdmission {
        Objects.requireNonNull(turnId, "turnId");
        if (accepted == queued) {
            throw new IllegalArgumentException("admission must be exactly accepted or queued");
        }
        if (accepted && activeTurnId != null) {
            throw new IllegalArgumentException("accepted turn cannot expose an active predecessor");
        }
        if (queued && activeTurnId == null) {
            throw new IllegalArgumentException("queued turn must identify its active predecessor");
        }
    }
}
