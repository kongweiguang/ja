// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

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
import java.util.concurrent.CountDownLatch;

/** Test-only authoritative port used to make reservation barriers deterministic. */
final class InMemoryTurnStatePort implements TurnStatePort {
    private static final ServerInstanceId SERVER = new ServerInstanceId("srv_cancel");
    private final Map<TurnId, TurnState> states = new HashMap<>();
    private final Map<TurnId, String> reservations = new HashMap<>();
    private final Map<String, TurnId> turnByReservation = new HashMap<>();
    private final Map<String, CancellationPublishAcknowledgement.Status> history = new HashMap<>();
    private final Map<String, TurnState> historyStates = new HashMap<>();
    private CountDownLatch ackEntered;
    private CountDownLatch ackRelease;
    private boolean commitThenThrowReserve;
    private JaErrorCode rejectReserve;
    private boolean failAck;
    private JaErrorCode rejectAck;
    private boolean commitThenThrowAck;
    private boolean failComplete;
    private boolean commitThenThrowComplete;

    /** Stores a fixture before a use-case call so the port has an authoritative value. */
    synchronized void put(TurnState state) {
        states.put(state.turnId(), state);
    }

    /** Reads a fixture for retry input without adding a read path to the production port. */
    synchronized TurnState state(TurnId turnId) {
        TurnState state = states.get(turnId);
        if (state == null) {
            throw new ProtocolException(JaErrorCode.TURN_NOT_FOUND);
        }
        return state;
    }

    /** Returns the stable reservation id used by a turn for tombstone assertions. */
    String reservationId(TurnId turnId) {
        return TurnCancellationReservation.stableId(SERVER, state(turnId));
    }

    /** Exposes active reservation cardinality so tests detect duplicate leases. */
    synchronized int reservationCount() {
        return reservations.size();
    }

    /** Pauses acknowledgement before it observes a competing terminal completion. */
    synchronized void blockNextAck() {
        ackEntered = new CountDownLatch(1);
        ackRelease = new CountDownLatch(1);
    }

    /** Waits until the publish acknowledgement has reached its deterministic barrier. */
    void awaitAckEntered() {
        CountDownLatch entered;
        synchronized (this) {
            entered = ackEntered;
        }
        await(entered, "cancellation acknowledgement did not enter");
    }

    /** Releases acknowledgement after the competing completion has been committed. */
    synchronized void releaseAck() {
        if (ackRelease == null) {
            throw new AssertionError("acknowledgement barrier was not armed");
        }
        ackRelease.countDown();
    }

    /** Simulates a durable reservation commit whose response is lost. */
    synchronized void commitThenThrowReserveOnce() {
        commitThenThrowReserve = true;
    }

    /** Simulates an authoritative final reserve rejection without a durable mutation. */
    synchronized void rejectReserveOnce(JaErrorCode code) {
        rejectReserve = Objects.requireNonNull(code, "code");
    }

    /** Simulates an acknowledgement transport failure without changing durable history. */
    synchronized void failAckOnce() {
        failAck = true;
    }

    /** Simulates an authoritative final acknowledgement rejection without a durable mutation. */
    synchronized void rejectAckOnce(JaErrorCode code) {
        rejectAck = Objects.requireNonNull(code, "code");
    }

    /** Simulates an acknowledgement commit whose response is lost. */
    synchronized void commitThenThrowAckOnce() {
        commitThenThrowAck = true;
    }

    /** Simulates a terminal completion failure before its transaction commits. */
    synchronized void failCompleteOnce() {
        failComplete = true;
    }

    /** Simulates a terminal completion commit whose response is lost. */
    synchronized void commitThenThrowCompleteOnce() {
        commitThenThrowComplete = true;
    }

    /** Returns the instance namespace used in stable reservation derivation. */
    @Override
    public ServerInstanceId serverInstanceId() {
        return SERVER;
    }

