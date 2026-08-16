// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.TurnState;
import io.github.kongweiguang.ja.domain.TurnStatus;

import java.time.Instant;

/**
 * Authoritative turn-state boundary. Implementations must resolve reservation,
 * publish acknowledgement, and terminal completion under one durable barrier.
 */
public interface TurnStatePort {
    /** Returns the namespace that scopes stable reservation ids and tombstones. */
    ServerInstanceId serverInstanceId();

    /**
     * Atomically compares the full snapshot and records a reservation while
     * transitioning to INTERRUPTING. A retry with the same stable id returns
     * ALREADY_RESERVED; terminal or stale business outcomes are typed values.
     * An IO/commit exception is ambiguous and must not be treated as rejection.
     */
    TurnCancellationReservation reserveCancellation(TurnState expected,
                                                     String reservationId, Instant at);

    /**
     * Records that the cancellation was published. The reservation remains a
     * durable history/tombstone; TERMINAL means completion won the race and
     * permits the caller to discard its local token.
     */
    CancellationPublishAcknowledgement acknowledgeCancellationPublished(String reservationId);

    /**
     * Completes a turn and resolves its reservation atomically. This is the only
     * terminal-completion path; the expected snapshot identifies the execution
     * revision and the caller supplies the real terminal time. An INTERRUPTING
     * state is a valid post-reservation revision of that same snapshot.
     */
    TurnState completeAndResolveCancellation(TurnState expected, String reservationId,
                                             TurnStatus terminal, Instant at);
}
