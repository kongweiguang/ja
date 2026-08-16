// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.TurnState;
import io.github.kongweiguang.ja.domain.TurnStatus;
import io.github.kongweiguang.ja.protocol.JaErrorCode;

import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HexFormat;
import java.util.Objects;

/**
 * Typed result of an authoritative cancellation reservation.
 * The stable id is scoped by server instance and immutable execution identity,
 * so an ambiguous commit can be retried without inventing a second reservation.
 */
public record TurnCancellationReservation(Status status, String reservationId,
                                          TurnState state, JaErrorCode rejection) {
    /** Reservation outcomes are data so business rejection does not require exceptions. */
    public enum Status {
        RESERVED,
        ALREADY_RESERVED,
        TERMINAL,
        REJECTED
    }

    /** Binds status, state, and rejection code so callers cannot observe an ambiguous result. */
    public TurnCancellationReservation {
        Objects.requireNonNull(status, "status");
        if (reservationId == null || reservationId.isBlank() || reservationId.length() > 128) {
            throw new IllegalArgumentException("invalid cancellation reservation id");
        }
        switch (status) {
            case RESERVED, ALREADY_RESERVED -> {
                Objects.requireNonNull(state, "state");
                if (state.status() != TurnStatus.INTERRUPTING || rejection != null) {
                    throw new IllegalArgumentException("invalid active reservation result");
                }
            }
            case TERMINAL -> {
                Objects.requireNonNull(state, "state");
                if (!state.status().terminal() || rejection != null) {
                    throw new IllegalArgumentException("invalid terminal reservation result");
                }
            }
            case REJECTED -> {
                Objects.requireNonNull(rejection, "rejection");
                if (state != null) {
                    throw new IllegalArgumentException("rejected reservation cannot carry state");
                }
            }
        }
    }

    /** Creates a stable rejection value while preserving the id used for a future retry. */
    public static TurnCancellationReservation rejected(String reservationId, JaErrorCode code) {
        return new TurnCancellationReservation(Status.REJECTED, reservationId, null,
                Objects.requireNonNull(code, "code"));
    }

    /** Creates the terminal result returned when completion won before publish acknowledgement. */
    public static TurnCancellationReservation terminal(String reservationId, TurnState state) {
        return new TurnCancellationReservation(Status.TERMINAL, reservationId,
                Objects.requireNonNull(state, "state"), null);
    }

    /** Derives one collision-resistant id from instance and immutable execution identity. */
    public static String stableId(ServerInstanceId serverInstanceId, TurnState state) {
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(state, "state");
        byte[] material = (serverInstanceId.value() + "\n" + state.turnId().value() + "\n"
                + state.threadId().value() + "\n" + state.mode().name() + "\n"
                + state.permissionMode().name() + "\n" + state.startedAt())
                .getBytes(StandardCharsets.UTF_8);
        try {
            byte[] digest = MessageDigest.getInstance("SHA-256").digest(material);
            return "cancel_" + HexFormat.of().formatHex(digest);
        } catch (NoSuchAlgorithmException impossible) {
            throw new AssertionError("JDK must provide SHA-256", impossible);
        }
    }
}