    /** Implements typed reservation outcomes without a current-state guessing round trip. */
    @Override
    public synchronized TurnCancellationReservation reserveCancellation(TurnState expected,
                                                                           String reservationId,
                                                                           Instant at) {
        Objects.requireNonNull(expected, "expected");
        Objects.requireNonNull(reservationId, "reservationId");
        Objects.requireNonNull(at, "at");
        if (rejectReserve != null) {
            JaErrorCode code = rejectReserve;
            rejectReserve = null;
            return TurnCancellationReservation.rejected(reservationId, code);
        }
        TurnState current = states.get(expected.turnId());
        if (current == null) {
            return TurnCancellationReservation.rejected(reservationId, JaErrorCode.TURN_NOT_FOUND);
        }
        if (current.status().terminal()) {
            return TurnCancellationReservation.terminal(reservationId, current);
        }
        String existing = reservations.get(expected.turnId());
        if (existing != null) {
            if (!existing.equals(reservationId)) {
                return TurnCancellationReservation.rejected(reservationId, JaErrorCode.CONFLICT);
            }
            return new TurnCancellationReservation(TurnCancellationReservation.Status.ALREADY_RESERVED,
                    reservationId, current, null);
        }
        if (history.get(reservationId) == CancellationPublishAcknowledgement.Status.ACKNOWLEDGED
                && expected.turnId().equals(turnByReservation.get(reservationId))) {
            // The durable acknowledgement is a tombstone; retries must not create a second lease.
            return new TurnCancellationReservation(TurnCancellationReservation.Status.ALREADY_RESERVED,
                    reservationId, current, null);
        }
        if (!current.equals(expected)) {
            return TurnCancellationReservation.rejected(reservationId, JaErrorCode.CONFLICT);
        }
        TurnState transitioned;
        try {
            transitioned = current.transition(TurnStatus.INTERRUPTING, at);
        } catch (ProtocolException rejection) {
            return TurnCancellationReservation.rejected(reservationId, rejection.code());
        }
        states.put(expected.turnId(), transitioned);
        reservations.put(expected.turnId(), reservationId);
        turnByReservation.put(reservationId, expected.turnId());
        if (commitThenThrowReserve) {
            commitThenThrowReserve = false;
            throw new ProtocolException(JaErrorCode.INTERNAL_ERROR);
        }
        return new TurnCancellationReservation(TurnCancellationReservation.Status.RESERVED,
                reservationId, transitioned, null);
    }

    /**
     * Resolves cancellation publication and retains an acknowledgement tombstone.
     *
     * <p>The test barrier is published and signalled while holding this instance lock. The
     * completed {@code ackEntered} reference intentionally remains visible until the next
     * {@link #blockNextAck()} call: the observer can start after a fast acknowledgement and must
     * still obtain the latch that carried the signal. The lock publishes the reference, while
     * {@link CountDownLatch#countDown()} provides the normal happens-before edge for the wait.
     */
    @Override
    public CancellationPublishAcknowledgement acknowledgeCancellationPublished(String reservationId) {
        Objects.requireNonNull(reservationId, "reservationId");
        CountDownLatch entered;
        CountDownLatch release;
        synchronized (this) {
            if (failAck) {
                failAck = false;
                throw new ProtocolException(JaErrorCode.INTERNAL_ERROR);
            }
            entered = ackEntered;
            release = ackRelease;
            if (entered != null) {
                entered.countDown();
            }
        }
        if (release != null) {
            await(release, "acknowledgement barrier was not released");
            synchronized (this) {
                if (ackRelease == release) {
                    ackRelease = null;
                }
            }
        }
        synchronized (this) {
            if (rejectAck != null) {
                JaErrorCode code = rejectAck;
                rejectAck = null;
                return CancellationPublishAcknowledgement.rejected(reservationId, code);
            }
            CancellationPublishAcknowledgement result = acknowledgeLocked(reservationId);
            if (commitThenThrowAck) {
                commitThenThrowAck = false;
                throw new ProtocolException(JaErrorCode.INTERNAL_ERROR);
            }
            return result;
        }
    }

