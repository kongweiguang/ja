// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.CancellationToken;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.domain.TurnState;
import io.github.kongweiguang.ja.domain.TurnStatus;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.time.Instant;
import java.util.HashMap;
import java.util.Map;
import java.util.Objects;

/** Coordinates cancellation with an authoritative atomic turn-state port. */
public final class TurnCancellationUseCase {
    /** Bounds turn cancellation tokens retained before terminal cleanup. */
    public static final int DEFAULT_MAX_TOKENS = 1_024;
    /** Absolute token cap prevents attacker-controlled turn ids from pinning memory. */
    public static final int MAX_TOKENS = 1_024;

    private final TurnStatePort statePort;
    private final int maxTokens;
    private final Object monitor = new Object();
    private final Map<TurnId, CancellationToken> tokens = new HashMap<>();
    private final Map<TurnId, String> reservations = new HashMap<>();
    private final ServerInstanceId serverInstanceId;

    /** Requires the durable state authority so stale snapshots cannot recreate cancelled turns. */
    public TurnCancellationUseCase(TurnStatePort statePort) {
        this(statePort, DEFAULT_MAX_TOKENS);
    }

    /** Creates a cancellation use case with an explicit bounded token budget. */
    public TurnCancellationUseCase(TurnStatePort statePort, int maxTokens) {
        this.statePort = Objects.requireNonNull(statePort, "statePort");
        this.serverInstanceId = Objects.requireNonNull(statePort.serverInstanceId(), "serverInstanceId");
        if (maxTokens < 1 || maxTokens > MAX_TOKENS) {
            throw new IllegalArgumentException("maxTokens is outside absolute bounds");
        }
        this.maxTokens = maxTokens;
    }

    /**
     * Prepares the token and stable id before asking the port for its reservation.
     * An exception is an unknown commit outcome, so the local handle is retained
     * and the next call retries the same id instead of creating a second lease.
     */
    public TurnCancellationReservation execute(TurnState snapshot, Instant at) {
        Objects.requireNonNull(snapshot, "snapshot");
        Objects.requireNonNull(at, "at");
        TurnId turnId = snapshot.turnId();
        String reservationId = stableReservationId(snapshot);
        CancellationToken token;
        synchronized (monitor) {
            token = tokens.get(turnId);
            if (snapshot.status().terminal() && token == null) {
                return TurnCancellationReservation.rejected(reservationId, JaErrorCode.TURN_NOT_ACTIVE);
            }
            if (token == null) {
                if (tokens.size() >= maxTokens) {
                    return TurnCancellationReservation.rejected(reservationId, JaErrorCode.QUEUE_FULL);
                }
                token = new CancellationToken();
                tokens.put(turnId, token);
            }
            // Keep the id beside the token before any ambiguous durable call begins.
            String existingReservation = reservations.get(turnId);
            if (existingReservation != null && !existingReservation.equals(reservationId)) {
                return TurnCancellationReservation.rejected(reservationId, JaErrorCode.CONFLICT);
            }
            reservations.putIfAbsent(turnId, reservationId);
        }

        // Do not hold the local monitor while the durable port may wait for a terminal race.
        if (snapshot.status().terminal()) {
            token.cancel();
            CancellationPublishAcknowledgement acknowledgement = acknowledge(reservationId);
            return finishTerminalSnapshotAcknowledgement(turnId, reservationId, acknowledgement);
        }

        TurnCancellationReservation reservation;
        try {
            reservation = statePort.reserveCancellation(snapshot, reservationId, at);
        } catch (RuntimeException failure) {
            // The port may have committed before reporting an IO failure; retain both.
            throw failure;
        }
        if (reservation == null || !reservationId.equals(reservation.reservationId())
                || (reservation.state() != null && !turnId.equals(reservation.state().turnId()))) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        if (reservation.status() == TurnCancellationReservation.Status.REJECTED) {
            removeLocal(turnId);
            return reservation;
        }
        if (reservation.status() == TurnCancellationReservation.Status.TERMINAL) {
            removeLocal(turnId);
            return reservation;
        }
        synchronized (monitor) {
            // A concurrent retry uses the same immutable execution identity and id.
            reservations.putIfAbsent(turnId, reservationId);
        }
        try {
            token.cancel();
        } catch (RuntimeException failure) {
            // A listener may fail after cancellation is published locally; ack remains retryable.
            throw failure;
        }
        CancellationPublishAcknowledgement acknowledgement = acknowledge(reservationId);
        return finishAcknowledgement(turnId, reservationId, acknowledgement, reservation);
    }

