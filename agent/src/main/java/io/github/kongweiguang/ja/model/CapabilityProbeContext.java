// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import java.util.Objects;
import java.util.concurrent.CancellationException;

/** Bounded context passed to a transport so implementations can honor the same absolute deadline. */
public record CapabilityProbeContext(
        long deadlineNanos,
        CapabilityProbeCancellation cancellation,
        int maxCapabilities,
        int maxDiagnosticChars) {
    /** Validates limits before an untrusted provider implementation is allowed to run. */
    public CapabilityProbeContext {
        Objects.requireNonNull(cancellation, "cancellation");
        // nanoTime is an arbitrary signed origin, so an expired absolute deadline may be <= 0.
        if (maxCapabilities <= 0 || maxDiagnosticChars <= 0) {
            throw new IllegalArgumentException("invalid probe context limits");
        }
    }

    /** Returns remaining monotonic time, avoiding wall-clock changes during a network probe. */
    public long remainingNanos() {
        if (deadlineNanos == Long.MAX_VALUE) {
            return Long.MAX_VALUE;
        }
        long remaining = deadlineNanos - System.nanoTime();
        return remaining > 0 ? remaining : 0;
    }

    /** Allows a transport to fail cooperatively before producing an unbounded response. */
    public void checkActive() {
        if (cancellation.isCancelled()) {
            throw new CancellationException("probe cancelled");
        }
        if (remainingNanos() == 0) {
            throw new CapabilityProbeTimeoutException();
        }
    }
}