    /** Completes and resolves terminal reservation state under one durable monitor. */
    @Override
    public synchronized TurnState completeAndResolveCancellation(TurnState expected, String reservationId,
                                                                  TurnStatus terminal, Instant at) {
        Objects.requireNonNull(expected, "expected");
        Objects.requireNonNull(reservationId, "reservationId");
        Objects.requireNonNull(terminal, "terminal");
        Objects.requireNonNull(at, "at");
        if (!terminal.terminal()) {
            throw new IllegalArgumentException("terminal status required");
        }
        if (failComplete) {
            failComplete = false;
            throw new ProtocolException(JaErrorCode.INTERNAL_ERROR);
        }
        TurnId turnId = expected.turnId();
        TurnId knownTurn = turnByReservation.get(reservationId);
        if (knownTurn != null && !knownTurn.equals(turnId)) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        String activeReservation = reservations.get(turnId);
        if (activeReservation != null && !activeReservation.equals(reservationId)) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        TurnState current = states.get(turnId);
        if (current == null) {
            throw new ProtocolException(JaErrorCode.TURN_NOT_FOUND);
        }
        if (current.status().terminal()) {
            reservations.remove(turnId);
            turnByReservation.put(reservationId, turnId);
            history.put(reservationId, CancellationPublishAcknowledgement.Status.TERMINAL);
            historyStates.put(reservationId, current);
            return current;
        }
        if (!current.equals(expected) && !(current.status() == TurnStatus.INTERRUPTING
                && sameExecutionRevision(current, expected))) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        TurnState completed = current.transition(terminal, at);
        states.put(turnId, completed);
        reservations.remove(turnId);
        turnByReservation.put(reservationId, turnId);
        history.put(reservationId, CancellationPublishAcknowledgement.Status.TERMINAL);
        historyStates.put(reservationId, completed);
        if (commitThenThrowComplete) {
            commitThenThrowComplete = false;
            throw new ProtocolException(JaErrorCode.INTERNAL_ERROR);
        }
        return completed;
    }

    /** Allows terminal resolution from the original snapshot without a second current read. */
    private static boolean sameExecutionRevision(TurnState current, TurnState expected) {
        return current.turnId().equals(expected.turnId())
                && current.threadId().equals(expected.threadId())
                && current.mode() == expected.mode()
                && current.permissionMode() == expected.permissionMode()
                && current.startedAt().equals(expected.startedAt());
    }

    /** Resolves acknowledgement from active or durable tombstone state. */
    private CancellationPublishAcknowledgement acknowledgeLocked(String reservationId) {
        CancellationPublishAcknowledgement.Status previous = history.get(reservationId);
        TurnState previousState = historyStates.get(reservationId);
        if (previous == CancellationPublishAcknowledgement.Status.TERMINAL) {
            return new CancellationPublishAcknowledgement(
                    CancellationPublishAcknowledgement.Status.TERMINAL, reservationId, previousState, null);
        }
        if (previous == CancellationPublishAcknowledgement.Status.ACKNOWLEDGED) {
            return new CancellationPublishAcknowledgement(
                    CancellationPublishAcknowledgement.Status.ACKNOWLEDGED, reservationId, previousState, null);
        }
        TurnId turnId = turnByReservation.get(reservationId);
        if (turnId == null) {
            return CancellationPublishAcknowledgement.rejected(reservationId, JaErrorCode.TURN_NOT_FOUND);
        }
        String activeReservation = reservations.get(turnId);
        if (!reservationId.equals(activeReservation)) {
            return CancellationPublishAcknowledgement.rejected(reservationId, JaErrorCode.CONFLICT);
        }
        TurnState current = states.get(turnId);
        if (current == null) {
            return CancellationPublishAcknowledgement.rejected(reservationId, JaErrorCode.TURN_NOT_FOUND);
        }
        if (current.status().terminal()) {
            reservations.remove(turnId);
            history.put(reservationId, CancellationPublishAcknowledgement.Status.TERMINAL);
            historyStates.put(reservationId, current);
            return new CancellationPublishAcknowledgement(
                    CancellationPublishAcknowledgement.Status.TERMINAL, reservationId, current, null);
        }
        reservations.remove(turnId);
        history.put(reservationId, CancellationPublishAcknowledgement.Status.ACKNOWLEDGED);
        historyStates.put(reservationId, current);
        return new CancellationPublishAcknowledgement(
                CancellationPublishAcknowledgement.Status.ACKNOWLEDGED, reservationId, current, null);
    }

    /** Waits on a latch and converts interruption into a deterministic test failure. */
    private static void await(CountDownLatch latch, String message) {
        try {
            latch.await();
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            throw new AssertionError(message, exception);
        }
    }
}
