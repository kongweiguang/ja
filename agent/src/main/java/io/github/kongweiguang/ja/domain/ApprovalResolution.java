// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.util.Objects;

/** Atomic result returned after approval state and its resolved event are durable. */
public record ApprovalResolution(ApprovalState state, ApprovalResolvedEvent event) {
    /** Keeps the returned state and outbox identity inseparable for retry callers. */
    public ApprovalResolution {
        Objects.requireNonNull(state, "state");
        Objects.requireNonNull(event, "event");
        if (!state.approvalId().equals(event.approvalId())
                || !state.threadId().equals(event.threadId())
                || !state.turnId().equals(event.turnId())) {
            throw new IllegalArgumentException("approval resolution identity mismatch");
        }
    }
}
