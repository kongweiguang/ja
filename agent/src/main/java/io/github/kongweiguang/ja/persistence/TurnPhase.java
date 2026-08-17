// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

/** Persisted turn phases contain only the small restart-visible lifecycle state. */
public enum TurnPhase {
    QUEUED(false),
    RUNNING(false),
    WAITING_APPROVAL(false),
    INTERRUPTING(false),
    COMPLETED(true),
    INTERRUPTED(true),
    FAILED(true),
    ABORTED_BY_RUNTIME(true);

    private final boolean terminal;

    TurnPhase(boolean terminal) {
        this.terminal = terminal;
    }

    /** Identifies whether a completion timestamp is required for this phase. */
    public boolean terminal() {
        return terminal;
    }
}
