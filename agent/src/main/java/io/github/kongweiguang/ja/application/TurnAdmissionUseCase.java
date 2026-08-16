// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.ThreadId;
import io.github.kongweiguang.ja.domain.ThreadTurnCoordinator;
import io.github.kongweiguang.ja.domain.TurnAdmission;
import io.github.kongweiguang.ja.domain.TurnId;

import java.util.Objects;

/** Applies same-thread admission without pretending a queued turn is already running. */
public final class TurnAdmissionUseCase {
    private final ThreadTurnCoordinator coordinator;

    /** Injects the coordinator so all callers share one same-thread admission boundary. */
    public TurnAdmissionUseCase(ThreadTurnCoordinator coordinator) {
        this.coordinator = Objects.requireNonNull(coordinator, "coordinator");
    }

    /** Returns accepted/queued explicitly so callers can emit the matching RPC result. */
    public TurnAdmission execute(ThreadId threadId, TurnId turnId) {
        return coordinator.admit(threadId, turnId);
    }

    /** Releases only a matching active turn after its terminal event is persisted. */
    public boolean release(ThreadId threadId, TurnId turnId) {
        return coordinator.release(threadId, turnId);
    }
}
