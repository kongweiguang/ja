// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.time.Instant;
import java.util.EnumSet;
import java.util.Map;
import java.util.Objects;

/** Immutable turn state with an explicit, contract-checked finite-state machine. */
public record TurnState(TurnId turnId, ThreadId threadId, TurnStatus status, TurnMode mode,
                        PermissionMode permissionMode, Instant startedAt, Instant completedAt) {
    private static final Map<TurnStatus, EnumSet<TurnStatus>> TRANSITIONS = Map.of(
            TurnStatus.QUEUED, EnumSet.of(TurnStatus.WAITING_WORKSPACE, TurnStatus.RUNNING,
                    TurnStatus.INTERRUPTING, TurnStatus.INTERRUPTED, TurnStatus.FAILED,
                    TurnStatus.RECOVERY_REQUIRED, TurnStatus.ABORTED_BY_RUNTIME),
            TurnStatus.WAITING_WORKSPACE, EnumSet.of(TurnStatus.RUNNING, TurnStatus.INTERRUPTING,
                    TurnStatus.INTERRUPTED, TurnStatus.FAILED, TurnStatus.RECOVERY_REQUIRED,
                    TurnStatus.ABORTED_BY_RUNTIME),
            TurnStatus.RUNNING, EnumSet.of(TurnStatus.WAITING_APPROVAL, TurnStatus.COMPLETED,
                    TurnStatus.FAILED, TurnStatus.INTERRUPTING, TurnStatus.RECOVERY_REQUIRED,
                    TurnStatus.ABORTED_BY_RUNTIME),
            TurnStatus.WAITING_APPROVAL, EnumSet.of(TurnStatus.RUNNING, TurnStatus.INTERRUPTING,
                    TurnStatus.FAILED, TurnStatus.RECOVERY_REQUIRED, TurnStatus.ABORTED_BY_RUNTIME),
            TurnStatus.INTERRUPTING, EnumSet.of(TurnStatus.INTERRUPTED, TurnStatus.RECOVERY_REQUIRED,
                    TurnStatus.ABORTED_BY_RUNTIME),
            TurnStatus.COMPLETED, EnumSet.noneOf(TurnStatus.class),
            TurnStatus.INTERRUPTED, EnumSet.noneOf(TurnStatus.class),
            TurnStatus.FAILED, EnumSet.noneOf(TurnStatus.class),
            TurnStatus.ABORTED_BY_RUNTIME, EnumSet.noneOf(TurnStatus.class),
            TurnStatus.RECOVERY_REQUIRED, EnumSet.noneOf(TurnStatus.class));

    public TurnState {
        Objects.requireNonNull(turnId, "turnId");
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(status, "status");
        Objects.requireNonNull(mode, "mode");
        Objects.requireNonNull(permissionMode, "permissionMode");
        Objects.requireNonNull(startedAt, "startedAt");
        if (completedAt != null && completedAt.isBefore(startedAt)) {
            throw new IllegalArgumentException("completedAt precedes startedAt");
        }
        if (!status.terminal() && completedAt != null) {
            throw new IllegalArgumentException("non-terminal turn cannot have completedAt");
        }
        if (status.terminal() && completedAt == null) {
            throw new IllegalArgumentException("terminal turn requires completedAt");
        }
    }

    /** Creates the only valid initial turn state. */
    public static TurnState queued(TurnId turnId, ThreadId threadId, TurnMode mode,
                                   PermissionMode permissionMode, Instant startedAt) {
        return new TurnState(turnId, threadId, TurnStatus.QUEUED, mode, permissionMode, startedAt, null);
    }

    /** Applies a lifecycle transition and timestamps terminal states once. */
    public TurnState transition(TurnStatus next, Instant at) {
        Objects.requireNonNull(next, "next");
        Objects.requireNonNull(at, "at");
        EnumSet<TurnStatus> allowed = TRANSITIONS.get(status);
        if (allowed == null || !allowed.contains(next)) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        if (at.isBefore(startedAt)) {
            throw new IllegalArgumentException("transition time precedes start");
        }
        return new TurnState(turnId, threadId, next, mode, permissionMode, startedAt,
                next.terminal() ? at : null);
    }
}
