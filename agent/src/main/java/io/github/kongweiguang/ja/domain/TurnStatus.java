// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Frozen turn lifecycle values from ja-rpc/v1. */
public enum TurnStatus {
    QUEUED,
    RUNNING,
    WAITING_APPROVAL,
    INTERRUPTING,
    COMPLETED,
    INTERRUPTED,
    FAILED,
    ABORTED_BY_RUNTIME;

    /** Returns whether this status is terminal and cannot transition again. */
    public boolean terminal() {
        return switch (this) {
            case COMPLETED, INTERRUPTED, FAILED, ABORTED_BY_RUNTIME -> true;
            default -> false;
        };
    }
}
