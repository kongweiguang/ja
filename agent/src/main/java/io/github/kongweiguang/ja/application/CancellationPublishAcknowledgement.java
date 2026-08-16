// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.TurnState;
import io.github.kongweiguang.ja.protocol.JaErrorCode;

import java.util.Objects;

/** Typed result of publishing a cancellation event and acknowledging its lease. */
public record CancellationPublishAcknowledgement(Status status, String reservationId,
                                                 TurnState state, JaErrorCode rejection) {
    /** Acknowledgement outcomes distinguish terminal races from final typed rejection. */
    public enum Status { ACKNOWLEDGED, TERMINAL, REJECTED }

    /** Keeps terminal state and business rejection mutually exclusive. */
    public CancellationPublishAcknowledgement {
        Objects.requireNonNull(status, "status");
        if (reservationId == null || reservationId.isBlank() || reservationId.length() > 128) {
            throw new IllegalArgumentException("invalid cancellation reservation id");
        }
        switch (status) {
            case ACKNOWLEDGED -> {
                Objects.requireNonNull(state, "state");
                if (state.status().terminal() || rejection != null) {
                    throw new IllegalArgumentException("invalid acknowledged state");
                }
            }
            case TERMINAL -> {
                Objects.requireNonNull(state, "state");
                if (!state.status().terminal() || rejection != null) {
                    throw new IllegalArgumentException("invalid terminal acknowledgement");
                }
            }
            case REJECTED -> {
                Objects.requireNonNull(rejection, "rejection");
                if (state != null) {
                    throw new IllegalArgumentException("rejected acknowledgement cannot carry state");
                }
            }
        }
    }

    /** Creates a typed rejection without conflating it with an IO/commit failure. */
    public static CancellationPublishAcknowledgement rejected(String reservationId,
                                                               JaErrorCode code) {
        return new CancellationPublishAcknowledgement(Status.REJECTED, reservationId, null,
                Objects.requireNonNull(code, "code"));
    }
}