    /** Calls the authority outside the local map lock so terminal completion cannot deadlock. */
    private CancellationPublishAcknowledgement acknowledge(String reservationId) {
        try {
            return statePort.acknowledgeCancellationPublished(reservationId);
        } catch (RuntimeException failure) {
            // Publish outcome is ambiguous; keep the stable handle for recovery.
            throw failure;
        }
    }

    /** Converts a typed acknowledgement and releases only authority-final rejection handles. */
    private TurnCancellationReservation finishAcknowledgement(TurnId turnId, String reservationId,
                                                              CancellationPublishAcknowledgement acknowledgement,
                                                              TurnCancellationReservation reservation) {
        if (acknowledgement == null || !reservationId.equals(acknowledgement.reservationId())
                || (acknowledgement.state() != null
                && !turnId.equals(acknowledgement.state().turnId()))) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        if (acknowledgement.status() == CancellationPublishAcknowledgement.Status.ACKNOWLEDGED) {
            removeLocal(turnId);
            return reservation;
        }
        if (acknowledgement.status() == CancellationPublishAcknowledgement.Status.TERMINAL) {
            removeLocal(turnId);
            return TurnCancellationReservation.terminal(reservationId, acknowledgement.state());
        }
        // A typed authority rejection is final; only an exception has an unknown commit outcome.
        removeLocal(turnId);
        return TurnCancellationReservation.rejected(reservationId, acknowledgement.rejection());
    }

    /** Cleans a stale local handle when a caller retries after receiving a terminal snapshot. */
    private TurnCancellationReservation finishTerminalSnapshotAcknowledgement(
            TurnId turnId, String reservationId, CancellationPublishAcknowledgement acknowledgement) {
        if (acknowledgement == null || !reservationId.equals(acknowledgement.reservationId())
                || (acknowledgement.state() != null
                && !turnId.equals(acknowledgement.state().turnId()))) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        if (acknowledgement.status() == CancellationPublishAcknowledgement.Status.ACKNOWLEDGED) {
            if (acknowledgement.state().status() != TurnStatus.INTERRUPTING) {
                throw new ProtocolException(JaErrorCode.INVALID_STATE);
            }
            removeLocal(turnId);
            return new TurnCancellationReservation(TurnCancellationReservation.Status.ALREADY_RESERVED,
                    reservationId, acknowledgement.state(), null);
        }
        if (acknowledgement.status() == CancellationPublishAcknowledgement.Status.TERMINAL) {
            removeLocal(turnId);
            return TurnCancellationReservation.terminal(reservationId, acknowledgement.state());
        }
        // A typed authority rejection is final; only an exception has an unknown commit outcome.
        removeLocal(turnId);
        return TurnCancellationReservation.rejected(reservationId, acknowledgement.rejection());
    }

    /** Completes through the durable reservation barrier using the caller's actual terminal time. */
    public TurnState complete(TurnState expected, TurnStatus terminal, Instant at) {
        TurnState state = Objects.requireNonNull(expected, "expected");
        TurnId id = state.turnId();
        Objects.requireNonNull(terminal, "terminal");
        Objects.requireNonNull(at, "at");
        if (!terminal.terminal()) {
            throw new IllegalArgumentException("terminal status required");
        }
        String reservationId;
        synchronized (monitor) {
            reservationId = reservations.getOrDefault(id, stableReservationId(state));
        }
        // The authority, not this local recovery map, serializes completion with publish ack.
        TurnState completed = statePort.completeAndResolveCancellation(state, reservationId, terminal, at);
        if (completed == null || !id.equals(completed.turnId()) || !completed.status().terminal()) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        removeLocal(id);
        return completed;
    }

    /** Returns the number of cancellation tokens awaiting terminal cleanup. */
    public int trackedCount() {
        synchronized (monitor) {
            return tokens.size();
        }
    }

    /** Derives the same reservation id for every retry in this server namespace. */
    private String stableReservationId(TurnState state) {
        return TurnCancellationReservation.stableId(serverInstanceId, state);
    }

    /** Drops local state only after the port has durably resolved or rejected the lease. */
    private void removeLocal(TurnId turnId) {
        synchronized (monitor) {
            reservations.remove(turnId);
            tokens.remove(turnId);
        }
    }
}
